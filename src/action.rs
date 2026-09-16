//! Custom action execution (actions spec: 自定义动作执行契约).
//!
//! Everything here runs on worker threads spawned by the control plane —
//! never on the supervisor loop or through the command queue. The action
//! subprocess is the platform shell (unix `/bin/sh -c`, Windows `cmd /C`)
//! so pipes/redirects/`&&` work; this is the deliberate no-shell divergence
//! from supervised `command` fields (design D2). Results live only in the
//! HTTP response and the daemon log — never in the status projection.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::config::{self, ResolvedAction, VarRef};
use crate::platform;

/// The response carries at most this many bytes of output (the tail); the
/// full output goes to the daemon log line by line (design D8).
pub const OUTPUT_TAIL_BYTES: usize = 4000;

/// Result of one action run — the HTTP response shape (control-plane spec).
/// `exit_code` is `None` when no code was observed (spawn failure, killed by
/// the timeout teardown, or signal death on unix).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ActionResult {
    pub program: String,
    pub action: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub output: String,
}

/// The daemon-log prefix for one action's output:
/// `[program.<name>.action.<name>]`.
pub fn log_prefix(program: &str, action: &str) -> String {
    format!("[program.{program}.action.{action}]")
}

/// Keep at most the last [`OUTPUT_TAIL_BYTES`] bytes without splitting a
/// UTF-8 character; shorter output passes through unchanged.
pub fn output_tail(output: &str) -> String {
    if output.len() <= OUTPUT_TAIL_BYTES {
        return output.to_string();
    }
    let mut cut = output.len() - OUTPUT_TAIL_BYTES;
    while !output.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &output[cut..])
}

/// Write the full action output to the daemon log, one line per line, each
/// prefixed with [`log_prefix`].
fn log_full_output(prefix: &str, exit_line: &str, text: &str) {
    info!("{prefix} {exit_line}");
    for line in text.lines() {
        info!("{prefix} {line}");
    }
}

/// The platform shell invocation for an action command (design D2).
fn shell_command(command: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(command);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    }
}

/// Kill the action process tree: the child was put in its own process group
/// (unix) / job object (windows), so grandchildren die with it.
fn kill_tree(child: &mut Child, job: Option<platform::JobHandle>, pid: u32) {
    #[cfg(unix)]
    {
        let _ = job; // placeholder only on unix
        platform::kill_group(pid);
    }
    #[cfg(windows)]
    {
        // Closing the job handle triggers JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        // killing every process left in the job (the whole tree).
        drop(job);
    }
    let _ = child.kill();
}

