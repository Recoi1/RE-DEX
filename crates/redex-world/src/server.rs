//! Accept loop and one thread per connection (std threads; the load is a few hundred 10 Hz senders).
use crate::steam::Steam;
use crate::transport::{Link, Recv, Transport};
use crate::world::{Deny, World, allow, outbox};
use redex_proto::appearance::Appearance;
use redex_proto::{Auth, Character, DAY_MS, Msg, NAME_CHARS, NOTE_CHARS, ProtoError, WHISPER_CHARS, refuse as code};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::PathBuf;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Config {
    /// We Ping this often; clients answer with Pong (and Ping us the same way).
    pub heartbeat: Duration,
    /// No frame for this long (handshake included) drops the connection.
    pub timeout: Duration,
    pub max_conns: usize,
    /// Keyed by the peer address as given by the transport. Future work: group IPv6 by /64.
    pub max_per_peer: usize,
    /// Frames of any kind per second before the connection is dropped (Pos itself is capped at 10 Hz in `World`).
    pub max_frames_per_sec: u32,
    /// Accept dev ids (`Auth::DevId`): two instances on one PC, never a public server (main allows it on localhost only).
    pub dev: bool,
    /// Steam login (`Auth::SteamTicket`); without it every ticket is refused. Over TCP only on a `dev` server: a ticket
    /// is not bound to the server it is sent to, so a TCP server could replay it to another one (`steam:` binds it).
    pub steam: Option<Arc<Steam>>,
    /// Steam logins per account a minute; more get a retryable `FLOOD` (each one is a Steam auth session).
    pub max_logins_per_min: usize,
    /// Where `reports.jsonl` goes (created on the first report), `accounts/` (each account's characters) and
    /// `bans.txt` (SteamID64s kept out, read at each Steam login).
    pub data: PathBuf,
    /// `--allow FILE`: only the SteamID64s in it log in with Steam (read at each login). Moderation (M8) does not
    /// exist yet, so main refuses `--p2p` without it: a public world is invite-only.
    pub allow: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(2),
            timeout: Duration::from_secs(5),
            max_conns: 256,
            max_per_peer: 8,
            max_frames_per_sec: 60,
            dev: false,
            steam: None,
            max_logins_per_min: 3,
            data: PathBuf::from("redex-world-data"),
            allow: None,
        }
    }
}

/// Character puts a session may make: 16 in 10 s (a login's whole list of 8 and a change right after fit; more are
/// dropped unanswered).
const PUTS_MAX: usize = 16;
const PUTS_WINDOW: Duration = Duration::from_secs(10);

struct Shared {
    world: Mutex<World>,
    /// held while an account file is read, merged and written, so two sessions of one account never interleave
    accounts: Mutex<()>,
    conns: Mutex<HashMap<String, usize>>,
    logins: Mutex<Logins>,
    stop: AtomicBool,
}

/// When each account logged in, over the last minute.
#[derive(Default)]
struct Logins(HashMap<u64, Vec<Instant>>);

impl Logins {
    const WINDOW: Duration = Duration::from_secs(60);

    /// Fewer than `max` logins of `id` in the minute before `now` (and accounts without one are forgotten).
    fn allow(&mut self, id: u64, max: usize, now: Instant) -> bool {
        self.0.retain(|_, v| {
            v.retain(|&t| now.duration_since(t) < Self::WINDOW);
            !v.is_empty()
        });
        self.0.get(&id).map_or(0, Vec::len) < max
    }

    fn record(&mut self, id: u64, now: Instant) {
        self.0.entry(id).or_default().push(now);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Releases a peer's connection slot however its thread ends.
struct Slot(Arc<Shared>, String);
impl Drop for Slot {
    fn drop(&mut self) {
        let mut c = lock(&self.0.conns);
        if let Some(n) = c.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                c.remove(&self.1);
            }
        }
    }
}

/// Removes the session from the world however `session` ends (a panic included), so no ghost stays.
struct Gone<'a>(&'a Shared, u32);
impl Drop for Gone<'_> {
    fn drop(&mut self) {
        lock(&self.0.world).remove(self.1);
    }
}

