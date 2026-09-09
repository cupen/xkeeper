//! The local control plane: loopback HTTP/1.1 JSON API on std::TcpListener.
//!
//! Hand-rolled (instead of a framework) for one reason: the `log --follow`
//! endpoint must stream lines as they are produced and flush after every
//! write; generic HTTP stacks buffer responses and make that impossible
//! (see design D2). The request surface we need is deliberately tiny:
//! GET/POST, no request bodies, `Connection: close`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::supervisor::{Command, Supervisor};

const MAX_FOLLOWS: usize = 8;
const MAX_HEADER_BYTES: usize = 32 * 1024;

static FOLLOWS: AtomicUsize = AtomicUsize::new(0);

/// Shared status projection: served as JSON by the control plane and
/// re-used verbatim by the web console (`/api/*` + WS, MessagePack).
///
/// Metric fields (`cpu_percent`/`mem_bytes`/`log_rate`/`system`) are
/// incremental, backwards-compatible additions sourced from the sideband
/// sampler ([`crate::metrics`]): `None`/default means "no data", never 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProgramInfo {
    pub app: String,
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
    pub unhealthy: bool,
    pub uptime_secs: f64,
    pub total_exits: u64,
    pub restart_backoff: f64,
    pub last_exit: Option<String>,
    pub fatal_reason: Option<String>,
    pub wait_reason: Option<String>,
    /// CPU% normalized to the whole machine (0..=100).
    pub cpu_percent: Option<f64>,
    /// Resident set size in bytes.
    pub mem_bytes: Option<u64>,
    /// Per-stream log line rates (lines/sec at the display windows).
    pub log_rate: crate::metrics::LogRates,
    /// Declared command and args (console detail display).
    pub command: String,
    pub args: Vec<String>,
    /// Working directory (empty = inherited).
    pub work_dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StatusDoc {
    pub daemon: DaemonInfo,
    pub programs: Vec<ProgramInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DaemonInfo {
    pub version: String,
    pub port: u16,
    pub apps: usize,
    /// Host-level resource overview.
    pub system: crate::metrics::SystemMetrics,
    /// Configured monitor interval (console overview).
    pub monitor_interval: f64,
    /// Daemon wall-clock runtime in seconds.
    pub uptime_secs: f64,
    /// Where the daemon config was loaded from.
    pub config_source: String,
}

/// One status snapshot for any consumer (control plane + web console).
pub(crate) fn status_doc(sup: &Arc<Supervisor>) -> StatusDoc {
    let st = sup.state.lock().unwrap();
    let mut programs: Vec<ProgramInfo> = st
        .programs
        .values()
        .map(|p| program_info(p, Some(&sup.metrics)))
        .collect();
    programs.sort_by(|a, b| (&a.app, &a.name).cmp(&(&b.app, &b.name)));
    StatusDoc {
        daemon: DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            port: st.config.daemon.port,
            apps: st.apps.len(),
            system: sup.metrics.system(),
            monitor_interval: st.config.daemon.monitor_interval,
            uptime_secs: (sup.uptime_secs() * 100.0).round() / 100.0,
            config_source: sup.config_path().display().to_string(),
        },
        programs,
    }
}

/// Build the status projection of one program. `metrics` is the shared
/// sampler table; passing `None` leaves metric fields empty (used by tests).
pub(crate) fn program_info(
    p: &crate::program::ManagedProgram,
    metrics: Option<&crate::metrics::MetricsTable>,
) -> ProgramInfo {
    let m = metrics
        .and_then(|t| t.program(&p.def.name))
        .unwrap_or_default();
    ProgramInfo {
        app: p.def.app.clone(),
        name: p.def.name.clone(),
        state: p.state().to_string(),
        pid: p.pid(),
        unhealthy: p.unhealthy(),
        uptime_secs: (p.uptime_secs() * 100.0).round() / 100.0,
        total_exits: p.total_exits(),
        restart_backoff: p.def.restart_backoff,
        last_exit: p.last_exit().map(String::from),
        fatal_reason: p.fatal_reason().map(String::from),
        wait_reason: p.wait_reason(),
        cpu_percent: m.cpu_percent,
        mem_bytes: m.mem_bytes,
        log_rate: p.log_rates(),
        command: p.def.command.clone(),
        args: p.def.args.clone(),
        work_dir: p.def.work_dir.display().to_string(),
    }
}

