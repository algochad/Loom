use std::time::Duration;

use loom_sdk::*;

use crate::runner::{R, T};

/// Screens open by name and lay out inside the window: the engine's menus and a mod's own.
pub async fn screens(t: T) -> R {
    t.screen("core:title").await?;
    t.shot("title").await?;
    t.laid_out(&["text:Singleplayer", "text:Multiplayer", "text:Mods", "text:Settings", "id:profile", "text:Quit Game", "text:About Test Mod"]);
    // A mod's button in the main menu opens its own screen, with no world around.
    t.check(t.click_ui("text:About Test Mod", 0), "the test mod's main menu button");
    t.screen("testmod:about").await?;
    t.c().close_screen();
    t.screen("core:title").await?;
    for (screen, texts) in [
        (&screens::SETTINGS, &["text:View distance", "text:Language", "text:Controls", "text:Shader Packs", "text:Done"][..]),
        (&screens::LANGUAGE, &["text:English", "text:Done"][..]),
        (&screens::CONTROLS, &["text:Reset All", "text:Done"][..]),
        (&screens::WORLDS, &["text:Create New World", "id:saves_folder"][..]),
        (&screens::MODS, &["id:mod:base", "id:mod:testmod", "id:mod_description"][..]),
        (&screens::PROFILE, &["id:profile_name", "text:Copy Key"][..]),
    ] {
        let name = screen.name();
        t.check(t.c().open_screen(screen), format!("cannot open {name}"));
        t.screen(name).await?;
        t.shot(name.trim_start_matches("core:")).await?;
        t.laid_out(texts);
        t.c().close_screen();
        t.screen("core:title").await?;
    }
    t.flat().await?;
    t.c().chat("/about");
    t.screen("testmod:about").await?;
    t.until("its text", Duration::from_secs(2), |t| t.c().ui_rect("text:opened by name").is_some()).await?;
    t.shot("about").await?;
    t.laid_out(&["text:Test Mod", "text:opened by name"]);
    // A part of a mod's screen with a tooltip of its own.
    let [x, y, w, h] = t.c().ui_rect("text:Hover me").ok_or("no part to hover")?;
    t.c().cursor(x + w / 2.0, y + h / 2.0);
    t.frames(4).await?;
    let shot = t.shot("about_tooltip").await?;
    let red = shot.share(x + w / 2.0, y - 0.02, (x + 0.3).min(1.0), y + 0.06, super::probe::red);
    t.probe("the tooltip's red text", red, red > 0.005);
    // What the engine's screens have, a mod's has too: it pauses, takes text, slides, shows items.
    t.check(t.flag(client::client_flags::PAUSED), "a mod's screen declared to pause pauses single player");
    t.check(t.click_ui("id:about_name", 0), "the mod screen's text field");
    t.frames(2).await?;
    t.c().type_text("Steve");
    t.until("the typed name", Duration::from_secs(3), |t| t.c().ui_rect("text:Hello, Steve").is_some()).await?;
    let [x, y, w, h] = t.c().ui_rect("slider").ok_or("no slider")?;
    t.c().cursor(x + w * 0.75, y + h / 2.0);
    t.c().click(0, 0.1);
    t.until("the slider moved", Duration::from_secs(3), |t| (70..=80).any(|v| t.c().ui_rect(&format!("text:Volume: {v}")).is_some())).await?;
    t.check(t.c().ui_rect("text:Field of view here: 70").is_some(), "a mod's screen reads the settings");
    let coal = Item::lookup("base:coal").ok_or("no coal")?;
    t.check(t.c().ui_rect(&format!("icon:{coal}")).is_some(), "the item on the mod's screen");
    t.shot("about_parts").await?;
    t.c().close_screen();
    t.no_screen().await?;
    t.check(!t.flag(client::client_flags::PAUSED), "running again when it closed");
    Ok(())
}

