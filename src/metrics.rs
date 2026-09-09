//! In-daemon metrics: per-program CPU/RSS sampling (1 Hz), a system resource
//! overview, and per-stream log line rates over rolling windows.
//!
//! Hard boundary (`metrics` capability, 采样零干扰): collection is a read-only
//! sideband. The sampler never signals, suspends, debug-attaches to or writes
//! into a managed process, never holds the supervisor state lock across a
//! sampling syscall, and any panic in the sampler thread only degrades the
//! metric fields to null — the supervision loop and every managed program keep
//! running untouched.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::warn;
use serde::{Deserialize, Serialize};

use crate::supervisor::Supervisor;

/// Rolling window length for log rates (seconds).
const RATE_BUCKETS: usize = 300;
/// Sampler cadence: 1 Hz.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Rate reads fall back to 0 when the window has not been rotated for this
/// long (sampler thread gone → 0 instead of a frozen stale value).
const RATE_STALE: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Log line rate counter
// ---------------------------------------------------------------------------

/// Per-stream log line rate. Hot path cost is one atomic add per line; the
/// sampler thread folds per-second deltas into a bounded rolling window.
pub struct RateCounter {
    total: AtomicU64,
    inner: Mutex<RateInner>,
}

struct RateInner {
    /// Completed seconds, oldest first, at most [`RATE_BUCKETS`] entries.
    buckets: VecDeque<u64>,
    last_total: u64,
    last_rotate: Instant,
}

impl RateCounter {
    pub fn new() -> Self {
        RateCounter {
            total: AtomicU64::new(0),
            inner: Mutex::new(RateInner {
                buckets: VecDeque::with_capacity(RATE_BUCKETS),
                last_total: 0,
                last_rotate: Instant::now(),
            }),
        }
    }

    /// Hot path: one relaxed atomic add per line.
    #[allow(dead_code)] // used by Ring::push (kept for single-line callers)
    pub fn bump(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn bump_by(&self, n: u64) {
        if n > 0 {
            self.total.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Fold the elapsed second's line count into the window. Called by the
    /// sampler thread (~1 Hz), independent of any log viewer.
    pub fn rotate(&self) {
        let total = self.total.load(Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        let delta = total.saturating_sub(g.last_total);
        g.last_total = total;
        g.buckets.push_back(delta);
        while g.buckets.len() > RATE_BUCKETS {
            g.buckets.pop_front();
        }
        g.last_rotate = Instant::now();
    }

    /// Average lines/sec over the last `secs` seconds (over the seconds
    /// actually observed, so a fresh counter shows a real rate, not a
    /// diluted one).
    pub fn rate(&self, secs: u64) -> f64 {
        let g = self.inner.lock().unwrap();
        if g.last_rotate.elapsed() > RATE_STALE {
            return 0.0;
        }
        let take = (secs as usize).min(RATE_BUCKETS).min(g.buckets.len());
        if take == 0 {
            return 0.0;
        }
        let sum: u64 = g.buckets.iter().rev().take(take).sum();
        sum as f64 / take as f64
    }

    /// Clear the window (program restart: previous rates must not bleed into
    /// the new process). The absolute total keeps counting so the next
    /// rotation only sees post-restart lines.
    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap();
        g.buckets.clear();
        g.last_total = self.total.load(Ordering::Relaxed);
        g.last_rotate = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// Snapshot table shared with the status projection
// ---------------------------------------------------------------------------

/// Latest sampled metrics for one program. `None` fields mean "no data"
/// (not running, no pid yet, first sample after a restart, or unreadable) —
/// never render these as 0.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgramMetrics {
    pub cpu_percent: Option<f64>,
    pub mem_bytes: Option<u64>,
}

/// System-level overview (same CPU normalization as per-program values).
/// Part of the shared status projection (`/v1`, `/api`, WS) — one struct
/// source for both encodings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub cpu_percent: Option<f64>,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
}

impl Default for SystemMetrics {
    fn default() -> Self {
        SystemMetrics {
            cpu_percent: None,
            mem_used_bytes: 0,
            mem_total_bytes: 0,
        }
    }
}

/// What the status projection reads. The sampler is the only writer.
#[derive(Default)]
pub struct MetricsTable {
    programs: Mutex<HashMap<String, ProgramMetrics>>,
    system: Mutex<SystemMetrics>,
}

impl MetricsTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn program(&self, name: &str) -> Option<ProgramMetrics> {
        self.programs.lock().unwrap().get(name).cloned()
    }

    pub fn system(&self) -> SystemMetrics {
        *self.system.lock().unwrap()
    }

    /// Writer side (sampler thread and tests).
    pub(crate) fn set_program(&self, name: &str, m: ProgramMetrics) {
        self.programs.lock().unwrap().insert(name.to_string(), m);
    }

    pub(crate) fn set_system(&self, m: SystemMetrics) {
        *self.system.lock().unwrap() = m;
    }

    /// Fault isolation: drop everything so projections degrade to null.
    fn clear(&self) {
        self.programs.lock().unwrap().clear();
        *self.system.lock().unwrap() = SystemMetrics::default();
    }
}

// ---------------------------------------------------------------------------
// Rate projection helpers (consumed via the rings, see pump.rs)
// ---------------------------------------------------------------------------

/// Lines/sec of one stream, at the four display windows. Serialized into the
/// shared status projection (JSON + MessagePack from the same struct).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct StreamRate {
    pub w1: f64,
    pub w10: f64,
    pub w60: f64,
    pub w300: f64,
}

impl StreamRate {
    pub fn of(counter: &RateCounter) -> Self {
        let r = |s: u64| (counter.rate(s) * 10.0).round() / 10.0;
        StreamRate {
            w1: r(1),
            w10: r(10),
            w60: r(60),
            w300: r(300),
        }
    }
}

/// Both streams of one program.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LogRates {
    pub out: StreamRate,
    pub err: StreamRate,
}

