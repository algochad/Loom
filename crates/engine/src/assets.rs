//! Asset lookup. A name like `base:block/stone` resolves to `textures/block/stone.png` inside the
//! `base` mod's assets, unless an enabled resource pack provides `base/textures/block/stone.png`.

use std::path::{Path, PathBuf};

use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Clone)]
pub struct Assets {
    mods: FxHashMap<String, PathBuf>,
    /// Highest priority first.
    packs: Vec<PathBuf>,
    /// Every file under the roots, listed once: a content-heavy mod asks for thousands of files
    /// (and for optional ones like `.anim` that mostly do not exist), and each existence check
    /// costs a system call.
    files: FxHashSet<PathBuf>,
}

impl Assets {
    pub fn new(mods: impl IntoIterator<Item = (String, PathBuf)>, packs: Vec<PathBuf>) -> Assets {
        let mut a = Assets { mods: mods.into_iter().collect(), packs, files: FxHashSet::default() };
        a.rescan();
        a
    }

    /// Resource pack folders, highest priority first; call `rescan` after.
    pub fn set_packs(&mut self, packs: Vec<PathBuf>) {
        self.packs = packs;
    }

    /// Re-lists files after they changed on disk (hot reload).
    pub fn rescan(&mut self) {
        let mut files = FxHashSet::default();
        for root in self.mods.values().chain(&self.packs) {
            walk(root, &mut |p| {
                files.insert(p.to_path_buf());
            });
        }
        self.files = files;
    }

    /// `kind` is the top folder ("textures", "lang", "ui", "shaders").
    pub fn path(&self, name: &str, kind: &str, ext: &str) -> Option<PathBuf> {
        let (ns, rel) = name.split_once(':').unwrap_or(("core", name));
        let file = format!("{kind}/{rel}.{ext}");
        for p in &self.packs {
            let candidate = p.join(ns).join(&file);
            if self.files.contains(&candidate) {
                return Some(candidate);
            }
        }
        let candidate = self.mods.get(ns)?.join(&file);
        self.files.contains(&candidate).then_some(candidate)
    }

    /// `path`, if a resource pack has the file (not only a mod).
    pub fn pack_path(&self, name: &str, kind: &str, ext: &str) -> Option<PathBuf> {
        let (ns, rel) = name.split_once(':').unwrap_or(("core", name));
        let file = format!("{kind}/{rel}.{ext}");
        self.packs.iter().map(|p| p.join(ns).join(&file)).find(|c| self.files.contains(c))
    }

    pub fn read(&self, name: &str, kind: &str, ext: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(name, kind, ext)?).ok()
    }

    /// `<name><suffix>.<ext>` from the same pack or mod as `<name>.<ext>` itself: a mask drawn
    /// for one texture must not land on a pack's different picture of it.
    pub fn beside(&self, name: &str, kind: &str, ext: &str, suffix: &str) -> Option<PathBuf> {
        let own = self.path(name, kind, ext)?;
        let stem = own.file_stem()?.to_str()?;
        let p = own.with_file_name(format!("{stem}{suffix}.{ext}"));
        self.files.contains(&p).then_some(p)
    }

    /// Every file of a kind across mods (pack overrides win), keyed by `ns:relative/path`.
    pub fn list(&self, kind: &str, ext: &str) -> Vec<(String, PathBuf)> {
        let mut out: FxHashMap<String, PathBuf> = FxHashMap::default();
        let mut add = |ns: &str, root: &Path| {
            let dir = root.join(kind);
            walk(&dir, &mut |p| {
                if p.extension().and_then(|e| e.to_str()) == Some(ext) {
                    let rel = p.strip_prefix(&dir).unwrap().with_extension("");
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    out.insert(format!("{ns}:{rel}"), p.to_path_buf());
                }
            });
        };
        for (ns, root) in &self.mods {
            add(ns, root);
        }
        for pack in self.packs.iter().rev() {
            for (ns, _) in &self.mods {
                add(ns, &pack.join(ns));
            }
        }
        let mut v: Vec<_> = out.into_iter().collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Like `list`, but every file instead of the one that wins: the engine's, then the mods' own
    /// files (by name), then each pack's from the lowest to the top one. For files read on top of each
    /// other, like translations, where a pack changes single keys.
    pub fn layers(&self, kind: &str, ext: &str) -> Vec<(String, PathBuf)> {
        let collect = |roots: &mut dyn Iterator<Item = (&String, PathBuf)>| {
            let mut v = Vec::new();
            for (ns, root) in roots {
                let dir = root.join(kind);
                walk(&dir, &mut |p| {
                    if p.extension().and_then(|e| e.to_str()) == Some(ext) {
                        let rel = p.strip_prefix(&dir).unwrap().with_extension("");
                        v.push((format!("{ns}:{}", rel.to_string_lossy().replace('\\', "/")), p.to_path_buf()));
                    }
                });
            }
            v.sort_by(|a: &(String, PathBuf), b| a.0.cmp(&b.0));
            v
        };
        let mut out = collect(&mut self.mods.iter().map(|(ns, root)| (ns, root.clone())));
        // The engine's defaults under everything a mod says.
        out.sort_by_key(|f| !f.0.starts_with("core:"));
        for pack in self.packs.iter().rev() {
            out.extend(collect(&mut self.mods.keys().map(|ns| (ns, pack.join(ns)))));
        }
        out
    }

}

