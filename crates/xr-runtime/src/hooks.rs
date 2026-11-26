//! Hooked guest functions (`functions/<build>/hooks.txt`; generated code calls `xr_rt->hooks[i]` in place of the
//! function on hooks.txt's line i) and guest calls made from them.
//!
//! A hook runs on the guest thread that made the call, holding the baton, between two guest instructions the game
//! itself put a call at: there the game's own functions can be called the way its code calls them. The puppets
//! (xr-net `x::puppet`) live here: the main loop says who should be shown ([`PUPPETS`]), the per-frame hook shows them,
//! and the save hook sweeps them first.
//!
//! Hooks fire from code.bin's call sites, native or interpreted (`hook_for`: after a save state loads, the main loop's
//! frame goes on interpreted). Every call from a CRO module (through its import stubs: DllField, DllUSR13 and others
//! import Create and DeleteModel) reaches the function itself and skips the hook.
use crate::abi::{XrCpu, XrFn};
use crate::exec::exec;
use crate::{interp, log};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use xr_net::x::puppet::Puppets;

/// hooks.txt's lines, in order (`hooks_txt_matches` checks the file). New hooks go at the end: a DLL built with the
/// list's first k (k >= REQUIRED) still runs with this redex (game.rs), it just never calls the hooks after them.
pub const HOOKED: [u32; 21] = [TICK, SAVE, CREATE, LOAD, NAME_LOOKUP, SCENE_ANIMS, MOTION, ENV_DRIFT, RAIL_MOVE, ANIM_SET, LAYOUT_ANIMS, REQUESTS, BATTLE_PART, TASKS, SEQUENCE, FACE, BATTLE_UI, BATTLE_CLIENT, BATTLE_MAIN, BATTLE_PROC, INPUT];
/// The hooks every DLL with hooks must have (the multiplayer ones, TICK to LOAD); the ones after are speed-ups.
pub const REQUIRED: usize = 4;
/// code.bin 0x15F860: the main loop (code.bin 0x1066D8) calls it once per pass, outside every proc's update.
pub const TICK: u32 = 0x0015_F860;
/// code.bin 0x433B20: the field save; it writes FieldMoveModelSave (StoreAll 0x4AAE64, its only caller).
pub const SAVE: u32 = 0x0043_3B20;

/// code.bin 0x3D41D8: move-model Create, from code.bin's call sites (the zone NPC loader 0x3D6DF0 among them). A full
/// pool hangs it, so a puppet makes room first.
pub const CREATE: u32 = xr_net::x::puppet::CREATE;
/// code.bin 0x3D6DF0: the zone NPC loader (mgr, entries, count, zone, extras), from code.bin 0x3B8B88 (the map
/// change). Puppets wait for it, so they allocate after the zone's own NPCs.
pub const LOAD: u32 = 0x003D_6DF0;
/// code.bin 0x4CE1B4 (X and Y alike): a name's hash looked up in a table by a linear search that divides for the
/// table's length on every step (code.bin 0x3006F8). Amie spends most of its CPU in the two; see `name_lookup`.
pub const NAME_LOOKUP: u32 = 0x004C_E1B4;
/// code.bin 0x2ECD20 (X and Y alike): a scene node's animations go on a step each update (r1 the node: its +0xDC list
/// steps by +0x25C into +0x254, its +0xCC list by +0x16C into +0x164, then its children, +0xBC, through 0x2D2B5C).
/// With the Frame rate over 30 each step is cut to 1/S for the call (rate.rs).
pub const SCENE_ANIMS: u32 = 0x002E_CD20;
/// code.bin 0x39664C (X and Y alike): a model's motion goes on: frame (+0x18) + speed (+0x24), the speed set again
/// each update by the game; `virtual`, so the CROs' calls (DllBattle's trainers) reach the hook too. With the Frame rate over 30 the speed is cut to 1/S for the call.
pub const MOTION: u32 = 0x0039_664C;
/// code.bin 0x45B734 (X and Y alike): the environment's 8 slow drifts (r0 the table: each 0x24 bytes, an
/// angle at +0x18 and a second value at +0x1C both going on by the speed at +0x20 each update). With the Frame rate
/// over 30 the speeds are cut to 1/S for the call.
pub const ENV_DRIFT: u32 = 0x0045_B734;
/// code.bin 0x3D0458 (X and Y alike): a move model goes a step along its rail (r0 the move model, s0 and s1 the step's
/// two parts), Lumiose's rail walks (DllField's rail actions call it once an update, through their import stub). With
/// the Frame rate over 30 in the field the step is cut to 1/S.
pub const RAIL_MOVE: u32 = 0x003D_0458;
/// code.bin 0x369F08 (X and Y alike): an animation set's animations go on (r0 the set: u32 count at +4, the list at
/// +8; each animation's f32 frame +0x28 goes on by its speed +0x30, wrapped or held in +0x34..+0x38), called through
/// its class's vtable (the battle's camera and stage animations among them). With the Frame rate over 30 each speed
/// is cut to 1/S for the call.
pub const ANIM_SET: u32 = 0x0036_9F08;
/// code.bin 0x14DEC4 (X and Y alike): a layout's animations go on (r0 the layout work: u32 count at +0xAC, entries of
/// 0x2C bytes at [+0xA8], each with flags at +0..+3, the animation at [+8] (frame at its +0x10) and its f32 speed at
/// +0x1C), the menus' and the battle's screens. With the Frame rate over 30 each speed is cut to 1/S for the call.
pub const LAYOUT_ANIMS: u32 = 0x0014_DEC4;
/// code.bin 0x111F3C: the main loop's request queue hands one queued request on each pass (r0 the queue: count at
/// +0xC; DllBattle queues its scene's requests there and waits for the queue to empty). In a battle with the Frame
/// rate over 30 it runs one pass in S, so the queue empties when it would at 30.
pub const REQUESTS: u32 = 0x0011_1F3C;
/// code.bin 0x37E398: called through DllBattle's import stub by, among others, the battle's trainer object each
/// update (DllBattle `BATTLE_TRAINER_SITE`, r4 the object), before it steps its own frame (+0x1A4) by its speed
/// (+0x1AC). In a battle with the Frame rate over 30 that speed is cut to 1/S (rate.rs `cut_kept`).
pub const BATTLE_PART: u32 = 0x0037_E398;
/// code.bin 0x38FEC0: a task list runs its tasks (r0 the list; each task's function at +0x14 called with the list and
/// the task). DllBattle's scene runs its sequence, its waits and countdowns this way (DllBattle 0x721104, 0x71BD58,
/// ...). In a battle with the Frame rate over 30 a list runs one pass in S.
pub const TASKS: u32 = 0x0038_FEC0;
/// code.bin 0x3AD190: a timeline goes on a frame (+0x14 the frame, +0x18 the last; the commands due run first), the
/// battle scene's sequences (DllBattle 0x6E9EF4, once an update). In a battle over 30 it runs one pass in S.
pub const SEQUENCE: u32 = 0x003A_D190;
/// code.bin 0x441BC0: a battle Pokemon's face (blinks: +0x330 counts down 2 an update, its +0x3ED set). In a battle
/// over 30 it runs one pass in S.
pub const FACE: u32 = 0x0044_1BC0;
/// A CRO module's function in hooks.txt (`Module+0xOFF virtual`, OFF from the module's code segment): this bit and OFF.
pub const MODULE_HOOK: u32 = 0x8000_0000;
/// DllBattle+0x10C08 (X v1.0 at 0x6B5D88): the battle's command screens' steps (their waits count updates), called
/// through a pointer by the battle's main update (DllBattle 0x6BB024). In a battle over 30 it runs one pass in S.
pub const BATTLE_UI: u32 = MODULE_HOOK | 0x10C08;
/// DllBattle+0x44E8 (X v1.0 at 0x6A9668): the battle client's update (the battle's logic, its menus and their
/// layouts), called through a pointer by the battle's main update. In a battle over 30 it runs one pass in S, at the
/// game's own steps (the animation hooks do not cut inside it).
pub const BATTLE_CLIENT: u32 = MODULE_HOOK | 0x44E8;
/// DllBattle+0x15EA4 (X v1.0 at 0x6BB024): the battle's main update. It calls the state function of its state machine
/// (a member function pointer at [+0x820]+0, +4) and of its four clients ([+0x828..+0x834] +0x30, +0x34), each a step
/// of the battle's flow that counts updates (waits, the move's phases), then the scene. In a battle over 30 the state
/// functions run one pass in S: on the other passes they are swapped for one that returns 0 ("not done yet").
pub const BATTLE_MAIN: u32 = MODULE_HOOK | 0x15EA4;
/// DllBattle+0x32F54 (X v1.0 at 0x6D80D4): the battle's proc, called each pass by the game's proc runner (code.bin
/// 0x393D84): its phase at +0x40 (0 start, 1 the battle, 2 the end: the battle's teardown in steps that count
/// updates). In a battle over 30 the end runs one pass in S (0, "not done yet", on the other passes).
pub const BATTLE_PROC: u32 = MODULE_HOOK | 0x32F54;
/// code.bin 0x3570C0 (X and Y alike): the input devices' update (buttons, d-pad, sticks, touch: each reads the hardware
/// and works out what was pressed since its last update), called by the main loop through its vtable (code.bin
/// 0x106710) before TICK. In a battle over 30 it runs on the passes the battle's steps run (one in S), so a press is
/// seen on a pass that reads it: run every pass, a press would show on one pass only, mostly one the battle skips.
pub const INPUT: u32 = 0x0035_70C0;
/// code.bin 0x3006F8: the ARM run-time's signed division with remainder (r0 / r1 -> r0, r1).
const DIVIDE: u32 = 0x0030_06F8;

