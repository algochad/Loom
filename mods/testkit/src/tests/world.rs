use std::time::Duration;

use loom_sdk::abi::entry_kind as ek;
use loom_sdk::terrain::Column;
use loom_sdk::*;

use super::{add, at};
use super::probe;
use crate::runner::{R, T};

/// A player saved in a dimension that is gone (its mod removed) keeps everything and starts at
/// the spawn; the world remembers the mods it was played with.
pub async fn missing_dimension(t: T) -> R {
    t.world(7).await?;
    t.op("put me 3 diamond 5").await?;
    let flat = Dimension::lookup("base:flatlands").ok_or("no flatlands")?;
    t.cmd("/dim base:flatlands").await?;
    t.until("in the flatlands", Duration::from_secs(20), |t| t.state().dim == flat).await?;
    let name = format!("test-{}", t.name());
    t.c().leave_world();
    t.screen("core:title").await?;
    let dir = format!("{}/saves/{name}", t.c().run_dir());
    let world = std::fs::read_to_string(format!("{dir}/world.txt")).map_err(|e| e.to_string())?;
    t.check(world.contains("mod.base = ") && world.contains("mod.testkit = "), "world.txt lists the mods");
    std::fs::write(format!("{dir}/world.txt"), world + "mod.ghost = 1.0\n").map_err(|e| e.to_string())?;
    for f in std::fs::read_dir(format!("{dir}/players")).map_err(|e| e.to_string())?.flatten() {
        let src = std::fs::read_to_string(f.path()).map_err(|e| e.to_string())?;
        std::fs::write(f.path(), src.replace("dim = base:flatlands", "dim = ghost:gone")).map_err(|e| e.to_string())?;
    }
    // Opened from the list like a player would: the missing mod is named first.
    t.c().open_screen(&screens::WORLDS);
    t.screen("core:worlds").await?;
    t.frames(2).await?;
    // A click lasts 50 ms; buttons act on release.
    t.click_ui(&format!("text:{name} |"), 0);
    t.secs(0.2).await?;
    t.click_ui("id:play", 0);
    t.screen("core:missing_mods").await?;
    t.frames(2).await?;
    t.shot("missing_mods").await?;
    t.click_ui("id:continue", 0);
    t.until("joined again", Duration::from_secs(60), |t| t.flag(client::client_flags::JOINED)).await?;
    t.loaded().await?;
    t.check_eq(t.state().dim, Dimension::lookup("base:overworld").unwrap_or(u32::MAX), "the player is in the spawn dimension");
    t.check_eq(t.slot("me", 3).await?, ("base:diamond".into(), 5, 0), "and kept the inventory");
    Ok(())
}

/// Menus open on virtual inventories and on far away blocks; a menu for a block without an
/// entity is refused with a reason instead of opening something broken.
pub async fn virtual_menus(t: T) -> R {
    t.flat().await?;
    let id = t.op("vmenu base:chest 27 diamond 3 stick 10").await?;
    t.screen("core:menu").await?;
    let diamond = Item::lookup("base:diamond").ok_or("no item base:diamond")?;
    t.until("chest contents on the client", Duration::from_secs(5), |t| t.c().menu_slot(0).item == diamond).await?;
    t.check_eq(t.c().menu_slot(1).count, 10, "second virtual slot");
    t.shot("virtual_chest").await?;
    t.c().close_screen();
    t.no_screen().await?;
    t.op(&format!("remove {id}")).await?;

    let f = t.feet();
    let plain = add(f, [2, -1, 0]);
    match t.op(&format!("menu base:furnace {} {} {}", plain[0], plain[1], plain[2])).await {
        Ok(_) => t.fail("a furnace menu opened on grass"),
        Err(e) => {
            t.check(e.contains("no block entity"), format!("unexpected refusal: {e}"));
        }
    }
    t.frames(5).await?;
    t.check(t.state().screen.len == 0, "a screen opened for the refused menu");

    // Out of reach and one after another.
    let machines = [("base:furnace", "base:furnace", [20, 0, 0]), ("base:chest", "base:chest", [-20, 0, 5]), ("testmod:generator", "testmod:generator", [0, 0, 24])];
    for (block, menu, d) in machines {
        let p = add(f, d);
        t.set(p, block).await?;
        t.op(&format!("menu {menu} {} {} {}", p[0], p[1], p[2])).await?;
        let want = Menu::lookup(menu).unwrap_or(u32::MAX);
        t.until(&format!("{menu} open"), Duration::from_secs(5), |t| t.c().menu().is_some_and(|m| m[0] == want)).await?;
        t.screen("core:menu").await?;
    }
    t.c().close_screen();
    t.no_screen().await
}

/// Fresh world load time and idle frame time on this machine (thresholds are generous: they
/// catch large regressions, the numbers themselves go to the report).
pub async fn load_benchmark(t: T) -> R {
    let start = std::time::Instant::now();
    t.world(5).await?;
    t.idle().await?;
    let s = t.state();
    t.note(format!("idle after {:.2} s with {} cubes (generated {}, {:.0} us each)", start.elapsed().as_secs_f32(), s.cubes, s.generated, s.gen_us));
    let b = t.bench(4.0, "idle").await?;
    t.check(b[2] < 50.0, format!("p99 frame {:.1} ms", b[2]));
    t.check(b[4] < 25.0, format!("server tick {:.1} ms", b[4]));
    Ok(())
}

