//! Scenarios that are mostly about the picture, checked against reference screenshots, plus
//! multiplayer through a second game process.

use std::time::Duration;

use loom_sdk::*;

use super::{add, probe};
use crate::runner::{R, Shot, T};

/// Share of the box around where `p` is drawn (half size `r`) for which `f` holds.
fn around(t: &T, shot: &Shot, p: [f64; 3], r: f32, f: impl FnMut([u8; 3]) -> bool) -> f32 {
    let Some((x, y)) = t.on_screen(p) else { return 0.0 };
    shot.share(x - r, y - r, x + r, y + r, f)
}

/// Average color of the box around where `p` is drawn.
fn avg_around(t: &T, shot: &Shot, p: [f64; 3], r: f32) -> [f32; 3] {
    let Some((x, y)) = t.on_screen(p) else { return [0.0; 3] };
    shot.avg(x - r, y - r, x + r, y + r)
}

/// Share of pixels that changed in a box between two grabs of the same size.
fn changed(a: &[u8], b: &[u8], w: u32, h: u32, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let (ax, ay, bx, by) = ((x0 * w as f32) as u32, (y0 * h as f32) as u32, ((x1 * w as f32) as u32).min(w), ((y1 * h as f32) as u32).min(h));
    let (mut n, mut all) = (0.0f32, 0.0f32);
    for y in ay..by {
        for x in ax..bx {
            let i = ((y * w + x) * 4) as usize;
            n += (0..3).any(|k| (a[i + k] as i32 - b[i + k] as i32).abs() > 40) as u8 as f32;
            all += 1.0;
        }
    }
    n / all.max(1.0)
}

fn item(name: &str) -> u32 {
    Item::lookup(name).unwrap_or(u32::MAX)
}

/// Items in the first person hand: each drawn in its own colors at the hand's place.
pub async fn held_items(t: T) -> R {
    t.flat().await?;
    for (i, name) in ["wood_pickaxe", "diamond_sword", "glass", "rose", "torch"].iter().enumerate() {
        t.op(&format!("put me {i} {name} 1")).await?;
    }
    t.c().look(0.0, -10.0);
    let looks: [(&str, fn([u8; 3]) -> bool, f32); 5] = [("pickaxe", probe::brown, 0.03), ("sword", probe::cyan, 0.03), ("block", probe::light, 0.03), ("flower", probe::red, 0.02), ("torch", probe::dark, 0.03)];
    for (i, (name, color, min)) in looks.into_iter().enumerate() {
        t.c().select(i as u32);
        t.until("selected", Duration::from_secs(5), |t| t.state().selected == i as u32).await?;
        // The hand lowers and comes back up on a switch.
        t.secs(0.6).await?;
        let shot = t.shot(name).await?;
        let v = shot.share(0.78, 0.55, 1.0, 0.95, color);
        t.probe(&format!("{name} colors in the hand"), v, v > min);
    }
    Ok(())
}

