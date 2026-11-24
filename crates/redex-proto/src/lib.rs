//! RE:DEX world protocol: messages and framing, shared by `redex-world` (server) and the client in `xr-net`.
//! Frame = u32 LE length, then payload (tag byte + fields, little endian). The length is checked against
//! per-state limit before anything is buffered. The framing design follows metaxy (see NOTICE; no code copied).
//!
//! # Frozen `Hello` prefix
//! `Hello` is always `tag 1 (u8), version (u16 LE)`, then version-specific fields. That prefix never changes, so
//! a server can always read the version of any client and refuse it with a clear reason, whatever else changed.
//!
//! # Identity
//! `Hello.auth` says who the player is: a Steam auth session ticket with the SteamID64 it claims (the server has Steam
//! verify the ticket for that id; a ticket is good for one login), or a dev id (128 random bits made once per install) that only a `--dev` server on localhost accepts, for two
//! instances on one PC. The identity is only a key: the server assigns the session id and never relays the identity.
//! A second `Hello` with the same identity replaces the live session (a crash needs reconnect); the old connection
//! gets `Refuse` `ELSEWHERE`.
//!
//! # Client contract
//! - Run the connection on a dedicated net thread: the server drops a connection that sends nothing for 5 s.
//!   Answer every `Ping` with `Pong` (the server pings every 2 s).
//! - Clear all peers on `MapLeave` and on an `Entered` whose map or channel differs from the current one (peers
//!   are re-announced with `Peer`).
//! - On reconnect (after a drop or server restart) send `Hello` again, then `MapEnter` again.
//! - To change map send only `MapEnter` (it leaves the old map), and count it done only on its `Entered`.
//!   `MapEnter`/`MapLeave` are limited to about 2 per second: one inside the 450 ms cooldown is ignored and answered
//!   with where the player still is (`Entered` with `repeat: true` and the current map and channel, or `MapLeave`
//!   when on none), so retry it after the cooldown. A repeat announces nobody: it is not a new room. A flood gets the
//!   connection closed.
//! - `Pos` is capped at 10 Hz; the newest dropped one is delivered on the next tick, so the last position arrives.
//! - A `Refuse` is always followed by a close. Its `code` (`refuse::*`) says whether a retry can help:
//!   `refuse::lasting` codes (version, title, logged in elsewhere, protocol misuse) must not be retried.
//!   `Refuse` is laid out `tag 3, code (u8), reason` from version 2 on and never changes, like the `Hello` prefix.

pub mod appearance;

