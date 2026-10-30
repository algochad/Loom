//! Screen effects mods turn on (`client_op::POST_EFFECT`): `shaders/post/<name>.wgsl` snippets in
//! one pipeline. With none on, the world is drawn straight to the screen; with some,
//! it goes into an image the effects read one after another, the last one onto the screen.

use loom_render::*;

use crate::assets::Assets;
use crate::shaders::{Hook, Slots};
use crate::gpu_layout::Pass;

pub const HOOK: Hook = Hook {
    kind: "post",
    entry: "apply",
    dispatch: "post_apply",
    params: "p: PostPixel",
    args: "p",
    ret: "vec4<f32>",
    fallback: "return p.color;",
};

pub struct Post {
    pipeline: Option<PipelineId>,
    slots: Slots,
    /// Effects on, in the order they were turned on: (name, strength).
    pub active: Vec<(String, f32)>,
    /// Two images the size of the screen (scene and the one after each pass), with their
    /// bindless indices.
    targets: Option<([TextureId; 2], [u32; 2], (u32, u32))>,
    params: Vec<BufferId>,
}

const MAX_PASSES: usize = 16;

impl Post {
    pub fn new(gpu: &mut dyn Backend, assets: &Assets) -> Post {
        let mut p = Post { pipeline: None, slots: Slots::default(), active: Vec::new(), targets: None, params: Vec::new() };
        p.reload(gpu, assets);
        p
    }

    pub fn reload(&mut self, gpu: &mut dyn Backend, assets: &Assets) {
        let snippets = crate::shaders::snippets(assets, &HOOK);
        let made = crate::packs::compose(assets, "post.wgsl", &[(&HOOK, &snippets)]).map_err(|e| e.0).and_then(|(s, mut slots)| {
            let p = gpu
                .create_pipeline(&PipelineDesc { label: "post", shader: &s, vertex: "vs", fragment: Some("fs"), color: Some(Format::Surface), depth: None, blend: Blend::None, cull_back: false, topology: Topology::Triangles })
                .map_err(|e| e.0)?;
            Ok((p, slots.pop().unwrap_or_default()))
        });
        match made {
            Ok((p, slots)) => {
                if let Some(old) = self.pipeline.replace(p) {
                    gpu.destroy_pipeline(old);
                }
                self.slots = slots;
            }
            Err(e) => log::error!("post shader: {e}"),
        }
    }

    /// Turns `name` on at `strength` (0 = off). False if no such effect exists.
    pub fn set(&mut self, name: &str, strength: f32) -> bool {
        let known = self.slots.slot(name) != 0;
        self.active.retain(|(n, _)| n != name);
        if strength > 0.0 && known {
            self.active.push((name.to_string(), strength));
        }
        known || strength <= 0.0
    }

    /// Whether this frame needs the scene drawn into an image first.
    pub fn on(&self) -> bool {
        self.pipeline.is_some() && !self.active.is_empty()
    }

    /// The image the scene is drawn into, sized to the frame.
    pub fn scene_target(&mut self, gpu: &mut dyn Backend, w: u32, h: u32) -> TextureId {
        if self.targets.is_none_or(|t| t.2 != (w, h)) {
            if let Some((old, ..)) = self.targets.take() {
                old.into_iter().for_each(|t| gpu.destroy_texture(t));
            }
            let desc = TextureDesc { width: w.max(1), height: h.max(1), mips: 1, format: Format::Surface, render_target: true };
            let t = [gpu.create_texture(&desc, "post a"), gpu.create_texture(&desc, "post b")];
            self.targets = Some((t, t.map(|t| gpu.texture_index(t)), (w, h)));
        }
        self.targets.unwrap().0[0]
    }

    /// Runs the effects over the scene; the last one draws onto the screen and leaves that pass
    /// open for the UI.
    pub fn apply(&mut self, gpu: &mut dyn Backend, list: &mut CommandList, frame: &FrameInfo) {
        let (Some(pipe), Some((tex, idx, _))) = (self.pipeline, self.targets) else { return };
        while self.params.len() < gpu.frames_in_flight() {
            self.params.push(gpu.create_buffer((size_of::<Pass>() * MAX_PASSES) as u64, usage::STORAGE | usage::HOST, "post passes"));
        }
        let time = crate::shaders::time();
        let n = self.active.len().min(MAX_PASSES);
        let passes: Vec<Pass> = self.active[..n]
            .iter()
            .enumerate()
            .map(|(i, (name, strength))| Pass { scene: idx[i % 2], slot: self.slots.slot(name), strength: *strength, time, size: [frame.width as f32, frame.height as f32], pad: [0.0; 2] })
            .collect();
        let buf = self.params[frame.slot];
        gpu.mapped(buf)[..passes.len() * size_of::<Pass>()].copy_from_slice(bytemuck::cast_slice(&passes));
        for i in 0..n {
            let target = if i + 1 == n { Target::Surface } else { Target::Texture(tex[(i + 1) % 2]) };
            list.push(Cmd::BeginPass { color: Some((target, Some([0.0; 4]))), depth: None });
            list.pipeline(pipe);
            list.bind(crate::gpu_layout::bind::post::PASSES, buf);
            list.push(Cmd::Draw { vertices: 3, instances: 1, first_vertex: 0, first_instance: i as u32 });
            if i + 1 < n {
                list.push(Cmd::EndPass);
            }
        }
    }

    pub fn destroy(&mut self, gpu: &mut dyn Backend) {
        if let Some((t, ..)) = self.targets.take() {
            t.into_iter().for_each(|t| gpu.destroy_texture(t));
        }
        if let Some(p) = self.pipeline.take() {
            gpu.destroy_pipeline(p);
        }
        for b in self.params.drain(..) {
            gpu.destroy_buffer(b);
        }
    }
}
