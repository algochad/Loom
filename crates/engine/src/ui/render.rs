//! Turns a laid-out `Ui` into quads. Rebuilt only when the tree is dirty; the GPU copy is reused
//! across frames otherwise.

use crate::gpu_layout::ui_mode;
use rustc_hash::FxHashMap;
use loom_render::*;

use std::sync::Arc;

use super::font::Font;
use crate::model::{Animation, Prop};
use super::text::Placed;
use super::look::Look;
use super::{Kind, Rect, Style, Ui};
use crate::assets::{Assets, decode_png};
use crate::shaders::{Hook, Slots};
use crate::client::textures::GameTextures;
use crate::content::Content;
pub use crate::gpu_layout::Quad;

/// Quads of a node and its children moved by keyframes every frame: `animation = ns:name` in the
/// `.gui` of a class's texture names `animations/<name>.animation.json`, whose `open` animation
/// plays once when the screen opens and `idle` loops after. Any bone: position in GUI pixels (y up),
/// rotation z in degrees clockwise and scale, about the node's center.
#[derive(Clone)]
pub struct Animated {
    pub quads: std::ops::Range<usize>,
    open: Option<Arc<Animation>>,
    idle: Option<Arc<Animation>>,
    since: f32,
    center: [f32; 2],
    scale: f32,
}

/// Moves the animated ranges of `quads` (all quads of the frame) to this moment.
pub fn animate(quads: &mut [Quad], list: &[Animated]) {
    let now = crate::shaders::clock();
    for a in list {
        // Automated runs skip opening so screenshots do not depend on timing.
        let (anim, t) = match (&a.open, &a.idle) {
            (Some(o), _) if !crate::shaders::pinned() && now - a.since < o.length => (o, now - a.since),
            (_, Some(i)) => (i, crate::shaders::time().rem_euclid(i.length)),
            _ => continue,
        };
        let get = |prop: Prop, default: [f32; 3]| anim.channels.iter().find(|c| c.prop == prop).map_or(default, |c| c.sample(t));
        let (pos, rot, sc) = (get(Prop::Position, [0.0; 3]), get(Prop::Rotation, [0.0; 3]), get(Prop::Scale, [1.0; 3]));
        let (s, c) = rot[2].to_radians().sin_cos();
        let turn = |p: [f32; 2]| {
            let (x, y) = (p[0] * sc[0], p[1] * sc[1]);
            [x * c - y * s, x * s + y * c]
        };
        let to = [a.center[0] + pos[0] * a.scale, a.center[1] - pos[1] * a.scale];
        for q in quads.get_mut(a.quads.clone()).into_iter().flatten() {
            let r = turn([q.p0[0] - a.center[0], q.p0[1] - a.center[1]]);
            (q.p0, q.u, q.v) = ([to[0] + r[0], to[1] + r[1]], turn(q.u), turn(q.v));
        }
    }
}

/// How a GUI texture fills a rectangle of another size. Set by the texture itself: a `.gui` file
/// next to it (`scaling = nine_slice | tile | stretch`, `border = 3` or `border = l t r b`,
/// `size = w h` when the image is a higher resolution than the GUI pixels it stands for). A bare
/// image is stretched.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slicing {
    /// Corners keep their size, edges and center stretch; borders left, top, right, bottom in GUI
    /// pixels of an image `size` GUI pixels big.
    Nine { size: [f32; 2], border: [f32; 4] },
    /// `turn = random`: each tile of a square texture turned and mirrored its own way, so the
    /// repeat does not show.
    Tile { size: [f32; 2], turn: bool },
    Stretch,
}

/// GUI texture names starting with this are a skin file's face (`screens::face_texture`).
pub const FACE: &str = "skin_face:";

/// The 8x8 face of a skin (64x64 or 64x32), its hat layer drawn over it.
fn face_of(skin: &crate::assets::Image) -> Option<Vec<u8>> {
    if skin.width < 64 || skin.height < 16 {
        return None;
    }
    let mut out = vec![0u8; 8 * 8 * 4];
    for y in 0..8 {
        for x in 0..8 {
            let at = |sx: u32, sy: u32| {
                let i = ((sy * skin.width + sx) * 4) as usize;
                [skin.rgba[i], skin.rgba[i + 1], skin.rgba[i + 2], skin.rgba[i + 3]]
            };
            let (base, hat) = (at(8 + x, 8 + y), at(40 + x, 8 + y));
            let px = if hat[3] > 0 { hat } else { [base[0], base[1], base[2], 255] };
            out[((y * 8 + x) * 4) as usize..][..4].copy_from_slice(&px);
        }
    }
    Some(out)
}

fn parse_gui(src: &str, pixels: [f32; 2]) -> Slicing {
    let mut kind = "nine_slice";
    let mut border = [0.0f32; 4];
    let mut size = pixels;
    let mut turn = false;
    for (_, k, v) in crate::kv::pairs(src) {
        let nums: Vec<f32> = v.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        match k {
            "scaling" => kind = if v.trim() == "tile" { "tile" } else if v.trim() == "stretch" { "stretch" } else { "nine_slice" },
            "border" if nums.len() == 1 => border = [nums[0]; 4],
            "border" if nums.len() == 4 => border = [nums[0], nums[1], nums[2], nums[3]],
            "size" if nums.len() == 2 => size = [nums[0], nums[1]],
            "turn" => turn = v.trim() == "random",
            _ => {}
        }
    }
    match kind {
        "tile" => Slicing::Tile { size, turn: turn && size[0] == size[1] },
        "stretch" => Slicing::Stretch,
        _ => Slicing::Nine { size, border },
    }
}

pub struct UiRenderer {
    pipeline: PipelineId,
    pub font: Font,
    font_tex: TextureId,
    font_idx: u32,
    textures: FxHashMap<String, Option<(u32, u32, u32)>>,
    /// Animated GUI textures by bindless index: `Quad::layer` for them.
    strips: FxHashMap<u32, u32>,
    /// Slicing a texture declares for itself, and how far it reaches outside the box it is
    /// drawn for (`outset` in its `.gui`: shadows, frames drawn around the content).
    slicing: FxHashMap<String, Option<Slicing>>,
    outset: FxHashMap<String, f32>,
    owned: Vec<TextureId>,
    buffers: Vec<(BufferId, u32)>,
    screen: Vec<BufferId>,
    pub quads: Vec<Quad>,
    /// Clip of each deferred item icon, in the order they were queued.
    icon_clips: Vec<[f32; 4]>,
    /// Stack counts drawn over queued item icons: (icon number, count, text pixel size).
    /// Item nodes of the tree being built: where, what, and their clip.
    stacks: Vec<(Rect, loom_abi::ItemStack, [f32; 4])>,
    /// Text effects and GUI fills the shader has, and the effect slot by `text::fx_id`.
    fx: Slots,
    gui: Slots,
    fx_slots: Vec<u32>,
    /// UI animation files by name, and the nodes the last `build` animated.
    anims: FxHashMap<String, Vec<Arc<Animation>>>,
    pub animated: Vec<Animated>,
}

