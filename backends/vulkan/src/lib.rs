//! Vulkan 1.3 backend: dynamic rendering, synchronization2, bindless sampled images,
//! push descriptors for per-draw buffers.

use std::ffi::{CString, c_void};
use std::mem::ManuallyDrop;

use ash::{ext, khr, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator, AllocatorCreateDesc};
use loom_render::*;

const FRAMES: usize = 2;
/// Timestamp queries per frame: start, after uploads, marks, end.
const MAX_MARKS: u32 = 16;
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;

struct Slab<T> {
    items: Vec<Option<T>>,
    free: Vec<u32>,
}

impl<T> Slab<T> {
    fn new() -> Self {
        Slab { items: Vec::new(), free: Vec::new() }
    }
    fn insert(&mut self, v: T) -> u32 {
        if let Some(i) = self.free.pop() {
            self.items[i as usize] = Some(v);
            i
        } else {
            self.items.push(Some(v));
            self.items.len() as u32 - 1
        }
    }
    fn remove(&mut self, i: u32) -> Option<T> {
        let v = self.items.get_mut(i as usize)?.take();
        if v.is_some() {
            self.free.push(i);
        }
        v
    }
    fn get(&self, i: u32) -> &T {
        self.items[i as usize].as_ref().expect("stale render handle")
    }
    fn get_mut(&mut self, i: u32) -> &mut T {
        self.items[i as usize].as_mut().expect("stale render handle")
    }
}

struct Buffer {
    raw: vk::Buffer,
    alloc: Allocation,
    size: u64,
}

struct Texture {
    image: vk::Image,
    view: vk::ImageView,
    alloc: Allocation,
    desc: TextureDesc,
    layout: vk::ImageLayout,
    /// Index into the matching bindless array, `u32::MAX` for depth.
    index: u32,
}

struct Pipeline {
    raw: vk::Pipeline,
    layout: vk::PipelineLayout,
    set_layout: vk::DescriptorSetLayout,
    bind_point: vk::PipelineBindPoint,
    buffers: Vec<(u32, vk::DescriptorType)>,
}

enum Deferred {
    Buffer(vk::Buffer, Allocation),
    Texture(vk::Image, vk::ImageView, Allocation, u32),
    Pipeline(vk::Pipeline, vk::PipelineLayout, vk::DescriptorSetLayout),
}

/// Upload memory of one frame slot: a list of chunks filled in order. When a chunk is full the
/// next one is used (made if needed) instead of growing and copying: reading back mapped upload
/// memory is very slow (write-combined), a 100 MB copy took half a second.
struct Staging {
    chunks: Vec<(vk::Buffer, Allocation, u64)>,
    current: usize,
    used: u64,
}

struct Frame {
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    acquired: vk::Semaphore,
    fence: vk::Fence,
    staging: Staging,
    deferred: Vec<Deferred>,
    waited: bool,
    /// Timestamps written by the frame now in flight in this slot.
    marks: u32,
}

enum Upload {
    Buffer { src: (u32, u64), dst: u32, offset: u64, size: u64 },
    Texture { src: (u32, u64), dst: u32, mip: u32 },
    InitTexture(u32),
}

struct Swapchain {
    raw: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    rendered: Vec<vk::Semaphore>,
    layouts: Vec<vk::ImageLayout>,
    format: vk::Format,
    extent: vk::Extent2D,
}

pub struct VulkanBackend {
    _entry: ash::Entry,
    instance: ash::Instance,
    debug: Option<(ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    surface_fn: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    swapchain_fn: khr::swapchain::Device,
    push_fn: khr::push_descriptor::Device,
    allocator: ManuallyDrop<Allocator>,
    swapchain: Option<Swapchain>,
    frames: Vec<Frame>,
    slot: usize,
    image_index: Option<u32>,
    want_size: (u32, u32),
    vsync: bool,
    dirty_swapchain: bool,
    buffers: Slab<Buffer>,
    textures: Slab<Texture>,
    pipelines: Slab<Pipeline>,
    uploads: Vec<Upload>,
    global_layout: vk::DescriptorSetLayout,
    global_pool: vk::DescriptorPool,
    global_set: vk::DescriptorSet,
    samplers: [vk::Sampler; 4],
    free_tex: Vec<u32>,
    next_tex: u32,
    capture_requested: bool,
    capture: Option<(vk::Buffer, Allocation, u32, u32, vk::Format)>,
    device_name: String,
    queries: vk::QueryPool,
    ts_period: f32,
    gpu_times: Vec<f32>,
}

// Raw Vulkan handles are plain integers/pointers that may be used from any thread as long as access
// is externally synchronized, which `&mut self` guarantees.
unsafe impl Send for VulkanBackend {}

unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _ty: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user: *mut c_void,
) -> vk::Bool32 {
    let msg = unsafe { (*data).message_as_c_str() }.map(|s| s.to_string_lossy()).unwrap_or_default();
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        log::error!(target: "vulkan", "{msg}");
    } else if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        log::warn!(target: "vulkan", "{msg}");
    } else {
        log::debug!(target: "vulkan", "{msg}");
    }
    vk::FALSE
}

fn err<E: std::fmt::Debug>(what: &str) -> impl FnOnce(E) -> Error + '_ {
    move |e| Error(format!("{what}: {e:?}"))
}

impl VulkanBackend {
    pub fn new(desc: &BackendDesc) -> Result<VulkanBackend, Error> {
        unsafe { Self::create(desc) }
    }

