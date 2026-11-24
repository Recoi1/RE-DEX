//! Friends: the player's Steam friends who run RE:DEX, and chat and invites with them, game to game over Steam's
//! networking messages on their own channel (`FRIEND_CHANNEL`), relayed by Steam like the world connection. Never
//! through Steam's friend chat (it would show in the friend's Steam chat window) and never Steam's game invite (it
//! would launch Spacewar for anyone without RE:DEX). A session is accepted only from a Steam friend who isn't ignored;
//! frames from anyone else, past `RECEIVE_MAX` per `RECEIVE_WINDOW` per sender, or that don't decode are dropped.
//! `FakeFriends` (tests and `mp-tests` builds, `XR_FAKE_FRIENDS`) stands in for Steam.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use redex_proto::FriendMsg;
use redex_proto::{decode_friend, encode_friend};

/// The networking-messages channel friend frames travel on (the world connection uses sockets, not messages).
pub const FRIEND_CHANNEL: u32 = 7;
/// Frames taken from one friend at most per `RECEIVE_WINDOW`; more are dropped.
pub const RECEIVE_MAX: usize = 10;
pub const RECEIVE_WINDOW: Duration = Duration::from_secs(10);
/// The rich-presence key that says "running RE:DEX" (its value: the protocol version).
pub const PRESENCE_KEY: &str = "redex";

/// A Steam friend who is online.
#[derive(Debug, Clone, PartialEq)]
pub struct Friend {
    pub steam: u64,
    pub name: String,
    /// running RE:DEX now (app 480 with the `redex` presence key): only then can they be messaged or invited
    pub in_redex: bool,
    /// their presence's `status` (the world and where, or "Playing offline")
    pub status: Option<String>,
}

/// Steam friends, or fakes in tests. Called from the game's thread.
pub trait FriendsLink: Send {
    /// friends online now (from what Steam last heard)
    fn list(&self) -> Vec<Friend>;
    /// asks Steam for friends' presence again
    fn refresh(&self);
    /// a frame to a friend running RE:DEX; Err says why it didn't go (for the player)
    fn send(&self, to: u64, m: &FriendMsg) -> Result<(), String>;
    /// frames that came in and passed the gate, oldest first
    fn take(&self) -> Vec<(u64, FriendMsg)>;
    /// friends whose session Steam gave up on since the last call (what was sent lately may not have arrived)
    fn take_failed(&self) -> Vec<u64> {
        Vec::new()
    }
    /// the players whose sessions and frames are refused
    fn set_ignored(&self, ids: &[u64]);
    /// the run's frame (fakes deliver their scripted frames by it)
    fn frame(&self, _n: u64) {}
}

/// At most `max` in `window`: true (and counted) when one more may go now.
#[derive(Debug, Default)]
pub struct Limiter(VecDeque<Instant>);

impl Limiter {
    pub fn allow(&mut self, now: Instant, max: usize, window: Duration) -> bool {
        while self.0.front().is_some_and(|&t| now.duration_since(t) >= window) {
            self.0.pop_front();
        }
        if self.0.len() >= max {
            return false;
        }
        self.0.push_back(now);
        true
    }
}

/// Running this RE:DEX: the game is ours (`app`: app 480, or Steam can't say) and the `redex` key is this protocol's.
pub fn in_redex(app: bool, key: Option<&str>) -> bool {
    app && key == Some(redex_proto::VERSION.to_string().as_str())
}

/// Why Steam refused a send, for the player.
pub fn send_error(e: &steamworks::SteamError) -> String {
    use steamworks::SteamError as E;
    match e {
        E::LimitExceeded | E::RateLimitExceeded => "sending too fast; wait a moment".into(),
        E::NoConnection | E::ConnectFailed | E::Timeout => "the connection to them failed".into(),
        e => format!("Steam refused it ({e})"),
    }
}

fn not_reachable(friend: Option<&Friend>) -> String {
    match friend {
        Some(f) if f.status.is_some() && !f.in_redex => "runs another RE:DEX version".into(),
        Some(_) => "isn't running RE:DEX".into(),
        None => "isn't a Steam friend online".into(),
    }
}

