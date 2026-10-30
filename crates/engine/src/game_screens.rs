//! In-game screens: container menus (built by the mod that registered the menu), death, chat.

use std::cell::RefCell;
use std::rc::Rc;

use winit::keyboard::KeyCode;

use crate::app::{Ctx, Request};
use crate::input::Key;
use crate::ui::text::{self, Span};
use crate::client::modui;
use crate::content::content;
use crate::screens::{BuildCx, Screen, button, label};
use crate::ui::{Kind, Style, Ui};

pub struct MenuScreen {
    pub kind: u32,
    /// Creative inventory filter and open tab.
    search: Rc<RefCell<String>>,
    tab: Rc<std::cell::Cell<usize>>,
}

impl MenuScreen {
    pub fn new(kind: u32) -> MenuScreen {
        MenuScreen { kind, search: Default::default(), tab: Default::default() }
    }
}

fn tab_title(cx: &BuildCx, name: &str) -> Vec<Span> {
    text::Name::Tab(name.to_string()).spans(cx.lang, Some(cx.input))
}

/// Tab row, search field and the items of the open tab (or of every tab matching the search) as
/// a scrolling grid of creative entries.
fn creative_panel(ui: &mut Ui, cx: &BuildCx, search: &Rc<RefCell<String>>, tab: &Rc<std::cell::Cell<usize>>) {
    let c = content();
    let filter = search.borrow().to_lowercase();
    let open = tab.get().min(c.creative_tabs.len().saturating_sub(1));
    let items: Vec<u32> = if filter.is_empty() {
        c.creative_tabs.get(open).map_or(Vec::new(), |t| t.items.clone())
    } else {
        c.creative_tabs
            .iter()
            .flat_map(|t| t.items.iter().copied())
            .filter(|&i| c.items[i as usize].name.to_lowercase().contains(&filter) || text::Name::Item(i).text(cx.lang).to_lowercase().contains(&filter))
            .collect()
    };
    ui.panel("panel", Style::default().pad(7.0).gap(4.0), |ui| {
        ui.container(Style::row().gap(1.0), |ui| {
            for (i, t) in c.creative_tabs.iter().enumerate() {
                let tab = tab.clone();
                let b = ui.button("button", Style::default().size(20.0, 20.0), Rc::new(move |ctx: &mut Ctx| {
                    tab.set(i);
                    ctx.requests.push(Request::Rebuild);
                }), |ui| {
                    ui.add(Kind::Custom { tag: modui::TAG_ICON, data: t.icon as u64 }, Style::default().size(16.0, 16.0));
                });
                if let Kind::Button { selected, .. } = &mut ui.nodes[b].kind {
                    *selected = i == open && filter.is_empty();
                }
                ui.nodes[b].tooltip = Some(tab_title(cx, &t.name));
            }
        });
        if let Some(t) = c.creative_tabs.get(open).filter(|_| filter.is_empty()) {
            ui.text(tab_title(cx, &t.name), "default");
        }
        let s = search.clone();
        let input = ui.add(
            Kind::Input {
                value: search.borrow().clone(),
                caret: search.borrow().chars().count(),
                max: 32,
                on_change: Some(Rc::new(move |ctx: &mut Ctx, v: &str| {
                    *s.borrow_mut() = v.to_string();
                    ctx.requests.push(Request::Rebuild);
                })),
            },
            Style::default().h(14.0).w(162.0),
        );
        ui.set_id(input, "creative_search");
        let grid = ui.container(Style::default().w(162.0).h(18.0 * 8.0).scroll(), |ui| {
            for row in items.chunks(9) {
                ui.container(Style::row(), |ui| {
                    for &i in row {
                        ui.add(Kind::Custom { tag: modui::TAG_CREATIVE, data: i as u64 }, Style::default().size(18.0, 18.0));
                    }
                });
            }
        });
        ui.set_id(grid, "creative_grid");
    });
}

impl Screen for MenuScreen {
    fn name(&self) -> &str {
        "core:menu"
    }

