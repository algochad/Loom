//! Proves the engine is moddable without touching the core. Everything here uses only
//! `loom_sdk` and the base game's API crate `base_api` (types, no game code):
//! - a generator, a cable and an electric furnace talking through the engine's energy contract,
//! - a block with its own screen (energy bar bound to synced menu data),
//! - reads the base game's data (fuel values, smelting recipes) through `base_api`, checked when
//!   it compiles and again by the engine when the mods load,
//! - replaces a base game block (torches burn blue),
//! - adds its own dimensions, one of them a planet built only from terrain presets,
//! - adds an ore to the base game's overworld,
//! - adds a liquid (oil) whose bucket is the base game's bucket filled, without bucket code,
//! - makes cooked meat heal: its own component on the base game's items plus the base game's
//!   `eaten` event, and lets zombies drop tin through a loot modifier,
//! - shows off particles, shaders, text effects and glow masks (`looks`).

mod bodies;
mod looks;

use loom_sdk::terrain::*;
use loom_sdk::*;

declare_mod!(deps: ["base"], init: init);

static GENERATOR: Block = Block::named("generator");
static HUM: Sound = Sound::named("machine.hum");
static CABLE: Block = Block::named("cable");
/// Its shape is `models/block/pedestal.json`, a Blockbench Java block model.
static PEDESTAL: Block = Block::named("pedestal");
static DYNAMITE: Block = Block::named("dynamite");
static BOOM: Sound = Sound::named("boom");
/// How far testmod's dynamite blows (a server setting).
static DYNAMITE_RADIUS: Config<i32> = Config::named("dynamite_radius");
static ELECTRIC_FURNACE: Block = Block::named("electric_furnace");
static BATTERY: Block = Block::named("battery");
static RED_SAND: Block = Block::named("red_sand");
static VENT: Block = Block::named("vent");
static REINFORCED_STONE: Block = Block::named("reinforced_stone");
static STEEL_PICKAXE: Item = Item::named("steel_pickaxe");
static OIL: Block = Block::named("oil");
static OIL_BUCKET: Item = Item::named("oil_bucket");
static MACHINES: SoundCategory = SoundCategory::named("machines");
static ENERGY_GIZMOS: GizmoGroup = GizmoGroup::named("energy");
// The base game's things, by name: no code dependency on it.
pub use base_api::OVERWORLD;
static BUCKET: Item = Item::named("base:bucket");
static ZOMBIE: Entity = Entity::named("base:zombie");
use base_api::tabs::{BUILDING as BASE_BUILDING, TOOLS as BASE_TOOLS};
static SOUL_TORCH: Block = Block::named("base:torch");
static GENERATOR_MENU: Menu = Menu::named("generator");
static FURNACE_MENU: Menu = Menu::named("electric_furnace");
static BATTERY_MENU: Menu = Menu::named("battery");
use base_api::SMELTING as SMELTING_RECIPES;
static VOID: Dimension = Dimension::named("void");
static MARS: Dimension = Dimension::named("mars");
static DUST: Biome = Biome::named("dust");
static SCORCHED: Biome = Biome::named("scorched");
static REGOLITH: Block = Block::named("regolith");
static BASALT: Block = Block::named("basalt");
static TIN_ORE: Block = Block::named("tin_ore");
static TIN_INGOT: Item = Item::named("tin_ingot");
static ANOTHER_STICK: Item = Item::named("another_stick");

value! {
    /// A generator's fire.
    #[derive(Default)]
    struct Burner { burn: u32, burn_max: u32 }
}
value! {
    /// An electric furnace's work on the item in it.
    #[derive(Default)]
    struct Work { progress: u32, progress_max: u32 }
}

static BURNER: Component<Burner> = Component::named("testmod:burner");
static WORK: Component<Work> = Component::named("testmod:work");
/// The common fuel value; the base game sets it, this mod only reads it.
use base_api::FUEL;
static FOOD_HEAL: Component<f32> = Component::named("testmod:food_heal");


