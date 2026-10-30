//! The boundary between the engine and mods. Everything here is `repr(C)` so a mod built with
//! any rustc (or another language) can be loaded. Rules:
//! - A mod is loaded only if it was built against this exact file (`ABI_VERSION` is a hash of
//!   it), so layouts may change freely; mods are rebuilt with the engine.
//! - Descriptor structs passed to the host are read during the call only.
//! - Strings are UTF-8 `(ptr, len)`, never NUL terminated.
#![no_std]

use core::ffi::c_void;
use core::sync::atomic::AtomicU32;

/// FNV-1a of this file: any change to it (layouts included) makes mods built before it refuse
/// to load instead of reading the wrong memory.
pub const ABI_VERSION: u64 = fnv(include_str!("lib.rs").as_bytes());
/// Symbol every mod library exports: `extern "C" fn() -> *const ModDecl`.
pub const ENTRY_SYMBOL: &[u8] = b"loom_mod\0";

const fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        h = (h ^ bytes[i] as u64).wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    h
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Str {
    pub ptr: *const u8,
    pub len: usize,
}

unsafe impl Send for Str {}
unsafe impl Sync for Str {}

impl Str {
    pub const EMPTY: Str = Str { ptr: core::ptr::null(), len: 0 };

    pub const fn new(s: &str) -> Str {
        Str { ptr: s.as_ptr(), len: s.len() }
    }

    /// # Safety
    /// The pointer must reference valid UTF-8 for the chosen lifetime.
    pub unsafe fn as_str<'a>(&self) -> &'a str {
        if self.len == 0 {
            return "";
        }
        unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(self.ptr, self.len)) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Slice<T> {
    pub ptr: *const T,
    pub len: usize,
}

unsafe impl<T: Sync> Sync for Slice<T> {}
unsafe impl<T: Send> Send for Slice<T> {}

impl<T> Slice<T> {
    pub const EMPTY: Slice<T> = Slice { ptr: core::ptr::null(), len: 0 };

    pub const fn new(s: &[T]) -> Slice<T> {
        Slice { ptr: s.as_ptr(), len: s.len() }
    }

