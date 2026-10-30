//! Components: typed values attached to content entries (block states, items, entity kinds,
//! biomes, dimensions), one namespace for every mod. A tag is a component without a value.
//! Names without a prefix are the common vocabulary (`plant_soil`, `fuel`, `ingots/iron`),
//! `modid:` ones are private to a mod.
//!
//! At freeze every name gets an id (sorted, so ids only depend on the set of names) and each entry
//! kind becomes a table: one bitset row per entry (has component) and one dense column per
//! component that carries values on that kind. A check is one load and a bit test; mods read the
//! same tables directly through `abi::ComponentTable`.

use rustc_hash::FxHashMap;
use loom_abi::{self as abi, ModId, Str, entry_kind as ek};

pub const BLOCK: usize = ek::BLOCK as usize;
pub const ITEM: usize = ek::ITEM as usize;
pub const ENTITY: usize = ek::ENTITY as usize;
pub const BIOME: usize = ek::BIOME as usize;
pub const DIMENSION: usize = ek::DIMENSION as usize;
pub const KINDS: usize = ek::COUNT;

/// Schemas of the components the engine reads itself.
pub const BUILTIN: &[(&str, &str)] = &[
    (abi::comp::MAX_STACK, "u32"),
    (abi::comp::DURABILITY, "u32"),
    (abi::comp::ATTACK_DAMAGE, "f32"),
    (abi::comp::MINING_SPEED, "f32"),
    (abi::comp::TOOL_LEVEL, "u32"),
    (abi::comp::HARVEST_LEVEL, "u32"),
    (abi::comp::DAMAGE, "u32"),
    (abi::comp::HARDNESS, "f32"),
    (abi::comp::FRICTION, "f32"),
    (abi::comp::DRAG, "f32"),
    (abi::comp::LIGHT, "[u8;3]"),
    (abi::comp::TINT, "tint"),
    (abi::comp::TINT_FACES, "u8"),
    (abi::comp::SUPPORT_FACE, "u8"),
    (abi::comp::SUPPORT, "tag"),
    (abi::comp::SUPPORT_NEAR, "tag"),
    (abi::comp::SPREADS_TO, "tag"),
    (abi::comp::SPREAD_TIME, "f32"),
    (abi::comp::SPREAD_LIGHT, "u32"),
    (abi::comp::SMOTHERED_INTO, "block"),
    (abi::comp::SMOTHER_TIME, "f32"),
    (abi::comp::AMBIENT_PARTICLE, "str"),
    (abi::comp::CONDUCTOR, ""),
    (abi::comp::ENERGY, <abi::Energy as abi::Schema>::SCHEMA),
    (abi::comp::WORN, <abi::Worn as abi::Schema>::SCHEMA),
    (abi::comp::WORN_TEXTURE, "str"),
    (abi::comp::WEAR_PER_HIT, "u32"),
    (abi::attribute::MAX_HEALTH, "f64"),
];

/// One value as a mod registered it; `text` holds the copied string of a `str` value.
#[derive(Clone, Debug)]
pub struct RawValue {
    pub name: String,
    pub schema: String,
    pub bytes: Vec<u8>,
    pub text: Option<String>,
}

impl RawValue {
    /// # Safety
    /// The value's pointers must be valid for this call.
    pub unsafe fn copy(v: &abi::CompValue) -> RawValue {
        unsafe {
            let schema = v.schema.as_str().to_string();
            let bytes = v.value.as_slice().to_vec();
            let text = (written_as_text(&schema) && bytes.len() == size_of::<Str>()).then(|| std::ptr::read_unaligned(bytes.as_ptr() as *const Str).as_str().to_string());
            RawValue { name: v.name.as_str().to_string(), schema, bytes, text }
        }
    }

    pub fn tag(name: &str) -> RawValue {
        RawValue { name: name.to_string(), schema: String::new(), bytes: Vec::new(), text: None }
    }

    /// # Safety
    /// The slice must hold valid values for this call.
    pub unsafe fn list(s: abi::Slice<abi::CompValue>) -> Vec<RawValue> {
        unsafe { s.as_slice().iter().map(|v| RawValue::copy(v)).collect() }
    }
}

#[derive(Clone, Debug)]
pub enum Target {
    /// Block (all states, or those with `[prop=value]`) and item of that name.
    Name(String),
    /// Blocks and items with a tag matching the pattern (`*` matches any text).
    Tagged(String),
    Entity(String),
    Biome(String),
    Dimension(String),
}

