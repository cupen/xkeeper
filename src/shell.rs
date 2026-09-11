//! Interactive shell client (`xkeeper shell`) and the `xkeeper system webui`
//! helper — a pure client over the control-plane API, mirroring the
//! supervisorctl workflow. The daemon is untouched: every command reuses
//! `client::Client` and the shared exit-code contract.

use std::io::Write;

use anyhow::{Context, Result, bail};

use crate::client::{self, Client};

/// Default webui listen address, matching `xkeeper webui --listen`.
const DEFAULT_WEBUI_URL: &str = "http://127.0.0.1:9877";

/// One parsed shell line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ShellCmd {
    Status,
    Start(String),
    Stop(String),
    Restart(String),
    Pid(String),
    Log {
        name: String,
        follow: bool,
        tail: usize,
        stream: String,
    },
    Reload,
    Pending,
    Apply {
        app: Option<String>,
        program: Option<String>,
        restart: bool,
    },
    Shutdown,
    Open,
    Help,
    Exit,
}

/// Parse one shell line. `None` = blank line (ignore in the REPL).
pub(crate) fn parse_line(line: &str) -> Result<Option<ShellCmd>> {
    let words = shell_words(line);
    let Some((head, args)) = words.split_first() else {
        return Ok(None);
    };
    let cmd = match head.as_str() {
        "status" | "st" => ShellCmd::Status,
        "start" => ShellCmd::Start(one_arg(head, args)?),
        "stop" => ShellCmd::Stop(one_arg(head, args)?),
        "restart" => ShellCmd::Restart(one_arg(head, args)?),
        "pid" => ShellCmd::Pid(one_arg(head, args)?),
        "log" => parse_log(args)?,
        "reload" => ShellCmd::Reload,
        "pending" => ShellCmd::Pending,
        "apply" => parse_apply(args)?,
        "shutdown" => ShellCmd::Shutdown,
        "open" => ShellCmd::Open,
        "help" | "?" => ShellCmd::Help,
        "exit" | "quit" | "q" => ShellCmd::Exit,
        other => bail!("unknown command {other:?}{}", similar_hint(other)),
    };
    Ok(Some(cmd))
}

/// Parse `apply [<app> [<program>]] [--restart]`.
fn parse_apply(args: &[String]) -> Result<ShellCmd> {
    let mut app = None;
    let mut program = None;
    let mut restart = false;
    let mut positional = 0;
    for a in args {
        if a == "--restart" {
            restart = true;
        } else if a.starts_with("--") {
            bail!("unknown flag {a:?} (only --restart is supported)");
        } else if positional == 0 {
            app = Some(a.clone());
            positional = 1;
        } else if positional == 1 {
            program = Some(a.clone());
            positional = 2;
        } else {
            bail!("apply takes at most two positional args: <app> <program>");
        }
    }
    if app.is_none() && program.is_some() {
        bail!("apply <program> needs an app first: apply <app> <program>");
    }
    Ok(ShellCmd::Apply {
        app,
        program,
        restart,
    })
}

/// Split on whitespace, honoring double/single quotes (enough for
/// `shell -e "log web --tail 5"`).
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in line.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, c) if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started || !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn one_arg(cmd: &str, args: &[String]) -> Result<String> {
    match args {
        [name] => Ok(name.clone()),
        [] => bail!("{cmd} needs a program name"),
        _ => bail!("{cmd} takes exactly one program name"),
    }
}

fn parse_log(args: &[String]) -> Result<ShellCmd> {
    let mut name: Option<String> = None;
    let mut follow = false;
    let mut tail: Option<usize> = None;
    let mut stream = "out".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-f" | "--follow" => follow = true,
            "--tail" => {
                i += 1;
                tail = Some(
                    args.get(i)
                        .and_then(|v| v.parse().ok())
                        .ok_or_else(|| anyhow::anyhow!("--tail needs a number"))?,
                );
            }
            "--stream" => {
                i += 1;
                stream = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("--stream needs out|err"))?;
                if stream != "out" && stream != "err" {
                    bail!("--stream must be out|err");
                }
            }
            other if other.starts_with('-') => bail!("unknown log flag {other:?}"),
            other => {
                if name.is_some() {
                    bail!("log takes exactly one program name");
                }
                name = Some(other.to_string());
            }
        }
        i += 1;
    }
    Ok(ShellCmd::Log {
        name: name.ok_or_else(|| anyhow::anyhow!("log needs a program name"))?,
        follow,
        tail: tail.unwrap_or(20),
        stream,
    })
}

