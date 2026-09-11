//! The `xkeeper webui` HTTP server: embedded console SPA + WebSocket push.
//!
//! Design consistency with the control plane (`webui-api` capability):
//! this server re-uses the *same* status projection as `crate::server`
//! (`/v1` JSON), routes control actions through the supervisor command
//! queue (the loop is the only writer), and serves log data from the
//! in-memory ring buffers (`crate::pump`) — never from log files. What it
//! adds for the browser console: the embedded SPA and a `/ws` push channel
//! (snapshot → status deltas → log chunks → heartbeat, MessagePack binary
//! frames, see [`crate::api`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tower_http::compression::CompressionLayer;

use crate::api::{self, msg_type};
use crate::assets;
use crate::pump;
use crate::pump::Stream;
use crate::server::{ProgramInfo, StatusDoc, program_info, status_doc};
use crate::supervisor::{ApplyScope, Command, Supervisor};

/// Replay context when a client subscribes to a log stream.
const LOG_TAIL_DEFAULT: usize = 200;
/// Status-diff cadence for WS sessions (daemon tick granularity is
/// `monitor_interval`, so this is at least twice as fine).
const WS_POLL: Duration = Duration::from_millis(500);
const WS_HEARTBEAT: Duration = Duration::from_secs(30);
/// How long an HTTP control request waits for the supervisor to execute it
/// (a stop may block up to the program's stop_timeout).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// One log frame carries multiple lines: flush at this size…
const FRAME_BYTES: usize = 64 * 1024;
/// …or this long after the first pending byte (latency bound).
const FRAME_INTERVAL: Duration = Duration::from_millis(100);
/// Log frames wait up to this long for a session-writer slot. Blocking (not
/// closing) is what routes backpressure into the bounded rx queue, where
/// loss is counted and surfaced as Gap markers; a truly dead client still
/// frees the session when the budget lapses (and on sink errors).
const LOG_SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct LogQuery {
    stream: Option<String>,
    tail: Option<usize>,
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

async fn overview(State(sup): State<Arc<Supervisor>>) -> Json<StatusDoc> {
    Json(status_doc(&sup))
}

async fn programs(State(sup): State<Arc<Supervisor>>) -> Json<serde_json::Value> {
    Json(json!({ "programs": status_doc(&sup).programs }))
}

async fn program_detail(State(sup): State<Arc<Supervisor>>, Path(name): Path<String>) -> Response {
    let st = sup.state.lock().unwrap();
    match st.programs.get(&name) {
        Some(p) => Json(program_info(p, Some(&sup.metrics))).into_response(),
        None => api_error(StatusCode::NOT_FOUND, &format!("unknown program {name:?}")),
    }
}

async fn program_logs(
    State(sup): State<Arc<Supervisor>>,
    Path(name): Path<String>,
    Query(q): Query<LogQuery>,
) -> Response {
    let (ring, tail) = {
        let st = sup.state.lock().unwrap();
        let Some(p) = st.programs.get(&name) else {
            return api_error(StatusCode::NOT_FOUND, &format!("unknown program {name:?}"));
        };
        let stream = match q.stream.as_deref().unwrap_or("out") {
            "out" => Stream::Out,
            "err" => Stream::Err,
            other => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    &format!("stream must be \"out\" or \"err\", got {other:?}"),
                );
            }
        };
        (p.ring(stream), q.tail.unwrap_or(LOG_TAIL_DEFAULT))
    };
    // Capped by the ring buffer itself (log-management spec: tail comes
    // from the in-memory buffer, unaffected by rotation).
    Json(json!({
        "program": name,
        "lines": ring.tail(tail),
    }))
    .into_response()
}

async fn pending(State(sup): State<Arc<Supervisor>>) -> Json<crate::supervisor::PendingDoc> {
    let doc = sup.state.lock().unwrap().pending.clone();
    Json(doc)
}

#[derive(Deserialize, Default)]
struct ApplyQuery {
    app: Option<String>,
    program: Option<String>,
    restart: Option<bool>,
}

