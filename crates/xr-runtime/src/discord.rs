//! Discord Rich Presence over the local Discord app's IPC pipe (`discord-ipc-0` to `-9`): the game, the mode and the play
//! time on the player's profile. Own thread, one update at most every GAP; no Discord SDK, no network (only the Discord
//! app on this PC is spoken to). Discord not running: nothing happens and the pipe is tried again every RETRY.
//!
//! An open lobby (friends only or public) sets a join secret, its world's `steam:<id>`, and a party (the room code, players
//! of max): Discord then shows Join on the profile. A friend's Join reaches their running game as ACTIVITY_JOIN with that
//! secret (`Handle::take_joins`), which joins it like an invite.

use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The RE:DEX Discord application (public). Its Rich Presence assets (see `activity`) carry the art.
pub const APP_ID: &str = "1574992626707603597";

/// Discord allows 5 updates per 20 s; this sends at most one per 15 s.
const GAP: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Game {
    X,
    Y,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Offline,
    Official,
    P2p,
}

/// What the status says about the game right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub game: Game,
    pub mode: Mode,
    /// The current map's name, once the game's own text can be read cheaply (nothing is committed): none shows the region only.
    pub place: Option<String>,
    /// An open lobby's world (`steam:<id>`): set, it goes out as the activity's join secret, so Discord shows Join.
    pub join_secret: Option<String>,
    /// The lobby's party: (id, players, max), which Discord wants beside a join secret.
    pub party: Option<(String, u32, u32)>,
}

/// What an open lobby shows on Discord.
#[derive(Debug, Clone, PartialEq)]
pub struct Lobby {
    pub secret: String,
    pub party: String,
    pub size: u32,
    pub max: u32,
}

/// The settings that decide what is sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub status: bool,
    pub location: bool,
}

/// The activity to show, or none when the status is off (then nothing is sent, and a shown one is cleared).
pub fn activity(info: &Info, cfg: &Config, start: u64) -> Option<Value> {
    if !cfg.status {
        return None;
    }
    let (name, key) = match info.game {
        Game::X => ("Pok\u{e9}mon X", "x"),
        Game::Y => ("Pok\u{e9}mon Y", "y"),
    };
    let mode = match info.mode {
        Mode::Offline => "Offline",
        Mode::Official => "Official world",
        Mode::P2p => "P2P world",
    };
    let mut a = json!({
        "details": format!("{name} \u{b7} {mode}"),
        "timestamps": { "start": start },
        // assets uploaded to the Discord application: `logo` (large), `x` and `y` (small)
        "assets": { "large_image": "logo", "large_text": "RE:DEX", "small_image": key, "small_text": name },
    });
    if cfg.location {
        a["state"] = match &info.place {
            Some(p) => format!("In {p}"),
            None => "Exploring Kalos".to_string(),
        }
        .into();
    }
    if let (Some(secret), Some((id, size, max))) = (&info.join_secret, &info.party) {
        // Join needs both: the secret, and a party with room in it
        a["secrets"] = json!({ "join": secret });
        a["party"] = json!({ "id": id, "size": [size, max] });
    }
    Some(a)
}

// ---- the IPC protocol: u32 LE opcode, u32 LE length, JSON ----

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;

fn write_frame(w: &mut impl Write, op: u32, body: &Value) -> io::Result<()> {
    let json = body.to_string();
    let mut f = Vec::with_capacity(8 + json.len());
    f.extend_from_slice(&op.to_le_bytes());
    f.extend_from_slice(&(json.len() as u32).to_le_bytes());
    f.extend_from_slice(json.as_bytes());
    w.write_all(&f)?; // one write: a frame is never split
    w.flush()
}

fn read_frame(r: &mut impl Read) -> io::Result<(u32, Value)> {
    let mut h = [0u8; 8];
    r.read_exact(&mut h)?;
    let (op, len) = (u32::from_le_bytes(h[..4].try_into().unwrap()), u32::from_le_bytes(h[4..].try_into().unwrap()) as usize);
    if len > 64 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too long"));
    }
    let mut b = vec![0u8; len];
    r.read_exact(&mut b)?;
    Ok((op, serde_json::from_slice(&b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?))
}

fn handshake(s: &mut (impl Read + Write)) -> io::Result<()> {
    write_frame(s, OP_HANDSHAKE, &json!({ "v": 1, "client_id": APP_ID }))?;
    let (_, r) = read_frame(s)?;
    if r["evt"] == "READY" { Ok(()) } else { Err(io::Error::other(format!("no READY: {r}"))) }
}