/// The receive gate: a frame is taken only from a friend who isn't ignored, and at most `RECEIVE_MAX` per
/// `RECEIVE_WINDOW` from each (counted before decoding, so junk counts too).
#[derive(Debug, Default)]
pub struct Inbound(HashMap<u64, Limiter>);

impl Inbound {
    pub fn admit(&mut self, from: u64, friends: &HashSet<u64>, ignored: &HashSet<u64>, now: Instant) -> bool {
        friends.contains(&from) && !ignored.contains(&from) && self.0.entry(from).or_default().allow(now, RECEIVE_MAX, RECEIVE_WINDOW)
    }
}

/// The real link: Steam's networking messages and friends list.
pub struct SteamFriends {
    steam: crate::steam::Steam,
    ignored: Arc<Mutex<HashSet<u64>>>,
    gate: Mutex<Inbound>,
    failed: Arc<Mutex<Vec<u64>>>,
}

impl SteamFriends {
    /// Starts taking friends' sessions (once per process: Steam keeps one session-request handler).
    pub fn new(steam: &crate::steam::Steam) -> SteamFriends {
        let ignored: Arc<Mutex<HashSet<u64>>> = Arc::default();
        let (client, refused) = (steam.client().clone(), ignored.clone());
        steam.client().networking_utils().init_relay_network_access();
        steam.client().networking_messages().session_request_callback(move |req| {
            let ok = req.remote().steam_id().is_some_and(|id| {
                !refused.lock().is_ok_and(|i| i.contains(&id.raw())) && client.friends().get_friend(id).has_friend(steamworks::FriendFlags::IMMEDIATE)
            });
            if ok {
                req.accept();
            } else {
                req.reject();
            }
        });
        // a session that never came up or broke: what was sent to them may be lost
        let failed: Arc<Mutex<Vec<u64>>> = Arc::default();
        let lost = failed.clone();
        steam.client().networking_messages().session_failed_callback(move |info| {
            let who = info.identity_remote().and_then(|i| i.steam_id()).map(|id| id.raw());
            log::warn!("friends: Steam gave up on the session with {who:?} ({:?})", info.end_reason());
            if let (Some(id), Ok(mut l)) = (who, lost.lock()) {
                l.push(id);
            }
        });
        SteamFriends { steam: steam.clone(), ignored, gate: Mutex::default(), failed }
    }

    fn friends(&self) -> Vec<steamworks::Friend> {
        self.steam.client().friends().get_friends(steamworks::FriendFlags::IMMEDIATE)
    }
}

impl FriendsLink for SteamFriends {
    fn list(&self) -> Vec<Friend> {
        self.friends()
            .into_iter()
            .filter(|f| f.state() != steamworks::FriendState::Offline)
            .map(|f| {
                // the key alone decides when Steam can't say which game they play
                let app = f.game_played().is_none_or(|g| g.game.app_id() == steamworks::AppId(crate::steam::APP_ID));
                let key = f.rich_presence(PRESENCE_KEY);
                let in_redex = in_redex(app, key.as_deref());
                let status = match () {
                    _ if in_redex => f.rich_presence("status").map(|s| crate::client::clean(&s, 63)),
                    _ if app && key.is_some() => Some("Another RE:DEX version".into()),
                    _ => None,
                };
                Friend { steam: f.id().raw(), name: crate::client::clean(&f.name(), 32), in_redex, status }
            })
            .collect()
    }

    fn refresh(&self) {
        use steamworks_sys as sys;
        for f in self.friends().iter().filter(|f| f.state() != steamworks::FriendState::Offline) {
            // SAFETY: Steam is initialised (self.steam holds its client); the call only queues a presence request
            unsafe { sys::SteamAPI_ISteamFriends_RequestFriendRichPresence(sys::SteamAPI_SteamFriends_v018(), f.id().raw()) };
        }
    }

