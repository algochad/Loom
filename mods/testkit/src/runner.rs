//! Runs tests as futures polled once per frame from the client frame hook. A test awaits
//! readiness (world loaded, screen laid out, a server answer), never fixed sleeps unless it says
//! so; every wait has a timeout. Results go to `run/test-reports/report.txt` and the exit code.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use loom_sdk::client::{self, ClientState, client_flags as cf};
use loom_sdk::*;

pub type R = Result<(), String>;
pub type BoxTest = Pin<Box<dyn Future<Output = R>>>;

pub struct TestDef {
    pub name: String,
    pub run: Box<dyn Fn(T) -> BoxTest>,
}

#[macro_export]
macro_rules! tests {
    ($($f:path),* $(,)?) => {
        vec![$($crate::runner::TestDef { name: { let n = stringify!($f); n.rsplit("::").next().unwrap_or(n).trim().to_string() }, run: Box::new(|t| Box::pin($f(t))) }),*]
    };
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Inner {
    client: Option<Client>,
    pub state: ClientState,
    pub screen: String,
    pub name: String,
    failures: Vec<String>,
    notes: Vec<String>,
}

/// Handle a test uses for everything; cheap to clone, only touched on the main thread. A
/// scenario of a shared scene gets a copy labeled with its name (`scoped`).
#[derive(Clone)]
pub struct T(Rc<RefCell<Inner>>, Option<Rc<str>>);

impl T {
    fn new(name: &str) -> T {
        T(
            Rc::new(RefCell::new(Inner {
            client: None,
            state: ClientState::DEFAULT,
            screen: String::new(),
            name: name.to_string(),
            failures: Vec::new(),
            notes: Vec::new(),
            })),
            None,
        )
    }

    /// The same test, its failures and notes labeled with `label`.
    pub fn scoped(&self, label: &str) -> T {
        T(self.0.clone(), Some(Rc::from(label)))
    }

    fn labeled(&self, m: String) -> String {
        match &self.1 {
            Some(l) => format!("{l}: {m}"),
            None => m,
        }
    }

    /// Whether this scenario (or the whole test, unlabeled) has failed a check.
    pub fn failed(&self) -> bool {
        let i = self.0.borrow();
        match &self.1 {
            Some(l) => i.failures.iter().any(|f| f.starts_with(&format!("{l}: "))),
            None => !i.failures.is_empty(),
        }
    }

    /// The client of the frame being polled.
    pub fn c(&self) -> Client {
        self.0.borrow().client.expect("testkit: client used outside a frame")
    }

    pub fn state(&self) -> ClientState {
        self.0.borrow().state
    }

    pub fn flag(&self, f: u32) -> bool {
        self.state().flags & f != 0
    }

    pub fn name(&self) -> String {
        self.0.borrow().name.clone()
    }

    /// A failed check that does not stop the test.
    pub fn fail(&self, msg: impl Into<String>) {
        let m = self.labeled(msg.into());
        loom_sdk::warn!("[{}] FAIL {m}", self.name());
        self.0.borrow_mut().failures.push(m);
    }

    pub fn note(&self, msg: impl Into<String>) {
        let m = self.labeled(msg.into());
        loom_sdk::info!("[{}] {m}", self.name());
        self.0.borrow_mut().notes.push(m);
    }

    pub fn check(&self, ok: bool, msg: impl Into<String>) -> bool {
        if !ok {
            self.fail(msg);
        }
        ok
    }

    pub fn check_eq<V: PartialEq + std::fmt::Debug>(&self, got: V, want: V, what: &str) -> bool {
        self.check(got == want, format!("{what}: got {got:?}, want {want:?}"))
    }

    /// A measured value always goes to the report (to tune limits); fails when not `ok`.
    pub fn probe(&self, what: &str, value: f32, ok: bool) -> bool {
        if ok {
            self.note(format!("{what}: {value:.4}"));
        }
        self.check(ok, format!("{what}: {value:.4}"))
    }

    /// Where world point `p` is drawn this frame (fractions of the window), seen from the eyes
    /// of the local player in first person; `None` behind the camera.
    pub fn on_screen(&self, p: [f64; 3]) -> Option<(f32, f32)> {
        let s = self.state();
        let eye_y = self.c().entity(0).map_or(1.62, |e| e.size[2] as f64);
        let eye = [s.shown[0], s.shown[1] + eye_y, s.shown[2]];
        let (sy, cy) = (s.yaw as f64).sin_cos();
        let (sp, cp) = (s.pitch as f64).sin_cos();
        let fwd = [-sy * cp, sp, -cy * cp];
        let right = [cy, 0.0, -sy];
        let up = [right[1] * fwd[2] - right[2] * fwd[1], right[2] * fwd[0] - right[0] * fwd[2], right[0] * fwd[1] - right[1] * fwd[0]];
        let d = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
        let dot = |a: [f64; 3]| a[0] * d[0] + a[1] * d[1] + a[2] * d[2];
        let z = dot(fwd);
        if z <= 0.05 {
            return None;
        }
        let tan = (70f64.to_radians() / 2.0).tan();
        let aspect = s.window[0] as f64 / s.window[1].max(1) as f64;
        let (x, y) = (dot(right) / (z * tan * aspect), dot(up) / (z * tan));
        Some(((0.5 + 0.5 * x) as f32, (0.5 - 0.5 * y) as f32))
    }

    /// Where the middle of block `b` (offset by `d` blocks) is drawn.
    pub fn block_on_screen(&self, b: [i32; 3], d: [f64; 3]) -> Option<(f32, f32)> {
        self.on_screen([b[0] as f64 + 0.5 + d[0], b[1] as f64 + 0.5 + d[1], b[2] as f64 + 0.5 + d[2]])
    }

    // ------------------------------------------------ waiting

    /// Waits until `f` holds, failing after `timeout`.
    pub fn until<F: FnMut(&T) -> bool + Unpin>(&self, what: &str, timeout: Duration, f: F) -> Wait<F> {
        Wait { t: self.clone(), f, deadline: Instant::now() + timeout, what: what.to_string() }
    }

    pub fn frames(&self, n: u64) -> Wait<impl FnMut(&T) -> bool + Unpin> {
        let end = self.state().frame + n;
        self.until("frames", Duration::from_secs(600), move |t| t.state().frame >= end)
    }

    /// A plain delay, for things that take game time (smelting, falling).
    pub fn secs(&self, s: f32) -> Wait<impl FnMut(&T) -> bool + Unpin> {
        let end = Instant::now() + Duration::from_secs_f32(s);
        self.until("time", Duration::from_secs_f32(s + 600.0), move |_| Instant::now() >= end)
    }

    /// The world around the player is loaded and meshed.
    pub fn loaded(&self) -> Wait<impl FnMut(&T) -> bool + Unpin> {
        self.until("world loaded", Duration::from_secs(60), |t| t.flag(cf::LOADED))
    }

    /// Nothing left to generate, send or mesh.
    pub fn idle(&self) -> Wait<impl FnMut(&T) -> bool + Unpin> {
        let mut since: Option<Instant> = None;
        self.until("idle", Duration::from_secs(120), move |t| {
            // Cubes sent may still be in flight when the server reports idle.
            if !t.flag(cf::IDLE) {
                since = None;
                return false;
            }
            since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_millis(300)
        })
    }

    /// `name` is the top screen and it is laid out and drawn.
    pub fn screen(&self, name: &str) -> Wait<impl FnMut(&T) -> bool + Unpin> {
        let n = name.to_string();
        self.until(&format!("screen {name}"), DEFAULT_TIMEOUT, move |t| t.0.borrow().screen == n && t.flag(cf::SCREEN_READY))
    }

    /// Standing still on the ground for a few frames (camera at rest for screenshots).
    pub fn settled(&self) -> Wait<impl FnMut(&T) -> bool + Unpin> {
        let mut still = 0;
        let mut last = [f64::NAN; 3];
        self.until("standing still", DEFAULT_TIMEOUT, move |t| {
            let s = t.state();
            let moved = (0..3).any(|i| (s.pos[i] - last[i]).abs() > 1e-6 || s.pos[i].is_nan());
            last = s.pos;
            still = if moved || !t.flag(cf::ON_GROUND) { 0 } else { still + 1 };
            still >= 20
        })
    }

    /// Back in the world with no screen open.
    pub fn no_screen(&self) -> Wait<impl FnMut(&T) -> bool + Unpin> {
        self.until("no screen", DEFAULT_TIMEOUT, |t| t.0.borrow().screen.is_empty())
    }

    // ------------------------------------------------ worlds

    /// A fresh world `test-<name>` with `seed`, waiting until it is loaded.
    pub async fn world(&self, seed: u64) -> R {
        let name = format!("test-{}", self.name());
        self.c().new_world(&name, seed);
        self.until("joined", Duration::from_secs(60), |t| t.flag(cf::JOINED)).await?;
        // Mobs only where a test puts them, so pictures repeat.
        self.cmd("/rule core:mob_spawning 0").await?;
        self.loaded().await
    }

    /// Runs a chat command and returns the reply line(s) the server printed.
    pub async fn cmd(&self, line: &str) -> Result<String, String> {
        let before = self.c().chat_line(0);
        let marker = self.0.borrow().state.frame;
        self.c().chat(line);
        let mut seen = String::new();
        self.until(&format!("reply to {line}"), Duration::from_secs(10), |t| {
            let now = t.c().chat_line(0);
            if now != before || t.state().frame > marker + 600 {
                seen = now;
                true
            } else {
                false
            }
        })
        .await?;
        Ok(seen)
    }

    /// Runs an operation of the testkit's server half (`/tk <op>`, see `server.rs`) and waits
    /// for its answer.
    pub async fn op(&self, text: &str) -> Result<String, String> {
        let id = self.c().run_command(&format!("/tk {text}"));
        let mut answer = None;
        self.until(&format!("server answer to '{text}'"), Duration::from_secs(10), |t| {
            answer = t.c().command_reply(id);
            answer.is_some()
        })
        .await?;
        let a = answer.unwrap_or_default();
        match a.strip_prefix("ok") {
            Some(rest) => Ok(rest.trim().to_string()),
            None => Err(format!("'{text}': {}", a.strip_prefix("err ").unwrap_or(&a))),
        }
    }

    /// The block's name as the server has it (`base:furnace[facing=north,lit=true]`).
    pub async fn block(&self, pos: [i32; 3]) -> Result<String, String> {
        self.op(&format!("get {} {} {}", pos[0], pos[1], pos[2])).await
    }

    pub async fn set(&self, pos: [i32; 3], block: &str) -> R {
        self.op(&format!("set {} {} {} {block}", pos[0], pos[1], pos[2])).await.map(|_| ())
    }

    pub async fn fill(&self, a: [i32; 3], b: [i32; 3], block: &str) -> R {
        self.op(&format!("fill {} {} {} {} {} {} {block}", a[0], a[1], a[2], b[0], b[1], b[2])).await.map(|_| ())
    }

    /// (item name, count, damage) in an inventory slot of `owner` (`me`, `x y z` or `#id`).
    pub async fn slot(&self, owner: &str, slot: u32) -> Result<(String, u32, u32), String> {
        let s = self.op(&format!("slot {owner} {slot}")).await?;
        let mut p = s.split_whitespace();
        let name = p.next().unwrap_or("-").to_string();
        let count = p.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let dmg = p.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        Ok((name, count, dmg))
    }

    /// Polls a slot until something is in it (machines), giving what was there at the end.
    pub async fn wait_slot(&self, owner: &str, slot: u32, secs: f32) -> Result<(String, u32, u32), String> {
        let deadline = Instant::now() + Duration::from_secs_f32(secs);
        loop {
            let s = self.slot(owner, slot).await?;
            if s.1 > 0 || Instant::now() > deadline {
                return Ok(s);
            }
            self.secs(0.25).await?;
        }
    }

    /// Player's block position as the client has it.
    pub fn feet(&self) -> [i32; 3] {
        let p = self.state().pos;
        [p[0].floor() as i32, p[1].floor() as i32, p[2].floor() as i32]
    }

    // ------------------------------------------------ input

    pub fn press(&self, action: &str) {
        if !self.c().press(action, 0.0) {
            self.fail(format!("no action {action}"));
        }
    }

    /// Shows or hides F3 and waits until it is so.
    pub async fn debug(&self, on: bool) -> R {
        if self.flag(cf::DEBUG) != on {
            self.press("core:debug");
        }
        self.until("F3 toggled", Duration::from_secs(2), |t| t.flag(cf::DEBUG) == on).await
    }

    pub fn hold(&self, action: &str, secs: f32) {
        if !self.c().press(action, secs) {
            self.fail(format!("no action {action}"));
        }
    }

    /// Every element is on the top screen (or the HUD) and fully inside the window.
    pub fn laid_out(&self, queries: &[&str]) {
        for q in queries {
            match self.c().ui_rect(q) {
                None => self.fail(format!("no UI element {q} on {}", self.0.borrow().screen)),
                Some([x, y, w, h]) => {
                    self.check(x >= 0.0 && y >= 0.0 && x + w <= 1.001 && y + h <= 1.001, format!("{q} sticks out of the window ({x:.3} {y:.3} {w:.3} {h:.3})"));
                }
            }
        }
    }

    /// Clicks a UI element (`slot:N`, `id:x`, `text:words`) of the top screen.
    pub fn click_ui(&self, query: &str, button: u32) -> bool {
        let ok = self.c().click_ui(query, button);
        if !ok {
            self.fail(format!("no UI element {query} on {}", self.0.borrow().screen));
        }
        ok
    }

    // ------------------------------------------------ evidence

    /// Saves the whole frame as `run/test-shots/<test>/<name>.png` (for looking at after a
    /// failure) and returns it; tests decide with probes on it, never by comparing pictures.
    pub async fn shot(&self, name: &str) -> Result<Shot, String> {
        self.capture(name).await
    }

    async fn capture(&self, name: &str) -> Result<Shot, String> {
        let path = format!("{}/test-shots/{}/{name}.png", self.c().run_dir(), self.name());
        let id = self.c().screenshot(&path);
        let mut shot = None;
        self.until(&format!("screenshot {name}"), Duration::from_secs(10), |_| {
            shot = client::image(id).map(|(w, h, p)| Shot { w, h, px: p.to_vec() });
            shot.is_some()
        })
        .await?;
        client::image_free(id);
        Ok(shot.unwrap())
    }

    /// This frame's pixels (RGBA), for comparing frames within a test.
    pub async fn grab(&self) -> Result<Vec<u8>, String> {
        Ok(self.capture("grab").await?.px)
    }

    /// This frame's color at (x, y), fractions of the window size.
    pub async fn pixel(&self, x: f32, y: f32) -> Result<[u8; 3], String> {
        Ok(self.capture("pixel").await?.at(x, y))
    }

    /// Share of pixels that differ between two grabs.
    pub fn differ(a: &[u8], b: &[u8]) -> f64 {
        compare(a, b) as f64 / (a.len() / 4).max(1) as f64
    }

    /// Frame/GPU/server numbers over `secs`: [fps, avg ms, p99 ms, gpu ms, server ms].
    pub async fn bench(&self, secs: f32, label: &str) -> Result<[f32; 5], String> {
        let before = self.state().benches;
        self.c().bench(secs, label);
        self.until(&format!("bench {label}"), Duration::from_secs_f32(secs + 30.0), |t| t.state().benches > before).await?;
        let s = self.state();
        let v = [s.bench_fps, s.bench_avg_ms, s.bench_p99_ms, s.bench_gpu_ms, s.bench_server_ms];
        self.note(format!("bench {label}: {:.0} fps, avg {:.2} ms, p99 {:.2} ms, gpu {:.2} ms, server {:.2} ms", v[0], v[1], v[2], v[3], v[4]));
        Ok(v)
    }

    /// Compares what the server and the client have: blocks in the box between `a` and `b`.
    pub async fn sync_blocks(&self, a: [i32; 3], b: [i32; 3]) -> R {
        let server = self.op(&format!("snapshot {} {} {} {} {} {}", a[0], a[1], a[2], b[0], b[1], b[2])).await?;
        let server: Vec<u32> = server.split_whitespace().filter_map(|s| s.parse().ok()).collect();
        let c = self.c();
        let mut i = 0;
        let mut diffs = Vec::new();
        for y in a[1].min(b[1])..=a[1].max(b[1]) {
            for z in a[2].min(b[2])..=a[2].max(b[2]) {
                for x in a[0].min(b[0])..=a[0].max(b[0]) {
                    let s = server.get(i).copied().unwrap_or(u32::MAX);
                    i += 1;
                    match c.block([x, y, z]) {
                        Some(cl) if cl == s => {}
                        Some(cl) => diffs.push(format!("{x} {y} {z}: server {} client {}", block_name(s), block_name(cl))),
                        None => diffs.push(format!("{x} {y} {z}: server {}, client has not loaded it", block_name(s))),
                    }
                }
            }
        }
        if !diffs.is_empty() {
            let n = diffs.len();
            diffs.truncate(8);
            self.fail(format!("{n} blocks differ between server and client: {}", diffs.join("; ")));
        }
        Ok(())
    }

    /// Own inventory on the server against the client's copy.
    pub async fn sync_inventory(&self) -> R {
        let server = self.op("inv me").await?;
        let c = self.c();
        let mut diffs = Vec::new();
        let mut on_server: HashMap<u32, (String, u32)> = HashMap::new();
        for part in server.split_whitespace() {
            let mut f = part.splitn(3, ':');
            let (Some(slot), Some(ns), Some(rest)) = (f.next(), f.next(), f.next()) else { continue };
            let (name, count) = rest.rsplit_once(':').unwrap_or((rest, "0"));
            on_server.insert(slot.parse().unwrap_or(0), (format!("{ns}:{name}"), count.parse().unwrap_or(0)));
        }
        for slot in 0..40 {
            let s = c.inventory_slot(slot);
            let mine = (!s.is_empty()).then(|| (item_name(s.item).to_string(), s.count));
            let theirs = on_server.get(&slot).cloned();
            if mine != theirs {
                diffs.push(format!("slot {slot}: server {theirs:?} client {mine:?}"));
            }
        }
        if !diffs.is_empty() {
            self.fail(format!("inventory differs: {}", diffs.join("; ")));
        }
        Ok(())
    }
}

/// A screenshot kept for looking at; checks probe it. Coordinates are fractions of the frame.
pub struct Shot {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

impl Shot {
    pub fn at(&self, x: f32, y: f32) -> [u8; 3] {
        let i = self.index(x, y);
        [self.px[i], self.px[i + 1], self.px[i + 2]]
    }

    fn index(&self, x: f32, y: f32) -> usize {
        let (px, py) = (((x * self.w as f32) as u32).min(self.w - 1), ((y * self.h as f32) as u32).min(self.h - 1));
        (py * self.w + px) as usize * 4
    }

    fn each(&self, x0: f32, y0: f32, x1: f32, y1: f32, mut f: impl FnMut([u8; 3])) {
        let (ax, ay) = (((x0 * self.w as f32) as u32).min(self.w - 1), ((y0 * self.h as f32) as u32).min(self.h - 1));
        let (bx, by) = (((x1 * self.w as f32) as u32).clamp(ax + 1, self.w), ((y1 * self.h as f32) as u32).clamp(ay + 1, self.h));
        for y in ay..by {
            for x in ax..bx {
                let i = ((y * self.w + x) * 4) as usize;
                f([self.px[i], self.px[i + 1], self.px[i + 2]]);
            }
        }
    }

    /// Average color of the box from (x0, y0) to (x1, y1).
    pub fn avg(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> [f32; 3] {
        let (mut sum, mut n) = ([0.0f32; 3], 0.0f32);
        self.each(x0, y0, x1, y1, |c| {
            for k in 0..3 {
                sum[k] += c[k] as f32;
            }
            n += 1.0;
        });
        sum.map(|v| v / n.max(1.0))
    }

    /// Share of the box's pixels for which `f` holds.
    pub fn share(&self, x0: f32, y0: f32, x1: f32, y1: f32, mut f: impl FnMut([u8; 3]) -> bool) -> f32 {
        let (mut hit, mut n) = (0.0f32, 0.0f32);
        self.each(x0, y0, x1, y1, |c| {
            hit += f(c) as u8 as f32;
            n += 1.0;
        });
        hit / n.max(1.0)
    }
}

/// Perceived brightness 0..255.
pub fn lum(c: [f32; 3]) -> f32 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

/// Pixels whose color differs noticeably.
fn compare(a: &[u8], b: &[u8]) -> u32 {
    let n = a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(p, q)| (0..3).any(|i| (p[i] as i32 - q[i] as i32).abs() > 40)).count();
    n as u32
}

/// Runs the futures side by side (each polled every frame); the first error of each is reported
/// by `timed`, so this only waits for all of them.
pub struct All(Vec<Option<BoxTest>>);

pub fn all(parts: Vec<BoxTest>) -> All {
    All(parts.into_iter().map(Some).collect())
}

impl Future for All {
    type Output = R;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        for slot in self.0.iter_mut() {
            if let Some(f) = slot {
                if f.as_mut().poll(cx).is_ready() {
                    *slot = None;
                }
            }
        }
        if self.0.iter().all(Option::is_none) { Poll::Ready(Ok(())) } else { Poll::Pending }
    }
}

/// One scenario of a scene: labeled failures, its error reported as a failure, its time noted.
pub fn scenario<F: Future<Output = R> + 'static>(t: &T, name: &str, f: impl FnOnce(T) -> F) -> BoxTest {
    let t = t.scoped(name);
    let fut = f(t.clone());
    Box::pin(async move {
        let start = Instant::now();
        if let Err(e) = fut.await {
            t.fail(e);
        }
        t.note(format!("{} in {:.1} s", if t.failed() { "FAILED" } else { "ok" }, start.elapsed().as_secs_f32()));
        Ok(())
    })
}

