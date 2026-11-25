//! "Host lobby": the running offline game becomes a lobby's host without a restart. The world server starts in this
//! process (`redex_world::lobby::Host`, on its own thread: Steam's game server takes up to 20 s to log on), the game
//! then joins its own world like any other (the player stays where they are), and a room code goes up as a Steam lobby
//! (`xr_net::steam::Steam::open_room`) so friends find it by code with no server of ours. Closing the lobby, or the
//! game, tells everyone in it before the server stops (`Host`'s drop).
//!
//! The state machine: Off -> Starting (server coming up) -> Open (code published, players come and go, the host can
//! remove them) -> Off (closed by the host, or the game quitting). Players in it see the host close it as the world's
//! lasting refusal (`refuse::CLOSED`), which ends their stay there.
//!
//! In mp-tests builds a dev-id game hosts on TCP 127.0.0.1 instead (`Host::dev`) and publishes its code as a file in
//! `XR_FAKE_ROOMS` (a dev input like `XR_FAKE_FRIENDS`): the pair tests run two games on one PC without Steam.
use redex_world::lobby::{Door, Host};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};
use xr_frontend::online::{Access, LobbySettings, LobbyView};
use xr_net::steam::{Answer, Steam};

/// How often a friends-only lobby reads the host's Steam friends again (a friend added meanwhile can come in).
const FRIENDS_AGAIN: Duration = Duration::from_secs(30);

/// Who hosts, and where the code goes: Steam (P2P relays, a Steam lobby for the code), or a dev id (TCP on this PC, the
/// code a file).
pub enum How {
    Steam(Steam),
    #[cfg(feature = "mp-tests")]
    Dev { id: u128, rooms: Option<PathBuf> },
    /// tests: a dev lobby whose start takes the process's game-server slot and `delay` (Steam's logon), with a fake
    /// game server whose drop shuts down whichever is up (as `SteamGameServer_Shutdown` does)
    #[cfg(all(test, feature = "mp-tests"))]
    Fake { id: u128, delay: Duration },
}

#[cfg(all(test, feature = "mp-tests"))]
pub mod fake {
    use std::sync::atomic::{AtomicBool, Ordering};
    /// a fake game server is up (one per process, like Steam's)
    pub static ALIVE: AtomicBool = AtomicBool::new(false);
    pub struct Server {
        _slot: redex_world::steam::InUse,
    }
    impl Server {
        pub fn up(slot: redex_world::steam::InUse) -> Server {
            ALIVE.store(true, Ordering::SeqCst);
            Server { _slot: slot }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            // global, whichever server is up: the hazard the slot guards against
            ALIVE.store(false, Ordering::SeqCst);
        }
    }
}

/// What happened since the last `poll`.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// The server is up: join it (`address`); `code` is the room code friends type.
    Opened { address: String, code: String },
    /// It did not start (why, for the player); the lobby is Off again.
    Failed(String),
}

enum Phase {
    Off,
    Starting { settings: LobbySettings, how: How, rx: Receiver<Result<Host, String>> },
    Open(Open),
}

struct Open {
    host: Host,
    settings: LobbySettings,
    code: String,
    how: How,
    /// the Steam lobby carrying the code
    room: Option<xr_net::steam::Room>,
    friends_at: Option<Instant>,
}

/// The game's lobby, if it hosts one.
pub struct Lobby {
    phase: Phase,
    /// random bits for the next code
    random: fn() -> u64,
}

impl Default for Lobby {
    fn default() -> Lobby {
        Lobby { phase: Phase::Off, random: random_u64 }
    }
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    // shortcut: without OS randomness the time is used (a code only needs to differ from other open ones)
    if getrandom::fill(&mut b).is_err() {
        b = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()) as u64).to_le_bytes();
    }
    u64::from_le_bytes(b)
}

/// The settings as a host may have them: a name (the default when empty, 32 characters at most, cleaned like a
/// player's) and 2 to 8 players.
pub fn checked(s: &LobbySettings, fallback_name: &str) -> LobbySettings {
    let name = xr_net::client::clean(s.name.trim(), 32);
    let name = if name.is_empty() { format!("{}'s lobby", xr_net::client::clean(fallback_name, 16)) } else { name };
    LobbySettings { name, max: s.max.clamp(redex_world::lobby::MIN_PLAYERS, redex_world::lobby::MAX_PLAYERS), access: s.access }
}

