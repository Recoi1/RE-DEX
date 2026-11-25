//! The character creator: "New character" and "Edit look" open this glass panel over the start screen. Gender, skin,
//! hair style, hair colour, eyes and a name, with the game's own live preview of the look (a `redex --preview` child,
//! [`Session`]) that the player can turn.
use std::path::{Path, PathBuf};

use egui::epaint::{Mesh, Vertex};
use egui::{pos2, vec2, Align, Align2, Color32, ColorImage, FontId, Key, Layout, Rect, Sense, Shape, Stroke, TextureHandle, TextureOptions, Ui};
use xr_net::x::field::{Appearance, Facing};
use xr_net::x::look::{hair_styles, Look, EYES, HAIR_COLOURS, SKINS};

use super::characters::{check_name, now_ms, Store};
use super::glass::{self, Glass, Icon, BODY, BRAND_RED, TITLE};
use super::library::{GameEntry, Library};
use super::preview::{self, keep_card, preview_game, Frame, Session};
use zakuro_gpu::studio::ZOOM_MAX;
use super::{at, bold, text, CARD_BODY, RED};
use redex_proto::MAX_CHARACTERS;

/// The swatches' colours: the median of the skin, hair or iris pixels of the live preview's crop (the pixels that
/// change with that option), sampled once per value on the girl in her starting outfit (her long hair; the boy's,
/// under his hat, is the same colours in shadow) and pinned here. Skin 1 and 2 are that close in the game too.
const SKIN: [Color32; 3] = [Color32::from_rgb(0xEE, 0xDD, 0xD5), Color32::from_rgb(0xEE, 0xCD, 0xC5), Color32::from_rgb(0xC5, 0x9B, 0x83)];
const HAIR: [Color32; 5] = [
    Color32::from_rgb(0x30, 0x3B, 0x38),
    Color32::from_rgb(0x59, 0x44, 0x30),
    Color32::from_rgb(0xB4, 0x91, 0x62),
    Color32::from_rgb(0xC5, 0x7C, 0x51),
    Color32::from_rgb(0xDD, 0xCE, 0xA4),
];
/// natural, then the five contact colours (the iris pixels: those that differ from every other value's)
const EYE: [Color32; 6] = [
    Color32::from_rgb(0x72, 0x70, 0x7A),
    Color32::from_rgb(0x72, 0x4C, 0x30),
    Color32::from_rgb(0x83, 0x74, 0x41),
    Color32::from_rgb(0x49, 0x58, 0x62),
    Color32::from_rgb(0x28, 0x7C, 0x59),
    Color32::from_rgb(0x41, 0x78, 0x93),
];

/// the turns, in the order "›" steps through them: front, the figure's left side, back, its right side
const TURNS: [Facing; 4] = [Facing::Down, Facing::Right, Facing::Up, Facing::Left];
/// a horizontal drag this long on the preview is a quarter turn
const DRAG_TURN: f32 = 40.0;

/// what the controller asked this frame (the keyboard is read here)
#[derive(Default, Clone, Copy)]
pub struct Pad {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub turn_left: bool,
    pub turn_right: bool,
    pub confirm: bool,
    pub cancel: bool,
    /// the keyboard's + and - and 0: the studio view's zoom, and back to the front
    pub zoom_in: bool,
    pub zoom_out: bool,
    pub reset: bool,
}

// the rows, top to bottom; the footer (Save) after them
const NAME: usize = 0;
const GENDER: usize = 1;
const SKIN_ROW: usize = 2;
const STYLE: usize = 3;
const COLOUR: usize = 4;
const EYES_ROW: usize = 5;
const FOOTER: usize = 6;

pub struct Creator {
    /// the character whose look is edited, None for a new one
    pub editing: Option<u64>,
    pub name: String,
    pub look: Look,
    /// what the controller asked this frame; the caller sets it before `ui`
    pub pad: Pad,
    /// the clothes worn: the character's own when editing, else None (the gender's starting outfit)
    outfit: Option<[u8; 16]>,
    /// the newest look's seq; frames carry the seq of the look worn
    seq: u32,
    /// the frame on screen: the newest that came, never an older look's or the state's own (seq 0)
    shown: Option<Frame>,
    /// `shown` on the GPU, and whether it is behind; a studio frame's coverage alone (white), for its rim light
    tex: Option<TextureHandle>,
    rim: Option<TextureHandle>,
    fresh: bool,
    session: Option<Session>,
    /// the child (pid) and the seq it was sent, and the turn it was given
    sent: Option<(u32, u32)>,
    faced: Option<(u32, Turn)>,
    /// how the figure is turned: the studio view's camera, or the four facings when the child sends crops
    view: Facings,
    studio: Studio,
    /// whether the child sends studio frames (v2), None before its first frame; and frames since the orbit was sent
    v2: Option<bool>,
    since_orbit: u32,
    exe: PathBuf,
    /// the game the preview runs on, looked up once
    game: Option<Option<GameEntry>>,
    /// the preview could not run (logged); the creator still works
    broken: bool,
    /// the focused row, and whether to show it (the controller or arrows were used)
    row: usize,
    show_row: bool,
    /// the name field had the keyboard last frame
    typing: bool,
    error: Option<String>,
    /// a NEW character for the account (the Online tab was up when the creator opened); an edit never is
    pub online: bool,
}

pub enum CreatorAction {
    None,
    Saved(u64),
    /// a new online character: made in memory only, for the account (characters.json never holds it)
    SavedOnline(redex_proto::Character),
    Cancel,
}

impl Creator {
    pub fn new(store: &Store, editing: Option<u64>) -> Creator {
        let own = editing.and_then(|id| store.get(id)).and_then(|c| Some((c.id, c.name.clone(), Appearance::from_bytes(&c.look)?)));
        let (editing, name, look, outfit) = match own {
            Some((id, name, a)) => (Some(id), name, Look::of(&a), Some(a.fashion)),
            None => (None, String::new(), Look::default_for(0), None),
        };
        Creator {
            editing,
            name,
            look,
            pad: Pad::default(),
            outfit,
            seq: 1,
            shown: None,
            tex: None,
            rim: None,
            fresh: false,
            session: None,
            sent: None,
            faced: None,
            view: Facings::default(),
            studio: Studio::default(),
            online: false,
            v2: None,
            since_orbit: 0,
            exe: std::env::current_exe().unwrap_or_default(),
            game: None,
            broken: false,
            row: if editing.is_some() { STYLE } else { NAME },
            show_row: false,
            typing: false,
            error: None,
        }
    }

    /// the look in the clothes worn: the character's own when editing, else the starting outfit
    fn dress(&self, look: Look) -> Option<Appearance> {
        match self.outfit {
            Some(o) => look.wearing(o),
            None => look.appearance(),
        }
    }

    /// The look to send (its new seq), or None when it is invalid, unchanged, or changes what an edit can't.
    pub fn set(&mut self, look: Look) -> Option<(u32, Appearance)> {
        let locked = self.editing.is_some() && (look.gender != self.look.gender || look.skin != self.look.skin);
        if look == self.look || locked {
            return None;
        }
        let a = self.dress(look)?;
        (self.look, self.seq) = (look, self.seq + 1);
        Some((self.seq, a))
    }

    /// The running child (its gender, None when there is none) can't show this look.
    pub fn needs_restart(&self, child_gender: Option<u8>) -> bool {
        child_gender != Some(self.look.gender)
    }

    /// The turn the child should be given now.
    fn wanted(&self) -> Turn {
        match self.v2 {
            None => Turn::Wait,
            Some(false) => Turn::Face(self.view.index()),
            // the child's yaw turns the front to the left; a turn to the right (`›`, a drag right) shows the figure's
            // left side, as the facings do
            Some(true) => Turn::Orbit((-self.studio.orbit().0).rem_euclid(360.0) + 0.0, self.studio.orbit().1),
        }
    }