    fn shows_menu(&self) -> bool {
        true
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        let c = content();
        ui.root_style(Style::default().stretch());
        let creative = cx.client.is_some_and(|cl| cl.mode == crate::world::GameMode::Creative) && c.game.player_menu == Some(self.kind);
        ui.panel("overlay", Style::default().grow().center(), |ui| {
            ui.container(Style::row().gap(4.0).align(crate::ui::Align::Start), |ui| {
                if creative {
                    creative_panel(ui, cx, &self.search, &self.tab);
                }
                if let Some(def) = c.game.menus.get(self.kind as usize) {
                    if let Some(cb) = def.screen {
                        let first = ui.nodes.len();
                        modui::build(ui, cx.lang, Some(cx.input), def.owner, cb, cx.client);
                        // The arrows of a menu that makes recipes open them.
                        if !def.recipes.is_empty() {
                            let tip = cx.t("recipes.show", &[]);
                            for n in &mut ui.nodes[first..] {
                                if n.recipes {
                                    n.tooltip = Some(tip.clone());
                                }
                            }
                        }
                    }
                }
            });
        });
    }
}

pub struct Death;

impl Screen for Death {
    fn name(&self) -> &str {
        "core:death"
    }

    fn escape_closes(&self) -> bool {
        false
    }

    fn closes_on_respawn(&self) -> bool {
        true
    }

    /// Built from node descriptors like a mod's screen, so a replacement can match it.
    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        use crate::ui::desc;
        use loom_abi as abi;
        desc::build(ui, cx, |d| {
            let panel = abi::UiNode { class: abi::Str::new("death"), grow: 1.0, align: abi::ui_align::CENTER, justify: abi::ui_justify::CENTER, gap: 12.0, ..desc::node(abi::ui_kind::PANEL) };
            d.open(&panel, |d| {
                d.title("death.title");
                d.button("death.respawn", &[], 200.0, false, |r| r.handle(vec![Request::Respawn]));
                d.button("menu.save_quit", &[], 200.0, false, |r| r.handle(vec![Request::LeaveWorld]));
            });
        });
    }
}

/// The chat's messages: the last 20 in the open chat, otherwise the last 10 of those under ten
/// seconds old (the HUD).
pub fn chat_lines(ui: &mut Ui, client: &crate::client::Client, lang: &crate::ui::lang::Lang, input: &crate::input::Input, open: bool) {
    let now = std::time::Instant::now();
    ui.container(Style::default().gap(1.0).pad(2.0).align(crate::ui::Align::Start).self_align(crate::ui::Align::Start), |ui| {
        for (line, at) in client.chat.iter().rev().take(if open { 20 } else { 10 }).rev() {
            if open || now.duration_since(*at).as_secs() < 10 {
                ui.text(line.spans(lang, Some(input)), "chat");
            }
        }
    });
}

/// Chat input with the recent messages above it and, for commands, completions of the word being
/// typed. Tab takes the highlighted one and further presses go on through the same list; the arrows
/// browse sent lines, or the list once Tab started.
pub struct Chat {
    state: Rc<RefCell<ChatState>>,
}

#[derive(Default)]
struct ChatState {
    text: String,
    completion: Option<crate::commands::Completion>,
    selected: usize,
    /// Position in the sent-lines history while browsing it with the arrows.
    history: Option<usize>,
    /// The text as typed before Tab: further presses go on through its suggestions.
    stem: Option<String>,
}

impl Chat {
    /// Opened with `/` the command list shows at once.
    pub fn new(text: &str) -> Chat {
        let mut st = ChatState::default();
        st.update(text, &[]);
        Chat { state: Rc::new(RefCell::new(st)) }
    }
}

impl ChatState {
    fn update(&mut self, text: &str, players: &[String]) {
        self.text = text.to_string();
        self.selected = 0;
        self.stem = None;
        self.completion = text.strip_prefix('/').map(|line| {
            let mut comp = crate::commands::complete(content(), line, players);
            comp.from += 1;
            comp
        });
    }
}

impl Screen for Chat {
    fn name(&self) -> &str {
        "core:chat"
    }

    fn shows_chat(&self) -> bool {
        true
    }

