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
    /// App-level fan-out: start/stop/restart every program of one app,
    /// expanded server-side in the established ordering (actions spec).
    AppFanout {
        kind: FanoutKind,
        app: String,
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

/// Which per-program action an app fan-out runs (actions spec: 动作层级矩阵).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanoutKind {
    Start,
    Stop,
    Restart,
}

impl FanoutKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FanoutKind::Start => "start",
            FanoutKind::Stop => "stop",
            FanoutKind::Restart => "restart",
        }
    }
}

/// One per-program line of a fan-out result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanoutStep {
    pub program: String,
    /// Human-readable per-program outcome ("started", "already running", …).
    pub result: String,
}

/// Structured result of one app fan-out. The supervisor replies with this
/// serialized; the CLI renders it via [`render_fanout`], the API returns it
/// alongside the rendered text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanoutResult {
    pub app: String,
    pub action: String,
    pub programs: Vec<FanoutStep>,
}

/// Render a fan-out result for the CLI/shell: one line per program, in the
/// order the programs were processed (start order; stop is reversed).
pub fn render_fanout(r: &FanoutResult) -> String {
    let mut out = vec![format!(
        "app[{}] {}: {} program(s)",
        r.app,
        r.action,
        r.programs.len()
    )];
    for s in &r.programs {
        out.push(format!("  {}: {}", s.program, s.result));
    }
    out.join("\n")
}

/// Why a `signal` request failed; carries the HTTP status for the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalError {
    UnknownProgram,
    NotRunning,
    Delivery(String),
}

impl SignalError {
    pub fn http_status(&self) -> u16 {
        match self {
            SignalError::UnknownProgram => 404,
            SignalError::NotRunning => 409,
            SignalError::Delivery(_) => 400,
        }
    }
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignalError::UnknownProgram => write!(f, "unknown program"),
            SignalError::NotRunning => {
                write!(f, "program is not running (no child process to signal)")
            }
            SignalError::Delivery(msg) => write!(f, "{msg}"),
        }
    }
}

/// Deliver a whitelist signal to a program's current child process. The
/// state machine is untouched (actions spec: signal 成功投递 MUST NOT 改变
/// 程序状态机); a program without a live child is an error.
pub fn signal_program(
    sup: &Supervisor,
    name: &str,
    sig: crate::platform::Signal,
) -> std::result::Result<String, SignalError> {
    let (pid, state) = {
        let st = sup.state.lock().unwrap();
        let p = st.programs.get(name).ok_or(SignalError::UnknownProgram)?;
        (
            p.pid().ok_or(SignalError::NotRunning)?,
            p.state().to_string(),
        )
    };
    crate::platform::send_signal(pid, sig).map_err(SignalError::Delivery)?;
    Ok(format!(
        "signal {} delivered to program {name} (pid {pid}, state {state})",
        sig.name()
    ))
}