pub struct Server {
    accept: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl Server {
    pub fn start(cfg: Config, t: impl Transport) -> Server {
        let shared = Arc::new(Shared {
            world: Mutex::new(World::default()),
            accounts: Mutex::new(()),
            conns: Mutex::new(HashMap::new()),
            logins: Mutex::new(Logins::default()),
            stop: AtomicBool::new(false),
        });
        let sh = shared.clone();
        let accept = thread::Builder::new()
            .name("redex-accept".into())
            .spawn(move || {
                while !sh.stop.load(Ordering::Relaxed) {
                    // a panic in one accept round is logged and the loop carries on
                    let r = catch_unwind(AssertUnwindSafe(|| match t.accept(Duration::from_millis(50)) {
                        Ok(Some(link)) => admit(&sh, &cfg, link),
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("redex-world: accept: {e}");
                            thread::sleep(Duration::from_millis(100));
                        }
                    }));
                    if r.is_err() {
                        eprintln!("redex-world: accept loop panicked; continuing");
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("spawn accept thread");
        Server { accept: Some(accept), shared }
    }

    /// Players currently logged in.
    pub fn players(&self) -> usize {
        lock(&self.shared.world).len()
    }

    /// False once the accept thread has ended (the process should exit rather than park).
    pub fn running(&self) -> bool {
        self.accept.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Stops accepting; connection threads send Bye and end on their next tick.
    pub fn stop(self) {}
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

fn admit(sh: &Arc<Shared>, cfg: &Config, mut link: Box<dyn Link>) {
    let peer = link.peer();
    {
        let mut c = lock(&sh.conns);
        let total: usize = c.values().sum();
        let mine = c.entry(peer.clone()).or_insert(0);
        if total >= cfg.max_conns || *mine >= cfg.max_per_peer {
            if *mine == 0 {
                c.remove(&peer);
            }
            let _ = link.send(&Msg::Refuse { code: code::FULL, reason: "server full".into() });
            return;
        }
        *mine += 1;
    }
    // The cell lets us take the link back to send a Refuse if the thread cannot be spawned.
    let cell = Arc::new(Mutex::new(Some((link, Slot(sh.clone(), peer)))));
    let (sh, cfg, c2) = (sh.clone(), cfg.clone(), cell.clone());
    let spawned = thread::Builder::new().name("redex-conn".into()).spawn(move || {
        let Some((link, _slot)) = lock(&c2).take() else { return };
        let _ = connection(&sh, &cfg, link); // errors just end this connection
    });
    if spawned.is_err() {
        if let Some((mut link, _slot)) = lock(&cell).take() {
            let _ = link.send(&Msg::Refuse { code: code::FULL, reason: "server busy".into() });
        }
    }
}

/// What the server keeps and relays of an appearance: a whole look of the session's generation that
/// `Appearance::from_bytes` takes (every field in range), as for `CharacterPut`. Anything else becomes empty (the
/// player is drawn with the default look) rather than refusing the login.
fn clean_appearance(a: Vec<u8>, game: u8) -> Vec<u8> {
    if a.first() == Some(&game) && Appearance::from_bytes(&a).is_some() { a } else { Vec::new() }
}

/// reports.jsonl's cap: past it the file becomes reports.jsonl.1 (the older .1 goes), so reports take 32 MB at most.
const REPORTS_MAX: u64 = 16 << 20;

/// Appends one report line, `reports.jsonl` rotated first when the line would take it past `max` bytes; a failure
/// is logged, never fatal.
fn write_report(dir: &std::path::Path, line: &str, max: u64) {
    // one writer at a time: two connections rotating together would move a fresh file over the full .1
    static WRITER: Mutex<()> = Mutex::new(());
    let _one = lock(&WRITER);
    let (path, line) = (dir.join("reports.jsonl"), format!("{line}\n"));
    let r = std::fs::create_dir_all(dir).and_then(|_| {
        if std::fs::metadata(&path).is_ok_and(|m| m.len() + line.len() as u64 > max) {
            std::fs::rename(&path, dir.join("reports.jsonl.1"))?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        f.write_all(line.as_bytes())
    });
    if let Err(e) = r {
        eprintln!("redex-world: cannot write the report: {e}");
    }
}

/// A put as the server keeps it (the name cleaned), or None when it is dropped: id 0, a look that fails
/// `Appearance::from_bytes` (after `clean_appearance`), or a `changed` more than `DAY_MS` past `now_ms`.
fn clean_character(c: Character, game: u8, now_ms: u64) -> Option<Character> {
    let look = clean_appearance(c.look, game);
    let ok = c.id != 0 && c.changed <= now_ms.saturating_add(DAY_MS) && !look.is_empty();
    ok.then(|| Character { name: clean_name(&c.name), look, ..c })
}

/// Merges a clean put into the account's file (written when the list changed); the account's list as stored. An
/// account file that cannot be read now (nor kept aside) is left alone: nothing is merged and the answer is empty.
fn put_character(sh: &Shared, data: &std::path::Path, key: u128, c: Character) -> Vec<Character> {
    let _held = lock(&sh.accounts);
    let Some(mut list) = crate::accounts::load(data, key) else { return Vec::new() };
    let before = list.clone();
    if redex_proto::merge(&mut list, c) {
        if let Err(e) = crate::accounts::save(data, key, &list) {
            eprintln!("redex-world: cannot save account {key:032x}: {e}");
            return before;
        }
    }
    list
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Names nobody may show (they pass for the server's staff or the game's makers), compared `fold`ed, with only
/// letters and digits kept.
const RESERVED: [&str; 8] = ["admin", "moderator", "redex", "nintendo", "gamefreak", "pokemon", "system", "server"];

/// A character as `clean_name` compares it: full-width forms as ASCII, lower case, and common look-alikes (digits,
/// symbols, Cyrillic) as the letter they pass for.
fn fold(c: char) -> char {
    let c = match c as u32 {
        0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        _ => c,
    };
    match c.to_lowercase().next().unwrap_or(c) {
        '0' => 'o',
        '1' | 'l' | '|' | '!' => 'i',
        '3' | 'é' | 'è' | 'ê' => 'e',
        '4' | '@' => 'a',
        '5' | '$' => 's',
        '7' => 't',
        'а' => 'a',
        'е' => 'e',
        'о' => 'o',
        'р' => 'p',
        'с' => 'c',
        'х' => 'x',
        'у' => 'y',
        'і' => 'i',
        'ѕ' => 's',
        'к' => 'k',
        'м' => 'm',
        'т' => 't',
        c => c,
    }
}

fn clean_name(n: &str) -> String {
    let n = clean_text(n, NAME_CHARS);
    let key: String = n.chars().map(fold).filter(|c| c.is_alphanumeric()).collect();
    if key.is_empty() || RESERVED.contains(&key.as_str()) { "Trainer".into() } else { n }
}

/// The SteamID64 one list line names: the SteamID64 itself, `STEAM_X:Y:Z`, `[U:1:N]` or a
/// `steamcommunity.com/profiles/<id64>` URL.
fn steam_id_of(s: &str) -> Option<u64> {
    const BASE: u64 = 76_561_197_960_265_728; // the SteamID64 of account 0 (individual, public universe)
    if let Some(r) = s.strip_prefix("STEAM_") {
        let v: Vec<&str> = r.split(':').collect();
        let [_, y, z] = v[..] else { return None };
        let (y, z) = (y.parse::<u64>().ok().filter(|&y| y <= 1)?, z.parse::<u64>().ok()?);
        return z.checked_mul(2)?.checked_add(BASE + y);
    }
    if let Some(n) = s.strip_prefix("[U:1:").and_then(|r| r.strip_suffix(']')) {
        return n.parse::<u64>().ok()?.checked_add(BASE);
    }
    s.find("/profiles/").map_or(s, |i| s[i + "/profiles/".len()..].trim_end_matches('/')).parse().ok()
}

/// An allow or ban list's ids, and the numbers (from 1) of the lines that name none. A BOM, spaces, blank lines and
/// `#` comments are fine.
fn parse_ids(text: &str) -> (HashSet<u64>, Vec<usize>) {
    let (mut ids, mut bad) = (HashSet::new(), Vec::new());
    for (i, l) in text.trim_start_matches('\u{FEFF}').lines().enumerate() {
        let l = l.split('#').next().unwrap_or("").trim();
        match steam_id_of(l) {
            Some(id) => drop(ids.insert(id)),
            None if !l.is_empty() => bad.push(i + 1),
            None => {}
        }
    }
    (ids, bad)
}

/// A list file's ids (None when there is no file), parsed again only when its size or time changed: it is read at
/// every login and every `RECHECK`, and a line that names no id is warned about once per change, by number.
fn read_ids(path: &std::path::Path) -> io::Result<Option<HashSet<u64>>> {
    type Cached = (PathBuf, (u64, Option<std::time::SystemTime>), HashSet<u64>);
    static CACHE: Mutex<Vec<Cached>> = Mutex::new(Vec::new());
    let m = match std::fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        m => m?,
    };
    let key = (m.len(), m.modified().ok());
    let mut cache = lock(&CACHE);
    if let Some(c) = cache.iter().find(|c| c.0 == path && c.1 == key) {
        return Ok(Some(c.2.clone()));
    }
    let (ids, bad) = parse_ids(&std::fs::read_to_string(path)?);
    for n in bad {
        eprintln!("redex-world: WARNING: {} line {n} names no Steam account (a SteamID64, STEAM_0:X:Y, [U:1:N] or a profile URL); that line counts for nothing", path.display());
    }
    cache.retain(|c| c.0 != path);
    cache.push((path.to_path_buf(), key, ids.clone()));
    Ok(Some(ids))
}

/// How often a connected Steam player is checked against the lists again (a ban then disconnects them).
const RECHECK: Duration = Duration::from_secs(30);

/// Whether a verified Steam account may play: listed in `allow` when there is a list, and not in `bans`. Both are read
/// now (at login and every `RECHECK`), so an edit counts at once. A list that can't be read refuses everyone (a missing
/// bans file bans nobody).
fn admitted(steam_id: u64, allow: Option<&std::path::Path>, bans: &std::path::Path) -> Result<(), &'static str> {
    if let Some(a) = allow {
        match read_ids(a) {
            Ok(Some(ids)) if ids.contains(&steam_id) => {}
            Ok(Some(_)) => return Err("this world is invite-only, and you are not on its list"),
            r => {
                eprintln!("redex-world: cannot read the allow list {}: {}; refusing logins", a.display(), r.err().map_or("no such file".into(), |e| e.to_string()));
                return Err("this world cannot check its invite list just now");
            }
        }
    }
    match read_ids(bans) {
        Ok(Some(ids)) if ids.contains(&steam_id) => Err("you are banned from this world"),
        Err(e) => {
            eprintln!("redex-world: cannot read {}: {e}; refusing logins", bans.display());
            Err("this world cannot check its bans just now")
        }
        _ => Ok(()),
    }
}

/// Text shown to other players: no control, bidi-override or zero-width characters, trimmed, cut to `max` characters.
fn clean_text(n: &str, max: usize) -> String {
    let bad = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    };
    let n: String = n.chars().filter(|&c| !bad(c)).collect();
    n.trim().chars().take(max).collect::<String>().trim_end().to_string()
}

/// The reason sent for a recv error: a version mismatch says so, anything else is a bad frame.
fn refusal(e: &io::Error) -> (u8, String) {
    match e.get_ref().and_then(|i| i.downcast_ref::<ProtoError>()) {
        Some(ProtoError::Version(v)) => {
            (code::VERSION, format!("update RE:DEX: this world speaks protocol {}, you sent {v}", redex_proto::VERSION))
        }
        Some(p) => (code::BAD_FRAME, format!("bad frame: {p}")),
        None => (code::BAD_FRAME, format!("connection error: {e}")),
    }
}

fn refuse(link: &mut dyn Link, (code, reason): (u8, impl Into<String>)) -> io::Result<()> {
    link.send(&Msg::Refuse { code, reason: reason.into() })
}

fn connection(sh: &Shared, cfg: &Config, mut link: Box<dyn Link>) -> io::Result<()> {
    let start = Instant::now();
    let hello = loop {
        match link.recv(Duration::from_millis(100)) {
            Ok(Recv::Msg(m @ Msg::Hello { .. })) => break m,
            Ok(Recv::Msg(_)) => return refuse(&mut *link, (code::MISUSE, "expected Hello first")),
            Ok(Recv::Closed) => return Ok(()),
            Ok(Recv::Idle) if start.elapsed() > cfg.timeout => return refuse(&mut *link, (code::TIMEOUT, "timed out waiting for Hello")),
            Ok(Recv::Idle) => {}
            Err(e) => return refuse(&mut *link, refusal(&e)),
        }
    };
    let Msg::Hello { auth, name, game, title, appearance, .. } = hello else { unreachable!() };
    // the world's login key; its top bit keeps a SteamID and a dev id from ever standing for each other
    let (key, steam) = match auth {
        // never over Steam P2P, which is reachable from anywhere
        Auth::DevId(id) if cfg.dev && link.steam_id().is_none() => (id & !(1 << 127), None),
        Auth::DevId(_) => return refuse(&mut *link, (code::AUTH, "dev ids are accepted only by a --dev server over TCP")),
        Auth::SteamTicket { .. } if link.steam_id().is_none() && !cfg.dev => {
            return refuse(&mut *link, (code::AUTH, "a Steam ticket over TCP is not bound to this server: log in over steam:<id> (TCP takes Steam logins on a --dev server only)"));
        }
        Auth::SteamTicket { steam_id, ticket } => {
            let Some(s) = &cfg.steam else { return refuse(&mut *link, (code::AUTH, "this server has no Steam login")) };
            // over Steam P2P the connection's own account must be the one the ticket claims
            if link.steam_id().is_some_and(|remote| remote != steam_id) {
                return refuse(&mut *link, (code::AUTH, "the Steam connection belongs to another account than the ticket"));
            }
            // counted once verified, so tickets that fail do not use up the account's logins
            if !lock(&sh.logins).allow(steam_id, cfg.max_logins_per_min, Instant::now()) {
                return refuse(&mut *link, (code::FLOOD, "too many logins for this account; try again in a minute"));
            }
            match s.verify(steam_id, &ticket) {
                Ok(token) => {
                    lock(&sh.logins).record(steam_id, Instant::now());
                    let login = SteamLogin(s.clone(), steam_id, token); // its drop ends the auth session if refused here
                    if let Err(r) = admitted(steam_id, cfg.allow.as_deref(), &cfg.data.join("bans.txt")) {
                        return refuse(&mut *link, (code::AUTH, r));
                    }
                    (1 << 127 | u128::from(steam_id), Some(login))
                }
                Err(r) => return refuse(&mut *link, r),
            }
        }
    };
    let (tx, rx) = outbox();
    let Some(sid) = lock(&sh.world).login(key, clean_name(&name), game, title, clean_appearance(appearance, game), tx) else {
        return refuse(&mut *link, (code::TITLE, format!("title {title:016X} (generation {game}) is not supported")));
    };
    let _gone = Gone(sh, sid);
    link.set_live();
    session(sh, cfg, &mut *link, sid, key, game, &rx, steam.as_ref())
}

/// A verified Steam login (Steam, SteamID, token): `EndAuthSession` however the connection ends.
struct SteamLogin(Arc<Steam>, u64, u64);
impl Drop for SteamLogin {
    fn drop(&mut self) {
        self.0.end(self.1, self.2);
    }
}

/// `key`: the verified login's (a Steam account or a dev id), the only thing an account file is found by.
#[allow(clippy::too_many_arguments)]
fn session(sh: &Shared, cfg: &Config, link: &mut dyn Link, sid: u32, key: u128, game: u8, rx: &Receiver<Msg>, steam: Option<&SteamLogin>) -> io::Result<()> {
    link.send(&Msg::Welcome { session: sid })?;
    let list = {
        let _held = lock(&sh.accounts);
        crate::accounts::load(&cfg.data, key).unwrap_or_default() // unreadable now: an empty list, the file left alone
    };
    link.send(&Msg::Characters { list })?;
    let mut puts = VecDeque::new();
    let (mut last_rx, mut last_ping) = (Instant::now(), Instant::now());
    let (mut window, mut frames) = (Instant::now(), 0u32);
    let mut checked = Instant::now();
    loop {
        if sh.stop.load(Ordering::Relaxed) {
            return link.send(&Msg::Bye);
        }
        // the lists again (cached until they change): a ban, or leaving the allow list, ends a session in play
        if let Some(s) = steam.filter(|_| checked.elapsed() >= RECHECK) {
            checked = Instant::now();
            if let Err(r) = admitted(s.1, cfg.allow.as_deref(), &cfg.data.join("bans.txt")) {
                return refuse(link, (code::AUTH, r));
            }
        }
        if steam.is_some_and(|s| s.0.revoked(s.2)) {
            // Steam's answer names only the account: the client may try once more in case it was its old connection's
            return refuse(link, (code::AUTH, format!("{}: Steam ended it (offline, ticket cancelled or banned)", redex_proto::refuse::STEAM_ANSWER)));
        }
        match link.recv(Duration::from_millis(25)) {
            Ok(Recv::Closed) => return Ok(()),
            Ok(Recv::Idle) => {}
            Err(e) => {
                let _ = refuse(link, refusal(&e));
                return Err(e);
            }
            Ok(Recv::Msg(m)) => {
                let now = Instant::now();
                last_rx = now;
                if now.duration_since(window) >= Duration::from_secs(1) {
                    (window, frames) = (now, 0);
                }
                frames += 1;
                if frames > cfg.max_frames_per_sec {
                    return refuse(link, (code::FLOOD, "too many messages"));
                }
                let denied = match m {
                    Msg::Bye => return Ok(()),
                    Msg::Ping(n) => {
                        link.send(&Msg::Pong(n))?;
                        None
                    }
                    Msg::MapEnter { game, map } => lock(&sh.world).enter(sid, game, map, now).err(),
                    Msg::MapLeave => lock(&sh.world).leave(sid, now).err(),
                    Msg::Pos(p) => {
                        lock(&sh.world).pos(sid, p, now);
                        None
                    }
                    Msg::Whisper { to_session, text } => {
                        lock(&sh.world).whisper(sid, to_session, clean_text(&text, WHISPER_CHARS), now);
                        None
                    }
                    Msg::Report { session, steam_id, reason, note } => {
                        let line = lock(&sh.world).report(sid, session, steam_id, reason, &clean_text(&note, NOTE_CHARS), now);
                        if let Some(l) = line {
                            write_report(&cfg.data, &l, REPORTS_MAX);
                        }
                        None
                    }
                    Msg::CharacterPut { character } => {
                        // dropped without an answer: over the rate, or failing a check
                        if allow(&mut puts, PUTS_MAX, PUTS_WINDOW, now) {
                            if let Some(c) = clean_character(character, game, unix_ms()) {
                                link.send(&Msg::Characters { list: put_character(sh, &cfg.data, key, c) })?;
                            }
                        }
                        None
                    }
                    Msg::AppearanceUpdate { appearance } => {
                        // a bad one is ignored: the previous look stays and nothing is relayed
                        let a = clean_appearance(appearance, game);
                        if !a.is_empty() {
                            lock(&sh.world).set_appearance(sid, a, now);
                        }
                        None
                    }
                    _ => None, // Pong, and anything a client should not send, is only a sign of life
                };
                match denied {
                    None | Some(Deny::Cooldown) => {}
                    Some(Deny::Flood) => return refuse(link, (code::FLOOD, "too many map changes")),
                    Some(Deny::Game) => return refuse(link, (code::MISUSE, "game does not match Hello")),
                    Some(Deny::Map) => return refuse(link, (code::MISUSE, "invalid map")),
                    Some(Deny::Unknown) => return Ok(()),
                }
            }
        }
        lock(&sh.world).tick(sid, Instant::now());
        if last_rx.elapsed() > cfg.timeout {
            return Ok(());
        }
        if last_ping.elapsed() >= cfg.heartbeat {
            last_ping = Instant::now();
            link.send(&Msg::Ping(0))?;
        }
        loop {
            match rx.try_recv() {
                Ok(m) => {
                    link.send(&m)?;
                    if matches!(m, Msg::Bye | Msg::Refuse { .. }) {
                        return Ok(());
                    }
                }
                Err(TryRecvError::Empty) => break,
                // removed from the world: replaced, or its outbox overflowed
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Logins, admitted, clean_appearance, clean_name, write_report};
    use std::time::{Duration, Instant};

    #[test]
    fn names_lose_invisible_and_bidi_characters() {
        assert_eq!(clean_name("A\u{202E}sh\u{200B}\u{FEFF}\n"), "Ash");
        assert_eq!(clean_name("\u{2066}\u{2069}  "), "Trainer");
    }

    /// Names that would pass for the server's staff or the game's makers become "Trainer", whatever their case and spaces.
    #[test]
    fn reserved_names_become_trainer() {
        for n in ["Admin", "ADMIN", " a d m i n ", "Moderator", "RE:DEX", "Redex", "re dex", "Nintendo", "Game Freak", "GameFreak", "Pokemon", "Pok\u{e9}mon", "System", "server", "Ad\u{200B}min"] {
            assert_eq!(clean_name(n), "Trainer", "{n:?}");
        }
        for n in ["Adminton", "Serena", "Ash"] {
            assert_eq!(clean_name(n), n);
        }
    }

    /// Full-width letters and common look-alikes (digits, Cyrillic) don't get a reserved name through.
    #[test]
    fn reserved_names_see_through_look_alikes() {
        for n in ["\u{FF21}\u{FF24}\u{FF2D}\u{FF29}\u{FF2E}", "\u{0410}dmin", "Adm1n", "AdmIn", "M0derat0r", "P0k\u{e9}m0n", "5erver", "N1nt3nd0", "Sy5tem", "R3D3X", "\u{0405}erver"] {
            assert_eq!(clean_name(n), "Trainer", "{n:?}");
        }
        for n in ["Serena", "Shauna", "\u{30B5}\u{30C8}\u{30B7}", "Lillie"] {
            assert_eq!(clean_name(n), n);
        }
    }

    /// One id a line in any of Steam's forms: the SteamID64, STEAM_0:X:Y, [U:1:N] or a profile URL; a BOM, spaces, blank
    /// lines and `#` comments are fine; any other line is reported by its number, never dropped without a word.
    #[test]
    fn steam_ids_in_every_form_and_bad_lines_named() {
        use super::parse_ids;
        let text = "\u{FEFF}76561197960287930\r\n# friends\r\n  STEAM_0:1:11101  \n[U:1:22202]\nhttps://steamcommunity.com/profiles/76561197960287933/\n\nJo's id\n7656119796028793x # typo\n";
        let (ids, bad) = parse_ids(text);
        let mut ids: Vec<u64> = ids.into_iter().collect();
        ids.sort();
        // STEAM_0:1:11101 is 76561197960265728 + 2 * 11101 + 1, [U:1:22202] is 76561197960265728 + 22202
        assert_eq!(ids, [76561197960287930, 76561197960287931, 76561197960287933]);
        assert_eq!(bad, [7, 8]);
    }

    /// `--allow` admits only the accounts it lists, bans.txt keeps its accounts out; both read at each login, `#`
    /// comments and blank lines allowed. A list that can't be read refuses everyone; a missing bans.txt bans nobody.
    #[test]
    fn the_allow_list_and_bans_are_read_at_each_login() {
        let dir = std::env::temp_dir().join(format!("redex-allow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (allow, bans) = (dir.join("allow.txt"), dir.join("bans.txt"));
        std::fs::write(&allow, "# the test group\n76561197960287930\n\n  76561197960287931  # Jo\n").unwrap();
        assert!(admitted(76561197960287930, None, &bans).is_ok(), "no list, no bans: anyone");
        assert!(admitted(76561197960287931, Some(&allow), &bans).is_ok());
        assert!(admitted(76561197960287932, Some(&allow), &bans).is_err(), "not listed");
        assert!(admitted(1, Some(&dir.join("missing.txt")), &bans).is_err(), "an unreadable list refuses");
        std::fs::write(&bans, "76561197960287931\n").unwrap();
        assert!(admitted(76561197960287931, Some(&allow), &bans).is_err(), "banned, though listed");
        assert!(admitted(76561197960287930, Some(&allow), &bans).is_ok());
        std::fs::write(&allow, "76561197960287932\n").unwrap();
        assert!(admitted(76561197960287932, Some(&allow), &bans).is_ok(), "the edit counts at the next login");
        // saved with a BOM (Notepad, PowerShell 5.1's utf8): its one ban still counts
        std::fs::write(&bans, "\u{FEFF}76561197960287932\r\n").unwrap();
        assert!(admitted(76561197960287932, Some(&allow), &bans).is_err(), "a BOM'd ban");
        std::fs::write(&bans, "STEAM_0:0:11102\n").unwrap();
        assert!(admitted(76561197960287932, Some(&allow), &bans).is_err(), "a STEAM_0 ban");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// reports.jsonl is kept under its cap: past it, the file becomes reports.jsonl.1 (an older .1 goes) and a new one starts.
    #[test]
    fn reports_rotate_at_the_cap() {
        let dir = std::env::temp_dir().join(format!("redex-reports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for i in 0..10 {
            write_report(&dir, &format!("{{\"n\":{i}}}"), 30);
        }
        let now = std::fs::read_to_string(dir.join("reports.jsonl")).unwrap();
        let old = std::fs::read_to_string(dir.join("reports.jsonl.1")).unwrap();
        assert!(now.len() <= 30 && old.len() <= 30, "{now:?} {old:?}");
        assert!(now.ends_with("{\"n\":9}\n") && old.ends_with(&format!("{{\"n\":{}}}\n", 9 - now.lines().count())), "{old:?} {now:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reports from many connections at once around one rotation: every line is in reports.jsonl or .1, none lost
    /// to two threads rotating together.
    #[test]
    fn reports_from_many_threads_lose_no_line_at_a_rotation() {
        for round in 0..20 {
            let dir = std::env::temp_dir().join(format!("redex-reports-race-{}-{round}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            // 8 x 25 lines of 10 bytes: one rotation at 1500 bytes, the rest stays under the cap
            let threads: Vec<_> = (0..8).map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || (0..25).for_each(|i| write_report(&dir, &format!("{{\"n\":{:03}}}", t * 25 + i), 1500)))
            }).collect();
            threads.into_iter().for_each(|t| t.join().unwrap());
            let all = ["reports.jsonl", "reports.jsonl.1"].map(|f| std::fs::read_to_string(dir.join(f)).unwrap_or_default()).concat();
            assert_eq!(all.lines().count(), 200, "round {round}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn names_are_cut_to_sixteen_characters() {
        assert_eq!(clean_name("0123456789abcdefXYZ"), "0123456789abcdef");
        assert_eq!(clean_name("\u{202E}0123456789abcde\u{e9}XYZ"), "0123456789abcde\u{e9}");
    }

    #[test]
    fn appearance_must_be_a_whole_valid_look() {
        let girl = vec![6, 1, 1, 0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0];
        assert_eq!(clean_appearance(girl.clone(), 6), girl);
        assert!(clean_appearance(vec![], 6).is_empty());
        assert!(clean_appearance(vec![6, 1, 0], 6).is_empty(), "a tag and a gender, no outfit");
        assert!(clean_appearance([&[5u8][..], &girl[1..]].concat(), 6).is_empty(), "other generation");
        assert!(clean_appearance(girl.clone(), 7).is_empty(), "not the session's generation");
        assert!(clean_appearance([&[6u8, 2][..], &girl[2..]].concat(), 6).is_empty(), "unknown version");
        assert!(clean_appearance(vec![0], 6).is_empty(), "the v3 one-byte gender");
        assert!(clean_appearance([&girl[..], &[0]].concat(), 6).is_empty(), "a byte too many");
        let mut bad = girl.clone();
        bad[18] = 0xFF;
        assert!(clean_appearance(bad, 6).is_empty(), "bits the game does not read");
    }

    #[test]
    fn three_logins_a_minute_per_account() {
        let (mut l, t0, s) = (Logins::default(), Instant::now(), Duration::from_secs);
        for k in 0..3 {
            assert!(l.allow(7, 3, t0 + s(k)));
            l.record(7, t0 + s(k));
        }
        assert!(!l.allow(7, 3, t0 + s(10)), "the fourth within a minute");
        assert!(l.allow(8, 3, t0 + s(10)), "another account is not limited by this one");
        assert!(!l.allow(7, 3, t0 + s(59)));
        assert!(l.allow(7, 3, t0 + s(60)), "the first one is a minute old");
        // forgotten once a minute old: the table does not grow with accounts that come and go
        assert!(l.allow(9, 3, t0 + s(200)) && l.0.is_empty());
    }
}
