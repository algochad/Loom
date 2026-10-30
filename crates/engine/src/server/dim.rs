use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use glam::{DVec3, IVec3};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use loom_abi::{self as abi, ItemStack, block_flags as bf, damage, entity_flags as ef};

use super::entity::{Entities, Entity, step};
use crate::components as comps;
use crate::content::{AIR, Content};
use crate::host::{self, CallCtx};
use crate::world::cube::VOLUME;
use crate::world::light::{self, CubeMap, Lighter};
use crate::world::{Cube, DIRS, Rng, cube_of, local_index};

const MAX_NEIGHBOR_UPDATES: usize = 1 << 16;
/// Missed ticks are made up in at most this many parts, all in one tick (`run_systems`).
const CATCH_UP_PARTS: u64 = 20;
const ITEM_LIFETIME: u64 = 20 * 60 * 5;

pub enum DimEvent {
    /// `block`: the state it was opened on, 0 when opened from code.
    OpenMenu { player: u64, menu: u32, owner: u64, block: u32 },
    Died { player: u64 },
    Packet { player: u64, channel: u32, data: Vec<u8> },
    /// `dim` u32::MAX = the region's own dimension.
    Teleport { from: u32, id: u64, dim: u32, pos: DVec3 },
    SetTime { dim: u32, time: u64 },
    /// `server::area`, done at the end of the tick across every region of `dim`.
    AreaFill { dim: u32, fill: super::area::AreaFill },
    SetMode { player: u64, mode: crate::world::GameMode },
    Particles { dim: u32, burst: abi::ParticleBurst },
    Sound { dim: u32, sound: u32, pos: DVec3, volume: f32, pitch: f32 },
    Animation { id: u64, name: String, looping: bool },
}

/// Path searches a region does per tick at most (in parallel); more wait for the next tick (a
/// spawned crowd all starting to walk at once).
const PATHS_PER_TICK: usize = 128;

pub struct Dim {
    pub id: u32,
    /// The world seed.
    pub seed: u64,
    pub cubes: FxHashMap<IVec3, Arc<Cube>>,
    pub time: u64,
    rng: Rng,
    scheduled: BinaryHeap<Reverse<(u64, i32, i32, i32)>>,
    /// Next growth step of every growing block (the source of truth) and the queue ordering
    /// them (entries that no longer match the map are stale).
    growth: FxHashMap<IVec3, u64>,
    growth_queue: BinaryHeap<Reverse<(u64, i32, i32, i32)>>,
    /// Time of the growth step being run, so blocks it places time from it rather than now.
    growing_at: Option<u64>,
    /// Blocks along the edge of area edits waiting for their neighbor update, a share per tick
    /// (a crater's rim is millions).
    pub(super) edge_updates: VecDeque<IVec3>,
    /// Players mods see this tick (not spectators), listed once on first use.
    players: Option<Vec<u64>>,
    neighbor_queue: VecDeque<(IVec3, IVec3)>,
    /// Block changes this tick, sent as deltas.
    pub changes: Vec<(IVec3, u32)>,
    /// Cubes whose light changed this tick, sent whole.
    pub resend: Vec<(IVec3, Arc<Cube>)>,
    pub(super) light_dirty: FxHashSet<IVec3>,
    /// Cubes changed since the last save. Untouched cubes are never written (regenerating is cheaper).
    pub modified: FxHashSet<IVec3>,
    pub entities: Entities,
    pub events: Vec<DimEvent>,
    next_id: Arc<AtomicU64>,
    pub grid: super::energy::Grid,
    /// Cubes whose sky light is final: open sky above, or the cube above is settled. Only these
    /// are sent, so clients never see light being filled in afterwards.
    pub settled: FxHashSet<IVec3>,
    /// Bumped whenever a cube becomes settled, so senders know when to look again.
    pub settled_gen: u64,
    /// Cubes settled since the server last looked (to send them to players).
    pub newly_settled: Vec<IVec3>,
    /// Loaded cubes per section, kept as cubes come and go (`sections` for regionizing).
    section_cubes: FxHashMap<IVec3, u32>,
    /// Cubes with block changes since players' sight was last brought up to date (lines of
    /// sight may have opened or closed).
    pub edited: FxHashSet<IVec3>,
    /// Face connectivity of loaded cubes (see `Cube::connectivity`); missing = recompute.
    pub conn: FxHashMap<IVec3, u16>,
    /// Gizmo shapes mods drew this tick, and the groups some player here shows (set before the
    /// tick, so mods can skip drawing nobody sees).
    pub gizmos: Vec<crate::net::GizmoShape>,
    pub gizmo_groups: Arc<[String]>,
    /// World rule values by rule id (`Game::rules`), set by the server before each tick.
    /// Event values waiting for delivery, by event id (ordered, so delivery is deterministic).
    pub event_queue: std::collections::BTreeMap<u32, Vec<u8>>,
    /// A moving structure's own space (`structure.rs`): blocks in its own coordinates, lit as
    /// open to the sky, entities spawned in it handed to the world it moves in.
    pub framed: bool,
    /// The structures of entities of this region, by entity id.
    pub structures: rustc_hash::FxHashMap<u64, Box<Dim>>,
    /// A structure's block bounds (inclusive) and cubes to light again.
    pub bounds: Option<(IVec3, IVec3)>,
    pub relight: FxHashSet<IVec3>,
}

pub(super) struct Cubes<'a>(pub(super) &'a mut FxHashMap<IVec3, Arc<Cube>>);

impl CubeMap for Cubes<'_> {
    fn cube(&self, c: IVec3) -> Option<&Cube> {
        self.0.get(&c).map(|a| &**a)
    }
    fn cube_mut(&mut self, c: IVec3) -> Option<&mut Cube> {
        self.0.get_mut(&c).map(Arc::make_mut)
    }
}

/// What every cube goes through once made (generated, loaded, or on the GPU) before it joins a
/// world, on the server or a client: the area edits made since, then its light on its own as if
/// open sky were above (`light::prelight`; joining the world fixes that up).
pub fn prepare(c: &Content, dim: u32, edits: &[super::area::AreaFill], pos: IVec3, cube: &mut Cube) {
    super::area::catch_up(c, edits, pos, cube);
    light::prelight(c, cube, c.dims[dim as usize].sky_light);
}

pub fn generate(c: &Content, dim: u32, seed: u64, pos: IVec3) -> Cube {
    let def = &c.dims[dim as usize];
    if pos.y < def.min_cube_y || pos.y > def.max_cube_y {
        return Cube::uniform(AIR, 0);
    }
    let mut blocks = vec![0u32; VOLUME];
    let mut biomes = [0u16; 256];
    let cols = match (def.builtin, &def.terrain, def.generator) {
        (Some(f), _, _) => {
            f(c, pos, &mut blocks);
            return finish_generated(c, dim, seed, pos, blocks, biomes, None, true);
        }
        (None, None, None) => return Cube::uniform(AIR, 0),
        (None, Some(t), _) => Some(Arc::new(t.generate(seed, pos.to_array(), &mut blocks, &mut biomes).cols)),
        (None, None, Some(_)) => def.columns(seed, pos.x, pos.z),
    };
    finish_generated(c, dim, seed, pos, blocks, biomes, cols, false)
}

/// Everything generation does after the terrain (which the GPU may have made): the
/// dimension's generator, scattered things, decorations, ores and structures. `builtin`: the
/// blocks came from a built-in dimension, which has none of the first three.
#[allow(clippy::too_many_arguments)]
pub fn finish_generated(c: &Content, dim: u32, seed: u64, pos: IVec3, mut blocks: Vec<u32>, mut biomes: [u16; 256], cols: Option<Arc<[abi::TerrainColumn; 256]>>, builtin: bool) -> Cube {
    let def = &c.dims[dim as usize];
    if !builtin {
        let columns = cols.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let mut args = abi::GenArgs { seed, cube: pos.to_array(), blocks: blocks.as_mut_ptr(), biomes: biomes.as_mut_ptr(), dim, columns };
        if let Some(cb) = def.generator {
            host::invoke(def.owner, cb, &mut args);
        }
        place_scatters(c, def, dim, seed, pos, &mut blocks);
        let mut args = abi::GenArgs { blocks: blocks.as_mut_ptr(), biomes: biomes.as_mut_ptr(), ..args };
        for &(owner, cb) in &def.decorations {
            host::invoke(owner, cb, &mut args);
        }
    }
    let max = c.states.len() as u32;
    let mut bad = 0;
    for b in blocks.iter_mut() {
        if *b >= max {
            bad = *b;
            *b = AIR;
        }
    }
    if bad != 0 {
        host::misuse("generator", || format!("{} wrote state {bad}, which does not exist (air instead)", def.name));
    }
    place_ores(c, def, seed, pos, &mut blocks, &biomes);
    place_structures(c, def, seed, pos, &mut blocks);
    let mut cube = Cube::from_states(blocks.as_slice().try_into().unwrap());
    if biomes.iter().any(|&b| b != 0) {
        cube.biomes = Some(Box::new(biomes));
    }
    cube
}

use loom_noise::hash;

