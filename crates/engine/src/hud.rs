//! In-game overlay: crosshair, hotbar, debug lines, chat, and HUD parts registered by mods.
//! Rebuilt only when what it shows changes; mod parts update through their bindings.

use loom_abi::ItemStack;

use crate::app::Ctx;
use crate::client::Client;
use crate::client::modui::{self, TAG_ENTITY, TAG_SLOT};
use crate::client::textures::GameTextures;
use crate::content::content;
use crate::ui::lang::Lang;
use crate::ui::render::UiRenderer;
use crate::ui::text::{self, Arg};
use crate::ui::look::Look;
use crate::ui::{Align, Justify, Kind, Rect, Style, Ui};

pub const TAG_HOTBAR: u32 = 1;

/// One element of the HUD: the engine's hotbar or held item name, or a mod's part.
#[derive(Clone, Copy)]
enum Part {
    Hotbar,
    HeldName,
    Mod(loom_abi::ModId, loom_abi::Callback),
}
pub const TAG_CROSSHAIR: u32 = 2;

#[derive(Default)]
pub struct Hud {
    pub ui: Ui,
    key: Option<(bool, usize, usize, bool, u32, u8, bool, (u32, u32, u8))>,
    /// Player overrides from `config/hud.txt`: element name -> (anchor, order).
    layout: rustc_hash::FxHashMap<String, (String, i32)>,
}

impl Hud {
    /// Reads `config/hud.txt`: lines `element = anchor [order]`, e.g. `base:food = top_right`.
    pub fn load_layout(&mut self, run_dir: &std::path::Path) {
        self.layout.clear();
        let src = std::fs::read_to_string(run_dir.join("config").join("hud.txt")).unwrap_or_default();
        for (_, name, v) in crate::kv::pairs(&src) {
            let mut it = v.split_whitespace();
            let Some(anchor) = it.next() else { continue };
            let order = it.next().and_then(|o| o.parse().ok()).unwrap_or(0);
            self.layout.insert(name.trim().to_string(), (anchor.to_string(), order));
        }
        self.key = None;
    }

    pub fn invalidate(&mut self) {
        self.key = None;
    }

