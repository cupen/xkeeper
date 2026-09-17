//! On-disk log reconciliation (design D6): walk the live file plus all
//! rotation files in rotation order and verify the embedded per-stream seqs
//! are contiguous from 0 — no gaps, no duplicates, no disorder.

use std::path::{Path, PathBuf};


#[derive(Debug, Clone, PartialEq)]
pub struct StreamCheck {
    pub expected: u64,
    pub found: u64,
    pub pass: bool,
    pub problems: Vec<String>,
}

/// `live` is `<name>.<stream>.log`; rotation files are `live.1`, `live.2`, ...
/// (pump shifts live -> .1 -> ... -> .keep). Rotation order is oldest first,
/// i.e. the HIGHEST suffix first, then down to the live file.
pub fn rotation_chain(live: &Path) -> Vec<PathBuf> {
    let suffix = live
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut rotated: Vec<(u32, PathBuf)> = Vec::new();
    if let Some(parent) = live.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(num) = name.strip_prefix(&suffix) {
                    if let Some(n) = num.strip_prefix('.') {
                        if let Ok(n) = n.parse::<u32>() {
                            rotated.push((n, e.path()));
                        }
                    }
                }
            }
        }
    }
    rotated.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    let mut chain = rotated.into_iter().map(|(_, p)| p).collect::<Vec<_>>();
    chain.push(live.to_path_buf());
    chain
}

/// Number of rotation files that exist for this stream (the report's
/// rotation_count sums this over all programs and streams).
pub fn count_rotated(live: &Path) -> u64 {
    let chain_len = rotation_chain(live).len();
    if chain_len == 0 {
        0
    } else {
        (chain_len - 1) as u64
    }
}

fn check_stream(live: &Path, tag: &str, expected: u64) -> StreamCheck {
    let mut found = 0u64;
    let mut next = 0u64;
    let mut problems = Vec::new();
    for path in rotation_chain(live) {
        let Ok(bytes) = std::fs::read(&path) else {
            // A missing live file with expected rows is a real problem; with
            // zero expected rows it is fine (nothing was ever written).
            if expected > 0 {
                problems.push(format!("cannot read {}: expected rows exist", path.display()));
            }
            continue;
        };
        let mut lines = bytes.split(|&b| b == b'\n').peekable();
        while let Some(raw) = lines.next() {
            // A trailing newline yields one empty tail element.
            if raw.is_empty() && lines.peek().is_none() {
                continue;
            }
            found += 1;
            let line = String::from_utf8_lossy(raw);
            match crate::generate::parse_seq(&line, tag) {
                Some(seq) => {
                    if seq != next {
                        problems.push(format!(
                            "{} line {} (seq {seq}): expected {next} (gap/duplicate/disorder)",
                            path.display(),
                            found
                        ));
                        // resync to keep checking the rest of the stream
                        next = seq + 1;
                    } else {
                        next += 1;
                    }
                }
                None => problems.push(format!(
                    "{} line {}: not a [{tag}] seq line: {:?}",
                    path.display(),
                    found,
                    line.chars().take(60).collect::<String>()
                )),
            }
        }
    }
    if expected != found {
        problems.push(format!(
            "row count mismatch: expected {expected} rows on disk, found {found}"
        ));
    }
    StreamCheck {
        expected,
        found,
        pass: problems.is_empty(),
        problems,
    }
}

