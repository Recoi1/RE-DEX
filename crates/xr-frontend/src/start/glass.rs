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
const ANIM_TIME: f32 = 0.2;
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
    /// 0..1, lifts the rim and the sheen
    pub hover: f32,
    pub tint: f32,
    /// the specular dot (cards and Play only)
    pub dot: bool,
    pub rim: bool,
    /// a picture inside the glass (a game's title screen), dimmed, over the backdrop and under the tint
    pub picture: Option<TextureId>,
}

impl Default for Look {
    fn default() -> Look {
        Look { hover: 0.0, tint: 0.0, dot: false, rim: true, picture: None }
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

/// the rim: one continuous ring along the outline, feathered over 1.5 px (clear, full, clear across it), white 70% at
/// the top-left fading to 20% at the bottom-right (`hover` 0..1 lifts it). The ring's width follows the outline's own
/// normal, so it does not break at the corners.
pub fn rim_mesh(rect: Rect, radius: f32, hover: f32) -> Mesh {
    let pts = outline(rect, radius);
    let n = pts.len();
    let half = 0.75;
    let mut m = Mesh::default();
    for (i, p) in pts.iter().enumerate() {
        let d = diag(rect, *p);
        let a = (0.70 - 0.50 * d + 0.18 * hover).min(0.95);
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

    /// a title: bold caps with a little letter spacing (egui has no bold face, so it is drawn a hair apart, more as it grows)
    pub fn title(&self, painter: &Painter, pos: Pos2, text: &str, size: f32) {
        let font = FontId::proportional(size);
        let d = (size * 0.045).max(0.6);
        for dx in [0.0, d * 0.5, d] {
            let mut x = pos.x + dx;
            for ch in text.to_uppercase().chars() {
                let g = painter.layout_no_wrap(ch.to_string(), font.clone(), TITLE);
                let w = g.size().x;
                painter.galley(pos2(x, pos.y), g, Color32::WHITE);
                x += w + size * 0.05;
            }
        }
    }

    /// the backdrop, to paint behind everything
    pub fn paint_backdrop(&self, painter: &Painter, rect: Rect) {
        let (a, b) = (self.uv(rect.min), self.uv(rect.max));
        let mut m = Mesh::with_texture(self.tex.id());
        m.add_rect_with_uv(rect, Rect::from_min_max(a, b), Color32::WHITE);
        painter.add(Shape::mesh(m));
    }

    /// one piece of glass: the backdrop where it sits, a near-clear tint, a sheen, the rim, then the dot if asked
    pub fn paint(&self, painter: &Painter, rect: Rect, radius: f32, look: Look) {
        let mut m = fan(rect, radius, |p| self.uv(p), |_| Color32::WHITE);
        m.texture_id = self.tex.id();
        painter.add(Shape::mesh(m));
        if let Some(tex) = look.picture {
            self.picture(painter, rect, radius, tex, Color32::from_gray(170), true);
        }
        // almost clear: white 6% at the top to 2% at the bottom, the rim does the work
        painter.add(Shape::mesh(fan(rect, radius, |_| egui::epaint::WHITE_UV, |p| {
            let t = ((p.y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 1.0);
            white(0.06 - 0.04 * t + look.tint)
        })));
        let sheen = 0.03 + 0.08 * look.hover;
        painter.add(Shape::mesh(fan(rect, radius, |_| egui::epaint::WHITE_UV, |p| {
            white(sheen * (1.0 - diag(rect, p) * 1.8).max(0.0))
        })));
        if look.rim {
            painter.add(Shape::mesh(rim_mesh(rect, radius, look.hover)));
            // a faint 1 px highlight just inside the top edge
            let inset = radius.min(rect.width() / 2.0);
            painter.hline((rect.min.x + inset)..=(rect.max.x - inset), rect.min.y + 2.0, Stroke::new(1.0, white(0.10 + 0.06 * look.hover)));
        }
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
        self.paint(ui.painter(), rect, radius, Look::default());
    }

    /// a game card: glass with a dot; selected, it has the bright rim and a soft red glow
    /// `wash` is the game's own colour, laid over the glass while it has no title picture
    pub fn card(&self, painter: &Painter, rect: Rect, radius: f32, selected: bool, hover: f32, picture: Option<TextureId>, wash: Color32) {
        if selected {
            glow(painter, rect, radius, BRAND_RED, 1.0);
        }
        self.paint(painter, rect, radius, Look { hover: if selected { 1.0 } else { hover * 0.6 }, dot: true, picture, ..Look::default() });
        if picture.is_none() {
            // strongest top-left, where the light is
            painter.add(Shape::mesh(fan(rect, radius, |_| egui::epaint::WHITE_UV, |p| {
                let t = (((p.x - rect.min.x) / rect.width().max(1.0) + (p.y - rect.min.y) / rect.height().max(1.0)) / 2.0).clamp(0.0, 1.0);
                wash.gamma_multiply(0.34 - 0.2 * t)
            })));
        }
    }

    /// a card for something that is not there yet: glass with a dashed rim
    pub fn dashed_card(&self, painter: &Painter, rect: Rect, radius: f32, hover: f32, dim: f32) {
        self.paint(painter, rect, radius, Look { hover: hover * 0.5, rim: false, ..Look::default() });
        let mut pts = outline(rect, radius);
        pts.push(pts[0]);
        painter.extend(Shape::dashed_line(&pts, Stroke::new(1.2, white((0.35 + 0.3 * hover) * dim)), 6.0, 5.0));
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
            Color32::from_black_alpha((255.0 * (0.15 + 0.6 * t * t)) as u8)
        })));
    }

