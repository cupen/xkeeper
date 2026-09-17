//! Orchestration: topology setup → register + apply → wait running → wait
//! done → drain → reconcile → report → cleanup. Exit code semantics live in
//! main.rs; this module decides pass/fail of the measurement itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::cases::{self, ProgramPlan};
use crate::cli::{BenchArgs, Effective};
use crate::api::Client;
use crate::generate::GenCounters;
use crate::integrity;
use crate::report::{self, Aggregate, BinVersions, IntegrityDoc, ProgramMetrics, RssDoc, Report};
use crate::rss;
use crate::topology::{self, Site};

/// How long to wait for pumps to stop writing after the load programs exited
/// (implementation tuning item, design D3/D6: size-stability based).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
const DRAIN_POLL: Duration = Duration::from_millis(300);
/// Grace beyond the requested wall clock before bench force-stops the load.
const EXIT_GRACE: Duration = Duration::from_secs(60);

fn progress(msg: &str) {
    // Single-line stderr refresh (design non-goal: no TUI/progress bars).
    // Throttled so non-TTY logs don't fill up with refresh lines.
    static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_MS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 500 {
        return;
    }
    LAST_MS.store(now, Ordering::Relaxed);
    eprint!("\r[bench] {msg}          ");
}

fn is_live(state: &str) -> bool {
    matches!(state, "starting" | "running" | "stopping" | "backoff")
}

/// Wait until every load program either reports `running` or has already
/// finished cleanly (tiny row bounds can beat the first poll).
fn wait_all_running(client: &Client, names: &[String]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let states = client.program_states(names)?;
        let settled = names.iter().all(|n| {
            states
                .iter()
                .any(|(name, s)| name == n && (s == "running" || s == "exited"))
        });
        if settled {
            return Ok(());
        }
        for (name, s) in &states {
            // start failures cycle through backoff before going fatal
            if matches!(s.as_str(), "fatal" | "backoff" | "stopped") {
                bail!("load program {name} is {s} instead of running (start failure?)");
            }
        }
        if Instant::now() > deadline {
            bail!("load programs never reached running (states: {states:?})");
        }
        progress("waiting for load programs to come up");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait until every load program left the live states. The generators exit by
/// themselves (their bounds trip first); anything still alive past the cap is
/// stopped via the control plane (the graceful-stop contract) and the run fails.
fn wait_all_done(client: &Client, names: &[String], wall_cap: Duration) -> Result<()> {
    let deadline = Instant::now() + wall_cap + EXIT_GRACE;
    loop {
        let states = client.program_states(names)?;
        if !states.iter().any(|(_, s)| is_live(s)) {
            for (name, s) in &states {
                if s == "fatal" {
                    bail!("load program {name} entered FATAL state");
                }
            }
            return Ok(());
        }
        if Instant::now() > deadline {
            eprintln!("[bench] load programs outlived the wall cap; stopping them via the daemon");
            for name in names {
                let _ = client.stop_program(name);
            }
            let stop_deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < stop_deadline {
                let states = client.program_states(names)?;
                if !states.iter().any(|(_, s)| is_live(s)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            bail!("load programs did not finish within the wall cap + grace");
        }
        progress("waiting for load programs to finish");
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Wait until the bench log files stop growing (pump drain window), bounded.
fn drain(log_dir: &std::path::Path, names: &[String]) -> Result<()> {
    let size = || -> u64 {
        names
            .iter()
            .flat_map(|n| {
                [
                    log_dir.join(format!("{n}.out.log")),
                    log_dir.join(format!("{n}.err.log")),
                ]
            })
            .map(|p| std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0))
            .sum()
    };
    let mut prev = [size(), u64::MAX, 0];
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            eprintln!(
                "[bench] warning: log files kept growing for {DRAIN_TIMEOUT:?}; reconciling anyway"
            );
            return Ok(());
        }
        let cur = size();
        prev = [cur, prev[0], prev[1]];
        if prev[0] == prev[1] && prev[1] == prev[2] && prev[0] == cur {
            return Ok(());
        }
        progress("draining pump output");
        std::thread::sleep(DRAIN_POLL);
    }
}

fn read_counts(count_files: &[(String, std::path::PathBuf)]) -> Result<Vec<(String, GenCounters)>> {
    let mut out = Vec::new();
    for (name, path) in count_files {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read the generator count file for {name} ({}): the load program \
                 likely crashed or was killed before finishing",
                path.display()
            )
        })?;
        let c: GenCounters =
            serde_json::from_str(&text).with_context(|| format!("parse count file for {name}"))?;
        out.push((name.clone(), c));
    }
    Ok(out)
}

