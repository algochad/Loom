//! Draws meshed cubes. All face records live in a few big storage-buffer pages; each frame the CPU
//! culls cubes and writes one indirect draw per (cube, layer) plus the cube origin it uses.
//! Cubes of moving structures are meshed in the structure's own coordinates and drawn moved
//! by its pose (an offset per structure, `origin.w` in the shader): a block placed on one
//! remeshes only its cube.

use glam::{DVec3, IVec3, Mat4, Vec3, Vec4};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use loom_render::*;

use super::mesher::Mesh;
use super::textures::GameTextures;
use crate::content::Content;
pub use crate::gpu_layout::Globals;

const FACE_WORDS: u32 = 4;
const QUAD_WORDS: u32 = 8;
const PAGE_BYTES: u64 = 64 << 20;
/// A cube holds at most 6 * 4096 faces; plants and boxes add a few quads per block.
const MAX_QUADS_PER_DRAW: u32 = 6 * 4096 * 2;

#[derive(Clone, Copy)]
struct Alloc {
    page: u32,
    offset: u32,
    len: u32,
}

struct Pool {
    words: u32,
    pages: Vec<(BufferId, FreeList)>,
    label: &'static str,
}

/// Free ranges of a page by start and by (length, start): best fit and merging with the
/// neighbors in log time. A big edit frees and allocates tens of thousands of ranges at once,
/// which a scanned list took seconds for.
#[derive(Default)]
struct FreeList {
    by_start: std::collections::BTreeMap<u32, u32>,
    by_len: std::collections::BTreeSet<(u32, u32)>,
}

impl FreeList {
    fn add(&mut self, start: u32, len: u32) {
        self.by_start.insert(start, len);
        self.by_len.insert((len, start));
    }

    fn take(&mut self, start: u32, len: u32) {
        self.by_start.remove(&start);
        self.by_len.remove(&(len, start));
    }
}

impl Pool {
    fn records_per_page(&self) -> u32 {
        (PAGE_BYTES / (self.words as u64 * 4)) as u32
    }

    fn alloc(&mut self, gpu: &mut dyn Backend, n: u32) -> Alloc {
        let per = self.records_per_page();
        assert!(n <= per, "mesh larger than a page");
        for (pi, (_, free)) in self.pages.iter_mut().enumerate() {
            if let Some(&(len, start)) = free.by_len.range((n, 0)..).next() {
                free.take(start, len);
                if len > n {
                    free.add(start + n, len - n);
                }
                return Alloc { page: pi as u32, offset: start, len: n };
            }
        }
        let buf = gpu.create_buffer(PAGE_BYTES, usage::STORAGE, self.label);
        let mut free = FreeList::default();
        if per > n {
            free.add(n, per - n);
        }
        self.pages.push((buf, free));
        Alloc { page: self.pages.len() as u32 - 1, offset: 0, len: n }
    }

    fn free(&mut self, a: Alloc) {
        let free = &mut self.pages[a.page as usize].1;
        let (mut start, mut len) = (a.offset, a.len);
        if let Some((&s, &l)) = free.by_start.range(..start).next_back().filter(|(s, l)| **s + **l == start) {
            free.take(s, l);
            (start, len) = (s, l + len);
        }
        if let Some(&l) = free.by_start.get(&(start + len)) {
            free.take(start + len, l);
            len += l;
        }
        free.add(start, len);
    }
}

#[derive(Clone, Copy)]
struct GpuMesh {
    faces: Option<Alloc>,
    /// Per layer, per direction rank (see `mesher::DIR_RANK`).
    face_counts: [[u32; 6]; 3],
    quads: Option<Alloc>,
    quad_counts: [u32; 3],
}

#[derive(Clone, Copy)]
pub struct Camera {
    pub pos: DVec3,
    pub yaw: f32,
    pub pitch: f32,
    pub fov: f32,
    /// Walking sway in view space: offset, roll and extra pitch (vanilla's view bobbing).
    pub bob: (Vec3, f32, f32),
}

impl Camera {
    pub fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(-sy * cp, sp, -cy * cp)
    }

    pub fn view(&self) -> Mat4 {
        let (offset, roll, pitch) = self.bob;
        Mat4::from_translation(offset) * Mat4::from_rotation_z(roll) * Mat4::from_rotation_x(pitch) * glam::camera::rh::view::look_to_mat4(Vec3::ZERO, self.forward(), Vec3::Y)
    }

    pub fn proj(&self, aspect: f32) -> Mat4 {
        // Y-up clip space: naga already flips Y for Vulkan.
        glam::camera::rh::proj::directx::perspective_infinite_reverse(self.fov.to_radians(), aspect, 0.05)
    }
}

/// One indirect draw: kind (0 faces, 1 quads, 3 distant terrain), layer, page, arguments, cube origin (w: offset
/// index), cube center relative to the camera.
type DrawRecord = (u8, u8, u32, DrawIndexedArgs, [i32; 4], Vec3);

/// A mapped buffer written from every core, each record by one.
#[derive(Clone, Copy)]
struct Mapped(*mut u8);

unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

impl Mapped {
    /// SAFETY: `off` + what is written there stays inside the buffer.
    unsafe fn at(&self, off: usize) -> *mut u8 {
        unsafe { self.0.add(off) }
    }
}

