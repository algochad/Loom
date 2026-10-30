//! One flat world where every check that needs no input from the player runs at once, each
//! scenario on a plot of its own: they wait for game time side by side instead of one after
//! another. Chat commands and world rules answer through the chat, so the scenarios using them
//! run one after another in `chained`.

use std::collections::HashMap;
use std::time::Duration;

use loom_sdk::*;

use super::{add, at};
use crate::runner::{self, R, T};
use crate::server::MARK;

/// Plots are this far apart: liquids spread 7 blocks, mobs wander a few.
const PLOT: i32 = 40;

pub async fn scene(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let plot = |i: i32, j: i32| add(f, [i * PLOT, 0, j * PLOT]);
    runner::all(vec![
        runner::scenario(&t, "sync_edits", move |t| sync_edits(t, plot(1, 0))),
        runner::scenario(&t, "sand_and_support", move |t| sand_and_support(t, plot(-1, 0))),
        runner::scenario(&t, "furnace", move |t| furnace(t, plot(0, 1))),
        runner::scenario(&t, "water_flow", move |t| water_flow(t, plot(0, -1))),
        runner::scenario(&t, "water_finds_hole", move |t| water_finds_hole(t, plot(1, 1))),
        runner::scenario(&t, "infinite_water", move |t| infinite_water(t, plot(-1, -1))),
        runner::scenario(&t, "lava_and_water", move |t| lava_and_water(t, plot(1, -1))),
        runner::scenario(&t, "lava_kills", move |t| lava_kills(t, plot(-1, 1))),
        runner::scenario(&t, "energy_grid", move |t| energy_grid(t, plot(2, 0))),
        runner::scenario(&t, "pathfinding", move |t| pathfinding(t, plot(-2, 0))),
        runner::scenario(&t, "crate_and_crowd", move |t| crate_and_crowd(t, plot(0, 2))),
        runner::scenario(&t, "chained", move |t| chained(t, plot(0, -2))),
    ])
    .await
}

/// The scenarios that talk through the chat, one after another.
async fn chained(t: T, o: [i32; 3]) -> R {
    for (name, run) in [
        ("commands", 0),
        ("damage_chain", 1),
        ("loot_tables", 2),
        ("tool_tiers", 3),
        ("interceptor", 4),
        ("watcher_sync", 5),
        ("facing_states", 6),
    ] {
        let s = t.scoped(name);
        let r = match run {
            0 => commands(&s).await,
            1 => damage_chain(&s).await,
            2 => loot_tables(&s).await,
            3 => tool_tiers(&s, o).await,
            4 => interceptor(&s, add(o, [0, 0, 12])).await,
            5 => watcher_sync(&s, add(o, [12, 0, 0])).await,
            _ => facing_states(&s).await,
        };
        if let Err(e) = r {
            s.fail(e);
        }
    }
    Ok(())
}

async fn count_at(t: &T, kind: &str, p: [i32; 3], r: f64) -> Result<usize, String> {
    let list = t.op(&format!("near {} {} {} {r}", p[0], p[1], p[2])).await?;
    Ok(list.split_whitespace().filter(|e| e.ends_with(&format!(":{kind}"))).count())
}

fn counts(s: &str) -> HashMap<String, u32> {
    s.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.parse().unwrap_or(0))).collect()
}

/// First number of an `attr`/`damage` answer ("6.08 | damage: 8, ...").
fn value(s: &str) -> f64 {
    s.split('|').next().and_then(|v| v.trim().parse().ok()).unwrap_or(f64::NAN)
}

async fn spawn(t: &T, kind: &str, p: [i32; 3]) -> Result<u64, String> {
    let id = t.op(&format!("spawn {kind} {}", at(p))).await?;
    id.trim().parse().map_err(|_| format!("spawn {kind}: {id}"))
}

async fn pos_of(t: &T, id: u64) -> Result<[f64; 3], String> {
    let line = t.op(&format!("entity {id}")).await?;
    let v: Vec<f64> = line.split_whitespace().skip(1).take(3).filter_map(|s| s.parse().ok()).collect();
    v.try_into().map_err(|_| format!("entity #{id}: {line}"))
}

