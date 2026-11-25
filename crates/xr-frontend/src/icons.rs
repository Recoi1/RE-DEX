//! RE:DEX's own icons, drawn in code (no image files): one per element, all in one style. Each is a few convex parts
//! on a 2 x 2 grid; drawn as a dark ink outline around the whole shape (every part's outline first, so the seams
//! between parts vanish), then the parts lit from the top-left (light top, deeper bottom) in the element's colour, and
//! a white glint top-left. Tilted a few degrees on buttons, like stickers.
use egui::epaint::{Mesh, PathShape, PathStroke, Vertex};
use egui::{pos2, vec2, Color32, Painter, Pos2, Shape, Vec2};

use crate::theme::{deep, INK};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Play,
    Plus,
    Close,
    Prev,
    Next,
    Cartridge,
    AddGame,
    Person,
    NewPerson,
    Account,
    Import,
    Trash,
    Storage,
    Shirt,
    House,
    Friends,
    Globe,
    Monitor,
    Pin,
    Sliders,
    Puzzle,
    Power,
    Copy,
    Flag,
    Key,
    Group,
    PersonMinus,
    DoorOut,
    Enter,
    Check,
    Chat,
    Retry,
    Search,
    Warning,
    Info,
    Bolt,
    Pen,
    Drop,
    Comb,
    Palette,
    Eye,
    Volume,
    Gamepad,
    Keyboard,
    Sparkle,
    Mic,
    Pause,
    Gear,
}

#[derive(Clone, Copy)]
enum Tone {
    Main,
    Shade,
    Light,
    Ink,
    Of(Color32),
}

enum Part {
    Poly(Vec<Vec2>, Tone),
    Circle(Vec2, f32, Tone),
    /// an open stroke, round-ended
    Line(Vec<Vec2>, f32, Tone),
}

fn v(x: f32, y: f32) -> Vec2 {
    vec2(x, y)
}

fn rrect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32, t: Tone) -> Part {
    let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
    let mut pts = Vec::new();
    for (cx, cy, a0) in [(x1 - r, y0 + r, -90.0f32), (x1 - r, y1 - r, 0.0), (x0 + r, y1 - r, 90.0), (x0 + r, y0 + r, 180.0)] {
        for i in 0..=4 {
            let a = (a0 + 22.5 * i as f32).to_radians();
            pts.push(v(cx + r * a.cos(), cy + r * a.sin()));
        }
    }
    Part::Poly(pts, t)
}

fn poly(pts: &[(f32, f32)], t: Tone) -> Part {
    Part::Poly(pts.iter().map(|&(x, y)| v(x, y)).collect(), t)
}

fn line(pts: &[(f32, f32)], w: f32, t: Tone) -> Part {
    Part::Line(pts.iter().map(|&(x, y)| v(x, y)).collect(), w, t)
}

fn arc(cx: f32, cy: f32, r: f32, from: f32, to: f32, w: f32, t: Tone) -> Part {
    let n = 14;
    Part::Line((0..=n).map(|i| {
        let a = (from + (to - from) * i as f32 / n as f32).to_radians();
        v(cx + r * a.cos(), cy + r * a.sin())
    }).collect(), w, t)
}

fn circle(x: f32, y: f32, r: f32, t: Tone) -> Part {
    Part::Circle(v(x, y), r, t)
}

/// a person: head and shoulders
fn person(dx: f32, dy: f32, k: f32, t: Tone) -> [Part; 2] {
    let shoulders: Vec<(f32, f32)> = (0..=10).map(|i| {
        let a = std::f32::consts::PI * (1.0 + i as f32 / 10.0);
        (dx + 0.72 * k * a.cos(), dy + k * (0.9 + 0.78 * a.sin()))
    }).collect();
    [circle(dx, dy - 0.36 * k, 0.34 * k, t), poly(&shoulders, t)]
}

