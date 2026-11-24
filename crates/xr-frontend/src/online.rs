//! Multiplayer over the game: names above the other players on the top screen, the Players list, the
//! player menu (Whisper, Add friend, Report), whispers and their log, and the server browser. The runtime hands a
//! [`View`] over each frame (who is near, where their heads are on the top screen, new whispers, Steam's server list)
//! and does the [`Request`]s; everything here is drawn by egui over the window, never into the 3DS screens. Every
//! panel works with the mouse, the keyboard (arrows, Enter, Esc) and a controller alone (d-pad, A, B, the Players
//! button), and animates in and out like the Configure window.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Id, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use crate::menus::{self, Menus, ACCENT};
use crate::settings::Orientation;

#[path = "friends_ui.rs"]
mod friends_ui;
pub use friends_ui::{friends_sorted, FRIENDS_AGAIN, INVITE_ARM};
pub use xr_net::friends::{Friend, FriendMsg, Limiter};

/// A player on our map, as the runtime sees them this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    /// the server's session id, what whispers and reports name
    pub session: u32,
    /// already cleaned of control and bidi characters (xr-net)
    pub name: String,
    /// their SteamID64, 0 for a test id (nobody to befriend)
    pub steam: u64,
    /// "X" or "Y", the version they play (X and Y share Kalos)
    pub version: &'static str,
    /// how far, in tiles
    pub tiles: f32,
    /// over their head on the top screen, in the console's pixels (400x240, y down), while their puppet is drawn and
    /// the field camera is up; None otherwise (no name then)
    pub label: Option<[f32; 2]>,
}

/// A whisper that came in.
#[derive(Debug, Clone, PartialEq)]
pub struct Whisper {
    pub from: u32,
    /// the sender's SteamID64 (0: a test id, or not known), what Ignore goes by
    pub steam: u64,
    pub name: String,
    pub text: String,
}

/// A world server in Steam's list.
#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    /// what Join connects to, `steam:<id>`
    pub address: String,
    /// empty when Steam could not ask the server for it
    pub name: String,
    /// None when Steam could not ask the server itself (the count is unknown)
    pub players: Option<u32>,
    pub max: Option<u32>,
    /// None when the server did not answer Steam's ping
    pub ping: Option<u32>,
}

/// What the runtime says this frame.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// the world joined or being joined (its address), none offline
    pub world: Option<String>,
    /// logged in to it
    pub online: bool,
    pub peers: Vec<Peer>,
    /// whispers since the last frame
    pub whispers: Vec<Whisper>,
    /// Steam's server list as far as it came, and whether a query still runs
    pub servers: Vec<Server>,
    pub searching: bool,
    /// things to tell the player (a toast each)
    pub notes: Vec<String>,
    /// a world being joined (until it logs in or gives up): the current one stays meanwhile
    pub joining: Option<String>,
    /// what Steam's gamepad keyboard gave back (`Request::Type`)
    pub typed: Option<String>,
    /// Steam friends online, as last asked (`Request::FriendsOpened`)
    pub friends: Vec<Friend>,
    /// friends' frames since the last frame, past the link's gate (taken once each, like whispers)
    pub friend_msgs: Vec<(u64, FriendMsg)>,
    /// where this Steam account's Ignore list is kept (`<data>/user/ignored-<steamid>.txt`), once Steam is up
    pub ignore_file: Option<std::path::PathBuf>,
    /// sends to friends that failed since the last frame: (to, the chat text when it was one, why); a session Steam gave
    /// up on has no text (taken once each)
    pub friend_failed: Vec<(u64, Option<String>, String)>,
}

/// Why a player is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Cheating,
    Harassment,
    Name,
    Other,
}

pub const REASONS: [(Reason, &str); 4] =
    [(Reason::Cheating, "Cheating"), (Reason::Harassment, "Harassment or spam"), (Reason::Name, "Offensive name"), (Reason::Other, "Something else")];

/// What the player asked the runtime for.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// `name` for the "sent" note once the runtime took it
    Whisper { to: u32, name: String, text: String },
    /// `steam_id`: the account meant (0 for a test id); the server drops the report if the session is someone else now
    Report { session: u32, steam_id: u64, name: String, reason: Reason, note: String },
    /// Steam's overlay on "add friend" for this SteamID64
    AddFriend(u64),
    /// query Steam's server list again
    Search,
    /// join a world: `steam:<id>` or `host:port`
    Join(String),
    /// Steam's gamepad keyboard for the whisper box or the chat (a controller alone), starting from this text
    Type(String),
    /// chat text to a friend running RE:DEX (cleaned and rate-limited here)
    FriendChat { to: u64, text: String },
    /// "Invite to my world": the current (Steam) world's name and `steam:<id>` to this friend
    Invite { to: u64 },
    /// Join on a friend's invite: through the usual join, every check of it
    AcceptInvite { from: u64, connect: String },
    /// Not now on a friend's invite
    DeclineInvite { from: u64 },
    /// the Friends tab is open: ask friends' presence again (on opening, then every FRIENDS_AGAIN)
    FriendsOpened,
    /// the Steam accounts ignored now (the friends link refuses their sessions and frames)
    Ignored(Vec<u64>),
}

/// One step of the keyboard or a controller in the panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    /// tabs: Players nearby / Friends, and the chat's friends
    Left,
    Right,
    Accept,
    Back,
}

/// A whisper's characters at most, as the server cuts it.
pub const WHISPER_CHARS: usize = 120;
/// A report note's characters at most.
pub const NOTE_CHARS: usize = 200;
/// Whispers the server lets through: 5 in 10 s; more are dropped there, so the box says so here instead.
pub const WHISPER_MAX: usize = 5;
pub const WHISPER_WINDOW: Duration = Duration::from_secs(10);
/// The server list asks Steam again this often while it is open (a listing shows in only some answers).
pub const SEARCH_AGAIN: Duration = Duration::from_secs(20);
/// A server seen once stays listed this long (a later answer may miss it).
pub const SERVER_KEPT: Duration = Duration::from_secs(180);
/// Lines the whisper log keeps.
const LOG_LINES: usize = 50;
/// How long the log stays up after a whisper, unless pinned open.
const LOG_SHOWN: Duration = Duration::from_secs(8);
/// Within this many console pixels of the top screen's edge a name fades out.
const EDGE_FADE: f32 = 24.0;
const QUICK: f32 = 0.18;

/// The text a whisper box sends: trimmed, cut to WHISPER_CHARS, none when empty.
pub fn whisper_text(text: &str) -> Option<String> {
    let text: String = text.trim().chars().take(WHISPER_CHARS).collect();
    (!text.is_empty()).then_some(text)
}

/// A point of the top screen (console pixels, 400x240, y down) in the window, given where the presenter put the top
/// screen (its rectangle, in the window's points) and how it is turned.
pub fn place(p: [f32; 2], top: Rect, orientation: Orientation) -> Pos2 {
    let (u, v) = (p[0] / 400.0, p[1] / 240.0);
    // the inverse of the touch mapping in lib.rs
    let (x, y) = match orientation {
        Orientation::North => (u, v),
        Orientation::East => (1.0 - v, u),
        Orientation::South => (1.0 - u, 1.0 - v),
        Orientation::West => (v, 1.0 - u),
    };
    Pos2::new(top.left() + x * top.width(), top.top() + y * top.height())
}

/// How much of a name at p shows: whole inside the top screen, fading out near its edges, none past them.
pub fn edge_alpha(p: [f32; 2]) -> f32 {
    let inside = p[0].min(400.0 - p[0]).min(p[1]).min(240.0 - p[1]);
    (inside / EDGE_FADE).clamp(0.0, 1.0)
}

/// The size of a name over the top screen: the console's font size times the screen's scale, readable from the
/// smallest window to fullscreen.
pub fn label_size(top: Rect) -> f32 {
    (top.width().min(top.height()) / 240.0 * 7.0).clamp(11.0, 26.0)
}

/// Who a panel is about, and in which world: session ids start over in every server, so a target of another world
/// is never whispered to or reported.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    session: u32,
    name: String,
    steam: u64,
    version: &'static str,
    world: Option<String>,
}

impl Target {
    fn of(p: &Peer, world: &Option<String>) -> Target {
        Target { session: p.session, name: p.name.clone(), steam: p.steam, version: p.version, world: world.clone() }
    }
}

/// The player menu: who, where it opened (by their name, or beside the list), the action the keyboard is on.
struct PlayerMenu {
    who: Target,
    at: Option<Pos2>,
    focus: usize,
}

const ACTIONS: [&str; 4] = ["Whisper", "Add friend", "Report\u{2026}", "Ignore"];

struct ReportBox {
    who: Target,
    reason: usize,
    note: String,
    /// 0..4 the reasons, 4 Send, 5 Cancel
    focus: usize,
}

struct Line {
    /// the other player's session and name, and the world they whispered in
    session: u32,
    name: String,
    world: Option<String>,
    text: String,
    mine: bool,
}

/// The multiplayer panels and what they remember between frames.
pub struct Online {
    pub players: bool,
    /// the row the keyboard is on in the list (the peers, then Whispers, then Browse servers)
    focus: usize,
    menu: Option<PlayerMenu>,
    menu_open: bool,
    /// the whisper box: who to, the text
    whisper: Option<(Target, String)>,
    whisper_open: bool,
    whisper_focus: bool,
    /// the whisper box was opened with a controller: A types with Steam's keyboard, then sends
    whisper_pad: bool,
    report: Option<ReportBox>,
    report_open: bool,
    log: VecDeque<Line>,
    /// the log shows till then after a whisper, or always while pinned
    log_until: Option<Instant>,
    pub log_pinned: bool,
    unread: usize,
    pub browser: bool,
    direct: String,
    /// the server row picked
    picked: usize,
    searched: Option<Instant>,
    servers: Vec<(Server, Instant)>,
    limiter: Limiter,
    /// names still on screen: where they were last, for their fade out
    labels: HashMap<u32, (Pos2, String, &'static str)>,
    was_online: Option<bool>,
    /// the world of the last frame (Some once seen): when it changes the panels about its players close
    world: Option<Option<String>>,
    /// players whose whispers, chat and invites are dropped here: (Steam account, or 0 for a test id; world and session,
    /// what a test id is known by). Steam accounts are kept on disk per own account (friends_ui: load_ignored)
    ignored: Vec<(u64, Option<String>, u32)>,
    /// the Friends tab, chats and invite prompt
    fr: friends_ui::FriendsUi,
    nav: Vec<Nav>,
    pub requests: Vec<Request>,
    pub motion: bool,
}

impl Default for Online {
    fn default() -> Online {
        Online {
            players: false,
            focus: 0,
            menu: None,
            menu_open: false,
            whisper: None,
            whisper_open: false,
            whisper_focus: false,
            whisper_pad: false,
            report: None,
            report_open: false,
            log: VecDeque::new(),
            log_until: None,
            log_pinned: false,
            unread: 0,
            browser: false,
            direct: String::new(),
            picked: 0,
            searched: None,
            servers: Vec::new(),
            limiter: Limiter::default(),
            labels: HashMap::new(),
            was_online: None,
            world: None,
            ignored: Vec::new(),
            fr: friends_ui::FriendsUi::default(),
            nav: Vec::new(),
            requests: Vec::new(),
            motion: true,
        }
    }
}

impl Online {
    fn time(&self, seconds: f32) -> f32 {
        if self.motion { seconds } else { 0.0 }
    }

