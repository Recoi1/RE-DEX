//! The "liquid glass" painter for the start screen and ROM builder. egui has no backdrop blur, so the backdrop (a deep
//! navy gradient with a few big soft colour blobs in RE:DEX's colours) is drawn to a small image once per size, which
//! is already smooth, and uploaded. It is a little bigger than the window, and its view pans slowly (a texture offset,
//! no per-frame work). Panels are UV-mapped meshes that sample the same image, with a faint white tint, a feathered
//! rim (brightest top-left, one continuous path), and on cards and Play a specular dot and a soft sheen.

use std::time::{Duration, Instant};

use egui::epaint::{Mesh, Vertex};
use egui::{
    pos2, vec2, Align2, Color32, ColorImage, Context, FontId, Painter, Pos2, Rect, Response, Sense, Shape, Stroke,
    TextureHandle, TextureId, TextureOptions, Ui,
};

use crate::theme;

const BASE_TOP: [f32; 3] = [0x0d as f32, 0x12 as f32, 0x20 as f32];
const BASE_BOTTOM: [f32; 3] = [0x14 as f32, 0x1a as f32, 0x2e as f32];
/// RE:DEX red, sampled from assets/readme-logo.png
pub const BRAND_RED: Color32 = Color32::from_rgb(0xb2, 0x12, 0x2e);
/// the backdrop image is this much bigger than the window, so its view has room to pan
const OVERSCAN: f32 = 1.25;
/// and is made at a quarter of the window's size: blobs are smooth, so nothing is lost
const SCALE: f32 = 4.0;
/// (x, y in the big image 0..1, spread as a fraction of its height, colour, strength)
const BLOBS: [(f32, f32, f32, [f32; 3], f32); 4] = [
    (0.14, 0.88, 0.30, [178.0, 18.0, 46.0], 0.60),
    (0.88, 0.12, 0.34, [61.0, 107.0, 255.0], 0.50),
    (0.58, 0.98, 0.26, [122.0, 77.0, 255.0], 0.31),
    (0.30, 0.10, 0.22, [61.0, 107.0, 255.0], 0.30),
];
/// a dragged window re-makes the backdrop once its size has held this long
const SETTLE: Duration = Duration::from_millis(150);
/// titles are near-white; body text is #E0E0E0 at 75%
pub const TITLE: Color32 = Color32::from_rgb(0xF2, 0xF4, 0xF8);
pub const BODY: Color32 = Color32::from_rgba_premultiplied(168, 168, 168, 191);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Play,
    Folder,
    Home,
    Share,
    Plus,
    Close,
    Prev,
    Next,
}

/// how one piece of glass is dressed
#[derive(Clone, Copy)]
pub struct Look {
    /// 0..1, lifts the rim, the gloss and the streaks
    pub hover: f32,
    pub tint: f32,
    /// the specular dot (cards and Play only)
    pub dot: bool,
    pub rim: bool,
    /// a picture inside the glass (a game's title screen), dimmed, over the backdrop and under the tint
    pub picture: Option<TextureId>,
    /// the piece's own colour (a button's kind, a card's game), washed over the glass `wash` strong (0..1)
    pub hue: Option<Color32>,
    pub wash: f32,
    /// where its streaks fall, so no two pieces look cloned
    pub seed: u32,
    /// 0..1, how high it floats (its shadow)
    pub lift: f32,
}

impl Default for Look {
    fn default() -> Look {
        Look { hover: 0.0, tint: 0.0, dot: false, rim: true, picture: None, hue: None, wash: 0.0, seed: 0, lift: 0.0 }
    }
}

pub struct Glass {
    size: [u32; 2],
    tex: TextureHandle,
    /// how many times the backdrop was made (it must only be per size)
    pub blur_passes: u32,
    /// a size asked for during a drag, and since when it has not changed
    pending: Option<([u32; 2], Instant)>,
    /// where the view of the backdrop sits, 0..1 of the spare room
    pan: (f32, f32),
}

/// the backdrop the glass sits on, at the quarter-size image for a window of `size`
pub fn backdrop(size: [u32; 2]) -> ColorImage {
    let w = ((size[0].max(1) as f32 * OVERSCAN / SCALE).ceil() as usize).max(2);
    let h = ((size[1].max(1) as f32 * OVERSCAN / SCALE).ceil() as usize).max(2);
    let aspect = w as f32 / h as f32;
    let mut px = Vec::with_capacity(w * h);
    for y in 0..h {
        let v = y as f32 / (h - 1) as f32;
        let base: [f32; 3] = std::array::from_fn(|c| BASE_TOP[c] + (BASE_BOTTOM[c] - BASE_TOP[c]) * v);
        for x in 0..w {
            let u = x as f32 / (w - 1) as f32;
            let mut c = base;
            for (bx, by, spread, color, strength) in BLOBS {
                let d2 = ((u - bx) * aspect).powi(2) + (v - by).powi(2);
                let k = strength * (-d2 / (2.0 * spread * spread)).exp();
                (0..3).for_each(|i| c[i] += (color[i] - c[i]) * k);
            }
            px.push(Color32::from_rgb(c[0].round() as u8, c[1].round() as u8, c[2].round() as u8));
        }
    }
    ColorImage { size: [w, h], source_size: vec2(w as f32, h as f32), pixels: px }
}

