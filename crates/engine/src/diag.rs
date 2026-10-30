//! Diagnostics that cost nothing until switched on through the log channels (`log::CHANNELS`):
//! counters, periodic reports, the allocation counter and crash reports.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use log::Level;
use loom_abi::ModId;

use crate::log::{Channel, on};

// ---------------------------------------------------------------- counters

pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Counter {
        Counter(AtomicU64::new(0))
    }
    #[inline]
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

pub static LIGHT_CUBES: Counter = Counter::new();
pub static LIGHT_STEPS: Counter = Counter::new();
pub static MESHED: Counter = Counter::new();
pub static MESH_NS: Counter = Counter::new();
/// Client terrain: cubes the GPU made, time finishing cubes on workers, cubes the terrain thread
/// put in and the time it was busy.
pub static TERRAIN_GPU: Counter = Counter::new();
pub static TERRAIN_FINISHED: Counter = Counter::new();
pub static TERRAIN_FINISH_NS: Counter = Counter::new();
pub static TERRAIN_JOINED: Counter = Counter::new();
pub static TERRAIN_BUSY_NS: Counter = Counter::new();

/// Messages and bytes per message kind: `[0]` to clients, `[1]` to the server.
static NET: [[(AtomicU64, AtomicU64); 64]; 2] = [const { [const { (AtomicU64::new(0), AtomicU64::new(0)) }; 64] }; 2];

/// Every message sent counts once where it is handed over (`count` 1); network transports add
/// the bytes it took when they serialize it (`count` 0).
#[inline]
pub fn net_message(to_server: bool, kind: usize, count: u64, bytes: usize) {
    if on(Channel::Net, Level::Debug) {
        let c = &NET[to_server as usize][kind.min(63)];
        c.0.fetch_add(count, Ordering::Relaxed);
        c.1.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- allocations

static COUNT_ALLOCS: AtomicBool = AtomicBool::new(false);
static ALLOCS: Counter = Counter::new();
static ALLOC_BYTES: Counter = Counter::new();

/// The executable's global allocator: mimalloc (the system one on Windows serialized the
/// workers allocating cubes and meshes by the thousand while loading), counting while the
/// `alloc` channel is on.
pub struct CountingAlloc;

static INNER: mimalloc::MiMalloc = mimalloc::MiMalloc;

unsafe impl GlobalAlloc for CountingAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_ALLOCS.load(Ordering::Relaxed) {
            ALLOCS.add(1);
            ALLOC_BYTES.add(layout.size() as u64);
        }
        unsafe { INNER.alloc(layout) }
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNT_ALLOCS.load(Ordering::Relaxed) {
            ALLOCS.add(1);
            ALLOC_BYTES.add(layout.size() as u64);
        }
        unsafe { INNER.alloc_zeroed(layout) }
    }
    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { INNER.dealloc(ptr, layout) }
    }
    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNT_ALLOCS.load(Ordering::Relaxed) {
            ALLOCS.add(1);
            ALLOC_BYTES.add(new_size as u64);
        }
        unsafe { INNER.realloc(ptr, layout, new_size) }
    }
}

/// Resident memory of the process in bytes, 0 where unknown.
#[cfg(windows)]
pub fn memory() -> u64 {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_non_paged: usize,
        quota_non_paged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn K32GetProcessMemoryInfo(process: *mut core::ffi::c_void, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut c = Counters { cb: size_of::<Counters>() as u32, ..Default::default() };
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0 { c.working_set as u64 } else { 0 }
}

#[cfg(not(windows))]
pub fn memory() -> u64 {
    0
}

/// Physical memory of the machine in bytes, 0 where unknown.
#[cfg(windows)]
pub fn total_memory() -> u64 {
    #[repr(C)]
    struct Status {
        length: u32,
        load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page: u64,
        avail_page: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended: u64,
    }
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(status: *mut Status) -> i32;
    }
    let mut s = Status { length: size_of::<Status>() as u32, load: 0, total_phys: 0, avail_phys: 0, total_page: 0, avail_page: 0, total_virtual: 0, avail_virtual: 0, avail_extended: 0 };
    if unsafe { GlobalMemoryStatusEx(&mut s) } != 0 { s.total_phys } else { 0 }
}

#[cfg(not(windows))]
pub fn total_memory() -> u64 {
    0
}

