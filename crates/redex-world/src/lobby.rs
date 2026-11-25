//! In-game lobbies: a world the player's own game runs in-process ("Host lobby" in the overlay), for 2 to 8 players.
//! The host is a player of it like anyone else (its game joins it as `steam:<id>`); it can remove a player
//! (`Host::kick`), and closing it (`Host::close`, or dropping it) tells everyone before the server stops. `Door` keeps
//! out who may not come in: under friends-only anyone but the host's Steam friends, and anyone the host removed.
//!
//! In-process rather than a child `redex-world.exe`: nothing more to ship or to find next to redex.exe, no process to
//! orphan when the game dies (the server goes with it), and kicks and the member list are plain calls. Steam runs a
//! game server beside the game's own client in one process (Valve's listen-server setup), and Steam short-cuts the
//! host's own connection to it.
use crate::server::{Config, Server};
use crate::world::{STEAM_KEY, player_id};
use redex_proto::{Msg, PlayerId, refuse};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a removed player is told (and told again if they try to come back).
pub const REMOVED: &str = "the host removed you from this lobby";
/// What everyone is told when the host closes the lobby or quits the game.
pub const CLOSED: &str = "the host closed the lobby";
/// Under friends-only, what anyone else is told.
pub const FRIENDS_ONLY: &str = "this lobby is for the host's Steam friends only";
/// A login past the lobby's size (the host's seat is always kept).
pub const FULL: &str = "this lobby is full";
/// Connections a lobby keeps for logins in the handshake (`Host::config`).
#[cfg_attr(not(any(feature = "steam", feature = "mp-tests")), allow(dead_code))]
const PENDING: usize = 4;
/// Players in a lobby, the host counted.
pub const MIN_PLAYERS: u8 = 2;
pub const MAX_PLAYERS: u8 = 8;

/// Who may log in. The host always; under friends-only only the host's Steam friends (a dev id has no friends); never
/// anyone the host removed, for as long as the lobby lasts.
#[derive(Default)]
pub struct Door(Mutex<DoorState>);

#[derive(Default)]
struct DoorState {
    host: u128,
    /// Some under friends-only: the login keys of the host's Steam friends as last told
    friends: Option<HashSet<u128>>,
    removed: HashSet<u128>,
    /// players with the host; 0: no cap here (the server's own limits only)
    max: usize,
    /// the lobby is closing: everyone is told so
    closed: bool,
}