pub const FX: Hook = Hook {
    kind: "fx",
    entry: "effect",
    dispatch: "apply_fx",
    params: "g: Glyph",
    args: "g",
    ret: "Glyph",
    fallback: "return g;",
};

pub const GUI: Hook = Hook {
    kind: "gui",
    entry: "fill",
    dispatch: "gui_fill",
    params: "p: GuiPixel",
    args: "p",
    ret: "vec4<f32>",
    fallback: "return p.color;",
};

fn load_shader(assets: &Assets) -> Result<(Shader, Slots, Slots), ShaderError> {
    let (fx, gui) = (crate::shaders::snippets(assets, &FX), crate::shaders::snippets(assets, &GUI));
    let (shader, mut slots) = crate::packs::compose(assets, "ui.wgsl", &[(&FX, &fx), (&GUI, &gui)])?;
    let gui = slots.pop().unwrap_or_default();
    let fx = slots.pop().unwrap_or_default();
    Ok((shader, fx, gui))
}

impl UiRenderer {
    pub fn new(gpu: &mut dyn Backend, assets: &Assets) -> Result<UiRenderer, String> {
        let (pipeline, fx, gui) = Self::pipeline(gpu, assets)?;
        let (font, font_tex, font_idx) = Self::load_font(gpu, assets).ok_or("font missing")?;
        let frames = gpu.frames_in_flight();
        let screen = (0..frames).map(|_| gpu.create_buffer(16, usage::STORAGE | usage::HOST, "ui screen")).collect();
        Ok(UiRenderer {
            pipeline,
            font,
            font_tex,
            font_idx,
            textures: FxHashMap::default(),
            strips: FxHashMap::default(),
            slicing: FxHashMap::default(),
            outset: FxHashMap::default(),
            owned: Vec::new(),
            buffers: Vec::new(),
            screen,
            quads: Vec::new(),
            icon_clips: Vec::new(),
            stacks: Vec::new(),
            fx,
            gui,
            fx_slots: Vec::new(),
            anims: FxHashMap::default(),
            animated: Vec::new(),
        })
    }

    /// `core:fonts/default` (atlas and its `.font`) over `core:fonts/unifont`; packs can replace either.
    fn load_font(gpu: &mut dyn Backend, assets: &Assets) -> Option<(Font, TextureId, u32)> {
        // Built in too: text must show even without the engine's asset folder.
        let hex = assets.read("core:fonts/unifont", "", "hex").unwrap_or_else(|| include_bytes!("../../assets/fonts/unifont.hex").to_vec());
        let desc = assets.read("core:fonts/default", "", "font").unwrap_or_else(|| include_bytes!("../../assets/fonts/default.font").to_vec());
        let img = assets.read("core:fonts/default", "", "png").unwrap_or_else(|| include_bytes!("../../assets/fonts/default.png").to_vec());
        let (desc, img) = (Some(desc), crate::assets::decode_png(&img));
        let desc = desc.map(|d| String::from_utf8_lossy(&d).into_owned());
        let font = Font::new(&String::from_utf8_lossy(&hex), desc.as_deref().zip(img.as_ref()));
        let tex = gpu.create_texture(
            &TextureDesc { width: font.atlas_width, height: font.atlas_height, mips: 1, format: Format::R8Unorm, render_target: false },
            "font",
        );
        gpu.write_texture(tex, 0, &font.atlas);
        let idx = gpu.texture_index(tex);
        Some((font, tex, idx))
    }

    fn pipeline(gpu: &mut dyn Backend, assets: &Assets) -> Result<(PipelineId, Slots, Slots), String> {
        let (shader, fx, gui) = load_shader(assets).map_err(|e| e.0)?;
        let p = gpu.create_pipeline(&PipelineDesc {
            label: "ui",
            shader: &shader,
            vertex: "vs",
            fragment: Some("fs"),
            color: Some(Format::Surface),
            depth: None,
            blend: Blend::Alpha,
            cull_back: false,
            topology: Topology::Triangles,
        })
        .map_err(|e| e.0)?;
        Ok((p, fx, gui))
    }

    /// Shader slot of a text effect (`text::fx_id`), 0 when no snippet of that name exists.
    fn fx_slot(&mut self, id: u16) -> u32 {
        if id == 0 {
            return 0;
        }
        while self.fx_slots.len() < id as usize {
            let name = super::text::fx_name(self.fx_slots.len() as u16 + 1).unwrap_or_default();
            let slot = self.fx.slot(&name);
            if slot == 0 {
                log::warn!("text effect {name}: no shaders/fx/<name>.wgsl for it in any mod or pack");
            }
            self.fx_slots.push(slot);
        }
        self.fx_slots[id as usize - 1]
    }

    /// A rectangle filled by GUI shader `name` (`shaders/gui/<name>.wgsl`); false if there is none.
    pub fn shader_fill(&mut self, r: Rect, name: &str, color: u32) -> bool {
        let slot = self.gui.slot(name);
        if slot == 0 {
            return false;
        }
        self.quads.push(Quad { p0: [r.x, r.y], u: [r.w, 0.0], v: [0.0, r.h], uv: [0.0, 0.0, 1.0, 1.0], color, mode: ui_mode::GUI, fx: slot, ..Default::default() });
        true
    }

    pub fn reload(&mut self, gpu: &mut dyn Backend, assets: &Assets) {
        if let Some((font, tex, idx)) = Self::load_font(gpu, assets) {
            gpu.destroy_texture(self.font_tex);
            (self.font, self.font_tex, self.font_idx) = (font, tex, idx);
        }
        match Self::pipeline(gpu, assets) {
            Ok((p, fx, gui)) => {
                gpu.destroy_pipeline(self.pipeline);
                (self.pipeline, self.fx, self.gui) = (p, fx, gui);
                self.fx_slots.clear();
            }
            Err(e) => log::error!("ui shader: {e}"),
        }
        for t in self.owned.drain(..) {
            gpu.destroy_texture(t);
        }
        self.textures.clear();
        self.strips.clear();
        // How textures slice comes with them: a pack's `.gui` must not outlive the pack.
        self.slicing.clear();
        self.outset.clear();
        self.anims.clear();
    }