/// a small round badge on the bottom right with a sign in it
fn badge(color: Color32, sign: char) -> Vec<Part> {
    let (x, y, r) = (0.5, 0.5, 0.42);
    let mut p = vec![circle(x, y, r, Tone::Of(color))];
    match sign {
        '+' => {
            p.push(line(&[(x - 0.22, y), (x + 0.22, y)], 0.12, Tone::Light));
            p.push(line(&[(x, y - 0.22), (x, y + 0.22)], 0.12, Tone::Light));
        }
        '-' => p.push(line(&[(x - 0.22, y), (x + 0.22, y)], 0.12, Tone::Light)),
        _ => p.push(line(&[(x - 0.2, y + 0.01), (x - 0.05, y + 0.16), (x + 0.22, y - 0.14)], 0.11, Tone::Light)),
    }
    p
}

fn parts(icon: Icon) -> Vec<Part> {
    use Tone::*;
    match icon {
        Icon::Play => vec![poly(&[(-0.5, -0.78), (0.82, 0.0), (-0.5, 0.78)], Main)],
        Icon::Plus => vec![rrect(-0.2, -0.78, 0.2, 0.78, 0.12, Main), rrect(-0.78, -0.2, 0.78, 0.2, 0.12, Main)],
        Icon::Close => vec![line(&[(-0.55, -0.55), (0.55, 0.55)], 0.3, Main), line(&[(-0.55, 0.55), (0.55, -0.55)], 0.3, Main)],
        Icon::Prev => vec![line(&[(0.3, -0.62), (-0.32, 0.0), (0.3, 0.62)], 0.28, Main)],
        Icon::Next => vec![line(&[(-0.3, -0.62), (0.32, 0.0), (-0.3, 0.62)], 0.28, Main)],
        Icon::Cartridge => vec![
            poly(&[(-0.72, -0.86), (0.4, -0.86), (0.72, -0.56), (0.72, 0.86), (-0.72, 0.86)], Main),
            rrect(-0.48, -0.56, 0.42, 0.24, 0.1, Light),
            rrect(-0.48, 0.48, 0.42, 0.66, 0.06, Shade),
        ],
        Icon::AddGame => {
            let mut p = vec![
                poly(&[(-0.82, -0.86), (0.2, -0.86), (0.5, -0.58), (0.5, 0.7), (-0.82, 0.7)], Main),
                rrect(-0.6, -0.56, 0.24, 0.16, 0.1, Light),
            ];
            p.extend(badge(crate::theme::MINT, '+'));
            p
        }
        Icon::Person => person(0.0, 0.0, 1.0, Main).into(),
        Icon::NewPerson => {
            let mut p: Vec<Part> = person(-0.15, -0.05, 0.9, Main).into();
            p.extend(badge(crate::theme::MINT, '+'));
            p
        }
        Icon::PersonMinus => {
            let mut p: Vec<Part> = person(-0.15, -0.05, 0.9, Main).into();
            p.extend(badge(crate::theme::CORAL, '-'));
            p
        }
        Icon::Account => {
            let mut p = vec![circle(0.0, 0.0, 0.9, Main)];
            p.push(circle(0.0, -0.22, 0.28, Light));
            let s: Vec<(f32, f32)> = (0..=10).map(|i| {
                let a = std::f32::consts::PI * (1.0 + i as f32 / 10.0);
                (0.5 * a.cos(), 0.62 + 0.5 * a.sin())
            }).collect();
            p.push(poly(&s, Light));
            p
        }
        Icon::Import => vec![
            line(&[(-0.78, 0.2), (-0.78, 0.78), (0.78, 0.78), (0.78, 0.2)], 0.24, Shade),
            line(&[(0.0, -0.85), (0.0, 0.1)], 0.28, Main),
            poly(&[(-0.42, -0.05), (0.42, -0.05), (0.0, 0.42)], Main),
        ],
        Icon::Trash => vec![
            poly(&[(-0.62, -0.42), (0.62, -0.42), (0.5, 0.86), (-0.5, 0.86)], Main),
            rrect(-0.8, -0.68, 0.8, -0.44, 0.08, Shade),
            rrect(-0.22, -0.86, 0.22, -0.64, 0.06, Shade),
            line(&[(-0.2, -0.18), (-0.16, 0.6)], 0.1, Light),
            line(&[(0.2, -0.18), (0.16, 0.6)], 0.1, Light),
        ],
        Icon::Storage => vec![
            rrect(-0.74, -0.34, 0.74, 0.84, 0.1, Main),
            rrect(-0.86, -0.72, 0.86, -0.3, 0.1, Shade),
            rrect(-0.26, -0.08, 0.26, 0.14, 0.05, Light),
        ],
        Icon::Shirt => vec![
            poly(&[(-0.5, -0.66), (0.5, -0.66), (0.5, 0.84), (-0.5, 0.84)], Main),
            poly(&[(-0.5, -0.66), (-0.9, -0.3), (-0.66, 0.02), (-0.5, -0.12)], Main),
            poly(&[(0.5, -0.66), (0.9, -0.3), (0.66, 0.02), (0.5, -0.12)], Main),
            poly(&[(-0.22, -0.66), (0.22, -0.66), (0.0, -0.4)], Light),
        ],
        Icon::House => vec![
            rrect(-0.62, -0.1, 0.62, 0.84, 0.06, Main),
            poly(&[(-0.9, 0.02), (0.0, -0.86), (0.9, 0.02)], Shade),
            rrect(-0.18, 0.28, 0.18, 0.84, 0.05, Light),
        ],
        Icon::Friends => {
            let mut p: Vec<Part> = person(0.32, -0.1, 0.78, Shade).into();
            p.extend(person(-0.28, 0.06, 0.86, Main));
            p
        }
        Icon::Group => {
            let mut p: Vec<Part> = person(-0.5, -0.08, 0.62, Shade).into();
            p.extend(person(0.5, -0.08, 0.62, Shade));
            p.extend(person(0.0, 0.06, 0.78, Main));
            p
        }
        Icon::Globe => vec![
            circle(0.0, 0.0, 0.88, Main),
            line(&[(-0.86, 0.0), (0.86, 0.0)], 0.1, Light),
            arc(0.0, 0.0, 0.86, -90.0, 90.0, 0.1, Light),
            Part::Line((0..=14).map(|i| {
                let a = (-90.0 + 180.0 * i as f32 / 14.0f32).to_radians();
                v(0.38 * a.cos(), 0.86 * a.sin())
            }).collect(), 0.1, Light),
            Part::Line((0..=14).map(|i| {
                let a = (-90.0 + 180.0 * i as f32 / 14.0f32).to_radians();
                v(-0.38 * a.cos(), 0.86 * a.sin())
            }).collect(), 0.1, Light),
        ],
        Icon::Monitor => vec![
            rrect(-0.88, -0.72, 0.88, 0.4, 0.1, Main),
            rrect(-0.7, -0.56, 0.7, 0.24, 0.05, Light),
            poly(&[(-0.16, 0.4), (0.16, 0.4), (0.26, 0.7), (-0.26, 0.7)], Shade),
            rrect(-0.5, 0.66, 0.5, 0.84, 0.06, Shade),
        ],
        Icon::Pin => vec![
            poly(&[(-0.5, 0.02), (0.5, 0.02), (0.0, 0.9)], Main),
            circle(0.0, -0.3, 0.6, Main),
            circle(0.0, -0.3, 0.24, Light),
        ],
        Icon::Sliders => vec![
            line(&[(-0.8, -0.5), (0.8, -0.5)], 0.14, Shade),
            line(&[(-0.8, 0.0), (0.8, 0.0)], 0.14, Shade),
            line(&[(-0.8, 0.5), (0.8, 0.5)], 0.14, Shade),
            circle(-0.3, -0.5, 0.24, Main),
            circle(0.38, 0.0, 0.24, Main),
            circle(-0.1, 0.5, 0.24, Main),
        ],
        Icon::Puzzle => vec![
            rrect(-0.78, -0.52, 0.5, 0.82, 0.1, Main),
            circle(-0.14, -0.6, 0.26, Main),
            circle(0.6, 0.15, 0.26, Main),
        ],
        Icon::Power => vec![arc(0.0, 0.06, 0.7, -50.0, 230.0, 0.24, Main), line(&[(0.0, -0.86), (0.0, -0.06)], 0.24, Main)],
        Icon::Copy => vec![rrect(-0.36, -0.86, 0.82, 0.4, 0.12, Shade), rrect(-0.82, -0.4, 0.36, 0.86, 0.12, Main), rrect(-0.6, -0.12, 0.14, 0.02, 0.03, Light), rrect(-0.6, 0.22, 0.04, 0.36, 0.03, Light)],
        Icon::Flag => vec![
            line(&[(-0.62, -0.86), (-0.62, 0.86)], 0.18, Shade),
            poly(&[(-0.56, -0.8), (0.84, -0.56), (0.84, 0.08), (-0.56, 0.18)], Main),
            circle(-0.62, -0.86, 0.14, Shade),
        ],
        Icon::Key => vec![
            circle(-0.42, 0.0, 0.46, Main),
            circle(-0.42, 0.0, 0.16, Ink),
            line(&[(0.0, 0.0), (0.86, 0.0)], 0.22, Main),
            line(&[(0.5, 0.0), (0.5, 0.36)], 0.18, Main),
            line(&[(0.78, 0.0), (0.78, 0.3)], 0.18, Main),
        ],
        Icon::DoorOut => vec![
            rrect(-0.78, -0.86, 0.12, 0.86, 0.08, Shade),
            circle(-0.12, 0.06, 0.08, Light),
            line(&[(-0.1, 0.0), (0.56, 0.0)], 0.22, Main),
            poly(&[(0.44, -0.36), (0.9, 0.0), (0.44, 0.36)], Main),
        ],
        Icon::Enter => vec![
            line(&[(0.1, -0.78), (0.78, -0.78), (0.78, 0.78), (0.1, 0.78)], 0.2, Shade),
            line(&[(-0.86, 0.0), (0.12, 0.0)], 0.22, Main),
            poly(&[(0.0, -0.38), (0.46, 0.0), (0.0, 0.38)], Main),
        ],
        Icon::Check => vec![line(&[(-0.62, 0.02), (-0.18, 0.48), (0.66, -0.46)], 0.3, Main)],
        Icon::Chat => vec![
            rrect(-0.86, -0.72, 0.86, 0.44, 0.3, Main),
            poly(&[(-0.5, 0.3), (-0.06, 0.3), (-0.5, 0.86)], Main),
            circle(-0.38, -0.14, 0.11, Light),
            circle(0.0, -0.14, 0.11, Light),
            circle(0.38, -0.14, 0.11, Light),
        ],
        Icon::Retry => vec![arc(0.0, 0.05, 0.66, -160.0, 150.0, 0.22, Main), poly(&[(-0.95, -0.4), (-0.3, -0.5), (-0.72, 0.1)], Main)],
        Icon::Search => vec![circle(-0.18, -0.18, 0.62, Main), circle(-0.18, -0.18, 0.36, Light), line(&[(0.3, 0.3), (0.78, 0.78)], 0.3, Shade)],
        Icon::Warning => vec![
            poly(&[(0.0, -0.86), (0.92, 0.76), (-0.92, 0.76)], Main),
            line(&[(0.0, -0.3), (0.0, 0.22)], 0.18, Ink),
            circle(0.0, 0.52, 0.1, Ink),
        ],
        Icon::Info => vec![circle(0.0, 0.0, 0.88, Main), line(&[(0.0, -0.08), (0.0, 0.52)], 0.2, Light), circle(0.0, -0.4, 0.12, Light)],
        Icon::Bolt => vec![poly(&[(0.18, -0.9), (-0.62, 0.12), (-0.02, 0.12)], Main), poly(&[(-0.18, 0.9), (0.62, -0.12), (0.02, -0.12)], Main), poly(&[(-0.3, 0.12), (0.3, -0.12), (0.02, -0.12), (-0.02, 0.12)], Main)],
        Icon::Pen => vec![
            poly(&[(0.5, -0.86), (0.86, -0.5), (-0.36, 0.72), (-0.72, 0.36)], Main),
            poly(&[(-0.72, 0.36), (-0.36, 0.72), (-0.86, 0.86)], Light),
            poly(&[(0.5, -0.86), (0.86, -0.5), (0.68, -0.32), (0.32, -0.68)], Shade),
        ],
        Icon::Drop => vec![circle(0.0, 0.28, 0.56, Main), poly(&[(0.0, -0.9), (0.5, 0.06), (-0.5, 0.06)], Main)],
        Icon::Comb => {
            let mut p = vec![rrect(-0.86, -0.7, 0.86, -0.2, 0.1, Main)];
            for i in 0..6 {
                let x = -0.68 + i as f32 * 0.27;
                p.push(rrect(x, -0.3, x + 0.14, 0.82, 0.05, Main));
            }
            p
        }
        Icon::Palette => vec![
            circle(0.0, 0.0, 0.86, Main),
            circle(-0.38, -0.32, 0.17, Of(crate::theme::CORAL)),
            circle(0.08, -0.5, 0.17, Of(crate::theme::GOLD)),
            circle(0.46, -0.18, 0.17, Of(crate::theme::MINT)),
            circle(0.3, 0.38, 0.22, Ink),
        ],
        Icon::Eye => {
            let lens: Vec<(f32, f32)> = (0..=24).map(|i| {
                let a = i as f32 / 24.0 * std::f32::consts::TAU;
                (0.92 * a.cos(), 0.52 * a.sin() * (1.0 - 0.25 * a.cos().abs()))
            }).collect();
            vec![poly(&lens, Light), circle(0.0, 0.0, 0.4, Main), circle(0.0, 0.0, 0.16, Ink)]
        }
        Icon::Volume => vec![
            rrect(-0.86, -0.3, -0.42, 0.3, 0.06, Main),
            poly(&[(-0.5, -0.3), (0.1, -0.8), (0.1, 0.8), (-0.5, 0.3)], Main),
            arc(0.1, 0.0, 0.42, -50.0, 50.0, 0.14, Shade),
            arc(0.1, 0.0, 0.72, -50.0, 50.0, 0.14, Shade),
        ],
        Icon::Gamepad => vec![
            rrect(-0.9, -0.46, 0.9, 0.42, 0.3, Main),
            circle(-0.56, 0.3, 0.36, Main),
            circle(0.56, 0.3, 0.36, Main),
            line(&[(-0.62, -0.04), (-0.26, -0.04)], 0.13, Light),
            line(&[(-0.44, -0.22), (-0.44, 0.14)], 0.13, Light),
            circle(0.36, -0.12, 0.1, Light),
            circle(0.58, 0.06, 0.1, Light),
        ],
        Icon::Keyboard => {
            let mut p = vec![rrect(-0.92, -0.56, 0.92, 0.56, 0.14, Main)];
            for row in 0..2 {
                for k in 0..5 {
                    let (x, y) = (-0.7 + k as f32 * 0.3, -0.32 + row as f32 * 0.3);
                    p.push(rrect(x, y, x + 0.2, y + 0.18, 0.04, Light));
                }
            }
            p.push(rrect(-0.46, 0.28, 0.46, 0.4, 0.04, Light));
            p
        }
        Icon::Sparkle => vec![
            poly(&[(0.0, -0.9), (0.2, -0.2), (0.0, 0.0), (-0.2, -0.2)], Main),
            poly(&[(0.9, 0.0), (0.2, 0.2), (0.0, 0.0), (0.2, -0.2)], Main),
            poly(&[(0.0, 0.9), (-0.2, 0.2), (0.0, 0.0), (0.2, 0.2)], Main),
            poly(&[(-0.9, 0.0), (-0.2, -0.2), (0.0, 0.0), (-0.2, 0.2)], Main),
            circle(0.62, -0.62, 0.16, Shade),
        ],
        Icon::Mic => vec![
            rrect(-0.3, -0.88, 0.3, 0.28, 0.3, Main),
            arc(0.0, 0.0, 0.56, 10.0, 170.0, 0.14, Shade),
            line(&[(0.0, 0.56), (0.0, 0.86)], 0.14, Shade),
        ],
        Icon::Pause => vec![rrect(-0.6, -0.8, -0.14, 0.8, 0.1, Main), rrect(0.14, -0.8, 0.6, 0.8, 0.1, Main)],
        Icon::Gear => {
            // eight square teeth on a ring with a clear hub: reads as a gear even at 24 px
            let mut p = Vec::new();
            for k in 0..8 {
                let a = (k as f32 * 45.0f32).to_radians();
                let (c, s2) = (a.cos(), a.sin());
                let q = |r: f32, t: f32| (r * c - t * s2, r * s2 + t * c);
                p.push(poly(&[q(0.5, -0.2), q(0.94, -0.17), q(0.94, 0.17), q(0.5, 0.2)], Main));
            }
            p.push(circle(0.0, 0.0, 0.68, Main));
            p.push(circle(0.0, 0.0, 0.3, Ink));
            p.push(circle(0.0, 0.0, 0.18, Light));
            p
        }
    }
}

