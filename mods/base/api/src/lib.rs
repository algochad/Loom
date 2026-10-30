//! What other mods use of the base game, as types: its events with their values, components,
//! value chains, recipe kinds, equipment slots, creative tabs and dimensions. A mod adds
//! `base_api` to its `Cargo.toml` and gets these checked when it compiles, without depending on
//! the game's code; the engine still compares the value layouts when the mods load.
//!
//! Names here are full (`base:..`): the statics compile into whichever mod uses them.

use loom_sdk::*;

value! {
    /// Fired when a furnace finishes one item.
    pub struct Smelted { pub furnace: u64, pub input: u32, pub output: ItemStack }
}
pub static SMELTED: Event<Smelted> = Event::named("base:smelted");

value! {
    /// A furnace's state, synced to whoever has it open.
    pub struct Smelting { pub burn: u32, pub burn_max: u32, pub cook: u32, pub cook_max: u32 }
}
pub static SMELTING_STATE: Component<Smelting> = Component::named("base:smelting");

value! {
    /// A player's hunger and air.
    pub struct Hunger { pub food: f32, pub saturation: f32, pub exhaustion: f32, pub air: f32 }
}
pub static HUNGER: Component<Hunger> = Component::named("base:hunger");

value! {
    /// Read by the hunger system; other mods set it on their own food.
    pub struct Food { pub hunger: u32, pub saturation: f32 }
}
value! {
    /// Protection while worn: 4% less damage per point, like 1.7.10.
    pub struct Armor { pub points: u32 }
}

// The common vocabulary the base game reads; any mod may set these on its own content.
pub static FOOD: Component<Food> = Component::named("food");
/// Burn time in ticks.
pub static FUEL: Component<u32> = Component::named("fuel");
pub static ARMOR: Component<Armor> = Component::named("armor");
/// How readily fire takes it.
pub static FLAMMABLE: Component<u32> = Component::named("flammable");

/// A furnace's cooking time: base is the recipe's time, `entity` the furnace, `item` the input.
pub static SMELT_TIME: Chain = Chain::named("base:smelt_time");

pub static CRAFTING: RecipeKind = RecipeKind::named("base:crafting");
pub static SMELTING: RecipeKind = RecipeKind::named("base:smelting");

/// The player's equipment slots, head to feet.
pub static HEAD: EquipmentSlot = EquipmentSlot::named("base:head");
pub static CHEST: EquipmentSlot = EquipmentSlot::named("base:chest");
pub static LEGS: EquipmentSlot = EquipmentSlot::named("base:legs");
pub static FEET: EquipmentSlot = EquipmentSlot::named("base:feet");

pub static OVERWORLD: Dimension = Dimension::named("base:overworld");
pub static FLATLANDS: Dimension = Dimension::named("base:flatlands");

/// Vanilla's creative tabs.
pub mod tabs {
    use loom_sdk::CreativeTab;
    pub static BUILDING: CreativeTab = CreativeTab::named("base:building");
    pub static COLORED: CreativeTab = CreativeTab::named("base:colored");
    pub static NATURAL: CreativeTab = CreativeTab::named("base:natural");
    pub static FUNCTIONAL: CreativeTab = CreativeTab::named("base:functional");
    pub static TOOLS: CreativeTab = CreativeTab::named("base:tools");
    pub static COMBAT: CreativeTab = CreativeTab::named("base:combat");
    pub static FOOD: CreativeTab = CreativeTab::named("base:food");
    pub static INGREDIENTS: CreativeTab = CreativeTab::named("base:ingredients");
}