const PER_TICK: i64 = 40;
const FURNACE_USE: i64 = 20;

fn init(r: &mut Registry) {
    looks::register(r);
    bodies::register(r);
    debug_panel(r);
    about_screen(r);
    morph(r);
    dynamite(r);
    r.block(&CABLE).model_box([5.0, 5.0, 5.0], [11.0, 11.0, 11.0]).tag("conductor").tag("cables").hardness(0.5);
    // Faces the file leaves to the block (`#top`, `#side`) show these.
    r.block(&PEDESTAL).textures("machine_top", "generator", "machine_top").hardness(1.0).tab(&BASE_BUILDING);

    // Same name as the base game's torch: the base game's handle, item and recipes give this.
    // It keeps the base torch's states (floor and walls) and placement, only its texture and
    // light are its own.
    r.block(&SOUL_TORCH)
        .replacing()
        .retexture("base:torch", "soul_torch")
        .light_rgb(3, 9, 35)
        .hardness(0.0);

    // Machines: block, entity with the machine and energy components, menu, all in one builder.
    // The components sync to open menus; screens bind to them.
    // Each kind ticks as a system over its own state: every machine of a region in one call.
    r.entity_component(&BURNER, Burner::default(), component_flags::SAVE | component_flags::SYNC_MENU);
    r.entity_component(&WORK, Work::default(), component_flags::SAVE | component_flags::SYNC_MENU);
    r.system(&BURNER, tick_generators);
    r.system(&WORK, tick_furnaces);
    // Machines' sounds have their own category (and volume setting `testmod:volume_machines`).
    r.sound_category(&MACHINES, true);
    r.sound(&HUM).files(&["base:dig/stone1"]).category(&MACHINES).pitch(0.45, 0.5);
    r.machine(&GENERATOR, &GENERATOR_MENU)
        .block(|b| b.textures("machine_top", "generator", "machine_top").hardness(3.0).mineable("pickaxe").needs_tool(0))
        // Only fuel goes in (the base game's `fuel` component), and stays in until burnt.
        .group("fuel", Slots::single().accepts("fuel").input().tooltip("testmod.fuel_slot"))
        .attach(&BURNER)
        .attach_with(&ENERGY, Energy { stored: 0, capacity: 20_000, max_in: 0, max_out: 200 })
        .screen(|ui| {
            machine_screen(ui, |ui| {
                ui.column(S::col().gap(2.0).center(), |ui| {
                    ui.group("fuel");
                    ui.progress_bind_dir("flame", S::default().size(14.0, 14.0), abi::ui_dir::UP, |c| c.menu_component(&BURNER).map_or(0.0, |m| ratio(m.burn, m.burn_max)));
                });
            })
        });
    r.machine(&ELECTRIC_FURNACE, &FURNACE_MENU)
        .block(|b| b.textures("machine_top", "electric_furnace", "machine_top").hardness(3.0).mineable("pickaxe").needs_tool(0))
        // At most 16 to smelt at a time.
        .group("input", Slots::single().limit(16))
        .group("output", Slots::single().output())
        .recipes(&SMELTING_RECIPES)
        .attach(&WORK)
        .attach_with(&ENERGY, Energy { stored: 0, capacity: 40_000, max_in: 400, max_out: 0 })
        .screen(|ui| {
            machine_screen(ui, |ui| {
                ui.group("input");
                ui.recipe_arrow_bind(|c| c.menu_component(&WORK).map_or(0.0, |m| ratio(m.progress, m.progress_max)));
                ui.group("output");
            })
        });

    // No tick of its own: only the grid fills and drains it, so its bar shows that components
    // reach open menus whoever changed them.
    r.machine(&BATTERY, &BATTERY_MENU)
        .block(|b| b.textures("machine_top", "battery", "machine_top").hardness(3.0).mineable("pickaxe").needs_tool(0))
        .attach_with(&ENERGY, Energy { stored: 0, capacity: 100_000, max_in: 400, max_out: 400 })
        .screen(|ui| machine_screen(ui, |_| {}));

    // The base game's kinds, by name: no code dependency on it.
    use base_api::CRAFTING;
    recipe_showcase(r);
    r.shaped(&CRAFTING, "generator", 1, &["iii", "ifi", "iii"], &[('i', "#ingots/iron"), ('f', "#furnaces")]);
    r.shaped(&CRAFTING, "electric_furnace", 1, &["iii", "iFi", "ici"], &[('i', "#ingots/iron"), ('F', "#furnaces"), ('c', "#cables")]);
    r.shaped(&CRAFTING, "cable", 8, &["iii"], &[('i', "#ingots/iron")]);

    r.block(&REGOLITH).hardness(0.5).mineable("shovel");
    // Only the common `sand` tag: cactus and sugar cane accept it without knowing it.
    r.block(&RED_SAND).hardness(0.5).mineable("shovel").tag("sand");
    r.block(&BASALT).hardness(1.5).mineable("pickaxe").tag("stone_ore_replaceables");
    // Only tags: the base game's `#ores/*` smelting recipe and tools pick these up.
    r.block(&TIN_ORE).hardness(3.0).mineable("pickaxe").needs_tool(0).tag("ores/tin");
    r.item(&TIN_INGOT).tag("ingots/tin");


    r.item(&ANOTHER_STICK).tag("stick");


    // A tier between vanilla's iron (200) and diamond (300): mines everything iron does plus
    // reinforced stone, which iron cannot.
    r.item(&STEEL_PICKAXE).tool("pickaxe").tool_level(250).mining_speed(7.0).durability(800).attack_damage(4.0).set(&WEAR_PER_HIT, 2).tab(&BASE_TOOLS);
    // May face up, north or east only: three states, not six.
    r.block(&VENT)
        .facing(Facings::only(&[Dir::Up, Dir::North, Dir::East]), Toward::Player)
        .states(|p, s| {
            let mut f = ["vent_side"; 6];
            f[p.facing().face() as usize] = "vent_front";
            s.faces(f)
        })
        .hardness(2.0)
        .mineable("pickaxe")
        .tab(&BASE_BUILDING);
    r.block(&REINFORCED_STONE).shader("stripes").hardness(4.0).mineable("pickaxe").needs_tool(250).tab(&BASE_BUILDING);
    r.biome(&DUST).surface("regolith", "regolith", 3).tint(&tints::GRASS, 0xb5764a).tint(&tints::FOLIAGE, 0x9a6040).tint(&tints::WATER, 0x8a5a40);
    // The hottest, driest part of base's deserts.
    r.biome(&SCORCHED).temperature(0.4..).humidity(..-0.1).surface("regolith", "regolith", 2).tint(&tints::GRASS, 0xbf8a50).tint(&tints::FOLIAGE, 0xae7a3a).tag("dry").tag("hot");
    // Base's overworld, edited from outside: a biome in the hottest, driest deserts, a rock needle
    // from a feature of this mod's own, and boulders on the new biome.
    r.terrain(&OVERWORLD)
        .biome_before("base:desert", BiomeRule::new("scorched"))
        .feature(Feature::new(|_, x, z| if (x - NEEDLE[0] * 16).abs().max((z - NEEDLE[1] * 16).abs()) < 3 * 16 { 90 * 16 } else { 0 }).named("needle"));
    r.decorate(boulders).dimension(&OVERWORLD);
    // One ore for every dimension: base's stone and Mars' basalt both take it.
    r.ore(&TIN_ORE).y(-64, 90).veins(5).size(8).replace("#stone_ore_replaceables");
    r.terrain(&MARS)
        .stone("basalt")
        .base_height(60.0)
        .feature(preset::hills(8.0, 220.0))
        .feature(preset::plateaus(24.0, 320.0, 6.0).mask(Mask::noise(900.0, 0.1, 0.4)))
        .feature(preset::canyons(26.0, 9.0).mask(Mask::noise(1200.0, -0.2, 0.1)))
        .feature(preset::craters(32.0, 16.0, 150.0))
        .feature(preset::dunes(3.0, 22.0).mask(Mask::noise(500.0, 0.15, 0.45)))
        // Base's forest in the cold parts of Mars (any humidity, unlike at home): it brings its
        // grass, and the engine grows its trees here like in base's overworld.
        .biome(BiomeRule::new("base:forest").temperature(..-0.3).humidity(..))
        .biome(BiomeRule::new("dust"));
    r.dimension(&MARS)
        .spawn(|seed| [8, MARS.column(seed, 8, 8).height + 1, 8])
        .height(-4, 12)
        .sky_above(11)
        .gravity(12.0)
        .fixed_time(5000)
        .sky(0xd8a080, 0xe0b090);

    // A liquid of its own, flowing by the engine's rules; its bucket fills from the base game's.
    r.liquid(&OIL).opacity(4).drag(0.4).flow_delay(15).drop_off(2).slope_find(3).tag("oil").bucket(&OIL_BUCKET, &BUCKET, &BASE_TOOLS);

    // Healing food without touching the base game's eating code: any used item that heals.
    r.set("#cooked_meat/*", &FOOD_HEAL, 2.0);
    r.on(&ITEM_USED, |b| {
        let w = b.world();
        for e in b.iter() {
            if let (Some(heal), Some(mut st)) = (FOOD_HEAL.on_stack(&e.stack), w.entity(e.player)) {
                st.health = (st.health + heal).min(st.max_health);
                w.set_entity(e.player, &st);
            }
        }
    });
    r.loot_modify(&ZOMBIE).add_pool(Pool::new().when(Cond::Chance(0.05)).item("tin_ingot"));

    r.command("tagbench", "[millions:int]", tag_bench);
    // The engine's /gizmo suggests this mod's group too.
    r.gizmo_group(&ENERGY_GIZMOS);

    r.dimension(&VOID)
        .generator(island)
        .spawn(|_| [0, 64, 0])
        .height(2, 6)
        .sky_above(5)
        .gravity(14.0)
        .fixed_time(18000)
        .ambient(5)
        .sky(0x2a1040, 0x40206a);
}

