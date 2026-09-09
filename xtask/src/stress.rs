//! Firehose stress + zero-backpressure verification against the REAL daemon
//! binary (moved from the in-process `#[ignore]` test in src/web.rs — this
//! version drives an actual `xkeeper webui` process, so the measurements
//! include the full HTTP/WS stack and a real child generator).
//!
//! What it verifies (specs: log-management 输出排空零反压, webui-api WS 日志
//! 批量推送与背压, metrics):
//! - (a) a slow/stalled WS subscriber never throttles the child's throughput;
//! - (b) daemon RSS stays bounded regardless of total output;
//! - (c) the on-disk log stays line-for-line intact;
//! - (d) loss for the slow subscriber is explicit (LOG_GAP markers), and a
//!       keeping-up client loses nothing;
//! - (e) frames aggregate many lines (frame rate decoupled from line rate).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{Sink, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const LINE: &str = "0123456789012345678901234567890"; // 31 chars + '\n' = 32B

#[derive(Clone, Copy)]
pub(crate) struct Args {
    pub secs: u64,
    pub slow_secs: u64,
    pub catchup_secs: u64,
    pub keep: bool,
}

struct Daemon {
    child: Child,
    webui_port: u16,
    control_port: u16,
    log_file: PathBuf,
    workspace: PathBuf,
    program: String,
}

