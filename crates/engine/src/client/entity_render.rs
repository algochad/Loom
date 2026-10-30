//! Entities: their models (`models.rs`, posed per frame and drawn instanced per kind), dropped
//! items, the held item, flames, the block selection outline, break progress and gizmo boxes
//! (textured boxes built on the CPU each frame, a few hundred at most).

use crate::gpu_layout::box_mode;
use super::models::{MAX_LAYERS, Posed};
use glam::{DVec3, IVec3, Mat4, Quat, Vec3, Vec4};
use rustc_hash::FxHashMap;
use loom_render::*;

use super::ClientEntity;
use super::models::{Instance, Models};
use super::textures::GameTextures;
use super::world_render::Camera;
use crate::assets::{Assets, decode_png};
use crate::content::content;
use crate::physics::Hit;
use crate::gpu_layout::{GpuBox, Scene};

/// What the first person view holds: the item (or the bare arm), the swing progress and the
/// walk distance for bobbing.
pub struct Hand {
    pub stack: loom_abi::ItemStack,
    pub swing: Option<f32>,
    /// The local player's animation started by the server and the player's age, for the `hand`
    /// bone of its keyframes.
    pub anim: Option<(String, f32, bool)>,
    pub age: f32,
}

/// Opaque pixels of a sprite, for drawing it as a 1/16 thick solid like vanilla's held items.
struct Sprite {
    size: u32,
    /// Opaque pixels and which of their sides touch another one (left, right, below, above).
    pixels: Vec<(u8, u8, u32)>,
}


pub struct EntityRenderer {
    hand_pipe: PipelineId,
    pipeline: PipelineId,
    overlay: PipelineId,
    /// Model pipelines for the screen and for the inventory figure's image.
    model_pipe: PipelineId,
    preview_pipe: PipelineId,
    pub models: Models,
    buffers: Vec<(BufferId, u32)>,
    /// The held item's projection: a fixed field of view, like vanilla, so it keeps its shape
    /// whatever the player's setting.
    hand_views: Vec<BufferId>,
    /// The inventory figure's view; it is drawn into its own texture so the UI can layer it like
    /// any image.
    preview: Vec<BufferId>,
    /// Color, depth, size, bindless index.
    preview_target: Option<(TextureId, TextureId, [u32; 2], u32)>,
    /// Texture, bindless index and size, by name; and the glow masks of those that have one.
    skins: FxHashMap<String, Option<(TextureId, u32, [u32; 2])>>,
    /// Players' own skins by entity, replacing the player kind's texture on them.
    player_skins: FxHashMap<u64, (TextureId, u32, [u32; 2])>,
    /// Skins received and not uploaded yet.
    pub skin_pending: Vec<(u64, Vec<u8>)>,
    /// The local player's entity.
    pub local: u64,
    glows: FxHashMap<String, (TextureId, u32)>,
    /// Shader slot of each entity kind.
    kind_shaders: Vec<u32>,
    sprites: FxHashMap<String, Option<Sprite>>,
    /// Entities to pose this frame, kept between frames.
    posed: Vec<Posed>,
    /// `preload`'s kinds and items already looked at this frame.
    seen_kinds: Vec<bool>,
    seen_items: Vec<bool>,
    /// Item models drawn instanced: pipelines for the world and the hand, each item's boxes
    /// (first, count) in `item_boxes`, the GPU copy (buffer, capacity, boxes uploaded) and the
    /// texture generation they were made with.
    item_pipe: PipelineId,
    hand_item_pipe: PipelineId,
    item_models: FxHashMap<u32, (u32, u32)>,
    item_boxes: Vec<GpuBox>,
    item_buf: Option<(BufferId, u32, usize)>,
    item_generation: u64,
    /// This frame's item instances: (item, instance).
    instances: Vec<(u32, GpuBox)>,
}

fn shader(assets: &Assets) -> Result<Shader, ShaderError> {
    crate::packs::shader(assets, "entity.wgsl")
}

pub const HOOK: crate::shaders::Hook = crate::shaders::Hook {
    kind: "entity",
    entry: "shade",
    dispatch: "entity_shade",
    params: "p: EntityPixel",
    args: "p",
    ret: "vec4<f32>",
    fallback: "return vec4(p.albedo.rgb * p.light, p.albedo.a);",
};

/// Light on boxes and model layers drawn without any (the inventory figure, flames).
const FULL_LIGHT: u32 = u32::MAX;

/// Gives boxes pushed since `from` the light they stand in (the shader turns it into color).
fn light_boxes(boxes: &mut [GpuBox], from: usize, light: u32) {
    for b in &mut boxes[from..] {
        b.ids2[1] = light;
    }
}

impl EntityRenderer {
    pub fn new(gpu: &mut dyn Backend, assets: &Assets) -> Result<EntityRenderer, String> {
        let (pipeline, overlay) = Self::pipelines(gpu, assets)?;
        let ([model_pipe, preview_pipe], kind_shaders) = Self::model_pipelines(gpu, assets)?;
        let models = Models::new(gpu, assets);
        let frames = gpu.frames_in_flight();
        let hand_views = (0..frames).map(|_| gpu.create_buffer(size_of::<Scene>() as u64, usage::STORAGE | usage::HOST, "hand scene")).collect();
        let preview = (0..frames).map(|_| gpu.create_buffer(size_of::<Scene>() as u64, usage::STORAGE | usage::HOST, "preview scene")).collect();
        let hand_pipe = Self::hand_pipeline(gpu, assets)?;
        let (item_pipe, hand_item_pipe) = Self::item_pipelines(gpu, assets)?;
        Ok(EntityRenderer {
            item_pipe,
            hand_item_pipe,
            item_models: FxHashMap::default(),
            item_boxes: Vec::new(),
            item_buf: None,
            item_generation: 0,
            instances: Vec::new(),
            seen_items: Vec::new(),
            pipeline,
            overlay,
            hand_pipe,
            model_pipe,
            models,
            buffers: Vec::new(),
            hand_views,
            preview,
            preview_pipe,
            preview_target: None,
            skins: FxHashMap::default(),
            player_skins: FxHashMap::default(),
            skin_pending: Vec::new(),
            local: 0,
            glows: FxHashMap::default(),
            kind_shaders,
            sprites: FxHashMap::default(),
            posed: Vec::new(),
            seen_kinds: Vec::new(),
        })
    }

