//! An account of the account backend (`redex-backend`): sign in with an address or username and a password for a
//! session token, then one short-lived world ticket per connection (`Auth::WorldTicket`). The password is used once and
//! never kept or logged. The session token is kept between runs only encrypted for this Windows user (DPAPI,
//! `save_session`), so later launches sign in with it alone.
use redex_proto::Character;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The official central server's account service and world (`tls:host:port`), one host name for both. The settings can
/// name others (tests).
pub const OFFICIAL_ACCOUNTS: &str = "https://playredex.mooo.com";
pub const OFFICIAL_WORLD: &str = "tls:playredex.mooo.com:7777";

#[derive(Clone)]
pub struct Account {
    /// The backend's base URL (`https://accounts.example`).
    pub server: String,
    /// The session token: in memory, and on disk only through `save_session`.
    token: String,
    pub account_id: u64,
    /// What other players see, and the name of this account's save root on this PC (`online_root`)
    pub public_id: u64,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("server", &self.server).field("public_id", &self.public_id).finish_non_exhaustive()
    }
}

/// Why a call to the account service did not work.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// No answer: refused, timed out, no such host, TLS failed, or the proxy in front of it could not reach it (502,
    /// 504). The official server is down for us: the start screen offers P2P instead.
    Unreachable(String),
    /// The session token is no longer good (expired, logged out, revoked, account disabled): signed out.
    SignedOut(String),
    /// Any other answer: its HTTP status (0: refused before sending) and the service's own message.
    Refused { status: u16, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unreachable(e) => write!(f, "cannot reach the account service: {e}"),
            Error::SignedOut(m) | Error::Refused { message: m, .. } => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

/// The signed-in account as `GET /v1/account` tells it.
#[derive(Debug, Clone, PartialEq)]
pub struct Me {
    pub account_id: u64,
    pub public_id: u64,
    pub username: String,
    pub email_verified: bool,
}

/// The client for `server`: no redirects (a password or token must not follow one to another host), and for a service
/// on this PC no proxy either (`HTTP_PROXY` would get the password of a plain-http dev backend in clear)
fn agent(server: &str) -> ureq::Agent {
    let c = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).http_status_as_error(false).max_redirects(0);
    let c = if server.starts_with("http://") { c.proxy(None) } else { c };
    c.build().into()
}

