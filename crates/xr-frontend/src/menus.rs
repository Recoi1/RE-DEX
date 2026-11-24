//! what egui draws over the game: the Configure window (Azahar's tabs: General, Graphics, Audio, Controls), the frame
//! rate pill, toasts, the paused look and the game's own keyboard, themed in RE:DEX's colours and animated. Ported from
//! Zakuro's menus.rs (commit fdf67b2): its settings window, the helper rows, the controls grids, `keyboard`, `message`
//! and the overlay badges; the library, cheats, multiplayer, the per-game settings and the compiler offer stay behind
//! (see NOTICE). Everything here is drawn by the presenter over the window, never into the 3DS screens.

use std::time::Instant;

use egui::{Color32, CornerRadius, Id, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use winit::keyboard::KeyCode;
use xr_hle::services::keyboard::Request;

use crate::gamepad::button_name;
use crate::settings::{Filter, Keys, Language, MicrophoneSource, Orientation, PadButtons, Renderer, Screens, Settings, SmoothMotion, Stereo3d, Theme, TopShape};

/// what the user asked for
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// the settings changed, to apply (saved once Configure closes)
    Settings,
    /// Configure closed after changes, to save them
    Save,
    /// close the game's keyboard w/ this text and the button pressed
    Keyboard(String, usize),
    /// start the game again, for what only applies when it starts
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    General,
    Graphics,
    Audio,
    Controls,
}

const TABS: [(Tab, &str); 4] = [(Tab::General, "General"), (Tab::Graphics, "Graphics"), (Tab::Audio, "Audio"), (Tab::Controls, "Controls")];

/// a note for a moment, sliding in from the bottom right corner
struct Toast {
    text: String,
    born: Instant,
    id: u64,
    /// the text pulses once, for the speed changing
    pulse: bool,
}

/// how long a toast stays, its slides included
const TOAST_LIFE: f32 = 2.4;
/// `rebinding` and `rebinding_pad` waiting for the Players list's key or button (not a console button)
pub const PLAYERS: usize = usize::MAX;
/// the usual animation, everything is quick
const QUICK: f32 = 0.18;

pub struct Menus {
    /// the Configure window
    pub configure_open: bool,
    pub tab: Tab,
    /// when the tab last changed, the new one fades in
    tab_since: Instant,
    /// the binding waiting for a key, by its place in Keys::all_mut
    pub rebinding: Option<usize>,
    /// the controller binding waiting for a button, by its place in PadButtons::all_mut
    pub rebinding_pad: Option<usize>,
    /// a binding that just lost its key to another, it shakes, and since when
    shake: Option<(usize, Instant)>,
    /// something to tell the user, till they close it
    pub message: Option<String>,
    toasts: Vec<Toast>,
    next_toast: u64,
    /// a screenshot's flash, and the dip after the screens moved, since when
    flash: Option<Instant>,
    dip: Option<Instant>,
    /// what's being typed into the game's keyboard, and what it asked for
    typing: Option<(Request, String)>,
    /// Windows shows animations (its "Show animations in Windows"), else everything changes at once
    pub motion: bool,
    /// the frame rate and speed shown, eased so the number doesn't jitter
    rates: (f32, f32),
    /// the settings changed while Configure was open (a rebinding counts), it saves them and says so when it closes
    pub changed: bool,
    /// the keyboard was put on the tabs since Configure opened
    focused: bool,
    /// the frame the tab last moved by key in, egui may run a frame's UI twice w/ the same keys
    tab_key_frame: u64,
    /// the tab the keyboard goes back to next frame, after egui's own arrow keys moved it elsewhere
    refocus: Option<usize>,
}

impl Default for Menus {
    fn default() -> Menus {
        Menus {
            configure_open: false,
            tab: Tab::default(),
            tab_since: Instant::now(),
            rebinding: None,
            rebinding_pad: None,
            shake: None,
            message: None,
            toasts: Vec::new(),
            next_toast: 0,
            flash: None,
            dip: None,
            typing: None,
            motion: true,
            rates: (0.0, 0.0),
            changed: false,
            focused: false,
            tab_key_frame: u64::MAX,
            refocus: None,
        }
    }
}

/// what the corner of the screen shows while playing
pub struct Status {
    /// frames shown a second and the speed against the console's, when asked for
    pub fps: Option<(f32, f32)>,
    /// the game runs (not paused, nor waiting on Configure or its keyboard)
    pub playing: bool,
    pub paused: bool,
    pub fast: bool,
}

/// RE:DEX's red, the gear's in its icon
pub const ACCENT: Color32 = Color32::from_rgb(0xE4, 0x34, 0x3F);
pub(crate) const ACCENT_HOVER: Color32 = Color32::from_rgb(0xF0, 0x4C, 0x56);
pub(crate) const NOTE: Color32 = Color32::from_rgb(232, 176, 92);

/// the theme's own colours on top of egui's
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) window: Color32,
    pub(crate) group: Color32,
    pub(crate) line: Color32,
    pub(crate) text: Color32,
    pub(crate) weak: Color32,
    pub(crate) control: Color32,
    pub(crate) control_hover: Color32,
}

pub(crate) fn palette(dark: bool) -> Palette {
    if dark {
        // the icon's near-black, lifted a little
        Palette {
            window: Color32::from_rgb(0x17, 0x1A, 0x1E),
            group: Color32::from_rgb(0x1F, 0x23, 0x28),
            line: Color32::from_rgb(0x34, 0x39, 0x41),
            text: Color32::from_rgb(0xE8, 0xEA, 0xEC),
            weak: Color32::from_rgb(0x9A, 0xA1, 0xA9),
            control: Color32::from_rgb(0x2B, 0x30, 0x37),
            control_hover: Color32::from_rgb(0x36, 0x3C, 0x45),
        }
    } else {
        // the icon's light greys
        Palette {
            window: Color32::from_rgb(0xF4, 0xF4, 0xF2),
            group: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            line: Color32::from_rgb(0xD3, 0xD6, 0xD9),
            text: Color32::from_rgb(0x1B, 0x1E, 0x22),
            weak: Color32::from_rgb(0x5F, 0x66, 0x6E),
            control: Color32::from_rgb(0xE6, 0xE8, 0xEA),
            control_hover: Color32::from_rgb(0xDA, 0xDD, 0xE0),
        }
    }
}

fn is_dark(ui: &egui::Ui) -> bool {
    ui.visuals().dark_mode
}

