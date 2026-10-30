//! Entity serialization for saves. Kinds, items and components are stored by name so a save
//! survives mods being added, removed or reordered.

use glam::{DVec3, IVec3};
use loom_abi::{ItemStack, component_flags as cf};

use super::entity::Entity;
use crate::content::Content;

/// An entity read back: itself, its inventory, components and, for a moving structure, its
/// blocks.
pub type Saved = (Entity, Option<Vec<ItemStack>>, Vec<(u32, Box<[u8]>)>, Option<Vec<(IVec3, u32)>>);
pub type ToSave<'a> = (&'a Entity, Option<&'a Vec<ItemStack>>, Vec<(u32, &'a [u8])>, Option<Vec<(IVec3, String)>>);

struct W<'a>(&'a mut Vec<u8>);

impl W<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u16(s.len() as u16);
        self.0.extend_from_slice(s.as_bytes());
    }
    fn stack(&mut self, c: &Content, s: ItemStack) {
        if s.is_empty() {
            self.str("");
            return;
        }
        self.str(&c.items[s.item as usize].name);
        self.u32(s.count);
        let data = crate::stack::to_names(crate::stack::for_save(s.data));
        self.u32(data.len() as u32);
        for (name, bytes) in &data {
            self.str(name);
            self.u32(bytes.len() as u32);
            self.0.extend_from_slice(bytes);
        }
    }
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        Some(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn stack(&mut self, c: &Content) -> Option<ItemStack> {
        let name = self.str()?;
        if name.is_empty() {
            return Some(ItemStack::EMPTY);
        }
        let count = self.u32()?;
        let n = self.u32()?;
        let mut list = Vec::new();
        for _ in 0..n.min(64) {
            let name = self.str()?;
            let len = self.u32()? as usize;
            list.push((name, self.take(len)?.to_vec()));
        }
        let data = crate::stack::from_names(list);
        // Items of removed mods are dropped rather than turned into something else.
        Some(c.item_names.get(&name).map_or(ItemStack::EMPTY, |&i| ItemStack { item: i, count, data }))
    }
}

pub fn write(c: &Content, out: &mut Vec<u8>, list: &[ToSave<'_>]) {
    let mut w = W(out);
    w.u32(list.len() as u32);
    for (e, inv, comps, blocks) in list {
        w.str(&c.game.entities[e.kind as usize].name);
        w.u64(e.id);
        for v in e.pos.to_array().into_iter().chain(e.vel.to_array()) {
            w.f64(v);
        }
        w.f32(e.yaw);
        w.f32(e.pitch);
        w.f32(e.health);
        w.u64(e.age);
        w.stack(c, e.item);
        match e.anchor {
            Some(a) => {
                w.u8(1);
                for v in a.to_array() {
                    w.u32(v as u32);
                }
            }
            None => w.u8(0),
        }
        match inv {
            Some(inv) => {
                w.u16(inv.len() as u16);
                for s in inv.iter() {
                    w.stack(c, *s);
                }
            }
            None => w.u16(u16::MAX),
        }
        let saved: Vec<&(u32, &[u8])> = comps.iter().filter(|(id, _)| c.game.components[*id as usize].flags & cf::SAVE != 0).collect();
        w.u16(saved.len() as u16);
        for (id, bytes) in saved {
            w.str(&c.game.components[*id as usize].name);
            w.u16(bytes.len() as u16);
            w.0.extend_from_slice(bytes);
        }
        match blocks {
            Some(list) => {
                w.u32(list.len() as u32);
                for (p, name) in list {
                    for v in p.to_array() {
                        w.u32(v as u32);
                    }
                    w.str(name);
                }
            }
            None => w.u32(u32::MAX),
        }
    }
}

pub fn read(c: &Content, data: &[u8]) -> Vec<Saved> {
    let mut r = R(data);
    let mut out = Vec::new();
    let Some(n) = r.u32() else { return out };
    for _ in 0..n {
        let Some(e) = read_one(c, &mut r) else { break };
        if let Some(e) = e {
            out.push(e);
        }
    }
    out
}

fn read_one(c: &Content, r: &mut R) -> Option<Option<Saved>> {
    let kind_name = r.str()?;
    let id = r.u64()?;
    let mut v = [0.0f64; 6];
    for x in v.iter_mut() {
        *x = r.f64()?;
    }
    let yaw = r.f32()?;
    let pitch = r.f32()?;
    let health = r.f32()?;
    let age = r.u64()?;
    let item = r.stack(c)?;
    let anchor = if r.u8()? == 1 {
        Some(IVec3::new(r.u32()? as i32, r.u32()? as i32, r.u32()? as i32))
    } else {
        None
    };
    let inv_len = r.u16()?;
    let inv = if inv_len == u16::MAX {
        None
    } else {
        let mut v = Vec::with_capacity(inv_len as usize);
        for _ in 0..inv_len {
            v.push(r.stack(c)?);
        }
        Some(v)
    };
    let nc = r.u16()?;
    let mut comps = Vec::new();
    for _ in 0..nc {
        let name = r.str()?;
        let len = r.u16()? as usize;
        let bytes = r.take(len)?.to_vec();
        if let Some(id) = c.game.component(&name) {
            let mut b = bytes;
            // Size 0: variable length (attribute modifiers).
            let size = c.game.components[id as usize].size as usize;
            if size > 0 {
                b.resize(size, 0);
            }
            comps.push((id, b.into_boxed_slice()));
        }
    }
    let nb = r.u32()?;
    let blocks = if nb == u32::MAX {
        None
    } else {
        let mut v = Vec::with_capacity(nb as usize);
        for _ in 0..nb {
            let p = IVec3::new(r.u32()? as i32, r.u32()? as i32, r.u32()? as i32);
            // Blocks of removed mods are left out.
            if let Some(s) = c.state_by_name(&r.str()?) {
                v.push((p, s));
            }
        }
        Some(v)
    };
    let Some(&kind) = c.game.entity_names.get(&kind_name) else { return Some(None) };
    let mut e = Entity::new(id, kind, DVec3::new(v[0], v[1], v[2]), health);
    e.vel = DVec3::new(v[3], v[4], v[5]);
    e.yaw = yaw;
    e.pitch = pitch;
    e.age = age;
    e.item = item;
    e.anchor = anchor;
    if kind == c.game.item_kind && item.is_empty() {
        return Some(None);
    }
    Some(Some((e, inv, comps, blocks)))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}