    /// What the stage turns: the studio view once the child sends its frames, else the four facings.
    fn source(&mut self) -> &mut dyn FrameSource {
        if self.v2 == Some(true) { &mut self.studio } else { &mut self.view }
    }

    /// The frame on screen shows the front: the first facing, or the studio at rest a few frames after its camera was.
    fn front(&self) -> bool {
        match self.v2 {
            Some(true) => self.studio.at_rest() && self.since_orbit >= 3,
            _ => self.view.index() == 0,
        }
    }

    pub fn on_frame(&mut self, f: Frame) {
        // what the child sends: any frame says (the studio falls back to crops when it finds no player)
        self.v2 = Some(f.floor.is_some());
        self.since_orbit = self.since_orbit.saturating_add(1);
        // seq 0 is the state's own outfit (a new child before our look is on); an older look never comes back
        let newer = f.seq != 0 && self.shown.as_ref().is_none_or(|s| f.seq >= s.seq);
        if newer && !f.rgba.is_empty() {
            (self.shown, self.fresh) = (Some(f), true);
        }
    }

    /// The frame on screen is an older look's: the newest is still being put on.
    pub fn dressing(&self) -> bool {
        self.shown.as_ref().is_some_and(|f| f.seq < self.seq)
    }

    /// Creates the character or sets its look, saves the list, then the card picture when the frame on screen is this
    /// look facing the front. The caller loads `store` with `Store::try_load` first.
    pub fn save(&self, store: &mut Store, data: &Path, now: u64) -> Result<u64, String> {
        let a = self.dress(self.look).ok_or("That look is out of the game's ranges.")?;
        let id = match self.editing {
            Some(id) if store.set_look(id, a, now) => id,
            Some(_) => return Err("That character isn't there any more.".into()),
            None if store.characters.len() >= MAX_CHARACTERS => return Err("You already have 8 characters, the most there can be.".into()),
            None => store.create(&self.name, a, now)?,
        };
        store.save(data).map_err(|e| format!("We couldn't save your character: {e}"))?;
        if let (Some(f), Some(c), true) = (self.shown.as_ref().filter(|f| f.seq == self.seq), store.get(id), self.front()) {
            // without it the start screen draws the card itself
            if let Err(e) = keep_card(data, c, f) {
                log::warn!("creator: {e}");
            }
        }
        Ok(id)
    }

    /// Starts the preview child for the look's gender, sends it the turn and the newest look, takes its frames.
    fn pump(&mut self, data: &Path, lib: &Library) {
        let Some(game) = self.game.get_or_insert_with(|| preview_game(lib, data).cloned()).clone() else { return };
        if self.needs_restart(self.session.as_ref().map(Session::gender)) {
            self.session = None; // the other gender's child goes first
            (self.session, self.broken) = (Some(Session::start(&game, data, self.look.gender)), false);
        }
        let (want, turn) = (self.dress(self.look), self.wanted());
        let child = match self.session.as_mut().map(|s| s.pump(&self.exe, &game, data)) {
            Some(Ok(Some(child))) => child,
            Some(Err(e)) => {
                if !self.broken {
                    log::warn!("creator: no preview: {e}");
                }
                self.broken = true;
                return;
            }
            _ => return,
        };
        let pid = child.pid();
        // a child that is gone says so on the next pump
        if self.faced != Some((pid, turn)) {
            let sent = match turn {
                Turn::Wait => Err(String::new()),
                Turn::Face(i) => child.face(TURNS[i]),
                Turn::Orbit(yaw, zoom) => child.orbit(yaw, zoom).map(|()| self.since_orbit = 0),
            };
            if sent.is_ok() {
                self.faced = Some((pid, turn));
            }
        }
        if self.sent != Some((pid, self.seq)) {
            if let Some(a) = want {
                if child.look(self.seq, &a).is_ok() {
                    self.sent = Some((pid, self.seq));
                }
            }
        }
        if let Some(f) = child.latest() {
            self.on_frame(f);
        }
    }

    /// whether a row can change: an edit keeps the gender, the skin and the name
    fn open(&self, row: usize) -> bool {
        self.editing.is_none() || !matches!(row, NAME | GENDER | SKIN_ROW)
    }

    /// the focused row's value one step left (-1) or right (+1)
    fn step(&mut self, row: usize, d: i32) {
        let l = self.look;
        let clamp = |v: u8, n: u8| (v as i32 + d).clamp(0, n as i32 - 1) as u8;
        let next = match row {
            GENDER => Some(gender(l, if d > 0 { 1 } else { 0 })),
            SKIN_ROW => {
                let i = SKINS.iter().position(|&s| s == l.skin).unwrap_or(0) as u8;
                Some(Look { skin: SKINS[clamp(i, SKINS.len() as u8) as usize], ..l })
            }
            STYLE => {
                let n = hair_styles(l.gender) as i32;
                Some(Look { hair_style: (l.hair_style as i32 + d).rem_euclid(n) as u8, ..l })
            }
            COLOUR => Some(Look { hair_colour: clamp(l.hair_colour, HAIR_COLOURS), ..l }),
            EYES_ROW => Some(Look { eyes: clamp(l.eyes, EYES), ..l }),
            _ => None,
        };
        if let Some(n) = next {
            self.set(n);
        }
    }

    fn can_save(&self) -> bool {
        self.editing.is_some() || check_name(&self.name).is_ok()
    }

    /// Saves (the list read again first: another RE:DEX may have changed it); the action once saved, else the error is
    /// shown. A new online character is made in memory, with an id no character on this PC has, and is not saved here.
    fn try_save(&mut self, data: &Path) -> Option<CreatorAction> {
        let Some(mut store) = Store::try_load(data) else {
            self.error = Some("We couldn't open your characters just now. Close anything else using RE:DEX, then try again.".into());
            return None;
        };
        if self.online && self.editing.is_none() {
            return self.make_online(&mut store, data, now_ms()).map(CreatorAction::SavedOnline).map_err(|e| self.error = Some(e)).ok();
        }
        self.save(&mut store, data, now_ms()).map(CreatorAction::Saved).map_err(|e| self.error = Some(e)).ok()
    }

    /// A new character for the account: made in `store` (so its id is no local character's) and taken out again,
    /// characters.json untouched; its card picture is kept as for any character.
    fn make_online(&self, store: &mut Store, data: &Path, now: u64) -> Result<redex_proto::Character, String> {
        let a = self.dress(self.look).ok_or("That look is out of the game's ranges.")?;
        let id = store.create(&self.name, a, now)?;
        let c = store.get(id).cloned().ok_or("the new character went missing")?;
        if let (Some(f), true) = (self.shown.as_ref().filter(|f| f.seq == self.seq), self.front()) {
            if let Err(e) = keep_card(data, &c, f) {
                log::warn!("creator: {e}");
            }
        }
        Ok(c)
    }

    /// Types `name` and presses Create (the start screen's tests).
    #[cfg(test)]
    pub(super) fn create_now(&mut self, data: &Path, name: &str) -> Option<CreatorAction> {
        self.name = name.into();
        self.try_save(data)
    }