    /// The first person hand: drawn last with its depth squeezed to the front, so it never sinks
    /// into walls.
    fn hand_pipeline(gpu: &mut dyn Backend, assets: &Assets) -> Result<PipelineId, String> {
        let s = shader(assets).map_err(|e| e.0)?;
        gpu.create_pipeline(&PipelineDesc {
            label: "hand",
            shader: &s,
            vertex: "vs_hand",
            fragment: Some("fs"),
            color: Some(Format::Surface),
            depth: Some(DepthState { write: true, compare: Compare::GreaterEqual }),
            blend: Blend::None,
            cull_back: true,
            topology: Topology::Triangles,
        })
        .map_err(|e| e.0)
    }

    /// Item models instanced in the world and in the hand.
    fn item_pipelines(gpu: &mut dyn Backend, assets: &Assets) -> Result<(PipelineId, PipelineId), String> {
        let s = shader(assets).map_err(|e| e.0)?;
        let mut pipe = |label, vertex| {
            gpu.create_pipeline(&PipelineDesc {
                label,
                shader: &s,
                vertex,
                fragment: Some("fs"),
                color: Some(Format::Surface),
                depth: Some(DepthState { write: true, compare: Compare::GreaterEqual }),
                blend: Blend::None,
                cull_back: true,
                topology: Topology::Triangles,
            })
            .map_err(|e| e.0)
        };
        Ok((pipe("items", "vs_item")?, pipe("hand item", "vs_hand_item")?))
    }

    fn pipelines(gpu: &mut dyn Backend, assets: &Assets) -> Result<(PipelineId, PipelineId), String> {
        let s = shader(assets).map_err(|e| e.0)?;
        let main = gpu
            .create_pipeline(&PipelineDesc {
                label: "entities",
                shader: &s,
                vertex: "vs",
                fragment: Some("fs"),
                color: Some(Format::Surface),
                depth: Some(DepthState { write: true, compare: Compare::GreaterEqual }),
                blend: Blend::None,
                cull_back: true,
                topology: Topology::Triangles,
            })
            .map_err(|e| e.0)?;
        let overlay = gpu
            .create_pipeline(&PipelineDesc {
                label: "overlay",
                shader: &s,
                vertex: "vs",
                fragment: Some("fs_overlay"),
                color: Some(Format::Surface),
                depth: Some(DepthState { write: false, compare: Compare::GreaterEqual }),
                blend: Blend::Alpha,
                cull_back: true,
                topology: Topology::Triangles,
            })
            .map_err(|e| e.0)?;
        Ok((main, overlay))
    }

    /// The model pipelines (screen, inventory figure image) with every entity shader snippet,
    /// and each kind's slot.
    fn model_pipelines(gpu: &mut dyn Backend, assets: &Assets) -> Result<([PipelineId; 2], Vec<u32>), String> {
        let snippets = crate::shaders::snippets(assets, &HOOK);
        let (s, mut slots) = crate::packs::compose(assets, "model.wgsl", &[(&HOOK, &snippets)]).map_err(|e| e.0)?;
        let slots = slots.pop().unwrap_or_default();
        let c = content();
        let comp = c.components.id(loom_abi::comp::SHADER);
        let kinds = (0..c.game.entities.len() as u32)
            .map(|k| {
                let name = comp.and_then(|id| c.components.text(crate::components::ENTITY, k, id));
                let slot = name.map_or(0, |n| slots.slot(n));
                if slot == 0 && name.is_some() {
                    log::warn!("entity {}: no entity shader {}", c.game.entities[k as usize].name, name.unwrap_or(""));
                }
                slot
            })
            .collect();
        let mut make = |label: &str, color: Format| {
            gpu.create_pipeline(&PipelineDesc {
                label,
                shader: &s,
                vertex: "vs",
                fragment: Some("fs"),
                color: Some(color),
                depth: Some(DepthState { write: true, compare: Compare::GreaterEqual }),
                blend: Blend::None,
                cull_back: true,
                topology: Topology::Triangles,
            })
            .map_err(|e| e.0)
        };
        Ok(([make("models", Format::Surface)?, make("entity preview", Format::Rgba8Srgb)?], kinds))
    }

    pub fn reload(&mut self, gpu: &mut dyn Backend, assets: &Assets) {
        match Self::model_pipelines(gpu, assets) {
            Ok(([p, q], kinds)) => {
                gpu.destroy_pipeline(self.model_pipe);
                gpu.destroy_pipeline(self.preview_pipe);
                (self.model_pipe, self.preview_pipe) = (p, q);
                self.kind_shaders = kinds;
            }
            Err(e) => log::error!("model shader: {e}"),
        }
        self.models.reload(gpu, assets);
        // Skins and masks are read again the next time they are used.
        for t in self.skins.drain().filter_map(|(_, v)| v).map(|v| v.0).chain(self.glows.drain().map(|(_, g)| g.0)) {
            gpu.destroy_texture(t);
        }
        if let Ok(h) = Self::hand_pipeline(gpu, assets) {
            gpu.destroy_pipeline(self.hand_pipe);
            self.hand_pipe = h;
        }
        if let Ok((a, b)) = Self::item_pipelines(gpu, assets) {
            gpu.destroy_pipeline(self.item_pipe);
            gpu.destroy_pipeline(self.hand_item_pipe);
            (self.item_pipe, self.hand_item_pipe) = (a, b);
        }
        // Sprites and the models made of them are read again.
        self.sprites.clear();
        self.item_models.clear();
        self.item_boxes.clear();
        match Self::pipelines(gpu, assets) {
            Ok((a, b)) => {
                gpu.destroy_pipeline(self.pipeline);
                gpu.destroy_pipeline(self.overlay);
                self.pipeline = a;
                self.overlay = b;
            }
            Err(e) => log::error!("entity shader: {e}"),
        }
    }

    /// Uploads player `entity`'s own skin.
    pub fn set_skin(&mut self, gpu: &mut dyn Backend, entity: u64, png: &[u8]) {
        let Some(img) = crate::assets::decode_png(png) else { return };
        let t = gpu.create_texture(&TextureDesc { width: img.width, height: img.height, mips: 1, format: Format::Rgba8Srgb, render_target: false }, "player skin");
        gpu.write_texture(t, 0, &img.rgba);
        if let Some(old) = self.player_skins.insert(entity, (t, gpu.texture_index(t), [img.width, img.height])) {
            gpu.destroy_texture(old.0);
        }
    }