impl Door {
    /// `host`: the host's own login key (`steam_key`, or a dev id's).
    pub fn new(host: u128, friends_only: bool) -> Door {
        Door(Mutex::new(DoorState { host, friends: friends_only.then(HashSet::new), removed: HashSet::new(), max: 0, closed: false }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DoorState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Under friends-only: the host's friends now, as login keys (`steam_key` of each Steam friend; Steam's list changes
    /// while the lobby is up).
    pub fn set_friends(&self, ids: impl IntoIterator<Item = u128>) {
        if let Some(f) = &mut self.state().friends {
            *f = ids.into_iter().collect();
        }
    }

    /// Kept out until the lobby closes.
    pub fn remove(&self, key: u128) {
        self.state().removed.insert(key);
    }

    /// The host's login key (always let in, never counted against the size).
    pub fn host(&self) -> u128 {
        self.state().host
    }

    /// Players in the lobby with the host.
    pub fn set_max(&self, max: usize) {
        self.state().max = max;
    }

    /// The lobby is closing: from now on every login is told so.
    pub fn close(&self) {
        self.state().closed = true;
    }

    /// Ok, or the refusal for this login; `others`: players logged in now, the host not counted.
    pub fn check(&self, key: u128, others: usize) -> Result<(), (u8, &'static str)> {
        let s = self.state();
        if s.closed {
            return Err((refuse::CLOSED, CLOSED));
        }
        // who may come in at all first (lasting refusals), then the size (worth a retry)
        Self::who(&s, key)?;
        if key != s.host && s.max > 0 && others + 1 >= s.max {
            return Err((refuse::FULL, FULL));
        }
        Ok(())
    }

    /// Removed, or not a friend under friends-only: refused for good, the host never.
    fn who(s: &DoorState, key: u128) -> Result<(), (u8, &'static str)> {
        if s.removed.contains(&key) {
            return Err((refuse::CLOSED, REMOVED));
        }
        match &s.friends {
            Some(f) if key != s.host && !f.contains(&key) => Err((refuse::AUTH, FRIENDS_ONLY)),
            _ => Ok(()),
        }
    }

    /// A connection whose account is known before Hello (Steam P2P): the host's own comes in past every connection
    /// limit; one the door would turn away anyway is refused now, before it takes a slot.
    pub fn early(&self, key: u128) -> Early {
        let s = self.state();
        if s.closed {
            return Early::Refuse((refuse::CLOSED, CLOSED));
        }
        if key == s.host {
            return Early::Host;
        }
        match Self::who(&s, key) {
            Err(r) => Early::Refuse(r),
            Ok(()) => Early::Ok,
        }
    }
}

/// What the door says about a connection before it logs in (`Door::early`).
#[derive(Debug, PartialEq)]
pub enum Early {
    /// the host's own account: past the connection limits
    Host,
    /// may try (the connection limits apply)
    Ok,
    /// refused now
    Refuse((u8, &'static str)),
}

/// A Steam account's login key, as the world keys a verified Steam login.
pub fn steam_key(id: u64) -> u128 {
    STEAM_KEY | u128::from(id)
}

/// A dev id's login key (tests: the TCP dev path).
pub fn dev_key(id: u128) -> u128 {
    id & !(1 << 127)
}

/// A lobby being hosted: the server, its door, and the address players join.
pub struct Host {
    server: Option<Server>,
    door: Arc<Door>,
    address: String,
    host: u128,
    /// what must outlive the server (Steam's game server), dropped after it
    keep: Option<Box<dyn Send + Sync>>,
}

#[cfg_attr(not(any(feature = "steam", feature = "mp-tests")), allow(dead_code))]
impl Host {
    /// A lobby's limits: `max` players (the host's seat kept, `Door`), at most two connections per account (a client
    /// reconnecting briefly holds two), `PENDING` connections in the handshake in all, and 2 s to say Hello, so nobody
    /// keeps the lobby full without logging in. No reports file (nobody reads it on a player's PC).
    fn config(max: u8, door: &Arc<Door>, data: PathBuf) -> Config {
        let max = max.clamp(MIN_PLAYERS, MAX_PLAYERS) as usize;
        door.set_max(max);
        Config {
            max_conns: max + PENDING,
            max_per_peer: 2,
            max_pending: PENDING,
            hello_timeout: Duration::from_secs(2),
            reports: false,
            door: Some(door.clone()),
            data,
            ..Config::default()
        }
    }

    /// A lobby over Steam's relays: a Steam game server in this process (it waits up to 20 s for Steam's logon, so call
    /// it off the game's thread), listening on Steam P2P, joined as `steam:<id>`. `public`: listed in Steam's server
    /// browser as `name` (tags `redex,v<n>,kalos`), else not listed at all. `data`: the world's own folder (reports,
    /// players' character lists).
    #[cfg(feature = "steam")]
    pub fn steam(slot: crate::steam::InUse, name: &str, max: u8, public: bool, door: Arc<Door>, host: u128, data: PathBuf) -> Result<Host, String> {
        use crate::steam::Steam;
        use crate::transport::SteamTransport;
        // a listed server needs a query port of its own (Steam asks it for the name); unlisted, none
        let query = if public { free_udp_port().unwrap_or(27016) } else { steamworks::QUERY_PORT_SHARED };
        // over Steam P2P there is no game socket, but the master server lists a server only with a nonzero game port
        let steam = Arc::new(Steam::start_with(slot, 27015, query)?);
        if public {
            steam.advertise(name, i32::from(max.clamp(MIN_PLAYERS, MAX_PLAYERS)));
        }
        let t = SteamTransport::listen(&steam).map_err(|e| e.to_string())?;
        let address = format!("steam:{}", steam.server_id());
        let cfg = Config { steam: Some(steam.clone()), ..Self::config(max, &door, data) };
        let mut h = Host::start(cfg, t, address, door, host);
        h.keep = Some(Box::new(steam));
        Ok(h)
    }

    /// A lobby on TCP 127.0.0.1 for dev ids (the MP test harness: two games on one PC, no Steam).
    #[cfg(feature = "mp-tests")]
    pub fn dev(max: u8, door: Arc<Door>, host: u128, data: PathBuf) -> std::io::Result<Host> {
        let t = crate::transport::TcpTransport::bind("127.0.0.1:0")?;
        let address = t.local_addr()?.to_string();
        // every client on this PC is the same peer (127.0.0.1)
        let cfg = Config { dev: true, max_per_peer: 8, ..Self::config(max, &door, data) };
        Ok(Host::start(cfg, t, address, door, host))
    }

    /// A lobby on any transport, configured as a Steam lobby is (tests: peers named as Steam names them).
    #[cfg(feature = "mp-tests")]
    pub fn on(t: impl crate::transport::Transport, address: String, max: u8, door: Arc<Door>, host: u128, data: PathBuf) -> Host {
        let cfg = Config { dev: true, ..Self::config(max, &door, data) };
        Host::start(cfg, t, address, door, host)
    }

    fn start(cfg: Config, t: impl crate::transport::Transport, address: String, door: Arc<Door>, host: u128) -> Host {
        // players' character lists are a lobby's own: none carried over from an earlier one
        let _ = std::fs::remove_dir_all(cfg.data.join("accounts"));
        Host { server: Some(Server::start(cfg, t)), door, address, host, keep: None }
    }

    /// `what` lives until the server is gone (a game server the lobby runs on).
    pub fn keeping(mut self, what: Box<dyn Send + Sync>) -> Host {
        self.keep = Some(what);
        self
    }

    /// Connections open now, logged in or not.
    pub fn connections(&self) -> usize {
        self.server.as_ref().map_or(0, Server::connections)
    }

    /// What players join: `steam:<id>`, or `127.0.0.1:<port>` for the dev path.
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn door(&self) -> &Door {
        &self.door
    }

    /// Everyone in the lobby but the host: (session, name, Steam id or 0), by session.
    pub fn members(&self) -> Vec<(u32, String, u64)> {
        let list = self.server.as_ref().map(Server::list).unwrap_or_default();
        list.into_iter()
            .filter(|e| e.2 != self.host)
            .map(|(sid, name, key)| (sid, name, if let PlayerId::Steam(id) = player_id(key) { id } else { 0 }))
            .collect()
    }

    /// Removes a player: told why, out of the world, kept out until the lobby closes. False when there is no such
    /// player (the host itself is never removed).
    pub fn kick(&self, session: u32) -> bool {
        let Some(server) = &self.server else { return false };
        if !server.list().iter().any(|e| e.0 == session && e.2 != self.host) {
            return false;
        }
        match server.end(session, Msg::Refuse { code: refuse::CLOSED, reason: REMOVED.into() }) {
            Some(key) => {
                self.door.remove(key);
                true
            }
            None => false,
        }
    }

    /// Closes the lobby: everyone is told the host closed it (their games stop retrying), then the server stops. Waits
    /// up to a second for every connection to pass the word on and end. The connections still open then (0: all
    /// drained).
    pub fn close(mut self) -> usize {
        self.shut(false)
    }

    /// `here`: the last half second (`LINGER`) on this thread, not a short-lived one (the game is quitting anyway).
    fn shut(&mut self, here: bool) -> usize {
        let Some(server) = self.server.take() else { return 0 };
        // new logins are told it closed (not a bare Bye) while the sessions end
        self.door.close();
        for (sid, _, _) in server.list() {
            server.end(sid, Msg::Refuse { code: refuse::CLOSED, reason: CLOSED.into() });
        }
        // each connection passes its refusal on at its next tick (25 ms) and ends; logins in the handshake end at the door
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until && server.connections() > 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        let left = server.connections();
        // Steam sends a closed connection's last frames while it lingers, but only while the listen socket and the game
        // server are up: they stay a moment longer (the process's game-server slot covers that), off the game's thread
        let linger = move || {
            std::thread::sleep(LINGER);
            drop(server);
        };
        if here {
            linger();
        } else if let Err(e) = std::thread::Builder::new().name("redex-lobby-close".into()).spawn(linger) {
            eprintln!("redex-world: lobby close thread: {e}"); // the closure, and the server, dropped with it
        }
        left
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shut(true);
    }
}

/// How long a closed lobby's server stays up after its last connection closed, so their last frames go out.
const LINGER: Duration = Duration::from_millis(500);

/// A UDP port nobody uses now (for a listed lobby's query port).
#[cfg(feature = "steam")]
fn free_udp_port() -> Option<u16> {
    std::net::UdpSocket::bind("0.0.0.0:0").ok()?.local_addr().ok().map(|a| a.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: u64 = 76_561_197_960_287_930;
    const FRIEND: u64 = 76_561_197_960_287_931;
    const STRANGER: u64 = 76_561_197_960_287_932;

    /// Friends-only lets in the host and its Steam friends only (dev ids have none); code-only and public anyone; a
    /// removed player stays out, as a friend too, and hears why.
    #[test]
    fn the_door_lets_in_friends_and_keeps_removed_players_out() {
        let d = Door::new(steam_key(HOST), true);
        assert!(d.check(steam_key(HOST), 0).is_ok(), "the host, before any friends list");
        assert_eq!(d.check(steam_key(FRIEND), 0), Err((refuse::AUTH, FRIENDS_ONLY)));
        d.set_friends([steam_key(FRIEND)]);
        assert!(d.check(steam_key(FRIEND), 0).is_ok());
        assert!(d.check(steam_key(STRANGER), 0).is_err());
        assert!(d.check(dev_key(7), 0).is_err(), "a dev id is nobody's friend");
        // a SteamID64 as a dev id's key is not that account
        assert!(d.check(u128::from(FRIEND), 0).is_err());
        d.remove(steam_key(FRIEND));
        assert_eq!(d.check(steam_key(FRIEND), 0), Err((refuse::CLOSED, REMOVED)));

        let open = Door::new(steam_key(HOST), false);
        open.set_friends([steam_key(FRIEND)]); // ignored: not friends-only
        assert!(open.check(steam_key(STRANGER), 0).is_ok() && open.check(dev_key(7), 0).is_ok());
        open.remove(dev_key(7));
        assert!(open.check(dev_key(7), 0).is_err() && open.check(dev_key(8), 0).is_ok());

        // the size: with the host 4 at most; the host always comes in; closing tells everyone
        open.set_max(4);
        assert!(open.check(dev_key(8), 2).is_ok());
        assert_eq!(open.check(dev_key(8), 3), Err((refuse::FULL, FULL)));
        assert!(open.check(steam_key(HOST), 3).is_ok() && open.check(steam_key(HOST), 9).is_ok(), "the host's seat is kept");
        open.close();
        assert_eq!(open.check(steam_key(HOST), 0), Err((refuse::CLOSED, CLOSED)));

        // a full friends-only lobby: a stranger is refused for good (not "full", which a game retries)
        let full = Door::new(steam_key(HOST), true);
        full.set_max(2);
        full.set_friends([steam_key(FRIEND)]);
        assert_eq!(full.check(steam_key(STRANGER), 1), Err((refuse::AUTH, FRIENDS_ONLY)));
        assert_eq!(full.check(steam_key(FRIEND), 1), Err((refuse::FULL, FULL)));
        // before Hello: the host past every limit, a stranger refused at once, a friend let try
        assert_eq!(full.early(steam_key(HOST)), Early::Host);
        assert_eq!(full.early(steam_key(STRANGER)), Early::Refuse((refuse::AUTH, FRIENDS_ONLY)));
        assert_eq!(full.early(steam_key(FRIEND)), Early::Ok);
        full.remove(steam_key(FRIEND));
        assert_eq!(full.early(steam_key(FRIEND)), Early::Refuse((refuse::CLOSED, REMOVED)));
        full.close();
        assert_eq!(full.early(steam_key(HOST)), Early::Refuse((refuse::CLOSED, CLOSED)));
    }
}