/// Craters of growing radius through the mod API as it is (`w.set` per block), each on fresh
/// ground: the loop in the mod, the command round trip (the settling after it included), then
/// frames and server ticks while clients remesh.
pub async fn crater(t: T) -> R {
    t.world(5).await?;
    t.idle().await?;
    t.op("mode spectator").await?;
    for (i, r) in [16, 32, 48, 64].into_iter().enumerate() {
        let x = 4000 * (i as i32 + 1);
        let h = OVERWORLD.column(5, x, 0).height;
        t.cmd(&format!("/tp {x} {h} 0")).await?;
        t.secs(0.5).await?;
        t.loaded().await?;
        t.idle().await?;
        let start = std::time::Instant::now();
        let out = t.op(&format!("crater {r}")).await?;
        let round = start.elapsed().as_secs_f64() * 1000.0;
        let v: Vec<&str> = out.split_whitespace().collect();
        t.note(format!("r {r}: {} set, {} already air, {} not loaded; loop {} ms, command round trip {round:.0} ms", v[0], v[1], v[2], v[3]));
        t.bench(3.0, &format!("after crater {r}")).await?;
    }
    // The area edit: one call, the engine's own log line says where the time went
    // (`area fill r ...`); here the round trip, what is left, and the frames after.
    for (i, r) in [64, 128, 250, 500, 1000].into_iter().enumerate() {
        let x = 4000 * (i as i32 + 5);
        let h = OVERWORLD.column(5, x, 0).height;
        t.cmd(&format!("/tp {x} {h} 0")).await?;
        t.secs(0.5).await?;
        t.loaded().await?;
        t.idle().await?;
        let f = t.feet();
        let start = std::time::Instant::now();
        t.op(&format!("crater_fast {r}")).await?;
        // The fill runs at the end of the tick the command came in; the next answer comes after.
        t.op("look").await?;
        t.note(format!("fast r {r}: done {:.0} ms after the command", start.elapsed().as_secs_f64() * 1000.0));
        // Deeper than that is beyond what is loaded below the player.
        if r <= 64 {
            let below = add(f, [0, -(r - 2), 0]);
            t.check_eq(t.op(&format!("get {} {} {}", below[0], below[1], below[2])).await?, "core:air".to_string(), &format!("r {r}: hollow near the bottom"));
            let out = add(f, [0, -(r + 2), 0]);
            t.check(t.op(&format!("get {} {} {}", out[0], out[1], out[2])).await? != "core:air", format!("r {r}: solid below the crater"));
            let light: u32 = t.op(&format!("light {} {} {}", below[0], below[1], below[2])).await?.parse().unwrap_or(0);
            t.check_eq(light & 255, 15, &format!("r {r}: full sky light near the bottom, the crater relit"));
        }
        // From beyond the rim, looking at the middle: the far part was never loaded when it
        // went off and gets the crater as it loads.
        let rim = f[2] + r + 40;
        let top = OVERWORLD.column(5, f[0], rim).height.max(f[1]);
        t.cmd(&format!("/tp {} {} {}", f[0], top + 40, rim)).await?;
        t.c().look(0.0, -25.0);
        t.secs(1.0).await?;
        t.loaded().await?;
        t.secs(1.0).await?;
        t.shot(&format!("crater_{r}")).await?;
        t.bench(3.0, &format!("after fast crater {r}")).await?;
    }
    Ok(())
}

/// Testmod's dynamite: lit with a right click, it burns down its fuse and leaves a crater of
/// the set radius, dug by the engine's area edit in one go.
pub async fn dynamite(t: T) -> R {
    static RADIUS: Config<i32> = Config::named("testmod:dynamite_radius");
    t.flat().await?;
    t.op("mode creative").await?;
    let before = RADIUS.get();
    RADIUS.set(24);
    // Into whatever block the player looks at, whatever the ground does there.
    t.c().look(0.0, -20.0);
    t.secs(0.4).await?;
    let aim = t.op("look").await?;
    let v: Vec<i32> = aim.split_whitespace().skip(1).take(3).filter_map(|v| v.parse().ok()).collect();
    let [x, y, z] = v[..] else { return Err(format!("looking at nothing: {aim}")) };
    let d = [x, y, z];
    t.set(d, "testmod:dynamite").await?;
    t.secs(0.2).await?;
    t.press("core:use");
    t.secs(0.5).await?;
    let lit = t.block(d).await?;
    t.check(lit.starts_with("testmod:dynamite[fuse="), format!("lit dynamite is {lit}"));
    t.check(lit != "testmod:dynamite[fuse=0]", "the click lit the fuse");
    // Out of the blast, above it, looking down into where it will be.
    t.cmd(&format!("/tp {} {} {}", d[0], d[1] + 30, d[2] + 40)).await?;
    t.c().look(0.0, -35.0);
    t.secs(6.0).await?;
    RADIUS.set(before);
    t.check_eq(t.block(d).await?, "core:air".to_string(), "the dynamite went off");
    for p in [add(d, [0, -20, 0]), add(d, [20, 0, 0]), add(d, [0, 0, -20]), add(d, [10, -10, 10])] {
        t.check_eq(t.block(p).await?, "core:air".to_string(), &format!("{p:?} is in the crater"));
    }
    t.shot("crater").await?;
    Ok(())
}

