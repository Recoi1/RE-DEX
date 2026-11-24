//! RE:DEX account backend: accounts (Argon2id), email verification, password reset, sessions per device, account
//! settings, the trainer profile, rate limits, an audit log, and short-lived world tickets that world servers verify
//! server to server (`POST /internal/world-tickets/verify` with the shared secret). Every token is random, handed out
//! once and stored as its SHA-256.
pub mod config;
mod mail;

use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::{HeaderMap, StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use base64::Engine;
use config::{Config, Limit};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

pub use mail::Mailer;

pub struct App {
    pub db: PgPool,
    pub cfg: Config,
    pub mail: Mailer,
    hashing: Semaphore,
}

pub type Shared = Arc<App>;

impl App {
    pub fn new(db: PgPool, cfg: Config) -> Result<Shared, String> {
        let mail = Mailer::new(&cfg.mail, &cfg.mail_from)?;
        Ok(Arc::new(App { db, hashing: Semaphore::new(cfg.hash_slots), cfg, mail }))
    }
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Passwords: at least this many characters, at most `PASSWORD_MAX_BYTES` (Argon2 work stays bounded).
pub const PASSWORD_MIN_CHARS: usize = 10;
pub const PASSWORD_MAX_BYTES: usize = 256;
/// A trainer name, as the world server cuts names (`redex_proto::NAME_CHARS`).
pub const TRAINER_NAME_CHARS: usize = 16;
/// Usernames: 3 to 16 ASCII letters, digits, `_`, `-` or `.`, starting with a letter or digit, and not a name of
/// staff or the game's makers (`redex_proto::names::reserved`, the world server's own check).
/// Days an account may stay unconfirmed before `prune` deletes it.
pub const UNVERIFIED_DAYS: i32 = 7;
pub const USERNAME_MIN: usize = 3;
pub const USERNAME_MAX: usize = 16;
const DEVICE_CHARS: usize = 64;
const WORLD_CHARS: usize = 64;
pub const SESSION_PREFIX: &str = "rdx_s_";
pub const TICKET_PREFIX: &str = "rdx_t_";
const MAIL_PREFIX: &str = "rdx_m_";

pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/verify-email", get(verify_page))
        .route("/reset-password", get(reset_page))
        .route("/v1/register", post(register))
        .route("/v1/verify-email", post(verify_email))
        .route("/v1/verify-email/resend", post(resend_verification))
        .route("/v1/login", post(login))
        .route("/v1/logout", post(logout))
        .route("/v1/logout-all", post(logout_all))
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{id}", delete(revoke_session))
        .route("/v1/password/forgot", post(forgot_password))
        .route("/v1/password/reset", post(reset_password))
        .route("/v1/password/change", post(change_password))
        .route("/v1/account", get(account))
        .route("/v1/account/settings", patch(update_settings))
        .route("/v1/profile", get(profile).put(put_profile))
        .route("/v1/characters", get(characters))
        .route("/v1/characters/{id}", axum::routing::put(put_character).delete(delete_character))
        .route("/v1/world-tickets", post(issue_ticket))
        .route("/internal/world-tickets/verify", post(verify_ticket))
        .route("/internal/sessions/check", post(check_sessions))
        .route("/internal/accounts/{id}/characters", get(world_characters).put(world_put_character))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(StatusCode::SERVICE_UNAVAILABLE, REQUEST_TIMEOUT))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(app)
}

/// Deletes what has expired: rate-limit windows, used or stale tickets and mail tokens, long-dead sessions, and accounts
/// whose address was never confirmed within `UNVERIFIED_DAYS` (their usernames free again; everything of theirs cascades).
pub async fn prune(db: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM accounts WHERE email_verified_at IS NULL AND created_at < now() - make_interval(days => $1)").bind(UNVERIFIED_DAYS).execute(db).await?;
    sqlx::query("DELETE FROM rate_limits WHERE window_start < now() - interval '1 day'").execute(db).await?;
    sqlx::query("DELETE FROM world_tickets WHERE expires_at < now() - interval '1 hour'").execute(db).await?;
    sqlx::query("DELETE FROM email_tokens WHERE expires_at < now() - interval '7 days'").execute(db).await?;
    sqlx::query("DELETE FROM sessions WHERE expires_at < now() - interval '30 days' OR revoked_at < now() - interval '30 days'").execute(db).await?;
    Ok(())
}

// ---- errors

#[derive(Debug)]
pub enum ApiError {
    Bad(&'static str),
    Unauthorized(&'static str),
    Forbidden(&'static str),
    NotFound,
    Conflict(&'static str),
    TooMany,
    /// Every hashing slot stayed busy for `HASH_WAIT`: try again shortly.
    Busy,
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ApiError::Bad(m) => (StatusCode::BAD_REQUEST, "bad_request", m),
            ApiError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "unauthorized", m),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden", m),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found"),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m),
            ApiError::TooMany => (StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many attempts; try again later"),
            ApiError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy", "the service is busy; try again in a moment"),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error"),
        };
        (status, Json(json!({ "error": code, "message": message }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "database");
        ApiError::Internal
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---- tokens and passwords

/// A new random token (`prefix` + 32 random bytes, base64url) and its hash.
fn new_token(prefix: &str) -> ApiResult<(String, Vec<u8>)> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| {
        tracing::error!(error = %e, "OS random generator");
        ApiError::Internal
    })?;
    let t = format!("{prefix}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b));
    let h = token_hash(&t);
    Ok((t, h))
}

pub fn token_hash(t: &str) -> Vec<u8> {
    Sha256::digest(t.as_bytes()).to_vec()
}

/// Argon2id at OWASP's minimum (19 MiB, 2 passes, 1 lane).
fn argon() -> argon2::Argon2<'static> {
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, argon2::Params::new(19_456, 2, 1, None).expect("argon2 params"))
}

/// How long a request waits for a hashing slot before it is told the service is busy: a flood of password checks must
/// not queue everyone else behind it.
pub const HASH_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
/// Every request is answered within this, whatever it waits on.
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn hashing_slot(app: &App) -> ApiResult<tokio::sync::SemaphorePermit<'_>> {
    match tokio::time::timeout(HASH_WAIT, app.hashing.acquire()).await {
        Ok(Ok(p)) => Ok(p),
        Ok(Err(_)) => Err(ApiError::Internal),
        Err(_) => Err(ApiError::Busy),
    }
}

async fn hash_password(app: &App, pw: String) -> ApiResult<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let _slot = hashing_slot(app).await?;
    tokio::task::spawn_blocking(move || {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|_| ApiError::Internal)?;
        let salt = SaltString::encode_b64(&b).map_err(|_| ApiError::Internal)?;
        argon().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|_| ApiError::Internal)
    })
    .await
    .map_err(|_| ApiError::Internal)?
}