/// Bumped on any wire change. The server refuses other versions in `Hello`.
/// 2: `Hello` carries `auth` and the 3DS title id, `Peer` the title, `Refuse` a code, `Entered` a repeat flag.
/// 3: `Auth::SteamTicket` carries the SteamID64 (and v2 builds differ in layouts changed after v2 first shipped).
/// 4: appearance is filled in (up to `MAX_APPEARANCE` bytes), `MODE_SURF`, `Whisper`/`PeerWhisper`, `Report`,
///    `AppearanceUpdate`/`PeerAppearance`; a whisper is `WHISPER_CHARS` characters of any width (a u16-length string); names are at most `NAME_CHARS` characters (the wire allows `MAX_NAME` bytes);
///    `Peer` carries the player's SteamID64 (0 for a dev id), for "Add friend"; `Report` names the target's SteamID64
///    too (the server checks it against the session) and its note may be `NOTE_CHARS` characters of any width.
///    (All within v4: no v4 build was released before these, so the version stays.)
/// 5: CharacterPut / Characters, a character per account
pub const VERSION: u16 = 5;
/// Characters an account holds (and a `Characters` list carries) at most.
pub const MAX_CHARACTERS: usize = 8;
/// How far ahead of the server's clock a put's `changed` may be.
pub const DAY_MS: u64 = 86_400_000;
/// Largest payload accepted before `Hello` (a stranger) and after login. Separate so the live limit can grow
/// for trades and the market without loosening what an unauthenticated peer may make us buffer.
pub const MAX_FRAME_PRE_HELLO: usize = 1024;
pub const MAX_FRAME_LIVE: usize = 1024;
/// Highest valid `Pos.facing` (0 up, 1 down, 2 left, 3 right) and `Pos.mode` (`MODE_*`).
pub const MAX_FACING: u8 = 3;
pub const MAX_MODE: u8 = 4;
pub const MODE_WALK: u8 = 0;
pub const MODE_RUN: u8 = 1;
pub const MODE_SKATES: u8 = 2;
pub const MODE_BIKE: u8 = 3;
/// In the protocol, but a client sends it only once surf is traced.
pub const MODE_SURF: u8 = 4;
/// Largest map id the server accepts.
pub const MAX_MAP: u32 = u16::MAX as u32;
/// Wire limit of a name in bytes (16 characters of up to 4 bytes); the server cuts it to `NAME_CHARS` characters.
pub const MAX_NAME: usize = 64;
/// Characters of a name after the server strips control and bidi characters.
pub const NAME_CHARS: usize = 16;
/// Characters of a whisper after stripping.
pub const WHISPER_CHARS: usize = 120;
/// Bytes of a whisper on the wire: `WHISPER_CHARS` characters of up to 4 bytes (a u16 length; a longer text is cut at a
/// character).
pub const WHISPER_BYTES: usize = WHISPER_CHARS * 4;
/// Characters of a report note.
pub const NOTE_CHARS: usize = 200;
/// Bytes of a report note on the wire: `NOTE_CHARS` characters of up to 4 bytes (a u16 length).
pub const NOTE_BYTES: usize = NOTE_CHARS * 4;
pub const MAX_APPEARANCE: usize = 64;
/// The only appearance format version the server lets through (byte 1; byte 0 is the generation tag).
pub const APPEARANCE_VERSION: u8 = 1;
/// `Report.reason`
pub const REPORT_OTHER: u8 = 0;
pub const REPORT_NAME: u8 = 1;
pub const REPORT_CHAT: u8 = 2;
pub const REPORT_CHEAT: u8 = 3;
pub const MAX_REPORT_REASON: u8 = 3;
/// Largest Steam auth ticket in a `Hello` (Steam's session tickets are about 240 bytes).
pub const MAX_TICKET: usize = 512;

/// Who a `Hello` says the player is.
#[derive(Debug, Clone, PartialEq)]
pub enum Auth {
    /// The per-install dev id: accepted only by a `--dev` server (localhost).
    DevId(u128),
    /// A Steam auth session ticket (`ISteamUser::GetAuthSessionTicket`) and the SteamID64 that made it; the server has
    /// Steam check that the two belong together (`BeginAuthSession`).
    SteamTicket { steam_id: u64, ticket: Vec<u8> },
}
/// Generation 6 (X, Y, Omega Ruby, Alpha Sapphire): the `game` field of `Hello`, `MapEnter` and `Pos`.
pub const GEN_X: u8 = 6;

/// 3DS title ids (USA).
pub const TITLE_X: u64 = 0x0004_0000_0005_5D00;
pub const TITLE_Y: u64 = 0x0004_0000_0005_5E00;
pub const TITLE_OR: u64 = 0x0004_0000_0011_C400;
pub const TITLE_AS: u64 = 0x0004_0000_0011_C500;

/// Whose map ids a player's are. Rooms are keyed by region, so X and Y players share Kalos, and a Hoenn map id never
/// mixes with a Kalos one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    Kalos,
    Hoenn,
}

/// `Refuse.code`: why the server closes the connection.
pub mod refuse {
    /// A frame that does not decode.
    pub const BAD_FRAME: u8 = 1;
    /// The client speaks another protocol version.
    pub const VERSION: u8 = 2;
    /// The title (or its generation) is not let in.
    pub const TITLE: u8 = 3;
    /// The same identity logged in again; this older session is closed.
    pub const ELSEWHERE: u8 = 4;
    /// A message other than `Hello` came first, or `MapEnter` named another generation or an invalid map.
    pub const MISUSE: u8 = 5;
    /// Server full or busy.
    pub const FULL: u8 = 6;
    /// No `Hello` in time.
    pub const TIMEOUT: u8 = 7;
    /// Too many messages or map changes.
    pub const FLOOD: u8 = 8;
    /// The identity is not accepted here (a dev id on a server without `--dev`, or a ticket that does not verify).
    pub const AUTH: u8 = 9;