pub struct RawSet {
    pub owner: ModId,
    pub target: Target,
    pub value: RawValue,
    pub remove: bool,
}

/// Schemas a mod writes as text: `str` stays text; references (`block`: one block or state,
/// `blocks`: a weighted list `name[:weight] ..`, `item`: an item, `tag`: `#name`, `tint`: a tint
/// kind) are resolved into ids at freeze, with errors.
pub fn written_as_text(schema: &str) -> bool {
    matches!(schema, "str" | "block" | "blocks" | "item" | "tag" | "tint")
}

/// Byte size a schema implies, if it is a primitive.
fn primitive_size(schema: &str) -> Option<usize> {
    Some(match schema {
        "" => 0,
        "u8" | "i8" | "bool" => 1,
        "u16" | "i16" => 2,
        "u32" | "i32" | "f32" | "block" | "blocks" | "item" | "tag" | "tint" => 4,
        "u64" | "i64" | "f64" => 8,
        "str" => size_of::<Str>(),
        _ => return None,
    })
}

struct Table {
    entries: u32,
    words: u32,
    bits: Vec<u64>,
    /// Per component: stride and data (empty when the kind has no values for it).
    cols: Vec<(u32, Vec<u8>)>,
}

impl Table {
    fn new(entries: usize, components: usize) -> Table {
        let words = components.div_ceil(64).max(1);
        Table { entries: entries as u32, words: words as u32, bits: vec![0; entries * words], cols: (0..components).map(|_| (0, Vec::new())).collect() }
    }

    #[inline]
    fn has(&self, entry: u32, comp: u32) -> bool {
        entry < self.entries && (self.bits[(entry * self.words + comp / 64) as usize] >> (comp % 64)) & 1 != 0
    }

    fn value(&self, entry: u32, comp: u32) -> Option<&[u8]> {
        let (stride, data) = &self.cols[comp as usize];
        (self.has(entry, comp) && *stride > 0).then(|| &data[(entry * stride) as usize..((entry + 1) * stride) as usize])
    }
}

pub struct Components {
    pub names: Vec<String>,
    pub schemas: Vec<String>,
    by_name: FxHashMap<String, u32>,
    tables: Vec<Table>,
    /// Strings `str` values point into; boxed so the pointers stay put.
    _texts: Vec<Box<str>>,
    _columns: Vec<Vec<abi::Column>>,
    abi: Box<abi::ComponentTable>,
}

unsafe impl Send for Components {}
unsafe impl Sync for Components {}

impl Components {
    pub fn id(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }

    pub fn count(&self) -> u32 {
        self.names.len() as u32
    }

    #[inline]
    pub fn has(&self, kind: usize, entry: u32, comp: u32) -> bool {
        (comp as usize) < self.names.len() && self.tables[kind].has(entry, comp)
    }

    /// Value bytes, if the entry has the component and it carries a value.
    pub fn bytes(&self, kind: usize, entry: u32, comp: u32) -> Option<&[u8]> {
        if comp as usize >= self.names.len() {
            return None;
        }
        self.tables[kind].value(entry, comp)
    }

    /// Typed read; the caller knows the schema (engine built-ins).
    pub fn get<T: Copy>(&self, kind: usize, entry: u32, comp: u32) -> Option<T> {
        self.bytes(kind, entry, comp).filter(|b| b.len() >= size_of::<T>()).map(|b| unsafe { std::ptr::read_unaligned(b.as_ptr() as *const T) })
    }

    pub fn text(&self, kind: usize, entry: u32, comp: u32) -> Option<&str> {
        (self.schemas.get(comp as usize).map(String::as_str) == Some("str")).then(|| self.get::<Str>(kind, entry, comp)).flatten().map(|s| unsafe { s.as_str() })
    }

    /// Component ids an entry has, in id order.
    pub fn of(&self, kind: usize, entry: u32) -> impl Iterator<Item = u32> + '_ {
        let t = &self.tables[kind];
        let row: &[u64] = if entry < t.entries { &t.bits[(entry * t.words) as usize..((entry + 1) * t.words) as usize] } else { &[] };
        row.iter().enumerate().flat_map(|(w, &bits)| (0..64).filter(move |b| bits >> b & 1 != 0).map(move |b| w as u32 * 64 + b))
    }

    pub fn is_tag(&self, comp: u32) -> bool {
        self.schemas.get(comp as usize).is_some_and(|s| s.is_empty())
    }

    pub fn table(&self) -> *const abi::ComponentTable {
        &*self.abi
    }
}