/// A crater bigger than what is loaded survives closing the world: the part never loaded gets
/// it when visited after reopening, and what was built in the crater since stays.
pub async fn crater_saved(t: T) -> R {
    t.world(8).await?;
    t.idle().await?;
    t.op("mode creative").await?;
    let f = t.feet();
    t.op("crater_fast 600").await?;
    t.op("look").await?;
    let built = add(f, [3, -20, 0]);
    t.set(built, "gold_block").await?;
    let name = format!("test-{}", t.name());
    t.c().leave_world();
    t.screen("core:title").await?;
    t.c().open_world(&name, 8);
    t.until("joined again", Duration::from_secs(60), |t| t.flag(client::client_flags::JOINED)).await?;
    t.loaded().await?;
    t.check_eq(t.block(built).await?, "base:gold_block".to_string(), "a block built in the crater stays after reopening");
    let far = [f[0] + 450, f[1] - 100, f[2]];
    t.cmd(&format!("/tp {} {} {}", far[0], far[1], far[2])).await?;
    t.secs(1.0).await?;
    t.loaded().await?;
    for p in [far, add(far, [0, -40, 0]), add(far, [0, 40, 30])] {
        t.check_eq(t.block(p).await?, "core:air".to_string(), &format!("{p:?}, first loaded after reopening, is in the crater"));
    }
    Ok(())
}

/// The view reaches as far down as sideways: standing beyond a deep crater's rim, its bottom
/// stays loaded and lit (it used to reach six cubes down plus the air under the player, so the
/// bottom unloaded the moment the player stepped off the rim).
pub async fn crater_view(t: T) -> R {
    t.world(5).await?;
    t.idle().await?;
    t.op("mode creative").await?;
    let f = t.feet();
    t.op("crater_fast 150").await?;
    t.op("look").await?;
    // The wall's cubes, cut through, reach the client at once with the rest (they used to wait
    // for their light to be redone, a share per tick: pillars of old blocks for seconds).
    t.secs(0.2).await?;
    t.sync_blocks(add(f, [144, -6, -2]), add(f, [152, -1, 2])).await?;
    let rim = [f[0], OVERWORLD.column(5, f[0], f[2] + 170).height.max(f[1]) + 1, f[2] + 170];
    t.cmd(&format!("/tp {} {} {}", rim[0], rim[1], rim[2])).await?;
    t.secs(1.0).await?;
    t.loaded().await?;
    t.idle().await?;
    // Beyond what the server simulates: the client made the crater on its own terrain and lit it.
    let bottom = add(f, [0, -140, 0]);
    let light = t.c().light(bottom).unwrap_or(0);
    t.check_eq(light & 255, 15, "the crater's bottom, seen from beyond the rim, is loaded and lit");
    t.check_eq(t.c().block(bottom), Some(0), "the crater's bottom is air on the client");
    t.c().look(0.0, -30.0);
    t.frames(20).await?;
    t.shot("from_rim").await?;
    Ok(())
}

/// An edited cube in the sky column over the view (generated so sky light comes down right,
/// never drawn) is announced like any other: the client keeps the edit, not generation. It
/// used to be announced only up to the view's top.
pub async fn sky_column_edit(t: T) -> R {
    loom_sdk::settings::VIEW_DISTANCE.set(6);
    let r = sky_column(&t).await;
    loom_sdk::settings::VIEW_DISTANCE.set(12);
    r
}

async fn sky_column(t: &T) -> R {
    t.world(5).await?;
    t.idle().await?;
    let f = t.feet();
    // Two cubes over the view's top (6 over the player's cube), under open sky (cube 16).
    let cube = (f[1] >> 4) + 8;
    if cube >= 16 {
        t.fail(format!("the player stands too high ({}) for a sky column", f[1]));
        return Ok(());
    }
    let p = [f[0], cube * 16 + 5, f[2]];
    t.set(p, "gold_block").await?;
    t.until("the client has the block", Duration::from_secs(5), |t| t.c().block(p).is_some_and(|b| b != 0)).await?;
    Ok(())
}

use base_api::OVERWORLD;

/// Testmod edits base's overworld from outside: a biome of its own tried before base's desert,
/// with its own surface and boulders, and a rock needle from a feature written in testmod.
pub async fn foreign_terrain(t: T) -> R {
    t.world(5).await?;
    t.op("mode spectator").await?;
    let seed = 5;
    // testmod's `NEEDLE`.
    let needle = OVERWORLD.column(seed, 1000, 1000);
    let beside = OVERWORLD.column(seed, 1010, 1000);
    t.note(format!("needle {} beside {}", needle.height, beside.height));
    t.check(needle.height - beside.height > 60, "the needle rises from the land around it");
    t.cmd(&format!("/tp 1000 {} 1000", needle.height + 3)).await?;
    t.secs(0.5).await?;
    t.loaded().await?;
    let top = t.op(&format!("get 1000 {} 1000", needle.height)).await?;
    t.check(top != "core:air", format!("the needle is generated where the terrain says ({top})"));

    let grid: Vec<(i32, i32, Column)> = (-80..80).flat_map(|i| (-80..80).map(move |j| (i * 64, j * 64))).map(|(x, z)| (x, z, OVERWORLD.column(seed, x, z))).collect();
    let count = |name: &str| grid.iter().filter(|g| name_of(ek::BIOME, g.2.biome) == name).count();
    t.note(format!("of {} columns: desert {}, scorched {}", grid.len(), count("base:desert"), count("testmod:scorched")));
    let scorched = grid
        .into_iter()
        .find(|&(x, z, c)| name_of(ek::BIOME, c.biome) == "testmod:scorched" && !OVERWORLD.surface_carved(seed, x, z));
    let (x, z, c) = scorched.ok_or("no scorched biome in base's overworld")?;
    t.note(format!("scorched at {x} {z}, height {}", c.height));
    t.cmd(&format!("/tp {x} {} {z}", c.height + 3)).await?;
    t.secs(0.5).await?;
    t.loaded().await?;
    let ground = t.op(&format!("get {x} {} {z}", c.height)).await?;
    t.check(ground == "testmod:regolith" || ground == "testmod:basalt", format!("the scorched biome's surface ({ground})"));
    let census = t.op("census 24 testmod:basalt,testmod:regolith").await?;
    t.note(census.clone());
    let n: Vec<u32> = census.split_whitespace().filter_map(|e| e.rsplit_once('=')?.1.parse().ok()).collect();
    t.check(n.first().is_some_and(|&b| b > 0), "boulders from testmod's decoration");
    t.check(n.get(1).is_some_and(|&r| r > 100), "the biome's regolith");
    Ok(())
}