    /// # Safety
    /// Must point to `len` valid elements for the chosen lifetime.
    pub unsafe fn as_slice<'a>(&self) -> &'a [T] {
        if self.len == 0 {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// `DEFAULT` for descriptors, so mods fill in only what they set (`..X::DEFAULT`).
macro_rules! defaults {
    ($t:ident { $($f:ident: $v:expr),* $(,)? }) => {
        impl $t {
            pub const DEFAULT: Self = $t { $($f: $v),* };
        }
    };
}

/// A mod-side function plus its captured state. `args` points to the callback-specific args struct.
/// Returns 0 on success, anything else means the callback failed (the SDK catches panics).
pub type CallbackFn = unsafe extern "C" fn(data: *const c_void, args: *mut c_void) -> u32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callback {
    pub func: Option<CallbackFn>,
    pub data: *const c_void,
}

unsafe impl Send for Callback {}
unsafe impl Sync for Callback {}

impl Callback {
    pub const NONE: Callback = Callback { func: None, data: core::ptr::null() };

    pub fn is_some(&self) -> bool {
        self.func.is_some()
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModId(pub u32);

/// Opaque per-mod registration container.
#[repr(C)]
pub struct Registrar {
    _p: [u8; 0],
}

/// Opaque view of one world (dimension) handed to callbacks. Only valid during the call.
#[repr(C)]
pub struct WorldCtx {
    _p: [u8; 0],
}

#[repr(C)]
pub struct ModDecl {
    pub abi_version: u64,
    /// The crate name, which is also the library's and the mod folder's name.
    pub id: Str,
    /// The crate version.
    pub version: Str,
    /// Mod ids this one must load after. Registration itself is parallel, this only orders
    /// `post_init` and callback priority.
    pub deps: Slice<Str>,
    pub init: unsafe extern "C" fn(host: *const Host, me: ModId, reg: *mut Registrar) -> u32,
}

unsafe impl Sync for ModDecl {}

// ---------------------------------------------------------------- components

/// One component value on a content entry. `schema` names the value type (see the SDK's `Value`):
/// empty for a tag, `u32`, `f32`, `str` (a `Str`, copied by the engine), `{a:u32,b:f32}` for
/// structs. Every use of a name must agree on the schema; the engine checks at freeze.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CompValue {
    pub name: Str,
    pub schema: Str,
    pub value: Slice<u8>,
}

/// Registry kinds for `Host::find`; the first `COUNT` also have component tables (blocks per state).
pub mod entry_kind {
    pub const BLOCK: u32 = 0;
    pub const ITEM: u32 = 1;
    pub const ENTITY: u32 = 2;
    pub const BIOME: u32 = 3;
    pub const DIMENSION: u32 = 4;
    pub const MENU: u32 = 5;
    pub const PARTICLE: u32 = 6;
    pub const CONFIG: u32 = 7;
    pub const SOUND: u32 = 8;
    /// Kinds that carry components (block to dimension).
    pub const COUNT: usize = 5;
    /// Each kind's name for messages, by value.
    pub const NAMES: [&str; 9] = ["block", "item", "entity", "biome", "dimension", "menu", "particle", "setting", "sound"];
}

/// What a `SetDesc` applies to.
pub mod set_target {
    /// The block (all states, or one with `name#3`) and the item of that name, whichever exist.
    pub const NAME: u32 = 0;
    /// Blocks and items carrying a tag; `*` in it matches any text (`ingots/*`).
    pub const TAGGED: u32 = 1;
    pub const ENTITY: u32 = 2;
    pub const BIOME: u32 = 3;
    pub const DIMENSION: u32 = 4;
}

/// Sets (or with `remove` takes away) a component on entries any mod registered. Applied after
/// every mod registered, in mod load order; the last one wins and overrides are logged.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SetDesc {
    pub target_kind: u32,
    pub target: Str,
    pub value: CompValue,
    pub remove: u32,
}
defaults!(SetDesc { target_kind: 0, target: Str::EMPTY, value: CompValue { name: Str::EMPTY, schema: Str::EMPTY, value: Slice::EMPTY }, remove: 0 });

/// Frozen components of one entry kind, readable directly by mods: whether entry `e` has
/// component `c` is bit `c % 64` of `bits[e * words + c / 64]`; its value (if the component has
/// one on this kind) is at `columns[c].data + e * columns[c].stride`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KindTable {
    pub entries: u32,
    pub words: u32,
    pub bits: *const u64,
    /// One per component id; `stride` 0 when no entry of this kind has a value for it.
    pub columns: *const Column,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Column {
    pub data: *const u8,
    pub stride: u32,
}

#[repr(C)]
pub struct ComponentTable {
    /// Number of component ids.
    pub components: u32,
    pub kinds: [KindTable; entry_kind::COUNT],
}

unsafe impl Sync for ComponentTable {}
unsafe impl Send for ComponentTable {}

/// Component names the engine itself reads. Items: `max_stack: u32` (default 64), `durability: u32`,
/// `attack_damage: f32` (default 1), `tool_level: u32` with a `tools/<class>` tag and
/// `mining_speed: f32` (default 1). Blocks: `mineable/<class>` tags; `needs_tool` means nothing
/// drops without a tool of a matching class and at least `harvest_level: u32`. Stacks: `damage: u32`.
pub mod comp {
    pub const MAX_STACK: &str = "max_stack";
    pub const DURABILITY: &str = "durability";
    pub const ATTACK_DAMAGE: &str = "attack_damage";
    pub const MINING_SPEED: &str = "mining_speed";
    pub const TOOL_LEVEL: &str = "tool_level";
    /// The block drops its loot only to a fitting tool of at least this level (0 = any tier).
    pub const HARVEST_LEVEL: &str = "harvest_level";
    pub const DAMAGE: &str = "damage";
    /// Block: seconds to break by hand at speed 1 (`f32`, 1 by default); negative = unbreakable.
    pub const HARDNESS: &str = "hardness";
    /// Block: how slippery its top is (`f32`, 0.6 by default; ice 0.98).
    pub const FRICTION: &str = "friction";
    /// Block: movement speed multiplier inside it (`f32`, 1 by default; cobweb, liquids).
    pub const DRAG: &str = "drag";
    /// Block: emitted light per color channel (`[u8;3]`). 15 is a full-strength source (torch
    /// 14); higher values reach further.
    pub const LIGHT: &str = "light";
    /// Block: a tint kind by name (`tint`: `core:grass`, `core:foliage`, `core:water` or a mod's
    /// own), colored by the biome.
    pub const TINT: &str = "tint";
    /// Block: faces the tint applies to (`u8`, a bit per face in -X, +X, -Y, +Y, -Z, +Z order;
    /// all by default).
    pub const TINT_FACES: &str = "tint_faces";
    /// Block: face (-X, +X, -Y, +Y, -Z, +Z) whose neighbor holds it up (`u8`). Unsupported, it
    /// cannot be placed and breaks off when the support goes.
    pub const SUPPORT_FACE: &str = "support_face";
    /// Block: the tag the supporting block needs (`tag`, `#plant_soil`); without it any solid
    /// block holds it.
    pub const SUPPORT: &str = "support";
    /// Block: the tag one of the supporting block's horizontal neighbors needs (`tag`, sugar
    /// cane: `#water`), unless the support is the same block.
    pub const SUPPORT_NEAR: &str = "support_near";
    /// The own name of one stack or block entity (UTF-8), as a player gave it: shown instead of
    /// the item's or block's, and carried from the stack to the placed block and back.
    pub const NAME: &str = "core:name";
    /// Blocks connecting energy networks.
    pub const CONDUCTOR: &str = "conductor";
    /// Entity component, layout `Energy`.
    pub const ENERGY: &str = "energy";
    /// Item shown on the wearer's model: `Worn`, plus `worn_texture: str` in the wearer's skin layout.
    pub const WORN: &str = "worn";
    pub const WORN_TEXTURE: &str = "worn_texture";
    /// Item: the liquid (block name, `str`) it holds; with `remainder` (the empty container's
    /// name) the engine fills and empties it like a bucket.
    pub const LIQUID: &str = "liquid";
    /// Item left behind when this one is used up (fuel, crafting, pouring): `item`.
    pub const REMAINDER: &str = "remainder";
    /// Entity kinds and blocks: shaded by `shaders/entity/<name>.wgsl` or `shaders/block/<name>.wgsl`
    /// (`str`).
    pub const SHADER: &str = "shader";
    /// Entity kinds: model file (`models/<name>.geo.json`, Bedrock geometry as Blockbench
    /// writes it) used instead of the parts in code; without it `<ns>:<kind name>` is tried.
    pub const MODEL: &str = "model";
    /// Entity kinds: keyframe animations (`animations/<name>.animation.json`, Bedrock format);
    /// `idle` and `walk` play by themselves, others when the server says so. Without it
    /// `<ns>:<kind name>` is tried.
    pub const ANIMATIONS: &str = "animations";
    /// Sound event prefix (`str`): blocks play `<prefix>.break`, `.place`, `.step`, `.hit`;
    /// entity kinds `.hurt`, `.death`, `.ambient`, `.step` (each only if registered).
    pub const SOUND: &str = "sound";
    /// Items: durability lost per hit on a mob (`u32`); without it tools lose 2, like vanilla's
    /// tools made for blocks (swords set 1).
    pub const WEAR_PER_HIT: &str = "wear_per_hit";
    /// Tag of biomes players may start in (the engine's spawn search tries them first).
    pub const SPAWN_BIOME: &str = "spawn_biome";
    /// Block: spreads onto blocks with this tag (`tag`; grass onto dirt) within 1 block
    /// sideways, 3 down and 1 up, where the target has light (`spread_light`) and nothing
    /// opaque on top. In `spread_time` seconds on average, sooner with more of it around.
    pub const SPREADS_TO: &str = "spreads_to";
    /// `f32`: mean seconds for one spreading block to take a target (vanilla grass about 765).
    pub const SPREAD_TIME: &str = "spread_time";
    /// `u32`: light the target needs above it (vanilla grass 9).
    pub const SPREAD_LIGHT: &str = "spread_light";
    /// Block: turns into this one (`block`) when something opaque covers it (grass to dirt).
    pub const SMOTHERED_INTO: &str = "smothered_into";
    /// `f32`: mean seconds before a covered block turns (vanilla about 68).
    pub const SMOTHER_TIME: &str = "smother_time";
    /// Block: a particle kind (`str`) clients show around it now and then, near the player only
    /// (vanilla's torch smoke); nothing of it reaches the server.
    pub const AMBIENT_PARTICLE: &str = "ambient_particle";

}

/// Harvest levels (`tool_level` of a tool, `harvest_level` of a block) of the vanilla tiers.
/// Hundreds apart so mods fit their own tiers in between (a 250 tool mines up to 250).
pub mod tool_tier {
    pub const WOOD: u32 = 0;
    pub const STONE: u32 = 100;
    pub const IRON: u32 = 200;
    pub const DIAMOND: u32 = 300;
}

// ---------------------------------------------------------------- content descriptors

pub mod block_flags {
    /// Entities collide with it.
    pub const SOLID: u32 = 1;
    /// Full opaque cube: hides neighbor faces, blocks light, gets ambient occlusion.
    pub const OPAQUE: u32 = 2;
    /// Placing a block into it replaces it (air, grass, water).
    pub const REPLACEABLE: u32 = 4;
    pub const LIQUID: u32 = 8;
    pub const CLIMBABLE: u32 = 16;
    /// Raycasts pass through (air, liquids for normal interaction).
    pub const NOT_TARGETABLE: u32 = 32;
    /// Hide faces between two blocks of this kind (glass). Without it see-through blocks keep
    /// their inner faces, which is what makes leaves look dense.
    pub const CULL_SAME: u32 = 128;
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    None = 0,
    Cube = 1,
    /// Two diagonal quads (plants).
    Cross = 2,
    /// `boxes` with their own textures.
    Boxes = 4,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Opaque = 0,
    /// Alpha tested (leaves, glass, plants).
    Cutout = 1,
    Translucent = 2,
}

/// How a struct shared as a component or event value is laid out, written next to it so the
/// engine and every mod compare the same text (`"{field:type,...}"`).
pub trait Schema {
    const SCHEMA: &'static str;
}

/// Faces in order: -X, +X, -Y, +Y, -Z, +Z.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModelBox {
    /// In 1/16 block units.
    pub min: [f32; 3],
    pub max: [f32; 3],
    /// Per face; empty takes the state's texture of that face.
    pub textures: [Str; 6],
    /// Per face texel rectangle (u0, v0, u1, v1, 0..16); negative: from the box's position, like
    /// a cube's face.
    pub uv: [[f32; 4]; 6],
}
defaults!(ModelBox { min: [0.0; 3], max: [16.0; 3], textures: [Str::EMPTY; 6], uv: [[-1.0; 4]; 6] });

#[repr(C)]
#[derive(Clone, Copy)]
pub struct StateDesc {
    pub flags: u32,
    /// Extra per-channel absorption on top of `opacity`, for colored glass.
    pub filter: [u8; 3],
    /// How much light it absorbs per block, 0 for air/glass, 15 or more blocks it fully.
    pub opacity: u8,
    pub layer: Layer,
    pub model: Model,
    pub textures: [Str; 6],
    pub boxes: Slice<ModelBox>,
    /// Collision boxes in 1/16 units; ignored for full cubes.
    pub collision: Slice<[f32; 6]>,
    /// What the crosshair picks, outlines and cracks, in 1/16 units; empty: the collision boxes,
    /// else the model's boxes, else a plant's cross, else the whole block.
    pub selection: Slice<[f32; 6]>,
    /// Components of this state only, on top of the block's (hardness, light, support and tint
    /// are components too, `comp::HARDNESS` ...).
    pub components: Slice<CompValue>,
}
defaults!(StateDesc {
    flags: block_flags::SOLID | block_flags::OPAQUE,
    filter: [0; 3],
    opacity: 15,
    layer: Layer::Opaque,
    model: Model::Cube,
    textures: [Str::EMPTY; 6],
    boxes: Slice::EMPTY,
    collision: Slice::EMPTY,
    selection: Slice::EMPTY,
    components: Slice::EMPTY,
});

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlockDesc {
    pub name: Str,
    /// Receives the id of the first state once registries are frozen (null = no handle).
    pub cell: *const AtomicU32,
    /// One per combination of `props` values, the last property changing fastest (with no
    /// properties, one state).
    pub states: Slice<StateDesc>,
    /// Named properties (`facing`, `lit`, `age`), each with its values; the first value is the
    /// default.
    pub props: Slice<PropDesc>,
    /// Components of every state (tags, mining, fuel ...). Its item gets them too unless it sets
    /// its own.
    pub components: Slice<CompValue>,
    /// `UseArgs`; return `handled = 1` to consume the click.
    pub on_use: Callback,
    pub on_neighbor: Callback,
    pub on_scheduled: Callback,
    /// `PlaceArgs`: choose the state from the player's facing/hit face.
    pub placement: Callback,
    /// Non-zero: replaces a block of the same name registered by a mod this one depends on.
    pub replace: u32,
    /// Growth over time: mean seconds between growth steps (0 = none). A step needs at least
    /// `grow_light` light (sky light by the daylight of that moment, or block light). Steps are
    /// timed by world time, so a block in an unloaded area catches up when it loads again.
    pub grow_seconds: f32,
    pub grow_light: u32,
    /// `BlockEventArgs`, a growth step; without it a step moves to the next state and the last
    /// state stops growing.
    pub on_grow: Callback,
    /// 0: a replacement (`replace`) without states of its own, which keeps the replaced block's.
    pub own_states: u32,
    /// Replacements keeping the replaced block's states: (old texture, new texture) pairs, full
    /// names, applied to those states and the block's item.
    pub retexture: Slice<Str>,
    /// A menu using the block opens (on its block entity, if it has one). A block opening a
    /// menu with recipe kinds is a station of those kinds in the recipe viewer.
    pub opens: Str,
}
defaults!(BlockDesc {
    name: Str::EMPTY,
    cell: core::ptr::null(),
    states: Slice::EMPTY,
    props: Slice::EMPTY,
    components: Slice::EMPTY,
    on_use: Callback::NONE,

    on_neighbor: Callback::NONE,
    on_scheduled: Callback::NONE,
    placement: Callback::NONE,
    replace: 0,
    grow_seconds: 0.0,
    grow_light: 0,
    on_grow: Callback::NONE,
    own_states: 1,
    retexture: Slice::EMPTY,
    opens: Str::EMPTY,
});

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PropDesc {
    pub name: Str,
    pub values: Slice<Str>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ItemDesc {
    pub name: Str,
    pub cell: *const AtomicU32,
    /// Texture for the flat icon. Empty with `block` set means a 3D block icon.
    pub texture: Str,
    /// Block placed on use, empty = none.
    pub block: Str,
    /// Stack size, durability, tool and weapon numbers, food ... (see `comp`).
    pub components: Slice<CompValue>,
    /// `ItemUseArgs`.
    pub on_use: Callback,
}
defaults!(ItemDesc { name: Str::EMPTY, cell: core::ptr::null(), texture: Str::EMPTY, block: Str::EMPTY, components: Slice::EMPTY, on_use: Callback::NONE });

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BiomeDesc {
    pub name: Str,
    pub cell: *const AtomicU32,
    /// The biome's own climate and surface (`rule.fields` says which are set): every terrain
    /// rule placing it starts from these, and sets only what differs there.
    pub rule: BiomeRuleDesc,
    pub components: Slice<CompValue>,
}
defaults!(BiomeDesc { name: Str::EMPTY, cell: core::ptr::null(), rule: BiomeRuleDesc::DEFAULT, components: Slice::EMPTY });

#[repr(C)]
pub struct GenArgs {
    pub seed: u64,
    pub cube: [i32; 3],
    /// 4096 state ids, index = (y * 16 + z) * 16 + x. Pre-filled with 0 (air).
    pub blocks: *mut u32,
    /// 256 biome ids per column, index = z * 16 + x.
    pub biomes: *mut u16,
    /// The dimension being generated.
    pub dim: u32,
    /// The dimension's 256 columns of this cube (from its terrain or `DimensionDesc::column`),
    /// index = z * 16 + x. Null for a dimension without either.
    pub columns: *const TerrainColumn,
}

/// Something scattered over the ground on a grid (trees), in every dimension where its
/// biomes stand. One site per `cell` x `cell` square at a seeded spot, kept with the chance
/// the biome's `density` component gives; not under the terrain's sea, not where caves took
/// the ground.
#[repr(C)]
pub struct ScatterDesc {
    pub name: Str,
    /// Space separated full dimension names, empty: every dimension (`dim_flags::BARE` ones
    /// only when named).
    pub dimensions: Str,
    pub cell: i32,
    /// How far blocks reach sideways from the site, so cubes next to it get their part.
    pub reach: i32,
    /// A biome component (`f32`, 0..1): the chance of a site per cell. No value: none there.
    pub density: Str,
    /// `ScatterArgs`. Pure: called once for each cube the site reaches.
    pub shape: Callback,
}
defaults!(ScatterDesc { name: Str::EMPTY, dimensions: Str::EMPTY, cell: 8, reach: 0, density: Str::EMPTY, shape: Callback::NONE });

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScatterBlock {
    /// From `ScatterArgs::pos`.
    pub offset: [i32; 3],
    pub state: u32,
    /// 1: replaces anything; 0: only air.
    pub force: u32,
}

#[repr(C)]
pub struct ScatterArgs {
    pub seed: u64,
    pub dim: u32,
    /// The block above the ground at the site.
    pub pos: [i32; 3],
    pub biome: u32,
    /// Random bits of this site.
    pub random: u64,
    /// Output: up to `cap` blocks, `len` written.
    pub out: *mut ScatterBlock,
    pub cap: u32,
    pub len: u32,
}

#[repr(C)]
pub struct SpawnArgs {
    pub seed: u64,
    /// Output, block position the player stands on top of.
    pub pos: [i32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DimensionDesc {
    pub name: Str,
    pub cell: *const AtomicU32,
    /// `GenArgs`, called in parallel from worker threads, must be a pure function of its args.
    pub generator: Callback,
    /// `SpawnArgs`. Without one, the engine looks for open ground near 0, 0 (biomes tagged
    /// `spawn_biome` first) on the dimension's columns, away from scattered things.
    pub spawn: Callback,
    /// Inclusive cube-y bounds, `i32::MIN`/`i32::MAX` for unbounded.
    pub min_cube_y: i32,
    pub max_cube_y: i32,
    /// Cubes at or above this y are guaranteed empty by the generator, so sky light there is full
    /// without generating them. `i32::MAX` if unknown.
    pub sky_cube_y: i32,
    pub sky_light: u32,
    pub gravity: f32,
    /// Ticks per day, 0 = time does not advance.
    pub day_length: u32,
    pub fixed_time: u32,
    pub sky_color: u32,
    pub fog_color: u32,
    /// Light level everything gets at least, 0..15.
    pub ambient: u32,
    pub components: Slice<CompValue>,
    /// `ColumnArgs`: surface height and biome of a column, for a dimension without a terrain.
    /// The one source of its columns: the generator reads them (`GenArgs::columns`), and
    /// structures, scatters and the spawn search stand on them. Pure like the generator.
    pub column: Callback,
    /// `dim_flags`.
    pub flags: u32,
}
defaults!(DimensionDesc {
    name: Str::EMPTY,
    cell: core::ptr::null(),
    generator: Callback::NONE,
    spawn: Callback::NONE,
    min_cube_y: i32::MIN,
    max_cube_y: i32::MAX,
    sky_cube_y: i32::MAX,
    sky_light: 1,
    gravity: 32.0,
    day_length: 24000,
    fixed_time: 6000,
    sky_color: 0x78a7ff,
    fog_color: 0xc0d8ff,
    ambient: 0,
    components: Slice::EMPTY,
    column: Callback::NONE,
    flags: 0,
});

pub mod dim_flags {
    /// Players start here (and respawn without a bed). Of several, the mod loaded last wins.
    pub const PLAYER_SPAWN: u32 = 1;
    /// Blocks never update: no neighbor updates (support, liquids), scheduled, random or
    /// growth ticks. For showcases like the engine's `core:debug_blocks`.
    pub const NO_BLOCK_UPDATES: u32 = 2;
    /// Only what names it: ores, scatters and decorations meant for every dimension skip it
    /// (a superflat test world).
    pub const BARE: u32 = 4;
}

#[repr(C)]
pub struct ColumnArgs {
    pub seed: u64,
    pub x: i32,
    pub z: i32,
    /// Output: the top solid block's height and the biome there.
    pub height: i32,
    pub biome: u32,
}

/// A block of a structure template, relative to its lowest corner.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StructureBlock {
    pub pos: [i32; 3],
    /// Block state name (`base:furnace#2`); `core:air` clears what is there.
    pub block: Str,
}

/// A structure the engine places while generating (`Host::reg_structure`): one chance per
/// `spacing` x `spacing` cell of the world, standing on the surface (the dimension's `column`)
/// sunk by `sink` blocks, where the biome has the tag.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StructureDesc {
    /// The template (`Host::reg_template`) it places; the structure is called like it.
    pub template: Str,
    pub dimension: Str,
    pub spacing: u32,
    pub chance: f32,
    pub biome: Str,
    pub sink: i32,
}
defaults!(StructureDesc { template: Str::EMPTY, dimension: Str::EMPTY, spacing: 32, chance: 0.5, biome: Str::EMPTY, sink: 0 });

/// A fixed shape of blocks, checked when the game loads: world structures, moving structures
/// (`EntityDesc::template`) and `Host::world_place` take it.
#[repr(C)]
pub struct TemplateDesc {
    pub name: Str,
    /// Positions from its lowest corner; `core:air` clears.
    pub blocks: Slice<StructureBlock>,
}

// ---------------------------------------------------------------- callback args

#[repr(C)]
pub struct BlockEventArgs {
    pub world: *mut WorldCtx,
    pub pos: [i32; 3],
    pub state: u32,
    /// Neighbor position for `on_neighbor`, otherwise unused.
    pub other: [i32; 3],
    pub entity: u64,
}

#[repr(C)]
pub struct UseArgs {
    pub world: *mut WorldCtx,
    pub pos: [i32; 3],
    pub state: u32,
    pub face: u32,
    pub player: u64,
    pub handled: u32,
}

#[repr(C)]
pub struct PlaceArgs {
    pub pos: [i32; 3],
    pub face: u32,
    /// Player look direction.
    pub look: [f32; 3],
    /// In/out: starts as the first state.
    pub state: u32,
}

#[repr(C)]
pub struct ItemUseArgs {
    pub world: *mut WorldCtx,
    pub player: u64,
    /// In/out: the held stack; change it to consume (eating, placing).
    pub stack: ItemStack,
    /// Target block if any (`has_target`).
    pub pos: [i32; 3],
    pub face: u32,
    pub has_target: u32,
    pub handled: u32,
}

// ---------------------------------------------------------------- items, entities, menus

/// `data` is 0 for a plain stack, otherwise a handle to its stack components (damage taken,
/// charge, contents ...). Handles are interned: two stacks with equal data have equal handles,
/// so stacks merge exactly when `item` and `data` match. Read and change data through the host.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Hash)]
pub struct ItemStack {
    pub item: u32,
    pub count: u32,
    pub data: u32,
}
impl Schema for ItemStack {
    const SCHEMA: &'static str = "{item:u32,count:u32,data:u32}";
}

impl ItemStack {
    pub const EMPTY: ItemStack = ItemStack { item: 0, count: 0, data: 0 };

    pub fn new(item: u32, count: u32) -> ItemStack {
        ItemStack { item, count, data: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.item == 0 || self.count == 0
    }

    /// Same item and data, so the two can share a slot.
    pub fn stacks_with(&self, other: &ItemStack) -> bool {
        self.item == other.item && self.data == other.data
    }
}

pub mod entity_flags {
    pub const FALL_DAMAGE: u32 = 1;
    /// Other entities push it away.
    pub const PUSHABLE: u32 = 2;
    pub const NO_GRAVITY: u32 = 4;
    pub const INVULNERABLE: u32 = 8;
    /// Burns in daylight (undead).
    pub const BURNS_IN_SUN: u32 = 16;
    pub const SWIMS: u32 = 32;
    /// A solid body: others stand on it (riding along), bump into it and get pushed by it.
    pub const SOLID: u32 = 64;
    /// Moved exactly by its velocity (set by mod logic): no gravity, no block collision.
    pub const KINEMATIC: u32 = 128;
    /// A solid body hurts what it squeezes against blocks instead of only pushing.
    pub const CRUSHES: u32 = 256;
    /// Not a kind flag: set in `EntityState::flags` when its last move ran into a wall.
    pub const BLOCKED: u32 = 1 << 31;
}

/// Which body part a model piece is, so the engine can animate it (walk cycle, head look).
/// Box in model pixels (1/16 block) relative to the part pivot; `uv` is the top-left texel of the
/// classic box unwrap, or with `per_face` set each face takes its own rectangle.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModelCuboid {
    pub origin: [f32; 3],
    pub extent: [f32; 3],
    pub uv: [f32; 2],
    pub inflate: f32,
    /// Box unwrap mirrored left to right (Bedrock/Java `mirror`).
    pub mirror: u32,
    /// Texel rectangles (u, v, width, height; negative sizes flip) of the faces in -X, +X, -Y,
    /// +Y, -Z, +Z order, used when `per_face` is non-zero.
    pub faces: [[f32; 4]; 6],
    pub per_face: u32,
}
defaults!(ModelCuboid { origin: [0.0; 3], extent: [1.0; 3], uv: [0.0; 2], inflate: 0.0, mirror: 0, faces: [[0.0; 4]; 6], per_face: 0 });

/// A corner of a model's free-form triangles (spheres, curved shapes): model pixels like the
/// pivots, texels.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeshVertex {
    pub pos: [f32; 3],
    pub uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModelPart {
    /// Pivot in model pixels, y up from the feet.
    pub pivot: [f32; 3],
    pub cuboids: Slice<ModelCuboid>,
    /// Resting rotation in radians about x, y, z, applied x first; walking and looking add to it.
    pub rest: [f32; 3],
    /// Bone name: what keyframe animations address, and what decides how it moves by itself
    /// (`head` looks, `rightArm`/`leftArm`/`rightLeg`/`leftLeg` swing, `body` leans when
    /// sneaking, `leg0`..`leg3` of a quadruped, `leg0`..`leg7` of a spider sweep).
    pub name: Str,
    /// Index of the part this one hangs from (it moves with it), -1 for none.
    pub parent: i32,
    /// Triangles, three corners each, besides the boxes.
    pub mesh: Slice<MeshVertex>,
}
defaults!(ModelPart { pivot: [0.0; 3], cuboids: Slice::EMPTY, rest: [0.0; 3], name: Str::EMPTY, parent: -1, mesh: Slice::EMPTY });

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EntityDesc {
    pub name: Str,
    pub cell: *const AtomicU32,
    pub width: f32,
    pub height: f32,
    pub eye: f32,
    /// Walking speed in blocks per second.
    pub speed: f32,
    pub flags: u32,
    pub texture: Str,
    pub texture_size: [u32; 2],
    pub parts: Slice<ModelPart>,
    /// `EntityTickArgs`: every entity of this kind in one call.
    pub on_tick: Callback,
    /// `EntityEventArgs` when a player right clicks one and `question::INTERACT_ENTITY` let it;
    /// `handled = 1` swings the player's arm.
    pub on_interact: Callback,
    /// Non-zero: replaces an entity kind of the same name from a mod this one depends on.
    pub replace: u32,
    pub components: Slice<CompValue>,
    /// Solid bodies: how far above it a rider may be (jumping) and still move with it, keeping
    /// its own motion relative to the body; 0 = only while standing on it.
    pub carry_height: f32,
    /// A moving structure built of this template (`Host::reg_template`), block (0, 0, 0)
    /// centered on the entity; its box is what the blocks fill.
    pub template: Str,
    /// A category by name (`core:misc`, `core:hostile`, `core:passive`, or one from
    /// `Host::reg_entity_category`); empty = `core:misc`.
    pub category_name: Str,
}
defaults!(EntityDesc {
    name: Str::EMPTY,
    cell: core::ptr::null(),
    width: 0.6,
    height: 1.8,
    eye: 1.6,
    speed: 3.0,
    flags: entity_flags::FALL_DAMAGE | entity_flags::PUSHABLE,
    texture: Str::EMPTY,
    texture_size: [64, 32],
    parts: Slice::EMPTY,
    on_tick: Callback::NONE,
    on_interact: Callback::NONE,
    replace: 0,
    components: Slice::EMPTY,
    carry_height: 0.0,
    template: Str::EMPTY,
    category_name: Str::EMPTY,
});

#[repr(C)]
pub struct EntityTickArgs {
    pub world: *mut WorldCtx,
    pub kind: u32,
    pub ids: *const u64,
    pub count: usize,
}

#[repr(C)]
pub struct EntityEventArgs {
    pub world: *mut WorldCtx,
    pub entity: u64,
    /// Attacker / interacting player, 0 if none.
    pub other: u64,
    /// In/out for `on_hurt`: change it to reduce damage (armor).
    pub amount: f32,
    pub cause: u32,
    pub handled: u32,
}

/// Damage causes. Mods add their own numbers; a cause with `BYPASSES_ARMOR` set is not
/// reduced by armor (falling, starving, drowning, the void).
pub mod damage {
    pub const BYPASSES_ARMOR: u32 = 1 << 31;
    pub const GENERIC: u32 = 0;
    pub const FALL: u32 = 1 | BYPASSES_ARMOR;
    pub const ATTACK: u32 = 2;
    pub const FIRE: u32 = 3;
    pub const STARVE: u32 = 4 | BYPASSES_ARMOR;
    pub const DROWN: u32 = 5 | BYPASSES_ARMOR;
    pub const VOID: u32 = 6 | BYPASSES_ARMOR;
    /// Inside a burning hot liquid.
    pub const LAVA: u32 = 7;
    /// Squeezed against blocks by a crushing body.
    pub const CRUSH: u32 = 8;
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EntityState {
    pub kind: u32,
    pub flags: u32,
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: u32,
    pub in_liquid: u32,
    pub health: f32,
    pub max_health: f32,
    pub age: u64,
    /// Blocks fallen since last on ground.
    pub fall: f32,
    /// Distance walked this tick (hunger exhaustion); read on a client, the limb swing (0
    /// standing, 1 running) in the frame of what it stands on.
    pub moved: f32,
    /// Box width and height and eye height now (crouching and `set_size` included).
    pub size: [f32; 3],
}
defaults!(EntityState {
    kind: 0,
    flags: 0,
    pos: [0.0; 3],
    vel: [0.0; 3],
    yaw: 0.0,
    pitch: 0.0,
    on_ground: 0,
    in_liquid: 0,
    health: 0.0,
    max_health: 0.0,
    age: 0,
    fall: 0.0,
    moved: 0.0,
    size: [0.0; 3],
});

impl Default for EntityState {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A ball filled with one block (`Host::area_fill`): a crater with air.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AreaFillDesc {
    pub center: [f64; 3],
    pub radius: f64,
    pub state: u32,
    /// `area_flags`.
    pub flags: u32,
}
defaults!(AreaFillDesc { center: [0.0; 3], radius: 0.0, state: 0, flags: 0 });

pub mod area_flags {
    /// Unbreakable blocks (negative hardness) stay.
    pub const KEEP_UNBREAKABLE: u32 = 1;
}

pub mod component_flags {
    pub const SAVE: u32 = 1;
    /// Sent to the player that owns it (HUD values like hunger).
    pub const SYNC_OWNER: u32 = 2;
    /// Sent to every player that sees the entity, when it changes.
    pub const SYNC_WATCHERS: u32 = 4;
    /// Sent to players with a menu open on the entity (machine progress), when it changes; read
    /// on the client with `ClientView::menu_component`.
    pub const SYNC_MENU: u32 = 8;
}

/// Per-entity data storage (hunger, machine state): created with `default` on spawn of the kinds
/// it is attached to (`Host::reg_attach_value`). The name lives in the same namespace as content
/// components.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ComponentDesc {
    pub name: Str,
    pub schema: Str,
    /// Value size in bytes.
    pub value_size: u32,
    pub flags: u32,
    /// Initial bytes, `value_size` long.
    pub default: Slice<u8>,
}
defaults!(ComponentDesc { name: Str::EMPTY, schema: Str::EMPTY, value_size: 0, flags: 0, default: Slice::EMPTY });

/// Recipe ingredients are item names or `#tag`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RecipeKey {
    pub ch: u32,
    pub item: Str,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RecipeDesc {
    /// Recipe type, e.g. "base:crafting", "base:smelting". Any mod can add types.
    pub kind: Str,
    /// Shaped recipes: rows of key characters. Empty for shapeless/processing.
    pub pattern: Slice<Str>,
    pub keys: Slice<RecipeKey>,
    /// Shapeless/processing inputs: item names, `#tags` or `fluid:<liquid>`.
    pub inputs: Slice<Str>,
    /// How many of each input (millibuckets for fluids); empty: one each.
    pub input_counts: Slice<u32>,
    pub output: Str,
    pub count: u32,
    /// Processing time in ticks, meaning is up to the recipe type.
    pub time: u32,
    /// More results after `output` (byproducts, chances).
    pub outputs: Slice<RecipeOutput>,
    /// Data machines read from the recipe and the viewer shows (energy per tick).
    pub values: Slice<CompValue>,
    /// `recipe_flags`.
    pub flags: u32,
}
defaults!(RecipeDesc {
    kind: Str::EMPTY,
    pattern: Slice::EMPTY,
    keys: Slice::EMPTY,
    inputs: Slice::EMPTY,
    input_counts: Slice::EMPTY,
    output: Str::EMPTY,
    count: 1,
    time: 0,
    outputs: Slice::EMPTY,
    values: Slice::EMPTY,
    flags: 0
});

pub mod recipe_flags {
    /// Works, but the recipe viewer doesn't show it.
    pub const HIDDEN: u32 = 1;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RecipeOutput {
    /// An item or `fluid:<liquid>`.
    pub item: Str,
    pub count: u32,
    /// 0..1, 1 for always.
    pub chance: f32,
}

/// A kind of recipe and how the recipe viewer shows it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RecipeKindDesc {
    pub name: Str,
    /// Inputs and outputs as grids of this size (a crafting table's 3 x 3 whatever the recipe
    /// uses); 0: as many as a recipe has.
    pub input_grid: [u32; 2],
    pub output_grid: [u32; 2],
    /// Translation under each recipe, with `{0}`.. filled from `info_values`: `time` (seconds) or
    /// the name of a component the recipe carries.
    pub info: Str,
    pub info_values: Slice<Str>,
    /// `RecipeUiArgs`, client: builds one recipe instead of the default layout.
    pub display: Callback,
}
defaults!(RecipeKindDesc { name: Str::EMPTY, input_grid: [0; 2], output_grid: [0; 2], info: Str::EMPTY, info_values: Slice::EMPTY, display: Callback::NONE });

/// A recipe the viewer shows with a kind's own `display`.
#[repr(C)]
pub struct RecipeUiArgs {
    pub ui: *mut UiCtx,
    pub client: *mut ClientCtx,
    pub kind: Str,
    /// The recipe's number in its kind, for `Host::recipe_info` and friends.
    pub recipe: u32,
}

/// What `Host::recipe_info` tells about one recipe.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RecipeInfo {
    pub time: u32,
    pub inputs: u32,
    pub outputs: u32,
    pub flags: u32,
}

pub mod slot_flags {
    /// Can only be taken from (crafting result).
    pub const OUTPUT: u32 = 1;
    /// Can only be put into, nothing comes back out by hand.
    pub const INPUT: u32 = 2;
    /// The player's hotbar (a group of `reg_player_inventory`): selected with the number keys,
    /// shown at the bottom of the screen.
    pub const HOTBAR: u32 = 4;
}

/// Whose inventory a slot group shows.
pub mod slot_owner {
    /// The menu's own slots, emptied back to the player on close (crafting grids).
    pub const MENU: u32 = 0;
    /// The entity the menu opened on (a machine).
    pub const BLOCK: u32 = 1;
    /// The player's inventory from `start`.
    pub const PLAYER: u32 = 2;
    /// The player's main inventory, laid out like `Registry::player_inventory` says.
    pub const PLAYER_MAIN: u32 = 3;
    pub const PLAYER_HOTBAR: u32 = 4;
    /// Stands for one group per equipment slot of the player, named like the slots, where it
    /// is in the list (`ui_kind::EQUIPMENT` draws them).
    pub const PLAYER_EQUIPMENT: u32 = 5;
}

/// Slots shown and handled together: a grid of one owner's inventory with the same rules.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SlotGroupDesc {
    /// The screen and the menu's callbacks find the group by this.
    pub name: Str,
    /// `slot_owner`.
    pub owner: u32,
    /// First index in the owner's inventory, `u32::MAX`: right after the previous group of the
    /// same owner.
    pub start: u32,
    pub cols: u32,
    pub rows: u32,
    /// `slot_flags`.
    pub flags: u32,
    /// Only items with this tag or component.
    pub accepts: Str,
    /// Most items a slot holds, 0: as many as the item stacks to.
    pub limit: u32,
    /// Side of a slot in GUI pixels, 0: 18.
    pub size: f32,
    /// GUI texture shown in an empty slot.
    pub background: Str,
    /// Translation shown over an empty slot.
    pub tooltip: Str,
    /// Shift-clicks from the player's inventory fill groups with a higher priority first.
    pub priority: i32,
}
defaults!(SlotGroupDesc {
    name: Str::EMPTY,
    owner: 0,
    start: u32::MAX,
    cols: 1,
    rows: 1,
    flags: 0,
    accepts: Str::EMPTY,
    limit: 0,
    size: 0.0,
    background: Str::EMPTY,
    tooltip: Str::EMPTY,
    priority: 0
});

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MenuDesc {
    pub name: Str,
    pub cell: *const AtomicU32,
    /// Its slots, in this order.
    pub groups: Slice<SlotGroupDesc>,
    /// The entity kind of the machine it belongs to (`slot_owner::BLOCK` groups are its inventory).
    pub entity: Str,
    /// Recipe kinds it makes, space separated: its `arrow` bars open them in the recipe viewer.
    pub recipes: Str,
    /// `MenuArgs`, server: a slot changed (recompute crafting output).
    pub on_change: Callback,
    /// `MenuArgs` with `slot` set, server: an OUTPUT slot was taken (consume ingredients).
    pub on_take: Callback,
    /// `MenuArgs`, server, every tick while open.
    pub on_tick: Callback,
    /// `UiArgs`, client: build the screen. Slot widgets refer to the groups by name.
    pub screen: Callback,
}
defaults!(MenuDesc {
    name: Str::EMPTY,
    cell: core::ptr::null(),
    groups: Slice::EMPTY,
    entity: Str::EMPTY,
    recipes: Str::EMPTY,
    on_change: Callback::NONE,
    on_take: Callback::NONE,
    on_tick: Callback::NONE,
    screen: Callback::NONE,
});

#[repr(C)]
pub struct MenuCtx {
    _p: [u8; 0],
}

#[repr(C)]
pub struct MenuArgs {
    pub world: *mut WorldCtx,
    pub menu: *mut MenuCtx,
    pub player: u64,
    /// Entity whose inventory the menu shows (block entity), 0 for the player's own.
    pub owner: u64,
    pub slot: u32,
}

// ---------------------------------------------------------------- mod UI

#[repr(C)]
pub struct UiCtx {
    _p: [u8; 0],
}

/// Client-side game view for HUD bindings.
#[repr(C)]
pub struct ClientCtx {
    _p: [u8; 0],
}

/// Layout of the `worn` item component; see `comp::WORN`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Worn {
    /// The regions of the wearer's model it covers (`worn_parts`).
    pub roles: u32,
    /// Model pixels added on every side, so layers sit above the skin and each other.
    pub inflate: f32,
}
impl Schema for Worn {
    const SCHEMA: &'static str = "{roles:u32,inflate:f32}";
}

/// Regions of a model by its bone names: the head, the body (and any bone not named as a limb),
/// the arms, the legs.
pub mod worn_parts {
    pub const HEAD: u32 = 1;
    pub const BODY: u32 = 2;
    pub const ARMS: u32 = 4;
    pub const LEGS: u32 = 8;
}

pub mod ui_kind {
    pub const CONTAINER: u32 = 0;
    pub const PANEL: u32 = 1;
    pub const TEXT: u32 = 2;
    pub const BUTTON: u32 = 3;
    /// Slots of the group named by `class`: all of them as its grid, or only number `slot` when
    /// it is not `u32::MAX`.
    pub const SLOT: u32 = 4;
    /// Bound value 0..1 from `bind`, filling in direction `dir`.
    pub const PROGRESS: u32 = 5;
    pub const IMAGE: u32 = 6;
    pub const SPACER: u32 = 7;
    /// Row of `count` icons (full/half/empty textures from the theme `class`) driven by `bind`;
    /// `data` 1 fills from the right.
    pub const ICONS: u32 = 8;
    /// The local player's model with what it wears, turning towards the mouse.
    pub const ENTITY: u32 = 9;
    /// In a recipe's `display`: its inputs (`data` 0) or outputs (1) as the kind's grid, or only
    /// number `slot` when it is not `u32::MAX`.
    pub const RECIPE_SLOT: u32 = 10;
    /// What mods add to place `text` of screen `class` (`Host::reg_screen_part`).
    pub const PARTS: u32 = 11;
    /// The player's equipment slots of the open menu (`slot_owner::PLAYER_EQUIPMENT`), a column.
    pub const EQUIPMENT: u32 = 13;
    /// The open menu's title: the own name of what it belongs to (a renamed chest), else the
    /// name of the block it was opened on, else the translation `menu.<ns>.<name>`.
    pub const TITLE: u32 = 14;
    /// A one-line text field showing `text` (a ready value, not a key), at most `count`
    /// characters (0: any); `on_change` gets each edit.
    pub const INPUT: u32 = 15;
    /// A slider at `bind` (0..1); `on_change` gets the value the player drags it to.
    pub const SLIDER: u32 = 16;
    /// Item `data` as its icon, with `count` under it when above 1.
    pub const ITEM: u32 = 17;
    /// A progress bar (`bind`, class `arrow` unless given) that opens the open menu's recipe
    /// kinds when clicked, with "Recipes" over it.
    pub const RECIPE_ARROW: u32 = 12;
}

/// Fill directions of progress bars.
pub mod ui_dir {
    pub const RIGHT: u32 = 0;
    pub const UP: u32 = 1;
    pub const LEFT: u32 = 2;
    pub const DOWN: u32 = 3;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UiArg {
    /// `ui_arg`.
    pub kind: u32,
    pub text: Str,
    pub num: f64,
    /// Decimals of a float.
    pub digits: u32,
}
defaults!(UiArg { kind: 0, text: Str::EMPTY, num: 0.0, digits: 1 });

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UiNode {
    pub kind: u32,
    pub class: Str,
    /// Translation key (or texture for IMAGE). Empty with one argument: that argument as it is.
    pub text: Str,
    pub args: Slice<UiArg>,
    /// NaN = automatic.
    pub width: f32,
    pub height: f32,
    /// Percent of the parent instead, NaN = not.
    pub width_pct: f32,
    pub height_pct: f32,
    pub min: [f32; 2],
    pub max_width: f32,
    /// `ui_align` of this node in its parent, `u32::MAX` = the parent's.
    pub self_align: u32,
    /// `ui_flags`.
    pub flags: u32,
    /// Text size, 1 = normal.
    pub scale: f32,
    pub tooltip_args: Slice<UiArg>,
    /// INPUT and SLIDER: `UiChangeArgs` when the player changes the value.
    pub on_change: Callback,
    pub grow: f32,
    pub gap: f32,
    pub pad: [f32; 4],
    pub row: u32,
    /// 0 start, 1 center, 2 end, 3 stretch.
    pub align: u32,
    /// 0 start, 1 center, 2 end, 3 space between.
    pub justify: u32,
    pub slot: u32,
    pub data: u32,
    pub count: u32,
    /// `UiBindArgs`: fills `value` (and text via `Host::ui_text`) every frame.
    pub bind: Callback,
    pub on_click: Callback,
    pub tooltip: Str,
    /// `ui_dir` for progress bars.
    pub dir: u32,
}
defaults!(UiNode {
    kind: 0,
    class: Str::EMPTY,
    text: Str::EMPTY,
    args: Slice::EMPTY,
    width: f32::NAN,
    height: f32::NAN,
    width_pct: f32::NAN,
    height_pct: f32::NAN,
    min: [0.0; 2],
    max_width: f32::INFINITY,
    self_align: u32::MAX,
    flags: 0,
    scale: 1.0,
    tooltip_args: Slice::EMPTY,
    on_change: Callback::NONE,
    grow: 0.0,
    gap: 0.0,
    pad: [0.0; 4],
    row: 0,
    align: 0,
    justify: 0,
    slot: 0,
    data: u32::MAX,
    count: 0,
    bind: Callback::NONE,
    on_click: Callback::NONE,
    tooltip: Str::EMPTY,
    dir: 0,
});

#[repr(C)]
pub struct UiArgs {
    pub ui: *mut UiCtx,
    pub client: *mut ClientCtx,
}

#[repr(C)]
pub struct UiBindArgs {
    pub client: *mut ClientCtx,
    /// Out: numeric value.
    pub value: f32,
    /// Pass to `Host::ui_text` to set text.
    pub text: *mut UiCtx,
}

/// A player changed an INPUT (`text`) or SLIDER (`value`, 0..1).
#[repr(C)]
pub struct UiChangeArgs {
    pub client: *mut ClientCtx,
    pub text: Str,
    pub value: f32,
}

/// `UiNode::flags`.
pub mod ui_flags {
    /// Children scroll inside the node (mouse wheel) and are clipped to it.
    pub const SCROLL: u32 = 1;
    /// Text stays on one line.
    pub const NO_WRAP: u32 = 2;
    /// Text with digit-wide cells, so changing numbers don't move what follows.
    pub const MONO: u32 = 4;
    /// A button that can't be pressed.
    pub const DISABLED: u32 = 8;
    /// A button shown as chosen (the open tab).
    pub const SELECTED: u32 = 16;
}

/// A screen (`Host::reg_screen`): how it is built and how it behaves, the same properties the
/// engine's screens have, so a mod's `core:pause` still pauses.
#[repr(C)]
pub struct ScreenDesc {
    pub name: Str,
    /// `UiArgs`.
    pub build: Callback,
    /// `screen_flags`.
    pub flags: u32,
    /// `ScreenKeyArgs` for every key pressed while it is on top, before Escape and the other
    /// global keys.
    pub on_key: Callback,
}
defaults!(ScreenDesc { name: Str::EMPTY, build: Callback::NONE, flags: 0, on_key: Callback::NONE });

pub mod screen_flags {
    /// Single player stops while it is open.
    pub const PAUSES: u32 = 1;
    /// Escape doesn't close it (the death screen).
    pub const KEEP_ON_ESCAPE: u32 = 2;
    /// Closes when the player respawns.
    pub const CLOSES_ON_RESPAWN: u32 = 4;
    /// Gets keys before Escape and the other global keys.
    pub const KEYS_FIRST: u32 = 8;
    /// Like the chat screen: the HUD leaves the messages to it, and Enter sends the text of its
    /// INPUT of class `chat`.
    pub const SHOWS_CHAT: u32 = 16;
}

#[repr(C)]
pub struct ScreenKeyArgs {
    pub client: *mut ClientCtx,
    /// The key's settings name (`Enter`, `KeyE`).
    pub key: Str,
}

// ---------------------------------------------------------------- host

pub mod level {
    pub const ERROR: u32 = 1;
    pub const WARN: u32 = 2;
    pub const INFO: u32 = 3;
    pub const DEBUG: u32 = 4;
    pub const TRACE: u32 = 5;
}

#[repr(C)]
pub struct Host {
    pub log: unsafe extern "C" fn(me: ModId, level: u32, msg: Str),
    /// Like `log` under a sub-target of the mod's own: `mod:<id>::<target>` (`mod:<id>` if empty),
    /// so players can filter it with `--log`/`/log`.
    pub log_at: unsafe extern "C" fn(me: ModId, level: u32, target: Str, msg: Str),
    /// Non-zero when a record at `level` for that target would be written; check before building
    /// costly diagnostics.
    pub log_enabled: unsafe extern "C" fn(me: ModId, level: u32, target: Str) -> u32,

