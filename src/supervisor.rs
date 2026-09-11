//! The supervisor: shared state, the command-driven main loop, ordered
//! startup, per-app reload and shutdown orchestration.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};

use crate::config::{self, DaemonConfig, ResolvedApp, ResolvedProgram};
use crate::health;
use crate::program::{ManagedProgram, ProgramState};
use crate::registry;

pub type Reply = std::sync::mpsc::Sender<Result<String>>;

#[derive(Debug)]
pub enum Command {
    Start {
        name: String,
        reply: Option<Reply>,
    },
    Stop {
        name: String,
        reply: Option<Reply>,
    },
    Restart {
        name: String,
        reply: Option<Reply>,
    },
    Reload {
        reply: Option<Reply>,
    },
    Apply {
        scope: ApplyScope,
        restart: bool,
        reply: Option<Reply>,
    },
    Shutdown {
        reply: Option<Reply>,
    },
    /// Sent by the health checker when a program needs a restart.
    HealthRestart {
        name: String,
    },
}

/// Range an `apply` acts on: everything, one app, or one program.
#[derive(Debug, Clone)]
pub enum ApplyScope {
    All,
    App(String),
    Program(String, String),
}

impl ApplyScope {
    pub fn matches(&self, app: &str, program: &str) -> bool {
        match self {
            ApplyScope::All => true,
            ApplyScope::App(a) => a == app,
            ApplyScope::Program(a, p) => a == app && p == program,
        }
    }
}

#[derive(Clone)]
pub struct AppRecord {
    pub name: String,
    #[allow(dead_code)]
    pub path: PathBuf,
    pub autostart: bool,
    pub priority: i32,
}

/// One program's entry in the pending-change projection: the on-disk config
/// differs from what the daemon is currently running (or the program is not
/// yet known to the daemon at all).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingProgram {
    pub app: String,
    pub program: String,
    /// Running now (`true`) vs not running (`false`): a changed program that
    /// is up will restart on apply; a stopped one only gets redefined.
    pub running: bool,
}

/// The detected-but-not-yet-applied configuration diff, as shown by
/// `GET /v1/pending`, the `reload` preview and the WS snapshot.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PendingDoc {
    pub programs: Vec<PendingProgram>,
    pub apps_added: Vec<String>,
    pub apps_removed: Vec<String>,
    /// Daemon fields that cannot hot-apply (host/port/auth...).
    pub daemon_hints: Vec<String>,
    /// Per-app load failures isolated by detection (bad files keep the old
    /// definition; the text explains what is wrong).
    pub errors: Vec<String>,
}

impl PendingDoc {
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty()
            && self.apps_added.is_empty()
            && self.apps_removed.is_empty()
            && self.daemon_hints.is_empty()
            && self.errors.is_empty()
    }
}

/// What an apply did to one program (one entry per in-scope program).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgramAction {
    pub app: String,
    pub program: String,
    /// Disk config differs from the running definition (new = changed).
    pub changed: bool,
    /// update-and-restart | update-only | restart | start | keep-stopped |
    /// none | remove
    pub action: String,
    /// "ok" or a short failure explanation.
    pub result: String,
}

/// The structured result of one `apply`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ApplyResult {
    pub programs: Vec<ProgramAction>,
    pub apps_added: Vec<String>,
    pub apps_removed: Vec<String>,
    pub daemon_hints: Vec<String>,
    /// Apps whose (re)load failed during this apply; their pending entry is
    /// kept so a later apply can retry.
    pub errors: Vec<String>,
    /// One human-readable line ("no changes" when nothing was pending).
    pub summary: String,
}

/// A full detection pass: what differs between disk and the running
/// daemon, without touching any process.
struct Detection {
    config: DaemonConfig,
    /// Apps that loaded cleanly (their definitions may still differ from
    /// what is running — that is what `pending` is about).
    resolved: Vec<ResolvedApp>,
    records: Vec<AppRecord>,
    /// Apps whose files failed to load; they keep their old definition.
    failed: Vec<String>,
}

/// Everything the API threads read and the main loop mutates.
pub struct SupervisorState {
    pub programs: HashMap<String, ManagedProgram>,
    pub apps: Vec<AppRecord>,
    pub config: DaemonConfig,
    pub config_dir: PathBuf,
    pub shutdown: AtomicBool,
    /// Latest detection outcome (disk config vs running definitions).
    /// Written by the supervision loop, read by the API threads.
    pub pending: PendingDoc,
    /// mtime+size snapshot per config file, to skip re-parsing unchanged
    /// files on every detect pass.
    pub file_stamps: HashMap<PathBuf, (std::time::SystemTime, u64)>,
}

pub struct Supervisor {
    pub state: Arc<Mutex<SupervisorState>>,
    queue: Mutex<VecDeque<Command>>,
    cv: Condvar,
    pub health_tasks: health::TaskMap,
    /// Latest metrics from the sideband sampler thread; the only writer is
    /// [`crate::metrics::spawn_sampler`], the supervision loop never touches it.
    pub metrics: Arc<crate::metrics::MetricsTable>,
    config_path: Mutex<PathBuf>,
    started: std::time::Instant,
    interval: Duration,
}

