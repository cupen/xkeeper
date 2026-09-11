//! App registry: the set of registered apps lives as `<name>.toml` links in
//! the daemon-configured `app_dir`. add/remove/list manage those links; the
//! linked deployment file is always the source of truth.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use log::{debug, warn};

use crate::config::{
    AppDefaults, DaemonConfig, RestartPolicy, is_valid_name, resolve_app, validate_all,
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
}

/// Register an app: validate it, create the `app_dir` link, and write the
/// micro-tuning flags into the deployment file's `[app]` table. Idempotent
/// (upsert by name). Returns the app name.
pub fn add(
    config: &DaemonConfig,
    config_dir: &Path,
    path: &Path,
    opts: &AddOptions,
) -> Result<String> {
    let path = if path.is_dir() {
        let f = path.join("xkeeper.toml");
        if !f.exists() {
            bail!("no xkeeper.toml found in directory {}", path.display());
        }
        f
    } else {
        path.to_path_buf()
    };
    if !path.exists() {
        bail!("app config not found: {}", path.display());
    }
    // Canonicalize so relative paths (e.g. `add .`) still yield a proper
    // directory name and stable link targets.
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    let name = match &opts.name {
        Some(n) => n.clone(),
        None => name_from_dir(&path),
    };
    if !is_valid_name(&name) {
        bail!("app name {name:?} is not filename-safe");
    }

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
    Ok(name)
}

/// Remove an app registration (delete the link, never the deployment file).
pub fn remove(config: &DaemonConfig, config_dir: &Path, name: &str) -> Result<PathBuf> {
    let link = link_path(config, config_dir, name);
    if !link.exists() {
        bail!(
            "app {name:?} is not registered (no link at {})",
            link.display()
        );
    }
    let real = std::fs::canonicalize(&link).unwrap_or_else(|_| link.clone());
    std::fs::remove_file(&link)
        .with_context(|| format!("failed to remove registration link {}", link.display()))?;
    debug!(
        "app[{name}] unregistered (config kept at {})",
        real.display()
    );
    Ok(real)
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
        let opts = AddOptions {
            name: Some("myapp".into()),
            ..Default::default()
        };
        let r = add(&config, &tmp, &deploy.join("xkeeper.toml"), &opts);
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
