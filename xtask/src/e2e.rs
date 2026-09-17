//! Acceptance e2e for the apply-workflow + app-registry + actions changes
//! (`cargo run -p xtask -- e2e`).
//!
//! Drives the REAL daemon binary end-to-end, in two passes:
//!
//! - CLI pass — the acceptance checklist as executable scenarios (spec:
//!   apply-workflow, configuration, control-plane, shell-client,
//!   app-registry, actions, glossary):
//!   A. detect does not touch processes (periodic + reload preview),
//!   B. scoped apply leaves other apps untouched,
//!   C. --restart restarts unchanged programs but never a user-stopped one,
//!   D. online `add` enters pending and starts on apply,
//!   E. no-change apply is idempotent,
//!   G. add-process scaffold: offline `--apply` fails loudly (exit 3) and
//!      `--env` parse errors exit 2 (G0, daemon not yet running) → generate →
//!      apply → no-change re-add → changed re-add + `apply all` → `--apply`
//!      one-step (app-scoped) → reserved `all` rejected → remove deletes the
//!      generated file and stops the program.
//!   H. actions (actions/glossary change): offline declaration + rejection
//!      cases (charset / unknown variable / reserved word / timeout range),
//!      then online: `--app` fan-out order + user_stopped coverage, custom
//!      action success with variable substitution, work_dir resolution,
//!      timeout tree kill, same-action mutual exclusion, chained
//!      `xkeeper restart` inside an action, stopped-program pid→empty,
//!      signal whitelist in/out + not-running (unix).
//!   K. config subcommand (add-config-subcommand): init (defaults + comments
//!      + app dir + example.toml.sample), get (defaults / file values /
//!      multi-key), strong-typed set (comments and key order preserved),
//!      rejection paths (unknown key, type violation, missing file, init
//!      overwrite), delete (default fallback, idempotent, unknown table),
//!      and `--edit` through a fake $EDITOR (valid + invalid). The webui
//!      keys ride the same engine (K16: set/get/delete on webui.listen and
//!      the whole [webui] table).
//! - webui lifecycle pass (config-driven-webui): the daemon boots with
//!   `xkeeper run` and a `[webui]` section in daemon.toml; W1 deletes the
//!   section + reload → connection refused, W2 sets a new listen + reload →
//!   serving on the new address, W3 restores the listen + reload → rebind,
//!   W4/W5 occupy then free the listen → reload degrades with a note and
//!   recovers, W7 boots a second daemon with a bare `[webui]` → serves the
//!   built-in default 127.0.0.1:9877 and the console exits with the daemon.
//!   Offline: G0c2 pins the removed `webui`/`system webui` subcommands,
//!   G0c3 covers the shell exit-3 contract, G0d2 rejects an unknown
//!   `[webui]` key through `validate`.
//! - webui pass — a real browser (playwright, chromium) against the embedded
//!   console: the pending badge appears after a disk edit, "Apply 此应用"
//!   (app-scope) confirms and applies, the result text renders, and a second
//!   app's pending is untouched by the scoped apply.
//!
//! Fast correctness tests stay in `cargo test` + `pnpm test`; this harness
//! exists because these paths span process boundaries (child processes, HTTP,
//! WS, a browser) and take ~20-40s.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub(crate) struct Args {
    pub keep: bool,
    /// Skip the browser pass (no node/playwright on the machine).
    pub no_browser: bool,
}

// -- workspace -------------------------------------------------------------

struct Workspace {
    root: PathBuf,
    control_port: u16,
    webui_port: u16,
}