    fn keys_first(&self) -> bool {
        true
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        let st = self.state.borrow();
        ui.root_style(Style::default().stretch());
        ui.container(Style::default().grow().pad(4.0).gap(1.0).justify(crate::ui::Justify::End).stretch(), |ui| {
            if let Some(client) = cx.client {
                chat_lines(ui, client, cx.lang, cx.input, true);
            }
            if let Some(comp) = &st.completion {
                if !comp.suggestions.is_empty() {
                    // A window of ten around the highlighted suggestion.
                    let first = st.selected.saturating_sub(9);
                    ui.panel("suggest", Style::default().pad(2.0).self_align(crate::ui::Align::Start), |ui| {
                        for (i, s) in comp.suggestions.iter().enumerate().skip(first).take(10) {
                            let color = cx.lang.color(if i == st.selected { "color.suggest" } else { "color.suggest_dim" }).map(|c| c >> 8);
                            ui.text(vec![Span::text(s.clone(), color)], "chat");
                        }
                    });
                } else if !comp.hint.is_empty() {
                    ui.container(Style::default().pad(2.0).self_align(crate::ui::Align::Start), |ui| {
                        ui.text(vec![Span::text(comp.hint.clone(), cx.lang.color("color.suggest_hint").map(|c| c >> 8))], "chat");
                    });
                }
            }
            let state = self.state.clone();
            let input = ui.add(
                Kind::Input {
                    value: st.text.clone(),
                    caret: st.text.chars().count(),
                    max: 256,
                    on_change: Some(Rc::new(move |ctx: &mut Ctx, v: &str| {
                        let players = ctx.client.as_ref().map(|c| c.players.clone()).unwrap_or_default();
                        let mut s = state.borrow_mut();
                        s.update(v, &players);
                        s.history = None;
                        ctx.requests.push(Request::Rebuild);
                    })),
                },
                Style::default().h(14.0),
            );
            ui.set_id(input, "chat");
            ui.focused = Some(input);
        });
    }

    fn key(&mut self, key: Key, ctx: &mut Ctx) -> bool {
        let players = ctx.client.as_ref().map(|c| c.players.clone()).unwrap_or_default();
        let sent = ctx.client.as_ref().map(|c| c.sent.clone()).unwrap_or_default();
        let mut st = self.state.borrow_mut();
        let n = st.completion.as_ref().map_or(0, |c| c.suggestions.len());
        let set = |st: &mut ChatState, text: String, ctx: &mut Ctx| {
            st.update(&text, &players);
            ctx.set_input("chat", text);
            ctx.requests.push(Request::Rebuild);
        };
        // Puts the highlighted suggestion of the stem's list into the input, keeping the list.
        let take = |st: &mut ChatState, ctx: &mut Ctx| {
            let stem = st.stem.get_or_insert_with(|| st.text.clone()).clone();
            let comp = st.completion.as_ref().unwrap();
            let text = format!("{}{}", &stem[..comp.from.min(stem.len())], comp.suggestions[st.selected.min(n - 1)]);
            st.text = text.clone();
            ctx.set_input("chat", text);
            ctx.requests.push(Request::Rebuild);
        };
        match key {
            Key::Code(KeyCode::Tab) if n > 0 => {
                if st.stem.is_some() {
                    st.selected = (st.selected + 1) % n;
                }
                take(&mut st, ctx);
                true
            }
            Key::Code(KeyCode::ArrowUp | KeyCode::ArrowDown) if n > 0 && st.stem.is_some() => {
                let up = key == Key::Code(KeyCode::ArrowUp);
                st.selected = if up { (st.selected + n - 1) % n } else { (st.selected + 1) % n };
                take(&mut st, ctx);
                true
            }
            Key::Code(KeyCode::ArrowUp | KeyCode::ArrowDown) if !sent.is_empty() => {
                let up = key == Key::Code(KeyCode::ArrowUp);
                let at = match (st.history, up) {
                    (None, true) => Some(sent.len() - 1),
                    (None, false) => None,
                    (Some(i), true) => Some(i.saturating_sub(1)),
                    (Some(i), false) => (i + 1 < sent.len()).then_some(i + 1),
                };
                let text = at.map_or(String::new(), |i| sent[i].clone());
                set(&mut st, text, ctx);
                st.history = at;
                true
            }
            _ => false,
        }
    }
}

/// Which recipes the viewer shows.
#[derive(Clone, Debug, PartialEq)]
pub enum RecipeQuery {
    /// Recipes making the item.
    MadeBy(u32),
    /// Recipes using it.
    UsedIn(u32),
    /// Every recipe of these kinds (a machine's arrow).
    Kinds(Vec<String>),
}