/// `/tick`: a frozen world keeps its time, steps exactly, and a sprint runs a day at once.
pub async fn tick_control(t: T) -> R {
    t.flat().await?;
    let time = async || -> Result<u64, String> {
        let r = t.cmd("/time query").await?;
        r.rsplit("tick ").next().and_then(|v| v.trim_end_matches(')').parse().ok()).ok_or(format!("time: {r}"))
    };
    t.cmd("/tick freeze").await?;
    let a = time().await?;
    t.secs(0.5).await?;
    t.check_eq(time().await?, a, "time while frozen");
    t.cmd("/tick step 10").await?;
    t.secs(1.0).await?;
    t.check_eq(time().await?, a + 10, "time after ten steps");
    t.cmd("/tick unfreeze").await?;
    let b = time().await?;
    t.cmd("/tick sprint 24000").await?;
    t.until("sprint done", Duration::from_secs(60), |t| (0..20).any(|i| t.c().chat_line(i).contains("sprinted"))).await?;
    t.check(time().await? >= b + 24000, "a day went by in the sprint");
    Ok(())
}

/// Frame time with thousands of dropped items in view (item farms).
pub async fn item_farm(t: T) -> R {
    t.flat().await?;
    t.op("drops 3000").await?;
    t.c().look(0.0, -30.0);
    t.secs(2.0).await?;
    t.note(format!("{} entities on the client", t.c().entities().len()));
    t.shot("farm").await?;
    let b = t.bench(4.0, "item_farm").await?;
    t.note(format!("fps {:.0}, frame {:.2} ms, p99 {:.2} ms (in tests the hidden window's presents are throttled: compare the log's cpu record ms)", b[0], b[1], b[2]));
    Ok(())
}

/// Plants grow by world time: wheat ripens when the clock jumps ahead, and sugar cane in an area
/// nobody was in catches up when the player comes back.
pub async fn growth(t: T) -> R {
    t.flat().await?;
    t.op("time 1000").await?;
    let f = t.feet();
    let farm = add(f, [2, -1, 0]);
    t.set(farm, "farmland").await?;
    t.set(add(farm, [0, 1, 0]), "wheat").await?;
    // Sugar cane needs water beside the block it stands on.
    let cane = add(f, [-2, 0, 0]);
    t.set(cane, "sugar_cane").await?;
    t.secs(0.2).await?;
    t.check_eq(t.block(cane).await?, "core:air".to_string(), "sugar cane without water breaks off");
    t.set(add(cane, [0, -1, 1]), "water").await?;
    t.set(cane, "sugar_cane").await?;
    t.op("time 200000").await?;
    t.secs(0.5).await?;
    t.check_eq(t.block(add(farm, [0, 1, 0])).await?, "base:wheat[age=7]".to_string(), "wheat after a jump of 2.5 hours");
    t.check_eq(t.block(add(cane, [0, 2, 0])).await?, "base:sugar_cane".to_string(), "sugar cane after the jump");
    // Far away the cane's area unloads; time passes; coming back, it has grown.
    let cane2 = add(f, [-4, 0, 0]);
    t.set(add(cane2, [0, -1, -1]), "water").await?;
    t.set(cane2, "sugar_cane").await?;
    t.cmd("/tp 3000 10 3000").await?;
    t.secs(3.0).await?;
    t.check_eq(t.block(cane2).await?, "core:air".to_string(), "far away, the cane's area is not here");
    t.op("time 400000").await?;
    t.cmd(&format!("/tp {} {} {}", f[0], f[1], f[2])).await?;
    t.secs(1.0).await?;
    t.loaded().await?;
    t.secs(0.5).await?;
    t.check_eq(t.block(add(cane2, [0, 2, 0])).await?, "base:sugar_cane".to_string(), "sugar cane grown while unloaded");
    Ok(())
}

