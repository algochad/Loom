//! Liquids flow in the engine, following modern vanilla's FlowingFluid step by step: a liquid
//! block reacts to neighbor changes by scheduling a tick after its delay; a tick recomputes the
//! level from the neighbors, falls if it can and otherwise spreads sideways only towards the
//! nearest way down within `slope_find`. Mods describe liquids and reactions as data
//! (`abi::LiquidDesc`, `abi::ReactionDesc`), nothing calls into a mod per update.

use glam::{DVec3, IVec3};
use loom_abi::{ItemStack, block_flags as bf};

use super::dim::Dim;
use crate::content::{AIR, Content, LiquidDef};
use crate::world::DIRS;

/// North, east, south, west: vanilla's horizontal order, which decides ties.
const HORIZONTAL: [IVec3; 4] = [IVec3::new(0, 0, -1), IVec3::new(1, 0, 0), IVec3::new(0, 0, 1), IVec3::new(-1, 0, 0)];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fluid {
    id: u16,
    /// 1..8, 8 for sources and falling liquid.
    amount: u32,
    source: bool,
    falling: bool,
}

fn decode(c: &Content, state: u32) -> Option<Fluid> {
    let id = c.states[state as usize].liquid?;
    let i = state - c.liquids[id as usize].first_state;
    let (source, falling) = (i == 0, i >= 8);
    Some(Fluid { id, amount: if source || falling { 8 } else { 8 - i }, source, falling })
}

fn encode(def: &LiquidDef, amount: u32, falling: bool) -> u32 {
    if falling {
        def.first_state + 8
    } else {
        def.first_state + 8 - amount.clamp(1, 8)
    }
}

impl Dim {
    fn fluid(&self, c: &Content, p: IVec3) -> Option<Fluid> {
        self.get(p).and_then(|s| decode(c, s))
    }

    /// Room for liquid: neither solid nor a ladder (unloaded counts as solid).
    fn holds_liquid(&self, c: &Content, p: IVec3) -> bool {
        self.get(p).is_some_and(|s| c.states[s as usize].flags & (bf::SOLID | bf::CLIMBABLE) == 0)
    }

    fn solid(&self, c: &Content, p: IVec3) -> bool {
        self.get(p).is_none_or(|s| c.states[s as usize].flags & bf::SOLID != 0)
    }

    /// What liquid `l` would make of `p` from its neighbors: a state, or `None` for nothing.
    fn new_liquid(&self, c: &Content, p: IVec3, l: u16, down: IVec3) -> Option<u32> {
        let def = &c.liquids[l as usize];
        let (mut max, mut sources) = (0, 0);
        for d in HORIZONTAL {
            if let Some(f) = self.fluid(c, p + d).filter(|f| f.id == l) {
                sources += f.source as u32;
                max = max.max(f.amount);
            }
        }
        if sources >= def.renewable {
            let under = self.fluid(c, p + down);
            if self.solid(c, p + down) || under.is_some_and(|f| f.id == l && f.source) {
                return Some(def.first_state);
            }
        }
        if self.fluid(c, p - down).is_some_and(|f| f.id == l) {
            return Some(encode(def, 8, true));
        }
        let k = max as i32 - def.drop_off as i32;
        (k > 0).then(|| encode(def, k as u32, false))
    }

    fn is_hole(&self, c: &Content, below: IVec3, l: u16) -> bool {
        self.fluid(c, below).is_some_and(|f| f.id == l) || self.holds_liquid(c, below)
    }

    fn passes(&self, c: &Content, p: IVec3, l: u16) -> bool {
        !self.fluid(c, p).is_some_and(|f| f.id == l && f.source) && self.holds_liquid(c, p)
    }

    /// Whether `l` may flow into `p` from direction `dir`: into empty room, or down into another
    /// liquid it reacts with.
    fn can_spread_to(&self, c: &Content, p: IVec3, l: u16, falling_in: bool) -> bool {
        match self.fluid(c, p) {
            Some(f) if f.id == l => false,
            Some(_) => falling_in && self.falls_into(c, l, p).is_some(),
            None => self.holds_liquid(c, p),
        }
    }

