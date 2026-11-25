//! The Lobby panel (Players list > Lobby…): host a lobby from the running game (name, players, who may come in, the
//! online rules said first), then its room code, who is in it and Remove for each; or join one by its room code, and
//! leave the world the game is in. A child of `online`, drawn like its other panels; the runtime does the requests.
use egui::{Align2, Color32, Id, RichText, Vec2};

use super::{card, focus_ring, hint, menus, radio, row, title, LobbySettings, Menus, Nav, Online, Request, View, ACCESS, LOBBY_RULES, QUICK};

/// What the Lobby panel remembers between frames.
#[derive(Default)]
pub(super) struct LobbyUi {
    pub open: bool,
    /// the host form (kept while the panel closes and opens)
    pub form: LobbySettings,
    /// the room code being typed
    pub code: String,
    pub focus: usize,
    /// the code Copy put up, for the clipboard on this frame
    copied: Option<String>,
    /// just opened: the focus goes to a harmless item once the panel knows what it shows (never on Remove)
    fresh: bool,
    /// a Remove pressed once: this session is removed on the second press
    confirm: Option<u32>,
}

/// The focusable items of the panel, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Item {
    Players,
    Access(usize),
    Host,
    Join,
    Leave,
    Member(usize),
    Copy,
    CloseLobby,
    Close,
}

fn items(view: &View) -> Vec<Item> {
    let mut v = Vec::new();
    match &view.lobby {
        Some(l) => {
            v.extend((0..l.members.len()).map(Item::Member));
            if l.open {
                v.push(Item::Copy);
            }
            v.push(Item::CloseLobby);
        }
        None => {
            v.push(Item::Players);
            v.extend((0..ACCESS.len()).map(Item::Access));
            v.push(Item::Host);
            v.push(Item::Join);
            if view.world.is_some() {
                v.push(Item::Leave);
            }
        }
    }
    v.push(Item::Close);
    v
}

impl Online {
    pub fn open_lobby(&mut self) {
        self.close_all();
        self.lb.open = true;
        (self.lb.focus, self.lb.fresh, self.lb.confirm) = (0, true, None);
    }

    /// a fresh panel's focus: Copy when it hosts (Remove is never one press away), else the top
    fn settle_focus(&mut self, items: &[Item]) {
        if std::mem::take(&mut self.lb.fresh) {
            self.lb.focus = items.iter().position(|i| matches!(i, Item::Copy | Item::CloseLobby)).unwrap_or(0);
        }
        self.lb.focus = self.lb.focus.min(items.len() - 1);
    }

    /// the panel's keyboard and controller step; true when it took it
    pub(super) fn lobby_step(&mut self, nav: Nav, view: &View, menus: &mut Menus) -> bool {
        if !self.lb.open {
            return false;
        }
        let items = items(view);
        self.settle_focus(&items);
        let item = items[self.lb.focus];
        if !matches!(nav, Nav::Accept) || !matches!(item, Item::Member(_)) {
            self.lb.confirm = None;
        }
        match nav {
            Nav::Up => self.lb.focus = self.lb.focus.saturating_sub(1),
            Nav::Down => self.lb.focus = (self.lb.focus + 1).min(items.len() - 1),
            Nav::Left | Nav::Right if item == Item::Players => {
                let max = &mut self.lb.form.max;
                *max = if nav == Nav::Left { max.saturating_sub(1).max(2) } else { (*max + 1).min(8) };
            }
            Nav::Left | Nav::Right => {}
            Nav::Back => self.lb.open = false,
            Nav::Accept => self.lobby_act(item, view, menus),
        }
        true
    }