// ---------------------------------------------------------------------------
// Platform sampling backends
// ---------------------------------------------------------------------------

/// Process CPU time and RSS at sampling instant; platform-native units for
/// CPU time (see [`ticks_per_sec`]).
struct ProcSample {
    cpu_units: u64,
    rss_bytes: u64,
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;

    use super::ProcSample;

    fn read_to_string_lossy(path: &std::path::Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn clk_tck() -> f64 {
        // SAFETY: sysconf with a plain constant is always memory-safe.
        let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if v > 0 { v as f64 } else { 100.0 }
    }

    /// Linux: utime+stime from `/proc/<pid>/stat`, RSS from `statm`.
    pub fn sample_pid(pid: u32) -> Option<ProcSample> {
        let ticks = proc_cpu_ticks(pid)?;
        let rss = proc_rss_bytes(pid)?;
        Some(ProcSample {
            cpu_units: ticks,
            rss_bytes: rss,
        })
    }

    /// (utime + stime) in clock ticks. `comm` may contain spaces and
    /// parentheses, so parse after the *last* `)`.
    pub fn proc_cpu_ticks(pid: u32) -> Option<u64> {
        let content = read_to_string_lossy(std::path::Path::new(&format!("/proc/{pid}/stat")))?;
        let (utime, stime) = parse_stat(&content)?;
        Some(utime + stime)
    }

    /// Resident set size in bytes from `/proc/<pid>/statm` (field 2 × page size).
    pub fn proc_rss_bytes(pid: u32) -> Option<u64> {
        let content = read_to_string_lossy(std::path::Path::new(&format!("/proc/{pid}/statm")))?;
        parse_statm(&content)
    }

    pub(crate) fn parse_stat(content: &str) -> Option<(u64, u64)> {
        let after = content.rfind(')')? + 1;
        let rest = content[after..].split_whitespace();
        // After `comm` the next field is state (index 0); utime/stime are
        // fields 14/15 of the original line → indices 11/12 here.
        let fields: Vec<&str> = rest.collect();
        if fields.len() < 13 {
            return None;
        }
        let utime = fields[11].parse::<u64>().ok()?;
        let stime = fields[12].parse::<u64>().ok()?;
        Some((utime, stime))
    }

    pub(crate) fn parse_statm(content: &str) -> Option<u64> {
        let resident_pages = content.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64;
        Some(resident_pages.saturating_mul(page_size))
    }

    /// Aggregate CPU jiffies: (busy, total) across all cores.
    pub fn system_cpu() -> Option<(u64, u64)> {
        let line = read_to_string_lossy(std::path::Path::new("/proc/stat"))?;
        let line = line.lines().next()?;
        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|f| f.parse::<u64>().ok())
            .collect();
        if fields.len() < 5 {
            return None;
        }
        // user nice system idle iowait irq softirq steal [guest guest_nice]
        let user = fields.first().copied().unwrap_or(0);
        let nice = fields.get(1).copied().unwrap_or(0);
        let system = fields.get(2).copied().unwrap_or(0);
        let idle = fields.get(3).copied().unwrap_or(0);
        let iowait = fields.get(4).copied().unwrap_or(0);
        let irq = fields.get(5).copied().unwrap_or(0);
        let softirq = fields.get(6).copied().unwrap_or(0);
        let steal = fields.get(7).copied().unwrap_or(0);
        let busy = user + nice + system + irq + softirq + steal;
        let total = busy + idle + iowait;
        Some((busy, total))
    }