/// Time the world skips is made up, without random ticks: grass takes bare dirt and dies under a
/// block, a furnace smelts what its fuel allows and a generator runs an electric furnace through
/// a cable, all after a jump of the time and, for an area that was unloaded, when it comes back.
pub async fn catch_up(t: T) -> R {
    t.flat().await?;
    t.op("time 1000").await?;
    let f = t.feet();
    let dirt = add(f, [3, -1, 0]);
    t.set(dirt, "dirt").await?;
    let covered = add(f, [3, -1, 3]);
    t.set(covered, "grass").await?;
    t.set(add(covered, [0, 1, 0]), "stone").await?;
    let furnace = add(f, [-3, 0, 0]);
    t.set(furnace, "furnace").await?;
    t.op(&format!("insert {} raw_iron 10", at(furnace))).await?;
    t.op(&format!("insert {} coal 1", at(furnace))).await?;
    let g = add(f, [-3, 0, 3]);
    t.set(g, "testmod:generator").await?;
    t.set(add(g, [-1, 0, 0]), "testmod:cable").await?;
    let e = add(g, [-2, 0, 0]);
    t.set(e, "testmod:electric_furnace").await?;
    t.op(&format!("insert {} coal 4", at(g))).await?;
    t.op(&format!("insert {} iron_ore 3", at(e))).await?;
    t.secs(0.3).await?;
    t.check_eq(t.block(dirt).await?, "base:dirt".to_string(), "bare dirt is still dirt");
    t.op("time 200000").await?;
    // Made up at once, in the tick after the jump.
    t.secs(1.0).await?;
    t.check_eq(t.block(dirt).await?, "base:grass".to_string(), "grass took the bare dirt");
    t.check_eq(t.block(covered).await?, "base:dirt".to_string(), "grass under stone died");
    // One coal burns 1600 ticks: eight of the ten ores, no more.
    t.check_eq(t.slot(&at(furnace), 2).await?, ("base:iron_ingot".into(), 8, 0), "furnace smelted what its coal allows");
    t.check_eq(t.slot(&at(furnace), 0).await?, ("base:raw_iron".into(), 2, 0), "and left the rest");
    t.check_eq(t.slot(&at(e), 1).await?, ("base:iron_ingot".into(), 3, 0), "the generator powered the electric furnace");
    // Unloaded the whole time: done when the area comes back.
    let far = add(f, [-6, 0, 0]);
    t.set(far, "furnace").await?;
    t.op(&format!("insert {} raw_iron 4", at(far))).await?;
    t.op(&format!("insert {} coal 1", at(far))).await?;
    t.secs(0.2).await?;
    t.cmd("/tp 3000 10 3000").await?;
    t.secs(3.0).await?;
    t.op("time 400000").await?;
    t.cmd(&format!("/tp {} {} {}", f[0], f[1], f[2])).await?;
    t.secs(1.0).await?;
    t.loaded().await?;
    t.secs(0.5).await?;
    t.check_eq(t.slot(&at(far), 2).await?, ("base:iron_ingot".into(), 4, 0), "a furnace caught up while unloaded");
    Ok(())
}

/// What a player carries over leaving and opening a world again: placed blocks, the inventory,
/// a moving structure with a block added, permanent attribute modifiers; server-only stack data
/// is not saved, and expired modifiers are gone before.
pub async fn persistence(t: T) -> R {
    t.world(7).await?;
    let p = add(t.feet(), [2, 0, 2]);
    t.set(p, "gold_block").await?;
    t.op("put me 3 diamond 5").await?;
    // A moving structure with a block added, in a cube nothing else changed.
    let skiff = t.op(&format!("spawn testmod:skiff {} {} {}", p[0], p[1] + 6, p[2] + 8)).await?;
    let skiff = skiff.trim().trim_start_matches('#').to_string();
    t.op(&format!("sblock {skiff} 0 1 0 base:glass")).await?;
    t.op("put me 0 iron_sword 1").await?;
    // A machine's contents are kept, and its entity can't be made without the block.
    let chest = add(p, [-3, 0, 0]);
    t.set(chest, "chest").await?;
    t.op(&format!("insert {} {} {} coal 5", chest[0], chest[1], chest[2])).await?;
    t.check_eq(t.op(&format!("spawn base:chest {} {} {}", p[0], p[1] + 2, p[2])).await?, "0".to_string(), "no chest entity without a chest");
    let said = t.cmd("/summon base:furnace").await?;
    t.check(said.contains("belongs to a block"), format!("/summon refuses a furnace: {said}"));
    t.op("secret me 0 1234").await?;
    t.check_eq(t.op("secret me 0").await?, "1234".into(), "server keeps the secret");
    t.frames(5).await?;
    t.check_eq(crate::server::SECRET.stack_only(&t.c().inventory_slot(0)), None, "client never sees the secret");
    t.check_eq(value(&t.op("attr me core:movement_speed").await?), 1.0, "speed without modifiers");
    t.op("attr_mod me core:movement_speed testkit:haste 1 0.5 40").await?;
    t.op("attr_mod me core:max_health testkit:vigor 0 10 0").await?;
    let speed = t.op("attr me core:movement_speed").await?;
    t.check_eq(value(&speed), 1.5, "speed with +50% of base");
    t.check(speed.contains("testkit:haste"), format!("explained: {speed}"));
    t.check_eq(value(&t.op("attr me core:max_health").await?), 30.0, "max health +10");
    t.secs(2.5).await?;
    t.check_eq(value(&t.op("attr me core:movement_speed").await?), 1.0, "the 40 tick modifier ran out");
    // A timer due after the reload is saved with the player and fires there.
    let rings: u32 = t.op("rings").await?.parse().unwrap_or(0);
    t.op("after 120").await?;
    // A world's setting (a game rule) goes with the world.
    t.cmd("/rule testkit:count_marks 1").await?;

    let name = format!("test-{}", t.name());
    t.c().leave_world();
    t.screen("core:title").await?;
    t.c().open_world(&name, 7);
    t.until("joined again", Duration::from_secs(60), |t| t.flag(client::client_flags::JOINED)).await?;
    t.loaded().await?;
    t.check_eq(t.block(p).await?, "base:gold_block".into(), "placed block after reload");
    t.check_eq(t.slot("me", 3).await?, ("base:diamond".into(), 5, 0), "inventory after reload");
    t.check_eq(t.op(&format!("sblock {skiff} 0 1 0")).await?, "base:glass".to_string(), "the structure and its added block after reload");
    t.check_eq(t.slot("me", 0).await?.0, "base:iron_sword".into(), "the sword was saved");
    t.check_eq(t.op(&format!("inv {} {} {}", chest[0], chest[1], chest[2])).await?, "0:base:coal:5".to_string(), "the chest kept its coal");
    t.check_eq(t.op("secret me 0").await?, "none".into(), "the secret was not");
    t.check_eq(value(&t.op("attr me core:max_health").await?), 30.0, "max health modifier after reload");
    let mut now = rings;
    for _ in 0..40 {
        now = t.op("rings").await?.parse().unwrap_or(0);
        if now > rings {
            break;
        }
        t.secs(0.25).await?;
    }
    t.check_eq(now, rings + 1, "the timer saved with the player fired after the reload");
    let rule = t.cmd("/rule testkit:count_marks").await?;
    t.check(rule.contains("true"), format!("the world's setting after reload: {rule}"));
    t.cmd("/rule testkit:count_marks 0").await?;
    // One maximum everywhere: /heal fills to it and the client hears the same.
    t.cmd("/heal").await?;
    t.check_eq(t.op("health").await?, "30".to_string(), "healed to the modified maximum");
    t.until("the client has 30", Duration::from_secs(2), |t| t.state().health == 30.0).await?;
    t.op("attr_rm me core:max_health testkit:vigor").await?;
    t.check_eq(value(&t.op("attr me core:max_health").await?), 20.0, "removed");
    t.sync_inventory().await
}