/// Parent-side cap for the wait loop: the generators' own bounds (duration,
/// rows/rate) tell us when they should be done.
fn wall_cap(eff: &Effective) -> Duration {
    if eff.duration > 0 {
        Duration::from_secs(eff.duration)
    } else if let Some(r) = eff.rate {
        if r > 0 && eff.rows_per_program > 0 {
            return Duration::from_secs_f64(eff.rows_per_program as f64 / r as f64 * 2.0 + 300.0);
        }
        Duration::from_secs(3600)
    } else {
        // full speed with a row/size bound: bounded work, no meaningful cap
        Duration::from_secs(3600)
    }
}

pub fn run_bench(a: &BenchArgs) -> Result<()> {
    let eff: Effective = crate::cli::resolve(a)?;
    let exe = std::env::current_exe()
        .context("resolve current_exe (the generator re-enters this binary)")?
        .canonicalize()
        .context("canonicalize current_exe")?;
    let started_epoch = report::now_epoch_secs();

    // 1. Topology.
    let site: Site = match &a.connect {
        Some(addr) => {
            let s = topology::connect_site(addr, a.token.as_deref())
                .map_err(|e| e.context("connect topology"))?;
            eprintln!(
                "[bench] connected to daemon at {addr} (pid unknown: RSS sampling off in \
                 connect mode)"
            );
            s
        }
        None => {
            let s =
                topology::spawn_site(a.daemon.as_deref()).map_err(|e| e.context("spawn topology"))?;
            eprintln!(
                "[bench] spawned isolated daemon (pid {}, control {})",
                s.daemon_pid.unwrap_or(0),
                s.client.addr()
            );
            s
        }
    };

    // 2. Plans against the site's count dir, then install the cleanup net
    //    BEFORE touching the daemon's registry.
    let plans = cases::plan(&eff, &exe, &site.count_dir)?;
    topology::install_ctrlc_handler()?;
    topology::install_cleanup(site.cleanup_ctx(&plans, a.keep));

    let names: Vec<String> = plans.iter().map(|p| p.name.clone()).collect();
    let result = measure(&site, &plans, &names, &eff, a, started_epoch);

    // 3. Cleanup no matter what (success, failure, and — via the ctrl-C
    //    handler — interruption). Whoever takes the context owns it.
    if let Some(ctx) = topology::take_cleanup() {
        if topology::claim_cleanup() {
            let kept = ctx.keep;
            let root = ctx.root.clone();
            let log_dir = ctx.log_dir.clone();
            topology::run_cleanup(&ctx);
            topology::release_cleanup();
            if kept {
                eprintln!(
                    "[bench] --keep: kept {} (logs under {})",
                    root.as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "bench logs".to_string()),
                    log_dir.display()
                );
            }
        }
    } else if topology::cleanup_in_progress() {
        // The ctrl-C handler is cleaning up and will exit the process; give
        // it a bounded window so main's exit does not cut it short.
        for _ in 0..40 {
            if !topology::cleanup_in_progress() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    eprintln!("[bench] done.");
    result
}

fn measure(
    site: &Site,
    plans: &[ProgramPlan],
    names: &[String],
    eff: &Effective,
    a: &BenchArgs,
    started_epoch: u64,
) -> Result<()> {
    // The load-phase wall clock starts when the apps are registered and ends
    // when every generator has exited — deterministic even when a tiny row
    // bound finishes before the first state poll.
    let t0 = Instant::now();
    site.register(plans)
        .context("register bench apps on the daemon")?;

    // RSS sampler over the measurement window (spawn mode only: we know the
    // daemon pid; connect mode reports null).
    let stop = Arc::new(AtomicBool::new(false));
    let rss_handle = site
        .daemon_pid
        .and_then(|pid| rss::start(pid, stop.clone()));

    // Wait for the load to finish (generators self-terminate on their bounds).
    wait_all_running(&site.client, names)?;
    wait_all_done(&site.client, names, wall_cap(eff))?;

    // Drain, then stop sampling.
    drain(&site.log_dir, names)?;
    stop.store(true, Ordering::Relaxed);
    let daemon_rss = rss_handle
        .and_then(|h| h.join().ok())
        .and_then(|samples| rss::summarize(&samples))
        .map(|(peak_kib, avg_kib)| RssDoc { peak_kib, avg_kib });
    let wall = t0.elapsed().as_secs_f64().max(0.001);

    // Counters back from the generators.
    let count_files: Vec<(String, std::path::PathBuf)> = plans
        .iter()
        .map(|p| (p.name.clone(), p.count_file.clone()))
        .collect();
    let counts = read_counts(&count_files)?;

    // Reconcile the disk (integrity) and count rotations.
    let mut programs = Vec::new();
    let mut expected_total = 0u64;
    let mut found_total = 0u64;
    let mut rotation_count = 0u64;
    let mut problems = Vec::new();
    for (name, c) in &counts {
        let (out, err) =
            integrity::verify_program(&site.log_dir, name, c.out_rows, c.err_rows);
        rotation_count += integrity::count_rotated(&site.log_dir.join(format!("{name}.out.log")));
        rotation_count += integrity::count_rotated(&site.log_dir.join(format!("{name}.err.log")));
        expected_total += out.expected + err.expected;
        found_total += out.found + err.found;
        problems.extend(out.problems.iter().cloned());
        problems.extend(err.problems.iter().cloned());
        programs.push(ProgramMetrics {
            name: name.clone(),
            rows: c.rows,
            bytes: c.bytes,
            out_rows: c.out_rows,
            err_rows: c.err_rows,
            rows_per_sec: c.rows as f64 / wall,
            bytes_per_sec: c.bytes as f64 / wall,
        });
    }
    if problems.len() > 5 {
        let extra = problems.len() - 5;
        problems.truncate(5);
        problems.push(format!("... and {extra} more problems"));
    }

    let aggregate_rows: u64 = programs.iter().map(|p| p.rows).sum();
    let aggregate_bytes: u64 = programs.iter().map(|p| p.bytes).sum();
    let rep = Report {
        case: eff.case.as_str().to_string(),
        programs,
        params: serde_json::json!({
            "log_rows": eff.rows_per_program,
            "log_row_size": eff.row_size,
            "log_total_size": eff.total_all,
            "rate": eff.rate,
            "duration": eff.duration,
            "programs": eff.programs,
            "connect": a.connect,
        }),
        aggregate: Aggregate {
            rows: aggregate_rows,
            bytes: aggregate_bytes,
            rows_per_sec: aggregate_rows as f64 / wall,
            bytes_per_sec: aggregate_bytes as f64 / wall,
        },
        wall_time_secs: wall,
        rotation_count,
        daemon_rss,
        integrity: IntegrityDoc {
            pass: problems.is_empty(),
            expected_rows: expected_total,
            found_rows: found_total,
            problems,
        },
        started_at: started_epoch,
        bin_versions: BinVersions {
            xkeeper: site.daemon_version.clone(),
            bench: env!("CARGO_PKG_VERSION").to_string(),
        },
    };

    // Report: table to stdout, JSON to --json. Progress noise never touches
    // stdout.
    println!();
    print!("{}", report::render_table(&rep));
    if let Some(p) = &a.json {
        rep.write_json(p)?;
        eprintln!("[bench] JSON report written to {}", p.display());
    }

    // Exit semantics: a failed integrity check is a failed run even though
    // the report was emitted.
    if !rep.integrity.pass {
        bail!(
            "integrity check failed: expected {} rows, found {} (see the report above)",
            rep.integrity.expected_rows,
            rep.integrity.found_rows
        );
    }
    Ok(())
}