    pub reg_block: unsafe extern "C" fn(reg: *mut Registrar, desc: *const BlockDesc),
    pub reg_item: unsafe extern "C" fn(reg: *mut Registrar, desc: *const ItemDesc),
    pub reg_biome: unsafe extern "C" fn(reg: *mut Registrar, desc: *const BiomeDesc),
    pub reg_dimension: unsafe extern "C" fn(reg: *mut Registrar, desc: *const DimensionDesc),
    pub reg_entity: unsafe extern "C" fn(reg: *mut Registrar, desc: *const EntityDesc),
    /// The model (parts on a skin of `uv_width` x `uv_height` texels) of a kind of any mod or the engine's
    /// (`core:player`); a later mod's wins.
    pub reg_entity_model: unsafe extern "C" fn(reg: *mut Registrar, kind: Str, parts: Slice<ModelPart>, uv_width: u32, uv_height: u32),
    pub reg_component: unsafe extern "C" fn(reg: *mut Registrar, desc: *const ComponentDesc),
    /// `cb` (`WorldTickArgs`) once per tick in every region of every dimension, for logic that
    /// isn't one entity's or block's (a dimension's loaded parts are regions run in parallel).
    pub reg_world_tick: unsafe extern "C" fn(reg: *mut Registrar, cb: Callback),
    /// In `ticks` ticks, `event` fires with `data` (one value of its type), and the timer is
    /// saved with `entity` until then. One per entity and event: a second call moves it. A
    /// timer of a gone entity never fires.
    pub entity_after: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, ticks: u32, event: u32, data: Slice<u8>),
    pub reg_recipe: unsafe extern "C" fn(reg: *mut Registrar, desc: *const RecipeDesc),
    pub reg_menu: unsafe extern "C" fn(reg: *mut Registrar, desc: *const MenuDesc),
    /// Menu opened by the inventory key.
    pub set_player_menu: unsafe extern "C" fn(reg: *mut Registrar, menu: Str),
    /// Block entity kind created (with an inventory of `slots`) when the block is placed, removed
    /// (dropping its items) when broken.
    pub set_block_entity: unsafe extern "C" fn(reg: *mut Registrar, block: Str, entity: Str, slots: u32),
    /// HUD element `name` placed at `anchor` (see `hud_anchor`), `order` 0 nearest the anchor's
    /// edge. Players can move any element to another anchor in `config/hud.txt`.
    pub reg_hud_element: unsafe extern "C" fn(reg: *mut Registrar, name: Str, anchor: Str, order: i32, cb: Callback),
    /// Chat command `/name`, arguments described by `usage` (see the engine's command syntax);
    /// `cb` gets `CommandArgs` with one value per named parameter.
    pub reg_command: unsafe extern "C" fn(reg: *mut Registrar, name: Str, usage: Str, cb: Callback),
    pub reg_ore: unsafe extern "C" fn(reg: *mut Registrar, desc: *const OreDesc),
    pub reg_scatter: unsafe extern "C" fn(reg: *mut Registrar, desc: *const ScatterDesc),
    /// `cb` (`GenArgs`) runs after a cube was generated, in the dimensions named (space
    /// separated; empty: every dimension but `dim_flags::BARE` ones). Pure like a generator.
    pub reg_decoration: unsafe extern "C" fn(reg: *mut Registrar, dimensions: Str, cb: Callback),
    /// Creative inventory tab shown with `icon` (an item name); items join it with the tag
    /// `tab/<name>`. Tabs appear in mod load order, then registration order.
    pub reg_creative_tab: unsafe extern "C" fn(reg: *mut Registrar, name: Str, icon: Str),
    /// Sets or removes a component (or tag) on entries of any mod, see `SetDesc`.
    pub reg_set: unsafe extern "C" fn(reg: *mut Registrar, desc: *const SetDesc),

