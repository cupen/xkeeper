//! systemd service registration: `xkeeper service install|uninstall`.
//!
//! Linux only — writes a unit file (default `/etc/systemd/system/xkeeper.service`,
//! override with `--unit-file`), then drives `systemctl` to reload/enable/start
//! it. Windows builds keep the subcommands discoverable but fail at runtime with
//! a clear error. All template rendering, path validation and write decisions
//! are platform-independent pure functions so they are testable everywhere.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Default `--unit-file` value.
#[cfg_attr(windows, allow(dead_code))]
pub const DEFAULT_UNIT_FILE: &str = "/etc/systemd/system/xkeeper.service";
/// Fallback stop budget when the config cannot be loaded.
#[cfg_attr(windows, allow(dead_code))]
const FALLBACK_TIMEOUT_STOP_SEC: u64 = 90;

// The pure helpers below run on unix in production but are exercised by the
// unit tests on every platform, hence the windows dead_code allowance.
#[derive(Debug, Clone)]
#[cfg_attr(windows, allow(dead_code))]
pub struct ServiceOptions {
    /// Full path of the unit file to write/remove (must end in `.service`).
    pub unit_file: PathBuf,
    /// Optional `User=` the service runs as.
    pub user: Option<String>,
    /// Overwrite an existing unit with different content.
    pub force: bool,
    /// `systemctl start` right after install.
    pub now: bool,
}

// -- platform-independent pure logic ----------------------------------------

/// systemd unit file paths we accept: file name is a legal unit name
/// (ASCII alphanumerics plus `.` `_` `-`, non-empty, not ending in `.`)
/// with a mandatory `.service` extension.
#[cfg_attr(windows, allow(dead_code))]
pub fn is_valid_unit_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".service") else {
        return false;
    };
    is_valid_unit_name(stem)
}

/// systemd unit names we accept: ASCII alphanumerics plus `.` `_` `-`,
/// non-empty, not ending in `.`.
#[cfg_attr(windows, allow(dead_code))]
fn is_valid_unit_name(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Unit name (without `.service`) derived from a validated unit file path.
#[cfg_attr(windows, allow(dead_code))]
fn unit_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".service"))
        .unwrap_or_default()
        .to_string()
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
    config_path: &Path,
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
        "ExecStart={} run --config {}\n",
        quote_exec_arg(&exe.display().to_string()),
        quote_exec_arg(&config_path.display().to_string())
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

