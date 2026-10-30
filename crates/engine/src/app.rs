pub mod api;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use glam::Vec2;
use loom_abi as abi;
use loom_render::{Backend, BackendDesc, Cmd, CommandList, Format, Target, TextureDesc, TextureId};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::assets::Assets;
use crate::client::Client;
use crate::client::textures::GameTextures;
use crate::content::content;
use crate::hud::Hud;
use crate::input::{Input, Key};
use crate::modloader::{self, LoadedMod};
use crate::net;
use crate::saves::{self, WorldMeta};
use crate::screens::{self, BuildCx, Screen};
use crate::server::{self, ServerConfig, ServerHandle};
use crate::settings::Settings;
use crate::ui::lang::Lang;
use crate::ui::render::UiRenderer;
use crate::ui::text::{self, Arg, Msg};
use crate::ui::look::Look;
use crate::ui::{Kind, Ui};

pub type BackendFactory = fn(&BackendDesc) -> Result<Box<dyn Backend>, loom_render::Error>;

#[derive(Clone, Debug)]
pub struct Options {
    pub run_dir: PathBuf,
    pub mod_dirs: Vec<PathBuf>,
    pub gpu_debug: bool,
    /// Automation: skip the menu and play this world (created if missing).
    pub world: Option<String>,
    pub seed: Option<u64>,
    /// Automation: open this built-in screen.
    pub screen: Option<String>,
    pub size: (u32, u32),
    pub dev: bool,
    /// A mod drives the game (tests, scripts): no pause on focus loss, settings not saved.
    pub automated: bool,
    /// Benchmarks: force vsync off / a view distance without touching saved settings.
    pub no_vsync: bool,
    pub view: Option<i32>,
}

pub enum Request {
    Open(Box<dyn Screen>),
    /// A screen by name: a mod's replacement if there is one, else the engine's.
    Screen(String),
    /// A mod's button was clicked: its callback (`UiArgs`) runs with the whole client API, like
    /// a frame hook, after the input is handled.
    ModCall(loom_abi::ModId, loom_abi::Callback),
    /// A mod's input or slider changed: its `on_change` with the text or value.
    ModChange(loom_abi::ModId, loom_abi::Callback, String, f32),
    /// A key for a mod's screen (`on_key`), by its settings name.
    ModKey(loom_abi::ModId, loom_abi::Callback, String),
    Close,
    Quit,
    Rebuild,
    Rebind(String, crate::input::Chord),
    ResetBinds,
    StartWorld(WorldMeta),
    /// Without asking about mods that are missing.
    StartWorldAnyway(WorldMeta),
    LeaveWorld,
    Host,
    Join(String),
    Respawn,
    SendChat(String),
    /// Re-read packs and assets, as the reload key does.
    ReloadAssets,
}

/// What UI actions and bindings can see and change.
pub struct Ctx<'a> {
    pub settings: &'a mut Settings,
    pub client: Option<&'a mut Client>,
    pub input: &'a Input,
    pub requests: &'a mut Vec<Request>,
    pub stats: &'a Stats,
    pub debug: bool,
    /// The top screen and its UI, for what its buttons and keys do to it.
    pub top: Option<Top<'a>>,
}

pub struct Top<'a> {
    /// None while the screen itself handles a key.
    pub screen: Option<&'a mut dyn Screen>,
    pub ui: &'a mut Ui,
}

impl Ctx<'_> {
    /// The text of the top screen's input `id`.
    pub fn input_text(&self, id: &str) -> Option<String> {
        let ui = &self.top.as_ref()?.ui;
        ui.find(id).and_then(|n| match &ui.nodes[n].kind {
            Kind::Input { value, .. } => Some(value.clone()),
            _ => None,
        })
    }

    /// Replaces the text of the top screen's input `id` (completion, history).
    pub fn set_input(&mut self, id: &str, text: String) {
        let Some(top) = &mut self.top else { return };
        if let Some(n) = top.ui.find(id)
            && let Kind::Input { value, caret, .. } = &mut top.ui.nodes[n].kind
        {
            *caret = text.chars().count();
            *value = text;
            top.ui.dirty = true;
        }
    }

    /// The top screen as what it is, for its own buttons.
    pub fn screen<S: Screen>(&mut self) -> Option<&mut S> {
        let s: &mut dyn Screen = self.top.as_mut()?.screen.as_deref_mut()?;
        (s as &mut dyn std::any::Any).downcast_mut::<S>()
    }
}

/// Numbers the debug overlay shows, refreshed every frame.
#[derive(Default, Clone)]
pub struct Stats {
    pub fps: f32,
    pub frame_ms: f32,
    /// CPU parts of the last frame (`bench::CPU_PARTS`) and GPU passes (`bench::GPU_PARTS`).
    pub cpu: [f32; 5],
    pub gpu: Vec<f32>,
    pub server_ms: f32,
    pub phases: [f32; 6],
    pub regions: u32,
    /// Cubes generating or waiting for insertion, and cubes not yet sent to a player.
    pub load: (u64, u64),
    pub gpu_name: String,
    pub cpu_name: String,
    /// Physical memory of the machine, bytes.
    pub memory_total: u64,
    pub threads: usize,
    pub window: (u32, u32),
    pub gui_scale: f32,
}

pub struct Game {
    pub client: Client,
    pub server: Option<ServerHandle>,
}

/// Commands the engine runs on the client that types them: name, usage, handler. One table for
/// completion, `/help` and running them.
pub(crate) const CLIENT_COMMANDS: &[(&str, &str, fn(&mut Running, &crate::commands::Args) -> Vec<Msg>)] = &[
    // Logging is per process.
    ("log", "[filters:text]", |_, a| crate::commands::log_command(a.text("filters").unwrap_or(""))),
    // The shown gizmo groups belong to this client.
    ("gizmo", "[group:word(#core:gizmos)]", |r, a| r.game.as_mut().map_or(Vec::new(), |g| g.client.gizmo_command(a.text("group").unwrap_or("")))),
    ("music", "", |r, _| match &mut r.audio {
        Some(a) => {
            a.skip_music();
            vec![Msg::new("command.music.next", Vec::new())]
        }
        None => vec![Msg::new("command.music.no_device", Vec::new())],
    }),
];

pub(crate) struct Running {
    gpu: Box<dyn Backend>,
    window: Arc<Window>,
    depth: (TextureId, u32, u32),
    list: CommandList,
    input: Input,
    pub(crate) settings: Settings,
    lang: Lang,
    languages: Vec<(String, String)>,
    look: Look,
    assets: Assets,
    textures: GameTextures,
    ui: UiRenderer,
    post: crate::post::Post,
    audio: Option<crate::audio::Audio>,
    screens: Vec<(Box<dyn Screen>, Ui)>,
    hud: Hud,
    game: Option<Game>,
    last: Instant,
    shot_pending: bool,
    fps: f32,
    frame_ms: f32,
    fps_timer: Instant,
    fps_frames: u32,
    quads_dirty: bool,
    debug: bool,
    quit: bool,
    mods: Vec<LoadedMod>,
    client_hash: u64,
    /// Texture generation the UI quads were built with (icons drawn blank until loaded).
    ui_textures: u64,
    pub(crate) stats: Stats,
    /// F3 held and used for a combination since it went down (then releasing it does not toggle).
    f3_combo: Option<bool>,
    reload_requested: bool,
    /// The settings in effect, and the config changes seen (`apply_settings`).
    applied: Applied,
    /// Packs changed while their screen was open (`apply_settings`).
    packs_pending: bool,
    config_seen: u64,
    /// CPU milliseconds of the last frame, see `bench::CPU_PARTS`.
    prof: [f32; 5],
    bench: Option<crate::bench::Bench>,
    diag: crate::diag::Reporter,
    /// UI quads as of the last rebuild; per-frame parts are added on top.
    ui_cached: Vec<crate::ui::render::Quad>,
    ui_animated: Vec<crate::ui::render::Animated>,
    /// Where UI asked for the player model, physical pixels.
    previews: Vec<[f32; 4]>,
    /// Texture holding the rendered inventory figure, drawn by the UI.
    preview_tex: Option<u32>,
    shot_path: Option<PathBuf>,
    /// Image id the pending screenshot is stored under for mods.
    shot_image: Option<u64>,
    /// Keys mods hold down, released at the given time.
    mod_held: Vec<(Key, Instant)>,
    /// What mods draw this frame (`Host::client_draw`).
    mod_gizmos: Vec<crate::net::GizmoShape>,
    /// Sounds of the UI and of mods' client code, played after this frame's world sounds.
    ui_sounds: Vec<crate::audio::Play>,
    mod_screen_text: Vec<(f32, f32, Msg, u32, f32)>,
    frame_no: u64,
    frame_dt: f32,
    /// The top screen as of the last frame (stack depth, name) and frames since it changed.
    screen_seen: (usize, String),
    screen_frames: u32,
    /// Finished benches, the last one's fps, avg, p99, gpu and server ms, and its report line.
    benches: u32,
    bench_last: [f32; 5],
    bench_line: String,
}

struct App {
    factory: BackendFactory,
    opts: Options,
    run: Option<Running>,
    error: Option<String>,
}

pub fn run(factory: BackendFactory, opts: Options) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App { factory, opts, run: None, error: None };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The engine's own assets: next to the executable in a distribution, else (a build run from the
/// source tree, or `--dev`) the source tree's.
fn engine_assets(dev: bool) -> PathBuf {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let shipped = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("assets").join("core")));
    match shipped {
        Some(s) if !dev && s.is_dir() => s,
        _ => source,
    }
}

