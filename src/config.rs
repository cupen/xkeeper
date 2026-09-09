//! Configuration: the unique global daemon config plus per-app deployment
//! config files (`xkeeper.toml`), with layered defaults resolution.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Resolve `p` against `base` unless it is absolute (or empty).
pub fn resolve_path(p: &Path, base: &Path) -> PathBuf {
    if p.is_absolute() || p.as_os_str().is_empty() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Names become link file names and log file names, so they must be safe.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.chars().any(|c| {
            matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
        })
}

/// Split a command line into argv using shell word rules (whitespace split,
/// quote awareness). We never invoke a shell — no pipes/variables/redirects.
pub fn split_command(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in line.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                cur.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse human-friendly sizes: "10MB", "512KB", "1GB" or plain bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let (num, mult) = if let Some(n) = t.strip_suffix("GB") {
        (n, 1024u64.pow(3))
    } else if let Some(n) = t.strip_suffix("MB") {
        (n, 1024u64.pow(2))
    } else if let Some(n) = t.strip_suffix("KB") {
        (n, 1024)
    } else {
        (t, 1)
    };
    let n: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid size {s:?}"))?;
    Ok(n * mult)
}

// ---------------------------------------------------------------------------
// restart policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    #[serde(rename = "always")]
    Always,
    #[serde(rename = "on-failure")]
    OnFailure,
    #[serde(rename = "never")]
    Never,
}

// ---------------------------------------------------------------------------
// daemon config (global only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(default)]
    pub daemon: DaemonSettings,
    #[serde(default, rename = "app-default")]
    pub app_default: Option<AppDefaults>,
}

/// Built-in rotation defaults: 50MB per file, 2 rotated files kept
/// (3 files on disk per stream including the live one).
const DEFAULT_LOG_MAX_SIZE: u64 = 50 * 1024 * 1024;
const DEFAULT_LOG_ROTATE_KEEP: u32 = 2;

/// Fixed absolute default so the log location never depends on where the
/// daemon config lives. `/tmp` is volatile by design (see README); pin an
/// absolute `log_dir` (e.g. `/var/log/xkeeper`) for logs that survive reboots.
fn default_log_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::temp_dir().join("xkeeper").join("logs")
    } else {
        PathBuf::from("/tmp/xkeeper/logs")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonSettings {
    pub log_level: String,
    pub log_dir: PathBuf,
    pub monitor_interval: f64,
    pub host: String,
    pub port: u16,
    pub auth_token: String,
    pub log_buffer_lines: usize,
    pub app_dir: PathBuf,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            log_dir: default_log_dir(),
            monitor_interval: 1.0,
            host: "127.0.0.1".into(),
            port: 7310,
            auth_token: String::new(),
            log_buffer_lines: 1000,
            app_dir: PathBuf::from("apps"),
        }
    }
}

impl DaemonConfig {
    /// Load the daemon config; a missing file means "run with defaults".
    pub fn load_or_default(path: &Path) -> Result<(DaemonConfig, bool)> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let cfg: DaemonConfig = toml::from_str(&text).with_context(|| {
                    format!("failed to parse daemon config: {}", path.display())
                })?;
                cfg.validate()?;
                Ok((cfg, true))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok((DaemonConfig::empty(), false))
            }
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn empty() -> Self {
        DaemonConfig {
            daemon: DaemonSettings::default(),
            app_default: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();
        let d = &self.daemon;
        if !matches!(
            d.log_level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            errors.push(format!("daemon.log_level: unknown level {:?}", d.log_level));
        }
        if d.log_dir.as_os_str().is_empty() {
            errors.push("daemon.log_dir: must not be empty".to_string());
        } else if !d.log_dir.is_absolute() {
            errors.push(format!(
                "daemon.log_dir: {:?} must be an absolute path (e.g. log_dir = \"/var/log/xkeeper\")",
                d.log_dir
            ));
        }
        if d.monitor_interval <= 0.0 || d.monitor_interval > 60.0 {
            errors.push(format!(
                "daemon.monitor_interval: {} out of range (0, 60]",
                d.monitor_interval
            ));
        }
        if d.port == 0 {
            errors.push("daemon.port: must be > 0".to_string());
        }
        if d.log_buffer_lines == 0 {
            errors.push("daemon.log_buffer_lines: must be > 0".to_string());
        }
        if let Some(def) = &self.app_default {
            check_level_defaults("[app-default]", def, &mut errors);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(
                "daemon config validation failed:\n  - {}",
                errors.join("\n  - ")
            );
        }
    }
}

