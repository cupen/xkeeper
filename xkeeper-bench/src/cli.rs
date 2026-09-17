//! CLI definition, case enum and parameter validation.
//!
//! The user-facing surface is flag-driven (`--case firehose ...`); the only
//! subcommand is the hidden `__generate` self-reentry mode used by the bench
//! to run as a daemon-supervised load program (design D2).

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, Subcommand, ValueEnum};

/// The `[k] <seq> <ts> ` prefix must fit plus some fill text: worst case is
/// 6 + 20 (u64 seq) + 1 + 13 (ms timestamp) + 1 = 41 bytes; 48 leaves room.
pub const MIN_ROW_SIZE: u64 = 48;

/// Drip's built-in steady-state rate (rows/sec) when `--rate` is not given.
pub const DRIP_DEFAULT_RATE: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Case {
    Firehose,
    Rotation,
    Fanout,
    Drip,
}

impl Case {
    pub fn as_str(self) -> &'static str {
        match self {
            Case::Firehose => "firehose",
            Case::Rotation => "rotation",
            Case::Fanout => "fanout",
            Case::Drip => "drip",
        }
    }
}

/// User-facing benchmark invocation.
#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Load case to measure: firehose | rotation | fanout | drip.
    #[arg(long, value_enum)]
    pub case: Option<Case>,

    /// Total rows produced per load program (0 or absent = unlimited).
    #[arg(long, default_value_t = 0)]
    pub log_rows: u64,

    /// Payload bytes of one log line, newline excluded.
    #[arg(long, default_value_t = 128)]
    pub log_row_size: u64,

    /// Aggregate byte ceiling over all load programs, first limit wins (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub log_total_size: u64,

    /// Production rate in rows/sec per program (0 = full speed; drip defaults to 100 when absent).
    #[arg(long)]
    pub rate: Option<u64>,

    /// Wall-clock ceiling in seconds for the load phase (0 = unlimited).
    #[arg(long, default_value_t = 30)]
    pub duration: u64,

    /// Number of parallel load programs (consumed by the fanout case only).
    #[arg(long, default_value_t = 4)]
    pub programs: u32,

    /// Connect to an already-running daemon at HOST:PORT instead of spawning
    /// an isolated one. The daemon must be on the same host (bench reads its
    /// app registry and log files from disk). NOTE: the load writes real disk
    /// logs through the target daemon.
    #[arg(long)]
    pub connect: Option<String>,

    /// Bearer token for the daemon control plane (use with --connect).
    #[arg(long)]
    pub token: Option<String>,

    /// Path of the xkeeper daemon binary (default: discovered next to this
    /// binary, then target/{debug,release} of this workspace).
    #[arg(long)]
    pub daemon: Option<PathBuf>,

    /// Also write the machine-readable report to this JSON file.
    #[arg(long)]
    pub json: Option<PathBuf>,

    /// Keep the measurement site for inspection: spawn mode keeps the temp
    /// workspace, connect mode keeps the bench log files. The bench app is
    /// always unregistered from the daemon.
    #[arg(long)]
    pub keep: bool,
}

/// Hidden self-reentry generator: `xkeeper-bench __generate ...` runs as a
/// supervised daemon child writing paced log lines to stdout/stderr.
#[derive(Debug, Args)]
pub struct GenerateArgs {
    /// Rows per second (0 or absent = full speed).
    #[arg(long)]
    pub rate: Option<f64>,

    /// Total rows to produce (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub rows: u64,

    /// Payload bytes of one line, newline excluded.
    #[arg(long)]
    pub row_size: u64,

    /// Total produced bytes ceiling (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub total_size: u64,

    /// Wall-clock ceiling in seconds (0 = unlimited).
    #[arg(long, default_value_t = 0.0)]
    pub duration: f64,

    /// Every Nth row goes to stderr (0 = never; fanout uses 10).
    #[arg(long, default_value_t = 0)]
    pub stderr_every: u32,

    /// Where to write the produced-rows/bytes counters as JSON on exit.
    #[arg(long)]
    pub count_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Hidden self-reentry load generator (not a user-facing case).
    #[command(name = "__generate", hide = true)]
    Generate(GenerateArgs),
}

#[derive(Debug, clap::Parser)]
#[command(
    name = "xkeeper-bench",
    version,
    about = "Load benchmark harness for a real xkeeper daemon (cases: firehose, rotation, fanout, drip)",
    after_help = "Exit code reflects run success only; compare two JSON reports yourself for regressions."
)]
pub struct Cli {
    #[command(flatten)]
    pub bench: BenchArgs,

    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

/// Parameters after case-specific resolution (drip default rate, programs
/// consumed only by fanout, ...). This is what the report documents.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub case: Case,
    pub rows_per_program: u64,
    pub row_size: u64,
    pub total_all: u64,
    /// None = full speed.
    pub rate: Option<u64>,
    pub duration: u64,
    pub programs: u32,
}

impl Effective {
    /// Per-program byte budget implied by `--log-total-size` (0 = unlimited).
    pub fn total_per_program(&self) -> u64 {
        if self.total_all == 0 {
            0
        } else {
            (self.total_all / self.programs as u64).max(1)
        }
    }
}

