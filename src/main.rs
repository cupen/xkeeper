//! xkeeper — a cross-platform, application-layer process keeper with a TOML
//! layered config (global daemon config + per-app deployment files).
//!
//! `xkeeper run` is the daemon; everything else is a client: control
//! subcommands talk to the loopback HTTP API, and add/remove/list manage the
//! app registry (app_dir links).

mod api;
mod assets;
mod client;
mod config;
mod health;
mod metrics;
mod platform;
mod program;
mod pump;
mod registry;
mod server;
mod service;
mod shell;
mod supervisor;
mod web;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use log::info;

use crate::config::{DaemonConfig, RestartPolicy};
use crate::registry::AddOptions;
use crate::supervisor::Supervisor;

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_CONFIG: i32 = 2;

#[derive(Parser, Debug)]
#[command(
    name = "xkeeper",
    version,
    about = "Cross-platform application-layer process keeper (daemon + CLI)"
)]
struct Cli {
    /// Path to the daemon config file. Defaults to the platform location
    /// (/etc/xkeeper/daemon.toml on Linux, %APPDATA%\xkeeper\daemon.toml on Windows).
    #[arg(short = 'c', long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the daemon in the foreground (default when no subcommand given)
    Run,
    /// Run the daemon and serve the web console (API + embedded UI)
    Webui {
        /// Address for the console to listen on
        #[arg(long, default_value = "127.0.0.1:9877")]
        listen: String,
    },
    /// Validate the daemon config + all registered apps, or one app file
    Validate {
        /// Optional path to a single app config file
        path: Option<PathBuf>,
    },
    /// Edit the daemon config in $VISUAL/$EDITOR (vi/notepad), then validate
    Edit,
    /// Overview of daemon and all programs
    Status,
    /// Start a program
    Start { name: String },
    /// Stop a program (graceful, then force after stop timeout)
    Stop { name: String },
    /// Restart a program
    Restart { name: String },
    /// Print a program's pid
    Pid { name: String },
    /// Show a program's logs
    Log {
        name: String,
        #[arg(long, default_value = "20")]
        tail: usize,
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(long, value_parser = ["out", "err"], default_value = "out")]
        stream: String,
    },
    /// Reload daemon config and all registered apps (per-app atomic)
    Reload,
    /// Stop all programs and exit the daemon
    Shutdown,
    /// Register an app (a directory with xkeeper.toml, or a config file path)
    Add {
        /// App directory (containing xkeeper.toml) or a config file path
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        autostart: bool,
        #[arg(long, conflicts_with = "autostart")]
        no_autostart: bool,
        #[arg(long, value_parser = ["always", "on-failure", "never"])]
        autorestart: Option<String>,
        #[arg(long)]
        restart_backoff: Option<f64>,
        #[arg(long)]
        priority: Option<i32>,
    },
    /// Unregister an app (the deployment config is kept)
    Remove { name: String },
    /// List registered apps
    List,
    /// Interactive shell client (like supervisorctl)
    Shell {
        /// Execute a single shell command and exit (script-friendly)
        #[arg(short = 'e', long = "exec")]
        cmd: Option<String>,
    },
    /// Local helper commands not needing a running daemon
    System {
        #[command(subcommand)]
        cmd: SystemCmd,
    },
    /// Install/uninstall xkeeper as a system service (Linux systemd; root required)
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
}

#[derive(Subcommand, Debug)]
enum SystemCmd {
    /// Open the web console in the system browser, starting the daemon if needed
    Webui {
        /// Console URL (default: the webui listen address, 127.0.0.1:9877)
        url: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ServiceCmd {
    /// Generate the systemd unit, daemon-reload and enable it
    Install {
        /// Also start the service right after installing
        #[arg(long)]
        now: bool,
        /// Overwrite an existing unit whose content differs
        #[arg(long)]
        force: bool,
        /// systemd unit name without the .service suffix
        #[arg(long)]
        name: Option<String>,
        /// Run the service as this user (User= in the unit)
        #[arg(long)]
        user: Option<String>,
    },
    /// Stop, disable and remove the systemd unit
    Uninstall {
        /// systemd unit name without the .service suffix
        #[arg(long)]
        name: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match dispatch(&cli) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("xkeeper: error: {e:#}");
            EXIT_ERROR
        }
    };
    std::process::exit(code);
}

/// Platform default location for the daemon config.
pub(crate) fn default_config_path() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/etc/xkeeper/daemon.toml")
    }
    #[cfg(windows)]
    {
        // %APPDATA%\xkeeper\daemon.toml
        match std::env::var("APPDATA") {
            Ok(appdata) => PathBuf::from(appdata).join("xkeeper").join("daemon.toml"),
            Err(_) => {
                let home = std::env::var("USERPROFILE").unwrap_or_default();
                PathBuf::from(home)
                    .join("AppData")
                    .join("Roaming")
                    .join("xkeeper")
                    .join("daemon.toml")
            }
        }
    }
}