    /// Bindless index and size of a GUI texture, loaded the first time it is used.
    pub fn texture(&mut self, gpu: &mut dyn Backend, assets: &Assets, name: &str) -> Option<(u32, u32, u32)> {
        if let Some(t) = self.textures.get(name) {
            return *t;
        }
        if let Some(path) = name.strip_prefix(FACE) {
            let kind = crate::content::try_content().map(|c| c.game.entities[c.game.player_kind as usize].texture.clone()).unwrap_or_default();
            let skin = std::fs::read(path).ok().or_else(|| assets.read(&kind, "textures", "png"));
            let loaded = skin.as_deref().and_then(decode_png).and_then(|img| face_of(&img)).map(|rgba| {
                let t = gpu.create_texture(&TextureDesc { width: 8, height: 8, mips: 1, format: Format::Rgba8Srgb, render_target: false }, "face");
                gpu.write_texture(t, 0, &rgba);
                self.owned.push(t);
                (gpu.texture_index(t), 8, 8)
            });
            self.textures.insert(name.to_string(), loaded);
            return loaded;
        }
        let anim = assets.read(name, "textures", "anim").map(|b| String::from_utf8_lossy(&b).into_owned());
        let loaded = assets.read(name, "textures", "png").and_then(|b| decode_png(&b)).map(|img| {
            let t = gpu.create_texture(
                &TextureDesc { width: img.width, height: img.height, mips: 1, format: Format::Rgba8Srgb, render_target: false },
                name,
            );
            gpu.write_texture(t, 0, &img.rgba);
            self.owned.push(t);
            let idx = gpu.texture_index(t);
            // Animated like a block texture: frames stacked down (square, or `frame_height` pixels
            // high), `frametime` ticks each.
            let num = |src: &str, k: &str| crate::kv::get(src, k).and_then(|v| v.parse::<u32>().ok());
            let frame = anim.as_deref().map(|src| num(src, "frame_height").unwrap_or(img.width).max(1));
            match (anim.as_deref(), frame) {
                (Some(src), Some(fh)) if img.height > fh && img.height % fh == 0 => {
                    let ticks = num(src, "frametime").unwrap_or(1);
                    self.strips.insert(idx, (img.height / fh).min(0xFFFF) | ticks.clamp(1, 0xFFFF) << 16);
                    (idx, img.width, fh)
                }
                _ => (idx, img.width, img.height),
            }
        });
        if loaded.is_none() {
            log::warn!("missing gui texture {name}");
        }
        self.textures.insert(name.to_string(), loaded);
        loaded
    }

    fn declared_slicing(&mut self, assets: &Assets, name: &str, pixels: [f32; 2]) -> Option<Slicing> {
        if let Some(s) = self.slicing.get(name) {
            return *s;
        }
        let read = |ext: &str| assets.read(name, "textures", ext).map(|b| String::from_utf8_lossy(&b).into_owned());
        let gui = read("gui");
        let outset = gui.as_deref().and_then(|src| {
            crate::kv::pairs(src).find(|p| p.1 == "outset").and_then(|(line, k, v)| crate::kv::number(name, line, k, v))
        });
        self.outset.insert(name.to_string(), outset.unwrap_or(0.0));
        let s = gui.map(|src| parse_gui(&src, pixels));
        self.slicing.insert(name.to_string(), s);
        s
    }

    /// Loads the textures of a text's inline images, so `glyph_run` can draw them.
    pub fn load_images(&mut self, gpu: &mut dyn Backend, assets: &Assets, layout: &super::text::TextLayout) {
        for id in super::text::images(layout) {
            if let Some(name) = super::text::image_name(id) {
                self.texture(gpu, assets, &name);
            }
        }
    }

    /// Draws GUI texture `name` over `r` the way the texture asks for (its `.gui`), or with
    /// `fallback` when it says nothing. False if the texture is missing.
    pub fn draw_gui(&mut self, gpu: &mut dyn Backend, assets: &Assets, name: &str, r: Rect, scale: f32, color: u32, fallback: Slicing) -> bool {
        let Some(tex) = self.texture(gpu, assets, name) else { return false };
        let pixels = [tex.1 as f32, tex.2 as f32];
        let declared = self.declared_slicing(assets, name, pixels);
        let o = self.outset.get(name).copied().unwrap_or(0.0) * scale;
        let r = Rect { x: r.x - o, y: r.y - o, w: r.w + 2.0 * o, h: r.h + 2.0 * o };
        let slicing = declared.unwrap_or(match fallback {
            Slicing::Nine { border, .. } => Slicing::Nine { size: pixels, border },
            f => f,
        });
        match slicing {
            Slicing::Nine { size, border } => self.nine_sliced(r, tex.0, size, border, scale, color),
            Slicing::Tile { size, turn } => self.tile(r, tex, size[0] * scale, turn, color),
            Slicing::Stretch => self.image(r, tex.0, [0.0, 0.0, 1.0, 1.0], color),
        }
        true
    }

    /// A class's look in a state (`ui::look`): the shader its `.gui` names, else its texture,
    /// else the missing-texture checkerboard.
    #[allow(clippy::too_many_arguments)]
    pub fn class_background(&mut self, gpu: &mut dyn Backend, assets: &Assets, look: &Look, class: &str, state: &str, r: Rect, scale: f32) {
        if let Some(shader) = look.sidecar(class, "shader") {
            if self.shader_fill(r, &shader, 0xFFFFFFFF) {
                return;
            }
        }
        let drawn = look.texture_or_plain(class, state).is_some_and(|n| self.draw_gui(gpu, assets, &n, r, scale, 0xFFFFFFFF, Slicing::Stretch));
        if !drawn {
            self.checker(r, scale);
        }
    }

    /// Magenta and black squares (4 GUI pixels), where a class has no texture.
    pub fn checker(&mut self, r: Rect, scale: f32) {
        let cell = 4.0 * scale;
        let (nx, ny) = ((r.w / cell).ceil() as i32, (r.h / cell).ceil() as i32);
        self.rect(r, 0x000000FF);
        for j in 0..ny.min(64) {
            for i in (j % 2..nx.min(64)).step_by(2) {
                let (x, y) = (r.x + i as f32 * cell, r.y + j as f32 * cell);
                self.rect(Rect { x, y, w: cell.min(r.x + r.w - x), h: cell.min(r.y + r.h - y) }, 0xF800F8FF);
            }
        }
    }

