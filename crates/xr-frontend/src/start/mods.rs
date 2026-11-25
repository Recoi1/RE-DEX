//! The Mods screen, opened by the start screen's Mods pill: Pokémon X and Y's mods on GameBanana, most liked first,
//! with what the RE:DEX developers tested, a search, and Install / Open page / Turn on or off / Remove. A mod that
//! changes more than looks (data, text, code) is marked "Offline only": it keeps the game out of worlds while it is
//! on. Mods change only this player's own screen. The list and the pictures load on a thread of the screen's own; an
//! install runs on one the start screen keeps ([`Installer`]), so it outlives the screen and blocks Play until done.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use egui::{pos2, vec2, Align, Align2, Color32, ColorImage, FontId, Layout, Rect, Sense, Shape, TextureHandle, TextureOptions, Ui};
use xr_hle::modlist::{self, Installed};

use super::glass::{self, Glass, BODY, BRAND_RED, TITLE};
use super::{at, bold, fit, text, AMBER, CARD_BODY, GREEN, RED};
use crate::gamebanana::{self as gb, Mod, Stage, Status, Tested};

/// what the list thread sends
enum Event {
    List(Result<Vec<Mod>, String>),
    Tested(HashMap<u64, Tested>),
    Picture(u64, Option<ColorImage>),
}

/// what the install thread sends
enum Step {
    Stage(Stage, u64),
    Done(Result<Installed, String>),
}

/// the start screen's one install at a time: it goes on while the Mods screen is closed, and the start screen keeps
/// Play (and the Mods screen open) until it ends. Dropped (the window closed), it cancels the download
pub struct Installer {
    job: Option<Job>,
    steps: Receiver<Step>,
    send: Sender<Step>,
    /// the last install's outcome, until the Mods screen shows it
    outcome: Option<(String, bool)>,
}

impl Installer {
    pub fn new() -> Installer {
        let (send, steps) = channel();
        Installer { job: None, steps, send, outcome: None }
    }

    /// an install is under way
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// takes in what the install thread sent
    pub fn pump(&mut self) {
        while let Ok(step) = self.steps.try_recv() {
            match step {
                Step::Stage(stage, total) => {
                    if let Some(job) = &mut self.job {
                        (job.stage, job.total) = (stage, total);
                    }
                }
                Step::Done(result) => {
                    let name = self.job.take().map(|j| j.name).unwrap_or_default();
                    self.outcome = Some(match result {
                        Ok(_) => (format!("{name} is installed and on. It shows the next time you press Play."), false),
                        Err(e) if e == "cancelled" => ("Install cancelled.".into(), false),
                        Err(e) => (e, true),
                    });
                }
            }
        }
    }

    fn start(&mut self, data: PathBuf, m: &Mod) {
        let (got, cancel) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        self.job = Some(Job { id: m.id, name: m.name.clone(), stage: Stage::Asking, total: 0, got: got.clone(), cancel: cancel.clone() });
        self.outcome = None;
        let (send, m) = (self.send.clone(), m.clone());
        std::thread::Builder::new()
            .name("mod install".into())
            .spawn(move || {
                let stage = |s, total| drop(send.send(Step::Stage(s, total)));
                let result = gb::install(&data, &m, &stage, &cancel, &got);
                if let Err(e) = &result {
                    log::warn!("mods: installing {} ({}) failed: {e}", m.name, m.id);
                }
                let _ = send.send(Step::Done(result));
            })
            .ok();
    }
}

impl Default for Installer {
    fn default() -> Installer {
        Installer::new()
    }
}