    /// Looks up a frozen id by full name ("base:stone"), `u32::MAX` if missing. Blocks give their
    /// first state and write the state count to `count` (others write 1). `kind` is `entry_kind`.
    pub find: unsafe extern "C" fn(kind: u32, name: Str, count: *mut u32) -> u32,
    /// State id -> first state id of its block.
    pub block_of: unsafe extern "C" fn(state: u32) -> u32,
    /// The value of property `name` in `state` (empty if the block has no such property).
    pub state_prop: unsafe extern "C" fn(state: u32, name: Str) -> Str,
    /// `state` with property `name` set to `value`, the others kept; `u32::MAX` if the block has
    /// no such property or value.
    pub state_with: unsafe extern "C" fn(state: u32, name: Str, value: Str) -> u32,
    pub state_flags: unsafe extern "C" fn(state: u32) -> u32,
    /// Component id by name, `u32::MAX` if nothing uses it. A non-empty `schema` that differs from
    /// the registered one is an error (logged) and also gives `u32::MAX`.
    pub find_component: unsafe extern "C" fn(name: Str, schema: Str) -> u32,
    /// The frozen component tables (null before freeze); stable for the whole session.
    pub components: unsafe extern "C" fn() -> *const ComponentTable,
    /// Value bytes of stack component `comp` on stack data `data`, null if absent.
    pub stack_get: unsafe extern "C" fn(data: u32, comp: u32, len: *mut usize) -> *const u8,
    /// Stack data with `comp` set to `value` (or removed when `remove` is non-zero).
    pub stack_with: unsafe extern "C" fn(data: u32, comp: u32, value: Slice<u8>, remove: u32) -> u32,