    /// joined to a world, connected or not (a dropped connection is still the world's)
    pub fn in_world(&self) -> bool {
        self.world.as_ref().is_some_and(Option::is_some)
    }

    /// is this player ignored: their Steam account (they may come back as another session), or this world's session
    fn ignoring(&self, session: u32, steam: u64, world: &Option<String>) -> bool {
        // a Steam account by its id only: sessions start over when a server restarts, so another player gets theirs
        self.ignored.iter().any(|(s, w, id)| if *s != 0 { *s == steam } else { steam == 0 && w == world && *id == session })
    }

    /// Ignore, or Stop ignoring, the player the menu is about
    fn toggle_ignore(&mut self, who: &Target, menus: &mut Menus) {
        if self.ignoring(who.session, who.steam, &who.world) {
            self.ignored.retain(|(s, w, id)| if *s != 0 { *s != who.steam } else { !(who.steam == 0 && *w == who.world && *id == who.session) });
            menus.toast(format!("{} comes through again", who.name));
        } else {
            self.ignored.push((who.steam, who.world.clone(), who.session));
            menus.toast(if who.steam != 0 {
                format!("Ignoring {}: their whispers, chat and invites are dropped", who.name)
            } else {
                format!("Ignoring {}: their whispers are dropped until RE:DEX closes", who.name)
            });
        }
        if who.steam != 0 {
            self.ignore_changed();
        }
    }

    /// a panel is up that takes the keyboard and the controller from the game (the game itself keeps running)
    pub fn takes_input(&self) -> bool {
        self.players || self.menu_open || self.whisper_open || self.report_open || self.browser || self.fr.menu_open || self.fr.chat_open || self.fr.invite.is_some()
    }

    /// a controller's step, done on the next frame
    pub fn nav(&mut self, nav: Nav) {
        self.nav.push(nav);
    }

    /// the Players button: opens the list, or closes whatever is up
    pub fn toggle_players(&mut self) {
        if self.takes_input() {
            self.close_all();
        } else {
            self.players = true;
            self.focus = 0;
        }
    }

    pub fn open_browser(&mut self) {
        self.close_all();
        self.browser = true;
        self.picked = 0;
        // asked again on every open
        self.searched = None;
    }

    fn close_all(&mut self) {
        self.players = false;
        self.menu_open = false;
        self.whisper_open = false;
        self.report_open = false;
        self.browser = false;
        (self.fr.menu_open, self.fr.chat_open) = (false, false);
        // the prompt goes too: Not now
        self.decline_invite();
    }

    fn open_menu(&mut self, who: Target, at: Option<Pos2>) {
        self.menu = Some(PlayerMenu { who, at, focus: 0 });
        self.menu_open = true;
    }

    fn open_whisper(&mut self, who: Target, pad: bool) {
        (self.menu_open, self.fr.chat_open) = (false, false);
        self.whisper = Some((who, String::new()));
        self.whisper_open = true;
        self.whisper_focus = true;
        self.whisper_pad = pad;
        if pad {
            // no keys at hand: Steam's on-screen keyboard (the runtime says so when it cannot show it)
            self.requests.push(Request::Type(String::new()));
        }
    }

    /// the world the panels' targets came from is still the one we are in
    fn here(&self, who: &Target) -> bool {
        self.world.as_ref().is_some_and(|w| *w == who.world)
    }

    fn open_report(&mut self, who: Target) {
        self.menu_open = false;
        self.report = Some(ReportBox { who, reason: 0, note: String::new(), focus: 0 });
        self.report_open = true;
    }

    fn line(&mut self, session: u32, name: &str, text: &str, mine: bool) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        let world = self.world.clone().flatten();
        self.log.push_back(Line { session, name: name.to_owned(), world, text: text.to_owned(), mine });
        self.log_until = Some(Instant::now() + LOG_SHOWN);
    }

    /// does an action of the player menu
    fn act(&mut self, action: usize, menus: &mut Menus, online: bool, pad: bool) {
        let Some(who) = self.menu.as_ref().map(|m| m.who.clone()) else { return };
        match action {
            0 => self.open_whisper(who, pad),
            1 => {
                self.menu_open = false;
                if who.steam == 0 {
                    menus.toast(format!("{} has no Steam account to add (a test id)", who.name));
                } else {
                    self.requests.push(Request::AddFriend(who.steam));
                }
            }
            3 => {
                self.menu_open = false;
                self.toggle_ignore(&who, menus);
            }
            _ if online => self.open_report(who),
            _ => {
                self.menu_open = false;
                menus.toast("Not connected to a world: reports go to its server");
            }
        }
    }

    /// sends what the whisper box holds
    fn send_whisper(&mut self, menus: &mut Menus, online: bool) {
        let Some((who, text)) = &self.whisper else { return };
        let Some(text) = whisper_text(text) else { return };
        let who = who.clone();
        if !self.here(&who) {
            menus.toast(format!("{} was in another world", who.name));
            self.whisper_open = false;
            return;
        }
        if !online {
            menus.toast("Not connected to a world");
            return;
        }
        if !self.limiter.allow(Instant::now(), WHISPER_MAX, WHISPER_WINDOW) {
            menus.toast(format!("Whispers: slow down, at most {WHISPER_MAX} every {} seconds", WHISPER_WINDOW.as_secs()));
            return;
        }
        // the runtime says "sent" once its client took it, or why not
        self.requests.push(Request::Whisper { to: who.session, name: who.name.clone(), text: text.clone() });
        self.line(who.session, &who.name, &text, true);
        self.whisper_open = false;
    }

    fn send_report(&mut self, menus: &mut Menus) {
        let Some(r) = &self.report else { return };
        if !self.here(&r.who) {
            menus.toast(format!("{} was in another world", r.who.name));
        } else {
            let note: String = r.note.trim().chars().take(NOTE_CHARS).collect();
            let (session, steam_id, name) = (r.who.session, r.who.steam, r.who.name.clone());
            self.requests.push(Request::Report { session, steam_id, name, reason: REASONS[r.reason].0, note });
        }
        self.report_open = false;
    }

    /// the steps of this frame, from a controller (true) and from the keys egui saw (false)
    fn steps(&mut self, ctx: &egui::Context) -> Vec<(Nav, bool)> {
        let mut steps: Vec<(Nav, bool)> = std::mem::take(&mut self.nav).into_iter().map(|n| (n, true)).collect();
        let typing = ctx.egui_wants_keyboard_input();
        ctx.input(|input| {
            if input.key_pressed(egui::Key::Escape) {
                steps.push((Nav::Back, false));
            }
            if !typing {
                let keys = [(egui::Key::ArrowUp, Nav::Up), (egui::Key::ArrowDown, Nav::Down), (egui::Key::ArrowLeft, Nav::Left), (egui::Key::ArrowRight, Nav::Right), (egui::Key::Enter, Nav::Accept), (egui::Key::Space, Nav::Accept)];
                for (key, nav) in keys {
                    if input.key_pressed(key) {
                        steps.push((nav, false));
                    }
                }
            }
        });
        steps
    }

    /// does a step in the panel on top
    fn step(&mut self, nav: Nav, pad: bool, view: &View, menus: &mut Menus) {
        if self.friend_step(nav, pad, view, menus) {
            return;
        }
        if self.report_open {
            let Some(r) = &mut self.report else { return };
            match nav {
                Nav::Up => r.focus = r.focus.saturating_sub(1),
                Nav::Down => r.focus = (r.focus + 1).min(5),
                Nav::Accept if r.focus < 4 => {
                    r.reason = r.focus;
                    r.focus = 4;
                }
                Nav::Accept if r.focus == 4 => self.send_report(menus),
                Nav::Accept | Nav::Back => self.report_open = false,
                Nav::Left | Nav::Right => {}
            }
        } else if self.whisper_open {
            // the box is typed into; with a controller, A types with Steam's keyboard, and sends once there is text
            match nav {
                Nav::Back => self.whisper_open = false,
                Nav::Accept if pad => match self.whisper.as_ref().map(|(_, t)| (whisper_text(t).is_some(), t.clone())) {
                    Some((true, _)) => self.send_whisper(menus, view.online),
                    Some((false, text)) => {
                        self.whisper_pad = true;
                        self.requests.push(Request::Type(text));
                    }
                    None => {}
                },
                _ => {}
            }
        } else if self.menu_open {
            let Some(m) = &mut self.menu else { return };
            match nav {
                Nav::Up => m.focus = m.focus.saturating_sub(1),
                Nav::Down => m.focus = (m.focus + 1).min(ACTIONS.len() - 1),
                Nav::Accept => {
                    let focus = m.focus;
                    self.act(focus, menus, view.online, pad);
                }
                Nav::Back => self.menu_open = false,
                Nav::Left | Nav::Right => {}
            }
        } else if self.browser {
            let rows = self.listed().len();
            match nav {
                Nav::Up => self.picked = self.picked.saturating_sub(1),
                Nav::Down => self.picked = (self.picked + 1).min(rows.saturating_sub(1)),
                Nav::Accept => {
                    if let Some(s) = self.listed().get(self.picked).map(|s| s.address.clone()) {
                        self.join(s, menus);
                    }
                }
                Nav::Back => self.browser = false,
                Nav::Left | Nav::Right => {}
            }
        } else if self.players {
            // the peers, then Whispers, then Browse servers
            let rows = view.peers.len() + 2;
            match nav {
                Nav::Up => self.focus = self.focus.saturating_sub(1),
                Nav::Down => self.focus = (self.focus + 1).min(rows - 1),
                Nav::Accept => self.pick_row(self.focus, view),
                Nav::Back => self.players = false,
                Nav::Right => self.show_friends_tab(true),
                Nav::Left => {}
            }
        }
    }

    fn pick_row(&mut self, row: usize, view: &View) {
        let peers = sorted(&view.peers);
        if let Some(p) = peers.get(row) {
            self.open_menu(Target::of(p, &view.world), None);
        } else if row == peers.len() {
            self.log_pinned = !self.log_pinned;
            self.unread = 0;
        } else {
            self.open_browser();
        }
    }

    fn join(&mut self, address: String, menus: &mut Menus) {
        menus.toast(format!("Joining {address}"));
        self.requests.push(Request::Join(address));
        self.browser = false;
    }

    /// the servers seen lately, the fullest first
    fn listed(&self) -> Vec<Server> {
        let mut list: Vec<Server> = self.servers.iter().filter(|(_, seen)| seen.elapsed() < SERVER_KEPT).map(|(s, _)| s.clone()).collect();
        list.sort_by(|a, b| b.players.cmp(&a.players).then(a.name.cmp(&b.name)));
        list
    }