async fn apply(State(sup): State<Arc<Supervisor>>, body: Option<Json<ApplyQuery>>) -> Response {
    use std::sync::mpsc;
    let q = body.map(|Json(q)| q).unwrap_or_default();
    // Scope validation mirrors /v1/apply (unknown app/program → 404).
    let (known_app, known_program) = {
        let st = sup.state.lock().unwrap();
        let app_ok = match &q.app {
            Some(a) => st.apps.iter().any(|r| &r.name == a),
            None => true,
        };
        let prog_ok = match (&q.app, &q.program) {
            (Some(a), Some(p)) => st
                .programs
                .get(p)
                .map(|x| &x.def.app == a)
                .unwrap_or(false),
            _ => true,
        };
        (app_ok, prog_ok)
    };
    if let Some(a) = &q.app {
        if !known_app {
            return api_error(StatusCode::NOT_FOUND, &format!("unknown app {a:?}"));
        }
    }
    if let Some(p) = &q.program {
        if !known_program {
            return api_error(StatusCode::NOT_FOUND, &format!("unknown program {p:?}"));
        }
    }
    let scope = match (q.app, q.program) {
        (Some(a), Some(p)) => ApplyScope::Program(a, p),
        (Some(a), None) => ApplyScope::App(a),
        _ => ApplyScope::All,
    };
    let (tx, rx) = mpsc::channel();
    sup.enqueue(Command::Apply {
        scope,
        restart: q.restart.unwrap_or(false),
        reply: Some(tx),
    });
    match rx.recv_timeout(COMMAND_TIMEOUT) {
        Ok(Ok(msg)) => Json(json!({ "result": msg })).into_response(),
        Ok(Err(e)) => api_error(StatusCode::CONFLICT, &format!("{e:#}")),
        Err(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "supervisor did not answer in time",
        ),
    }
}

async fn program_action(
    State(sup): State<Arc<Supervisor>>,
    Path((name, action)): Path<(String, String)>,
) -> Response {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let cmd = match action.as_str() {
        "start" => Command::Start {
            name,
            reply: Some(tx),
        },
        "stop" => Command::Stop {
            name,
            reply: Some(tx),
        },
        "restart" => Command::Restart {
            name,
            reply: Some(tx),
        },
        other => {
            return api_error(
                StatusCode::NOT_FOUND,
                &format!("unknown action {other:?} (start | stop | restart)"),
            );
        }
    };
    sup.enqueue(cmd);
    match rx.recv_timeout(COMMAND_TIMEOUT) {
        Ok(Ok(msg)) => Json(json!({ "result": msg })).into_response(),
        Ok(Err(e)) => api_error(StatusCode::CONFLICT, &format!("{e:#}")),
        Err(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "supervisor did not answer in time",
        ),
    }
}

/// Build the console router (exposed for integration tests).
pub fn build_router(sup: Arc<Supervisor>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/overview", get(overview))
        .route("/api/programs", get(programs))
        .route("/api/programs/{name}", get(program_detail))
        .route("/api/programs/{name}/logs", get(program_logs))
        .route("/api/programs/{name}/{action}", post(program_action))
        .route("/api/pending", get(pending))
        .route("/api/apply", post(apply))
        .route("/assets/{*path}", get(assets::asset))
        .route("/ws", get(ws_upgrade))
        .layer(CompressionLayer::new())
        .with_state(sup)
        .fallback(assets::spa_fallback)
}

/// Serve the console on `listen`. Blocks the calling thread; run it on a
/// dedicated worker thread. Exits when the daemon shuts down.
pub fn serve(sup: Arc<Supervisor>, listen: &str) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let app = build_router(sup.clone());
        let listener = tokio::net::TcpListener::bind(listen).await?;
        log::info!("webui listening on http://{listen}");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                // Exit when the daemon shuts down (flag is set by Ctrl+C /
                // POST /v1/shutdown), so the process can terminate.
                loop {
                    if sup.state.lock().unwrap().shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
            .await
            .map_err(|e| anyhow::anyhow!("webui server error: {e}"))
    })
}