    unsafe fn create(desc: &BackendDesc) -> Result<VulkanBackend, Error> {
        unsafe {
            let entry = ash::Entry::load().map_err(err("loading Vulkan"))?;
            let display = desc.window.display_handle().map_err(err("display handle"))?.as_raw();
            let window = desc.window.window_handle().map_err(err("window handle"))?.as_raw();

            let mut exts: Vec<*const i8> =
                ash_window::enumerate_required_extensions(display).map_err(err("surface extensions"))?.to_vec();
            let mut layers: Vec<*const i8> = Vec::new();
            let validation = c"VK_LAYER_KHRONOS_validation";
            let debug = desc.debug
                && entry
                    .enumerate_instance_layer_properties()
                    .unwrap_or_default()
                    .iter()
                    .any(|l| l.layer_name_as_c_str() == Ok(validation));
            if debug {
                layers.push(validation.as_ptr());
                exts.push(ext::debug_utils::NAME.as_ptr());
            } else if desc.debug {
                log::warn!("validation layer requested but not installed");
            }

            let app = vk::ApplicationInfo::default()
                .application_name(c"Loom")
                .engine_name(c"Loom")
                .api_version(vk::API_VERSION_1_3);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default()
                        .application_info(&app)
                        .enabled_extension_names(&exts)
                        .enabled_layer_names(&layers),
                    None,
                )
                .map_err(err("vkCreateInstance"))?;

            let debug = if debug {
                let f = ext::debug_utils::Instance::new(&entry, &instance);
                let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
                    .message_severity(
                        vk::DebugUtilsMessageSeverityFlagsEXT::ERROR | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING,
                    )
                    .message_type(
                        vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                            | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                            | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
                    )
                    .pfn_user_callback(Some(debug_callback));
                let m = f.create_debug_utils_messenger(&info, None).map_err(err("debug messenger"))?;
                Some((f, m))
            } else {
                None
            };

            let surface_fn = khr::surface::Instance::new(&entry, &instance);
            let surface = ash_window::create_surface(&entry, &instance, display, window, None)
                .map_err(err("creating surface"))?;

            let mut best = None;
            for pdev in instance.enumerate_physical_devices().map_err(err("enumerating GPUs"))? {
                let props = instance.get_physical_device_properties(pdev);
                if props.api_version < vk::API_VERSION_1_3 {
                    continue;
                }
                let families = instance.get_physical_device_queue_family_properties(pdev);
                let Some(qf) = families.iter().enumerate().position(|(i, f)| {
                    f.queue_flags.contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
                        && surface_fn.get_physical_device_surface_support(pdev, i as u32, surface).unwrap_or(false)
                }) else {
                    continue;
                };
                let score = match props.device_type {
                    vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                    vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                    _ => 1,
                };
                if best.map_or(true, |(_, _, s)| score > s) {
                    best = Some((pdev, qf as u32, score));
                }
            }
            let (pdev, queue_family, _) = best.ok_or_else(|| Error("no Vulkan 1.3 GPU with presentation".into()))?;
            let props = instance.get_physical_device_properties(pdev);
            let device_name = props.device_name_as_c_str().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            log::info!("GPU: {device_name}");
            let mut indexing = vk::PhysicalDeviceDescriptorIndexingProperties::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut indexing);
            instance.get_physical_device_properties2(pdev, &mut props2);
            let images = indexing.max_per_stage_descriptor_update_after_bind_sampled_images.min(indexing.max_descriptor_set_update_after_bind_sampled_images);
            if images < MAX_TEXTURES {
                return Err(Error(format!("{device_name} can bind {images} textures at once, the game needs {MAX_TEXTURES}")));
            }

