//! Batched events and placement by facing; the other hooks are checked in the scene.

use std::collections::HashMap;
use std::time::Duration;


use super::{add, at};
use crate::runner::{R, T};

fn counts(s: &str) -> HashMap<String, u32> {
    s.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.parse().unwrap_or(0))).collect()
}

pub async fn events_batched(t: T) -> R {
    t.flat().await?;
    t.op("events reset").await?;
    let f = t.feet();
    for i in 0..3 {
        let p = add(f, [3, 0, i]);
        t.set(p, "stone").await?;
        t.op(&format!("break {} stone_pickaxe", at(p))).await?;
    }
    t.op("put me 0 dirt 4").await?;
    t.c().select(0);
    let ground = add(f, [0, -1, -2]);
    t.look_at(ground);
    t.frames(3).await?;
    t.press("core:use");
    t.until("dirt placed", Duration::from_secs(5), |t| t.c().inventory_slot(0).count == 3).await?;
    t.cmd("/feed 10").await?;
    t.op("put me 1 bread 2").await?;
    t.c().select(1);
    t.secs(0.3).await?;
    t.press("core:use");
    t.until("ate", Duration::from_secs(5), |t| t.c().inventory_slot(1).count == 1).await?;
    // The broken stone dropped cobblestone next to the player.
    t.c().teleport([f[0] as f64 + 3.5, f[1] as f64, f[2] as f64 + 1.5]);
    t.secs(1.0).await?;
    let n = counts(&t.op("events").await?);
    t.check_eq(n["block_broken"], 3, "block_broken events");
    t.check_eq(n["block_placed"], 1, "block_placed events");
    t.check_eq(n["item_used"], 1, "item_used events (eating; placing a block is not one)");
    t.check(n["item_picked_up"] >= 1, format!("item_picked_up events: {}", n["item_picked_up"]));
    Ok(())
}

/// The questions before things happen and timers: a denied `HURT` keeps a player alive, a
/// denied `USE_ITEM` keeps the bread, a timer fires its event once, when due.
pub async fn vetoes_and_timers(t: T) -> R {
    t.flat().await?;
    t.cmd("/rule testkit:no_hurt 1").await?;
    t.cmd("/kill").await?;
    t.secs(0.3).await?;
    t.check(!t.flag(loom_sdk::client::client_flags::DEAD), "question::HURT denied, the player lives");
    t.cmd("/rule testkit:no_hurt 0").await?;
    t.cmd("/feed 10").await?;
    t.op("put me 1 bread 2").await?;
    t.c().select(1);
    t.cmd("/rule testkit:no_use 1").await?;
    t.secs(0.3).await?;
    t.press("core:use");
    t.secs(0.5).await?;
    t.check_eq(t.c().inventory_slot(1).count, 2, "question::USE_ITEM denied, the bread stays");
    t.cmd("/rule testkit:no_use 0").await?;
    t.press("core:use");
    t.until("ate", Duration::from_secs(5), |t| t.c().inventory_slot(1).count == 1).await?;
    let before: u32 = t.op("rings").await?.parse().unwrap_or(0);
    t.op("after 30").await?;
    t.op("after 20").await?;
    t.secs(0.5).await?;
    t.check_eq(t.op("rings").await?, before.to_string(), "not before it is due");
    t.secs(1.5).await?;
    t.check_eq(t.op("rings").await?, (before + 1).to_string(), "one timer per entity and event: the second moved the first");
    Ok(())
}

/// Placing picks the allowed facing nearest the player's look: the test mod's vent (up, north,
/// east) put against a wall faces the player.
pub async fn facing_placement(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    t.op("mode creative").await?;
    t.op("put me 0 testmod:vent 4").await?;
    t.c().select(0);
    // Against a wall two blocks south, looking at it: the front turns north, towards the player
    // (looking steeply down it would face up).
    let wall = add(f, [0, 0, 3]);
    t.fill(wall, add(wall, [0, 1, 0]), "cobblestone").await?;
    t.look_at(wall);
    t.frames(2).await?;
    t.press("core:use");
    t.secs(0.3).await?;
    t.check_eq(t.block(add(wall, [0, 0, -1])).await?, "testmod:vent[facing=north]".to_string(), "the vent faces north");
    // States by name in a command, and one property changed by name keeping the other.
    let p = add(f, [3, 0, 0]);
    t.cmd(&format!("/setblock {} base:furnace[lit=true,facing=east]", at(p))).await?;
    t.check_eq(t.block(p).await?, "base:furnace[facing=east,lit=true]".to_string(), "/setblock with properties");
    t.check_eq(t.op(&format!("setprop {} lit false", at(p))).await?, "1".to_string(), "set_prop");
    t.check_eq(t.block(p).await?, "base:furnace[facing=east,lit=false]".to_string(), "put out, still facing east");
    t.check_eq(t.op(&format!("setprop {} lit maybe", at(p))).await?, "0".to_string(), "an unknown value is refused");
    Ok(())
}


