//! The start screen's account: sign in (address or username and a password), make an account, ask for a password
//! reset, sign out, and the account's own characters (the online ones). Every call to the service runs on a thread of
//! its own and lands in `poll`, so the screen never waits on the network. The password goes from the form to that
//! thread and is dropped there; only the session token is kept, encrypted for this Windows user
//! (`xr_net::account::save_session`), and later launches sign in with it alone. When the official service or world
//! does not answer, `unreachable` says so and the screen offers P2P play with the characters on this PC.
//!
//! Online characters never touch characters.json: one made on the Online tab exists only in memory until the account
//! has it, and its saves live under the account's own root on this PC (`characters::online_user_dir`).
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use redex_proto::Character;
use xr_net::account::{self, Account, Error, Me};

use crate::settings::Settings;

/// Where the account service and its world are: the official ones, or (test-hooks builds only) the settings' overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Servers {
    pub accounts: String,
    /// `tls:host:port`
    pub world: String,
}

impl Servers {
    pub fn official() -> Servers {
        Servers { accounts: account::OFFICIAL_ACCOUNTS.into(), world: account::OFFICIAL_WORLD.into() }
    }

    pub fn of(settings: &Settings) -> Servers {
        // a normal build always talks to the official server: a settings file can't point the password form elsewhere
        #[cfg(feature = "test-hooks")]
        {
            let or = |s: &str, official: &str| if s.trim().is_empty() { official.to_string() } else { s.trim().to_string() };
            Servers { accounts: or(&settings.account_server, account::OFFICIAL_ACCOUNTS), world: or(&settings.account_world, account::OFFICIAL_WORLD) }
        }
        #[cfg(not(feature = "test-hooks"))]
        {
            let _ = settings;
            Servers::official()
        }
    }

    pub fn is_official(&self) -> bool {
        *self == Servers::official()
    }

    /// The account service's host, as the panel names a server that is not the official one.
    pub fn host(&self) -> &str {
        let rest = self.accounts.split_once("://").map_or(self.accounts.as_str(), |(_, r)| r);
        rest.split(['/', '?', '#']).next().unwrap_or(rest)
    }
}

/// A line the panel shows: the service's own words, or what to do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    Info(String),
    Error(String),
}

/// What a finished call brought back.
enum Done {
    /// signed in (or a kept session still good): the account, who it is, its characters, whether the world answers, and
    /// whether the session is kept on this PC
    In(Account, Me, Vec<Character>, Result<(), String>, Result<(), String>),
    /// no session on this PC: whether the service and the world answer
    Out(Result<(), String>),
    Failed(Error),
    /// a kept session met a server error (5xx): its words, and P2P is offered
    Down(String),
    Registered(String),
    MailAsked,
    /// a character went onto the account: the account's list after it
    Created(Vec<Character>),
    /// signed out on this PC; whether the service was told
    SignedOut(Result<(), String>),
}