    /// Starts the reason of an `AUTH` that came from Steam's own later answer about the account (not a malformed or
    /// replayed ticket). Steam says only which account an answer is for, so one meant for an older connection can land
    /// on a new login: a client may retry such a refusal once, with a fresh ticket.
    pub const STEAM_ANSWER: &str = "Steam did not accept the login";

    /// A retry would get the same answer (or kick the newer login): do not reconnect on these.
    pub fn lasting(code: u8) -> bool {
        matches!(code, BAD_FRAME | VERSION | TITLE | ELSEWHERE | MISUSE | AUTH)
    }
}

/// Every title the protocol knows: (title, generation, region).
pub const KNOWN: [(u64, u8, Region); 4] =
    [(TITLE_X, 6, Region::Kalos), (TITLE_Y, 6, Region::Kalos), (TITLE_OR, 6, Region::Hoenn), (TITLE_AS, 6, Region::Hoenn)];

/// Titles the server lets in now (Omega Ruby and Alpha Sapphire once they are supported).
pub const TITLES: [u64; 2] = [TITLE_X, TITLE_Y];

/// A player's character: a name and a look, the same in every game. Times are ms since the Unix epoch; `look` is the 19
/// appearance wire bytes (xr-net's `Appearance::to_bytes`).
#[derive(Debug, Clone, PartialEq)]
pub struct Character {
    pub id: u64,
    pub name: String,
    pub look: Vec<u8>,
    pub created: u64,
    pub changed: u64,
}

