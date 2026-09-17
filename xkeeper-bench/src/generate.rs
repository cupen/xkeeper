//! The load generator: bench self-reentry as a daemon-supervised child
//! (design D2). Writes paced, self-describing log lines to stdout/stderr.
//!
//! Line contract (design D6): `[k] <seq> <ts> <fill>` where `k` is the stream
//! tag (`out`/`err`), `<seq>` is monotonic within its (program, stream) pair,
//! `<ts>` is the wall-clock millisecond, and the whole payload is exactly
//! `--row-size` bytes (newline excluded) so every row reconciles on disk.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

/// Fill text cycled through the payload: wordy ASCII, no newlines.
pub const FILL: &str = "xkeeper bench load generator fills every line with realistic text so each row can be reconciled on disk independently ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

impl Stream {
    pub fn tag(self) -> &'static str {
        match self {
            Stream::Out => "out",
            Stream::Err => "err",
        }
    }
}

/// fanout split (design D2): every Nth row (1-based) goes to stderr, the rest
/// to stdout. `stderr_every == 0` never splits.
pub fn decide_stream(row_index: u64, stderr_every: u32) -> Stream {
    let n = stderr_every as u64;
    if n > 0 && row_index % n == n - 1 {
        Stream::Err
    } else {
        Stream::Out
    }
}

/// Render one line with an exact payload width. `None` when the prefix does
/// not fit `row_size` (the caller turns this into a hard error).
pub fn render_line(tag: &str, seq: u64, ts_ms: u64, row_size: usize) -> Option<String> {
    let prefix = format!("[{tag}] {seq} {ts_ms} ");
    if prefix.len() >= row_size {
        return None;
    }
    let mut line = String::with_capacity(row_size);
    line.push_str(&prefix);
    let fill = FILL.as_bytes();
    let mut i = 0;
    while line.len() < row_size {
        line.push(fill[i % fill.len()] as char);
        i += 1;
    }
    Some(line)
}