/// `/morph base:pig 0.9 0.9`: the player looks like another kind and gets a box of that size
/// (eyes at 85%), still the same player; `/morph` alone turns back.
fn morph(r: &mut Registry) {
    r.command("morph", "[kind:entity] [width:float] [height:float]", |cmd| {
        let (w, me) = (cmd.world(), cmd.player());
        match cmd.text("kind").map(str::to_string) {
            Some(kind) => {
                let (width, height) = (cmd.float("width").unwrap_or(0.9) as f32, cmd.float("height").unwrap_or(0.9) as f32);
                w.set_look(me, Some(&kind));
                w.set_size(me, Some([width, height, height * 0.85]));
                cmd.reply("command.testmod.morph", &[Arg::Text(&kind), Arg::Float(width as f64, 2), Arg::Float(height as f64, 2)]);
            }
            None => {
                w.set_look(me, None);
                w.set_size(me, None);
                cmd.reply("command.testmod.unmorph", &[]);
            }
        }
    });
}

static ABOUT: Screen = Screen::named("about");
static DEBUG_PANEL: HudElement = HudElement::named("debug");

/// A screen of the mod's own (`/about` here, `--screen testmod:about`).
fn about_screen(r: &mut Registry) {
    // Everything the engine's own screens use: a large title, a text field, a slider, an item,
    // a tooltip with arguments, and pausing like the engine's pause menu.
    static NAME: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    static VOLUME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    r.screen(&ABOUT, |ui| {
        ui.panel("testmod_panel", S::col().pad(8.0).gap(4.0).min_w(220.0), |ui| {
            ui.text_styled(S::col().class("title").scale(2.0).no_wrap(), "testmod.about.title", &[]);
            ui.text("testmod.about.text");
            ui.panel("well", S::col().pad(4.0).tooltip_args("testmod.about.tip", &[Arg::Int(3)]), |ui| ui.text("testmod.about.hover"));
            ui.input("", 16, S::col().class("about_name").w(200.0), |_, text| *NAME.lock().unwrap() = text.to_string());
            ui.text_bind(|_, out| out.set("testmod.about.name", &[Arg::Text(&NAME.lock().unwrap())]));
            ui.slider(S::col().w(200.0).h(20.0), |_| VOLUME.load(std::sync::atomic::Ordering::Relaxed) as f32 / 100.0, |_, v| VOLUME.store((v * 100.0).round() as u32, std::sync::atomic::Ordering::Relaxed));
            ui.text_bind(|_, out| out.set("testmod.about.fov", &[Arg::Float(loom_sdk::settings::FOV.get() as f64, 0)]));
            ui.text_bind_styled(S::col().mono(), |_, out| out.set("testmod.about.volume", &[Arg::Int(VOLUME.load(std::sync::atomic::Ordering::Relaxed) as i64)]));
            ui.row(S::row().gap(4.0), |ui| {
                ui.item(Item::lookup("base:coal").unwrap_or(0), 3, S::default());
                ui.button("testmod.about.closed", S::col().disabled(true), |_| {});
            });
        });
    })
    .pauses();
    // A button of its own in the main menu, next to the engine's and other mods'.
    r.screen_part(&screens::TITLE, "buttons", 0, |ui| {
        ui.button("testmod.about.button", S::default(), |c| {
            c.client().open_screen(&ABOUT);
        });
    });
    r.client_command("about", "", |c| {
        c.client().open_screen(&ABOUT);
    });
}

