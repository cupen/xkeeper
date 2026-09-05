//! systemd service registration: `xkeeper service install|uninstall`.
//!
//! Linux only — generates a unit file under `/etc/systemd/system/`, then
//! drives `systemctl` to reload/enable/start it. Windows builds keep the
//! subcommands discoverable but fail at runtime with a clear error. All
//! template rendering, name validation and write decisions are
//! platform-independent pure functions so they are testable everywhere.

use std::path::Path;

use anyhow::{bail, Result};

pub const DEFAULT_UNIT_NAME: &str = "xkeeper";
/// Fallback stop budget when the config cannot be loaded.
#[cfg_attr(windows, allow(dead_code))]
const FALLBACK_TIMEOUT_STOP_SEC: u64 = 90;

// The pure helpers below run on unix in production but are exercised by the
// unit tests on every platform, hence the windows dead_code allowance.
#[derive(Debug, Clone)]
#[cfg_attr(windows, allow(dead_code))]
pub struct ServiceOptions {
    /// systemd unit name (without the `.service` suffix).
    pub unit_name: String,
    /// Optional `User=` the service runs as.
    pub user: Option<String>,
    /// Overwrite an existing unit with different content.
    pub force: bool,
    /// `systemctl start` right after install.
    pub now: bool,
}

// -- platform-independent pure logic ----------------------------------------

/// systemd unit names we accept: ASCII alphanumerics plus `.` `_` `-`,
/// non-empty, not ending in `.`.
#[cfg_attr(windows, allow(dead_code))]
pub fn is_valid_unit_name(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Quote an ExecStart argument when it contains whitespace, systemd-style.
#[cfg_attr(windows, allow(dead_code))]
fn quote_exec_arg(s: &str) -> String {
    if s.is_empty() || s.chars().any(|c| c.is_whitespace()) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Render the unit file. `timeout_stop_sec` should already account for the
/// slowest configured graceful stop (see `compute_timeout_stop_sec`).
#[cfg_attr(windows, allow(dead_code))]
pub fn render_unit(
    exe: &Path,
    core_config: &Path,
    user: Option<&str>,
    timeout_stop_sec: u64,
) -> String {
    let mut s = String::new();
    s.push_str("[Unit]\n");
    s.push_str("Description=xkeeper process keeper\n");
    s.push_str("After=network.target\n");
    s.push('\n');
    s.push_str("[Service]\n");
    s.push_str(&format!(
        "ExecStart={} run -c {}\n",
        quote_exec_arg(&exe.display().to_string()),
        quote_exec_arg(&core_config.display().to_string())
    ));
    s.push_str("Restart=always\n");
    s.push_str("RestartSec=3\n");
    s.push_str("KillSignal=SIGTERM\n");
    s.push_str(&format!("TimeoutStopSec={timeout_stop_sec}\n"));
    if let Some(u) = user {
        s.push_str(&format!("User={u}\n"));
    }
    s.push('\n');
    s.push_str("[Install]\n");
    s.push_str("WantedBy=multi-user.target\n");
    s
}

/// What to do with an existing unit file before writing.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))]
pub enum WriteDecision {
    /// No existing unit — write the rendered one.
    Write,
    /// Existing unit is byte-identical — skip the write (still reload+enable).
    SkipIdentical,
    /// Existing unit differs and `--force` was not given.
    RefuseNeedsForce,
}

#[cfg_attr(windows, allow(dead_code))]
pub fn decide_write(existing: Option<&str>, rendered: &str, force: bool) -> WriteDecision {
    match existing {
        None => WriteDecision::Write,
        Some(cur) if cur == rendered => WriteDecision::SkipIdentical,
        Some(_) if force => WriteDecision::Write,
        Some(_) => WriteDecision::RefuseNeedsForce,
    }
}

/// Stop budget from the slowest program: 2× max stop_timeout + 10s margin.
#[cfg_attr(windows, allow(dead_code))]
fn timeout_from_max_stop(max_stop_timeout: f64) -> u64 {
    (2.0 * max_stop_timeout + 10.0).ceil() as u64
}

/// Try to load the core config and every registered app and derive
/// `TimeoutStopSec` from the largest `stop_timeout`. Any load failure falls
/// back to a conservative 90s.
#[cfg_attr(windows, allow(dead_code))]
pub fn compute_timeout_stop_sec(core_path: &Path) -> u64 {
    let compute = || -> Result<u64> {
        let (core, _) = crate::config::CoreConfig::load_or_default(core_path)?;
        let core_dir = core_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let apps = crate::registry::list(&core, core_dir)?;
        let mut max = 0.0f64;
        for listed in apps {
            let (raw, _) = crate::config::AppRaw::load(&listed.path)?;
            let app = crate::config::resolve_app(
                &listed.name,
                &listed.path,
                &raw,
                core.app_default.as_ref(),
            )?;
            for p in &app.programs {
                max = max.max(p.stop_timeout);
            }
        }
        Ok(timeout_from_max_stop(max))
    };
    compute().unwrap_or(FALLBACK_TIMEOUT_STOP_SEC)
}

// -- platform entry points ---------------------------------------------------

pub fn install(core_path: &Path, opts: &ServiceOptions) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = (core_path, opts);
        bail!("service registration is not supported on Windows yet");
    }
    #[cfg(unix)]
    {
        unix_install(core_path, opts)
    }
}