    /// Returns true if the tree was rebuilt.
    /// `chat_open`: the chat screen shows the messages itself.
    /// `groups`: bit per `DEBUG_GROUPS` entry shown with the debug overlay.
    pub fn update(&mut self, client: &Client, debug: bool, groups: u32, chat_open: bool, lang: &Lang, input: &crate::input::Input) -> bool {
        let chat_visible = if chat_open { 0 } else { client.chat.iter().filter(|(_, t)| t.elapsed().as_secs() < 10).count() };
        // The held item's name shows for two seconds after a change, fading for the last half.
        let (held, since) = client.held_name;
        let fade = if held.is_empty() { 0 } else { ((2.0 - since.elapsed().as_secs_f32()) / 0.5 * 16.0).clamp(0.0, 16.0) as u8 };
        // Narrow screens stack the debug panels, so a width change rebuilds.
        let key = (debug, chat_visible, client.chat.len(), client.dead, groups, client.mode as u8, self.ui.size.x >= 640.0, (held.item, held.data, fade));
        if self.key == Some(key) {
            return false;
        }
        self.key = Some(key);
        let c = content();
        // Elements per anchor, mod parts plus the hotbar itself, with the player's overrides.
        // The engine's own elements are placed like the mods': `config/hud.txt` moves them too.
        let engine = [("core:hotbar", loom_abi::hud_anchor::HOTBAR, Part::Hotbar), ("core:held_name", loom_abi::hud_anchor::ABOVE_HOTBAR, Part::HeldName)];
        let mut placed: Vec<(String, i32, Part)> = engine
            .into_iter()
            .map(|(n, a, p)| (n.to_string(), a.to_string(), 0, p))
            .chain(c.game.huds.iter().map(|h| (h.name.clone(), h.anchor.clone(), h.order, Part::Mod(h.owner, h.cb))))
            .map(|(name, anchor, order, part)| {
                let (anchor, order) = self.layout.get(&name).cloned().unwrap_or((anchor, order));
                (anchor, order, part)
            })
            .collect();
        placed.sort_by_key(|p| p.1);
        let mods_debug = mod_debug_groups();
        let at = |anchor: &str, upwards: bool| -> Vec<Part> {
            let mut v: Vec<_> = placed.iter().filter(|p| p.0 == anchor).map(|p| p.2).collect();
            if upwards {
                v.reverse();
            }
            v
        };
        let draw = |ui: &mut Ui, part: Part| match part {
            Part::Mod(owner, cb) => modui::build(ui, lang, Some(input), owner, cb, Some(client)),
            Part::HeldName if fade > 0 => {
                let id = ui.text(stack_name(lang, Some(input), held), "label");
                if let Kind::Text(t) = &mut ui.nodes[id].kind {
                    t.wrap = false;
                    t.color = Some(0xFFFFFF00 | (fade as u32 * 255 / 16));
                }
            }
            Part::HeldName => {}
            Part::Hotbar if client.mode == crate::world::GameMode::Spectator => {}
            Part::Hotbar => {
                ui.container(Style::row().gap(0.0), |ui| {
                    for i in 0..content().game.player_layout.hotbar as u64 {
                        ui.add(Kind::Custom { tag: TAG_HOTBAR, data: i }, Style::default().size(20.0, 20.0));
                    }
                });
            }
        };
        use loom_abi::hud_anchor as a;
        let ui = &mut self.ui;
        ui.clear();
        ui.root_style(Style::default().stretch());
        // Both halves start at zero height and split the rest evenly, so the crosshair sits at the
        // exact screen center whatever they contain.
        ui.container(Style::row().h(0.0).grow().pad(4.0).align(Align::Start), |ui| {
            // Half the screen each, so wide text on one side never pushes the other off screen
            // (the two may overlap on narrow screens, like vanilla's).
            ui.container(Style::default().w(0.0).grow().gap(1.0).align(Align::Start), |ui| {
                if debug {
                    debug_groups(ui, groups, false, &mods_debug, &|ui, (o, cb)| draw(ui, Part::Mod(o, cb)));
                }
                for p in at(a::TOP_LEFT, false) {
                    draw(ui, p);
                }
            });
            ui.container(Style::default().w(0.0).grow().gap(1.0).align(Align::End), |ui| {
                if debug {
                    debug_groups(ui, groups, true, &mods_debug, &|ui, (o, cb)| draw(ui, Part::Mod(o, cb)));
                }
                for p in at(a::TOP_RIGHT, false) {
                    draw(ui, p);
                }
            });
        });
        ui.add(Kind::Custom { tag: TAG_CROSSHAIR, data: 0 }, Style::default().size(9.0, 9.0).self_align(Align::Center));
        ui.container(Style::default().h(0.0).grow().justify(Justify::End).align(Align::Stretch).pad(2.0).gap(1.0), |ui| {
            if chat_visible > 0 {
                crate::game_screens::chat_lines(ui, client, lang, input, false);
            }
            ui.container(Style::row().align(Align::End), |ui| {
                ui.container(Style::default().w(0.0).gap(1.0).align(Align::Start), |ui| {
                    for p in at(a::BOTTOM_LEFT, true) {
                        draw(ui, p);
                    }
                });
                ui.container(Style::default().grow(), |_| {});
                ui.container(Style::default().gap(1.0).align(Align::Center), |ui| {
                    for p in at(a::ABOVE_HOTBAR, true) {
                        draw(ui, p);
                    }
                    ui.container(Style::row().w(182.0).gap(2.0).align(Align::End), |ui| {
                        ui.container(Style::default().gap(1.0).align(Align::Start), |ui| {
                            for p in at(a::HOTBAR_LEFT, true) {
                                draw(ui, p);
                            }
                        });
                        ui.container(Style::default().grow(), |_| {});
                        ui.container(Style::default().gap(1.0).align(Align::End), |ui| {
                            for p in at(a::HOTBAR_RIGHT, true) {
                                draw(ui, p);
                            }
                        });
                    });
                    for p in at(a::HOTBAR, true) {
                        draw(ui, p);
                    }
                });
                ui.container(Style::default().grow(), |_| {});
                ui.container(Style::default().w(0.0).gap(1.0).align(Align::End), |ui| {
                    for p in at(a::BOTTOM_RIGHT, true) {
                        draw(ui, p);
                    }
                });
            });
        });
        true
    }
}


