//! Log pumps: one thread per child output stream. Each line is pushed into
//! an in-memory ring buffer (for `log --tail/--follow` and the web console),
//! broadcast to bounded subscribers, and appended to the on-disk log file
//! with size-based rotation.
//!
//! Backpressure boundary (log-management capability, 输出排空零反压): the
//! pump thread is the only thing draining the child's pipe. Nothing below it
//! — file writes, rotation, ring contention, slow or stalled subscribers —
//! may stop it for longer than one bounded flush. File work is batched
//! (≥[`FLUSH_BYTES`] or a drained pipe), and subscriber delivery is
//! non-blocking: a slow viewer only loses its own copy (surfaced as a Gap),
//! never the child's throughput.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::{debug, warn};

use crate::metrics::RateCounter;

/// Pending bytes (or a drained pipe) trigger one flush: a single file write
/// plus a single ring push per batch instead of per line.
const FLUSH_BYTES: usize = 256 * 1024;
/// A pipe read that took at least this long had to block (nothing was
/// available): the quiet-period burst just processed is flushed immediately
/// so followers are not delayed until the next line.
const BLOCK_THRESHOLD: Duration = Duration::from_millis(1);
/// Subscriber queue depth in batches. Depth × batch ≈ 16 MiB in flight per
/// subscriber — a normally-consuming client never triggers a Gap.
pub(crate) const SUB_CHANNEL_BATCHES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Out,
    Err,
}

impl Stream {
    pub fn suffix(self) -> &'static str {
        match self {
            Stream::Out => "out",
            Stream::Err => "err",
        }
    }
}

/// One delivery unit for a live subscriber: a batch of complete lines, or a
/// loss marker telling the consumer that N earlier lines will never arrive
/// on this channel (they remain intact in the ring and on disk).
#[derive(Debug, Clone, PartialEq)]
pub enum SubItem {
    Lines(Vec<String>),
    Gap(u64),
}

struct Subscriber {
    tx: SyncSender<SubItem>,
    /// Lines dropped because this subscriber's queue was full; delivered as
    /// a Gap before the next successful batch.
    dropped: u64,
}