fn walk(dir: &Path, f: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, f);
        } else {
            f(&p);
        }
    }
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn decode_png(bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..(w * h * 4) as usize].to_vec(),
        png::ColorType::GrayscaleAlpha => buf[..(w * h * 2) as usize]
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Rgb => buf[..(w * h * 3) as usize].chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => buf[..(w * h) as usize].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some(Image { width: w, height: h, rgba })
}

pub fn decode_png_file(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let img = decode_png(&std::fs::read(path).ok()?)?;
    Some((img.width, img.height, img.rgba))
}

pub fn encode_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(std::io::Error::other)?;
    w.write_image_data(rgba).map_err(std::io::Error::other)?;
    Ok(())
}

fn missing(size: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let on = ((x * 2 / size) + (y * 2 / size)) % 2 == 0;
            v.extend_from_slice(if on { &[255, 0, 220, 255] } else { &[0, 0, 0, 255] });
        }
    }
    v
}

/// One texture ready for the GPU: its frames (square, at the file's own size rounded up to a
/// power of two, each with its mip chain), a glow mask per frame (full size only) if it has one
/// (see `emission_mask`), and its animation over frame indices.
pub struct LoadedTexture {
    pub size: u32,
    pub frames: Vec<Vec<Vec<u8>>>,
    pub glow: Vec<Option<Vec<u8>>>,
    pub anim: Option<Anim>,
}

/// Emission like LabPBR, only that part of it: `<name>_s.png` (a LabPBR specular map) glows by
/// its alpha, 0..254 from none to full and 255 for none; or our own `<name>_e.png`, glowing by
/// its red channel times alpha. Same size and frames as the texture, and from the same pack or
/// mod. One byte per texel.
pub fn emission_mask(assets: &Assets, name: &str) -> Option<Image> {
    let read = |suffix: &str| assets.beside(name, "textures", "png", suffix).and_then(|p| std::fs::read(p).ok()).and_then(|b| decode_png(&b));
    if let Some(mut img) = read("_e") {
        img.rgba = img.rgba.chunks_exact(4).map(|p| (p[0] as u32 * p[3] as u32 / 255) as u8).collect();
        return Some(img);
    }
    let mut img = read("_s")?;
    img.rgba = img.rgba.chunks_exact(4).map(|p| if p[3] == 255 { 0 } else { (p[3] as u32 * 255 / 254) as u8 }).collect();
    Some(img)
}

/// One frame of a mask (one byte per texel) scaled to `to` like the texture's frames.
fn mask_frame(mask: &Image, frame: u32, to: u32) -> Vec<u8> {
    let from = mask.width;
    let frames = (mask.height / from.max(1)).max(1);
    let f = (frame % frames) as usize * (from * from) as usize;
    let mut out = vec![0u8; (to * to) as usize];
    for y in 0..to {
        for x in 0..to {
            out[(y * to + x) as usize] = mask.rgba[f + ((y * from / to) * from + x * from / to) as usize];
        }
    }
    out
}

/// Frame sequence of an animated texture: (frame index, ticks shown).
#[derive(Clone, Debug, PartialEq)]
pub struct Anim {
    pub frames: Vec<(u32, u32)>,
    pub interpolate: bool,
}