/// separable box blur, `passes` times, edge-clamped (for pictures: the game cards' title screens)
pub fn box_blur(img: &mut ColorImage, radius: usize, passes: usize) {
    let [w, h] = img.size;
    let mut buf: Vec<[f32; 3]> = img.pixels.iter().map(|c| [c.r() as f32, c.g() as f32, c.b() as f32]).collect();
    let mut tmp = buf.clone();
    let n = (2 * radius + 1) as f32;
    for _ in 0..passes {
        for horizontal in [true, false] {
            let (len, lines) = if horizontal { (w, h) } else { (h, w) };
            let at = |l: usize, i: usize| if horizontal { l * w + i } else { i * w + l };
            for l in 0..lines {
                let mut acc = [0f32; 3];
                for k in -(radius as isize)..=(radius as isize) {
                    let p = buf[at(l, k.clamp(0, len as isize - 1) as usize)];
                    (0..3).for_each(|c| acc[c] += p[c]);
                }
                for i in 0..len {
                    tmp[at(l, i)] = [acc[0] / n, acc[1] / n, acc[2] / n];
                    let add = buf[at(l, (i + radius + 1).min(len - 1))];
                    let sub = buf[at(l, i.saturating_sub(radius))];
                    (0..3).for_each(|c| acc[c] += add[c] - sub[c]);
                }
            }
            std::mem::swap(&mut buf, &mut tmp);
        }
    }
    for (p, b) in img.pixels.iter_mut().zip(&buf) {
        *p = Color32::from_rgb(b[0].round() as u8, b[1].round() as u8, b[2].round() as u8);
    }
}

/// outline of a rounded rect, clockwise (on screen) from the top-left arc's start
fn outline(rect: Rect, radius: f32) -> Vec<Pos2> {
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let steps = ((r * 0.6).ceil() as usize).clamp(3, 24);
    let corners = [
        (pos2(rect.min.x + r, rect.min.y + r), 180.0f32),
        (pos2(rect.max.x - r, rect.min.y + r), 270.0),
        (pos2(rect.max.x - r, rect.max.y - r), 0.0),
        (pos2(rect.min.x + r, rect.max.y - r), 90.0),
    ];
    let mut pts = Vec::new();
    for (c, start) in corners {
        for i in 0..=steps {
            let a = (start + 90.0 * i as f32 / steps as f32).to_radians();
            pts.push(c + vec2(a.cos(), a.sin()) * r);
        }
    }
    pts
}

/// how far along the top-left -> bottom-right diagonal a point is, 0..1
fn diag(rect: Rect, p: Pos2) -> f32 {
    let c = rect.center();
    let half = rect.size() * 0.5;
    (((p.x - c.x) / half.x.max(1.0) + (p.y - c.y) / half.y.max(1.0)) * 0.25 + 0.5).clamp(0.0, 1.0)
}