impl Supervisor {
    pub fn new(config: DaemonConfig, config_dir: &Path) -> Result<Arc<Self>> {
        let log_dir = config::resolve_path(&config.daemon.log_dir, config_dir);
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("failed to create log dir: {}", log_dir.display()))?;
        let interval = Duration::from_secs_f64(config.daemon.monitor_interval.max(0.05));
        Ok(Arc::new(Supervisor {
            state: Arc::new(Mutex::new(SupervisorState {
                programs: HashMap::new(),
                apps: Vec::new(),
                config,
                config_dir: config_dir.to_path_buf(),
                shutdown: AtomicBool::new(false),
                pending: PendingDoc::default(),
                file_stamps: HashMap::new(),
            })),
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            health_tasks: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(crate::metrics::MetricsTable::new()),
            config_path: Mutex::new(config_dir.join("xkeeper.toml")),
            started: std::time::Instant::now(),
            interval,
        }))
    }

    /// Remember where the daemon config lives so `reload` re-reads it.
    pub fn set_config_path(&self, p: &Path) {
        *self.config_path.lock().unwrap() = p.to_path_buf();
    }

    /// Where the daemon config lives (config-source display for the console).
    pub fn config_path(&self) -> PathBuf {
        self.config_path.lock().unwrap().clone()
    }

    /// How long the daemon has been running (console overview).
    pub fn uptime_secs(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn enqueue(&self, cmd: Command) {
        self.queue.lock().unwrap().push_back(cmd);
        self.cv.notify_all();
    }

    pub fn request_shutdown(&self) {
        self.state
            .lock()
            .unwrap()
            .shutdown
            .store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    fn pop_command(&self) -> Option<Command> {
        self.queue.lock().unwrap().pop_front()
    }

    // -- bootstrap ----------------------------------------------------------

    /// Load every registered app and start eligible programs.
    pub fn bootstrap(&self) -> Result<()> {
        let (config, config_dir) = {
            let st = self.state.lock().unwrap();
            (st.config.clone(), st.config_dir.clone())
        };
        let listed = registry::list(&config, &config_dir)?;
        let mut records = Vec::new();
        let mut resolved: Vec<ResolvedApp> = Vec::new();
        for l in &listed {
            if let Some(reason) = &l.broken {
                error!("app[{}] failed to load at startup (skipped): {}", l.name, reason);
                continue;
            }
            match registry::reload_app(&l.name, &l.path, config.app_default.as_ref()) {
                Ok(a) => {
                    records.push(AppRecord {
                        name: a.name.clone(),
                        path: a.path.clone(),
                        autostart: a.autostart,
                        priority: a.priority,
                    });
                    resolved.push(a);
                }
                Err(e) => error!("app[{}] failed to load at startup (skipped): {e:#}", l.name),
            }
        }
        // Uniqueness of program names across apps is enforced at add-time, but
        // files may have drifted since; report instead of refusing to boot.
        if let Err(e) = config::validate_all(&resolved) {
            error!("cross-app validation problem at startup: {e}");
        }
        {
            let mut st = self.state.lock().unwrap();
            let log_dir = config::resolve_path(&st.config.daemon.log_dir, &st.config_dir);
            let ring_cap = st.config.daemon.log_buffer_lines;
            for app in &resolved {
                for p in app.programs.clone() {
                    st.programs.insert(
                        p.name.clone(),
                        ManagedProgram::new(p, log_dir.clone(), ring_cap),
                    );
                }
            }
            st.apps = records;
        }
        self.sync_health_tasks();
        {
            let st = self.state.lock().unwrap();
            info!(
                "bootstrap: {} app(s), {} program(s)",
                st.apps.len(),
                st.programs.len()
            );
        }
        self.start_eligible();
        Ok(())
    }

    fn sync_health_tasks(&self) {
        let st = self.state.lock().unwrap();
        let mut tasks = self.health_tasks.lock().unwrap();
        tasks.clear();
        for p in st.programs.values() {
            if let Some(h) = &p.def.health {
                tasks.insert(p.def.name.clone(), health::HealthTask { check: h.clone() });
            }
        }
    }

    // -- ordered startup ----------------------------------------------------

    /// Priority-ordered startup with dependency waiting/giving-up. Programs
    /// whose dependencies are not yet running stay `stopped` (with a wait
    /// reason); permanently unavailable dependencies make them `fatal`.
    pub fn start_eligible(&self) {
        enum Action {
            Spawn,
            Fatalize(String),
        }
        loop {
            let snapshot: Vec<(String, Vec<String>)> = {
                let st = self.state.lock().unwrap();
                let mut eligible: Vec<&AppRecord> =
                    st.apps.iter().filter(|a| a.autostart).collect();
                eligible.sort_by_key(|a| (a.priority, a.name.clone()));
                eligible
                    .iter()
                    .flat_map(|a| {
                        let mut names: Vec<&String> = st
                            .programs
                            .values()
                            .filter(|p| p.def.app == a.name)
                            .map(|p| &p.def.name)
                            .collect();
                        names.sort();
                        names.into_iter().cloned().collect::<Vec<_>>()
                    })
                    .map(|n| {
                        let deps = st
                            .programs
                            .get(&n)
                            .map(|p| p.def.depends_on.clone())
                            .unwrap_or_default();
                        (n, deps)
                    })
                    .collect()
            };
            // Pass 1: decide (immutable).
            let mut actions: Vec<(String, Action)> = Vec::new();
            {
                let st = self.state.lock().unwrap();
                for (name, deps) in &snapshot {
                    let Some(p) = st.programs.get(name) else {
                        continue;
                    };
                    if p.state() != ProgramState::Stopped || p.user_stopped() {
                        continue;
                    }
                    let mut ready = true;
                    let mut dead = Vec::new();
                    for d in deps {
                        match st.programs.get(d) {
                            Some(dep) => match dep.state() {
                                ProgramState::Running => {}
                                ProgramState::Fatal => dead.push(format!(
                                    "{d} (fatal: {})",
                                    dep.fatal_reason().unwrap_or("unknown")
                                )),
                                ProgramState::Exited => dead.push(d.clone()),
                                _ => ready = false,
                            },
                            None => dead.push(format!("{d} (not registered)")),
                        }
                    }
                    if !dead.is_empty() {
                        actions.push((name.clone(), Action::Fatalize(dead.join(", "))));
                    } else if ready {
                        actions.push((name.clone(), Action::Spawn));
                    }
                }
            }
            // Pass 2: apply (mutable).
            let mut progress = false;
            {
                let mut st = self.state.lock().unwrap();
                for (name, action) in actions {
                    match action {
                        Action::Spawn => {
                            if let Some(p) = st.programs.get_mut(&name) {
                                if p.state() == ProgramState::Stopped {
                                    p.spawn();
                                    progress = true;
                                }
                            }
                        }
                        Action::Fatalize(reason) => {
                            if let Some(p) = st.programs.get_mut(&name) {
                                p.mark_dependency_fatal(reason);
                            }
                        }
                    }
                }
            }
            if !progress {
                break;
            }
        }
    }

    // -- command execution --------------------------------------------------

    fn execute(&self, cmd: Command) {
        match cmd {
            Command::Start { name, reply } => answer(reply, self.cmd_start(&name)),
            Command::Stop { name, reply } => answer(reply, self.cmd_stop(&name)),
            Command::Restart { name, reply } => {
                let r = self.cmd_stop(&name).and_then(|_| self.cmd_start(&name));
                answer(reply, r);
            }
            Command::HealthRestart { name } => {
                warn!("health checker requesting restart of program[{name}]");
                let _ = self.cmd_stop(&name);
                let _ = self.cmd_start(&name);
            }
            Command::Reload { reply } => answer(reply, self.cmd_reload()),
            Command::Apply {
                scope,
                restart,
                reply,
            } => answer(reply, self.cmd_apply(&scope, restart)),
            Command::Shutdown { reply } => {
                answer(reply, Ok("shutting down".to_string()));
                self.request_shutdown();
            }
        }
    }

    fn cmd_start(&self, name: &str) -> Result<String> {
        let mut st = self.state.lock().unwrap();
        let p = st
            .programs
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("unknown program {name:?}"))?;
        if matches!(p.state(), ProgramState::Running | ProgramState::Starting) {
            bail!("program {name:?} is already {}", p.state());
        }
        p.clear_fatal();
        p.spawn();
        Ok(format!("program {name} is {}", p.state()))
    }

    fn cmd_stop(&self, name: &str) -> Result<String> {
        let mut st = self.state.lock().unwrap();
        let p = st
            .programs
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("unknown program {name:?}"))?;
        if matches!(
            p.state(),
            ProgramState::Stopped | ProgramState::Exited | ProgramState::Fatal
        ) {
            return Ok(format!("program {name} was already {}", p.state()));
        }
        p.stop();
        Ok(format!("program {name} is stopped"))
    }

    // -- reload / apply -----------------------------------------------------

    /// Rescan the registry and daemon config, compute the pending diff and
    /// publish it. `rescan` forces a full pass (reload/apply commands); the
    /// periodic tick passes `false` so unchanged files (mtime+size match)
    /// skip re-parsing. Errors mean the daemon config itself is invalid —
    /// the previously published pending state is then left untouched.
    fn detect(&self, rescan: bool) -> Result<PendingDoc> {
        let config_path = self.config_path.lock().unwrap().clone();
        let (new_config, _) = DaemonConfig::load_or_default(&config_path)
            .map_err(|e| anyhow::anyhow!("detection failed, daemon config invalid: {e:#}"))?;
        let config_dir = self.state.lock().unwrap().config_dir.clone();
        let defaults = new_config.app_default.clone();

        let listed = registry::list(&new_config, &config_dir)?;
        let stamps_now = self.state.lock().unwrap().file_stamps.clone();
        let cached_defs: Mutex<HashMap<PathBuf, ResolvedApp>> = Mutex::new(HashMap::new());
        let mut errors: Vec<String> = Vec::new();
        let mut new_records: Vec<AppRecord> = Vec::new();
        let mut resolved: Vec<ResolvedApp> = Vec::new();

        for l in &listed {
            if let Some(reason) = &l.broken {
                errors.push(format!("app[{}] kept old config: {reason}", l.name));
                let st = self.state.lock().unwrap();
                if let Some(old) = st.apps.iter().find(|a| a.name == l.name) {
                    new_records.push(old.clone());
                }
                continue;
            }
            // Fast path: file unchanged since the last detection — reuse
            // the disk-side definitions we parsed last time (NOT the running
            // ones: a changed file stays "changed" until applied).
            if !rescan
                && file_stamp(&l.path).map(|s| stamps_now.get(&l.path) == Some(&s))
                    == Some(true)
            {
                let cached = cached_defs.lock().unwrap().get(&l.path).cloned();
                if let Some(a) = cached {
                    new_records.push(AppRecord {
                        name: a.name.clone(),
                        path: a.path.clone(),
                        autostart: a.autostart,
                        priority: a.priority,
                    });
                    resolved.push(a);
                    continue;
                }
                // No cached parse yet (first pass after boot): fall through
                // to a full parse below.
            }
            match registry::reload_app(&l.name, &l.path, defaults.as_ref()) {
                Ok(a) => {
                    cached_defs.lock().unwrap().insert(l.path.clone(), a.clone());
                    new_records.push(AppRecord {
                        name: a.name.clone(),
                        path: a.path.clone(),
                        autostart: a.autostart,
                        priority: a.priority,
                    });
                    resolved.push(a);
                }
                Err(e) => {
                    errors.push(format!("app[{}] kept old config: {e:#}", l.name));
                    let st = self.state.lock().unwrap();
                    if let Some(old) = st.apps.iter().find(|a| a.name == l.name) {
                        new_records.push(old.clone());
                    }
                }
            }
        }
        if let Err(e) = config::validate_all(&resolved) {
            // Cross-app conflict: refuse to publish any pending state; the
            // running config stays authoritative until the conflict is fixed.
            bail!("detection failed, cross-app validation failed: {e}");
        }

        let mut doc = PendingDoc {
            errors,
            ..PendingDoc::default()
        };
        {
            let st = self.state.lock().unwrap();
            // Apps no longer registered on disk.
            let keep: HashSet<String> = new_records.iter().map(|a| a.name.clone()).collect();
            for a in &st.apps {
                if !keep.contains(&a.name) {
                    doc.apps_removed.push(a.name.clone());
                }
            }
            // Per-app program diff: hash mismatch, new program, or dropped
            // program (file no longer defines it) — all count as changed.
            for app in &resolved {
                if !st.apps.iter().any(|a| a.name == app.name) {
                    doc.apps_added.push(app.name.clone());
                }
                let new_names: HashSet<&str> =
                    app.programs.iter().map(|p| p.name.as_str()).collect();
                for def in &app.programs {
                    let changed = match st.programs.get(&def.name) {
                        Some(p) => p.def.hash != def.hash,
                        None => true,
                    };
                    if changed {
                        let running = st
                            .programs
                            .get(&def.name)
                            .map(|p| p.is_running())
                            .unwrap_or(false);
                        doc.programs.push(PendingProgram {
                            app: app.name.clone(),
                            program: def.name.clone(),
                            running,
                        });
                    }
                }
                for p in st.programs.values() {
                    if p.def.app == app.name && !new_names.contains(p.def.name.as_str()) {
                        doc.programs.push(PendingProgram {
                            app: app.name.clone(),
                            program: p.def.name.clone(),
                            running: p.is_running(),
                        });
                    }
                }
            }
            // Daemon fields that cannot hot-apply.
            let old_daemon = &st.config.daemon;
            if old_daemon.port != new_config.daemon.port
                || old_daemon.host != new_config.daemon.host
            {
                doc.daemon_hints
                    .push("daemon host/port changed: restart xkeeper to apply".into());
            }
            if old_daemon.auth_token != new_config.daemon.auth_token {
                doc.daemon_hints
                    .push("daemon auth_token changed: restart xkeeper to apply".into());
            }
        }
        // Refresh file stamps (all files we parsed).
        let mut stamps = HashMap::new();
        for l in &listed {
            if let Some(s) = file_stamp(&l.path) {
                stamps.insert(l.path.clone(), s);
            }
        }
        {
            let mut st = self.state.lock().unwrap();
            st.file_stamps = stamps;
            st.pending = doc.clone();
        }
        Ok(doc)
    }

    /// `reload` now only detects: rescan and publish the pending preview.
    fn cmd_reload(&self) -> Result<String> {
        let doc = self.detect(true)?;
        Ok(render_pending(&doc))
    }

    /// `apply`: detect fresh, then act on the in-scope part of the diff.
    fn cmd_apply(&self, scope: &ApplyScope, restart: bool) -> Result<String> {
        let det = self.detect_details()?;
        let result = self.apply_detection(&det, scope, restart)?;
        let s = render_apply(&result);
        Ok(s)
    }

    /// Like [`Self::detect`] but returns the loaded definitions so they can
    /// be applied (same isolation rules: broken apps keep their old config
    /// and stay in `failed`).
    fn detect_details(&self) -> Result<Detection> {
        let config_path = self.config_path.lock().unwrap().clone();
        let (new_config, _) = DaemonConfig::load_or_default(&config_path)
            .map_err(|e| anyhow::anyhow!("apply aborted, daemon config invalid: {e:#}"))?;
        let config_dir = self.state.lock().unwrap().config_dir.clone();
        let defaults = new_config.app_default.clone();
        let listed = registry::list(&new_config, &config_dir)?;
        let mut failed: Vec<String> = Vec::new();
        let mut new_records: Vec<AppRecord> = Vec::new();
        let mut resolved: Vec<ResolvedApp> = Vec::new();
        for l in &listed {
            if let Some(reason) = &l.broken {
                failed.push(format!("app[{}] kept old config: {reason}", l.name));
                let st = self.state.lock().unwrap();
                if let Some(old) = st.apps.iter().find(|a| a.name == l.name) {
                    new_records.push(old.clone());
                }
                continue;
            }
            match registry::reload_app(&l.name, &l.path, defaults.as_ref()) {
                Ok(a) => {
                    new_records.push(AppRecord {
                        name: a.name.clone(),
                        path: a.path.clone(),
                        autostart: a.autostart,
                        priority: a.priority,
                    });
                    resolved.push(a);
                }
                Err(e) => {
                    failed.push(format!("app[{}] kept old config: {e:#}", l.name));
                    let st = self.state.lock().unwrap();
                    if let Some(old) = st.apps.iter().find(|a| a.name == l.name) {
                        new_records.push(old.clone());
                    }
                }
            }
        }
        if let Err(e) = config::validate_all(&resolved) {
            bail!("apply aborted, running config unchanged: cross-app validation failed: {e}");
        }
        Ok(Detection {
            config: new_config,
            resolved,
            records: new_records,
            failed,
        })
    }

    /// Mutate the running state per the detection and scope. Reuses the
    /// reload rules: changed programs are stopped, redefined and — when
    /// they were alive (running/starting) or waiting out a crash backoff —
    /// respawned; programs the user stopped stay stopped.
    fn apply_detection(
        &self,
        det: &Detection,
        scope: &ApplyScope,
        restart: bool,
    ) -> Result<ApplyResult> {
        let new_config = &det.config;
        let resolved = &det.resolved;
        let new_records = &det.records;
        let mut result = ApplyResult {
            errors: det.failed.clone(),
            ..ApplyResult::default()
        };
        // Which programs existed before (for app add/remove bookkeeping).
        let prior_apps: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.apps.iter().map(|a| a.name.clone()).collect()
        };

        {
            let mut st = self.state.lock().unwrap();
            let log_dir = config::resolve_path(&st.config.daemon.log_dir, &st.config_dir);
            let ring_cap = st.config.daemon.log_buffer_lines;

            // Apps no longer registered: stop and drop all their programs.
            let keep: HashSet<String> = new_records.iter().map(|a| a.name.clone()).collect();
            let dropped: Vec<String> = st
                .programs
                .values()
                .filter(|p| !keep.contains(&p.def.app))
                .map(|p| p.def.name.clone())
                .collect();
            for name in dropped {
                let in_scope = st
                    .programs
                    .get(&name)
                    .map(|p| scope.matches(&p.def.app, &p.def.name))
                    .unwrap_or(false);
                if !in_scope {
                    continue;
                }
                if let Some(mut p) = st.programs.remove(&name) {
                    info!("program[{name}] stopped (app unregistered)");
                    p.stop();
                    result.programs.push(ProgramAction {
                        app: p.def.app.clone(),
                        program: name.clone(),
                        changed: true,
                        action: "remove".into(),
                        result: "ok".into(),
                    });
                }
            }

            // Per-app program diff.
            for app in resolved {
                let new_defs: &Vec<ResolvedProgram> = &app.programs;
                let new_names: HashSet<&str> = new_defs.iter().map(|p| p.name.as_str()).collect();
                let old_names: Vec<String> = st
                    .programs
                    .values()
                    .filter(|p| p.def.app == app.name)
                    .map(|p| p.def.name.clone())
                    .collect();
                for old in old_names {
                    if !new_names.contains(old.as_str()) {
                        let in_scope = st
                            .programs
                            .get(&old)
                            .map(|p| scope.matches(&p.def.app, &p.def.name))
                            .unwrap_or(false);
                        if !in_scope {
                            continue;
                        }
                        if let Some(mut p) = st.programs.remove(&old) {
                            info!("program[{old}] removed by apply");
                            p.stop();
                            result.programs.push(ProgramAction {
                                app: p.def.app.clone(),
                                program: old.clone(),
                                changed: true,
                                action: "remove".into(),
                                result: "ok".into(),
                            });
                        }
                    }
                }
                for def in new_defs {
                    if !scope.matches(&app.name, &def.name) {
                        continue;
                    }
                    match st.programs.get(&def.name) {
                        Some(p) if p.def.hash == def.hash => {
                            // Unchanged definition. `--restart` restarts it
                            // unless the user stopped it; a program that
                            // crashed mid-retry (backoff) is pulled up.
                            if restart {
                                let s = p.state();
                                let user_stopped = p.user_stopped();
                                if user_stopped || s == ProgramState::Stopped {
                                    result.programs.push(ProgramAction {
                                        app: app.name.clone(),
                                        program: def.name.clone(),
                                        changed: false,
                                        action: "keep-stopped".into(),
                                        result: "ok".into(),
                                    });
                                } else {
                                    let p = st.programs.get_mut(&def.name).unwrap();
                                    if s == ProgramState::Backoff {
                                        p.stop();
                                    }
                                    p.spawn();
                                    result.programs.push(ProgramAction {
                                        app: app.name.clone(),
                                        program: def.name.clone(),
                                        changed: false,
                                        action: "restart".into(),
                                        result: "ok".into(),
                                    });
                                }
                            }
                            // Unchanged and no --restart: nothing happened,
                            // no entry (keeps "no changes" true).
                        }
                        Some(_) => {
                            // Changed definition: stop, rebuild; respawn only
                            // if it was alive (running/starting) or waiting
                            // out a crash backoff — never when the user
                            // stopped it.
                            let mut old = st.programs.remove(&def.name).unwrap();
                            let was_alive =
                                old.is_running() || matches!(old.state(), ProgramState::Backoff);
                            let keep_stopped = old.user_stopped() && !was_alive;
                            old.stop();
                            let mut np =
                                ManagedProgram::new(def.clone(), log_dir.clone(), ring_cap);
                            if was_alive {
                                np.spawn();
                            } else if keep_stopped {
                                // The user stopped this program; a redefinition
                                // must not resurrect it (start_eligible would
                                // otherwise autostart it again).
                                let _ = np.stop();
                            }
                            st.programs.insert(def.name.clone(), np);
                            result.programs.push(ProgramAction {
                                app: app.name.clone(),
                                program: def.name.clone(),
                                changed: true,
                                action: if was_alive {
                                    "update-and-restart".into()
                                } else {
                                    "update-only".into()
                                },
                                result: "ok".into(),
                            });
                        }
                        None => {
                            // New program: apply the app's autostart (with
                            // `--restart` we start it regardless).
                            let mut np =
                                ManagedProgram::new(def.clone(), log_dir.clone(), ring_cap);
                            let started = if app.autostart || restart {
                                np.spawn();
                                true
                            } else {
                                false
                            };
                            st.programs.insert(def.name.clone(), np);
                            result.programs.push(ProgramAction {
                                app: app.name.clone(),
                                program: def.name.clone(),
                                changed: true,
                                action: if started { "start" } else { "none" }.into(),
                                result: "ok".into(),
                            });
                        }
                    }
                }
            }

            // Daemon fields that cannot hot-apply.
            let old_daemon = st.config.daemon.clone();
            if old_daemon.port != new_config.daemon.port
                || old_daemon.host != new_config.daemon.host
            {
                result
                    .daemon_hints
                    .push("daemon host/port changed: restart xkeeper to apply".into());
            }
            if old_daemon.auth_token != new_config.daemon.auth_token {
                result
                    .daemon_hints
                    .push("daemon auth_token changed: restart xkeeper to apply".into());
            }
            st.config = new_config.clone();
            st.apps = new_records.clone();
        }

        // App add/remove bookkeeping for the result.
        let now_apps: HashSet<String> = new_records.iter().map(|a| a.name.clone()).collect();
        result.apps_removed = prior_apps
            .iter()
            .filter(|a| !now_apps.contains(*a))
            .cloned()
            .collect();
        result.apps_added = new_records
            .iter()
            .map(|a| a.name.clone())
            .filter(|a| !prior_apps.contains(a))
            .collect();

        // Summary line.
        let changed = result.programs.iter().filter(|p| p.changed).count();
        if result.programs.is_empty()
            && result.apps_added.is_empty()
            && result.apps_removed.is_empty()
        {
            result.summary = "no changes".into();
        } else {
            let restarted = result
                .programs
                .iter()
                .filter(|p| p.action.contains("restart") || p.action == "start")
                .count();
            result.summary = format!(
                "applied: {changed} program change(s), {restarted} restart/start(s)"
            );
        }
        self.sync_health_tasks();
        self.start_eligible();
        // Re-detect so the published pending reflects the post-apply truth
        // (a scoped apply may leave other apps still pending).
        let _ = self.detect(true);
        for e in &result.errors {
            error!("apply: {e}");
        }
        Ok(result)
    }

    // -- shutdown -----------------------------------------------------------

    /// Stop everything, highest priority first (reverse of startup order).
    pub fn shutdown_all(&self) {
        let order: Vec<String> = {
            let st = self.state.lock().unwrap();
            let mut apps = st.apps.clone();
            apps.sort_by_key(|a| std::cmp::Reverse((a.priority, a.name.clone())));
            apps.iter()
                .flat_map(|a| {
                    let mut names: Vec<String> = st
                        .programs
                        .values()
                        .filter(|p| p.def.app == a.name)
                        .map(|p| p.def.name.clone())
                        .collect();
                    names.sort();
                    names
                })
                .collect()
        };
        info!(
            "shutdown: stopping {} program(s) in reverse order",
            order.len()
        );
        let mut st = self.state.lock().unwrap();
        for name in order {
            if let Some(p) = st.programs.get_mut(&name) {
                p.stop();
            }
        }
    }

    // -- main loop ----------------------------------------------------------

    /// Consume commands, tick programs, until shutdown. Runs on the spawned
    /// supervisor thread.
    pub fn run(self: &Arc<Self>) {
        info!(
            "supervisor loop started (interval {:.1}s)",
            self.interval.as_secs_f64()
        );
        while !self.state.lock().unwrap().shutdown.load(Ordering::SeqCst) {
            while let Some(cmd) = self.pop_command() {
                self.execute(cmd);
            }
            if self.state.lock().unwrap().shutdown.load(Ordering::SeqCst) {
                break;
            }
            // Periodic detection: disk edits become pending without any
            // manual `reload`. The mtime+size fast path keeps this cheap.
            if let Err(e) = self.detect(false) {
                warn!("periodic detection skipped: {e:#}");
            }
            {
                let mut st = self.state.lock().unwrap();
                let now = Instant::now();
                for p in st.programs.values_mut() {
                    p.tick(now);
                }
            }
            // Re-evaluate dependencies each pass: a dependency that reached
            // running (or became permanently unavailable) unblocks (or
            // fatalizes) waiting programs.
            self.start_eligible();
            // Sleep in slices so commands and shutdown wake us promptly.
            let deadline = Instant::now() + self.interval;
            while Instant::now() < deadline {
                if self.state.lock().unwrap().shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let guard = self.queue.lock().unwrap();
                let remaining = deadline.saturating_duration_since(Instant::now());
                let slice = remaining.min(Duration::from_millis(100));
                let (guard, _) = self.cv.wait_timeout(guard, slice).unwrap();
                let pending = !guard.is_empty();
                drop(guard);
                if pending {
                    break;
                }
            }
        }
        info!("shutdown requested, stopping all programs...");
        self.shutdown_all();
        info!("all programs stopped, xkeeper exits");
    }
}