/// the look: dark or light like Windows (or as chosen), RE:DEX's red on what's selected, roomy, rounded
pub fn style(ctx: &egui::Context, dark: bool, motion: bool) {
    let colors = palette(dark);
    let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    visuals.window_fill = colors.window;
    visuals.panel_fill = colors.window;
    visuals.extreme_bg_color = if dark { Color32::from_rgb(0x12, 0x14, 0x17) } else { Color32::WHITE };
    visuals.faint_bg_color = colors.group;
    visuals.window_stroke = Stroke::new(1.0, colors.line);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.override_text_color = Some(colors.text);
    visuals.weak_text_color = Some(colors.weak);
    visuals.hyperlink_color = ACCENT;
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.5, if dark { Color32::WHITE } else { Color32::BLACK });
    visuals.slider_trailing_fill = true;
    visuals.window_shadow = egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if dark { 150 } else { 60 }) };
    visuals.popup_shadow = egui::Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(if dark { 120 } else { 40 }) };
    let widgets = &mut visuals.widgets;
    for (state, fill) in [(&mut widgets.inactive, colors.control), (&mut widgets.hovered, colors.control_hover), (&mut widgets.active, colors.control_hover), (&mut widgets.open, colors.control_hover)] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.corner_radius = CornerRadius::same(6);
        state.fg_stroke.color = colors.text;
    }
    widgets.inactive.bg_stroke = Stroke::new(1.0, colors.line);
    widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.8));
    widgets.active.bg_stroke = Stroke::new(1.5, ACCENT);
    widgets.open.bg_stroke = Stroke::new(1.0, ACCENT);
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, colors.line);
    widgets.noninteractive.fg_stroke.color = colors.text;
    let theme = if dark { egui::Theme::Dark } else { egui::Theme::Light };
    ctx.set_visuals_of(theme, visuals);
    ctx.set_theme(theme);
    ctx.all_styles_mut(|style| {
        use egui::{FontId, TextStyle};
        style.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
        style.text_styles.insert(TextStyle::Button, FontId::proportional(14.0));
        style.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
        style.text_styles.insert(TextStyle::Heading, FontId::proportional(19.0));
        style.spacing.item_spacing = Vec2::new(8.0, 7.0);
        style.spacing.button_padding = Vec2::new(12.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        style.spacing.slider_width = 200.0;
        style.spacing.combo_width = 230.0;
        style.spacing.window_margin = egui::Margin::same(14);
        style.animation_time = if motion { 0.12 } else { 0.0 };
    });
}

/// eases out, fast then settling
pub(crate) fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

impl Menus {
    /// how long an animation takes, none w/ Windows' animations off
    fn time(&self, seconds: f32) -> f32 {
        if self.motion { seconds } else { 0.0 }
    }

    /// how far along an animation started at since is, 0 to 1 and on, done at once w/o motion
    fn progress(&self, since: Instant, seconds: f32) -> f32 {
        if !self.motion {
            return 1.0;
        }
        since.elapsed().as_secs_f32() / seconds
    }

    /// is something open that stops the game while it is
    pub fn over_game(&self) -> bool {
        self.configure_open
    }

    /// a short note in the corner, the newest under the others
    pub fn toast(&mut self, text: impl Into<String>) {
        self.add_toast(text.into(), false);
    }

    /// a note that pulses once as it comes in, for the speed changing
    pub fn pulse(&mut self, text: impl Into<String>) {
        self.add_toast(text.into(), true);
    }

    fn add_toast(&mut self, text: String, pulse: bool) {
        // the same kind of note again replaces the one showing instead of stacking up
        let kind = |text: &str| text.split(':').next().unwrap_or_default().to_owned();
        self.toasts.retain(|toast| kind(&toast.text) != kind(&text));
        self.toasts.push(Toast { text, born: Instant::now(), id: self.next_toast, pulse });
        self.next_toast += 1;
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    /// a camera flash over the window, for a screenshot
    pub fn flash(&mut self) {
        self.flash = Some(Instant::now());
    }

    /// a quick fade in after the screens moved, so a new layout doesn't just jump
    pub fn dip(&mut self) {
        self.dip = Some(Instant::now());
    }

    /// shakes a binding that just lost its key to another
    pub fn shake(&mut self, index: usize) {
        self.shake = Some((index, Instant::now()));
    }

    /// what goes over a running game: the paused look, fast forward, the frame rate pill, toasts and flashes
    pub fn game(&mut self, ctx: &egui::Context, status: &Status) {
        let screen = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Background, Id::new("game overlay")));
        let mut busy = false;

        // the screens' new place fades in from black
        if let Some(since) = self.dip {
            let t = self.progress(since, 0.2);
            if t < 1.0 {
                painter.rect_filled(screen, 0.0, Color32::from_black_alpha(((1.0 - ease(t)) * 255.0) as u8));
                busy = true;
            } else {
                self.dip = None;
            }
        }