/// The settings `Running::apply_settings` last put into effect.
#[derive(Default, PartialEq)]
struct Applied {
    vsync: bool,
    view: i32,
    language: String,
    gui_scale: u32,
    /// Resource and shader packs: put into effect once their screens are closed, however.
    packs: (Vec<String>, Option<String>),
}

/// Mod ids in load order (where a bare UI class looks for its texture, the last first).
fn mod_ids() -> Vec<String> {
    let mut mods: Vec<&crate::content::ModMeta> = content().mods.iter().collect();
    mods.sort_by_key(|m| m.order);
    mods.iter().map(|m| m.id.clone()).collect()
}

/// `--hide-hud` (or `--record nohud`): the world without the HUD, for screenshots and footage.
fn hud_hidden() -> bool {
    let o = api::launch_options();
    o.contains_key("hide-hud") || o.get("record").is_some_and(|v| v == "nohud")
}

/// `--record`: a script run on screen for filming (full screen, animated time, no cursor).
fn recording() -> bool {
    api::launch_options().contains_key("record")
}

impl App {
    fn start(&mut self, el: &ActiveEventLoop) -> Result<Running, String> {
        // Mods first: the engine's settings are `core:` configs like theirs.
        let cache = self.opts.run_dir.join("cache");
        let mods = modloader::load(&self.opts.mod_dirs, &cache)?;
        crate::config::set_automated(self.opts.automated);
        crate::settings::migrate(&self.opts.run_dir);
        crate::config::load(&self.opts.run_dir);
        if self.opts.no_vsync {
            crate::config::override_for_run("core:vsync", "false");
        }
        if let Some(v) = self.opts.view {
            crate::config::override_for_run("core:view_distance", &v.to_string());
        }
        let mut settings = Settings::new(&self.opts.run_dir);
        let icon = crate::assets::decode_png_file(&engine_assets(self.opts.dev).join("icon.png")).and_then(|(w, h, px)| winit::window::Icon::from_rgba(px, w, h).ok());
        let mut attrs = Window::default_attributes()
            .with_title("Loom")
            .with_window_icon(icon)
            .with_inner_size(winit::dpi::PhysicalSize::new(self.opts.size.0, self.opts.size.1));
        // Automated runs work in the background: no focus taken, under other windows, off the
        // desktop (still rendered at full size, so screenshots are the same).
        if recording() {
            // Sized to what the owner's OBS scene keeps of the left monitor (crops 30 px above, 50 below).
            attrs = attrs.with_decorations(false).with_window_level(winit::window::WindowLevel::AlwaysOnTop).with_inner_size(winit::dpi::PhysicalSize::new(2560, 1360)).with_position(winit::dpi::PhysicalPosition::new(0, 30));
        } else if self.opts.automated {
            attrs = attrs.with_active(false).with_window_level(winit::window::WindowLevel::AlwaysOnBottom).with_position(winit::dpi::PhysicalPosition::new(-32000, -32000));
        }
        let window = Arc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
        let size = window.inner_size();
        let mut gpu = (self.factory)(&BackendDesc {
            window: &*window,
            width: size.width,
            height: size.height,
            vsync: settings.vsync(),
            debug: self.opts.gpu_debug,
        })
        .map_err(|e| e.to_string())?;

        let mut roots: Vec<(String, PathBuf)> = vec![("core".into(), engine_assets(self.opts.dev))];
        roots.extend(mods.iter().map(|m| (m.id.clone(), m.assets.clone())));
        // Not in automated runs: their pictures are compared against fixed expectations.
        if !self.opts.automated && crate::minecraft::ensure_pack(&self.opts.run_dir, &roots) {
            let mut packs = settings.resource_packs();
            if !packs.iter().any(|p| p == crate::minecraft::PACK) {
                packs.insert(0, crate::minecraft::PACK.to_string());
                settings.set_resource_packs(&packs);
            }
        }
        let packs_now = (settings.resource_packs(), settings.shader_pack());
        let packs = crate::packs::enabled(&self.opts.run_dir, &settings.resource_packs());
        crate::packs::set_shader_pack(settings.shader_pack().as_ref().map(|p| crate::packs::shader_dir(&self.opts.run_dir).join(p)));
        let assets = Assets::new(roots, packs);
        crate::assets::set_current(&assets);
        let lang = Lang::load(&assets, &settings.language(), self.opts.dev);
        let look = Look::new(&assets, &mod_ids(), &lang);
        let textures = GameTextures::new(&mut *gpu, content(), &assets);
        let ui = UiRenderer::new(&mut *gpu, &assets)?;
        let post = crate::post::Post::new(&mut *gpu, &assets);
        let mut input = Input::default();
        for (action, key) in &content().game.keys {
            match crate::settings::parse_chord(key) {
                Some(k) => input.register(action, k),
                None => log::warn!("key binding {action}: unknown key {key}"),
            }
        }
        settings.apply_binds(&mut input);
        settings.read_only = self.opts.automated;
        crate::shaders::pin_time(self.opts.automated && !recording());
        let depth = make_depth(&mut *gpu, size.width, size.height);
        let gpu_name = gpu.device_name().to_string();
        let mut r = Running {
            gpu,
            window,
            depth,
            list: CommandList::default(),
            input,
            settings,
            lang,
            languages: Vec::new(),
            look,
            assets,
            textures,
            ui,
            post,
            audio: crate::audio::Audio::new().map(|mut a| {
                a.muted = self.opts.automated;
                a
            }),
            screens: Vec::new(),
            hud: {
                let mut h = Hud::default();
                h.load_layout(&self.opts.run_dir);
                h
            },
            game: None,
            last: Instant::now(),
            shot_pending: false,
            fps: 0.0,
            frame_ms: 0.0,
            fps_timer: Instant::now(),
            fps_frames: 0,
            quads_dirty: true,
            debug: false,
            quit: false,
            mods,
            client_hash: 0,
            ui_textures: 0,
            stats: Stats {
                gpu_name: gpu_name.clone(),
                cpu_name: crate::diag::cpu_name(),
                memory_total: crate::diag::total_memory(),
                threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
                ..Default::default()
            },
            f3_combo: None,
            reload_requested: false,
            packs_pending: false,
            applied: Applied { packs: packs_now, ..Default::default() },
            config_seen: u64::MAX,
            prof: [0.0; 5],
            bench: None,
            diag: Default::default(),
            ui_cached: Vec::new(),
            ui_animated: Vec::new(),
            previews: Vec::new(),
            preview_tex: None,
            shot_path: None,
            shot_image: None,
            mod_held: Vec::new(),
            mod_gizmos: Vec::new(),
            ui_sounds: Vec::new(),
            mod_screen_text: Vec::new(),
            frame_no: 0,
            frame_dt: 0.0,
            screen_seen: (0, String::new()),
            screen_frames: 0,
            benches: 0,
            bench_last: [0.0; 5],
            bench_line: String::new(),
        };
        if let Some(world) = &self.opts.world {
            let saves_dir = r.settings.saves_dir();
            let meta = saves::list(&saves_dir)
                .into_iter()
                .find(|w| &w.name == world)
                .unwrap_or_else(|| saves::create(&saves_dir, world, self.opts.seed.unwrap_or(1)));
            r.start_world(meta);
        } else {
            r.open(screens::engine("core:title"));
            if let Some(name) = &self.opts.screen {
                match screens::named(name) {
                    Some(s) => r.open(s),
                    None => log::warn!("--screen: no screen {name}"),
                }
            }
        }
        Ok(r)
    }
}

fn make_depth(gpu: &mut dyn Backend, w: u32, h: u32) -> (TextureId, u32, u32) {
    let t = gpu.create_texture(
        &TextureDesc { width: w.max(1), height: h.max(1), mips: 1, format: Format::Depth32, render_target: true },
        "depth",
    );
    (t, w, h)
}

impl Running {
    fn gui_scale(&self) -> f32 {
        let max = crate::settings::max_gui_scale(self.gpu.surface_size());
        let s = if self.settings.gui_scale() == 0 { max } else { self.settings.gui_scale().min(max) };
        s as f32
    }