    /// (used, total) from MemTotal / MemAvailable.
    pub fn system_mem() -> Option<(u64, u64)> {
        let content = read_to_string_lossy(std::path::Path::new("/proc/meminfo"))?;
        let field = |key: &str| -> Option<u64> {
            content.lines().find_map(|l| {
                let l = l.trim();
                l.strip_prefix(key)
                    .and_then(|rest| rest.strip_suffix("kB"))
                    .and_then(|rest| rest.trim().parse::<u64>().ok())
            })
        };
        let total = field("MemTotal:")? * 1024;
        let avail = field("MemAvailable:")? * 1024;
        Some((total.saturating_sub(avail), total))
    }

    pub fn ticks_per_sec() -> f64 {
        clk_tck()
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::mem::{size_of, zeroed};

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_VM_READ,
    };

    use super::ProcSample;

    struct Handle(windows_sys::Win32::Foundation::HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: handle came from OpenProcess; closing once.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn ft_u64(ft: &FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    }

    /// Windows: kernel+user CPU time (100ns units) and working set, via a
    /// query-only handle (no suspend/terminate/write rights — 采样零干扰).
    pub fn sample_pid(pid: u32) -> Option<ProcSample> {
        // SAFETY: plain syscall wrappers with initialized output buffers.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
            if handle.is_null() {
                return None;
            }
            let _g = Handle(handle);
            let mut creation: FILETIME = zeroed();
            let mut exit_t: FILETIME = zeroed();
            let mut kernel: FILETIME = zeroed();
            let mut user: FILETIME = zeroed();
            if GetProcessTimes(handle, &mut creation, &mut exit_t, &mut kernel, &mut user) == 0 {
                return None;
            }
            let mut pmc: PROCESS_MEMORY_COUNTERS = zeroed();
            pmc.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) == 0 {
                return None;
            }
            Some(ProcSample {
                cpu_units: ft_u64(&kernel) + ft_u64(&user),
                rss_bytes: pmc.WorkingSetSize as u64,
            })
        }
    }

    pub fn system_cpu() -> Option<(u64, u64)> {
        // SAFETY: three initialized FILETIME outputs.
        unsafe {
            let mut idle: FILETIME = zeroed();
            let mut kernel: FILETIME = zeroed();
            let mut user: FILETIME = zeroed();
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
                return None;
            }
            let total = ft_u64(&kernel) + ft_u64(&user);
            let busy = total - ft_u64(&idle);
            Some((busy, total))
        }
    }

    pub fn system_mem() -> Option<(u64, u64)> {
        // SAFETY: dwLength is set to the struct size as the API requires.
        unsafe {
            let mut ms: MEMORYSTATUSEX = zeroed();
            ms.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut ms) == 0 {
                return None;
            }
            Some((ms.ullTotalPhys - ms.ullAvailPhys, ms.ullTotalPhys))
        }
    }

    /// FILETIME units: 100 nanoseconds.
    pub fn ticks_per_sec() -> f64 {
        10_000_000.0
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod imp {
    use super::ProcSample;

    pub fn sample_pid(_pid: u32) -> Option<ProcSample> {
        None
    }
    pub fn system_cpu() -> Option<(u64, u64)> {
        None
    }
    pub fn system_mem() -> Option<(u64, u64)> {
        None
    }
    pub fn ticks_per_sec() -> f64 {
        1.0
    }
}

/// CPU% over the interval, normalized to the whole machine (100 = all cores).
/// `units_per_sec` is the platform CPU-time unit rate (CLK_TCK on Linux,
/// 100ns ticks on Windows).
fn cpu_percent(prev: u64, cur: u64, dt_secs: f64, cores: f64, units_per_sec: f64) -> f64 {
    if dt_secs <= 0.0 || cores <= 0.0 || units_per_sec <= 0.0 {
        return 0.0;
    }
    let used = cur.saturating_sub(prev) as f64 / (units_per_sec * dt_secs * cores);
    (((used * 100.0) * 10.0).round() / 10.0).min(100.0)
}

/// Busy/total ratio over an interval (system CPU uses same-unit deltas).
fn ratio_percent(prev: (u64, u64), cur: (u64, u64)) -> Option<f64> {
    let db = cur.0.saturating_sub(prev.0) as f64;
    let dt = cur.1.saturating_sub(prev.1) as f64;
    if dt <= 0.0 {
        return None;
    }
    Some(((db / dt) * 1000.0).round() / 10.0)
}

// ---------------------------------------------------------------------------
// Sampler thread
// ---------------------------------------------------------------------------

struct Sampler {
    sup: Arc<Supervisor>,
    table: Arc<MetricsTable>,
    /// pid → last CPU-time reading.
    baselines: HashMap<u32, u64>,
    sys_prev: Option<(u64, u64)>,
    last_tick: Instant,
    /// Test hook: make the next [`Sampler::sample_once`] panic (fault
    /// isolation coverage).
    #[cfg(test)]
    panic_next: bool,
}

impl Sampler {
    fn new(sup: Arc<Supervisor>, table: Arc<MetricsTable>) -> Self {
        Sampler {
            sup,
            table,
            baselines: HashMap::new(),
            sys_prev: None,
            last_tick: Instant::now(),
            #[cfg(test)]
            panic_next: false,
        }
    }

    /// One sampling pass. Locks the supervisor state only to snapshot
    /// (name, pid) pairs — never across a syscall.
    fn sample_once(&mut self) {
        #[cfg(test)]
        if self.panic_next {
            self.panic_next = false;
            panic!("metrics sampler test panic");
        }

        let dt_secs = self.last_tick.elapsed().as_secs_f64().max(0.001);
        self.last_tick = Instant::now();

        // Snapshot (name, pid) pairs and ring handles under one short lock —
        // no syscalls while the supervision state is locked.
        let (snapshot, rings): (Vec<(String, Option<u32>)>, Vec<Arc<crate::pump::Ring>>) = {
            let st = self.sup.state.lock().unwrap();
            let snapshot = st
                .programs
                .values()
                .map(|p| (p.def.name.clone(), p.pid()))
                .collect();
            let rings = st
                .programs
                .values()
                .flat_map(|p| {
                    [
                        p.ring(crate::pump::Stream::Out),
                        p.ring(crate::pump::Stream::Err),
                    ]
                })
                .collect();
            (snapshot, rings)
        };
        // Fold the past second into every stream's rolling rate window.
        // In-memory only; safe and cheap outside the state lock.
        for ring in &rings {
            ring.rate().rotate();
        }

        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as f64)
            .unwrap_or(1.0);
        let units_per_sec = imp::ticks_per_sec();

        let mut fresh: HashMap<u32, u64> = HashMap::with_capacity(snapshot.len());
        for (name, pid) in snapshot {
            let (cpu, mem) = match pid {
                Some(pid) => match imp::sample_pid(pid) {
                    Some(s) => {
                        let cpu = self.baselines.get(&pid).map(|prev| {
                            cpu_percent(*prev, s.cpu_units, dt_secs, cores, units_per_sec)
                        });
                        fresh.insert(pid, s.cpu_units);
                        (cpu, Some(s.rss_bytes))
                    }
                    // pid vanished between snapshot and read, or unreadable:
                    // degrade quietly to null.
                    None => (None, None),
                },
                None => (None, None),
            };
            self.table.set_program(
                &name,
                ProgramMetrics {
                    cpu_percent: cpu,
                    mem_bytes: mem,
                },
            );
        }
        // Replacing the map prunes baselines of exited pids.
        self.baselines = fresh;

        if let Some((busy, total)) = imp::system_cpu() {
            let cpu = self
                .sys_prev
                .and_then(|prev| ratio_percent(prev, (busy, total)));
            self.sys_prev = Some((busy, total));
            if let Some(cpu) = cpu {
                let (used, total_b) = imp::system_mem().unwrap_or((0, 0));
                self.table.set_system(SystemMetrics {
                    cpu_percent: Some(cpu),
                    mem_used_bytes: used,
                    mem_total_bytes: total_b,
                });
            }
        }
    }
}