/// Recipes making an item, using it, or of a machine's kinds (NEI style): one tab per kind of
/// recipe, pages of recipes in each. Opened with the recipe/uses keys over any item in a screen
/// or by clicking a machine's arrow; clicking an item in it shows that item's recipes, a tag
/// ingredient the items with the tag.
pub struct RecipeView {
    pub query: RecipeQuery,
    /// (tab, page), kept while the screen rebuilds.
    state: Rc<std::cell::Cell<(usize, usize)>>,
}

impl RecipeView {
    pub fn new(query: RecipeQuery) -> RecipeView {
        RecipeView { query, state: Rc::new(std::cell::Cell::new((0, 0))) }
    }

    pub fn item(item: u32, uses: bool) -> RecipeView {
        RecipeView::new(if uses { RecipeQuery::UsedIn(item) } else { RecipeQuery::MadeBy(item) })
    }
}

/// What the cursor points at in a screen: a slot, a creative entry, or an icon (also inside a
/// button, like the recipe viewer's).
pub fn hovered_item(ui: &Ui, client: Option<&crate::client::Client>) -> Option<u32> {
    let h = ui.hovered?;
    let item = |n: usize| match ui.nodes[n].kind {
        Kind::Custom { tag: modui::TAG_SLOT, data } => client.and_then(|c| c.menu.as_ref()).and_then(|m| m.slots.get(data as usize)).map(|s| s.item),
        Kind::Custom { tag: modui::TAG_CREATIVE | modui::TAG_ICON, data } => Some(data as u32),
        Kind::Custom { tag: crate::hud::TAG_HOTBAR, data } => client.and_then(|c| c.inventory.get(data as usize)).map(|s| s.item),
        Kind::Item { last, .. } => Some(last.0),
        _ => None,
    };
    item(h).or_else(|| ui.nodes[h].children.iter().find_map(|&c| item(c))).filter(|&i| i != 0)
}

/// Recipe numbers per kind the query finds, kinds by name, hidden recipes left out.
fn found_recipes(q: &RecipeQuery) -> Vec<(&'static str, Vec<usize>)> {
    use crate::registry::Ingredient;
    let c = content();
    let mut kinds: Vec<&String> = c.game.recipes.keys().collect();
    kinds.sort();
    let mut out = Vec::new();
    for kind in kinds {
        let list: Vec<usize> = c.game.recipes[kind]
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.hidden)
            .filter(|(_, r)| match q {
                RecipeQuery::MadeBy(item) => r.outputs.iter().any(|o| o.what == Ingredient::Item(*item)),
                RecipeQuery::UsedIn(item) => r.inputs.iter().chain(r.shaped.iter().flat_map(|s| s.2.iter())).any(|i| !matches!(i, Ingredient::Empty) && i.matches(*item, &c.components)),
                RecipeQuery::Kinds(k) => k.iter().any(|k| k == kind),
            })
            .map(|(i, _)| i)
            .collect();
        if !list.is_empty() {
            out.push((kind.as_str(), list));
        }
    }
    out
}

fn kind_title(cx: &BuildCx, kind: &str) -> Vec<text::Span> {
    text::Name::Recipe(kind.to_string()).spans(cx.lang, Some(cx.input))
}

/// Item standing for a kind on its tab: its first station, or what its first recipe makes.
fn kind_icon(kind: &str) -> u32 {
    let c = content();
    c.game.recipe_stations.get(kind).and_then(|v| v.first().copied()).unwrap_or_else(|| c.game.recipes.get(kind).and_then(|l| l.first()).map_or(0, |r| r.result().item))
}

