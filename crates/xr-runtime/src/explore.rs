//! XR_EXPLORE=ADDR (hex, the player's x,y,z floats; Pokemon X: 0x0818290C while DllField sits at 0x006A5000): a
//! scripting aid. After a script's last press it walks the map one 18-unit tile at a time, toward the least visited
//! tiles (a walk, not a search: a follower such as Shauna blocks the way back), and logs every try as
//! `explore X,Z DIR -> X2,Z2` (tile numbers; the same tile means blocked), each step's press as a script line
//! (`explore-press T DIR N`, N the frames it was really held), so a walk that got somewhere can be replayed. The log
//! is mapped by hand or a script to plan walks. Battles (with `autobattle`) are run from (see `flee`); one still on after 20 s, a trainer's, is
//! fought. A screen with no player for a while gets A. XR_EXPLORE_GOAL=TX,TZ (tiles, e.g. a warp from the zone data)
//! walks toward that tile instead: each step follows the shortest path over the steps seen so far, untried steps
//! counting as open (so it learns the walls on the way). The goal is reached on that tile or by a warp taken within 3
//! tiles of it (`explore: arrived`); `TX,TZ;TX2,TZ2;...` then walks to the next one, and the walk ends after the last.
//! A goal `A` turns up and talks (14 A, then 2 B): the nurse, from the counter tile; `A[U|R|D|L][N]` faces that way
//! and presses A N times (Shauna, behind the player in Parfum Palace's maze: `AR3`). A goal `M<id>` explores (least
//! visited tiles) until the map id is that one: for places whose warps are not in tiles (Lumiose's rails).
//! A goal `Wx,z` (world units) steers there through the field camera: the d-pad toward where the goal is on the top
//! screen (Lumiose turns its camera with the player), stepping aside when stuck; its presses are logged as runs.
use xr_hle::services::hid::{InputState, PadState};

/// Frames after a press that has not moved the player before it counts as blocked (a pillar hop crouches first).
const HOP_WAIT: u64 = 28;

const DIRS: [(PadState, i32, i32, &str); 4] =
    [(PadState::UP, 0, -1, "U"), (PadState::RIGHT, 1, 0, "R"), (PadState::DOWN, 0, 1, "D"), (PadState::LEFT, -1, 0, "L")];

pub struct Explorer {
    pub addr: u32,
    tries: std::collections::HashMap<((i32, i32), usize), ((i32, i32), u32)>, // where a step led, how often tried
    visits: std::collections::HashMap<(i32, i32), u32>,
    last: Option<(f32, f32)>,
    still: u32,
    gone: u32,
    pending: Option<((i32, i32), usize, u32, u64)>, // from, dir, frames held so far (less one), the frame it started
    goals: Vec<(i32, i32)>,
    a_left: u8, // frames left of an A press
    pub map: u16, // the current map (zone) id, set by the caller each frame
    back: Option<usize>, // the next step: back out of a door just taken
    world: Vec<(f32, f32)>, // `W` goals (world x,z): steered to through the camera
    /// set by the caller for a `W` goal: the player's and the goal's top-screen pixels (None: behind the camera)
    pub screen: Option<([f32; 2], Option<[f32; 2]>)>,
    run: Option<(PadState, u64)>, // the d-pad held since (a `W` walk's press, logged when it changes)
    side: (u32, PadState, bool), // a sidestep round an obstacle: frames left, its direction, the side last tried
    prev: Option<(f32, f32)>,
    lost: Option<((i32, i32), usize)>, // a step cut short: from, dir (judged when the player is seen again)
    rot: usize, // pressing DIRS[d] walks DIRS[(d + rot) % 4]: a camera turned round (Parfum Palace's garden), learned from steps
}

