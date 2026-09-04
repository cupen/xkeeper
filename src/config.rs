//! Configuration file (`config.toml`) parsing, defaults and validation.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Resolve `p` against `base` unless it is absolute (or empty).
pub fn resolve_path(p: &Path, base: &Path) -> PathBuf {
    if p.is_absolute() || p.as_os_str().is_empty() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Top-level configuration file structure.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Global daemon settings, `[daemon]`.
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// Programs to keep alive, declared as `[[program]]` tables.
    #[serde(default, rename = "program")]
    pub programs: Vec<ProgramConfig>,
}

impl Config {
    /// Read, parse and validate the config file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("failed to parse config file: {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Cross-field checks that serde cannot express.
    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();

        if !matches!(
            self.daemon.log_level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            errors.push(format!(
                "daemon.log_level: unknown level {:?} (expected trace/debug/info/warn/error)",
                self.daemon.log_level
            ));
        }
        if self.daemon.log_dir.as_os_str().is_empty() {
            errors.push("daemon.log_dir: must not be empty".to_string());
        }
        if self.daemon.monitor_interval <= 0.0 || self.daemon.monitor_interval > 60.0 {
            errors.push(format!(
                "daemon.monitor_interval: {} out of range (0, 60]",
                self.daemon.monitor_interval
            ));
        }

        let mut seen = HashSet::new();
        for (i, p) in self.programs.iter().enumerate() {
            let label = format!("program #{} (name={:?})", i + 1, p.name);
            if p.name.is_empty() {
                errors.push(format!("{label}: name is required"));
            } else if !is_valid_name(&p.name) {
                errors.push(format!(
                    "{label}: name must not be '.', '..' and must not contain \
                     /\\:*?\"<>| or control characters"
                ));
            } else if !seen.insert(p.name.clone()) {
                errors.push(format!("{label}: duplicate program name {:?}", p.name));
            }
            if p.command.trim().is_empty() {
                errors.push(format!("{label}: command is required"));
            }
            if p.restart_backoff <= 0.0 {
                errors.push(format!("{label}: restart_backoff must be > 0"));
            }
            if p.max_restart_backoff < p.restart_backoff {
                errors.push(format!(
                    "{label}: max_restart_backoff ({}) must be >= restart_backoff ({})",
                    p.max_restart_backoff, p.restart_backoff
                ));
            }
            if p.stop_timeout < 0.0 {
                errors.push(format!("{label}: stop_timeout must be >= 0"));
            }
            if p.backoff_reset_after < 0.0 {
                errors.push(format!("{label}: backoff_reset_after must be >= 0"));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            bail!("config validation failed:\n  - {}", errors.join("\n  - "));
        }
    }
}

/// Program names become log file names, so they must be filename-safe.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.chars().any(|c| {
            matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
        })
}

/// `[daemon]` section: settings of xkeeper itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Log level of xkeeper itself: trace/debug/info/warn/error.
    pub log_level: String,
    /// Directory for per-program stdout/stderr log files.
    pub log_dir: PathBuf,
    /// How often (seconds) children are checked for exit/restart.
    pub monitor_interval: f64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            log_dir: PathBuf::from("logs"),
            monitor_interval: 1.0,
        }
    }
}

/// One `[[program]]` table: a supervised child process and its restart policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProgramConfig {
    /// Unique name, also used as the log file base name.
    pub name: String,
    /// Executable to run (looked up in PATH if not an absolute path).
    pub command: String,
    /// Command line arguments.
    pub args: Vec<String>,
    /// Working directory of the child (default: the config file's directory).
    pub working_dir: PathBuf,
    /// Restart the program after it exits.
    pub autorestart: bool,
    /// Delay before the first restart; doubles on every consecutive restart.
    pub restart_backoff: f64,
    /// Upper bound of the exponential backoff.
    pub max_restart_backoff: f64,
    /// Give up (fatal state) after this many consecutive restarts; 0 = unlimited.
    pub max_restarts: u32,
    /// Seconds to wait after SIGTERM before force killing (Unix only).
    pub stop_timeout: f64,
    /// Extra environment variables for the child.
    pub environment: BTreeMap<String, String>,
    /// A run longer than this many seconds resets the backoff/restart counter.
    pub backoff_reset_after: f64,
}

impl Default for ProgramConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            working_dir: PathBuf::from("."),
            autorestart: true,
            restart_backoff: 1.0,
            max_restart_backoff: 30.0,
            max_restarts: 0,
            stop_timeout: 10.0,
            environment: BTreeMap::new(),
            backoff_reset_after: 60.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_example() {
        let cfg: Config = toml::from_str(
            r#"
            [daemon]
            log_level = "debug"
            log_dir = "var/log"
            monitor_interval = 0.5

            [[program]]
            name = "web"
            command = "python"
            args = ["-m", "http.server"]
            working_dir = "/srv/www"
            autorestart = false
            restart_backoff = 0.5
            max_restart_backoff = 10.0
            max_restarts = 5
            stop_timeout = 3
            backoff_reset_after = 30

            [program.environment]
            PORT = "8080"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.programs.len(), 1);
        let p = &cfg.programs[0];
        assert_eq!(p.name, "web");
        assert_eq!(p.args, vec!["-m", "http.server"]);
        assert!(!p.autorestart);
        assert_eq!(p.max_restarts, 5);
        assert_eq!(p.environment.get("PORT").map(String::as_str), Some("8080"));
        assert_eq!(cfg.daemon.monitor_interval, 0.5);
        cfg.validate().unwrap();
    }

    #[test]
    fn applies_defaults() {
        let cfg: Config = toml::from_str("[[program]]\nname='a'\ncommand='x'\n").unwrap();
        let p = &cfg.programs[0];
        assert!(p.autorestart);
        assert_eq!(p.restart_backoff, 1.0);
        assert_eq!(p.max_restart_backoff, 30.0);
        assert_eq!(p.stop_timeout, 10.0);
        assert_eq!(p.max_restarts, 0);
        assert_eq!(p.backoff_reset_after, 60.0);
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_duplicate_names() {
        let cfg: Config =
            toml::from_str("[[program]]\nname='a'\ncommand='x'\n[[program]]\nname='a'\ncommand='y'\n")
                .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_missing_name() {
        let cfg: Config = toml::from_str("[[program]]\ncommand='x'\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unsafe_name() {
        let cfg: Config = toml::from_str("[[program]]\nname='../evil'\ncommand='x'\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let r: Result<Config, _> =
            toml::from_str("[[program]]\nname='a'\ncommand='x'\nautorestrat=true\n");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_bad_interval() {
        let cfg: Config = toml::from_str("[daemon]\nmonitor_interval=0\n").unwrap();
        assert!(cfg.validate().is_err());
    }
}