impl Lobby {
    pub fn hosting(&self) -> bool {
        !matches!(self.phase, Phase::Off)
    }

    /// The lobby's world address once it is open (what the host's own game joins).
    pub fn address(&self) -> Option<&str> {
        match &self.phase {
            Phase::Open(o) => Some(o.host.address()),
            _ => None,
        }
    }

    /// Starts hosting (Off -> Starting). `data`: the world's own folder. Err when already hosting.
    pub fn host(&mut self, settings: LobbySettings, how: How, data: PathBuf) -> Result<(), String> {
        if self.hosting() {
            return Err("You are hosting a lobby already".into());
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (max, access) = (settings.max, settings.access);
        let start: Box<dyn FnOnce() -> Result<Host, String> + Send> = match &how {
            How::Steam(steam) => {
                // one game server per process, taken here at once: an earlier lobby's may still be going (closed while
                // it started, or a login in flight), and a second would be shut down when the first goes
                let slot = redex_world::steam::InUse::take()?;
                let me = redex_world::lobby::steam_key(steam.steam_id());
                let name = settings.name.clone();
                Box::new(move || Host::steam(slot, &name, max, access == Access::Public, Arc::new(Door::new(me, access == Access::Friends)), me, data))
            }
            #[cfg(feature = "mp-tests")]
            How::Dev { id, .. } => {
                let me = redex_world::lobby::dev_key(*id);
                Box::new(move || Host::dev(max, Arc::new(Door::new(me, access == Access::Friends)), me, data).map_err(|e| e.to_string()))
            }
            #[cfg(all(test, feature = "mp-tests"))]
            How::Fake { id, delay } => {
                let (me, delay) = (redex_world::lobby::dev_key(*id), *delay);
                let slot = redex_world::steam::InUse::take()?;
                Box::new(move || {
                    std::thread::sleep(delay);
                    let server = fake::Server::up(slot);
                    let h = Host::dev(max, Arc::new(Door::new(me, false)), me, data).map_err(|e| e.to_string())?;
                    Ok(h.keeping(Box::new(server)))
                })
            }
        };
        std::thread::Builder::new()
            .name("redex-lobby-start".into())
            .spawn(move || {
                let _ = tx.send(start());
            })
            .map_err(|e| format!("could not start the lobby: {e}"))?;
        self.phase = Phase::Starting { settings, how, rx };
        Ok(())
    }

    /// Moves the lobby on: the server that came up (its code published), the friends list of a friends-only lobby.
    pub fn poll(&mut self) -> Option<Event> {
        match &mut self.phase {
            Phase::Off => None,
            Phase::Starting { rx, .. } => {
                let got = match rx.try_recv() {
                    Ok(r) => r,
                    Err(TryRecvError::Empty) => return None,
                    Err(TryRecvError::Disconnected) => Err("the lobby's server did not start".into()),
                };
                let Phase::Starting { settings, how, .. } = std::mem::replace(&mut self.phase, Phase::Off) else { unreachable!() };
                let host = match got {
                    Ok(h) => h,
                    Err(e) => return Some(Event::Failed(e)),
                };
                let code = redex_proto::room::new((self.random)());
                let address = host.address().to_owned();
                let room = match &how {
                    How::Steam(s) => Some(s.open_room(&code, &address, &settings.name, settings.max)),
                    #[cfg(feature = "mp-tests")]
                    How::Dev { rooms, .. } => {
                        if let Some(dir) = rooms {
                            let _ = std::fs::create_dir_all(dir);
                            if let Err(e) = std::fs::write(dir.join(&code), &address) {
                                crate::log::log(&format!("lobby: the fake room {code} was not written: {e}"));
                            }
                        }
                        None
                    }
                    #[cfg(all(test, feature = "mp-tests"))]
                    How::Fake { .. } => None,
                };
                self.phase = Phase::Open(Open { host, settings, code: code.clone(), how, room, friends_at: None });
                Some(Event::Opened { address, code })
            }
            Phase::Open(o) => {
                if let (How::Steam(s), Access::Friends) = (&o.how, o.settings.access) {
                    if o.friends_at.is_none_or(|t| t.elapsed() >= FRIENDS_AGAIN) {
                        o.host.door().set_friends(s.friend_ids().into_iter().map(redex_world::lobby::steam_key));
                        o.friends_at = Some(Instant::now());
                    }
                }
                // Steam could not make the code's lobby: said once, the lobby itself goes on (invites still work)
                if let Some(Some(Err(e))) = o.room.as_ref().and_then(|r| r.answer.lock().ok().map(|mut a| a.take_if(|a| a.is_err()))) {
                    crate::log::log(&format!("lobby: room code {} not published: {e}", o.code));
                    o.room = None;
                }
                None
            }
        }
    }

    /// Connections to the lobby's server now (tests).
    #[cfg(test)]
    fn connections(&self) -> usize {
        match &self.phase {
            Phase::Open(o) => o.host.connections(),
            _ => 0,
        }
    }

    /// Removes a player (told why, kept out until the lobby closes). False when there is no such player.
    pub fn kick(&self, session: u32) -> bool {
        matches!(&self.phase, Phase::Open(o) if o.host.kick(session))
    }

    /// Closes the lobby (everyone in it told) and its room code. False when there was none.
    pub fn close(&mut self) -> bool {
        self.end(false)
    }

    /// `close`; `quitting`: the server's last half second is spent here (`Host`'s own drop), not on a thread the
    /// process exit would cut short.
    fn end(&mut self, quitting: bool) -> bool {
        match std::mem::replace(&mut self.phase, Phase::Off) {
            Phase::Off => false,
            // the server thread's Host, when it comes, is dropped with the channel (closing it)
            Phase::Starting { .. } => true,
            Phase::Open(o) => {
                match (&o.how, &o.room) {
                    // also when Steam has not made it yet: it is left as soon as it is
                    (How::Steam(s), Some(room)) => s.close_room(room),
                    #[cfg(feature = "mp-tests")]
                    (How::Dev { rooms: Some(dir), .. }, _) => drop(std::fs::remove_file(dir.join(&o.code))),
                    _ => {}
                }
                if quitting {
                    drop(o.host);
                } else {
                    o.host.close();
                }
                true
            }
        }
    }

    /// What the overlay shows.
    pub fn view(&self) -> Option<LobbyView> {
        match &self.phase {
            Phase::Off => None,
            Phase::Starting { settings, .. } => Some(LobbyView { code: String::new(), settings: settings.clone(), members: Vec::new(), open: false }),
            Phase::Open(o) => Some(LobbyView {
                code: o.code.clone(),
                settings: o.settings.clone(),
                members: o.host.members().into_iter().map(|(sid, name, _)| (sid, name)).collect(),
                open: true,
            }),
        }
    }
}

impl Drop for Lobby {
    /// The game is quitting: everyone in the lobby is told, and the word goes out before the process ends.
    fn drop(&mut self) {
        self.end(true);
    }
}

/// The fake room codes' folder (mp-tests builds: `XR_FAKE_ROOMS`).
pub fn fake_rooms() -> Option<PathBuf> {
    #[cfg(feature = "mp-tests")]
    return std::env::var_os("XR_FAKE_ROOMS").map(PathBuf::from);
    #[cfg(not(feature = "mp-tests"))]
    None
}

/// Looks a typed room code up: through Steam's lobbies, or (mp-tests) the fake rooms' folder. The answer comes later
/// (Steam's callbacks), or now; Err when the text is no code or nothing can look it up.
pub fn find(typed: &str, steam: Option<&Steam>, fake: Option<&Path>) -> Result<(String, Answer<String>), String> {
    let code = redex_proto::room::parse(typed).map_err(str::to_owned)?;
    if let Some(dir) = fake {
        let got = std::fs::read_to_string(dir.join(&code)).map(|a| a.trim().to_owned()).map_err(|_| xr_net::steam::NO_ROOM.to_owned());
        return Ok((code, Arc::new(std::sync::Mutex::new(Some(got)))));
    }
    let steam = steam.ok_or_else(|| format!("{} to join by room code", xr_net::steam::START_STEAM))?;
    Ok((code.clone(), steam.find_room(&code)))
}

/// The answer to a `find`, once there is one (taken).
pub fn answered(a: &Answer<String>) -> Option<Result<String, String>> {
    a.lock().ok().and_then(|mut a| a.take())
}

/// Why this game may not host or join a lobby now, if it may not: lobbies take this PC's (offline) characters only, so
/// a game playing an account's online character stays out.
pub fn local_only(account: bool) -> Option<&'static str> {
    account.then_some("Lobbies use characters on this PC: play one from \"On this PC\" to host or join a lobby (your online characters stay on the official server)")
}

