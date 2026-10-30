//! Engine shaders extended by snippets that mods and resource packs ship as assets,
//! `shaders/<kind>/<name>.wgsl`: text effects (`fx`), GUI fills (`gui`), particles (`particle`),
//! entities (`entity`) and screen effects (`post`). A snippet defines one function named after
//! its kind's hook (`fn effect(g: Glyph) -> Glyph` for text ...); the engine renames it per
//! snippet and calls it through a generated `switch`, so all snippets of a kind share one
//! pipeline and are chosen per quad, particle or entity by a number.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use loom_render::{Shader, ShaderError};

use crate::assets::Assets;

static START: OnceLock<Instant> = OnceLock::new();
static PINNED: AtomicBool = AtomicBool::new(false);

/// Seconds the shaders animate by (text effects, GUI fills, particles, entities, screen effects).
/// Automated runs pin it, so their screenshots come out the same every time.
pub fn time() -> f32 {
    if PINNED.load(Ordering::Relaxed) { 1.0 } else { START.get_or_init(Instant::now).elapsed().as_secs_f32() }
}

/// Real seconds since start, never pinned.
pub fn clock() -> f32 {
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

pub fn pinned() -> bool {
    PINNED.load(Ordering::Relaxed)
}

pub fn pin_time(on: bool) {
    PINNED.store(on, Ordering::Relaxed);
}

/// How snippets of one kind plug into their base shader: the base calls `dispatch(slot, ...)`.
pub struct Hook {
    /// Folder under `shaders/` and the function every snippet defines.
    pub kind: &'static str,
    pub entry: &'static str,
    pub dispatch: &'static str,
    /// Parameters after `slot`, the call arguments and the return type.
    pub params: &'static str,
    pub args: &'static str,
    pub ret: &'static str,
    /// Body for slot 0 and unknown slots.
    pub fallback: &'static str,
}

/// Names of the snippets a composed shader has, per hook: `names[i]` is slot `i + 1`.
#[derive(Default, Clone)]
pub struct Slots {
    pub names: Vec<String>,
}

impl Slots {
    /// Slot of a snippet (`ns:name`), 0 if it is not there.
    pub fn slot(&self, name: &str) -> u32 {
        self.names.iter().position(|n| n == name).map_or(0, |i| i as u32 + 1)
    }
}

/// Every snippet of `hook.kind` in the mods and enabled packs, as (`ns:name`, source).
pub fn snippets(assets: &Assets, hook: &Hook) -> Vec<(String, String)> {
    let prefix = format!("{}/", hook.kind);
    assets
        .list("shaders", "wgsl")
        .into_iter()
        .filter_map(|(name, path)| {
            let (ns, rel) = name.split_once(':')?;
            let short = rel.strip_prefix(&prefix)?;
            Some((format!("{ns}:{short}"), std::fs::read_to_string(path).ok()?))
        })
        .collect()
}

type Snippet<'a> = (usize, &'a (String, String));

fn source(base: &str, hooks: &[&Hook], chosen: &[Snippet]) -> String {
    let mut out = String::from(base);
    for (h, hook) in hooks.iter().enumerate() {
        let mut cases = String::new();
        for (i, (_, (_, src))) in chosen.iter().filter(|s| s.0 == h).enumerate() {
            let slot = i + 1;
            out.push('\n');
            out.push_str(&src.replacen(&format!("fn {}(", hook.entry), &format!("fn {}_{slot}(", hook.entry), 1));
            cases.push_str(&format!("        case {slot}u: {{ return {}_{slot}({}); }}\n", hook.entry, hook.args));
        }
        out.push_str(&format!(
            "\nfn {}(slot: u32, {}) -> {} {{\n    switch slot {{\n{cases}        default: {{ {} }}\n    }}\n}}\n",
            hook.dispatch, hook.params, hook.ret, hook.fallback
        ));
    }
    out
}

/// Compiles `base` with the snippets of each hook. A snippet that does not compile (or clashes
/// with another) is left out with an error naming it; the rest keep working.
pub fn compose(label: &str, base: &str, parts: &[(&Hook, &[(String, String)])]) -> Result<(Shader, Vec<Slots>), ShaderError> {
    let hooks: Vec<&Hook> = parts.iter().map(|p| p.0).collect();
    let mut all: Vec<Snippet> = Vec::new();
    for (h, (hook, list)) in parts.iter().enumerate() {
        for s in list.iter() {
            if s.1.contains(&format!("fn {}(", hook.entry)) {
                all.push((h, s));
            } else {
                log::error!("shader {} ({}): has no `fn {}(...)`", s.0, hook.kind, hook.entry);
            }
        }
    }
    let slots = |chosen: &[Snippet]| -> Vec<Slots> { (0..hooks.len()).map(|h| Slots { names: chosen.iter().filter(|s| s.0 == h).map(|s| s.1.0.clone()).collect() }).collect() };
    if let Ok(shader) = Shader::from_wgsl(label, &source(base, &hooks, &all)) {
        return Ok((shader, slots(&all)));
    }
    // Find the culprits one by one, keeping those that work together with the ones before.
    let mut kept: Vec<Snippet> = Vec::new();
    for s in all {
        let mut trial = kept.clone();
        trial.push(s);
        match Shader::from_wgsl(label, &source(base, &hooks, &trial)) {
            Ok(_) => kept = trial,
            Err(e) => log::error!("shader {} ({}) left out: {}", s.1.0, hooks[s.0].kind, e.0),
        }
    }
    let shader = Shader::from_wgsl(label, &source(base, &hooks, &kept))?;
    Ok((shader, slots(&kept)))
}
