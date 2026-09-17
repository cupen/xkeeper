//! xkeeper — a cross-platform, application-layer process keeper with a TOML
//! layered config (global daemon config + per-app deployment files).
//!
//! `xkeeper run` is the daemon; everything else is a client: control
//! subcommands talk to the loopback HTTP API, and add/remove/list manage the
//! app registry (app_dir links).

mod action;
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
use clap::{CommandFactory, Parser, Subcommand};
use log::info;

use crate::config::{DaemonConfig, RestartPolicy};
use crate::registry::AddOptions;
use crate::supervisor::Supervisor;

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_CONFIG: i32 = 2;
/// Client contract: 3 = daemon unreachable (`add --apply` offline, etc.).
const EXIT_UNREACHABLE: i32 = 3;

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
    /// Run the daemon in the foreground (default when no subcommand given).
    /// The web console is config-driven: add a `[webui]` section to the
    /// daemon config (or `xkeeper config --set webui.listen=...` + reload)
    /// to enable it
    Run,
    /// Validate the daemon config + all registered apps, or one app file
    Validate {
        /// Optional path to a single app config file
        path: Option<PathBuf>,
    },
    /// Manage the daemon config file locally: --init / --set / --get /
    /// --delete / --edit. Actions are mutually exclusive — one class of
    /// action per call. Purely local file operations; a running daemon is
    /// only probed for a "reload needed" hint after a successful write.
    Config {
        /// Set a daemon config key (repeatable): --set port=8080 or
        /// --set webui.listen=127.0.0.1:9877. Strong-typed whitelist; unknown
        /// keys / type violations are rejected untouched and the result is
        /// validated before it is written atomically
        #[arg(long, value_name = "KEY=VALUE", conflicts_with_all = ["get", "delete", "edit", "init"])]
        set: Vec<String>,
        /// Print the effective value of a daemon config key (repeatable; a
        /// bare value per key; unset keys answer with the built-in default,
        /// e.g. `webui.listen` falls back to 127.0.0.1:9877)
        #[arg(long, value_name = "KEY", conflicts_with_all = ["delete", "edit", "init"])]
        get: Vec<String>,
        /// Remove a daemon config key or whole section (repeatable): bare
        /// keys are shorthand for [daemon] leaves, dotted paths like
        /// daemon.port or webui.listen name them directly, and a bare
        /// section name (`webui`) removes the whole table — deleting
        /// `[webui]` turns the console off. Idempotent — absent targets
        /// change nothing
        #[arg(long, value_name = "KEY", conflicts_with_all = ["edit", "init"])]
        delete: Vec<String>,
        /// Open the daemon config in $VISUAL/$EDITOR (vi/notepad), creating
        /// it when missing, then validate the result
        #[arg(long, conflicts_with = "init")]
        edit: bool,
        /// Initialize a fresh daemon config (refuses to overwrite) plus the
        /// app registry directory and an example.toml.sample template
        #[arg(long)]
        init: bool,
    },
    /// Overview of daemon and all programs
    Status,
    /// Start a program, or fan out over one app with --app
    Start {
        /// Program name (the bare form keeps per-program semantics)
        #[arg(required_unless_present = "app", conflicts_with = "app")]
        name: Option<String>,
        /// Start every program of this app, in the established order
        #[arg(long)]
        app: Option<String>,
    },
    /// Stop a program (graceful, then force after stop timeout), or every
    /// program of one app with --app
    Stop {
        /// Program name (the bare form keeps per-program semantics)
        #[arg(required_unless_present = "app", conflicts_with = "app")]
        name: Option<String>,
        /// Stop every program of this app, in reverse start order
        #[arg(long)]
        app: Option<String>,
    },
    /// Restart a program, or every program of one app with --app
    Restart {
        /// Program name (the bare form keeps per-program semantics)
        #[arg(required_unless_present = "app", conflicts_with = "app")]
        name: Option<String>,
        /// Restart every program of this app
        #[arg(long)]
        app: Option<String>,
    },
    /// Execute a program's declared custom action; the CLI exit code is the
    /// action's own exit code (call failures: 1, daemon unreachable: 3)
    Action { program: String, name: String },
    /// Send a whitelist signal (TERM INT HUP QUIT USR1 USR2) to a program's
    /// child process; unix only
    Signal { program: String, signal: String },
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
    /// Apply pending config changes (scope: all | <app> | <app> <program>).
    /// The literal `all` means everything (same as no argument) and is a
    /// reserved word — it can never select an app named "all".
    Apply {
        /// Restrict the apply to one app (and optionally one program);
        /// the literal `all` applies everything
        #[arg(default_value = None)]
        app: Option<String>,
        #[arg(default_value = None)]
        program: Option<String>,
        /// Also restart unchanged programs (user-stopped ones stay stopped)
        #[arg(long)]
        restart: bool,
    },
    /// Stop all programs and exit the daemon
    Shutdown,
    /// Register an app: a directory with xkeeper.toml, a config file path,
    /// or an executable program (scaffolds a fresh config in app_dir).
    /// Re-running `add` on the same executable regenerates its whole config
    /// from the given flags — manual edits to the generated file are
    /// overwritten.
    Add {
        /// App directory (containing xkeeper.toml), a config file path, or
        /// an executable program to scaffold a config from
        #[arg(default_value = ".")]
        path: PathBuf,
        /// App name (default: directory name, or the executable's file
        /// name without extension for scaffolds; `all` is reserved)
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
        /// (scaffold) startup args as one string, split like a shell line:
        /// --args "--port 8080 --mode x"
        #[arg(long)]
        args: Option<String>,
        /// (scaffold) environment variable K=V; repeat for more
        #[arg(long = "env")]
        envs: Vec<String>,
        /// (scaffold) working directory for the program
        /// (default: the directory `add` runs in)
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Apply this app right after registering (errors out when the
        /// daemon is offline — the config is kept either way)
        #[arg(long)]
        apply: bool,
    },
    /// Unregister an app: external deployment configs are kept; a
    /// scaffold-generated record (a real file inside app_dir) is deleted
    Remove { name: String },
    /// List registered apps
    List,
    /// Interactive shell client (like supervisorctl)
    Shell {
        /// Execute a single shell command and exit (script-friendly)
        #[arg(short = 'e', long = "exec")]
        cmd: Option<String>,
    },
    /// Install/uninstall xkeeper as a system service (Linux systemd; root required)
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
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
        /// Full path to the unit file to write (must end in .service)
        #[arg(long, value_name = "PATH", default_value = service::DEFAULT_UNIT_FILE)]
        unit_file: PathBuf,
        /// Run the service as this user (User= in the unit)
        #[arg(long)]
        user: Option<String>,
    },
    /// Stop, disable and remove the systemd unit
    Uninstall {
        /// Full path to the unit file to remove (must end in .service)
        #[arg(long, value_name = "PATH", default_value = service::DEFAULT_UNIT_FILE)]
        unit_file: PathBuf,
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
    // silently. `run` (and bare `xkeeper`) init their own logger from the
    // config's log_level.
    if !matches!(&cli.cmd, None | Some(Cmd::Run)) {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .try_init()
            .ok();
    }
    let config_path = config_path_of(cli);
    match &cli.cmd {
        Some(Cmd::Run) | None => run_daemon(&config_path),
        Some(Cmd::Validate { path }) => validate(&config_path, path.as_deref()),
        Some(Cmd::Config {
            set,
            get,
            delete,
            edit,
            init,
        }) => {
            if set.is_empty() && get.is_empty() && delete.is_empty() && !*edit && !*init {
                // `xkeeper config` with no action flag prints its own usage
                // and exits 0 (a global --config alone must stay legal, so
                // clap's arg_required_else_help does not apply here).
                let mut cmd = Cli::command();
                let sub = cmd
                    .find_subcommand_mut("config")
                    .expect("config subcommand exists");
                println!("{}", sub.render_help());
                return Ok(());
            }
            let code = config_cmd(&config_path, set, get, delete, *edit, *init)?;
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
        Some(Cmd::Start { name, app }) => match (name, app) {
            (Some(n), _) => action_cmd(&config_path, &n, "start"),
            (None, Some(a)) => fanout_cmd(&config_path, &a, "start"),
            (None, None) => unreachable!("clap enforces name xor app"),
        },
        Some(Cmd::Stop { name, app }) => match (name, app) {
            (Some(n), _) => action_cmd(&config_path, &n, "stop"),
            (None, Some(a)) => fanout_cmd(&config_path, &a, "stop"),
            (None, None) => unreachable!("clap enforces name xor app"),
        },
        Some(Cmd::Restart { name, app }) => match (name, app) {
            (Some(n), _) => action_cmd(&config_path, &n, "restart"),
            (None, Some(a)) => fanout_cmd(&config_path, &a, "restart"),
            (None, None) => unreachable!("clap enforces name xor app"),
        },
        Some(Cmd::Action { program, name }) => custom_action_cmd(&config_path, program, name),
        Some(Cmd::Signal { program, signal }) => client_cmd(&config_path, |c| {
            let v = c.signal(program, signal)?;
            println!(
                "{}",
                v.get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("delivered")
            );
            Ok(())
        }),
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
        Some(Cmd::Apply {
            app,
            program,
            restart,
        }) => client_cmd(&config_path, |c| {
            let v = c.apply(app.as_deref(), program.as_deref(), *restart)?;
            println!(
                "{}",
                v.get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("applied")
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
            args,
            envs,
            workdir,
            apply: apply_now,
        }) => {
            let config = client::load_config(&config_path)?;
            let config_dir = config_dir_of(&config_path);

            // v0.1 single-file configs are recognized and converted first
            // (only real .toml candidates — anything else may be a binary
            // executable destined for the scaffold flow).
            let mut target = path.clone();
            if path.is_file() && path.extension().map(|x| x == "toml").unwrap_or(false) {
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

            // --env K=V (repeatable): missing '=' or an empty key is a
            // config-level error.
            let mut env = std::collections::BTreeMap::new();
            for kv in envs {
                let (k, v) = match kv.split_once('=') {
                    Some(pair) => pair,
                    None => {
                        eprintln!("xkeeper: error: --env expects K=V, got {kv:?}");
                        std::process::exit(EXIT_CONFIG);
                    }
                };
                if k.trim().is_empty() {
                    eprintln!("xkeeper: error: --env key must not be empty: {kv:?}");
                    std::process::exit(EXIT_CONFIG);
                }
                env.insert(k.to_string(), v.to_string());
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
                args: args.clone(),
                env: if env.is_empty() { None } else { Some(env) },
                workdir: workdir.clone(),
            };
            let result = registry::add(&config, &config_dir, &target, &opts)?;
            if result.scaffolded {
                println!("generated {}", result.file.display());
                println!("app[{}] program[{}]", result.app, result.program);
                println!("  command:  {}", result.command);
                if result.args.is_empty() {
                    println!("  args:     (none)");
                } else {
                    println!("  args:     {}", shell_join(&result.args));
                }
                if result.env.is_empty() {
                    println!("  env:      (none)");
                } else {
                    let kv: Vec<String> =
                        result.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    println!("  env:      {}", kv.join(" "));
                }
                println!("  work_dir: {}", result.work_dir.display());
                if result.changed {
                    println!(
                        "changed: run `xkeeper apply {}` (or bare `xkeeper apply`) to take effect",
                        result.app
                    );
                } else {
                    println!("no change: generated config is already up to date");
                }
            } else {
                println!("app[{}] registered", result.app);
            }
            sync_if_online(&config)?;

            if *apply_now {
                match apply_after_add(&config, &result.app, &result.file) {
                    AddApplyOutcome::Applied(text) => println!("{text}"),
                    AddApplyOutcome::Offline(msg) => {
                        eprintln!("xkeeper: error: {msg}");
                        std::process::exit(EXIT_UNREACHABLE);
                    }
                    AddApplyOutcome::Failed(msg, code) => {
                        eprintln!("xkeeper: error: {msg}");
                        std::process::exit(code);
                    }
                }
            }
            Ok(())
        }
        Some(Cmd::Remove { name }) => {
            let config = client::load_config(&config_path)?;
            let config_dir = config_dir_of(&config_path);
            let removed = registry::remove(&config, &config_dir, name)?;
            if removed.deleted_file {
                println!(
                    "app[{name}] unregistered (generated config file removed: {})",
                    removed.path.display()
                );
            } else {
                println!(
                    "app[{name}] unregistered (deployment config kept at {})",
                    removed.path.display()
                );
            }
            sync_if_online(&config)?;
            Ok(())
        }
        Some(Cmd::Shell { cmd }) => shell::run(&config_path, cmd.as_deref()),
        Some(Cmd::Service { cmd }) => {
            let (action, unit_file, user, force, now) = match cmd {
                ServiceCmd::Install {
                    now,
                    force,
                    unit_file,
                    user,
                } => ("install", unit_file, user, *force, *now),
                ServiceCmd::Uninstall { unit_file } => ("uninstall", unit_file, &None, false, false),
            };
            let opts = service::ServiceOptions {
                unit_file: unit_file.clone(),
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
            let apps = registry::ListedApp::good(registry::list(&config, &config_dir)?);
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

/// Render argv as one paste-able line (quoting mirrors the quote-aware
/// `config::split_command` rules, so the output round-trips).
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars().any(|c| c.is_whitespace()) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
        // reload = rescan + detect only: the registry change enters pending;
        // `xkeeper apply` makes it take effect (app-registry spec).
        c.reload().with_context(|| {
            "registry change persisted, but the running daemon failed to rescan; \
             run `xkeeper reload` once the problem is fixed, then `xkeeper apply`"
        })?;
        println!("daemon synced: registration is pending — run `xkeeper apply` to take effect");
    }
    Ok(())
}

fn client_cmd(config_path: &Path, f: impl FnOnce(&client::Client) -> Result<()>) -> Result<()> {
    let config = client::load_config(config_path)?;
    let c = client::Client::from_config(&config);
    match f(&c) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Exit with the class code, but never silently — the message is
            // the only trace of what the control plane answered.
            eprintln!("xkeeper: error: {e:#}");
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

/// `xkeeper action <program> <action>`: print the output tail and pass the
/// action's own exit code through (control-plane spec: 调用失败 1、守护不可达 3、
/// 超时 1 — anything else is the action's exit status).
fn custom_action_cmd(config_path: &Path, program: &str, action: &str) -> Result<()> {
    let config = client::load_config(config_path)?;
    let c = client::Client::from_config(&config);
    let v = match c.custom_action(program, action) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("xkeeper: error: {e:#}");
            std::process::exit(client::exit_code_of(&e));
        }
    };
    if let Some(out) = v
        .get("output")
        .and_then(|o| o.as_str())
        .filter(|s| !s.is_empty())
    {
        println!("{out}");
    }
    if v.get("timed_out")
        .and_then(|t| t.as_bool())
        .unwrap_or(false)
    {
        eprintln!("xkeeper: error: action {program}.{action} timed out (process tree killed)");
        std::process::exit(EXIT_ERROR);
    }
    match v.get("exit_code").and_then(|e| e.as_i64()) {
        Some(0) | None => Ok(()),
        Some(code) => {
            eprintln!("action {program}.{action} exited with code {code}");
            std::process::exit(code.clamp(i32::MIN as i64, 255) as i32);
        }
    }
}

/// `xkeeper start|stop|restart --app <name>`: app-level fan-out; the server
/// expands it in the established order and reports per-program results.
fn fanout_cmd(config_path: &Path, app: &str, verb: &str) -> Result<()> {
    client_cmd(config_path, |c| {
        let v = c.app_action(app, verb)?;
        println!(
            "{}",
            v.get("result").and_then(|r| r.as_str()).unwrap_or("done")
        );
        Ok(())
    })
}

// -- add --apply --------------------------------------------------------------

/// Outcome of `add --apply`: the caller prints `Applied`/the message and
/// terminates with the outcome's exit code (0 for `Applied`). Kept free of
/// `process::exit` so tests can drive the offline/API-error branches.
#[derive(Debug)]
enum AddApplyOutcome {
    /// The control-plane result text (rendered apply result).
    Applied(String),
    /// Daemon offline: message explains that the config was generated and
    /// names the remedy (`xkeeper apply <app>`); exit code 3.
    Offline(String),
    /// The daemon answered but the apply failed: message + exit code.
    Failed(String, i32),
}

/// `add --apply`: apply one app right after registering (app scope only —
/// other apps' pending is untouched). An explicit request that did not take
/// effect must be visible: offline is a non-zero exit with the remedy.
fn apply_after_add(config: &DaemonConfig, app: &str, file: &Path) -> AddApplyOutcome {
    let c = client::Client::from_config(config);
    if c.health().is_err() {
        return AddApplyOutcome::Offline(format!(
            "daemon is offline — the config for app[{app}] is at {}; \
             start the daemon and run `xkeeper apply {app}` to take effect",
            file.display()
        ));
    }
    match c.apply(Some(app), None, false) {
        Ok(v) => AddApplyOutcome::Applied(
            v.get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("applied")
                .to_string(),
        ),
        Err(e) => AddApplyOutcome::Failed(format!("{e:#}"), client::exit_code_of(&e)),
    }
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
    let apps = registry::ListedApp::good(registry::list(&config, &config_dir)?);
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

// -- config subcommand ------------------------------------------------------

/// One short health probe after a config write (design D6): online → a reload
/// hint, offline/failure → an offline note. Never retries, never blocks and
/// never fails the action; the probe address comes from the pre-modification
/// config (a running daemon still listens on the old values).
fn config_sync_hint(config: &DaemonConfig) -> &'static str {
    let c = client::Client::from_config(config);
    if c.health().is_ok() {
        "note: daemon is online — run `xkeeper reload` (then `xkeeper apply` if apps changed) \
         for the new config to take effect"
    } else {
        "note: daemon is offline — the change takes effect at next start"
    }
}

/// `xkeeper config` body: perform exactly one action class (clap enforces
/// the mutual exclusion). Returns the process exit code so tests can drive
/// the branches; config-level problems print to stderr and return
/// `EXIT_CONFIG` (2), IO errors bubble up (`EXIT_ERROR` via main).
fn config_cmd(
    config_path: &Path,
    sets: &[String],
    gets: &[String],
    deletes: &[String],
    edit: bool,
    init: bool,
) -> Result<i32> {
    // The pre-action config feeds the online probe: the daemon still listens
    // on the values that were in effect before this write. A broken config
    // file must not block the action itself (the probe is advisory, and
    // `--edit` is exactly how a broken file gets repaired) — the probe then
    // falls back to the built-in default address.
    let pre_config = client::load_config(config_path).unwrap_or_else(|_| DaemonConfig::empty());

    if !gets.is_empty() {
        // Validate every key first — an unknown key answers nothing.
        let text = match std::fs::read_to_string(config_path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", config_path.display()));
            }
        };
        let mut out = Vec::new();
        for k in gets {
            match config::config_effective_value(text.as_deref(), k) {
                Ok(v) => out.push((k.clone(), v)),
                Err(e) => {
                    eprintln!("xkeeper: error: {e:#}");
                    return Ok(EXIT_CONFIG);
                }
            }
        }
        if out.len() == 1 {
            println!("{}", out[0].1);
        } else {
            for (k, v) in &out {
                println!("{k}={v}");
            }
        }
        return Ok(EXIT_OK);
    }

    if !sets.is_empty() {
        let parsed = match config::parse_sets(sets) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("xkeeper: error: {e:#}");
                return Ok(EXIT_CONFIG);
            }
        };
        if !config_path.exists() {
            eprintln!(
                "xkeeper: error: no daemon config at {} — run `xkeeper config --init` first",
                config_path.display()
            );
            return Ok(EXIT_CONFIG);
        }
        let text = std::fs::read_to_string(config_path)
            .with_context(|| format!("cannot read {}", config_path.display()))?;
        let rendered = match config::config_apply_sets(&text, &parsed) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("xkeeper: error: {e:#}");
                return Ok(EXIT_CONFIG);
            }
        };
        if let Err(e) = config::atomic_write_daemon_config(config_path, &rendered) {
            eprintln!("xkeeper: error: {e:#}");
            return Ok(EXIT_CONFIG);
        }
        for (k, kv) in &parsed {
            println!("set {k} = {kv}");
        }
        println!("{}", config_sync_hint(&pre_config));
        return Ok(EXIT_OK);
    }

    if !deletes.is_empty() {
        if !config_path.exists() {
            eprintln!(
                "xkeeper: error: no daemon config at {} — run `xkeeper config --init` first",
                config_path.display()
            );
            return Ok(EXIT_CONFIG);
        }
        let text = std::fs::read_to_string(config_path)
            .with_context(|| format!("cannot read {}", config_path.display()))?;
        let rendered = match config::config_apply_deletes(&text, deletes) {
            Ok(config::DeleteOutcome::Changed(r)) => Some(r),
            Ok(config::DeleteOutcome::NoChange) => None,
            Err(e) => {
                eprintln!("xkeeper: error: {e:#}");
                return Ok(EXIT_CONFIG);
            }
        };
        match rendered {
            Some(r) => {
                if let Err(e) = config::atomic_write_daemon_config(config_path, &r) {
                    eprintln!("xkeeper: error: {e:#}");
                    return Ok(EXIT_CONFIG);
                }
                for k in deletes {
                    println!("deleted {k}");
                }
                println!("{}", config_sync_hint(&pre_config));
            }
            None => {
                println!("no change: {} was not set", deletes.join(", "));
            }
        }
        return Ok(EXIT_OK);
    }

    if edit {
        let code = edit_config(config_path)?;
        if code == EXIT_OK {
            println!("{}", config_sync_hint(&pre_config));
        }
        return Ok(code);
    }

    if init {
        if config_path.exists() {
            eprintln!(
                "xkeeper: error: {} already exists — refusing to initialize over an existing \
                 config (delete it first or use `xkeeper config --edit`)",
                config_path.display()
            );
            return Ok(EXIT_CONFIG);
        }
        let rep = config::config_init(config_path)?;
        println!("created {}", rep.config_file.display());
        println!("created app dir {}", rep.app_dir.display());
        if rep.sample_created {
            println!("created {}", rep.sample_file.display());
        } else {
            println!("kept existing {}", rep.sample_file.display());
        }
        println!(
            "next: `xkeeper config --edit` to customize, `xkeeper add <dir>` to register apps"
        );
        println!("{}", config_sync_hint(&pre_config));
        return Ok(EXIT_OK);
    }

    unreachable!("an action was selected (dispatch checks the flags)")
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
/// with `EDIT_SEED`, so first-time setup via `xkeeper config --edit` works.
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
/// `run` is the only daemon entry. The web console is config-driven: its
/// initial state comes from the `[webui]` section of the daemon config
/// (absent = off), and `xkeeper reload` converges it later at runtime.
fn run_daemon(config_path: &Path) -> Result<()> {
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

    // Capture the console intent before `config` is moved into the supervisor.
    let webui_intent = supervisor::WebuiIntent::from_config(&config);

    let config_dir = config_dir_of(config_path);
    let sup = Supervisor::new(config, &config_dir)?;
    sup.set_config_path(config_path);

    // Web console manager: converges the running console to the latest
    // intent (published here for startup and by `reload` later). Bind
    // failures degrade non-fatally; the manager exits with the daemon.
    sup.webui.set_desired(webui_intent);
    {
        let sup2 = sup.clone();
        std::thread::Builder::new()
            .name("webui-manager".into())
            .spawn(move || supervisor::webui_manager_loop(sup2))
            .context("failed to spawn webui manager thread")?;
    }

    // Metrics sampler: 1 Hz read-only sideband (CPU/RSS + log rates). Runs
    // for the daemon; exits with it. Its failure only degrades metric
    // fields to null.
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
#[cfg(unix)] // every test in here needs a real editor process (/bin/sh)
mod edit_tests {
    static EDIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
        let _guard = EDIT_TEST_LOCK.lock().unwrap();
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
        // Spawning a real editor process here is flaky under full-parallel
        // test load (observed sporadic failures only in `cargo test` runs);
        // serialize the editor-spawning tests.
        let _guard = EDIT_TEST_LOCK.lock().unwrap();
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

    #[test]
    #[cfg(unix)]
    fn editor_abnormal_exit_is_an_error() {
        // Spec: 编辑器异常退出以非零码退出（Err bubbles up to EXIT_ERROR）.
        let _guard = EDIT_TEST_LOCK.lock().unwrap();
        let dir = temp_root("abnormal");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.toml");
        std::fs::write(&path, "[daemon]\nport = 8080\n").unwrap();
        let script = dir.join("fake-editor.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let r = edit_config_with(script.to_str().unwrap(), &path);
        assert!(
            r.is_err(),
            "an editor that exits non-zero must fail the command"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[daemon]\nport = 8080\n",
            "the file keeps its pre-edit content"
        );
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
            err.to_string().contains("failed to rescan"),
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

    /// add --apply offline: the config is already on disk, so the failure
    /// must be visible (exit 3) and name the remedy — never a silent no-op.
    #[test]
    fn add_apply_offline_reports_generated_config_and_remedy() {
        let config = config_with_port(1).unwrap(); // loopback:1 is never open
        match apply_after_add(&config, "abc", Path::new("apps/abc.toml")) {
            AddApplyOutcome::Offline(msg) => {
                assert!(msg.contains("offline"), "names the offline cause: {msg}");
                assert!(msg.contains("apps/abc.toml"), "names the config: {msg}");
                assert!(msg.contains("xkeeper apply abc"), "names the remedy: {msg}");
            }
            other => panic!("offline add --apply must be Offline, got {other:?}"),
        }
    }

    /// add --apply online: one health probe + one app-scoped apply; the
    /// rendered control-plane result comes back (fake daemon, sync_tests
    /// style).
    #[test]
    fn add_apply_online_runs_app_scoped_apply() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().unwrap();
                // Read the full request: headers to the blank line, then the
                // Content-Length body (it may arrive in a separate segment).
                let mut raw = Vec::new();
                let mut byte = [0u8; 1];
                while !raw.ends_with(b"\r\n\r\n") {
                    if std::io::Read::read(&mut sock, &mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    raw.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&raw).to_string();
                let len: usize = head
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body_bytes = vec![0u8; len];
                if len > 0 {
                    std::io::Read::read_exact(&mut sock, &mut body_bytes).unwrap();
                }
                let req = format!(
                    "{}{}",
                    head.lines().next().unwrap_or(""),
                    String::from_utf8_lossy(&body_bytes)
                );
                let (status, body) = if req.starts_with("GET /v1/health") {
                    ("200 OK", "{}".to_string())
                } else {
                    assert!(req.starts_with("POST /v1/apply"), "apply is POSTed: {req}");
                    assert!(
                        req.contains("\"app\": \"abc\"") || req.contains("\"app\":\"abc\""),
                        "app scope rides in the body: {req}"
                    );
                    (
                        "200 OK",
                        "{\"result\":\"changed:\\n  abc.abc -> start\"}".to_string(),
                    )
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
        match apply_after_add(&config, "abc", Path::new("apps/abc.toml")) {
            AddApplyOutcome::Applied(text) => {
                assert!(text.contains("abc.abc -> start"), "result text: {text}");
            }
            other => panic!("online add --apply must be Applied, got {other:?}"),
        }
        server.join().unwrap();
    }
}

#[cfg(test)]
mod config_cmd_tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xk-cmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // -- clap: parsing and action mutual exclusion (task 1.1) ----------------

    #[test]
    fn clap_config_repeated_set_parses() {
        let cli = Cli::try_parse_from(["xkeeper", "config", "--set", "port=1", "--set", "host=x"])
            .unwrap();
        match cli.cmd {
            Some(Cmd::Config {
                set,
                get,
                delete,
                edit,
                init,
            }) => {
                assert_eq!(set, vec!["port=1", "host=x"]);
                assert!(get.is_empty() && delete.is_empty() && !edit && !init);
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn clap_config_bare_action_flags_parse() {
        let cli = Cli::try_parse_from(["xkeeper", "config", "--edit"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Config { edit: true, .. })));
        let cli = Cli::try_parse_from(["xkeeper", "config", "--init"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Config { init: true, .. })));
        // Bare `config` parses too — dispatch then prints help with exit 0.
        let cli = Cli::try_parse_from(["xkeeper", "config"]).unwrap();
        match cli.cmd {
            Some(Cmd::Config {
                set,
                get,
                delete,
                edit,
                init,
            }) => {
                assert!(set.is_empty() && get.is_empty() && delete.is_empty());
                assert!(!edit && !init);
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn clap_config_actions_are_mutually_exclusive() {
        // One class of action per call (spec: 动作互斥).
        let combos: &[&[&str]] = &[
            &["--init", "--get", "port"],
            &["--set", "port=1", "--delete", "port"],
            &["--set", "port=1", "--get", "port"],
            &["--get", "port", "--delete", "port"],
            &["--edit", "--init"],
            &["--edit", "--set", "port=1"],
            &["--edit", "--get", "port"],
            &["--edit", "--delete", "port"],
            &["--init", "--set", "port=1"],
            &["--init", "--delete", "port"],
        ];
        for combo in combos {
            let mut argv = vec!["xkeeper", "config"];
            argv.extend_from_slice(combo);
            let r = Cli::try_parse_from(&argv);
            assert!(r.is_err(), "config {combo:?} must be rejected");
        }
    }

    #[test]
    fn config_no_action_prints_help_and_exits_zero() {
        // Spec: 无任何动作 flag 时打印五个动作 flag 的用法说明并以 0 退出。
        // dispatch renders the subcommand help and returns Ok (exit 0); the
        // usage content itself is asserted by the e2e config pass (K13).
        let cli = Cli::try_parse_from(["xkeeper", "config"]).unwrap();
        dispatch(&cli).expect("bare `xkeeper config` must exit 0");
    }

    // -- orchestration: exit codes over real temp files -----------------------

    #[test]
    fn set_missing_file_is_rejected_with_init_hint() {
        let dir = temp_root("set-missing");
        let path = dir.join("daemon.toml");
        let code = config_cmd(&path, &["port=8080".to_string()], &[], &[], false, false).unwrap();
        assert_eq!(code, EXIT_CONFIG, "missing file set must exit 2");
        assert!(!path.exists(), "a rejected set must not create the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_missing_file_is_rejected() {
        let dir = temp_root("del-missing");
        let path = dir.join("daemon.toml");
        let code = config_cmd(&path, &[], &[], &["port".to_string()], false, false).unwrap();
        assert_eq!(code, EXIT_CONFIG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_missing_file_answers_defaults_and_exits_zero() {
        let dir = temp_root("get-missing");
        let path = dir.join("nested").join("daemon.toml");
        let code = config_cmd(&path, &[], &["port".to_string()], &[], false, false).unwrap();
        assert_eq!(code, EXIT_OK, "--get on a missing file is all defaults");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_get_delete_roundtrip_over_a_real_file() {
        let dir = temp_root("roundtrip");
        let path = dir.join("daemon.toml");
        std::fs::write(&path, "[daemon]\nport = 1234\n").unwrap();

        // set: exit 0, value lands on disk.
        let code = config_cmd(
            &path,
            &["port=8080".to_string(), "log_level=debug".to_string()],
            &[],
            &[],
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, EXIT_OK);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("port = 8080") && text.contains("log_level = \"debug\""),
            "{text}"
        );
        // A rejected set leaves the file byte-identical.
        let before = text.clone();
        let code = config_cmd(&path, &["port=abc".to_string()], &[], &[], false, false).unwrap();
        assert_eq!(code, EXIT_CONFIG, "type violation exits 2");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // delete: exit 0, key gone; second delete of an absent key is still 0.
        let code = config_cmd(&path, &[], &[], &["port".to_string()], false, false).unwrap();
        assert_eq!(code, EXIT_OK);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("port"), "{text}");
        assert!(
            text.contains("log_level = \"debug\""),
            "untouched key kept: {text}"
        );
        let before = text.clone();
        let code = config_cmd(&path, &[], &[], &["port".to_string()], false, false).unwrap();
        assert_eq!(code, EXIT_OK, "absent-key delete is idempotent success");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_unknown_key_is_rejected_with_config_exit_code() {
        let dir = temp_root("get-unknown");
        let path = dir.join("daemon.toml");
        std::fs::write(&path, "[daemon]\nport = 8080\n").unwrap();
        let code = config_cmd(&path, &[], &["foo".to_string()], &[], false, false).unwrap();
        assert_eq!(code, EXIT_CONFIG, "an unknown --get key must exit 2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_multiple_targets_in_one_call() {
        // --delete is repeatable (spec): several targets go in one call.
        let dir = temp_root("del-multi");
        let path = dir.join("daemon.toml");
        std::fs::write(
            &path,
            "[daemon]\n# port note\nport = 8080\nhost = \"0.0.0.0\"\nlog_level = \"debug\"\n",
        )
        .unwrap();
        let code = config_cmd(
            &path,
            &[],
            &[],
            &["port".to_string(), "host".to_string()],
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, EXIT_OK);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("port") && !text.contains("host"), "{text}");
        assert!(
            text.contains("log_level = \"debug\""),
            "the untouched key survives: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_creates_workspace_and_second_init_is_rejected() {
        let dir = temp_root("init");
        let path = dir.join("conf").join("daemon.toml");
        let code = config_cmd(&path, &[], &[], &[], false, true).unwrap();
        assert_eq!(code, EXIT_OK);
        assert!(path.is_file());
        assert!(dir.join("conf/apps/example.toml.sample").is_file());
        // Second init on the existing config: exit 2, file untouched.
        let before = std::fs::read_to_string(&path).unwrap();
        let code = config_cmd(&path, &[], &[], &[], false, true).unwrap();
        assert_eq!(code, EXIT_CONFIG);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_then_set_then_get_roundtrip() {
        let dir = temp_root("init-flow");
        let path = dir.join("daemon.toml");
        assert_eq!(
            config_cmd(&path, &[], &[], &[], false, true).unwrap(),
            EXIT_OK
        );
        // set on the init-produced file works (8-key template is legal).
        assert_eq!(
            config_cmd(
                &path,
                &["monitor_interval=2.5".to_string()],
                &[],
                &[],
                false,
                false
            )
            .unwrap(),
            EXIT_OK
        );
        assert_eq!(
            config_cmd(
                &path,
                &[],
                &["monitor_interval".to_string()],
                &[],
                false,
                false
            )
            .unwrap(),
            EXIT_OK
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- online probe (task 4.1): mock daemon, both branches ------------------

    fn config_with_port(port: u16) -> Result<DaemonConfig> {
        Ok(toml::from_str(&format!(
            "[daemon]\nhost = \"127.0.0.1\"\nport = {port}\n"
        ))?)
    }

    /// A /v1/health answerer on a free port, sync_tests style.
    fn spawn_health_mock() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn sync_hint_online_suggests_reload() {
        let (listener, port) = spawn_health_mock();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = std::io::Read::read(&mut sock, &mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(
                req.starts_with("GET /v1/health"),
                "probe hits /v1/health: {req}"
            );
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                        Content-Length: 2\r\nConnection: close\r\n\r\n{}";
            std::io::Write::write_all(&mut sock, resp.as_bytes()).unwrap();
        });
        let config = config_with_port(port).unwrap();
        let msg = config_sync_hint(&config);
        assert!(msg.contains("online") && msg.contains("reload"), "{msg}");
        server.join().unwrap();
    }

    #[test]
    fn sync_hint_offline_names_next_start() {
        let config = config_with_port(1).unwrap(); // loopback:1 is never open
        let msg = config_sync_hint(&config);
        assert!(
            msg.contains("offline") && msg.contains("next start"),
            "{msg}"
        );
    }
}