/// The real Steam path on one PC (the Steam client running and logged in): `XR_REAL_STEAM=1 cargo test -p xr-runtime
/// --bins -- --ignored a_real_steam_lobby` (steam_api64.dll on the PATH). The lobby's Steam game server starts in this
/// process, our own client joins it over Steam's relays as the overlay's Host lobby does, the room code resolves through
/// Steam's lobby list, and closing tells our client. A second account (another PC) is what this cannot show.
#[cfg(test)]
mod steam_check {
    use super::*;

    fn wait<T>(steam: &Steam, what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
        let until = Instant::now() + Duration::from_secs(secs);
        loop {
            steam.pump();
            if let Some(t) = f() {
                return t;
            }
            assert!(Instant::now() < until, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// xr-net's `lobby:` and `world:` log lines on stdout (the member counts Steam reports for a code's lobbies).
    struct Print;
    impl log::Log for Print {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            let line = r.args().to_string();
            if line.starts_with("lobby:") || line.starts_with("world:") {
                println!("  log: {line}");
            }
        }
        fn flush(&self) {}
    }
    static PRINT: Print = Print;

    /// Hosts a friends-only lobby over Steam and joins it with our own client over Steam's relays.
    fn open_and_join(steam: &Steam, dir: &Path) -> (Lobby, xr_net::Client, String, String) {
        let mut lobby = Lobby::default();
        let s = LobbySettings { name: "RE:DEX lobby check".into(), max: 2, access: Access::Friends };
        // a previous lobby's game server may still be closing (one per process): `still closing` until it has
        let until = Instant::now() + Duration::from_secs(30);
        let mut refused = 0;
        loop {
            match lobby.host(s.clone(), How::Steam(steam.clone()), dir.to_path_buf()) {
                Ok(()) => break,
                Err(e) => {
                    assert_eq!(e, redex_world::steam::STILL_CLOSING);
                    assert!(Instant::now() < until, "the last lobby's game server never let go");
                    refused += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        if refused > 0 {
            println!("hosting again: refused {refused} time(s) while the last lobby's game server was closing");
        }
        let t0 = Instant::now();
        let Event::Opened { address, code } = wait(steam, "the lobby's game server", 40, || lobby.poll()) else { panic!("did not open") };
        println!("lobby open on {address} after {:?}, room code {code}", t0.elapsed());
        assert!(redex_proto::steam_world(&address));
        let me = steam.steam_id();
        let opts = xr_net::Options { steam: Some(steam.clone()), ..xr_net::Options::new(&address, redex_proto::Auth::SteamTicket { steam_id: me, ticket: Vec::new() }, redex_proto::TITLE_X, "check") };
        let mut c = xr_net::Client::start(opts).unwrap();
        let t1 = Instant::now();
        wait(steam, "our own login over Steam P2P", 40, || {
            c.frame(None);
            lobby.poll();
            assert!(!c.stopped, "refused: {:?}", c.refused);
            c.online.then_some(())
        });
        println!("our client logged in over Steam P2P after {:?}; members besides the host: {:?}", t1.elapsed(), lobby.view().map(|v| v.members));
        (lobby, c, address, code)
    }

    fn told(steam: &Steam, c: &mut xr_net::Client, what: &str) -> Option<String> {
        let t = Instant::now();
        let why = wait(steam, what, 15, || {
            c.frame(None);
            c.stopped.then(|| c.refused.clone())
        });
        println!("{what}: {why:?} after {:?}", t.elapsed());
        why
    }

    #[test]
    #[ignore = "needs the Steam client (XR_REAL_STEAM=1)"]
    fn a_real_steam_lobby() {
        if std::env::var("XR_REAL_STEAM").as_deref() != Ok("1") {
            return;
        }
        let _ = log::set_logger(&PRINT).map(|()| log::set_max_level(log::LevelFilter::Info));
        let steam = Steam::init().expect("the Steam client, running and logged in");
        // the friends-only door reads these (counted only: no ids printed)
        println!("Steam friends the friends-only door would let in: {}", steam.friend_ids().len());
        let dir = std::env::temp_dir().join(format!("redex-lobby-steam-{}", std::process::id()));

        // 1. host, join over the relays, the room code through Steam's lobby list
        let (mut lobby, mut c, address, code) = open_and_join(&steam, &dir);
        let t2 = Instant::now();
        let found = wait(&steam, "the room code through Steam's lobby list", 60, || {
            let (_, a) = find(&redex_proto::room::shown(&code), Some(&steam), None).unwrap();
            let r = wait(&steam, "Steam's answer", 20, || answered(&a));
            lobby.poll();
            c.frame(None);
            r.ok().or_else(|| {
                std::thread::sleep(Duration::from_secs(2));
                None
            })
        });
        println!("room code {code} -> {found} after {:?}", t2.elapsed());
        assert_eq!(found, address);

        // 2. the game quitting (Lobby's drop): the word reaches our client over Steam before the server goes
        drop(lobby);
        assert_eq!(told(&steam, &mut c, "told the lobby closed (quit)").as_deref(), Some(redex_world::lobby::CLOSED));
        drop(c);

        // 3. host again at once (one game server per process), then Close lobby (the linger off the game's thread)
        let (mut lobby, mut c, address2, _) = open_and_join(&steam, &dir);
        println!("the second lobby's address {} the first's", if address2 == address { "is" } else { "differs from" });
        assert!(lobby.close());
        assert_eq!(told(&steam, &mut c, "told the lobby closed (Close lobby)").as_deref(), Some(redex_world::lobby::CLOSED));
        // the code is gone with the lobby
        std::thread::sleep(Duration::from_secs(3));
        let (_, a) = find(&redex_proto::room::shown(&code), Some(&steam), None).unwrap();
        println!("the first code after both lobbies closed: {:?}", wait(&steam, "Steam's answer", 20, || answered(&a)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
#[cfg(all(test, feature = "mp-tests"))]
mod tests {
    use super::*;
    use redex_proto::{Auth, TITLE_X};

    fn wait<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(t) = f() {
                return t;
            }
            assert!(Instant::now() < until, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn client(addr: &str, id: u128, name: &str) -> xr_net::Client {
        xr_net::Client::start(xr_net::Options::new(addr, Auth::DevId(id), TITLE_X, name)).unwrap()
    }

    fn online(c: &mut xr_net::Client) -> bool {
        c.frame(None);
        c.online
    }

    fn refused(c: &mut xr_net::Client) -> Option<String> {
        c.frame(None);
        c.stopped.then(|| c.refused.clone().unwrap_or_default())
    }

    fn settings(max: u8, access: Access) -> LobbySettings {
        LobbySettings { name: "Test lobby".into(), max, access }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redex-lobby-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Off -> Starting -> Open: the host's own game and a player join (by the code), the host removes the player, who is
    /// told and kept out; another player leaves on their own; the host quits and the one left is told.
    #[test]
    fn host_join_kick_leave_and_quit() {
        let dir = temp("sm");
        let rooms = dir.join("rooms");
        let mut lobby = Lobby::default();
        lobby.random = || 42;
        assert!(lobby.view().is_none() && lobby.address().is_none());
        lobby.host(settings(4, Access::Code), How::Dev { id: 1, rooms: Some(rooms.clone()) }, dir.join("world")).unwrap();
        assert!(lobby.host(settings(4, Access::Code), How::Dev { id: 1, rooms: None }, dir.join("w2")).is_err(), "one lobby at a time");
        assert_eq!(lobby.view().map(|v| v.open), Some(false), "starting");
        let Event::Opened { address, code } = wait("opened", || lobby.poll()) else { panic!("did not open") };
        assert_eq!(code, redex_proto::room::new(42));
        assert_eq!(lobby.address(), Some(address.as_str()));

        // the host's own game joins first: never a member, never removable
        let mut me = client(&address, 1, "Host");
        wait("host online", || online(&mut me).then_some(()));
        // a player joins by the code
        let (_, found) = find(&redex_proto::room::shown(&code).to_lowercase(), None, Some(&rooms)).unwrap();
        let at = answered(&found).unwrap().unwrap();
        assert_eq!(at, address);
        let mut ash = client(&at, 2, "Ash");
        let mut brock = client(&at, 3, "Brock");
        wait("players online", || (online(&mut ash) && online(&mut brock)).then_some(()));
        let members = wait("two members", || lobby.view().filter(|v| v.members.len() == 2));
        let mut names: Vec<&str> = members.members.iter().map(|m| m.1.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Ash", "Brock"], "the host is not its own member");

        // kick
        let ash_session = members.members.iter().find(|m| m.1 == "Ash").unwrap().0;
        assert!(!lobby.kick(9999));
        assert!(lobby.kick(ash_session));
        assert_eq!(wait("ash told", || refused(&mut ash)), redex_world::lobby::REMOVED);
        // a lasting refusal: Ash's game does not come back on its own (host and Brock are the only connections)
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(1500) {
            assert!(lobby.connections() <= 2, "a removed player reconnected");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut back = client(&at, 2, "Ash");
        assert_eq!(wait("ash kept out", || refused(&mut back)), redex_world::lobby::REMOVED);

        // leave: Brock's game drops its client
        drop(brock);
        wait("brock gone", || lobby.view().filter(|v| v.members.is_empty()));
        let mut misty = client(&at, 4, "Misty");
        wait("misty online", || online(&mut misty).then_some(()));

        // the host quits: everyone told, the code gone, the lobby Off
        assert!(lobby.close());
        assert_eq!(wait("misty told", || refused(&mut misty)), redex_world::lobby::CLOSED);
        assert_eq!(wait("host told", || refused(&mut me)), redex_world::lobby::CLOSED);
        assert!(lobby.view().is_none() && !lobby.close());
        assert_eq!(answered(&find(&code, None, Some(&rooms)).unwrap().1), Some(Err(xr_net::steam::NO_ROOM.into())));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The player closes a lobby while it starts (Steam's logon can take 20 s) and hosts again: the first start still
    /// holds the process's game server, so the second waits ("still closing") rather than start one the first would
    /// shut down when it goes; once it has, the new lobby comes up and stays up.
    #[test]
    fn a_lobby_closed_while_starting_never_takes_down_the_next() {
        let dir = temp("rehost");
        let mut lobby = Lobby::default();
        lobby.host(settings(4, Access::Code), How::Fake { id: 1, delay: Duration::from_millis(600) }, dir.join("w1")).unwrap();
        assert!(lobby.close(), "closed while it starts");
        let until = Instant::now() + Duration::from_secs(5);
        let mut waited = false;
        loop {
            match lobby.host(settings(4, Access::Code), How::Fake { id: 1, delay: Duration::ZERO }, dir.join("w2")) {
                Ok(()) => break,
                Err(e) => {
                    assert_eq!(e, redex_world::steam::STILL_CLOSING);
                    assert!(Instant::now() < until, "the first start never let go");
                    waited = true;
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        assert!(waited, "hosting again at once is refused while the first lobby still starts");
        let Event::Opened { address, .. } = wait("opened", || lobby.poll()) else { panic!("did not open") };
        let mut a = client(&address, 2, "A");
        wait("a online", || online(&mut a).then_some(()));
        std::thread::sleep(Duration::from_millis(1000));
        assert!(fake::ALIVE.load(std::sync::atomic::Ordering::SeqCst), "the new lobby's game server was shut down");
        assert!(online(&mut a) && !a.stopped, "the new lobby lost its player");
        assert!(lobby.close());
        assert!(!fake::ALIVE.load(std::sync::atomic::Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Dropping the game's lobby (the game closing) closes it the same way.
    #[test]
    fn a_game_that_quits_closes_its_lobby() {
        let dir = temp("drop");
        let mut lobby = Lobby::default();
        lobby.host(settings(2, Access::Code), How::Dev { id: 1, rooms: None }, dir.join("world")).unwrap();
        let Event::Opened { address, .. } = wait("opened", || lobby.poll()) else { panic!() };
        let mut a = client(&address, 5, "A");
        wait("a online", || online(&mut a).then_some(()));
        drop(lobby);
        // the last half second (its connections' last frames) was spent on the quitting thread, not a thread the
        // process exit would cut short: the server is gone already
        assert!(std::net::TcpStream::connect_timeout(&address.parse().unwrap(), Duration::from_millis(300)).is_err(), "the lobby still listens after its game quit");
        assert_eq!(wait("a told", || refused(&mut a)), redex_world::lobby::CLOSED);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_are_kept_in_range_and_named() {
        let s = checked(&LobbySettings { name: "  \u{202E}Hi\n ".into(), max: 99, access: Access::Public }, "Tobin");
        assert_eq!((s.name.as_str(), s.max, s.access), ("Hi", 8, Access::Public));
        let s = checked(&LobbySettings { name: String::new(), max: 0, access: Access::Friends }, "Tobin");
        assert_eq!((s.name.as_str(), s.max), ("Tobin's lobby", 2));
        assert!(local_only(true).is_some() && local_only(false).is_none());
        assert_eq!(find("ABC0EF", None, None).err().as_deref(), Some(redex_proto::room::BAD_CHAR));
        assert!(find(&redex_proto::room::new(1), None, None).unwrap_err().contains("Steam"));
    }
}