/// Pretty-render a written unit file inside a rounded box for the terminal.
/// `color` enables ANSI styling (section headers cyan, keys bold, borders
/// dim); padding is computed on visible characters so colored output stays
/// aligned. Pure so it is testable on every platform.
#[cfg_attr(windows, allow(dead_code))]
pub fn render_unit_display(path: &str, unit: &str, color: bool) -> String {
    // colorize one unit-file line; the caller pads the raw line first so
    // escape sequences never skew the box width
    fn paint(line: &str, color: bool) -> String {
        if !color {
            return line.to_string();
        }
        let t = line.trim_start();
        if t.starts_with('[') && t.ends_with(']') {
            format!("\x1b[1;36m{line}\x1b[0m")
        } else if t.starts_with('#') || t.starts_with(';') {
            format!("\x1b[90m{line}\x1b[0m")
        } else if let Some(eq) = line.find('=') {
            format!("\x1b[1m{}\x1b[0m{}", &line[..=eq], &line[eq + 1..])
        } else {
            line.to_string()
        }
    }

    let lines: Vec<&str> = unit.lines().collect();
    let title_w = path.chars().count();
    let content_w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    // title + 2 keeps at least one filler dash on each side of the title
    let w = content_w.max(title_w + 2);
    let dash = "─".repeat(w - title_w - 1);
    let rule = "─".repeat(w + 2);

    let mut out = String::new();
    if color {
        out.push_str(&format!(
            "\x1b[2m╭─\x1b[0m \x1b[1m{path}\x1b[0m \x1b[2m{dash}╮\x1b[0m\n"
        ));
    } else {
        out.push_str(&format!("╭─ {path} {dash}╮\n"));
    }
    for line in &lines {
        let pad = " ".repeat(w - line.chars().count());
        let body = paint(line, color);
        if color {
            out.push_str(&format!("\x1b[2m│\x1b[0m {body}{pad} \x1b[2m│\x1b[0m\n"));
        } else {
            out.push_str(&format!("│ {body}{pad} │\n"));
        }
    }
    if color {
        out.push_str(&format!("\x1b[2m╰{rule}╯\x1b[0m"));
    } else {
        out.push_str(&format!("╰{rule}╯"));
    }
    out
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

/// Try to load the daemon config and every registered app and derive
/// `TimeoutStopSec` from the largest `stop_timeout`. Any load failure falls
/// back to a conservative 90s.
#[cfg_attr(windows, allow(dead_code))]
pub fn compute_timeout_stop_sec(config_path: &Path) -> u64 {
    let compute = || -> Result<u64> {
        let (config, _) = crate::config::DaemonConfig::load_or_default(config_path)?;
        let config_dir = config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let apps = crate::registry::ListedApp::good(crate::registry::list(&config, config_dir)?);
        let mut max = 0.0f64;
        for listed in apps {
            let (raw, _) = crate::config::AppRaw::load(&listed.path)?;
            let app = crate::config::resolve_app(
                &listed.name,
                &listed.path,
                &raw,
                config.app_default.as_ref(),
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

pub fn install(config_path: &Path, opts: &ServiceOptions) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = (config_path, opts);
        bail!("service registration is not supported on Windows yet");
    }
    #[cfg(unix)]
    {
        unix_install(config_path, opts)
    }
}

pub fn uninstall(_config_path: &Path, opts: &ServiceOptions) -> Result<()> {
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
use anyhow::Context as _;

#[cfg(unix)]
fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("service install/uninstall requires root (try sudo)");
    }
    Ok(())
}

/// Color the unit box only when stdout is a real terminal and the user has
/// not opted out via `NO_COLOR`.
#[cfg(unix)]
fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1
}

#[cfg(unix)]
fn unix_install(config_path: &Path, opts: &ServiceOptions) -> Result<()> {
    if !is_valid_unit_file(&opts.unit_file) {
        bail!(
            "invalid unit file {} (file name must be a legal unit name ending in '.service': \
             letters, digits, '.', '_', '-'; must not end with '.')",
            opts.unit_file.display()
        );
    }
    require_root()?;
    let unit_name = unit_name_of(&opts.unit_file);

    let exe = std::env::current_exe()
        .context("cannot locate the current executable")?
        .canonicalize()
        .context("cannot resolve the current executable path")?;
    let config_abs = std::path::absolute(config_path).with_context(|| {
        format!(
            "cannot resolve daemon config path {}",
            config_path.display()
        )
    })?;
    let rendered = render_unit(
        &exe,
        &config_abs,
        opts.user.as_deref(),
        compute_timeout_stop_sec(config_path),
    );

    let path = &opts.unit_file;
    let existing = std::fs::read_to_string(path).ok();
    match decide_write(existing.as_deref(), &rendered, opts.force) {
        WriteDecision::RefuseNeedsForce => bail!(
            "unit {} already exists with different content; use --force to overwrite",
            path.display()
        ),
        WriteDecision::SkipIdentical => {
            println!("unit {} already up to date", path.display());
        }
        WriteDecision::Write => {
            std::fs::write(path, &rendered)
                .with_context(|| format!("cannot write {}", path.display()))?;
            println!("unit written: {}", path.display());
        }
    }

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", &format!("{unit_name}.service")])?;
    if opts.now {
        systemctl(&["start", &format!("{unit_name}.service")])?;
        println!("service {unit_name} started");
    }
    // only reached on a fully successful install — show what landed on disk
    println!(
        "{}",
        render_unit_display(&path.display().to_string(), &rendered, use_color())
    );
    Ok(())
}

#[cfg(unix)]
fn unix_uninstall(opts: &ServiceOptions) -> Result<()> {
    if !is_valid_unit_file(&opts.unit_file) {
        bail!(
            "invalid unit file {} (file name must be a legal unit name ending in '.service': \
             letters, digits, '.', '_', '-'; must not end with '.')",
            opts.unit_file.display()
        );
    }
    require_root()?;
    let unit_name = unit_name_of(&opts.unit_file);

    let path = &opts.unit_file;
    if !path.exists() {
        println!("service {unit_name} is not installed");
        return Ok(());
    }

    // stop and disable failures are non-fatal: the goal is a clean removal.
    if let Err(e) = systemctl(&["stop", &format!("{unit_name}.service")]) {
        eprintln!("xkeeper: warning: {e:#}");
    }
    if let Err(e) = systemctl(&["disable", &format!("{unit_name}.service")]) {
        eprintln!("xkeeper: warning: {e:#}");
    }
    std::fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()))?;
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
    fn unit_file_accepts_legal_and_rejects_illegal() {
        assert!(is_valid_unit_file(Path::new("/etc/systemd/system/xkeeper.service")));
        assert!(is_valid_unit_file(Path::new("xk-prod_1.service")));
        assert!(is_valid_unit_file(Path::new("a.service")));
        assert!(is_valid_unit_file(Path::new("my-app_v2.service")));
        for bad in [
            "xkeeper",             // missing .service
            ".service",            // empty stem
            "ends..service",       // stem ends with '.'
            "my app.service",      // space in stem
            "x.service.bak",       // wrong extension
            "/etc/systemd/system/", // directory, no file name
        ] {
            assert!(
                !is_valid_unit_file(Path::new(bad)),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn unit_name_derived_from_file_stem() {
        assert_eq!(unit_name_of(Path::new("/etc/systemd/system/xk.service")), "xk");
        assert_eq!(unit_name_of(Path::new("my-app.service")), "my-app");
    }

    #[test]
    fn renders_unit_without_user() {
        let unit = render_unit(
            Path::new("/usr/local/bin/xkeeper"),
            Path::new("/etc/xkeeper/daemon.toml"),
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
             ExecStart=/usr/local/bin/xkeeper run --config /etc/xkeeper/daemon.toml\n\
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
        assert!(unit.contains(
            "ExecStart=\"/opt/my tools/xkeeper\" run --config \"/etc/my conf/xkeeper.toml\"\n"
        ));
        assert!(unit.contains("User=svc-xk\n"));
        assert!(unit.contains("TimeoutStopSec=60\n"));
    }

    #[test]
    fn unit_display_plain_box_frames_and_aligns() {
        let out = render_unit_display("/x.service", "[Unit]\nDescription=demo\n", false);
        assert_eq!(
            out,
            "╭─ /x.service ─────╮\n\
             │ [Unit]           │\n\
             │ Description=demo │\n\
             ╰──────────────────╯"
        );
    }

    #[test]
    fn unit_display_color_highlights_and_stays_aligned() {
        fn strip_ansi(s: &str) -> String {
            let mut out = String::new();
            let mut it = s.chars();
            while let Some(c) = it.next() {
                if c == '\x1b' {
                    for e in it.by_ref() {
                        if e == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }
        let unit = render_unit(
            Path::new("/usr/local/bin/xkeeper"),
            Path::new("/etc/xkeeper/daemon.toml"),
            None,
            90,
        );
        let out = render_unit_display("/etc/systemd/system/xkeeper.service", &unit, true);
        assert!(out.contains("\x1b[1;36m[Unit]\x1b[0m"));
        assert!(out.contains("\x1b[1mExecStart=\x1b[0m"));
        assert!(out.starts_with("\x1b[2m╭─"));
        assert!(out.contains("\x1b[2m╰"));
        // escapes must not skew the box: every line has the same visible width
        let widths: std::collections::HashSet<usize> =
            out.lines().map(|l| strip_ansi(l).chars().count()).collect();
        assert_eq!(widths.len(), 1, "uneven box lines: {widths:?}");
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
        assert_eq!(
            decide_write(Some("other\n"), rendered, true),
            WriteDecision::Write
        );
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
        let config = dir.join("xkeeper.toml");
        std::fs::write(&config, "not [valid toml").unwrap();
        assert_eq!(compute_timeout_stop_sec(&config), FALLBACK_TIMEOUT_STOP_SEC);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn timeout_computed_from_registered_apps() {
        let dir = temp_dir("apps");
        let apps = dir.join("apps");
        std::fs::create_dir_all(&apps).unwrap();
        let config = dir.join("xkeeper.toml");
        std::fs::write(&config, "[daemon]\napp_dir = \"apps\"\n").unwrap();
        std::fs::write(
            apps.join("demo.toml"),
            "[program.api]\ncommand = \"python -m http.server 8000\"\nstop_timeout = 25\n",
        )
        .unwrap();
        assert_eq!(compute_timeout_stop_sec(&config), 60);
        std::fs::remove_dir_all(&dir).ok();
    }
}