/// F3 shows its panels inside the window, also in the widest case (Russian at GUI scale 3), the
/// engine's on the left and right halves and a mod's own panel, which its group bit hides.
pub async fn f3(t: T) -> R {
    t.world(5).await?;
    t.idle().await?;
    t.c().look(0.0, 0.0);
    t.debug(true).await?;
    t.frames(10).await?;
    t.shot("f3").await?;
    let sides = |t: &T| {
        for q in ["text:XYZ", "text:base:overworld"] {
            t.check(t.c().ui_rect(q).is_some_and(|r| r[0] < 0.5), format!("{q} on the left"));
        }
        for q in ["text:calls", "text:1600x900"] {
            t.check(t.c().ui_rect(q).is_some_and(|r| r[0] + r[2] > 0.5), format!("{q} on the right"));
        }
    };
    t.laid_out(&["text:XYZ", "text:base:overworld", "text:calls", "text:1600x900"]);
    sides(&t);
    t.until("testmod panel shown", Duration::from_secs(2), |t| t.c().ui_rect("text:fall").is_some()).await?;
    t.laid_out(&["text:fall"]);
    loom_sdk::settings::DEBUG_GROUPS.set(u32::MAX & !(1 << 5));
    t.until("testmod panel hidden", Duration::from_secs(2), |t| t.c().ui_rect("text:fall").is_none()).await?;
    t.check(t.c().ui_rect("text:XYZ").is_some(), "engine panels stay");
    loom_sdk::settings::DEBUG_GROUPS.set(u32::MAX);
    loom_sdk::settings::LANGUAGE.set("ru_ru");
    loom_sdk::settings::GUI_SCALE.set(3);
    t.frames(10).await?;
    t.shot("f3_ru_3").await?;
    t.laid_out(&["text:XYZ", "text:base:overworld", "text:calls", "text:1600x900"]);
    sides(&t);
    loom_sdk::settings::LANGUAGE.set("en_us");
    loom_sdk::settings::GUI_SCALE.set(2);
    t.debug(false).await
}

fn id(name: &str) -> u32 {
    Item::lookup(name).unwrap_or(u32::MAX)
}

/// The 2x2 grid of the inventory screen, driven by clicks on slots like a player would.
pub async fn inventory_crafting(t: T) -> R {
    t.flat().await?;
    t.op("put me 0 oak_log 16").await?;
    t.press("core:inventory");
    t.screen("core:menu").await?;
    // Menu slots: 0-3 grid, 4 result, 5-8 armor, 9-35 main, 36-44 hotbar.
    let (log, planks) = (id("base:oak_log"), id("base:oak_planks"));
    t.click_ui("slot:36", 0);
    t.until("logs on the cursor", Duration::from_secs(5), |t| t.c().cursor_stack().item == log).await?;
    t.click_ui("slot:0", 1);
    t.until("planks in the result", Duration::from_secs(5), |t| t.c().menu_slot(4).item == planks).await?;
    t.check_eq(t.c().menu_slot(4).count, 4, "planks from one log");
    t.click_ui("slot:36", 0);
    t.until("cursor empty", Duration::from_secs(5), |t| t.c().cursor_stack().is_empty()).await?;
    t.shot("grid").await?;
    t.click_ui("slot:4", 0);
    t.until("planks on the cursor", Duration::from_secs(5), |t| t.c().cursor_stack().item == planks).await?;
    t.click_ui("slot:37", 0);
    t.until("planks in the hotbar", Duration::from_secs(5), |t| t.c().inventory_slot(1).item == planks).await?;
    t.c().close_screen();
    t.no_screen().await?;
    t.check_eq(t.slot("me", 0).await?, ("base:oak_log".into(), 15, 0), "logs left");
    t.check_eq(t.slot("me", 1).await?, ("base:oak_planks".into(), 4, 0), "planks made");
    t.sync_inventory().await
}

