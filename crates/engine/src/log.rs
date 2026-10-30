//! One log sink for the engine and mods: colored on the terminal, mirrored into `logs/latest.log`
//! (the previous logs are kept with their date), filtered per target.
//!
//! Filters are `target=level` items separated by commas, given with `--log` or `/log`:
//! `server=debug,mod:base=trace,load=debug`. A bare level (or `all=level`) sets the default for
//! the engine and mods; third-party crates stay at warnings unless named. A target matches itself
//! and everything below it (`server` covers `server::dim`). Besides module paths there are
//! diagnostic channels (`CHANNELS`) that switch extra reports on.

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

use log::{Level, LevelFilter, Metadata, Record};
use parking_lot::{Mutex, RwLock};

/// Diagnostic channels: name, what turning it on reports (debug level; trace adds detail).
pub const CHANNELS: &[(&str, &str)] = &[
    ("load", "load pipeline counters every second; trace: why cubes wait"),
    ("physics", "trace: local player position, velocity and contacts every tick"),
    ("net", "messages and bytes per type every second"),
    ("light", "lighting work every second"),
    ("mesh", "meshing queue and cost every second"),
    ("alloc", "heap allocations every second"),
    ("frame", "frame, GPU pass and server phase times every second"),
    ("summary", "tick time, world size, memory and costliest mods every 10 seconds"),
    ("modifiers", "how value chains computed damage and other numbers"),
];

#[derive(Clone, Copy)]
pub enum Channel {
    Load,
    Physics,
    Net,
    Light,
    Mesh,
    Alloc,
    Frame,
    Summary,
    Modifiers,
}

static CHANNEL_LEVEL: [AtomicU8; 9] = [const { AtomicU8::new(0) }; 9];

/// Hot paths ask this instead of the logger: one atomic load.
#[inline]
pub fn on(ch: Channel, level: Level) -> bool {
    CHANNEL_LEVEL[ch as usize].load(Ordering::Relaxed) >= level as u8
}

const KEEP_LOGS: usize = 5;
const RECENT: usize = 300;

#[derive(Clone)]
struct Filter {
    default: LevelFilter,
    /// Longest target first, so the most specific rule wins.
    rules: Vec<(String, LevelFilter)>,
}

impl Filter {
    fn level(&self, target: &str, ours: bool) -> LevelFilter {
        for (t, l) in &self.rules {
            if target == t || target.strip_prefix(t.as_str()).is_some_and(|rest| rest.starts_with("::")) {
                return *l;
            }
        }
        if ours { self.default } else { LevelFilter::Warn }
    }

    fn max(&self) -> LevelFilter {
        self.rules.iter().map(|r| r.1).fold(self.default.max(LevelFilter::Warn), |a, b| a.max(b))
    }

    /// Applies `spec` on top of the current rules.
    fn apply(&mut self, spec: &str) -> Result<(), String> {
        for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (target, level) = match item.split_once('=') {
                Some((t, l)) => (t.trim(), l.trim()),
                None => ("all", item),
            };
            let level: LevelFilter = level.parse().map_err(|_| format!("unknown level '{level}' (off, error, warn, info, debug, trace)"))?;
            if target == "all" || target == "*" {
                self.default = level;
            } else {
                self.rules.retain(|r| r.0 != target);
                self.rules.push((target.to_string(), level));
            }
        }
        self.rules.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
        Ok(())
    }

    fn describe(&self) -> String {
        let mut s = format!("all={}", self.default.as_str().to_lowercase());
        for (t, l) in self.rules.iter().rev() {
            s.push_str(&format!(",{t}={}", l.as_str().to_lowercase()));
        }
        s
    }
}

struct Logger {
    start: Instant,
    file: Mutex<Option<std::fs::File>>,
    filter: RwLock<Filter>,
    initial: Filter,
    recent: Mutex<VecDeque<String>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Short target shown in brackets and matched by filters: `loom_engine::server::dim` becomes
/// `server::dim`, `loom_vulkan` becomes `vulkan`.
fn short(target: &str) -> &str {
    target.strip_prefix("loom_engine::").or_else(|| target.strip_prefix("loom_")).unwrap_or(target)
}

fn is_ours(target: &str) -> bool {
    target.starts_with("loom") || target.starts_with("mod:") || target == "vulkan" || CHANNELS.iter().any(|c| c.0 == target)
}

/// `verbose` is `--verbose` (debug for everything of ours), `spec` the `--log` filters.
pub fn init(file: Option<&Path>, verbose: bool, spec: &str) {
    let file = file.and_then(|p| {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
            rotate(dir, p);
        }
        std::fs::File::create(p).ok()
    });
    let mut filter = Filter { default: if verbose { LevelFilter::Debug } else { LevelFilter::Info }, rules: Vec::new() };
    let bad = filter.apply(spec).err();
    let logger = LOGGER.get_or_init(|| Logger {
        start: Instant::now(),
        file: Mutex::new(file),
        initial: filter.clone(),
        filter: RwLock::new(filter),
        recent: Mutex::new(VecDeque::with_capacity(RECENT)),
    });
    if log::set_logger(logger).is_ok() {
        refresh(logger);
    }
    enable_ansi();
    if let Some(e) = bad {
        log::warn!("--log: {e}");
    }
}

/// Changes filters at run time (`/log`): `reset` goes back to the command line ones. Returns the
/// filters now in effect.
pub fn set(spec: &str) -> Result<String, String> {
    let Some(l) = LOGGER.get() else { return Err("logging is not initialized".into()) };
    {
        let mut f = l.filter.write();
        if spec.trim() == "reset" {
            *f = l.initial.clone();
        } else {
            let mut next = f.clone();
            next.apply(spec)?;
            *f = next;
        }
    }
    refresh(l);
    Ok(l.filter.read().describe())
}