type DebugLine = (&'static str, fn(&Ctx) -> Vec<Arg>);

type Panels = &'static [(&'static str, &'static [DebugLine])];

/// Debug overlay groups in F3+number order: title key (for the help), right side, and its
/// panels (title, lines), side by side.
pub const DEBUG_GROUPS: &[(&str, bool, Panels)] = &[
    ("hud.debug.group.performance", true, &[("hud.debug.group.cpu", CPU), ("hud.debug.group.gpu", GPU)]),
    ("hud.debug.group.position", false, &[("hud.debug.group.position", POSITION)]),
    ("hud.debug.group.world", false, &[("hud.debug.group.world", WORLD)]),
    ("hud.debug.group.mods", true, &[("hud.debug.group.mods", MODS)]),
    ("hud.debug.group.system", true, &[("hud.debug.group.system", SYSTEM)]),
];

fn cpu(c: &Ctx) -> Vec<Arg> {
    c.stats.cpu.iter().map(|v| Arg::Float(*v as f64, 2)).collect()
}

fn gpu(c: &Ctx) -> Vec<Arg> {
    let g = &c.stats.gpu;
    let at = |i: usize| Arg::Float(g.get(i).copied().unwrap_or(0.0) as f64, 2);
    vec![Arg::Float(g.iter().sum::<f32>() as f64, 2), at(1), at(2), at(3)]
}

// Short lines: the two panels stand side by side, also at GUI scale 3 in Russian.
const CPU: &[DebugLine] = &[
    ("hud.debug.fps", |c| vec![Arg::Int(c.stats.fps.round() as i64), Arg::Float(c.stats.frame_ms as f64, 2)]),
    ("hud.debug.cpu", cpu),
    ("hud.debug.cpu_submit", cpu),
    ("hud.debug.server", |c| vec![Arg::Float(c.stats.server_ms as f64, 2), Arg::Int(c.stats.regions as i64)]),
    ("hud.debug.phases", |c| {
        let p = c.stats.phases;
        vec![Arg::Float(p[0] as f64, 2), Arg::Float(p[2] as f64, 2), Arg::Float((p[3] + p[4]) as f64, 2)]
    }),
];

const GPU: &[DebugLine] = &[("hud.debug.gpu", gpu), ("hud.debug.gpu_world", gpu), ("hud.debug.gpu_ui", gpu)];

const POSITION: &[DebugLine] = &[
    ("hud.debug.pos", |c| {
        let p = c.client.as_ref().map_or(glam::DVec3::ZERO, |cl| cl.player.pos);
        vec![Arg::Float(p.x, 3), Arg::Float(p.y, 3), Arg::Float(p.z, 3)]
    }),
    ("hud.debug.block", |c| {
        let p = c.client.as_ref().map_or(glam::IVec3::ZERO, |cl| cl.player.pos.floor().as_ivec3());
        vec![Arg::Int(p.x as i64), Arg::Int(p.y as i64), Arg::Int(p.z as i64)]
    }),
    ("hud.debug.cube", |c| {
        let p = c.client.as_ref().map_or(glam::IVec3::ZERO, |cl| cl.player.pos.floor().as_ivec3() >> 4i32);
        vec![Arg::Int(p.x as i64), Arg::Int(p.y as i64), Arg::Int(p.z as i64)]
    }),
    ("hud.debug.facing", |c| {
        let (yaw, pitch) = c.client.as_ref().map_or((0.0, 0.0), |cl| (cl.player.yaw, cl.player.pitch));
        // Yaw 0 looks along -Z (north); turning right goes to east (+X).
        let deg = (-yaw.to_degrees()).rem_euclid(360.0);
        let dir = ["north", "east", "south", "west"][(((deg + 45.0) / 90.0) as usize) % 4];
        vec![Arg::Key(format!("hud.debug.dir.{dir}")), Arg::Float(deg as f64, 1), Arg::Float(pitch.to_degrees() as f64, 1)]
    }),
    ("hud.debug.biome", |c| {
        let Some(cl) = c.client.as_ref() else { return vec![Arg::Text("-".into())] };
        let p = cl.player.pos.floor().as_ivec3();
        let id = cl.world.cube(p >> 4i32).map_or(0, |cube| cube.biome((p.x & 15) as usize, (p.z & 15) as usize));
        vec![Arg::Text(content().biomes.get(id as usize).map_or("-".into(), |b| b.name.clone()))]
    }),
    ("hud.debug.dimension", |c| {
        let d = c.client.as_ref().map_or(0, |cl| cl.world.dim);
        vec![Arg::Text(content().dims[d as usize].name.clone())]
    }),
    ("hud.debug.light", |c| {
        let l = c.client.as_ref().map_or(0, |cl| cl.world.light((cl.player.pos + glam::DVec3::Y * 0.5).floor().as_ivec3()));
        let ch = |s: u32| Arg::Int(((l >> s) & 255) as i64);
        vec![ch(0), ch(8), ch(16), ch(24)]
    }),
    ("hud.debug.looking", |c| {
        let name = c.client.as_ref().and_then(|cl| cl.target).map(|h| content().state_name(h.state)).unwrap_or_else(|| "-".into());
        vec![Arg::Text(name)]
    }),
];

const WORLD: &[DebugLine] = &[
    ("hud.debug.world", |c| {
        let Some(cl) = c.client.as_ref() else { return vec![] };
        vec![Arg::Int(cl.world.len() as i64), Arg::Int(cl.pending_meshes() as i64)]
    }),
    ("hud.debug.draws", |c| {
        let Some(cl) = c.client.as_ref() else { return vec![] };
        vec![Arg::Int(cl.render.drawn as i64), Arg::Int((cl.render.faces_drawn / 1000) as i64)]
    }),
    ("hud.debug.lod", |c| {
        let Some(cl) = c.client.as_ref() else { return vec![] };
        let (shown, made, quads, pending) = cl.lod_stats();
        vec![Arg::Int(shown as i64), Arg::Int(made as i64), Arg::Int((quads / 1000) as i64), Arg::Int(pending as i64)]
    }),
    ("hud.debug.entities", |c| vec![Arg::Int(c.client.as_ref().map_or(0, |cl| cl.entities.len()) as i64), Arg::Int(c.client.as_ref().map_or(0, |cl| cl.particles.len()) as i64)]),
    ("hud.debug.load", |c| vec![Arg::Int(c.stats.load.0 as i64), Arg::Int(c.stats.load.1 as i64)]),
];

const MODS: &[DebugLine] = &[("", |_| vec![Arg::Text(mod_profile())])];

const SYSTEM: &[DebugLine] = &[
    ("hud.debug.gpu_name", |c| vec![Arg::Text(c.stats.gpu_name.clone())]),
    ("hud.debug.cpu_name", |c| vec![Arg::Text(c.stats.cpu_name.clone()), Arg::Int(c.stats.threads as i64)]),
    ("hud.debug.memory", |c| vec![Arg::Text(crate::diag::size(crate::diag::memory())), Arg::Text(crate::diag::size(c.stats.memory_total))]),
    ("hud.debug.window", |c| vec![Arg::Int(c.stats.window.0 as i64), Arg::Int(c.stats.window.1 as i64), Arg::Float(c.stats.gui_scale as f64, 1)]),
];

/// Milliseconds per server tick and calls per tick for each mod, averaged over the last second.
fn mod_profile() -> String {
    use std::sync::Mutex;
    static LAST: Mutex<Option<(std::time::Instant, String, crate::diag::ModWindow)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    let (at, text, window) = last.get_or_insert_with(|| (std::time::Instant::now() - std::time::Duration::from_secs(2), String::new(), Default::default()));
    if at.elapsed().as_secs_f32() < 1.0 {
        return text.clone();
    }
    let (mods, secs) = window.take();
    let ticks = (secs * crate::world::TPS as f64).max(1.0);
    let lines: Vec<String> = mods.iter().map(|(id, ns, calls)| format!("{id}: {:.2} ms, {:.0} calls", *ns as f64 / 1e6 / ticks, *calls as f64 / ticks)).collect();
    *text = lines.join("
");
    *at = std::time::Instant::now();
    text.clone()
}

/// Engine-drawn nodes: crosshair, hotbar slots, menu slots.
#[allow(clippy::too_many_arguments)]
pub fn draw_custom(
    ren: &mut UiRenderer,
    look: &Look,
    textures: &GameTextures,
    client: Option<&Client>,
    tag: u32,
    data: u64,
    r: Rect,
    hovered: bool,
    icon: Option<u32>,
    scale: f32,
    text_unit: f32,
) {
    match tag {
        TAG_CROSSHAIR => loaded(ren, look, "crosshair", r, scale),
        TAG_HOTBAR => {
            let Some(client) = client else { return };
            let selected = client.selected as u64 == data;
            slot_background(ren, look, "hotbar", r, selected, scale);
            let stack = client.inventory.get(data as usize).copied().unwrap_or_default();
            stack_icon(ren, textures, r, stack, scale, text_unit);
        }
        TAG_ENTITY => {
            // Inset frame; the model itself is drawn in a later pass.
            loaded(ren, look, "well", r, scale);
        }
        modui::TAG_ICON => stack_icon(ren, textures, r, ItemStack::new(data as u32, 1), scale, text_unit),
        modui::TAG_CREATIVE => {
            slot_background(ren, look, "slot", r, false, scale);
            stack_icon(ren, textures, r, ItemStack::new(data as u32, 1), scale, text_unit);
            if hovered {
                loaded(ren, look, "slot_highlight", Rect { x: r.x + scale, y: r.y + scale, w: r.w - 2.0 * scale, h: r.h - 2.0 * scale }, scale);
            }
        }
        TAG_SLOT => {
            let Some(client) = client else { return };
            slot_background(ren, look, "slot", r, false, scale);
            let stack = client.menu.as_ref().and_then(|m| m.slots.get(data as usize)).copied().unwrap_or_default();
            if let (Some(t), true) = (icon, stack.is_empty()) {
                let s = 16.0 * scale;
                ren.image(Rect { x: r.x + (r.w - s) / 2.0, y: r.y + (r.h - s) / 2.0, w: s, h: s }, t, [0.0, 0.0, 1.0, 1.0], 0xFFFFFFFF);
            }
            stack_icon(ren, textures, r, stack, scale, text_unit);
            let dragged = client.drag.as_ref().is_some_and(|d| d.1.contains(&(data as u32)));
            if hovered || dragged {
                loaded(ren, look, "slot_highlight", Rect { x: r.x + scale, y: r.y + scale, w: r.w - 2.0 * scale, h: r.h - 2.0 * scale }, scale);
            }
        }
        _ => {}
    }
}

/// A stack in `r` (`UiRenderer::stack_icon`).
pub fn stack_icon(ren: &mut UiRenderer, textures: &GameTextures, r: Rect, stack: ItemStack, scale: f32, text_unit: f32) {
    ren.stack_icon(content(), textures, r, stack, scale, text_unit);
}

/// A slot of class `class` (`slot` in menus, `hotbar` on the HUD), with `hotbar_selected` around
/// it when selected.
pub fn slot_background(ren: &mut UiRenderer, look: &Look, class: &str, r: Rect, selected: bool, scale: f32) {
    loaded(ren, look, class, r, scale);
    if selected {
        loaded(ren, look, "hotbar_selected", Rect { x: r.x - scale, y: r.y - scale, w: r.w + 2.0 * scale, h: r.h + 2.0 * scale }, scale);
    }
}

/// A class's texture already loaded for engine-drawn nodes, else the missing-texture checkerboard.
fn loaded(ren: &mut UiRenderer, look: &Look, class: &str, r: Rect, scale: f32) {
    if !look.texture(class, "").is_some_and(|t| ren.draw_loaded(&t, r, scale, 0xFFFFFFFF)) {
        ren.checker(r, scale);
    }
}


/// What a stack is called: its own name, else its item's.
pub fn stack_name(lang: &Lang, input: Option<&crate::input::Input>, stack: loom_abi::ItemStack) -> Vec<text::Span> {
    match crate::stack::name(&stack) {
        Some(n) => text::rich(&text::escape(&n), lang, input),
        None => text::Name::Item(stack.item).spans(lang, input),
    }
}

/// The name, then the `<name key>.tooltip` translation in gray (details usually behind
/// `{on_key:core:sneak}`) and the mods' own lines under it, then durability for damaged tools.
pub fn item_tooltip(lang: &Lang, input: &crate::input::Input, stack: loom_abi::ItemStack) -> Vec<text::Span> {
    let gray = lang.color("color.tooltip_dim").map(|c| c >> 8);
    let cx = text::Ctx::new(lang, Some(input));
    let name = text::Name::Item(stack.item);
    let mut out = stack_name(lang, Some(input), stack);
    let line = |template: &str, args: &[text::Arg], out: &mut Vec<text::Span>| {
        out.push(text::Span::text("\n", None));
        let start = out.len();
        text::format(template, args, &cx, out);
        for s in &mut out[start..] {
            s.color = s.color.or(gray);
        }
    };
    if let Some(key) = name.key(lang).map(|k| format!("{k}.tooltip")) {
        if lang.has(&key) {
            line(lang.get(&key), &[], &mut out);
        }
        // Every mod can add lines of its own under `<tooltip key>.<mod id>`, in load order.
        let mut mods: Vec<_> = content().mods.iter().collect();
        mods.sort_by_key(|m| m.order);
        for m in mods {
            let extra = format!("{key}.{}", m.id);
            if lang.has(&extra) {
                line(lang.get(&extra), &[], &mut out);
            }
        }
    }
    let max = content().items.get(stack.item as usize).map_or(0, |d| d.durability);
    let damage = crate::stack::damage(&stack);
    if max > 0 && damage > 0 {
        line(lang.get("tooltip.durability"), &[text::Arg::Int((max - damage.min(max)) as i64), text::Arg::Int(max as i64)], &mut out);
    }
    out
}

type ModPart = (loom_abi::ModId, loom_abi::Callback);

/// Mods' F3 panels in group order (after the engine's): title key, right side, part.
pub fn mod_debug_groups() -> Vec<(&'static str, bool, ModPart)> {
    use loom_abi::hud_anchor as a;
    let mut v: Vec<_> = content().game.huds.iter().filter(|h| h.anchor == a::DEBUG_LEFT || h.anchor == a::DEBUG_RIGHT).collect();
    v.sort_by_key(|h| h.order);
    v.into_iter().take(9 - DEBUG_GROUPS.len()).map(|h| (h.title.as_str(), h.anchor == a::DEBUG_RIGHT, (h.owner, h.cb))).collect()
}

fn panel_title(ui: &mut Ui, key: String) {
    let t = ui.text_bound("debug_title", Box::new(move |_: &Ctx| (key.clone(), vec![])));
    if let Some(t) = ui.text_mut(t) {
        t.wrap = false;
    }
}

/// The debug overlay groups of one side, each a dark panel with a title; the help panel on the
/// left lists the F3 combinations.
fn debug_groups(ui: &mut Ui, groups: u32, right: bool, mods: &[(&'static str, bool, ModPart)], draw: &dyn Fn(&mut Ui, ModPart)) {
    let align = if right { Align::End } else { Align::Start };
    for (i, (_, side, panels)) in DEBUG_GROUPS.iter().enumerate() {
        if *side != right || groups & (1 << i) == 0 {
            continue;
        }
        // Side by side where they fit (GUI scale 3 in Russian does not), else stacked.
        let style = if ui.size.x >= 640.0 { Style::row().gap(2.0).align(Align::Start) } else { Style::default().gap(2.0).align(align) };
        ui.container(style, |ui| {
            for (title, lines) in panels.iter() {
                ui.container(Style::default().pad(2.0).gap(0.0).align(align), |ui| {
                    ui.nodes.last_mut().unwrap().kind = Kind::Container { class: Some("debug_panel".into()) };
                    panel_title(ui, title.to_string());
                    for (key, f) in lines.iter() {
                        let key = key.to_string();
                        let n = ui.text_bound("debug", Box::new(move |c: &Ctx| (key.clone(), f(c))));
                        if let Some(t) = ui.text_mut(n) {
                            t.wrap = false;
                            t.mono = true;
                        }
                    }
                });
            }
        });
    }
    for (i, (name, side, part)) in mods.iter().enumerate() {
        if *side != right || groups & (1 << (DEBUG_GROUPS.len() + i)) == 0 {
            continue;
        }
        ui.container(Style::default().pad(2.0).gap(0.0).align(align), |ui| {
            ui.nodes.last_mut().unwrap().kind = Kind::Container { class: Some("debug_panel".into()) };
            panel_title(ui, name.to_string());
            let first = ui.nodes.len();
            draw(ui, *part);
            // The engine's panel style for every line: as wide as it needs, digits in cells.
            for n in &mut ui.nodes[first..] {
                if let Kind::Text(t) = &mut n.kind {
                    t.wrap = false;
                    t.mono = true;
                }
            }
        });
    }
    if !right {
        ui.container(Style::default().pad(2.0).gap(0.0).align(align), |ui| {
            ui.nodes.last_mut().unwrap().kind = Kind::Container { class: Some("debug_panel".into()) };
            panel_title(ui, "hud.debug.group.help".into());
            let titles = DEBUG_GROUPS.iter().map(|g| g.0).chain(mods.iter().map(|m| m.0));
            for (i, title) in titles.enumerate() {
                let on = groups & (1 << i) != 0;
                let state = if on { "hud.debug.shown" } else { "hud.debug.hidden" };
                let n = ui.text_bound(
                    "debug",
                    Box::new(move |_: &Ctx| ("hud.debug.toggle".into(), vec![Arg::Int(i as i64 + 1), Arg::Key(title.to_string()), Arg::Key(state.into())])),
                );
                if let Some(t) = ui.text_mut(n) {
                    t.wrap = false;
                }
            }
            for key in ["hud.debug.gamemode", "hud.debug.cubes", "hud.debug.hitboxes"] {
                let n = ui.text_bound("debug", Box::new(move |_: &Ctx| (key.into(), vec![])));
                if let Some(t) = ui.text_mut(n) {
                    t.wrap = false;
                }
            }
        });
    }
}