/// The processor's marketing name ("AMD Ryzen 9 5900X") from CPUID, without the trademark and
/// core count noise; the OS identifier string where CPUID has no name.
pub fn cpu_name() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
            let mut bytes = Vec::with_capacity(48);
            for leaf in 0x8000_0002u32..=0x8000_0004 {
                let r = __cpuid(leaf);
                for v in [r.eax, r.ebx, r.ecx, r.edx] {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
            }
            let raw = String::from_utf8_lossy(&bytes).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string();
            let name = tidy_cpu_name(&raw);
            if !name.is_empty() {
                return name;
            }
        }
    }
    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default()
}

fn tidy_cpu_name(raw: &str) -> String {
    let mut s = raw.replace("(R)", "").replace("(TM)", "").replace("(tm)", "");
    // Intel: "... CPU @ 3.60GHz"; AMD: "... 12-Core Processor".
    if let Some(i) = s.find(" @ ") {
        s.truncate(i);
    }
    let words: Vec<&str> = s.split_whitespace().filter(|w| !w.ends_with("-Core") && *w != "Processor" && *w != "CPU").collect();
    words.join(" ")
}

// ---------------------------------------------------------------- per-mod time

/// Mod callback time and calls between two looks, without resetting what other readers see.
#[derive(Default)]
pub struct ModWindow {
    last: Vec<(u64, u64)>,
    at: Option<Instant>,
}

impl ModWindow {
    /// (mod id, nanoseconds, calls) since the previous call, and the seconds that covers.
    pub fn take(&mut self) -> (Vec<(String, u64, u64)>, f64) {
        let Some(c) = crate::content::try_content() else { return (Vec::new(), 0.0) };
        let now: Vec<(u64, u64)> = (0..c.mods.len()).map(|i| crate::host::mod_totals(ModId(i as u32))).collect();
        let secs = self.at.map_or(0.0, |t| t.elapsed().as_secs_f64());
        if self.at.is_none() {
            // The first look only sets the baseline.
            self.last = now.clone();
        }
        let out = c
            .mods
            .iter()
            .zip(&now)
            .enumerate()
            .map(|(i, (m, n))| {
                let (ns, calls) = self.last.get(i).copied().unwrap_or_default();
                (m.id.clone(), n.0 - ns, n.1 - calls)
            })
            .collect();
        self.last = now;
        self.at = Some(Instant::now());
        (out, secs)
    }
}

// ---------------------------------------------------------------- periodic reports

/// What the app hands over each frame for the `frame` and `summary` reports.
pub struct FrameSample<'a> {
    pub frame_ms: f32,
    pub cpu: &'a [f32; 5],
    pub gpu: &'a [f32],
    pub client_cubes: usize,
    pub client_entities: usize,
    pub pending_meshes: usize,
    pub server: Option<&'a crate::server::Shared>,
}

#[derive(Default)]
pub struct Reporter {
    second: Option<Instant>,
    summary: Option<Instant>,
    frames: u32,
    frame_sum: f64,
    frame_max: f32,
    cpu: [f64; 5],
    gpu: [f64; 5],
    gpu_frames: u32,
    mods: ModWindow,
}

impl Reporter {
    pub fn frame(&mut self, s: &FrameSample) {
        COUNT_ALLOCS.store(on(Channel::Alloc, Level::Debug), Ordering::Relaxed);
        let now = Instant::now();
        if self.summary.is_none() {
            self.mods.take();
        }
        let second = *self.second.get_or_insert(now);
        let summary = *self.summary.get_or_insert(now);
        self.frames += 1;
        self.frame_sum += s.frame_ms as f64;
        self.frame_max = self.frame_max.max(s.frame_ms);
        for (a, v) in self.cpu.iter_mut().zip(s.cpu) {
            *a += *v as f64;
        }
        if s.gpu.len() >= 5 {
            for (a, v) in self.gpu.iter_mut().zip(s.gpu) {
                *a += *v as f64;
            }
            self.gpu_frames += 1;
        }
        let elapsed = (now - second).as_secs_f64();
        if elapsed >= 1.0 {
            self.second = Some(now);
            self.per_second(s, elapsed);
        }
        if (now - summary).as_secs_f64() >= 10.0 && on(Channel::Summary, Level::Debug) {
            self.summary = Some(now);
            self.summarize(s);
        }
    }

