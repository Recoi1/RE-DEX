//! Renders a liquid glass demo sheet to a PNG with a tiny software rasterizer over egui's tessellation, to compare with
//! the reference image by eye. Run: `cargo run -p xr-frontend --example glass_demo -- out.png`
use std::collections::HashMap;

use egui::epaint::Primitive;
use egui::{pos2, vec2, Align2, Color32, FontId, Rect, TextureId};
use xr_frontend::start::glass::{Glass, Icon};

const W: u32 = 740;
const H: u32 = 680;

fn main() {
    let out = std::env::args().nth(1).unwrap_or("glass-demo.png".into());
    let ctx = egui::Context::default();
    let mut glass = Glass::new(&ctx, [W, H]);
    glass.resize([W, H]);
    let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(W as f32, H as f32));
    let mut input = egui::RawInput::default();
    input.screen_rect = Some(screen);
    let mut full = ctx.run_ui(input, |ui| {
        let r = |x: f32, y: f32, w: f32, h: f32| Rect::from_min_size(pos2(x, y), vec2(w, h));
        glass.paint_backdrop(ui.painter(), screen);
        glass.panel(ui, r(64.0, 48.0, 434.0, 50.0), 25.0);
        glass.panel(ui, r(517.0, 48.0, 158.0, 56.0), 28.0);
        glass.panel(ui, r(64.0, 123.0, 86.0, 186.0), 22.0);
        glass.panel(ui, r(168.0, 123.0, 252.0, 158.0), 22.0);
        glass.panel(ui, r(445.0, 130.0, 230.0, 244.0), 24.0);
        ui.painter().text(pos2(467.0, 160.0), Align2::LEFT_CENTER, "LIQUID GLASS", FontId::proportional(14.0), Color32::WHITE);
        ui.painter().text(pos2(467.0, 182.0), Align2::LEFT_CENTER, "BANNER UI", FontId::proportional(14.0), Color32::WHITE);
        glass.panel(ui, r(168.0, 307.0, 118.0, 40.0), 20.0);
        glass.panel(ui, r(302.0, 307.0, 118.0, 40.0), 20.0);
        glass.panel(ui, r(64.0, 377.0, 158.0, 70.0), 22.0);
        glass.panel(ui, r(237.0, 376.0, 162.0, 162.0), 28.0);
        for (x, y) in [(415.0, 405.0), (468.0, 405.0)] {
            glass.panel(ui, r(x, y, 46.0, 46.0), 23.0);
        }
        glass.panel(ui, r(531.0, 405.0, 144.0, 46.0), 23.0);
        glass.panel(ui, r(64.0, 467.0, 72.0, 72.0), 18.0);
        glass.panel(ui, r(150.0, 467.0, 72.0, 72.0), 18.0);
        let icons = [Icon::Folder, Icon::Home, Icon::Play, Icon::Share];
        for (i, icon) in icons.into_iter().enumerate() {
            let x = 64.0 + 104.0 * i as f32;
            glass.panel(ui, r(x, 556.0, 70.0, 70.0), 18.0);
            xr_frontend::start::glass::draw_icon(ui.painter(), icon, pos2(x + 35.0, 591.0), 13.0);
        }
        xr_frontend::start::glass::draw_icon(ui.painter(), Icon::Plus, pos2(318.0, 457.0), 26.0);
        glass.panel(ui, r(467.0, 477.0, 124.0, 124.0), 62.0);
        xr_frontend::start::glass::draw_icon(ui.painter(), Icon::Play, pos2(529.0, 539.0), 26.0);
        for y in [487.0, 573.0] {
            glass.panel(ui, r(623.0, y, 52.0, 52.0), 26.0);
        }
        // live widgets, placed over the reference's empty spots
        ui.scope_builder(egui::UiBuilder::new().max_rect(r(168.0, 307.0, 118.0, 40.0)), |ui| {
            glass.pill_button(ui, "Play");
        });
        ui.scope_builder(egui::UiBuilder::new().max_rect(r(541.0, 420.0, 124.0, 16.0)), |ui| glass.progress(ui, Some(0.6), true));
    });

    let mut textures: HashMap<TextureId, (usize, usize, Vec<Color32>)> = HashMap::new();
    for (id, deltas) in std::mem::take(&mut full.textures_delta.set) {
        for delta in deltas {
            let egui::ImageData::Color(img) = &delta.image;
            match delta.pos {
                None => {
                    textures.insert(id, (img.width(), img.height(), img.pixels.clone()));
                }
                Some([ox, oy]) => {
                    let t = textures.get_mut(&id).unwrap();
                    for y in 0..img.height() {
                        for x in 0..img.width() {
                            t.2[(oy + y) * t.0 + ox + x] = img.pixels[y * img.width() + x];
                        }
                    }
                }
            }
        }
    }
    let mut fb = vec![[0f32; 4]; (W * H) as usize];
    for clipped in ctx.tessellate(full.shapes, 1.0) {
        let Primitive::Mesh(mesh) = clipped.primitive else { continue };
        let (tw, th, tpx) = &textures[&mesh.texture_id];
        let sample = |u: f32, v: f32| {
            let x = (u * *tw as f32 - 0.5).round().clamp(0.0, *tw as f32 - 1.0) as usize;
            let y = (v * *th as f32 - 0.5).round().clamp(0.0, *th as f32 - 1.0) as usize;
            tpx[y * tw + x].to_array().map(|c| c as f32 / 255.0)
        };
        for t in mesh.indices.chunks(3) {
            let [a, b, c] = [t[0], t[1], t[2]].map(|i| mesh.vertices[i as usize]);
            let area = (b.pos - a.pos).x * (c.pos - a.pos).y - (b.pos - a.pos).y * (c.pos - a.pos).x;
            if area.abs() < 1e-6 {
                continue;
            }
            let min = a.pos.min(b.pos).min(c.pos);
            let max = a.pos.max(b.pos).max(c.pos);
            for y in (min.y.floor().max(0.0) as u32)..=(max.y.ceil().min(H as f32 - 1.0) as u32) {
                for x in (min.x.floor().max(0.0) as u32)..=(max.x.ceil().min(W as f32 - 1.0) as u32) {
                    let p = pos2(x as f32 + 0.5, y as f32 + 0.5);
                    let w0 = ((b.pos - p).x * (c.pos - p).y - (b.pos - p).y * (c.pos - p).x) / area;
                    let w1 = ((c.pos - p).x * (a.pos - p).y - (c.pos - p).y * (a.pos - p).x) / area;
                    let w2 = 1.0 - w0 - w1;
                    if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                        continue;
                    }
                    let uv = a.uv.to_vec2() * w0 + b.uv.to_vec2() * w1 + c.uv.to_vec2() * w2;
                    let tex = sample(uv.x, uv.y);
                    let col = |k: usize| {
                        let f = |v: egui::epaint::Vertex| v.color.to_array()[k] as f32 / 255.0;
                        f(a) * w0 + f(b) * w1 + f(c) * w2
                    };
                    let src: [f32; 4] = std::array::from_fn(|k| tex[k] * col(k)); // premultiplied
                    let d = &mut fb[(y * W + x) as usize];
                    for k in 0..4 {
                        d[k] = src[k] + d[k] * (1.0 - src[3]);
                    }
                }
            }
        }
    }
    let bytes: Vec<u8> = fb.iter().flat_map(|p| [p[0], p[1], p[2], 1.0].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)).collect();
    let file = std::fs::File::create(&out).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&bytes).unwrap();
    println!("wrote {out}");
}