/// Meshes are kept in regions of this many cubes a side (log2), culled as a whole first.
const REGION: i32 = 3;

fn region_of(key: MeshKey) -> (u64, IVec3) {
    (key.0, key.1 >> REGION)
}

/// The draws of one visible mesh (its corner `lo` relative to the camera, `k` its structure or
/// 0), returning the faces drawn. A face direction can be seen only from the side its normal
/// points to; contiguous visible runs become one draw.
fn mesh_draws(m: &GpuMesh, key: MeshKey, lo: Vec3, k: i32, cam_cube: IVec3, out: &mut Vec<DrawRecord>) -> u64 {
    let rel = if k == 0 { key.1 - cam_cube } else { key.1 };
    let origin = [rel.x, rel.y, rel.z, k];
    let center = lo + Vec3::splat(8.0);
    let mut faces = 0u64;
    let args = |s: u32, len: u32| DrawIndexedArgs { indices: len * 6, instances: 1, first_index: 0, vertex_offset: (s * 4) as i32, first_instance: 0 };
    if let Some(a) = m.faces {
        // Directions by rank: +X +Y +Z -X -Y -Z.
        let seen = [lo.x < 0.0, lo.y < 0.0, lo.z < 0.0, lo.x > -16.0, lo.y > -16.0, lo.z > -16.0];
        let mut off = a.offset;
        for layer in 0..3 {
            let counts = m.face_counts[layer];
            let mut run: Option<(u32, u32)> = None;
            for rank in 0..6 {
                let n = counts[rank];
                if n > 0 && seen[rank] {
                    faces += n as u64;
                    run = Some(match run {
                        Some((s, len)) => (s, len + n),
                        None => (off, n),
                    });
                } else if n > 0 {
                    if let Some((s, len)) = run.take() {
                        out.push((0, layer as u8, a.page, args(s, len), origin, center));
                    }
                }
                off += n;
            }
            if let Some((s, len)) = run {
                out.push((0, layer as u8, a.page, args(s, len), origin, center));
            }
        }
    }
    if let Some(a) = m.quads {
        let mut off = a.offset;
        for layer in 0..3 {
            let n = m.quad_counts[layer];
            if n > 0 {
                out.push((1, layer as u8, a.page, args(off, n), origin, center));
            }
            off += n;
        }
    }
    faces
}

/// A mesh's place: the structure (entity) it belongs to, 0 for the world, and its cube.
pub type MeshKey = (u64, IVec3);

/// Which blocks around the camera cast shadows, kept on the GPU for shader packs (`solid_at`
/// in the world shader's prelude); only while the chosen pack's world shader uses it. 16^3 cube
/// slots addressed by cube position modulo 16: first a tag per slot (cube x y z, 1 when
/// filled), then 128 words of bits per slot in `cube::index` order.
struct Solid {
    buf: BufferId,
    on: bool,
    /// Per slot: the cube it holds and the `cube_rev` of its blocks then.
    held: Vec<Option<(IVec3, u32)>>,
    /// The tags as uploaded: sent whole when one changes, since copies within a frame have no
    /// order between them and a slot's tag must never lose to an older write.
    tags: Vec<[u32; 4]>,
    /// Per block state: casts a shadow.
    casts: Vec<bool>,
}

const SOLID_SLOTS: usize = 4096;
/// Cubes rebuilt per frame at most; the rest follow in the next frames.
const SOLID_PER_FRAME: usize = 192;

impl Solid {
    fn new(gpu: &mut dyn Backend, c: &Content) -> Solid {
        let casts = c.states.iter().map(|st| st.opacity > 0 && st.liquid.is_none()).collect();
        let mut s = Solid { buf: gpu.create_buffer(4, usage::STORAGE, "solid"), on: false, held: Vec::new(), tags: Vec::new(), casts };
        s.set_on(gpu, crate::packs::pack_uses("world.wgsl", "solid_at"));
        s
    }

    fn set_on(&mut self, gpu: &mut dyn Backend, on: bool) {
        if on == self.on {
            return;
        }
        gpu.destroy_buffer(self.buf);
        self.on = on;
        self.held = vec![None; if on { SOLID_SLOTS } else { 0 }];
        self.tags = vec![[0; 4]; if on { SOLID_SLOTS } else { 0 }];
        let words = if on { SOLID_SLOTS * (4 + 128) } else { 1 };
        self.buf = gpu.create_buffer(words as u64 * 4, usage::STORAGE, "solid");
        gpu.write_buffer(self.buf, 0, &vec![0u8; words * 4]);
    }
}

