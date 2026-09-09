//! xtask — developer tasks for xkeeper.
//!
//! - `cargo run -p xtask -- test`   full verification suite (fmt + backend
//!   tests + frontend typecheck/tests/build + rebuild)
//! - `cargo run -p xtask -- stress` firehose end-to-end against the REAL
//!   daemon binary: ~10MB/s paced generator, a live WS subscriber, and the
//!   zero-backpressure / gap-marker / disk-integrity verification
//!   (log-management 输出排空零反压, webui-api WS 日志批量推送与背压).
//!
//! The stress harness lives here (not in `cargo test`) because it drives a
//! real daemon process, produces multi-GB log streams and takes ~30s — the
//! fast correctness tests stay in `cargo test`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};

mod stress;

#[derive(clap::Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Run the full verification suite (fmt, backend tests, frontend
    /// typecheck/tests/build, rebuild).
    Test,
    /// Firehose stress + zero-backpressure verification against a real daemon.
    Stress {
        /// Seconds of full-speed consumption (phase 1).
        #[arg(long, default_value_t = 3)]
        secs: u64,
        /// Seconds of pace-limited consumption (phase 2, ~0.6MB/s).
        #[arg(long, default_value_t = 6)]
        slow_secs: u64,
        /// Seconds of catch-up drain after the slow phase.
        #[arg(long, default_value_t = 3)]
        catchup_secs: u64,
        /// Keep the temp workspace (logs, daemon config) for inspection.
        #[arg(long)]
        keep: bool,
    },
}

fn main() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    match cli.cmd {
        Cmd::Test => test(),
        Cmd::Stress {
            secs,
            slow_secs,
            catchup_secs,
            keep,
        } => stress::run(stress::Args {
            secs,
            slow_secs,
            catchup_secs,
            keep,
        }),
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the repo")
        .to_path_buf()
}

fn run_step(name: &str, dir: &Path, program: &str, args: &[&str]) -> Result<()> {
    println!("==> {name}");
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .with_context(|| format!("failed to spawn {program} (is it on PATH?)"))?;
    if !status.success() {
        bail!("{name} failed with {status}");
    }
    Ok(())
}

fn test() -> Result<()> {
    let root = repo_root();
    let webui = root.join("webui");
    let started = Instant::now();

    run_step("cargo fmt --check", &root, "cargo", &["fmt", "--check"])?;
    run_step("cargo test", &root, "cargo", &["test"])?;
    run_step(
        "webui tsc --noEmit",
        &webui,
        "pnpm",
        &["exec", "tsc", "--noEmit"],
    )?;
    run_step("webui test", &webui, "pnpm", &["test"])?;
    run_step("webui build", &webui, "pnpm", &["build"])?;
    // Rebuild so the binary embeds the fresh dist.
    run_step("cargo build (re-embed)", &root, "cargo", &["build"])?;

    println!(
        "\nAll checks passed in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Quiet cargo build of the daemon binary; returns its path.
pub(crate) fn build_daemon() -> Result<PathBuf> {
    let root = repo_root();
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .current_dir(&root)
        .stdout(Stdio::null())
        .status()
        .context("failed to run cargo build")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    Ok(root.join("target/debug/xkeeper"))
}