    fn lobby_act(&mut self, item: Item, view: &View, menus: &mut Menus) {
        match item {
            Item::Players => {}
            Item::Access(i) => self.lb.form.access = ACCESS[i].0,
            Item::Host => match &view.lobby_blocked {
                Some(why) => menus.toast(why.clone()),
                None => {
                    self.requests.push(Request::HostLobby(self.lb.form.clone()));
                    menus.toast("Starting your lobby\u{2026}");
                }
            },
            Item::Join => self.join_code(view, menus),
            Item::Leave => {
                self.requests.push(Request::Leave);
                self.lb.open = false;
            }
            Item::Member(i) => {
                let Some((session, name)) = view.lobby.as_ref().and_then(|l| l.members.get(i)).cloned() else { return };
                // a second press on the same player removes them: one press by habit never does
                if self.lb.confirm.take() == Some(session) {
                    self.requests.push(Request::Kick { session, name });
                } else {
                    self.lb.confirm = Some(session);
                    menus.toast(format!("Remove {name}? Press again to remove them from the lobby"));
                }
            }
            Item::Copy => {
                if let Some(l) = &view.lobby {
                    self.lb.copied = Some(redex_proto::room::shown(&l.code));
                    menus.toast(format!("Room code {} copied", redex_proto::room::shown(&l.code)));
                }
            }
            Item::CloseLobby => self.requests.push(Request::CloseLobby),
            Item::Close => self.lb.open = false,
        }
    }

    fn join_code(&mut self, view: &View, menus: &mut Menus) {
        if let Some(why) = &view.lobby_blocked {
            return menus.toast(why.clone());
        }
        match redex_proto::room::parse(&self.lb.code) {
            Ok(code) => {
                menus.toast(format!("Looking for lobby {}\u{2026}", redex_proto::room::shown(&code)));
                self.requests.push(Request::JoinCode(code));
                self.lb.open = false;
            }
            Err(why) => menus.toast(why),
        }
    }