    pub fn destroy(&mut self, gpu: &mut dyn Backend) {
        for (t, ..) in self.player_skins.drain().map(|(_, v)| v) {
            gpu.destroy_texture(t);
        }
        for (b, _) in self.buffers.drain(..) {
            gpu.destroy_buffer(b);
        }
        self.models.destroy(gpu);
        gpu.destroy_pipeline(self.model_pipe);
        gpu.destroy_pipeline(self.hand_pipe);
        gpu.destroy_pipeline(self.preview_pipe);
        if let Some((c, d, ..)) = self.preview_target.take() {
            gpu.destroy_texture(c);
            gpu.destroy_texture(d);
        }
        let preview: Vec<BufferId> = self.preview.drain(..).collect();
        for b in self.hand_views.drain(..).chain(preview) {
            gpu.destroy_buffer(b);
        }
        for t in self.skins.drain().filter_map(|(_, v)| v).map(|v| v.0).chain(self.glows.drain().map(|(_, g)| g.0)) {
            gpu.destroy_texture(t);
        }
        gpu.destroy_pipeline(self.pipeline);
        gpu.destroy_pipeline(self.overlay);
        gpu.destroy_pipeline(self.item_pipe);
        gpu.destroy_pipeline(self.hand_item_pipe);
        if let Some((b, ..)) = self.item_buf.take() {
            gpu.destroy_buffer(b);
        }
    }

    /// Loads skins and worn textures the first time something visible uses them; `worn` is what
    /// the local player wears.
    pub fn preload(&mut self, gpu: &mut dyn Backend, assets: &Assets, entities: &FxHashMap<u64, ClientEntity>, worn: &[u32], held: u32) {
        let c = content();
        self.load_sprite(assets, held);
        // Each kind and item looked at once per frame, without listing thousands of entities.
        let mut seen = std::mem::take(&mut self.seen_kinds);
        seen.clear();
        seen.resize(c.game.entities.len(), false);
        let mut seen_items = std::mem::take(&mut self.seen_items);
        seen_items.clear();
        seen_items.resize(c.items.len(), false);
        for e in entities.values() {
            if !e.item.is_empty() && seen_items.get(e.item.item as usize).is_some_and(|s| !s) {
                seen_items[e.item.item as usize] = true;
                self.load_sprite(assets, e.item.item);
            }
            if !std::mem::replace(&mut seen[e.look() as usize], true) {
                self.load(gpu, assets, &c.game.entities[e.look() as usize].texture);
            }
            for &i in &e.worn {
                if let Some((_, t)) = c.worn(i) {
                    self.load(gpu, assets, t);
                }
            }
        }
        self.seen_kinds = seen;
        self.seen_items = seen_items;
        self.load(gpu, assets, &c.game.entities[c.game.player_kind as usize].texture);
        for &i in worn {
            if let Some((_, t)) = c.worn(i) {
                self.load(gpu, assets, t);
            }
        }
    }

    /// The opaque pixels of an item's sprite, read the first time it is held or dropped.
    fn load_sprite(&mut self, assets: &Assets, item: u32) {
        let Some(name) = sprite_name(item) else { return };
        if self.sprites.contains_key(name) {
            return;
        }
        let img = assets.read(name, "textures", "png").and_then(|b| decode_png(&b));
        let sprite = img.map(|img| {
            let size = img.width;
            // Animated strips: the first frame.
            let solid = |x: i64, y: i64| (0..size as i64).contains(&x) && (0..size as i64).contains(&y) && img.rgba.get((y as usize * size as usize + x as usize) * 4 + 3).is_some_and(|&a| a >= 128);
            let pixels = (0..size * size)
                .map(|i| ((i % size) as i64, (i / size) as i64))
                .filter(|&(x, y)| solid(x, y))
                .map(|(x, y)| (x as u8, y as u8, solid(x - 1, y) as u32 | (solid(x + 1, y) as u32) << 1 | (solid(x, y + 1) as u32) << 2 | (solid(x, y - 1) as u32) << 3))
                .collect();
            Sprite { size, pixels }
        });
        self.sprites.insert(name.to_string(), sprite);
    }

    /// An item's model as boxes in the unit cube, made once: a block item's shape, else its
    /// sprite one texel thick (flat when the sprite could not be read). (first box, count).
    fn item_model(&mut self, textures: &GameTextures, item: u32) -> Option<(u32, u32)> {
        if let Some(&m) = self.item_models.get(&item) {
            return Some(m);
        }
        let c = content();
        let def = c.items.get(item as usize).filter(|_| item != 0)?;
        let first = self.item_boxes.len();
        let white = Vec4::new(1.0, 1.0, 1.0, 0.0);
        let block = def.block.map(|st| &c.states[st as usize]);
        let looks = super::looks::current();
        let look = looks.item(item);
        let model = look.and_then(|l| l.model.as_deref()).unwrap_or(&def.model);
        let icon = look.and_then(|l| l.icon).unwrap_or(def.icon);
        match block {
            _ if !model.is_empty() => boxes_shape(&mut self.item_boxes, textures, Mat4::IDENTITY, model, &c.states[0], white),
            Some(st) if def.icon == u32::MAX && st.item_sprite().is_none() => block_shape(&mut self.item_boxes, textures, Mat4::IDENTITY, st, white),
            _ => {
                let l = match block {
                    Some(st) if icon == u32::MAX => textures.texture(st.item_sprite().unwrap_or(st.textures[0])),
                    _ => textures.texture(icon),
                };
                match sprite_name(item).and_then(|n| self.sprites.get(n)).and_then(|s| s.as_ref()) {
                    Some(sp) => {
                        let n = sp.size as f32;
                        // Only the outline: a side face against a neighboring pixel would fight
                        // with it. Mode 4: every face takes the color of this one texel.
                        for &(x, y, hidden) in &sp.pixels {
                            let m = Mat4::from_translation(Vec3::new(x as f32 / n, 1.0 - (y as f32 + 1.0) / n, 7.5 / 16.0)) * Mat4::from_scale(Vec3::new(1.0 / n, 1.0 / n, 1.0 / 16.0));
                            self.item_boxes.push(GpuBox { rows: rows(m), uvo: [(x as f32 + 0.5) / n, (y as f32 + 0.5) / n, 0.0, 0.0], color: white.to_array(), ids: [0, box_mode::TEXEL, l, hidden], ..Default::default() });
                        }
                    }
                    None => {
                        let m = Mat4::from_translation(Vec3::new(0.0, 0.0, 0.5)) * Mat4::from_scale(Vec3::new(1.0, 1.0, 0.0));
                        self.item_boxes.push(GpuBox { rows: rows(m), color: white.to_array(), ids: [0, box_mode::FACES, l | l << 16, l | l << 16], ids2: [l | l << 16, 0, 0, 0], ..Default::default() });
                    }
                }
            }
        }
        let m = (first as u32, (self.item_boxes.len() - first) as u32);
        self.item_models.insert(item, m);
        Some(m)
    }

