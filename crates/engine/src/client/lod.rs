//! Distant terrain beyond the view: made on the GPU straight from the dimension's terrain at a
//! coarser step (`shaders/terrain.wgsl`'s `lod`), never from cubes, so it reaches as far as wanted
//! for a few kilobytes a tile. A tile is 32x32 cells of 2^level blocks; each cell is the column
//! at its middle: its top block, the one under it, its biome, and tree canopies over it (no
//! caves or edits).
//! Tiles are picked by distance, nearer ones split into the four of the level below; one is
//! shown once made, its parent or children standing in until then. Where a column of real cubes
//! shows its ground the tiles' pixels are dropped (`fs_lod`, `Lod::mask`), so they fill in for
//! cubes not made or meshed yet (flying fast), and nowhere stand through them.

use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use glam::{DVec3, IVec2};
use rustc_hash::FxHashSet;

use crate::content::{Content, content};
use crate::gpu_layout::face;

/// Cells a tile side.
pub const CELLS: i32 = 32;
/// Columns a tile is made from: its cells and one around them (for the walls at its edges).
pub const SAMPLES: usize = crate::gpu_layout::terrain::lod::SAMPLES as usize;
const SIDE: i32 = crate::gpu_layout::terrain::lod::SIDE as i32;
/// Words per tile coming back from the GPU.
pub const LOD_WORDS_PER_TILE: usize = SAMPLES * 2;
pub const MAX_LEVEL: u32 = 5;
/// A tile splits into the level below while nearer than this many of its widths: a cell stays
/// about the same size on screen (a 32-block cell at 4096 blocks).
const SPLIT: f64 = 4.0;
/// Tiles asked for and not back yet, at most.
const IN_FLIGHT: usize = 256;

/// A tile: its level and place in the grid of its level's tiles.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TileKey {
    pub level: u32,
    pub x: i32,
    pub z: i32,
}

impl TileKey {
    /// Width in blocks.
    pub fn size(self) -> i32 {
        CELLS << self.level
    }

    /// Lowest block corner (x, z).
    pub fn origin(self) -> IVec2 {
        IVec2::new(self.x, self.z) * self.size()
    }

    fn parent(self) -> TileKey {
        TileKey { level: self.level + 1, x: self.x.div_euclid(2), z: self.z.div_euclid(2) }
    }

    fn children(self) -> [TileKey; 4] {
        std::array::from_fn(|i| TileKey { level: self.level - 1, x: self.x * 2 + (i as i32 & 1), z: self.z * 2 + (i as i32 >> 1) })
    }

    /// Nearest and farthest horizontal distance from `p` (x, z) to it.
    fn distance(self, p: glam::DVec2) -> (f64, f64) {
        let lo = self.origin().as_dvec2();
        let hi = lo + glam::DVec2::splat(self.size() as f64);
        let near = (lo - p).max(p - hi).max(glam::DVec2::ZERO).length();
        let far = (p - lo).abs().max((p - hi).abs()).length();
        (near, far)
    }
}

/// A tile for the GPU to make, with the heights of mods' own features at its columns.
pub struct LodJob {
    pub dim: u32,
    pub seed: u64,
    pub key: TileKey,
    pub custom: Vec<i32>,
    pub done: Sender<LodMesh>,
}

/// A made tile's quads (`mesh`) and the heights they span.
pub struct LodMesh {
    pub dim: u32,
    pub seed: u64,
    pub key: TileKey,
    pub quads: Vec<[u32; 4]>,
    pub y: (i32, i32),
}

/// What came back for `job`, meshed on a worker.
pub fn made(job: LodJob, words: Vec<u32>) {
    crate::background().spawn(move || {
        let (quads, y) = mesh(content(), job.key, &words);
        let _ = job.done.send(LodMesh { dim: job.dim, seed: job.seed, key: job.key, quads, y });
    });
}

/// A record of `world.wgsl`'s `vs_lod`: the `dir` face of a box at (x, y, z) in the tile, size
/// (sx, sy, sz) blocks, drawn in the average color of texture `tex` (its smallest mip), tinted
/// for `biome` when `tint`.
fn quad(dir: u32, x: i32, y: i32, z: i32, size: (i32, i32, i32), tex: u32, tint: u32, biome: u32) -> [u32; 4] {
    crate::gpu_layout::lod_record(dir, [x, y, z], [size.0, size.1, size.2], tex, tint, biome)
}

