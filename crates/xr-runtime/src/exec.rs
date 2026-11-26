//! The executor. Every guest thread is a host thread; a baton makes exactly one of them, or the frontend's thread
//! (the "main loop", `Machine::run_frame`), run at a time, and Zakuro's kernel decides which guest thread is next.
//!
//! Invariant: while the main loop holds the baton, `System.cpu` holds the current guest thread's live registers
//! (guests copy their XrCpu in before handing the baton over), so `System::reschedule` saves the right context.
//! A guest thread that gets the baton back copies `System.cpu` into its XrCpu, which picks up wait results and the
//! cleared exclusive monitor after a switch.
use crate::abi::{XrCpu, XrFn, XrRuntime, XR_ABI_VERSION};
use crate::{bridge, clock, game::Game, interp, log, progress};
use std::cell::UnsafeCell;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use xr_hle::kernel::thread::{ThreadId, ThreadStatus, THREAD_EXIT_MAGIC};
use xr_hle::{Config, FrameOutcome, System, CYCLES_PER_AUDIO_FRAME};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Turn {
    Main,
    Host(usize),
}

pub struct Exec {
    turn: Mutex<Turn>,
    /// the thread that waits for each turn
    waiters: Mutex<std::collections::HashMap<Turn, std::thread::Thread>>,
    state: UnsafeCell<State>,
    lookup: unsafe extern "C" fn(u32) -> XrFn,
    /// `xr_lookup_native` (see game.rs), or `lookup` for a DLL without it
    lookup_native: unsafe extern "C" fn(u32) -> XrFn,
}

// Safety: `state` is only touched by the baton holder, one host thread at a time; the hand-over goes through
// `turn`'s mutex. (System isn't Send because of its renderer trait object; the software renderer has no thread
// affinity. A GL renderer might.)
unsafe impl Sync for Exec {}
unsafe impl Send for Exec {}

/// Everything the baton guards.
pub struct State {
    pub sys: System,
    /// per kernel ThreadId: (Thread.guest_id, host index) of the host thread running it (slots get reused)
    hosts: Vec<Option<(u32, usize)>>,
    /// per host index: the kernel thread it runs
    host_tid: Vec<ThreadId>,
    /// the clock value the frame being run ends at
    pub deadline: u64,
    /// X's field camera from change to change, for frame interpolation's cut hint (see camera_cut)
    camera: xr_net::x::field::CameraCuts,
    /// Responsive movement's predictor (see ahead.rs)
    ahead: crate::ahead::Ahead,
    /// per host index: where it waits for the baton (see Park)
    parked: Vec<Park>,
    /// per host index: the hook bodies (hooks.rs) it is inside of; a save state can't resume Rust code
    in_hook: Vec<u32>,
    /// threads a loaded save state left mid-function, by kernel ThreadId: their back-edge budget; their host threads
    /// start interpreted at the thread's PC (see host_main)
    resume: std::collections::HashMap<ThreadId, i32>,
}

/// Where a host thread waits for the baton (between frames, every live one does), which says whether its guest
/// state is all in `System` for a save state.
#[derive(Clone, Copy)]
pub enum Park {
    /// in an SVC from compiled code: r0-r14 are spilled and PC is the SVC instruction's own address
    Svc,
    /// at a compiled loop's back-edge (xr_rt->preempt): generated code never writes the guest PC there (xr-codegen
    /// `preempt`), so where the thread goes on is only on its host stack
    Preempt,
    /// in the interpreter, with the guest state exact in `System`; the back-edge budget it had left
    Interp(i32),
}

/// The baton holder notes where host thread `me` waits, just before it calls `checkpoint`.
pub fn park(me: usize, how: Park) {
    unsafe { exec().st() }.parked[me] = how;
}

/// The baton holder notes host thread `me` going into (`true`) or out of a hook's body (hooks.rs).
pub fn hook_depth(me: usize, into: bool) {
    let d = &mut unsafe { exec().st() }.in_hook[me];
    *d = if into { *d + 1 } else { d.saturating_sub(1) };
}

/// A save state starts with its magic and the SHA-256 of the xgame.dll it was made with (redex_builder::state_matches
/// reads the same header).
fn state_header(dll: [u8; 32]) -> Vec<u8> {
    [&redex_builder::STATE_MAGIC[..], &dll].concat()
}