impl Explorer {
    pub fn from_env() -> Option<Explorer> {
        let a = std::env::var("XR_EXPLORE").ok()?;
        let addr = u32::from_str_radix(a.trim().trim_start_matches("0x"), 16).ok()?;
        let goals = std::env::var("XR_EXPLORE_GOAL").unwrap_or_default();
        // `Wx,z;...`: walk to these world positions (Lumiose's streets, where a tile step means nothing)
        let world = goals.split(';').filter_map(|g| {
            let (x, z) = g.trim().strip_prefix('W')?.split_once(',')?;
            Some((x.trim().parse().ok()?, z.trim().parse().ok()?))
        });
        let world = world.collect();
        let goals = goals.split(';').filter(|g| !g.trim().starts_with('W')).filter_map(|g| {
            if let Some(a) = g.trim().strip_prefix('A') {
                // `A[U|R|D|L][N]`: face that way (UP), then N A presses (14); the second number counts its frames
                let dir = ["U", "R", "D", "L"].iter().position(|d| a.starts_with(d));
                let n: i32 = a[dir.map_or(0, |_| 1)..].parse().unwrap_or(14).clamp(1, 99);
                return Some((TALK + dir.unwrap_or(0) as i32 * 100 + n, 0));
            }
            if let Some(m) = g.trim().strip_prefix('M') {
                return Some((MAP, m.parse().ok()?)); // reached on that map; walked to the least visited tiles
            }
            let (x, z) = g.split_once(',')?;
            Some((x.trim().parse().ok()?, z.trim().parse().ok()?))
        });
        Some(Explorer { addr, tries: Default::default(), visits: Default::default(), last: None, still: 0, gone: 0, pending: None, goals: goals.collect(), a_left: 0, map: 0, back: None, world, screen: None, run: None, side: (0, PadState::empty(), false), prev: None, lost: None, rot: 0 })
    }

    /// The `W` goal being walked to, if any.
    pub fn world_goal(&self) -> Option<(f32, f32)> {
        self.world.first().copied().filter(|_| self.addr != 0)
    }

    /// Logs the d-pad run that ends now (one script line per button held), and starts `b`'s.
    fn hold(&mut self, b: PadState, t: u64) {
        if self.run.is_some_and(|(r, _)| r == b) {
            return;
        }
        if let Some((r, start)) = self.run.take() {
            for (bit, name) in [(PadState::UP, "UP"), (PadState::DOWN, "DOWN"), (PadState::LEFT, "LEFT"), (PadState::RIGHT, "RIGHT")] {
                if r.contains(bit) {
                    crate::log::log(&format!("explore-press {start} {name} {}", t - start));
                }
            }
        }
        self.run = (!b.is_empty()).then_some((b, t));
    }

    /// A `W` goal: hold the d-pad toward where the goal is on the top screen (Lumiose turns the camera with the
    /// player, so this follows the street); stuck for 30 frames, step aside for 40 (alternating sides). Reached within
    /// 24 units, or by a warp (a jump of over 60 units in a frame, e.g. a gate).
    fn steer(&mut self, at: (f32, f32), goal: (f32, f32), t: u64) -> InputState {
        let jump = self.prev.is_some_and(|p| (p.0 - at.0).hypot(p.1 - at.1) > 60.0);
        self.prev = Some(at);
        if (goal.0 - at.0).hypot(goal.1 - at.1) < 24.0 || jump {
            crate::log::log(&format!("explore: arrived at {:.0},{:.0} (map {}) for goal W{},{}", at.0, at.1, self.map, goal.0, goal.1));
            self.hold(PadState::empty(), t);
            self.world.remove(0);
            if self.world.is_empty() && self.goals.is_empty() {
                self.addr = 0;
            }
            return InputState::default();
        }
        if self.still >= 200 && self.still % 40 == 0 {
            self.hold(PadState::empty(), t);
            return self.press_a(t); // a text box
        }
        let toward = match self.screen {
            Some((p, Some(g))) => dir8(g[0] - p[0], g[1] - p[1]),
            _ => PadState::DOWN, // behind the camera: turn round
        };
        if self.side.0 == 0 && self.still >= 30 && self.still < 200 {
            self.side.2 = !self.side.2;
            // across the way the player goes: LEFT or RIGHT (DOWN would turn Lumiose's camera round)
            let across = if toward == PadState::LEFT || toward == PadState::RIGHT {
                PadState::UP
            } else if self.side.2 {
                PadState::LEFT
            } else {
                PadState::RIGHT
            };
            self.side = (40, across, self.side.2);
        }
        let b = if self.side.0 > 0 {
            self.side.0 -= 1;
            self.side.1
        } else {
            toward
        };
        self.hold(b, t);
        InputState { buttons: b, ..Default::default() }
    }

