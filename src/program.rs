//! A single supervised program and its restart policy.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};

use crate::config::{resolve_path, ProgramConfig};
use crate::platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    /// Child process is alive.
    Running,
    /// Waiting for the next restart attempt.
    Backoff,
    /// Exited and `autorestart = false`.
    Exited,
    /// Stopped on request (daemon shutdown).
    Stopped,
    /// Exceeded `max_restarts`, no longer restarted.
    Fatal,
}

impl fmt::Display for ProgramState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProgramState::Running => "running",
            ProgramState::Backoff => "backoff",
            ProgramState::Exited => "exited",
            ProgramState::Stopped => "stopped",
            ProgramState::Fatal => "fatal",
        };
        f.write_str(s)
    }
}

pub struct ManagedProgram {
    cfg: ProgramConfig,
    working_dir: PathBuf,
    out_log: PathBuf,
    err_log: PathBuf,
    state: ProgramState,
    child: Option<Child>,
    /// Keeps the (Windows) job object alive while the child runs.
    job: Option<platform::JobHandle>,
    start_time: Option<Instant>,
    restart_at: Option<Instant>,
    restarts_done: u32,
    total_exits: u64,
}

impl ManagedProgram {
    pub fn new(cfg: ProgramConfig, base_dir: &Path, log_dir: &Path) -> Self {
        let working_dir = resolve_path(&cfg.working_dir, base_dir);
        let out_log = log_dir.join(format!("{}.out.log", cfg.name));
        let err_log = log_dir.join(format!("{}.err.log", cfg.name));
        Self {
            cfg,
            working_dir,
            out_log,
            err_log,
            state: ProgramState::Stopped,
            child: None,
            job: None,
            start_time: None,
            restart_at: None,
            restarts_done: 0,
            total_exits: 0,
        }
    }

    // The getters below are exercised by unit tests and kept for status reporting.
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    #[allow(dead_code)]
    pub fn state(&self) -> ProgramState {
        self.state
    }

    pub fn is_running(&self) -> bool {
        self.state == ProgramState::Running && self.child.is_some()
    }

    #[allow(dead_code)]
    pub fn restart_count(&self) -> u32 {
        self.restarts_done
    }

    #[allow(dead_code)]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    fn open_log(&self, path: &Path) -> std::io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }

    /// Start the child process. On failure the normal backoff/fatal logic applies.
    pub fn spawn(&mut self) {
        let name = &self.cfg.name;
        let mut cmd = Command::new(&self.cfg.command);
        cmd.args(&self.cfg.args)
            .envs(&self.cfg.environment)
            .stdin(Stdio::null());

        if !self.working_dir.as_os_str().is_empty() {
            cmd.current_dir(&self.working_dir);
        }

        let (out, err) = match (self.open_log(&self.out_log), self.open_log(&self.err_log)) {
            (Ok(o), Ok(e)) => (o, e),
            (Err(e), _) | (_, Err(e)) => {
                error!(
                    "program[{name}] cannot open log file ({}/{}): {e}",
                    self.out_log.display(),
                    self.err_log.display()
                );
                self.schedule_restart_or_fatal(Instant::now());
                return;
            }
        };
        cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                self.job = platform::JobHandle::attach(&child);
                self.child = Some(child);
                self.start_time = Some(Instant::now());
                self.state = ProgramState::Running;
                info!("program[{name}] started (pid {pid})");
            }
            Err(e) => {
                error!("program[{name}] failed to start {:?}: {e}", self.cfg.command);
                self.schedule_restart_or_fatal(Instant::now());
            }
        }
    }

    /// Check on the child; called once per monitor tick.
    pub fn tick(&mut self, now: Instant) {
        match self.state {
            ProgramState::Running => {
                let status = match self.child.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => Some(status),
                        Ok(None) => None,
                        Err(e) => {
                            warn!("program[{}] try_wait failed: {e}", self.cfg.name);
                            None
                        }
                    },
                    None => None,
                };
                if let Some(status) = status {
                    let uptime = self.start_time.map(|t| t.elapsed()).unwrap_or_default();
                    self.child = None;
                    // Dropping the job handle lets the kernel clean up any
                    // processes the child left behind (Windows).
                    self.job = None;
                    self.total_exits += 1;
                    if uptime >= Duration::from_secs_f64(self.cfg.backoff_reset_after.max(0.0)) {
                        debug!(
                            "program[{}] ran for {:.1}s (>= backoff_reset_after), \
                             resetting restart counter",
                            self.cfg.name,
                            uptime.as_secs_f64()
                        );
                        self.restarts_done = 0;
                    }
                    info!(
                        "program[{}] exited: {status} (ran {:.1}s, exit #{})",
                        self.cfg.name,
                        uptime.as_secs_f64(),
                        self.total_exits
                    );
                    self.schedule_restart_or_fatal(now);
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

    /// Stop the child process (if any) and wait for it to die.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id();
        info!("program[{}] stopping (pid {pid})...", self.cfg.name);

        #[cfg(unix)]
        {
            // Ask nicely first, then fall through to the hard kill below
            // once stop_timeout has elapsed.
            if platform::terminate_gracefully(pid) {
                let deadline =
                    Instant::now() + Duration::from_secs_f64(self.cfg.stop_timeout.max(0.0));
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

        // Last resort (and the only option on Windows): hard kill.
        let _ = child.kill();
        let _ = child.wait();
        self.job = None;
        self.state = ProgramState::Stopped;
        info!("program[{}] stopped", self.cfg.name);
    }

    /// Decide what happens after an exit or a failed start.
    fn schedule_restart_or_fatal(&mut self, now: Instant) {
        if !self.cfg.autorestart {
            self.state = ProgramState::Exited;
            info!(
                "program[{}] not restarted (autorestart = false)",
                self.cfg.name
            );
            return;
        }
        if self.cfg.max_restarts > 0 && self.restarts_done >= self.cfg.max_restarts {
            self.state = ProgramState::Fatal;
            error!(
                "program[{}] exceeded max_restarts ({}), entering FATAL state",
                self.cfg.name, self.cfg.max_restarts
            );
            return;
        }
        let delay = (self.cfg.restart_backoff * 2f64.powi(self.restarts_done as i32))
            .min(self.cfg.max_restart_backoff);
        self.restart_at = Some(now + Duration::from_secs_f64(delay));
        self.restarts_done += 1;
        self.state = ProgramState::Backoff;
        warn!(
            "program[{}] will restart in {:.1}s (restart #{})",
            self.cfg.name, delay, self.restarts_done
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn tmp_log_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xkeeper-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn fast_exit_config() -> ProgramConfig {
        ProgramConfig {
            name: "fast-exit".to_string(),
            command: if cfg!(windows) { "cmd" } else { "sh" }.to_string(),
            args: if cfg!(windows) {
                vec!["/c".into(), "exit".into(), "3".into()]
            } else {
                vec!["-c".into(), "exit 3".into()]
            },
            restart_backoff: 0.05,
            max_restart_backoff: 0.05,
            backoff_reset_after: 3600.0,
            ..Default::default()
        }
    }

    fn drive(p: &mut ManagedProgram, until: impl Fn(&ManagedProgram) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !until(p) {
            p.tick(Instant::now());
            thread::sleep(Duration::from_millis(10));
        }
        p.tick(Instant::now());
    }

    #[test]
    fn restarts_failing_program() {
        let logs = tmp_log_dir("restart");
        let mut p = ManagedProgram::new(fast_exit_config(), Path::new("."), &logs);
        p.spawn();
        assert_eq!(p.state(), ProgramState::Running);
        drive(&mut p, |p| p.restart_count() >= 2);
        assert!(
            p.restart_count() >= 2,
            "expected at least 2 restarts, got {}",
            p.restart_count()
        );
        p.stop();
        let _ = std::fs::remove_dir_all(&logs);
    }

    #[test]
    fn fatal_after_max_restarts() {
        let logs = tmp_log_dir("fatal");
        let mut cfg = fast_exit_config();
        cfg.max_restarts = 2;
        let mut p = ManagedProgram::new(cfg, Path::new("."), &logs);
        p.spawn();
        drive(&mut p, |p| p.state() == ProgramState::Fatal);
        assert_eq!(p.state(), ProgramState::Fatal);
        p.stop();
        let _ = std::fs::remove_dir_all(&logs);
    }

    #[test]
    fn stops_running_program_and_writes_logs() {
        let logs = tmp_log_dir("stop");
        let cfg = ProgramConfig {
            name: "sleeper".to_string(),
            command: if cfg!(windows) { "ping" } else { "sleep" }.to_string(),
            args: if cfg!(windows) {
                vec!["-n".into(), "30".into(), "127.0.0.1".into()]
            } else {
                vec!["30".into()]
            },
            ..Default::default()
        };
        let mut p = ManagedProgram::new(cfg, Path::new("."), &logs);
        p.spawn();
        assert!(p.is_running());
        assert!(p.pid().is_some());

        let t0 = Instant::now();
        p.stop();
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "stop() should return quickly"
        );
        assert!(!p.is_running());
        assert_eq!(p.state(), ProgramState::Stopped);
        assert!(logs.join("sleeper.out.log").exists());
        let _ = std::fs::remove_dir_all(&logs);
    }
}
