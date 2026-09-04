//! The supervisor: shared state, the command-driven main loop, ordered
//! startup, per-app reload and shutdown orchestration.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use log::{error, info, warn};

use crate::config::{self, CoreConfig, ResolvedApp, ResolvedProgram};
use crate::health;
use crate::program::{ManagedProgram, ProgramState};
use crate::registry;

pub type Reply = std::sync::mpsc::Sender<Result<String>>;

#[derive(Debug)]
pub enum Command {
    Start { name: String, reply: Option<Reply> },
    Stop { name: String, reply: Option<Reply> },
    Restart { name: String, reply: Option<Reply> },
    Reload { reply: Option<Reply> },
    Shutdown { reply: Option<Reply> },
    /// Sent by the health checker when a program needs a restart.
    HealthRestart { name: String },
}

#[derive(Clone)]
pub struct AppRecord {
    pub name: String,
    #[allow(dead_code)]
    pub path: PathBuf,
    pub autostart: bool,
    pub priority: i32,
}

/// Everything the API threads read and the main loop mutates.
pub struct SupervisorState {
    pub programs: HashMap<String, ManagedProgram>,
    pub apps: Vec<AppRecord>,
    pub core: CoreConfig,
    pub core_dir: PathBuf,
    pub shutdown: AtomicBool,
}

pub struct Supervisor {
    pub state: Arc<Mutex<SupervisorState>>,
    queue: Mutex<VecDeque<Command>>,
    cv: Condvar,
    pub health_tasks: health::TaskMap,
    core_path: Mutex<PathBuf>,
    interval: Duration,
}