fn config_path_of(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(default_config_path)
}

fn dispatch(cli: &Cli) -> Result<()> {
    // Client subcommands don't reach the daemon's logger init; without
    // this, warnings from the registry/config layers would be dropped
    // silently. `run`/`webui` (and bare `xkeeper`) init their own logger
    // from the config's log_level.
    if !matches!(&cli.cmd, None | Some(Cmd::Run) | Some(Cmd::Webui { .. })) {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .try_init()
            .ok();
    }
    let config_path = config_path_of(cli);
    match &cli.cmd {
        Some(Cmd::Run) | None => run_daemon(&config_path, None),
        Some(Cmd::Webui { listen }) => run_daemon(&config_path, Some(listen.clone())),
        Some(Cmd::Validate { path }) => validate(&config_path, path.as_deref()),
        Some(Cmd::Edit) => {
            let code = edit_config(&config_path)?;
            if code != EXIT_OK {
                std::process::exit(code);
            }
            Ok(())
        }
        Some(Cmd::Status) => client_cmd(&config_path, |c| {
            let v = c.status()?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }),
        Some(Cmd::Pid { name }) => client_cmd(&config_path, |c| {
            let v = c.program(name)?;
            match v.get("pid") {
                Some(pid) if !pid.is_null() => println!("{pid}"),
                _ => println!("not running"),
            }
            Ok(())
        }),
        Some(Cmd::Start { name }) => action_cmd(&config_path, name, "start"),
        Some(Cmd::Stop { name }) => action_cmd(&config_path, name, "stop"),
        Some(Cmd::Restart { name }) => action_cmd(&config_path, name, "restart"),
        Some(Cmd::Reload) => client_cmd(&config_path, |c| {
            let v = c.reload()?;
            println!(
                "{}",
                v.get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("reloaded")
            );
            Ok(())
        }),
        Some(Cmd::Shutdown) => client_cmd(&config_path, |c| {
            let v = c.shutdown()?;
            println!(
                "{}",
                v.get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("shutting down")
            );
            Ok(())
        }),
        Some(Cmd::Log {
            name,
            tail,
            follow,
            stream,
        }) => client_cmd(&config_path, |c| {
            if *follow {
                c.log_follow(name, stream)
            } else {
                for line in c.log_tail(name, stream, *tail)? {
                    println!("{line}");
                }
                Ok(())
            }
        }),
        Some(Cmd::Add {
            path,
            name,
            description,
            autostart,
            no_autostart,
            autorestart,
            restart_backoff,
            priority,
        }) => {
            let config = client::load_config(&config_path)?;
            let config_dir = config_dir_of(&config_path);

            // v0.1 single-file configs are recognized and converted first.
            let mut target = path.clone();
            if path.is_file() {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                if config::looks_legacy(&text) {
                    let imp = config::import_legacy(&path)?;
                    println!(
                        "legacy config imported as app {:?}: {} generated (source kept untouched)",
                        imp.app_name,
                        imp.new_file.display()
                    );
                    if !imp.daemon_hint.is_empty() {
                        eprintln!(
                            "note: [daemon] settings ({}) belong in the daemon config; move them manually",
                            imp.daemon_hint.join(", ")
                        );
                    }
                    target = imp.new_file;
                }
            }

            let opts = AddOptions {
                name: name.clone(),
                description: description.clone(),
                autostart: if *autostart {
                    Some(true)
                } else if *no_autostart {
                    Some(false)
                } else {
                    None
                },
                autorestart: match autorestart.as_deref().map(parse_restart) {
                    Some(Ok(v)) => Some(v),
                    Some(Err(e)) => {
                        eprintln!("xkeeper: error: {e:#}");
                        std::process::exit(EXIT_CONFIG);
                    }
                    None => None,
                },
                restart_backoff: *restart_backoff,
                priority: *priority,
            };
            let app = registry::add(&config, &config_dir, &target, &opts)?;
            println!("app[{app}] registered");
            sync_if_online(&config)?;
            Ok(())
        }
        Some(Cmd::Remove { name }) => {
            let config = client::load_config(&config_path)?;
            let config_dir = config_dir_of(&config_path);
            registry::remove(&config, &config_dir, name)?;
            println!("app[{name}] unregistered (deployment config kept)");
            sync_if_online(&config)?;
            Ok(())
        }
        Some(Cmd::Shell { cmd }) => shell::run(&config_path, cmd.as_deref()),
        Some(Cmd::System { cmd }) => match cmd {
            SystemCmd::Webui { url } => shell::system_webui(url.as_deref()),
        },
        Some(Cmd::Service { cmd }) => {
            let (action, name, user, force, now) = match cmd {
                ServiceCmd::Install {
                    now,
                    force,
                    name,
                    user,
                } => ("install", name, user, *force, *now),
                ServiceCmd::Uninstall { name } => ("uninstall", name, &None, false, false),
            };
            let opts = service::ServiceOptions {
                unit_name: name
                    .clone()
                    .unwrap_or_else(|| service::DEFAULT_UNIT_NAME.to_string()),
                user: user.clone(),
                force,
                now,
            };
            if action == "install" {
                service::install(&config_path, &opts)?;
            } else {
                service::uninstall(&config_path, &opts)?;
            }
            Ok(())
        }
        Some(Cmd::List) => {
            let config = client::load_config(&config_path)?;
            let config_dir = config_dir_of(&config_path);
            let apps = registry::list(&config, &config_dir)?;
            if apps.is_empty() {
                println!(
                    "no apps registered (use `xkeeper add <dir>` in an app deployment directory)"
                );
                return Ok(());
            }
            for a in apps {
                let desc = a.description.as_deref().unwrap_or("");
                println!("{:<20} {}", a.name, a.path.display());
                if !desc.is_empty() {
                    println!("{:<20} {desc}", "");
                }
            }
            Ok(())
        }
    }
}