    pub fn ui(&mut self, ui: &mut Ui, glass: &Glass, data: &Path, lib: &Library) -> CreatorAction {
        self.pump(data, lib);
        let panel = Rect::from_min_max(pos2(32.0, 104.0), pos2(1008.0, 608.0));
        glass.paint(ui.painter(), panel, crate::theme::R_PANEL, glass::Look { dot: true, lift: 1.0, seed: 4, ..Default::default() });

        // the keyboard moves like the d-pad unless the name field has it
        let mut pad = std::mem::take(&mut self.pad);
        if !self.typing {
            ui.input(|i| {
                pad.up |= i.key_pressed(Key::ArrowUp);
                pad.down |= i.key_pressed(Key::ArrowDown);
                pad.left |= i.key_pressed(Key::ArrowLeft);
                pad.right |= i.key_pressed(Key::ArrowRight);
                pad.confirm |= i.key_pressed(Key::Enter);
                pad.cancel |= i.key_pressed(Key::Escape);
                pad.zoom_in |= i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals);
                pad.zoom_out |= i.key_pressed(Key::Minus);
                pad.reset |= i.key_pressed(Key::Num0);
            });
        }
        if pad.cancel {
            return CreatorAction::Cancel;
        }
        let rows: Vec<usize> = (NAME..=FOOTER).filter(|&r| self.open(r)).collect();
        let at_row = rows.iter().position(|&r| r == self.row).unwrap_or(0);
        if pad.up || pad.down {
            let i = if pad.up { at_row.saturating_sub(1) } else { (at_row + 1).min(rows.len() - 1) };
            (self.row, self.show_row) = (rows[i], true);
        }
        if pad.left || pad.right {
            self.show_row = true;
            self.step(self.row, if pad.left { -1 } else { 1 });
        }
        if pad.turn_left || pad.turn_right {
            self.source().turn(if pad.turn_left { -90.0 } else { 90.0 });
        }
        if pad.zoom_in || pad.zoom_out {
            self.source().zoom(if pad.zoom_in { 1.25 } else { 0.8 });
        }
        if pad.reset {
            self.source().reset();
        }

        self.stage(ui, glass);

        // the right column: the title, then a 56 px row each
        let (label_x, x) = (344.0, 472.0);
        let title = if self.editing.is_some() { "Edit look" } else { "New character" };
        let icon = if self.editing.is_some() { crate::icons::Icon::Shirt } else { crate::icons::Icon::NewPerson };
        crate::icons::draw(ui.painter(), icon, pos2(label_x + 10.0, 144.0), 24.0, crate::theme::PINK, -8.0);
        glass.title(ui.painter(), pos2(label_x + 30.0, 136.0), title, crate::theme::EYEBROW + 1.5);
        let cy = |row: usize| 188.0 + 56.0 * row as f32;
        let mut clicked_row = None;
        for row in NAME..FOOTER {
            if self.show_row && self.row == row {
                let r = Rect::from_min_max(pos2(label_x - 16.0, cy(row) - 24.0), pos2(976.0, cy(row) + 24.0));
                ui.painter().rect_filled(r, 16.0, Color32::from_white_alpha(16));
                ui.painter().add(Shape::mesh(glass::rim_mesh(r, 16.0, 0.4)));
                ui.painter().rect_filled(Rect::from_min_size(r.min + vec2(0.0, 12.0), vec2(3.0, r.height() - 24.0)), 2.0, crate::theme::BRAND_HI);
            }
            use crate::icons::Icon as I;
            let label = ["Name", "Gender", "Skin", "Hair style", "Hair colour", "Eyes"][row];
            let (icon, hue) = [(I::Pen, crate::theme::GOLD), (I::Person, crate::theme::AZURE), (I::Drop, crate::theme::AMBER), (I::Comb, crate::theme::VIOLET), (I::Palette, crate::theme::PINK), (I::Eye, crate::theme::TEAL)][row];
            let open = self.open(row);
            let tone = if open { CARD_BODY } else { dim(CARD_BODY) };
            crate::icons::draw_alpha(ui.painter(), icon, pos2(label_x + 9.0, cy(row)), 19.0, hue, -8.0, if open { 1.0 } else { 0.45 });
            ui.painter().text(pos2(label_x + 26.0, cy(row)), Align2::LEFT_CENTER, label, crate::theme::heavy(ui.ctx(), crate::theme::CAPTION + 1.0), tone);
        }