impl Subscriber {
    /// Non-blocking delivery (零反压: the pump never waits on a viewer). A
    /// full or disconnected queue only drops THIS subscriber's copy.
    fn deliver(&mut self, lines: &[String]) -> bool {
        if self.dropped > 0 {
            let n = std::mem::replace(&mut self.dropped, 0);
            match self.tx.try_send(SubItem::Gap(n)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.dropped = n + lines.len() as u64;
                    return true;
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        match self.tx.try_send(SubItem::Lines(lines.to_vec())) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped += lines.len() as u64;
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/// Bounded ring of recent lines plus live-follow subscribers. Each ring also
/// counts its lines for the log-rate metrics (hot path: one atomic add per
/// line — no lock, no viewer required).
pub struct Ring {
    inner: Mutex<RingInner>,
    rate: RateCounter,
}

struct RingInner {
    lines: VecDeque<String>,
    cap: usize,
    subs: Vec<Subscriber>,
}

impl Ring {
    pub fn new(cap: usize) -> Arc<Ring> {
        Arc::new(Ring {
            inner: Mutex::new(RingInner {
                lines: VecDeque::with_capacity(cap),
                cap,
                subs: Vec::new(),
            }),
            rate: RateCounter::new(),
        })
    }

    /// Append a batch of complete lines: one rate add, one lock, one
    /// delivery attempt per subscriber.
    pub fn push_batch(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        self.rate.bump_by(lines.len() as u64);
        let mut g = self.inner.lock().unwrap();
        for line in lines {
            if g.lines.len() == g.cap {
                g.lines.pop_front();
            }
            g.lines.push_back(line.clone());
        }
        g.subs.retain_mut(|s| s.deliver(lines));
    }

    #[allow(dead_code)] // single-line convenience; hot path is push_batch
    pub fn push(&self, line: String) {
        self.push_batch(std::slice::from_ref(&line));
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        g.lines.iter().rev().take(n).rev().cloned().collect()
    }

    /// Subscribe to live batches. The channel is bounded: consuming slower
    /// than the pump produces loses only this subscriber's undelivered
    /// batches (signalled via [`SubItem::Gap`]); it can never slow the
    /// child, the ring, or the log files.
    pub fn subscribe(&self) -> Receiver<SubItem> {
        let (tx, rx) = mpsc::sync_channel(SUB_CHANNEL_BATCHES);
        self.inner
            .lock()
            .unwrap()
            .subs
            .push(Subscriber { tx, dropped: 0 });
        rx
    }

    /// The rate counter of this stream (rotation is driven by the metrics
    /// sampler; reading works at any time).
    pub fn rate(&self) -> &RateCounter {
        &self.rate
    }

    /// Clear the rate window (program restart: the previous process's rates
    /// must not bleed into the new one).
    pub fn reset_rate(&self) {
        self.rate.reset();
    }
}

pub struct PumpSet {
    handles: Vec<JoinHandle<()>>,
}

impl PumpSet {
    /// Wait for both pump threads. Only safe when the child's pipe writers
    /// are known dead (normal EOF); see [`Drop`][PumpSet::drop] for why the
    /// teardown path must NOT join.
    #[allow(dead_code)]
    pub fn join(&mut self) {
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for PumpSet {
    fn drop(&mut self) {
        // DETACH, never join. Joining here hangs the supervisor when a
        // child's pipeline grandchildren escape the process-group kill
        // (e.g. `timeout` puts its command in a new group): they keep the
        // pipe open, the pump never sees EOF, and stop()/shutdown would
        // block forever (线上事故级). A detached pump simply exits on EOF —
        // or with process exit — and holds only bounded resources
        // (Arc<Ring> + the log file handle).
        for h in self.handles.drain(..) {
            let _ = h; // detached
        }
    }
}

pub fn log_path(log_dir: &Path, name: &str, stream: Stream) -> PathBuf {
    log_dir.join(format!("{}.{}.log", name, stream.suffix()))
}

/// Start one pump thread per stream. Called right after spawn with the
/// child's piped stdout/stderr. The rings outlive the child so tails and
/// followers keep working after restarts.
pub fn start(
    out: std::process::ChildStdout,
    err: std::process::ChildStderr,
    name: &str,
    out_ring: Arc<Ring>,
    err_ring: Arc<Ring>,
    log_dir: &Path,
    max_size: Option<u64>,
    rotate_keep: u32,
) -> PumpSet {
    let out_path = log_path(log_dir, name, Stream::Out);
    let err_path = log_path(log_dir, name, Stream::Err);
    let h_out = std::thread::Builder::new()
        .name(format!("pump-{name}-out"))
        .spawn(move || pump(out, out_ring, out_path, max_size, rotate_keep))
        .expect("failed to spawn pump thread");
    let h_err = std::thread::Builder::new()
        .name(format!("pump-{name}-err"))
        .spawn(move || pump(err, err_ring, err_path, max_size, rotate_keep))
        .expect("failed to spawn pump thread");
    PumpSet {
        handles: vec![h_out, h_err],
    }
}

/// Split a raw pipe chunk into complete lines, carrying an incomplete tail
/// over to the next chunk. Appends to `batch`; `bytes` counts line bytes
/// (+ newline) for flush thresholds.
fn collect_lines(chunk: &[u8], leftover: &mut Vec<u8>, batch: &mut Vec<String>, bytes: &mut usize) {
    let mut start = 0usize;
    for (i, b) in chunk.iter().enumerate() {
        if *b == b'\n' {
            leftover.extend_from_slice(&chunk[start..i]);
            start = i + 1;
            let mut s = String::from_utf8_lossy(leftover).into_owned();
            if s.ends_with('\r') {
                s.pop();
            }
            *bytes += s.len() + 1;
            batch.push(s);
            leftover.clear();
        }
    }
    leftover.extend_from_slice(&chunk[start..]);
}

/// Commit one batch: rotate if needed, append to the ring (memory is the
/// source of truth for tails/followers), then one file write. Write failure
/// degrades to memory-only and never blocks the pipe drain.
fn flush_batch(
    ring: &Ring,
    batch: &mut Vec<String>,
    file: &mut File,
    size: &mut u64,
    path: &Path,
    limit: Option<u64>,
    keep: u32,
    warned: &mut bool,
) {
    if batch.is_empty() {
        return;
    }
    if let Some(l) = limit {
        if *size >= l {
            match rotate(path, keep).and_then(|_| open_append(path)) {
                Ok(f) => {
                    *file = f;
                    *size = 0;
                }
                Err(e) => {
                    if !*warned {
                        warn!("log rotation failed for {}: {e}", path.display());
                        *warned = true;
                    }
                }
            }
        }
    }
    ring.push_batch(batch);
    let mut data = batch.join("\n");
    data.push('\n');
    match file.write_all(data.as_bytes()) {
        Ok(()) => *size += data.len() as u64,
        Err(e) => {
            if !*warned {
                warn!("cannot write log file {}: {e}", path.display());
                *warned = true;
            }
        }
    }
    batch.clear();
}

/// `pub(crate)` for integration tests that push synthetic bursts through the
/// real flush path.
pub(crate) fn pump(
    reader: impl Read,
    ring: Arc<Ring>,
    path: PathBuf,
    max_size: Option<u64>,
    keep: u32,
) {
    let mut file = match open_append(&path) {
        Ok(f) => f,
        Err(e) => {
            warn!(
                "cannot open log file {}: {e}; output goes to memory only",
                path.display()
            );
            drain_only(reader, ring);
            return;
        }
    };
    let mut size = file.seek(SeekFrom::End(0)).unwrap_or(0);
    let mut reader = reader;
    let mut buf = vec![0u8; 64 * 1024];
    let mut leftover: Vec<u8> = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let mut batch_bytes = 0usize;
    let mut warned = false;
    // First iteration behaves as if the previous read blocked, so a batch
    // carried over from anywhere is flushed before the first blocking read.
    let mut prev_read_blocked = true;
    loop {
        if prev_read_blocked {
            flush_batch(
                &ring,
                &mut batch,
                &mut file,
                &mut size,
                &path,
                max_size,
                keep,
                &mut warned,
            );
            batch_bytes = 0;
        }
        let t0 = Instant::now();
        let n = match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) => {
                debug!("log pump read error: {e}");
                break;
            }
        };
        // A slow read means the pipe ran dry: the child paused. Flush right
        // away so live viewers see the burst now, not at the next burst.
        prev_read_blocked = t0.elapsed() >= BLOCK_THRESHOLD;
        collect_lines(&buf[..n], &mut leftover, &mut batch, &mut batch_bytes);
        // During sustained output the read keeps returning instantly; cap
        // the batch so file writes and ring locks stay bounded.
        if batch_bytes >= FLUSH_BYTES {
            flush_batch(
                &ring,
                &mut batch,
                &mut file,
                &mut size,
                &path,
                max_size,
                keep,
                &mut warned,
            );
            batch_bytes = 0;
        }
    }
    // EOF: the trailing partial line (no final newline) still counts, then
    // flush whatever is pending.
    if !leftover.is_empty() {
        let s = String::from_utf8_lossy(&leftover).into_owned();
        batch.push(s);
    }
    flush_batch(
        &ring,
        &mut batch,
        &mut file,
        &mut size,
        &path,
        max_size,
        keep,
        &mut warned,
    );
}

/// Best-effort drain when the file cannot be opened at all.
fn drain_only(reader: impl Read, ring: Arc<Ring>) {
    let mut reader = reader;
    let mut buf = vec![0u8; 64 * 1024];
    let mut leftover: Vec<u8> = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                collect_lines(&buf[..n], &mut leftover, &mut batch, &mut bytes);
                if batch.len() >= 4096 {
                    ring.push_batch(&batch);
                    batch.clear();
                }
            }
        }
    }
    if !leftover.is_empty() {
        batch.push(String::from_utf8_lossy(&leftover).into_owned());
    }
    ring.push_batch(&batch);
}

fn open_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Shift `path` -> `path.1` -> ... -> `path.<keep>`, dropping the oldest.
fn rotate(path: &Path, keep: u32) -> std::io::Result<()> {
    if keep == 0 {
        std::fs::remove_file(path)?;
        return Ok(());
    }
    for i in (1..keep).rev() {
        let from = numbered(path, i);
        let to = numbered(path, i + 1);
        if from.exists() {
            let _ = std::fs::remove_file(&to);
            std::fs::rename(&from, &to)?;
        }
    }
    let first = numbered(path, 1);
    let _ = std::fs::remove_file(&first);
    std::fs::rename(path, &first)?;
    Ok(())
}

fn numbered(path: &Path, i: u32) -> PathBuf {
    let os = path.as_os_str().to_string_lossy();
    PathBuf::from(format!("{os}.{i}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_tail_and_subscribe() {
        let r = Ring::new(3);
        for i in 0..5 {
            r.push(format!("line{i}"));
        }
        assert_eq!(r.tail(10), vec!["line2", "line3", "line4"]);
        assert_eq!(r.tail(2), vec!["line3", "line4"]);
        let rx = r.subscribe();
        r.push("live".to_string());
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_millis(100))
                .unwrap(),
            SubItem::Lines(vec!["live".to_string()])
        );
    }

