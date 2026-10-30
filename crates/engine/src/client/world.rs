use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use glam::{IVec2, IVec3};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use super::mesher::{self, Mesh, Neighborhood, PAD_VOLUME, pidx};
use crate::content::{AIR, content};
use crate::world::cube::{ALL_AROUND, around_mask};
use crate::world::{Cube, cube_of, local_index};

pub struct MeshResult {
    pub pos: IVec3,
    pub version: u32,
    pub mesh: Mesh,
}

/// What the client keeps per cube, in one record: a lookup found it all (it was five maps, each
/// a cache miss among a million and a half cubes).
struct Slot {
    cube: Arc<Cube>,
    /// Of its newest mesh job (`next_version`).
    version: u32,
    /// The cubes around it that are never coming (`cube::around_mask` bits): faces toward them
    /// are meshed as solid, and it is meshed once all 26 around it are here or never coming
    /// (meshing it before its edge and corner neighbors came meshed it again for each of them).
    hidden: u32,
    /// Its mesh is out of date.
    dirty: bool,
    /// A mesh of it has been drawn (or it is air): its column shows it (`column_shown`).
    shown: bool,
    /// Changes when its blocks do (not its light), from `next_rev`.
    rev: u32,
}

pub struct ClientWorld {
    pub dim: u32,
    cubes: FxHashMap<IVec3, Slot>,
    pub time: u64,
    /// Dirty cubes whose six neighbors are all known: meshable now. Kept up to date as cubes
    /// come and go, so a frame never walks every dirty cube (tens of thousands while loading).
    ready: FxHashSet<IVec3>,
    /// `ready` sorted farthest first around `order_center` (taken from the end), sorted again
    /// only when the player moved or `ready` grew by a quarter since: a frame no longer walks
    /// hundreds of thousands of ready cubes at a view of 64.
    order: Vec<IVec3>,
    order_center: IVec3,
    order_size: usize,
    in_flight: FxHashSet<IVec3>,
    tx: Sender<MeshResult>,
    rx: Receiver<MeshResult>,
    pub removed: Vec<IVec3>,
    next_version: u32,
    next_rev: u32,
    /// An opaque state standing in for hidden neighbors.
    filler: u32,
    columns: FxHashMap<IVec2, Column>,
    /// Columns whose blended biome colors are out of date.
    tints_dirty: FxHashSet<IVec2>,
    /// A moving structure's blocks: cubes that are not there are air (the server sends all of
    /// them), and the block bounds (inclusive), for collisions.
    pub bounded: bool,
    pub bounds: Option<(IVec3, IVec3)>,
    /// Updates that came together, shown together once all their cubes are meshed: new meshes
    /// next to old ones open holes for a few frames otherwise (a crater showing the caves
    /// under it before its walls, a block at a cube's edge).
    batches: Vec<Batch>,
    /// The batch being collected (`begin_batch` .. `end_batch`).
    open: Option<Batch>,
    /// Heights the view spans (`terrain::Out::Range`), and columns whose cubes changed what
    /// is shown since `take_changed_columns`.
    range: (i32, i32),
    /// The camera's cube height: cubes above it still on their way do not hide a column's ground
    /// (`column_shown`), or rising made loaded ground flicker to distant terrain.
    eye: i32,
    changed_columns: FxHashSet<IVec2>,
}

struct Batch {
    /// Cubes whose new mesh it still waits for.
    members: FxHashSet<IVec3>,
    /// While open: cubes sent again and what they were, compared at the end on every core.
    resent: Vec<(IVec3, Arc<Cube>)>,
    results: Vec<MeshResult>,
    since: Instant,
    /// Some cube it touched was there before (only loading new cubes is not held).
    update: bool,
    /// Cubes it brought that were not there before.
    fresh: Vec<IVec3>,
    /// While open: cubes touched and the ones around them to remesh (`cube::around_mask`),
    /// made into `members` at the end, on every core.
    touched: Vec<(IVec3, u32)>,
    /// Held until all of it is meshed when it changes cubes shown (`begin_batch`).
    whole: bool,
}

