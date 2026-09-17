//! The measurement report: one data structure serialized two ways — the
//! machine-readable JSON (design D8 schema) and the human-readable table for
//! stdout (D8: the table is a projection of the same data).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ProgramMetrics {
    pub name: String,
    pub rows: u64,
    pub bytes: u64,
    pub out_rows: u64,
    pub err_rows: u64,
    pub rows_per_sec: f64,
    pub bytes_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Aggregate {
    pub rows: u64,
    pub bytes: u64,
    pub rows_per_sec: f64,
    pub bytes_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntegrityDoc {
    pub pass: bool,
    pub expected_rows: u64,
    pub found_rows: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// Sampled daemon RSS (design D5): peak/avg are sampling-based, not exact.
#[derive(Debug, Clone, Serialize)]
pub struct RssDoc {
    pub peak_kib: u64,
    pub avg_kib: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub case: String,
    pub params: serde_json::Value,
    pub programs: Vec<ProgramMetrics>,
    pub aggregate: Aggregate,
    pub wall_time_secs: f64,
    pub rotation_count: u64,
    /// null when RSS sampling is unavailable (Windows, connect mode).
    pub daemon_rss: Option<RssDoc>,
    pub integrity: IntegrityDoc,
    /// Unix epoch seconds when the measurement phase started.
    pub started_at: u64,
    pub bin_versions: BinVersions,
}

#[derive(Debug, Clone, Serialize)]
pub struct BinVersions {
    pub xkeeper: String,
    pub bench: String,
}

pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Report {
    pub fn write_json(&self, path: &Path) -> anyhow::Result<()> {
        let data = serde_json::to_vec_pretty(self).context("serialize report")?;
        std::fs::write(path, data)
            .with_context(|| format!("write JSON report to {}", path.display()))?;
        Ok(())
    }
}

fn fmt_rate(v: f64) -> String {
    if v >= 1024.0 * 1024.0 {
        format!("{:.2} M", v / 1024.0 / 1024.0)
    } else if v >= 1024.0 {
        format!("{:.1} K", v / 1024.0)
    } else {
        format!("{v:.1}")
    }
}

/// The stdout rendering (design: diagnostics/progress go to stderr, this is
/// the only stdout output besides nothing else).
/// One-line projection of the effective parameters for the human table
/// (the same fields the JSON report carries under `params`). Fixed key
/// order so the table reads the same on every run.
fn render_params(p: &serde_json::Value) -> String {
    const KEYS: [&str; 7] = [
        "log_rows",
        "log_row_size",
        "log_total_size",
        "rate",
        "duration",
        "programs",
        "connect",
    ];
    KEYS
        .iter()
        .filter_map(|k| p.get(k).map(|v| format!("{k}={v}")))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn render_table(r: &Report) -> String {
    let mut s = String::new();
    s.push_str(&format!("== xkeeper-bench report: {} ==\n", r.case));
    s.push_str(&format!("  case            : {}\n", r.case));
    s.push_str(&format!("  params          : {}\n", render_params(&r.params)));
    s.push_str(&format!(
        "  rows / bytes    : {} / {}\n",
        r.aggregate.rows, r.aggregate.bytes
    ));
    s.push_str(&format!(
        "  rows/s          : {}\n",
        fmt_rate(r.aggregate.rows_per_sec)
    ));
    s.push_str(&format!(
        "  bytes/s         : {}\n",
        fmt_rate(r.aggregate.bytes_per_sec)
    ));
    s.push_str(&format!("  wall time       : {:.2} s\n", r.wall_time_secs));
    s.push_str(&format!("  rotations       : {}\n", r.rotation_count));
    s.push_str(&format!(
        "  daemon rss      : {}\n",
        match &r.daemon_rss {
            Some(v) => format!("peak {} KiB / avg {} KiB (sampled)", v.peak_kib, v.avg_kib),
            None => "n/a (no external sampling available)".to_string(),
        }
    ));
    s.push_str(&format!(
        "  integrity       : {} (expected {} rows, found {})\n",
        if r.integrity.pass { "PASS" } else { "FAIL" },
        r.integrity.expected_rows,
        r.integrity.found_rows
    ));
    for p in &r.integrity.problems {
        s.push_str(&format!("    - {p}\n"));
    }
    s.push_str(&format!(
        "  versions        : xkeeper {} / bench {}\n",
        r.bin_versions.xkeeper, r.bin_versions.bench
    ));
    s.push_str("\n  programs:\n");
    s.push_str(&format!(
        "  {:<28} {:>12} {:>14} {:>12} {:>14} {:>10} {:>10}\n",
        "name", "rows", "bytes", "rows/s", "bytes/s", "out", "err"
    ));
    for p in &r.programs {
        s.push_str(&format!(
            "  {:<28} {:>12} {:>14} {:>12} {:>14} {:>10} {:>10}\n",
            p.name,
            p.rows,
            p.bytes,
            fmt_rate(p.rows_per_sec),
            fmt_rate(p.bytes_per_sec),
            p.out_rows,
            p.err_rows
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> Report {
        Report {
            case: "firehose".into(),
            params: serde_json::json!({
                "log_rows": 10000, "log_row_size": 128, "log_total_size": 0,
                "rate": null, "duration": 30, "programs": 1, "connect": null
            }),
            programs: vec![ProgramMetrics {
                name: "xkeeper-bench-firehose".into(),
                rows: 10000,
                bytes: 1_290_000,
                out_rows: 10000,
                err_rows: 0,
                rows_per_sec: 5234.1,
                bytes_per_sec: 675_201.3,
            }],
            aggregate: Aggregate {
                rows: 10000,
                bytes: 1_290_000,
                rows_per_sec: 5234.1,
                bytes_per_sec: 675_201.3,
            },
            wall_time_secs: 1.91,
            rotation_count: 0,
            daemon_rss: Some(RssDoc { peak_kib: 8123, avg_kib: 7900 }),
            integrity: IntegrityDoc {
                pass: true,
                expected_rows: 10000,
                found_rows: 10000,
                problems: vec![],
            },
            started_at: 1_700_000_000,
            bin_versions: BinVersions {
                xkeeper: "0.1.0".into(),
                bench: env!("CARGO_PKG_VERSION").into(),
            },
        }
    }

    #[test]
    fn json_schema_has_the_d8_fields() {
        let r = sample_report();
        let v = serde_json::to_value(&r).unwrap();
        for key in [
            "case", "params", "programs", "aggregate", "wall_time_secs",
            "rotation_count", "daemon_rss", "integrity", "started_at", "bin_versions",
        ] {
            assert!(v.get(key).is_some(), "schema field {key} missing");
        }
        assert_eq!(v["aggregate"]["rows_per_sec"], 5234.1);
        assert_eq!(v["programs"][0]["name"], "xkeeper-bench-firehose");
        assert_eq!(v["integrity"]["pass"], true);
        assert_eq!(v["bin_versions"]["bench"], env!("CARGO_PKG_VERSION"));
        // null-able rss serializes as null when absent
        let mut no_rss = r.clone();
        no_rss.daemon_rss = None;
        let v = serde_json::to_value(&no_rss).unwrap();
        assert!(v["daemon_rss"].is_null());
    }

    #[test]
    fn integrity_problems_are_listed_in_json_when_present() {
        let mut r = sample_report();
        r.integrity.pass = false;
        r.integrity.problems.push("gap at row 41".into());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["integrity"]["pass"], false);
        assert_eq!(v["integrity"]["problems"][0], "gap at row 41");
        // and omitted entirely when clean
        let v = serde_json::to_value(&sample_report()).unwrap();
        assert!(v["integrity"].get("problems").is_none());
    }

    #[test]
    fn table_is_stdout_clean_and_names_the_key_numbers() {
        let text = render_table(&sample_report());
        assert!(text.contains("== xkeeper-bench report: firehose =="));
        // The effective parameters line (spec: the table carries the params).
        assert!(
            text.contains(
                "params          : log_rows=10000, log_row_size=128, log_total_size=0, \
                 rate=null, duration=30, programs=1, connect=null"
            ),
            "{text}"
        );
        assert!(text.contains("rows/s"), "{text}");
        assert!(text.contains("bytes/s"));
        assert!(text.contains("wall time"));
        assert!(text.contains("rotations"));
        assert!(text.contains("peak 8123 KiB / avg 7900 KiB"));
        assert!(text.contains("integrity       : PASS"));
        assert!(text.contains("xkeeper-bench-firehose"));
        assert!(text.contains("0.1.0"));
        // progress noise would use carriage returns / live refresh markers;
        // the table must be a plain multi-line block.
        assert!(!text.contains('\r'));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn failed_integrity_says_fail_and_lists_problems() {
        let mut r = sample_report();
        r.integrity.pass = false;
        r.integrity.problems.push("row count mismatch: expected 7 rows on disk, found 5".into());
        let text = render_table(&r);
        assert!(text.contains("FAIL"));
        assert!(text.contains("row count mismatch"));
    }

    #[test]
    fn missing_rss_renders_as_na() {
        let mut r = sample_report();
        r.daemon_rss = None;
        let text = render_table(&r);
        assert!(text.contains("n/a"));
    }

    #[test]
    fn json_file_round_trips() {
        let r = sample_report();
        let dir = std::env::temp_dir().join(format!("xk-bench-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("report.json");
        r.write_json(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["case"], "firehose");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