/// Edits on the server reach the client block for block; an entity is where the server has it.
async fn sync_edits(t: T, o: [i32; 3]) -> R {
    t.fill(o, add(o, [4, 2, 4]), "cobblestone").await?;
    t.fill(add(o, [1, 1, 1]), add(o, [3, 1, 3]), "glass").await?;
    t.set(add(o, [2, 2, 2]), "furnace[facing=east,lit=true]").await?;
    t.set(add(o, [0, 2, 0]), "air").await?;
    t.frames(5).await?;
    t.sync_blocks(o, add(o, [4, 2, 4])).await?;
    let pig = spawn(&t, "pig", add(o, [0, 0, 8])).await?;
    t.until("client sees the pig", Duration::from_secs(5), |t| t.c().entities().contains(&pig)).await?;
    t.frames(20).await?;
    let s = pos_of(&t, pig).await?;
    if let Some(e) = t.c().entity(pig) {
        let d = ((e.pos[0] - s[0]).powi(2) + (e.pos[1] - s[1]).powi(2) + (e.pos[2] - s[2]).powi(2)).sqrt();
        t.check(d < 1.5, format!("pig is {d:.2} blocks off between server and client"));
    }
    Ok(())
}

/// Sand falls as an entity and lands; a torch drops when its block goes.
async fn sand_and_support(t: T, o: [i32; 3]) -> R {
    let air = add(o, [0, 4, 0]);
    t.set(air, "sand").await?;
    let mut falling = false;
    for _ in 0..20 {
        falling |= count_at(&t, "falling_block", air, 6.0).await? == 1;
        t.frames(2).await?;
    }
    t.check(falling, "the sand falls as an entity");
    t.secs(2.0).await?;
    t.check_eq(t.block(air).await?, "core:air".into(), "sand left the air");
    t.check_eq(t.block(o).await?, "base:sand".into(), "sand landed on the ground");
    let stone = add(o, [6, 0, 0]);
    t.set(stone, "stone").await?;
    t.set(add(stone, [0, 1, 0]), "torch").await?;
    t.set(stone, "air").await?;
    t.frames(5).await?;
    t.check_eq(t.block(add(stone, [0, 1, 0])).await?, "core:air".into(), "torch without support");
    t.check(count_at(&t, "item", stone, 3.0).await? >= 1, "the torch dropped as an item");
    Ok(())
}

/// Smelting with coal, the lit state, and a lava bucket leaving its bucket behind.
async fn furnace(t: T, o: [i32; 3]) -> R {
    let (a, b) = (o, add(o, [4, 0, 0]));
    t.set(a, "furnace").await?;
    t.set(b, "furnace").await?;
    t.op(&format!("insert {} raw_iron 1", at(a))).await?;
    t.op(&format!("insert {} coal 1", at(a))).await?;
    t.op(&format!("insert {} sand 1", at(b))).await?;
    t.op(&format!("insert {} lava_bucket 1", at(b))).await?;
    t.secs(1.0).await?;
    let lit = t.block(a).await?;
    t.check(lit.starts_with("base:furnace[") && lit.contains("lit=true"), format!("burning furnace is {lit}"));
    t.check_eq(t.wait_slot(&at(a), 2, 20.0).await?, ("base:iron_ingot".into(), 1, 0), "furnace output");
    t.check_eq(t.wait_slot(&at(b), 2, 5.0).await?.0, "base:glass".into(), "sand smelted with lava");
    t.check_eq(t.slot(&at(b), 1).await?.0, "base:bucket".into(), "lava bucket leaves the bucket");
    Ok(())
}