/// The state after its header, or why it can't be loaded with the xgame.dll whose fingerprint is `dll`.
fn state_body(bytes: &[u8], dll: [u8; 32]) -> Result<&[u8], String> {
    if bytes.starts_with(b"REDEXST1") {
        return Err("an older save state; make it again".into());
    }
    let r = bytes.strip_prefix(&redex_builder::STATE_MAGIC[..]).ok_or("not a RE:DEX save state")?;
    let (theirs, r) = r.split_at_checked(32).ok_or("the save state ends early")?;
    if theirs != dll {
        return Err("the save state was made with another xgame.dll; make it again".into());
    }
    Ok(r)
}

/// How a thread waiting at `park`, inside `hooks` hook bodies, goes on from a save state: its back-edge budget and, when
/// its PC still points at its SVC (`word_at_pc` reads it), that SVC's length; Err says why it can't.
fn resume_of(park: Park, hooks: u32, word_at_pc: impl FnOnce() -> u32) -> Result<(i32, u32), String> {
    if hooks > 0 {
        // a resumed thread would go on in the guest code only, never back in the hook's Rust code after its call
        return Err("is inside a runtime hook (hooks.rs), whose Rust code after its guest call is only on its host stack".into());
    }
    match park {
        Park::Preempt => Err("waits at a compiled loop's back-edge, where its guest PC is not kept".into()),
        Park::Interp(budget) => Ok((budget, 0)),
        Park::Svc => {
            let w = word_at_pc();
            // Thumb `svc` is 0xDFxx; ARM's is cond 1111 imm24 (no 3DS SVC number reaches 0xDF00)
            if w & 0xFF00 == 0xDF00 {
                Ok((clock::BUDGET, 2))
            } else if (w >> 24) & 0xF == 0xF {
                Ok((clock::BUDGET, 4))
            } else {
                Err(format!("waits in an SVC but its PC holds no SVC (0x{w:08X})"))
            }
        }
    }
}

static EXEC: OnceLock<Exec> = OnceLock::new();

/// From the fault handler, on the faulting thread: true when guest addr `a` is on a page lazy GPU coherency protects,
/// now written back (or opened for the GPU's own access) so the access can be retried. Only the baton holder runs
/// guest code or touches System, so the faulting thread holds it.
pub fn lazy_fault(a: u32) -> bool {
    let Some(x) = EXEC.get() else { return false };
    unsafe { x.st() }.sys.memory.fault(a)
}

/// The page bits Memory keeps for guest addr `a` (permissions, MAPPED, LAZY, OPEN), for the fault handler's message.
pub fn page_bits(a: u32) -> Option<u8> {
    Some(unsafe { EXEC.get()?.st() }.sys.memory.page_bits(a))
}

fn turn_key(t: Turn) -> u64 {
    match t {
        Turn::Main => 0,
        Turn::Host(i) => i as u64 + 1,
    }
}

pub fn exec() -> &'static Exec {
    EXEC.get().expect("executor booted")
}

impl Exec {
    /// The baton-guarded state. Only the baton holder may call this, and it must not keep the reference across a
    /// call that can hand the baton over (`checkpoint`, native code).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn st(&self) -> &mut State {
        unsafe { &mut *self.state.get() }
    }

    /// wakes only the thread whose turn it is (a notify_all woke every host thread at each hand-over, a cost that grew
    /// with every thread the title ever made while ended ones stayed parked: fps fell with each map load)
    fn give(&self, to: Turn) {
        *self.turn.lock().unwrap() = to;
        if let Some(t) = self.waiters.lock().unwrap().get(&to) {
            t.unpark();
        }
    }

    fn wait(&self, me: Turn) {
        if crate::hostprof::on() {
            crate::hostprof::register(turn_key(me));
        }
        // registered before the check, so a give after it finds us and its unpark ends the park below
        self.waiters.lock().unwrap().insert(me, std::thread::current());
        while *self.turn.lock().unwrap() != me {
            std::thread::park();
        }
    }

    /// The function itself at `addr`, never a hook (hooks.rs runs a `virtual` hook's function through it).
    pub fn lookup_native(&self, addr: u32) -> XrFn {
        unsafe { (self.lookup_native)(addr) }
    }

    pub fn lookup(&self, addr: u32) -> XrFn {
        // XR_INTERP_ALL=1 runs everything in the interpreter (about 60 fps in the field): tells a codegen bug from the rest
        static ALL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ALL.get_or_init(|| std::env::var_os("XR_INTERP_ALL").is_some()) {
            return None;
        }
        unsafe { (self.lookup)(addr) }
    }
}