    /// A battle started: the step it cut short proves nothing.
    pub fn interrupt(&mut self) {
        self.cut();
    }

    /// An A for 8 frames, logged as its script line.
    fn press_a(&mut self, t: u64) -> InputState {
        crate::log::log(&format!("explore-press {t} A 8"));
        self.a_left = 7;
        InputState { buttons: PadState::A, ..Default::default() }
    }

    /// Ends the step: logs its press as a script line with the frames it was really held (13 unless cut short), so a
    /// replay holds it exactly as long (a 13 for a step cut at 10 walked further in the replay).
    fn cut(&mut self) {
        if let Some((from, d, held, start)) = self.pending.take() {
            if held < 12 {
                log_press(start, d, held); // (a finished press was logged as it finished)
            }
            self.lost = Some((from, d)); // judged where the player is next seen: a door's fade cuts its step short
        }
    }

    /// A step from `from` (direction `d`) ended on `tile`: logs it, takes a goal it reached, and learns where it led.
    /// Returns true when that was the last goal.
    fn step_done(&mut self, from: (i32, i32), d: usize, tile: (i32, i32)) -> bool {
        crate::log::log(&format!("explore {},{} {} -> {},{} (map {})", from.0, from.1, DIRS[d].3, tile.0, tile.1, self.map));
        let jump = (tile.0 - from.0).abs() > 2 || (tile.1 - from.1).abs() > 2;
        // a warp next to the goal (a door, a gate) reaches it; any other one (a wrong door, a whiteout) does not
        let near = |g: (i32, i32)| (from.0 - g.0).abs() <= 3 && (from.1 - g.1).abs() <= 3;
        let reached = self.goals.first().copied().filter(|&g| g.0 != MAP && !is_talk(g.0) && if jump { near(g) } else { tile == g });
        if let Some(g) = reached {
            crate::log::log(&format!("explore: arrived at {},{} for goal {},{}", tile.0, tile.1, g.0, g.1));
            self.goals.remove(0);
            if self.goals.is_empty() {
                self.addr = 0;
                return true;
            }
        }
        // a one-tile step, or a two-tile hop (Route 8's rock pillars, under a camera turned a quarter), shows which way
        // this press walks here (a camera turned round turns the d-pad with it)
        let along = |k: usize, n: i32| (from.0 + n * DIRS[k].1, from.1 + n * DIRS[k].2) == tile;
        if let Some(k) = (0..4).find(|&k| along(k, 1) || along(k, 2)) {
            self.rot = (k + 4 - d) % 4;
        }
        let e = self.tries.entry((from, d)).or_insert((tile, 0));
        *e = (tile, e.1 + 1);
        *self.visits.entry(tile).or_insert(0) += 1;
        // a door that led elsewhere (looking for a map, or not the goal's door) is not taken again, and the walk steps
        // straight back out (the opposite way) instead of exploring that building
        if jump && reached.is_none() && self.goals.first().is_some_and(|g| !is_talk(g.0)) {
            self.tries.insert((from, d), (from, 1000));
            self.back = Some((d + 2) % 4);
        }
        false
    }