/// An F3 panel of the mod's own (F3+6 toggles it).
fn debug_panel(r: &mut Registry) {
    r.hud_element(&DEBUG_PANEL, hud_anchor::DEBUG_LEFT, 0, |ui| {
        ui.text_bind(|c, out| {
            let Some(me) = c.me() else { return };
            let speed = (me.vel[0] * me.vel[0] + me.vel[2] * me.vel[2]).sqrt() * 20.0;
            out.set("testmod.debug.speed", &[Arg::Float(speed, 2), Arg::Float(me.fall as f64, 1)]);
        });
    });
}

fn machine_screen(ui: &mut Ui, slots: impl FnOnce(&mut Ui)) {
    // Drawn by the mod's GUI shader (`shader` in `gui/testmod_panel.gui`).
    ui.panel("testmod_panel", S::col().pad(7.0).gap(4.0), |ui| {
        ui.title();
        ui.row(S::row().gap(8.0), |ui| {
            ui.column(S::col().gap(2.0), |ui| {
                ui.text_bind(|c, out| {
                    let e = c.menu_component(&ENERGY).unwrap_or_default();
                    out.set("testmod.energy", &[Arg::Int(e.stored), Arg::Int(e.capacity)]);
                });
                ui.progress_bind("energy", S::default().size(80.0, 8.0), |c| c.menu_component(&ENERGY).map_or(0.0, |e| if e.capacity > 0 { e.stored as f32 / e.capacity as f32 } else { 0.0 }));
            });
            ui.spacer();
            ui.row(S::row().gap(6.0), slots);
        });
        ui.player_inventory(true);
    });
}

