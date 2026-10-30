//! Finds mod libraries, loads them and runs registration in parallel.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use rayon::prelude::*;
use loom_abi::{self as abi, ModId};

use crate::content::{self, ModMeta, RawRegistrar};

pub struct LoadedMod {
    pub id: String,
    pub assets: PathBuf,
    _lib: libloading::Library,
    decl: &'static abi::ModDecl,
}

static NAMES: OnceLock<Vec<String>> = OnceLock::new();

pub fn mod_id_name(m: ModId) -> &'static str {
    NAMES.get().and_then(|n| n.get(m.0 as usize)).map_or("?", |s| s.as_str())
}

/// Mods found in `mod_dirs`, earlier dirs winning: a folder (`assets/` and the library) or the
/// same packed into `<name>.zip`, unpacked into `cache` once per version of the file. A folder's
/// library may also sit next to the engine executable instead (development builds,
/// `target/<profile>/`).
pub fn discover(mod_dirs: &[PathBuf], cache: &Path) -> Vec<(String, PathBuf, PathBuf)> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let mut found = Vec::new();
    for dir in mod_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            let (name, root) = if path.is_dir() {
                (path.file_name().unwrap_or_default().to_string_lossy().into_owned(), path)
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
                let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                match unpack(&path, &cache.join("unpacked"), &name) {
                    Ok(root) => (name, root),
                    Err(e) => {
                        log::error!("mod {}: {e}", path.display());
                        continue;
                    }
                }
            } else {
                continue;
            };
            let file = libloading::library_filename(&name);
            let lib = [Some(root.join(&file)), exe_dir.as_ref().map(|d| d.join(&file))].into_iter().flatten().find(|p| p.exists());
            match lib {
                Some(lib) => found.push((name, root, lib)),
                // The library is named after the mod (its crate), and so must be the folder or zip.
                None => log::error!("mod {}: no {} in it; a mod's folder or zip is named like its crate", root.display(), file.to_string_lossy()),
            }
        }
    }
    // Stable: for a name found twice, the first dir's copy stays.
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    found
}