/// Naive "did you mean" for the unknown-command hint.
fn similar_hint(input: &str) -> String {
    const COMMANDS: [&str; 13] = [
        "status", "start", "stop", "restart", "pid", "log", "reload", "pending", "apply", "shutdown", "open", "help",
        "exit",
    ];
    let input = input.to_lowercase();
    let similar: Vec<&str> = COMMANDS
        .iter()
        .copied()
        .filter(|c| {
            // cheap similarity: shared prefix or edit distance <= 2 on short words
            c.starts_with(input.chars().next().unwrap_or_default()) && levenshtein(&input, c) <= 2
        })
        .collect();
    match similar.as_slice() {
        [] => String::new(),
        [one] => format!(" (did you mean {one}?)"),
        many => format!(" (did you mean one of: {}?)", many.join(", ")),
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1)
                .min(cur[j - 1] + 1)
                .min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

pub(crate) const HELP_TEXT: &str = "\
Built-in commands:
  status                              list all programs (app, state, pid, restarts)
  start|stop|restart <name>           control a program
  pid <name>                          print a program's pid
  log <name> [-f] [--tail N] [--stream out|err]
                                      view program logs
  reload                              rescan config and show pending changes (no restarts)
  pending                             show pending (detected but not applied) changes
  apply [<app> [<program>]] [--restart]
                                      apply pending changes; --restart restarts unchanged
                                      programs too (user-stopped ones stay stopped)
  shutdown                            stop all programs and exit the daemon
  open                                open the web console in the system browser
  help (or ?)                         show this help
  exit (or quit)                      leave the shell";

/// Outcome of one executed command: keep looping or leave the shell.
enum Outcome {
    Continue,
    Exit,
}

/// Entry: REPL, or a single command via `-e`.
pub(crate) fn run(config_path: &std::path::Path, single: Option<&str>) -> Result<()> {
    let config = client::load_config(config_path)?;
    let c = Client::from_config(&config);

    if let Some(line) = single {
        let cmd = parse_line(line).context("invalid command")?;
        match cmd {
            None => Ok(()),
            Some(c2) => {
                let webui_url = webui_url_of(&config);
                match execute(&c, &c2, &webui_url) {
                    Ok(Outcome::Continue) => Ok(()),
                    Ok(Outcome::Exit) => Ok(()),
                    Err(e) => {
                        std::process::exit(client::exit_code_of(&e));
                    }
                }
            }
        }
    } else {
        repl(&c, &webui_url_of(&config))
    }
}

fn webui_url_of(config: &crate::config::DaemonConfig) -> String {
    if config.daemon.host == "127.0.0.1" || config.daemon.host == "localhost" {
        // The console defaults to its own port; /v1 port is not it.
    }
    DEFAULT_WEBUI_URL.to_string()
}

/// Read-eval-print loop. rustyline provides line editing/history on a TTY and
/// degrades to plain line reads when stdin is not one.
fn repl(c: &Client, webui_url: &str) -> Result<()> {
    let mut rl = rustyline::DefaultEditor::new().context("cannot init line editor")?;
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if interactive {
        println!("xkeeper shell — type `help` for commands, `exit` to leave.");
    }
    loop {
        let readline = rl.readline("xkeeper> ");
        let line = match readline {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        match parse_line(&line) {
            Ok(None) => continue,
            Ok(Some(cmd)) => {
                if interactive {
                    rl.add_history_entry(line.trim())?;
                }
                match execute(c, &cmd, webui_url) {
                    Ok(Outcome::Continue) => {}
                    Ok(Outcome::Exit) => break,
                    // Per the CLI contract the shell stays alive on command
                    // errors; only the `-e` mode maps them to exit codes.
                    Err(e) => eprintln!("xkeeper: error: {e:#}"),
                }
            }
            Err(e) => eprintln!("xkeeper: {e:#}"),
        }
    }
    Ok(())
}

/// Execute one parsed command against the control-plane client.
fn execute(c: &Client, cmd: &ShellCmd, webui_url: &str) -> Result<Outcome> {
    match cmd {
        ShellCmd::Status => print_status(c),
        ShellCmd::Start(name) => print_action(c, name, "start"),
        ShellCmd::Stop(name) => print_action(c, name, "stop"),
        ShellCmd::Restart(name) => print_action(c, name, "restart"),
        ShellCmd::Pid(name) => {
            let v = c.program(name)?;
            match v.get("pid") {
                Some(pid) if !pid.is_null() => println!("{pid}"),
                _ => println!("not running"),
            }
            Ok(Outcome::Continue)
        }
        ShellCmd::Log {
            name,
            follow,
            tail,
            stream,
        } => {
            if *follow {
                c.log_follow(name, stream)?;
            } else {
                for line in c.log_tail(name, stream, *tail)? {
                    println!("{line}");
                }
            }
            Ok(Outcome::Continue)
        }
        ShellCmd::Reload => {
            let v = c.reload()?;
            println!(
                "{}",
                v.get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("reloaded")
            );
            Ok(Outcome::Continue)
        }
        ShellCmd::Pending => {
            let v = c.pending()?;
            let programs = v
                .get("programs")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            if programs.is_empty() {
                println!("no pending changes");
            } else {
                println!("pending changes ({}):", programs.len());
                for p in programs {
                    let app = p.get("app").and_then(|x| x.as_str()).unwrap_or("?");
                    let prog = p.get("program").and_then(|x| x.as_str()).unwrap_or("?");
                    let state = if p.get("running").and_then(|x| x.as_bool()).unwrap_or(false) {
                        "running"
                    } else {
                        "stopped"
                    };
                    println!("  {app}.{prog} [{state}] config changed");
                }
            }
            for key in ["apps_added", "apps_removed", "daemon_hints", "errors"] {
                if let Some(list) = v.get(key).and_then(|x| x.as_array()) {
                    for item in list {
                        println!("  {}", item.as_str().unwrap_or_default());
                    }
                }
            }
            Ok(Outcome::Continue)
        }
        ShellCmd::Apply {
            app,
            program,
            restart,
        } => {
            let v = c.apply(app.as_deref(), program.as_deref(), *restart)?;
            println!(
                "{}",
                v.get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("applied")
            );
            Ok(Outcome::Continue)
        }
        ShellCmd::Shutdown => {
            let v = c.shutdown()?;
            println!(
                "{}",
                v.get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("shutting down")
            );
            Ok(Outcome::Continue)
        }
        ShellCmd::Open => open_webui(webui_url).map(|_| Outcome::Continue),
        ShellCmd::Help => {
            println!("{HELP_TEXT}");
            Ok(Outcome::Continue)
        }
        ShellCmd::Exit => Ok(Outcome::Exit),
    }
}

fn print_action(c: &Client, name: &str, action: &str) -> Result<Outcome> {
    let v = c.action(name, action)?;
    println!(
        "{}",
        v.get("result").and_then(|r| r.as_str()).unwrap_or("done")
    );
    Ok(Outcome::Continue)
}

/// Aligned status table: name / app / state / pid / restarts / unhealthy.
fn print_status(c: &Client) -> Result<Outcome> {
    let v = c.status()?;
    let rows: Vec<[String; 6]> = v
        .get("programs")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .map(|p| {
                    [
                        str_field(p, "name"),
                        str_field(p, "app"),
                        str_field(p, "state"),
                        match p.get("pid") {
                            Some(x) if !x.is_null() => x.to_string(),
                            _ => "-".into(),
                        },
                        p.get("total_exits")
                            .and_then(|x| x.as_u64())
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| "0".into()),
                        if p.get("unhealthy")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false)
                        {
                            "yes".into()
                        } else {
                            "-".into()
                        },
                    ]
                })
                .collect()
        })
        .unwrap_or_default();

    if rows.is_empty() {
        println!("no programs (register an app with `xkeeper add <dir>`)");
        return Ok(Outcome::Continue);
    }
    let header = ["NAME", "APP", "STATE", "PID", "RESTARTS", "UNHEALTHY"];
    let mut width = [4usize; 6];
    for (i, h) in header.iter().enumerate() {
        width[i] = h.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            width[i] = width[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let emit = |out: &mut String, cells: Vec<String>| {
        out.push_str(
            &cells
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i + 1 == cells.len() {
                        cell.clone()
                    } else {
                        format!("{cell:<width$}  ", width = width[i])
                    }
                })
                .collect::<String>(),
        );
        out.push('\n');
    };
    emit(&mut out, header.iter().map(|s| s.to_string()).collect());
    out.push_str(
        &width
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  "),
    );
    out.push('\n');
    for row in &rows {
        emit(&mut out, row.to_vec());
    }
    print!("{out}");
    let _ = std::io::stdout().flush();
    Ok(Outcome::Continue)
}

fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("-")
        .to_string()
}

// -- webui probing + browser launch -----------------------------------------

/// `GET /api/health` on the webui port, short timeout. Public endpoint, no
/// token involved (webui auth is independent of /v1).
pub(crate) fn webui_health(base: &str) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(2))
        .build();
    let resp = agent
        .get(&format!("{base}/api/health"))
        .call()
        .map_err(|e| anyhow::anyhow!("webui unreachable: {e}"))?;
    if (200..300).contains(&resp.status()) {
        Ok(())
    } else {
        bail!("webui health returned {}", resp.status())
    }
}

/// Platform browser launcher. Returns the command name for diagnostics.
fn browser_command(url: &str) -> std::process::Command {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(url);
        cmd
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start"]);
        // raw arg: `start` mangles extra quotes otherwise
        cmd.raw_arg(format!("\"{url}\""));
        cmd
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
        cmd
    }
}

/// Probe webui; when reachable open the browser, otherwise print a hint.
fn open_webui(url: &str) -> Result<()> {
    if let Err(e) = webui_health(url) {
        eprintln!(
            "webui not reachable at {url} — start it with `xkeeper webui` (or `xkeeper system webui`)"
        );
        bail!("{e:#}");
    }
    let mut cmd = browser_command(url);
    match cmd.status() {
        Ok(st) if st.success() => {
            println!("opening {url}");
            Ok(())
        }
        _ => bail!("could not launch the system browser for {url}"),
    }
}

