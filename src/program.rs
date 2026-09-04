//! A single supervised program: 7-state machine with startsecs/startretries,
//! restart policies, graceful stop and health marking.

use std::fmt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};

use crate::config::{ResolvedProgram, RestartPolicy};
use crate::platform;
use crate::pump::{self, PumpSet, Stream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    /// Child process is alive but has not reached `startsecs` yet.
    Starting,
    /// Child process is alive and stable.
    Running,
    /// Graceful stop in progress.
    Stopping,
    /// Waiting for the next restart attempt.
    Backoff,
    /// Exited and will not be restarted (policy or expected exit).
    Exited,
    /// Stopped on request (daemon shutdown or explicit stop).
    Stopped,
    /// Start retries exhausted or `max_restarts` exceeded; no auto restart.
    Fatal,
}

impl fmt::Display for ProgramState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProgramState::Starting => "starting",
            ProgramState::Running => "running",
            ProgramState::Stopping => "stopping",
            ProgramState::Backoff => "backoff",
            ProgramState::Exited => "exited",
            ProgramState::Stopped => "stopped",
            ProgramState::Fatal => "fatal",
        };
        f.write_str(s)
    }
}

pub struct ManagedProgram {
    pub def: ResolvedProgram,
    pub log_dir: PathBuf,
    out_ring: Arc<pump::Ring>,
    err_ring: Arc<pump::Ring>,
    state: ProgramState,
    child: Option<Child>,
    job: Option<platform::JobHandle>,
    start_time: Option<Instant>,
    restart_at: Option<Instant>,
    start_failures: u32,
    restarts_done: u32,
    total_exits: u64,
    health_fails: u32,
    unhealthy: bool,
    /// Set by an explicit stop; cleared on the next start. A user-stopped
    /// program must not be auto-started again by dependency evaluation.
    user_stopped: bool,
    last_exit: Option<String>,
    fatal_reason: Option<String>,
    pumps: Option<PumpSet>,
}