fn white(a: f32) -> Color32 {
    Color32::from_white_alpha((a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// the rim: one continuous ring along the outline, feathered over 1.5 px (clear, full, clear across it), white 92% at
/// the top-left fading to 30% at the bottom-right (`hover` 0..1 lifts it). The ring's width follows the outline's own
/// normal, so it does not break at the corners.
pub fn rim_mesh(rect: Rect, radius: f32, hover: f32) -> Mesh {
    let pts = outline(rect, radius);
    let n = pts.len();
    let half = 0.75;
    let mut m = Mesh::default();
    for (i, p) in pts.iter().enumerate() {
        let d = diag(rect, *p);
        let a = (0.92 - 0.62 * d + 0.08 * hover).min(0.98);
        let t = pts[(i + 1) % n] - pts[(i + n - 1) % n];
        let inward = vec2(-t.y, t.x).normalized() * half;
        let v = |pos: Pos2, a: f32| Vertex { pos, uv: egui::epaint::WHITE_UV, color: white(a) };
        // the core sits on the outline, so the nearest vertex to the edge is the bright one
        m.vertices.push(v(*p, a));
        m.vertices.push(v(*p - inward, 0.0));
        m.vertices.push(v(*p + inward, 0.0));
    }
    let n = n as u32;
    for i in 0..n {
        let (a, b) = (i * 3, ((i + 1) % n) * 3);
        m.indices.extend([a, a + 1, b, b, a + 1, b + 1, a, a + 2, b, b, a + 2, b + 2]);
    }
    m
}

/// a triangle fan over the outline, vertex colours from `color(point)`, UVs from `uv(point)`
fn fan(rect: Rect, radius: f32, uv: impl Fn(Pos2) -> Pos2, color: impl Fn(Pos2) -> Color32) -> Mesh {
    let pts = outline(rect, radius);
    let mut m = Mesh::default();
    let c = rect.center();
    m.vertices.push(Vertex { pos: c, uv: uv(c), color: color(c) });
    for p in &pts {
        m.vertices.push(Vertex { pos: *p, uv: uv(*p), color: color(*p) });
    }
    let n = pts.len() as u32;
    for i in 0..n {
        m.indices.extend([0, 1 + i, 1 + (i + 1) % n]);
    }
    m
}

/// a rounded rect shaded from `top` to `bottom`
pub fn gradient(painter: &Painter, rect: Rect, radius: f32, top: Color32, bottom: Color32) {
    painter.add(Shape::mesh(fan(rect, radius, |_| egui::epaint::WHITE_UV, |p| {
        top.lerp_to_gamma(bottom, ((p.y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 1.0))
    })));
}

/// a soft coloured glow around a rounded rect: a few fainter and fainter rings of the same shape
pub fn glow(painter: &Painter, rect: Rect, radius: f32, color: Color32, strength: f32) {
    for (grow, a) in [(20.0, 0.020), (14.0, 0.030), (9.0, 0.045), (5.0, 0.065), (2.0, 0.09)] {
        let c = color.gamma_multiply((a * strength).min(1.0));
        painter.rect_filled(rect.expand(grow), radius + grow, c);
    }
}

impl Glass {
    pub fn new(ctx: &Context, size: [u32; 2]) -> Glass {
        theme::install_fonts(ctx);
        let tex = ctx.load_texture("glass-backdrop", backdrop(size), TextureOptions::LINEAR);
        Glass { size, tex, blur_passes: 1, pending: None, pan: (0.5, 0.5) }
    }

    /// re-makes the backdrop only when the size really changed
    pub fn resize(&mut self, size: [u32; 2]) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.tex.set(backdrop(size), TextureOptions::LINEAR);
        self.blur_passes += 1;
    }

    /// the window's size now; the re-make waits in `settle` until it stops changing
    pub fn request_size(&mut self, size: [u32; 2], now: Instant) {
        match self.pending {
            Some((p, _)) if p == size => {}
            _ if size == self.size => self.pending = None,
            _ => self.pending = Some((size, now)),
        }
    }

    /// re-makes the backdrop when the requested size has been stable for 150 ms
    pub fn settle(&mut self, now: Instant) {
        if let Some((size, since)) = self.pending {
            if now.duration_since(since) >= SETTLE {
                self.pending = None;
                self.resize(size);
            }
        }
    }

    /// true while a size waits for its re-make
    pub fn settling(&self) -> bool {
        self.pending.is_some()
    }

    /// moves the view of the backdrop, a 20-40 s cycle (still with reduced motion)
    pub fn animate(&mut self, secs: f64) {
        self.pan = if crate::animations() {
            let t = secs as f32 * std::f32::consts::TAU;
            (0.5 + 0.5 * (t / 37.0).sin(), 0.5 + 0.5 * (t / 29.0 + 1.0).sin())
        } else {
            (0.5, 0.5)
        };
    }

    /// where a point of the window falls on the backdrop image
    fn uv(&self, p: Pos2) -> Pos2 {
        let view = 1.0 / OVERSCAN;
        let room = 1.0 - view;
        let (w, h) = (self.size[0].max(1) as f32, self.size[1].max(1) as f32);
        pos2(self.pan.0 * room + p.x / w * view, self.pan.1 * room + p.y / h * view)
    }

    /// a section title: heavy caps with a little letter spacing, on a soft shadow of its own
    pub fn title(&self, painter: &Painter, pos: Pos2, text: &str, size: f32) {
        let font = theme::heavy(painter.ctx(), size);
        let mut x = pos.x;
        for ch in text.to_uppercase().chars() {
            let g = painter.layout_no_wrap(ch.to_string(), font.clone(), TITLE);
            let w = g.size().x;
            painter.galley(pos2(x, pos.y + 1.0), painter.layout_no_wrap(ch.to_string(), font.clone(), Color32::from_black_alpha(110)), Color32::BLACK);
            painter.galley(pos2(x, pos.y), g, TITLE);
            x += w + size * 0.08;
        }
    }

    /// the backdrop, to paint behind everything
    pub fn paint_backdrop(&self, painter: &Painter, rect: Rect) {
        let (a, b) = (self.uv(rect.min), self.uv(rect.max));
        let mut m = Mesh::with_texture(self.tex.id());
        m.add_rect_with_uv(rect, Rect::from_min_max(a, b), Color32::WHITE);
        painter.add(Shape::mesh(m));
    }

    /// one piece of glass, in layers: its shadow when it floats, the backdrop where it sits, a near-clear tint (and its
    /// colour's wash), a gloss on its top half, two diagonal streaks, the dark edge that sets it off the backdrop, the
    /// bright rim, an inner line and a glow along its bottom
    pub fn paint(&self, painter: &Painter, rect: Rect, radius: f32, look: Look) {
        if look.lift > 0.0 {
            shadow(painter, rect, radius, look.lift);
        }
        let mut m = fan(rect, radius, |p| self.uv(p), |_| Color32::WHITE);
        m.texture_id = self.tex.id();
        painter.add(Shape::mesh(m));
        if let Some(tex) = look.picture {
            self.picture(painter, rect, radius, tex, Color32::from_gray(185), true);
        }
        glaze(painter, rect, radius, look);
        if look.dot {
            self.dot(painter, rect, radius);
        }
    }

    /// the specular dot, top-left (drawn again over a picture that covers it)
    pub fn dot(&self, painter: &Painter, rect: Rect, radius: f32) {
        let dot = rect.min + vec2(radius * 0.55 + 4.0, radius * 0.45 + 4.0).min(rect.size() * 0.4);
        painter.circle_filled(dot, 5.0, white(0.07));
        painter.circle_filled(dot, 3.5, white(0.12));
        painter.circle_filled(dot, 2.0, white(0.9));
    }

    pub fn panel(&self, ui: &mut Ui, rect: Rect, radius: f32) {
        self.paint(ui.painter(), rect, radius, Look { lift: 0.6, ..Look::default() });
    }

    /// a card: glass with a dot, washed in its own colour; selected, it has the bright rim and a soft red glow
    /// `wash` is the card's colour (a game's, a character's)
    pub fn card(&self, painter: &Painter, rect: Rect, radius: f32, selected: bool, hover: f32, picture: Option<TextureId>, wash: Color32) {
        if selected {
            glow(painter, rect, radius, BRAND_RED, 1.6);
        }
        let seed = (rect.min.x * 7.0 + rect.min.y * 3.0) as u32;
        let look = Look { hover: if selected { 1.0 } else { hover }, dot: true, picture, hue: Some(wash), wash: if picture.is_some() { 0.0 } else { 0.30 }, seed, lift: 0.8 + 0.4 * hover, ..Look::default() };
        self.paint(painter, rect, radius, look);
        if selected {
            painter.rect_stroke(rect.expand(1.5), radius + 1.5, Stroke::new(1.6, theme::BRAND_HI.gamma_multiply(0.9)), egui::StrokeKind::Outside);
        }
    }

    /// a card for something that is not there yet: glass with a dashed rim
    pub fn dashed_card(&self, painter: &Painter, rect: Rect, radius: f32, hover: f32, dim: f32) {
        self.paint(painter, rect, radius, Look { hover: hover * 0.5, rim: false, tint: -0.02, ..Look::default() });
        let mut pts = outline(rect, radius);
        pts.push(pts[0]);
        painter.extend(Shape::dashed_line(&pts, Stroke::new(1.5, white((0.42 + 0.35 * hover) * dim)), 7.0, 5.0));
    }

    /// a picture in a rounded rect, `tint` multiplying it (dimming), with a scrim under where the words go when asked
    pub fn picture(&self, painter: &Painter, rect: Rect, radius: f32, tex: TextureId, tint: Color32, scrim: bool) {
        let uv = |p: Pos2| pos2((p.x - rect.min.x) / rect.width().max(1.0), (p.y - rect.min.y) / rect.height().max(1.0));
        let mut m = fan(rect, radius, uv, |_| tint);
        m.texture_id = tex;
        painter.add(Shape::mesh(m));
        if !scrim {
            return;
        }
        painter.add(Shape::mesh(fan(rect, radius, |_| egui::epaint::WHITE_UV, |p| {
            let t = ((p.y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 1.0);
            Color32::from_rgba_premultiplied(4, 6, 14, (255.0 * (0.10 + 0.72 * t * t)) as u8)
        })));
    }

    /// hover 0..1 and the scale: up a little on hover, in fast on a press, back with a spring on its release (none
    /// when the system turns animations off)
    fn interact(&self, ui: &Ui, resp: &Response) -> (f32, f32) {
        press_scale(ui, resp, theme::HOVER_SCALE)
    }

    /// hover for something the caller laid out itself (a card), eased the same way
    pub fn hover_of(&self, ui: &Ui, resp: &Response) -> f32 {
        self.interact(ui, resp).0
    }

    pub fn pill_button(&self, ui: &mut Ui, text: &str) -> Response {
        self.pill(ui, text, None, true)
    }

    /// a 40 px glass button, `width` fixed when given
    pub fn pill(&self, ui: &mut Ui, text: &str, width: Option<f32>, enabled: bool) -> Response {
        self.button(ui, text, None, Kind::Secondary, width, enabled)
    }

    /// a 40 px glass button with its icon, in its kind's colours
    pub fn button(&self, ui: &mut Ui, text: &str, icon: Option<(crate::icons::Icon, Color32)>, kind: Kind, width: Option<f32>, enabled: bool) -> Response {
        let font = theme::heavy(ui.ctx(), theme::LABEL);
        let galley = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE);
        let pad = if icon.is_some() { 44.0 } else { 24.0 };
        let size = vec2(width.unwrap_or(galley.size().x + pad + 20.0), 40.0);
        let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
        let (hover, scale) = if enabled { self.interact(ui, &resp) } else { (0.0, 1.0) };
        self.button_face(ui.painter(), rect, scale, hover, kind, enabled);
        let r = Rect::from_center_size(rect.center(), rect.size() * scale);
        let color = if !enabled { MUTED_TEXT } else if kind == Kind::Danger { theme::CORAL } else { TITLE };
        let icon = icon.map(|(i, h)| (i, if kind == Kind::DangerSolid { Color32::WHITE } else { h }));
        let text_w = galley.size().x + if icon.is_some() { 26.0 } else { 0.0 };
        let mut x = r.center().x - text_w / 2.0;
        if let Some((icon, hue)) = icon {
            crate::icons::draw_alpha(ui.painter(), icon, pos2(x + 10.0, r.center().y - 0.5), 22.0, hue, -8.0, if enabled { 1.0 } else { 0.45 });
            x += 26.0;
        }
        shadowed(ui.painter(), pos2(x, r.center().y), Align2::LEFT_CENTER, text, font, color);
        resp
    }

    /// a button's glass: a lip under it (its thickness, gone while pressed) and the face in its kind's colour
    pub fn button_face(&self, painter: &Painter, rect: Rect, scale: f32, hover: f32, kind: Kind, enabled: bool) {
        let r = Rect::from_center_size(rect.center(), rect.size() * scale);
        let radius = r.height().min(rect.height()) / 2.0;
        let pressed = (1.0 - scale).max(0.0) / (1.0 - theme::PRESS_SCALE);
        let (hue, wash) = match kind {
            Kind::Primary => (Some(theme::BRAND), 0.78),
            Kind::Secondary => (None, 0.0),
            Kind::Danger => (Some(theme::CORAL), 0.10),
            Kind::DangerSolid => (Some(theme::CORAL), 0.9),
            Kind::Tint(c) => (Some(c), 0.22),
        };
        let lip = 3.0 * (1.0 - pressed.min(1.0));
        if lip > 0.2 {
            let shade = hue.map_or(Color32::from_rgba_premultiplied(6, 8, 18, 150), |h| theme::deep(h).gamma_multiply(0.9));
            painter.rect_filled(r.translate(vec2(0.0, lip)), radius, shade);
        }
        let face = r.translate(vec2(0.0, 2.0 * pressed.min(1.0)));
        let seed = (rect.min.x * 13.0 + rect.min.y * 5.0) as u32;
        self.paint(painter, face, radius, Look { hover: if enabled { hover } else { 0.0 }, tint: 0.02 * hover, hue, wash: wash + 0.08 * hover, seed, ..Look::default() });
        if !enabled {
            painter.rect_filled(face, radius, Color32::from_rgba_premultiplied(8, 11, 22, 120));
        }
    }

    pub fn circle_button(&self, ui: &mut Ui, icon: Icon, diameter: f32) -> Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(diameter, diameter), Sense::click());
        let (hover, scale) = self.interact(ui, &resp);
        self.button_face(ui.painter(), rect, scale, hover, Kind::Secondary, true);
        draw_icon(ui.painter(), icon, rect.center(), diameter * 0.32);
        resp
    }

    /// a glass track with a fill in the brand's colour and a sheen that moves while it runs (`fraction` None: not
    /// known yet, a short bar slides to and fro)
    pub fn progress(&self, ui: &mut Ui, fraction: Option<f32>, running: bool) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width().max(64.0), 16.0), Sense::hover());
        self.paint(ui.painter(), rect, rect.height() / 2.0, Look::default());
        let inner = rect.shrink(3.0);
        let time = ui.input(|i| i.time) as f32;
        let fill = match fraction {
            Some(f) => {
                let w = (inner.width() * f.clamp(0.0, 1.0)).max(if running { inner.height() } else { 0.0 });
                if w <= 0.0 {
                    return;
                }
                Rect::from_min_size(inner.min, vec2(w, inner.height()))
            }
            None => {
                let w = inner.width() * 0.3;
                let p = if crate::animations() { 1.0 - ((time * 0.5) % 2.0 - 1.0).abs() } else { 0.5 };
                Rect::from_min_size(inner.min + vec2(p * (inner.width() - w), 0.0), vec2(w, inner.height()))
            }
        };
        let phase = if crate::animations() { (time * 0.7) % 1.6 - 0.3 } else { 0.5 };
        ui.painter().add(Shape::mesh(fan(fill, fill.height() / 2.0, |_| egui::epaint::WHITE_UV, |p| {
            let x = (p.x - fill.min.x) / fill.width().max(1.0);
            let y = (p.y - fill.min.y) / fill.height().max(1.0);
            theme::BRAND_HI.lerp_to_gamma(theme::BRAND_DEEP, y * 0.6).lerp_to_gamma(Color32::WHITE, 0.35 * (1.0 - (x - phase).abs() * 3.5).max(0.0))
        })));
        ui.painter().add(Shape::mesh(rim_mesh(fill, fill.height() / 2.0, 0.8)));
    }
}