/// The puppets, when online (or with XR_FAKE_PEERS). The main loop sets what to show with `try_lock`: a hook can hold
/// the lock while a game call it made hands the baton to the main loop at a frame's end.
pub static PUPPETS: Mutex<Option<Puppets>> = Mutex::new(None);
/// The puppets without waiting: None only while someone else holds them. A poisoned lock (a panic while held) is
/// taken anyway, once logged: the puppets' state is checked against the pool every tick, and losing the hooks for
/// good would leave puppets in the pool unswept.
pub fn try_puppets() -> Option<MutexGuard<'static, Option<Puppets>>> {
    match PUPPETS.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(e)) => {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| log::log("puppets: the lock was poisoned by a panic; going on with its state"));
            Some(e.into_inner())
        }
    }
}

/// Passes of the per-frame hook: 0 after many frames means xgame.dll was built without the hooks.
pub static TICKS: AtomicU64 = AtomicU64::new(0);
/// Cycles of refreshes not yet counted as a puppet tick (see `tick`).
static PUPPET_CYCLES: AtomicU64 = AtomicU64::new(0);
/// The game function a puppet call is in right now (0: none): the main loop logs it when TICKS stops moving.
pub static IN_CALL: AtomicU32 = AtomicU32::new(0);

/// The table for `XrRuntime.hooks`, in hooks.txt order (made at boot, before the executor; only generated code calls it).
pub fn table() -> *mut XrFn {
    let t: [XrFn; HOOKED.len()] = HOOKED.map(hook_of);
    Box::leak(Box::new(t)).as_mut_ptr()
}

fn hook_of(pc: u32) -> XrFn {
    match pc {
        TICK => Some(tick),
        SAVE => Some(save),
        CREATE => Some(create),
        LOAD => Some(load),
        NAME_LOOKUP => Some(name_lookup),
        SCENE_ANIMS => Some(scene_anims),
        MOTION => Some(motion),
        ENV_DRIFT => Some(env_drift),
        RAIL_MOVE => Some(rail_move),
        ANIM_SET => Some(anim_set),
        LAYOUT_ANIMS => Some(layout_anims),
        REQUESTS => Some(requests),
        BATTLE_PART => Some(battle_part),
        TASKS => Some(tasks),
        SEQUENCE => Some(sequence),
        FACE => Some(face),
        BATTLE_UI => Some(battle_ui),
        BATTLE_CLIENT => Some(battle_client),
        BATTLE_MAIN => Some(battle_main),
        BATTLE_PROC => Some(battle_proc),
        INPUT => Some(input),
        _ => None,
    }
}

/// The hook of a `HOOKED` address, for an interpreted call to it; None for any other. The name lookup's is a stand-in
/// for its generated code only: where that isn't there (XR_INTERP_ALL) the interpreter runs the function as it is.
pub fn hook_for(pc: u32) -> XrFn {
    static GENERATED: OnceLock<XrFn> = OnceLock::new();
    match pc {
        NAME_LOOKUP => generated(NAME_LOOKUP, &GENERATED).and(hook_of(pc)),
        // DllBattle's hooked functions, by their offset in its code
        _ if pc >= 0x0060_0000 && pc < 0x0100_0000 => match battle_code() {
            Some(code) if pc >= code && pc - code < MODULE_HOOK && HOOKED.contains(&(MODULE_HOOK | (pc - code))) => hook_of(MODULE_HOOK | (pc - code)),
            _ => hook_of(pc),
        },
        _ => hook_of(pc),
    }
}

/// DllBattle's code segment while it is loaded.
fn battle_code() -> Option<u32> {
    let st = unsafe { exec().st() };
    st.sys.cro.modules.iter().rev().find(|m| m.name == "DllBattle").and_then(|m| m.segments.first().copied())
}

/// The hooked call's frame (its return address and sp), taken at the hook's entry.
#[derive(Clone, Copy)]
struct Entry {
    ret_lr: u32,
    ret_sp: u32,
    sp: u32,
}

unsafe fn entry(c: *mut XrCpu) -> Entry {
    unsafe { Entry { ret_lr: (*c).ret_lr, ret_sp: (*c).ret_sp, sp: (*c).r[13] } }
}

/// A hook's body, or a guest call from one, on the host thread `c` runs: its Rust code after a guest call lives only
/// on this host stack, so a save state taken meanwhile (a frame can end inside the call) is refused (exec.rs).
struct InHook(usize);