    /// This frame's input, from the player's position (x, z; None when the field is not loaded).
    pub fn input(&mut self, pos: Option<(f32, f32)>, t: u64) -> InputState {
        if self.addr == 0 {
            return InputState::default(); // arrived
        }
        let pos = pos.filter(|&(x, z)| x != 0.0 || z != 0.0);
        if pos.is_none() {
            self.cut(); // a battle or a map change cut the step short: it proves nothing
        }
        self.still = if pos.is_some() && pos == self.last { self.still + 1 } else { 0 };
        self.last = pos;
        self.gone = if pos.is_none() { self.gone + 1 } else { 0 };
        if self.a_left > 0 {
            self.a_left -= 1; // an A is held its 8 frames whatever happens, as its script line says
            return InputState { buttons: PadState::A, ..Default::default() };
        }
        let Some((x, z)) = pos else {
            self.hold(PadState::empty(), t); // (a `W` walk) nothing is held while the field is away
            if self.gone > 300 && self.gone % 40 == 0 {
                return self.press_a(t); // e.g. the text after a whiteout
            }
            return InputState::default();
        };
        if let Some(g) = self.world_goal() {
            return self.steer((x, z), g, t);
        }
        let tile = ((x / 18.0).floor() as i32, (z / 18.0).floor() as i32);
        if let Some(&(MAP, m)) = self.goals.first() {
            if i32::from(self.map) == m {
                crate::log::log(&format!("explore: arrived on map {m} at {},{}", tile.0, tile.1));
                self.cut();
                self.lost = None; // the door into the goal map is not a wrong door for the next goal
                self.goals.remove(0);
                if self.goals.is_empty() {
                    self.addr = 0;
                    return InputState::default();
                }
            }
        }
        // an `A` goal: face up (or its direction), then talk (14 A, or its count, then 2 B, 40 frames apart), e.g. the
        // nurse from the counter
        if let Some(&(g, k)) = self.goals.first().filter(|g| is_talk(g.0)) {
            let (dir, n) = (((g - TALK) / 100) as usize, (g - TALK) % 100);
            self.cut();
            self.goals[0].1 += 1;
            if k < 40 {
                // face up first (the nurse is across the counter; a walk may arrive facing sideways)
                if k == 0 {
                    crate::log::log(&format!("explore-press {t} {} 8", ["UP", "RIGHT", "DOWN", "LEFT"][dir]));
                }
                return InputState { buttons: if k < 8 { DIRS[dir].0 } else { PadState::empty() }, ..Default::default() };
            }
            let k = k - 40;
            if k >= (n + 2) * 40 {
                self.goals.remove(0);
                crate::log::log("explore: talked");
                if self.goals.is_empty() {
                    self.addr = 0;
                }
                return InputState::default();
            }
            let b = if k < n * 40 { PadState::A } else { PadState::B };
            if k % 40 == 0 {
                crate::log::log(&format!("explore-press {t} {} 8", if k < n * 40 { "A" } else { "B" }));
            }
            return InputState { buttons: if k % 40 < 8 { b } else { PadState::empty() }, ..Default::default() };
        }
        if self.still >= 200 && self.still % 40 == 0 {
            self.cut(); // a text box (a trainer saw the player): the step proves nothing
            // nor did the steps tried here while it held the player: a scene on Route 8's trigger had marked all four
            // sides blocked dozens of times, and the walk then went back the way it came
            self.tries.retain(|&(from, _), &mut (to, _)| from != tile || to != tile);
            return self.press_a(t);
        }
        if let Some((from, d, held, start)) = self.pending {
            if held < 12 {
                self.pending = Some((from, d, held + 1, start));
                if held == 11 {
                    log_press(start, d, 12); // held to the end (13 frames)
                }
                return InputState { buttons: DIRS[d].0, ..Default::default() };
            }
            // let the step (or a slide) finish; a step that has not moved yet gets longer: Route 8's rock pillars crouch
            // still before each hop, and a hop judged blocked at 8 frames was credited to the next press instead
            if self.still < 8 || tile == from && t < start + 13 + HOP_WAIT {
                return InputState::default();
            }
            self.pending = None;
            if self.step_done(from, d, tile) {
                return InputState::default();
            }
        } else if let Some((from, d)) = self.lost.take() {
            // a step cut short (a battle, a text box, a door's fade) counts where it got to (a tile, a warp), if anywhere
            if tile != from && self.step_done(from, d, tile) {
                return InputState::default();
            }
        }
        if self.still < 8 {
            return InputState::default();
        }
        // untried first; then the step to the least visited tile (a blocked step counts as visited more, by its tries)
        let score = |d: usize| match self.tries.get(&(tile, d)) {
            None => 0,
            Some(&(to, n)) if to == tile => 40 * n, // a follower or an NPC may have been in the way: try again later
            Some(&(to, n)) => 1 + 4 * self.visits.get(&to).copied().unwrap_or(0) + n,
        };
        let d = match (self.back.take(), self.goals.first()) {
            (Some(b), _) => b,
            // a talk goal next (a step just arrived): stand still, the next frame talks (`toward` would search forever)
            (None, Some(&g)) if is_talk(g.0) => return InputState::default(),
            (None, Some(&g)) if g.0 != MAP => self.toward(tile, g),
            _ => (0..4).min_by_key(|&d| score(d)).unwrap(), // no goal, or a map goal: the least visited tiles
        };
        self.pending = Some((tile, d, 0, t));
        InputState { buttons: DIRS[d].0, ..Default::default() }
    }