impl Supervisor {
    pub fn new(core: CoreConfig, core_dir: &Path) -> Result<Arc<Self>> {
        let log_dir = config::resolve_path(&core.daemon.log_dir, core_dir);
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("failed to create log dir: {}", log_dir.display()))?;
        let interval = Duration::from_secs_f64(core.daemon.monitor_interval.max(0.05));
        Ok(Arc::new(Supervisor {
            state: Arc::new(Mutex::new(SupervisorState {
                programs: HashMap::new(),
                apps: Vec::new(),
                core,
                core_dir: core_dir.to_path_buf(),
                shutdown: AtomicBool::new(false),
            })),
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            health_tasks: Arc::new(Mutex::new(HashMap::new())),
            core_path: Mutex::new(core_dir.join("xkeeper.toml")),
            interval,
        }))
    }

    /// Remember where the core config lives so `reload` re-reads it.
    pub fn set_core_path(&self, p: &Path) {
        *self.core_path.lock().unwrap() = p.to_path_buf();
    }

    pub fn enqueue(&self, cmd: Command) {
        self.queue.lock().unwrap().push_back(cmd);
        self.cv.notify_all();
    }

    pub fn request_shutdown(&self) {
        self.state.lock().unwrap().shutdown.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    fn pop_command(&self) -> Option<Command> {
        self.queue.lock().unwrap().pop_front()
    }

    // -- bootstrap ----------------------------------------------------------

    /// Load every registered app and start eligible programs.
    pub fn bootstrap(&self) -> Result<()> {
        let (core, core_dir) = {
            let st = self.state.lock().unwrap();
            (st.core.clone(), st.core_dir.clone())
        };
        let listed = registry::list(&core, &core_dir)?;
        let mut records = Vec::new();
        let mut resolved: Vec<ResolvedApp> = Vec::new();
        for l in listed {
            match registry::reload_app(&l.name, &l.path, core.app_default.as_ref()) {
                Ok(a) => {
                    records.push(AppRecord {
                        name: a.name.clone(),
                        path: a.path.clone(),
                        autostart: a.autostart,
                        priority: a.priority,
                    });
                    resolved.push(a);
                }
                Err(e) => error!(
                    "app[{}] failed to load at startup (skipped): {e:#}",
                    l.name
                ),
            }
        }
        // Uniqueness of program names across apps is enforced at add-time, but
        // files may have drifted since; report instead of refusing to boot.
        if let Err(e) = config::validate_all(&resolved) {
            error!("cross-app validation problem at startup: {e}");
        }
        {
            let mut st = self.state.lock().unwrap();
            let log_dir = config::resolve_path(&st.core.daemon.log_dir, &st.core_dir);
            let ring_cap = st.core.daemon.log_buffer_lines;
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
                    let Some(p) = st.programs.get(name) else { continue };
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

    // -- reload -------------------------------------------------------------

    /// Rescan the registry, diff every app, apply changes. Per-app atomic: a
    /// broken file freezes only that app. Aborts entirely if the core config
    /// or cross-app validation fails.
    fn cmd_reload(&self) -> Result<String> {
        let core_path = self.core_path.lock().unwrap().clone();
        let (new_core, _) = CoreConfig::load_or_default(&core_path)
            .map_err(|e| anyhow::anyhow!("reload aborted, core config invalid: {e:#}"))?;
        let core_dir = self.state.lock().unwrap().core_dir.clone();
        let defaults = new_core.app_default.clone();

        let listed = registry::list(&new_core, &core_dir)?;
        let mut errors: Vec<String> = Vec::new();
        let mut summary: Vec<String> = Vec::new();
        let mut new_records: Vec<AppRecord> = Vec::new();
        let mut resolved: Vec<ResolvedApp> = Vec::new();

        for l in listed {
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
                    errors.push(format!("app[{}] kept old config: {e:#}", l.name));
                    let st = self.state.lock().unwrap();
                    if let Some(old) = st.apps.iter().find(|a| a.name == l.name) {
                        new_records.push(old.clone());
                    }
                }
            }
        }
        if let Err(e) = config::validate_all(&resolved) {
            bail!("reload aborted, running config unchanged: cross-app validation failed: {e}");
        }

        {
            let mut st = self.state.lock().unwrap();
            let log_dir = config::resolve_path(&st.core.daemon.log_dir, &st.core_dir);
            let ring_cap = st.core.daemon.log_buffer_lines;

            // Apps no longer registered: stop and drop all their programs.
            let keep: HashSet<String> = new_records.iter().map(|a| a.name.clone()).collect();
            let dropped: Vec<String> = st
                .programs
                .values()
                .filter(|p| !keep.contains(&p.def.app))
                .map(|p| p.def.name.clone())
                .collect();
            for name in dropped {
                if let Some(mut p) = st.programs.remove(&name) {
                    info!("program[{name}] stopped (app unregistered)");
                    p.stop();
                    summary.push(format!("program[{name}] removed (app unregistered)"));
                }
            }

            // Per-app program diff.
            for app in &resolved {
                let new_defs: &Vec<ResolvedProgram> = &app.programs;
                let new_names: HashSet<&str> =
                    new_defs.iter().map(|p| p.name.as_str()).collect();
                let old_names: Vec<String> = st
                    .programs
                    .values()
                    .filter(|p| p.def.app == app.name)
                    .map(|p| p.def.name.clone())
                    .collect();
                for old in old_names {
                    if !new_names.contains(old.as_str()) {
                        if let Some(mut p) = st.programs.remove(&old) {
                            info!("program[{old}] removed by reload");
                            p.stop();
                            summary.push(format!("program[{old}] removed"));
                        }
                    }
                }
                for def in new_defs {
                    match st.programs.get(&def.name) {
                        Some(p) if p.def.hash == def.hash => {}
                        Some(_) => {
                            let mut old = st.programs.remove(&def.name).unwrap();
                            let was_running = old.is_running();
                            old.stop();
                            let mut np =
                                ManagedProgram::new(def.clone(), log_dir.clone(), ring_cap);
                            if was_running {
                                np.spawn();
                            }
                            st.programs.insert(def.name.clone(), np);
                            summary.push(format!("program[{}] redefined", def.name));
                        }
                        None => {
                            let mut np =
                                ManagedProgram::new(def.clone(), log_dir.clone(), ring_cap);
                            if app.autostart {
                                np.spawn();
                            }
                            st.programs.insert(def.name.clone(), np);
                            summary.push(format!("program[{}] added", def.name));
                        }
                    }
                }
            }

            // Daemon fields that cannot hot-apply.
            let old_daemon = st.core.daemon.clone();
            if old_daemon.port != new_core.daemon.port
                || old_daemon.host != new_core.daemon.host
            {
                summary.push("daemon host/port changed: restart xkeeper to apply".into());
            }
            if old_daemon.auth_token != new_core.daemon.auth_token {
                summary.push("daemon auth_token changed: restart xkeeper to apply".into());
            }
            st.core = new_core;
            st.apps = new_records;
        }
        self.sync_health_tasks();
        self.start_eligible();
        summary.push("reload complete".into());
        for e in &errors {
            error!("reload: {e}");
        }
        let mut out = errors;
        out.extend(summary);
        Ok(out.join("\n"))
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
        info!("shutdown: stopping {} program(s) in reverse order", order.len());
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
        info!("supervisor loop started (interval {:.1}s)", self.interval.as_secs_f64());
        while !self.state.lock().unwrap().shutdown.load(Ordering::SeqCst) {
            while let Some(cmd) = self.pop_command() {
                self.execute(cmd);
            }
            if self.state.lock().unwrap().shutdown.load(Ordering::SeqCst) {
                break;
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
                let (guard, _) = self
                    .cv
                    .wait_timeout(guard, slice)
                    .unwrap();
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

fn answer(reply: Option<Reply>, r: Result<String>) {
    if let Some(tx) = reply {
        let _ = tx.send(r);
    }
}