        // name
        let field = Rect::from_min_size(pos2(x, cy(NAME) - 20.0), vec2(280.0, 40.0));
        if self.editing.is_some() {
            bold(ui, pos2(x, cy(NAME)), &self.name, 16.0, TITLE);
        } else {
            glass::well(ui.painter(), field, self.typing);
            let edit = at(ui, field.shrink2(vec2(16.0, 8.0)), Layout::left_to_right(Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.name)
                        .frame(egui::Frame::NONE)
                        .desired_width(248.0)
                        .font(FontId::proportional(16.0))
                        .text_color(TITLE)
                        .hint_text(egui::RichText::new("Your trainer's name").color(BODY))
                        .char_limit(12),
                )
            });
            if pad.confirm && self.row == NAME {
                edit.request_focus();
            }
            if edit.clicked() {
                (self.row, self.show_row) = (NAME, false);
            }
            self.typing = edit.has_focus();
            let (rule, tone) = match check_name(&self.name) {
                Err(e) if !self.name.trim().is_empty() => (e, RED),
                _ => ("Up to 12 letters".to_owned(), CARD_BODY),
            };
            at(ui, Rect::from_min_size(pos2(x + 280.0 + 16.0, cy(NAME) - 20.0), vec2(976.0 - x - 296.0, 40.0)), Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(egui::RichText::new(rule).size(13.0).color(tone)).wrap());
            });
        }

        // gender: two pills
        for (i, s) in ["Boy", "Girl"].into_iter().enumerate() {
            let r = Rect::from_min_size(pos2(x + i as f32 * 112.0, cy(GENDER) - 20.0), vec2(104.0, 40.0));
            if self.choice(ui, glass, r, ("gender", i), s, self.look.gender == i as u8, self.open(GENDER)) {
                self.set(gender(self.look, i as u8));
                clicked_row = Some(GENDER);
            }
        }
        if !self.open(GENDER) {
            text(ui, pos2(x + 232.0, cy(GENDER)), Align2::LEFT_CENTER, "Set when the character was made", 13.0, CARD_BODY);
        }
        // skin, hair colour, eyes: swatches
        let picked = self.swatches(ui, glass, x, cy(SKIN_ROW), ("skin", 0), &SKIN, SKINS.iter().position(|&s| s == self.look.skin), self.open(SKIN_ROW));
        if let Some(i) = picked {
            self.set(Look { skin: SKINS[i], ..self.look });
            clicked_row = Some(SKIN_ROW);
        }
        if let Some(i) = self.swatches(ui, glass, x, cy(COLOUR), ("hair", 0), &HAIR, Some(self.look.hair_colour as usize), true) {
            self.set(Look { hair_colour: i as u8, ..self.look });
            clicked_row = Some(COLOUR);
        }
        if let Some(i) = self.swatches(ui, glass, x, cy(EYES_ROW), ("eyes", 0), &EYE, Some(self.look.eyes as usize), true) {
            self.set(Look { eyes: i as u8, ..self.look });
            clicked_row = Some(EYES_ROW);
        }
        let eyes = if self.look.eyes == 0 { "Natural" } else { "Coloured contacts" };
        text(ui, pos2(x + 6.0 * 48.0 + 8.0, cy(EYES_ROW)), Align2::LEFT_CENTER, eyes, 13.0, CARD_BODY);
        // hair style: ‹ n/N ›
        let n = hair_styles(self.look.gender);
        for (i, (icon, dx, d)) in [(Icon::Prev, 0.0, -1), (Icon::Next, 112.0, 1)].into_iter().enumerate() {
            let r = Rect::from_min_size(pos2(x + dx, cy(STYLE) - 16.0), vec2(32.0, 32.0));
            let resp = ui.interact(r, ui.id().with(("style", i)), Sense::click());
            let (hover, scale) = glass::press_scale(ui, &resp, 1.06);
            glass.button_face(ui.painter(), r, scale, hover, glass::Kind::Secondary, true);
            glass::draw_icon(ui.painter(), icon, r.center(), 6.5 * scale);
            if resp.clicked() {
                self.step(STYLE, d);
                clicked_row = Some(STYLE);
            }
        }
        text(ui, pos2(x + 72.0, cy(STYLE)), Align2::CENTER_CENTER, &format!("{} / {n}", self.look.hair_style + 1), 16.0, TITLE);

        // the footer: what is worn, then Cancel and the one primary action
        let fy = 552.0;
        let note = match (&self.error, self.editing) {
            (Some(e), _) => (e.as_str(), RED),
            (None, None) => ("Starting outfit · new clothes come from Kalos boutiques", CARD_BODY),
            (None, Some(_)) => ("Your own clothes stay on · change them in Kalos boutiques", CARD_BODY),
        };
        at(ui, Rect::from_min_max(pos2(label_x, fy - 20.0), pos2(696.0, fy + 20.0)), Layout::left_to_right(Align::Center), |ui| {
            ui.add(egui::Label::new(egui::RichText::new(note.0).size(13.0).color(note.1)).wrap());
        });
        let cancel = Rect::from_min_size(pos2(712.0, fy - 20.0), vec2(104.0, 40.0));
        let resp = ui.interact(cancel, ui.id().with("cancel"), Sense::click());
        let (hover, scale) = glass::press_scale(ui, &resp, 1.04);
        glass.button_face(ui.painter(), cancel, scale, hover, glass::Kind::Secondary, true);
        crate::icons::draw(ui.painter(), crate::icons::Icon::Close, cancel.center() - vec2(30.0, 0.0), 16.0 * scale, crate::theme::SLATE, 0.0);
        glass::shadowed(ui.painter(), cancel.center() + vec2(8.0, 0.0), Align2::CENTER_CENTER, "Cancel", crate::theme::heavy(ui.ctx(), crate::theme::LABEL), TITLE);
        if resp.clicked() {
            return CreatorAction::Cancel;
        }
        let save = Rect::from_min_size(pos2(832.0, fy - 24.0), vec2(144.0, 48.0));
        let ok = self.can_save();
        let resp = ui.interact(save, ui.id().with("save"), if ok { Sense::click() } else { Sense::hover() });
        let (hover, scale) = if ok { glass::press_scale(ui, &resp, 1.04) } else { (0.0, 1.0) };
        if ok {
            glass::glow(ui.painter(), save, 24.0, BRAND_RED, 1.2 + 0.6 * hover);
        }
        glass.button_face(ui.painter(), save, scale, hover, if ok { glass::Kind::Primary } else { glass::Kind::Secondary }, ok);
        // still readable while it waits for a name
        let (word, tone) = (if self.editing.is_some() { "Save" } else { "Create" }, if ok { TITLE } else { dim(CARD_BODY) });
        let font = crate::theme::heavy(ui.ctx(), crate::theme::H2);
        let g = ui.painter().layout_no_wrap(word.to_owned(), font.clone(), tone);
        let x = save.center().x - (g.size().x + 26.0) / 2.0;
        crate::icons::draw_alpha(ui.painter(), crate::icons::Icon::Check, pos2(x + 9.0, save.center().y), 20.0 * scale, Color32::WHITE, -8.0, if ok { 1.0 } else { 0.5 });
        glass::shadowed(ui.painter(), pos2(x + 26.0, save.center().y), Align2::LEFT_CENTER, word, font, tone);
        if self.show_row && self.row == FOOTER {
            glass::focus_ring(ui.painter(), save, save.height() / 2.0, ui.input(|i| i.time));
        }
        if let Some(r) = clicked_row {
            (self.row, self.show_row) = (r, false);
        }
        let confirm = pad.confirm && !self.typing && self.row != NAME;
        if ok && (resp.clicked() || confirm) {
            if let Some(action) = self.try_save(data) {
                return action;
            }
        }
        CreatorAction::None
    }

    /// a pill that is one of a set: brighter with a rim when it is the one picked; true when clicked
    fn choice(&self, ui: &mut Ui, glass: &Glass, r: Rect, id: (&str, usize), s: &str, on: bool, open: bool) -> bool {
        let resp = ui.interact(r, ui.id().with(id), if open { Sense::click() } else { Sense::hover() });
        let (hover, scale) = if open { glass::press_scale(ui, &resp, 1.04) } else { (0.0, 1.0) };
        let kind = if on { glass::Kind::Tint(crate::theme::BRAND) } else { glass::Kind::Secondary };
        glass.button_face(ui.painter(), r, scale, hover, kind, open || on);
        glass::shadowed(ui.painter(), r.center(), Align2::CENTER_CENTER, s, crate::theme::heavy(ui.ctx(), crate::theme::LABEL), if open || on { TITLE } else { dim(TITLE) });
        resp.clicked()
    }

    /// a row of 32 px swatches 16 apart from `x`; the index clicked
    #[allow(clippy::too_many_arguments)]
    fn swatches(&self, ui: &mut Ui, glass: &Glass, x: f32, cy: f32, id: (&str, usize), colours: &[Color32], on: Option<usize>, open: bool) -> Option<usize> {
        let mut hit = None;
        for (i, &c) in colours.iter().enumerate() {
            let centre = pos2(x + 16.0 + i as f32 * 48.0, cy);
            let r = Rect::from_center_size(centre, vec2(32.0, 32.0));
            let resp = ui.interact(r, ui.id().with((id.0, i)), if open { Sense::click() } else { Sense::hover() });
            let hover = if open { glass.hover_of(ui, &resp) } else { 0.0 };
            // locked, the one worn keeps its colour and the rest fade
            let k = if open || on == Some(i) { 1.0 } else { 0.35 };
            if on == Some(i) {
                ui.painter().circle_stroke(centre, 20.5, Stroke::new(2.5, crate::theme::BRAND_HI.gamma_multiply(k)));
            } else if hover > 0.0 {
                ui.painter().circle_stroke(centre, 20.0, Stroke::new(1.0, Color32::from_white_alpha((90.0 * hover) as u8)));
            }
            ui.painter().circle_filled(centre, 16.0, c.gamma_multiply(k));
            // a glass bead: a soft highlight top-left and a thin rim
            ui.painter().circle_filled(centre + vec2(-5.0, -6.0), 5.0, Color32::from_white_alpha((40.0 * k) as u8));
            ui.painter().circle_stroke(centre, 16.0, Stroke::new(1.0, Color32::from_white_alpha((70.0 * k) as u8)));
            if resp.clicked() {
                hit = Some(i);
            }
        }
        hit
    }

    /// the preview: its picture on the GPU, the stage around it, and what the preview is doing while there is none
    fn stage(&mut self, ui: &mut Ui, glass: &Glass) {
        if let Some(f) = &self.shown {
            if std::mem::take(&mut self.fresh) || self.tex.is_none() {
                let mut img = ColorImage::from_rgba_unmultiplied([f.width.into(), f.height.into()], &f.rgba);
                // a crop is the game's whole picture, drawn at whole pixels; a studio frame is the model alone, scaled
                let options = if f.floor.is_some() { TextureOptions::LINEAR } else { TextureOptions::NEAREST };
                if f.floor.is_none() {
                    img.pixels.iter_mut().for_each(|p| *p = Color32::from_rgb(p.r(), p.g(), p.b()));
                }
                if f.floor.is_some() {
                    let mask = ColorImage { size: img.size, source_size: img.source_size, pixels: img.pixels.iter().map(|p| Color32::from_white_alpha(p.a())).collect() };
                    match &mut self.rim {
                        Some(t) => t.set(mask, options),
                        None => self.rim = Some(ui.ctx().load_texture("creator-rim", mask, options)),
                    }
                }
                match &mut self.tex {
                    Some(t) => t.set(img, options),
                    None => self.tex = Some(ui.ctx().load_texture("creator-figure", img, options)),
                }
            }
        }
        let rim = self.rim.as_ref().map(TextureHandle::id);
        let picture = self.shown.as_ref().zip(self.tex.as_ref()).map(|(f, t)| Picture { tex: t.id(), size: vec2(f.width.into(), f.height.into()), floor: f.floor, rim });
        let busy = self.dressing().then_some("Dressing…");
        let stage = Rect::from_min_size(pos2(64.0, 136.0), vec2(248.0, 400.0));
        let room = preview_stage(ui, glass, stage, picture, busy, self.source());
        if picture.is_some() {
            return;
        }
        let c = room.center();
        silhouette(ui, Rect::from_center_size(c - vec2(0.0, 40.0), vec2(96.0, 160.0)));
        let making = self.session.as_ref().and_then(Session::making);
        let words = if self.game.as_ref().is_some_and(Option::is_none) {
            ("No preview yet", "Build Pokémon X or Y to see your character here")
        } else if self.broken {
            ("No preview this time", "You can still pick a look and save it")
        } else if making.is_some() {
            ("Getting the preview ready", "About a minute, first time only")
        } else {
            ("Starting the preview…", "")
        };
        text(ui, pos2(c.x, c.y + 64.0), Align2::CENTER_CENTER, words.0, 14.0, TITLE);
        at(ui, Rect::from_center_size(pos2(c.x, c.y + 92.0), vec2(room.width(), 32.0)), Layout::top_down(Align::Center), |ui| {
            ui.add(egui::Label::new(egui::RichText::new(words.1).size(13.0).color(CARD_BODY)).wrap());
        });
        if let Some(f) = making {
            at(ui, Rect::from_center_size(pos2(c.x, c.y + 128.0), vec2(room.width() - 16.0, 16.0)), Layout::top_down(Align::Center), |ui| {
                glass.progress(ui, Some(f), true);
            });
        }
    }
}