/// A batch waits no longer than this for its slowest cube.
const HOLD_MAX: Duration = Duration::from_secs(5);

/// Ready cubes this close to the player (in cubes) are meshed first and past the limit on jobs:
/// what the player just placed or broke shows the next frame, whatever else waits.
const URGENT: i32 = 2;

impl crate::physics::Blocks for ClientWorld {
    fn block(&self, p: IVec3) -> u32 {
        match self.block(p) {
            u32::MAX => AIR,
            s => s,
        }
    }
}

/// Biomes of a block column, taken from the first of its cubes that has them; cubes store none
/// when every biome is 0.
struct Column {
    biomes: Option<Box<[u16; 256]>>,
    cubes: u32,
}

/// Biome colors are averaged over (2 BLEND + 1)^2 blocks.
const BLEND: i32 = 4;
/// About 9 us each, blended on every core; while loading, columns get redone as neighbors
/// arrive.
const TINTS_PER_FRAME: usize = 256;

/// Grass, foliage and water colors of one block column, 0xFFRRGGBB.
/// Blended colors of a column: per block (z, x), one per tint kind.
pub type ColumnTints = [u32];

impl ClientWorld {
    pub fn new(dim: u32) -> Self {
        let (tx, rx) = unbounded();
        ClientWorld {
            dim,
            cubes: FxHashMap::default(),
            time: 0,
            ready: FxHashSet::default(),
            order: Vec::new(),
            order_center: IVec3::MAX,
            order_size: 0,
            in_flight: FxHashSet::default(),
            tx,
            rx,
            removed: Vec::new(),
            next_version: 0,
            next_rev: 0,
            filler: content().states.iter().position(|s| s.flags & loom_abi::block_flags::OPAQUE != 0).unwrap_or(0) as u32,
            columns: FxHashMap::default(),
            tints_dirty: FxHashSet::default(),
            bounded: false,
            bounds: None,
            batches: Vec::new(),
            open: None,
            range: (0, -1),
            eye: 0,
            changed_columns: FxHashSet::default(),
        }
    }

    /// A moving structure's blocks (in its own coordinates).
    pub fn structure(dim: u32) -> Self {
        ClientWorld { bounded: true, ..ClientWorld::new(dim) }
    }

    /// Recomputes `bounds` from the blocks (structures, after their cubes changed).
    pub fn scan_bounds(&mut self) {
        self.bounds = crate::world::block_bounds(self.cubes.iter().map(|(p, s)| (p, &*s.cube)));
    }

    /// Heights the view spans now.
    pub fn set_range(&mut self, lo: i32, hi: i32) {
        if (lo, hi) != self.range {
            self.range = (lo, hi);
            self.changed_columns.extend(self.columns.keys().copied());
        }
    }

    pub fn set_eye(&mut self, y: i32) {
        if y != self.eye {
            self.eye = y;
            self.changed_columns.extend(self.columns.keys().copied());
        }
    }

    /// A mesh of `pos` was drawn.
    pub fn mark_shown(&mut self, pos: IVec3) {
        if let Some(s) = self.cubes.get_mut(&pos).filter(|s| !s.shown) {
            s.shown = true;
            self.changed_columns.insert(IVec2::new(pos.x, pos.z));
        }
    }

    /// Whether column `col` shows its ground: from the top of the view down, every cube is here
    /// and drawn, down to the first that is not air (distant terrain stands in until then).
    /// Above the camera a cube not here yet is taken for air: those are the new top of the view
    /// while rising, nearly always sky.
    pub fn column_shown(&self, col: IVec2) -> bool {
        shows_ground(self.range, self.eye, |y| self.cubes.get(&IVec3::new(col.x, y, col.y)).map(|s| (s.shown, s.cube.uniform_state() == Some(AIR))))
    }

    /// Columns whose `column_shown` may have changed since the last call.
    pub fn take_changed_columns(&mut self) -> Vec<IVec2> {
        self.changed_columns.drain().collect()
    }

    pub fn len(&self) -> usize {
        self.cubes.len()
    }

    pub fn cube(&self, p: IVec3) -> Option<&Arc<Cube>> {
        self.cubes.get(&p).map(|s| &s.cube)
    }

