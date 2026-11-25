//! Discord Rich Presence over the local Discord app's IPC pipe (`discord-ipc-0` to `-9`): the game, the mode and the play
//! time on the player's profile. Own thread, one update at most every GAP; no Discord SDK, no network (only the Discord
//! app on this PC is spoken to). Discord not running: nothing happens and the pipe is tried again every RETRY.

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
    // the lobbies come with a later branch
    #[allow(dead_code)]
    P2p,
}

/// What the status says about the game right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub game: Game,
    pub mode: Mode,
    /// The current map's name, once the game's own text can be read cheaply (nothing is committed): none shows the region only.
    pub place: Option<String>,
    /// Where a "Join" button will come from (P2P lobbies, a later branch): set, it goes out as the activity's join secret.
    pub join_secret: Option<String>,
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
    if let Some(secret) = &info.join_secret {
        // Discord also wants a party (id and size) for the Join button; the lobby branch adds it
        a["secrets"] = json!({ "join": secret });
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

/// SET_ACTIVITY (none clears it) and Discord's answer: an ERROR event is an error.
fn set_activity(s: &mut (impl Read + Write), pid: u32, nonce: u64, activity: Option<&Value>) -> io::Result<()> {
    let args = match activity {
        Some(a) => json!({ "pid": pid, "activity": a }),
        None => json!({ "pid": pid }),
    };
    write_frame(s, OP_FRAME, &json!({ "cmd": "SET_ACTIVITY", "args": args, "nonce": nonce.to_string() }))?;
    let (_, r) = read_frame(s)?;
    if r["evt"] == "ERROR" { Err(io::Error::other(format!("Discord refused it: {}", r["data"]["message"]))) } else { Ok(()) }
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
fn run<S: Read + Write>(
    open: impl Fn() -> Option<S>,
    shared: Arc<Mutex<Info>>,
    settings: impl Fn() -> Config,
    start: u64,
    timing: Timing,
    stop: mpsc::Receiver<()>,
    note: impl Fn(&str),
) {
    let pid = std::process::id();
    let (mut conn, mut shown, mut nonce): (Option<S>, Option<Value>, u64) = (None, None, 0);
    loop {
        let want = activity(&shared.lock().unwrap().clone(), &settings(), start);
        let mut wait = timing.gap;
        // off and nothing shown: Discord is not even asked
        if want.is_some() || conn.is_some() {
            if conn.is_none() {
                conn = open().and_then(|mut s| handshake(&mut s).ok().map(|_| s));
                shown = None;
                match conn {
                    Some(_) => note("discord: connected"),
                    None => wait = timing.retry,
                }
            }
            if let Some(s) = conn.as_mut().filter(|_| want != shown) {
                nonce += 1;
                match set_activity(s, pid, nonce, want.as_ref()) {
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
        if let Err(RecvTimeoutError::Disconnected) = stop.recv_timeout(wait) {
            return;
        }
    }
}

/// The running status: `set_mode` from the game thread (a lock, no I/O); dropping it ends the thread.
pub struct Handle {
    shared: Arc<Mutex<Info>>,
    _stop: mpsc::Sender<()>,
}

impl Handle {
    pub fn start(game: Game, settings: impl Fn() -> Config + Send + 'static, note: impl Fn(&str) + Send + 'static) -> Handle {
        let shared = Arc::new(Mutex::new(Info { game, mode: Mode::Offline, place: None, join_secret: None }));
        let (stop_tx, stop) = mpsc::channel();
        let (s, begun) = (shared.clone(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
        // no thread, no status: the game does not care
        let _ = std::thread::Builder::new()
            .name("discord".into())
            .spawn(move || run(open_pipe, s, settings, begun, Timing { gap: GAP, retry: RETRY }, stop, note));
        Handle { shared, _stop: stop_tx }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.shared.lock().unwrap().mode = mode;
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
        Info { game: Game::X, mode, place: None, join_secret: None }
    }
    const ON: Config = Config { status: true, location: true };
    const WAIT: Duration = Duration::from_secs(5);

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
        assert!(x.get("secrets").is_none(), "no Join in this branch");
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
            run(open, s, cfg, 77, timing, rx, |_| {});
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
        let l = fake_discord_quitting(tx, false, 2);
        let (shared, stop, t) = spawn_run(port_of(&l), || ON);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().0, OP_HANDSHAKE);
        rx.recv_timeout(WAIT).unwrap();
        // Discord quit (the connection is gone) and is back (the listener answers again); the mode changed meanwhile
        shared.lock().unwrap().mode = Mode::P2p;
        let got: Vec<_> = std::iter::from_fn(|| rx.recv_timeout(WAIT).ok()).take(2).collect();
        assert_eq!(got[0].0, OP_HANDSHAKE, "a new handshake");
        assert_eq!(got[1].1["args"]["activity"]["details"], "Pok\u{e9}mon X \u{b7} P2P world", "the status as it is now");
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
        let ops: Vec<u32> = std::iter::from_fn(|| rx.recv_timeout(WAIT).ok()).take(4).map(|f| f.0).collect();
        assert_eq!(ops, [OP_HANDSHAKE, OP_FRAME, OP_HANDSHAKE, OP_FRAME], "each refusal reconnects");
        drop(stop);
        t.join().unwrap();
    }
}