            let features = vk::PhysicalDeviceFeatures::default()
                .multi_draw_indirect(true)
                .draw_indirect_first_instance(true)
                .sampler_anisotropy(true)
                .fill_mode_non_solid(true)
                .shader_int16(true);
            let mut f11 = vk::PhysicalDeviceVulkan11Features::default().storage_buffer16_bit_access(true);
            let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
                .descriptor_indexing(true)
                .runtime_descriptor_array(true)
                .descriptor_binding_partially_bound(true)
                .descriptor_binding_sampled_image_update_after_bind(true)
                .shader_sampled_image_array_non_uniform_indexing(true)
                .draw_indirect_count(true)
                .scalar_block_layout(true);
            let mut f13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true).synchronization2(true);
            let mut f2 = vk::PhysicalDeviceFeatures2::default()
                .features(features)
                .push_next(&mut f11)
                .push_next(&mut f12)
                .push_next(&mut f13);
            let dev_exts = [khr::swapchain::NAME.as_ptr(), khr::push_descriptor::NAME.as_ptr()];
            let prio = [1.0];
            let qinfo = [vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&prio)];
            let device = instance
                .create_device(
                    pdev,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&qinfo)
                        .enabled_extension_names(&dev_exts)
                        .push_next(&mut f2),
                    None,
                )
                .map_err(err("vkCreateDevice"))?;
            let queue = device.get_device_queue(queue_family, 0);

            let allocator = Allocator::new(&AllocatorCreateDesc {
                instance: instance.clone(),
                device: device.clone(),
                physical_device: pdev,
                debug_settings: Default::default(),
                buffer_device_address: false,
                allocation_sizes: Default::default(),
            })
            .map_err(err("GPU allocator"))?;

            let swapchain_fn = khr::swapchain::Device::new(&instance, &device);
            let push_fn = khr::push_descriptor::Device::new(&instance, &device);

            let mut frames = Vec::new();
            for _ in 0..FRAMES {
                let pool = device
                    .create_command_pool(
                        &vk::CommandPoolCreateInfo::default()
                            .queue_family_index(queue_family)
                            .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                        None,
                    )
                    .map_err(err("command pool"))?;
                let cmd = device
                    .allocate_command_buffers(
                        &vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1),
                    )
                    .map_err(err("command buffer"))?[0];
                frames.push(Frame {
                    pool,
                    cmd,
                    acquired: device.create_semaphore(&Default::default(), None).map_err(err("semaphore"))?,
                    fence: device
                        .create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)
                        .map_err(err("fence"))?,
                    staging: Staging { chunks: Vec::new(), current: 0, used: 0 },
                    deferred: Vec::new(),
                    waited: false,
                    marks: 0,
                });
            }

            let flags = vk::DescriptorBindingFlags::PARTIALLY_BOUND | vk::DescriptorBindingFlags::UPDATE_AFTER_BIND;
            let binding_flags = [flags, vk::DescriptorBindingFlags::empty(), Default::default(), Default::default(), Default::default()];
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .descriptor_count(MAX_TEXTURES)
                    .stage_flags(vk::ShaderStageFlags::ALL),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::ALL),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(2)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::ALL),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(3)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::ALL),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(4)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::ALL),
            ];
            let mut flag_info = vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&binding_flags);
            let global_layout = device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default()
                        .bindings(&bindings)
                        .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
                        .push_next(&mut flag_info),
                    None,
                )
                .map_err(err("global set layout"))?;
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLED_IMAGE,
                    descriptor_count: MAX_TEXTURES,
                },
                vk::DescriptorPoolSize { ty: vk::DescriptorType::SAMPLER, descriptor_count: 4 },
            ];
            let global_pool = device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .pool_sizes(&sizes)
                        .max_sets(1)
                        .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND),
                    None,
                )
                .map_err(err("descriptor pool"))?;
            let layouts = [global_layout];
            let global_set = device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default().descriptor_pool(global_pool).set_layouts(&layouts),
                )
                .map_err(err("global set"))?[0];

            let sampler = |filter: vk::Filter, mode: vk::SamplerAddressMode, mips: bool| {
                // The filtered sampler gets anisotropy: block faces at grazing angles stay sharp.
                let aniso = filter == vk::Filter::LINEAR && mips;
                device.create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(filter)
                        .min_filter(filter)
                        .mipmap_mode(if mips { vk::SamplerMipmapMode::LINEAR } else { vk::SamplerMipmapMode::NEAREST })
                        .address_mode_u(mode)
                        .address_mode_v(mode)
                        .address_mode_w(mode)
                        .anisotropy_enable(aniso)
                        .max_anisotropy(if aniso { 8.0 } else { 1.0 })
                        .max_lod(if mips { vk::LOD_CLAMP_NONE } else { 0.0 }),
                    None,
                )
            };
            let samplers = [
                sampler(vk::Filter::NEAREST, vk::SamplerAddressMode::CLAMP_TO_EDGE, false).map_err(err("sampler"))?,
                sampler(vk::Filter::LINEAR, vk::SamplerAddressMode::CLAMP_TO_EDGE, false).map_err(err("sampler"))?,
                sampler(vk::Filter::NEAREST, vk::SamplerAddressMode::CLAMP_TO_EDGE, true).map_err(err("sampler"))?,
                sampler(vk::Filter::LINEAR, vk::SamplerAddressMode::CLAMP_TO_EDGE, true).map_err(err("sampler"))?,
            ];
            let infos: Vec<[vk::DescriptorImageInfo; 1]> =
                samplers.iter().map(|&s| [vk::DescriptorImageInfo::default().sampler(s)]).collect();
            let writes: Vec<_> = infos
                .iter()
                .enumerate()
                .map(|(i, info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(global_set)
                        .dst_binding(1 + i as u32)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .image_info(info)
                })
                .collect();
            device.update_descriptor_sets(&writes, &[]);

            let queries = device
                .create_query_pool(
                    &vk::QueryPoolCreateInfo::default().query_type(vk::QueryType::TIMESTAMP).query_count(MAX_MARKS * FRAMES as u32),
                    None,
                )
                .map_err(err("query pool"))?;
            let mut backend = VulkanBackend {
                _entry: entry,
                instance,
                debug,
                surface_fn,
                surface,
                pdev,
                device,
                queue,
                swapchain_fn,
                push_fn,
                allocator: ManuallyDrop::new(allocator),
                swapchain: None,
                frames,
                slot: 0,
                image_index: None,
                want_size: (desc.width, desc.height),
                vsync: desc.vsync,
                dirty_swapchain: true,
                buffers: Slab::new(),
                textures: Slab::new(),
                pipelines: Slab::new(),
                uploads: Vec::new(),
                global_layout,
                global_pool,
                global_set,
                samplers,
                free_tex: Vec::new(),
                next_tex: 0,
                capture_requested: false,
                capture: None,
                device_name,
                queries,
                ts_period: props.limits.timestamp_period,
                gpu_times: Vec::new(),
            };
            backend.recreate_swapchain()?;
            Ok(backend)
        }
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    unsafe fn recreate_swapchain(&mut self) -> Result<(), Error> {
        unsafe {
            let caps = self
                .surface_fn
                .get_physical_device_surface_capabilities(self.pdev, self.surface)
                .map_err(err("surface caps"))?;
            let extent = if caps.current_extent.width != u32::MAX {
                caps.current_extent
            } else {
                vk::Extent2D {
                    width: self.want_size.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: self.want_size.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            };
            if extent.width == 0 || extent.height == 0 {
                return Ok(());
            }
            self.device.device_wait_idle().ok();

            let formats = self
                .surface_fn
                .get_physical_device_surface_formats(self.pdev, self.surface)
                .map_err(err("surface formats"))?;
            let format = formats
                .iter()
                .find(|f| {
                    f.format == vk::Format::B8G8R8A8_SRGB && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                })
                .or_else(|| formats.iter().find(|f| f.format == vk::Format::R8G8B8A8_SRGB))
                .unwrap_or(&formats[0]);
            let modes = self
                .surface_fn
                .get_physical_device_surface_present_modes(self.pdev, self.surface)
                .map_err(err("present modes"))?;
            let mode = if self.vsync {
                vk::PresentModeKHR::FIFO
            } else if modes.contains(&vk::PresentModeKHR::MAILBOX) {
                vk::PresentModeKHR::MAILBOX
            } else if modes.contains(&vk::PresentModeKHR::IMMEDIATE) {
                vk::PresentModeKHR::IMMEDIATE
            } else {
                vk::PresentModeKHR::FIFO
            };
            let mut count = caps.min_image_count + 1;
            if caps.max_image_count > 0 {
                count = count.min(caps.max_image_count);
            }
            let old = self.swapchain.take();
            let raw = self
                .swapchain_fn
                .create_swapchain(
                    &vk::SwapchainCreateInfoKHR::default()
                        .surface(self.surface)
                        .min_image_count(count)
                        .image_format(format.format)
                        .image_color_space(format.color_space)
                        .image_extent(extent)
                        .image_array_layers(1)
                        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
                        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .pre_transform(caps.current_transform)
                        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                        .present_mode(mode)
                        .clipped(true)
                        .old_swapchain(old.as_ref().map_or(vk::SwapchainKHR::null(), |s| s.raw)),
                    None,
                )
                .map_err(err("vkCreateSwapchainKHR"))?;
            if let Some(old) = old {
                self.destroy_swapchain(old);
            }
            let images = self.swapchain_fn.get_swapchain_images(raw).map_err(err("swapchain images"))?;
            let mut views = Vec::new();
            let mut rendered = Vec::new();
            for &image in &images {
                views.push(
                    self.device
                        .create_image_view(
                            &vk::ImageViewCreateInfo::default()
                                .image(image)
                                .view_type(vk::ImageViewType::TYPE_2D)
                                .format(format.format)
                                .subresource_range(color_range(1)),
                            None,
                        )
                        .map_err(err("swapchain view"))?,
                );
                rendered.push(self.device.create_semaphore(&Default::default(), None).map_err(err("semaphore"))?);
            }
            log::debug!("swapchain {}x{} {:?} {:?}", extent.width, extent.height, format.format, mode);
            self.swapchain = Some(Swapchain {
                raw,
                layouts: vec![vk::ImageLayout::UNDEFINED; images.len()],
                images,
                views,
                rendered,
                format: format.format,
                extent,
            });
            self.dirty_swapchain = false;
            Ok(())
        }
    }

    unsafe fn destroy_swapchain(&mut self, sc: Swapchain) {
        unsafe {
            for v in sc.views {
                self.device.destroy_image_view(v, None);
            }
            for s in sc.rendered {
                self.device.destroy_semaphore(s, None);
            }
            self.swapchain_fn.destroy_swapchain(sc.raw, None);
        }
    }

    fn vk_format(&self, f: Format) -> vk::Format {
        match f {
            Format::Rgba8Srgb => vk::Format::R8G8B8A8_SRGB,
            Format::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
            Format::R8Unorm => vk::Format::R8_UNORM,
            Format::Rgba16Float => vk::Format::R16G16B16A16_SFLOAT,
            Format::Depth32 => DEPTH_FORMAT,
            Format::Surface => self.swapchain.as_ref().map_or(vk::Format::B8G8R8A8_SRGB, |s| s.format),
        }
    }

    /// The slot's previous submission must be finished before its staging memory or command
    /// buffer is reused. Called lazily so uploads before `begin_frame` are safe too.
    fn ensure_slot_ready(&mut self) {
        let frame = &mut self.frames[self.slot];
        if frame.waited {
            return;
        }
        unsafe {
            self.device.wait_for_fences(&[frame.fence], true, u64::MAX).ok();
        }
        frame.waited = true;
        frame.staging.current = 0;
        frame.staging.used = 0;
        if frame.marks > 1 {
            let mut ts = vec![0u64; frame.marks as usize];
            let first = self.slot as u32 * MAX_MARKS;
            if unsafe { self.device.get_query_pool_results(self.queries, first, &mut ts, vk::QueryResultFlags::TYPE_64) }.is_ok() {
                let period = self.ts_period as f64 / 1e6;
                self.gpu_times = ts.windows(2).map(|w| (w[1].saturating_sub(w[0]) as f64 * period) as f32).collect();
            }
        }
        let deferred = std::mem::take(&mut frame.deferred);
        for d in deferred {
            self.destroy_now(d);
        }
    }

    fn destroy_now(&mut self, d: Deferred) {
        unsafe {
            match d {
                Deferred::Buffer(b, a) => {
                    self.device.destroy_buffer(b, None);
                    self.allocator.free(a).ok();
                }
                Deferred::Texture(image, view, a, index) => {
                    self.device.destroy_image_view(view, None);
                    self.device.destroy_image(image, None);
                    self.allocator.free(a).ok();
                    if index != u32::MAX {
                        self.free_tex.push(index);
                    }
                }
                Deferred::Pipeline(p, l, s) => {
                    self.device.destroy_pipeline(p, None);
                    self.device.destroy_pipeline_layout(l, None);
                    self.device.destroy_descriptor_set_layout(s, None);
                }
            }
        }
    }

    /// Copies `data` into this frame's upload memory: (chunk, offset).
    fn stage(&mut self, data: &[u8]) -> (u32, u64) {
        self.ensure_slot_ready();
        let size = data.len() as u64;
        let frame = &mut self.frames[self.slot];
        let st = &mut frame.staging;
        let mut offset = (st.used + 15) & !15;
        if st.chunks.get(st.current).is_none_or(|c| offset + size > c.2) {
            // Move on to the next chunk that fits, making one if none does.
            if !st.chunks.is_empty() {
                st.current += 1;
            }
            while st.chunks.get(st.current).is_some_and(|c| size > c.2) {
                st.current += 1;
            }
            if st.current >= st.chunks.len() {
                let chunk = size.max(st.chunks.last().map_or(8 << 20, |c| c.2 * 2));
                let (buffer, alloc) =
                    raw_buffer(&self.device, &mut self.allocator, chunk, vk::BufferUsageFlags::TRANSFER_SRC, MemoryLocation::CpuToGpu, "staging");
                st.chunks.push((buffer, alloc, chunk));
                st.current = st.chunks.len() - 1;
            }
            offset = 0;
        }
        let c = &mut st.chunks[st.current];
        c.1.mapped_slice_mut().unwrap()[offset as usize..(offset + size) as usize].copy_from_slice(data);
        st.used = offset + size;
        (st.current as u32, offset)
    }

    unsafe fn barrier_image(
        &self,
        cmd: vk::CommandBuffer,
        image: vk::Image,
        range: vk::ImageSubresourceRange,
        old: vk::ImageLayout,
        new: vk::ImageLayout,
    ) {
        let b = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
            .old_layout(old)
            .new_layout(new)
            .image(image)
            .subresource_range(range)];
        unsafe { self.device.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&b)) };
    }

    unsafe fn memory_barrier(&self, cmd: vk::CommandBuffer) {
        let b = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS | vk::PipelineStageFlags2::HOST)
            .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE | vk::AccessFlags2::HOST_READ)];
        unsafe { self.device.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&b)) };
    }

    unsafe fn record_uploads(&mut self, cmd: vk::CommandBuffer) {
        if self.uploads.is_empty() {
            return;
        }
        unsafe {
            self.memory_barrier(cmd);
            let chunks: Vec<vk::Buffer> = self.frames[self.slot].staging.chunks.iter().map(|c| c.0).collect();
            let uploads = std::mem::take(&mut self.uploads);
            for u in &uploads {
                match *u {
                    Upload::Buffer { src, dst, offset, size } => {
                        let Some(Some(b)) = self.buffers.items.get(dst as usize) else { continue };
                        let region = [vk::BufferCopy { src_offset: src.1, dst_offset: offset, size }];
                        self.device.cmd_copy_buffer(cmd, chunks[src.0 as usize], b.raw, &region);
                    }
                    Upload::Texture { src, dst, mip } => {
                        let staging = chunks[src.0 as usize];
                        let src = src.1;
                        let Some(Some(t)) = self.textures.items.get(dst as usize) else { continue };
                        let range = color_range(t.desc.mips);
                        let (image, layout) = (t.image, t.layout);
                        self.barrier_image(cmd, image, range, layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
                        let region = [vk::BufferImageCopy::default()
                            .buffer_offset(src)
                            .image_subresource(vk::ImageSubresourceLayers {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                mip_level: mip,
                                base_array_layer: 0,
                                layer_count: 1,
                            })
                            .image_extent(vk::Extent3D {
                                width: (t.desc.width >> mip).max(1),
                                height: (t.desc.height >> mip).max(1),
                                depth: 1,
                            })];
                        self.device.cmd_copy_buffer_to_image(
                            cmd,
                            staging,
                            image,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &region,
                        );
                        self.barrier_image(
                            cmd,
                            image,
                            range,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        );
                        self.textures.get_mut(dst).layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                    }
                    Upload::InitTexture(id) => {
                        let Some(Some(t)) = self.textures.items.get(id as usize) else { continue };
                        if t.layout != vk::ImageLayout::UNDEFINED {
                            continue;
                        }
                        let depth = t.index == u32::MAX;
                        let (range, new) = if depth {
                            (depth_range(), vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                        } else {
                            (color_range(t.desc.mips), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        };
                        let image = t.image;
                        self.barrier_image(cmd, image, range, vk::ImageLayout::UNDEFINED, new);
                        self.textures.get_mut(id).layout = new;
                    }
                }
            }
            self.memory_barrier(cmd);
        }
    }
}

fn color_range(mips: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: mips,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn depth_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::DEPTH,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn raw_buffer(
    device: &ash::Device,
    allocator: &mut Allocator,
    size: u64,
    usage: vk::BufferUsageFlags,
    location: MemoryLocation,
    name: &str,
) -> (vk::Buffer, Allocation) {
    unsafe {
        let buffer = device
            .create_buffer(&vk::BufferCreateInfo::default().size(size.max(16)).usage(usage), None)
            .expect("vkCreateBuffer");
        let requirements = device.get_buffer_memory_requirements(buffer);
        let alloc = allocator
            .allocate(&AllocationCreateDesc {
                name,
                requirements,
                location,
                linear: true,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            })
            .unwrap_or_else(|e| panic!("allocating {size} bytes for {name}: {e}"));
        device.bind_buffer_memory(buffer, alloc.memory(), alloc.offset()).expect("bind buffer memory");
        (buffer, alloc)
    }
}

fn compare_op(c: Compare) -> vk::CompareOp {
    match c {
        Compare::Always => vk::CompareOp::ALWAYS,
        Compare::Greater => vk::CompareOp::GREATER,
        Compare::GreaterEqual => vk::CompareOp::GREATER_OR_EQUAL,
        Compare::Less => vk::CompareOp::LESS,
        Compare::LessEqual => vk::CompareOp::LESS_OR_EQUAL,
    }
}

impl VulkanBackend {
    fn spirv(&self, shader: &Shader, stage: naga::ShaderStage, entry: &str) -> Result<vk::ShaderModule, Error> {
        let mut options = naga::back::spv::Options::default();
        options.lang_version = (1, 3);
        options.force_loop_bounding = false;
        let words = naga::back::spv::write_vec(
            &shader.module,
            &shader.info,
            &options,
            Some(&naga::back::spv::PipelineOptions { shader_stage: stage, entry_point: entry.to_string() }),
        )
        .map_err(|e| Error(format!("SPIR-V for {entry}: {e}")))?;
        unsafe {
            self.device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .map_err(err("shader module"))
        }
    }

    fn layout_for(&self, shader: &Shader) -> Result<(vk::PipelineLayout, vk::DescriptorSetLayout, Vec<(u32, vk::DescriptorType)>), Error> {
        let buffers: Vec<(u32, vk::DescriptorType)> = shader
            .buffers
            .iter()
            .map(|&(b, kind)| {
                let ty = match kind {
                    BufferKind::Uniform => vk::DescriptorType::UNIFORM_BUFFER,
                    _ => vk::DescriptorType::STORAGE_BUFFER,
                };
                (b, ty)
            })
            .collect();
        let bindings: Vec<_> = buffers
            .iter()
            .map(|&(b, ty)| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(ty)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::ALL)
            })
            .collect();
        unsafe {
            let set_layout = self
                .device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default()
                        .bindings(&bindings)
                        .flags(vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR),
                    None,
                )
                .map_err(err("push descriptor layout"))?;
            let sets = [self.global_layout, set_layout];
            let ranges = [vk::PushConstantRange {
                stage_flags: vk::ShaderStageFlags::ALL,
                offset: 0,
                size: shader.push_constant_size,
            }];
            let mut info = vk::PipelineLayoutCreateInfo::default().set_layouts(&sets);
            if shader.push_constant_size > 0 {
                info = info.push_constant_ranges(&ranges);
            }
            let layout = self.device.create_pipeline_layout(&info, None).map_err(err("pipeline layout"))?;
            Ok((layout, set_layout, buffers))
        }
    }
}

