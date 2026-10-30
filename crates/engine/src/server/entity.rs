//! Entities of one dimension: core state in a dense list, mod components as raw bytes, inventories
//! as an engine contract every mod can read and write.

use rayon::prelude::*;
use glam::{DVec3, IVec3};
use rustc_hash::{FxHashMap, FxHashSet};
use loom_abi::{ItemStack, entity_flags as ef};

use crate::content::Content;
use crate::physics;

pub struct Entity {
    pub id: u64,
    pub kind: u32,
    pub pos: DVec3,
    pub vel: DVec3,
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub in_liquid: bool,
    /// The last move hit something sideways.
    pub blocked: bool,
    pub fall: f32,
    pub moved: f32,
    pub health: f32,
    pub age: u64,
    pub hurt_cooldown: u32,
    /// Ticks until it may attack again (mobs).
    pub attack_cooldown: u32,
    /// AI steering requested this tick: direction, speed factor, jump.
    pub walk: Option<([f32; 2], f32, bool)>,
    /// Jump strength attribute when its walk was set (1 = vanilla's).
    pub jump_mul: f32,
    pub removed: bool,
    pub item: ItemStack,
    pub pickup_delay: u32,
    pub player: bool,
    pub mode: crate::world::GameMode,
    /// Ticks left burning.
    pub fire: u32,
    /// Hotbar slot the player holds.
    pub selected: u32,
    pub anchor: Option<IVec3>,
    pub sent: Option<(DVec3, f32, f32, bool)>,
    /// Worn items as last sent to clients.
    pub worn: Vec<u32>,
    /// The solid body it rides (stands on, or flies above within its carry height).
    pub support: Option<u64>,
    /// Its own box (width, height, eye) and another kind it looks like, set by mods.
    pub size: Option<[f32; 3]>,
    pub look: Option<u32>,
    /// Falling blocks: ticks it has lain on the ground.
    pub rested: u8,
    /// Crouching players: a 1.5 high box with lower eyes.
    pub crouched: bool,
    /// Where it walks to by itself (`Host::navigate`).
    pub nav: Option<super::path::Nav>,
}

impl Entity {
    pub fn new(id: u64, kind: u32, pos: DVec3, health: f32) -> Entity {
        Entity {
            id,
            kind,
            pos,
            vel: DVec3::ZERO,
            blocked: false,
            yaw: 0.0,
            pitch: 0.0,
            on_ground: false,
            in_liquid: false,
            fall: 0.0,
            moved: 0.0,
            health,
            age: 0,
            hurt_cooldown: 0,
            attack_cooldown: 0,
            walk: None,
            jump_mul: 1.0,
            removed: false,
            item: ItemStack::EMPTY,
            pickup_delay: 0,
            player: false,
            mode: crate::world::GameMode::Survival,
            fire: 0,
            selected: 0,
            anchor: None,
            sent: None,
            worn: Vec::new(),
            support: None,
            size: None,
            look: None,
            rested: 0,
            crouched: false,
            nav: None,
        }
    }

    /// Half width, height and eye height of its box: its own size if a mod gave it one, else
    /// its kind's; crouching lowers either.
    pub fn dims(&self, c: &Content) -> (f64, f64, f64) {
        let k = &c.game.entities[self.kind as usize];
        let [w, h, eye] = self.size.unwrap_or([k.width, k.height, k.eye]).map(|v| v as f64);
        let (h, eye) = if self.crouched { physics::crouched(h, eye) } else { (h, eye) };
        (w / 2.0, h, eye)
    }

    pub fn height(&self, c: &Content) -> f64 {
        self.dims(c).1
    }

    pub fn eye(&self, c: &Content) -> f64 {
        self.dims(c).2
    }

    /// Its collision box now.
    pub fn aabb(&self, c: &Content) -> crate::physics::Aabb {
        let (hw, h, _) = self.dims(c);
        crate::physics::Aabb::around(self.pos, hw, h)
    }

    /// Where items it throws start: just below its eyes (vanilla).
    pub fn drop_point(&self, c: &Content) -> DVec3 {
        self.pos + DVec3::Y * (self.eye(c) - 0.3)
    }

    pub fn state(&self, c: &Content, max_health: f32) -> loom_abi::EntityState {
        let k = &c.game.entities[self.kind as usize];
        loom_abi::EntityState {
            kind: self.kind,
            flags: k.flags | if self.blocked { ef::BLOCKED } else { 0 },
            pos: self.pos.to_array(),
            vel: self.vel.to_array(),
            yaw: self.yaw,
            pitch: self.pitch,
            on_ground: self.on_ground as u32,
            in_liquid: self.in_liquid as u32,
            health: self.health,
            max_health,
            age: self.age,
            fall: self.fall,
            moved: self.moved,
            size: { let (hw, h, eye) = self.dims(c); [hw as f32 * 2.0, h as f32, eye as f32] },
        }
    }
}

