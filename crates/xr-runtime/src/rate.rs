//! Frame rate (experimental, the setting; XR_FRAME_RATE for headless runs): the field's logic at 60, 120 or 240
//! updates a second instead of the game's 30, at the game's own speed. Runs from the per-frame hook (hooks.rs `tick`),
//! once a main-loop pass, so once a field update. At 30 (the default) nothing here touches the game.
//!
//! How (X v1.0; docs/re/x-frame-rate.md):
//! - The game's own 60 Hz path: the frame controller (code.bin 0x108A88) switches mode when a request is pending, so
//!   the field gets request 0 (every pass a full one) and the field's leaving (or the X menu) gets request 1 back.
//!   Over 60 the refresh itself is made shorter (System.frame_cycles), so a pass a refresh is rate passes a second.
//! - Nothing in the game scales by that mode, so what the field advances a fixed step an update is stretched over
//!   S = rate / 30 updates, after each update: walk and run steps (a count against a length) get an S times longer
//!   length, a turn S times as long a countdown, the motion frame and the bike's position 1/S of their step, an NPC's
//!   wander wait one count in S.
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use xr_net::x::field;
use xr_net::x::puppet::{Calls, Dll};

/// code.bin: the frame controller's pointer (the object is built once, in application main).
const CONTROLLER_PTR: u32 = 0x005E_9E90;
/// Frame controller: u8 pending rate request (2 none), u8 current mode (1 every other pass, 0 every pass).
const REQUEST: u32 = 0x0C;
const MODE: u32 = 0x0D;
const NO_REQUEST: u8 = 2;
/// Player snapshot: u32 flags, 0x3000 standing, 0x1000 walking; 0x100 while the X menu is open.
const SNAP_FLAGS: u32 = 0x48;
const FLAG_BUSY: u32 = 0x100;
/// Player field object: u32 pointer to the field's touch screen; its u8 +0x91 is 0 while an event runs (DllField
/// 0x6D32D4 clears it as one starts, through 0x729E9C), 1 otherwise.
const PFO_SUBSCREEN: u32 = 0x16C;
const SUBSCREEN_ON: u32 = 0x91;
/// The touch screen's u8 lock (0xF from the end of a battle until its menu shows; 0 in the field), its u32 count of
/// updates since the menu showed (its slide in reads it).
const SUBSCREEN_LOCK: u32 = 0x94;
const SUBSCREEN_SHOWN: u32 = 0x9C;
/// Updates the touch screen's menu takes to slide in once shown (8 at 30, with a margin).
const SLIDE_IN: u32 = 12;
/// Move model: u16 the current action (then u16 its direction), u32 its state (1 playing, 2 ended).
const ACTION: u32 = 0x8FC;
const ACTION_STATE: u32 = 0x900;
/// How an action keeps its time in ACTION_STEPS.
#[derive(Clone, Copy)]
enum Kind {
    /// u16 length, i16 updates done (counted up, the action ends at the length)
    Up,
    /// i16 updates left in the high half (counted down to 0)
    Down,
}

/// The actions stretched: 0x12 a walk step (7 updates a tile), 0x0D a run step (4), 0x17 a turn on the spot (2 updates).
fn kind(code: u16) -> Option<Kind> {
    match code {
        0x12 | 0x0D => Some(Kind::Up),
        0x17 => Some(Kind::Down),
        _ => None,
    }
}

/// Move model: the current action's step count, u16 length then i16 updates done (DllField text+0x79AC0 sets them,
/// text+0x4EF90 counts one an update and ends the action at the length).
const ACTION_STEPS: u32 = 0x904;
/// Move model: u32 pointer to its motion (DllField class): f32 frame at +0x18, the last at +0x1C, the length at
/// +0x20, f32 speed (frames an update) at +0x24.
const MOTION: u32 = 0x97C;
const MOTION_FRAME: u32 = 0x18;
const MOTION_LENGTH: u32 = 0x20;
/// Move model: u16 updates an NPC that wanders waits before its next step (counts down).
const WANDER_WAIT: u32 = 0x8C4;
/// Move model: u32 pointer to the map's animations manager (DllField 0x718A1C: each of its +0x68 frames at
/// [+0x6C] goes on by +0x70, f32 1.0, each update).
const MM_MAP_ANIMS: u32 = 0x898;
const ANIMS_STEP: u32 = 0x70;
/// Move model: f32 x, y, z world position, and the copy the game keeps 0x28 on.
const MM_POS: u32 = 0xEC;
const MM_POS_COPY: u32 = 0x114;
/// Camera block: its follow state, eased each update: +0xD94 f32 x, y, z the point it follows (DllField 0x72C87C;
/// the target at +0xBE8 and the eye at +0xBDC are made from it), +0xDF8 the point that one eases to, and the easing's
/// own gains (+0xDBC, +0xDD8, +0xE20, +0xE3C, three each).
const CAM_STATE: u32 = 0xD94;
const CAM_STATE_WORDS: u32 = (0xE48 - 0xD94) / 4;
/// The follow's ease an update at the 60 Hz mode is the 30 Hz mode's to the power 0.653 (Laverre: 0.843 against
/// 0.770), so an update's step at S is cut by S * 0.653 to keep 30's ease in time.
const CAM_EASE: f32 = 0.78;

#[derive(Clone, Copy)]
struct Stretch {
    addr: u32,
    /// the length this action has now (ours, once stretched), and the game's
    len: u16,
    len0: u16,
    /// added to the count after each update (the game adds 1)
    extra: u16,
    /// the motion frame after the last pass (NaN: none yet)
    frame: f32,
    /// the wander wait after the last pass, and the updates counted since it last went down
    wait: u16,
    phase: u16,
    /// the action (mm +0x8FC), its count of updates done after the last pass and whether it was playing
    code: u16,
    done: i16,
    playing: bool,
}