fn json_bytes(status: u16, body: impl Serialize) -> Vec<u8> {
    let data = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match status {
        200..=299 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    )
    .into_bytes()
    .into_iter()
    .chain(data)
    .collect()
}

fn err_bytes(status: u16, msg: &str) -> Vec<u8> {
    json_bytes(status, serde_json::json!({ "error": msg }))
}

/// Bind the API port. Called on the startup path so a port clash fails fast.
pub fn bind(sup: &Arc<Supervisor>) -> Result<TcpListener> {
    let (host, port) = {
        let st = sup.state.lock().unwrap();
        (st.config.daemon.host.clone(), st.config.daemon.port)
    };
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).with_context(|| {
        format!("failed to bind control API on {addr} (is another xkeeper running?)")
    })?;
    info!("control API listening on http://{addr}");
    Ok(listener)
}

/// Accept loop; run on a background thread after `bind` succeeded.
pub fn serve(sup: Arc<Supervisor>, listener: TcpListener) {
    let token = {
        let st = sup.state.lock().unwrap();
        st.config.daemon.auth_token.clone()
    };
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let sup = sup.clone();
                let token = token.clone();
                std::thread::Builder::new()
                    .name("api-conn".into())
                    .spawn(move || handle_connection(&sup, &token, stream))
                    .ok();
            }
            Err(e) => {
                warn!("api accept error: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_connection(sup: &Arc<Supervisor>, token: &str, stream: TcpStream) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);

    // Request line.
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    if method.is_empty() || target.is_empty() {
        let _ = writer.write_all(&err_bytes(400, "bad request"));
        return;
    }

    // Headers (bounded). We never accept request bodies; a small body is
    // drained if a client sends one.
    let mut total = 0usize;
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() {
            return;
        }
        total += h.len();
        if total > MAX_HEADER_BYTES {
            let _ = writer.write_all(&err_bytes(431, "headers too large"));
            return;
        }
        let trimmed = h.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > 0 {
            let mut sink = vec![0u8; len.min(64 * 1024)];
            let _ = reader.read_exact(&mut sink);
        }
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    if !authorized(&headers, token, &path) {
        let _ = writer.write_all(&err_bytes(
            401,
            "unauthorized: missing or invalid bearer token",
        ));
        return;
    }

    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let resp: Vec<u8> = match (method.as_str(), segments.as_slice()) {
        ("GET", ["v1", "health"]) => json_bytes(200, serde_json::json!({"status": "ok"})),
        ("GET", ["v1", "status"]) => {
            let st = sup.state.lock().unwrap();
            let mut programs: Vec<ProgramInfo> = st
                .programs
                .values()
                .map(|p| program_info(p, Some(&sup.metrics)))
                .collect();
            programs.sort_by(|a, b| (&a.app, &a.name).cmp(&(&b.app, &b.name)));
            json_bytes(
                200,
                StatusDoc {
                    daemon: DaemonInfo {
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        port: st.config.daemon.port,
                        apps: st.apps.len(),
                        system: sup.metrics.system(),
                        monitor_interval: st.config.daemon.monitor_interval,
                        uptime_secs: (sup.uptime_secs() * 100.0).round() / 100.0,
                        config_source: sup.config_path().display().to_string(),
                    },
                    programs,
                },
            )
        }
        ("GET", ["v1", "programs"]) => {
            let st = sup.state.lock().unwrap();
            let mut programs: Vec<ProgramInfo> = st
                .programs
                .values()
                .map(|p| program_info(p, Some(&sup.metrics)))
                .collect();
            programs.sort_by(|a, b| (&a.app, &a.name).cmp(&(&b.app, &b.name)));
            json_bytes(200, serde_json::json!({ "programs": programs }))
        }
        ("GET", ["v1", "programs", name]) => {
            let st = sup.state.lock().unwrap();
            match st.programs.get(*name) {
                Some(p) => json_bytes(200, program_info(p, Some(&sup.metrics))),
                None => err_bytes(404, &format!("unknown program {name:?}")),
            }
        }
        (
            "POST",
            [
                "v1",
                "programs",
                name,
                action @ ("start" | "stop" | "restart"),
            ],
        ) => {
            let (tx, rx) = mpsc::channel();
            let cmd = match *action {
                "start" => Command::Start {
                    name: name.to_string(),
                    reply: Some(tx),
                },
                "stop" => Command::Stop {
                    name: name.to_string(),
                    reply: Some(tx),
                },
                _ => Command::Restart {
                    name: name.to_string(),
                    reply: Some(tx),
                },
            };
            sup.enqueue(cmd);
            match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(Ok(msg)) => json_bytes(200, serde_json::json!({ "result": msg })),
                Ok(Err(e)) => err_bytes(409, &format!("{e:#}")),
                Err(_) => err_bytes(500, "supervisor did not answer in time"),
            }
        }
        ("GET", ["v1", "programs", name, "logs"]) => {
            // Streaming response: owns the connection until the client leaves.
            respond_logs(sup, name, &query, writer);
            return;
        }
        ("POST", ["v1", "reload"]) => {
            let (tx, rx) = mpsc::channel();
            sup.enqueue(Command::Reload { reply: Some(tx) });
            match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(Ok(msg)) => json_bytes(200, serde_json::json!({ "result": msg })),
                Ok(Err(e)) => err_bytes(400, &format!("{e:#}")),
                Err(_) => err_bytes(500, "supervisor did not answer in time"),
            }
        }
        ("POST", ["v1", "shutdown"]) => {
            let (tx, rx) = mpsc::channel();
            sup.enqueue(Command::Shutdown { reply: Some(tx) });
            let _ = rx.recv_timeout(Duration::from_secs(5));
            json_bytes(200, serde_json::json!({ "result": "shutting down" }))
        }
        _ => err_bytes(404, &format!("no route {method} {path}")),
    };
    let _ = writer.write_all(&resp);
}