fn ratio(a: u32, b: u32) -> f32 {
    if b == 0 { 0.0 } else { a as f32 / b as f32 }
}

/// `/gizmo testmod:energy`: stored energy above every machine, the mod-side use of gizmos.
fn energy_gizmos(w: &World, ids: &[u64]) {
    if w.time() % 10 != 0 || !w.gizmo_on(&ENERGY_GIZMOS) {
        return;
    }
    for &id in ids {
        if let (Some(st), Some(e)) = (w.entity(id), w.energy(id)) {
            let args = [Arg::Int(e.stored as i64), Arg::Int(e.capacity as i64)];
            w.draw_text(&ENERGY_GIZMOS, [st.pos[0], st.pos[1] + 1.3, st.pos[2]], "testmod.energy_label", &args, 0x60FF60FF, 1.0, 11);
        }
    }
}

/// Dynamite: lit by a right click, five seconds of fuse throwing sparks, then a crater of
/// `dynamite_radius` blocks through the engine's area edit: one call however big, the part not
/// loaded gets it when it loads.
fn dynamite(r: &mut Registry) {
    r.config(&DYNAMITE_RADIUS).int(500, 4, 2000).server();
    r.sound(&BOOM).files(&["base:dig/stone1"]).pitch(0.2, 0.3);
    r.block(&DYNAMITE)
        .prop("fuse", 0..=10)
        .states(|p, s| {
            let lit = !p.is("fuse", "0");
            let side = "dynamite_side";
            let s = s.faces([side, side, "dynamite_top", if lit { "dynamite_top_lit" } else { "dynamite_top" }, side, side]);
            if lit { s.light_rgb(12, 7, 2) } else { s }
        })
        .hardness(0.0)
        .tab(&BASE_TOOLS)
        .on_use(|u| {
            let (w, pos) = (u.world(), u.pos());
            if w.prop(pos, "fuse") == Some("0") {
                w.set_prop(pos, "fuse", "10");
                w.schedule(pos, 10);
            }
            true
        })
        .on_scheduled(|e| {
            let (w, pos) = (e.world(), e.pos());
            let Some(left) = w.prop(pos, "fuse").and_then(|v| v.parse::<u32>().ok()).filter(|&f| f > 0) else { return };
            let center = [pos[0] as f64 + 0.5, pos[1] as f64 + 0.5, pos[2] as f64 + 0.5];
            if left > 1 {
                w.set_prop(pos, "fuse", &(left - 1).to_string());
                w.spawn_particles(&looks::SPARK, [center[0], center[1] + 0.6, center[2]]).count(6).spread(0.2).speed(0.5);
                w.schedule(pos, 10);
            } else {
                w.play_sound(&BOOM, center, 4.0);
                w.fill_sphere(center, DYNAMITE_RADIUS.get() as f64, 0, true);
            }
        });
}