/// Run one action command via the platform shell, capturing output. Blocks
/// until the child exits or `spec.timeout` elapses (then the tree is killed
/// and `timed_out` is set). Never touches supervisor state — safe to call
/// from any worker thread.
pub fn execute(
    spec: &ResolvedAction,
    program: &str,
    action: &str,
    command: &str,
    work_dir: &Path,
) -> ActionResult {
    let started = Instant::now();
    let prefix = log_prefix(program, action);
    let mut cmd = shell_command(command);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !work_dir.as_os_str().is_empty() {
        cmd.current_dir(work_dir);
    }
    #[cfg(unix)]
    {
        // Own process group so the timeout teardown can kill the whole tree.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!("{prefix} failed to spawn ({e})");
            return ActionResult {
                program: program.to_string(),
                action: action.to_string(),
                exit_code: None,
                duration_ms: 0,
                timed_out: false,
                output: format!("failed to spawn action: {e}"),
            };
        }
    };
    let pid = child.id();
    // Windows: kill-on-close job object so orphaned grandchildren cannot
    // survive this worker. Unix: placeholder (process group covers it).
    let job = platform::JobHandle::attach(&child);

    // Drain stdout/stderr on reader threads while the parent waits bounded.
    let out_pipe = child.stdout.take().expect("stdout was piped");
    let err_pipe = child.stderr.take().expect("stderr was piped");
    fn drain(p: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::Builder::new()
            .name("action-read".into())
            .spawn(move || {
                let mut buf = Vec::new();
                let mut p = p;
                let _ = std::io::Read::read_to_end(&mut p, &mut buf);
                buf
            })
            .expect("spawn action reader")
    }
    let out_reader = drain(out_pipe);
    let err_reader = drain(err_pipe);

    let timeout = Duration::from_secs(spec.timeout.max(1));
    let deadline = started + timeout;
    let mut status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break None,
        }
    };

    let timed_out = status.is_none();
    if timed_out {
        warn!(
            "{prefix} timed out after {}s, killing process tree (pid {pid})",
            spec.timeout
        );
        kill_tree(&mut child, job, pid);
        // Reap so the child does not linger; the exit code is meaningless now.
        status = child.wait().ok();
    } else {
        drop(job);
    }
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();

    // Byte-to-UTF-8 lenient conversion (windows `cmd /C` may emit a local
    // codepage; lossy keeps the daemon alive, replacement chars are accepted).
    let mut full = String::from_utf8_lossy(&out).into_owned();
    full.push_str(&String::from_utf8_lossy(&err));
    let duration_ms = started.elapsed().as_millis() as u64;
    let exit_line = match (&timed_out, status.as_ref().and_then(|s| s.code())) {
        (true, _) => "timed out".to_string(),
        (false, Some(code)) => format!("exited with code {code}"),
        (false, None) => "exited (no exit code)".to_string(),
    };
    log_full_output(&prefix, &exit_line, &full);
    ActionResult {
        program: program.to_string(),
        action: action.to_string(),
        exit_code: if timed_out {
            None
        } else {
            status.and_then(|s| s.code())
        },
        duration_ms,
        timed_out,
        output: output_tail(&full),
    }
}

// ---------------------------------------------------------------------------
// (program, action) mutual exclusion
// ---------------------------------------------------------------------------

/// Registry of actions currently in flight. Keyed by (program, action): a
/// second run of the SAME pair is refused while one is running (no queueing,
/// actions spec); different actions of the same program may run concurrently.
#[derive(Default)]
pub struct ActionRuns(Mutex<HashSet<(String, String)>>);

/// Released on the worker thread when the run ends (or on early returns) —
/// dropping the guard frees the (program, action) slot.
pub struct ActionRunGuard {
    registry: Arc<ActionRuns>,
    key: (String, String),
}

/// The pair is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight;

impl ActionRuns {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a (program, action) run or report the conflict.
    pub fn try_begin(
        self: &Arc<Self>,
        program: &str,
        action: &str,
    ) -> Result<ActionRunGuard, InFlight> {
        let mut slots = self.0.lock().unwrap();
        let key = (program.to_string(), action.to_string());
        if !slots.insert(key.clone()) {
            return Err(InFlight);
        }
        Ok(ActionRunGuard {
            registry: self.clone(),
            key,
        })
    }

    #[cfg(test)]
    fn in_flight(&self, program: &str, action: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .contains(&(program.to_string(), action.to_string()))
    }
}

impl Drop for ActionRunGuard {
    fn drop(&mut self) {
        self.registry.0.lock().unwrap().remove(&self.key);
    }
}

// ---------------------------------------------------------------------------
// spawn-time variable substitution
// ---------------------------------------------------------------------------

/// Runtime values for one program, snapshotted from supervisor state.
#[derive(Debug, Clone, Default)]
pub struct ProgramVars {
    /// `None` while the program has no live child — `${...pid}` becomes the
    /// empty string (actions spec: 对已停程序执行动作).
    pub pid: Option<u32>,
    pub state: String,
    pub app: String,
    pub work_dir: PathBuf,
    pub log_dir: PathBuf,
}

/// Everything `${...}` references can resolve to at spawn time
/// (configuration spec: 动作定义表 variable table).
#[derive(Debug, Clone, Default)]
pub struct RunContext {
    pub daemon_pid: u32,
    pub daemon_host: String,
    pub daemon_port: u16,
    pub daemon_log_dir: PathBuf,
    pub daemon_app_dir: PathBuf,
    /// App name -> app config path.
    pub apps: BTreeMap<String, PathBuf>,
    /// Program name -> runtime values (programs are globally unique, so a
    /// flat map covers cross-program references).
    pub programs: BTreeMap<String, ProgramVars>,
}