    fn send(&self, to: u64, m: &FriendMsg) -> Result<(), String> {
        let list = self.list();
        let friend = list.iter().find(|f| f.steam == to);
        if !friend.is_some_and(|f| f.in_redex) {
            return Err(not_reachable(friend));
        }
        let to = steamworks::networking_types::NetworkingIdentity::new_steam_id(steamworks::SteamId::from_raw(to));
        let flags = steamworks::networking_types::SendFlags::RELIABLE | steamworks::networking_types::SendFlags::AUTO_RESTART_BROKEN_SESSION;
        self.steam.client().networking_messages().send_message_to_user(to, flags, &encode_friend(m), FRIEND_CHANNEL).map_err(|e| {
            log::warn!("friends: Steam refused a send: {e:?}");
            send_error(&e)
        })
    }

    fn take_failed(&self) -> Vec<u64> {
        self.failed.lock().map(|mut f| std::mem::take(&mut *f)).unwrap_or_default()
    }

    fn take(&self) -> Vec<(u64, FriendMsg)> {
        self.steam.pump();
        let got = self.steam.client().networking_messages().receive_messages_on_channel(FRIEND_CHANNEL, 32);
        if got.is_empty() {
            return Vec::new();
        }
        let friends: HashSet<u64> = self.friends().iter().map(|f| f.id().raw()).collect();
        let ignored = self.ignored.lock().map(|i| i.clone()).unwrap_or_default();
        let (now, mut gate) = (Instant::now(), self.gate.lock().unwrap_or_else(|e| e.into_inner()));
        got.iter()
            .filter_map(|msg| {
                let from = msg.identity_peer().steam_id()?.raw();
                gate.admit(from, &friends, &ignored, now).then(|| decode_friend(msg.data()).map(|m| (from, m))).flatten()
            })
            .collect()
    }

    fn set_ignored(&self, ids: &[u64]) {
        if let Ok(mut i) = self.ignored.lock() {
            *i = ids.iter().copied().collect();
        }
    }
}

/// Made-up friends: `XR_FAKE_FRIENDS=ID:NAME[:STATUS];...` (a STATUS: running RE:DEX; none: online in Steam only) and
/// `XR_FAKE_FRIEND_SAYS=FRAME:ID:chat:TEXT|FRAME:ID:invite:steam:<id> WORLD|FRAME:ID:decline;...`, frames that arrive
/// at that run frame. Sends are logged and kept (`sent`).
#[cfg(any(test, feature = "mp-tests"))]
#[derive(Default)]
pub struct FakeFriends {
    friends: Vec<Friend>,
    script: Mutex<Vec<(u64, u64, FriendMsg)>>,
    inbox: Mutex<Vec<(u64, FriendMsg)>>,
    sent: Mutex<Vec<(u64, FriendMsg)>>,
    failed: Mutex<Vec<u64>>,
    ignored: Mutex<HashSet<u64>>,
    gate: Mutex<Inbound>,
}

#[cfg(any(test, feature = "mp-tests"))]
impl FakeFriends {
    pub fn from_env() -> Option<FakeFriends> {
        let f = FakeFriends::parse(&std::env::var("XR_FAKE_FRIENDS").ok()?);
        f.script(&std::env::var("XR_FAKE_FRIEND_SAYS").unwrap_or_default());
        Some(f)
    }

    /// `ID:NAME[:STATUS];...`; entries that don't parse are skipped
    pub fn parse(spec: &str) -> FakeFriends {
        let friends = spec
            .split(';')
            .filter_map(|e| {
                let mut p = e.trim().splitn(3, ':');
                let steam = p.next()?.parse().ok()?;
                let name = p.next()?.to_string();
                let status = p.next().filter(|s| !s.is_empty()).map(String::from);
                Some(Friend { steam, name, in_redex: status.is_some(), status })
            })
            .collect();
        FakeFriends { friends, ..FakeFriends::default() }
    }