impl Workspace {
    fn config_arg(&self) -> String {
        format!("{}", self.root.join("daemon.toml").display())
    }
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// alpha: web + worker. beta: job. Both autostart.
fn setup_workspace(tag: &str) -> Result<Workspace> {
    let root = std::env::temp_dir().join(format!("xk-xtask-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["apps", "logs", "app-alpha", "app-beta"] {
        std::fs::create_dir_all(root.join(d)).context("create workspace dirs")?;
    }
    let control_port = free_port()?;
    let webui_port = free_port()?;
    // Forward slashes in the TOML basic string: a raw Windows path would
    // put invalid `\U`-style escapes into the config.
    let log_dir = root.join("logs").to_string_lossy().replace('\\', "/");
    // The web console is config-driven: the `[webui]` section enables it and
    // pins its port for the browser pass.
    std::fs::write(
        root.join("daemon.toml"),
        format!(
            "[daemon]\nhost = \"127.0.0.1\"\nport = {control_port}\nlog_dir = \"{log_dir}\"\nlog_level = \"warn\"\nmonitor_interval = 0.5\n\n[webui]\nlisten = \"127.0.0.1:{webui_port}\"\n",
        ),
    )?;
    std::fs::write(
        root.join("app-alpha/xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.web]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n\n[program.worker]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n",
    )?;
    std::fs::write(
        root.join("app-beta/xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.job]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n",
    )?;
    link_registration(
        &root.join("app-alpha/xkeeper.toml"),
        &root.join("apps/alpha.toml"),
    )?;
    link_registration(
        &root.join("app-beta/xkeeper.toml"),
        &root.join("apps/beta.toml"),
    )?;
    Ok(Workspace {
        root,
        control_port,
        webui_port,
    })
}

/// Register a config as `app_dir/<name>.toml` — symlink where available,
/// hard-link fallback (unprivileged Windows), mirroring registry::make_link.
fn link_registration(target: &Path, link: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)?;
    }
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_file(target, link).is_err() {
            std::fs::hard_link(target, link)?;
        }
    }
    Ok(())
}

// -- daemon lifecycle --------------------------------------------------------

struct Daemon {
    child: Child,
}

fn spawn_daemon(bin: &Path, ws: &Workspace) -> Result<Daemon> {
    // The daemon must not inherit our stdio (see stress.rs for the reasons),
    // but its output lands in workspace files so a crash leaves evidence.
    let out = std::fs::File::create(ws.root.join("daemon.out.log"))?;
    let err = out.try_clone()?;
    let mut child = Command::new(bin)
        .arg("--config")
        .arg(ws.root.join("daemon.toml"))
        .arg("run")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .context("spawn daemon")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("daemon did not become healthy in 20s");
        }
        if let Ok(Some(_)) = child.try_wait() {
            bail!("daemon exited during startup");
        }
        let ok = Command::new("curl")
            .args([
                "-sf",
                &format!("http://127.0.0.1:{}/api/health", ws.webui_port),
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Ok(Daemon { child })
}

fn shutdown_daemon(ws: &Workspace, daemon: &mut Daemon) {
    let _ = Command::new("curl")
        .args([
            "-sf",
            "-m",
            "10",
            "-X",
            "POST",
            &format!("http://127.0.0.1:{}/v1/shutdown", ws.control_port),
        ])
        .output();
    for _ in 0..60 {
        if matches!(daemon.child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
}

// -- helpers -------------------------------------------------------------------

fn cli(bin: &Path, ws: &Workspace, args: &[&str]) -> Result<String> {
    let out = Command::new(bin)
        .arg("--config")
        .arg(ws.config_arg())
        .args(args)
        .output()
        .with_context(|| format!("run xkeeper {:?}", args))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        bail!(
            "xkeeper {:?} failed ({}): {}{}",
            args,
            out.status,
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(stdout)
}

/// Run the CLI expecting FAILURE; returns (success, exit code, stdout+stderr).
fn cli_try(bin: &Path, ws: &Workspace, args: &[&str]) -> Result<(bool, i32, String)> {
    let out = Command::new(bin)
        .arg("--config")
        .arg(ws.config_arg())
        .args(args)
        .output()
        .with_context(|| format!("run xkeeper {:?}", args))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let code = out.status.code().unwrap_or(-1);
    Ok((out.status.success(), code, combined))
}

/// Run the CLI against an explicit daemon config (for bad-config scenarios
/// where the workspace's own daemon.toml must stay valid), expecting FAILURE.
fn cli_try_cfg(bin: &Path, cfg: &Path, args: &[&str]) -> Result<(bool, i32, String)> {
    let out = Command::new(bin)
        .arg("--config")
        .arg(cfg)
        .args(args)
        .output()
        .with_context(|| format!("run xkeeper {:?} (cfg {})", args, cfg.display()))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let code = out.status.code().unwrap_or(-1);
    Ok((out.status.success(), code, combined))
}

/// Run the CLI against an explicit daemon config, expecting SUCCESS.
fn cli_cfg(bin: &Path, cfg: &Path, args: &[&str]) -> Result<String> {
    let (ok, _code, combined) = cli_try_cfg(bin, cfg, args)?;
    if !ok {
        bail!(
            "xkeeper {:?} (cfg {}) failed: {combined}",
            args,
            cfg.display()
        );
    }
    Ok(combined)
}

/// HTTP status code + body of a control-plane request (curl, like `api`,
/// but the status code is kept so error mappings can be asserted).
fn api_checked(ws: &Workspace, method: &str, path: &str, body: &str) -> Result<(u16, String)> {
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-o", "-", "-w", "\n%{http_code}", "-X", method]);
    if !body.is_empty() {
        cmd.args(["-d", body]);
    }
    cmd.arg(format!("http://127.0.0.1:{}{path}", ws.control_port));
    let out = cmd.output().context("curl control plane")?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    Ok((code.trim().parse().unwrap_or(0), body.to_string()))
}

/// A `sleep` executable for the scaffold scenarios: standard locations on
/// unix, PATH search elsewhere (Git for Windows ships one).
fn find_sleep() -> Result<PathBuf> {
    if cfg!(unix) {
        for p in ["/usr/bin/sleep", "/bin/sleep"] {
            if Path::new(p).is_file() {
                return Ok(PathBuf::from(p));
            }
        }
    }
    let exe = if cfg!(windows) { "sleep.exe" } else { "sleep" };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join(exe);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    bail!("no `sleep` executable found — needed by the scaffold e2e scenario");
}

fn api(ws: &Workspace, path: &str) -> Result<String> {
    let out = Command::new("curl")
        .args([
            "-sf",
            &format!("http://127.0.0.1:{}{path}", ws.control_port),
        ])
        .output()
        .context("curl control plane")?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// pid of a program via the control plane (None = not running or unknown).
fn pid_of(ws: &Workspace, program: &str) -> Result<Option<u32>> {
    let out = Command::new("curl")
        .args([
            "-s",
            "-o",
            "-",
            "-w",
            "\n%{http_code}",
            &format!("http://127.0.0.1:{}/v1/programs/{program}", ws.control_port),
        ])
        .output()
        .context("curl program")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, "000"));
    if code.trim() == "404" {
        return Ok(None);
    }
    if !code.trim().starts_with('2') {
        bail!("GET {program} -> {code}: {body}");
    }
    let v: serde_json::Value = serde_json::from_str(body)?;
    Ok(v.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32))
}

fn wait_running(ws: &Workspace, program: &str) -> Result<u32> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(pid) = pid_of(ws, program)? {
            return Ok(pid);
        }
        if Instant::now() > deadline {
            bail!("{program} never reached running");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn wait_pending_empty(ws: &Workspace) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let v = api(ws, "/v1/pending")?;
        let v: serde_json::Value = serde_json::from_str(&v)?;
        if v.get("programs")
            .and_then(|p| p.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true)
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("pending never cleared: {v}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn step(name: &str) {
    println!("\n==> {name}");
}

fn expect(cond: bool, what: &str) -> Result<()> {
    if !cond {
        bail!("assertion failed: {what}");
    }
    println!("    ok: {what}");
    Ok(())
}

// -- offline pass ----------------------------------------------------------------

/// Runs BEFORE the daemon exists: an explicit `add --apply` that did not take
/// effect must be visible (exit 3 + remedy, config kept), and `--env` parse
/// errors must exit with the config code before anything is registered.
fn offline_pass(bin: &Path, ws: &Workspace) -> Result<()> {
    let sleep = find_sleep()?;

    step("G0. offline `add --apply` fails loudly: exit 3, config path + remedy");
    let (ok, code, combined) = cli_try(
        bin,
        ws,
        &[
            "add",
            sleep.to_str().unwrap(),
            "--name",
            "offline-demo",
            "--apply",
        ],
    )?;
    expect(!ok, "offline add --apply fails")?;
    expect(
        code == 3,
        &format!("exit code is 3 (daemon unreachable), got {code}"),
    )?;
    expect(
        combined.contains("offline"),
        "error names the offline cause",
    )?;
    expect(
        combined.contains("offline-demo.toml"),
        "error names the generated config file",
    )?;
    expect(
        combined.contains("xkeeper apply offline-demo"),
        "error names the remedy",
    )?;
    expect(
        ws.root.join("apps/offline-demo.toml").is_file(),
        "the generated config is kept for the retry",
    )?;

    step("G0b. `--env` without '=' / with an empty key exits 2 (config error)");
    let (ok, code, combined) = cli_try(
        bin,
        ws,
        &[
            "add",
            sleep.to_str().unwrap(),
            "--name",
            "env-bad",
            "--env",
            "NOEQUALS",
        ],
    )?;
    expect(
        !ok && code == 2,
        &format!("--env missing '=' exits 2, got {code}"),
    )?;
    expect(combined.contains("--env"), "error names the offending flag")?;
    expect(
        !ws.root.join("apps/env-bad.toml").exists(),
        "bad --env registers nothing",
    )?;
    let (ok, code, _) = cli_try(
        bin,
        ws,
        &[
            "add",
            sleep.to_str().unwrap(),
            "--name",
            "env-bad",
            "--env",
            "=v",
        ],
    )?;
    expect(
        !ok && code == 2,
        &format!("empty --env key exits 2, got {code}"),
    )?;
    expect(
        !ws.root.join("apps/env-bad.toml").exists(),
        "empty --env key registers nothing",
    )?;

    step("G0c. `xkeeper status` with no daemon is unreachable (exit 3)");
    let (ok, code, combined) = cli_try(bin, ws, &["status"])?;
    expect(
        !ok && code == 3,
        &format!("status without a daemon exits 3, got {code}"),
    )?;
    expect(
        combined.contains("unreachable"),
        "the error says the daemon is unreachable",
    )?;

    step("G0c2. removed webui/system subcommands stay removed (BREAKING, config-driven-webui)");
    for bad in [&["webui"][..], &["system", "webui"][..]] {
        let (ok, _code, combined) = cli_try(bin, ws, bad)?;
        expect(
            !ok && combined.contains("unrecognized subcommand"),
            &format!("{bad:?} must be rejected as an unknown subcommand: {combined:?}"),
        )?;
    }

    step("G0c3. `shell -e status` with no daemon exits 3 (shell 纳入退出码约定)");
    let (ok, code, combined) = cli_try(bin, ws, &["shell", "-e", "status"])?;
    expect(
        !ok && code == 3,
        &format!("shell -e status without a daemon exits 3, got {code}"),
    )?;
    expect(
        combined.contains("unreachable"),
        "the shell reports the daemon as unreachable",
    )?;

    step("G0d. a relative daemon.log_dir fails validate AND refuses daemon startup");
    let bad_cfg = ws.root.join("daemon-bad.toml");
    std::fs::write(&bad_cfg, "[daemon]\nlog_dir = \"logs\"\n")?;
    let (ok, _code, combined) = cli_try_cfg(bin, &bad_cfg, &["validate"])?;
    expect(!ok, "validate with a relative log_dir fails")?;
    expect(
        combined.contains("daemon.log_dir") && combined.contains("absolute"),
        "the error names daemon.log_dir and demands an absolute path",
    )?;
    // The daemon itself must refuse to boot on that config (fast, non-zero).
    let out = std::fs::File::create(ws.root.join("bad-daemon.out.log"))?;
    let err = out.try_clone()?;
    let mut child = Command::new(bin)
        .arg("--config")
        .arg(&bad_cfg)
        .arg("run")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .context("spawn daemon with relative log_dir")?;
    let mut refused = false;
    for _ in 0..50 {
        match child.try_wait()? {
            Some(st) => {
                refused = !st.success();
                break;
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    if !refused {
        let _ = child.kill();
        let _ = child.wait();
    }
    expect(
        refused,
        "the daemon exits non-zero instead of starting (relative log_dir)",
    )?;
    let refusal = std::fs::read_to_string(ws.root.join("bad-daemon.out.log"))?;
    expect(
        refusal.contains("daemon.log_dir") && refusal.contains("absolute"),
        "the startup refusal names daemon.log_dir and the fix",
    )?;

    step("G0d2. an unknown [webui] key fails validate (whole-config rejection)");
    let bad_webui = ws.root.join("daemon-webui-bad.toml");
    std::fs::write(&bad_webui, "[webui]\nauth = true\n")?;
    let (ok, _code, combined) = cli_try_cfg(bin, &bad_webui, &["validate"])?;
    expect(!ok, "validate rejects an unknown [webui] key")?;
    expect(
        combined.contains("auth") && combined.contains("unknown field"),
        "the error names the unknown field: {combined:?}",
    )?;

    // Clean the offline registration so the daemon pass starts clean.
    let out = cli(bin, ws, &["remove", "offline-demo"])?;
    expect(
        out.contains("generated config file removed"),
        "offline remove deletes the generated file",
    )?;
    Ok(())
}

// -- config subcommand pass (add-config-subcommand) -------------------------------

/// Offline scenarios for `xkeeper config` (runs BEFORE the daemon exists, so
/// write actions report the offline note). Uses its own config paths under
/// `cfgcmd/` so the workspace's daemon.toml stays untouched for the daemon
/// pass. Covers: init → get defaults → set → get file values → illegal sets
/// rejected with the file unchanged → delete falls back to defaults →
/// idempotent delete → unknown-key delete rejected → init-overwrite rejected
/// → set on a missing file refused → sample not scanned as an app →
/// `config --edit` through a fake $EDITOR (valid + invalid).
fn config_pass(bin: &Path, ws: &Workspace) -> Result<()> {
    let dir = ws.root.join("cfgcmd");
    let cfg = dir.join("daemon.toml");

    step("K1. config --init creates defaults + comments + app dir + sample");
    let out = cli_cfg(bin, &cfg, &["config", "--init"])?;
    expect(cfg.is_file(), "daemon.toml created")?;
    let text = std::fs::read_to_string(&cfg)?;
    for key in [
        "log_level",
        "log_dir",
        "monitor_interval",
        "host",
        "port",
        "auth_token",
        "log_buffer_lines",
        "app_dir",
    ] {
        expect(
            text.contains(key),
            &format!("the template renders the known key {key}"),
        )?;
    }
    expect(
        text.contains("port = 7310") && text.contains("host = \"127.0.0.1\""),
        "defaults are rendered with values",
    )?;
    expect(
        text.contains("auth_token = \"\""),
        "auth_token renders empty",
    )?;
    expect(
        text.contains("# [app-default]"),
        "a fully commented [app-default] template section exists",
    )?;
    expect(dir.join("apps").is_dir(), "the app registry dir is created")?;
    let sample = dir.join("apps/example.toml.sample");
    expect(sample.is_file(), "example.toml.sample is created")?;
    expect(out.contains("next:"), "init prints the next-step hint")?;

    step("K2. config --get: defaults for unset keys, bare value / key=value");
    let out = cli_cfg(bin, &cfg, &["config", "--get", "port"])?;
    expect(
        out.trim() == "7310",
        &format!("a single key prints the bare value: {out:?}"),
    )?;
    let out = cli_cfg(bin, &cfg, &["config", "--get", "port", "--get", "host"])?;
    expect(
        out.contains("port=7310") && out.contains("host=127.0.0.1"),
        &format!("multiple keys print key=value lines: {out:?}"),
    )?;

    step("K3. config --init refuses to overwrite an existing config");
    let before = std::fs::read_to_string(&cfg)?;
    let (ok, code, combined) = cli_try_cfg(bin, &cfg, &["config", "--init"])?;
    expect(
        !ok && code == 2,
        &format!("init over an existing file exits 2, got {code}"),
    )?;
    expect(
        combined.contains("already exists"),
        "the error explains the refusal",
    )?;
    expect(
        std::fs::read_to_string(&cfg)? == before,
        "the existing config was not touched",
    )?;

    step("K4. config --set writes strong-typed keys (offline note printed)");
    let out = cli_cfg(
        bin,
        &cfg,
        &["config", "--set", "port=8080", "--set", "log_level=debug"],
    )?;
    expect(out.contains("port"), "set reports the written keys: {out}")?;
    expect(
        out.contains("offline"),
        "the offline note says the change takes effect at next start",
    )?;
    expect(
        out.contains("set port = 8080") && out.contains("set log_level = debug"),
        "set prints the written keys with their new values",
    )?;
    let out = cli_cfg(bin, &cfg, &["config", "--get", "port"])?;
    expect(out.trim() == "8080", "the file value wins over the default")?;
    let out = cli_cfg(bin, &cfg, &["config", "--get", "log_level"])?;
    expect(out.trim() == "debug", "log_level was written")?;

    step("K5. --set keeps comments, key order and untouched values");
    std::fs::write(
        &cfg,
        "# my header\n[daemon]\n# keep me\nlog_level = \"info\"\nlog_buffer_lines = 7 # count\nport = 1234 # trailing\n",
    )?;
    cli_cfg(bin, &cfg, &["config", "--set", "port=8081"])?;
    let text = std::fs::read_to_string(&cfg)?;
    for snippet in [
        "# my header",
        "# keep me",
        "log_level = \"info\"",
        "log_buffer_lines = 7 # count",
    ] {
        expect(
            text.contains(snippet),
            &format!("untouched content survives: {snippet:?}"),
        )?;
    }
    expect(
        text.contains("port = 8081 # trailing"),
        "the touched key's trailing comment survives",
    )?;

    step("K6. illegal sets are rejected with the file unchanged");
    let before = std::fs::read_to_string(&cfg)?;
    for bad in [
        "port=abc",
        "port=0",
        "port=65536",
        "foo=1",
        "log_level=verbose",
        "monitor_interval=0",
        "log_dir=relative/logs", // parses, fails whole-config validation
    ] {
        let (ok, code, _combined) = cli_try_cfg(bin, &cfg, &["config", "--set", bad])?;
        expect(
            !ok && code == 2,
            &format!("--set {bad} is rejected with exit 2"),
        )?;
    }
    expect(
        std::fs::read_to_string(&cfg)? == before,
        "rejected sets leave the file byte-identical",
    )?;

    step("K7. --set on a missing file is refused with the --init hint");
    let fresh = ws.root.join("cfgcmd-fresh.toml");
    let (ok, code, combined) = cli_try_cfg(bin, &fresh, &["config", "--set", "port=1"])?;
    expect(
        !ok && code == 2,
        &format!("set without a config exits 2, got {code}"),
    )?;
    expect(
        combined.contains("config --init"),
        "the error points at `xkeeper config --init`",
    )?;
    expect(!fresh.exists(), "nothing was created by the refused set")?;

    step("K8. --delete removes the key, the default applies again");
    cli_cfg(bin, &cfg, &["config", "--delete", "port"])?;
    let out = cli_cfg(bin, &cfg, &["config", "--get", "port"])?;
    expect(
        out.trim() == "7310",
        "the deleted key answers with the default",
    )?;
    let text = std::fs::read_to_string(&cfg)?;
    expect(!text.contains("port ="), "the port line is gone")?;
    expect(
        text.contains("log_buffer_lines = 7 # count"),
        "untouched keys survive the delete",
    )?;

    step("K9. --delete of an absent key is an idempotent success");
    let before = std::fs::read_to_string(&cfg)?;
    let out = cli_cfg(bin, &cfg, &["config", "--delete", "host"])?;
    expect(
        out.contains("no change"),
        "the output explains nothing changed",
    )?;
    expect(
        std::fs::read_to_string(&cfg)? == before,
        "the file was not rewritten",
    )?;

    step("K10. unknown keys/tables are refused");
    for bad in ["foo", "daemon.nope", "webui.theme", "webui.listen.x"] {
        let (ok, code, combined) = cli_try_cfg(bin, &cfg, &["config", "--delete", bad])?;
        expect(!ok && code == 2, &format!("--delete {bad} exits 2"))?;
        expect(
            combined.contains("unknown"),
            "the error says the key/table is unknown",
        )?;
    }
    expect(
        std::fs::read_to_string(&cfg)? == before,
        "rejected deletes leave the file unchanged",
    )?;

    // K16: webui keys ride the same engine (config-driven-webui).
    step("K16a. --set webui.listen writes a [webui] section; illegal listen is refused");
    let (ok, code, combined) =
        cli_try_cfg(bin, &cfg, &["config", "--set", "webui.listen=not-an-addr"])?;
    expect(!ok && code == 2, "an illegal listen exits 2 untouched")?;
    expect(
        combined.contains("webui.listen") && std::fs::read_to_string(&cfg)? == before,
        "the error names webui.listen and the file is byte-identical",
    )?;
    let out = cli_cfg(
        bin,
        &cfg,
        &[
            "config",
            "--set",
            "webui.listen=127.0.0.1:19877",
        ],
    )?;
    expect(out.contains("webui.listen"), "set reports the written key")?;
    let text = std::fs::read_to_string(&cfg)?;
    expect(
        text.contains("[webui]") && text.contains("listen = \"127.0.0.1:19877\""),
        "the [webui] section was created with the listen value",
    )?;

    step("K16b. --get webui.listen answers the file value; default when unset");
    let out = cli_cfg(bin, &cfg, &["config", "--get", "webui.listen"])?;
    expect(out.trim() == "127.0.0.1:19877", "get reads the section")?;
    let other = dir.join("daemon-empty.toml");
    std::fs::write(&other, "[daemon]\nport = 7310\n")?;
    let out = cli_cfg(bin, &other, &["config", "--get", "webui.listen"])?;
    expect(
        out.trim() == "127.0.0.1:9877",
        "an absent section answers the built-in default",
    )?;

    step("K16c. --delete webui removes the whole section (console off)");
    let out = cli_cfg(bin, &cfg, &["config", "--delete", "webui"])?;
    let text = std::fs::read_to_string(&cfg)?;
    expect(!text.contains("[webui]"), "the section is gone")?;
    expect(out.contains("webui"), "delete reports the removed table")?;
    // Idempotent second delete of the absent table.
    let (ok, code, _) = cli_try_cfg(bin, &cfg, &["config", "--delete", "webui"])?;
    expect(ok && code == 0, "an absent table delete is an idempotent success")?;

    step("K11. config --edit through a fake $EDITOR (valid + invalid)");
    if cfg!(unix) {
        let script = dir.join("fake-editor.sh");
        // Valid edit: the config passes validation afterwards.
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '[daemon]\\nport = 9999\\n' > \"$1\"\n",
        )?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let out = Command::new(bin)
            .env("EDITOR", &script)
            .arg("--config")
            .arg(&cfg)
            .args(["config", "--edit"])
            .output()
            .context("run config --edit (valid)")?;
        expect(out.status.success(), "a valid edit exits 0")?;
        let out = cli_cfg(bin, &cfg, &["config", "--get", "port"])?;
        expect(out.trim() == "9999", "the edited value is effective")?;
        // Invalid edit: exit 2 and the file keeps the editor's content.
        std::fs::write(&script, "#!/bin/sh\nprintf 'bogus_field = 1\\n' > \"$1\"\n")?;
        let out = Command::new(bin)
            .env("EDITOR", &script)
            .arg("--config")
            .arg(&cfg)
            .args(["config", "--edit"])
            .output()
            .context("run config --edit (invalid)")?;
        expect(
            out.status.code() == Some(2),
            &format!(
                "an invalid edit result exits 2, got {:?}",
                out.status.code()
            ),
        )?;
        let text = std::fs::read_to_string(&cfg)?;
        expect(
            text.contains("bogus_field"),
            "the file keeps the editor's content (no rollback)",
        )?;
        // Restore a legal config for the following scenario.
        std::fs::write(&cfg, "[daemon]\nport = 9999\n")?;
    } else {
        println!("    (windows: the fake-EDITOR edit scenario requires unix; skipping)");
    }

    step("K12. example.toml.sample is not scanned as an app");
    let out = cli_cfg(bin, &cfg, &["list"])?;
    expect(
        out.contains("no apps registered"),
        "the init-produced app_dir lists no apps (.sample ignored)",
    )?;

    step("K13. bare `xkeeper config` prints the five-action usage and exits 0");
    let out = cli_cfg(bin, &cfg, &["config"])?;
    for flag in ["--set", "--get", "--delete", "--edit", "--init"] {
        expect(
            out.contains(flag),
            &format!("the usage mentions {flag}: {out}"),
        )?;
    }

    step("K14. unknown --get key exits 2; --delete on a missing file points at --init");
    let (ok, code, combined) = cli_try_cfg(bin, &cfg, &["config", "--get", "foo"])?;
    expect(!ok && code == 2, &format!("--get foo exits 2, got {code}"))?;
    expect(
        combined.contains("unknown"),
        "the error names the key as unknown",
    )?;
    let missing = ws.root.join("cfgcmd-missing.toml");
    let (ok, code, combined) = cli_try_cfg(bin, &missing, &["config", "--delete", "port"])?;
    expect(
        !ok && code == 2,
        &format!("delete without a config exits 2, got {code}"),
    )?;
    expect(
        combined.contains("config --init"),
        "the error points at `xkeeper config --init`",
    )?;
    expect(!missing.exists(), "nothing was created by the refused delete")?;

    step("K15. $VISUAL wins over $EDITOR for config --edit");
    if cfg!(unix) {
        let visual = dir.join("visual-editor.sh");
        let editor = dir.join("env-editor.sh");
        std::fs::write(&visual, "#!/bin/sh\nprintf '[daemon]\\nport = 9111\\n' > \"$1\"\n")?;
        std::fs::write(&editor, "#!/bin/sh\nprintf '[daemon]\\nport = 9222\\n' > \"$1\"\n")?;
        use std::os::unix::fs::PermissionsExt;
        for s in [&visual, &editor] {
            std::fs::set_permissions(s, std::fs::Permissions::from_mode(0o755))?;
        }
        let out = Command::new(bin)
            .env("VISUAL", &visual)
            .env("EDITOR", &editor)
            .arg("--config")
            .arg(&cfg)
            .args(["config", "--edit"])
            .output()
            .context("run config --edit with both $VISUAL and $EDITOR")?;
        expect(out.status.success(), "the edit through $VISUAL exits 0")?;
        let out = cli_cfg(bin, &cfg, &["config", "--get", "port"])?;
        expect(
            out.trim() == "9111",
            &format!("$VISUAL won over $EDITOR (9111, not 9222): {out:?}"),
        )?;
    } else {
        println!("    (windows: the $VISUAL-precedence scenario requires unix; skipping)");
    }
    Ok(())
}

// -- CLI pass -------------------------------------------------------------------

fn cli_pass(bin: &Path, ws: &Workspace) -> Result<()> {
    // Sanity: both apps up.
    let web_pid = wait_running(ws, "web")?;
    let worker_pid = wait_running(ws, "worker")?;
    wait_running(ws, "job")?;

    step("E. no-change apply is idempotent (exit 0, no restarts)");
    let out = cli(bin, ws, &["apply"])?;
    expect(out.contains("no changes"), "empty apply reports no changes")?;
    expect(pid_of(ws, "web")? == Some(web_pid), "web was not touched")?;

    step("A. disk edit only enters pending (no process action)");
    std::fs::write(
        ws.root.join("app-alpha/xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.web]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n\n[program.worker]\ncommand = \"sleep 200000\"\nstartsecs = 0.2\n",
    )?;
    // Periodic detection window (monitor_interval 0.5s).
    std::thread::sleep(Duration::from_secs(2));
    let v: serde_json::Value = serde_json::from_str(&api(ws, "/v1/pending")?)?;
    let progs = v
        .get("programs")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    expect(
        progs.len() == 1 && progs[0].get("program").and_then(|p| p.as_str()) == Some("worker"),
        "periodic detect published worker as changed (no manual reload)",
    )?;
    expect(
        pid_of(ws, "worker")? == Some(worker_pid),
        "worker still runs the old definition",
    )?;
    let out = cli(bin, ws, &["reload"])?;
    expect(
        out.contains("worker") && out.contains("run `xkeeper apply`"),
        "reload prints a preview, applies nothing",
    )?;
    expect(
        pid_of(ws, "worker")? == Some(worker_pid),
        "reload left worker alone",
    )?;

    step("B. scoped apply leaves other apps untouched");
    // Give beta a pending change too.
    std::fs::write(
        ws.root.join("app-beta/xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.job]\ncommand = \"sleep 300000\"\nstartsecs = 0.2\n",
    )?;
    let job_pid = pid_of(ws, "job")?.expect("job running");
    std::thread::sleep(Duration::from_secs(2));
    let out = cli(bin, ws, &["apply", "alpha"])?;
    expect(
        out.contains("update-and-restart"),
        "alpha apply rebuilt+restarted the changed worker",
    )?;
    let new_worker = wait_running(ws, "worker")?;
    expect(new_worker != worker_pid, "worker restarted (pid changed)")?;
    expect(
        pid_of(ws, "job")? == Some(job_pid),
        "beta job untouched by alpha-scoped apply",
    )?;
    let v: serde_json::Value = serde_json::from_str(&api(ws, "/v1/pending")?)?;
    let progs = v
        .get("programs")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    expect(
        progs.len() == 1 && progs[0].get("program").and_then(|p| p.as_str()) == Some("job"),
        "beta's pending survived the alpha apply",
    )?;

    step("B2. shell pending + scoped apply mirror the CLI with real pending");
    let out = cli(bin, ws, &["shell", "-e", "pending"])?;
    expect(
        out.contains("pending changes") && out.contains("job"),
        "shell pending lists the changed program",
    )?;
    let out = cli(bin, ws, &["shell", "-e", "apply beta"])?;
    expect(
        out.contains("update-and-restart") && out.contains("job"),
        "shell scoped apply reports the per-program result",
    )?;
    wait_running(ws, "job")?;

    step("C. --restart restarts unchanged programs but keeps user-stopped ones");
    cli(bin, ws, &["stop", "web"])?;
    let out = cli(bin, ws, &["apply", "--restart"])?;
    expect(
        out.contains("restarted (--restart)"),
        "--restart restarted unchanged programs",
    )?;
    expect(
        out.contains("keep-stopped"),
        "--restart reported the user-stopped program as kept",
    )?;
    let v: serde_json::Value = serde_json::from_str(&api(ws, "/v1/programs/web")?)?;
    expect(
        v.get("state").and_then(|s| s.as_str()) == Some("stopped"),
        "user-stopped web stayed stopped under --restart",
    )?;
    // job (unchanged, running) was restarted by --restart; catch its new pid.
    let job2 = pid_of(ws, "job")?;
    expect(
        job2.is_some() && job2 != Some(job_pid),
        "job was restarted by --restart",
    )?;

    step("D. online add enters pending and starts on apply");
    let gamma = ws.root.join("app-gamma");
    std::fs::create_dir_all(&gamma)?;
    std::fs::write(
        gamma.join("xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.gamma]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n",
    )?;
    let out = cli(
        bin,
        ws,
        &["add", gamma.to_str().unwrap(), "--name", "gamma"],
    )?;
    expect(
        out.contains("registration is pending"),
        "add sync reports pending (not immediate start)",
    )?;
    expect(
        pid_of(ws, "gamma")?.is_none(),
        "gamma not started before apply",
    )?;
    let out = cli(bin, ws, &["apply"])?;
    expect(out.contains("gamma"), "apply picked the new app up")?;
    wait_running(ws, "gamma")?;

    step("shell: pending + apply single-command mode");
    let out = cli(bin, ws, &["shell", "-e", "pending"])?;
    expect(
        out.contains("no pending changes"),
        "shell pending is clean after full apply",
    )?;
    let out = cli(bin, ws, &["shell", "-e", "apply alpha"])?;
    expect(out.contains("no changes"), "shell scoped apply idempotent")?;

    scaffold_pass(bin, ws)?;

    // Re-sync disk with the running definitions so the browser pass starts
    // from a clean slate (worker currently runs sleep 200000 after step B).
    std::fs::write(
        ws.root.join("app-alpha/xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.web]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n\n[program.worker]\ncommand = \"sleep 200000\"\nstartsecs = 0.2\n",
    )?;
    wait_pending_empty(ws)
}

/// Scenario G (add-process-scaffold): generate from an executable, apply,
/// regenerate, `--apply`, the `all` reserved word, and remove-file semantics.
fn scaffold_pass(bin: &Path, ws: &Workspace) -> Result<()> {
    let sleep = find_sleep()?;
    let apps = ws.root.join("apps");

    step("G1. add scaffold generates a real config with absolute paths");
    let out = cli(
        bin,
        ws,
        &[
            "add",
            sleep.to_str().unwrap(),
            "--name",
            "abc",
            "--args",
            "100000",
            "--env",
            "XK_E2E=1",
        ],
    )?;
    expect(out.contains("generated"), "add prints the generated file")?;
    expect(
        out.contains("pending"),
        "online sync reports the pending registration",
    )?;
    expect(
        out.contains("changed"),
        "first generation is announced as changed",
    )?;
    // Full non-interactive summary (app-registry: 注册与再生成功后完整打印).
    expect(
        out.contains("app[abc]") && out.contains("program[abc]"),
        "summary names the app and the program",
    )?;
    expect(
        out.contains("command:") && out.contains(sleep.to_string_lossy().as_ref()),
        "summary prints the absolute command",
    )?;
    expect(
        out.contains("args:") && out.contains("100000"),
        "summary prints the args",
    )?;
    expect(
        out.contains("env:") && out.contains("XK_E2E=1"),
        "summary prints the env",
    )?;
    expect(out.contains("work_dir:"), "summary prints the work_dir")?;
    let abc_file = apps.join("abc.toml");
    expect(abc_file.is_file(), "apps/abc.toml is a real file")?;
    let abc_text = std::fs::read_to_string(&abc_file)?;
    expect(
        abc_text.contains("XK_E2E"),
        "env written into the generated file",
    )?;
    // toml emits Windows paths as single-quoted literal strings; accept both
    // quote styles when extracting the value.
    let wd = abc_text
        .split("work_dir")
        .nth(1)
        .and_then(|rest| {
            let open = rest.find(['"', '\''])?;
            let rest = &rest[open + 1..];
            rest.find(['"', '\'']).map(|end| &rest[..end])
        })
        .unwrap_or_default();
    expect(
        wd.starts_with('/') || (wd.len() > 2 && wd.as_bytes()[1] == b':'),
        &format!("work_dir in the file is absolute: {wd:?}"),
    )?;
    // Round-trip through config load: the generated file must validate and
    // the command must have been written as an absolute path.
    let generated = cli(bin, ws, &["validate", &abc_file.to_string_lossy()])?;
    expect(
        generated.contains("abc") && generated.contains("program(s)"),
        "generated config passes validate",
    )?;
    expect(
        generated.contains(sleep.to_string_lossy().as_ref()),
        "validated command is the absolute executable path",
    )?;
    expect(pid_of(ws, "abc")?.is_none(), "abc not started before apply")?;
    // list scans app_dir and shows the scaffolded record like any other.
    let out = cli(bin, ws, &["list"])?;
    expect(out.contains("abc"), "list shows the scaffolded app name")?;
    expect(
        out.contains("abc.toml"),
        "list shows the scaffolded record path",
    )?;

    step("G2. apply abc starts the scaffolded program");
    let out = cli(bin, ws, &["apply", "abc"])?;
    expect(out.contains("abc"), "scoped apply picked up abc")?;
    let abc_pid = wait_running(ws, "abc")?;

    step("G3. identical re-add reports no change");
    let out = cli(
        bin,
        ws,
        &[
            "add",
            sleep.to_str().unwrap(),
            "--name",
            "abc",
            "--args",
            "100000",
            "--env",
            "XK_E2E=1",
        ],
    )?;
    expect(out.contains("no change"), "identical re-add is a no change")?;
    expect(
        pid_of(ws, "abc")? == Some(abc_pid),
        "no-change re-add does not touch the running program",
    )?;

    step("G4. changed re-add, a second pending app, then `apply all` takes both");
    let out = cli(
        bin,
        ws,
        &[
            "add",
            sleep.to_str().unwrap(),
            "--name",
            "abc",
            "--args",
            "200000",
        ],
    )?;
    expect(
        out.contains("changed") && out.contains("xkeeper apply abc"),
        "changed re-add points at the remedy",
    )?;
    let out = cli(
        bin,
        ws,
        &[
            "add",
            sleep.to_str().unwrap(),
            "--name",
            "fox",
            "--args",
            "100000",
        ],
    )?;
    expect(out.contains("changed"), "fox first generation is changed")?;
    let out = cli(bin, ws, &["apply", "all"])?;
    expect(out.contains("abc"), "`apply all` applied abc's change")?;
    expect(
        out.contains("fox"),
        "`apply all` also applied the other pending app (full-registry scope)",
    )?;
    let abc_pid2 = wait_running(ws, "abc")?;
    expect(abc_pid2 != abc_pid, "abc restarted with the new args")?;
    wait_running(ws, "fox")?;

    step("G5. add --apply is one step and app-scoped (other pending survives)");
    // Give alpha a pending change; delta's app-scoped --apply must not touch it.
    let worker_before = pid_of(ws, "worker")?;
    std::fs::write(
        ws.root.join("app-alpha/xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.web]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n\n[program.worker]\ncommand = \"sleep 300000\"\nstartsecs = 0.2\n",
    )?;
    cli(bin, ws, &["reload"])?; // publish the pending preview
    let out = cli(
        bin,
        ws,
        &["add", sleep.to_str().unwrap(), "--name", "delta", "--apply"],
    )?;
    expect(out.contains("generated"), "delta scaffold generated")?;
    expect(out.contains("-> start"), "apply result shows delta started")?;
    wait_running(ws, "delta")?;
    let v: serde_json::Value = serde_json::from_str(&api(ws, "/v1/pending")?)?;
    let progs = v
        .get("programs")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    expect(
        progs
            .iter()
            .any(|p| p.get("program").and_then(|x| x.as_str()) == Some("worker")),
        "alpha's pending survived delta's app-scoped --apply",
    )?;
    expect(
        pid_of(ws, "worker")? == worker_before && worker_before.is_some(),
        "delta's --apply did not restart alpha's worker",
    )?;
    // Restore alpha's disk to the applied content so no pending leaks on.
    std::fs::write(
        ws.root.join("app-alpha/xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.web]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n\n[program.worker]\ncommand = \"sleep 200000\"\nstartsecs = 0.2\n",
    )?;
    cli(bin, ws, &["reload"])?;
    wait_pending_empty(ws)?;

    step("G6. `--name all` is rejected (reserved apply-all keyword)");
    let (ok, _code, combined) =
        cli_try(bin, ws, &["add", sleep.to_str().unwrap(), "--name", "all"])?;
    expect(!ok, "add --name all fails")?;
    expect(
        combined.contains("reserved"),
        "error explains the reserved word",
    )?;

    step("G7. remove deletes the generated file, keeps external bodies");
    let out = cli(bin, ws, &["remove", "abc"])?;
    expect(
        out.contains("generated config file removed"),
        "remove announces the deleted file",
    )?;
    expect(!abc_file.exists(), "apps/abc.toml is gone")?;
    let out = cli(bin, ws, &["remove", "fox"])?;
    expect(
        out.contains("generated config file removed"),
        "fox's generated file is deleted too",
    )?;
    expect(!apps.join("fox.toml").exists(), "apps/fox.toml is gone")?;
    // A link registration (gamma) is removed without touching the external
    // deployment file — whatever the message says, the body must survive
    // (on Windows the record is a hard link, so the message differs).
    let out = cli(bin, ws, &["remove", "gamma"])?;
    expect(out.contains("unregistered"), "gamma unregistered")?;
    expect(
        ws.root.join("app-gamma/xkeeper.toml").is_file(),
        "gamma's xkeeper.toml still exists",
    )?;
    // Apply the removals: the stopped programs disappear from the status.
    cli(bin, ws, &["apply"])?;
    expect(
        pid_of(ws, "abc")?.is_none(),
        "abc's program stopped and gone",
    )?;
    expect(
        pid_of(ws, "fox")?.is_none(),
        "fox's program stopped and gone",
    )?;
    Ok(())
}

// -- actions scenario group (actions + glossary change) ----------------------

/// Write the scenario app used by the actions pass: one long-running service
/// with a custom-action toolbox, a USR1/USR2-trapping program and a plain
/// sleeper, chained by dependencies so the fan-out order is observable.
/// The `xkeeper` binary path and daemon config path are baked into the
/// chained-restart action (the CLI reads connection info from the config).
fn setup_actions_app(ws: &Workspace, bin: &Path) -> Result<()> {
    let dir = ws.root.join("acts");
    std::fs::create_dir_all(&dir)?;
    let bin_s = bin.display().to_string().replace('\\', "/");
    let cfg_s = ws.config_arg().replace('\\', "/");
    let text = format!(
        r#"[app]
autostart = false

[program.svc]
command = "sleep 100000"
startsecs = 0.2

[program.svc.action.ping]
command = "echo svc_pid=${{program.svc.pid}} svc_state=${{program.svc.state}}"
timeout = 10

[program.svc.action.workdir_probe]
command = "pwd"
timeout = 5

[program.svc.action.slow]
command = "sleep 31"
timeout = 2

[program.svc.action.ring]
command = "sleep 4"
timeout = 30

[program.svc.action.chained]
command = "echo old_svc_pid=${{program.svc.pid}}; {bin} --config {cfg} restart svc; {bin} --config {cfg} pid svc"
timeout = 60

[program.svc.action.stopped_pid]
command = "echo stopped_svc_pid=[${{program.svc.pid}}]"
timeout = 10

[program.svc.action.multiline]
command = '''
echo ml-one
echo ml-two
'''
timeout = 10

[program.svc.action.fail3]
command = "exit 3"
timeout = 10

[program.signalee]
command = "sleep 100000"
startsecs = 0.2
autorestart = "never"
depends_on = ["svc"]

[program.trapper]
command = "sh"
args = ["-c", "trap '' USR1 USR2; sleep 100000"]
startsecs = 0.2
autorestart = "never"
depends_on = ["signalee"]

[program.logger]
command = "sh"
args = ["-c", "while true; do echo tick; sleep 0.2; done"]
startsecs = 0.2
autorestart = "never"
"#,
        bin = bin_s,
        cfg = cfg_s,
    );
    std::fs::write(dir.join("xkeeper.toml"), text)?;
    link_registration(&dir.join("xkeeper.toml"), &ws.root.join("apps/acts.toml"))?;
    Ok(())
}

/// Offline (daemon not running): declarations validate, bad ones are refused
/// with actionable errors — charset (breaking tighten), unknown action
/// variable, built-in reserved word, timeout range.
fn actions_offline_pass(bin: &Path, ws: &Workspace) -> Result<()> {
    step("H0. action declarations pass validate");
    let acts_file = ws.root.join("acts/xkeeper.toml");
    let out = cli(bin, ws, &["validate", &acts_file.to_string_lossy()])?;
    expect(
        out.contains("OK") && out.contains("program(s)"),
        "the acts config with 7 declared actions validates",
    )?;

    step("H1. illegal identifier names are refused (dots, spaces, non-ASCII)");
    let bad_dir = ws.root.join("badcase");
    std::fs::create_dir_all(&bad_dir)?;
    let bad_file = bad_dir.join("xkeeper.toml");
    std::fs::write(&bad_file, "[program.\"my.web\"]\ncommand = 'sleep 1'\n")?;
    let (ok, code, combined) = cli_try(bin, ws, &["validate", &bad_file.to_string_lossy()])?;
    expect(!ok, "a dotted program name fails validate")?;
    expect(code == 1, &format!("validate failure exits 1, got {code}"))?;
    expect(
        combined.contains("[A-Za-z0-9_-]"),
        "the error states the identifier charset",
    )?;
    std::fs::write(&bad_file, "[program.\"my web\"]\ncommand = 'sleep 1'\n")?;
    let (ok, _code, combined) = cli_try(bin, ws, &["validate", &bad_file.to_string_lossy()])?;
    expect(
        !ok && combined.contains("[A-Za-z0-9_-]"),
        "a spaced program name is refused",
    )?;
    let (ok, _code, combined) = cli_try(
        bin,
        ws,
        &["add", &acts_file.to_string_lossy(), "--name", "my.app"],
    )?;
    expect(
        !ok && combined.contains("[A-Za-z0-9_-]"),
        "`add --name my.app` is refused (dots are not identifiers)",
    )?;
    let (ok, _code, combined) = cli_try(
        bin,
        ws,
        &["add", &acts_file.to_string_lossy(), "--name", "bad name"],
    )?;
    expect(
        !ok && combined.contains("[A-Za-z0-9_-]"),
        "`add --name \"bad name\"` is refused (spaces are not identifiers)",
    )?;

    step("H2. unknown action variables are refused at validate time");
    std::fs::write(
        &bad_file,
        "[program.ok]\ncommand = 'sleep 1'\n\n[program.ok.action.bad]\ncommand = 'curl http://h/?x=${program.ok.hello}'\n",
    )?;
    let (ok, _code, combined) = cli_try(bin, ws, &["validate", &bad_file.to_string_lossy()])?;
    expect(!ok, "an unknown variable field fails validate")?;
    expect(
        combined.contains("unknown action variable") && combined.contains("program.ok.hello"),
        "the error names the offending variable",
    )?;

    step("H3. built-in action names are reserved");
    std::fs::write(
        &bad_file,
        "[program.ok]\ncommand = 'sleep 1'\n\n[program.ok.action.stop]\ncommand = 'echo nope'\n",
    )?;
    let (ok, _code, combined) = cli_try(bin, ws, &["validate", &bad_file.to_string_lossy()])?;
    expect(
        !ok && combined.contains("reserved"),
        "action `stop` is refused as reserved",
    )?;
    // Observe-only command names are NOT reserved (glossary 动作词表边界).
    std::fs::write(
        &bad_file,
        "[program.ok]\ncommand = 'sleep 1'\n\n[program.ok.action.status]\ncommand = 'echo hi'\n",
    )?;
    let out = cli(bin, ws, &["validate", &bad_file.to_string_lossy()])?;
    expect(
        out.contains("OK"),
        "a custom action named `status` is allowed",
    )?;

    step("H4. action timeout must be > 0");
    std::fs::write(
        &bad_file,
        "[program.ok]\ncommand = 'sleep 1'\n\n[program.ok.action.t0]\ncommand = 'x'\ntimeout = 0\n",
    )?;
    let (ok, _code, combined) = cli_try(bin, ws, &["validate", &bad_file.to_string_lossy()])?;
    expect(
        !ok && combined.contains("timeout"),
        "timeout = 0 is refused",
    )?;
    std::fs::write(
        &bad_file,
        "[program.ok]\ncommand = 'sleep 1'\n\n[program.ok.action.tn]\ncommand = 'x'\ntimeout = -1\n",
    )?;
    let (ok, _code, combined) = cli_try(bin, ws, &["validate", &bad_file.to_string_lossy()])?;
    expect(
        !ok && combined.contains("timeout"),
        "a negative timeout is refused",
    )?;
    Ok(())
}

/// Process-tree helper: pids whose argv contains exactly `arg` (unix only;
/// windows runs no tree assertions).
#[cfg(unix)]
fn pids_with_arg(arg: &str) -> Vec<u32> {
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
        let Ok(cmdline) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        if cmdline.split(|&b| b == 0).any(|a| a == arg.as_bytes()) {
            out.push(name.parse().unwrap());
        }
    }
    out
}

fn state_of(ws: &Workspace, program: &str) -> Result<String> {
    let out = Command::new("curl")
        .args([
            "-s",
            &format!("http://127.0.0.1:{}/v1/programs/{program}", ws.control_port),
        ])
        .output()
        .context("curl program state")?;
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))?;
    Ok(v.get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string())
}

fn wait_state(ws: &Workspace, program: &str, want: &str) -> Result<()> {
    wait_state_in(ws, program, &[want])
}

fn wait_state_in(ws: &Workspace, program: &str, want: &[&str]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let have = state_of(ws, program)?;
        if want.contains(&have.as_str()) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("{program} never reached {want:?} (state {have})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Online actions pass: fan-out, custom-action contract, signal.
fn actions_online_pass(bin: &Path, ws: &Workspace) -> Result<()> {
    // Everything below addresses the acts app; make sure the daemon loaded it.
    let svc_running = pid_of(ws, "svc")?;
    expect(svc_running.is_none(), "acts programs are not autostarted")?;
    let acts_dir = ws.root.join("acts");

    step("H5. `start --app acts` fans out in dependency order");
    let out = cli(bin, ws, &["start", "--app", "acts"])?;
    let (psvc, psig, ptrap) = (
        out.find("  svc:"),
        out.find("  signalee:"),
        out.find("  trapper:"),
    );
    expect(
        matches!((psvc, psig, ptrap), (Some(a), Some(b), Some(c)) if a < b && b < c),
        &format!("fan-out start lists svc < signalee < trapper: {out:?}"),
    )?;
    for name in ["svc", "signalee", "trapper"] {
        wait_running(ws, name)?;
    }

    if cfg!(unix) {
        step("H5b. `xkeeper log -f` streams new lines and stays alive (unix)");
        // logger (started by the fan-out above) echoes `tick` every 0.2s.
        let follow_path = ws.root.join("follow.out");
        let follow = std::fs::File::create(&follow_path)?;
        let mut child = Command::new(bin)
            .arg("--config")
            .arg(ws.config_arg())
            .args(["log", "logger", "--tail", "1", "-f"])
            .stdout(Stdio::from(follow))
            .stderr(Stdio::null())
            .spawn()
            .context("spawn `log -f`")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut len1 = 0usize;
        loop {
            let text = std::fs::read_to_string(&follow_path).unwrap_or_default();
            if text.contains("tick") {
                len1 = text.len();
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("log follow never showed a tick line");
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        // The stream must keep growing after the initial tail (following,
        // not a one-shot tail), and the client must still be running.
        std::thread::sleep(Duration::from_millis(1500));
        let len2 = std::fs::read_to_string(&follow_path)?.len();
        expect(
            len2 > len1,
            &format!("new lines kept arriving after the tail ({len1} -> {len2} bytes)"),
        )?;
        expect(
            matches!(child.try_wait(), Ok(None)),
            "`log -f` is still following (no self-exit)",
        )?;
        let _ = child.kill();
        let _ = child.wait()?;
    }

    step("H5c. control-plane API: status JSON, 409 transition, 404 unknown targets");
    let (code, body) = api_checked(ws, "GET", "/v1/status", "")?;
    expect(
        code == 200,
        &format!("GET /v1/status answers 200, got {code}"),
    )?;
    expect(
        body.contains("\"daemon\"")
            && body.contains("\"programs\"")
            && body.contains("\"unhealthy\"")
            && body.contains("\"pid\""),
        "status JSON carries daemon info and per-program pid/unhealthy",
    )?;
    let (code, body) = api_checked(ws, "POST", "/v1/programs/svc/start", "{}")?;
    expect(
        code == 409,
        &format!("starting a running program is 409, got {code}"),
    )?;
    expect(
        body.contains("already"),
        "the 409 explains the invalid transition",
    )?;
    let (code, _) = api_checked(ws, "GET", "/v1/programs/ghost", "")?;
    expect(code == 404, &format!("unknown program is 404, got {code}"))?;
    let (code, body) = api_checked(ws, "POST", "/v1/programs/ghost/start", "{}")?;
    expect(
        code == 404,
        &format!("starting an unknown program is 404, got {code}"),
    )?;
    expect(
        body.contains("unknown program"),
        "the 404 names the unknown program",
    )?;
    let (code, _) = api_checked(ws, "POST", "/v1/programs/svc/actions/nosuch", "{}")?;
    expect(code == 404, &format!("unknown action is 404, got {code}"))?;
    let (code, _) = api_checked(ws, "POST", "/v1/apps/ghost/start", "{}")?;
    expect(
        code == 404,
        &format!("unknown app fan-out is 404, got {code}"),
    )?;
    // The CLI surfaces the same answers with exit code 1.
    let (ok, code, combined) = cli_try(bin, ws, &["action", "svc", "nosuch"])?;
    expect(
        !ok && code == 1,
        &format!("unknown action exits 1, got {code}"),
    )?;
    expect(combined.contains("404"), "the CLI error reports the 404")?;
    let (ok, code, combined) = cli_try(bin, ws, &["start", "--app", "ghost"])?;
    expect(
        !ok && code == 1,
        &format!("unknown app fan-out exits 1, got {code}"),
    )?;
    expect(
        combined.contains("unknown app") || combined.contains("404"),
        "the CLI error names the unknown app",
    )?;

    step("H6. custom action runs with variable substitution");
    wait_state(ws, "svc", "running")?;
    let svc_pid = pid_of(ws, "svc")?.expect("svc has a pid");
    let out = cli(bin, ws, &["action", "svc", "ping"])?;
    expect(
        out.contains(&format!("svc_pid={svc_pid}")),
        &format!("${{program.svc.pid}} substituted with the real pid: {out:?}"),
    )?;
    expect(
        out.contains("svc_state=running"),
        "state variable resolves to running",
    )?;

    step("H6b. a multi-line command runs as one shell script");
    let out = cli(bin, ws, &["action", "svc", "multiline"])?;
    expect(
        out.contains("ml-one") && out.contains("ml-two"),
        &format!("both lines of the multi-line command executed: {out:?}"),
    )?;

    step("H6c. `xkeeper action` passes the action's own exit code through");
    let (ok, code, combined) = cli_try(bin, ws, &["action", "svc", "fail3"])?;
    expect(
        !ok && code == 3,
        &format!("an exit-3 action exits 3, got {code}"),
    )?;
    expect(
        combined.contains("exited with code 3"),
        "the failure explains the action's exit code",
    )?;

    step("H7. the action cwd is the program's work_dir");
    let out = cli(bin, ws, &["action", "svc", "workdir_probe"])?;
    expect(
        out.trim() == acts_dir.to_str().unwrap(),
        &format!("pwd == work_dir ({}): {out:?}", acts_dir.display()),
    )?;

    step("H8. timeout kills the action process tree");
    let t0 = Instant::now();
    let (ok, code, combined) = cli_try(bin, ws, &["action", "svc", "slow"])?;
    let elapsed = t0.elapsed();
    expect(!ok, "a timed-out action fails the call")?;
    expect(code == 1, &format!("timeout exits 1, got {code}"))?;
    expect(
        combined.contains("timed out"),
        "the error says the action timed out",
    )?;
    expect(
        elapsed < Duration::from_secs(20),
        &format!("returned near the 2s timeout, took {elapsed:?} (sleep 31 was killed)"),
    )?;
    expect(
        pid_of(ws, "svc")? == Some(svc_pid),
        "the supervised program is unaffected by the action timeout",
    )?;
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline && !pids_with_arg("31").is_empty() {
            std::thread::sleep(Duration::from_millis(100));
        }
        expect(
            pids_with_arg("31").is_empty(),
            "no `sleep 31` survives the tree kill (/proc scan)",
        )?;
    }

    step("H9. same (program, action) is mutually exclusive, no queueing");
    // Fire the first ring call in the background (sleep 4, timeout 30),
    // then race a second one in: it must hit the 409, not queue.
    let handle = std::thread::spawn({
        let bin = bin.to_path_buf();
        let cfg = ws.config_arg();
        move || {
            let out = Command::new(&bin)
                .arg("--config")
                .arg(&cfg)
                .args(["action", "svc", "ring"])
                .output()
                .expect("run first ring");
            (
                out.status.success(),
                out.status.code().unwrap_or(-1),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            )
        }
    });
    std::thread::sleep(Duration::from_millis(700));
    let (ok2, code2, combined2) = cli_try(bin, ws, &["action", "svc", "ring"])?;
    expect(!ok2, "the concurrent second call is refused")?;
    expect(
        code2 == 1,
        &format!("conflict exits 1, got {code2}: {combined2:?}"),
    )?;
    expect(
        combined2.contains("409") || combined2.contains("already running"),
        "the error says the action is already running",
    )?;
    let (ok1, code1, _) = handle.join().unwrap();
    expect(
        ok1 && code1 == 0,
        "the first ring call completed normally (exit 0)",
    )?;
    expect(
        pid_of(ws, "svc")? == Some(svc_pid),
        "mutual exclusion left the program untouched",
    )?;

    step("H10. an action can chain `xkeeper restart` via the control plane");
    let out = cli(bin, ws, &["action", "svc", "chained"])?;
    expect(
        out.contains(&format!("old_svc_pid={svc_pid}")),
        "the substitution saw the pre-restart pid",
    )?;
    let new_pid = wait_running(ws, "svc")?;
    expect(
        new_pid != svc_pid,
        "the chained restart replaced the process",
    )?;
    expect(
        out.contains(&new_pid.to_string()),
        &format!("the action observed the new pid ({new_pid}): {out:?}"),
    )?;

    step("H11. actions run for stopped programs; pid substitutes to empty");
    cli(bin, ws, &["stop", "svc"])?;
    wait_state(ws, "svc", "stopped")?;
    let out = cli(bin, ws, &["action", "svc", "stopped_pid"])?;
    expect(
        out.contains("stopped_svc_pid=[]"),
        &format!("the pid of a stopped program is an empty string: {out:?}"),
    )?;

    if cfg!(unix) {
        step("H12. signal: whitelist in/out, case-insensitive, not-running (unix)");
        // USR2 lowercase: accepted, normalized; the trapper ignores USR1/USR2,
        // so its state machine must stay running with the same pid.
        let trap_pid = wait_running(ws, "trapper")?;
        let out = cli(bin, ws, &["signal", "trapper", "usr2"])?;
        expect(out.contains("delivered"), "lowercase usr2 is accepted")?;
        expect(
            pid_of(ws, "trapper")? == Some(trap_pid),
            "the trapped process survived USR2",
        )?;
        expect(
            state_of(ws, "trapper")? == "running",
            "signal did not change the state machine",
        )?;
        let out = cli(bin, ws, &["signal", "trapper", "USR1"])?;
        expect(out.contains("delivered"), "USR1 delivered to the trapper")?;
        expect(
            state_of(ws, "trapper")? == "running",
            "trapper still running after USR1",
        )?;
        // KILL is off-whitelist; the error points at stop.
        let (ok, code, combined) = cli_try(bin, ws, &["signal", "trapper", "KILL"])?;
        expect(!ok && code == 1, "KILL exits 1")?;
        expect(
            combined.contains("whitelist") && combined.contains("stop"),
            "the error explains the whitelist and the stop alternative",
        )?;
        expect(
            pid_of(ws, "trapper")? == Some(trap_pid),
            "the rejected signal did not touch the program",
        )?;
        // TERM really terminates (signalee has autorestart = never).
        let out = cli(bin, ws, &["signal", "signalee", "TERM"])?;
        expect(out.contains("delivered"), "TERM delivered")?;
        wait_state(ws, "signalee", "exited")?;
        // No live child anymore: signaling must fail.
        let (ok, code, combined) = cli_try(bin, ws, &["signal", "signalee", "USR1"])?;
        expect(!ok && code == 1, "signaling an exited program exits 1")?;
        expect(
            combined.contains("not running"),
            "the error says there is no child to signal",
        )?;
    } else {
        println!("    (windows: signal scenarios require unix; skipping)");
    }

    step("H13. `stop --app acts` runs in reverse order");
    let out = cli(bin, ws, &["stop", "--app", "acts"])?;
    let (ptrap, psig, psvc) = (
        out.find("  trapper:"),
        out.find("  signalee:"),
        out.find("  svc:"),
    );
    expect(
        matches!((ptrap, psig, psvc), (Some(a), Some(b), Some(c)) if a < b && b < c),
        &format!("fan-out stop lists trapper < signalee < svc (reverse): {out:?}"),
    )?;
    // signalee was TERM-signaled to exit in H12 (autorestart never): a
    // terminal state (exited) is as stopped as stopped itself — fan-out stop
    // must simply not leave anything running.
    for name in ["svc", "signalee", "trapper"] {
        wait_state_in(ws, name, &["stopped", "exited"])?;
    }

    step("H14. fan-out start covers user_stopped programs (issued from the shell)");
    // Every acts program is user-stopped at this point (explicit stop above
    // and in H11); the fan-out start must pull all of them up and clear the
    // marker. Issued via the shell to prove the shell verb fans out too.
    let out = cli(bin, ws, &["shell", "-e", "start --app acts"])?;
    expect(
        matches!((out.find("  svc:"), out.find("  signalee:"), out.find("  trapper:")),
            (Some(a), Some(b), Some(c)) if a < b && b < c),
        "fan-out start again lists all three in dependency order",
    )?;
    for name in ["svc", "signalee", "trapper"] {
        wait_running(ws, name)?;
        expect(
            matches!(state_of(ws, name)?.as_str(), "running" | "starting"),
            &format!("user-stopped {name} was started by the fan-out (marker cleared)"),
        )?;
    }

    step("H15. shell: status table, unknown-command tolerance, restart, action exit code");
    // status table: program name is the primary column, app is attribution.
    let out = cli(bin, ws, &["shell", "-e", "status"])?;
    expect(
        ["NAME", "APP", "STATE", "PID", "RESTARTS", "UNHEALTHY"]
            .iter()
            .all(|h| out.contains(h)),
        &format!("shell status prints the aligned table header: {out:?}"),
    )?;
    expect(
        out.contains("svc") && out.contains("acts"),
        "rows name the program with its app as a separate column",
    )?;

    // REPL: an unknown command is reported (with a did-you-mean hint) and the
    // shell keeps reading — piped stdin drives the non-TTY fallback path.
    let svc_pid2 = wait_running(ws, "svc")?;
    let mut repl = Command::new(bin)
        .arg("--config")
        .arg(ws.config_arg())
        .arg("shell")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn shell repl")?;
    {
        use std::io::Write;
        repl.stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(b"sttaus\npid svc\nexit\n")?;
    }
    let repl_out = repl.wait_with_output()?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&repl_out.stdout),
        String::from_utf8_lossy(&repl_out.stderr)
    );
    expect(
        repl_out.status.success(),
        "the shell leaves via `exit` with code 0",
    )?;
    expect(
        combined.contains("unknown command"),
        "the typo is reported as an unknown command",
    )?;
    expect(
        combined.contains("status"),
        "the did-you-mean hint suggests `status`",
    )?;
    expect(
        combined.contains(&svc_pid2.to_string()),
        "the shell kept reading after the typo (`pid svc` answered)",
    )?;

    // restart through the shell hits the same control plane (same migration).
    let old_pid = wait_running(ws, "svc")?;
    let out = cli(bin, ws, &["shell", "-e", "restart svc"])?;
    expect(
        out.contains("svc"),
        &format!("shell restart reports the program: {out:?}"),
    )?;
    let new_pid = wait_running(ws, "svc")?;
    expect(
        new_pid != old_pid,
        "shell restart replaced the process like the CLI does",
    )?;

    // shell -e passes the action's own exit code through (like `xkeeper action`).
    let (ok, code, combined) = cli_try(bin, ws, &["shell", "-e", "action svc fail3"])?;
    expect(
        !ok && code == 3,
        &format!("shell action exit-3 → exit 3, got {code}"),
    )?;
    expect(
        combined.contains("exited with code 3"),
        "the shell explains the action's failure",
    )?;
    Ok(())
}

// -- webui lifecycle pass ---------------------------------------------------------

/// `GET /api/health` probe via curl (same mechanism as the daemon wait).
fn http_ok(url: &str) -> bool {
    Command::new("curl")
        .args(["-sf", "-m", "3", url])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The console is config-driven: `config --delete webui` + `reload` turns it
/// off, `config --set webui.listen=...` + `reload` turns it on or rebinds it
/// (config-driven-webui acceptance scenarios). Restores the setup listen so
/// the browser pass finds the console on `ws.webui_port`.
fn webui_pass(bin: &Path, ws: &Workspace) -> Result<()> {
    let health = |port: u16| http_ok(&format!("http://127.0.0.1:{port}/api/health"));

    step("W1. config --delete webui + reload stops the console (connection refused)");
    expect(
        health(ws.webui_port),
        "the console answers before the test (setup wrote [webui])",
    )?;
    let out = cli(bin, ws, &["config", "--delete", "webui"])?;
    expect(
        out.contains("deleted webui"),
        &format!("delete reports the removed table: {out:?}"),
    )?;
    let out = cli(bin, ws, &["reload"])?;
    expect(
        out.contains("webui: webui console disabled"),
        &format!("reload reports the disabled console: {out:?}"),
    )?;
    expect(
        !health(ws.webui_port),
        "the console port refuses connections after the reload",
    )?;

    step("W2. config --set webui.listen + reload serves on the new address (off→on)");
    let new_port = free_port()?;
    let out = cli(
        bin,
        ws,
        &["config", "--set", &format!("webui.listen=127.0.0.1:{new_port}")],
    )?;
    expect(
        out.contains("webui.listen"),
        &format!("set reports the written key: {out:?}"),
    )?;
    let out = cli(bin, ws, &["reload"])?;
    expect(
        out.contains(&format!(
            "webui: webui console serving on http://127.0.0.1:{new_port}"
        )),
        &format!("reload reports the serving console: {out:?}"),
    )?;
    expect(health(new_port), "the console answers on the new port")?;
    expect(!health(ws.webui_port), "the old port stays closed")?;

    step("W3. restoring the listen + reload rebinds back (listen change)");
    let out = cli(
        bin,
        ws,
        &[
            "config",
            "--set",
            &format!("webui.listen=127.0.0.1:{}", ws.webui_port),
        ],
    )?;
    expect(
        out.contains("webui.listen"),
        &format!("set reports the restored key: {out:?}"),
    )?;
    let out = cli(bin, ws, &["reload"])?;
    expect(
        out.contains(&format!(
            "serving on http://127.0.0.1:{}",
            ws.webui_port
        )),
        &format!("reload reports the restored address: {out:?}"),
    )?;
    expect(
        health(ws.webui_port),
        "the original console port answers again",
    )?;
    expect(!health(new_port), "the temporary port is released")?;

    step("W4. reload with an occupied port degrades: note in the output, daemon keeps running");
    let blocker = std::net::TcpListener::bind("127.0.0.1:0")?;
    let occupied = blocker.local_addr()?.port();
    let out = cli(
        bin,
        ws,
        &["config", "--set", &format!("webui.listen=127.0.0.1:{occupied}")],
    )?;
    let out = cli(bin, ws, &["reload"])?;
    expect(
        out.contains("webui: webui console unavailable"),
        &format!("reload reports the degraded console: {out:?}"),
    )?;
    expect(
        api(ws, "/v1/status")?.contains("programs"),
        "the daemon keeps serving the control plane",
    )?;

    step("W5. freeing the port + reload recovers the console on the same address");
    drop(blocker);
    std::thread::sleep(Duration::from_millis(150));
    let out = cli(bin, ws, &["reload"])?;
    expect(
        out.contains(&format!(
            "webui: webui console serving on http://127.0.0.1:{occupied}"
        )),
        &format!("reload recovers the console: {out:?}"),
    )?;
    expect(health(occupied), "the console answers after recovery")?;

    step("W6. restore the setup listen for the browser pass");
    let out = cli(
        bin,
        ws,
        &[
            "config",
            "--set",
            &format!("webui.listen=127.0.0.1:{}", ws.webui_port),
        ],
    )?;
    let _ = cli(bin, ws, &["reload"])?;
    expect(health(ws.webui_port), "the setup port serves again")?;
    expect(!health(occupied), "the temporary port is released")?;

    step("W7. a bare [webui] section serves the built-in default 127.0.0.1:9877 and exits with the daemon");
    // Skip politely when something else owns the default port on this host.
    match std::net::TcpListener::bind("127.0.0.1:9877") {
        Err(_) => println!("    skip: 127.0.0.1:9877 is occupied on this host"),
        Ok(reserve) => {
            drop(reserve);
            let control2 = free_port()?;
            let log2 = ws.root.join("logs2");
            let apps2 = ws.root.join("apps2");
            std::fs::create_dir_all(&log2)?;
            std::fs::create_dir_all(&apps2)?;
            // A second daemon with its own control port, log dir and an empty
            // app registry — the bare `[webui]` section is the only switch.
            let cfg2 = ws.root.join("daemon2.toml");
            std::fs::write(
                &cfg2,
                format!(
                    "[daemon]\nhost = \"127.0.0.1\"\nport = {control2}\nlog_dir = \"{}\"\nlog_level = \"warn\"\napp_dir = \"{}\"\n\n[webui]\n",
                    log2.display().to_string().replace('\\', "/"),
                    apps2.display().to_string().replace('\\', "/"),
                ),
            )?;
            let out = std::fs::File::create(ws.root.join("daemon2.out.log"))?;
            let err = out.try_clone()?;
            let mut child = Command::new(bin)
                .arg("--config")
                .arg(&cfg2)
                .arg("run")
                .stdout(Stdio::from(out))
                .stderr(Stdio::from(err))
                .spawn()?;
            let mut up = false;
            for _ in 0..120 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                if http_ok("http://127.0.0.1:9877/api/health") {
                    up = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            expect(
                up,
                "the console serves on the default listen without an explicit listen",
            )?;
            // The console exits with the daemon (POST /v1/shutdown).
            let _ = Command::new("curl")
                .args([
                    "-sf",
                    "-m",
                    "10",
                    "-X",
                    "POST",
                    &format!("http://127.0.0.1:{control2}/v1/shutdown"),
                ])
                .output();
            for _ in 0..80 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            std::thread::sleep(Duration::from_millis(300));
            expect(
                !http_ok("http://127.0.0.1:9877/api/health"),
                "the console is gone once the daemon has exited",
            )?;
        }
    }
    Ok(())
}

// -- browser pass -----------------------------------------------------------------

/// Drive the embedded console with playwright (node script via npx-resolved
/// module). Kept as a subprocess so xtask carries no node dependency.
fn browser_pass(ws: &Workspace) -> Result<()> {
    step("F. browser: badge appears, app-scoped apply works, result renders");
    let script = browser_script(ws);
    let script_path = ws.root.join("e2e-browser.mjs");
    std::fs::write(&script_path, script)?;

    // Resolve playwright from the npx cache (no fresh install), then run.
    let resolve = r#"
let candidates = [];
const envDir = process.env.HOME + '/.npm/_npx';
try {
  for (const d of require('fs').readdirSync(envDir)) {
    candidates.push(envDir + '/' + d + '/node_modules/playwright');
  }
} catch {}
const local = process.argv[2] + '/webui/node_modules/playwright';
candidates.push(local);
for (const c of candidates) {
  try { require.resolve(c + '/package.json'); console.log(c); process.exit(0); } catch {}
}
console.error('playwright not found'); process.exit(3);
"#;
    let resolve_path = ws.root.join("resolve-playwright.cjs");
    std::fs::write(&resolve_path, resolve)?;
    let pw = Command::new("node")
        .arg(&resolve_path)
        .arg(repo_root())
        .output()
        .context("resolve playwright")?;
    if !pw.status.success() {
        bail!(
            "playwright unavailable ({}): install with `npx playwright install chromium` \
             or rerun with --no-browser",
            String::from_utf8_lossy(&pw.stderr)
        );
    }
    let pw_dir = PathBuf::from(String::from_utf8_lossy(&pw.stdout).trim());

    let status = Command::new("node")
        .env("NODE_PATH", pw_dir.join(".."))
        .arg(&script_path)
        .arg(pw_dir.to_str().unwrap())
        .status()
        .context("run browser e2e script")?;
    if !status.success() {
        bail!("browser pass failed with {status}");
    }
    Ok(())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the repo")
        .to_path_buf()
}

/// The browser script asserts the webui-ui spec scenarios. Node-side asserts
/// print `ok:` lines; failures throw and exit non-zero.
fn browser_script(ws: &Workspace) -> String {
    format!(
        r###"// Generated by xtask e2e — drives the embedded console UI.
import {{ createRequire }} from 'node:module';
const require = createRequire(import.meta.url);
const {{ chromium }} = require(process.argv[2] + '/index.js');

const URL = 'http://127.0.0.1:{webui}/';
const ALPHA_CFG = '{alpha}';
const ok = (m) => console.log('    ok: ' + m);

// The overview bar and app view live in shadow roots; pierce with a helper.
const pierce = (el, sel) => el.shadowRoot?.querySelector(sel);

const browser = await chromium.launch({{ headless: true }});
const page = await browser.newPage();
page.setDefaultTimeout(10_000);
try {{
  await page.goto(URL);
  await page.waitForFunction(() => {{
    const app = document.querySelector('xkeeper-app');
    const bar = app?.shadowRoot?.querySelector('xkeeper-overview-bar');
    return !!bar?.shadowRoot?.querySelector('.bar');
  }});
  // No pending initially: the badge must not exist.
  await page.waitForFunction(() => {{
    const app = document.querySelector('xkeeper-app');
    const bar = app?.shadowRoot?.querySelector('xkeeper-overview-bar');
    return bar && !bar.shadowRoot.querySelector('.pending');
  }});
  ok('console loads with no pending badge');

  // Edit alpha's config on disk → badge with count 1.
  const fs = await import('node:fs');
  fs.writeFileSync(ALPHA_CFG, [
    '[app]',
    'autostart = true',
    '',
    '[program.web]',
    'command = "sleep 100000"',
    'startsecs = 0.2',
    '',
    '[program.worker]',
    'command = "sleep 400000"',
    'startsecs = 0.2',
    '',
  ].join('\n'));

  await page.waitForFunction(() => {{
    const app = document.querySelector('xkeeper-app');
    const bar = app?.shadowRoot?.querySelector('xkeeper-overview-bar');
    const badge = bar?.shadowRoot?.querySelector('.pending .badge');
    return badge && badge.textContent.includes('待应用变更');
  }});
  ok('pending badge appears after the disk edit');

  // Navigate to the alpha app view; the changed row carries the mark.
  await page.goto(URL + '#/app/alpha').catch(() => {{}});
  // SPA uses history routing; drive the tree link instead.
  await page.evaluate(() => {{
    const app = document.querySelector('xkeeper-app');
    const tree = app?.shadowRoot?.querySelector('xkeeper-console-tree');
    const link = [...(tree?.shadowRoot?.querySelectorAll('a') ?? [])].find((a) =>
      a.getAttribute('href')?.includes('/app/alpha'),
    );
    link?.click();
  }});
  await page.waitForFunction(() => {{
    const app = document.querySelector('xkeeper-app');
    const view = app?.shadowRoot?.querySelector('xkeeper-app-overview');
    return !!view?.shadowRoot?.querySelector('.apply-strip button');
  }});
  ok('alpha view shows the apply strip');

  // Confirm dialog: auto-accept.
  page.on('dialog', (d) => d.accept());
  await page.evaluate(() => {{
    const app = document.querySelector('xkeeper-app');
    const view = app?.shadowRoot?.querySelector('xkeeper-app-overview');
    view?.shadowRoot?.querySelector('.apply-strip button')?.click();
  }});

  // Result text renders with the per-program action.
  await page.waitForFunction(() => {{
    const app = document.querySelector('xkeeper-app');
    const view = app?.shadowRoot?.querySelector('xkeeper-app-overview');
    const fb = view?.shadowRoot?.querySelector('.apply-strip .feedback');
    return fb && fb.textContent.includes('update-and-restart');
  }});
  ok('apply result lists the rebuilt program');

  // Badge cleared after the (alpha-scoped) apply.
  await page.waitForFunction(() => {{
    const app = document.querySelector('xkeeper-app');
    const bar = app?.shadowRoot?.querySelector('xkeeper-overview-bar');
    return bar && !bar.shadowRoot.querySelector('.pending');
  }});
  ok('badge clears after the scoped apply');
}} finally {{
  await browser.close();
}}
console.log('BROWSER PASS OK');
"###,
        webui = ws.webui_port,
        // Forward slashes: a raw Windows path would put `\x`-style escapes
        // into the JS string literal (node fs accepts `/` everywhere).
        alpha = ws
            .root
            .join("app-alpha/xkeeper.toml")
            .display()
            .to_string()
            .replace('\\', "/"),
    )
}

// -- entry ----------------------------------------------------------------------

pub(crate) fn run(args: Args) -> Result<()> {
    let started = Instant::now();
    let bin = crate::build_daemon()?;
    let ws = setup_workspace("main")?;
    // The actions scenario app must exist before the daemon boots (bootstrap
    // loads it, autostart = false keeps everything under our control).
    setup_actions_app(&ws, &bin)?;

    println!(
        "workspace: {}\ndaemon: run (webui 127.0.0.1:{} via [webui]), control 127.0.0.1:{}",
        ws.root.display(),
        ws.webui_port,
        ws.control_port
    );
    // Offline scenarios first: the daemon must NOT be running yet.
    offline_pass(&bin, &ws)?;
    actions_offline_pass(&bin, &ws)?;
    config_pass(&bin, &ws)?;
    let mut daemon = spawn_daemon(&bin, &ws)?;
    let result = (|| {
        cli_pass(&bin, &ws)?;
        actions_online_pass(&bin, &ws)?;
        webui_pass(&bin, &ws)?;
        if args.no_browser {
            println!("\n(--no-browser: skipping browser pass)");
        } else {
            browser_pass(&ws)?;
        }
        Ok(())
    })();

    shutdown_daemon(&ws, &mut daemon);
    let outcome = match result {
        Ok(()) => {
            println!(
                "\nACCEPTANCE E2E PASSED ({:.1}s)",
                started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("\nACCEPTANCE E2E FAILED: {e:#}");
            Err(e)
        }
    };
    if args.keep {
        println!("workspace kept at {}", ws.root.display());
    } else if let Err(e) = std::fs::remove_dir_all(&ws.root) {
        eprintln!("warning: cleanup failed: {e}");
    }
    outcome
}