/// What the preview stage turns and zooms: the studio view ([`Studio`], only the player's model drawn with our own
/// camera), or the game's own preview child standing at four facings ([`Facings`], when the child sends crops).
pub trait FrameSource {
    /// spin by `degrees`, positive clockwise seen from above (the arrows, L and R)
    fn turn(&mut self, degrees: f32);
    /// zoom by `factor`, above 1 is closer
    fn zoom(&mut self, factor: f32);
    /// a horizontal drag of `dx` points at `now` seconds: a quarter turn per DRAG_TURN points unless the source spins
    /// freely
    fn drag(&mut self, dx: f32, _now: f64) {
        self.turn(dx * 90.0 / DRAG_TURN);
    }
    /// the drag ended
    fn release(&mut self, _now: f64) {}
    /// back to the front and the whole body (a double-click)
    fn reset(&mut self) {}
    /// moves on to `now`; true while it still moves
    fn tick(&mut self, _now: f64) -> bool {
        false
    }
}

/// Degrees a point of drag spins the studio view (400 points a whole turn).
const SPIN: f32 = 0.9;
/// The ease-out's time constant: a coast or a turn is 95 % done in 0.21 s.
const TAU: f32 = 0.07;
/// A drag held still this long before it ends does not coast.
const STILL: f64 = 0.08;
/// The fastest coast, degrees a second.
const FASTEST: f32 = 1080.0;

/// The studio view's camera: a free spin that coasts on after a drag and eases out, and a zoom from the whole body
/// (1) to the face (`ZOOM_MAX`). The yaw grows without wrapping while it moves (so a reset turns back the short way).
#[derive(Debug, Clone)]
pub struct Studio {
    yaw: f32,
    to: f32,
    zoom: f32,
    zoom_to: f32,
    /// degrees a second while dragged, when the drag last moved and the last drag and tick
    speed: f32,
    moved: f64,
    dragged: Option<f64>,
    ticked: Option<f64>,
}

impl Default for Studio {
    fn default() -> Studio {
        Studio { yaw: 0.0, to: 0.0, zoom: 1.0, zoom_to: 1.0, speed: 0.0, moved: 0.0, dragged: None, ticked: None }
    }
}

impl Studio {
    /// the yaw in [0, 360) and the zoom to draw now
    pub fn orbit(&self) -> (f32, f32) {
        (self.yaw.rem_euclid(360.0), self.zoom)
    }

    /// at the front, the whole body, and still
    fn at_rest(&self) -> bool {
        self.orbit() == (0.0, 1.0) && self.to == self.yaw && self.zoom_to == self.zoom
    }
}

impl FrameSource for Studio {
    fn turn(&mut self, degrees: f32) {
        self.to += degrees;
    }

    fn zoom(&mut self, factor: f32) {
        self.zoom_to = (self.zoom_to * factor).clamp(1.0, ZOOM_MAX);
    }

    fn drag(&mut self, dx: f32, now: f64) {
        // a press that does not move (a click, a double-click's) leaves the turn under way alone
        if dx != 0.0 {
            let d = dx * SPIN;
            let dt = self.dragged.map_or(1.0 / 60.0, |t| (now - t) as f32).max(1.0 / 240.0);
            (self.speed, self.moved) = (0.5 * self.speed + 0.5 * d / dt, now);
            self.yaw += d;
            self.to = self.yaw;
        }
        self.dragged = Some(now);
    }

    fn release(&mut self, now: f64) {
        // the coast an exponential ease-out from the drag's speed covers; none after a drag held still
        if now - self.moved <= STILL {
            self.to = self.yaw + self.speed.clamp(-FASTEST, FASTEST) * TAU;
        }
        (self.speed, self.dragged) = (0.0, None);
    }

    fn reset(&mut self) {
        (self.to, self.zoom_to) = ((self.yaw / 360.0).round() * 360.0, 1.0);
    }

    fn tick(&mut self, now: f64) -> bool {
        let dt = self.ticked.map_or(0.0, |t| (now - t) as f32).clamp(0.0, 0.1);
        self.ticked = Some(now);
        let k = 1.0 - (-dt / TAU).exp();
        self.yaw += (self.to - self.yaw) * k;
        self.zoom += (self.zoom_to - self.zoom) * k;
        if (self.to - self.yaw).abs() < 0.05 {
            // there: whole turns are the front again
            self.yaw = self.to.rem_euclid(360.0);
            self.to = self.yaw;
        }
        if (self.zoom_to - self.zoom).abs() < 0.002 {
            self.zoom = self.zoom_to;
        }
        self.yaw != self.to || self.zoom != self.zoom_to
    }
}

/// The turn a child is given.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Turn {
    /// not yet: the child has not said whether it sends studio frames
    Wait,
    /// a facing, an index into `TURNS`
    Face(usize),
    /// the studio view's yaw and zoom
    Orbit(f32, f32),
}

/// The preview child's four facings: the angle asked for, snapped to the nearest 90°. Zoom is ignored.
#[derive(Default)]
pub struct Facings {
    angle: f32,
}

impl Facings {
    /// the facing shown, an index into `TURNS`
    fn index(&self) -> usize {
        (self.angle / 90.0).round() as usize % TURNS.len()
    }
}

impl FrameSource for Facings {
    fn turn(&mut self, degrees: f32) {
        self.angle = (self.angle + degrees).rem_euclid(360.0);
    }

    fn zoom(&mut self, _factor: f32) {}
}