    /// Item models made since the last upload go to the GPU; a new texture generation remakes
    /// them all.
    fn upload_items(&mut self, gpu: &mut dyn Backend) {
        let n = self.item_boxes.len();
        let (buf, cap, done) = match self.item_buf {
            Some((b, cap, done)) if (n as u32) <= cap => (b, cap, done.min(n)),
            old => {
                if let Some((b, ..)) = old {
                    gpu.destroy_buffer(b);
                }
                let cap = (n as u32).max(1024).next_power_of_two();
                (gpu.create_buffer(cap as u64 * 128, usage::STORAGE, "item models"), cap, 0)
            }
        };
        if done < n {
            gpu.write_buffer(buf, done as u64 * 128, bytemuck::cast_slice(&self.item_boxes[done..]));
        }
        self.item_buf = Some((buf, cap, n));
    }

    fn load(&mut self, gpu: &mut dyn Backend, assets: &Assets, name: &str) {
        if name.is_empty() || self.skins.contains_key(name) {
            return;
        }
        let loaded = assets.read(name, "textures", "png").and_then(|b| decode_png(&b)).map(|img| {
            let t = gpu.create_texture(
                &TextureDesc { width: img.width, height: img.height, mips: 1, format: Format::Rgba8Srgb, render_target: false },
                name,
            );
            gpu.write_texture(t, 0, &img.rgba);
            (t, gpu.texture_index(t), [img.width, img.height])
        });
        if loaded.is_none() {
            log::warn!("missing entity texture {name}");
        }
        if let Some(mask) = crate::assets::emission_mask(assets, name) {
            let t = gpu.create_texture(&TextureDesc { width: mask.width, height: mask.height, mips: 1, format: Format::R8Unorm, render_target: false }, name);
            gpu.write_texture(t, 0, &mask.rgba);
            self.glows.insert(name.to_string(), (t, gpu.texture_index(t)));
        }
        self.skins.insert(name.to_string(), loaded);
    }

    /// Model instances of `entity`, of `kind`: its skin (a player's own if they sent one) and what
    /// it wears (how many in `.1`).
    fn layers(&self, kind: u32, entity: u64, worn: &[u32], light: u32, hurt: f32) -> ([Instance; MAX_LAYERS], usize) {
        let c = content();
        let k = &c.game.entities[kind as usize];
        let mut out = ([Instance::default(); MAX_LAYERS], 0);
        let Some(Some((_, skin, _))) = self.skins.get(&k.texture) else { return out };
        let glow = |name: &str| self.glows.get(name).map_or(u32::MAX, |g| g.1);
        let size = self.models.kinds.get(kind as usize).and_then(|m| m.texture).unwrap_or(k.texture_size);
        let own = self.player_skins.get(&entity).filter(|_| kind == c.game.player_kind);
        let (skin, size) = own.map_or((skin, size), |o| (&o.1, o.2));
        let base = Instance {
            color: [1.0; 3],
            light,
            hurt,
            tex: *skin,
            tex_size: [size[0] as f32, size[1] as f32],
            roles: u32::MAX,
            shader: self.kind_shaders.get(kind as usize).copied().unwrap_or(0),
            glow: glow(&k.texture),
            ..Default::default()
        };
        out.0[0] = base;
        out.1 = 1;
        for &i in worn {
            if let Some((w, t)) = c.worn(i) {
                if let Some(Some((_, idx, size))) = self.skins.get(t).filter(|_| out.1 < MAX_LAYERS) {
                    out.0[out.1] = Instance { tex: *idx, tex_size: [size[0] as f32, size[1] as f32], roles: crate::model::role_mask(w.roles), inflate: w.inflate, glow: glow(t), ..base };
                    out.1 += 1;
                }
            }
        }
        out
    }