/// Hovering a slot shows a tooltip box next to the cursor.
pub async fn tooltips(t: T) -> R {
    t.flat().await?;
    t.op("put me 0 furnace 1").await?;
    t.op("put me 1 coal 5").await?;
    t.press("core:inventory");
    t.screen("core:menu").await?;
    t.c().cursor(0.02, 0.02);
    t.frames(4).await?;
    let plain = t.grab().await?;
    let (w, h) = (t.state().window[0], t.state().window[1]);
    for (slot, name) in [(36, "furnace"), (37, "coal")] {
        let Some([x, y, sw, sh]) = t.c().ui_rect(&format!("slot:{slot}")) else {
            t.fail(format!("no slot {slot}"));
            continue;
        };
        let (cx, cy) = (x + sw / 2.0, y + sh / 2.0);
        t.c().cursor(cx, cy);
        t.frames(4).await?;
        t.shot(name).await?;
        let now = t.grab().await?;
        // The tooltip opens to the right of the cursor. Its dark box is close to the panel's
        // color, so only part of the pixels change.
        let v = changed(&plain, &now, w, h, cx + 0.02, cy - 0.02, cx + 0.15, cy + 0.05);
        t.probe(&format!("{name} tooltip drawn"), v, v > 0.1);
        // Another mod's line under coal's own (`item.base.coal.tooltip.testmod`), in red.
        if name == "coal" {
            let red = t.shot("coal_lines").await?.share(cx + 0.02, cy - 0.05, cx + 0.3, cy + 0.12, probe::red);
            t.probe("the test mod's red line under coal's tooltip", red, red > 0.002);
            // `{block:..}` in a tooltip draws the block, not an item icon.
            let block = t.shot("coal_block_icon").await?.share(cx + 0.02, cy - 0.05, cx + 0.3, cy + 0.12, probe::cyan);
            t.probe("diamond block icon in the tooltip", block, block > 0.0008);
        }
    }
    t.c().close_screen();
    t.no_screen().await
}

/// Block light in the dark: a closed stone room is black until a glowstone and a red lamp light
/// it, the floor by the lamp taking its color.
pub async fn night_light(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    t.fill(add(f, [-5, -1, -9]), add(f, [5, 4, 2]), "stone").await?;
    t.fill(add(f, [-4, 0, -8]), add(f, [4, 3, 1]), "air").await?;
    t.fill(add(f, [-4, -1, -8]), add(f, [4, -1, 1]), "birch_planks").await?;
    t.c().look(0.0, -35.0);
    t.secs(0.5).await?;
    let dark = t.shot("dark").await?;
    let v = dark.share(0.1, 0.3, 0.9, 0.8, probe::dark);
    t.probe("the closed room is dark", v, v > 0.9);
    let (glow, lamp) = (add(f, [3, 0, -6]), add(f, [-3, 0, -6]));
    t.set(glow, "glowstone").await?;
    t.set(lamp, "red_lamp").await?;
    t.secs(0.5).await?;
    let lit = t.shot("night").await?;
    let floor = |b: [i32; 3]| [b[0] as f64 + 0.5, b[1] as f64, b[2] as f64 + 0.5];
    let by_glow = avg_around(&t, &lit, floor(add(glow, [-1, 0, 1])), 0.02);
    let by_lamp = avg_around(&t, &lit, floor(add(lamp, [1, 0, 1])), 0.02);
    t.probe("floor by the glowstone is lit", crate::runner::lum(by_glow), crate::runner::lum(by_glow) > 50.0);
    let red = by_lamp[0] - by_lamp[1];
    t.probe("floor by the red lamp is reddish (red minus green)", red, red > 15.0);
    Ok(())
}

/// Third person shows the player's model from behind and from the front.
pub async fn third_person(t: T) -> R {
    t.flat().await?;
    t.op("put me 0 iron_sword 1").await?;
    t.c().select(0);
    t.press("core:perspective");
    t.c().look(30.0, -15.0);
    t.secs(0.5).await?;
    for name in ["behind", "front"] {
        let shot = t.shot(name).await?;
        let (shirt, skin) = (shot.share(0.42, 0.35, 0.58, 0.8, probe::cyan), shot.share(0.42, 0.35, 0.58, 0.8, probe::skin));
        t.probe(&format!("{name}: shirt"), shirt, shirt > 0.05);
        t.probe(&format!("{name}: skin"), skin, skin > 0.05);
        t.press("core:perspective");
        t.secs(0.3).await?;
    }
    Ok(())
}