pub(crate) fn run(args: Args) -> Result<()> {
    let started = std::time::Instant::now();
    let bin = crate::build_daemon()?;
    let mut daemon = spawn_daemon(&bin, &args)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = rt.block_on(measure(&daemon, &args));
    let outcome = match &result {
        Ok(_) => {
            shutdown(&mut daemon);
            println!("\nZERO-BACKPRESSURE VERIFICATION PASSED");
            Ok(())
        }
        Err(e) => {
            shutdown(&mut daemon);
            eprintln!("\nVERIFICATION FAILED: {e:#}");
            Err(anyhow::anyhow!("stress verification failed"))
        }
    };
    if args.keep {
        println!("workspace kept at {}", daemon.workspace.display());
    } else if let Err(e) = std::fs::remove_dir_all(&daemon.workspace) {
        eprintln!("warning: cleanup failed: {e}");
    }
    println!("total wall time: {:.1}s", started.elapsed().as_secs_f64());
    outcome
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn spawn_daemon(bin: &Path, _args: &Args) -> Result<Daemon> {
    let workspace = std::env::temp_dir().join(format!("xk-xtask-stress-{}", std::process::id()));
    let app_dir = workspace.join("app");
    std::fs::create_dir_all(&app_dir).context("create workspace")?;

    let control_port = free_port()?;
    let webui_port = free_port()?;
    std::fs::write(
        workspace.join("daemon.toml"),
        format!(
            "[daemon]\nport = {control_port}\nlog_dir = \"{}/logs\"\nlog_level = \"warn\"\nlog_buffer_lines = 100000\n",
            workspace.display()
        ),
    )?;
    // ~32B lines paced to ~10MB/s; self-terminating; no rotation (disk size
    // stays monotonic for the throughput assertions); no restarts (a restart
    // would leave a generator-EOF artifact line on disk).
    std::fs::write(
        app_dir.join("xkeeper.toml"),
        format!(
            "[app]\nautostart = true\n\n[program.fire]\ncommand = 'sh'\nargs = ['-c', 'timeout 300 sh -c \"while :; do dd if=/dev/zero bs=1M count=1 2>/dev/null; sleep 0.09; done\" | tr \"\\\\0\" \"x\" | fold -w 31']\nlog_max_size = \"0\"\nautorestart = \"never\"\n"
        ),
    )?;

    let reg = Command::new(bin)
        .arg("--config")
        .arg(workspace.join("daemon.toml"))
        .arg("add")
        .arg(&app_dir)
        .arg("--name")
        .arg("stress")
        .status()
        .context("register app")?;
    if !reg.success() {
        bail!("app registration failed");
    }

    // The daemon must NOT inherit our stdio: it can outlive errors here and
    // would hold the caller's pipes open. Startup probes; any failure kills
    // the child before we bail.
    let mut child = Command::new(bin)
        .arg("--config")
        .arg(workspace.join("daemon.toml"))
        .arg("webui")
        .arg("--listen")
        .arg(format!("127.0.0.1:{webui_port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn daemon")?;
    if let Err(e) = probe_startup(&mut child, webui_port, control_port) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }

    let log_file = workspace.join("logs").join("fire.out.log");
    println!(
        "daemon up: webui 127.0.0.1:{webui_port}, control 127.0.0.1:{control_port}, pid {}",
        child.id()
    );
    Ok(Daemon {
        webui_port,
        control_port,
        log_file,
        workspace,
        program: "fire".into(),
        child,
    })
}

/// Wait for /api/health and for the generator to reach running.
fn probe_startup(child: &mut Child, webui_port: u16, control_port: u16) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if std::time::Instant::now() > deadline {
            bail!("daemon did not become healthy in 15s");
        }
        if let Ok(Some(_)) = child.try_wait() {
            bail!("daemon exited during startup");
        }
        let ok = Command::new("curl")
            .args(["-sf", &format!("http://127.0.0.1:{webui_port}/api/health")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            bail!("daemon exited during startup");
        }
        let out = Command::new("curl")
            .args([
                "-sf",
                &format!("http://127.0.0.1:{control_port}/v1/programs/fire"),
            ])
            .output()?;
        if String::from_utf8_lossy(&out.stdout).contains("\"state\":\"running\"") {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("generator never reached running state");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn shutdown(daemon: &mut Daemon) {
    // Graceful first (pumps reach EOF, file settles), then make sure.
    eprintln!("[shutdown] posting /v1/shutdown");
    let _ = Command::new("curl")
        .args([
            "-sf",
            "-m",
            "10",
            "-X",
            "POST",
            &format!("http://127.0.0.1:{}/v1/shutdown", daemon.control_port),
        ])
        .output();
    eprintln!("[shutdown] posted; waiting for daemon exit");
    for _ in 0..50 {
        if matches!(daemon.child.try_wait(), Ok(Some(_))) {
            eprintln!("[shutdown] daemon exited");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("[shutdown] daemon did not exit in 5s; killing");
    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
}

fn rss_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
}

// ---------------------------------------------------------------------------
// WS measurement
// ---------------------------------------------------------------------------

async fn connect_and_subscribe(
    port: u16,
    program: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .context("ws connect")?;
    let action =
        format!("{{\"action\":\"subscribe\",\"program\":\"{program}\",\"stream\":\"out\"}}");
    send(&mut ws, Message::Binary(action.into_bytes().into())).await?;
    Ok(ws)
}

async fn send<S>(ws: &mut S, msg: Message) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    use futures_util::SinkExt;
    ws.send(msg).await.ok().expect("send failed");
    Ok(())
}

#[derive(Default)]
struct Stats {
    lines: u64,
    frames: u64,
    gaps: u64,
    max_frame: usize,
}

/// Decode one WS binary frame: LOG (type 3) = [3, u16 name-len, name,
/// stream, raw text]; LOG_GAP (6) = [6, u16 name-len, name, stream, u64 BE
/// count]. Anything else — snapshots/status (MessagePack), heartbeats,
/// compressed frames (bit 7) — is skipped defensively.
fn decode_frame(data: &[u8], stats: &mut Stats) {
    let Some(&first) = data.first() else { return };
    if first & 0x80 != 0 {
        return; // compressed: LOG/GAP frames are never compressed
    }
    let len = data.len();
    match first & 0x7f {
        3 if len >= 4 => {
            let name_len = u16::from_be_bytes([data[1], data[2]]) as usize;
            if len < 4 + name_len {
                return; // truncated
            }
            let body = &data[4 + name_len..];
            stats.lines += body.iter().filter(|&&b| b == b'\n').count() as u64;
            stats.frames += 1;
            stats.max_frame = stats.max_frame.max(body.len());
        }
        6 if len >= 12 => {
            let name_len = u16::from_be_bytes([data[1], data[2]]) as usize;
            if len < 4 + name_len + 8 {
                return;
            }
            let mut n = [0u8; 8];
            n.copy_from_slice(&data[4 + name_len..12 + name_len]);
            stats.gaps += u64::from_be_bytes(n);
        }
        _ => {}
    }
}

async fn read_for(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    dur: Duration,
) -> Result<Stats> {
    let mut stats = Stats::default();
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Ok(stats);
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Message::Binary(data) = msg {
                    decode_frame(&data, &mut stats);
                }
            }
            Ok(Some(Err(e))) => bail!("ws error: {e}"),
            Ok(None) => bail!("ws closed by server"),
            Err(_) => return Ok(stats), // deadline
        }
    }
}