    /// Renders the local player's model into a `size` texture (physical pixels), turned towards
    /// `look` (relative to the texture's top left) like the inventory figure. Returns the
    /// texture's bindless index for the UI to draw.
    pub fn draw_preview(&mut self, gpu: &mut dyn Backend, list: &mut CommandList, frame: &FrameInfo, size: [u32; 2], look: glam::Vec2, worn: &[u32]) -> Option<u32> {
        let c = content();
        if size[0] == 0 || size[1] == 0 {
            return None;
        }
        if self.preview_target.is_none_or(|t| t.2 != size) {
            if let Some((col, dep, ..)) = self.preview_target.take() {
                gpu.destroy_texture(col);
                gpu.destroy_texture(dep);
            }
            let desc = |format| TextureDesc { width: size[0], height: size[1], mips: 1, format, render_target: true };
            let col = gpu.create_texture(&desc(Format::Rgba8Srgb), "preview color");
            let dep = gpu.create_texture(&desc(Format::Depth32), "preview depth");
            self.preview_target = Some((col, dep, size, gpu.texture_index(col)));
        }
        let (col, dep, _, index) = self.preview_target?;
        let (x, y, w, h) = (0.0, 0.0, size[0] as f32, size[1] as f32);
        let (sw, sh) = (w, h);
        let s = h / 2.4;
        let (cx, feet) = (x + w / 2.0, y + h - h * 0.08);
        let head = feet - 1.5 * s;
        let yaw = std::f32::consts::PI + ((look.x - cx) / (s * 1.6)).atan() * 0.7;
        let pitch = ((head - look.y) / (s * 1.6)).atan() * 0.5;
        let proj = Mat4::from_cols(
            Vec4::new(2.0 * s / sw, 0.0, 0.0, 0.0),
            Vec4::new(0.0, 2.0 * s / sh, 0.0, 0.0),
            Vec4::new(0.0, 0.0, 0.1, 0.0),
            Vec4::new(2.0 * cx / sw - 1.0, 1.0 - 2.0 * feet / sh, 0.5, 1.0),
        );
        let layers = self.layers(c.game.player_kind, self.local, worn, FULL_LIGHT, 0.0);
        self.models.begin();
        let root = Mat4::from_rotation_y(yaw) * Mat4::from_scale(Vec3::splat(1.0 / 16.0));
        self.models.add(c.game.player_kind, root, &crate::model::Motion { pitch, swing: 0.0, anims: &[], crouch: false }, layers.0[..layers.1].iter().copied());
        let vb = self.preview[frame.slot];
        // Full daylight, no fog.
        let v = Scene { view_proj: proj.to_cols_array_2d(), params: [1e9, 2e9, 1.0, crate::shaders::time()], ..Default::default() };
        gpu.mapped(vb)[..size_of::<Scene>()].copy_from_slice(bytemuck::bytes_of(&v));
        list.push(Cmd::BeginPass { color: Some((Target::Texture(col), Some([0.0; 4]))), depth: Some((dep, Some(0.0))) });
        self.models.draw(gpu, list, frame, super::models::Batch::Preview, self.preview_pipe, vb);
        list.push(Cmd::EndPass);
        Some(index)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        gpu: &mut dyn Backend,
        list: &mut CommandList,
        frame: &FrameInfo,
        cam: &Camera,
        entities: &FxHashMap<u64, ClientEntity>,
        me: Option<&ClientEntity>,
        hand: Option<Hand>,
        textures: &GameTextures,
        light: &impl Fn(IVec3) -> u32,
        scene: &Scene,
        scene_buf: BufferId,
        target: Option<(Hit, DVec3)>,
        breaking: Option<(IVec3, f32, DVec3)>,
        gizmo_lines: &[super::gizmo::Line],
        gizmo_fills: &[super::gizmo::Fill],
        late: &mut CommandList,
    ) {
        let c = content();
        let aspect = frame.width as f32 / frame.height.max(1) as f32;
        let mut boxes: Vec<GpuBox> = Vec::new();
        if textures.generation() != self.item_generation {
            self.item_generation = textures.generation();
            self.item_models.clear();
            self.item_boxes.clear();
            if let Some(b) = &mut self.item_buf {
                b.2 = 0;
            }
        }
        let mut instances = std::mem::take(&mut self.instances);
        instances.clear();
        let rel = |p: DVec3| (p - cam.pos).as_vec3();
        let shade = |p: DVec3| light(p.floor().as_ivec3());

        // Models: bones posed here (in parallel when many), drawn instanced per kind.
        let mut posed = std::mem::take(&mut self.posed);
        posed.clear();
        for (id, e) in entities.iter().map(|(i, e)| (*i, e)).chain(me.map(|m| (self.local, m))) {
            let base = rel(e.pos);
            if base.length_squared() > 160.0 * 160.0 {
                continue;
            }
            let lit = shade(e.pos + DVec3::Y * 0.5);
            let hurt = (e.hurt / 0.4).clamp(0.0, 1.0);
            if !e.item.is_empty() {
                if let Some(m) = self.item_model(textures, e.item.item) {
                    instances.push((e.item.item, item_instance(ground_transform(base, e.age, e.item.item), m.0, hurt, lit)));
                }
                continue;
            }
            let kind = e.look();
            let (layers, layer_count) = self.layers(kind, id, &e.worn, lit, hurt);
            if layer_count == 0 {
                continue;
            }
            if e.burning {
                let (hw, h, _) = e.dims();
                let from = boxes.len();
                fire_boxes(&mut boxes, textures, base, hw as f32 * 2.0, h as f32, cam.yaw, e.age);
                light_boxes(&mut boxes, from, FULL_LIGHT);
            }
            // Vanilla's limb swing: 0.6662 rad per tick of limbSwing, which grows 4 per block;
            // models with their own walk animation use that instead.
            let mut swing = (e.walk * 2.66).sin() * 1.2 * e.swing;
            let (mut anims, mut anim_count) = ([(0, 0.0, 0.0); 2], 0);
            let mut add = |a| {
                anims[anim_count] = a;
                anim_count += 1;
            };
            if let Some(m) = self.models.kinds.get(kind as usize) {
                match m.playing(e.anim.as_ref(), e.age) {
                    Some((i, t)) => {
                        add((i, t, 1.0));
                        swing = 0.0;
                    }
                    _ => {
                        if let Some(i) = m.idle {
                            add((i, m.anims[i].at(e.age, true), 1.0));
                        }
                        if let Some(i) = m.walk {
                            add((i, m.anims[i].at(e.walk * 0.6, true), e.swing.min(1.0)));
                            swing = 0.0;
                        }
                    }
                }
            }
            // Vanilla: sqrt of the death time over a second, up to a quarter turn.
            let roll = e.dying.map_or(0.0, |d| (d * 1.6).sqrt().min(1.0) * std::f32::consts::FRAC_PI_2);
            let root = Mat4::from_translation(base) * Mat4::from_rotation_y(e.yaw) * Mat4::from_rotation_z(roll) * Mat4::from_scale(Vec3::splat(1.0 / 16.0));
            posed.push(Posed { kind, root, pitch: e.pitch, swing, crouch: e.crouched, anims, anim_count, layers, layer_count });
        }
        let vb = scene_buf;
        self.models.begin();
        self.models.add_many(&posed);
        self.posed = posed;
        self.models.draw(gpu, list, frame, super::models::Batch::World, self.model_pipe, vb);

        // Selection outline: twelve thin dark boxes along the edges of each selection box.
        let outlined = target.map(|(hit, at)| (&c.states[hit.state as usize].selection, rel(hit.block.as_dvec3() + at)));
        for (sel, o) in outlined.iter().flat_map(|(sel, o)| sel.iter().map(move |s| (s, *o))) {
            let (mn, mx) = (Vec3::new(sel[0], sel[1], sel[2]), Vec3::new(sel[3], sel[4], sel[5]));
            let t = 0.006;
            let (a, b) = (o + mn - Vec3::splat(t), o + mx + Vec3::splat(t));
            for axis in 0..3 {
                for corner in 0..4 {
                    let mut lo = a;
                    let mut size = Vec3::splat(t * 2.0);
                    size[axis] = b[axis] - a[axis];
                    let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
                    lo[u] = if corner & 1 == 0 { a[u] } else { b[u] - t * 2.0 };
                    lo[v] = if corner & 2 == 0 { a[v] } else { b[v] - t * 2.0 };
                    let m = Mat4::from_translation(lo) * Mat4::from_scale(size);
                    boxes.push(GpuBox { rows: rows(m), color: [0.02, 0.02, 0.02, 0.0], ids: [0, box_mode::SOLID, 0, 0], ..Default::default() });
                }
            }
        }
        // Gizmo lines: thin boxes like the outline, a little thicker so they read from afar.
        for &(a, b, color) in gizmo_lines {
            let (a, b) = (rel(a), rel(b));
            let d = b - a;
            let len = d.length();
            if len < 1e-4 || a.length_squared().min(b.length_squared()) > 160.0 * 160.0 {
                continue;
            }
            let t = 0.012 + 0.0012 * a.length().min(b.length());
            let rot = Quat::from_rotation_arc(Vec3::X, d / len);
            let m = Mat4::from_rotation_translation(rot, a) * Mat4::from_translation(Vec3::new(0.0, -t / 2.0, -t / 2.0)) * Mat4::from_scale(Vec3::new(len, t, t));
            let rgb = |s: u32| ((color >> s) & 255) as f32 / 255.0;
            boxes.push(GpuBox { rows: rows(m), color: [rgb(24), rgb(16), rgb(8), 0.0], ids: [0, box_mode::SOLID, 0, 0], ..Default::default() });
        }

        let overlay_start = boxes.len();
        // Filled gizmo boxes: see-through by their own alpha (mode 5), slightly grown so they do
        // not fight with the faces of the blocks they cover.
        for &(a, b, color) in gizmo_fills {
            let (a, b) = (rel(a) - Vec3::splat(0.002), rel(b) + Vec3::splat(0.002));
            if a.length_squared().min(b.length_squared()) > 160.0 * 160.0 {
                continue;
            }
            let m = Mat4::from_translation(a) * Mat4::from_scale(b - a);
            let ch = |s: u32| ((color >> s) & 255) as f32 / 255.0;
            boxes.push(GpuBox { rows: rows(m), color: [ch(24), ch(16), ch(8), 0.0], ids: [0, box_mode::FILLED, ch(0).to_bits(), 0], ..Default::default() });
        }
        if let Some((p, progress, at)) = breaking {
            if progress > 0.0 {
                // Over the faces of the block's model (a ladder's board, not its whole cube),
                // textured like the block's own faces there.
                let stage = ((progress * 10.0) as usize).min(9);
                let l = textures.texture(c.crack_textures[stage]);
                let lit = shade(p.as_dvec3() + at + DVec3::splat(0.5));
                let state = target.filter(|h| h.0.block == p).map(|h| &c.states[h.0.state as usize]);
                let shape: Vec<(Vec3, Vec3)> = match state {
                    Some(s) => s.selection.iter().map(|b| (Vec3::new(b[0], b[1], b[2]), Vec3::new(b[3], b[4], b[5]))).collect(),
                    None => vec![(Vec3::ZERO, Vec3::ONE)],
                };
                for (mn, mx) in shape {
                    let (a, b) = (mn - Vec3::splat(0.004), mx + Vec3::splat(0.004));
                    let m = Mat4::from_translation(rel(p.as_dvec3() + at) + a) * Mat4::from_scale(b - a);
                    boxes.push(GpuBox {
                        rows: rows(m),
                        dims: mn.extend(1.0).to_array(),
                        uvo: mx.extend(0.0).to_array(),
                        color: [1.0, 1.0, 1.0, 0.0],
                        ids: [0, box_mode::FACES, l | l << 16, l | l << 16],
                        ids2: [l | l << 16, lit, 0, 0],
                    });
                }
            }
        }
        let hand_start = boxes.len();
        let mut hand_item: Option<(GpuBox, u32)> = None;
        if let Some(h) = hand {
            let k = &c.game.entities[c.game.player_kind as usize];
            let player_skin = match (self.player_skins.get(&self.local), self.skins.get(&k.texture)) {
                (Some((_, idx, size)), _) => Some((*idx, [size[0] as f32, size[1] as f32])),
                (None, Some(Some((_, idx, _)))) => Some((*idx, [k.texture_size[0] as f32, k.texture_size[1] as f32])),
                _ => None,
            };
            // Keyframes may move the hand too: the `hand` bone of what plays (else of idle).
            let pose = self.models.kinds.get(c.game.player_kind as usize).and_then(|m| {
                let (i, t) = m.playing(h.anim.as_ref(), h.age).or_else(|| m.idle.map(|i| (i, m.anims[i].at(h.age, true))))?;
                m.anims[i].anim.free_bone("hand", t)
            });
            let from = boxes.len();
            match hand_transform(cam, &h, pose.unwrap_or(Mat4::IDENTITY)) {
                Some(m) => hand_item = self.item_model(textures, h.stack.item).map(|(first, n)| (item_instance(m, first, 0.0, shade(cam.pos)), n)),
                None => {
                    // The arm is the player model's own right arm bone.
                    let parts = self.models.kinds.get(c.game.player_kind as usize).map_or(&k.parts, |m| &m.parts);
                    let arm = parts.iter().find(|p| p.name.eq_ignore_ascii_case("rightArm"));
                    hand_boxes(&mut boxes, cam, &h, player_skin, arm, pose.unwrap_or(Mat4::IDENTITY));
                    light_boxes(&mut boxes, from, shade(cam.pos));
                }
            }
        }
        // Item instances after the hand, grouped by item: one instanced draw each.
        let hand_end = boxes.len();
        instances.sort_unstable_by_key(|i| i.0);
        boxes.extend(instances.iter().map(|i| i.1));
        if let Some((inst, _)) = hand_item {
            boxes.push(inst);
        }
        if boxes.is_empty() {
            self.instances = instances;
            return;
        }
        if !instances.is_empty() || hand_item.is_some() {
            self.upload_items(gpu);
        }

        let buf = crate::gpu_layout::frame_buffer(gpu, &mut self.buffers, frame.slot, boxes.len() as u32, 128, usage::STORAGE, "entity boxes");
        gpu.mapped(buf)[..boxes.len() * 128].copy_from_slice(bytemuck::cast_slice(&boxes));
        let glow = textures.glow_buffer();

        if overlay_start > 0 {
            list.pipeline(self.pipeline);
            list.bind(crate::gpu_layout::bind::entity::BOXES, buf);
            list.bind(crate::gpu_layout::bind::entity::SCENE, vb);
            list.bind(crate::gpu_layout::bind::entity::GLOW_MAP, glow);
            list.draw(overlay_start as u32 * 36, 1);
        }
        let item_buf = self.item_buf.map(|b| b.0);
        if let (Some(ib), false) = (item_buf, instances.is_empty()) {
            list.pipeline(self.item_pipe);
            list.bind(crate::gpu_layout::bind::entity::BOXES, buf);
            list.bind(crate::gpu_layout::bind::entity::SCENE, vb);
            list.bind(crate::gpu_layout::bind::entity::GLOW_MAP, glow);
            list.bind(crate::gpu_layout::bind::entity::ITEM_BOXES, ib);
            let mut i = 0;
            while i < instances.len() {
                let item = instances[i].0;
                let run = instances[i..].iter().take_while(|x| x.0 == item).count();
                let boxes_per = self.item_models.get(&item).map_or(0, |m| m.1);
                list.push(Cmd::Draw { vertices: boxes_per * 36, instances: run as u32, first_vertex: 0, first_instance: (hand_end + i) as u32 });
                i += run;
            }
        }
        // Cracks, filled gizmos and the hand come after translucent blocks.
        let list = late;
        if hand_start > overlay_start {
            list.pipeline(self.overlay);
            list.bind(crate::gpu_layout::bind::entity::BOXES, buf);
            list.bind(crate::gpu_layout::bind::entity::SCENE, vb);
            list.bind(crate::gpu_layout::bind::entity::GLOW_MAP, glow);
            list.push(Cmd::Draw { vertices: (hand_start - overlay_start) as u32 * 36, instances: 1, first_vertex: overlay_start as u32 * 36, first_instance: 0 });
        }
        if hand_end > hand_start || hand_item.is_some() {
            let hand_cam = Camera { fov: 70.0, ..*cam };
            let hv = Scene { view_proj: (hand_cam.proj(aspect) * cam.view()).to_cols_array_2d(), ..*scene };
            let hb = self.hand_views[frame.slot];
            gpu.mapped(hb)[..size_of::<Scene>()].copy_from_slice(bytemuck::bytes_of(&hv));
            if hand_end > hand_start {
                list.pipeline(self.hand_pipe);
                list.bind(crate::gpu_layout::bind::entity::BOXES, buf);
                list.bind(crate::gpu_layout::bind::entity::SCENE, hb);
                list.bind(crate::gpu_layout::bind::entity::GLOW_MAP, glow);
                list.push(Cmd::Draw { vertices: (hand_end - hand_start) as u32 * 36, instances: 1, first_vertex: hand_start as u32 * 36, first_instance: 0 });
            }
            if let (Some((_, n)), Some(ib)) = (hand_item, item_buf) {
                list.pipeline(self.hand_item_pipe);
                list.bind(crate::gpu_layout::bind::entity::BOXES, buf);
                list.bind(crate::gpu_layout::bind::entity::SCENE, hb);
                list.bind(crate::gpu_layout::bind::entity::GLOW_MAP, glow);
                list.bind(crate::gpu_layout::bind::entity::ITEM_BOXES, ib);
                list.push(Cmd::Draw { vertices: n * 36, instances: 1, first_vertex: 0, first_instance: boxes.len() as u32 - 1 });
            }
        }
        self.instances = instances;
    }
}