struct State {
    /// the field's request 0 was ours, its 1 is owed when the field ends
    forced: bool,
    /// the last pass ran the field at S
    was_active: bool,
    slots: Vec<Stretch>,
    /// the player's position on the bike after the last pass (empty off the bike)
    bike: Vec<f32>,
    /// MOTION_CALLS at the last pass
    motion_calls: u64,
    /// the map animations' manager, the game's step and ours, while ours is in it
    map_anims: Option<(u32, f32, f32)>,
    /// the skates' velocity after the last pass (empty off the skates)
    velocity: Vec<f32>,
    /// the camera's follow state after the last pass (empty: none yet)
    follow: Vec<f32>,
    /// CTRL_COUNTERS after the last pass
    counters: [Counter; 5],
}

static STATE: Mutex<State> = Mutex::new(State { forced: false, was_active: false, slots: Vec::new(), bike: Vec::new(), motion_calls: 0, map_anims: None, velocity: Vec::new(), follow: Vec::new(), counters: [Counter { last: None, n: 0 }; 5] });

/// Player controller: counters of updates, (offset, 32 bits wide): +0x1A u16 standing still, +0x44 u32 since the
/// last step ended, and the skates' +0x48 (down), +0x54 and +0x58 (up).
const CTRL_COUNTERS: [(u32, bool); 5] = [(0x1A, false), (0x44, true), (0x48, true), (0x54, true), (0x58, true)];
/// Player controller: f32 x, y, z velocity an update (the bike's, set each update; the skates', changed each update).
const CTRL_VELOCITY: u32 = 0x38;

/// A counter of updates seen last pass, and its counts since it last started over.
#[derive(Clone, Copy, Default)]
struct Counter {
    last: Option<u32>,
    n: u32,
}

/// A counter the game adds (or takes) 1 each update counts one in S (the first of each S), so it reaches a value when it would
/// at 30; anything else it does (a reset, a jump) is the game's.
fn slow(g: &mut impl Calls, at: u32, wide: bool, s: u16, c: &mut Counter) {
    let Some(v) = (if wide { g.u32(at) } else { g.u16(at).map(u32::from) }) else { return };
    match c.last {
        Some(last) if v == last.wrapping_add(1) || v == last.wrapping_sub(1) => {
            c.n += 1;
            if (c.n - 1) % u32::from(s) != 0 {
                if wide { g.write(at, &last.to_le_bytes()) } else { g.write(at, &(last as u16).to_le_bytes()) };
                return;
            }
        }
        Some(last) if v == last => {}
        _ => c.n = 0,
    }
    c.last = Some(v);
}

