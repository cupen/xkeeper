//! App registry: the set of registered apps lives as `<name>.toml` links in
//! the daemon-configured `app_dir`. add/remove/list manage those links; the
//! linked deployment file is always the source of truth.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use log::{debug, warn};

use crate::config::{
    AppDefaults, AppMeta, AppRaw, DaemonConfig, ProgramRaw, RestartPolicy, is_valid_name,
    resolve_app, split_command, validate_all,
};

#[derive(Debug, Clone)]
pub struct ListedApp {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    /// Set when the config file exists but is broken (parse/read failure).
    /// Detection isolates such apps instead of treating them as removed.
    pub broken: Option<String>,
}

impl ListedApp {
    /// Only the apps whose config loaded cleanly.
    pub fn good(apps: Vec<ListedApp>) -> Vec<ListedApp> {
        apps.into_iter().filter(|a| a.broken.is_none()).collect()
    }
}

/// `<app_dir>/<name>.toml` for a given app name.
pub fn link_path(config: &DaemonConfig, config_dir: &Path, name: &str) -> PathBuf {
    crate::config::resolve_path(&config.daemon.app_dir, config_dir).join(format!("{name}.toml"))
}

/// The resolved `app_dir` directory.
pub fn app_dir(config: &DaemonConfig, config_dir: &Path) -> PathBuf {
    crate::config::resolve_path(&config.daemon.app_dir, config_dir)
}

/// Derive the app name from a config path's parent directory.
pub fn name_from_dir(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "app".to_string())
}