pub fn resolve(a: &BenchArgs) -> Result<Effective> {
    let case = a
        .case
        .ok_or_else(|| anyhow::anyhow!("--case is required (firehose | rotation | fanout | drip)"))?;
    if a.log_row_size < MIN_ROW_SIZE {
        bail!(
            "--log-row-size must be >= {MIN_ROW_SIZE} (the [stream] seq timestamp prefix must fit), got {}",
            a.log_row_size
        );
    }
    if a.programs == 0 {
        bail!("--programs must be >= 1");
    }
    if a.log_rows == 0 && a.log_total_size == 0 && a.duration == 0 {
        bail!(
            "no load bound given: set --log-rows, --log-total-size or a non-zero --duration \
             (the load would never end)"
        );
    }
    let programs = match case {
        Case::Fanout => a.programs,
        // firehose/rotation/drip are single-program cases; --programs is ignored.
        _ => 1,
    };
    let rate = match case {
        // drip defaults to its built-in steady rate; an explicit --rate
        // overrides it. Other cases: absent = full speed.
        Case::Drip => Some(a.rate.unwrap_or(DRIP_DEFAULT_RATE)),
        _ => a.rate,
    };
    // Normalize: 0 rows/s means full speed everywhere.
    let rate = match rate {
        Some(0) => None,
        other => other,
    };
    Ok(Effective {
        case,
        rows_per_program: a.log_rows,
        row_size: a.log_row_size,
        total_all: a.log_total_size,
        rate,
        duration: a.duration,
        programs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Effective> {
        let cli = Cli::try_parse_from(std::iter::once("xkeeper-bench").chain(args.iter().copied()))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        resolve(&cli.bench)
    }

    #[test]
    fn defaults_are_documented() {
        let e = parse(&["--case", "firehose"]).unwrap();
        assert_eq!(e.case, Case::Firehose);
        assert_eq!(e.rows_per_program, 0);
        assert_eq!(e.row_size, 128);
        assert_eq!(e.total_all, 0);
        assert_eq!(e.rate, None, "firehose defaults to full speed");
        assert_eq!(e.duration, 30);
        assert_eq!(e.programs, 1, "non-fanout cases are single-program");
    }

    #[test]
    fn drip_defaults_to_builtin_rate_but_zero_overrides() {
        let e = parse(&["--case", "drip"]).unwrap();
        assert_eq!(e.rate, Some(DRIP_DEFAULT_RATE), "drip default rate is built in");
        let e = parse(&["--case", "drip", "--rate", "0"]).unwrap();
        assert_eq!(e.rate, None, "explicit --rate 0 means full speed");
        let e = parse(&["--case", "drip", "--rate", "42"]).unwrap();
        assert_eq!(e.rate, Some(42));
    }

    #[test]
    fn fanout_consumes_programs() {
        let e = parse(&["--case", "fanout", "--programs", "7"]).unwrap();
        assert_eq!(e.programs, 7);
    }

    #[test]
    fn unknown_case_prints_supported_list_and_fails() {
        let err = Cli::try_parse_from(["xkeeper-bench", "--case", "turbo"]).unwrap_err();
        let text = format!("{err}");
        for c in ["firehose", "rotation", "fanout", "drip"] {
            assert!(text.contains(c), "supported list must mention {c}: {text}");
        }
    }

    #[test]
    fn missing_case_is_rejected() {
        assert!(parse(&[]).is_err(), "case is required in bench mode");
    }

    #[test]
    fn negative_and_non_numeric_values_are_rejected() {
        assert!(Cli::try_parse_from(["xkeeper-bench", "--case", "firehose", "--log-rows", "-5"])
            .is_err());
        assert!(Cli::try_parse_from(["xkeeper-bench", "--case", "firehose", "--log-rows", "abc"])
            .is_err());
        assert!(
            Cli::try_parse_from(["xkeeper-bench", "--case", "firehose", "--log-row-size", "-1"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["xkeeper-bench", "--case", "firehose", "--programs", "x"])
            .is_err());
    }

    #[test]
    fn zero_programs_is_rejected() {
        for case in ["fanout", "firehose"] {
            let err = parse(&["--case", case, "--programs", "0"]).unwrap_err();
            assert!(format!("{err:#}").contains("--programs"), "{err:#}");
        }
    }

    #[test]
    fn tiny_row_size_is_rejected() {
        let err = parse(&["--case", "firehose", "--log-row-size", "10"]).unwrap_err();
        assert!(format!("{err:#}").contains("--log-row-size"));
    }

    #[test]
    fn unbounded_load_is_rejected() {
        let err =
            parse(&["--case", "firehose", "--duration", "0"]).unwrap_err();
        assert!(format!("{err:#}").contains("no load bound"));
    }

    #[test]
    fn total_size_splits_across_programs() {
        let e = parse(&["--case", "fanout", "--programs", "4", "--log-total-size", "1000"])
            .unwrap();
        assert_eq!(e.total_per_program(), 250);
        let e = parse(&["--case", "fanout", "--programs", "4", "--log-total-size", "3"]).unwrap();
        assert_eq!(e.total_per_program(), 1, "tiny budgets stay positive");
        let e = parse(&["--case", "firehose"]).unwrap();
        assert_eq!(e.total_per_program(), 0, "0 means unlimited");
    }

    #[test]
    fn generate_mode_is_hidden_but_parses() {
        let cli = Cli::try_parse_from([
            "xkeeper-bench",
            "__generate",
            "--row-size",
            "64",
            "--rows",
            "10",
        ])
        .unwrap();
        match cli.cmd {
            Some(Cmd::Generate(g)) => {
                assert_eq!(g.row_size, 64);
                assert_eq!(g.rows, 10);
                assert_eq!(g.stderr_every, 0);
                assert_eq!(g.rate, None);
            }
            _ => panic!("__generate must parse into Cmd::Generate"),
        }
    }
}