    /// everything multiplayer over the game, for a frame. `top` is the top screen's rectangle in the window (points),
    /// none when it does not show; `names` the "Show player names" setting; `active` false while Configure is open
    /// (the panels then take no keys)
    pub fn ui(&mut self, ctx: &egui::Context, view: &View, top: Option<Rect>, orientation: Orientation, names: bool, active: bool, menus: &mut Menus) {
        // another world: its session ids mean other players here, so what was open about one closes, and its names go
        if self.world.as_ref().is_some_and(|w| *w != view.world) {
            (self.menu_open, self.whisper_open, self.report_open) = (false, false, false);
            (self.menu, self.whisper, self.report) = (None, None, None);
            self.labels.clear();
        }
        self.world = Some(view.world.clone());
        for note in &view.notes {
            menus.toast(note.clone());
        }
        self.friends_frame(view, menus);
        for w in &view.whispers {
            if self.ignoring(w.from, w.steam, &view.world) {
                continue;
            }
            self.line(w.from, &w.name, &w.text, false);
            menus.toast(format!("{} whispers: {}", w.name, short(&w.text, 48)));
            self.unread += 1;
        }
        if self.was_online.replace(view.online).is_some_and(|was| was != view.online) {
            menus.toast(if view.online { "Online: connected to the world" } else { "Offline: lost the world, reconnecting" });
        }
        let now = Instant::now();
        for s in &view.servers {
            self.servers.retain(|(x, _)| x.address != s.address);
            self.servers.push((s.clone(), now));
        }
        if self.browser && !view.searching && self.searched.is_none_or(|t| t.elapsed() >= SEARCH_AGAIN) {
            self.searched = Some(now);
            self.requests.push(Request::Search);
        }
        if let (Some(typed), true) = (&view.typed, self.whisper_open) {
            if let Some((_, text)) = &mut self.whisper {
                *text = typed.chars().take(WHISPER_CHARS).collect();
            }
        }
        if active {
            for (nav, pad) in self.steps(ctx) {
                self.step(nav, pad, view, menus);
            }
        } else {
            self.nav.clear();
        }
        // a whisper box or menu for someone who left the map: still useful (whispers reach any map), kept

        match top {
            Some(top) => self.labels(ctx, view, top, orientation, names, active),
            // no top screen: nothing to fade out at places of an old layout when it comes back
            None => self.labels.clear(),
        }
        self.players_panel(ctx, view);
        self.menu_panel(ctx, view, menus);
        self.whisper_panel(ctx, view, menus);
        self.report_panel(ctx, menus);
        self.browser_panel(ctx, view, menus);
        self.log_panel(ctx);
        self.friend_menu_panel(ctx, view, menus);
        self.chat_panel(ctx, menus);
        self.invite_panel(ctx, menus);
    }

    /// the names over the players' heads, fading in and out, a click opens the player menu
    fn labels(&mut self, ctx: &egui::Context, view: &View, top: Rect, orientation: Orientation, names: bool, active: bool) {
        let size = label_size(top);
        let mut seen = Vec::new();
        for p in &view.peers {
            let Some(at) = p.label.filter(|_| names) else { continue };
            let alpha = edge_alpha(at);
            if alpha <= 0.0 {
                continue;
            }
            seen.push(p.session);
            let id = Id::new(("name", p.session));
            // a first appearance starts from nothing (egui would start an unknown value at its target)
            if self.labels.insert(p.session, (place(at, top, orientation), p.name.clone(), p.version)).is_none() {
                ctx.animate_value_with_time(id, 0.0, self.time(0.25));
            }
            let shown = ctx.animate_value_with_time(id, alpha, self.time(0.25));
            // not clickable behind Configure
            if self.label(ctx, p.session, size, shown, top, active) {
                self.open_menu(Target::of(p, &view.world), self.labels.get(&p.session).map(|l| l.0));
            }
        }
        // the ones gone this frame fade out where they were (a quarter second at most)
        let gone: Vec<u32> = self.labels.keys().filter(|s| !seen.contains(s)).copied().collect();
        for session in gone {
            let shown = ctx.animate_value_with_time(Id::new(("name", session)), 0.0, self.time(0.15));
            if shown <= 0.0 {
                self.labels.remove(&session);
            } else {
                self.label(ctx, session, size, shown, top, false);
            }
        }
    }

