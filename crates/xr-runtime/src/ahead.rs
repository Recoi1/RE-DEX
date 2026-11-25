//! Responsive movement, the runtime's side (roadmap 2b): once a refresh, before the game runs, X's field is read
//! (never written) through DllField's data segment, the input taken, and xr_net's Predictor says where the trainer and
//! the camera go ahead of frame interpolation in the pictures in between of the picture this refresh draws. Those
//! become the GPU's clip-space transforms (zakuro_gpu::Prediction). Off (the setting, or Smooth motion off) nothing is
//! read and the GPU has none, so the pictures are frame interpolation's, bit for bit.
use xr_hle::services::hid::PadState;
use xr_hle::System;
use xr_net::x::field::{self, Facing};
use xr_net::x::predict::{Held, Predictor, Seen};
use xr_net::Guest;
use zakuro_gpu::studio::{self, M4};

/// the 3DS's sideways framebuffer: the GPU's clip x is the picture's up (see preview.rs, the studio)
const SIDEWAYS: M4 = [[0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
const IDENTITY: M4 = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
/// the cylinder on the feet that holds the trainer, world units: on foot about 8 wide, the bike longer; a neighbour a
/// tile away never fits in whole
const REACH: f32 = 14.0;
const TALL: f32 = 48.0;

/// what the runtime keeps for it
#[derive(Default)]
pub struct Ahead {
    pub predictor: Predictor,
    /// the GPU's draws w/ the field camera's projection at the last refresh's start (more now: that refresh drew its
    /// picture of the field, w/ the prediction it had; frame interpolation only finishes a picture a refresh later)
    field_draws: u64,
    on: bool,
}

/// someone (an NPC, a puppet) within this of the trainer turns Responsive movement off: their draws and shadows at
/// the trainer's feet (the cylinder is 14 across) must never move w/ the input (world units, under a tile)
const NEAR: f32 = 13.0;
/// ... and the GPU only learns the trainer's draws w/ nobody within this (a tile and a third)
const NEAR_LEARN: f32 = 24.0;

/// how far the nearest other move model (an NPC, a multiplayer puppet) is from `pos`, across; unreadable is 0
fn nearest(g: &mut impl Guest, data: u32, pos: [f32; 3]) -> f32 {
    let mut near = || -> Option<f32> {
        let p = field::player(g, data)?;
        let slots = field::slots(g, p.manager)?;
        let mut nearest = f32::INFINITY;
        for slot in slots.iter().filter(|slot| slot.addr != p.move_model) {
            let mut at = |o: u32| g.f32(slot.addr + field::MM_POS + o);
            let (x, z) = (at(0)?, at(8)?);
            nearest = nearest.min((x - pos[0]).hypot(z - pos[2]));
        }
        Some(nearest)
    };
    near().unwrap_or(0.0)
}

/// the trainer's look: its outfit (gender and fashion) and its form; another one reloads its model (the GPU learns its
/// draws again)
fn look(g: &mut impl Guest, data: u32, form: u8) -> u64 {
    let a = field::appearance(g, data);
    let bytes = a.map(|a| [&[a.gender][..], &a.fashion[..]].concat()).unwrap_or_default();
    bytes.iter().chain([form].iter()).fold(0xCBF2_9CE4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01B3))
}

/// the field as this refresh's picture draws it: the move model's place (the snapshot's lags it an update)
fn seen(g: &mut impl Guest, data: u32) -> Option<(Seen, field::ViewProj)> {
    let p = field::player(g, data)?;
    let cam = field::camera(g, data)?;
    let pos = [g.f32(p.move_model + field::MM_POS)?, g.f32(p.move_model + field::MM_POS + 4)?, g.f32(p.move_model + field::MM_POS + 8)?];
    let snap = g.u32(data + field::SNAPSHOT + field::SNAP_CONTROLLER)?;
    let form = g.u8(snap + field::CTRL_FORM)?;
    // the view-projection's w row is the camera's forward (see field::camera)
    let forward = [cam.m[3][0], cam.m[3][2]];
    Some((Seen { map: p.map, pos, eye: cam.eye()?, facing: p.facing, form, moving: p.moving, forward, alone: true }, cam))
}

/// the d-pad one way, B, and the buttons that open or answer something
pub fn held(buttons: PadState, touch: bool) -> Held {
    let ways = [(PadState::UP, Facing::Up), (PadState::DOWN, Facing::Down), (PadState::LEFT, Facing::Left), (PadState::RIGHT, Facing::Right)];
    let mut pressed = ways.iter().filter(|(b, _)| buttons.contains(*b)).map(|&(_, f)| f);
    let dir = match (pressed.next(), pressed.next()) {
        (Some(f), None) => Some(f),
        _ => None,
    };
    let other = PadState::A | PadState::X | PadState::Y | PadState::L | PadState::R | PadState::START | PadState::SELECT | PadState::ZL | PadState::ZR;
    Held { dir, run: buttons.contains(PadState::B), other: buttons.intersects(other) || touch }
}

/// XR_PREDICT_LOG=1 (test-hooks builds): a line a refresh of what Responsive movement drew ahead, for the measurements
fn log_on() -> bool {
    #[cfg(feature = "mp-tests")]
    {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("XR_PREDICT_LOG").is_some_and(|v| v == "1"))
    }
    #[cfg(not(feature = "mp-tests"))]
    false
}