impl Screen for RecipeView {
    fn name(&self) -> &str {
        "core:recipes"
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        let found = found_recipes(&self.query);
        let (mut tab, mut page) = self.state.get();
        tab = tab.min(found.len().saturating_sub(1));
        let (title, args) = match &self.query {
            RecipeQuery::MadeBy(i) => ("recipes.made_by", vec![text::Arg::Name(text::Name::Item(*i))]),
            RecipeQuery::UsedIn(i) => ("recipes.uses", vec![text::Arg::Name(text::Name::Item(*i))]),
            RecipeQuery::Kinds(_) => ("recipes.all", Vec::new()),
        };
        // As many recipes on a page as fit the window.
        let tall = found.get(tab).map_or(1.0, |(kind, _)| recipe_height(kind));
        let room = (cx.size.y - 110.0).max(60.0);
        let per_page = ((room / (tall + 10.0)) as usize).clamp(1, 8);
        let pages = found.get(tab).map_or(1, |(_, l)| l.len().div_ceil(per_page)).max(1);
        page = page.min(pages - 1);
        self.state.set((tab, page));
        ui.root_style(Style::default().stretch());
        ui.panel("overlay", Style::default().grow().center().pad(10.0).gap(6.0), |ui| {
            label(ui, cx, title, &args, "title");
            if found.is_empty() {
                label(ui, cx, "recipes.none", &[], "dim");
            }
            ui.container(Style::row().gap(2.0), |ui| {
                for (i, (kind, _)) in found.iter().enumerate() {
                    let state = self.state.clone();
                    let b = ui.button("button", Style::default().size(22.0, 22.0).center(), Rc::new(move |c: &mut Ctx| {
                        state.set((i, 0));
                        c.requests.push(Request::Rebuild);
                    }), |ui| {
                        ui.add(Kind::Custom { tag: modui::TAG_ICON, data: kind_icon(kind) as u64 }, Style::default().size(16.0, 16.0));
                    });
                    if let Kind::Button { selected, .. } = &mut ui.nodes[b].kind {
                        *selected = i == tab;
                    }
                    ui.nodes[b].tooltip = Some(kind_title(cx, kind));
                    ui.set_id(b, &format!("tab:{kind}"));
                }
            });
            if let Some((kind, list)) = found.get(tab) {
                let p = ui.panel("panel", Style::default().pad(7.0).gap(10.0).min_w(220.0), |ui| {
                    ui.container(Style::row().gap(4.0), |ui| {
                        for &station in content().game.recipe_stations.get(*kind).map_or(&[][..], |v| v.as_slice()) {
                            ui.add(Kind::Custom { tag: modui::TAG_ICON, data: station as u64 }, Style::default().size(12.0, 12.0));
                        }
                        ui.text(kind_title(cx, kind), "default");
                    });
                    for &r in list.iter().skip(page * per_page).take(per_page) {
                        recipe_entry(ui, cx, kind, r);
                    }
                });
                ui.set_id(p, "recipes");
            }
            ui.container(Style::row().gap(4.0).center(), |ui| {
                let turn = |d: isize| {
                    let state = self.state.clone();
                    Rc::new(move |c: &mut Ctx| {
                        let (t, p) = state.get();
                        state.set((t, (p as isize + d).rem_euclid(pages as isize) as usize));
                        c.requests.push(Request::Rebuild);
                    }) as crate::ui::Action
                };
                let prev = ui.button("button", Style::default().size(20.0, 20.0).center(), turn(-1), |ui| {
                    ui.text(text::plain("<"), "button");
                });
                ui.set_id(prev, "page_prev");
                label(ui, cx, "recipes.page", &[text::Arg::Int(page as i64 + 1), text::Arg::Int(pages as i64)], "default");
                let next = ui.button("button", Style::default().size(20.0, 20.0).center(), turn(1), |ui| {
                    ui.text(text::plain(">"), "button");
                });
                ui.set_id(next, "page_next");
                ui.add(Kind::Container { class: None }, Style::default().w(12.0));
                button(ui, cx, "recipes.tags", &[], 70.0, |c| c.requests.push(Request::Open(Box::new(TagList::default()))));
                button(ui, cx, "menu.back", &[], 70.0, |c| c.requests.push(Request::Close));
            });
        });
    }
}

/// Grid size of a kind's inputs or outputs for recipe `r`: the kind's own, or as many as it has.
fn grid_size(kind: &str, r: &crate::registry::Recipe, outputs: bool) -> (u32, u32) {
    let def = content().game.recipe_kind_defs.get(kind);
    let fixed = def.map_or([0; 2], |d| if outputs { d.output_grid } else { d.input_grid });
    if fixed[0] > 0 && fixed[1] > 0 {
        return (fixed[0], fixed[1]);
    }
    if outputs {
        return (r.outputs.len().max(1) as u32, 1);
    }
    match &r.shaped {
        Some((w, h, _)) => (*w, *h),
        None => {
            let w = (r.inputs.len() as f32).sqrt().ceil().max(1.0) as u32;
            (w, (r.inputs.len() as u32).div_ceil(w).max(1))
        }
    }
}