impl ManagedProgram {
    pub fn new(def: ResolvedProgram, log_dir: PathBuf, ring_cap: usize) -> Self {
        Self {
            def,
            log_dir,
            out_ring: pump::Ring::new(ring_cap),
            err_ring: pump::Ring::new(ring_cap),
            state: ProgramState::Stopped,
            child: None,
            job: None,
            start_time: None,
            restart_at: None,
            start_failures: 0,
            restarts_done: 0,
            total_exits: 0,
            health_fails: 0,
            unhealthy: false,
            user_stopped: false,
            last_exit: None,
            fatal_reason: None,
            pumps: None,
        }
    }

    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.def.name
    }

    pub fn state(&self) -> ProgramState {
        self.state
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, ProgramState::Running | ProgramState::Starting)
            && self.child.is_some()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    pub fn unhealthy(&self) -> bool {
        self.unhealthy
    }

    /// True when an explicit user stop requested this program to stay down.
    pub fn user_stopped(&self) -> bool {
        self.user_stopped
    }

    pub fn total_exits(&self) -> u64 {
        self.total_exits
    }

    pub fn last_exit(&self) -> Option<&str> {
        self.last_exit.as_deref()
    }

    pub fn fatal_reason(&self) -> Option<&str> {
        self.fatal_reason.as_deref()
    }

    pub fn wait_reason(&self) -> Option<String> {        if !self.def.depends_on.is_empty()
            && self.state == ProgramState::Stopped
            && !self.user_stopped
        {
            Some(format!("waiting for dependencies: {}", self.def.depends_on.join(", ")))
        } else {
            None
        }
    }

    /// Access the ring buffer of a stream (for the log API). The rings live
    /// as long as the program, so tails survive exits and restarts.
    pub fn ring(&self, stream: Stream) -> Arc<pump::Ring> {
        match stream {
            Stream::Out => self.out_ring.clone(),
            Stream::Err => self.err_ring.clone(),
        }
    }

    fn backoff_delay(&self, attempt: u32) -> f64 {
        (self.def.restart_backoff * 2f64.powi(attempt as i32)).min(self.def.max_restart_backoff)
    }

    /// Start the child process. On failure the normal backoff/fatal logic applies.
    pub fn spawn(&mut self) {
        self.user_stopped = false;
        let name = self.def.name.clone();
        let mut cmd = Command::new(&self.def.command);
        cmd.args(&self.def.args)
            .envs(&self.def.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !self.def.work_dir.as_os_str().is_empty() {
            cmd.current_dir(&self.def.work_dir);
        }
        #[cfg(unix)]
        {
            // Own process group so we can kill the whole tree later.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        match cmd.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                self.job = platform::JobHandle::attach(&child);
                let out = child.stdout.take().expect("stdout was piped");
                let err = child.stderr.take().expect("stderr was piped");
                self.pumps = Some(pump::start(
                    out,
                    err,
                    &name,
                    self.out_ring.clone(),
                    self.err_ring.clone(),
                    &self.log_dir,
                    self.def.log_max_size,
                    self.def.log_rotate_keep,
                ));
                self.child = Some(child);
                self.start_time = Some(Instant::now());
                self.state = ProgramState::Starting;
                info!("program[{name}] starting (pid {pid})");
            }
            Err(e) => {
                error!("program[{name}] failed to start {:?}: {e}", self.def.command);
                self.last_exit = Some(format!("spawn error: {e}"));
                self.total_exits += 1;
                self.handle_failed_start(Instant::now());
            }
        }
    }

    /// One monitoring step; called from the supervisor loop.
    pub fn tick(&mut self, now: Instant) {
        match self.state {
            ProgramState::Starting => {
                // Did it die before reaching startsecs?
                let exited = match self.child.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => Some(status),
                        Ok(None) => None,
                        Err(e) => {
                            warn!("program[{}] try_wait failed: {e}", self.def.name);
                            None
                        }
                    },
                    None => None,
                };
                if let Some(status) = exited {
                    let uptime = self.start_time.map(|t| t.elapsed()).unwrap_or_default();
                    self.finish_child(status);
                    self.total_exits += 1;
                    self.last_exit =
                        Some(format!("{status} after {:.1}s", uptime.as_secs_f64()));
                    self.handle_failed_start(now);
                } else if self
                    .start_time
                    .map(|t| t.elapsed() >= Duration::from_secs_f64(self.def.startsecs))
                    .unwrap_or(false)
                {
                    // Stable run: the start-retry budget resets.
                    self.state = ProgramState::Running;
                    self.start_failures = 0;
                    self.unhealthy = false;
                    debug!("program[{}] is now running (stable for {:.1}s)",
                        self.def.name, self.def.startsecs);
                }
            }
            ProgramState::Running => {
                let status = match self.child.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => Some(status),
                        Ok(None) => None,
                        Err(e) => {
                            warn!("program[{}] try_wait failed: {e}", self.def.name);
                            None
                        }
                    },
                    None => None,
                };
                if let Some(status) = status {
                    self.finish_child(status);
                    self.total_exits += 1;
                    let uptime = self.start_time.map(|t| t.elapsed()).unwrap_or_default();
                    self.last_exit = Some(format!("{status} after {:.1}s", uptime.as_secs_f64()));
                    info!(
                        "program[{}] exited: {} (exit #{})",
                        self.def.name,
                        self.last_exit.as_deref().unwrap_or_default(),
                        self.total_exits
                    );
                    if uptime >= Duration::from_secs_f64(self.def.backoff_reset_after.max(0.0)) {
                        debug!(
                            "program[{}] ran stable for {:.1}s, resetting restart counter",
                            self.def.name,
                            uptime.as_secs_f64()
                        );
                        self.restarts_done = 0;
                    }
                    self.schedule_after_exit(now);
                }
            }
            ProgramState::Backoff => {
                if matches!(self.restart_at, Some(t) if now >= t) {
                    self.spawn();
                }
            }
            _ => {}
        }
    }

    fn finish_child(&mut self, status: std::process::ExitStatus) {
        self.child = None;
        // Dropping the job handle lets the kernel clean up leftovers (Windows);
        // pumps finish on EOF.
        self.job = None;
        self.pumps = None;
        self.unhealthy = false;
        self.health_fails = 0;
        let _ = status;
    }

    fn handle_failed_start(&mut self, now: Instant) {
        self.start_failures += 1;
        if self.start_failures > self.def.startretries {
            self.state = ProgramState::Fatal;
            self.fatal_reason = Some(format!(
                "start failed {} times (budget {})",
                self.start_failures, self.def.startretries
            ));
            error!(
                "program[{}] entering FATAL state: {}",
                self.def.name,
                self.fatal_reason.as_deref().unwrap_or_default()
            );
            return;
        }
        let delay = self.backoff_delay(self.start_failures.saturating_sub(1));
        self.restart_at = Some(now + Duration::from_secs_f64(delay));
        self.state = ProgramState::Backoff;
        warn!(
            "program[{}] start failed ({}/{}), retrying in {:.1}s",
            self.def.name, self.start_failures, self.def.startretries, delay
        );
    }

    fn schedule_after_exit(&mut self, now: Instant) {
        let restart = match self.def.autorestart {
            RestartPolicy::Never => false,
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => {
                let code = self.last_exit.as_deref().and_then(|s| {
                    // "exit code: N" is the std formatting for numeric exits
                    s.split("exit code: ").nth(1).and_then(|r| {
                        r.split(|c: char| !c.is_ascii_digit()).next()?.parse::<i32>().ok()
                    })
                });
                !matches!(code, Some(c) if self.def.exit_codes.contains(&c))
            }
        };
        if !restart {
            self.state = ProgramState::Exited;
            info!(
                "program[{}] not restarted (autorestart = {:?})",
                self.def.name, self.def.autorestart
            );
            return;
        }
        if self.def.max_restarts > 0 && self.restarts_done >= self.def.max_restarts {
            self.state = ProgramState::Fatal;
            self.fatal_reason = Some(format!(
                "exceeded max_restarts ({})",
                self.def.max_restarts
            ));
            error!(
                "program[{}] entering FATAL state: {}",
                self.def.name,
                self.fatal_reason.as_deref().unwrap_or_default()
            );
            return;
        }
        let delay = self.backoff_delay(self.restarts_done);
        self.restart_at = Some(now + Duration::from_secs_f64(delay));
        self.restarts_done += 1;
        self.state = ProgramState::Backoff;
        warn!(
            "program[{}] will restart in {:.1}s (restart #{})",
            self.def.name, delay, self.restarts_done
        );
    }

    /// Stop the child (if any) and wait for it to die.
    pub fn stop(&mut self) {
        self.user_stopped = true;
        self.restart_at = None;
        let Some(mut child) = self.child.take() else {
            if matches!(self.state, ProgramState::Backoff) {
                self.state = ProgramState::Stopped;
            }
            return;
        };
        let pid = child.id();
        self.state = ProgramState::Stopping;
        info!("program[{}] stopping (pid {pid})...", self.def.name);

        #[cfg(unix)]
        {
            if platform::terminate_gracefully(pid) {
                let deadline =
                    Instant::now() + Duration::from_secs_f64(self.def.stop_timeout.max(0.0));
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() >= deadline => break,
                        Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                        Err(_) => break,
                    }
                }
            }
        }

        // Last resort (and the only option on Windows): hard kill the tree.
        #[cfg(unix)]
        let _ = platform::kill_group(pid);
        let _ = child.kill();
        let _ = child.wait();
        self.job = None;
        self.pumps = None;
        self.unhealthy = false;
        self.state = ProgramState::Stopped;
        info!("program[{}] stopped", self.def.name);
    }

    /// Record a health probe result. Returns true if this should trigger a
    /// restart (unhealthy + restart_on_unhealthy).
    pub fn record_health(&mut self, healthy: bool, _now: Instant) -> bool {
        match healthy {
            true => {
                self.health_fails = 0;
                if self.unhealthy {
                    self.unhealthy = false;
                    info!("program[{}] is healthy again", self.def.name);
                }
                false
            }
            false => {
                // Failures during the warm-up window do not count.
                let warming = self
                    .start_time
                    .map(|t| t.elapsed() < Duration::from_secs_f64(self.def.startsecs + 0.0))
                    .unwrap_or(false)
                    || self.state != ProgramState::Running;
                if warming {
                    return false;
                }
                self.health_fails += 1;
                if self.health_fails < self.def.health.as_ref().map(|h| h.retries).unwrap_or(1) {
                    return false;
                }
                self.health_fails = 0;
                if !self.unhealthy {
                    self.unhealthy = true;
                    warn!("program[{}] is UNHEALTHY", self.def.name);
                }
                self.def.restart_on_unhealthy && self.state == ProgramState::Running
            }
        }
    }

    /// Clear a fatal state after an explicit start request.
    pub fn clear_fatal(&mut self) {
        self.fatal_reason = None;
        self.start_failures = 0;
        self.restarts_done = 0;
    }

    /// Enter fatal because a dependency is permanently unavailable.
    pub fn mark_dependency_fatal(&mut self, reason: String) {
        if self.state != ProgramState::Stopped {
            return;
        }
        self.state = ProgramState::Fatal;
        self.fatal_reason = Some(format!("dependencies unavailable: {reason}"));
        error!(
            "program[{}] entering FATAL state: {}",
            self.def.name,
            self.fatal_reason.as_deref().unwrap_or_default()
        );
    }

    /// Uptime of the current (or last) run, for status display.
    pub fn uptime_secs(&self) -> f64 {
        self.start_time
            .filter(|_| self.is_running())
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::thread;

    use crate::config::{resolve_app, AppRaw};

    fn resolved(toml_text: &str) -> ResolvedProgram {
        let raw: AppRaw = toml::from_str(toml_text).unwrap();
        let app = resolve_app("demo", Path::new("x.toml"), &raw, None).unwrap();
        app.programs.into_iter().next().unwrap()
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xk-prog-{}-{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn make(toml_text: &str, tag: &str) -> ManagedProgram {
        let def = resolved(toml_text);
        ManagedProgram::new(def, tmp_dir(tag), 100)
    }

    fn fast_exit(overrides: &str) -> String {
        format!(
            "[program.t]\ncommand = \"{}\"\nstartsecs = 0.05\nrestart_backoff = 0.05\nmax_restart_backoff = 0.05\nbackoff_reset_after = 3600\n{}",
            if cfg!(windows) { "cmd /c exit 3" } else { "sh -c \"exit 3\"" },
            overrides
        )
    }

    /// A program that lives ~1s and then exits 0, long enough to pass
    /// `startsecs` and become Running.
    fn slow_exit(overrides: &str) -> String {
        format!(
            "[program.t]\ncommand = \"{}\"\nstartsecs = 0.2\nrestart_backoff = 0.05\nmax_restart_backoff = 0.05\nbackoff_reset_after = 3600\n{}",
            if cfg!(windows) { "ping -n 2 127.0.0.1" } else { "sleep 1" },
            overrides
        )
    }

    fn drive(p: &mut ManagedProgram, until: impl Fn(&ManagedProgram) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && !until(p) {
            p.tick(Instant::now());
            thread::sleep(Duration::from_millis(10));
        }
        p.tick(Instant::now());
    }

    #[test]
    fn restarts_and_reaches_stable_running() {
        let mut p = make(&slow_exit(""), "restart");
        p.spawn();
        drive(&mut p, |p| p.total_exits() >= 2 && p.state() == ProgramState::Running);
        assert_eq!(p.state(), ProgramState::Running);
        assert!(p.total_exits() >= 2, "expected restarts, exits={}", p.total_exits());
        p.stop();
        let _ = std::fs::remove_dir_all(p.log_dir.clone());
    }

    #[test]
    fn fatal_after_start_budget() {
        let mut p = make(&fast_exit("startretries = 2\n"), "fatal");
        p.spawn();
        drive(&mut p, |p| p.state() == ProgramState::Fatal);
        assert_eq!(p.state(), ProgramState::Fatal);
        assert!(p.fatal_reason().unwrap().contains("start failed"));
        p.stop();
        let _ = std::fs::remove_dir_all(p.log_dir.clone());
    }

    #[test]
    fn never_policy_stays_exited() {
        let mut p = make(&slow_exit("autorestart = \"never\"\n"), "never");
        p.spawn();
        drive(&mut p, |p| p.state() == ProgramState::Exited);
        assert_eq!(p.state(), ProgramState::Exited);
        for _ in 0..5 {
            p.tick(Instant::now());
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(p.state(), ProgramState::Exited, "must stay exited");
        let _ = std::fs::remove_dir_all(p.log_dir.clone());
    }

    #[test]
    fn stops_running_program_and_writes_logs() {
        let mut p = make(
            &format!(
                "[program.t]\ncommand = \"{}\"\nstartsecs = 0.2\n",
                if cfg!(windows) { "ping -n 30 127.0.0.1" } else { "sleep 30" }
            ),
            "stop",
        );
        p.spawn();
        drive(&mut p, |p| p.state() == ProgramState::Running);
        assert!(p.is_running());
        let t0 = Instant::now();
        p.stop();
        assert!(t0.elapsed() < Duration::from_secs(5), "stop should be quick");
        assert_eq!(p.state(), ProgramState::Stopped);
        assert!(p.log_dir.join("t.out.log").exists());
        let _ = std::fs::remove_dir_all(p.log_dir.clone());
    }

    #[test]
    fn on_failure_respects_expected_exit_codes() {
        // exits 0 which IS expected -> no restart, stays exited
        let mut p = make(
            &slow_exit("autorestart = \"on-failure\"\nexit_codes = [0]\n"),
            "expect",
        );
        p.spawn();
        drive(&mut p, |p| p.state() == ProgramState::Exited);
        assert_eq!(p.state(), ProgramState::Exited);
        let _ = std::fs::remove_dir_all(p.log_dir.clone());
    }

    #[test]
    fn on_failure_restarts_on_unexpected_exit_code() {
        // exits 0 but only 3 is expected -> restart and run again
        let mut p = make(
            &slow_exit("autorestart = \"on-failure\"\nexit_codes = [3]\n"),
            "unexpected",
        );
        p.spawn();
        drive(&mut p, |p| p.total_exits() >= 1 && p.state() == ProgramState::Running);
        assert_eq!(p.state(), ProgramState::Running);
        p.stop();
        let _ = std::fs::remove_dir_all(p.log_dir.clone());
    }
}
