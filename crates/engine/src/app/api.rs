//! Client side of the mod ABI: what client callbacks see of the running game, and what frame
//! hooks may do with it (input, the local player, worlds, screens, screenshots).

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use glam::{DVec3, IVec3, Vec2};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use loom_abi::{self as abi, ItemStack, Slice, Str, client_flags as cf};

use super::{Request, Running};
use crate::client::Client;
use crate::client::modui::ClientView;
use crate::input::Key;
use winit::keyboard::KeyCode;
use crate::ui::Kind;

static LAUNCH: OnceLock<FxHashMap<String, String>> = OnceLock::new();
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

/// `--name value` pairs the engine itself does not know, for mods (`Host::launch_option`).
pub fn set_launch_options(o: Vec<(String, String)>) {
    let _ = LAUNCH.set(o.into_iter().collect());
}

pub fn launch_options() -> &'static FxHashMap<String, String> {
    LAUNCH.get_or_init(FxHashMap::default)
}

/// Exit code a mod asked for with `client_op::QUIT`.
pub fn exit_code() -> i32 {
    EXIT_CODE.load(Ordering::Relaxed)
}

pub unsafe extern "C" fn launch_option(name: Str) -> Str {
    let name = unsafe { name.as_str() };
    launch_options().get(name).map_or(Str::EMPTY, |v| Str::new(v))
}

// ---------------------------------------------------------------- images

static IMAGES: Mutex<Option<FxHashMap<u64, (u32, u32, Vec<u8>)>>> = Mutex::new(None);
static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);

pub fn new_image_id() -> u64 {
    NEXT_IMAGE.fetch_add(1, Ordering::Relaxed)
}

pub fn store_image(id: u64, w: u32, h: u32, rgba: Vec<u8>) {
    IMAGES.lock().get_or_insert_with(FxHashMap::default).insert(id, (w, h, rgba));
}

pub unsafe extern "C" fn image_get(id: u64, w: *mut u32, h: *mut u32) -> *const u8 {
    let g = IMAGES.lock();
    match g.as_ref().and_then(|m| m.get(&id)) {
        Some((iw, ih, px)) => {
            unsafe {
                *w = *iw;
                *h = *ih;
            }
            // The pixel buffer stays put until the image is freed, whatever the map does.
            px.as_ptr()
        }
        None => std::ptr::null(),
    }
}

pub unsafe extern "C" fn image_load(path: Str) -> u64 {
    let path = unsafe { path.as_str() };
    match crate::assets::decode_png_file(std::path::Path::new(path)) {
        Some((w, h, px)) => {
            let id = new_image_id();
            store_image(id, w, h, px);
            id
        }
        None => 0,
    }
}

pub unsafe extern "C" fn image_save(path: Str, rgba: Slice<u8>, w: u32, h: u32) -> u32 {
    let (path, px) = unsafe { (path.as_str(), rgba.as_slice()) };
    if px.len() != (w * h * 4) as usize {
        return 0;
    }
    crate::assets::encode_png(std::path::Path::new(path), w, h, px).is_ok() as u32
}

pub unsafe extern "C" fn image_free(id: u64) {
    if let Some(m) = IMAGES.lock().as_mut() {
        m.remove(&id);
    }
}

// ---------------------------------------------------------------- context

/// Frame hooks get a `ClientView` whose `run` points at the whole client: these functions only
/// act when it does (UI bindings see a view without it).
unsafe fn view<'a>(c: *mut abi::ClientCtx) -> Option<&'a ClientView> {
    unsafe { (c as *const ClientView).as_ref() }
}

unsafe fn client<'a>(c: *mut abi::ClientCtx) -> Option<&'a Client> {
    unsafe { view(c).and_then(|v| v.client.as_ref()) }
}

pub(crate) unsafe fn run<'a>(c: *mut abi::ClientCtx) -> Option<&'a mut Running> {
    unsafe { view(c).and_then(|v| (v.run as *mut Running).as_mut()) }
}