/// Text a server sent, as the screen shows it: control characters dropped, at most 200 characters.
pub fn said(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(200).collect();
    if s.chars().filter(|c| !c.is_control()).count() > 200 { t + "…" } else { t }
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

/// The base URL trimmed, or why it may not be used.
fn base(server: &str) -> Result<String, Error> {
    let server = server.trim().trim_end_matches('/').to_string();
    if !secure(&server) {
        return Err(Error::Refused { status: 0, message: format!("the account service must be https:// (plain http only on this PC): {server}") });
    }
    Ok(server)
}

/// An answer as (status, JSON body); a request that got none is `Unreachable`, and so is a proxy's "upstream down".
fn send(r: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<(u16, serde_json::Value), Error> {
    let mut r = r.map_err(|e| Error::Unreachable(e.to_string()))?;
    let status = r.status().as_u16();
    if matches!(status, 502 | 504) {
        return Err(Error::Unreachable(format!("the account service's proxy answered {status}")));
    }
    Ok((status, r.body_mut().read_json().unwrap_or_default()))
}

/// The `message` of an error answer (else its status), as `Refused`; a 401 to a session call is `SignedOut`.
fn refused(status: u16, v: &serde_json::Value, session: bool) -> Error {
    let message = v["message"].as_str().map_or_else(|| format!("the account service answered {status}"), said);
    if session && status == 401 {
        Error::SignedOut(message)
    } else {
        Error::Refused { status, message }
    }
}

fn character(v: &serde_json::Value) -> Option<Character> {
    let id = v["id"].as_str().filter(|s| s.len() == 16).and_then(|s| u64::from_str_radix(s, 16).ok())?;
    let hex = v["look"].as_str().filter(|s| s.len() % 2 == 0 && s.is_ascii())?;
    let look = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect::<Option<Vec<u8>>>()?;
    Some(Character { id, name: v["name"].as_str()?.to_string(), look, created: v["created"].as_u64()?, changed: v["changed"].as_u64()? })
}

fn characters(v: &serde_json::Value) -> Vec<Character> {
    v["characters"].as_array().map_or_else(Vec::new, |l| l.iter().filter_map(character).collect())
}

impl Account {
    /// Signs in with an address or a username; `device` names this PC in the account's session list.
    pub fn login(server: &str, login: &str, password: &str, device: &str) -> Result<Account, Error> {
        let server = base(server)?;
        let agent = agent(&server);
        let body = serde_json::json!({ "email": login, "password": password, "device": device });
        let (status, v) = send(agent.post(&format!("{server}/v1/login")).send_json(&body))?;
        if status != 200 {
            return Err(refused(status, &v, false));
        }
        let Some(token) = v["token"].as_str() else {
            return Err(Error::Refused { status, message: "the account service gave a broken answer".into() });
        };
        let mut a = Account { server, token: token.into(), account_id: 0, public_id: 0, agent };
        let me = a.me()?;
        (a.account_id, a.public_id) = (me.account_id, me.public_id);
        Ok(a)
    }

    /// A saved session (`saved_session`) checked with the service: the account and who it is. A token the service no
    /// longer takes is `SignedOut`.
    pub fn resume(server: &str, token: &str) -> Result<(Account, Me), Error> {
        let server = base(server)?;
        let mut a = Account { agent: agent(&server), server, token: token.to_string(), account_id: 0, public_id: 0 };
        let me = a.me()?;
        (a.account_id, a.public_id) = (me.account_id, me.public_id);
        Ok((a, me))
    }

    fn get(&self, path: &str) -> Result<serde_json::Value, Error> {
        let (status, v) = send(self.agent.get(&format!("{}{path}", self.server)).header("Authorization", &format!("Bearer {}", self.token)).call())?;
        if status == 200 { Ok(v) } else { Err(refused(status, &v, true)) }
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<(u16, serde_json::Value), Error> {
        send(self.agent.post(&format!("{}{path}", self.server)).header("Authorization", &format!("Bearer {}", self.token)).send_json(&body))
    }

    /// Who this session is.
    pub fn me(&self) -> Result<Me, Error> {
        let v = self.get("/v1/account")?;
        let (Some(account_id), Some(public_id), Some(username)) = (v["account_id"].as_u64(), v["public_id"].as_u64(), v["username"].as_str()) else {
            return Err(Error::Refused { status: 200, message: "the account service gave a broken answer".into() });
        };
        Ok(Me { account_id, public_id, username: said(username), email_verified: v["email_verified"].as_bool() == Some(true) })
    }

    /// A fresh one-use ticket for `world` (the `host:port` of a `tls:` address).
    pub fn ticket(&self, world: &str) -> Result<String, Error> {
        let (status, v) = self.post("/v1/world-tickets", serde_json::json!({ "world": world }))?;
        match (status, v["ticket"].as_str()) {
            (200, Some(t)) => Ok(t.into()),
            _ => Err(refused(status, &v, true)),
        }
    }

    /// The account's characters (the online ones).
    pub fn characters(&self) -> Result<Vec<Character>, Error> {
        self.get("/v1/characters").map(|v| characters(&v))
    }

    /// Puts a character on the account (the newest `changed` wins there); the account's list after it.
    pub fn put_character(&self, c: &Character) -> Result<Vec<Character>, Error> {
        let look: String = c.look.iter().map(|b| format!("{b:02x}")).collect();
        let body = serde_json::json!({ "name": c.name, "look": look, "created": c.created, "changed": c.changed });
        let r = self.agent.put(&format!("{}/v1/characters/{:016x}", self.server, c.id)).header("Authorization", &format!("Bearer {}", self.token)).send_json(&body);
        let (status, v) = send(r)?;
        if status == 200 { Ok(characters(&v)) } else { Err(refused(status, &v, true)) }
    }

    /// Ends this session on the service.
    pub fn logout(&self) -> Result<(), Error> {
        let (status, v) = self.post("/v1/logout", serde_json::json!({}))?;
        if status == 204 || status == 401 { Ok(()) } else { Err(refused(status, &v, true)) }
    }
}

/// Makes an account; the service's own words on success (it mails a confirmation code the address must send back
/// before the account can play online).
pub fn register(server: &str, email: &str, username: &str, password: &str, trainer_name: &str) -> Result<String, Error> {
    let server = base(server)?;
    let body = serde_json::json!({ "email": email, "username": username, "password": password, "trainer_name": trainer_name });
    let (status, v) = send(agent(&server).post(&format!("{server}/v1/register")).send_json(&body))?;
    match status {
        200 | 202 => Ok(said(v["status"].as_str().unwrap_or("account made"))),
        _ => Err(refused(status, &v, false)),
    }
}

/// Asks for a password reset mail. The service answers the same whether the address has an account or not.
pub fn forgot_password(server: &str, email: &str) -> Result<(), Error> {
    let server = base(server)?;
    let (status, v) = send(agent(&server).post(&format!("{server}/v1/password/forgot")).send_json(&serde_json::json!({ "email": email })))?;
    if matches!(status, 200 | 202) { Ok(()) } else { Err(refused(status, &v, false)) }
}

/// Whether the service answers at all (`GET /healthz`).
pub fn health(server: &str) -> Result<(), Error> {
    let server = base(server)?;
    let (status, v) = send(agent(&server).get(&format!("{server}/healthz")).call())?;
    if status == 200 { Ok(()) } else { Err(refused(status, &v, false)) }
}

/// Where the encrypted session lives in a data folder.
pub fn session_file(data: &Path) -> PathBuf {
    data.join("account").join("session.dpapi")
}

/// Keeps the session for later launches: the server and token, encrypted for this Windows user (DPAPI); whole or not
/// at all (a temporary file, then a rename).
pub fn save_session(data: &Path, a: &Account) -> Result<(), String> {
    let mut plain = serde_json::json!({ "server": a.server, "token": a.token }).to_string().into_bytes();
    let sealed = dpapi::protect(&plain);
    // the clear copy is wiped before it is freed
    plain.fill(0);
    let sealed = sealed?;
    let file = session_file(data);
    let dir = file.parent().ok_or("no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("session.new-{}", std::process::id()));
    std::fs::write(&tmp, sealed).and_then(|()| std::fs::rename(&tmp, &file)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// The kept session's (server, token), None when there is none or it cannot be opened (another user's, broken). A
/// session kept for another server is not given out: its token is for that server only.
pub fn saved_session(data: &Path, server: &str) -> Option<String> {
    let sealed = std::fs::read(session_file(data)).ok()?;
    let mut plain = dpapi::unprotect(&sealed).inspect_err(|e| log::warn!("account: the kept session cannot be opened: {e}")).ok()?;
    let v: Option<serde_json::Value> = serde_json::from_slice(&plain).ok();
    plain.fill(0);
    let v = v?;
    (v["server"].as_str()? == server.trim().trim_end_matches('/')).then(|| v["token"].as_str().map(String::from)).flatten()
}

/// Forgets the kept session (signed out on this PC).
pub fn forget_session(data: &Path) {
    let _ = std::fs::remove_file(session_file(data));
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

    /// Ties the blob to RE:DEX's sessions: another program's DPAPI blob does not open as one. Any process of the same
    /// Windows user can still open it (user-scope DPAPI), which is the protection asked for here.
    pub const ENTROPY: &[u8] = b"RE:DEX account session v1";

    fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: b.len() as u32, pbData: b.as_ptr() as *mut u8 }
    }

    pub fn run(input: &[u8], seal: bool, entropy: &[u8]) -> Result<Vec<u8>, String> {
        let (i, e) = (blob(input), blob(entropy));
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // safe: the blobs point at live slices for the call; `out` is LocalAlloc'd by the call and freed below
        let ok = unsafe {
            if seal {
                CryptProtectData(&i, std::ptr::null(), &e, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&i, std::ptr::null_mut(), &e, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            }
        };
        if ok == 0 {
            return Err(format!("DPAPI failed: {}", std::io::Error::last_os_error()));
        }
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        // the system's copy may be the clear token: wiped before it is freed
        unsafe {
            std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
            LocalFree(out.pbData as _)
        };
        Ok(v)
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        run(plain, true, ENTROPY)
    }

    pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, String> {
        run(sealed, false, ENTROPY)
    }
}

#[cfg(not(windows))]
mod dpapi {
    // shortcut: no OS secret store off Windows, so the session lasts one run there; use the platform keyring when RE:DEX ships elsewhere
    pub fn protect(_: &[u8]) -> Result<Vec<u8>, String> {
        Err("no secure store on this system".into())
    }

    pub fn unprotect(_: &[u8]) -> Result<Vec<u8>, String> {
        Err("no secure store on this system".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_go_over_tls_or_stay_on_this_pc() {
        for ok in ["https://accounts.example.org", "http://127.0.0.1:8080", "http://localhost:8080/", "http://[::1]:9"] {
            assert!(super::secure(ok), "{ok}");
        }
        for bad in ["http://accounts.example.org", "http://10.0.0.2:8080", "ftp://x", "accounts.example.org", "http://127.0.0.1.evil.org", "http://localhost:1@evil.example/", "http://127.0.0.1@evil.example"] {
            assert!(!super::secure(bad), "{bad}");
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redex-account-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[cfg(windows)]
    #[test]
    fn a_session_round_trips_through_dpapi_and_is_not_on_disk_in_the_clear() {
        let data = temp("dpapi");
        let token = "rdx_s_test-token-AbCdEf0123456789";
        let a = Account { server: "https://accounts.example.org".into(), token: token.into(), account_id: 0, public_id: 0, agent: agent("https://accounts.example.org") };
        save_session(&data, &a).unwrap();
        let bytes = std::fs::read(session_file(&data)).unwrap();
        assert!(!bytes.windows(token.len()).any(|w| w == token.as_bytes()), "the token is in the file as is");
        assert!(!bytes.windows(8).any(|w| w == b"accounts"), "the server is in the file as is");
        assert_eq!(saved_session(&data, "https://accounts.example.org/").as_deref(), Some(token));
        // a session kept for one server is never handed to another
        assert_eq!(saved_session(&data, "https://other.example.org"), None);
        // a blob changed on disk does not open
        let mut bad = bytes.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        std::fs::write(session_file(&data), &bad).unwrap();
        assert_eq!(saved_session(&data, "https://accounts.example.org"), None);
        // DPAPI blobs of other programs (other entropy, or none) do not open as a session
        assert!(dpapi::unprotect(b"not a blob").is_err());
        let other = dpapi::run(br#"{"server":"https://accounts.example.org","token":"x"}"#, true, b"another program").unwrap();
        assert!(dpapi::run(&other, false, b"another program").is_ok(), "the blob itself is good");
        assert!(dpapi::unprotect(&other).is_err(), "opened with RE:DEX's entropy");
        let none = dpapi::run(b"{}", true, b"").unwrap();
        assert!(dpapi::unprotect(&none).is_err());
        forget_session(&data);
        assert!(!session_file(&data).exists());
        assert_eq!(saved_session(&data, "https://accounts.example.org"), None);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// a session kept for an earlier official address is not given out for the new one: signed out
    #[cfg(windows)]
    #[test]
    fn a_session_kept_for_the_old_official_server_reads_as_signed_out() {
        let data = temp("moved");
        let old = "https://accounts.old-server.example.org";
        let a = Account { server: old.into(), token: "rdx_s_old".into(), account_id: 0, public_id: 0, agent: agent(old) };
        save_session(&data, &a).unwrap();
        assert_eq!(saved_session(&data, old).as_deref(), Some("rdx_s_old"));
        assert_eq!(saved_session(&data, OFFICIAL_ACCOUNTS), None);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// a proxy from the environment never sees a plain-http request to this PC; server text is capped and clean
    #[test]
    fn no_proxy_for_a_service_on_this_pc_and_server_text_is_capped() {
        assert!(agent("http://127.0.0.1:8080").config().proxy().is_none());
        assert_eq!(said(&format!("a\nb{}", "x".repeat(300))).chars().count(), 201, "200 characters and an ellipsis");
        assert!(!said("a\nb\u{7}c").contains(['\n', '\u{7}']));
    }

    #[test]
    fn a_closed_port_is_unreachable_and_plain_http_elsewhere_is_refused_before_sending() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        assert!(matches!(Account::login(&url, "a", "b", "t"), Err(Error::Unreachable(_))));
        assert!(matches!(health(&url), Err(Error::Unreachable(_))));
        assert!(matches!(Account::login("http://accounts.example.org", "a", "b", "t"), Err(Error::Refused { status: 0, .. })));
    }
}