impl InHook {
    unsafe fn new(c: *const XrCpu) -> InHook {
        let me = unsafe { (*c).thread } as usize;
        crate::exec::hook_depth(me, true);
        InHook(me)
    }
}

impl Drop for InHook {
    fn drop(&mut self) {
        crate::exec::hook_depth(self.0, false);
    }
}

/// Runs the hooked function itself, as the call that reached the hook would have.
unsafe fn original(c: *mut XrCpu, f: u32, e: Entry) {
    unsafe { interp::interp_entry(c, f, e.ret_lr, e.ret_sp) };
}

/// Runs a `virtual` hook's function itself: `xr_lookup` hands out the hook for its address (hooks.txt), so not through
/// it; interpreted when XR_INTERP_ALL is on or the DLL has no such function.
unsafe fn original_native(c: *mut XrCpu, f: u32, e: Entry) {
    static INTERP_ALL: OnceLock<bool> = OnceLock::new();
    let all = *INTERP_ALL.get_or_init(|| std::env::var_os("XR_INTERP_ALL").is_some());
    match exec().lookup_native(f).filter(|_| !all) {
        Some(native) => unsafe { crate::bridge::enter_native(c, native, e.ret_lr, e.ret_sp) },
        None => unsafe { original(c, f, e) },
    }
}

/// The name of the guest thread running now (for the log).
fn thread_name() -> String {
    let k = &unsafe { exec().st() }.sys.kernel;
    k.current_thread.map_or("?".into(), |t| k.thread(t).name.clone())
}

fn drain(p: &mut Puppets) {
    for e in p.events.drain(..) {
        log::log(&format!("puppets: {e}"));
    }
}

/// DllField's code and data segments (CRO segments 0 and 2), None while DllField is not loaded.
pub fn dll_of(s: &xr_hle::System) -> Option<xr_net::x::puppet::Dll> {
    let m = s.cro.modules.iter().rev().find(|m| m.name == "DllField")?;
    Some(xr_net::x::puppet::Dll { code: *m.segments.first()?, data: *m.segments.get(2)? })
}

fn dll_field() -> Option<xr_net::x::puppet::Dll> {
    dll_of(&unsafe { exec().st() }.sys)
}

unsafe extern "C" fn tick(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    unsafe { original(c, TICK, e) };
    let rate = crate::rate::rate(&unsafe { exec().st() }.sys);
    let battle = unsafe { exec().st() }.sys.cro.modules.iter().any(|m| m.name == "DllBattle");
    let cycles = crate::rate::tick(&mut unsafe { GuestCalls::new(c, e) }, dll_field(), battle, rate);
    unsafe { exec().st() }.sys.frame_cycles = cycles;
    if TICKS.fetch_add(1, Ordering::Relaxed) == 0 {
        log::log(&format!("puppets: the per-frame hook runs on guest thread '{}'", thread_name()));
    }
    if TICKS.load(Ordering::Relaxed) % 600 == 300 {
        if let (Ok(spec), Some(dll)) = (std::env::var("XR_RAILPOS"), dll_field()) {
            rail_positions(&mut unsafe { GuestCalls::new(c, e) }, dll, &spec);
        }
    }
    // --character over an intro of the other gender: that gender's starting clothes, filled by the game's own code
    if let Some((block, girl)) = crate::character::take_clothes() {
        use crate::character::{CLOTHES_LEN, FILL_CLOTHES};
        use xr_net::x::puppet::Calls;
        use xr_net::Guest;
        let mut g = unsafe { GuestCalls::new(c, e) };
        // the filler only sets bits: what the intro's gender's fill set comes off, the character's go on, and
        // anything set before the intro stays
        let mut fill = |gender: u32| -> [u8; CLOTHES_LEN] {
            let mut out = [0; CLOTHES_LEN];
            g.write(block, &[0; CLOTHES_LEN]);
            g.call(FILL_CLOTHES, &[block, gender]);
            g.read(block, &mut out);
            out
        };
        let mut now = [0; CLOTHES_LEN];
        unsafe { GuestCalls::new(c, e) }.read(block, &mut now);
        let (intro, theirs) = (fill(girl ^ 1), fill(girl));
        let merged: Vec<u8> = (0..CLOTHES_LEN).map(|i| now[i] & !intro[i] | theirs[i]).collect();
        unsafe { GuestCalls::new(c, e) }.write(block, &merged);
        log::log(&format!("character: starting clothes filled at 0x{block:08X} (girl {girl})"));
    }
    // --character on a new game: the intro's questions answered with the character
    {
        let s = &mut unsafe { exec().st() }.sys;
        let intro = s.cro.modules.iter().rev().find(|m| m.name == "DllIntro").and_then(|m| Some((*m.segments.first()?, *m.segments.get(1)?)));
        let heap = |rodata: u32| move || crate::intro::find_work(&mut unsafe { exec().st() }.sys.memory, rodata);
        crate::intro::tick(&mut unsafe { GuestCalls::new(c, e) }, intro, heap(intro.map_or(0, |i| i.1)));
    }
    // `redex --preview`: the creator's look, changed in place
    if crate::preview::ON.load(Ordering::Relaxed) {
        crate::preview::tick(&mut unsafe { GuestCalls::new(c, e) }, dll_field());
    }
    // puppets keep a console frame's pace (their speeds and waits are per 60 Hz tick): over 60 the refreshes are
    // shorter, and only the ones that finish a console frame's worth of cycles tick them
    // shortcut: puppets move 60 times a second at a Frame rate of 120 or 240; per-tick speeds scaled by the
    // refresh's cycles would make them as smooth as the rest
    let due = PUPPET_CYCLES.fetch_add(cycles, Ordering::Relaxed) + cycles;
    if due < xr_hle::CYCLES_PER_FRAME {
        return;
    }
    PUPPET_CYCLES.store(due - xr_hle::CYCLES_PER_FRAME, Ordering::Relaxed);
    // the main loop never holds the lock across a frame; a failed try is a tick missed, not a wait
    let Some(mut lock) = try_puppets() else { return };
    if let Some(p) = lock.as_mut() {
        p.tick(&mut unsafe { GuestCalls::new(c, e) }, dll_field());
        drain(p);
    }
}

