//! Run topologies (design D4/D7): spawn an isolated daemon in a temp
//! workspace, or connect to an already-running one. Both go through the same
//! /v1 client code path; cleanup runs on every exit path (success, failure,
//! ctrl-C) and removes everything bench created.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::api::Client;
use crate::cases::{ProgramPlan, BENCH_PREFIX};
use crate::dcfg;

/// The daemon child lives in a global slot so the ctrl-C handler can stop it
/// without dragging a `Child` through every call frame.
static DAEMON_CHILD: Mutex<Option<Child>> = Mutex::new(None);
static CLEANUP: Mutex<Option<CleanupCtx>> = Mutex::new(None);
/// Set by whoever currently owns the cleanup (ctrl-C handler or main); the
/// loser of take_cleanup() waits instead of racing the removals.
static CLEANUP_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub struct CleanupCtx {
    pub client: Client,
    /// Absolute paths of the app config files bench wrote (connect: inside
    /// the target daemon's app_dir).
    pub app_files: Vec<PathBuf>,
    pub app_names: Vec<String>,
    pub log_dir: PathBuf,
    /// Temp workspace root (spawn mode only).
    pub root: Option<PathBuf>,
    /// Transient counter directory (spawn: inside root; connect: temp dir).
    pub count_dir: Option<PathBuf>,
    pub keep: bool,
    pub spawn: bool,
}

pub fn install_cleanup(ctx: CleanupCtx) {
    *CLEANUP.lock().unwrap() = Some(ctx);
}

/// Take the pending cleanup context (idempotent: whoever takes it runs it).
pub fn take_cleanup() -> Option<CleanupCtx> {
    CLEANUP.lock().unwrap().take()
}

pub fn install_ctrlc_handler() -> Result<()> {
    ctrlc::set_handler(|| {
        eprintln!("\n[bench] interrupted: cleaning up bench resources");
        if CLEANUP_RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return; // main already cleans up; let it finish
        }
        if let Some(ctx) = take_cleanup() {
            run_cleanup(&ctx);
        }
        std::process::exit(130);
    })
    .context("install ctrl-C handler")
}

/// True while the ctrl-C handler owns the cleanup; the normal exit path
/// waits (bounded) so the handler is not killed mid-removal by main's exit.
pub fn cleanup_in_progress() -> bool {
    CLEANUP_RUNNING.load(Ordering::SeqCst)
}

