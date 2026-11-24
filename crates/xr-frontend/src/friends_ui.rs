//! Friends: the Friends tab beside Players nearby, the chat window (a tab per friend) and the invite prompt, drawn
//! over the game like the other online panels. A child of `online`: it shares its panels' look and the Ignore list.
//! The runtime hands over friends and their frames in the [`View`] and sends what [`Request`]s ask.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Id, Pos2, Rect, RichText, Sense, Vec2};
use xr_net::friends::{Friend, FriendMsg, Limiter};

use super::{card, focus_ring, hint, menus, row, short, whisper_text, Menus, Nav, Online, Request, View, ACCENT, QUICK, WHISPER_CHARS, WHISPER_MAX, WHISPER_WINDOW};

/// Friends' presence is asked again this often while the Friends tab is open.
pub const FRIENDS_AGAIN: Duration = Duration::from_secs(10);
/// Lines a chat tab keeps (this run only: Steam keeps no in-game chat).
const CHAT_LINES: usize = 50;
const FRIEND_ACTIONS: [&str; 3] = ["Chat", "Invite to my world", "Ignore"];
const ONLINE_GREEN: Color32 = Color32::from_rgb(0x3C, 0xB3, 0x71);
/// An invite prompt takes no A / Enter / Join for this long after it shows (a press meant for the game or a box).
pub const INVITE_ARM: Duration = Duration::from_millis(500);

pub(super) struct ChatLine {
    pub mine: bool,
    pub text: String,
    /// not sent, and why (a line of ours only)
    pub failed: Option<String>,
}

pub(super) struct Chat {
    pub steam: u64,
    pub name: String,
    /// oldest first
    pub lines: Vec<ChatLine>,
    pub unread: usize,
}

impl Chat {
    fn push(&mut self, mine: bool, text: String) {
        self.lines.push(ChatLine { mine, text, failed: None });
        if self.lines.len() > CHAT_LINES {
            self.lines.remove(0);
        }
    }
}

pub(super) struct Invite {
    pub from: u64,
    pub name: String,
    pub world: String,
    pub connect: String,
    shown: Instant,
}

/// What the friends panels remember between frames.
#[derive(Default)]
pub(super) struct FriendsUi {
    /// the Players panel shows the Friends tab
    pub tab_friends: bool,
    /// the row the keyboard is on in the Friends tab
    pub focus: usize,
    /// the friend menu: whose, and the action the keyboard is on
    pub menu: Option<(u64, usize)>,
    pub menu_open: bool,
    pub chats: Vec<Chat>,
    /// the chat tab shown
    pub active: usize,
    pub chat_open: bool,
    pub chat_text: String,
    chat_focus: bool,
    /// opened with a controller: A types with Steam's keyboard, then sends
    chat_pad: bool,
    /// at most one prompt: a new invite replaces it
    pub invite: Option<Invite>,
    /// 0 Join, 1 Not now
    pub invite_focus: usize,
    /// the last step came from a controller
    pub pad: bool,
    asked: Option<Instant>,
    limiter: Limiter,
    ignore_file: Option<PathBuf>,
    /// the Ignore file could not be read or set aside: it is not written over this run
    ignore_locked: bool,
    /// the view's friends this frame
    list: Vec<Friend>,
    toasts: Vec<String>,
}

/// Friends running RE:DEX first, then the others; each by name, whatever its case.
pub fn friends_sorted(list: &[Friend]) -> Vec<Friend> {
    let mut list = list.to_vec();
    list.sort_by_key(|f| (!f.in_redex, f.name.to_lowercase()));
    list
}

impl Online {
    fn friend(&self, steam: u64) -> Option<&Friend> {
        self.fr.list.iter().find(|f| f.steam == steam)
    }

    fn friend_name(&self, steam: u64) -> String {
        self.friend(steam).map(|f| f.name.clone()).or_else(|| self.fr.chats.iter().find(|c| c.steam == steam).map(|c| c.name.clone())).unwrap_or_else(|| "A friend".into())
    }

    fn in_steam_world(view: &View) -> bool {
        view.online && view.world.as_deref().is_some_and(redex_proto::steam_world)
    }