/// A frame Discord sent on its own: a friend's Join (ACTIVITY_JOIN) goes to joins, anything else is dropped.
fn event(frame: &Value, joins: &Mutex<Vec<String>>) {
    if frame["evt"] == "ACTIVITY_JOIN" {
        if let Some(secret) = frame["data"]["secret"].as_str() {
            joins.lock().unwrap().push(secret.to_owned());
        }
    }
}

/// Discord's answer to the command with `nonce` (events before it go to `event`): an ERROR event is an error.
fn answer(s: &mut impl Read, nonce: u64, joins: &Mutex<Vec<String>>) -> io::Result<()> {
    loop {
        let (_, r) = read_frame(s)?;
        if r["nonce"].as_str() != Some(nonce.to_string().as_str()) {
            event(&r, joins);
            continue;
        }
        return if r["evt"] == "ERROR" { Err(io::Error::other(format!("Discord refused it: {}", r["data"]["message"]))) } else { Ok(()) };
    }
}

/// SET_ACTIVITY (none clears it) and Discord's answer.
fn set_activity(s: &mut (impl Read + Write), pid: u32, nonce: u64, activity: Option<&Value>, joins: &Mutex<Vec<String>>) -> io::Result<()> {
    let args = match activity {
        Some(a) => json!({ "pid": pid, "activity": a }),
        None => json!({ "pid": pid }),
    };
    write_frame(s, OP_FRAME, &json!({ "cmd": "SET_ACTIVITY", "args": args, "nonce": nonce.to_string() }))?;
    answer(s, nonce, joins)
}

/// Asks Discord for friends' Joins (ACTIVITY_JOIN), once per connection while a lobby is open.
fn subscribe_joins(s: &mut (impl Read + Write), nonce: u64, joins: &Mutex<Vec<String>>) -> io::Result<()> {
    write_frame(s, OP_FRAME, &json!({ "cmd": "SUBSCRIBE", "evt": "ACTIVITY_JOIN", "args": {}, "nonce": nonce.to_string() }))?;
    answer(s, nonce, joins)
}

/// A connection that can say whether Discord sent something (read without blocking).
pub trait Pipe: Read + Write {
    fn waiting(&self) -> bool;
}

#[cfg(windows)]
impl Pipe for std::fs::File {
    fn waiting(&self) -> bool {
        use std::os::windows::io::AsRawHandle;
        let mut avail = 0u32;
        // SAFETY: a valid pipe handle; only the count of bytes waiting is asked
        let ok = unsafe { windows_sys::Win32::System::Pipes::PeekNamedPipe(self.as_raw_handle() as _, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) };
        ok != 0 && avail > 0
    }
}

#[cfg(unix)]
impl Pipe for std::os::unix::net::UnixStream {
    fn waiting(&self) -> bool {
        let _ = self.set_nonblocking(true);
        let got = self.peek(&mut [0u8]).is_ok_and(|n| n > 0);
        let _ = self.set_nonblocking(false);
        got
    }
}