pub fn uninstall(_core_path: &Path, opts: &ServiceOptions) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = opts;
        bail!("service registration is not supported on Windows yet");
    }
    #[cfg(unix)]
    {
        unix_uninstall(opts)
    }
}

// -- unix (systemd) implementation -------------------------------------------

#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use anyhow::Context as _;

#[cfg(unix)]
const SYSTEMD_UNIT_DIR: &str = "/etc/systemd/system";

#[cfg(unix)]
fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("service install/uninstall requires root (try sudo)");
    }
    Ok(())
}

#[cfg(unix)]
fn unit_path(unit_name: &str) -> PathBuf {
    Path::new(SYSTEMD_UNIT_DIR).join(format!("{unit_name}.service"))
}

#[cfg(unix)]
fn unix_install(core_path: &Path, opts: &ServiceOptions) -> Result<()> {
    require_root()?;
    if !is_valid_unit_name(&opts.unit_name) {
        bail!(
            "invalid unit name {:?} (allowed: letters, digits, '.', '_', '-'; must not end with '.')",
            opts.unit_name
        );
    }

    let exe = std::env::current_exe()
        .context("cannot locate the current executable")?
        .canonicalize()
        .context("cannot resolve the current executable path")?;
    let core_abs = std::path::absolute(core_path)
        .with_context(|| format!("cannot resolve core config path {}", core_path.display()))?;
    let rendered = render_unit(
        &exe,
        &core_abs,
        opts.user.as_deref(),
        compute_timeout_stop_sec(core_path),
    );

    let path = unit_path(&opts.unit_name);
    let existing = std::fs::read_to_string(&path).ok();
    match decide_write(existing.as_deref(), &rendered, opts.force) {
        WriteDecision::RefuseNeedsForce => bail!(
            "unit {} already exists with different content; use --force to overwrite",
            path.display()
        ),
        WriteDecision::SkipIdentical => {
            println!("unit {} already up to date", path.display());
        }
        WriteDecision::Write => {
            std::fs::write(&path, &rendered)
                .with_context(|| format!("cannot write {}", path.display()))?;
            println!("unit written: {}", path.display());
        }
    }

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", &format!("{}.service", opts.unit_name)])?;
    if opts.now {
        systemctl(&["start", &format!("{}.service", opts.unit_name)])?;
        println!("service {} started", opts.unit_name);
    }
    Ok(())
}

#[cfg(unix)]
fn unix_uninstall(opts: &ServiceOptions) -> Result<()> {
    require_root()?;
    if !is_valid_unit_name(&opts.unit_name) {
        bail!(
            "invalid unit name {:?} (allowed: letters, digits, '.', '_', '-'; must not end with '.')",
            opts.unit_name
        );
    }

    let path = unit_path(&opts.unit_name);
    if !path.exists() {
        println!("service {} is not installed", opts.unit_name);
        return Ok(());
    }

    // stop and disable failures are non-fatal: the goal is a clean removal.
    if let Err(e) = systemctl(&["stop", &format!("{}.service", opts.unit_name)]) {
        eprintln!("xkeeper: warning: {e:#}");
    }
    if let Err(e) = systemctl(&["disable", &format!("{}.service", opts.unit_name)]) {
        eprintln!("xkeeper: warning: {e:#}");
    }
    std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    println!("unit removed: {}", path.display());
    systemctl(&["daemon-reload"])?;
    Ok(())
}