    /// adds `FRAME:ID:KIND[:REST];...` frames to deliver (see the type)
    pub fn script(&self, spec: &str) {
        let parsed = spec.split(';').filter_map(|e| {
            let mut p = e.trim().splitn(4, ':');
            let (at, from, kind, rest) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?, p.next().unwrap_or(""));
            let m = match kind {
                "chat" => FriendMsg::Chat(rest.into()),
                "invite" => {
                    let (connect, world) = rest.split_once(' ').unwrap_or((rest, ""));
                    FriendMsg::Invite { world: world.into(), connect: connect.into() }
                }
                "decline" => FriendMsg::Decline,
                _ => return None,
            };
            Some((at, from, m))
        });
        if let Ok(mut s) = self.script.lock() {
            s.extend(parsed);
        }
    }

    /// a frame from `from`, through the same gate and codec as Steam's
    pub fn inject(&self, from: u64, m: FriendMsg) {
        let friends: HashSet<u64> = self.friends.iter().map(|f| f.steam).collect();
        let ignored = self.ignored.lock().map(|i| i.clone()).unwrap_or_default();
        let ok = self.gate.lock().is_ok_and(|mut g| g.admit(from, &friends, &ignored, Instant::now()));
        if let (true, Some(m), Ok(mut inbox)) = (ok, decode_friend(&encode_friend(&m)), self.inbox.lock()) {
            inbox.push((from, m));
        }
    }

    /// Steam gives up on the session with `peer`
    pub fn fail(&self, peer: u64) {
        if let Ok(mut f) = self.failed.lock() {
            f.push(peer);
        }
    }

    /// what was sent, oldest first
    pub fn sent(&self) -> Vec<(u64, FriendMsg)> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[cfg(any(test, feature = "mp-tests"))]