/// Rough height of one recipe of `kind` in GUI pixels, for sizing pages.
fn recipe_height(kind: &str) -> f32 {
    let c = content();
    let def = c.game.recipe_kind_defs.get(kind);
    let rows = def.map_or(1, |d| d.input_grid[1].max(d.output_grid[1]).max(1));
    let first = c.game.recipes.get(kind).and_then(|l| l.first());
    let rows = first.map_or(rows, |r| rows.max(grid_size(kind, r, false).1).max(grid_size(kind, r, true).1));
    let info = def.is_some_and(|d| d.info.is_some()) as u32 as f32 * 12.0;
    rows as f32 * 18.0 + info + 12.0
}

/// One recipe: the kind's own `display`, or inputs, an arrow and outputs as the kind's grids,
/// with its info line under them.
fn recipe_entry(ui: &mut Ui, cx: &BuildCx, kind: &str, index: usize) {
    let c = content();
    let Some(r) = c.game.recipes.get(kind).and_then(|l| l.get(index)) else { return };
    let def = c.game.recipe_kind_defs.get(kind);
    if let Some((owner, cb)) = def.and_then(|d| d.display.map(|cb| (d.owner, cb))) {
        ui.container(Style::default().gap(2.0), |ui| {
            modui::build_recipe(ui, cx.lang, Some(cx.input), owner, cb, cx.client, kind, index as u32);
        });
        return;
    }
    ui.container(Style::default().gap(2.0), |ui| {
        ui.container(Style::row().gap(6.0), |ui| {
            recipe_grid(ui, cx.lang, cx.input, kind, r, false);
            ui.container(Style::default().gap(1.0).center(), |ui| {
                let [w, h] = crate::ui::RECIPE_ARROW;
                ui.add(Kind::Progress { class: "arrow".into(), bind: Box::new(|_: &Ctx| 0.0), value: 0.0, dir: loom_abi::ui_dir::RIGHT }, Style::default().size(w, h));
                if r.time > 0 && def.is_none_or(|d| d.info.is_none()) {
                    label(ui, cx, "recipes.time", &[text::Arg::Float(r.time as f64 / crate::world::TPS as f64, 1)], "dim");
                }
            });
            recipe_grid(ui, cx.lang, cx.input, kind, r, true);
        });
        if let Some(d) = def.filter(|d| d.info.is_some()) {
            let args = info_args(d, r);
            label(ui, cx, d.info.as_deref().unwrap_or(""), &args, "dim");
        }
    });
}

/// The values a kind's info line asks for, from one recipe.
fn info_args(def: &crate::registry::RecipeKindDef, r: &crate::registry::Recipe) -> Vec<text::Arg> {
    let c = content();
    def.info_values
        .iter()
        .map(|name| {
            if name == "time" {
                return text::Arg::Float(r.time as f64 / crate::world::TPS as f64, 1);
            }
            let Some(id) = c.components.id(name) else { return text::Arg::Text(String::new()) };
            let schema = c.components.schemas.get(id as usize).map_or("", String::as_str);
            let b = r.value(id).unwrap_or(&[]);
            let num = |n: usize| b.get(..n).map(|s| s.to_vec());
            match schema {
                "u32" => num(4).map_or(text::Arg::Int(0), |v| text::Arg::Int(u32::from_le_bytes(v.try_into().unwrap()) as i64)),
                "i32" => num(4).map_or(text::Arg::Int(0), |v| text::Arg::Int(i32::from_le_bytes(v.try_into().unwrap()) as i64)),
                "u64" | "i64" => num(8).map_or(text::Arg::Int(0), |v| text::Arg::Int(i64::from_le_bytes(v.try_into().unwrap()))),
                "f32" => num(4).map_or(text::Arg::Float(0.0, 1), |v| text::Arg::Float(f32::from_le_bytes(v.try_into().unwrap()) as f64, 1)),
                "f64" => num(8).map_or(text::Arg::Float(0.0, 1), |v| text::Arg::Float(f64::from_le_bytes(v.try_into().unwrap()), 1)),
                _ => text::Arg::Text(String::new()),
            }
        })
        .collect()
}