        // paused, the game dims w/ a soft vignette and says so
        let paused = ctx.animate_bool_with_time(Id::new("paused"), status.paused && !self.configure_open, self.time(QUICK));
        if paused > 0.0 {
            vignette(&painter, screen, paused);
            let label = RichText::new("Paused").size(30.0).strong().color(Color32::WHITE.gamma_multiply(paused));
            let hint = RichText::new("Esc to resume").size(14.0).color(Color32::from_white_alpha((200.0 * paused) as u8));
            egui::Area::new(Id::new("paused label")).anchor(egui::Align2::CENTER_CENTER, Vec2::new(0.0, (1.0 - paused) * 8.0)).interactable(false).show(ctx, |ui| {
                egui::Frame::new().fill(Color32::from_black_alpha((150.0 * paused) as u8)).corner_radius(16).inner_margin(egui::Margin::symmetric(28, 12)).show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(label);
                        ui.label(hint);
                    });
                });
            });
        }

        // fast forward, two arrows chasing each other
        let fast = ctx.animate_bool_with_time(Id::new("fast"), status.fast, self.time(0.12));
        if fast > 0.0 {
            let rect = Rect::from_min_size(Pos2::new(screen.right() - 66.0, screen.top() + 10.0), Vec2::new(56.0, 26.0));
            painter.rect_filled(rect, 13.0, Color32::from_black_alpha((170.0 * fast) as u8));
            let time = ctx.input(|input| input.time) as f32;
            for i in 0..2 {
                let phase = if self.motion { ((time * 6.0 - i as f32 * 1.2).sin() * 0.5 + 0.5) * 0.6 + 0.4 } else { 1.0 };
                let x = rect.left() + 17.0 + i as f32 * 12.0;
                let y = rect.center().y;
                let color = Color32::WHITE.gamma_multiply(fast * phase);
                painter.add(egui::Shape::convex_polygon(vec![Pos2::new(x, y - 6.0), Pos2::new(x + 10.0, y), Pos2::new(x, y + 6.0)], color, Stroke::NONE));
            }
            busy = true;
        }

        // the frame rate pill, fading w/ its setting, its numbers eased
        let shown = ctx.animate_bool_with_time(Id::new("fps"), status.fps.is_some(), self.time(QUICK));
        if let Some(rates) = status.fps {
            // eased, except the first numbers and the ones after a pause, which start from where the game is
            let k = if self.rates.0 == 0.0 || self.rates.1 == 0.0 { 1.0 } else { 0.08 };
            self.rates = (self.rates.0 + (rates.0 - self.rates.0) * k, self.rates.1 + (rates.1 - self.rates.1) * k);
        }
        if !status.playing {
            // a stopped game has no speed to show
            self.rates.1 = 0.0;
        }
        if shown > 0.0 {
            let speed = if status.playing { format!("{:.0}%", self.rates.1 * 100.0) } else { "paused".to_owned() };
            let text = format!("{:.0} FPS  \u{b7}  {speed}", self.rates.0);
            let galley = painter.layout_no_wrap(text, egui::FontId::monospace(13.0), Color32::WHITE.gamma_multiply(shown));
            let rect = Rect::from_min_size(Pos2::new(screen.left() + 10.0, screen.top() + 10.0), galley.size() + Vec2::new(34.0, 12.0));
            painter.rect_filled(rect, rect.height() / 2.0, Color32::from_black_alpha((175.0 * shown) as u8));
            painter.circle_filled(Pos2::new(rect.left() + 13.0, rect.center().y), 3.5, ACCENT.gamma_multiply(shown));
            painter.galley(Pos2::new(rect.left() + 24.0, rect.top() + 6.0), galley, Color32::WHITE);
        }

        busy |= self.toasts(ctx);

        // a screenshot's flash
        if let Some(since) = self.flash {
            let t = self.progress(since, 0.22);
            if t < 1.0 {
                let top = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("flash")));
                top.rect_filled(screen, 0.0, Color32::from_white_alpha(((1.0 - ease(t)) * 200.0) as u8));
                busy = true;
            } else {
                self.flash = None;
            }
        }
        if busy {
            ctx.request_repaint();
        }
    }

    /// the toasts, stacked up from the bottom right, sliding in and out; true while any shows
    fn toasts(&mut self, ctx: &egui::Context) -> bool {
        let screen = ctx.content_rect();
        self.toasts.retain(|toast| toast.born.elapsed().as_secs_f32() < TOAST_LIFE);
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("toasts")));
        let mut bottom = screen.bottom() - 12.0;
        for toast in self.toasts.iter().rev() {
            let age = toast.born.elapsed().as_secs_f32();
            let (come, go) = if self.motion { (ease(age / QUICK), ease((age - (TOAST_LIFE - QUICK)) / QUICK)) } else { (1.0, 0.0) };
            let shown = come * (1.0 - go);
            let font = if toast.pulse && self.motion && age < 0.3 { 15.0 + 2.5 * (age / 0.3 * std::f32::consts::PI).sin() } else { 15.0 };
            let galley = painter.layout_no_wrap(toast.text.clone(), egui::FontId::proportional(font), Color32::WHITE.gamma_multiply(shown));
            let size = galley.size() + Vec2::new(36.0, 16.0);
            // the others make room smoothly as one comes or goes
            let top = ctx.animate_value_with_time(Id::new(("toast y", toast.id)), bottom - size.y, self.time(0.15));
            bottom -= size.y + 8.0;
            let slide = (1.0 - come + go) * 48.0;
            let rect = Rect::from_min_size(Pos2::new(screen.right() - 12.0 - size.x + slide, top), size);
            painter.rect_filled(rect, 10.0, Color32::from_rgba_unmultiplied(0x17, 0x1A, 0x1E, (230.0 * shown) as u8));
            painter.rect_filled(Rect::from_min_size(rect.min + Vec2::new(0.0, 8.0), Vec2::new(3.0, size.y - 16.0)), 2.0, ACCENT.gamma_multiply(shown));
            painter.galley(rect.min + Vec2::new(18.0, 8.0), galley, Color32::WHITE);
        }
        !self.toasts.is_empty()
    }

    /// the Configure window, opening and closing w/ a fade and a slight scale; booted is what the running game started
    /// w/, to tell what waits for a restart
    pub fn configure(&mut self, ctx: &egui::Context, settings: &mut Settings, booted: &Settings) -> Vec<Action> {
        let mut actions = Vec::new();
        let before = settings.clone();
        let shown = ctx.animate_bool_with_time(Id::new("configure shown"), self.configure_open, self.time(QUICK));
        if !self.configure_open {
            self.focused = false;
        }
        if shown == 0.0 {
            if std::mem::take(&mut self.changed) {
                actions.push(Action::Save);
                self.toast("Settings saved");
            }
            return actions;
        }
        let t = ease(shown);
        let screen = ctx.content_rect();
        // the game dims behind the window, it's paused meanwhile
        ctx.layer_painter(egui::LayerId::background()).rect_filled(screen, 0.0, Color32::from_black_alpha((150.0 * t) as u8));
        // taller than a short screen has room for, so it scrolls instead of running past the bottom
        let room = (screen.height() - 16.0).max(120.0);
        let width = (screen.width() - 16.0).clamp(280.0, 640.0);
        let layer = egui::LayerId::new(egui::Order::Middle, Id::new("configure"));
        let scale = 0.95 + 0.05 * t;
        ctx.set_transform_layer(layer, egui::emath::TSTransform::new(screen.center().to_vec2() * (1.0 - scale), scale));
        let mut keep = true;
        let frame = egui::Frame::window(&ctx.global_style()).multiply_with_opacity(t);
        egui::Window::new("Configure")
            .id(Id::new("configure"))
            .open(&mut keep)
            .frame(frame)
            .collapsible(false)
            .resizable(false)
            .interactable(self.configure_open)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size([width, 0.0])
            .max_height(room)
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                if self.tabs(ui) {
                    self.rebinding = None;
                    self.rebinding_pad = None;
                }
                ui.add_space(4.0);
                let restart = waits_for_restart(settings, booted);
                // the tabs scroll, the note and Close stay in sight under them
                let below = if restart { 84.0 } else { 48.0 };
                // every tab the same height, so the window stays put going from one to another
                let height = (room.min(700.0) - 150.0 - below).max(60.0);
                let fade = ease(self.progress(self.tab_since, 0.16));
                if fade < 1.0 {
                    ui.ctx().request_repaint();
                }
                egui::ScrollArea::both().min_scrolled_height(height).max_height(height).auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
                    // laid out to the visible width: a sideways scroll otherwise hands the tabs last frame's width, so
                    // they grow past the window's edge; only what can't get narrower than this scrolls sideways
                    ui.set_max_width(viewport.width());
                    // the new tab fades in, rising a few pixels
                    ui.multiply_opacity(fade);
                    ui.add_space((1.0 - fade) * 10.0);
                    match self.tab {
                        Tab::General => general(ui, settings),
                        Tab::Graphics => graphics(ui, settings, booted),
                        Tab::Audio => audio(ui, settings),
                        Tab::Controls => self.controls(ui, settings),
                    }
                });
                ui.add_space(2.0);
                ui.separator();
                if restart {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("\u{25cf} Some changes apply after a restart.").color(NOTE));
                        if button(ui, "Restart now", true).on_hover_text("Starts the game again. Unsaved progress is lost.").clicked() {
                            actions.push(Action::Restart);
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Changes apply and are saved as you make them.").weak().small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, "Close", false).clicked() {
                            self.configure_open = false;
                        }
                    });
                });
            });
        self.configure_open &= keep;
        if !self.configure_open {
            self.rebinding = None;
            self.rebinding_pad = None;
        }
        if *settings != before {
            self.changed = true;
            actions.push(Action::Settings);
        }
        actions
    }

    /// the tab bar, the accent's underline gliding to the chosen tab; true when the tab changed
    fn tabs(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        // Ctrl+Tab and Ctrl+Shift+Tab go thru the tabs from anywhere in the window, Left and Right too while the
        // keyboard is on the tab bar (or on nothing), going round
        let at = TABS.iter().position(|(tab, _)| *tab == self.tab).unwrap_or(0);
        if let Some(i) = self.refocus.take() {
            ui.memory_mut(|memory| memory.request_focus(Id::new(("tab", i))));
        }
        let focus = ui.memory(|memory| memory.focused());
        let on_bar = focus.is_none_or(|id| (0..TABS.len()).any(|i| id == Id::new(("tab", i))));
        let step = ui.input_mut(|input| {
            let ctrl = egui::Modifiers::CTRL;
            if input.consume_key(ctrl | egui::Modifiers::SHIFT, egui::Key::Tab) || (on_bar && input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft)) {
                Some(TABS.len() - 1)
            } else if input.consume_key(ctrl, egui::Key::Tab) || (on_bar && input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight)) {
                Some(1)
            } else {
                None
            }
        });
        let frame = ui.ctx().cumulative_frame_nr();
        if let Some(step) = step.filter(|_| self.tab_key_frame != frame) {
            self.tab_key_frame = frame;
            let next = (at + step) % TABS.len();
            self.tab = TABS[next].0;
            self.tab_since = Instant::now();
            ui.memory_mut(|memory| memory.request_focus(Id::new(("tab", next))));
            self.refocus = Some(next);
            changed = true;
        }
        let colors = palette(is_dark(ui));
        let height = 34.0;
        let (bar, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height + 6.0), Sense::hover());
        let width = (bar.width() / TABS.len() as f32).min(130.0);
        let mut chosen = Rect::NOTHING;
        for (i, (tab, name)) in TABS.iter().enumerate() {
            let rect = Rect::from_min_size(bar.min + Vec2::new(i as f32 * width, 0.0), Vec2::new(width, height));
            let response = ui.interact(rect, Id::new(("tab", i)), Sense::click());
            // the chosen tab has the keyboard when the window opens, Tab and the arrows go on from it
            if !self.focused && self.tab == *tab && self.configure_open {
                response.request_focus();
                self.focused = true;
            }
            let hover = ui.ctx().animate_bool_with_time(response.id, response.hovered(), self.time(0.12));
            if hover > 0.0 {
                ui.painter().rect_filled(rect.shrink2(Vec2::new(3.0, 2.0)), 7.0, colors.control.gamma_multiply(hover));
            }
            let selected = self.tab == *tab;
            let color = if selected { colors.text } else { colors.weak.lerp_to_gamma(colors.text, hover) };
            let font = egui::FontId::proportional(15.0);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, *name, font, color);
            if response.clicked() && !selected {
                self.tab = *tab;
                self.tab_since = Instant::now();
                changed = true;
            }
            if self.tab == *tab {
                chosen = rect;
            }
            if response.has_focus() {
                ui.painter().rect_stroke(rect.shrink(2.0), 7.0, Stroke::new(1.5, ACCENT), egui::StrokeKind::Inside);
            }
        }
        // the underline glides over
        let left = ui.ctx().animate_value_with_time(Id::new("tab line left"), chosen.left() + 18.0, self.time(QUICK));
        let right = ui.ctx().animate_value_with_time(Id::new("tab line right"), chosen.right() - 18.0, self.time(QUICK));
        let y = bar.top() + height;
        ui.painter().rect_filled(Rect::from_min_max(Pos2::new(bar.left(), y + 2.0), Pos2::new(bar.right(), y + 3.0)), 0.0, colors.line);
        ui.painter().rect_filled(Rect::from_min_max(Pos2::new(left, y), Pos2::new(right, y + 3.0)), 2.0, ACCENT);
        changed
    }

    fn controls(&mut self, ui: &mut egui::Ui, settings: &mut Settings) {
        let time = ui.ctx().input(|input| input.time) as f32;
        let motion = self.motion;
        let shake = self.shake.filter(|(_, since)| since.elapsed().as_secs_f32() < 0.3 && motion);
        if shake.is_some() || self.rebinding.is_some() || self.rebinding_pad.is_some() {
            ui.ctx().request_repaint();
        }
        let keys: Vec<KeyCode> = settings.keys.clone().all_mut().into_iter().map(|(_, key)| *key).collect();
        let names: Vec<&str> = settings.keys.clone().all_mut().into_iter().map(|(name, _)| name).collect();
        let rebinding = self.rebinding;
        let mut clicked = None;
        group(ui, "Keyboard", |ui| {
            ui.label(RichText::new("Click a button, then press the key for it. A key already in use moves over.").weak().small());
            // two columns side by side where they fit, one under the other where the window is narrow or the UI big
            let (count, name, width) = binding_layout(ui.available_width(), widest(ui, &names), ui.spacing().item_spacing.x);
            let per = keys.len().div_ceil(count);
            ui.columns(count, |columns| {
                for (column, ui) in columns.iter_mut().enumerate() {
                    egui::Grid::new(("keys", column)).num_columns(2).spacing([10.0, 5.0]).min_col_width(name).show(ui, |ui| {
                        for i in (column * per)..(column * per + per).min(keys.len()) {
                            ui.label(names[i]);
                            let waiting = rebinding == Some(i);
                            let others: Vec<&str> = (0..keys.len()).filter(|&j| j != i && keys[j] == keys[i]).map(|j| names[j]).collect();
                            let text = if waiting { "Press a key\u{2026} (Esc to cancel)".to_owned() } else { key_name(keys[i]) };
                            let offset = shake.filter(|(at, _)| *at == i).map_or(0.0, |(_, since)| {
                                let t = since.elapsed().as_secs_f32();
                                (t * 60.0).sin() * 5.0 * (1.0 - t / 0.3)
                            });
                            let mut response = binding(ui, width, &text, waiting, !others.is_empty(), time, offset);
                            if !others.is_empty() {
                                response = response.on_hover_text(format!("Also on {}", others.join(", ")));
                            }
                            if response.clicked() {
                                clicked = Some(i);
                            }
                            ui.end_row();
                        }
                    });
                }
            });
            egui::Grid::new(("players binding", "keys")).num_columns(2).spacing([10.0, 5.0]).min_col_width(name).show(ui, |ui| {
                ui.label("Players list");
                let waiting = rebinding == Some(PLAYERS);
                let key = settings.keys.players;
                let text = if waiting {
                    "Press a key\u{2026} (Esc to cancel)".to_owned()
                } else if key == KeyCode::Tab {
                    "Tab (tap)".to_owned()
                } else {
                    key_name(key)
                };
                let r = binding(ui, width, &text, waiting, false, time, 0.0).on_hover_text("Opens the players near you. On Tab a tap opens it and holding Tab still fast forwards.");
                if r.clicked() {
                    clicked = Some(PLAYERS);
                }
                ui.end_row();
            });
            ui.label(RichText::new("Drag with the right mouse button to tilt the console. Click the bottom screen to touch it.").weak().small());
        });
        if let Some(i) = clicked {
            self.rebinding = Some(i);
            self.rebinding_pad = None;
        }

        let buttons: Vec<gilrs::Button> = settings.pad.clone().all_mut().into_iter().map(|(_, button)| *button).collect();
        let pad_names: Vec<&str> = settings.pad.clone().all_mut().into_iter().map(|(name, _)| name).collect();
        let rebinding_pad = self.rebinding_pad;
        let mut clicked = None;
        group(ui, "Controller", |ui| {
            let (count, name, width) = binding_layout(ui.available_width(), widest(ui, &pad_names), ui.spacing().item_spacing.x);
            let per = buttons.len().div_ceil(count);
            ui.columns(count, |columns| {
                for (column, ui) in columns.iter_mut().enumerate() {
                    egui::Grid::new(("pad", column)).num_columns(2).spacing([10.0, 5.0]).min_col_width(name).show(ui, |ui| {
                        for i in (column * per)..(column * per + per).min(buttons.len()) {
                            ui.label(pad_names[i]);
                            let waiting = rebinding_pad == Some(i);
                            let conflict = buttons.iter().enumerate().any(|(j, button)| j != i && *button == buttons[i]);
                            let text = if waiting { "Press a button\u{2026} (Esc to cancel)".to_owned() } else { button_name(buttons[i]).to_owned() };
                            if binding(ui, width, &text, waiting, conflict, time, 0.0).clicked() {
                                clicked = Some(i);
                            }
                            ui.end_row();
                        }
                    });
                }
            });
            egui::Grid::new(("players binding", "pad")).num_columns(2).spacing([10.0, 5.0]).min_col_width(name).show(ui, |ui| {
                ui.label("Players list");
                let waiting = rebinding_pad == Some(PLAYERS);
                let shared = settings.pad.clone().all_mut().into_iter().find(|(_, b)| **b == settings.pad.players).map(|(name, _)| name);
                let text = if waiting { "Press a button\u{2026} (Esc to cancel)".to_owned() } else { button_name(settings.pad.players).to_owned() };
                let r = binding(ui, width, &text, waiting, false, time, 0.0)
                    .on_hover_text(match shared {
                        Some(console) => format!("Also the console's {console}: the game never gets it while this opens the list. Pick a free button."),
                        None => "Opens the players near you; the d-pad, A and B work the list. Console buttons and Home cannot be used for it (the left stick click is the default).".to_owned(),
                    });
                if r.clicked() {
                    clicked = Some(PLAYERS);
                }
                ui.end_row();
            });
            ui.label(RichText::new("The circle pad is the left stick. Home opens and closes this window. ZR (or the right stick) held is fast forward.").weak().small());
        });
        if let Some(i) = clicked {
            self.rebinding_pad = Some(i);
            self.rebinding = None;
        }
        ui.add_space(2.0);
        if button(ui, "Reset to defaults", false).on_hover_text("Both the keyboard and the controller.").clicked() {
            settings.keys = Keys::default();
            settings.pad = PadButtons::default();
            self.rebinding = None;
            self.rebinding_pad = None;
        }
    }

    /// the keyboard a game opened to get text typed in, it waits till a button closes it
    pub fn keyboard(&mut self, ctx: &egui::Context, request: &Request) -> Option<Action> {
        let fresh = self.typing.as_ref().is_none_or(|(asked, _)| asked != request);
        if fresh {
            self.typing = Some((request.clone(), request.text.clone()));
        }
        let (_, text) = self.typing.as_mut()?;
        let confirm = request.buttons.len() - 1;
        let mut action = None;
        egui::Window::new("Keyboard").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            let field = ui.add(egui::TextEdit::singleline(text).hint_text(&request.hint).char_limit(request.max_length).desired_width(260.0));
            if fresh {
                field.request_focus();
            }
            let valid = request.accepts(text);
            ui.horizontal(|ui| {
                for (button, label) in request.buttons.iter().enumerate() {
                    if ui.add_enabled(button != confirm || valid, egui::Button::new(label)).clicked() {
                        action = Some(Action::Keyboard(text.clone(), button));
                    }
                }
            });
            // enter confirms, when the text will do
            if valid && field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                action = Some(Action::Keyboard(text.clone(), confirm));
            }
        });
        if action.is_some() {
            self.typing = None;
        }
        action
    }

    /// a message box, when there's something to tell
    pub fn message(&mut self, ctx: &egui::Context) {
        let Some(text) = self.message.clone() else { return };
        let room = (ctx.content_rect().height() - 120.0).max(60.0);
        egui::Window::new("RE:DEX")
            .id(Id::new("message"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(room).show(ui, |ui| ui.label(text.as_str()));
                if button(ui, "OK", true).clicked() {
                    self.message = None;
                }
            });
    }
}