    fn build_cx(&self) -> BuildCx<'_> {
        BuildCx {
            lang: &self.lang,
            input: &self.input,
            settings: &self.settings,
            languages: &self.languages,
            in_game: self.game.is_some(),
            remote: self.game.as_ref().is_some_and(|g| g.server.is_none()),
            mods: self.mods.len(),
            client: self.game.as_ref().map(|g| &g.client),
            size: {
                let (w, h) = self.gpu.surface_size();
                glam::Vec2::new(w as f32, h as f32) / self.gui_scale()
            },
            look: &self.look,
            assets: &self.assets,
        }
    }

    fn open(&mut self, mut s: Box<dyn Screen>) {
        if s.needs_languages() && self.languages.is_empty() {
            self.languages = Lang::available(&self.assets);
        }
        let mut ui = Ui::new();
        s.build(&mut ui, &self.build_cx());
        self.screens.push((s, ui));
        self.quads_dirty = true;
        self.set_capture(false);
    }

    fn rebuild_all(&mut self) {
        let mut screens = std::mem::take(&mut self.screens);
        for (s, ui) in &mut screens {
            let (hover, focus) = (ui.hovered, ui.focused);
            let focus_id = focus.and_then(|f| ui.nodes.get(f)).and_then(|n| n.id.clone());
            let inputs: Vec<(String, String, usize)> = ui
                .nodes
                .iter()
                .filter_map(|n| match (&n.id, &n.kind) {
                    (Some(id), Kind::Input { value, caret, .. }) => Some((id.clone(), value.clone(), *caret)),
                    _ => None,
                })
                .collect();
            ui.clear();
            s.build(ui, &self.build_cx());
            // Typed text survives rebuilds (language switch, resize).
            for (id, v, c) in inputs {
                if let Some(n) = ui.find(&id) {
                    if let Kind::Input { value, caret, .. } = &mut ui.nodes[n].kind {
                        *value = v;
                        *caret = c;
                    }
                }
            }
            ui.hovered = hover.filter(|&h| h < ui.nodes.len());
            // Focus follows the same input (by id), or whatever the screen focused itself.
            if let Some(id) = focus_id {
                ui.focused = ui.find(&id).or(ui.focused);
            } else if ui.focused.is_none() {
                ui.focused = focus.filter(|&h| h < ui.nodes.len());
            }
        }
        self.screens = screens;
        self.hud.invalidate();
        self.quads_dirty = true;
    }

    fn set_capture(&mut self, on: bool) {
        let on = on && self.screens.is_empty() && self.game.is_some();
        // Automated runs (read-only settings) never take the real mouse: they run in the background.
        if recording() {
            self.window.set_cursor_visible(false);
        } else if self.settings.read_only {
        } else if on {
            let _ = self.window.set_cursor_grab(CursorGrabMode::Locked).or_else(|_| self.window.set_cursor_grab(CursorGrabMode::Confined));
            self.window.set_cursor_visible(false);
        } else {
            let _ = self.window.set_cursor_grab(CursorGrabMode::None);
            self.window.set_cursor_visible(true);
        }
        self.input.captured = on;
    }

    fn start_world(&mut self, meta: WorldMeta) {
        log::info!("opening world '{}' (seed {})", meta.name, meta.seed);
        if meta.format != saves::SAVE_FORMAT {
            log::error!("world '{}' was saved in format {}, this build reads {}", meta.name, meta.format, saves::SAVE_FORMAT);
            self.screens.clear();
            self.open(Box::new(screens::Message { title: "world.open_failed".into(), args: vec![Arg::Key("world.old_format".into())] }));
            return;
        }
        for m in meta.missing_mods(content()) {
            log::warn!("the world was played with mod {m}, which is not loaded");
        }
        let storage = saves::Storage::open(meta.clone(), content());
        let server = server::spawn(
            ServerConfig { seed: meta.seed, view_radius: self.settings.view_distance() },
            Some(storage),
        );
        let (client_conn, server_conn) = net::local_pair();
        let _ = server.connect.send(server_conn);
        let key = net::p2p::player_key(&self.settings.run_dir);
        match Client::new(&mut *self.gpu, &self.assets, &self.textures, client_conn, &self.settings.name(), &key, self.settings.view_distance(), std::fs::read(self.settings.run_dir.join("skins").join("skin.png")).ok()) {
            Ok(mut client) => {
                client.sent = load_chat_history(&self.settings.run_dir);
                self.game = Some(Game { client, server: Some(server) });
                self.screens.clear();
                self.hud.invalidate();
                self.quads_dirty = true;
                self.set_capture(true);
            }
            Err(e) => {
                log::error!("{e}");
                // Something to look at instead of an empty window; the server stops with `server`.
                self.screens.clear();
                self.open(Box::new(screens::Message { title: "world.open_failed".into(), args: vec![Arg::Text(e.to_string())] }));
            }
        }
    }

    fn leave_world(&mut self) {
        self.leave_world_silent();
        self.screens.clear();
        self.open(screens::engine("core:title"));
    }

    fn leave_world_silent(&mut self) {
        if let Some(mut g) = self.game.take() {
            if g.server.is_none() {
                // A remote server's synced settings were in force; back to our own.
                crate::config::load(&self.settings.run_dir);
            }
            g.client.leave();
            self.gpu.wait_idle();
            g.client.destroy(&mut *self.gpu);
            if let Some(mut s) = g.server.take() {
                s.stop();
                net::p2p::stop_hosting();
            }
        }
        self.set_capture(false);
    }

    pub(crate) fn handle(&mut self, reqs: Vec<Request>) {
        for r in reqs {
            match r {
                Request::Open(s) => self.open(s),
                Request::Close => {
                    self.screens.pop();
                    self.quads_dirty = true;
                    if self.screens.is_empty() {
                        self.set_capture(true);
                    } else {
                        self.rebuild_all();
                    }
                }
                Request::Quit => self.quit = true,
                Request::Rebuild => self.rebuild_all(),
                Request::ModCall(owner, cb) => {
                    let mut v = self.frame_view();
                    let mut a = loom_abi::UiArgs { ui: std::ptr::null_mut(), client: v.raw() };
                    crate::host::invoke(owner, cb, &mut a);
                }
                Request::ModChange(owner, cb, text, value) => {
                    let mut v = self.frame_view();
                    let mut a = loom_abi::UiChangeArgs { client: v.raw(), text: loom_abi::Str::new(&text), value };
                    crate::host::invoke(owner, cb, &mut a);
                }
                Request::ModKey(owner, cb, key) => {
                    let mut v = self.frame_view();
                    let mut a = loom_abi::ScreenKeyArgs { client: v.raw(), key: loom_abi::Str::new(&key) };
                    crate::host::invoke(owner, cb, &mut a);
                }
                Request::Screen(name) => match screens::named(&name) {
                    Some(s) => self.open(s),
                    None => log::warn!("no screen {name}"),
                },
                Request::ReloadAssets => self.reload_requested = true,
                Request::Rebind(action, key) => {
                    self.input.rebind(&action, key);
                    self.settings.set_bind(&action, key);
                }
                Request::ResetBinds => {
                    let defaults: Vec<(String, crate::input::Chord)> = self.input.binds.iter().map(|b| (b.action.clone(), b.default)).collect();
                    for (a, k) in defaults {
                        self.input.rebind(&a, k);
                        self.settings.set_bind(&a, k);
                    }
                    self.rebuild_all();
                }
                Request::StartWorld(meta) => {
                    let missing = meta.missing_mods(content());
                    if missing.is_empty() {
                        self.start_world(meta);
                    } else {
                        self.open(Box::new(saves::MissingMods { world: meta, missing }));
                    }
                }
                Request::StartWorldAnyway(meta) => self.start_world(meta),
                Request::LeaveWorld => self.leave_world(),
                Request::Host => {
                    if let Some(Game { server: Some(s), .. }) = &self.game {
                        net::host(s, &self.settings);
                    }
                    self.rebuild_all();
                }
                Request::Respawn => {
                    if let Some(g) = &mut self.game {
                        g.client.send(net::ToServer::Respawn);
                    }
                    self.screens.retain(|(s, _)| !s.closes_on_respawn());
                    self.set_capture(true);
                }
                Request::SendChat(text) => {
                    if self.game.is_some() && !self.client_command(&text) && !text.trim().is_empty() {
                        if let Some(g) = &mut self.game {
                            g.client.send(net::ToServer::Chat(text));
                        }
                    }
                }
                Request::Join(ticket) => match net::join(&ticket, &self.settings) {
                    Ok(conn) => {
                        self.leave_world_silent();
                        let key = net::p2p::player_key(&self.settings.run_dir);
                        match Client::new(&mut *self.gpu, &self.assets, &self.textures, conn, &self.settings.name(), &key, self.settings.view_distance(), std::fs::read(self.settings.run_dir.join("skins").join("skin.png")).ok()) {
                            Ok(mut client) => {
                                client.sent = load_chat_history(&self.settings.run_dir);
                                self.game = Some(Game { client, server: None });
                                self.screens.clear();
                                self.hud.invalidate();
                                self.set_capture(true);
                            }
                            Err(e) => log::error!("{e}"),
                        }
                    }
                    Err(e) => self.open(Box::new(screens::Message { title: "multiplayer.disconnected".into(), args: vec![Arg::Text(e)] })),
                },
            }
        }
    }

    /// Mouse and keyboard for the top screen.
    fn screen_input(&mut self, scale: f32, requests: &mut Vec<Request>) {
        let cursor = self.input.cursor / scale;
        let Some((_, ui)) = self.screens.last_mut() else { return };
        if self.input.scroll != 0.0 && ui.scroll_at(cursor, self.input.scroll) {
            self.quads_dirty = true;
        }
        // A scroll bar takes the mouse before anything under it.
        let on_bar = if self.input.pressed_button(0) {
            ui.bar_press(cursor)
        } else if self.input.held_button(0) {
            ui.bar_drag(cursor)
        } else {
            ui.bar_release();
            false
        };
        if on_bar {
            self.quads_dirty = true;
            return;
        }
        let hovered = ui.hit(cursor);
        if hovered != ui.hovered {
            ui.hovered = hovered;
            self.quads_dirty = true;
        }
        let mut actions: Vec<crate::ui::Action> = Vec::new();
        let mut slider: Option<(std::rc::Rc<dyn Fn(&mut Ctx, f32)>, f32)> = None;
        let mut text_changed: Option<(std::rc::Rc<dyn Fn(&mut Ctx, &str)>, String)> = None;
        let menu_screen = self.screens.last().is_some_and(|(s, _)| s.shows_menu());
        if menu_screen {
            let Some((_, ui)) = self.screens.last_mut() else { return };
            let slot = hovered.and_then(|h| match ui.nodes[h].kind {
                Kind::Custom { tag: crate::client::modui::TAG_SLOT, data } => Some(data as u32),
                _ => None,
            });
            if let Some(g) = &mut self.game {
                let shift = self.input.held_key(Key::Code(KeyCode::ShiftLeft)) || self.input.held_key(Key::Code(KeyCode::ShiftRight));
                // Pressing with a stack on the cursor starts a drag; releasing over one slot is
                // a plain click, over several it spreads the stack.
                let holding = g.client.menu.as_ref().is_some_and(|m| !m.cursor.is_empty());
                use crate::server::menu::{GATHER, HOTBAR_SWAP};
                for button in [0u8, 1] {
                    if self.input.pressed_button(button) {
                        let inside = ui.nodes.iter().any(|n| matches!(n.kind, Kind::Container { class: Some(_) }) && n.rect.contains(cursor) && n.rect.w < ui.size.x);
                        // Vanilla: the same slot and button again within a quarter second, any other
                        // click in between breaks it.
                        let double = button == 0 && g.client.last_click.is_some_and(|(l, b, t)| Some(l) == slot && b == button && t.elapsed().as_secs_f32() < 0.25);
                        g.client.last_click = slot.map(|s| (s, button, Instant::now()));
                        if shift && button == 0 {
                            g.client.sweep.extend(slot);
                        }
                        match slot {
                            // Double click: the rest of the stack just picked up joins it (the
                            // server knows what is on the cursor even before we do).
                            Some(s) if double && !shift => {
                                g.client.send(net::ToServer::Click { slot: s, button: GATHER, shift: false });
                                g.client.last_click = None;
                            }
                            Some(s) if holding && !shift && g.client.drag.is_none() => g.client.drag = Some((button, vec![s])),
                            Some(s) => g.client.send(net::ToServer::Click { slot: s, button, shift }),
                            None if !inside => g.client.send(net::ToServer::Click { slot: u32::MAX, button, shift: false }),
                            None => {}
                        }
                    }
                }
                // Shift with the left button held: every slot swept over is shift-clicked.
                if shift && self.input.held_button(0) && !holding {
                    if let Some(s) = slot.filter(|s| !g.client.sweep.contains(s)) {
                        g.client.sweep.push(s);
                        g.client.send(net::ToServer::Click { slot: s, button: 0, shift: true });
                    }
                } else if !self.input.held_button(0) {
                    g.client.sweep.clear();
                }
                // Number keys over a slot swap it with that hotbar slot.
                if let Some(s) = slot {
                    for (n, action) in crate::input::HOTBAR.into_iter().enumerate() {
                        if self.input.pressed(action) {
                            g.client.send(net::ToServer::Click { slot: s, button: HOTBAR_SWAP + n as u8, shift: false });
                        }
                    }
                }
                if let Some((button, slots)) = &mut g.client.drag {
                    if self.input.held_button(*button) {
                        if let Some(s) = slot.filter(|s| !slots.contains(s)) {
                            slots.push(s);
                            self.quads_dirty = true;
                        }
                    } else {
                        let (button, slots) = g.client.drag.take().unwrap();
                        if slots.len() == 1 {
                            g.client.send(net::ToServer::Click { slot: slots[0], button, shift: false });
                        } else {
                            g.client.send(net::ToServer::Drag { slots, button });
                        }
                        self.quads_dirty = true;
                    }
                }
                // Creative entries put a full stack on the cursor.
                let creative_item = hovered.and_then(|h| match ui.nodes[h].kind {
                    Kind::Custom { tag: crate::client::modui::TAG_CREATIVE, data } => Some(data as u32),
                    _ => None,
                });
                if let Some(item) = creative_item {
                    for button in [0u8, 1] {
                        if self.input.pressed_button(button) {
                            g.client.send(net::ToServer::CreativeTake { item, button });
                        }
                    }
                }
                // Tooltip with the hovered item's name.
                if let Some(h) = hovered {
                    if let Kind::Custom { tag, data } = ui.nodes[h].kind {
                        let stack = if tag == crate::client::modui::TAG_CREATIVE {
                            loom_abi::ItemStack::new(data as u32, 1)
                        } else {
                            g.client.menu.as_ref().and_then(|m| m.slots.get(data as usize)).copied().unwrap_or_default()
                        };
                        // An empty slot shows its group's hint, if it has one.
                        let hint = || {
                            let c = content();
                            let def = c.game.menus.get(g.client.menu.as_ref()?.kind as usize)?;
                            let key = def.group_of(&c.game.player_layout, data as u32)?.0.tooltip.clone()?;
                            Some(crate::ui::text::rich(self.lang.get(&key), &self.lang, Some(&self.input)))
                        };
                        let tip = if !stack.is_empty() {
                            Some(crate::hud::item_tooltip(&self.lang, &self.input, stack))
                        } else if tag == crate::client::modui::TAG_SLOT {
                            hint()
                        } else {
                            None
                        };
                        if ui.nodes[h].tooltip != tip {
                            ui.nodes[h].tooltip = tip;
                            self.quads_dirty = true;
                        }
                    }
                }
            }
        }
        let Some((_, ui)) = self.screens.last_mut() else { return };
        if self.input.pressed_button(0) && menu_screen {
            let arrow = hovered.is_some_and(|h| ui.nodes[h].recipes);
            let kinds = self.game.as_ref().and_then(|g| g.client.menu.as_ref()).and_then(|m| content().game.menus.get(m.kind as usize)).map(|d| d.recipes.clone()).unwrap_or_default();
            if arrow && !kinds.is_empty() {
                requests.push(Request::Open(Box::new(crate::game_screens::RecipeView::new(crate::game_screens::RecipeQuery::Kinds(kinds)))));
                return;
            }
        }
        if self.input.pressed_button(0) {
            ui.pressed = hovered;
            ui.focused = hovered.filter(|&h| matches!(ui.nodes[h].kind, Kind::Input { .. }));
            ui.anchor = None;
            if let Some(h) = hovered {
                if let Kind::Text(t) = &ui.nodes[h].kind {
                    let r = ui.nodes[h].rect;
                    let unit = ui.text_unit * t.scale;
                    let p = (cursor - Vec2::new(r.x, r.y)) / unit;
                    let target = t.layout.links.iter().find(|(_, b)| p.x >= b[0] && p.x <= b[2] && p.y >= b[1] && p.y <= b[3]).map(|l| l.0.clone());
                    if let Some(n) = target.and_then(|t| ui.find(&t)) {
                        ui.focused = Some(n);
                    }
                }
            }
            self.quads_dirty = true;
        }
        if self.input.held_button(0) {
            if let Some(p) = ui.pressed {
                let r = ui.nodes[p].rect;
                if let Kind::Slider { value, on_change, .. } = &mut ui.nodes[p].kind {
                    let v = ((cursor.x - r.x) / r.w).clamp(0.0, 1.0);
                    if (v - *value).abs() > 1e-4 {
                        *value = v;
                        slider = Some((on_change.clone(), v));
                        self.quads_dirty = true;
                    }
                }
            }
        } else if let Some(p) = ui.pressed.take() {
            if Some(p) == hovered {
                if let Kind::Button { action: Some(a), enabled: true, .. } = &ui.nodes[p].kind {
                    actions.push(a.clone());
                }
            }
            self.quads_dirty = true;
        }
        if let Some(f) = ui.focused {
            if let Kind::Input { value, caret, max, on_change } = &mut ui.nodes[f].kind {
                let outcome = crate::ui::edit::Field { value, caret, anchor: &mut ui.anchor, max: *max }.handle(&self.input);
                let (changed, moved) = (outcome.changed, outcome.moved);
                if moved {
                    self.quads_dirty = true;
                }
                if changed {
                    ui.dirty = true;
                    self.quads_dirty = true;
                    if let Some(cb) = on_change {
                        text_changed = Some((cb.clone(), value.clone()));
                    }
                }
            }
        }
        // Recipes of (or uses for) the item under the cursor, in any screen.
        let typing = ui.focused.is_some_and(|f| matches!(ui.nodes[f].kind, Kind::Input { .. }));
        for (action, uses) in [("core:recipes", false), ("core:uses", true)] {
            if !typing && self.input.pressed(action) {
                if let Some(item) = crate::game_screens::hovered_item(ui, self.game.as_ref().map(|g| &g.client)) {
                    requests.push(Request::Open(Box::new(crate::game_screens::RecipeView::item(item, uses))));
                }
            }
        }
        let ran = !actions.is_empty() || slider.is_some() || text_changed.is_some();
        let mut ctx = Ctx {
            settings: &mut self.settings,
            client: self.game.as_mut().map(|g| &mut g.client),
            input: &self.input,
            requests,
            stats: &self.stats,
            debug: self.debug,
            top: self.screens.last_mut().map(|(s, ui)| Top { screen: Some(&mut **s), ui }),
        };
        if !actions.is_empty() {
            if let Some(&click) = content().game.sound_names.get("core:ui.click") {
                self.ui_sounds.push(crate::audio::Play { sound: click, pos: None, volume: 1.0, pitch: 0.0 });
            }
        }
        for a in actions {
            a(&mut ctx);
        }
        if let Some((f, v)) = slider {
            f(&mut ctx, v);
        }
        if let Some((f, v)) = text_changed {
            f(&mut ctx, &v);
        }
        if ran {
            self.quads_dirty = true;
        }
    }

    /// Runs the mods' client hooks of one kind with a view that may drive the client.
    fn client_hooks(&mut self, hook: u32, screen: &str) {
        let hooks: Vec<(abi::ModId, abi::Callback)> = content().game.client_hooks.iter().filter(|h| h.1 == hook).map(|h| (h.0, h.2)).collect();
        for (owner, cb) in hooks {
            let mut v = self.frame_view();
            if hook == abi::client_hook::FRAME {
                let mut a = abi::ClientFrameArgs { client: v.raw() };
                crate::host::invoke(owner, cb, &mut a);
            } else {
                let mut a = abi::ClientScreenArgs { client: v.raw(), screen: abi::Str::new(screen) };
                crate::host::invoke(owner, cb, &mut a);
            }
        }
    }

    /// Every entity's cosmetics this frame: its kind's mod tick or the engine's; finished deaths
    /// go after their last call.
    fn entity_ticks(&mut self, dt: f32) {
        let Some(g) = &self.game else { return };
        let c = content();
        let all: Vec<abi::ClientEntityArgs> = g.client.entities.iter().filter(|(_, e)| e.item.is_empty() && e.structure.is_none()).map(|(&id, e)| e.args(id, dt)).collect();
        for mut a in all {
            match c.game.client_entity_ticks.get(&a.kind) {
                Some(&(owner, cb)) => {
                    let mut v = self.frame_view();
                    a.client = v.raw();
                    crate::host::invoke(owner, cb, &mut a);
                }
                None => {
                    if let Some(g) = &mut self.game {
                        g.client.entity_effects(&a);
                    }
                }
            }
        }
        if let Some(g) = &mut self.game {
            g.client.entities.retain(|_, e| !e.dying.is_some_and(|d| d >= 1.0));
        }
    }

    /// Puts what changed in the settings into effect, whoever changed them: a screen, a mod, a
    /// command or a test.
    fn apply_settings(&mut self) {
        let s = self.settings.clone();
        let mut now = Applied { vsync: s.vsync(), view: s.view_distance(), language: s.language(), gui_scale: s.gui_scale(), packs: (s.resource_packs(), s.shader_pack()) };
        if now.vsync != self.applied.vsync {
            self.gpu.set_vsync(now.vsync);
        }
        if now.view != self.applied.view {
            if let Some(g) = &mut self.game {
                g.client.set_view_distance(now.view);
            }
        }
        let relang = now.language != self.applied.language;
        if relang {
            self.lang = Lang::load(&self.assets, &now.language, self.lang.dev);
            self.look = Look::new(&self.assets, &mod_ids(), &self.lang);
        }
        s.apply_binds(&mut self.input);
        if relang || now.gui_scale != self.applied.gui_scale {
            self.rebuild_all();
        }
        // Not while choosing: every click would reload everything.
        if now.packs != self.applied.packs {
            if self.screens.iter().any(|(s, _)| matches!(s.name(), "core:resource_packs" | "core:shader_packs")) {
                now.packs = self.applied.packs.clone();
                self.packs_pending = true;
            } else {
                self.reload_requested = true;
            }
        }
        self.applied = now;
    }

    fn frame_view(&mut self) -> crate::client::modui::ClientView {
        let mut v = crate::client::modui::ClientView::new(self.game.as_ref().map(|g| &g.client));
        v.run = self as *mut Running as *mut std::ffi::c_void;
        v
    }

    /// Runs a command for code that wants the answer (`client_op::RUN_COMMAND`): a client
    /// command here and now, any other on the server. The id finds the answer.
    pub(crate) fn run_command(&mut self, line: &str) -> u32 {
        let line = if line.starts_with('/') { line.to_string() } else { format!("/{line}") };
        let Some(g) = &mut self.game else { return 0 };
        g.client.next_command += 1;
        let id = g.client.next_command;
        match self.client_command_lines(&line) {
            Some(lines) => {
                if let Some(g) = &mut self.game {
                    g.client.command_replies.insert(id, lines);
                }
            }
            None => {
                if let Some(g) = &mut self.game {
                    g.client.send(net::ToServer::Command { id, line });
                }
            }
        }
        id
    }

    /// Runs `line` here if it is a command marked to run on the client; false otherwise.
    fn client_command(&mut self, line: &str) -> bool {
        let Some(reply) = self.client_command_lines(line) else { return false };
        if let Some(g) = &mut self.game {
            for line in reply {
                g.client.chat_line(line);
            }
        }
        true
    }

    /// The answer of `line` if it is a command that runs on the client.
    fn client_command_lines(&mut self, line: &str) -> Option<Vec<Msg>> {
        let rest = line.trim().strip_prefix('/')?;
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        let def = tokens.first().and_then(|n| content().commands.iter().find(|d| d.client && d.name == *n))?;
        let g = self.game.as_ref()?;
        let p = &g.client.player;
        let here = crate::commands::Origin { pos: p.pos, yaw: p.yaw, pitch: p.pitch };
        let players = g.client.players.clone();
        let reply = match def.parse(content(), &tokens[1..], here, &players) {
            Err(e) => vec![Msg::new("command.error_usage", vec![Arg::Msg(e), Arg::Text(def.name.clone()), Arg::Text(def.usage.clone())])],
            Ok((args, handler)) => match handler {
                None => match CLIENT_COMMANDS.iter().find(|c| c.0 == def.name) {
                    Some(c) => (c.2)(self, &args),
                    None => Vec::new(),
                },
                Some((owner, cb)) => {
                    let values = crate::commands::abi_values(&args, &[]);
                    let mut reply: Vec<Msg> = Vec::new();
                    let reply_ptr = &mut reply as *mut Vec<Msg> as *mut core::ffi::c_void;
                    let player = self.game.as_ref().map_or(0, |g| g.client.player_id);
                    let mut v = self.frame_view();
                    let mut a = abi::CommandArgs { world: std::ptr::null_mut(), player, values: values.as_ptr(), count: values.len() as u32, reply: reply_ptr, client: v.raw() };
                    crate::host::invoke(owner, cb, &mut a);
                    reply
                }
            },
        };
        Some(reply)
    }

    /// Mod channel messages that arrived for the client, each to its handler.
    fn client_packets(&mut self) {
        let Some(g) = self.game.as_mut() else { return };
        let packets = std::mem::take(&mut g.client.packets);
        for (channel, data) in packets {
            let Some((owner, cb)) = content().game.packets.get(channel as usize).and_then(|p| p.client) else { continue };
            let mut v = self.frame_view();
            let mut a = abi::ClientPacketArgs { client: v.raw(), data: abi::Slice::new(&data) };
            crate::host::invoke(owner, cb, &mut a);
        }
    }

    /// Re-evaluates bound values; layout and geometry only change when a value did.
    fn poll_bindings(&mut self) {
        let mut reqs = Vec::new();
        let server = self.game.as_ref().and_then(|g| g.server.as_ref());
        let gui_scale = self.gui_scale();
        let st = &mut self.stats;
        (st.fps, st.frame_ms, st.cpu) = (self.fps, self.frame_ms, self.prof);
        st.gpu.clear();
        st.gpu.extend_from_slice(self.gpu.gpu_times());
        (st.server_ms, st.regions, st.phases) = server.map_or((0.0, 0, [0.0; 6]), |s| (s.tick_ms(), s.regions(), s.phases()));
        st.load = server.map_or((0, 0), |s| (s.shared.pending.load(std::sync::atomic::Ordering::Relaxed), s.shared.unsent.load(std::sync::atomic::Ordering::Relaxed)));
        st.window = self.gpu.surface_size();
        st.gui_scale = gui_scale;
        let ctx = Ctx {
            settings: &mut self.settings,
            client: self.game.as_mut().map(|g| &mut g.client),
            input: &self.input,
            requests: &mut reqs,
            stats: &self.stats,
            debug: self.debug,
            top: None,
        };
        let tcx = text::Ctx::new(&self.lang, Some(ctx.input));
        // Each condition is asked once per frame however many texts use it.
        let asked: std::cell::RefCell<Vec<(String, bool)>> = Default::default();
        let dev = self.lang.dev;
        let test = |t: &text::Test| match t {
            text::Test::Keys(keys) => keys.iter().all(|k| if k.contains(':') { ctx.input.held(k) } else { crate::settings::parse_key(k).is_some_and(|key| ctx.input.held_key(key)) }),
            text::Test::If(name) => {
                if let Some(&(_, v)) = asked.borrow().iter().find(|(n, _)| n == name) {
                    return v;
                }
                let v = text_condition(name, ctx.client.as_deref(), ctx.debug, dev);
                asked.borrow_mut().push((name.clone(), v));
                v
            }
        };
        let mut uis: Vec<&mut Ui> = self.screens.iter_mut().map(|(_, u)| u).collect();
        uis.push(&mut self.hud.ui);
        for ui in uis {
            let mut relayout = false;
            let mut redraw = false;
            for n in &mut ui.nodes {
                if let Some(tip) = &mut n.tooltip {
                    redraw |= text::refresh(tip, &test);
                }
                match &mut n.kind {
                    Kind::Text(t) => {
                        if let Some(b) = &t.bind {
                            let v = b(&ctx);
                            if t.last.as_ref() != Some(&v) {
                                t.spans.clear();
                                text::message(&v.0, &v.1, &tcx, &mut t.spans);
                                t.last = Some(v);
                                relayout = true;
                            }
                        }
                        relayout |= text::refresh(&mut t.spans, &test);
                    }
                    Kind::Progress { bind, value, .. } | Kind::Icons { bind, value, .. } => {
                        let v = bind(&ctx);
                        if (v - *value).abs() > 1e-4 {
                            *value = v;
                            redraw = true;
                        }
                    }
                    Kind::Slider { bind, value, .. } => {
                        let v = bind(&ctx);
                        if v.is_finite() && (v - *value).abs() > 1e-4 {
                            *value = v;
                            redraw = true;
                        }
                    }
                    Kind::Item { bind, last } => {
                        let v = bind(&ctx);
                        if v != *last {
                            *last = v;
                            redraw = true;
                        }
                    }
                    _ => {}
                }
            }
            if relayout {
                ui.dirty = true;
            }
            if relayout || redraw {
                self.quads_dirty = true;
            }
        }
    }
}