impl Backend for VulkanBackend {
    fn name(&self) -> &str {
        "vulkan"
    }

    fn device_name(&self) -> &str {
        &self.device_name
    }

    fn frames_in_flight(&self) -> usize {
        FRAMES
    }

    fn surface_size(&self) -> (u32, u32) {
        self.swapchain.as_ref().map_or(self.want_size, |s| (s.extent.width, s.extent.height))
    }

    fn resize(&mut self, width: u32, height: u32) {
        if self.want_size != (width, height) {
            self.want_size = (width, height);
            self.dirty_swapchain = true;
        }
    }

    fn set_vsync(&mut self, vsync: bool) {
        if self.vsync != vsync {
            self.vsync = vsync;
            self.dirty_swapchain = true;
        }
    }

    fn create_buffer(&mut self, size: u64, usage: u32, label: &str) -> BufferId {
        let mut flags = vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::TRANSFER_SRC;
        if usage & usage::STORAGE != 0 {
            flags |= vk::BufferUsageFlags::STORAGE_BUFFER;
        }
        if usage & usage::INDEX != 0 {
            flags |= vk::BufferUsageFlags::INDEX_BUFFER;
        }
        if usage & usage::INDIRECT != 0 {
            flags |= vk::BufferUsageFlags::INDIRECT_BUFFER;
        }
        if usage & usage::UNIFORM != 0 {
            flags |= vk::BufferUsageFlags::UNIFORM_BUFFER;
        }
        let location = if usage & usage::READBACK != 0 {
            MemoryLocation::GpuToCpu
        } else if usage & usage::HOST != 0 {
            MemoryLocation::CpuToGpu
        } else {
            MemoryLocation::GpuOnly
        };
        let (raw, alloc) = raw_buffer(&self.device, &mut self.allocator, size, flags, location, label);
        BufferId(self.buffers.insert(Buffer { raw, alloc, size }))
    }