/// A skin of the player's own (`skins/skin.png`) goes to the server with the join and the model
/// wears it: an all red one turns the figure red instead of its shirt and skin.
pub async fn player_skin(t: T) -> R {
    let path = format!("{}/skins/skin.png", t.c().run_dir());
    std::fs::create_dir_all(format!("{}/skins", t.c().run_dir())).map_err(|e| e.to_string())?;
    let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 64, 64);
    enc.set_color(png::ColorType::Rgba);
    let px: Vec<u8> = (0..64 * 64).flat_map(|_| [220u8, 30, 30, 255]).collect();
    enc.write_header().and_then(|mut w| w.write_image_data(&px)).map_err(|e| e.to_string())?;
    let result = async {
        t.flat().await?;
        t.press("core:perspective");
        t.c().look(30.0, -15.0);
        t.secs(0.8).await?;
        let shot = t.shot("red").await?;
        let (red, shirt) = (shot.share(0.42, 0.35, 0.58, 0.8, probe::red), shot.share(0.42, 0.35, 0.58, 0.8, probe::cyan));
        t.probe("the model in the player's red skin", red, red > 0.05);
        t.probe("and not in the default shirt", shirt, shirt < 0.01);
        t.press("core:perspective");
        t.press("core:perspective");
        Ok::<(), String>(())
    }
    .await;
    drop(std::fs::remove_file(&path));
    result
}

/// Crouching shows in third person: the model is lower (vanilla's pose).
pub async fn crouch_pose(t: T) -> R {
    t.flat().await?;
    t.press("core:perspective");
    t.c().look(30.0, -15.0);
    t.secs(0.3).await?;
    t.press("core:perspective");
    t.secs(0.3).await?;
    let standing = t.shot("standing").await?;
    t.hold("core:sneak", 2.0);
    t.secs(1.0).await?;
    let crouching = t.shot("crouching").await?;
    // The camera goes down with the eyes, so the model is not lower on the picture but shorter:
    // from the top of its hair (below the sky) to the bottom of its trousers in the middle column.
    let height = |s: &Shot| {
        let rows: Vec<f32> = (0..s.h).map(|y| y as f32 / s.h as f32).collect();
        let top = rows.iter().copied().find(|&y| !probe::light(s.at(0.5, y))).unwrap_or(1.0);
        let bottom = rows.iter().copied().filter(|&y| probe::blue(s.at(0.5, y))).last().unwrap_or(top);
        bottom - top
    };
    let ratio = height(&crouching) / height(&standing).max(1e-3);
    t.probe("crouching model height / standing", ratio, ratio < 0.92 && ratio > 0.6);
    t.secs(1.2).await?;
    t.press("core:perspective");
    Ok(())
}

/// What stands behind water is seen through it, tinted: a pig in a pool shows violet, not
/// pink; lava covers what is under it.
pub async fn liquid_layers(t: T) -> R {
    t.flat().await?;
    t.op("time 6000").await?;
    let f = t.feet();
    // A pool dug into the ground, half of it under ice, and a lava pit beside it.
    t.fill(add(f, [-3, -3, -7]), add(f, [1, -1, -3]), "water").await?;
    t.fill(add(f, [-3, -1, -7]), add(f, [-2, -1, -3]), "ice").await?;
    t.fill(add(f, [3, -2, -6]), add(f, [4, -1, -4]), "lava").await?;
    t.op(&format!("spawn pig {} {} {}", f[0], f[1] - 3, f[2] - 5)).await?;
    t.c().look(0.0, -40.0);
    t.frames(30).await?;
    let pool = t.shot("pool").await?;
    let water = around(&t, &pool, [f[0] as f64, f[1] as f64 - 0.1, f[2] as f64 - 4.5], 0.03, probe::blue);
    t.probe("the pool is drawn blue", water, water > 0.5);
    let lava = around(&t, &pool, [f[0] as f64 + 4.0, f[1] as f64 - 0.1, f[2] as f64 - 5.0], 0.02, probe::orange);
    t.probe("the lava pit is drawn orange", lava, lava > 0.5);
    // Over the pool, looking down at the pig under the water.
    t.c().look(0.0, -70.0);
    t.cmd(&format!("/tp {} {} {}", f[0] as f64 - 0.5, f[1], f[2] as f64 - 4.0)).await?;
    t.frames(20).await?;
    let over = t.shot("outline_water").await?;
    let violet = over.share(0.2, 0.2, 0.8, 0.9, probe::under_water);
    let pink = over.share(0.2, 0.2, 0.8, 0.9, |c| c[0] > 200 && c[1] > 120 && c[2] > 120 && c[0] as i32 > c[2] as i32 + 30);
    t.probe("the pig under water is tinted violet", violet, violet > 0.005);
    t.probe("and not drawn pink", pink, pink < 0.002);
    t.cmd(&format!("/tp {} {} {}", f[0] as f64 + 2.5, f[1], f[2] as f64 - 4.5)).await?;
    t.c().look(270.0, -55.0);
    t.frames(20).await?;
    let pit = t.shot("outline_lava").await?;
    let v = pit.share(0.3, 0.3, 0.7, 0.7, probe::orange);
    t.probe("looking into the lava pit", v, v > 0.6);
    Ok(())
}

