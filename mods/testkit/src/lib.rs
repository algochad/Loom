//! Automated tests and scripted runs, written on the public client and server mod API.
//!
//! `loom --test all` runs every test (`--test furnace,armor` some, `--test list` names them,
//! `--test bench` the benchmarks), each in a fresh throwaway world, then writes
//! `run/test-reports/report.txt` and exits with the number of failed tests. Screenshots of the
//! run go to `run/test-shots/` for looking at; checks never compare pictures with references.
//! `--test-keep` keeps the test worlds. `loom --world w --script file.txt` runs a line script (see `script.rs`).
//! Without those options the mod only registers its message channels.

mod runner;
mod script;
mod server;
mod tests;

use std::sync::OnceLock;

use loom_sdk::*;

declare_mod!(deps: [], init: init);

static PLAN: OnceLock<(Option<String>, Option<String>)> = OnceLock::new();

/// What came back on the typed channels (`extend::typed_channels`).
pub static ECHOED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static GREETED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// Calls of the pigs' client tick, and the last ones, when a death ends (`extend::client_entity_tick`).
pub static PIG_TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static PIG_DEATHS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn init(r: &mut Registry) {
    server::register(r);
    r.client_packet(&server::ECHO_BACK, |_, e| ECHOED.store(e.n, std::sync::atomic::Ordering::Relaxed));
    r.client_packet(&server::GREET, |_, text| *GREETED.lock().unwrap() = text);
    r.key(&runner::PROBE, "F9");
    // A branch of an engine command, handled by this mod (on the client, like /gizmo).
    r.client_command_branch("gizmo", "probe <n:int>", |cmd| {
        let n = cmd.int("n").unwrap_or(0);
        cmd.reply("command.testkit.gizmo_probe", &[Arg::Int(n as i64)]);
    });
    // Pigs' cosmetics through a mod's tick that keeps the engine's (testkit is never shipped).
    static PIG: Entity = Entity::named("base:pig");
    r.client_entity_tick(&PIG, |_, e| {
        PIG_TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if e.dying().is_some_and(|d| d >= 1.0) {
            PIG_DEATHS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        e.engine_effects();
    });
    // An engine command a mod replaces (testkit is never shipped).
    r.command("regions", "", |cmd| cmd.reply("command.testkit.regions", &[]));
    // A command that never reaches the server: answers from the client it runs on.
    r.client_command("probe", "<what:word(alpha,beta)>", |cmd| {
        // A `ClientCommand` only ever runs here; the reply says so.
        let side = "command.testkit.client";
        let what = cmd.text("what").unwrap_or("").to_string();
        cmd.reply("command.testkit.probe", &[Arg::Text(&what), Arg::Key(side)]);
    });
    let (test, script) = (launch_option("test"), launch_option("script"));
    if test.is_none() && script.is_none() {
        return;
    }
    let _ = PLAN.set((test, script));
    r.client_frame(|c| {
        static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            start(c);
        }
        runner::frame(c);
    });
}

/// Builds the queue on the main thread, where the runner lives.
fn start(c: &Client) {
    let Some((test, script)) = PLAN.get() else { return };
    if let Some(path) = script {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                error!("cannot read script {path}: {e}");
                c.quit(2);
                return;
            }
        };
        let name = std::path::Path::new(path).file_stem().map_or("script".into(), |s| s.to_string_lossy().into_owned());
        runner::start(vec![runner::TestDef { name, run: Box::new(move |t| Box::pin(script::run(t, src.clone()))) }]);
        return;
    }
    // Same conditions on every machine; nothing of this is saved.
    {
        use loom_sdk::settings::*;
        LANGUAGE.set("en_us");
        GUI_SCALE.set(2);
        VIEW_DISTANCE.set(12);
        FOV.set(70.0);
        BRIGHTNESS.set(0.5);
        VSYNC.set(true);
        RESOURCE_PACKS.set("");
        SHADER_PACK.set("");
        DEBUG_GROUPS.set(u32::MAX);
    }
    // The player's packs would change every picture.
    c.reload_assets();
    let want = test.as_deref().unwrap_or("all");
    // Benchmarks run by name too, or all of them with `bench`.
    let all = match want {
        "bench" => tests::bench(),
        "all" | "list" => tests::all(),
        _ => tests::all().into_iter().chain(tests::bench()).collect(),
    };
    if want == "list" {
        for t in &all {
            info!("test {}", t.name);
        }
        c.quit(0);
        return;
    }
    let queue: Vec<runner::TestDef> = if want == "all" || want == "bench" {
        all
    } else {
        let names: Vec<&str> = want.split(',').map(str::trim).collect();
        if let Some(bad) = names.iter().find(|n| !all.iter().any(|t| t.name == **n)) {
            error!("no test named {bad}; --test list shows them");
            c.quit(2);
            return;
        }
        all.into_iter().filter(|t| names.contains(&t.name.as_str())).collect()
    };
    runner::start(queue);
}
