//! Responsive movement (roadmap 2b): where the local trainer and the field camera are drawn ahead of frame
//! interpolation, from live input and the game's own speeds. Render side only: the runtime turns `Ahead` into the
//! GPU's transforms for the pictures in between; nothing here is written to game memory, and what other players see
//! comes from the game's real positions.
//!
//! The model (measured on the 100% save, docs/re/x-field-move-models.md):
//! - Frame interpolation shows a picture's pictures in between at t of the way from the picture b4 (`last`) to it
//!   (`now`), one field update behind. Ahead, the trainer is shown at `now + v t` instead, at most one update past the
//!   game's newest picture.
//! - **Moving:** v is the game's own last step. Let go (or turned another way), the trainer stops at the next tile
//!   centre, as the game does.
//! - **Starting:** facing the way pressed, the game's first step is in the picture an update after the press; ahead, the
//!   trainer starts at the press, so it gets there w/ the game's step. Pressed another way, the trainer turns at once;
//!   the game's walk then starts two updates after its own turn, and the trainer eases into it over those two.
//! - **The camera** follows the trainer with a lag of its own; ahead it goes on as its last step did (scaled as that
//!   step grew or shrank).
//! - **Reconciling:** at each picture, where the last set ended and where this one starts differ only when the game
//!   didn't do what was predicted (a wall, a ledge, a person). The difference fades over three pictures in between,
//!   or goes at once past half a tile.
//! - **Off:** anything but walking, running or the bike on the d-pad (the circle pad, roller skates in the 100% save,
//!   and surfing aren't predicted), a map change or a warp, a game moving the trainer on its own (a cutscene) after
//!   the d-pad's let go, and while another button was pressed last (a menu, a talk, an item) until the game answers a
//!   press by moving. The d-pad's ways turn with the camera; a camera turned between two ways (Lumiose's round
//!   boulevards) only follows the game's own moves.

use crate::x::field::Facing;

/// one tile, world units
pub const TILE: f32 = 18.0;
/// per field update: walking (18/7), running, on the bike
pub const WALK: f32 = 18.0 / 7.0;
pub const RUN: f32 = 4.5;
pub const BIKE: f32 = 9.0;
/// updates from a press to the game's first step, facing the way pressed, and from its turn to its first step
pub const START: f32 = 1.0;
pub const AFTER_TURN: f32 = 2.0;
/// updates a start may run late before it counts as refused (a wall, a menu): the game's first step comes right on time
pub const GRACE: f32 = 0.5;
/// updates from a press to the game's own turn (it shows a picture and a half later); late by GRACE, it's refused
pub const TURN: f32 = 1.5;
/// a start is drawn at most this far ahead till the game takes its first step (a walk's step; the bike's is half a tile)
pub const START_REACH: f32 = WALK;
/// a step past this is a warp, not a step; a difference past this goes at once
pub const WARP: f32 = 24.0;
pub const SNAP: f32 = TILE / 2.0;
/// how far off a ground way (in quarter turns) the camera may look and the d-pad still be taken for its ways: 20 degrees
pub const AXIS: f32 = 20.0 / 90.0;
/// how long a difference takes to fade, in field updates (about 15 ms)
pub const FADE: f32 = 0.45;
/// the camera turning more than this a field update (degrees) isn't drawn ahead (its turn isn't extrapolated)
pub const CAMERA_TURN: f32 = 0.2;
/// updates the game is followed after the last press while it moves on its own (a tile's walk is 7)
pub const LET_GO: u64 = 8;

/// the game as one picture draws it
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seen {
    pub map: u16,
    /// the trainer's feet, the camera's eye
    pub pos: [f32; 3],
    pub eye: [f32; 3],
    pub facing: Facing,
    /// controller form: 0 on foot, 1 the bike, 2 roller skates; anything else (surf) is off
    pub form: u8,
    /// the move model's speed code is set
    pub moving: bool,
    /// the camera's forward on the ground (x, z): the d-pad goes the way it's nearest to (Lumiose's streets turn it)
    pub forward: [f32; 2],
    /// nobody (an NPC, a puppet) near the trainer: else nothing is drawn ahead, their draws at its feet would move too
    pub alone: bool,
}

