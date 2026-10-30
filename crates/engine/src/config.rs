//! Settings the engine (namespace `core`) and mods declare: typed, ranged values read with one
//! atomic load (text ones under a lock), edited on screens built from the declarations. The scope
//! says where a value lives: `CLIENT` in each player's `config/<ns>.txt`; `SERVER` in the
//! server's file, sent to its players whenever it changes; `WORLD` saved with the world (game
//! rules, `/rule`) and sent the same way. The engine's own settings and key bindings are `core:`
//! ones like any other, in `config/core.txt`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use loom_abi::{config_kind as ck, config_scope as scope};

use crate::content::content;

#[derive(Clone, Debug)]
pub struct ConfigDef {
    pub name: String,
    pub kind: u32,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub choices: Vec<String>,
    pub flags: u32,
    pub scope: u32,
    /// The default of a `TEXT` setting.
    pub text: String,
}

impl ConfigDef {
    /// The server's value counts for its players.
    pub fn synced(&self) -> bool {
        self.scope != scope::CLIENT
    }

    /// `v` made valid for this setting.
    pub fn fit(&self, v: f64) -> f64 {
        if !v.is_finite() {
            return self.default;
        }
        match self.kind {
            ck::BOOL => (v != 0.0) as u8 as f64,
            ck::CHOICE => v.round().clamp(0.0, self.choices.len().saturating_sub(1) as f64),
            ck::TEXT => 0.0,
            _ => {
                let mut v = v.clamp(self.min.min(self.max), self.max.max(self.min));
                if self.step > 0.0 {
                    v = self.min + ((v - self.min) / self.step).round() * self.step;
                }
                if self.kind == ck::INT { v.round() } else { v }
            }
        }
    }

    fn parse(&self, s: &str) -> Option<f64> {
        let s = s.trim();
        match self.kind {
            ck::BOOL => match s {
                "true" | "on" | "1" => Some(1.0),
                "false" | "off" | "0" => Some(0.0),
                _ => None,
            },
            ck::CHOICE => self.choices.iter().position(|c| c == s).map(|i| i as f64),
            _ => s.parse().ok(),
        }
    }

    pub fn show(&self, v: f64) -> String {
        match self.kind {
            ck::BOOL => (v != 0.0).to_string(),
            ck::CHOICE => self.choices.get(v as usize).cloned().unwrap_or_default(),
            ck::INT => format!("{}", v as i64),
            _ => {
                let s = format!("{v:.3}");
                s.trim_end_matches('0').trim_end_matches('.').to_string()
            }
        }
    }

    pub fn number(name: &str, kind: u32, default: f64, min: f64, max: f64, step: f64) -> ConfigDef {
        ConfigDef { name: name.into(), kind, default, min, max, step, choices: Vec::new(), flags: 0, scope: scope::CLIENT, text: String::new() }
    }

    pub fn text(name: &str, default: &str) -> ConfigDef {
        ConfigDef { text: default.into(), ..ConfigDef::number(name, ck::TEXT, 0.0, 0.0, 0.0, 0.0) }
    }
}

/// Value bits by config id.
static VALUES: OnceLock<Vec<AtomicU64>> = OnceLock::new();
/// Values of text settings by config id (empty for the others).
static TEXTS: RwLock<Vec<String>> = RwLock::new(Vec::new());
/// Counts changes, so whoever applies settings (the client) sees that something moved.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static RUN_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
/// Automated runs change settings for themselves only: they read the player's files and write
/// to `test-config/` beside them.
static AUTOMATED: AtomicBool = AtomicBool::new(false);
/// Values given on the command line for this run: (id, value in the file). Saving keeps the
/// file's value unless the setting was changed since.
static OVERRIDES: Mutex<Vec<(u32, String, String)>> = Mutex::new(Vec::new());

fn values() -> &'static [AtomicU64] {
    VALUES.get_or_init(|| content().game.configs.iter().map(|d| AtomicU64::new(d.default.to_bits())).collect())
}

fn def(id: u32) -> Option<&'static ConfigDef> {
    content().game.configs.get(id as usize)
}

pub fn get(id: u32) -> f64 {
    values().get(id as usize).map_or(0.0, |v| f64::from_bits(v.load(Ordering::Relaxed)))
}

pub fn text(id: u32) -> String {
    TEXTS.read().unwrap().get(id as usize).cloned().unwrap_or_default()
}