/// Verify both streams of one program.
/// Returns (out_check, err_check).
pub fn verify_program(log_dir: &Path, name: &str, expected_out: u64, expected_err: u64) -> (StreamCheck, StreamCheck) {
    (
        check_stream(&log_dir.join(format!("{name}.out.log")), "out", expected_out),
        check_stream(&log_dir.join(format!("{name}.err.log")), "err", expected_err),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn cleanup(d: &Path) {
        let _ = fs::remove_dir_all(d);
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "xk-bench-int-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn line(tag: &str, seq: u64) -> String {
        crate::generate::render_line(tag, seq, 1000, 64).unwrap()
    }

    #[test]
    fn contiguous_single_file_passes() {
        let d = tmpdir("ok");
        let live = d.join("p.out.log");
        let text: String = (0..10).map(|i| format!("{}\n", line("out", i))).collect();
        fs::write(&live, text).unwrap();
        let c = check_stream(&live, "out", 10);
        assert!(c.pass, "{:?}", c.problems);
        assert_eq!(c.found, 10);
        cleanup(&d);
    }

    #[test]
    fn rotation_chain_is_read_oldest_first() {
        let d = tmpdir("rot");
        let live = d.join("p.out.log");
        // pump shifts live -> .1 -> .2: .2 holds the oldest rows
        fs::write(d.join("p.out.log.2"), {
            let mut s = String::new();
            for i in 0..5 {
                s.push_str(&format!("{}\n", line("out", i)));
            }
            s
        })
        .unwrap();
        fs::write(d.join("p.out.log.1"), {
            let mut s = String::new();
            for i in 5..10 {
                s.push_str(&format!("{}\n", line("out", i)));
            }
            s
        })
        .unwrap();
        fs::write(&live, {
            let mut s = String::new();
            for i in 10..12 {
                s.push_str(&format!("{}\n", line("out", i)));
            }
            s
        })
        .unwrap();
        let chain = rotation_chain(&live);
        assert_eq!(chain.len(), 3);
        assert!(chain[0].to_string_lossy().ends_with(".2"));
        assert!(chain[1].to_string_lossy().ends_with(".1"));
        let c = check_stream(&live, "out", 12);
        assert!(c.pass, "{:?}", c.problems);
        assert_eq!(c.found, 12);
        assert_eq!(count_rotated(&live), 2);
        cleanup(&d);
    }

    #[test]
    fn gap_dup_and_disorder_fail() {
        let d = tmpdir("gap");
        let live = d.join("p.out.log");
        fs::write(
            &live,
            format!("{}\n{}\n", line("out", 0), line("out", 2)), // gap at 1
        )
        .unwrap();
        let c = check_stream(&live, "out", 2);
        assert!(!c.pass);
        cleanup(&d);

        let d = tmpdir("dup");
        let live = d.join("p.out.log");
        fs::write(
            &live,
            format!("{}\n{}\n", line("out", 0), line("out", 0)), // duplicate
        )
        .unwrap();
        assert!(!check_stream(&live, "out", 2).pass);
        cleanup(&d);

        let d = tmpdir("disorder");
        let live = d.join("p.out.log");
        fs::write(
            &live,
            format!("{}\n{}\n", line("out", 1), line("out", 0)), // disorder
        )
        .unwrap();
        assert!(!check_stream(&live, "out", 2).pass);
        cleanup(&d);
    }

    #[test]
    fn wrong_tag_or_foreign_lines_fail() {
        let d = tmpdir("tag");
        let live = d.join("p.out.log");
        fs::write(&live, format!("{}\n", line("err", 0))).unwrap(); // err line in out file
        let c = check_stream(&live, "out", 1);
        assert!(!c.pass);
        cleanup(&d);
    }

    #[test]
    fn count_mismatch_fails_even_when_seqs_look_fine() {
        let d = tmpdir("count");
        let live = d.join("p.out.log");
        let text: String = (0..5).map(|i| format!("{}\n", line("out", i))).collect();
        fs::write(&live, text).unwrap();
        let c = check_stream(&live, "out", 7);
        assert!(!c.pass);
        assert_eq!(c.found, 5);
        assert_eq!(c.expected, 7);
        cleanup(&d);
    }

    #[test]
    fn missing_file_with_zero_expected_is_a_pass() {
        let d = tmpdir("empty");
        let c = check_stream(&d.join("nothing.out.log"), "out", 0);
        assert!(c.pass);
        cleanup(&d);
    }

    #[test]
    fn non_numeric_suffixes_are_not_chain_members() {
        // foreign suffixes next to the live file (e.g. editor backups) must
        // not be pulled into the rotation order or break reconciliation
        let d = tmpdir("sfx");
        let live = d.join("p.out.log");
        fs::write(d.join("p.out.log.bak"), "not a bench line\n").unwrap();
        fs::write(&live, format!("{}\n", line("out", 0))).unwrap();
        assert_eq!(count_rotated(&live), 0);
        let c = check_stream(&live, "out", 1);
        assert!(c.pass, "{:?}", c.problems);
        cleanup(&d);
    }
}
