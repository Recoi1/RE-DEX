//! The world-server client (plan 4 Task 3). A net thread owns the socket; the game thread calls [`Client::frame`] once
//! per frame, which never blocks: the local state goes out over a bounded channel (`try_send`) and what the server
//! says comes back over another (`try_iter`). Connecting, timeouts, disconnects and the reconnect backoff all happen
//! on the net thread. Nothing here touches game memory: the caller reads the local player and gets `peers` back.
use crate::x::field::Appearance;
use redex_proto::{Character, Decoder, MAX_FRAME_LIVE, Msg, Pos, VERSION};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use steamworks::networking_sockets::NetConnection;
use steamworks::networking_types::SendFlags;
use std::time::{Duration, Instant};

/// Positions kept per remote player, newest last (Task 4 interpolates over them).
pub const TRACK: usize = 32;
/// Remote players kept at most (a room this full shows MAX_PUPPETS of them anyway).
pub const MAX_PEERS: usize = 64;
/// Least time between two `Pos` (the server relays at most 10 Hz).
pub const POS_GAP: Duration = Duration::from_millis(100);
/// A `Pos` goes out at least this often, moving or not.
pub const POS_KEEP: Duration = Duration::from_secs(1);
/// The server pings every 2 s: this long without a byte means it is gone.
pub const SILENCE: Duration = Duration::from_secs(5);
/// Events the game thread has not drained yet. Past this (the game paused for long) the net thread drops them and
/// resyncs: a `Clear`, then a fresh `MapEnter` so the server re-announces everyone.
pub const EVENTS: usize = 1024;
/// A session that lasted this long counts as good: the reconnect backoff starts over after it. A shorter one (kicked
/// right after login) keeps the backoff growing.
pub const STABLE: Duration = Duration::from_secs(10);
/// Whispers kept for the frontend to drain (`Client::whispers`); past this the oldest goes.
pub const WHISPERS: usize = 64;
/// Sessions whose Steam account is kept for whispers (`Client::seen`): a busy server's map changes over a long session.
const SEEN: usize = 4096;
/// Whispers, reports and character puts waiting for the net thread.
pub const OUT: usize = 16;
/// The account's character lists kept for the game to drain (`Client::accounts`); past this the oldest goes (each list
/// is the whole account, so the newest is what counts).
pub const ACCOUNTS: usize = 16;

/// Text from another player (a hostile server can send anything): control, bidi and zero-width characters
/// stripped, cut to `max` characters.
pub fn clean(text: &str, max: usize) -> String {
    let bad = |c: char| c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}');
    text.chars().filter(|&c| !bad(c)).take(max).collect()
}

/// The local player as the game thread read it this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Local {
    pub map: u32,
    pub pos: [f32; 3],
    /// 0 up, 1 down, 2 left, 3 right
    pub facing: u8,
    /// `redex_proto::MODE_*`
    pub mode: u8,
    /// what the player wears now (None: not read). One with a gender of 0 or 1 that differs from the last one sent
    /// goes out as an `AppearanceUpdate` (and in the next `Hello`); receivers validate the outfit.
    pub appearance: Option<Appearance>,
}

#[derive(Clone)]
pub struct Options {
    /// host:port of the world server
    pub addr: String,
    /// who the player is: the dev id (`id::load_or_create`), or with `steam` set the SteamID (its ticket is replaced)
    pub auth: redex_proto::Auth,
    /// Steam login: every connection sends a fresh auth session ticket, cancelled when the connection ends.
    pub steam: Option<crate::steam::Steam>,
    /// the running game's 3DS title id (from the loader, never from the user): the server's region and version key
    pub title: u64,
    pub name: String,
    pub appearance: Vec<u8>,
    /// Least time between our `MapEnter`s: over the server's 450 ms cooldown, so a retry is not ignored again.
    pub move_gap: Duration,
    /// First reconnect wait, doubled after each failed try up to `backoff_max`.
    pub backoff: Duration,
    pub backoff_max: Duration,
    /// `EVENTS`; tests make it small.
    pub events: usize,
    /// The player's characters: each is put to the account after every `Welcome` (the server keeps the newest copy).
    pub characters: Vec<Character>,
}

impl Options {
    pub fn new(addr: impl Into<String>, auth: redex_proto::Auth, title: u64, name: impl Into<String>) -> Options {
        Options {
            addr: addr.into(),
            auth,
            steam: None,
            title,
            name: name.into(),
            appearance: Vec::new(),
            move_gap: Duration::from_millis(500),
            backoff: Duration::from_millis(250),
            backoff_max: Duration::from_secs(5),
            events: EVENTS,
            characters: Vec::new(),
        }
    }
}