/// A frame on the GPU for the stage: its size, and the floor shadow of a studio frame (None: a crop).
#[derive(Clone, Copy)]
pub struct Picture {
    pub tex: egui::TextureId,
    pub size: egui::Vec2,
    pub floor: Option<[i16; 4]>,
    /// a studio frame's coverage in white: drawn just around the model, a rim light that keeps dark shoes and hair off
    /// the dark backdrop
    pub rim: Option<egui::TextureId>,
}

/// The preview stage, the creator's swappable widget, on a neutral studio gradient with turn arrows under it:
/// - a studio frame: the model alone, standing in the stage on its floor shadow under a soft backdrop light;
/// - a crop: framed in glass at a whole-number scale.
///
/// A horizontal drag spins `source` (with a coast once let go), the wheel zooms it (the creator also reads + - and 0), a
/// double-click resets it, the
/// arrows turn it by 90°. `busy` is said under the picture with a sheen over it (the last picture is held meanwhile).
/// Returns the room for the picture, for the caller to fill while there is none.
pub fn preview_stage(ui: &mut Ui, glass: &Glass, stage: Rect, picture: Option<Picture>, busy: Option<&str>, source: &mut dyn FrameSource) -> Rect {
    let rgb = |c: [u8; 3]| Color32::from_rgb(c[0], c[1], c[2]);
    glass::gradient(ui.painter(), stage, 24.0, rgb(preview::STUDIO_TOP), rgb(preview::STUDIO_BOTTOM));
    ui.painter().add(Shape::mesh(glass::rim_mesh(stage, 24.0, 0.0)));
    let arrows_y = stage.max.y - 16.0 - 20.0;
    let room = Rect::from_min_max(stage.min + vec2(16.0, 16.0), pos2(stage.max.x - 16.0, arrows_y - 20.0 - 16.0));
    let now = ui.input(|i| i.time);
    let studio = picture.is_some_and(|p| p.floor.is_some());
    // the studio's model stands in the whole stage above the arrows
    let area = if studio { Rect::from_min_max(stage.min + vec2(0.0, 8.0), pos2(stage.max.x, arrows_y - 28.0)) } else { room };
    let resp = ui.interact(area, ui.id().with("stage"), Sense::click_and_drag());
    if resp.dragged() {
        source.drag(resp.drag_delta().x, now);
    }
    if resp.drag_stopped() {
        source.release(now);
    }
    if resp.double_clicked() {
        source.reset();
    }
    if resp.hovered() {
        // the wheel's own steps: the studio eases its zoom itself (egui's smoothed scroll would ease it twice, and lose
        // its tail once the pointer leaves)
        let notches: f32 = ui.input(|i| {
            i.events
                .iter()
                .map(|e| match e {
                    egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, .. } => delta.y / 50.0,
                    egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta, .. } => delta.y,
                    egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Page, delta, .. } => delta.y * 4.0,
                    _ => 0.0,
                })
                .sum()
        });
        if notches != 0.0 {
            source.zoom(1.25f32.powf(notches));
        }
    }
    if source.tick(now) {
        ui.ctx().request_repaint();
    }
    if picture.is_some() {
        resp.on_hover_cursor(if ui.input(|i| i.pointer.primary_down()) { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
    }
    match picture {
        Some(Picture { tex, size, floor: Some(e), rim }) => {
            // the frame's height fills the area; its sides, clear, may pass the stage's
            let k = area.height() / size.y;
            let pic = Rect::from_center_size(area.center(), size * k);
            // a soft light on the backdrop behind the upper body, then the floor shadow, then the model
            oval(&ui.painter().with_clip_rect(stage.shrink(1.0)), pos2(pic.center().x, pic.min.y + pic.height() * 0.4), vec2(stage.width() * 0.62, pic.height() * 0.42), |t| Color32::from_white_alpha((20.0 * (1.0 - t * t).powi(2)) as u8));
            let painter = ui.painter().with_clip_rect(stage.shrink(1.0).intersect(pic));
            let [cx, cy, rx, ry] = e.map(f32::from);
            if rx > 0.0 && ry > 0.0 {
                oval(&painter, pic.min + vec2(cx, cy) * k, vec2(rx, ry) * k, |t| Color32::from_black_alpha((255.0 * preview::SHADOW * (1.0 - t * t).powi(2)) as u8));
            }
            if let Some(rim) = rim {
                // the coverage nudged 1.5 points each way, faint: a soft light edge where the model meets the backdrop
                for k in 0..8 {
                    let a = k as f32 * std::f32::consts::TAU / 8.0;
                    let mut m = Mesh::with_texture(rim);
                    m.add_rect_with_uv(pic.translate(vec2(a.cos(), a.sin()) * 1.5), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::from_white_alpha(26));
                    painter.add(Shape::mesh(m));
                }
            }
            // the frame's last 12 points fade out: a model zoomed past the frame's bottom has no hard edge
            let fade = 12.0;
            let mut m = Mesh::with_texture(tex);
            let cut = pic.max.y - fade;
            let v = (cut - pic.min.y) / pic.height();
            m.add_rect_with_uv(Rect::from_min_max(pic.min, pos2(pic.max.x, cut)), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, v)), Color32::WHITE);
            for (pos, uv, c) in [
                (pos2(pic.min.x, cut), pos2(0.0, v), Color32::WHITE),
                (pos2(pic.max.x, cut), pos2(1.0, v), Color32::WHITE),
                (pic.left_bottom(), pos2(0.0, 1.0), Color32::TRANSPARENT),
                (pic.right_bottom(), pos2(1.0, 1.0), Color32::TRANSPARENT),
            ] {
                m.vertices.push(Vertex { pos, uv, color: c });
            }
            m.add_triangle(4, 5, 6);
            m.add_triangle(5, 7, 6);
            painter.add(Shape::mesh(m));
            if let Some(what) = busy {
                sheen(ui, pic.intersect(stage));
                text(ui, pos2(stage.center().x, arrows_y), Align2::CENTER_CENTER, what, 13.0, CARD_BODY);
            }
        }
        Some(Picture { tex, size, floor: None, .. }) => {
            // 2x when it fits, so every game pixel is a whole number of screen pixels
            let k = (room.width() / size.x).min(room.height() / size.y).floor().clamp(1.0, 2.0);
            let pic = Rect::from_center_size(pos2(room.center().x, room.min.y + size.y * k / 2.0), size * k);
            glass.picture(ui.painter(), pic, 16.0, tex, Color32::WHITE, false);
            ui.painter().add(Shape::mesh(glass::rim_mesh(pic, 16.0, 0.0)));
            if let Some(what) = busy {
                sheen(ui, pic);
                text(ui, pos2(stage.center().x, arrows_y), Align2::CENTER_CENTER, what, 13.0, CARD_BODY);
            }
        }
        None => {}
    }
    // nothing to turn until there is a picture
    let turns = if picture.is_some() { [(Icon::Prev, -88.0, -90.0), (Icon::Next, 88.0, 90.0)].as_slice() } else { &[] };
    for (i, &(icon, dx, d)) in turns.iter().enumerate() {
        let r = Rect::from_center_size(pos2(stage.center().x + dx, arrows_y), vec2(40.0, 40.0));
        let resp = ui.interact(r, ui.id().with(("turn", i)), Sense::click());
        let (hover, scale) = glass::press_scale(ui, &resp, 1.06);
        glass.button_face(ui.painter(), r, scale, hover, glass::Kind::Secondary, true);
        glass::draw_icon(ui.painter(), icon, r.center(), 7.5 * scale);
        if resp.clicked() {
            source.turn(d);
        }
    }
    room
}