pub async fn creative_inventory(t: T) -> R {
    t.flat().await?;
    t.op("mode creative").await?;
    t.until("creative", Duration::from_secs(5), |t| t.state().mode == 1).await?;
    t.press("core:inventory");
    t.screen("core:menu").await?;
    t.shot("tab").await?;
    let stone = id("base:bricks");
    if t.click_ui(&format!("item:{stone}"), 0) {
        t.until("bricks on the cursor", Duration::from_secs(5), |t| t.c().cursor_stack().item == stone).await?;
        t.click_ui("slot:36", 0);
        t.until("bricks in the hotbar", Duration::from_secs(5), |t| t.c().inventory_slot(0).item == stone).await?;
    }
    t.c().close_screen();
    t.no_screen().await?;
    t.check_eq(t.slot("me", 0).await?.0, "base:bricks".into(), "creative pick");
    Ok(())
}

/// The chat: `/` opens it with the command list shown; Tab completes items, client commands,
/// word sets and other mods' branches; commands run.
pub async fn chat(t: T) -> R {
    t.world(3).await?;
    t.press("core:command");
    t.screen("core:chat").await?;
    t.until("the command list", Duration::from_secs(2), |t| t.c().ui_rect("text:gamemode").is_some()).await?;
    t.check_eq(t.c().input_text(), "/".to_string(), "the line");
    // A mod's key does not fire while the chat has the input.
    let probes = crate::runner::PROBES.with(|p| p.get());
    t.c().key("F9", 0.3);
    t.secs(0.5).await?;
    t.check(crate::runner::PROBES.with(|p| p.get()) == probes, "the testkit:probe key fired while typing in the chat");
    t.c().key("Backspace", 0.0);
    t.frames(2).await?;
    for (typed, want) in [("/give @p birch_lo", "/give @p base:birch_log"), ("/giz", "/gizmo"), ("/gizmo cu", "/gizmo cubes"), ("/gizmo testm", "/gizmo testmod:energy"), ("/gizmo pro", "/gizmo probe"), ("/probe al", "/probe alpha"), ("/rule mob", "/rule core:mob_spawning")] {
        t.c().type_text(typed);
        t.frames(2).await?;
        t.c().key("Tab", 0.0);
        t.frames(3).await?;
        let text = t.c().input_text();
        t.check(text.starts_with(want) || (typed.starts_with("/give") && text.contains("birch_log")), format!("Tab completed {typed} to '{text}', want {want}"));
        if typed.starts_with("/give") {
            t.c().key("Enter", 0.0);
            t.no_screen().await?;
            t.until("logs given", Duration::from_secs(5), |t| t.c().inventory_slot(0).item == id("base:birch_log")).await?;
            t.press("core:chat");
            t.screen("core:chat").await?;
            continue;
        }
        for _ in 0..text.chars().count() {
            t.c().key("Backspace", 0.0);
            t.frames(1).await?;
        }
    }
    t.c().key("Escape", 0.0);
    t.no_screen().await?;
    let reply = t.cmd("/gizmo cubes").await?;
    t.check(reply.contains("cubes on"), format!("/gizmo cubes said '{reply}'"));
    // Any word goes, the list is only for completion (mods' groups).
    let reply = t.cmd("/gizmo testmod:energy").await?;
    t.check(reply.contains("testmod:energy on"), format!("/gizmo testmod:energy said '{reply}'"));
    let reply = t.cmd("/gizmo probe 7").await?;
    t.check_eq(reply, "gizmo probe 7 from testkit".into(), "another mod's branch of /gizmo");
    let help = t.cmd("/help gizmo").await?;
    t.check(help.contains("#core:gizmos") && help.contains("probe"), format!("/help gizmo said '{help}'"));
    let reply = t.cmd("/probe beta").await?;
    t.check_eq(reply, "probe beta on the client".into(), "mod client command");
    let reply = t.cmd("/regions").await?;
    t.check_eq(reply, "regions from testkit".into(), "a mod's command takes the engine's place");
    // What a player types shows as typed, braces included; it can't format.
    t.c().chat("braces {#ff0000}stay");
    t.until("the chat line", Duration::from_secs(5), |t| t.c().chat_line(0).contains("stay")).await?;
    t.press("core:chat");
    t.screen("core:chat").await?;
    t.frames(2).await?;
    t.check(t.c().ui_rect("text:{#ff0000}stay").is_some(), "a player's braces show literally in chat");
    t.c().key("Escape", 0.0);
    t.no_screen().await?;

    // Replies travel as a key and arguments: the client words them, and again after a language
    // change. The engine's, a mod's and a parse error alike.
    let replies = [("/time query", "time is", "время"), ("/feed 5", "food set to 5", "сытость теперь 5"), ("/give @p base:nothing", "usage: /give", "использование: /give")];
    for (line, _, _) in replies {
        t.cmd(line).await?;
    }
    let lines = |t: &T| (0..3).rev().map(|i| t.c().chat_line(i)).collect::<Vec<_>>();
    let en = lines(&t);
    loom_sdk::settings::LANGUAGE.set("ru_ru");
    t.frames(3).await?;
    let ru = lines(&t);
    loom_sdk::settings::LANGUAGE.set("en_us");
    for (i, (line, want_en, want_ru)) in replies.into_iter().enumerate() {
        t.check(en[i].contains(want_en), format!("{line} in English: '{}'", en[i]));
        t.check(ru[i].contains(want_ru), format!("{line} in Russian: '{}'", ru[i]));
    }
    Ok(())
}

