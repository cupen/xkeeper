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

/// The identifier charset shared by app names, program names and custom
/// action names (glossary 标识符不变量). App names become link file names,
/// program names become log file names, and all three address objects from
/// every panel — so one strict rule covers them all.
pub const NAME_RULE: &str =
    "names may only contain ASCII letters, digits, '_' and '-' ([A-Za-z0-9_-])";

/// The single gate for app names, program names and action names. Dots,
/// spaces, non-ASCII and control characters are rejected; an empty name is
/// rejected. `app.program`-style composite names are therefore never valid
/// identifiers (glossary: they must not be parsed as addressing syntax).
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Built-in action names reserved for custom actions (glossary 动作词表边界:
/// observe-only commands and `apply` are NOT reserved).
pub const RESERVED_ACTIONS: [&str; 6] =
    ["start", "stop", "restart", "signal", "reload", "shutdown"];

/// Action `timeout` when the field is omitted (actions/configuration spec).
pub const DEFAULT_ACTION_TIMEOUT: u64 = 30;

/// A known-domain `${...}` reference in an action command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarRef {
    pub domain: VarDomain,
    /// Program/app name; empty for the daemon domain.
    pub name: String,
    pub field: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarDomain {
    Program,
    App,
    Daemon,
}

/// Fields allowed per domain (`${program.<n>.field}` etc., configuration
/// spec: 动作定义表). Names are NOT checked here — program names are globally
/// unique and cross-program references are allowed.
pub const PROGRAM_VAR_FIELDS: [&str; 5] = ["pid", "state", "app", "work_dir", "log_dir"];
pub const APP_VAR_FIELDS: [&str; 1] = ["path"];
pub const DAEMON_VAR_FIELDS: [&str; 5] = ["pid", "host", "port", "log_dir", "app_dir"];

/// Parse the inside of one `${...}`. `None` = the content does not start with
/// a known domain prefix, so it stays verbatim for the shell (e.g. `${HOME}`).
/// `Some(Ok(_))` = a well-formed known-domain variable; `Some(Err(_))` = it
/// looked like a known-domain variable but is malformed (unknown field or
/// missing parts) — rejected at resolve time so typos fail fast.
pub fn parse_var_ref(content: &str) -> Option<Result<VarRef, String>> {
    let parts: Vec<&str> = content.split('.').collect();
    let known = |field: &str, fields: &[&str]| fields.contains(&field);
    match parts.as_slice() {
        ["program", name, field] if known(field, &PROGRAM_VAR_FIELDS) => Some(Ok(VarRef {
            domain: VarDomain::Program,
            name: (*name).to_string(),
            field: (*field).to_string(),
        })),
        ["app", name, field] if known(field, &APP_VAR_FIELDS) => Some(Ok(VarRef {
            domain: VarDomain::App,
            name: (*name).to_string(),
            field: (*field).to_string(),
        })),
        ["daemon", field] if known(field, &DAEMON_VAR_FIELDS) => Some(Ok(VarRef {
            domain: VarDomain::Daemon,
            name: String::new(),
            field: (*field).to_string(),
        })),
        ["program", _, _] | ["app", _, _] => Some(Err(format!(
            "unknown variable field (allowed: program.<name>.{{{}}}, app.<name>.{{{}}}, daemon.{{{}}})",
            PROGRAM_VAR_FIELDS.join(", "),
            APP_VAR_FIELDS.join(", "),
            DAEMON_VAR_FIELDS.join(", ")
        ))),
        ["daemon", _] => Some(Err(format!(
            "unknown variable field (allowed: daemon.{{{}}})",
            DAEMON_VAR_FIELDS.join(", ")
        ))),
        ["program"] | ["app"] | ["daemon"] | ["program", _] | ["app", _] => Some(Err(
            "incomplete variable reference (e.g. ${program.<name>.pid} or ${daemon.pid})".into(),
        )),
        _ => None,
    }
}

/// Extract the inside text of every `${...}` occurrence (no nesting: the
/// first `}` closes the reference).
fn extract_var_texts(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                out.push(&after[..end]);
                rest = &after[end + 1..];
            }
            None => break, // unterminated: shell's problem, left verbatim
        }
    }
    out
}

/// Known-domain variables in `cmd` whose text is malformed, for the
/// resolve-time check (configuration spec: 未知动作变量被拒绝).
pub fn unknown_action_vars(cmd: &str) -> Vec<String> {
    extract_var_texts(cmd)
        .into_iter()
        .filter_map(|raw| {
            parse_var_ref(raw).and_then(|r| r.err().map(|e| format!("${{{raw}}}: {e}")))
        })
        .collect()
}

/// Replace known-domain `${...}` variables via `resolve`; unknown-domain
/// `${...}` stays verbatim for the shell (user's `${HOME}` is untouched).
/// A known-domain reference resolving to `None` (e.g. the pid of a stopped
/// program) becomes the empty string.
pub fn substitute_vars(cmd: &str, resolve: impl Fn(&VarRef) -> Option<String>) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut rest = cmd;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        out.push_str(&rest[..start]);
        match after.find('}') {
            Some(end) => {
                let raw = &after[..end];
                match parse_var_ref(raw) {
                    Some(Ok(r)) => out.push_str(&resolve(&r).unwrap_or_default()),
                    _ => {
                        // Unknown domain or malformed: hand the text to the shell.
                        out.push_str("${");
                        out.push_str(raw);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Split a command line into argv using shell word rules (whitespace split,

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
    /// Optional `[webui]` section: its mere presence enables the embedded
    /// console (config-driven-webui D1, presence-based — there is no
    /// `enabled` boolean to keep the delete semantics crisp).
    #[serde(default)]
    pub webui: Option<WebuiSettings>,
}

/// Built-in web console listen address (`webui.listen` default).
pub const DEFAULT_WEBUI_LISTEN: &str = "127.0.0.1:9877";

/// The optional `[webui]` section of the daemon config. Only `listen` exists
/// for now; unknown keys are rejected like everywhere else.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebuiSettings {
    pub listen: String,
}

impl Default for WebuiSettings {
    fn default() -> Self {
        Self {
            listen: DEFAULT_WEBUI_LISTEN.to_string(),
        }
    }
}

/// The shared `host:port` rule for `webui.listen`: non-empty, a non-empty
/// host part and a port in 1..=65535. Write-time (`config --set`) and
/// load-time (`validate`/`run`/`reload`) use this one gate, so a value
/// accepted by one is accepted by the other.
pub fn validate_listen_addr(s: &str) -> std::result::Result<(), String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("must not be empty (expected host:port)".to_string());
    }
    if t != s {
        return Err(format!(
            "must not have surrounding whitespace, got {s:?} (expected host:port)"
        ));
    }
    let Some((host, port)) = s.rsplit_once(':') else {
        return Err(format!("must be host:port, got {s:?} (missing port)"));
    };
    if host.is_empty() {
        return Err(format!("must be host:port, got {s:?} (missing host)"));
    }
    match port.parse::<u16>() {
        Ok(p) if p > 0 => Ok(()),
        _ => Err(format!(
            "port must be an integer in 1..=65535, got {port:?}"
        )),
    }
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
            webui: None,
        }
    }

    /// The webui console intent this config expresses: `None` when the
    /// `[webui]` section is absent (console off, presence-based).
    pub fn webui_listen(&self) -> Option<&str> {
        self.webui.as_ref().map(|w| w.listen.as_str())
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
        if let Some(w) = &self.webui {
            if let Err(e) = validate_listen_addr(&w.listen) {
                errors.push(format!("webui.listen: {e}"));
            }
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

/// One `[program.<name>.action.<action-name>]` table as written on disk.
/// Only `command` is required; `timeout` is optional (seconds, default
/// [`DEFAULT_ACTION_TIMEOUT`], must be > 0).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRaw {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
}

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
    /// Custom actions declared as `[program.<name>.action.<action-name>]`
    /// tables; the map key is the action name (configuration spec: 动作定义表).
    pub action: BTreeMap<String, ActionRaw>,
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

/// One resolved custom action: the command template (substituted at spawn
/// time) and the effective timeout in seconds.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedAction {
    pub command: String,
    pub timeout: u64,
}

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
    /// Custom actions of this program, validated at resolve time
    /// (actions spec: 自定义动作执行契约).
    pub actions: BTreeMap<String, ResolvedAction>,
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
        bail!("program name {prog_name:?} is invalid: {NAME_RULE}");
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

    // Custom actions: validate the name (charset + reserved words), the
    // command (non-empty, variable names known) and the timeout (> 0) at
    // resolve time so `validate`/`reload` fail fast (configuration spec).
    let mut actions = BTreeMap::new();
    for (aname, araw) in &raw.action {
        if !is_valid_name(aname) {
            bail!("program[{prog_name}] action name {aname:?} is invalid: {NAME_RULE}");
        }
        if RESERVED_ACTIONS.contains(&aname.as_str()) {
            bail!(
                "program[{prog_name}] action name {aname:?} is reserved (built-in action names \
                 {} cannot be redefined)",
                RESERVED_ACTIONS.join(", ")
            );
        }
        if araw.command.trim().is_empty() {
            bail!("program[{prog_name}] action[{aname}]: command must not be empty");
        }
        let timeout = match araw.timeout {
            Some(0) => bail!("program[{prog_name}] action[{aname}]: timeout must be > 0"),
            Some(t) => t,
            None => DEFAULT_ACTION_TIMEOUT,
        };
        let unknown = unknown_action_vars(&araw.command);
        if !unknown.is_empty() {
            bail!(
                "program[{prog_name}] action[{aname}]: unknown action variable(s): {}",
                unknown.join(", ")
            );
        }
        actions.insert(
            aname.clone(),
            ResolvedAction {
                command: araw.command.clone(),
                timeout,
            },
        );
    }

    let p = ResolvedProgram {
        app: app_name.to_string(),
        name: prog_name.to_string(),
        command,
        args,
        actions,
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
        bail!("app name {name:?} is invalid: {NAME_RULE}");
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
                action: BTreeMap::new(),
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
// `config` subcommand engine (add-config-subcommand)
// ---------------------------------------------------------------------------

/// A strong-typed `[daemon]` key value after CLI parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValue {
    Str(String),
    Int(i64),
    Float(f64),
}