/// On open flat ground: diamond of radius 7, level 7 at the rim, not faster than vanilla (water
/// steps every 5 ticks).
async fn water_flow(t: T, s: [i32; 3]) -> R {
    t.set(s, "water").await?;
    // Ring k arrives after k * 5 ticks: at one second only about four rings exist.
    t.secs(1.0).await?;
    t.check_eq(t.block(add(s, [3, 0, 0])).await?, "base:water[level=3]".into(), "three blocks out after a second");
    t.check_eq(t.block(add(s, [6, 0, 0])).await?, "core:air".into(), "six blocks out after a second");
    t.secs(2.5).await?;
    for (d, want) in [([7, 0, 0], "base:water[level=7]"), ([0, 0, -7], "base:water[level=7]"), ([4, 0, 3], "base:water[level=7]"), ([8, 0, 0], "core:air"), ([4, 0, 4], "core:air"), ([1, 0, 0], "base:water[level=1]")] {
        t.check_eq(t.block(add(s, d)).await?, want.into(), &format!("water at {d:?}"));
    }
    Ok(())
}

/// Flowing water goes only towards a hole within reach instead of in every direction.
async fn water_finds_hole(t: T, s: [i32; 3]) -> R {
    let hole = add(s, [3, -1, 0]);
    t.set(hole, "air").await?;
    t.set(s, "water").await?;
    t.secs(3.0).await?;
    t.check_eq(t.block(hole).await?, "base:water[level=8]".into(), "the hole filled (falling water)");
    t.check_eq(t.block(add(s, [-1, 0, 0])).await?, "core:air".into(), "no spread away from the hole");
    t.check_eq(t.block(add(s, [0, 0, 1])).await?, "core:air".into(), "no spread sideways");
    Ok(())
}

/// Two sources with one block between them over solid ground make a third.
async fn infinite_water(t: T, a: [i32; 3]) -> R {
    t.set(a, "water").await?;
    t.set(add(a, [2, 0, 0]), "water").await?;
    t.secs(1.5).await?;
    t.check_eq(t.block(add(a, [1, 0, 0])).await?, "base:water[level=0]".into(), "new source between two");
    // Lava is not renewable: between two sources it only flows.
    let l = add(a, [0, 0, 12]);
    t.set(l, "lava").await?;
    t.set(add(l, [2, 0, 0]), "lava").await?;
    t.secs(2.0).await?;
    let mid = t.block(add(l, [1, 0, 0])).await?;
    t.check(mid.starts_with("base:lava[") && mid != "base:lava[level=0]", format!("between two lava sources: {mid}"));
    Ok(())
}

/// Lava touched by water: sources harden to obsidian, flowing lava to cobblestone; lava falling
/// into water makes stone.
async fn lava_and_water(t: T, o: [i32; 3]) -> R {
    let a = o;
    t.fill(add(a, [-1, -1, -1]), add(a, [3, -1, 1]), "stone").await?;
    t.set(a, "lava").await?;
    t.set(add(a, [2, 0, 0]), "water").await?;
    let b = add(o, [0, 0, 14]);
    t.set(b, "lava").await?;
    let c = add(o, [14, 0, 0]);
    t.set(c, "water").await?;
    t.set(add(c, [0, 2, 0]), "lava").await?;
    t.secs(1.0).await?;
    t.check_eq(t.block(a).await?, "base:obsidian".into(), "lava source next to water");
    // Lava needs 30 ticks per step.
    t.secs(1.0).await?;
    t.check_eq(t.block(add(b, [1, 0, 0])).await?, "base:lava[level=2]".into(), "lava flowed one block");
    t.set(add(b, [1, 1, 0]), "water").await?;
    t.secs(0.5).await?;
    t.check_eq(t.block(add(b, [1, 0, 0])).await?, "base:cobblestone".into(), "flowing lava under water");
    t.secs(1.5).await?;
    t.check_eq(t.block(c).await?, "base:stone".into(), "lava falling into water");
    Ok(())
}

/// A pig in lava dies.
async fn lava_kills(t: T, o: [i32; 3]) -> R {
    let pool = add(o, [0, -1, 0]);
    t.fill(pool, add(pool, [1, 0, 1]), "lava").await?;
    let pig = spawn(&t, "pig", pool).await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if t.op(&format!("entity {pig}")).await.is_err() {
            return Ok(());
        }
        t.secs(0.5).await?;
    }
    t.fail("a pig in lava did not die within 8 seconds");
    Ok(())
}