/// Settings mods declare get a screen built from the declarations; changes apply and are saved.
pub async fn mod_settings(t: T) -> R {
    static HUM: Config<bool> = Config::named("testmod:generator_hum");
    t.c().open_screen(&screens::MOD_SETTINGS);
    t.screen("core:mod_settings").await?;
    t.frames(2).await?;
    t.check(t.click_ui("text:Test Mod", 0), "test mod listed");
    t.screen("core:config").await?;
    t.frames(3).await?;
    t.shot("testmod").await?;
    let before = HUM.get();
    t.check(t.click_ui("text:Generator hum", 0), "hum toggle shown");
    t.until("toggled", std::time::Duration::from_secs(2), |_| HUM.get() != before).await?;
    // A test run writes beside the player's settings, never over them.
    let file = std::fs::read_to_string(format!("{}/test-config/testmod.txt", t.c().run_dir())).unwrap_or_default();
    t.check(file.contains(&format!("generator_hum = {}", !before)), format!("saved: {file:?}"));
    HUM.set(before);
    t.c().close_screen();
    t.c().close_screen();
    Ok(())
}

/// The recipe viewer: R over an item shows what makes it, U what it is used in; tagged
/// ingredients and machines of every mod show up.
pub async fn recipe_viewer(t: T) -> R {
    t.flat().await?;
    t.op("put me 0 iron_ingot 3").await?;
    t.press("core:inventory");
    t.screen("core:menu").await?;
    let Some([x, y, w, h]) = t.c().ui_rect("slot:36") else { return Err("no hotbar slot".into()) };
    t.c().cursor(x + w / 2.0, y + h / 2.0);
    t.frames(3).await?;
    t.press("core:recipes");
    t.screen("core:recipes").await?;
    t.frames(3).await?;
    t.check(t.c().ui_rect("id:tab:base:smelting").is_some(), "smelting recipes for the ingot");
    t.shot("iron_recipes").await?;
    t.c().close_screen();
    t.screen("core:menu").await?;
    t.c().cursor(x + w / 2.0, y + h / 2.0);
    t.frames(3).await?;
    t.press("core:uses");
    t.screen("core:recipes").await?;
    t.frames(3).await?;
    t.check(t.c().ui_rect("text:crafting").is_some(), "crafting uses of the ingot");
    t.shot("iron_uses").await?;
    // One tab per kind; the test mod's alloying shows its info line from the recipe's values.
    t.check(t.c().ui_rect("id:tab:base:crafting").is_some(), "a crafting tab");
    t.check(t.click_ui("id:tab:testmod:alloying", 0), "an alloying tab");
    t.secs(0.2).await?;
    t.frames(3).await?;
    t.check(t.c().ui_rect("text:32 FE/t").is_some(), "the alloying recipe's energy in its info line");
    t.shot("alloying").await?;
    // Tags: the list, then the items of one.
    t.check(t.click_ui("text:Tags", 0), "a tags button");
    t.screen("core:tags").await?;
    t.frames(3).await?;
    t.click_ui("id:tag_search", 0);
    t.frames(2).await?;
    t.c().type_text("ingots/iron");
    t.frames(4).await?;
    t.check(t.click_ui("id:tag:ingots/iron", 0), "the ingots/iron tag in the list");
    t.screen("core:tag").await?;
    t.frames(3).await?;
    t.check(t.c().ui_rect("text:#ingots/iron").is_some(), "the tag's items");
    t.shot("tag").await?;
    t.c().close_screen();
    t.screen("core:tags").await?;
    t.c().close_screen();
    t.screen("core:recipes").await?;
    t.c().close_screen();
    t.screen("core:menu").await?;
    // A kind laid out by its mod, with a fluid.
    t.op("put me 1 diamond 1").await?;
    t.frames(3).await?;
    let Some([x, y, w, h]) = t.c().ui_rect("slot:37") else { return Err("no hotbar slot".into()) };
    t.c().cursor(x + w / 2.0, y + h / 2.0);
    t.frames(3).await?;
    t.press("core:uses");
    t.screen("core:recipes").await?;
    t.frames(3).await?;
    t.check(t.click_ui("id:tab:testmod:infusing", 0), "an infusing tab");
    t.secs(0.2).await?;
    t.frames(3).await?;
    t.check(t.c().ui_rect("text:laid out by the test mod").is_some(), "the infusing kind's own layout");
    t.shot("infusing").await?;
    t.c().close_screen();
    t.screen("core:menu").await?;
    // The arrow of a menu that crafts opens its recipes.
    let Some([x, y, w, h]) = t.c().ui_rect("class:arrow") else { return Err("no arrow in the inventory".into()) };
    t.c().cursor(x + w / 2.0, y + h / 2.0);
    t.frames(2).await?;
    t.c().click(0, 0.05);
    t.screen("core:recipes").await?;
    t.frames(3).await?;
    t.check(t.c().ui_rect("text:Recipes").is_some() && t.c().ui_rect("id:tab:base:crafting").is_some(), "the arrow shows the crafting recipes");
    // The crafting table opens the crafting menu, so it is the station of its recipes.
    t.check(t.c().ui_rect(&format!("icon:{}", id("base:crafting_table"))).is_some(), "the crafting table shown as the station");
    t.shot("arrow").await?;
    t.c().close_screen();
    t.c().close_screen();
    t.no_screen().await
}