/// The tile's top faces (runs along x merged) and the walls down to lower neighbors: the top
/// block's top color for its own height (a grass side averages to the dirt under its rim, and
/// every slope came out striped brown), then the block under it for the rest.
pub fn mesh(c: &Content, key: TileKey, words: &[u32]) -> (Vec<[u32; 4]>, (i32, i32)) {
    let s = 1 << key.level;
    let at = |i: i32, j: i32| {
        let k = ((j + 1) * SIDE + i + 1) as usize * 2;
        let (a, b) = (words[k], words[k + 1]);
        ((a & 0xFFFF) as u16 as i16 as i32, a >> 16, b & 0xFFFF, b >> 16)
    };
    let face_of = |state: u32, dir: usize| {
        let info = &c.states[state as usize];
        let tint = if info.tint_faces & (1 << dir) != 0 { info.tint as u32 } else { 0 };
        (info.textures[dir] & face::TEX_MASK, tint)
    };
    let mut out = Vec::new();
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    for j in 0..CELLS {
        let mut i = 0;
        while i < CELLS {
            let (y, top, biome, _) = at(i, j);
            let mut end = i + 1;
            while end < CELLS && at(end, j).0 == y && at(end, j).1 == top && at(end, j).2 == biome {
                end += 1;
            }
            let (tex, tint) = face_of(top, 3);
            out.push(quad(3, i * s, y, j * s, ((end - i) * s, 1, s), tex, tint, biome));
            (lo, hi) = (lo.min(y), hi.max(y + 1));
            i = end;
        }
    }
    for j in 0..CELLS {
        for i in 0..CELLS {
            let (y, top, biome, under) = at(i, j);
            for (dir, (di, dj)) in [(0usize, (-1, 0)), (1, (1, 0)), (4, (0, -1)), (5, (0, 1))] {
                let mut ny = at(i + di, j + dj).0;
                // At the tile's edge the tile beside may be of another level, its ground not
                // where this one's sample says: the wall goes on down below it (a skirt), or
                // the sky shows through the crack.
                if !(0..CELLS).contains(&(i + di)) || !(0..CELLS).contains(&(j + dj)) {
                    ny = ny.min(y - 1) - 2 * s;
                }
                if ny >= y {
                    continue;
                }
                let (tex, tint) = face_of(top, 3);
                out.push(quad(dir as u32, i * s, y, j * s, (s, 1, s), tex, tint, biome));
                if y - 1 > ny {
                    let (tex, tint) = face_of(under, dir);
                    out.push(quad(dir as u32, i * s, ny + 1, j * s, (s, y - 1 - ny, s), tex, tint, biome));
                }
                lo = lo.min(ny + 1);
            }
        }
    }
    (out, (lo, hi))
}

/// The client's distant terrain: which tiles to show, asking for missing ones, taking made ones.
pub struct Lod {
    jobs: Sender<LodJob>,
    done: (Sender<LodMesh>, Receiver<LodMesh>),
    dim: u32,
    seed: u64,
    /// Indices of the dimension's features whose heights run mods' code (`terrain_gpu`);
    /// `None`: no distant terrain here.
    custom: Option<Arc<Vec<usize>>>,
    ready: FxHashSet<TileKey>,
    pending: FxHashSet<TileKey>,
    /// Tiles wanted, nearest first, and where the camera was then.
    wanted: Vec<TileKey>,
    wanted_at: Option<(glam::DVec2, i32, i32)>,
    /// A bit per cube column around the camera's (`MASK_MARGIN` past the view, addressed
    /// modulo the window's width so it does not move with the camera): its cubes show its
    /// ground. Its half width and center, and whether it changed since uploaded.
    mask: Vec<u32>,
    half: i32,
    center: IVec2,
    mask_changed: bool,
}

/// Columns the mask reaches past the view radius (cubes are kept that far).
const MASK_MARGIN: i32 = 3;