    fn mapped(&mut self, buffer: BufferId) -> &mut [u8] {
        let b = self.buffers.get_mut(buffer.0);
        let size = b.size as usize;
        &mut b.alloc.mapped_slice_mut().expect("buffer is not host visible")[..size]
    }

    fn write_buffer(&mut self, buffer: BufferId, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        debug_assert!(offset + data.len() as u64 <= self.buffers.get(buffer.0).size);
        let src = self.stage(data);
        self.uploads.push(Upload::Buffer { src, dst: buffer.0, offset, size: data.len() as u64 });
    }

    fn destroy_buffer(&mut self, buffer: BufferId) {
        if let Some(b) = self.buffers.remove(buffer.0) {
            self.frames[self.slot].deferred.push(Deferred::Buffer(b.raw, b.alloc));
        }
    }

    fn create_texture(&mut self, desc: &TextureDesc, label: &str) -> TextureId {
        let format = self.vk_format(desc.format);
        let depth = desc.format == Format::Depth32;
        let usage = if depth {
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT
        } else {
            let mut u = vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST;
            if desc.render_target {
                u |= vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC;
            }
            u
        };
        unsafe {
            let image = self
                .device
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D { width: desc.width, height: desc.height, depth: 1 })
                        .mip_levels(desc.mips)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(usage),
                    None,
                )
                .expect("vkCreateImage");
            let requirements = self.device.get_image_memory_requirements(image);
            let alloc = self
                .allocator
                .allocate(&AllocationCreateDesc {
                    name: label,
                    requirements,
                    location: MemoryLocation::GpuOnly,
                    linear: false,
                    allocation_scheme: AllocationScheme::GpuAllocatorManaged,
                })
                .unwrap_or_else(|e| panic!("allocating texture {label}: {e}"));
            self.device.bind_image_memory(image, alloc.memory(), alloc.offset()).expect("bind image memory");
            let range = if depth { depth_range() } else { color_range(desc.mips) };
            let view = self
                .device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .subresource_range(range),
                    None,
                )
                .expect("vkCreateImageView");

            let index = if depth {
                u32::MAX
            } else {
                let index = self.free_tex.pop().unwrap_or_else(|| {
                    self.next_tex += 1;
                    self.next_tex - 1
                });
                assert!(index < MAX_TEXTURES, "more than {MAX_TEXTURES} textures");
                let info = [vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let write = vk::WriteDescriptorSet::default()
                    .dst_set(self.global_set)
                    .dst_binding(0)
                    .dst_array_element(index)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&info);
                self.device.update_descriptor_sets(&[write], &[]);
                index
            };
            let id = self.textures.insert(Texture {
                image,
                view,
                alloc,
                desc: desc.clone(),
                layout: vk::ImageLayout::UNDEFINED,
                index,
            });
            self.uploads.push(Upload::InitTexture(id));
            TextureId(id)
        }
    }

    fn write_texture(&mut self, texture: TextureId, mip: u32, data: &[u8]) {
        let src = self.stage(data);
        self.uploads.push(Upload::Texture { src, dst: texture.0, mip });
    }

    fn texture_index(&self, texture: TextureId) -> u32 {
        self.textures.get(texture.0).index
    }

    fn destroy_texture(&mut self, texture: TextureId) {
        if let Some(t) = self.textures.remove(texture.0) {
            self.frames[self.slot].deferred.push(Deferred::Texture(
                t.image,
                t.view,
                t.alloc,
                t.index,
            ));
        }
    }

    fn create_pipeline(&mut self, desc: &PipelineDesc) -> Result<PipelineId, Error> {
        let vs = self.spirv(desc.shader, naga::ShaderStage::Vertex, desc.vertex)?;
        let fs = match desc.fragment {
            Some(f) => Some(self.spirv(desc.shader, naga::ShaderStage::Fragment, f)?),
            None => None,
        };
        let (layout, set_layout, buffers) = self.layout_for(desc.shader)?;
        let vs_name = CString::new(desc.vertex).unwrap();
        let fs_name = CString::new(desc.fragment.unwrap_or("main")).unwrap();
        let mut stages = vec![
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(&vs_name),
        ];
        if let Some(fs) = fs {
            stages.push(
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fs)
                    .name(&fs_name),
            );
        }
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(match desc.topology {
            Topology::Triangles => vk::PrimitiveTopology::TRIANGLE_LIST,
            Topology::Lines => vk::PrimitiveTopology::LINE_LIST,
        });
        let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(if desc.cull_back { vk::CullModeFlags::BACK } else { vk::CullModeFlags::NONE })
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample =
            vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let depth = match desc.depth {
            Some(d) => vk::PipelineDepthStencilStateCreateInfo::default()
                .depth_test_enable(true)
                .depth_write_enable(d.write)
                .depth_compare_op(compare_op(d.compare)),
            None => vk::PipelineDepthStencilStateCreateInfo::default(),
        };
        let blend_attachment = {
            let a = vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA);
            match desc.blend {
                Blend::None => a,
                Blend::Alpha => a
                    .blend_enable(true)
                    .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                    .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .color_blend_op(vk::BlendOp::ADD)
                    .src_alpha_blend_factor(vk::BlendFactor::ONE)
                    .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .alpha_blend_op(vk::BlendOp::ADD),
                Blend::Premultiplied => a
                    .blend_enable(true)
                    .src_color_blend_factor(vk::BlendFactor::ONE)
                    .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .color_blend_op(vk::BlendOp::ADD)
                    .src_alpha_blend_factor(vk::BlendFactor::ONE)
                    .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .alpha_blend_op(vk::BlendOp::ADD),
                Blend::Additive => a
                    .blend_enable(true)
                    .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                    .dst_color_blend_factor(vk::BlendFactor::ONE)
                    .color_blend_op(vk::BlendOp::ADD)
                    .src_alpha_blend_factor(vk::BlendFactor::ZERO)
                    .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                    .alpha_blend_op(vk::BlendOp::ADD),
            }
        };
        let attachments = [blend_attachment];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(if desc.color.is_some() {
            &attachments
        } else {
            &[]
        });
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let color_formats = desc.color.map(|f| [self.vk_format(f)]);
        let mut rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(color_formats.as_ref().map_or(&[][..], |f| &f[..]))
            .depth_attachment_format(if desc.depth.is_some() { DEPTH_FORMAT } else { vk::Format::UNDEFINED });
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .push_next(&mut rendering);
        let result = unsafe {
            self.device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
        };
        unsafe {
            self.device.destroy_shader_module(vs, None);
            if let Some(fs) = fs {
                self.device.destroy_shader_module(fs, None);
            }
        }
        let raw = result.map_err(|(_, e)| Error(format!("pipeline {}: {e:?}", desc.label)))?[0];
        Ok(PipelineId(self.pipelines.insert(Pipeline {
            raw,
            layout,
            set_layout,
            bind_point: vk::PipelineBindPoint::GRAPHICS,
            buffers,
        })))
    }

    fn create_compute(&mut self, desc: &ComputeDesc) -> Result<PipelineId, Error> {
        let cs = self.spirv(desc.shader, naga::ShaderStage::Compute, desc.entry)?;
        let (layout, set_layout, buffers) = self.layout_for(desc.shader)?;
        let name = CString::new(desc.entry).unwrap();
        let stage =
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::COMPUTE).module(cs).name(&name);
        let info = vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout);
        let result = unsafe { self.device.create_compute_pipelines(vk::PipelineCache::null(), &[info], None) };
        unsafe { self.device.destroy_shader_module(cs, None) };
        let raw = result.map_err(|(_, e)| Error(format!("compute pipeline {}: {e:?}", desc.label)))?[0];
        Ok(PipelineId(self.pipelines.insert(Pipeline {
            raw,
            layout,
            set_layout,
            bind_point: vk::PipelineBindPoint::COMPUTE,
            buffers,
        })))
    }

    fn destroy_pipeline(&mut self, pipeline: PipelineId) {
        if let Some(p) = self.pipelines.remove(pipeline.0) {
            self.frames[self.slot].deferred.push(Deferred::Pipeline(p.raw, p.layout, p.set_layout));
        }
    }

    fn begin_frame(&mut self) -> Option<FrameInfo> {
        self.ensure_slot_ready();
        if self.dirty_swapchain || self.swapchain.is_none() {
            if let Err(e) = unsafe { self.recreate_swapchain() } {
                log::error!("{e}");
                return None;
            }
        }
        let sc = self.swapchain.as_ref()?;
        let acquired = self.frames[self.slot].acquired;
        let result = unsafe { self.swapchain_fn.acquire_next_image(sc.raw, u64::MAX, acquired, vk::Fence::null()) };
        match result {
            Ok((index, suboptimal)) => {
                if suboptimal {
                    self.dirty_swapchain = true;
                }
                self.image_index = Some(index);
                Some(FrameInfo { width: sc.extent.width, height: sc.extent.height, slot: self.slot })
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.dirty_swapchain = true;
                None
            }
            Err(e) => {
                log::error!("vkAcquireNextImageKHR: {e:?}");
                None
            }
        }
    }

    fn end_frame(&mut self, list: &CommandList) {
        let Some(image_index) = self.image_index.take() else { return };
        let frame_cmd = self.frames[self.slot].cmd;
        let frame_pool = self.frames[self.slot].pool;
        unsafe {
            let d = &self.device;
            d.reset_command_pool(frame_pool, vk::CommandPoolResetFlags::empty()).ok();
            d.begin_command_buffer(
                frame_cmd,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .ok();
        }
        let cmd = frame_cmd;
        unsafe {
            let first_query = self.slot as u32 * MAX_MARKS;
            self.device.cmd_reset_query_pool(cmd, self.queries, first_query, MAX_MARKS);
            self.device.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::TOP_OF_PIPE, self.queries, first_query);
            self.record_uploads(cmd);
            self.device.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::ALL_COMMANDS, self.queries, first_query + 1);
            let mut marks = 2;
            let sets = [self.global_set];
            let mut current: Option<u32> = None;
            let mut in_pass_targets: Vec<u32> = Vec::new();
            let (sc_image, sc_view, sc_extent) = {
                let sc = self.swapchain.as_ref().unwrap();
                (sc.images[image_index as usize], sc.views[image_index as usize], sc.extent)
            };
            for cmd_item in &list.cmds {
                match *cmd_item {
                    Cmd::BeginPass { color, depth } => {
                        let mut extent = sc_extent;
                        let mut color_att = None;
                        if let Some((target, clear)) = color {
                            let (image, view, old) = match target {
                                Target::Surface => {
                                    let sc = self.swapchain.as_mut().unwrap();
                                    let old = sc.layouts[image_index as usize];
                                    sc.layouts[image_index as usize] = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
                                    (sc_image, sc_view, old)
                                }
                                Target::Texture(t) => {
                                    let tex = self.textures.get_mut(t.0);
                                    extent = vk::Extent2D { width: tex.desc.width, height: tex.desc.height };
                                    let old = tex.layout;
                                    tex.layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
                                    in_pass_targets.push(t.0);
                                    (tex.image, tex.view, old)
                                }
                            };
                            let old = if clear.is_some() { vk::ImageLayout::UNDEFINED } else { old };
                            // Also when the layout stays: a pass loading what the last one wrote.
                            self.barrier_image(cmd, image, color_range(1), old, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
                            color_att = Some(
                                vk::RenderingAttachmentInfo::default()
                                    .image_view(view)
                                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                                    .load_op(if clear.is_some() {
                                        vk::AttachmentLoadOp::CLEAR
                                    } else if old == vk::ImageLayout::UNDEFINED {
                                        vk::AttachmentLoadOp::DONT_CARE
                                    } else {
                                        vk::AttachmentLoadOp::LOAD
                                    })
                                    .store_op(vk::AttachmentStoreOp::STORE)
                                    .clear_value(vk::ClearValue {
                                        color: vk::ClearColorValue { float32: clear.unwrap_or_default() },
                                    }),
                            );
                        }
                        let depth_att = depth.map(|(t, clear)| {
                            let tex = self.textures.get(t.0);
                            extent = vk::Extent2D { width: tex.desc.width, height: tex.desc.height };
                            vk::RenderingAttachmentInfo::default()
                                .image_view(tex.view)
                                .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                                .load_op(if clear.is_some() { vk::AttachmentLoadOp::CLEAR } else { vk::AttachmentLoadOp::LOAD })
                                .store_op(vk::AttachmentStoreOp::STORE)
                                .clear_value(vk::ClearValue {
                                    depth_stencil: vk::ClearDepthStencilValue { depth: clear.unwrap_or(0.0), stencil: 0 },
                                })
                        });
                        let colors: Vec<_> = color_att.into_iter().collect();
                        let mut info = vk::RenderingInfo::default()
                            .render_area(vk::Rect2D { offset: Default::default(), extent })
                            .layer_count(1)
                            .color_attachments(&colors);
                        if let Some(d) = depth_att.as_ref() {
                            info = info.depth_attachment(d);
                        }
                        self.device.cmd_begin_rendering(cmd, &info);
                        self.device.cmd_set_viewport(
                            cmd,
                            0,
                            &[vk::Viewport {
                                x: 0.0,
                                y: 0.0,
                                width: extent.width as f32,
                                height: extent.height as f32,
                                min_depth: 0.0,
                                max_depth: 1.0,
                            }],
                        );
                        self.device.cmd_set_scissor(cmd, 0, &[vk::Rect2D { offset: Default::default(), extent }]);
                    }
                    Cmd::EndPass => {
                        self.device.cmd_end_rendering(cmd);
                        for t in in_pass_targets.drain(..) {
                            let image = self.textures.get(t).image;
                            self.barrier_image(
                                cmd,
                                image,
                                color_range(1),
                                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            );
                            self.textures.get_mut(t).layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                        }
                    }
                    Cmd::SetPipeline(p) => {
                        let pl = self.pipelines.get(p.0);
                        self.device.cmd_bind_pipeline(cmd, pl.bind_point, pl.raw);
                        self.device.cmd_bind_descriptor_sets(cmd, pl.bind_point, pl.layout, 0, &sets, &[]);
                        current = Some(p.0);
                    }
                    Cmd::BindBuffer { binding, buffer, offset, size } => {
                        let Some(p) = current else { continue };
                        let pl = self.pipelines.get(p);
                        // A binding the shader never reads is not in its reflection (a shader
                        // pack's that skips a buffer): nothing to bind.
                        let Some(&(_, ty)) = pl.buffers.iter().find(|b| b.0 == binding) else { continue };
                        let b = self.buffers.get(buffer.0);
                        let range = if size == u64::MAX { vk::WHOLE_SIZE } else { size };
                        let info = [vk::DescriptorBufferInfo { buffer: b.raw, offset, range }];
                        let write = vk::WriteDescriptorSet::default()
                            .dst_binding(binding)
                            .descriptor_type(ty)
                            .buffer_info(&info);
                        self.push_fn.cmd_push_descriptor_set(cmd, pl.bind_point, pl.layout, 1, &[write]);
                    }
                    Cmd::PushConstants { start, len } => {
                        let Some(p) = current else { continue };
                        let pl = self.pipelines.get(p);
                        let bytes = &list.data[start as usize..(start + len) as usize];
                        self.device.cmd_push_constants(cmd, pl.layout, vk::ShaderStageFlags::ALL, 0, bytes);
                    }
                    Cmd::Scissor { x, y, w, h } => {
                        self.device.cmd_set_scissor(
                            cmd,
                            0,
                            &[vk::Rect2D { offset: vk::Offset2D { x, y }, extent: vk::Extent2D { width: w, height: h } }],
                        );
                    }
                    Cmd::Draw { vertices, instances, first_vertex, first_instance } => {
                        self.device.cmd_draw(cmd, vertices, instances, first_vertex, first_instance);
                    }
                    Cmd::DrawIndirect { buffer, offset, count } => {
                        let b = self.buffers.get(buffer.0).raw;
                        self.device.cmd_draw_indirect(cmd, b, offset, count, size_of::<DrawArgs>() as u32);
                    }
                    Cmd::IndexBuffer(buffer) => {
                        let b = self.buffers.get(buffer.0).raw;
                        self.device.cmd_bind_index_buffer(cmd, b, 0, vk::IndexType::UINT32);
                    }
                    Cmd::DrawIndexedIndirect { buffer, offset, count } => {
                        let b = self.buffers.get(buffer.0).raw;
                        self.device.cmd_draw_indexed_indirect(cmd, b, offset, count, size_of::<DrawIndexedArgs>() as u32);
                    }
                    Cmd::DrawIndirectCount { buffer, offset, count_buffer, count_offset, max } => {
                        let b = self.buffers.get(buffer.0).raw;
                        let c = self.buffers.get(count_buffer.0).raw;
                        self.device.cmd_draw_indirect_count(
                            cmd,
                            b,
                            offset,
                            c,
                            count_offset,
                            max,
                            size_of::<DrawArgs>() as u32,
                        );
                    }
                    Cmd::Dispatch { x, y, z } => self.device.cmd_dispatch(cmd, x, y, z),
                    Cmd::Barrier => self.memory_barrier(cmd),
                    Cmd::Mark => {
                        if marks < MAX_MARKS - 1 {
                            self.device.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::ALL_COMMANDS, self.queries, first_query + marks);
                            marks += 1;
                        }
                    }
                }
            }

            let old = self.swapchain.as_ref().unwrap().layouts[image_index as usize];
            if self.capture_requested {
                self.capture_requested = false;
                let (w, h) = (sc_extent.width, sc_extent.height);
                let (buf, alloc) = raw_buffer(
                    &self.device,
                    &mut self.allocator,
                    (w * h * 4) as u64,
                    vk::BufferUsageFlags::TRANSFER_DST,
                    MemoryLocation::GpuToCpu,
                    "capture",
                );
                self.barrier_image(cmd, sc_image, color_range(1), old, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
                let region = [vk::BufferImageCopy::default()
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D { width: w, height: h, depth: 1 })];
                self.device.cmd_copy_image_to_buffer(cmd, sc_image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buf, &region);
                self.barrier_image(cmd, sc_image, color_range(1), vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::ImageLayout::PRESENT_SRC_KHR);
                let fmt = self.swapchain.as_ref().unwrap().format;
                self.capture = Some((buf, alloc, w, h, fmt));
            } else {
                self.barrier_image(cmd, sc_image, color_range(1), old, vk::ImageLayout::PRESENT_SRC_KHR);
            }
            self.swapchain.as_mut().unwrap().layouts[image_index as usize] = vk::ImageLayout::PRESENT_SRC_KHR;
            self.device.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::ALL_COMMANDS, self.queries, first_query + marks);
            self.frames[self.slot].marks = marks + 1;
            self.device.end_command_buffer(cmd).ok();

            let frame = &mut self.frames[self.slot];
            let rendered = self.swapchain.as_ref().unwrap().rendered[image_index as usize];
            let waits = [vk::SemaphoreSubmitInfo::default()
                .semaphore(frame.acquired)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let signals = [vk::SemaphoreSubmitInfo::default()
                .semaphore(rendered)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            let submit = vk::SubmitInfo2::default()
                .wait_semaphore_infos(&waits)
                .command_buffer_infos(&cmds)
                .signal_semaphore_infos(&signals);
            self.device.reset_fences(&[frame.fence]).ok();
            if let Err(e) = self.device.queue_submit2(self.queue, &[submit], frame.fence) {
                log::error!("vkQueueSubmit2: {e:?}");
            }
            frame.waited = false;

            let swapchains = [self.swapchain.as_ref().unwrap().raw];
            let indices = [image_index];
            let wait = [rendered];
            let present =
                vk::PresentInfoKHR::default().wait_semaphores(&wait).swapchains(&swapchains).image_indices(&indices);
            match self.swapchain_fn.queue_present(self.queue, &present) {
                Ok(false) => {}
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.dirty_swapchain = true,
                Err(e) => log::error!("vkQueuePresentKHR: {e:?}"),
            }
        }
        self.slot = (self.slot + 1) % FRAMES;
    }

    fn request_capture(&mut self) {
        self.capture_requested = true;
    }

    fn gpu_times(&self) -> &[f32] {
        &self.gpu_times
    }

    fn take_capture(&mut self) -> Option<Capture> {
        let (buf, alloc, w, h, fmt) = self.capture.take()?;
        unsafe { self.device.device_wait_idle().ok() };
        let mut rgba = alloc.mapped_slice().unwrap()[..(w * h * 4) as usize].to_vec();
        if matches!(fmt, vk::Format::B8G8R8A8_SRGB | vk::Format::B8G8R8A8_UNORM) {
            for px in rgba.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        self.frames[self.slot].deferred.push(Deferred::Buffer(buf, alloc));
        Some(Capture { width: w, height: h, rgba })
    }

    fn wait_idle(&mut self) {
        unsafe { self.device.device_wait_idle().ok() };
    }
}