// ---------------------------------------------------------------------------
// app-level default fields (used by daemon [app-default] and app [app])
// ---------------------------------------------------------------------------

/// Optional program-level knobs shared across layers. Each layer may set any
/// subset; resolution picks the highest layer that sets a field.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LevelOptions {
    pub autostart: Option<bool>,
    pub priority: Option<i32>,
    pub autorestart: Option<RestartPolicy>,
    pub exit_codes: Option<Vec<i32>>,
    pub restart_backoff: Option<f64>,
    pub max_restart_backoff: Option<f64>,
    pub max_restarts: Option<u32>,
    pub backoff_reset_after: Option<f64>,
    pub startsecs: Option<f64>,
    pub startretries: Option<u32>,
    pub stop_timeout: Option<f64>,
    pub restart_on_unhealthy: Option<bool>,
    pub log_max_size: Option<String>,
    pub log_rotate_keep: Option<u32>,
}

/// Defaults shared by every app, from the daemon config `[app-default]` table.
pub type AppDefaults = LevelOptions;

/// The `[app]` table of an app config: metadata plus app-level defaults.
/// This is where `xkeeper add` micro-tuning flags are written.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppMeta {
    pub description: Option<String>,
    pub autostart: Option<bool>,
    pub priority: Option<i32>,
    pub autorestart: Option<RestartPolicy>,
    pub exit_codes: Option<Vec<i32>>,
    pub restart_backoff: Option<f64>,
    pub max_restart_backoff: Option<f64>,
    pub max_restarts: Option<u32>,
    pub backoff_reset_after: Option<f64>,
    pub startsecs: Option<f64>,
    pub startretries: Option<u32>,
    pub stop_timeout: Option<f64>,
    pub restart_on_unhealthy: Option<bool>,
    pub log_max_size: Option<String>,
    pub log_rotate_keep: Option<u32>,
}

fn check_level_defaults(label: &str, l: &AppDefaults, errors: &mut Vec<String>) {
    if let Some(b) = l.restart_backoff {
        if b <= 0.0 {
            errors.push(format!("{label}.restart_backoff must be > 0"));
        }
    }
    if let (Some(mx), Some(mn)) = (l.max_restart_backoff, l.restart_backoff) {
        if mx < mn {
            errors.push(format!(
                "{label}.max_restart_backoff must be >= restart_backoff"
            ));
        }
    }
    if let Some(t) = l.stop_timeout {
        if t < 0.0 {
            errors.push(format!("{label}.stop_timeout must be >= 0"));
        }
    }
    if let Some(s) = l.startsecs {
        if s < 0.0 {
            errors.push(format!("{label}.startsecs must be >= 0"));
        }
    }
    if let Some(b) = l.backoff_reset_after {
        if b < 0.0 {
            errors.push(format!("{label}.backoff_reset_after must be >= 0"));
        }
    }
}