fn parse_restart(s: &str) -> Result<RestartPolicy> {
    Ok(match s {
        "always" => RestartPolicy::Always,
        "on-failure" => RestartPolicy::OnFailure,
        "never" => RestartPolicy::Never,
        _ => bail!("invalid --autorestart value {s:?} (always | on-failure | never)"),
    })
}

fn config_dir_of(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

/// After a registry change, poke a running daemon so it picks the change up
/// immediately. Offline is fine — the next `run` reads the registry anyway.
/// A reachable daemon whose reload fails is a partial failure and must not
/// exit 0: the registry changed but the daemon did not pick it up.
fn sync_if_online(config: &DaemonConfig) -> Result<()> {
    let c = client::Client::from_config(config);
    if c.health().is_ok() {
        let v = c.reload().with_context(|| {
            "registry change persisted, but the running daemon failed to reload; \
             run `xkeeper reload` once the problem is fixed"
        })?;
        println!(
            "daemon synced: {}",
            v.get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("reloaded")
        );
    }
    Ok(())
}

fn client_cmd(config_path: &Path, f: impl FnOnce(&client::Client) -> Result<()>) -> Result<()> {
    let config = client::load_config(config_path)?;
    let c = client::Client::from_config(&config);
    match f(&c) {
        Ok(()) => Ok(()),
        Err(e) => {
            std::process::exit(client::exit_code_of(&e));
        }
    }
}

fn action_cmd(config_path: &Path, name: &str, action: &str) -> Result<()> {
    client_cmd(config_path, |c| {
        let v = c.action(name, action)?;
        println!(
            "{}",
            v.get("result").and_then(|r| r.as_str()).unwrap_or("done")
        );
        Ok(())
    })
}

// -- validate ---------------------------------------------------------------

fn validate(config_path: &Path, single: Option<&Path>) -> Result<()> {
    if let Some(p) = single {
        let config = client::load_config(config_path)?;
        let (raw, _) = config::AppRaw::load(p)?;
        let name = registry::name_from_dir(p);
        let app = config::resolve_app(&name, p, &raw, config.app_default.as_ref())?;
        config::validate_all(&[app.clone()])?;
        println!(
            "OK: app {:?} with {} program(s)",
            app.name,
            app.programs.len()
        );
        for prog in &app.programs {
            println!("  - {} <{}>", prog.name, prog.command);
        }
        return Ok(());
    }
    let (config, existed) = DaemonConfig::load_or_default(config_path)?;
    if !existed {
        println!(
            "OK: no daemon config at {} (built-in defaults in effect)",
            config_path.display()
        );
    }
    let config_dir = config_dir_of(config_path);
    let apps = registry::list(&config, &config_dir)?;
    let mut resolved = Vec::new();
    for l in &apps {
        let (raw, _) = config::AppRaw::load(&l.path)?;
        resolved.push(config::resolve_app(
            &l.name,
            &l.path,
            &raw,
            config.app_default.as_ref(),
        )?);
    }
    config::validate_all(&resolved)?;
    println!(
        "OK: daemon config + {} registered app(s), {} program(s)",
        resolved.len(),
        resolved.iter().map(|a| a.programs.len()).sum::<usize>()
    );
    for a in &resolved {
        println!("  app {:?} ({})", a.name, a.path.display());
        for p in &a.programs {
            println!("    - {} <{}>", p.name, p.command);
        }
    }
    Ok(())
}

/// Content written when `edit` creates a missing config file.
const EDIT_SEED: &str = "\
# xkeeper daemon config — every field is optional; built-in defaults apply
# for anything omitted. Annotated templates live in the repository:
#   conf/daemon.toml  (this file)    conf/app.toml  (per-app deployment)
";

/// Launch the user's editor on the daemon config, then validate the result.
///
/// `$VISUAL` wins over `$EDITOR` (standard precedence); both fall back to a
/// platform default. A missing config file is created (with parents) seeded
/// with `EDIT_SEED`, so first-time setup via `xkeeper edit` works.
/// Returns the process exit code: `EXIT_OK` when the config is valid (or
/// absent, meaning defaults), `EXIT_CONFIG` when the editor left an invalid
/// file behind; launching the editor itself fails with `Err`.
fn edit_config(config_path: &Path) -> Result<i32> {
    let from_env = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    let editor = from_env("VISUAL")
        .or_else(|| from_env("EDITOR"))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });
    edit_config_with(&editor, config_path)
}

