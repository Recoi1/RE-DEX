//! Steam login (plan 4 Task 3b). A Steamworks game server for app 480 (Valve's Spacewar test app), logged on
//! anonymously, has Steam check each `Auth::SteamTicket`: `BeginAuthSession` checks the ticket against the SteamID64 it
//! claims, then Steam's `ValidateAuthTicketResponse` says whether the player is really online with it. The session is
//! then bound to that SteamID; `end` (`EndAuthSession`) when the connection ends.
//!
//! Needs `steam_api64.dll` next to the binary and Steam's client library: an installed Steam client, or on a machine
//! without one steamclient64.dll, tier0_s64.dll and vstdlib_s64.dll from SteamCMD beside the binary (see NOTICE).
use redex_proto::refuse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use steamworks::{AuthSessionValidateError, CallbackResult, Server, ServerMode, SteamId};

/// The server's game tags: `redex,v<protocol>,<region>` (X and Y share Kalos).
pub fn game_tags() -> String {
    format!("redex,v{},kalos", redex_proto::VERSION)
}

/// Spacewar: Valve's public test app, which every Steam account can use.
pub const APP_ID: u32 = 480;
/// How long a login waits for Steam's answer: under the client's 5 s silence limit, so a slow Steam gets a retryable
/// refusal rather than a client that gave up.
pub const ANSWER: Duration = Duration::from_secs(4);
/// Tickets remembered as used, so a replay is refused even after its session ended (oldest forgotten first).
const USED: usize = 65536;

#[derive(Default)]
struct State {
    next: u64,
    /// SteamID -> (token, the login waiting for Steam's answer)
    pending: HashMap<u64, (u64, SyncSender<Result<(), String>>)>,
    /// SteamID -> token of its live auth session
    live: HashMap<u64, u64>,
    /// tokens of live sessions Steam later turned down (the player went offline, the ticket was cancelled, a ban)
    revoked: HashSet<u64>,
    used: HashSet<u64>,
    order: VecDeque<u64>,
    /// logged on to Steam (anonymous game server)
    on: bool,
}