    /// With a number that changes whenever its blocks do (`Slot::rev`).
    pub fn cube_rev(&self, p: IVec3) -> Option<(&Arc<Cube>, u32)> {
        self.cubes.get(&p).map(|s| (&s.cube, s.rev))
    }

    /// Whether every cube around `p` is present or never coming.
    fn neighbors_known(&self, p: IVec3) -> bool {
        if self.bounded {
            return true;
        }
        let def = &content().dims[self.dim as usize];
        let hidden = self.cubes.get(&p).map_or(0, |s| s.hidden);
        (0..27).all(|i| {
            let n = p + crate::world::cube::around(i);
            i == 13 || hidden & (1 << i) != 0 || n.y < def.min_cube_y || n.y > def.max_cube_y || self.cubes.contains_key(&n)
        })
    }

    /// Updates whether `p` is meshable now.
    fn refresh(&mut self, p: IVec3) {
        let known = self.cubes.get(&p).is_some_and(|s| s.dirty) && self.neighbors_known(p);
        self.set_ready(p, known);
    }

    /// `p` is dirty and all around it is known (`ready`), or not.
    fn set_ready(&mut self, p: IVec3, ready: bool) {
        if ready {
            // Air cubes never produce faces themselves (neighbors draw the faces facing them):
            // done right away, without a job (most of a tall view is sky).
            if let Some(s) = self.cubes.get_mut(&p).filter(|s| s.cube.uniform_state() == Some(AIR)) {
                s.dirty = false;
                let version = s.version;
                self.ready.remove(&p);
                let _ = self.tx.send(MeshResult { pos: p, version, mesh: Mesh::default() });
                return;
            }
            self.ready.insert(p);
        } else {
            self.ready.remove(&p);
        }
    }

    pub fn clear(&mut self) {
        self.removed.extend(self.cubes.keys().copied());
        self.cubes.clear();
        self.ready.clear();
        self.changed_columns.clear();
        self.columns.clear();
        self.tints_dirty.clear();
        self.batches.clear();
    }

    /// Starts collecting what arrives into one batch; `whole`: shown once all of it is meshed
    /// (edits), else as it comes (loading, light reaching cubes shown already).
    pub fn begin_batch(&mut self, whole: bool) {
        self.open = Some(Batch { members: FxHashSet::default(), resent: Vec::new(), results: Vec::new(), since: Instant::now(), update: false, fresh: Vec::new(), touched: Vec::new(), whole });
    }

    pub fn end_batch(&mut self) {
        let Some(mut b) = self.open.take() else { return };
        // A resend remeshes only what reads a changed block: light resends after an edit
        // reach far more cubes than the edit does.
        let resent = std::mem::take(&mut b.resent);
        let cubes = &self.cubes;
        let masks: Vec<(IVec3, u32)> = resent.par_iter().filter_map(|(p, old)| cubes.get(p).and_then(|now| now.cube.changed_around(old)).map(|m| (*p, m))).collect();
        b.touched.extend(masks);
        let mut members: Vec<IVec3> = std::mem::take(&mut b.touched).into_par_iter().flat_map_iter(|(p, mask)| around(p, mask)).filter(|p| self.cubes.contains_key(p)).collect();
        members.par_sort_unstable_by_key(|p| p.to_array());
        members.dedup();
        // Each looks at the 26 around it: on every core, a batch while loading has thousands.
        // Air that was not there before had no mesh and never has one: nothing to do (half of
        // what loads is sky).
        let mut fresh = std::mem::take(&mut b.fresh);
        fresh.par_sort_unstable_by_key(|p| p.to_array());
        let cubes = &self.cubes;
        let todo: Vec<(IVec3, Option<bool>)> = members
            .par_iter()
            .map(|&p| {
                let new_air = cubes[&p].cube.uniform_state() == Some(AIR) && fresh.binary_search_by_key(&p.to_array(), |f| f.to_array()).is_ok();
                (p, (!new_air).then(|| self.neighbors_known(p)))
            })
            .collect();
        for (p, known) in todo {
            let Some(known) = known else {
                self.mark_shown(p);
                continue;
            };
            self.bump(p);
            self.set_ready(p, known);
            if b.update && b.whole {
                b.members.insert(p);
            }
        }
        if b.update && b.whole && !b.members.is_empty() {
            self.batches.push(b);
        }
    }