/// The test mod's generator powers its electric furnace through cables.
async fn energy_grid(t: T, g: [i32; 3]) -> R {
    t.set(g, "testmod:generator").await?;
    t.set(add(g, [-1, 0, 0]), "testmod:cable").await?;
    t.set(add(g, [-2, 0, 0]), "testmod:cable").await?;
    let e = add(g, [-3, 0, 0]);
    t.set(e, "testmod:electric_furnace").await?;
    t.op(&format!("insert {} coal 4", at(g))).await?;
    t.op(&format!("insert {} iron_ore 2", at(e))).await?;
    t.check_eq(t.wait_slot(&at(e), 1, 20.0).await?.0, "base:iron_ingot".into(), "electric furnace output");
    Ok(())
}

/// Navigation finds the way around a wall through its only gap.
async fn pathfinding(t: T, o: [i32; 3]) -> R {
    t.fill(add(o, [3, 0, -6]), add(o, [3, 2, 6]), "cobblestone").await?;
    t.fill(add(o, [3, 0, 5]), add(o, [3, 1, 5]), "air").await?;
    let id = spawn(&t, "testmod:idol", o).await?;
    let goal = add(o, [7, 0, 0]);
    t.check_eq(t.op(&format!("navigate {id} {}", at(goal))).await?, "1".to_string(), "the walk is accepted");
    t.secs(8.0).await?;
    let p = pos_of(&t, id).await?;
    let d = ((p[0] - goal[0] as f64 - 0.5).powi(2) + (p[2] - goal[2] as f64 - 0.5).powi(2)).sqrt();
    t.check(d < 1.5, format!("the idol walked around the wall ({:.1} {:.1}, {d:.1} from the target)", p[0], p[2]));
    Ok(())
}

/// A crate falls like any entity; mobs (soft bodies) standing in one spot drift apart.
async fn crate_and_crowd(t: T, o: [i32; 3]) -> R {
    let id = spawn(&t, "testmod:crate", add(o, [0, 3, 0])).await?;
    let a = spawn(&t, "testmod:idol", add(o, [4, 0, 0])).await?;
    let b = spawn(&t, "testmod:idol", add(o, [4, 0, 0])).await?;
    t.secs(1.5).await?;
    let c = pos_of(&t, id).await?;
    t.check((c[1] - o[1] as f64).abs() < 0.01, format!("the crate landed (y {:.2})", c[1]));
    let (pa, pb) = (pos_of(&t, a).await?, pos_of(&t, b).await?);
    t.check(((pa[0] - pb[0]).powi(2) + (pa[2] - pb[2]).powi(2)).sqrt() > 0.4, "two idols in one spot pushed each other apart");
    Ok(())
}

async fn commands(t: &T) -> R {
    let help = t.cmd("/help give").await?;
    t.check(help.contains("/give"), format!("/help give said '{help}'"));
    let bad = t.cmd("/give nothing_here").await?;
    t.check(bad.contains("usage") || bad.contains("unknown"), format!("bad item reply '{bad}'"));
    let me = t.feet();
    t.cmd("/summon pig 3").await?;
    t.frames(3).await?;
    t.check_eq(count_at(t, "base:pig", me, 4.0).await?, 3, "summoned pigs");
    let feed = t.cmd("/feed 9").await?;
    t.check(feed.contains('9'), format!("/feed said '{feed}'"));
    let time = t.cmd("/time query").await?;
    t.check(time.contains("time is"), format!("/time query said '{time}'"));
    Ok(())
}