/// Collects schemas and values during freeze and turns them into `Components`.
pub struct Builder {
    /// Name -> (schema, who declared it first).
    decl: FxHashMap<String, (String, String)>,
    names: Vec<String>,
    ids: FxHashMap<String, u32>,
    tables: Vec<Table>,
    texts: Vec<Box<str>>,
    /// Mod that last set a value through `r.set`, for override logs.
    setters: FxHashMap<(usize, u32, u32), String>,
    /// Values that could not be stored (freeze errors).
    pub errors: Vec<String>,
    /// Turns the text of a reference value (`block`, `blocks`) into its bytes.
    pub refs: Option<Box<dyn Fn(&str, &str) -> Result<Vec<u8>, String>>>,
}

impl Default for Builder {
    fn default() -> Self {
        let mut b = Builder { decl: FxHashMap::default(), names: Vec::new(), ids: FxHashMap::default(), tables: Vec::new(), texts: Vec::new(), setters: FxHashMap::default(), errors: Vec::new(), refs: None };
        for (name, schema) in BUILTIN {
            b.decl.insert(name.to_string(), (schema.to_string(), "the engine".into()));
        }
        b
    }
}

impl Builder {
    /// Records a use of `name` with `schema`; uses must agree. `who` names the mod and entry for
    /// the error.
    pub fn declare(&mut self, name: &str, schema: &str, bytes: Option<usize>, who: &dyn Fn() -> String, errors: &mut Vec<String>) {
        if name.is_empty() {
            errors.push(format!("{}: component without a name", who()));
            return;
        }
        if let (Some(n), Some(want)) = (bytes, primitive_size(schema)) {
            if n != want {
                errors.push(format!("{}: component {name} is {schema} ({want} bytes) but the value has {n} bytes", who()));
                return;
            }
        }
        match self.decl.get(name) {
            Some((s, first)) if s != schema => {
                let shown = |s: &str| if s.is_empty() { "a tag".to_string() } else { format!("'{s}'") };
                errors.push(format!("{}: component {name} used as {} but {first} uses it as {}", who(), shown(schema), shown(s)));
            }
            Some(_) => {}
            None => {
                self.decl.insert(name.to_string(), (schema.to_string(), who()));
            }
        }
    }

    /// Declares a value's component, and the tag a `tag` value names (it exists by being named).
    pub fn declare_value(&mut self, v: &RawValue, who: &dyn Fn() -> String, errors: &mut Vec<String>) {
        self.declare(&v.name, &v.schema, v.text.is_none().then_some(v.bytes.len()), who, errors);
        if let (Some(t), "tag") = (&v.text, v.schema.as_str()) {
            match crate::refs::tag_name(t) {
                Ok(tag) => self.declare(tag, "", None, who, errors),
                Err(e) => errors.push(format!("{}: {}: {e}", who(), v.name)),
            }
        }
    }

    /// Assigns ids and allocates the tables; values go in with `put` afterwards.
    pub fn start(&mut self, entries: [usize; KINDS]) {
        let mut names: Vec<String> = self.decl.keys().cloned().collect();
        names.sort();
        self.ids = names.iter().enumerate().map(|(i, n)| (n.clone(), i as u32)).collect();
        self.tables = entries.iter().map(|&n| Table::new(n, names.len())).collect();
        self.names = names;
    }

