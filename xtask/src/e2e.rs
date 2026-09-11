//! Acceptance e2e for the apply-workflow change (`cargo run -p xtask -- e2e`).
//!
//! Drives the REAL daemon binary end-to-end, in two passes:
//!
//! - CLI pass — the acceptance checklist as executable scenarios (spec:
//!   apply-workflow, configuration, control-plane, shell-client, app-registry):
//!   A. detect does not touch processes (periodic + reload preview),
//!   B. scoped apply leaves other apps untouched,
//!   C. --restart restarts unchanged programs but never a user-stopped one,
//!   D. online `add` enters pending and starts on apply,
//!   E. no-change apply is idempotent.
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
    std::fs::write(
        root.join("daemon.toml"),
        format!(
            "[daemon]\nhost = \"127.0.0.1\"\nport = {control_port}\nlog_dir = \"{}/logs\"\nlog_level = \"warn\"\nmonitor_interval = 0.5\n",
            root.display()
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
    std::os::unix::fs::symlink(
        root.join("app-alpha/xkeeper.toml"),
        root.join("apps/alpha.toml"),
    )?;
    std::os::unix::fs::symlink(
        root.join("app-beta/xkeeper.toml"),
        root.join("apps/beta.toml"),
    )?;
    Ok(Workspace {
        root,
        control_port,
        webui_port,
    })
}

// -- daemon lifecycle --------------------------------------------------------

struct Daemon {
    child: Child,
}

fn spawn_daemon(bin: &Path, ws: &Workspace) -> Result<Daemon> {
    // The daemon must not inherit our stdio (see stress.rs for the reasons).
    let mut child = Command::new(bin)
        .arg("--config")
        .arg(ws.root.join("daemon.toml"))
        .arg("webui")
        .arg("--listen")
        .arg(format!("127.0.0.1:{}", ws.webui_port))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
            .args(["-sf", &format!("http://127.0.0.1:{}/api/health", ws.webui_port)])
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

fn api(ws: &Workspace, path: &str) -> Result<String> {
    let out = Command::new("curl")
        .args(["-sf", &format!("http://127.0.0.1:{}{path}", ws.control_port)])
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
        if v.get("programs").and_then(|p| p.as_array()).map(|a| a.is_empty()).unwrap_or(true) {
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
    let progs = v.get("programs").and_then(|p| p.as_array()).cloned().unwrap_or_default();
    expect(
        progs.len() == 1 && progs[0].get("program").and_then(|p| p.as_str()) == Some("worker"),
        "periodic detect published worker as changed (no manual reload)",
    )?;
    expect(pid_of(ws, "worker")? == Some(worker_pid), "worker still runs the old definition")?;
    let out = cli(bin, ws, &["reload"])?;
    expect(
        out.contains("worker") && out.contains("run `xkeeper apply`"),
        "reload prints a preview, applies nothing",
    )?;
    expect(pid_of(ws, "worker")? == Some(worker_pid), "reload left worker alone")?;

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
    expect(pid_of(ws, "job")? == Some(job_pid), "beta job untouched by alpha-scoped apply")?;
    let v: serde_json::Value = serde_json::from_str(&api(ws, "/v1/pending")?)?;
    let progs = v.get("programs").and_then(|p| p.as_array()).cloned().unwrap_or_default();
    expect(
        progs.len() == 1 && progs[0].get("program").and_then(|p| p.as_str()) == Some("job"),
        "beta's pending survived the alpha apply",
    )?;

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
    expect(job2.is_some() && job2 != Some(job_pid), "job was restarted by --restart")?;

    step("D. online add enters pending and starts on apply");
    let gamma = ws.root.join("app-gamma");
    std::fs::create_dir_all(&gamma)?;
    std::fs::write(
        gamma.join("xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.gamma]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n",
    )?;
    let out = cli(bin, ws, &["add", gamma.to_str().unwrap(), "--name", "gamma"])?;
    expect(
        out.contains("registration is pending"),
        "add sync reports pending (not immediate start)",
    )?;
    expect(pid_of(ws, "gamma")?.is_none(), "gamma not started before apply")?;
    let out = cli(bin, ws, &["apply"])?;
    expect(out.contains("gamma"), "apply picked the new app up")?;
    wait_running(ws, "gamma")?;

    step("shell: pending + apply single-command mode");
    let out = cli(bin, ws, &["shell", "-e", "pending"])?;
    expect(out.contains("no pending changes"), "shell pending is clean after full apply")?;
    let out = cli(bin, ws, &["shell", "-e", "apply alpha"])?;
    expect(out.contains("no changes"), "shell scoped apply idempotent")?;

    // Re-sync disk with the running definitions so the browser pass starts
    // from a clean slate (worker currently runs sleep 200000 after step B).
    std::fs::write(
        ws.root.join("app-alpha/xkeeper.toml"),
        "[app]\nautostart = true\n\n[program.web]\ncommand = \"sleep 100000\"\nstartsecs = 0.2\n\n[program.worker]\ncommand = \"sleep 200000\"\nstartsecs = 0.2\n",
    )?;
    wait_pending_empty(ws)
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
        alpha = ws.root.join("app-alpha/xkeeper.toml").display(),
    )
}

// -- entry ----------------------------------------------------------------------

pub(crate) fn run(args: Args) -> Result<()> {
    let started = Instant::now();
    let bin = crate::build_daemon()?;
    let ws = setup_workspace("main")?;

    println!(
        "workspace: {}\ndaemon: webui 127.0.0.1:{}, control 127.0.0.1:{}",
        ws.root.display(),
        ws.webui_port,
        ws.control_port
    );
    let mut daemon = spawn_daemon(&bin, &ws)?;
    let result = (|| {
        cli_pass(&bin, &ws)?;
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
            println!("\nACCEPTANCE E2E PASSED ({:.1}s)", started.elapsed().as_secs_f64());
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