/// A recipe's inputs or outputs as the kind's grid (a 1 x 2 recipe in a crafting table's 3 x 3).
pub fn recipe_grid(ui: &mut Ui, lang: &crate::ui::lang::Lang, input: &crate::input::Input, kind: &str, r: &crate::registry::Recipe, outputs: bool) {
    use crate::registry::Ingredient;
    let (cols, rows) = grid_size(kind, r, outputs);
    let cell = |x: u32, y: u32| -> Option<(Ingredient, u32, f32)> {
        if outputs {
            let o = r.outputs.get((y * cols + x) as usize)?;
            return Some((o.what.clone(), o.count, o.chance));
        }
        match &r.shaped {
            Some((w, h, cells)) if x < *w && y < *h => Some((cells[(y * w + x) as usize].clone(), 1, 1.0)),
            Some(_) => None,
            None => {
                let i = (y * cols + x) as usize;
                Some((r.inputs.get(i)?.clone(), r.input_counts.get(i).copied().unwrap_or(1), 1.0))
            }
        }
    };
    ui.container(Style::default().gap(1.0), |ui| {
        for y in 0..rows {
            ui.container(Style::row().gap(1.0), |ui| {
                for x in 0..cols {
                    match cell(x, y) {
                        Some((ing, n, chance)) => recipe_slot(ui, lang, input, &ing, n, chance),
                        None => recipe_slot(ui, lang, input, &Ingredient::Empty, 0, 1.0),
                    }
                }
            });
        }
    });
}

/// An ingredient or result: a slot showing the item (cycling a tag's items) or a fluid; clicking
/// shows the item's recipes, or a tag's items.
pub fn recipe_slot(ui: &mut Ui, lang: &crate::ui::lang::Lang, input: &crate::input::Input, ing: &crate::registry::Ingredient, count: u32, chance: f32) {
    use crate::registry::Ingredient;
    let c = content();
    let fmt = |key: &str, args: &[text::Arg]| {
        let tcx = text::Ctx::new(lang, Some(input));
        let mut out = Vec::new();
        text::format(lang.get(key), args, &tcx, &mut out);
        out
    };
    let size = Style::default().size(18.0, 18.0);
    let mut tip: Vec<text::Span>;
    let b = match ing {
        Ingredient::Empty => {
            ui.add(Kind::Container { class: Some("recipe_slot".into()) }, size);
            return;
        }
        Ingredient::Fluid(name) => {
            // The liquid's own top texture, tinted like its item would be (water is gray).
            let state = c.liquids.iter().find(|l| &l.name == name).and_then(|l| c.states.get(l.first_state as usize));
            let texture = state.and_then(|s| c.textures.get(s.textures[2] as usize)).cloned().unwrap_or_default();
            let color = state.map_or(0xFFFFFFFF, |s| if s.tint != 0 { (crate::content::item_tint(s.tint) << 8) | 0xFF } else { 0xFFFFFFFF });
            let b = ui.button("recipe_slot", size, Rc::new(|_: &mut Ctx| {}), |ui| {
                ui.add(Kind::Image { texture, color }, Style::default().size(16.0, 16.0));
            });
            tip = text::Name::Liquid(name.clone()).spans(lang, None);
            tip.push(text::Span::text("\n", None));
            tip.extend(fmt("recipes.fluid_amount", &[text::Arg::Int(count as i64)]));
            b
        }
        Ingredient::Item(_) | Ingredient::Tag(_) | Ingredient::Any(_) => {
            let items: Vec<u32> = match ing {
                Ingredient::Item(i) => vec![*i],
                Ingredient::Any(list) => list.clone(),
                Ingredient::Tag(t) => (1..c.items.len() as u32).filter(|&i| c.components.has(crate::components::ITEM, i, *t)).collect(),
                _ => Vec::new(),
            };
            if items.is_empty() {
                ui.add(Kind::Container { class: Some("recipe_slot".into()) }, size);
                return;
            }
            let shown = items.clone();
            let tag = if let Ingredient::Tag(t) = ing { Some(*t) } else { None };
            let b = ui.button("recipe_slot", size, Rc::new(move |ctx: &mut Ctx| match tag {
                Some(t) => ctx.requests.push(Request::Open(Box::new(TagView { tag: t }))),
                None => {
                    let i = shown[(std::time::UNIX_EPOCH.elapsed().map_or(0, |d| d.as_secs()) as usize) % shown.len()];
                    ctx.requests.push(Request::Open(Box::new(RecipeView::item(i, false))));
                }
            }), |ui| {
                let list = items.clone();
                ui.add(
                    Kind::Item { bind: Box::new(move |_: &Ctx| (list[(std::time::UNIX_EPOCH.elapsed().map_or(0, |d| d.as_secs()) as usize) % list.len()], count)), last: (items[0], count) },
                    Style::default().size(16.0, 16.0),
                );
            });
            tip = match tag {
                Some(t) => fmt("recipes.tag", &[text::Arg::Text(c.components.names[t as usize].clone())]),
                None => crate::hud::item_tooltip(lang, input, loom_abi::ItemStack::new(items[0], count.max(1))),
            };
            b
        }
    };
    if chance < 1.0 {
        tip.push(text::Span::text("\n", None));
        tip.extend(fmt("recipes.chance", &[text::Arg::Float(chance as f64 * 100.0, 0)]));
    }
    ui.nodes[b].tooltip = Some(tip);
}