    pub get_block: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [i32; 3]) -> u32,
    /// Returns 0 if the position is not loaded.
    pub set_block: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [i32; 3], state: u32) -> u32,
    pub schedule_tick: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [i32; 3], delay: u32),
    pub world_time: unsafe extern "C" fn(world: *mut WorldCtx) -> u64,
    /// Deterministic per-world random stream.
    pub random: unsafe extern "C" fn(world: *mut WorldCtx) -> u64,
    pub light: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [i32; 3]) -> u32,
    /// 0..1 sun strength for the world's time of day.
    pub daylight: unsafe extern "C" fn(world: *mut WorldCtx) -> f32,

    pub entity_spawn: unsafe extern "C" fn(world: *mut WorldCtx, kind: u32, pos: *const [f64; 3]) -> u64,
    pub entity_remove: unsafe extern "C" fn(world: *mut WorldCtx, id: u64),
    pub entity_get: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, out: *mut EntityState) -> u32,
    /// Writes pos, vel, yaw, pitch and health from `state`.
    /// Adds `v` (blocks per second) to the entity's velocity: a mob's here, a player's on its
    /// client, the way a hit's knockback goes.
    pub entity_push: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, v: *const [f64; 3]),
    pub entity_set: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, state: *const EntityState),
    /// AI steering: horizontal direction and speed factor applied by the physics, optional jump.
    pub entity_walk: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, dir: *const [f32; 2], speed: f32, jump: u32),
    pub entity_damage: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, amount: f32, source: u64, cause: u32) -> u32,
    /// `attacker` hits `target` like a player does: the `ATTACK` question, the `ATTACK_DAMAGE`
    /// chain from the held item's (players) or the kind's `attack_damage`, and the attacker's
    /// cooldown (mobs: once a second). Non-zero if the hit landed.
    pub entity_attack: unsafe extern "C" fn(world: *mut WorldCtx, attacker: u64, target: u64) -> u32,
    /// The block an entity looks at from its eyes within `max` blocks, liquids too if `liquids`:
    /// its state + 1 (0 = nothing), position and face hit.
    /// The block entity `id` looks at within `max` (the world's or a moving structure's: `frame`
    /// is its entity, 0 for the world, `pos` in its blocks); state + 1, or 0 for none.
    pub entity_look_at: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, max: f64, liquids: u32, pos: *mut [i32; 3], face: *mut u32, frame: *mut u64) -> u32,
    /// Fills `out` with ids near `pos` (all kinds if `kind` is u32::MAX), returns how many matched.
    pub entities_near: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [f64; 3], radius: f64, kind: u32, out: *mut u64, max: usize) -> usize,
    /// An entity's component bytes, null if it has none. Changes are found by comparing with
    /// what was last sent, so writing through the pointer is enough.
    pub component: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, comp: u32) -> *mut u8,
    pub player_count: unsafe extern "C" fn(world: *mut WorldCtx) -> u32,
    pub player_at: unsafe extern "C" fn(world: *mut WorldCtx, index: u32) -> u64,
    /// Non-zero for a player in creative mode (no damage, hunger or item use costs).
    pub is_creative: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64) -> u32,

    pub inv_size: unsafe extern "C" fn(world: *mut WorldCtx, owner: u64) -> u32,
    pub inv_get: unsafe extern "C" fn(world: *mut WorldCtx, owner: u64, slot: u32) -> ItemStack,
    pub inv_set: unsafe extern "C" fn(world: *mut WorldCtx, owner: u64, slot: u32, stack: ItemStack),
    /// Merges into existing stacks first; returns what did not fit.
    pub inv_insert: unsafe extern "C" fn(world: *mut WorldCtx, owner: u64, stack: ItemStack) -> ItemStack,
    pub selected_slot: unsafe extern "C" fn(world: *mut WorldCtx, player: u64) -> u32,
    pub spawn_item: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [f64; 3], stack: ItemStack),
    pub block_entity: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [i32; 3]) -> u64,
    pub open_menu: unsafe extern "C" fn(world: *mut WorldCtx, player: u64, menu: u32, owner: u64),

    /// Group `name` of an open menu: its first menu slot and how many slots it has. 0 if none.
    /// Group `name` of an open menu: its first slot, slot count and columns.
    pub menu_group: unsafe extern "C" fn(menu: *mut MenuCtx, name: Str, out: *mut [u32; 3]) -> u32,
    /// Group `name` of a machine (`MenuDesc::entity`) in `entity`'s inventory: first index and
    /// count. 0 if the entity has no such group.
    pub inventory_group: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, name: Str, out: *mut [u32; 2]) -> u32,
    pub menu_get: unsafe extern "C" fn(menu: *mut MenuCtx, slot: u32) -> ItemStack,
    pub menu_set: unsafe extern "C" fn(menu: *mut MenuCtx, slot: u32, stack: ItemStack),

    /// Matches a grid against recipes of `kind`; returns the output stack (empty if none).
    pub recipe_match: unsafe extern "C" fn(kind: Str, grid: *const ItemStack, width: u32, height: u32) -> ItemStack,
    /// Processing recipe for one input item: output stack, time written to `time_out`.
    pub recipe_process: unsafe extern "C" fn(kind: Str, input: u32, time_out: *mut u32) -> ItemStack,
    /// The first recipe of `kind` whose inputs (with counts) are all in `stacks`, any order:
    /// its number + 1, or 0.
    pub recipe_find: unsafe extern "C" fn(kind: Str, stacks: *const ItemStack, count: u32) -> u32,
    pub recipe_info: unsafe extern "C" fn(kind: Str, recipe: u32, out: *mut RecipeInfo) -> u32,
    /// Output `index` of a recipe: the stack (empty for a fluid) and its chance.
    pub recipe_output: unsafe extern "C" fn(kind: Str, recipe: u32, index: u32, stack: *mut ItemStack, chance: *mut f32) -> u32,
    /// A value the recipe carries: its bytes into `out` (up to `cap`); returns the value's size,
    /// 0 if it has none.
    pub recipe_value: unsafe extern "C" fn(kind: Str, recipe: u32, component: Str, out: *mut u8, cap: u32) -> u32,
    /// Takes the recipe's inputs out of `stacks` (counts, the matching stacks); 0 if they are not
    /// all there, then nothing changes.
    pub recipe_consume: unsafe extern "C" fn(kind: Str, recipe: u32, stacks: *mut ItemStack, count: u32) -> u32,
    /// The block state an item places, 0 if none.
    pub item_block: unsafe extern "C" fn(item: u32) -> u32,

    pub ui_open: unsafe extern "C" fn(ui: *mut UiCtx, node: *const UiNode),
    pub ui_close: unsafe extern "C" fn(ui: *mut UiCtx),
    /// From a text binding: set the translation key and args shown (an empty key with one
    /// argument shows that argument as it is).
    pub ui_text: unsafe extern "C" fn(ui: *mut UiCtx, key: Str, args: Slice<UiArg>),
    pub client_player: unsafe extern "C" fn(client: *mut ClientCtx) -> u64,
    /// Synced component of an entity the client knows (own player always), null if unknown.
    pub client_component: unsafe extern "C" fn(client: *mut ClientCtx, id: u64, comp: u32) -> *const u8,
    pub client_entity: unsafe extern "C" fn(client: *mut ClientCtx, id: u64, out: *mut EntityState) -> u32,
    /// Non-zero when the local player is in creative mode (client side, for HUDs and screens).
    pub client_creative: unsafe extern "C" fn(client: *mut ClientCtx) -> u32,

    /// Energy contract (`energy` component): returns how much was (or would be, when `simulate`
    /// is non-zero) accepted / taken, respecting capacity and rate limits.
    pub energy_insert: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, amount: i64, simulate: u32) -> i64,
    pub energy_extract: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, amount: i64, simulate: u32) -> i64,
    /// A line for the player who ran the command: translation `key` with `list`, formatted by
    /// their client in its language. Call again for more lines.
    pub command_reply: unsafe extern "C" fn(args: *mut CommandArgs, key: Str, list: Slice<UiArg>),
    /// Non-zero when a player in this world shows gizmo group `group` (`/gizmo <group>`).
    pub gizmo_on: unsafe extern "C" fn(world: *mut WorldCtx, group: Str) -> u32,
    /// Debug shape for players showing `group`: `kind` 0 line from `a` to `b`, 1 box between
    /// corners `a` and `b`, 2 the translation `key` with `args` at `a`, 3 filled box; `color` is
    /// 0xRRGGBBAA, kept for `ticks`.
    pub gizmo: unsafe extern "C" fn(world: *mut WorldCtx, group: Str, kind: u32, a: *const [f64; 3], b: *const [f64; 3], color: u32, ticks: u32, key: Str, args: Slice<UiArg>),

    // ------------------------------------------------ client side and mod channels
    /// Client callback, see `client_hook`.
    pub reg_client_hook: unsafe extern "C" fn(reg: *mut Registrar, hook: u32, cb: Callback),
    /// Cosmetics of entities of `kind` on the client (`ClientEntityArgs`), every frame, instead of
    /// the engine's (steps, idle noises, the smoke when a death ends); one per kind, a later mod's
    /// replaces an earlier one's.
    pub reg_client_entity_tick: unsafe extern "C" fn(reg: *mut Registrar, kind: Str, cb: Callback),
    /// The engine's own cosmetics for this entity, what a kind without a tick gets.
    pub client_entity_defaults: unsafe extern "C" fn(args: *mut ClientEntityArgs),
    /// Message channel `name` between a mod's client and server parts, carrying values of
    /// `schema` (both sides must agree, sends with another are refused). `side` 0: `cb` runs on
    /// the server with `PacketArgs` when a client sends; 1: on the client with `ClientPacketArgs`.
    pub reg_packet: unsafe extern "C" fn(reg: *mut Registrar, name: Str, schema: Str, side: u32, cb: Callback),
    /// Key binding `name` (shown in Controls as `key.<ns>.<name>`), default key by its settings
    /// name (`KeyJ`, `mouse3`, ...). Read with `client_pressed`.
    pub reg_key: unsafe extern "C" fn(reg: *mut Registrar, name: Str, key: Str),
    /// A mistake in what a mod registers that the SDK caught (a bad asset path ...): loading
    /// stops at freeze with it, named with the mod.
    pub reg_error: unsafe extern "C" fn(reg: *mut Registrar, message: Str),
    /// Value of `--name value` on the command line, empty if not given.
    pub launch_option: unsafe extern "C" fn(name: Str) -> Str,

    /// Server: sends `data` on channel `name` to `player`'s client (after this tick's changes).
    pub send_packet: unsafe extern "C" fn(world: *mut WorldCtx, player: u64, name: Str, schema: Str, data: Slice<u8>),
    /// Opens `menu` on the block entity at `pos`. 0 when there is none: the reason is logged and
    /// nothing opens.
    pub open_menu_at: unsafe extern "C" fn(world: *mut WorldCtx, player: u64, menu: u32, pos: *const [i32; 3]) -> u32,
    /// A virtual inventory of `slots` (an invisible entity at `pos`, never saved) that menus can be
    /// opened on like on a chest. Remove it with `entity_remove`.
    pub inventory_create: unsafe extern "C" fn(world: *mut WorldCtx, slots: u32, pos: *const [f64; 3]) -> u64,
    /// Moves an entity (players too, their client follows) at the end of the tick; `dim` is a
    /// dimension id, `u32::MAX` = the same one.
    pub entity_teleport: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, dim: u32, pos: *const [f64; 3]),
    /// Sets this dimension's time of day counter at the end of the tick.
    pub set_time: unsafe extern "C" fn(world: *mut WorldCtx, time: u64),
    /// Fills a ball with one block at the end of the tick, the whole dimension at once: loaded
    /// cubes on every core, the rest when they load (`AreaFillDesc`).
    pub area_fill: unsafe extern "C" fn(world: *mut WorldCtx, desc: *const AreaFillDesc),
    /// 0 survival, 1 creative, 2 spectator.
    pub set_game_mode: unsafe extern "C" fn(world: *mut WorldCtx, player: u64, mode: u32),

    /// Client, during any client callback: the local view, see `ClientState`.
    pub client_state: unsafe extern "C" fn(client: *mut ClientCtx, out: *mut ClientState),
    /// Client frame hooks only: simulated input, see `client_input`. Returns 0 for an unknown
    /// action or key.
    pub client_input: unsafe extern "C" fn(client: *mut ClientCtx, op: u32, text: Str, x: f32, y: f32) -> u32,
    /// Bit 1: action pressed this frame, bit 2: held.
    pub client_pressed: unsafe extern "C" fn(client: *mut ClientCtx, action: Str) -> u32,
    /// Client frame hooks only: moves/turns the local player directly.
    pub client_set_player: unsafe extern "C" fn(client: *mut ClientCtx, control: *const PlayerControl),
    /// Client frame hooks only: see `client_op`.
    pub client_command: unsafe extern "C" fn(client: *mut ClientCtx, op: u32, text: Str, num: f64) -> u64,
    /// Texts the client knows, see `client_text`. Valid until the next call.
    pub client_text: unsafe extern "C" fn(client: *mut ClientCtx, which: u32, index: u32) -> Str,
    /// 1 and the answer's lines (in the client's language, joined by `\n`) once command `id`
    /// (`client_op::RUN_COMMAND`) was answered, 0 before. An answer is read once.
    pub client_command_reply: unsafe extern "C" fn(client: *mut ClientCtx, id: u32, out: *mut Str) -> u32,
    /// Block the client has at `pos`, `u32::MAX` if that cube is not loaded there.
    pub client_block: unsafe extern "C" fn(client: *mut ClientCtx, pos: *const [i32; 3]) -> u32,
    /// Light the client has at `pos` (sky level in the low byte, then block red, green, blue),
    /// `u32::MAX` if that cube is not loaded there.
    pub client_light: unsafe extern "C" fn(client: *mut ClientCtx, pos: *const [i32; 3]) -> u32,
    /// Group `name` of the player inventory (`reg_player_inventory`, equipment slots,
    /// `core:equipment`): first slot and count.
    pub player_group: unsafe extern "C" fn(name: Str, out: *mut [u32; 2]) -> u32,
    /// `container` 0 own inventory, 1 the open menu's slots, 2 the menu cursor.
    pub client_slot: unsafe extern "C" fn(client: *mut ClientCtx, container: u32, slot: u32) -> ItemStack,
    /// Open menu as (kind, window, slot count); 0 if none.
    pub client_menu: unsafe extern "C" fn(client: *mut ClientCtx, out: *mut [u32; 3]) -> u32,
    /// Ids of entities the client knows (the local player not included), returns how many.
    pub client_entities: unsafe extern "C" fn(client: *mut ClientCtx, out: *mut u64, max: usize) -> usize,
    /// Where a UI element of the top screen is, as fractions of the window (x, y, w, h):
    /// `slot:N` a menu slot, `item:N` a creative inventory entry of item id N, `id:name` a node
    /// id, `text:words` the first text containing them (case-insensitive).
    pub client_ui_rect: unsafe extern "C" fn(client: *mut ClientCtx, query: Str, out: *mut [f32; 4]) -> u32,
    /// Client: draws for this frame only. `kind` 0 line, 1 box outline, 2 the translation `key`
    /// with `args` at a world position, 3 the same on screen at `a[0], a[1]` (fractions of the
    /// window), 4 filled box (see-through by its alpha: range previews, blueprints); color
    /// 0xRRGGBBAA.
    /// A `gizmo_kind` shape for this frame; with a `group` only while the player shows it.
    pub client_draw: unsafe extern "C" fn(client: *mut ClientCtx, group: Str, kind: u32, a: *const [f64; 3], b: *const [f64; 3], color: u32, key: Str, args: Slice<UiArg>),
    /// Client: sends `data` on channel `name` to the server.
    pub client_send_packet: unsafe extern "C" fn(client: *mut ClientCtx, name: Str, schema: Str, data: Slice<u8>),

    /// RGBA pixels of an image (a screenshot id from `client_op::SCREENSHOT` once taken, or one
    /// from `image_load`), null while not available.
    pub image_get: unsafe extern "C" fn(id: u64, w: *mut u32, h: *mut u32) -> *const u8,
    /// Reads a PNG file, 0 if it cannot.
    pub image_load: unsafe extern "C" fn(path: Str) -> u64,
    pub image_save: unsafe extern "C" fn(path: Str, rgba: Slice<u8>, w: u32, h: u32) -> u32,
    pub image_free: unsafe extern "C" fn(id: u64),
    /// Full name of a frozen id (`find` backwards); a block state gives `ns:block` or
    /// `ns:block#i` for its `i`-th state. Empty if unknown. Valid for the whole session.
    pub name_of: unsafe extern "C" fn(kind: u32, id: u32) -> Str,

    // ------------------------------------------------ liquids
    /// Makes a registered block (16 liquid states, see `LiquidDesc`) a liquid the engine flows.
    pub reg_liquid: unsafe extern "C" fn(reg: *mut Registrar, desc: *const LiquidDesc),
    pub reg_liquid_reaction: unsafe extern "C" fn(reg: *mut Registrar, desc: *const ReactionDesc),
    /// Liquid id of a block state, `u32::MAX` for none.
    pub liquid_of: unsafe extern "C" fn(state: u32) -> u32,
    /// Places or takes a liquid like a bucket would for `player` holding `stack` (what the engine
    /// does for items with a `liquid` component or empty containers of one). Returns the stack
    /// the player holds afterwards; `handled` is set non-zero if anything happened.
    pub use_container: unsafe extern "C" fn(world: *mut WorldCtx, player: u64, stack: ItemStack, handled: *mut u32) -> ItemStack,

    // ------------------------------------------------ events, interceptors, modifiers, loot
    /// Subscribes to event `name` carrying values of `schema` (checked against every other use);
    /// `cb` gets `EventArgs` once per tick and region with every value queued since.
    pub reg_event_handler: unsafe extern "C" fn(reg: *mut Registrar, name: Str, schema: Str, cb: Callback),
    /// Event id, `u32::MAX` when nobody listens (then there is nothing to emit).
    pub find_event: unsafe extern "C" fn(name: Str, schema: Str) -> u32,
    /// Queues one value of event `event` (its schema's size) for the end of the tick.
    pub emit: unsafe extern "C" fn(world: *mut WorldCtx, event: u32, value: Slice<u8>),
    /// "May it happen" handler for question `name` (`question::*` or a mod's own): gets
    /// `InterceptArgs` and sets `Query::verdict`. Any deny wins.
    pub reg_interceptor: unsafe extern "C" fn(reg: *mut Registrar, name: Str, cb: Callback),
    /// Asks the interceptors of `name`; 1 if allowed.
    pub ask: unsafe extern "C" fn(world: *mut WorldCtx, name: Str, query: *mut Query) -> u32,
    /// Modifier on value chain `name`, run in order of `stage` (`stage::*`), then mod load order;
    /// gets `ModifyArgs`.
    pub reg_modifier: unsafe extern "C" fn(reg: *mut Registrar, name: Str, stage: u32, cb: Callback),
    /// Runs chain `name` from `base`. With `explain` non-zero the steps are kept for
    /// `explanation`.
    pub compute: unsafe extern "C" fn(world: *mut WorldCtx, name: Str, base: f64, ctx: *const Query, explain: u32) -> f64,
    /// Steps of the last explained `compute` or `attribute_explain` on this thread.
    pub explanation: unsafe extern "C" fn() -> Str,
    /// An attribute of living entities (`attribute::*` are the engine's).
    pub reg_attribute: unsafe extern "C" fn(reg: *mut Registrar, name: Str, default: f64, min: f64, max: f64),
    /// Its current value for an entity: base (the kind's component of the same name, else the
    /// default), then modifiers by `attr_op`, clamped.
    pub attribute: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, name: Str) -> f64,
    /// Adds (or replaces, same `id`) a modifier for `ticks` (0 = until removed). Saved.
    pub attribute_modifier: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, name: Str, id: Str, op: u32, amount: f64, ticks: u64),
    pub attribute_remove: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, name: Str, id: Str),
    /// Fills `explanation` with how the value came about; returns the value.
    pub attribute_explain: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, name: Str) -> f64,
    pub reg_loot: unsafe extern "C" fn(reg: *mut Registrar, desc: *const LootDesc),
    pub reg_loot_modifier: unsafe extern "C" fn(reg: *mut Registrar, desc: *const LootModDesc),
    /// Rolls table `name`; writes at most `max` stacks to `out` and returns how many it made.
    pub roll_loot: unsafe extern "C" fn(world: *mut WorldCtx, name: Str, ctx: *const LootCtx, out: *mut ItemStack, max: usize) -> usize,
    /// Breaks a block as if mined with `tool` (by `player`, 0 for none): asks `break_block`,
    /// drops its loot when the tool may harvest it and `drops` is non-zero, fires `block_broken`.
    /// 0 if nothing was broken.
    pub break_block: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [i32; 3], tool: ItemStack, player: u64, drops: u32) -> u32,
    /// Declares a stack component's flags (`stack_flags`).
    pub reg_stack_component: unsafe extern "C" fn(reg: *mut Registrar, name: Str, schema: Str, flags: u32),
    /// Client: the entity the open menu belongs to (a machine), 0 if none.
    pub client_menu_owner: unsafe extern "C" fn(client: *mut ClientCtx) -> u64,
    /// A system: once per tick and region `cb` gets `SystemArgs` with every entity that has the
    /// per-entity component `name` and their values side by side, to change in place.
    pub reg_system: unsafe extern "C" fn(reg: *mut Registrar, name: Str, schema: Str, cb: Callback),
    /// A command that runs on the client that types it (never sent to the server), same usage
    /// syntax as `reg_command`; `cb` gets `CommandArgs` with `client` set.
    pub reg_client_command: unsafe extern "C" fn(reg: *mut Registrar, name: Str, usage: Str, cb: Callback),
    /// Adds alternatives (`usage` syntax) to an existing command, handled by `cb` with
    /// `CommandArgs` laid out by this usage; runs where the command runs.
    pub reg_command_branch: unsafe extern "C" fn(reg: *mut Registrar, command: Str, usage: Str, client: u32, cb: Callback),

    // ------------------------------------------------ text, shaders, particles
    /// Condition `<ns>:<name>` for `{if:...}` in texts, answered by `cb` (`TextConditionArgs`)
    /// on the client while a text using it is on screen.
    pub reg_text_condition: unsafe extern "C" fn(reg: *mut Registrar, name: Str, cb: Callback),
    /// A particle kind, see `ParticleDesc`.
    pub reg_particle: unsafe extern "C" fn(reg: *mut Registrar, desc: *const ParticleDesc),
    /// Server: particles for every player that sees `burst.pos`.
    pub particles: unsafe extern "C" fn(world: *mut WorldCtx, burst: *const ParticleBurst),
    /// Client: particles only this client shows.
    pub client_particles: unsafe extern "C" fn(client: *mut ClientCtx, burst: *const ParticleBurst),

    // ------------------------------------------------ configs, sound, models
    /// A setting players change in Settings > Mods (and `config/<ns>.txt`), see `ConfigDesc`.
    pub reg_config: unsafe extern "C" fn(reg: *mut Registrar, desc: *const ConfigDesc),
    /// Current value of config `id` (bools 0/1, choices their index); 0 for unknown ids.
    pub config_value: unsafe extern "C" fn(id: u32) -> f64,
    /// Changes config `id` (clamped to its range, saved). Synced ones only on the server.
    pub config_set: unsafe extern "C" fn(id: u32, value: f64),
    /// The value of a `TEXT` setting.
    pub config_text: unsafe extern "C" fn(id: u32) -> Str,
    pub config_set_text: unsafe extern "C" fn(id: u32, value: Str),
    /// A sound event, see `SoundDesc`.
    pub reg_sound: unsafe extern "C" fn(reg: *mut Registrar, desc: *const SoundDesc),
    /// Background music, see `MusicDesc`.
    pub reg_music: unsafe extern "C" fn(reg: *mut Registrar, desc: *const MusicDesc),
    /// Server: entity `id` plays keyframe animation `name` (from its kind's animation file) for
    /// everyone who sees it; `looping` keeps it going until another one starts, empty `name`
    /// goes back to idle/walk.
    pub play_animation: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, name: Str, looping: u32),
    /// Server: sound event `sound` (`find` with `entry_kind::SOUND`) at `pos` for every player in
    /// its range; `pitch` 0 picks one from the event's range.
    pub play_sound: unsafe extern "C" fn(world: *mut WorldCtx, sound: u32, pos: *const [f64; 3], volume: f32, pitch: f32),
    /// Client: only this client hears it; `pos` null = not positioned (UI).
    pub client_play_sound: unsafe extern "C" fn(client: *mut ClientCtx, sound: u32, pos: *const [f64; 3], volume: f32, pitch: f32),
    /// A natural spawning rule, see `SpawnDesc`.
    pub reg_spawn: unsafe extern "C" fn(reg: *mut Registrar, desc: *const SpawnDesc),
    /// Biome id at a block (underground biomes included); 0 where nothing is loaded.
    pub biome_at: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [i32; 3]) -> u32,
    /// Entity `id` walks to `target` by itself along a path around obstacles (a moving target can
    /// be given every tick), at `speed` times its walking speed; 0 stops it. Returns 1 when the
    /// walk was accepted.
    pub navigate: unsafe extern "C" fn(world: *mut WorldCtx, id: u64, target: *const [f64; 3], speed: f32) -> u32,
    /// Whether `id` is still walking somewhere.
    pub navigating: unsafe extern "C" fn(world: *mut WorldCtx, id: u64) -> u32,
    pub reg_structure: unsafe extern "C" fn(reg: *mut Registrar, desc: *const StructureDesc),
    pub reg_template: unsafe extern "C" fn(reg: *mut Registrar, desc: *const TemplateDesc),
    /// A `blocks` component value's list: (state, weight) pairs.
    pub ref_list: unsafe extern "C" fn(index: u32) -> Slice<[u32; 2]>,
    /// Places template `name` with its lowest corner at `pos`; 0 if it doesn't exist or part of
    /// it isn't loaded (nothing is placed then).
    pub world_place: unsafe extern "C" fn(world: *mut WorldCtx, name: Str, pos: *const [i32; 3]) -> u32,
    /// The nearest structure `name` within `radius` blocks of `pos` (whether generated yet or
    /// not): its lowest corner in `out`. Returns 0 if none.
    pub structure_near: unsafe extern "C" fn(world: *mut WorldCtx, name: Str, pos: *const [i32; 3], radius: i32, out: *mut [i32; 3]) -> u32,
    /// The block at `pos` falls as an entity and is placed again where it lands; returns the
    /// entity (0 if there was no block).
    pub fall_block: unsafe extern "C" fn(world: *mut WorldCtx, pos: *const [i32; 3]) -> u64,
    /// Gives every entity of `kind` the component `component` (any mod's or the engine's) at
    /// spawn, starting as `value` (the component's size in bytes), empty: the component's default.
    pub reg_attach_value: unsafe extern "C" fn(reg: *mut Registrar, kind: Str, component: Str, value: Slice<u8>),
    /// A kind of recipe (`base:crafting`): recipes, stations and loot naming a kind nobody
    /// registered are freeze errors.
    pub reg_recipe_kind: unsafe extern "C" fn(reg: *mut Registrar, desc: *const RecipeKindDesc),
    /// An entity category: at most `cap` of it spawn naturally within 64 blocks of a player;
    /// with `despawn` > 0 they vanish when no player is that close.
    pub reg_entity_category: unsafe extern "C" fn(reg: *mut Registrar, name: Str, cap: u32, despawn: f32),
    /// A sound category with its own volume setting (`<ns>:volume_<name>`); `world` != 0: heard
    /// through caves and water like block sounds (else like music and UI).
    pub reg_sound_category: unsafe extern "C" fn(reg: *mut Registrar, name: Str, world: u32),
    /// A tint kind: blocks naming it take a color per biome, `default` (0xRRGGBB) where the biome
    /// sets none and on items.
    pub reg_tint_kind: unsafe extern "C" fn(reg: *mut Registrar, name: Str, default: u32),
    /// The color of a tint kind in a biome (any mod's of either).
    pub reg_biome_tint: unsafe extern "C" fn(reg: *mut Registrar, biome: Str, kind: Str, color: u32),
    /// The player inventory as named groups (one flagged `slot_flags::HOTBAR`); the engine lays
    /// them out, the hotbar first and the equipment slots last. Of several, the mod loaded last
    /// wins.
    pub reg_player_inventory: unsafe extern "C" fn(reg: *mut Registrar, groups: Slice<SlotGroupDesc>),
    /// An equipment slot of players: `parts` (`worn_parts`) of the model what is worn there
    /// covers, `background` the GUI texture of the empty slot. It takes items tagged
    /// `worn/<name>`, is never filled by pickups and shows on the model.
    pub reg_equipment_slot: unsafe extern "C" fn(reg: *mut Registrar, name: Str, parts: u32, background: Str),
    /// A screen, opened by name (`client_op::OPEN_SCREEN`, `--screen`); one named like an engine
    /// screen (`core:pause`) replaces it.
    pub reg_screen: unsafe extern "C" fn(reg: *mut Registrar, desc: *const ScreenDesc),
    /// Adds comma separated words to a word set: command parameters `word(#set)` complete from
    /// it (`core:rules`, `core:gizmos`, or any mod's).
    pub reg_words: unsafe extern "C" fn(reg: *mut Registrar, set: Str, words: Str),
    /// An entity's own box (width, height, eye height) instead of its kind's; `width` <= 0 goes
    /// back to the kind's. Crouching shrinks whichever applies.
    pub set_entity_size: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, width: f32, height: f32, eye: f32),
    /// Draws an entity with another kind's model, texture and animations (it stays what it is);
    /// empty goes back to its own.
    pub set_entity_look: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, kind: Str),
    /// A block of moving structure `entity` (its own coordinates); `u32::MAX` if it is none.
    pub structure_block: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, pos: *const [i32; 3]) -> u32,
    /// Sets a block of moving structure `entity` like `set_block` (updates, callbacks); 0 if it
    /// is none.
    pub set_structure_block: unsafe extern "C" fn(world: *mut WorldCtx, entity: u64, pos: *const [i32; 3], state: u32) -> u32,
    /// Adds `cb` (`UiArgs`) to place `area` of screen `screen` (`core:title` has `buttons` and
    /// `side`), lowest `order` first. Engine screens and mods' own build these places.
    pub reg_screen_part: unsafe extern "C" fn(reg: *mut Registrar, screen: Str, area: Str, order: i32, cb: Callback),
    /// Describes or edits the terrain of a dimension, see `TerrainDesc`.
    pub reg_terrain: unsafe extern "C" fn(reg: *mut Registrar, desc: *const TerrainDesc),
    /// Column (`x`, `z`) of dimension `dim` for `seed` (its terrain's or `DimensionDesc::column`),
    /// generated or not. Returns 0 if the dimension has neither.
    pub dim_column: unsafe extern "C" fn(dim: u32, seed: u64, x: i32, z: i32, out: *mut TerrainColumn) -> u32,
    /// 1 if caves took the surface block of column (`x`, `z`), so nothing should stand there.
    pub dim_carved: unsafe extern "C" fn(dim: u32, seed: u64, x: i32, z: i32) -> u32,
}

