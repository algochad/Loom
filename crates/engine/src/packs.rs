//! Player-chosen client packs. Resource packs (`resourcepacks/<id>/`) are laid out like mod assets
//! per namespace (`<ns>/textures/...`, `<ns>/lang/...`, `.ctm`, GUI textures) and stack in the order the
//! player picks. A shader pack (`shaderpacks/<id>/`) replaces engine shaders file by file
//! (`world.wgsl`, `entity.wgsl`, `model.wgsl`, `particle.wgsl`, `post.wgsl`, `ui.wgsl`) and has
//! to keep their entry points (structs and bindings come from the prelude); a file that fails to
//! compile falls back to the engine's own, an asset under `core:shaders/`.
//! Snippets mods ship (`shaders/<kind>/`, see `shaders.rs`) are assets, overridden by resource
//! packs like textures. Both may have a `pack.cfg` with
//! `name = ...` and `description = ...`.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::RwLock;

use crate::assets::Assets;

use loom_render::{Shader, ShaderError};

use crate::app::{Ctx, Request};
use crate::screens::{BuildCx, Screen, button, frame, label};
use crate::ui::text::{self, Arg};
use crate::ui::{Justify, Kind, Style, Ui};

pub struct PackInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

pub fn list(dir: &Path) -> Vec<PackInfo> {
    let mut out: Vec<PackInfo> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let id = e.file_name().to_string_lossy().into_owned();
            let cfg = std::fs::read_to_string(e.path().join("pack.cfg")).unwrap_or_default();
            let get = |key: &str| crate::kv::get(&cfg, key).map(str::to_string);
            PackInfo { name: get("name").unwrap_or_else(|| id.clone()), description: get("description").unwrap_or_default(), id }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn resource_dir(run_dir: &Path) -> PathBuf {
    run_dir.join("resourcepacks")
}

pub fn shader_dir(run_dir: &Path) -> PathBuf {
    run_dir.join("shaderpacks")
}

/// Folders of the enabled resource packs that exist, highest priority first.
pub fn enabled(run_dir: &Path, ids: &[String]) -> Vec<PathBuf> {
    ids.iter().map(|id| resource_dir(run_dir).join(id)).filter(|p| p.is_dir()).collect()
}

static SHADER_PACK: RwLock<Option<PathBuf>> = RwLock::new(None);

pub fn set_shader_pack(dir: Option<PathBuf>) {
    *SHADER_PACK.write().unwrap() = dir;
}

/// The chosen pack's `name` mentions `what` (features the engine only pays for when used).
pub fn pack_uses(name: &str, what: &str) -> bool {
    pack_source(name).is_some_and(|s| s.contains(what))
}

fn pack_source(name: &str) -> Option<String> {
    let path = SHADER_PACK.read().unwrap().as_ref().map(|d| d.join(name))?;
    std::fs::read_to_string(path).ok()
}

/// An engine shader as an asset (`core:shaders/<name>`) like everything else: development runs
/// read the source tree, so edits show after a reload.
fn engine_source(assets: &Assets, name: &str) -> String {
    let asset = format!("core:{}", name.trim_end_matches(".wgsl"));
    assets.read(&asset, "shaders", "wgsl").map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|| {
        log::error!("engine shader {name} is missing from the assets");
        String::new()
    })
}

/// An engine shader no pack replaces, after `prelude`.
pub fn engine_shader(assets: &Assets, name: &str, prelude: &str) -> Result<Shader, ShaderError> {
    Shader::from_wgsl(name, &format!("{prelude}{}", engine_source(assets, name)))
}

/// What every engine shader starts with: its prelude (structs, bindings, constants) and
/// `common.wgsl` (the lightmap, fog, the structs snippets get), so a shader only writes functions.
fn with_common(assets: &Assets, name: &str, src: &str) -> String {
    format!("{}{src}", crate::gpu_layout::prelude(name, &engine_source(assets, "common.wgsl")))
}

/// `name` from the shader pack if it has a valid one, else the engine's.
pub fn shader(assets: &Assets, name: &str) -> Result<Shader, ShaderError> {
    if let Some(src) = pack_source(name) {
        match Shader::from_wgsl(name, &with_common(assets, name, &src)) {
            Ok(s) => return Ok(s),
            Err(e) => log::error!("shader pack {name}: {}", e.0),
        }
    }
    Shader::from_wgsl(name, &with_common(assets, name, &engine_source(assets, name)))
}

/// `shader` for engine shaders that take mod snippets (`shaders::compose`).
pub fn compose(assets: &Assets, name: &str, parts: &[(&crate::shaders::Hook, &[(String, String)])]) -> Result<(Shader, Vec<crate::shaders::Slots>), ShaderError> {
    if let Some(src) = pack_source(name) {
        match crate::shaders::compose(name, &with_common(assets, name, &src), parts) {
            Ok(made) => return Ok(made),
            Err(e) => log::error!("shader pack {name}: {}", e.0),
        }
    }
    crate::shaders::compose(name, &with_common(assets, name, &engine_source(assets, name)), parts)
}

pub(crate) fn open_folder(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(dir).spawn() {
        log::warn!("opening {}: {e}", dir.display());
    }
}

fn pack_row(ui: &mut Ui, cx: &BuildCx, info: &PackInfo, buttons: impl FnOnce(&mut Ui)) {
    ui.container(Style::row().gap(4.0).stretch(), |ui| {
        ui.container(Style::default().gap(1.0).grow(), |ui| {
            ui.text(cx.rich(&info.name), "default");
            if !info.description.is_empty() {
                ui.text(cx.rich(&info.description), "dim");
            }
        });
        buttons(ui);
    });
}