/// A player on our map and channel, for Task 4's puppets.
#[derive(Debug, Clone)]
pub struct Remote {
    pub name: String,
    /// which version they play (`redex_proto::TITLE_*`)
    pub title: u64,
    /// their SteamID64 as the server verified it, 0 for a dev id (no Steam account)
    pub steam: u64,
    /// their look: `Appearance::from_bytes` of what the server relayed (so validated); None for missing or bad bytes,
    /// which shows the plain look of `gender`
    pub appearance: Option<Appearance>,
    /// 0 boy, 1 girl: the bytes' gender even when their outfit fails the check (`Appearance::gender_of`), else 0
    pub gender: u8,
    /// (when the net thread got it, the position), newest last, at most `TRACK`.
    pub track: VecDeque<(Instant, Pos)>,
}

enum Event {
    Msg(Instant, Msg),
    /// Peers are no longer known (resync after a backlog).
    Clear,
    /// The connection dropped: peers gone, a reconnect is under way.
    Offline,
}

/// A whisper another player sent us (Task 4's chat drains `Client::whispers`).
#[derive(Debug, Clone, PartialEq)]
pub struct Whisper {
    pub from_session: u32,
    /// the sender's SteamID64 as their `Peer` said (0: a dev id, or a session never announced to us)
    pub steam: u64,
    pub name: String,
    pub text: String,
}

pub struct Client {
    tx: SyncSender<Local>,
    /// whispers and reports for the net thread to send (dropped while not logged in)
    out: SyncSender<Msg>,
    rx: Receiver<Event>,
    sent: Option<Local>,
    /// Players on our map and channel, by server session.
    pub peers: HashMap<u32, Remote>,
    /// Every session's Steam account announced this connection (sessions are the server's, so forgotten offline): a
    /// whisper from someone no longer on our map still names its sender's account. At most `SEEN` (then started over).
    seen: HashMap<u32, u64>,
    /// Whispers not yet shown, oldest first (at most `WHISPERS`).
    pub whispers: VecDeque<Whisper>,
    /// Every `Characters` list the server sent (the account's characters), oldest first (at most `ACCOUNTS`). Not
    /// checked here: the caller drops looks that fail `Appearance::from_bytes`.
    pub accounts: VecDeque<Vec<Character>>,
    /// Where the server has us: (map, channel), only after its `Entered`.
    pub room: Option<(u32, u16)>,
    /// Logged in (a message came since the last drop).
    pub online: bool,
    /// The net thread is gone for good (refused, or it panicked): nothing more is sent or heard.
    pub stopped: bool,
}