    /// A frame from a friend (already past the link's gate): dropped when they are ignored here.
    pub fn friend_message(&mut self, from: u64, m: FriendMsg) {
        if self.ignoring(0, from, &None) {
            return;
        }
        let name = self.friend_name(from);
        match m {
            FriendMsg::Chat(text) => {
                // a friend's text comes straight from their game: control and bidi characters out, as names
                let Some(text) = whisper_text(&xr_net::client::clean(&text, WHISPER_CHARS)) else { return };
                let i = self.chat_index(from, &name);
                let shown = self.fr.chat_open && self.fr.active == i;
                let chat = &mut self.fr.chats[i];
                chat.push(false, text.clone());
                if !shown {
                    chat.unread += 1;
                    self.fr.toasts.push(format!("{name}: {}", short(&text, 48)));
                }
            }
            FriendMsg::Invite { world, connect } if redex_proto::steam_world(&connect) => {
                let world = xr_net::client::clean(&world, redex_proto::friends::WORLD_CHARS);
                let world = if world.is_empty() { "their world".to_owned() } else { world };
                // Not now first, and armed only after INVITE_ARM: a press meant for something else never joins
                self.fr.invite = Some(Invite { from, name, world, connect, shown: Instant::now() });
                self.fr.invite_focus = 1;
            }
            FriendMsg::Invite { .. } => {}
            FriendMsg::Decline => self.fr.toasts.push(format!("{name} can't come right now")),
        }
    }

    /// The runtime could not send to `to`: the line with `text` (or, for a lost session, the last one sent) is marked.
    fn chat_failed(&mut self, to: u64, text: Option<&str>, why: &str) {
        let Some(chat) = self.fr.chats.iter_mut().find(|c| c.steam == to) else { return };
        let line = chat.lines.iter_mut().rev().filter(|l| l.mine && l.failed.is_none()).find(|l| text.is_none_or(|t| l.text == t));
        if let Some(l) = line {
            l.failed = Some(why.to_owned());
        }
    }

    #[cfg(test)]
    pub(super) fn age_invite(&mut self, by: Duration) {
        if let Some(i) = &mut self.fr.invite {
            i.shown = i.shown.checked_sub(by).unwrap_or(i.shown);
        }
    }

    fn chat_index(&mut self, steam: u64, name: &str) -> usize {
        match self.fr.chats.iter().position(|c| c.steam == steam) {
            Some(i) => i,
            None => {
                self.fr.chats.push(Chat { steam, name: name.to_owned(), lines: Vec::new(), unread: 0 });
                self.fr.chats.len() - 1
            }
        }
    }

    /// (name, unread) of each chat tab
    #[cfg(test)]
    pub(super) fn chat_tabs(&self) -> Vec<(String, usize)> {
        self.fr.chats.iter().map(|c| (c.name.clone(), c.unread)).collect()
    }

    pub fn friends_unread(&self) -> usize {
        self.fr.chats.iter().map(|c| c.unread).sum()
    }

    /// opens the chat window on this friend's tab
    pub(super) fn open_chat(&mut self, steam: u64) {
        let name = self.friend_name(steam);
        self.fr.active = self.chat_index(steam, &name);
        self.fr.chats[self.fr.active].unread = 0;
        (self.fr.chat_open, self.fr.chat_focus, self.fr.menu_open, self.whisper_open) = (true, true, false, false);
        self.fr.chat_pad = self.fr.pad;
        if self.fr.pad {
            self.requests.push(Request::Type(self.fr.chat_text.clone()));
        }
    }

    /// why the shown chat can't send now (the friend left RE:DEX or Steam)
    pub(super) fn chat_blocked(&self) -> Option<String> {
        let c = self.fr.chats.get(self.fr.active)?;
        (!self.friend(c.steam).is_some_and(|f| f.in_redex)).then(|| format!("{} isn't in RE:DEX", c.name))
    }

    pub(super) fn send_chat(&mut self, menus: &mut Menus) {
        let Some(to) = self.fr.chats.get(self.fr.active).map(|c| c.steam) else { return };
        let Some(text) = whisper_text(&self.fr.chat_text) else { return };
        if let Some(why) = self.chat_blocked() {
            menus.toast(why);
            return;
        }
        if !self.fr.limiter.allow(Instant::now(), WHISPER_MAX, WHISPER_WINDOW) {
            menus.toast(format!("Chat: slow down, at most {WHISPER_MAX} every {} seconds", WHISPER_WINDOW.as_secs()));
            return;
        }
        self.requests.push(Request::FriendChat { to, text: text.clone() });
        self.fr.chats[self.fr.active].push(true, text);
        self.fr.chat_text.clear();
    }