/// First number of an `attr` answer ("1.5 | ...").
fn value(s: &str) -> f64 {
    s.split('|').next().and_then(|v| v.trim().parse().ok()).unwrap_or(f64::NAN)
}

/// Dimensions: each mod's own one is reached and drawn as itself; the engine's void is empty,
/// without gravity and refuses blocks; the debug dimension shows every state and never updates.
pub async fn dimensions(t: T) -> R {
    t.world(11).await?;
    type Look = Option<fn(&crate::runner::Shot) -> (f32, bool)>;
    let looks: [(&str, Look); 4] = [
        (
            "base:flatlands",
            Some(|s| {
                let v = s.share(0.0, 0.6, 1.0, 0.9, probe::grass);
                (v, v > 0.6)
            }),
        ),
        (
            "testmod:mars",
            Some(|s| {
                let v = s.share(0.0, 0.0, 1.0, 0.9, |c| c[0] as i32 > c[1] as i32 + 40 && c[0] as i32 > c[2] as i32 + 60);
                (v, v > 0.6)
            }),
        ),
        (
            "testmod:void",
            Some(|s| {
                let v = s.share(0.0, 0.0, 1.0, 0.25, probe::dark);
                (v, v > 0.9)
            }),
        ),
        // Generated terrain: reaching it is the check, the picture is for looking at.
        ("base:overworld", None),
    ];
    for (dim, look) in looks {
        let Some(id) = Dimension::lookup(dim) else {
            t.fail(format!("no dimension {dim}"));
            continue;
        };
        t.cmd(&format!("/dim {dim}")).await?;
        t.until(&format!("in {dim}"), Duration::from_secs(20), |t| t.state().dim == id).await?;
        t.loaded().await?;
        t.idle().await?;
        t.settled().await?;
        t.c().look(30.0, -20.0);
        t.frames(3).await?;
        let shot = t.shot(dim.split(':').nth(1).unwrap_or(dim)).await?;
        if let Some(look) = look {
            let (v, ok) = look(&shot);
            t.probe(&format!("{dim} drawn as itself"), v, ok);
        }
        if dim == "testmod:void" {
            // testmod's island generator places a chest (`ISLAND_CHEST`) without its entity.
            t.check_eq(t.op("insert 4 64 0 coal 3").await, Ok("3".to_string()), "a chest placed by a generator has its block entity");
        }
    }

    let void = Dimension::lookup("core:void").ok_or("no core:void")?;
    t.cmd("/dim core:void").await?;
    t.until("in the void", Duration::from_secs(20), |t| t.state().dim == void).await?;
    t.loaded().await?;
    let said = (0..4).any(|i| t.c().chat_line(i).contains("nothing here can be changed"));
    t.check(said, "chat says where the player is");
    let p = add(t.feet(), [0, -1, 0]);
    t.check(t.set(p, "stone").await.is_err(), "no block can be set in the void");
    t.check(t.block(p).await? != "base:stone", "the void stays empty");
    let y = t.state().pos[1];
    t.secs(1.0).await?;
    t.check((t.state().pos[1] - y).abs() < 0.01, format!("no gravity in the void (y {y:.2} -> {:.2})", t.state().pos[1]));

    t.cmd("/dim core:debug_blocks").await?;
    t.secs(0.5).await?;
    t.loaded().await?;
    let at = t.op("find 40 base:water").await?;
    let w: Vec<i32> = at.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    let [x, y, z] = w[..] else { return Err(format!("no water among the debug blocks: {at}")) };
    t.secs(1.0).await?;
    t.check_eq(t.block([x + 1, y, z]).await?, "core:air".to_string(), "debug water does not flow");
    t.cmd(&format!("/setblock {} {y} {z} base:stone", x + 1)).await?;
    t.check_eq(t.block([x + 1, y, z]).await?, "core:air".to_string(), "debug blocks refuse /setblock");
    // Breaking is refused like placing, and nothing holds the grid up.
    let at = t.op("find 40 base:stone").await?;
    let s: Vec<i32> = at.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    let [sx, sy, sz] = s[..] else { return Err(format!("no stone among the debug blocks: {at}")) };
    // Refused, not just undone: a refused break sends the block back to the player's client.
    let broke = t.op(&format!("break {sx} {sy} {sz}")).await?;
    t.check_eq(broke, "0".to_string(), "breaking a debug block is refused");
    t.check_eq(t.block([sx, sy, sz]).await?, "base:stone".to_string(), "debug blocks refuse breaking");
    t.check_eq(t.block([sx, sy - 1, sz]).await?, "core:air".to_string(), "no floor under the debug blocks");
    t.cmd("/rule core:mob_spawning 1").await?;
    t.secs(4.5).await?;
    let mobs = t.op(&format!("near {sx} {sy} {sz} 64")).await?;
    t.cmd("/rule core:mob_spawning 0").await?;
    // Block entities (furnaces, chests) are there; mobs must not be.
    let spawned: Vec<&str> = mobs.split_whitespace().filter(|e| ["pig", "cow", "zombie", "spider"].iter().any(|m| e.ends_with(&format!("base:{m}")))).collect();
    t.check(spawned.is_empty(), format!("nothing spawns among the debug blocks: {spawned:?}"));
    // Lava is drawn solid: blocks behind ice do not show through it.
    let at = t.op("find 60 base:lava").await?;
    let l: Vec<i32> = at.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    if let [lx, ly, lz] = l[..] {
        t.cmd(&format!("/tp {} {} {}", lx as f64 + 0.5, ly + 2, lz as f64 + 4.5)).await?;
        t.c().look(0.0, -30.0);
        t.frames(20).await?;
        t.shot("lava").await?;
    }
    t.cmd("/tp -3 1 -3").await?;
    t.c().look(225.0, -25.0);
    t.frames(20).await?;
    let grid = t.shot("debug_blocks").await?;
    // Many different blocks in view: no color class covers most of the grid (it floats in the
    // sky, so only the band it fills counts).
    let biggest = [probe::grass, probe::dark, probe::light, probe::brown].map(|f| grid.share(0.3, 0.22, 0.7, 0.36, f)).into_iter().fold(0.0f32, f32::max);
    t.probe("debug blocks: largest share of one color class", biggest, biggest < 0.6);
    Ok(())
}