/// One naming rule for every kind of thing: a name without a namespace is the declaring mod's,
/// another mod's is written in full, and both resolve the same way when read.
pub async fn names(t: T) -> R {
    use loom_sdk::*;
    static PROBE: Key = Key::named("probe");
    static OWN_EVENT: Event<u32> = Event::named("marks");
    static COAL: Item = Item::named("base:coal");
    static NOWHERE: Block = Block::named("no_such_block");
    t.flat().await?;
    t.check_eq(PROBE.name().to_string(), "testkit:probe".into(), "a key's relative name");
    t.check_eq(OWN_EVENT.name().to_string(), "testkit:marks".into(), "an event's relative name");
    t.check_eq(screens::PAUSE.name().to_string(), "core:pause".into(), "an engine screen");
    t.check_eq(question::BREAK_BLOCK.name().to_string(), "core:break_block".into(), "an engine question");
    t.check_eq(Some(COAL.id()), Item::lookup("base:coal"), "another mod's item by handle and by typed name");
    t.check_eq(Item::generated("base:coal".into()).id(), COAL.id(), "a generated handle");
    t.check_eq(NOWHERE.id(), u32::MAX, "an unknown block");
    Ok(())
}

/// Block properties are components: testkit's `r.set` gives base's mossy cobblestone blue light,
/// and the engine's lighting uses it.
pub async fn block_properties(t: T) -> R {
    t.flat().await?;
    let p = add(t.feet(), [3, 0, 0]);
    t.set(p, "mossy_cobblestone").await?;
    t.frames(5).await?;
    let l: u32 = t.op(&format!("light {}", at(add(p, [0, 1, 0])))).await?.parse().map_err(|e| format!("light: {e}"))?;
    t.check_eq([(l >> 8) & 255, (l >> 16) & 255, l >> 24], [0, 0, 8], "block light above the overridden block (r, g, b)");
    Ok(())
}

/// A mod's client tick for pigs runs every frame, gets a last call when a death ends, and keeps
/// the engine's cosmetics (the smoke) through `engine_effects`.
pub async fn client_entity_tick(t: T) -> R {
    use std::sync::atomic::Ordering::Relaxed;
    t.flat().await?;
    let p = add(t.feet(), [3, 0, 0]);
    let before = crate::PIG_TICKS.load(Relaxed);
    t.op(&format!("spawn pig {}", at(p))).await?;
    t.until("the pig's client tick", Duration::from_secs(3), |_| crate::PIG_TICKS.load(Relaxed) > before + 10).await?;
    let deaths = crate::PIG_DEATHS.load(Relaxed);
    t.cmd("/kill @e[type=base:pig]").await?;
    t.until("the last call when the death ends", Duration::from_secs(5), |_| crate::PIG_DEATHS.load(Relaxed) > deaths).await?;
    let smoke = t.state().particles;
    t.check(smoke >= 10, format!("the engine's death smoke through engine_effects: {smoke} particles"));
    Ok(())
}

/// Typed channels: a `value!` struct and a string go to the server and the answers come back
/// typed, no bytes handled by hand.
pub async fn typed_channels(t: T) -> R {
    t.flat().await?;
    t.c().send(&crate::server::ECHO, &crate::server::Echo { n: 21 });
    t.until("the doubled echo", Duration::from_secs(5), |_| crate::ECHOED.load(std::sync::atomic::Ordering::Relaxed) == 42).await?;
    t.c().send(&crate::server::GREET, &"Steve".to_string());
    t.until("the greeting", Duration::from_secs(5), |_| *crate::GREETED.lock().unwrap() == "hello, Steve").await?;
    Ok(())
}

/// A key declared with `on_press_server` reaches the server by itself, and not while the chat
/// has the input.
pub async fn key_to_server(t: T) -> R {
    t.flat().await?;
    let before: u32 = t.op("pings").await?.parse().unwrap_or(0);
    t.c().key("F10", 0.2);
    let mut now = before;
    for _ in 0..50 {
        now = t.op("pings").await?.parse().unwrap_or(0);
        if now > before {
            break;
        }
        t.frames(2).await?;
    }
    t.check_eq(now, before + 1, "presses the server got");
    t.press("core:chat");
    t.screen("core:chat").await?;
    t.c().key("F10", 0.2);
    t.secs(0.5).await?;
    t.check_eq(t.op("pings").await?.parse().unwrap_or(0), now, "presses while typing in the chat");
    t.c().key("Escape", 0.0);
    t.no_screen().await
}