    pub(super) fn open_friend_menu(&mut self, steam: u64) {
        self.fr.menu = Some((steam, 0));
        self.fr.menu_open = true;
    }

    /// does an action of the friend menu
    pub(super) fn friend_act(&mut self, action: usize, view: &View, menus: &mut Menus) {
        let Some((steam, _)) = self.fr.menu else { return };
        self.fr.menu_open = false;
        let Some(f) = self.friend(steam).cloned() else {
            menus.toast("That friend is no longer online");
            return;
        };
        match action {
            0 | 1 if !f.in_redex => menus.toast(format!("{} isn't running RE:DEX: message them through Steam", f.name)),
            0 => self.open_chat(steam),
            1 if !Online::in_steam_world(view) => menus.toast("Invites need a Steam world: join one, then invite friends into it"),
            1 => self.requests.push(Request::Invite { to: steam }),
            _ => {
                let who = super::Target { session: 0, name: f.name, steam, version: "", world: None };
                self.toggle_ignore(&who, menus);
            }
        }
    }

    /// what a friend's row says on its right
    pub(super) fn friend_detail(&self, f: &Friend) -> String {
        let unread = self.fr.chats.iter().find(|c| c.steam == f.steam).map_or(0, |c| c.unread);
        match () {
            _ if self.ignoring(0, f.steam, &None) => "ignored".into(),
            _ if unread > 0 => format!("{unread} new"),
            _ if f.in_redex => f.status.clone().unwrap_or_else(|| "In RE:DEX".into()),
            // another protocol version says so
            _ => f.status.clone().unwrap_or_else(|| "Not in RE:DEX".into()),
        }
    }

    /// the Steam accounts ignored (the list kept on disk and given to the link)
    fn ignored_ids(&self) -> Vec<u64> {
        self.ignored.iter().map(|e| e.0).filter(|&s| s != 0).collect()
    }