/// `<texture>.anim` next to the png, same `key = value` style as other data files:
///
/// ```text
/// frametime = 2          # ticks per frame (default 1)
/// interpolate = true     # blend towards the next frame
/// frames = 0 1 2 3*5 2 1 # order; `3*5` shows frame 3 for 5 ticks
/// ```
pub fn parse_anim(src: &str, frame_count: u32, first: u32) -> Anim {
    let mut frametime = 1u32;
    let mut interpolate = false;
    let mut order: Option<Vec<(u32, Option<u32>)>> = None;
    for (_, k, v) in crate::kv::pairs(src) {
        match k {
            "frametime" => frametime = v.trim().parse().unwrap_or(1).max(1),
            "interpolate" => interpolate = v.trim() == "true",
            "frames" => {
                order = Some(
                    v.split_whitespace()
                        .filter_map(|t| match t.split_once('*') {
                            Some((i, d)) => Some((i.parse().ok()?, d.parse().ok())),
                            None => Some((t.parse().ok()?, None)),
                        })
                        .filter(|(i, _)| *i < frame_count)
                        .collect(),
                )
            }
            _ => {}
        }
    }
    let order = order.filter(|o| !o.is_empty()).unwrap_or_else(|| (0..frame_count).map(|i| (i, None)).collect());
    Anim { frames: order.into_iter().map(|(i, d)| (first + i, d.unwrap_or(frametime).max(1))).collect(), interpolate }
}

/// A texture by name: animated ones are vertical strips of square frames (`.anim` next to it
/// says how they play). A missing or malformed file gives the magenta checker and a warning.
pub fn load_texture(assets: &Assets, name: &str) -> LoadedTexture {
    let img = (name != "core:missing").then(|| assets.read(name, "textures", "png").and_then(|b| decode_png(&b))).flatten();
    let img = match img {
        Some(img) if img.height >= img.width && img.width > 0 && img.height % img.width == 0 => img,
        Some(img) => {
            log::warn!("texture {name} is {}x{}, not square or a vertical strip", img.width, img.height);
            return missing_texture();
        }
        None => {
            if name != "core:missing" {
                log::warn!("missing texture {name}");
            }
            return missing_texture();
        }
    };
    let size = img.width.next_power_of_two();
    let count = img.height / img.width;
    let frame_bytes = (img.width * img.width * 4) as usize;
    let frames: Vec<Vec<Vec<u8>>> = (0..count as usize).map(|f| mip_chain(scale_nearest(&img.rgba[f * frame_bytes..(f + 1) * frame_bytes], img.width, size), size)).collect();
    let mask = emission_mask(assets, name).filter(|m| m.width > 0);
    let glow = (0..count).map(|f| mask.as_ref().map(|m| mask_frame(m, f, size))).collect();
    let meta = assets.read(name, "textures", "anim").map(|b| String::from_utf8_lossy(&b).into_owned());
    let anim = (count > 1 || meta.is_some()).then(|| parse_anim(meta.as_deref().unwrap_or(""), count, 0));
    LoadedTexture { size, frames, glow, anim }
}

pub fn missing_texture() -> LoadedTexture {
    LoadedTexture { size: 16, frames: vec![mip_chain(missing(16), 16)], glow: vec![None], anim: None }
}

/// A texture made of the top-left, top-right, bottom-left and bottom-right quarters of the first
/// frames of four others (connected textures' compact form), at the largest of their sizes.
pub fn composite_texture(parts: [&LoadedTexture; 4]) -> LoadedTexture {
    let size = parts.iter().map(|p| p.size).max().unwrap_or(16);
    let half = (size / 2) as usize;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for (q, p) in parts.iter().enumerate() {
        let from = scale_nearest(&p.frames[0][0], p.size, size);
        let (qx, qy) = ((q % 2) * half, (q / 2) * half);
        for y in qy..qy + half {
            let row = (y * size as usize + qx) * 4;
            out[row..row + half * 4].copy_from_slice(&from[row..row + half * 4]);
        }
    }
    LoadedTexture { size, frames: vec![mip_chain(out, size)], glow: vec![None], anim: None }
}

/// Fingerprint of the files a texture is made of (image, `.anim`, glow masks): changes when any of
/// them is edited, added or removed, so a reload reads only what changed.
pub fn texture_stamp(assets: &Assets, name: &str) -> u64 {
    let mut h = loom_noise::FNV;
    for (n, ext) in [(name.to_string(), "png"), (name.to_string(), "anim"), (format!("{name}_e"), "png"), (format!("{name}_s"), "png")] {
        let meta = assets.path(&n, "textures", ext).and_then(|p| std::fs::metadata(p).ok());
        let (len, time) = meta.map_or((u64::MAX, 0), |m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64)));
        for v in [len, time] {
            h = loom_noise::fnv_step(h, v);
        }
    }
    h
}

/// Full size first, halved down to 1x1. Transparent texels take the color of the opaque ones
/// next to them first: filtering at cutout edges (tall grass, leaves, overlays) then blends
/// towards the right color instead of black, which drew dark seams.
fn mip_chain(mut rgba: Vec<u8>, size: u32) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(size.trailing_zeros() as usize + 1);
    let mut s = size;
    loop {
        bleed(&mut rgba, s);
        let next = (s > 1).then(|| downsample(&rgba, s));
        out.push(rgba);
        match next {
            Some(n) => (rgba, s) = (n, s / 2),
            None => return out,
        }
    }
}