/// Whether `pw` matches `hash`. With no hash (no such account) a dummy one is checked, so the answer takes as long.
async fn check_password(app: &App, pw: String, hash: Option<String>) -> ApiResult<bool> {
    use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
    static DUMMY: OnceLock<String> = OnceLock::new();
    let _slot = hashing_slot(app).await?;
    tokio::task::spawn_blocking(move || {
        let real = hash.is_some();
        let hash = hash.unwrap_or_else(|| {
            DUMMY.get_or_init(|| argon().hash_password(b"not a password", &SaltString::encode_b64(b"redex-dummy-salt").unwrap()).unwrap().to_string()).clone()
        });
        let Ok(parsed) = PasswordHash::new(&hash) else {
            tracing::error!("a stored password hash does not parse");
            return Ok(false);
        };
        Ok(argon().verify_password(pw.as_bytes(), &parsed).is_ok() && real)
    })
    .await
    .map_err(|_| ApiError::Internal)?
}

fn check_new_password(pw: &str) -> ApiResult<()> {
    if pw.len() > PASSWORD_MAX_BYTES {
        return Err(ApiError::Bad("password too long"));
    }
    if pw.chars().count() < PASSWORD_MIN_CHARS {
        return Err(ApiError::Bad("password must be at least 10 characters"));
    }
    Ok(())
}

/// The address as typed (trimmed) and as compared (lower case), or Bad.
fn check_email(e: &str) -> ApiResult<(String, String)> {
    let e = e.trim();
    let ok = (3..=254).contains(&e.len())
        && !e.chars().any(|c| c.is_whitespace() || c.is_control())
        && e.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.') && !d.contains('@'));
    if !ok {
        return Err(ApiError::Bad("not an email address"));
    }
    Ok((e.to_string(), e.to_lowercase()))
}

/// Visible text only: no control, bidi or zero-width characters; trimmed and cut to `max` characters.
fn clean_text(s: &str, max: usize) -> String {
    let bad = |c: char| c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}');
    let s: String = s.chars().filter(|&c| !bad(c)).collect();
    s.trim().chars().take(max).collect::<String>().trim_end().to_string()
}

fn trainer_name(s: &str) -> ApiResult<String> {
    let n = clean_text(s, TRAINER_NAME_CHARS);
    if n.is_empty() { Err(ApiError::Bad("trainer name is empty")) } else { Ok(n) }
}

/// The username as typed and as compared (lower case), or Bad.
fn check_username(u: &str) -> ApiResult<(String, String)> {
    let u = u.trim();
    if !(USERNAME_MIN..=USERNAME_MAX).contains(&u.len()) {
        return Err(ApiError::Bad("username must be 3 to 16 characters"));
    }
    let ok = u.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) && u.as_bytes()[0].is_ascii_alphanumeric();
    if !ok {
        return Err(ApiError::Bad("username: letters, digits, _ - and . only, starting with a letter or digit"));
    }
    if redex_proto::names::reserved(u) {
        return Err(ApiError::Bad("this username is reserved"));
    }
    Ok((u.to_string(), u.to_lowercase()))
}

/// A character's JSON: `id` 16 hex digits, `look` hex, times in ms since the Unix epoch.
pub fn character_json(c: &redex_proto::Character) -> Value {
    let look: String = c.look.iter().map(|b| format!("{b:02x}")).collect();
    json!({ "id": format!("{:016x}", c.id), "name": c.name, "look": look, "created": c.created, "changed": c.changed })
}

#[derive(Deserialize)]
struct CharacterBody {
    name: String,
    look: String,
    created: u64,
    changed: u64,
}

/// A character as the account keeps it (the name cleaned), or Bad: id 0, a look `Appearance::from_bytes` refuses (the
/// world server's check), or `changed` more than a day ahead of now.
fn check_character(id: &str, b: CharacterBody) -> ApiResult<redex_proto::Character> {
    let id = u64::from_str_radix(id, 16).ok().filter(|&i| i != 0 && id.len() == 16).ok_or(ApiError::Bad("character id must be 16 hex digits, not 0"))?;
    let look: Option<Vec<u8>> = (b.look.is_ascii() && b.look.len() % 2 == 0 && b.look.len() <= 2 * redex_proto::MAX_APPEARANCE)
        .then(|| (0..b.look.len()).step_by(2).map(|i| u8::from_str_radix(&b.look[i..i + 2], 16).ok()).collect())
        .flatten();
    let look = look.filter(|l| redex_proto::appearance::Appearance::from_bytes(l).is_some()).ok_or(ApiError::Bad("not a valid look"))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    if b.changed > now.saturating_add(redex_proto::DAY_MS) {
        return Err(ApiError::Bad("changed is in the future"));
    }
    let name = clean_text(&b.name, redex_proto::NAME_CHARS);
    let name = if redex_proto::names::reserved(&name) { "Trainer".to_string() } else { name };
    Ok(redex_proto::Character { id, name, look, created: b.created, changed: b.changed })
}

type CharacterRow = (i64, String, Vec<u8>, i64, i64);

async fn load_characters(db: &mut sqlx::PgConnection, account: i64) -> ApiResult<Vec<redex_proto::Character>> {
    let rows: Vec<CharacterRow> =
        sqlx::query_as("SELECT character_id, name, look, created_ms, changed_ms FROM account_characters WHERE account_id = $1 ORDER BY created_ms, character_id")
            .bind(account)
            .fetch_all(db)
            .await?;
    // ids and times are u64 kept bit for bit in BIGINT
    Ok(rows.into_iter().map(|(id, name, look, created, changed)| redex_proto::Character { id: id as u64, name, look, created: created as u64, changed: changed as u64 }).collect())
}