fn tick_generators(b: &mut SystemBatch<Burner>) {
    let w = b.world();
    energy_gizmos(&w, b.ids());
    for (id, m, ticks) in b.iter_mut() {
        let Some(e) = w.energy(id) else { continue };
        let room = e.capacity - e.stored;
        if ticks > 1 {
            // Catching up: it runs while there is room for what it makes, lighting fuel as it
            // goes (the network takes from it between parts of the catch-up).
            let mut want = (ticks as i64).min(room / PER_TICK).max(0) as u32;
            let mut ran = want.min(m.burn);
            m.burn -= ran;
            want -= ran;
            let slot = w.inventory(id).group("fuel");
            while want > 0 {
                let fuel = slot.get(0);
                let Some(t) = FUEL.on_item(fuel.item).filter(|_| !fuel.is_empty()) else { break };
                (m.burn, m.burn_max) = (t, t);
                slot.set(0, use_one(fuel));
                let r = want.min(m.burn);
                m.burn -= r;
                ran += r;
                want -= r;
            }
            if let Some(e) = w.energy_mut(id) {
                e.stored = (e.stored + ran as i64 * PER_TICK).min(e.capacity);
            }
            continue;
        }
        if m.burn == 0 && room >= PER_TICK {
            let slot = w.inventory(id).group("fuel");
            let fuel = slot.get(0);
            // Any mod's fuel works: the value is the common `fuel` component.
            if let Some(ticks) = FUEL.on_item(fuel.item).filter(|_| !fuel.is_empty()) {
                m.burn = ticks;
                m.burn_max = ticks;
                // A lava bucket leaves its bucket, like in the furnace.
                slot.set(0, use_one(fuel));
            }
        }
        if m.burn > 0 {
            m.burn -= 1;
            if let Some(e) = w.energy_mut(id) {
                e.stored = (e.stored + PER_TICK).min(e.capacity);
            }
            // A low hum every two seconds, under the test mod's own volume setting.
            if w.time() % 40 == 0 && looks::GENERATOR_HUM.get() {
                if let Some(st) = w.entity(id) {
                    w.play_sound(&HUM, st.pos, 0.4);
                }
            }
        }
    }
}

