//! External daemon RSS sampling (design D5): the parent bench polls
//! `/proc/<pid>/status` at 1 Hz while the load runs. Windows has no light
//! cross-process equivalent, so it reports "no data" (the JSON field is
//! null) — never 0.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(unix)]
fn rss_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
}

/// Spawn the sampler; `None` where RSS sampling is unavailable (Windows) or
/// the pid is unknown (connect mode). The handle resolves to the sample list
/// once `stop` is set (or the process disappears).
#[cfg(unix)]
pub fn start(pid: u32, stop: Arc<AtomicBool>) -> Option<JoinHandle<Vec<u64>>> {
    std::thread::Builder::new()
        .name("bench-rss".into())
        .spawn(move || {
            let mut samples = Vec::new();
            let mut misses = 0u32;
            while !stop.load(Ordering::Relaxed) {
                match rss_kib(pid) {
                    Some(kib) => {
                        samples.push(kib);
                        misses = 0;
                    }
                    None => {
                        // process gone (or not linux-like): stop soon after
                        misses += 1;
                        if misses >= 3 {
                            break;
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            samples
        })
        .ok()
}

#[cfg(not(unix))]
pub fn start(_pid: u32, _stop: Arc<AtomicBool>) -> Option<JoinHandle<Vec<u64>>> {
    None // D5: no light cross-process RSS on Windows — report null
}

/// Sampled peak/average over the measurement window; `None` = no data.
pub fn summarize(samples: &[u64]) -> Option<(u64, u64)> {
    if samples.is_empty() {
        return None;
    }
    let sum: u128 = samples.iter().map(|&s| s as u128).sum();
    Some((
        *samples.iter().max().unwrap(),
        (sum / samples.len() as u128) as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_gives_peak_and_mean() {
        assert_eq!(summarize(&[]), None);
        assert_eq!(summarize(&[10]), Some((10, 10)));
        assert_eq!(summarize(&[10, 20, 30]), Some((30, 20)));
        assert_eq!(summarize(&[5, 7]), Some((7, 6)));
    }

    #[cfg(unix)]
    #[test]
    fn sampling_this_process_yields_data() {
        let stop = Arc::new(AtomicBool::new(false));
        let h = start(std::process::id(), stop.clone()).expect("sampler thread");
        std::thread::sleep(Duration::from_millis(1300));
        stop.store(true, Ordering::Relaxed);
        let samples = h.join().unwrap();
        assert!(!samples.is_empty(), "1.3s at 1 Hz must produce samples");
        assert!(samples.iter().all(|&s| s > 0));
        assert!(summarize(&samples).is_some());
    }
}