/// S while the field runs at it (the hooks.rs animation hooks cut their steps by it), 1 otherwise.
pub static SCALE: AtomicU32 = AtomicU32::new(1);
/// Calls of hooks.rs `motion`: when it runs (a DLL with that hook), the motion frames are cut there, not here.
pub static MOTION_CALLS: AtomicU64 = AtomicU64::new(0);
/// The field runs at S (not a battle): the motion hook cuts only then (in a battle a cut motion speed faults; to find).
/// Main-loop passes while the rate runs (the motion hook's battle cadence).
pub static PASSES: AtomicU64 = AtomicU64::new(0);
pub static FIELD_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The battle is past its setup (hooks.rs: the proc in its battle phase, the scene's state machine past its setup)
/// and before its end: only then does it run at the rate. Setup and end stay at the game's 30 (they wait on loads,
/// which the main thread's extra passes would slow).
pub static BATTLE_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// XR_RATE_HOOKS=MASK (debug): which animation hooks cut (1 scene animations, 2 motions, 4 environment drifts, 8
/// animation sets, 16 layout animations, 32 the battle's command screens' steps, 64 the battle client; all by default).
pub fn hook_on(bit: u32) -> bool {
    static MASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MASK.get_or_init(|| std::env::var("XR_RATE_HOOKS").ok().and_then(|v| v.parse().ok()).unwrap_or(127)) & bit != 0
}

/// A scene node's animation lists, (list offset, step offset in each animation) (code.bin 0x2ECD20).
const NODE_ANIMS: [(u32, u32); 2] = [(0xDC, 0x25C), (0xCC, 0x16C)];
/// A motion object's f32 speed (code.bin 0x39664C).
const MOTION_SPEED: u32 = 0x24;

/// Cuts the scene node's animation steps to 1/S; what was there, to put back after the call.
pub fn cut_steps(g: &mut impl Calls, node: u32, s: u32) -> Vec<(u32, u32)> {
    let mut saved = Vec::new();
    for (list, step) in NODE_ANIMS {
        let (Some(begin), Some(end)) = (g.u32(node.wrapping_add(list)), g.u32(node.wrapping_add(list + 4))) else { continue };
        if end < begin || end - begin > 4 * 256 {
            continue;
        }
        for at in (begin..end).step_by(4) {
            if let Some(anim) = g.u32(at).filter(|&a| a != 0) {
                cut(g, anim.wrapping_add(step), s, &mut saved);
            }
        }
    }
    saved
}

/// Cuts the motion's speed to 1/S; what was there, to put back after the call.
pub fn cut_motion(g: &mut impl Calls, obj: u32, s: u32) -> Vec<(u32, u32)> {
    let mut saved = Vec::new();
    cut(g, obj.wrapping_add(MOTION_SPEED), s, &mut saved);
    // 1/2 exactly would take the in-between path (code.bin 0x396688 compares with 0.500001): just over it
    if s == 2 {
        if let Some(v) = g.f32(obj.wrapping_add(MOTION_SPEED)).filter(|v| v.abs() >= 0.5 && v.abs() < MOTION_SLOW) {
            g.write(obj.wrapping_add(MOTION_SPEED), &MOTION_SLOW.copysign(v).to_le_bytes());
        }
    }
    saved
}

/// The motion's in-between path is for speeds under this (code.bin 0x396830, 0x3F000011), so just over it.
const MOTION_SLOW: f32 = f32::from_bits(0x3F00_0012);

/// Cuts the environment table's 8 drift speeds to 1/S (code.bin 0x45B734); what was there, to put back.
pub fn cut_drifts(g: &mut impl Calls, table: u32, s: u32) -> Vec<(u32, u32)> {
    let mut saved = Vec::new();
    for i in 0..8 {
        cut(g, table.wrapping_add(0x24 * i + 0x20), s, &mut saved);
    }
    saved
}

/// Cuts an animation set's speeds to 1/S (code.bin 0x369F08: count at +4, list at +8, each animation's speed at
/// +0x30, skipped as the game skips it without its +0x1C); what was there, to put back.
pub fn cut_anim_set(g: &mut impl Calls, set: u32, s: u32) -> Vec<(u32, u32)> {
    let mut saved = Vec::new();
    let (Some(n), Some(list)) = (g.u32(set.wrapping_add(4)), g.u32(set.wrapping_add(8))) else { return saved };
    for i in 0..n.min(256) {
        if let Some(a) = g.u32(list.wrapping_add(4 * i)).filter(|&a| a != 0 && g.u32(a.wrapping_add(0x1C)).is_some_and(|r| r != 0)) {
            cut(g, a.wrapping_add(0x30), s, &mut saved);
        }
    }
    saved
}

/// Cuts a layout's animation speeds to 1/S (code.bin 0x14DEC4: count at +0xAC, 0x2C-byte entries at [+0xA8], the
/// speed at +0x1C of each one the game steps: flags +0, +1, +2 set and an animation at +8); what was there, to put back.
pub fn cut_layout_anims(g: &mut impl Calls, work: u32, s: u32) -> Vec<(u32, u32)> {
    let mut saved = Vec::new();
    let (Some(n), Some(list)) = (g.u32(work.wrapping_add(0xAC)), g.u32(work.wrapping_add(0xA8))) else { return saved };
    for i in 0..n.min(256) {
        let at = list.wrapping_add(0x2C * i);
        let on = g.u32(at).is_some_and(|f| f & 0xFF != 0 && f & 0xFF00 != 0 && f & 0xFF_0000 != 0) && g.u32(at.wrapping_add(8)).is_some_and(|a| a != 0);
        if on {
            cut(g, at.wrapping_add(0x1C), s, &mut saved);
        }
    }
    saved
}

fn cut(g: &mut impl Calls, at: u32, s: u32, saved: &mut Vec<(u32, u32)>) {
    if let Some(bits) = g.u32(at) {
        let v = f32::from_bits(bits);
        if v.is_finite() && v != 0.0 {
            saved.push((at, bits));
            g.write(at, &(v / s as f32).to_le_bytes());
        }
    }
}

/// Puts back what `cut_steps` or `cut_motion` cut.
pub fn put_back(g: &mut impl Calls, saved: &[(u32, u32)]) {
    for &(at, bits) in saved.iter().rev() {
        g.write(at, &bits.to_le_bytes());
    }
}

/// S while a battle runs at the rate, None otherwise.
pub fn battle_scale() -> Option<u32> {
    let s = SCALE.load(Ordering::Relaxed);
    (s > 1 && !FIELD_ON.load(Ordering::Relaxed)).then_some(s)
}

/// DllBattle call sites (return address minus DllBattle's code segment, X v1.0) whose object steps a speed of its
/// own each update, and where that speed is: the trainer object's frame (DllBattle 0x6DB5B0: +0x1A4 += +0x1AC).
const BATTLE_TRAINER_SITE: u32 = 0x6DB5D0;
/// Where DllBattle's code segment sits in X v1.0 (its sites above are given at that address).
const BATTLE_CODE: u32 = 0x6A5180;

/// A DllBattle site's object (`hooks::battle_part`): its speed cut to 1/S.
pub fn battle_site(g: &mut impl Calls, site: u32, obj: u32, s: u32) {
    if site == BATTLE_TRAINER_SITE - BATTLE_CODE {
        cut_kept(g, obj.wrapping_add(0x1AC), s);
    }
}

/// Speeds this module cut and left cut: (address, the value written).
static KEPT: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// Cuts a speed the game sets once and steps by every update to 1/S, and leaves it cut: a value that is not the one
/// written here is the game's (a new speed) and is cut again.
fn cut_kept(g: &mut impl Calls, at: u32, s: u32) {
    let (Some(bits), Ok(mut kept)) = (g.u32(at), KEPT.lock()) else { return };
    if kept.iter().any(|&(a, v)| a == at && v == bits) {
        return;
    }
    let v = f32::from_bits(bits);
    if !v.is_finite() || v == 0.0 {
        return;
    }
    let ours = (v / s as f32).to_bits();
    g.write(at, &ours.to_le_bytes());
    kept.retain(|&(a, _)| a != at);
    if kept.len() > 256 {
        kept.remove(0);
    }
    kept.push((at, ours));
}

/// DllBattle's tasks that move something smoothly (X v1.0 addresses): the camera moves 0x71C4AC and 0x71F1E4, the
/// models' moves 0x71D7B8 and 0x71DEC0. Each reads interpolators (BATTLE_INTERPS) once an update and ends when they
/// are done; over 30 they run every pass with the interpolators' steps cut to 1/S.
const BATTLE_MOVES: [u32; 4] = [0x71C4AC, 0x71F1E4, 0x71D7B8, 0x71DEC0];
/// DllBattle's interpolators, by the function at their vtable's +0xC (the step, which returns the value): the vector
/// (step +0x28 of +0x2C, its three scalars at +0x34..+0x3C) and two scalars (step +0x10 of +0x14).
const BATTLE_VECTOR: u32 = 0x7718FC;
const BATTLE_SCALARS: [u32; 2] = [0x77170C, 0x771780];
/// A `bx lr` in DllBattle: a task's function while its list passes without it.
const BATTLE_NOOP: u32 = 0x77177C;

/// A task list's pass in a battle over 30 (hooks.rs `tasks`; list: first node +0x1C, its end +0x14; a node's next +8,
/// its data +0x10, its function +0x14): the move tasks' interpolators are cut to 1/S (once each); on a pass that is not
/// one in S (`all` false) every other task's function is swapped for a no-op, so only the moves go on. Returns the
/// swapped nodes for `put_back_tasks`.
pub fn battle_tasks(g: &mut impl Calls, list: u32, code: u32, s: u32, all: bool) -> Vec<(u32, u32)> {
    let at = |a: u32| a.wrapping_sub(BATTLE_CODE).wrapping_add(code);
    let mut swapped = Vec::new();
    let end = list.wrapping_add(0x14);
    let mut node = g.u32(list.wrapping_add(0x1C)).unwrap_or(end);
    for _ in 0..1024 {
        if node == end || node == 0 {
            break;
        }
        let (Some(f), Some(data)) = (g.u32(node.wrapping_add(0x14)), g.u32(node.wrapping_add(0x10))) else { break };
        if BATTLE_MOVES.iter().any(|&m| at(m) == f) {
            for k in 0..6 {
                if let Some(p) = g.u32(data.wrapping_add(4 * k)) {
                    cut_interp(g, p, code, s, true);
                }
            }
        } else if !all {
            g.write(node.wrapping_add(0x14), &at(BATTLE_NOOP).to_le_bytes());
            swapped.push((node, f));
        }
        node = g.u32(node.wrapping_add(8)).unwrap_or(end);
    }
    swapped
}

/// Puts back the task functions `battle_tasks` swapped (those still swapped).
pub fn put_back_tasks(g: &mut impl Calls, swapped: &[(u32, u32)], code: u32) {
    let noop = BATTLE_NOOP.wrapping_sub(BATTLE_CODE).wrapping_add(code);
    for &(node, f) in swapped {
        if g.u32(node.wrapping_add(0x14)) == Some(noop) {
            g.write(node.wrapping_add(0x14), &f.to_le_bytes());
        }
    }
}

/// A DllBattle function that returns 0 (a state function's "not done yet").
const BATTLE_ZERO: u32 = 0x6A6FBC;

/// The battle main update's state functions off for a pass (hooks.rs `battle_main`; `this` its r0): each member function
/// pointer (function or vtable offset, then this-adjust * 2 + virtual) the update would call becomes BATTLE_ZERO with
/// the same adjust. Returns (address, the game's word, ours) for `put_back_states`.
pub fn battle_states_off(g: &mut impl Calls, this: u32, code: u32) -> Vec<(u32, u32, u32)> {
    let zero = BATTLE_ZERO.wrapping_sub(BATTLE_CODE).wrapping_add(code);
    let mut pmfs = Vec::new();
    // the state machine: called unless both words say none (then the update ends the machine; left alone)
    if let Some(sm) = g.u32(this.wrapping_add(0x820)).filter(|&a| a != 0) {
        let (f, adj) = (g.u32(sm).unwrap_or(0), g.u32(sm.wrapping_add(4)).unwrap_or(0));
        let done = g.u8(sm.wrapping_add(0x11D)) == Some(2);
        if !done && (f != 0 || adj & 1 != 0) {
            pmfs.push(sm);
        }
    }
    for i in 0..4 {
        if let Some(cl) = g.u32(this.wrapping_add(0x828 + 4 * i)).filter(|&a| a != 0) {
            pmfs.push(cl.wrapping_add(0x30));
        }
    }
    let mut saved = Vec::new();
    // the scene's message windows ([this+0x14] the scene, its +0x190 their manager, DllBattle 0x748BCC): the text
    // printer, its waits and its counters step once an update; their update is skipped while the manager's +0x34 is 0
    let scene = g.u32(this.wrapping_add(0x14)).filter(|&v| v != 0 && v & 3 == 0);
    let wm = scene.and_then(|v| g.u32(v.wrapping_add(0x190))).filter(|&w| w != 0 && w & 3 == 0);
    if let Some(at) = wm.map(|w| w.wrapping_add(0x34)) {
        if let Some(word) = g.u32(at).filter(|w| w & 0xFF != 0) {
            g.write(at, &(word & !0xFF).to_le_bytes());
            saved.push((at, word, word & !0xFF));
        }
    }
    for at in pmfs {
        let (Some(f), Some(adj)) = (g.u32(at), g.u32(at.wrapping_add(4))) else { continue };
        g.write(at, &zero.to_le_bytes());
        g.write(at.wrapping_add(4), &(adj & !1).to_le_bytes());
        saved.push((at, f, zero));
        saved.push((at.wrapping_add(4), adj, adj & !1));
    }
    saved
}

/// Puts back what `battle_states_off` swapped, where it is still ours.
pub fn put_back_states(g: &mut impl Calls, saved: &[(u32, u32, u32)]) {
    for &(at, game, ours) in saved {
        if g.u32(at) == Some(ours) {
            g.write(at, &game.to_le_bytes());
        }
    }
}

/// Cuts a DllBattle interpolator's step to 1/S (its step and length times S), if `p` is one (and its scalars too).
fn cut_interp(g: &mut impl Calls, p: u32, code: u32, s: u32, subs: bool) {
    let at = |a: u32| a.wrapping_sub(BATTLE_CODE).wrapping_add(code);
    let Some(step) = g.u32(p).and_then(|vt| g.u32(vt.wrapping_add(0xC))) else { return };
    if step == at(BATTLE_VECTOR) {
        stretch_count(g, p.wrapping_add(0x28), s);
        if subs {
            for o in [0x34, 0x38, 0x3C] {
                if let Some(q) = g.u32(p.wrapping_add(o)).filter(|&q| q != 0) {
                    cut_interp(g, q, code, s, false);
                }
            }
        }
    } else if BATTLE_SCALARS.iter().any(|&f| at(f) == step) {
        stretch_count(g, p.wrapping_add(0x10), s);
    }
}

/// A count (u32 step at `at`, u32 length at +4) times S, once: a length that is not the one written here is the game's.
// shortcut: a new length equal to one written here is taken as already cut; fine for the battle's short moves.
fn stretch_count(g: &mut impl Calls, at: u32, s: u32) {
    let n_at = at.wrapping_add(4);
    let (Some(i), Some(n), Ok(mut kept)) = (g.u32(at), g.u32(n_at), KEPT.lock()) else { return };
    if kept.iter().any(|&(a, v)| a == n_at && v == n) || n == 0 || n > 0x0100_0000 {
        return;
    }
    let ours = n * s;
    g.write(at, &(i.min(n) * s).to_le_bytes());
    g.write(n_at, &ours.to_le_bytes());
    kept.retain(|&(a, _)| a != n_at);
    if kept.len() > 256 {
        kept.remove(0);
    }
    kept.push((n_at, ours));
}

/// The rate the game's logic runs at: XR_FRAME_RATE (headless runs) over the setting.
pub fn rate(sys: &xr_hle::System) -> u32 {
    static ENV: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    ENV.get_or_init(|| std::env::var("XR_FRAME_RATE").ok().and_then(|v| v.parse().ok())).unwrap_or(sys.frame_rate)
}

/// One field update's worth: `rate` is the setting (30, 60, 120, 240). The cycles from one refresh to the next
/// it wants: over 60 the screens refresh rate / 60 times as often while the field runs every pass.
pub fn tick(g: &mut impl Calls, dll: Option<Dll>, battle: bool, rate: u32) -> u64 {
    let console = xr_hle::CYCLES_PER_FRAME;
    let Ok(mut st) = STATE.lock() else { return console };
    // at 30 nothing is touched, unless the setting just went back to 30 with the field at 60: that goes back too
    if rate <= 30 && !st.forced {
        return console;
    }
    let Some(ctrl) = g.u32(CONTROLLER_PTR).filter(|&c| c != 0) else { return console };
    let (Some(req), Some(mode)) = (g.u8(ctrl + REQUEST), g.u8(ctrl + MODE)) else { return console };
    let player = dll.and_then(|d| field::player(g, d.data));
    // the X menu (and the events that freeze the field) set the snapshot's 0x100: they stay at the game's 30 for now
    let busy = dll.and_then(|d| g.u32(d.data + field::SNAPSHOT + SNAP_FLAGS)).is_none_or(|f| f & FLAG_BUSY != 0);
    // an event (a door, a talk, a script) turns the field's touch screen off: events stay at 30 too
    let event = dll
        .and_then(|d| g.u32(d.data + field::SNAPSHOT + field::SNAP_PLAYER_OBJ))
        .and_then(|pfo| g.u32(pfo + PFO_SUBSCREEN))
        .filter(|&o| o != 0)
        .is_none_or(|o| {
            // after a battle: the touch screen's menu waits locked, then slides in (its own steps, not cut): at 30
            // shortcut: the slide's length (SLIDE_IN updates) is measured on the field's menu; other menus would need theirs
            g.u8(o + SUBSCREEN_ON) == Some(0) || g.u8(o + SUBSCREEN_LOCK) != Some(0) || g.u32(o + SUBSCREEN_SHOWN).is_none_or(|n| n < SLIDE_IN)
        });
    let busy = busy || event;
    // battles at the rate; XR_RATE_BATTLE=0 (test override) keeps them at 30. The field only without a battle
    // (DllField stays loaded in one)
    let battle_on = std::env::var("XR_RATE_BATTLE").as_deref() != Ok("0");
    let want = rate > 30 && ((player.is_some() && !busy && !battle) || (battle && battle_on && BATTLE_READY.load(Ordering::Relaxed)));
    if trace() {
        let flags = dll.and_then(|d| g.u32(d.data + field::SNAPSHOT + SNAP_FLAGS)).unwrap_or(0);
        let sub = dll.and_then(|d| g.u32(d.data + field::SNAPSHOT + field::SNAP_PLAYER_OBJ)).and_then(|pfo| g.u32(pfo + PFO_SUBSCREEN)).unwrap_or(0);
        crate::log::log(&format!("rate: pass sub {sub:08X} mode {mode} req {req} flags {flags:08X} event {event} battle {battle} player {} forced {} motion calls {}", player.is_some(), st.forced, MOTION_CALLS.load(Ordering::Relaxed)));
        trace_player(g, dll);
    }
    if want && mode == 1 && req == NO_REQUEST && !trace_only() {
        g.write(ctrl + REQUEST, &[0]);
        st.forced = true;
    } else if !want && st.forced && req == NO_REQUEST {
        if mode == 0 {
            g.write(ctrl + REQUEST, &[1]);
        }
        st.forced = false;
    }
    let s = (rate / 30).clamp(2, 8) as u16;
    let active = mode == 0 && st.forced;
    // the field's own scaling only with the field running (in a battle DllField stays loaded, its objects stale)
    let field_on = active && !battle;
    // back at 30 (an event, the menu, a scene of the game's): steps stretched for S go back to the game's count
    if st.was_active && !field_on {
        for t in st.slots.iter().filter(|t| t.len != t.len0 && matches!(kind(t.code), Some(Kind::Up))) {
            let (Some(len), Some(done)) = (g.u16(t.addr + ACTION_STEPS), g.u16(t.addr + ACTION_STEPS + 2)) else { continue };
            if len == t.len && done < len {
                let back = (u32::from(done) * u32::from(t.len0) / u32::from(len)) as u16;
                g.write(t.addr + ACTION_STEPS, &t.len0.to_le_bytes());
                g.write(t.addr + ACTION_STEPS + 2, &back.to_le_bytes());
            }
        }
        st.slots.clear();
    }
    st.was_active = field_on;
    SCALE.store(if active { u32::from(s) } else { 1 }, Ordering::Relaxed);
    FIELD_ON.store(field_on, Ordering::Relaxed);
    PASSES.fetch_add(1, Ordering::Relaxed);
    // the motion hook cuts the motions itself (a DLL built with it); without it they are cut here, after the update
    let motion_hooked = std::mem::replace(&mut st.motion_calls, MOTION_CALLS.load(Ordering::Relaxed)) != st.motion_calls;
    let slots = match player {
        Some(p) if field_on || trace_only() => field::slots(g, p.manager).unwrap_or_default(),
        _ => Vec::new(),
    };
    // the bike and the skates: the controller moves the player by a velocity each update; S updates go as far as one
    // did. On the bike the velocity is set again each update; on the skates it speeds up and slows down an update at a
    // time, and that change is cut to 1/S too (the position's step with it)
    let ctrl = dll.filter(|_| field_on).and_then(|d| g.u32(d.data + field::SNAPSHOT + field::SNAP_CONTROLLER)).filter(|&c| c != 0);
    let form = ctrl.and_then(|c| g.u8(c + field::CTRL_FORM));
    match (player.filter(|_| field_on), ctrl, form) {
        (Some(p), Some(c), Some(field::FORM_BIKE | field::FORM_SKATES)) => {
            let mut ratio = [1.0f32; 3];
            if form == Some(field::FORM_SKATES) {
                let mut v: Vec<f32> = (0..3).map(|i| g.f32(c + CTRL_VELOCITY + 4 * i).unwrap_or(0.0)).collect();
                if st.velocity.len() == 3 {
                    for i in 0..3 {
                        let dv = v[i] - st.velocity[i];
                        if dv.is_finite() && dv != 0.0 && dv.abs() <= 18.0 {
                            let ours = st.velocity[i] + dv / f32::from(s);
                            if v[i].abs() > 0.01 {
                                ratio[i] = (ours / v[i]).clamp(0.0, 2.0);
                            }
                            g.write(c + CTRL_VELOCITY + 4 * i as u32, &ours.to_le_bytes());
                            v[i] = ours;
                        }
                    }
                }
                st.velocity = v;
            } else {
                st.velocity.clear();
            }
            let mm = p.move_model;
            let now: Vec<f32> = (0..3).map(|i| g.f32(mm + MM_POS + 4 * i).unwrap_or(0.0)).collect();
            let mut to = now.clone();
            if st.bike.len() == 3 {
                let step: Vec<f32> = (0..3).map(|i| now[i] - st.bike[i]).collect();
                if step.iter().all(|d| d.abs() <= 18.0) && step.iter().any(|d| *d != 0.0) {
                    to = (0..3).map(|i| st.bike[i] + step[i] * ratio[i] / f32::from(s)).collect();
                    for i in 0..3 {
                        for at in [MM_POS, MM_POS_COPY] {
                            g.write(mm + at + 4 * i as u32, &to[i].to_le_bytes());
                        }
                    }
                }
            }
            st.bike = to;
        }
        _ => {
            st.bike.clear();
            st.velocity.clear();
        }
    }
    // the map's own animations (flags, water, doors' frames) step by the manager's step each update: 1/S of it while
    // the field runs at S, the game's own again after
    let anims = player.filter(|_| field_on).and_then(|p| g.u32(p.move_model + MM_MAP_ANIMS)).filter(|&m| m != 0);
    match (anims, st.map_anims) {
        (Some(m), Some((was, _, ours))) if was == m && g.f32(m + ANIMS_STEP) == Some(ours) => {}
        (Some(m), _) => {
            if let Some((was, theirs, _)) = st.map_anims.filter(|(was, ..)| *was != m) {
                g.write(was + ANIMS_STEP, &theirs.to_le_bytes());
            }
            if let Some(theirs) = g.f32(m + ANIMS_STEP).filter(|v| v.is_finite() && *v > 0.0) {
                let ours = theirs / f32::from(s);
                g.write(m + ANIMS_STEP, &ours.to_le_bytes());
                st.map_anims = Some((m, theirs, ours));
            }
        }
        (None, Some((was, theirs, ours))) => {
            if g.f32(was + ANIMS_STEP) == Some(ours) {
                g.write(was + ANIMS_STEP, &theirs.to_le_bytes());
            }
            st.map_anims = None;
        }
        (None, None) => {}
    }
    // the camera's follow point eases toward the player by a share of the way each update (a share the game makes
    // smaller at its 60 Hz mode, though not by enough): each update's step is cut to match 30's ease
    let cam = dll.filter(|_| field_on).and_then(|d| {
        let pfo = g.u32(d.data + field::SNAPSHOT + field::SNAP_PLAYER_OBJ).filter(|&p| p != 0)?;
        g.u32(pfo + field::PFO_CAMERA).filter(|&c| c != 0)
    });
    match cam {
        Some(c) => {
            let now: Vec<f32> = (0..CAM_STATE_WORDS).map(|i| g.f32(c + CAM_STATE + 4 * i).unwrap_or(f32::NAN)).collect();
            let mut to = now.clone();
            if st.follow.len() == now.len() {
                for (i, v) in to.iter_mut().enumerate() {
                    let step = now[i] - st.follow[i];
                    if step.is_finite() && step != 0.0 && step.abs() <= 18.0 {
                        *v = st.follow[i] + step / (f32::from(s) * CAM_EASE);
                        g.write(c + CAM_STATE + 4 * i as u32, &v.to_le_bytes());
                    }
                }
            }
            st.follow = to;
        }
        None => st.follow.clear(),
    }
    // the controller's counters of updates (standing still, since the last step): one count in S
    if let Some(c) = dll.filter(|_| field_on).and_then(|d| g.u32(d.data + field::SNAPSHOT + field::SNAP_CONTROLLER)).filter(|&c| c != 0) {
        for (i, &(at, wide)) in CTRL_COUNTERS.iter().enumerate() {
            slow(g, c + at, wide, s, &mut st.counters[i]);
        }
    } else {
        st.counters = Default::default();
    }
    // XR_RATE_SLOW=ADDR,... (hex, debug): u32 counters of updates counted one in S while the rate runs
    if active {
        static SLOW: Mutex<Vec<(u32, Counter)>> = Mutex::new(Vec::new());
        let mut v = SLOW.lock().unwrap();
        if v.is_empty() {
            *v = std::env::var("XR_RATE_SLOW").unwrap_or_default().split(',').filter_map(|a| u32::from_str_radix(a.trim(), 16).ok()).map(|a| (a, Counter::default())).collect();
        }
        for (a, c) in v.iter_mut() {
            slow(g, *a, true, s, c);
        }
    }
    let mut next = Vec::with_capacity(slots.len());
    for slot in &slots {
        let a = slot.addr;
        let (Some(len), Some(done)) = (g.u16(a + ACTION_STEPS), g.u16(a + ACTION_STEPS + 2)) else {
            continue;
        };
        let done = done as i16;
        if trace() {
            let code = g.u32(a + ACTION).unwrap_or(0);
            let state = g.u32(a + ACTION_STATE).unwrap_or(0);
            let frame = g.u32(a + MOTION).and_then(|m| g.f32(m + MOTION_FRAME)).unwrap_or(0.0);
            crate::log::log(&format!("rate: id {:X} code {code:08X} state {state} len {len} done {done} frame {frame} (tracked {:?})", slot.id, st.slots.iter().find(|t| t.addr == a).map(|t| (t.len, t.extra))));
        }
        if trace_only() {
            continue;
        }
        let (code, playing) = (g.u16(a + ACTION).unwrap_or(0), g.u32(a + ACTION_STATE) == Some(1));
        let old = st.slots.iter().find(|t| t.addr == a).copied();
        // the same action as last pass: same code and length, still counting on (a new one starts over)
        let same = old.filter(|o| o.code == code && o.len == len && (done >= o.done || !matches!(kind(code), Some(Kind::Up))) && !(playing && !o.playing));
        let t = match (same, kind(code)) {
            (Some(t), _) => {
                if t.extra > 0 && done > 0 && (done as u16) < t.len {
                    let to = (done as u16 + t.extra).min(t.len - 1);
                    g.write(a + ACTION_STEPS + 2, &to.to_le_bytes());
                }
                t
            }
            (None, Some(Kind::Up)) if playing => stretch(g, a, len, done, s, st.was_active && old.is_some()),
            (None, Some(Kind::Down)) if playing => {
                if done >= 1 {
                    let left = (turn(done as u16 + 1, s) - 1).min(i16::MAX as u16);
                    g.write(a + ACTION_STEPS + 2, &left.to_le_bytes());
                }
                Stretch { addr: a, len, len0: len, extra: 0, code, frame: f32::NAN, wait: u16::MAX, phase: 0, done: 0, playing: false }
            }
            _ => Stretch { addr: a, len, len0: len, extra: 0, code, frame: f32::NAN, wait: u16::MAX, phase: 0, done: 0, playing: false },
        };
        let done_now = g.u16(a + ACTION_STEPS + 2).map_or(done, |d| d as i16);
        let mut t = Stretch { code, frame: f32::NAN, done: done_now, playing, ..t };
        // the motion's frame: the game steps it by its speed each update; S updates go as far as one did
        if let Some(m) = g.u32(a + MOTION).filter(|&m| m != 0 && !motion_hooked) {
            let (f, n) = (g.f32(m + MOTION_FRAME).unwrap_or(f32::NAN), g.f32(m + MOTION_LENGTH).unwrap_or(0.0));
            let was = old.map_or(f32::NAN, |o| o.frame);
            let step = if f >= was { f - was } else { f + n - was };
            t.frame = f;
            if step > 0.0 && step <= 2.0 && n > 0.0 {
                let mut to = was + step / f32::from(s);
                if to >= n {
                    to -= n;
                }
                g.write(m + MOTION_FRAME, &to.to_le_bytes());
                t.frame = to;
            }
        }
        // the NPCs' wait before their next wander step counts down an update at a time: one in S counts
        if let Some(w) = g.u16(a + WANDER_WAIT) {
            let (was, phase) = old.map_or((u16::MAX, 0), |o| (o.wait, o.phase));
            (t.wait, t.phase) = (w, 0);
            if w.wrapping_add(1) == was {
                let phase = (phase + 1) % s;
                if phase != 0 {
                    g.write(a + WANDER_WAIT, &was.to_le_bytes());
                    t.wait = was;
                }
                t.phase = phase;
            }
        }
        next.push(t);
    }
    st.slots = next;
    if active { console * 60 / u64::from(rate.max(60)) } else { console }
}

/// A new action (length `len`, `done` updates in): made S times longer. One not yet counted just gets the longer
/// length; one an update in (the update that started it counted it) keeps where that update put it, and the rest of
/// its way is spread over the S * len - 1 updates left, on a finer count: length len * (S * len - 1), each update
/// len - 1 on (the game's 1 and our len - 2).
fn stretch(g: &mut impl Calls, a: u32, len: u16, done: i16, s: u16, fresh: bool) -> Stretch {
    let keep = Stretch { addr: a, len, len0: len, extra: 0, code: 0, frame: f32::NAN, wait: u16::MAX, phase: 0, done: 0, playing: false };
    let Some((len, done, extra)) = stretched(len, done, s, fresh) else { return keep };
    g.write(a + ACTION_STEPS, &len.to_le_bytes());
    g.write(a + ACTION_STEPS + 2, &done.to_le_bytes());
    Stretch { len, extra, ..keep }
}

/// `stretch`'s count: the new length, updates done and the extra each pass adds (None: left as it is). `fresh`: the
/// update that counted the action's first `done` ran at S (else they were the game's 30 Hz updates, each S of ours).
/// The rest of the way is spread over the updates left on a finer count: length len * left, each update len - done
/// on (the game's 1 and our extra).
fn stretched(len: u16, done: i16, s: u16, fresh: bool) -> Option<(u16, u16, u16)> {
    let (len, done, s) = (u32::from(len), u32::try_from(done).ok()?, u32::from(s));
    if done == 0 {
        return (len > 0 && len * s <= i16::MAX as u32).then(|| ((len * s) as u16, 0, 0));
    }
    if done >= len {
        return None;
    }
    let left = if fresh { s * len - done } else { s * (len - done) };
    let fine = len * left;
    if fine > i16::MAX as u32 {
        return None;
    }
    let extra = len - done - 1;
    Some((fine as u16, (done * left + extra) as u16, extra as u16))
}

/// A turn of `t` updates at S: the controller looks at the d-pad 2 updates after it ends (3 frames at 30), so one
/// held 2t + 3 frames walks at 30; at S the turn lasts (2t + 3) * S / 2 - 2 updates.
fn turn(t: u16, s: u16) -> u16 {
    ((2 * t + 3) * s / 2).saturating_sub(2).max(t)
}

/// XR_RATE_TRACE=1: every move model's action counts and the player controller's changes, each update, to the log.
pub(crate) fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XR_RATE_TRACE").is_some())
}