    fn falls_into(&self, c: &Content, l: u16, p: IVec3) -> Option<u32> {
        let state = self.get(p)?;
        let mine = c.liquids[l as usize].first_state;
        c.reactions.iter().find(|r| r.falls_into.is_some() && c.matches(&r.liquid, mine) && c.matches(&r.touching, state)).and_then(|r| r.falls_into)
    }

    fn spread_to(&mut self, c: &'static Content, p: IVec3, state: u32, falling_in: bool, l: u16) {
        let Some(old) = self.get(p) else { return };
        if c.states[old as usize].liquid.is_some() {
            if falling_in {
                if let Some(b) = self.falls_into(c, l, p) {
                    self.set(c, p, b);
                }
            }
            return;
        }
        self.set(c, p, state);
        if old != AIR {
            // Plants and torches wash away with their drops, like vanilla.
            self.drop_block_loot(c, p, old, ItemStack::EMPTY, false, None);
        }
    }

    fn slope_distance(&self, c: &Content, p: IVec3, depth: u32, from: IVec3, l: u16, down: IVec3) -> u32 {
        let mut min = 1000;
        for d in HORIZONTAL {
            if d == from {
                continue;
            }
            let n = p + d;
            if self.passes(c, n, l) {
                if self.is_hole(c, n + down, l) {
                    return depth;
                }
                if depth < c.liquids[l as usize].slope_find {
                    min = min.min(self.slope_distance(c, n, depth + 1, -d, l, down));
                }
            }
        }
        min
    }

    /// Directions to spread in: the ones nearest to a way down, each with the state it gets.
    fn spread_dirs(&self, c: &Content, p: IVec3, l: u16, down: IVec3) -> Vec<(IVec3, u32)> {
        let mut min = 1000;
        let mut out = Vec::new();
        for d in HORIZONTAL {
            let n = p + d;
            if !self.passes(c, n, l) {
                continue;
            }
            let Some(state) = self.new_liquid(c, n, l, down) else { continue };
            let slope = if self.is_hole(c, n + down, l) { 0 } else { self.slope_distance(c, n, 1, -d, l, down) };
            if slope < min {
                out.clear();
            }
            if slope <= min {
                out.push((d, state));
                min = slope;
            }
        }
        out
    }

    fn spread_sides(&mut self, c: &'static Content, p: IVec3, f: Fluid, down: IVec3) {
        let def = &c.liquids[f.id as usize];
        let amount = if f.falling { 7 } else { f.amount as i32 - def.drop_off as i32 };
        if amount <= 0 {
            return;
        }
        for (d, state) in self.spread_dirs(c, p, f.id, down) {
            if self.can_spread_to(c, p + d, f.id, false) {
                self.spread_to(c, p + d, state, false, f.id);
            }
        }
    }

    /// The scheduled tick of a liquid block.
    pub(super) fn liquid_tick(&mut self, c: &'static Content, p: IVec3, state: u32) {
        let Some(mut f) = decode(c, state) else { return };
        let def = &c.liquids[f.id as usize];
        let down = if def.rises { IVec3::Y } else { IVec3::NEG_Y };
        if !f.source {
            match self.new_liquid(c, p, f.id, down) {
                None => {
                    self.set(c, p, AIR);
                    return;
                }
                Some(s) if s != state => {
                    self.set(c, p, s);
                    f = decode(c, s).unwrap_or(f);
                }
                _ => {}
            }
        }
        let below = p + down;
        if self.can_spread_to(c, below, f.id, true) {
            let state = self.new_liquid(c, below, f.id, down).unwrap_or(encode(def, 8, true));
            self.spread_to(c, below, state, true, f.id);
            let sources = HORIZONTAL.iter().filter(|&&d| self.fluid(c, p + d).is_some_and(|n| n.id == f.id && n.source)).count();
            if sources >= 3 {
                self.spread_sides(c, p, f, down);
            }
        } else if f.source || !self.is_hole(c, below, f.id) {
            self.spread_sides(c, p, f, down);
        }
    }

