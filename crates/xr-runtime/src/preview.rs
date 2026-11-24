//! `redex --preview STATE --cci DUMP --build DIR`: the character creator's live preview (start-screen plan 2 Task 5).
//! A headless game from a bedroom save state (`--make-preview`), at 60 frames a second, deterministic clock, software
//! renderer, no sound, logging to `<data>/logs/preview.log`. The start screen drives it:
//! - stdin, one command per line: `look <seq> <38 hex>` (the 19 appearance wire bytes; dropped with a log line unless
//!   `Appearance::from_bytes` takes them), `face up|down|left|right` (a 4-frame d-pad tap: the player turns in place),
//!   `orbit <yaw degrees> <zoom>` (the studio view's camera: yaw 0 faces us, zoom 1 to `ZOOM_MAX`), `quit`. Closing stdin
//!   quits too (exit 0).
//! - stdout: frames only (`xr_frontend::start::preview`), none from a rebuild's tick until 5 frames after it (the
//!   player is not drawn for one picture between looks) and none while the player wears the state's own outfit on the
//!   way between two looks (`Wardrobe::between`), each carrying the seq of the last look worn (0: the state's own):
//!   - the studio view (protocol v2, `zakuro_gpu::studio`): only the player's model, 360x480 with alpha and its floor
//!     shadow, every second frame (the game draws at 30 Hz) and on every frame the orbit changed (a 60 fps spin);
//!     A look's frames show a model drawn after its rebuild, never one still cached from before it.
//!   - else (v1) a head-to-toe crop of the top screen every second frame: after `FLAP` studio tries in a row fail (logged
//!     with why; back after `FLAP` pictures in a row), or with `XR_STUDIO=0`.
//!
//! The look is changed in place by xr-net's `Wardrobe`, on the game's thread in the per-frame hook ([`tick`]).
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::Mutex;

use xr_frontend::start::preview::{write_frame, Frame};
use xr_hle::services::hid::{InputState, PadState};
use xr_hle::FrameOutcome;
use xr_net::x::field::{self, Appearance, Facing, ViewProj, SCREEN_H, SCREEN_W};
use xr_net::x::puppet::{Calls, Dll};
use xr_net::x::wardrobe::Wardrobe;

use crate::log;

/// The per-frame hook runs [`tick`] while this is set (a `--preview` run).
pub static ON: AtomicBool = AtomicBool::new(false);
/// The wardrobe: `run` hands it looks, `tick` dresses them. Both only try the lock: a game call the hook makes can end
/// the frame while the hook holds it.
static WARDROBE: Mutex<Option<Wardrobe>> = Mutex::new(None);
/// The seq of the look the player was rebuilt in during this frame, for `emit`.
static REBUILT: Mutex<Option<u32>> = Mutex::new(None);
/// The player wears the game's own outfit on the way between two looks (`Wardrobe::between`): no frame then.
static BETWEEN: AtomicBool = AtomicBool::new(false);

