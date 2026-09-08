//! xkeeper — a cross-platform, application-layer process keeper with a TOML
//! layered config (global core + per-app deployment files).
//!
//! `xkeeper run` is the daemon; everything else is a client: control
//! subcommands talk to the loopback HTTP API, and add/remove/list manage the
//! app registry (app_dir links).

mod api;
mod assets;
mod client;
mod config;
mod health;
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
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use log::info;

use crate::config::{CoreConfig, RestartPolicy};
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
    /// Path to the core config file. Defaults to the platform location
    /// (/etc/xkeeper.toml on Linux, %APPDATA%\xkeeper\xkeeper.toml on Windows).
    /// Long option only: `-c` belongs to `xkeeper shell -c <cmd>`.
    #[arg(long, global = true)]
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
    /// Validate the core config + all registered apps, or one app file
    Validate {
        /// Optional path to a single app config file
        path: Option<PathBuf>,
    },
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
    /// Reload core config and all registered apps (per-app atomic)
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
        #[arg(short = 'c')]
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

/// Platform default location for the core config.
pub(crate) fn default_core_path() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/etc/xkeeper.toml")
    }
    #[cfg(windows)]
    {
        // %APPDATA%\xkeeper\xkeeper.toml
        match std::env::var("APPDATA") {
            Ok(appdata) => PathBuf::from(appdata).join("xkeeper").join("xkeeper.toml"),
            Err(_) => {
                let home = std::env::var("USERPROFILE").unwrap_or_default();
                PathBuf::from(home)
                    .join("AppData")
                    .join("Roaming")
                    .join("xkeeper")
                    .join("xkeeper.toml")
            }
        }
    }
}

fn core_path_of(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(default_core_path)
}