/// Inventory hotkeys: a number key over a slot swaps it with that hotbar slot, a double click
/// gathers matching items onto the cursor, and shift with the left button held moves every
/// stack swept over (mouse tweaks).
pub async fn inventory_hotkeys(t: T) -> R {
    t.flat().await?;
    t.op("put me 9 stone 5").await?;
    t.op("put me 10 dirt 10").await?;
    t.op("put me 11 dirt 7").await?;
    t.op("put me 20 cobblestone 3").await?;
    t.op("put me 21 cobblestone 4").await?;
    t.press("core:inventory");
    t.screen("core:menu").await?;
    let hover = |slot: u32| -> R {
        let Some([x, y, w, h]) = t.c().ui_rect(&format!("slot:{slot}")) else { return Err(format!("no slot {slot}")) };
        t.c().cursor(x + w / 2.0, y + h / 2.0);
        Ok(())
    };
    hover(9)?;
    t.frames(3).await?;
    t.press("core:hotbar3");
    t.secs(0.3).await?;
    t.check_eq(t.slot("me", 2).await?.0, "base:stone".to_string(), "number key 3 moved the stack to hotbar slot 3");
    t.check_eq(t.slot("me", 9).await?.1, 0, "and emptied the slot it was in");

    hover(10)?;
    t.frames(3).await?;
    t.c().click(0, 0.0);
    t.frames(4).await?;
    t.c().click(0, 0.0);
    t.secs(0.3).await?;
    t.check_eq(t.c().cursor_stack().count, 17, "a double click gathered both dirt stacks");
    t.c().click(0, 0.0);
    t.secs(0.3).await?;

    t.c().key("ShiftLeft", 1.0);
    hover(20)?;
    t.frames(3).await?;
    t.c().click(0, 0.6);
    t.frames(3).await?;
    hover(21)?;
    t.secs(0.8).await?;
    t.check(t.slot("me", 20).await?.1 == 0 && t.slot("me", 21).await?.1 == 0, "shift-sweeping moved both cobblestone stacks to the hotbar");
    Ok(())
}

