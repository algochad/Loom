//! Things that only change how the game looks, all as plain mod code and assets:
//! - a glow crystal whose veins glow by a `_e` mask (LabPBR-style emission, no light source),
//!   and a glowing birch, leaves, moss and flower with masks of their own,
//! - sparks: a particle kind with its own shader, sent from the server for everyone nearby,
//! - wisps: floating entities drawn by an entity shader, their core glowing by a mask,
//! - a rune whose tooltip uses text effects (engine and own) and conditions (own and engine),
//!   and which shows the area it would affect as a see-through box while held,
//! - a screen effect toggled with a key, and machine screens drawn by a GUI shader (named in a `.gui`),
//! - an idol from a Blockbench model file (bones, a mesh pair of wings) that idles by keyframes
//!   and bows when used, and an orb whose model is built in code (a sphere mesh and a ring of
//!   stones on a child bone) but animated from a file,
//! - the player waves when using the rune: keyframes for the player's model in third person and
//!   for the first person hand.

use loom_sdk::*;

pub static SPARK: Particle = Particle::named("spark");
static WISP: Entity = Entity::named("wisp");
static IDOL: Entity = Entity::named("idol");
static ORB: Entity = Entity::named("orb");
static MOSAIC: Block = Block::named("mosaic");
static GLOW_MOSS: Block = Block::named("glow_moss");
static GLOW_LOG: Block = Block::named("glow_log");
static GLOW_LEAVES: Block = Block::named("glow_leaves");
static GLOW_FLOWER: Block = Block::named("glow_flower");
static GLOW_CRYSTAL: Block = Block::named("glow_crystal");
static GLOW: TintKind = TintKind::named("glow");
static SPIRITS: EntityCategory = EntityCategory::named("spirits");
static HURT: TextCondition = TextCondition::named("hurt");
static DREAM_KEY: Key = Key::named("dream");
static RUNE: Item = Item::named("rune");
/// Drawn from `models/item/totem.json`: a stone base and a crystal, no icon of its own.
static TOTEM: Item = Item::named("totem");
/// Idols currently bowing.
static BOWED: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
/// Settings players change in Settings > Mod Settings.
/// The generator's low hum (in `lib.rs`).
pub static GENERATOR_HUM: Config<bool> = Config::named("generator_hum");
static WISP_SPARKS: Config<i32> = Config::named("wisp_sparks");
static DREAM_STRENGTH: Config<f32> = Config::named("dream_strength");
static DREAM_STYLE: Config<u32> = Config::named("dream_style");