/// Scan `app_dir` and list every registered app. Dangling links are warned
/// about and skipped.
pub fn list(config: &DaemonConfig, config_dir: &Path) -> Result<Vec<ListedApp>> {
    let dir = app_dir(config, config_dir);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
        .collect();
    entries.sort();
    for link in entries {
        let name = link
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let real = std::fs::canonicalize(&link).with_context(|| {
            format!(
                "dangling registration link {} (run `xkeeper remove {name}`)",
                link.display()
            )
        });
        let real = match real {
            Ok(r) => r,
            Err(e) => {
                warn!("{e:#}");
                continue;
            }
        };
        match std::fs::read_to_string(&real) {
            Ok(text) => match toml::from_str::<crate::config::AppRaw>(&text) {
                Ok(raw) => out.push(ListedApp {
                    description: raw.app.as_ref().and_then(|m| m.description.clone()),
                    name,
                    path: real,
                    broken: None,
                }),
                // Keep the entry so the supervisor can isolate it explicitly
                // (broken file ≠ unregistered); non-supervisor callers filter.
                Err(e) => {
                    warn!("registered app {name:?} fails to parse: {e}");
                    out.push(ListedApp {
                        description: None,
                        name,
                        path: real,
                        broken: Some(format!("app config fails to parse: {e}")),
                    })
                }
            },
            Err(e) => {
                warn!("registered app {name:?} cannot be read: {e}");
                out.push(ListedApp {
                    description: None,
                    name,
                    path: real,
                    broken: Some(format!("app config cannot be read: {e}")),
                })
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Default, Clone)]
pub struct AddOptions {
    pub name: Option<String>,
    pub description: Option<String>,
    pub autostart: Option<bool>,
    pub autorestart: Option<RestartPolicy>,
    pub restart_backoff: Option<f64>,
    pub priority: Option<i32>,
    /// Scaffold source only: startup args as one shell-lexed string
    /// (split with `config::split_command`, same rules as a single-line
    /// `command` in a config file).
    pub args: Option<String>,
    /// Scaffold source only: `--env K=V` pairs (repeatable; last wins).
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Scaffold source only: working directory override. Default is the
    /// current directory at `add` time (written as an absolute path).
    pub workdir: Option<PathBuf>,
}

/// What one `registry::add` call did — everything the CLI prints.
#[derive(Debug, Clone)]
pub struct AddResult {
    pub app: String,
    /// The registration record path: the `app_dir` link (dir/.toml flow)
    /// or the generated config file (scaffold flow).
    pub file: PathBuf,
    /// True when the record was scaffold-generated from an executable.
    pub scaffolded: bool,
    /// Scaffold only: the regenerated file differed from the disk content
    /// (a first generation counts as changed). `false` → no change.
    pub changed: bool,
    /// Scaffold only: resolved program values for the CLI summary.
    pub program: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub work_dir: PathBuf,
}

impl AddResult {
    fn registered(app: String, file: PathBuf) -> Self {
        Self {
            app,
            file,
            scaffolded: false,
            changed: false,
            program: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            work_dir: PathBuf::new(),
        }
    }
}

/// Reject names that would collide with CLI keywords. `all` is the
/// apply-all keyword (`xkeeper apply all`), so it can never be an app name
/// (apply-workflow spec).
fn check_name(name: &str) -> Result<()> {
    if !is_valid_name(name) {
        bail!("app name {name:?} is not filename-safe");
    }
    if name == crate::supervisor::ALL_KEYWORD {
        bail!(
            "app name {name:?} is reserved: `xkeeper apply all` means apply-all; \
             choose another --name"
        );
    }
    Ok(())
}

/// Register an app: validate it, create the `app_dir` link, and write the
/// micro-tuning flags into the deployment file's `[app]` table. Idempotent
/// (upsert by name). Dispatches by path shape:
///
/// - directory (containing `xkeeper.toml`) → existing directory flow;
/// - `.toml` file → existing config-file registration flow;
/// - any other regular file (an executable program) → scaffold generation:
///   render a single-program `AppRaw` and write it as a REAL file
///   `<app_dir>/<name>.toml` (no link — the file IS the record). Re-running
///   the same add fully regenerates the file from the given flags.
pub fn add(
    config: &DaemonConfig,
    config_dir: &Path,
    path: &Path,
    opts: &AddOptions,
) -> Result<AddResult> {
    if path.is_dir() {
        let f = path.join("xkeeper.toml");
        if !f.exists() {
            bail!("no xkeeper.toml found in directory {}", path.display());
        }
        return add_config(config, config_dir, &f, opts);
    }
    if path.is_file() {
        if path.extension().map(|x| x == "toml").unwrap_or(false) {
            return add_config(config, config_dir, path, opts);
        }
        return add_scaffold(config, config_dir, path, opts);
    }
    bail!("app config not found: {}", path.display())
}

/// The existing registration flow (validate → write flags → link).
fn add_config(
    config: &DaemonConfig,
    config_dir: &Path,
    path: &Path,
    opts: &AddOptions,
) -> Result<AddResult> {
    // Canonicalize so relative paths (e.g. `add .`) still yield a proper
    // directory name and stable link targets.
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    let name = match &opts.name {
        Some(n) => n.clone(),
        None => name_from_dir(&path),
    };
    check_name(&name)?;

    // Load + validate this app against the current registry.
    let (raw, _) = crate::config::AppRaw::load(&path)?;
    let resolved = resolve_app(&name, &path, &raw, config.app_default.as_ref())?;
    let existing = list(config, config_dir)?;
    let mut all_apps = Vec::new();
    for l in &existing {
        if l.name == name {
            continue; // replaced by the new definition below
        }
        let (r, _) = crate::config::AppRaw::load(&l.path)?;
        all_apps.push(resolve_app(
            &l.name,
            &l.path,
            &r,
            config.app_default.as_ref(),
        )?);
    }
    all_apps.push(resolved.clone());
    validate_all(&all_apps)?;

    // Write flags into the deployment file's [app] table.
    apply_flags(&path, opts)
        .with_context(|| format!("cannot write tuning flags into {}", path.display()))?;

    // Create the registration link (this IS the registry record — the
    // daemon discovers apps by scanning app_dir, so a command that leaves
    // no link behind must not report success).
    let dir = app_dir(config, config_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create app_dir {}", dir.display()))?;
    let link = link_path(config, config_dir, &name);
    let _ = std::fs::remove_file(&link); // upsert: replace any old link
    if let Err(e) = make_link(&link, &path) {
        bail!(
            "cannot write the registration link into app_dir {}: {e:#}; \
             fix write access (re-run with sudo, or chown the app_dir to the current user) \
             and run `xkeeper add` again",
            dir.display()
        );
    }
    debug!(
        "app[{name}] registered from {} (link: {})",
        path.display(),
        link.display()
    );
    Ok(AddResult::registered(name, link))
}

/// The executable-derived values one scaffold generation renders.
struct ScaffoldSpec {
    command: PathBuf,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    work_dir: PathBuf,
}

/// Scaffold flow: render a single-program app config from an executable and
/// write it as a REAL file `<app_dir>/<name>.toml` (the record is the file
/// itself, no link). Same-name re-add fully regenerates the file from the
/// given flags — validation must pass before anything is written, and the
/// new text is compared against the disk content so the CLI can tell
/// "changed" from "no change".
fn add_scaffold(
    config: &DaemonConfig,
    config_dir: &Path,
    exe: &Path,
    opts: &AddOptions,
) -> Result<AddResult> {
    let exe = exe
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", exe.display()))?;
    let name = match &opts.name {
        Some(n) => n.clone(),
        None => exe
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .context("cannot derive an app name from the executable path; pass --name")?,
    };
    check_name(&name)?;

    let dir = app_dir(config, config_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create app_dir {}", dir.display()))?;
    let dir_canon = dir.canonicalize()?;
    let target = link_path(config, config_dir, &name);
    // Never write through a registration that lives outside app_dir: a
    // link pointing at an external deployment file would clobber that file
    // on regeneration.
    if let Ok(md) = std::fs::symlink_metadata(&target) {
        if md.is_symlink() {
            match target.canonicalize() {
                Ok(real) if real.parent() == Some(&dir_canon) => {} // our own generation
                Ok(real) => bail!(
                    "app {name:?} is already registered from {} — run `xkeeper remove {name}` \
                     first (scaffolding would overwrite that file)",
                    real.display()
                ),
                Err(_) => bail!(
                    "stale registration {} cannot be resolved — run `xkeeper remove {name}` first",
                    target.display()
                ),
            }
        } else if file_link_count(&target, &md) > 1 {
            // A plain multi-link file in app_dir is a hard link (the
            // Windows `make_link` fallback) — its other name may be an
            // external config the write would clobber.
            bail!(
                "app {name:?} is registered via a hard link (possibly to an external config) — \
                 run `xkeeper remove {name}` before scaffolding over it"
            );
        }
    }

    // Working directory: explicit --workdir wins, else the directory the
    // add command ran in; always written as an absolute path.
    let work_dir = match &opts.workdir {
        Some(w) => display_absolute(w.canonicalize().unwrap_or_else(|_| absolutize(w))),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    warn_if_not_executable(&exe);
    let spec = ScaffoldSpec {
        command: display_absolute(exe),
        args: opts.args.as_deref().map(split_command).unwrap_or_default(),
        env: opts.env.clone().unwrap_or_default(),
        work_dir,
    };

    let text = render_scaffold(&name, &spec, opts)?;

    // Validate exactly what will land on disk (round-trip the rendered
    // text) against the current registry, before any write.
    let raw: AppRaw = toml::from_str(&text)
        .with_context(|| format!("rendered scaffold for app {name:?} is not valid TOML"))?;
    let resolved = resolve_app(&name, &target, &raw, config.app_default.as_ref())?;
    let existing = list(config, config_dir)?;
    let mut all_apps = Vec::new();
    for l in &existing {
        if l.name == name {
            continue; // replaced by the regeneration below
        }
        let (r, _) = AppRaw::load(&l.path)?;
        all_apps.push(resolve_app(
            &l.name,
            &l.path,
            &r,
            config.app_default.as_ref(),
        )?);
    }
    all_apps.push(resolved);
    validate_all(&all_apps)?;

    // Change detection on the rendered text (a first generation counts as
    // changed); nothing is written when the content is identical.
    let changed = std::fs::read_to_string(&target)
        .map(|old| old != text)
        .unwrap_or(true);
    if changed {
        std::fs::write(&target, &text)
            .with_context(|| format!("failed to write {}", target.display()))?;
    }
    debug!(
        "app[{name}] scaffold {} (changed: {changed})",
        target.display()
    );
    Ok(AddResult {
        app: name.clone(),
        file: absolutize(&target),
        scaffolded: true,
        changed,
        program: name,
        command: spec.command.display().to_string(),
        args: spec.args,
        env: spec.env,
        work_dir: spec.work_dir,
    })
}

/// `p` joined onto the current directory when relative.
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Number of directory entries pointing at the file's content (hard links).
#[cfg(unix)]
fn file_link_count(_target: &Path, md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.nlink()
}

/// windows: `MetadataExt::number_of_links` is still unstable, so query the
/// file handle directly (`GetFileInformationByHandle`).
#[cfg(windows)]
fn file_link_count(target: &Path, _md: &std::fs::Metadata) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let Ok(f) = std::fs::File::open(target) else {
        return 1;
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // RawHandle and windows-sys HANDLE are both `*mut c_void`.
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) };
    if ok != 0 {
        info.nNumberOfLinks as u64
    } else {
        1
    }
}

/// Windows `canonicalize` yields verbatim (`\\?\C:\...`) paths; strip the
/// prefix from plain drive paths so generated configs stay readable (the
/// result is still absolute).
fn display_absolute(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            let bytes = rest.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                return PathBuf::from(rest.to_string());
            }
        }
    }
    p
}