fn tick_furnaces(b: &mut SystemBatch<Work>) {
    let w = b.world();
    energy_gizmos(&w, b.ids());
    for (id, m, ticks) in b.iter_mut() {
        let Some(e) = w.energy(id) else { continue };
        let stored = e.stored;
        let inv = w.inventory(id);
        let (input_slot, output_slot) = (inv.group("input"), inv.group("output"));
        let (input, output) = (input_slot.get(0), output_slot.get(0));
        let recipe = (!input.is_empty()).then(|| recipe_process(&SMELTING_RECIPES, input.item)).flatten();
        let fits = recipe.is_some_and(|(out, _)| output.is_empty() || (output.stacks_with(&out) && output.count + out.count <= max_stack(out.item)));
        if !fits || stored < FURNACE_USE {
            m.progress = 0;
            continue;
        }
        let (out, time) = recipe.unwrap();
        // Twice as fast as the base furnace, paid for in energy.
        m.progress_max = (time / 2).max(1);
        if ticks > 1 {
            // Catching up: as many steps as the energy, the input and the room allow.
            let room = if output.is_empty() { max_stack(out.item) } else { max_stack(out.item) - output.count } / out.count.max(1);
            let work = (input.count.min(room) * m.progress_max).saturating_sub(m.progress) as i64;
            let steps = (ticks as i64).min(stored / FURNACE_USE).min(work);
            let total = m.progress + steps as u32;
            let done = total / m.progress_max;
            m.progress = total % m.progress_max;
            if let Some(e) = w.energy_mut(id) {
                e.stored -= steps * FURNACE_USE;
            }
            if done > 0 {
                input_slot.set(0, input.shrink(done));
                output_slot.set(0, if output.is_empty() { ItemStack { count: out.count * done, ..out } } else { output.grow(out.count * done) });
            }
            continue;
        }
        m.progress += 1;
        let done = m.progress >= m.progress_max;
        if done {
            m.progress = 0;
        }
        if let Some(e) = w.energy_mut(id) {
            e.stored -= FURNACE_USE;
        }
        if done {
            input_slot.set(0, input.shrink(1));
            output_slot.set(0, if output.is_empty() { out } else { output.grow(out.count) });
        }
    }
}

/// Another mod's blocks, found by name the first time they are used.
static STONE: Block = Block::named("base:stone");
static GRASS: Block = Block::named("base:grass");
static DIRT: Block = Block::named("base:dirt");
static CHEST: Block = Block::named("base:chest");
/// Where the island's chest stands: a block entity's block placed by a generator.
pub const ISLAND_CHEST: [i32; 3] = [4, 64, 0];

/// A floating island of stone and grass in an empty void, with a chest on it.
static ALLOYING: RecipeKind = RecipeKind::named("alloying");
static INFUSING: RecipeKind = RecipeKind::named("infusing");
static ENERGY_PER_TICK: Component<u32> = Component::named("testmod:energy_per_tick");

/// Recipe kinds of this mod, only for the recipe viewer: one shown the usual way with an info line
/// from the recipe's values and a result that comes by chance, one laid out by hand with a fluid.
fn recipe_showcase(r: &mut Registry) {
    r.recipe_kind(&ALLOYING).inputs(2, 1).outputs(2, 1).info_line("recipe.testmod.alloying.info", &["testmod:energy_per_tick", "time"]);
    r.shapeless(&ALLOYING, "tin_ingot", 2, &["#ingots/iron", "#gems/coal"], 100)
        .input_amounts(&[3, 1])
        .chance_output("base:diamond", 1, 0.05)
        .value(&ENERGY_PER_TICK, 32);
    r.recipe_kind(&INFUSING).layout(|ui, recipe| {
        ui.column(S::col().gap(2.0), |ui| {
            ui.row(S::row().gap(4.0).center(), |ui| {
                ui.recipe_input(0);
                ui.text("testmod.infusing.plus");
                ui.recipe_input(1);
                ui.recipe_arrow();
                ui.recipe_outputs();
            });
            ui.text_args("testmod.infusing.time", &[Arg::Int(recipe.time() as i64 / 20)]);
        });
    });
    r.shapeless(&INFUSING, "glow_crystal", 1, &["#gems/diamond", "fluid:#water"], 200).input_amounts(&[1, 1000]);
}