/// World generation from data, sampled with seed 5: deep caves (an underground biome) with the
/// test mod's glow crystals only there, birches in birch forests and oaks on the plains, and
/// structures where the engine says they are.
pub async fn worldgen(t: T) -> R {
    t.world(5).await?;
    t.op("mode spectator").await?;
    let (mut caves, mut inside, mut outside) = (0, 0, 0);
    for (i, x) in [0, 200].into_iter().enumerate() {
        t.cmd(&format!("/tp {x} -60 {}", i * 150)).await?;
        t.secs(0.5).await?;
        t.loaded().await?;
        let survey = t.op("survey 12").await?;
        let (biomes, crystals) = survey.split_once(" | crystals ").ok_or(format!("survey: {survey}"))?;
        caves += biomes.split_whitespace().filter_map(|e| e.strip_prefix("base:deep_caves=")).filter_map(|n| n.parse::<u32>().ok()).sum::<u32>();
        let n: Vec<u32> = crystals.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        inside += n.first().copied().unwrap_or(0);
        outside += n.get(1).copied().unwrap_or(0);
        t.note(format!("{x}: {survey}"));
    }
    t.check(caves > 0, "deep caves were found underground");
    t.check(inside > 0, format!("glow crystals grow in deep caves ({inside})"));
    t.check_eq(outside, 0, "glow crystals outside deep caves");

    // Birch forests and plains of seed 5, found from the generator's columns: the middle of
    // a patch of the biome, above ground.
    let patch = |biome: &str, skip: usize| {
        (-40..40)
            .flat_map(|i| (-40..40).map(move |j| (i * 48, j * 48)))
            .filter(|&(x, z)| [(0, 0), (24, 0), (-24, 0), (0, 24), (0, -24)].iter().all(|&(dx, dz)| name_of(ek::BIOME, OVERWORLD.column(5, x + dx, z + dz).biome) == biome))
            .nth(skip)
            .map(|(x, z)| [x, OVERWORLD.column(5, x, z).height + 6, z])
    };
    let spots = [(patch("base:birch_forest", 0), true), (patch("base:birch_forest", 3), true), (patch("base:plains", 0), false), (patch("base:plains", 3), false)];
    for (spot, forest) in spots {
        let [x, y, z] = spot.ok_or(format!("no {} patch", if forest { "birch forest" } else { "plains" }))?;
        t.cmd(&format!("/tp {x} {y} {z}")).await?;
        t.secs(0.5).await?;
        t.loaded().await?;
        let census = t.op("census 28 base:birch_log,base:oak_log,testmod:glow_log,testmod:glow_flower").await?;
        let n: Vec<u32> = census.split_whitespace().filter_map(|e| e.rsplit_once('=')?.1.parse().ok()).collect();
        t.note(format!("{x} {z}: {census}"));
        if forest {
            t.check(n[0] > 0, format!("birches in the birch forest at {x} {z}"));
        } else {
            t.check(n[1] > 0, format!("oaks on the plains at {x} {z}"));
        }
        t.check_eq(n[2] + n[3], 0, &format!("the test mod's glowing plants are not generated ({x} {z})"));
    }

    t.check(t.op("structure testmod:obelisk 3000").await? != "none", "another mod's obelisks are in the world too");
    // The same template placed by code.
    let f = t.feet();
    t.check_eq(t.op(&format!("place testmod:obelisk {} {} {}", f[0] + 4, f[1] + 10, f[2])).await?, "1".to_string(), "w.place puts a template");
    t.check_eq(t.block([f[0] + 5, f[1] + 11, f[2] + 1]).await?, "testmod:glow_crystal".to_string(), "its glowing core");

    // Testmod puts base's forest on Mars; the engine scatters its trees there like at home.
    static MARS: Dimension = Dimension::named("testmod:mars");
    let seed = 5;
    let forest = |x: i32, z: i32| name_of(ek::BIOME, MARS.column(seed, x, z).biome) == "base:forest";
    let spot = (-60..60).flat_map(|i| (-60..60).map(move |j| (i * 32, j * 32))).find(|&(x, z)| [(0, 0), (20, 0), (-20, 0), (0, 20), (0, -20)].iter().all(|&(dx, dz)| forest(x + dx, z + dz)));
    let (x, z) = spot.ok_or("no forest on Mars")?;
    let h = MARS.column(seed, x, z).height;
    t.cmd("/dim testmod:mars").await?;
    t.until("on Mars", Duration::from_secs(20), |t| t.state().dim == MARS.id()).await?;
    t.cmd(&format!("/tp {x} {} {z}", h + 3)).await?;
    t.secs(0.5).await?;
    t.loaded().await?;
    let census = t.op("census 20 base:oak_log,base:birch_log,base:grass").await?;
    t.note(format!("Mars forest at {x} {z}: {census}"));
    let n: Vec<u32> = census.split_whitespace().filter_map(|e| e.rsplit_once('=')?.1.parse().ok()).collect();
    t.check(n[0] + n[1] > 0, "trees grow in base's forest on Mars");
    t.check(n[2] > 0, "the forest brings its grass to Mars");
    Ok(())
}


