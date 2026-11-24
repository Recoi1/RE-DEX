//! Configuration from the environment only. A secret `X` may instead be given as a file path in `X_FILE` (Docker
//! secrets); nothing secret has a default.
use std::net::SocketAddr;
use std::path::PathBuf;

/// How many of something are allowed per window (seconds).
#[derive(Clone, Copy, Debug)]
pub struct Limit {
    pub max: i32,
    pub window_secs: i64,
}

const fn limit(max: i32, window_secs: i64) -> Limit {
    Limit { max, window_secs }
}

#[derive(Clone, Debug)]
pub struct Limits {
    /// Login attempts from one address (an IPv6 /64 counts as one).
    pub login_ip: Limit,
    /// Failed logins of one account from one address: counted before the password is hashed, given back on success.
    /// Keyed by address too, so guesses from elsewhere cannot lock the owner out.
    pub login_account: Limit,
    /// Failed logins of one account from all addresses together: the ceiling against guessing from many addresses.
    pub login_account_total: Limit,
    pub register_ip: Limit,
    /// Password-reset and verification mails to one address, and requests from one IP.
    pub mail_account: Limit,
    pub mail_ip: Limit,
    /// World tickets per account (each reconnect takes one).
    pub tickets: Limit,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            login_ip: limit(30, 900),
            login_account: limit(10, 900),
            login_account_total: limit(100, 900),
            register_ip: limit(5, 3600),
            mail_account: limit(3, 3600),
            mail_ip: limit(10, 3600),
            tickets: limit(30, 60),
        }
    }
}

#[derive(Clone, Debug)]
pub enum MailConfig {
    /// Dev and tests: each mail is a file in this folder.
    Dir(PathBuf),
    /// `smtps://user:pass@host:465` or `smtp://user:pass@host:587?tls=required` (lettre's URL form).
    Smtp(String),
}

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    /// Shared with the world servers: they present it to verify world tickets.
    pub world_secret: String,
    /// Base of the links in mails.
    pub public_url: String,
    pub mail: MailConfig,
    pub mail_from: String,
    /// Behind Caddy: the client address is the last `X-Forwarded-For` entry (the one the proxy added).
    pub trust_proxy: bool,
    pub session_ttl_secs: i64,
    pub ticket_ttl_secs: i64,
    pub verify_ttl_secs: i64,
    pub reset_ttl_secs: i64,
    pub limits: Limits,
    /// Argon2id passwords hashed at once (each takes 19 MiB).
    pub hash_slots: usize,
}

impl std::fmt::Debug for Config {
    // never prints the database URL (it may hold a password) or the world secret
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config").field("bind", &self.bind).field("public_url", &self.public_url).field("trust_proxy", &self.trust_proxy).finish_non_exhaustive()
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `name`, or the trimmed contents of the file `name_FILE` names.
fn secret(name: &str) -> Result<Option<String>, String> {
    if let Some(v) = var(name) {
        return Ok(Some(v));
    }
    match var(&format!("{name}_FILE")) {
        Some(p) => std::fs::read_to_string(&p).map(|s| Some(s.trim().to_string())).map_err(|e| format!("{name}_FILE {p}: {e}")),
        None => Ok(None),
    }
}

fn num(name: &str, default: i64) -> Result<i64, String> {
    var(name).map_or(Ok(default), |v| v.parse().map_err(|_| format!("{name} must be a number")))
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let database_url = secret("DATABASE_URL")?.ok_or("DATABASE_URL (or DATABASE_URL_FILE) is required")?;
        let world_secret = secret("REDEX_WORLD_SECRET")?.ok_or("REDEX_WORLD_SECRET (or REDEX_WORLD_SECRET_FILE) is required")?;
        if world_secret.len() < 32 {
            return Err("REDEX_WORLD_SECRET must be at least 32 characters".into());
        }
        let mail = match (secret("REDEX_SMTP_URL")?, var("REDEX_MAIL_DIR")) {
            (Some(url), _) => MailConfig::Smtp(url),
            (None, Some(dir)) => MailConfig::Dir(dir.into()),
            (None, None) => return Err("set REDEX_SMTP_URL (or REDEX_SMTP_URL_FILE) to send mail, or REDEX_MAIL_DIR to write mails to a folder (dev)".into()),
        };
        Ok(Config {
            bind: var("REDEX_BIND").unwrap_or_else(|| "127.0.0.1:8080".into()).parse().map_err(|_| "REDEX_BIND must be ADDR:PORT")?,
            database_url,
            world_secret,
            public_url: var("REDEX_PUBLIC_URL").unwrap_or_else(|| "http://127.0.0.1:8080".into()),
            mail,
            mail_from: var("REDEX_MAIL_FROM").unwrap_or_else(|| "RE:DEX <no-reply@localhost>".into()),
            trust_proxy: var("REDEX_TRUST_PROXY").is_some_and(|v| v == "1" || v == "true"),
            session_ttl_secs: num("REDEX_SESSION_TTL_SECS", 30 * 86_400)?,
            ticket_ttl_secs: num("REDEX_TICKET_TTL_SECS", 60)?,
            verify_ttl_secs: num("REDEX_VERIFY_TTL_SECS", 86_400)?,
            reset_ttl_secs: num("REDEX_RESET_TTL_SECS", 3_600)?,
            limits: Limits::default(),
            hash_slots: num("REDEX_HASH_SLOTS", 4)?.clamp(1, 64) as usize,
        })
    }
}