/// A soft ellipse: `colour(t)` from its middle (t = 0) out to its rim (t = 1), in rings.
fn oval(painter: &egui::Painter, centre: egui::Pos2, radii: egui::Vec2, colour: impl Fn(f32) -> Color32) {
    const RINGS: u32 = 6;
    const SIDES: u32 = 48;
    let mut m = Mesh::default();
    m.colored_vertex(centre, colour(0.0));
    for r in 1..=RINGS {
        let t = r as f32 / RINGS as f32;
        for s in 0..SIDES {
            let a = s as f32 * std::f32::consts::TAU / SIDES as f32;
            m.colored_vertex(centre + vec2(a.cos() * radii.x, a.sin() * radii.y) * t, colour(t));
        }
    }
    let at = |r: u32, s: u32| if r == 0 { 0 } else { 1 + (r - 1) * SIDES + s % SIDES };
    for s in 0..SIDES {
        m.add_triangle(0, at(1, s), at(1, s + 1));
        for r in 1..RINGS {
            m.add_triangle(at(r, s), at(r + 1, s), at(r + 1, s + 1));
            m.add_triangle(at(r, s), at(r + 1, s + 1), at(r, s + 1));
        }
    }
    painter.add(Shape::mesh(m));
}

/// the look as the other gender: skin, hair colour and eyes kept, the hair style kept when that gender has it
fn gender(l: Look, g: u8) -> Look {
    Look { gender: g, hair_style: l.hair_style.min(hair_styles(g) - 1), ..l }
}

/// a locked row's words: fainter, still readable
fn dim(c: Color32) -> Color32 {
    c.gamma_multiply(0.7)
}

/// a soft band of light that sweeps over the picture while the new look is put on
fn sheen(ui: &Ui, pic: Rect) {
    let t = ui.input(|i| i.time) as f32;
    let p = if crate::animations() { (t / 1.4).fract() } else { 0.5 };
    let x = pic.min.x - 80.0 + (pic.width() + 160.0) * p;
    let painter = ui.painter().with_clip_rect(pic);
    let mut m = Mesh::default();
    // a slanted band, clear at its edges and white 20% along its middle
    for (dx, a) in [(-48.0, 0.0), (0.0, 0.2), (48.0, 0.0)] {
        let c = Color32::from_white_alpha((a * 255.0) as u8);
        m.vertices.push(Vertex { pos: pos2(x + dx + 40.0, pic.min.y), uv: egui::epaint::WHITE_UV, color: c });
        m.vertices.push(Vertex { pos: pos2(x + dx - 40.0, pic.max.y), uv: egui::epaint::WHITE_UV, color: c });
    }
    m.indices.extend([0, 1, 2, 2, 1, 3, 2, 3, 4, 4, 3, 5]);
    painter.add(Shape::mesh(m));
    ui.ctx().request_repaint();
}