/// Damage passes the base game's armor step and the test kit's final step, in stage order,
/// and the result can be explained.
async fn damage_chain(t: &T) -> R {
    let bare = t.op("damage 8 0").await?;
    t.check((value(&bare) - 8.0).abs() < 1e-6, format!("no armor: {bare}"));
    // Iron chestplate: 6 points, 4% each.
    t.op("put me 37 iron_chestplate 1").await?;
    let armored = t.op("damage 8 0").await?;
    t.check((value(&armored) - 6.08).abs() < 1e-4, format!("iron chestplate: {armored}"));
    let fall = t.op(&format!("damage 8 {}", abi::damage::FALL)).await?;
    t.check((value(&fall) - 8.0).abs() < 1e-6, format!("armor does not soften falls: {fall}"));
    t.cmd("/rule testkit:half_damage 1").await?;
    let both = t.op("damage 8 0").await?;
    t.cmd("/rule testkit:half_damage 0").await?;
    t.check((value(&both) - 3.04).abs() < 1e-4, format!("armor then halved: {both}"));
    t.check(both.contains("base at stage 200") && both.contains("testkit at stage 300"), format!("explanation names both steps: {both}"));
    Ok(())
}

/// Vanilla's chances, measured over many rolls; a modifier from another mod adds to a table.
async fn loot_tables(t: &T) -> R {
    let oak = counts(&t.op("loot base:oak_leaves 4000").await?);
    let saplings = oak.get("base:sapling").copied().unwrap_or(0);
    t.check((140..=260).contains(&saplings), format!("saplings from 4000 oak leaves: {saplings} (vanilla 5%: 200)"));
    let apples = oak.get("base:apple").copied().unwrap_or(0);
    t.check((5..=40).contains(&apples), format!("apples from 4000 oak leaves: {apples} (vanilla 0.5%: 20)"));
    let birch = counts(&t.op("loot base:birch_leaves 2000").await?);
    t.check(!birch.contains_key("base:apple"), "birch leaves gave apples");
    let zombie = counts(&t.op("loot base:zombie 2000").await?);
    let flesh = zombie.get("base:rotten_flesh").copied().unwrap_or(0);
    t.check((1700..=2300).contains(&flesh), format!("rotten flesh from 2000 zombies: {flesh} (0..2 each)"));
    let tin = zombie.get("testmod:tin_ingot").copied().unwrap_or(0);
    t.check((50..=160).contains(&tin), format!("tin from 2000 zombies through testmod's modifier: {tin} (5%: 100)"));
    let pig = counts(&t.op("loot base:pig 200 fire").await?);
    t.check(pig.contains_key("base:cooked_porkchop") && !pig.contains_key("base:porkchop"), format!("burning pigs drop cooked meat: {pig:?}"));
    let grass = counts(&t.op("loot base:tall_grass 4000").await?);
    let seeds = grass.get("base:wheat_seeds").copied().unwrap_or(0);
    t.check((380..=620).contains(&seeds), format!("seeds from 4000 tall grass: {seeds} (vanilla 12.5%: 500)"));
    let stone = counts(&t.op("loot base:stone 10").await?);
    t.check_eq(stone.get("base:cobblestone").copied(), Some(10), "drops() is a one entry table, broken by the world");
    t.check_eq(t.op("loot base:stone 10 mined").await?, String::new(), "stone mined by hand drops nothing");
    let stone = counts(&t.op("loot base:stone 10 mined wood_pickaxe").await?);
    t.check_eq(stone.get("base:cobblestone").copied(), Some(10), "stone mined with a pickaxe");
    t.check_eq(t.op("loot base:bricks 10 mined").await?, String::new(), "a block's own item needs the tool too");
    let shorn = counts(&t.op("loot base:oak_leaves 4000 mined").await?);
    let saplings = shorn.get("base:sapling").copied().unwrap_or(0);
    t.check((140..=260).contains(&saplings), format!("saplings from 4000 oak leaves mined by hand: {saplings}"));
    Ok(())
}

