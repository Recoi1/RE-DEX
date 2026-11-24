//! `--character` on a new game: DllIntro's questions (boy or girl, the look, the name and its confirm)
//! are skipped, and the intro goes on as if the player had answered with the character, through the intro's own code.
//!
//! DllIntro's sequence (code+0x6FC0, one state per frame in its work's +0x24; docs/re/x-field-move-models.md, "The
//! intro's sequence"):
//! 0x0A the mirrors are up; 0x0B..0x0C boy or girl; 0x0E..0x10 the look; 0x13..0x14 the name question;
//! 0x15 fade out; 0x16..0x17 the screens are handed over; 0x17 commits gender and look (code+0x6E84) and starts the
//! name entry (DllStrInput); 0x18 commits the typed name to MyStatus; 0x19..0x1B the mirror shows the player (a model
//! built from MyStatus); 0x1B..0x1D the name said back and confirmed, yes/no; 0x1D's yes fills the gender's
//! starting clothes; 0x1E on, the farewell and the wake-up.
//!
//! The skip, from the per-frame hook (the game's thread, between procs): at 0x0A the answers go into the work and the
//! sequence goes to 0x15; at 0x17 (once the game's own ready check holds) it does 0x17's work with the name put in
//! the name entry's buffer instead of starting it; at 0x1B (the mirror's camera at its stop) it registers the name for
//! the messages and answers yes, going to 0x1D. Everything MyStatus and the clothes get is then the intro's own code.
use crate::log;
use std::sync::Mutex;
use xr_net::Guest;
use xr_net::x::field::{self, Appearance, MS_FASHION, MS_GENDER};
use xr_net::x::look::Look;
use xr_net::x::puppet::Calls;

/// DllIntro's work: its vtable at rodata (CRO segment 1) + this.
const WORK_VTABLE: u32 = 0x9E0;
/// The work's fields: sequence state, camera frame (f32), two counters (f32), a flag, the message window, the
/// gender (u8; +0x3C the last one shown), the look (0..2, u32), the name entry's result (u8) and its StrBuf.
const W_MSG: u32 = 0x1C;
const W_STATE: u32 = 0x24;
const W_CAMERA: u32 = 0x28;
const W_COUNT: u32 = 0x2C;
const W_COUNT2: u32 = 0x30;
const W_FLAG: u32 = 0x34;
const W_SHOWN: u32 = 0x3C;
const W_GENDER: u32 = 0x3D;
const W_LOOK: u32 = 0x40;
const W_ENTERED: u32 = 0x60;
const W_NAME: u32 = 0x64;
/// The message window's yes/no answer (u16: 0 yes, 1 no, 2 none yet).
const MSG_ANSWER: u32 = 0x1E;
/// The camera's stops (f32 frame, u32 flag) at rodata + this; 0x1B waits for stop 13.
const CAMERA_STOPS: u32 = 0x218;
const STOP_MIRROR: u32 = 13;

/// DllIntro code offsets: its own functions, and the import stubs 0x17 calls code.bin through.
const COMMIT_LOOK: u32 = 0x6E84; // (work): gender, default outfit, sprite bytes and the look's features into MyStatus
const MSG_COMMAND: u32 = 0x5094; // (window, n): 0x14 registers MyStatus's name for the messages
const STUB_GFX: u32 = 0x11C; // code.bin 0x1172C8 in X
const STUB_GFX_READY: u32 = 0x5B4; // 0x4974CC
const STUB_HAND_OVER: u32 = 0x5BC; // 0x38B898
const STUB_HAND_OVER2: u32 = 0x5C4; // 0x358E40
const STUB_STRBUF_SET: u32 = 0x5CC; // 0x38E478 (strbuf, utf16 *)
const STUB_GAMESYS: u32 = 0x27C; // 0x14EB04: [[it + 0x1C]] is GameData

const QUESTIONS: u32 = 0x0A;
const SKIP_TO: u32 = 0x15;
const NAME: u32 = 0x17;
const NAME_DONE: u32 = 0x18;
const CONFIRM: u32 = 0x1B;
const ANSWERED: u32 = 0x1D;
const STATES: u32 = 0x28;

/// The heap the work is allocated in (X: 0x081FB234), searched a page at a time.
const HEAP: std::ops::Range<u32> = 0x0800_0000..0x0900_0000;
/// While the work is not found, it is looked for this often (ticks).
const SCAN_EVERY: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    /// waiting for the mirrors (state 0x0A)
    Questions,
    /// sent to the fade-out, waiting for the name entry's turn (0x17)
    Name,
    /// the name is in, waiting for the mirror's confirm (0x1B at its camera stop)
    Confirm,
    /// answered yes: the intro goes on by itself
    Done,
}

#[derive(Debug, PartialEq)]
enum Act {
    Questions,
    Name,
    Confirm,
    Nothing,
}

/// What this tick does, for the step reached and the work's state (and camera frame vs the mirror's stop).
fn act(step: Step, state: u32, camera: f32, mirror: f32) -> Act {
    match (step, state) {
        (Step::Questions, QUESTIONS) => Act::Questions,
        (Step::Name, NAME) => Act::Name,
        (Step::Confirm, CONFIRM) if camera >= mirror => Act::Confirm,
        _ => Act::Nothing,
    }
}