/// XR_RAILPOS=ANCHOR:A:B:W:H;... (the anchor in hex, the rest in tiles: a rail-placed warp's +0x08..+0x13): logs the
/// world position the game's own code gives each such warp, every 600 ticks while DllField is loaded (a debug aid for
/// Lumiose's rails, docs/re/x-field-move-models.md). The warp hit test (code.bin 0x3AEC4C, from DllField+0x1E5C8)
/// does the same: code.bin 0x3AF2C8(warp, location) makes a rail location from the entry, then DllField's functions
/// behind code.bin's stubs 0x19AA44 (a converter on the rail system: [[[controller+0x34]+0xFC]+0x7C] and
/// [[controller+0x34]+0xFC]), 0x19AA4C (converter, location, 1) and 0x19AA54 (converter, x,y,z out) give the position.
fn rail_positions(g: &mut GuestCalls, dll: xr_net::x::puppet::Dll, spec: &str) {
    use xr_net::Guest;
    use xr_net::x::field::{SNAPSHOT, SNAP_CONTROLLER};
    use xr_net::x::puppet::Calls;
    let ctx = (|| {
        let snap = dll.data + SNAPSHOT; // the snapshot sits in DllField's data segment
        let ctrl = g.u32(snap.checked_add(SNAP_CONTROLLER)?)?;
        let ctx = g.u32(ctrl.checked_add(0x34)?)?;
        let x = g.u32(ctx.checked_add(0xFC)?)?;
        Some((g.u32(x.checked_add(0x7C)?)?, x))
    })();
    let Some((rail, x)) = ctx.filter(|&(r, x)| r != 0 && x != 0) else { return };
    let s = g.scratch(); // warp entry at +0, location +0x20, converter +0x50, x,y,z +0xA0 (all inside the 0x100 gap)
    for w in spec.split(';').filter(|w| !w.trim().is_empty()) {
        let f: Vec<&str> = w.trim().split(':').collect();
        let (Some(anchor), Some(v)) = (f.first().and_then(|a| u32::from_str_radix(a, 16).ok()), f[1..].iter().map(|v| v.parse::<i16>().ok()).collect::<Option<Vec<_>>>()) else { continue };
        if v.len() != 4 {
            continue;
        }
        let mut entry = [0u8; 0x18];
        entry[6] = 1; // placement: rail
        entry[8..12].copy_from_slice(&anchor.to_le_bytes());
        for (i, n) in v.iter().enumerate() {
            entry[12 + 2 * i..14 + 2 * i].copy_from_slice(&n.to_le_bytes());
        }
        g.write(s, &entry);
        g.write(s + 0xA0, &[0; 12]);
        g.call(0x003A_F2C8, &[s, s + 0x20]);
        g.call(0x0019_AA44, &[s + 0x50, rail, x]);
        g.call(0x0019_AA4C, &[s + 0x50, s + 0x20, 1]);
        g.call(0x0019_AA54, &[s + 0x50, s + 0xA0]);
        let mut p = [0u8; 12];
        g.read(s + 0xA0, &mut p);
        let f = |i: usize| f32::from_le_bytes(p[i..i + 4].try_into().unwrap());
        log::log(&format!("railpos {w}: {:.1},{:.1},{:.1} (tile {},{})", f(0), f(4), f(8), (f(0) / 18.0).floor(), (f(8) / 18.0).floor()));
    }
}

unsafe extern "C" fn save(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    {
        let fieldctx = unsafe { (*c).r[0] };
        log::log(&format!("puppets: the save (code.bin 0x433B20, returning to 0x{:08X}) runs on guest thread '{}'", e.ret_lr, thread_name()));
        // no wait: if the lock were held, the puppets' NO_SAVE bit still keeps them out of the save
        let mut lock = try_puppets();
        if let (Some(p), Some(dll)) = (lock.as_mut().and_then(|l| l.as_mut()), dll_field()) {
            let g = &mut unsafe { GuestCalls::new(c, e) };
            // XR_PUPPET_SAVECHECK=1 (tests): what StoreAll writes with the puppets in the pool, to compare below
            let check = std::env::var_os("XR_PUPPET_SAVECHECK").is_some() && !p.shown().is_empty();
            let with = check.then(|| store_all(g, fieldctx)).flatten();
            let n = p.shown().len();
            p.sweep(g, dll);
            drain(p);
            if let (Some(with), Some(without)) = (with, check.then(|| store_all(g, fieldctx)).flatten()) {
                let same = with == without;
                log::log(&format!(
                    "puppets: save check: FieldMoveModelSave with {n} puppets in the pool {} the one without them (0x{:X} bytes)",
                    if same { "equals" } else { "DIFFERS FROM" },
                    with.len()
                ));
            }
        }
    }
    unsafe { original(c, SAVE, e) };
}

unsafe extern "C" fn create(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    let mgr = unsafe { (*c).r[0] };
    // the game adds an NPC: never into a full pool because of a puppet (the tick holds the lock only on this thread,
    // and never while the game itself creates)
    if let Some(mut lock) = try_puppets() {
        if let (Some(p), Some(dll)) = (lock.as_mut(), dll_field()) {
            p.make_room(&mut unsafe { GuestCalls::new(c, e) }, dll, mgr);
            drain(p);
        }
    }
    unsafe { original(c, CREATE, e) };
}

/// What `NAME_LOOKUP` does with a call, worked out without writing anything (see `name_lookup`).
#[derive(Debug, PartialEq)]
struct Lookup {
    /// the hash matched an entry's (r0 0, the value stored), or not (r0 0x13, 0 stored)
    found: bool,
    /// the value of the last entry read, which goes to [sp-56] (None: no entry was read)
    slot: Option<u32>,
    /// back-edges the loops took
    back_edges: i32,
    /// the last DIVIDE call's r0, r1 and r2 going in, and if the search made it (lr 0x4CE22C) or the first (0x4CE204)
    last: (u32, u32, u32, bool),
}

/// DIVIDE's short paths: both operands under 2^31 and a quotient under 2^16 (b == 0 is excluded), which take no
/// back-edge. Gives the quotient and the r2 DIVIDE leaves: its own (q >> 1) unless the quotient is decided at once (a
/// half under b), when r2 stays as it came in. None for the long paths.
fn short_divide(a: u32, b: u32, r2: u32) -> Option<(u32, u32)> {
    if (a | b) >> 31 != 0 || a >> 16 >= b {
        return None;
    }
    let q = a / b;
    Some((q, if a >> 1 >= b { q >> 1 } else { r2 }))
}

/// `NAME_LOOKUP(obj = r0, name = r1, out = r2)` as the generated code runs it, reading guest memory through `read`
/// (address, size 1/2/4: the value zero-extended, None to give up), from registers `r` and the back-edge budget.
/// None when the call must go to the generated function: the budget would run out inside it (a preemption point the
/// hook cannot place), a division takes DIVIDE's long path, or `read` gave up.
fn plan_lookup(r: &[u32; 16], budget: i32, read: &mut impl FnMut(u32, u32) -> Option<u32>) -> Option<Lookup> {
    let back_edge = |n: &mut i32| {
        *n += 1;
        (*n < budget).then_some(())
    };
    let half = |v: u32| v as u16 as i16 as u32;
    let (obj, mut p, mut n) = (r[0], r[1], 0);
    // r7 (the hash compared) is never compared before an entry's is read into it, see below
    // 0x4CE1C4: the name's hash, h = c ^ h * 131 over its signed chars
    let mut h = 0u32;
    if read(p, 1)? != 0 {
        loop {
            let ch = read(p, 1)? as u8 as i8 as u32;
            let next = read(p.wrapping_add(1), 1)?;
            p = p.wrapping_add(1);
            h = ch ^ h.wrapping_mul(131);
            if next == 0 {
                break;
            }
            back_edge(&mut n)?;
        }
    }
    // 0x4CE1EC: the table's length, (end - begin) / entry size
    let o = read(obj, 4)?;
    let (end, begin, size) = (read(o.wrapping_add(0x30), 4)?, read(o.wrapping_add(0x2C), 4)?, half(read(o.wrapping_add(0xA), 2)?));
    let a = end.wrapping_sub(begin);
    let (count, r2) = short_divide(a, size, end)?;
    let (mut slot, mut found) = (None, false);
    // 0x4CE210, a step per entry: the length again (the same reads and division: nothing writes guest memory
    // meanwhile, so it is count again, with r2 as the first division left it), then entry i (value, hash); the hash
    // compared is r7's until an entry is read, and every step reads one (i < count)
    if count as i32 > 0 {
        for i in 0..count {
            let at = i.wrapping_mul(size).wrapping_add(begin).wrapping_add(o);
            let hash = read(at.wrapping_add(4), 4)?;
            slot = Some(read(at, 4)?);
            if h == hash {
                found = true;
                break;
            }
            if i + 1 == count {
                break;
            }
            back_edge(&mut n)?;
        }
    }
    // the last division: a step's (r2 going in as the one before left it), or with no step the first (r2 = end)
    let last = if count as i32 > 0 { (a, size, r2, true) } else { (a, size, end, false) };
    Some(Lookup { found, slot, back_edges: n, last })
}

