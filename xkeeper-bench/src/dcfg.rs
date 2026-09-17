//! Daemon-side paths for the connect topology: the app registry (`app_dir`)
//! and log directory are derived from the daemon's own config file, which the
//! status projection exposes as `daemon.config_source`. This keeps bench a
//! pure external consumer — no new /v1 fields, no daemon changes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct DaemonPaths {
    pub app_dir: PathBuf,
    pub log_dir: PathBuf,
}

/// Defaults mirroring `config::DaemonSettings::default()` (app_dir "apps"
/// relative to the config dir; a fixed absolute default log dir).
pub fn default_log_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::temp_dir().join("xkeeper").join("logs")
    } else {
        PathBuf::from("/tmp/xkeeper/logs")
    }
}

fn resolve(p: &str, config_dir: &Path) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() || pb.as_os_str().is_empty() {
        pb
    } else {
        config_dir.join(pb)
    }
}

/// Read the daemon config the running daemon was started with and resolve
/// `daemon.app_dir` / `daemon.log_dir`.
pub fn discover(config_source: &Path) -> Result<DaemonPaths> {
    let text = std::fs::read_to_string(config_source).with_context(|| {
        format!(
            "cannot read the target daemon's config {} — connect mode requires bench \
             to run on the same host as the daemon (it registers apps and reads logs \
             from disk)",
            config_source.display()
        )
    })?;
    let v: toml::Value =
        toml::from_str(&text).with_context(|| format!("cannot parse {}", config_source.display()))?;
    let config_dir = config_source
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let daemon = v.get("daemon");
    let app_dir = daemon
        .and_then(|d| d.get("app_dir"))
        .and_then(|s| s.as_str())
        .map(|s| resolve(s, &config_dir))
        .unwrap_or_else(|| resolve("apps", &config_dir));
    let log_dir = daemon
        .and_then(|d| d.get("log_dir"))
        .and_then(|s| s.as_str())
        .map(|s| resolve(s, &config_dir))
        .unwrap_or_else(default_log_dir);
    Ok(DaemonPaths { app_dir, log_dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xk-bench-dcfg-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("daemon.toml");
        std::fs::write(&f, body).unwrap();
        f
    }

    #[test]
    fn explicit_paths_win() {
        let f = write_tmp(
            "explicit",
            "[daemon]\napp_dir = \"/data/apps\"\nlog_dir = \"/data/logs\"\n",
        );
        let p = discover(&f).unwrap();
        assert_eq!(p.app_dir, PathBuf::from("/data/apps"));
        assert_eq!(p.log_dir, PathBuf::from("/data/logs"));
        let _ = std::fs::remove_dir_all(f.parent().unwrap());
    }

    #[test]
    fn relative_app_dir_resolves_against_config_dir() {
        let f = write_tmp("relative", "[daemon]\napp_dir = \"apps\"\n");
        let p = discover(&f).unwrap();
        let dir = f.parent().unwrap();
        assert_eq!(p.app_dir, dir.join("apps"));
        assert_eq!(p.log_dir, default_log_dir(), "log_dir falls back to the default");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_is_a_clear_error() {
        let err = discover(Path::new("/nonexistent/xk-bench/daemon.toml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("same host"),
            "error explains the connect-mode requirement: {err:#}"
        );
    }

    #[test]
    fn windows_style_literal_strings_parse() {
        // the daemon config writer emits single-quoted literal TOML strings
        // for Windows paths; make sure the toml crate handles those here too.
        let f = write_tmp("literal", "[daemon]\nlog_dir = 'C:\\xk\\logs'\n");
        let p = discover(&f).unwrap();
        // resolve_path semantics: C:\... is absolute only on Windows
        let expected = if cfg!(windows) {
            PathBuf::from("C:\\xk\\logs")
        } else {
            f.parent().unwrap().join("C:\\xk\\logs")
        };
        assert_eq!(p.log_dir, expected, "backslashes must survive the literal string");
        let _ = std::fs::remove_dir_all(f.parent().unwrap());
    }
}