/// Cracks cover a block's model (a ladder's board, not its cube), and instant breaks show none:
/// in creative the next block under a held button stays clean while the break waits.
pub async fn break_overlay(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let wall = add(f, [0, 0, -2]);
    t.fill(wall, add(wall, [0, 1, 0]), "cobblestone").await?;
    let ladder = add(wall, [0, 0, 1]);
    t.set(ladder, "ladder[facing=south]").await?;
    t.look_at(ladder);
    t.frames(3).await?;
    let (w, h) = (t.state().window[0], t.state().window[1]);
    let before = t.grab().await?;
    t.hold("core:attack", 0.3);
    t.secs(0.25).await?;
    let cracked = t.shot("ladder").await?.px;
    let face = t.block_on_screen(ladder, [0.0, 0.0, -0.45]).unwrap_or((0.5, 0.5));
    let v = changed(&before, &cracked, w, h, face.0 - 0.04, face.1 - 0.06, face.0 + 0.04, face.1 + 0.06);
    t.probe("cracks on the ladder", v, v > 0.05);
    let above = t.block_on_screen(add(wall, [0, 1, 0]), [0.0, 0.0, 0.5]).unwrap_or((0.5, 0.2));
    let v = changed(&before, &cracked, w, h, above.0 - 0.04, above.1 - 0.04, above.0 + 0.04, above.1 + 0.04);
    t.probe("none on the wall above it", v, v < 0.02);
    t.secs(0.3).await?;
    t.op("mode creative").await?;
    t.set(ladder, "air").await?;
    let next = add(wall, [0, 0, -1]);
    t.fill(wall, add(next, [0, 1, 0]), "dirt").await?;
    t.look_at(wall);
    t.frames(3).await?;
    t.hold("core:attack", 0.5);
    t.until("the first block broken", Duration::from_secs(2), |t| t.c().block(wall) == Some(0)).await?;
    let held = t.shot("creative").await?.px;
    // The same blocks without a button held: the next block as it looks clean.
    t.secs(0.6).await?;
    t.fill(next, add(next, [0, 1, 0]), "dirt").await?;
    t.set(wall, "air").await?;
    t.frames(10).await?;
    let clean = t.grab().await?;
    let p = t.block_on_screen(next, [0.0, 0.0, 0.5]).unwrap_or((0.5, 0.45));
    let v = changed(&held, &clean, w, h, p.0 - 0.05, p.1 - 0.05, p.0 + 0.05, p.1 + 0.05);
    t.probe("no cracks on the next block of an instant break", v, v < 0.03);
    Ok(())
}