impl Client {
    /// Starts the net thread. It runs until the `Client` is dropped (then says Bye) or the server refuses us for
    /// good (another protocol version or game, or the same id logged in elsewhere).
    /// An unknown title is an error (the protocol has no generation or region for it).
    pub fn start(mut o: Options) -> io::Result<Client> {
        let (game, _) = redex_proto::title_info(o.title)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("title {:016X} has no world", o.title)))?;
        if o.addr.starts_with("steam:") && o.steam.is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a steam: world address needs Steam (not --dev-id)"));
        }
        // the server cuts a name to NAME_CHARS anyway; a longer one on the wire is refused as a bad frame, on every retry
        o.name = o.name.chars().take(redex_proto::NAME_CHARS).collect();
        o.appearance.truncate(redex_proto::MAX_APPEARANCE);
        let (tx, local_rx) = sync_channel(8);
        let (out, out_rx) = sync_channel(OUT);
        let (event_tx, rx) = sync_channel(o.events.max(1));
        std::thread::Builder::new().name("redex-net".into()).spawn(move || Net::new(o, game, local_rx, out_rx, event_tx).run())?;
        Ok(Client::over(tx, out, rx))
    }

    fn over(tx: SyncSender<Local>, out: SyncSender<Msg>, rx: Receiver<Event>) -> Client {
        Client { tx, out, rx, sent: None, peers: HashMap::new(), seen: HashMap::new(), whispers: VecDeque::new(), accounts: VecDeque::new(), room: None, online: false, stopped: false }
    }

    /// Whispers to a player on any map (cut to `WHISPER_CHARS`; control and bidi characters stripped). False when
    /// there is nothing to send, we are not logged in, or the queue is full. The server drops more than 5 in 10 s.
    pub fn whisper(&self, to_session: u32, text: &str) -> bool {
        let text = clean(text.trim(), redex_proto::WHISPER_CHARS);
        !text.is_empty() && self.online && self.out.try_send(Msg::Whisper { to_session, text }).is_ok()
    }

    /// Reports a player (`redex_proto::REPORT_*`; the note is cut to `NOTE_CHARS` characters). `steam_id` is the account
    /// meant (their `Remote.steam`): the server drops the report when the session is no longer it. False when not logged
    /// in, the reason is unknown, or the session on our map is known to be another account now.
    pub fn report(&self, session: u32, steam_id: u64, reason: u8, note: &str) -> bool {
        let note = clean(note.trim(), redex_proto::NOTE_CHARS);
        let moved = self.peers.get(&session).is_some_and(|r| r.steam != steam_id);
        !moved && reason <= redex_proto::MAX_REPORT_REASON && self.online && self.out.try_send(Msg::Report { session, steam_id, reason, note }).is_ok()
    }

    /// A character for the account (a new one, or a new look): the net thread keeps it in its copy of
    /// `Options.characters` (by `redex_proto::merge`), so every later login puts the newest, and puts it now when logged
    /// in. False when the queue is full or the net thread is gone.
    pub fn put_character(&self, c: Character) -> bool {
        self.out.try_send(Msg::CharacterPut { character: c }).is_ok()
    }

    /// Once per frame, on the game thread; never blocks. `local` is None outside the field (nothing is sent then).
    pub fn frame(&mut self, local: Option<Local>) {
        if self.stopped {
            return;
        }
        if let Some(l) = local.filter(|l| Some(*l) != self.sent) {
            // full: the net thread is busy; the newest state goes on a later frame
            if self.tx.try_send(l).is_ok() {
                self.sent = Some(l);
            }
        }
        loop {
            match self.rx.try_recv() {
                Ok(e) => self.apply(e),
                Err(TryRecvError::Empty) => break,
                // the net thread ended (refused for good, or panicked): offline for the rest of the run
                Err(TryRecvError::Disconnected) => {
                    self.apply(Event::Offline);
                    self.stopped = true;
                    break;
                }
            }
        }
    }

    fn apply(&mut self, e: Event) {
        let m = match e {
            Event::Msg(at, m) => {
                self.online = true;
                (at, m)
            }
            Event::Clear => return self.clear(),
            Event::Offline => {
                self.online = false;
                self.seen.clear();
                return self.clear();
            }
        };
        match m {
            (_, Msg::Entered { repeat: true, .. }) => {} // where we still are: nothing changed
            (_, Msg::Entered { map, channel, .. }) => {
                if self.room != Some((map, channel)) {
                    self.peers.clear(); // a new room re-announces its players
                }
                self.room = Some((map, channel));
            }
            (_, Msg::MapLeave) => self.clear(),
            // a server can announce any number of sessions: past MAX_PEERS new ones are ignored (a known one is updated)
            (_, Msg::Peer { session, name, title, appearance, steam, .. }) if self.peers.len() < MAX_PEERS || self.peers.contains_key(&session) => {
                let (look, gender) = (Appearance::from_bytes(&appearance), Appearance::gender_of(&appearance));
                let name = clean(&name, redex_proto::NAME_CHARS);
                if self.seen.len() >= SEEN {
                    self.seen.clear();
                }
                self.seen.insert(session, steam);
                self.peers.insert(session, Remote { name, title, steam, appearance: look, gender, track: VecDeque::new() });
            }
            (_, Msg::PeerAppearance { session, appearance }) => {
                if let Some(r) = self.peers.get_mut(&session) {
                    (r.appearance, r.gender) = (Appearance::from_bytes(&appearance), Appearance::gender_of(&appearance));
                }
            }
            (_, Msg::PeerWhisper { from_session, name, text }) => {
                if self.whispers.len() == WHISPERS {
                    self.whispers.pop_front();
                }
                let steam = self.seen.get(&from_session).copied().unwrap_or(0);
                self.whispers.push_back(Whisper { from_session, steam, name: clean(&name, redex_proto::NAME_CHARS), text: clean(&text, redex_proto::WHISPER_CHARS) });
            }
            (at, Msg::PeerPos { session, pos }) => {
                if let Some(r) = self.peers.get_mut(&session) {
                    if r.track.len() == TRACK {
                        r.track.pop_front();
                    }
                    r.track.push_back((at, pos));
                }
            }
            (_, Msg::PeerGone { session }) => {
                self.peers.remove(&session);
            }
            (_, Msg::Characters { list }) => {
                if self.accounts.len() == ACCOUNTS {
                    self.accounts.pop_front();
                }
                self.accounts.push_back(list);
            }
            _ => {}
        }
    }

    fn clear(&mut self) {
        self.peers.clear();
        self.room = None;
    }
}

/// Reconnect waits: `first`, doubled after each failed try up to `max`, each spread by +-50% so clients dropped
/// together do not all come back together.
struct Backoff {
    next: Duration,
    first: Duration,
    max: Duration,
}

impl Backoff {
    fn new(first: Duration, max: Duration) -> Backoff {
        Backoff { next: first, first, max }
    }

