//! Who is where. Pure state plus per-session outboxes; no sockets, so it tests without a network.
use redex_proto::{MAX_FACING, MAX_MAP, MAX_MODE, Msg, PlayerId, Pos, Region, TITLES, title_info};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Players per map-channel (an author's constant). Overflow opens the next channel.
pub const CHANNEL_CAP: usize = 16;
/// 10 Hz Pos cap per sender; 10 ms slack for timer jitter.
pub const POS_MIN_GAP: Duration = Duration::from_millis(90);
/// Queued messages per session. A reader that falls this far behind is dropped.
pub const OUTBOX_CAP: usize = 256;
/// MapEnter/MapLeave: about 2 per second per session.
pub const MOVE_COOLDOWN: Duration = Duration::from_millis(450);
/// Consecutive too-fast MapEnter/MapLeave before the connection is closed.
pub const MOVE_STRIKES: u32 = 5;
const COORD_MAX: f32 = 1e5;
/// Whispers per sender: 5 in 10 s; the excess is dropped and the sender told nothing.
pub const WHISPER_MAX: usize = 5;
pub const WHISPER_WINDOW: Duration = Duration::from_secs(10);
/// A whisper goes to the sender's map and channel, or as a reply: to a player who whispered the sender this recently.
pub const REPLY_WINDOW: Duration = Duration::from_secs(600);
/// How far a player may move in 100 ms (world units; a tile is 18): 4 tiles, over twice a bike's 1.5. A further Pos is
/// dropped (the first after `Entered` excepted); the allowance grows with the time since the last relayed one.
pub const MAX_STEP: f32 = 4.0 * 18.0;
/// Reports per reporter: 3 a minute (an author's call: a report costs a human's attention, a whisper does not).
pub const REPORT_MAX: usize = 3;
pub const REPORT_WINDOW: Duration = Duration::from_secs(60);
/// Smallest gap between two `AppearanceUpdate`s that are relayed.
pub const APPEARANCE_GAP: Duration = Duration::from_secs(1);
/// What a report carries of each player: the last 30 s of positions, and of the target the last 20 whispers it sent.
pub const TRAIL: Duration = Duration::from_secs(30);
pub const WHISPER_LOG: usize = 20;

pub fn outbox() -> (SyncSender<Msg>, Receiver<Msg>) {
    sync_channel(OUTBOX_CAP)
}

/// Why a request was refused. `Cooldown` is ignored, and the client is sent where it still is (`Entered` with its
/// current map and channel, or `MapLeave` when on none); the rest end the connection.
#[derive(Debug, PartialEq)]
pub enum Deny {
    Unknown,
    /// Not the generation from Hello.
    Game,
    Map,
    Cooldown,
    Flood,
}

struct Session {
    tx: SyncSender<Msg>,
    id: u128,
    name: String,
    game: u8,
    title: u64,
    region: Region,
    appearance: Vec<u8>,
    /// (map, channel) when on a map.
    room: Option<(u32, u16)>,
    last_pos: Option<Pos>,
    last_pos_at: Option<Instant>,
    /// Newest Pos dropped by the rate cap, flushed by `tick`.
    pending: Option<Pos>,
    last_move_at: Option<Instant>,
    strikes: u32,
    /// Outbox full or closed: removed by `sweep`.
    dead: bool,
    /// Relayed positions of the last `TRAIL`, for reports.
    trail: VecDeque<(Instant, Pos)>,
    /// The last `WHISPER_LOG` whispers this session sent: (when, recipient's identity, text).
    whispers: VecDeque<(Instant, String, String)>,
    whisper_times: VecDeque<Instant>,
    /// Who whispered this session (login key -> when): it may reply to them from any map within `REPLY_WINDOW`.
    heard_from: HashMap<u128, Instant>,
    report_times: VecDeque<Instant>,
    last_appearance_at: Option<Instant>,
    /// Newest `AppearanceUpdate` held by the 1 s gap, flushed by `tick`.
    pending_appearance: Option<Vec<u8>>,
}

/// True (and counted) when fewer than `max` events happened in the `window` before `now`.
pub(crate) fn allow(q: &mut VecDeque<Instant>, max: usize, window: Duration, now: Instant) -> bool {
    while q.front().is_some_and(|&t| now.duration_since(t) >= window) {
        q.pop_front();
    }
    if q.len() >= max {
        return false;
    }
    q.push_back(now);
    true
}

/// Login keys: a dev id has the top bit clear, a verified SteamID64 is `STEAM_KEY | id`, an account id is
/// `ACCOUNT_KEY | id`. The tags never overlap, so no kind of login can stand for another (and Steam keys, and so their
/// account files, stay what they were before accounts existed).
pub const STEAM_KEY: u128 = 1 << 127;
pub const ACCOUNT_KEY: u128 = 3 << 126;

/// Who a login key is, as peers see it.
pub fn player_id(key: u128) -> PlayerId {
    match key >> 126 {
        0b10 => PlayerId::Steam(key as u64),
        0b11 => PlayerId::Account(key as u64),
        _ => PlayerId::Dev,
    }
}

/// Who a login key is, for reports.
fn who(key: u128) -> String {
    match player_id(key) {
        PlayerId::Dev => format!("dev:{key:032x}"),
        PlayerId::Steam(id) => format!("steam:{id}"),
        PlayerId::Account(id) => format!("account:{id}"),
    }
}

fn json(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if c.is_control() => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o + "\""
}

impl Session {
    fn push(&mut self, m: Msg) {
        if self.tx.try_send(m).is_err() {
            self.dead = true;
        }
    }
}

pub struct World {
    next: u32,
    sessions: HashMap<u32, Session>,
    /// Login key (the verified SteamID, or a dev id on a --dev server; see server.rs) -> session. A second login with
    /// the same key replaces the first.
    by_id: HashMap<u128, u32>,
    /// (region, map) -> channels (index = channel number) -> member sessions. X and Y share Kalos; a Hoenn map id never
    /// mixes with a Kalos one.
    rooms: HashMap<(Region, u32), Vec<Vec<u32>>>,
    /// Titles let in (`redex_proto::TITLES`).
    pub titles: Vec<u64>,
}

impl Default for World {
    fn default() -> Self {
        World { next: 0, sessions: HashMap::new(), by_id: HashMap::new(), rooms: HashMap::new(), titles: TITLES.to_vec() }
    }
}

fn valid(p: &Pos) -> bool {
    [p.x, p.y, p.z].iter().all(|v| v.is_finite() && v.abs() < COORD_MAX)
        && p.facing <= MAX_FACING
        && p.mode <= MAX_MODE
        && p.map <= MAX_MAP
}

impl World {
    /// Server assigns the session id; `id` is only a reconnect key. `None` if the title is not allowed or `game` is not
    /// its generation.
    pub fn login(&mut self, id: u128, name: String, game: u8, title: u64, appearance: Vec<u8>, tx: SyncSender<Msg>) -> Option<u32> {
        let (generation, region) = title_info(title).filter(|_| self.titles.contains(&title))?;
        if generation != game {
            return None;
        }
        if let Some(old) = self.by_id.get(&id).copied() {
            eprintln!("redex-world: session {old} replaced by a new login with the same id");
            if let Some(s) = self.sessions.get(&old) {
                let _ = s.tx.try_send(Msg::Refuse { code: redex_proto::refuse::ELSEWHERE, reason: "logged in elsewhere".into() }); // its thread closes after it
            }
            self.remove(old);
        }
        self.next = self.next.wrapping_add(1).max(1);
        let sid = self.next;
        self.by_id.insert(id, sid);
        self.sessions.insert(
            sid,
            Session {
                tx, id, name, game, title, region, appearance, room: None, last_pos: None, last_pos_at: None,
                pending: None, last_move_at: None, strikes: 0, dead: false, trail: VecDeque::new(), whispers: VecDeque::new(),
                whisper_times: VecDeque::new(), heard_from: HashMap::new(), report_times: VecDeque::new(), last_appearance_at: None, pending_appearance: None,
            },
        );
        Some(sid)
    }

    /// MapEnter/MapLeave rate limit shared by both.
    fn gate(&mut self, sid: u32, now: Instant) -> Result<(), Deny> {
        let s = self.sessions.get_mut(&sid).ok_or(Deny::Unknown)?;
        if s.last_move_at.is_some_and(|t| now.duration_since(t) < MOVE_COOLDOWN) {
            s.strikes += 1;
            if s.strikes >= MOVE_STRIKES {
                return Err(Deny::Flood);
            }
            // ignored, but the client is told where it still is, so it retries instead of assuming the move worked
            let here = match s.room {
                Some((map, channel)) => Msg::Entered { map, channel, repeat: true },
                None => Msg::MapLeave,
            };
            s.push(here);
            return Err(Deny::Cooldown);
        }
        s.last_move_at = Some(now);
        s.strikes = 0;
        Ok(())
    }

    /// Joins `map`, in the first channel with room. Returns the channel.
    pub fn enter(&mut self, sid: u32, game: u8, map: u32, now: Instant) -> Result<u16, Deny> {
        let s = self.sessions.get(&sid).ok_or(Deny::Unknown)?;
        if s.game != game {
            return Err(Deny::Game);
        }
        let region = s.region;
        if map > MAX_MAP {
            return Err(Deny::Map);
        }
        self.gate(sid, now)?;
        self.leave_inner(sid);
        let chans = self.rooms.entry((region, map)).or_default();
        let ch = match chans.iter().position(|c| c.len() < CHANNEL_CAP) {
            Some(i) => i,
            None => {
                chans.push(Vec::new());
                chans.len() - 1
            }
        };
        let mates = chans[ch].clone();
        chans[ch].push(sid);
        let me = self.sessions.get_mut(&sid).ok_or(Deny::Unknown)?;
        me.room = Some((map, ch as u16));
        me.last_pos = None;
        me.pending = None;
        me.push(Msg::Entered { map, channel: ch as u16, repeat: false });
        let mine = Msg::Peer { session: sid, name: me.name.clone(), game, title: me.title, appearance: me.appearance.clone(), who: player_id(me.id) };
        for m in mates {
            let o = self.sessions.get_mut(&m).expect("room member exists");
            o.push(mine.clone());
            let theirs = Msg::Peer { session: m, name: o.name.clone(), game, title: o.title, appearance: o.appearance.clone(), who: player_id(o.id) };
            let last = o.last_pos.clone();
            let me = self.sessions.get_mut(&sid).expect("checked above");
            me.push(theirs);
            if let Some(p) = last {
                me.push(Msg::PeerPos { session: m, pos: p });
            }
        }
        self.sweep();
        Ok(ch as u16)
    }

    pub fn leave(&mut self, sid: u32, now: Instant) -> Result<(), Deny> {
        self.gate(sid, now)?;
        self.leave_inner(sid);
        self.sweep();
        Ok(())
    }

    fn leave_inner(&mut self, sid: u32) {
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        s.pending = None;
        let Some((map, ch)) = s.room.take() else { return };
        let region = s.region;
        if let Some(chans) = self.rooms.get_mut(&(region, map)) {
            chans[ch as usize].retain(|&x| x != sid);
            for &m in &chans[ch as usize] {
                if let Some(o) = self.sessions.get_mut(&m) {
                    o.push(Msg::PeerGone { session: sid });
                }
            }
            while chans.last().is_some_and(|c| c.is_empty()) {
                chans.pop();
            }
            if chans.is_empty() {
                self.rooms.remove(&(region, map));
            }
        }
    }

    /// Relays to the sender's channel only. Dropped (not an error) when invalid (non-finite or huge numbers,
    /// bad facing/mode/map), when not on a map, or when game/map disagree with where the server put the player.
    /// Over 10 Hz it is held as `pending` instead and `tick` sends the newest one, so the final position arrives.
    pub fn pos(&mut self, sid: u32, mut p: Pos, now: Instant) {
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        let Some((map, ch)) = s.room else { return };
        if !valid(&p) || p.game != s.game || p.map != map {
            return;
        }
        // shortcut: the client's own claim, only its speed is checked; a world simulation would be the real check
        if let (Some(last), Some(at)) = (&s.last_pos, s.last_pos_at) {
            let tenths = (now.duration_since(at).as_secs_f32() * 10.0).max(1.0);
            if (p.x - last.x).hypot(p.z - last.z).hypot(p.y - last.y) > MAX_STEP * tenths {
                return;
            }
        }
        p.channel = ch;
        if s.last_pos_at.is_some_and(|t| now.duration_since(t) < POS_MIN_GAP) {
            s.pending = Some(p);
            return;
        }
        s.pending = None;
        self.relay(sid, p, now);
    }

    /// Flushes this session's held Pos once the 10 Hz gap has passed.
    pub fn tick(&mut self, sid: u32, now: Instant) {
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        if !s.last_pos_at.is_some_and(|t| now.duration_since(t) < POS_MIN_GAP) {
            if let Some(p) = s.pending.take() {
                self.relay(sid, p, now);
            }
        }
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        if s.last_appearance_at.is_some_and(|t| now.duration_since(t) >= APPEARANCE_GAP) {
            if let Some(a) = s.pending_appearance.take() {
                self.apply_appearance(sid, a, now);
            }
        }
    }

    fn relay(&mut self, sid: u32, p: Pos, now: Instant) {
        let s = self.sessions.get_mut(&sid).expect("caller checked");
        let Some((map, ch)) = s.room else { return };
        s.last_pos_at = Some(now);
        s.last_pos = Some(p.clone());
        s.trail.push_back((now, p.clone()));
        while s.trail.front().is_some_and(|(t, _)| now.duration_since(*t) > TRAIL) {
            s.trail.pop_front();
        }
        let members = self.rooms[&(s.region, map)][ch as usize].clone();
        for m in members {
            if m != sid {
                if let Some(o) = self.sessions.get_mut(&m) {
                    o.push(Msg::PeerPos { session: sid, pos: p.clone() });
                }
            }
        }
        self.sweep();
    }

    /// Drops sessions whose outbox filled (a reader that stopped draining), and any they notify in turn.
    fn sweep(&mut self) {
        loop {
            let dead: Vec<u32> = self.sessions.iter().filter(|(_, s)| s.dead).map(|(&k, _)| k).collect();
            if dead.is_empty() {
                return;
            }
            for sid in dead {
                self.remove_inner(sid);
            }
        }
    }

    pub fn remove(&mut self, sid: u32) {
        self.remove_inner(sid);
        self.sweep();
    }

    fn remove_inner(&mut self, sid: u32) {
        self.leave_inner(sid);
        if let Some(s) = self.sessions.remove(&sid) {
            if self.by_id.get(&s.id) == Some(&sid) {
                self.by_id.remove(&s.id);
            }
        }
    }

    /// Relays a whisper to `to`: a player in the sender's map and channel, or one who whispered the sender within
    /// `REPLY_WINDOW` (from any map). Dropped without a word to the sender when over the rate cap, to self, to anyone
    /// else (session ids can be counted through), or empty; `text` is already cleaned by the caller.
    pub fn whisper(&mut self, sid: u32, to: u32, text: String, now: Instant) {
        let Some(me) = self.sessions.get_mut(&sid) else { return };
        if !allow(&mut me.whisper_times, WHISPER_MAX, WHISPER_WINDOW, now) || to == sid || text.is_empty() {
            return;
        }
        let Some(o) = self.sessions.get(&to) else { return };
        let me = &self.sessions[&sid];
        let same_room = me.room.is_some() && (me.region, me.room) == (o.region, o.room);
        let reply = me.heard_from.get(&o.id).is_some_and(|t| now.duration_since(*t) < REPLY_WINDOW);
        if !same_room && !reply {
            return;
        }
        let (name, my_id) = (me.name.clone(), me.id);
        let o = self.sessions.get_mut(&to).expect("checked above");
        o.heard_from.retain(|_, t| now.duration_since(*t) < REPLY_WINDOW);
        o.heard_from.insert(my_id, now);
        o.push(Msg::PeerWhisper { from_session: sid, name, text: text.clone() });
        let to_who = who(o.id);
        let me = self.sessions.get_mut(&sid).expect("checked above");
        if me.whispers.len() == WHISPER_LOG {
            me.whispers.pop_front();
        }
        me.whispers.push_back((now, to_who, text));
        self.sweep();
    }

    /// A report as one `reports.jsonl` line, or `None` (dropped) when over the cap, about oneself or an unknown session.
    /// Metadata only: who, when, why, and the last 30 s of both players' positions and the target's last whispers.
    /// The trails are what the clients sent (only their speed was checked), so the line says `"trails":"client-claimed"`.
    pub fn report(&mut self, sid: u32, target: u32, target_who: PlayerId, reason: u8, note: &str, now: Instant) -> Option<String> {
        // the session must still be the account the reporter means (a session id is reused across server restarts)
        if self.sessions.get(&target).is_none_or(|t| player_id(t.id) != target_who) {
            return None;
        }
        let me = self.sessions.get_mut(&sid)?;
        if !allow(&mut me.report_times, REPORT_MAX, REPORT_WINDOW, now) || target == sid || reason > redex_proto::MAX_REPORT_REASON {
            return None;
        }
        let (a, b) = (self.sessions.get(&sid)?, self.sessions.get(&target)?);
        let ago = |t: &Instant| now.duration_since(*t).as_millis();
        let trail = |s: &Session| {
            let v: Vec<String> = s.trail.iter().filter(|(t, _)| now.duration_since(*t) <= TRAIL).map(|(t, p)| {
                format!("{{\"ago_ms\":{},\"map\":{},\"x\":{},\"y\":{},\"z\":{},\"mode\":{}}}", ago(t), p.map, p.x, p.y, p.z, p.mode)
            }).collect();
            v.join(",")
        };
        let whispers: Vec<String> = b.whispers.iter()
            .map(|(t, to, text)| format!("{{\"ago_ms\":{},\"to\":{},\"text\":{}}}", ago(t), json(to), json(text))).collect();
        let unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Some(format!(
            "{{\"time\":{unix},\"reporter\":{},\"reporter_session\":{sid},\"reporter_name\":{},\"target\":{},\"target_steam_id\":{},\"target_session\":{target},\"target_name\":{},\"reason\":{reason},\"note\":{},\"trails\":\"client-claimed\",\"reporter_trail\":[{}],\"target_trail\":[{}],\"target_whispers\":[{}]}}",
            json(&who(a.id)), json(&a.name), json(&who(b.id)), target_who.steam(), json(&b.name), json(note), trail(a), trail(b), whispers.join(",")
        ))
    }

    /// A new (already checked) appearance: kept for later peers and relayed to the room, at most once a second.
    pub fn set_appearance(&mut self, sid: u32, appearance: Vec<u8>, now: Instant) {
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        if s.last_appearance_at.is_some_and(|t| now.duration_since(t) < APPEARANCE_GAP) {
            s.pending_appearance = Some(appearance); // the newest one is applied by `tick`
            return;
        }
        s.pending_appearance = None;
        self.apply_appearance(sid, appearance, now);
    }

    fn apply_appearance(&mut self, sid: u32, appearance: Vec<u8>, now: Instant) {
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        s.last_appearance_at = Some(now);
        s.appearance = appearance.clone();
        let Some((map, ch)) = s.room else { return };
        let members = self.rooms[&(s.region, map)][ch as usize].clone();
        for m in members.into_iter().filter(|&m| m != sid) {
            if let Some(o) = self.sessions.get_mut(&m) {
                o.push(Msg::PeerAppearance { session: sid, appearance: appearance.clone() });
            }
        }
        self.sweep();
    }

    pub fn contains(&self, sid: u32) -> bool {
        self.sessions.contains_key(&sid)
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Everyone logged in: (session, name, login key), by session.
    pub fn list(&self) -> Vec<(u32, String, u128)> {
        let mut v: Vec<_> = self.sessions.iter().map(|(&sid, s)| (sid, s.name.clone(), s.id)).collect();
        v.sort_by_key(|e| e.0);
        v
    }

    /// Ends a session from the server's side (a lobby host's kick, or the lobby closing): it is sent `why` (a
    /// `Refuse`, which its connection passes on before it closes) and leaves the world. Its login key; None when there
    /// is no such session.
    pub fn end(&mut self, sid: u32, why: Msg) -> Option<u128> {
        let s = self.sessions.get(&sid)?;
        let key = s.id;
        // straight into the outbox: its thread takes what was queued before it sees the sender gone
        let _ = s.tx.try_send(why);
        self.remove(sid);
        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use redex_proto::{TITLE_OR, TITLE_X, TITLE_Y};
    const G: u8 = 6;

    fn add(w: &mut World, id: u128) -> (u32, Receiver<Msg>) {
        add_as(w, id, TITLE_X)
    }
    fn add_as(w: &mut World, id: u128, title: u64) -> (u32, Receiver<Msg>) {
        let (tx, rx) = outbox();
        (w.login(id, format!("p{id}"), G, title, vec![], tx).unwrap(), rx)
    }
    fn pos(map: u32) -> Pos {
        Pos { game: G, map, channel: 0, x: 1.0, y: 2.0, z: 3.0, facing: 0, mode: 0, t: 0 }
    }
    fn drain(rx: &Receiver<Msg>) -> Vec<Msg> {
        rx.try_iter().collect()
    }
    /// Enter at a time far enough from any earlier action.
    fn enter(w: &mut World, s: u32, map: u32) -> u16 {
        thread_local!(static T: std::cell::Cell<u64> = const { std::cell::Cell::new(0) });
        let n = T.with(|t| {
            t.set(t.get() + 1);
            t.get()
        });
        w.enter(s, G, map, Instant::now() + Duration::from_secs(n * 10)).unwrap()
    }

    /// A session ended by the server gets its refusal, then nothing: it is out of the world, and its mates see it go.
    #[test]
    fn an_ended_session_is_told_and_gone() {
        let mut w = World::default();
        let (a, ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2);
        enter(&mut w, a, 10);
        enter(&mut w, b, 10);
        drain(&ra);
        drain(&rb);
        assert_eq!(w.list().iter().map(|e| (e.0, e.2)).collect::<Vec<_>>(), [(a, 1), (b, 2)]);
        let why = Msg::Refuse { code: redex_proto::refuse::CLOSED, reason: "removed".into() };
        assert_eq!(w.end(b, why.clone()), Some(2));
        assert_eq!(drain(&rb), [why]);
        assert!(matches!(rb.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)), "its outbox is closed");
        assert!(matches!(drain(&ra)[..], [Msg::PeerGone { session }] if session == b));
        assert!(!w.contains(b) && w.len() == 1);
        assert_eq!(w.end(b, Msg::Bye), None);
    }

    #[test]
    fn relay_scoped_to_map_and_channel() {
        let mut w = World::default();
        let (a, ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2);
        let (c, rc) = add(&mut w, 3);
        enter(&mut w, a, 10);
        enter(&mut w, b, 10);
        enter(&mut w, c, 11);
        drain(&ra);
        drain(&rb);
        drain(&rc);
        w.pos(a, pos(10), Instant::now());
        assert!(matches!(drain(&rb)[..], [Msg::PeerPos { session, .. }] if session == a));
        assert!(drain(&rc).is_empty(), "other map gets nothing");
        assert!(drain(&ra).is_empty(), "no echo");
        w.pos(a, pos(11), Instant::now() + Duration::from_secs(1));
        assert!(drain(&rc).is_empty());
    }

    #[test]
    fn channel_overflow_at_cap() {
        let mut w = World::default();
        let mut rxs = vec![];
        for i in 0..(CHANNEL_CAP as u128 * 2 + 1) {
            let (s, rx) = add(&mut w, i);
            assert_eq!(enter(&mut w, s, 5) as usize, i as usize / CHANNEL_CAP);
            rxs.push((s, rx));
        }
        let (s16, _) = &rxs[CHANNEL_CAP];
        for (_, rx) in &rxs {
            drain(rx);
        }
        w.pos(*s16, pos(5), Instant::now());
        assert!(drain(&rxs[0].1).is_empty());
        assert_eq!(drain(&rxs[CHANNEL_CAP + 1].1).len(), 1);
        w.leave_inner(rxs[3].0);
        let (s, _rx) = add(&mut w, 999);
        assert_eq!(enter(&mut w, s, 5), 0);
    }

    #[test]
    fn rate_cap_keeps_the_newest_pos_and_leave_notifies() {
        let mut w = World::default();
        let (a, _ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2);
        enter(&mut w, a, 1);
        enter(&mut w, b, 1);
        drain(&rb);
        let t = Instant::now();
        let at = |x: f32| Pos { x, ..pos(1) };
        w.pos(a, at(1.0), t);
        w.pos(a, at(2.0), t + Duration::from_millis(20)); // held
        w.pos(a, at(3.0), t + Duration::from_millis(40)); // replaces the held one
        w.tick(a, t + Duration::from_millis(60)); // gap not over yet
        assert_eq!(drain(&rb).len(), 1);
        w.tick(a, t + Duration::from_millis(100));
        let got = drain(&rb);
        assert!(matches!(&got[..], [Msg::PeerPos { pos, .. }] if pos.x == 3.0), "{got:?}");
        w.tick(a, t + Duration::from_millis(300));
        assert!(drain(&rb).is_empty(), "nothing left to flush");
        w.remove(a);
        assert!(matches!(drain(&rb)[..], [Msg::PeerGone { session }] if session == a));
    }

    #[test]
    fn same_stable_id_replaces_old_session_with_a_reason() {
        let mut w = World::default();
        let (a, ra) = add(&mut w, 7);
        let (b, _rb) = add(&mut w, 7);
        assert_ne!(a, b);
        assert_eq!(w.len(), 1);
        assert_eq!(drain(&ra), vec![Msg::Refuse { code: redex_proto::refuse::ELSEWHERE, reason: "logged in elsewhere".into() }]);
    }

    #[test]
    fn different_game_is_refused() {
        let mut w = World::default();
        let (tx, _rx) = outbox();
        assert_eq!(w.login(1, "x".into(), G, TITLE_OR, vec![], tx.clone()), None, "title not on the allowlist yet");
        assert_eq!(w.login(1, "x".into(), G, 0x0004_0000_0012_3400, vec![], tx.clone()), None, "unknown title");
        assert_eq!(w.login(1, "x".into(), 7, TITLE_X, vec![], tx), None, "not X's generation");
        let (a, _ra) = add(&mut w, 2);
        assert_eq!(w.enter(a, 7, 1, Instant::now()), Err(Deny::Game));
        assert_eq!(w.enter(a, G, 70000, Instant::now()), Err(Deny::Map));
    }

    #[test]
    fn invalid_pos_is_dropped() {
        let mut w = World::default();
        let (a, _ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2);
        enter(&mut w, a, 1);
        enter(&mut w, b, 1);
        drain(&rb);
        let mut t = Instant::now();
        let bad = [
            Pos { x: f32::NAN, ..pos(1) },
            Pos { y: f32::INFINITY, ..pos(1) },
            Pos { z: f32::NEG_INFINITY, ..pos(1) },
            Pos { x: 1e9, ..pos(1) },
            Pos { facing: 4, ..pos(1) },
            Pos { mode: 9, ..pos(1) },
        ];
        for p in bad {
            t += Duration::from_secs(1);
            w.pos(a, p, t);
        }
        assert!(drain(&rb).is_empty());
        w.pos(a, pos(1), t + Duration::from_secs(1));
        assert_eq!(drain(&rb).len(), 1);
    }

    #[test]
    fn enter_leave_flood_is_cooled_down_then_closed() {
        let mut w = World::default();
        let (a, ra) = add(&mut w, 1);
        let t = Instant::now();
        assert!(w.enter(a, G, 1, t).is_ok());
        assert_eq!(drain(&ra), vec![Msg::Entered { map: 1, channel: 0, repeat: false }]);
        for i in 1..MOVE_STRIKES {
            assert_eq!(w.enter(a, G, 2, t + Duration::from_millis(i as u64)), Err(Deny::Cooldown));
            // the ignored move is answered with where the player still is
            assert_eq!(drain(&ra), vec![Msg::Entered { map: 1, channel: 0, repeat: true }]);
        }
        assert_eq!(w.leave(a, t + Duration::from_millis(50)), Err(Deny::Flood));
        // a patient client is fine again
        assert!(w.enter(a, G, 2, t + Duration::from_secs(1)).is_ok());
        drain(&ra);
        // off any map, an ignored move is answered with MapLeave
        assert!(w.leave(a, t + Duration::from_secs(2)).is_ok());
        assert_eq!(w.enter(a, G, 3, t + Duration::from_millis(2001)), Err(Deny::Cooldown));
        assert_eq!(drain(&ra), vec![Msg::MapLeave]);
    }

    #[test]
    fn slow_reader_is_dropped_not_buffered() {
        let mut w = World::default();
        let (a, _ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2); // rb is never drained
        enter(&mut w, a, 1);
        enter(&mut w, b, 1);
        let t = Instant::now();
        for i in 0..(OUTBOX_CAP as u64 + 10) {
            w.pos(a, pos(1), t + Duration::from_millis(100 * (i + 1)));
        }
        assert!(!w.contains(b), "full outbox drops the session");
        assert!(w.contains(a));
        assert!(rb.try_iter().count() <= OUTBOX_CAP);
    }

    /// A Steam login's peers see its SteamID64 (for "Add friend"), an account login its account id; a dev id shows
    /// only that it is one, never its key.
    #[test]
    fn peers_carry_the_verified_player_id() {
        let mut w = World::default();
        let steam = STEAM_KEY | 76_561_197_960_287_930;
        let (s, rs) = add(&mut w, steam);
        let (d, rd) = add(&mut w, 0x1234);
        let (a, ra) = add(&mut w, ACCOUNT_KEY | 42);
        for p in [s, d, a] {
            enter(&mut w, p, 4);
        }
        let (rd, rs, ra) = (drain(&rd), drain(&rs), drain(&ra));
        assert!(rd.iter().any(|m| matches!(m, Msg::Peer { session, who: PlayerId::Steam(76_561_197_960_287_930), .. } if *session == s)));
        assert!(rs.iter().any(|m| matches!(m, Msg::Peer { session, who: PlayerId::Dev, .. } if *session == d)));
        assert!(rs.iter().any(|m| matches!(m, Msg::Peer { session, who: PlayerId::Account(42), .. } if *session == a)));
        assert!(ra.iter().any(|m| matches!(m, Msg::Peer { session, who: PlayerId::Steam(_), .. } if *session == s)));
        // the three kinds of key never collide, whatever the id
        assert_eq!(player_id(STEAM_KEY | u128::from(u64::MAX)), PlayerId::Steam(u64::MAX));
        assert_eq!(player_id(ACCOUNT_KEY | u128::from(u64::MAX)), PlayerId::Account(u64::MAX));
        assert_eq!(player_id(u128::MAX >> 1), PlayerId::Dev);
    }

    #[test]
    fn x_and_y_share_kalos_and_hoenn_stays_apart() {
        let mut w = World { titles: redex_proto::KNOWN.iter().map(|k| k.0).collect(), ..World::default() };
        let (x, rx) = add_as(&mut w, 1, TITLE_X);
        let (y, ry) = add_as(&mut w, 2, TITLE_Y);
        let (h, rh) = add_as(&mut w, 3, TITLE_OR);
        for s in [x, y, h] {
            enter(&mut w, s, 10); // the same map id in Kalos and in Hoenn
        }
        // Y sees X (with X's title), X sees Y; the Hoenn player on "map 10" sees neither, and is seen by neither
        assert!(drain(&ry).iter().any(|m| matches!(m, Msg::Peer { session, title, .. } if *session == x && *title == TITLE_X)));
        let seen_by_x = drain(&rx);
        assert!(seen_by_x.iter().any(|m| matches!(m, Msg::Peer { session, title, .. } if *session == y && *title == TITLE_Y)));
        assert!(!seen_by_x.iter().any(|m| matches!(m, Msg::Peer { session, .. } if *session == h)), "{seen_by_x:?}");
        assert_eq!(drain(&rh), vec![Msg::Entered { map: 10, channel: 0, repeat: false }]);
        w.pos(x, pos(10), Instant::now());
        assert_eq!(drain(&ry).len(), 1);
        assert!(drain(&rh).is_empty());
    }

    #[test]
    fn whisper_rate_and_report_cap_are_per_sender_and_expire() {
        let mut w = World::default();
        let (a, _ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2);
        let t0 = Instant::now();
        enter(&mut w, a, 1);
        enter(&mut w, b, 1);
        drain(&rb);
        for k in 0..8 {
            w.whisper(a, b, format!("m{k}"), t0 + Duration::from_millis(k * 100));
        }
        assert_eq!(drain(&rb).len(), 5, "5 in 10 s, the rest dropped");
        w.whisper(a, b, "later".into(), t0 + Duration::from_secs(11));
        assert_eq!(drain(&rb).len(), 1, "the window moved on");
        w.whisper(a, 777, "nobody".into(), t0 + Duration::from_secs(12));
        w.whisper(a, a, "me".into(), t0 + Duration::from_secs(12));
        assert!(drain(&rb).is_empty());
        // a report naming another account than the session's is dropped (and costs nothing of the cap)
        assert!(w.report(a, b, PlayerId::Steam(765), 1, "wrong account", t0).is_none());
        let n = (0..5).filter(|k| w.report(a, b, PlayerId::Dev, 1, "n", t0 + Duration::from_secs(*k)).is_some()).count();
        assert_eq!(n, 3, "3 reports a minute");
        assert!(w.report(a, b, PlayerId::Dev, 1, "n", t0 + Duration::from_secs(61)).is_some());
        assert!(w.report(a, 777, PlayerId::Dev, 1, "unknown", t0 + Duration::from_secs(300)).is_none() && w.report(a, a, PlayerId::Dev, 1, "self", t0 + Duration::from_secs(300)).is_none());
        assert!(w.report(a, b, PlayerId::Dev, 9, "bad reason", t0 + Duration::from_secs(200)).is_none());
    }

    /// A whisper reaches a player on the sender's map and channel, or one who whispered the sender in the last
    /// `REPLY_WINDOW` (a reply, from any map); nobody else (session ids are small numbers anyone can count through).
    #[test]
    fn whispers_reach_the_same_channel_or_a_reply_only() {
        let mut w = World::default();
        let (a, ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2);
        let (c, rc) = add(&mut w, 3);
        let t0 = Instant::now();
        w.enter(a, G, 10, t0).unwrap();
        w.enter(b, G, 10, t0).unwrap();
        w.enter(c, G, 11, t0).unwrap();
        drain(&rb);
        drain(&rc);
        w.whisper(a, c, "other map".into(), t0);
        assert!(drain(&rc).is_empty(), "another map, never whispered: dropped");
        w.whisper(a, b, "hi".into(), t0);
        assert_eq!(drain(&rb).len(), 1, "same map and channel");
        // b moves away: a can't reach b now; b answers a from there, and then a may answer b back
        w.enter(b, G, 12, t0 + Duration::from_secs(1)).unwrap();
        drain(&rb);
        drain(&ra);
        w.whisper(a, b, "still there?".into(), t0 + Duration::from_secs(2));
        assert!(drain(&rb).is_empty());
        w.whisper(b, a, "a reply".into(), t0 + Duration::from_secs(3));
        assert!(drain(&ra).iter().any(|m| matches!(m, Msg::PeerWhisper { text, .. } if text == "a reply")), "a whispered b in the window");
        w.whisper(a, b, "and back".into(), t0 + Duration::from_secs(4));
        assert_eq!(drain(&rb).len(), 1, "b whispered a just now");
        // the reply right ends with the window
        w.whisper(b, a, "late".into(), t0 + Duration::from_secs(4) + REPLY_WINDOW);
        assert!(drain(&ra).is_empty());
        // the same map id in another channel is another room
        let mut w = World::default();
        let mut s = Vec::new();
        for i in 0..=CHANNEL_CAP as u128 {
            let (sid, rx) = add(&mut w, 100 + i);
            w.enter(sid, G, 5, t0).unwrap();
            s.push((sid, rx));
        }
        drain(&s[CHANNEL_CAP].1);
        w.whisper(s[0].0, s[CHANNEL_CAP].0, "channel 0 to 1".into(), t0);
        assert!(drain(&s[CHANNEL_CAP].1).is_empty());
    }

    /// A Pos further from the last relayed one than `MAX_STEP` per 100 ms (at least 100 ms' worth) is dropped, except
    /// the first after entering a map; the allowance grows with the time since, so a warp inside a map comes through late.
    #[test]
    fn a_pos_too_far_from_the_last_is_dropped() {
        let mut w = World::default();
        let (a, _ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2);
        let t0 = Instant::now();
        w.enter(a, G, 1, t0).unwrap();
        w.enter(b, G, 1, t0).unwrap();
        drain(&rb);
        let at = |x: f32| Pos { x, ..pos(1) };
        let ms = Duration::from_millis;
        w.pos(a, at(5000.0), t0);
        assert_eq!(drain(&rb).len(), 1, "the first after Entered goes anywhere");
        w.pos(a, at(5000.0 + MAX_STEP), t0 + ms(100));
        assert_eq!(drain(&rb).len(), 1, "one step");
        w.pos(a, at(5000.0 + 3.0 * MAX_STEP), t0 + ms(200));
        assert!(drain(&rb).is_empty(), "two steps' worth in 100 ms");
        w.pos(a, at(5000.0 + 3.0 * MAX_STEP), t0 + ms(300));
        assert_eq!(drain(&rb).len(), 1, "200 ms after the last relayed one, two steps are fine");
        w.pos(a, at(100.0), t0 + ms(400));
        assert!(drain(&rb).is_empty(), "a teleport");
        // a new map: anywhere again
        w.enter(a, G, 2, t0 + Duration::from_secs(1)).unwrap();
        w.enter(b, G, 2, t0 + Duration::from_secs(1)).unwrap();
        drain(&rb);
        w.pos(a, Pos { x: -9000.0, ..pos(2) }, t0 + Duration::from_secs(1));
        assert!(drain(&rb).iter().any(|m| matches!(m, Msg::PeerPos { .. })));
        // a report says the trails are what the clients claimed
        let line = w.report(b, a, PlayerId::Dev, 1, "", t0 + Duration::from_secs(2)).unwrap();
        assert!(line.contains("\"trails\":\"client-claimed\""), "{line}");
    }

    #[test]
    fn a_throttled_appearance_update_is_delivered_later() {
        let mut w = World::default();
        let (a, _ra) = add(&mut w, 1);
        let (b, rb) = add(&mut w, 2);
        let t0 = Instant::now();
        w.enter(a, G, 5, t0).unwrap();
        w.enter(b, G, 5, t0).unwrap();
        drain(&rb);
        w.set_appearance(a, vec![6, 1, 0, 1], t0);
        w.set_appearance(a, vec![6, 1, 0, 2], t0 + Duration::from_millis(300)); // held
        w.tick(a, t0 + Duration::from_millis(500));
        assert_eq!(drain(&rb).len(), 1, "only the first so far");
        w.tick(a, t0 + Duration::from_millis(1100));
        assert_eq!(drain(&rb), vec![Msg::PeerAppearance { session: a, appearance: vec![6, 1, 0, 2] }]);
        // a late joiner sees the newest look
        let (c, rc) = add(&mut w, 3);
        w.enter(c, G, 5, t0 + Duration::from_secs(5)).unwrap();
        assert!(drain(&rc).iter().any(|m| matches!(m, Msg::Peer { session, appearance, .. } if *session == a && appearance == &vec![6, 1, 0, 2])));
    }
}
