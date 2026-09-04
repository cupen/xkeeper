//! The health-checker: a single thread that probes every registered
//! tcp/http/exec check and reports results to the supervisor loop.

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::debug;

use crate::config::{HealthCheck, HealthKind};

/// What the supervisor publishes for the checker to probe.
#[derive(Clone)]
pub struct HealthTask {
    pub check: HealthCheck,
}

pub type TaskMap = Arc<Mutex<HashMap<String, HealthTask>>>;

/// Probe every due task once per second and report via `report`.
/// Serial probing is fine at app-layer scale (see design D6).
pub fn checker_loop(tasks: TaskMap, report: impl Fn(String, bool) + Send + 'static) {
    let agent = ureq::AgentBuilder::new().build();
    let mut next_due: HashMap<String, Instant> = HashMap::new();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let snapshot: Vec<(String, HealthCheck)> = {
            let g = match tasks.lock() {
                Ok(g) => g.iter().map(|(k, v)| (k.clone(), v.check.clone())).collect(),
                Err(_) => continue,
            };
            g
        };
        let now = Instant::now();
        for (name, check) in snapshot {
            let due = next_due.get(&name).map(|t| now >= *t).unwrap_or(true);
            if !due {
                continue;
            }
            next_due.insert(name.clone(), now + Duration::from_secs_f64(check.interval.max(1.0)));
            let ok = probe(&agent, &check);
            debug!("health probe program[{name}]: {}", if ok { "ok" } else { "fail" });
            report(name, ok);
        }
    }
}

fn probe(agent: &ureq::Agent, check: &HealthCheck) -> bool {
    let timeout = Duration::from_secs_f64(check.timeout.max(0.1));
    match &check.kind {
        HealthKind::Http { url } => http_ok(agent, url, timeout),
        HealthKind::Tcp { addr } => tcp_ok(addr, timeout),
        HealthKind::Exec { command, args } => exec_ok(command, args, timeout),
    }
}

/// 2xx/3xx counts as healthy (matches the spec).
fn http_ok(agent: &ureq::Agent, url: &str, timeout: Duration) -> bool {
    match agent.get(url).timeout(timeout).call() {
        Ok(resp) => resp.status() < 400,
        Err(ureq::Error::Status(code, _)) => code < 400,
        Err(_) => false,
    }
}

fn tcp_ok(addr: &str, timeout: Duration) -> bool {
    match addr.to_socket_addrs() {
        Ok(addrs) => addrs
            .into_iter()
            .any(|a| TcpStream::connect_timeout(&a, timeout).is_ok()),
        Err(_) => false,
    }
}

fn exec_ok(command: &str, args: &[String], timeout: Duration) -> bool {
    let mut child = match std::process::Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn tcp_probe_succeeds_against_listener() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        // Accept exactly once so the thread can terminate and be joined.
        let h = std::thread::spawn(move || {
            if let Ok((_, _)) = l.accept() {
                // connect() already succeeded; drop the socket immediately.
            }
        });
        assert!(tcp_ok(&addr.to_string(), Duration::from_secs(2)));
        assert!(!tcp_ok("127.0.0.1:1", Duration::from_millis(200)));
        h.join().ok();
    }

    #[test]
    fn exec_probe_exit_codes() {
        let (ok_cmd, bad_cmd) = if cfg!(windows) {
            ("cmd /c exit 0", "cmd /c exit 1")
        } else {
            ("true", "false")
        };
        let mut ok = crate::config::split_command(ok_cmd);
        let mut bad = crate::config::split_command(bad_cmd);
        let c = ok.remove(0);
        assert!(exec_ok(&c, &ok, Duration::from_secs(5)));
        let c = bad.remove(0);
        assert!(!exec_ok(&c, &bad, Duration::from_secs(5)));
    }

    #[test]
    fn http_probe_against_raw_server() {
        use std::io::Write as _;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = l.accept() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut sock, &mut buf); // consume request
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        });
        let agent = ureq::AgentBuilder::new().build();
        assert!(http_ok(&agent, &format!("http://{addr}/"), Duration::from_secs(3)));
        h.join().ok();
    }
}