fn edit_config_with(editor: &str, config_path: &Path) -> Result<i32> {
    if !config_path.exists() {
        if let Some(dir) = config_path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create config dir {}", dir.display()))?;
        }
        std::fs::write(config_path, EDIT_SEED)
            .with_context(|| format!("cannot create {}", config_path.display()))?;
        println!("created {}", config_path.display());
    }

    let mut argv = config::split_command(editor);
    let prog = argv.drain(..).next().context("editor command is empty")?;
    let status = std::process::Command::new(prog)
        .args(argv)
        .arg(config_path)
        .status()
        .with_context(|| format!("cannot launch editor {editor:?}"))?;
    if !status.success() {
        bail!("editor {editor:?} exited with {status}");
    }

    // Report what the edit produced, mirroring `validate` wording.
    let (cfg, existed) = match DaemonConfig::load_or_default(config_path) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("xkeeper: daemon config is invalid:\n{e:#}");
            return Ok(EXIT_CONFIG);
        }
    };
    if let Err(e) = cfg.validate() {
        eprintln!("xkeeper: daemon config is invalid:\n{e:#}");
        return Ok(EXIT_CONFIG);
    }
    if existed {
        println!("OK: daemon config at {} is valid", config_path.display());
    } else {
        println!(
            "OK: no daemon config at {} (built-in defaults in effect)",
            config_path.display()
        );
    }
    Ok(EXIT_OK)
}