    #[test]
    fn rate_counts_without_viewers_and_resets() {
        let r = Ring::new(10);
        for _ in 0..5 {
            r.push("x".to_string());
        }
        // Sampler normally rotates at 1 Hz; do it by hand here.
        r.rate().rotate();
        assert!(
            (r.rate().rate(1) - 5.0).abs() < 1e-9,
            "counts with no subscriber"
        );
        // Restart semantics: window clears, old lines don't count afterwards.
        r.reset_rate();
        assert_eq!(r.rate().rate(1), 0.0);
        r.push("after-restart".to_string());
        r.rate().rotate();
        assert!((r.rate().rate(1) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn slow_subscriber_gets_gap_ring_and_disk_stay_complete() {
        let tmp = std::env::temp_dir().join(format!("xk-gap-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let ring = Ring::new(1000);
        let path = tmp.join("g.log");
        let file = open_append(&path).unwrap();

        let rx = ring.subscribe();
        // Fill the subscriber's bounded queue, then overflow it.
        let mut file = file;
        let mut size = 0u64;
        let mut warned = false;
        let total_batches = SUB_CHANNEL_BATCHES + 8;
        for b in 0..total_batches {
            let lines: Vec<String> = (0..10).map(|i| format!("b{b}l{i}")).collect();
            flush_batch(
                &ring,
                &mut lines.clone(),
                &mut file,
                &mut size,
                &path,
                None,
                2,
                &mut warned,
            );
        } // Ring has every line even though the subscriber queue overflowed.
        assert_eq!(ring.tail(10000).len(), total_batches * 10);
        // Drain the queue: batches only — no Gap ever made it in while the
        // queue was full; the loss is accounted on the subscriber.
        let mut seen_lines = 0usize;
        let mut seen_gaps = 0u64;
        while let Ok(item) = rx.try_recv() {
            match item {
                SubItem::Lines(v) => seen_lines += v.len(),
                SubItem::Gap(n) => seen_gaps += n,
            }
        }
        assert_eq!(seen_lines, SUB_CHANNEL_BATCHES * 10);
        assert_eq!(seen_gaps, 0);
        // One more batch: the pending loss is delivered as a Gap BEFORE it.
        let fresh = vec!["fresh".to_string()];
        flush_batch(
            &ring,
            &mut fresh.clone(),
            &mut file,
            &mut size,
            &path,
            None,
            2,
            &mut warned,
        );
        match rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
        {
            SubItem::Gap(n) => assert_eq!(n, 8 * 10, "exactly the dropped lines"),
            SubItem::Lines(_) => panic!("gap must precede the next delivered batch"),
        }
        match rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
        {
            SubItem::Lines(v) => assert_eq!(v, fresh),
            SubItem::Gap(_) => panic!("unexpected second gap"),
        }
        // Disk copy is complete: all 720 batch lines plus the fresh one.
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), total_batches * 10 + 1);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn gap_precedes_next_delivered_batch() {
        let r = Ring::new(1000);
        let rx = r.subscribe();
        // Overflow the queue without draining it.
        for b in 0..(SUB_CHANNEL_BATCHES + 3) {
            let line = format!("overflow-{b}");
            r.push_batch(&[line]);
        }
        // Drain the queued batches; none of them is a Gap.
        while let Ok(item) = rx.try_recv() {
            assert!(
                matches!(item, SubItem::Lines(_)),
                "no gap while queue was full"
            );
        }
        // The next successful delivery starts with the accumulated Gap.
        r.push_batch(&["fresh".to_string()]);
        match rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
        {
            SubItem::Gap(n) => assert_eq!(n, 3),
            SubItem::Lines(_) => panic!("gap must precede the next delivered batch"),
        }
        match rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
        {
            SubItem::Lines(v) => assert_eq!(v, vec!["fresh"]),
            SubItem::Gap(_) => panic!("gap must not be re-delivered after catch-up"),
        }
    }

    #[test]
    fn dead_subscriber_is_dropped_silently() {
        let r = Ring::new(10);
        let rx = r.subscribe();
        drop(rx);
        r.push("nobody listens".to_string());
        assert_eq!(r.tail(10).len(), 1, "ring unaffected by dead subscriber");
    }

    #[test]
    fn collect_lines_handles_split_chunks_and_crlf() {
        let mut leftover: Vec<u8> = Vec::new();
        let mut batch: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        collect_lines(b"hel", &mut leftover, &mut batch, &mut bytes);
        assert!(batch.is_empty());
        collect_lines(b"lo\r\nwor", &mut leftover, &mut batch, &mut bytes);
        assert_eq!(batch, vec!["hello"]);
        collect_lines(b"ld\n", &mut leftover, &mut batch, &mut bytes);
        assert_eq!(batch, vec!["hello", "world"]);
        assert_eq!(bytes, "hello\n".len() + "world\n".len());
    }

    #[test]
    fn pump_batches_lines_and_flushes_on_eof() {
        let tmp = std::env::temp_dir().join(format!("xk-pump-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let data = "l1\nl2\nl3\n";
        let reader = std::io::Cursor::new(data.as_bytes().to_vec());
        let ring = Ring::new(10);
        let path = tmp.join("p.log");
        pump(reader, ring.clone(), path.clone(), Some(4), 2);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("l"));
        assert_eq!(ring.tail(10), vec!["l1", "l2", "l3"]);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn pump_flushes_partial_line_at_eof() {
        let tmp = std::env::temp_dir().join(format!("xk-pump-eof-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let reader = std::io::Cursor::new(b"complete\npartial-no-newline".to_vec());
        let ring = Ring::new(10);
        let path = tmp.join("p.log");
        pump(reader, ring.clone(), path.clone(), None, 2);
        assert_eq!(ring.tail(10), vec!["complete", "partial-no-newline"]);
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// log-management 输出排空零反压: an unwritable log location must not
    /// stop the drain — output still reaches the ring (memory is the source
    /// of truth for tails/followers) and the pump keeps going. Unix-only:
    /// root bypasses permission bits, and Windows has no chmod.
    #[test]
    #[cfg(unix)]
    fn unwritable_log_dir_degrades_to_memory_only() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root can write anywhere; the scenario cannot be built
        }
        let tmp = std::env::temp_dir().join(format!("xk-ro-{}", std::process::id()));
        let dir = tmp.join("readonly");
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        let path = dir.join("p.log");
        let ring = Ring::new(100);
        let data = "line-1\nline-2\n";
        // open_append fails (read-only dir) → drain_only path.
        pump(
            std::io::Cursor::new(data.as_bytes().to_vec()),
            ring.clone(),
            path.clone(),
            None,
            2,
        );
        assert_eq!(
            ring.tail(10),
            vec!["line-1", "line-2"],
            "output reaches the ring"
        );
        assert!(!path.exists(), "no file may appear in the read-only dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn rotation_shifts_files() {
        let tmp = std::env::temp_dir().join(format!("xk-rot-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let p = tmp.join("t.log");
        std::fs::write(&p, "current").unwrap();
        std::fs::write(numbered(&p, 1), "one").unwrap();
        std::fs::write(numbered(&p, 2), "two").unwrap();
        rotate(&p, 2).unwrap();
        assert_eq!(std::fs::read_to_string(numbered(&p, 1)).unwrap(), "current");
        assert_eq!(std::fs::read_to_string(numbered(&p, 2)).unwrap(), "one");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