fn tone(t: Tone, hue: Color32) -> Color32 {
    match t {
        Tone::Main => hue,
        Tone::Shade => hue.lerp_to_gamma(deep(hue), 0.38),
        Tone::Light => Color32::from_rgb(0xF6, 0xF8, 0xFC),
        Tone::Ink => INK,
        Tone::Of(c) => c,
    }
}

fn circle_pts(c: Vec2, r: f32) -> Vec<Vec2> {
    (0..28).map(|i| {
        let a = i as f32 / 28.0 * std::f32::consts::TAU;
        c + vec2(a.cos(), a.sin()) * r
    }).collect()
}

/// `icon` in a `size` box centred on `c`, in the element's colour `hue`, turned `tilt` degrees
pub fn draw(p: &Painter, icon: Icon, c: Pos2, size: f32, hue: Color32, tilt: f32) {
    draw_alpha(p, icon, c, size, hue, tilt, 1.0);
}

/// the same, faded (a disabled element)
pub fn draw_alpha(p: &Painter, icon: Icon, c: Pos2, size: f32, hue: Color32, tilt: f32, alpha: f32) {
    let half = size / 2.0;
    let (s, co) = tilt.to_radians().sin_cos();
    let at = |u: Vec2| c + vec2(u.x * co - u.y * s, u.x * s + u.y * co) * half;
    let ow = (size * 0.055).max(1.2);
    let ink = INK.gamma_multiply(alpha);
    let parts = parts(icon);
    // the outline: every part grown by the ink, under every part's colour
    for part in &parts {
        match part {
            Part::Poly(pts, _) => {
                let pts: Vec<Pos2> = pts.iter().map(|u| at(*u)).collect();
                p.add(Shape::Path(PathShape { points: pts, closed: true, fill: ink, stroke: PathStroke::new(ow * 2.0, ink) }));
            }
            Part::Circle(o, r, _) => {
                p.circle_filled(at(*o), r * half + ow, ink);
            }
            Part::Line(pts, w, _) => {
                let pts: Vec<Pos2> = pts.iter().map(|u| at(*u)).collect();
                let width = w * half + ow * 2.0;
                for e in [pts[0], pts[pts.len() - 1]] {
                    p.circle_filled(e, width / 2.0, ink);
                }
                p.add(Shape::Path(PathShape { points: pts, closed: false, fill: Color32::TRANSPARENT, stroke: PathStroke::new(width, ink) }));
            }
        }
    }
    let mut glint = None;
    for part in &parts {
        match part {
            Part::Poly(pts, t) => fill(p, pts.iter().map(|u| at(*u)).collect(), tone(*t, hue), alpha, c, half),
            Part::Circle(o, r, t) => fill(p, circle_pts(*o, *r).iter().map(|u| at(*u)).collect(), tone(*t, hue), alpha, c, half),
            Part::Line(pts, w, t) => {
                let pts: Vec<Pos2> = pts.iter().map(|u| at(*u)).collect();
                let col = tone(*t, hue).gamma_multiply(alpha);
                let width = w * half;
                for e in [pts[0], pts[pts.len() - 1]] {
                    p.circle_filled(e, width / 2.0, col);
                }
                p.add(Shape::Path(PathShape { points: pts, closed: false, fill: Color32::TRANSPARENT, stroke: PathStroke::new(width, col) }));
            }
        }
        // the glint sits on the first big lit part, toward its top-left
        if glint.is_none() {
            let (pts, t) = match part {
                Part::Poly(pts, t) => (pts.clone(), *t),
                Part::Circle(o, r, t) => (circle_pts(*o, *r), *t),
                Part::Line(..) => continue,
            };
            if matches!(t, Tone::Main | Tone::Shade) {
                let n = pts.len() as f32;
                let mid = pts.iter().fold(Vec2::ZERO, |a, b| a + *b) / n;
                let min = pts.iter().fold(v(f32::MAX, f32::MAX), |a, b| v(a.x.min(b.x), a.y.min(b.y)));
                let max = pts.iter().fold(v(f32::MIN, f32::MIN), |a, b| v(a.x.max(b.x), a.y.max(b.y)));
                if (max - min).min_elem() > 0.5 {
                    glint = Some((mid + (min - mid) * 0.5, (max - min).min_elem()));
                }
            }
        }
    }
    if let Some((g, extent)) = glint {
        let r = (extent * 0.09 * half).max(1.0);
        p.circle_filled(at(g), r, Color32::from_white_alpha((230.0 * alpha) as u8));
        let d = vec2(extent * 0.16, extent * 0.06);
        p.line_segment([at(g + d), at(g + d * 2.2)], egui::Stroke::new(r * 0.9, Color32::from_white_alpha((120.0 * alpha) as u8)));
    }
}