#[derive(Default)]
pub struct Entities {
    pub list: Vec<Entity>,
    index: FxHashMap<u64, usize>,
    pub comps: FxHashMap<(u64, u32), Box<[u8]>>,
    pub inventories: FxHashMap<u64, Vec<ItemStack>>,
    pub dirty_inv: FxHashSet<u64>,
    pub dirty_comp: FxHashSet<(u64, u32)>,
    /// Synced components as last sent, to find what changed (`find_changes`).
    sent: FxHashMap<(u64, u32), Box<[u8]>>,
    pub anchors: FxHashMap<IVec3, u64>,
    pub spawned: Vec<u64>,
    pub despawned: Vec<u64>,
    /// Killed this tick: clients play the death animation when they go.
    pub killed: Vec<u64>,
    /// Item entities picked up whole this tick, and by whom (clients pull them in).
    pub taken: Vec<(u64, u64)>,
    /// Hurt entities and where the hit came from (players move themselves, so they get told).
    /// Velocity changes for players' clients: (player, change, halve the velocity first), in
    /// blocks per tick like the client's own.
    pub pushed: Vec<(u64, ([f32; 3], bool))>,
}

/// Falls up to this many blocks do no damage.
pub const SAFE_FALL: f32 = 3.0;


impl Entities {
    pub fn get(&self, id: u64) -> Option<&Entity> {
        self.index.get(&id).map(|&i| &self.list[i]).filter(|e| !e.removed)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Entity> {
        self.index.get(&id).map(|&i| &mut self.list[i]).filter(|e| !e.removed)
    }

    pub fn insert(&mut self, c: &Content, e: Entity) {
        let id = e.id;
        let kind = &c.game.entities[e.kind as usize];
        for &comp in &kind.components {
            self.comps.entry((id, comp)).or_insert_with(|| match kind.values.iter().find(|v| v.0 == comp) {
                Some(v) => v.1.clone(),
                None => c.game.components[comp as usize].default.clone().into_boxed_slice(),
            });
        }
        if kind.slots > 0 {
            self.inventories.entry(id).or_insert_with(|| vec![ItemStack::EMPTY; kind.slots as usize]);
        }
        if e.player {
            self.inventories.entry(id).or_insert_with(|| vec![ItemStack::EMPTY; c.game.player_layout.slots as usize]);
        }
        if let Some(a) = e.anchor {
            self.anchors.insert(a, id);
        }
        self.index.insert(id, self.list.len());
        self.list.push(e);
        self.spawned.push(id);
    }

    /// `insert` with the inventory and components it had (loaded, or moved from another region).
    pub fn insert_with(&mut self, c: &Content, e: Entity, inv: Option<Vec<ItemStack>>, comps: Vec<(u32, Box<[u8]>)>) {
        let id = e.id;
        self.insert(c, e);
        if let Some(inv) = inv {
            self.inventories.insert(id, inv);
        }
        for (k, v) in comps {
            self.comps.insert((id, k), v);
        }
    }

    pub fn remove(&mut self, id: u64) {
        if let Some(e) = self.get_mut(id) {
            e.removed = true;
            if let Some(a) = e.anchor {
                self.anchors.remove(&a);
            }
        }
    }

    /// Drops removed entities and their data. Keeps list order (it is the tick order).
    pub fn compact(&mut self) {
        if !self.list.iter().any(|e| e.removed) {
            return;
        }
        let gone: FxHashSet<u64> = self.list.iter().filter(|e| e.removed).map(|e| e.id).collect();
        self.list.retain(|e| !e.removed);
        self.index = self.list.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        self.comps.retain(|(e, _), _| !gone.contains(e));
        self.sent.retain(|(e, _), _| !gone.contains(e));
        for id in &gone {
            self.inventories.remove(id);
            self.dirty_inv.remove(id);
        }
        self.despawned.extend(gone);
    }

    /// Marks synced components whose bytes differ from what was last sent, however they changed
    /// (mods write through pointers, systems like energy write directly).
    pub fn find_changes(&mut self, synced: &[bool]) {
        for (k, v) in &self.comps {
            if synced.get(k.1 as usize).copied().unwrap_or(false) && self.sent.get(k) != Some(v) {
                self.dirty_comp.insert(*k);
                self.sent.insert(*k, v.clone());
            }
        }
    }

    /// Takes an entity out entirely (moving it to another dimension).
    pub fn take(&mut self, id: u64) -> Option<(Entity, Option<Vec<ItemStack>>, Vec<(u32, Box<[u8]>)>)> {
        let i = *self.index.get(&id)?;
        let e = self.list.remove(i);
        self.index = self.list.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        let inv = self.inventories.remove(&id);
        let keys: Vec<(u64, u32)> = self.comps.keys().filter(|k| k.0 == id).copied().collect();
        let comps = keys.into_iter().filter_map(|k| self.comps.remove(&k).map(|v| (k.1, v))).collect();
        self.sent.retain(|k, _| k.0 != id);
        self.despawned.push(id);
        Some((e, inv, comps))
    }

    /// Canonical tick order after regions merge.
    pub fn sort_by_id(&mut self) {
        self.list.sort_by_key(|e| e.id);
        self.index = self.list.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
    }

    pub fn set_slot(&mut self, owner: u64, slot: u32, stack: ItemStack) {
        if let Some(inv) = self.inventories.get_mut(&owner) {
            if let Some(s) = inv.get_mut(slot as usize) {
                *s = if stack.count == 0 { ItemStack::EMPTY } else { stack };
                self.dirty_inv.insert(owner);
            }
        }
    }

    /// Merges into matching stacks, then empty slots. Returns the rest.
    pub fn insert_item(&mut self, c: &Content, owner: u64, mut stack: ItemStack) -> ItemStack {
        let max = c.max_stack(stack.item);
        // Pickups never go into a player's equipment.
        let worn = if self.get(owner).is_some_and(|e| e.player) { c.game.player_layout.worn.clone() } else { 0..0 };
        let Some(inv) = self.inventories.get_mut(&owner) else { return stack };
        for (_, s) in inv.iter_mut().enumerate().filter(|(i, _)| !worn.contains(&(*i as u32))) {
            if stack.count == 0 {
                break;
            }
            if s.item == stack.item && s.data == stack.data && s.count < max && !s.is_empty() {
                let n = (max - s.count).min(stack.count);
                s.count += n;
                stack.count -= n;
            }
        }
        for (_, s) in inv.iter_mut().enumerate().filter(|(i, _)| !worn.contains(&(*i as u32))) {
            if stack.count == 0 {
                break;
            }
            if s.is_empty() {
                let n = max.min(stack.count);
                *s = ItemStack { count: n, ..stack };
                stack.count -= n;
            }
        }
        self.dirty_inv.insert(owner);
        if stack.count == 0 { ItemStack::EMPTY } else { stack }
    }

    pub fn near(&self, pos: DVec3, radius: f64, kind: u32) -> Vec<u64> {
        let r2 = radius * radius;
        self.list
            .iter()
            .filter(|e| !e.removed && (kind == u32::MAX || e.kind == kind) && (e.pos - pos).length_squared() <= r2)
            .map(|e| e.id)
            .collect()
    }
}

/// What a step did besides moving: the fall it landed from, and whether a crushing body
/// squeezed it into blocks.
pub struct Stepped {
    pub land: Option<f32>,
    pub crushed: bool,
}

/// One physics step (1/20 s) for a non-player entity. `block` gives states, `None` = unloaded;
/// `bodies` are the solid bodies that moved this tick before it.
pub fn step(c: &Content, e: &mut Entity, gravity: f64, block: &impl Fn(IVec3) -> Option<u32>, bodies: &[physics::Body]) -> Stepped {
    const DT: f64 = crate::world::TICK;
    let k = &c.game.entities[e.kind as usize];
    let mut out = Stepped { land: None, crushed: false };
    if e.anchor.is_some() {
        return out;
    }
    if k.is(ef::KINEMATIC) {
        e.pos += e.vel * DT;
        e.moved = (e.vel.with_y(0.0) * DT).length() as f32;
        return out;
    }
    let (hw, h, _) = e.dims(c);
    // The body it rides moves it first (blocks still stop it), then bodies that ran into it push.
    let ride = physics::ride(e.pos, hw, &mut e.support, bodies);
    if ride.carry != DVec3::ZERO {
        e.pos = physics::move_box(c, block, e.pos, hw, h, ride.carry, 0.0).pos;
    }
    if let Some(d) = ride.released {
        e.vel += d / DT;
    }
    let (shift, crushes) = physics::push(c, e.pos, hw, h, bodies, e.id, e.support);
    if shift != DVec3::ZERO {
        // Blocks stop the push; a crushing body hurts what it cannot push away.
        let to = physics::move_box(c, block, e.pos, hw, h, shift, 0.0).pos;
        out.crushed = crushes && (to - e.pos - shift).length_squared() > 1e-6;
        e.pos = to;
    }
    // After riding: falls and walking are measured relative to the body.
    let start = e.pos;
    // The step players take, in blocks per tick: a mob walks at its target speed on plain
    // ground, faster on ice, slower in lava, and climbs ladders it walks into.
    let mut ctl = physics::Control { swim: k.is(ef::SWIMS), ..Default::default() };
    if let Some((dir, speed, jump)) = e.walk.take() {
        let target = k.speed as f64 * speed as f64 * DT;
        ctl.wish = DVec3::new(dir[0] as f64, 0.0, dir[1] as f64);
        // Vanilla's ground friction keeps 0.546 of the speed a tick; this much a tick holds it.
        ctl.ground = target * (1.0 - 0.6 * 0.91);
        ctl.air = 0.02;
        if jump {
            ctl.jump = 0.42 * e.jump_mul as f64;
            ctl.swim = true;
        }
    }
    let frame = physics::Frame { half_width: hw, height: h, step: if e.item.is_empty() { 0.6 } else { 0.0 }, gravity: if k.is(ef::NO_GRAVITY) { 0.0 } else { gravity * DT * DT } };
    let mut v = e.vel * DT;
    let r = physics::walk(c, block, bodies, e.id, &mut e.pos, &mut v, e.on_ground, &ctl, &frame);
    e.vel = v / DT;
    e.in_liquid = r.in_liquid;
    e.blocked = r.hit[0] || r.hit[2];
    let was_ground = e.on_ground;
    e.on_ground = r.on_ground;
    if r.on_ground {
        if let Some(b) = physics::standing_on(c, e.pos, hw, bodies, e.id) {
            e.support = Some(b);
        }
    }
    let dy = e.pos.y - start.y;
    e.moved = ((e.pos.x - start.x).powi(2) + (e.pos.z - start.z).powi(2)).sqrt() as f32;
    if e.on_ground || e.in_liquid {
        if !was_ground && e.fall > 0.0 && !e.in_liquid {
            out.land = Some(e.fall);
        }
        e.fall = 0.0;
    } else if dy < 0.0 {
        e.fall += (-dy) as f32;
    }
    out
}

/// Soft bodies (`PUSHABLE`) overlapping each other drift apart like vanilla mobs: a push of up
/// to 1 block/s per neighbor. `pushers` may push without being pushed (players move on their
/// clients). Returns the velocity change per entity.
/// Vanilla's `maxEntityCramming`.
const MAX_CRAMMING: u32 = 24;

pub fn soft_push(c: &Content, list: &[Entity]) -> Vec<(usize, DVec3)> {
    let soft = |e: &Entity| {
        let k = &c.game.entities[e.kind as usize];
        !e.removed && e.item.is_empty() && e.anchor.is_none() && k.is(ef::PUSHABLE) && !k.is(ef::SOLID)
    };
    let mut cells: FxHashMap<(i32, i32), Vec<usize>> = FxHashMap::default();
    for (i, e) in list.iter().enumerate().filter(|(_, e)| soft(e)) {
        cells.entry(((e.pos.x / 2.0).floor() as i32, (e.pos.z / 2.0).floor() as i32)).or_default().push(i);
    }
    list.par_iter().with_min_len(256).enumerate().filter(|(_, e)| soft(e) && !e.player).filter_map(|(i, e)| {
        let (cx, cz) = ((e.pos.x / 2.0).floor() as i32, (e.pos.z / 2.0).floor() as i32);
        let mut push = DVec3::ZERO;
        // Each mob feels at most this many others (vanilla's cramming limit), so a thousand in
        // one spot cost a thousand times that, not a million pairs.
        let (mut felt, mut looked) = (0, 0);
        'near: for dz in -1..=1 {
            for dx in -1..=1 {
                for &j in cells.get(&(cx + dx, cz + dz)).into_iter().flatten() {
                    looked += 1;
                    if felt == MAX_CRAMMING || looked > 4 * MAX_CRAMMING {
                        break 'near;
                    }
                    let o = &list[j];
                    let reach = e.dims(c).0 + o.dims(c).0;
                    let d = e.pos - o.pos;
                    if j == i || d.x.abs() >= reach || d.z.abs() >= reach || d.y >= o.height(c) || -d.y >= e.height(c) {
                        continue;
                    }
                    // Exactly on top of each other: split by id so they still drift apart.
                    let d = if d.x.abs() + d.z.abs() < 1e-4 { DVec3::new(if e.id < o.id { -0.01 } else { 0.01 }, d.y, 0.0) } else { d };
                    push += physics::nudge(d);
                    felt += 1;
                }
            }
        }
        (push != DVec3::ZERO).then_some((i, push * crate::world::TPS as f64))
    })
    .collect()
}
