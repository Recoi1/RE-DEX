//! RE:DEX's design tokens: colours, type, spacing, radii and motion, shared by the start screen (start/glass.rs) and
//! the panels over the game. The type is Montserrat (SIL Open Font License 1.1, assets/fonts/OFL.txt): ExtraBold for
//! titles, buttons and numbers, SemiBold for everything else.
use egui::{Color32, Context, FontData, FontDefinitions, FontFamily, FontId, Id};

/// what the layout is drawn for; a bigger window scales it up whole (start screen)
pub const CANVAS: [f32; 2] = [1040.0, 640.0];

/// one scale for a whole window, from its size in points: the canvas fitted, never below 0.8 nor above 2
pub fn ui_scale(w: f32, h: f32) -> f32 {
    (w / CANVAS[0]).min(h / CANVAS[1]).clamp(0.8, 2.0)
}

/// the panels over the game: the same fit, but never smaller than drawn (a 1x window keeps its text), times
/// REDEX_UI_ZOOM when it is set
pub fn game_scale(w: f32, h: f32) -> f32 {
    static ZOOM: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    let zoom = *ZOOM.get_or_init(|| std::env::var("REDEX_UI_ZOOM").ok().and_then(|z| z.parse().ok()).unwrap_or(1.0));
    (w / CANVAS[0]).min(h / CANVAS[1]).clamp(1.0, 2.0) * zoom
}

// colours
pub const INK: Color32 = Color32::from_rgb(0x0A, 0x0E, 0x1A);
pub const TITLE: Color32 = Color32::from_rgb(0xF4, 0xF6, 0xFB);
pub const BODY: Color32 = Color32::from_rgb(0xC9, 0xD1, 0xE0);
pub const MUTED: Color32 = Color32::from_rgb(0xA3, 0xAC, 0xC2);
pub const BRAND: Color32 = Color32::from_rgb(0xE8, 0x36, 0x4A);
pub const BRAND_HI: Color32 = Color32::from_rgb(0xFF, 0x64, 0x74);
pub const BRAND_DEEP: Color32 = Color32::from_rgb(0x7E, 0x0C, 0x1F);
/// a value inside a sentence (a count, a code), as the reference UIs mark them
pub const GOLD: Color32 = Color32::from_rgb(0xFF, 0xD2, 0x3F);
pub const MINT: Color32 = Color32::from_rgb(0x3D, 0xDC, 0x97);
pub const AMBER: Color32 = Color32::from_rgb(0xFF, 0xB5, 0x47);
pub const CORAL: Color32 = Color32::from_rgb(0xFF, 0x6B, 0x6B);
pub const AZURE: Color32 = Color32::from_rgb(0x3F, 0xB2, 0xFF);
pub const VIOLET: Color32 = Color32::from_rgb(0xA4, 0x7B, 0xFF);
pub const SLATE: Color32 = Color32::from_rgb(0xC3, 0xCC, 0xDD);
pub const TEAL: Color32 = Color32::from_rgb(0x2E, 0xD3, 0xC6);
pub const PINK: Color32 = Color32::from_rgb(0xF0, 0x6A, 0xB0);
pub const BLUE: Color32 = Color32::from_rgb(0x5B, 0x8C, 0xFF);
/// a panel over the game: the game is busy and bright, so the glass is smoked
pub const SMOKE: Color32 = Color32::from_rgba_premultiplied(11, 14, 26, 240);

// type sizes (points on the canvas)
pub const DISPLAY: f32 = 26.0;
pub const H1: f32 = 20.0;
pub const H2: f32 = 16.0;
pub const LABEL: f32 = 14.5;
pub const BODY_SIZE: f32 = 13.5;
pub const CAPTION: f32 = 12.0;
pub const EYEBROW: f32 = 11.5;

// radii (spacing is a 4 pt grid: 4, 8, 12, 16, 24, 32)
pub const R_CARD: f32 = 18.0;
pub const R_PANEL: f32 = 22.0;