/// The intro look (0..2) whose features are nearest the character's: all four the same first, then skin, eyes, hair
/// colour and style in that order. `presets`: each look's features as the intro commits them.
fn nearest(presets: &[Look], c: &Look) -> usize {
    let score = |p: &Look| (p.skin == c.skin, p.eyes == c.eyes, p.hair_colour == c.hair_colour, p.hair_style == c.hair_style);
    let full = |p: &Look| (p.skin, p.eyes, p.hair_colour, p.hair_style) == (c.skin, c.eyes, c.hair_colour, c.hair_style);
    (0..presets.len()).max_by_key(|&i| (full(&presets[i]), score(&presets[i]), std::cmp::Reverse(i))).unwrap_or(0)
}

pub struct Skip {
    look: Appearance,
    name: Vec<u16>,
    step: Step,
    work: Option<u32>,
    ticks: u64,
}

static SKIP: Mutex<Option<Skip>> = Mutex::new(None);

/// A character is played: its new game's intro takes these answers.
pub fn arm(look: Appearance, name: &str) {
    // at most 12 UTF-16 units, cut at a character boundary so a surrogate pair is never split
    let mut units = 0;
    let name = name.chars().take_while(|c| { units += c.len_utf16(); units <= 12 }).collect::<String>();
    let name = name.encode_utf16().chain([0]).collect();
    *SKIP.lock().unwrap_or_else(|e| e.into_inner()) = Some(Skip { look, name, step: Step::Questions, work: None, ticks: 0 });
}

/// The per-frame hook's part: DllIntro's code and rodata segments (None while it is not loaded) and the heap.
pub fn tick(g: &mut impl Calls, intro: Option<(u32, u32)>, mem: impl FnOnce() -> Option<u32>) {
    let mut lock = SKIP.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = lock.as_mut() else { return };
    let Some((code, rodata)) = intro else {
        // not in the intro (or it ended, or a soft reset unloaded it): the next intro starts over
        (s.work, s.step) = (None, Step::Questions);
        return;
    };
    s.ticks += 1;
    if s.step == Step::Done {
        return;
    }
    let vtable = rodata + WORK_VTABLE;
    if s.work.is_some_and(|w| g.u32(w) != Some(vtable)) {
        s.work = None;
    }
    if s.work.is_none() {
        if s.ticks % SCAN_EVERY != 1 {
            return;
        }
        s.work = mem();
        let Some(w) = s.work else { return };
        log::log(&format!("intro: DllIntro's work at 0x{w:08X}"));
    }
    let w = s.work.unwrap();
    let (Some(state), Some(camera), Some(mirror)) = (g.u32(w + W_STATE), g.f32(w + W_CAMERA), g.f32(rodata + CAMERA_STOPS + 8 * STOP_MIRROR)) else { return };
    match act(s.step, state, camera, mirror) {
        Act::Nothing => {}
        Act::Questions => {
            // the answers, as 0x0C and 0x10 leave them, then 0x15 (the fade before the name) as 0x14 goes to it
            let gender = s.look.gender;
            g.write(w + W_GENDER, &[gender]);
            g.write(w + W_SHOWN, &[gender]);
            goto(g, w, SKIP_TO);
            s.step = Step::Name;
            log::log(&format!("intro: the questions skipped: a {}", if gender == 0 { "boy" } else { "girl" }));
        }
        Act::Name => {
            // 0x17's own check: the screens are handed over
            let gfx = g.call(code + STUB_GFX, &[]);
            if g.call(code + STUB_GFX_READY, &[gfx]) == 0 {
                return;
            }
            g.call(code + STUB_HAND_OVER, &[1]);
            g.call(code + STUB_HAND_OVER2, &[gfx]);
            goto(g, w, NAME_DONE);
            commit_look(g, code, w, &s.look);
            // what the name entry leaves: the name in the intro's StrBuf and "entered"; 0x18 copies it to MyStatus
            let text = g.scratch();
            let bytes: Vec<u8> = s.name.iter().flat_map(|u| u.to_le_bytes()).collect();
            g.write(text, &bytes);
            if let Some(strbuf) = g.u32(w + W_NAME) {
                g.call(code + STUB_STRBUF_SET, &[strbuf, text]);
            }
            g.write(w + W_ENTERED, &[1]);
            s.step = Step::Confirm;
            log::log(&format!("intro: the name entry skipped: {}", String::from_utf16_lossy(&s.name[..s.name.len() - 1])));
        }
        Act::Confirm => {
            // 0x1B's name registration, then 0x1D with "yes" (it fills the starting clothes)
            let Some(msg) = g.u32(w + W_MSG) else { return };
            g.call(code + MSG_COMMAND, &[msg, 0x14]);
            g.write(msg + MSG_ANSWER, &0u16.to_le_bytes());
            goto(g, w, ANSWERED);
            s.step = Step::Done;
            log::log("intro: the confirm skipped (yes)");
        }
    }
}