/// Claim/release cleanup ownership on the normal exit path.
pub fn claim_cleanup() -> bool {
    CLEANUP_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

pub fn release_cleanup() {
    CLEANUP_RUNNING.store(false, Ordering::SeqCst);
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// -- daemon binary discovery (design D4) -------------------------------------

pub fn discover_daemon(explicit: Option<&Path>) -> Result<PathBuf> {
    let exe_name = if cfg!(windows) { "xkeeper.exe" } else { "xkeeper" };
    if let Some(p) = explicit {
        let p = p
            .canonicalize()
            .with_context(|| format!("--daemon {}: not found", p.display()))?;
        if !p.is_file() {
            bail!("--daemon {} is not a file", p.display());
        }
        return Ok(p);
    }
    // Sibling of this bench binary (same target profile, or release layout).
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let cand = dir.join(exe_name);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    // Workspace target dirs, debug first (design D4).
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = manifest.parent() {
        for profile in ["debug", "release"] {
            let cand = root.join("target").join(profile).join(exe_name);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    bail!(
        "cannot find the xkeeper daemon binary (tried the bench binary's directory and \
         workspace target/{{debug,release}}); pass --daemon <path to xkeeper>"
    )
}

// -- site ---------------------------------------------------------------------

pub struct Site {
    pub client: Client,
    pub app_dir: PathBuf,
    pub log_dir: PathBuf,
    pub count_dir: PathBuf,
    pub daemon_version: String,
    /// Only known in spawn mode (we own the process).
    pub daemon_pid: Option<u32>,
    pub spawn: bool,
}

impl Site {
    /// Write the app configs into the registry and make the daemon pick them
    /// up: reload (rescan + detect) then apply (autostart spawns the load
    /// programs). Same path for both topologies.
    pub fn register(&self, plans: &[ProgramPlan]) -> Result<()> {
        for p in plans {
            let path = self.app_dir.join(format!("{}.toml", p.name));
            std::fs::write(&path, &p.toml)
                .with_context(|| format!("write app config {}", path.display()))?;
        }
        self.client.reload().context("reload after registering bench apps")?;
        // Spawn mode owns the whole workspace, so a global apply is fine; in
        // connect mode scope the apply to the bench apps so any pending user
        // changes on the target daemon are never applied as a side effect.
        if self.spawn {
            self.client.apply(None).context("apply bench apps")?;
        } else {
            for p in plans {
                self.client
                    .apply(Some(&p.name))
                    .with_context(|| format!("apply bench app {}", p.name))?;
            }
        }
        Ok(())
    }

    pub fn cleanup_ctx(&self, plans: &[ProgramPlan], keep: bool) -> CleanupCtx {
        CleanupCtx {
            client: self.client.clone(),
            app_files: plans
                .iter()
                .map(|p| self.app_dir.join(format!("{}.toml", p.name)))
                .collect(),
            app_names: plans.iter().map(|p| p.name.clone()).collect(),
            log_dir: self.log_dir.clone(),
            root: if self.spawn {
                Some(self.count_dir.parent().unwrap_or(Path::new("")).to_path_buf())
            } else {
                None
            },
            count_dir: Some(self.count_dir.clone()),
            keep,
            spawn: self.spawn,
        }
    }
}

// -- spawn topology ------------------------------------------------------------

/// Wait for /v1/health; bail early when the daemon child exits.
fn wait_health(client: &Client, deadline_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        if Instant::now() > deadline {
            bail!("daemon did not become healthy within {deadline_secs}s");
        }
        if daemon_exited() {
            bail!("daemon exited during startup (see its workspace daemon logs)");
        }
        if client.health().is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn daemon_exited() -> bool {
    let mut guard = DAEMON_CHILD.lock().unwrap();
    match guard.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(Some(_))),
        None => false,
    }
}

pub fn spawn_site(daemon_bin: Option<&Path>) -> Result<Site> {
    let bin = discover_daemon(daemon_bin)?;
    let root = std::env::temp_dir().join(format!(
        "xk-bench-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let result = spawn_site_inner(&bin, &root);
    if let Err(e) = result {
        // Don't leave a half-created workspace or a stuck daemon behind.
        wait_and_kill_daemon_child();
        let _ = std::fs::remove_dir_all(&root);
        return Err(e);
    }
    result
}

fn spawn_site_inner(bin: &Path, root: &Path) -> Result<Site> {
    for d in ["apps", "logs", "counts"] {
        std::fs::create_dir_all(root.join(d))
            .with_context(|| format!("create workspace dir {}", root.join(d).display()))?;
    }
    let control_port = free_port()?;
    // Forward slashes so the TOML basic string stays valid on Windows.
    let log_dir = root.join("logs").to_string_lossy().replace('\\', "/");
    // No [webui] section: the bench environment never serves the console.
    std::fs::write(
        root.join("daemon.toml"),
        format!(
            "[daemon]\nhost = \"127.0.0.1\"\nport = {control_port}\nlog_dir = \"{log_dir}\"\nlog_level = \"warn\"\nmonitor_interval = 0.3\n",
        ),
    )
    .with_context(|| format!("write daemon.toml in {}", root.display()))?;

    // The daemon must not inherit bench's stdio: its output lands in
    // workspace files so a crash leaves evidence.
    let out = std::fs::File::create(root.join("daemon.out.log"))?;
    let err = out.try_clone()?;
    let child = Command::new(bin)
        .arg("--config")
        .arg(root.join("daemon.toml"))
        .arg("run")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .with_context(|| format!("spawn daemon {}", bin.display()))?;
    *DAEMON_CHILD.lock().unwrap() = Some(child);

    let client = Client::new(&format!("127.0.0.1:{control_port}"), None)?;
    wait_health(&client, 20)?;
    let daemon_version = client.daemon_version()?;
    let daemon_pid = {
        let guard = DAEMON_CHILD.lock().unwrap();
        guard.as_ref().map(|c| c.id())
    };
    Ok(Site {
        app_dir: root.join("apps"),
        log_dir: root.join("logs"),
        count_dir: root.join("counts"),
        client,
        daemon_version,
        daemon_pid,
        spawn: true,
    })
}

// -- connect topology ----------------------------------------------------------

/// Connect to a running daemon and resolve where its apps/logs live. Refuses
/// to run when any bench-prefixed app already exists there (design D7).
pub fn connect_site(addr: &str, token: Option<&str>) -> Result<Site> {
    let client = Client::new(addr, token)?;
    client
        .health()
        .with_context(|| format!("cannot reach the target daemon at {addr}"))?;
    let daemon_version = client.daemon_version()?;
    let config_source = client.config_source()?;
    let paths = dcfg::discover(Path::new(&config_source))?;

    // Collision check BEFORE registering anything: refuse rather than touch
    // or delete anything bench did not create.
    let existing_programs = client.bench_prefixed_programs()?;
    if !existing_programs.is_empty() {
        bail!(
            "the target daemon already has bench app(s) {existing_programs:?} — refusing to \
             run (another bench measurement may be in progress; they must finish first)"
        );
    }
    let existing_files = bench_app_files(&paths.app_dir)?;
    if !existing_files.is_empty() {
        bail!(
            "the target daemon's app_dir already holds bench registration(s) {:?} — refusing \
             to run",
            existing_files
        );
    }

    let count_dir = std::env::temp_dir().join(format!(
        "xk-bench-counts-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let _ = std::fs::remove_dir_all(&count_dir);
    Ok(Site {
        client,
        app_dir: paths.app_dir,
        log_dir: paths.log_dir,
        count_dir,
        daemon_version,
        daemon_pid: None,
        spawn: false,
    })
}

fn bench_app_files(app_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !app_dir.exists() {
        return Ok(out);
    }
    for e in std::fs::read_dir(app_dir)
        .with_context(|| format!("scan app_dir {}", app_dir.display()))?
    {
        let p = e?.path();
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if name.starts_with(BENCH_PREFIX) && name.ends_with(".toml") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

// -- cleanup -------------------------------------------------------------------

/// Remove every log file of one bench program: live out/err files plus all
/// rotation suffixes.
fn remove_log_files(log_dir: &Path, name: &str) {
    for stream in ["out", "err"] {
        let live = log_dir.join(format!("{name}.{stream}.log"));
        let _ = std::fs::remove_file(&live);
        // rotation chain files
        if let Ok(entries) = std::fs::read_dir(log_dir) {
            let suffix = format!("{name}.{stream}.log.");
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with(&suffix)
                    && n[suffix.len()..].bytes().all(|b| b.is_ascii_digit())
                    && n.len() > suffix.len()
                {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

fn wait_and_kill_daemon_child() {
    let mut guard = DAEMON_CHILD.lock().unwrap();
    if let Some(child) = guard.as_mut() {
        for _ in 0..60 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Best-effort resource cleanup shared by the normal exit path and ctrl-C.
/// Failures are warnings — never panic here (we may run inside a handler).
pub fn run_cleanup(ctx: &CleanupCtx) {
    // 1. Unregister the bench apps: delete the registry files; in connect
    //    mode make the running daemon drop (and thereby stop) the programs.
    let drop_app_files = !(ctx.spawn && ctx.keep);
    if drop_app_files {
        for f in &ctx.app_files {
            if let Err(e) = std::fs::remove_file(f) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("[bench] warning: cannot remove app file {}: {e}", f.display());
                }
            }
        }
        if !ctx.spawn {
            let _ = ctx.client.reload();
            // Scoped per bench app: pending user changes on the target
            // daemon must not be applied by our cleanup.
            for name in &ctx.app_names {
                let _ = ctx.client.apply(Some(name));
            }
        }
    }
    // 2. Log files (unless --keep asked to keep the scene).
    if !ctx.keep {
        // give the daemon's pumps a beat to finish the last batch
        std::thread::sleep(Duration::from_millis(300));
        for name in &ctx.app_names {
            remove_log_files(&ctx.log_dir, name);
        }
    }
    // 3. Spawn mode: stop the daemon and delete the workspace.
    if ctx.spawn {
        let _ = ctx.client.shutdown();
        wait_and_kill_daemon_child();
        if !ctx.keep {
            if let Some(root) = &ctx.root {
                if let Err(e) = std::fs::remove_dir_all(root) {
                    eprintln!(
                        "[bench] warning: cannot remove workspace {}: {e}",
                        root.display()
                    );
                }
            }
        }
    }
    // 4. Transient counter dir (kept with the workspace in spawn+keep).
    if !(ctx.spawn && ctx.keep) {
        if let Some(cd) = &ctx.count_dir {
            let _ = std::fs::remove_dir_all(cd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_prefers_explicit_and_bails_when_missing() {
        assert!(discover_daemon(Some(Path::new("/nonexistent/xkeeper"))).is_err());
        // sibling/manifest discovery is environment dependent; only assert the
        // explicit path contract here.
    }

    #[test]
    fn cleanup_is_idempotent_on_missing_files() {
        let dir = std::env::temp_dir().join(format!("xk-bench-cln-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = CleanupCtx {
            client: Client::new("127.0.0.1:9", None).unwrap(),
            app_files: vec![dir.join("nope.toml")],
            app_names: vec!["xkeeper-bench-nope".into()],
            log_dir: dir.clone(),
            root: None,
            count_dir: Some(dir.clone()),
            keep: false,
            spawn: false,
        };
        // must not panic even though the daemon at :9 is unreachable
        run_cleanup(&ctx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_log_files_takes_the_whole_chain() {
        let dir = std::env::temp_dir().join(format!("xk-bench-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "p.out.log",
            "p.out.log.1",
            "p.out.log.2",
            "p.err.log",
            "p.err.log.1",
            "other.out.log", // untouched
        ] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        remove_log_files(&dir, "p");
        assert!(!dir.join("p.out.log").exists());
        assert!(!dir.join("p.out.log.1").exists());
        assert!(!dir.join("p.out.log.2").exists());
        assert!(!dir.join("p.err.log").exists());
        assert!(!dir.join("p.err.log.1").exists());
        assert!(dir.join("other.out.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