/// The player figure in the inventory stays while mobs are drawn in the world behind it (both
/// once shared the model buffers of a frame).
pub async fn inventory_figure(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    for dz in -2..=2 {
        t.op(&format!("spawn testmod:idol {} {} {}", f[0] + 4, f[1], f[2] + dz)).await?;
    }
    t.look_at(add(f, [4, 1, 0]));
    t.secs(0.5).await?;
    t.press("core:inventory");
    t.screen("core:menu").await?;
    t.frames(4).await?;
    let Some([x, y, w, h]) = t.c().ui_rect("entity") else { return Err("no player figure in the inventory".into()) };
    let shot = t.shot("figure").await?;
    let v = shot.share(x, y, x + w, y + h, probe::skin);
    t.probe("skin in the figure's box", v, v > 0.03);
    t.press("core:inventory");
    Ok(())
}

/// Right click swings the arm only when it did something: on grass with an empty hand the
/// picture stays as it was, placing a block swings.
pub async fn use_swing(t: T) -> R {
    t.flat().await?;
    t.c().look(0.0, -45.0);
    t.frames(10).await?;
    let still = t.grab().await?;
    t.press("core:use");
    t.frames(4).await?;
    let after = t.grab().await?;
    t.check(T::differ(&still, &after) < 0.001, format!("the arm moved on a use that did nothing ({:.4})", T::differ(&still, &after)));
    t.op("put me 0 dirt 4").await?;
    t.c().select(0);
    // Past the use cooldown of the first click.
    t.secs(0.3).await?;
    t.press("core:use");
    t.until("placed", Duration::from_secs(3), |t| t.c().inventory_slot(0).count == 3).await?;
    t.frames(2).await?;
    t.shot("placed").await?;
    Ok(())
}

/// A ladder takes the facing of the wall it was put on.
pub async fn ladder_placement(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let wall = add(f, [0, 0, -2]);
    t.fill(wall, add(wall, [0, 1, 0]), "cobblestone").await?;
    t.op("put me 0 ladder 8").await?;
    t.c().select(0);
    t.look_at(wall);
    t.frames(3).await?;
    t.press("core:use");
    let at = add(wall, [0, 0, 1]);
    t.until("ladder placed", Duration::from_secs(5), |t| t.c().inventory_slot(0).count == 7).await?;
    // States face away from the wall, in face order (west, east, north, south): on a wall to
    // its north it faces south.
    t.check_eq(t.block(at).await?, "base:ladder[facing=south]".into(), "ladder on the north wall");
    Ok(())
}

/// A torch put against a wall hangs on it, and drops when the wall goes.
pub async fn wall_torch(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let wall = add(f, [0, 0, -2]);
    t.fill(wall, add(wall, [0, 1, 0]), "cobblestone").await?;
    t.op("put me 0 torch 8").await?;
    t.c().select(0);
    t.look_at(wall);
    t.frames(3).await?;
    t.press("core:use");
    let at = add(wall, [0, 0, 1]);
    t.until("torch placed", Duration::from_secs(5), |t| t.c().inventory_slot(0).count == 7).await?;
    // West, east, up (the floor), north, south: on a wall to its north it faces south.
    t.check_eq(t.block(at).await?, "base:torch[facing=south]".into(), "torch on the north wall");
    t.frames(5).await?;
    t.shot("wall_torch").await?;
    t.set(wall, "air").await?;
    t.secs(0.3).await?;
    t.check_eq(t.block(at).await?, "core:air".into(), "the torch fell with its wall");
    Ok(())
}

pub async fn pick_block(t: T) -> R {
    t.flat().await?;
    t.op("mode creative").await?;
    t.until("creative", Duration::from_secs(5), |t| t.state().mode == 1).await?;
    let f = t.feet();
    let b = add(f, [0, -1, -1]);
    t.set(b, "bricks").await?;
    t.look_at(b);
    t.frames(3).await?;
    t.c().click(2, 0.0);
    t.until("bricks picked", Duration::from_secs(5), |t| t.c().inventory_slot(t.state().selected).item == item("base:bricks")).await
}