/// Changes so far; compare with an earlier value to see whether anything was set.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Sets a value without saving (server values arriving, loading files).
pub fn put(id: u32, v: f64) {
    if let (Some(slot), Some(d)) = (values().get(id as usize), def(id)) {
        let bits = d.fit(v).to_bits();
        if slot.swap(bits, Ordering::Relaxed) != bits {
            GENERATION.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn put_text(id: u32, v: &str) {
    let mut t = TEXTS.write().unwrap();
    if let Some(slot) = t.get_mut(id as usize).filter(|s| s.as_str() != v) {
        *slot = v.to_string();
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Sets a value and saves it where its scope keeps it (a world's are saved with the world).
pub fn set(id: u32, v: f64) {
    put(id, v);
    save_of(id);
}

pub fn set_text(id: u32, v: &str) {
    put_text(id, v);
    save_of(id);
}

fn save_of(id: u32) {
    let (Some(dir), Some(d)) = (RUN_DIR.get(), def(id)) else { return };
    if let (Some((ns, _)), false) = (d.name.split_once(':'), d.scope == scope::WORLD) {
        save(dir, ns);
    }
}

pub fn by_name(name: &str) -> Option<u32> {
    content().game.config_names.get(name).copied()
}

pub fn set_automated(on: bool) {
    AUTOMATED.store(on, Ordering::Relaxed);
}

/// A value for this run only (`--view`, `--no-vsync`), as the file writes it.
pub fn override_for_run(name: &str, value: &str) {
    let Some(id) = by_name(name) else { return };
    let saved = shown(id);
    put_value(id, value);
    let now = shown(id);
    OVERRIDES.lock().unwrap().push((id, now, saved));
}

/// A value as its file writes it.
fn shown(id: u32) -> String {
    match def(id) {
        Some(d) if d.kind == ck::TEXT => text(id),
        Some(d) => d.show(get(id)),
        None => String::new(),
    }
}

/// Puts a value written as text; false if it does not fit the setting.
fn put_value(id: u32, v: &str) -> bool {
    let Some(d) = def(id) else { return false };
    if d.kind == ck::TEXT {
        put_text(id, v.trim());
        return true;
    }
    match d.parse(v) {
        Some(n) => {
            put(id, n);
            true
        }
        None => false,
    }
}

/// Every setting back to its default, then each namespace's `config/<ns>.txt` (not the world's);
/// missing lines keep their defaults, unknown or bad ones are reported.
pub fn load(run_dir: &Path) {
    let _ = RUN_DIR.set(run_dir.to_path_buf());
    let c = content();
    {
        let mut t = TEXTS.write().unwrap();
        *t = c.game.configs.iter().map(|d| d.text.clone()).collect();
    }
    for (id, d) in c.game.configs.iter().enumerate() {
        put(id as u32, d.default);
    }
    let mut namespaces: Vec<&str> = c.game.configs.iter().filter_map(|d| d.name.split_once(':').map(|x| x.0)).collect();
    namespaces.dedup();
    for ns in namespaces {
        let Ok(src) = std::fs::read_to_string(file(run_dir, "config", ns)) else { continue };
        for (_, k, v) in crate::kv::pairs(&src) {
            let name = format!("{ns}:{k}");
            match by_name(&name).filter(|&id| c.game.configs[id as usize].scope != scope::WORLD) {
                Some(id) => {
                    if !put_value(id, v) {
                        let d = &c.game.configs[id as usize];
                        log::warn!("config {name}: cannot use '{}', keeping {}", v.trim(), d.show(d.default));
                    }
                }
                None => log::warn!("config/{ns}.txt: no setting {}", k.trim()),
            }
        }
    }
    for (id, value, _) in OVERRIDES.lock().unwrap().iter() {
        put_value(*id, value);
    }
}

fn file(run_dir: &Path, dir: &str, ns: &str) -> std::path::PathBuf {
    run_dir.join(dir).join(format!("{ns}.txt"))
}

/// Writes a namespace's settings, every one of them so players see what exists.
pub fn save(run_dir: &Path, ns: &str) {
    let c = content();
    let overrides = OVERRIDES.lock().unwrap().clone();
    let mut out = String::new();
    for (id, d) in c.game.configs.iter().enumerate().filter(|(_, d)| d.scope != scope::WORLD) {
        let Some(key) = d.name.strip_prefix(ns).and_then(|k| k.strip_prefix(':')) else { continue };
        let now = shown(id as u32);
        let value = overrides.iter().find(|o| o.0 == id as u32 && o.1 == now).map_or(now, |o| o.2.clone());
        out.push_str(&format!("{key} = {value}\n"));
    }
    let path = file(run_dir, if AUTOMATED.load(Ordering::Relaxed) { "test-config" } else { "config" }, ns);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Err(e) = std::fs::write(&path, out) {
        log::error!("saving {}: {e}", path.display());
    }
}

/// Values of the settings the server decides, for its players.
pub fn synced() -> Vec<(u32, f64)> {
    content().game.configs.iter().enumerate().filter(|(_, d)| d.synced() && d.kind != ck::TEXT).map(|(i, _)| (i as u32, get(i as u32))).collect()
}

/// A world's settings as it saves them.
pub fn world_values() -> Vec<(String, f64)> {
    content().game.configs.iter().enumerate().filter(|(_, d)| d.scope == scope::WORLD).map(|(i, d)| (d.name.clone(), get(i as u32))).collect()
}

/// Opening a world: its settings as it saved them, defaults for the rest.
pub fn load_world(saved: &[(String, f64)]) {
    for (id, d) in content().game.configs.iter().enumerate().filter(|(_, d)| d.scope == scope::WORLD) {
        put(id as u32, saved.iter().find(|s| s.0 == d.name).map_or(d.default, |s| s.1));
    }
}

/// Settings the engine itself reads: the client's options, `core:mob_spawning` for worlds, and
/// a binding per key (`core:key.<action>`, from `keys`: action and default key); each sound
/// category adds its volume (`volume`).
pub fn engine_configs(keys: &[(String, String)]) -> Vec<ConfigDef> {
    let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "Player".into());
    let mut out = vec![
        volume("core:volume_master"),
        ConfigDef::text("core:language", &crate::settings::system_language()),
        ConfigDef::number("core:gui_scale", ck::INT, 0.0, 0.0, 16.0, 1.0),
        ConfigDef::number("core:view_distance", ck::INT, 12.0, 2.0, 64.0, 1.0),
        // Distant terrain beyond the view (`client::lod`), in cubes; 0 off.
        ConfigDef::number("core:lod_distance", ck::INT, 256.0, 0.0, 2048.0, 16.0),
        ConfigDef::number("core:vsync", ck::BOOL, 1.0, 0.0, 1.0, 0.0),
        ConfigDef::number("core:terrain_gpu", ck::BOOL, 1.0, 0.0, 1.0, 0.0),
        ConfigDef::number("core:terrain_check", ck::BOOL, 0.0, 0.0, 1.0, 0.0),
        ConfigDef::number("core:client_visibility", ck::INT, 0.0, 0.0, 2.0, 1.0),
        ConfigDef::number("core:fov", ck::FLOAT, 70.0, 30.0, 110.0, 1.0),
        ConfigDef::number("core:sensitivity", ck::FLOAT, 1.0, 0.2, 3.0, 0.05),
        ConfigDef::number("core:brightness", ck::FLOAT, 0.5, 0.0, 1.0, 0.0),
        ConfigDef::number("core:debug_groups", ck::INT, u32::MAX as f64, 0.0, u32::MAX as f64, 1.0),
        ConfigDef::text("core:name", &user),
        ConfigDef::text("core:relays", ""),
        ConfigDef::text("core:resource_packs", ""),
        ConfigDef::text("core:shader_pack", ""),
        ConfigDef { scope: scope::WORLD, ..ConfigDef::number("core:mob_spawning", ck::BOOL, 1.0, 0.0, 1.0, 0.0) },
        ConfigDef { scope: scope::WORLD, ..ConfigDef::number("core:simulation_distance", ck::INT, 8.0, 2.0, 64.0, 1.0) },
    ];
    out.extend(keys.iter().map(|(action, key)| ConfigDef::text(&format!("core:key.{action}"), key)));
    out
}

/// A volume setting in percent.
pub fn volume(name: &str) -> ConfigDef {
    ConfigDef::number(name, ck::INT, 100.0, 0.0, 100.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_fit_their_setting() {
        let d = ConfigDef::number("t:x", ck::INT, 5.0, 0.0, 10.0, 2.0);
        assert_eq!((d.fit(3.2), d.fit(99.0), d.fit(f64::NAN)), (4.0, 10.0, 5.0));
        let c = ConfigDef { kind: ck::CHOICE, choices: vec!["low".into(), "high".into()], ..d.clone() };
        assert_eq!((c.parse("high"), c.parse("mid"), c.show(0.0)), (Some(1.0), None, "low".into()));
        let b = ConfigDef { kind: ck::BOOL, ..d };
        assert_eq!((b.parse("on"), b.show(1.0)), (Some(1.0), "true".into()));
    }
}