    pub fn id(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Entries of `kind` carrying any of `comps`: one masked compare per row word.
    pub fn entries_with_any(&self, kind: usize, comps: &[u32]) -> Vec<u32> {
        let t = &self.tables[kind];
        let mut mask = vec![0u64; t.words as usize];
        for &c in comps {
            mask[(c / 64) as usize] |= 1 << (c % 64);
        }
        (0..t.entries).filter(|&e| t.bits[(e * t.words) as usize..((e + 1) * t.words) as usize].iter().zip(&mask).any(|(b, m)| b & m != 0)).collect()
    }

    fn encode<'v>(&mut self, v: &'v RawValue) -> Result<std::borrow::Cow<'v, [u8]>, String> {
        match &v.text {
            Some(t) if v.schema == "tag" => {
                let tag = crate::refs::tag_name(t)?;
                self.id(tag).map(|id| id.to_le_bytes().to_vec().into()).ok_or_else(|| format!("unknown tag {tag}"))
            }
            Some(t) if v.schema != "str" => match &self.refs {
                Some(resolve) => resolve(&v.schema, t).map(Into::into),
                None => Err("references resolve only at load".into()),
            },
            Some(t) => {
                let boxed: Box<str> = t.as_str().into();
                let s = Str { ptr: boxed.as_ptr(), len: boxed.len() };
                self.texts.push(boxed);
                Ok(unsafe { std::slice::from_raw_parts(&s as *const Str as *const u8, size_of::<Str>()).to_vec().into() })
            }
            None => Ok(v.bytes.as_slice().into()),
        }
    }

    /// Sets a value (or tag) on an entry. `setter` is the mod of an `r.set` (None for an entry's own
    /// registration); replacing another mod's different value is logged.
    pub fn put(&mut self, kind: usize, entry: u32, v: &RawValue, setter: Option<(ModId, &str)>, label: &dyn Fn() -> String) {
        let Some(comp) = self.id(&v.name) else { return };
        let bytes = match self.encode(v) {
            Ok(b) => b,
            Err(e) => {
                self.errors.push(format!("{}: {}: {e}", label(), v.name));
                return;
            }
        };
        let t = &mut self.tables[kind];
        if entry >= t.entries {
            return;
        }
        let had = t.has(entry, comp);
        if let Some((_, mod_name)) = setter {
            let changed = t.value(entry, comp).is_some_and(|old| old != &*bytes);
            if had && changed {
                let before = self.setters.get(&(kind, entry, comp)).map_or("its registration", String::as_str);
                log::info!("{mod_name} overrides {} on {} (was set by {before})", v.name, label());
            }
            self.setters.insert((kind, entry, comp), mod_name.to_string());
        }
        let t = &mut self.tables[kind];
        if !bytes.is_empty() {
            let (stride, data) = &mut t.cols[comp as usize];
            if *stride == 0 {
                *stride = bytes.len() as u32;
                data.resize(t.entries as usize * bytes.len(), 0);
            }
            if bytes.len() != *stride as usize {
                // Marking it present with no value would read as zero.
                self.errors.push(format!("{}: {} has a value of {} bytes, others have {}", label(), v.name, bytes.len(), stride));
                return;
            }
            let at = (entry * *stride) as usize;
            data[at..at + bytes.len()].copy_from_slice(&bytes);
        }
        let w = (entry * t.words + comp / 64) as usize;
        t.bits[w] |= 1 << (comp % 64);
    }

    pub fn remove(&mut self, kind: usize, entry: u32, name: &str) {
        let Some(comp) = self.id(name) else { return };
        let t = &mut self.tables[kind];
        if entry < t.entries {
            t.bits[(entry * t.words + comp / 64) as usize] &= !(1 << (comp % 64));
        }
    }

    /// Copies every component of `from` that `to` lacks (block items get their block's).
    pub fn inherit(&mut self, from: (usize, u32), to: (usize, u32)) {
        let src = &self.tables[from.0];
        if from.1 >= src.entries {
            return;
        }
        let row = (from.1 * src.words) as usize;
        let set: Vec<u32> = (0..src.words as usize).flat_map(|w| {
            let bits = src.bits[row + w];
            (0..64u32).filter(move |b| bits >> b & 1 != 0).map(move |b| w as u32 * 64 + b)
        }).collect();
        for comp in set {
            if self.tables[to.0].has(to.1, comp) {
                continue;
            }
            let value = self.tables[from.0].value(from.1, comp).map(<[u8]>::to_vec);
            let t = &mut self.tables[to.0];
            t.bits[(to.1 * t.words + comp / 64) as usize] |= 1 << (comp % 64);
            if let Some(bytes) = value {
                let (stride, data) = &mut t.cols[comp as usize];
                if *stride == 0 {
                    *stride = bytes.len() as u32;
                    data.resize(t.entries as usize * bytes.len(), 0);
                }
                if bytes.len() == *stride as usize {
                    let at = (to.1 * *stride) as usize;
                    data[at..at + bytes.len()].copy_from_slice(&bytes);
                }
            }
        }
    }

    pub fn finish(self) -> Components {
        let Builder { decl, names, ids, tables, texts, .. } = self;
        let schemas: Vec<String> = names.iter().map(|n| decl[n].0.clone()).collect();
        let columns: Vec<Vec<abi::Column>> = tables.iter().map(|t| t.cols.iter().map(|(stride, data)| abi::Column { data: data.as_ptr(), stride: *stride }).collect()).collect();
        let kinds: [abi::KindTable; KINDS] = std::array::from_fn(|k| {
            let t = &tables[k];
            abi::KindTable { entries: t.entries, words: t.words, bits: t.bits.as_ptr(), columns: columns[k].as_ptr() }
        });
        let abi = Box::new(abi::ComponentTable { components: names.len() as u32, kinds });
        Components { names, schemas, by_name: ids, tables, _texts: texts, _columns: columns, abi }
    }
}