/// the window darkened toward its edges, deeper w/ t
fn vignette(painter: &egui::Painter, screen: Rect, t: f32) {
    let edge = Color32::from_black_alpha((175.0 * t) as u8);
    let middle = Color32::from_black_alpha((70.0 * t) as u8);
    let inner = screen.shrink2(screen.size() * 0.28);
    let outer = [screen.left_top(), screen.right_top(), screen.right_bottom(), screen.left_bottom()];
    let inside = [inner.left_top(), inner.right_top(), inner.right_bottom(), inner.left_bottom()];
    let mut mesh = egui::Mesh::default();
    // a ring from the edge in, and the middle
    for i in 0..4 {
        mesh.colored_vertex(outer[i], edge);
        mesh.colored_vertex(inside[i], middle);
    }
    for i in 0..4u32 {
        let (o, n, o2, n2) = (i * 2, i * 2 + 1, (i * 2 + 2) % 8, (i * 2 + 3) % 8);
        mesh.add_triangle(o, o2, n);
        mesh.add_triangle(n, o2, n2);
    }
    mesh.add_triangle(1, 3, 5);
    mesh.add_triangle(1, 5, 7);
    painter.add(mesh);
}

/// a button w/ hover and press easing in, the accent filled when primary, a focus ring w/ the keyboard on it
pub(crate) fn button(ui: &mut egui::Ui, text: &str, primary: bool) -> egui::Response {
    let colors = palette(is_dark(ui));
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(14.0), colors.text);
    let size = Vec2::new((galley.size().x + 28.0).max(84.0), 30.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let motion = ui.style().animation_time > 0.0;
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), if motion { 0.12 } else { 0.0 });
    let press = ui.ctx().animate_bool_with_time(response.id.with("press"), response.is_pointer_button_down_on(), if motion { 0.06 } else { 0.0 });
    let (rest, lit) = if primary { (ACCENT, ACCENT_HOVER) } else { (colors.control, colors.control_hover) };
    let fill = rest.lerp_to_gamma(lit, hover).gamma_multiply(1.0 - 0.12 * press);
    let rect = rect.shrink(press);
    let stroke = if primary { ACCENT } else { colors.line.lerp_to_gamma(ACCENT, hover * 0.6) };
    ui.painter().rect(rect, 7.0, fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    let color = if primary { Color32::WHITE } else { colors.text };
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    if response.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.5), 9.0, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
    }
    response
}