impl std::fmt::Display for KeyValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyValue::Str(s) => f.write_str(s),
            KeyValue::Int(i) => write!(f, "{i}"),
            KeyValue::Float(v) => f.write_str(&fmt_float(*v)),
        }
    }
}

/// Render a float the way TOML and humans expect it (1.0 keeps its decimal
/// point — a bare `1` would change the field's type on re-read).
fn fmt_float(v: f64) -> String {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e15 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// Type parsers for the known `[daemon]` keys (design D3). Range rules that
/// belong to whole-config validation (e.g. monitor_interval <= 60, non-zero
/// log_buffer_lines) are NOT duplicated here — the post-write validation
/// catches them and the write rolls back.
const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

fn parse_log_level(s: &str) -> std::result::Result<KeyValue, String> {
    if LOG_LEVELS.contains(&s) {
        Ok(KeyValue::Str(s.to_string()))
    } else {
        Err(format!("log_level must be one of {}", LOG_LEVELS.join("|")))
    }
}

fn parse_port(s: &str) -> std::result::Result<KeyValue, String> {
    let v: u16 = s
        .parse()
        .map_err(|_| format!("port must be an integer in 1..=65535, got {s:?}"))?;
    if v == 0 {
        return Err("port must be an integer in 1..=65535".to_string());
    }
    Ok(KeyValue::Int(v as i64))
}

fn parse_monitor_interval(s: &str) -> std::result::Result<KeyValue, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("monitor_interval must be a positive number, got {s:?}"))?;
    if v <= 0.0 || !v.is_finite() {
        return Err(format!("monitor_interval must be > 0, got {s:?}"));
    }
    Ok(KeyValue::Float(v))
}

fn parse_count(s: &str) -> std::result::Result<KeyValue, String> {
    let v: i64 = s
        .parse()
        .map_err(|_| format!("log_buffer_lines must be a non-negative integer, got {s:?}"))?;
    if v < 0 {
        return Err(format!("log_buffer_lines must be >= 0, got {s:?}"));
    }
    Ok(KeyValue::Int(v))
}

fn parse_string(s: &str) -> std::result::Result<KeyValue, String> {
    Ok(KeyValue::Str(s.to_string()))
}

/// `webui.listen` parser: the same host:port gate the loader applies, so an
/// accepted value can never fail validation after the write.
fn parse_webui_listen(s: &str) -> std::result::Result<KeyValue, String> {
    validate_listen_addr(s).map_err(|e| format!("webui.listen: {e}"))?;
    Ok(KeyValue::Str(s.to_string()))
}

/// One known config key: a leaf inside a known table. Built-in defaults are
/// NOT copied here — they flow from the settings structs' `Default` impls via
/// the display helpers and the init template (design D3: one source of truth,
/// no drift). `set`/`get`/`delete` all resolve through these tables, so a
/// later change that adds keys extends one list.
pub struct DaemonKey {
    pub name: &'static str,
    pub parse: fn(&str) -> std::result::Result<KeyValue, String>,
}

pub const DAEMON_KEYS: &[DaemonKey] = &[
    DaemonKey {
        name: "log_level",
        parse: parse_log_level,
    },
    DaemonKey {
        name: "log_dir",
        parse: parse_string,
    },
    DaemonKey {
        name: "monitor_interval",
        parse: parse_monitor_interval,
    },
    DaemonKey {
        name: "host",
        parse: parse_string,
    },
    DaemonKey {
        name: "port",
        parse: parse_port,
    },
    DaemonKey {
        name: "auth_token",
        parse: parse_string,
    },
    DaemonKey {
        name: "log_buffer_lines",
        parse: parse_count,
    },
    DaemonKey {
        name: "app_dir",
        parse: parse_string,
    },
];

pub fn find_daemon_key(name: &str) -> Option<&'static DaemonKey> {
    DAEMON_KEYS.iter().find(|k| k.name == name)
}

/// `[webui]` section keys (config-driven-webui): the section's presence
/// enables the console, so `listen` is the only key — no `enabled` boolean.
pub const WEBUI_KEYS: &[DaemonKey] = &[DaemonKey {
    name: "listen",
    parse: parse_webui_listen,
}];

fn find_webui_key(name: &str) -> Option<&'static DaemonKey> {
    WEBUI_KEYS.iter().find(|k| k.name == name)
}

/// Resolve a CLI key to its `(table, leaf)`. Bare names are `[daemon]` leaf
/// shorthand (existing behavior); `daemon.<leaf>` and `webui.<leaf>` name the
/// table directly. Returns `None` for unknown keys.
fn resolve_key(key: &str) -> Option<(&'static str, &str)> {
    if let Some(leaf) = key.strip_prefix("daemon.") {
        return find_daemon_key(leaf).map(|_| ("daemon", leaf));
    }
    if let Some(leaf) = key.strip_prefix("webui.") {
        return find_webui_key(leaf).map(|_| ("webui", leaf));
    }
    find_daemon_key(key).map(|_| ("daemon", key))
}

/// The table a (pre-validated) key addresses — the write path routes on it.
fn resolve_key_table(key: &str) -> Option<&'static str> {
    resolve_key(key).map(|(table, _)| table)
}

/// The leaf of a (pre-validated) dotted or bare key.
fn key_leaf(key: &str) -> &str {
    key.rsplit_once('.').map(|(_, leaf)| leaf).unwrap_or(key)
}

fn known_key_list() -> String {
    let mut names: Vec<String> = DAEMON_KEYS.iter().map(|k| k.name.to_string()).collect();
    names.extend(WEBUI_KEYS.iter().map(|k| format!("webui.{}", k.name)));
    names.join(", ")
}

/// Display one `[daemon]` field for `--get`. Reads the typed field off the
/// settings struct, so a file value and the built-in default render the same
/// way (the default flows from `DaemonSettings::default`, never a literal).
pub fn daemon_key_display(settings: &DaemonSettings, name: &str) -> String {
    match name {
        "log_level" => settings.log_level.clone(),
        "log_dir" => settings.log_dir.to_string_lossy().into_owned(),
        "monitor_interval" => fmt_float(settings.monitor_interval),
        "host" => settings.host.clone(),
        "port" => settings.port.to_string(),
        "auth_token" => settings.auth_token.clone(),
        "log_buffer_lines" => settings.log_buffer_lines.to_string(),
        "app_dir" => settings.app_dir.to_string_lossy().into_owned(),
        // Callers resolve names through DAEMON_KEYS first.
        _ => unreachable!("key table and display are in sync: {name:?}"),
    }
}

/// `config --get` effective value: a missing file answers with the built-in
/// defaults (same semantics as the daemon's `load_or_default`); a present
/// file must be valid, and its typed value wins over the default. A missing
/// `[webui]` section answers with the built-in default listen too (the
/// section is presence-based; the key itself keeps a default).
pub fn config_effective_value(text: Option<&str>, name: &str) -> Result<String> {
    let Some((table, leaf)) = resolve_key(name) else {
        bail!(
            "unknown config key {name:?} (known keys: {})",
            known_key_list()
        );
    };
    let cfg = match text {
        None => DaemonConfig::empty(),
        Some(t) => {
            let cfg: DaemonConfig =
                toml::from_str(t).with_context(|| "daemon config is invalid".to_string())?;
            cfg.validate()
                .with_context(|| "daemon config is invalid".to_string())?;
            cfg
        }
    };
    Ok(match table {
        "daemon" => daemon_key_display(&cfg.daemon, leaf),
        "webui" => cfg
            .webui
            .as_ref()
            .map(|w| w.listen.clone())
            .unwrap_or_else(|| DEFAULT_WEBUI_LISTEN.to_string()),
        other => unreachable!("key tables are daemon|webui: {other:?}"),
    })
}

