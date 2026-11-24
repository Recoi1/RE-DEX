//! An account of the account backend (`redex-backend`): log in with email and password for a session token, then one
//! short-lived world ticket per connection (`Auth::WorldTicket`). The password is used once and never kept or logged.
use std::time::Duration;

#[derive(Clone)]
pub struct Account {
    /// The backend's base URL (`https://accounts.example`).
    pub server: String,
    /// The session token; it lives only in memory.
    token: String,
    pub account_id: u64,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("server", &self.server).field("account_id", &self.account_id).finish_non_exhaustive()
    }
}

fn agent() -> ureq::Agent {
    // no redirects: a password must not follow one to another host
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).http_status_as_error(false).max_redirects(0).build().into()
}

/// Passwords and tokens go over TLS only: `https://`, or `http://` to this PC (tests, a local dev backend).
fn secure(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.contains('@') {
        return false; // user:pass@host: the real host is after the @
    }
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h).trim_matches(['[', ']']);
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The `message` of an error answer, or the status.
fn error(status: u16, v: &serde_json::Value) -> String {
    v["message"].as_str().map_or_else(|| format!("the account service answered {status}"), String::from)
}

impl Account {
    /// Logs in; `device` names this PC in the account's session list.
    pub fn login(server: &str, email: &str, password: &str, device: &str) -> Result<Account, String> {
        let server = server.trim_end_matches('/').to_string();
        if !secure(&server) {
            return Err(format!("the account service must be https:// (plain http only on this PC): {server}"));
        }
        let agent = agent();
        let body = serde_json::json!({ "email": email, "password": password, "device": device });
        let mut r = agent.post(&format!("{server}/v1/login")).send_json(&body).map_err(|e| format!("cannot reach the account service: {e}"))?;
        let status = r.status().as_u16();
        let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
        if status != 200 {
            return Err(error(status, &v));
        }
        let (Some(token), Some(account_id)) = (v["token"].as_str(), v["account_id"].as_u64()) else {
            return Err("the account service gave a broken answer".into());
        };
        Ok(Account { server, token: token.into(), account_id, agent })
    }

    /// A fresh one-use ticket for `world` (the `host:port` of a `tls:` address).
    pub fn ticket(&self, world: &str) -> Result<String, String> {
        let mut r = self
            .agent
            .post(&format!("{}/v1/world-tickets", self.server))
            .header("Authorization", &format!("Bearer {}", self.token))
            .send_json(&serde_json::json!({ "world": world }))
            .map_err(|e| format!("cannot reach the account service: {e}"))?;
        let status = r.status().as_u16();
        let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
        match (status, v["ticket"].as_str()) {
            (200, Some(t)) => Ok(t.into()),
            _ => Err(error(status, &v)),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn passwords_go_over_tls_or_stay_on_this_pc() {
        for ok in ["https://accounts.example.org", "http://127.0.0.1:8080", "http://localhost:8080/", "http://[::1]:9"] {
            assert!(super::secure(ok), "{ok}");
        }
        for bad in ["http://accounts.example.org", "http://10.0.0.2:8080", "ftp://x", "accounts.example.org", "http://127.0.0.1.evil.org", "http://localhost:1@evil.example/", "http://127.0.0.1@evil.example"] {
            assert!(!super::secure(bad), "{bad}");
        }
    }
}