/// Puts `c` by `redex_proto::merge` (the newest `changed` wins, at most `MAX_CHARACTERS`), one put of an account at a
/// time; the account's list after it.
async fn merge_character(db: &PgPool, account: i64, c: redex_proto::Character) -> ApiResult<Vec<redex_proto::Character>> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT 1 FROM accounts WHERE id = $1 FOR UPDATE").bind(account).execute(&mut *tx).await?;
    let mut list = load_characters(&mut tx, account).await?;
    if redex_proto::merge(&mut list, c.clone()) {
        sqlx::query(
            "INSERT INTO account_characters (account_id, character_id, name, look, created_ms, changed_ms) VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (account_id, character_id) DO UPDATE SET name = $3, look = $4, created_ms = $5, changed_ms = $6",
        )
        .bind(account)
        .bind(c.id as i64)
        .bind(&c.name)
        .bind(&c.look)
        .bind(c.created as i64)
        .bind(c.changed as i64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(list)
}

fn characters_json(list: &[redex_proto::Character]) -> Json<Value> {
    Json(json!({ "characters": list.iter().map(character_json).collect::<Vec<_>>() }))
}

/// The shared world secret as the bearer token, or Unauthorized.
fn world_only(app: &App, headers: &HeaderMap, ip: &str) -> ApiResult<()> {
    if bearer(headers).is_some_and(|s| same_secret(s, &app.cfg.world_secret)) {
        return Ok(());
    }
    tracing::warn!(%ip, "internal call without the world secret");
    Err(ApiError::Unauthorized("world secret required"))
}

/// Equal-time comparison (of the hashes, so lengths do not show either).
fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- rate limits (fixed windows in Postgres, so every backend instance shares them)

/// Counts one event for `key`; whether it is within `l`.
async fn hit(db: &PgPool, key: &str, l: Limit) -> ApiResult<bool> {
    let n: i32 = sqlx::query_scalar(
        "INSERT INTO rate_limits (key, window_start, count) VALUES ($1, to_timestamp(floor(extract(epoch from now())::float8 / $2) * $2), 1)
         ON CONFLICT (key, window_start) DO UPDATE SET count = rate_limits.count + 1 RETURNING count",
    )
    .bind(key)
    .bind(l.window_secs as f64)
    .fetch_one(db)
    .await?;
    Ok(n <= l.max)
}

/// Gives back one event `hit` counted for `key` in the current window (an attempt that turned out fine).
async fn unhit(db: &PgPool, key: &str, l: Limit) -> ApiResult<()> {
    sqlx::query("UPDATE rate_limits SET count = count - 1 WHERE key = $1 AND window_start = to_timestamp(floor(extract(epoch from now())::float8 / $2) * $2) AND count > 0")
        .bind(key)
        .bind(l.window_secs as f64)
        .execute(db)
        .await?;
    Ok(())
}

/// What an address counts as for rate limits: an IPv4 address itself, an IPv6 address by its /64 (one host or user
/// gets a whole /64), an IPv4-mapped IPv6 address as its IPv4. Anything that does not parse stays as it is.
pub fn bucket(ip: &str) -> String {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
        Ok(v4) => v4.to_string(),
        Err(_) => ip.to_string(),
    }
}

/// The /48 an IPv6 address is in (a site or tunnel-broker allocation holds 65,536 /64s), None for IPv4.
pub fn bucket48(ip: &str) -> Option<String> {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            Some(format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2]))
        }
        _ => None,
    }
}

/// Counts one `kind` event from `ip`: per address (`bucket`), and for IPv6 also per /48 with 8 times the room.
async fn limit_ip(db: &PgPool, kind: &str, ip: &str, l: Limit) -> ApiResult<()> {
    limit(db, &format!("{kind}:ip:{}", bucket(ip)), l).await?;
    if let Some(b) = bucket48(ip) {
        limit(db, &format!("{kind}:ip48:{b}"), Limit { max: l.max.saturating_mul(8), ..l }).await?;
    }
    Ok(())
}

/// Counts one failed-login attempt for `who` (an account id, or the typed name when there is none) from this address,
/// before any password is hashed, and for an account its total from everywhere. Over either limit: TooMany, and nothing
/// was hashed. The keys to give back (`unhit`) when the password turns out right.
async fn count_attempt(app: &App, kind: &str, who: &str, account: Option<i64>, ip: &str) -> ApiResult<Vec<(String, Limit)>> {
    let here = (format!("{kind}:fail:{who}:{}", bucket(ip)), app.cfg.limits.login_account);
    // an unknown name has a total of its own too: a guess at a real account and at no account cost the same writes
    let total = (format!("{kind}:fail:total:{who}"), app.cfg.limits.login_account_total);
    let here_ok = hit(&app.db, &here.0, here.1).await?;
    let total_ok = hit(&app.db, &total.0, total.1).await?;
    // over the total only: guesses from many addresses must not lock the owner out of the addresses they play from
    if !here_ok || (!total_ok && !known_address(app, account, ip).await?) {
        return Err(ApiError::TooMany);
    }
    Ok(vec![here, total])
}

/// Ends the account's "known addresses" (`known_address`), all but `keep`'s: after "log out everywhere", a password
/// reset or change, an address a hijacker used must not stay trusted. The sessions' addresses are cleared (the audit
/// log keeps them).
async fn forget_addresses<'e>(db: impl sqlx::PgExecutor<'e>, account: i64, keep: Option<i64>) -> ApiResult<()> {
    sqlx::query("UPDATE sessions SET ip = NULL WHERE account_id = $1 AND id IS DISTINCT FROM $2").bind(account).bind(keep).execute(db).await?;
    Ok(())
}

/// Whether `ip` is in the same address bucket as one of the account's sessions of the last 30 days.
async fn known_address(app: &App, account: Option<i64>, ip: &str) -> ApiResult<bool> {
    let Some(id) = account else { return Ok(false) };
    let ips: Vec<Option<String>> = sqlx::query_scalar("SELECT DISTINCT ip FROM sessions WHERE account_id = $1 AND created_at > now() - interval '30 days'")
        .bind(id)
        .fetch_all(&app.db)
        .await?;
    let here = bucket(ip);
    Ok(ips.iter().flatten().any(|i| bucket(i) == here))
}

/// Whether a (lower-cased) username has an account.
async fn username_taken(db: &PgPool, norm: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts WHERE username_norm = $1)").bind(norm).fetch_one(db).await?)
}

/// A random public id for an account (63 bits, never 0): what other players see, never the sequential account id.
fn new_public_id() -> ApiResult<i64> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|_| ApiError::Internal)?;
    Ok(((u64::from_le_bytes(b) >> 1).max(1)) as i64)
}

async fn limit(db: &PgPool, key: &str, l: Limit) -> ApiResult<()> {
    if hit(db, key, l).await? { Ok(()) } else { Err(ApiError::TooMany) }
}

async fn audit(db: &PgPool, account: Option<i64>, event: &str, ip: &str, detail: Value) {
    let r = sqlx::query("INSERT INTO audit_log (account_id, event, ip, detail) VALUES ($1, $2, $3, $4)").bind(account).bind(event).bind(ip).bind(detail).execute(db).await;
    if let Err(e) = r {
        tracing::error!(error = %e, event, "audit log write failed");
    }
}

// ---- extractors

/// The caller's address: the socket's, or behind a trusted proxy the last `X-Forwarded-For` entry.
pub struct ClientIp(pub String);

impl FromRequestParts<Shared> for ClientIp {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, ApiError> {
        if app.cfg.trust_proxy {
            if let Some(ip) = parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit(',').next()).map(str::trim).filter(|v| !v.is_empty()) {
                return Ok(ClientIp(ip.to_string()));
            }
        }
        let ip = parts.extensions.get::<ConnectInfo<SocketAddr>>().map_or_else(|| "unknown".into(), |c| c.0.ip().to_string());
        Ok(ClientIp(ip))
    }
}

/// A live session from `Authorization: Bearer <token>` (not expired, not revoked, account not disabled).
pub struct Authed {
    pub account: i64,
    pub session: i64,
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim)
}

impl FromRequestParts<Shared> for Authed {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, ApiError> {
        let t = bearer(&parts.headers).filter(|t| t.starts_with(SESSION_PREFIX)).ok_or(ApiError::Unauthorized("log in first"))?;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "UPDATE sessions s SET last_seen_at = now() FROM accounts a
             WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.expires_at > now() AND a.id = s.account_id AND a.disabled_at IS NULL
             RETURNING s.id, s.account_id",
        )
        .bind(token_hash(t))
        .fetch_optional(&app.db)
        .await?;
        let (session, account) = row.ok_or(ApiError::Unauthorized("session expired or logged out"))?;
        Ok(Authed { account, session })
    }
}

