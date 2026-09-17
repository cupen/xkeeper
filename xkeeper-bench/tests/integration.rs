//! Full-cycle integration tests: bench binary against a real daemon binary.
//!
//! These spawn processes and take seconds each (repo convention: process-
//! spanning checks stay out of the default `cargo test` run — see xtask's
//! stress harness for the same reasoning). Run them explicitly:
//!
//! ```sh
//! cargo test -p xkeeper-bench --test integration -- --ignored --test-threads=1
//! ```
//!
//! Every cycle asserts the cleanup contract: no `xk-bench-*` workspaces and
//! no leftover generator processes after the run.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The bench binary under test (provided by cargo for integration tests).
fn bench_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_xkeeper-bench"))
}

fn daemon_bin() -> PathBuf {
    let name = if cfg!(windows) { "xkeeper.exe" } else { "xkeeper" };
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().expect("crate lives in the workspace");
    let cand = root.join("target").join("debug").join(name);
    assert!(cand.is_file(), "daemon binary missing at {} (cargo build first)", cand.display());
    cand
}

fn run_bench(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bench_bin())
        .args(args)
        .output()
        .expect("spawn bench");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Generator = bench self-reentry: any `__generate` argv means a leftover
/// load program (unix only, like the e2e harness's process scan).
#[cfg(unix)]
fn leftover_generators() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(cmdline) = std::fs::read(e.path().join("cmdline")) {
            if cmdline.split(|&b| b == 0).any(|a| a == b"__generate") {
                out.push(name.parse().unwrap());
            }
        }
    }
    out
}

#[cfg(unix)]
fn leftover_workspaces() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // spawn workspaces are exactly xk-bench-<pid>-<nanos>; ignore the
            // it- report files and counts dirs that share the prefix
            let rest = name.strip_prefix("xk-bench-").unwrap_or("");
            let mut parts = rest.split('-');
            let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                if numeric(a) && numeric(b) && parts.next().is_none() {
                    out.push(e.path());
                }
            }
        }
    }
    out
}

#[cfg(unix)]
fn assert_no_residue() {
    // workspace removal can lag the process exit by a moment
    for _ in 0..20 {
        if leftover_generators().is_empty() && leftover_workspaces().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        leftover_generators().is_empty(),
        "leftover generator processes: {:?}",
        leftover_generators()
    );
    assert!(
        leftover_workspaces().is_empty(),
        "leftover bench workspaces: {:?}",
        leftover_workspaces()
    );
}

fn read_json(path: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("JSON report written");
    serde_json::from_str(&text).expect("valid JSON report")
}

// -- scratch daemon helper (connect-mode tests) ----------------------------------

struct ScratchDaemon {
    dir: PathBuf,
    port: u16,
    child: std::process::Child,
}