pub fn register(r: &mut Registry) {
    r.config(&GENERATOR_HUM).bool(true);
    // The server decides how much its wisps sparkle; joining players get its value.
    r.config(&WISP_SPARKS).int(2, 0, 10).server();
    r.config(&DREAM_STRENGTH).float(1.0, 0.1, 1.0).step(0.05);
    r.config(&DREAM_STYLE).choice(0, &["warm", "cold"]);
    r.particle(&SPARK)
        .textures(&["core:glint_3", "core:glint_2", "core:glint_1", "core:glint_0"])
        .life(0.6, 1.2)
        .size(0.12, 0.2)
        .gravity(-0.6)
        .drag(0.9)
        .color(0xFFFFFFFF, 0xFFFFFF60)
        .fullbright()
        .translucent()
        .shader("twinkle");

    // A glowing obelisk here and there in the deserts of another mod's world.
    static OBELISK: Template = Template::named("obelisk");
    let mut obelisk = r.template(&OBELISK).layer(&["sss", "sss", "sss"]);
    for _ in 0..4 {
        obelisk = obelisk.layer(&["   ", " g ", "   "]);
    }
    drop(obelisk.key('s', "base:sandstone").key('g', "glow_crystal"));
    r.structure(&crate::OVERWORLD, &OBELISK).spacing(128).chance(0.5).biome("#dry").buried(1);
    // Glow crystals grow only in the overworld's deep caves (an underground biome).
    r.ore(&GLOW_CRYSTAL).veins(3).size(6).replace("#stone_ore_replaceables").biome("#cave");
    // A 64x64 texture among 16x16 ones: every texture keeps its own size.
    r.block(&MOSAIC).hardness(1.5).mineable("pickaxe");
    // A tint kind of the mod's own: violet by default (and on the item), teal in plains.
    r.tint_kind(&GLOW, 0xc080ff);
    r.biome_tint(&Biome::named("base:plains"), &GLOW, 0x40e0c0);
    r.block(&GLOW_MOSS).tint(&GLOW).hardness(0.6).mineable("shovel");
    // A glowing birch (a log's `tree` component grows it from a sapling or worldgen) and
    // flower; not generated in the world, only from the creative tabs.
    static TREE_LEAVES: Component<BlockRef> = Component::named("tree_leaves");
    r.block(&GLOW_LOG).textures("glow_log_top", "glow_log", "glow_log_top").hardness(2.0).mineable("axe").tag("logs").set_text(&TREE_LEAVES, "glow_leaves");
    r.block(&GLOW_LEAVES).cutout().opacity(1).tint(&GLOW).hardness(0.2).tag("leaves").mineable("shears");
    r.block(&GLOW_FLOWER).plant().needs_below("#plant_soil").tint(&GLOW);
    r.block(&GLOW_CRYSTAL)
        .hardness(1.5)
        .mineable("pickaxe")
        // Sparks now and then, shown by the client near the player.
        .ambient_particle(SPARK.name());

    // Its own category (no spawning rule: wisps come only from commands and creative): they
    // drift off again when nobody is within 64 blocks.
    r.entity_category(&SPIRITS, 4, 64.0);
    r.entity(&WISP)
        .category(&SPIRITS)
        .size(0.4, 0.4)
        .health(4.0)
        .speed(1.5)
        .flags(entity_flags::NO_GRAVITY)
        .unpushable()
        
        .model(Model::from(vec![Part::new("wisp", [0.0, 3.0, 0.0]).cube([-3.0, -3.0, -3.0], [6.0, 6.0, 6.0], [0.0, 0.0])]).uv(32, 16))
        .shader("ghost")
        .on_tick(wisps);

    r.entity(&IDOL).size(0.6, 1.1).health(20.0).model_file("idol").animations("idol").on_interact(|e| {
        let mut bowed = BOWED.lock().unwrap();
        let id = e.entity();
        if let Some(i) = bowed.iter().position(|&b| b == id) {
            bowed.swap_remove(i);
            e.world().play_animation(id, "", false);
        } else {
            // Bows to whoever used it.
            let w = e.world();
            if let (Some(mut me), Some(them)) = (w.entity(id), w.entity(e.other())) {
                me.yaw = (-(them.pos[0] - me.pos[0]) as f32).atan2(-(them.pos[2] - me.pos[2]) as f32);
                w.set_entity(id, &me);
            }
            bowed.push(id);
            w.play_animation(id, "bow", false);
        }
    });
    let ring = Part::new("ring", [0.0, 8.0, 0.0]).child_of(0);
    let ring = [[6.0, -1.0, -1.0], [-8.0, -1.0, -1.0], [-1.0, -1.0, 6.0], [-1.0, -1.0, -8.0]].into_iter().fold(ring, |p, at| p.cube(at, [2.0, 2.0, 2.0], [16.0, 0.0]));
    r.entity(&ORB)
        .size(0.6, 0.9)
        .health(4.0)
        .flags(entity_flags::NO_GRAVITY)
        
        .model(Model::from(vec![Part::new("core", [0.0, 8.0, 0.0]).mesh(sphere([0.0, 8.0, 0.0], 4.0, 8, 12, [0.0, 0.0, 16.0, 16.0])), ring]).uv(32, 16));

    r.set_text_on(Target::Entity("core:player"), &ANIMATIONS, "testmod:player");
    r.item(&TOTEM).max_stack(1);
    r.item(&RUNE).max_stack(1).on_use(|u| {
        let w = u.world();
        w.play_animation(u.player(), "wave", false);
        if let Some(st) = w.entity(u.player()) {
            w.spawn_particles(&SPARK, [st.pos[0], st.pos[1] + 1.2, st.pos[2]]).count(40).spread(0.2).speed(4.0);
        }
        true
    });
    r.text_condition(&HURT, |v| v.me().is_some_and(|m| m.health < m.max_health));

    r.key(&DREAM_KEY, "N").on_press(|c| {
        let on = DREAM.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
        let effect = if DREAM_STYLE.get() == 1 { "dream_cold" } else { "dream" };
        c.post_effect("dream", 0.0);
        c.post_effect("dream_cold", 0.0);
        if !on {
            c.post_effect(effect, DREAM_STRENGTH.get());
        }
    });
    r.client_frame(|c| {
        // Holding the rune shows what it would reach: a see-through sheet over the ground around.
        let st = c.state();
        if c.inventory_slot(st.selected).item == RUNE.id() {
            let p = st.pos.map(f64::floor);
            c.draw_fill(None, [p[0] - 2.0, p[1], p[2] - 2.0], [p[0] + 3.0, p[1] + 0.06, p[2] + 3.0], 0x60D0FF50);
        }
    });
}

static DREAM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Wisps drift in a new direction every two seconds, bob up and down and shed sparks.
fn wisps(t: &EntityTick) {
    let w = t.world();
    let time = w.time();
    for &id in t.ids() {
        let Some(mut st) = w.entity(id) else { continue };
        let phase = (id % 40) as u64;
        if (time + phase) % 40 == 0 {
            let a = w.random_below(360) as f32 * std::f32::consts::PI / 180.0;
            st.yaw = a;
        }
        st.vel[1] = ((time as f64 + phase as f64) * 0.1).sin() * 0.8;
        w.set_entity(id, &st);
        w.walk(id, [-st.yaw.sin(), -st.yaw.cos()], 1.0, false);
        if (time + phase) % 20 == 0 {
            w.spawn_particles(&SPARK, [st.pos[0], st.pos[1] + 0.2, st.pos[2]]).count(WISP_SPARKS.get().max(0) as u32).spread(0.15).speed(0.3);
        }
    }
}