/// A known title's generation and region.
pub fn title_info(title: u64) -> Option<(u8, Region)> {
    KNOWN.iter().find(|k| k.0 == title).map(|k| (k.1, k.2))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pos {
    /// Game-specific (see `game`).
    pub game: u8,
    pub map: u32,
    /// Ignored by the server on input (it assigns channels); set on relayed copies.
    pub channel: u16,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub facing: u8,
    /// `MODE_*`: walk, run, skates, bike
    pub mode: u8,
    /// Sender's own clock in ms; receivers use it to order and interpolate.
    pub t: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// First message from a client. `auth` is the player's identity, never used as a session id. `game` is the
    /// generation, `title` the running game's 3DS title id (the server derives the region from it).
    Hello { version: u16, auth: Auth, name: String, game: u8, title: u64, appearance: Vec<u8> },
    /// Server reply to Hello; `session` is server-assigned.
    Welcome { session: u32 },
    /// `code` is one of `refuse::*`; `reason` is for people.
    Refuse { code: u8, reason: String },
    MapEnter { game: u8, map: u32 },
    /// Server reply to MapEnter.
    /// `repeat`: not an answer to this move but where the player still is (the move came inside the cooldown and
    /// was ignored); nobody is (re-)announced.
    Entered { map: u32, channel: u16, repeat: bool },
    MapLeave,
    Pos(Pos),
    /// A player is now visible to you (same region, map and channel); `title` says which version they play, `steam`
    /// is their verified SteamID64 (0: a dev id, no Steam account).
    Peer { session: u32, name: String, game: u8, title: u64, appearance: Vec<u8>, steam: u64 },
    PeerPos { session: u32, pos: Pos },
    PeerGone { session: u32 },
    Bye,
    Ping(u32),
    Pong(u32),
    /// To one player on any map. The server drops it (the sender hears nothing) for an unknown session, an empty text
    /// or past 5 per 10 s.
    Whisper { to_session: u32, text: String },
    /// A whisper for you. `name` is the sender's name (they may not be on your map).
    PeerWhisper { from_session: u32, name: String, text: String },
    /// Tell the server about a player (`REPORT_*`). `steam_id` is who the reporter means (their `Peer.steam`, 0 for a
    /// dev id): the server drops a report whose session is no longer that account. Metadata only, at most 3 a minute
    /// per reporter.
    Report { session: u32, steam_id: u64, reason: u8, note: String },
    /// New clothes or bike mid-map. The server checks it like the `Hello` one (a bad one is ignored) and allows 1 a second.
    AppearanceUpdate { appearance: Vec<u8> },
    /// A peer's appearance changed.
    PeerAppearance { session: u32, appearance: Vec<u8> },
    /// A character of ours for the account (sent for each one after `Welcome`, and when one changes). The server drops
    /// one whose id is 0, whose look fails `Appearance::from_bytes`, whose `changed` is more than `DAY_MS` ahead of its
    /// clock, or past 16 a session in 10 s; it cleans the name; else it keeps it by `merge` and answers `Characters`.
    CharacterPut { character: Character },
    /// The account's characters: after `Welcome` and after each put.
    Characters { list: Vec<Character> },
}

/// Puts `c` in `list`: it replaces the character with its id only when its `changed` is strictly newer (a tie keeps
/// the copy already held), and a new id goes in only while the list has fewer than `MAX_CHARACTERS`. Whether the list
/// changed.
pub fn merge(list: &mut Vec<Character>, c: Character) -> bool {
    match list.iter().position(|o| o.id == c.id) {
        Some(i) if c.changed > list[i].changed => list[i] = c,
        Some(_) => return false,
        None if list.len() < MAX_CHARACTERS => list.push(c),
        None => return false,
    }
    true
}

#[derive(Debug, PartialEq)]
pub enum ProtoError {
    TooLarge(usize),
    /// A `Hello` with a version this build does not speak.
    Version(u16),
    Truncated,
    BadTag(u8),
    BadString,
    Trailing,
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ProtoError {}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) { self.0.push(v) }
    fn u16(&mut self, v: u16) { self.0.extend(v.to_le_bytes()) }
    fn u32(&mut self, v: u32) { self.0.extend(v.to_le_bytes()) }
    fn u64(&mut self, v: u64) { self.0.extend(v.to_le_bytes()) }
    fn u128(&mut self, v: u128) { self.0.extend(v.to_le_bytes()) }
    fn f32(&mut self, v: f32) { self.0.extend(v.to_le_bytes()) }
    fn bytes(&mut self, b: &[u8]) {
        let b = &b[..b.len().min(255)];
        self.u8(b.len() as u8);
        self.0.extend(b);
    }
    /// A string of up to `max` bytes (a u16 length), cut at a character boundary.
    fn long_str(&mut self, s: &str, max: usize) {
        let mut n = s.len().min(max).min(u16::MAX as usize);
        while !s.is_char_boundary(n) { n -= 1; }
        self.u16(n as u16);
        self.0.extend(&s.as_bytes()[..n]);
    }
    /// A string cut at a character boundary, so it always decodes.
    fn str(&mut self, s: &str) { self.str_max(s, 255) }
    /// A string of up to `max` (at most 255) bytes, cut at a character boundary.
    fn str_max(&mut self, s: &str, max: usize) {
        let mut n = s.len().min(max);
        while !s.is_char_boundary(n) { n -= 1; }
        self.bytes(&s.as_bytes()[..n]);
    }
    /// Cut to what `R::character` accepts, so it always decodes.
    fn character(&mut self, c: &Character) {
        self.u64(c.id); self.str_max(&c.name, MAX_NAME); self.bytes(&c.look[..c.look.len().min(MAX_APPEARANCE)]);
        self.u64(c.created); self.u64(c.changed);
    }
    fn pos(&mut self, p: &Pos) {
        self.u8(p.game); self.u32(p.map); self.u16(p.channel);
        self.f32(p.x); self.f32(p.y); self.f32(p.z);
        self.u8(p.facing); self.u8(p.mode); self.u32(p.t);
    }
}

struct R<'a>(&'a [u8]);
impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], ProtoError> {
        if self.0.len() < n { return Err(ProtoError::Truncated); }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, ProtoError> { Ok(self.take(1)?[0]) }
    fn u16(&mut self) -> Result<u16, ProtoError> { Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap())) }
    fn u32(&mut self) -> Result<u32, ProtoError> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn u64(&mut self) -> Result<u64, ProtoError> { Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap())) }
    fn u128(&mut self) -> Result<u128, ProtoError> { Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap())) }
    fn f32(&mut self) -> Result<f32, ProtoError> { Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, ProtoError> {
        let n = self.u8()? as usize;
        if n > max { return Err(ProtoError::TooLarge(n)); }
        Ok(self.take(n)?.to_vec())
    }
    fn long_string(&mut self, max: usize) -> Result<String, ProtoError> {
        let n = self.u16()? as usize;
        if n > max { return Err(ProtoError::TooLarge(n)); }
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| ProtoError::BadString)
    }
    fn string(&mut self, max: usize) -> Result<String, ProtoError> {
        String::from_utf8(self.bytes(max)?).map_err(|_| ProtoError::BadString)
    }
    fn character(&mut self) -> Result<Character, ProtoError> {
        Ok(Character { id: self.u64()?, name: self.string(MAX_NAME)?, look: self.bytes(MAX_APPEARANCE)?, created: self.u64()?, changed: self.u64()? })
    }
    fn pos(&mut self) -> Result<Pos, ProtoError> {
        Ok(Pos {
            game: self.u8()?, map: self.u32()?, channel: self.u16()?,
            x: self.f32()?, y: self.f32()?, z: self.f32()?,
            facing: self.u8()?, mode: self.u8()?, t: self.u32()?,
        })
    }
}