/// what a glass button is for: the one main action (the brand's red), an ordinary one, one that removes or ends
/// something (coral words on clear glass), or one in an element's own colour
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
    /// the last step of something that cannot be undone (Remove in a confirm): solid coral, white words
    DangerSolid,
    Tint(Color32),
}

const MUTED_TEXT: Color32 = Color32::from_rgb(0x8A, 0x93, 0xA8);

/// hover 0..1 and the scale of something pressable (see Glass::interact), `up` its hover scale
pub fn press_scale(ui: &Ui, resp: &Response, up: f32) -> (f32, f32) {
    let motion = crate::animations();
    let t = |s: f32| if motion { s } else { 0.0 };
    let ctx = ui.ctx();
    let hover = ctx.animate_bool_with_time_and_easing(resp.id.with("hover"), resp.hovered(), t(theme::HOVER), theme::quad_out);
    let down = resp.is_pointer_button_down_on();
    let d = ctx.animate_bool_with_time(resp.id.with("down"), down, if down { t(theme::PRESS) } else { t(theme::RELEASE) });
    let pressed = if down { d } else { 1.0 - theme::back_out(1.0 - d) };
    if resp.clicked() {
        crate::ui_sound::play(crate::ui_sound::Sound::Click);
    }
    (hover, 1.0 + (up - 1.0) * hover - (1.0 - theme::PRESS_SCALE) * pressed)
}

