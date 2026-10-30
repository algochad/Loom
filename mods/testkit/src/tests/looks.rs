//! Particles, text effects and conditions, shaders from mods, glow masks, models.

use std::time::Duration;

use loom_sdk::*;

use super::{add, probe};
use crate::runner::{R, T};

static SPARK: Particle = Particle::named("testmod:spark");

/// Bursts from the client and from the server (a broken block's dust) live and die out.
pub async fn particles(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let pos = [f[0] as f64 + 0.5, f[1] as f64 + 1.5, f[2] as f64 - 2.5];
    drop(t.c().spawn_particles(&SPARK, pos).count(200).spread(0.5).speed(1.0));
    t.until("client particles", Duration::from_secs(2), |t| t.state().particles >= 200).await?;
    // Sparks live at most 1.2 s.
    t.until("particles gone", Duration::from_secs(4), |t| t.state().particles == 0).await?;
    let b = add(f, [0, 0, -2]);
    t.set(b, "dirt").await?;
    t.op(&format!("break {} {} {}", b[0], b[1], b[2])).await?;
    // Vanilla's 4x4x4 pieces, sent by the server.
    t.until("block dust", Duration::from_secs(3), |t| t.state().particles >= 60).await?;
    t.until("dust settled", Duration::from_secs(4), |t| t.state().particles == 0).await?;
    Ok(())
}

/// Huge counts stay cheap: simulated in parallel, drawn in one instanced draw.
pub async fn particle_crowd(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let pos = [f[0] as f64 + 0.5, f[1] as f64 + 1.5, f[2] as f64 - 2.5];
    for _ in 0..10 {
        drop(t.c().spawn_particles(&SPARK, pos).count(10_000).spread(4.0).speed(1.0));
    }
    t.frames(2).await?;
    let n = t.state().particles;
    let b = t.bench(0.6, "100k particles").await?;
    t.note(format!("{n} particles: {:.0} fps, frame {:.2} ms, gpu {:.2} ms", b[0], b[1], b[3]));
    t.check(n >= 99_000 && b[1] < 33.0, format!("{n} particles in {:.1} ms frames", b[1]));
    Ok(())
}

/// Texture edges at a distance, where mip levels mix texels: no light line along the edge of a
/// snowy grass block over dirt; a 64x64 texture shows its own colors among 16x16 ones.
pub async fn texture_seams(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    t.fill(add(f, [-8, 0, -12]), add(f, [8, 0, -12]), "dirt").await?;
    t.fill(add(f, [-8, 1, -12]), add(f, [8, 1, -12]), "snowy_grass").await?;
    t.fill(add(f, [-8, 2, -12]), add(f, [8, 2, -12]), "dirt").await?;
    t.fill(add(f, [-8, 3, -12]), add(f, [8, 3, -12]), "grass").await?;
    t.fill(add(f, [-8, 0, -9]), add(f, [8, 0, -9]), "tall_grass").await?;
    // 64x64 next to 16x16 textures.
    let mosaic = add(f, [1, 0, -5]);
    t.fill(mosaic, add(mosaic, [1, 1, 0]), "testmod:mosaic").await?;
    t.c().look(35.0, 8.0);
    t.settled().await?;
    let shot = t.shot("seams").await?;
    // Just below and above the edge between the dirt and the snowy grass on top of it, on the
    // face the camera sees.
    let mut light = 0;
    let mut seen = 0;
    for x in -7..=7 {
        for dy in [0.96, 1.04] {
            if let Some((sx, sy)) = t.on_screen([(f[0] + x) as f64 + 0.5, f[1] as f64 + dy, (f[2] - 11) as f64 + 0.01]) {
                if (0.0..1.0).contains(&sx) && (0.0..1.0).contains(&sy) {
                    seen += 1;
                    light += (probe::lum(shot.at(sx, sy)) > 170.0) as u32;
                }
            }
        }
    }
    t.check(seen > 6, format!("the wall's edge is in view ({seen} points)"));
    t.probe("light points along the snowy grass edge", light as f32, light == 0);
    // The middle of the 2x2 mosaic face the camera sees.
    let p = t.on_screen([(mosaic[0] + 1) as f64, (mosaic[1] + 1) as f64, mosaic[2] as f64 + 1.01]).unwrap_or((0.5, 0.5));
    let colors = shot.share(p.0 - 0.03, p.1 - 0.05, p.0 + 0.03, p.1 + 0.05, |c| {
        let (hi, lo) = (*c.iter().max().unwrap() as i32, *c.iter().min().unwrap() as i32);
        hi - lo > 80
    });
    t.probe("the mosaic's saturated colors", colors, colors > 0.3);
    Ok(())
}