impl Msg {
    /// Frame bytes: length prefix + payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(vec![0; 4]);
        match self {
            Msg::Hello { version, auth, name, game, title, appearance } => {
                w.u8(1); w.u16(*version);
                match auth {
                    Auth::DevId(id) => { w.u8(1); w.u128(*id) }
                    Auth::SteamTicket { steam_id, ticket } => {
                        let t = &ticket[..ticket.len().min(MAX_TICKET)];
                        w.u8(2); w.u64(*steam_id); w.u16(t.len() as u16); w.0.extend(t);
                    }
                } w.str(name); w.u8(*game); w.u64(*title); w.bytes(appearance);
            }
            Msg::Welcome { session } => { w.u8(2); w.u32(*session) }
            Msg::Refuse { code, reason } => { w.u8(3); w.u8(*code); w.str(reason) }
            Msg::MapEnter { game, map } => { w.u8(4); w.u8(*game); w.u32(*map) }
            Msg::Entered { map, channel, repeat } => { w.u8(5); w.u32(*map); w.u16(*channel); w.u8(*repeat as u8) }
            Msg::MapLeave => w.u8(6),
            Msg::Pos(p) => { w.u8(7); w.pos(p) }
            Msg::Peer { session, name, game, title, appearance, steam } => {
                w.u8(8); w.u32(*session); w.str(name); w.u8(*game); w.u64(*title); w.bytes(appearance); w.u64(*steam);
            }
            Msg::PeerPos { session, pos } => { w.u8(9); w.u32(*session); w.pos(pos) }
            Msg::PeerGone { session } => { w.u8(10); w.u32(*session) }
            Msg::Bye => w.u8(11),
            Msg::Ping(n) => { w.u8(12); w.u32(*n) }
            Msg::Pong(n) => { w.u8(13); w.u32(*n) }
            Msg::Whisper { to_session, text } => { w.u8(14); w.u32(*to_session); w.long_str(text, WHISPER_BYTES) }
            Msg::PeerWhisper { from_session, name, text } => { w.u8(15); w.u32(*from_session); w.str(name); w.long_str(text, WHISPER_BYTES) }
            Msg::Report { session, steam_id, reason, note } => { w.u8(16); w.u32(*session); w.u64(*steam_id); w.u8(*reason); w.long_str(note, NOTE_BYTES) }
            Msg::AppearanceUpdate { appearance } => { w.u8(17); w.bytes(appearance) }
            Msg::PeerAppearance { session, appearance } => { w.u8(18); w.u32(*session); w.bytes(appearance) }
            Msg::CharacterPut { character } => { w.u8(19); w.character(character) }
            Msg::Characters { list } => {
                let list = &list[..list.len().min(MAX_CHARACTERS)];
                w.u8(20); w.u8(list.len() as u8);
                list.iter().for_each(|c| w.character(c));
            }
        }
        let n = (w.0.len() - 4) as u32;
        w.0[..4].copy_from_slice(&n.to_le_bytes());
        w.0
    }

    /// Decode one payload (no length prefix).
    pub fn decode(payload: &[u8]) -> Result<Msg, ProtoError> {
        let mut r = R(payload);
        let m = match r.u8()? {
            1 => {
                let version = r.u16()?;
                if version != VERSION {
                    return Err(ProtoError::Version(version)); // before reading fields that may have changed
                }
                let auth = match r.u8()? {
                    1 => Auth::DevId(r.u128()?),
                    2 => {
                        let steam_id = r.u64()?;
                        let n = r.u16()? as usize;
                        if n > MAX_TICKET { return Err(ProtoError::TooLarge(n)); }
                        Auth::SteamTicket { steam_id, ticket: r.take(n)?.to_vec() }
                    }
                    t => return Err(ProtoError::BadTag(t)),
                };
                Msg::Hello { version, auth, name: r.string(MAX_NAME)?, game: r.u8()?, title: r.u64()?, appearance: r.bytes(MAX_APPEARANCE)? }
            }
            2 => Msg::Welcome { session: r.u32()? },
            3 => Msg::Refuse { code: r.u8()?, reason: r.string(255)? },
            4 => Msg::MapEnter { game: r.u8()?, map: r.u32()? },
            5 => Msg::Entered { map: r.u32()?, channel: r.u16()?, repeat: r.u8()? != 0 },
            6 => Msg::MapLeave,
            7 => Msg::Pos(r.pos()?),
            8 => Msg::Peer { session: r.u32()?, name: r.string(MAX_NAME)?, game: r.u8()?, title: r.u64()?, appearance: r.bytes(MAX_APPEARANCE)?, steam: r.u64()? },
            9 => Msg::PeerPos { session: r.u32()?, pos: r.pos()? },
            10 => Msg::PeerGone { session: r.u32()? },
            11 => Msg::Bye,
            12 => Msg::Ping(r.u32()?),
            13 => Msg::Pong(r.u32()?),
            14 => Msg::Whisper { to_session: r.u32()?, text: r.long_string(WHISPER_BYTES)? },
            15 => Msg::PeerWhisper { from_session: r.u32()?, name: r.string(MAX_NAME)?, text: r.long_string(WHISPER_BYTES)? },
            16 => Msg::Report { session: r.u32()?, steam_id: r.u64()?, reason: r.u8()?, note: r.long_string(NOTE_BYTES)? },
            17 => Msg::AppearanceUpdate { appearance: r.bytes(MAX_APPEARANCE)? },
            18 => Msg::PeerAppearance { session: r.u32()?, appearance: r.bytes(MAX_APPEARANCE)? },
            19 => Msg::CharacterPut { character: r.character()? },
            20 => {
                let n = r.u8()? as usize;
                if n > MAX_CHARACTERS { return Err(ProtoError::TooLarge(n)); }
                Msg::Characters { list: (0..n).map(|_| r.character()).collect::<Result<_, _>>()? }
            }
            t => return Err(ProtoError::BadTag(t)),
        };
        if !r.0.is_empty() { return Err(ProtoError::Trailing); }
        Ok(m)
    }
}