/// words on a soft shadow of their own (a point down): the reference UIs' extruded labels, quieter
pub fn shadowed(painter: &Painter, pos: Pos2, align: Align2, text: &str, font: FontId, color: Color32) -> Rect {
    painter.text(pos + vec2(0.0, 1.2), align, text, font.clone(), Color32::from_black_alpha(140));
    painter.text(pos, align, text, font, color)
}

/// the keyboard's and controller's place: a red ring with a soft glow around it, breathing
pub fn focus_ring(painter: &Painter, rect: Rect, radius: f32, time: f64) {
    let pulse = if crate::animations() { 0.5 + 0.5 * (time as f32 * 5.0).sin() } else { 0.5 };
    glow(painter, rect, radius, theme::BRAND, 0.9 + 0.5 * pulse);
    painter.rect_stroke(rect.expand(3.0), radius + 3.0, Stroke::new(2.0 + 0.6 * pulse, theme::BRAND_HI), egui::StrokeKind::Outside);
    painter.add(Shape::mesh(rim_mesh(rect, radius, 1.0)));
}

/// a small static chip: an icon and a word on a quiet tinted ground (a state, a tag)
pub fn chip(painter: &Painter, at: Pos2, text: &str, icon: Option<crate::icons::Icon>, hue: Color32) -> Rect {
    let font = theme::heavy(painter.ctx(), theme::CAPTION - 0.5);
    let g = painter.layout_no_wrap(text.to_owned(), font.clone(), TITLE);
    let w = g.size().x + if icon.is_some() { 30.0 } else { 16.0 };
    let rect = Rect::from_min_size(at, vec2(w, 22.0));
    painter.rect_filled(rect, 11.0, hue.gamma_multiply(0.20));
    painter.rect_stroke(rect, 11.0, Stroke::new(1.0, hue.gamma_multiply(0.55)), egui::StrokeKind::Inside);
    let mut x = rect.min.x + 8.0;
    if let Some(icon) = icon {
        crate::icons::draw(painter, icon, pos2(x + 6.0, rect.center().y), 14.0, hue, 0.0);
        x += 16.0;
    }
    painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, text, font, TITLE);
    rect
}