// ---------------------------------------------------------------- WebSocket

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum ClientAction {
    Subscribe { program: String, stream: String },
    Unsubscribe { program: String, stream: String },
}

fn stream_of(stream: &str) -> Option<Stream> {
    match stream {
        "out" => Some(Stream::Out),
        "err" => Some(Stream::Err),
        _ => None,
    }
}

fn stream_byte(stream: Stream) -> u8 {
    match stream {
        Stream::Out => 0,
        Stream::Err => 1,
    }
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(sup): State<Arc<Supervisor>>) -> Response {
    ws.on_upgrade(move |socket| ws_session(socket, sup))
}

/// Compute which programs changed between two snapshots (spec: incremental
/// events carry the program name plus its new status fields).
fn status_delta(prev: &[ProgramInfo], cur: &[ProgramInfo]) -> Vec<ProgramInfo> {
    if prev.len() != cur.len() {
        return cur.to_vec();
    }
    cur.iter()
        .zip(prev.iter())
        .filter(|(c, p)| c != p)
        .map(|(c, _)| c.clone())
        .collect()
}

async fn ws_session(socket: WebSocket, sup: Arc<Supervisor>) {
    let (mut sink, mut source) = socket.split();
    // All outgoing frames funnel through this channel; one writer task.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(128);
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
        }
    });

    // webui-api spec: the first message is always a full snapshot.
    let snap = status_doc(&sup);
    let mut last: Vec<ProgramInfo> = snap.programs.clone();
    let mut last_pending = snap.pending.clone();
    // Named (map) encoding: the browser's msgpack decoder produces JS
    // objects; compact array encoding yields a tuple, which the client's
    // StatusDoc handling cannot consume (the WS channel would silently
    // stall on snapshot decode). Field names keep JSON/MessagePack
    // isomorphic per the webui-api data-format requirement.
    if let Ok(payload) = rmp_serde::to_vec_named(&snap) {
        let _ = out_tx
            .send(api::encode_ws_message(msg_type::SNAPSHOT, &payload))
            .await;
    }

    let mut heartbeat = tokio::time::interval(WS_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut subs: HashMap<(String, Stream), tokio::task::JoinHandle<()>> = HashMap::new();
    // A saturated session closes wholesale; subscribe tasks signal here.
    let (close_tx, mut close_rx) = mpsc::channel::<()>(1);

    loop {
        tokio::select! {
            msg = source.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        match serde_json::from_slice::<ClientAction>(&data)
                            .or_else(|_| rmp_serde::from_slice::<ClientAction>(&data))
                        {
                            Ok(ClientAction::Subscribe { program, stream }) => {
                                let stream = match stream_of(&stream) {
                                    Some(s) => s,
                                    None => {
                                        let _ = out_tx.send(api::encode_ws_message(
                                            msg_type::ERROR,
                                            b"{\"error\":\"stream must be out or err\"}",
                                        )).await;
                                        continue;
                                    }
                                };
                                let known = sup.state.lock().unwrap()
                                    .programs.contains_key(&program);
                                if !known {
                                    let _ = out_tx.send(api::encode_ws_message(
                                        msg_type::ERROR,
                                        json!({ "error": format!("unknown program {program:?}") })
                                            .to_string().as_bytes(),
                                    )).await;
                                    continue;
                                }
                                let key = (program.clone(), stream);
                                if let std::collections::hash_map::Entry::Vacant(e) = subs.entry(key.clone()) {
                                    e.insert(tokio::spawn(subscribe_logs(
                                        sup.clone(), out_tx.clone(), close_tx.clone(),
                                        program, stream,
                                    )));
                                }
                            }
                            Ok(ClientAction::Unsubscribe { program, stream }) => {
                                if let Some(s) = stream_of(&stream) {
                                    if let Some(handle) = subs.remove(&(program, s)) {
                                        handle.abort();
                                    }
                                }
                            }
                            Err(_) => {
                                let _ = out_tx.send(api::encode_ws_message(
                                    msg_type::ERROR,
                                    b"{\"error\":\"bad message\"}",
                                )).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {} // ping/pong/text frames are ignored
                }
            }
            _ = tokio::time::sleep(WS_POLL) => {
                // Diff the shared state against what this session last sent;
                // the control-plane loop is the single writer. Pending changes
                // ride the STATUS frame as a `pending` field when they differ
                // (apply-workflow: pending appears in snapshot + deltas).
                let doc = status_doc(&sup);
                let cur = doc.programs.clone();
                let pending_changed = doc.pending != last_pending;
                let delta = status_delta(&last, &cur);
                last = cur;
                if !delta.is_empty() || pending_changed {
                    last_pending = doc.pending.clone();
                    let mut payload = serde_json::json!({ "programs": delta });
                    if pending_changed {
                        payload["pending"] =
                            serde_json::to_value(&doc.pending).unwrap_or_default();
                    }
                    if let Ok(bytes) = serde_json::to_vec(&payload) {
                        let _ = out_tx
                            .send(api::encode_ws_message(msg_type::STATUS, &bytes))
                            .await;
                    }
                }
                // Daemon shutting down: end the session so the console
                // server's graceful drain (webui-api: 随守护进程退出) can
                // complete instead of waiting on this connection forever.
                if sup.state.lock().unwrap().shutdown.load(Ordering::SeqCst) {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                let _ = out_tx.send(api::encode_ws_message(msg_type::HEARTBEAT, b"null")).await;
            }
            _ = close_rx.recv() => {
                // A log subscriber hit sustained backpressure: drop the whole
                // session; the client reconnects with backoff and resyncs.
                break;
            }
        }
    }

    for (_, handle) in subs.drain() {
        handle.abort();
    }
    writer.abort();
}

/// Replay the current tail once, then forward ring-buffer batches as
/// aggregated frames (one frame = many lines, bounded by [`FRAME_BYTES`] /
/// [`FRAME_INTERVAL`]). Gap markers from the bounded subscription are sent
/// as their own LOG_GAP frames, always ordered before subsequent content.
/// If the session writer stays saturated past [`SEND_TIMEOUT`], the close
/// channel is signalled so the whole session drops (webui-api: WS 日志批量
/// 推送与背压; slow viewers never back up the pump).
async fn subscribe_logs(
    sup: Arc<Supervisor>,
    out_tx: mpsc::Sender<Vec<u8>>,
    close_tx: mpsc::Sender<()>,
    program: String,
    stream: Stream,
) {
    let ring = {
        let st = sup.state.lock().unwrap();
        match st.programs.get(&program) {
            Some(p) => p.ring(stream),
            None => return,
        }
    };
    let sb = stream_byte(stream);

    // Replay context first (spec: subscribing yields the tail, then follows).
    let tail = ring.tail(LOG_TAIL_DEFAULT);
    if !tail.is_empty() {
        let mut data = tail.join("\n");
        data.push('\n');
        if out_tx
            .send(api::encode_log_frame(&program, sb, data.as_bytes()))
            .await
            .is_err()
        {
            return; // client went away
        }
    }

    let rx = ring.subscribe();
    let mut frame: Vec<u8> = Vec::with_capacity(FRAME_BYTES * 2);
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        // Drain what is queued. Frames flush mid-batch — never splitting a
        // line, never dropping the remainder of a batch — so a frame is
        // bounded at FRAME_BYTES + one line.
        let mut drained_any = false;
        while let Ok(item) = rx.try_recv() {
            drained_any = true;
            match item {
                pump::SubItem::Lines(lines) => {
                    for l in lines {
                        frame.extend_from_slice(l.as_bytes());
                        frame.push(b'\n');
                        if frame.len() >= FRAME_BYTES {
                            let payload = std::mem::take(&mut frame);
                            deadline = None;
                            if !send_frame(&out_tx, api::encode_log_frame(&program, sb, &payload))
                                .await
                            {
                                saturate_close(&close_tx).await;
                                return;
                            }
                        }
                    }
                }
                pump::SubItem::Gap(n) => {
                    // The marker must precede content that arrived after the
                    // loss, so flush pending content first.
                    if !frame.is_empty() {
                        let payload = std::mem::take(&mut frame);
                        deadline = None;
                        if !send_frame(&out_tx, api::encode_log_frame(&program, sb, &payload)).await
                        {
                            saturate_close(&close_tx).await;
                            return;
                        }
                    }
                    // A gap marker is accounting the client is owed: wait for
                    // a writer slot (past the ordinary saturation bound) so a
                    // merely-slow client still learns what it missed.
                    if !send_frame(&out_tx, api::encode_gap_frame(&program, sb, n)).await {
                        saturate_close(&close_tx).await;
                        return;
                    }
                }
            }
        }
        if !frame.is_empty() && deadline.is_none() {
            deadline = Some(tokio::time::Instant::now() + FRAME_INTERVAL);
        }
        let due = deadline
            .map(|d| tokio::time::Instant::now() >= d)
            .unwrap_or(false);
        if due && !frame.is_empty() {
            let payload = std::mem::take(&mut frame);
            deadline = None;
            if !send_frame(&out_tx, api::encode_log_frame(&program, sb, &payload)).await {
                saturate_close(&close_tx).await;
                return;
            }
        }
        // Idle (nothing drained, nothing pending): poll in slices. Anything
        // in flight: short slices so throughput stays production-bound.
        tokio::time::sleep(if !drained_any && frame.is_empty() {
            Duration::from_millis(20)
        } else {
            Duration::from_millis(2)
        })
        .await;
    }
}