/// A block placed while the whole view waits to be meshed again (as after a big crater or a
/// resource reload) shows at once: the cubes next to the player are meshed first, past the
/// far ones (it used to wait behind all of them).
pub async fn edit_while_loading(t: T) -> R {
    static VIEW: Config<i32> = Config::named("core:view_distance");
    let before = VIEW.get();
    VIEW.set(32);
    let out = edit_while_loading_at(&t).await;
    VIEW.set(before);
    out
}

async fn edit_while_loading_at(t: &T) -> R {
    t.world(5).await?;
    t.idle().await?;
    t.op("mode spectator").await?;
    t.cmd("/tp ~ ~40 ~").await?;
    t.c().look(0.0, 0.0);
    t.frames(5).await?;
    // Eye level, three blocks north.
    let f = t.feet();
    let p = add(f, [0, 1, -3]);
    let gold = |c: [u8; 3]| c[0] > 180 && c[1] > 130 && c[2] < 110;
    t.check(!gold(t.pixel(0.53, 0.53).await?), "sky in front before");
    t.c().reload_assets();
    let mut most = 0;
    t.until("everything to mesh again", Duration::from_secs(10), |t| {
        most = most.max(t.state().pending_meshes);
        most > 5000
    })
    .await
    .map_err(|e| format!("{e} (at most {most}, {} cubes)", t.state().cubes))?;
    let backlog = t.state().pending_meshes;
    let start = std::time::Instant::now();
    t.set(p, "gold_block").await?;
    let mut shown = false;
    while start.elapsed() < Duration::from_secs(5) && !shown {
        shown = gold(t.pixel(0.53, 0.53).await?);
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    t.note(format!("placed block shown after {ms:.0} ms with {backlog} meshes waiting"));
    t.check(shown, "the placed block shows");
    t.check(ms < 500.0, format!("shown after {ms:.0} ms"));
    Ok(())
}

/// Past the view, distant terrain is made and drawn: tiles come, and the horizon seen from up
/// high is land, not the sky it is with distant terrain off.
pub async fn distant_terrain(t: T) -> R {
    static VIEW: Config<i32> = Config::named("core:view_distance");
    static LOD: Config<i32> = Config::named("core:lod_distance");
    let before = (VIEW.get(), LOD.get());
    VIEW.set(8);
    LOD.set(256);
    let out = distant_terrain_at(&t).await;
    VIEW.set(before.0);
    LOD.set(before.1);
    out
}

async fn distant_terrain_at(t: &T) -> R {
    static LOD: Config<i32> = Config::named("core:lod_distance");
    t.world(5).await?;
    t.idle().await?;
    t.op("mode spectator").await?;
    t.cmd("/tp ~ ~100 ~").await?;
    t.c().look(0.0, -8.0);
    t.until("distant terrain made", Duration::from_secs(20), |t| t.state().lod_tiles > 50 && t.state().lod_pending == 0).await?;
    t.frames(5).await?;
    let tiles = t.state().lod_tiles;
    t.note(format!("{tiles} distant terrain tiles shown"));
    let with = t.grab().await?;
    LOD.set(0);
    t.frames(10).await?;
    let without = t.grab().await?;
    let changed = T::differ(&with, &without);
    t.note(format!("share of the picture that is distant terrain: {changed:.2}"));
    t.check(changed > 0.1, format!("the horizon has land on it ({changed:.2} of the picture changes without it)"));
    t.check_eq(t.state().lod_tiles, 0, "no tiles shown with it off");
    Ok(())
}

/// Terrain the GPU makes is the CPU's to the bit (integer math on both sides): every cube of a
/// fresh world is made again on the CPU and compared.
pub async fn gpu_terrain(t: T) -> R {
    static CHECK: Config<i32> = Config::named("core:terrain_check");
    let before = CHECK.get();
    CHECK.set(1);
    let start = t.state().gpu_cubes;
    let out = async {
        t.world(5).await?;
        t.idle().await?;
        let s = t.state();
        let made = s.gpu_cubes - start;
        t.note(format!("{made} cubes made on the GPU, {} differing", s.gpu_mismatches));
        t.check(made > 1000, format!("the GPU made the terrain ({made} cubes)"));
        t.check_eq(s.gpu_mismatches, 0, "cubes the GPU made differently from the CPU");
        Ok(())
    }
    .await;
    CHECK.set(before);
    out
}