/// Guest side, baton held, `System.cpu` = this thread's live state. Runs a due audio frame and a due reschedule in
/// place; returns once this thread is the current one and the frame isn't over, else hands the baton to the main
/// loop and waits for it to come back. Never returns for a thread that ended or after ExitProcess/Break.
pub fn checkpoint(me: usize) {
    let x = exec();
    loop {
        let st = unsafe { x.st() };
        let s = &mut st.sys;
        if !s.exited && !s.broke {
            if s.cpu.cycles >= s.next_audio_frame {
                s.next_audio_frame = s.cpu.cycles + CYCLES_PER_AUDIO_FRAME;
                s.audio_frame();
            }
            s.preempt();
            if s.kernel.reschedule_pending {
                s.reschedule();
            }
            if s.kernel.current_thread == Some(st.host_tid[me]) && s.cpu.cycles < st.deadline {
                return;
            }
        }
        // its guest thread ended (returned or svcExitThread): nothing gives this host the baton again, so it goes
        let ended = st.sys.kernel.thread(st.host_tid[me]).status == ThreadStatus::Dead;
        if ended {
            x.waiters.lock().unwrap().remove(&Turn::Host(me));
        }
        x.give(Turn::Main);
        if ended {
            end_host_thread();
        }
        x.wait(Turn::Host(me));
    }
}

/// Ends the calling host thread without unwinding: it can sit under native frames (svcExitThread), which a Rust
/// unwind may not cross. The OS frees its stack; shortcut: the boxed XrCpu and std's spawn closure leak, so no frame
/// below may hold a pinned or address-registered value.
/// Thread-local destructors still run here, after give(Main): they must not touch the baton or System state.
#[cfg(windows)]
fn end_host_thread() -> ! {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ExitThread(code: u32) -> !;
    }
    unsafe { ExitThread(0) }
}

#[cfg(not(windows))]
fn end_host_thread() -> ! {
    loop {
        std::thread::park();
    }
}

/// A host thread: waits until its guest thread is first scheduled, then runs its entry natively or interpreted.
fn host_main(me: usize) {
    let x = exec();
    x.wait(Turn::Host(me));
    let mut c: Box<XrCpu> = Box::new(unsafe { std::mem::zeroed() });
    let (entry, resumed, until_sp) = {
        let st = unsafe { x.st() };
        let s = &mut st.sys;
        bridge::store(&s.cpu, &mut c);
        c.thread = me as *mut std::ffi::c_void;
        // a thread a save state left mid-function goes on interpreted from its PC (native code is entered at function
        // starts only), with the budget it had, until it returns from its entry (sp back at its stack's top)
        let resumed = st.resume.remove(&st.host_tid[me]);
        c.budget = resumed.unwrap_or(clock::BUDGET);
        let entry = s.cpu.regs[15] | s.cpu.cpsr.thumb as u32;
        let native = resumed.is_none() && x.lookup(entry).is_some();
        let t = s.kernel.thread(st.host_tid[me]);
        let how = if resumed.is_some() { "interpreted, resumed from a save state" } else if native { "native" } else { "interpreted" };
        log::log(&format!("thread {} '{}' starts at 0x{entry:08X} ({how}), sp 0x{:08X}, priority {}", t.id, t.name, c.r[13], t.priority));
        (entry, resumed.is_some(), if resumed.is_some() { t.stack_top } else { c.r[13] })
    };
    if me == 0 {
        progress::milestone("entry");
    }
    let c = &mut *c as *mut XrCpu;
    match x.lookup(entry).filter(|_| !resumed) {
        Some(f) => unsafe { bridge::enter_native(c, f, THREAD_EXIT_MAGIC, until_sp) },
        None => unsafe { interp::interp_entry(c, entry, THREAD_EXIT_MAGIC, until_sp) },
    }
    // the entry returned (to THREAD_EXIT_MAGIC): the thread is over
    {
        let st = unsafe { x.st() };
        let s = &mut st.sys;
        clock::charge(s, unsafe { &mut *c });
        bridge::load(&mut s.cpu, unsafe { &*c });
        let tid = st.host_tid[me];
        if s.kernel.current_thread == Some(tid) {
            s.kernel.end_thread(tid);
        }
    }
    checkpoint(me);
    unreachable!("an ended thread got the baton back");
}