/// The sequence to `state`, with the counters and flag as every state change of the intro's resets them.
fn goto(g: &mut impl Calls, w: u32, state: u32) {
    debug_assert!(state < STATES);
    g.write(w + W_STATE, &state.to_le_bytes());
    g.write(w + W_COUNT, &0f32.to_le_bytes());
    g.write(w + W_COUNT2, &0f32.to_le_bytes());
    g.write(w + W_FLAG, &[0]);
}

/// The intro's gender and look commit (code+0x6E84) with the intro look nearest the character's; the mirror's model
/// is then built from MyStatus, so a look no preset has is put on after it (as `LookSync` would before the field).
fn commit_look(g: &mut impl Calls, code: u32, w: u32, look: &Appearance) {
    let gs = g.call(code + STUB_GAMESYS, &[]);
    let ms = g.u32(gs + 0x1C).and_then(|p| g.u32(p)).and_then(|gd| field::mystatus_of(g, gd));
    let Some(ms) = ms else {
        // no MyStatus to compare with: the first look, as a player pressing A three times
        g.write(w + W_LOOK, &0u32.to_le_bytes());
        g.call(code + COMMIT_LOOK, &[w]);
        log::log("intro: MyStatus not found, the intro's first look committed");
        return;
    };
    let mut presets = Vec::new();
    for i in 0..3u32 {
        g.write(w + W_LOOK, &i.to_le_bytes());
        g.call(code + COMMIT_LOOK, &[w]);
        presets.push(appearance(g, ms).unwrap_or(*look));
    }
    let best = nearest(&presets.iter().map(Look::of).collect::<Vec<_>>(), &Look::of(look));
    g.write(w + W_LOOK, &(best as u32).to_le_bytes());
    g.call(code + COMMIT_LOOK, &[w]);
    let now = appearance(g, ms);
    if now.as_ref() != Some(look) {
        g.write(ms + MS_FASHION, &look.fashion);
        let hex = |a: &Appearance| a.to_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
        let intro: Vec<String> = presets.iter().map(hex).collect();
        log::log(&format!("intro: look {best} committed, then the character's own outfit (the intro's looks are {})", intro.join(", ")));
    } else {
        log::log(&format!("intro: look {best} committed (the character's)"));
    }
}

/// MyStatus's gender and outfit (`ms`: its data block).
fn appearance(g: &mut impl Guest, ms: u32) -> Option<Appearance> {
    let mut fashion = [0u8; 16];
    let gender = g.u8(ms + MS_GENDER)?;
    g.read(ms + MS_FASHION, &mut fashion).then_some(Appearance { gender, fashion })
}

/// DllIntro's work in the heap: its vtable, a state the sequence has, and a StrBuf.
pub fn find_work(mem: &mut xr_hle::memory::Memory, rodata: u32) -> Option<u32> {
    const PAGE: u32 = 0x1000;
    let vtable = (rodata + WORK_VTABLE).to_le_bytes();
    for page in HEAP.step_by(PAGE as usize) {
        let Some(s) = mem.slice_mut(page, PAGE) else { continue };
        let hits: Vec<u32> = s.chunks_exact(4).enumerate().filter(|(_, w)| *w == vtable).map(|(i, _)| page + 4 * i as u32).collect();
        for w in hits {
            let word = |mem: &mut xr_hle::memory::Memory, at: u32| mem.slice_mut(at, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
            if word(mem, w + W_STATE).is_some_and(|s| s < STATES) && word(mem, w + W_NAME).is_some_and(|p| p != 0) {
                return Some(w);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_skip_happens_at_its_state_and_in_order() {
        let m = 1710.0;
        assert_eq!(act(Step::Questions, 0x0A, 0.0, m), Act::Questions);
        assert_eq!(act(Step::Questions, 0x0B, 0.0, m), Act::Nothing, "already asking: left to the player");
        assert_eq!(act(Step::Questions, 0x17, 0.0, m), Act::Nothing);
        assert_eq!(act(Step::Name, 0x16, 0.0, m), Act::Nothing);
        assert_eq!(act(Step::Name, 0x17, 0.0, m), Act::Name);
        assert_eq!(act(Step::Confirm, 0x1B, 1709.0, m), Act::Nothing, "the camera still on its way up the mirror");
        assert_eq!(act(Step::Confirm, 0x1B, 1710.0, m), Act::Confirm);
        assert_eq!(act(Step::Done, 0x0A, 0.0, m), Act::Nothing);
    }

    #[test]
    fn the_nearest_intro_look_is_picked() {
        let l = |skin, hair_style, hair_colour, eyes| Look { gender: 0, skin, hair_style, hair_colour, eyes };
        let presets = [l(1, 0, 3, 1), l(2, 0, 1, 2), l(3, 0, 0, 3)];
        assert_eq!(nearest(&presets, &l(2, 0, 1, 2)), 1, "a preset itself");
        assert_eq!(nearest(&presets, &l(3, 4, 2, 0)), 2, "the skin first");
        assert_eq!(nearest(&presets, &l(0, 4, 2, 2)), 1, "then the eyes");
        assert_eq!(nearest(&presets, &l(0, 4, 4, 0)), 0, "nothing in common: the first");
    }
}