/// Every item tag, with how many items carry it; clicking one lists them.
#[derive(Default)]
pub struct TagList {
    filter: Rc<RefCell<String>>,
}

impl Screen for TagList {
    fn name(&self) -> &str {
        "core:tags"
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        let c = content();
        let mut tags: Vec<(u32, usize)> = (0..c.components.names.len() as u32)
            .filter(|&t| c.components.is_tag(t))
            .map(|t| (t, (1..c.items.len() as u32).filter(|&i| c.components.has(crate::components::ITEM, i, t)).count()))
            .filter(|t| t.1 > 0)
            .collect();
        tags.sort_by(|a, b| c.components.names[a.0 as usize].cmp(&c.components.names[b.0 as usize]));
        let filter = self.filter.borrow().to_lowercase();
        tags.retain(|t| c.components.names[t.0 as usize].to_lowercase().contains(&filter));
        ui.root_style(Style::default().stretch());
        ui.panel("overlay", Style::default().grow().center().pad(10.0).gap(6.0), |ui| {
            label(ui, cx, "recipes.tags", &[], "title");
            let f = self.filter.clone();
            let input = ui.add(
                Kind::Input {
                    value: self.filter.borrow().clone(),
                    caret: self.filter.borrow().chars().count(),
                    max: 48,
                    on_change: Some(Rc::new(move |ctx: &mut Ctx, v: &str| {
                        *f.borrow_mut() = v.to_string();
                        ctx.requests.push(Request::Rebuild);
                    })),
                },
                Style::default().h(14.0).w(240.0),
            );
            ui.set_id(input, "tag_search");
            let list = ui.panel("panel", Style::default().pad(7.0).gap(2.0).w(240.0).h(0.0).grow().min_h(60.0).scroll().stretch(), |ui| {
                for (t, n) in tags {
                    let b = ui.button("button", Style::row().min_h(20.0).pad_xy(6.0, 4.0).stretch(), Rc::new(move |c: &mut Ctx| c.requests.push(Request::Open(Box::new(TagView { tag: t })))), |ui| {
                        ui.text(text::plain(&format!("#{}", c.components.names[t as usize])), "button");
                        ui.spacer();
                        ui.text(text::plain(&n.to_string()), "button");
                    });
                    ui.set_id(b, &format!("tag:{}", c.components.names[t as usize]));
                }
            });
            ui.set_id(list, "tags");
            button(ui, cx, "menu.back", &[], 160.0, |c| c.requests.push(Request::Close));
        });
    }
}

/// The items carrying one tag; clicking one shows its recipes.
pub struct TagView {
    pub tag: u32,
}

impl Screen for TagView {
    fn name(&self) -> &str {
        "core:tag"
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        use crate::registry::Ingredient;
        let c = content();
        let items: Vec<u32> = (1..c.items.len() as u32).filter(|&i| c.components.has(crate::components::ITEM, i, self.tag)).collect();
        let name = c.components.names.get(self.tag as usize).cloned().unwrap_or_default();
        ui.root_style(Style::default().stretch());
        ui.panel("overlay", Style::default().grow().center().pad(10.0).gap(6.0), |ui| {
            ui.text(text::plain(&format!("#{name}")), "title");
            ui.panel("panel", Style::default().pad(7.0).gap(1.0), |ui| {
                for row in items.chunks(9) {
                    ui.container(Style::row().gap(1.0), |ui| {
                        for &i in row {
                            recipe_slot(ui, cx.lang, cx.input, &Ingredient::Item(i), 1, 1.0);
                        }
                    });
                }
            });
            button(ui, cx, "menu.back", &[], 160.0, |c| c.requests.push(Request::Close));
        });
    }
}