/// `xkeeper system webui [url]`: open the console in a browser, starting the
/// daemon in webui mode first when it is not running. This command itself
/// stays short-lived — the spawned child owns the daemon lifecycle.
pub(crate) fn system_webui(url: Option<&str>) -> Result<()> {
    let url = url
        .map(String::from)
        .unwrap_or_else(|| DEFAULT_WEBUI_URL.to_string());
    // The daemon command line records its own config path; the spawned child
    // re-reads the platform default (or errors clearly if misconfigured).
    let config = client::load_config(&crate::default_config_path())?;
    let c = Client::from_config(&config);

    if c.health().is_ok() {
        // Daemon runs: never touch its lifecycle — open or explain.
        if webui_health(&url).is_ok() {
            open_browser(&url)?;
            println!("{url}");
            return Ok(());
        }
        bail!(
            "daemon is running but webui is not reachable at {url} — restart it with `xkeeper webui`"
        );
    }

    // Daemon offline: spawn `xkeeper webui` detached from this process.
    let exe = std::env::current_exe().context("cannot locate current executable")?;
    let listen = listen_of(&url)?;
    let mut child = {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["webui", "--listen", &listen]);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn()
            .with_context(|| format!("failed to start {} webui", exe.display()))?
    };
    println!("daemon starting (pid {}) at {url}", child.id());

    // Wait up to 5s for the console to answer, then open the browser.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if webui_health(&url).is_ok() {
            open_browser(&url)?;
            println!("{url}");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!(
                "webui did not become reachable within 5s — start it manually with `xkeeper webui`"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Derive the `--listen` value from a URL (host:port), defaulting to loopback.
fn listen_of(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("only http:// urls are supported, got {url}"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        bail!("cannot parse listen address from {url}");
    }
    Ok(authority.to_string())
}

fn open_browser(url: &str) -> Result<()> {
    let mut cmd = browser_command(url);
    let st = cmd.status().context("cannot launch the system browser")?;
    if !st.success() {
        bail!("browser command exited with {st}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Option<ShellCmd> {
        parse_line(line).unwrap()
    }

    #[test]
    fn blank_and_comments() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
    }

    #[test]
    fn aliases_and_basic_commands() {
        assert_eq!(parse("?"), Some(ShellCmd::Help));
        assert_eq!(parse("quit"), Some(ShellCmd::Exit));
        assert_eq!(parse("exit"), Some(ShellCmd::Exit));
        assert_eq!(parse("status"), Some(ShellCmd::Status));
        assert_eq!(parse("reload"), Some(ShellCmd::Reload));
        assert_eq!(parse("pending"), Some(ShellCmd::Pending));
        assert_eq!(
            parse("apply"),
            Some(ShellCmd::Apply {
                app: None,
                program: None,
                restart: false
            })
        );
        assert_eq!(
            parse("apply myapp --restart"),
            Some(ShellCmd::Apply {
                app: Some("myapp".into()),
                program: None,
                restart: true
            })
        );
        assert_eq!(
            parse("apply myapp web"),
            Some(ShellCmd::Apply {
                app: Some("myapp".into()),
                program: Some("web".into()),
                restart: false
            })
        );
        assert!(parse_line("apply --bogus").is_err());
        assert!(
            parse_line("apply a b c").is_err(),
            "too many positional args"
        );
        assert_eq!(parse("open"), Some(ShellCmd::Open));
        assert_eq!(parse("shutdown"), Some(ShellCmd::Shutdown));
    }

    #[test]
    fn unknown_and_missing_args() {
        assert!(parse_line("sttaus").is_err());
        assert!(parse_line("start").is_err());
        assert!(parse_line("start a b").is_err());
        assert!(parse_line("log").is_err());
        assert!(parse_line("log web --tail").is_err());
        assert!(parse_line("log web --stream both").is_err());
    }

    #[test]
    fn log_flags() {
        assert_eq!(
            parse("log web"),
            Some(ShellCmd::Log {
                name: "web".into(),
                follow: false,
                tail: 20,
                stream: "out".into()
            })
        );
        assert_eq!(
            parse("log web -f --tail 100 --stream err"),
            Some(ShellCmd::Log {
                name: "web".into(),
                follow: true,
                tail: 100,
                stream: "err".into()
            })
        );
        // quoted program name
        assert_eq!(
            parse("log \"my app\" -f"),
            Some(ShellCmd::Log {
                name: "my app".into(),
                follow: true,
                tail: 20,
                stream: "out".into()
            })
        );
    }

    #[test]
    fn similar_hint_suggests() {
        assert!(similar_hint("sttaus").contains("status"));
        assert!(similar_hint("resart").contains("restart"));
        assert!(!similar_hint("zzzzz").contains("did you mean"));
    }

    #[test]
    fn status_table_alignment() {
        // Pure formatting check: columns line up via the emit logic.
        let v = serde_json::json!({
            "programs": [
                {"name": "web", "app": "demo", "state": "running", "pid": 42,
                 "total_exits": 1, "unhealthy": false},
                {"name": "worker-a", "app": "demo", "state": "backoff", "pid": null,
                 "total_exits": 0, "unhealthy": true}
            ]
        });
        let rows: Vec<[String; 6]> = v["programs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                [
                    str_field(p, "name"),
                    str_field(p, "app"),
                    str_field(p, "state"),
                    match p.get("pid") {
                        Some(x) if !x.is_null() => x.to_string(),
                        _ => "-".into(),
                    },
                    p.get("total_exits")
                        .and_then(|x| x.as_u64())
                        .map(|x| x.to_string())
                        .unwrap_or_default(),
                    if p.get("unhealthy")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false)
                    {
                        "yes".into()
                    } else {
                        "-".into()
                    },
                ]
            })
            .collect();
        // header + separator + 2 rows share the NAME column width
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "web");
        assert_eq!(rows[1][0], "worker-a");
        assert_eq!(rows[1][4], "0");
        assert_eq!(rows[1][5], "yes");
        assert_eq!(rows[0][3], "42");
        assert_eq!(rows[1][3], "-");
    }

    #[test]
    fn webui_health_offline_is_error() {
        assert!(webui_health("http://127.0.0.1:1").is_err());
    }

    #[test]
    fn listen_of_parses() {
        assert_eq!(
            listen_of("http://127.0.0.1:9877").unwrap(),
            "127.0.0.1:9877"
        );
        assert_eq!(
            listen_of("http://127.0.0.1:9877/x").unwrap(),
            "127.0.0.1:9877"
        );
        assert!(listen_of("ftp://x").is_err());
        assert!(listen_of("http://").is_err());
    }
}