/// The generated function at a code.bin address (None when everything is interpreted, XR_INTERP_ALL), looked up once.
fn generated(addr: u32, slot: &'static OnceLock<XrFn>) -> XrFn {
    *slot.get_or_init(|| exec().lookup(addr))
}

/// `NAME_LOOKUP`, natively: every register, flag and byte of guest memory as the generated code leaves them, and the
/// same back-edge budget paid, so the guest clock and every hash stay the same. `plan_lookup` works the call out
/// first without writing anything; the call goes to the generated function itself when it gives up, when a read
/// would touch the call's own stack frame (the function reads nothing there but what it wrote), when `out` lies in
/// that frame, or when lr is not the call's return address. Then, in the function's order: the last DIVIDE call runs
/// for real (the r2, r3 and flags the function leaves are its; the others' results are dead but for r2), the push, the
/// last entry's value at [sp-56], the result at `out`.
unsafe extern "C" fn name_lookup(c: *mut XrCpu) {
    static ORIGINAL: OnceLock<XrFn> = OnceLock::new();
    static DIV: OnceLock<XrFn> = OnceLock::new();
    static BASE: OnceLock<usize> = OnceLock::new();
    static LOGGED: AtomicBool = AtomicBool::new(false);
    let (Some(original_fn), Some(divide)) = (generated(NAME_LOOKUP, &ORIGINAL), generated(DIVIDE, &DIV)) else {
        let _hook = unsafe { InHook::new(c) };
        let e = unsafe { entry(c) };
        return unsafe { original(c, NAME_LOOKUP, e) };
    };
    let base = *BASE.get_or_init(|| unsafe { exec().st() }.sys.memory.base() as usize) as *mut u8;
    let (r, budget, ret_lr) = unsafe { ((*c).r, (*c).budget, (*c).ret_lr) };
    let (sp, out) = (r[13], r[2]);
    // the frame: the 12 registers pushed and the 8 bytes below them
    let (lo, hi) = ((sp as u64).saturating_sub(56), sp as u64);
    let in_frame = |a: u32, size: u32| (a as u64) < hi && a as u64 + size as u64 > lo;
    let mut read = |a: u32, size: u32| -> Option<u32> {
        if in_frame(a, size) || a as u64 + size as u64 > 1 << 32 {
            return None;
        }
        // safe bc guest memory is 4 GiB reserved at base; a page that faults does so as in the generated code
        let p = unsafe { base.add(a as usize) };
        Some(unsafe {
            match size {
                1 => p.read() as u32,
                2 => (p as *const u16).read_unaligned() as u32,
                _ => (p as *const u32).read_unaligned(),
            }
        })
    };
    let plan = if sp < 56 || in_frame(out, 4) || r[14] != ret_lr { None } else { plan_lookup(&r, budget, &mut read) };
    let Some(plan) = plan else {
        return unsafe { original_fn(c) };
    };
    let (a, b, r2, looped) = plan.last;
    let ret = if looped { 0x004C_E22C } else { 0x004C_E204 };
    let frame = sp - 56;
    unsafe {
        (*c).r[0] = a;
        (*c).r[1] = b;
        (*c).r[2] = r2;
        (*c).r[13] = frame;
        (*c).r[14] = ret;
        (*c).ret_lr = ret;
        (*c).ret_sp = frame;
        divide(c);
    }
    // the short paths pay no back-edge and give a / b; anything else means the model is wrong, and the call can't be
    // undone (the budget is spent, a preemption may have run another thread): stop rather than go on off the clock
    let (q, left) = unsafe { ((*c).r[0], (*c).budget) };
    if q != a / b || left != budget {
        log::stop(
            1,
            &format!("name lookup hook: the division helper gave {q} for {a} / {b} with budget {budget} -> {left}; the hook's model of it is wrong"),
        );
    }
    if !LOGGED.load(Ordering::Relaxed) && !LOGGED.swap(true, Ordering::Relaxed) {
        log::log("name lookup: the native hook runs (code.bin 0x4CE1B4)");
    }
    let write = |a: u32, v: u32| unsafe { (base.add(a as usize) as *mut u32).write_unaligned(v) };
    let pushed = [r[0], r[1], r[2], r[4], r[5], r[6], r[7], r[8], r[9], r[10], r[11], r[14]];
    for (k, v) in pushed.into_iter().enumerate() {
        write(sp - 48 + 4 * k as u32, v);
    }
    if let Some(v) = plan.slot {
        write(frame, v);
    }
    let (r0, r1) = if plan.found {
        // safe as the reads above: the frame is the guest's stack
        write(out, unsafe { (base.add(frame as usize) as *const u32).read_unaligned() });
        (0, out)
    } else {
        write(out, 0);
        (0x13, 0)
    };
    unsafe {
        (*c).r[0] = r0;
        (*c).r[1] = r1;
        (&mut (*c).r)[4..12].copy_from_slice(&r[4..12]);
        (*c).r[13] = sp;
        (*c).budget = budget - plan.back_edges;
    }
}

/// The scene node's animation steps cut to 1/S while the hooked call runs, then put back (see SCENE_ANIMS).
unsafe extern "C" fn scene_anims(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    static SEEN: std::sync::Once = std::sync::Once::new();
    SEEN.call_once(|| log::log("frame rate: the scene animation hook runs (code.bin 0x2ECD20)"));
    let s = crate::rate::SCALE.load(Ordering::Relaxed);
    if s <= 1 || !crate::rate::hook_on(1) {
        return unsafe { original_native(c, SCENE_ANIMS, e) };
    }
    let node = unsafe { (*c).r[1] };
    let mut g = unsafe { GuestCalls::new(c, e) };
    let saved = crate::rate::cut_steps(&mut g, node, s);
    unsafe { original_native(c, SCENE_ANIMS, e) };
    crate::rate::put_back(&mut unsafe { GuestCalls::new(c, e) }, &saved);
}

