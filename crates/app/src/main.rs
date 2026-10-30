use std::path::PathBuf;

use loom_engine::Options;
use loom_render::{Backend, BackendDesc, Error};

#[global_allocator]
static ALLOC: loom_engine::diag::CountingAlloc = loom_engine::diag::CountingAlloc;

fn vulkan(desc: &BackendDesc) -> Result<Box<dyn Backend>, Error> {
    Ok(Box::new(loom_vulkan::VulkanBackend::new(desc)?))
}

/// Where `run` (and, in the source tree, `mods`) live: the working directory if it has either,
/// otherwise the nearest folder above the executable that does (a development build runs from
/// `target/<profile>/`).
fn game_root() -> PathBuf {
    let has = |d: &std::path::Path| d.join("run").is_dir() || d.join("mods").is_dir();
    let cwd = std::env::current_dir().unwrap_or_default();
    if has(&cwd) {
        return cwd;
    }
    let exe = std::env::current_exe().unwrap_or_default();
    exe.ancestors().skip(1).find(|d| has(d)).map(PathBuf::from).unwrap_or(cwd)
}

fn main() {
    let root = game_root();
    let mut opts = Options {
        run_dir: root.join("run"),
        // Mod sources of the game itself; players' mods go into `run/mods`, added below.
        mod_dirs: vec![root.join("mods")],
        gpu_debug: false,
        world: None,
        seed: None,
        screen: None,
        size: (1600, 900),
        dev: cfg!(debug_assertions),
        automated: false,
        no_vsync: false,
        view: None,
    };
    let mut verbose = false;
    let mut log_spec = String::new();
    let mut pack: Option<PathBuf> = None;
    let mut dist: Option<PathBuf> = None;
    // Options the engine does not know go to mods (`--test furnace`, `--script x.txt`).
    let mut extra: Vec<(String, String)> = Vec::new();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_default();
        match a.as_str() {
            "--run-dir" => opts.run_dir = PathBuf::from(value()),
            "--mods" => opts.mod_dirs.push(PathBuf::from(value())),
            "--gpu-debug" => opts.gpu_debug = true,
            "--verbose" => verbose = true,
            "--log" => log_spec = value(),
            "--world" => opts.world = Some(value()),
            "--seed" => opts.seed = value().parse().ok(),
            "--screen" => opts.screen = Some(value()),
            "--no-vsync" => opts.no_vsync = true,
            // Engine shaders and assets live from the source tree in a release build too.
            "--dev" => opts.dev = true,
            "--view" => opts.view = value().parse().ok(),
            "--pack-mod" => pack = Some(PathBuf::from(value())),
            "--dist" => dist = Some(PathBuf::from(value())),
            "--size" => {
                let v: Vec<u32> = value().split('x').filter_map(|s| s.parse().ok()).collect();
                if v.len() == 2 {
                    opts.size = (v[0], v[1]);
                }
            }
            other => match other.strip_prefix("--") {
                Some(name) => {
                    let v = if args.peek().is_some_and(|n| !n.starts_with("--")) { args.next().unwrap_or_default() } else { "1".into() };
                    extra.push((name.to_string(), v));
                }
                None => eprintln!("unknown argument {other}"),
            },
        }
    }
    opts.automated = extra.iter().any(|(k, _)| k == "script" || k == "test");
    loom_engine::set_launch_options(extra);
    let player_mods = opts.run_dir.join("mods");
    if let Some(dir) = pack {
        match loom_engine::modloader::pack(&dir, &player_mods) {
            Ok(path) => println!("packed {}", path.display()),
            Err(e) => eprintln!("cannot pack {}: {e}", dir.display()),
        }
        return;
    }
    if let Some(out) = dist {
        match make_dist(&root, &out) {
            Ok(()) => println!("game ready in {}", out.display()),
            Err(e) => eprintln!("cannot make {}: {e}", out.display()),
        }
        return;
    }
    loom_engine::log::init(Some(&opts.run_dir.join("logs").join("latest.log")), verbose, &log_spec);
    loom_engine::diag::install_crash_handler(&opts.run_dir);
    // Release builds load the game's own mods the way players get them: each packed into one
    // file in `run/mods` (again only when it changed).
    if !cfg!(debug_assertions) && root.join("mods").is_dir() {
        for dir in subdirs(&root.join("mods")) {
            match loom_engine::modloader::pack_if_stale(&dir, &player_mods) {
                Ok((path, true)) => log::info!("packed {}", path.display()),
                Ok(_) => {}
                Err(e) => log::warn!("cannot pack {}: {e}", dir.display()),
            }
        }
        let sources = root.join("mods");
        opts.mod_dirs.retain(|d| *d != sources);
    }
    opts.mod_dirs.push(player_mods);
    loom_engine::init_workers();
    if let Err(e) = loom_engine::run(vulkan, opts) {
        log::error!("{e}");
        std::process::exit(1);
    }
    std::process::exit(loom_engine::exit_code());
}

fn subdirs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// A folder to hand to someone: the executable, the engine's assets and every mod packed.
fn make_dist(root: &std::path::Path, out: &std::path::Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    std::fs::copy(&exe, out.join(exe.file_name().unwrap())).map_err(|e| e.to_string())?;
    copy_dir(&root.join("crates").join("engine").join("assets"), &out.join("assets").join("core"))?;
    for dir in subdirs(&root.join("mods")) {
        // Development tools (the test kit) mark themselves `dist = false` in Cargo.toml.
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
        if manifest.lines().any(|l| l.replace(' ', "") == "dist=false") {
            continue;
        }
        loom_engine::modloader::pack(&dir, &out.join("run").join("mods"))?;
    }
    Ok(())
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?.flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()))?;
        } else {
            std::fs::copy(&p, to.join(e.file_name())).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