/// Where the needle stands in base's overworld.
pub const NEEDLE: [i32; 2] = [1000, 1000];

/// Basalt boulders on the scorched biome of base's overworld.
fn boulders(g: &mut Gen) {
    let Some(&cols) = g.columns() else { return };
    let [ox, oy, oz] = g.origin();
    let (scorched, basalt) = (SCORCHED.id(), BASALT.id());
    for (i, c) in cols.iter().enumerate() {
        let y = c.height + 1 - oy;
        if c.biome != scorched || !(1..16).contains(&y) {
            continue;
        }
        let (x, y, z) = (i % 16, y as usize, i / 16);
        let h = noise::hash(g.seed() ^ 0xB0, (ox + x as i32) as i64, 0, (oz + z as i32) as i64);
        if h % 40 == 0 && g.get(x, y, z) == 0 && g.get(x, y - 1, z) != 0 {
            g.set(x, y, z, basalt);
        }
    }
}

fn island(g: &mut Gen) {
    let [ox, oy, oz] = g.origin();
    let (stone, grass, dirt) = (STONE.id(), GRASS.id(), DIRT.id());
    for y in 0..16usize {
        for z in 0..16usize {
            for x in 0..16usize {
                let (wx, wy, wz) = ((ox + x as i32) as f32, oy + y as i32, (oz + z as i32) as f32);
                let r = (wx * wx + wz * wz).sqrt();
                let top = 63;
                let depth = (24.0 - r) * 0.8;
                if r < 24.0 && wy <= top && (top - wy) as f32 <= depth {
                    let s = if wy == top { grass } else if top - wy < 3 { dirt } else { stone };
                    g.set(x, y, z, s);
                }
            }
        }
    }
    let [cx, cy, cz] = ISLAND_CHEST;
    let (lx, ly, lz) = (cx - ox, cy - oy, cz - oz);
    if (0..16).contains(&lx) && (0..16).contains(&ly) && (0..16).contains(&lz) {
        g.set(lx as usize, ly as usize, lz as usize, CHEST.id());
    }
}

/// `/tagbench`: what a tag check costs a mod compared with reading a field.
fn tag_bench(cmd: &mut ServerCommand) {
    static PLANT_SOIL: Tag = Tag::named("plant_soil");
    let n = cmd.int("millions").unwrap_or(10).clamp(1, 1000) as u32 * 1_000_000;
    let states: Vec<u32> = (0..256).map(|i| i * 7 % 120).collect();
    let flags: Vec<u32> = (0..256).map(|i| i * 31).collect();
    let time = |f: &dyn Fn(u32) -> u32| {
        let t = std::time::Instant::now();
        let mut acc = 0u32;
        for i in 0..n {
            acc = acc.wrapping_add(f(states[(i & 255) as usize]));
        }
        std::hint::black_box(acc);
        t.elapsed().as_secs_f64() * 1e9 / n as f64
    };
    let field = time(&|s| flags[s as usize & 255]);
    let tag = time(&|s| PLANT_SOIL.on_block(s) as u32);
    let host_call = time(&|s| state_flags(s));
    let by_name = time(&|s| block_has_tag(s, "plant_soil") as u32);
    cmd.reply("command.testmod.tagbench", &[Arg::Float(field, 2), Arg::Float(tag, 2), Arg::Float(host_call, 2), Arg::Float(by_name, 2)]);
}