/// a binding's button: pulses the accent while it waits for a key, flags a key on two buttons, shakes by offset
fn binding(ui: &mut egui::Ui, width: f32, text: &str, waiting: bool, conflict: bool, time: f32, offset: f32) -> egui::Response {
    let colors = palette(is_dark(ui));
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::click());
    let motion = ui.style().animation_time > 0.0;
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), if motion { 0.12 } else { 0.0 });
    let rect = rect.translate(Vec2::new(offset, 0.0));
    let fill = if waiting {
        let pulse = if motion { (time * 5.0).sin() * 0.5 + 0.5 } else { 0.5 };
        colors.control.lerp_to_gamma(ACCENT, 0.3 + 0.45 * pulse)
    } else {
        colors.control.lerp_to_gamma(colors.control_hover, hover)
    };
    let stroke = if conflict { Stroke::new(1.5, NOTE) } else { Stroke::new(1.0, colors.line.lerp_to_gamma(ACCENT, hover * 0.6)) };
    ui.painter().rect(rect, 6.0, fill, stroke, egui::StrokeKind::Inside);
    let size = if waiting { 12.0 } else { 13.5 };
    let color = if conflict { NOTE } else if waiting { Color32::WHITE } else { colors.text };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(size), color);
    if response.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.0), 8.0, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
    }
    response
}

/// the bindings' columns in this much room, names this wide: how many (two side by side when both fit, one under the
/// other otherwise), how wide each name column is and each binding's button, so nothing runs past the group's edge
fn binding_layout(available: f32, name: f32, gap: f32) -> (usize, f32, f32) {
    const GRID_GAP: f32 = 10.0;
    const BUTTON: f32 = 120.0;
    let name = name.max(60.0);
    let count = if available >= 2.0 * (name + GRID_GAP + BUTTON) + gap { 2 } else { 1 };
    let column = (available - gap * (count - 1) as f32) / count as f32;
    (count, name, (column - name - GRID_GAP - 2.0).clamp(60.0, 200.0))
}

/// the widest of some labels as the body font draws them
fn widest(ui: &egui::Ui, names: &[&str]) -> f32 {
    names
        .iter()
        .map(|name| egui::WidgetText::from(*name).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body).size().x)
        .fold(0.0, f32::max)
}