/// Natural spawning around players (`Host::reg_spawn`). Every two seconds the engine tries a few
/// spots 20 to 45 blocks from each player; a spot takes one of the rules whose conditions all
/// hold there (by weight) while that entity category is under its cap near the player (8
/// hostile, 6 passive, 6 other within 64 blocks). The `core:mob_spawning` rule turns it off.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpawnDesc {
    pub entity: Str,
    /// Tag the spot's biome must carry (`forest`, `cave`), empty: any biome.
    pub biome: Str,
    /// Tag the block under the spot must carry, empty: any solid block.
    pub ground: Str,
    /// Full dimension name, empty: any.
    pub dimension: Str,
    /// Inclusive range of light at the spot: the brighter of sky light times daylight and
    /// block light.
    pub light: [u32; 2],
    pub y: [i32; 2],
    /// How many spawn together.
    pub group: [u32; 2],
    pub weight: u32,
    /// Chance per attempt once the rule is picked.
    pub chance: f32,
}
defaults!(SpawnDesc {
    entity: Str::EMPTY,
    biome: Str::EMPTY,
    ground: Str::EMPTY,
    dimension: Str::EMPTY,
    light: [0, 15],
    y: [i32::MIN, i32::MAX],
    group: [1, 1],
    weight: 1,
    chance: 1.0,
});

/// Light as the world stores it: a byte each for sky, red, green and blue, 0..15.
pub mod light {
    pub const fn sky(l: u32) -> u32 {
        l & 0xFF
    }
    /// The strongest block light channel.
    pub fn block(l: u32) -> u32 {
        ((l >> 8) & 0xFF).max((l >> 16) & 0xFF).max((l >> 24) & 0xFF)
    }
    /// What spawning and growth go by: sky light dimmed by the daylight (0..1), or block light,
    /// whichever is more.
    pub fn effective(l: u32, daylight: f32) -> u32 {
        ((sky(l) as f32 * daylight) as u32).max(block(l)).min(15)
    }
}

/// `UiArg::kind`.
pub mod ui_arg {
    pub const TEXT: u32 = 0;
    /// A translation key.
    pub const KEY: u32 = 1;
    pub const INT: u32 = 2;
    pub const FLOAT: u32 = 3;
    /// An item id, shown as its icon.
    pub const ITEM: u32 = 4;
    /// A display name, `text` like in `{name:..}`: `item:base:coal`, `mod:base`, `key:core:jump`.
    pub const NAME: u32 = 5;
}

/// `UiNode::align`.
pub mod ui_align {
    pub const START: u32 = 0;
    pub const CENTER: u32 = 1;
    pub const END: u32 = 2;
    pub const STRETCH: u32 = 3;
}

/// `UiNode::justify`.
pub mod ui_justify {
    pub const START: u32 = 0;
    pub const CENTER: u32 = 1;
    pub const END: u32 = 2;
    pub const BETWEEN: u32 = 3;
}

/// Debug shapes, the same for `Host::gizmo` (server, for some ticks) and `Host::client_draw`
/// (this frame).
pub mod gizmo_kind {
    /// From `a` to `b`.
    pub const LINE: u32 = 0;
    /// Edges of the box `a`..`b`.
    pub const BOX: u32 = 1;
    /// `text` at `a`, `b.x` times the size of menu text (0 counts as 1).
    pub const LABEL: u32 = 2;
    /// The box `a`..`b` filled, see-through by the color's alpha.
    pub const FILL: u32 = 3;
    /// Client only: `text` centered at fractions of the window (`a[0]`, `a[1]`), sized like
    /// `LABEL`.
    pub const SCREEN_LABEL: u32 = 4;
}

/// `Host::reg_entity_hook`.

/// Tint kinds of block faces: color from the biome. The engine has `core:grass`,
/// `core:foliage` and `core:water`; mods add their own with `Host::reg_tint_kind` and name them
/// in `StateDesc::tint_name`.
pub mod tint {
    /// Kinds that fit the face and quad records, the engine's three included.
    pub const MAX: u32 = 15;
}

/// A sound event: plays one of `files` (`sounds/<name>.ogg` or `.wav` in any mod or resource
/// pack) picked at random.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SoundDesc {
    pub name: Str,
    pub cell: *const AtomicU32,
    pub files: Slice<Str>,
    pub volume: f32,
    /// Playback speed picked between the two (1 = as recorded).
    pub pitch: [f32; 2],
    /// Blocks away it can still be heard (at volume 1).
    pub range: f32,
    /// Category by name, for its volume setting: the engine's `core:blocks` (empty),
    /// `core:creatures`, `core:players`, `core:ambient`, `core:music`, `core:ui`, or one from
    /// `Host::reg_sound_category`.
    pub category_name: Str,
}
defaults!(SoundDesc { name: Str::EMPTY, cell: core::ptr::null(), files: Slice::EMPTY, volume: 1.0, pitch: [1.0, 1.0], range: 16.0, category_name: Str::EMPTY });

/// When background music may play; 0 = always.
pub mod music_when {
    pub const DAY: u32 = 1;
    pub const NIGHT: u32 = 2;
    /// No sky over the player.
    pub const UNDERGROUND: u32 = 4;
    pub const SURFACE: u32 = 8;
    pub const CREATIVE: u32 = 16;
}

/// Background music: after a quiet while, a track of the first entry whose conditions hold.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MusicDesc {
    pub name: Str,
    /// Sound files streamed (`sounds/<name>.ogg|.wav`).
    pub tracks: Slice<Str>,
    /// Full dimension name it plays in, empty = any.
    pub dimension: Str,
    pub when: u32,
}
defaults!(MusicDesc { name: Str::EMPTY, tracks: Slice::EMPTY, dimension: Str::EMPTY, when: 0 });

/// Kinds of `ConfigDesc` values.
pub mod config_kind {
    pub const BOOL: u32 = 0;
    pub const INT: u32 = 1;
    pub const FLOAT: u32 = 2;
    /// One of `choices`, stored as its index.
    pub const CHOICE: u32 = 3;
    /// Text (`Host::config_text`); its default is `ConfigDesc::text`. Client scope only.
    pub const TEXT: u32 = 4;
}

pub mod config_flags {
    /// Takes effect after a restart; the settings screen says so.
    pub const RESTART: u32 = 2;
}

/// Where a setting's value lives.
pub mod config_scope {
    /// Each player's own, in their `config/<ns>.txt`.
    pub const CLIENT: u32 = 0;
    /// The server's, in its `config/<ns>.txt`; its players get it, and every change.
    pub const SERVER: u32 = 1;
    /// Saved with the world (game rules, changed with `/rule`); players get it like the server's.
    pub const WORLD: u32 = 2;
}

/// A mod setting. Shown as `config.<ns>.<key>` (and `.tooltip`) from the language files.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConfigDesc {
    /// `ns:key`.
    pub name: Str,
    pub cell: *const AtomicU32,
    pub kind: u32,
    pub default: f64,
    /// Range of ints and floats, and the step the slider moves by (0 = any).
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub choices: Slice<Str>,
    pub flags: u32,
    /// `config_scope`.
    pub scope: u32,
    /// The default of a `TEXT` setting.
    pub text: Str,
}
defaults!(ConfigDesc { name: Str::EMPTY, cell: core::ptr::null(), kind: config_kind::BOOL, default: 0.0, min: 0.0, max: 1.0, step: 0.0, choices: Slice::EMPTY, flags: 0, scope: config_scope::CLIENT, text: Str::EMPTY });

#[repr(C)]
pub struct TextConditionArgs {
    pub client: *mut ClientCtx,
    /// Out: non-zero when the condition holds.
    pub result: u32,
}

pub mod particle_flags {
    /// Stops on blocks, like vanilla's particles.
    pub const COLLIDE: u32 = 1;
    /// Ignores light (flames, sparks).
    pub const FULLBRIGHT: u32 = 2;
    /// Alpha blended (smoke fading out) instead of cut out.
    pub const TRANSLUCENT: u32 = 4;
    /// Shrinks to nothing over its life.
    pub const SHRINK: u32 = 8;
}

/// A particle kind. Particles live only on clients; the server and mods send bursts.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ParticleDesc {
    pub name: Str,
    pub cell: *const AtomicU32,
    /// Frames shown in order over its life; empty: a random piece of the burst's block texture
    /// (block dust).
    pub textures: Slice<Str>,
    /// Seconds, picked between the two.
    pub life: [f32; 2],
    /// Edge length in blocks, picked between the two.
    pub scale: [f32; 2],
    /// Blocks per second squared, pulling down.
    pub gravity: f32,
    /// Fraction of its speed kept per tick (20 per second); vanilla's particles 0.98.
    pub drag: f32,
    /// 0xRRGGBBAA at birth and at the end of its life, blended in between.
    pub color: u32,
    pub color_end: u32,
    pub flags: u32,
    /// Fragment shader `shaders/particle/<name>.wgsl`, empty for the plain texture.
    pub shader: Str,
}
defaults!(ParticleDesc {
    name: Str::EMPTY,
    cell: core::ptr::null(),
    textures: Slice::EMPTY,
    life: [0.5, 1.0],
    scale: [0.1, 0.2],
    gravity: 0.0,
    drag: 0.98,
    color: 0xFFFFFFFF,
    color_end: 0xFFFFFFFF,
    flags: 0,
    shader: Str::EMPTY,
});

/// Particles of one kind appearing together.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleBurst {
    /// Particle id (`find` with `entry_kind::PARTICLE`).
    pub kind: u32,
    pub pos: [f64; 3],
    /// Each particle starts up to this far from `pos` on each axis.
    pub spread: [f32; 3],
    /// Starting velocity in blocks per second.
    pub velocity: [f32; 3],
    /// Random extra velocity up to this, in any direction.
    pub speed: f32,
    pub count: u32,
    /// Block state the textureless kinds take their texture from.
    pub block: u32,
}
defaults!(ParticleBurst { kind: 0, pos: [0.0; 3], spread: [0.0; 3], velocity: [0.0; 3], speed: 0.0, count: 1, block: 0 });

/// Particle kinds the engine itself uses.
pub mod particle {
    /// Pieces of a block's texture (breaking, landing).
    pub const BLOCK: &str = "core:block";
    /// Smoke puffs (a mob dying).
    pub const POOF: &str = "core:poof";
}

/// Values are copied out before the call and written back after it (changes of synced
/// components get sent), so they form one dense, aligned array.
#[repr(C)]
pub struct SystemArgs {
    pub world: *mut WorldCtx,
    pub component: u32,
    pub ids: *const u64,
    pub values: *mut u8,
    pub count: u32,
    pub stride: u32,
    /// Per entity, the ticks this call stands for: 1, or more while it catches up on time it
    /// missed (loaded again, the world's time jumped).
    pub ticks: *const u32,
}