/// Harvest levels: iron ore needs a stone pickaxe; tiers are numbers, so the test mod's steel
/// pickaxe (250) mines its reinforced stone (250) and what iron (200) does, not obsidian (300).
async fn tool_tiers(t: &T, o: [i32; 3]) -> R {
    let mut drops = 0;
    for (i, (block, tool, gets)) in [
        ("stone", "stick", false),
        ("stone", "wood_pickaxe", true),
        ("iron_ore", "wood_pickaxe", false),
        ("iron_ore", "stone_pickaxe", true),
        ("diamond_ore", "stone_pickaxe", false),
        ("testmod:reinforced_stone", "iron_pickaxe", false),
        ("testmod:reinforced_stone", "testmod:steel_pickaxe", true),
        ("gold_ore", "testmod:steel_pickaxe", true),
        ("obsidian", "testmod:steel_pickaxe", false),
    ]
    .into_iter()
    .enumerate()
    {
        let p = add(o, [i as i32 * 2 - 8, 0, 0]);
        t.set(p, block).await?;
        t.op(&format!("break {} {tool}", at(p))).await?;
        t.frames(5).await?;
        drops += gets as usize;
        t.check_eq(count_at(t, "core:item", o, 10.0).await?, drops, &format!("{tool} on {block}: {}", if gets { "drops" } else { "nothing" }));
    }
    Ok(())
}

/// An interceptor refuses breaking gold while its rule is on.
async fn interceptor(t: &T, p: [i32; 3]) -> R {
    t.set(p, "gold_block").await?;
    t.cmd("/rule testkit:protect_gold 1").await?;
    let refused = t.op(&format!("break {} diamond_pickaxe", at(p))).await?;
    t.cmd("/rule testkit:protect_gold 0").await?;
    t.check_eq(refused, "0".into(), "break refused");
    t.check_eq(t.block(p).await?, "base:gold_block".into(), "the gold stays");
    t.check_eq(t.op(&format!("break {} diamond_pickaxe", at(p))).await?, "1".into(), "break allowed again");
    t.check_eq(t.block(p).await?, "core:air".into(), "broken");
    Ok(())
}

/// Components of other entities reach everyone who sees them, also when a system changes them.
async fn watcher_sync(t: &T, o: [i32; 3]) -> R {
    let pig = spawn(t, "pig", o).await?;
    t.until("pig known", Duration::from_secs(5), |t| t.c().entities().contains(&pig)).await?;
    t.op(&format!("mark {pig} 77")).await?;
    t.until("mark on the client", Duration::from_secs(5), |t| t.c().view().component_of(pig, &MARK) == Some(77)).await?;
    t.op(&format!("mark {pig} 78")).await?;
    t.until("changed mark on the client", Duration::from_secs(5), |t| t.c().view().component_of(pig, &MARK) == Some(78)).await?;
    // The test kit's system counts marks up once a tick while the rule is on.
    t.cmd("/rule testkit:count_marks 1").await?;
    t.secs(1.5).await?;
    t.cmd("/rule testkit:count_marks 0").await?;
    t.frames(5).await?;
    let now = t.c().view().component_of(pig, &MARK).unwrap_or(0);
    t.check((100..=120).contains(&now), format!("mark after 1.5 s of a counting system: {now} (78 + about 30 ticks)"));
    Ok(())
}

/// A facing property holds only the allowed directions: the vent faces up, north or east, the
/// torch any way but down; the furnace's `lit` combines with its facing.
async fn facing_states(t: &T) -> R {
    static VENT: Block = Block::named("testmod:vent");
    static FURNACE: Block = Block::named("base:furnace");
    static TORCH: Block = Block::named("base:torch");
    t.check(VENT.with(&[("facing", "north")]) != u32::MAX, "the vent faces north");
    t.check_eq(VENT.with(&[("facing", "down")]), u32::MAX, "the vent never faces down");
    t.check_eq(TORCH.with(&[("facing", "down")]), u32::MAX, "the torch never faces down");
    let lit = FURNACE.with(&[("facing", "east"), ("lit", "true")]);
    t.check_eq(state_prop(lit, "facing"), Some("east"), "facing of a lit furnace");
    t.check_eq(state_with(lit, "lit", "false").and_then(|s| state_prop(s, "facing")), Some("east"), "putting it out keeps its facing");
    t.check_eq(block_name(lit).to_string(), "base:furnace[facing=east,lit=true]".to_string(), "the state's name");
    Ok(())
}
