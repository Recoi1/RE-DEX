//! The Lobby panel (Players list > Lobby…): host a lobby from the running game (name, players, who may come in, the
//! online rules said first), then its room code, who is in it and Remove for each; or join one by its room code, and
//! leave the world the game is in. A child of `online`, drawn like its other panels; the runtime does the requests.
use egui::{Align2, Color32, Id, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use super::{card_with, focus_ring, glass, glass_button, hint, menus, radio, LobbySettings, Menus, Nav, Online, Request, View, ACCESS, LOBBY_RULES};

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
        let t = ctx.animate_bool_with_time(Id::new("lobby shown"), self.lb.open, self.time(if self.lb.open { crate::theme::OPEN } else { crate::theme::CLOSE }));
        if t <= 0.0 {
            return;
        }
        let items = items(view);
        self.settle_focus(&items);
        let focused = items[self.lb.focus];
        let screen = ctx.content_rect();
        ctx.layer_painter(egui::LayerId::background()).rect_filled(screen, 0.0, Color32::from_black_alpha((120.0 * t) as u8));
        // opens with a little spring (0.92 to 1, back-out), closes quick; never with Windows' animations off
        let grow = if self.lb.open { 0.92 + 0.08 * crate::theme::back_out(t) } else { 0.96 + 0.04 * t };
        let layer = egui::LayerId::new(egui::Order::Middle, Id::new("lobby"));
        let c = screen.center().to_vec2();
        ctx.set_transform_layer(layer, egui::emath::TSTransform::from_translation(c) * egui::emath::TSTransform::from_scaling(grow) * egui::emath::TSTransform::from_translation(-c));
        let mut acted = None;
        use crate::icons::Icon as I;
        use crate::theme as th;
        use glass::Kind;
        egui::Area::new(Id::new("lobby"))
            .anchor(Align2::CENTER_CENTER, Vec2::new(0.0, (1.0 - t) * 14.0))
            .order(egui::Order::Middle)
            .interactable(self.lb.open)
            .show(ctx, |ui| {
                ui.multiply_opacity(t.min(1.0));
                ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);
                card_with(ui, (screen.width() - 24.0).clamp(320.0, 440.0), Some((I::Flag, th::GOLD)), |ui| match &view.lobby {
                    Some(l) => {
                        heading(ui, &l.settings.name);
                        if !l.open {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(16.0).color(menus::ACCENT));
                                ui.label(RichText::new("Starting your lobby\u{2026} (Steam can take a few seconds)").color(th::BODY));
                            });
                        } else {
                            eyebrow(ui, I::Key, th::GOLD, "Room code");
                            ui.horizontal(|ui| {
                                room_code(ui, &redex_proto::room::shown(&l.code));
                                ui.add_space(6.0);
                                let r = glass_button(ui, "Copy", Some((I::Copy, th::SLATE)), Kind::Secondary);
                                if focused == Item::Copy {
                                    focus_ring(ui, r.rect);
                                }
                                if r.clicked() {
                                    acted = Some(Item::Copy);
                                }
                            });
                            let access = ACCESS.iter().find(|a| a.0 == l.settings.access).map_or("", |a| a.1);
                            ui.horizontal(|ui| {
                                fact(ui, I::Friends, th::VIOLET, access, None);
                                fact(ui, I::Group, th::AZURE, &format!("{} of {}", l.members.len() + 1, l.settings.max), Some("players"));
                            });
                        }
                        ui.add_space(2.0);
                        separator(ui);
                        eyebrow(ui, I::Group, th::AZURE, "Players");
                        if l.members.is_empty() {
                            ui.label(RichText::new("No one has joined yet. Invite friends from the Friends tab, or give them the code.").color(th::BODY));
                        }
                        for (i, (session, name)) in l.members.iter().enumerate() {
                            let asking = self.lb.confirm == Some(*session);
                            if member_row(ui, ("lobby member", i), focused == Item::Member(i), name, asking).clicked() {
                                acted = Some(Item::Member(i));
                            }
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let x = glass_button(ui, "Close lobby", Some((I::DoorOut, th::CORAL)), Kind::Danger);
                            if focused == Item::CloseLobby {
                                focus_ring(ui, x.rect);
                            }
                            if x.clicked() {
                                acted = Some(Item::CloseLobby);
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let c = glass_button(ui, "Close", Some((I::Close, th::SLATE)), Kind::Secondary);
                                if focused == Item::Close {
                                    focus_ring(ui, c.rect);
                                }
                                if c.clicked() {
                                    acted = Some(Item::Close);
                                }
                            });
                        });
                        hint(ui, "Remove (press twice) keeps a player out until the lobby closes; another Steam account could still come in unless the lobby is Friends only \u{b7} Close lobby tells everyone and stops it");
                    }
                    None => {
                        heading(ui, "Host a lobby");
                        ui.add(egui::TextEdit::singleline(&mut self.lb.form.name).char_limit(32).hint_text("Lobby name (your name's lobby)").desired_width(f32::INFINITY));
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Players").font(th::heavy(ui.ctx(), th::LABEL - 1.0)));
                            if glass_button(ui, "\u{2212}", None, Kind::Secondary).clicked() {
                                self.lb.form.max = self.lb.form.max.saturating_sub(1).max(2);
                            }
                            let r = ui.label(RichText::new(self.lb.form.max.to_string()).font(th::heavy(ui.ctx(), th::H1)).color(th::GOLD));
                            if focused == Item::Players {
                                focus_ring(ui, r.rect.expand(4.0));
                            }
                            if glass_button(ui, "+", None, Kind::Secondary).clicked() {
                                self.lb.form.max = (self.lb.form.max + 1).min(8);
                            }
                            ui.label(RichText::new("2 to 8, you included").size(th::CAPTION).color(th::MUTED));
                        });
                        for (i, (access, name, _)) in ACCESS.iter().enumerate() {
                            if radio(ui, ("lobby access", i), self.lb.form.access == *access, focused == Item::Access(i), name).clicked() {
                                acted = Some(Item::Access(i));
                            }
                        }
                        // what the choice means, wrapped (Unlisted's warning that codes are not secret among it)
                        if let Some((_, _, detail)) = ACCESS.iter().find(|a| a.0 == self.lb.form.access) {
                            ui.label(RichText::new(*detail).size(th::CAPTION).color(th::BODY));
                        }
                        ui.label(RichText::new(LOBBY_RULES).size(th::CAPTION).color(th::AMBER));
                        if let Some(why) = &view.lobby_blocked {
                            ui.label(RichText::new(why).size(th::CAPTION).color(th::CORAL));
                        }
                        ui.horizontal(|ui| {
                            let r = ui.add_enabled_ui(view.lobby_blocked.is_none(), |ui| glass_button(ui, "Host lobby", Some((I::Flag, th::GOLD)), Kind::Primary)).inner;
                            if focused == Item::Host {
                                focus_ring(ui, r.rect);
                            }
                            if r.clicked() {
                                acted = Some(Item::Host);
                            }
                        });
                        separator(ui);
                        eyebrow(ui, I::Key, th::GOLD, "Join with a room code");
                        ui.horizontal(|ui| {
                            let field = ui.add(egui::TextEdit::singleline(&mut self.lb.code).char_limit(9).hint_text("K7M-Q2X").desired_width(ui.available_width() - 100.0));
                            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                acted = Some(Item::Join);
                            }
                            let r = glass_button(ui, "Join", Some((I::Enter, th::MINT)), Kind::Secondary);
                            if focused == Item::Join {
                                focus_ring(ui, r.rect);
                            }
                            if r.clicked() {
                                acted = Some(Item::Join);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let c = glass_button(ui, "Close", Some((I::Close, th::SLATE)), Kind::Secondary);
                                if focused == Item::Close {
                                    focus_ring(ui, c.rect);
                                }
                                if c.clicked() {
                                    acted = Some(Item::Close);
                                }
                                if view.world.is_some() {
                                    let r = glass_button(ui, "Leave this world", Some((I::DoorOut, th::CORAL)), Kind::Danger);
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

/// the panel's title, clear of the icon on its corner
fn heading(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(30.0);
        ui.label(RichText::new(text).font(crate::theme::heavy(ui.ctx(), crate::theme::H1 + 2.0)).color(crate::theme::TITLE));
    });
}

/// a small section name in heavy caps after its icon
fn eyebrow(ui: &mut egui::Ui, icon: crate::icons::Icon, hue: Color32, text: &str) {
    let font = crate::theme::heavy(ui.ctx(), crate::theme::EYEBROW);
    let w = ui.painter().layout_no_wrap(text.to_uppercase(), font.clone(), crate::theme::BODY).size().x;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w + 24.0, 18.0), Sense::hover());
    crate::icons::draw(ui.painter(), icon, Pos2::new(rect.min.x + 8.0, rect.center().y), 17.0, hue, -8.0);
    ui.painter().text(Pos2::new(rect.min.x + 22.0, rect.center().y), Align2::LEFT_CENTER, text.to_uppercase(), font, crate::theme::BODY);
}