    /// The wait before the next try; `r` is a random u32.
    fn wait(&mut self, r: u32) -> Duration {
        let d = self.next.mul_f64(0.5 + f64::from(r) / f64::from(u32::MAX));
        self.next = (self.next * 2).min(self.max);
        d
    }

    fn reset(&mut self) {
        self.next = self.first;
    }
}

/// The connection to the world server: TCP (`host:port`), or Steam P2P over the relays (`steam:<steamid64>`), whose
/// messages carry the same frame bytes.
enum Conn {
    Tcp(TcpStream),
    Steam { conn: NetConnection, steam: crate::steam::Steam, rx: Vec<u8> },
}

impl Conn {
    fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => s.write_all(b),
            Conn::Steam { conn, .. } => conn.send_message(b, SendFlags::RELIABLE_NO_NAGLE).map(|_| ()).map_err(io::Error::other),
        }
    }

    /// As a TCP read with a 10 ms timeout: Ok(0) once closed, WouldBlock when nothing came.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (conn, steam, rx) = match self {
            Conn::Tcp(s) => return s.read(buf),
            Conn::Steam { conn, steam, rx } => (conn, steam, rx),
        };
        if rx.is_empty() {
            for m in conn.receive_messages(16).map_err(|_| io::Error::from(io::ErrorKind::ConnectionReset))? {
                rx.extend_from_slice(m.data());
            }
        }
        if rx.is_empty() {
            if !steam.live(conn) {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(10));
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = rx.len().min(buf.len());
        buf[..n].copy_from_slice(&rx[..n]);
        rx.drain(..n);
        Ok(n)
    }
}

/// A session that stayed up resets the backoff; one kicked straight after login does not.
fn stable(welcomed: bool, lasted: Duration) -> bool {
    welcomed && lasted >= STABLE
}

/// How a connection ended.
enum End {
    /// Dropped or failed: reconnect after the backoff. `true` when the session lasted `STABLE` (the backoff starts over).
    Retry(bool),
    /// Refused for good, or the game dropped the client.
    Stop,
}

struct Net {
    o: Options,
    /// generation, from the title
    game: u8,
    local_rx: Receiver<Local>,
    out_rx: Receiver<Msg>,
    tx: SyncSender<Event>,
    local: Option<Local>,
    /// A reset the game thread still has to get (the channel was full). Until it goes through nothing else is sent:
    /// everything after it must follow it.
    owed: Option<Event>,
    /// the game dropped the client
    gone: bool,
    start: Instant,
    /// a Steam-answer AUTH refusal was already retried since the last login
    steam_retried: bool,
}

impl Net {
    fn new(o: Options, game: u8, local_rx: Receiver<Local>, out_rx: Receiver<Msg>, tx: SyncSender<Event>) -> Net {
        Net { o, game, local_rx, out_rx, tx, local: None, owed: None, gone: false, start: Instant::now(), steam_retried: false }
    }

    fn run(mut self) {
        let mut backoff = Backoff::new(self.o.backoff, self.o.backoff_max);
        loop {
            let end = match self.login() {
                Ok((auth, ticket)) => {
                    let end = match self.connect() {
                        Ok(s) => self.session(s, auth),
                        Err(e) => {
                            log::debug!("world: cannot reach {}: {e}", self.o.addr);
                            End::Retry(false)
                        }
                    };
                    // a ticket is good for one connection: cancelled, it cannot be used again
                    if let (Some(steam), Some(t)) = (&self.o.steam, ticket) {
                        steam.cancel(t);
                    }
                    end
                }
                Err(e) => {
                    log::warn!("world: no Steam ticket ({e}); retrying");
                    End::Retry(false)
                }
            };
            match end {
                End::Stop => return,
                End::Retry(true) => backoff.reset(),
                End::Retry(false) => {}
            }
            self.owed = Some(Event::Offline);
            if !self.wait(backoff.wait(getrandom::u32().unwrap_or(u32::MAX / 2))) {
                return;
            }
        }
    }

    /// The `Hello.auth` for one connection: the dev id, or a fresh Steam ticket (and its handle, to cancel).
    fn login(&self) -> io::Result<(redex_proto::Auth, Option<steamworks::AuthTicket>)> {
        let Some(steam) = &self.o.steam else { return Ok((self.o.auth.clone(), None)) };
        // bound to the server's SteamID when the address names it
        let server = self.o.addr.strip_prefix("steam:").and_then(|id| id.parse().ok());
        let (t, ticket) = steam.ticket(server, Duration::from_secs(5))?;
        Ok((redex_proto::Auth::SteamTicket { steam_id: steam.steam_id(), ticket }, Some(t)))
    }