impl Drop for Installer {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// an install under way
struct Job {
    id: u64,
    name: String,
    stage: Stage,
    total: u64,
    got: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

/// what the screen asks the start screen for
#[derive(Debug, PartialEq, Eq)]
pub enum ModsAction {
    None,
    Close,
}

/// the keys and buttons the start screen heard this frame
#[derive(Default, Clone, Copy)]
pub struct ModsNav {
    pub up: bool,
    pub down: bool,
    pub select: bool,
    pub back: bool,
}

pub struct Mods {
    data: PathBuf,
    events: Receiver<Event>,
    loading: bool,
    error: Option<String>,
    mods: Vec<Mod>,
    tested: HashMap<u64, Tested>,
    pictures: HashMap<u64, TextureHandle>,
    installed: Vec<Installed>,
    /// each installed mod's first file off the cosmetic list (what makes it offline only)
    offline: HashMap<u64, Option<String>>,
    clashes: Vec<(String, Vec<u64>)>,
    search: String,
    typing: bool,
    selected: Option<u64>,
    /// the list scrolls to the selection once after a key moved it
    reveal: bool,
    /// a game runs from this data folder (Install, Remove and Turn on/off wait for it to close)
    game_open: bool,
    /// the last install's or removal's outcome, and whether it went wrong
    message: Option<(String, bool)>,
}

/// a picture GameBanana sent (JPEG or PNG) as an image, None when it can't be read
fn decode(bytes: &[u8]) -> Option<ColorImage> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        let (w, h) = (info.width as usize, info.height as usize);
        let rgba: Vec<u8> = match (info.color_type, info.bit_depth) {
            (png::ColorType::Rgba, png::BitDepth::Eight) => buf[..w * h * 4].to_vec(),
            (png::ColorType::Rgb, png::BitDepth::Eight) => buf[..w * h * 3].chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            _ => return None,
        };
        return Some(ColorImage::from_rgba_unmultiplied([w, h], &rgba));
    }
    // a thumbnail: anything bigger than 2048 px a side is refused before it is decoded
    let options = zune_jpeg::zune_core::options::DecoderOptions::default().set_max_width(2048).set_max_height(2048);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = pixels.len() / (w * h).max(1);
    let rgba: Vec<u8> = match channels {
        1 => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        3 => pixels.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        4 => pixels,
        _ => return None,
    };
    Some(ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

/// every page of the list (most liked first), then the tested list, then each mod's picture
fn fetch_all(send: Sender<Event>) {
    let mut mods = Vec::new();
    for page in 1..=gb::MAX_PAGES {
        match gb::fetch(&gb::list_url(page), 4 << 20).and_then(|bytes| gb::parse_list(&bytes)) {
            Ok((more, complete)) => {
                mods.extend(more);
                if complete {
                    break;
                }
            }
            Err(e) if mods.is_empty() => return drop(send.send(Event::List(Err(e)))),
            Err(e) => {
                log::warn!("mods: page {page} of the list: {e}");
                break;
            }
        }
    }
    let pictures: Vec<(u64, String)> = mods.iter().filter_map(|m| Some((m.id, m.thumb.clone()?))).collect();
    let _ = send.send(Event::List(Ok(mods)));
    // missing or unreachable: every mod shows as untested
    if let Ok(bytes) = gb::fetch(gb::COMPAT_URL, 1 << 20) {
        let _ = send.send(Event::Tested(gb::parse_compat(&bytes)));
    }
    for (id, url) in pictures {
        let image = gb::fetch(&url, 2 << 20).ok().and_then(|bytes| decode(&bytes));
        if send.send(Event::Picture(id, image)).is_err() {
            return; // the screen was closed
        }
    }
}

/// the status shown for a mod: what was tested, or Untested
fn status(tested: Option<&Tested>) -> (&'static str, Color32) {
    match tested.map(|t| t.status) {
        Some(Status::Verified) => ("Verified", GREEN),
        Some(Status::Partial) => ("Partial", AMBER),
        Some(Status::Broken) => ("Broken", RED),
        None => ("Untested", CARD_BODY),
    }
}

/// 1st, 2nd, 3rd ...
fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// does a mod match what was typed: its name, author or category, any case
fn matches(m: &Mod, search: &str) -> bool {
    let search = search.trim().to_lowercase();
    search.is_empty() || [&m.name, &m.author, &m.category].iter().any(|s| s.to_lowercase().contains(&search))
}

/// a small badge: a dot and a word, on a faint pill; its width
fn badge(ui: &Ui, left: egui::Pos2, word: &str, color: Color32) -> f32 {
    let g = ui.painter().layout_no_wrap(word.to_owned(), FontId::proportional(12.0), CARD_BODY);
    let r = Rect::from_min_size(pos2(left.x, left.y - 11.0), vec2(g.size().x + 28.0, 22.0));
    ui.painter().rect_filled(r, 11.0, Color32::from_white_alpha(18));
    ui.painter().circle_filled(pos2(r.min.x + 11.0, r.center().y), 3.5, color);
    ui.painter().galley(pos2(r.min.x + 19.0, r.center().y - g.size().y / 2.0), g, CARD_BODY);
    r.width()
}

impl Mods {
    pub fn new(data: PathBuf) -> Mods {
        let (send, events) = channel();
        std::thread::Builder::new().name("mods list".into()).spawn(move || fetch_all(send)).ok();
        let mut screen = Mods {
            data,
            events,
            loading: true,
            error: None,
            mods: Vec::new(),
            tested: HashMap::new(),
            pictures: HashMap::new(),
            installed: Vec::new(),
            offline: HashMap::new(),
            clashes: Vec::new(),
            search: String::new(),
            typing: false,
            selected: None,
            reveal: false,
            game_open: false,
            message: None,
        };
        screen.reload();
        screen
    }

    /// the installed mods read again (after a change), with what each changes
    fn reload(&mut self) {
        self.installed = modlist::load(&self.data);
        self.offline = self.installed.iter().map(|m| (m.id, modlist::offline_file(&modlist::folder(&self.data, m.id)))).collect();
        self.clashes = modlist::clashes(&self.data, &self.installed);
    }

    /// takes in what the threads sent
    fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::List(Ok(mods)) => (self.mods, self.loading) = (mods, false),
                Event::List(Err(e)) => (self.error, self.loading) = (Some(e), false),
                Event::Tested(t) => self.tested = t,
                Event::Picture(id, Some(image)) => {
                    self.pictures.insert(id, ctx.load_texture(format!("mod-{id}"), image, TextureOptions::LINEAR));
                }
                Event::Picture(_, None) => {}
            }
        }
    }

    /// the mods to list: GameBanana's (most liked first) that match the search, then installed ones it no longer lists
    fn rows(&self) -> Vec<Mod> {
        let mut rows: Vec<Mod> = self.mods.iter().filter(|m| matches(m, &self.search)).cloned().collect();
        for m in &self.installed {
            if !self.mods.iter().any(|g| g.id == m.id) {
                let row = Mod { id: m.id, name: m.name.clone(), author: String::new(), category: "Installed".into(), likes: 0, page: format!("https://gamebanana.com/mods/{}", m.id), thumb: None };
                if matches(&row, &self.search) {
                    rows.push(row);
                }
            }
        }
        rows
    }

    /// a flat pill button; true when pressed
    fn button(&self, ui: &mut Ui, glass: &Glass, rect: Rect, id: &str, word: &str, primary: bool) -> bool {
        use crate::icons::Icon as I;
        use crate::theme as th;
        let r = ui.interact(rect, ui.id().with(("mods-button", id)), Sense::click());
        let (hover, scale) = glass::press_scale(ui, &r, 1.04);
        let (kind, icon, hue) = match id {
            "install" => (glass::Kind::Primary, I::Import, Color32::WHITE),
            "remove" => (glass::Kind::Danger, I::Trash, th::CORAL),
            "page" => (glass::Kind::Secondary, I::Globe, th::AZURE),
            _ => (if primary { glass::Kind::Primary } else { glass::Kind::Secondary }, I::Close, th::SLATE),
        };
        if primary {
            glass::glow(ui.painter(), rect, rect.height() / 2.0, BRAND_RED, 1.0 + 0.6 * hover);
        }
        glass.button_face(ui.painter(), rect, scale, hover, kind, true);
        let font = th::heavy(ui.ctx(), th::LABEL);
        let w = ui.painter().layout_no_wrap(word.to_owned(), font.clone(), TITLE).size().x + 24.0;
        let x = rect.center().x - w / 2.0;
        crate::icons::draw(ui.painter(), icon, pos2(x + 9.0, rect.center().y), 20.0 * scale, hue, -8.0);
        let color = if kind == glass::Kind::Danger { th::CORAL } else { TITLE };
        glass::shadowed(ui.painter(), pos2(x + 24.0, rect.center().y), Align2::LEFT_CENTER, word, font, color);
        r.clicked()
    }

    /// the screen; it stays open while `installer` runs an install
    pub fn ui(&mut self, ui: &mut Ui, glass: &Glass, nav: ModsNav, installer: &mut Installer) -> ModsAction {
        self.pump(ui.ctx());
        if let Some(outcome) = installer.outcome.take() {
            self.message = Some(outcome);
            self.reload();
        }
        let panel = Rect::from_min_max(pos2(32.0, 104.0), pos2(1008.0, 608.0));
        glass.paint(ui.painter(), panel, crate::theme::R_PANEL, glass::Look { dot: true, lift: 1.0, seed: 3, ..Default::default() });
        let busy = installer.busy();
        self.game_open = super::storage::game_running(&self.data);
        if nav.back && !self.typing && !busy {
            return ModsAction::Close;
        }
        let rows = self.rows();
        if self.selected.is_none_or(|id| !rows.iter().any(|m| m.id == id)) {
            self.selected = rows.first().map(|m| m.id);
        }
        let at_row = self.selected.and_then(|id| rows.iter().position(|m| m.id == id)).unwrap_or(0);
        if !self.typing && (nav.up || nav.down) && !rows.is_empty() {
            let i = if nav.up { at_row.saturating_sub(1) } else { (at_row + 1).min(rows.len() - 1) };
            (self.selected, self.reveal) = (Some(rows[i].id), true);
        }

        // header: back, the title, and the search field on the right
        let back = Rect::from_min_size(pos2(56.0, 124.0), vec2(40.0, 40.0));
        // while an install runs the screen stays: cancel it first
        let resp = ui.interact(back, ui.id().with("mods-back"), if busy { Sense::hover() } else { Sense::click() });
        let (hover, scale) = if busy { (0.0, 1.0) } else { glass::press_scale(ui, &resp, 1.06) };
        glass.button_face(ui.painter(), back, scale, hover, glass::Kind::Secondary, !busy);
        crate::icons::draw(ui.painter(), crate::icons::Icon::Prev, back.center(), 18.0 * scale, TITLE, 0.0);
        if busy {
            ui.painter().circle_filled(back.center(), 21.0, Color32::from_rgba_unmultiplied(0x10, 0x16, 0x28, 150));
        }
        if resp.clicked() && !busy {
            return ModsAction::Close;
        }
        crate::icons::draw(ui.painter(), crate::icons::Icon::Puzzle, pos2(122.0, 144.0), 22.0, crate::theme::AMBER, -8.0);
        glass.title(ui.painter(), pos2(140.0, 136.0), "Mods", crate::theme::EYEBROW + 1.5);
        let count = match (self.loading, &self.error) {
            (true, _) => "Asking GameBanana…".to_owned(),
            (_, Some(_)) => "GameBanana can't be reached".to_owned(),
            _ if self.search.trim().is_empty() => format!("{} on GameBanana, most liked first", self.mods.len()),
            _ => format!("{} of {} match", rows.len(), self.mods.len()),
        };
        text(ui, pos2(206.0, 144.0), Align2::LEFT_CENTER, &count, crate::theme::CAPTION, CARD_BODY);
        let field = Rect::from_min_size(pos2(656.0, 124.0), vec2(320.0, 40.0));
        glass::well(ui.painter(), field, self.typing);
        crate::icons::draw(ui.painter(), crate::icons::Icon::Search, pos2(field.min.x + 21.0, field.center().y), 20.0, crate::theme::AZURE, -8.0);
        let edit = at(ui, Rect::from_min_max(field.min + vec2(40.0, 8.0), field.max - vec2(16.0, 8.0)), Layout::left_to_right(Align::Center), |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .frame(egui::Frame::NONE)
                    .desired_width(264.0)
                    .font(FontId::proportional(16.0))
                    .text_color(TITLE)
                    .hint_text(egui::RichText::new("Search mods").color(BODY))
                    .char_limit(40),
            )
        });
        self.typing = edit.has_focus();

        // the list
        let list = Rect::from_min_max(pos2(48.0, 184.0), pos2(576.0, 592.0));
        if self.loading || self.error.is_some() && self.mods.is_empty() && self.installed.is_empty() {
            let (line, tone) = match &self.error {
                Some(e) => (format!("{e}. Check your internet connection; installed mods still work."), RED),
                None => ("Loading the list…".to_owned(), CARD_BODY),
            };
            at(ui, list.shrink2(vec2(16.0, 8.0)), Layout::top_down(Align::Min), |ui| {
                ui.add(egui::Label::new(egui::RichText::new(line).size(14.0).color(tone)).wrap());
                if self.loading {
                    ui.add_space(12.0);
                    glass.progress(ui, None, true);
                }
            });
        } else if rows.is_empty() {
            text(ui, list.min + vec2(16.0, 20.0), Align2::LEFT_CENTER, "No mod matches that. Try another word.", 14.0, CARD_BODY);
        } else {
            let mut clicked = None;
            let reveal = std::mem::take(&mut self.reveal);
            at(ui, list, Layout::top_down(Align::Min), |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    for m in &rows {
                        let (rect, resp) = ui.allocate_exact_size(vec2(list.width() - 16.0, 72.0), Sense::click());
                        let selected = self.selected == Some(m.id);
                        if selected && reveal {
                            resp.scroll_to_me(None);
                        }
                        self.row(ui, glass, rect, &resp, m, selected);
                        if resp.clicked() {
                            clicked = Some(m.id);
                        }
                    }
                });
            });
            if clicked.is_some() {
                self.selected = clicked;
            }
        }

        // the selected mod
        let detail = Rect::from_min_max(pos2(600.0, 184.0), pos2(976.0, 592.0));
        let chosen = self.selected.and_then(|id| rows.iter().find(|m| m.id == id)).cloned();
        if let Some(m) = chosen {
            self.detail(ui, glass, detail, &m, nav.select && !self.typing, installer);
        }
        ModsAction::None
    }

    /// one mod in the list: its picture, name, who made it, and its badges
    fn row(&self, ui: &Ui, glass: &Glass, rect: Rect, resp: &egui::Response, m: &Mod, selected: bool) {
        let hover = glass.hover_of(ui, resp);
        ui.painter().rect_filled(rect, 16.0, Color32::from_white_alpha(if selected { 24 } else { (8.0 + 12.0 * hover) as u8 }));
        if selected {
            ui.painter().rect_stroke(rect, 16.0, egui::Stroke::new(1.2, crate::theme::BRAND_HI.gamma_multiply(0.8)), egui::StrokeKind::Inside);
            ui.painter().rect_filled(Rect::from_min_size(rect.min + vec2(0.0, 14.0), vec2(3.0, rect.height() - 28.0)), 2.0, crate::theme::BRAND_HI);
        }
        if selected {
            ui.painter().add(Shape::mesh(glass::rim_mesh(rect, 16.0, 0.7)));
        }
        let pic = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(100.0, 56.0));
        match self.pictures.get(&m.id) {
            Some(tex) => glass.picture(ui.painter(), pic, 10.0, tex.id(), Color32::WHITE, false),
            None => {
                ui.painter().rect_filled(pic, 10.0, Color32::from_white_alpha(14));
                glass::draw_icon(ui.painter(), glass::Icon::Folder, pic.center(), 9.0);
            }
        }
        let x = pic.max.x + 16.0;
        let installed = self.installed.iter().find(|i| i.id == m.id);
        // the badges on the right: a test result (Untested only in the detail: on every row it is noise), then on or
        // off, and offline only
        let mut words: Vec<(&str, Color32)> = self.tested.get(&m.id).map(|t| status(Some(t))).into_iter().collect();
        if let Some(i) = installed {
            words.push(if i.enabled { ("On", GREEN) } else { ("Off", CARD_BODY) });
            if self.offline.get(&m.id).is_some_and(Option::is_some) {
                words.push(("Offline only", AMBER));
            }
        }
        let widths: f32 = words.iter().map(|(w, _)| ui.painter().layout_no_wrap(w.to_string(), FontId::proportional(12.0), TITLE).size().x + 28.0 + 6.0).sum();
        let mut bx = rect.max.x - 12.0 - widths;
        for (word, color) in &words {
            bx += badge(ui, pos2(bx, rect.min.y + 24.0), word, *color) + 6.0;
        }
        let name_w = rect.max.x - 12.0 - widths - x - 8.0;
        bold(ui, pos2(x, rect.min.y + 24.0), &fit(ui, &m.name, 16.0, name_w), 16.0, TITLE);
        let by = match (m.author.is_empty(), m.likes) {
            (true, _) => m.category.clone(),
            (false, 0) => format!("{} · {}", m.author, m.category),
            (false, likes) => format!("{} · {} · {likes} likes", m.author, m.category),
        };
        text(ui, pos2(x, rect.min.y + 50.0), Align2::LEFT_CENTER, &fit(ui, &by, 13.0, rect.max.x - 12.0 - x), 13.0, CARD_BODY);
    }

    /// the selected mod: its picture and name, what was tested, where it stands, and what can be done w/ it
    fn detail(&mut self, ui: &mut Ui, glass: &Glass, area: Rect, m: &Mod, select: bool, installer: &mut Installer) {
        // the picture at its own shape, as wide as the column and at most 128 px tall, rounded like the list's
        let tex = self.pictures.get(&m.id);
        let aspect = tex.map_or(9.0 / 16.0, |t| t.size_vec2().y / t.size_vec2().x.max(1.0));
        let height = (area.width() * aspect).min(128.0);
        let pic = Rect::from_min_size(area.min, vec2((height / aspect).min(area.width()), height));
        match tex {
            Some(tex) => glass.picture(ui.painter(), pic, 16.0, tex.id(), Color32::WHITE, false),
            None => {
                ui.painter().rect_filled(pic, 16.0, Color32::from_white_alpha(14));
            }
        }
        let mut y = pic.max.y + 28.0;
        bold(ui, pos2(area.min.x, y), &fit(ui, &m.name, 18.0, area.width()), 18.0, TITLE);
        y += 26.0;
        let installed = self.installed.iter().position(|i| i.id == m.id);
        let tested = self.tested.get(&m.id);
        let (word, color) = status(tested);
        let mut x = area.min.x;
        x += badge(ui, pos2(x, y), word, color) + 6.0;
        if let Some(i) = installed {
            let on = self.installed[i].enabled;
            x += badge(ui, pos2(x, y), if on { "On" } else { "Off" }, if on { GREEN } else { CARD_BODY }) + 6.0;
            if self.offline.get(&m.id).is_some_and(Option::is_some) {
                badge(ui, pos2(x, y), "Offline only", AMBER);
            }
        }
        y += 22.0;
        // what was tested, then where this mod stands
        let mut lines: Vec<(String, Color32)> = Vec::new();
        match tested {
            Some(t) if !t.note.is_empty() => lines.push((t.note.clone(), CARD_BODY)),
            Some(_) => {}
            // once installed the badges say where it stands, so the general advice goes
            None if installed.is_none() => lines.push(("Not tested with RE:DEX yet. Skins and texture packs usually work; mods that change the game's code can't be installed.".into(), CARD_BODY)),
            None => {}
        }
        if let Some(i) = installed {
            let im = &self.installed[i];
            if let Some(Some(file)) = self.offline.get(&m.id) {
                lines.push((format!("Offline only: it changes game data ({file}), so worlds are closed while it is on."), AMBER));
            }
            let on: Vec<u64> = self.installed.iter().filter(|o| o.enabled).map(|o| o.id).collect();
            if im.enabled && on.len() > 1 {
                let place = on.iter().position(|&id| id == m.id).unwrap_or(0) + 1;
                lines.push((format!("Loaded {} of {} mods on: a later one wins where two change the same file.", ordinal(place), on.len()), CARD_BODY));
            }
            let others: Vec<String> = self.clashes.iter().filter(|(_, ids)| ids.contains(&m.id)).flat_map(|(_, ids)| ids.iter().copied()).filter(|&id| id != m.id).map(|id| self.installed.iter().find(|o| o.id == id).map_or(id.to_string(), |o| o.name.clone())).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            if im.enabled && !others.is_empty() {
                lines.push((format!("Clashes with {}: both change the same files. Turn one off to choose.", others.join(", ")), AMBER));
            }
        }
        let notes = Rect::from_min_max(pos2(area.min.x, y), pos2(area.max.x, area.max.y - 64.0));
        at(ui, notes, Layout::top_down(Align::Min), |ui| {
            // a long story scrolls inside its space, never over the buttons
            egui::ScrollArea::vertical().id_salt("mod notes").max_height(notes.height()).auto_shrink([false, true]).show(ui, |ui| {
            for (line, tone) in &lines {
                ui.add(egui::Label::new(egui::RichText::new(line).size(13.0).color(*tone)).wrap());
                ui.add_space(6.0);
            }
            if let Some((message, bad)) = &self.message {
                ui.add(egui::Label::new(egui::RichText::new(message).size(13.0).color(if *bad { RED } else { GREEN })).wrap());
            }
            });
        });

        // the actions, along the bottom: the main one on the right
        let by = area.max.y - 44.0;
        let r = |x: f32, w: f32| Rect::from_min_size(pos2(x, by), vec2(w, 40.0));
        if let Some(job) = installer.job.as_ref().filter(|j| j.id == m.id) {
            let (what, fraction) = match job.stage {
                Stage::Asking => ("Asking GameBanana…".to_owned(), None),
                Stage::Downloading if job.total > 0 => {
                    let got = job.got.load(Ordering::Relaxed);
                    (format!("Downloading {} of {} MB", got >> 20, (job.total >> 20).max(1)), Some(got as f32 / job.total as f32))
                }
                Stage::Downloading => ("Downloading…".to_owned(), None),
                Stage::Unpacking => ("Unpacking and checking…".to_owned(), None),
            };
            text(ui, pos2(area.min.x, by - 30.0), Align2::LEFT_CENTER, &what, 13.0, CARD_BODY);
            at(ui, Rect::from_min_size(pos2(area.min.x, by + 12.0), vec2(area.width() - 128.0, 16.0)), Layout::left_to_right(Align::Center), |ui| glass.progress(ui, fraction, true));
            if self.button(ui, glass, r(area.max.x - 112.0, 112.0), "cancel", "Cancel", false) {
                job.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }
        let page = !m.page.is_empty();
        if page && self.button(ui, glass, r(area.min.x, 112.0), "page", "Open page", false) {
            gb::open_page(&m.page);
        }
        // a game running from this data folder reads the mods' files as it goes: they stay as they are till it closes
        if self.game_open {
            text(ui, pos2(area.max.x, by + 20.0), Align2::RIGHT_CENTER, "Close the game to change mods", 13.0, CARD_BODY);
            return;
        }
        match installed.map(|i| self.installed[i].clone()) {
            None => {
                let busy = installer.busy();
                if !busy && (self.button(ui, glass, r(area.max.x - 128.0, 128.0), "install", "Install", true) || select) {
                    self.message = None;
                    installer.start(self.data.clone(), m);
                } else if busy {
                    text(ui, pos2(area.max.x, by + 20.0), Align2::RIGHT_CENTER, "Another install is running", 13.0, CARD_BODY);
                }
            }
            Some(im) => {
                if self.button(ui, glass, r(area.min.x + if page { 128.0 } else { 0.0 }, 96.0), "remove", "Remove", false) {
                    self.message = Some(match modlist::remove(&self.data, im.id) {
                        Ok(()) => (format!("{} is removed.", im.name), false),
                        Err(e) => (format!("We couldn't remove {}: {e}", im.name), true),
                    });
                    self.reload();
                }
                let word = if im.enabled { "Turn off" } else { "Turn on" };
                if self.button(ui, glass, r(area.max.x - 128.0, 128.0), "toggle", word, true) || select {
                    if let Err(e) = modlist::set_enabled(&self.data, im.id, !im.enabled) {
                        self.message = Some((format!("We couldn't save that: {e}"), true));
                    } else {
                        self.message = None;
                    }
                    self.reload();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u64, name: &str, author: &str, category: &str) -> Mod {
        Mod { id, name: name.into(), author: author.into(), category: category.into(), likes: 0, page: String::new(), thumb: None }
    }

    #[test]
    fn search_matches_name_author_or_category() {
        let skin = m(1, "Delta over Serena", "Someone", "Skins");
        assert!(matches(&skin, "") && matches(&skin, "  serena ") && matches(&skin, "SOME") && matches(&skin, "skin"));
        assert!(!matches(&skin, "texture"));
    }

    #[test]
    fn badges_and_order_words() {
        assert_eq!(status(None).0, "Untested");
        assert_eq!(status(Some(&Tested { status: Status::Verified, note: String::new() })).0, "Verified");
        assert_eq!([1, 2, 3, 4, 11, 12, 13, 21, 22].map(ordinal), ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd"]);
    }

    #[test]
    fn pictures_decode_from_png() {
        let mut bytes = Vec::new();
        let mut enc = png::Encoder::new(&mut bytes, 2, 1);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&[255, 0, 0, 0, 0, 255]).unwrap();
        let image = decode(&bytes).unwrap();
        assert_eq!((image.size, image.pixels[0], image.pixels[1]), ([2, 1], Color32::RED, Color32::BLUE));
        assert!(decode(b"not a picture").is_none());
    }

    /// installed mods GameBanana no longer lists still show, so they can be turned off or removed
    #[test]
    fn installed_mods_stay_listed() {
        let data = std::env::temp_dir().join(format!("redex-mods-screen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let unpacked = modlist::root(&data).join(".unpack");
        std::fs::create_dir_all(unpacked.join("romfs/a/0/8")).unwrap();
        std::fs::write(unpacked.join("romfs/a/0/8/8"), b"skin").unwrap();
        modlist::install(&data, 42, "Gone from GameBanana", "", &unpacked).unwrap();
        let (_send, events) = channel();
        let mut screen = Mods { data: data.clone(), events, loading: false, error: None, mods: vec![m(1, "Listed", "a", "Skins")], tested: HashMap::new(), pictures: HashMap::new(), installed: Vec::new(), offline: HashMap::new(), clashes: Vec::new(), search: String::new(), typing: false, selected: None, reveal: false, game_open: false, message: None };
        screen.reload();
        assert_eq!(screen.rows().iter().map(|r| r.id).collect::<Vec<_>>(), [1, 42]);
        assert_eq!(screen.offline[&42], None, "a player skin is cosmetic");
        screen.search = "gone".into();
        assert_eq!(screen.rows().iter().map(|r| r.id).collect::<Vec<_>>(), [42]);
        std::fs::remove_dir_all(data).unwrap();
    }
}