pub struct Wait<F> {
    t: T,
    f: F,
    deadline: Instant,
    what: String,
}

impl<F: FnMut(&T) -> bool + Unpin> Future for Wait<F> {
    type Output = R;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<R> {
        let me = &mut *self;
        if (me.f)(&me.t) {
            Poll::Ready(Ok(()))
        } else if Instant::now() > me.deadline {
            Poll::Ready(Err(format!("timed out waiting for {}", me.what)))
        } else {
            Poll::Pending
        }
    }
}

// ---------------------------------------------------------------- the run

struct Outcome {
    name: String,
    ok: bool,
    secs: f32,
    lines: Vec<String>,
}

pub struct Runner {
    queue: Vec<TestDef>,
    current: Option<(T, BoxTest, Instant)>,
    done: Vec<Outcome>,
    started: Instant,
    finished: bool,
    /// Frames to wait before starting, so the window and the title screen are up.
    warmup: u32,
}

pub static PROBE: Key = Key::named("probe");

thread_local! {
    /// Frames the test kit's own key binding was seen pressed.
    pub static PROBES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static RUNNER: RefCell<Option<Runner>> = const { RefCell::new(None) };
    static ACTIVE: RefCell<Option<T>> = const { RefCell::new(None) };
}

pub fn start(queue: Vec<TestDef>) {
    loom_sdk::info!("testkit: running {} test(s): {}", queue.len(), queue.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));
    RUNNER.with(|r| *r.borrow_mut() = Some(Runner { queue, current: None, done: Vec::new(), started: Instant::now(), finished: false, warmup: 3 }));
}

