//! Account logins on the central world: the client's `Auth::WorldTicket` is checked with the account backend, server to
//! server (`POST <backend>/internal/world-tickets/verify`, the shared secret as a bearer token). The backend says whose
//! ticket it is and uses it up; nothing the client says about itself counts.
use redex_proto::{Character, refuse};
use std::time::Duration;

pub struct Tickets {
    url: String,
    /// The backend's base URL.
    base: String,
    secret: String,
    /// This world's id: the `host:port` players dial (`tls:<id>`); a ticket is good only for the world it was asked for.
    pub world: String,
    agent: ureq::Agent,
}

/// How long a login waits for the backend: under the client's 5 s silence limit.
const ANSWER: Duration = Duration::from_secs(3);

impl Tickets {
    pub fn new(backend: &str, secret: &str, world: &str) -> Tickets {
        let agent = ureq::Agent::config_builder().timeout_global(Some(ANSWER)).http_status_as_error(false).build().into();
        // host names are not case-sensitive: the backend compares world ids lower-cased
        Tickets { url: format!("{}/internal/world-tickets/verify", backend.trim_end_matches('/')), base: backend.trim_end_matches('/').into(), secret: secret.into(), world: world.to_lowercase(), agent }
    }

    /// From `REDEX_BACKEND_URL`, `REDEX_WORLD_SECRET` (or `REDEX_WORLD_SECRET_FILE`) and `REDEX_WORLD_ID`.
    pub fn from_env() -> Result<Tickets, String> {
        let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        let backend = var("REDEX_BACKEND_URL").ok_or("REDEX_BACKEND_URL is required (the account backend, e.g. http://backend:8080)")?;
        let secret = match (var("REDEX_WORLD_SECRET"), var("REDEX_WORLD_SECRET_FILE")) {
            (Some(s), _) => s,
            (None, Some(f)) => std::fs::read_to_string(&f).map_err(|e| format!("REDEX_WORLD_SECRET_FILE {f}: {e}"))?.trim().to_string(),
            (None, None) => return Err("REDEX_WORLD_SECRET (or REDEX_WORLD_SECRET_FILE) is required".into()),
        };
        let world = var("REDEX_WORLD_ID").ok_or("REDEX_WORLD_ID is required (the host:port players dial, as in tls:<host:port>)")?;
        Ok(Tickets::new(&backend, &secret, &world))
    }

    /// The account's public id and the backend session the ticket belongs to (the ticket is used up), or the `Refuse` to send: `AUTH` when the backend
    /// turned it down (unknown, used, expired, another world's, logged out), `FULL` (retryable) when it did not answer.
    pub fn verify(&self, ticket: &str) -> Result<(u64, u64), (u8, String)> {
        // a fresh ticket is `rdx_t_` and 43 base64url characters: anything else is not worth a call
        let shaped = ticket.len() == 49 && ticket.starts_with("rdx_t_") && ticket[6..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !shaped {
            return Err((refuse::AUTH, "not a login ticket".into()));
        }
        let body = serde_json::json!({ "ticket": ticket, "world": self.world });
        let answer = self.agent.post(&self.url).header("Authorization", &format!("Bearer {}", self.secret)).send_json(&body);
        let mut r = match answer {
            Ok(r) => r,
            Err(e) => {
                eprintln!("redex-world: account backend unreachable: {e}");
                return Err((refuse::FULL, "the account service did not answer; try again".into()));
            }
        };
        let status = r.status().as_u16();
        let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
        match status {
            // the account as other players see it (its public id) and its backend session
            200 => v["public_id"].as_u64().filter(|&id| id > 0).zip(v["session_id"].as_u64()).ok_or_else(|| {
                eprintln!("redex-world: account backend answered 200 without an account id");
                (refuse::FULL, "the account service gave a broken answer; try again".to_string())
            }),
            401 if v["reason"].is_string() => Err((refuse::AUTH, format!("login ticket not accepted ({})", v["reason"].as_str().unwrap_or("?")))),
            _ => {
                // 401 without a reason: our secret is wrong; anything else: the backend is unwell
                eprintln!("redex-world: account backend answered {status} to a ticket check: {v}");
                Err((refuse::FULL, "the account service could not check the login; try again".into()))
            }
        }
    }

    /// Which of these backend sessions are dead (logged out, ended by a password change or reset, expired, or the
    /// account disabled): their players must leave the world.
    pub fn dead(&self, sessions: &[u64]) -> Result<Vec<u64>, String> {
        let r = self.agent.post(&format!("{}/internal/sessions/check", self.base)).header("Authorization", &format!("Bearer {}", self.secret)).send_json(&serde_json::json!({ "sessions": sessions }));
        let mut r = r.map_err(|e| format!("account backend unreachable: {e}"))?;
        let status = r.status().as_u16();
        let v: serde_json::Value = r.body_mut().read_json().map_err(|e| format!("account backend answered {status}: {e}"))?;
        match (status, v["dead"].as_array()) {
            (200, Some(d)) => Ok(d.iter().filter_map(serde_json::Value::as_u64).collect()),
            _ => Err(format!("account backend answered {status}: {v}")),
        }
    }

    /// The account's characters as the backend keeps them (the central world's account store).
    pub fn characters(&self, account: u64) -> Result<Vec<Character>, String> {
        let r = self.agent.get(&format!("{}/internal/accounts/{account}/characters", self.base)).header("Authorization", &format!("Bearer {}", self.secret)).call();
        list(r)
    }

    /// A checked `CharacterPut` to the backend, which merges it (`redex_proto::merge`); the account's list after it.
    pub fn put_character(&self, account: u64, c: &Character) -> Result<Vec<Character>, String> {
        let look: String = c.look.iter().map(|b| format!("{b:02x}")).collect();
        let body = serde_json::json!({ "id": format!("{:016x}", c.id), "name": c.name, "look": look, "created": c.created, "changed": c.changed });
        let r = self.agent.put(&format!("{}/internal/accounts/{account}/characters", self.base)).header("Authorization", &format!("Bearer {}", self.secret)).send_json(&body);
        list(r)
    }
}

/// A `{"characters": [...]}` answer (`redex-backend`'s `character_json`).
fn list(r: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Vec<Character>, String> {
    let mut r = r.map_err(|e| format!("account backend unreachable: {e}"))?;
    let status = r.status().as_u16();
    let v: serde_json::Value = r.body_mut().read_json().map_err(|e| format!("account backend answered {status}: {e}"))?;
    if status != 200 {
        return Err(format!("account backend answered {status}: {v}"));
    }
    let hex = |s: &str| (s.is_ascii() && s.len() % 2 == 0).then(|| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect::<Option<Vec<u8>>>()).flatten();
    v["characters"]
        .as_array()
        .ok_or("no character list")?
        .iter()
        .take(redex_proto::MAX_CHARACTERS)
        .map(|c| {
            Some(Character {
                id: u64::from_str_radix(c["id"].as_str()?, 16).ok()?,
                name: c["name"].as_str()?.into(),
                look: hex(c["look"].as_str()?)?,
                created: c["created"].as_u64()?,
                changed: c["changed"].as_u64()?,
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "a broken character in the backend's list".into())
}