/// The motion's speed cut to 1/S while the hooked call runs, then put back (see MOTION).
unsafe extern "C" fn motion(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    if crate::rate::MOTION_CALLS.fetch_add(1, Ordering::Relaxed) == 0 {
        log::log("frame rate: the motion hook runs (code.bin 0x39664C)");
    }
    let s = crate::rate::SCALE.load(Ordering::Relaxed);
    if s <= 1 || !crate::rate::hook_on(2) {
        return unsafe { original_native(c, MOTION, e) };
    }
    // a speed under 0.5 takes the motion's in-between path, which allocates a buffer the first time: in a battle that
    // allocation fails (and the game writes through null). There the speed is cut to 1/2 at most and the motion runs
    // on every (S / 2)th pass only
    let field = crate::rate::FIELD_ON.load(Ordering::Relaxed);
    if !field && crate::rate::PASSES.load(Ordering::Relaxed) % u64::from(s / 2) != 0 {
        return;
    }
    let obj = unsafe { (*c).r[0] };
    let mut g = unsafe { GuestCalls::new(c, e) };
    let saved = crate::rate::cut_motion(&mut g, obj, if field { s } else { 2 });
    if crate::rate::trace() {
        use xr_net::Guest;
        log::log(&format!("rate: motion 0x{obj:08X} lr 0x{:08X} frame {:?} speed {:?} len {:?}", e.ret_lr, g.f32(obj + 0x18), g.f32(obj + 0x24), g.f32(obj + 0x20)));
    }
    unsafe { original_native(c, MOTION, e) };
    crate::rate::put_back(&mut unsafe { GuestCalls::new(c, e) }, &saved);
}

/// The environment's drift speeds cut to 1/S while the hooked call runs, then put back (see ENV_DRIFT).
unsafe extern "C" fn env_drift(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    let s = crate::rate::SCALE.load(Ordering::Relaxed);
    if s <= 1 || !crate::rate::hook_on(4) {
        return unsafe { original(c, ENV_DRIFT, e) };
    }
    let table = unsafe { (*c).r[0] };
    let mut g = unsafe { GuestCalls::new(c, e) };
    let saved = crate::rate::cut_drifts(&mut g, table, s);
    unsafe { original(c, ENV_DRIFT, e) };
    crate::rate::put_back(&mut unsafe { GuestCalls::new(c, e) }, &saved);
}

/// The rail step cut to 1/S while the field runs at S (see RAIL_MOVE).
unsafe extern "C" fn rail_move(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    static SEEN: std::sync::Once = std::sync::Once::new();
    SEEN.call_once(|| log::log("frame rate: the rail step hook runs (code.bin 0x3D0458)"));
    let s = crate::rate::SCALE.load(Ordering::Relaxed);
    if s > 1 && crate::rate::FIELD_ON.load(Ordering::Relaxed) {
        // safe: s0 and s1 are the step's arguments, the callee's to change anyway
        unsafe { (*c).v.s[0] /= s as f32; (*c).v.s[1] /= s as f32 };
    }
    unsafe { original_native(c, RAIL_MOVE, e) };
}

/// The animation set's speeds cut to 1/S while the hooked call runs, then put back (see ANIM_SET).
unsafe extern "C" fn anim_set(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    static SEEN: std::sync::Once = std::sync::Once::new();
    SEEN.call_once(|| log::log("frame rate: the animation set hook runs (code.bin 0x369F08)"));
    let s = crate::rate::SCALE.load(Ordering::Relaxed);
    if s <= 1 || !crate::rate::hook_on(8) {
        return unsafe { original_native(c, ANIM_SET, e) };
    }
    let set = unsafe { (*c).r[0] };
    let mut g = unsafe { GuestCalls::new(c, e) };
    let saved = crate::rate::cut_anim_set(&mut g, set, s);
    unsafe { original_native(c, ANIM_SET, e) };
    crate::rate::put_back(&mut unsafe { GuestCalls::new(c, e) }, &saved);
}

/// The layout's animation speeds cut to 1/S while the hooked call runs, then put back (see LAYOUT_ANIMS).
unsafe extern "C" fn layout_anims(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    static SEEN: std::sync::Once = std::sync::Once::new();
    SEEN.call_once(|| log::log("frame rate: the layout animation hook runs (code.bin 0x14DEC4)"));
    let s = crate::rate::SCALE.load(Ordering::Relaxed);
    if s <= 1 || !crate::rate::hook_on(16) {
        return unsafe { original_native(c, LAYOUT_ANIMS, e) };
    }
    let work = unsafe { (*c).r[0] };
    let mut g = unsafe { GuestCalls::new(c, e) };
    let saved = crate::rate::cut_layout_anims(&mut g, work, s);
    unsafe { original_native(c, LAYOUT_ANIMS, e) };
    crate::rate::put_back(&mut unsafe { GuestCalls::new(c, e) }, &saved);
}

/// The request queue's pass, one pass in S in a battle over 30 (see REQUESTS).
unsafe extern "C" fn requests(c: *mut XrCpu) {
    unsafe { one_in_s(c, REQUESTS) }
}

/// A task list's pass in a battle over 30 (see TASKS): its moves every pass at 1/S, the rest one pass in S.
unsafe extern "C" fn tasks(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    let (Some(s), Some(code)) = (crate::rate::battle_scale(), battle_code()) else { return unsafe { original_native(c, TASKS, e) } };
    let all = crate::rate::PASSES.load(Ordering::Relaxed) % u64::from(s) == 0;
    let list = unsafe { (*c).r[0] };
    let swapped = crate::rate::battle_tasks(&mut unsafe { GuestCalls::new(c, e) }, list, code, s, all);
    unsafe { original_native(c, TASKS, e) };
    crate::rate::put_back_tasks(&mut unsafe { GuestCalls::new(c, e) }, &swapped, code);
}

/// A timeline's frame, one pass in S in a battle over 30 (see SEQUENCE).
unsafe extern "C" fn sequence(c: *mut XrCpu) {
    unsafe { one_in_s(c, SEQUENCE) }
}

/// A battle Pokemon's face, one pass in S in a battle over 30 (see FACE).
unsafe extern "C" fn face(c: *mut XrCpu) {
    unsafe { one_in_s(c, FACE) }
}

/// The battle's command screens, one pass in S in a battle over 30 (see BATTLE_UI).
unsafe extern "C" fn battle_ui(c: *mut XrCpu) {
    unsafe { battle_module(c, BATTLE_UI) }
}

/// The battle client, one pass in S in a battle over 30 (see BATTLE_CLIENT).
unsafe extern "C" fn battle_client(c: *mut XrCpu) {
    unsafe { battle_module(c, BATTLE_CLIENT) }
}