/// the room code as keycaps, one glass key a character, the dash between its halves
fn room_code(ui: &mut egui::Ui, code: &str) {
    let font = crate::theme::heavy(ui.ctx(), 24.0);
    for ch in code.chars() {
        if ch == '-' {
            let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 44.0), Sense::hover());
            ui.painter().line_segment([Pos2::new(r.min.x + 1.0, r.center().y), Pos2::new(r.max.x - 1.0, r.center().y)], Stroke::new(2.5, crate::theme::MUTED));
            continue;
        }
        let (r, _) = ui.allocate_exact_size(Vec2::new(34.0, 44.0), Sense::hover());
        glass::face(ui.painter(), r, 1.0, 0.0, glass::Kind::Secondary, true);
        glass::shadowed(ui.painter(), r.center() - Vec2::new(0.0, 1.5), Align2::CENTER_CENTER, &ch.to_string(), font.clone(), crate::theme::GOLD);
    }
}

/// a fact with its icon: a value (gold) and what it counts
fn fact(ui: &mut egui::Ui, icon: crate::icons::Icon, hue: Color32, value: &str, unit: Option<&str>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let (r, _) = ui.allocate_exact_size(Vec2::new(18.0, 18.0), Sense::hover());
        crate::icons::draw(ui.painter(), icon, r.center(), 18.0, hue, -8.0);
        match unit {
            Some(u) => {
                ui.label(RichText::new(value).font(crate::theme::heavy(ui.ctx(), crate::theme::BODY_SIZE)).color(crate::theme::GOLD));
                ui.label(RichText::new(u).size(crate::theme::BODY_SIZE).color(crate::theme::BODY));
            }
            None => {
                ui.label(RichText::new(value).size(crate::theme::BODY_SIZE).color(crate::theme::BODY));
            }
        }
    });
    ui.add_space(8.0);
}