/// a convex shape lit from the top-left: lighter at the top, the colour's deep shade toward the bottom
fn fill(p: &Painter, pts: Vec<Pos2>, col: Color32, alpha: f32, c: Pos2, half: f32) {
    let mut m = Mesh::default();
    let shade = |q: Pos2| {
        let t = (((q.y - c.y) / half + (q.x - c.x) / half * 0.35) * 0.5 + 0.5).clamp(0.0, 1.0);
        let lit = if t < 0.5 { col.lerp_to_gamma(Color32::WHITE, (0.5 - t) * 0.55) } else { col.lerp_to_gamma(deep(col), (t - 0.5) * 0.5) };
        lit.gamma_multiply(alpha)
    };
    let mid = pts.iter().fold(Vec2::ZERO, |a, b| a + b.to_vec2()) / pts.len() as f32;
    let mid = pos2(mid.x, mid.y);
    m.vertices.push(Vertex { pos: mid, uv: egui::epaint::WHITE_UV, color: shade(mid) });
    for q in &pts {
        m.vertices.push(Vertex { pos: *q, uv: egui::epaint::WHITE_UV, color: shade(*q) });
    }
    let n = pts.len() as u32;
    for i in 0..n {
        m.indices.extend([0, 1 + i, 1 + (i + 1) % n]);
    }
    p.add(Shape::mesh(m));
}
