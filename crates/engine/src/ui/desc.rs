//! Engine screens built the way a mod builds its own: `abi::UiNode` descriptors through the
//! host's `ui_open`/`ui_close`, with callbacks like a mod's. So whatever these screens show, a
//! mod's replacement can show too.

use std::ffi::c_void;

use loom_abi::{self as abi, Callback, Str, ui_flags as fl, ui_kind as k};

use crate::app::Running;
use crate::client::modui;
use crate::screens::BuildCx;
use crate::settings::Settings;
use crate::ui::text::Arg;
use crate::ui::{Style, Ui};

unsafe extern "C" fn tramp<A, F: Fn(&mut A)>(data: *const c_void, args: *mut c_void) -> u32 {
    unsafe { (*(data as *const F))(&mut *(args as *mut A)) };
    0
}

fn callback_of<A, F: Fn(&mut A)>(f: &F) -> Callback {
    Callback { func: Some(tramp::<A, F>), data: f as *const F as *const c_void }
}

/// A node of `kind` with everything else at its default.
pub(crate) fn node(kind: u32) -> abi::UiNode {
    abi::UiNode { kind, ..abi::UiNode::DEFAULT }
}

/// Lays out one screen with `f`, on the whole window.
pub(crate) fn build(ui: &mut Ui, cx: &BuildCx, f: impl Fn(&mut D)) {
    ui.root_style(Style::default().stretch());
    let run = |a: &mut abi::UiArgs| f(&mut D { u: a.ui });
    modui::build(ui, cx.lang, Some(cx.input), crate::registry::ENGINE, callback_of(&run), cx.client);
}

/// The usual menu: a dimmed (in game) or plain background with a title over `f`'s parts.
pub(crate) fn frame(d: &mut D, in_game: bool, title: &str, f: impl FnOnce(&mut D)) {
    let bg = if in_game { "overlay" } else { "background" };
    let n = abi::UiNode { class: Str::new(bg), grow: 1.0, align: abi::ui_align::CENTER, justify: abi::ui_justify::CENTER, pad: [12.0; 4], gap: 10.0, ..node(k::PANEL) };
    d.open(&n, |d| {
        d.title(title);
        f(d);
    });
}

/// The open screen being built.
pub(crate) struct D {
    u: *mut abi::UiCtx,
}

impl D {
    /// A callback living as long as the screen's nodes.
    fn keep<A: 'static, F: Fn(&mut A) + 'static>(&mut self, f: F) -> Callback {
        let b = Box::new(f);
        let cb = callback_of(&*b);
        unsafe { modui::keep(self.u, b) };
        cb
    }

    pub(crate) fn open(&mut self, n: &abi::UiNode, f: impl FnOnce(&mut D)) {
        unsafe { modui::ui_open(self.u, n) };
        f(self);
        unsafe { modui::ui_close(self.u) };
    }

    pub(crate) fn leaf(&mut self, n: &abi::UiNode) {
        self.open(n, |_| {});
    }

    /// `key` with `args` in node `n` (its class, flags, scale).
    pub(crate) fn text(&mut self, n: abi::UiNode, key: &str, args: &[Arg]) {
        let (raw, _texts) = raw_args(args);
        self.leaf(&abi::UiNode { kind: k::TEXT, text: Str::new(key), args: abi::Slice::new(&raw), ..n });
    }

    /// Text worked out every frame from the settings.
    pub(crate) fn text_bound(&mut self, n: abi::UiNode, f: impl Fn(&Settings) -> (String, Vec<Arg>) + 'static) {
        let bind = self.keep(move |a: &mut abi::UiBindArgs| {
            // Settings are read from the configs; no client needed.
            let (key, args) = f(&Settings::new(std::path::Path::new("")));
            let (raw, _texts) = raw_args(&args);
            unsafe { modui::ui_text(a.text, Str::new(&key), abi::Slice::new(&raw)) };
        });
        self.leaf(&abi::UiNode { kind: k::TEXT, bind, ..n });
    }

    /// A screen's title, large, on one line.
    pub(crate) fn title(&mut self, key: &str) {
        self.text(abi::UiNode { class: Str::new("title"), scale: 2.0, flags: fl::NO_WRAP, ..node(k::TEXT) }, key, &[]);
    }