/// Two layers of flames facing the camera, the inner one smaller, like vanilla's burning mobs.
fn fire_boxes(out: &mut Vec<GpuBox>, textures: &GameTextures, base: Vec3, width: f32, height: f32, cam_yaw: f32, age: f32) {
    let c = content();
    let frame = |offset: f32| textures.frame_of(c.fire_texture, (age * crate::world::TPS as f32 + offset) as u32);
    for (i, (scale, back)) in [(1.4, 0.0), (1.0, 0.2)].into_iter().enumerate() {
        let l = frame(i as f32 * 11.0);
        let w = width * scale;
        let m = Mat4::from_translation(base)
            * Mat4::from_rotation_y(cam_yaw)
            * Mat4::from_translation(Vec3::new(-w / 2.0, 0.0, back * width))
            * Mat4::from_scale(Vec3::new(w, height * scale * 0.95, 0.0));
        out.push(GpuBox { rows: rows(m), color: [1.0, 1.0, 1.0, 0.0], ids: [0, box_mode::FLAMES, l | l << 16, l | l << 16], ids2: [l | l << 16, 0, 0, 0], ..Default::default() });
    }
}

/// Texture a held item is drawn from as a solid sprite: its icon, or the texture of a plant block.
fn sprite_name(item: u32) -> Option<&'static str> {
    let c = content();
    let def = c.items.get(item as usize).filter(|_| item != 0)?;
    if def.icon != u32::MAX {
        return c.textures.get(def.icon as usize).map(String::as_str);
    }
    let st = &c.states[def.block? as usize];
    st.item_sprite().and_then(|t| c.textures.get(t as usize)).map(String::as_str)
}