    /// one name, a pill over the head with the version's dot; true when clicked
    fn label(&self, ctx: &egui::Context, session: u32, size: f32, alpha: f32, clip: Rect, clickable: bool) -> bool {
        let Some((at, name, version)) = self.labels.get(&session).cloned() else { return false };
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Background, Id::new(("name layer", session)))).with_clip_rect(clip);
        let galley = painter.layout_no_wrap(name, FontId::proportional(size), Color32::WHITE);
        let dot = size * 0.32;
        let pad = Vec2::new(size * 0.55, size * 0.22);
        let width = galley.size().x + pad.x * 2.0 + dot * 2.0 + size * 0.3;
        let rect = Rect::from_center_size(at - Vec2::new(0.0, galley.size().y / 2.0 + pad.y + size * 0.25), Vec2::new(width, galley.size().y + pad.y * 2.0));
        let response = egui::Area::new(Id::new(("name area", session)))
            .order(egui::Order::Background)
            .fixed_pos(rect.min)
            .interactable(clickable && alpha > 0.5)
            .show(ctx, |ui| ui.allocate_exact_size(rect.size(), Sense::click()).1)
            .inner;
        let hover = ctx.animate_bool_with_time(response.id.with("hover"), response.hovered(), self.time(0.12));
        let fill = Color32::from_black_alpha((120.0 + 60.0 * hover) as u8).gamma_multiply(alpha);
        // a soft shadow under the pill, then the pill, then the name with a 1 px shadow
        painter.rect_filled(rect.translate(Vec2::new(0.0, 1.5)).expand(1.0), rect.height() / 2.0, Color32::from_black_alpha((50.0 * alpha) as u8));
        painter.rect(rect, rect.height() / 2.0, fill, Stroke::new(1.0, ACCENT.gamma_multiply(hover * alpha)), egui::StrokeKind::Inside);
        let dot_at = Pos2::new(rect.left() + pad.x + dot, rect.center().y);
        painter.circle_filled(dot_at, dot, version_color(version).gamma_multiply(alpha));
        let text_at = Pos2::new(dot_at.x + dot + size * 0.3, rect.top() + pad.y);
        painter.galley(text_at + Vec2::new(0.0, 1.0), galley.clone(), Color32::from_black_alpha((160.0 * alpha) as u8));
        painter.galley(text_at, galley, Color32::WHITE.gamma_multiply(alpha));
        // a small point down to the head
        let tip = Pos2::new(rect.center().x, rect.bottom() + size * 0.25);
        let half = size * 0.22;
        painter.add(egui::Shape::convex_polygon(vec![Pos2::new(tip.x - half, rect.bottom()), Pos2::new(tip.x + half, rect.bottom()), tip], fill, Stroke::NONE));
        if response.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response.clicked()
    }

    /// the Players list, sliding in from the left
    fn players_panel(&mut self, ctx: &egui::Context, view: &View) {
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("players shown"), self.players, self.time(QUICK)));
        if t <= 0.0 {
            return;
        }
        let peers = sorted(&view.peers);
        let rows = peers.len() + 2;
        self.focus = self.focus.min(rows - 1);
        self.fr.focus = self.fr.focus.min(view.friends.len().saturating_sub(1));
        let (mut picked, mut friend, mut tab) = (None, None, None);
        egui::Area::new(Id::new("players"))
            .anchor(Align2::LEFT_TOP, Vec2::new(12.0 - (1.0 - t) * 36.0, 12.0))
            .interactable(self.players)
            .order(egui::Order::Middle)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                card(ui, 280.0, |ui| {
                    tab = self.tabs(ui, peers.len());
                    let (status, color) = match (&view.world, view.online) {
                        _ if view.joining.is_some() => (format!("Joining {}\u{2026}", short(view.joining.as_deref().unwrap_or(""), 26)), menus::NOTE),
                        (Some(w), true) => (format!("Online \u{b7} {}", short(w, 30)), Color32::from_rgb(0x3C, 0xB3, 0x71)),
                        (Some(_), false) => ("Connecting\u{2026}".to_owned(), menus::NOTE),
                        (None, _) => ("Offline".to_owned(), ui.visuals().weak_text_color()),
                    };
                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 4.0, color);
                        ui.label(RichText::new(status).small().weak());
                    });
                    ui.add_space(4.0);
                    if self.fr.tab_friends {
                        friend = self.friend_rows(ui, view);
                        return;
                    }
                    if peers.is_empty() {
                        let text = if view.world.is_none() { "Not in a world yet. Browse servers to join one." } else { "No one else on this map yet." };
                        ui.label(RichText::new(text).weak());
                        ui.add_space(4.0);
                    }
                    for (i, p) in peers.iter().enumerate() {
                        let detail = format!("{:.0} {}", p.tiles, if p.tiles.round() == 1.0 { "tile" } else { "tiles" });
                        if row(ui, ("player row", p.session), self.focus == i, Some(p.version), &p.name, &detail).clicked() {
                            picked = Some(i);
                        }
                    }
                    ui.separator();
                    let unread = if self.unread > 0 { format!("{} new", self.unread) } else { String::new() };
                    let whispers = if self.log_pinned { "Hide whispers" } else { "Show whispers" };
                    if row(ui, "whispers row", self.focus == peers.len(), None, whispers, &unread).clicked() {
                        picked = Some(peers.len());
                    }
                    if row(ui, "browse row", self.focus == peers.len() + 1, None, "Browse servers\u{2026}", "").clicked() {
                        picked = Some(peers.len() + 1);
                    }
                    hint(ui, "Up/Down choose \u{b7} Enter / A open \u{b7} Esc / B close");
                });
            });
        if let Some(i) = picked {
            self.focus = i;
            self.pick_row(i, view);
        }
        if let Some(f) = friend.and_then(|i| friends_sorted(&view.friends).get(i).map(|f| (i, f.steam))) {
            self.fr.focus = f.0;
            self.open_friend_menu(f.1);
        }
        if let Some(on) = tab {
            self.show_friends_tab(on);
        }
    }

    /// the player menu, by their name or beside the list
    fn menu_panel(&mut self, ctx: &egui::Context, view: &View, menus: &mut Menus) {
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("player menu shown"), self.menu_open, self.time(QUICK)));
        if t <= 0.0 {
            if !self.menu_open {
                self.menu = None;
            }
            return;
        }
        let Some(m) = &self.menu else { return };
        let screen = ctx.content_rect();
        // right of their name, or left of it where the right has no room (never over the player it is about)
        let at = m.at.map_or(Pos2::new(304.0, 12.0), |p| if p.x + 260.0 > screen.right() { p - Vec2::new(262.0, -6.0) } else { p + Vec2::new(12.0, 6.0) });
        let at = Pos2::new(at.x.min(screen.right() - 252.0).max(screen.left() + 8.0), at.y.min(screen.bottom() - 190.0).max(8.0));
        let tiles = view.peers.iter().find(|p| p.session == m.who.session).map(|p| p.tiles);
        let (focus, who) = (m.focus, m.who.clone());
        let mut chosen = None;
        egui::Area::new(Id::new("player menu"))
            .fixed_pos(at + Vec2::new(0.0, (1.0 - t) * 8.0))
            .order(egui::Order::Foreground)
            .interactable(self.menu_open)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                card(ui, 220.0, |ui| {
                    ui.horizontal(|ui| {
                        version_pill(ui, who.version);
                        ui.add(egui::Label::new(RichText::new(&who.name).strong().size(16.0)).truncate());
                    });
                    let away = tiles.map_or("on another map".to_owned(), |t| format!("{t:.0} tiles away"));
                    ui.label(RichText::new(format!("Pok\u{e9}mon {} \u{b7} {away}", who.version)).small().weak());
                    ui.add_space(4.0);
                    let ignored = self.ignoring(who.session, who.steam, &who.world);
                    for (i, &action) in ACTIONS.iter().enumerate() {
                        let action = if i == 3 && ignored { "Stop ignoring" } else { action };
                        if row(ui, ("action", i), focus == i, None, action, "").clicked() {
                            chosen = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = chosen {
            self.act(i, menus, view.online, false);
        }
        // a click anywhere else closes it
        if self.menu_open && ctx.input(|i| i.pointer.any_pressed()) && !ctx.is_pointer_over_egui() {
            self.menu_open = false;
        }
    }

    /// the whisper box, bottom left over the log
    fn whisper_panel(&mut self, ctx: &egui::Context, view: &View, menus: &mut Menus) {
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("whisper shown"), self.whisper_open, self.time(QUICK)));
        if t <= 0.0 {
            if !self.whisper_open {
                self.whisper = None;
            }
            return;
        }
        let focus = std::mem::take(&mut self.whisper_focus);
        let pad = self.whisper_pad;
        let mut send = false;
        let mut cancel = false;
        let Some((who, text)) = &mut self.whisper else { return };
        egui::Area::new(Id::new("whisper"))
            .anchor(Align2::LEFT_BOTTOM, Vec2::new(12.0, -12.0 + (1.0 - t) * 16.0))
            .order(egui::Order::Foreground)
            .interactable(self.whisper_open)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                card(ui, 340.0, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Whisper to").weak());
                        version_pill(ui, who.version);
                        ui.add(egui::Label::new(RichText::new(&who.name).strong()).truncate());
                    });
                    if pad {
                        hint(ui, "A: type with Steam's keyboard, A again sends \u{b7} B: cancel. No Steam keyboard? Connect a keyboard to type.");
                    }
                    let field = ui.add(egui::TextEdit::singleline(text).char_limit(WHISPER_CHARS).hint_text("Say something\u{2026}").desired_width(f32::INFINITY));
                    if focus {
                        field.request_focus();
                    }
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        send = true;
                    }
                    ui.horizontal(|ui| {
                        let n = text.chars().count();
                        let color = if n >= WHISPER_CHARS { menus::NOTE } else { ui.visuals().weak_text_color() };
                        ui.label(RichText::new(format!("{n}/{WHISPER_CHARS}")).small().color(color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if menus::button(ui, "Send", true).clicked() {
                                send = true;
                            }
                            if menus::button(ui, "Cancel", false).clicked() {
                                cancel = true;
                            }
                        });
                    });
                });
            });
        if cancel {
            self.whisper_open = false;
        }
        if send && self.whisper_open {
            self.send_whisper(menus, view.online);
            // not sent (nothing typed, too fast, offline): the box keeps the keyboard
            self.whisper_focus = self.whisper_open;
        }
    }

    /// the report card, in the middle
    fn report_panel(&mut self, ctx: &egui::Context, menus: &mut Menus) {
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("report shown"), self.report_open, self.time(QUICK)));
        if t <= 0.0 {
            if !self.report_open {
                self.report = None;
            }
            return;
        }
        let (mut send, mut cancel) = (false, false);
        let Some(r) = &mut self.report else { return };
        // the game dims a little behind it
        ctx.layer_painter(egui::LayerId::background()).rect_filled(ctx.content_rect(), 0.0, Color32::from_black_alpha((110.0 * t) as u8));
        egui::Area::new(Id::new("report"))
            .anchor(Align2::CENTER_CENTER, Vec2::new(0.0, (1.0 - t) * 10.0))
            .order(egui::Order::Foreground)
            .interactable(self.report_open)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                card(ui, 360.0, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Report").strong().size(17.0));
                        version_pill(ui, r.who.version);
                        ui.add(egui::Label::new(RichText::new(&r.who.name).strong().size(17.0)).truncate());
                    });
                    ui.label(RichText::new("The server's moderators see the reason, your note and the last 30 seconds of where you both were.").small().weak());
                    ui.add_space(4.0);
                    for (i, (_, name)) in REASONS.iter().enumerate() {
                        if radio(ui, ("reason", i), r.reason == i, r.focus == i, name).clicked() {
                            r.reason = i;
                            r.focus = i;
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("Note (optional)").small());
                    ui.add(egui::TextEdit::multiline(&mut r.note).char_limit(NOTE_CHARS).desired_rows(2).desired_width(f32::INFINITY).hint_text("What happened?"));
                    let n = r.note.chars().count();
                    ui.label(RichText::new(format!("{n}/{NOTE_CHARS}")).small().weak());
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let s = menus::button(ui, "Send report", true);
                            if r.focus == 4 {
                                focus_ring(ui, s.rect);
                            }
                            if s.clicked() {
                                send = true;
                            }
                            let c = menus::button(ui, "Cancel", false);
                            if r.focus == 5 {
                                focus_ring(ui, c.rect);
                            }
                            if c.clicked() {
                                cancel = true;
                            }
                        });
                    });
                });
            });
        if cancel {
            self.report_open = false;
        } else if send && self.report_open {
            self.send_report(menus);
        }
    }

    /// Multiplayer > Browse servers: Steam's list of worlds, joined by a double-click, and a direct connect
    fn browser_panel(&mut self, ctx: &egui::Context, view: &View, menus: &mut Menus) {
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("browser shown"), self.browser, self.time(QUICK)));
        if t <= 0.0 {
            return;
        }
        let list = self.listed();
        self.picked = self.picked.min(list.len().saturating_sub(1));
        let screen = ctx.content_rect();
        let width = (screen.width() - 24.0).clamp(300.0, 560.0);
        let layer = egui::LayerId::new(egui::Order::Middle, Id::new("browser"));
        let scale = 0.95 + 0.05 * t;
        ctx.set_transform_layer(layer, egui::emath::TSTransform::new(screen.center().to_vec2() * (1.0 - scale), scale));
        ctx.layer_painter(egui::LayerId::background()).rect_filled(screen, 0.0, Color32::from_black_alpha((110.0 * t) as u8));
        let (mut join, mut close, mut search, mut connect) = (None, false, false, false);
        egui::Area::new(Id::new("browser"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Middle)
            .interactable(self.browser)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                card(ui, width, |ui| {
                    ui.horizontal(|ui| {
                        title(ui, "Browse servers", None);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if menus::button(ui, "Refresh", false).on_hover_text("Ask Steam's server list again").clicked() {
                                search = true;
                            }
                            if view.searching {
                                ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                            }
                        });
                    });
                    let status = if view.searching {
                        "Searching Steam's server list\u{2026}".to_owned()
                    } else {
                        match list.len() {
                            0 => "No worlds listed yet".to_owned(),
                            1 => "1 world".to_owned(),
                            n => format!("{n} worlds"),
                        }
                    };
                    ui.label(RichText::new(status).small().weak());
                    ui.add_space(4.0);
                    // the header
                    let colors = menus::palette(ui.visuals().dark_mode);
                    // the header, over the rows' columns (server_row)
                    let (head, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
                    let font = FontId::proportional(12.0);
                    ui.painter().text(Pos2::new(head.left() + 12.0, head.center().y), Align2::LEFT_CENTER, "Name", font.clone(), colors.weak);
                    ui.painter().text(Pos2::new(head.right() - 108.0, head.center().y), Align2::CENTER_CENTER, "Players", font.clone(), colors.weak);
                    ui.painter().text(Pos2::new(head.right() - 38.0, head.center().y), Align2::CENTER_CENTER, "Ping", font, colors.weak);
                    egui::ScrollArea::vertical().max_height((screen.height() - 300.0).clamp(80.0, 260.0)).auto_shrink([false, true]).show(ui, |ui| {
                        if list.is_empty() {
                            ui.add_space(8.0);
                            ui.label(RichText::new(if view.searching { "Looking\u{2026}" } else { "No worlds found yet." }).strong());
                            ui.label(RichText::new("A new server can take up to 2 minutes to appear in Steam's list, and Steam does not always return it. The list asks again every 20 seconds; Refresh to ask now, or use Direct connect with the address the server printed.").small().weak());
                            ui.add_space(8.0);
                        }
                        for (i, s) in list.iter().enumerate() {
                            let r = server_row(ui, i == self.picked, s, view.world.as_deref() == Some(s.address.as_str()));
                            if r.clicked() {
                                self.picked = i;
                            }
                            if r.double_clicked() {
                                join = Some(s.address.clone());
                            }
                        }
                    });
                    ui.add_space(4.0);
                    ui.separator();
                    ui.label(RichText::new("Direct connect").small());
                    ui.horizontal(|ui| {
                        let field = ui.add(egui::TextEdit::singleline(&mut self.direct).hint_text("steam:<server id> or host:port").desired_width(ui.available_width() - 100.0));
                        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            connect = true;
                        }
                        if menus::button(ui, "Connect", false).clicked() {
                            connect = true;
                        }
                    });
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Double-click a world to join \u{b7} A / Enter joins \u{b7} B / Esc closes").small().weak());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if menus::button(ui, "Close", false).clicked() {
                                close = true;
                            }
                            let can = list.get(self.picked).is_some();
                            if ui.add_enabled_ui(can, |ui| menus::button(ui, "Join", true)).inner.clicked() {
                                join = list.get(self.picked).map(|s| s.address.clone());
                            }
                        });
                    });
                });
            });
        if search {
            self.searched = Some(Instant::now());
            self.requests.push(Request::Search);
            menus.toast("Servers: asking Steam again");
        }
        if connect {
            let address = self.direct.trim().to_owned();
            if valid_address(&address) {
                join = Some(address);
            } else {
                menus.toast("Direct connect: steam:<server id> or host:port");
            }
        }
        if let Some(address) = join {
            self.join(address, menus);
        }
        if close {
            self.browser = false;
        }
    }

    /// the whispers, bottom left, for a while after one or while pinned; a line from someone answers them
    fn log_panel(&mut self, ctx: &egui::Context) {
        let recent = self.log_until.is_some_and(|t| Instant::now() < t);
        let up = (self.log_pinned || recent) && !self.log.is_empty() && !self.whisper_open;
        let t = ctx.animate_bool_with_time(Id::new("log shown"), up, self.time(0.3));
        if recent {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if t <= 0.0 {
            return;
        }
        let mut reply = None;
        egui::Area::new(Id::new("whisper log"))
            .anchor(Align2::LEFT_BOTTOM, Vec2::new(12.0, -12.0))
            .order(egui::Order::Middle)
            .interactable(up)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                egui::Frame::new().fill(Color32::from_rgba_unmultiplied(0x17, 0x1A, 0x1E, 200)).corner_radius(10).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(320.0);
                    ui.label(RichText::new("Whispers").small().strong().color(Color32::WHITE));
                    for line in self.log.iter().rev().take(8).collect::<Vec<_>>().into_iter().rev() {
                        let (who, color) = if line.mine { (format!("To {}", line.name), Color32::from_gray(170)) } else { (line.name.clone(), ACCENT_LIGHT) };
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&format!("{who}: "), 0.0, egui::TextFormat::simple(FontId::proportional(13.0), color));
                        job.append(&line.text, 0.0, egui::TextFormat::simple(FontId::proportional(13.0), Color32::WHITE));
                        let r = ui.add(egui::Label::new(job).wrap().sense(Sense::click()));
                        if r.clicked() {
                            reply = Some((line.session, line.name.clone(), line.world.clone()));
                        }
                        if r.hovered() {
                            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                    }
                    ui.label(RichText::new("Click a line to answer").small().color(Color32::from_gray(140)));
                });
            });
        if let Some((session, name, world)) = reply {
            let who = Target { session, name, steam: 0, version: "", world };
            // a line from another world names a session that is someone else here
            if self.here(&who) {
                self.open_whisper(who, false);
            }
            self.unread = 0;
        }
    }
}