    pub(super) fn lobby_panel(&mut self, ctx: &egui::Context, view: &View, menus: &mut Menus) {
        let t = menus::ease(ctx.animate_bool_with_time(Id::new("lobby shown"), self.lb.open, self.time(QUICK)));
        if t <= 0.0 {
            return;
        }
        let items = items(view);
        self.settle_focus(&items);
        let focused = items[self.lb.focus];
        let screen = ctx.content_rect();
        ctx.layer_painter(egui::LayerId::background()).rect_filled(screen, 0.0, Color32::from_black_alpha((110.0 * t) as u8));
        let mut acted = None;
        egui::Area::new(Id::new("lobby"))
            .anchor(Align2::CENTER_CENTER, Vec2::new(0.0, (1.0 - t) * 10.0))
            .order(egui::Order::Middle)
            .interactable(self.lb.open)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                card(ui, (screen.width() - 24.0).clamp(300.0, 420.0), |ui| match &view.lobby {
                    Some(l) => {
                        title(ui, &l.settings.name, None);
                        if !l.open {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(16.0).color(menus::ACCENT));
                                ui.label(RichText::new("Starting your lobby\u{2026} (Steam can take a few seconds)").weak());
                            });
                        } else {
                            ui.label(RichText::new("Room code").small().weak());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(redex_proto::room::shown(&l.code)).monospace().strong().size(28.0));
                                let r = menus::button(ui, "Copy", false);
                                if focused == Item::Copy {
                                    focus_ring(ui, r.rect);
                                }
                                if r.clicked() {
                                    acted = Some(Item::Copy);
                                }
                            });
                            let access = ACCESS.iter().find(|a| a.0 == l.settings.access).map_or("", |a| a.1);
                            ui.label(RichText::new(format!("{access} \u{b7} {} of {} players", l.members.len() + 1, l.settings.max)).small().weak());
                        }
                        ui.add_space(4.0);
                        if l.members.is_empty() {
                            ui.label(RichText::new("No one has joined yet. Invite friends from the Friends tab, or give them the code.").weak());
                        }
                        for (i, (session, name)) in l.members.iter().enumerate() {
                            let detail = if self.lb.confirm == Some(*session) { "Remove? Press again" } else { "Remove" };
                            if row(ui, ("lobby member", i), focused == Item::Member(i), None, name, detail).clicked() {
                                acted = Some(Item::Member(i));
                            }
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let c = menus::button(ui, "Close", false);
                                if focused == Item::Close {
                                    focus_ring(ui, c.rect);
                                }
                                if c.clicked() {
                                    acted = Some(Item::Close);
                                }
                                let x = menus::button(ui, "Close lobby", true);
                                if focused == Item::CloseLobby {
                                    focus_ring(ui, x.rect);
                                }
                                if x.clicked() {
                                    acted = Some(Item::CloseLobby);
                                }
                            });
                        });
                        hint(ui, "Remove (press twice) keeps a player out until the lobby closes; another Steam account could still come in unless the lobby is Friends only \u{b7} Close lobby tells everyone and stops it");
                    }
                    None => {
                        title(ui, "Host a lobby", None);
                        ui.add(egui::TextEdit::singleline(&mut self.lb.form.name).char_limit(32).hint_text("Lobby name (your name's lobby)").desired_width(f32::INFINITY));
                        ui.horizontal(|ui| {
                            ui.label("Players");
                            if menus::button(ui, "\u{2212}", false).clicked() {
                                self.lb.form.max = self.lb.form.max.saturating_sub(1).max(2);
                            }
                            let r = ui.label(RichText::new(self.lb.form.max.to_string()).strong());
                            if focused == Item::Players {
                                focus_ring(ui, r.rect.expand(4.0));
                            }
                            if menus::button(ui, "+", false).clicked() {
                                self.lb.form.max = (self.lb.form.max + 1).min(8);
                            }
                            ui.label(RichText::new("2 to 8, you included").small().weak());
                        });
                        for (i, (access, name, _)) in ACCESS.iter().enumerate() {
                            if radio(ui, ("lobby access", i), self.lb.form.access == *access, focused == Item::Access(i), name).clicked() {
                                acted = Some(Item::Access(i));
                            }
                        }
                        // what the choice means, wrapped (Unlisted's warning that codes are not secret among it)
                        if let Some((_, _, detail)) = ACCESS.iter().find(|a| a.0 == self.lb.form.access) {
                            ui.label(RichText::new(*detail).small().weak());
                        }
                        ui.label(RichText::new(LOBBY_RULES).small().color(menus::NOTE));
                        if let Some(why) = &view.lobby_blocked {
                            ui.label(RichText::new(why).small().color(Color32::from_rgb(0xE0, 0x55, 0x5E)));
                        }
                        ui.horizontal(|ui| {
                            let r = ui.add_enabled_ui(view.lobby_blocked.is_none(), |ui| menus::button(ui, "Host lobby", true)).inner;
                            if focused == Item::Host {
                                focus_ring(ui, r.rect);
                            }
                            if r.clicked() {
                                acted = Some(Item::Host);
                            }
                        });
                        ui.separator();
                        ui.label(RichText::new("Join with a room code").small());
                        ui.horizontal(|ui| {
                            let field = ui.add(egui::TextEdit::singleline(&mut self.lb.code).char_limit(9).hint_text("K7M-Q2X").desired_width(ui.available_width() - 90.0));
                            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                acted = Some(Item::Join);
                            }
                            let r = menus::button(ui, "Join", false);
                            if focused == Item::Join {
                                focus_ring(ui, r.rect);
                            }
                            if r.clicked() {
                                acted = Some(Item::Join);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let c = menus::button(ui, "Close", false);
                                if focused == Item::Close {
                                    focus_ring(ui, c.rect);
                                }
                                if c.clicked() {
                                    acted = Some(Item::Close);
                                }
                                if view.world.is_some() {
                                    let r = menus::button(ui, "Leave this world", false);
                                    if focused == Item::Leave {
                                        focus_ring(ui, r.rect);
                                    }
                                    if r.clicked() {
                                        acted = Some(Item::Leave);
                                    }
                                }
                            });
                        });
                        hint(ui, "Up/Down choose \u{b7} Left/Right players \u{b7} Enter / A pick \u{b7} Esc / B close");
                    }
                });
            });
        if let Some(item) = acted {
            if let Some(i) = items.iter().position(|x| *x == item) {
                self.lb.focus = i;
            }
            self.lobby_act(item, view, menus);
        }
        if let Some(text) = self.lb.copied.take() {
            ctx.copy_text(text);
        }
    }
}

#[cfg(test)]
pub(super) fn focus_of(view: &View, item: &str) -> Option<usize> {
    items(view).iter().position(|i| format!("{i:?}").starts_with(item))
}