/// The cube and light gizmos draw their lines and numbers.
pub async fn gizmos(t: T) -> R {
    t.flat().await?;
    t.c().look(30.0, -30.0);
    t.secs(0.5).await?;
    let off = t.shot("off").await?;
    t.cmd("/gizmo cubes").await?;
    t.cmd("/gizmo light").await?;
    t.secs(0.5).await?;
    let on = t.shot("cubes_light").await?;
    let v = on.share(0.0, 0.0, 1.0, 0.9, probe::yellow) - off.share(0.0, 0.0, 1.0, 0.9, probe::yellow);
    t.probe("yellow gizmo pixels added", v, v > 0.002);
    t.cmd("/gizmo cubes").await?;
    t.cmd("/gizmo light").await?;
    Ok(())
}

/// Hosts the test world and lets a second game process join it over the network transport.
pub async fn multiplayer(t: T) -> R {
    t.world(13).await?;
    t.c().host_world();
    t.until("hosting", Duration::from_secs(60), |t| t.flag(client::client_flags::HOSTING)).await?;
    let run = t.c().run_dir();
    let dir = format!("{run}/test-mp");
    let _ = std::fs::create_dir_all(&dir);
    let ticket = format!("{dir}/ticket.txt");
    let script = format!("{dir}/join.txt");
    std::fs::write(&ticket, t.c().ticket()).map_err(|e| e.to_string())?;
    std::fs::write(&script, format!("join {ticket}\nloaded\nchat hello from the second player\nwait 3\nsay_config testmod:wisp_sparks\nwait 1\nquit\n")).map_err(|e| e.to_string())?;
    static SPARKS: loom_sdk::Config<i32> = loom_sdk::Config::named("testmod:wisp_sparks");
    let sparks = SPARKS.get();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(exe)
        .args(["--run-dir", &dir, "--script", &script, "--size", "640x360"])
        .spawn()
        .map_err(|e| format!("cannot start the second client: {e}"))?;
    let heard = t
        .until("the second player's chat", Duration::from_secs(90), |t| (0..5).any(|i| t.c().chat_line(i).contains("hello from the second player")))
        .await;
    let seen = t.c().entities().iter().filter_map(|&id| t.c().entity(id)).any(|e| name_of(abi::entry_kind::ENTITY, e.kind) == "core:player");
    // A synced setting the host changes while the other player is in reaches them.
    SPARKS.set(7);
    let synced = t.until("the second player's config", Duration::from_secs(30), |t| (0..5).any(|i| t.c().chat_line(i).contains("config testmod:wisp_sparks = 7"))).await;
    SPARKS.set(sparks);
    if heard.is_err() || synced.is_err() {
        let _ = child.kill();
    }
    let mut status = None;
    let _ = t
        .until("the second client to quit", Duration::from_secs(30), |_| {
            status = child.try_wait().ok().flatten();
            status.is_some()
        })
        .await;
    if status.is_none() {
        let _ = child.kill();
    }
    heard?;
    synced?;
    t.check(seen, "the host does not see the second player");
    t.check(status.is_some_and(|s| s.success()), format!("second client ended with {status:?}"));
    Ok(())
}