/// a field's well: deep and clear, with an inner line; a red ring while it has the keyboard
pub fn well(painter: &Painter, rect: Rect, focused: bool) {
    let r = 12.0_f32.min(rect.height() / 2.0);
    painter.rect_filled(rect, r, Color32::from_rgba_premultiplied(4, 6, 14, 120));
    painter.rect_stroke(rect, r, Stroke::new(1.0, white(if focused { 0.0 } else { 0.22 })), egui::StrokeKind::Inside);
    painter.hline((rect.min.x + r)..=(rect.max.x - r), rect.min.y + 1.5, Stroke::new(1.0, Color32::from_black_alpha(90)));
    if focused {
        painter.rect_stroke(rect, r, Stroke::new(1.8, theme::BRAND_HI), egui::StrokeKind::Inside);
    }
}

/// a soft dark shadow under something that floats, `k` 0..1 how high
pub fn shadow(painter: &Painter, rect: Rect, radius: f32, k: f32) {
    for (grow, dy, a) in [(10.0, 10.0, 0.05), (6.0, 7.0, 0.08), (3.0, 4.0, 0.12), (1.0, 2.0, 0.14)] {
        painter.rect_filled(rect.expand(grow * k).translate(vec2(0.0, dy * k)), radius + grow * k, Color32::from_black_alpha((255.0 * a * k.min(1.5)) as u8));
    }
}

/// the glass over whatever is under it (the backdrop, a picture, the game): see Glass::paint
pub fn glaze(painter: &Painter, rect: Rect, radius: f32, look: Look) {
    // almost clear: white 7% at the top to 2% at the bottom, the rim does the work; a colour's wash over it
    painter.add(Shape::mesh(fan(rect, radius, |_| egui::epaint::WHITE_UV, |p| {
        let t = ((p.y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 1.0);
        let base = white((0.07 - 0.05 * t + look.tint).max(0.0));
        match look.hue {
            Some(h) if look.wash > 0.0 => {
                let k = look.wash * (1.0 - 0.45 * t);
                let c = h.lerp_to_gamma(theme::deep(h), t * 0.5);
                let add = |x: u8, y: u8| ((x as f32 * k) as u8).saturating_add(y);
                Color32::from_rgba_premultiplied(add(c.r(), base.r()), add(c.g(), base.g()), add(c.b(), base.b()), ((255.0 * k) as u8).saturating_add(base.a()))
            }
            _ => base,
        }
    })));
    // the gloss: the top 45% lit, strongest at the top edge
    let top = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.min.y + rect.height() * 0.45));
    painter.add(Shape::mesh(fan(top, radius.min(top.height() / 2.0), |_| egui::epaint::WHITE_UV, |p| {
        let t = ((p.y - top.min.y) / top.height().max(1.0)).clamp(0.0, 1.0);
        white((0.10 + 0.05 * look.hover) * (1.0 - t) * (1.0 - t))
    })));
    // two diagonal streaks, placed by the seed so no two pieces look cloned, sliding a little on hover
    let a = 0.16 + 0.22 * ((look.seed as f32 * 0.618_034) % 1.0) + 0.05 * look.hover;
    // a big pane gets fainter, thinner streaks: they are glints, not stripes
    let big = (90.0 / rect.height()).clamp(0.3, 1.0);
    for (from, thick, alpha) in [(a, 0.07 * big, 0.065 * big), (a + 0.13, 0.025 * big, 0.05 * big)] {
        let clipped = clip_convex(&streak(rect, from, thick), &outline(rect, radius));
        if clipped.len() >= 3 {
            painter.add(Shape::convex_polygon(clipped, white(alpha + 0.03 * look.hover), Stroke::NONE));
        }
    }
    if look.rim {
        // the dark edge just outside, then the bright rim on it, and a faint inner line
        painter.rect_stroke(rect.expand(0.6), radius + 0.6, Stroke::new(1.2, Color32::from_rgba_premultiplied(3, 5, 12, 150)), egui::StrokeKind::Outside);
        painter.add(Shape::mesh(rim_mesh(rect, radius, look.hover)));
        let inner = rect.shrink(2.5);
        painter.rect_stroke(inner, (radius - 2.5).max(1.0), Stroke::new(1.0, white(0.07 + 0.04 * look.hover)), egui::StrokeKind::Inside);
        // a glow along the bottom, in the colour when there is one
        let inset = radius.min(rect.width() / 2.0);
        let glow_c = look.hue.map_or(white(0.16), |h| h.gamma_multiply(0.55));
        painter.hline((rect.min.x + inset)..=(rect.max.x - inset), rect.max.y - 1.8, Stroke::new(1.4, glow_c));
    }
}

