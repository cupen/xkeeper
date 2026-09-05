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
use std::sync::atomic::Ordering;
use std::sync::Arc;
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
use crate::pump::Stream;
use crate::server::{program_info, status_doc, ProgramInfo, StatusDoc};
use crate::supervisor::{Command, Supervisor};

/// Replay context when a client subscribes to a log stream.
const LOG_TAIL_DEFAULT: usize = 200;
/// Status-diff cadence for WS sessions (daemon tick granularity is
/// `monitor_interval`, so this is at least twice as fine).
const WS_POLL: Duration = Duration::from_millis(500);
const WS_HEARTBEAT: Duration = Duration::from_secs(30);
/// How long an HTTP control request waits for the supervisor to execute it
/// (a stop may block up to the program's stop_timeout).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

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

async fn program_detail(
    State(sup): State<Arc<Supervisor>>,
    Path(name): Path<String>,
) -> Response {
    let st = sup.state.lock().unwrap();
    match st.programs.get(&name) {
        Some(p) => Json(program_info(p)).into_response(),
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
                )
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

async fn program_action(
    State(sup): State<Arc<Supervisor>>,
    Path((name, action)): Path<(String, String)>,
) -> Response {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let cmd = match action.as_str() {
        "start" => Command::Start { name, reply: Some(tx) },
        "stop" => Command::Stop { name, reply: Some(tx) },
        "restart" => Command::Restart { name, reply: Some(tx) },
        other => {
            return api_error(
                StatusCode::NOT_FOUND,
                &format!("unknown action {other:?} (start | stop | restart)"),
            )
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
    if let Ok(payload) = rmp_serde::to_vec(&snap) {
        let _ = out_tx
            .send(api::encode_ws_message(msg_type::SNAPSHOT, &payload))
            .await;
    }

    let mut heartbeat = tokio::time::interval(WS_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut subs: HashMap<(String, Stream), tokio::task::JoinHandle<()>> = HashMap::new();

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
                                        sup.clone(), out_tx.clone(), program, stream,
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
                // the control-plane loop is the single writer.
                let cur = status_doc(&sup).programs;
                let delta = status_delta(&last, &cur);
                last = cur;
                if !delta.is_empty() {
                    if let Ok(payload) = rmp_serde::to_vec(&delta) {
                        let _ = out_tx.send(api::encode_ws_message(msg_type::STATUS, &payload)).await;
                    }
                }
            }
            _ = heartbeat.tick() => {
                let _ = out_tx.send(api::encode_ws_message(msg_type::HEARTBEAT, b"null")).await;
            }
        }
    }

    for (_, handle) in subs.drain() {
        handle.abort();
    }
    writer.abort();
}

/// Replay the current tail once, then forward ring-buffer lines as they are
/// pumped (log-management spec: follow pushes lines as they are written).
async fn subscribe_logs(
    sup: Arc<Supervisor>,
    out_tx: mpsc::Sender<Vec<u8>>,
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
        let _ = out_tx.send(api::encode_log_frame(&program, sb, data.as_bytes())).await;
    }

    // The ring's subscribe channel is a blocking std mpsc receiver; poll it
    // without blocking the async runtime.
    let rx = ring.subscribe();
    loop {
        let mut got = false;
        while let Ok(line) = rx.try_recv() {
            got = true;
            let mut data = line;
            data.push('\n');
            if out_tx.send(api::encode_log_frame(&program, sb, data.as_bytes())).await.is_err() {
                return; // client went away
            }
        }
        if !got {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoreConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_sup() -> Arc<Supervisor> {
        let core = CoreConfig::default();
        let dir = std::env::temp_dir().join(format!("xkeeper-web-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        Supervisor::new(core, &dir).unwrap()
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, axum::body::Bytes) {
        let resp = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
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
            app: "app".into(), name: "a".into(), state: "running".into(),
            pid: Some(1), unhealthy: false, uptime_secs: 1.0, total_exits: 0,
            restart_backoff: 1.0, last_exit: None, fatal_reason: None, wait_reason: None,
        };
        let b_changed = { let mut x = a.clone(); x.state = "backoff".into(); x };
        let delta = status_delta(&[a.clone()], &[b_changed.clone()]);
        assert_eq!(delta, vec![b_changed]);
        assert!(status_delta(&[a.clone()], &[a]).is_empty());
    }
}