/// The first of Discord's pipes that opens.
#[cfg(windows)]
fn open_pipe() -> Option<std::fs::File> {
    (0..10).find_map(|i| std::fs::OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{i}")).ok())
}

#[cfg(unix)]
fn open_pipe() -> Option<std::os::unix::net::UnixStream> {
    let dir = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"].iter().find_map(|v| std::env::var(v).ok()).unwrap_or_else(|| "/tmp".into());
    (0..10).find_map(|i| std::os::unix::net::UnixStream::connect(format!("{dir}/discord-ipc-{i}")).ok())
}

struct Timing {
    gap: Duration,
    retry: Duration,
}

/// The presence thread's loop, until `stop` is dropped. `open` gives a fresh connection to Discord or none, `note` hears
/// what happened (for the log).
#[allow(clippy::too_many_arguments)]
fn run<S: Pipe>(
    open: impl Fn() -> Option<S>,
    shared: Arc<Mutex<Info>>,
    settings: impl Fn() -> Config,
    start: u64,
    timing: Timing,
    stop: mpsc::Receiver<()>,
    note: impl Fn(&str),
    joins: Arc<Mutex<Vec<String>>>,
) {
    let pid = std::process::id();
    let (mut conn, mut shown, mut nonce): (Option<S>, Option<Value>, u64) = (None, None, 0);
    // this connection asked for friends' Joins
    let mut subscribed = false;
    loop {
        let want = activity(&shared.lock().unwrap().clone(), &settings(), start);
        let mut wait = timing.gap;
        // off and nothing shown: Discord is not even asked
        if want.is_some() || conn.is_some() {
            if conn.is_none() {
                conn = open().and_then(|mut s| handshake(&mut s).ok().map(|_| s));
                (shown, subscribed) = (None, false);
                match conn {
                    Some(_) => note("discord: connected"),
                    None => wait = timing.retry,
                }
            }
            // every game, hosting or not: Discord sends a Join to the game of the player who pressed it
            if let Some(s) = conn.as_mut().filter(|_| !subscribed) {
                nonce += 1;
                subscribed = true; // once a connection; a refusal leaves the status working
                if let Err(e) = subscribe_joins(s, nonce, &joins) {
                    note(&format!("discord: Join not available: {e}"));
                }
            }
            if let Some(s) = conn.as_mut().filter(|_| want != shown) {
                nonce += 1;
                match set_activity(s, pid, nonce, want.as_ref(), &joins) {
                    Ok(()) => {
                        note(if want.is_some() { "discord: activity accepted" } else { "discord: activity cleared" });
                        shown = want;
                    }
                    Err(e) => {
                        note(&format!("discord: {e}; trying again later"));
                        conn = None;
                        wait = timing.retry;
                    }
                }
            }
        }
        // until the next update: what Discord sends meanwhile (a friend's Join) is read as it comes
        let until = std::time::Instant::now() + wait;
        loop {
            if let Some(s) = conn.as_mut().filter(|s| s.waiting()) {
                match read_frame(s) {
                    Ok((_, frame)) => event(&frame, &joins),
                    Err(e) => {
                        note(&format!("discord: {e}; trying again later"));
                        conn = None;
                    }
                }
                continue;
            }
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            if let Err(RecvTimeoutError::Disconnected) = stop.recv_timeout(left.min(Duration::from_millis(250))) {
                return;
            }
        }
    }
}

/// The running status: `set_mode` from the game thread (a lock, no I/O); dropping it ends the thread.
pub struct Handle {
    shared: Arc<Mutex<Info>>,
    joins: Arc<Mutex<Vec<String>>>,
    _stop: mpsc::Sender<()>,
}

impl Handle {
    pub fn start(game: Game, settings: impl Fn() -> Config + Send + 'static, note: impl Fn(&str) + Send + 'static) -> Handle {
        let shared = Arc::new(Mutex::new(Info { game, mode: Mode::Offline, place: None, join_secret: None, party: None }));
        let joins = Arc::<Mutex<Vec<String>>>::default();
        let (stop_tx, stop) = mpsc::channel();
        let (s, begun) = (shared.clone(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
        // no thread, no status: the game does not care
        let _ = std::thread::Builder::new()
            .name("discord".into())
            .spawn({
                let joins = joins.clone();
                move || run(open_pipe, s, settings, begun, Timing { gap: GAP, retry: RETRY }, stop, note, joins)
            });
        Handle { shared, joins, _stop: stop_tx }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.shared.lock().unwrap().mode = mode;
    }

    /// The open lobby friends may join from Discord, or none.
    pub fn set_lobby(&self, lobby: Option<Lobby>) {
        let mut i = self.shared.lock().unwrap();
        i.join_secret = lobby.as_ref().map(|l| l.secret.clone());
        i.party = lobby.map(|l| (l.party, l.size, l.max));
    }

    /// The secrets of friends' Joins since the last call (what to join).
    pub fn take_joins(&self) -> Vec<String> {
        std::mem::take(&mut *self.joins.lock().unwrap())
    }
}

pub fn game_of(title: u64) -> Option<Game> {
    match title {
        redex_proto::TITLE_X => Some(Game::X),
        redex_proto::TITLE_Y => Some(Game::Y),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering::SeqCst};

    fn info(mode: Mode) -> Info {
        Info { game: Game::X, mode, place: None, join_secret: None, party: None }
    }
    const ON: Config = Config { status: true, location: true };
    const WAIT: Duration = Duration::from_secs(5);

    impl Pipe for TcpStream {
        fn waiting(&self) -> bool {
            let _ = self.set_nonblocking(true);
            let got = self.peek(&mut [0u8]).is_ok_and(|n| n > 0);
            let _ = self.set_nonblocking(false);
            got
        }
    }

    /// An open lobby (friends only or public) shows Join: its world as the secret, the room code and players as the party.
    #[test]
    fn a_lobby_sets_the_join_secret_and_party() {
        let i = Info { join_secret: Some("steam:900".into()), party: Some(("K7MQ2X".into(), 2, 4)), ..info(Mode::P2p) };
        let a = activity(&i, &ON, 0).unwrap();
        assert_eq!(a["secrets"]["join"], "steam:900");
        assert_eq!((a["party"]["id"].as_str(), &a["party"]["size"]), (Some("K7MQ2X"), &json!([2, 4])));
        // a secret without a party (or the reverse) shows no Join
        let half = activity(&Info { party: None, ..i.clone() }, &ON, 0).unwrap();
        assert!(half.get("secrets").is_none() && half.get("party").is_none());
    }

    /// Discord delivers ACTIVITY_JOIN to the game of the player who pressed Join, which hosts nothing: every game asks for
    /// Joins once per connection, and the Join (whenever Discord sends it, also between an update and its answer)
    /// reaches `joins`.
    #[test]
    fn a_friends_join_from_discord_arrives() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let (tx, rx) = mpsc::channel();
        let l2 = l.try_clone().unwrap();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = l2.accept() else { return };
            while let Ok((op, v)) = read_frame(&mut s) {
                let _ = tx.send(v.clone());
                if op == OP_HANDSHAKE {
                    let _ = write_frame(&mut s, OP_FRAME, &json!({ "cmd": "DISPATCH", "evt": "READY" }));
                    continue;
                }
                let join = json!({ "cmd": "DISPATCH", "evt": "ACTIVITY_JOIN", "data": { "secret": "steam:900" } });
                if v["cmd"] == "SET_ACTIVITY" {
                    // an event before the answer: it must not be taken for it
                    let _ = write_frame(&mut s, OP_FRAME, &join);
                }
                let _ = write_frame(&mut s, OP_FRAME, &json!({ "cmd": v["cmd"], "evt": null, "data": {}, "nonce": v["nonce"] }));
                if v["cmd"] == "SUBSCRIBE" {
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = write_frame(&mut s, OP_FRAME, &json!({ "cmd": "DISPATCH", "evt": "ACTIVITY_JOIN", "data": { "secret": "steam:901" } }));
                }
            }
        });
        // the joiner: offline, no lobby, no secret of its own
        let shared = Arc::new(Mutex::new(info(Mode::Offline)));
        let joins = Arc::<Mutex<Vec<String>>>::default();
        let (stop, stopped) = mpsc::channel();
        let (s, j) = (shared.clone(), joins.clone());
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let timing = Timing { gap: Duration::from_millis(20), retry: Duration::from_millis(40) };
            run(move || TcpStream::connect(("127.0.0.1", port)).ok(), s, || ON, 1, timing, stopped, |_| {}, j);
        });
        let cmds: Vec<String> = std::iter::from_fn(|| rx.recv_timeout(WAIT).ok()).take(3).map(|v| v["cmd"].as_str().unwrap_or("").to_owned()).collect();
        assert_eq!(cmds, ["", "SUBSCRIBE", "SET_ACTIVITY"], "handshake, then Joins asked for once, then the status");
        let until = std::time::Instant::now() + WAIT;
        while joins.lock().unwrap().len() < 2 && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut got = joins.lock().unwrap().clone();
        got.sort();
        assert_eq!(got, ["steam:900", "steam:901"]);
        drop(stop);
        t.join().unwrap();
    }
    #[test]
    fn text_for_each_mode_and_game() {
        let a = |g, m| activity(&Info { game: g, ..info(m) }, &ON, 100).unwrap();
        assert_eq!(a(Game::X, Mode::Offline)["details"], "Pok\u{e9}mon X \u{b7} Offline");
        assert_eq!(a(Game::Y, Mode::Official)["details"], "Pok\u{e9}mon Y \u{b7} Official world");
        assert_eq!(a(Game::Y, Mode::P2p)["details"], "Pok\u{e9}mon Y \u{b7} P2P world");
        let x = a(Game::X, Mode::Offline);
        assert_eq!((x["assets"]["large_image"].as_str(), x["assets"]["small_image"].as_str()), (Some("logo"), Some("x")));
        assert_eq!(a(Game::Y, Mode::Offline)["assets"]["small_image"], "y");
        assert_eq!(x["timestamps"]["start"], 100);
        assert!(x.get("secrets").is_none() && x.get("party").is_none(), "no lobby, no Join");
    }

    #[test]
    fn location_follows_its_setting() {
        assert_eq!(activity(&info(Mode::Offline), &ON, 0).unwrap()["state"], "Exploring Kalos");
        let named = Info { place: Some("Vaniville Town".into()), ..info(Mode::Offline) };
        assert_eq!(activity(&named, &ON, 0).unwrap()["state"], "In Vaniville Town");
        let hidden = activity(&named, &Config { status: true, location: false }, 0).unwrap();
        assert!(hidden.get("state").is_none() && !hidden.to_string().contains("Vaniville"));
    }

    #[test]
    fn status_off_builds_nothing() {
        assert_eq!(activity(&info(Mode::Official), &Config { status: false, location: true }, 0), None);
    }

    #[test]
    fn frame_layout_is_opcode_length_json() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 1, &json!({ "a": 1 })).unwrap();
        assert_eq!(&buf[..4], &[1, 0, 0, 0]);
        assert_eq!(&buf[4..8], &7u32.to_le_bytes());
        assert_eq!(&buf[8..], br#"{"a":1}"#);
    }

    /// A fake Discord on a loopback socket (the same framing as the pipe): READY for the handshake, an answer carrying the
    /// nonce for each SET_ACTIVITY (or an ERROR with `error`), every frame it gets goes to the test.
    fn fake_discord(frames: mpsc::Sender<(u32, Value)>, error: bool) -> TcpListener {
        fake_discord_quitting(frames, error, usize::MAX)
    }

    /// The same, but the first connection dies after `quit_after` frames (Discord quitting).
    fn fake_discord_quitting(frames: mpsc::Sender<(u32, Value)>, error: bool, quit_after: usize) -> TcpListener {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let l2 = l.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut left = quit_after;
            while let Ok((mut s, _)) = l2.accept() {
                while left > 0 {
                    let Ok((op, v)) = read_frame(&mut s) else { break };
                    left -= 1;
                    let _ = frames.send((op, v.clone()));
                    let reply = if op == OP_HANDSHAKE {
                        json!({ "cmd": "DISPATCH", "evt": "READY", "data": { "v": 1 } })
                    } else if error {
                        json!({ "cmd": "SET_ACTIVITY", "evt": "ERROR", "data": { "code": 4000, "message": "no" }, "nonce": v["nonce"] })
                    } else {
                        json!({ "cmd": "SET_ACTIVITY", "evt": null, "data": {}, "nonce": v["nonce"] })
                    };
                    if write_frame(&mut s, OP_FRAME, &reply).is_err() {
                        break;
                    }
                }
                left = usize::MAX;
            }
        });
        l
    }

    /// `run` on a thread, connecting to whatever port `port` holds (0: nobody is there).
    fn spawn_run(port: Arc<AtomicU16>, cfg: impl Fn() -> Config + Send + 'static) -> (Arc<Mutex<Info>>, mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let shared = Arc::new(Mutex::new(info(Mode::Offline)));
        let (tx, rx) = mpsc::channel();
        let s = shared.clone();
        let t = std::thread::spawn(move || {
            let open = move || match port.load(SeqCst) {
                0 => None,
                p => TcpStream::connect(("127.0.0.1", p)).ok(),
            };
            let timing = Timing { gap: Duration::from_millis(20), retry: Duration::from_millis(40) };
            run(open, s, cfg, 77, timing, rx, |_| {}, Arc::default());
        });
        (shared, tx, t)
    }

    fn port_of(l: &TcpListener) -> Arc<AtomicU16> {
        Arc::new(AtomicU16::new(l.local_addr().unwrap().port()))
    }

    #[test]
    fn handshake_and_set_activity_frames() {
        let (tx, rx) = mpsc::channel();
        let l = fake_discord(tx, false);
        let (shared, stop, t) = spawn_run(port_of(&l), || ON);
        let (op, hello) = rx.recv_timeout(WAIT).unwrap();
        assert_eq!((op, &hello["v"], hello["client_id"].as_str()), (OP_HANDSHAKE, &json!(1), Some(APP_ID)));
        let (op, sub) = rx.recv_timeout(WAIT).unwrap();
        assert_eq!((op, sub["cmd"].as_str(), sub["evt"].as_str()), (OP_FRAME, Some("SUBSCRIBE"), Some("ACTIVITY_JOIN")), "Joins asked for first");
        let (op, set) = rx.recv_timeout(WAIT).unwrap();
        assert_eq!((op, set["cmd"].as_str()), (OP_FRAME, Some("SET_ACTIVITY")));
        assert_eq!(set["args"]["pid"], std::process::id());
        assert_eq!(set["args"]["activity"]["details"], "Pok\u{e9}mon X \u{b7} Offline");
        assert_eq!(set["args"]["activity"]["timestamps"]["start"], 77);
        assert!(set["nonce"].is_string());
        // the mode changes: the next update says so; while nothing changes nothing is resent
        shared.lock().unwrap().mode = Mode::Official;
        let (_, set) = rx.recv_timeout(WAIT).unwrap();
        assert_eq!(set["args"]["activity"]["details"], "Pok\u{e9}mon X \u{b7} Official world");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "an unchanged status is not resent");
        drop(stop);
        t.join().unwrap();
    }

    #[test]
    fn status_off_never_connects_and_clears_what_was_shown() {
        let (tx, rx) = mpsc::channel();
        let l = fake_discord(tx, false);
        let on = Arc::new(AtomicBool::new(false));
        let o = on.clone();
        let (_, stop, t) = spawn_run(port_of(&l), move || Config { status: o.load(SeqCst), location: true });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "off: no connection, no frame");
        on.store(true, SeqCst);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().0, OP_HANDSHAKE);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().1["cmd"], "SUBSCRIBE");
        assert!(rx.recv_timeout(WAIT).unwrap().1["args"]["activity"].is_object());
        on.store(false, SeqCst);
        let (_, clear) = rx.recv_timeout(WAIT).unwrap();
        assert!(clear["args"].get("activity").is_none(), "switched off: the shown activity is cleared");
        drop(stop);
        t.join().unwrap();
    }

    #[test]
    fn reconnects_after_discord_restarts() {
        let (tx, rx) = mpsc::channel();
        let l = fake_discord_quitting(tx, false, 3);
        let (shared, stop, t) = spawn_run(port_of(&l), || ON);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().0, OP_HANDSHAKE);
        rx.recv_timeout(WAIT).unwrap();
        rx.recv_timeout(WAIT).unwrap();
        // Discord quit (the connection is gone) and is back (the listener answers again); the mode changed meanwhile
        shared.lock().unwrap().mode = Mode::P2p;
        let got: Vec<_> = std::iter::from_fn(|| rx.recv_timeout(WAIT).ok()).take(3).collect();
        assert_eq!(got[0].0, OP_HANDSHAKE, "a new handshake");
        assert_eq!(got[1].1["cmd"], "SUBSCRIBE", "Joins asked for again on the new connection");
        assert_eq!(got[2].1["args"]["activity"]["details"], "Pok\u{e9}mon X \u{b7} P2P world", "the status as it is now");
        drop(stop);
        t.join().unwrap();
    }

    #[test]
    fn nobody_listening_is_quiet_and_tried_again() {
        let (tx, rx) = mpsc::channel();
        let l = fake_discord(tx, false);
        let port = port_of(&l);
        let live = port.swap(0, SeqCst);
        let (_, stop, t) = spawn_run(port.clone(), || ON);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "Discord not running: nothing happens");
        port.store(live, SeqCst);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().0, OP_HANDSHAKE, "found once it runs");
        drop(stop);
        t.join().unwrap();
    }

    #[test]
    fn a_refused_activity_is_retried_not_fatal() {
        let (tx, rx) = mpsc::channel();
        let l = fake_discord(tx, true);
        let (_, stop, t) = spawn_run(port_of(&l), || ON);
        let ops: Vec<u32> = std::iter::from_fn(|| rx.recv_timeout(WAIT).ok()).take(6).map(|f| f.0).collect();
        // a refused SUBSCRIBE keeps the connection; a refused SET_ACTIVITY reconnects
        assert_eq!(ops, [OP_HANDSHAKE, OP_FRAME, OP_FRAME, OP_HANDSHAKE, OP_FRAME, OP_FRAME], "each refusal reconnects");
        drop(stop);
        t.join().unwrap();
    }
}