pub struct Steam {
    server: Server,
    state: Arc<Mutex<State>>,
    hasher: RandomState,
    stop: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Steam {
    /// Starts the game server and waits (up to 20 s) until it is logged on to Steam. `port` is the world's TCP port
    /// (only reported to Steam). Call before other threads use the environment: it sets `SteamAppId`.
    pub fn start(port: u16) -> Result<Steam, String> {
        Self::start_on(port, steamworks::QUERY_PORT_SHARED)
    }

    /// `start` with the UDP port Steam answers server-browser queries on. A server meant for the browser needs its own
    /// (open `query_port` in the firewall): with the shared one and no UDP game socket (Steam P2P) the master server
    /// lists nothing.
    pub fn start_on(port: u16, query_port: u16) -> Result<Steam, String> {
        // SAFETY: Windows' environment block is locked by the OS; on other systems call this before spawning threads
        unsafe {
            std::env::set_var("SteamAppId", APP_ID.to_string());
            std::env::set_var("SteamGameId", APP_ID.to_string());
        }
        let (server, _client) = Server::init(Ipv4Addr::UNSPECIFIED, port, query_port, ServerMode::Authentication, "1.0.0.0")
            .map_err(|e| format!("Steam game server did not start: {e}"))?;
        server.set_product("redex");
        server.set_game_description("RE:DEX world");
        server.set_dedicated_server(true);
        server.log_on_anonymous();
        let (state, stop) = (Arc::new(Mutex::new(State::default())), Arc::new(AtomicBool::new(false)));
        let (s, st, halt) = (server.clone(), state.clone(), stop.clone());
        let pump = std::thread::Builder::new()
            .name("redex-steam".into())
            .spawn(move || {
                while !halt.load(Ordering::Relaxed) {
                    let mut end = Vec::new();
                    s.process_callbacks(|cb| on_callback(&st, cb, &mut end));
                    for id in end {
                        s.end_authentication_session(SteamId::from_raw(id));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
            .map_err(|e| format!("Steam thread: {e}"))?;
        let steam = Steam { server, state, hasher: RandomState::new(), stop, pump: Some(pump) };
        let until = Instant::now() + Duration::from_secs(20);
        while !lock(&steam.state).on {
            if Instant::now() > until {
                return Err("the Steam game server did not log on within 20 s".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(steam)
    }

    /// Lists the world in Steam's server browser (app 480, filter `gametagsand` `redex`): the tags say what and which
    /// protocol and region, so a client lists only worlds it can join. Steam cuts a name at 63 bytes.
    pub fn advertise(&self, name: &str, max_players: i32) {
        let mut name = name.to_string();
        while name.len() > 63 {
            name.pop();
        }
        self.server.set_mod_dir("redex");
        self.server.set_map_name("kalos");
        self.server.set_server_name(&name);
        self.server.set_max_players(max_players);
        self.server.set_game_tags(&game_tags());
        self.server.set_advertise_server_active(true);
    }

    /// The game server's own SteamID (anonymous game server account).
    pub fn server_id(&self) -> u64 {
        self.server.steam_id().raw()
    }

    /// The Steamworks game server (its callbacks run on this module's thread).
    pub fn server(&self) -> &Server {
        &self.server
    }

    /// Checks a ticket for `steam_id`; blocks up to `ANSWER` for Steam. Ok: the session token (pass it to `end` and
    /// `revoked`). Err: the `Refuse` to send.
    pub fn verify(&self, steam_id: u64, ticket: &[u8]) -> Result<u64, (u8, String)> {
        let h = self.hasher.hash_one(ticket);
        let (token, rx) = {
            let mut s = lock(&self.state);
            if s.used.contains(&h) {
                return Err((refuse::AUTH, "this Steam ticket was already used".into()));
            }
            // anyone can claim a public SteamID64: one login in flight per account, so a claim cannot clobber another
            if s.pending.contains_key(&steam_id) {
                return Err((refuse::FULL, "another login for this Steam account is in progress; try again".into()));
            }
            s.next += 1;
            let (tx, rx) = sync_channel(1);
            let token = s.next;
            // registered before BeginAuthSession, so an answer cannot come before anyone waits for it
            s.pending.insert(steam_id, (token, tx));
            (token, rx)
        };
        let id = SteamId::from_raw(steam_id);
        let mut r = self.server.begin_authentication_session(id, ticket);
        if matches!(r, Err(steamworks::AuthSessionError::DuplicateRequest)) {
            // The account has a live session on another connection. Steam checks the ticket's signature for the
            // account before it looks for a live session (a forged ticket claiming the account gets InvalidTicket;
            // tests/steam.rs checks it), so only a real ticket of the account gets here. Steam keeps one auth session
            // per account, so the old one must end for this one to begin; `live`, and so the old world session, changes
            // only once Steam's answer says Ok.
            self.server.end_authentication_session(id);
            r = self.server.begin_authentication_session(id, ticket);
        }
        if let Err(e) = r {
            self.forget(steam_id, token);
            return Err((refuse::AUTH, format!("Steam refused the ticket: {e}")));
        }
        {
            // accepted by BeginAuthSession, so a replay is refused from now on (only accepted tickets are remembered:
            // garbage cannot push them out). A server restart forgets them; Steam itself refuses a cancelled ticket.
            let mut s = lock(&self.state);
            s.used.insert(h);
            s.order.push_back(h);
            if s.order.len() > USED {
                let old = s.order.pop_front().unwrap();
                s.used.remove(&old);
            }
        }
        let answer = match rx.recv_timeout(ANSWER) {
            Ok(a) => a,
            Err(_) => {
                let mine = self.forget(steam_id, token);
                // the answer may have come between the timeout and `forget`
                match rx.try_recv() {
                    Ok(a) => a,
                    Err(_) => {
                        if mine {
                            self.server.end_authentication_session(id);
                        }
                        // Steam slow or down: worth a retry, unlike a bad ticket
                        return Err((refuse::FULL, "Steam did not confirm the login in time".into()));
                    }
                }
            }
        };
        answer.map(|()| token).map_err(|e| (refuse::AUTH, format!("{}: {e}", refuse::STEAM_ANSWER)))
    }

    /// Drops a pending login if it is still `token`'s.
    fn forget(&self, steam_id: u64, token: u64) -> bool {
        let mut s = lock(&self.state);
        let mine = s.pending.get(&steam_id).is_some_and(|p| p.0 == token);
        if mine {
            s.pending.remove(&steam_id);
        }
        mine
    }

    /// The connection of session `token` ended: `EndAuthSession`, unless a newer login of the account owns it now.
    pub fn end(&self, steam_id: u64, token: u64) {
        let mine = {
            let mut s = lock(&self.state);
            s.revoked.remove(&token);
            let mine = s.live.get(&steam_id) == Some(&token);
            if mine {
                s.live.remove(&steam_id);
            }
            mine
        };
        if mine {
            self.server.end_authentication_session(SteamId::from_raw(steam_id));
        }
    }

    /// Steam turned the session down after it was accepted: drop the connection.
    pub fn revoked(&self, token: u64) -> bool {
        lock(&self.state).revoked.contains(&token)
    }
}

impl Drop for Steam {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.pump.take() {
            let _ = h.join();
        }
    }
}

/// On the Steam thread. Ids in `end` get `EndAuthSession` after the state lock is released.
fn on_callback(state: &Mutex<State>, cb: CallbackResult, end: &mut Vec<u64>) {
    let mut s = lock(state);
    match cb {
        CallbackResult::SteamServersConnected(_) => {
            s.on = true;
            eprintln!("redex-world: logged on to Steam (game server, app {APP_ID})");
        }
        CallbackResult::SteamServerConnectFailure(f) => eprintln!("redex-world: Steam logon failed: {} (retrying: {})", f.reason, f.still_retrying),
        CallbackResult::SteamServersDisconnected(d) => {
            s.on = false;
            eprintln!("redex-world: lost the connection to Steam: {}", d.reason);
        }
        // Steam says only which account an answer is for. A login in flight takes Ok and the errors; a cancelled ticket
        // is an older connection's (clients cancel a ticket only once its connection is over), so it revokes the live
        // session. Late, it can land after the account's next login went live and revoke that fresh session once: the
        // client's reconnect (a new ticket) covers it. Auth sessions end here only when no login of the account is in
        // flight.
        CallbackResult::ValidateAuthTicketResponse(v) => {
            let id = v.steam_id.raw();
            let cancelled = matches!(v.response, Err(AuthSessionValidateError::AuthTicketCancelled));
            match (v.response, cancelled) {
                (Ok(()), _) => {
                    if let Some((token, tx)) = s.pending.remove(&id) {
                        s.live.insert(id, token);
                        // the login gave up waiting
                        if tx.try_send(Ok(())).is_err() {
                            s.live.remove(&id);
                            end.push(id);
                        }
                    }
                }
                (Err(e), false) if s.pending.contains_key(&id) => {
                    let (_, tx) = s.pending.remove(&id).unwrap();
                    end.push(id);
                    let _ = tx.try_send(Err(e.to_string()));
                }
                (Err(_), _) => {
                    if let Some(token) = s.live.remove(&id) {
                        s.revoked.insert(token);
                        if !s.pending.contains_key(&id) {
                            end.push(id);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn game_tags_name_game_protocol_and_region() {
        assert_eq!(super::game_tags(), "redex,v5,kalos");
    }
}