    /// Versions come from one counter so a mesh job of a removed cube can never match a new cube
    /// at the same position (dimension change).
    /// Marks it dirty, for a job of a new version.
    fn bump(&mut self, pos: IVec3) {
        if let Some(s) = self.cubes.get_mut(&pos) {
            self.next_version += 1;
            (s.version, s.dirty) = (self.next_version, true);
        }
    }

    /// Remeshes the cubes of `mask` around `pos` (`cube::around_mask`); in a batch once at its
    /// end (a crater's cubes touch each neighbor up to 27 times).
    fn touch(&mut self, pos: IVec3, mask: u32) {
        if let Some(b) = &mut self.open {
            b.touched.push((pos, mask));
            return;
        }
        for n in around(pos, mask) {
            if self.cubes.contains_key(&n) {
                self.bump(n);
                self.refresh(n);
            }
        }
    }

    /// `hidden`: the cubes around it never coming (`cube::around_mask` bits).
    pub fn insert(&mut self, pos: IVec3, cube: Arc<Cube>, hidden: u32) {
        let col = IVec2::new(pos.x, pos.z);
        let mut fresh = false;
        let column = self.columns.entry(col).or_insert_with(|| {
            fresh = true;
            Column { biomes: None, cubes: 0 }
        });
        if fresh || column.biomes.is_none() && cube.biomes.is_some() {
            column.biomes = column.biomes.take().or_else(|| cube.biomes.clone());
            for dz in -1..=1 {
                for dx in -1..=1 {
                    self.tints_dirty.insert(col + IVec2::new(dx, dz));
                }
            }
        }
        match self.cubes.entry(pos) {
            std::collections::hash_map::Entry::Vacant(e) => {
                self.next_rev += 1;
                e.insert(Slot { cube, version: 0, hidden, dirty: false, shown: false, rev: self.next_rev });
                self.changed_columns.insert(col);
                if let Some(b) = &mut self.open {
                    b.fresh.push(pos);
                }
                self.columns.get_mut(&col).unwrap().cubes += 1;
                self.touch(pos, ALL_AROUND);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let s = e.get_mut();
                let fresh_hidden = std::mem::replace(&mut s.hidden, hidden) != hidden;
                let old = std::mem::replace(&mut s.cube, cube);
                if !s.cube.same_blocks(&old) {
                    self.next_rev += 1;
                    s.rev = self.next_rev;
                }
                if fresh_hidden {
                    self.touch(pos, ALL_AROUND);
                } else if let Some(b) = &mut self.open {
                    b.update = true;
                    b.resent.push((pos, old));
                } else if let Some(mask) = self.cubes[&pos].cube.changed_around(&old) {
                    self.touch(pos, mask);
                }
            }
        }
    }

    pub fn remove(&mut self, pos: IVec3) {
        if self.cubes.remove(&pos).is_some() {
            let col = IVec2::new(pos.x, pos.z);
            if let Some(c) = self.columns.get_mut(&col) {
                c.cubes -= 1;
                if c.cubes == 0 {
                    self.columns.remove(&col);
                }
            }
            self.ready.remove(&pos);
            self.removed.push(pos);
            self.changed_columns.insert(col);
            for n in around(pos, ALL_AROUND) {
                if n != pos && self.ready.contains(&n) {
                    self.refresh(n);
                }
            }
        }
    }

    pub fn set_block(&mut self, p: IVec3, state: u32) {
        let cp = cube_of(p);
        let Some(slot) = self.cubes.get_mut(&cp) else { return };
        Arc::make_mut(&mut slot.cube).set(local_index(p), state);
        self.next_rev += 1;
        slot.rev = self.next_rev;
        if let Some(b) = &mut self.open {
            b.update = true;
        }
        let l = p & 15;
        self.touch(cp, around_mask(l.x, l.y, l.z));
    }