/// the input at a refresh
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Held {
    /// the d-pad, one way only
    pub dir: Option<Facing>,
    pub run: bool,
    /// a button that opens or answers something (A, X, Y, L, R, START, SELECT, the touch screen)
    pub other: bool,
}

/// per picture in between: the trainer's move past where frame interpolation draws it, the turn about its feet
/// (radians, about +y), and the camera's move
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ahead {
    pub player: Vec<([f32; 3], f32)>,
    pub camera: Vec<[f32; 3]>,
}

impl Ahead {
    pub fn moves(&self) -> bool {
        self.player.iter().any(|(d, yaw)| *d != [0.0; 3] || *yaw != 0.0) || self.camera.iter().any(|d| *d != [0.0; 3])
    }
}

/// what reconciling did so far
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    /// pictures drawn ahead, and of those the ones that started from a difference: faded, or gone at once
    pub sets: u64,
    pub blends: u64,
    pub snaps: u64,
    /// starts and turns the game refused
    pub refused: u64,
    /// the largest difference faded so far, world units
    pub worst: f32,
}

/// a picture's plan: where the trainer and camera are at t (world), and the turn
#[derive(Clone, Copy, Debug, PartialEq)]
struct Plan {
    now: Seen,
    /// where frame interpolation has the trainer and the camera come from (the picture b4)
    from: [f32; 3],
    eye_from: [f32; 3],
    /// the trainer drawn ahead (else as frame interpolation has it): at now + step * along(t), along clamped to
    /// `until`, and turned
    ahead: bool,
    step: [f32; 3],
    ramp: Option<(f32, f32)>,
    until: f32,
    yaw: f32,
    /// the turn left of the game's own (the picture b4 faced another way): frame interpolation turns the model from
    /// the one way to the other over the picture, this keeps it turned as it was drawn ahead
    rest: f32,
    /// the camera drawn ahead at eye + cam * t, else as frame interpolation has it
    cam: Option<[f32; 3]>,
    /// it drew anything ahead, and how it started: 1 faded from a miss, 2 gone at once
    real: bool,
    missed: u8,
    worst: f32,
}

impl Plan {
    /// a plan that draws nothing ahead: frame interpolation's, from `from`
    fn follow(now: Seen, from: Seen) -> Plan {
        Plan { now, from: from.pos, eye_from: from.eye, ahead: false, step: [0.0; 3], ramp: None, until: 1.0, yaw: 0.0, rest: 0.0, cam: None, real: false, missed: 0, worst: 0.0 }
    }

    fn player(&self, t: f32) -> [f32; 3] {
        if !self.ahead {
            return add(self.from, scale(sub(self.now.pos, self.from), t));
        }
        let along = match self.ramp {
            // (updates since the start at t = 0, the updates the start takes)
            Some((e, over)) => ((e + t) / over).clamp(0.0, 1.0).min(START_REACH / len(self.step).max(1e-6)),
            None => t,
        }
        .min(self.until);
        add(self.now.pos, scale(self.step, along))
    }

    /// the turn on top of frame interpolation's at t
    fn yaw(&self, t: f32) -> f32 {
        self.yaw + self.rest * (1.0 - t)
    }