/// Incremental frame reader: push bytes as they arrive, pop whole messages.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    limit: Option<usize>,
}

impl Decoder {
    /// Switch to the post-login frame limit.
    pub fn set_limit(&mut self, max: usize) { self.limit = Some(max) }

    pub fn push(&mut self, bytes: &[u8]) { self.buf.extend_from_slice(bytes) }

    /// `Ok(None)` = need more bytes. An oversize length errors as soon as the 4-byte header is in.
    pub fn next(&mut self) -> Result<Option<Msg>, ProtoError> {
        if self.buf.len() < 4 { return Ok(None); }
        let n = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if n == 0 || n > self.limit.unwrap_or(MAX_FRAME_PRE_HELLO) { return Err(ProtoError::TooLarge(n)); }
        if self.buf.len() < 4 + n { return Ok(None); }
        let m = Msg::decode(&self.buf[4..4 + n]);
        self.buf.drain(..4 + n);
        m.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos() -> Pos { Pos { game: 6, map: 7, channel: 2, x: 1.5, y: -2.0, z: 3.25, facing: 3, mode: 1, t: 99 } }

    fn all() -> Vec<Msg> {
        vec![
            Msg::Hello { version: VERSION, auth: Auth::SteamTicket { steam_id: 76561197960287930, ticket: vec![9; 240] }, name: "Brock".into(), game: 6, title: TITLE_X, appearance: vec![] },
            Msg::Hello { version: VERSION, auth: Auth::DevId(u128::MAX), name: "Ash".into(), game: 6, title: TITLE_Y, appearance: vec![1, 2, 3] },
            Msg::Welcome { session: 5 }, Msg::Refuse { code: refuse::FULL, reason: "no".into() },
            Msg::MapEnter { game: 6, map: 9 }, Msg::Entered { map: 9, channel: 1, repeat: false },
            Msg::Entered { map: 9, channel: 1, repeat: true }, Msg::MapLeave,
            Msg::Pos(pos()), Msg::Peer { session: 1, name: "B".into(), game: 6, title: TITLE_X, appearance: vec![], steam: 76561197960287930 },
            Msg::PeerPos { session: 1, pos: pos() }, Msg::PeerGone { session: 1 },
            Msg::Bye, Msg::Ping(4), Msg::Pong(4),
            Msg::Whisper { to_session: 3, text: "hi é".into() },
            Msg::PeerWhisper { from_session: 3, name: "Ash".into(), text: "hi".into() },
            Msg::Report { session: 3, steam_id: 76561197960287930, reason: REPORT_CHAT, note: "rude".into() },
            Msg::Report { session: 3, steam_id: 0, reason: REPORT_OTHER, note: "é".repeat(NOTE_CHARS) },
            Msg::AppearanceUpdate { appearance: vec![6, 1, 0, 9] }, Msg::PeerAppearance { session: 3, appearance: vec![6, 1, 1] },
            Msg::CharacterPut { character: ch(7, "Serena", 5) },
            Msg::Characters { list: vec![] }, Msg::Characters { list: vec![ch(7, "Serena", 5), ch(u64::MAX, "é", u64::MAX)] },
        ]
    }

    /// a character with a 19-byte look
    fn ch(id: u64, name: &str, changed: u64) -> Character {
        Character { id, name: name.into(), look: vec![6, 1, 1, 0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0], created: 1, changed }
    }

    #[test]
    fn characters_list_of_eight_fits_a_frame() {
        // twelve 3-byte characters, as a name the game takes; and the most the server keeps (16 of 4 bytes)
        for name in ["\u{3042}".repeat(12), "\u{1F600}".repeat(NAME_CHARS)] {
            let list: Vec<Character> = (1..=MAX_CHARACTERS as u64).map(|id| ch(id, &name, id)).collect();
            let e = Msg::Characters { list: list.clone() }.encode();
            assert!(e.len() - 4 <= MAX_FRAME_LIVE, "{} bytes", e.len() - 4);
            assert_eq!(Msg::decode(&e[4..]), Ok(Msg::Characters { list }));
        }
        // a ninth on the wire is refused (a list is at most MAX_CHARACTERS)
        let mut e = Msg::Characters { list: vec![ch(1, "a", 1)] }.encode()[4..].to_vec();
        e[1] = MAX_CHARACTERS as u8 + 1;
        assert_eq!(Msg::decode(&e), Err(ProtoError::TooLarge(MAX_CHARACTERS + 1)));
    }

    /// Review Focus 4: one character changed offline on two PCs, the newest `changed` wins; a tie keeps ours.
    #[test]
    fn merge_keeps_the_newest_and_ties_keep_ours() {
        let mut list = vec![ch(1, "Serena", 10)];
        assert!(!merge(&mut list, ch(1, "Older", 9)), "an older copy");
        assert!(!merge(&mut list, ch(1, "Tie", 10)), "a tie keeps ours");
        assert_eq!(list, [ch(1, "Serena", 10)]);
        assert!(merge(&mut list, ch(1, "Newer", 11)));
        assert_eq!(list, [ch(1, "Newer", 11)]);
        // new ids go in up to MAX_CHARACTERS; a ninth does not, but a known one is still replaced
        for id in 2..=MAX_CHARACTERS as u64 {
            assert!(merge(&mut list, ch(id, "x", 1)));
        }
        assert!(!merge(&mut list, ch(99, "ninth", 1)));
        assert_eq!(list.len(), MAX_CHARACTERS);
        assert!(merge(&mut list, ch(2, "y", 2)));
        assert_eq!(list[1], ch(2, "y", 2));
    }

    #[test]
    fn round_trip_all_even_byte_by_byte() {
        let mut d = Decoder::default();
        for m in all() {
            for b in m.encode() {
                assert_eq!(d.next().unwrap(), None);
                d.push(&[b]);
            }
            assert_eq!(d.next().unwrap(), Some(m));
        }
    }

    #[test]
    fn oversize_length_rejected_before_payload() {
        let mut d = Decoder::default();
        d.push(&(MAX_FRAME_PRE_HELLO as u32 + 1).to_le_bytes());
        assert_eq!(d.next(), Err(ProtoError::TooLarge(MAX_FRAME_PRE_HELLO + 1)));
        let mut d = Decoder::default();
        d.set_limit(MAX_FRAME_LIVE);
        d.push(&(MAX_FRAME_LIVE as u32 + 1).to_le_bytes());
        assert!(d.next().is_err());
        let mut d = Decoder::default();
        d.push(&0u32.to_le_bytes());
        assert!(d.next().is_err());
    }

    #[test]
    fn malformed_payloads() {
        assert_eq!(Msg::decode(&[99]), Err(ProtoError::BadTag(99)));
        assert_eq!(Msg::decode(&[2, 1]), Err(ProtoError::Truncated));
        assert_eq!(Msg::decode(&[11, 0]), Err(ProtoError::Trailing));
        let mut h = vec![1, VERSION as u8, 0, 1];
        h.extend([0u8; 16]);
        h.push(70);
        h.extend(vec![b'a'; 70]);
        assert!(matches!(Msg::decode(&h), Err(ProtoError::TooLarge(70))));
        assert_eq!(Msg::decode(&[1, 9, 0, 1, 2, 3]), Err(ProtoError::Version(9)));
        // version-1 and -2 Hellos (other layouts) still get read far enough to be refused by their version
        assert_eq!(Msg::decode(&[1, 1, 0, 1, 2, 3]), Err(ProtoError::Version(1)));
        assert_eq!(Msg::decode(&[1, 2, 0, 2, 2, 0, 1, 2]), Err(ProtoError::Version(2)));
        // an unknown auth kind, and a ticket over the cap
        assert_eq!(Msg::decode(&[1, VERSION as u8, 0, 7]), Err(ProtoError::BadTag(7)));
        let mut t = vec![1, VERSION as u8, 0, 2];
        t.extend(7u64.to_le_bytes());
        t.extend(((MAX_TICKET + 1) as u16).to_le_bytes());
        assert_eq!(Msg::decode(&t), Err(ProtoError::TooLarge(MAX_TICKET + 1)));
    }

    #[test]
    fn v4_hello_is_refused_by_version_and_long_text_is_cut_at_a_character() {
        // the frozen prefix: tag 1, version 3 or 4, then whatever they had
        assert_eq!(Msg::decode(&[1, 3, 0, 1, 2, 3]), Err(ProtoError::Version(3)));
        let mut v4 = vec![1, 4, 0, 1];
        v4.extend([7u8; 16]);
        v4.extend([1, b'A', 6, 0, 0x5D, 5, 0, 0, 4, 0, 0, 0]);
        assert_eq!(Msg::decode(&v4), Err(ProtoError::Version(4)));
        let m = Msg::Whisper { to_session: 1, text: "é".repeat(300) }; // 600 bytes, cut to WHISPER_BYTES
        let mut d = Decoder::default();
        d.set_limit(MAX_FRAME_LIVE);
        d.push(&m.encode());
        let Some(Msg::Whisper { text, .. }) = d.next().unwrap() else { panic!() };
        assert_eq!(text.chars().count(), WHISPER_BYTES / 2);
        // WHISPER_CHARS characters of 4 bytes each fit in a frame, in both directions
        let t = char::from_u32(0x1F600).unwrap().to_string().repeat(WHISPER_CHARS);
        for m in [Msg::Whisper { to_session: 1, text: t.clone() }, Msg::PeerWhisper { from_session: 1, name: "n".repeat(MAX_NAME), text: t.clone() }] {
            let e = m.encode();
            assert!(e.len() - 4 <= MAX_FRAME_LIVE);
            assert_eq!(Msg::decode(&e[4..]), Ok(m));
        }
    }

    #[test]
    fn titles_map_to_regions() {
        assert_eq!(title_info(TITLE_X), Some((6, Region::Kalos)));
        assert_eq!(title_info(TITLE_Y), Some((6, Region::Kalos)));
        assert_eq!(title_info(TITLE_OR), Some((6, Region::Hoenn)));
        assert_eq!(title_info(TITLE_AS), Some((6, Region::Hoenn)));
        assert_eq!(title_info(0x0004_0000_0012_3400), None);
        assert!(TITLES.contains(&TITLE_X) && TITLES.contains(&TITLE_Y) && !TITLES.contains(&TITLE_OR));
    }
}