/// Veins start in this and the 26 neighboring cubes and wander a block per step, so the result
/// does not depend on which cube is generated first.
fn place_ores(c: &Content, def: &crate::content::DimensionDef, seed: u64, pos: IVec3, blocks: &mut [u32], biomes: &[u16; 256]) {
    let o = pos * 16;
    let air = c.states[AIR as usize].block;
    if blocks.iter().all(|&b| b == AIR) && def.ores.iter().all(|ore| ore.replace.binary_search(&air).is_err()) {
        return;
    }
    for ore in &def.ores {
        let reach = ore.size as i32;
        if o.y > ore.max_y.saturating_add(reach) || o.y + 15 < ore.min_y.saturating_sub(reach) {
            continue;
        }
        for nz in pos.z - 1..=pos.z + 1 {
            for ny in pos.y - 1..=pos.y + 1 {
                for nx in pos.x - 1..=pos.x + 1 {
                    for v in 0..ore.veins {
                        let h = hash(seed ^ ore.salt, nx as i64, ny as i64 * 31 + v as i64, nz as i64);
                        let mut p = IVec3::new(nx * 16 + (h % 16) as i32, ny * 16 + ((h >> 4) % 16) as i32, nz * 16 + ((h >> 8) % 16) as i32);
                        // A step moves a block at most: veins starting farther away never get here.
                        let far = (o - p).max(p - o - IVec3::splat(15)).max_element();
                        if p.y > ore.max_y || p.y < ore.min_y || far > reach {
                            continue;
                        }
                        for k in 0..ore.size {
                            let hk = hash(h, k as i64, 0, 0);
                            p += IVec3::new((hk % 3) as i32 - 1, ((hk >> 2) % 3) as i32 - 1, ((hk >> 4) % 3) as i32 - 1);
                            let l = p - o;
                            if l.cmpge(IVec3::ZERO).all() && l.cmplt(IVec3::splat(16)).all() {
                                let i = (l.y * 256 + l.z * 16 + l.x) as usize;
                                let biome_ok = ore.biome.is_none_or(|t| c.components.has(comps::BIOME, biomes[(l.z * 16 + l.x) as usize] as u32, t));
                                if biome_ok && ore.replace.binary_search(&c.states[blocks[i] as usize].block).is_ok() {
                                    blocks[i] = ore.state;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Where structure `s` stands in its grid cell (`cell` x, z), if it does there: its lowest
/// corner. Depends only on the seed and the dimension's columns, so every cube agrees.
pub fn structure_in(def: &crate::content::DimensionDef, c: &Content, s: &crate::content::StructureDef, seed: u64, cell: (i32, i32)) -> Option<IVec3> {
    let h = hash(seed ^ s.salt, cell.0 as i64, 7, cell.1 as i64);
    if loom_noise::unit(h) >= s.chance as f64 {
        return None;
    }
    let room = (s.spacing - s.extent.x.max(s.extent.z)).max(1) as u64;
    let (x, z) = (cell.0 * s.spacing + ((h >> 3) % room) as i32, cell.1 * s.spacing + ((h >> 29) % room) as i32);
    let col = def.column(seed, x + s.extent.x / 2, z + s.extent.z / 2)?;
    if s.biome.is_some_and(|t| !c.components.has(comps::BIOME, col.biome, t)) {
        return None;
    }
    Some(IVec3::new(x, col.height + 1 - s.sink, z))
}

/// The site of scatter `s` in its grid cell, if it has one there: the block above the ground,
/// the biome and the site's random bits. Depends only on the seed and the columns.
pub fn scatter_site(def: &crate::content::DimensionDef, c: &Content, s: &crate::content::ScatterDef, seed: u64, cell: (i32, i32)) -> Option<(IVec3, u32, u64)> {
    let h = hash(seed ^ s.salt, cell.0 as i64, 0, cell.1 as i64);
    let (x, z) = (cell.0 * s.cell + (h % s.cell as u64) as i32, cell.1 * s.cell + ((h >> 8) % s.cell as u64) as i32);
    let col = def.column(seed, x, z)?;
    if col.height == i32::MIN || def.sea_level().is_some_and(|l| col.height < l) {
        return None;
    }
    let density = s.density.and_then(|d| c.components.get::<f32>(comps::BIOME, col.biome, d)).unwrap_or(0.0);
    if loom_noise::unit(h.rotate_left(17)) >= density as f64 || def.carved(seed, x, z) {
        return None;
    }
    Some((IVec3::new(x, col.height + 1, z), col.biome, hash(h, 1, 2, 3)))
}

/// How far above and below its site a scattered thing may put blocks.
const SCATTER_UP: i32 = 32;
const SCATTER_DOWN: i32 = 16;

/// A scattered thing: where it stands and its blocks (the shape callback's, made once and
/// shared by every cube it reaches), with the corners of the box around them.
pub struct Site {
    pos: IVec3,
    blocks: Vec<abi::ScatterBlock>,
    lo: IVec3,
    hi: IVec3,
}

/// Every site whose thing may reach into cube column (`x`, `z`), from this and neighboring
/// cells: the same for every cube of the column, so worked out once for it.
fn scatter_sites(c: &Content, def: &crate::content::DimensionDef, dim: u32, seed: u64, x: i32, z: i32) -> std::sync::Arc<Vec<Site>> {
    def.scatter_cache.get(seed, x, z, || {
        let o = IVec3::new(x, 0, z) * 16;
        let mut out = Vec::new();
        let mut buf = vec![abi::ScatterBlock { offset: [0; 3], state: 0, force: 0 }; 4096];
        let states = c.states.len() as u32;
        for s in &def.scatters {
            let cells = |lo: i32| (lo - s.reach).div_euclid(s.cell)..=(lo + 15 + s.reach).div_euclid(s.cell);
            for cz in cells(o.z) {
                for cx in cells(o.x) {
                    let Some((pos, biome, random)) = scatter_site(def, c, s, seed, (cx, cz)) else { continue };
                    let mut args = abi::ScatterArgs { seed, dim, pos: pos.to_array(), biome, random, out: buf.as_mut_ptr(), cap: buf.len() as u32, len: 0 };
                    host::invoke(s.owner, s.shape, &mut args);
                    let blocks: Vec<abi::ScatterBlock> = buf[..(args.len as usize).min(buf.len())].iter().filter(|b| b.state < states && (-SCATTER_DOWN..=SCATTER_UP).contains(&b.offset[1])).copied().collect();
                    let (lo, hi) = blocks.iter().fold((IVec3::MAX, IVec3::MIN), |(lo, hi), b| (lo.min(pos + IVec3::from_array(b.offset)), hi.max(pos + IVec3::from_array(b.offset))));
                    if !blocks.is_empty() {
                        out.push(Site { pos, blocks, lo, hi });
                    }
                }
            }
        }
        out
    })
}

/// Works out the scattered things of cube column (`x`, `z`) ahead of its cubes: they all need
/// them, and the ones finished meanwhile waited for whichever was working them out.
pub fn warm_scatters(c: &Content, dim: u32, seed: u64, x: i32, z: i32) {
    let def = &c.dims[dim as usize];
    if !def.scatters.is_empty() {
        scatter_sites(c, def, dim, seed, x, z);
    }
}

/// The parts of scattered things (trees) that fall into this cube, from sites in this and
/// neighboring cells.
fn place_scatters(c: &Content, def: &crate::content::DimensionDef, dim: u32, seed: u64, pos: IVec3, blocks: &mut [u32]) {
    if def.scatters.is_empty() {
        return;
    }
    let o = pos * 16;
    let sites = scatter_sites(c, def, dim, seed, pos.x, pos.z);
    for site in sites.iter().filter(|s| s.lo.cmple(o + 15).all() && s.hi.cmpge(o).all()) {
        let p = site.pos;
        for b in &site.blocks {
            let l = p + IVec3::from_array(b.offset) - o;
            if l.cmpge(IVec3::ZERO).all() && l.cmplt(IVec3::splat(16)).all() {
                let i = (l.y * 256 + l.z * 16 + l.x) as usize;
                if b.force != 0 || blocks[i] == AIR {
                    blocks[i] = b.state;
                }
            }
        }
    }
}

/// Whether a scattered thing stands within its reach of column (`x`, `z`): spawn search keeps
/// players out of trees.
pub fn scattered_near(def: &crate::content::DimensionDef, c: &Content, seed: u64, x: i32, z: i32) -> bool {
    def.scatters.iter().any(|s| {
        let r = s.reach.max(1);
        let cells = |v: i32| (v - r).div_euclid(s.cell)..=(v + r).div_euclid(s.cell);
        cells(z).any(|cz| cells(x).any(|cx| scatter_site(def, c, s, seed, (cx, cz)).is_some_and(|(p, ..)| (p.x - x).abs() <= r && (p.z - z).abs() <= r)))
    })
}

/// Structure blocks that fall into this cube.
fn place_structures(c: &Content, def: &crate::content::DimensionDef, seed: u64, pos: IVec3, blocks: &mut [u32]) {
    let o = pos * 16;
    for s in &def.structures {
        let cells = |lo: i32, size: i32| (lo - size).div_euclid(s.spacing)..=(lo + 15).div_euclid(s.spacing);
        for cz in cells(o.z, s.extent.z) {
            for cx in cells(o.x, s.extent.x) {
                let Some(a) = structure_in(def, c, s, seed, (cx, cz)) else { continue };
                if a.y > o.y + 15 || a.y + s.extent.y <= o.y {
                    continue;
                }
                for &(off, state) in &s.blocks {
                    let l = a + off - o;
                    if l.cmpge(IVec3::ZERO).all() && l.cmplt(IVec3::splat(16)).all() {
                        blocks[(l.y * 256 + l.z * 16 + l.x) as usize] = state;
                    }
                }
            }
        }
    }
}

/// Appends a timer: fire time, event name and value, as the `TIMERS` component keeps them.
pub fn push_timer(out: &mut Vec<u8>, until: u64, event: &str, data: &[u8]) {
    out.extend_from_slice(&until.to_le_bytes());
    out.extend_from_slice(&(event.len().min(u16::MAX as usize) as u16).to_le_bytes());
    out.extend_from_slice(&event.as_bytes()[..event.len().min(u16::MAX as usize)]);
    out.extend_from_slice(&(data.len().min(u16::MAX as usize) as u16).to_le_bytes());
    out.extend_from_slice(&data[..data.len().min(u16::MAX as usize)]);
}

pub fn read_timers(mut b: &[u8]) -> Vec<(u64, String, Vec<u8>)> {
    let mut out = Vec::new();
    let take = |b: &mut &[u8], n: usize| -> Option<Vec<u8>> {
        let (head, tail) = (b.get(..n)?.to_vec(), b.get(n..)?);
        *b = tail;
        Some(head)
    };
    loop {
        let Some(until) = take(&mut b, 8).map(|v| u64::from_le_bytes(v.try_into().unwrap())) else { break };
        let Some(n) = take(&mut b, 2).map(|v| u16::from_le_bytes([v[0], v[1]]) as usize) else { break };
        let Some(name) = take(&mut b, n).map(|v| String::from_utf8_lossy(&v).into_owned()) else { break };
        let Some(n) = take(&mut b, 2).map(|v| u16::from_le_bytes([v[0], v[1]]) as usize) else { break };
        let Some(data) = take(&mut b, n) else { break };
        out.push((until, name, data));
    }
    out
}

/// Face connectivity through blocks that do not hide what is behind them.
pub fn see_through(c: &Content, cube: &Cube) -> u16 {
    cube.connectivity(|s| c.states[s as usize].flags & bf::OPAQUE == 0)
}

/// Seconds to break `state` holding `tool` (item id, 0 = hand); vanilla's slower rate without
/// a tool that harvests it.
pub fn break_time(c: &Content, state: u32, tool: u32) -> f32 {
    let s = &c.states[state as usize];
    if s.hardness < 0.0 {
        return f32::INFINITY;
    }
    let speed = if c.tool_fits(state, tool) { c.item_value::<f32>(tool, c.ids.mining_speed).unwrap_or(1.0) } else { 1.0 };
    s.hardness * if c.harvests(state, tool) { 1.5 } else { 5.0 } / speed
}

impl Dim {
    /// Writes the fields clients show (`registry::HEALTH` ...) into their synced components when
    /// they differ, so they travel like mods' components. Absent ones stay absent while default.
    pub fn mirror_fields(&mut self, c: &Content) {
        let [health, burning, worn, shape] = c.game.field_comps;
        let maxes: Vec<(u64, f32)> = self.entities.list.iter().filter(|e| e.player && !e.removed).map(|e| e.id).collect::<Vec<_>>().into_iter().map(|id| (id, self.max_health(c, id))).collect();
        let Entities { list, comps, .. } = &mut self.entities;
        for e in list.iter().filter(|e| !e.removed && e.anchor.is_none() && e.kind != c.game.virtual_kind) {
            let put = |comps: &mut FxHashMap<(u64, u32), Box<[u8]>>, comp: u32, bytes: &[u8]| match comps.get_mut(&(e.id, comp)) {
                Some(v) if **v != *bytes => *v = bytes.into(),
                Some(_) => {}
                None if *c.game.components[comp as usize].default != *bytes => {
                    comps.insert((e.id, comp), bytes.into());
                }
                None => {}
            };
            let max = maxes.iter().find(|m| m.0 == e.id).map_or(0.0, |m| m.1);
            let mut hp = [0u8; 8];
            hp[..4].copy_from_slice(&e.health.max(0.0).to_le_bytes());
            hp[4..].copy_from_slice(&max.to_le_bytes());
            put(comps, health, &hp);
            put(comps, burning, &[(e.fire > 0) as u8]);
            let items: Vec<u8> = e.worn.iter().flat_map(|i| i.to_le_bytes()).collect();
            put(comps, worn, &items);
            let mut sh = [0u8; 16];
            for (i, v) in e.size.unwrap_or([0.0; 3]).iter().enumerate() {
                sh[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            sh[12..].copy_from_slice(&e.look.unwrap_or(u32::MAX).to_le_bytes());
            put(comps, shape, &sh);
        }
    }

    pub fn new(id: u32, seed: u64, next_id: Arc<AtomicU64>) -> Dim {
        Dim {
            id,
            seed,
            cubes: FxHashMap::default(),
            time: 1000,
            rng: Rng(loom_noise::mix(seed ^ id as u64)),
            scheduled: BinaryHeap::new(),
            growth: FxHashMap::default(),
            growth_queue: BinaryHeap::new(),
            growing_at: None,
            edge_updates: VecDeque::new(),
            players: None,
            neighbor_queue: VecDeque::new(),
            changes: Vec::new(),
            resend: Vec::new(),
            light_dirty: FxHashSet::default(),
            modified: FxHashSet::default(),
            entities: Entities::default(),
            events: Vec::new(),
            next_id,
            grid: Default::default(),
            settled: FxHashSet::default(),
            settled_gen: 0,
            newly_settled: Vec::new(),
            section_cubes: FxHashMap::default(),
            framed: false,
            structures: FxHashMap::default(),
            bounds: None,
            relight: FxHashSet::default(),
            edited: FxHashSet::default(),
            conn: FxHashMap::default(),
            gizmos: Vec::new(),
            gizmo_groups: Arc::from([]),
            event_queue: Default::default(),
        }
    }

    pub fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The solid body right under feet at `pos` (a step at most), and where that body is.
    pub fn body_under(&self, c: &Content, pos: DVec3, half_width: f64) -> Option<(u64, DVec3)> {
        let bodies: Vec<crate::physics::Body> = self
            .entities
            .list
            .iter()
            .filter(|e| !e.removed && !e.player && c.game.entities[e.kind as usize].is(ef::SOLID))
            .filter_map(|e| {
                let built = self.structures.get(&e.id);
                let aabb = built.map_or(Some(e.aabb(c)), |s| s.structure_box(e.pos))?;
                let blocks = built.map(|s| (&**s as &dyn crate::physics::Blocks, super::structure::origin(e.pos)));
                Some(crate::physics::Body { id: e.id, aabb, delta: DVec3::ZERO, carry: 0.0, crushes: false, blocks })
            })
            .collect();
        let id = crate::physics::body_below(c, pos, half_width, &bodies, 0.6)?;
        self.entities.get(id).map(|e| (id, e.pos))
    }

    pub fn has(&self, pos: IVec3) -> bool {
        self.cubes.contains_key(&pos)
    }

    pub fn snapshot(&self, pos: IVec3) -> Option<Arc<Cube>> {
        self.cubes.get(&pos).cloned()
    }

    /// `prelit`: the cube carries light from `light::prelight`.
    pub fn insert(&mut self, c: &Content, pos: IVec3, mut cube: Cube, prelit: bool) {
        if self.cubes.contains_key(&pos) {
            return;
        }
        self.grid.cube_loaded(c, pos, &cube);
        let def = &c.dims[self.id as usize];
        let open_above = light::open_above(def, pos.y);
        // Clear cubes have a cheaper path of their own in `light_new`.
        let clear = cube.palette().iter().all(|&s| c.tight[s as usize].opacity == 0);
        // The worker's light assumed open sky above.
        let prelit = prelit
            && !clear
            && (!def.sky_light
                || match light::sky_below(self.cubes.get(&(pos + IVec3::Y)).map(|a| &**a), open_above) {
                    light::SkyBelow::Full => true,
                    light::SkyBelow::Dark => {
                        cube.mask_light(!0xFF);
                        true
                    }
                    light::SkyBelow::Partial => false,
                });
        // What time acts on continues from when the cube was last simulated: steps due while it
        // was away run in the next tick, in order. Spreading and covered blocks only in cubes
        // that were saved with some: freshly generated terrain stays as generated until
        // something changes near it (a cliff of dirt does not start greening everywhere at once,
        // which kept every new area remeshing).
        let mut lives = Vec::new();
        let saved = cube.ticked != 0;
        let since = if saved { cube.ticked.min(self.time) } else { self.time };
        let wanted = |s: u32| if saved { c.lives(s) } else { c.grows(s).is_some() };
        if cube.palette().iter().any(|&s| wanted(s)) {
            for i in 0..VOLUME {
                let s = cube.get(i);
                if wanted(s) {
                    let p = pos * 16 + crate::world::cube::offset(i);
                    match c.grows(s) {
                        Some(_) => self.schedule_growth(c, p, s, since),
                        None => lives.push(p),
                    }
                }
            }
        }
        self.add_cube(pos, Arc::new(cube));
        for p in lives {
            self.consider(c, p, since);
        }
        let mut map = Cubes(&mut self.cubes);
        let mut lighter = Lighter::new(&mut map, c, def.sky_light);
        if prelit {
            lighter.light_prelit(pos);
        } else {
            lighter.light_new(pos, open_above);
        }
        let changed = std::mem::take(&mut lighter.changed);
        changed.into_iter().filter(|&p| p != pos).for_each(|p| {
            self.light_dirty.insert(p);
        });
        if open_above || !def.sky_light || self.settled.contains(&(pos + IVec3::Y)) {
            self.settle_down(pos);
        }
    }

    /// Drops cubes `keep` rejects; returns the ones that must be saved (modified or holding
    /// entities) together with the entities that lived in them, which leave the world.
    pub fn retain_cubes(&mut self, c: &Content, keep: impl Fn(IVec3) -> bool) -> Vec<(IVec3, Arc<Cube>, Vec<u64>)> {
        let mut out = Vec::new();
        let gone: FxHashSet<IVec3> = self.cubes.keys().copied().filter(|&p| !keep(p)).collect();
        if gone.is_empty() {
            return out;
        }
        // One pass over entities and growth for all cubes leaving, not one per cube (a crowd of
        // mobs times thousands of cubes).
        let mut inside: FxHashMap<IVec3, Vec<u64>> = FxHashMap::default();
        for e in self.entities.list.iter().filter(|e| !e.removed && !e.player) {
            let cp = cube_of(e.pos.floor().as_ivec3());
            if gone.contains(&cp) {
                inside.entry(cp).or_default().push(e.id);
            }
        }
        let mut grew = false;
        let mut gone: Vec<IVec3> = gone.into_iter().collect();
        gone.sort_unstable_by_key(|p| (p.x, p.y, p.z));
        for pos in gone {
            let inside = inside.remove(&pos).unwrap_or_default();
            let mut cube = self.cubes.remove(&pos).unwrap();
            self.cube_gone(pos);
            self.grid.cube_unloaded(pos);
            self.settled.remove(&pos);
            self.conn.remove(&pos);
            // Only cubes where something happened are saved; the rest generate again the same.
            // Saved ones keep the time their growing blocks were simulated to, to catch up later
            // (untouched ones start growing again when they come back, like vanilla's).
            let grows = cube.palette().iter().any(|&s| c.lives(s));
            grew |= grows;
            let modified = self.modified.remove(&pos);
            if modified || !inside.is_empty() {
                if grows {
                    Arc::make_mut(&mut cube).ticked = self.time;
                }
                out.push((pos, cube, inside));
            }
        }
        if grew {
            let cubes = &self.cubes;
            self.growth.retain(|p, _| cubes.contains_key(&cube_of(*p)));
        }
        out
    }

    pub fn get(&self, p: IVec3) -> Option<u32> {
        self.cubes.get(&cube_of(p)).map(|cube| cube.get(local_index(p)))
    }

    pub fn light(&self, p: IVec3) -> u32 {
        self.cubes.get(&cube_of(p)).map_or(0, |cube| cube.light(local_index(p)))
    }

    /// 0..1 strength of the sun.
    pub fn daylight(&self, c: &Content) -> f32 {
        self.daylight_at(c, self.time)
    }

    pub fn daylight_at(&self, c: &Content, time: u64) -> f32 {
        c.dims[self.id as usize].daylight(time)
    }

    /// Writes a block, relights, records the change for clients, queues neighbor updates and keeps
    /// block entities in sync with their block.
    pub fn set(&mut self, c: &'static Content, p: IVec3, state: u32) -> Option<u32> {
        if !c.dims[self.id as usize].editable {
            return None;
        }
        let cp = cube_of(p);
        if self.framed && state != AIR {
            self.structure_cube(c, cp);
        }
        let cube = Arc::make_mut(self.cubes.get_mut(&cp)?);
        let old = cube.set(local_index(p), state);
        if old == state {
            return Some(old);
        }
        self.changes.push((p, state));
        self.modified.insert(cp);
        self.conn.remove(&cp);
        self.edited.insert(cp);
        if self.framed {
            self.grow_bounds(p, state);
            self.relight.insert(cp);
        } else {
            let def = &c.dims[self.id as usize];
            let mut map = Cubes(&mut self.cubes);
            let mut lighter = Lighter::new(&mut map, c, def.sky_light);
            lighter.block_changed(p, old);
            let changed = std::mem::take(&mut lighter.changed);
            self.light_dirty.extend(changed);
        }
        // The new block hears about itself too (like vanilla's onBlockAdded): a liquid starts
        // flowing, a plant without support drops, whoever set it.
        self.neighbor_queue.push_back((p, p));
        for d in DIRS {
            self.neighbor_queue.push_back((p + d, p));
        }
        self.grid.block_changed(c, p, old, state);
        let from = self.growing_at.unwrap_or(self.time);
        if c.grows(state).is_none() {
            self.growth.remove(&p);
        } else if !self.growth.contains_key(&p) {
            self.schedule_growth(c, p, state, from);
        }
        // What may now happen with time: here, to the block it covers or uncovers, and around a
        // block that spreads.
        self.consider(c, p, from);
        self.consider(c, p - IVec3::Y, from);
        if c.states[state as usize].spread.is_some() {
            for dy in -3..=1 {
                for dz in -1..=1 {
                    for dx in -1..=1 {
                        self.consider(c, p + IVec3::new(dx, dy, dz), from);
                    }
                }
            }
        }
        let (ob, nb) = (c.states[old as usize].block, c.states[state as usize].block);
        if ob != nb {
            if let Some(&id) = self.entities.anchors.get(&p) {
                if let Some(items) = self.entities.inventories.get(&id).cloned() {
                    for s in items.into_iter().filter(|s| !s.is_empty()) {
                        self.spawn_item(c, p.as_dvec3() + DVec3::splat(0.5), s);
                    }
                }
                self.entities.remove(id);
            }
            if let Some(&kind) = c.game.block_entities.get(&nb) {
                self.add_block_entity(c, p, kind);
            }
        }
        Some(old)
    }

    fn add_block_entity(&mut self, c: &Content, p: IVec3, kind: u32) {
        let id = self.new_id();
        let mut e = Entity::new(id, kind, p.as_dvec3() + DVec3::splat(0.5), 1.0);
        e.anchor = Some(p);
        self.entities.insert(c, e);
    }

    /// Whether a saved block entity still has its block: the block at its anchor is one that
    /// has this kind, and no other entity took the place.
    pub fn block_entity_fits(&self, c: &Content, e: &Entity) -> bool {
        e.anchor.is_some_and(|a| {
            !self.entities.anchors.contains_key(&a) && self.get(a).and_then(|s| c.game.block_entities.get(&c.states[s as usize].block)) == Some(&e.kind)
        })
    }

    /// Gives every block of the cube that has a block entity one, if it has none: blocks a
    /// generator placed, or a block a mod gave an entity after the world was saved.
    pub fn add_missing_block_entities(&mut self, c: &Content, pos: IVec3) {
        let Some(cube) = self.cubes.get(&pos).cloned() else { return };
        if !cube.palette().iter().any(|&s| c.game.block_entities.contains_key(&c.states[s as usize].block)) {
            return;
        }
        for i in 0..VOLUME {
            let Some(&kind) = c.game.block_entities.get(&c.states[cube.get(i) as usize].block) else { continue };
            let p = pos * 16 + crate::world::cube::offset(i);
            if !self.entities.anchors.contains_key(&p) {
                self.add_block_entity(c, p, kind);
            }
        }
    }

    /// 0 for kinds that belong to a block: those come only with their block.
    pub fn spawn(&mut self, c: &Content, kind: u32, pos: DVec3) -> u64 {
        if c.game.is_block_entity(kind) {
            return 0;
        }
        let id = self.new_id();
        let k = &c.game.entities[kind as usize];
        if let Some(blocks) = super::structure::template(c, kind) {
            self.structures.insert(id, Box::new(Dim::new_structure(c, self.id, self.seed, self.next_id.clone(), blocks)));
        }
        let mut e = Entity::new(id, kind, pos, 0.0);
        // Solid bodies collide as axis-aligned boxes, so they start square to the world.
        if !k.is(ef::SOLID) {
            e.yaw = (self.rng.float() * std::f32::consts::TAU) - std::f32::consts::PI;
        }
        self.entities.insert(c, e);
        let max = self.max_health(c, id);
        if let Some(e) = self.entities.get_mut(id) {
            e.health = max;
        }
        id
    }

    /// The block at `p` starts falling (sand, gravel, anvils): a moving structure of that one
    /// block, drawn and colliding like any. It is placed again where it lands, or drops what it
    /// drops if it cannot be.
    pub fn fall_block(&mut self, c: &'static Content, p: IVec3) -> u64 {
        let Some(state) = self.get(p).filter(|&s| s != AIR) else { return 0 };
        self.set(c, p, AIR);
        let id = self.spawn(c, c.game.falling_kind, p.as_dvec3() + DVec3::new(0.5, 0.0, 0.5));
        if id != 0 {
            self.structures.insert(id, Box::new(Dim::new_structure(c, self.id, self.seed, self.next_id.clone(), &[(IVec3::ZERO, state)])));
        }
        id
    }

    /// Falling blocks that came down: placed where they landed, else dropped as loot. They rest
    /// on the ground a few ticks first, the time clients show bodies behind the server, so the
    /// block appears where the falling one is drawn.
    fn land_falling(&mut self, c: &'static Content) {
        for e in self.entities.list.iter_mut().filter(|e| e.kind == c.game.falling_kind) {
            e.rested = if e.on_ground { e.rested.saturating_add(1) } else { 0 };
        }
        let structures = &self.structures;
        let landed: Vec<(u64, IVec3, u32, bool)> = self
            .entities
            .list
            .iter()
            // One without its block (loaded from a save mid-fall) just goes.
            .filter(|e| e.kind == c.game.falling_kind && !e.removed)
            .map(|e| (e, structures.get(&e.id).and_then(|s| s.get(IVec3::ZERO)).unwrap_or(AIR)))
            .filter(|(e, state)| *state == AIR || e.rested > 2 || e.age > 600)
            .map(|(e, state)| (e.id, e.pos.floor().as_ivec3(), state, e.on_ground))
            .collect();
        for (id, p, state, ground) in landed {
            self.entities.remove(id);
            let state = if state == AIR { 0 } else { state };
            if state == 0 {
                continue;
            }
            let free = self.get(p).is_some_and(|s| c.states[s as usize].flags & bf::REPLACEABLE != 0 || s == AIR);
            if ground && free && self.supported(c, p, state) {
                self.set(c, p, state);
            } else {
                self.drop_block_loot(c, p, state, ItemStack::EMPTY, false, None);
            }
        }
    }

    pub fn spawn_item(&mut self, c: &Content, pos: DVec3, stack: ItemStack) -> u64 {
        if stack.is_empty() {
            return 0;
        }
        let id = self.new_id();
        let mut e = Entity::new(id, c.game.item_kind, pos, 1.0);
        e.item = stack;
        e.pickup_delay = 10;
        let (a, b) = (self.rng.float() - 0.5, self.rng.float() - 0.5);
        e.vel = DVec3::new(a as f64 * 2.0, 3.0, b as f64 * 2.0);
        self.entities.insert(c, e);
        id
    }

    /// Calls a mod callback with this dimension as its world.
    pub fn call<A>(&mut self, owner: abi::ModId, cb: abi::Callback, make: impl FnOnce(*mut abi::WorldCtx) -> A) -> A {
        let mut ctx = CallCtx { dim: self as *mut Dim };
        let mut a = make(ctx.raw());
        host::invoke(owner, cb, &mut a);
        a
    }

    fn call_block<A>(&mut self, c: &'static Content, state: u32, pick: impl Fn(&crate::content::BlockCallbacks) -> Option<abi::Callback>, args: impl FnOnce(*mut abi::WorldCtx) -> A) -> Option<A> {
        let block = c.block_of(state);
        let cb = pick(&block.cb)?;
        Some(self.call(block.owner, cb, args))
    }

    /// A melee hit of `attacker` on `target`, the same for players and mobs: the `ATTACK`
    /// question, then the `ATTACK_DAMAGE` chain from the held item's attack (players) or the
    /// kind's `attack_damage` component (mobs, 1 without), then the damage. Mobs wait a second
    /// between hits, like vanilla's; players are only held back by the target's hurt time.
    pub fn attack(&mut self, c: &'static Content, attacker: u64, target: u64, held: ItemStack) -> bool {
        let (Some(me), Some(t)) = (self.entities.get(attacker), self.entities.get(target)) else { return false };
        if target == attacker || !t.item.is_empty() || t.anchor.is_some() || me.attack_cooldown > 0 {
            return false;
        }
        let (kind, player) = (me.kind, me.player);
        let mut q = abi::Query { entity: attacker, other: target, item: held.item, ..abi::Query::DEFAULT };
        if !self.ask(c, abi::question::ATTACK, &mut q) {
            return false;
        }
        let base = if held.item != 0 {
            c.items.get(held.item as usize).map_or(1.0, |i| i.attack)
        } else if player {
            1.0
        } else {
            c.components.id(abi::comp::ATTACK_DAMAGE).and_then(|id| c.components.get::<f32>(crate::components::ENTITY, kind, id)).unwrap_or(1.0)
        };
        if let Some(e) = self.entities.get_mut(attacker).filter(|e| !e.player) {
            e.attack_cooldown = 20;
        }
        let dmg = self.compute(c, abi::chain::ATTACK_DAMAGE, base as f64, &q, false) as f32;
        self.damage(c, target, dmg, attacker, damage::ATTACK)
    }

    pub fn damage(&mut self, c: &'static Content, id: u64, amount: f32, source: u64, cause: u32) -> bool {
        let Some(e) = self.entities.get(id) else { return false };
        let k = &c.game.entities[e.kind as usize];
        if ((k.is(ef::INVULNERABLE) || e.mode.invulnerable()) && cause != damage::VOID) || e.hurt_cooldown > 0 || e.health <= 0.0 {
            return false;
        }
        // One path: the `HURT` veto, the `DAMAGE` chain, then the `ENTITY_HURT` event.
        let mut q = abi::Query { entity: id, other: source, cause, amount: amount as f64, ..abi::Query::DEFAULT };
        if !self.ask(c, abi::question::HURT, &mut q) {
            return false;
        }
        let amount = self.compute(c, abi::chain::DAMAGE, amount as f64, &q, crate::log::on(crate::log::Channel::Modifiers, log::Level::Debug)) as f32;
        if crate::log::on(crate::log::Channel::Modifiers, log::Level::Debug) {
            log::debug!(target: "modifiers", "{}", super::extend::explanation());
        }
        if amount <= 0.0 {
            return false;
        }
        self.emit_engine(c, abi::event::ENTITY_HURT, &abi::Hurt { entity: id, source, amount, cause });
        let knock = self.entities.get(source).map(|s| s.pos);
        let Some(e) = self.entities.get_mut(id) else { return false };
        e.health -= amount;
        e.hurt_cooldown = 10;
        let mut knocked: Option<[f32; 2]> = None;
        if let Some(from) = knock {
            let d = (e.pos - from).with_y(0.0).normalize_or_zero();
            e.vel += d * 6.0 + DVec3::Y * 5.0;
            knocked = e.player.then(|| [(from.x - e.pos.x) as f32, (from.z - e.pos.z) as f32]);
        }
        let (health, player, pos, kind, on_fire) = (e.health, e.player, e.pos, e.kind, e.fire > 0);
        let sounds = k.sounds;
        self.sound(if health <= 0.0 { sounds[1] } else { sounds[0] }, pos + DVec3::Y * (k.height as f64 * 0.5), 1.0, 0.0);
        if let Some([x, z]) = knocked {
            // Vanilla's knockBack: half the speed, 0.4 away from the hit and up.
            let d = glam::Vec2::new(x, z);
            let d = if d.length_squared() < 1e-8 { glam::Vec2::X } else { d.normalize() };
            self.entities.pushed.push((id, ([-d.x * 0.4, 0.4, -d.y * 0.4], true)));
        }
        if health <= 0.0 {
            self.entities.killed.push(id);
        }
        if health <= 0.0 {
            self.emit_engine(c, abi::event::ENTITY_DIED, &abi::Death { entity: id, killer: source, kind, cause, pos: pos.to_array() });
            if !player {
                let by_player = self.entities.get(source).is_some_and(|s| s.player);
                let tool = self.entities.get(source).filter(|s| s.player).and_then(|s| self.entities.inventories.get(&source).and_then(|i| i.get(s.selected as usize)).copied()).unwrap_or_default();
                let ctx = crate::loot::Ctx { tool, by_player, on_fire: on_fire || cause == damage::FIRE || cause == damage::LAVA, mined: 0 };
                for s in self.roll_loot(c, &k.name, &ctx) {
                    self.spawn_item(c, pos + DVec3::Y * 0.5, s);
                }
            }
            if player {
                if let Some(items) = self.entities.inventories.get(&id).cloned() {
                    let pos = self.entities.get(id).map_or(DVec3::ZERO, |e| e.pos);
                    for (i, s) in items.into_iter().enumerate() {
                        if !s.is_empty() {
                            self.spawn_item(c, pos + DVec3::Y, s);
                            self.entities.set_slot(id, i as u32, ItemStack::EMPTY);
                        }
                    }
                }
                self.events.push(DimEvent::Died { player: id });
            } else {
                self.entities.remove(id);
            }
        }
        true
    }

    pub fn tick(&mut self, c: &'static Content) {
        self.time += 1;
        self.players = None;
        let updates = c.dims[self.id as usize].block_updates;
        if !updates {
            self.scheduled.clear();
        }

        while let Some(&Reverse((at, x, y, z))) = self.scheduled.peek() {
            if at > self.time {
                break;
            }
            self.scheduled.pop();
            // The same block scheduled several times for one tick runs once.
            while self.scheduled.peek() == Some(&Reverse((at, x, y, z))) {
                self.scheduled.pop();
            }
            let p = IVec3::new(x, y, z);
            if let Some(state) = self.get(p) {
                if c.states[state as usize].liquid.is_some() {
                    self.liquid_tick(c, p, state);
                } else {
                    self.call_block(c, state, |b| b.on_scheduled, |w| abi::BlockEventArgs { world: w, pos: p.to_array(), state, other: [0; 3], entity: 0 });
                }
            }
        }

        if updates {
            self.grow(c);
        }

        self.natural_spawns(c);
        self.tick_entities(c);
        self.tick_structures(c);
        self.grid.tick(&mut self.entities, c, None);
        self.flush_neighbors(c);
        self.entities.compact();
        self.finish_tick(c);
    }

    /// Structures tick like the world (scheduled ticks, neighbor updates, growth) at its time;
    /// what they let go of comes over here. Structures whose entity is gone go with it.
    fn tick_structures(&mut self, c: &'static Content) {
        let entities = &self.entities;
        self.structures.retain(|id, _| entities.get(*id).is_some());
        let mut ids: Vec<u64> = self.structures.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let s = self.structures.get_mut(&id).unwrap();
            s.time = self.time - 1;
            s.tick(c);
            self.hand_over(c, id);
        }
    }

    /// Players mods see (spectators left out, so mobs neither chase nor spawn around them).
    pub fn visible_players(&mut self) -> &[u64] {
        let list = &self.entities.list;
        self.players.get_or_insert_with(|| list.iter().filter(|e| e.player && !e.removed && e.mode != crate::world::GameMode::Spectator).map(|e| e.id).collect())
    }

    /// Starts or retargets `id`'s walk to `target`. A target close to the current one only
    /// moves it (the path follows at the next look); for a new one the path is looked for in
    /// `navigation`, after the mods' tick, together with every other walk started this tick.
    pub fn navigate(&mut self, id: u64, target: DVec3, speed: f32) -> bool {
        let Some(e) = self.entities.get_mut(id) else { return false };
        if speed <= 0.0 {
            e.nav = None;
            return false;
        }
        if let Some(nav) = e.nav.as_mut().filter(|n| (n.target - target).length_squared() < 1.0) {
            (nav.target, nav.speed) = (target, speed);
            return true;
        }
        e.nav = Some(super::path::Nav { target, speed, path: Vec::new(), next: 1, repath: e.age, found_for: None, stuck: 0 });
        true
    }

    /// Entities walking somewhere by themselves: a new path when the target moved off or the walk
    /// got stuck (at most once a second), steering towards the next cell, done at the target or
    /// after two seconds stuck. Steering a mod gave this
    /// tick (`walk`) wins.
    fn navigation(&mut self, c: &Content) {
        let cubes = &self.cubes;
        let block = |p: IVec3| cubes.get(&cube_of(p)).map(|cb| cb.get(local_index(p)));
        let walks = |e: &Entity| e.nav.is_some() && !e.removed && !e.player && e.walk.is_none();
        let list = &mut self.entities.list;
        let due: Vec<usize> = (0..list.len()).filter(|&i| walks(&list[i]) && list[i].nav.as_ref().is_some_and(|n| n.stale(list[i].age))).take(PATHS_PER_TICK).collect();
        let found: Vec<Vec<IVec3>> = due
            .par_iter()
            .map(|&i| {
                let (e, nav) = (&list[i], list[i].nav.as_ref().unwrap());
                super::path::find(c, &block, e.pos.floor().as_ivec3(), nav.target.floor().as_ivec3(), c.game.entities[e.kind as usize].height.ceil() as i32)
            })
            .collect();
        for (&i, path) in due.iter().zip(found) {
            let e = &mut list[i];
            let nav = e.nav.as_mut().unwrap();
            nav.path = path;
            (nav.next, nav.repath, nav.found_for, nav.stuck) = (1, e.age + 20, Some(nav.target), 0);
        }
        for e in list.iter_mut().filter(|e| walks(e)) {
            let nav = e.nav.as_mut().unwrap();
            nav.stuck = if e.moved < 0.02 { nav.stuck + 1 } else { 0 };
            let close = (nav.target - e.pos).with_y(0.0).length() < 1.0 && (nav.target.y - e.pos.y).abs() < 1.5;
            match super::path::steer(nav, e.pos, e.blocked) {
                _ if close || nav.stuck > 40 => e.nav = None,
                Some((dir, jump)) => {
                    e.yaw = (-dir[0]).atan2(-dir[1]);
                    e.walk = Some((dir, nav.speed, jump));
                }
                // The end of a path that stopped short: look again.
                None => nav.found_for = None,
            }
        }
    }

    /// Natural spawning (`abi::SpawnDesc`) around this region's players every two seconds.
    fn natural_spawns(&mut self, c: &'static Content) {
        // Nothing spawns where nothing may change (the debug blocks, the void).
        if self.time % 40 != 0 || c.game.spawns.is_empty() || !c.dims[self.id as usize].editable {
            return;
        }
        let on = crate::config::get(c.game.mob_spawning) != 0.0;
        if !on {
            return;
        }
        let cats = &c.game.categories;
        let daylight = self.daylight(c);
        let players: Vec<DVec3> = self.entities.list.iter().filter(|e| e.player && !e.removed && e.health > 0.0).map(|e| e.pos).collect();
        for p in players {
            let mut count = vec![0u32; cats.len()];
            for e in self.entities.list.iter().filter(|e| !e.removed && !e.player && e.item.is_empty() && (e.pos - p).length_squared() < 64.0 * 64.0) {
                count[c.game.entities[e.kind as usize].category as usize] += 1;
            }
            for _ in 0..4 {
                let a = self.rng.float() as f64 * std::f64::consts::TAU;
                let r = 20.0 + self.rng.float() as f64 * 25.0;
                let (x, z) = ((p.x + a.cos() * r).floor() as i32, (p.z + a.sin() * r).floor() as i32);
                // First spot with room for a mob, scanning down from above the player.
                // Unloaded counts as solid; liquids are not solid.
                let state = |y: i32| self.get(IVec3::new(x, y, z)).map(|s| &c.states[s as usize]);
                let solid = |y: i32| state(y).is_none_or(|s| s.flags & bf::SOLID != 0);
                let top = p.y.floor() as i32 + 16;
                let Some(y) = (top - 32..top).rev().find(|&y| solid(y - 1) && !solid(y) && state(y).is_some_and(|s| s.liquid.is_none()) && !solid(y + 1)) else {
                    continue;
                };
                let spot = IVec3::new(x, y, z);
                let light = abi::light::effective(self.light(spot), daylight);
                let biome = self.cubes.get(&cube_of(spot)).map_or(0, |cube| cube.biome((x & 15) as usize, (z & 15) as usize)) as u32;
                let ground = self.get(spot - IVec3::Y).unwrap_or(AIR);
                let fits = |s: &crate::registry::SpawnRule| {
                    count[s.category as usize] < cats[s.category as usize].cap
                        && s.dimension.is_none_or(|d| d == self.id)
                        && (s.light[0]..=s.light[1]).contains(&light)
                        && (s.y[0]..=s.y[1]).contains(&y)
                        && s.biome.is_none_or(|t| c.components.has(comps::BIOME, biome, t))
                        && s.ground.is_none_or(|t| c.components.has(comps::BLOCK, ground, t))
                };
                let total: u32 = c.game.spawns.iter().filter(|s| fits(s)).map(|s| s.weight).sum();
                if total == 0 {
                    continue;
                }
                let mut pick = (self.rng.next() % total as u64) as u32;
                let Some(rule) = c.game.spawns.iter().filter(|s| fits(s)).find(|s| {
                    let hit = pick < s.weight;
                    pick = pick.saturating_sub(s.weight);
                    hit
                }) else {
                    continue;
                };
                if self.rng.float() >= rule.chance {
                    continue;
                }
                let n = rule.group[0] + (self.rng.next() % (rule.group[1] - rule.group[0] + 1) as u64) as u32;
                for _ in 0..n {
                    self.spawn(c, rule.entity, DVec3::new(x as f64 + 0.5, y as f64, z as f64 + 0.5));
                }
                count[rule.category as usize] += n;
            }
        }
    }

    /// Timers (`Host::entity_after`) of this region's entities that are due fire their events.
    fn fire_timers(&mut self, c: &'static Content) {
        let comp = c.game.timers_comp;
        let ids: Vec<u64> = self.entities.list.iter().filter(|e| !e.removed).map(|e| e.id).filter(|id| self.entities.comps.contains_key(&(*id, comp))).collect();
        for id in ids {
            let Some(bytes) = self.entities.comps.get(&(id, comp)) else { continue };
            let mut due = Vec::new();
            let mut kept = Vec::new();
            for (until, name, data) in read_timers(bytes) {
                if until <= self.time {
                    due.push((name, data));
                } else {
                    push_timer(&mut kept, until, &name, &data);
                }
            }
            if due.is_empty() {
                continue;
            }
            if kept.is_empty() {
                self.entities.comps.remove(&(id, comp));
            } else {
                self.entities.comps.insert((id, comp), kept.into_boxed_slice());
            }
            for (name, data) in due {
                match c.game.event_names.get(&name) {
                    Some(&e) => self.emit(c, e, &data),
                    None => log::warn!("entity {id}: a timer for event {name}, which no mod has now"),
                }
            }
        }
    }

    fn tick_entities(&mut self, c: &'static Content) {
        // Mod logic: one call per kind and callback with every entity of that kind.
        let mut by_kind: FxHashMap<u32, Vec<u64>> = FxHashMap::default();
        for e in &self.entities.list {
            if !e.removed && (!e.player || e.health > 0.0) {
                by_kind.entry(e.kind).or_default().push(e.id);
            }
        }
        let mut kinds: Vec<u32> = by_kind.keys().copied().collect();
        kinds.sort();
        for kind in kinds {
            let ids = &by_kind[&kind];
            for &(owner, cb) in &c.game.entities[kind as usize].on_tick {
                let ids_ptr = ids.as_ptr();
                let count = ids.len();
                self.call(owner, cb, |w| abi::EntityTickArgs { world: w, kind, ids: ids_ptr, count });
            }
        }
        self.run_systems(c);
        for &(owner, cb) in &c.game.world_ticks {
            self.call(owner, cb, |w| abi::WorldTickArgs { world: w });
        }
        self.fire_timers(c);
        self.navigation(c);

        let gravity = c.dims[self.id as usize].gravity as f64;
        let day = self.daylight(c);
        let players: Vec<DVec3> = self.entities.list.iter().filter(|e| e.player && !e.removed).map(|e| e.pos).collect();
        let cubes = &self.cubes;
        let block = |p: IVec3| cubes.get(&cube_of(p)).map(|cb| cb.get(local_index(p)));
        // Solid bodies move first, in list order, so riders and whatever they run into see where
        // they ended up; a body may ride one moved before it.
        let mut bodies: Vec<crate::physics::Body> = Vec::new();
        let structures = &self.structures;
        for e in self.entities.list.iter_mut() {
            let k = &c.game.entities[e.kind as usize];
            if e.removed || e.player || !k.is(ef::SOLID) {
                continue;
            }
            let before = e.pos;
            step(c, e, gravity, &block, &bodies);
            // A structure collides as its blocks, an empty one not at all.
            let built = structures.get(&e.id);
            let Some(aabb) = built.map_or(Some(e.aabb(c)), |s| s.structure_box(e.pos)) else { continue };
            let blocks = built.map(|s| (&**s as &dyn crate::physics::Blocks, super::structure::origin(e.pos)));
            bodies.push(crate::physics::Body { id: e.id, aabb, delta: e.pos - before, carry: k.carry_height as f64, crushes: k.is(ef::CRUSHES), blocks });
        }
        let bodies = &bodies;
        // Physics only reads blocks and bodies and writes its own entity, so entities move in
        // parallel; damage is applied afterwards in list order to stay deterministic.
        let hits: Vec<(u64, Option<f32>, bool, bool, bool)> = self
            .entities
            .list
            .par_iter_mut()
            .with_min_len(64)
            .filter_map(|e| {
                if e.removed {
                    return None;
                }
                e.age += 1;
                e.hurt_cooldown = e.hurt_cooldown.saturating_sub(1);
                e.attack_cooldown = e.attack_cooldown.saturating_sub(1);
                if e.player && (e.health <= 0.0 || e.mode == crate::world::GameMode::Spectator) {
                    return None;
                }
                let k = &c.game.entities[e.kind as usize];
                let mut land = None;
                let mut crushed = false;
                // Players move on their client, bodies moved above; the rest here.
                if !e.player && !k.is(ef::SOLID) {
                    let s = step(c, e, gravity, &block, bodies);
                    crushed = s.crushed;
                    if let Some(fall) = s.land {
                        if k.is(ef::FALL_DAMAGE) && fall > super::entity::SAFE_FALL {
                            land = Some(fall - super::entity::SAFE_FALL);
                        }
                    }
                }
                // Liquid temperatures where the entity is now (`in_liquid` is from before the move).
                let temps = [e.pos, e.pos + DVec3::Y * (k.height as f64 * 0.5)]
                    .map(|p| block(p.floor().as_ivec3()).and_then(|s| c.states[s as usize].liquid).map(|l| c.liquids[l as usize].temperature));
                // Like vanilla lava: 15 seconds of fire and 4 damage while inside.
                let hot = temps.iter().any(|t| t.is_some_and(|t| t >= super::liquid::BURNING));
                let wet = temps.iter().any(|t| t.is_some_and(|t| t < super::liquid::BURNING));
                if hot {
                    e.fire = e.fire.max(300);
                }
                // Sunlight sets them on fire for 8 seconds; burning hurts once a second, water puts it out.
                if k.is(ef::BURNS_IN_SUN) && day > 0.5 && e.age % crate::world::TPS as u64 == 0 && !e.in_liquid {
                    let head = (e.pos + DVec3::Y * k.eye as f64).floor().as_ivec3();
                    let sky = cubes.get(&cube_of(head)).map_or(0, |cb| cb.light(local_index(head)) & 0xFF);
                    if sky >= 15 {
                        e.fire = e.fire.max(160);
                    }
                }
                if wet && !hot {
                    e.fire = 0;
                }
                e.fire = e.fire.saturating_sub(1);
                let burn = e.fire > 0 && e.fire % crate::world::TPS == 0 && !hot;
                let despawn = c.game.categories[k.category as usize].despawn;
                if despawn > 0.0 && e.age % 100 == 0 {
                    let near = players.iter().any(|p| (*p - e.pos).length_squared() < despawn * despawn);
                    if !near {
                        e.removed = true;
                    }
                }
                if !e.item.is_empty() {
                    e.pickup_delay = e.pickup_delay.saturating_sub(1);
                    if e.age > ITEM_LIFETIME {
                        e.removed = true;
                    }
                }
                (land.is_some() || burn || hot || crushed).then_some((e.id, land, burn, hot, crushed))
            })
            .collect();
        for (i, push) in super::entity::soft_push(c, &self.entities.list) {
            self.entities.list[i].vel += push;
        }
        for &(id, land, burn, hot, crushed) in &hits {
            if let Some(amount) = land {
                self.damage(c, id, amount, 0, damage::FALL);
            }
            if crushed {
                self.damage(c, id, 1.0, 0, damage::CRUSH);
            }
            if hot {
                self.damage(c, id, 4.0, 0, damage::LAVA);
            } else if burn {
                self.damage(c, id, 1.0, 0, damage::FIRE);
            }
        }
        self.items(c);
        self.land_falling(c);
    }

    /// Systems over per-entity components: values copied into one aligned array, handed over
    /// in one call, written back (changes of synced components marked for sending).
    fn run_systems(&mut self, c: &'static Content) {
        if c.game.systems.is_empty() {
            return;
        }
        // Every entity a system works on remembers the time it was simulated to. One that fell
        // behind (unloaded, the time jumped) makes the missed ticks up right here, before its
        // normal tick: in at most `CATCH_UP_PARTS` calls whatever was missed, each followed by
        // the energy network moving that many ticks' worth, so machines that feed each other
        // keep pace (a generator does not fill its buffer and stop while the furnace waits).
        let (sim, now) = (c.game.simulated_comp, self.time);
        let worked: Vec<u64> =
            self.entities.list.iter().filter(|e| !e.removed && c.game.systems.iter().any(|s| self.entities.comps.contains_key(&(e.id, s.1)))).map(|e| e.id).collect();
        let mut missed: Vec<(u64, u64)> = Vec::new();
        for id in worked {
            let last = self.entities.comps.get(&(id, sim)).and_then(|b| b.get(..8)).map(|b| u64::from_le_bytes(b.try_into().unwrap())).filter(|&t| t < now);
            if let Some(t) = last.filter(|&t| now - t > 1) {
                missed.push((id, now - t - 1));
            }
            self.entities.comps.insert((id, sim), now.to_le_bytes().into());
        }
        let mut part = FxHashMap::default();
        for k in 0..CATCH_UP_PARTS {
            part.clear();
            for &(id, n) in &missed {
                let parts = n.min(CATCH_UP_PARTS);
                if k < parts {
                    let ticks = n * (k + 1) / parts - n * k / parts;
                    part.insert(id, ticks.min(u32::MAX as u64) as u32);
                }
            }
            if part.is_empty() {
                break;
            }
            self.systems_once(c, Some(&part));
            self.grid.tick(&mut self.entities, c, Some(&part));
        }
        self.systems_once(c, None);
    }

    /// Every system once: over `only` (entity, ticks) when given, else over all with one tick.
    fn systems_once(&mut self, c: &'static Content, only: Option<&FxHashMap<u64, u32>>) {
        for &(owner, comp, cb) in &c.game.systems {
            let size = c.game.components[comp as usize].size as usize;
            if size == 0 {
                continue;
            }
            let ids: Vec<u64> = self
                .entities
                .list
                .iter()
                .filter(|e| !e.removed && self.entities.comps.contains_key(&(e.id, comp)) && only.is_none_or(|o| o.contains_key(&e.id)))
                .map(|e| e.id)
                .collect();
            if ids.is_empty() {
                continue;
            }
            let mut buf = vec![0u64; (ids.len() * size).div_ceil(8)];
            let bytes = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, ids.len() * size) };
            for (i, id) in ids.iter().enumerate() {
                bytes[i * size..(i + 1) * size].copy_from_slice(&self.entities.comps[&(*id, comp)]);
            }
            let ticks: Vec<u32> = ids.iter().map(|id| only.and_then(|o| o.get(id).copied()).unwrap_or(1)).collect();
            let (ids_ptr, values, count, ticks) = (ids.as_ptr(), bytes.as_mut_ptr(), ids.len() as u32, ticks.as_ptr());
            self.call(owner, cb, |w| abi::SystemArgs { world: w, component: comp, ids: ids_ptr, values, count, stride: size as u32, ticks });
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, ids.len() * size) };
            for (i, id) in ids.iter().enumerate() {
                if let Some(old) = self.entities.comps.get_mut(&(*id, comp)) {
                    old.copy_from_slice(&bytes[i * size..(i + 1) * size]);
                }
            }
        }
    }

    /// Item entities: merge neighbors, get picked up by players.
    fn items(&mut self, c: &'static Content) {
        let players: Vec<(u64, DVec3)> =
            self.entities.list.iter().filter(|e| e.player && !e.removed && e.health > 0.0 && e.mode != crate::world::GameMode::Spectator).map(|e| (e.id, e.pos)).collect();
        let items: Vec<(u64, DVec3)> = self.entities.list.iter().filter(|e| !e.removed && !e.item.is_empty()).map(|e| (e.id, e.pos)).collect();
        for (id, pos) in items {
            let Some(e) = self.entities.get(id) else { continue };
            if e.pickup_delay > 0 {
                continue;
            }
            let stack = e.item;
            if let Some(&(pid, _)) = players.iter().find(|(_, pp)| (*pp + DVec3::Y * 0.5 - pos).length_squared() < 2.25) {
                let mut q = abi::Query { entity: pid, other: id, item: stack.item, amount: stack.count as f64, ..abi::Query::DEFAULT };
                if !self.ask(c, abi::question::PICKUP_ITEM, &mut q) {
                    continue;
                }
                let rest = self.entities.insert_item(c, pid, stack);
                let taken = stack.count - rest.count;
                if taken > 0 {
                    let pickup = c.game.sound_names.get("core:item.pickup").copied().unwrap_or(u32::MAX);
                    self.sound(pickup, pos, 1.0, 0.0);
                    self.emit_engine(c, abi::event::ITEM_PICKED_UP, &abi::Pickup { player: pid, stack: ItemStack { count: taken, ..stack } });
                }
                if let Some(e) = self.entities.get_mut(id) {
                    if rest.is_empty() {
                        e.removed = true;
                        self.entities.taken.push((id, pid));
                    } else {
                        e.item = rest;
                    }
                }
                continue;
            }
            if self.time % 10 == 0 {
                let max = c.max_stack(stack.item);
                let other = self.entities.list.iter().position(|o| {
                    o.id != id
                        && !o.removed
                        && o.item.item == stack.item
                        && o.item.data == stack.data
                        && o.item.count + stack.count <= max
                        && (o.pos - pos).length_squared() < 1.0
                });
                if let Some(oi) = other {
                    self.entities.list[oi].item.count += stack.count;
                    if let Some(e) = self.entities.get_mut(id) {
                        e.removed = true;
                    }
                }
            }
        }
    }

    pub fn flush_neighbors(&mut self, c: &'static Content) {
        if !c.dims[self.id as usize].block_updates {
            self.neighbor_queue.clear();
            self.edge_updates.clear();
            return;
        }
        let share = self.edge_updates.len().min(MAX_NEIGHBOR_UPDATES / 2);
        for p in self.edge_updates.drain(..share) {
            self.neighbor_queue.push_back((p, p));
        }
        let mut budget = MAX_NEIGHBOR_UPDATES;
        while let Some((p, from)) = self.neighbor_queue.pop_front() {
            if budget == 0 {
                log::warn!("neighbor update storm in dimension {}, dropping {} updates", self.id, self.neighbor_queue.len());
                self.neighbor_queue.clear();
                break;
            }
            budget -= 1;
            let Some(state) = self.get(p) else { continue };
            if !self.supported(c, p, state) {
                self.break_off(c, p, state);
                continue;
            }
            if c.states[state as usize].liquid.is_some() {
                self.liquid_neighbor(c, p, state);
                continue;
            }
            self.call_block(c, state, |b| b.on_neighbor, |w| abi::BlockEventArgs { world: w, pos: p.to_array(), state, other: from.to_array(), entity: 0 });
        }
    }

    /// The block `state` would be held up at `p` (unloaded neighbors count as holding).
    fn supported(&self, c: &'static Content, p: IVec3, state: u32) -> bool {
        let Some((side, _)) = c.states[state as usize].support else { return true };
        let at = p + DIRS[side as usize];
        let Some(n) = self.get(at) else { return true };
        if !c.supports(state, n) {
            return false;
        }
        // Something the support must stand next to (water by sugar cane); a stack of the same
        // block is held by the one at its bottom.
        match c.states[state as usize].support_near {
            Some(t) if c.states[n as usize].block != c.states[state as usize].block => {
                [IVec3::X, -IVec3::X, IVec3::Z, -IVec3::Z].iter().any(|d| self.get(at + *d).is_none_or(|s| c.block_has(s, Some(t))))
            }
            _ => true,
        }
    }

    /// Breaks a block that lost its support, dropping what it drops.
    fn break_off(&mut self, c: &'static Content, p: IVec3, state: u32) {
        self.set(c, p, AIR);
        self.block_dust(c, p, state);
        self.drop_block_loot(c, p, state, ItemStack::EMPTY, false, None);
        self.emit_engine(c, abi::event::BLOCK_BROKEN, &abi::BlockChange { player: 0, pos: p.to_array(), state, item: 0 });
    }

    /// What happened since the last settling takes effect: neighbor updates, then events.
    /// After every player's or mod's action (`Server::settle`) and at the end of a tick.
    pub fn settle(&mut self, c: &'static Content) {
        self.flush_neighbors(c);
        self.finish_tick(c);
    }

    pub fn finish_tick(&mut self, c: &'static Content) {
        if self.framed {
            self.relight();
        }
        self.deliver_events(c);
        for cp in std::mem::take(&mut self.light_dirty) {
            if let Some(cube) = self.cubes.get(&cp) {
                self.resend.push((cp, cube.clone()));
            }
        }
        if !self.resend.is_empty() {
            let resent: FxHashSet<IVec3> = self.resend.iter().map(|r| r.0).collect();
            self.changes.retain(|(p, _)| !resent.contains(&cube_of(*p)));
        }
    }

    /// Breaks a block as if mined with `tool`: asks interceptors, rolls its loot (if `drops`),
    /// tells the block and fires `block_broken`.
    pub fn break_block(&mut self, c: &'static Content, p: IVec3, tool: ItemStack, player: u64, drops: bool) -> bool {
        let Some(state) = self.get(p) else { return false };
        if state == AIR || c.states[state as usize].hardness < 0.0 {
            return false;
        }
        let mut q = abi::Query { entity: player, pos: p.to_array(), state, item: tool.item, ..abi::Query::DEFAULT };
        if !self.ask(c, abi::question::BREAK_BLOCK, &mut q) {
            return false;
        }
        // The block entity's own name goes back onto the block's item.
        let name = self.entities.anchors.get(&p).and_then(|id| self.entities.comps.get(&(*id, c.game.name_comp))).map(|b| String::from_utf8_lossy(b).into_owned());
        self.set(c, p, AIR);
        self.block_dust(c, p, state);
        self.sound(c.states[state as usize].sounds[0], p.as_dvec3() + DVec3::splat(0.5), 1.0, 0.0);
        if drops {
            self.drop_block_loot(c, p, state, tool, true, name.as_deref());
        }
        self.emit_engine(c, abi::event::BLOCK_BROKEN, &abi::BlockChange { player, pos: p.to_array(), state, item: tool.item });
        true
    }

    pub fn place(&mut self, c: &'static Content, p: IVec3, face: u8, state: u32, look: [f32; 3], player: u64, item: u32) -> bool {
        let Some(cur) = self.get(p) else { return false };
        if c.states[cur as usize].flags & bf::REPLACEABLE == 0 {
            return false;
        }
        let def = c.block_of(state);
        let mut chosen = def.first_state;
        if let Some(cb) = def.cb.placement {
            let mut args = abi::PlaceArgs { pos: p.to_array(), face: face as u32, look, state: def.first_state };
            host::invoke(def.owner, cb, &mut args);
            if args.state >= def.first_state && args.state < def.first_state + def.states {
                chosen = args.state;
            } else {
                host::misuse("placement", || format!("{} chose state {}, not one of its own", def.name, args.state));
            }
        }
        if !self.supported(c, p, chosen) {
            return false;
        }
        let mut q = abi::Query { entity: player, pos: p.to_array(), state: chosen, item, ..abi::Query::DEFAULT };
        if !self.ask(c, abi::question::PLACE_BLOCK, &mut q) {
            return false;
        }
        self.set(c, p, chosen);
        self.sound(c.states[chosen as usize].sounds[1], p.as_dvec3() + DVec3::splat(0.5), 1.0, 0.0);
        self.emit_engine(c, abi::event::BLOCK_PLACED, &abi::BlockChange { player, pos: p.to_array(), state: chosen, item });
        true
    }

    /// True if the block consumed the click.
    pub fn player_use(&mut self, c: &'static Content, p: IVec3, face: u8, player: u64) -> bool {
        let Some(state) = self.get(p) else { return false };
        let opens = c.block_of(state).opens;
        if c.block_of(state).cb.on_use.is_some() || opens.is_some() {
            let mut q = abi::Query { entity: player, pos: p.to_array(), state, ..abi::Query::DEFAULT };
            if !self.ask(c, abi::question::USE_BLOCK, &mut q) {
                // Refused uses are consumed: no item use or placement behind them.
                return true;
            }
        }
        if let Some(menu) = opens {
            // Its block entity's menu, or one of its own (a crafting table).
            let owner = self.entities.anchors.get(&p).copied().unwrap_or(0);
            self.events.push(crate::server::DimEvent::OpenMenu { player, menu, owner, block: state });
            return true;
        }
        let r = self.call_block(c, state, |b| b.on_use, |w| abi::UseArgs { world: w, pos: p.to_array(), state, face: face as u32, player, handled: 0 });
        r.is_some_and(|a| a.handled != 0)
    }

    /// The next growth step of the block at `p`, drawn like a random tick would come (an
    /// exponential wait with the block's mean) from `from`, deterministically from the place and
    /// time.
    fn schedule_growth(&mut self, c: &Content, p: IVec3, state: u32, from: u64) {
        let Some((mean, _)) = c.grows(state) else {
            self.growth.remove(&p);
            return;
        };
        self.schedule_at(p, mean, from);
    }

    /// What time does to `p` next, in `mean` ticks on average from `from` (an exponential wait
    /// drawn from the place and time, like a random tick would come).
    fn schedule_at(&mut self, p: IVec3, mean: f64, from: u64) {
        let h = hash(0x6F0 ^ from ^ (self.id as u64) << 48, p.x as i64, p.y as i64, p.z as i64);
        let u = ((h >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        let at = from + (-u.ln() * mean).max(1.0) as u64;
        self.growth.insert(p, at);
        self.growth_queue.push(Reverse((at, p.x, p.y, p.z)));
    }

    /// Covered (`smother`) or something near spreads onto it: schedules the change if it can
    /// happen now. Growing blocks have their own steps; one change waits at a time.
    fn consider(&mut self, c: &Content, p: IVec3, from: u64) {
        if self.growth.contains_key(&p) {
            return;
        }
        let Some(state) = self.get(p) else { return };
        if c.grows(state).is_some() {
            return;
        }
        let covered = self.get(p + IVec3::Y).is_some_and(|s| c.states[s as usize].flags & bf::OPAQUE != 0);
        let mean = match c.states[state as usize].smother {
            Some((_, mean)) if covered => mean,
            Some(_) => return,
            None if covered || !c.spread_target(state) => return,
            None => match self.spreaders_near(c, p, state) {
                (Some((_, mean, _)), n) => mean / n as f64,
                _ => return,
            },
        };
        self.schedule_at(p, mean, from);
    }

    /// A block that spreads onto `state` at `p` from where it reaches (1 sideways, 1 below to 3
    /// above): its state, mean ticks and light needed, and how many there are.
    fn spreaders_near(&self, c: &Content, p: IVec3, state: u32) -> (Option<(u32, f64, u32)>, u32) {
        let mut first = None;
        let mut n = 0;
        for dy in -1..=3 {
            for dz in -1..=1 {
                for dx in -1..=1 {
                    let Some(s) = self.get(p + IVec3::new(dx, dy, dz)) else { continue };
                    if let Some((tag, mean, light)) = c.states[s as usize].spread
                        && c.components.has(comps::BLOCK, state, tag)
                    {
                        first.get_or_insert((s, mean, light));
                        n += 1;
                    }
                }
            }
        }
        (first, n)
    }

    /// A scheduled change of a block that does not grow comes due at `at`: a covered block
    /// turns, an uncovered one with light above takes what spreads onto it. Too dark: it tries
    /// again later.
    fn react(&mut self, c: &'static Content, p: IVec3, state: u32, at: u64) {
        let covered = self.get(p + IVec3::Y).is_some_and(|s| c.states[s as usize].flags & bf::OPAQUE != 0);
        self.growing_at = Some(at);
        match c.states[state as usize].smother {
            Some((into, _)) if covered => {
                self.set(c, p, into);
            }
            Some(_) => {}
            None if covered => {}
            None => {
                if let (Some((into, _, need)), _) = self.spreaders_near(c, p, state) {
                    if abi::light::effective(self.light(p + IVec3::Y), self.daylight_at(c, at)) >= need {
                        self.set(c, p, into);
                    } else {
                        self.consider(c, p, at);
                    }
                }
            }
        }
        self.growing_at = None;
    }

    /// Growth steps due by now, in time order (catching up on unloaded time included).
    fn grow(&mut self, c: &'static Content) {
        while let Some(&Reverse((at, x, y, z))) = self.growth_queue.peek() {
            if at > self.time {
                break;
            }
            self.growth_queue.pop();
            let p = IVec3::new(x, y, z);
            if self.growth.get(&p) != Some(&at) {
                continue;
            }
            self.growth.remove(&p);
            let Some(state) = self.get(p) else { continue };
            let Some((_, need)) = c.grows(state) else {
                self.react(c, p, state, at);
                continue;
            };
            // Support that went away without a neighbor noticing (water two blocks off).
            if !self.supported(c, p, state) {
                self.break_off(c, p, state);
                continue;
            }
            let light = abi::light::effective(self.light(p), self.daylight_at(c, at));
            self.growing_at = Some(at);
            if light >= need {
                match c.block_of(state).cb.on_grow {
                    Some(_) => {
                        self.call_block(c, state, |b| b.on_grow, |w| abi::BlockEventArgs { world: w, pos: p.to_array(), state, other: [0; 3], entity: 0 });
                    }
                    None => {
                        if let Some(next) = c.grown(state) {
                            self.set(c, p, next);
                        }
                    }
                }
            }
            // The next step counts from this one, not from now.
            if let Some(now) = self.get(p).filter(|&s| c.grows(s).is_some()) {
                self.schedule_growth(c, p, now, at);
            }
            self.growing_at = None;
        }
    }

    pub fn schedule(&mut self, p: IVec3, delay: u32) {
        let at = self.time + delay.max(1) as u64;
        self.scheduled.push(Reverse((at, p.x, p.y, p.z)));
    }

    pub fn random(&mut self) -> u64 {
        self.rng.next()
    }

    /// Uniform in 0..1 from the same stream.
    pub fn random_unit(&mut self) -> f32 {
        self.rng.float()
    }

    /// Sound event `sound` (`u32::MAX`: nothing) for players in its range.
    pub fn sound(&mut self, sound: u32, pos: DVec3, volume: f32, pitch: f32) {
        if sound != u32::MAX {
            self.events.push(DimEvent::Sound { dim: self.id, sound, pos, volume, pitch });
        }
    }

    /// Vanilla's 4x4x4 pieces of a broken block flying apart, for everyone watching.
    pub fn block_dust(&mut self, c: &Content, p: IVec3, state: u32) {
        let Some(&kind) = c.game.particle_names.get(abi::particle::BLOCK) else { return };
        let pos = (p.as_dvec3() + DVec3::splat(0.5)).to_array();
        let burst = abi::ParticleBurst { kind, pos, spread: [0.5; 3], velocity: [0.0, 2.0, 0.0], speed: 3.0, count: 64, block: state, ..abi::ParticleBurst::DEFAULT };
        self.events.push(DimEvent::Particles { dim: self.id, burst });
    }

    /// Spawns what a block's loot table gives for `tool`; `mined`: by that tool, not by the world
    /// (support, liquids), which always harvests.
    /// `own_name`: what a renamed block entity was called, given back to the block's item.
    pub fn drop_block_loot(&mut self, c: &'static Content, p: IVec3, state: u32, tool: ItemStack, mined: bool, own_name: Option<&str>) {
        let b = c.block_of(state);
        let own_item = c.items.iter().position(|i| i.block == Some(b.first_state)).map(|i| i as u32);
        for s in self.roll_loot(c, &b.name, &crate::loot::Ctx { tool, mined: if mined { state } else { 0 }, ..Default::default() }) {
            let s = if Some(s.item) == own_item { crate::stack::with_name(s, own_name) } else { s };
            self.spawn_item(c, p.as_dvec3() + DVec3::splat(0.5), s);
        }
    }
}

/// Section = 4x4x4 cubes, the unit regions are made of.
#[inline]
pub fn section_of_cube(cube: IVec3) -> IVec3 {
    cube >> 2
}

#[inline]
pub fn section_of(p: DVec3) -> IVec3 {
    section_of_cube(cube_of(p.floor().as_ivec3()))
}

type Taken = (Entity, Option<Vec<ItemStack>>, Vec<(u32, Box<[u8]>)>);

/// Everything stored for a set of sections, moved between regions when they merge or split.
#[derive(Default)]
pub struct Parcel {
    cubes: Vec<(IVec3, Arc<Cube>, bool, bool)>,
    entities: Vec<Taken>,
    structures: Vec<(u64, Box<Dim>)>,
    scheduled: Vec<Reverse<(u64, i32, i32, i32)>>,
    growth: Vec<(IVec3, u64)>,
}

impl Dim {
    /// Sections this region occupies: loaded cubes plus where its entities stand.
    pub fn add_cube(&mut self, pos: IVec3, cube: Arc<Cube>) {
        if self.cubes.insert(pos, cube).is_none() {
            *self.section_cubes.entry(section_of_cube(pos)).or_default() += 1;
        }
    }

    fn cube_gone(&mut self, pos: IVec3) {
        let s = section_of_cube(pos);
        if let Some(n) = self.section_cubes.get_mut(&s) {
            *n -= 1;
            if *n == 0 {
                self.section_cubes.remove(&s);
            }
        }
    }

    pub fn sections(&self) -> FxHashSet<IVec3> {
        let mut s: FxHashSet<IVec3> = self.section_cubes.keys().copied().collect();
        s.extend(self.entities.list.iter().filter(|e| !e.removed).map(|e| section_of(e.pos)));
        s
    }

    /// Moves out cubes, entities and pending ticks located in sections where `take` is true.
    pub fn take_sections(&mut self, take: impl Fn(IVec3) -> bool) -> Parcel {
        let mut p = Parcel::default();
        let cubes: Vec<IVec3> = self.cubes.keys().copied().filter(|&c| take(section_of_cube(c))).collect();
        for pos in cubes {
            let cube = self.cubes.remove(&pos).unwrap();
            self.cube_gone(pos);
            self.grid.cube_unloaded(pos);
            self.conn.remove(&pos);
            p.cubes.push((pos, cube, self.modified.remove(&pos), self.settled.remove(&pos)));
        }
        let ids: Vec<u64> = self.entities.list.iter().filter(|e| take(section_of(e.pos))).map(|e| e.id).collect();
        for id in ids {
            if let Some(t) = self.entities.take(id) {
                p.entities.push(t);
            }
            if let Some(st) = self.structures.remove(&id) {
                p.structures.push((id, st));
            }
        }
        let (moved, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.scheduled)
            .into_iter()
            .partition(|Reverse((_, x, y, z))| take(section_of_cube(cube_of(IVec3::new(*x, *y, *z)))));
        self.scheduled = kept.into_iter().collect();
        p.scheduled = moved;
        p.growth = self.growth.iter().filter(|(q, _)| take(section_of_cube(cube_of(**q)))).map(|(q, at)| (*q, *at)).collect();
        for (q, _) in &p.growth {
            self.growth.remove(q);
        }
        p
    }

    pub fn absorb(&mut self, c: &Content, p: Parcel) {
        for (pos, cube, modified, settled) in p.cubes {
            self.grid.cube_loaded(c, pos, &cube);
            self.add_cube(pos, cube);
            if modified {
                self.modified.insert(pos);
            }
            if settled {
                self.settled.insert(pos);
                self.settled_gen += 1;
            }
        }
        for (e, inv, comps) in p.entities {
            self.entities.insert_with(c, e, inv, comps);
        }
        self.structures.extend(p.structures);
        self.entities.sort_by_id();
        self.scheduled.extend(p.scheduled);
        for (q, at) in p.growth {
            self.growth.insert(q, at);
            self.growth_queue.push(Reverse((at, q.x, q.y, q.z)));
        }
    }

    pub fn connectivity(&mut self, c: &Content, pos: IVec3) -> Option<u16> {
        if let Some(&v) = self.conn.get(&pos) {
            return Some(v);
        }
        let v = see_through(c, self.cubes.get(&pos)?);
        self.conn.insert(pos, v);
        Some(v)
    }

    /// Marks `pos` and the loaded cubes below it settled: nothing above will ever bring sky
    /// light (it is not going to be loaded), so their light is final as it is.
    pub fn settle_down(&mut self, pos: IVec3) {
        let mut p = pos;
        while self.cubes.contains_key(&p) && self.settled.insert(p) {
            self.settled_gen += 1;
            self.newly_settled.push(p);
            p -= IVec3::Y;
        }
    }

    pub fn set_rng(&mut self, seed: u64) {
        self.rng = Rng(seed);
    }
}