/// mtime+size of a config file, or None when it cannot be stat'ed.
fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn answer(reply: Option<Reply>, r: Result<String>) {
    if let Some(tx) = reply {
        let _ = tx.send(r);
    }
}

// -- human-readable rendering ---------------------------------------------

/// Render a pending preview: what would change if `apply` ran now.
pub fn render_pending(doc: &PendingDoc) -> String {
    if doc.is_empty() {
        return "no pending changes".into();
    }
    let mut out = Vec::new();
    if !doc.programs.is_empty() {
        out.push(format!("pending changes ({}):", doc.programs.len()));
        for p in &doc.programs {
            let state = if p.running { "running" } else { "stopped" };
            out.push(format!("  {}.{} [{state}] config changed", p.app, p.program));
        }
    }
    for a in &doc.apps_added {
        out.push(format!("  app[{a}] registered (new)"));
    }
    for a in &doc.apps_removed {
        out.push(format!("  app[{a}] unregistered"));
    }
    for h in &doc.daemon_hints {
        out.push(format!("  {h}"));
    }
    for e in &doc.errors {
        out.push(format!("  {e}"));
    }
    out.push("run `xkeeper apply` to apply".into());
    out.join("\n")
}

/// Render an apply result as three plain groups: applied changes, restarts,
/// untouched — one line per program so what happened is obvious at a glance.
pub fn render_apply(r: &ApplyResult) -> String {
    let mut out = Vec::new();
    let applied: Vec<&ProgramAction> = r
        .programs
        .iter()
        .filter(|p| p.changed && p.action != "remove")
        .collect();
    let restarted: Vec<&ProgramAction> = r
        .programs
        .iter()
        .filter(|p| !p.changed && p.action.contains("restart"))
        .collect();
    let untouched: Vec<&ProgramAction> = r
        .programs
        .iter()
        .filter(|p| p.action == "none" || p.action == "keep-stopped")
        .collect();
    let removed: Vec<&ProgramAction> =
        r.programs.iter().filter(|p| p.action == "remove").collect();

    if r.programs.is_empty() && r.apps_added.is_empty() && r.apps_removed.is_empty() {
        out.push("no changes".into());
    }
    if !applied.is_empty() {
        out.push("changed:".into());
        for p in applied {
            out.push(format!("  {}.{} -> {}", p.app, p.program, p.action));
        }
    }
    if !restarted.is_empty() {
        out.push("restarted (--restart):".into());
        for p in restarted {
            out.push(format!("  {}.{}", p.app, p.program));
        }
    }
    if !removed.is_empty() {
        out.push("removed:".into());
        for p in removed {
            out.push(format!("  {}.{}", p.app, p.program));
        }
    }
    if !untouched.is_empty() {
        out.push("untouched:".into());
        for p in untouched {
            out.push(format!("  {}.{} ({})", p.app, p.program, p.action));
        }
    }
    for a in &r.apps_added {
        out.push(format!("app[{a}] added"));
    }
    for a in &r.apps_removed {
        out.push(format!("app[{a}] removed"));
    }
    for h in &r.daemon_hints {
        out.push(h.clone());
    }
    for e in &r.errors {
        out.push(e.clone());
    }
    out.push(r.summary.clone());
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write an app config, register it (link), build a supervisor with it
    /// loaded — the harness for detect/apply tests.
    fn unique_tag() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        SEQ.fetch_add(1, Ordering::Relaxed) ^ ((std::process::id() as u64) << 32)
    }

    fn setup(app_name: &str, toml_text: &str) -> (Arc<Supervisor>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("xk-apply-{}-{}", unique_tag(), app_name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("apps")).unwrap();
        let cfg_path = dir.join("daemon.toml");
        std::fs::write(&cfg_path, "").unwrap();
        let app_file = dir.join("xkeeper.toml");
        std::fs::write(&app_file, toml_text).unwrap();
        std::os::unix::fs::symlink(&app_file, dir.join("apps").join(format!("{app_name}.toml")))
            .unwrap();

        let (config, _) = DaemonConfig::load_or_default(&cfg_path).unwrap();
        let sup = Supervisor::new(config, &dir).unwrap();
        sup.set_config_path(&cfg_path);
        sup.bootstrap().unwrap();
        (sup, app_file)
    }

    /// Poll until `name` reaches `want`, ticking the program like the
    /// supervision loop would (tests have no run loop).
    fn wait_state(sup: &Supervisor, name: &str, want: ProgramState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let mut st = sup.state.lock().unwrap();
            if let Some(p) = st.programs.get_mut(name) {
                p.tick(Instant::now());
                if p.state() == want {
                    return;
                }
            }
            drop(st);
            std::thread::sleep(Duration::from_millis(20));
        }
        let st = sup.state.lock().unwrap();
        let got = st.programs.get(name).map(|p| p.state()).unwrap_or(ProgramState::Fatal);
        panic!("program {name} never reached {want:?}, got {got:?}");
    }

    /// configuration: reload only detects — a modified file shows up in the
    /// pending preview while the program's state and pid are untouched.
    #[test]
    fn reload_detects_without_touching_programs() {
        let (sup, app_file) = setup("demo", "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n");
        wait_state(&sup, "p", ProgramState::Running);
        let pid_before = sup.state.lock().unwrap().programs["p"].pid();

        std::fs::write(&app_file, "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 60'\nstartsecs = 0.0\n").unwrap();
        let out = sup.cmd_reload().unwrap();
        assert!(out.contains("p"), "preview lists the changed program: {out}");

        {
            let st = sup.state.lock().unwrap();
            assert_eq!(st.programs["p"].pid(), pid_before, "no restart on reload");
            let cmdline = format!(
                "{} {}",
                st.programs["p"].def.command,
                st.programs["p"].def.args.join(" ")
            );
            assert_eq!(cmdline, "sleep 30", "old definition still running");
            assert_eq!(st.pending.programs.len(), 1);
            assert!(st.pending.programs[0].running);
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: apply with nothing pending is a no-op.
    #[test]
    fn apply_with_no_changes_is_idempotent() {
        let (sup, _) = setup("demo", "[app]\nautostart = false\n[program.p]\ncommand = 'true'\nstartsecs = 0.0\n");
        let out = sup.cmd_apply(&ApplyScope::All, false).unwrap();
        assert!(out.contains("no changes"), "{out}");
        {
            let st = sup.state.lock().unwrap();
            assert!(st.pending.is_empty());
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: a changed running program stops, rebuilds, restarts.
    #[test]
    fn apply_updates_and_restarts_running_program() {
        let (sup, app_file) = setup("demo", "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n");
        wait_state(&sup, "p", ProgramState::Running);
        std::fs::write(&app_file, "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 60'\nstartsecs = 0.0\n").unwrap();

        let out = sup.cmd_apply(&ApplyScope::All, false).unwrap();
        assert!(out.contains("update-and-restart"), "{out}");
        wait_state(&sup, "p", ProgramState::Running);
        {
            let st = sup.state.lock().unwrap();
            let cmdline = format!(
                "{} {}",
                st.programs["p"].def.command,
                st.programs["p"].def.args.join(" ")
            );
            assert_eq!(cmdline, "sleep 60");
            assert!(st.pending.is_empty(), "pending cleared after full apply");
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: a changed but user-stopped program is redefined and
    /// stays stopped.
    #[test]
    fn apply_redefines_but_keeps_user_stopped() {
        let (sup, app_file) = setup("demo", "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n");
        wait_state(&sup, "p", ProgramState::Running);
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.get_mut("p").unwrap().stop();
        }
        wait_state(&sup, "p", ProgramState::Stopped);
        std::fs::write(&app_file, "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 60'\nstartsecs = 0.0\n").unwrap();

        let out = sup.cmd_apply(&ApplyScope::All, false).unwrap();
        assert!(out.contains("update-only"), "{out}");
        {
            let st = sup.state.lock().unwrap();
            let cmdline = format!(
                "{} {}",
                st.programs["p"].def.command,
                st.programs["p"].def.args.join(" ")
            );
            assert_eq!(cmdline, "sleep 60", "definition replaced");
            assert_eq!(st.programs["p"].state(), ProgramState::Stopped);
            assert!(st.programs["p"].pid().is_none());
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: --restart restarts an unchanged running program but
    /// never pulls up one the user stopped.
    #[test]
    fn apply_restart_flag_respects_user_stops() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = true\n[program.up]\ncommand = 'sleep 30'\nstartsecs = 0.0\n\
             [program.down]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        wait_state(&sup, "up", ProgramState::Running);
        wait_state(&sup, "down", ProgramState::Running);
        let pid_up_before = sup.state.lock().unwrap().programs["up"].pid();
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.get_mut("down").unwrap().stop();
        }
        wait_state(&sup, "down", ProgramState::Stopped);

        let out = sup.cmd_apply(&ApplyScope::All, true).unwrap();
        assert!(out.contains("restarted"), "{out}");
        wait_state(&sup, "up", ProgramState::Running);

        {
            let st = sup.state.lock().unwrap();
            assert_ne!(st.programs["up"].pid(), pid_up_before, "up was restarted");
            assert_eq!(st.programs["down"].state(), ProgramState::Stopped, "user-stopped stays down");
        }
        assert!(out.contains("keep-stopped"), "result distinguishes the skipped one: {out}");
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: --restart pulls up a program waiting in backoff (it
    /// crashed, the user never stopped it).
    #[test]
    fn apply_restart_flag_pulls_up_backoff_program() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n[program.crashy]\ncommand = 'false'\nstartsecs = 0.0\nautorestart = 'always'\nrestart_backoff = 100.0\n",
        );
        // Start manually, let it die, land in backoff.
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.get_mut("crashy").unwrap().spawn();
        }
        for _ in 0..100 {
            let st = sup.state.lock().unwrap();
            if st.programs["crashy"].state() == ProgramState::Backoff {
                break;
            }
            drop(st);
            std::thread::sleep(Duration::from_millis(20));
        }
        wait_state(&sup, "crashy", ProgramState::Backoff);

        let out = sup.cmd_apply(&ApplyScope::All, true).unwrap();
        // It restarts into starting/backoff again immediately.
        {
            let st = sup.state.lock().unwrap();
            let s = st.programs["crashy"].state();
            assert!(
                matches!(s, ProgramState::Starting | ProgramState::Backoff),
                "pulled up out of backoff, got {s:?}"
            );
        }
        assert!(out.contains("crashy"), "{out}");
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: a scoped apply leaves other apps' pending intact.
    #[test]
    fn scoped_apply_does_not_touch_other_apps() {
        let dir = std::env::temp_dir().join(format!("xk-scope-{}", unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("apps")).unwrap();
        let cfg_path = dir.join("daemon.toml");
        std::fs::write(&cfg_path, "").unwrap();
        let a_file = dir.join("a.toml");
        let b_file = dir.join("b.toml");
        std::fs::write(&a_file, "[app]\nautostart = true\n[program.a]\ncommand = 'sleep 30'\nstartsecs = 0.0\n").unwrap();
        std::fs::write(&b_file, "[app]\nautostart = true\n[program.b]\ncommand = 'sleep 30'\nstartsecs = 0.0\n").unwrap();
        std::os::unix::fs::symlink(&a_file, dir.join("apps").join("a.toml")).unwrap();
        std::os::unix::fs::symlink(&b_file, dir.join("apps").join("b.toml")).unwrap();

        let (config, _) = DaemonConfig::load_or_default(&cfg_path).unwrap();
        let sup = Supervisor::new(config, &dir).unwrap();
        sup.set_config_path(&cfg_path);
        sup.bootstrap().unwrap();
        wait_state(&sup, "a", ProgramState::Running);
        wait_state(&sup, "b", ProgramState::Running);
        let b_pid = sup.state.lock().unwrap().programs["b"].pid();

        // Change both on disk, apply only app a.
        std::fs::write(&a_file, "[app]\nautostart = true\n[program.a]\ncommand = 'sleep 60'\nstartsecs = 0.0\n").unwrap();
        std::fs::write(&b_file, "[app]\nautostart = true\n[program.b]\ncommand = 'sleep 90'\nstartsecs = 0.0\n").unwrap();
        let out = sup.cmd_apply(&ApplyScope::App("a".into()), false).unwrap();
        assert!(out.contains("update-and-restart"), "{out}");
        wait_state(&sup, "a", ProgramState::Running);

        {
            let st = sup.state.lock().unwrap();
            let cmd = |n: &str| {
                let p = &st.programs[n];
                format!("{} {}", p.def.command, p.def.args.join(" "))
            };
            assert_eq!(cmd("a"), "sleep 60");
            assert_eq!(cmd("b"), "sleep 30", "b untouched");
            assert_eq!(st.programs["b"].pid(), b_pid, "b never restarted");
            assert_eq!(st.pending.programs.len(), 1, "b still pending: {:?}", st.pending.programs);
        }
        let _ = sup.shutdown_all();
    }

    /// configuration: a broken app file is isolated — no pending for it, the
    /// rest still detected.
    #[test]
    fn detect_isolates_broken_app_file() {
        let (sup, app_file) = setup("demo", "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n");
        wait_state(&sup, "p", ProgramState::Running);
        std::fs::write(&app_file, "not [ valid toml").unwrap();
        let doc = sup.detect(true).unwrap();
        assert!(!doc.errors.is_empty(), "load failure reported: {doc:?}");
        assert!(doc.programs.is_empty(), "broken app contributes no pending: {doc:?}");
        {
            let st = sup.state.lock().unwrap();
            let cmdline = format!(
                "{} {}",
                st.programs["p"].def.command,
                st.programs["p"].def.args.join(" ")
            );
            assert_eq!(cmdline, "sleep 30", "old definition kept");
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: a newly registered autostart program starts on apply.
    #[test]
    fn apply_starts_newly_registered_app() {
        let (sup, app_file) = setup("demo", "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n");
        let dir = app_file.parent().unwrap().to_path_buf();
        // Add a second program to the same app file (a "new" definition).
        std::fs::write(&app_file, "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n[program.q]\ncommand = 'sleep 30'\nstartsecs = 0.0\n").unwrap();
        let out = sup.cmd_apply(&ApplyScope::All, false).unwrap();
        assert!(out.contains("q"), "{out}");
        wait_state(&sup, "q", ProgramState::Running);
        let _ = dir;
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: removing a program from the file stops and drops it.
    #[test]
    fn apply_removes_dropped_program() {
        let (sup, app_file) = setup("demo", "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n[program.q]\ncommand = 'sleep 30'\nstartsecs = 0.0\n");
        wait_state(&sup, "q", ProgramState::Running);
        std::fs::write(&app_file, "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n").unwrap();
        let out = sup.cmd_apply(&ApplyScope::All, false).unwrap();
        assert!(out.contains("removed"), "{out}");
        {
            let st = sup.state.lock().unwrap();
            assert!(!st.programs.contains_key("q"));
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: cross-app validation failure aborts with no pending.
    #[test]
    fn detect_refuses_cross_app_conflict() {
        let dir = std::env::temp_dir().join(format!("xk-conflict-{}", unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("apps")).unwrap();
        let cfg_path = dir.join("daemon.toml");
        std::fs::write(&cfg_path, "").unwrap();
        let a_file = dir.join("a.toml");
        let b_file = dir.join("b.toml");
        std::fs::write(&a_file, "[program.dup]\ncommand = 'true'\n").unwrap();
        std::fs::write(&b_file, "[program.dup]\ncommand = 'true'\n").unwrap();
        std::os::unix::fs::symlink(&a_file, dir.join("apps").join("a.toml")).unwrap();
        std::os::unix::fs::symlink(&b_file, dir.join("apps").join("b.toml")).unwrap();

        let (config, _) = DaemonConfig::load_or_default(&cfg_path).unwrap();
        let sup = Supervisor::new(config, &dir).unwrap();
        sup.set_config_path(&cfg_path);
        sup.bootstrap().unwrap(); // bootstrap reports, does not fail
        assert!(sup.detect(true).is_err(), "conflict refused");
        {
            let st = sup.state.lock().unwrap();
            assert!(st.pending.is_empty(), "no pending published on conflict");
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: periodic detect picks up a disk edit without any
    /// command; the mtime fast path skips re-parsing unchanged files.
    #[test]
    fn periodic_detect_publishes_pending() {
        let (sup, app_file) = setup("demo", "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n");
        wait_state(&sup, "p", ProgramState::Running);
        std::fs::write(&app_file, "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 60'\nstartsecs = 0.0\n").unwrap();
        sup.detect(false).unwrap();
        {
            let st = sup.state.lock().unwrap();
            assert_eq!(st.pending.programs.len(), 1);
            assert_eq!(st.pending.programs[0].program, "p");
        }
        // Fast path: a second pass with no further edits is stable.
        sup.detect(false).unwrap();
        {
            let st = sup.state.lock().unwrap();
            assert_eq!(st.pending.programs.len(), 1, "unchanged file re-detected from cache");
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: render distinguishes changed / restarted / untouched.
    #[test]
    fn render_apply_groups_by_action() {
        let r = ApplyResult {
            programs: vec![
                ProgramAction { app: "a".into(), program: "x".into(), changed: true, action: "update-and-restart".into(), result: "ok".into() },
                ProgramAction { app: "a".into(), program: "y".into(), changed: false, action: "restart".into(), result: "ok".into() },
                ProgramAction { app: "b".into(), program: "z".into(), changed: false, action: "keep-stopped".into(), result: "ok".into() },
            ],
            apps_added: vec!["c".into()],
            apps_removed: vec![],
            daemon_hints: vec![],
            errors: vec![],
            summary: "applied".into(),
        };
        let out = render_apply(&r);
        assert!(out.contains("changed:") && out.contains("a.x -> update-and-restart"));
        assert!(out.contains("restarted (--restart):") && out.contains("a.y"));
        assert!(out.contains("untouched:") && out.contains("b.z (keep-stopped)"));
        assert!(out.contains("app[c] added"));
    }

    #[test]
    fn apply_scope_matching() {
        assert!(ApplyScope::All.matches("a", "x"));
        assert!(ApplyScope::App("a".into()).matches("a", "x"));
        assert!(!ApplyScope::App("a".into()).matches("b", "x"));
        assert!(ApplyScope::Program("a".into(), "x".into()).matches("a", "x"));
        assert!(!ApplyScope::Program("a".into(), "x".into()).matches("a", "y"));
    }
}