fn authorized(headers: &HashMap<String, String>, token: &str, path: &str) -> bool {
    if token.is_empty() || path == "/v1/health" {
        return true;
    }
    headers
        .get("authorization")
        .map(|v| v == &format!("Bearer {token}"))
        .unwrap_or(false)
}

fn respond_logs(sup: &Arc<Supervisor>, name: &str, query: &str, mut writer: TcpStream) {
    let mut stream = "out";
    let mut tail = 100usize;
    let mut follow = false;
    for kv in query.split('&') {
        let mut it = kv.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        match k {
            "stream" if v == "err" => stream = "err",
            "stream" if v == "out" => stream = "out",
            "tail" => tail = v.parse().unwrap_or(100),
            "follow" => follow = v == "1" || v == "true",
            _ => {}
        }
    }
    let ring = {
        let st = sup.state.lock().unwrap();
        match st.programs.get(name) {
            Some(p) => Some(p.ring(if stream == "err" {
                crate::pump::Stream::Err
            } else {
                crate::pump::Stream::Out
            })),
            None => None,
        }
    };
    let Some(ring) = ring else {
        let _ = writer.write_all(&err_bytes(404, &format!("unknown program {name:?}")));
        return;
    };

    if !follow {
        let lines = ring.tail(tail);
        let _ = writer.write_all(&json_bytes(
            200,
            serde_json::json!({ "program": name, "stream": stream, "lines": lines }),
        ));
        return;
    }

    if FOLLOWS.load(Ordering::SeqCst) >= MAX_FOLLOWS {
        let _ = writer.write_all(&err_bytes(429, "too many concurrent log followers"));
        return;
    }
    FOLLOWS.fetch_add(1, Ordering::SeqCst);

    // Subscribe first, then snapshot: worst case a line appears twice, never
    // goes missing.
    let rx = ring.subscribe();
    let snapshot = ring.tail(tail);

    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if writer.write_all(head.as_bytes()).is_err() || writer.flush().is_err() {
        FOLLOWS.fetch_sub(1, Ordering::SeqCst);
        return;
    }

    let write_chunk = |writer: &mut TcpStream, line: &str| -> bool {
        let body = line.as_bytes();
        // Chunk data is the line plus one trailing '\n'; then the CRLF terminator.
        let head = format!("{:x}\r\n", body.len() + 1);
        writer.write_all(head.as_bytes()).is_ok()
            && writer.write_all(body).is_ok()
            && writer.write_all(b"\n").is_ok()
            && writer.write_all(b"\r\n").is_ok()
            && writer.flush().is_ok()
    };

    let mut client_gone = false;
    for l in snapshot {
        if !write_chunk(&mut writer, &l) {
            client_gone = true;
            break;
        }
    }
    if !client_gone {
        // Bounded subscription: Lines batches, Gap = this follower fell
        // behind and missed N lines (ring/file still complete — show an
        // explicit marker instead of silently lying).
        for item in rx {
            let ok = match item {
                crate::pump::SubItem::Lines(lines) => {
                    lines.iter().all(|l| write_chunk(&mut writer, l))
                }
                crate::pump::SubItem::Gap(n) => {
                    write_chunk(&mut writer, &format!("[[xkeeper: skipped {n} lines]]"))
                }
            };
            if !ok {
                client_gone = true;
                break; // client disconnected
            }
        }
    }
    let _ = writer.write_all(b"0\r\n\r\n");
    let _ = writer.flush();
    FOLLOWS.fetch_sub(1, Ordering::SeqCst);
    if client_gone {
        info!("log follow for program[{name}] ended (client disconnected)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppRaw, resolve_app};
    use crate::metrics::{ProgramMetrics, StreamRate, SystemMetrics};
    use crate::program::ManagedProgram;
    use std::path::Path;

    fn sample_doc() -> StatusDoc {
        let info = ProgramInfo {
            app: "app".into(),
            name: "a".into(),
            state: "running".into(),
            pid: Some(1),
            unhealthy: false,
            uptime_secs: 1.0,
            total_exits: 0,
            restart_backoff: 1.0,
            last_exit: None,
            fatal_reason: None,
            wait_reason: None,
            cpu_percent: Some(42.1),
            mem_bytes: Some(8192),
            log_rate: crate::metrics::LogRates {
                out: StreamRate {
                    w1: 5.0,
                    w10: 5.0,
                    w60: 4.9,
                    w300: 5.1,
                },
                err: StreamRate::default(),
            },
            command: "true".into(),
            args: vec![],
            work_dir: String::new(),
        };
        StatusDoc {
            daemon: DaemonInfo {
                version: "t".into(),
                port: 1,
                apps: 1,
                system: SystemMetrics {
                    cpu_percent: Some(7.3),
                    mem_used_bytes: 10,
                    mem_total_bytes: 100,
                },
                monitor_interval: 1.0,
                uptime_secs: 5.0,
                config_source: "/tmp/xkeeper.toml".into(),
            },
            programs: vec![info],
        }
    }

    /// webui-api: WS structured payloads and REST JSON must come from the
    /// same serde structs — decoding either encoding yields equal values.
    #[test]
    fn projection_json_msgpack_isomorphic() {
        let doc = sample_doc();
        let j = serde_json::to_vec(&doc).unwrap();
        let m = rmp_serde::to_vec(&doc).unwrap();
        let dj: StatusDoc = serde_json::from_slice(&j).unwrap();
        let dm: StatusDoc = rmp_serde::from_slice(&m).unwrap();
        assert_eq!(dj, dm);
        assert_eq!(dj, doc);
        assert!(
            m.len() < j.len(),
            "msgpack should not be larger than json here"
        );
    }

    /// metrics/control-plane: a supervised program's projection carries the
    /// metric fields from the shared sampler table; not-running → null.
    #[test]
    fn program_info_merges_metrics_from_table() {
        let dir = std::env::temp_dir().join(format!("xk-proj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = crate::config::DaemonConfig::default();
        let sup = Supervisor::new(config, &dir).unwrap();
        let raw: AppRaw = toml::from_str("[program.p]\ncommand = 'true'\n").unwrap();
        let app = resolve_app("demo", Path::new("x.toml"), &raw, None).unwrap();
        let def = app.programs.into_iter().next().unwrap();
        {
            let mut st = sup.state.lock().unwrap();
            st.programs.insert(
                def.name.clone(),
                ManagedProgram::new(def.clone(), dir.clone(), 10),
            );
        }
        // No metrics yet → fields empty (not 0).
        let doc = status_doc(&sup);
        let info = doc.programs.iter().find(|p| p.name == def.name).unwrap();
        assert_eq!(info.cpu_percent, None);
        assert_eq!(info.mem_bytes, None);

        sup.metrics.set_program(
            &def.name,
            ProgramMetrics {
                cpu_percent: Some(12.5),
                mem_bytes: Some(2048),
            },
        );
        let doc = status_doc(&sup);
        let info = doc.programs.iter().find(|p| p.name == def.name).unwrap();
        assert_eq!(info.cpu_percent, Some(12.5));
        assert_eq!(info.mem_bytes, Some(2048));
        assert_eq!(
            doc.daemon.system.cpu_percent, None,
            "system untouched by program entry"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