/// Rebinding in Controls: the clicked entry waits for a key (Escape cancels and leaves the screen
/// open). A key pressed with Ctrl makes a chord that the key alone does not trigger; a modifier
/// let go of with nothing else pressed is the key itself.
pub async fn rebind(t: T) -> R {
    t.flat().await?;
    // `keys`: (key, seconds held, seconds before the next one).
    let bind = async |keys: &[(&str, f32, f32)]| -> R {
        t.c().open_screen(&screens::CONTROLS);
        t.screen("core:controls").await?;
        t.frames(2).await?;
        t.click_ui("id:bind:core:forward", 0);
        t.secs(0.2).await?;
        for &(key, held, after) in keys {
            t.c().key(key, held);
            t.secs(after).await?;
        }
        t.frames(4).await?;
        Ok(())
    };
    let walked = async |keys: &[&str]| -> Result<f64, String> {
        t.c().close_screen();
        t.no_screen().await?;
        let before = t.state().pos;
        for k in keys {
            t.c().key(k, 0.6);
        }
        t.secs(0.8).await?;
        let after = t.state().pos;
        Ok((after[0] - before[0]).hypot(after[2] - before[2]))
    };
    bind(&[("Escape", 0.0, 0.0)]).await?;
    t.screen("core:controls").await?;
    t.c().close_screen();
    t.no_screen().await?;
    let k = {
        bind(&[("KeyK", 0.0, 0.0)]).await?;
        walked(&["KeyK"]).await?
    };
    let (alone, chord) = {
        bind(&[("ControlLeft", 0.5, 0.1), ("KeyK", 0.0, 0.5)]).await?;
        t.check(t.c().ui_rect("text:Ctrl+K").is_some(), "the entry shows Ctrl+K");
        t.shot("chord").await?;
        (walked(&["KeyK"]).await?, walked(&["ControlLeft", "KeyK"]).await?)
    };
    let shift = {
        bind(&[("ShiftRight", 0.1, 0.3)]).await?;
        walked(&["ShiftRight"]).await?
    };
    bind(&[("KeyW", 0.0, 0.0)]).await?;
    t.c().close_screen();
    t.no_screen().await?;
    t.check(k > 1.0, format!("K walks after rebinding forward to it (moved {k:.2})"));
    t.check(alone < 0.2 && chord > 1.0, format!("bound to Ctrl+K, K alone moved {alone:.2} and Ctrl+K {chord:.2}"));
    t.check(shift > 1.0, format!("a right Shift let go of alone is the key (moved {shift:.2})"));
    Ok(())
}

/// A long list shows a scroll bar; clicking low on its track scrolls the list down.
pub async fn scrollbar(t: T) -> R {
    t.c().open_screen(&screens::CONTROLS);
    t.screen("core:controls").await?;
    t.frames(2).await?;
    let [x, y, w, h] = t.c().ui_rect("scrollbar").ok_or("no scroll bar on the controls screen")?;
    let first = t.c().ui_rect("text:Walk forward").ok_or("no first entry")?;
    t.c().cursor(x + w / 2.0, y + h * 0.95);
    t.frames(1).await?;
    t.c().click(0, 0.05);
    t.frames(4).await?;
    let moved = t.c().ui_rect("text:Walk forward").map_or(-1.0, |r| r[1]);
    t.check(moved < first[1] - 0.05, format!("the list scrolled (first entry at {:.3}, was {:.3})", moved, first[1]));
    t.c().close_screen();
    Ok(())
}
