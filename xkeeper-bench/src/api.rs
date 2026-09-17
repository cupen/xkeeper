//! /v1 control-plane client (design: bench is a pure external consumer of the
//! existing control plane — no daemon changes, same endpoints the CLI uses).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: Option<String>,
}

impl Client {
    /// `addr` is `HOST:PORT` (an optional `http://` prefix is tolerated).
    pub fn new(addr: &str, token: Option<&str>) -> Result<Self> {
        let addr = addr.trim();
        let addr = addr.strip_prefix("http://").unwrap_or(addr);
        if addr.is_empty() || !addr.contains(':') {
            return Err(anyhow!(
                "invalid daemon address {addr:?}: expected HOST:PORT (e.g. 127.0.0.1:7310)"
            ));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(180))
            .build();
        Ok(Self {
            agent,
            base: format!("http://{addr}"),
            token: token.filter(|t| !t.is_empty()).map(String::from),
        })
    }

    pub fn addr(&self) -> &str {
        self.base.trim_start_matches("http://")
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        let r = self.agent.request(method, &format!("{}{}", self.base, path));
        match &self.token {
            Some(t) => r.set("Authorization", &format!("Bearer {t}")),
            None => r,
        }
    }

    fn send_json(r: ureq::Request, body: Option<&Value>) -> Result<Value> {
        // 4xx/5xx are control-plane ANSWERS (409 conflict, 404 unknown...),
        // not transport failures; only Transport means "unreachable".
        let resp = match body {
            Some(b) => r
                .set("Content-Type", "application/json")
                .send_string(&b.to_string()),
            None => r.call(),
        };
        let resp = match resp {
            Ok(resp) => resp,
            Err(ureq::Error::Status(_, resp)) => resp,
            Err(e) => {
                return Err(anyhow!(e)).context("daemon unreachable (transport error)");
            }
        };
        let status = resp.status();
        let text = resp.into_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            Ok(v)
        } else {
            let msg = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or(&text)
                .to_string();
            Err(anyhow!("control plane answered {}: {}", status, msg))
        }
    }

    pub fn health(&self) -> Result<()> {
        Self::send_json(self.request("GET", "/v1/health"), None)
            .map(|_| ())
            .with_context(|| format!("cannot reach daemon at {}", self.addr()))
    }

    pub fn status(&self) -> Result<Value> {
        Self::send_json(self.request("GET", "/v1/status"), None)
    }

    pub fn daemon_version(&self) -> Result<String> {
        let v = self.status()?;
        Ok(v.pointer("/daemon/version")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string())
    }

    /// The daemon-side config path (status projection `config_source`); the
    /// connect topology reads app_dir/log_dir from that file.
    pub fn config_source(&self) -> Result<String> {
        let v = self.status()?;
        v.pointer("/daemon/config_source")
            .and_then(|x| x.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("status projection has no daemon.config_source"))
    }

    pub fn reload(&self) -> Result<Value> {
        Self::send_json(self.request("POST", "/v1/reload"), Some(&serde_json::json!({})))
    }

    pub fn apply(&self, app: Option<&str>) -> Result<Value> {
        let body = serde_json::json!({ "app": app });
        Self::send_json(self.request("POST", "/v1/apply"), Some(&body))
    }

    pub fn stop_program(&self, name: &str) -> Result<Value> {
        Self::send_json(
            self.request("POST", &format!("/v1/programs/{name}/stop")),
            Some(&serde_json::json!({})),
        )
    }

    pub fn shutdown(&self) -> Result<()> {
        Self::send_json(self.request("POST", "/v1/shutdown"), Some(&serde_json::json!({})))
            .map(|_| ())
    }

    /// States of the named programs from one status snapshot.
    /// Missing programs are simply absent from the result.
    pub fn program_states(&self, names: &[String]) -> Result<Vec<(String, String)>> {
        let v = self.status()?;
        let mut out = Vec::new();
        if let Some(ps) = v.get("programs").and_then(|p| p.as_array()) {
            for p in ps {
                let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let state = p.get("state").and_then(|s| s.as_str()).unwrap_or("");
                if names.iter().any(|n| n == name) && !state.is_empty() {
                    out.push((name.to_string(), state.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// Names of running programs carrying the bench prefix (collision check).
    pub fn bench_prefixed_programs(&self) -> Result<Vec<String>> {
        let v = self.status()?;
        let mut out = Vec::new();
        if let Some(ps) = v.get("programs").and_then(|p| p.as_array()) {
            for p in ps {
                if let Some(name) = p.get("name").and_then(|n| n.as_str()) {
                    if name.starts_with(crate::cases::BENCH_PREFIX) {
                        out.push(name.to_string());
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_forms_are_accepted() {
        assert!(Client::new("127.0.0.1:7310", None).is_ok());
        assert!(Client::new("http://127.0.0.1:7310", None).is_ok());
        assert!(Client::new("localhost:7310", Some("tok")).is_ok());
    }

    #[test]
    fn bad_addrs_are_rejected() {
        assert!(Client::new("", None).is_err());
        assert!(Client::new("7310", None).is_err(), "port-only is not an address");
        assert!(Client::new("  ", None).is_err());
    }
}