/// Gives fully transparent texels the average color of their non-transparent neighbors, a few
/// rings deep (alpha stays 0).
fn bleed(rgba: &mut [u8], size: u32) {
    let n = size as usize;
    if !rgba.chunks_exact(4).any(|p| p[3] == 0) || !rgba.chunks_exact(4).any(|p| p[3] != 0) {
        return;
    }
    let mut known: Vec<bool> = rgba.chunks_exact(4).map(|p| p[3] != 0).collect();
    for _ in 0..4 {
        let mut filled = Vec::new();
        for y in 0..n {
            for x in 0..n {
                if known[y * n + x] {
                    continue;
                }
                let (mut sum, mut k) = ([0u32; 3], 0);
                for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1), (-1, -1), (1, -1), (-1, 1), (1, 1)] {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= n as i32 || ny >= n as i32 || !known[ny as usize * n + nx as usize] {
                        continue;
                    }
                    let i = (ny as usize * n + nx as usize) * 4;
                    for c in 0..3 {
                        sum[c] += rgba[i + c] as u32;
                    }
                    k += 1;
                }
                if k > 0 {
                    filled.push((y * n + x, sum.map(|v| (v / k) as u8)));
                }
            }
        }
        if filled.is_empty() {
            break;
        }
        for (i, c) in filled {
            rgba[i * 4..i * 4 + 3].copy_from_slice(&c);
            known[i] = true;
        }
    }
}

fn scale_nearest(src: &[u8], from: u32, to: u32) -> Vec<u8> {
    if from == to {
        return src.to_vec();
    }
    let mut out = vec![0u8; (to * to * 4) as usize];
    for y in 0..to {
        for x in 0..to {
            let sx = x * from / to;
            let sy = y * from / to;
            let s = ((sy * from + sx) * 4) as usize;
            let d = ((y * to + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

/// Alpha-weighted 2x2 box filter, so transparent texels do not darken cutout edges.
fn downsample(src: &[u8], size: u32) -> Vec<u8> {
    let half = size / 2;
    let mut out = vec![0u8; (half * half * 4) as usize];
    for y in 0..half {
        for x in 0..half {
            let mut rgb = [0u32; 3];
            let mut a_sum = 0u32;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let i = (((y * 2 + dy) * size + x * 2 + dx) * 4) as usize;
                let a = src[i + 3] as u32;
                for c in 0..3 {
                    rgb[c] += src[i + c] as u32 * a;
                }
                a_sum += a;
            }
            let d = ((y * half + x) * 4) as usize;
            for c in 0..3 {
                out[d + c] = if a_sum > 0 { (rgb[c] / a_sum) as u8 } else { 0 };
            }
            // Plain average: with vanilla's alpha test of 0.1 a cutout texel stays while a tenth of
            // what it covers is opaque, so leaves get denser with distance like vanilla's.
            out[d + 3] = (a_sum / 4) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anim_order_and_durations() {
        let a = parse_anim("frametime = 2\ninterpolate = true\nframes = 0 2*5 1 9", 3, 10);
        assert!(a.interpolate);
        // Frame 9 does not exist and is dropped.
        assert_eq!(a.frames, vec![(10, 2), (12, 5), (11, 2)]);
        let d = parse_anim("", 4, 0);
        assert_eq!(d.frames.len(), 4);
        assert!(d.frames.iter().all(|f| f.1 == 1));
    }
}

static CURRENT: std::sync::RwLock<Option<std::sync::Arc<Assets>>> = std::sync::RwLock::new(None);
static SIZES: std::sync::Mutex<Option<FxHashMap<String, Option<(u32, u32)>>>> = std::sync::Mutex::new(None);

/// The assets the game uses now, for code that has no handle to them (inline images in text).
pub fn set_current(a: &Assets) {
    *CURRENT.write().unwrap() = Some(std::sync::Arc::new(a.clone()));
    *SIZES.lock().unwrap() = None;
}

/// Pixel size of a texture (`base:gui/arrow`), read once from its PNG header.
pub fn texture_size(name: &str) -> Option<(u32, u32)> {
    let mut map = SIZES.lock().unwrap();
    let map = map.get_or_insert_with(Default::default);
    if let Some(s) = map.get(name) {
        return *s;
    }
    let assets = CURRENT.read().unwrap().clone();
    let size = assets.and_then(|a| a.read(name, "textures", "png")).and_then(|b| png_size(&b));
    map.insert(name.to_string(), size);
    size
}

fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
    (b.starts_with(b"\x89PNG") && w > 0 && h > 0).then_some((w, h))
}