/// The frontend's handle on the running game. Its methods run on the thread that booted it (the main loop).
pub struct Machine {
    /// xgame.dll's build id (`xr_game_info.build`)
    pub build: String,
    /// the running title's id (the loader's, which `xr_game_info.title_id` was checked against)
    pub title_id: u64,
}

impl Machine {
    /// Load the title from `cci` into a fresh 4 GB guest region and `build_dir/xgame.dll` against it.
    pub fn boot(cci: &Path, build_dir: &Path, config: Config) -> Result<Machine, String> {
        let mem = crate::mem::GuestMem::reserve().map_err(|e| format!("reserve 4 GB of address space: {e}"))?;
        crate::GUEST_BASE.store(mem.base as usize, std::sync::atomic::Ordering::Relaxed);
        if config.update.is_some() {
            return Err("an update runs its own code.bin, which the recompiled build doesn't include (not supported yet): run without the update".into());
        }
        let mut sys = xr_hle::loader::load(cci, config, Box::new(mem)).map_err(|e| format!("{}: {e}", cci.display()))?;
        if sys.code_modded {
            return Err("a mod changes the game's code (code.bin/code.ips/code.bps in its mods folder); the recompiled build doesn't include that change, so it is refused: remove the code mod".into());
        }
        let title = sys.title.as_ref().expect("a loaded title");
        let code = title.code().map_err(|e| format!("{}: code.bin: {e}", cci.display()))?;
        let title_id = title.program_id();
        let rt: &'static XrRuntime = Box::leak(Box::new(XrRuntime {
            abi_version: XR_ABI_VERSION,
            mem: sys.memory.base(),
            svc: Some(bridge::svc_entry),
            interp: Some(interp::interp_entry),
            preempt: Some(bridge::preempt_entry),
            // a DLL built without hooks.txt's entries never calls them (no puppets then)
            hooks: crate::hooks::table(),
        }));
        let game = Game::open(&build_dir.join("xgame.dll"), rt)?;
        let build = game.check(&code, title_id)?;
        let lookup = game.lookup_fn()?;
        let lookup_native = game.lookup_native_fn().unwrap_or(lookup);
        let modules = game.modules()?;
        log::log(&format!("xgame.dll: {} native CRO modules", modules.by_name.len()));
        let _ = bridge::MODULES.set(modules);
        // always: it also tracks IN_BATTLE; XR_INTERP_MODULES=1 is checked inside it
        sys.module_hook = Some(bridge::module_hook);
        std::mem::forget(game); // the DLL stays loaded for the life of the process
        let deadline = sys.next_frame_boundary;
        let state = State { sys, hosts: Vec::new(), host_tid: Vec::new(), deadline, camera: Default::default(), ahead: Default::default(), parked: Vec::new(), in_hook: Vec::new(), resume: Default::default() };
        EXEC.set(Exec { turn: Mutex::new(Turn::Main), waiters: Mutex::default(), state: UnsafeCell::new(state), lookup, lookup_native })
            .map_err(|_| "a Machine is already booted in this process".to_string())?;
        // GPU coherency phase 2 (XR_EAGER_GPU=1 keeps phase 1's write-back after every command list, for bisecting)
        if std::env::var_os("XR_EAGER_GPU").is_none() {
            unsafe { exec().st() }.sys.lazy_gpu();
        }
        if crate::hostprof::on() {
            let logs = build_dir.parent().and_then(|p| p.parent()).map_or_else(|| build_dir.to_path_buf(), |d| d.join("logs"));
            crate::hostprof::start(logs, || turn_key(*exec().turn.lock().unwrap()));
        }
        Ok(Machine { build, title_id })
    }

    /// The System, between frames (the main loop holds the baton).
    pub fn system(&mut self) -> &mut System {
        &mut unsafe { exec().st() }.sys
    }

    #[allow(dead_code)] // set_input/take_audio/screen: the frontend's, headless redex doesn't need them
    pub fn set_input(&mut self, input: xr_hle::services::hid::InputState) {
        self.system().set_input(input);
    }

    #[allow(dead_code)]
    pub fn take_audio(&mut self) -> Vec<[i16; 2]> {
        self.system().take_audio()
    }

    /// RGBA of one screen, as the last frame left it.
    #[allow(dead_code)]
    pub fn screen(&mut self, screen: zakuro_common::Screen) -> Vec<u8> {
        self.system().read_screen(screen)
    }

    /// A save state, between frames: xr-hle's (`System::save_state`) after the runtime's part, `frames` (the runner's
    /// frame count and script clock), each live thread's back-edge budget and the CRO modules xgame.dll placed. Every
    /// live thread's guest state is in `System` when it waits in an SVC or the interpreter; one waiting at a compiled
    /// loop's back-edge has its PC only on its host stack, and the state is refused (another frame may have none).
    /// `dll` is the fingerprint of this run's xgame.dll (`redex_builder::dll_fingerprint`).
    pub fn save_state(&mut self, frames: (u64, u64), dll: [u8; 32]) -> Result<Vec<u8>, String> {
        let st = unsafe { exec().st() };
        let s = &mut st.sys;
        // (tid, budget, the SVC's length when the PC still points at it)
        let mut live = Vec::new();
        for (tid, host) in st.hosts.iter().enumerate() {
            let t = &s.kernel.threads[tid];
            let Some((gid, h)) = *host else { continue };
            // a thread with no host yet starts at its entry as usual
            if gid != t.guest_id || t.status == ThreadStatus::Dead {
                continue;
            }
            let pc = if s.kernel.current_thread == Some(tid as ThreadId) { s.cpu.regs[15] } else { t.context.regs[15] };
            let word = || s.memory.slice_mut(pc & !1, 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
            let (budget, len) = resume_of(st.parked[h], st.in_hook[h], word).map_err(|e| format!("thread {tid} '{}' {e} (pc 0x{pc:08X}); try another frame", t.name))?;
            live.push((tid, budget, len));
        }
        // the state has each thread at the instruction after its SVC (where the interpreter goes on); this process's
        // host threads go on from their own stacks, so the contexts are put back right after
        let at = |s: &mut System, tid: usize| -> (u32, bool) {
            if s.kernel.current_thread == Some(tid as ThreadId) { (s.cpu.regs[15], s.cpu.cpsr.thumb) } else { let c = &s.kernel.threads[tid].context; (c.regs[15], c.cpsr.thumb) }
        };
        let set = |s: &mut System, tid: usize, (pc, thumb): (u32, bool)| {
            let (r, p) = if s.kernel.current_thread == Some(tid as ThreadId) { (&mut s.cpu.regs, &mut s.cpu.cpsr) } else { let c = &mut s.kernel.threads[tid].context; (&mut c.regs, &mut c.cpsr) };
            (r[15], p.thumb) = (pc, thumb);
        };
        let was: Vec<(u32, bool)> = live.iter().map(|&(tid, _, _)| at(s, tid)).collect();
        for (&(tid, _, len), &(pc, _)) in live.iter().zip(&was).filter(|((_, _, len), _)| *len > 0) {
            set(s, tid, ((pc & !1) + len, len == 2));
        }
        let hle = s.save_state();
        for (&(tid, _, _), &w) in live.iter().zip(&was) {
            set(s, tid, w);
        }
        let mut out = state_header(dll);
        for v in [frames.0, frames.1] {
            out.extend(v.to_le_bytes());
        }
        out.extend((live.len() as u32).to_le_bytes());
        for &(tid, budget, _) in &live {
            out.extend((tid as u32).to_le_bytes());
            out.extend(budget.to_le_bytes());
        }
        let placed = bridge::placed();
        out.extend((placed.len() as u32).to_le_bytes());
        for (k, base, size) in placed {
            out.extend([k, base, size].iter().flat_map(|v| v.to_le_bytes()));
        }
        out.extend(hle?);
        Ok(out)
    }

    /// Puts the game back as `save_state` had it, before the first frame of this process; returns its `frames`. A state
    /// made with another xgame.dll than `dll` (its fingerprint) is refused.
    pub fn load_state(&mut self, bytes: &[u8], dll: [u8; 32]) -> Result<(u64, u64), String> {
        let st = unsafe { exec().st() };
        if !st.hosts.is_empty() {
            return Err("a save state loads only before the first frame".into());
        }
        let mut r = state_body(bytes, dll)?;
        let mut word = |n: usize| -> Result<u64, String> {
            let (v, rest) = r.split_at_checked(n).ok_or("the save state ends early")?;
            r = rest;
            Ok(v.iter().rev().fold(0, |a, &b| a << 8 | b as u64))
        };
        let frames = (word(8)?, word(8)?);
        let mut resume = std::collections::HashMap::new();
        for _ in 0..word(4)? {
            resume.insert(word(4)? as ThreadId, word(4)? as u32 as i32);
        }
        let mut placed = Vec::new();
        for _ in 0..word(4)? {
            placed.push((word(4)? as u32, word(4)? as u32, word(4)? as u32));
        }
        st.sys.load_state(r)?;
        st.resume = resume;
        bridge::place_again(&placed, st.sys.cro.modules.iter().any(|m| m.name == "DllBattle"));
        st.deadline = st.sys.next_frame_boundary;
        Ok(frames)
    }

    /// Run the guest until the clock reaches the next frame boundary, then do the end-of-frame work
    /// (Zakuro's `System::run_frame`, with guest threads running on their own host threads).
    pub fn run_frame(&mut self) -> FrameOutcome {
        let x = exec();
        crate::hostprof::FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let st = unsafe { x.st() };
            if outcome(&st.sys).is_none() {
                let data = field_data(&st.sys);
                st.ahead.refresh(&mut st.sys, data);
            }
            let s = &mut st.sys;
            if let Some(o) = outcome(s) {
                return o;
            }
            let input = s.services.hid.input;
            s.idle_interrupted(false);
            xr_hle::services::hid::update(s, input);
            xr_hle::cheats::run(s);
            st.deadline = s.next_frame_boundary;
        }
        loop {
            let st = unsafe { x.st() };
            let s = &mut st.sys;
            if let Some(o) = outcome(s) {
                return o;
            }
            if s.cpu.cycles >= st.deadline {
                break;
            }
            if s.cpu.cycles >= s.next_audio_frame {
                s.next_audio_frame = s.cpu.cycles + CYCLES_PER_AUDIO_FRAME;
                s.audio_frame();
            }
            if s.kernel.reschedule_pending || s.kernel.current_thread.is_none() {
                s.reschedule();
            }
            let Some(tid) = s.kernel.current_thread else {
                if s.kernel.live_thread_count() == 0 {
                    log::log("every guest thread ended");
                    s.exited = true;
                    continue;
                }
                // everything's blocked: jump to the next event, never past the frame or (while the DSP runs) the
                // next audio frame, whose interrupt an audio thread waits on (Zakuro's System::step)
                let mut limit = st.deadline;
                if s.services.dsp.running {
                    limit = limit.min(s.next_audio_frame);
                }
                s.cpu.cycles = match s.kernel.next_event() {
                    Some(tick) if tick > s.cpu.cycles => tick.min(limit),
                    _ => limit,
                };
                s.kernel.reschedule_pending = true;
                continue;
            };
            debug_assert!(st.sys.kernel.thread(tid).status != xr_hle::kernel::thread::ThreadStatus::Dead, "the baton goes to an ended thread");
            let h = st.host_for(tid);
            x.give(Turn::Host(h));
            x.wait(Turn::Main);
        }
        let st = unsafe { x.st() };
        camera_cut(st);
        let s = &mut st.sys;
        s.next_frame_boundary = s.cpu.cycles + s.frame_cycles;
        s.end_frame();
        progress::services(&s.service_calls);
        if s.command_lists > 0 {
            progress::milestone("command_list");
        }
        FrameOutcome::Completed
    }
}