// ---- handlers

async fn healthz(State(app): State<Shared>) -> Response {
    match sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&app.db).await {
        Ok(_) => Json(json!({ "status": "ok" })).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "health check: database");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "database unavailable" }))).into_response()
        }
    }
}

/// A page for a mail link: plain HTML, one inline script and one inline style, both allowed by hash in a CSP that
/// allows nothing else (no other origin, no forms, no frames). The script reads the code from the fragment (never sent
/// to a server), clears it from the address bar and POSTs it to this same origin.
fn page(title: &str, body: &str, script: &str) -> Response {
    const STYLE: &str = "body{font:16px system-ui,sans-serif;max-width:28rem;margin:3rem auto;padding:0 1rem}input,button{font:inherit;padding:.4rem}";
    let sha = |s: &str| base64::engine::general_purpose::STANDARD.encode(Sha256::digest(s.as_bytes()));
    let csp = format!(
        "default-src 'none'; script-src 'sha256-{}'; style-src 'sha256-{}'; connect-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'",
        sha(script),
        sha(STYLE)
    );
    let html = format!("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title}</title><style>{STYLE}</style></head><body><h1>{title}</h1>{body}<script>{script}</script></body></html>");
    let headers = [
        ("content-type", "text/html; charset=utf-8".to_string()),
        ("content-security-policy", csp),
        ("x-content-type-options", "nosniff".into()),
        ("referrer-policy", "no-referrer".into()),
        ("cache-control", "no-store".into()),
    ];
    let mut r = html.into_response();
    for (k, v) in headers {
        if let Ok(v) = axum::http::HeaderValue::from_str(&v) {
            r.headers_mut().insert(k, v);
        }
    }
    r
}

/// The code from the fragment, the fragment cleared (so it stays out of history and screenshots).
const TOKEN_JS: &str = "const t=new URLSearchParams(location.hash.slice(1)).get('token');history.replaceState(null,'',location.pathname);const o=document.getElementById('out');";

async fn verify_page() -> Response {
    let script = format!("{TOKEN_JS}if(!t){{o.textContent='This link has no code. Enter the code from the mail in RE:DEX instead.'}}else{{fetch('/v1/verify-email',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{token:t}})}}).then(r=>r.json().then(j=>{{o.textContent=r.ok?'Your address is confirmed. You can play online now.':(j.message||'That did not work.')}})).catch(()=>{{o.textContent='The account service could not be reached.'}})}}");
    page("Confirm your address", "<p id=\"out\">Confirming...</p>", &script)
}

async fn reset_page() -> Response {
    let body = "<form id=\"f\"><label>New password (10 characters or more)<br><input id=\"p\" type=\"password\" minlength=\"10\" maxlength=\"256\" autocomplete=\"new-password\" required></label><p><button>Set the new password</button></p></form><p id=\"out\"></p>";
    let script = format!("{TOKEN_JS}const f=document.getElementById('f');if(!t){{f.hidden=true;o.textContent='This link has no code. Ask for a new reset mail in RE:DEX.'}}f.addEventListener('submit',e=>{{e.preventDefault();fetch('/v1/password/reset',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{token:t,password:document.getElementById('p').value}})}}).then(r=>r.json().then(j=>{{if(r.ok){{f.hidden=true;o.textContent='Your password is changed. Every device was logged out; log in again with the new one.'}}else{{o.textContent=j.message||'That did not work.'}}}})).catch(()=>{{o.textContent='The account service could not be reached.'}})}});");
    page("Choose a new password", body, &script)
}

/// A mail token for `account`, sent in the background (the answer does not wait for, or reveal, the mail).
async fn send_mail_token(app: &Shared, account: i64, to: String, purpose: &'static str) -> ApiResult<()> {
    let (token, hash) = new_token(MAIL_PREFIX)?;
    let ttl = if purpose == "verify" { app.cfg.verify_ttl_secs } else { app.cfg.reset_ttl_secs };
    sqlx::query("INSERT INTO email_tokens (token_hash, account_id, purpose, expires_at) VALUES ($1, $2, $3, now() + make_interval(secs => $4))")
        .bind(hash)
        .bind(account)
        .bind(purpose)
        .bind(ttl as f64)
        .execute(&app.db)
        .await?;
    let (subject, path, what) = match purpose {
        "verify" => ("Confirm your RE:DEX account", "verify-email", "confirm this address"),
        _ => ("Reset your RE:DEX password", "reset-password", "choose a new password (if you did not ask for this, ignore this mail)"),
    };
    let body = format!(
        "To {what}, open {}/{path}#token={token}\r\nor enter this code in RE:DEX:\r\n\r\n{token}\r\n\r\nThe code works once and expires in {} minutes.{}",
        app.cfg.public_url.trim_end_matches('/'),
        ttl / 60,
        if purpose == "verify" { format!("\r\n\r\nAn account whose address is not confirmed within {UNVERIFIED_DAYS} days is deleted, with its characters.") } else { String::new() }
    );
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = app.mail.send(&to, subject, body).await {
            tracing::error!(error = %e, account, purpose, "mail not sent");
        }
    });
    Ok(())
}

#[derive(Deserialize)]
struct Register {
    email: String,
    username: String,
    password: String,
    trainer_name: String,
}