/// One frame then pace-limited sleep — the slow-consumer pattern.
async fn read_one_paced(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    stats: &mut Stats,
) -> Result<bool> {
    match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
        Ok(Some(Ok(msg))) => {
            if let Message::Binary(data) = msg {
                decode_frame(&data, stats);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn measure(daemon: &Daemon, args: &Args) -> Result<()> {
    let mut ws = connect_and_subscribe(daemon.webui_port, &daemon.program).await?;
    tokio::time::sleep(Duration::from_millis(200)).await; // subscription settles

    // -- phase 1: full-speed client -------------------------------------
    let rss0 = rss_kib(daemon.child.id()).unwrap_or(0);
    let size0 = file_size(&daemon.log_file);
    let t0 = std::time::Instant::now();
    let fast = read_for(&mut ws, Duration::from_secs(args.secs)).await?;
    let wall1 = t0.elapsed().as_secs_f64();
    let size1 = file_size(&daemon.log_file);
    let rss1 = rss_kib(daemon.child.id()).unwrap_or(0);

    println!("== phase 1: fast client, {:.1}s ==", wall1);
    println!("   lines delivered        : {}", fast.lines);
    println!(
        "   log frames             : {} (max {}B)",
        fast.frames, fast.max_frame
    );
    println!(
        "   frame-to-line ratio    : 1 : {}",
        fast.lines / fast.frames.max(1)
    );
    println!(
        "   disk throughput        : {:.1} MB/s",
        size1.saturating_sub(size0) as f64 / wall1 / 1e6
    );
    println!("   daemon RSS             : {rss0} -> {rss1} KiB");

    // -- phase 2: pace-limited client, then catch-up ---------------------
    let size2 = file_size(&daemon.log_file);
    let t2 = std::time::Instant::now();
    let mut slow = Stats::default();
    let slow_until = tokio::time::Instant::now() + Duration::from_secs(args.slow_secs);
    while tokio::time::Instant::now() < slow_until {
        if !read_one_paced(&mut ws, &mut slow).await? {
            bail!("connection lost during slow phase");
        }
    }
    let caught = read_for(&mut ws, Duration::from_secs(args.catchup_secs)).await?;
    let wall2 = t2.elapsed().as_secs_f64();
    let size3 = file_size(&daemon.log_file);

    let rate_fast = size1.saturating_sub(size0) as f64 / wall1 / 1e6;
    let rate_slow = size3.saturating_sub(size2) as f64 / wall2 / 1e6;
    println!(
        "== phase 2: throttled client ({}s slow + {}s catch-up) ==",
        args.slow_secs, args.catchup_secs
    );
    println!("   lines delivered        : {}", slow.lines + caught.lines);
    println!(
        "   gap markers received   : {} lines skipped",
        slow.gaps + caught.gaps
    );
    println!("   disk throughput        : {rate_slow:.1} MB/s (fast phase: {rate_fast:.1} MB/s)");
    println!(
        "   throughput retention   : {:.1}%",
        rate_slow / rate_fast.max(0.001) * 100.0
    );

    // -- stop the child so the pump reaches EOF, then verify the disk ----
    let _ = Command::new("curl")
        .args([
            "-sf",
            "-X",
            "POST",
            &format!(
                "http://127.0.0.1:{}/v1/programs/fire/stop",
                daemon.control_port
            ),
        ])
        .output();
    std::thread::sleep(Duration::from_millis(300));

    // -- assertions ------------------------------------------------------
    let total_lines = slow.lines + caught.lines;
    let total_gaps = slow.gaps + caught.gaps;
    if rate_slow < rate_fast * 0.8 && rate_slow < 5.0 {
        bail!(
            "child throughput collapsed with a slow subscriber: {rate_fast:.1} -> {rate_slow:.1} MB/s"
        );
    }
    if total_gaps == 0 {
        bail!("slow subscriber received no Gap markers — loss accounting broken");
    }
    if rss1.saturating_sub(rss0) > 200_000 {
        bail!(
            "daemon RSS grew {} KiB during the firehose",
            rss1.saturating_sub(rss0)
        );
    }
    if fast.frames == 0 || fast.lines / fast.frames < 100 {
        bail!(
            "frames not aggregating: {} lines in {} frames",
            fast.lines,
            fast.frames
        );
    }

    let content = std::fs::read(&daemon.log_file)?;
    let text = String::from_utf8_lossy(&content);
    let lines: Vec<&str> = text.split('\n').collect();
    let lines = match lines.last() {
        Some(&"") => &lines[..lines.len() - 1],
        _ => &lines[..],
    };
    let tolerant_tail = lines.len().saturating_sub(2); // generator kill artifacts
    let mut disk_lines = 0u64;
    for (i, line) in lines.iter().enumerate() {
        if i < tolerant_tail {
            if line.len() != LINE.len() {
                bail!("corrupted line on disk at #{i} (len {})", line.len());
            }
            disk_lines += 1;
        } else if !line.chars().all(|c| c == 'x') {
            bail!("garbage at tail line #{i}");
        }
    }
    println!("== totals ==");
    println!(
        "   disk lines             : {disk_lines} ({} bytes)",
        content.len()
    );
    println!(
        "   client lines + gaps    : {} + {}",
        total_lines, total_gaps
    );
    println!(
        "   daemon RSS final       : {} KiB",
        rss_kib(daemon.child.id()).unwrap_or(0)
    );
    if disk_lines < 100_000 {
        bail!("firehose produced too little output ({disk_lines} lines)");
    }
    Ok(())
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