pub struct WorldRenderer {
    face_pipes: [PipelineId; 3],
    quad_pipes: [PipelineId; 3],
    lod_pipe: PipelineId,
    /// Distant terrain tiles (`lod`): their quads (in `faces`, four words like a face), count
    /// and the heights they span; the ones to draw this frame; and the columns whose cubes show
    /// their ground (`Lod::mask`, half its width), where their pixels are left to the cubes.
    lod: FxHashMap<super::lod::TileKey, (Alloc, u32, (i32, i32))>,
    pub lod_draw: Vec<super::lod::TileKey>,
    pub lod_half: u32,
    lod_mask: (BufferId, usize),
    faces: Pool,
    quads: Pool,
    /// Index into its region's meshes by key. A frame culls the regions, then goes through the
    /// meshes of the ones seen, nearest first, in parallel.
    meshes: FxHashMap<MeshKey, usize>,
    regions: FxHashMap<(u64, IVec3), Vec<(MeshKey, GpuMesh)>>,
    globals: Vec<BufferId>,
    origins: Vec<(BufferId, u32)>,
    /// Per frame: camera-relative positions of the structures drawn (index 0: none, the world).
    offsets: Vec<(BufferId, u32)>,
    indirect: Vec<(BufferId, u32)>,
    /// Of the last `draw`: its runs (kind, layer, page, first record, records), and its
    /// translucent draws, sorted far to near.
    runs: Vec<(u8, u8, u32, u32, u32)>,
    trans: Vec<DrawRecord>,
    /// Draws of each region seen, kept between frames.
    parts: Vec<Vec<DrawRecord>>,
    /// Of the last `draw`: where its translucent draws start in `scratch.2`, and the buffers
    /// they use (globals, origins, indirect, offsets, the frame's scene).
    translucent: usize,
    buffers: Option<(BufferId, BufferId, BufferId, BufferId, BufferId)>,
    /// Biome table, then per block column of the loaded area its blended colors: tiles of
    /// 16x16 blocks addressed by column position modulo `tint_tiles`.
    biomes: BufferId,
    biome_table: Vec<u32>,
    tint_tiles: u32,
    tex_info: BufferId,
    anim: BufferId,
    /// Start of the overlay table in `anim`.
    overlays: u32,
    /// Block shader slot per texture id (`shader_table`).
    tex_shader: BufferId,
    /// `GameTextures::glow_buffer`, bound as it is.
    glow: BufferId,
    /// `GameTextures::generation` the tables were built from.
    tex_generation: u64,
    /// (0 1 2)(0 2 3) for as many quads as one draw can hold; every face and quad draw shares it
    /// so each corner is shaded once instead of once per triangle.
    quad_index: BufferId,
    solid: Solid,
    pub drawn: u32,
    pub faces_drawn: u64,
}

/// Block shaders: `fn shade(p: BlockPixel) -> vec4<f32>` in `shaders/block/<name>.wgsl`, for
/// blocks with a `shader` component.
pub const HOOK: crate::shaders::Hook = crate::shaders::Hook {
    kind: "block",
    entry: "shade",
    dispatch: "block_shade",
    params: "p: BlockPixel",
    args: "p",
    ret: "vec4<f32>",
    fallback: "return vec4(p.albedo.rgb * p.light, p.albedo.a);",
};

pub fn world_shader(assets: &crate::assets::Assets) -> Result<(Shader, crate::shaders::Slots), ShaderError> {
    let snippets = crate::shaders::snippets(assets, &HOOK);
    let (s, mut slots) = crate::packs::compose(assets, "world.wgsl", &[(&HOOK, &snippets)])?;
    Ok((s, slots.pop().unwrap_or_default()))
}

/// Block shader slot per texture id: a block's `shader` goes with its textures.
fn shader_table(gpu: &mut dyn Backend, c: &Content, textures: &GameTextures, slots: &crate::shaders::Slots) -> BufferId {
    let mut table = vec![0u32; textures.count().max(1)];
    if let Some(comp) = c.components.id(loom_abi::comp::SHADER) {
        for (i, s) in c.states.iter().enumerate() {
            let Some(name) = c.components.text(crate::components::BLOCK, i as u32, comp) else { continue };
            let slot = slots.slot(name);
            if slot == 0 {
                log::warn!("block {}: no block shader {name}", c.block_of(i as u32).name);
                continue;
            }
            for t in s.textures.iter().chain(s.boxes.iter().flat_map(|b| b.textures.iter())) {
                if let Some(e) = table.get_mut(*t as usize) {
                    *e = slot;
                }
            }
        }
    }
    let buf = gpu.create_buffer((table.len() * 4) as u64, usage::STORAGE, "texture shaders");
    gpu.write_buffer(buf, 0, bytemuck::cast_slice(&table));
    buf
}