/// unix: a scaffolded program without any execute bit is almost certainly a
/// start failure waiting to happen — warn (add still succeeds; Windows has
/// no such concept and stays silent).
#[cfg(unix)]
fn warn_if_not_executable(exe: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let lacks = std::fs::metadata(exe)
        .map(|md| md.mode() & 0o111 == 0)
        .unwrap_or(false);
    if lacks {
        warn!(
            "{} has no execute bit (chmod +x); the program may fail to start",
            exe.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_not_executable(_exe: &Path) {}

/// Render the scaffold TOML: header comments + `toml::to_string_pretty` of
/// the single-program `AppRaw` (guaranteed re-readable). When the command
/// path contains whitespace it is rendered as a single-line quoted command
/// (quote-aware splitting recovers it — see `config::split_command`).
fn render_scaffold(name: &str, spec: &ScaffoldSpec, opts: &AddOptions) -> Result<String> {
    let command_str = spec.command.display().to_string();
    let (command, args) = if command_str.contains(char::is_whitespace) {
        (render_command_line(&command_str, &spec.args)?, None)
    } else {
        (
            command_str,
            if spec.args.is_empty() {
                None
            } else {
                Some(spec.args.clone())
            },
        )
    };
    let program = ProgramRaw {
        command: Some(command),
        args,
        work_dir: Some(spec.work_dir.clone()),
        env: if spec.env.is_empty() {
            None
        } else {
            Some(spec.env.clone())
        },
        ..Default::default()
    };
    let raw = AppRaw {
        app: Some(AppMeta {
            description: opts.description.clone(),
            autostart: opts.autostart,
            autorestart: opts.autorestart,
            restart_backoff: opts.restart_backoff,
            priority: opts.priority,
            ..Default::default()
        }),
        program: std::collections::BTreeMap::from([(name.to_string(), program)]),
    };
    let body = toml::to_string_pretty(&raw).context("failed to render scaffold config")?;
    Ok(format!(
        "# Generated by `xkeeper add <executable>` — scaffold for app {name:?}.\n\
         # Re-running the same `xkeeper add` regenerates this whole file (manual edits are overwritten).\n\
         # After hand edits run `xkeeper apply {name}` (or bare `xkeeper apply`) to take effect.\n\
         \n{body}"
    ))
}

/// Render `command args...` as one single-line command the quote-aware
/// splitter turns back into the same argv (paths/args containing whitespace
/// get double-quoted; embedded quotes cannot be represented).
fn render_command_line(command: &str, args: &[String]) -> Result<String> {
    let mut parts = Vec::with_capacity(args.len() + 1);
    for s in std::iter::once(command).chain(args.iter().map(|s| s.as_str())) {
        if s.contains('"') {
            bail!(
                "cannot represent {s:?} in a single-line command (quote character); \
                 edit the generated config by hand"
            );
        }
        if s.chars().any(|c| c.is_whitespace()) {
            parts.push(format!("\"{s}\""));
        } else {
            parts.push(s.to_string());
        }
    }
    Ok(parts.join(" "))
}

/// What one `registry::remove` call did.
#[derive(Debug, Clone)]
pub struct RemovedApp {
    /// The config the registration pointed at — or, when `deleted_file` is
    /// true, the removed record itself (it lived inside `app_dir`).
    pub path: PathBuf,
    /// True when the registration record was a real file inside `app_dir`
    /// (scaffold-generated): the config file was deleted along with the
    /// registration and `path` no longer exists. Link registrations keep
    /// the external deployment file untouched.
    pub deleted_file: bool,
}

/// Remove an app registration: delete the `app_dir` record. A link record
/// is removed while the external deployment file is kept; a record that is
/// a real file inside `app_dir` (scaffold-generated) is deleted with the
/// registration. The criterion is the path, not the link kind — Windows
/// `make_link` falls back to hard links, which symlink probes can't see.
pub fn remove(config: &DaemonConfig, config_dir: &Path, name: &str) -> Result<RemovedApp> {
    let link = link_path(config, config_dir, name);
    if !link.exists() {
        bail!(
            "app {name:?} is not registered (no link at {})",
            link.display()
        );
    }
    let real_canon = std::fs::canonicalize(&link).unwrap_or_else(|_| link.clone());
    std::fs::remove_file(&link)
        .with_context(|| format!("failed to remove registration link {}", link.display()))?;
    let dir_canon = app_dir(config, config_dir).canonicalize().ok();
    let deleted_file = dir_canon
        .map(|d| real_canon.parent() == Some(&d))
        .unwrap_or(false);
    let real = display_absolute(real_canon);
    if deleted_file {
        debug!(
            "app[{name}] unregistered (generated config removed: {})",
            real.display()
        );
    } else {
        debug!(
            "app[{name}] unregistered (config kept at {})",
            real.display()
        );
    }
    Ok(RemovedApp {
        path: real,
        deleted_file,
    })
}

fn make_link(link: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => Ok(()),
            Err(e1) => {
                // Symlinks need privileges on Windows; fall back to a hard
                // link (same volume only), then give up with a warning.
                std::fs::hard_link(target, link)
                    .with_context(|| {
                        format!(
                            "symlink failed ({e1}); hard link also failed — is the file on another volume?"
                        )
                    })
            }
        }
    }
}

/// Merge tuning flags into the deployment file's `[app]` table, preserving
/// formatting and comments via toml_edit. Idempotent upsert semantics.
fn apply_flags(path: &Path, opts: &AddOptions) -> Result<()> {
    let needs_write = opts.description.is_some()
        || opts.autostart.is_some()
        || opts.autorestart.is_some()
        || opts.restart_backoff.is_some()
        || opts.priority.is_some();
    if !needs_write {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let app_tbl = doc
        .entry("app")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .context("[app] entry is not a table")?;
    if let Some(d) = &opts.description {
        app_tbl["description"] = toml_edit::value(d.clone());
    }
    if let Some(v) = opts.autostart {
        app_tbl["autostart"] = toml_edit::value(v);
    }
    if let Some(v) = opts.autorestart {
        app_tbl["autorestart"] = toml_edit::value(match v {
            RestartPolicy::Always => "always",
            RestartPolicy::OnFailure => "on-failure",
            RestartPolicy::Never => "never",
        });
    }
    if let Some(v) = opts.restart_backoff {
        app_tbl["restart_backoff"] = toml_edit::value(v);
    }
    if let Some(v) = opts.priority {
        app_tbl["priority"] = toml_edit::value(v as i64);
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Re-parse the deployment file after flag writes so callers see final state.
pub fn reload_app(
    name: &str,
    path: &Path,
    defaults: Option<&AppDefaults>,
) -> Result<crate::config::ResolvedApp> {
    let (raw, _) = crate::config::AppRaw::load(path)?;
    resolve_app(name, path, &raw, defaults)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xk-reg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn opts(name: &str) -> AddOptions {
        AddOptions {
            name: Some(name.into()),
            ..Default::default()
        }
    }

    /// A fake executable (content is irrelevant to registration).
    fn make_exe(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        p
    }

    // -- 1.1 scaffold render --------------------------------------------------

    /// The scaffolded file is a real file loadable by `AppRaw::load`; the
    /// command and work_dir are absolute; --args/--env land in the program.
    #[test]
    fn scaffold_generates_loadable_config_with_absolute_paths() {
        let tmp = tmp_dir("scaffold");
        let exe = make_exe(&tmp.join("bin"), "proc"); // parent created inside
        let config = DaemonConfig::default();
        let mut env = BTreeMap::new();
        env.insert("LOG".to_string(), "debug".to_string());
        let o = AddOptions {
            name: Some("abc".into()),
            args: Some("--port 8080 --msg \"a b\"".into()),
            env: Some(env),
            ..opts("abc")
        };
        let r = add(&config, &tmp, &exe, &o).unwrap();
        assert!(r.scaffolded && r.changed, "first generation is changed");

        let file = tmp.join("apps").join("abc.toml");
        assert_eq!(r.file, file);
        let md = std::fs::symlink_metadata(&file).unwrap();
        assert!(md.is_file(), "record must be a real file, not a link");

        let text = std::fs::read_to_string(&file).unwrap();
        let (raw, _) = AppRaw::load(&file).unwrap();
        let prog = raw.program.get("abc").expect("program named after the app");
        let cmd = prog.command.as_deref().unwrap();
        assert!(Path::new(cmd).is_absolute(), "command absolute: {cmd}");
        assert!(
            !cmd.contains(r"\\?\"),
            "no verbatim prefix in generated command: {cmd}"
        );
        assert_eq!(
            prog.args.as_deref(),
            Some(
                &[
                    "--port".to_string(),
                    "8080".to_string(),
                    "--msg".to_string(),
                    "a b".to_string()
                ][..]
            )
        );
        assert_eq!(
            prog.env
                .as_ref()
                .and_then(|e| e.get("LOG"))
                .map(|s| s.as_str()),
            Some("debug")
        );
        let wd = prog.work_dir.as_ref().unwrap();
        assert!(wd.is_absolute(), "work_dir absolute: {}", wd.display());
        assert!(text.contains("[app]"), "[app] table always rendered");

        // Round-trip resolution matches what the CLI reports.
        let resolved = resolve_app("abc", &file, &raw, None).unwrap();
        assert_eq!(resolved.programs[0].command, cmd);
        assert_eq!(resolved.programs[0].work_dir, *wd);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Executable path without --name: the app name is the file stem.
    #[test]
    fn scaffold_name_defaults_to_executable_stem() {
        let tmp = tmp_dir("stem");
        let exe = make_exe(&tmp, "web-server");
        let config = DaemonConfig::default();
        let r = add(&config, &tmp, &exe, &AddOptions::default()).unwrap();
        assert_eq!(r.app, "web-server");
        assert!(tmp.join("apps").join("web-server.toml").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A command path containing whitespace renders as a quoted single-line
    /// command and resolves back to the exact absolute path.
    #[test]
    fn scaffold_spaceful_path_round_trips() {
        let tmp = tmp_dir("space");
        let bin = tmp.join("my bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = make_exe(&bin, "proc");
        let config = DaemonConfig::default();
        let r = add(&config, &tmp, &exe, &opts("sp")).unwrap();
        let text = std::fs::read_to_string(&r.file).unwrap();
        let raw: AppRaw = toml::from_str(&text).unwrap();
        let resolved = resolve_app("sp", &r.file, &raw, None).unwrap();
        let p = &resolved.programs[0];
        assert_eq!(p.command, r.command, "single-line split recovers the path");
        let canon = exe.canonicalize().unwrap();
        assert_eq!(
            Path::new(&p.command),
            display_absolute(canon),
            "command is the canonical absolute path (no verbatim prefix)"
        );
        assert!(p.args.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// work_dir: default is the exact current directory of the add call;
    /// `--workdir` overrides it (existing dirs canonicalized, relative ones
    /// absolutized) — always an absolute path in the generated file.
    #[test]
    fn scaffold_workdir_defaults_to_cwd_and_overridable() {
        let tmp = tmp_dir("workdir");
        let exe = make_exe(&tmp, "proc");
        let config = DaemonConfig::default();

        // Default: the add-time current directory, verbatim.
        let r = add(&config, &tmp, &exe, &opts("dflt")).unwrap();
        let (raw, _) = AppRaw::load(&r.file).unwrap();
        let wd = raw.program["dflt"].work_dir.as_ref().unwrap();
        assert!(wd.is_absolute(), "default work_dir absolute: {}", wd.display());
        assert_eq!(
            wd,
            &std::env::current_dir().unwrap(),
            "work_dir defaults to the add-time cwd"
        );

        // --workdir with an existing directory wins (canonicalized absolute).
        let wdir = tmp.join("wd");
        std::fs::create_dir_all(&wdir).unwrap();
        let mut o = opts("ovr");
        o.workdir = Some(wdir.clone());
        let r = add(&config, &tmp, &exe, &o).unwrap();
        let (raw, _) = AppRaw::load(&r.file).unwrap();
        let wd = raw.program["ovr"].work_dir.as_ref().unwrap();
        assert!(wd.is_absolute(), "override work_dir absolute: {}", wd.display());
        assert_eq!(
            wd.canonicalize().unwrap(),
            wdir.canonicalize().unwrap(),
            "--workdir override lands in the file"
        );

        // A relative override that cannot be resolved is still absolutized.
        let mut o = opts("rel");
        o.workdir = Some(PathBuf::from("no-such-rel-dir"));
        let r = add(&config, &tmp, &exe, &o).unwrap();
        let (raw, _) = AppRaw::load(&r.file).unwrap();
        let wd = raw.program["rel"].work_dir.as_ref().unwrap();
        assert!(
            wd.is_absolute() && wd.ends_with("no-such-rel-dir"),
            "relative --workdir is absolutized: {}",
            wd.display()
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The scaffold supports the full micro-tuning flag set; every flag
    /// renders into `[app]` and survives a load/resolve round-trip.
    #[test]
    fn scaffold_renders_app_level_flags_and_round_trips() {
        let tmp = tmp_dir("flags");
        let exe = make_exe(&tmp, "proc");
        let config = DaemonConfig::default();
        let o = AddOptions {
            description: Some("demo service".into()),
            autostart: Some(false),
            autorestart: Some(RestartPolicy::OnFailure),
            restart_backoff: Some(2.5),
            priority: Some(-5),
            ..opts("tuned")
        };
        let r = add(&config, &tmp, &exe, &o).unwrap();
        let text = std::fs::read_to_string(&r.file).unwrap();
        for needle in ["demo service", "on-failure", "2.5", "-5"] {
            assert!(text.contains(needle), "{needle:?} rendered: {text}");
        }

        let (raw, _) = AppRaw::load(&r.file).unwrap();
        let meta = raw.app.as_ref().expect("[app] table rendered");
        assert_eq!(meta.description.as_deref(), Some("demo service"));
        assert_eq!(meta.autostart, Some(false));
        assert_eq!(meta.autorestart, Some(RestartPolicy::OnFailure));
        assert_eq!(meta.restart_backoff, Some(2.5));
        assert_eq!(meta.priority, Some(-5));

        let resolved = resolve_app("tuned", &r.file, &raw, None).unwrap();
        assert!(!resolved.autostart, "autostart=false survives resolution");
        assert_eq!(resolved.priority, -5);
        assert_eq!(resolved.description.as_deref(), Some("demo service"));
        let p = &resolved.programs[0];
        assert_eq!(p.autorestart, RestartPolicy::OnFailure);
        assert_eq!(p.restart_backoff, 2.5);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- 1.2 dispatch -----------------------------------------------------------

    #[test]
    fn add_dispatches_by_path_shape() {
        let tmp = tmp_dir("dispatch");
        let config = DaemonConfig::default();

        // Directory containing xkeeper.toml → existing directory flow.
        let deploy = tmp.join("myapp");
        std::fs::create_dir_all(&deploy).unwrap();
        std::fs::write(
            deploy.join("xkeeper.toml"),
            "[program.a]\ncommand = \"true\"\n",
        )
        .unwrap();
        let r = add(&config, &tmp, &deploy, &opts("myapp")).unwrap();
        assert!(!r.scaffolded);
        assert!(tmp.join("apps").join("myapp.toml").exists());

        // .toml file → existing file registration flow.
        let cfg_file = tmp.join("external.toml");
        std::fs::write(&cfg_file, "[program.b]\ncommand = \"true\"\n").unwrap();
        let r = add(&config, &tmp, &cfg_file, &opts("ext")).unwrap();
        assert!(!r.scaffolded);
        assert!(tmp.join("apps").join("ext.toml").exists());

        // Other regular file → scaffold.
        let exe = make_exe(&tmp, "proc");
        let r = add(&config, &tmp, &exe, &opts("scaf")).unwrap();
        assert!(r.scaffolded);
        assert!(tmp.join("apps").join("scaf.toml").exists());

        // Missing path → same error as before.
        let err = add(&config, &tmp, &tmp.join("nope"), &opts("x")).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err:#}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- 1.3 regeneration ---------------------------------------------------

    #[test]
    fn scaffold_regeneration_no_change_then_changed() {
        let tmp = tmp_dir("regen");
        let exe = make_exe(&tmp, "proc");
        let config = DaemonConfig::default();
        let first = add(&config, &tmp, &exe, &opts("abc")).unwrap();
        assert!(first.changed);

        // Identical re-add: unchanged, file bytes untouched.
        let before = std::fs::read_to_string(first.file.clone()).unwrap();
        let second = add(&config, &tmp, &exe, &opts("abc")).unwrap();
        assert!(!second.changed, "identical re-add is a no change");
        assert_eq!(
            std::fs::read_to_string(&first.file).unwrap(),
            before,
            "file untouched on no-change re-add"
        );

        // Different --args: full regeneration (hand edits are overwritten).
        let mut hand_edited = before.clone();
        hand_edited.push_str("\n# my manual note\n");
        std::fs::write(&first.file, &hand_edited).unwrap();
        let o = AddOptions {
            args: Some("--port 9090".into()),
            ..opts("abc")
        };
        let third = add(&config, &tmp, &exe, &o).unwrap();
        assert!(third.changed, "differing args are a change");
        let now = std::fs::read_to_string(&first.file).unwrap();
        assert!(now.contains("9090"), "new args written: {now}");
        assert!(!now.contains("my manual note"), "hand edit overwritten");
        assert!(!now.contains("8080"), "old args gone: {now}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Validation gates the write in BOTH directions: a first scaffold whose
    /// program name duplicates an existing app writes nothing, and a
    /// regeneration whose flags fail validation leaves the old file intact.
    #[test]
    fn scaffold_refuses_to_write_when_validation_fails() {
        let tmp = tmp_dir("prevalidate");
        let config = DaemonConfig::default();

        // An existing dir app already owns the program name "shared".
        let deploy = tmp.join("myapp");
        std::fs::create_dir_all(&deploy).unwrap();
        std::fs::write(
            deploy.join("xkeeper.toml"),
            "[program.shared]\ncommand = 'true'\n",
        )
        .unwrap();
        add(&config, &tmp, &deploy, &opts("myapp")).unwrap();

        // First scaffold with a colliding program name: refused, nothing lands.
        let exe = make_exe(&tmp, "proc");
        let err = add(&config, &tmp, &exe, &opts("shared")).unwrap_err();
        assert!(err.to_string().contains("duplicated"), "{err:#}");
        assert!(
            !tmp.join("apps").join("shared.toml").exists(),
            "failed scaffold writes nothing"
        );

        // A good scaffold succeeds, then a regeneration with an invalid flag
        // (restart_backoff must be > 0) must not touch the existing file.
        let ok = add(&config, &tmp, &exe, &opts("okapp")).unwrap();
        let before = std::fs::read_to_string(&ok.file).unwrap();
        let mut bad = opts("okapp");
        bad.restart_backoff = Some(0.0);
        let err = add(&config, &tmp, &exe, &bad).unwrap_err();
        assert!(err.to_string().contains("restart_backoff"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(&ok.file).unwrap(),
            before,
            "failed regeneration leaves the old file"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- 1.4 remove -----------------------------------------------------------

    #[test]
    fn remove_deletes_generated_file_but_keeps_external_body() {
        let tmp = tmp_dir("remove");
        let config = DaemonConfig::default();

        // Scaffold record: a real file inside app_dir → deleted with it.
        let exe = make_exe(&tmp, "proc");
        let r = add(&config, &tmp, &exe, &opts("abc")).unwrap();
        let removed = remove(&config, &tmp, "abc").unwrap();
        assert!(removed.deleted_file, "generated file is deleted");
        assert_eq!(removed.path, r.file);
        assert!(!r.file.exists(), "scaffold file gone");
        assert!(!removed.path.exists());

        // Link record to an external file → link gone, body kept.
        let deploy = tmp.join("myapp");
        std::fs::create_dir_all(&deploy).unwrap();
        let body = deploy.join("xkeeper.toml");
        std::fs::write(&body, "[program.a]\ncommand = \"true\"\n").unwrap();
        add(&config, &tmp, &deploy, &opts("myapp")).unwrap();
        let removed = remove(&config, &tmp, "myapp").unwrap();
        // The external body always survives: on unix the record is a
        // symlink (path criterion sees the external parent); on Windows
        // make_link falls back to a hard link, so the path criterion sees
        // an app_dir parent and reports the record itself as removed — the
        // hardlink semantics keep the external file's content intact.
        if cfg!(unix) {
            assert!(!removed.deleted_file, "external body kept");
        } else {
            assert!(removed.deleted_file, "hardlink record is removed in place");
        }
        assert!(body.exists(), "deployment config untouched");
        assert!(!tmp.join("apps").join("myapp.toml").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Scaffolding over a name registered from OUTSIDE app_dir must refuse
    /// (a write through the link would clobber the external config).
    #[test]
    fn scaffold_refuses_to_clobber_external_registration() {
        let tmp = tmp_dir("clobber");
        let config = DaemonConfig::default();
        let deploy = tmp.join("myapp");
        std::fs::create_dir_all(&deploy).unwrap();
        std::fs::write(
            deploy.join("xkeeper.toml"),
            "[program.a]\ncommand = \"true\"\n",
        )
        .unwrap();
        add(&config, &tmp, &deploy, &opts("myapp")).unwrap();
        let exe = make_exe(&tmp, "proc");
        let err = add(&config, &tmp, &exe, &opts("myapp")).unwrap_err();
        assert!(err.to_string().contains("remove"), "{err:#}");
        assert!(
            std::fs::read_to_string(deploy.join("xkeeper.toml"))
                .unwrap()
                .contains("[program.a]"),
            "external config untouched"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- 1.5 reserved keyword ---------------------------------------------------

    #[test]
    fn add_rejects_reserved_name_all() {
        let tmp = tmp_dir("reserved");
        let config = DaemonConfig::default();
        let exe = make_exe(&tmp, "proc");

        // Explicit --name all.
        let err = add(&config, &tmp, &exe, &opts("all")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("reserved") && msg.contains("apply"), "{msg}");

        // Stem-derived: an executable named `all` (all.exe on Windows).
        let exe_all = make_exe(&tmp, if cfg!(windows) { "all.exe" } else { "all" });
        let err = add(&config, &tmp, &exe_all, &AddOptions::default()).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err:#}");

        // The directory flow is equally guarded.
        let dir_all = tmp.join("all");
        std::fs::create_dir_all(&dir_all).unwrap();
        std::fs::write(
            dir_all.join("xkeeper.toml"),
            "[program.a]\ncommand = \"true\"\n",
        )
        .unwrap();
        let err = add(&config, &tmp, &dir_all, &AddOptions::default()).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err:#}");
        assert!(
            !tmp.join("apps").join("all.toml").exists(),
            "nothing registered under the reserved name"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- 1.6 unix execute bit ----------------------------------------------------

    /// unix-only: the scaffold warns about a missing execute bit but still
    /// registers; with the bit set nothing is warned about. Windows has no
    /// execute-bit concept, so the whole check is cfg(unix).
    #[test]
    #[cfg(unix)]
    fn scaffold_warns_on_missing_execute_bit_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tmp_dir("execbit");
        let exe = make_exe(&tmp, "proc");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644)).unwrap();
        let config = DaemonConfig::default();
        // Registration still succeeds without the bit (warning only).
        let r = add(&config, &tmp, &exe, &opts("abc")).unwrap();
        assert!(r.scaffolded);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// add must fail loudly (not "register" with a swallowed warning) when
    /// the app_dir link cannot be written — the link IS the registry record.
    #[test]
    #[cfg(unix)]
    fn add_fails_when_app_dir_not_writable() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root can write anything; can't simulate the failure
        }
        let tmp = std::env::temp_dir().join(format!("xk-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let app_dir = tmp.join("apps");
        std::fs::create_dir_all(&app_dir).unwrap();
        let deploy = tmp.join("myapp");
        std::fs::create_dir_all(&deploy).unwrap();
        std::fs::write(
            deploy.join("xkeeper.toml"),
            "[program.a]\ncommand = \"true\"\n",
        )
        .unwrap();

        // Simulate a root-owned app_dir: drop write permission for everyone.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let config = DaemonConfig::default();
        let o = opts("myapp");
        let r = add(&config, &tmp, &deploy.join("xkeeper.toml"), &o);
        let err = r.err().expect("add must fail when app_dir is not writable");
        let msg = err.to_string();
        assert!(msg.contains("app_dir"), "must name app_dir: {msg}");
        assert!(msg.contains("sudo"), "must hint at the fix: {msg}");

        // The registration must NOT have persisted.
        assert!(
            std::fs::read_dir(&app_dir).unwrap().next().is_none(),
            "no link may appear in app_dir on failure"
        );

        std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
