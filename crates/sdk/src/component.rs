//! Components and tags: typed data on blocks, items, entity kinds, biomes and dimensions, shared
//! between mods by name. Names without a prefix are the common vocabulary (`fuel`, `plant_soil`,
//! `ingots/iron`); `mymod:name` is private to a mod.
//!
//! ```ignore
//! pub static FUEL: Component<u32> = Component::named("fuel");
//! pub static PLANT_SOIL: Tag = Tag::named("plant_soil");
//! r.item(&PEAT).set(&FUEL, 800);
//! r.set("#logs", &FUEL, 300);                        // every block and item tagged `logs`
//! if let Some(ticks) = FUEL.on_item(stack.item) { .. }
//! if PLANT_SOIL.on_block(w.get(below)) { .. }
//! ```
//! Lookups read the engine's frozen tables directly (one load and a bit test, no host call).
//! Every use of a name must agree on its value type; the engine checks that at startup and names
//! the mod and entry that disagree.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::abi::{self, ItemStack, Str, entry_kind as ek};
use crate::host;

/// A type a component can hold: plain data with a schema string the engine compares between mods.
/// Implemented for numbers, `bool`, `Text`, a few engine structs, and any struct declared with
/// [`value!`](crate::value).
///
/// # Safety
/// The type must be `repr(C)` plain data valid for whatever bytes another mod wrote with the same
/// schema.
pub unsafe trait Value: Copy + 'static {
    fn schema() -> String;
}

macro_rules! primitive {
    ($($t:ty => $s:literal),*) => {
        $(unsafe impl Value for $t {
            fn schema() -> String { $s.into() }
        })*
    };
}
primitive!(u8 => "u8", i8 => "i8", u16 => "u16", i16 => "i16", u32 => "u32", i32 => "i32", u64 => "u64", i64 => "i64", f32 => "f32", f64 => "f64", bool => "bool");

unsafe impl<T: Value, const N: usize> Value for [T; N] {
    fn schema() -> String {
        format!("[{};{N}]", T::schema())
    }
}

unsafe impl Value for abi::Worn {
    fn schema() -> String {
        <abi::Worn as abi::Schema>::SCHEMA.into()
    }
}

unsafe impl Value for abi::Energy {
    fn schema() -> String {
        <abi::Energy as abi::Schema>::SCHEMA.into()
    }
}

/// Text owned by the engine (`str` schema). Set it with the builders' `set_text`.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Text(Str);

impl Text {
    /// A text value from a static string.
    pub const fn new(s: &'static str) -> Text {
        Text(Str::new(s))
    }
    pub fn as_str(&self) -> &'static str {
        // Values read from the frozen tables live as long as the game.
        unsafe { self.0.as_str() }
    }
}

unsafe impl Value for Text {
    fn schema() -> String {
        "str".into()
    }
}

/// Values written as text and read as something else: `Text` stays text, `BlockRef`,
/// `BlockList` and `ItemRef` name things that the engine resolves when the game loads (a typo is
/// a load error).
pub trait TextValue: Value {}
impl TextValue for Text {}
impl TextValue for BlockRef {}
impl TextValue for ItemRef {}
impl TextValue for BlockList {}
impl TextValue for TagRef {}
impl TextValue for TintRef {}

/// A tag a component names (`#plant_soil`), read back as the tag's component id.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TagRef(pub u32);

unsafe impl Value for TagRef {
    fn schema() -> String {
        "tag".into()
    }
}

/// A tint kind a component names (`core:grass`), read back as its number.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TintRef(pub u32);

unsafe impl Value for TintRef {
    fn schema() -> String {
        "tint".into()
    }
}

/// A block (or a state, `base:furnace[lit=true]`) a component names: `set_text(&TREE_LEAVES,
/// "oak_leaves")`, read back as its state id.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockRef(pub u32);

unsafe impl Value for BlockRef {
    fn schema() -> String {
        "block".into()
    }
}

/// An item a component names (`set_text(&REMAINDER, "bucket")`), read back as its id.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ItemRef(pub u32);

unsafe impl Value for ItemRef {
    fn schema() -> String {
        "item".into()
    }
}

/// Blocks with weights a component names: `set_text(&TREE_KINDS, "birch_log:1 oak_log:4")`
/// (weight 1 when left out), resolved when the game loads.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockList(pub u32);