/// `{if:name}`: the engine's conditions, then mods' (`Host::reg_text_condition`); unknown ones
/// are false.
fn text_condition(name: &str, client: Option<&Client>, debug: bool, dev: bool) -> bool {
    use crate::world::GameMode;
    let mode = client.map(|c| c.mode);
    match name {
        "creative" => mode == Some(GameMode::Creative),
        "survival" => mode == Some(GameMode::Survival),
        "spectator" => mode == Some(GameMode::Spectator),
        "debug" => debug,
        "dev" => dev,
        _ => {
            let Some(&(owner, cb)) = content().game.text_conditions.get(name) else { return false };
            let mut v = crate::client::modui::ClientView::new(client);
            let mut a = abi::TextConditionArgs { client: v.raw(), result: 0 };
            crate::host::invoke(owner, cb, &mut a);
            a.result != 0
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.run.is_some() {
            return;
        }
        match self.start(el) {
            Ok(r) => self.run = Some(r),
            Err(e) => {
                self.error = Some(e);
                el.exit();
            }
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let (Some(r), DeviceEvent::MouseMotion { delta }) = (&mut self.run, event) {
            if r.input.captured {
                r.input.mouse_delta += Vec2::new(delta.0 as f32, delta.1 as f32);
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(r) = &mut self.run else { return };
        match event {
            WindowEvent::CloseRequested => r.quit = true,
            WindowEvent::Resized(size) => {
                r.gpu.resize(size.width, size.height);
                r.rebuild_all();
            }
            WindowEvent::Focused(false) if !self.opts.automated => {
                r.input.release_all();
                if r.game.is_some() && r.screens.is_empty() {
                    r.open(screens::engine("core:pause"));
                }
                r.set_capture(false);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    match event.state {
                        ElementState::Pressed if event.repeat => r.input.key_repeat(Key::Code(code)),
                        ElementState::Pressed => r.input.key_down(Key::Code(code)),
                        ElementState::Released => r.input.key_up(Key::Code(code)),
                    }
                }
                if event.state == ElementState::Pressed {
                    if let Some(t) = &event.text {
                        r.input.typed.push_str(t);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(n) => (5 + n).min(255) as u8,
                };
                if state == ElementState::Pressed {
                    if !r.input.captured && r.screens.is_empty() && r.game.is_some() && b == 0 {
                        r.set_capture(true);
                        return;
                    }
                    r.input.key_down(Key::Mouse(b));
                } else {
                    r.input.key_up(Key::Mouse(b));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                r.input.scroll += match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0) as f32,
                };
            }
            WindowEvent::CursorMoved { position, .. } => {
                r.input.cursor = Vec2::new(position.x as f32, position.y as f32);
            }
            WindowEvent::RedrawRequested => {
                if frame(r, &self.opts) || r.quit {
                    r.leave_world_silent();
                    el.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(r) = &self.run {
            r.window.request_redraw();
        }
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        if let Some(mut r) = self.run.take() {
            r.gpu.wait_idle();
            r.leave_world_silent();
            drop(r.gpu);
        }
    }
}

/// One frame. Returns true when the app should quit.
fn frame(r: &mut Running, opts: &Options) -> bool {
    let now = Instant::now();
    let dt = (now - r.last).as_secs_f64();
    r.last = now;
    let scale = r.gui_scale();

    r.frame_no += 1;
    if crate::config::generation() != r.config_seen {
        r.config_seen = crate::config::generation();
        r.apply_settings();
    }
    r.frame_dt = dt as f32;
    // Textures first asked for while drawing the last frame.
    r.textures.load_requested(&mut *r.gpu);
    r.release_mod_keys();
    r.mod_gizmos.clear();
    r.mod_screen_text.clear();
    r.client_hooks(abi::client_hook::FRAME, "");
    if opts.automated && r.screens.is_empty() && r.game.is_some() {
        // The window may not have focus while a mod drives it.
        r.input.captured = true;
    }
    let mut requests = Vec::new();
    // Keys the top screen wants first (chat: completion, history; controls: the key to bind).
    // Escape still closes it below unless it took Escape itself.
    let mut escape_taken = false;
    if let Some((s, ui)) = r.screens.last_mut().filter(|(s, _)| s.keys_first()) {
        for k in r.input.pressed_list() {
            let mut ctx = Ctx {
                settings: &mut r.settings,
                client: r.game.as_mut().map(|g| &mut g.client),
                input: &r.input,
                requests: &mut requests,
                stats: &r.stats,
                debug: r.debug,
                top: Some(Top { screen: None, ui: &mut *ui }),
            };
            if s.key(k, &mut ctx) && k == Key::Code(KeyCode::Escape) {
                escape_taken = true;
            }
        }
        for k in r.input.released_list() {
            let mut ctx = Ctx {
                settings: &mut r.settings,
                client: r.game.as_mut().map(|g| &mut g.client),
                input: &r.input,
                requests: &mut requests,
                stats: &r.stats,
                debug: r.debug,
                top: Some(Top { screen: None, ui: &mut *ui }),
            };
            s.key_up(k, &mut ctx);
        }
        if ui.dirty {
            r.quads_dirty = true;
        }
    }
    if !escape_taken && r.input.pressed_key(Key::Code(KeyCode::Escape)) {
        if r.screens.last().is_some_and(|(s, _)| !s.escape_closes()) {
        } else if r.screens.len() > 1 || (r.game.is_some() && !r.screens.is_empty()) {
            if r.screens.last().is_some_and(|(s, _)| s.shows_menu()) {
                if let Some(g) = &mut r.game {
                    g.client.close_menu();
                }
            }
            requests.push(Request::Close);
        } else if r.game.is_some() && r.screens.is_empty() {
            r.open(screens::engine("core:pause"));
        }
    }
    // F3 alone toggles the overlay on release; F3 + digit toggles one group instead.
    let f3 = r.input.key_of("core:debug").is_some_and(|k| r.input.held_key(k.key));
    if r.input.pressed("core:debug") {
        r.f3_combo = Some(false);
    }
    let mut f3_reload = false;
    if f3 {
        if r.input.pressed_key(Key::Code(KeyCode::KeyT)) {
            f3_reload = true;
            r.f3_combo = Some(true);
        }
        // F3+F4: next game mode, through the command so permissions still apply.
        if r.input.pressed_key(Key::Code(KeyCode::F4)) {
            if let Some(g) = &mut r.game {
                let all = crate::world::GameMode::ALL;
                let next = all[(all.iter().position(|&m| m == g.client.mode).unwrap_or(0) + 1) % all.len()];
                g.client.send(net::ToServer::Chat(format!("/gamemode {}", next.name())));
            }
            r.f3_combo = Some(true);
        }
        for (key, group) in [(KeyCode::KeyG, "cubes"), (KeyCode::KeyB, "hitboxes")] {
            if r.input.pressed_key(Key::Code(key)) {
                if let Some(g) = &mut r.game {
                    g.client.toggle_gizmo(group);
                }
                r.f3_combo = Some(true);
            }
        }
        let digits = [KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4, KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9];
        let groups = crate::hud::DEBUG_GROUPS.len() + crate::hud::mod_debug_groups().len();
        for (i, k) in digits.into_iter().take(groups).enumerate() {
            if r.input.pressed_key(Key::Code(k)) {
                r.settings.set("debug_groups", (r.settings.debug_groups() ^ (1 << i)) as f64);
                r.f3_combo = Some(true);
                r.debug = true;
                r.hud.invalidate();
            }
        }
    } else if let Some(combo) = r.f3_combo.take() {
        if !combo {
            r.debug = !r.debug;
            r.hud.invalidate();
        }
    }
    if r.packs_pending && !r.screens.iter().any(|(s, _)| matches!(s.name(), "core:resource_packs" | "core:shader_packs")) {
        r.packs_pending = false;
        r.apply_settings();
    }
    if r.input.pressed("core:reload") || f3_reload || std::mem::take(&mut r.reload_requested) {
        r.assets.set_packs(crate::packs::enabled(&opts.run_dir, &r.settings.resource_packs()));
        crate::packs::set_shader_pack(r.settings.shader_pack().as_ref().map(|p| crate::packs::shader_dir(&opts.run_dir).join(p)));
        r.assets.rescan();
        crate::assets::set_current(&r.assets);
        r.hud.load_layout(&opts.run_dir);
        r.ui.reload(&mut *r.gpu, &r.assets);
        r.post.reload(&mut *r.gpu, &r.assets);
        r.lang = Lang::load(&r.assets, &r.settings.language(), r.lang.dev);
        r.look = Look::new(&r.assets, &mod_ids(), &r.lang);
        r.textures.reload(&mut *r.gpu, content(), &r.assets);
        if let Some(a) = &mut r.audio {
            a.reload();
        }
        if let Some(g) = &mut r.game {
            g.client.render.reload_shaders(&mut *r.gpu, &r.textures, &r.assets);
            g.client.render.set_textures(&mut *r.gpu, &r.textures);
            g.client.entity_render.reload(&mut *r.gpu, &r.assets);
            g.client.particles.reload(&mut *r.gpu, &r.assets);
            g.client.world.remesh_all();
        }
        r.rebuild_all();
        log::info!("assets reloaded");
    }
    if r.input.pressed("core:screenshot") {
        r.gpu.request_capture();
        r.shot_pending = true;
    }
    r.screen_input(scale, &mut requests);
    r.handle(requests);

    let paused = r.screens.iter().any(|(s, _)| s.pauses());
    let world_input = r.screens.is_empty();
    if let Some(g) = &mut r.game {
        if let Some(s) = &g.server {
            s.set_paused(paused && !s.is_hosting());
        }
        g.client.fov = r.settings.fov();
        g.client.brightness = r.settings.brightness();
        let t = Instant::now();
        if world_input {
            g.client.update(&mut *r.gpu, &r.textures, &r.input, dt);
        } else {
            g.client.update(&mut *r.gpu, &r.textures, &Input::default(), dt);
        }
        r.prof[0] = t.elapsed().as_secs_f32() * 1000.0;
        r.prof[1] = Instant::now().duration_since(now).as_secs_f32() * 1000.0;
    }
    r.entity_ticks(dt as f32);
    r.client_packets();
    if let Some(a) = &mut r.audio {
        let listener = r.game.as_ref().map(|g| g.client.listener());
        a.update(&r.assets, listener.as_ref());
        if let Some(g) = &mut r.game {
            for p in g.client.sounds.drain(..) {
                a.play(&r.assets, p);
            }
        }
        for p in r.ui_sounds.drain(..) {
            a.play(&r.assets, p);
        }
    }
    let events = r.game.as_mut().map(|g| std::mem::take(&mut g.client.events)).unwrap_or_default();
    for ev in events {
        match ev {
            crate::client::ClientEvent::OpenMenu => {
                let kind = r.game.as_ref().and_then(|g| g.client.menu.as_ref()).map_or(0, |m| m.kind);
                r.screens.retain(|(s, _)| !s.shows_menu());
                r.open(Box::new(crate::game_screens::MenuScreen::new(kind)));
            }
            crate::client::ClientEvent::CloseMenu => {
                r.screens.retain(|(s, _)| !s.shows_menu());
                r.quads_dirty = true;
                r.set_capture(true);
            }
            crate::client::ClientEvent::Died => {
                r.screens.clear();
                r.open(screens::engine("core:death"));
            }
        }
    }
    if r.game.is_some() && r.screens.is_empty() {
        let command = r.input.pressed("core:command");
        let f3_held = r.input.key_of("core:debug").is_some_and(|k| r.input.held_key(k.key));
        if command || (r.input.pressed("core:chat") && !f3_held) {
            r.open(Box::new(crate::game_screens::Chat::new(if command { "/" } else { "" })));
            r.input.typed.clear();
        }
    }
    let (top_chat, top_menu) = r.screens.last().map_or((false, false), |(s, _)| (s.shows_chat(), s.shows_menu()));
    if top_chat && r.input.pressed_key(Key::Code(KeyCode::Enter)) {
        let text = r.screens.last().and_then(|(_, ui)| {
            ui.find("chat").and_then(|n| match &ui.nodes[n].kind {
                Kind::Input { value, .. } => Some(value.clone()),
                _ => None,
            })
        });
        r.screens.pop();
        r.set_capture(true);
        r.quads_dirty = true;
        if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
            if let Some(g) = &mut r.game {
                if g.client.sent.last() != Some(&t) {
                    g.client.sent.push(t.clone());
                    let sent = &mut g.client.sent;
                    sent.drain(..sent.len().saturating_sub(CHAT_HISTORY));
                    std::fs::write(chat_history_path(&r.settings.run_dir), sent.join("
")).ok();
                }
            }
            r.handle(vec![Request::SendChat(t)]);
        }
    }
    let typing = r.screens.last().is_some_and(|(_, ui)| ui.focused.is_some_and(|f| matches!(ui.nodes.get(f).map(|n| &n.kind), Some(Kind::Input { .. }))));
    if top_menu && r.input.pressed("core:inventory") && !typing {
        if let Some(g) = &mut r.game {
            g.client.close_menu();
        }
        r.screens.retain(|(s, _)| !s.shows_menu());
        r.set_capture(true);
        r.quads_dirty = true;
    }
    if let Some(reason) = r.game.as_ref().and_then(|g| g.client.disconnected.clone()) {
        log::info!("disconnected: {reason}");
        r.leave_world_silent();
        r.screens.clear();
        r.open(screens::engine("core:title"));
        r.open(Box::new(screens::Message { title: "multiplayer.disconnected".into(), args: vec![Arg::Text(reason)] }));
    }
    r.input.sensitivity = r.settings.sensitivity();
    r.input.end_frame();
    let top = (r.screens.len(), r.screens.last().map(|(s, _)| s.name().to_string()).unwrap_or_default());
    if top != r.screen_seen {
        let name = top.1.clone();
        r.screen_seen = top;
        r.screen_frames = 0;
        r.client_hooks(abi::client_hook::SCREEN, &name);
    } else {
        r.screen_frames = r.screen_frames.saturating_add(1);
    }

    if let Some(g) = &r.game {
        let chat_open = r.screens.last().is_some_and(|(s, _)| s.shows_chat());
        if r.hud.update(&g.client, r.debug, r.settings.debug_groups(), chat_open, &r.lang, &r.input) {
            r.quads_dirty = true;
        }
        let mut h = g.client.selected as u64;
        for s in g.client.inventory.iter().chain(g.client.menu.iter().flat_map(|m| m.slots.iter().chain([&m.cursor]))) {
            h = crate::world::mix(h ^ ((s.item as u64) << 32 | s.count as u64) ^ (s.data as u64) << 48);
        }
        if h != r.client_hash {
            r.client_hash = h;
            r.quads_dirty = true;
        }
    }
    if r.textures.generation() != r.ui_textures {
        r.ui_textures = r.textures.generation();
        r.quads_dirty = true;
    }
    r.poll_bindings();
    let surface = r.gpu.surface_size();
    let gui_size = Vec2::new(surface.0 as f32, surface.1 as f32) / scale;
    // Whole physical pixels per pixel of the font's design.
    let p = r.ui.font.pixel;
    let text_unit = (scale * 0.5 * p).round().max(1.0) / (scale * p);
    {
        let mut uis: Vec<&mut Ui> = Vec::new();
        if r.game.is_some() {
            uis.push(&mut r.hud.ui);
        }
        uis.extend(r.screens.iter_mut().map(|(_, u)| u));
        for ui in uis {
            if ui.dirty || ui.size != gui_size || ui.text_unit != text_unit {
                ui.text_unit = text_unit;
                ui.layout(gui_size, &r.ui.font);
                ui.dirty = false;
                r.quads_dirty = true;
            }
        }
    }

    let t = Instant::now();
    r.prof[1] = (t.duration_since(now).as_secs_f32() * 1000.0 - r.prof[1]).max(0.0);
    let Some(info) = r.gpu.begin_frame() else { return false };
    let t2 = Instant::now();
    r.prof[2] = t2.duration_since(t).as_secs_f32() * 1000.0;
    if (r.depth.1, r.depth.2) != (info.width, info.height) {
        r.gpu.destroy_texture(r.depth.0);
        r.depth = make_depth(&mut *r.gpu, info.width, info.height);
    }
    if r.quads_dirty {
        r.quads_dirty = false;
        let mut all = Vec::new();
        let game = crate::content::try_content().map(|c| (c, &r.textures));
        let client = r.game.as_ref().map(|g| &g.client);
        let mut previews = Vec::new();
        let preview_tex = r.preview_tex;
        let mut layers: Vec<&Ui> = Vec::new();
        let mut animated = Vec::new();
        if r.game.is_some() && !hud_hidden() {
            layers.push(&r.hud.ui);
        }
        // Only the top screen shows, like vanilla: the ones under it are not drawn through its
        // see-through overlay.
        layers.extend(r.screens.last().map(|(_, u)| u));
        for ui in layers {
            let look = &r.look;
            let textures = &r.textures;
            let unit = ui.text_unit;
            let mut custom = |ren: &mut UiRenderer, tag: u32, data: u64, rect: crate::ui::Rect, hovered: bool, icon: Option<u32>| {
                crate::hud::draw_custom(ren, look, textures, client, tag, data, rect, hovered, icon, scale, unit);
                if tag == crate::client::modui::TAG_ENTITY {
                    let inner = crate::ui::Rect { x: rect.x + scale, y: rect.y + scale, w: rect.w - 2.0 * scale, h: rect.h - 2.0 * scale };
                    previews.push([inner.x, inner.y, inner.w, inner.h]);
                    if let Some(t) = preview_tex {
                        ren.image(inner, t, [0.0, 0.0, 1.0, 1.0], 0xFFFFFFFF);
                    }
                }
            };
            let base = all.len();
            r.ui.build(&mut *r.gpu, &r.assets, ui, &r.look, scale, game, &mut custom);
            all.append(&mut r.ui.quads);
            animated.extend(r.ui.animated.drain(..).map(|mut a| {
                a.quads = a.quads.start + base..a.quads.end + base;
                a
            }));
        }
        r.ui_cached = all;
        r.ui_animated = animated;
        r.previews = previews;
    }
    // The stack held by the mouse follows it every frame, outside the cached quads.
    r.ui.quads.clear();
    r.ui.quads.extend_from_slice(&r.ui_cached);
    crate::ui::render::animate(&mut r.ui.quads, &r.ui_animated);
    // The hurt flash fades every frame, so it stays out of the cache, under the rest.
    if let Some(flash) = r.game.as_ref().map(|g| g.client.hurt_flash).filter(|&f| f > 0.0) {
        if let Some((tex, _, _)) = r.ui.texture(&mut *r.gpu, &r.assets, "core:gui/vignette") {
            let cached = std::mem::take(&mut r.ui.quads);
            r.ui.image(crate::ui::Rect { x: 0.0, y: 0.0, w: surface.0 as f32, h: surface.1 as f32 }, tex, [0.0, 0.0, 1.0, 1.0], 0xD0101000 | (flash / 0.5 * 200.0) as u32);
            r.ui.quads.extend_from_slice(&cached);
        }
    }
    if let Some(g) = &mut r.game {
        g.client.collect_gizmos();
        let cl = &mut g.client;
        for s in &r.mod_gizmos {
            crate::client::gizmo::shape(s, &mut cl.gizmo_lines, &mut cl.gizmo_fills, &mut cl.gizmo_labels);
        }
    }
    // Gizmo labels and mods' screen text: text nodes placed at points (GUI pixels), the same
    // text as anywhere else.
    let mut labels: Vec<(f32, f32, &crate::ui::text::Msg, u32, f32)> = Vec::new();
    if let Some(g) = &r.game {
        let cam = g.client.render_camera();
        let view_proj = cam.proj(info.width as f32 / info.height.max(1) as f32) * cam.view();
        for (pos, text, color, size) in &g.client.gizmo_labels {
            let clip = view_proj * (*pos - cam.pos).as_vec3().extend(1.0);
            if clip.w < 0.1 || (*pos - cam.pos).length_squared() > 24.0 * 24.0 {
                continue;
            }
            let (x, y) = ((clip.x / clip.w * 0.5 + 0.5) * info.width as f32, (0.5 - clip.y / clip.w * 0.5) * info.height as f32);
            labels.push((x / scale, y / scale - 4.0, text, *color, *size));
        }
    }
    let (sw, sh) = r.gpu.surface_size();
    labels.extend(r.mod_screen_text.iter().map(|(x, y, text, color, size)| (x * sw as f32 / scale, y * sh as f32 / scale, text, *color, *size)));
    if !labels.is_empty() {
        let mut o = crate::ui::Ui::new();
        o.text_unit = text_unit;
        for (x, y, text, color, size) in labels {
            let id = o.text(text.spans(&r.lang, Some(&r.input)), "label");
            o.nodes[id].style = crate::ui::Style::default().at(x, y, [0.5, 0.0]);
            if let crate::ui::Kind::Text(t) = &mut o.nodes[id].kind {
                t.wrap = false;
                t.color = Some(color);
                t.scale = size;
            }
        }
        o.layout(Vec2::new(sw as f32 / scale, sh as f32 / scale), &r.ui.font);
        let game = crate::content::try_content().map(|c| (c, &r.textures));
        r.ui.append(&mut *r.gpu, &r.assets, &o, &r.look, scale, game, &mut |_, _, _, _, _, _| {});
    }
    if let Some((_, ui)) = r.screens.last() {
        let game = crate::content::try_content().map(|c| (c, &r.textures));
        r.ui.tooltip(&mut *r.gpu, &r.assets, ui, &r.look, scale, r.input.cursor, game);
    }
    if let Some(cursor) = r.game.as_ref().and_then(|g| g.client.menu.as_ref()).map(|m| m.cursor).filter(|s| !s.is_empty()) {
        let p = r.input.cursor;
        let rect = crate::ui::Rect { x: p.x - 9.0 * scale, y: p.y - 9.0 * scale, w: 18.0 * scale, h: 18.0 * scale };
        crate::hud::stack_icon(&mut r.ui, &r.textures, rect, cursor, scale, text_unit);
    }

    r.list.clear();
    if let Some(g) = r.game.as_mut().filter(|_| !r.screens.is_empty()) {
        if let Some(&[x, y, w, h]) = r.previews.first() {
            let worn = g.client.worn();
            let look = r.input.cursor - Vec2::new(x, y);
            let tex = g.client.entity_render.draw_preview(&mut *r.gpu, &mut r.list, &info, [w.round() as u32, h.round() as u32], look, &worn);
            if tex != r.preview_tex {
                r.preview_tex = tex;
                r.quads_dirty = true;
            }
        }
    }
    let clear = r.game.as_ref().map_or([0.02, 0.02, 0.03, 1.0], |g| g.client.fog_color());
    // Screen effects need the world in an image they can read.
    let post = r.post.on() && r.game.is_some();
    let world_target = if post { Target::Texture(r.post.scene_target(&mut *r.gpu, info.width, info.height)) } else { Target::Surface };
    if let Some(g) = &mut r.game {
        g.client.gpu_terrain.frame(&mut *r.gpu, &mut r.list, info.slot);
    }
    r.list.push(Cmd::BeginPass { color: Some((world_target, Some(clear))), depth: Some((r.depth.0, Some(0.0))) });
    if let Some(g) = &mut r.game {
        let worn = g.client.worn();
        let held = g.client.held().item;
        g.client.entity_render.preload(&mut *r.gpu, &r.assets, &g.client.entities, &worn, held);
        g.client.render(&mut *r.gpu, &mut r.list, &info, &r.textures);
    }
    r.list.push(Cmd::EndPass);
    if post {
        r.post.apply(&mut *r.gpu, &mut r.list, &info);
    } else {
        // The UI pipeline has no depth, so it cannot draw in a pass that has one.
        r.list.push(Cmd::BeginPass { color: Some((Target::Surface, None)), depth: None });
    }
    r.ui.draw(&mut *r.gpu, &mut r.list, &info, scale);
    r.list.push(Cmd::Mark);
    r.list.push(Cmd::EndPass);

    let capture_now = r.shot_pending;
    let t3 = Instant::now();
    r.prof[3] = t3.duration_since(t2).as_secs_f32() * 1000.0;
    r.gpu.end_frame(&r.list);
    r.prof[4] = t3.elapsed().as_secs_f32() * 1000.0;
    if let Some(g) = &r.game {
        r.diag.frame(&crate::diag::FrameSample {
            frame_ms: dt as f32 * 1000.0,
            cpu: &r.prof,
            gpu: r.gpu.gpu_times(),
            client_cubes: g.client.world.len(),
            client_entities: g.client.entities.len(),
            pending_meshes: if crate::log::on(crate::log::Channel::Mesh, log::Level::Debug) { g.client.pending_meshes() } else { 0 },
            server: g.server.as_ref().map(|s| &*s.shared),
        });
    }
    if let Some(b) = &mut r.bench {
        let server = r.game.as_ref().and_then(|g| g.server.as_ref());
        let (server_ms, phases) = server.map_or((0.0, [0.0; 6]), |s| (s.tick_ms(), s.phases()));
        b.sample(dt as f32 * 1000.0, &r.prof, r.gpu.gpu_times(), server_ms, phases, &r.list);
        if b.done() {
            let extra = match &r.game {
                Some(g) => format!(
                    "pos {:.1} {:.1} {:.1}, cubes {}, draws {}, faces {}k, entities {}, regions {}",
                    g.client.player.pos.x,
                    g.client.player.pos.y,
                    g.client.player.pos.z,
                    g.client.world.len(),
                    g.client.render.drawn,
                    g.client.render.faces_drawn / 1000,
                    g.client.entities.len(),
                    g.server.as_ref().map_or(0, |s| s.regions())
                ),
                None => String::new(),
            };
            let line = b.report(&extra);
            log::info!("{line}");
            r.bench_last = b.summary();
            r.bench_line = line;
            r.benches += 1;
            r.bench = None;
        }
    }
    if capture_now {
        r.shot_pending = false;
        if let Some(cap) = r.gpu.take_capture() {
            let path = r.shot_path.take().unwrap_or_else(|| {
                r.settings.run_dir.join("screenshots").join(format!("{}.png", saves::now_secs()))
            });
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match crate::assets::encode_png(&path, cap.width, cap.height, &cap.rgba) {
                Ok(()) => log::info!("screenshot saved to {}", path.display()),
                Err(e) => log::error!("screenshot: {e}"),
            }
            if let Some(id) = r.shot_image.take() {
                api::store_image(id, cap.width, cap.height, cap.rgba);
            }
        }
    }
    r.fps_frames += 1;
    let el = r.fps_timer.elapsed().as_secs_f32();
    if el >= 0.5 {
        r.fps = r.fps_frames as f32 / el;
        r.frame_ms = el * 1000.0 / r.fps_frames as f32;
        r.fps_timer = Instant::now();
        r.fps_frames = 0;
    }
    false
}

/// Lines typed into chat, kept across sessions for the arrow keys.
const CHAT_HISTORY: usize = 100;

fn chat_history_path(run_dir: &std::path::Path) -> PathBuf {
    run_dir.join("config").join("chat_history.txt")
}

fn load_chat_history(run_dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(chat_history_path(run_dir)).map(|s| s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}
