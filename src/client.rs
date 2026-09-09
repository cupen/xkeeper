//! Thin HTTP client behind the CLI control subcommands, with the shared
//! exit-code contract (0 ok / 1 error / 2 config / 3 daemon unreachable).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

pub struct Client {
    agent: ureq::Agent,
    pub base: String,
    token: String,
}

impl Client {
    /// Build a client from the daemon config (host/port/token). Works with
    /// default settings even when no daemon config file exists.
    pub fn from_config(config: &crate::config::DaemonConfig) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(2))
                .timeout(Duration::from_secs(70))
                .build(),
            base: format!("http://{}:{}", config.daemon.host, config.daemon.port),
            token: config.daemon.auth_token.clone(),
        }
    }

    fn call(&self, method: &str, path: &str) -> Result<serde_json::Value> {
        let mut r = self
            .agent
            .request(method, &format!("{}{}", self.base, path));
        if !self.token.is_empty() {
            r = r.set("Authorization", &format!("Bearer {}", self.token));
        }
        let resp = r.call().map_err(|e| unreachable(e))?;
        let status = resp.status();
        let text = resp.into_string().unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({}));
        if (200..300).contains(&status) {
            Ok(v)
        } else {
            let msg = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("request failed")
                .to_string();
            Err(anyhow::anyhow!("api error {status}: {msg}"))
        }
    }

    /// Cheap reachability probe.
    pub fn health(&self) -> Result<()> {
        self.call("GET", "/v1/health").map(|_| ())
    }

    pub fn status(&self) -> Result<serde_json::Value> {
        self.call("GET", "/v1/status")
    }

    pub fn program(&self, name: &str) -> Result<serde_json::Value> {
        self.call("GET", &format!("/v1/programs/{name}"))
    }

    pub fn action(&self, name: &str, action: &str) -> Result<serde_json::Value> {
        self.call("POST", &format!("/v1/programs/{name}/{action}"))
    }

    pub fn reload(&self) -> Result<serde_json::Value> {
        self.call("POST", "/v1/reload")
    }

    pub fn shutdown(&self) -> Result<serde_json::Value> {
        self.call("POST", "/v1/shutdown")
    }

    pub fn log_tail(&self, name: &str, stream: &str, tail: usize) -> Result<Vec<String>> {
        let v = self.call(
            "GET",
            &format!("/v1/programs/{name}/logs?stream={stream}&tail={tail}"),
        )?;
        Ok(v.get("lines")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Stream `follow` output to stdout until interrupted or disconnected.
    pub fn log_follow(&self, name: &str, stream: &str) -> Result<()> {
        let mut path = format!("/v1/programs/{name}/logs?stream={stream}&tail=20&follow=1");
        if !self.token.is_empty() {
            path = format!("{path}&auth=1"); // token goes via header below
        }
        let mut r = self
            .agent
            .get(&format!("{}{}", self.base, path))
            .set("Accept", "text/plain");
        if !self.token.is_empty() {
            r = r.set("Authorization", &format!("Bearer {}", self.token));
        }
        let resp = r.call().map_err(|e| unreachable(e))?;
        if resp.status() != 200 {
            let code = resp.status();
            let msg = resp.into_string().unwrap_or_default();
            anyhow::bail!("api error {code}: {msg}");
        }
        let mut reader = resp.into_reader();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    use std::io::Write;
                    std::io::stdout().write_all(&buf[..n])?;
                    std::io::stdout().flush().ok();
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

fn unreachable(e: ureq::Error) -> anyhow::Error {
    let hint = "daemon unreachable (is `xkeeper run` active?)";
    match e {
        ureq::Error::Transport(t) => anyhow::anyhow!("{hint}: {t}"),
        other => anyhow::anyhow!("{hint}: {other}"),
    }
}

/// Exit code class for a client error, per the CLI contract:
/// 3 = daemon unreachable, 1 = everything else (2 handled at parse time).
pub fn exit_code_of(err: &anyhow::Error) -> i32 {
    let s = format!("{err:#}");
    if s.contains("daemon unreachable") {
        3
    } else {
        1
    }
}

/// Load the daemon config for client commands (defaults when absent).
pub fn load_config(config_path: &Path) -> Result<crate::config::DaemonConfig> {
    crate::config::DaemonConfig::load_or_default(config_path)
        .with_context(|| format!("failed to load daemon config {}", config_path.display()))
        .map(|(c, _)| c)
}