pub struct Session {
    data: PathBuf,
    pub servers: Servers,
    account: Option<Account>,
    me: Option<Me>,
    /// the account's characters, the online ones (none while signed out); never one of characters.json's ids
    pub characters: Vec<Character>,
    pub note: Option<Note>,
    /// the service or world did not answer (why): P2P is offered
    pub unreachable: Option<String>,
    /// the session is kept on this PC, so the game can sign in with it
    kept: bool,
    /// a character made on the Online tab while another call was out: put once that one is back
    queued: Option<Character>,
    /// the call out, and what the screen says meanwhile
    job: Option<(Receiver<Done>, &'static str)>,
}

/// "RE:DEX on <this PC's name>", as the account's session list shows it.
fn device() -> String {
    format!("RE:DEX on {}", std::env::var("COMPUTERNAME").unwrap_or_else(|_| "a PC".into()))
}

/// Whether the world takes a TCP connection within 3 s (`tls:host:port`).
pub fn reach(world: &str) -> Result<(), String> {
    let addr = world.strip_prefix("tls:").unwrap_or(world);
    let addrs: Vec<_> = addr.to_socket_addrs().map_err(|e| format!("{addr}: {e}"))?.collect();
    let mut last = format!("{addr}: no address");
    for a in addrs {
        match std::net::TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
            Ok(_) => return Ok(()),
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    Err(last)
}

/// Signed in: who, the characters, and whether the world answers (`kept`: whether the session is on disk).
fn signed_in(a: Account, world: &str, kept: Result<(), String>) -> Done {
    let me = match a.me() {
        Ok(m) => m,
        Err(e) => return Done::Failed(e),
    };
    match a.characters() {
        Ok(list) => Done::In(a, me, list, reach(world), kept),
        Err(e) => Done::Failed(e),
    }
}

/// At start or Try again, a server error (5xx, a database down) is an outage: its words and the P2P offer.
fn outage(e: Error) -> Done {
    match e {
        Error::Refused { status, message } if status >= 500 => Done::Down(message),
        e => Done::Failed(e),
    }
}

impl Session {
    /// The account as this PC has it: a kept session is checked with the service (signed in with it alone), else the
    /// service and world are only asked whether they answer.
    pub fn start(data: PathBuf, servers: Servers) -> Session {
        let mut s = Session::idle(data, servers);
        s.check();
        s
    }

    /// Signed out, nothing asked yet (`check` asks).
    pub fn idle(data: PathBuf, servers: Servers) -> Session {
        Session { data, servers, account: None, me: None, characters: Vec::new(), note: None, unreachable: None, kept: false, queued: None, job: None }
    }

    /// Asks again (after "Try again", or at start).
    pub fn check(&mut self) {
        let (data, s) = (self.data.clone(), self.servers.clone());
        match account::saved_session(&data, &s.accounts) {
            Some(token) => self.run("Signing in…", move || match Account::resume(&s.accounts, &token) {
                Ok((a, _)) => match signed_in(a, &s.world, Ok(())) {
                    Done::Failed(e) => outage(e),
                    d => d,
                },
                Err(e) => outage(e),
            }),
            None => self.run("Checking the server…", move || match account::health(&s.accounts) {
                Ok(()) => Done::Out(reach(&s.world)),
                Err(e) => match outage(e) {
                    Done::Failed(e) => Done::Out(Err(e.to_string())),
                    d => d,
                },
            }),
        }
    }

    fn run(&mut self, what: &'static str, f: impl FnOnce() -> Done + Send + 'static) {
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("redex-account".into())
            .spawn(move || {
                let _ = tx.send(f());
            })
            .expect("spawn the account thread");
        self.job = Some((rx, what));
        self.note = None;
    }

    /// What the screen says while a call is out ("Signing in…"), None when none is.
    pub fn busy(&self) -> Option<&'static str> {
        self.job.as_ref().map(|j| j.1)
    }

    pub fn me(&self) -> Option<&Me> {
        self.me.as_ref()
    }

    /// Online play: signed in with the session kept on this PC, the address confirmed, and the server answering.
    pub fn can_play_online(&self) -> bool {
        self.kept && self.me.as_ref().is_some_and(|m| m.email_verified) && self.unreachable.is_none() && self.job.is_none()
    }

    /// The server did not answer: offer P2P with the characters on this PC.
    pub fn offers_p2p(&self) -> bool {
        self.unreachable.is_some()
    }

    /// Signs in. The password is moved into the call's thread and dropped there.
    pub fn sign_in(&mut self, login: String, password: String) {
        let (data, s) = (self.data.clone(), self.servers.clone());
        self.run("Signing in…", move || {
            let a = match Account::login(&s.accounts, login.trim(), &password, &device()) {
                Ok(a) => a,
                Err(e) => return Done::Failed(e),
            };
            drop(password);
            let kept = account::save_session(&data, &a).inspect_err(|e| log::warn!("account: the session could not be kept: {e}"));
            signed_in(a, &s.world, kept)
        });
    }

    pub fn register(&mut self, email: String, username: String, password: String, trainer_name: String) {
        let s = self.servers.clone();
        self.run("Creating your account…", move || match account::register(&s.accounts, email.trim(), username.trim(), &password, trainer_name.trim()) {
            Ok(said) => Done::Registered(said),
            Err(e) => Done::Failed(e),
        });
    }

    pub fn forgot(&mut self, email: String) {
        let s = self.servers.clone();
        self.run("Asking for a reset mail…", move || match account::forgot_password(&s.accounts, email.trim()) {
            Ok(()) => Done::MailAsked,
            Err(e) => Done::Failed(e),
        });
    }

    /// Signs out on this PC at once (the kept session is deleted), then tells the service in the background.
    pub fn sign_out(&mut self) {
        account::forget_session(&self.data);
        (self.me, self.characters, self.kept, self.queued) = (None, Vec::new(), false, None);
        if let Some(a) = self.account.take() {
            self.run("Signing out…", move || Done::SignedOut(a.logout().map_err(|e| e.to_string())));
        }
        log::info!("account: signed out on this PC");
    }

    /// Puts a character made on the Online tab on the account. It was never in characters.json; it shows on the Online
    /// tab once the account has it. While another call is out it waits for it.
    pub fn create_online(&mut self, c: Character) {
        if self.job.is_some() {
            self.queued = Some(c);
            return;
        }
        let Some(a) = self.account.clone() else {
            log::warn!("account: {:016x} {:?} not put on the account: signed out", c.id, c.name);
            self.note = Some(Note::Error("Sign in again to make an online character.".into()));
            return;
        };
        self.run("Putting the character on your account…", move || {
            let id = c.id;
            match a.put_character(&c) {
                Ok(list) if list.iter().any(|x| x.id == id) => Done::Created(list),
                Ok(_) => Done::Failed(Error::Refused { status: 200, message: "the account already has 8 characters".into() }),
                Err(e) => Done::Failed(e),
            }
        });
    }

    /// Takes in a finished call; true when something changed.
    pub fn poll(&mut self) -> bool {
        let Some((rx, _)) = &self.job else { return false };
        let changed = match rx.try_recv() {
            Ok(done) => {
                self.job = None;
                self.apply(done);
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                log::warn!("account: the call's thread ended without an answer");
                self.note = Some(Note::Error("Something went wrong talking to the account service. Try again.".into()));
                true
            }
        };
        if self.job.is_none() {
            if let Some(c) = self.queued.take() {
                self.create_online(c);
            }
        }
        changed
    }

    /// Waits for the calls out (tests).
    pub fn wait(&mut self) {
        while self.job.is_some() {
            if !self.poll() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// The account's list without this PC's ids: an id is on one list only (an old client put this PC's characters on
    /// the account; those stay "On this PC", where their saves are).
    fn online_only(&self, list: Vec<Character>) -> Vec<Character> {
        let local = super::characters::Store::load(&self.data);
        list.into_iter()
            .filter(|c| {
                let mine = local.get(c.id).is_some();
                if mine {
                    log::info!("account: {:016x} is on this PC too; it stays under On this PC only", c.id);
                }
                !mine
            })
            .collect()
    }

    fn apply(&mut self, done: Done) {
        match done {
            Done::In(a, me, list, world, kept) => {
                log::info!("account: signed in as {} ({} online character(s))", me.username, list.len());
                self.kept = kept.is_ok();
                if let Err(e) = kept {
                    self.note = Some(Note::Error(format!("Signed in for now, but this PC could not keep the session ({e}), so online play is off. Sign in again later.")));
                } else if !me.email_verified {
                    self.note = Some(Note::Info("Confirm your email address to play online: open the link in the confirmation email, then sign in again.".into()));
                }
                self.unreachable = world.err().inspect(|e| log::warn!("account: the world does not answer: {e}"));
                self.characters = self.online_only(list);
                (self.account, self.me) = (Some(a), Some(me));
            }
            Done::Out(answer) => {
                self.unreachable = answer.err().inspect(|e| log::warn!("account: the server does not answer: {e}"));
            }
            Done::Down(message) => {
                log::warn!("account: the server is in trouble: {message}");
                self.note = Some(Note::Error(format!("The server says: {message}")));
                self.unreachable = Some(message);
            }
            Done::Failed(Error::Unreachable(e)) => {
                log::warn!("account: the server does not answer: {e}");
                self.unreachable = Some(e);
            }
            Done::Failed(Error::SignedOut(m)) => {
                log::info!("account: the kept session is no longer good ({m}); signed out");
                account::forget_session(&self.data);
                (self.account, self.me, self.characters, self.unreachable, self.kept) = (None, None, Vec::new(), None, false);
                self.note = Some(Note::Error(format!("You were signed out ({m}). Sign in again to play online.")));
            }
            Done::Failed(Error::Refused { status, message }) => {
                log::info!("account: the service said {status}: {message}");
                self.unreachable = None;
                self.note = Some(Note::Error(format!("The server says: {message}")));
            }
            Done::Registered(said) => {
                self.unreachable = None;
                self.note = Some(Note::Info(format!("The server says: {said}. Your account needs that confirmation email: open its link, then sign in.")));
            }
            Done::MailAsked => {
                self.unreachable = None;
                self.note = Some(Note::Info("The server took the request. If that address has an account, a reset email is on its way.".into()));
            }
            Done::Created(list) => {
                self.characters = self.online_only(list);
                self.note = Some(Note::Info("The character is on your account now.".into()));
            }
            Done::SignedOut(Ok(())) => {}
            Done::SignedOut(Err(e)) => {
                log::warn!("account: the service did not take the sign-out ({e}); this PC forgot the session anyway");
                self.note = Some(Note::Info("Signed out on this PC. The server could not be told; that session ends by itself within 30 days.".into()));
            }
        }
    }

    /// Sets what a sign-in would, without a server (the start screen's tests).
    #[cfg(test)]
    pub(super) fn assume(&mut self, me: Option<Me>, unreachable: Option<String>) {
        (self.kept, self.me, self.unreachable) = (me.is_some(), me, unreachable);
    }
}

/// A fake account service for the start screen's tests (here and in mod.rs).
#[cfg(test)]
pub(super) mod testing {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    /// A request as the fake backend saw it.
    pub struct Req {
        pub method: String,
        pub path: String,
        pub bearer: Option<String>,
        pub body: serde_json::Value,
    }

    /// A backend on 127.0.0.1 answering each request with `route`'s (status, JSON); its port. Lives for the test.
    pub fn fake(route: impl Fn(&Req) -> (u16, serde_json::Value) + Send + Sync + 'static) -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let route = Arc::new(route);
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let route = route.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    let mut it = line.split_whitespace();
                    let (method, path) = (it.next().unwrap_or_default().to_string(), it.next().unwrap_or_default().to_string());
                    let (mut len, mut bearer) = (0usize, None);
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        let (k, v) = h.split_once(':').unwrap_or((h, ""));
                        match k.to_ascii_lowercase().as_str() {
                            "content-length" => len = v.trim().parse().unwrap(),
                            "authorization" => bearer = v.trim().strip_prefix("Bearer ").map(String::from),
                            _ => {}
                        }
                    }
                    let mut body = vec![0; len];
                    r.read_exact(&mut body).unwrap();
                    let req = Req { method, path, bearer, body: serde_json::from_slice(&body).unwrap_or_default() };
                    let (status, v) = route(&req);
                    let text = if status == 204 { String::new() } else { v.to_string() };
                    let mut s = s;
                    let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                });
            }
        });
        port
    }

    pub const PASSWORD: &str = "pw-Zq8!unique-test-secret-4471";
    pub const TOKEN: &str = "rdx_s_fake-session-token-9vX2kLmN";

    pub fn look() -> String {
        xr_net::x::look::Look::default_for(1).appearance().unwrap().to_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// What the fake account holds and what it was asked.
    #[derive(Default)]
    pub struct State {
        pub revoked: bool,
        /// answer every session call with this status (a database down)
        pub fail: Option<u16>,
        /// the account's characters (JSON), one "Online" to start with
        pub characters: Vec<serde_json::Value>,
        /// "METHOD path" of every request
        pub seen: Vec<String>,
    }

    /// A backend with one account `tester` (confirmed or not).
    pub fn backend(verified: bool, state: Arc<Mutex<State>>) -> u16 {
        state.lock().unwrap().characters.push(serde_json::json!({ "id": "00000000000000aa", "name": "Online", "look": look(), "created": 1, "changed": 2 }));
        fake(move |r| {
            let mut st = state.lock().unwrap();
            st.seen.push(format!("{} {}", r.method, r.path));
            let authed = r.bearer.as_deref() == Some(TOKEN) && !st.revoked;
            match (r.method.as_str(), r.path.as_str()) {
                ("GET", "/healthz") => (200, serde_json::json!({ "status": "ok" })),
                ("POST", "/v1/login") if r.body["email"] == "tester" && r.body["password"] == PASSWORD => {
                    (200, serde_json::json!({ "token": TOKEN, "account_id": 7, "session_id": 1, "expires_at": 0, "email_verified": verified }))
                }
                ("POST", "/v1/login") => (401, serde_json::json!({ "error": "unauthorized", "message": "wrong email or password" })),
                ("POST", "/v1/register") => (202, serde_json::json!({ "status": "check your mail to confirm the address" })),
                ("POST", "/v1/password/forgot") => (202, serde_json::json!({})),
                (_, _) if !authed => (401, serde_json::json!({ "error": "unauthorized", "message": "session expired or logged out" })),
                (_, _) if st.fail.is_some() => (st.fail.unwrap_or(500), serde_json::json!({ "error": "internal", "message": "internal error" })),
                ("GET", "/v1/account") => (200, serde_json::json!({ "account_id": 7, "public_id": 4242, "username": "tester", "email_verified": verified })),
                ("GET", "/v1/characters") => (200, serde_json::json!({ "characters": st.characters })),
                ("PUT", p) if p.starts_with("/v1/characters/") => {
                    let mut c = r.body.clone();
                    c["id"] = p.trim_start_matches("/v1/characters/").into();
                    st.characters.push(c);
                    (200, serde_json::json!({ "characters": st.characters }))
                }
                ("POST", "/v1/logout") => (204, serde_json::json!({})),
                _ => (404, serde_json::json!({ "error": "not_found", "message": "not found" })),
            }
        })
    }

    pub fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redex-signin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn servers(port: u16, world_port: u16) -> Servers {
        Servers { accounts: format!("http://127.0.0.1:{port}"), world: format!("tls:127.0.0.1:{world_port}") }
    }

    pub fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    pub fn started(data: &std::path::Path, s: Servers) -> Session {
        let mut s = Session::start(data.to_path_buf(), s);
        s.wait();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::sync::{Arc, Mutex};

    fn error(s: &Session) -> String {
        match &s.note {
            Some(Note::Error(e)) => e.clone(),
            other => panic!("no error note: {other:?}"),
        }
    }

    /// signed in with a password, kept, and a relaunch signs in with the kept token alone
    #[test]
    fn signing_in_keeps_the_session_and_a_relaunch_needs_no_password() {
        let data = temp("ok");
        let port = backend(true, Arc::default());
        let mut s = started(&data, servers(port, port));
        assert_eq!((s.me(), s.unreachable.as_ref()), (None, None), "signed out, server up");
        s.sign_in("tester".into(), PASSWORD.into());
        assert_eq!(s.busy(), Some("Signing in…"));
        s.wait();
        assert_eq!(s.me().map(|m| (m.username.as_str(), m.public_id)), Some(("tester", 4242)));
        assert_eq!(s.characters.len(), 1);
        assert!(s.can_play_online() && !s.offers_p2p());
        assert!(account::session_file(&data).is_file());
        // a relaunch: the kept token alone
        let again = started(&data, servers(port, port));
        assert_eq!(again.me().map(|m| m.account_id), Some(7));
        assert!(again.can_play_online());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_wrong_password_says_so_and_keeps_nothing() {
        let data = temp("wrong");
        let port = backend(true, Arc::default());
        let mut s = started(&data, servers(port, port));
        s.sign_in("tester".into(), "not the password".into());
        s.wait();
        assert_eq!(s.me(), None);
        assert!(error(&s).contains("wrong email or password"), "{:?}", s.note);
        assert!(!s.offers_p2p(), "a wrong password is not an outage");
        assert!(!account::session_file(&data).exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn an_unconfirmed_account_signs_in_but_does_not_play_online() {
        let data = temp("unconfirmed");
        let port = backend(false, Arc::default());
        let mut s = started(&data, servers(port, port));
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        assert_eq!(s.me().map(|m| m.email_verified), Some(false));
        assert!(!s.can_play_online());
        assert!(matches!(&s.note, Some(Note::Info(m)) if m.contains("Confirm your email")), "{:?}", s.note);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 429 and 503 to a password sign-in are the server's answer, shown as it gave it: not an outage, no P2P offer
    #[test]
    fn rate_limits_and_busy_answers_show_the_servers_words() {
        let data = temp("limits");
        for (status, code, message) in [(429, "rate_limited", "too many attempts; try again later"), (503, "busy", "the service is busy; try again in a moment")] {
            let port = fake(move |r| match r.path.as_str() {
                "/healthz" => (200, serde_json::json!({ "status": "ok" })),
                _ => (status, serde_json::json!({ "error": code, "message": message })),
            });
            let mut s = started(&data, servers(port, port));
            s.sign_in("tester".into(), PASSWORD.into());
            s.wait();
            assert_eq!(s.me(), None);
            assert_eq!(error(&s), format!("The server says: {message}"));
            assert!(!s.offers_p2p());
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    /// with a kept session, a 5xx (a database down) shows the server's words AND offers P2P; the token stays
    #[test]
    fn a_server_error_on_a_kept_session_says_so_and_offers_p2p() {
        let data = temp("5xx");
        let state = Arc::new(Mutex::new(State::default()));
        let port = backend(true, state.clone());
        let mut s = started(&data, servers(port, port));
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        for status in [500, 503] {
            state.lock().unwrap().fail = Some(status);
            let s = started(&data, servers(port, port));
            assert_eq!(error(&s), "The server says: internal error", "{status}");
            assert!(s.offers_p2p() && !s.can_play_online(), "{status}");
            assert!(account::session_file(&data).is_file(), "{status}: the token was thrown away");
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    /// a kept token the server no longer takes: signed out, told so, and the token is gone from disk
    #[test]
    fn an_expired_token_means_signed_out_and_a_prompt() {
        let data = temp("expired");
        let state = Arc::new(Mutex::new(State::default()));
        let port = backend(true, state.clone());
        let mut s = started(&data, servers(port, port));
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        assert!(s.me().is_some());
        state.lock().unwrap().revoked = true;
        let s = started(&data, servers(port, port));
        assert_eq!(s.me(), None);
        assert!(error(&s).contains("signed out") && error(&s).contains("Sign in again"), "{:?}", s.note);
        assert!(!account::session_file(&data).exists(), "the dead token stays on disk");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// no answer from the service, or from the world: the P2P fallback is offered; a kept session is not thrown away
    #[test]
    fn an_unreachable_server_offers_p2p() {
        let data = temp("down");
        let port = backend(true, Arc::default());
        let dead = closed_port();
        // signed out, service down
        let mut s = started(&data, servers(dead, port));
        assert!(s.offers_p2p() && !s.can_play_online());
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        assert!(s.offers_p2p() && s.me().is_none());
        // signed in, then the service goes away: the kept token stays for when it is back
        let mut s = started(&data, servers(port, port));
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        let s = started(&data, servers(dead, port));
        assert!(s.offers_p2p());
        assert!(account::session_file(&data).is_file());
        // the service answers, the world does not
        let s = Servers { world: format!("tls:127.0.0.1:{dead}"), ..servers(port, port) };
        let s = started(&data, s);
        assert!(s.me().is_some() && s.offers_p2p() && !s.can_play_online());
        let _ = std::fs::remove_dir_all(&data);
    }

    /// every flow that takes the password, then the data folder searched for it (UTF-8 and UTF-16) and for the token
    #[test]
    fn no_password_or_clear_token_is_ever_written_to_disk() {
        let data = temp("disk");
        let port = backend(true, Arc::default());
        let mut s = started(&data, servers(port, port));
        s.register("a@example.org".into(), "tester".into(), PASSWORD.into(), "Ana".into());
        s.wait();
        assert!(matches!(&s.note, Some(Note::Info(m)) if m.contains("check your mail") && m.contains("confirmation email")), "{:?}", s.note);
        s.forgot("a@example.org".into());
        s.wait();
        s.sign_in("tester".into(), "a wrong one first".into());
        s.wait();
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        assert!(s.me().is_some());
        drop(started(&data, servers(port, port)));
        let utf16: Vec<u8> = PASSWORD.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut files = 0;
        let mut dirs = vec![data.clone()];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                if e.file_type().unwrap().is_dir() {
                    dirs.push(e.path());
                    continue;
                }
                files += 1;
                let b = std::fs::read(e.path()).unwrap();
                for needle in [PASSWORD.as_bytes(), &utf16, TOKEN.as_bytes(), b"wrong one first"] {
                    assert!(!b.windows(needle.len()).any(|w| w == needle), "{} holds a secret", e.path().display());
                }
            }
        }
        assert!(files >= 1, "the session file was written");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn signing_out_forgets_the_session() {
        let data = temp("out");
        let port = backend(true, Arc::default());
        let mut s = started(&data, servers(port, port));
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        s.sign_out();
        assert!(!account::session_file(&data).exists(), "forgotten before the service is told");
        s.wait();
        assert_eq!((s.me(), s.characters.len()), (None, 0));
        assert!(!account::session_file(&data).exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    /// a normal build ignores the settings' server overrides (a settings file cannot point the password form elsewhere);
    /// a test-hooks build takes them and names the host
    #[test]
    fn server_overrides_only_in_test_builds() {
        let settings = Settings { account_server: "https://elsewhere.example.org/".into(), account_world: "tls:elsewhere.example.org:1".into(), ..Settings::default() };
        let s = Servers::of(&settings);
        if cfg!(feature = "test-hooks") {
            assert!(!s.is_official() && s.host() == "elsewhere.example.org", "{s:?}");
        } else {
            assert!(s.is_official(), "{s:?}");
        }
        assert_eq!(Servers::official().host(), "playredex.mooo.com");
    }

    /// the official address moved (sslip.io to playredex.mooo.com): a session kept for the old address is not sent to
    /// the new one. Signed out without an error, one health check, nothing asked again, and a sign-in replaces it
    #[test]
    fn a_session_kept_for_a_moved_server_is_signed_out_quietly() {
        let data = temp("moved");
        let old = backend(true, Arc::default());
        let mut s = started(&data, servers(old, old));
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        assert!(s.me().is_some() && account::session_file(&data).is_file());
        let state = Arc::new(Mutex::new(State::default()));
        let new = backend(true, state.clone());
        let mut s = started(&data, servers(new, new));
        assert_eq!((s.me(), &s.note, s.offers_p2p()), (None, &None, false));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!s.poll() && s.busy().is_none(), "asked again");
        // (the world probe is a bare TCP connect to the same port: an empty request line)
        let seen: Vec<String> = state.lock().unwrap().seen.iter().filter(|r| !r.trim().is_empty()).cloned().collect();
        assert_eq!(seen, ["GET /healthz"], "the old token went to the new server, or it looped");
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        assert!(started(&data, servers(new, new)).can_play_online(), "the new server's session is kept");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// an id on this PC's list never shows on the Online tab too, even when the account has it
    #[test]
    fn an_id_is_on_one_list_only() {
        let data = temp("one-list");
        let mut store = super::super::characters::Store::default();
        let a = xr_net::x::look::Look::default_for(1).appearance().unwrap();
        let id = store.create("Here", a, 1).unwrap();
        store.save(&data).unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        state.lock().unwrap().characters.push(serde_json::json!({ "id": format!("{id:016x}"), "name": "Here", "look": look(), "created": 1, "changed": 1 }));
        let port = backend(true, state);
        let mut s = started(&data, servers(port, port));
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        assert_eq!(s.characters.iter().map(|c| c.id).collect::<Vec<_>>(), vec![0xaa], "the account's copy of this PC's character is left out");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// a character made while another call is out is put once that call is back, not dropped
    #[test]
    fn a_new_online_character_waits_for_the_call_out() {
        let data = temp("queue");
        let state = Arc::new(Mutex::new(State::default()));
        let port = backend(true, state.clone());
        let mut s = started(&data, servers(port, port));
        s.sign_in("tester".into(), PASSWORD.into());
        s.wait();
        s.check();
        let look = xr_net::x::look::Look::default_for(0).appearance().unwrap().to_bytes().to_vec();
        s.create_online(Character { id: 0xbb, name: "New".into(), look, created: 5, changed: 5 });
        s.wait();
        assert!(s.characters.iter().any(|c| c.id == 0xbb), "{:?} {:?} {:?}", s.characters, s.note, state.lock().unwrap().seen);
        assert!(state.lock().unwrap().seen.iter().any(|r| r == "PUT /v1/characters/00000000000000bb"));
        assert!(!data.join("characters.json").exists(), "an online character never touches characters.json");
        let _ = std::fs::remove_dir_all(&data);
    }
}