/// Frame interpolation's cut hint, once a refresh b4 its pacing: X's field camera, read
/// (never written) through DllField's camera block, breaking from its own motion (CameraCuts: a door, a warp, a
/// scripted cut) tells the pacing it cut elsewhere, and no pictures in between are shown across it. The last camera is
/// kept while there's none (a map's load), so a warp is seen when the next map's comes.
fn camera_cut(st: &mut State) {
    let s = &mut st.sys;
    if !s.gpu.interpolates() {
        st.camera = Default::default();
        return;
    }
    let Some(data) = field_data(s) else { return };
    let Some(now) = xr_net::x::field::camera(&mut crate::Mem(&mut s.memory), data) else { return };
    if st.camera.see(now) {
        s.camera_cut();
    }
}

/// DllField's data segment while it's loaded
fn field_data(s: &System) -> Option<u32> {
    s.cro.modules.iter().rev().find(|m| m.name == "DllField").and_then(|m| m.segments.get(2)).copied()
}

fn outcome(s: &System) -> Option<FrameOutcome> {
    if s.exited {
        Some(FrameOutcome::Exited)
    } else if s.broke {
        Some(FrameOutcome::Faulted)
    } else {
        None
    }
}

impl State {
    /// The host thread running kernel thread `tid`, spawned on its first turn.
    fn host_for(&mut self, tid: ThreadId) -> usize {
        let gid = self.sys.kernel.thread(tid).guest_id;
        if let Some(Some((g, h))) = self.hosts.get(tid as usize) {
            if *g == gid {
                return *h;
            }
        }
        let h = self.host_tid.len();
        self.host_tid.push(tid);
        self.parked.push(Park::Preempt); // (set at its first checkpoint, before the main loop runs again)
        self.in_hook.push(0);
        if self.hosts.len() <= tid as usize {
            self.hosts.resize(tid as usize + 1, None);
        }
        self.hosts[tid as usize] = Some((gid, h));
        // Generated code recurses on the host stack, so guest threads get big ones (reserved, committed as used).
        // it ends with its guest thread (see checkpoint); X starts over 300 short threads before its first town
        std::thread::Builder::new()
            .name(format!("guest {tid}"))
            .stack_size(256 << 20)
            .spawn(move || host_main(h))
            .expect("spawn a guest host thread");
        h
    }
}