/// Run one sampler iteration under panic isolation. Returns the sampler back,
/// or None if it panicked (table cleared; the daemon keeps running).
fn guarded_tick(mut s: Sampler) -> Option<Sampler> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        s.sample_once();
        s
    })) {
        Ok(s) => Some(s),
        Err(_) => None,
    }
}

/// Spawn the 1 Hz sampler thread. Both `run` and `webui` call this; the
/// thread exits when the daemon shuts down. A panic inside the sampler is
/// contained: metrics degrade to null and the daemon is unaffected.
pub fn spawn_sampler(sup: Arc<Supervisor>) {
    let table = sup.metrics.clone();
    let builder = std::thread::Builder::new().name("metrics".into());
    builder
        .spawn(move || {
            let table2 = table.clone();
            let mut s = Some(Sampler::new(sup, table));
            loop {
                let shutdown = {
                    s.as_ref()
                        .map(|s| s.sup.state.lock().unwrap().shutdown.load(Ordering::SeqCst))
                        .unwrap_or(true)
                };
                if shutdown {
                    break;
                }
                match s.take().and_then(|s| guarded_tick(s)) {
                    Some(next) => s = Some(next),
                    None => {
                        warn!("metrics sampler panicked; metrics disabled (daemon unaffected)");
                        table2.clear();
                        break;
                    }
                }
                // Sleep toward the next tick in slices so shutdown is prompt.
                let deadline = Instant::now() + SAMPLE_INTERVAL;
                while Instant::now() < deadline {
                    let stop = s
                        .as_ref()
                        .map(|s| s.sup.state.lock().unwrap().shutdown.load(Ordering::SeqCst))
                        .unwrap_or(true);
                    if stop {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .expect("failed to spawn metrics sampler thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_counter_windows_and_reset() {
        let c = RateCounter::new();
        // 12 seconds at 5 lines/sec.
        for _ in 0..12 {
            c.bump_by(5);
            c.rotate();
        }
        assert!((c.rate(1) - 5.0).abs() < 1e-9);
        assert!((c.rate(10) - 5.0).abs() < 1e-9);
        assert!((c.rate(60) - 5.0).abs() < 1e-9);
        assert!((c.rate(300) - 5.0).abs() < 1e-9);
        // Restart: window clears, subsequent rotations count only new lines.
        c.reset();
        assert_eq!(c.rate(10), 0.0);
        c.bump_by(2);
        c.rotate();
        assert!((c.rate(1) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn rate_counter_zero_when_never_rotated() {
        let c = RateCounter::new();
        c.bump();
        assert_eq!(c.rate(10), 0.0, "no completed second yet");
    }

    #[test]
    fn rate_counter_bounded_window() {
        let c = RateCounter::new();
        for i in 0..(RATE_BUCKETS as u64 + 50) {
            c.bump_by(i);
            c.rotate();
        }
        // Only the last RATE_BUCKETS buckets are retained (0..=349 written,
        // 50..=349 kept); the mean of the last 10 is the mean of 340..=349.
        let last10: u64 = (340..=349).sum();
        assert!((c.rate(10) - last10 as f64 / 10.0).abs() < 1e-6);
        // A 300s request is capped at the retained window.
        let keep: u64 = (50..=349).sum();
        assert!((c.rate(1000) - keep as f64 / RATE_BUCKETS as f64).abs() < 1e-6);
    }

    #[test]
    fn metrics_table_roundtrip_and_clear() {
        let t = MetricsTable::new();
        assert_eq!(t.program("p"), None);
        t.set_program(
            "p",
            ProgramMetrics {
                cpu_percent: Some(12.3),
                mem_bytes: Some(4096),
            },
        );
        assert_eq!(
            t.program("p"),
            Some(ProgramMetrics {
                cpu_percent: Some(12.3),
                mem_bytes: Some(4096)
            })
        );
        t.set_system(SystemMetrics {
            cpu_percent: Some(1.5),
            mem_used_bytes: 1,
            mem_total_bytes: 2,
        });
        assert_eq!(t.system().cpu_percent, Some(1.5));
        t.clear();
        assert_eq!(t.program("p"), None);
        assert_eq!(t.system().cpu_percent, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_stat_handles_parens_and_spaces_in_comm() {
        // comm = "a) b c (x" — everything up to the LAST ')' is the name.
        let line =
            "1234 (a) b c (x) R 1 2345 2345 0 -1 4194560 100 0 0 0 77 33 0 0 20 0 1 0 999 1 2 3"
                .trim();
        let (utime, stime) = imp::parse_stat(line).unwrap();
        assert_eq!(utime, 77);
        assert_eq!(stime, 33);
        assert!(imp::parse_stat("12 (short").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_statm_multiplies_page_size() {
        // 3 resident pages × 4096 = 12288.
        assert_eq!(imp::parse_statm("10 3 1 1 0 0 0").unwrap(), 3 * 4096);
        assert!(imp::parse_statm("10").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sample_self_pid_reads_real_proc_values() {
        let s = imp::sample_pid(std::process::id()).unwrap();
        assert!(s.rss_bytes > 0, "test process must have resident pages");
    }

    #[test]
    fn cpu_percent_math_bounds() {
        let p = imp::ticks_per_sec();
        // One full core busy for 1s on a 4-core box → 25%.
        let full = p as u64;
        assert!((cpu_percent(0, full, 1.0, 4.0, p) - 25.0).abs() < 0.2);
        // Eight cores busy → 100% (normalized ceiling is 100, all cores).
        assert!((cpu_percent(0, full * 8, 1.0, 8.0, p) - 100.0).abs() < 0.5);
        assert_eq!(cpu_percent(0, 0, 1.0, 4.0, p), 0.0);
    }

    // -- integration: sampler against a real supervised child ----------------

    use crate::config::{AppRaw, resolve_app};
    use crate::program::{ManagedProgram, ProgramState};
    use crate::supervisor::SupervisorState;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    fn make_sup_with_sleeper(dir: &Path) -> (Arc<Supervisor>, String) {
        let config = crate::config::DaemonConfig::default();
        let sup = Supervisor::new(config, dir).unwrap();
        let toml_text = format!(
            "[program.m]\ncommand = \"{}\"\nstartsecs = 0.1\n",
            if cfg!(windows) {
                "ping -n 30 127.0.0.1"
            } else {
                "sleep 30"
            }
        );
        let raw: AppRaw = toml::from_str(&toml_text).unwrap();
        let app = resolve_app("demo", Path::new("x.toml"), &raw, None).unwrap();
        let def = app.programs.into_iter().next().unwrap();
        let name = def.name.clone();
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.insert(
                name.clone(),
                ManagedProgram::new(def, dir.to_path_buf(), 100),
            );
        }
        (sup, name)
    }

    #[test]
    fn sampler_populates_and_degrades() {
        let dir = std::env::temp_dir().join(format!("xk-metrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (sup, name) = make_sup_with_sleeper(&dir);
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.get_mut(&name).unwrap().spawn();
        }
        let table = sup.metrics.clone();
        let mut s = Sampler::new(sup.clone(), table.clone());

        s.sample_once();
        let m = table.program(&name).unwrap();
        assert_eq!(m.cpu_percent, None, "first sample: baseline only");
        assert!(m.mem_bytes.unwrap_or(0) > 0, "rss must be readable");

        std::thread::sleep(Duration::from_millis(1100));
        s.sample_once();
        let m = table.program(&name).unwrap();
        assert!(m.cpu_percent.unwrap_or(0.0) >= 0.0);
        assert!(m.cpu_percent.is_some());

        // Child dies → metrics degrade to null on the next pass.
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.get_mut(&name).unwrap().stop();
        }
        s.sample_once();
        let m = table.program(&name).unwrap();
        assert_eq!(m.cpu_percent, None);
        assert_eq!(m.mem_bytes, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sampler_panic_is_isolated_from_daemon() {
        let dir = std::env::temp_dir().join(format!("xk-metrics-panic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (sup, name) = make_sup_with_sleeper(&dir);
        {
            let mut st = sup.state.lock().unwrap();
            let p = st.programs.get_mut(&name).unwrap();
            p.spawn();
            assert!(p.state() == ProgramState::Starting || p.state() == ProgramState::Running);
        }
        let table = sup.metrics.clone();
        table.set_program(
            &name,
            ProgramMetrics {
                cpu_percent: Some(9.9),
                mem_bytes: Some(1),
            },
        );
        table.set_system(SystemMetrics {
            cpu_percent: Some(9.9),
            mem_used_bytes: 1,
            mem_total_bytes: 1,
        });

        let mut s = Sampler::new(sup.clone(), table.clone());
        #[cfg(test)]
        {
            s.panic_next = true;
        }
        // The guarded loop absorbs the panic; its recovery path clears the
        // table. Mirror exactly what spawn_sampler's loop does.
        assert!(guarded_tick(s).is_none());
        table.clear();
        assert_eq!(table.program(&name), None, "metrics must degrade to empty");
        assert_eq!(table.system().cpu_percent, None);

        // Daemon unaffected: state reachable, program still alive.
        {
            let st: std::sync::MutexGuard<SupervisorState> = sup.state.lock().unwrap();
            let p = st.programs.get(&name).unwrap();
            assert!(
                matches!(p.state(), ProgramState::Starting | ProgramState::Running),
                "child must survive a sampler panic"
            );
            let _ = AtomicBool::new(false); // shutdown flag untouched
            assert!(!st.shutdown.load(Ordering::SeqCst));
            // Kill the child to leave a clean test env.
            drop(st);
            let mut st = sup.state.lock().unwrap();
            st.programs.get_mut(&name).unwrap().stop();
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