/// an on/off switch, its knob sliding over and the accent filling in
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let colors = palette(is_dark(ui));
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(42.0, 22.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let enabled = ui.is_enabled();
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, *on, ""));
    let motion = ui.style().animation_time > 0.0;
    let t = ui.ctx().animate_bool_with_time(response.id, *on, if motion { 0.14 } else { 0.0 });
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), if motion { 0.12 } else { 0.0 });
    let off = colors.control.lerp_to_gamma(colors.control_hover, hover);
    let fill = off.lerp_to_gamma(ACCENT.lerp_to_gamma(ACCENT_HOVER, hover), t);
    let radius = rect.height() / 2.0;
    ui.painter().rect(rect, radius, fill, Stroke::new(1.0, colors.line.lerp_to_gamma(ACCENT, t.max(hover * 0.6))), egui::StrokeKind::Inside);
    let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    let knob = if is_dark(ui) || t > 0.5 { Color32::WHITE } else { Color32::from_rgb(0x8A, 0x90, 0x97) };
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), radius - 4.0 + hover, knob);
    if response.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.5), radius + 2.5, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
    }
    response
}

/// do settings differ from booted in what only takes effect when the game starts
pub fn waits_for_restart(settings: &Settings, booted: &Settings) -> bool {
    settings.renderer != booted.renderer
        || settings.hardware_rasterizer != booted.hardware_rasterizer
        || settings.resolution != booted.resolution
        || settings.texture_packs != booted.texture_packs
        || settings.language != booted.language
}

/// a group box like Azahar's: a rounded panel w/ its title on top
fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let colors = palette(is_dark(ui));
    ui.add_space(4.0);
    egui::Frame::new().fill(colors.group).stroke(Stroke::new(1.0, colors.line)).corner_radius(10).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().size(15.0).color(colors.text));
        ui.add_space(2.0);
        add(ui);
    });
    ui.add_space(4.0);
}

/// the labels' column, wide enough for the longest so every tab's controls line up
const LABEL_WIDTH: f32 = 240.0;

/// a form of label and control rows, the labels lined up; a narrow window (or a big UI scale) narrows both columns
/// so nothing runs past the edge
fn form(ui: &mut egui::Ui, id: &str, rows: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    let label = label_width(width);
    let control = (width - label - 24.0).clamp(120.0, 230.0);
    ui.spacing_mut().combo_width = control;
    ui.spacing_mut().slider_width = (control - 60.0).max(80.0);
    egui::Grid::new(id).num_columns(2).spacing([16.0, 9.0]).min_col_width(label).max_col_width(label.max(control)).show(ui, rows);
}

/// the labels' column for a form this wide
fn label_width(width: f32) -> f32 {
    (width * 0.44).min(LABEL_WIDTH)
}

/// a row's label, w/ the note that it waits for a restart when it does
fn label(ui: &mut egui::Ui, text: &str, restart: bool) {
    ui.horizontal(|ui| {
        ui.label(text);
        if restart {
            ui.label(RichText::new("applies after restart").small().color(NOTE));
        }
    });
}

fn general(ui: &mut egui::Ui, settings: &mut Settings) {
    group(ui, "Emulation", |ui| {
        form(ui, "general", |ui| {
            ui.label("Limit speed");
            ui.horizontal(|ui| {
                toggle(ui, &mut settings.limit_speed).on_hover_text("Off runs the game as fast as the computer can (Unlocked), with no sound, like holding Tab.");
                if settings.limit_speed {
                    // the switch before it takes room from the slider, so the value box stays whole
                    ui.spacing_mut().slider_width = (ui.spacing().slider_width - 50.0).max(50.0);
                    ui.add(egui::Slider::new(&mut settings.speed_limit, SPEED_MIN..=SPEED_MAX).step_by(10.0).suffix("%"))
                        .on_hover_text("The game's speed against the console's. Below 100% the sound is stretched to match; above it the sound is off, as with fast forward.");
                } else {
                    ui.label(RichText::new("Unlocked").color(ACCENT));
                }
            });
            ui.end_row();
            label(ui, "System language", false);
            language(ui, &mut settings.language);
            ui.end_row();
        });
    });
    group(ui, "Interface", |ui| {
        form(ui, "interface", |ui| {
            ui.label("Theme");
            egui::ComboBox::from_id_salt("theme").selected_text(settings.theme.name()).show_ui(ui, |ui| {
                for theme in Theme::ALL {
                    ui.selectable_value(&mut settings.theme, theme, theme.name());
                }
            });
            ui.end_row();
            ui.label("Show FPS and speed");
            toggle(ui, &mut settings.show_fps);
            ui.end_row();
            ui.label("Show player names");
            toggle(ui, &mut settings.show_names).on_hover_text("Names over the other players' heads, on the top screen.");
            ui.end_row();
        });
    });
    group(ui, "Hotkeys", |ui| {
        egui::Grid::new("hotkeys").num_columns(2).spacing([16.0, 5.0]).min_col_width(label_width(ui.available_width())).show(ui, |ui| {
            for (key, what) in [
                ("Esc / F1", "Pause and resume"),
                ("Tab (hold)", "Fast forward"),
                ("Tab (tap)", "Players nearby"),
                ("Left stick click (controller)", "Players nearby"),
                ("F9", "Next screen layout"),
                ("F10", "Swap screens"),
                ("F11", "Fullscreen"),
                ("F12", "Screenshot"),
                ("M (hold)", "Blow into the microphone"),
                ("Home (controller)", "Configure"),
            ] {
                ui.label(RichText::new(key).monospace());
                ui.label(RichText::new(what).weak());
                ui.end_row();
            }
        });
    });
}

/// the speed limit's range, in percent of the console's speed
/// the sound stretcher makes up to twice the time, but at half speed it sits at that limit and runs dry, so 60% is
/// the slowest that keeps the sound whole
pub const SPEED_MIN: u32 = 60;
pub const SPEED_MAX: u32 = 500;