impl Lod {
    pub fn new(jobs: Sender<LodJob>) -> Lod {
        Lod {
            jobs,
            done: unbounded(),
            dim: u32::MAX,
            seed: 0,
            custom: None,
            ready: FxHashSet::default(),
            pending: FxHashSet::default(),
            wanted: Vec::new(),
            wanted_at: None,
            mask: Vec::new(),
            half: 0,
            center: IVec2::ZERO,
            mask_changed: false,
        }
    }

    /// Tiles asked for and not back yet.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Distance in blocks it reaches (`core:lod_distance` cubes), 0 when off.
    pub fn distance() -> i32 {
        let c = content();
        c.game.config_names.get("core:lod_distance").map_or(0, |&id| crate::config::get(id) as i32) * 16
    }

    /// Picks, asks for and hands the renderer this frame's tiles. `view`: the view radius in
    /// cubes (inside it the real cubes are drawn).
    pub fn update(&mut self, gpu: &mut dyn loom_render::Backend, render: &mut super::world_render::WorldRenderer, world: &mut super::world::ClientWorld, textures: &super::textures::GameTextures, seed: u64, cam: DVec3, view: i32) {
        let c = content();
        let dim = world.dim;
        self.update_mask(gpu, render, world, cam, view);
        let reach = Self::distance();
        if (dim, seed) != (self.dim, self.seed) || reach == 0 {
            for key in self.ready.drain() {
                render.remove_lod(key);
            }
            self.pending.clear();
            self.wanted.clear();
            self.wanted_at = None;
        }
        if (dim, seed) != (self.dim, self.seed) {
            (self.dim, self.seed) = (dim, seed);
            self.custom = super::terrain_gpu::describe(c, dim, seed).map(|(_, custom)| Arc::new(custom));
        }
        // Tiles stand in for cubes not shown yet; the inner half of the view is shown long
        // before the edge, where flying outruns the cubes.
        let hide = (view / 2 * 16).max(0);
        while let Ok(m) = self.done.1.try_recv() {
            if (m.dim, m.seed) != (self.dim, self.seed) || !self.pending.remove(&m.key) {
                continue;
            }
            render.upload_lod(gpu, textures, m.key, &m.quads, m.y);
            self.ready.insert(m.key);
        }
        if reach == 0 || self.custom.is_none() {
            render.lod_draw.clear();
            return;
        }
        let p = glam::DVec2::new(cam.x, cam.z);
        if self.wanted_at.is_none_or(|(at, h, r)| (at - p).length() > 16.0 || h != hide || r != reach) {
            self.wanted_at = Some((p, hide, reach));
            self.pick(p, hide as f64, reach as f64);
        }
        // Shown: each wanted tile when made, else its children when all four are, else its
        // nearest made ancestor.
        let mut shown = FxHashSet::default();
        let mut keep = FxHashSet::default();
        for &key in &self.wanted {
            keep.insert(key);
            if self.ready.contains(&key) {
                shown.insert(key);
                continue;
            }
            if key.level > 1 && key.children().iter().all(|k| self.ready.contains(k)) {
                shown.extend(key.children());
                keep.extend(key.children());
                continue;
            }
            let mut a = key;
            while a.level < MAX_LEVEL {
                a = a.parent();
                if self.ready.contains(&a) {
                    shown.insert(a);
                    keep.insert(a);
                    break;
                }
            }
        }
        render.lod_draw.clear();
        render.lod_draw.extend(shown);
        let gone: Vec<TileKey> = self.ready.iter().copied().filter(|k| !keep.contains(k)).collect();
        for key in gone {
            self.ready.remove(&key);
            render.remove_lod(key);
        }
        self.request();
    }