/// Where the held item's model goes in front of the camera, posed like vanilla's
/// ItemInHandRenderer: swing translation, arm offset, attack rotation, then the item model's
/// first person display transform. `None` for the bare arm.
fn hand_transform(cam: &Camera, h: &Hand, pose: Mat4) -> Option<Mat4> {
    use std::f32::consts::PI;
    let c = content();
    c.items.get(h.stack.item as usize).filter(|_| !h.stack.is_empty())?;
    let p = h.swing.unwrap_or(0.0);
    let (s, sr) = ((p * PI).sin(), (p.sqrt() * PI).sin());
    let view = cam.view().inverse() * pose;
    let deg = |d: f32| d.to_radians();
    let swing_sin = (p * p * PI).sin();
    let arm = view
        * Mat4::from_translation(Vec3::new(-0.4 * sr, 0.2 * (p.sqrt() * PI * 2.0).sin(), -0.2 * s))
        * Mat4::from_translation(Vec3::new(0.56, -0.52, -0.72))
        * Mat4::from_rotation_y(deg(45.0 - swing_sin * 20.0))
        * Mat4::from_rotation_z(deg(-sr * 20.0))
        * Mat4::from_rotation_x(deg(-sr * 80.0))
        * Mat4::from_rotation_y(deg(-45.0));
    Some(if shows_block(h.stack.item) {
        // block.json: rotated 45 degrees, scale 0.4.
        arm * Mat4::from_rotation_y(deg(45.0)) * Mat4::from_scale(Vec3::splat(0.4)) * Mat4::from_translation(Vec3::splat(-0.5))
    } else {
        // generated.json / handheld.json first person: rotation 0 -90 25, translation 1.13 3.2
        // 1.13 (pixels), scale 0.68.
        arm * Mat4::from_translation(Vec3::new(1.13, 3.2, 1.13) / 16.0) * Mat4::from_rotation_y(deg(-90.0)) * Mat4::from_rotation_z(deg(25.0)) * Mat4::from_scale(Vec3::splat(0.68)) * Mat4::from_translation(Vec3::splat(-0.5))
    })
}