/// Parse `--set K=V` pairs against the key whitelist. Every pair is validated
/// before anything is written (all-or-nothing; spec: 不落盘 on rejection).
pub fn parse_sets(pairs: &[String]) -> Result<Vec<(String, KeyValue)>> {
    let mut out = Vec::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--set expects KEY=VALUE, got {p:?}"))?;
        let key = resolve_key_table(k).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown config key {k:?} (known keys: {})",
                known_key_list()
            )
        })?;
        let leaf = key_leaf(k);
        // The leaf parser is the strong-typed gate (resolve_key validated the
        // table membership; this validates the value).
        let kv = match key {
            "daemon" => (find_daemon_key(leaf).expect("resolve_key validated").parse)(v),
            "webui" => (find_webui_key(leaf).expect("resolve_key validated").parse)(v),
            other => unreachable!("key tables are daemon|webui: {other:?}"),
        }
        .map_err(|e| anyhow::anyhow!("--set {p}: {e}"))?;
        out.push((k.to_string(), kv));
    }
    Ok(out)
}

/// The old value's decor (whitespace after `=` / same-line trailing comment),
/// re-attached to the new value so the line keeps its exact spacing and
/// comments. Comments above the key live on the key node and survive the
/// replacement untouched.
type ValueDecor = (Option<toml_edit::RawString>, Option<toml_edit::RawString>);

fn table_value_decor(tbl: &toml_edit::Table, key: &str) -> ValueDecor {
    tbl.get(key)
        .and_then(|it| it.as_value())
        .map(|v| (v.decor().prefix().cloned(), v.decor().suffix().cloned()))
        .unwrap_or((None, None))
}

fn inline_value_decor(tbl: &toml_edit::InlineTable, key: &str) -> ValueDecor {
    tbl.get(key)
        .map(|v| (v.decor().prefix().cloned(), v.decor().suffix().cloned()))
        .unwrap_or((None, None))
}

/// Build a toml_edit value item carrying the old line's decor.
fn typed_value_item(kv: &KeyValue, decor: ValueDecor) -> toml_edit::Item {
    let mut item = match kv {
        KeyValue::Str(s) => toml_edit::value(s.clone()),
        KeyValue::Int(i) => toml_edit::value(*i),
        KeyValue::Float(f) => toml_edit::value(*f),
    };
    if let Some(v) = item.as_value_mut() {
        if let Some(pfx) = decor.0 {
            v.decor_mut().set_prefix(pfx);
        }
        if let Some(sfx) = decor.1 {
            v.decor_mut().set_suffix(sfx);
        }
    }
    item
}

/// Replace one value in a `[daemon]` table. Assignment through `IndexMut`
/// swaps only the value node — the key keeps its own decor (comments above
/// the line, spacing). `Table::insert` would reformat the key and is avoided.
fn set_value_in_table(tbl: &mut toml_edit::Table, key: &str, kv: &KeyValue) {
    let decor = table_value_decor(tbl, key);
    let item = typed_value_item(kv, decor);
    tbl[key] = item;
}

/// Same for the dotted-key style (`daemon.port = 1` parses as an inline table).
fn set_value_in_inline(tbl: &mut toml_edit::InlineTable, key: &str, kv: &KeyValue) {
    let decor = inline_value_decor(tbl, key);
    let item = typed_value_item(kv, decor);
    // typed_value_item always builds Item::Value, so this cannot fail.
    let value = match item.into_value() {
        Ok(v) => v,
        Err(_) => unreachable!("typed_value_item builds Item::Value"),
    };
    match tbl.get_mut(key) {
        Some(slot) => *slot = value,
        None => {
            tbl.insert(key, value);
        }
    }
}

/// Parse+validate daemon config text (the base of a write must be legal —
/// design D2: only touch valid files).
fn load_daemon_text(text: &str) -> Result<DaemonConfig> {
    let cfg: DaemonConfig =
        toml::from_str(text).with_context(|| "existing daemon config is invalid".to_string())?;
    cfg.validate()
        .with_context(|| "existing daemon config is invalid".to_string())?;
    Ok(cfg)
}

/// `config --set`: read-modify-write via toml_edit — only the target value
/// nodes change; untouched keys keep their order, formatting and comments.
/// Keys route to their table (`[daemon]` leaves, `webui.listen` → `[webui]`);
/// setting a `[webui]` key creates the section when missing (that is how
/// `--set webui.listen=...` turns the console on). The rendered text is
/// validated as a whole before it is returned; the caller writes it
/// atomically (design D2/D4).
pub fn config_apply_sets(text: &str, sets: &[(String, KeyValue)]) -> Result<String> {
    load_daemon_text(text)?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| "failed to parse daemon config as TOML".to_string())?;
    for (key, kv) in sets {
        let table = resolve_key_table(key).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown config key {key:?} (known keys: {})",
                known_key_list()
            )
        })?;
        let leaf = key_leaf(key);
        let item = doc
            .entry(table)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        match item {
            toml_edit::Item::Table(t) => set_value_in_table(t, leaf, kv),
            toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => {
                set_value_in_inline(t, leaf, kv)
            }
            _ => bail!("[{table}] entry is not a table — cannot set {key:?}"),
        }
    }
    let rendered = doc.to_string();
    let out: DaemonConfig = toml::from_str(&rendered)
        .with_context(|| "the new config does not validate".to_string())?;
    out.validate()
        .with_context(|| "the new config does not validate".to_string())?;
    Ok(rendered)
}

/// `config --delete` result: `Changed` carries the re-rendered text; `NoChange`
/// means no target existed (file untouched, idempotent success).
#[derive(Debug, PartialEq)]
pub enum DeleteOutcome {
    Changed(String),
    NoChange,
}

/// One resolved `--delete` target: a leaf key inside a known table, or a
/// whole optional table. Bare keys are shorthand for `[daemon]` leaves
/// (`port` ≡ `daemon.port`); the presence-based `[webui]` section is deleted
/// as a whole with its bare table name (`--delete webui` = console off).
#[derive(Debug, PartialEq)]
enum DeleteTarget {
    Leaf(&'static str, String),
    Table(&'static str),
}

fn resolve_delete_target(path: &str) -> std::result::Result<DeleteTarget, String> {
    match path.split_once('.') {
        None => {
            if find_daemon_key(path).is_some() {
                Ok(DeleteTarget::Leaf("daemon", path.to_string()))
            } else if path == "webui" {
                Ok(DeleteTarget::Table("webui"))
            } else {
                Err(format!(
                    "unknown config key {path:?} (known keys: {}; a bare optional table name \
                     deletes the whole section, e.g. --delete webui)",
                    known_key_list()
                ))
            }
        }
        Some((table, leaf)) => match table {
            "daemon" => {
                if find_daemon_key(leaf).is_some() {
                    Ok(DeleteTarget::Leaf("daemon", leaf.to_string()))
                } else {
                    Err(format!(
                        "unknown [daemon] key {path:?} (known keys: {})",
                        known_key_list()
                    ))
                }
            }
            "webui" => {
                if find_webui_key(leaf).is_some() {
                    Ok(DeleteTarget::Leaf("webui", leaf.to_string()))
                } else {
                    Err(format!(
                        "unknown [webui] key {path:?} (known keys: {}; delete the whole \
                         section with `--delete webui`)",
                        known_key_list()
                    ))
                }
            }
            other => Err(format!(
                "unknown table {other:?} in delete path {path:?} — the config key table covers \
                 [daemon] leaves and the [webui] section"
            )),
        },
    }
}

/// `config --delete`: resolve every target first (all-or-nothing), then remove
/// the value nodes via toml_edit. Absent targets are skipped; when nothing
/// was removed the original text is returned untouched as `NoChange`.
/// Deletion puts a key back into its "unconfigured" state (built-in defaults
/// apply again); deleting the `[webui]` table switches the console off.
/// Rendered output is validated like `--set`.
pub fn config_apply_deletes(text: &str, targets: &[String]) -> Result<DeleteOutcome> {
    load_daemon_text(text)?;
    let mut resolved = Vec::new();
    for t in targets {
        resolved.push(resolve_delete_target(t).map_err(|e| anyhow::anyhow!("{e}"))?);
    }
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| "failed to parse daemon config as TOML".to_string())?;
    let mut any = false;
    for target in &resolved {
        match target {
            DeleteTarget::Leaf(table, leaf) => match doc.get_mut(*table) {
                Some(toml_edit::Item::Table(tbl)) => {
                    if tbl.contains_key(leaf.as_str()) {
                        tbl.remove(leaf.as_str());
                        any = true;
                    }
                }
                // Dotted-key style config: `daemon.port = 1` parses as an
                // inline table.
                Some(toml_edit::Item::Value(toml_edit::Value::InlineTable(tbl))) => {
                    if tbl.contains_key(leaf.as_str()) {
                        tbl.remove(leaf.as_str());
                        any = true;
                    }
                }
                // Table not present at all: the target is absent.
                _ => {}
            },
            DeleteTarget::Table(table) => {
                // Covers both the `[webui]` table style and the dotted-key
                // style (a root `webui` key holding an inline table).
                if doc.remove(*table).is_some() {
                    any = true;
                }
            }
        }
    }
    if !any {
        return Ok(DeleteOutcome::NoChange);
    }
    let rendered = doc.to_string();
    let out: DaemonConfig = toml::from_str(&rendered)
        .with_context(|| "the new config does not validate".to_string())?;
    out.validate()
        .with_context(|| "the new config does not validate".to_string())?;
    Ok(DeleteOutcome::Changed(rendered))
}

/// Write a rendered daemon config atomically with a post-write validation
/// gate (design D4): temp file next to the target → load+validate the temp
/// content → rename over the target. Validation failure removes the temp
/// file and leaves the target untouched; a crash never leaves a half-written
/// config behind.
pub fn atomic_write_daemon_config(path: &Path, text: &str) -> Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "daemon.toml".to_string());
    let tmp = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(format!("{name}.tmp"));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Err(e) = DaemonConfig::load_or_default(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("{e:#}")).context(format!(
            "rejected: the new config does not validate; {} was not modified",
            path.display()
        ));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// The fully commented `[app-default]` template section of the init output.