    /// hover 0..1 and press scale, eased out over 200 ms (none when the system turns animations off)
    fn interact(&self, ui: &Ui, resp: &Response) -> (f32, f32) {
        let time = if crate::animations() { ANIM_TIME } else { 0.0 };
        let ease = egui::emath::easing::cubic_out;
        let hover = ui.ctx().animate_bool_with_time_and_easing(resp.id.with("hover"), resp.hovered(), time, ease);
        let down = ui.ctx().animate_bool_with_time_and_easing(resp.id.with("down"), resp.is_pointer_button_down_on(), time * 0.75, ease);
        (hover, 1.0 - 0.03 * down)
    }

    /// hover for something the caller laid out itself (a card), eased the same way
    pub fn hover_of(&self, ui: &Ui, resp: &Response) -> f32 {
        self.interact(ui, resp).0
    }

    pub fn pill_button(&self, ui: &mut Ui, text: &str) -> Response {
        self.pill(ui, text, None, true)
    }

    /// a 40 px glass pill, `width` fixed when given
    pub fn pill(&self, ui: &mut Ui, text: &str, width: Option<f32>, enabled: bool) -> Response {
        let font = FontId::proportional(16.0);
        let galley = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE);
        let size = vec2(width.unwrap_or(galley.size().x + 48.0), 40.0);
        let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
        let (hover, scale) = if enabled { self.interact(ui, &resp) } else { (0.0, 1.0) };
        let r = Rect::from_center_size(rect.center(), rect.size() * scale);
        self.paint(ui.painter(), r, r.height() / 2.0, Look { hover, tint: 0.02 * hover, ..Look::default() });
        let color = if enabled { TITLE } else { BODY.gamma_multiply(0.6) };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, text, font, color);
        resp
    }

    pub fn circle_button(&self, ui: &mut Ui, icon: Icon, diameter: f32) -> Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(diameter, diameter), Sense::click());
        let (hover, scale) = self.interact(ui, &resp);
        let r = Rect::from_center_size(rect.center(), rect.size() * scale);
        self.paint(ui.painter(), r, diameter / 2.0, Look { hover, tint: 0.02 * hover, ..Look::default() });
        draw_icon(ui.painter(), icon, r.center(), diameter * 0.42);
        resp
    }

    /// the one primary action: the biggest, brightest glass, with a soft glow while it is enabled, dimmed to ~35% when not
    pub fn primary_button(&self, ui: &mut Ui, icon: Icon, diameter: f32, enabled: bool) -> Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(diameter, diameter), if enabled { Sense::click() } else { Sense::hover() });
        let (hover, scale) = if enabled { self.interact(ui, &resp) } else { (0.0, 1.0) };
        let r = Rect::from_center_size(rect.center(), rect.size() * scale);
        let p = ui.painter();
        if enabled {
            glow(p, r, diameter / 2.0, BRAND_RED, 1.4 + 0.6 * hover);
        }
        self.paint(p, r, diameter / 2.0, Look { hover: 0.55 + 0.45 * hover, tint: 0.04 + 0.03 * hover, dot: true, rim: true, picture: None });
        draw_icon(p, icon, r.center(), diameter * 0.42);
        if !enabled {
            p.circle_filled(r.center(), diameter / 2.0 + 1.0, Color32::from_rgba_unmultiplied(0x10, 0x16, 0x28, 166));
        }
        resp
    }

    /// a clear glass track with a brighter, frosted fill (white ~25%) and a sheen that moves while it runs
    /// (`fraction` None: not known yet, a short bar slides to and fro)
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
            white(0.25 + 0.22 * (1.0 - (x - phase).abs() * 3.5).max(0.0))
        })));
        ui.painter().add(Shape::mesh(rim_mesh(fill, fill.height() / 2.0, 0.8)));
    }
}