unsafe impl Value for BlockList {
    fn schema() -> String {
        "blocks".into()
    }
}

impl BlockList {
    /// (state, weight) pairs.
    pub fn entries(&self) -> &'static [[u32; 2]] {
        unsafe { (host().ref_list)(self.0).as_slice() }
    }
    /// One state by its weight, `roll` being any random number.
    pub fn pick(&self, roll: u64) -> Option<u32> {
        let e = self.entries();
        let total: u64 = e.iter().map(|x| x[1] as u64).sum();
        if total == 0 {
            return None;
        }
        let mut r = roll % total;
        for x in e {
            if r < x[1] as u64 {
                return Some(x[0]);
            }
            r -= x[1] as u64;
        }
        None
    }
}

/// Declares a struct usable as a component value; its schema lists the fields, so two mods that
/// declare the same struct agree without depending on each other.
///
/// ```ignore
/// loom_sdk::value! {
///     pub struct Food { pub hunger: u32, pub saturation: f32 }
/// }
/// ```
#[macro_export]
macro_rules! value {
    ($(#[$m:meta])* $vis:vis struct $name:ident { $($(#[$fm:meta])* $fvis:vis $field:ident : $ty:ty),* $(,)? }) => {
        $(#[$m])*
        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        $vis struct $name { $($(#[$fm])* $fvis $field: $ty),* }
        unsafe impl $crate::Value for $name {
            fn schema() -> String {
                let fields: Vec<String> = vec![$(format!("{}:{}", stringify!($field), <$ty as $crate::Value>::schema())),*];
                format!("{{{}}}", fields.join(","))
            }
        }
    };
}

static TABLE: AtomicPtr<abi::ComponentTable> = AtomicPtr::new(std::ptr::null_mut());

fn table() -> Option<&'static abi::ComponentTable> {
    let mut p = TABLE.load(Ordering::Relaxed);
    if p.is_null() {
        p = unsafe { (host().components)() } as *mut _;
        if p.is_null() {
            return None;
        }
        TABLE.store(p, Ordering::Relaxed);
    }
    Some(unsafe { &*p })
}

#[inline]
fn has(kind: u32, entry: u32, comp: u32) -> bool {
    let Some(t) = table() else { return false };
    let k = &t.kinds[kind as usize];
    if entry >= k.entries || comp >= t.components {
        return false;
    }
    unsafe { *k.bits.add((entry * k.words + comp / 64) as usize) >> (comp % 64) & 1 != 0 }
}

const UNRESOLVED: u32 = u32::MAX - 1;
const MISSING: u32 = u32::MAX;
const KINDS: usize = ek::COUNT;

static NO_BITS: u64 = 0;

/// Where one component sits in one kind's table, cached in its handle on first use so a check
/// is one dependent load.
struct Slot {
    /// The word holding this component's bit in entry 0's row; null until filled.
    bits: AtomicPtr<u64>,
    words: AtomicU32,
    entries: AtomicU32,
    data: AtomicPtr<u8>,
    stride: AtomicU32,
}

impl Slot {
    const fn new() -> Slot {
        Slot { bits: AtomicPtr::new(std::ptr::null_mut()), words: AtomicU32::new(0), entries: AtomicU32::new(0), data: AtomicPtr::new(std::ptr::null_mut()), stride: AtomicU32::new(0) }
    }
}

struct Lookup {
    id: AtomicU32,
    slots: [Slot; KINDS],
}

impl Lookup {
    const fn new() -> Lookup {
        Lookup { id: AtomicU32::new(UNRESOLVED), slots: [const { Slot::new() }; KINDS] }
    }

    fn id(&self, name: &str, schema: &dyn Fn() -> String) -> u32 {
        let id = self.id.load(Ordering::Relaxed);
        if id != UNRESOLVED {
            return id;
        }
        if table().is_none() {
            // Registration is still running: nothing to look up yet.
            return MISSING;
        }
        let id = unsafe { (host().find_component)(Str::new(name), Str::new(&schema())) };
        self.id.store(id, Ordering::Relaxed);
        id
    }

    #[inline]
    fn has(&self, kind: u32, entry: u32, name: &str, schema: &dyn Fn() -> String) -> bool {
        let s = &self.slots[kind as usize];
        let mut bits = s.bits.load(Ordering::Acquire);
        if bits.is_null() {
            bits = self.fill(kind, name, schema);
            if bits.is_null() {
                return false;
            }
        }
        entry < s.entries.load(Ordering::Relaxed) && unsafe { *bits.add((entry * s.words.load(Ordering::Relaxed)) as usize) } >> (self.id.load(Ordering::Relaxed) % 64) & 1 != 0
    }

    #[inline]
    fn read<T: Copy>(&self, kind: u32, entry: u32, name: &str, schema: &dyn Fn() -> String) -> Option<T> {
        if !self.has(kind, entry, name, schema) {
            return None;
        }
        let s = &self.slots[kind as usize];
        let stride = s.stride.load(Ordering::Relaxed);
        (stride as usize == size_of::<T>()).then(|| unsafe { std::ptr::read_unaligned(s.data.load(Ordering::Relaxed).add((entry * stride) as usize) as *const T) })
    }

    #[cold]
    fn fill(&self, kind: u32, name: &str, schema: &dyn Fn() -> String) -> *mut u64 {
        let id = self.id(name, schema);
        let Some(t) = table() else { return std::ptr::null_mut() };
        let s = &self.slots[kind as usize];
        let k = &t.kinds[kind as usize];
        let bits = if id < t.components {
            let col = unsafe { *k.columns.add(id as usize) };
            s.words.store(k.words, Ordering::Relaxed);
            s.entries.store(k.entries, Ordering::Relaxed);
            s.data.store(col.data as *mut u8, Ordering::Relaxed);
            s.stride.store(col.stride, Ordering::Relaxed);
            unsafe { k.bits.add((id / 64) as usize) as *mut u64 }
        } else {
            // Nothing uses it: every check says no.
            &NO_BITS as *const u64 as *mut u64
        };
        s.bits.store(bits, Ordering::Release);
        bits
    }
}

/// Typed handle to a component, found by name the first time it is used. Component and tag
/// names are a vocabulary shared by all mods, not things a mod owns: `fuel` means the same
/// to everyone and is used as written, a `modid:` prefix only keeps a term to one mod.
pub struct Component<T: Value> {
    name: &'static str,
    lookup: Lookup,
    _p: PhantomData<fn() -> T>,
}

impl<T: Value> Component<T> {
    pub const fn named(name: &'static str) -> Self {
        Component { name, lookup: Lookup::new(), _p: PhantomData }
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
    /// `u32::MAX` if nothing uses it (or before the registries are frozen).
    #[inline]
    pub fn id(&self) -> u32 {
        self.lookup.id(self.name, &T::schema)
    }
    #[inline]
    pub fn on_block(&self, state: u32) -> Option<T> {
        self.lookup.read(ek::BLOCK, state, self.name, &T::schema)
    }
    #[inline]
    pub fn on_item(&self, item: u32) -> Option<T> {
        self.lookup.read(ek::ITEM, item, self.name, &T::schema)
    }
    #[inline]
    pub fn on_entity(&self, kind: u32) -> Option<T> {
        self.lookup.read(ek::ENTITY, kind, self.name, &T::schema)
    }
    #[inline]
    pub fn on_biome(&self, biome: u32) -> Option<T> {
        self.lookup.read(ek::BIOME, biome, self.name, &T::schema)
    }
    #[inline]
    pub fn on_dimension(&self, dim: u32) -> Option<T> {
        self.lookup.read(ek::DIMENSION, dim, self.name, &T::schema)
    }
    /// The stack's own value (damage taken, charge ...), else its item's.
    pub fn on_stack(&self, stack: &ItemStack) -> Option<T> {
        self.stack_only(stack).or_else(|| self.on_item(stack.item))
    }
    /// Only what the stack itself carries.
    pub fn stack_only(&self, stack: &ItemStack) -> Option<T> {
        if stack.data == 0 {
            return None;
        }
        let mut len = 0usize;
        let p = unsafe { (host().stack_get)(stack.data, self.id(), &mut len) };
        (!p.is_null() && len == size_of::<T>()).then(|| unsafe { std::ptr::read_unaligned(p as *const T) })
    }
    pub(crate) fn raw_value(&self, v: &T) -> crate::content::Pending {
        crate::content::Pending::value(self.name, T::schema(), pod_bytes(v))
    }
}

fn no_schema() -> String {
    String::new()
}

/// A component without a value: membership in a group (`plant_soil`, `logs`, `ingots/iron`).
pub struct Tag {
    name: &'static str,
    lookup: Lookup,
}

impl Tag {
    pub const fn named(name: &'static str) -> Self {
        Tag { name, lookup: Lookup::new() }
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
    #[inline]
    pub fn id(&self) -> u32 {
        self.lookup.id(self.name, &no_schema)
    }
    #[inline]
    pub fn on_block(&self, state: u32) -> bool {
        self.lookup.has(ek::BLOCK, state, self.name, &no_schema)
    }
    #[inline]
    pub fn on_item(&self, item: u32) -> bool {
        self.lookup.has(ek::ITEM, item, self.name, &no_schema)
    }
    #[inline]
    pub fn on_entity(&self, kind: u32) -> bool {
        self.lookup.has(ek::ENTITY, kind, self.name, &no_schema)
    }
    #[inline]
    pub fn on_biome(&self, biome: u32) -> bool {
        self.lookup.has(ek::BIOME, biome, self.name, &no_schema)
    }
    #[inline]
    pub fn on_dimension(&self, dim: u32) -> bool {
        self.lookup.has(ek::DIMENSION, dim, self.name, &no_schema)
    }
}

/// By name, for rare lookups; keep a static `Tag` in hot code.
pub fn block_has_tag(state: u32, tag: &str) -> bool {
    let id = unsafe { (host().find_component)(Str::new(tag), Str::new("")) };
    has(ek::BLOCK, state, id)
}

pub fn item_has_tag(item: u32, tag: &str) -> bool {
    let id = unsafe { (host().find_component)(Str::new(tag), Str::new("")) };
    has(ek::ITEM, item, id)
}

pub(crate) fn pod_bytes<T: Copy>(v: &T) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, size_of::<T>()).to_vec() }
}

/// Stack data (see `ItemStack::data`) beyond what `Component::on_stack` reads.
/// The stack's own name, if a player gave it one (`with_name`).
pub fn stack_name(stack: &ItemStack) -> Option<String> {
    let id = NAME.id();
    if stack.data == 0 || id == u32::MAX {
        return None;
    }
    let mut len = 0usize;
    let p = unsafe { (host().stack_get)(stack.data, id, &mut len) };
    (!p.is_null()).then(|| String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(p, len) }).into_owned())
}

/// The stack named `name` (`None` or empty: its item's name again). It shows in its tooltip and
/// above the hotbar, and a block placed from it keeps the name (a chest's window title) until
/// it is broken back into an item.
pub fn with_name(stack: ItemStack, name: Option<&str>) -> ItemStack {
    let b = name.filter(|n| !n.is_empty()).map(str::as_bytes);
    let data = unsafe { (host().stack_with)(stack.data, NAME.id(), b.map_or(abi::Slice::EMPTY, abi::Slice::new), b.is_none() as u32) };
    ItemStack { data, ..stack }
}

static NAME: Tag = Tag::named(abi::comp::NAME);

pub trait StackData: Sized {
    /// This stack with its own value of `c` set.
    fn with<T: Value>(self, c: &Component<T>, v: T) -> Self;
    fn without<T: Value>(self, c: &Component<T>) -> Self;
    /// `count` less, empty when nothing is left.
    fn shrink(self, n: u32) -> Self;
    fn grow(self, n: u32) -> Self;
    /// Damage taken (the `damage` stack component).
    fn damage(&self) -> u32;
    fn with_damage(self, d: u32) -> Self;
}

impl StackData for ItemStack {
    fn with<T: Value>(self, c: &Component<T>, v: T) -> Self {
        let b = pod_bytes(&v);
        ItemStack { data: unsafe { (host().stack_with)(self.data, c.id(), abi::Slice::new(&b), 0) }, ..self }
    }
    fn without<T: Value>(self, c: &Component<T>) -> Self {
        ItemStack { data: unsafe { (host().stack_with)(self.data, c.id(), abi::Slice::EMPTY, 1) }, ..self }
    }
    fn shrink(self, n: u32) -> Self {
        if self.count <= n { ItemStack::EMPTY } else { ItemStack { count: self.count - n, ..self } }
    }
    fn grow(self, n: u32) -> Self {
        ItemStack { count: self.count + n, ..self }
    }
    fn damage(&self) -> u32 {
        DAMAGE.stack_only(self).unwrap_or(0)
    }
    fn with_damage(self, d: u32) -> Self {
        if d == 0 { self.without(&DAMAGE) } else { self.with(&DAMAGE, d) }
    }
}

// The components the engine reads itself.
pub static MAX_STACK: Component<u32> = Component::named(abi::comp::MAX_STACK);
pub static DURABILITY: Component<u32> = Component::named(abi::comp::DURABILITY);
pub static ATTACK_DAMAGE: Component<f32> = Component::named(abi::comp::ATTACK_DAMAGE);
pub static MINING_SPEED: Component<f32> = Component::named(abi::comp::MINING_SPEED);
pub static TOOL_LEVEL: Component<u32> = Component::named(abi::comp::TOOL_LEVEL);
pub static HARVEST_LEVEL: Component<u32> = Component::named(abi::comp::HARVEST_LEVEL);
pub static DAMAGE: Component<u32> = Component::named(abi::comp::DAMAGE);
pub static HARDNESS: Component<f32> = Component::named(abi::comp::HARDNESS);
pub static FRICTION: Component<f32> = Component::named(abi::comp::FRICTION);
pub static DRAG: Component<f32> = Component::named(abi::comp::DRAG);
pub static LIGHT: Component<[u8; 3]> = Component::named(abi::comp::LIGHT);
pub static TINT: Component<TintRef> = Component::named(abi::comp::TINT);
pub static TINT_FACES: Component<u8> = Component::named(abi::comp::TINT_FACES);
pub static SUPPORT_FACE: Component<u8> = Component::named(abi::comp::SUPPORT_FACE);
pub static SUPPORT: Component<TagRef> = Component::named(abi::comp::SUPPORT);
pub static SUPPORT_NEAR: Component<TagRef> = Component::named(abi::comp::SUPPORT_NEAR);
pub static SPREADS_TO: Component<TagRef> = Component::named(abi::comp::SPREADS_TO);
pub static SPREAD_TIME: Component<f32> = Component::named(abi::comp::SPREAD_TIME);
pub static SPREAD_LIGHT: Component<u32> = Component::named(abi::comp::SPREAD_LIGHT);
pub static SMOTHERED_INTO: Component<BlockRef> = Component::named(abi::comp::SMOTHERED_INTO);
pub static SMOTHER_TIME: Component<f32> = Component::named(abi::comp::SMOTHER_TIME);
pub static AMBIENT_PARTICLE: Component<Text> = Component::named(abi::comp::AMBIENT_PARTICLE);
pub static CONDUCTOR: Tag = Tag::named(abi::comp::CONDUCTOR);
pub static ENERGY: Component<abi::Energy> = Component::named(abi::comp::ENERGY);
pub static WORN: Component<abi::Worn> = Component::named(abi::comp::WORN);
pub static WORN_TEXTURE: Component<Text> = Component::named(abi::comp::WORN_TEXTURE);
pub static WEAR_PER_HIT: Component<u32> = Component::named(abi::comp::WEAR_PER_HIT);
pub static LIQUID: Component<Text> = Component::named(abi::comp::LIQUID);
pub static REMAINDER: Component<ItemRef> = Component::named(abi::comp::REMAINDER);
pub static SHADER: Component<Text> = Component::named(abi::comp::SHADER);
pub static MODEL: Component<Text> = Component::named(abi::comp::MODEL);
pub static ANIMATIONS: Component<Text> = Component::named(abi::comp::ANIMATIONS);
pub static SOUND: Component<Text> = Component::named(abi::comp::SOUND);

/// The stack after one of it is used up (burnt as fuel, taken by a recipe): one less, and the
/// last one leaves its `remainder` behind (a lava bucket its bucket).
pub fn use_one(stack: ItemStack) -> ItemStack {
    match REMAINDER.on_item(stack.item) {
        Some(left) if stack.count == 1 => ItemStack::new(left.0, 1),
        _ => stack.shrink(1),
    }
}

/// Stack size of an item (64 unless its `max_stack` says otherwise).
pub fn max_stack(item: u32) -> u32 {
    MAX_STACK.on_item(item).unwrap_or(64)
}

/// The block state an item places, if any.
pub fn item_block(item: u32) -> Option<u32> {
    let s = unsafe { (host().item_block)(item) };
    (s != 0).then_some(s)
}
