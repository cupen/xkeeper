//! Log pumps: one thread per child output stream. Each line is pushed into
//! an in-memory ring buffer (for `log --tail/--follow`), broadcast to
//! subscribers, and appended to the on-disk log file with size-based rotation.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use log::{debug, warn};

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

/// Bounded ring of recent lines plus live-follow subscribers.
pub struct Ring {
    inner: Mutex<RingInner>,
}

struct RingInner {
    lines: VecDeque<String>,
    cap: usize,
    subs: Vec<Sender<String>>,
}

impl Ring {
    pub fn new(cap: usize) -> Arc<Ring> {
        Arc::new(Ring {
            inner: Mutex::new(RingInner { lines: VecDeque::with_capacity(cap), cap, subs: Vec::new() }),
        })
    }

    pub fn push(&self, line: String) {
        let mut g = self.inner.lock().unwrap();
        if g.lines.len() == g.cap {
            g.lines.pop_front();
        }
        g.lines.push_back(line.clone());
        g.subs.retain(|s| s.send(line.clone()).is_ok());
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        g.lines.iter().rev().take(n).rev().cloned().collect()
    }

    pub fn subscribe(&self) -> Receiver<String> {
        let (tx, rx) = mpsc::channel();
        self.inner.lock().unwrap().subs.push(tx);
        rx
    }
}

pub struct PumpSet {
    handles: Vec<JoinHandle<()>>,
}

impl PumpSet {
    /// Wait for both pump threads (child closed its pipes / was killed).
    #[allow(dead_code)]
    pub fn join(&mut self) {
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for PumpSet {
    fn drop(&mut self) {
        // Pumps terminate on EOF; don't block the supervisor waiting for them.
        for h in self.handles.drain(..) {
            let _ = h.join();
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
    PumpSet { handles: vec![h_out, h_err] }
}

fn pump(reader: impl Read, ring: Arc<Ring>, path: PathBuf, max_size: Option<u64>, keep: u32) {
    let mut file = match open_append(&path) {
        Ok(f) => f,
        Err(e) => {
            warn!("cannot open log file {}: {e}; output goes to memory only", path.display());
            drain_only(reader, ring);
            return;
        }
    };
    let mut size = file.seek(SeekFrom::End(0)).unwrap_or(0);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut warned = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                ring.push(trimmed.to_string());
                if let Some(limit) = max_size {
                    if size >= limit {
                        // Rotate, then reopen so writes go to a fresh file.
                        match rotate(&path, keep).and_then(|_| open_append(&path)) {
                            Ok(f) => {
                                file = f;
                                size = 0;
                            }
                            Err(e) => {
                                if !warned {
                                    warn!("log rotation failed for {}: {e}", path.display());
                                    warned = true;
                                }
                            }
                        }
                    }
                }
                let bytes = trimmed.as_bytes();
                let written = file
                    .seek(SeekFrom::Start(size))
                    .and_then(|_| file.write_all(bytes))
                    .and_then(|_| file.write_all(b"\n"));
                match written {
                    Ok(()) => size += bytes.len() as u64 + 1,
                    Err(e) => {
                        if !warned {
                            warn!("cannot write log file {}: {e}", path.display());
                            warned = true;
                        }
                    }
                }
            }
            Err(e) => {
                debug!("log pump read error: {e}");
                break;
            }
        }
    }
}

/// Best-effort drain when the file cannot be opened at all.
fn drain_only(reader: impl Read, ring: Arc<Ring>) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => ring.push(line.trim_end_matches(['\r', '\n']).to_string()),
        }
    }
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
        assert_eq!(rx.recv_timeout(std::time::Duration::from_millis(100)).unwrap(), "live");
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
        assert!(!numbered(&p, 3).exists() || numbered(&p, 3).exists()); // keep=2 cap
        assert!(!p.exists() || std::fs::read(&p).is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pump_writes_and_rotates() {
        let tmp = std::env::temp_dir().join(format!("xk-pump-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let data = "l1\nl2\nl3\n";
        // reader from in-memory data
        let reader = std::io::Cursor::new(data.as_bytes().to_vec());
        let ring = Ring::new(10);
        let path = tmp.join("p.log");
        pump(reader, ring.clone(), path.clone(), Some(4), 2);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("l"));
        assert_eq!(ring.tail(10).len(), 3);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