    fn connect(&self) -> io::Result<Conn> {
        if let (Some(id), Some(steam)) = (self.o.addr.strip_prefix("steam:"), &self.o.steam) {
            let id = id.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "steam: needs a SteamID64"))?;
            return Ok(Conn::Steam { conn: steam.connect(id, Duration::from_secs(10))?, steam: steam.clone(), rx: Vec::new() });
        }
        let mut last = io::Error::new(io::ErrorKind::NotFound, "no address");
        for a in self.o.addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&a, Duration::from_secs(2)) {
                Ok(s) => {
                    s.set_nodelay(true)?;
                    s.set_read_timeout(Some(Duration::from_millis(10)))?;
                    // a server that stops reading drops the connection instead of stalling this thread for long
                    s.set_write_timeout(Some(Duration::from_secs(2)))?;
                    return Ok(Conn::Tcp(s));
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Sleeps `d` in small steps, keeping the newest local state and the owed reset moving. False when the game
    /// dropped the client.
    fn wait(&mut self, d: Duration) -> bool {
        let end = Instant::now() + d;
        loop {
            self.pump();
            self.keep_puts(false);
            if self.gone {
                return false;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(Duration::from_millis(20)));
        }
    }

    /// Takes the newest local state and tries the owed reset again. True when the reset just went through.
    fn pump(&mut self) -> bool {
        if let Some(steam) = &self.o.steam {
            steam.pump();
        }
        loop {
            match self.local_rx.try_recv() {
                Ok(l) => {
                    // the newest look also goes in the next Hello; one whose outfit fails the check still carries the
                    // gender (peers check the outfit themselves and show the plain look of that gender)
                    if let Some(a) = l.appearance.filter(|a| a.gender <= 1) {
                        self.o.appearance = a.to_bytes().to_vec();
                    }
                    self.local = Some(l);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.gone = true;
                    break;
                }
            }
        }
        let Some(e) = self.owed.take() else { return false };
        match self.tx.try_send(e) {
            Ok(()) => true,
            Err(TrySendError::Full(e)) => {
                self.owed = Some(e);
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.gone = true;
                false
            }
        }
    }

    /// Takes what waits in the out queue: a character put goes into our copy of `Options.characters` (uploaded at each
    /// login), and with `send` everything comes back encoded to go out now; without it the rest is dropped (it names
    /// sessions of an older login).
    fn keep_puts(&mut self, send: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for m in self.out_rx.try_iter() {
            if let Msg::CharacterPut { character } = &m {
                redex_proto::merge(&mut self.o.characters, character.clone());
            }
            if send {
                out.extend(m.encode());
            }
        }
        out
    }

    fn forward(&mut self, m: Msg) {
        if self.owed.is_some() {
            return;
        }
        match self.tx.try_send(Event::Msg(Instant::now(), m)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.owed = Some(Event::Clear),
            Err(TrySendError::Disconnected(_)) => self.gone = true,
        }
    }

    fn session(&mut self, mut s: Conn, auth: redex_proto::Auth) -> End {
        let o = &self.o;
        let hello = Msg::Hello { version: VERSION, auth, name: o.name.clone(), game: self.game, title: o.title, appearance: o.appearance.clone() };
        if s.write_all(&hello.encode()).is_err() {
            return End::Retry(false);
        }
        let mut dec = Decoder::default();
        dec.set_limit(MAX_FRAME_LIVE);
        let mut welcomed = false;
        // what was queued before this login names sessions of an older one (maybe another server's): never sent; a
        // character put is kept and goes up after Welcome
        self.keep_puts(false);
        let began = Instant::now();
        let stable = |welcomed: bool| stable(welcomed, began.elapsed());
        // the server's map for us; when we last asked for one (cleared by its answer); when we last sent a move
        let (mut entered, mut asked, mut moved) = (None::<u32>, None::<Instant>, None::<Instant>);
        let (mut pos_at, mut pos_sent) = (None::<Instant>, None::<Local>);
        let mut heard = Instant::now();
        // the look the server has for us (the Hello's); a newer one goes out as an AppearanceUpdate (the server sends on
        // at most one a second, the newest last)
        let mut look = self.o.appearance.clone();
        let mut buf = [0u8; 4096];
        loop {
            if self.pump() {
                entered = None; // a backlog was cleared: enter again so the server re-announces everyone
            }
            if self.gone {
                let _ = s.write_all(&Msg::Bye.encode());
                return End::Stop;
            }
            let now = Instant::now();
            let mut out = Vec::new();
            if let (true, Some(l)) = (welcomed, self.local) {
                // a move counts only once the server's Entered says so; an ignored one is answered and retried
                if entered != Some(l.map)
                    && asked.is_none_or(|t| now - t > Duration::from_secs(2))
                    && moved.is_none_or(|t| now - t >= self.o.move_gap)
                {
                    out.extend(Msg::MapEnter { game: self.game, map: l.map }.encode());
                    (asked, moved) = (Some(now), Some(now));
                }
                if entered == Some(l.map) {
                    let due = pos_at.is_none_or(|t| (pos_sent != Some(l) && now - t >= POS_GAP) || now - t >= POS_KEEP);
                    if due {
                        let t = now.duration_since(self.start).as_millis() as u32;
                        let p = Pos { game: self.game, map: l.map, channel: 0, x: l.pos[0], y: l.pos[1], z: l.pos[2], facing: l.facing, mode: l.mode, t };
                        out.extend(Msg::Pos(p).encode());
                        (pos_at, pos_sent) = (Some(now), Some(l));
                    }
                }
                if look != self.o.appearance {
                    look = self.o.appearance.clone();
                    out.extend(Msg::AppearanceUpdate { appearance: look.clone() }.encode());
                }
            }
            if welcomed {
                out.extend(self.keep_puts(true));
            }
            if !out.is_empty() && s.write_all(&out).is_err() {
                return End::Retry(stable(welcomed));
            }
            match s.read(&mut buf) {
                Ok(0) => return End::Retry(stable(welcomed)),
                Ok(n) => {
                    heard = Instant::now();
                    dec.push(&buf[..n]);
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(_) => return End::Retry(stable(welcomed)),
            }
            loop {
                let m = match dec.next() {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!("world: bad frame from the server ({e}); reconnecting");
                        return End::Retry(stable(welcomed));
                    }
                };
                match &m {
                    Msg::Welcome { session } => {
                        log::info!("world: logged in to {} as session {session}", self.o.addr);
                        (welcomed, self.steam_retried) = (true, false);
                        // our characters to the account: it keeps the newest copy of each and answers with its list
                        let puts: Vec<u8> = self.o.characters.iter().flat_map(|c| Msg::CharacterPut { character: c.clone() }.encode()).collect();
                        if !puts.is_empty() && s.write_all(&puts).is_err() {
                            return End::Retry(stable(welcomed));
                        }
                    }
                    Msg::Refuse { code, reason } => {
                        // the same refusal again would be pointless (or would kick our other login back); Steam's own
                        // answer may have been meant for our previous connection, so that one gets one more try
                        let steam_answer = *code == redex_proto::refuse::AUTH && reason.starts_with(redex_proto::refuse::STEAM_ANSWER) && !self.steam_retried;
                        self.steam_retried |= steam_answer;
                        let lasting = redex_proto::refuse::lasting(*code) && !steam_answer;
                        log::warn!("world: refused ({code}): {reason}{}", if lasting { "; staying offline" } else { "; retrying" });
                        return if lasting { End::Stop } else { End::Retry(stable(welcomed)) };
                    }
                    Msg::Ping(n) => {
                        if s.write_all(&Msg::Pong(*n).encode()).is_err() {
                            return End::Retry(stable(welcomed));
                        }
                    }
                    // ignored inside the cooldown: where we are did not change, so ask again after the gap (after a
                    // backlog `entered` is None here and must stay so, or nobody would be re-announced)
                    Msg::Entered { repeat: true, .. } => asked = None,
                    Msg::Entered { map, .. } => {
                        (entered, asked) = (Some(*map), None);
                        pos_at = None; // new mates need our position now
                    }
                    Msg::MapLeave => (entered, asked) = (None, None),
                    Msg::Bye => {
                        log::info!("world: the server is stopping; reconnecting");
                        return End::Retry(stable(welcomed));
                    }
                    _ => {}
                }
                if matches!(m, Msg::Welcome { .. } | Msg::Entered { repeat: false, .. } | Msg::MapLeave | Msg::Peer { .. } | Msg::PeerPos { .. } | Msg::PeerGone { .. } | Msg::PeerAppearance { .. } | Msg::PeerWhisper { .. } | Msg::Characters { .. }) {
                    self.forward(m);
                }
            }
            if heard.elapsed() > SILENCE {
                log::info!("world: no word from the server for {}s; reconnecting", SILENCE.as_secs());
                return End::Retry(stable(welcomed));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_to_its_cap_with_jitter_and_starts_over() {
        let (first, max) = (Duration::from_millis(250), Duration::from_secs(5));
        let mut b = Backoff::new(first, max);
        // r = middle: no jitter, so the growth shows: 250, 500, 1000, 2000, 4000, then the 5 s cap
        let mid = u32::MAX / 2;
        let ms = |d: Duration| (d.as_secs_f64() * 1000.0).round() as u64;
        let grown: Vec<u64> = (0..7).map(|_| ms(b.wait(mid))).collect();
        assert_eq!(grown, [250, 500, 1000, 2000, 4000, 5000, 5000]);
        // jitter spreads each wait over 50%..150%
        let (lo, hi) = (b.wait(0), b.wait(u32::MAX));
        assert_eq!((ms(lo), ms(hi)), (2500, 7500));
        b.reset();
        assert_eq!(ms(b.wait(mid)), 250);
    }

    /// The backoff starts over only after a session that was welcomed and lasted `STABLE` (10 s).
    #[test]
    fn only_a_ten_second_session_resets_the_backoff() {
        assert_eq!(STABLE, Duration::from_secs(10));
        assert!(!stable(true, STABLE - Duration::from_millis(1)), "kicked just before 10 s: keep backing off");
        assert!(stable(true, STABLE));
        assert!(stable(true, STABLE * 6));
        assert!(!stable(false, STABLE * 6), "never logged in: not a good session however long");
    }

    /// The net thread gone (a panic unwinds it, dropping its sender): the game sees offline, no peers, and stops.
    #[test]
    fn a_dead_net_thread_means_offline_and_stopped() {
        let (tx, _local_rx) = sync_channel(8);
        let (out, _out_rx) = sync_channel(OUT);
        let (event_tx, rx) = sync_channel(8);
        let mut c = Client::over(tx, out, rx);
        let peer = Msg::Peer { session: 7, name: "B".into(), game: 6, title: redex_proto::TITLE_X, appearance: vec![], steam: 0 };
        event_tx.send(Event::Msg(Instant::now(), Msg::Entered { map: 1, channel: 0, repeat: false })).unwrap();
        event_tx.send(Event::Msg(Instant::now(), peer)).unwrap();
        let net = std::thread::spawn(move || {
            let _keep = event_tx;
            panic!("net thread panic (expected by this test)");
        });
        assert!(net.join().is_err());
        c.frame(None);
        assert!(!c.online && c.stopped && c.peers.is_empty() && c.room.is_none());
        let l = Local { map: 1, pos: [0.0; 3], facing: 0, mode: 0, appearance: None };
        c.frame(Some(l));
        assert_eq!(c.sent, None, "nothing is sent once stopped");
    }

    /// A hostile server's names and texts lose control, bidi and zero-width characters and are cut to their caps;
    /// the SteamID comes through for "Add friend".
    #[test]
    fn peer_names_are_cleaned_and_carry_the_steam_id() {
        assert_eq!(clean("A\u{202E}sh\u{200B}\n\u{7}!", 16), "Ash!");
        assert_eq!(clean(&"é".repeat(40), 16), "é".repeat(16));
        let (tx, _local_rx) = sync_channel(8);
        let (out, _out_rx) = sync_channel(OUT);
        let (event_tx, rx) = sync_channel(8);
        let mut c = Client::over(tx, out, rx);
        let name = format!("\u{202E}{}", "B".repeat(30));
        event_tx.send(Event::Msg(Instant::now(), Msg::Peer { session: 1, name, game: 6, title: 0, appearance: vec![], steam: 765 })).unwrap();
        event_tx.send(Event::Msg(Instant::now(), Msg::PeerWhisper { from_session: 1, name: "B\u{2066}".into(), text: format!("x{}", "y".repeat(300)) })).unwrap();
        c.frame(None);
        assert_eq!((c.peers[&1].name.as_str(), c.peers[&1].steam), ("BBBBBBBBBBBBBBBB", 765));
        let w = c.whispers.pop_front().unwrap();
        assert_eq!((w.name.as_str(), w.text.chars().count()), ("B", redex_proto::WHISPER_CHARS));
        assert!(c.whisper(1, "hi") && !c.whisper(1, " \u{200B}\n"), "a whisper of nothing is not sent");
    }

    /// A whisper says whose Steam account sent it (what Ignore goes by), from the `Peer` that announced the session,
    /// even once they left our map; 0 for a session never announced, and every session is forgotten offline (a
    /// reconnect starts the server's sessions over for us).
    #[test]
    fn a_whisper_names_its_senders_steam_account() {
        let (tx, _local_rx) = sync_channel(8);
        let (out, _out_rx) = sync_channel(OUT);
        let (event_tx, rx) = sync_channel(8);
        let mut c = Client::over(tx, out, rx);
        let say = |from| Event::Msg(Instant::now(), Msg::PeerWhisper { from_session: from, name: "B".into(), text: "hi".into() });
        event_tx.send(Event::Msg(Instant::now(), Msg::Peer { session: 1, name: "B".into(), game: 6, title: 0, appearance: vec![], steam: 765 })).unwrap();
        event_tx.send(Event::Msg(Instant::now(), Msg::PeerGone { session: 1 })).unwrap();
        event_tx.send(say(1)).unwrap();
        event_tx.send(say(2)).unwrap();
        c.frame(None);
        assert_eq!(c.whispers.iter().map(|w| w.steam).collect::<Vec<_>>(), [765, 0]);
        c.whispers.clear();
        event_tx.send(Event::Offline).unwrap();
        event_tx.send(say(1)).unwrap();
        c.frame(None);
        assert_eq!(c.whispers.pop_front().map(|w| w.steam), Some(0));
    }

    #[test]
    fn a_repeat_entered_changes_nothing() {
        let (tx, _local_rx) = sync_channel(8);
        let (out, _out_rx) = sync_channel(OUT);
        let (event_tx, rx) = sync_channel(8);
        let mut c = Client::over(tx, out, rx);
        let at = Instant::now();
        event_tx.send(Event::Msg(at, Msg::Entered { map: 1, channel: 0, repeat: false })).unwrap();
        event_tx.send(Event::Msg(at, Msg::Peer { session: 7, name: "B".into(), game: 6, title: 0, appearance: vec![], steam: 0 })).unwrap();
        event_tx.send(Event::Msg(at, Msg::Entered { map: 1, channel: 0, repeat: true })).unwrap();
        c.frame(None);
        assert_eq!((c.room, c.peers.len()), (Some((1, 0)), 1));
    }

    #[test]
    fn the_peer_table_stops_at_max_peers() {
        let (tx, _local_rx) = sync_channel(8);
        let (out, _out_rx) = sync_channel(OUT);
        let (event_tx, rx) = sync_channel(MAX_PEERS * 2 + 8);
        let mut c = Client::over(tx, out, rx);
        let peer = |session| Event::Msg(Instant::now(), Msg::Peer { session, name: "B".into(), game: 6, title: 0, appearance: vec![], steam: 0 });
        for s in 0..(MAX_PEERS as u32 * 2) {
            event_tx.send(peer(s)).unwrap();
        }
        // a known one is still updated
        event_tx.send(Event::Msg(Instant::now(), Msg::Peer { session: 3, name: "C".into(), game: 6, title: 0, appearance: vec![], steam: 0 })).unwrap();
        c.frame(None);
        assert_eq!(c.peers.len(), MAX_PEERS);
        assert_eq!(c.peers[&3].name, "C");
        assert!(!c.peers.contains_key(&(MAX_PEERS as u32)));
    }

    /// A peer's look reaches the puppets only through `Appearance::from_bytes`: hostile or empty bytes become None
    /// (the plain boy), a valid one is kept, and a `PeerAppearance` replaces it (for a known session only).
    #[test]
    fn peer_looks_are_validated_and_whispers_queue() {
        let (tx, _local_rx) = sync_channel(8);
        let (out, _out_rx) = sync_channel(OUT);
        let (event_tx, rx) = sync_channel(256);
        let mut c = Client::over(tx, out, rx);
        let girl = Appearance { gender: 1, fashion: [0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0] };
        let send = |m| event_tx.send(Event::Msg(Instant::now(), m)).unwrap();
        let peer = |session, appearance| Msg::Peer { session, name: "B".into(), game: 6, title: 0, appearance, steam: 0 };
        let mut bad = girl.to_bytes().to_vec();
        bad[3 + 4] = 0xFF; // a top past the game's range
        send(peer(1, girl.to_bytes().to_vec()));
        send(peer(2, bad.clone()));
        send(peer(3, vec![]));
        send(peer(4, vec![0xFF; 64]));
        send(peer(5, vec![1])); // the M1 gender byte
        c.frame(None);
        assert_eq!(c.peers[&1].appearance, Some(girl));
        assert!((2..=5).all(|s| c.peers[&s].appearance.is_none()));
        // a girl whose outfit fails the check stays a girl (the plain one); anything else is the plain boy
        assert_eq!([1, 2, 3, 4, 5].map(|s| c.peers[&s].gender), [1, 1, 0, 0, 0]);
        // the look changes mid-map: a good one replaces, a bad one clears (the plain look), an unknown session is ignored
        send(Msg::PeerAppearance { session: 3, appearance: girl.to_bytes().to_vec() });
        send(Msg::PeerAppearance { session: 1, appearance: bad });
        send(Msg::PeerAppearance { session: 99, appearance: girl.to_bytes().to_vec() });
        c.frame(None);
        assert_eq!((c.peers[&3].appearance, c.peers[&1].appearance), (Some(girl), None));
        assert!(!c.peers.contains_key(&99));
        // whispers queue for the frontend, the oldest dropped past WHISPERS
        for i in 0..WHISPERS as u32 + 3 {
            send(Msg::PeerWhisper { from_session: 1, name: "B".into(), text: format!("hi {i}") });
        }
        c.frame(None);
        assert_eq!(c.whispers.len(), WHISPERS);
        assert_eq!(c.whispers.front().unwrap().text, "hi 3");
        assert_eq!(c.whispers.back().unwrap(), &Whisper { from_session: 1, steam: 0, name: "B".into(), text: format!("hi {}", WHISPERS + 2) });
    }
}