/// Parse the embedded seq back out of a disk line (integrity reconciliation).
pub fn parse_seq(line: &str, tag: &str) -> Option<u64> {
    let rest = line.strip_prefix('[')?;
    let rest = rest.strip_prefix(tag)?;
    let rest = rest.strip_prefix("] ")?;
    let seq_end = rest.find(' ')?;
    rest[..seq_end].parse().ok()
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct GenCounters {
    pub rows: u64,
    pub out_rows: u64,
    pub err_rows: u64,
    pub bytes: u64,
    pub out_bytes: u64,
    pub err_bytes: u64,
}

pub struct GenLimits {
    /// 0 = unlimited.
    pub rows: u64,
    /// 0 = unlimited.
    pub total_size: u64,
    /// None = unlimited.
    pub duration: Option<Duration>,
    /// None or Some(0) = full speed.
    pub rate: Option<f64>,
}

/// Produce lines until the first bound trips (rows / total bytes / duration).
/// Pacing uses an absolute schedule (line n is due at start + n/rate) so the
/// rate does not drift with per-line overhead. The wall-clock bound is checked
/// against the NEXT line's scheduled time, so a slow pace never overshoots the
/// deadline by sleeping.
pub fn run(
    limits: &GenLimits,
    stderr_every: u32,
    row_size: usize,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<GenCounters> {
    let mut c = GenCounters::default();
    let start = Instant::now();
    loop {
        if limits.rows > 0 && c.rows >= limits.rows {
            break;
        }
        // "First limit wins": stop once the produced bytes reach the ceiling
        // (the line that crosses it is allowed: ceiling + one line of slack).
        if limits.total_size > 0 && c.bytes >= limits.total_size {
            break;
        }
        // Absolute-schedule pacing for the upcoming line.
        if let Some(r) = limits.rate {
            if r > 0.0 {
                let due = Duration::from_secs_f64((c.rows + 1) as f64 / r);
                // If the next line is scheduled past the wall clock, stop now
                // instead of sleeping into the deadline.
                if let Some(d) = limits.duration {
                    if due >= d {
                        break;
                    }
                }
                let target = start + due;
                let now = Instant::now();
                if target > now {
                    std::thread::sleep(target - now);
                }
            }
        }
        if let Some(d) = limits.duration {
            if start.elapsed() >= d {
                break;
            }
        }
        let stream = decide_stream(c.rows, stderr_every);
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let line = render_line(stream.tag(), match stream {
            Stream::Out => c.out_rows,
            Stream::Err => c.err_rows,
        }, ts_ms, row_size)
        .ok_or_else(|| {
            anyhow::anyhow!("row size {row_size} is too small for the line prefix")
        })?;
        debug_assert_eq!(line.len(), row_size);

        let sink: &mut dyn Write = match stream {
            Stream::Out => &mut *out,
            Stream::Err => &mut *err,
        };
        sink.write_all(line.as_bytes())
            .and_then(|_| sink.write_all(b"\n"))
            .with_context(|| format!("writing load line {} to {stream:?}", c.rows))?;
        let produced = line.len() as u64 + 1;
        match stream {
            Stream::Out => {
                c.out_rows += 1;
                c.out_bytes += produced;
            }
            Stream::Err => {
                c.err_rows += 1;
                c.err_bytes += produced;
            }
        }
        c.rows += 1;
        c.bytes += produced;
    }
    out.flush().context("flush stdout")?;
    err.flush().context("flush stderr")?;
    Ok(c)
}

/// Write the produced counters as JSON (the "agreed channel" back to the
/// parent bench, design D8/5.1). Best effort: a failure only warns.
pub fn write_count_file(path: &Path, c: &GenCounters) {
    if let Err(e) = std::fs::write(
        path,
        serde_json::to_vec(c).unwrap_or_else(|_| b"{}".to_vec()),
    ) {
        eprintln!("xkeeper-bench: warning: cannot write count file {}: {e}", path.display());
    }
}

    /// Entry for the hidden `__generate` mode.
pub fn run_generate(args: &crate::cli::GenerateArgs) -> Result<()> {
    if args.row_size < crate::cli::MIN_ROW_SIZE {
        bail!("__generate --row-size must be >= {}", crate::cli::MIN_ROW_SIZE);
    }
    if args.rows == 0 && args.total_size == 0 && args.duration <= 0.0 {
        bail!("__generate with no bound would run forever (refusing)");
    }
    let limits = GenLimits {
        rows: args.rows,
        total_size: args.total_size,
        duration: (args.duration > 0.0).then(|| Duration::from_secs_f64(args.duration)),
        rate: args.rate,
    };
    let mut out = std::io::BufWriter::with_capacity(256 * 1024, std::io::stdout().lock());
    let mut err = std::io::BufWriter::with_capacity(64 * 1024, std::io::stderr().lock());
    let counters = run(&limits, args.stderr_every, args.row_size as usize, &mut out, &mut err)?;
    if let Some(p) = &args.count_file {
        write_count_file(p, &counters);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_lines_have_exact_width_and_prefix() {
        let line = render_line("out", 42, 1_700_000_000_123, 128).unwrap();
        assert_eq!(line.len(), 128);
        assert!(line.starts_with("[out] 42 1700000000123 "));
        // seq with many digits still renders
        let line = render_line("err", u64::MAX, 1, 96).unwrap();
        assert_eq!(line.len(), 96);
        assert!(line.starts_with("[err] 18446744073709551615 1 "));
    }

    #[test]
    fn too_small_row_size_is_reported() {
        assert!(render_line("out", 0, 0, 10).is_none());
    }

    #[test]
    fn fill_has_no_newlines_and_is_not_blank_only() {
        for size in [48usize, 64, 128, 200] {
            let line = render_line("out", 7, 5, size).unwrap();
            assert_eq!(line.len(), size);
            assert!(!line.contains('\n'));
            // the fill region must be real text, not blank padding
            assert!(
                line[22..].chars().any(|c| c.is_ascii_alphabetic()),
                "fill region is blank at size {size}: {line:?}"
            );
        }
    }

    #[test]
    fn seq_round_trips() {
        let line = render_line("err", 12345, 99, 64).unwrap();
        assert_eq!(parse_seq(&line, "err"), Some(12345));
        assert_eq!(parse_seq(&line, "out"), None, "tag mismatch is not parseable");
        assert_eq!(parse_seq("garbage", "out"), None);
        assert_eq!(parse_seq("[out] notanumber 1 x", "out"), None);
    }

    #[test]
    fn stderr_split_is_one_in_ten() {
        // every 10th row (1-based) goes to stderr
        assert_eq!(decide_stream(0, 10), Stream::Out);
        assert_eq!(decide_stream(8, 10), Stream::Out);
        assert_eq!(decide_stream(9, 10), Stream::Err);
        assert_eq!(decide_stream(19, 10), Stream::Err);
        assert_eq!(decide_stream(9, 0), Stream::Out, "0 = never split");
    }

    fn counters(limits: &GenLimits, stderr_every: u32, row_size: usize) -> GenCounters {
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(limits, stderr_every, row_size, &mut out, &mut err).unwrap()
    }

    #[test]
    fn row_bound_stops_production() {
        let c = counters(
            &GenLimits { rows: 1000, total_size: 0, duration: None, rate: None },
            0,
            128,
        );
        assert_eq!(c.rows, 1000);
        assert_eq!(c.out_rows, 1000);
        assert_eq!(c.err_rows, 0);
        assert_eq!(c.bytes, 1000 * 129);
    }

    #[test]
    fn size_bound_stops_within_one_line_of_ceiling() {
        let c = counters(
            &GenLimits { rows: 0, total_size: 1000, duration: None, rate: None },
            0,
            100,
        );
        assert!(c.bytes >= 1000, "produces up to the ceiling");
        assert!(c.bytes < 1000 + 101, "at most one line of slack, got {}", c.bytes);
    }

    #[test]
    fn per_stream_seqs_are_monotonic_from_zero() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            &GenLimits { rows: 37, total_size: 0, duration: None, rate: None },
            10,
            64,
            &mut out,
            &mut err,
        )
        .unwrap();
        let out_text = String::from_utf8(out).unwrap();
        let err_text = String::from_utf8(err).unwrap();
        let out_seqs: Vec<u64> = out_text
            .lines()
            .map(|l| parse_seq(l, "out").expect("out line"))
            .collect();
        let err_seqs: Vec<u64> = err_text
            .lines()
            .map(|l| parse_seq(l, "err").expect("err line"))
            .collect();
        assert_eq!(out_seqs.len(), 34);
        assert_eq!(err_seqs.len(), 3, "3 of 37 rows land on stderr");
        assert_eq!(out_seqs, (0..34).collect::<Vec<_>>());
        assert_eq!(err_seqs, (0..3).collect::<Vec<_>>());
    }

    #[test]
    fn duration_bound_stops_and_never_overshoots() {
        // 50 Hz for 100ms: lines due every 20ms, next line lands at the wall —
        // roughly 5 rows, and the run finishes promptly.
        let t0 = Instant::now();
        let c = counters(
            &GenLimits {
                rows: 0,
                total_size: 0,
                duration: Some(Duration::from_millis(100)),
                rate: Some(50.0),
            },
            0,
            64,
        );
        let elapsed = t0.elapsed();
        assert!(
            (3..=7).contains(&c.rows),
            "50 Hz for 100ms yields ~5 rows, got {}",
            c.rows
        );
        assert!(
            elapsed < Duration::from_millis(600),
            "a slow pace must not sleep past the deadline, took {elapsed:?}"
        );
    }

    /// Full-speed throughput must not degrade because of the pacing logic.
    #[test]
    fn full_speed_throughput_is_not_degraded_by_pacing() {
        let t0 = Instant::now();
        let c = counters(
            &GenLimits { rows: 200_000, total_size: 0, duration: None, rate: None },
            0,
            128,
        );
        let secs = t0.elapsed().as_secs_f64();
        assert_eq!(c.rows, 200_000);
        // 25.8 MB of rendering + writes; a degraded loop would not manage 20 MB/s.
        assert!(
            c.bytes as f64 / secs > 20.0 * 1024.0 * 1024.0,
            "full-speed throughput {:.1} MB/s is too low",
            c.bytes as f64 / secs / 1048576.0
        );
    }

    /// Rate precision: 1s at 200 rows/s must land within ±20% (task 2.2).
    #[test]
    #[ignore = "real-time pacing check; run explicitly with --ignored"]
    fn paced_rate_is_within_twenty_percent() {
        let c = counters(
            &GenLimits {
                rows: 0,
                total_size: 0,
                duration: Some(Duration::from_secs(1)),
                rate: Some(200.0),
            },
            0,
            64,
        );
        assert!(
            (160..=240).contains(&c.rows),
            "expected ~200 rows in 1s at 200 rows/s, got {}",
            c.rows
        );
    }

    /// The hidden mode itself refuses to run without any bound and with a
    /// too-small row size (both guards run before anything is written).
    #[test]
    fn generate_mode_refuses_unbounded_and_tiny_row_size() {
        let unbounded = crate::cli::GenerateArgs {
            rate: Some(10.0),
            rows: 0,
            row_size: 64,
            total_size: 0,
            duration: 0.0,
            stderr_every: 0,
            count_file: None,
        };
        let err = run_generate(&unbounded).unwrap_err();
        assert!(format!("{err:#}").contains("forever"), "{err:#}");
        let tiny = crate::cli::GenerateArgs { row_size: 10, ..unbounded };
        let err = run_generate(&tiny).unwrap_err();
        assert!(format!("{err:#}").contains("row-size"), "{err:#}");
    }
}