/// One frame to the session writer. Ordinary frames carry the saturation
/// bound (a frozen network must free the session); gap frames wait far
/// longer. False = give up and close this session.
async fn send_frame(out_tx: &mpsc::Sender<Vec<u8>>, frame: Vec<u8>) -> bool {
    match out_tx.send_timeout(frame, LOG_SEND_TIMEOUT).await {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// Signal the session loop to close after sustained writer saturation.
async fn saturate_close(close_tx: &mpsc::Sender<()>) {
    let _ = close_tx.send(()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DaemonConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_sup() -> Arc<Supervisor> {
        let config = DaemonConfig::default();
        let dir = std::env::temp_dir().join(format!("xkeeper-web-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        Supervisor::new(config, &dir).unwrap()
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, axum::body::Bytes) {
        let resp = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn rest_endpoints_serve_projection_and_404_objects() {
        let app = build_router(test_sup());

        let (status, body) = get(app.clone(), "/api/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"ok": true}).to_string());

        let (status, body) = get(app.clone(), "/api/overview").await;
        assert_eq!(status, StatusCode::OK);
        let doc: StatusDoc = serde_json::from_slice(&body).unwrap();
        assert_eq!(doc.programs.len(), 0);

        let (status, body) = get(app.clone(), "/api/programs/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("unknown program"));

        let (status, _) = get(app, "/api/programs/nope/logs").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn status_delta_reports_only_changes() {
        let a = ProgramInfo {
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
            cpu_percent: None,
            mem_bytes: None,
            log_rate: crate::metrics::LogRates::default(),
            command: String::new(),
            args: Vec::new(),
            work_dir: String::new(),
        };
        let b_changed = {
            let mut x = a.clone();
            x.state = "backoff".into();
            x
        };
        let delta = status_delta(&[a.clone()], &[b_changed.clone()]);
        assert_eq!(delta, vec![b_changed]);
        assert!(status_delta(&[a.clone()], &[a]).is_empty());
    }

    #[test]
    fn status_delta_reports_metric_changes() {
        // webui-api: WS 增量携带指标变化 — a metric-only difference must
        // produce a delta entry (the STATUS event then carries the new value).
        let a = ProgramInfo {
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
            cpu_percent: None,
            mem_bytes: None,
            log_rate: crate::metrics::LogRates::default(),
            command: String::new(),
            args: Vec::new(),
            work_dir: String::new(),
        };
        let mut b = a.clone();
        b.cpu_percent = Some(37.5);
        let delta = status_delta(&[a], &[b.clone()]);
        assert_eq!(delta, vec![b]);
    }

    // -- metrics contract tests (webui-api: 投影携带指标) --------------------

    use crate::config::{AppRaw, resolve_app};
    use crate::metrics::{ProgramMetrics, SystemMetrics};
    use crate::program::ManagedProgram;
    use std::path::Path;

    /// A supervisor with one declared program "m" and metric values injected
    /// into the shared sampler table.
    fn sup_with_metrics() -> (Arc<Supervisor>, String) {
        let config = DaemonConfig::default();
        let dir = std::env::temp_dir().join(format!("xk-web-metrics-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sup = Supervisor::new(config, &dir).unwrap();
        let raw: AppRaw = toml::from_str("[program.m]\ncommand = 'true'\n").unwrap();
        let app = resolve_app("demo", Path::new("x.toml"), &raw, None).unwrap();
        let def = app.programs.into_iter().next().unwrap();
        let name = def.name.clone();
        {
            let mut st = sup.state.lock().unwrap();
            st.programs
                .insert(name.clone(), ManagedProgram::new(def, dir, 10));
        }
        sup.metrics.set_program(
            &name,
            ProgramMetrics {
                cpu_percent: Some(12.5),
                mem_bytes: Some(4096),
            },
        );
        sup.metrics.set_system(SystemMetrics {
            cpu_percent: Some(3.4),
            mem_used_bytes: 1024,
            mem_total_bytes: 8192,
        });
        (sup, name)
    }

    #[tokio::test]
    async fn overview_json_carries_metric_fields() {
        let (sup, name) = sup_with_metrics();
        let app = build_router(sup);
        let (status, body) = get(app, "/api/overview").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let p = &v["programs"][0];
        assert_eq!(p["name"], name);
        assert_eq!(p["cpu_percent"], 12.5);
        assert_eq!(p["mem_bytes"], 4096);
        assert!(
            p["log_rate"]["out"]["w10"].is_number(),
            "log_rate fields present"
        );
        assert_eq!(v["daemon"]["system"]["cpu_percent"], 3.4);
        assert_eq!(v["daemon"]["system"]["mem_total_bytes"], 8192);
    }

    #[tokio::test]
    async fn ws_snapshot_carries_metric_fields() {
        let (sup, name) = sup_with_metrics();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sup2 = sup.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, build_router(sup2)).await;
        });
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("ws connect");
        let msg = ws.next().await.unwrap().unwrap();
        let (t, payload) = api::decode_ws_message(&msg.into_data()).unwrap();
        assert_eq!(t, msg_type::SNAPSHOT);
        let doc: StatusDoc = rmp_serde::from_slice(&payload).unwrap();
        let p = doc.programs.iter().find(|p| p.name == name).unwrap();
        assert_eq!(p.cpu_percent, Some(12.5));
        assert_eq!(p.mem_bytes, Some(4096));
        assert_eq!(doc.daemon.system.cpu_percent, Some(3.4));
    }

    #[tokio::test]
    async fn control_plane_status_carries_metrics() {
        let (sup, name) = sup_with_metrics();
        // Port 0 → OS-assigned ephemeral port; no clashes with other tests.
        {
            let mut st = sup.state.lock().unwrap();
            st.config.daemon.port = 0;
        }
        let listener = crate::server::bind(&sup).expect("ephemeral bind");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || crate::server::serve(sup, listener));

        let resp = ureq::get(&format!("http://{addr}/v1/status"))
            .call()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        let p = v["programs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .expect("program in /v1/status");
        assert_eq!(p["cpu_percent"], 12.5);
        assert_eq!(p["mem_bytes"], 4096);
        assert_eq!(v["daemon"]["system"]["cpu_percent"], 3.4);
    }
}

#[cfg(test)]
mod high_speed_tests {
    use super::*;
    use crate::config::{AppRaw, DaemonConfig, resolve_app};
    use crate::program::ManagedProgram;
    use crate::pump::SUB_CHANNEL_BATCHES;
    use futures_util::{Sink, StreamExt};
    use std::path::Path;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    type ClientWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    /// Supervisor + router on an ephemeral port + one declared program "m"
    /// whose out-ring is directly pushable (stand-in for a firehose child).
    async fn firehose_setup() -> (Arc<Supervisor>, String, ClientWs, Arc<pump::Ring>) {
        let config = DaemonConfig::default();
        let dir = std::env::temp_dir().join(format!("xk-hs-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sup = Supervisor::new(config, &dir).unwrap();
        let raw: AppRaw = toml::from_str("[program.m]\ncommand = 'true'\n").unwrap();
        let app = resolve_app("demo", Path::new("x.toml"), &raw, None).unwrap();
        let def = app.programs.into_iter().next().unwrap();
        let name = def.name.clone();
        let ring = {
            let mut st = sup.state.lock().unwrap();
            let p = ManagedProgram::new(def, dir, 100000);
            let ring = p.ring(Stream::Out);
            st.programs.insert(name.clone(), p);
            ring
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sup2 = sup.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, build_router(sup2)).await;
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("ws connect");
        (sup, name, ws, ring)
    }

    async fn subscribe<S>(ws: &mut S, program: &str, stream: &str)
    where
        S: Sink<ClientMessage> + Unpin,
        S::Error: std::fmt::Debug,
    {
        let action = serde_json::json!({
            "action": "subscribe", "program": program, "stream": stream
        });
        ws.send(ClientMessage::Binary(
            serde_json::to_vec(&action).unwrap().into(),
        ))
        .await
        .expect("subscribe send failed");
        // Let the session spawn the subscriber (tail replay of an empty ring).
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// webui-api 多行单帧 + 内容完整: a fast producer whose client keeps up
    /// must see every line, aggregated into far fewer frames than lines.
    #[tokio::test]
    async fn high_speed_output_delivers_every_line_in_multi_line_frames() {
        let (_sup, name, mut ws, ring) = firehose_setup().await;
        subscribe(&mut ws, &name, "out").await;

        const WAVES: usize = 40;
        const BATCHES_PER_WAVE: usize = 30;
        const LINES_PER_BATCH: usize = 10;
        let total = WAVES * BATCHES_PER_WAVE * LINES_PER_BATCH;

        // Concurrent reader: a keeping-up client (reading while we produce)
        // must see every line. Waves drain fully between sends: 30 batches
        // per wave (< queue depth 64) with 50 ms gaps vs the subscriber's
        // 20 ms poll, so at most one wave is ever queued.
        let reader = tokio::spawn(async move {
            let mut lines_seen = 0usize;
            let mut log_frames = 0usize;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while lines_seen < total {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timeout: {lines_seen}/{total} lines in {log_frames} frames"
                );
                let msg = ws.next().await.expect("ws closed early").unwrap();
                let (t, payload) = api::decode_ws_message(&msg.into_data()).unwrap();
                if t != msg_type::LOG {
                    continue; // snapshot / status / heartbeat frames may interleave
                }
                let (_, _, data) = api::decode_log_frame(&rebuild(t, &payload)).unwrap();
                lines_seen += data.lines().count();
                log_frames += 1;
            }
            (lines_seen, log_frames)
        });

        for w in 0..WAVES {
            for b in 0..BATCHES_PER_WAVE {
                let lines: Vec<String> = (0..LINES_PER_BATCH)
                    .map(|i| format!("w{w}b{b}l{i}-0123456789"))
                    .collect();
                ring.push_batch(&lines);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let (lines_seen, log_frames) = reader.await.unwrap();
        assert_eq!(lines_seen, total, "a keeping-up client loses nothing");
        assert!(
            log_frames < total / 100,
            "frames must aggregate many lines (got {log_frames} frames for {total} lines)"
        );
    }

    /// decode_ws_message strips (and decompresses) the frame header; the
    /// decode_*_frame helpers expect the full frame back.
    fn rebuild(t: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(payload.len() + 1);
        f.push(t);
        f.extend_from_slice(payload);
        f
    }

    /// webui-api 慢客户端收到丢弃标记: a stalled client overflows the bounded
    /// subscriber queue; when it catches up, the loss arrives as an explicit
    /// LOG_GAP frame, never mixed into log content.
    #[tokio::test]
    async fn stalled_client_receives_explicit_gap_frame() {
        let (_sup, name, mut ws, ring) = firehose_setup().await;
        subscribe(&mut ws, &name, "out").await;

        // Synchronous burst on the single-threaded test runtime: the
        // subscriber task cannot run mid-burst, so overflow is deterministic.
        // Queue holds SUB_CHANNEL_BATCHES batches; the rest is dropped there.
        const BURST: usize = SUB_CHANNEL_BATCHES + 200;
        for i in 0..BURST {
            ring.push_batch(&[format!("burst-{i}")]);
        }
        // Let the subscriber drain the queued batches (no Gap among them).
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Next delivery must start with the accumulated Gap.
        ring.push_batch(&["trigger".to_string()]);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "gap frame never arrived"
            );
            let msg = ws.next().await.unwrap().unwrap();
            let (t, payload) = api::decode_ws_message(&msg.into_data()).unwrap();
            if t == msg_type::LOG_GAP {
                let (prog, stream, dropped) = api::decode_gap_frame(&rebuild(t, &payload)).unwrap();
                assert_eq!(prog, name);
                assert_eq!(stream, 0);
                assert_eq!(dropped, (BURST - SUB_CHANNEL_BATCHES) as u64);
                // The stream continues: the trigger line follows.
                let msg = ws.next().await.unwrap().unwrap();
                let (t, payload) = api::decode_ws_message(&msg.into_data()).unwrap();
                assert_eq!(t, msg_type::LOG);
                let (_, _, data) = api::decode_log_frame(&rebuild(t, &payload)).unwrap();
                assert!(
                    data.contains("trigger"),
                    "stream resumes after the gap: {data:?}"
                );
                return;
            }
            // (snapshot/status frames may precede; keep reading)
        }
    }

    /// 3.4 落盘完整性: a large burst through the real pump path lands on disk
    /// line-for-line while the ring stays bounded.
    #[test]
    fn pump_writes_every_line_to_disk_under_batching() {
        let tmp = std::env::temp_dir().join(format!("xk-hs-disk-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let total = 50_000;
        let text: String = (0..total)
            .map(|i| format!("disk-{i}-0123456789abcdef\n"))
            .collect();
        let ring = pump::Ring::new(1000);
        let path = tmp.join("d.log");
        // max_size None: no rotation — file must hold every line.
        pump::pump(
            std::io::Cursor::new(text.into_bytes()),
            ring.clone(),
            path.clone(),
            None,
            2,
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), total);
        assert_eq!(ring.tail(total + 1).len(), 1000, "ring stays bounded");
        let _ = std::fs::remove_dir_all(tmp);
    }
}

#[cfg(test)]
mod pending_ws_tests {
    use super::*;
    use crate::supervisor::{PendingDoc, PendingProgram};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn sup_with_pending() -> std::sync::Arc<Supervisor> {
        let config = crate::config::DaemonConfig::default();
        let dir = std::env::temp_dir().join(format!("xk-web-pending-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sup = Supervisor::new(config, &dir).unwrap();
        {
            let mut st = sup.state.lock().unwrap();
            st.pending = PendingDoc {
                programs: vec![PendingProgram {
                    app: "demo".into(),
                    program: "web".into(),
                    running: true,
                }],
                ..Default::default()
            };
        }
        sup
    }

    /// webui-api: pending changes ride STATUS delta frames with a `pending`
    /// field when they differ from the last sent state.
    #[tokio::test]
    async fn ws_status_delta_carries_pending_changes() {
        let sup = sup_with_pending();
        let app = build_router(sup.clone());
        let resp = app
            .oneshot(
                Request::get("/api/overview")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["pending"]["programs"][0]["program"], "web");
        assert_eq!(v["pending"]["programs"][0]["running"], true);
        // And /api/pending serves the same doc.
        let app2 = build_router(sup.clone());
        let resp = app2
            .oneshot(Request::get("/api/pending").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["programs"][0]["app"], "demo");
    }
}