fn separator(ui: &mut egui::Ui) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(r.x_range(), r.center().y, Stroke::new(1.0, Color32::from_white_alpha(26)));
}

/// a player in the lobby: their mark, their name, and Remove (coral; lit, asking again, after a first press)
fn member_row(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, focused: bool, name: &str, asking: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    let id = Id::new(id);
    let motion = crate::animations();
    let hover = ui.ctx().animate_bool_with_time(id.with("hover"), response.hovered(), if motion { crate::theme::HOVER } else { 0.0 });
    let lit = ui.ctx().animate_bool_with_time(id.with("focus"), focused, if motion { 0.14 } else { 0.0 });
    ui.painter().rect_filled(rect, 12.0, Color32::from_white_alpha((10.0 + 14.0 * hover.max(lit)) as u8));
    if lit > 0.0 {
        let bar = Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 9.0), Vec2::new(3.0 * lit, rect.height() - 18.0));
        ui.painter().rect_filled(bar, 2.0, crate::theme::BRAND_HI);
    }
    let c = Pos2::new(rect.min.x + 24.0, rect.center().y);
    ui.painter().circle_filled(c, 15.0, Color32::from_white_alpha(16));
    crate::icons::draw(ui.painter(), crate::icons::Icon::Person, c, 20.0, crate::theme::AZURE, 0.0);
    glass::shadowed(ui.painter(), Pos2::new(rect.min.x + 48.0, rect.center().y), Align2::LEFT_CENTER, name, crate::theme::heavy(ui.ctx(), crate::theme::LABEL), crate::theme::TITLE);
    // Remove, as a coral chip: a ghost until asked, then filled, saying press again
    let label = if asking { "Press again to remove" } else { "Remove" };
    let font = crate::theme::heavy(ui.ctx(), crate::theme::CAPTION);
    let w = ui.painter().layout_no_wrap(label.to_owned(), font.clone(), crate::theme::CORAL).size().x + 34.0;
    let chip = Rect::from_min_size(Pos2::new(rect.max.x - 8.0 - w, rect.center().y - 13.0), Vec2::new(w, 26.0));
    ui.painter().rect_filled(chip, 13.0, crate::theme::CORAL.gamma_multiply(if asking { 0.55 } else { 0.10 + 0.10 * hover }));
    ui.painter().rect_stroke(chip, 13.0, Stroke::new(1.0, crate::theme::CORAL.gamma_multiply(0.7)), egui::StrokeKind::Inside);
    crate::icons::draw(ui.painter(), crate::icons::Icon::PersonMinus, Pos2::new(chip.min.x + 14.0, chip.center().y), 16.0, crate::theme::CORAL, 0.0);
    ui.painter().text(Pos2::new(chip.min.x + 26.0, chip.center().y), Align2::LEFT_CENTER, label, font, if asking { crate::theme::TITLE } else { crate::theme::CORAL });
    response
}

#[cfg(test)]
pub(super) fn focus_of(view: &View, item: &str) -> Option<usize> {
    items(view).iter().position(|i| format!("{i:?}").starts_with(item))
}

