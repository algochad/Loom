//! A starting point for your own mod. Copy this folder, rename it, and replace the
//! examples with your stuff.
//!
//! Rename checklist:
//! - `Cargo.toml`: `name` is your mod id (and the library's file name and the mod folder's),
//!   `version` the version players see.
//! - `declare_mod!`: `deps`, the mods yours needs.
//! - `assets/lang/*.lang`: the keys start with your mod id; `mod.<id>.name` is the name players
//!   see (without it, the id).
//!
//! Build: `cargo build --release`. Then make `run/mods/<your id>/` with the library from
//! `target/release/` and the `assets` folder next to it.
//! To share it, run `loom --pack-mod run/mods/<your id>`,
//! it packs that folder (library included) into a zip.
//!
//! This example adds one item, a setting, and a gift for joined players.
//! Everything else is commented examples. The full cheat sheet is at `docs/modding.md`.
//! `mods/base` and `mods/testmod` use exactly this API, so for more examples check those.

use loom_sdk::*;

declare_mod!(deps: ["base"], init: init);

// Everything your mod adds is named in a `static` handle and registered by it. A name without
// `mod:` is yours (`examplemod:example_item`), `Item::named("base:coal")` is another mod's.
// Once the game is loaded `EXAMPLE_ITEM.id()` is the numeric id hot code works with.
pub static EXAMPLE_ITEM: Item = Item::named("example_item");
pub static GIFT_ON_JOIN: Config<bool> = Config::named("gift_on_join");
// The base game's tab, by name.
static INGREDIENTS: CreativeTab = CreativeTab::named("base:ingredients");

/// Called once at startup, in parallel with the other mods: register everything here.
/// Nothing exists in the world yet, you only describe what your mod adds.
fn init(r: &mut Registry) {
    config(r);
    blocks(r);
    items(r);
    recipes(r);
    worldgen(r);
    entities(r);
    events(r);
    commands(r);
    client(r);
}

/// Settings show up in Settings > Mod Settings by themselves and are saved to
/// `config/examplemod.txt`. Name and tooltip come from `config.examplemod.<key>` in the lang file.
fn config(r: &mut Registry) {
    r.config(&GIFT_ON_JOIN).bool(true);
    // More examples:
    //      static POWER: Config<i32> = Config::named("power");
    //      static STYLE: Config<u32> = Config::named("style");
    //      r.config(&POWER).int(10, 0, 100);
    //      r.config(&STYLE).choice(0, &["warm", "cold"]);
    // Where the value lives: the player's own by default, `.server()` the server's (its players
    // get it and every change, for anything gameplay depends on), `.world()` saved with the
    // world and changed with `/rule` (game rules):
    //      static HARD_MODE: Config<bool> = Config::named("hard_mode");
    //      r.config(&HARD_MODE).bool(false).world();
    // The engine's own settings are the same: `settings::FOV.get()`, `settings::LANGUAGE.set("ru_ru")`.
}

/// Your blocks, texture is `block/<name>.png`, translation is `block.examplemod.<name>`.
/// More at `mods/base/src/blocks.rs` (support rules, plants, falling sand, torches, axis, etc.)
/// And `mods/testmod/src/lib.rs` (machines, facing, replacing another mod's block etc.)
fn blocks(_r: &mut Registry) {
    // static RUBY_ORE: Block = Block::named("ruby_ore");
    // static NATURAL: CreativeTab = CreativeTab::named("base:natural");
    // r.block(&RUBY_ORE)
    //     .hardness(3.0)
    //     .mineable("pickaxe")
    //     .needs_tool(tool_tier::IRON)      // drops nothing without at least an iron pickaxe
    //     .drops("ruby", 1)
    //     .tab(&NATURAL);
    //
    // Light and glass-like looks plus a custom box:
    // static LAMP: Block = Block::named("lamp");
    // static PANE: Block = Block::named("pane");
    // r.block(&LAMP).light_rgb(15, 10, 4).hardness(0.3);
    // r.block(&PANE).model_box([0.0, 0.0, 7.0], [16.0, 16.0, 9.0]).cutout();   // faces show block/pane
    // A shape made in Blockbench needs no code: `assets/models/block/<name>.json` (Java block model).
    //
    // Behavior (all optional, all get a `World`):
    // static BUTTON: Block = Block::named("button");
    // r.block(&BUTTON).on_use(|u| { u.world(); u.pos(); u.player(); true });
    //
    // A family of blocks made in a loop gets its handles from the names:
    // for color in ["red", "blue"] {
    //     r.block(Block::generated(format!("{color}_brick"))).hardness(2.0);
    // }
}

/// Your items, texture is `item/<name>.png`, translation is `item.examplemod.<name>`
/// Gray tooltip with `item.examplemod.<name>.tooltip`, for more tooltip options check `mods/base/assets/lang/en_us.lang`
fn items(r: &mut Registry) {
    r.item(&EXAMPLE_ITEM).tab(&INGREDIENTS).tag("gems/example");
    // Tools, weapons, food, fuel are all components or tags, so other mods can use them:
    // static RUBY_PICKAXE: Item = Item::named("ruby_pickaxe");
    // r.item(&RUBY_PICKAXE).tool("pickaxe").tool_level(tool_tier::DIAMOND).mining_speed(8.0).durability(1200).attack_damage(4.0);
    // r.item(&RUBY_APPLE).set(&FOOD, ...)     // see `mods/base/src/items.rs` for food and armor
    // static FUEL: Component<u32> = Component::named("fuel");   // the common burn time, in ticks
    // r.set("examplemod:example_item", &FUEL, 1600);
}