/// The bare right arm (the model's `rightArm` bone) in front of the camera (vanilla's
/// ItemInHandRenderer).
fn hand_boxes(out: &mut Vec<GpuBox>, cam: &Camera, h: &Hand, skin: Option<(u32, [f32; 2])>, arm: Option<&crate::registry::Part>, pose: Mat4) {
    use std::f32::consts::PI;
    let (Some((tex, size)), Some(arm)) = (skin, arm) else { return };
    let p = h.swing.unwrap_or(0.0);
    let (s, sr) = ((p * PI).sin(), (p.sqrt() * PI).sin());
    // From view space to the camera-relative world: the hand stays put on screen while the
    // bobbing view moves.
    let view = cam.view().inverse() * pose;
    let swing = Mat4::from_translation(Vec3::new(-0.4 * sr, 0.2 * (p.sqrt() * PI * 2.0).sin(), -0.2 * s));
    // Raised and turned in from the right; its boxes are around the shoulder pivot.
    let shoulder = view
        * swing
        * Mat4::from_translation(Vec3::new(0.56, -0.78, -0.36))
        * Mat4::from_rotation_y(0.32 + sr * 0.3)
        * Mat4::from_rotation_x(1.85 + s * 0.5)
        * Mat4::from_rotation_z(0.12)
        * Mat4::from_scale(Vec3::splat(1.0 / 16.0));
    for c in &arm.cuboids {
        let m = shoulder * Mat4::from_translation(Vec3::from_array(c.origin)) * Mat4::from_scale(Vec3::from_array(c.size));
        out.push(GpuBox { rows: rows(m), dims: [c.size[0], c.size[1], c.size[2], 0.0], uvo: [c.uv[0], c.uv[1], size[0], size[1]], color: [1.0, 1.0, 1.0, 0.0], ids: [tex, box_mode::SKIN, 0, 0], ids2: [0; 4] });
    }
}

/// A block item drawn as its model's boxes; `m` maps the block's unit cube.
fn block_shape(out: &mut Vec<GpuBox>, textures: &GameTextures, m: Mat4, s: &crate::content::StateInfo, color: Vec4) {
    boxes_shape(out, textures, m, &s.item_shape(), s, color);
}

/// Model boxes (a block's, or an item's from its file) in the unit cube `m` maps, tinted like
/// `s` (an item's own boxes pass air: no tint).
fn boxes_shape(out: &mut Vec<GpuBox>, textures: &GameTextures, m: Mat4, boxes: &[crate::content::ModelBox], s: &crate::content::StateInfo, color: Vec4) {
    for b in boxes {
        let (mn, mx) = (Vec3::from_array(b.min), Vec3::from_array(b.max));
        let mb = m * Mat4::from_translation(mn) * Mat4::from_scale(mx - mn);
        let l = b.textures.map(|t| textures.texture(t));
        // Faces the block tints take the tint here, not from a tinted copy drawn on top (the
        // untinted faces would show through cutout holes and at the edges).
        let overlaid = |d: usize| textures.overlays.get(b.textures[d] as usize).is_some_and(|&o| o != 0);
        let tinted = if s.tint == 0 { 0 } else { (0..6).filter(|&d| s.tint_faces & (1 << d) != 0 && !overlaid(d)).fold(0, |m, d| m | 1 << d) };
        out.push(GpuBox {
            rows: rows(mb),
            dims: mn.extend(1.0).to_array(),
            uvo: mx.extend(0.0).to_array(),
            color: color.to_array(),
            ids: [0, box_mode::FACES, l[0] | l[1] << 16, l[2] | l[3] << 16],
            ids2: [l[4] | l[5] << 16, 0, tinted, crate::content::item_tint(s.tint) & 0xff_ffff],
        });
        tinted_faces(out, textures, mb, s, &b.textures, color);
    }
}

fn rows(m: Mat4) -> [[f32; 4]; 3] {
    [m.row(0).to_array(), m.row(1).to_array(), m.row(2).to_array()]
}

/// The tinted overlays of a block item's textures (grass sides) drawn on top. `m` maps the unit
/// cube.
fn tinted_faces(out: &mut Vec<GpuBox>, textures: &GameTextures, m: Mat4, s: &crate::content::StateInfo, faces: &[u32; 6], color: Vec4) {
    if s.tint == 0 {
        return;
    }
    let tint = crate::content::item_tint(s.tint);
    let rgb = Vec3::new(((tint >> 16) & 255) as f32, ((tint >> 8) & 255) as f32, (tint & 255) as f32) / 255.0;
    for d in 0..6 {
        let Some(tex) = textures.overlays.get(faces[d] as usize).filter(|&&o| o != 0).map(|o| o - 1) else { continue };
        let l = textures.texture(tex);
        // A zero-thick box just outside that face.
        let axis = d / 2;
        let mut offset = Vec3::ZERO;
        let mut scale = Vec3::ONE;
        offset[axis] = if d % 2 == 1 { 1.002 } else { -0.002 };
        scale[axis] = 0.0;
        let f = m * Mat4::from_translation(offset) * Mat4::from_scale(scale);
        out.push(GpuBox {
            rows: rows(f),
            color: (color.truncate() * rgb).extend(color.w).to_array(),
            ids: [0, box_mode::FACES, l | l << 16, l | l << 16],
            ids2: [l | l << 16, 0, 0, 0],
            ..Default::default()
        });
    }
}

/// An item model instance: `m` places the model's unit cube.
fn item_instance(m: Mat4, first: u32, hurt: f32, light: u32) -> GpuBox {
    GpuBox { rows: rows(m), color: [1.0, 1.0, 1.0, hurt], ids: [0, 0, first, 0], ids2: [0, light, 0, 0], ..Default::default() }
}

/// Whether the item shows as its block's shape (else as a sprite).
fn shows_block(item: u32) -> bool {
    let c = content();
    let reshaped = super::looks::current().item(item).is_some_and(|l| l.model.is_some());
    c.items.get(item as usize).is_some_and(|d| reshaped || !d.model.is_empty() || (d.icon == u32::MAX && d.block.is_some_and(|st| c.states[st as usize].item_sprite().is_none())))
}

/// Dropped items spin and bob: a block a quarter of a block across, a sprite half a block
/// (vanilla's ground transforms).
fn ground_transform(base: Vec3, age: f32, item: u32) -> Mat4 {
    let bob = (age * 2.5).sin() * 0.06 + 0.15;
    let spin = Mat4::from_translation(base + Vec3::Y * bob) * Mat4::from_rotation_y(age * 1.5);
    if shows_block(item) {
        spin * Mat4::from_translation(Vec3::splat(-0.125)) * Mat4::from_scale(Vec3::splat(0.25))
    } else {
        spin * Mat4::from_translation(Vec3::new(-0.25, 0.0, -0.25)) * Mat4::from_scale(Vec3::splat(0.5))
    }
}