fn graphics(ui: &mut egui::Ui, settings: &mut Settings, booted: &Settings) {
    group(ui, "Renderer", |ui| {
        form(ui, "renderer", |ui| {
            label(ui, "Graphics API", settings.renderer != booted.renderer);
            egui::ComboBox::from_id_salt("api")
                .selected_text(renderer_name(settings.renderer))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.renderer, Renderer::Vulkan, renderer_name(Renderer::Vulkan));
                    ui.selectable_value(&mut settings.renderer, Renderer::OpenGl, renderer_name(Renderer::OpenGl));
                })
                .response
                .on_hover_text("What shows the screens in the window. Vulkan is needed for smooth motion.");
            ui.end_row();
            label(ui, "3D rendering", settings.hardware_rasterizer != booted.hardware_rasterizer);
            egui::ComboBox::from_id_salt("rasterizer")
                .selected_text(rasterizer_name(settings.hardware_rasterizer))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.hardware_rasterizer, true, rasterizer_name(true));
                    ui.selectable_value(&mut settings.hardware_rasterizer, false, rasterizer_name(false));
                })
                .response
                .on_hover_text("The hardware rasterizer draws the 3D on the GPU (fast, upscaled). Software draws it on the CPU at the console's resolution.");
            ui.end_row();
            ui.label("VSync");
            toggle(ui, &mut settings.vsync)
                .on_hover_text("On: no tearing. Off: frames go to the screen right away, a little less latency, but the picture may tear. The game keeps its own pace either way.");
            ui.end_row();
        });
    });

    group(ui, "Enhancements", |ui| {
        form(ui, "enhancements", |ui| {
            label(ui, "Internal resolution", settings.resolution != booted.resolution);
            ui.add_enabled_ui(settings.hardware_rasterizer, |ui| resolution(ui, &mut settings.resolution));
            ui.end_row();
            label(ui, "Texture packs", settings.texture_packs != booted.texture_packs);
            ui.add_enabled_ui(settings.hardware_rasterizer, |ui| {
                toggle(ui, &mut settings.texture_packs).on_hover_text("Draw a texture pack made for Citra or Azahar, put in the game's mods folder.")
            });
            ui.end_row();
            ui.label("Smooth motion (frame interpolation)");
            // it shows the in between pictures straight from the renderer's device, which only the GPU 3D on Vulkan has
            let can = booted.hardware_rasterizer && booted.renderer == Renderer::Vulkan;
            ui.add_enabled_ui(can, |ui| {
                egui::ComboBox::from_id_salt("smooth motion")
                    .selected_text(settings.frame_interpolation.name())
                    .show_ui(ui, |ui| {
                        for smooth in SmoothMotion::ALL {
                            ui.selectable_value(&mut settings.frame_interpolation, smooth, smooth.name());
                        }
                    })
                    .response
                    .on_hover_text(
                        "The game runs at 30 frames a second; this draws pictures in between each two of its own, everything part of the way between where it was in both. \
                         60 fps: one in between, the screens show a 60th of a second late. \
                         Match monitor: one for each refresh of your monitor, up to 240 a second (seven in between), the screens about a 35th of a second late. \
                         On a 60 Hz monitor both are the same.",
                    )
            })
            .response
            .on_disabled_hover_text("Needs the GPU 3D and Vulkan, restart after choosing them.");
            ui.end_row();
        });
    });

    group(ui, "Layout", |ui| {
        form(ui, "layout", |ui| {
            ui.label("Screen layout");
            screens(ui, &mut settings.layout);
            ui.end_row();
            ui.label("Swap screens");
            toggle(ui, &mut settings.swap_screens).on_hover_text("The bottom screen where the top one goes. F10 switches it while playing.");
            ui.end_row();
            ui.label("Rotation");
            rotation(ui, &mut settings.orientation);
            ui.end_row();
            ui.label("Top screen shape");
            top_shape(ui, &mut settings.top_shape);
            ui.end_row();
            ui.label("Window size");
            ui.add(egui::Slider::new(&mut settings.scale, 1..=6).suffix("x")).on_hover_text("The window's size, times the console's screens.");
            ui.end_row();
            ui.label("Integer scaling");
            toggle(ui, &mut settings.integer_scale).on_hover_text("Every pixel gets the same size, with a border around the screens.");
            ui.end_row();
            ui.label("Output filter");
            filter(ui, &mut settings.filter);
            ui.end_row();
        });
    });

    group(ui, "Stereoscopy", |ui| {
        form(ui, "stereo", |ui| {
            ui.label("Stereoscopic 3D mode");
            stereo_3d(ui, &mut settings.stereo);
            ui.end_row();
            let stereo = settings.stereo != Stereo3d::Off;
            ui.label("Depth");
            ui.add_enabled_ui(stereo, |ui| {
                ui.add(egui::Slider::new(&mut settings.depth_3d, 0..=100).suffix("%")).on_hover_text("The console's 3D slider, how deep the game draws the 3D.")
            });
            ui.end_row();
            ui.label("Swap eyes");
            ui.add_enabled_ui(stereo, |ui| {
                toggle(ui, &mut settings.swap_eyes)
                    .on_hover_text("Each eye gets the other's picture, for viewing cross-eyed, glasses worn the other way round, or a monitor whose rows start with the other eye.")
            });
            ui.end_row();
        });
    });
}

fn audio(ui: &mut egui::Ui, settings: &mut Settings) {
    group(ui, "Output", |ui| {
        form(ui, "audio", |ui| {
            ui.label("Volume");
            ui.add_enabled(!settings.mute, egui::Slider::new(&mut settings.volume, 0.0..=1.0).custom_formatter(|value, _| format!("{:.0}%", value * 100.0)));
            ui.end_row();
            ui.label("Mute");
            toggle(ui, &mut settings.mute);
            ui.end_row();
        });
    });
    group(ui, "Microphone", |ui| {
        form(ui, "microphone", |ui| {
            ui.label("Input");
            egui::ComboBox::from_id_salt("microphone")
                .selected_text(settings.microphone.name())
                .show_ui(ui, |ui| {
                    for source in MicrophoneSource::ALL {
                        ui.selectable_value(&mut settings.microphone, source, source.name());
                    }
                })
                .response
                .on_hover_text("What the console's microphone hears. The computer's microphone is open only while the game listens. Holding M blows into it either way.");
            ui.end_row();
        });
    });
}

fn renderer_name(renderer: Renderer) -> &'static str {
    match renderer {
        Renderer::Vulkan => "Vulkan",
        Renderer::OpenGl => "OpenGL",
    }
}

fn rasterizer_name(hardware: bool) -> &'static str {
    if hardware { "Hardware (GPU)" } else { "Software (CPU)" }
}

/// a resolution as Azahar names it, the times and the top screen's size
pub fn resolution_name(times: u32) -> String {
    match times {
        1 => "Native 1x (400x240)".to_owned(),
        n => format!("{n}x ({}x{})", 400 * n, 240 * n),
    }
}

fn resolution(ui: &mut egui::Ui, value: &mut u32) {
    egui::ComboBox::from_id_salt("resolution")
        .selected_text(resolution_name(*value))
        .show_ui(ui, |ui| {
            for times in 1..=10 {
                ui.selectable_value(value, times, resolution_name(times));
            }
        })
        .response
        .on_hover_text("How many times the console's resolution the 3D is drawn at. Past what the window shows, the extra pixels smooth the edges, at a cost to the GPU that grows fast.");
}

fn screens(ui: &mut egui::Ui, value: &mut Screens) {
    egui::ComboBox::from_id_salt("screens").selected_text(value.name()).show_ui(ui, |ui| {
        for layout in Screens::ALL {
            ui.selectable_value(value, layout, layout.name());
        }
    });
}

fn rotation(ui: &mut egui::Ui, value: &mut Orientation) {
    egui::ComboBox::from_id_salt("rotation").selected_text(value.name()).show_ui(ui, |ui| {
        for orientation in Orientation::ALL {
            ui.selectable_value(value, orientation, orientation.name());
        }
    });
}

fn filter(ui: &mut egui::Ui, value: &mut Filter) {
    egui::ComboBox::from_id_salt("filter")
        .selected_text(value.name())
        .show_ui(ui, |ui| {
            for filter in Filter::ALL {
                ui.selectable_value(value, filter, filter.name());
            }
        })
        .response
        .on_hover_text("How the screens' pixels look scaled up to the window: smooth, sharp with soft edges, or plain squares.");
}

fn top_shape(ui: &mut egui::Ui, value: &mut TopShape) {
    egui::ComboBox::from_id_salt("top shape")
        .selected_text(value.name())
        .show_ui(ui, |ui| {
            for shape in TopShape::ALL {
                ui.selectable_value(value, shape, shape.name());
            }
        })
        .response
        .on_hover_text("Shows the top screen wider than the console's. The game still draws it at the console's shape, so this stretches it unless a widescreen code is on.");
}

fn stereo_3d(ui: &mut egui::Ui, value: &mut Stereo3d) {
    egui::ComboBox::from_id_salt("stereo")
        .selected_text(value.name())
        .show_ui(ui, |ui| {
            for stereo in Stereo3d::ALL {
                ui.selectable_value(value, stereo, stereo.name());
            }
        })
        .response
        .on_hover_text("The top screen in 3D: red and cyan for glasses of those colors, side by side for 3D TVs and headsets, alternate rows for passive 3D monitors. Off, the game draws flat, as with the console's 3D slider down.");
}