// ---------------------------------------------------------------------------
// app config file
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppRaw {
    #[serde(default)]
    pub app: Option<AppMeta>,
    #[serde(default)]
    pub program: BTreeMap<String, ProgramRaw>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProgramRaw {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub work_dir: Option<PathBuf>,
    pub env: Option<BTreeMap<String, String>>,
    pub depends_on: Option<Vec<String>>,
    pub autorestart: Option<RestartPolicy>,
    pub exit_codes: Option<Vec<i32>>,
    pub restart_backoff: Option<f64>,
    pub max_restart_backoff: Option<f64>,
    pub max_restarts: Option<u32>,
    pub backoff_reset_after: Option<f64>,
    pub startsecs: Option<f64>,
    pub startretries: Option<u32>,
    pub stop_timeout: Option<f64>,
    pub restart_on_unhealthy: Option<bool>,
    pub log_max_size: Option<String>,
    pub log_rotate_keep: Option<u32>,
    pub health_check: Option<String>,
    pub health_interval: Option<f64>,
    pub health_timeout: Option<f64>,
    pub health_retries: Option<u32>,
    pub health_start_period: Option<f64>,
}

impl AppRaw {
    pub fn load(path: &Path) -> Result<(AppRaw, PathBuf)> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read app config: {}", path.display()))?;
        let raw: AppRaw = toml::from_str(&text)
            .with_context(|| format!("failed to parse app config: {}", path.display()))?;
        if raw.program.is_empty() {
            bail!("app config {} has no [program.*] tables", path.display());
        }
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok((raw, dir))
    }
}

// ---------------------------------------------------------------------------
// resolved model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct HealthCheck {
    pub kind: HealthKind,
    pub interval: f64,
    pub timeout: f64,
    pub retries: u32,
    pub start_period: f64,
}