async fn register(State(app): State<Shared>, ClientIp(ip): ClientIp, Json(r): Json<Register>) -> ApiResult<Response> {
    let (email, norm) = check_email(&r.email)?;
    let (username, user_norm) = check_username(&r.username)?;
    check_new_password(&r.password)?;
    let name = trainer_name(&r.trainer_name)?;
    limit_ip(&app.db, "register", &ip, app.cfg.limits.register_ip).await?;
    // a taken username is told first, whatever the address: the answer for the address never depends on it
    if username_taken(&app.db, &user_norm).await? {
        return Err(ApiError::Conflict("this username is taken"));
    }
    let hash = hash_password(&app, r.password).await?;
    let mut tx = app.db.begin().await?;
    // the unique indexes (address, username, public id) decide between registrations racing each other
    let mut id: Option<i64> = None;
    for _ in 0..3 {
        id = sqlx::query_scalar(
            "INSERT INTO accounts (email, email_norm, username, username_norm, password_hash, public_id) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(&email)
        .bind(&norm)
        .bind(&username)
        .bind(&user_norm)
        .bind(&hash)
        .bind(new_public_id()?)
        .fetch_optional(&mut *tx)
        .await?;
        let other = sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM accounts WHERE username_norm = $1 OR email_norm = $2)").bind(&user_norm).bind(&norm).fetch_one(&mut *tx).await?;
        // a conflict on neither: the random public id was taken (one in 2^62); try another
        if id.is_some() || other {
            break;
        }
    }
    // a taken address gets the same answer as a new account (whether an address has an account is not told, as for a
    // password reset); its owner gets a notice instead. A taken username is told: it is public anyway.
    let accepted = || (StatusCode::ACCEPTED, Json(json!({ "status": "check your mail to confirm the address" }))).into_response();
    let Some(id) = id else {
        drop(tx);
        if username_taken(&app.db, &user_norm).await? {
            return Err(ApiError::Conflict("this username is taken"));
        }
        let owner: Option<(i64, String)> = sqlx::query_as("SELECT id, email FROM accounts WHERE email_norm = $1").bind(&norm).fetch_optional(&app.db).await?;
        let Some((owner, to)) = owner else { return Err(ApiError::Conflict("this username is taken")) };
        if hit(&app.db, &format!("mail:account:{owner}"), app.cfg.limits.mail_account).await? {
            audit(&app.db, Some(owner), "register_existing_address", &ip, json!({})).await;
            let app = app.clone();
            tokio::spawn(async move {
                let body = "Someone tried to make a new RE:DEX account with this address. It already has an account, so nothing changed.\r\n\r\nIf it was you, log in instead (or reset the password). If not, you need not do anything.".to_string();
                if let Err(e) = app.mail.send(&to, "Your RE:DEX address", body).await {
                    tracing::error!(error = %e, account = owner, "mail not sent");
                }
            });
        }
        return Ok(accepted());
    };
    sqlx::query("INSERT INTO account_settings (account_id) VALUES ($1)").bind(id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO trainer_profiles (account_id, trainer_name) VALUES ($1, $2)").bind(id).bind(&name).execute(&mut *tx).await?;
    tx.commit().await?;
    audit(&app.db, Some(id), "register", &ip, json!({ "username": username })).await;
    send_mail_token(&app, id, email, "verify").await?;
    Ok(accepted())
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

/// Marks a mail token used if it is live and of `purpose`; its account.
async fn take_mail_token(db: &mut sqlx::PgConnection, token: &str, purpose: &str) -> ApiResult<i64> {
    let id: Option<i64> = sqlx::query_scalar("UPDATE email_tokens SET used_at = now() WHERE token_hash = $1 AND purpose = $2 AND used_at IS NULL AND expires_at > now() RETURNING account_id")
        .bind(token_hash(token.trim()))
        .bind(purpose)
        .fetch_optional(db)
        .await?;
    id.ok_or(ApiError::Bad("this code is wrong, used or expired"))
}

async fn verify_email(State(app): State<Shared>, ClientIp(ip): ClientIp, Json(b): Json<TokenBody>) -> ApiResult<Json<Value>> {
    limit_ip(&app.db, "token", &ip, app.cfg.limits.login_ip).await?;
    let mut tx = app.db.begin().await?;
    let id = take_mail_token(&mut tx, &b.token, "verify").await?;
    sqlx::query("UPDATE accounts SET email_verified_at = coalesce(email_verified_at, now()), updated_at = now() WHERE id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    audit(&app.db, Some(id), "email_verified", &ip, json!({})).await;
    Ok(Json(json!({ "account_id": id, "email_verified": true })))
}

async fn resend_verification(State(app): State<Shared>, ClientIp(ip): ClientIp, a: Authed) -> ApiResult<StatusCode> {
    let (email, verified): (String, bool) = sqlx::query_as("SELECT email, email_verified_at IS NOT NULL FROM accounts WHERE id = $1").bind(a.account).fetch_one(&app.db).await?;
    if verified {
        return Err(ApiError::Conflict("this address is already confirmed"));
    }
    limit_ip(&app.db, "mail", &ip, app.cfg.limits.mail_ip).await?;
    limit(&app.db, &format!("mail:account:{}", a.account), app.cfg.limits.mail_account).await?;
    send_mail_token(&app, a.account, email, "verify").await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct Login {
    /// The email address or the username.
    #[serde(alias = "username", alias = "login")]
    email: String,
    password: String,
    #[serde(default)]
    device: String,
}

async fn login(State(app): State<Shared>, ClientIp(ip): ClientIp, Json(l): Json<Login>) -> ApiResult<Json<Value>> {
    limit_ip(&app.db, "login", &ip, app.cfg.limits.login_ip).await?;
    let norm = l.email.trim().to_lowercase();
    if l.password.len() > PASSWORD_MAX_BYTES || norm.len() > 254 {
        return Err(ApiError::Unauthorized("wrong email or password"));
    }
    // an address has an '@' and a username never does, so one lookup finds either
    let row: Option<(i64, String, bool, bool)> = sqlx::query_as(
        "SELECT id, password_hash, email_verified_at IS NOT NULL, disabled_at IS NOT NULL FROM accounts WHERE email_norm = $1 OR username_norm = $1",
    )
    .bind(&norm)
    .fetch_optional(&app.db)
    .await?;
    // every attempt counts before its password is hashed (parallel guesses cannot all slip under the limit), per account
    // (by address or username alike, or the typed name when there is no such account) and address; a right password
    // gives it back
    let account = row.as_ref().map(|r| r.0);
    // two namespaces: a typed name that looks like a number never counts against that account id
    let who = account.map_or_else(|| format!("name:{norm}"), |id| format!("id:{id}"));
    let keys = count_attempt(&app, "login", &who, account, &ip).await?;
    let ok = check_password(&app, l.password, row.as_ref().map(|r| r.1.clone())).await?;
    let Some((id, _, verified, disabled)) = row.filter(|_| ok) else {
        audit(&app.db, account, "login_failed", &ip, json!({ "login": norm })).await;
        return Err(ApiError::Unauthorized("wrong email or password"));
    };
    for (k, l) in &keys {
        unhit(&app.db, k, *l).await?;
    }
    if disabled {
        return Err(ApiError::Forbidden("this account is disabled"));
    }
    let device = clean_text(&l.device, DEVICE_CHARS);
    let device = if device.is_empty() { "unnamed device".to_string() } else { device };
    let (token, hash) = new_token(SESSION_PREFIX)?;
    let (sid, expires): (i64, i64) = sqlx::query_as(
        "INSERT INTO sessions (account_id, token_hash, device, ip, expires_at) VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))
         RETURNING id, extract(epoch from expires_at)::bigint",
    )
    .bind(id)
    .bind(hash)
    .bind(&device)
    .bind(&ip)
    .bind(app.cfg.session_ttl_secs as f64)
    .fetch_one(&app.db)
    .await?;
    audit(&app.db, Some(id), "login", &ip, json!({ "session": sid, "device": device })).await;
    Ok(Json(json!({ "token": token, "account_id": id, "session_id": sid, "expires_at": expires, "email_verified": verified })))
}

async fn logout(State(app): State<Shared>, ClientIp(ip): ClientIp, a: Authed) -> ApiResult<StatusCode> {
    sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL").bind(a.session).execute(&app.db).await?;
    audit(&app.db, Some(a.account), "logout", &ip, json!({ "session": a.session })).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn logout_all(State(app): State<Shared>, ClientIp(ip): ClientIp, a: Authed) -> ApiResult<Json<Value>> {
    let n = sqlx::query("UPDATE sessions SET revoked_at = now() WHERE account_id = $1 AND revoked_at IS NULL").bind(a.account).execute(&app.db).await?.rows_affected();
    forget_addresses(&app.db, a.account, None).await?;
    audit(&app.db, Some(a.account), "logout_all", &ip, json!({ "sessions": n })).await;
    Ok(Json(json!({ "revoked": n })))
}

async fn sessions(State(app): State<Shared>, a: Authed) -> ApiResult<Json<Value>> {
    let rows: Vec<(i64, String, Option<String>, i64, i64, i64)> = sqlx::query_as(
        "SELECT id, device, ip, extract(epoch from created_at)::bigint, extract(epoch from last_seen_at)::bigint, extract(epoch from expires_at)::bigint
         FROM sessions WHERE account_id = $1 AND revoked_at IS NULL AND expires_at > now() ORDER BY last_seen_at DESC",
    )
    .bind(a.account)
    .fetch_all(&app.db)
    .await?;
    let list: Vec<Value> = rows
        .into_iter()
        .map(|(id, device, ip, created, seen, expires)| json!({ "id": id, "device": device, "ip": ip, "created_at": created, "last_seen_at": seen, "expires_at": expires, "current": id == a.session }))
        .collect();
    Ok(Json(json!({ "sessions": list })))
}

async fn revoke_session(State(app): State<Shared>, ClientIp(ip): ClientIp, a: Authed, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    // only the account's own sessions: another account's id is "not found", the same as no such session
    let n = sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1 AND account_id = $2 AND revoked_at IS NULL").bind(id).bind(a.account).execute(&app.db).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound);
    }
    audit(&app.db, Some(a.account), "session_revoked", &ip, json!({ "session": id })).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Forgot {
    email: String,
}

/// Always 202: whether the address has an account is not told.
async fn forgot_password(State(app): State<Shared>, ClientIp(ip): ClientIp, Json(f): Json<Forgot>) -> ApiResult<StatusCode> {
    limit_ip(&app.db, "mail", &ip, app.cfg.limits.mail_ip).await?;
    let norm = f.email.trim().to_lowercase();
    let row: Option<(i64, String)> = sqlx::query_as("SELECT id, email FROM accounts WHERE email_norm = $1 AND disabled_at IS NULL").bind(&norm).fetch_optional(&app.db).await?;
    if let Some((id, email)) = row {
        // over the per-address limit nothing more is sent, and the answer stays the same
        if hit(&app.db, &format!("mail:account:{id}"), app.cfg.limits.mail_account).await? {
            send_mail_token(&app, id, email, "reset").await?;
            audit(&app.db, Some(id), "password_reset_requested", &ip, json!({})).await;
        }
    }
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct Reset {
    token: String,
    password: String,
}

async fn reset_password(State(app): State<Shared>, ClientIp(ip): ClientIp, Json(r): Json<Reset>) -> ApiResult<Json<Value>> {
    limit_ip(&app.db, "token", &ip, app.cfg.limits.login_ip).await?;
    check_new_password(&r.password)?;
    // a code that cannot work costs no hashing (it is taken for good, conditionally, below)
    let live: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM email_tokens WHERE token_hash = $1 AND purpose = 'reset' AND used_at IS NULL AND expires_at > now())")
        .bind(token_hash(r.token.trim()))
        .fetch_one(&app.db)
        .await?;
    if !live {
        return Err(ApiError::Bad("this code is wrong, used or expired"));
    }
    let hash = hash_password(&app, r.password).await?;
    let mut tx = app.db.begin().await?;
    let id = take_mail_token(&mut tx, &r.token, "reset").await?;
    // a reset mail proves the address too; every session (and so every ticket) ends
    sqlx::query("UPDATE accounts SET password_hash = $2, email_verified_at = coalesce(email_verified_at, now()), updated_at = now() WHERE id = $1").bind(id).bind(hash).execute(&mut *tx).await?;
    let n = sqlx::query("UPDATE sessions SET revoked_at = now() WHERE account_id = $1 AND revoked_at IS NULL").bind(id).execute(&mut *tx).await?.rows_affected();
    forget_addresses(&mut *tx, id, None).await?;
    sqlx::query("UPDATE email_tokens SET used_at = now() WHERE account_id = $1 AND purpose = 'reset' AND used_at IS NULL").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    audit(&app.db, Some(id), "password_reset", &ip, json!({ "sessions_revoked": n })).await;
    Ok(Json(json!({ "account_id": id })))
}

#[derive(Deserialize)]
struct Change {
    current_password: String,
    new_password: String,
}

async fn change_password(State(app): State<Shared>, ClientIp(ip): ClientIp, a: Authed, Json(c): Json<Change>) -> ApiResult<Json<Value>> {
    check_new_password(&c.new_password)?;
    // counted before hashing, given back when right (as for logins)
    let keys = count_attempt(&app, "password", &format!("id:{}", a.account), Some(a.account), &ip).await?;
    let old: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id = $1").bind(a.account).fetch_one(&app.db).await?;
    if !check_password(&app, c.current_password, Some(old)).await? {
        return Err(ApiError::Unauthorized("the current password is wrong"));
    }
    for (k, l) in &keys {
        unhit(&app.db, k, *l).await?;
    }
    let hash = hash_password(&app, c.new_password).await?;
    let mut tx = app.db.begin().await?;
    sqlx::query("UPDATE accounts SET password_hash = $2, updated_at = now() WHERE id = $1").bind(a.account).bind(hash).execute(&mut *tx).await?;
    // a reset code mailed before the change must not undo it
    sqlx::query("UPDATE email_tokens SET used_at = now() WHERE account_id = $1 AND purpose = 'reset' AND used_at IS NULL").bind(a.account).execute(&mut *tx).await?;
    // the other devices log in again; this one stays
    let n = sqlx::query("UPDATE sessions SET revoked_at = now() WHERE account_id = $1 AND id <> $2 AND revoked_at IS NULL").bind(a.account).bind(a.session).execute(&mut *tx).await?.rows_affected();
    forget_addresses(&mut *tx, a.account, Some(a.session)).await?;
    tx.commit().await?;
    audit(&app.db, Some(a.account), "password_changed", &ip, json!({ "sessions_revoked": n })).await;
    Ok(Json(json!({ "sessions_revoked": n })))
}

async fn account(State(app): State<Shared>, a: Authed) -> ApiResult<Json<Value>> {
    let (email, username, public_id, verified, created, language, show_online, allow_whispers, name): (String, String, i64, bool, i64, String, bool, bool, String) = sqlx::query_as(
        "SELECT a.email, a.username, a.public_id, a.email_verified_at IS NOT NULL, extract(epoch from a.created_at)::bigint, s.language, s.show_online, s.allow_whispers, p.trainer_name
         FROM accounts a JOIN account_settings s ON s.account_id = a.id JOIN trainer_profiles p ON p.account_id = a.id WHERE a.id = $1",
    )
    .bind(a.account)
    .fetch_one(&app.db)
    .await?;
    Ok(Json(json!({
        "account_id": a.account, "public_id": public_id, "email": email, "username": username, "email_verified": verified, "created_at": created,
        "settings": { "language": language, "show_online": show_online, "allow_whispers": allow_whispers },
        "profile": { "trainer_name": name },
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    language: Option<String>,
    show_online: Option<bool>,
    allow_whispers: Option<bool>,
}

async fn update_settings(State(app): State<Shared>, ClientIp(ip): ClientIp, a: Authed, Json(s): Json<Settings>) -> ApiResult<Json<Value>> {
    if let Some(l) = &s.language {
        // a BCP 47-ish tag: letters, digits and '-', 2 to 16 characters
        if !(2..=16).contains(&l.len()) || !l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(ApiError::Bad("language must be a language tag such as en or fr-CA"));
        }
    }
    let (language, show_online, allow_whispers): (String, bool, bool) = sqlx::query_as(
        "UPDATE account_settings SET language = coalesce($2, language), show_online = coalesce($3, show_online), allow_whispers = coalesce($4, allow_whispers), updated_at = now()
         WHERE account_id = $1 RETURNING language, show_online, allow_whispers",
    )
    .bind(a.account)
    .bind(&s.language)
    .bind(s.show_online)
    .bind(s.allow_whispers)
    .fetch_one(&app.db)
    .await?;
    audit(&app.db, Some(a.account), "settings_changed", &ip, json!({})).await;
    Ok(Json(json!({ "language": language, "show_online": show_online, "allow_whispers": allow_whispers })))
}

async fn profile(State(app): State<Shared>, a: Authed) -> ApiResult<Json<Value>> {
    let (name, created, updated): (String, i64, i64) =
        sqlx::query_as("SELECT trainer_name, extract(epoch from created_at)::bigint, extract(epoch from updated_at)::bigint FROM trainer_profiles WHERE account_id = $1")
            .bind(a.account)
            .fetch_one(&app.db)
            .await?;
    Ok(Json(json!({ "account_id": a.account, "trainer_name": name, "created_at": created, "updated_at": updated })))
}

#[derive(Deserialize)]
struct Profile {
    trainer_name: String,
}

async fn put_profile(State(app): State<Shared>, ClientIp(ip): ClientIp, a: Authed, Json(p): Json<Profile>) -> ApiResult<Json<Value>> {
    let name = trainer_name(&p.trainer_name)?;
    sqlx::query("UPDATE trainer_profiles SET trainer_name = $2, updated_at = now() WHERE account_id = $1").bind(a.account).bind(&name).execute(&app.db).await?;
    audit(&app.db, Some(a.account), "profile_changed", &ip, json!({ "trainer_name": name })).await;
    Ok(Json(json!({ "account_id": a.account, "trainer_name": name })))
}

#[derive(Deserialize)]
struct TicketRequest {
    world: String,
}

/// A one-use ticket for joining `world`, valid for `ticket_ttl_secs`. Needs a confirmed address.
async fn issue_ticket(State(app): State<Shared>, a: Authed, Json(t): Json<TicketRequest>) -> ApiResult<Json<Value>> {
    // host names are not case-sensitive: world ids are compared lower-cased on both sides
    let world = &t.world.trim().to_lowercase();
    if world.is_empty() || world.len() > WORLD_CHARS {
        return Err(ApiError::Bad("world must be 1 to 64 characters"));
    }
    let verified: bool = sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM accounts WHERE id = $1").bind(a.account).fetch_one(&app.db).await?;
    if !verified {
        return Err(ApiError::Forbidden("confirm your email address before playing online"));
    }
    limit(&app.db, &format!("ticket:account:{}", a.account), app.cfg.limits.tickets).await?;
    let (ticket, hash) = new_token(TICKET_PREFIX)?;
    sqlx::query("INSERT INTO world_tickets (ticket_hash, account_id, session_id, world, expires_at) VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))")
        .bind(hash)
        .bind(a.account)
        .bind(a.session)
        .bind(world)
        .bind(app.cfg.ticket_ttl_secs as f64)
        .execute(&app.db)
        .await?;
    Ok(Json(json!({ "ticket": ticket, "account_id": a.account, "expires_in": app.cfg.ticket_ttl_secs })))
}

async fn characters(State(app): State<Shared>, a: Authed) -> ApiResult<Json<Value>> {
    let mut c = app.db.acquire().await?;
    Ok(characters_json(&load_characters(&mut c, a.account).await?))
}

/// The newest `changed` wins (an older copy changes nothing); a new character only while the account has fewer than
/// `MAX_CHARACTERS`. The account's list after it.
async fn put_character(State(app): State<Shared>, a: Authed, Path(id): Path<String>, Json(b): Json<CharacterBody>) -> ApiResult<Json<Value>> {
    let c = check_character(&id, b)?;
    Ok(characters_json(&merge_character(&app.db, a.account, c).await?))
}

async fn delete_character(State(app): State<Shared>, ClientIp(ip): ClientIp, a: Authed, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let id = u64::from_str_radix(&id, 16).map_err(|_| ApiError::Bad("character id must be 16 hex digits"))?;
    let n = sqlx::query("DELETE FROM account_characters WHERE account_id = $1 AND character_id = $2").bind(a.account).bind(id as i64).execute(&app.db).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound);
    }
    audit(&app.db, Some(a.account), "character_deleted", &ip, json!({ "character": format!("{id:016x}") })).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The account a world names by its public id.
async fn by_public_id(db: &PgPool, public: i64) -> ApiResult<i64> {
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE public_id = $1").bind(public).fetch_optional(db).await?;
    id.ok_or(ApiError::NotFound)
}

/// World servers only: an account's characters (the central world sends them after `Welcome`), by its public id.
async fn world_characters(State(app): State<Shared>, ClientIp(ip): ClientIp, headers: HeaderMap, Path(public): Path<i64>) -> ApiResult<Json<Value>> {
    world_only(&app, &headers, &ip)?;
    let account = by_public_id(&app.db, public).await?;
    let mut c = app.db.acquire().await?;
    Ok(characters_json(&load_characters(&mut c, account).await?))
}

/// World servers only: a player's `CharacterPut`, which the world checked already; checked again here.
async fn world_put_character(State(app): State<Shared>, ClientIp(ip): ClientIp, headers: HeaderMap, Path(public): Path<i64>, Json(b): Json<Value>) -> ApiResult<Json<Value>> {
    world_only(&app, &headers, &ip)?;
    let id = b["id"].as_str().unwrap_or_default().to_string();
    let body: CharacterBody = serde_json::from_value(b).map_err(|_| ApiError::Bad("not a character"))?;
    let c = check_character(&id, body)?;
    let account = by_public_id(&app.db, public).await?;
    Ok(characters_json(&merge_character(&app.db, account, c).await?))
}

/// Sessions a world asks about at once, at most.
const CHECK_MAX: usize = 1000;

#[derive(Deserialize)]
struct CheckSessions {
    sessions: Vec<i64>,
}

/// World servers only: which of these sessions are dead (logged out, logged out everywhere, ended by a password
/// change or reset, expired, or their account disabled). One query for a world's whole list.
async fn check_sessions(State(app): State<Shared>, ClientIp(ip): ClientIp, headers: HeaderMap, Json(c): Json<CheckSessions>) -> ApiResult<Json<Value>> {
    world_only(&app, &headers, &ip)?;
    if c.sessions.len() > CHECK_MAX {
        return Err(ApiError::Bad("at most 1000 sessions at once"));
    }
    let dead: Vec<i64> = sqlx::query_scalar(
        "SELECT q.id FROM unnest($1::bigint[]) AS q(id) WHERE NOT EXISTS (
             SELECT 1 FROM sessions s JOIN accounts a ON a.id = s.account_id
             WHERE s.id = q.id AND s.revoked_at IS NULL AND s.expires_at > now() AND a.disabled_at IS NULL)",
    )
    .bind(&c.sessions)
    .fetch_all(&app.db)
    .await?;
    Ok(Json(json!({ "dead": dead })))
}

#[derive(Deserialize)]
struct VerifyTicket {
    ticket: String,
    world: String,
}

/// World servers only (the shared secret): uses the ticket up. 200 with the account, or 401 with why (`reason`:
/// `unknown`, `used`, `expired`, `world`, `session`).
async fn verify_ticket(State(app): State<Shared>, ClientIp(ip): ClientIp, headers: HeaderMap, Json(v): Json<VerifyTicket>) -> ApiResult<Response> {
    world_only(&app, &headers, &ip)?;
    let hash = token_hash(v.ticket.trim());
    let world = v.world.trim().to_lowercase();
    // the row lock makes this one-use even when two worlds verify the same ticket at once
    let row: Option<(i64, i64, i64, String)> = sqlx::query_as(
        "UPDATE world_tickets t SET used_at = now() FROM sessions s, accounts a, trainer_profiles p
         WHERE t.ticket_hash = $1 AND t.world = $2 AND t.used_at IS NULL AND t.expires_at > now()
           AND s.id = t.session_id AND s.revoked_at IS NULL AND s.expires_at > now()
           AND a.id = t.account_id AND a.disabled_at IS NULL AND p.account_id = a.id
         RETURNING t.account_id, a.public_id, t.session_id, p.trainer_name",
    )
    .bind(&hash)
    .bind(&world)
    .fetch_optional(&app.db)
    .await?;
    if let Some((account, public, session, name)) = row {
        audit(&app.db, Some(account), "world_login", &ip, json!({ "world": world, "session": session })).await;
        // the world asks about this session again every 30 s (`check_sessions`) and drops the player once it is dead
        // the world knows the account by its public id only (it is what other players see)
        return Ok(Json(json!({ "public_id": public, "session_id": session, "trainer_name": name })).into_response());
    }
    let why: Option<(bool, bool, bool)> = sqlx::query_as("SELECT used_at IS NOT NULL, expires_at <= now(), world <> $2 FROM world_tickets WHERE ticket_hash = $1")
        .bind(&hash)
        .bind(&world)
        .fetch_optional(&app.db)
        .await?;
    let reason = match why {
        None => "unknown",
        Some((true, _, _)) => "used",
        Some((_, true, _)) => "expired",
        Some((_, _, true)) => "world",
        Some(_) => "session",
    };
    tracing::info!(reason, "world ticket refused");
    Ok((StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized", "reason": reason }))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_checked_and_folded() {
        assert_eq!(check_email("  Ash@Example.COM ").unwrap(), ("Ash@Example.COM".into(), "ash@example.com".into()));
        for bad in ["", "ash", "@x.com", "ash@", "ash@x", "a b@x.com", "ash@x.com.", "a@b@c.com", "ash@.com"] {
            assert!(check_email(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ipv6_addresses_count_by_their_64() {
        assert_eq!(bucket("2001:db8:1:2:aaaa::1"), bucket("2001:db8:1:2:ffff:ffff:ffff:ffff"));
        assert_eq!(bucket("2001:db8:1:2::1"), "2001:db8:1:2::/64");
        assert_ne!(bucket("2001:db8:1:2::1"), bucket("2001:db8:1:3::1"));
        assert_eq!(bucket("203.0.113.7"), "203.0.113.7");
        assert_eq!(bucket("::ffff:203.0.113.7"), "203.0.113.7");
        assert_eq!(bucket("unknown"), "unknown");
    }

    #[test]
    fn usernames_are_short_ascii_and_never_reserved() {
        assert_eq!(check_username(" Ash_99 ").unwrap(), ("Ash_99".into(), "ash_99".into()));
        assert!(check_username("a.b-c").is_ok());
        for bad in ["ab", "x".repeat(17).as_str(), "_ash", "a sh", "\u{e9}clair", "Admin", "M0derat0r", "re-dex", "Game.Freak"] {
            assert!(check_username(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn characters_are_checked_like_the_world_checks_them() {
        let body = |look: &str, changed: u64| CharacterBody { name: "Serena\u{202E}".into(), look: look.into(), created: 1, changed };
        let girl = "060101c00024024c0590020004000000000000";
        let c = check_character("00000000000000ff", body(girl, 5)).unwrap();
        assert_eq!((c.id, c.name.as_str(), c.look.len()), (255, "Serena", 19));
        assert!(check_character("0000000000000000", body(girl, 5)).is_err(), "id 0");
        assert!(check_character("ff", body(girl, 5)).is_err(), "not 16 digits");
        assert!(check_character("00000000000000ff", body("0601", 5)).is_err(), "not a whole look");
        assert!(check_character("00000000000000ff", body(girl, u64::MAX)).is_err(), "changed far ahead");
        assert_eq!(character_json(&c)["look"], json!(girl));
    }

    #[test]
    fn passwords_need_ten_characters_and_stay_bounded() {
        assert!(check_new_password("short").is_err());
        assert!(check_new_password("long enough").is_ok());
        assert!(check_new_password(&"x".repeat(PASSWORD_MAX_BYTES + 1)).is_err());
    }

    #[test]
    fn names_lose_invisible_characters() {
        assert_eq!(trainer_name(" A\u{202E}sh\u{200B} ").unwrap(), "Ash");
        assert!(trainer_name("\u{2066}\u{2069}").is_err());
        assert_eq!(trainer_name(&"x".repeat(40)).unwrap().len(), TRAINER_NAME_CHARS);
    }

    #[test]
    fn tokens_are_random_and_only_their_hash_is_kept() {
        let (a, ha) = new_token(SESSION_PREFIX).unwrap();
        let (b, _) = new_token(SESSION_PREFIX).unwrap();
        assert!(a.starts_with(SESSION_PREFIX) && a.len() == SESSION_PREFIX.len() + 43);
        assert_ne!(a, b);
        assert_eq!(ha, token_hash(&a));
        assert!(same_secret("x", "x") && !same_secret("x", "y"));
    }
}