const INIT_APP_DEFAULT_SECTION: &str = "\
# ---------------------------------------------------------------------------
# Shared defaults for every app ([app-default]) — optional, omitted here.
# Priority: [program.*] explicit fields > the app file's [app] table > this.
# Uncomment to use; field names match program-level fields:
# [app-default]
# autostart = true
# autorestart = \"always\"
# restart_backoff = 1.0
# max_restart_backoff = 30.0
# max_restarts = 0
";

/// Render the `config --init` daemon template: `[daemon]` with every known
/// key at its built-in default (injected from [`DaemonSettings::default`],
/// design D3/D5) plus a fully commented `[app-default]` template section.
pub fn render_init_template() -> String {
    let d = DaemonSettings::default();
    // Strings render through toml_edit so escaping is always valid TOML.
    let s = |v: &str| toml_edit::Value::from(v.to_string()).to_string();
    let mut out = format!(
        "\
# xkeeper daemon config — generated by `xkeeper config --init`.
#
# Every field below is the built-in default; edit freely, or delete the file
# entirely (a missing config means \"run with defaults\").
# The embedded web console is opt-in and stays OFF by default: add a
# `[webui]` section (optionally with `listen = \"127.0.0.1:9877\"`) to enable
# it, or run `xkeeper config --set webui.listen=127.0.0.1:9877` + reload.
# Validate:  xkeeper validate          Edit:  xkeeper config --edit
# Scripted:  xkeeper config --get port | --set port=8080 | --delete port

[daemon]
# Daemon log level: trace | debug | info | warn | error.
log_level = {log_level}
# Directory for child process stdout/stderr logs; must be an absolute path.
# /tmp (%TEMP% on Windows) is volatile — pin an absolute path such as
# /var/log/xkeeper for logs that survive reboots.
log_dir = {log_dir}
# Supervision loop period in seconds, range (0, 60].
monitor_interval = {monitor_interval}
# Control-plane HTTP API bind address; the CLI and webui clients talk to it.
# Loopback only by default — evaluate auth before exposing it.
host = {host}
port = {port}
# Non-empty = the control plane requires `Authorization: Bearer <token>`
# (CLI and webui must send the same value); empty = no auth.
auth_token = {auth_token}
# Recent output lines kept in memory per stream (the source for
# `xkeeper log` and the webui live log).
log_buffer_lines = {log_buffer_lines}
# App registry directory: one <name>.toml record per registered app.
# A relative value resolves against the directory of THIS config file.
app_dir = {app_dir}

",
        log_level = s(&d.log_level),
        log_dir = s(&d.log_dir.to_string_lossy()),
        monitor_interval = fmt_float(d.monitor_interval),
        host = s(&d.host),
        port = d.port,
        auth_token = s(&d.auth_token),
        log_buffer_lines = d.log_buffer_lines,
        app_dir = s(&d.app_dir.to_string_lossy()),
    );
    out.push_str(INIT_APP_DEFAULT_SECTION);
    out
}

/// The all-commented app config template written as
/// `<app_dir>/example.toml.sample` by `config --init`. The `.sample` suffix
/// keeps it out of the registry scan (only `.toml` records are discovered).
pub const APP_EXAMPLE_SAMPLE: &str = "\
# example.toml.sample — an annotated APP config template (not loaded by
# xkeeper; the .sample suffix keeps it out of the registry scan).
#
# Usage: copy it into your app deployment directory as `xkeeper.toml`, set
# `command`, then run `xkeeper add . --name <app>` in that directory.
# Priority: [program.*] explicit fields > the [app] table > [app-default].
#
# [app]
# description = \"my app\"
# autostart = true
# autorestart = \"always\"      # always | on-failure | never
#
# [program.main]
# command = \"/opt/myapp/myserver --port 8080\"   # single line, shell-lexed
# work_dir = \".\"
# env = { LOG = \"info\" }
# depends_on = []
# health_check = \"http://127.0.0.1:8080/health\"  # http(s):// | tcp:// | exec
";

/// What `config --init` created (for the caller's report).
#[derive(Debug)]
pub struct InitReport {
    pub config_file: PathBuf,
    pub app_dir: PathBuf,
    pub sample_file: PathBuf,
    pub sample_created: bool,
}