impl Drop for VulkanBackend {
    fn drop(&mut self) {
        unsafe {
            self.device.device_wait_idle().ok();
            for i in 0..self.frames.len() {
                let deferred = std::mem::take(&mut self.frames[i].deferred);
                for d in deferred {
                    self.destroy_now(d);
                }
                for (buffer, alloc, _) in std::mem::take(&mut self.frames[i].staging.chunks) {
                    self.device.destroy_buffer(buffer, None);
                    self.allocator.free(alloc).ok();
                }
                let f = &self.frames[i];
                self.device.destroy_command_pool(f.pool, None);
                self.device.destroy_semaphore(f.acquired, None);
                self.device.destroy_fence(f.fence, None);
            }
            self.device.destroy_query_pool(self.queries, None);
            if let Some((buf, alloc, ..)) = self.capture.take() {
                self.device.destroy_buffer(buf, None);
                self.allocator.free(alloc).ok();
            }
            for i in 0..self.buffers.items.len() as u32 {
                if let Some(b) = self.buffers.remove(i) {
                    self.device.destroy_buffer(b.raw, None);
                    self.allocator.free(b.alloc).ok();
                }
            }
            for i in 0..self.textures.items.len() as u32 {
                if let Some(t) = self.textures.remove(i) {
                    self.device.destroy_image_view(t.view, None);
                    self.device.destroy_image(t.image, None);
                    self.allocator.free(t.alloc).ok();
                }
            }
            for i in 0..self.pipelines.items.len() as u32 {
                if let Some(p) = self.pipelines.remove(i) {
                    self.device.destroy_pipeline(p.raw, None);
                    self.device.destroy_pipeline_layout(p.layout, None);
                    self.device.destroy_descriptor_set_layout(p.set_layout, None);
                }
            }
            for s in self.samplers {
                self.device.destroy_sampler(s, None);
            }
            self.device.destroy_descriptor_pool(self.global_pool, None);
            self.device.destroy_descriptor_set_layout(self.global_layout, None);
            if let Some(sc) = self.swapchain.take() {
                self.destroy_swapchain(sc);
            }
            ManuallyDrop::drop(&mut self.allocator);
            self.device.destroy_device(None);
            self.surface_fn.destroy_surface(self.surface, None);
            if let Some((f, m)) = self.debug.take() {
                f.destroy_debug_utils_messenger(m, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}