/// thin white line icons, `s` is the icon's half-size
pub fn draw_icon(p: &Painter, icon: Icon, c: Pos2, s: f32) {
    let stroke = Stroke::new((s * 0.11).clamp(1.2, 2.2), white(0.95));
    let line = |pts: Vec<Pos2>, closed: bool| {
        p.add(Shape::Path(egui::epaint::PathShape {
            points: pts,
            closed,
            fill: Color32::TRANSPARENT,
            stroke: stroke.into(),
        }));
    };
    let q = |x: f32, y: f32| c + vec2(x, y) * s;
    match icon {
        Icon::Play => line(vec![q(-0.4, -0.7), q(0.7, 0.0), q(-0.4, 0.7)], true),
        Icon::Plus => {
            line(vec![q(-0.6, 0.0), q(0.6, 0.0)], false);
            line(vec![q(0.0, -0.6), q(0.0, 0.6)], false);
        }
        Icon::Close => {
            line(vec![q(-0.6, -0.6), q(0.6, 0.6)], false);
            line(vec![q(-0.6, 0.6), q(0.6, -0.6)], false);
        }
        Icon::Prev => line(vec![q(0.3, -0.7), q(-0.4, 0.0), q(0.3, 0.7)], false),
        Icon::Next => line(vec![q(-0.3, -0.7), q(0.4, 0.0), q(-0.3, 0.7)], false),
        Icon::Home => {
            line(vec![q(-0.75, 0.0), q(0.0, -0.7), q(0.75, 0.0)], false);
            line(vec![q(-0.55, -0.12), q(-0.55, 0.65), q(0.55, 0.65), q(0.55, -0.12)], false);
            line(vec![q(-0.17, 0.65), q(-0.17, 0.2), q(0.17, 0.2), q(0.17, 0.65)], false);
        }
        Icon::Folder => line(
            vec![q(-0.75, -0.5), q(-0.2, -0.5), q(0.0, -0.3), q(0.75, -0.3), q(0.75, 0.55), q(-0.75, 0.55)],
            true,
        ),
        Icon::Share => {
            let (a, b, d) = (q(-0.6, 0.0), q(0.6, -0.6), q(0.6, 0.6));
            line(vec![b, a, d], false);
            for n in [a, b, d] {
                p.circle(n, s * 0.2, Color32::from_rgb(0x14, 0x1a, 0x2e), stroke);
            }
        }
    }
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