    /// The first step of the shortest path from `at` to `goal`: a step seen to lead somewhere costs 1, an untried step
    /// 1 (optimism: it may be open), a blocked one 1 + 100 per try (a follower may have been in the way). The search
    /// stays within 40 tiles of the box around both ends.
    fn toward(&self, at: (i32, i32), goal: (i32, i32)) -> usize {
        let (x0, x1) = (at.0.min(goal.0) - 40, at.0.max(goal.0) + 40);
        let (z0, z1) = (at.1.min(goal.1) - 40, at.1.max(goal.1) + 40);
        let mut best = std::collections::HashMap::from([(at, (0u32, usize::MAX))]);
        let mut heap = std::collections::BinaryHeap::from([std::cmp::Reverse((0u32, at, usize::MAX))]);
        while let Some(std::cmp::Reverse((c, t, first))) = heap.pop() {
            if t == goal {
                return first % 4; // (at the goal already: any step; the caller arrives there first)
            }
            if best.get(&t).is_some_and(|&(b, _)| b < c) {
                continue;
            }
            for d in 0..4 {
                let w = DIRS[(d + self.rot) % 4]; // where this press walks (see `rot`)
                let (to, cost) = match self.tries.get(&(t, d)) {
                    Some(&(to, n)) if to == t => ((t.0 + w.1, t.1 + w.2), 1 + 100 * n),
                    Some(&(to, _)) => (to, 1),
                    None => ((t.0 + w.1, t.1 + w.2), 1),
                };
                if to.0 < x0 || to.0 > x1 || to.1 < z0 || to.1 > z1 {
                    continue;
                }
                let (c2, f2) = (c + cost, if t == at { d } else { first });
                if best.get(&to).is_none_or(|&(b, _)| c2 < b) {
                    best.insert(to, (c2, f2));
                    heap.push(std::cmp::Reverse((c2, to, f2)));
                }
            }
        }
        0
    }
}

/// The d-pad toward a screen offset (x right, y down): UP/DOWN within 45 degrees (Lumiose's camera follows the player,
/// so straight on keeps to the street), LEFT/RIGHT when sideways more than twice, else a diagonal.
fn dir8(dx: f32, dy: f32) -> PadState {
    let h = if dx > 0.0 { PadState::RIGHT } else { PadState::LEFT };
    let v = if dy < 0.0 { PadState::UP } else { PadState::DOWN };
    match (dx.abs(), dy.abs()) {
        (x, y) if y >= x => v,
        (x, y) if x > 2.0 * y => h,
        _ => h | v,
    }
}