/// Recipes reference items by name or `#tag`, so a recipe made with `#ingots/iron` tag takes any mod's
/// item with the same tag. `base:crafting` and `base:smelting` belong to the base game.
fn recipes(_r: &mut Registry) {
    // static CRAFTING: RecipeKind = RecipeKind::named("base:crafting");
    // static SMELTING: RecipeKind = RecipeKind::named("base:smelting");
    // r.shaped(&CRAFTING, "ruby_pickaxe", 1, &["rrr", " s ", " s "], &[('r', "#gems/ruby"), ('s', "#stick")]);
    // r.shapeless(&CRAFTING, "ruby", 9, &["ruby_block"], 0);
    // r.shapeless(&SMELTING, "ruby", 1, &["ruby_ore"], 200);    // 200 ticks in a furnace
}

/// Ores, biomes, structures and dimensions. Ores go into every dimension whose stone they replace.
/// Mars as a whole planet from terrain presets is at `mods/testmod/src/lib.rs`
/// More at `mods/base/src/worldgen.rs`
fn worldgen(_r: &mut Registry) {
    // static OVERWORLD: Dimension = Dimension::named("base:overworld");
    // static RUBY_ORE: Block = Block::named("ruby_ore");
    // r.ore(&RUBY_ORE).y(-64, 16).veins(2).size(5).replace("#stone_ore_replaceables");
    // static SHRINE: Template = Template::named("shrine");
    // r.template(&SHRINE).layer(&["ccc", "c c", "ccc"]).key('c', "base:cobblestone");
    // r.structure(&OVERWORLD, &SHRINE).spacing(256).chance(0.3);
}

/// Mobs and other entities
/// Models in code (`biped`, `quadruped`) or Blockbench files in `assets/models`
/// More at `mods/base/src/mobs.rs` (AI goals, spawn rules, etc.).
fn entities(_r: &mut Registry) {
    // static GHOST: Entity = Entity::named("ghost");
    // r.entity(&GHOST).size(0.6, 1.8).health(10.0).speed(3.0).hostile().model(biped(false)).sound("hurt");
    // r.spawn(&GHOST).light(0, 4).weight(1);
}

/// Reacting to the game, events arrive batched once per tick
/// for "may this happen" use `r.intercept`
/// to change a number use `r.modify`
/// For more info check "Hooking into the game" in `docs/modding.md`
fn events(r: &mut Registry) {
    // Players joining get an example item if the setting allows it.
    r.on(&PLAYER_JOINED, |batch| {
        if !GIFT_ON_JOIN.get() {
            return;
        }
        let w = batch.world();
        for joined in batch.iter() {
            w.inventory(joined.player).insert(ItemStack::new(EXAMPLE_ITEM.id(), 1));
            info!("gave player {} an example item", joined.player);
        }
    });
    // r.on(&BLOCK_BROKEN, |b| for e in b.iter() { /* e.player, e.pos, e.state */ });
    // r.intercept(&question::BREAK_BLOCK, |q| if q.pos()[1] < -60 { Verdict::Deny } else { Verdict::Abstain });
    // r.modify(&chain::DAMAGE, stage::MULTIPLY, |m| m.value *= 0.9);
}

/// Chat commands
/// The usage string is parsed, checked, completed with Tab and shown in `/help` by the engine
/// you only read the values by name.
fn commands(_r: &mut Registry) {
    // r.command("example", "[times:int]", |cmd| {
    //     let n = cmd.int("times").unwrap_or(1);
    //     cmd.reply("examplemod.hello", &[Arg::Int(n)]);    // `examplemod.hello = hello {0} times` in lang
    // });
}

/// Client side: screens, HUD, key bindings, packets.
/// More at:
/// `mods/base/src/player.rs` (HUD),
/// `mods/testmod/src/lib.rs` (screens, machine panels),
/// `mods/testmod/src/looks.rs` (particles, shaders, sounds).
fn client(_r: &mut Registry) {
    // static DASH: Key = Key::named("dash");
    // static ABOUT: Screen = Screen::named("about");
    // static MANA: HudElement = HudElement::named("mana");
    // r.key(&DASH, "R").on_press_server(|p| { let w = p.world(); info!("{} dashes", p.player()); w.time(); });
    // r.screen(&ABOUT, |ui| ui.panel("background", S::col().pad(8.0), |ui| ui.text("examplemod.about")));
    // r.hud_element(&MANA, hud_anchor::HOTBAR_RIGHT, 2, |ui| ui.text_bind(|c, out| { /* ... */ }));
    //
    // Sparks around a mob while it lives, the engine's steps and idle noises kept:
    // static WISP: Entity = Entity::named("wisp");
    // static SPARK: Particle = Particle::named("spark");
    // r.client_entity_tick(&WISP, |c, e| {
    //     if e.dying().is_none() {
    //         c.spawn_particles(&SPARK, e.pos());
    //     }
    //     e.engine_effects();
    // });
}