impl Drop for ScratchDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Spawn a scratch daemon with its own temp app_dir/log_dir on a random port.
/// `extra_daemon_keys` splices raw keys into the [daemon] table (e.g. auth_token).
fn spawn_scratch_daemon(tag: &str, extra_daemon_keys: &str) -> ScratchDaemon {
    let dir = std::env::temp_dir().join(format!("xk-bench-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("apps")).unwrap();
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let log_dir = dir.join("logs").to_string_lossy().replace('\\', "/");
    let app_dir = dir.join("apps").to_string_lossy().replace('\\', "/");
    std::fs::write(
        dir.join("daemon.toml"),
        format!(
            "[daemon]\nhost = \"127.0.0.1\"\nport = {port}\nlog_dir = \"{log_dir}\"\n\
             app_dir = \"{app_dir}\"\nlog_level = \"warn\"\nmonitor_interval = 0.3\n\
             {extra_daemon_keys}"
        ),
    )
    .unwrap();
    let out = std::fs::File::create(dir.join("daemon.out.log")).unwrap();
    let err = out.try_clone().unwrap();
    let child = Command::new(daemon_bin())
        .arg("--config")
        .arg(dir.join("daemon.toml"))
        .arg("run")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn scratch daemon");
    ScratchDaemon { dir, port, child }
}

/// /v1/health answers (auth-exempt on the daemon side).
fn wait_health(port: u16) -> bool {
    for _ in 0..50 {
        if Command::new("curl")
            .args(["-sf", &format!("http://127.0.0.1:{port}/v1/health")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// Any bench-prefixed file left in the scratch daemon's app_dir/logs.
fn bench_traces(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for sub in ["apps", "logs"] {
        if let Ok(entries) = std::fs::read_dir(dir.join(sub)) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("xkeeper-bench-") {
                    out.push(format!("{sub}/{n}"));
                }
            }
        }
    }
    out
}

/// Pids whose cmdline references `needle` anywhere (unix only).
#[cfg(unix)]
fn pids_referencing(needle: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(cmdline) = std::fs::read(e.path().join("cmdline")) {
            let needle = needle.as_bytes();
            if cmdline
                .split(|&b| b == 0)
                .any(|a| a.windows(needle.len()).any(|w| w == needle))
            {
                out.push(name.parse().unwrap());
            }
        }
    }
    out
}

// -- full cycles (ignored: real daemon + real load) ---------------------------

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn firehose_full_cycle_reports_and_cleans_up() {
    let json = std::env::temp_dir().join("it-firehose-report.json");
    let (code, stdout, stderr) = run_bench(&[
        "--case", "firehose",
        "--log-rows", "20000",
        "--json", json.to_str().unwrap(),
        "--daemon", daemon_bin().to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "bench failed: {stderr}");
    assert!(stdout.contains("rows/s"), "table on stdout: {stdout}");
    assert!(stdout.contains("integrity"), "table on stdout: {stdout}");
    assert!(!stdout.contains('\r'), "stdout must be free of progress refreshes: {stdout}");
    let v = read_json(json.to_str().unwrap());
    assert_eq!(v["case"], "firehose");
    assert_eq!(v["aggregate"]["rows"], 20000, "rows bound honored exactly");
    assert_eq!(v["integrity"]["pass"], true);
    assert!(v["aggregate"]["rows_per_sec"].as_f64().unwrap() > 0.0);
    assert!(v["bin_versions"]["xkeeper"].is_string());
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn rotation_full_cycle_rotates_and_reconciles() {
    let json = std::env::temp_dir().join("it-rotation-report.json");
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "rotation",
        "--log-rows", "120000",
        "--json", json.to_str().unwrap(),
        "--daemon", daemon_bin().to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "bench failed: {stderr}");
    let v = read_json(json.to_str().unwrap());
    assert_eq!(v["case"], "rotation");
    let rotations = v["rotation_count"].as_u64().unwrap();
    assert!(rotations > 0, "rotation case must rotate, got {rotations}");
    assert_eq!(v["integrity"]["pass"], true, "integrity: {v}");
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn fanout_full_cycle_splits_streams_and_aggregates() {
    let json = std::env::temp_dir().join("it-fanout-report.json");
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "fanout",
        "--programs", "3",
        "--log-rows", "20000",
        "--json", json.to_str().unwrap(),
        "--daemon", daemon_bin().to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "bench failed: {stderr}");
    let v = read_json(json.to_str().unwrap());
    assert_eq!(v["case"], "fanout");
    let progs = v["programs"].as_array().unwrap();
    assert_eq!(progs.len(), 3);
    for (i, p) in progs.iter().enumerate() {
        assert_eq!(p["name"], format!("xkeeper-bench-fanout-{i}"));
        let rows = p["rows"].as_u64().unwrap();
        let err = p["err_rows"].as_u64().unwrap();
        assert_eq!(rows, 20000);
        // 10:1 split: every 10th row goes to stderr
        let want_err = ((rows / 10).saturating_sub(1))..=((rows / 10) + 1);
        assert!(
            want_err.contains(&err),
            "err rows should be ~1/10: {err} of {rows}"
        );
    }
    assert_eq!(v["aggregate"]["rows"], 60000);
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn drip_full_cycle_samples_rss_and_defaults_to_100_rows_per_sec() {
    let json = std::env::temp_dir().join("it-drip-report.json");
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "drip",
        "--duration", "3",
        "--json", json.to_str().unwrap(),
        "--daemon", daemon_bin().to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "bench failed: {stderr}");
    let v = read_json(json.to_str().unwrap());
    assert_eq!(v["case"], "drip");
    let rows = v["aggregate"]["rows"].as_u64().unwrap();
    // default rate is 100 rows/s for 3s (allow pacing jitter)
    let expected = 300u64;
    let want_rows = (expected / 2)..=(expected + expected / 2);
    assert!(
        want_rows.contains(&rows),
        "drip default rate should be ~100 rows/s, got {rows} rows in 3s"
    );
    // RSS sampling (unix): peak and avg present and positive
    let rss = &v["daemon_rss"];
    if cfg!(unix) {
        assert!(rss["peak_kib"].as_u64().unwrap() > 0, "sampled RSS peak: {v}");
        assert!(rss["avg_kib"].as_u64().unwrap() > 0);
    } else {
        assert!(rss.is_null(), "RSS must degrade to null on Windows");
    }
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn keep_preserves_the_spawn_workspace() {
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "drip",
        "--duration", "2",
        "--keep",
        "--daemon", daemon_bin().to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "bench failed: {stderr}");
    assert!(stderr.contains("--keep"), "kept path is announced: {stderr}");
    let kept = leftover_workspaces();
    assert_eq!(kept.len(), 1, "exactly the kept workspace remains: {kept:?}");
    let ws = &kept[0];
    assert!(ws.join("daemon.toml").is_file());
    assert!(ws.join("apps").is_dir());
    // daemon was stopped, not left running
    #[cfg(unix)]
    assert!(leftover_generators().is_empty());
    let _ = std::fs::remove_dir_all(ws);
}

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn integrity_failure_exits_nonzero_with_report() {
    let json = std::env::temp_dir().join("it-inject-report.json");
    let bench = bench_bin().to_path_buf();
    let daemon = daemon_bin();
    let mut child = Command::new(&bench)
        .args([
            "--case", "rotation",
            "--log-rows", "400000",
            "--json", json.to_str().unwrap(),
            "--daemon", daemon.to_str().unwrap(),
        ])
        .spawn()
        .expect("spawn bench");
    // give the load a moment to produce rotation files, then delete one
    std::thread::sleep(Duration::from_millis(1200));
    let mut deleted = false;
    for _ in 0..10 {
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("xk-bench-") {
                    continue;
                }
                let logs = e.path().join("logs");
                if let Ok(files) = std::fs::read_dir(&logs) {
                    let rotated: Vec<_> = files
                        .flatten()
                        .map(|f| f.path())
                        .filter(|p| {
                            p.to_string_lossy().contains("xkeeper-bench-rotation.out.log.")
                        })
                        .collect();
                    if let Some(victim) = rotated.first() {
                        deleted = std::fs::remove_file(victim).is_ok();
                    }
                }
            }
        }
        if deleted {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(deleted, "could not inject a rotation-file deletion");
    let out = child.wait().expect("wait bench");
    assert!(!out.success(), "integrity failure must exit non-zero");
    let v = read_json(json.to_str().unwrap());
    assert_eq!(v["integrity"]["pass"], false);
    assert_ne!(
        v["integrity"]["expected_rows"], v["integrity"]["found_rows"],
        "found rows must fall short of expected after the deletion"
    );
    // "失败也清理": even a failed run leaves no workspace or load process
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

// -- connect topology ----------------------------------------------------------

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn connect_cycle_leaves_no_trace_on_the_target_daemon() {
    use std::process::{Command as Cmd, Stdio};

    // scratch daemon on a random port with its own temp app_dir/log_dir
    let scratch = std::env::temp_dir().join(format!("xk-bench-it-connect-{}", std::process::id()));
    std::fs::create_dir_all(scratch.join("apps")).unwrap();
    std::fs::create_dir_all(scratch.join("logs")).unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let log_dir = scratch.join("logs").to_string_lossy().replace('\\', "/");
    std::fs::write(
        scratch.join("daemon.toml"),
        format!(
            "[daemon]\nhost = \"127.0.0.1\"\nport = {port}\nlog_dir = \"{log_dir}\"\nlog_level = \"warn\"\napp_dir = \"{}\"\nmonitor_interval = 0.3\n",
            scratch.join("apps").to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    let out = std::fs::File::create(scratch.join("daemon.out.log")).unwrap();
    let err = out.try_clone().unwrap();
    let mut daemon = Cmd::new(daemon_bin())
        .arg("--config")
        .arg(scratch.join("daemon.toml"))
        .arg("run")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn scratch daemon");

    // wait healthy
    let mut healthy = false;
    for _ in 0..50 {
        if Cmd::new("curl")
            .args(["-sf", &format!("http://127.0.0.1:{port}/v1/health")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            healthy = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    assert!(healthy, "scratch daemon did not become healthy");

    let (code, _stdout, stderr) = run_bench(&[
        "--case", "firehose",
        "--log-rows", "20000",
        "--connect", &format!("127.0.0.1:{port}"),
    ]);
    assert_eq!(code, 0, "connect bench failed: {stderr}");

    // no bench apps or logs left on the target daemon
    let apps_left = std::fs::read_dir(scratch.join("apps"))
        .unwrap()
        .flatten()
        .count();
    assert_eq!(apps_left, 0, "bench app files must be removed");
    let logs_left: Vec<_> = std::fs::read_dir(scratch.join("logs"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("xkeeper-bench-"))
        .collect();
    assert!(logs_left.is_empty(), "bench log files must be removed: {logs_left:?}");
    let status = Cmd::new("curl")
        .args(["-sf", &format!("http://127.0.0.1:{port}/v1/status")])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&status.stdout).to_string();
    assert!(!text.contains("xkeeper-bench-"), "no bench programs on the daemon: {text}");

    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn connect_unreachable_and_collision_exit_nonzero_without_registration() {
    // unreachable target
    let (code, _stdout, stderr) = run_bench(&["--case", "firehose", "--connect", "127.0.0.1:9"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("cannot reach"), "{stderr}");

    // collision: plant a bench-named app file in a scratch daemon's app_dir
    use std::process::{Command as Cmd, Stdio};
    let scratch = std::env::temp_dir().join(format!("xk-bench-it-collide-{}", std::process::id()));
    std::fs::create_dir_all(scratch.join("apps")).unwrap();
    std::fs::create_dir_all(scratch.join("logs")).unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    std::fs::write(
        scratch.join("daemon.toml"),
        format!(
            "[daemon]\nhost = \"127.0.0.1\"\nport = {port}\nlog_dir = \"{}\"\napp_dir = \"{}\"\n",
            scratch.join("logs").to_string_lossy().replace('\\', "/"),
            scratch.join("apps").to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    std::fs::write(
        scratch.join("apps").join("xkeeper-bench-firehose.toml"),
        "[app]\nautostart = false\n\n[program.xkeeper-bench-firehose]\ncommand = \"/bin/sleep\"\nargs = [\"30\"]\n",
    )
    .unwrap();
    let out = std::fs::File::create(scratch.join("daemon.out.log")).unwrap();
    let err = out.try_clone().unwrap();
    let mut daemon = Cmd::new(daemon_bin())
        .arg("--config")
        .arg(scratch.join("daemon.toml"))
        .arg("run")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));

    let (code, _stdout, stderr) =
        run_bench(&["--case", "firehose", "--connect", &format!("127.0.0.1:{port}")]);
    assert_ne!(code, 0, "collision must be refused");
    assert!(stderr.contains("refusing"), "{stderr}");
    // the planted file is untouched
    assert!(scratch.join("apps").join("xkeeper-bench-firehose.toml").is_file());

    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&scratch);
}

// -- load parameter bounds (duration / total-size first-wins) --------------------

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn firehose_rate_bound_stops_at_duration_with_full_json_schema() {
    let json = std::env::temp_dir().join("it-firehose-rate-report.json");
    let t0 = std::time::Instant::now();
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "firehose",
        "--rate", "100",
        "--duration", "3",
        "--json", json.to_str().unwrap(),
        "--daemon", daemon_bin().to_str().unwrap(),
    ]);
    let elapsed = t0.elapsed();
    assert_eq!(code, 0, "bench failed: {stderr}");
    assert!(
        elapsed < Duration::from_secs(20),
        "the duration bound must trip the run, took {elapsed:?}"
    );
    let v = read_json(json.to_str().unwrap());
    assert_eq!(v["case"], "firehose");
    // ~100 rows/s for ~3s (spec allows rate jitter)
    let rows = v["aggregate"]["rows"].as_u64().unwrap();
    assert!((150..=600).contains(&rows), "~300 rows expected, got {rows}");
    // the full D8 schema, field by field
    assert_eq!(v["params"]["rate"], 100);
    assert_eq!(v["params"]["duration"], 3);
    assert!(v["aggregate"]["bytes"].as_u64().unwrap() > 0);
    assert!(v["aggregate"]["bytes_per_sec"].as_f64().unwrap() > 0.0);
    assert!(v["wall_time_secs"].as_f64().unwrap() > 0.0);
    assert_eq!(v["rotation_count"], 0, "firehose runs without rotation");
    assert_eq!(v["integrity"]["pass"], true, "{v}");
    assert_eq!(v["integrity"]["expected_rows"], v["integrity"]["found_rows"]);
    assert!(v["started_at"].as_u64().unwrap() > 0);
    assert!(v["bin_versions"]["xkeeper"].is_string());
    assert!(v["bin_versions"]["bench"].is_string());
    let p = &v["programs"][0];
    assert_eq!(p["name"], "xkeeper-bench-firehose");
    assert!(p["rows_per_sec"].as_f64().unwrap() > 0.0);
    assert!(p["bytes_per_sec"].as_f64().unwrap() > 0.0);
    assert!(p["out_rows"].as_u64().unwrap() > 0);
    assert_eq!(p["err_rows"], 0, "single-stream case writes no err rows");
    if cfg!(unix) {
        assert!(
            v["daemon_rss"]["peak_kib"].as_u64().unwrap_or(0) > 0,
            "spawn mode must sample the daemon RSS: {v}"
        );
    } else {
        assert!(v["daemon_rss"].is_null(), "RSS degrades to null off unix");
    }
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn firehose_full_speed_stops_at_duration() {
    // duration is the only bound: full-speed production must stop at the wall
    let json = std::env::temp_dir().join("it-firehose-dur-report.json");
    let t0 = std::time::Instant::now();
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "firehose",
        "--duration", "3",
        "--json", json.to_str().unwrap(),
        "--daemon", daemon_bin().to_str().unwrap(),
    ]);
    let elapsed = t0.elapsed();
    assert_eq!(code, 0, "bench failed: {stderr}");
    assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
    let v = read_json(json.to_str().unwrap());
    let rows = v["aggregate"]["rows"].as_u64().unwrap();
    assert!(rows > 1000, "full speed for 3s must produce many rows, got {rows}");
    assert!(v["aggregate"]["rows_per_sec"].as_f64().unwrap() > 0.0);
    assert!(
        v["wall_time_secs"].as_f64().unwrap() < 15.0,
        "wall time reflects the 3s bound, not the 30s default: {v}"
    );
    assert_eq!(v["integrity"]["pass"], true, "{v}");
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn firehose_total_size_bound_stops_at_the_ceiling() {
    // spec scenario: --log-row-size 100 --log-total-size 2000 --log-rows 0
    let json = std::env::temp_dir().join("it-firehose-size-report.json");
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "firehose",
        "--log-rows", "0",
        "--log-row-size", "100",
        "--log-total-size", "2000",
        "--json", json.to_str().unwrap(),
        "--daemon", daemon_bin().to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "bench failed: {stderr}");
    let v = read_json(json.to_str().unwrap());
    let rows = v["aggregate"]["rows"].as_u64().unwrap();
    let bytes = v["aggregate"]["bytes"].as_u64().unwrap();
    assert!(
        (10..60).contains(&rows),
        "~20 rows of 101 bytes make the ceiling, got {rows} rows"
    );
    assert!(
        bytes >= 2000 && bytes < 2101,
        "total bytes {bytes} must sit between the 2000 ceiling and ceiling+one line"
    );
    assert_eq!(v["integrity"]["pass"], true, "{v}");
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

// -- default daemon discovery -----------------------------------------------------

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn daemon_is_discovered_without_the_flag() {
    // no --daemon given: bench must find the sibling daemon in target/debug
    assert!(daemon_bin().is_file(), "cargo build first");
    let json = std::env::temp_dir().join("it-discovery-report.json");
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "firehose",
        "--log-rows", "20000",
        "--json", json.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "bench failed: {stderr}");
    let v = read_json(json.to_str().unwrap());
    assert_eq!(v["aggregate"]["rows"], 20000);
    assert_eq!(v["integrity"]["pass"], true, "{v}");
    #[cfg(unix)]
    assert_no_residue();
    let _ = std::fs::remove_file(&json);
}

// -- connect auth (--token) --------------------------------------------------------

#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn connect_auth_token_gate_and_success_cycle() {
    let daemon = spawn_scratch_daemon("auth", "auth_token = \"bench-it-token\"\n");
    assert!(
        wait_health(daemon.port),
        "scratch daemon did not become healthy (health is auth-exempt)"
    );
    let addr = format!("127.0.0.1:{}", daemon.port);

    // no token → 401, nothing registered on the target
    let (code, _stdout, stderr) =
        run_bench(&["--case", "firehose", "--log-rows", "100", "--connect", &addr]);
    assert_ne!(code, 0, "connect without a token must fail");
    assert!(stderr.contains("401"), "auth failure reports the 401: {stderr}");
    assert!(
        bench_traces(&daemon.dir).is_empty(),
        "no bench traces without auth: {:?}",
        bench_traces(&daemon.dir)
    );

    // wrong token → 401, still nothing registered
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "firehose",
        "--log-rows", "100",
        "--token", "wrong-token",
        "--connect", &addr,
    ]);
    assert_ne!(code, 0, "connect with a wrong token must fail");
    assert!(stderr.contains("401"), "wrong-token failure reports the 401: {stderr}");
    assert!(bench_traces(&daemon.dir).is_empty());

    // correct token → full cycle and no residue on the target
    let (code, _stdout, stderr) = run_bench(&[
        "--case", "firehose",
        "--log-rows", "20000",
        "--token", "bench-it-token",
        "--connect", &addr,
    ]);
    assert_eq!(code, 0, "connect with the right token must succeed: {stderr}");
    assert!(
        bench_traces(&daemon.dir).is_empty(),
        "bench traces must be cleaned: {:?}",
        bench_traces(&daemon.dir)
    );
    let status = Command::new("curl")
        .args(["-sf", &format!("http://{addr}/v1/status")])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&status.stdout).to_string();
    assert!(!text.contains("xkeeper-bench-"), "no bench programs left: {text}");
    #[cfg(unix)]
    assert_no_residue();
}

// -- interruption cleanup (ctrl-C) -------------------------------------------------

#[cfg(unix)]
#[test]
#[ignore = "spawns a real daemon; run with --ignored"]
fn ctrl_c_interrupt_cleans_up_spawn_site() {
    let mut child = Command::new(bench_bin())
        .args([
            "--case", "drip",
            "--duration", "300",
            "--daemon", daemon_bin().to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bench");
    let bench_pid = child.id();

    // wait until the load program is up — by then the ctrl-C handler and the
    // cleanup net are installed (they precede registration)
    let mut up = false;
    for _ in 0..100 {
        if !leftover_generators().is_empty() {
            up = true;
            break;
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(up, "the drip load program never came up");

    // SIGINT to the bench process only: bench itself must clean up its daemon
    let st = Command::new("sh")
        .args(["-c", &format!("kill -s INT {bench_pid}")])
        .status()
        .expect("send SIGINT");
    assert!(st.success(), "kill -s INT failed");

    let out = child.wait().expect("wait bench");
    assert_eq!(out.code(), Some(130), "an interrupted bench exits 130");
    if let Some(mut p) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut p, &mut String::new());
    }
    if let Some(mut p) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut p, &mut String::new());
    }
    assert_no_residue();
    assert!(
        pids_referencing("xk-bench-").is_empty(),
        "the bench-spawned daemon is still alive: {:?}",
        pids_referencing("xk-bench-")
    );
}

// -- cheap subprocess checks (not ignored) --------------------------------------

#[test]
fn unknown_case_prints_supported_list_and_exits_nonzero() {
    let (code, _stdout, stderr) = run_bench(&["--case", "turbo"]);
    assert_ne!(code, 0);
    for c in ["firehose", "rotation", "fanout", "drip"] {
        assert!(format!("{stderr}").contains(c), "list must contain {c}: {stderr}");
    }
}

#[test]
fn help_lists_cases_hides_generator_and_bare_run_is_rejected() {
    let (code, stdout, _stderr) = run_bench(&["--help"]);
    assert_eq!(code, 0);
    for c in ["firehose", "rotation", "fanout", "drip"] {
        assert!(stdout.contains(c), "help must list the case {c}: {stdout}");
    }
    assert!(
        !stdout.contains("__generate"),
        "the self-reentry generator must stay hidden from --help: {stdout}"
    );
    // bare `xkeeper-bench` (no --case) is a parameter error, not a run
    let (code, _stdout, stderr) = run_bench(&[]);
    assert_ne!(code, 0, "--case is required");
    assert!(stderr.contains("--case"), "{stderr}");
}