/// The x of an `A` goal (talk) in XR_EXPLORE_GOAL.
const TALK: i32 = i32::MIN + 1000; // + direction x 100 + the A presses

fn is_talk(x: i32) -> bool {
    (TALK..TALK + 400).contains(&x)
}
/// The x of an `M<id>` goal (reach that map, exploring) in XR_EXPLORE_GOAL.
const MAP: i32 = i32::MIN + 1;

/// A step's press as a replayable script line: `held` is the frames it was held less one.
fn log_press(start: u64, d: usize, held: u32) {
    crate::log::log(&format!("explore-press {start} {} {}", ["UP", "RIGHT", "DOWN", "LEFT"][d], held + 1));
}

/// A battle while exploring, at frame n: RUN on the command screen (a trainer battle refuses it and goes on), now and
/// then B for text (A would pick FIGHT). `last`: the fleeing ends soon, so touch FIGHT instead: the command cursor
/// stays on the last button touched, and the autobattle's A would pick RUN again.
pub fn flee(n: u64, last: bool) -> InputState {
    match n % 80 {
        0..8 => InputState { touch: Some(if last { (160, 90) } else { (160, 225) }), ..Default::default() },
        40..48 => InputState { buttons: PadState::B, ..Default::default() },
        _ => InputState::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Explorer {
        fn from_goals(goals: Vec<(i32, i32)>) -> Explorer {
            Explorer { addr: 1, tries: Default::default(), visits: Default::default(), last: None, still: 0, gone: 0, pending: None, goals, a_left: 0, map: 0, back: None, world: vec![], screen: None, run: None, side: (0, PadState::empty(), false), prev: None, lost: None, rot: 0 }
        }
    }

    #[test]
    fn goal_walk_goes_around_a_learned_wall() {
        let mut e = Explorer::from_goals(vec![(0, -5)]);
        assert_eq!(e.toward((0, 0), (0, -5)), 0); // straight up while nothing is known
        e.tries.insert(((0, 0), 0), ((0, 0), 1)); // up is blocked
        assert_ne!(e.toward((0, 0), (0, -5)), 0);
        e.tries.insert(((0, 0), 1), ((2, 0), 1)); // right jumps two tiles (a ledge) and is the way round
        e.tries.insert(((0, 0), 3), ((0, 0), 1)); // left is blocked
        assert_eq!(e.toward((0, 0), (0, -5)), 1);
    }

    #[test]
    fn a_wrong_door_is_backed_out_of_and_the_goal_door_arrives() {
        let mut e = Explorer::from_goals(vec![(50, 6), (40, 58)]);
        assert!(!e.step_done((60, 30), 2, (59, 137))); // a door far from the goal: blocked, step back up
        assert_eq!(e.back, Some(0));
        assert_eq!(e.tries[&((60, 30), 2)], ((60, 30), 1000));
        assert!(!e.step_done((50, 7), 0, (59, 137))); // the goal's door (a warp within 3 tiles): arrived
        assert_eq!(e.goals, vec![(40, 58)]);
        assert!(e.step_done((40, 57), 2, (40, 58))); // the last goal ends the walk
    }

    #[test]
    fn talk_goals_parse() {
        // SAFETY: no other test reads these variables
        unsafe {
            std::env::set_var("XR_EXPLORE", "818290C");
            std::env::set_var("XR_EXPLORE_GOAL", "A;AR3;40,58;AL");
        }
        let e = Explorer::from_env().unwrap();
        assert_eq!(e.goals, vec![(TALK + 14, 0), (TALK + 103, 0), (40, 58), (TALK + 314, 0)]);
        assert_eq!(e.goals.iter().filter(|g| is_talk(g.0)).count(), 3);
    }

    #[test]
    fn a_turned_camera_turns_the_dpad() {
        let mut e = Explorer::from_goals(vec![(40, 50)]);
        e.step_done((58, 137), 2, (59, 137)); // DOWN walked +x (the garden's camera)
        assert_eq!(e.rot, 3);
        assert_eq!(e.toward((59, 137), (59, 120)), 1); // so -z is RIGHT
    }

    #[test]
    fn a_talk_after_a_tile_goal_talks() {
        // the walk arrives on 0,-1, then faces right and talks (it searched toward the talk goal forever: a hang in
        // Parfum Palace's maze, `43,30;AR3`)
        let mut e = Explorer::from_goals(vec![(0, -1), (TALK + 103, 0)]);
        let mut held = vec![];
        for t in 0..200u64 {
            let at = if held.contains(&PadState::UP) { -9.0 } else { 9.0 }; // one UP walks a tile up
            held.push(e.input(Some((9.0, at)), t).buttons);
        }
        assert!(held.contains(&PadState::RIGHT) && held.contains(&PadState::A));
    }

    #[test]
    fn a_text_box_forgets_the_steps_it_blocked() {
        // a scene holds the player on 0,0 (Route 8's trigger): every press there is "blocked" until its text is read
        let mut e = Explorer::from_goals(vec![(0, -5)]);
        e.tries.insert(((1, 0), 0), ((1, 0), 1)); // a real wall elsewhere is kept
        let mut t = 0u64;
        while e.input(Some((9.0, 9.0)), t).buttons != PadState::A {
            t += 1;
            assert!(t < 400, "no A for the text box");
            if t == 150 {
                assert!(e.tries.keys().any(|&(from, _)| from == (0, 0))); // blocked tries were learned meanwhile
            }
        }
        assert!(e.tries.keys().all(|&(from, _)| from != (0, 0)));
        assert!(e.tries.contains_key(&((1, 0), 0)));
        assert_eq!(e.toward((0, 0), (0, -5)), 0); // straight up again
    }

    #[test]
    fn a_hop_teaches_the_turned_dpad() {
        let mut e = Explorer::from_goals(vec![(40, 50)]);
        e.step_done((547, 730), 2, (549, 730)); // DOWN hopped two tiles +x (Route 8's pillars)
        assert_eq!(e.rot, 3);
    }

    #[test]
    fn a_slow_hop_is_credited_to_its_own_press() {
        // Route 8's pillars: the player crouches about 20 frames after the press, then lands two tiles on
        let mut e = Explorer::from_goals(vec![(2, -9)]);
        let (mut x, mut first, mut landed) = (9.0f32, None, false);
        for t in 0..120u64 {
            let b = e.input(Some((x, 9.0)), t).buttons;
            if first.is_none() && !b.is_empty() {
                first = Some((b, t));
            }
            if let Some((_, t0)) = first.filter(|&(b, _)| b == PadState::UP || b == PadState::RIGHT) {
                if !landed && t >= t0 + 34 {
                    (x, landed) = (9.0 + 36.0, true); // two tiles on, whatever was pressed
                }
            }
        }
        let (b, _) = first.unwrap();
        let d = DIRS.iter().position(|p| p.0 == b).unwrap();
        assert_eq!(e.tries.get(&((0, 0), d)).map(|t| t.0), Some((2, 0)), "the hop went to {b:?}'s press");
    }

    #[test]
    fn steering_picks_the_screen_direction() {
        assert_eq!(dir8(0.0, -50.0), PadState::UP);
        assert_eq!(dir8(80.0, 10.0), PadState::RIGHT);
        assert_eq!(dir8(-30.0, 40.0), PadState::DOWN);
        assert_eq!(dir8(-30.0, 20.0), PadState::LEFT | PadState::DOWN);
    }
}