impl WorldRenderer {
    pub fn new(gpu: &mut dyn Backend, c: &Content, textures: &GameTextures, assets: &crate::assets::Assets) -> Result<WorldRenderer, String> {
        let (shader, slots) = world_shader(assets).map_err(|e| e.0)?;
        let (face_pipes, quad_pipes, lod_pipe) = Self::pipelines(gpu, &shader)?;
        let tex_shader = shader_table(gpu, c, textures, &slots);

        let (tex_info, anim, overlays) = Self::texture_info(gpu, textures);

        // Tint kind colors per biome, then the blended columns (`set_tint_tiles`).
        let biome_data: Vec<u32> = if c.biomes.is_empty() {
            c.tint_kinds.iter().map(|k| k.1).collect()
        } else {
            c.biomes.iter().flat_map(|b| b.tints.iter().copied()).collect()
        };
        let biomes = gpu.create_buffer((biome_data.len() * 4) as u64, usage::STORAGE, "biomes");
        gpu.write_buffer(biomes, 0, bytemuck::cast_slice(&biome_data));

        let indices: Vec<u32> = (0..MAX_QUADS_PER_DRAW).flat_map(|q| [0, 1, 2, 0, 2, 3].map(|i| q * 4 + i)).collect();
        let quad_index = gpu.create_buffer((indices.len() * 4) as u64, usage::INDEX, "quad indices");
        gpu.write_buffer(quad_index, 0, bytemuck::cast_slice(&indices));

        let frames = gpu.frames_in_flight();
        let globals = (0..frames).map(|_| gpu.create_buffer(size_of::<Globals>() as u64, usage::STORAGE | usage::HOST, "globals")).collect();
        Ok(WorldRenderer {
            face_pipes,
            quad_pipes,
            lod_pipe,
            lod: FxHashMap::default(),
            lod_draw: Vec::new(),
            lod_half: 0,
            lod_mask: (gpu.create_buffer(4, usage::STORAGE, "lod mask"), 1),
            faces: Pool { words: FACE_WORDS, pages: Vec::new(), label: "face page" },
            quads: Pool { words: QUAD_WORDS, pages: Vec::new(), label: "quad page" },
            meshes: FxHashMap::default(),
            regions: FxHashMap::default(),
            parts: Vec::new(),
            offsets: Vec::new(),
            globals,
            origins: Vec::new(),
            indirect: Vec::new(),
            runs: Vec::new(),
            trans: Vec::new(),
            translucent: 0,
            buffers: None,
            biomes,
            biome_table: biome_data,
            tint_tiles: 0,
            tex_info,
            anim,
            overlays,
            glow: textures.glow_buffer(),
            tex_shader,
            tex_generation: textures.generation(),
            quad_index,
            solid: Solid::new(gpu, c),
            drawn: 0,
            faces_drawn: 0,
        })
    }

    /// The block texture tables (`GameTextures::world_tables`) on the GPU.
    fn texture_info(gpu: &mut dyn Backend, textures: &GameTextures) -> (BufferId, BufferId, u32) {
        let (info, frames, overlays) = textures.world_tables();
        let tex_info = gpu.create_buffer((info.len().max(1) * 4) as u64, usage::STORAGE, "texture info");
        gpu.write_buffer(tex_info, 0, bytemuck::cast_slice(&info));
        let anim = gpu.create_buffer((frames.len() * 4) as u64, usage::STORAGE, "texture frames");
        gpu.write_buffer(anim, 0, bytemuck::cast_slice(&frames));
        (tex_info, anim, overlays)
    }

    /// Builds the texture tables again when textures were loaded since (cheap: a few words per
    /// block texture).
    pub fn set_textures(&mut self, gpu: &mut dyn Backend, textures: &GameTextures) {
        self.glow = textures.glow_buffer();
        if self.tex_generation == textures.generation() {
            return;
        }
        gpu.destroy_buffer(self.tex_info);
        gpu.destroy_buffer(self.anim);
        (self.tex_info, self.anim, self.overlays) = Self::texture_info(gpu, textures);
        self.tex_generation = textures.generation();
    }

    fn pipelines(gpu: &mut dyn Backend, shader: &Shader) -> Result<([PipelineId; 3], [PipelineId; 3], PipelineId), String> {
        let depth_write = Some(DepthState { write: true, compare: Compare::GreaterEqual });
        let depth_read = Some(DepthState { write: false, compare: Compare::GreaterEqual });
        let mut make = |label: &str, vs: &str, fs: &str, depth, blend, cull| {
            gpu.create_pipeline(&PipelineDesc {
                label,
                shader,
                vertex: vs,
                fragment: Some(fs),
                color: Some(Format::Surface),
                depth,
                blend,
                cull_back: cull,
                topology: Topology::Triangles,
            })
            .map_err(|e| e.0)
        };
        let faces = [
            make("faces opaque", "vs_face", "fs_opaque", depth_write, Blend::None, true)?,
            make("faces cutout", "vs_face", "fs_cutout", depth_write, Blend::None, true)?,
            make("faces translucent", "vs_face", "fs_translucent", depth_read, Blend::Alpha, true)?,
        ];
        let quads = [
            make("quads opaque", "vs_quad", "fs_opaque", depth_write, Blend::None, true)?,
            make("quads cutout", "vs_quad", "fs_cutout", depth_write, Blend::None, true)?,
            make("quads translucent", "vs_quad", "fs_translucent", depth_read, Blend::Alpha, true)?,
        ];
        let lod = make("distant terrain", "vs_lod", "fs_lod", depth_write, Blend::None, true)?;
        Ok((faces, quads, lod))
    }

    /// Recompiles the world shader; keeps the old pipelines when the new source has errors.
    pub fn reload_shaders(&mut self, gpu: &mut dyn Backend, textures: &GameTextures, assets: &crate::assets::Assets) {
        match world_shader(assets).map_err(|e| e.0).and_then(|(s, slots)| Ok((Self::pipelines(gpu, &s)?, slots))) {
            Ok(((f, q, l), slots)) => {
                for p in self.face_pipes.into_iter().chain(self.quad_pipes).chain([self.lod_pipe]) {
                    gpu.destroy_pipeline(p);
                }
                self.face_pipes = f;
                self.quad_pipes = q;
                self.lod_pipe = l;
                gpu.destroy_buffer(self.tex_shader);
                self.tex_shader = shader_table(gpu, crate::content::content(), textures, &slots);
                self.solid.set_on(gpu, crate::packs::pack_uses("world.wgsl", "solid_at"));
                log::info!("world shader reloaded");
            }
            Err(e) => log::error!("world shader: {e}"),
        }
    }