/// `config --init`: render the daemon template at `config_path`, create the
/// app registry directory (the rendered `app_dir` resolved against the config
/// file's directory — same rule the supervisor uses) and write the commented
/// `example.toml.sample` into it (skipped when it already exists, design D5).
/// The caller refuses to run this when the config file already exists.
pub fn config_init(config_path: &Path) -> Result<InitReport> {
    let text = render_init_template();
    if let Some(dir) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create config dir {}", dir.display()))?;
    }
    std::fs::write(config_path, &text)
        .with_context(|| format!("cannot write {}", config_path.display()))?;

    let config_dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let app_dir = resolve_path(&DaemonSettings::default().app_dir, config_dir);
    std::fs::create_dir_all(&app_dir)
        .with_context(|| format!("cannot create app dir {}", app_dir.display()))?;
    let sample = app_dir.join("example.toml.sample");
    let sample_created = !sample.exists();
    if sample_created {
        std::fs::write(&sample, APP_EXAMPLE_SAMPLE)
            .with_context(|| format!("cannot write {}", sample.display()))?;
    }
    Ok(InitReport {
        config_file: config_path.to_path_buf(),
        app_dir,
        sample_file: sample,
        sample_created,
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

    /// configuration: identifiers are [A-Za-z0-9_-]+ — dots, spaces and
    /// non-ASCII are rejected (breaking tighten, glossary 标识符不变量).
    #[test]
    fn name_charset_is_strict() {
        for ok in ["api", "web-1", "worker_2", "A-b_c9"] {
            assert!(is_valid_name(ok), "{ok:?} must pass");
        }
        for bad in [
            "",           // empty
            "my.web",     // dot (composite names are not identifiers)
            "my web",     // space
            "プログラム", // non-ASCII
            "a/b",        // path separator
            "..",         // dot-dot (dots are illegal anyway)
            "a\nb",       // control char
        ] {
            assert!(!is_valid_name(bad), "{bad:?} must be rejected");
        }
        // The gate rejects at resolve time with a repair hint.
        let r = app_of("[program.\"my.web\"]\ncommand = 'x'\n");
        let msg = r.err().unwrap().to_string();
        assert!(
            msg.contains("my.web") && msg.contains("[A-Za-z0-9_-]"),
            "{msg}"
        );
        let app_err = resolve_app(
            "bad name",
            Path::new("x.toml"),
            &toml::from_str("[program.a]\ncommand='x'\n").unwrap(),
            None,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            app_err.contains("bad name") && app_err.contains("invalid"),
            "{app_err}"
        );
    }

    /// configuration: the minimal action table resolves; timeout defaults to
    /// 30 and explicit positive values pass through.
    #[test]
    fn action_table_minimal_and_timeout() {
        let a = app_of(
            "[program.api]\ncommand = 'x'\n\n[program.api.action.flush]\ncommand = 'curl -fsS http://h/flush'\n",
        )
        .unwrap();
        let act = a.programs[0].actions.get("flush").expect("flush resolved");
        assert_eq!(act.command, "curl -fsS http://h/flush");
        assert_eq!(act.timeout, 30, "omitted timeout defaults to 30");
        let b = app_of(
            "[program.api]\ncommand = 'x'\n\n[program.api.action.flush]\ncommand = 'x'\ntimeout = 5\n",
        )
        .unwrap();
        assert_eq!(b.programs[0].actions["flush"].timeout, 5);
    }

    /// configuration: action definitions are validated — timeout range,
    /// empty command, unknown fields, reserved words, bad names.
    #[test]
    fn action_table_validation() {
        // timeout = 0 is rejected and names the field.
        let r = app_of(
            "[program.api]\ncommand='x'\n\n[program.api.action.t0]\ncommand='x'\ntimeout = 0\n",
        );
        let msg = r.err().unwrap().to_string();
        assert!(msg.contains("timeout") && msg.contains("> 0"), "{msg}");
        // A negative timeout fails at TOML parse time and names the field.
        let r = app_of(
            "[program.api]\ncommand='x'\n\n[program.api.action.tn]\ncommand='x'\ntimeout = -1\n",
        );
        let msg = r.err().unwrap().to_string();
        assert!(
            msg.contains("timeout"),
            "parse error must name the field: {msg}"
        );
        // Unknown fields inside an action table are rejected.
        assert!(
            app_of(
                "[program.api]\ncommand='x'\n\n[program.api.action.u]\ncommand='x'\nretries = 3\n"
            )
            .is_err()
        );
        // Empty / whitespace-only command.
        let msg = app_of("[program.api]\ncommand='x'\n\n[program.api.action.e]\ncommand = ' '\n")
            .err()
            .unwrap()
            .to_string();
        assert!(msg.contains("command must not be empty"), "{msg}");
        // Built-in action names are reserved.
        for reserved in RESERVED_ACTIONS {
            let msg = app_of(&format!(
                "[program.api]\ncommand='x'\n\n[program.api.action.{reserved}]\ncommand='x'\n"
            ))
            .err()
            .unwrap()
            .to_string();
            assert!(
                msg.contains("reserved") && msg.contains(reserved),
                "{reserved} must be rejected: {msg}"
            );
        }
        // Observe-only commands and `apply` are NOT reserved (glossary).
        assert!(
            app_of("[program.api]\ncommand='x'\n\n[program.api.action.status]\ncommand='x'\n")
                .is_ok()
        );
        assert!(
            app_of("[program.api]\ncommand='x'\n\n[program.api.action.apply]\ncommand='x'\n")
                .is_ok()
        );
        // Action names follow the same identifier charset.
        let msg = app_of(
            "[program.api]\ncommand='x'\n\n[program.api.action.\"bad.name\"]\ncommand='x'\n",
        )
        .err()
        .unwrap()
        .to_string();
        assert!(msg.contains("bad.name") && msg.contains("invalid"), "{msg}");
    }

    /// configuration: variable names in action commands are checked at
    /// resolve time; unknown-domain `${...}` stays untouched for the shell;
    /// multi-line commands pass validation.
    #[test]
    fn action_variables_validated_and_multiline_ok() {
        // Unknown program field rejected, error names the variable.
        let msg = app_of(
            "[program.api]\ncommand='x'\n\n[program.api.action.f]\ncommand = 'curl http://h/?pid=${program.api.hello}'\n",
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            msg.contains("unknown action variable") && msg.contains("program.api.hello"),
            "{msg}"
        );
        // Unknown daemon field rejected.
        assert!(app_of("[program.api]\ncommand='x'\n\n[program.api.action.d]\ncommand = 'x ${daemon.wat}'\n").is_err());
        // Incomplete known-domain references rejected.
        assert!(
            app_of(
                "[program.api]\ncommand='x'\n\n[program.api.action.i]\ncommand = 'x ${program}'\n"
            )
            .is_err()
        );
        assert!(app_of("[program.api]\ncommand='x'\n\n[program.api.action.i2]\ncommand = 'x ${program.api}'\n").is_err());
        // All documented variables validate.
        let ok = app_of(
            "[program.api]\ncommand='x'\n\n[program.api.action.v]\ncommand = '''x ${program.api.pid} ${program.api.state} ${program.api.app} ${program.api.work_dir} ${program.api.log_dir} ${app.api.path} ${daemon.pid} ${daemon.host} ${daemon.port} ${daemon.log_dir} ${daemon.app_dir}'''",
        );
        assert!(ok.is_ok(), "documented variables must validate: {ok:?}");
        // Shell-domain variables are not xkeeper's business.
        assert!(app_of("[program.api]\ncommand='x'\n\n[program.api.action.h]\ncommand = 'echo ${HOME} ${foo.bar}'\n").is_ok());
        // Multi-line command (TOML literal string) validates.
        let multi = app_of(
            "[program.api]\ncommand='x'\n\n[program.api.action.m]\ncommand = '''\necho one\necho two\n'''",
        );
        assert!(multi.is_ok(), "multi-line command must validate: {multi:?}");
        // Unterminated ${ stays verbatim (shell's business).
        assert!(substitute_vars("echo ${oops", |_| Some("X".into())) == "echo ${oops");
    }

    /// configuration: spawn-time substitution replaces known-domain variables
    /// with runtime values; a pid that is absent becomes the empty string;
    /// unknown-domain text survives for the shell.
    #[test]
    fn variable_substitution_and_empty_pid() {
        let resolve = |r: &VarRef| -> Option<String> {
            match (r.domain, r.name.as_str(), r.field.as_str()) {
                (VarDomain::Program, "api", "pid") => Some("4242".into()),
                (VarDomain::Program, "api", "state") => Some("running".into()),
                (VarDomain::Program, "api", "work_dir") => Some("/opt/api".into()),
                (VarDomain::Program, "down", "pid") => None, // not running
                (VarDomain::App, "api", "path") => Some("/opt/api/xkeeper.toml".into()),
                (VarDomain::Daemon, _, "port") => Some("7310".into()),
                _ => None,
            }
        };
        assert_eq!(
            substitute_vars(
                "curl http://127.0.0.1:${daemon.port}/?pid=${program.api.pid}",
                resolve
            ),
            "curl http://127.0.0.1:7310/?pid=4242"
        );
        // pid of a stopped program: empty string, observable in the command.
        assert_eq!(
            substitute_vars("echo stopped=[${program.down.pid}]", resolve),
            "echo stopped=[]"
        );
        // Shell variables pass through untouched.
        assert_eq!(
            substitute_vars("echo ${HOME} and ${program.api.state} and $USER", resolve),
            "echo ${HOME} and running and $USER"
        );
        // Several references in one command.
        assert_eq!(
            substitute_vars("${program.api.work_dir}:${app.api.path}", resolve),
            "/opt/api:/opt/api/xkeeper.toml"
        );
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
        // An absolute path needs a drive letter on Windows (escaped for a
        // TOML basic string).
        let ok_dir = if cfg!(windows) {
            "C:\\\\var\\\\log\\\\xkeeper"
        } else {
            "/var/log/xkeeper"
        };
        let ok: DaemonConfig =
            toml::from_str(&format!("[daemon]\nlog_dir = \"{ok_dir}\"\n")).unwrap();
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

    /// configuration: a program name duplicated across two registered apps is
    /// refused and the error names the program AND both conflicting apps
    /// (glossary 标识符不变量: program names are globally unique).
    #[test]
    fn validate_all_duplicate_program_names_both_apps() {
        let raw: AppRaw = toml::from_str("[program.dup]\ncommand='x'\n").unwrap();
        let alpha = resolve_app("alpha", Path::new("/opt/alpha/xkeeper.toml"), &raw, None).unwrap();
        let beta = resolve_app("beta", Path::new("/opt/beta/xkeeper.toml"), &raw, None).unwrap();
        let err = validate_all(&[alpha, beta])
            .err()
            .expect("duplicate refused");
        let msg = err.to_string();
        assert!(
            msg.contains("dup") && msg.contains("alpha") && msg.contains("beta"),
            "error must name the program and both apps: {msg}"
        );
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

// ---------------------------------------------------------------------------
// tests: `config` subcommand engine (add-config-subcommand)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod config_cmd_tests {
    use super::*;

    /// The key table covers exactly the 8 documented `[daemon]` keys.
    #[test]
    fn key_table_lists_all_daemon_keys() {
        let names: Vec<_> = DAEMON_KEYS.iter().map(|k| k.name).collect();
        assert_eq!(
            names,
            [
                "log_level",
                "log_dir",
                "monitor_interval",
                "host",
                "port",
                "auth_token",
                "log_buffer_lines",
                "app_dir"
            ]
        );
    }

    /// Every key accepts legal values and rejects type violations (task 2.1).
    #[test]
    fn key_parsers_accept_legal_and_reject_illegal() {
        let parse = |k: &str, v: &str| {
            let key = find_daemon_key(k).unwrap_or_else(|| panic!("{k} must be known"));
            (key.parse)(v)
        };
        // log_level: enum only.
        for v in ["trace", "debug", "info", "warn", "error"] {
            assert_eq!(parse("log_level", v).unwrap(), KeyValue::Str(v.into()));
        }
        assert!(parse("log_level", "verbose").is_err());
        assert!(parse("log_level", "INFO").is_err());
        // port: u16 1..=65535.
        assert_eq!(parse("port", "8080").unwrap(), KeyValue::Int(8080));
        assert_eq!(parse("port", "1").unwrap(), KeyValue::Int(1));
        assert_eq!(parse("port", "65535").unwrap(), KeyValue::Int(65535));
        for bad in ["abc", "0", "-1", "65536", "8080.5", ""] {
            assert!(parse("port", bad).is_err(), "port {bad:?} must be rejected");
        }
        // monitor_interval: positive float.
        assert_eq!(
            parse("monitor_interval", "0.5").unwrap(),
            KeyValue::Float(0.5)
        );
        assert_eq!(
            parse("monitor_interval", "60").unwrap(),
            KeyValue::Float(60.0)
        );
        for bad in ["abc", "0", "-1", "", "-0.5"] {
            assert!(
                parse("monitor_interval", bad).is_err(),
                "monitor_interval {bad:?} must be rejected"
            );
        }
        // log_buffer_lines: non-negative integer.
        assert_eq!(
            parse("log_buffer_lines", "1000").unwrap(),
            KeyValue::Int(1000)
        );
        assert_eq!(parse("log_buffer_lines", "0").unwrap(), KeyValue::Int(0));
        for bad in ["abc", "-1", "1.5", ""] {
            assert!(
                parse("log_buffer_lines", bad).is_err(),
                "log_buffer_lines {bad:?} must be rejected"
            );
        }
        // Strings accept anything.
        for k in ["log_dir", "host", "auth_token", "app_dir"] {
            assert_eq!(
                parse(k, "anything").unwrap(),
                KeyValue::Str("anything".into())
            );
            assert_eq!(parse(k, "").unwrap(), KeyValue::Str(String::new()));
        }
    }

    /// Effective values are typed off DaemonSettings, so file values and the
    /// built-in defaults render identically (design D3, no copied literals).
    #[test]
    fn effective_value_file_priority_and_default_fallback() {
        // Missing file == all defaults (spec: 文件缺失全默认).
        assert_eq!(config_effective_value(None, "port").unwrap(), "7310");
        assert_eq!(
            config_effective_value(None, "host").unwrap(),
            DaemonSettings::default().host
        );
        assert_eq!(
            config_effective_value(None, "log_dir").unwrap(),
            DaemonSettings::default()
                .log_dir
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(
            config_effective_value(None, "monitor_interval").unwrap(),
            "1.0"
        );
        // File value wins.
        let text = "[daemon]\nport = 8080\n";
        assert_eq!(config_effective_value(Some(text), "port").unwrap(), "8080");
        // Unset key in an existing file still falls back to the default
        // (spec: 回退内置默认).
        assert_eq!(
            config_effective_value(Some(text), "host").unwrap(),
            "127.0.0.1"
        );
        // Unknown key is rejected; an invalid file is rejected.
        assert!(config_effective_value(None, "foo").is_err());
        assert!(config_effective_value(None, "webui").is_err());
        assert!(config_effective_value(Some("bogus_field = 1"), "port").is_err());
        assert!(config_effective_value(Some("[daemon]\nlog_dir = \"rel\"\n"), "port").is_err());
    }

    /// --set keeps untouched keys' order, values and comments byte-for-byte;
    /// the touched key's same-line trailing comment survives too.
    #[test]
    fn set_preserves_untouched_comments_and_order() {
        let base = "\
# header comment
[daemon]
# first key
log_level = \"info\"
log_buffer_lines = 7          # custom order + trailing note
# the port comment
port = 1234 # trailing
app_dir = \"apps\"

[app-default]
autostart = true
";
        let rendered =
            config_apply_sets(base, &[("port".to_string(), KeyValue::Int(8080))]).unwrap();
        // The result must still be a valid config.
        load_daemon_text(&rendered).unwrap();
        // Untouched content is byte-identical.
        for snippet in [
            "# header comment",
            "# first key",
            "log_level = \"info\"",
            "log_buffer_lines = 7          # custom order + trailing note",
            "# the port comment",
            "app_dir = \"apps\"",
            "[app-default]",
            "autostart = true",
        ] {
            assert!(
                rendered.contains(snippet),
                "lost {snippet:?} in:\n{rendered}"
            );
        }
        // The value changed; the key's own trailing comment survives.
        assert!(rendered.contains("port = 8080 # trailing"), "{rendered}");
        assert!(!rendered.contains("1234"), "{rendered}");
        // Re-read: the new value is effective.
        assert_eq!(
            config_effective_value(Some(&rendered), "port").unwrap(),
            "8080"
        );
    }

    /// --set that would make the whole config invalid renders an error and
    /// the caller never writes (spec: 校验失败回滚不落盘).
    #[test]
    fn set_rejects_configs_that_fail_whole_validation() {
        let base = "[daemon]\nport = 1234\n";
        // log_dir parses as a string but violates the absolute-path rule.
        let r = config_apply_sets(
            base,
            &[("log_dir".to_string(), KeyValue::Str("relative/logs".into()))],
        );
        assert!(r.is_err(), "invalid whole config must be rejected");
        // The base itself was invalid → refuse to touch (design D2).
        assert!(
            config_apply_sets("bogus_field = 1", &[("port".to_string(), KeyValue::Int(1))])
                .is_err()
        );
    }

    /// --set creates the [daemon] table when missing; dotted-key style
    /// configs are handled like tables.
    #[test]
    fn set_creates_missing_daemon_table_and_handles_dotted_style() {
        let rendered = config_apply_sets(
            "\n[app-default]\nautostart = true\n",
            &[("port".to_string(), KeyValue::Int(1))],
        )
        .unwrap();
        assert_eq!(
            config_effective_value(Some(&rendered), "port").unwrap(),
            "1"
        );
        // Dotted style.
        let dotted = "daemon.port = 1234\n";
        let rendered =
            config_apply_sets(dotted, &[("port".to_string(), KeyValue::Int(9999))]).unwrap();
        assert_eq!(
            config_effective_value(Some(&rendered), "port").unwrap(),
            "9999"
        );
    }

    /// parse_sets validates every pair up front (all-or-nothing).
    #[test]
    fn parse_sets_is_all_or_nothing() {
        let pairs = vec!["port=8080".to_string(), "foo=1".to_string()];
        assert!(parse_sets(&pairs).is_err(), "unknown key rejects the batch");
        let pairs = vec!["port".to_string()];
        assert!(parse_sets(&pairs).is_err(), "missing '=' is rejected");
        let pairs = vec!["port=1".to_string(), "log_level=debug".to_string()];
        let out = parse_sets(&pairs).unwrap();
        assert_eq!(out[0].0, "port");
        assert_eq!(out[1].1, KeyValue::Str("debug".into()));
    }

    /// --delete removes the key (comment attached to the key goes with it),
    /// keeps everything else, and the deleted key falls back to its default.
    #[test]
    fn delete_removes_key_and_falls_back_to_default() {
        let base = "\
[daemon]
# host note
host = \"0.0.0.0\"
port = 8080
log_buffer_lines = 7
";
        let rendered = match config_apply_deletes(base, &["port".to_string()]).unwrap() {
            DeleteOutcome::Changed(t) => t,
            other => panic!("expected Changed, got {other:?}"),
        };
        for snippet in ["# host note", "host = \"0.0.0.0\"", "log_buffer_lines = 7"] {
            assert!(
                rendered.contains(snippet),
                "lost {snippet:?} in:\n{rendered}"
            );
        }
        assert!(!rendered.contains("port"), "{rendered}");
        assert_eq!(
            config_effective_value(Some(&rendered), "port").unwrap(),
            "7310",
            "deleted key falls back to the built-in default"
        );
        load_daemon_text(&rendered).unwrap();
    }

    /// Deleting an absent key is an idempotent NoChange (file untouched).
    #[test]
    fn delete_absent_key_is_idempotent() {
        let base = "[daemon]\nport = 8080\n";
        assert_eq!(
            config_apply_deletes(base, &["host".to_string()]).unwrap(),
            DeleteOutcome::NoChange
        );
        // No [daemon] table at all: still a NoChange.
        assert_eq!(
            config_apply_deletes("[app-default]\nautostart = true\n", &["port".to_string()])
                .unwrap(),
            DeleteOutcome::NoChange
        );
    }

    /// Delete targets resolve through the whitelist; unknown keys/tables are
    /// rejected (spec: 未知键/未知表拒绝). The presence-based `[webui]`
    /// section deletes as a whole with its bare table name.
    #[test]
    fn delete_resolves_paths_and_rejects_unknowns() {
        // Bare key ≡ daemon.port.
        let rendered =
            match config_apply_deletes("[daemon]\nport = 1\n", &["port".to_string()]).unwrap() {
                DeleteOutcome::Changed(t) => t,
                other => panic!("expected Changed, got {other:?}"),
            };
        assert!(!rendered.contains("port"), "{rendered}");
        // Explicit dotted form works too.
        let r = config_apply_deletes("[daemon]\nport = 1\n", &["daemon.port".to_string()]);
        assert!(matches!(r, Ok(DeleteOutcome::Changed(_))), "{r:?}");
        // `--delete webui` removes the whole [webui] section (console off).
        let base = "[daemon]\nport = 1\n\n[webui]\n# console note\nlisten = \"127.0.0.1:9877\"\n";
        let rendered =
            match config_apply_deletes(base, &["webui".to_string()]).unwrap() {
                DeleteOutcome::Changed(t) => t,
                other => panic!("expected Changed, got {other:?}"),
            };
        assert!(!rendered.contains("webui") && !rendered.contains("listen"), "{rendered}");
        assert!(rendered.contains("port = 1"), "untouched [daemon] kept: {rendered}");
        assert!(
            !rendered.contains("# console note"),
            "the section's comments go with it: {rendered}"
        );
        // Idempotent when the section is absent.
        assert_eq!(
            config_apply_deletes("[daemon]\nport = 1\n", &["webui".to_string()]).unwrap(),
            DeleteOutcome::NoChange
        );
        // Unknown leaf, unknown table, bare table name, deeper path.
        for bad in [
            "foo",
            "webui.theme",
            "webui.listen.x",
            "daemon",
            "daemon.port.x",
            "daemon.foo",
        ] {
            let r = config_apply_deletes("[daemon]\nport = 1\n", &[bad.to_string()]);
            assert!(r.is_err(), "{bad:?} must be rejected");
        }
        // Rejected batches leave nothing half-done (all-or-nothing).
        let pairs = vec!["port".to_string(), "foo".to_string()];
        let r = config_apply_deletes("[daemon]\nport = 1\n", &pairs);
        assert!(r.is_err(), "one unknown key rejects the whole batch");
        // An invalid base file is refused before anything is removed.
        assert!(config_apply_deletes("bogus = 1", &["port".to_string()]).is_err());
    }

    /// --delete is repeatable: one call removes several targets (bare and
    /// dotted forms mixed) and the dotted-key file style (an inline table)
    /// is handled too; untouched keys survive byte-for-byte.
    #[test]
    fn delete_multiple_targets_in_one_call() {
        // Table style, mixed bare + dotted targets.
        let base = "\
[daemon]
# port note
port = 8080
host = \"0.0.0.0\"
log_buffer_lines = 7
";
        let rendered = match config_apply_deletes(
            base,
            &["port".to_string(), "daemon.host".to_string()],
        ) {
            Ok(DeleteOutcome::Changed(t)) => t,
            other => panic!("expected Changed, got {other:?}"),
        };
        assert!(!rendered.contains("port"), "{rendered}");
        assert!(!rendered.contains("host"), "{rendered}");
        assert!(
            rendered.contains("log_buffer_lines = 7"),
            "the untouched key survives: {rendered}"
        );
        load_daemon_text(&rendered).unwrap();

        // Dotted-key style (parses as an inline table): both targets go.
        let dotted = "daemon.port = 8080\ndaemon.host = \"0.0.0.0\"\n";
        let rendered = match config_apply_deletes(
            dotted,
            &["port".to_string(), "host".to_string()],
        ) {
            Ok(DeleteOutcome::Changed(t)) => t,
            other => panic!("expected Changed, got {other:?}"),
        };
        assert!(!rendered.contains("port") && !rendered.contains("host"), "{rendered}");
        load_daemon_text(&rendered).unwrap();
    }

    /// The init template loads as a valid daemon config whose values equal
    /// the built-in defaults (task 3.1; design D5/D3 single source).
    #[test]
    fn init_template_renders_defaults_and_loads() {
        let text = render_init_template();
        let cfg: DaemonConfig = toml::from_str(&text).expect("template must parse");
        cfg.validate().expect("template must validate");
        let d = DaemonSettings::default();
        assert_eq!(cfg.daemon.log_level, d.log_level);
        assert_eq!(cfg.daemon.log_dir, d.log_dir);
        assert_eq!(cfg.daemon.monitor_interval, d.monitor_interval);
        assert_eq!(cfg.daemon.host, d.host);
        assert_eq!(cfg.daemon.port, d.port);
        assert_eq!(cfg.daemon.auth_token, d.auth_token);
        assert_eq!(cfg.daemon.log_buffer_lines, d.log_buffer_lines);
        assert_eq!(cfg.daemon.app_dir, d.app_dir);
        assert!(cfg.app_default.is_none(), "app-default stays commented out");
        // Comments survive the round-trip (init is the first-contact docs).
        assert!(text.contains("# [app-default]"));
        assert!(text.contains("auth_token = \"\""));
        assert!(text.contains("app_dir = \"apps\""));
        assert!(
            text.contains("resolves against the directory of THIS config file"),
            "app_dir relative-resolution note present"
        );
    }

    /// config --init creates the config + app dir + sample; a re-init on an
    /// existing config is refused by the caller; a missing sample is written
    /// but an existing one is kept untouched (spec: 重复 init 幂等安全).
    #[test]
    fn init_creates_workspace_and_skips_existing_sample() {
        let tmp = std::env::temp_dir().join(format!("xk-cfg-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cfg_path = tmp.join("conf").join("daemon.toml"); // nested parents
        let rep = config_init(&cfg_path).unwrap();
        assert!(cfg_path.is_file());
        let d = DaemonSettings::default();
        let want_app_dir = resolve_path(&d.app_dir, cfg_path.parent().unwrap());
        assert_eq!(
            rep.app_dir, want_app_dir,
            "app_dir resolves like the daemon"
        );
        assert!(rep.app_dir.is_dir());
        assert!(rep.sample_created);
        let sample = tmp.join("conf/apps/example.toml.sample");
        assert_eq!(rep.sample_file, sample);
        assert!(sample.is_file());
        let sample_text = std::fs::read_to_string(&sample).unwrap();
        assert!(sample_text.contains("[program.main]"), "{sample_text}");
        // Second run (config file deleted, sample kept): sample untouched.
        std::fs::remove_file(&cfg_path).unwrap();
        let rep2 = config_init(&cfg_path).unwrap();
        assert!(!rep2.sample_created, "existing sample is skipped");
        assert_eq!(
            std::fs::read_to_string(&sample).unwrap(),
            sample_text,
            "the sample was not rewritten"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The `.sample` suffix keeps the example template out of the registry
    /// scan (task 3.3): a directory holding only the sample lists no apps.
    #[test]
    fn sample_suffix_is_ignored_by_registry_scan() {
        let tmp = std::env::temp_dir().join(format!("xk-cfg-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cfg_path = tmp.join("daemon.toml");
        let rep = config_init(&cfg_path).unwrap();
        let (config, _) = DaemonConfig::load_or_default(&cfg_path).unwrap();
        // Point the scan at the init-produced app_dir.
        let mut config = config;
        config.daemon.app_dir = rep.app_dir.clone();
        let listed = crate::registry::list(&config, cfg_path.parent().unwrap()).unwrap();
        assert!(
            listed.is_empty(),
            "example.toml.sample must not be scanned as an app: {listed:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The init template renders string values through toml_edit, so a
    /// Windows-style default log_dir (backslashes) round-trips as a valid
    /// TOML basic string (task 5.3: platform-different defaults render
    /// correctly; verifiable cross-platform without a Windows toolchain).
    #[test]
    fn init_template_escapes_windows_style_paths() {
        let win_path = r"C:\Users\x\AppData\Local\Temp\xkeeper\logs";
        let rendered = toml_edit::Value::from(win_path.to_string()).to_string();
        let doc: toml_edit::DocumentMut = format!("[daemon]\nlog_dir = {rendered}\n")
            .parse()
            .expect("the escaped path must be valid TOML");
        assert_eq!(doc["daemon"]["log_dir"].as_str(), Some(win_path));
    }

    /// Atomic write: an invalid payload never reaches the target and leaves
    /// no temp file behind (design D4).
    #[test]
    fn atomic_write_rejects_invalid_and_leaves_no_tmp() {
        let tmp = std::env::temp_dir().join(format!("xk-cfg-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg_path = tmp.join("daemon.toml");
        std::fs::write(&cfg_path, "[daemon]\nport = 1\n").unwrap();

        let bad = "[daemon]\nport = 1\nbogus_field = 2\n";
        let r = atomic_write_daemon_config(&cfg_path, bad);
        assert!(r.is_err(), "invalid payload must be rejected");
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            "[daemon]\nport = 1\n",
            "the target was not modified"
        );
        assert!(
            !tmp.join("daemon.toml.tmp").exists(),
            "no temp file left behind"
        );

        // A valid payload replaces the target and cleans up the temp file.
        atomic_write_daemon_config(&cfg_path, "[daemon]\nport = 8080\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            "[daemon]\nport = 8080\n"
        );
        assert!(!tmp.join("daemon.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- [webui] section (config-driven-webui, tasks 1.1/1.2/1.3) -------------

    /// The `[webui]` section is presence-based: absent = None, present with
    /// only a comment/empty body = default listen; unknown keys are rejected
    /// at parse time; an illegal listen fails whole-config validation.
    #[test]
    fn webui_section_parse_and_validate() {
        // No section: disabled.
        let none: DaemonConfig = toml::from_str("[daemon]\nport = 1\n").unwrap();
        assert!(none.webui.is_none());
        assert_eq!(none.webui_listen(), None);
        assert!(none.validate().is_ok());
        // Empty section (bare header): enabled with the default listen.
        let empty: DaemonConfig = toml::from_str("[webui]\n").unwrap();
        assert_eq!(empty.webui.as_ref().unwrap().listen, DEFAULT_WEBUI_LISTEN);
        // Explicit value round-trips.
        let explicit: DaemonConfig =
            toml::from_str("[webui]\nlisten = \"0.0.0.0:8080\"\n").unwrap();
        assert_eq!(explicit.webui_listen(), Some("0.0.0.0:8080"));
        assert!(explicit.validate().is_ok());
        // Unknown keys inside the section are rejected (deny_unknown_fields).
        let r: Result<DaemonConfig, _> = toml::from_str("[webui]\nauth = true\n");
        assert!(r.is_err(), "unknown [webui] key must be rejected");
        // Illegal listen values fail validation and name the field.
        for bad in ["9877", "", " ", ":9877", "127.0.0.1:0", "127.0.0.1:99999", "127.0.0.1:x"] {
            let cfg: DaemonConfig =
                toml::from_str(&format!("[webui]\nlisten = \"{bad}\"\n")).unwrap();
            let err = cfg.validate().unwrap_err().to_string();
            assert!(
                err.contains("webui.listen"),
                "illegal listen {bad:?} must name the field: {err}"
            );
        }
        // The shared gate accepts host:port forms (ipv6 bracket host too).
        for ok in ["127.0.0.1:9877", "0.0.0.0:80", "[::1]:9877", "localhost:1"] {
            assert!(
                validate_listen_addr(ok).is_ok(),
                "{ok:?} must be accepted"
            );
        }
    }

    /// `--get webui.listen`: the built-in default answers when the file or the
    /// section is missing; the file value wins when configured.
    #[test]
    fn get_webui_listen_falls_back_to_default() {
        // Missing file → default.
        assert_eq!(
            config_effective_value(None, "webui.listen").unwrap(),
            "127.0.0.1:9877"
        );
        // Existing file without the section → default (console off, but the
        // key still answers with its default).
        assert_eq!(
            config_effective_value(Some("[daemon]\nport = 1\n"), "webui.listen").unwrap(),
            "127.0.0.1:9877"
        );
        // Section present → its value.
        assert_eq!(
            config_effective_value(
                Some("[webui]\nlisten = \"0.0.0.0:8080\"\n"),
                "webui.listen"
            )
            .unwrap(),
            "0.0.0.0:8080"
        );
        // Unknown [webui] leaf / bare table name are rejected for --get.
        assert!(config_effective_value(None, "webui.theme").is_err());
        assert!(config_effective_value(None, "webui").is_err());
        // daemon.<leaf> dotted form works for --get too.
        assert_eq!(
            config_effective_value(Some("[daemon]\nport = 1\n"), "daemon.port").unwrap(),
            "1"
        );
    }

    /// `--set webui.listen` writes into the `[webui]` section, creating it
    /// when missing (写入即开启); an illegal value is rejected by the parser
    /// before anything is written, and an existing section keeps its comments.
    #[test]
    fn set_webui_listen_creates_section_and_validates() {
        // Creates the section on a config without one.
        let rendered = config_apply_sets(
            "[daemon]\nport = 1234\n",
            &[("webui.listen".to_string(), KeyValue::Str("127.0.0.1:9877".into()))],
        )
        .unwrap();
        assert!(rendered.contains("[webui]"), "section created: {rendered}");
        assert!(rendered.contains("listen = \"127.0.0.1:9877\""), "{rendered}");
        load_daemon_text(&rendered).unwrap();
        assert_eq!(
            config_effective_value(Some(&rendered), "webui.listen").unwrap(),
            "127.0.0.1:9877"
        );
        // Existing section: only the value node changes, comments survive.
        let base = "[webui]\n# console note\nlisten = \"127.0.0.1:1\" # trailing\n";
        let rendered = config_apply_sets(
            base,
            &[("webui.listen".to_string(), KeyValue::Str("0.0.0.0:9".into()))],
        )
        .unwrap();
        assert!(rendered.contains("# console note"), "{rendered}");
        assert!(rendered.contains("listen = \"0.0.0.0:9\" # trailing"), "{rendered}");
        // The daemon table is untouched.
        let rendered = config_apply_sets(
            "[daemon]\nport = 1234\n",
            &[
                ("port".to_string(), KeyValue::Int(4321)),
                ("webui.listen".to_string(), KeyValue::Str("127.0.0.1:9877".into())),
            ],
        )
        .unwrap();
        assert!(rendered.contains("port = 4321") && rendered.contains("[webui]"), "{rendered}");
        // parse_sets routes the webui key through the strong-typed parser.
        let parsed = parse_sets(&["webui.listen=9877".to_string()]).is_err();
        assert!(parsed, "a host-less listen is rejected at parse time");
        let parsed = parse_sets(&["webui.listen=".to_string()]).is_err();
        assert!(parsed, "an empty listen is rejected at parse time");
        let parsed = parse_sets(&["webui.listen=127.0.0.1:9877".to_string()]).unwrap();
        assert_eq!(parsed[0].0, "webui.listen");
        // Unknown keys under the section are rejected.
        assert!(parse_sets(&["webui.theme=dark".to_string()]).is_err());
        // Surrounding whitespace would pass a trim-based check but fail the
        // later TcpListener::bind — rejected at both gates.
        assert!(parse_sets(&["webui.listen= 127.0.0.1:9877".to_string()]).is_err());
        assert!(load_daemon_text("[daemon]\n[webui]\nlisten = \" 127.0.0.1:9877\"\n").is_err());
    }

    /// `--delete webui.listen` removes the leaf; the section (if otherwise
    /// empty) means "enabled with the default listen" — the off switch is the
    /// whole-table delete, so this asserts the leaf path keeps that semantics.
    #[test]
    fn delete_webui_listen_leaf() {
        let base = "[webui]\n# note\nlisten = \"127.0.0.1:9877\"\n";
        let rendered =
            match config_apply_deletes(base, &["webui.listen".to_string()]).unwrap() {
                DeleteOutcome::Changed(t) => t,
                other => panic!("expected Changed, got {other:?}"),
            };
        assert!(!rendered.contains("127.0.0.1:9877"), "{rendered}");
        // The empty section still parses (presence = enabled, default listen).
        let cfg: DaemonConfig = toml::from_str(&rendered).unwrap();
        assert!(cfg.webui.is_some());
        assert_eq!(cfg.webui_listen(), Some(DEFAULT_WEBUI_LISTEN));
        // Dotted-key style file.
        let dotted = "webui.listen = \"127.0.0.1:1\"\n";
        let rendered =
            match config_apply_deletes(dotted, &["webui.listen".to_string()]).unwrap() {
                DeleteOutcome::Changed(t) => t,
                other => panic!("expected Changed, got {other:?}"),
            };
        assert!(!rendered.contains("webui"), "{rendered}");
    }

    /// The `--init` template does not render the `[webui]` section (task 1.3:
    /// the console stays off by default); the product loads clean and parses
    /// with no webui section.
    #[test]
    fn init_template_has_no_webui_section() {
        let text = render_init_template();
        let cfg: DaemonConfig = toml::from_str(&text).expect("template must parse");
        cfg.validate().expect("template must validate");
        assert!(cfg.webui.is_none(), "init must not enable the console");
        assert!(
            !text.contains("[webui]\n"),
            "no rendered [webui] section in the template:\n{text}"
        );
        // The opt-in hint is present (first-contact discoverability).
        assert!(text.contains("[webui]"), "the template explains the opt-in");
        // config --init over the real filesystem loads with no webui section.
        let tmp = std::env::temp_dir().join(format!("xk-cfg-init-webui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cfg_path = tmp.join("daemon.toml");
        config_init(&cfg_path).unwrap();
        let (loaded, existed) = DaemonConfig::load_or_default(&cfg_path).unwrap();
        assert!(existed && loaded.webui.is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