    fn per_second(&mut self, s: &FrameSample, secs: f64) {
        let n = self.frames.max(1) as f64;
        if on(Channel::Frame, Level::Debug) {
            let cpu: Vec<String> = crate::bench::CPU_PARTS.iter().zip(self.cpu).map(|(k, v)| format!("{k} {:.2}", v / n)).collect();
            let g = self.gpu_frames.max(1) as f64;
            let gpu: Vec<String> = crate::bench::GPU_PARTS.iter().zip(self.gpu).map(|(k, v)| format!("{k} {:.2}", v / g)).collect();
            let server = s.server.map_or(String::new(), |sh| {
                let phases: Vec<String> = crate::server::PHASES
                    .iter()
                    .zip(&sh.phase_us)
                    .map(|(k, v)| format!("{k} {:.2}", v.load(Ordering::Relaxed) as f64 / 1000.0))
                    .collect();
                format!(" | server {:.2} ms: {}", sh.tick_us.load(Ordering::Relaxed) as f64 / 1000.0, phases.join(", "))
            });
            log::debug!(target: "frame", "{:.0} fps, frame avg {:.2} max {:.2} ms | cpu {} | gpu {}{server}", self.frames as f64 / secs, self.frame_sum / n, self.frame_max, cpu.join(", "), gpu.join(", "));
        }
        if on(Channel::Net, Level::Debug) {
            for (dir, name) in [(0, "to clients"), (1, "to server")] {
                let mut parts = Vec::new();
                for (k, c) in NET[dir].iter().enumerate() {
                    let (count, bytes) = (c.0.swap(0, Ordering::Relaxed), c.1.swap(0, Ordering::Relaxed));
                    if count > 0 {
                        use crate::net::Message;
                        let kind = if dir == 0 { crate::net::ToClient::KINDS } else { crate::net::ToServer::KINDS }.get(k).copied().unwrap_or("?");
                        parts.push(if bytes > 0 { format!("{kind} {count} ({})", size(bytes)) } else { format!("{kind} {count}") });
                    }
                }
                if !parts.is_empty() {
                    log::debug!(target: "net", "{name} per {secs:.1} s: {}", parts.join(", "));
                }
            }
        }
        if on(Channel::Light, Level::Debug) {
            let (cubes, steps) = (LIGHT_CUBES.take(), LIGHT_STEPS.take());
            if cubes + steps > 0 {
                log::debug!(target: "light", "{cubes} cubes lit, {steps} flood steps in {secs:.1} s");
            }
        }
        if on(Channel::Load, Level::Debug) {
            let (gpu, fin, fin_ns, joined, busy) = (TERRAIN_GPU.take(), TERRAIN_FINISHED.take(), TERRAIN_FINISH_NS.take(), TERRAIN_JOINED.take(), TERRAIN_BUSY_NS.take());
            if gpu + fin + joined > 0 {
                log::debug!(
                    target: "load",
                    "terrain per {secs:.1} s: {gpu} from the GPU, {fin} finished ({:.0} us each on a worker), {joined} put in, terrain thread busy {:.0}%",
                    fin_ns as f64 / fin.max(1) as f64 / 1000.0,
                    busy as f64 / secs / 1e7
                );
            }
        }
        if on(Channel::Mesh, Level::Debug) {
            let (meshed, ns) = (MESHED.take(), MESH_NS.take());
            if meshed > 0 || s.pending_meshes > 0 {
                log::debug!(target: "mesh", "{meshed} meshed in {secs:.1} s, {:.0} us each on a worker, {} waiting", ns as f64 / meshed.max(1) as f64 / 1000.0, s.pending_meshes);
            }
        }
        if on(Channel::Alloc, Level::Debug) {
            let (n, bytes) = (ALLOCS.take(), ALLOC_BYTES.take());
            log::debug!(target: "alloc", "{:.0} allocations/s, {}/s, {} resident", n as f64 / secs, size((bytes as f64 / secs) as u64), size(memory()));
        }
        self.frames = 0;
        self.frame_sum = 0.0;
        self.frame_max = 0.0;
        self.cpu = [0.0; 5];
        self.gpu = [0.0; 5];
        self.gpu_frames = 0;
    }

    fn summarize(&mut self, s: &FrameSample) {
        let (mods, secs) = self.mods.take();
        let ticks = (secs * crate::world::TPS as f64).max(1.0);
        let mut mods: Vec<(String, u64, u64)> = mods.into_iter().filter(|m| m.2 > 0).collect();
        mods.sort_by(|a, b| b.1.cmp(&a.1));
        let mods: Vec<String> = mods.iter().take(5).map(|(id, ns, calls)| format!("{id} {:.3} ms/{:.0} calls", *ns as f64 / 1e6 / ticks, *calls as f64 / ticks)).collect();
        let server = s.server.map_or(String::new(), |sh| {
            format!(
                "tick {:.2} ms (max {:.2}), {} regions, {} cubes, {} entities | ",
                sh.tick_us.load(Ordering::Relaxed) as f64 / 1000.0,
                sh.tick_max_us.swap(0, Ordering::Relaxed) as f64 / 1000.0,
                sh.regions.load(Ordering::Relaxed),
                sh.cubes.load(Ordering::Relaxed),
                sh.entities.load(Ordering::Relaxed),
            )
        });
        log::debug!(
            target: "summary",
            "{server}client {} cubes, {} entities | {} resident | mods per tick: {}",
            s.client_cubes,
            s.client_entities,
            size(memory()),
            if mods.is_empty() { "-".to_string() } else { mods.join(", ") }
        );
    }
}