/// a lighter red that reads on the dark log
const ACCENT_LIGHT: Color32 = Color32::from_rgb(0xFF, 0x7A, 0x82);

/// the peers, the nearest first
fn sorted(peers: &[Peer]) -> Vec<Peer> {
    let mut list = peers.to_vec();
    list.sort_by(|a, b| a.tiles.total_cmp(&b.tiles).then(a.name.cmp(&b.name)));
    list
}

/// Pokemon X's blue, Y's red
fn version_color(version: &str) -> Color32 {
    match version {
        "X" => Color32::from_rgb(0x3D, 0x7E, 0xEA),
        "Y" => Color32::from_rgb(0xE5, 0x3D, 0x4E),
        _ => Color32::from_gray(150),
    }
}

/// a text cut to n characters, with an ellipsis when cut
fn short(text: &str, n: usize) -> String {
    if text.chars().count() <= n { text.to_owned() } else { format!("{}\u{2026}", text.chars().take(n - 1).collect::<String>()) }
}

/// steam:<digits> or host:port
pub fn valid_address(a: &str) -> bool {
    match a.strip_prefix("steam:") {
        Some(id) => !id.is_empty() && id.parse::<u64>().is_ok(),
        None => a.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)),
    }
}

/// a panel: the window's colours, rounded, a red line along its top like the gear's
fn card(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let frame = egui::Frame::window(ui.style()).inner_margin(egui::Margin::symmetric(14, 12));
    let r = frame.show(ui, |ui| {
        ui.set_width(width);
        add(ui);
    });
    let rect = r.response.rect;
    let line = Rect::from_min_max(Pos2::new(rect.left() + 14.0, rect.top()), Pos2::new(rect.right() - 14.0, rect.top() + 3.0));
    ui.painter().rect_filled(line, 2.0, ACCENT);
}

fn title(ui: &mut egui::Ui, text: &str, count: Option<usize>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(text).strong().size(17.0));
        if let Some(n) = count {
            let color = if n == 0 { ui.visuals().weak_text_color() } else { Color32::WHITE };
            let galley = ui.painter().layout_no_wrap(n.to_string(), FontId::proportional(12.0), color);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x + 14.0, 18.0), Sense::hover());
            ui.painter().rect_filled(rect, 9.0, if n == 0 { menus::palette(ui.visuals().dark_mode).line } else { ACCENT });
            ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
        }
    });
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).small().weak());
}

/// "X" or "Y" in its version's colour
fn version_pill(ui: &mut egui::Ui, version: &str) {
    if version.is_empty() {
        return;
    }
    let galley = ui.painter().layout_no_wrap(version.to_owned(), FontId::proportional(11.0), Color32::WHITE);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x + 10.0, 16.0), Sense::hover());
    ui.painter().rect_filled(rect, 5.0, version_color(version));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// a list row: the keyboard's (or controller's) row lit w/ the accent bar, hover easing in, a press dipping
fn row(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, focused: bool, version: Option<&str>, text: &str, detail: &str) -> egui::Response {
    let colors = menus::palette(ui.visuals().dark_mode);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    let id = Id::new(id);
    let motion = ui.style().animation_time > 0.0;
    let time = |s: f32| if motion { s } else { 0.0 };
    let hover = ui.ctx().animate_bool_with_time(id.with("hover"), response.hovered(), time(0.12));
    let lit = ui.ctx().animate_bool_with_time(id.with("focus"), focused, time(0.14));
    let press = ui.ctx().animate_bool_with_time(id.with("press"), response.is_pointer_button_down_on(), time(0.06));
    let fill = colors.control.gamma_multiply(hover.max(lit));
    let rect = rect.shrink(press);
    if hover.max(lit) > 0.0 {
        ui.painter().rect_filled(rect, 7.0, fill);
    }
    if lit > 0.0 {
        let bar = Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 7.0), Vec2::new(3.0 * lit, rect.height() - 14.0));
        ui.painter().rect_filled(bar, 2.0, ACCENT);
    }
    let mut x = rect.left() + 12.0;
    if let Some(v) = version {
        let galley = ui.painter().layout_no_wrap(v.to_owned(), FontId::proportional(11.0), Color32::WHITE);
        let pill = Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), Vec2::new(galley.size().x + 10.0, 16.0));
        ui.painter().rect_filled(pill, 5.0, version_color(v));
        ui.painter().galley(pill.center() - galley.size() / 2.0, galley, Color32::WHITE);
        x = pill.right() + 8.0;
    }
    // a long name stops short of the detail (16 wide characters fit no row)
    let room = if detail.is_empty() { 10.0 } else { 70.0 };
    let clip = Rect::from_min_max(rect.min, Pos2::new(rect.right() - room, rect.bottom()));
    ui.painter().with_clip_rect(clip).text(Pos2::new(x + 2.0 * lit, rect.center().y), Align2::LEFT_CENTER, text, FontId::proportional(14.5), colors.text);
    if !detail.is_empty() {
        ui.painter().text(Pos2::new(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, detail, FontId::proportional(12.5), colors.weak);
    }
    response
}

/// a reason to pick, w/ its round mark
fn radio(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, on: bool, focused: bool, text: &str) -> egui::Response {
    let colors = menus::palette(ui.visuals().dark_mode);
    let response = row(ui, id, focused, None, "", "");
    let id = response.id;
    let motion = ui.style().animation_time > 0.0;
    let t = ui.ctx().animate_bool_with_time(id.with("on"), on, if motion { 0.14 } else { 0.0 });
    let center = Pos2::new(response.rect.left() + 22.0, response.rect.center().y);
    ui.painter().circle_stroke(center, 7.0, Stroke::new(1.5, colors.line.lerp_to_gamma(ACCENT, t)));
    if t > 0.0 {
        ui.painter().circle_filled(center, 4.0 * t, ACCENT);
    }
    ui.painter().text(Pos2::new(center.x + 16.0, center.y), Align2::LEFT_CENTER, text, FontId::proportional(14.0), colors.text);
    response
}