impl FriendsLink for FakeFriends {
    fn list(&self) -> Vec<Friend> {
        self.friends.clone()
    }
    fn refresh(&self) {}
    fn send(&self, to: u64, m: &FriendMsg) -> Result<(), String> {
        let friend = self.friends.iter().find(|f| f.steam == to);
        if !friend.is_some_and(|f| f.in_redex) {
            return Err(not_reachable(friend));
        }
        log::info!("friends (fake): sent to {to}: {m:?}");
        self.sent.lock().map(|mut s| s.push((to, m.clone()))).map_err(|_| "the fake's log is gone".into())
    }
    fn take_failed(&self) -> Vec<u64> {
        self.failed.lock().map(|mut f| std::mem::take(&mut *f)).unwrap_or_default()
    }
    fn take(&self) -> Vec<(u64, FriendMsg)> {
        self.inbox.lock().map(|mut i| std::mem::take(&mut *i)).unwrap_or_default()
    }
    fn set_ignored(&self, ids: &[u64]) {
        if let Ok(mut i) = self.ignored.lock() {
            *i = ids.iter().copied().collect();
        }
    }
    fn frame(&self, n: u64) {
        let due: Vec<_> = match self.script.lock() {
            Ok(mut s) => {
                s.sort_by_key(|e| e.0);
                let k = s.iter().take_while(|e| e.0 <= n).count();
                s.drain(..k).collect()
            }
            Err(_) => return,
        };
        for (_, from, m) in due {
            self.inject(from, m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERENA: u64 = 76_561_197_960_287_934;
    const CALEM: u64 = 76_561_197_960_287_935;
    const STRANGER: u64 = 76_561_197_960_287_999;

    fn fake() -> FakeFriends {
        FakeFriends::parse(&format!("{SERENA}:Serena:In Kalos;{CALEM}:Calem"))
    }

    #[test]
    fn only_redex_friends_are_reachable() {
        let f = fake();
        let list = f.list();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|x| x.steam == SERENA && x.in_redex && x.status.as_deref() == Some("In Kalos")));
        assert!(list.iter().any(|x| x.steam == CALEM && !x.in_redex && x.status.is_none()), "no status: online in Steam, not in RE:DEX");
        let hi = FriendMsg::Chat("hi".into());
        assert_eq!(f.send(SERENA, &hi), Ok(()));
        assert_eq!(f.send(CALEM, &hi), Err("isn't running RE:DEX".into()), "not running RE:DEX");
        assert_eq!(f.send(STRANGER, &hi), Err("isn't a Steam friend online".into()), "not a friend");
        assert_eq!(f.sent(), [(SERENA, hi)]);
    }

    /// A send that fails says why, by cause; a session Steam gave up on is reported once.
    #[test]
    fn send_errors_name_the_cause() {
        assert!(send_error(&steamworks::SteamError::LimitExceeded).contains("too fast"));
        assert!(send_error(&steamworks::SteamError::RateLimitExceeded).contains("too fast"));
        assert!(send_error(&steamworks::SteamError::NoConnection).contains("connection"));
        assert!(send_error(&steamworks::SteamError::Busy).contains("Steam"));
        let f = fake();
        f.fail(SERENA);
        assert_eq!(f.take_failed(), [SERENA]);
        assert!(f.take_failed().is_empty());
    }

    /// The `redex` key must hold this protocol's version: another version's frames would be dropped on both sides.
    #[test]
    fn another_version_is_not_in_redex() {
        let v = redex_proto::VERSION.to_string();
        assert!(in_redex(true, Some(&v)));
        assert!(!in_redex(true, Some("4")));
        assert!(!in_redex(true, None));
        assert!(!in_redex(false, Some(&v)), "another app");
    }

    #[test]
    fn ignored_sender_dropped() {
        let f = fake();
        f.inject(SERENA, FriendMsg::Chat("one".into()));
        f.set_ignored(&[SERENA]);
        f.inject(SERENA, FriendMsg::Chat("two".into()));
        f.inject(SERENA, FriendMsg::Invite { world: "w".into(), connect: "steam:1".into() });
        assert_eq!(f.take(), [(SERENA, FriendMsg::Chat("one".into()))]);
        f.set_ignored(&[]);
        f.inject(SERENA, FriendMsg::Decline);
        assert_eq!(f.take(), [(SERENA, FriendMsg::Decline)]);
    }

    #[test]
    fn receive_rate_limit_10_per_10s() {
        let friends = HashSet::from([SERENA, CALEM]);
        let none = HashSet::new();
        let mut gate = Inbound::default();
        let t0 = Instant::now();
        assert!((0..10).all(|i| gate.admit(SERENA, &friends, &none, t0 + Duration::from_millis(i * 100))));
        assert!(!gate.admit(SERENA, &friends, &none, t0 + Duration::from_secs(9)), "an 11th inside 10 s");
        assert!(gate.admit(CALEM, &friends, &none, t0 + Duration::from_secs(9)), "per sender");
        assert!(gate.admit(SERENA, &friends, &none, t0 + Duration::from_secs(10)), "the first one's 10 s are up");
        // through a fake: the 11th of a burst never reaches take()
        let f = fake();
        for i in 0..12 {
            f.inject(SERENA, FriendMsg::Chat(i.to_string()));
        }
        assert_eq!(f.take().len(), RECEIVE_MAX);
    }

    #[test]
    fn non_friend_frames_dropped() {
        let friends = HashSet::from([SERENA]);
        let mut gate = Inbound::default();
        let now = Instant::now();
        assert!(!gate.admit(STRANGER, &friends, &HashSet::new(), now));
        assert!(!gate.admit(SERENA, &friends, &HashSet::from([SERENA]), now), "ignored");
        assert!(gate.admit(SERENA, &friends, &HashSet::new(), now));
        let f = fake();
        f.inject(STRANGER, FriendMsg::Chat("hi".into()));
        assert!(f.take().is_empty());
    }

    #[test]
    fn fake_scripts_parse() {
        let f = FakeFriends::parse(&format!("{SERENA}:Serena:Playing offline; junk ;:x;{CALEM}:Calem:"));
        assert_eq!(f.list().iter().map(|x| (x.steam, x.in_redex)).collect::<Vec<_>>(), [(SERENA, true), (CALEM, false)]);
        f.script(&format!("30:{SERENA}:chat:hello there;10:{SERENA}:invite:steam:42 My World;50:{SERENA}:decline;x:y"));
        f.frame(20);
        assert_eq!(f.take(), [(SERENA, FriendMsg::Invite { world: "My World".into(), connect: "steam:42".into() })]);
        f.frame(60);
        assert_eq!(f.take(), [(SERENA, FriendMsg::Chat("hello there".into())), (SERENA, FriendMsg::Decline)]);
    }
}