// motion (seconds)
pub const HOVER: f32 = 0.08;
pub const PRESS: f32 = 0.05;
pub const RELEASE: f32 = 0.14;
pub const OPEN: f32 = 0.26;
pub const CLOSE: f32 = 0.14;
pub const HOVER_SCALE: f32 = 1.04;
pub const PRESS_SCALE: f32 = 0.95;

/// overshoots a little and settles (the release of a press, a panel opening)
pub fn back_out(t: f32) -> f32 {
    let (c1, t) = (1.70158, t.clamp(0.0, 1.0) - 1.0);
    1.0 + (c1 + 1.0) * t * t * t + c1 * t * t
}

pub fn quad_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

const HEAVY: &str = "Montserrat ExtraBold";
const SEMI: &str = "Montserrat SemiBold";

/// Montserrat in front of egui's own fonts (which stay as the fallback for what it lacks), once per context; it is in
/// use from the next frame
pub fn install_fonts(ctx: &Context) {
    let id = Id::new("redex fonts installed");
    if ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(false) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(id, true));
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(HEAVY.into(), FontData::from_static(include_bytes!("../../../assets/fonts/Montserrat-ExtraBold.ttf")).into());
    fonts.font_data.insert(SEMI.into(), FontData::from_static(include_bytes!("../../../assets/fonts/Montserrat-SemiBold.ttf")).into());
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    fonts.families.insert(FontFamily::Proportional, std::iter::once(SEMI.to_owned()).chain(fallback.iter().cloned()).collect());
    fonts.families.insert(FontFamily::Name(HEAVY.into()), std::iter::once(HEAVY.to_owned()).chain(fallback).collect());
    ctx.set_fonts(fonts);
}

/// the heavy weight, or the body face until it is in use (a context that never installed it: tests, a first frame)
pub fn heavy(ctx: &Context, size: f32) -> FontId {
    let family = FontFamily::Name(HEAVY.into());
    if ctx.fonts(|f| f.families().contains(&family)) {
        FontId::new(size, family)
    } else {
        FontId::proportional(size)
    }
}

/// the colour's own deep shade, what a label's shadow and an icon's outline take on that colour (never plain black)
pub fn deep(c: Color32) -> Color32 {
    Color32::from_rgb((c.r() as f32 * 0.16) as u8, (c.g() as f32 * 0.16) as u8, (c.b() as f32 * 0.2 + 6.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance contrast of two opaque colours
    fn contrast(a: Color32, b: Color32) -> f32 {
        let l = |c: Color32| {
            let ch = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
        };
        let (x, y) = (l(a), l(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// text colours keep WCAG AA (4.5:1) on the lightest ground they sit on: the backdrop's brightest blob under a
    /// panel's tint, and the smoked panel over the game
    #[test]
    fn text_keeps_aa_contrast() {
        // the backdrop's red corner under glass (measured on a capture), and the smoke on a white game frame
        for ground in [Color32::from_rgb(0x5A, 0x1E, 0x3A), Color32::from_rgb(0x2C, 0x30, 0x42)] {
            for (name, c) in [("title", TITLE), ("body", BODY), ("muted", MUTED), ("gold", GOLD), ("mint", MINT)] {
                assert!(contrast(c, ground) >= 4.5, "{name} on {ground:?}: {}", contrast(c, ground));
            }
        }
    }

    #[test]
    fn scale_fits_the_canvas() {
        assert_eq!(ui_scale(1040.0, 640.0), 1.0);
        assert!((ui_scale(1920.0, 1080.0) - 1.6875).abs() < 1e-4);
        assert_eq!(ui_scale(400.0, 300.0), 0.8);
        assert!(back_out(0.6) > 1.0 && (back_out(1.0) - 1.0).abs() < 1e-6 && back_out(0.0).abs() < 1e-6);
    }
}