/// The answer to a command run with `client_op::RUN_COMMAND`, once it came: its lines in the
/// client's language, one per line.
pub unsafe extern "C" fn client_command_reply(c: *mut abi::ClientCtx, id: u32, out: *mut Str) -> u32 {
    unsafe {
        let Some(r) = run(c) else { return 0 };
        let Some(g) = r.game.as_mut() else { return 0 };
        let Some(lines) = g.client.command_replies.remove(&id) else { return 0 };
        let text: Vec<String> = lines.iter().map(|m| m.text(&r.lang)).collect();
        *out = text_out(&text.join("\n"));
        1
    }
}

thread_local! {
    static TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

fn text_out(s: &str) -> Str {
    TEXT.with(|t| {
        let mut t = t.borrow_mut();
        t.clear();
        t.push_str(s);
        Str { ptr: t.as_ptr(), len: t.len() }
    })
}

impl Running {
    /// Whether the world around the player is loaded and meshed, and whether everything is idle.
    pub(super) fn load_state(&self) -> (bool, bool) {
        let loaded = self.game.as_ref().is_some_and(|g| {
            let terrain = g.client.terrain.pending.load(std::sync::atomic::Ordering::Relaxed);
            g.client.joined && g.client.world.len() > 50 && g.client.pending_meshes() == 0 && terrain == 0
        });
        let idle = loaded && self.game.as_ref().is_some_and(|g| g.server.as_ref().is_none_or(|s| s.idle()));
        (loaded, idle)
    }

    fn hold(&mut self, k: Key, secs: f32) {
        // A key still held from the last tap would not count as pressed again.
        self.input.key_up(k);
        self.input.key_down(k);
        let until = Instant::now() + Duration::from_secs_f32(secs.max(0.05));
        self.mod_held.retain(|(h, _)| *h != k);
        self.mod_held.push((k, until));
    }

    /// Releases keys mods held down once their time is up.
    pub(super) fn release_mod_keys(&mut self) {
        let now = Instant::now();
        let input = &mut self.input;
        self.mod_held.retain(|(k, at)| {
            if now >= *at {
                input.key_up(*k);
                false
            } else {
                true
            }
        });
    }
}

pub unsafe extern "C" fn client_state(c: *mut abi::ClientCtx, out: *mut abi::ClientState) {
    unsafe {
        let mut s = abi::ClientState::DEFAULT;
        if let Some(r) = run(c) {
            let (loaded, idle) = r.load_state();
            let mut flags = 0;
            if loaded {
                flags |= cf::LOADED;
            }
            if idle {
                flags |= cf::IDLE;
            }
            if let Some((_, ui)) = r.screens.last() {
                if r.screen_frames >= 2 && !ui.dirty {
                    flags |= cf::SCREEN_READY;
                }
            }
            if crate::net::current_ticket().is_some() {
                flags |= cf::HOSTING;
            }
            if r.debug {
                flags |= cf::DEBUG;
            }
            if r.audio.as_ref().is_some_and(|a| a.music_playing()) {
                flags |= cf::MUSIC;
            }
            let single = r.game.as_ref().and_then(|g| g.server.as_ref()).is_some_and(|s| !s.is_hosting());
            if single && r.screens.iter().any(|(s, _)| s.pauses()) {
                flags |= cf::PAUSED;
            }
            s.sounds = r.audio.as_ref().map_or(0, |a| a.played);
            s.flags = flags;
            s.frame = r.frame_no;
            s.dt = r.frame_dt;
            s.fps = r.fps;
            s.frame_ms = r.frame_ms;
            s.gpu_ms = r.gpu.gpu_times().iter().sum();
            s.window = r.gpu.surface_size().into();
            s.gui_scale = r.gui_scale();
            s.screen = r.screens.last().map_or(Str::EMPTY, |(sc, _)| Str::new(sc.name()));
            s.screen_frames = r.screen_frames;
            (s.benches, [s.bench_fps, s.bench_avg_ms, s.bench_p99_ms, s.bench_gpu_ms, s.bench_server_ms]) = (r.benches, r.bench_last);
            if let Some(srv) = r.game.as_ref().and_then(|g| g.server.as_ref()) {
                s.server_ms = srv.tick_ms();
                let (g, gus, i, ius) = srv.load_stats();
                (s.generated, s.gen_us, s.inserted, s.insert_us) = (g, gus as f32, i, ius as f32);
            }
        }
        if let Some(cl) = client(c) {
            s.flags |= cf::IN_WORLD;
            let p = &cl.player;
            let bit = |on: bool, f: u32| if on { f } else { 0 };
            s.flags |= bit(cl.joined, cf::JOINED) | bit(cl.dead, cf::DEAD) | bit(p.flying, cf::FLYING) | bit(p.on_ground, cf::ON_GROUND) | bit(cl.menu.is_some(), cf::MENU_OPEN);
            s.dim = cl.world.dim;
            s.player = cl.player_id;
            s.pos = p.pos.to_array();
            s.shown = cl.smooth_pos().to_array();
            if let Some(h) = cl.target {
                (s.target, s.target_frame) = (h.block.to_array(), h.frame);
            }
            s.vel = p.vel.to_array();
            s.yaw = p.yaw;
            s.pitch = p.pitch;
            s.selected = cl.selected as u32;
            s.mode = cl.mode as u32;
            s.health = cl.health;
            s.cubes = cl.world.len() as u32;
            s.pending_meshes = cl.pending_meshes() as u32;
            s.gpu_cubes = crate::client::terrain_gpu::GPU_CUBES.load(std::sync::atomic::Ordering::Relaxed);
            s.gpu_mismatches = crate::client::terrain_gpu::GPU_MISMATCHES.load(std::sync::atomic::Ordering::Relaxed);
            let (shown, _, _, pending) = cl.lod_stats();
            (s.lod_tiles, s.lod_pending) = (shown as u32, pending as u32);
            s.entities = cl.entities.len() as u32;
            s.particles = cl.particles.len() as u32;
        }
        *out = s;
    }
}

pub unsafe extern "C" fn client_input(c: *mut abi::ClientCtx, op: u32, text: Str, x: f32, y: f32) -> u32 {
    use abi::client_input as ci;
    unsafe {
        let Some(r) = run(c) else { return 0 };
        let text = text.as_str();
        match op {
            ci::ACTION => match r.input.key_of(text) {
                Some(c) => {
                    for (bit, k) in [(crate::input::CTRL, KeyCode::ControlLeft), (crate::input::SHIFT, KeyCode::ShiftLeft), (crate::input::ALT, KeyCode::AltLeft)] {
                        if c.mods & bit != 0 {
                            r.hold(Key::Code(k), x);
                        }
                    }
                    r.hold(c.key, x)
                }
                None => return 0,
            },
            ci::KEY => match crate::settings::parse_key(text) {
                Some(k) => r.hold(k, x),
                None => return 0,
            },
            ci::CLICK => r.hold(Key::Mouse(y.clamp(0.0, 255.0) as u8), x),
            ci::CURSOR => {
                let (w, h) = r.gpu.surface_size();
                r.input.cursor = Vec2::new(x * w as f32, y * h as f32);
            }
            ci::SCROLL => r.input.scroll += x,
            ci::TYPE => r.input.typed.push_str(text),
            ci::RELEASE_ALL => {
                r.input.release_all();
                r.mod_held.clear();
            }
            _ => return 0,
        }
        1
    }
}

pub unsafe extern "C" fn client_pressed(c: *mut abi::ClientCtx, action: Str) -> u32 {
    unsafe {
        let Some(r) = run(c) else { return 0 };
        // Mod keys belong to the world: while a screen (chat, inventory) has the input they stay up.
        if !r.input.captured || !r.screens.is_empty() {
            return 0;
        }
        let a = action.as_str();
        r.input.pressed(a) as u32 | (r.input.held(a) as u32) << 1
    }
}

pub unsafe extern "C" fn client_set_player(c: *mut abi::ClientCtx, control: *const abi::PlayerControl) {
    use abi::control_mask as m;
    unsafe {
        let Some(r) = run(c) else { return };
        let Some(g) = r.game.as_mut() else { return };
        let ctl = *control;
        let p = &mut g.client.player;
        if ctl.mask & m::POS != 0 && DVec3::from_array(ctl.pos).is_finite() {
            p.pos = DVec3::from_array(ctl.pos);
            p.prev = p.pos;
        }
        if ctl.mask & m::VEL != 0 && DVec3::from_array(ctl.vel).is_finite() {
            p.vel = DVec3::from_array(ctl.vel);
        }
        if ctl.mask & m::LOOK != 0 {
            p.yaw = ctl.yaw;
            p.pitch = ctl.pitch.clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
        }
        if ctl.mask & m::FLYING != 0 {
            p.flying = ctl.flying != 0;
        }
        if ctl.mask & m::FLY_SPEED != 0 {
            p.fly_speed = ctl.fly_speed.max(0.0);
        }
        if ctl.mask & m::SELECTED != 0 && ctl.selected < crate::content::content().game.player_layout.hotbar {
            g.client.selected = ctl.selected as usize;
            g.client.send(crate::net::ToServer::Select(ctl.selected));
        }
    }
}

pub unsafe extern "C" fn client_command(c: *mut abi::ClientCtx, op: u32, text: Str, num: f64) -> u64 {
    use abi::client_op as op_;
    unsafe {
        let Some(r) = run(c) else { return 0 };
        let text = text.as_str().to_string();
        match op {
            op_::CHAT => r.handle(vec![Request::SendChat(text)]),
            op_::SCREENSHOT => {
                let dir = r.settings.run_dir.join("screenshots");
                let path = if text.is_empty() {
                    dir.join(format!("{}.png", crate::saves::now_secs()))
                } else {
                    let p = std::path::PathBuf::from(&text);
                    if p.is_absolute() || p.components().count() > 1 { p } else { dir.join(p) }
                };
                let id = new_image_id();
                r.shot_path = Some(path);
                r.shot_image = Some(id);
                r.gpu.request_capture();
                r.shot_pending = true;
                return id;
            }
            op_::QUIT => {
                EXIT_CODE.store(num as i32, Ordering::Relaxed);
                r.quit = true;
            }
            op_::OPEN_WORLD | op_::NEW_WORLD => {
                r.leave_world_silent();
                let dir = r.settings.saves_dir();
                let existing = crate::saves::list(&dir).into_iter().find(|w| w.name == text);
                let meta = match existing {
                    Some(w) if op == op_::NEW_WORLD => {
                        let _ = std::fs::remove_dir_all(&w.dir);
                        crate::saves::create(&dir, &text, num as u64)
                    }
                    Some(w) => w,
                    None => crate::saves::create(&dir, &text, num as u64),
                };
                r.start_world(meta);
            }
            op_::LEAVE_WORLD => r.leave_world(),
            op_::HOST => r.handle(vec![Request::Host]),
            op_::JOIN => r.handle(vec![Request::Join(text)]),
            op_::OPEN_SCREEN => {
                if text == "core:inventory" {
                    if let Some(g) = r.game.as_mut() {
                        g.client.send(crate::net::ToServer::OpenInventory);
                    }
                    return 1;
                }
                let Some(s) = crate::screens::named(&text) else {
                    log::warn!("client_op::OPEN_SCREEN: no screen {text}");
                    return 0;
                };
                r.open(s);
            }
            op_::CLOSE_SCREEN => {
                if r.screens.last().is_some_and(|(s, _)| s.shows_menu()) {
                    if let Some(g) = r.game.as_mut() {
                        g.client.close_menu();
                    }
                }
                if !r.screens.is_empty() {
                    r.handle(vec![Request::Close]);
                }
            }
            op_::RELOAD_ASSETS => r.reload_requested = true,
            op_::POST_EFFECT => return r.post.set(&text, num as f32) as u64,
            op_::BENCH => r.bench = Some(crate::bench::Bench::new(&text, num.max(0.1) as f32)),
            op_::RESPAWN => r.handle(vec![Request::Respawn]),
            op_::RUN_COMMAND => return r.run_command(&text) as u64,
            _ => return 0,
        }
        1
    }
}

pub unsafe extern "C" fn client_text(c: *mut abi::ClientCtx, which: u32, index: u32) -> Str {
    use abi::client_text as ct;
    unsafe {
        match which {
            ct::TICKET => text_out(&crate::net::current_ticket().unwrap_or_default()),
            ct::RUN_DIR => match run(c) {
                Some(r) => text_out(&r.settings.run_dir.to_string_lossy()),
                None => Str::EMPTY,
            },
            // In the client's language, as the player reads it.
            ct::CHAT => match (client(c), run(c)) {
                (Some(cl), Some(r)) => cl.chat.iter().rev().nth(index as usize).map_or(Str::EMPTY, |(m, _)| text_out(&m.text(&r.lang))),
                _ => Str::EMPTY,
            },
            ct::BENCH => match run(c) {
                Some(r) => text_out(&r.bench_line),
                None => Str::EMPTY,
            },
            ct::INPUT => {
                let field = run(c).and_then(|r| r.screens.last()).and_then(|(_, ui)| {
                    let n = ui.focused.or_else(|| ui.nodes.iter().position(|n| matches!(n.kind, Kind::Input { .. })))?;
                    match &ui.nodes[n].kind {
                        Kind::Input { value, .. } => Some(value.clone()),
                        _ => None,
                    }
                });
                field.map_or(Str::EMPTY, |v| text_out(&v))
            }
            _ => Str::EMPTY,
        }
    }
}

pub unsafe extern "C" fn client_block(c: *mut abi::ClientCtx, pos: *const [i32; 3]) -> u32 {
    unsafe { client(c).map_or(u32::MAX, |cl| cl.world.block(IVec3::from_array(*pos))) }
}

pub unsafe extern "C" fn client_light(c: *mut abi::ClientCtx, pos: *const [i32; 3]) -> u32 {
    unsafe {
        let p = IVec3::from_array(*pos);
        client(c).map_or(u32::MAX, |cl| if cl.world.block(p) == u32::MAX { u32::MAX } else { cl.world.light(p) })
    }
}

pub unsafe extern "C" fn client_slot(c: *mut abi::ClientCtx, container: u32, slot: u32) -> ItemStack {
    unsafe {
        let Some(cl) = client(c) else { return ItemStack::EMPTY };
        let s = match container {
            0 => cl.inventory.get(slot as usize).copied(),
            1 => cl.menu.as_ref().and_then(|m| m.slots.get(slot as usize)).copied(),
            2 => cl.menu.as_ref().map(|m| m.cursor),
            _ => None,
        };
        s.unwrap_or_default()
    }
}

pub unsafe extern "C" fn client_menu(c: *mut abi::ClientCtx, out: *mut [u32; 3]) -> u32 {
    unsafe {
        match client(c).and_then(|cl| cl.menu.as_ref()) {
            Some(m) => {
                *out = [m.kind, m.window, m.slots.len() as u32];
                1
            }
            None => 0,
        }
    }
}

pub unsafe extern "C" fn client_entities(c: *mut abi::ClientCtx, out: *mut u64, max: usize) -> usize {
    unsafe {
        let Some(cl) = client(c) else { return 0 };
        let mut ids: Vec<u64> = cl.entities.keys().copied().collect();
        ids.sort_unstable();
        for (i, id) in ids.iter().take(max).enumerate() {
            *out.add(i) = *id;
        }
        ids.len()
    }
}

pub unsafe extern "C" fn client_ui_rect(c: *mut abi::ClientCtx, query: Str, out: *mut [f32; 4]) -> u32 {
    unsafe {
        let Some(r) = run(c) else { return 0 };
        let ui = r.screens.last().map_or(&r.hud.ui, |s| &s.1);
        let q = query.as_str();
        let found = if let Some(n) = q.strip_prefix("slot:").and_then(|n| n.parse::<u64>().ok()) {
            ui.nodes.iter().position(|node| matches!(node.kind, Kind::Custom { tag: crate::client::modui::TAG_SLOT, data } if data == n))
        } else if let Some(n) = q.strip_prefix("item:").and_then(|n| n.parse::<u64>().ok()) {
            ui.nodes.iter().position(|node| matches!(node.kind, Kind::Custom { tag: crate::client::modui::TAG_CREATIVE, data } if data == n))
        } else if let Some(n) = q.strip_prefix("icon:").and_then(|n| n.parse::<u64>().ok()) {
            ui.nodes.iter().position(|node| match node.kind {
                Kind::Custom { tag: crate::client::modui::TAG_ICON, data } => data == n,
                Kind::Item { last, .. } => last.0 as u64 == n,
                _ => false,
            })
        } else if q == "slider" {
            ui.nodes.iter().position(|node| matches!(node.kind, Kind::Slider { .. }))
        } else if q == "entity" {
            ui.nodes.iter().position(|node| matches!(node.kind, Kind::Custom { tag: crate::client::modui::TAG_ENTITY, .. }))
        } else if let Some(id) = q.strip_prefix("id:") {
            ui.find(id)
        } else if let Some(class) = q.strip_prefix("class:") {
            ui.nodes.iter().position(|node| match &node.kind {
                Kind::Container { class: Some(c) } | Kind::Button { class: c, .. } | Kind::Progress { class: c, .. } | Kind::Icons { class: c, .. } => c == class,
                _ => false,
            })
        } else if let Some(words) = q.strip_prefix("text:") {
            let words = words.to_lowercase();
            ui.nodes.iter().position(|node| match &node.kind {
                Kind::Text(t) => {
                    let s: String = t.spans.iter().filter_map(|s| if let crate::ui::text::Piece::Text(x) = &s.piece { Some(x.as_str()) } else { None }).collect();
                    s.to_lowercase().contains(&words)
                }
                _ => false,
            })
        } else {
            None
        };
        // `scrollbar`: the track of the first scroll bar shown.
        let bar = (q == "scrollbar").then(|| ui.nodes.iter().find_map(|n| n.scrollbar.map(|b| b.track))).flatten();
        let Some(rect) = bar.or_else(|| found.map(|i| ui.nodes[i].rect)) else { return 0 };
        let scale = r.gui_scale();
        let (w, h) = r.gpu.surface_size();
        *out = [rect.x * scale / w as f32, rect.y * scale / h as f32, rect.w * scale / w as f32, rect.h * scale / h as f32];
        1
    }
}

pub unsafe extern "C" fn client_draw(c: *mut abi::ClientCtx, group: Str, kind: u32, a: *const [f64; 3], b: *const [f64; 3], color: u32, key: Str, args: Slice<abi::UiArg>) {
    unsafe {
        let Some(r) = run(c) else { return };
        let group = group.as_str();
        if !group.is_empty() && !r.game.as_ref().is_some_and(|g| g.client.gizmos.on(group)) {
            return;
        }
        let (a, b, text) = (DVec3::from_array(*a), DVec3::from_array(*b), crate::ui::text::Msg::new(key.as_str(), crate::client::modui::args(args)));
        match kind {
            abi::gizmo_kind::SCREEN_LABEL => r.mod_screen_text.push((a.x as f32, a.y as f32, text, color, if b.x > 0.0 { b.x as f32 } else { 1.0 })),
            k if k <= abi::gizmo_kind::FILL => r.mod_gizmos.push(crate::net::GizmoShape { group: group.to_string(), kind: k as u8, a, b, color, ticks: 0, text }),
            _ => {}
        }
    }
}

pub unsafe extern "C" fn client_entity_defaults(a: *mut abi::ClientEntityArgs) {
    unsafe {
        let Some(a) = a.as_ref() else { return };
        if let Some(g) = run(a.client).and_then(|r| r.game.as_mut()) {
            g.client.entity_effects(a);
        }
    }
}

pub unsafe extern "C" fn client_send_packet(c: *mut abi::ClientCtx, name: Str, schema: Str, data: Slice<u8>) {
    unsafe {
        let Some(r) = run(c) else { return };
        let Some(channel) = crate::host::channel_for(name.as_str(), schema.as_str()) else { return };
        if let Some(g) = r.game.as_mut() {
            g.client.send(crate::net::ToServer::Packet { channel, data: data.as_slice().to_vec() });
        }
    }
}

pub unsafe extern "C" fn client_particles(c: *mut abi::ClientCtx, burst: *const abi::ParticleBurst) {
    unsafe {
        let Some(r) = run(c) else { return };
        if let Some(g) = r.game.as_mut() {
            g.client.particles.spawn(&*burst);
        }
    }
}

pub unsafe extern "C" fn client_play_sound(c: *mut abi::ClientCtx, sound: u32, pos: *const [f64; 3], volume: f32, pitch: f32) {
    unsafe {
        let Some(r) = run(c) else { return };
        let pos = (!pos.is_null()).then(|| DVec3::from_array(*pos));
        r.ui_sounds.push(crate::audio::Play { sound, pos, volume, pitch });
    }
}