/// a bright band crossing `rect` at `phase` (0..1 across it, beyond it on either side: none), clipped to its shape
pub fn sheen(painter: &Painter, rect: Rect, radius: f32, phase: f32) {
    let band = clip_convex(&streak(rect, phase, 0.10), &outline(rect, radius));
    if band.len() >= 3 {
        painter.add(Shape::convex_polygon(band, white(0.16), Stroke::NONE));
    }
}

/// paints `paint` behind what was drawn since `slot` (a `Shape::Noop` added first): a frame sized by its contents
pub fn paint_behind(painter: &Painter, slot: egui::layers::ShapeIdx, paint: impl FnOnce(&Painter)) {
    let layer = painter.layer_id();
    let start = painter.ctx().graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0));
    paint(painter);
    painter.ctx().graphics_mut(|g| {
        let list = g.entry(layer);
        let end = list.next_idx().0;
        let shapes: Vec<Shape> = list.all_entries().skip(start).take(end - start).map(|c| c.shape.clone()).collect();
        (start..end).for_each(|i| list.reset_shape(egui::layers::ShapeIdx(i)));
        list.set(slot, painter.clip_rect(), Shape::Vec(shapes));
    });
}

/// smoked glass over the game (no backdrop to sample, and the game is busy and bright): a deep translucent ground,
/// then the same gloss, streaks, edge and rim as the start screen's
pub fn smoke(painter: &Painter, rect: Rect, radius: f32, seed: u32) {
    shadow(painter, rect, radius, 1.2);
    painter.rect_filled(rect, radius, theme::SMOKE);
    glaze(painter, rect, radius, Look { seed, ..Look::default() });
}

/// a button's glass over the game: its lip, a face in its kind's colour (clear, red, coral) and the glaze
pub fn face(painter: &Painter, rect: Rect, scale: f32, hover: f32, kind: Kind, enabled: bool) {
    let r = Rect::from_center_size(rect.center(), rect.size() * scale);
    let radius = (r.height() / 2.0).min(12.0);
    let pressed = ((1.0 - scale).max(0.0) / (1.0 - theme::PRESS_SCALE)).min(1.0);
    let (hue, ground) = match kind {
        Kind::Primary => (Some(theme::BRAND), theme::BRAND.gamma_multiply(0.85 + 0.15 * hover)),
        Kind::Danger => (Some(theme::CORAL), theme::CORAL.gamma_multiply(0.16 + 0.10 * hover)),
        Kind::DangerSolid => (Some(theme::CORAL), theme::CORAL.gamma_multiply(0.92 + 0.08 * hover)),
        Kind::Tint(c) => (Some(c), c.gamma_multiply(0.24 + 0.10 * hover)),
        Kind::Secondary => (None, white(0.08 + 0.06 * hover)),
    };
    let lip = 3.0 * (1.0 - pressed);
    if lip > 0.2 {
        painter.rect_filled(r.translate(vec2(0.0, lip)), radius, hue.map_or(Color32::from_rgba_premultiplied(2, 3, 8, 170), |h| theme::deep(h)));
    }
    let face = r.translate(vec2(0.0, 2.0 * pressed));
    painter.rect_filled(face, radius, Color32::from_rgba_premultiplied(14, 18, 32, 235));
    painter.rect_filled(face, radius, ground);
    let seed = (rect.min.x * 13.0 + rect.min.y * 5.0) as u32;
    glaze(painter, face, radius, Look { hover, hue, wash: 0.0, seed, ..Look::default() });
    if !enabled {
        painter.rect_filled(face, radius, Color32::from_rgba_premultiplied(8, 11, 22, 140));
    }
}

/// a band across `rect` along the top-left to bottom-right diagonal, from `from` and `thick` wide (both 0..1 of it)
fn streak(rect: Rect, from: f32, thick: f32) -> Vec<Pos2> {
    let n = vec2(1.0, 0.62).normalized();
    let along = vec2(-n.y, n.x);
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom()];
    let s: Vec<f32> = corners.iter().map(|c| c.to_vec2().dot(n)).collect();
    let (s0, s1) = (s.iter().cloned().fold(f32::MAX, f32::min), s.iter().cloned().fold(f32::MIN, f32::max));
    let (a, b) = (s0 + from * (s1 - s0), s0 + (from + thick) * (s1 - s0));
    let mid = rect.center().to_vec2().dot(along);
    let l = rect.size().length();
    [(a, mid - l), (a, mid + l), (b, mid + l), (b, mid - l)].iter().map(|&(sn, sa)| (n * sn + along * sa).to_pos2()).collect()
}

/// the part of convex polygon `subject` inside convex polygon `clip` (Sutherland-Hodgman); `clip` clockwise on screen
fn clip_convex(subject: &[Pos2], clip: &[Pos2]) -> Vec<Pos2> {
    let mut out = subject.to_vec();
    for i in 0..clip.len() {
        let (a, b) = (clip[i], clip[(i + 1) % clip.len()]);
        let side = |p: Pos2| (b - a).x * (p - a).y - (b - a).y * (p - a).x;
        let input = std::mem::take(&mut out);
        for j in 0..input.len() {
            let (p, q) = (input[j], input[(j + 1) % input.len()]);
            let (dp, dq) = (side(p), side(q));
            if dp >= 0.0 && dq >= 0.0 {
                out.push(q);
            } else if dp >= 0.0 {
                out.push(p + (q - p) * (dp / (dp - dq)));
            } else if dq >= 0.0 {
                out.push(p + (q - p) * (dp / (dp - dq)));
                out.push(q);
            }
        }
        if out.is_empty() {
            break;
        }
    }
    out
}

