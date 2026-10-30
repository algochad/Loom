//! The `core:energy` contract: any mod's generator, cable and machine work together without
//! knowing each other. Cables are blocks tagged `core:conductor`; block entities next to a cable
//! with a `core:energy` component join its network. Networks are rebuilt only when a conductor or
//! an endpoint changes, and balanced in one pass per tick: total offer vs total demand, like
//! Factorio's electric networks, instead of ticking every cable block.

use glam::IVec3;
use rustc_hash::{FxHashMap, FxHashSet};
use loom_abi::Energy;

use super::entity::Entities;
use crate::content::Content;
use crate::world::{Cube, DIRS};

#[derive(Default)]
pub struct Grid {
    conductors: FxHashSet<IVec3>,
    nets: Vec<Vec<u64>>,
    pub dirty: bool,
}

pub fn read(e: &Entities, c: &Content, id: u64) -> Option<Energy> {
    let b = e.comps.get(&(id, c.game.energy))?;
    Some(unsafe { std::ptr::read_unaligned(b.as_ptr() as *const Energy) })
}

pub fn write(e: &mut Entities, c: &Content, id: u64, v: Energy) {
    if let Some(b) = e.comps.get_mut(&(id, c.game.energy)) {
        unsafe { std::ptr::write_unaligned(b.as_mut_ptr() as *mut Energy, v) };
    }
}

pub fn insert(e: &mut Entities, c: &Content, id: u64, amount: i64, simulate: bool) -> i64 {
    let Some(mut v) = read(e, c, id) else { return 0 };
    let n = amount.clamp(0, v.max_in.max(0)).min((v.capacity - v.stored).max(0));
    if !simulate && n > 0 {
        v.stored += n;
        write(e, c, id, v);
    }
    n
}

pub fn extract(e: &mut Entities, c: &Content, id: u64, amount: i64, simulate: bool) -> i64 {
    let Some(mut v) = read(e, c, id) else { return 0 };
    let n = amount.clamp(0, v.max_out.max(0)).min(v.stored.max(0));
    if !simulate && n > 0 {
        v.stored -= n;
        write(e, c, id, v);
    }
    n
}

impl Grid {
    pub fn cube_loaded(&mut self, c: &Content, pos: IVec3, cube: &Cube) {
        if !cube.palette().iter().any(|&s| c.states[s as usize].conductor) {
            return;
        }
        for i in 0..4096usize {
            if c.states[cube.get(i) as usize].conductor {
                self.conductors.insert(pos * 16 + crate::world::cube::offset(i));
            }
        }
        self.dirty = true;
    }

    pub fn cube_unloaded(&mut self, pos: IVec3) {
        let before = self.conductors.len();
        self.conductors.retain(|p| *p >> 4 != pos);
        self.dirty |= before != self.conductors.len();
    }

    pub fn block_changed(&mut self, c: &Content, p: IVec3, old: u32, new: u32) {
        let (a, b) = (c.states[old as usize].conductor, c.states[new as usize].conductor);
        if b {
            self.conductors.insert(p);
        } else if a {
            self.conductors.remove(&p);
        }
        // Endpoints appear and disappear with block entities next to cables.
        if a || b || DIRS.iter().any(|d| self.conductors.contains(&(p + *d))) {
            self.dirty = true;
        }
    }

    fn rebuild(&mut self, e: &Entities, c: &Content) {
        self.dirty = false;
        self.nets.clear();
        let mut seen: FxHashSet<IVec3> = FxHashSet::default();
        let mut order: Vec<IVec3> = self.conductors.iter().copied().collect();
        order.sort_by_key(|p| (p.x, p.y, p.z));
        for start in order {
            if !seen.insert(start) {
                continue;
            }
            let mut stack = vec![start];
            let mut ends: Vec<u64> = Vec::new();
            while let Some(p) = stack.pop() {
                for d in DIRS {
                    let n = p + d;
                    if self.conductors.contains(&n) {
                        if seen.insert(n) {
                            stack.push(n);
                        }
                    } else if let Some(&id) = e.anchors.get(&n) {
                        if e.comps.contains_key(&(id, c.game.energy)) {
                            ends.push(id);
                        }
                    }
                }
            }
            ends.sort();
            ends.dedup();
            if ends.len() > 1 {
                self.nets.push(ends);
            }
        }
    }

    /// `catch_up`: a part of a catch-up, the machines making up ticks and how many (their rates
    /// count that many times); the others sit it out.
    pub fn tick(&mut self, e: &mut Entities, c: &Content, catch_up: Option<&FxHashMap<u64, u32>>) {
        if self.dirty {
            self.rebuild(e, c);
        }
        for net in &self.nets {
            let mut offer: Vec<(u64, i64)> = Vec::new();
            let mut want: Vec<(u64, i64)> = Vec::new();
            for &id in net {
                let Some(v) = read(e, c, id) else { continue };
                let n = catch_up.map_or(1, |m| m.get(&id).copied().unwrap_or(0)) as i64;
                if v.max_out > 0 && v.stored > 0 {
                    offer.push((id, (v.max_out * n).min(v.stored)));
                }
                if v.max_in > 0 && v.stored < v.capacity {
                    want.push((id, (v.max_in * n).min(v.capacity - v.stored)));
                }
            }
            // An entity that both offers and wants (a battery) should not feed itself.
            let total_offer: i64 = offer.iter().map(|o| o.1).sum();
            let total_want: i64 = want.iter().map(|w| w.1).sum();
            let moved = total_offer.min(total_want);
            if moved <= 0 {
                continue;
            }
            for (list, sign) in [(&offer, -1i64), (&want, 1i64)] {
                let total: i64 = list.iter().map(|x| x.1).sum();
                let mut left = moved;
                let shares: Vec<(u64, i64)> = list
                    .iter()
                    .map(|&(id, amount)| {
                        let s = (amount as i128 * moved as i128 / total.max(1) as i128) as i64;
                        left -= s;
                        (id, s)
                    })
                    .collect();
                let mut extra: FxHashMap<u64, i64> = FxHashMap::default();
                for &(id, amount) in list.iter() {
                    if left == 0 {
                        break;
                    }
                    let s = shares.iter().find(|s| s.0 == id).map_or(0, |s| s.1);
                    if s < amount {
                        *extra.entry(id).or_default() += 1;
                        left -= 1;
                    }
                }
                for (id, s) in shares {
                    let d = s + extra.get(&id).copied().unwrap_or(0);
                    if let Some(mut v) = read(e, c, id) {
                        v.stored += sign * d;
                        write(e, c, id, v);
                    }
                }
            }
        }
    }
}