    /// Remeshes every cube, e.g. after textures were reloaded.
    pub fn remesh_all(&mut self) {
        let all: Vec<IVec3> = self.cubes.keys().copied().collect();
        for p in all {
            self.touch(p, 1 << 13);
        }
    }

    pub fn retint_all(&mut self) {
        self.tints_dirty.extend(self.columns.keys().copied());
    }

    /// Recomputes the blended colors of columns whose biomes or neighbors changed. Only columns
    /// at most `reach` from `center` are done: the GPU keeps a window of 2 reach + 1 columns, and
    /// cubes of an area the player just left would overwrite the ones around them.
    pub fn take_tints(&mut self, center: IVec2, reach: i32) -> Vec<(IVec2, Box<ColumnTints>)> {
        let mut cols = Vec::new();
        let dirty: Vec<IVec2> = self.tints_dirty.iter().copied().collect();
        for c in dirty {
            if !self.columns.contains_key(&c) {
                self.tints_dirty.remove(&c);
            } else if (c - center).abs().max_element() <= reach && cols.len() < TINTS_PER_FRAME {
                self.tints_dirty.remove(&c);
                cols.push(c);
            }
        }
        cols.into_par_iter().map(|c| (c, self.blend_column(c))).collect()
    }

    fn blend_column(&self, col: IVec2) -> Box<ColumnTints> {
        const W: usize = 16 + 2 * BLEND as usize;
        const SPAN: usize = 2 * BLEND as usize + 1;
        let c = content();
        let k = c.tint_kinds.len();
        let defaults: Vec<u32> = c.tint_kinds.iter().map(|t| t.1).collect();
        let get = |c: &Column, x: i32, z: i32| c.biomes.as_ref().map_or(0, |b| b[(z * 16 + x) as usize]);
        let own = &self.columns[&col];
        let around: [Option<&Column>; 9] = std::array::from_fn(|i| self.columns.get(&(col + IVec2::new(i as i32 % 3 - 1, i as i32 / 3 - 1))));
        // Blocks of missing neighbor columns repeat this column's edge.
        let biome_at = |x: i32, z: i32| -> u16 {
            match around[((z.div_euclid(16) + 1) * 3 + x.div_euclid(16) + 1) as usize] {
                Some(other) => get(other, x.rem_euclid(16), z.rem_euclid(16)),
                None => get(own, x.clamp(0, 15), z.clamp(0, 15)),
            }
        };
        // Per block, tint and channel, summed along x then along z.
        let mut rows = vec![[0u32; 3]; W * 16 * k];
        let mut line = vec![[0u32; 3]; W * k];
        for gz in 0..W {
            for gx in 0..W {
                let id = biome_at(gx as i32 - BLEND, gz as i32 - BLEND) as usize;
                let tints = c.biomes.get(id).or(c.biomes.first()).map_or(&defaults[..], |b| &b.tints[..]);
                for (t, rgb) in tints.iter().enumerate() {
                    line[gx * k + t] = [rgb >> 16 & 255, rgb >> 8 & 255, rgb & 255];
                }
            }
            for x in 0..16 {
                let out = &mut rows[(gz * 16 + x) * k..][..k];
                for v in line[x * k..(x + SPAN) * k].chunks_exact(k) {
                    for (o, v) in out.iter_mut().zip(v) {
                        for ch in 0..3 {
                            o[ch] += v[ch];
                        }
                    }
                }
            }
        }
        let n = (SPAN * SPAN) as u32;
        let mut out = vec![0u32; 256 * k].into_boxed_slice();
        for z in 0..16 {
            for x in 0..16 {
                for t in 0..k {
                    let mut sum = [0u32; 3];
                    for gz in z..z + SPAN {
                        let v = rows[(gz * 16 + x) * k + t];
                        for ch in 0..3 {
                            sum[ch] += v[ch];
                        }
                    }
                    let [r, g, b] = sum.map(|s| (s + n / 2) / n);
                    out[(z * 16 + x) * k + t] = 0xFF00_0000 | r << 16 | g << 8 | b;
                }
            }
        }
        out
    }