/// Snapshot the runtime values `${...}` substitution can resolve to
/// (one state lock, then the worker thread substitutes + spawns without
/// touching supervisor state).
pub fn run_context(st: &SupervisorState) -> crate::action::RunContext {
    let mut ctx = crate::action::RunContext {
        daemon_pid: std::process::id(),
        daemon_host: st.config.daemon.host.clone(),
        daemon_port: st.config.daemon.port,
        daemon_log_dir: config::resolve_path(&st.config.daemon.log_dir, &st.config_dir),
        daemon_app_dir: config::resolve_path(&st.config.daemon.app_dir, &st.config_dir),
        ..Default::default()
    };
    for a in &st.apps {
        ctx.apps.insert(a.name.clone(), a.path.clone());
    }
    for p in st.programs.values() {
        ctx.programs.insert(
            p.def.name.clone(),
            crate::action::ProgramVars {
                pid: p.pid(),
                state: p.state().to_string(),
                app: p.def.app.clone(),
                work_dir: p.def.work_dir.clone(),
                log_dir: p.log_dir.clone(),
            },
        );
    }
    ctx
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

/// The literal `all` in an apply scope is the whole-registry keyword
/// (`apply all` == bare `apply`), not an app name. `registry::add` rejects
/// registering an app named `all` so the two can never collide
/// (apply-workflow spec).
pub const ALL_KEYWORD: &str = "all";

/// Validate apply-scope request parts against the registered apps/programs
/// and resolve them into an [`ApplyScope`]. Single source shared by
/// `/v1/apply` (server.rs) and `/api/apply` (web.rs): the `all` keyword and
/// the unknown app/program 404s must never drift apart.
///
/// `Err` carries the user-facing message ("unknown app/program ...").
pub fn resolve_apply_scope(
    sup: &Supervisor,
    app: Option<&str>,
    program: Option<&str>,
) -> Result<ApplyScope, String> {
    if app == Some(ALL_KEYWORD) {
        // `all` is the reserved full-scope keyword and never addresses a
        // real app (registry::add rejects it), so `apply all <program>`
        // has no coherent meaning — reject instead of silently ignoring
        // the program part.
        if program.is_some() {
            return Err(format!(
                "{ALL_KEYWORD:?} is the full-scope keyword and takes no program argument"
            ));
        }
        return Ok(ApplyScope::All);
    }
    let st = sup.state.lock().unwrap();
    if let Some(a) = app {
        // Loaded apps validate directly; a freshly registered app may only
        // be pending (detected on disk, not applied yet) — it is still a
        // valid target (apply-workflow: a new app's programs start when
        // they enter the apply scope).
        let known = st.apps.iter().any(|r| r.name == a)
            || registry::list(&st.config, &st.config_dir)
                .map(|listed| listed.iter().any(|l| l.name == a && l.broken.is_none()))
                .unwrap_or(false);
        if !known {
            return Err(format!("unknown app {a:?}"));
        }
        if let Some(p) = program {
            let known = st.programs.get(p).map(|x| x.def.app == a).unwrap_or(false);
            if !known {
                return Err(format!("unknown program {p:?}"));
            }
        }
    }
    Ok(match (app, program) {
        (Some(a), Some(p)) => ApplyScope::Program(a.to_string(), p.to_string()),
        (Some(a), None) => ApplyScope::App(a.to_string()),
        _ => ApplyScope::All,
    })
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
    /// (program, action) mutual exclusion for custom actions. Only the
    /// control-plane worker threads touch it — never the supervision loop.
    pub actions: Arc<crate::action::ActionRuns>,
    /// Web-console intent handle (config-driven-webui): reload publishes a
    /// new intent here, the resident manager thread converges the runtime.
    pub webui: Arc<WebuiControl>,
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
            actions: crate::action::ActionRuns::new(),
            webui: WebuiControl::new(),
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
                error!(
                    "app[{}] failed to load at startup (skipped): {}",
                    l.name, reason
                );
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
            Command::AppFanout { kind, app, reply } => {
                answer(reply, self.cmd_app_fanout(kind, &app))
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

    // -- app-level fan-out --------------------------------------------------

    /// Programs of one app in fan-out order: dependency topological order
    /// among the app's programs with a name tie-break — the same effective
    /// ordering the daemon startup path uses (app priority is uniform within
    /// one app, names sort). Cross-app dependencies do not constrain the
    /// order. Stop is the reverse of this (actions spec: 排序复用
    /// process-management 规则).
    fn app_program_order(
        st: &SupervisorState,
        app: &str,
    ) -> std::result::Result<Vec<String>, String> {
        if !st.apps.iter().any(|a| a.name == app) {
            return Err(format!("unknown app {app:?}"));
        }
        let mut names: Vec<String> = st
            .programs
            .values()
            .filter(|p| p.def.app == app)
            .map(|p| p.def.name.clone())
            .collect();
        if names.is_empty() {
            return Err(format!("app {app:?} has no programs"));
        }
        names.sort();
        let members: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
        // Dependency edges restricted to the app (deduplicated so a repeated
        // entry in depends_on cannot skew the in-degree).
        let mut edges: HashSet<(&str, &str)> = HashSet::new();
        for n in &names {
            let deps = st
                .programs
                .get(n.as_str())
                .map(|p| p.def.depends_on.as_slice())
                .unwrap_or(&[]);
            for d in deps {
                if members.contains(d.as_str()) {
                    edges.insert((d.as_str(), n.as_str()));
                }
            }
        }
        let mut indegree: HashMap<&str, usize> = names.iter().map(|n| (n.as_str(), 0)).collect();
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for (dep, dependent) in &edges {
            *indegree.entry(dependent).or_insert(0) += 1;
            dependents.entry(dep).or_default().push(dependent);
        }
        // Kahn's algorithm; the sorted frontier is the name tie-break.
        let mut ready: std::collections::BTreeSet<&str> = names
            .iter()
            .map(|n| n.as_str())
            .filter(|n| indegree[n] == 0)
            .collect();
        let mut out = Vec::with_capacity(names.len());
        while let Some(n) = ready.pop_first() {
            for dep in dependents.remove(n).unwrap_or_default() {
                let e = indegree.get_mut(dep).unwrap();
                *e -= 1;
                if *e == 0 {
                    ready.insert(dep);
                }
            }
            out.push(n.to_string());
        }
        if out.len() < names.len() {
            // Unreachable: validate_all rejects dependency cycles at load.
            // Keep the fan-out total anyway instead of silently dropping.
            let done: HashSet<String> = out.iter().cloned().collect();
            for n in &names {
                if !done.contains(n) {
                    out.push(n.clone());
                }
            }
        }
        Ok(out)
    }

    /// Expand one app fan-out: sequential per-program runs of the existing
    /// start/stop/restart logic (spawn clears `user_stopped`; stop is the
    /// reverse of the start order). Errors are recorded per program — the
    /// fan-out never aborts halfway.
    fn cmd_app_fanout(&self, kind: FanoutKind, app: &str) -> Result<String> {
        let order = {
            let st = self.state.lock().unwrap();
            Self::app_program_order(&st, app).map_err(|e| anyhow::anyhow!("{e}"))?
        };
        let seq: Vec<String> = match kind {
            FanoutKind::Start | FanoutKind::Restart => order,
            FanoutKind::Stop => order.into_iter().rev().collect(),
        };
        let mut steps = Vec::with_capacity(seq.len());
        for name in &seq {
            let result = match kind {
                FanoutKind::Start => {
                    let busy = {
                        let st = self.state.lock().unwrap();
                        st.programs
                            .get(name)
                            .map(|p| {
                                matches!(p.state(), ProgramState::Running | ProgramState::Starting)
                            })
                            .unwrap_or(true)
                    };
                    if busy {
                        Ok(format!("program {name} is already running"))
                    } else {
                        self.cmd_start(name)
                    }
                }
                FanoutKind::Stop => self.cmd_stop(name),
                // Restart bounces each program in start order: dependencies
                // come up first, every program is replaced in place.
                FanoutKind::Restart => self.cmd_stop(name).and_then(|_| self.cmd_start(name)),
            };
            steps.push(FanoutStep {
                program: name.clone(),
                result: match result {
                    Ok(text) => text,
                    Err(e) => format!("error: {e:#}"),
                },
            });
        }
        let fr = FanoutResult {
            app: app.to_string(),
            action: kind.as_str().to_string(),
            programs: steps,
        };
        serde_json::to_string(&fr).context("failed to serialize fan-out result")
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
                && file_stamp(&l.path).map(|s| stamps_now.get(&l.path) == Some(&s)) == Some(true)
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
                    cached_defs
                        .lock()
                        .unwrap()
                        .insert(l.path.clone(), a.clone());
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
    /// The `[webui]` section never enters pending — it is hot-applied here:
    /// publish the new intent to the console manager and wait (bounded) for
    /// convergence. A bind failure degrades to a note (Ok), while an invalid
    /// `[webui]` section fails the whole reload earlier via `detect`
    /// (load-time validation), leaving the running console untouched.
    fn cmd_reload(&self) -> Result<String> {
        let doc = self.detect(true)?;
        let mut out = render_pending(&doc);
        let config_path = self.config_path.lock().unwrap().clone();
        let (new_config, _) = DaemonConfig::load_or_default(&config_path)
            .map_err(|e| anyhow::anyhow!("reload aborted, daemon config invalid: {e:#}"))?;
        let generation = self.webui.set_desired(WebuiIntent::from_config(&new_config));
        // Degraded convergence is logged by the webui manager; the note is
        // appended to the reply below for the CLI caller.
        let (_, note) = self.webui.wait_applied(generation, WEBUI_CONVERGE_TIMEOUT);
        out.push_str(&format!("\nwebui: {note}"));
        Ok(out)
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
            result.summary =
                format!("applied: {changed} program change(s), {restarted} restart/start(s)");
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

// -- Web console lifecycle (config-driven-webui) ----------------------------

/// How long `reload` waits for the webui manager to converge on a new intent.
const WEBUI_CONVERGE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the manager waits for a running console to stop before detaching
/// it (the detached thread ends on its own; its port may linger briefly).
const WEBUI_STOP_WAIT: Duration = Duration::from_secs(3);
/// Manager poll slice: reaps dead instances while idle between intents.
const WEBUI_MANAGER_POLL: Duration = Duration::from_millis(500);
/// How long `start_webui` waits for the bind report before assuming success.
const WEBUI_BIND_WAIT: Duration = Duration::from_secs(5);

/// The desired web-console state, derived from the `[webui]` config section:
/// present = enabled at `listen`, absent = disabled (design D1: presence IS
/// the switch, there is no boolean).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebuiIntent {
    pub enabled: bool,
    pub listen: String,
}

impl WebuiIntent {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            listen: String::new(),
        }
    }

    pub fn from_config(config: &DaemonConfig) -> Self {
        match config.webui_listen() {
            Some(listen) => Self {
                enabled: true,
                listen: listen.to_string(),
            },
            None => Self::disabled(),
        }
    }

    /// Does an instance bound to `listen` already satisfy this intent?
    fn serves(&self, listen: &str) -> bool {
        self.enabled && self.listen == listen
    }
}

/// Shared intent mailbox between the reload path (writer) and the resident
/// manager thread (reader/converger). Plain Mutex + Condvar on purpose: the
/// manager must be usable before any tokio runtime exists and must never be
/// tied to one (design D3). A generation counter makes the handshake
/// race-free: `set_desired` bumps it and `wait_applied` waits until the
/// manager reports exactly that generation.
#[derive(Default)]
struct WebuiShared {
    /// `None` = disabled intent at generation 0.
    desired: Option<(WebuiIntent, u64)>,
    /// Latest `(generation, ok, note)` reported by the manager.
    applied: Option<(u64, bool, String)>,
}

/// Handle for publishing the console intent and waiting for convergence.
/// Lives on `Supervisor` so the API threads (reload) and main.rs (startup)
/// share one instance with the manager thread.
pub struct WebuiControl {
    shared: Mutex<WebuiShared>,
    cv: Condvar,
}

impl WebuiControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: Mutex::new(WebuiShared::default()),
            cv: Condvar::new(),
        })
    }

    /// Publish a new intent and return its generation (reload/startup path).
    pub fn set_desired(&self, intent: WebuiIntent) -> u64 {
        let mut sh = self.shared.lock().unwrap();
        let generation = sh.desired.as_ref().map(|(_, g)| *g).unwrap_or(0) + 1;
        sh.desired = Some((intent, generation));
        sh.applied = None;
        self.cv.notify_all();
        generation
    }

    /// Current intent + generation (manager thread).
    fn desired(&self) -> (WebuiIntent, u64) {
        let sh = self.shared.lock().unwrap();
        sh.desired
            .clone()
            .unwrap_or((WebuiIntent::disabled(), 0))
    }

    /// Bounded wait until the manager reports `generation` applied. On timeout the
    /// console may still converge later; the caller surfaces the note.
    pub fn wait_applied(&self, generation: u64, timeout: Duration) -> (bool, String) {
        let mut sh = self.shared.lock().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some((g, ok, note)) = &sh.applied {
                if *g >= generation {
                    return (*ok, note.clone());
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return (
                    false,
                    "webui change did not converge in time (the manager retries on the next reload)"
                        .to_string(),
                );
            }
            let (guard, _) = self
                .cv
                .wait_timeout(sh, deadline.saturating_duration_since(now))
                .unwrap();
            sh = guard;
        }
    }

    /// Manager thread: record the outcome for generation `generation`.
    fn mark_applied(&self, generation: u64, ok: bool, note: String) {
        let mut sh = self.shared.lock().unwrap();
        sh.applied = Some((generation, ok, note));
        self.cv.notify_all();
    }

    /// Manager thread: bounded wait for a newer intent. Wakes early on
    /// `set_desired` (it notifies the condvar); daemon shutdown is noticed
    /// by the loop's own poll at the latest.
    fn wait_for_change(&self, seen: u64, timeout: Duration) {
        let guard = self.shared.lock().unwrap();
        let current = guard.desired.as_ref().map(|(_, g)| *g).unwrap_or(0);
        if current > seen {
            return;
        }
        let _ = self.cv.wait_timeout(guard, timeout).unwrap();
    }
}