fn focus_ring(ui: &mut egui::Ui, rect: Rect) {
    ui.painter().rect_stroke(rect.expand(2.5), 9.0, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
}

/// a server's row: its name (or "Unnamed world" and the end of its id), players, ping
fn server_row(ui: &mut egui::Ui, picked: bool, s: &Server, here: bool) -> egui::Response {
    let colors = menus::palette(ui.visuals().dark_mode);
    let response = row(ui, ("server", &s.address), picked, None, "", "");
    let rect = response.rect;
    let (name, color) = if s.name.is_empty() {
        let id = s.address.trim_start_matches("steam:");
        (format!("Unnamed world \u{2026}{}", &id[id.len().saturating_sub(4)..]), colors.weak)
    } else {
        (short(&s.name, 40), colors.text)
    };
    let name = if here { format!("{name}  (joined)") } else { name };
    ui.painter().with_clip_rect(Rect::from_min_max(rect.min, Pos2::new(rect.right() - 140.0, rect.bottom()))).text(
        Pos2::new(rect.left() + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        name,
        FontId::proportional(14.0),
        color,
    );
    let players = match (s.players, s.max) {
        (Some(p), Some(m)) if m > 0 => format!("{p}/{m}"),
        (Some(p), _) => p.to_string(),
        (None, _) => "\u{2014}".to_owned(),
    };
    ui.painter().text(Pos2::new(rect.right() - 108.0, rect.center().y), Align2::CENTER_CENTER, players, FontId::proportional(13.0), colors.text);
    let ping = s.ping.map_or("\u{2014}".to_owned(), |p| format!("{p} ms"));
    ui.painter().text(Pos2::new(rect.right() - 38.0, rect.center().y), Align2::CENTER_CENTER, ping, FontId::proportional(13.0), colors.weak);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(session: u32, name: &str, tiles: f32) -> Peer {
        Peer { session, name: name.into(), steam: 76_561_197_960_287_930 + session as u64, version: if session % 2 == 0 { "Y" } else { "X" }, tiles, label: Some([200.0, 90.0]) }
    }

    /// A point on the top screen lands where the presenter shows it, whatever the window's size and the rotation; a
    /// name fades out toward the screen's edges and is gone past them; its size follows the screen's scale.
    #[test]
    fn names_go_where_the_top_screen_shows_them() {
        // the default 2x window: the top screen at 0,0, 800x480 points
        let top = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 480.0));
        assert_eq!(place([200.0, 87.0], top, Orientation::North), Pos2::new(400.0, 174.0));
        // a 3x window w/ the screens centered sideways (a wider window): offset and scaled
        let wide = Rect::from_min_size(Pos2::new(100.0, 0.0), Vec2::new(1200.0, 720.0));
        assert_eq!(place([0.0, 0.0], wide, Orientation::North), Pos2::new(100.0, 0.0));
        assert_eq!(place([400.0, 240.0], wide, Orientation::North), Pos2::new(1300.0, 720.0));
        // turned a quarter clockwise the top screen stands 240 wide, 400 tall: its top left corner is at the right
        let turned = Rect::from_min_size(Pos2::ZERO, Vec2::new(480.0, 800.0));
        assert_eq!(place([0.0, 0.0], turned, Orientation::East), Pos2::new(480.0, 0.0));
        assert_eq!(place([400.0, 0.0], turned, Orientation::East), Pos2::new(480.0, 800.0));
        assert_eq!(place([0.0, 0.0], turned, Orientation::West), Pos2::new(0.0, 800.0));
        assert_eq!(place([0.0, 0.0], top, Orientation::South), Pos2::new(800.0, 480.0));
        // the touch mapping in lib.rs undoes it: window back to screen
        for o in Orientation::ALL {
            let p = place([100.0, 60.0], turned, o);
            let (u, v) = ((p.x - turned.left()) / turned.width(), (p.y - turned.top()) / turned.height());
            let (u, v) = match o {
                Orientation::North => (u, v),
                Orientation::East => (v, 1.0 - u),
                Orientation::South => (1.0 - u, 1.0 - v),
                Orientation::West => (1.0 - v, u),
            };
            assert!((u * 400.0 - 100.0).abs() < 1e-3 && (v * 240.0 - 60.0).abs() < 1e-3, "{o:?}");
        }
        assert_eq!(edge_alpha([200.0, 120.0]), 1.0);
        assert_eq!(edge_alpha([200.0, 12.0]), 0.5, "half faded near the top edge");
        assert_eq!(edge_alpha([-5.0, 120.0]), 0.0);
        assert_eq!(edge_alpha([200.0, 250.0]), 0.0);
        // readable at every scale: 1x 11 points, 2x 14, fullscreen 4K capped
        assert_eq!(label_size(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 240.0))), 11.0);
        assert_eq!(label_size(top), 14.0);
        assert_eq!(label_size(Rect::from_min_size(Pos2::ZERO, Vec2::new(3600.0, 2160.0))), 26.0);
    }

    /// A whisper is trimmed and cut to 120 characters, an empty one is not sent, and no more than 5 go in 10 s.
    #[test]
    fn the_whisper_box_keeps_the_servers_limits() {
        assert_eq!(whisper_text("  hi  "), Some("hi".into()));
        assert_eq!(whisper_text("   "), None);
        assert_eq!(whisper_text(&"é".repeat(300)).unwrap().chars().count(), WHISPER_CHARS);
        let mut l = Limiter::default();
        let t0 = Instant::now();
        assert!((0..5).all(|i| l.allow(t0 + Duration::from_secs(i), WHISPER_MAX, WHISPER_WINDOW)));
        assert!(!l.allow(t0 + Duration::from_secs(9), WHISPER_MAX, WHISPER_WINDOW), "a sixth inside 10 s");
        assert!(l.allow(t0 + Duration::from_secs(10), WHISPER_MAX, WHISPER_WINDOW), "the first one's 10 s are up");
        assert!(valid_address("steam:90071992547409920") && valid_address("127.0.0.1:7777") && valid_address("example.org:7777"));
        assert!(!valid_address("steam:") && !valid_address("steam:abc") && !valid_address("127.0.0.1") && !valid_address(":7777") && !valid_address("host:0"));
    }

    /// runs a frame of the panels with these steps from a controller
    fn frame(ctx: &egui::Context, online: &mut Online, menus: &mut Menus, view: &View, steps: &[Nav]) {
        for &s in steps {
            online.nav(s);
        }
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 960.0))), ..Default::default() };
        let top = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 480.0));
        ctx.run_ui(input, |ui| online.ui(ui.ctx(), view, Some(top), Orientation::North, true, true, menus)).drop_without_applying_deltas();
    }

    /// With a controller alone: the Players button opens the list (the nearest first), down and A open the second
    /// player's menu, down twice and A open Report, A picks a reason, A sends it; then Whisper and Add friend from the
    /// list, and B backs out of each panel and closes the list.
    #[test]
    fn a_controller_alone_goes_through_the_players_list() {
        let ctx = egui::Context::default();
        menus::style(&ctx, true, false);
        let mut online = Online { motion: false, ..Online::default() };
        let mut menus = Menus::default();
        menus.motion = false;
        let view = View { world: Some("steam:1".into()), online: true, peers: vec![peer(4, "Serena", 6.0), peer(3, "Calem", 2.0)], ..View::default() };
        online.toggle_players();
        assert!(online.players && online.takes_input());
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Down, Nav::Accept]);
        assert_eq!(online.menu.as_ref().map(|m| m.who.name.as_str()), Some("Serena"), "the second nearest");
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Down, Nav::Down, Nav::Accept]);
        assert!(online.report_open && !online.menu_open);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Down, Nav::Accept]);
        assert_eq!(online.report.as_ref().map(|r| (r.reason, r.focus)), Some((1, 4)), "Harassment picked, Send focused");
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept]);
        let report = Request::Report { session: 4, steam_id: 76_561_197_960_287_934, name: "Serena".into(), reason: Reason::Harassment, note: String::new() };
        assert_eq!(online.requests.pop(), Some(report), "the target's SteamID goes with the session");
        assert!(!online.report_open && online.players, "back on the list");
        // the nearest: Whisper opens the box, which B leaves
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Up, Nav::Accept]);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept]);
        assert!(online.whisper_open && online.whisper.as_ref().is_some_and(|w| w.0.name == "Calem"));
        // opened with a controller: Steam's gamepad keyboard is asked for; what it gives back fills the box, A sends it
        assert_eq!(online.requests.last(), Some(&Request::Type(String::new())));
        frame(&ctx, &mut online, &mut menus, &View { typed: Some("hi from a pad".into()), ..view.clone() }, &[]);
        assert_eq!(online.whisper.as_ref().map(|w| w.1.as_str()), Some("hi from a pad"));
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept]);
        assert_eq!(online.requests.last(), Some(&Request::Whisper { to: 3, name: "Calem".into(), text: "hi from a pad".into() }));
        assert!(!online.whisper_open);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept, Nav::Accept]);
        assert!(online.whisper_open);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Back]);
        assert!(!online.whisper_open);
        // Add friend asks Steam's overlay for their SteamID
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept, Nav::Down, Nav::Accept]);
        assert_eq!(online.requests.pop(), Some(Request::AddFriend(76_561_197_960_287_933)));
        // B on the menu, then B on the list: all closed, the game gets its input back
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept, Nav::Back]);
        assert!(!online.menu_open && online.players);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Back]);
        assert!(!online.takes_input());
        // the last rows: Browse servers opens the browser, which asks Steam at once; B closes it
        online.toggle_players();
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Down, Nav::Down, Nav::Down, Nav::Accept]);
        assert!(online.browser && !online.players);
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        assert_eq!(online.requests.pop(), Some(Request::Search));
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Back]);
        assert!(!online.takes_input());
    }

    /// Session ids start over in every server: when the world changes, the menu, whisper box and report card about a
    /// player of the old one close, its names go, and a whisper-log line from it cannot be answered.
    #[test]
    fn panels_about_another_worlds_player_close() {
        let ctx = egui::Context::default();
        let mut online = Online { motion: false, ..Online::default() };
        let mut menus = Menus::default();
        menus.motion = false;
        let a = View { world: Some("steam:1".into()), online: true, peers: vec![peer(4, "Serena", 2.0)], whispers: vec![Whisper { from: 4, steam: 0, name: "Serena".into(), text: "hi".into() }], ..View::default() };
        frame(&ctx, &mut online, &mut menus, &a, &[]);
        online.toggle_players();
        frame(&ctx, &mut online, &mut menus, &View { whispers: vec![], ..a.clone() }, &[Nav::Accept, Nav::Accept]);
        assert!(online.whisper_open);
        assert!(!online.labels.is_empty(), "Serena's name shows");
        // joined another world, where session 4 is somebody else
        let b = View { world: Some("steam:2".into()), online: true, peers: vec![], ..View::default() };
        frame(&ctx, &mut online, &mut menus, &b, &[]);
        assert!(!online.whisper_open && online.whisper.is_none() && online.menu.is_none() && online.labels.is_empty());
        // the old line is still in the log, but answering it opens nothing
        let old = online.log.front().map(|l| (l.session, l.name.clone(), l.world.clone())).unwrap();
        assert_eq!(old.2.as_deref(), Some("steam:1"));
        online.open_menu(Target { session: 4, name: "Serena".into(), steam: 0, version: "Y", world: old.2 }, None);
        online.act(0, &mut menus, true, false);
        online.whisper.as_mut().unwrap().1 = "hello?".into();
        online.send_whisper(&mut menus, true);
        assert!(!online.requests.iter().any(|r| matches!(r, Request::Whisper { .. })), "nothing goes to whoever has session 4 here");
    }

    /// Ignore (the player menu's last entry) drops that player's whispers here for the rest of the run, by their Steam
    /// account (from any session and any map; another player who later has their old session is not caught), or by
    /// world and session for a test id; Stop ignoring lets them through again.
    #[test]
    fn ignore_drops_a_players_whispers() {
        let ctx = egui::Context::default();
        let mut online = Online { motion: false, ..Online::default() };
        let mut menus = Menus::default();
        menus.motion = false;
        let serena = 76_561_197_960_287_934; // peer(4)'s account
        let says = |from: u32, steam: u64| vec![Whisper { from, steam, name: "Serena".into(), text: "hi".into() }];
        let view = View { world: Some("steam:1".into()), online: true, peers: vec![peer(4, "Serena", 2.0), peer(5, "Calem", 3.0)], ..View::default() };
        frame(&ctx, &mut online, &mut menus, &View { whispers: says(4, serena), ..view.clone() }, &[]);
        assert_eq!(online.log.len(), 1);
        online.toggle_players();
        // the nearest's menu, down to Ignore (Whisper, Add friend, Report, Ignore)
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept, Nav::Down, Nav::Down, Nav::Down, Nav::Accept]);
        assert!(!online.menu_open);
        assert!(online.in_world());
        frame(&ctx, &mut online, &mut menus, &View { whispers: says(4, serena), ..view.clone() }, &[]);
        assert_eq!(online.log.len(), 1, "an ignored player's whisper is dropped");
        // back with a new session, off our map: the same Steam account
        frame(&ctx, &mut online, &mut menus, &View { whispers: says(9, serena), peers: vec![], ..view.clone() }, &[]);
        assert_eq!(online.log.len(), 1);
        // someone else who has session 4 now (the server restarted): through
        frame(&ctx, &mut online, &mut menus, &View { whispers: says(4, 76_561_197_960_287_999), peers: vec![], ..view.clone() }, &[]);
        assert_eq!(online.log.len(), 2, "another account on the ignored session");
        // Stop ignoring
        online.open_menu(Target::of(&view.peers[0], &view.world), None);
        online.act(3, &mut menus, true, false);
        frame(&ctx, &mut online, &mut menus, &View { whispers: says(4, serena), ..view.clone() }, &[]);
        assert_eq!(online.log.len(), 3);
        // a test id (no Steam account): by world and session
        let mut test = peer(6, "Shauna", 1.0);
        test.steam = 0;
        online.open_menu(Target::of(&test, &view.world), None);
        online.act(3, &mut menus, true, false);
        frame(&ctx, &mut online, &mut menus, &View { whispers: says(6, 0), peers: vec![test], ..view.clone() }, &[]);
        assert_eq!(online.log.len(), 3);
        assert!(!Online::default().in_world());
    }

    /// The browser lists what Steam returned, keeps a server a later answer missed, joins the picked one with A, and
    /// says how long a new server takes when there is none.
    #[test]
    fn the_browser_keeps_servers_and_joins_by_steam_id() {
        let ctx = egui::Context::default();
        let mut online = Online { motion: false, ..Online::default() };
        let mut menus = Menus::default();
        menus.motion = false;
        online.open_browser();
        let server = |id: u64, players| Server { address: format!("steam:{id}"), name: String::new(), players: Some(players), max: Some(256), ping: None };
        let first = View { servers: vec![server(7, 1), server(9, 3)], ..View::default() };
        frame(&ctx, &mut online, &mut menus, &first, &[]);
        // the next answer misses one: it stays listed
        frame(&ctx, &mut online, &mut menus, &View { servers: vec![server(7, 2)], ..View::default() }, &[]);
        assert_eq!(online.listed().iter().map(|s| s.address.as_str()).collect::<Vec<_>>(), ["steam:9", "steam:7"], "the fullest first");
        frame(&ctx, &mut online, &mut menus, &View::default(), &[Nav::Down, Nav::Accept]);
        assert_eq!(online.requests.last(), Some(&Request::Join("steam:7".into())));
        assert!(!online.browser);
    }

    // ---- friends ------------------------------------------------------------------------------------------------

    const SERENA: u64 = 76_561_197_960_287_934;
    const CALEM: u64 = 76_561_197_960_287_935;
    const TIERNO: u64 = 76_561_197_960_287_936;

    fn friend(steam: u64, name: &str, in_redex: bool) -> Friend {
        Friend { steam, name: name.into(), in_redex, status: in_redex.then(|| "In Kalos".to_string()) }
    }

    fn friends_view() -> View {
        View { world: Some("steam:5".into()), online: true, friends: vec![friend(TIERNO, "Tierno", false), friend(SERENA, "Serena", true), friend(CALEM, "calem", true)], ..View::default() }
    }

    fn quiet() -> (egui::Context, Online, Menus) {
        let ctx = egui::Context::default();
        menus::style(&ctx, true, false);
        let mut menus = Menus::default();
        menus.motion = false;
        (ctx, Online { motion: false, ..Online::default() }, menus)
    }

    fn says(from: u64, text: &str) -> (u64, FriendMsg) {
        (from, FriendMsg::Chat(text.into()))
    }

    fn invite(from: u64, world: &str, connect: &str) -> (u64, FriendMsg) {
        (from, FriendMsg::Invite { world: world.into(), connect: connect.into() })
    }

    /// Friends running RE:DEX first, then the others (greyed), each by name whatever its case.
    #[test]
    fn friends_sorted_in_redex_first() {
        let names: Vec<String> = friends_sorted(&friends_view().friends).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["calem", "Serena", "Tierno"]);
    }

    /// A tab per friend who wrote, each with its unread count (cleaned like a whisper); showing a tab reads it.
    #[test]
    fn chat_tabs_and_unread() {
        let (ctx, mut online, mut menus) = quiet();
        let view = friends_view();
        let msgs = vec![says(SERENA, " hi\u{202E} "), says(CALEM, "yo"), says(SERENA, "there"), says(SERENA, "   ")];
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: msgs, ..view.clone() }, &[]);
        assert_eq!(online.chat_tabs(), [("Serena".to_string(), 2), ("calem".to_string(), 1)]);
        assert_eq!(online.friends_unread(), 3);
        assert_eq!((online.fr.chats[0].lines[0].mine, online.fr.chats[0].lines[0].text.as_str()), (false, "hi"), "cleaned like a whisper");
        online.open_chat(SERENA);
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        assert_eq!(online.chat_tabs(), [("Serena".to_string(), 0), ("calem".to_string(), 1)]);
        // a new line on the tab being read is read at once; on another tab it counts
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: vec![says(SERENA, "a"), says(CALEM, "b")], ..view.clone() }, &[]);
        assert_eq!(online.chat_tabs(), [("Serena".to_string(), 0), ("calem".to_string(), 2)]);
    }

    /// A friend who stops running RE:DEX while their chat is open: the tab stays, sending is off with a
    /// reason, and comes back with them.
    #[test]
    fn chat_with_friend_who_left() {
        let (ctx, mut online, mut menus) = quiet();
        let view = friends_view();
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        online.open_chat(SERENA);
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        let gone = View { friends: vec![friend(SERENA, "Serena", false)], ..view.clone() };
        frame(&ctx, &mut online, &mut menus, &gone, &[]);
        assert!(online.fr.chat_open && online.chat_tabs().len() == 1);
        assert_eq!(online.chat_blocked().as_deref(), Some("Serena isn't in RE:DEX"));
        online.fr.chat_text = "hello?".into();
        online.send_chat(&mut menus);
        assert!(!online.requests.iter().any(|r| matches!(r, Request::FriendChat { .. })));
        // off Steam altogether: the same
        frame(&ctx, &mut online, &mut menus, &View { friends: vec![], ..view.clone() }, &[]);
        assert!(online.chat_blocked().is_some());
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        assert_eq!(online.chat_blocked(), None);
        online.send_chat(&mut menus);
        assert_eq!(online.requests.last(), Some(&Request::FriendChat { to: SERENA, text: "hello?".into() }));
        assert_eq!(online.fr.chats[0].lines.last().map(|l| (l.mine, l.text.as_str())), Some((true, "hello?")));
        // the send limit of whispers: 5 in 10 s
        for i in 0..6 {
            online.fr.chat_text = format!("{i}");
            online.send_chat(&mut menus);
        }
        assert_eq!(online.requests.iter().filter(|r| matches!(r, Request::FriendChat { .. })).count(), WHISPER_MAX);
    }

    /// A second invite while a prompt is open replaces it; there is never more than one prompt.
    #[test]
    fn second_invite_replaces_prompt() {
        let (ctx, mut online, mut menus) = quiet();
        let view = friends_view();
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: vec![invite(SERENA, "A", "steam:1"), invite(SERENA, "B", "steam:2")], ..view.clone() }, &[]);
        let shown = |o: &Online| o.fr.invite.as_ref().map(|i| (i.from, i.world.clone(), i.connect.clone()));
        assert_eq!(shown(&online), Some((SERENA, "B".into(), "steam:2".into())));
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: vec![invite(CALEM, "C", "steam:3")], ..view.clone() }, &[]);
        assert_eq!(shown(&online), Some((CALEM, "C".into(), "steam:3".into())));
        assert!(online.takes_input());
    }

    /// Ignore on a friend drops their chat and invites; they stay listed, marked "ignored".
    #[test]
    fn ignored_friend_chat_and_invite_dropped_and_marked() {
        let (ctx, mut online, mut menus) = quiet();
        let view = friends_view();
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        online.open_friend_menu(SERENA);
        online.friend_act(2, &view, &mut menus);
        assert_eq!(online.requests.last(), Some(&Request::Ignored(vec![SERENA])));
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: vec![says(SERENA, "hi"), invite(SERENA, "w", "steam:1")], ..view.clone() }, &[]);
        assert!(online.chat_tabs().is_empty() && online.fr.invite.is_none());
        assert_eq!(online.friend_detail(&view.friends[1]), "ignored");
        assert!(friends_sorted(&view.friends).iter().any(|f| f.steam == SERENA), "still listed");
        // their whispers too (the same Ignore)
        frame(&ctx, &mut online, &mut menus, &View { whispers: vec![Whisper { from: 9, steam: SERENA, name: "Serena".into(), text: "psst".into() }], ..view.clone() }, &[]);
        assert!(online.log.is_empty());
    }

    /// The Ignore list is kept per Steam account (`<data>/user/ignored-<steamid>.txt`, one SteamID64 a line) and read
    /// back at the next start.
    #[test]
    fn ignore_survives_reload() {
        let (ctx, mut online, mut menus) = quiet();
        let dir = std::env::temp_dir().join(format!("redex_ignore_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("user/ignored-76561197960287930.txt");
        let view = View { ignore_file: Some(file.clone()), ..friends_view() };
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        online.open_friend_menu(SERENA);
        online.friend_act(2, &view, &mut menus);
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), SERENA.to_string());
        let (_, mut again, _) = quiet();
        frame(&ctx, &mut again, &mut menus, &view, &[]);
        assert!(again.ignoring(0, SERENA, &None));
        assert_eq!(again.requests.first(), Some(&Request::Ignored(vec![SERENA])), "the link hears the list at start");
        again.open_friend_menu(SERENA);
        again.friend_act(2, &view, &mut menus);
        let (_, mut third, _) = quiet();
        frame(&ctx, &mut third, &mut menus, &view, &[]);
        assert!(!third.ignoring(0, SERENA, &None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The panels as the window draws them, over a field screenshot, for the screenshot critique:
    /// `XR_UI_SHOTS=DIR [XR_UI_BG=<logs dir with top-cp-outside.png and bottom-cp-outside.png>] cargo test -p
    /// xr-frontend --lib friends_screenshots -- --ignored`. shortcut: the software rasterizer is examples/glass_demo.rs's,
    /// copied (an example can't reach these panels); share it if a third copy is ever needed.
    #[test]
    #[ignore]
    fn friends_screenshots() {
        use egui::epaint::Primitive;
        let Some(dir) = std::env::var_os("XR_UI_SHOTS").map(std::path::PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h) = (800usize, 960usize);
        let read = |p: std::path::PathBuf| -> Option<(usize, Vec<u8>)> {
            let mut r = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(p).ok()?)).read_info().ok()?;
            let mut px = vec![0; r.output_buffer_size()?];
            let info = r.next_frame(&mut px).ok()?;
            Some((info.width as usize, px))
        };
        let bg = std::env::var_os("XR_UI_BG").map(std::path::PathBuf::from);
        let mut back = vec![[0.09f32, 0.10, 0.12, 1.0]; w * h];
        if let Some(bg) = bg {
            for (name, ox, oy) in [("top-cp-outside.png", 0, 0), ("bottom-cp-outside.png", 80, 480)] {
                let Some((sw, px)) = read(bg.join(name)) else { continue };
                let sh = px.len() / 4 / sw;
                for y in 0..sh * 2 {
                    for x in 0..sw * 2 {
                        let s = ((y / 2) * sw + x / 2) * 4;
                        back[(oy + y) * w + ox + x] = [px[s] as f32 / 255.0, px[s + 1] as f32 / 255.0, px[s + 2] as f32 / 255.0, 1.0];
                    }
                }
            }
        }
        let shot = |name: &str, online: &mut Online, views: &[View]| {
            let ctx = egui::Context::default();
            menus::style(&ctx, true, false);
            let mut menus = Menus::default();
            menus.motion = false;
            let mut textures: HashMap<egui::TextureId, (usize, usize, Vec<Color32>)> = HashMap::new();
            let mut last = None;
            for view in views.iter().chain(std::iter::repeat_n(views.last().unwrap(), 3)) {
                let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w as f32, h as f32))), ..Default::default() };
                let top = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 480.0));
                let mut full = ctx.run_ui(input, |ui| online.ui(ui.ctx(), view, Some(top), Orientation::North, true, true, &mut menus));
                for (id, delta) in std::mem::take(&mut full.textures_delta.set).into_iter().flat_map(|(id, d)| d.into_iter().map(move |d| (id, d))) {
                    let egui::ImageData::Color(img) = &delta.image;
                    match delta.pos {
                        None => {
                            textures.insert(id, (img.width(), img.height(), img.pixels.clone()));
                        }
                        Some([ox, oy]) => {
                            let t = textures.get_mut(&id).unwrap();
                            for y in 0..img.height() {
                                for x in 0..img.width() {
                                    t.2[(oy + y) * t.0 + ox + x] = img.pixels[y * img.width() + x];
                                }
                            }
                        }
                    }
                }
                last = Some(full.shapes);
            }
            let mut fb = back.clone();
            for clipped in ctx.tessellate(last.unwrap(), 1.0) {
                let Primitive::Mesh(mesh) = clipped.primitive else { continue };
                let (tw, th, tpx) = &textures[&mesh.texture_id];
                let clip = clipped.clip_rect;
                for t in mesh.indices.chunks(3) {
                    let [a, b, c] = [t[0], t[1], t[2]].map(|i| mesh.vertices[i as usize]);
                    let area = (b.pos - a.pos).x * (c.pos - a.pos).y - (b.pos - a.pos).y * (c.pos - a.pos).x;
                    if area.abs() < 1e-6 {
                        continue;
                    }
                    let (min, max) = (a.pos.min(b.pos).min(c.pos).max(clip.min), a.pos.max(b.pos).max(c.pos).min(clip.max));
                    for y in (min.y.floor().max(0.0) as usize)..(max.y.ceil().min(h as f32) as usize) {
                        for x in (min.x.floor().max(0.0) as usize)..(max.x.ceil().min(w as f32) as usize) {
                            let p = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                            let w0 = ((b.pos - p).x * (c.pos - p).y - (b.pos - p).y * (c.pos - p).x) / area;
                            let w1 = ((c.pos - p).x * (a.pos - p).y - (c.pos - p).y * (a.pos - p).x) / area;
                            let w2 = 1.0 - w0 - w1;
                            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                                continue;
                            }
                            let uv = a.uv.to_vec2() * w0 + b.uv.to_vec2() * w1 + c.uv.to_vec2() * w2;
                            let tx = (uv.x * *tw as f32 - 0.5).round().clamp(0.0, *tw as f32 - 1.0) as usize;
                            let ty = (uv.y * *th as f32 - 0.5).round().clamp(0.0, *th as f32 - 1.0) as usize;
                            let tex = tpx[ty * tw + tx].to_array();
                            let d = &mut fb[y * w + x];
                            let src: [f32; 4] = std::array::from_fn(|k| {
                                let col = [a, b, c].iter().zip([w0, w1, w2]).map(|(v, f)| v.color.to_array()[k] as f32 / 255.0 * f).sum::<f32>();
                                tex[k] as f32 / 255.0 * col
                            });
                            for k in 0..4 {
                                d[k] = src[k] + d[k] * (1.0 - src[3]);
                            }
                        }
                    }
                }
            }
            let bytes: Vec<u8> = fb.iter().flat_map(|p| [p[0], p[1], p[2], 1.0].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)).collect();
            let mut enc = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(dir.join(format!("{name}.png"))).unwrap()), w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.write_header().unwrap().write_image_data(&bytes).unwrap();
        };
        let friends = vec![
            Friend { steam: SERENA, name: "Serena".into(), in_redex: true, status: Some("Kalos Friends \u{b7} In Kalos".into()) },
            Friend { steam: CALEM, name: "Calem".into(), in_redex: true, status: Some("Playing offline".into()) },
            Friend { steam: TIERNO, name: "Tierno".into(), in_redex: false, status: None },
            Friend { steam: 76_561_197_960_287_937, name: "Shauna".into(), in_redex: true, status: Some("In Kalos".into()) },
            Friend { steam: 76_561_197_960_287_938, name: "Trevor".into(), in_redex: false, status: None },
        ];
        let view = View { world: Some("steam:90297831083063301".into()), online: true, peers: vec![peer(4, "Serena", 3.0), peer(3, "Calem", 6.0)], friends, ..View::default() };
        // the existing panel, for comparison
        let mut o = Online { motion: false, ..Online::default() };
        o.toggle_players();
        shot("online-players-nearby", &mut o, &[view.clone()]);
        // the Friends tab: one ignored, two new messages from Calem
        let mut o = Online { motion: false, ..Online::default() };
        o.toggle_players();
        o.show_friends_tab(true);
        o.ignored.push((76_561_197_960_287_937, None, 0));
        let first = View { friend_msgs: vec![says(CALEM, "are you near Lumiose?"), says(CALEM, "come to Kalos Friends")], ..view.clone() };
        shot("friends-tab", &mut o, &[first.clone(), view.clone()]);
        // a chat with two tabs
        o.open_chat(CALEM);
        o.fr.chat_text = "on my way".into();
        let more = View { friend_msgs: vec![says(SERENA, "gg!")], ..view.clone() };
        o.fr.chats[0].lines.push(friends_ui::ChatLine { mine: true, text: "yes, by the Pok\u{e9}mon Center".into(), failed: None });
        shot("friends-chat", &mut o, &[more, view.clone()]);
        // the invite prompt
        let mut o = Online { motion: false, ..Online::default() };
        let inv = View { friend_msgs: vec![invite(SERENA, "Kalos Friends", "steam:90297831083063301")], ..view.clone() };
        shot("friends-invite", &mut o, &[inv, view.clone()]);
    }

    /// "Invite to my world" sends only while in a Steam world, and only to a friend running RE:DEX.
    #[test]
    fn invite_button_only_in_steam_world() {
        for (world, on, to, ok) in [
            (None, false, SERENA, false),
            (Some("127.0.0.1:7777"), true, SERENA, false),
            (Some("steam:5"), false, SERENA, false),
            (Some("steam:5"), true, TIERNO, false),
            (Some("steam:5"), true, SERENA, true),
            // a friend whose RE:DEX speaks another protocol
            (Some("steam:5"), true, 76_561_197_960_287_940, false),
        ] {
            let (ctx, mut online, mut menus) = quiet();
            let view = View { world: world.map(String::from), online: on, ..friends_view() };
            frame(&ctx, &mut online, &mut menus, &view, &[]);
            online.open_friend_menu(to);
            online.friend_act(1, &view, &mut menus);
            assert_eq!(online.requests.contains(&Request::Invite { to }), ok, "{world:?} online {on} to {to}");
        }
    }

    /// With a controller alone: right to the Friends tab (presence asked), down and A open the second friend's menu, A
    /// chats with Steam's keyboard, A sends, B closes; an invite's prompt opens on Not now, takes no A in its first half
    /// second, then Left and A join; B is Not now.
    #[test]
    fn a_controller_alone_goes_through_friends() {
        let (ctx, mut online, mut menus) = quiet();
        let view = friends_view();
        online.toggle_players();
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Right]);
        assert!(online.fr.tab_friends && online.requests.contains(&Request::FriendsOpened));
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Down, Nav::Accept]);
        assert_eq!(online.fr.menu.map(|m| m.0), Some(SERENA));
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept]);
        assert!(online.fr.chat_open && online.requests.last() == Some(&Request::Type(String::new())));
        frame(&ctx, &mut online, &mut menus, &View { typed: Some("hey".into()), ..view.clone() }, &[]);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept]);
        assert_eq!(online.requests.last(), Some(&Request::FriendChat { to: SERENA, text: "hey".into() }));
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Back]);
        assert!(!online.fr.chat_open && online.players, "back on the list");
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Left]);
        assert!(!online.fr.tab_friends);
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: vec![invite(CALEM, "Calem's", "steam:9")], ..view.clone() }, &[]);
        let asked = online.requests.len();
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept]);
        assert!(online.fr.invite.is_some() && online.requests.len() == asked, "an A at once is not an answer");
        online.age_invite(INVITE_ARM);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Left, Nav::Accept]);
        assert_eq!(online.requests.last(), Some(&Request::AcceptInvite { from: CALEM, connect: "steam:9".into() }));
        assert!(online.fr.invite.is_none());
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: vec![invite(CALEM, "Calem's", "steam:9")], ..view.clone() }, &[]);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Back]);
        assert_eq!(online.requests.last(), Some(&Request::DeclineInvite { from: CALEM }));
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Back]);
        assert!(!online.takes_input());
    }

    /// An invite never takes a press that wasn't meant for it: it opens on Not now, A in its first half second does
    /// nothing, and while the chat or whisper box is up their keys stay theirs (A sends, B closes the box).
    #[test]
    fn invite_prompt_ignores_early_and_typing_presses() {
        let (ctx, mut online, mut menus) = quiet();
        let view = friends_view();
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        online.fr.pad = true;
        online.open_chat(SERENA);
        online.fr.chat_text = "omw".into();
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: vec![invite(CALEM, "C", "steam:9")], ..view.clone() }, &[]);
        assert_eq!(online.fr.invite_focus, 1, "Not now");
        online.age_invite(INVITE_ARM);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept]);
        assert_eq!(online.requests.last(), Some(&Request::FriendChat { to: SERENA, text: "omw".into() }), "the chat's A");
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Back]);
        assert!(!online.fr.chat_open && online.fr.invite.is_some(), "B closed the chat, not the prompt");
        // the prompt answers now; a fresh one (replacing it) is armed again
        frame(&ctx, &mut online, &mut menus, &View { friend_msgs: vec![invite(SERENA, "S", "steam:8")], ..view.clone() }, &[]);
        let asked = online.requests.len();
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Left, Nav::Accept]);
        assert_eq!(online.requests.len(), asked);
        online.age_invite(INVITE_ARM);
        frame(&ctx, &mut online, &mut menus, &view, &[Nav::Accept]);
        assert_eq!(online.requests.last(), Some(&Request::AcceptInvite { from: SERENA, connect: "steam:8".into() }));
    }

    /// A chat line the runtime could not send is marked with why; a session Steam gave up on marks the last one sent.
    #[test]
    fn failed_chat_line_is_marked() {
        let (ctx, mut online, mut menus) = quiet();
        let view = friends_view();
        frame(&ctx, &mut online, &mut menus, &view, &[]);
        online.open_chat(SERENA);
        for text in ["one", "two"] {
            online.fr.chat_text = text.into();
            online.send_chat(&mut menus);
        }
        frame(&ctx, &mut online, &mut menus, &View { friend_failed: vec![(SERENA, Some("one".into()), "sending too fast".into())], ..view.clone() }, &[]);
        let failed = |o: &Online| o.fr.chats[0].lines.iter().map(|l| l.failed.clone()).collect::<Vec<_>>();
        assert_eq!(failed(&online), [Some("sending too fast".to_string()), None]);
        frame(&ctx, &mut online, &mut menus, &View { friend_failed: vec![(SERENA, None, "connection failed".into())], ..view.clone() }, &[]);
        assert_eq!(failed(&online), [Some("sending too fast".to_string()), Some("connection failed".to_string())]);
    }

    fn ignore_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("redex_ignore_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("user")).unwrap();
        dir
    }

    /// A malformed or unreadable Ignore file is never overwritten with less: it is kept aside as .bad-<time>, its
    /// readable ids count, and a BOM is no reason to lose the first id.
    #[test]
    fn a_bad_ignore_file_is_kept_aside() {
        let (ctx, _, mut menus) = quiet();
        let dir = ignore_dir("bad");
        let file = dir.join("user/ignored-1.txt");
        let view = View { ignore_file: Some(file.clone()), ..friends_view() };
        let bad = |dir: &std::path::Path| std::fs::read_dir(dir.join("user")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| n.contains(".bad-")).count();
        // a BOM, then fine ids: read whole, nothing set aside
        std::fs::write(&file, format!("\u{feff}{SERENA}\n{CALEM}\n")).unwrap();
        let (_, mut o, _) = quiet();
        frame(&ctx, &mut o, &mut menus, &view, &[]);
        assert!(o.ignoring(0, SERENA, &None) && o.ignoring(0, CALEM, &None));
        assert_eq!(bad(&dir), 0);
        // a line that isn't an id: the file is kept aside, the ids that read stay ignored and are saved again
        std::fs::write(&file, format!("{SERENA}\nnot an id\n")).unwrap();
        let (_, mut o, _) = quiet();
        frame(&ctx, &mut o, &mut menus, &view, &[]);
        assert!(o.ignoring(0, SERENA, &None));
        assert_eq!(bad(&dir), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), SERENA.to_string());
        // not UTF-8 (UTF-16 from a shell): kept aside, never emptied over
        std::fs::write(&file, [0xFF, 0xFE, b'1', 0, b'2', 0]).unwrap();
        let (_, mut o, _) = quiet();
        frame(&ctx, &mut o, &mut menus, &view, &[]);
        assert_eq!(bad(&dir), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ignores made before Steam was up (no file yet) are kept when the account's file loads, and saved with it.
    #[test]
    fn ignores_before_steam_are_kept_and_saved() {
        let (ctx, mut online, mut menus) = quiet();
        let dir = ignore_dir("early");
        let file = dir.join("user/ignored-2.txt");
        std::fs::write(&file, format!("{CALEM}\n")).unwrap();
        frame(&ctx, &mut online, &mut menus, &friends_view(), &[]);
        online.open_friend_menu(SERENA);
        online.friend_act(2, &friends_view(), &mut menus);
        frame(&ctx, &mut online, &mut menus, &View { ignore_file: Some(file.clone()), ..friends_view() }, &[]);
        assert!(online.ignoring(0, SERENA, &None) && online.ignoring(0, CALEM, &None));
        let saved: Vec<u64> = std::fs::read_to_string(&file).unwrap().lines().map(|l| l.parse().unwrap()).collect();
        assert!(saved.contains(&SERENA) && saved.contains(&CALEM), "{saved:?}");
        assert_eq!(online.requests.last(), Some(&Request::Ignored(saved)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}