fn language(ui: &mut egui::Ui, value: &mut Language) {
    egui::ComboBox::from_id_salt("language")
        .selected_text(value.name())
        .show_ui(ui, |ui| {
            for language in Language::ALL {
                ui.selectable_value(value, language, language.name());
            }
        })
        .response
        .on_hover_text("The console's system language, which the game picks its text by. It changes when the game starts again.");
}

/// a key as a person would call it
pub fn key_name(key: KeyCode) -> String {
    let name = format!("{key:?}");
    for prefix in ["Key", "Digit", "Arrow"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return rest.to_owned();
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    /// runs frames of the Configure window on a screen of this size, at a tab, light or dark
    fn configure_on(size: Vec2, tab: Tab, dark: bool) -> (Rect, Rect) {
        let ctx = egui::Context::default();
        style(&ctx, dark, false);
        let mut menus = Menus { configure_open: true, tab, motion: false, ..Menus::default() };
        let mut settings = Settings::default();
        let booted = Settings { resolution: 3, ..Settings::default() };
        let screen = Rect::from_min_size(Pos2::ZERO, size);
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            ctx.run_ui(input, |ui| {
                menus.configure(ui.ctx(), &mut settings, &booted);
            })
            .drop_without_applying_deltas();
        }
        assert_eq!(settings, Settings::default(), "opening the window changes nothing");
        (ctx.memory(|memory| memory.area_rect(Id::new("configure"))).unwrap(), screen)
    }

    /// the settings stay inside a short screen, scrolling in it instead of running past its bottom, on every tab
    #[test]
    fn the_settings_fit_a_short_screen() {
        for tab in [Tab::General, Tab::Graphics, Tab::Audio, Tab::Controls] {
            for size in [Vec2::new(800.0, 360.0), Vec2::new(400.0, 480.0), Vec2::new(1600.0, 960.0)] {
                for dark in [true, false] {
                    let (window, screen) = configure_on(size, tab, dark);
                    assert!(screen.contains_rect(window), "{tab:?}: {window:?} in {screen:?}");
                }
            }
        }
    }

    /// the bindings go side by side only when names and buttons fit twice, and never run past the room they have
    #[test]
    fn bindings_stack_when_two_columns_dont_fit() {
        // 150% UI in an 800 px window leaves about 470 points, 100% about 590
        for (available, name, columns) in [(470.0, 110.0, 1), (590.0, 105.0, 2), (300.0, 110.0, 1), (900.0, 110.0, 2)] {
            let (count, name_width, button) = binding_layout(available, name, 8.0);
            assert_eq!(count, columns, "{available} wide");
            let column = (available - 8.0 * (count - 1) as f32) / count as f32;
            assert!(name_width + 10.0 + button <= column.max(name_width + 10.0 + 60.0), "{available}: {name_width} + {button} in {column}");
        }
    }

    /// Right on the tab bar goes to the next tab, one a press, and Ctrl+Tab from anywhere, going round
    #[test]
    fn the_keyboard_goes_thru_the_tabs() {
        let ctx = egui::Context::default();
        style(&ctx, true, false);
        let mut menus = Menus { configure_open: true, motion: false, ..Menus::default() };
        let mut settings = Settings::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 900.0));
        let key = |key, modifiers: egui::Modifiers| vec![
            egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers },
            egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers },
        ];
        let mut frame = |events: Vec<egui::Event>, menus: &mut Menus| {
            let input = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
            ctx.run_ui(input, |ui| drop(menus.configure(ui.ctx(), &mut settings, &Settings::default()))).drop_without_applying_deltas();
        };
        for _ in 0..3 {
            frame(Vec::new(), &mut menus);
        }
        let mut seen = Vec::new();
        for _ in 0..4 {
            frame(key(egui::Key::ArrowRight, egui::Modifiers::NONE), &mut menus);
            frame(Vec::new(), &mut menus);
            frame(Vec::new(), &mut menus);
            seen.push(menus.tab);
        }
        assert_eq!(seen, [Tab::Graphics, Tab::Audio, Tab::Controls, Tab::General]);
        frame(key(egui::Key::Tab, egui::Modifiers::CTRL), &mut menus);
        assert_eq!(menus.tab, Tab::Graphics);
        frame(key(egui::Key::Tab, egui::Modifiers::CTRL | egui::Modifiers::SHIFT), &mut menus);
        frame(key(egui::Key::Tab, egui::Modifiers::CTRL | egui::Modifiers::SHIFT), &mut menus);
        assert_eq!(menus.tab, Tab::Controls, "Ctrl+Shift+Tab goes round");
    }

    /// however long a message gets, its OK stays on the screen
    #[test]
    fn a_long_message_can_still_be_closed() {
        let ctx = egui::Context::default();
        let mut menus = Menus { message: Some("Something went wrong, it did.\n\n".repeat(200)), ..Menus::default() };
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 480.0));
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            ctx.run_ui(input, |ui| menus.message(ui.ctx())).drop_without_applying_deltas();
        }
        let window = ctx.memory(|memory| memory.area_rect(Id::new("message"))).unwrap();
        assert!(screen.contains_rect(window), "{window:?} in {screen:?}");
    }

    /// clicking a switch flips it
    #[test]
    fn a_switch_flips_when_clicked() {
        let ctx = egui::Context::default();
        let mut on = false;
        let at = Pos2::new(20.0, 15.0);
        for click in [false, true] {
            let mut events = vec![egui::Event::PointerMoved(at)];
            if click {
                let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
                events.extend([press(true), press(false)]);
            }
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 100.0))), events, ..Default::default() };
            ctx.run_ui(input, |ui| drop(egui::Frame::NONE.inner_margin(8.0).show(ui, |ui| toggle(ui, &mut on)))).drop_without_applying_deltas();
        }
        assert!(on);
    }

    /// toasts of the same kind replace each other, and no more than four stack up
    #[test]
    fn toasts_replace_their_kind_and_stack_a_few() {
        let mut menus = Menus::default();
        menus.pulse("Speed: 200%");
        menus.pulse("Speed: Unlocked");
        assert_eq!(menus.toasts.len(), 1);
        for text in ["a", "b", "c", "d", "e"] {
            menus.toast(text);
        }
        assert_eq!(menus.toasts.len(), 4);
        assert_eq!(menus.toasts.last().unwrap().text, "e");
    }

    /// what takes effect only when the game starts shows as waiting for a restart, and nothing else does
    #[test]
    fn only_start_up_settings_wait_for_a_restart() {
        let booted = Settings::default();
        assert!(!waits_for_restart(&Settings { layout: Screens::SideBySide, vsync: false, frame_interpolation: SmoothMotion::Monitor, filter: Filter::Pixels, ..booted.clone() }, &booted));
        assert!(waits_for_restart(&Settings { resolution: 5, ..booted.clone() }, &booted));
        assert!(waits_for_restart(&Settings { hardware_rasterizer: false, ..booted.clone() }, &booted));
        assert!(waits_for_restart(&Settings { renderer: Renderer::OpenGl, ..booted.clone() }, &booted));
    }

    #[test]
    fn resolutions_are_named_like_azahars() {
        assert_eq!(resolution_name(1), "Native 1x (400x240)");
        assert_eq!(resolution_name(3), "3x (1200x720)");
        assert_eq!(resolution_name(10), "10x (4000x2400)");
    }

    #[test]
    fn keys_have_short_names() {
        assert_eq!(key_name(KeyCode::KeyX), "X");
        assert_eq!(key_name(KeyCode::ArrowUp), "Up");
        assert_eq!(key_name(KeyCode::Digit1), "1");
        assert_eq!(key_name(KeyCode::Enter), "Enter");
        assert_eq!(key_name(KeyCode::Space), "Space");
    }
}