    fn camera(&self, t: f32) -> [f32; 3] {
        match self.cam {
            Some(cam) => add(self.now.eye, scale(cam, t)),
            None => add(self.eye_from, scale(sub(self.now.eye, self.eye_from), t)),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Predictor {
    /// the picture drawn last and the one b4 it
    last: Option<Seen>,
    before: Option<Seen>,
    /// the plan of the picture drawn last, and the one being drawn (committed when it shows up)
    shown: Option<Plan>,
    pending: Option<Plan>,
    /// the d-pad way held and the refresh it was first held at; the refresh the game first faced it at
    press: Option<(Facing, u64)>,
    faced: Option<u64>,
    /// the last refresh a way was held at
    held_at: u64,
    /// the game answers d-pad presses (see Held::other)
    trusted: bool,
    /// starts and turns refused, by tile and way, till the trainer moves
    refused: Vec<([i32; 2], Facing)>,
    pub counts: Counts,
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn len(a: [f32; 3]) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

/// a way as a unit step on the ground: right +x, up -z
pub fn way(f: Facing) -> [f32; 3] {
    match f {
        Facing::Up => [0.0, 0.0, -1.0],
        Facing::Down => [0.0, 0.0, 1.0],
        Facing::Left => [-1.0, 0.0, 0.0],
        Facing::Right => [1.0, 0.0, 0.0],
    }
}

/// the turn about +y from facing `from` to `to`, in (-pi, pi]
pub fn turn(from: Facing, to: Facing) -> f32 {
    let angle = |f: Facing| {
        let w = way(f);
        w[0].atan2(w[2])
    };
    wrap(angle(to) - angle(from))
}

/// an angle in (-pi, pi]
fn wrap(d: f32) -> f32 {
    let pi = std::f32::consts::PI;
    if d > pi {
        d - 2.0 * pi
    } else if d <= -pi {
        d + 2.0 * pi
    } else {
        d
    }
}

/// d-pad way `d` on the ground for a camera looking along `forward`: up is the ground way nearest to it; none
/// for a camera turned too far between two (Lumiose's round boulevards), where the game's choice can't be told
pub fn relative(d: Facing, forward: [f32; 2]) -> Option<Facing> {
    const TURNS: [Facing; 4] = [Facing::Up, Facing::Right, Facing::Down, Facing::Left];
    let at = |f: Facing| TURNS.iter().position(|&t| t == f).unwrap_or(0);
    // quarter turns clockwise from -z (up) to the camera's forward, seen from above (+x is right)
    let quarters = forward[0].atan2(-forward[1]) / std::f32::consts::FRAC_PI_2;
    let quarter = quarters.round();
    ((quarters - quarter).abs() <= AXIS).then(|| TURNS[(at(d) as i32 + quarter as i32).rem_euclid(4) as usize])
}

fn tile(p: [f32; 3]) -> [i32; 2] {
    [(p[0] / TILE).floor() as i32, (p[2] / TILE).floor() as i32]
}

/// how far along `step` (in steps) the next tile centre at or past `p` is
fn to_centre(p: [f32; 3], step: [f32; 3]) -> f32 {
    let (axis, s) = if step[0].abs() >= step[2].abs() { (0, step[0]) } else { (2, step[2]) };
    if s == 0.0 {
        return 0.0;
    }
    // centres at 18n + 9
    let c = p[axis] - TILE / 2.0;
    let next = if s > 0.0 { (c / TILE).ceil() * TILE } else { (c / TILE).floor() * TILE };
    ((next - c) / s).max(0.0)
}

impl Predictor {
    /// the picture this refresh draws shows `now`; the GPU drew the last refresh's when `drawn`. What its pictures in
    /// between, at `ts`, show ahead of frame interpolation; none (and nothing kept) off
    pub fn refresh(&mut self, now: Option<Seen>, held: Held, refresh: u64, drawn: bool, ts: &[f32]) -> Option<Ahead> {
        if drawn {
            if let Some(plan) = self.pending.take() {
                // counted when its picture is drawn, a refresh that drew nothing made it for nothing
                self.counts.sets += plan.real as u64;
                self.counts.blends += (plan.missed == 1) as u64;
                self.counts.snaps += (plan.missed == 2) as u64;
                if plan.missed == 1 {
                    self.counts.worst = self.counts.worst.max(plan.worst);
                }
                self.before = self.last.replace(plan.now);
                self.shown = Some(plan);
            }
        }
        self.pending = None;
        // the input, every refresh
        if held.other {
            self.trusted = false;
            self.press = None;
        }
        match held.dir {
            Some(d) if self.press.is_some_and(|(p, _)| p == d) => {}
            Some(d) => (self.press, self.faced) = (Some((d, refresh)), None),
            None => self.press = None,
        }
        if held.dir.is_some() {
            self.held_at = refresh;
        }
        let now = now?;
        // the d-pad's ways on the ground, as the game's camera turns them
        let ground = |d: Facing| relative(d, now.forward);
        let reset = |p: &mut Predictor| {
            (p.last, p.before, p.shown) = (None, None, None);
            p.refused.clear();
            p.trusted = false;
        };
        let Some(last) = self.last else {
            self.pending = Some(Plan::follow(now, now));
            return None;
        };
        let step = sub(now.pos, last.pos);
        let cam = sub(now.eye, last.eye);
        if now.map != last.map || len(step) > WARP || len(cam) > WARP || now.form > 2 {
            reset(self);
            self.pending = Some(Plan::follow(now, now));
            return None;
        }
        // the camera goes on as it went, its step scaled as it grew or shrank
        let grew = self.before.map_or(1.0, |b| {
            let was = len(sub(last.eye, b.eye));
            if was > 0.05 { (len(cam) / was).clamp(0.0, 1.3) } else { 1.0 }
        });
        let mut plan = Plan::follow(now, last);
        let moving = len([step[0], 0.0, step[2]]) > 0.01;
        if let Some((d, _)) = self.press {
            if ground(d) == Some(now.facing) && self.faced.is_none() {
                self.faced = Some(refresh);
            }
            // the game answered the press: it moves, or faces the way pressed
            if moving || now.moving || ground(d) == Some(now.facing) {
                self.trusted |= now.moving || moving;
            }
        }
        let following = now.alone && (held.dir.is_some() || refresh.saturating_sub(self.held_at) <= 2 * LET_GO);
        // (skates go as frame interpolation has them: their meshes are never learned, see the GPU's predict)
        if moving && following && now.form <= 1 {
            plan.ahead = true;
            plan.step = step;
            // let go or turned elsewhere: the game finishes the tile it's on
            let on = held.dir.is_some_and(|d| {
                ground(d).is_none_or(|g| {
                    let w = way(g);
                    step[0] * w[0] + step[2] * w[2] > 0.0
                })
            });
            if !now.moving {
                // the game says it stopped here (a wall, a ledge, a person in the way, or the tile it was let go on)
                plan.until = 0.0;
            } else if !on {
                plan.until = to_centre(now.pos, step);
            }
            self.refused.clear();
        } else if let (false, Some((d, pressed)), true) = (moving, self.press, now.form <= 1 && now.alone) {
            let refused = self.refused.contains(&(tile(now.pos), d));
            let acked = self.trusted || now.moving;
            if acked && !refused {
                let g = ground(d);
                if g == Some(now.facing) {
                    let speed = match (now.form, held.run) {
                        (1, _) => BIKE,
                        (_, true) => RUN,
                        _ => WALK,
                    };
                    // eased in from the press, or from the game's own turn
                    let (from, over) = match self.faced {
                        Some(faced) if faced > pressed => (faced, AFTER_TURN),
                        _ => (pressed, START),
                    };
                    let e = (refresh - from) as f32 / 2.0;
                    if e > over + GRACE {
                        self.refuse(now.pos, d);
                    } else {
                        plan.ahead = true;
                        plan.step = scale(way(now.facing), speed);
                        plan.ramp = Some((e, over));
                    }
                } else if let Some(g) = g {
                    if ((refresh - pressed) as f32 / 2.0) > TURN + GRACE {
                        self.refuse(now.pos, d);
                    } else {
                        plan.ahead = true;
                        plan.yaw = turn(now.facing, g);
                    }
                }
            }
        }
        // the camera only goes ahead w/ the trainer, and not while it turns
        // (nor when its step broke from its last one by half a tile: a jump isn't a speed)
        let (a, b) = (last.forward, now.forward);
        let turned = (a[0] * b[1] - a[1] * b[0]).atan2(a[0] * b[0] + a[1] * b[1]).abs();
        let steady = self.before.is_some_and(|before| len(sub(cam, sub(last.eye, before.eye))) <= SNAP);
        if plan.ahead && steady && turned.to_degrees() <= CAMERA_TURN {
            plan.cam = Some(scale(cam, grew));
        }
        // the game turned the way the last picture was drawn turned: keep it so while frame interpolation turns it
        if self.shown.is_some_and(|shown| shown.now == last && shown.ahead && shown.yaw != 0.0) && now.facing != last.facing && now.alone {
            plan.rest = turn(last.facing, now.facing);
            plan.ahead = true;
        }
        let interp = |t: f32| add(last.pos, scale(step, t));
        let interp_eye = |t: f32| add(last.eye, scale(cam, t));
        // reconciling: where the last set ended against where this one starts
        let (miss, miss_eye, miss_yaw) = match self.shown {
            // turns as the trainer was drawn: the game's facing and the turn on top
            Some(shown) if shown.now == last => (sub(shown.player(1.0), plan.player(0.0)), sub(shown.camera(1.0), plan.camera(0.0)), wrap(shown.yaw(1.0) - plan.yaw(0.0))),
            _ => ([0.0; 3], [0.0; 3], 0.0),
        };
        // only a set that drew the trainer ahead can have missed; going ahead from frame interpolation (a start, a turn)
        // fades in uncounted
        let missed = self.shown.is_some_and(|shown| shown.real && shown.ahead);
        let snap = len(miss) > SNAP || len(miss_eye) > SNAP;
        if !ts.is_empty() && missed {
            if len(miss) <= SNAP {
                plan.worst = len(miss);
            }
            plan.missed = if snap {
                2
            } else {
                (len(miss) > 0.1 || miss_yaw.abs() > 0.05) as u8
            };
        }
        // over about 15 ms of the picture's own time, whatever the monitor (at 60 Hz its one picture in between is past it)
        let fade = |t: f32| if snap { 0.0 } else { (1.0 - t / FADE).max(0.0) };
        let ahead = Ahead {
            player: ts.iter().map(|&t| (add(sub(plan.player(t), interp(t)), scale(miss, fade(t))), plan.yaw(t) + miss_yaw * fade(t))).collect(),
            camera: ts.iter().map(|&t| add(sub(plan.camera(t), interp_eye(t)), scale(miss_eye, fade(t)))).collect(),
        };
        plan.real = ahead.moves();
        self.pending = Some(plan);
        plan.real.then_some(ahead)
    }

    /// the picture this refresh draws couldn't be drawn ahead after all (no room for the own picture drawn ahead):
    /// it's frame interpolation's, which the next picture reconciles against
    pub fn not_drawn_ahead(&mut self) {
        if let Some(plan) = self.pending.as_mut() {
            let follow = Plan::follow(plan.now, Seen { pos: plan.from, eye: plan.eye_from, ..plan.now });
            *plan = follow;
        }
    }

    fn refuse(&mut self, pos: [f32; 3], d: Facing) {
        self.refused.push((tile(pos), d));
        self.trusted = false;
        self.counts.refused += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(x: f32, eye: f32, facing: Facing, moving: bool) -> Seen {
        Seen { map: 200, pos: [x, 144.0, 9135.0], eye: [eye, 322.0, 9317.0], facing, form: 0, moving, forward: [0.0, -1.0], alone: true }
    }

    const TS: [f32; 3] = [0.25, 0.5, 0.75];

    /// pictures every other refresh from `from`, the game's positions (feet x, eye x, facing, moving) as given; as in
    /// the runtime, the game updates during the refresh that draws, so the refresh after it (which is told the picture
    /// was drawn) already reads the next picture's state
    fn run(p: &mut Predictor, from: u64, frames: &[(f32, f32, Facing, bool, Option<Facing>)]) -> Vec<Option<Ahead>> {
        let mut out = Vec::new();
        for (i, &(x, eye, facing, moving, dir)) in frames.iter().enumerate() {
            let r = from + 2 * i as u64;
            let held = Held { dir, ..Held::default() };
            out.push(p.refresh(Some(seen(x, eye, facing, moving)), held, r, false, &TS));
            if let Some(&(x, eye, facing, moving, _)) = frames.get(i + 1) {
                p.refresh(Some(seen(x, eye, facing, moving)), held, r + 1, true, &TS);
            }
        }
        out
    }

    #[test]
    fn standing_still_draws_nothing_ahead() {
        let mut p = Predictor::default();
        let out = run(&mut p, 100, &[(8667.0, 8667.0, Facing::Down, false, None); 5]);
        assert!(out.iter().all(Option::is_none));
    }

    #[test]
    fn walking_shows_one_update_ahead_and_the_steps_join() {
        let mut p = Predictor::default();
        let r = Facing::Right;
        let frames: Vec<_> = (0..8).map(|i| (8667.0 + WALK * i as f32, 8660.0 + 2.0 * i as f32, r, true, Some(r))).collect();
        let out = run(&mut p, 100, &frames);
        let last = out.last().unwrap().as_ref().unwrap();
        for (d, yaw) in &last.player {
            assert!((d[0] - WALK).abs() < 1e-2 && *yaw == 0.0, "{d:?}");
        }
        assert_eq!(p.counts.blends + p.counts.snaps + p.counts.refused, 0, "{:?}", p.counts);
    }

    #[test]
    fn moving_without_the_dpad_is_left_to_frame_interpolation() {
        // skates on the circle pad, a scripted walk: the trainer and the camera move, no d-pad for long
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let r = Facing::Right;
        let frames: Vec<_> = (0..6).map(|i| (8667.0 + 5.0 * i as f32, 8660.0 + 3.0 * i as f32, r, true, None)).collect();
        let out = run(&mut p, 100, &frames);
        assert!(out.iter().all(Option::is_none), "{out:?}");
    }

    #[test]
    fn skates_are_left_to_frame_interpolation_even_on_the_dpad() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let r = Facing::Right;
        let mut out = Vec::new();
        for i in 0..6u64 {
            let mut now = seen(8667.0 + 5.0 * i as f32, 8660.0 + 3.0 * i as f32, r, true);
            now.form = 2;
            out.push(p.refresh(Some(now), Held { dir: Some(r), ..Held::default() }, 100 + 2 * i, false, &TS));
            p.refresh(Some(now), Held { dir: Some(r), ..Held::default() }, 101 + 2 * i, true, &TS);
        }
        assert!(out.iter().all(Option::is_none), "{out:?}");
    }

    #[test]
    fn the_camera_only_goes_ahead_with_the_trainer() {
        // a cutscene pan: the camera moves, the trainer stands
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let frames: Vec<_> = (0..6).map(|i| (8667.0, 8660.0 + 4.0 * i as f32, Facing::Down, false, None)).collect();
        let out = run(&mut p, 100, &frames);
        assert!(out.iter().all(Option::is_none), "{out:?}");
    }

    #[test]
    fn a_camera_jump_is_not_taken_for_a_speed() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let r = Facing::Right;
        let mut frames: Vec<_> = (0..5).map(|i| (8667.0 + WALK * i as f32, 8660.0 + 2.0 * i as f32, r, true, Some(r))).collect();
        // the camera jumps 15 units (under a warp's 24, a scripted move the cut hint misses)
        frames.push((8667.0 + WALK * 5.0, 8660.0 + 8.0 + 15.0, r, true, Some(r)));
        frames.push((8667.0 + WALK * 6.0, 8660.0 + 8.0 + 17.0, r, true, Some(r)));
        let out = run(&mut p, 100, &frames);
        for k in [5, 6] {
            let a = out[k].as_ref().unwrap();
            assert!(a.camera.iter().all(|d| d[0].abs() < 3.0), "picture {k}: the jump left to frame interpolation, not extrapolated: {:?}", a.camera);
        }
    }

    #[test]
    fn a_miss_past_half_a_tile_snaps() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let r = Facing::Right;
        let mut frames: Vec<_> = (0..5).map(|i| (8667.0 + BIKE * i as f32, 8660.0 + 2.0 * i as f32, r, true, Some(r))).collect();
        // the game puts the trainer back 12 units (under a warp's 24) while it was drawn a bike's step ahead
        frames.push((8667.0 + BIKE * 4.0 - 12.0, 8660.0 + 10.0, r, true, Some(r)));
        frames.push((8667.0 + BIKE * 4.0 - 12.0, 8660.0 + 10.0, r, false, Some(r)));
        run(&mut p, 100, &frames);
        assert_eq!(p.counts.snaps, 1, "{:?}", p.counts);
    }

    #[test]
    fn a_turn_the_game_refuses_goes_within_two_updates() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let x = 8667.0;
        // pressed right while facing down; the game never turns (a menu, a scripted moment)
        let d = Facing::Down;
        let frames = [(x, x, d, false, None), (x, x, d, false, Some(Facing::Right)), (x, x, d, false, Some(Facing::Right)), (x, x, d, false, Some(Facing::Right)), (x, x, d, false, Some(Facing::Right))];
        let out = run(&mut p, 100, &frames);
        assert!(out[1].as_ref().is_some_and(|a| a.player.iter().any(|&(_, yaw)| yaw != 0.0)), "the turn shows at the press");
        assert_eq!(p.counts.refused, 1, "{:?}", p.counts);
        // refused 2 updates after the press: faded back over the first pictures in between, gone by the last
        let last = out[4].as_ref();
        assert!(last.is_none_or(|a| a.player.last().is_some_and(|&(_, yaw)| yaw == 0.0)), "refused after 2 updates: {last:?}");
    }

    #[test]
    fn a_press_facing_its_way_eases_into_the_games_first_step() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let r = Facing::Right;
        let x = 8667.0;
        // pressed at the third picture; the game's first step shows in the next
        let frames = [(x, x, r, false, None), (x, x, r, false, None), (x, x, r, false, Some(r)), (x + WALK, x, r, true, Some(r)), (x + 2.0 * WALK, x + 1.0, r, true, Some(r))];
        let out = run(&mut p, 100, &frames);
        let first = out[2].as_ref().expect("moves at the press");
        assert!(first.player[0].0[0] > 0.0 && first.player[0].0[0] < WALK / 2.0, "{:?}", first.player);
        assert_eq!(p.counts.blends + p.counts.snaps, 0, "{:?}", p.counts);
    }