    /// Reads this Steam account's Ignore list (one SteamID64 a line; a missing file is an empty list) into the accounts
    /// ignored so far (those made before Steam was up stay, and are saved with it), and tells the link. A file that
    /// doesn't read whole (a line that isn't an id, not UTF-8) is kept aside as `<name>.bad-<time>` and its ids that
    /// read count; one that can't be read at all is never written over this run.
    pub fn load_ignored(&mut self, path: &Path) {
        self.fr.ignore_file = Some(path.to_path_buf());
        self.fr.ignore_locked = false;
        let (found, bad) = match std::fs::read(path) {
            Ok(bytes) => {
                let mut bad = std::str::from_utf8(&bytes).is_err();
                let mut ids = Vec::new();
                for l in String::from_utf8_lossy(&bytes).lines().map(|l| l.trim().trim_start_matches('\u{feff}').trim()).filter(|l| !l.is_empty()) {
                    match l.parse::<u64>() {
                        Ok(id) if id != 0 => ids.push(id),
                        _ => bad = true,
                    }
                }
                (ids, bad)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(e) => {
                log::warn!("friends: cannot read the Ignore list {}: {e}", path.display());
                self.fr.toasts.push(format!("The Ignore list can't be read ({e}); it is not saved over until RE:DEX restarts"));
                self.fr.ignore_locked = true;
                (Vec::new(), false)
            }
        };
        if bad {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            let aside = path.with_file_name(format!("{}.bad-{stamp}", path.file_name().map_or("ignored".into(), |n| n.to_string_lossy())));
            match std::fs::rename(path, &aside) {
                Ok(()) => {
                    log::warn!("friends: the Ignore list {} had lines that aren't ids; kept as {}", path.display(), aside.display());
                    self.fr.toasts.push(format!("Part of the Ignore list didn't read; the file is kept as {}", aside.display()));
                }
                Err(e) => {
                    log::warn!("friends: cannot set aside the Ignore list {}: {e}", path.display());
                    self.fr.ignore_locked = true;
                }
            }
        }
        for &id in &found {
            if !self.ignoring(0, id, &None) {
                self.ignored.push((id, None, 0));
            }
        }
        let now = self.ignored_ids();
        let same = now.len() == found.len() && now.iter().all(|id| found.contains(id));
        if !self.fr.ignore_locked && (!same || bad && !now.is_empty()) {
            self.save_ignored(&now);
        }
        self.requests.push(Request::Ignored(now));
    }

    /// writes the list whole (a new file renamed over the old), unless the file couldn't be read this run
    fn save_ignored(&mut self, ids: &[u64]) {
        let Some(path) = self.fr.ignore_file.clone().filter(|_| !self.fr.ignore_locked) else { return };
        let text: String = ids.iter().map(|id| format!("{id}\n")).collect();
        let tmp = path.with_extension("tmp");
        let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&tmp, text)).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = saved {
            log::warn!("friends: cannot save the Ignore list {}: {e}", path.display());
            self.fr.toasts.push(format!("The Ignore list could not be saved ({e}); it holds until RE:DEX closes"));
        }
    }

    /// the Ignore list changed: to the link, and to disk
    pub(super) fn ignore_changed(&mut self) {
        let ids = self.ignored_ids();
        self.save_ignored(&ids);
        if self.fr.ignore_locked && self.fr.ignore_file.is_some() {
            self.fr.toasts.push("The Ignore list file couldn't be read: this change holds until RE:DEX closes".into());
        }
        self.requests.push(Request::Ignored(ids));
    }

    /// the frame's friends and their frames, before the panels
    pub(super) fn friends_frame(&mut self, view: &View, menus: &mut Menus) {
        if view.ignore_file.is_some() && view.ignore_file != self.fr.ignore_file {
            let path = view.ignore_file.clone().unwrap_or_default();
            self.load_ignored(&path);
        }
        self.fr.list = view.friends.clone();
        for (from, m) in &view.friend_msgs {
            self.friend_message(*from, m.clone());
        }
        for (to, text, why) in &view.friend_failed {
            self.chat_failed(*to, text.as_deref(), why);
        }
        for t in std::mem::take(&mut self.fr.toasts) {
            menus.toast(t);
        }
        if self.players && self.fr.tab_friends && self.fr.asked.is_none_or(|t| t.elapsed() >= FRIENDS_AGAIN) {
            self.fr.asked = Some(Instant::now());
            self.requests.push(Request::FriendsOpened);
        }
        if self.fr.chat_open {
            self.fr.active = self.fr.active.min(self.fr.chats.len().saturating_sub(1));
            if let Some(c) = self.fr.chats.get_mut(self.fr.active) {
                c.unread = 0;
            }
            if let Some(typed) = &view.typed {
                self.fr.chat_text = typed.chars().take(WHISPER_CHARS).collect();
            }
        }
    }

    pub(super) fn show_friends_tab(&mut self, on: bool) {
        if on && !self.fr.tab_friends {
            // asked at once on opening
            self.fr.asked = Some(Instant::now());
            self.requests.push(Request::FriendsOpened);
        }
        self.fr.tab_friends = on;
    }

    fn accept_invite(&mut self, menus: &mut Menus) {
        if self.fr.invite.as_ref().is_some_and(|i| i.shown.elapsed() < INVITE_ARM) {
            return;
        }
        if let Some(i) = self.fr.invite.take() {
            menus.toast(format!("Joining {}", i.world));
            self.requests.push(Request::AcceptInvite { from: i.from, connect: i.connect });
        }
    }

    pub(super) fn decline_invite(&mut self) {
        if let Some(i) = self.fr.invite.take() {
            self.requests.push(Request::DeclineInvite { from: i.from });
        }
    }

    /// A step in the friends panels on top, if one is. A box being typed in (report, whisper, chat) keeps its keys, even
    /// over an invite prompt; then the prompt (no Accept in its first INVITE_ARM), then the friend menu and list.
    pub(super) fn friend_step(&mut self, nav: Nav, pad: bool, view: &View, menus: &mut Menus) -> bool {
        self.fr.pad = pad;
        if self.report_open || self.whisper_open {
            return false;
        } else if self.fr.chat_open {
            let n = self.fr.chats.len().max(1);
            match nav {
                Nav::Back => self.fr.chat_open = false,
                Nav::Left => self.fr.active = (self.fr.active + n - 1) % n,
                Nav::Right => self.fr.active = (self.fr.active + 1) % n,
                Nav::Accept if pad => {
                    if whisper_text(&self.fr.chat_text).is_some() {
                        self.send_chat(menus);
                    } else {
                        self.fr.chat_pad = true;
                        self.requests.push(Request::Type(self.fr.chat_text.clone()));
                    }
                }
                _ => {}
            }
        } else if let Some(i) = &self.fr.invite {
            let armed = i.shown.elapsed() >= INVITE_ARM;
            match nav {
                Nav::Up | Nav::Left => self.fr.invite_focus = 0,
                Nav::Down | Nav::Right => self.fr.invite_focus = 1,
                Nav::Accept if !armed => {}
                Nav::Accept if self.fr.invite_focus == 0 => self.accept_invite(menus),
                Nav::Accept | Nav::Back => self.decline_invite(),
            }
        } else if self.fr.menu_open {
            let Some((steam, focus)) = self.fr.menu else { return false };
            match nav {
                Nav::Up => self.fr.menu = Some((steam, focus.saturating_sub(1))),
                Nav::Down => self.fr.menu = Some((steam, (focus + 1).min(FRIEND_ACTIONS.len() - 1))),
                Nav::Accept => self.friend_act(focus, view, menus),
                Nav::Back => self.fr.menu_open = false,
                _ => {}
            }
        } else if self.players && self.fr.tab_friends && !self.menu_open && !self.browser {
            let list = friends_sorted(&view.friends);
            match nav {
                Nav::Up => self.fr.focus = self.fr.focus.saturating_sub(1),
                Nav::Down => self.fr.focus = (self.fr.focus + 1).min(list.len().saturating_sub(1)),
                Nav::Accept => {
                    if let Some(f) = list.get(self.fr.focus) {
                        self.open_friend_menu(f.steam);
                    }
                }
                Nav::Left => self.show_friends_tab(false),
                Nav::Right => {}
                Nav::Back => self.players = false,
            }
        } else {
            return false;
        }
        true
    }

    /// the tabs at the top of the Players panel; Some(friends?) when one was clicked
    pub(super) fn tabs(&self, ui: &mut egui::Ui, nearby: usize) -> Option<bool> {
        let colors = menus::palette(ui.visuals().dark_mode);
        let mut clicked = None;
        ui.horizontal(|ui| {
            let unread = self.friends_unread();
            for (friends, text, count) in [(false, "Players nearby", nearby), (true, "Friends", unread)] {
                let on = self.fr.tab_friends == friends;
                let r = ui.add(egui::Label::new(RichText::new(text).strong().size(if on { 17.0 } else { 15.0 }).color(if on { colors.text } else { colors.weak })).sense(Sense::click()));
                if on {
                    let line = Rect::from_min_size(Pos2::new(r.rect.left(), r.rect.bottom() + 1.0), Vec2::new(r.rect.width(), 2.0));
                    ui.painter().rect_filled(line, 1.0, ACCENT);
                }
                if r.clicked() {
                    clicked = Some(friends);
                }
                if r.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                // Players nearby: how many; Friends: new messages, only when there are
                if !friends || count > 0 {
                    let color = if count == 0 { colors.weak } else { Color32::WHITE };
                    let galley = ui.painter().layout_no_wrap(count.to_string(), FontId::proportional(12.0), color);
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x + 14.0, 18.0), Sense::hover());
                    ui.painter().rect_filled(rect, 9.0, if count == 0 { colors.line } else { ACCENT });
                    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
                }
                ui.add_space(8.0);
            }
        });
        ui.add_space(4.0);
        clicked
    }

    /// the Friends tab's rows; the index clicked
    pub(super) fn friend_rows(&self, ui: &mut egui::Ui, view: &View) -> Option<usize> {
        let list = friends_sorted(&view.friends);
        let mut picked = None;
        if list.is_empty() {
            ui.label(RichText::new("No Steam friends online right now.").weak());
            ui.label(RichText::new("Friends running RE:DEX show here; invite them or chat across worlds.").small().weak());
            ui.add_space(4.0);
        }
        egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
            for (i, f) in list.iter().enumerate() {
                if friend_row(ui, self.fr.focus == i, f, &self.friend_detail(f)).clicked() {
                    picked = Some(i);
                }
            }
        });
        hint(ui, "Up/Down choose \u{b7} A open \u{b7} Left/Right tabs \u{b7} B close");
        picked
    }

    /// the friend menu, beside the list
    pub(super) fn friend_menu_panel(&mut self, ctx: &egui::Context, view: &View, menus: &mut Menus) {
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("friend menu shown"), self.fr.menu_open, self.time(QUICK)));
        if t <= 0.0 {
            return;
        }
        let Some((steam, focus)) = self.fr.menu else { return };
        let f = self.friend(steam).cloned().unwrap_or(Friend { steam, name: self.friend_name(steam), in_redex: false, status: None });
        let ignored = self.ignoring(0, steam, &None);
        let steam_world = Online::in_steam_world(view);
        let mut chosen = None;
        egui::Area::new(Id::new("friend menu"))
            .fixed_pos(Pos2::new(304.0, 12.0 + (1.0 - t) * 8.0))
            .order(egui::Order::Foreground)
            .interactable(self.fr.menu_open)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                card(ui, 230.0, |ui| {
                    ui.add(egui::Label::new(RichText::new(&f.name).strong().size(16.0)).truncate());
                    let status = if f.in_redex { f.status.clone().unwrap_or_else(|| "In RE:DEX".into()) } else { "Not running RE:DEX".into() };
                    ui.label(RichText::new(short(&status, 36)).small().weak());
                    ui.add_space(4.0);
                    for (i, &action) in FRIEND_ACTIONS.iter().enumerate() {
                        let (action, detail) = match i {
                            2 if ignored => ("Stop ignoring", ""),
                            0 | 1 if !f.in_redex => (action, "Steam only"),
                            1 if !steam_world => (action, "Steam world"),
                            _ => (action, ""),
                        };
                        if row(ui, ("friend action", i), focus == i, None, action, detail).clicked() {
                            chosen = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = chosen {
            self.fr.pad = false;
            self.friend_act(i, view, menus);
        }
        if self.fr.menu_open && ctx.input(|i| i.pointer.any_pressed()) && !ctx.is_pointer_over_egui() {
            self.fr.menu_open = false;
        }
    }

    /// the chat window, bottom right: a tab per friend, the lines, the box
    pub(super) fn chat_panel(&mut self, ctx: &egui::Context, menus: &mut Menus) {
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("chat shown"), self.fr.chat_open, self.time(QUICK)));
        if t <= 0.0 || self.fr.chats.is_empty() {
            return;
        }
        let focus = std::mem::take(&mut self.fr.chat_focus);
        let blocked = self.chat_blocked();
        let (pad, active) = (self.fr.chat_pad, self.fr.active.min(self.fr.chats.len() - 1));
        let (mut send, mut close, mut pick) = (false, false, None);
        let chats = &self.fr.chats;
        let text = &mut self.fr.chat_text;
        egui::Area::new(Id::new("friend chat"))
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-12.0, -12.0 + (1.0 - t) * 16.0))
            .order(egui::Order::Foreground)
            .interactable(self.fr.chat_open)
            .show(ctx, |ui| {
                let colors = menus::palette(ui.visuals().dark_mode);
                ui.multiply_opacity(t);
                card(ui, 340.0, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Chat").strong().size(17.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if menus::button(ui, "Close", false).clicked() {
                                close = true;
                            }
                        });
                    });
                    // the tabs: a friend each, unread in a red dot
                    ui.horizontal_wrapped(|ui| {
                        for (i, c) in chats.iter().enumerate() {
                            let on = i == active;
                            let label = if c.unread > 0 { format!("{} ({})", short(&c.name, 12), c.unread) } else { short(&c.name, 12) };
                            let r = ui.add(egui::Button::new(RichText::new(label).size(13.0).color(if on { Color32::WHITE } else { colors.text })).fill(if on { ACCENT } else { colors.control }).corner_radius(7));
                            if r.clicked() {
                                pick = Some(i);
                            }
                        }
                    });
                    ui.separator();
                    let chat = &chats[active];
                    egui::ScrollArea::vertical().max_height(170.0).auto_shrink([false, true]).stick_to_bottom(true).show(ui, |ui| {
                        if chat.lines.is_empty() {
                            ui.label(RichText::new(format!("Say hello to {}. Chat works across worlds and offline, while you both run RE:DEX.", chat.name)).small().weak());
                        }
                        for line in &chat.lines {
                            let (who, color) = if line.mine { ("You".to_owned(), colors.weak) } else { (chat.name.clone(), ACCENT) };
                            let mut job = egui::text::LayoutJob::default();
                            job.append(&format!("{who}: "), 0.0, egui::TextFormat::simple(FontId::proportional(13.5), color));
                            job.append(&line.text, 0.0, egui::TextFormat::simple(FontId::proportional(13.5), colors.text));
                            if let Some(why) = &line.failed {
                                job.append(&format!("  not sent: {why}"), 0.0, egui::TextFormat::simple(FontId::proportional(12.0), menus::NOTE));
                            }
                            ui.add(egui::Label::new(job).wrap());
                        }
                    });
                    ui.add_space(4.0);
                    if let Some(why) = &blocked {
                        ui.label(RichText::new(why).small().color(menus::NOTE));
                    } else if pad {
                        hint(ui, "A: type with Steam's keyboard, A again sends \u{b7} Left/Right: tabs \u{b7} B: close");
                    }
                    ui.horizontal(|ui| {
                        let field = ui.add_enabled(blocked.is_none(), egui::TextEdit::singleline(text).char_limit(WHISPER_CHARS).hint_text("Message\u{2026}").desired_width(ui.available_width() - 70.0));
                        if focus && blocked.is_none() {
                            field.request_focus();
                        }
                        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            send = true;
                        }
                        if ui.add_enabled_ui(blocked.is_none(), |ui| menus::button(ui, "Send", true)).inner.clicked() {
                            send = true;
                        }
                    });
                });
            });
        if let Some(i) = pick {
            self.fr.active = i;
            self.fr.chats[i].unread = 0;
        }
        if close {
            self.fr.chat_open = false;
        } else if send {
            self.send_chat(menus);
            self.fr.chat_focus = true;
        }
    }

    /// "<name> invites you to <world>": Join / Not now, at the top middle
    pub(super) fn invite_panel(&mut self, ctx: &egui::Context, menus: &mut Menus) {
        let up = self.fr.invite.is_some();
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("invite shown"), up, self.time(QUICK)));
        let Some(i) = self.fr.invite.as_ref().filter(|_| t > 0.0) else { return };
        let (name, world, focus) = (i.name.clone(), i.world.clone(), self.fr.invite_focus);
        let (mut join, mut later) = (false, false);
        egui::Area::new(Id::new("friend invite"))
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 16.0 - (1.0 - t) * 12.0))
            .order(egui::Order::Foreground)
            .interactable(up)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                card(ui, 330.0, |ui| {
                    ui.label(RichText::new("Invite").small().weak());
                    ui.add(egui::Label::new(RichText::new(format!("{name} invites you to {world}")).strong().size(16.0)).wrap());
                    ui.label(RichText::new("You keep your game where it is; joining brings you into their world.").small().weak());
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let j = menus::button(ui, "Join", true);
                            if focus == 0 {
                                focus_ring(ui, j.rect);
                            }
                            join |= j.clicked();
                            let n = menus::button(ui, "Not now", false);
                            if focus == 1 {
                                focus_ring(ui, n.rect);
                            }
                            later |= n.clicked();
                        });
                    });
                    hint(ui, "Left/Right choose \u{b7} A / Enter answers \u{b7} B / Esc: Not now");
                });
            });
        if join {
            self.accept_invite(menus);
        } else if later {
            self.decline_invite();
        }
    }
}