#[cfg(test)]
mod tests {
    use super::{resume_of, state_body, state_header, Park};
    use crate::clock::BUDGET;

    #[test]
    fn a_state_loads_only_with_its_own_dll() {
        let mut state = state_header([7; 32]);
        state.extend(b"body");
        assert_eq!(state_body(&state, [7; 32]), Ok(&b"body"[..]));
        assert!(state_body(&state, [8; 32]).unwrap_err().contains("made with another xgame.dll"));
        assert!(state_body(b"REDEXST1rest", [7; 32]).unwrap_err().contains("an older save state"));
        assert!(state_body(&state[..20], [7; 32]).is_err());
    }

    /// A save state holds a thread waiting in an SVC (at the next instruction) or in the interpreter (with its
    /// budget); one at a native back-edge, or inside a hook's body (whose Rust code after its guest call would never
    /// run after a load), is refused, wherever it waits.
    #[test]
    fn which_waiting_threads_a_state_can_hold() {
        let arm_svc = || 0xEF00_0024;
        assert_eq!(resume_of(Park::Svc, 0, arm_svc), Ok((BUDGET, 4)));
        assert_eq!(resume_of(Park::Svc, 0, || 0x0FFF_DF24), Ok((BUDGET, 2)), "thumb svc 0x24");
        assert!(resume_of(Park::Svc, 0, || 0xE28B_B010).is_err(), "no SVC at the PC");
        assert_eq!(resume_of(Park::Interp(300), 0, || 0), Ok((300, 0)));
        assert!(resume_of(Park::Preempt, 0, || 0).unwrap_err().contains("back-edge"));
        for park in [Park::Svc, Park::Interp(300), Park::Preempt] {
            assert!(resume_of(park, 1, arm_svc).unwrap_err().contains("hook"), "inside a hook");
            assert!(resume_of(park, 2, arm_svc).unwrap_err().contains("hook"), "a guest call inside a hook");
        }
    }
}