pub fn describe() -> String {
    LOGGER.get().map_or_else(String::new, |l| l.filter.read().describe())
}

fn refresh(l: &Logger) {
    let f = l.filter.read();
    log::set_max_level(f.max());
    for (i, (name, _)) in CHANNELS.iter().enumerate() {
        let level = f.level(name, true);
        CHANNEL_LEVEL[i].store(level as u8, Ordering::Relaxed);
    }
}

/// Whether a record for `target` at `level` would be written (mods ask before building costly
/// messages).
pub fn enabled(target: &str, level: Level) -> bool {
    LOGGER.get().is_some_and(|l| level <= l.filter.read().level(short(target), is_ours(target)))
}

/// The last lines written, oldest first, for crash reports.
pub fn recent() -> Vec<String> {
    LOGGER.get().map_or_else(Vec::new, |l| l.recent.lock().iter().cloned().collect())
}

pub fn flush() {
    log::logger().flush();
}

impl log::Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.filter.read().level(short(m.target()), is_ours(m.target()))
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let t = self.start.elapsed().as_secs_f32();
        let (color, name) = match record.level() {
            Level::Error => ("\x1b[1;31m", "ERROR"),
            Level::Warn => ("\x1b[33m", "WARN "),
            Level::Info => ("\x1b[32m", "INFO "),
            Level::Debug => ("\x1b[36m", "DEBUG"),
            Level::Trace => ("\x1b[90m", "TRACE"),
        };
        let short = short(record.target());
        let line = format!("{t:9.3} {name} [{short}] {}", record.args());
        let mut out = std::io::stderr().lock();
        let _ = writeln!(out, "\x1b[90m{t:9.3}\x1b[0m {color}{name}\x1b[0m \x1b[90m[{short}]\x1b[0m {}", record.args());
        if let Some(f) = self.file.lock().as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
        }
        let mut recent = self.recent.lock();
        if recent.len() == RECENT {
            recent.pop_front();
        }
        recent.push_back(line);
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().as_mut() {
            let _ = f.flush();
        }
    }
}

/// Keeps the previous `latest.log` under the time it was last written, and only the newest few.
fn rotate(dir: &Path, latest: &Path) {
    if let Ok(meta) = std::fs::metadata(latest) {
        let when = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let stamp = stamp(when);
        let mut to = dir.join(format!("{stamp}.log"));
        let mut n = 1;
        while to.exists() {
            to = dir.join(format!("{stamp}-{n}.log"));
            n += 1;
        }
        let _ = std::fs::rename(latest, to);
    }
    let mut old: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log") && p != latest).collect())
        .unwrap_or_default();
    old.sort();
    let extra = old.len().saturating_sub(KEEP_LOGS);
    for p in &old[..extra] {
        let _ = std::fs::remove_file(p);
    }
}

/// Local `YYYY-MM-DD_HH-MM-SS` for unix seconds, for file names that sort by time.
pub fn stamp(unix: u64) -> String {
    let t = unix as i64 + local_offset();
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

pub fn now_stamp() -> String {
    stamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

#[cfg(windows)]
fn local_offset() -> i64 {
    #[repr(C)]
    struct TimeZone {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: [u16; 8],
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: [u16; 8],
        daylight_bias: i32,
    }
    unsafe extern "system" {
        fn GetTimeZoneInformation(tz: *mut TimeZone) -> u32;
    }
    let mut tz: TimeZone = unsafe { std::mem::zeroed() };
    let r = unsafe { GetTimeZoneInformation(&mut tz) };
    // 2 = daylight saving time is in effect.
    let bias = tz.bias + if r == 2 { tz.daylight_bias } else { 0 };
    -(bias as i64) * 60
}

#[cfg(not(windows))]
fn local_offset() -> i64 {
    0
}

#[cfg(windows)]
fn enable_ansi() {
    // Older conhost windows only interpret escape codes with this flag set.
    unsafe extern "system" {
        fn GetStdHandle(n: u32) -> *mut core::ffi::c_void;
        fn GetConsoleMode(h: *mut core::ffi::c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: *mut core::ffi::c_void, mode: u32) -> i32;
    }
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    unsafe {
        let h = GetStdHandle(STD_ERROR_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | 0x4);
        }
    }
}

#[cfg(not(windows))]
fn enable_ansi() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_rules() {
        let mut f = Filter { default: LevelFilter::Info, rules: Vec::new() };
        f.apply("server=debug,server::dim=trace,mod:base=warn").unwrap();
        assert_eq!(f.level("server", true), LevelFilter::Debug);
        assert_eq!(f.level("server::regions", true), LevelFilter::Debug);
        assert_eq!(f.level("server::dim", true), LevelFilter::Trace);
        assert_eq!(f.level("serverx", true), LevelFilter::Info);
        assert_eq!(f.level("mod:base", true), LevelFilter::Warn);
        assert_eq!(f.level("iroh::magicsock", false), LevelFilter::Warn);
        f.apply("debug").unwrap();
        assert_eq!(f.level("client", true), LevelFilter::Debug);
        assert!(f.apply("load=loud").is_err());
        assert_eq!(f.max(), LevelFilter::Trace);
    }

    #[test]
    fn stamps_sort_by_time() {
        // Offsets move both stamps by the same amount, so only the order and format are checked.
        let a = stamp(1_700_000_000);
        let b = stamp(1_700_000_061);
        assert!(a < b && a.len() == 19);
    }
}