/// Unpacks a mod archive into `<cache>/<name>-<size>-<mtime>`, unless that is already there;
/// older unpacked versions of the mod are removed.
fn unpack(archive: &Path, cache: &Path, name: &str) -> Result<PathBuf, String> {
    let meta = std::fs::metadata(archive).map_err(|e| e.to_string())?;
    let stamp = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let dest = cache.join(format!("{name}-{}-{stamp}", meta.len()));
    if dest.join(".done").exists() {
        return Ok(dest);
    }
    if let Ok(old) = std::fs::read_dir(cache) {
        for e in old.flatten() {
            if e.file_name().to_string_lossy().rsplitn(3, '-').nth(2) == Some(name) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    zip.extract(&dest).map_err(|e| e.to_string())?;
    std::fs::write(dest.join(".done"), b"").map_err(|e| e.to_string())?;
    log::info!("unpacked mod {name}");
    Ok(dest)
}

/// Packs a mod folder (its `assets/`, and its library from the folder or from next to the engine
/// executable) into `<out>/<name>.zip`, the single file players drop into `mods/`.
pub fn pack(dir: &Path, out: &Path) -> Result<PathBuf, String> {
    use std::io::Write;
    let name = dir.file_name().ok_or("not a mod folder")?.to_string_lossy().into_owned();
    let file = libloading::library_filename(&name);
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let lib = [Some(dir.join(&file)), exe_dir.map(|d| d.join(&file))].into_iter().flatten().find(|p| p.exists()).ok_or_else(|| format!("{} is not built", file.to_string_lossy()))?;
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let path = out.join(format!("{name}.zip"));
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).map_err(|e| e.to_string())?);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut add = |rel: String, src: &Path| -> Result<(), String> {
        zip.start_file(rel, opts).map_err(|e| e.to_string())?;
        zip.write_all(&std::fs::read(src).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    };
    add(file.to_string_lossy().into_owned(), &lib)?;
    let mut files = Vec::new();
    collect(&dir.join("assets"), &mut files);
    for f in files {
        let rel = f.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
        add(rel, &f)?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(path)
}

/// `pack`, unless `<out>/<name>.zip` is already newer than the library and every asset. Returns
/// the archive and whether it was written.
pub fn pack_if_stale(dir: &Path, out: &Path) -> Result<(PathBuf, bool), String> {
    let name = dir.file_name().ok_or("not a mod folder")?.to_string_lossy().into_owned();
    let zip = out.join(format!("{name}.zip"));
    let file = libloading::library_filename(&name);
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let mut inputs = Vec::new();
    collect(&dir.join("assets"), &mut inputs);
    inputs.extend([Some(dir.join(&file)), exe_dir.map(|d| d.join(&file))].into_iter().flatten().filter(|p| p.exists()));
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let newest = inputs.iter().filter_map(|p| modified(p)).max();
    if let (Some(z), Some(n)) = (modified(&zip), newest) {
        if z >= n {
            return Ok((zip, false));
        }
    }
    pack(dir, out).map(|p| (p, true))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Loads every library, then calls all `init`s in parallel and freezes the registries.
pub fn load(mod_dirs: &[PathBuf], cache: &Path) -> Result<Vec<LoadedMod>, String> {
    let start = Instant::now();
    let shadow = cache.join("mods");
    std::fs::create_dir_all(&shadow).ok();
    if let Ok(entries) = std::fs::read_dir(&shadow) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }

    let mut mods = Vec::new();
    for (name, dir, lib_path) in discover(mod_dirs, cache) {
        // Windows locks loaded libraries; loading a copy keeps the original rebuildable.
        let copy = shadow.join(format!("{}-{}", std::process::id(), lib_path.file_name().unwrap().to_string_lossy()));
        let load_from = if std::fs::copy(&lib_path, &copy).is_ok() { copy } else { lib_path.clone() };
        let lib = unsafe { libloading::Library::new(&load_from) }
            .map_err(|e| format!("loading mod {name} from {}: {e}", lib_path.display()))?;
        let entry: libloading::Symbol<unsafe extern "C" fn() -> *const abi::ModDecl> =
            unsafe { lib.get(abi::ENTRY_SYMBOL) }.map_err(|_| format!("{} is not a Loom mod", lib_path.display()))?;
        let decl = unsafe { &*entry() };
        if decl.abi_version != abi::ABI_VERSION {
            return Err(format!("mod {name} was built against another mod ABI ({:016x}, this engine has {:016x}); rebuild it against the current loom_sdk", decl.abi_version, abi::ABI_VERSION));
        }
        let id = unsafe { decl.id.as_str() }.to_string();
        if id != name {
            return Err(format!("mod {name} is the crate {id}: name its folder or zip {id}"));
        }
        mods.push(LoadedMod { id, assets: dir.join("assets"), _lib: lib, decl });
    }
    mods.sort_by(|a, b| a.id.cmp(&b.id));
    let _ = NAMES.set(mods.iter().map(|m| m.id.clone()).collect());

    let metas: Vec<ModMeta> = mods
        .iter()
        .map(|m| unsafe {
            ModMeta {
                id: m.id.clone(),
                version: m.decl.version.as_str().to_string(),
                deps: m.decl.deps.as_slice().iter().map(|d| d.as_str().to_string()).collect(),
                order: 0,
                assets: m.assets.clone(),
            }
        })
        .collect();
    for m in &metas {
        for d in &m.deps {
            if !metas.iter().any(|o| &o.id == d) {
                return Err(format!("mod {} requires missing mod {d}", m.id));
            }
        }
    }

    let regs: Vec<RawRegistrar> = mods
        .par_iter()
        .enumerate()
        .map(|(i, m)| {
            let mut reg = RawRegistrar::new(ModId(i as u32));
            let t = Instant::now();
            let r = unsafe { (m.decl.init)(&crate::host::HOST, ModId(i as u32), &mut reg as *mut _ as *mut abi::Registrar) };
            if r != 0 {
                reg.errors.push("init panicked".into());
            }
            log::debug!("{} registered in {:.1} ms", m.id, t.elapsed().as_secs_f64() * 1000.0);
            reg
        })
        .collect();

    let content = content::freeze(metas, regs).map_err(|e| e.join("\n"))?;
    log::info!(
        "{} mods, {} blocks ({} states), {} items, {} dimensions in {:.1} ms",
        mods.len(),
        content.blocks.len(),
        content.states.len(),
        content.items.len(),
        content.dims.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    // Load order is part of the game's behaviour (later mods win on conflicts), so every mod is logged.
    let mut order: Vec<&ModMeta> = content.mods.iter().collect();
    order.sort_by_key(|m| m.order);
    for m in order {
        log::info!("loaded mod {} {} (load position {})", m.id, m.version, m.order);
    }
    content::set(content);
    Ok(mods)
}