/// a trainer's outline in frosted glass: where a picture of the character will be
pub fn silhouette(ui: &Ui, r: Rect) {
    let p = ui.painter();
    let (w, h) = (r.width(), r.height());
    let fill = Color32::from_white_alpha(30);
    let edge = Stroke::new(1.0, Color32::from_white_alpha(60));
    let head = pos2(r.center().x, r.min.y + h * 0.17);
    p.circle(head, w * 0.2, fill, edge);
    let body = Rect::from_min_max(pos2(r.min.x + w * 0.18, r.min.y + h * 0.35), pos2(r.max.x - w * 0.18, r.max.y));
    p.rect(body, egui::CornerRadius { nw: (w * 0.3) as u8, ne: (w * 0.3) as u8, sw: 4, se: 4 }, fill, edge, egui::StrokeKind::Inside);
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::characters::Character;

    fn frame(seq: u32, px: u8) -> Frame {
        Frame { seq, width: 1, height: 1, rgba: vec![px; 4], floor: None }
    }

    fn data(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redex-creator-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn one_change_sends_one_look_with_a_new_seq() {
        let mut c = Creator::new(&Store::default(), None);
        assert_eq!((c.editing, c.look), (None, Look::default_for(0)));
        let first = c.seq;
        let look = Look { hair_colour: 3, ..c.look };
        let (seq, a) = c.set(look).expect("a new look");
        assert!(seq > first);
        assert_eq!(a, look.appearance().unwrap());
        assert_eq!(c.look, look);
        // the same look again, or one out of range: nothing to send, the seq stays
        assert_eq!(c.set(look), None);
        assert_eq!(c.set(Look { eyes: 9, ..look }), None);
        assert_eq!(c.look, look);
        let (next, _) = c.set(Look { eyes: 2, ..look }).unwrap();
        assert_eq!(next, seq + 1);
    }

    #[test]
    fn a_gender_change_restarts_on_the_other_state() {
        let mut c = Creator::new(&Store::default(), None);
        assert!(c.needs_restart(None), "no child yet");
        assert!(!c.needs_restart(Some(0)));
        c.on_frame(frame(c.seq, 7));
        let girl = Look { gender: 1, ..Look::default_for(1) };
        let (_, a) = c.set(girl).unwrap();
        assert_eq!(a.gender, 1);
        assert!(c.needs_restart(Some(0)), "the boy's state can't wear a girl's look");
        assert!(!c.needs_restart(Some(1)));
        // the boy's last frame stays on screen while the girl's child starts
        assert_eq!(c.shown.as_ref().map(|f| f.rgba[0]), Some(7));
        assert!(c.dressing());
    }

    #[test]
    fn editing_locks_gender_and_skin() {
        let mut store = Store::default();
        // a character in boutique clothes: an edit keeps them
        let mut a = Look { hair_style: 2, ..Look::default_for(1) }.appearance().unwrap();
        a.fashion[12] = 0x03;
        assert!(a.validate());
        let id = store.create("Mira", a, 1).unwrap();
        let mut c = Creator::new(&store, Some(id));
        assert_eq!((c.editing, c.name.as_str(), c.look), (Some(id), "Mira", Look::of(&a)));
        assert_eq!(c.set(Look { gender: 0, ..Look::default_for(0) }), None, "gender");
        assert_eq!(c.set(Look { skin: if c.look.skin == 1 { 2 } else { 1 }, ..c.look }), None, "skin");
        let hair = Look { hair_colour: (c.look.hair_colour + 1) % 5, ..c.look };
        let (_, worn) = c.set(hair).expect("hair is free");
        assert_eq!(worn, hair.wearing(a.fashion).unwrap());
        assert_eq!(worn.fashion[12], 0x03, "the character's own clothes stay on");
        // an unknown id makes a new character
        assert_eq!(Creator::new(&store, Some(id + 1)).editing, None);
    }

    #[test]
    fn save_needs_a_valid_name() {
        let d = data("save");
        let mut store = Store::default();
        let mut c = Creator::new(&store, None);
        for bad in ["", "   ", "Thirteen chars"] {
            c.name = bad.into();
            assert!(c.save(&mut store, &d, 5).is_err(), "{bad:?}");
        }
        assert!(store.characters.is_empty() && !d.join("characters.json").exists());
        c.name = " Mira ".into();
        c.on_frame(frame(c.seq, 9));
        let id = c.save(&mut store, &d, 5).unwrap();
        let saved = Store::try_load(&d).unwrap();
        let m: &Character = saved.get(id).unwrap();
        assert_eq!((m.name.as_str(), m.look.clone()), ("Mira", c.look.appearance().unwrap().to_bytes().to_vec()));
        // the frame on screen is this look: it is the card
        assert!(super::super::preview::card_path(&d, m).is_file());

        // an edit sets the look and keeps the name
        let mut e = Creator::new(&saved, Some(id));
        let mut store = saved;
        e.set(Look { hair_style: 4, ..e.look }).unwrap();
        assert_eq!(e.save(&mut store, &d, 6), Ok(id));
        let after = Store::try_load(&d).unwrap();
        assert_eq!(after.characters.len(), 1);
        assert_eq!(Look::of(&Appearance::from_bytes(&after.characters[0].look).unwrap()).hair_style, 4);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn drags_snap_to_the_four_facings() {
        let mut v = Facings::default();
        assert_eq!(v.index(), 0);
        v.turn(30.0);
        assert_eq!(v.index(), 0, "under 45 degrees stays");
        v.turn(30.0);
        assert_eq!(v.index(), 1);
        v.zoom(2.0);
        v.turn(-150.0);
        assert_eq!(v.index(), 3, "back past the front");
        v.turn(-270.0 * 4.0);
        assert_eq!(v.index(), 3);
    }

    /// a drag of `dx` points a frame at 60 fps for `frames` frames from `t`, ticking as the stage does; the time after
    fn spin(s: &mut Studio, t: f64, dx: f32, frames: usize) -> f64 {
        let mut t = t;
        for _ in 0..frames {
            t += 1.0 / 60.0;
            s.tick(t);
            s.drag(dx, t);
        }
        t
    }


    #[test]
    fn a_drag_spins_freely_and_coasts_to_a_stop() {
        let mut s = Studio::default();
        s.tick(0.0);
        // 5 points a frame: 270 degrees a second
        let t = spin(&mut s, 0.0, 5.0, 30);
        let at = s.orbit().0;
        assert!((at - 30.0 * 5.0 * SPIN).abs() < 1e-3, "follows the pointer, not snapped: {at}");
        s.release(t);
        let released = t;
        let mut t = t;
        let mut seen = vec![];
        for _ in 0..30 {
            t += 1.0 / 60.0;
            seen.push((t, s.tick(t), s.orbit().0));
        }
        let carry = 270.0 * TAU;
        let end = seen.last().unwrap().2;
        assert!(((end - at) - carry).abs() < carry * 0.1, "coasted {} for {carry}", end - at);
        // an ease-out: 95 % of the way within 250 ms, every step smaller than the one before (the settle's last tenth of a
        // degree aside, under a pixel), and then still
        let by = seen.iter().find(|(tt, ..)| *tt - released >= 0.25 - 1e-9).map(|x| x.2).unwrap();
        assert!(end - by < carry * 0.05, "{by} -> {end}");
        let steps: Vec<f32> = seen.windows(2).map(|w| w[1].2 - w[0].2).collect();
        assert!(steps.windows(2).all(|w| w[1] <= w[0] + 1e-4 || w[1] < 0.1), "{steps:?}");
        assert!(!seen.last().unwrap().1, "still at the end");
        // held still before letting go: no coast
        let t = spin(&mut s, t, 5.0, 10);
        let t = spin(&mut s, t, 0.0, 6);
        let at = s.orbit().0;
        s.release(t);
        let mut t2 = t;
        for _ in 0..30 {
            t2 += 1.0 / 60.0;
            s.tick(t2);
        }
        assert!((s.orbit().0 - at).abs() < 1e-3);
    }

    #[test]
    fn zoom_is_clamped_and_a_double_click_resets() {
        let mut s = Studio::default();
        let settle = |s: &mut Studio, t: &mut f64| {
            for _ in 0..60 {
                *t += 1.0 / 60.0;
                s.tick(*t);
            }
        };
        let mut t = 0.0;
        s.tick(t);
        s.zoom(10.0);
        settle(&mut s, &mut t);
        assert_eq!(s.orbit().1, ZOOM_MAX);
        s.zoom(0.01);
        settle(&mut s, &mut t);
        assert_eq!(s.orbit().1, 1.0);
        // turned to 350 and zoomed: the reset goes on to 360 (the short way), whole body
        s.zoom(2.0);
        t = spin(&mut s, t, 350.0 / SPIN / 10.0, 10);
        s.release(t + 1.0);
        settle(&mut s, &mut t);
        assert!((s.orbit().0 - 350.0).abs() < 0.1, "{:?}", s.orbit());
        s.reset();
        let mut least = 360.0f32;
        for _ in 0..60 {
            t += 1.0 / 60.0;
            s.tick(t);
            least = least.min(if s.orbit().0 < 1.0 { 360.0 } else { s.orbit().0 });
        }
        assert!(least >= 349.9, "turned back the long way: {least}");
        assert_eq!(s.orbit(), (0.0, 1.0));
        assert!(s.at_rest());
        // a double-click's presses and releases (no movement) do not cancel the reset they make
        t = spin(&mut s, t, 100.0 / SPIN / 10.0, 10);
        s.release(t + 1.0);
        settle(&mut s, &mut t);
        s.reset();
        s.drag(0.0, t);
        s.release(t);
        s.drag(0.0, t);
        s.release(t);
        settle(&mut s, &mut t);
        assert_eq!(s.orbit(), (0.0, 1.0), "the reset held");
        // the arrows: a quarter turn, eased
        s.turn(90.0);
        t += 1.0 / 60.0;
        s.tick(t);
        assert!(s.orbit().0 > 0.0 && s.orbit().0 < 90.0);
        settle(&mut s, &mut t);
        assert_eq!(s.orbit().0, 90.0);
    }

    #[test]
    fn crops_turn_in_four_facings_and_studio_frames_spin() {
        let mut c = Creator::new(&Store::default(), None);
        assert_eq!(c.wanted(), Turn::Wait, "nothing sent before the child says what it sends");
        // the child sends crops (v1): the four facings
        c.on_frame(frame(0, 1));
        assert_eq!(c.wanted(), Turn::Face(0));
        c.source().drag(60.0, 0.1);
        assert_eq!(c.wanted(), Turn::Face(2), "60 points snap to a half turn");
        // studio frames (v2): a free spin
        c.on_frame(Frame { floor: Some([1, 1, 1, 1]), ..frame(0, 1) });
        assert_eq!(c.wanted(), Turn::Orbit(0.0, 1.0));
        c.source().drag(10.0, 0.2);
        assert_eq!(c.wanted(), Turn::Orbit(360.0 - 10.0 * SPIN, 1.0), "a drag right turns as the facings do");
        // and back to crops when the child falls back
        c.on_frame(frame(0, 1));
        assert_eq!(c.wanted(), Turn::Face(2));
    }

    #[test]
    fn the_last_frame_is_held_while_dressing() {
        let mut c = Creator::new(&Store::default(), None);
        assert!(!c.dressing(), "nothing shown yet is not dressing");
        // the state's own outfit (seq 0) is never shown
        c.on_frame(frame(0, 1));
        assert!(c.shown.is_none());
        c.on_frame(frame(c.seq, 2));
        assert!(!c.dressing());
        let old = c.seq;
        c.set(Look { hair_colour: (c.look.hair_colour + 1) % 5, ..c.look }).unwrap();
        assert!(c.dressing());
        // the old look's frames keep coming (a turn still shows), the state's own and empty ones never replace them
        c.on_frame(frame(old, 3));
        c.on_frame(frame(0, 4));
        c.on_frame(Frame { seq: c.seq, width: 0, height: 0, rgba: vec![], floor: None });
        assert_eq!(c.shown.as_ref().map(|f| (f.seq, f.rgba[0])), Some((old, 3)));
        assert!(c.dressing());
        c.on_frame(frame(c.seq, 5));
        assert!(!c.dressing());
        assert_eq!(c.shown.as_ref().map(|f| f.rgba[0]), Some(5));
        // once the new look is on, an older look's frame (from before) never comes back
        c.on_frame(frame(old, 6));
        assert_eq!(c.shown.as_ref().map(|f| f.rgba[0]), Some(5));
    }
}