fn small(ui: &mut Ui, glyph: &str, action: impl Fn(&mut Ctx) + 'static) {
    ui.button("button", Style::default().w(20.0).h(20.0), Rc::new(action), |ui| {
        ui.text(text::plain(glyph), "button");
    });
}

/// Available and selected resource packs side by side; the top selected pack wins. Put into
/// effect when the screen closes (`app::Running::apply_settings`).
pub struct ResourcePacks;

impl Screen for ResourcePacks {
    fn name(&self) -> &str {
        "core:resource_packs"
    }

    fn pauses(&self) -> bool {
        true
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        let dir = resource_dir(&cx.settings.run_dir);
        let all = list(&dir);
        let selected = cx.settings.resource_packs();
        frame(ui, cx, "settings.resource_packs", |ui| {
            ui.container(Style::row().gap(8.0).grow().h(0.0).stretch(), |ui| {
                ui.container(Style::default().gap(4.0).w(260.0).stretch(), |ui| {
                    label(ui, cx, "packs.available", &[], "default");
                    ui.panel("panel", Style::default().pad(6.0).gap(4.0).stretch().grow().h(0.0).scroll(), |ui| {
                        let available: Vec<&PackInfo> = all.iter().filter(|p| !selected.contains(&p.id)).collect();
                        if available.is_empty() {
                            label(ui, cx, if all.is_empty() { "packs.none" } else { "packs.all_selected" }, &[], "dim");
                        }
                        for p in available {
                            let id = p.id.clone();
                            pack_row(ui, cx, p, |ui| {
                                small(ui, "+", move |c| {
                                    let mut p = c.settings.resource_packs();
                                    p.insert(0, id.clone());
                                    c.settings.set_resource_packs(&p);
                                    c.requests.push(Request::Rebuild);
                                });
                            });
                        }
                    });
                });
                ui.container(Style::default().gap(4.0).w(260.0).stretch(), |ui| {
                    label(ui, cx, "packs.selected", &[], "default");
                    ui.panel("panel", Style::default().pad(6.0).gap(4.0).stretch().grow().h(0.0).scroll(), |ui| {
                        for (i, id) in selected.iter().enumerate() {
                            let info = all.iter().find(|p| &p.id == id);
                            let missing = PackInfo { id: id.clone(), name: id.clone(), description: cx.lang.get("packs.missing").to_string() };
                            pack_row(ui, cx, info.unwrap_or(&missing), |ui| {
                                if i > 0 {
                                    small(ui, "\u{25b2}", move |c| {
                                        let mut p = c.settings.resource_packs();
                                        p.swap(i, i - 1);
                                        c.settings.set_resource_packs(&p);
                                        c.requests.push(Request::Rebuild);
                                    });
                                }
                                if i + 1 < selected.len() {
                                    small(ui, "\u{25bc}", move |c| {
                                        let mut p = c.settings.resource_packs();
                                        p.swap(i, i + 1);
                                        c.settings.set_resource_packs(&p);
                                        c.requests.push(Request::Rebuild);
                                    });
                                }
                                small(ui, "-", move |c| {
                                    let mut p = c.settings.resource_packs();
                                    p.remove(i);
                                    c.settings.set_resource_packs(&p);
                                    c.requests.push(Request::Rebuild);
                                });
                            });
                        }
                    });
                });
            });
            ui.container(Style::row().gap(6.0).justify(Justify::Center), |ui| {
                button(ui, cx, "packs.open_folder", &[], 140.0, move |_| open_folder(&dir));
                button(ui, cx, "menu.done", &[], 140.0, |c| c.requests.push(Request::Close));
            });
        });
    }
}

/// One shader pack or the built-in shaders.
pub struct ShaderPacks;

impl Screen for ShaderPacks {
    fn name(&self) -> &str {
        "core:shader_packs"
    }

    fn pauses(&self) -> bool {
        true
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        let dir = shader_dir(&cx.settings.run_dir);
        let all = list(&dir);
        let current = cx.settings.shader_pack();
        frame(ui, cx, "settings.shader_packs", |ui| {
            ui.panel("panel", Style::default().pad(8.0).gap(4.0).w(320.0).h(0.0).grow().min_h(60.0).scroll().stretch(), |ui| {
                let choices = std::iter::once((None, cx.t("packs.shaders_off", &[]), String::new()))
                    .chain(all.iter().map(|p| (Some(p.id.clone()), cx.rich(&p.name), p.description.clone())));
                for (id, name, description) in choices {
                    let selected = id == current;
                    let b = ui.button("button", Style::default().min_h(20.0).pad_xy(6.0, 4.0).stretch(), Rc::new(move |c: &mut Ctx| {
                        c.settings.set_text("shader_pack", id.as_deref().unwrap_or(""));
                        c.requests.push(Request::Rebuild);
                    }), |ui| {
                        ui.text(name, "button");
                        if !description.is_empty() {
                            ui.text(cx.rich(&description), "button");
                        }
                    });
                    if let Kind::Button { selected: sel, .. } = &mut ui.nodes[b].kind {
                        *sel = selected;
                    }
                }
            });
            label(ui, cx, "packs.shaders_note", &[Arg::Text("world.wgsl, entity.wgsl, model.wgsl, particle.wgsl, post.wgsl".into())], "dim");
            ui.container(Style::row().gap(6.0).justify(Justify::Center), |ui| {
                button(ui, cx, "packs.open_folder", &[], 140.0, move |_| open_folder(&dir));
                button(ui, cx, "menu.done", &[], 140.0, |c| c.requests.push(Request::Close));
            });
        });
    }
}