#[cfg(test)]
mod cpu_tests {
    #[test]
    fn cpu_names_lose_the_noise() {
        assert_eq!(super::tidy_cpu_name("AMD Ryzen 9 5900X 12-Core Processor"), "AMD Ryzen 9 5900X");
        assert_eq!(super::tidy_cpu_name("Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz"), "Intel Core i7-8700K");
    }
}

pub fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.2} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

// ---------------------------------------------------------------- crash reports

thread_local! {
    static CURRENT_MOD: Cell<u32> = const { Cell::new(u32::MAX) };
}

/// Marks this thread as running `m`'s code; returns what to restore afterwards.
#[inline]
pub fn enter_mod(m: ModId) -> u32 {
    CURRENT_MOD.with(|c| c.replace(m.0))
}

#[inline]
pub fn leave_mod(prev: u32) {
    CURRENT_MOD.with(|c| c.set(prev));
}

fn current_mod() -> Option<String> {
    let m = CURRENT_MOD.try_with(|c| c.get()).unwrap_or(u32::MAX);
    (m != u32::MAX).then(|| crate::modloader::mod_id_name(ModId(m)).to_string())
}

static CRASH_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Panics and (on Windows) native faults write `crash-reports/crash-<time>.txt` naming the mod
/// that was running, with a backtrace and the last log lines.
pub fn install_crash_handler(run_dir: &std::path::Path) {
    let _ = CRASH_DIR.set(run_dir.join("crash-reports"));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into());
        let at = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        write_report(&format!("panic{at}: {msg}"));
        previous(info);
    }));
    #[cfg(windows)]
    native::install();
}

fn write_report(what: &str) {
    let Some(dir) = CRASH_DIR.get() else { return };
    let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
    let mut r = String::new();
    r.push_str(&format!("Loom {} crash report, {}\n\n", env!("CARGO_PKG_VERSION"), crate::log::now_stamp()));
    r.push_str(&format!("What:   {what}\nThread: {thread}\n"));
    match current_mod() {
        Some(m) => r.push_str(&format!("Mod:    {m} (its code was running when this happened)\n")),
        None => r.push_str("Mod:    none (engine code)\n"),
    }
    if let Some(c) = crate::content::try_content() {
        let mods: Vec<String> = c.mods.iter().map(|m| format!("{} {}", m.id, m.version)).collect();
        r.push_str(&format!("Mods:   {}\n", mods.join(", ")));
    }
    r.push_str(&format!("System: {} {}, {} threads\n", std::env::consts::OS, std::env::consts::ARCH, std::thread::available_parallelism().map_or(0, |n| n.get())));
    r.push_str(&format!("\nBacktrace:\n{}\n", std::backtrace::Backtrace::force_capture()));
    r.push_str("\nLast log lines:\n");
    for line in crate::log::recent() {
        r.push_str(&line);
        r.push('\n');
    }
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("crash-{}.txt", crate::log::now_stamp()));
    if std::fs::write(&path, r).is_ok() {
        log::error!("{what}; crash report saved to {}", path.display());
    }
    crate::log::flush();
}

#[cfg(windows)]
mod native {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[repr(C)]
    struct ExceptionPointers {
        record: *const u32,
        context: *const core::ffi::c_void,
    }

    unsafe extern "system" {
        fn SetUnhandledExceptionFilter(f: Option<unsafe extern "system" fn(*const ExceptionPointers) -> i32>) -> *const core::ffi::c_void;
    }

    pub fn install() {
        unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    }

    unsafe extern "system" fn filter(info: *const ExceptionPointers) -> i32 {
        static ONCE: AtomicBool = AtomicBool::new(false);
        if !ONCE.swap(true, Ordering::Relaxed) {
            let code = unsafe { info.as_ref().and_then(|i| i.record.as_ref()).copied().unwrap_or(0) };
            let name = match code {
                0xC000_0005 => "access violation",
                0xC000_001D => "illegal instruction",
                0xC000_0094 => "integer division by zero",
                0xC000_00FD => "stack overflow",
                _ => "native exception",
            };
            super::write_report(&format!("{name} (code {code:#010x})"));
        }
        // Let Windows end the process as it would have.
        0
    }
}