    /// `u32::MAX` when not loaded.
    pub fn block(&self, p: IVec3) -> u32 {
        self.cubes.get(&cube_of(p)).map_or(u32::MAX, |s| s.cube.get(local_index(p)))
    }

    /// The block, `None` while its cube has not arrived; outside the dimension's height there are
    /// no cubes, only air.
    pub fn block_or_air(&self, p: IVec3) -> Option<u32> {
        let s = self.block(p);
        if s != u32::MAX {
            return Some(s);
        }
        let def = &crate::content::content().dims[self.dim as usize];
        let y = p.y >> 4;
        (y < def.min_cube_y || y > def.max_cube_y).then_some(crate::content::AIR)
    }

    pub fn light(&self, p: IVec3) -> u32 {
        self.cubes.get(&cube_of(p)).map_or(15, |s| s.cube.light(local_index(p)))
    }

    /// Starts meshing jobs for dirty cubes nearest to `center` first.
    pub fn schedule_meshing(&mut self, center: IVec3, max_in_flight: usize) {
        if self.ready.is_empty() {
            return;
        }
        let mut start = Vec::new();
        for dz in -URGENT..=URGENT {
            for dy in -URGENT..=URGENT {
                for dx in -URGENT..=URGENT {
                    let p = center + IVec3::new(dx, dy, dz);
                    if self.ready.contains(&p) && !self.in_flight.contains(&p) {
                        start.push(p);
                    }
                }
            }
        }
        self.start_jobs(start, true);
        // Then updates of what is on screen, oldest first: a batch held for a cube at the back
        // of a long queue would hold everything it came with.
        let mut take = max_in_flight.saturating_sub(self.in_flight.len());
        let mut start = FxHashSet::default();
        'batches: for b in &self.batches {
            for p in &b.members {
                if take == 0 {
                    break 'batches;
                }
                if self.ready.contains(p) && !self.in_flight.contains(p) && start.insert(*p) {
                    take -= 1;
                }
            }
        }
        self.start_jobs(start.into_iter().collect(), true);
        if self.in_flight.len() >= max_in_flight {
            return;
        }
        let moved = (center - self.order_center).abs().max_element() > 1;
        if moved || self.order.is_empty() || self.ready.len() > self.order_size + self.order_size / 4 + 64 {
            self.order.clear();
            self.order.extend(self.ready.iter().copied());
            self.order.par_sort_unstable_by_key(|p| std::cmp::Reverse((*p - center).length_squared()));
            (self.order_center, self.order_size) = (center, self.ready.len());
        }
        let mut take = max_in_flight - self.in_flight.len();
        let mut start = Vec::with_capacity(take);
        while take > 0 {
            let Some(pos) = self.order.pop() else { break };
            if self.ready.contains(&pos) && !self.in_flight.contains(&pos) {
                start.push(pos);
                take -= 1;
            }
        }
        self.start_jobs(start, false);
    }

    /// `urgent`: around the player or edits, meshed apart from the loading queue (thousands long).
    fn start_jobs(&mut self, start: Vec<IVec3>, urgent: bool) {
        for pos in &start {
            if let Some(s) = self.cubes.get_mut(pos) {
                s.dirty = false;
            }
            self.ready.remove(pos);
            self.in_flight.insert(*pos);
        }
        // What each job takes from the world, gathered on every core: thousands start a frame
        // while loading.
        let cubes = &self.cubes;
        let jobs: Vec<(IVec3, [Option<Arc<Cube>>; 27], u32, u32)> = start
            .par_iter()
            .map(|&pos| {
                let around = std::array::from_fn(|i| cubes.get(&(pos + IVec3::new(i as i32 % 3 - 1, i as i32 / 9 - 1, i as i32 / 3 % 3 - 1))).map(|s| s.cube.clone()));
                let s = &cubes[&pos];
                (pos, around, s.version, s.hidden)
            })
            .collect();
        for (pos, around, version, hidden) in jobs {
            let tx = self.tx.clone();
            let filler = self.filler;
            let ctm = super::ctm::current();
            let pool = if urgent { crate::meshing_urgent() } else { crate::meshing() };
            pool.spawn(move || {
                let t = std::time::Instant::now();
                let n = neighborhood(&around, hidden, filler, pos);
                let mesh = mesher::build(content(), &n, &ctm);
                crate::diag::MESHED.add(1);
                crate::diag::MESH_NS.add(t.elapsed().as_nanos() as u64);
                let _ = tx.send(MeshResult { pos, version, mesh });
            });
        }
    }

    /// Finished meshes that are still current, those of a batch once it is complete: the ones
    /// to show now (batches of edits, cubes near `center`), and the rest.
    pub fn take_meshes(&mut self, center: IVec3) -> (Vec<MeshResult>, Vec<MeshResult>) {
        let mut now = Vec::new();
        let mut out = Vec::new();
        while let Ok(r) = self.rx.try_recv() {
            self.in_flight.remove(&r.pos);
            if self.cubes.get(&r.pos).map(|s| s.version) != Some(r.version) {
                continue;
            }
            // Into the oldest batch waiting for it; the others stop waiting for it too.
            let mut into = None;
            for (i, b) in self.batches.iter_mut().enumerate() {
                if b.members.remove(&r.pos) && into.is_none() {
                    into = Some(i);
                }
            }
            match into {
                Some(i) => self.batches[i].results.push(r),
                None => out.push(r),
            }
        }
        // Complete once nothing it waits for can still come now (a cube waiting for a neighbor
        // that has not arrived is not waited for).
        let (ready, in_flight, cubes) = (&self.ready, &self.in_flight, &self.cubes);
        self.batches.retain_mut(|b| {
            b.members.retain(|p| ready.contains(p) || in_flight.contains(p));
            if !b.members.is_empty() && b.since.elapsed() < HOLD_MAX {
                return true;
            }
            now.extend(b.results.drain(..).filter(|r| cubes.get(&r.pos).map(|s| s.version) == Some(r.version)));
            false
        });
        let (near, rest): (Vec<MeshResult>, Vec<MeshResult>) = out.into_iter().partition(|r| (r.pos - center).abs().max_element() <= URGENT);
        now.extend(near);
        (now, rest)
    }

    /// Whether a finished mesh is still the newest for its cube.
    pub fn current(&self, r: &MeshResult) -> bool {
        self.cubes.get(&r.pos).is_some_and(|s| s.version == r.version)
    }

    /// Jobs running plus dirty cubes that could be meshed now (edge cubes wait for neighbors
    /// that may never come and are not counted).
    pub fn pending_meshes(&self) -> usize {
        self.ready.len() + self.in_flight.len()
    }
}

