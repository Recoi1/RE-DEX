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
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use xr_net::x::puppet::Puppets;

/// hooks.txt's lines, in order (`hooks_txt_matches` checks the file).
pub const HOOKED: [u32; 4] = [TICK, SAVE, CREATE, LOAD];
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
/// The game function a puppet call is in right now (0: none): the main loop logs it when TICKS stops moving.
pub static IN_CALL: AtomicU32 = AtomicU32::new(0);

/// The table for `XrRuntime.hooks`, in hooks.txt order.
pub fn table() -> *mut XrFn {
    let t: [XrFn; HOOKED.len()] = HOOKED.map(hook_for);
    Box::leak(Box::new(t)).as_mut_ptr()
}

/// The hook of a `HOOKED` address, for an interpreted call to it; None for any other.
pub fn hook_for(pc: u32) -> XrFn {
    match pc {
        TICK => Some(tick),
        SAVE => Some(save),
        CREATE => Some(create),
        LOAD => Some(load),
        _ => None,
    }
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
                .map(|l| u32::from_str_radix(l.trim_start_matches("0x"), 16).unwrap())
                .collect();
            assert_eq!(listed, super::HOOKED, "{path}");
        }
    }
}