/// `*` in a pattern matches any text: `ingots/*`, `*_ore`.
pub fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, post)) => name.len() >= pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(name: &str, schema: &str, v: u32) -> RawValue {
        RawValue { name: name.into(), schema: schema.into(), bytes: v.to_le_bytes().to_vec(), text: None }
    }

    #[test]
    fn schemas_must_agree() {
        let mut b = Builder::default();
        let mut errors = Vec::new();
        b.declare("fuel", "u32", Some(4), &|| "base: base:coal".into(), &mut errors);
        b.declare("fuel", "u32", Some(4), &|| "testmod: testmod:peat".into(), &mut errors);
        assert!(errors.is_empty());
        b.declare("fuel", "f32", Some(4), &|| "other: other:wood".into(), &mut errors);
        b.declare("max_stack", "", None, &|| "other: other:thing".into(), &mut errors);
        b.declare("burn", "u32", Some(8), &|| "other: other:x".into(), &mut errors);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].contains("fuel") && errors[0].contains("other:wood") && errors[0].contains("base:coal"));
    }

    /// Block references become ids at freeze; one that names nothing is an error, not a zero.
    #[test]
    fn references_resolve_or_fail() {
        let mut b = Builder::default();
        let mut errors = Vec::new();
        b.declare("leaves", "block", None, &|| "a".into(), &mut errors);
        b.start([1, 0, 0, 0, 0]);
        b.refs = Some(Box::new(|_, t| if t == "a:oak_leaves" { Ok(7u32.to_le_bytes().to_vec()) } else { Err(format!("unknown block {t}")) }));
        let v = |t: &str| RawValue { name: "leaves".into(), schema: "block".into(), bytes: vec![], text: Some(t.into()) };
        b.put(BLOCK, 0, &v("a:oak_leaves"), None, &|| "log".into());
        b.put(BLOCK, 0, &v("a:oak_leafs"), None, &|| "log".into());
        assert_eq!(b.errors, vec!["log: leaves: unknown block a:oak_leafs".to_string()]);
        let c = b.finish();
        assert_eq!(c.get::<u32>(BLOCK, 0, c.id("leaves").unwrap()), Some(7));
    }

    #[test]
    fn tables_hold_bits_values_and_text() {
        let mut b = Builder::default();
        let mut errors = Vec::new();
        b.declare("fuel", "u32", Some(4), &|| "a".into(), &mut errors);
        b.declare("plant_soil", "", Some(0), &|| "a".into(), &mut errors);
        b.declare("label", "str", None, &|| "a".into(), &mut errors);
        b.start([3, 2, 0, 0, 0]);
        b.put(BLOCK, 1, &value("fuel", "u32", 300), None, &|| "x".into());
        b.put(BLOCK, 2, &RawValue::tag("plant_soil"), None, &|| "x".into());
        b.put(ITEM, 0, &RawValue { name: "label".into(), schema: "str".into(), bytes: vec![], text: Some("hi".into()) }, None, &|| "x".into());
        b.inherit((BLOCK, 1), (ITEM, 1));
        b.put(BLOCK, 1, &value("fuel", "u32", 100), Some((ModId(1), "testmod")), &|| "x".into());
        let c = b.finish();
        let (fuel, soil, label) = (c.id("fuel").unwrap(), c.id("plant_soil").unwrap(), c.id("label").unwrap());
        assert_eq!(c.get::<u32>(BLOCK, 1, fuel), Some(100));
        assert_eq!(c.get::<u32>(ITEM, 1, fuel), Some(300));
        assert!(c.has(BLOCK, 2, soil) && !c.has(BLOCK, 1, soil) && !c.has(BLOCK, 7, soil));
        assert_eq!(c.text(ITEM, 0, label), Some("hi"));
        assert_eq!(c.of(BLOCK, 2).collect::<Vec<_>>(), vec![soil]);
        // The table mods read says the same.
        let t = unsafe { &*c.table() };
        let k = &t.kinds[BLOCK];
        let word = unsafe { *k.bits.add((2 * k.words + soil / 64) as usize) };
        assert_eq!(word >> (soil % 64) & 1, 1);
    }

    #[test]
    fn patterns() {
        assert!(matches("ingots/*", "ingots/iron"));
        assert!(!matches("ingots/*", "gems/diamond"));
        assert!(matches("*_ore", "tin_ore"));
        assert!(matches("logs", "logs"));
    }
}