    /// Sizes the blended color area to `tiles` x `tiles` columns; true when it was reallocated
    /// and every column has to be uploaded again.
    pub fn set_tint_tiles(&mut self, gpu: &mut dyn Backend, tiles: u32) -> bool {
        if tiles == self.tint_tiles {
            return false;
        }
        gpu.destroy_buffer(self.biomes);
        let size = (self.biome_table.len() + (tiles * tiles) as usize * 256 * crate::content::content().tint_kinds.len()) * 4;
        self.biomes = gpu.create_buffer(size as u64, usage::STORAGE, "biomes");
        gpu.write_buffer(self.biomes, 0, bytemuck::cast_slice(&self.biome_table));
        // Zero marks a block without blended colors yet.
        gpu.write_buffer(self.biomes, self.biome_table.len() as u64 * 4, &vec![0u8; size - self.biome_table.len() * 4]);
        self.tint_tiles = tiles;
        true
    }

    pub fn upload_tints(&mut self, gpu: &mut dyn Backend, col: glam::IVec2, tints: &super::world::ColumnTints) {
        let t = self.tint_tiles as i32;
        let slot = (col.x.rem_euclid(t) + col.y.rem_euclid(t) * t) as usize;
        gpu.write_buffer(self.biomes, ((self.biome_table.len() + slot * tints.len()) * 4) as u64, bytemuck::cast_slice(tints));
    }

    pub fn remove(&mut self, key: MeshKey) {
        let Some(i) = self.meshes.remove(&key) else { return };
        let r = region_of(key);
        let list = self.regions.get_mut(&r).unwrap();
        let (_, m) = list.swap_remove(i);
        if let Some(&(moved, _)) = list.get(i) {
            self.meshes.insert(moved, i);
        }
        if list.is_empty() {
            self.regions.remove(&r);
        }
        if let Some(a) = m.faces {
            self.faces.free(a);
        }
        if let Some(a) = m.quads {
            self.quads.free(a);
        }
    }

    pub fn destroy(&mut self, gpu: &mut dyn Backend) {
        self.meshes.clear();
        self.regions.clear();
        self.lod.clear();
        for (b, _) in self.faces.pages.drain(..).chain(self.quads.pages.drain(..)) {
            gpu.destroy_buffer(b);
        }
        for b in self.globals.drain(..).chain([self.biomes, self.tex_info, self.anim, self.quad_index, self.tex_shader, self.lod_mask.0, self.solid.buf]) {
            gpu.destroy_buffer(b);
        }
        for (b, _) in self.origins.drain(..).chain(self.indirect.drain(..)).chain(self.offsets.drain(..)) {
            gpu.destroy_buffer(b);
        }
        for p in self.face_pipes.into_iter().chain(self.quad_pipes).chain([self.lod_pipe]) {
            gpu.destroy_pipeline(p);
        }
    }