/// the screens' older icons, drawn as RE:DEX's own (icons.rs); `s` is the icon's half-size
pub fn draw_icon(p: &Painter, icon: Icon, c: Pos2, s: f32) {
    use crate::icons::Icon as I;
    let (to, hue) = match icon {
        Icon::Play => (I::Play, Color32::WHITE),
        Icon::Folder => (I::Puzzle, theme::AMBER),
        Icon::Home => (I::House, theme::SLATE),
        Icon::Share => (I::Friends, theme::VIOLET),
        Icon::Plus => (I::Plus, theme::MINT),
        Icon::Close => (I::Close, theme::SLATE),
        Icon::Prev => (I::Prev, theme::TITLE),
        Icon::Next => (I::Next, theme::TITLE),
    };
    crate::icons::draw(p, to, c, s * 2.4, hue, 0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rim_is_brighter_top_left_than_bottom_right() {
        let rect = Rect::from_min_size(pos2(40.0, 40.0), vec2(300.0, 160.0));
        let m = rim_mesh(rect, 24.0, 0.0);
        let alpha_at = |target: Pos2| {
            m.vertices
                .iter()
                .min_by(|a, b| a.pos.distance(target).total_cmp(&b.pos.distance(target)))
                .unwrap()
                .color
                .a()
        };
        let tl = alpha_at(rect.min + vec2(24.0, 0.0));
        let br = alpha_at(rect.max - vec2(24.0, 0.0));
        assert!(tl > br + 40, "top-left rim {tl} should beat bottom-right {br}");
        // hover lifts it
        let hot = rim_mesh(rect, 24.0, 1.0);
        assert!(hot.vertices[0].color.a() > m.vertices[0].color.a());
    }

    #[test]
    fn rim_is_one_closed_path_with_no_gaps() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(280.0, 160.0));
        let m = rim_mesh(rect, 24.0, 0.0);
        let cores: Vec<Pos2> = m.vertices.chunks(3).map(|c| c[0].pos).collect();
        // consecutive core points are close except along the straight edges (one segment), including last back to first
        let worst = (0..cores.len()).map(|i| cores[i].distance(cores[(i + 1) % cores.len()])).fold(0.0, f32::max);
        assert!(worst < 240.0);
        // the feather runs across the edge, not toward the centre: on the top edge the sides sit above and below it
        let top = m.vertices.chunks(3).find(|c| (c[0].pos.y - rect.min.y).abs() < 0.01).unwrap();
        assert!(top[1].pos.y < top[0].pos.y && top[2].pos.y > top[0].pos.y);
        // and at a corner's start (the left edge's end) it still runs across: no vertex is left of the edge's outer side
        assert!(m.vertices.iter().all(|v| v.pos.x > rect.min.x - 1.0 && v.pos.y > rect.min.y - 1.0));
    }

    #[test]
    fn blur_recomputed_only_on_resize() {
        let ctx = Context::default();
        let mut g = Glass::new(&ctx, [320, 200]);
        for _ in 0..10 {
            let mut out = ctx.run_ui(Default::default(), |ui| {
                g.panel(ui, Rect::from_min_size(pos2(10.0, 10.0), vec2(100.0, 50.0)), 12.0);
                g.resize([320, 200]);
            });
            out.textures_delta.clear();
        }
        assert_eq!(g.blur_passes, 1);
        g.resize([640, 360]);
        assert_eq!(g.blur_passes, 2);
    }

    #[test]
    fn drag_reblurs_once_after_the_size_settles() {
        let ctx = Context::default();
        let mut g = Glass::new(&ctx, [320, 200]);
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // a drag: a new size every 20 ms
        for i in 1..=10u32 {
            g.request_size([320 + i * 10, 200], t0 + ms(20 * i as u64));
            g.settle(t0 + ms(20 * i as u64));
        }
        assert_eq!(g.blur_passes, 1, "no re-blur while the size keeps changing");
        g.settle(t0 + ms(200 + 100));
        assert_eq!(g.blur_passes, 1, "not yet stable for 150 ms");
        g.settle(t0 + ms(200 + 160));
        assert_eq!(g.blur_passes, 2);
        g.settle(t0 + ms(2000));
        assert_eq!(g.blur_passes, 2);
    }

    #[test]
    fn backdrop_is_smooth_and_coloured() {
        let img = backdrop([800, 500]);
        let [w, _] = img.size;
        let step = |a: Color32, b: Color32| (a.r() as i32 - b.r() as i32).abs().max((a.b() as i32 - b.b() as i32).abs());
        let worst = (0..img.pixels.len() - 1).filter(|i| (i + 1) % w != 0).map(|i| step(img.pixels[i], img.pixels[i + 1])).max().unwrap();
        assert!(worst <= 4, "blobs are soft, the largest step between neighbours was {worst}");
        // there is red, blue and a dark ground in it
        assert!(img.pixels.iter().any(|c| c.r() > c.b() + 40), "no red blob");
        assert!(img.pixels.iter().any(|c| c.b() > c.r() + 80), "no blue blob");
        assert!(img.pixels.iter().any(|c| c.r() < 0x16 && c.g() < 0x1c), "no dark ground");
    }
}