/// Engine events (`Host::reg_event_handler`); values are the structs named in each line.
pub mod event {
    /// `BlockChange`: a player or `break_block` broke a block (`item` = tool).
    pub const BLOCK_BROKEN: &str = "core:block_broken";
    /// `BlockChange`: a player placed a block (`item` = the item used).
    pub const BLOCK_PLACED: &str = "core:block_placed";
    /// `Death`.
    pub const ENTITY_DIED: &str = "core:entity_died";
    /// `PlayerEvent`.
    pub const PLAYER_JOINED: &str = "core:player_joined";
    /// `Pickup`.
    pub const ITEM_PICKED_UP: &str = "core:item_picked_up";
    /// `Hurt`: damage was applied (after armor and every modifier).
    pub const ENTITY_HURT: &str = "core:entity_hurt";
    /// `ItemUsed`: a player used the held item and something happened.
    pub const ITEM_USED: &str = "core:item_used";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ItemUsed {
    pub player: u64,
    /// As it was before the use.
    pub stack: ItemStack,
    pub pos: [i32; 3],
    /// 1 when used on the block at `pos`.
    pub has_target: u32,
}
impl Schema for ItemUsed {
    const SCHEMA: &'static str = "{player:u64,stack:{item:u32,count:u32,data:u32},pos:[i32;3],has_target:u32}";
}

#[repr(C)]
pub struct WorldTickArgs {
    pub world: *mut WorldCtx,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hurt {
    pub entity: u64,
    pub source: u64,
    pub amount: f32,
    pub cause: u32,
}
impl Schema for Hurt {
    const SCHEMA: &'static str = "{entity:u64,source:u64,amount:f32,cause:u32}";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockChange {
    pub player: u64,
    pub pos: [i32; 3],
    pub state: u32,
    pub item: u32,
}
impl Schema for BlockChange {
    const SCHEMA: &'static str = "{player:u64,pos:[i32;3],state:u32,item:u32}";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Death {
    pub entity: u64,
    pub killer: u64,
    pub kind: u32,
    pub cause: u32,
    pub pos: [f64; 3],
}
impl Schema for Death {
    const SCHEMA: &'static str = "{entity:u64,killer:u64,kind:u32,cause:u32,pos:[f64;3]}";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerEvent {
    pub player: u64,
}
impl Schema for PlayerEvent {
    const SCHEMA: &'static str = "{player:u64}";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pickup {
    pub player: u64,
    pub stack: ItemStack,
}
impl Schema for Pickup {
    const SCHEMA: &'static str = "{player:u64,stack:{item:u32,count:u32,data:u32}}";
}

#[repr(C)]
pub struct EventArgs {
    pub world: *mut WorldCtx,
    pub event: u32,
    pub data: *const u8,
    pub count: u32,
    /// Bytes per value.
    pub stride: u32,
}

/// Questions the engine asks before doing things (`Host::reg_interceptor`).
pub mod question {
    /// `entity` the player (0 for `break_block` without one), `pos`, `state`, `item` the tool.
    pub const BREAK_BLOCK: &str = "core:break_block";
    /// `entity`, `pos`, `state` to be placed, `item` used.
    pub const PLACE_BLOCK: &str = "core:place_block";
    /// Right click on a block: `entity`, `pos`, `state`.
    pub const USE_BLOCK: &str = "core:use_block";
    /// `entity` about to change dimension to `dim`.
    pub const ENTER_DIMENSION: &str = "core:enter_dimension";
    /// `entity` (a player) picking up `item` (count in `amount`), `other` the item entity.
    pub const PICKUP_ITEM: &str = "core:pickup_item";
    /// `entity` attacking `other` with `item`.
    pub const ATTACK: &str = "core:attack";
    /// `entity` about to be hurt by `other` for `amount` of `cause`, before the damage chain.
    pub const HURT: &str = "core:hurt";
    /// A player (`other`) right clicking `entity` holding `item`.
    pub const INTERACT_ENTITY: &str = "core:interact_entity";
    /// A player (`entity`) using the held `item`, at block `pos` when `state` is not 0.
    pub const USE_ITEM: &str = "core:use_item";
}

pub mod verdict {
    pub const ABSTAIN: u32 = 0;
    pub const ALLOW: u32 = 1;
    pub const DENY: u32 = 2;
}

/// What a question or a value chain is about; fields one does not use are 0.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Query {
    pub verdict: u32,
    pub entity: u64,
    pub other: u64,
    pub pos: [i32; 3],
    pub state: u32,
    pub item: u32,
    pub dim: u32,
    pub cause: u32,
    pub amount: f64,
}
defaults!(Query { verdict: 0, entity: 0, other: 0, pos: [0; 3], state: 0, item: 0, dim: 0, cause: 0, amount: 0.0 });

#[repr(C)]
pub struct InterceptArgs {
    pub world: *mut WorldCtx,
    pub question: Str,
    pub query: *mut Query,
}

/// Stage of a modifier; steps of one stage run in mod load order.
pub mod stage {
    pub const ADD: u32 = 100;
    pub const MULTIPLY: u32 = 200;
    pub const FINAL: u32 = 300;
}

/// Value chains the engine runs.
pub mod chain {
    /// Damage an entity takes (`entity` hurt by `other`, `cause`), after `on_hurt` hooks.
    pub const DAMAGE: &str = "core:damage";
    /// Melee damage of `entity` hitting `other` holding `item`, from the item's
    /// `attack_damage`.
    pub const ATTACK_DAMAGE: &str = "core:attack_damage";
}

#[repr(C)]
pub struct ModifyArgs {
    pub world: *mut WorldCtx,
    pub chain: Str,
    pub ctx: *const Query,
    pub base: f64,
    /// In/out.
    pub value: f64,
}

/// Attributes the engine reads.
pub mod attribute {
    pub const MAX_HEALTH: &str = "core:max_health";
    /// Multiplier on walking speed (1 = the kind's speed).
    pub const MOVEMENT_SPEED: &str = "core:movement_speed";
    /// Multiplier on how far mobs notice it (1; 0 in creative, so mobs ignore the player; a mod
    /// can lower it, like vanilla's mob heads).
    pub const VISIBILITY: &str = "core:visibility";
    /// Multiplier on jump speed (1: a block and a quarter high).
    pub const JUMP_STRENGTH: &str = "core:jump_strength";
}

/// How an attribute modifier applies, like vanilla: ADD amounts sum onto the base, then times
/// (1 + the sum of MUL_BASE), then times (1 + each MUL_TOTAL).
pub mod attr_op {
    pub const ADD: u32 = 0;
    pub const MUL_BASE: u32 = 1;
    pub const MUL_TOTAL: u32 = 2;
}

pub mod loot_cond {
    /// Passes with probability `value`.
    pub const CHANCE: u32 = 0;
    /// The tool has tag `text`.
    pub const TOOL_TAG: u32 = 1;
    /// The tool lacks tag `text`.
    pub const NOT_TOOL_TAG: u32 = 2;
    /// The tool's `tool_level` is at least `value`.
    pub const TOOL_LEVEL: u32 = 3;
    pub const KILLED_BY_PLAYER: u32 = 4;
    pub const ON_FIRE: u32 = 5;
    /// The block was harvested: broken by the world, or mined with a tool its `harvest_level`
    /// accepts. Block tables made from `drops` have it on their pool.
    pub const HARVESTED: u32 = 6;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LootCond {
    pub kind: u32,
    pub value: f32,
    pub text: Str,
}
defaults!(LootCond { kind: 0, value: 1.0, text: Str::EMPTY });

/// One weighted choice of a pool: `item` (name, or `#tag` for the tag's first item; empty = no
/// item), `min..=max` of it. A burning victim drops it cooked by recipe type `smelt` if set.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LootEntry {
    pub item: Str,
    pub weight: u32,
    pub min: u32,
    pub max: u32,
    pub conditions: Slice<LootCond>,
    pub smelt: Str,
}
defaults!(LootEntry { item: Str::EMPTY, weight: 1, min: 1, max: 1, conditions: Slice::EMPTY, smelt: Str::EMPTY });

/// Rolls `rolls_min..=rolls_max` times if its conditions pass; each roll picks one entry by
/// weight among those whose conditions pass.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LootPool {
    pub rolls_min: u32,
    pub rolls_max: u32,
    pub conditions: Slice<LootCond>,
    pub entries: Slice<LootEntry>,
}
defaults!(LootPool { rolls_min: 1, rolls_max: 1, conditions: Slice::EMPTY, entries: Slice::EMPTY });

/// A loot table. Blocks and entities use the table named like them; mods may name any others
/// (chests).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LootDesc {
    pub name: Str,
    pub pools: Slice<LootPool>,
}
defaults!(LootDesc { name: Str::EMPTY, pools: Slice::EMPTY });

pub mod loot_op {
    /// Adds `pool`.
    pub const ADD_POOL: u32 = 0;
    /// Every entry of item `from` gives `to` instead.
    pub const REPLACE_ITEM: u32 = 1;
    /// Drops entries of item `from`.
    pub const REMOVE_ITEM: u32 = 2;
    /// Empties the table (add pools after it).
    pub const CLEAR: u32 = 3;
}

/// A change to tables any mod made, applied after all tables in mod load order. `table` may end
/// in `*`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LootModDesc {
    pub table: Str,
    pub op: u32,
    pub pool: LootPool,
    pub from: Str,
    pub to: Str,
}
defaults!(LootModDesc { table: Str::EMPTY, op: 0, pool: LootPool::DEFAULT, from: Str::EMPTY, to: Str::EMPTY });

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LootCtx {
    pub tool: ItemStack,
    pub killer: u64,
    pub by_player: u32,
    pub on_fire: u32,
    /// The block state being mined with `tool` (0 = none), for `loot_cond::HARVESTED`.
    pub mined: u32,
}
defaults!(LootCtx { tool: ItemStack::EMPTY, killer: 0, by_player: 0, on_fire: 0, mined: 0 });

/// Flags of stack components (`Host::reg_stack_component`).
pub mod stack_flags {
    /// Kept on the server: clients get the stack without it.
    pub const SERVER_ONLY: u32 = 1;
    /// Not written to saves.
    pub const NO_SAVE: u32 = 2;
}

/// Flow rules of a liquid. Its block (registered by the same mod under the same name) has 16
/// states: 0 the source, 1..7 flowing with less and less liquid, 8 falling.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LiquidDesc {
    /// The block's full name.
    pub name: Str,
    /// Ticks between flow steps (vanilla water 5, lava 30).
    pub delay: u32,
    /// Level lost per block of spread: water 1 (7 blocks), lava 2 (3 blocks).
    pub drop_off: u32,
    /// How far flowing liquid looks for a way down before choosing directions (water 4, lava 2).
    pub slope_find: u32,
    /// A spot over solid ground (or a source) with at least this many sources beside it
    /// becomes one (vanilla water 2); 0 always; `u32::MAX` never.
    pub renewable: u32,
    /// Positive: falls; negative: rises (lighter than air).
    pub density: i32,
    /// Roughly Kelvin: water 300, lava 1300. From 1000 up it burns what touches it.
    pub temperature: u32,
}
defaults!(LiquidDesc { name: Str::EMPTY, delay: 5, drop_off: 1, slope_find: 4, renewable: u32::MAX, density: 1000, temperature: 300 });

/// What happens where two liquids meet. `liquid` and `touching` are block names or `#tags` of
/// liquid blocks. When `liquid` has `touching` beside or above it, it turns into `source` (if
/// it was a source) or `flowing`; when it flows down into `touching`, that block becomes
/// `falls_into`. Empty results leave things as they are.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReactionDesc {
    pub liquid: Str,
    pub touching: Str,
    pub source: Str,
    pub flowing: Str,
    pub falls_into: Str,
}
defaults!(ReactionDesc { liquid: Str::EMPTY, touching: Str::EMPTY, source: Str::EMPTY, flowing: Str::EMPTY, falls_into: Str::EMPTY });

// ---------------------------------------------------------------- client side

/// Hooks registered with `Host::reg_client_hook`. All run on the main thread.
pub mod client_hook {
    /// `ClientFrameArgs`, every frame before input is handled; may drive the client.
    pub const FRAME: u32 = 0;
    /// `ClientScreenArgs`: the top screen changed (empty name = no screen, back in the world).
    pub const SCREEN: u32 = 1;
}

#[repr(C)]
pub struct ClientFrameArgs {
    pub client: *mut ClientCtx,
}

/// One entity on the client, for `Host::reg_client_entity_tick`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClientEntityArgs {
    pub client: *mut ClientCtx,
    pub entity: u64,
    pub kind: u32,
    pub pos: [f64; 3],
    pub yaw: f32,
    /// Distance walked so far (it drives the limbs); steps go by how it grows.
    pub walked: f32,
    /// Seconds into its death animation (1 = over, its last call), negative while alive.
    pub dying: f32,
    /// Seconds since the client first saw it.
    pub age: f32,
    /// Seconds since the last call.
    pub dt: f32,
}

#[repr(C)]
pub struct ClientScreenArgs {
    pub client: *mut ClientCtx,
    pub screen: Str,
}

#[repr(C)]
pub struct PacketArgs {
    pub world: *mut WorldCtx,
    pub player: u64,
    pub data: Slice<u8>,
}

#[repr(C)]
pub struct ClientPacketArgs {
    pub client: *mut ClientCtx,
    pub data: Slice<u8>,
}

pub mod client_flags {
    /// A world is open (local or joined).
    pub const IN_WORLD: u32 = 1;
    /// The server accepted the player.
    pub const JOINED: u32 = 2;
    /// The world around the player is loaded and meshed.
    pub const LOADED: u32 = 4;
    /// Nothing left to generate, light, send or mesh (local server only).
    pub const IDLE: u32 = 8;
    /// The top screen is laid out and was drawn with its textures.
    pub const SCREEN_READY: u32 = 16;
    pub const DEAD: u32 = 32;
    pub const FLYING: u32 = 64;
    pub const ON_GROUND: u32 = 128;
    pub const MENU_OPEN: u32 = 256;
    /// Hosting and the join ticket is known.
    pub const HOSTING: u32 = 512;
    /// The F3 debug overlay is shown.
    pub const DEBUG: u32 = 1024;
    /// Background music is playing.
    pub const MUSIC: u32 = 2048;
    /// Single player stands still: an open screen pauses it.
    pub const PAUSED: u32 = 4096;
}

/// What a client callback can see of the local client, filled by `Host::client_state`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClientState {
    pub flags: u32,
    pub frame: u64,
    /// Seconds since the previous frame.
    pub dt: f32,
    /// Averages over the last half second.
    pub fps: f32,
    pub frame_ms: f32,
    /// GPU time of the last measured frame and the server's last tick, milliseconds.
    pub gpu_ms: f32,
    pub server_ms: f32,
    pub window: [u32; 2],
    pub gui_scale: f32,
    pub dim: u32,
    pub player: u64,
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    /// Radians, like `EntityState`.
    pub yaw: f32,
    pub pitch: f32,
    pub selected: u32,
    /// 0 survival, 1 creative, 2 spectator.
    pub mode: u32,
    pub health: f32,
    /// Loaded cubes, cubes waiting for a mesh, known entities.
    pub cubes: u32,
    pub pending_meshes: u32,
    pub entities: u32,
    /// Top screen (`core:menu`, `core:pause`, ...), empty in the world; valid during the call.
    pub screen: Str,
    /// Frames since the top screen changed.
    pub screen_frames: u32,
    /// Local server load totals: cubes generated and their average microseconds on a worker,
    /// cubes inserted and their average on the server thread.
    pub generated: u64,
    pub gen_us: f32,
    pub inserted: u64,
    pub insert_us: f32,
    /// Finished `client_op::BENCH` runs, and the numbers of the last one.
    pub benches: u32,
    pub bench_fps: f32,
    pub bench_avg_ms: f32,
    pub bench_p99_ms: f32,
    pub bench_gpu_ms: f32,
    pub bench_server_ms: f32,
    /// Particles alive on this client.
    pub particles: u32,
    /// Sounds started since the game started (0 without a sound device).
    pub sounds: u32,
    /// Where the player is drawn this frame (between physics ticks), unlike `pos`.
    pub shown: [f64; 3],
    /// The block looked at and the structure (entity) it is part of, 0 for the world;
    /// `target_frame` `u64::MAX` when nothing is looked at.
    pub target: [i32; 3],
    pub target_frame: u64,
    /// Cubes whose terrain the GPU made, and how many of them differed from the CPU's (counted
    /// with `core:terrain_check` on).
    pub gpu_cubes: u64,
    pub gpu_mismatches: u64,
    /// Distant terrain tiles made and shown, and asked for and not back yet.
    pub lod_tiles: u32,
    pub lod_pending: u32,
}
defaults!(ClientState {
    flags: 0,
    frame: 0,
    dt: 0.0,
    fps: 0.0,
    frame_ms: 0.0,
    gpu_ms: 0.0,
    server_ms: 0.0,
    window: [0; 2],
    gui_scale: 1.0,
    dim: 0,
    player: 0,
    pos: [0.0; 3],
    vel: [0.0; 3],
    yaw: 0.0,
    pitch: 0.0,
    selected: 0,
    mode: 0,
    health: 0.0,
    cubes: 0,
    pending_meshes: 0,
    entities: 0,
    screen: Str::EMPTY,
    screen_frames: 0,
    generated: 0,
    gen_us: 0.0,
    inserted: 0,
    insert_us: 0.0,
    benches: 0,
    bench_fps: 0.0,
    bench_avg_ms: 0.0,
    bench_p99_ms: 0.0,
    bench_gpu_ms: 0.0,
    bench_server_ms: 0.0,
    particles: 0,
    sounds: 0,
    shown: [0.0; 3],
    target: [0; 3],
    target_frame: u64::MAX,
    gpu_cubes: 0,
    gpu_mismatches: 0,
    lod_tiles: 0,
    lod_pending: 0,
});