impl RunContext {
    /// Resolve one variable reference; `None` = no value known (empty string
    /// at the substitution site).
    pub fn resolve(&self, r: &VarRef) -> Option<String> {
        match r.domain {
            config::VarDomain::Daemon => match r.field.as_str() {
                "pid" => Some(self.daemon_pid.to_string()),
                "host" => Some(self.daemon_host.clone()),
                "port" => Some(self.daemon_port.to_string()),
                "log_dir" => Some(self.daemon_log_dir.display().to_string()),
                "app_dir" => Some(self.daemon_app_dir.display().to_string()),
                _ => None,
            },
            config::VarDomain::App => self.apps.get(&r.name).map(|p| p.display().to_string()),
            config::VarDomain::Program => {
                self.programs.get(&r.name).map(|p| match r.field.as_str() {
                    "pid" => p.pid.map(|x| x.to_string()).unwrap_or_default(),
                    "state" => p.state.clone(),
                    "app" => p.app.clone(),
                    "work_dir" => p.work_dir.display().to_string(),
                    "log_dir" => p.log_dir.display().to_string(),
                    _ => String::new(),
                })
            }
        }
    }

    /// Substitute all known-domain variables; unknown-domain `${...}`
    /// survives verbatim for the shell.
    pub fn substitute(&self, template: &str) -> String {
        config::substitute_vars(template, |r| self.resolve(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str, timeout: u64) -> ResolvedAction {
        ResolvedAction {
            command: command.to_string(),
            timeout,
        }
    }

    fn work_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xk-action-{}-{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn log_prefix_shape() {
        assert_eq!(log_prefix("api", "upgrade"), "[program.api.action.upgrade]");
    }

    #[test]
    fn tail_keeps_last_bytes_and_utf8() {
        let short = "all good\n";
        assert_eq!(output_tail(short), short);
        let long = "x".repeat(OUTPUT_TAIL_BYTES + 500);
        let t = output_tail(&long);
        assert_eq!(t.len(), OUTPUT_TAIL_BYTES + "…".len());
        assert!(t.ends_with(&"x".repeat(50)));
        // Multi-byte characters are never split.
        let multi = "水".repeat((OUTPUT_TAIL_BYTES / 3) + 40);
        let t = output_tail(&multi);
        assert!(t.contains('水'));
        assert_eq!(t.chars().all(|c| c == '…' || c == '水'), true);
    }

    /// action: a successful command yields exit code 0 and captured output.
    #[test]
    fn executes_and_captures_output() {
        let (cmd, expect) = if cfg!(windows) {
            ("echo xkeeper-action-ok", "xkeeper-action-ok")
        } else {
            (
                "echo xkeeper-action-ok; echo err-line >&2",
                "xkeeper-action-ok",
            )
        };
        let r = execute(&spec(cmd, 10), "p", "a", cmd, &work_dir("exec"));
        assert_eq!(r.exit_code, Some(0));
        assert!(!r.timed_out);
        assert!(r.output.contains(expect), "output: {:?}", r.output);
        assert!(r.duration_ms < 10_000);
    }

    /// action: the child's exit code is carried through.
    #[test]
    fn carries_exit_code() {
        let cmd = if cfg!(windows) {
            "cmd /c exit 3"
        } else {
            "exit 3"
        };
        let r = execute(&spec(cmd, 10), "p", "a", cmd, &work_dir("code"));
        assert_eq!(r.exit_code, Some(3));
    }

    /// action: cwd defaults to the program's work_dir (scripts resolve
    /// relative paths against it).
    #[test]
    fn runs_in_work_dir() {
        let dir = work_dir("cwd");
        if cfg!(windows) {
            // Best effort on windows: `cd` prints the directory.
            let r = execute(&spec("cd", 10), "p", "a", "cd", &dir);
            assert!(
                r.output
                    .to_lowercase()
                    .contains(&dir.to_string_lossy().to_lowercase())
            );
        } else {
            let r = execute(&spec("pwd", 10), "p", "a", "pwd", &dir);
            assert_eq!(r.exit_code, Some(0));
            assert_eq!(r.output.trim(), dir.to_str().unwrap());
        }
    }

    /// action: a command exceeding its timeout is killed with its tree and
    /// reported as timed_out (the wall time stays near the timeout).
    #[test]
    fn timeout_kills_tree() {
        let (cmd, probe) = if cfg!(windows) {
            ("ping -n 31 127.0.0.1", None)
        } else {
            ("sleep 31", Some("31"))
        };
        let t0 = Instant::now();
        let r = execute(&spec(cmd, 1), "p", "a", cmd, &work_dir("timeout"));
        let elapsed = t0.elapsed();
        assert!(r.timed_out, "must be marked timed_out");
        assert_eq!(r.exit_code, None);
        assert!(
            elapsed < Duration::from_secs(15),
            "timeout teardown must not wait for the child: {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(900),
            "must honor the timeout"
        );
        // unix: prove the tree is really gone — nothing still runs the probe.
        #[cfg(unix)]
        if let Some(arg) = probe {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if scan_proc_for_arg(arg).is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(
                scan_proc_for_arg(arg).is_empty(),
                "the timed-out child (and its group) must be dead"
            );
        }
    }

    /// Scan /proc for any process whose argv contains `arg` (e2e-style tree
    /// assertions without external tools). Non-unix builds never call this.
    #[cfg(unix)]
    fn scan_proc_for_arg(arg: &str) -> Vec<u32> {
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

    /// action: (program, action) mutual exclusion — the same pair conflicts,
    /// different actions of one program run concurrently, and the slot is
    /// freed when the guard drops.
    #[test]
    fn mutex_registry() {
        let runs = ActionRuns::new();
        let g1 = runs.try_begin("api", "upgrade").expect("first run begins");
        assert!(runs.in_flight("api", "upgrade"));
        assert!(
            matches!(runs.try_begin("api", "upgrade"), Err(InFlight)),
            "same (program, action) must conflict"
        );
        // Different action, same program: allowed in parallel.
        let g2 = runs
            .try_begin("api", "flush")
            .expect("parallel action allowed");
        // Different program, same action name: allowed.
        let _g3 = runs
            .try_begin("web", "upgrade")
            .expect("other program allowed");
        drop(g1);
        assert!(!runs.in_flight("api", "upgrade"));
        assert!(
            runs.try_begin("api", "upgrade").is_ok(),
            "slot freed after drop"
        );
        drop(g2);
    }

    /// action: spawn-time substitution replaces known-domain variables;
    /// a pid that is absent becomes the empty string; shell vars survive.
    #[test]
    fn run_context_substitution() {
        let mut ctx = RunContext {
            daemon_pid: 7,
            daemon_host: "127.0.0.1".into(),
            daemon_port: 7310,
            ..Default::default()
        };
        ctx.programs.insert(
            "api".into(),
            ProgramVars {
                pid: Some(4242),
                state: "running".into(),
                app: "demo".into(),
                work_dir: PathBuf::from("/opt/api"),
                log_dir: PathBuf::from("/var/log/xk"),
            },
        );
        ctx.programs.insert(
            "down".into(),
            ProgramVars {
                pid: None,
                state: "stopped".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            ctx.substitute("pid=${program.api.pid} state=${program.api.state} app=${program.api.app} wd=${program.api.work_dir} ld=${program.api.log_dir}"),
            "pid=4242 state=running app=demo wd=/opt/api ld=/var/log/xk"
        );
        // Cross-program reference by global name.
        assert_eq!(ctx.substitute("api=${program.api.pid}"), "api=4242");
        // Stopped program: pid is the empty string, state still resolves.
        assert_eq!(
            ctx.substitute("[${program.down.pid}]${program.down.state}"),
            "[]stopped"
        );
        // Daemon domain.
        assert_eq!(
            ctx.substitute("d=${daemon.pid} ${daemon.host}:${daemon.port}"),
            "d=7 127.0.0.1:7310"
        );
        // Unknown domain passes through for the shell.
        assert_eq!(ctx.substitute("echo ${HOME}"), "echo ${HOME}");
    }
}