/// The battle's proc (see BATTLE_PROC): its end one pass in S in a battle over 30.
unsafe extern "C" fn battle_proc(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    let Some(code) = battle_code() else { return };
    let f = code + (BATTLE_PROC & !MODULE_HOOK);
    let this = unsafe { (*c).r[0] };
    let phase = { use xr_net::Guest; unsafe { GuestCalls::new(c, e) }.u32(this.wrapping_add(0x40)) };
    if phase != Some(1) {
        crate::rate::BATTLE_READY.store(false, Ordering::Relaxed);
    }
    let skip = phase == Some(2) && crate::rate::battle_scale().is_some_and(|s| crate::rate::PASSES.load(Ordering::Relaxed) % u64::from(s) != 0);
    if skip {
        unsafe { (*c).r[0] = 0 };
        return;
    }
    unsafe { original_native(c, f, e) }
}

/// The battle's main update (see BATTLE_MAIN): its state functions one pass in S in a battle over 30.
unsafe extern "C" fn battle_main(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    let Some(code) = battle_code() else { return };
    let f = code + (BATTLE_MAIN & !MODULE_HOOK);
    {
        // ready once the scene's state machine (DllBattle 0x71A210: state u8 +0x694 of the scene [this+0x14]) is past
        // its loads: its first state (DllBattle 0x6E446C, step u32 +0x6B8) at the intro's step 0x14 or later
        use xr_net::Guest;
        let mut g = unsafe { GuestCalls::new(c, e) };
        let scene = g.u32(unsafe { (*c).r[0] }.wrapping_add(0x14)).filter(|&v| v != 0);
        let ready = scene.is_some_and(|v| g.u8(v.wrapping_add(0x694)) != Some(0) || g.u32(v.wrapping_add(0x6B8)).is_some_and(|n| n >= 0x14));
        crate::rate::BATTLE_READY.store(ready, Ordering::Relaxed);
    }
    let off = crate::rate::battle_scale().filter(|&s| crate::rate::PASSES.load(Ordering::Relaxed) % u64::from(s) != 0);
    if off.is_none() {
        return unsafe { original_native(c, f, e) };
    }
    let this = unsafe { (*c).r[0] };
    let swapped = crate::rate::battle_states_off(&mut unsafe { GuestCalls::new(c, e) }, this, code);
    static SEEN: std::sync::Once = std::sync::Once::new();
    SEEN.call_once(|| log::log(&format!("frame rate: the battle's main update hook runs ({} words off)", swapped.len())));
    unsafe { original_native(c, f, e) };
    crate::rate::put_back_states(&mut unsafe { GuestCalls::new(c, e) }, &swapped);
}

/// A DllBattle function hooked by its offset (`key`): runs on one pass in S in a battle over 30, with the animation
/// hooks' cuts off inside it (it steps at the game's 30).
unsafe fn battle_module(c: *mut XrCpu, key: u32) {
    // only DllBattle reaches these (through xr_lookup while it is placed)
    let Some(code) = battle_code() else { return };
    let f = code + (key & !MODULE_HOOK);
    let bit = if key == BATTLE_UI { 32 } else { 64 };
    let Some(s) = crate::rate::battle_scale().filter(|_| crate::rate::hook_on(bit)) else { return unsafe { original_native(c, f, entry(c)) } };
    if crate::rate::PASSES.load(Ordering::Relaxed) % u64::from(s) != 0 {
        // a state function of the battle's: 0 is "not done yet", what it returns while it waits
        unsafe { (*c).r[0] = 0 };
        return;
    }
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    crate::rate::SCALE.store(1, Ordering::Relaxed);
    unsafe { original_native(c, f, e) };
    crate::rate::SCALE.store(s, Ordering::Relaxed);
}

/// The input devices' update (see INPUT): in a battle over 30 only before a pass the battle runs (TICK counts the pass
/// after this call).
unsafe extern "C" fn input(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    if let Some(s) = crate::rate::battle_scale() {
        if (crate::rate::PASSES.load(Ordering::Relaxed) + 1) % u64::from(s) != 0 {
            return;
        }
    }
    unsafe { original_native(c, INPUT, e) };
}

/// Runs the hooked function `f` (one that returns nothing the caller uses) on one pass in S in a battle over 30.
unsafe fn one_in_s(c: *mut XrCpu, f: u32) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    if let Some(s) = crate::rate::battle_scale() {
        if crate::rate::PASSES.load(Ordering::Relaxed) % u64::from(s) != 0 {
            return;
        }
    }
    unsafe { original_native(c, f, e) };
}

/// DllBattle's own per-update steps that reach code.bin first (see BATTLE_PART): cut to 1/S in a battle over 30.
unsafe extern "C" fn battle_part(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    if let Some(s) = crate::rate::battle_scale() {
        let st = unsafe { exec().st() };
        let code = st.sys.cro.modules.iter().rev().find(|m| m.name == "DllBattle").and_then(|m| m.segments.first().copied());
        if let Some(code) = code {
            static SEEN: std::sync::Once = std::sync::Once::new();
            SEEN.call_once(|| log::log(&format!("frame rate: the battle site hook runs (DllBattle code at 0x{code:08X})")));
            let r4 = unsafe { (*c).r[4] };
            let mut g = unsafe { GuestCalls::new(c, e) };
            crate::rate::battle_site(&mut g, e.ret_lr.wrapping_sub(code), r4, s);
        }
    }
    unsafe { original_native(c, BATTLE_PART, e) };
}

unsafe extern "C" fn load(c: *mut XrCpu) {
    let _hook = unsafe { InHook::new(c) };
    let e = unsafe { entry(c) };
    let zone = unsafe { (*c).r[3] } as u16;
    unsafe { original(c, LOAD, e) };
    if let Some(mut lock) = try_puppets() {
        if let Some(p) = lock.as_mut() {
            p.zone_loaded(zone);
        }
    }
}

/// code.bin 0x4AAE64 StoreAll(manager, fieldctx), as 0x433B20 calls it; the FieldMoveModelSave it fills
/// (`*fieldctx + 0x130C`: 32 entries of 0x108). For the save check only.
fn store_all(g: &mut GuestCalls, fieldctx: u32) -> Option<Vec<u8>> {
    use xr_net::Guest;
    use xr_net::x::puppet::Calls;
    let (mgr, game) = (g.u32(fieldctx.checked_add(0x20)?)?, g.u32(fieldctx)?);
    g.call(0x004A_AE64, &[mgr, fieldctx]);
    let mut b = vec![0u8; 0x2100];
    g.read(game.checked_add(0x130C)?, &mut b).then_some(b)
}

/// Bytes kept below the hook's sp: the scratch area (puppets use 0x90 of it: a header and a dress-up parameter); the
/// callees' frames grow down from its start, so they never reach it.
const SCRATCH_GAP: u32 = 0x100;

/// Game calls from inside a hook, on its guest thread: the callee runs with the scratch area's address as sp (its
/// frames grow below it) and returns to the hook's own return address, which with that sp no frame of the game's can
/// match. Every register, the flags and the VFP state are the hook's again afterwards; the budget is the callee's.
pub struct GuestCalls {
    c: *mut XrCpu,
    scratch: u32,
    ret: u32,
}