fn dispatch(cli: &Cli) -> Result<()> {
    let core_path = core_path_of(cli);
    match &cli.cmd {
        Some(Cmd::Run) | None => run_daemon(&core_path, None),
        Some(Cmd::Webui { listen }) => run_daemon(&core_path, Some(listen.clone())),
        Some(Cmd::Validate { path }) => validate(&core_path, path.as_deref()),
        Some(Cmd::Status) => client_cmd(&core_path, |c| {
            let v = c.status()?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }),
        Some(Cmd::Pid { name }) => client_cmd(&core_path, |c| {
            let v = c.program(name)?;
            match v.get("pid") {
                Some(pid) if !pid.is_null() => println!("{pid}"),
                _ => println!("not running"),
            }
            Ok(())
        }),
        Some(Cmd::Start { name }) => action_cmd(&core_path, name, "start"),
        Some(Cmd::Stop { name }) => action_cmd(&core_path, name, "stop"),
        Some(Cmd::Restart { name }) => action_cmd(&core_path, name, "restart"),
        Some(Cmd::Reload) => client_cmd(&core_path, |c| {
            let v = c.reload()?;
            println!("{}", v.get("result").and_then(|r| r.as_str()).unwrap_or("reloaded"));
            Ok(())
        }),
        Some(Cmd::Shutdown) => client_cmd(&core_path, |c| {
            let v = c.shutdown()?;
            println!("{}", v.get("result").and_then(|r| r.as_str()).unwrap_or("shutting down"));
            Ok(())
        }),
        Some(Cmd::Log { name, tail, follow, stream }) => client_cmd(&core_path, |c| {
            if *follow {
                c.log_follow(name, stream)
            } else {
                for line in c.log_tail(name, stream, *tail)? {
                    println!("{line}");
                }
                Ok(())
            }
        }),
        Some(Cmd::Add { path, name, description, autostart, no_autostart, autorestart, restart_backoff, priority }) => {
            let core = client::load_core(&core_path)?;
            let core_dir = core_dir_of(&core_path);

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
                            "note: [daemon] settings ({}) belong in the core config; move them manually",
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
            let app = registry::add(&core, &core_dir, &target, &opts)?;
            println!("app[{app}] registered");
            sync_if_online(&core)?;
            Ok(())
        }
        Some(Cmd::Remove { name }) => {
            let core = client::load_core(&core_path)?;
            let core_dir = core_dir_of(&core_path);
            registry::remove(&core, &core_dir, name)?;
            println!("app[{name}] unregistered (deployment config kept)");
            sync_if_online(&core)?;
            Ok(())
        }
        Some(Cmd::Shell { cmd }) => shell::run(&core_path, cmd.as_deref()),
        Some(Cmd::System { cmd }) => match cmd {
            SystemCmd::Webui { url } => shell::system_webui(url.as_deref()),
        },
        Some(Cmd::Service { cmd }) => {
            let (action, name, user, force, now) = match cmd {
                ServiceCmd::Install { now, force, name, user } => {
                    ("install", name, user, *force, *now)
                }
                ServiceCmd::Uninstall { name } => ("uninstall", name, &None, false, false),
            };
            let opts = service::ServiceOptions {
                unit_name: name.clone().unwrap_or_else(|| service::DEFAULT_UNIT_NAME.to_string()),
                user: user.clone(),
                force,
                now,
            };
            if action == "install" {
                service::install(&core_path, &opts)?;
            } else {
                service::uninstall(&core_path, &opts)?;
            }
            Ok(())
        }
        Some(Cmd::List) => {
            let core = client::load_core(&core_path)?;
            let core_dir = core_dir_of(&core_path);
            let apps = registry::list(&core, &core_dir)?;
            if apps.is_empty() {
                println!("no apps registered (use `xkeeper add <dir>` in an app deployment directory)");
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

fn core_dir_of(core_path: &Path) -> PathBuf {
    core_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

/// After a registry change, poke a running daemon so it picks the change up
/// immediately. Offline is fine — the next `run` reads the registry anyway.
fn sync_if_online(core: &CoreConfig) -> Result<()> {
    let c = client::Client::from_core(core);
    if c.health().is_ok() {
        match c.reload() {
            Ok(v) => println!(
                "daemon synced: {}",
                v.get("result").and_then(|r| r.as_str()).unwrap_or("reloaded")
            ),
            Err(e) => eprintln!("xkeeper: warning: daemon sync failed: {e:#}"),
        }
    }
    Ok(())
}

fn client_cmd(core_path: &Path, f: impl FnOnce(&client::Client) -> Result<()>) -> Result<()> {
    let core = client::load_core(core_path)?;
    let c = client::Client::from_core(&core);
    match f(&c) {
        Ok(()) => Ok(()),
        Err(e) => {
            std::process::exit(client::exit_code_of(&e));
        }
    }
}

fn action_cmd(core_path: &Path, name: &str, action: &str) -> Result<()> {
    client_cmd(core_path, |c| {
        let v = c.action(name, action)?;
        println!("{}", v.get("result").and_then(|r| r.as_str()).unwrap_or("done"));
        Ok(())
    })
}

// -- validate ---------------------------------------------------------------

fn validate(core_path: &Path, single: Option<&Path>) -> Result<()> {
    if let Some(p) = single {
        let core = client::load_core(core_path)?;
        let (raw, _) = config::AppRaw::load(p)?;
        let name = registry::name_from_dir(p);
        let app = config::resolve_app(&name, p, &raw, core.app_default.as_ref())?;
        config::validate_all(&[app.clone()])?;
        println!("OK: app {:?} with {} program(s)", app.name, app.programs.len());
        for prog in &app.programs {
            println!("  - {} <{}>", prog.name, prog.command);
        }
        return Ok(());
    }
    let (core, existed) = CoreConfig::load_or_default(core_path)?;
    if !existed {
        println!("OK: no core config at {} (built-in defaults in effect)", core_path.display());
    }
    let core_dir = core_dir_of(core_path);
    let apps = registry::list(&core, &core_dir)?;
    let mut resolved = Vec::new();
    for l in &apps {
        let (raw, _) = config::AppRaw::load(&l.path)?;
        resolved.push(config::resolve_app(&l.name, &l.path, &raw, core.app_default.as_ref())?);
    }
    config::validate_all(&resolved)?;
    println!(
        "OK: core config + {} registered app(s), {} program(s)",
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

// -- daemon -----------------------------------------------------------------

fn run_daemon(core_path: &Path, webui_listen: Option<String>) -> Result<()> {
    let (core, existed) = CoreConfig::load_or_default(core_path)
        .with_context(|| format!("failed to load core config {}", core_path.display()))?;
    if !existed {
        info!("no core config at {} — starting empty with built-in defaults", core_path.display());
    }

    // Honor RUST_LOG over the config level.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(&core.daemon.log_level),
    )
    .try_init()
    .ok();
    info!("core config: {}{}", core_path.display(), if existed { "" } else { " (absent)" });

    let core_dir = core_dir_of(core_path);
    let sup = Supervisor::new(core, &core_dir)?;
    sup.set_core_path(core_path);

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
    info!("xkeeper {} running (Ctrl+C to stop)", env!("CARGO_PKG_VERSION"));

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