fn translate(t: [f32; 3]) -> M4 {
    [[1.0, 0.0, 0.0, t[0]], [0.0, 1.0, 0.0, t[1]], [0.0, 0.0, 1.0, t[2]], [0.0, 0.0, 0.0, 1.0]]
}

/// a turn about the vertical through `at`, `yaw` radians about +y
fn turn_about(at: [f32; 3], yaw: f32) -> M4 {
    let (s, c) = yaw.sin_cos();
    let spin = [[c, 0.0, s, 0.0], [0.0, 1.0, 0.0, 0.0], [-s, 0.0, c, 0.0], [0.0, 0.0, 0.0, 1.0]];
    studio::mul(&translate(at), &studio::mul(&spin, &translate(at.map(|v| -v))))
}

/// world transform `w` as the clip-space one the GPU folds in, `g` world to clip and `gi` back
fn in_clip(g: &M4, gi: &M4, w: &M4) -> M4 {
    if *w == IDENTITY { IDENTITY } else { studio::mul(g, &studio::mul(w, gi)) }
}

impl Ahead {
    /// once a refresh, before the game runs
    pub fn refresh(&mut self, s: &mut System, data: Option<u32>) {
        if !(s.responsive && s.gpu.interpolates()) {
            if self.on {
                *self = Ahead::default();
                s.gpu.set_prediction(None);
                s.gpu.set_extra_twin(false);
            }
            return;
        }
        self.on = true;
        let field_draws = s.gpu.field_draws();
        let drawn = field_draws != self.field_draws;
        self.field_draws = field_draws;
        let input = s.services.hid.input;
        let held = held(input.buttons, input.touch.is_some());
        let mut now = data.and_then(|data| seen(&mut crate::Mem(&mut s.memory), data));
        let (nearest, look) = match (data, now) {
            (Some(data), Some((seen, _))) => {
                let mut g = crate::Mem(&mut s.memory);
                (nearest(&mut g, data, seen.pos), look(&mut g, data, seen.form))
            }
            _ => (0.0, 0),
        };
        let near = nearest < NEAR;
        if let Some((seen, _)) = now.as_mut() {
            seen.alone = !near;
        }
        // the pictures in between and the own picture drawn ahead after them (t = 1, see Gpu::set_extra_twin)
        let ts: Vec<f32> = (0..s.gpu.twins()).map(|k| s.gpu.twin_t(k)).chain((s.gpu.twins() > 0).then_some(1.0)).collect();
        let mut ahead = self.predictor.refresh(now.map(|(seen, _)| seen), held, s.frames, drawn, &ts);
        // nothing is drawn ahead till the GPU knows the trainer's draws (it learns them standing, see zakuro's predict)
        if ahead.is_some() && !s.gpu.knows_trainer() {
            self.predictor.not_drawn_ahead();
            ahead = None;
        }
        // the extra twin only for a picture drawn ahead; w/o room for it (the VRAM guard) the picture isn't drawn ahead
        s.gpu.set_extra_twin(ahead.is_some());
        if ahead.is_some() && s.gpu.extra_twin().is_none() {
            self.predictor.not_drawn_ahead();
            s.gpu.set_extra_twin(false);
            ahead = None;
        }
        if log_on() {
            let c = self.predictor.counts;
            let r = |v: f32| (v * 100.0).round() / 100.0;
            crate::log::log(&format!(
                "ahead: frame {} drawn {drawn} near {near} knows {} look {look:x} held {held:?} seen {:?} ts {:?} player {:?} camera {:?} counts sets {} blends {} snaps {} refused {} worst {:.2}",
                s.frames,
                s.gpu.knows_trainer(),
                now.map(|(seen, _)| (seen.pos.map(r), seen.facing, seen.form, seen.moving)),
                ts.iter().map(|&t| r(t)).collect::<Vec<_>>(),
                ahead.as_ref().map(|a| a.player.iter().map(|(d, yaw)| (d.map(r), r(*yaw))).collect::<Vec<_>>()),
                ahead.as_ref().map(|a| a.camera.iter().map(|d| d.map(r)).collect::<Vec<_>>()),
                c.sets,
                c.blends,
                c.snaps,
                c.refused,
                c.worst
            ));
        }
        // in the field the GPU always has one: it counts the field's draws (see drawn) and learns the trainer's while
        // calm (standing, nobody near); it moves draws only w/ the predictor's moves
        let prediction = now.and_then(|(seen, cam)| {
            let g = studio::mul(&SIDEWAYS, &cam.m);
            let gi = studio::invert(&g)?;
            let (mut player, mut world) = (Vec::new(), Vec::new());
            if let Some(ahead) = &ahead {
                world = ahead.camera.iter().map(|&d| in_clip(&g, &gi, &if d == [0.0; 3] { IDENTITY } else { translate(d.map(|v| -v)) })).collect();
                player = ahead
                    .player
                    .iter()
                    .zip(&ahead.camera)
                    .map(|(&(d, yaw), &c)| {
                        let moved = [d[0] - c[0], d[1] - c[1], d[2] - c[2]];
                        let w = match (moved == [0.0; 3], yaw == 0.0) {
                            (true, true) => IDENTITY,
                            (_, true) => translate(moved),
                            _ => studio::mul(&translate(moved), &turn_about(seen.pos, yaw)),
                        };
                        in_clip(&g, &gi, &w)
                    })
                    .collect();
            }
            let learn = nearest >= NEAR_LEARN && !seen.moving && seen.form <= 1;
            Some(zakuro_gpu::Prediction { g, feet: seen.pos, radius: REACH, height: TALL, map: seen.map, look, learn, player, world })
        });
        s.gpu.set_prediction(prediction);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dpad_one_way_and_the_buttons_that_open_things() {
        assert_eq!(held(PadState::RIGHT | PadState::B, false), Held { dir: Some(Facing::Right), run: true, other: false });
        assert_eq!(held(PadState::RIGHT | PadState::UP, false).dir, None, "two ways");
        assert!(held(PadState::X, false).other && held(PadState::empty(), true).other);
        assert_eq!(held(PadState::CIRCLE_RIGHT, false), Held::default(), "the circle pad is followed, not predicted");
    }

    #[test]
    fn a_world_move_in_clip_space_moves_clip_points() {
        let g: M4 = [[0.0, 3.7, 0.0, 1.0], [-2.2, 0.0, 0.0, 2.0], [0.0, 0.0, -1.0, -20.0], [0.0, 0.0, -1.0, 300.0]];
        let gi = studio::invert(&g).unwrap();
        let p = [10.0, 5.0, 40.0, 1.0];
        let c = in_clip(&g, &gi, &translate([2.0, 0.0, -1.0]));
        let a = studio::mul_v(&c, &studio::mul_v(&g, &p));
        let b = studio::mul_v(&g, &[12.0, 5.0, 39.0, 1.0]);
        for i in 0..4 {
            assert!((a[i] - b[i]).abs() < 1e-2, "{a:?} {b:?}");
        }
        assert_eq!(in_clip(&g, &gi, &IDENTITY), IDENTITY);
        let q = studio::mul_v(&turn_about([10.0, 0.0, 40.0], std::f32::consts::FRAC_PI_2), &[10.0, 3.0, 41.0, 1.0]);
        assert!((q[0] - 11.0).abs() < 1e-4 && (q[2] - 40.0).abs() < 1e-4, "{q:?}");
    }
}