impl GuestCalls {
    /// # Safety
    /// `c` is the XrCpu the hook got, `e` its frame at the hook's entry, and the hook still runs.
    unsafe fn new(c: *mut XrCpu, e: Entry) -> GuestCalls {
        GuestCalls { c, scratch: e.sp.wrapping_sub(SCRATCH_GAP) & !7, ret: e.ret_lr }
    }
}

impl xr_net::Guest for GuestCalls {
    fn read(&mut self, addr: u32, out: &mut [u8]) -> bool {
        let m = &mut unsafe { exec().st() }.sys.memory;
        m.slice_mut(addr, out.len() as u32).map(|s| out.copy_from_slice(s)).is_some()
    }
}

impl xr_net::x::puppet::Calls for GuestCalls {
    fn write(&mut self, addr: u32, data: &[u8]) -> bool {
        let m = &mut unsafe { exec().st() }.sys.memory;
        m.slice_mut(addr, data.len() as u32).map(|s| s.copy_from_slice(data)).is_some()
    }

    fn call(&mut self, f: u32, args: &[u32]) -> u32 {
        assert!(args.len() <= 4, "four register arguments at most");
        if std::env::var_os("XR_PUPPET_TRACE").is_some() {
            log::log(&format!("puppets: call 0x{f:08X} {args:08X?} sp 0x{:08X}", self.scratch));
        }
        let _hook = unsafe { InHook::new(self.c) };
        let c = unsafe { &mut *self.c };
        let saved = (c.r, c.cpsr, unsafe { c.v.u }, c.fpscr, c.ret_lr, c.ret_sp);
        // the hook's own return address, from its entry
        let ret = self.ret;
        c.r[..args.len()].copy_from_slice(args);
        (c.r[13], c.r[14]) = (self.scratch, ret);
        IN_CALL.store(f, Ordering::Relaxed);
        unsafe { interp::interp_entry(c, f, ret, self.scratch) };
        IN_CALL.store(0, Ordering::Relaxed);
        let r0 = c.r[0];
        (c.r, c.cpsr, c.v.u, c.fpscr, c.ret_lr, c.ret_sp) = saved;
        r0
    }

    fn scratch(&self) -> u32 {
        self.scratch
    }
}

#[cfg(test)]
mod tests {
    /// The table's order is hooks.txt's (X's and Y's): a hook at the wrong index would replace another function.
    #[test]
    fn hooks_txt_matches() {
        for build in ["x-usa-v0.2.0", "y-usa-v0.2.0"] {
            let path = format!("{}/../../functions/{build}/hooks.txt", env!("CARGO_MANIFEST_DIR"));
            let listed: Vec<u32> = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|l| l.split('#').next().unwrap().trim())
                .filter(|l| !l.is_empty())
                .map(|l| {
                    let w = l.split_whitespace().next().unwrap();
                    let (module, a) = w.split_once('+').map_or((false, w), |(_, off)| (true, off));
                    u32::from_str_radix(a.trim_start_matches("0x"), 16).unwrap() | if module { super::MODULE_HOOK } else { 0 }
                })
                .collect();
            assert_eq!(listed, super::HOOKED, "{path}");
        }
    }

    /// `plan_lookup` on a made-up table: the entry found, the back-edges (the name's chars but one, the search's steps
    /// but one), the last division's operands and the r2 it gets, and where it gives up
    #[test]
    fn name_lookup_plans_like_the_generated_code() {
        use super::{plan_lookup, Lookup};
        use std::collections::HashMap;
        // obj 0x1000 -> table 0x2000: end at +0x30, begin at +0x2C (an offset from the table), entry size at +0xA;
        // entry i (value, hash) at table + begin + i * size; the name at 0x3000
        let table = |entries: &[(u32, u32)], size: u16| {
            let mut m: HashMap<u32, u8> = HashMap::new();
            let mut put = |a: u32, bytes: &[u8]| bytes.iter().enumerate().for_each(|(k, &b)| _ = m.insert(a + k as u32, b));
            put(0x1000, &0x2000u32.to_le_bytes());
            put(0x2000 + 0x2C, &0x100u32.to_le_bytes());
            put(0x2000 + 0x30, &(0x100 + entries.len() as u32 * size as u32).to_le_bytes());
            put(0x2000 + 0xA, &size.to_le_bytes());
            for (i, (value, hash)) in entries.iter().enumerate() {
                let at = 0x2100 + i as u32 * size as u32;
                put(at, &value.to_le_bytes());
                put(at + 4, &hash.to_le_bytes());
            }
            put(0x3000, b"ab\0");
            m
        };
        let plan = |m: &HashMap<u32, u8>, budget| {
            let mut read = |a: u32, size: u32| (0..size).rev().try_fold(0u32, |v, k| Some(v << 8 | *m.get(&(a + k))? as u32));
            let mut r = [0u32; 16];
            (r[0], r[1], r[2], r[7]) = (0x1000, 0x3000, 0x4000, 0xDEAD);
            plan_lookup(&r, budget, &mut read)
        };
        let h = b"ab".iter().fold(0u32, |h, &c| c as i8 as u32 ^ h.wrapping_mul(131));
        let four = table(&[(10, 1), (11, 2), (12, h), (13, 4)], 8);
        // found at 2: 1 + 2 back-edges; 32 / 8 leaves r2 = 4 >> 1 for every later call
        assert_eq!(plan(&four, 1024), Some(Lookup { found: true, slot: Some(12), back_edges: 3, last: (32, 8, 2, true) }));
        // not found: 1 + 3 back-edges, the last entry read
        let missing = table(&[(10, 1), (11, 2), (12, 3), (13, 4)], 8);
        assert_eq!(plan(&missing, 1024), Some(Lookup { found: false, slot: Some(13), back_edges: 4, last: (32, 8, 2, true) }));
        // a budget the call would run out of (a back-edge reaching 0 preempts), and one it wouldn't
        assert_eq!(plan(&missing, 4), None);
        assert!(plan(&missing, 5).is_some());
        // one entry: 8 / 8 is decided at once and leaves r2 as it came in, the first call's being the table's end
        let one = table(&[(10, 1)], 8);
        assert_eq!(plan(&one, 1024), Some(Lookup { found: false, slot: Some(10), back_edges: 1, last: (8, 8, 0x108, true) }));
        // an empty table: no search, the first call last; r7 as it came in is never compared
        let empty = table(&[], 8);
        assert_eq!(plan(&empty, 1024), Some(Lookup { found: false, slot: None, back_edges: 1, last: (0, 8, 0x100, false) }));
        // DIVIDE's long paths go to the generated code: a zero size, a quotient of 2^16
        assert_eq!(plan(&table(&[(10, 1)], 0), 1024), None);
        assert_eq!(super::short_divide(0x10000, 1, 0), None);
        assert_eq!(super::short_divide(0xFFFF, 1, 7), Some((0xFFFF, 0x7FFF)));
        assert_eq!(super::short_divide(5, 6, 7), Some((0, 7)));
        assert_eq!(super::short_divide(0x8000_0000, 0x8000_0000, 7), None, "a negative operand");
    }
}