/// One live console instance owned by the manager thread.
struct RunningWebui {
    listen: String,
    stop: Arc<crate::web::WebuiLifecycle>,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
}

impl RunningWebui {
    /// If the serve thread already exited (unexpected crash), take and join
    /// it. Returns true when an instance was reaped.
    fn reap_if_finished(&mut self) -> bool {
        let done = self
            .handle
            .as_ref()
            .map(|h| h.is_finished())
            .unwrap_or(true);
        if done {
            if let Some(h) = self.handle.take() {
                if let Err(e) = h.join() {
                    error!("webui console thread panicked: {e:?}");
                }
            }
            return true;
        }
        false
    }

    /// Ask the instance to stop and wait (bounded) for its thread. Returns
    /// false when it did not stop in time — the thread is detached and its
    /// port may stay bound a little longer; a same-port restart then fails
    /// the bind, which the manager reports non-fatally (design D5).
    fn stop_and_wait(&mut self) -> bool {
        self.stop.stop();
        let deadline = Instant::now() + WEBUI_STOP_WAIT;
        while Instant::now() < deadline {
            if self.handle.as_ref().map(|h| h.is_finished()).unwrap_or(true) {
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        warn!(
            "webui console on {} did not stop within {:?}; detaching it",
            self.listen, WEBUI_STOP_WAIT
        );
        self.handle = None;
        false
    }
}

/// Spawn one console instance and report `(instance, ok, note)`. Bind
/// failures are non-fatal (design D5): the daemon keeps running without the
/// console and the note explains how to retry.
fn start_webui(sup: &Arc<Supervisor>, listen: &str) -> (Option<RunningWebui>, bool, String) {
    let stop = crate::web::WebuiLifecycle::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let sup2 = Arc::clone(sup);
    let stop2 = Arc::clone(&stop);
    let addr = listen.to_string();
    let handle = std::thread::Builder::new()
        .name("webui".into())
        .spawn(move || crate::web::serve(sup2, &addr, stop2, Some(tx)))
        .expect("spawn webui thread");
    match rx.recv_timeout(WEBUI_BIND_WAIT) {
        Ok(Ok(bound)) => (
            Some(RunningWebui {
                listen: bound.clone(),
                stop,
                handle: Some(handle),
            }),
            true,
            format!("webui console serving on http://{bound}"),
        ),
        Ok(Err(e)) => {
            let _ = handle.join();
            (
                None,
                false,
                format!(
                    "webui console unavailable: {e:#} (daemon continues without it; fix the config and reload to retry)"
                ),
            )
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => (
            None,
            false,
            "webui console exited during startup".to_string(),
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // Slow bind (rare): assume it is coming; the manager reaps the
            // instance if the thread dies.
            (
                Some(RunningWebui {
                    listen: listen.to_string(),
                    stop,
                    handle: Some(handle),
                }),
                true,
                format!("webui console binding on {listen}"),
            )
        }
    }
}

/// Resident manager: converges the running console to the latest intent.
/// Runs on its own thread for the daemon's lifetime; on daemon shutdown it
/// stops the console first so the port is released promptly.
pub fn webui_manager_loop(sup: Arc<Supervisor>) {
    let mut running: Option<RunningWebui> = None;
    let mut converged_gen: u64 = 0;
    loop {
        if sup.state.lock().unwrap().shutdown.load(Ordering::SeqCst) {
            if let Some(r) = running.as_mut() {
                r.stop_and_wait();
            }
            return;
        }
        // Reap crashed instances. No auto-restart (design D5): the console
        // comes back on the next intent change (i.e. an explicit reload).
        let reaped = running
            .as_mut()
            .map(|r| (r.reap_if_finished(), r.listen.clone()))
            .filter(|(done, _)| *done)
            .map(|(_, listen)| listen);
        if let Some(listen) = reaped {
            error!(
                "webui console on {listen} exited unexpectedly; daemon continues without it (reload to retry)"
            );
            running = None;
        }
        let (desired, generation) = sup.webui.desired();
        if generation > converged_gen {
            let matches = running
                .as_ref()
                .map(|r| desired.serves(&r.listen))
                .unwrap_or(false);
            if matches {
                sup.webui.mark_applied(
                    generation,
                    true,
                    format!("webui console serving on http://{}", desired.listen),
                );
            } else {
                if let Some(old) = running.as_mut() {
                    old.stop_and_wait();
                }
                running = None;
                if desired.enabled {
                    info!("webui console starting on {}", desired.listen);
                    let (inst, ok, note) = start_webui(&sup, &desired.listen);
                    running = inst;
                    // Log degraded starts (bind failure, early exit) right
                    // here so both `run` startup and `reload` record it —
                    // the note alone is only visible to reload's reply.
                    if !ok {
                        error!("{note}");
                    }
                    sup.webui.mark_applied(generation, ok, note);
                } else {
                    info!("webui console disabled");
                    sup.webui
                        .mark_applied(generation, true, "webui console disabled".to_string());
                }
            }
            converged_gen = generation;
        }
        sup.webui.wait_for_change(generation, WEBUI_MANAGER_POLL);
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
            out.push(format!(
                "  {}.{} [{state}] config changed",
                p.app, p.program
            ));
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
    let removed: Vec<&ProgramAction> = r.programs.iter().filter(|p| p.action == "remove").collect();

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

    /// Register `target` as `app_dir/<name>.toml` for tests, mirroring
    /// `registry::make_link` (symlink; hard-link fallback where symlinks
    /// need privileges, e.g. unprivileged Windows).
    fn link_for_test(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_file(target, link).is_err() {
                std::fs::hard_link(target, link).unwrap();
            }
        }
    }

    fn setup(app_name: &str, toml_text: &str) -> (Arc<Supervisor>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("xk-apply-{}-{}", unique_tag(), app_name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("apps")).unwrap();
        let cfg_path = dir.join("daemon.toml");
        std::fs::write(&cfg_path, "").unwrap();
        let app_file = dir.join("xkeeper.toml");
        std::fs::write(&app_file, toml_text).unwrap();
        link_for_test(
            &app_file,
            &dir.join("apps").join(format!("{app_name}.toml")),
        );

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
        let got = st
            .programs
            .get(name)
            .map(|p| p.state())
            .unwrap_or(ProgramState::Fatal);
        panic!("program {name} never reached {want:?}, got {got:?}");
    }

    /// configuration: reload only detects — a modified file shows up in the
    /// pending preview while the program's state and pid are untouched.
    #[test]
    fn reload_detects_without_touching_programs() {
        let (sup, app_file) = setup(
            "demo",
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        let mgr = spawn_manager(&sup);
        wait_state(&sup, "p", ProgramState::Running);
        let pid_before = sup.state.lock().unwrap().programs["p"].pid();

        std::fs::write(
            &app_file,
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 60'\nstartsecs = 0.0\n",
        )
        .unwrap();
        let out = sup.cmd_reload().unwrap();
        assert!(
            out.contains("p"),
            "preview lists the changed program: {out}"
        );
        assert!(
            out.contains("webui: webui console disabled"),
            "reload always reports the webui outcome: {out}"
        );

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
        end_manager(&sup, mgr);
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: apply with nothing pending is a no-op.
    #[test]
    fn apply_with_no_changes_is_idempotent() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'true'\nstartsecs = 0.0\n",
        );
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
        let (sup, app_file) = setup(
            "demo",
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        wait_state(&sup, "p", ProgramState::Running);
        std::fs::write(
            &app_file,
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 60'\nstartsecs = 0.0\n",
        )
        .unwrap();

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
        let (sup, app_file) = setup(
            "demo",
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        wait_state(&sup, "p", ProgramState::Running);
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.get_mut("p").unwrap().stop();
        }
        wait_state(&sup, "p", ProgramState::Stopped);
        std::fs::write(
            &app_file,
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 60'\nstartsecs = 0.0\n",
        )
        .unwrap();

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
            assert_eq!(
                st.programs["down"].state(),
                ProgramState::Stopped,
                "user-stopped stays down"
            );
        }
        assert!(
            out.contains("keep-stopped"),
            "result distinguishes the skipped one: {out}"
        );
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
        std::fs::write(
            &a_file,
            "[app]\nautostart = true\n[program.a]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        )
        .unwrap();
        std::fs::write(
            &b_file,
            "[app]\nautostart = true\n[program.b]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        )
        .unwrap();
        link_for_test(&a_file, &dir.join("apps").join("a.toml"));
        link_for_test(&b_file, &dir.join("apps").join("b.toml"));

        let (config, _) = DaemonConfig::load_or_default(&cfg_path).unwrap();
        let sup = Supervisor::new(config, &dir).unwrap();
        sup.set_config_path(&cfg_path);
        sup.bootstrap().unwrap();
        wait_state(&sup, "a", ProgramState::Running);
        wait_state(&sup, "b", ProgramState::Running);
        let b_pid = sup.state.lock().unwrap().programs["b"].pid();

        // Change both on disk, apply only app a.
        std::fs::write(
            &a_file,
            "[app]\nautostart = true\n[program.a]\ncommand = 'sleep 60'\nstartsecs = 0.0\n",
        )
        .unwrap();
        std::fs::write(
            &b_file,
            "[app]\nautostart = true\n[program.b]\ncommand = 'sleep 90'\nstartsecs = 0.0\n",
        )
        .unwrap();
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
            assert_eq!(
                st.pending.programs.len(),
                1,
                "b still pending: {:?}",
                st.pending.programs
            );
        }
        let _ = sup.shutdown_all();
    }

    /// configuration: a broken app file is isolated — no pending for it, the
    /// rest still detected.
    #[test]
    fn detect_isolates_broken_app_file() {
        let (sup, app_file) = setup(
            "demo",
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        wait_state(&sup, "p", ProgramState::Running);
        std::fs::write(&app_file, "not [ valid toml").unwrap();
        let doc = sup.detect(true).unwrap();
        assert!(!doc.errors.is_empty(), "load failure reported: {doc:?}");
        assert!(
            doc.programs.is_empty(),
            "broken app contributes no pending: {doc:?}"
        );
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
        let (sup, app_file) = setup(
            "demo",
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
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
        let (sup, app_file) = setup(
            "demo",
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n[program.q]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        wait_state(&sup, "q", ProgramState::Running);
        std::fs::write(
            &app_file,
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        )
        .unwrap();
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
        link_for_test(&a_file, &dir.join("apps").join("a.toml"));
        link_for_test(&b_file, &dir.join("apps").join("b.toml"));

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
        let (sup, app_file) = setup(
            "demo",
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        wait_state(&sup, "p", ProgramState::Running);
        std::fs::write(
            &app_file,
            "[app]\nautostart = true\n[program.p]\ncommand = 'sleep 60'\nstartsecs = 0.0\n",
        )
        .unwrap();
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
            assert_eq!(
                st.pending.programs.len(),
                1,
                "unchanged file re-detected from cache"
            );
        }
        let _ = sup.shutdown_all();
    }

    /// apply-workflow: render distinguishes changed / restarted / untouched.
    #[test]
    fn render_apply_groups_by_action() {
        let r = ApplyResult {
            programs: vec![
                ProgramAction {
                    app: "a".into(),
                    program: "x".into(),
                    changed: true,
                    action: "update-and-restart".into(),
                    result: "ok".into(),
                },
                ProgramAction {
                    app: "a".into(),
                    program: "y".into(),
                    changed: false,
                    action: "restart".into(),
                    result: "ok".into(),
                },
                ProgramAction {
                    app: "b".into(),
                    program: "z".into(),
                    changed: false,
                    action: "keep-stopped".into(),
                    result: "ok".into(),
                },
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

    /// apply-workflow: the literal `all` resolves to the whole-registry
    /// scope in every request shape; real app/program names still scope.
    #[test]
    fn apply_scope_all_keyword_and_validation() {
        let (sup, _app_file) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        // `all` is the reserved keyword; a program part has no meaning.
        assert!(
            resolve_apply_scope(&sup, Some("all"), Some("p"))
                .err()
                .is_some_and(|e| e.contains("full-scope")),
            "apply all <program> must be rejected"
        );
        assert!(matches!(
            resolve_apply_scope(&sup, None, None).unwrap(),
            ApplyScope::All
        ));
        // Real names scope down.
        assert!(
            matches!(
                resolve_apply_scope(&sup, Some("demo"), None).unwrap(),
                ApplyScope::App(a) if a == "demo"
            ),
            "app scope"
        );
        assert!(
            matches!(
                resolve_apply_scope(&sup, Some("demo"), Some("p")).unwrap(),
                ApplyScope::Program(a, p) if a == "demo" && p == "p"
            ),
            "program scope"
        );
        // Validation still rejects unknown names.
        assert!(resolve_apply_scope(&sup, Some("ghost"), None).is_err());
        assert!(resolve_apply_scope(&sup, Some("demo"), Some("ghost")).is_err());
        let _ = sup.shutdown_all();
    }

    /// A freshly registered app that is only pending (on disk, not applied
    /// into the daemon state yet) is still a valid app-scope target — the
    /// add → `apply <app>` flow (app-registry: --apply 在线一步生效).
    #[test]
    fn apply_scope_accepts_pending_new_app() {
        let (sup, app_file) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        // Register a second app on disk without any reload/apply.
        let dir = app_file.parent().unwrap();
        let other = dir.join("other.toml");
        std::fs::write(
            &other,
            "[app]\nautostart = false\n[program.q]\ncommand = 'sleep 30'\n",
        )
        .unwrap();
        link_for_test(&other, &dir.join("apps").join("other.toml"));
        {
            let st = sup.state.lock().unwrap();
            assert!(
                !st.apps.iter().any(|a| a.name == "other"),
                "other is not loaded into state yet"
            );
        }
        assert!(
            matches!(
                resolve_apply_scope(&sup, Some("other"), None).unwrap(),
                ApplyScope::App(a) if a == "other"
            ),
            "pending app is a valid apply target"
        );
        // Broken pending apps are not valid targets either.
        std::fs::write(&other, "not [ valid toml").unwrap();
        assert!(resolve_apply_scope(&sup, Some("other"), None).is_err());
        let _ = sup.shutdown_all();
    }

    // -- app fan-out --------------------------------------------------------

    fn fanout(sup: &Supervisor, kind: FanoutKind, app: &str) -> FanoutResult {
        let raw = sup
            .cmd_app_fanout(kind, app)
            .unwrap_or_else(|e| panic!("fanout {kind:?} {app} failed: {e:#}"));
        serde_json::from_str(&raw).expect("fan-out result is JSON")
    }

    fn names_of(fr: &FanoutResult) -> Vec<&str> {
        fr.programs.iter().map(|s| s.program.as_str()).collect()
    }

    /// actions: the fan-out start expands in dependency order, starts every
    /// program with the existing per-program logic (clearing user_stopped),
    /// and reports one line per program.
    #[test]
    fn app_fanout_start_orders_and_covers_user_stopped() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n\
             [program.a]\ncommand = 'sleep 30'\nstartsecs = 0.0\n\
             [program.b]\ncommand = 'sleep 30'\nstartsecs = 0.0\ndepends_on = ['a']\n\
             [program.c]\ncommand = 'sleep 30'\nstartsecs = 0.0\ndepends_on = ['b']\n",
        );
        let fr = fanout(&sup, FanoutKind::Start, "demo");
        assert_eq!(names_of(&fr), ["a", "b", "c"], "dependency order");
        for n in ["a", "b", "c"] {
            wait_state(&sup, n, ProgramState::Running);
        }
        assert!(fr.programs.iter().all(|s| !s.result.contains("error")));

        // The user stopped b; a fan-out start pulls it up again and clears
        // the marker (spawn clears user_stopped).
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.get_mut("b").unwrap().stop();
        }
        wait_state(&sup, "b", ProgramState::Stopped);
        assert!(sup.state.lock().unwrap().programs["b"].user_stopped());

        let fr = fanout(&sup, FanoutKind::Start, "demo");
        assert_eq!(names_of(&fr), ["a", "b", "c"]);
        wait_state(&sup, "b", ProgramState::Running);
        {
            let st = sup.state.lock().unwrap();
            assert!(
                !st.programs["b"].user_stopped(),
                "fan-out start clears user_stopped"
            );
        }
        // a/c were already running: recorded, not an error.
        let b_res = &fr.programs[0].result;
        assert!(b_res.contains("already running"), "a: {b_res}");
        assert!(
            !fr.programs[1].result.contains("error"),
            "b: {}",
            fr.programs[1].result
        );
        let _ = sup.shutdown_all();
    }

    /// actions: fan-out stop runs in the reverse of the start order;
    /// restart bounces every program (stop+start) in start order.
    #[test]
    fn app_fanout_stop_is_reverse_and_restart_bounces() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n\
             [program.a]\ncommand = 'sleep 30'\nstartsecs = 0.0\n\
             [program.b]\ncommand = 'sleep 30'\nstartsecs = 0.0\ndepends_on = ['a']\n\
             [program.c]\ncommand = 'sleep 30'\nstartsecs = 0.0\ndepends_on = ['b']\n",
        );
        fanout(&sup, FanoutKind::Start, "demo");
        for n in ["a", "b", "c"] {
            wait_state(&sup, n, ProgramState::Running);
        }
        let pids: Vec<Option<u32>> = {
            let st = sup.state.lock().unwrap();
            ["a", "b", "c"]
                .iter()
                .map(|n| st.programs[*n].pid())
                .collect()
        };

        let fr = fanout(&sup, FanoutKind::Stop, "demo");
        assert_eq!(names_of(&fr), ["c", "b", "a"], "stop is the reverse order");
        for n in ["a", "b", "c"] {
            wait_state(&sup, n, ProgramState::Stopped);
        }

        let fr = fanout(&sup, FanoutKind::Restart, "demo");
        assert_eq!(
            names_of(&fr),
            ["a", "b", "c"],
            "restart walks the start order"
        );
        for (i, n) in ["a", "b", "c"].iter().enumerate() {
            wait_state(&sup, n, ProgramState::Running);
            let st = sup.state.lock().unwrap();
            assert_ne!(
                st.programs[*n].pid(),
                pids[i],
                "{n} was replaced by the restart"
            );
        }
        let _ = sup.shutdown_all();
    }

    /// actions: unknown apps and (defensively) apps without programs are
    /// error paths, never silent no-ops.
    #[test]
    fn app_fanout_unknown_and_empty_apps() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\n",
        );
        {
            let mut st = sup.state.lock().unwrap();
            st.apps.push(AppRecord {
                name: "hollow".into(),
                path: PathBuf::new(),
                autostart: false,
                priority: 0,
            });
        }
        let e = sup
            .cmd_app_fanout(FanoutKind::Start, "ghost")
            .err()
            .expect("unknown app must fail")
            .to_string();
        assert!(e.contains("unknown app"), "{e}");
        let e = sup
            .cmd_app_fanout(FanoutKind::Stop, "hollow")
            .err()
            .expect("app without programs must fail")
            .to_string();
        assert!(e.contains("no programs"), "{e}");
        let _ = sup.shutdown_all();
    }

    /// actions: signal reaches the child without touching the state machine;
    /// a stopped program has nothing to signal (SignalError branches).
    #[test]
    #[cfg(unix)]
    fn signal_delivery_and_error_branches() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'sleep 30'\nstartsecs = 0.0\nautorestart = 'never'\n",
        );
        sup.cmd_start("p").unwrap();
        wait_state(&sup, "p", ProgramState::Running);
        let pid = sup.state.lock().unwrap().programs["p"].pid();

        let out = signal_program(&sup, "p", crate::platform::Signal::Usr1).unwrap();
        assert!(out.contains("delivered"), "{out}");
        // Immediately after delivery the state machine is untouched — the
        // program dies on its own and the next tick observes the exit.
        assert_eq!(
            sup.state.lock().unwrap().programs["p"].state(),
            ProgramState::Running,
            "signal must not change the state machine"
        );
        wait_state(&sup, "p", ProgramState::Exited);

        // Not running: nothing to deliver.
        let e = signal_program(&sup, "p", crate::platform::Signal::Term)
            .err()
            .unwrap();
        assert_eq!(e, SignalError::NotRunning);
        assert_eq!(e.http_status(), 409);
        // Unknown program.
        let e = signal_program(&sup, "ghost", crate::platform::Signal::Term)
            .err()
            .unwrap();
        assert_eq!(e, SignalError::UnknownProgram);
        assert_eq!(e.http_status(), 404);
        let _ = pid;
        let _ = sup.shutdown_all();
    }

    // -- web console lifecycle (config-driven-webui) ------------------------

    /// Spawn the resident manager thread like run_daemon does. Tests end it
    /// with [`end_manager`].
    fn spawn_manager(sup: &Arc<Supervisor>) -> std::thread::JoinHandle<()> {
        let sup2 = Arc::clone(sup);
        std::thread::Builder::new()
            .name("webui-manager-test".into())
            .spawn(move || webui_manager_loop(sup2))
            .unwrap()
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Flip the daemon shutdown flag so the manager stops any console and
    /// returns, then join it.
    fn end_manager(sup: &Supervisor, mgr: std::thread::JoinHandle<()>) {
        sup.state
            .lock()
            .unwrap()
            .shutdown
            .store(true, Ordering::SeqCst);
        mgr.join().unwrap();
    }

    fn wait_applied_ok(sup: &Supervisor, generation: u64) -> String {
        let (ok, note) = sup.webui.wait_applied(generation, Duration::from_secs(10));
        assert!(ok, "manager converged: {note}");
        note
    }

    /// manager: an enabled intent starts the console, a disabled one stops
    /// it and closes the port (reload 动态开关的服务端机制).
    #[test]
    fn webui_manager_follows_intent_on_and_off() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'true'\nstartsecs = 0.0\n",
        );
        let listen = format!("127.0.0.1:{}", free_port());
        let mgr = spawn_manager(&sup);

        let generation = sup.webui.set_desired(WebuiIntent {
            enabled: true,
            listen: listen.clone(),
        });
        let note = wait_applied_ok(&sup, generation);
        assert!(
            note.contains("serving on http://"),
            "note names the serving URL: {note}"
        );
        assert!(
            std::net::TcpStream::connect(&listen).is_ok(),
            "console reachable once enabled"
        );

        let generation = sup.webui.set_desired(WebuiIntent::disabled());
        let note = wait_applied_ok(&sup, generation);
        assert_eq!(note, "webui console disabled");
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            std::net::TcpStream::connect(&listen).is_err(),
            "console port closed after disable"
        );
        end_manager(&sup, mgr);
        let _ = sup.shutdown_all();
    }

    /// manager: a listen change stops the old instance and binds the new
    /// address (reload 换址重绑).
    #[test]
    fn webui_manager_rebinds_on_listen_change() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'true'\nstartsecs = 0.0\n",
        );
        let old = format!("127.0.0.1:{}", free_port());
        let new = format!("127.0.0.1:{}", free_port());
        let mgr = spawn_manager(&sup);

        let generation = sup.webui.set_desired(WebuiIntent {
            enabled: true,
            listen: old.clone(),
        });
        wait_applied_ok(&sup, generation);
        assert!(std::net::TcpStream::connect(&old).is_ok());

        let generation = sup.webui.set_desired(WebuiIntent {
            enabled: true,
            listen: new.clone(),
        });
        wait_applied_ok(&sup, generation);
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            std::net::TcpStream::connect(&old).is_err(),
            "old port released after rebind"
        );
        assert!(
            std::net::TcpStream::connect(&new).is_ok(),
            "new port serving after rebind"
        );
        end_manager(&sup, mgr);
        let _ = sup.shutdown_all();
    }

    /// manager: binding a taken port degrades non-fatally (D5) — the daemon
    /// stays alive, the failure is reported, and the next intent change
    /// (once the port is free again) recovers.
    #[test]
    fn webui_bind_failure_degrades_then_recovers() {
        let (sup, _) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'true'\nstartsecs = 0.0\n",
        );
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = format!("{}", blocker.local_addr().unwrap());
        let mgr = spawn_manager(&sup);

        let generation = sup.webui.set_desired(WebuiIntent {
            enabled: true,
            listen: taken.clone(),
        });
        let (ok, note) = sup.webui.wait_applied(generation, Duration::from_secs(10));
        assert!(!ok, "occupied port must not report success: {note}");
        assert!(
            note.contains("unavailable") && note.contains("reload to retry"),
            "note explains the degradation and the retry path: {note}"
        );
        // The daemon itself is unaffected.
        assert!(!sup.state.lock().unwrap().shutdown.load(Ordering::SeqCst));

        // Free the port and re-publish the same intent: the manager retries.
        drop(blocker);
        std::thread::sleep(Duration::from_millis(100));
        let generation = sup.webui.set_desired(WebuiIntent {
            enabled: true,
            listen: taken.clone(),
        });
        wait_applied_ok(&sup, generation);
        assert!(
            std::net::TcpStream::connect(&taken).is_ok(),
            "console serving after the port freed up"
        );
        end_manager(&sup, mgr);
        let _ = sup.shutdown_all();
    }

    /// The intent mapping is the single source for both startup (`run_daemon`)
    /// and reload (`cmd_reload`): section absent = disabled, bare section =
    /// enabled at the built-in default listen, explicit listen passes through
    /// (webui-api: 启动时按配置伺服; configuration: `listen` 缺省 127.0.0.1:9877).
    #[test]
    fn webui_intent_from_config_maps_presence_and_default() {
        let off: DaemonConfig = toml::from_str("[daemon]\nport = 1\n").unwrap();
        assert_eq!(WebuiIntent::from_config(&off), WebuiIntent::disabled());
        // A bare `[webui]` table enables the console at the default address.
        let bare: DaemonConfig = toml::from_str("[webui]\n").unwrap();
        assert_eq!(
            WebuiIntent::from_config(&bare),
            WebuiIntent {
                enabled: true,
                listen: crate::config::DEFAULT_WEBUI_LISTEN.to_string(),
            }
        );
        // An explicit listen passes through verbatim.
        let explicit: DaemonConfig =
            toml::from_str("[webui]\nlisten = '0.0.0.0:8080'\n").unwrap();
        assert_eq!(
            WebuiIntent::from_config(&explicit),
            WebuiIntent {
                enabled: true,
                listen: "0.0.0.0:8080".to_string(),
            }
        );
    }

    /// reload: an unchanged `[webui]` section re-publishes the same intent and
    /// must NOT bounce the console (a live TCP connection survives), while an
    /// invalid section fails the whole reload with the serving console
    /// untouched (webui-api: reload 时 [webui] 段非法 → 保持既有伺服状态不变),
    /// and a later valid section converges again.
    #[test]
    #[cfg(unix)]
    fn reload_invalid_webui_keeps_serving_and_recovers() {
        use std::io::Read as _;
        let (sup, _app_file) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'true'\nstartsecs = 0.0\n",
        );
        let cfg_path = sup.config_path();
        let mgr = spawn_manager(&sup);
        let port_a = free_port();
        let addr_a = format!("127.0.0.1:{port_a}");

        // Serving on A.
        std::fs::write(&cfg_path, format!("[webui]\nlisten = '127.0.0.1:{port_a}'\n")).unwrap();
        let out = sup.cmd_reload().unwrap();
        assert!(
            out.contains(&format!("webui: webui console serving on http://{addr_a}")),
            "reload reports the converged console: {out}"
        );
        // A live connection the reload must not disturb.
        let mut held = std::net::TcpStream::connect(&addr_a).unwrap();
        held.set_read_timeout(Some(Duration::from_millis(300))).unwrap();

        // Unchanged section: the intent is republished, the console is not
        // rebound (the held connection never sees an EOF).
        let out = sup.cmd_reload().unwrap();
        assert!(
            out.contains(&format!("webui: webui console serving on http://{addr_a}")),
            "an unchanged section still reports serving: {out}"
        );
        std::thread::sleep(Duration::from_millis(150));
        let mut buf = [0u8; 16];
        match held.read(&mut buf) {
            Ok(0) => panic!("the console was rebound on an unchanged reload"),
            Ok(_) => {} // unsolicited data would be harmless
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("the held connection broke on an unchanged reload: {e}"),
        }

        // Invalid section: the whole reload fails, A keeps serving.
        std::fs::write(&cfg_path, "[webui]\nlisten = 'no-host-or-port'\n").unwrap();
        let err = sup.cmd_reload().err().expect("invalid [webui] fails reload");
        assert!(
            err.to_string().contains("webui.listen"),
            "the error names the offending key: {err:#}"
        );
        assert!(
            std::net::TcpStream::connect(&addr_a).is_ok(),
            "the serving console is untouched by the failed reload"
        );

        // Recovery: a valid section converges again.
        std::fs::write(&cfg_path, format!("[webui]\nlisten = '127.0.0.1:{port_a}'\n")).unwrap();
        let out = sup.cmd_reload().unwrap();
        assert!(
            out.contains("webui: webui console serving on http://"),
            "reload recovers the console: {out}"
        );
        end_manager(&sup, mgr);
        let _ = sup.shutdown_all();
    }

    /// reload: the `[webui]` section is hot-applied (off→on→rebind→off),
    /// an invalid section fails the whole reload leaving the console
    /// untouched, and daemon hints still render (回归：[daemon] 语义不变).
    #[test]
    #[cfg(unix)]
    fn reload_hot_applies_webui_and_keeps_daemon_hints() {
        let (sup, _app_file) = setup(
            "demo",
            "[app]\nautostart = false\n[program.p]\ncommand = 'true'\nstartsecs = 0.0\n",
        );
        let cfg_path = sup.config_path();
        let mgr = spawn_manager(&sup);

        // off → on.
        let port_a = free_port();
        std::fs::write(
            &cfg_path,
            format!("[webui]\nlisten = '127.0.0.1:{port_a}'\n"),
        )
        .unwrap();
        let out = sup.cmd_reload().unwrap();
        assert!(
            out.contains(&format!("webui: webui console serving on http://127.0.0.1:{port_a}")),
            "reload reports the converged console: {out}"
        );
        assert!(std::net::TcpStream::connect(&format!("127.0.0.1:{port_a}")).is_ok());

        // Rebind: listen change.
        let port_b = free_port();
        std::fs::write(
            &cfg_path,
            format!("[webui]\nlisten = '127.0.0.1:{port_b}'\n"),
        )
        .unwrap();
        let out = sup.cmd_reload().unwrap();
        assert!(
            out.contains(&format!("serving on http://127.0.0.1:{port_b}")),
            "reload reports the new address: {out}"
        );
        std::thread::sleep(Duration::from_millis(150));
        assert!(std::net::TcpStream::connect(&format!("127.0.0.1:{port_a}")).is_err());
        assert!(std::net::TcpStream::connect(&format!("127.0.0.1:{port_b}")).is_ok());

        // on → off.
        std::fs::write(&cfg_path, "").unwrap();
        let out = sup.cmd_reload().unwrap();
        assert!(
            out.contains("webui: webui console disabled"),
            "reload reports the disabled console: {out}"
        );
        assert!(std::net::TcpStream::connect(&format!("127.0.0.1:{port_b}")).is_err());

        // Invalid listen: the whole reload fails, console stays off.
        std::fs::write(&cfg_path, "[webui]\nlisten = 'no-host-or-port'\n").unwrap();
        let err = sup.cmd_reload().err().expect("invalid [webui] fails reload");
        assert!(
            err.to_string().contains("webui.listen"),
            "the error names the offending key: {err:#}"
        );

        // Regression: [daemon] changes still produce restart hints, and webui
        // diffs never appear in pending.
        let port_c = free_port();
        std::fs::write(
            &cfg_path,
            format!("[daemon]\nport = 17310\n[webui]\nlisten = '127.0.0.1:{port_c}'\n"),
        )
        .unwrap();
        let out = sup.cmd_reload().unwrap();
        assert!(
            out.contains("daemon host/port changed: restart xkeeper to apply"),
            "daemon hint still renders: {out}"
        );
        assert!(out.contains("webui:"), "webui note appended: {out}");
        {
            let st = sup.state.lock().unwrap();
            assert!(
                st.pending.daemon_hints
                    .iter()
                    .any(|h| h.contains("host/port")),
                "hint recorded in pending"
            );
            assert!(
                st.pending.programs.is_empty() && st.pending.apps_added.is_empty(),
                "webui section must not create pending entries"
            );
        }
        end_manager(&sup, mgr);
        let _ = sup.shutdown_all();
    }
}