/// a friend's row: a green dot while in RE:DEX, the name (greyed when not), what they're doing on the right
fn friend_row(ui: &mut egui::Ui, focused: bool, f: &Friend, detail: &str) -> egui::Response {
    let colors = menus::palette(ui.visuals().dark_mode);
    let response = row(ui, ("friend", f.steam), focused, None, "", "");
    let rect = response.rect;
    let dot = Pos2::new(rect.left() + 17.0, rect.center().y);
    if f.in_redex {
        ui.painter().circle_filled(dot, 4.0, ONLINE_GREEN);
    } else {
        ui.painter().circle_stroke(dot, 3.5, egui::Stroke::new(1.2, colors.weak));
    }
    let name_end = rect.right() - 142.0;
    ui.painter().with_clip_rect(Rect::from_min_max(rect.min, Pos2::new(name_end, rect.bottom()))).text(
        Pos2::new(rect.left() + 30.0, rect.center().y),
        Align2::LEFT_CENTER,
        &f.name,
        FontId::proportional(14.5),
        if f.in_redex { colors.text } else { colors.weak },
    );
    let color = if detail.ends_with(" new") { ACCENT } else { colors.weak };
    ui.painter().with_clip_rect(Rect::from_min_max(Pos2::new(name_end + 4.0, rect.top()), rect.max)).text(
        Pos2::new(rect.right() - 10.0, rect.center().y),
        Align2::RIGHT_CENTER,
        short(detail, 22),
        FontId::proportional(12.5),
        color,
    );
    response
}