    /// A neighbor of a liquid changed: react with what touches it, else flow after the delay.
    pub(super) fn liquid_neighbor(&mut self, c: &'static Content, p: IVec3, state: u32) {
        let Some(f) = decode(c, state) else { return };
        let def = &c.liquids[f.id as usize];
        let down = if def.rises { IVec3::Y } else { IVec3::NEG_Y };
        for d in DIRS {
            // What lies in the direction it flows does not react with it (lava over water).
            if d == down {
                continue;
            }
            let Some(n) = self.get(p + d) else { continue };
            let hit = c.reactions.iter().find(|r| c.matches(&r.liquid, state) && c.matches(&r.touching, n));
            if let Some(b) = hit.and_then(|r| if f.source { r.source } else { r.flowing }) {
                self.set(c, p, b);
                return;
            }
        }
        self.schedule(p, def.delay);
    }

    /// Bucket logic for `player` holding `held`: pour a liquid item, or fill an empty container
    /// from a source it looks at. The new held stack, or `None` if nothing happened.
    pub fn use_container(&mut self, c: &'static Content, player: u64, held: ItemStack) -> Option<ItemStack> {
        if held.is_empty() {
            return None;
        }
        let creative = self.entities.get(player).is_some_and(|e| e.mode == crate::world::GameMode::Creative);
        if let Some(&(l, empty)) = c.item_liquid.get(&held.item) {
            let hit = self.look_ray(c, player, 5.0, false).filter(|h| h.frame == 0)?;
            let replaceable = |s: u32| c.states[s as usize].flags & bf::REPLACEABLE != 0;
            let target = if replaceable(hit.state) { hit.block } else { hit.block + DIRS[hit.face as usize] };
            if !self.get(target).is_some_and(replaceable) {
                return None;
            }
            self.spread_to(c, target, c.liquids[l as usize].first_state, false, l);
            return Some(if creative || empty == 0 { held } else { ItemStack::new(empty, 1) });
        }
        let hit = self.look_ray(c, player, 5.0, true).filter(|h| h.frame == 0)?;
        let f = decode(c, hit.state).filter(|f| f.source)?;
        let filled = *c.containers.get(&(held.item, f.id))?;
        self.set(c, hit.block, AIR);
        if creative {
            return Some(held);
        }
        if held.count == 1 {
            return Some(ItemStack::new(filled, 1));
        }
        let rest = self.entities.insert_item(c, player, ItemStack::new(filled, 1));
        if !rest.is_empty() {
            let pos = self.entities.get(player).map_or(DVec3::ZERO, |e| e.pos);
            self.spawn_item(c, pos, rest);
        }
        Some(ItemStack { count: held.count - 1, ..held })
    }

    /// The block an entity looks at from its eyes, in the world or a moving structure.
    pub fn look_ray(&self, c: &Content, id: u64, max: f64, liquids: bool) -> Option<crate::physics::Hit> {
        let e = self.entities.get(id)?;
        let eye = e.pos + DVec3::Y * e.eye(c);
        let (sy, cy) = (e.yaw as f64).sin_cos();
        let (sp, cp) = (e.pitch as f64).sin_cos();
        let dir = DVec3::new(-sy * cp, sp, -cy * cp);
        let structures: Vec<(u64, DVec3, &dyn crate::physics::Blocks)> = self
            .structures
            .iter()
            .filter_map(|(&sid, s)| self.entities.get(sid).map(|b| (sid, super::structure::origin(b.pos), &**s as &dyn crate::physics::Blocks)))
            .collect();
        crate::physics::pick(c, &|p| self.get(p), &structures, eye, dir, max, liquids)
    }
}

/// Liquids from this hot burn what touches them.
pub const BURNING: u32 = 1000;