/// A mod key binding is listed in Controls and reaches the mod; a frame hook draws in the world
/// and on the screen.
pub async fn client_hooks(t: T) -> R {
    t.flat().await?;
    let before = crate::runner::PROBES.with(|p| p.get());
    t.c().key("F9", 0.0);
    t.until("probe key seen", Duration::from_secs(3), |_| crate::runner::PROBES.with(|p| p.get()) > before).await?;
    t.c().open_screen(&screens::CONTROLS);
    t.screen("core:controls").await?;
    for _ in 0..40 {
        if t.c().ui_rect("text:test probe").is_some() {
            break;
        }
        t.c().cursor(0.5, 0.5);
        t.c().scroll(-3.0);
        t.frames(2).await?;
    }
    t.check(t.c().ui_rect("text:test probe").is_some(), "Controls does not list the mod's key");
    t.c().close_screen();
    t.no_screen().await?;
    let f = t.feet();
    let b = [f[0] as f64 + 2.0, f[1] as f64, f[2] as f64 - 4.0];
    t.c().look(0.0, -10.0);
    static DRAWN: loom_sdk::GizmoGroup = loom_sdk::GizmoGroup::named("drawn");
    let green = |shot: &crate::runner::Shot| shot.share(0.0, 0.0, 1.0, 0.9, |c| c[1] > 200 && c[0] < 80 && c[2] < 80);
    for _ in 0..3 {
        t.c().draw_fill(Some(&DRAWN), [b[0] - 3.0, b[1], b[2]], [b[0] - 2.0, b[1] + 1.0, b[2] + 1.0], 0x20FF20FF);
        t.c().draw_box(None, b, [b[0] + 1.0, b[1] + 2.0, b[2] + 1.0], 0xFF4040FF);
        t.c().draw_text(None, [b[0] + 0.5, b[1] + 2.5, b[2] + 0.5], "testkit.drawn", &[], 0xFFFF80FF, 2.0);
        t.c().screen_text(0.5, 0.3, "testkit.screen_text", &[], 0x80FFFFFF, 1.0);
        t.frames(1).await?;
    }
    t.c().draw_fill(Some(&DRAWN), [b[0] - 3.0, b[1], b[2]], [b[0] - 2.0, b[1] + 1.0, b[2] + 1.0], 0x20FF20FF);
    t.c().draw_box(None, b, [b[0] + 1.0, b[1] + 2.0, b[2] + 1.0], 0xFF4040FF);
    t.c().draw_text(None, [b[0] + 0.5, b[1] + 2.5, b[2] + 0.5], "testkit.drawn", &[], 0xFFFF80FF, 2.0);
    t.c().screen_text(0.5, 0.3, "testkit.screen_text", &[], 0x80FFFFFF, 1.0);
    let shot = t.shot("drawn").await?;
    let hidden = green(&shot);
    t.probe("a shape of a group that is off is not drawn", hidden, hidden < 0.0005);
    let red = shot.share(0.0, 0.0, 1.0, 0.9, probe::red);
    t.probe("the box drawn in the world", red, red > 0.001);
    let cyan = shot.share(0.25, 0.25, 0.75, 0.4, |c| c[2] > 200 && c[1] > 200 && c[0] < 190);
    t.probe("the screen text", cyan, cyan > 0.003);
    let yellow = shot.share(0.0, 0.0, 1.0, 0.5, |c| c[0] > 200 && c[1] > 200 && c[2] < 170);
    t.probe("the text drawn in the world", yellow, yellow > 0.001);
    t.cmd("/gizmo testkit:drawn").await?;
    for _ in 0..3 {
        t.c().draw_fill(Some(&DRAWN), [b[0] - 3.0, b[1], b[2]], [b[0] - 2.0, b[1] + 1.0, b[2] + 1.0], 0x20FF20FF);
        t.frames(1).await?;
    }
    t.c().draw_fill(Some(&DRAWN), [b[0] - 3.0, b[1], b[2]], [b[0] - 2.0, b[1] + 1.0, b[2] + 1.0], 0x20FF20FF);
    let shot = t.shot("drawn_group").await?;
    t.cmd("/gizmo testkit:drawn").await?;
    let shown = green(&shot);
    t.probe("the same shape once its group is on", shown, shown > 0.001);
    // Screen text is text like any other: icons of blocks and items show in it.
    for _ in 0..3 {
        t.c().screen_text(0.5, 0.7, "testkit.rich_label", &[], 0xFFFFFFFF, 2.0);
        t.frames(1).await?;
    }
    t.c().screen_text(0.5, 0.7, "testkit.rich_label", &[], 0xFFFFFFFF, 2.0);
    let rich = t.shot("rich_label").await?;
    let diamond = rich.share(0.35, 0.65, 0.65, 0.8, probe::cyan);
    t.probe("a block icon in screen text", diamond, diamond > 0.002);
    Ok(())
}