    /// The usual button: at least `width` wide, the label on one line; `act` runs with the whole
    /// client, like a mod's button.
    pub(crate) fn button(&mut self, key: &str, args: &[Arg], width: f32, grow: bool, act: impl Fn(&mut Running) + 'static) {
        let on_click = self.keep(move |a: &mut abi::UiArgs| {
            if let Some(r) = unsafe { crate::app::api::run(a.client) } {
                act(r);
            }
        });
        let n = abi::UiNode { class: Str::new("button"), min: [width, 20.0], pad: [6.0, 4.0, 6.0, 4.0], grow: grow as u32 as f32, on_click, ..node(k::BUTTON) };
        self.open(&n, |d| d.text(abi::UiNode { class: Str::new("button"), flags: fl::NO_WRAP, ..node(k::TEXT) }, key, args));
    }

    /// A labeled slider of the number setting `name`, over the range its declaration gives
    /// (snapped to its step), shown times `scale` (100 for percent).
    pub(crate) fn slider(&mut self, key: &'static str, name: &str, scale: f64) {
        let Some(id) = crate::config::by_name(name) else { return };
        let def = &crate::content::content().game.configs[id as usize];
        let (min, range) = (def.min, (def.max - def.min).max(1e-9));
        let col = abi::UiNode { align: abi::ui_align::STRETCH, gap: 2.0, self_align: abi::ui_align::STRETCH, ..node(k::CONTAINER) };
        self.open(&col, |d| {
            d.text_bound(node(k::TEXT), move |_| (key.to_string(), vec![Arg::Int((crate::config::get(id) * scale).round() as i64)]));
            let bind = d.keep(move |a: &mut abi::UiBindArgs| {
                a.value = ((crate::config::get(id) - min) / range) as f32;
            });
            let on_change = d.keep(move |a: &mut abi::UiChangeArgs| {
                crate::config::set(id, min + a.value as f64 * range);
            });
            d.leaf(&abi::UiNode { height: 20.0, self_align: abi::ui_align::STRETCH, bind, on_change, ..node(k::SLIDER) });
        });
    }

    /// What mods add to place `area` of `screen`.
    pub(crate) fn parts(&mut self, screen: &str, area: &str) {
        self.leaf(&abi::UiNode { class: Str::new(screen), text: Str::new(area), align: abi::ui_align::STRETCH, gap: 4.0, self_align: abi::ui_align::STRETCH, ..node(k::PARTS) });
    }
}

/// `args` as a mod passes them, and the texts they point into.
fn raw_args(args: &[Arg]) -> (Vec<abi::UiArg>, Vec<String>) {
    let texts: Vec<String> = args
        .iter()
        .map(|a| match a {
            Arg::Text(t) | Arg::Key(t) => t.clone(),
            Arg::Name(n) => n.spec(),
            _ => String::new(),
        })
        .collect();
    let raw = args
        .iter()
        .zip(&texts)
        .map(|(a, t)| match a {
            Arg::Text(_) => abi::UiArg { kind: abi::ui_arg::TEXT, text: Str::new(t), ..abi::UiArg::DEFAULT },
            Arg::Key(_) => abi::UiArg { kind: abi::ui_arg::KEY, text: Str::new(t), ..abi::UiArg::DEFAULT },
            Arg::Name(_) => abi::UiArg { kind: abi::ui_arg::NAME, text: Str::new(t), ..abi::UiArg::DEFAULT },
            Arg::Int(v) => abi::UiArg { kind: abi::ui_arg::INT, num: *v as f64, ..abi::UiArg::DEFAULT },
            Arg::Float(v, d) => abi::UiArg { kind: abi::ui_arg::FLOAT, num: *v, digits: *d as u32, ..abi::UiArg::DEFAULT },
            Arg::Item(i) => abi::UiArg { kind: abi::ui_arg::ITEM, num: *i as f64, ..abi::UiArg::DEFAULT },
            // Nested messages are for the server; screens pass their parts as arguments.
            Arg::Msg(_) => abi::UiArg::DEFAULT,
        })
        .collect();
    (raw, texts)
}