    /// Brings `Solid` up to date for the cubes within 7 of `center` (their slots are distinct).
    pub fn update_solid(&mut self, gpu: &mut dyn Backend, world: &super::world::ClientWorld, center: IVec3) {
        let s = &mut self.solid;
        if !s.on {
            return;
        }
        let mut rebuilt = 0;
        let mut bits = [0u32; 128];
        // Nearest first, so a frame out of budget has fixed what is close.
        'shells: for r in 0i32..=7 {
            for dy in -r..=r {
                for dz in -r..=r {
                    for dx in -r..=r {
                        if dx.abs().max(dy.abs()).max(dz.abs()) != r {
                            continue;
                        }
                        let p = center + IVec3::new(dx, dy, dz);
                        let slot = ((p.y & 15) * 256 + (p.z & 15) * 16 + (p.x & 15)) as usize;
                        let now = world.cube_rev(p);
                        let key = now.map(|(_, v)| (p, v));
                        if s.held[slot] == key {
                            continue;
                        }
                        if rebuilt >= SOLID_PER_FRAME {
                            break 'shells;
                        }
                        rebuilt += 1;
                        s.held[slot] = key;
                        let Some((cube, _)) = now else {
                            s.tags[slot] = [0; 4];
                            continue;
                        };
                        let casts = |st: u32| s.casts.get(st as usize).copied().unwrap_or(false);
                        match cube.uniform_state() {
                            Some(st) => bits.fill(if casts(st) { !0 } else { 0 }),
                            None => {
                                bits.fill(0);
                                for i in 0..crate::world::cube::VOLUME {
                                    if casts(cube.get(i)) {
                                        bits[i / 32] |= 1 << (i % 32);
                                    }
                                }
                            }
                        }
                        gpu.write_buffer(s.buf, ((SOLID_SLOTS * 4 + slot * 128) * 4) as u64, bytemuck::cast_slice(&bits));
                        s.tags[slot] = [p.x as u32, p.y as u32, p.z as u32, 1];
                    }
                }
            }
        }
        if rebuilt > 0 {
            gpu.write_buffer(s.buf, 0, bytemuck::cast_slice(&s.tags));
        }
    }

    /// A distant terrain tile's quads (`lod::mesh`), spanning heights `y`.
    pub fn upload_lod(&mut self, gpu: &mut dyn Backend, textures: &GameTextures, key: super::lod::TileKey, quads: &[[u32; 4]], y: (i32, i32)) {
        self.remove_lod(key);
        if quads.is_empty() {
            return;
        }
        let mut used: Vec<u32> = quads.iter().map(|q| q[1] & crate::gpu_layout::face::TEX_MASK).collect();
        used.sort_unstable();
        used.dedup();
        textures.ensure_blocks(gpu, used);
        let a = self.faces.alloc(gpu, quads.len() as u32);
        gpu.write_buffer(self.faces.pages[a.page as usize].0, a.offset as u64 * 16, bytemuck::cast_slice(quads));
        self.lod.insert(key, (a, quads.len() as u32, y));
    }

    /// The columns shown by cubes (`Lod::mask`).
    pub fn set_lod_mask(&mut self, gpu: &mut dyn Backend, mask: &[u32]) {
        if self.lod_mask.1 < mask.len() {
            gpu.destroy_buffer(self.lod_mask.0);
            self.lod_mask = (gpu.create_buffer((mask.len() * 4) as u64, usage::STORAGE, "lod mask"), mask.len());
        }
        gpu.write_buffer(self.lod_mask.0, 0, bytemuck::cast_slice(mask));
    }

    /// Distant terrain tiles held and their quads.
    pub fn lod_counts(&self) -> (usize, usize) {
        (self.lod.len(), self.lod.values().map(|t| t.1 as usize).sum())
    }

    pub fn remove_lod(&mut self, key: super::lod::TileKey) {
        if let Some((a, _, _)) = self.lod.remove(&key) {
            self.faces.free(a);
        }
    }

    /// Every cube of structure `frame` goes (it left).
    pub fn remove_frame(&mut self, frame: u64) {
        let keys: Vec<MeshKey> = self.regions.iter().filter(|(r, _)| r.0 == frame).flat_map(|(_, l)| l.iter().map(|e| e.0)).collect();
        for k in keys {
            self.remove(k);
        }
    }

    pub fn upload(&mut self, gpu: &mut dyn Backend, textures: &GameTextures, key: MeshKey, mesh: &Mesh) {
        self.remove(key);
        if mesh.is_empty() {
            return;
        }
        // The textures it shows are loaded before it is drawn.
        let mut used: Vec<u32> = mesh.faces.iter().flatten().map(|f| f[1] & crate::gpu_layout::face::TEX_MASK).chain(mesh.quads.iter().flatten().map(|q| q[4] & crate::gpu_layout::face::TEX_MASK)).collect();
        used.sort_unstable();
        used.dedup();
        textures.ensure_blocks(gpu, used);
        let face_counts = mesh.faces.each_ref().map(|v| {
            let mut n = [0u32; 6];
            for f in v {
                n[super::mesher::DIR_RANK[crate::gpu_layout::face_dir(f)] as usize] += 1;
            }
            n
        });
        let quad_counts = mesh.quads.each_ref().map(|v| v.len() as u32);
        let total_f: u32 = face_counts.iter().flatten().sum();
        let total_q: u32 = quad_counts.iter().sum();
        let faces = (total_f > 0).then(|| {
            let a = self.faces.alloc(gpu, total_f);
            let data: Vec<[u32; 4]> = mesh.faces.concat();
            gpu.write_buffer(self.faces.pages[a.page as usize].0, a.offset as u64 * 16, bytemuck::cast_slice(&data));
            a
        });
        let quads = (total_q > 0).then(|| {
            let a = self.quads.alloc(gpu, total_q);
            let data: Vec<[u32; 8]> = mesh.quads.concat();
            gpu.write_buffer(self.quads.pages[a.page as usize].0, a.offset as u64 * 32, bytemuck::cast_slice(&data));
            a
        });
        let list = self.regions.entry(region_of(key)).or_default();
        self.meshes.insert(key, list.len());
        list.push((key, GpuMesh { faces, face_counts, quads, quad_counts }));
    }

    /// `frames`: structures to draw and where their block (0, 0, 0) is in the world.
    pub fn draw(&mut self, gpu: &mut dyn Backend, list: &mut CommandList, frame: &FrameInfo, cam: &Camera, scene: BufferId, frames: &[(u64, DVec3)]) {
        let aspect = frame.width as f32 / frame.height.max(1) as f32;
        let view_proj = cam.proj(aspect) * cam.view();
        let cam_block = cam.pos.floor();
        let cam_cube: IVec3 = cam_block.as_ivec3() >> 4i32;
        let cam_frac = (cam.pos - (cam_cube * 16).as_dvec3()).as_vec3();
        let globals = Globals {
            cam_frac: [cam_frac.x, cam_frac.y, cam_frac.z, 0.0],
            ids: [0, 0, (self.biome_table.len() / crate::content::content().tint_kinds.len()) as u32, self.tint_tiles],
            cam_cube: cam_cube.extend(0).to_array(),
            extra: [self.overlays, 0, crate::content::content().tint_kinds.len() as u32, self.lod_half],
        };
        let gbuf = self.globals[frame.slot];
        gpu.mapped(gbuf)[..size_of::<Globals>()].copy_from_slice(bytemuck::bytes_of(&globals));

        let planes = frustum_planes(view_proj);
        // Camera-relative offset of each structure drawn (k + 1 its index; 0 the world).
        let frame_offsets: Vec<Vec3> = frames.iter().map(|f| (f.1 - (cam_cube * 16).as_dvec3()).as_vec3()).collect();
        // Regions in view, nearest first: what is drawn first then hides what is behind it.
        let side = (16 << REGION) as f32;
        let mut seen: Vec<(f32, &Vec<(MeshKey, GpuMesh)>)> = self
            .regions
            .iter()
            .filter_map(|(&(f, r), list)| {
                if f != 0 {
                    return Some((0.0, list));
                }
                let center = ((r << REGION) - cam_cube).as_vec3() * 16.0 - cam_frac + Vec3::splat(side / 2.0);
                planes.iter().all(|pl| pl.dot(center.extend(1.0)) > -side * 0.8661).then(|| (center.length_squared(), list))
            })
            .collect();
        seen.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        
        
        // A list per region seen, kept between frames: allocating them anew on every core each
        // frame cost more than the culling.
        let mut parts = std::mem::take(&mut self.parts);
        parts.resize_with(seen.len().max(parts.len()), Vec::new);
        let faces_drawn: u64 = seen
            .par_iter()
            .zip(parts.par_iter_mut())
            .map(|((_, list), out)| {
                out.clear();
                let mut faces = 0u64;
                for &(key, m) in list.iter() {
                    let (lo, k) = if key.0 == 0 {
                        ((key.1 - cam_cube).as_vec3() * 16.0 - cam_frac, 0)
                    } else {
                        let Some(k) = frames.iter().position(|f| f.0 == key.0) else { continue };
                        (frame_offsets[k] + key.1.as_vec3() * 16.0 - cam_frac, k as i32 + 1)
                    };
                    let center = lo + Vec3::splat(8.0);
                    if planes.iter().all(|pl| pl.dot(center.extend(1.0)) > -13.86) {
                        faces += mesh_draws(&m, key, lo, k, cam_cube, out);
                    }
                }
                faces
            })
            .sum();
        // Into runs of one pipeline and page (layer, kind, page; translucent last), written
        // straight into the mapped buffers from every core: each region knows where its draws
        // of each run go, so the near-to-far order holds inside runs without a sort.
        let seen_parts = &parts[..seen.len()];
        let pages = self.faces.pages.len().max(self.quads.pages.len()).max(1);
        // Opaque and cutout runs; translucent ones are sorted far to near over all of them.
        let nb = 4 * pages;
        let bucket = |d: &DrawRecord| (d.1 as usize * 2 + d.0 as usize) * pages + d.2 as usize;
        let counts: Vec<Vec<u32>> = seen_parts
            .par_iter()
            .map(|p| {
                let mut c = vec![0u32; nb];
                for d in p.iter().filter(|d| d.1 < 2) {
                    c[bucket(d)] += 1;
                }
                c
            })
            .collect();
        let mut totals = vec![0u32; nb];
        for c in &counts {
            for (t, v) in totals.iter_mut().zip(c) {
                *t += v;
            }
        }
        let mut next = Vec::with_capacity(nb);
        let mut opaque = 0u32;
        for t in &totals {
            next.push(opaque);
            opaque += t;
        }
        let mut starts: Vec<Vec<u32>> = Vec::with_capacity(counts.len());
        for c in &counts {
            starts.push(next.clone());
            for (n, v) in next.iter_mut().zip(c) {
                *n += v;
            }
        }
        let mut trans = std::mem::take(&mut self.trans);
        trans.clear();
        for p in seen_parts {
            trans.extend(p.iter().filter(|d| d.1 == 2));
        }
        // Far first; distances are positive, so their bits order like them. In one cube, seen
        // from above, the quads (liquid surfaces) go first, from below the faces.
        let rank = |d: &DrawRecord| if d.5.y < 0.0 { d.0 == 0 } else { d.0 == 1 };
        trans.par_sort_unstable_by_key(|d| (std::cmp::Reverse(d.5.length_squared().to_bits()), rank(d)));

        // Distant terrain after the cubes (nearer, they hide most of it), a run per page.
        let mut lod: Vec<DrawRecord> = self
            .lod_draw
            .iter()
            .filter_map(|key| {
                let &(a, count, (y0, y1)) = self.lod.get(key)?;
                let o = key.origin();
                let half = Vec3::new(key.size() as f32, (y1 - y0) as f32, key.size() as f32) / 2.0;
                let center = Vec3::new(o.x as f32, y0 as f32, o.y as f32) + half - (cam_cube * 16).as_vec3() - cam_frac;
                if !planes.iter().all(|pl| pl.dot(center.extend(1.0)) > -half.length()) {
                    return None;
                }
                let args = DrawIndexedArgs { indices: count * 6, instances: 1, first_index: 0, vertex_offset: (a.offset * 4) as i32, first_instance: 0 };
                Some((3, 0, a.page, args, [o.x / 16 - cam_cube.x, -cam_cube.y, o.y / 16 - cam_cube.z, 0], center))
            })
            .collect();
        lod.sort_by_key(|d| (d.2, d.5.length_squared().to_bits()));
        let lods = lod.len() as u32;
        let n = opaque + lods + trans.len() as u32;
        self.drawn = n;
        self.faces_drawn = faces_drawn;
        let mut runs = std::mem::take(&mut self.runs);
        runs.clear();
        if n == 0 {
            (self.parts, self.trans, self.runs) = (parts, trans, runs);
            self.buffers = None;
            return;
        }
        let obuf = crate::gpu_layout::frame_buffer(gpu, &mut self.origins, frame.slot, n, 16, usage::STORAGE, "origins");
        let fbuf = crate::gpu_layout::frame_buffer(gpu, &mut self.offsets, frame.slot, frames.len() as u32 + 1, 16, usage::STORAGE, "frame offsets");
        let offs = gpu.mapped(fbuf);
        offs[..16].fill(0);
        for (k, o) in frame_offsets.iter().enumerate() {
            offs[(k + 1) * 16..(k + 1) * 16 + 12].copy_from_slice(bytemuck::cast_slice(&o.to_array()));
        }
        let ibuf = crate::gpu_layout::frame_buffer(gpu, &mut self.indirect, frame.slot, n, 20, usage::INDIRECT, "indirect");
        let origins = Mapped(gpu.mapped(obuf)[..n as usize * 16].as_mut_ptr());
        let indirect = Mapped(gpu.mapped(ibuf)[..n as usize * 20].as_mut_ptr());
        let put = |k: u32, d: &DrawRecord| {
            let mut a = d.3;
            a.first_instance = k;
            // SAFETY: both buffers hold `n` records and every k below `n` is written once.
            unsafe {
                std::ptr::copy_nonoverlapping(bytemuck::bytes_of(&d.4).as_ptr(), origins.at(k as usize * 16), 16);
                std::ptr::copy_nonoverlapping(bytemuck::bytes_of(&a).as_ptr(), indirect.at(k as usize * 20), 20);
            }
        };
        seen_parts.par_iter().zip(starts.par_iter_mut()).for_each(|(p, at)| {
            for d in p.iter().filter(|d| d.1 < 2) {
                let b = bucket(d);
                put(at[b], d);
                at[b] += 1;
            }
        });
        for (j, d) in lod.iter().enumerate() {
            put(opaque + j as u32, d);
        }
        for (j, d) in trans.iter().enumerate() {
            put(opaque + lods + j as u32, d);
        }
        let mut start = 0;
        for (b, &t) in totals.iter().enumerate() {
            if t > 0 {
                runs.push(((b / pages % 2) as u8, (b / pages / 2) as u8, (b % pages) as u32, start, t));
            }
            start += t;
        }
        for (j, d) in lod.iter().enumerate() {
            match runs.last_mut() {
                Some(r) if r.0 == 3 && r.2 == d.2 => r.4 += 1,
                _ => runs.push((3, 0, d.2, opaque + j as u32, 1)),
            }
        }
        self.translucent = runs.len();
        for (j, d) in trans.iter().enumerate() {
            match runs.last_mut() {
                Some(r) if r.1 == 2 && (r.0, r.1, r.2) == (d.0, d.1, d.2) => r.4 += 1,
                _ => runs.push((d.0, d.1, d.2, opaque + lods + j as u32, 1)),
            }
        }
        (self.parts, self.trans, self.runs) = (parts, trans, runs);
        self.buffers = Some((gbuf, obuf, ibuf, fbuf, scene));
        self.emit(list, 0..self.translucent);
    }

    /// The translucent part of the last `draw`, after everything that may stand behind it.
    pub fn draw_translucent(&self, list: &mut CommandList) {
        let n = self.runs.len();
        if self.buffers.is_some() && self.translucent < n {
            self.emit(list, self.translucent..n);
        }
    }

    /// Runs `range` of the last `draw`.
    fn emit(&self, list: &mut CommandList, range: std::ops::Range<usize>) {
        let Some((gbuf, obuf, ibuf, fbuf, scene)) = self.buffers else { return };
        if range.is_empty() {
            return;
        }
        list.push(Cmd::IndexBuffer(self.quad_index));
        for &(kind, layer, page, start, count) in &self.runs[range] {
            let (pipe, buf) = if kind == 0 {
                (self.face_pipes[layer as usize], self.faces.pages[page as usize].0)
            } else if kind == 3 {
                (self.lod_pipe, self.faces.pages[page as usize].0)
            } else {
                (self.quad_pipes[layer as usize], self.quads.pages[page as usize].0)
            };
            list.pipeline(pipe);
            use crate::gpu_layout::bind::world as b;
            list.bind(b::G, gbuf);
            list.bind(b::ORIGINS, obuf);
            list.bind(b::RECORDS, buf);
            list.bind(b::BIOMES, self.biomes);
            list.bind(b::TEX_INFO, self.tex_info);
            list.bind(b::FRAMES, self.anim);
            list.bind(b::OFFSETS, fbuf);
            list.bind(b::GLOW_MAP, self.glow);
            list.bind(b::TEX_SHADER, self.tex_shader);
            list.bind(b::SCENE, scene);
            list.bind(b::LOD_MASK, self.lod_mask.0);
            list.bind(b::SOLID, self.solid.buf);
            list.push(Cmd::DrawIndexedIndirect { buffer: ibuf, offset: start as u64 * 20, count });
        }
    }
}

/// Left, right, bottom, top planes; the reverse-Z infinite projection has no useful far plane.
fn frustum_planes(m: Mat4) -> [Vec4; 4] {
    let r = |i: usize| m.row(i);
    let norm = |p: Vec4| p / p.truncate().length();
    [norm(r(3) + r(0)), norm(r(3) - r(0)), norm(r(3) + r(1)), norm(r(3) - r(1))]
}