#[cfg(unix)]
fn systemctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to run systemctl {}", args.join(" ")))?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

// -- tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xk-svc-{tag}-{}", std::process::id()))
    }

    #[test]
    fn unit_name_accepts_legal_and_rejects_illegal() {
        assert!(is_valid_unit_name("xkeeper"));
        assert!(is_valid_unit_name("xk-prod_1.service.test"));
        assert!(is_valid_unit_name("a"));
        for bad in ["", "a/b", "a b", "a\\b", "ends.", "ä"] {
            assert!(!is_valid_unit_name(bad), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn renders_unit_without_user() {
        let unit = render_unit(
            Path::new("/usr/local/bin/xkeeper"),
            Path::new("/etc/xkeeper.toml"),
            None,
            90,
        );
        assert_eq!(
            unit,
            "[Unit]\n\
             Description=xkeeper process keeper\n\
             After=network.target\n\
             \n\
             [Service]\n\
             ExecStart=/usr/local/bin/xkeeper run -c /etc/xkeeper.toml\n\
             Restart=always\n\
             RestartSec=3\n\
             KillSignal=SIGTERM\n\
             TimeoutStopSec=90\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n"
        );
    }

    #[test]
    fn renders_unit_with_user_and_quoted_paths() {
        let unit = render_unit(
            Path::new("/opt/my tools/xkeeper"),
            Path::new("/etc/my conf/xkeeper.toml"),
            Some("svc-xk"),
            60,
        );
        assert!(unit.contains("ExecStart=\"/opt/my tools/xkeeper\" run -c \"/etc/my conf/xkeeper.toml\"\n"));
        assert!(unit.contains("User=svc-xk\n"));
        assert!(unit.contains("TimeoutStopSec=60\n"));
    }

    #[test]
    fn write_decision_idempotent_refuse_and_force() {
        let rendered = "unit\n";
        assert_eq!(decide_write(None, rendered, false), WriteDecision::Write);
        // identical content is idempotent regardless of --force
        assert_eq!(
            decide_write(Some(rendered), rendered, false),
            WriteDecision::SkipIdentical
        );
        assert_eq!(
            decide_write(Some(rendered), rendered, true),
            WriteDecision::SkipIdentical
        );
        assert_eq!(
            decide_write(Some("other\n"), rendered, false),
            WriteDecision::RefuseNeedsForce
        );
        assert_eq!(decide_write(Some("other\n"), rendered, true), WriteDecision::Write);
    }

    #[test]
    fn timeout_scales_with_max_stop_timeout() {
        assert_eq!(timeout_from_max_stop(0.0), 10);
        assert_eq!(timeout_from_max_stop(25.0), 60);
        assert_eq!(timeout_from_max_stop(12.25), 35); // ceil(34.5)
    }

    #[test]
    fn timeout_falls_back_when_config_unloadable() {
        let dir = temp_dir("fallback");
        std::fs::create_dir_all(&dir).unwrap();
        let core = dir.join("xkeeper.toml");
        std::fs::write(&core, "not [valid toml").unwrap();
        assert_eq!(compute_timeout_stop_sec(&core), FALLBACK_TIMEOUT_STOP_SEC);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn timeout_computed_from_registered_apps() {
        let dir = temp_dir("apps");
        let apps = dir.join("apps");
        std::fs::create_dir_all(&apps).unwrap();
        let core = dir.join("xkeeper.toml");
        std::fs::write(&core, "[daemon]\napp_dir = \"apps\"\n").unwrap();
        std::fs::write(
            apps.join("demo.toml"),
            "[program.api]\ncommand = \"python -m http.server 8000\"\nstop_timeout = 25\n",
        )
        .unwrap();
        assert_eq!(compute_timeout_stop_sec(&core), 60);
        std::fs::remove_dir_all(&dir).ok();
    }
}