/// World units from the feet to the top of the head (the bedroom, spike report).
const HEAD: f32 = 40.0;
/// The yellow item in the girl's hand in her bedroom state (the game's own prop, not part of any look): its mesh
/// (`zakuro_gpu` mesh id, from the state's buffers, so the same every run of a state made by `--make-preview`), found by
/// bisecting the girl's 52 meshes of a frame. Hidden in every preview (the boy's frames were checked to draw whole).
const HELD_ITEM: u64 = 0x0460_60DD_1606_5F5E;
/// Frames a `face` tap holds the d-pad.
const TAP: u64 = 4;
/// The 3DS's sideways turn from the field camera's clip space to the GPU's: x_gpu = y, y_gpu = -x (the spike found it
/// from one frame's clip positions of every draw; only this turn puts the player's meshes on the feet).
const SIDEWAYS: zakuro_gpu::studio::M4 = [[0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
/// The cylinder on the feet that holds the player, world units: the widest player seen reaches 8 (spike report); a draw
/// with most of its vertices in is kept, and any partly in is logged.
const REACH: f32 = 18.0;
const TALL: f32 = 48.0;
/// Studio pictures that fail in a row before the cropped frames go out instead, and that come in a row before the studio
/// is back: one bad or good frame never flips it.
const FLAP: u32 = 10;

/// A studio try's picture can go out: there is one (`built`: the game's picture its model was built in) and its model
/// was built after the last rebuild. One from before is the old look's: a failure, so a player never drawn again after
/// a rebuild hands over to the crops instead of freezing the preview (the FLAP run absorbs the usual few frames).
fn usable(built: Option<u64>, rebuilt_at: u64) -> bool {
    built.is_some_and(|b| b > rebuilt_at)
}

/// Whether the cropped frames go out in place of the studio's, from the studio's last tries.
#[derive(Default)]
struct Fallback {
    cropping: bool,
    run: u32,
}

impl Fallback {
    /// One try, a picture or not: Some(cropping) when that changes.
    fn on(&mut self, ok: bool) -> Option<bool> {
        // a run counts the tries that would flip it: failures while the studio is on, pictures while cropping
        self.run = if ok == self.cropping { self.run + 1 } else { 0 };
        if self.run < FLAP {
            return None;
        }
        (self.cropping, self.run) = (!self.cropping, 0);
        Some(self.cropping)
    }
}

/// The wardrobe's tick, on the game's thread (the per-frame hook).
pub fn tick(g: &mut impl Calls, dll: Option<Dll>) {
    let (Some(dll), Ok(mut w)) = (dll, WARDROBE.try_lock()) else { return };
    let w = w.get_or_insert_with(Wardrobe::new);
    if let Some(seq) = w.tick(g, dll) {
        *REBUILT.lock().unwrap_or_else(|e| e.into_inner()) = Some(seq);
    }
    BETWEEN.store(w.between(), Ordering::Relaxed);
}

/// The crop around the player on the top screen at 1x, (x, y, w, h): the feet and the head (HEAD units above them)
/// projected, a box 1.2 times their distance high and 0.6 times as wide, centred on them and kept on the screen. None
/// when the feet are not on the screen.
pub fn crop_box(cam: &ViewProj, feet: [f32; 3]) -> Option<[u32; 4]> {
    let f = cam.project(feet).filter(|f| (0.0..SCREEN_W).contains(&f[0]) && (0.0..SCREEN_H).contains(&f[1]))?;
    let top = cam.project([feet[0], feet[1] + HEAD, feet[2]])?;
    let d = (f[0] - top[0]).hypot(f[1] - top[1]);
    if !(d >= 1.0) {
        return None;
    }
    let h = (1.2 * d).min(SCREEN_H).round();
    let w = (0.6 * h).round();
    let x = ((f[0] + top[0] - w) / 2.0).round().clamp(0.0, SCREEN_W - w);
    let y = ((f[1] + top[1] - h) / 2.0).round().clamp(0.0, SCREEN_H - h);
    Some([x, y, w, h].map(|v| v as u32))
}

/// Whether the frame just run (`frame`) goes out: every second one, but none from the rebuild's frame until 5 after it.
fn emit(frame: u64, rebuilt: Option<u64>) -> bool {
    frame % 2 == 0 && !rebuilding(frame, rebuilt)
}

/// From the rebuild's frame until 5 after it: the player is not drawn for one picture between looks.
fn rebuilding(frame: u64, rebuilt: Option<u64>) -> bool {
    rebuilt.is_some_and(|r| (r..r + 5).contains(&frame))
}

/// `orbit <yaw> <zoom>`: the yaw in [0, 360), the zoom clamped to 1..ZOOM_MAX; None unless both are finite numbers.
fn parse_orbit(yaw: &str, zoom: &str) -> Option<(f32, f32)> {
    let (y, z) = (yaw.parse::<f32>().ok().filter(|v| v.is_finite())?, zoom.parse::<f32>().ok().filter(|v| v.is_finite())?);
    Some((y.rem_euclid(360.0), z.clamp(1.0, zakuro_gpu::studio::ZOOM_MAX)))
}

/// The studio's yaw that turns a player facing `f` to face us.
fn front(f: Facing) -> f32 {
    match f {
        Facing::Down => 0.0,
        Facing::Left => 90.0,
        Facing::Up => 180.0,
        Facing::Right => 270.0,
    }
}

/// Turns the studio on around the player, from the field camera (`crop`'s reads); false (logged) when it stays off.
fn studio_setup(m: &mut crate::exec::Machine) -> bool {
    if std::env::var("XR_STUDIO").is_ok_and(|v| v == "0") {
        log::log("preview: studio off (XR_STUDIO=0): the cropped frames go out");
        return false;
    }
    let cam = crate::field_data(m).and_then(|data| field::camera(&mut crate::Mem(&mut m.system().memory), data));
    let (Some(cam), Some((p, _))) = (cam, crate::player(m)) else {
        log::log("preview: studio off: no field camera or player");
        return false;
    };
    let r = cam.m[0];
    let l = (r[0] * r[0] + r[1] * r[1] + r[2] * r[2]).sqrt();
    zakuro_gpu::studio::set(Some(zakuro_gpu::studio::Config {
        g: zakuro_gpu::studio::mul(&SIDEWAYS, &cam.m),
        right: [r[0] / l, r[1] / l, r[2] / l],
        feet: p.pos,
        radius: REACH,
        height: TALL,
        // the field camera looks 33 degrees down; tipping the model 23 toward it leaves the studio's 10
        tilt: -23.0,
        game: (SCREEN_H, SCREEN_W),
    }));
    true
}

/// `look <seq> <38 hex>`: the seq and the appearance, None unless `Appearance::from_bytes` takes the bytes.
fn parse_look(seq: &str, hex: &str) -> Option<(u32, Appearance)> {
    let seq = seq.parse().ok()?;
    let bytes = (hex.len() == 38).then(|| (0..19).map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()).collect::<Option<Vec<u8>>>()).flatten()?;
    Some((seq, Appearance::from_bytes(&bytes)?))
}

fn crop(m: &mut crate::exec::Machine) -> Option<[u32; 4]> {
    let data = crate::field_data(m)?;
    let cam = field::camera(&mut crate::Mem(&mut m.system().memory), data)?;
    let (p, _) = crate::player(m)?;
    crop_box(&cam, p.pos)
}

pub fn run(cci: PathBuf, build: PathBuf, state: PathBuf) -> Result<(), String> {
    let data = build.parent().and_then(|p| p.parent()).ok_or("--build must be <data>/build/<id>")?;
    // beside a game that is playing: its own log, and the game's loadcro.csv left alone
    crate::init_log(&data.join("logs/preview.log"))?;
    std::panic::set_hook(Box::new(|p| log::stop(101, &format!("panic: {p}"))));
    let native_clock = crate::clock::init(true);
    // the maker's saves folder, never the player's <data>/user
    let user_dir = Some(crate::preview_user(data, &build, if state.file_name().is_some_and(|n| n.to_string_lossy().starts_with("girl")) { "girl" } else { "boy" }));
    let config = xr_hle::Config { data_dir: Some(data.to_path_buf()), user_dir, clock: Some(crate::FIXED_CLOCK), skip_idle_loops: !native_clock, no_cro_log: true, ..Default::default() };
    let mut m = crate::exec::Machine::boot(&cci, &build, config)?;
    let bytes = std::fs::read(&state).map_err(|e| format!("{state:?}: {e}"))?;
    let dll = redex_builder::dll_fingerprint(&build.join("xgame.dll"))?;
    let (mut n, _) = m.load_state(&bytes, dll).map_err(|e| format!("{state:?}: {e}"))?;
    log::log(&format!("preview: {state:?} loaded at frame {n}"));
    *WARDROBE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Wardrobe::new());
    ON.store(true, Ordering::Relaxed);
    zakuro_gpu::hide_meshes(vec![HELD_ITEM]);

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("preview stdin".into())
        .spawn(move || {
            // ends at EOF (the start screen closed our stdin): the channel's end tells the loop
            for line in std::io::stdin().lock().lines() {
                if line.ok().is_none_or(|l| tx.send(l).is_err()) {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;

    let mut out = std::io::stdout().lock();
    let (start, n0) = (std::time::Instant::now(), n);
    // seq of the look worn, the frame of the last rebuild, a look not handed to the wardrobe yet, the d-pad tap and
    // the frame it ends, the crop (once the camera is up)
    let (mut seq, mut rebuilt, mut pending, mut tap, mut cropped) = (0u32, None::<u64>, None::<(u32, Appearance)>, None::<(PadState, u64)>, None);
    let mut first = true;
    // the studio: on or not, the orbit asked for and the one last shown, its fallback to the crops, and the count of
    // the game's pictures at the last rebuild (a model from then or before is the old look's)
    let (mut studio, mut orbit, mut shown) = (false, (0.0f32, 1.0f32), None::<(f32, f32)>);
    let (mut fallback, mut rebuilt_at) = (Fallback::default(), 0u64);
    loop {
        loop {
            let line = match rx.try_recv() {
                Ok(l) => l,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => log::stop(0, "preview: stdin closed"),
            };
            let w: Vec<&str> = line.split_whitespace().collect();
            match w[..] {
                ["quit"] => log::stop(0, "preview: quit"),
                ["look", s, hex] => match parse_look(s, hex) {
                    Some((s, a)) => {
                        log::log(&format!("preview: look {s} wanted at frame {n}"));
                        pending = Some((s, a));
                    }
                    None => log::log(&format!("preview: dropped {line:?}: not a valid look")),
                },
                ["orbit", y, z] => match parse_orbit(y, z) {
                    Some(o) => orbit = o,
                    None => log::log(&format!("preview: dropped {line:?}")),
                },
                ["face", d] => match d {
                    "up" => tap = Some((PadState::UP, n + TAP)),
                    "down" => tap = Some((PadState::DOWN, n + TAP)),
                    "left" => tap = Some((PadState::LEFT, n + TAP)),
                    "right" => tap = Some((PadState::RIGHT, n + TAP)),
                    _ => log::log(&format!("preview: dropped {line:?}")),
                },
                _ => log::log(&format!("preview: dropped {line:?}")),
            }
        }
        if let Some((s, a)) = pending {
            if let Ok(mut w) = WARDROBE.try_lock() {
                w.get_or_insert_with(Wardrobe::new).want(s, a);
                pending = None;
            }
        }
        m.set_input(InputState { buttons: tap.filter(|t| n < t.1).map_or(PadState::empty(), |t| t.0), ..Default::default() });
        match m.run_frame() {
            FrameOutcome::Completed => n += 1,
            o => log::stop(1, &format!("preview: the game stopped ({o:?}) at frame {n}")),
        }
        drop(m.take_audio()); // no sound
        if let Some(s) = REBUILT.lock().unwrap_or_else(|e| e.into_inner()).take() {
            log::log(&format!("preview: look {s} on at frame {n}"));
            (seq, rebuilt, rebuilt_at) = (s, Some(n), zakuro_gpu::studio::frame_ends());
            zakuro_gpu::studio::refit();
        }
        if cropped.is_none() {
            // the player stands still: one crop for the whole run
            cropped = crop(&mut m);
            if let Some(c) = cropped {
                log::log(&format!("preview: crop {c:?} at frame {n}"));
                studio = studio_setup(&mut m);
            }
        }
        let quiet = rebuilding(n, rebuilt) || BETWEEN.load(Ordering::Relaxed);
        let mut out_frame = None;
        if let (true, false) = (studio, quiet) {
            if n % 2 == 0 || shown != Some(orbit) {
                let base = crate::player(&mut m).map_or(front(Facing::Right), |(p, _)| front(p.facing));
                let tried = m.system().studio_frame(base + orbit.0, orbit.1);
                let ok = usable(tried.as_ref().ok().map(|s| s.built), rebuilt_at);
                match (fallback.on(ok), &tried) {
                    (Some(true), Err(why)) => log::log(&format!("preview: {why}, {FLAP} times in a row, at frame {n}: the cropped frames go out")),
                    (Some(true), Ok(_)) => log::log(&format!("preview: no model drawn since the rebuild, {FLAP} times in a row, at frame {n}: the cropped frames go out")),
                    (Some(false), _) => log::log(&format!("preview: studio pictures again at frame {n}, {FLAP} in a row: the studio frames go out")),
                    _ => {}
                }
                match tried {
                    Ok(s) if ok && !fallback.cropping => {
                        if shown.is_none() {
                            log::log(&format!("preview: studio frames at frame {n}: {} draws, {:.2} ms", s.draws, s.time.as_secs_f64() * 1e3));
                        }
                        if n % 120 == 0 {
                            let behind = std::time::Instant::now().saturating_duration_since(start + std::time::Duration::from_secs_f64((n - n0) as f64 / 60.0));
                            log::log(&format!("preview: studio frame {n}: {} draws, {:.2} ms, {:.1} ms behind", s.draws, s.time.as_secs_f64() * 1e3, behind.as_secs_f64() * 1e3));
                        }
                        shown = Some(orbit);
                        let floor = Some(s.floor.map(|v| v.round().clamp(-32768.0, 32767.0) as i16));
                        out_frame = Some(Frame { seq, width: s.width as u16, height: s.height as u16, rgba: s.rgba, floor });
                    }
                    _ => {}
                }
            }
        }
        let crop_now = !studio || fallback.cropping;
        if let (None, Some([x, y, w, h]), true, true) = (&out_frame, cropped, crop_now && emit(n, rebuilt), !BETWEEN.load(Ordering::Relaxed)) {
            let top = m.screen(zakuro_common::Screen::Top);
            let rgba = (y..y + h).flat_map(|r| &top[((r * SCREEN_W as u32 + x) * 4) as usize..((r * SCREEN_W as u32 + x + w) * 4) as usize]).copied().collect();
            out_frame = Some(Frame { seq, width: w as u16, height: h as u16, rgba, floor: None });
        }
        if let Some(f) = out_frame {
            if std::mem::take(&mut first) {
                log::log(&format!("preview: first frame at frame {n}, {}x{}{}", f.width, f.height, if f.floor.is_some() { " (studio)" } else { "" }));
            }
            if write_frame(&mut out, &f).and_then(|()| out.flush()).is_err() {
                log::stop(0, "preview: stdout closed");
            }
        }
        // 60 frames a second from the start, as --realtime
        let due = start + std::time::Duration::from_secs_f64((n - n0) as f64 / 60.0);
        if let Some(d) = due.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// World x to pixel x and world y to 240 - y: one unit is one pixel, up is up.
    fn one_to_one() -> ViewProj {
        ViewProj { m: [[1.0 / 200.0, 0.0, 0.0, -1.0], [0.0, 1.0 / 120.0, 0.0, -1.0], [0.0; 4], [0.0, 0.0, 0.0, 1.0]] }
    }

    #[test]
    fn crop_box_frames_head_to_toe() {
        let cam = one_to_one();
        let feet = [200.0, 40.0, 0.0];
        assert_eq!(cam.project(feet), Some([200.0, 200.0]));
        let [x, y, w, h] = crop_box(&cam, feet).unwrap();
        let inside = |p: [f32; 2]| (x as f32..=(x + w) as f32).contains(&p[0]) && (y as f32..=(y + h) as f32).contains(&p[1]);
        assert!(inside([200.0, 200.0]) && inside(cam.project([200.0, 40.0 + HEAD, 0.0]).unwrap()), "{:?}", [x, y, w, h]);
        assert!((h as f32 / w as f32 - 1.0 / 0.6).abs() < 0.05, "{w}x{h}");
        assert!(x + w <= 400 && y + h <= 240);
        // the head above the screen: the box is kept on it
        let [x, y, w, h] = crop_box(&cam, [10.0, 230.0, 0.0]).unwrap();
        assert!(x + w <= 400 && y + h <= 240 && w > 0 && h > 0);
        // feet off the screen
        assert_eq!(crop_box(&cam, [200.0, -100.0, 0.0]), None);
        assert_eq!(crop_box(&cam, [500.0, 40.0, 0.0]), None);
    }

    #[test]
    fn no_frame_in_the_blank_window() {
        assert!(emit(10, None) && !emit(11, None));
        for f in 100..105 {
            assert!(!emit(f, Some(100)), "{f}");
        }
        assert!(emit(106, Some(100)) && !emit(105, Some(100)) && emit(98, Some(100)));
    }

    #[test]
    fn orbits_parse_wrapped_and_clamped() {
        assert_eq!(parse_orbit("90", "1.5"), Some((90.0, 1.5)));
        assert_eq!(parse_orbit("-90", "0.2"), Some((270.0, 1.0)));
        assert_eq!(parse_orbit("725", "9"), Some((5.0, zakuro_gpu::studio::ZOOM_MAX)));
        for (y, z) in [("x", "1"), ("1", ""), ("NaN", "1"), ("1", "inf")] {
            assert_eq!(parse_orbit(y, z), None, "{y} {z}");
        }
        // each facing's front is a quarter turn from the next, the state's own (right) at 270
        assert_eq!([Facing::Down, Facing::Left, Facing::Up, Facing::Right].map(front), [0.0, 90.0, 180.0, 270.0]);
    }

    #[test]
    fn crops_only_after_a_run_of_failures_and_back_after_a_run_of_pictures() {
        let mut f = Fallback::default();
        // a few failures (the first frames, a look's rebuild): still the studio
        for _ in 0..FLAP - 1 {
            assert_eq!(f.on(false), None);
        }
        assert_eq!(f.on(true), None, "one picture resets the run");
        for _ in 0..FLAP - 1 {
            assert_eq!(f.on(false), None);
        }
        assert_eq!(f.on(false), Some(true), "crops after FLAP failures in a row");
        assert!(f.cropping);
        // back only after FLAP pictures in a row
        for _ in 0..FLAP - 1 {
            assert_eq!(f.on(true), None);
        }
        assert_eq!(f.on(false), None);
        assert!(f.cropping, "a failure in the run keeps the crops");
        for _ in 0..FLAP - 1 {
            assert_eq!(f.on(true), None);
        }
        assert_eq!(f.on(true), Some(false));
        assert!(!f.cropping);
    }

    #[test]
    fn a_model_from_before_the_rebuild_counts_as_a_failure() {
        assert!(!usable(Some(5), 5) && !usable(Some(4), 5) && usable(Some(6), 5) && !usable(None, 0));
        // a player never drawn again after a rebuild: the crops take over instead of the preview freezing
        let mut f = Fallback::default();
        let flips: Vec<_> = (0..FLAP).filter_map(|_| f.on(usable(Some(5), 5))).collect();
        assert_eq!(flips, [true]);
    }

    #[test]
    fn looks_parse_only_when_valid() {
        let red = "0601004864090101cb08034100000000000000";
        assert_eq!(parse_look("3", red).map(|(s, a)| (s, a.gender)), Some((3, 0)));
        assert!(parse_look("x", red).is_none());
        assert!(parse_look("3", &red[2..]).is_none());
        assert!(parse_look("3", "0601007fffffffffffffffffffffffffffffff").is_none());
        assert!(parse_look("3", "0601004864090101cb0803410000000000000g").is_none());
    }
}
