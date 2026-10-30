//! Per-stack component data (damage taken, a battery's charge, a tank's contents). Data is
//! immutable and interned: a stack holds a `u32` handle, equal data always gets the same handle,
//! so comparing stacks stays a field compare and merging needs no special cases. Changing a value
//! makes (or finds) another handle. Handles are per process; saves and the network carry the
//! components by name.

use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use loom_abi::ItemStack;

/// Components of one stack, sorted by component id.
pub type Data = Arc<[(u32, Box<[u8]>)]>;

struct Interner {
    list: Vec<Data>,
    index: FxHashMap<Data, u32>,
}

fn table() -> &'static RwLock<Interner> {
    static T: OnceLock<RwLock<Interner>> = OnceLock::new();
    T.get_or_init(|| {
        let empty: Data = Arc::from(Vec::new());
        let mut index = FxHashMap::default();
        index.insert(empty.clone(), 0);
        RwLock::new(Interner { list: vec![empty], index })
    })
}

pub fn get(data: u32) -> Data {
    let t = table().read();
    t.list.get(data as usize).cloned().unwrap_or_else(|| t.list[0].clone())
}

/// Value of `comp` in `data`. The bytes live as long as the process (data is never freed).
pub fn value(data: u32, comp: u32) -> Option<&'static [u8]> {
    if data == 0 {
        return None;
    }
    let d = get(data);
    let i = d.binary_search_by_key(&comp, |e| e.0).ok()?;
    // Interned data is never dropped, so the boxed bytes outlive every caller.
    Some(unsafe { std::slice::from_raw_parts(d[i].1.as_ptr(), d[i].1.len()) })
}

pub fn intern(mut comps: Vec<(u32, Box<[u8]>)>) -> u32 {
    if comps.is_empty() {
        return 0;
    }
    comps.sort_by_key(|e| e.0);
    comps.dedup_by_key(|e| e.0);
    let data: Data = Arc::from(comps);
    if let Some(&id) = table().read().index.get(&data) {
        return id;
    }
    let mut t = table().write();
    if let Some(&id) = t.index.get(&data) {
        return id;
    }
    let id = t.list.len() as u32;
    t.list.push(data.clone());
    t.index.insert(data, id);
    id
}

/// `data` with `comp` set to `bytes`, or removed with `None`.
pub fn with(data: u32, comp: u32, bytes: Option<&[u8]>) -> u32 {
    let mut comps: Vec<(u32, Box<[u8]>)> = get(data).iter().filter(|e| e.0 != comp).cloned().collect();
    if let Some(b) = bytes {
        comps.push((comp, b.into()));
    }
    intern(comps)
}

pub fn u32_of(stack: &ItemStack, comp: Option<u32>) -> u32 {
    comp.and_then(|c| value(stack.data, c)).filter(|b| b.len() >= 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Damage taken (the `damage` stack component), 0 for a fresh stack.
pub fn damage(stack: &ItemStack) -> u32 {
    u32_of(stack, crate::content::try_content().and_then(|c| c.ids.damage))
}

/// The stack's own name, if a player gave it one.
pub fn name(stack: &ItemStack) -> Option<String> {
    let c = crate::content::try_content()?;
    value(stack.data, c.ids.name?).map(|b| String::from_utf8_lossy(b).into_owned())
}

pub fn with_name(stack: ItemStack, name: Option<&str>) -> ItemStack {
    let Some(comp) = crate::content::try_content().and_then(|c| c.ids.name) else { return stack };
    ItemStack { data: with(stack.data, comp, name.filter(|n| !n.is_empty()).map(str::as_bytes)), ..stack }
}

pub fn with_damage(stack: ItemStack, damage: u32) -> ItemStack {
    let Some(comp) = crate::content::try_content().and_then(|c| c.ids.damage) else { return stack };
    let bytes = damage.to_le_bytes();
    ItemStack { data: with(stack.data, comp, (damage > 0).then_some(&bytes[..])), ..stack }
}

fn without(data: u32, flag: u32) -> u32 {
    let Some(c) = crate::content::try_content() else { return data };
    if data == 0 || c.stack_flags.is_empty() {
        return data;
    }
    let d = get(data);
    if !d.iter().any(|(id, _)| c.stack_flags.get(id).is_some_and(|f| f & flag != 0)) {
        return data;
    }
    intern(d.iter().filter(|(id, _)| !c.stack_flags.get(id).is_some_and(|f| f & flag != 0)).cloned().collect())
}

/// The stack as clients may see it: without `SERVER_ONLY` components.
pub fn for_client(s: ItemStack) -> ItemStack {
    ItemStack { data: without(s.data, loom_abi::stack_flags::SERVER_ONLY), ..s }
}

/// Stack data to write to saves: without `NO_SAVE` components.
pub fn for_save(data: u32) -> u32 {
    without(data, loom_abi::stack_flags::NO_SAVE)
}

/// Components by name, for saves and the network; names the reader does not know are dropped.
pub fn to_names(data: u32) -> Vec<(String, Vec<u8>)> {
    let Some(c) = crate::content::try_content() else { return Vec::new() };
    get(data).iter().map(|(id, b)| (c.components.names[*id as usize].clone(), b.to_vec())).collect()
}

/// `damage=05000000,mymod:charge=10270000` for text files; empty for plain stacks.
pub fn to_text(data: u32) -> String {
    to_names(data).iter().map(|(n, b)| format!("{n}={}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())).collect::<Vec<_>>().join(",")
}

pub fn from_text(s: &str) -> u32 {
    let unhex = |h: &str| (0..h.len() / 2).filter_map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()).collect::<Vec<u8>>();
    from_names(s.split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), unhex(v))).collect())
}

pub fn from_names(list: Vec<(String, Vec<u8>)>) -> u32 {
    let Some(c) = crate::content::try_content() else { return 0 };
    intern(list.into_iter().filter_map(|(name, b)| c.components.id(&name).map(|id| (id, b.into_boxed_slice()))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_data_equal_handles() {
        let a = with(0, 7, Some(&5u32.to_le_bytes()));
        let b = with(with(0, 3, Some(&[1])), 7, Some(&5u32.to_le_bytes()));
        let b = with(b, 3, None);
        assert_eq!(a, b);
        assert_ne!(a, 0);
        assert_eq!(value(a, 7), Some(&5u32.to_le_bytes()[..]));
        assert_eq!(with(a, 7, None), 0);
    }
}