    #[test]
    fn a_menu_press_waits_for_the_game() {
        let mut p = Predictor::default();
        let r = Facing::Right;
        let x = 8667.0;
        p.refresh(Some(seen(x, x, r, false)), Held { other: true, ..Held::default() }, 98, true, &TS);
        let out = run(&mut p, 100, &[(x, x, r, false, Some(r)); 4]);
        assert!(out.iter().all(Option::is_none), "the game never answered");
    }

    #[test]
    fn a_wall_is_refused_once_and_faded_back() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let r = Facing::Right;
        let x = 8667.0;
        let out = run(&mut p, 100, &[(x, x, r, false, None), (x, x, r, false, Some(r)), (x, x, r, true, Some(r)), (x, x, r, true, Some(r)), (x, x, r, true, Some(r)), (x, x, r, true, Some(r)), (x, x, r, true, Some(r))]);
        assert_eq!(p.counts.refused, 1);
        assert!(p.counts.blends >= 1 && p.counts.snaps == 0, "{:?}", p.counts);
        assert!(out.last().unwrap().is_none(), "the refused way is drawn as the game has it");
    }

    #[test]
    fn let_go_stops_at_the_tile_centre() {
        let mut p = Predictor { trusted: true, held_at: 99, ..Predictor::default() };
        let r = Facing::Right;
        // 8718.43 is a step short of the centre at 8721
        let frames = [(8713.286, 8706.0, r, true, None), (8715.857, 8709.0, r, true, None), (8718.429, 8712.0, r, true, None)];
        let out = run(&mut p, 100, &frames);
        let a = out[2].as_ref().unwrap();
        for (k, &t) in TS.iter().enumerate() {
            let shown = 8715.857 + WALK * t + a.player[k].0[0];
            assert!(shown <= 8721.0 + 1e-3, "{shown}");
        }
    }

    #[test]
    fn a_wall_stops_it_where_the_game_stops() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let u = Facing::Up;
        // walking up into a wall: the game stops at the tile centre and says so while the way is still held
        let frames = [(8667.0, 8660.0, u, true, Some(u)), (8669.571, 8661.0, u, true, Some(u)), (8672.143, 8662.0, u, false, Some(u))];
        let out = run(&mut p, 100, &frames.map(|(x, e, f, m, d)| (x, e, f, m, d)));
        let a = out[2].as_ref().unwrap();
        for (k, &t) in TS.iter().enumerate() {
            assert!((8669.571 + WALK * t + a.player[k].0[0] - 8672.143).abs() < 1e-2, "held at the wall");
        }
    }

    #[test]
    fn a_turn_shows_at_once() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let x = 8667.0;
        let out = run(&mut p, 100, &[(x, x, Facing::Down, false, None), (x, x, Facing::Down, false, Some(Facing::Right)), (x, x, Facing::Right, false, None), (x, x, Facing::Right, false, None)]);
        // the game turned as drawn: while frame interpolation turns the model, the rest of the turn keeps it as drawn,
        // nothing faded; then nothing ahead
        let rest = out[2].as_ref().expect("the rest of the turn");
        for (&(_, yaw), &t) in rest.player.iter().zip(&TS) {
            assert!((yaw - std::f32::consts::FRAC_PI_2 * (1.0 - t)).abs() < 1e-4, "{:?}", rest.player);
        }
        assert!(out[3].is_none(), "{:?}", out[3]);
        assert_eq!(p.counts.blends, 0, "{:?}", p.counts);
        let a = out[1].as_ref().unwrap();
        // faded in from frame interpolation's over the first pictures in between
        assert!(a.player[0].1 > 0.0 && (a.player[2].1 - turn(Facing::Down, Facing::Right)).abs() < 1e-6, "{:?}", a.player);
        assert!((turn(Facing::Down, Facing::Right) - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!((turn(Facing::Right, Facing::Left).abs() - std::f32::consts::PI).abs() < 1e-6);
    }

    #[test]
    fn a_warp_turns_it_off() {
        let mut p = Predictor { trusted: true, ..Predictor::default() };
        let r = Facing::Right;
        let out = run(&mut p, 100, &[(8667.0, 8667.0, r, true, Some(r)), (8669.6, 8668.0, r, true, Some(r)), (9500.0, 9400.0, r, true, Some(r))]);
        assert!(out[2].is_none());
    }

    #[test]
    fn the_dpad_follows_the_camera() {
        assert_eq!(relative(Facing::Up, [0.0, -1.0]), Some(Facing::Up));
        assert_eq!(relative(Facing::Down, [0.0, 1.0]), Some(Facing::Up), "Lumiose: the camera looks south");
        assert_eq!(relative(Facing::Left, [1.0, 0.1]), Some(Facing::Up));
        assert_eq!(relative(Facing::Right, [-0.9, 0.2]), Some(Facing::Up));
        assert_eq!(relative(Facing::Up, [0.7, -0.7]), None, "halfway between two ways");
    }

    #[test]
    fn tile_centres() {
        assert!((to_centre([8718.429, 0.0, 0.0], [WALK, 0.0, 0.0]) - 1.0).abs() < 1e-3);
        assert_eq!(to_centre([8721.0, 0.0, 0.0], [WALK, 0.0, 0.0]), 0.0);
        assert!((to_centre([8723.571, 0.0, 9135.0], [-WALK, 0.0, 0.0]) - 1.0).abs() < 1e-3);
    }
}