#[derive(Debug, Clone, Serialize)]
pub enum HealthKind {
    Http { url: String },
    Tcp { addr: String },
    Exec { command: String, args: Vec<String> },
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedProgram {
    pub app: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub work_dir: PathBuf,
    pub env: BTreeMap<String, String>,
    pub depends_on: Vec<String>,
    pub autorestart: RestartPolicy,
    pub exit_codes: Vec<i32>,
    pub restart_backoff: f64,
    pub max_restart_backoff: f64,
    pub max_restarts: u32,
    pub backoff_reset_after: f64,
    pub startsecs: f64,
    pub startretries: u32,
    pub stop_timeout: f64,
    pub restart_on_unhealthy: bool,
    pub log_max_size: Option<u64>,
    pub log_rotate_keep: u32,
    pub health: Option<HealthCheck>,
    /// Definition hash, used by reload to detect changes.
    #[serde(skip)]
    pub hash: u64,
}

#[derive(Debug, Clone)]
pub struct ResolvedApp {
    pub name: String,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub description: Option<String>,
    pub autostart: bool,
    pub priority: i32,
    pub programs: Vec<ResolvedProgram>,
}

fn parse_health(s: &str, raw: &ProgramRaw) -> Result<HealthCheck> {
    let kind = if s.starts_with("http://") || s.starts_with("https://") {
        HealthKind::Http { url: s.to_string() }
    } else if let Some(addr) = s.strip_prefix("tcp://") {
        if addr
            .rsplit_once(':')
            .map(|(_, p)| p.parse::<u16>())
            .transpose()?
            .is_none()
        {
            bail!("health_check tcp address must be host:port, got {s:?}");
        }
        HealthKind::Tcp {
            addr: addr.to_string(),
        }
    } else {
        let mut parts = split_command(s);
        if parts.is_empty() {
            bail!("health_check is empty");
        }
        let command = parts.remove(0);
        HealthKind::Exec {
            command,
            args: parts,
        }
    };
    Ok(HealthCheck {
        kind,
        interval: raw.health_interval.unwrap_or(10.0),
        timeout: raw.health_timeout.unwrap_or(2.0),
        retries: raw.health_retries.unwrap_or(3),
        start_period: raw.health_start_period.unwrap_or(5.0),
    })
}

fn def_hash(p: &ResolvedProgram) -> u64 {
    use std::hash::{Hash, Hasher};
    let json = serde_json::to_string(p).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    json.hash(&mut h);
    h.finish()
}

/// Resolve one `[program.<name>]` entry against the `[app]` layer and the
/// daemon `[app-default]` layer (highest priority first).
pub fn resolve_program(
    app_name: &str,
    prog_name: &str,
    raw: &ProgramRaw,
    meta: Option<&AppMeta>,
    defaults: Option<&AppDefaults>,
    base_dir: &Path,
) -> Result<ResolvedProgram> {
    if !is_valid_name(prog_name) {
        bail!("program name {prog_name:?} is not filename-safe");
    }
    let command_raw = raw
        .command
        .clone()
        .with_context(|| format!("program[{prog_name}]: command is required"))?;
    let (command, args) = match (&raw.args, command_raw.contains(char::is_whitespace)) {
        (Some(_), true) => bail!(
            "program[{prog_name}]: single-line command and explicit args are mutually exclusive"
        ),
        (Some(a), false) => (command_raw, a.clone()),
        (None, true) => {
            let mut parts = split_command(&command_raw);
            if parts.is_empty() {
                bail!("program[{prog_name}]: command is empty");
            }
            let c = parts.remove(0);
            (c, parts)
        }
        (None, false) => (command_raw, vec![]),
    };

    let autorestart = raw
        .autorestart
        .or(meta.as_ref().and_then(|m| m.autorestart))
        .or(defaults.and_then(|d| d.autorestart))
        .unwrap_or(RestartPolicy::Always);
    let exit_codes = raw
        .exit_codes
        .clone()
        .or(meta.as_ref().and_then(|m| m.exit_codes.clone()))
        .or(defaults.and_then(|d| d.exit_codes.clone()))
        .unwrap_or(vec![0]);
    let restart_backoff = raw
        .restart_backoff
        .or(meta.as_ref().and_then(|m| m.restart_backoff))
        .or(defaults.and_then(|d| d.restart_backoff))
        .unwrap_or(1.0);
    let max_restart_backoff = raw
        .max_restart_backoff
        .or(meta.as_ref().and_then(|m| m.max_restart_backoff))
        .or(defaults.and_then(|d| d.max_restart_backoff))
        .unwrap_or(30.0)
        .max(restart_backoff);
    let max_restarts = raw
        .max_restarts
        .or(meta.as_ref().and_then(|m| m.max_restarts))
        .or(defaults.and_then(|d| d.max_restarts))
        .unwrap_or(0);
    let backoff_reset_after = raw
        .backoff_reset_after
        .or(meta.as_ref().and_then(|m| m.backoff_reset_after))
        .or(defaults.and_then(|d| d.backoff_reset_after))
        .unwrap_or(60.0);
    let startsecs = raw
        .startsecs
        .or(meta.as_ref().and_then(|m| m.startsecs))
        .or(defaults.and_then(|d| d.startsecs))
        .unwrap_or(1.0);
    let startretries = raw
        .startretries
        .or(meta.as_ref().and_then(|m| m.startretries))
        .or(defaults.and_then(|d| d.startretries))
        .unwrap_or(3);
    let stop_timeout = raw
        .stop_timeout
        .or(meta.as_ref().and_then(|m| m.stop_timeout))
        .or(defaults.and_then(|d| d.stop_timeout))
        .unwrap_or(10.0);
    let restart_on_unhealthy = raw
        .restart_on_unhealthy
        .or(meta.as_ref().and_then(|m| m.restart_on_unhealthy))
        .or(defaults.and_then(|d| d.restart_on_unhealthy))
        .unwrap_or(false);
    let log_max_size = raw
        .log_max_size
        .clone()
        .or(meta.as_ref().and_then(|m| m.log_max_size.clone()))
        .or(defaults.and_then(|d| d.log_max_size.clone()))
        .map(|s| parse_size(&s))
        .transpose()?;
    // Rotation is on by default; "0" is the explicit opt-out (max_size None).
    let log_max_size = match log_max_size {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(DEFAULT_LOG_MAX_SIZE),
    };
    let log_rotate_keep = raw
        .log_rotate_keep
        .or(meta.as_ref().and_then(|m| m.log_rotate_keep))
        .or(defaults.and_then(|d| d.log_rotate_keep))
        .unwrap_or(DEFAULT_LOG_ROTATE_KEEP);

    if restart_backoff <= 0.0 {
        bail!("program[{prog_name}]: restart_backoff must be > 0");
    }
    if stop_timeout < 0.0 || startsecs < 0.0 || backoff_reset_after < 0.0 {
        bail!("program[{prog_name}]: negative timeout values are not allowed");
    }

    let health = match &raw.health_check {
        Some(s) => Some(
            parse_health(s, raw)
                .with_context(|| format!("program[{prog_name}]: invalid health_check"))?,
        ),
        None => None,
    };

    let work_dir = match &raw.work_dir {
        Some(w) => resolve_path(w, base_dir),
        None => base_dir.to_path_buf(),
    };
    let env = raw.env.clone().unwrap_or_default();
    let depends_on = raw.depends_on.clone().unwrap_or_default();

    let p = ResolvedProgram {
        app: app_name.to_string(),
        name: prog_name.to_string(),
        command,
        args,
        work_dir,
        env,
        depends_on,
        autorestart,
        exit_codes,
        restart_backoff,
        max_restart_backoff,
        max_restarts,
        backoff_reset_after,
        startsecs,
        startretries,
        stop_timeout,
        restart_on_unhealthy,
        log_max_size,
        log_rotate_keep,
        health,
        hash: 0,
    };
    Ok(ResolvedProgram {
        hash: def_hash(&p),
        ..p
    })
}

/// Resolve a whole app config file.
pub fn resolve_app(
    name: &str,
    path: &Path,
    raw: &AppRaw,
    defaults: Option<&AppDefaults>,
) -> Result<ResolvedApp> {
    if !is_valid_name(name) {
        bail!("app name {name:?} is not filename-safe");
    }
    let meta = raw.app.as_ref();
    let autostart = meta
        .and_then(|m| m.autostart)
        .or(defaults.and_then(|d| d.autostart))
        .unwrap_or(true);
    let priority = meta
        .and_then(|m| m.priority)
        .or(defaults.and_then(|d| d.priority))
        .unwrap_or(0);
    if let Some(l) = meta {
        let mut errors = Vec::new();
        check_level_defaults(&format!("app[{name}]"), &l.as_defaults(), &mut errors);
        if !errors.is_empty() {
            bail!("app[{name}]: {}", errors.join("; "));
        }
    }
    let programs = raw
        .program
        .iter()
        .map(|(pname, praw)| {
            resolve_program(
                name,
                pname,
                praw,
                meta,
                defaults,
                &path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ResolvedApp {
        name: name.to_string(),
        path: path.to_path_buf(),
        description: meta.and_then(|m| m.description.clone()),
        autostart,
        priority,
        programs,
    })
}

impl AppMeta {
    /// View this metadata as the "defaults" layer for shared checks.
    pub fn as_defaults(&self) -> AppDefaults {
        AppDefaults {
            autostart: self.autostart,
            priority: self.priority,
            autorestart: self.autorestart,
            exit_codes: self.exit_codes.clone(),
            restart_backoff: self.restart_backoff,
            max_restart_backoff: self.max_restart_backoff,
            max_restarts: self.max_restarts,
            backoff_reset_after: self.backoff_reset_after,
            startsecs: self.startsecs,
            startretries: self.startretries,
            stop_timeout: self.stop_timeout,
            restart_on_unhealthy: self.restart_on_unhealthy,
            log_max_size: self.log_max_size.clone(),
            log_rotate_keep: self.log_rotate_keep,
        }
    }
}

/// Cross-app validation: program-name uniqueness, dependency existence and
/// dependency cycles.
pub fn validate_all(apps: &[ResolvedApp]) -> Result<()> {
    let mut errors = Vec::new();
    let mut owners: HashMap<&str, &str> = HashMap::new();
    for a in apps {
        for p in &a.programs {
            if let Some(prev) = owners.insert(p.name.as_str(), a.name.as_str()) {
                errors.push(format!(
                    "program name {:?} is duplicated between app {:?} and app {:?}",
                    p.name, prev, a.name
                ));
            }
        }
    }
    // dependency existence + cycle detection (iterative DFS)
    let all: HashMap<&str, &ResolvedProgram> = apps
        .iter()
        .flat_map(|a| a.programs.iter().map(move |p| (p.name.as_str(), p)))
        .collect();
    let mut state: HashMap<&str, u8> = HashMap::new(); // 0=unvisited 1=visiting 2=done
    for start in all.keys() {
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        while let Some((node, i)) = stack.pop() {
            let st = state.entry(node).or_insert(0);
            if i == 0 {
                if *st == 1 {
                    errors.push(format!(
                        "dependency cycle detected through program {node:?}"
                    ));
                    stack.clear();
                    break;
                }
                *st = 1;
            }
            let deps = all
                .get(node)
                .map(|p| p.depends_on.as_slice())
                .unwrap_or(&[]);
            if let Some(next) = deps.get(i) {
                stack.push((node, i + 1));
                match all.get(next.as_str()) {
                    Some(_) => {
                        let s = state.entry(next.as_str()).or_insert(0);
                        if *s != 2 {
                            stack.push((next.as_str(), 0));
                        }
                    }
                    None => {
                        errors.push(format!(
                            "program {node:?} depends on unknown program {next:?}"
                        ));
                    }
                }
            } else {
                state.insert(node, 2);
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "));
    }
}

// ---------------------------------------------------------------------------
// legacy (v0.1) config import
// ---------------------------------------------------------------------------

pub fn looks_legacy(text: &str) -> bool {
    text.contains("[[program]]")
}

#[derive(Debug, Deserialize)]
struct LegacyFile {
    #[serde(default)]
    daemon: Option<toml::Table>,
    #[serde(default)]
    program: Vec<LegacyProgram>,
}

#[derive(Debug, Deserialize)]
struct LegacyProgram {
    name: String,
    command: String,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    environment: Option<BTreeMap<String, String>>,
    #[serde(default)]
    autorestart: Option<toml::Value>,
    #[serde(default)]
    restart_backoff: Option<f64>,
    #[serde(default)]
    max_restart_backoff: Option<f64>,
    #[serde(default)]
    max_restarts: Option<u32>,
    #[serde(default)]
    stop_timeout: Option<f64>,
    #[serde(default)]
    backoff_reset_after: Option<f64>,
}

pub struct LegacyImport {
    pub app_name: String,
    pub new_file: PathBuf,
    pub daemon_hint: Vec<String>,
}

/// Convert a v0.1 single-file config into a new-shape `xkeeper.toml` next to
/// it. The original file is never modified.
pub fn import_legacy(legacy_path: &Path) -> Result<LegacyImport> {
    let text = std::fs::read_to_string(legacy_path)
        .with_context(|| format!("failed to read {}", legacy_path.display()))?;
    let file: LegacyFile = toml::from_str(&text)
        .with_context(|| format!("failed to parse legacy config {}", legacy_path.display()))?;
    if file.program.is_empty() {
        bail!("legacy config has no [[program]] entries");
    }
    let dir = legacy_path.parent().unwrap_or(Path::new("."));
    let new_file = dir.join("xkeeper.toml");
    if new_file.exists() {
        bail!(
            "{} already exists; refusing to overwrite during legacy import",
            new_file.display()
        );
    }

    let mut programs: BTreeMap<String, ProgramRaw> = BTreeMap::new();
    for p in file.program {
        let autorestart = p
            .autorestart
            .map(|v| match v {
                toml::Value::Boolean(true) => Some(RestartPolicy::Always),
                toml::Value::Boolean(false) => Some(RestartPolicy::Never),
                other => other.as_str().and_then(|s| {
                    serde_json::from_value::<RestartPolicy>(serde_json::json!(s)).ok()
                }),
            })
            .flatten();
        programs.insert(
            p.name.clone(),
            ProgramRaw {
                command: Some(p.command),
                args: p.args,
                work_dir: p.working_dir.map(PathBuf::from),
                env: p.environment,
                depends_on: None,
                autorestart,
                exit_codes: None,
                restart_backoff: p.restart_backoff,
                max_restart_backoff: p.max_restart_backoff,
                max_restarts: p.max_restarts,
                backoff_reset_after: p.backoff_reset_after,
                startsecs: None,
                startretries: None,
                stop_timeout: p.stop_timeout,
                restart_on_unhealthy: None,
                log_max_size: None,
                log_rotate_keep: None,
                health_check: None,
                health_interval: None,
                health_timeout: None,
                health_retries: None,
                health_start_period: None,
            },
        );
    }

    let app_name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "legacy-app".to_string());
    let out = AppRaw {
        app: Some(AppMeta {
            description: Some(format!("imported from {}", legacy_path.display())),
            ..Default::default()
        }),
        program: programs,
    };
    let body = toml::to_string_pretty(&out).context("failed to serialize imported app config")?;
    std::fs::write(
        &new_file,
        format!(
            "# Generated by `xkeeper add` from a v0.1 config. Source: {}\n\n{}",
            legacy_path.display(),
            body
        ),
    )
    .with_context(|| format!("failed to write {}", new_file.display()))?;

    let daemon_hint: Vec<String> = file
        .daemon
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();

    Ok(LegacyImport {
        app_name,
        new_file,
        daemon_hint,
    })
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_command_lines() {
        assert_eq!(
            split_command("python -m http.server 8000"),
            vec!["python", "-m", "http.server", "8000"]
        );
        assert_eq!(split_command("echo \"a b\" c"), vec!["echo", "a b", "c"]);
        assert_eq!(split_command("  spaced   out  "), vec!["spaced", "out"]);
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("10MB").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("1GB").unwrap(), 1024u64.pow(3));
        assert_eq!(parse_size("512KB").unwrap(), 512 * 1024);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("abc").is_err());
    }

    fn app_of(toml_text: &str) -> Result<ResolvedApp> {
        let raw: AppRaw = toml::from_str(toml_text)?;
        resolve_app("demo", Path::new("/opt/demo/xkeeper.toml"), &raw, None)
    }

    #[test]
    fn minimal_map_program_resolves() {
        let a = app_of("[program.api]\ncommand = \"python -m http.server 8000\"\n").unwrap();
        assert_eq!(a.name, "demo");
        assert_eq!(a.autostart, true);
        let p = &a.programs[0];
        assert_eq!(p.name, "api");
        assert_eq!(p.command, "python");
        assert_eq!(p.args, vec!["-m", "http.server", "8000"]);
        assert_eq!(p.work_dir, PathBuf::from("/opt/demo"));
        assert_eq!(p.autorestart, RestartPolicy::Always);
        assert_eq!(p.restart_backoff, 1.0);
        assert_eq!(p.log_max_size, Some(50 * 1024 * 1024)); // built-in default
        assert_eq!(p.log_rotate_keep, 2); // built-in default
    }

    #[test]
    fn log_rotation_defaults_and_opt_out() {
        let a = app_of("[program.a]\ncommand=\"x\"\nlog_max_size = \"0\"\n").unwrap();
        assert_eq!(a.programs[0].log_max_size, None); // "0" disables rotation
        let b =
            app_of("[program.a]\ncommand=\"x\"\nlog_max_size = \"10MB\"\nlog_rotate_keep = 3\n")
                .unwrap();
        assert_eq!(b.programs[0].log_max_size, Some(10 * 1024 * 1024));
        assert_eq!(b.programs[0].log_rotate_keep, 3);
    }

    #[test]
    fn default_log_dir_is_absolute() {
        let d = DaemonSettings::default();
        assert!(d.log_dir.is_absolute());
        #[cfg(unix)]
        assert_eq!(d.log_dir, PathBuf::from("/tmp/xkeeper/logs"));
    }

    #[test]
    fn daemon_rejects_relative_or_empty_log_dir() {
        let rel: DaemonConfig = toml::from_str("[daemon]\nlog_dir = \"logs\"\n").unwrap();
        let err = rel.validate().unwrap_err().to_string();
        assert!(
            err.contains("daemon.log_dir"),
            "error must name the field: {err}"
        );
        assert!(err.contains("absolute"), "error must state the rule: {err}");
        let empty: DaemonConfig = toml::from_str("[daemon]\nlog_dir = \"\"\n").unwrap();
        assert!(empty.validate().is_err());
        let ok: DaemonConfig =
            toml::from_str("[daemon]\nlog_dir = \"/var/log/xkeeper\"\n").unwrap();
        ok.validate().unwrap();
    }

    #[test]
    fn command_and_args_are_mutually_exclusive() {
        let r = app_of("[program.api]\ncommand = \"python -m x\"\nargs = [\"-m\", \"x\"]\n");
        assert!(r.is_err());
    }

    #[test]
    fn four_layer_priority() {
        let defaults: AppDefaults =
            toml::from_str("restart_backoff = 2.0\nautorestart = \"always\"\n").unwrap();
        let raw: AppRaw = toml::from_str(
            "[app]\nautorestart = \"on-failure\"\n\n[program.a]\ncommand = \"x\"\nautorestart = \"never\"\n\n[program.b]\ncommand = \"y\"\n",
        )
        .unwrap();
        let a = resolve_app("demo", Path::new("x.toml"), &raw, Some(&defaults)).unwrap();
        let pa = a.programs.iter().find(|p| p.name == "a").unwrap();
        let pb = a.programs.iter().find(|p| p.name == "b").unwrap();
        assert_eq!(pa.autorestart, RestartPolicy::Never); // program explicit wins
        assert_eq!(pb.autorestart, RestartPolicy::OnFailure); // [app] beats [app-default]
        assert_eq!(pb.restart_backoff, 2.0); // [app-default] fills the gap
    }

    #[test]
    fn health_check_dispatch() {
        let a = app_of(
            "[program.a]\ncommand=\"x\"\nhealth_check = \"http://h/health\"\n\n\
             [program.b]\ncommand=\"x\"\nhealth_check = \"tcp://127.0.0.1:5432\"\n\n\
             [program.c]\ncommand=\"x\"\nhealth_check = \"curl -fsS http://h/\"\n",
        )
        .unwrap();
        let p = |n: &str| a.programs.iter().find(|p| p.name == n).unwrap();
        assert!(matches!(
            &p("a").health.as_ref().unwrap().kind,
            HealthKind::Http { .. }
        ));
        assert!(matches!(
            &p("b").health.as_ref().unwrap().kind,
            HealthKind::Tcp { .. }
        ));
        assert!(matches!(
            &p("c").health.as_ref().unwrap().kind,
            HealthKind::Exec { .. }
        ));
    }

    #[test]
    fn tcp_health_requires_port() {
        assert!(app_of("[program.a]\ncommand=\"x\"\nhealth_check = \"tcp://no-port\"\n").is_err());
    }

    #[test]
    fn daemon_rejects_app_entries() {
        let r: Result<DaemonConfig, _> =
            toml::from_str("[daemon]\nport = 1\n\n[[app]]\nname = \"x\"\n");
        assert!(r.is_err(), "[[app]] must be rejected in daemon config");
    }

    #[test]
    fn validate_all_detects_unknown_dependency_and_cycle() {
        let a = app_of("[program.a]\ncommand=\"x\"\ndepends_on = [\"b\"]\n\n[program.b]\ncommand=\"y\"\ndepends_on = [\"a\"]\n").unwrap();
        assert!(validate_all(&[a]).is_err());
        let b = app_of("[program.a]\ncommand=\"x\"\ndepends_on = [\"ghost\"]\n").unwrap();
        assert!(validate_all(&[b]).is_err());
        let c = app_of("[program.a]\ncommand=\"x\"\n").unwrap();
        assert!(validate_all(&[c]).is_ok());
    }

    #[test]
    fn legacy_import_converts_shape() {
        let tmp = std::env::temp_dir().join(format!("xk-legacy-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let legacy = tmp.join("config.toml");
        std::fs::write(
            &legacy,
            "[daemon]\nlog_level = \"info\"\n\n[[program]]\nname = \"web\"\ncommand = \"python\"\nargs = [\"-m\", \"http.server\"]\nautorestart = true\n",
        )
        .unwrap();
        let imp = import_legacy(&legacy).unwrap();
        assert_eq!(imp.app_name, tmp.file_name().unwrap().to_string_lossy());
        assert!(imp.daemon_hint.contains(&"log_level".to_string()));
        let text = std::fs::read_to_string(&imp.new_file).unwrap();
        assert!(text.contains("[program.web]"));
        let raw: AppRaw = toml::from_str(&text).unwrap();
        assert!(raw.program.contains_key("web"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