    /// `draw_gui` for a texture already loaded (by an earlier `texture` call); false otherwise.
    pub fn draw_loaded(&mut self, name: &str, r: Rect, scale: f32, color: u32) -> bool {
        let Some(Some(tex)) = self.textures.get(name).copied() else { return false };
        let pixels = [tex.1 as f32, tex.2 as f32];
        let slicing = self.slicing.get(name).copied().flatten().unwrap_or(Slicing::Stretch);
        let o = self.outset.get(name).copied().unwrap_or(0.0) * scale;
        let r = Rect { x: r.x - o, y: r.y - o, w: r.w + 2.0 * o, h: r.h + 2.0 * o };
        match slicing {
            Slicing::Nine { size, border } => self.nine_sliced(r, tex.0, if size == [0.0; 2] { pixels } else { size }, border, scale, color),
            Slicing::Tile { size, turn } => self.tile(r, tex, size[0] * scale, turn, color),
            Slicing::Stretch => self.image(r, tex.0, [0.0, 0.0, 1.0, 1.0], color),
        }
        true
    }

    pub fn rect(&mut self, r: Rect, color: u32) {
        self.quads.push(Quad { p0: [r.x, r.y], u: [r.w, 0.0], v: [0.0, r.h], uv: [0.0; 4], color, mode: ui_mode::SOLID, ..Default::default() });
    }

    pub fn image(&mut self, r: Rect, tex: u32, uv: [f32; 4], color: u32) {
        self.quads.push(Quad { p0: [r.x, r.y], u: [r.w, 0.0], v: [0.0, r.h], uv, color, tex, mode: ui_mode::TEXTURE, ..Default::default() });
    }

    /// 9-slice: corners keep their size (scaled), edges and center stretch.
    pub fn nine(&mut self, r: Rect, tex: (u32, u32, u32), border: f32, scale: f32, color: u32) {
        self.nine_sliced(r, tex.0, [tex.1 as f32, tex.2 as f32], [border; 4], scale, color);
    }

    /// 9-slice with its own border per side, in GUI pixels of an image `size` GUI pixels big.
    pub fn nine_sliced(&mut self, r: Rect, idx: u32, size: [f32; 2], border: [f32; 4], scale: f32, color: u32) {
        let [l, t, rr, b] = border.map(|v| v * scale);
        let (l, rr) = (l.min(r.w / 2.0), rr.min(r.w / 2.0));
        let (t, b) = (t.min(r.h / 2.0), b.min(r.h / 2.0));
        let xs = [r.x, r.x + l, r.x + r.w - rr, r.x + r.w];
        let ys = [r.y, r.y + t, r.y + r.h - b, r.y + r.h];
        let us = [0.0, border[0] / size[0], 1.0 - border[2] / size[0], 1.0];
        let vs = [0.0, border[1] / size[1], 1.0 - border[3] / size[1], 1.0];
        for j in 0..3 {
            for i in 0..3 {
                let rr = Rect { x: xs[i], y: ys[j], w: xs[i + 1] - xs[i], h: ys[j + 1] - ys[j] };
                if rr.w > 0.0 && rr.h > 0.0 {
                    self.image(rr, idx, [us[i], vs[j], us[i + 1], vs[j + 1]], color);
                }
            }
        }
    }

    /// Repeats the texture every `size` physical pixels from the rect's corner; `turn`: each tile
    /// in one of the eight turns and mirrors, picked by its place (the same every frame).
    pub fn tile(&mut self, r: Rect, tex: (u32, u32, u32), size: f32, turn: bool, color: u32) {
        let mut y = r.y;
        let mut j = 0;
        while y < r.y + r.h {
            let h = size.min(r.y + r.h - y);
            let mut x = r.x;
            let mut i = 0;
            while x < r.x + r.w {
                let w = size.min(r.x + r.w - x);
                let (fw, fh) = (w / size, h / size);
                if turn {
                    let k = loom_noise::hash(0x7117, i, j, 0) as u32 & 7;
                    // Where a texture point lands in the tile (0..1 each way), and back.
                    let to_tile = |t: [f32; 2]| {
                        let t = if k & 4 != 0 { [1.0 - t[0], t[1]] } else { t };
                        match k & 3 {
                            0 => t,
                            1 => [1.0 - t[1], t[0]],
                            2 => [1.0 - t[0], 1.0 - t[1]],
                            _ => [t[1], 1.0 - t[0]],
                        }
                    };
                    let to_tex = |s: [f32; 2]| {
                        let t = match k & 3 {
                            0 => s,
                            1 => [s[1], 1.0 - s[0]],
                            2 => [1.0 - s[0], 1.0 - s[1]],
                            _ => [1.0 - s[1], s[0]],
                        };
                        if k & 4 != 0 { [1.0 - t[0], t[1]] } else { t }
                    };
                    // The texture rectangle the visible part shows, drawn along turned edges.
                    let corners = [to_tex([0.0, 0.0]), to_tex([fw, 0.0]), to_tex([0.0, fh]), to_tex([fw, fh])];
                    let (u0, v0) = (corners.iter().map(|c| c[0]).fold(1.0f32, f32::min), corners.iter().map(|c| c[1]).fold(1.0f32, f32::min));
                    let (u1, v1) = (corners.iter().map(|c| c[0]).fold(0.0f32, f32::max), corners.iter().map(|c| c[1]).fold(0.0f32, f32::max));
                    let at = |t: [f32; 2]| {
                        let s = to_tile(t);
                        [x + s[0] * size, y + s[1] * size]
                    };
                    let p0 = at([u0, v0]);
                    let (pu, pv) = (at([u1, v0]), at([u0, v1]));
                    self.quads.push(Quad {
                        p0,
                        u: [pu[0] - p0[0], pu[1] - p0[1]],
                        v: [pv[0] - p0[0], pv[1] - p0[1]],
                        uv: [u0, v0, u1, v1],
                        color,
                        tex: tex.0,
                        mode: ui_mode::TEXTURE,
                        ..Default::default()
                    });
                } else {
                    self.image(Rect { x, y, w, h }, tex.0, [0.0, 0.0, fw, fh], color);
                }
                x += size;
                i += 1;
            }
            y += size;
            j += 1;
        }
    }