    /// Keeps the mask of columns shown by cubes current: columns that changed, and the ones
    /// coming into the window when the camera moved to another column.
    fn update_mask(&mut self, gpu: &mut dyn loom_render::Backend, render: &mut super::world_render::WorldRenderer, world: &mut super::world::ClientWorld, cam: DVec3, view: i32) {
        let half = view + MASK_MARGIN;
        let w = 2 * half + 1;
        let center = IVec2::new((cam.x / 16.0).floor() as i32, (cam.z / 16.0).floor() as i32);
        let slot = |col: IVec2| (col.y.rem_euclid(w) * w + col.x.rem_euclid(w)) as usize;
        let set = |mask: &mut Vec<u32>, col: IVec2, on: bool| {
            let i = slot(col);
            let (word, bit) = (i / 32, 1u32 << (i % 32));
            let old = mask[word];
            mask[word] = if on { old | bit } else { old & !bit };
            old != mask[word]
        };
        let changed = world.take_changed_columns();
        let inside = |col: IVec2, at: IVec2| (col - at).abs().max_element() <= half;
        if half != self.half || self.mask.is_empty() {
            self.half = half;
            self.mask = vec![0; ((w * w) as usize).div_ceil(32)];
            self.center = center;
            for z in -half..=half {
                for x in -half..=half {
                    let col = center + IVec2::new(x, z);
                    set(&mut self.mask, col, world.column_shown(col));
                }
            }
            self.mask_changed = true;
        } else if center != self.center {
            let old = self.center;
            self.center = center;
            for z in -half..=half {
                for x in -half..=half {
                    let col = center + IVec2::new(x, z);
                    if !inside(col, old) {
                        self.mask_changed |= set(&mut self.mask, col, world.column_shown(col));
                    }
                }
            }
        }
        for col in changed {
            if inside(col, center) {
                self.mask_changed |= set(&mut self.mask, col, world.column_shown(col));
            }
        }
        render.lod_half = half as u32;
        if self.mask_changed {
            self.mask_changed = false;
            render.set_lod_mask(gpu, &self.mask);
        }
    }

    /// The tiles to show: level 5 tiles within `reach`, split while near, leaving out what
    /// lies inside `hide` (where the view's cubes are always shown).
    fn pick(&mut self, p: glam::DVec2, hide: f64, reach: f64) {
        let top = CELLS << MAX_LEVEL;
        let (lo, hi) = (((p - reach) / top as f64).floor().as_ivec2(), ((p + reach) / top as f64).floor().as_ivec2());
        let mut out = Vec::new();
        let mut stack: Vec<TileKey> = Vec::new();
        for z in lo.y..=hi.y {
            for x in lo.x..=hi.x {
                stack.push(TileKey { level: MAX_LEVEL, x, z });
            }
        }
        while let Some(key) = stack.pop() {
            let (near, far) = key.distance(p);
            if near > reach || far < hide {
                continue;
            }
            if key.level > 1 && near < SPLIT * key.size() as f64 {
                stack.extend(key.children());
            } else {
                out.push((near, key));
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.wanted = out.into_iter().map(|t| t.1).collect();
    }

    /// Asks for the nearest wanted tiles not made yet.
    fn request(&mut self) {
        let c = content();
        let custom = self.custom.clone().unwrap();
        for &key in &self.wanted {
            if self.pending.len() >= IN_FLIGHT {
                break;
            }
            if self.ready.contains(&key) || self.pending.contains(&key) {
                continue;
            }
            self.pending.insert(key);
            let (dim, seed, done, jobs) = (self.dim, self.seed, self.done.0.clone(), self.jobs.clone());
            if custom.is_empty() {
                let _ = jobs.send(LodJob { dim, seed, key, custom: Vec::new(), done });
                continue;
            }
            let custom = custom.clone();
            // Mods' features run their code: on a worker.
            crate::background().spawn(move || {
                let heights = custom_heights(c, dim, seed, key, &custom);
                let _ = jobs.send(LodJob { dim, seed, key, custom: heights, done });
            });
        }
    }
}

/// Heights of the mod features `custom` at the tile's columns, for its job.
fn custom_heights(c: &Content, dim: u32, seed: u64, key: TileKey, custom: &[usize]) -> Vec<i32> {
    let Some(t) = c.dims[dim as usize].terrain.as_ref() else { return Vec::new() };
    let f = loom_noise::fixed::Fields { seed };
    let (o, s) = (key.origin(), 1 << key.level);
    let mut out = Vec::with_capacity(custom.len() * SAMPLES);
    for &i in custom {
        for k in 0..SAMPLES as i32 {
            let (x, z) = (o.x + (k % SIDE - 1) * s + s / 2, o.y + (k / SIDE - 1) * s + s / 2);
            out.push(t.features[i].height_at(&f, x * 16, z * 16));
        }
    }
    out
}