/// `Host::client_input` operations. Keys and buttons are held for `x` seconds (0 = one tap).
pub mod client_input {
    /// `text`: an action (`core:jump`, `mymod:jetpack`), pressed with its bound key.
    pub const ACTION: u32 = 0;
    /// `text`: a key by its settings name (`Tab`, `Enter`, `KeyE`, `F3`).
    pub const KEY: u32 = 1;
    /// Mouse button `y` (0 left, 1 right, 2 middle).
    pub const CLICK: u32 = 2;
    /// Moves the mouse to (`x`, `y`) as fractions of the window.
    pub const CURSOR: u32 = 3;
    /// Mouse wheel by `x` steps (negative = down).
    pub const SCROLL: u32 = 4;
    /// `text` typed into the focused field.
    pub const TYPE: u32 = 5;
    pub const RELEASE_ALL: u32 = 6;
}

/// Fields of `PlayerControl` to apply.
pub mod control_mask {
    pub const POS: u32 = 1;
    pub const VEL: u32 = 2;
    pub const LOOK: u32 = 4;
    pub const FLYING: u32 = 8;
    pub const FLY_SPEED: u32 = 16;
    pub const SELECTED: u32 = 32;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlayerControl {
    pub mask: u32,
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub flying: u32,
    /// Spectator/creative flight speed multiplier.
    pub fly_speed: f32,
    pub selected: u32,
}
defaults!(PlayerControl { mask: 0, pos: [0.0; 3], vel: [0.0; 3], yaw: 0.0, pitch: 0.0, flying: 0, fly_speed: 1.0, selected: 0 });

/// `Host::client_command` operations.
pub mod client_op {
    /// A chat line or `/command` as if typed.
    pub const CHAT: u32 = 0;
    /// Captures the next frame into `text` (a path; empty = `screenshots/<time>.png`, a bare
    /// name goes into `screenshots/`). Returns the image id for `image_get`.
    pub const SCREENSHOT: u32 = 1;
    /// Leaves the game with exit code `num`.
    pub const QUIT: u32 = 2;
    /// Opens world `text` (created with seed `num` if missing).
    pub const OPEN_WORLD: u32 = 3;
    /// Like `OPEN_WORLD`, deleting a save of that name first.
    pub const NEW_WORLD: u32 = 4;
    pub const LEAVE_WORLD: u32 = 5;
    /// Starts hosting the open world; the ticket appears in `client_text::TICKET`.
    pub const HOST: u32 = 6;
    /// Joins the host with ticket `text`.
    pub const JOIN: u32 = 7;
    /// `core:pause`, `core:settings`, `core:language`, `core:controls`, `core:worlds`,
    /// `core:create_world`, `core:title`, `core:chat`, `core:inventory`, `core:mod_settings`.
    pub const OPEN_SCREEN: u32 = 8;
    /// Closes the top screen (and its menu).
    pub const CLOSE_SCREEN: u32 = 9;
    pub const RELOAD_ASSETS: u32 = 10;
    /// Frame, GPU and server statistics over `num` seconds, logged as a `BENCH` line labeled
    /// `text`; the numbers land in `ClientState::bench_*`.
    pub const BENCH: u32 = 11;
    pub const RESPAWN: u32 = 12;
    /// Post-processing effect `text` (`shaders/post/<name>.wgsl`) at strength `num`, 0 turns it
    /// off. Effects stay on until changed and run in the order they were turned on.
    pub const POST_EFFECT: u32 = 14;
    /// Runs command `text` (`/tp ~ ~5 ~`) as the player, here if it is a client command, else
    /// on the server; returns an id for `Host::client_command_reply`. The chat stays out of it.
    pub const RUN_COMMAND: u32 = 16;
}

/// `Host::client_text` sources.
pub mod client_text {
    /// Join ticket while hosting.
    pub const TICKET: u32 = 0;
    /// The game's `run` directory.
    pub const RUN_DIR: u32 = 1;
    /// Chat line `index` counted from the newest (0).
    pub const CHAT: u32 = 2;
    /// The last `BENCH` line.
    pub const BENCH: u32 = 3;
    /// Text of the top screen's focused input field (chat, search).
    pub const INPUT: u32 = 4;
}

/// Ore veins a mod adds to any dimension, its own or another mod's. The engine scatters them after
/// the dimension's generator, so generators never need to know about each other's ores.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OreDesc {
    /// Space separated full dimension names, empty: every dimension whose blocks it may
    /// replace (`dim_flags::BARE` ones only when named).
    pub dimensions: Str,
    pub block: Str,
    /// Space separated block names or `#tags` the ore may replace.
    pub replace: Str,
    pub min_y: i32,
    pub max_y: i32,
    /// Veins started per cube.
    pub veins: u32,
    /// Blocks per vein.
    pub blocks: u32,
    /// Only in biomes with this tag (the block's own column of its cube), empty: anywhere.
    pub biome: Str,
}
defaults!(OreDesc { dimensions: Str::EMPTY, block: Str::EMPTY, replace: Str::EMPTY, min_y: i32::MIN, max_y: i32::MAX, veins: 4, blocks: 8, biome: Str::EMPTY });

/// Surface height and biome of one column of a terrain.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerrainColumn {
    /// The top solid block of the terrain itself (water, caves and decorations aside).
    pub height: i32,
    pub biome: u32,
}

/// One step of a terrain mask, the steps of a whole mask in postfix order: `ALL` and `NOT` take
/// the masks just before them.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MaskStep {
    /// `terrain_mask`.
    pub kind: u32,
    /// `ALL`: how many masks it combines.
    pub count: u32,
    pub salt: u64,
    pub scale: f64,
    pub octaves: u32,
    pub from: f64,
    pub to: f64,
}
defaults!(MaskStep { kind: 0, count: 0, salt: 0, scale: 1.0, octaves: 3, from: 0.0, to: 1.0 });

pub mod terrain_mask {
    /// Its own noise (`salt`, `scale`, `octaves`), ramping from `from` to `to`.
    pub const NOISE: u32 = 0;
    pub const CONTINENT: u32 = 1;
    pub const TEMPERATURE: u32 = 2;
    pub const HUMIDITY: u32 = 3;
    /// The product of the `count` masks before it.
    pub const ALL: u32 = 4;
    /// 1 minus the mask before it.
    pub const NOT: u32 = 5;
}

/// The engine's terrain presets; `params` in the order of `loom_noise::shape`.
pub mod terrain_preset {
    /// `height` is a callback (`FeatureArgs`).
    pub const CUSTOM: u32 = 0;
    pub const CONTINENTS: u32 = 1;
    pub const HILLS: u32 = 2;
    pub const MOUNTAINS: u32 = 3;
    pub const PLATEAUS: u32 = 4;
    pub const CANYONS: u32 = 5;
    pub const DUNES: u32 = 6;
    pub const CRATERS: u32 = 7;
}

/// Adds height to every column of a terrain, scaled by its mask.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FeatureDesc {
    /// Full name, empty for none: biome rules and other mods' edits find it by this.
    pub name: Str,
    /// `terrain_preset`.
    pub preset: u32,
    pub params: [f64; 4],
    /// `FeatureArgs`, for `terrain_preset::CUSTOM`. Called from worker threads, pure.
    pub height: Callback,
    /// Empty: everywhere at full strength.
    pub mask: Slice<MaskStep>,
}
defaults!(FeatureDesc { name: Str::EMPTY, preset: 0, params: [0.0; 4], height: Callback::NONE, mask: Slice::EMPTY });

/// Positions and the height in 1/16 block: terrain is integers only (`loom_noise::fixed`).
#[repr(C)]
pub struct FeatureArgs {
    pub seed: u64,
    pub x: i32,
    pub z: i32,
    /// Output: the height added at (x, z) at full strength.
    pub height: i32,
}

/// Which fields of a `BiomeRuleDesc` are given (all of them count for a new rule; an edit
/// changes only these).
pub mod rule_field {
    pub const BIOME: u32 = 1;
    pub const TEMPERATURE: u32 = 2;
    pub const HUMIDITY: u32 = 4;
    pub const CONTINENT: u32 = 8;
    pub const HEIGHT: u32 = 16;
    pub const FEATURE: u32 = 32;
    pub const SURFACE: u32 = 64;
    pub const UNDER: u32 = 128;
    pub const DEEP_TOP: u32 = 256;
    pub const BARE_ABOVE: u32 = 512;
    pub const FROZEN: u32 = 1024;
}

/// Picks a biome and its surface where its conditions hold. Rules are tried in order; the first
/// that fits wins and the last one is used where none does. Fields not in `fields` come from
/// the biome's own (`BiomeDesc::rule`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BiomeRuleDesc {
    /// Full name; other mods' edits find the rule by it.
    pub name: Str,
    /// `rule_field`.
    pub fields: u32,
    pub biome: Str,
    /// `min <= v < max`.
    pub temperature: [f64; 2],
    pub humidity: [f64; 2],
    pub continent: [f64; 2],
    /// Surface height, inclusive.
    pub height: [i32; 2],
    /// Where the named feature's mask is in `min < v <= max`.
    pub feature: Str,
    pub feature_range: [f64; 2],
    /// Top block, `depth` blocks of filler, then stone. Empty: stone.
    pub top: Str,
    pub filler: Str,
    pub depth: i32,
    /// The lowest `under_layers` of the filler become `under`.
    pub under: Str,
    pub under_layers: i32,
    /// Top block of columns lower than `deep_below`.
    pub deep_top: Str,
    pub deep_below: i32,
    /// Plain stone above this height.
    pub bare_above: i32,
    /// The water surface freezes into this block.
    pub ice: Str,
}
defaults!(BiomeRuleDesc {
    name: Str::EMPTY,
    fields: 0,
    biome: Str::EMPTY,
    temperature: [f64::NEG_INFINITY, f64::INFINITY],
    humidity: [f64::NEG_INFINITY, f64::INFINITY],
    continent: [f64::NEG_INFINITY, f64::INFINITY],
    height: [i32::MIN, i32::MAX],
    feature: Str::EMPTY,
    feature_range: [0.0, 0.0],
    top: Str::EMPTY,
    filler: Str::EMPTY,
    depth: 3,
    under: Str::EMPTY,
    under_layers: 0,
    deep_top: Str::EMPTY,
    deep_below: i32::MIN,
    bare_above: i32::MAX,
    ice: Str::EMPTY
});

pub mod terrain_op {
    /// `block`: the stone everything is made of.
    pub const STONE: u32 = 0;
    /// `value`: the ground height before features.
    pub const BASE_HEIGHT: u32 = 1;
    /// `block` fills everything up to `level`.
    pub const SEA: u32 = 2;
    /// Tunnels and caverns; lava lakes (`block`) between `level` and `to`.
    pub const CAVES: u32 = 3;
    pub const FEATURE: u32 = 4;
    /// The feature named `anchor` goes away.
    pub const REMOVE_FEATURE: u32 = 5;
    /// The mask of `feature` is multiplied into the one of the feature named `anchor`.
    pub const MASK_FEATURE: u32 = 6;
    /// `rule` after all others.
    pub const BIOME: u32 = 7;
    pub const BIOME_BEFORE: u32 = 8;
    pub const BIOME_AFTER: u32 = 9;
    pub const REPLACE_BIOME: u32 = 10;
    pub const REMOVE_BIOME: u32 = 11;
    /// The given fields of `rule` replace those of the rule named `anchor`.
    pub const EDIT_BIOME: u32 = 12;
    /// `rule.biome` in whole cubes at least `level` blocks under the surface, where a 3D noise
    /// of patches `size` across is above `value`.
    pub const CAVE_BIOME: u32 = 13;
    /// The cave biome `anchor` goes away.
    pub const REMOVE_CAVE_BIOME: u32 = 14;
}

/// One part of a dimension's terrain (`Host::reg_terrain`). The mod that owns the dimension
/// describes it; others edit it, applied after the owner's in load order.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TerrainDesc {
    /// Full dimension name.
    pub dimension: Str,
    /// `terrain_op`.
    pub op: u32,
    /// The full name of the feature, rule or cave biome an op refers to.
    pub anchor: Str,
    pub block: Str,
    pub level: i32,
    pub to: i32,
    pub value: f64,
    pub size: f64,
    pub feature: FeatureDesc,
    pub rule: BiomeRuleDesc,
}
defaults!(TerrainDesc {
    dimension: Str::EMPTY,
    op: 0,
    anchor: Str::EMPTY,
    block: Str::EMPTY,
    level: 0,
    to: 0,
    value: 0.0,
    size: 0.0,
    feature: FeatureDesc::DEFAULT,
    rule: BiomeRuleDesc::DEFAULT
});

/// One command argument; kinds in `command_value`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CommandValue {
    pub kind: u32,
    pub int: i64,
    pub float: f64,
    /// Text, or the full registry name for ids.
    pub text: Str,
    pub pos: [f64; 3],
    /// The parameter's name in the usage string.
    pub name: Str,
}

pub mod command_value {
    /// Optional parameter left out.
    pub const NONE: u32 = 0;
    pub const INT: u32 = 1;
    pub const FLOAT: u32 = 2;
    pub const TEXT: u32 = 3;
    pub const POS: u32 = 4;
    /// `int` is the registry id (item, block state, entity kind, dimension), `text` its name.
    pub const ID: u32 = 5;
    /// Selected entities: `text` holds their ids separated by spaces, `int` how many.
    pub const TARGETS: u32 = 6;
}

#[repr(C)]
pub struct CommandArgs {
    pub world: *mut WorldCtx,
    /// Entity id of the player who ran it.
    pub player: u64,
    pub values: *const CommandValue,
    pub count: u32,
    /// Engine side reply buffer, written through `Host::command_reply`.
    pub reply: *mut core::ffi::c_void,
    /// Client commands (`Host::reg_client_command`): the client that typed it; `world` is null.
    pub client: *mut ClientCtx,
}

/// Places for HUD elements. `HOTBAR_LEFT`/`HOTBAR_RIGHT` stack upwards above the hotbar's halves.
pub mod hud_anchor {
    pub const TOP_LEFT: &str = "top_left";
    pub const TOP_RIGHT: &str = "top_right";
    pub const BOTTOM_LEFT: &str = "bottom_left";
    pub const BOTTOM_RIGHT: &str = "bottom_right";
    pub const HOTBAR: &str = "hotbar";
    pub const HOTBAR_LEFT: &str = "hotbar_left";
    pub const HOTBAR_RIGHT: &str = "hotbar_right";
    /// Centered above the hotbar and what stands on it (the held item's name, `core:held_name`).
    pub const ABOVE_HOTBAR: &str = "above_hotbar";
    /// A panel of the F3 overlay (left or right side), titled by the element's name as a
    /// translation key; the engine's five groups take F3+1..5, mods' the next numbers up to 9.
    pub const DEBUG_LEFT: &str = "debug_left";
    pub const DEBUG_RIGHT: &str = "debug_right";
}

/// Layout of the engine's `energy` entity component. Blocks tagged `conductor` connect the
/// block entities next to them into a network the engine balances every tick.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Energy {
    pub stored: i64,
    pub capacity: i64,
    /// Rate limits per tick; a network pulls from entities with `max_out > 0` and feeds entities
    /// with `max_in > 0`.
    pub max_in: i64,
    pub max_out: i64,
}
impl Schema for Energy {
    const SCHEMA: &'static str = "{stored:i64,capacity:i64,max_in:i64,max_out:i64}";
}