pub fn frame(c: &Client) {
    RUNNER.with(|r| {
        let mut guard = r.borrow_mut();
        let Some(run) = guard.as_mut() else { return };
        if run.finished {
            return;
        }
        if run.warmup > 0 {
            // Each run's screenshots only.
            if run.warmup == 3 {
                let _ = std::fs::remove_dir_all(format!("{}/test-shots", c.run_dir()));
            }
            run.warmup -= 1;
            return;
        }
        if run.current.is_none() {
            if run.queue.is_empty() {
                run.finished = true;
                finish(c, run);
                return;
            }
            let def = run.queue.remove(0);
            loom_sdk::info!("testkit: --- {} ---", def.name);
            let t = T::new(&def.name);
            ACTIVE.with(|a| *a.borrow_mut() = Some(t.clone()));
            let fut = (def.run)(t.clone());
            run.current = Some((t, fut, Instant::now()));
        }
        let (t, fut, began) = run.current.as_mut().unwrap();
        // Held, not pressed: a key the test taps itself is pressed only within its own frame.
        if c.held(&PROBE) {
            PROBES.with(|p| p.set(p.get() + 1));
        }
        {
            let st = c.state();
            let mut i = t.0.borrow_mut();
            i.client = Some(*c);
            i.screen = unsafe { st.screen.as_str().to_string() };
            i.state = st;
        }
        let mut cx = Context::from_waker(Waker::noop());
        let result = fut.as_mut().poll(&mut cx);
        t.0.borrow_mut().client = None;
        if let Poll::Ready(res) = result {
            let i = t.0.borrow();
            let mut lines: Vec<String> = i.notes.clone();
            let mut ok = i.failures.is_empty();
            lines.extend(i.failures.iter().map(|f| format!("FAIL {f}")));
            if let Err(e) = res {
                ok = false;
                lines.push(format!("ERROR {e}"));
            }
            let out = Outcome { name: i.name.clone(), ok, secs: began.elapsed().as_secs_f32(), lines };
            loom_sdk::info!("testkit: {} {} in {:.1} s", if out.ok { "PASS" } else { "FAIL" }, out.name, out.secs);
            drop(i);
            run.done.push(out);
            run.current = None;
            c.release_all();
            if c.state().flags & cf::DEBUG != 0 {
                c.key("F3", 0.0);
            }
            if c.state().flags & cf::IN_WORLD != 0 {
                c.leave_world();
            }
        }
    });
}

fn finish(c: &Client, run: &mut Runner) {
    let failed = run.done.iter().filter(|o| !o.ok).count();
    let mut report = format!(
        "testkit report: {} passed, {failed} failed, {:.1} s\n",
        run.done.len() - failed,
        run.started.elapsed().as_secs_f32()
    );
    for o in &run.done {
        report.push_str(&format!("\n{} {} ({:.1} s)\n", if o.ok { "PASS" } else { "FAIL" }, o.name, o.secs));
        for l in &o.lines {
            report.push_str(&format!("    {l}\n"));
        }
    }
    let run_dir = c.run_dir();
    let dir = format!("{run_dir}/test-reports");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(format!("{dir}/report.txt"), &report);
    for line in report.lines() {
        loom_sdk::info!("{line}");
    }
    // Test worlds are throwaway.
    if launch_option("test-keep").is_none() {
        if let Ok(entries) = std::fs::read_dir(format!("{run_dir}/saves")) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("test-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
    }
    c.quit(failed.min(100) as i32);
}