/// The cubes of `mask` around `pos` (`cube::around_mask`), loaded or not.
fn around(pos: IVec3, mask: u32) -> impl Iterator<Item = IVec3> {
    (0..27).filter(move |i| mask & (1 << i) != 0).map(move |i| pos + crate::world::cube::around(i))
}

const BORDER_ROW: [i32; 18] = [-1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

fn neighborhood(around: &[Option<Arc<Cube>>; 27], hidden: u32, filler: u32, pos: IVec3) -> Neighborhood {
    let mut states = Box::new([AIR; PAD_VOLUME]);
    let mut light = Box::new([15u32; PAD_VOLUME]);
    let mut biomes = [0u16; 256];
    let center = around[13].as_ref().unwrap();
    for z in 0..16 {
        for x in 0..16 {
            biomes[z * 16 + x] = center.biome(x, z);
        }
    }
    // The cube itself whole, a row at a time; the border from its neighbors block by block.
    let mut own = [0u32; crate::world::cube::VOLUME];
    center.decode(&mut own);
    for y in 0..16 {
        for z in 0..16 {
            let (src, dst) = (crate::world::cube::index(0, y, z), pidx(0, y as i32, z as i32));
            states[dst..dst + 16].copy_from_slice(&own[src..src + 16]);
            match center.light_all() {
                Some(l) => light[dst..dst + 16].copy_from_slice(&l[src..src + 16]),
                None => light[dst..dst + 16].fill(center.light(0)),
            }
        }
    }
    for y in -1..17i32 {
        let cy = if y < 0 { 0 } else if y > 15 { 2 } else { 1 };
        for z in -1..17i32 {
            let cz = if z < 0 { 0 } else if z > 15 { 2 } else { 1 };
            let xs: &[i32] = if cy == 1 && cz == 1 { &[-1, 16] } else { &BORDER_ROW };
            for &x in xs {
                let cx = if x < 0 { 0 } else if x > 15 { 2 } else { 1 };
                let Some(cube) = &around[cy * 9 + cz * 3 + cx] else {
                    // A face neighbor that is never coming is solid ground: no faces toward it.
                    let face = [cx, cy, cz].iter().filter(|&&v| v != 1).count() == 1;
                    if face && hidden & (1 << (cy * 9 + cz * 3 + cx)) != 0 {
                        let pi = pidx(x, y, z);
                        states[pi] = filler;
                        light[pi] = 0;
                    }
                    continue;
                };
                let li = crate::world::cube::index((x & 15) as usize, (y & 15) as usize, (z & 15) as usize);
                let pi = pidx(x, y, z);
                states[pi] = cube.get(li);
                light[pi] = cube.light(li);
            }
        }
    }
    Neighborhood { states, light, biomes, origin: (pos * 16).to_array() }
}

/// `column_shown` over a column's cubes by height: (drawn, all air), `None` when not here.
fn shows_ground(range: (i32, i32), eye: i32, cube: impl Fn(i32) -> Option<(bool, bool)>) -> bool {
    let (lo, hi) = range;
    for y in (lo..=hi).rev() {
        match cube(y) {
            Some((true, air)) => {
                if !air {
                    return true;
                }
            }
            None if y > eye => {}
            Some((false, true)) if y > eye => {}
            _ => return false,
        }
    }
    hi >= lo
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rising, the view's new top cubes arrive after the ground is drawn: those above the camera
    /// do not hand the column back to distant terrain, a gap below it does.
    #[test]
    fn column_shown_while_rising() {
        // Ground at 0, air at 1, the rest not here yet.
        let cube = |y: i32| match y {
            0 => Some((true, false)),
            1 => Some((true, true)),
            _ => None,
        };
        assert!(shows_ground((0, 4), 1, cube), "missing cubes above the camera");
        assert!(!shows_ground((0, 4), 4, cube), "missing cubes below the camera");
    }

    /// The whole-cube copy gives what reading every block does.
    #[test]
    fn neighborhood_matches_blocks() {
        let mut rng = 1u64;
        let mut next = move || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (rng >> 33) as u32
        };
        let mut around: [Option<Arc<Cube>>; 27] = Default::default();
        for (i, slot) in around.iter_mut().enumerate() {
            if i % 5 == 2 && i != 13 {
                continue;
            }
            let states: Box<[u32; 4096]> = Box::new(std::array::from_fn(|_| next() % 7));
            let mut cube = Cube::from_states(&states);
            if i % 2 == 0 {
                for k in 0..4096 {
                    cube.set_light(k, next());
                }
            } else {
                cube.fill_light(next());
            }
            *slot = Some(Arc::new(cube));
        }
        let hidden = 1 << 4 | 1 << 22;
        let n = neighborhood(&around, hidden, 9, IVec3::ZERO);
        for y in -1..17i32 {
            for z in -1..17i32 {
                for x in -1..17i32 {
                    let c = |v: i32| if v < 0 { 0 } else if v > 15 { 2 } else { 1 };
                    let k = (c(y) * 9 + c(z) * 3 + c(x)) as usize;
                    let (s, l) = match &around[k] {
                        Some(cube) => {
                            let i = crate::world::cube::index((x & 15) as usize, (y & 15) as usize, (z & 15) as usize);
                            (cube.get(i), cube.light(i))
                        }
                        None if [c(x), c(y), c(z)].iter().filter(|&&v| v != 1).count() == 1 && hidden & (1 << k) != 0 => (9, 0),
                        None => (AIR, 15),
                    };
                    assert_eq!((n.states[pidx(x, y, z)], n.light[pidx(x, y, z)]), (s, l), "at {x} {y} {z}");
                }
            }
        }
    }
}