/// XR_RATE_TRACE=2: trace only, the game left at its own 30.
fn trace_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("XR_RATE_TRACE").is_ok_and(|v| v == "2"))
}

/// XR_RATE_TRACE: the player controller's words that changed since the last pass.
fn trace_player(g: &mut impl Calls, dll: Option<Dll>) {
    static LAST: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    let Some(d) = dll else { return };
    let Some(ctrl) = g.u32(d.data + field::SNAPSHOT + field::SNAP_CONTROLLER).filter(|&c| c != 0) else { return };
    let now: Vec<u32> = (0..0x100).map(|i| g.u32(ctrl + 4 * i).unwrap_or(0)).collect();
    let mut last = LAST.lock().unwrap();
    if last.len() == now.len() {
        let changed: Vec<String> = (0..now.len()).filter(|&i| now[i] != last[i]).map(|i| format!("+{:X}={:08X}", 4 * i, now[i])).collect();
        if !changed.is_empty() {
            crate::log::log(&format!("rate: ctrl {ctrl:08X} {}", changed.join(" ")));
        }
    }
    *last = now;
}

#[cfg(test)]
mod tests {
    use super::{stretched, turn};

    /// The game's walk: each update done + 1 then position = done / length, ending at the length; ours adds extra
    /// after each update. A step seen one update in takes S * len updates in all, never goes back, and ends at 1.
    #[test]
    fn a_stretched_step_takes_s_times_as_long() {
        for (len, s) in [(7u16, 2u16), (7, 4), (7, 8), (4, 2), (4, 8), (2, 2), (8, 4)] {
            let (l, mut done, extra) = stretched(len, 1, s, true).unwrap();
            let mut at = vec![1.0 / f64::from(len)];
            loop {
                done += 1;
                at.push(f64::from(done) / f64::from(l));
                if done >= l {
                    break;
                }
                done += extra;
            }
            assert_eq!(at.len(), usize::from(len * s), "len {len} at S {s}");
            assert!(at.windows(2).all(|w| w[1] > w[0]), "len {len} at S {s}: {at:?}");
            assert_eq!(*at.last().unwrap(), 1.0);
        }
        // not counted yet: just longer; too long for the count, or further in: left alone
        assert_eq!(stretched(7, 0, 4, true), Some((28, 0, 0)));
        assert_eq!(stretched(200, 1, 8, true), None);
        assert_eq!(stretched(7, 7, 2, false), None);
        // 3 of 7 done at 30 (each S of ours): the other 4 take 4 * S updates, from 3/7 on to the end
        for s in [2u16, 4, 8] {
            let (l, mut done, extra) = stretched(7, 3, s, false).unwrap();
            let mut n = 0;
            let mut last = 3.0 / 7.0;
            loop {
                done += 1;
                n += 1;
                let at = f64::from(done) / f64::from(l);
                assert!(at > last);
                last = at;
                if done >= l {
                    break;
                }
                done += extra;
            }
            assert_eq!(n, 4 * s);
        }
    }

    /// A turn: 2 updates at 30; the d-pad held 7 frames (60 Hz ones) walks at every rate.
    #[test]
    fn a_turn_lasts_as_long() {
        assert_eq!(turn(2, 2), 5);
        assert_eq!(turn(2, 4), 12);
        assert_eq!(turn(2, 8), 26);
    }
}