/// A 16x16 texture of one color.
fn write_png(path: &str, rgb: [u8; 3]) -> Result<(), String> {
    let dir = std::path::Path::new(path).parent().ok_or("no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    enc.set_color(png::ColorType::Rgba);
    let px: Vec<u8> = (0..256).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect();
    enc.write_header().and_then(|mut w| w.write_image_data(&px)).map_err(|e| e.to_string())
}

/// A mod's tint kind takes the biome's color where it sets one (plains: teal), else its
/// default (violet; the flatlands are beach).
pub async fn tint_kinds(t: T) -> R {
    for (flat, teal) in [(true, false), (false, true)] {
        if flat {
            t.flat().await?;
        } else {
            t.world(5).await?;
            t.idle().await?;
        }
        let f = t.feet();
        let (a, b) = (add(f, [-3, 0, -3]), add(f, [3, 3, -3]));
        t.fill(a, b, "testmod:glow_moss").await?;
        let moss = Block::lookup("testmod:glow_moss");
        // Every block of the wall must reach the client before its color can be looked at.
        t.until("the moss wall on the client", Duration::from_secs(5), |t| (a[1]..=b[1]).all(|y| (a[0]..=b[0]).all(|x| t.c().block([x, y, a[2]]) == moss))).await?;
        t.c().look(0.0, 0.0);
        t.settled().await?;
        let [r, g, b] = t.pixel(0.5, 0.45).await?;
        let ok = if teal { g > r + 40 && b > r + 30 } else { r > g + 20 && b > r };
        t.check(ok, format!("{} moss, got {r} {g} {b}", if teal { "teal" } else { "violet" }));
        t.shot(if teal { "plains" } else { "flatlands" }).await?;
    }
    Ok(())
}

/// Reloading assets picks up a resource pack's texture and later changes to that file.
pub async fn texture_reload(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    t.fill(add(f, [-3, 0, -3]), add(f, [3, 3, -3]), "dirt").await?;
    t.c().look(0.0, 0.0);
    t.settled().await?;
    let plain = t.grab().await?;
    let pack = format!("{}/resourcepacks/testkit_reload", t.c().run_dir());
    let file = format!("{pack}/base/textures/block/dirt.png");
    let reload = async |rgb: Option<[u8; 3]>| -> Result<Vec<u8>, String> {
        match rgb {
            Some(c) => write_png(&file, c)?,
            None => drop(std::fs::remove_dir_all(&pack)),
        }
        t.c().reload_assets();
        t.frames(5).await?;
        t.settled().await?;
        t.grab().await
    };
    loom_sdk::settings::RESOURCE_PACKS.set("testkit_reload");
    let red = reload(Some([200, 40, 40])).await?;
    let blue = reload(Some([40, 40, 200])).await?;
    loom_sdk::settings::RESOURCE_PACKS.set("");
    let back = reload(None).await?;
    t.check(T::differ(&plain, &red) > 0.2, "the pack's dirt shows after a reload");
    t.check(T::differ(&red, &blue) > 0.2, "a changed file shows after the next reload");
    t.check(T::differ(&plain, &back) < 0.01, "without the pack the dirt is back");
    Ok(())
}

/// A resource pack's translations change only the keys it has; the rest of the same mod's file
/// stays.
pub async fn pack_lang(t: T) -> R {
    let pack = format!("{}/resourcepacks/testkit_lang", t.c().run_dir());
    let dir = format!("{pack}/core/lang");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(format!("{dir}/en_us.lang"), "menu.done = Pack Done\n").map_err(|e| e.to_string())?;
    loom_sdk::settings::RESOURCE_PACKS.set("testkit_lang");
    t.c().reload_assets();
    t.frames(2).await?;
    t.c().open_screen(&screens::SETTINGS);
    t.screen("core:settings").await?;
    t.frames(2).await?;
    t.laid_out(&["text:Pack Done", "text:View distance", "text:Language"]);
    t.c().close_screen();
    loom_sdk::settings::RESOURCE_PACKS.set("");
    drop(std::fs::remove_dir_all(&pack));
    t.c().reload_assets();
    t.frames(2).await?;
    t.c().open_screen(&screens::SETTINGS);
    t.screen("core:settings").await?;
    t.frames(2).await?;
    t.check(t.c().ui_rect("text:Pack Done").is_none(), "without the pack the key is the mod's again");
    t.laid_out(&["text:Done"]);
    t.c().close_screen();
    Ok(())
}

/// A shader pack whose files do not compile changes nothing: every engine shader, the composed
/// ones (models, particles, screen effects, UI) too, falls back to the built-in one.
pub async fn broken_shader_pack(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    t.op(&format!("spawn testmod:idol {} {} {}", f[0], f[1], f[2] - 3)).await?;
    t.c().look(0.0, 0.0);
    t.settled().await?;
    t.secs(0.5).await?;
    let before = t.grab().await?;
    let dir = format!("{}/shaderpacks/testkit_broken", t.c().run_dir());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for name in ["world.wgsl", "entity.wgsl", "model.wgsl", "particle.wgsl", "post.wgsl", "ui.wgsl"] {
        std::fs::write(format!("{dir}/{name}"), "this is not wgsl").map_err(|e| e.to_string())?;
    }
    loom_sdk::settings::SHADER_PACK.set("testkit_broken");
    t.c().reload_assets();
    t.frames(5).await?;
    let after = t.grab().await?;
    loom_sdk::settings::SHADER_PACK.set("");
    t.c().reload_assets();
    let _ = std::fs::remove_dir_all(&dir);
    t.frames(2).await?;
    t.check(T::differ(&before, &after) < 0.02, format!("the picture stays ({:.1}% differ)", T::differ(&before, &after) * 100.0));
    // A working pack only writes functions: the engine's own world shader with a sepia ending,
    // its structs and bindings come from the prelude.
    let dir = format!("{}/shaderpacks/testkit_sepia", t.c().run_dir());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let world = include_str!("../../../../crates/engine/assets/shaders/world.wgsl");
    let ending = "    return vec4(mix(lit.rgb, scene.fog_color.rgb, fog), lit.a);";
    t.check(world.contains(ending), "the world shader still ends its shading like this");
    let sepia = world.replace(ending, "    let y = dot(mix(lit.rgb, scene.fog_color.rgb, fog), vec3(0.3, 0.59, 0.11));
    return vec4(vec3(y * 1.2, y, y * 0.75), lit.a);");
    std::fs::write(format!("{dir}/world.wgsl"), sepia).map_err(|e| e.to_string())?;
    loom_sdk::settings::SHADER_PACK.set("testkit_sepia");
    t.c().reload_assets();
    t.frames(5).await?;
    let shot = t.shot("sepia").await?;
    loom_sdk::settings::SHADER_PACK.set("");
    t.c().reload_assets();
    let _ = std::fs::remove_dir_all(&dir);
    t.frames(2).await?;
    // The grass floor turns brown: red over green over blue.
    let brown = shot.share(0.0, 0.7, 1.0, 0.9, |c| c[0] > c[1] && c[1] > c[2]);
    t.probe("a pack's sepia world shader is used", brown, brown > 0.8);
    Ok(())
}

/// The test mod's looks at night: a glow crystal's veins glow without lighting the floor next to
/// it, a wisp floats nearby, and the daydream screen effect changes the picture.
pub async fn shaders_and_glow(t: T) -> R {
    t.world(9).await?;
    t.idle().await?;
    t.op("time 18000").await?;
    let f = t.feet();
    // A level stage of its own, so terrain changes do not move the picture.
    t.fill(add(f, [-4, -1, -7]), add(f, [4, -1, 3]), "stone").await?;
    t.fill(add(f, [-4, 0, -7]), add(f, [4, 5, 3]), "air").await?;
    let crystal = add(f, [0, 1, -4]);
    t.fill(add(f, [-1, 0, -4]), add(f, [1, 2, -4]), "testmod:glow_crystal").await?;
    // A block shader (`shaders/block/stripes.wgsl`): magenta bands, lit or not.
    let striped = add(f, [-3, 1, -4]);
    t.set(striped, "testmod:reinforced_stone").await?;
    // Off to the side and back a little: it drifts, so it stays small in the picture.
    t.op(&format!("spawn testmod:wisp {} {} {}", f[0] + 3, f[1] + 1, f[2] - 5)).await?;
    t.check(t.count_near("wisp", 8.0).await? == 1, "a wisp floats nearby");
    t.op("put me 0 testmod:rune 1").await?;
    t.c().select(0);
    t.c().look(0.0, -5.0);
    t.settled().await?;
    t.secs(1.0).await?;
    let night = t.shot("night_glow").await?;
    let veins = match t.block_on_screen(crystal, [0.0, 0.0, 0.51]) {
        Some((x, y)) => night.share(x - 0.12, y - 0.15, x + 0.12, y + 0.15, |c| probe::cyan(c) && probe::lum(c) > 100.0),
        None => 0.0,
    };
    t.probe("the crystal's veins glow", veins, veins > 0.03);
    let floor = match t.on_screen([f[0] as f64 + 0.5, f[1] as f64, f[2] as f64 - 2.5]) {
        Some((x, y)) => crate::runner::lum(night.avg(x - 0.02, y - 0.02, x + 0.02, y + 0.02)),
        None => 255.0,
    };
    t.probe("the floor in front of it stays dark (glow is not light)", floor, floor < 60.0);
    let bands = match t.block_on_screen(striped, [0.0, 0.0, 0.51]) {
        Some((x, y)) => night.share(x - 0.03, y - 0.05, x + 0.03, y + 0.05, |c| c[0] > 100 && c[2] > 100 && c[1] < 60),
        None => 0.0,
    };
    t.probe("the block shader's magenta bands", bands, bands > 0.2 && bands < 0.9);
    let before = t.grab().await?;
    t.check(t.c().post_effect("testmod:dream", 1.0), "the daydream effect exists");
    t.check(!t.c().post_effect("testmod:nonexistent", 1.0), "an unknown effect is refused");
    t.frames(3).await?;
    t.shot("dream").await?;
    let after = t.grab().await?;
    let d = T::differ(&before, &after) as f32;
    t.probe("the daydream effect changes the picture", d, d > 0.03);
    t.c().post_effect("testmod:dream", 0.0);
    Ok(())
}

/// A block shaped by a Blockbench Java model file: the test mod's pedestal is a plate, a crystal
/// column and a top, so against the sky the sides of the column show sky, the column itself not.
pub async fn block_model_file(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let p = add(f, [0, 2, -3]);
    t.set(p, "testmod:pedestal").await?;
    t.c().look(0.0, 15.0);
    t.settled().await?;
    let shot = t.shot("pedestal").await?;
    let at = |x: f64| t.on_screen([p[0] as f64 + x, p[1] as f64 + 0.35, p[2] as f64 + 0.5]).map(|(sx, sy)| crate::runner::lum(shot.avg(sx - 0.003, sy - 0.003, sx + 0.003, sy + 0.003)));
    let (gap, column) = (at(0.18).unwrap_or(0.0), at(0.5).unwrap_or(255.0));
    t.probe("sky beside the column (the file's shape, not a cube)", gap, gap > 180.0);
    t.probe("the crystal column itself", column, column < 150.0);
    // A resource pack's model of the same block redraws it, here as a whole cube.
    let pack = format!("{}/resourcepacks/testkit_shapes", t.c().run_dir());
    let file = format!("{pack}/testmod/models/block/pedestal.json");
    std::fs::create_dir_all(std::path::Path::new(&file).parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&file, r#"{"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{"north":{},"south":{},"east":{},"west":{},"up":{},"down":{}}}]}"#).map_err(|e| e.to_string())?;
    loom_sdk::settings::RESOURCE_PACKS.set("testkit_shapes");
    t.c().reload_assets();
    t.frames(5).await?;
    t.settled().await?;
    let cube = t.shot("pedestal_pack").await?;
    loom_sdk::settings::RESOURCE_PACKS.set("");
    let _ = std::fs::remove_dir_all(&pack);
    t.c().reload_assets();
    t.frames(2).await?;
    let filled = t.on_screen([p[0] as f64 + 0.18, p[1] as f64 + 0.35, p[2] as f64 + 0.5]).map(|(sx, sy)| crate::runner::lum(cube.avg(sx - 0.003, sy - 0.003, sx + 0.003, sy + 0.003))).unwrap_or(255.0);
    t.probe("a pack's model draws a cube there", filled, filled < 150.0);
    Ok(())
}

/// An item shaped by a model file (the test mod's totem: a stone base, a crystal on it) shows its
/// boxes in the hand and in the hotbar slot.
pub async fn item_model_file(t: T) -> R {
    t.flat().await?;
    t.op("put me 0 testmod:totem 1").await?;
    t.c().select(0);
    t.c().look(0.0, 20.0);
    t.settled().await?;
    let shot = t.shot("totem").await?;
    let crystal = |c: probe::C| probe::cyan(c) || (c[2] > 120 && c[0] < 90);
    let hand = shot.share(0.55, 0.55, 1.0, 0.94, crystal);
    t.probe("the crystal in the hand", hand, hand > 0.01);
    // The first hotbar slot of the HUD, at the bottom middle (gui scale 2, 1600x900).
    let slot = shot.share(0.388, 0.95, 0.412, 0.995, crystal);
    t.probe("the crystal in the hotbar slot", slot, slot > 0.015);
    Ok(())
}

/// Models: the orb (built in code, a sphere mesh and a child bone) turns by its file's idle
/// animation; the idol (a Blockbench file with bones and mesh wings) bows when used, a server
/// triggered animation that holds its last frame.
pub async fn models(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    t.op(&format!("spawn testmod:orb {} {} {}", f[0], f[1] + 1, f[2] - 3)).await?;
    t.op(&format!("spawn testmod:idol {} {} {}", f[0], f[1], f[2] + 3)).await?;
    t.check(t.count_near("orb", 8.0).await? == 1 && t.count_near("idol", 8.0).await? == 1, "an orb and an idol stand nearby");
    t.c().look(0.0, 0.0);
    t.settled().await?;
    t.secs(0.5).await?;
    let a = t.grab().await?;
    t.secs(0.5).await?;
    let b = t.grab().await?;
    t.check(T::differ(&a, &b) > 0.001, "the orb's ring turns by its keyframes");
    t.look_at(add(f, [0, 0, 3]));
    t.frames(3).await?;
    t.press("core:use");
    t.secs(1.5).await?;
    let a = t.grab().await?;
    t.secs(0.3).await?;
    let b = t.grab().await?;
    t.check(T::differ(&a, &b) == 0.0, "the bow holds its last frame");
    t.shot("idol_bow").await?;
    // The test mod gives the player a wave: it moves the first person hand by its `hand` bone.
    t.c().look(90.0, 0.0);
    t.frames(3).await?;
    let a = t.grab().await?;
    t.op("animate me wave").await?;
    t.secs(0.5).await?;
    let b = t.grab().await?;
    t.check(T::differ(&a, &b) > 0.002, "the first person hand waves");
    Ok(())
}

/// Tooltip parts behind `{on_key}` and `{if}` appear while their condition holds: a hurt player
/// sees the rune's red line, sneaking swaps "more" for its long text.
pub async fn rich_text(t: T) -> R {
    t.flat().await?;
    t.op("put me 0 testmod:rune 1").await?;
    // A fall hurts the player, which the test mod's `{if:testmod:hurt}` shows.
    let s = t.state();
    t.c().teleport([s.pos[0], s.pos[1] + 6.0, s.pos[2]]);
    t.until("hurt by the fall", Duration::from_secs(5), |t| t.state().health < 20.0).await?;
    t.press("core:inventory");
    t.screen("core:menu").await?;
    let Some([x, y, w, h]) = t.c().ui_rect("slot:36") else { return Err("no hotbar slot in the inventory".into()) };
    t.c().cursor(x + w / 2.0, y + h / 2.0);
    t.frames(4).await?;
    let rune = t.shot("rune").await?;
    let wound = |c: [u8; 3]| c[0] > 200 && (100..170).contains(&c[1]) && (100..170).contains(&c[2]);
    // The tooltip opens beside the cursor.
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let v = rune.share(cx, cy - 0.15, cx + 0.3, cy + 0.15, wound);
    t.probe("the {if:testmod:hurt} line (light red)", v, v > 0.003);
    t.hold("core:sneak", 2.0);
    t.frames(4).await?;
    let sneak = t.shot("rune_sneak").await?;
    let d = T::differ(&rune.px, &sneak.px) as f32;
    t.probe("sneaking changes the tooltip ({on_key})", d, d > 0.002);
    t.c().release_all();
    t.c().close_screen();
    t.no_screen().await
}

/// The machine screen's panel is drawn by the test mod's GUI shader (teal circuit lines).
pub async fn gui_shader(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let p = add(f, [0, 0, -2]);
    t.set(p, "testmod:generator").await?;
    t.op(&format!("menu testmod:generator {} {} {}", p[0], p[1], p[2])).await?;
    t.screen("core:menu").await?;
    t.frames(3).await?;
    let shot = t.shot("generator").await?;
    let Some([x, y, _, h]) = t.c().ui_rect("text:Generator") else { return Err("no Generator title".into()) };
    let v = shot.share(x, y + h, x + 0.2, y + 0.12, probe::teal);
    t.probe("circuit lines in the panel", v, v > 0.01);
    t.c().close_screen();
    t.no_screen().await
}