    pub fn glyph_run(&mut self, items: &[Placed], x: f32, y: f32, px: f32, default_color: u32, shadow: Option<u32>, icons: &mut Vec<(f32, f32, f32, u32)>) {
        for pass in [true, false] {
            if pass && shadow.is_none() {
                continue;
            }
            let off = if pass { px * self.font.pixel } else { 0.0 };
            for (index, it) in items.iter().enumerate() {
                match *it {
                    Placed::Glyph { x: gx, y: gy, ch, color, fx } => {
                        if ch == ' ' {
                            continue;
                        }
                        let fx = self.fx_slot(fx) | if pass && fx != 0 { 1 << 31 } else { 0 };
                        let g = self.font.glyph(ch);
                        let uv = self.font.uv(g);
                        let w = (g.advance as f32 - 1.0) * px;
                        let c = if pass {
                            shadow.unwrap()
                        } else {
                            color.map_or(default_color, |c| c << 8 | (default_color & 0xFF))
                        };
                        // Whole physical pixels: centered text starts mid-pixel, and nearest sampling
                        // at texel borders would drop or double glyph columns.
                        self.quads.push(Quad {
                            p0: [(x + gx * px + off).round(), (y + gy * px + off).round()],
                            u: [w, 0.0],
                            v: [0.0, 16.0 * px],
                            uv,
                            color: c,
                            tex: self.font_idx,
                            mode: ui_mode::FONT,
                            fx,
                            index: index as f32,
                            ..Default::default()
                        });
                    }
                    Placed::Icon { x: ix, y: iy, item } if !pass => icons.push((x + ix * px, y + iy * px, 16.0 * px, item)),
                    Placed::Block { x: ix, y: iy, state } if !pass => icons.push((x + ix * px, y + iy * px, 16.0 * px, state | super::text::STATE_ICON)),
                    // Loaded by `load_images` before.
                    Placed::Image { x: ix, y: iy, w, h, image } if !pass => {
                        if let Some(Some(t)) = super::text::image_name(image).and_then(|n| self.textures.get(&n).copied()) {
                            self.image(Rect { x: x + ix * px, y: y + iy * px, w: w * px, h: h * px }, t.0, [0.0, 0.0, 1.0, 1.0], 0xFFFFFFFF);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Item icon: flat sprite, or an isometric cube for block items.
    pub fn item_icon(&mut self, c: &Content, tex: &GameTextures, x: f32, y: f32, size: f32, item: u32) {
        let Some(def) = c.items.get(item as usize) else { return };
        if item == 0 {
            return;
        }
        let looks = crate::client::looks::current();
        let look = looks.item(item);
        let model = look.and_then(|l| l.model.as_deref()).unwrap_or(&def.model);
        let icon = look.and_then(|l| l.icon).unwrap_or(def.icon);
        if !model.is_empty() {
            self.boxes_icon(tex, x, y, size, model, &c.states[0]);
            return;
        }
        if icon != u32::MAX {
            self.quads.push(Quad {
                p0: [x, y],
                u: [size, 0.0],
                v: [0.0, size],
                uv: [0.0, 0.0, 1.0, 1.0],
                color: 0xFFFFFFFF,
                tex: tex.texture(icon),
                mode: ui_mode::ICON,
                ..Default::default()
            });
            return;
        }
        let Some(state) = def.block else { return };
        self.state_icon(c, tex, x, y, size, state);
    }

    /// A block state the way block items show: a sprite for flat blocks, else an isometric model.
    pub fn state_icon(&mut self, c: &Content, tex: &GameTextures, x: f32, y: f32, size: f32, state: u32) {
        let Some(s) = c.states.get(state as usize) else { return };
        if let Some(t) = s.item_sprite() {
            let tint = if s.tint == 0 || s.tint_faces & 1 == 0 { 0xFFFFFFFF } else { crate::content::item_tint(s.tint) << 8 | 0xFF };
            self.quads.push(Quad {
                p0: [x, y],
                u: [size, 0.0],
                v: [0.0, size],
                uv: [0.0, 0.0, 1.0, 1.0],
                color: tint,
                tex: tex.texture(t),
                mode: ui_mode::ICON,
                ..Default::default()
            });
            return;
        }
        self.boxes_icon(tex, x, y, size, &s.item_shape(), s);
    }

    /// Model boxes drawn isometric, tinted like `s` (air for an item's own model).
    fn boxes_icon(&mut self, tex: &GameTextures, x: f32, y: f32, size: f32, boxes: &[crate::content::ModelBox], s: &crate::content::StateInfo) {
        let tint = |face: usize| -> u32 {
            if s.tint == 0 || s.tint_faces & (1 << face) == 0 {
                return 0xFFFFFFFF;
            }
            crate::content::item_tint(s.tint) << 8 | 0xFF
        };
        let shade = |c: u32, k: f32| {
            let ch = |s: u32| (((c >> s) & 255) as f32 * k) as u32;
            ch(24) << 24 | ch(16) << 16 | ch(8) << 8 | 0xFF
        };
        // Isometric: block point (X, Y, Z) lands at origin + X*ex + Y*ey + Z*ez.
        let (h, q) = (size / 2.0, size / 4.0);
        let o = [x + h, y + size * 0.03 + h];
        let at = |p: [f32; 3]| [o[0] + (p[0] - p[2]) * h, o[1] + (p[0] + p[2]) * q - p[1] * h];
        let grass = crate::content::item_tint(s.tint) << 8 | 0xFF;
        let mut shape = boxes.to_vec();
        // Back to front.
        shape.sort_by(|a, b| (a.min[0] + a.min[1] + a.min[2]).total_cmp(&(b.min[0] + b.min[1] + b.min[2])));
        for b in &shape {
            let (n, m) = (b.min, b.max);
            let (dx, dy, dz) = (m[0] - n[0], m[1] - n[1], m[2] - n[2]);
            // (corner, u edge, v edge, face, shade, uv rect): the faces seen from the front, with
            // the part of the texture the world mesher shows there.
            let faces = [
                (at([n[0], m[1], m[2]]), [dx * h, dx * q], [0.0, dy * h], 5, 0.8, [n[0], 1.0 - m[1], m[0], 1.0 - n[1]]),
                (at([m[0], m[1], m[2]]), [dz * h, -dz * q], [0.0, dy * h], 1, 0.62, [1.0 - m[2], 1.0 - m[1], 1.0 - n[2], 1.0 - n[1]]),
                (at([n[0], m[1], n[2]]), [dx * h, dx * q], [-dz * h, dz * q], 3, 1.0, [n[0], n[2], m[0], m[2]]),
            ];
            for (p0, u, v, face, k, uv) in faces {
                self.quads.push(Quad { p0, u, v, uv, color: shade(tint(face), k), tex: tex.texture(b.textures[face]), mode: ui_mode::ICON, ..Default::default() });
                // Tinted overlay on top of its face.
                if let Some(&ov) = tex.overlays.get(b.textures[face] as usize).filter(|&&ov| ov != 0) {
                    let color = shade(if s.tint == 0 { 0xFFFFFFFF } else { grass }, k);
                    self.quads.push(Quad { p0, u, v, uv, color, tex: tex.texture(ov - 1), mode: ui_mode::ICON, ..Default::default() });
                }
            }
        }
    }

    /// Regenerates quads for the whole tree. `custom` draws engine-specific nodes (slots).
    pub fn build(
        &mut self,
        gpu: &mut dyn Backend,
        assets: &Assets,
        ui: &Ui,
        look: &Look,
        scale: f32,
        game: Option<(&Content, &GameTextures)>,
        custom: &mut dyn FnMut(&mut UiRenderer, u32, u64, Rect, bool, Option<u32>),
    ) {
        self.quads.clear();
        self.animated.clear();
        self.append(gpu, assets, ui, look, scale, game, custom);
    }

    /// `build` on top of what is there: overlays drawn every frame (labels, the tooltip).
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &mut self,
        gpu: &mut dyn Backend,
        assets: &Assets,
        ui: &Ui,
        look: &Look,
        scale: f32,
        game: Option<(&Content, &GameTextures)>,
        custom: &mut dyn FnMut(&mut UiRenderer, u32, u64, Rect, bool, Option<u32>),
    ) {
        self.icon_clips.clear();
        self.stacks.clear();
        let mut icons = Vec::new();
        self.node(gpu, assets, ui, look, 0, scale, &mut icons, custom, None);
        if let Some((c, t)) = game {
            let clips = std::mem::take(&mut self.icon_clips);
            for (i, (x, y, s, item)) in icons.into_iter().enumerate() {
                let start = self.quads.len();
                self.text_icon(c, t, x, y, s, item);
                let clip = clips.get(i).copied().unwrap_or_default();
                for q in &mut self.quads[start..] {
                    q.clip = clip;
                }
            }
            for (r, stack, clip) in std::mem::take(&mut self.stacks) {
                let start = self.quads.len();
                self.stack_icon(c, t, r, stack, scale, ui.text_unit);
                for q in &mut self.quads[start..] {
                    q.clip = clip;
                }
            }
        }
    }

    /// A stack the one way slots, the hotbar, item nodes and the held cursor stack show it: the
    /// icon centered in `r`, the durability bar, the count at the bottom right.
    pub fn stack_icon(&mut self, c: &Content, textures: &GameTextures, r: Rect, stack: loom_abi::ItemStack, scale: f32, text_unit: f32) {
        if stack.is_empty() {
            return;
        }
        let s = 16.0 * scale;
        let (x, y) = (r.x + (r.w - s) / 2.0, r.y + (r.h - s) / 2.0);
        self.item_icon(c, textures, x, y, s, stack.item);
        let def = &c.items[stack.item as usize];
        let damage = crate::stack::damage(&stack);
        if def.durability > 0 && damage > 0 {
            let f = 1.0 - damage as f32 / def.durability as f32;
            let bar = Rect { x: x + scale, y: y + s - 2.0 * scale, w: 14.0 * scale, h: 2.0 * scale };
            self.rect(bar, 0x000000FF);
            let g = (f * 255.0) as u32;
            self.rect(Rect { w: 13.0 * scale * f, h: scale, ..bar }, ((255 - g) << 24) | (g << 16) | 0xFF);
        }
        if stack.count > 1 {
            let px = scale * text_unit;
            let layout = super::text::layout(&super::text::plain(&stack.count.to_string()), &self.font, None);
            let mut none = Vec::new();
            self.glyph_run(&layout.items, x + s - layout.width * px + scale, y + s - 16.0 * px + scale, px, 0xFFFFFFFF, Some(0x3F3F3FFF), &mut none);
        }
    }

    /// The hovered node's tooltip next to the mouse (physical pixels), drawn every frame on top:
    /// a `tooltip` panel with the text in it, like any other part of the UI.
    #[allow(clippy::too_many_arguments)]
    pub fn tooltip(&mut self, gpu: &mut dyn Backend, assets: &Assets, ui: &Ui, look: &Look, scale: f32, at: glam::Vec2, game: Option<(&Content, &GameTextures)>) {
        let Some(tip) = ui.hovered.and_then(|h| ui.nodes.get(h)).and_then(|n| n.tooltip.as_ref()) else { return };
        let mut t = Ui::new();
        t.text_unit = ui.text_unit;
        let place = |t: &mut Ui, x: f32, y: f32| t.nodes[1].style.at = Some([x, y, 0.0, 0.0]);
        // Vanilla's spacing: lines 10 GUI pixels apart, padding inside the frame, at most 320 wide.
        t.panel("tooltip", Style::default().pad(4.0), |t| {
            let id = t.text(tip.clone(), "tooltip");
            if let Kind::Text(n) = &mut t.nodes[id].kind {
                n.line = Some(10.0 / ui.text_unit);
            }
            t.nodes[id].style.max_w = 320.0;
        });
        let cursor = at / scale;
        place(&mut t, cursor.x + 12.0, cursor.y - 12.0);
        t.layout(ui.size, &self.font);
        // Like vanilla: right of and slightly above the cursor, flipped to the left near the edge.
        let r = t.nodes[1].rect;
        let x = if r.x + r.w > ui.size.x { (cursor.x - r.w - 12.0).max(0.0) } else { r.x };
        let y = r.y.min(ui.size.y - r.h).max(0.0);
        if (x, y) != (r.x, r.y) {
            place(&mut t, x, y);
            t.layout(ui.size, &self.font);
        }
        self.append(gpu, assets, &t, look, scale, game, &mut |_, _, _, _, _, _| {});
    }

    /// An icon placed by rich text: an item, or a block state (`{block:..}`).
    fn text_icon(&mut self, c: &Content, t: &GameTextures, x: f32, y: f32, s: f32, icon: u32) {
        if icon & super::text::STATE_ICON != 0 {
            self.state_icon(c, t, x, y, s, icon & !super::text::STATE_ICON);
        } else {
            self.item_icon(c, t, x, y, s, icon);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn node(
        &mut self,
        gpu: &mut dyn Backend,
        assets: &Assets,
        ui: &Ui,
        look: &Look,
        id: usize,
        scale: f32,
        icons: &mut Vec<(f32, f32, f32, u32)>,
        custom: &mut dyn FnMut(&mut UiRenderer, u32, u64, Rect, bool, Option<u32>),
        surface: Option<&str>,
    ) {
        let n = &ui.nodes[id];
        let r = Rect { x: n.rect.x * scale, y: n.rect.y * scale, w: n.rect.w * scale, h: n.rect.h * scale };
        let (quad_start, icon_start) = (self.quads.len(), icons.len());
        let hovered = ui.hovered == Some(id);
        let mut background = |me: &mut UiRenderer, class: &str, state: &str| me.class_background(gpu, assets, look, class, state, r, scale);
        match &n.kind {
            Kind::Container { class: Some(class) } => background(self, class, ""),
            Kind::Container { class: None } => {}
            Kind::Button { class, enabled, selected, .. } => {
                let state = if !enabled {
                    "disabled"
                } else if ui.pressed == Some(id) || *selected {
                    "pressed"
                } else if hovered {
                    "hover"
                } else {
                    ""
                };
                background(self, class, state);
            }
            Kind::Text(t) => {
                let px = scale * ui.text_unit * t.scale;
                let parent = ui.nodes.iter().position(|p| p.children.contains(&id));
                let parent_hover = parent.is_some_and(|p| ui.hovered == Some(p));
                let parent_disabled = parent.is_some_and(|p| matches!(ui.nodes[p].kind, Kind::Button { enabled: false, .. }));
                let state = if parent_disabled { "disabled" } else if parent_hover { "hover" } else { "" };
                // Generic classes inherit from the surface: `default` its text color, `dim` its
                // dim color.
                let class = match (t.class.as_str(), surface) {
                    ("default", Some(s)) => s,
                    _ => t.class.as_str(),
                };
                let (class, state) = match surface {
                    Some(s) if t.class == "dim" && look.has_text(&format!("{s}_dim")) => (s, "dim"),
                    _ => (class, state),
                };
                let default = t.color.unwrap_or_else(|| look.text(class, state));
                // A shadow fades with its text (the held item's name).
                let shadow = if parent_disabled { None } else { look.shadow(class).map(|s| (s & !0xFF) | (s & 0xFF) * (default & 0xFF) / 255) };
                let x = if t.center { r.x + (r.w - t.layout.width * px) / 2.0 } else { r.x };
                self.load_images(gpu, assets, &t.layout);
                self.glyph_run(&t.layout.items, x, r.y, px, default, shadow, icons);
            }
            Kind::Image { texture, color } => {
                if let Some((idx, _, _)) = self.texture(gpu, assets, texture) {
                    self.image(r, idx, [0.0, 0.0, 1.0, 1.0], *color);
                }
            }
            Kind::Item { last, .. } => {
                if last.0 != 0 {
                    let clip = n.clip.map_or([0.0; 4], |c| [c.x * scale, c.y * scale, (c.x + c.w) * scale, (c.y + c.h) * scale]);
                    self.stacks.push((r, loom_abi::ItemStack::new(last.0, last.1.max(1)), clip));
                }
            }
            Kind::Progress { class, value, dir, .. } => {
                background(self, class, "");
                let fill = look.texture(class, "fill").and_then(|n| self.texture(gpu, assets, &n));
                let v = value.clamp(0.0, 1.0);
                let (fr, uv) = match *dir {
                    loom_abi::ui_dir::UP => (Rect { y: r.y + r.h * (1.0 - v), h: r.h * v, ..r }, [0.0, 1.0 - v, 1.0, 1.0]),
                    loom_abi::ui_dir::LEFT => (Rect { x: r.x + r.w * (1.0 - v), w: r.w * v, ..r }, [1.0 - v, 0.0, 1.0, 1.0]),
                    loom_abi::ui_dir::DOWN => (Rect { h: r.h * v, ..r }, [0.0, 0.0, 1.0, v]),
                    _ => (Rect { w: r.w * v, ..r }, [0.0, 0.0, v, 1.0]),
                };
                match fill {
                    Some((idx, _, _)) => self.image(fr, idx, uv, 0xFFFFFFFF),
                    None => self.checker(fr, scale),
                }
            }
            Kind::Icons { class, count, value, rtl, .. } if *value >= 0.0 => {
                let names = ["full", "half", "empty"].map(|s| look.texture(class, s));
                let tex = names.map(|n| n.and_then(|n| self.texture(gpu, assets, &n)));
                for i in 0..*count {
                    let v = *value - i as f32 * 2.0;
                    let which = if v >= 2.0 { 0 } else if v >= 1.0 { 1 } else { 2 };
                    let slot = if *rtl { count - 1 - i } else { i };
                    let rr = Rect { x: r.x + slot as f32 * 8.0 * scale, y: r.y, w: 9.0 * scale, h: 9.0 * scale };
                    if let Some((idx, _, _)) = tex[2] {
                        self.image(rr, idx, [0.0, 0.0, 1.0, 1.0], 0xFFFFFFFF);
                    }
                    if which < 2 {
                        if let Some((idx, _, _)) = tex[which] {
                            self.image(rr, idx, [0.0, 0.0, 1.0, 1.0], 0xFFFFFFFF);
                        } else {
                            self.checker(rr, scale);
                        }
                    }
                }
            }
            Kind::Icons { .. } => {}
            Kind::Slider { value, .. } => {
                let k = Rect { x: r.x + (r.w - 8.0 * scale) * value.clamp(0.0, 1.0), y: r.y, w: 8.0 * scale, h: r.h };
                self.class_background(gpu, assets, look, "slider", "", r, scale);
                self.class_background(gpu, assets, look, "slider_knob", if hovered { "hover" } else { "" }, k, scale);
            }
            Kind::Input { value, caret, .. } => {
                background(self, "input", if ui.focused == Some(id) { "focused" } else { "" });
                let px = scale * ui.text_unit;
                let spans = super::text::plain(value);
                let layout = super::text::layout(&spans, &self.font, None);
                let before: String = value.chars().take(*caret).collect();
                let caret_x = super::text::layout(&super::text::plain(&before), &self.font, None).width * px;
                // Text longer than the field slides left so the caret stays visible; the rest is
                // clipped to the field.
                let room = r.w - 8.0 * scale;
                let shift = (caret_x - room + px).max(0.0);
                let tx = r.x + 4.0 * scale - shift;
                let ty = r.y + (r.h - 16.0 * px) / 2.0;
                let start = self.quads.len();
                if let Some(a) = ui.anchor.filter(|&a| ui.focused == Some(id) && a != *caret) {
                    let x_at = |n: usize| super::text::layout(&super::text::plain(&value.chars().take(n).collect::<String>()), &self.font, None).width * px;
                    let (x0, x1) = (x_at(a.min(*caret)), x_at(a.max(*caret)));
                    self.class_background(gpu, assets, look, "input_selection", "", Rect { x: tx + x0, y: ty, w: x1 - x0, h: 16.0 * px }, scale);
                }
                self.glyph_run(&layout.items, tx, ty, px, look.text("input", ""), None, icons);
                if ui.focused == Some(id) {
                    self.class_background(gpu, assets, look, "caret", "", Rect { x: tx + caret_x, y: ty, w: px.max(1.0), h: 16.0 * px }, scale);
                }
                let field = [r.x + 2.0 * scale, r.y, r.x + r.w - 2.0 * scale, r.y + r.h];
                for q in &mut self.quads[start..] {
                    q.clip = field;
                }
            }
            Kind::Custom { tag, data } => {
                // What engine-drawn nodes use, loaded for `draw_loaded`.
                for class in ["slot", "hotbar", "hotbar_selected", "slot_highlight", "well", "crosshair"] {
                    if let Some(name) = look.texture(class, "") {
                        if let Some(t) = self.texture(gpu, assets, &name) {
                            self.declared_slicing(assets, &name, [t.1 as f32, t.2 as f32]);
                        }
                    }
                }
                let icon = n.icon.clone().and_then(|i| self.texture(gpu, assets, &i)).map(|t| t.0);
                custom(self, *tag, *data, r, hovered, icon)
            }
        }
        // Children of a panel or button whose class has a text color read colors from it.
        let own = match &n.kind {
            Kind::Container { class: Some(c) } | Kind::Button { class: c, .. } if look.has_text(c) => Some(c.as_str()),
            _ => None,
        };
        let inner_surface = own.or(surface);
        let clip = n.clip.map_or([0.0; 4], |c| [c.x * scale, c.y * scale, (c.x + c.w) * scale, (c.y + c.h) * scale]);
        if n.clip.is_some() {
            for q in &mut self.quads[quad_start..] {
                // Quads may carry their own clip (text fields): keep the overlap of both.
                q.clip = if q.clip[2] > q.clip[0] {
                    [q.clip[0].max(clip[0]), q.clip[1].max(clip[1]), q.clip[2].min(clip[2]), q.clip[3].min(clip[3])]
                } else {
                    clip
                };
            }
        }
        self.icon_clips.resize(icon_start, [0.0; 4]);
        self.icon_clips.resize(icons.len(), clip);
        for &c in &n.children {
            self.node(gpu, assets, ui, look, c, scale, icons, custom, inner_surface);
        }
        if let Some(bar) = n.scrollbar {
            let start = self.quads.len();
            let px = |r: super::Rect| Rect { x: r.x * scale, y: r.y * scale, w: r.w * scale, h: r.h * scale };
            let track = px(super::Rect { x: bar.track.x + (bar.track.w - 6.0) / 2.0, w: 6.0, ..bar.track });
            self.class_background(gpu, assets, look, "scrollbar", "", track, scale);
            let handle = bar.handle();
            let over = ui.dragging_bar() == Some(id);
            self.class_background(gpu, assets, look, "scrollbar_handle", if over { "hover" } else { "" }, px(handle), scale);
            if n.clip.is_some() {
                for q in &mut self.quads[start..] {
                    q.clip = clip;
                }
            }
        }
        let class = match &n.kind {
            Kind::Container { class: Some(c) } | Kind::Button { class: c, .. } | Kind::Progress { class: c, .. } | Kind::Icons { class: c, .. } => Some(c.as_str()),
            Kind::Text(t) => Some(t.class.as_str()),
            _ => None,
        };
        // Item icons are queued apart and do not follow these animations yet.
        if let Some(file) = class.and_then(|c| look.sidecar(c, "animation")) {
            let list = self.anims.entry(file.clone()).or_insert_with(|| {
                let src = assets.read(&file, "animations", "animation.json").map(|b| String::from_utf8_lossy(&b).into_owned());
                match src.map(|s| crate::model::load_animations(&s)) {
                    Some(Ok(list)) => list.into_iter().map(Arc::new).collect(),
                    Some(Err(e)) => {
                        log::error!("ui animation {file}: {e}");
                        Vec::new()
                    }
                    None => {
                        log::error!("ui animation {file}: no such file");
                        Vec::new()
                    }
                }
            });
            let pick = |name: &str| list.iter().find(|a| a.name == name).cloned();
            let (open, idle) = (pick("open"), pick("idle"));
            if open.is_some() || idle.is_some() {
                let center = [r.x + r.w / 2.0, r.y + r.h / 2.0];
                self.animated.push(Animated { quads: quad_start..self.quads.len(), open, idle, since: ui.shown, center, scale });
            }
        }
    }

    pub fn draw(&mut self, gpu: &mut dyn Backend, list: &mut CommandList, frame: &FrameInfo, gui_scale: f32) {
        if self.quads.is_empty() {
            return;
        }
        if !self.strips.is_empty() {
            for q in &mut self.quads {
                if q.mode != ui_mode::FONT {
                    q.layer = self.strips.get(&q.tex).copied().unwrap_or(0);
                }
            }
        }
        let buf = crate::gpu_layout::frame_buffer(gpu, &mut self.buffers, frame.slot, self.quads.len() as u32, size_of::<Quad>() as u64, usage::STORAGE, "ui quads");
        gpu.mapped(buf)[..self.quads.len() * size_of::<Quad>()].copy_from_slice(bytemuck::cast_slice(&self.quads));
        let sb = self.screen[frame.slot];
        let time = crate::shaders::time();
        gpu.mapped(sb)[..16].copy_from_slice(bytemuck::cast_slice(&[frame.width as f32, frame.height as f32, time, gui_scale]));
        list.pipeline(self.pipeline);
        list.bind(crate::gpu_layout::bind::ui::QUADS, buf);
        list.bind(crate::gpu_layout::bind::ui::SCREEN, sb);
        list.draw(self.quads.len() as u32 * 6, 1);
    }

    pub fn destroy(&mut self, gpu: &mut dyn Backend) {
        gpu.destroy_texture(self.font_tex);
    }
}

#[cfg(test)]
mod slicing_tests {
    use super::*;

    #[test]
    fn gui_sidecar() {
        assert_eq!(parse_gui("border = 2 3 4 5\n", [16.0, 16.0]), Slicing::Nine { size: [16.0, 16.0], border: [2.0, 3.0, 4.0, 5.0] });
        assert_eq!(parse_gui("scaling = tile\nsize = 32 32", [64.0, 64.0]), Slicing::Tile { size: [32.0, 32.0], turn: false });
        assert_eq!(parse_gui("scaling = tile\nturn = random", [32.0, 32.0]), Slicing::Tile { size: [32.0, 32.0], turn: true });
    }
}

#[cfg(test)]
mod animation_tests {
    use super::*;

    #[test]
    fn keyframes_move_quads() {
        let src = r#"{"animations":{"animation.ui.idle":{"loop":true,"animation_length":2,"bones":{"node":{
            "position":{"0":[2,1,0]},"rotation":{"0":[0,0,90]}}}}}}"#;
        let idle = crate::model::load_animations(src).unwrap().pop().map(Arc::new);
        let a = Animated { quads: 0..1, open: None, idle, since: 0.0, center: [10.0, 10.0], scale: 2.0 };
        let mut q = [Quad { p0: [12.0, 10.0], u: [4.0, 0.0], v: [0.0, 4.0], ..Default::default() }];
        crate::shaders::pin_time(true);
        animate(&mut q, &[a]);
        // A quarter turn clockwise about the center, then 2 and 1 GUI pixels right and up.
        let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4;
        assert!(near(q[0].p0, [14.0, 10.0]) && near(q[0].u, [0.0, 4.0]) && near(q[0].v, [-4.0, 0.0]), "{:?} {:?} {:?}", q[0].p0, q[0].u, q[0].v);
    }
}