// -- daemon -----------------------------------------------------------------
fn run_daemon(config_path: &Path, webui_listen: Option<String>) -> Result<()> {
    let (config, existed) = DaemonConfig::load_or_default(config_path)
        .with_context(|| format!("failed to load daemon config {}", config_path.display()))?;
    if !existed {
        info!(
            "no daemon config at {} — starting empty with built-in defaults",
            config_path.display()
        );
    }

    // Honor RUST_LOG over the config level.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(&config.daemon.log_level),
    )
    .try_init()
    .ok();
    info!(
        "daemon config: {}{}",
        config_path.display(),
        if existed { "" } else { " (absent)" }
    );

    let config_dir = config_dir_of(config_path);
    let sup = Supervisor::new(config, &config_dir)?;
    sup.set_config_path(config_path);

    // Metrics sampler: 1 Hz read-only sideband (CPU/RSS + log rates). Runs
    // for both `run` and `webui`; exits with the daemon. Its failure only
    // degrades metric fields to null.
    metrics::spawn_sampler(sup.clone());

    // Health checker thread: probes due tasks, reports back into the state
    // and enqueues restarts for restart_on_unhealthy programs.
    {
        let sup = sup.clone();
        let tasks = sup.health_tasks.clone();
        std::thread::Builder::new()
            .name("health-checker".into())
            .spawn(move || {
                health::checker_loop(tasks, move |name, ok| {
                    let restart = {
                        let mut st = sup.state.lock().unwrap();
                        st.programs
                            .get_mut(&name)
                            .map(|p| p.record_health(ok, std::time::Instant::now()))
                            .unwrap_or(false)
                    };
                    if restart {
                        sup.enqueue(supervisor::Command::HealthRestart { name });
                    }
                });
            })
            .context("failed to spawn health checker")?;
    }

    // Bind the API first so a port clash fails fast and clearly, then serve
    // connections on a background thread while the main thread supervises.
    let listener = server::bind(&sup)?;
    {
        let sup2 = sup.clone();
        std::thread::Builder::new()
            .name("api-accept".into())
            .spawn(move || server::serve(sup2, listener))
            .context("failed to spawn api accept loop")?;
    }

    // Web console: SPA + WebSocket push on its own loopback port, sharing
    // the supervisor with the control plane. Exits with the daemon.
    if let Some(listen) = &webui_listen {
        let sup2 = sup.clone();
        let listen = listen.clone();
        std::thread::Builder::new()
            .name("webui".into())
            .spawn(move || {
                if let Err(e) = web::serve(sup2, &listen) {
                    log::error!("webui server error: {e:#}");
                }
            })
            .context("failed to spawn webui thread")?;
    }

    // Bootstrap the registry and enter the supervision loop.
    sup.bootstrap()?;
    info!(
        "xkeeper {} running (Ctrl+C to stop)",
        env!("CARGO_PKG_VERSION")
    );

    let shutdown = {
        let sup2 = sup.clone();
        let flag = Arc::new(AtomicBool::new(false));
        let f2 = flag.clone();
        ctrlc::set_handler(move || {
            info!("shutdown signal received");
            sup2.request_shutdown();
            let _ = f2;
        })
        .context("failed to install Ctrl+C / SIGTERM handler")?;
        flag
    };
    let _ = shutdown;

    sup.run();
    Ok(())
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    #[cfg(unix)]
    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xk-edit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    #[cfg(unix)]
    fn edit_creates_missing_config_and_passes() {
        let dir = temp_root("create");
        let path = dir.join("nested").join("daemon.toml");
        // `true` exits 0 without touching the file.
        let code = edit_config_with("true", &path).unwrap();
        assert_eq!(code, EXIT_OK);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# xkeeper daemon config"),
            "seed missing: {text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn edit_reports_invalid_result_with_config_exit_code() {
        let dir = temp_root("invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.toml");
        std::fs::write(&path, "").unwrap();
        let script = dir.join("fake-editor.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'bogus_field = 1\\n' > \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let code = edit_config_with(script.to_str().unwrap(), &path).unwrap();
        assert_eq!(code, EXIT_CONFIG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn editor_must_exist() {
        let dir = temp_root("noeditor");
        let path = dir.join("daemon.toml");
        let r = edit_config_with("xk-definitely-not-an-editor", &path);
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    fn config_with_port(port: u16) -> Result<DaemonConfig> {
        Ok(toml::from_str(&format!(
            "[daemon]\nhost = \"127.0.0.1\"\nport = {port}\n"
        ))?)
    }

    /// A reachable daemon whose reload fails must surface as an error (main
    /// maps it to a non-zero exit), never a swallowed warning — the registry
    /// changed but the daemon did not pick it up.
    #[test]
    fn reload_failure_while_online_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = std::io::Read::read(&mut sock, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = if req.starts_with("GET /v1/health") {
                    ("200 OK", "{}")
                } else {
                    ("500 Internal Server Error", "{\"error\":\"reload boom\"}")
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                std::io::Write::write_all(&mut sock, resp.as_bytes()).unwrap();
            }
        });

        let config = config_with_port(port).unwrap();
        let r = sync_if_online(&config);
        let err = r.err().expect("reload failure while online must error");
        assert!(
            err.to_string().contains("failed to reload"),
            "error must explain the sync failure: {err:#}"
        );
        server.join().unwrap();
    }

    /// Offline: registry ops are offline-first; unreachable daemon means
    /// nothing to sync — silent success.
    #[test]
    fn offline_sync_is_a_silent_success() {
        let config = config_with_port(1).unwrap(); // loopback:1 is never open
        sync_if_online(&config).expect("offline sync must be a silent success");
    }
}
