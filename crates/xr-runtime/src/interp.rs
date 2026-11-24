//! The interpreter fallback (ABI v3 `interp`): runs guest code the DLL lacks with zakuro-cpu, on `System.cpu`
//! over the flat guest memory, and hands back to native code at the next function it has.
use crate::abi::XrCpu;
use crate::exec::{checkpoint, exec};
use crate::{bridge, clock, log, progress};
use std::cell::Cell;
use xr_hle::Stepped;

thread_local! {
    /// The call-shaped entry (start, until_pc, until_sp) interp_entry is running natively on this host thread.
    static TRYING: Cell<(u32, u32, u32)> = const { Cell::new((1, 1, 1)) };
}

/// Run from `start` (odd = Thumb) until pc == until_pc (T bit included) with sp == until_sp. All guest state goes
/// in and out through `c`.
pub unsafe extern "C" fn interp_entry(c: *mut XrCpu, start: u32, until_pc: u32, until_sp: u32) {
    let x = exec();
    // A call-shaped entry (lr and sp are the frame's return) at a function the DLL has runs natively: a whole CRO
    // function called from interpreted code, or a module function placed after the caller looked. A native
    // function that hands the very same entry straight back (a whole-function miss stub, a miss at its first
    // instruction) is interpreted instead of bouncing.
    let key = (start, until_pc, until_sp);
    if unsafe { (*c).r[14] == until_pc && (*c).r[13] == until_sp } && TRYING.get() != key {
        if let Some(f) = x.lookup(start) {
            let outer = TRYING.replace(key);
            unsafe { bridge::enter_native(c, f, until_pc, until_sp) };
            TRYING.set(outer);
            return;
        }
    }
    {
        let st = unsafe { x.st() };
        let (s, cr) = (&mut st.sys, unsafe { &mut *c });
        clock::charge_switch(s, cr);
        bridge::load(&mut s.cpu, cr);
        bridge::check_fpscr(cr.fpscr);
        s.cpu.regs[15] = start & !1;
        s.cpu.cpsr.thumb = start & 1 != 0;
        s.idle_interrupted(true);
        if progress::entry(start, cr.r[14]) {
            log::log(&format!("interp: first entry at 0x{start:08X} (lr 0x{:08X}, {} so far)", cr.r[14], progress::entry_count()));
        }
    }
    run(c, until_pc, until_sp);
    let cr = unsafe { &mut *c };
    bridge::store(&unsafe { x.st() }.sys.cpu, cr);
    bridge::check_fpscr(cr.fpscr);
}

fn run(c: *mut XrCpu, until_pc: u32, until_sp: u32) {
    let x = exec();
    let me = unsafe { (*c).thread } as usize;
    // Native code is entered only on a call (the step left lr = the next instruction), with that return address as
    // its frame's return. A plain branch into a function entry (a tail call, or a jump into a split-function piece,
    // which is mid-function) carries no meaningful lr, so it stays interpreted until the next call.
    let mut call: Option<u32> = None;
    let mut unwound = false;
    loop {
        let st = unsafe { x.st() };
        let s = &mut st.sys;
        s.preempt();
        if s.kernel.reschedule_pending || s.cpu.cycles >= st.deadline || s.cpu.cycles >= s.next_audio_frame || s.exited || s.broke {
            crate::exec::park(me, crate::exec::Park::Interp(unsafe { (*c).budget }));
            checkpoint(me);
            continue;
        }
        let (pc, sp) = (s.cpu.regs[15] | s.cpu.cpsr.thumb as u32, s.cpu.regs[13]);
        if let Some(ret) = call.take() {
            // an interpreted call site (a frame resumed from a save state) runs the hook of a hooked address too
            if let Some(f) = crate::hooks::hook_for(pc).or_else(|| x.lookup(pc)) {
                s.idle_interrupted(true);
                bridge::store(&s.cpu, unsafe { &mut *c });
                unsafe { (*c).r[15] = pc & !1 };
                unsafe { bridge::enter_native(c, f, ret, sp) };
                let s = &mut unsafe { x.st() }.sys;
                clock::charge_switch(s, unsafe { &mut *c });
                bridge::load(&mut s.cpu, unsafe { &*c });
                s.cpu.regs[15] = ret & !1;
                s.cpu.cpsr.thumb = ret & 1 != 0;
                continue;
            }
        }
        // until_pc/until_sp is the caller's real frame return (ABI v4 ret_lr/ret_sp), so a match is the return
        if pc == until_pc && sp == until_sp {
            return;
        }
        // the stack popped past the frame (longjmp, EHABI unwinding) w/o returning: the frame's return never comes
        if sp > until_sp && !std::mem::replace(&mut unwound, true) {
            log::log(&format!("interp: a non-local exit at 0x{pc:08X} left the frame (sp 0x{sp:08X} above 0x{until_sp:08X}); thread {me} stays interpreted until it ends"));
        }
        let t = s.cpu.cpsr.thumb as u32;
        let next = s.cpu.regs[15].wrapping_add(if t != 0 { 2 } else { 4 });
        let native = clock::native_clock();
        // only the native charging model needs the state before the step (to_bits is not free)
        let pre = native.then(|| (s.cpu.regs[15], s.cpu.cycles, s.cpu.cpsr.to_bits()));
        let (at, watched) = (s.cpu.regs[15], watch::before(s));
        let stepped = s.step_instruction(Some(st.deadline));
        if let Some(w) = watched {
            watch::after(s, at, w);
        }
        // native charging: no cycle per instruction; the back-edges compiled code would pay, from the same budget
        if let Some((at, before, cpsr)) = pre.filter(|_| matches!(stepped, Stepped::Ran | Stepped::Svc(_))) {
            s.cpu.cycles = before;
            let jumped = s.cpu.regs[15] != next;
            let word = |a| s.memory.slice_mut(a, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
            if matches!(stepped, Stepped::Ran) && clock::pays_budget(word, at, t != 0, jumped, cpsr) {
                let cr = unsafe { &mut *c };
                cr.budget -= 1;
                if cr.budget <= 0 {
                    clock::charge(s, cr);
                }
            }
        }
        match stepped {
            Stepped::Ran | Stepped::Idled => {}
            Stepped::ThreadEnded => {
                checkpoint(me);
                unreachable!("an ended thread got the baton back");
            }
            Stepped::Svc(imm) => {
                if native {
                    // as svc_entry charges native code
                    clock::charge(s, unsafe { &mut *c });
                    s.cpu.cycles += clock::CYCLES_PER_SVC;
                }
                clock::trace_svc(s, imm);
                xr_hle::kernel::svc::dispatch(s, imm);
                progress::svc(imm, !s.unimplemented_svcs.contains(&imm));
            }
        }
        // zakuro-cpu steps a Thumb BL/BLX imm as its two 16-bit halves (ARMv6), so the second half leaves lr = next | 1
        // only an instruction that ran can be the call (an idled loop or an SVC leaves lr as it was)
        if matches!(stepped, Stepped::Ran) && s.cpu.regs[15] != next && s.cpu.regs[14] == next | t {
            call = Some(next | t);
        }
    }
}

/// XR_WATCH=ADDR[+LEN],... (hex; LEN bytes, default 4): logs every interpreted instruction that changes a watched
/// word, with its pc, lr and r0-r3 (run with XR_INTERP_ALL=1 so all guest code is interpreted). XR_WATCH_MAX caps the
/// lines (default 400). Reverse engineering only: what wrote a field (docs/re/).
mod watch {
    use crate::log;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::OnceLock;

    fn words() -> &'static [u32] {
        static W: OnceLock<Vec<u32>> = OnceLock::new();
        W.get_or_init(|| {
            let mut v = Vec::new();
            for spec in std::env::var("XR_WATCH").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
                let (a, n) = spec.split_once('+').unwrap_or((spec, "4"));
                let hex = |s: &str| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).unwrap_or(0);
                let a = hex(a) & !3;
                v.extend((0..hex(n).div_ceil(4)).map(|i| a + 4 * i));
            }
            v.truncate(16); // at most 16 words
            v
        })
    }

    fn read(s: &mut xr_hle::System, a: u32) -> u32 {
        s.memory.slice_mut(a, 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    /// (the watched words, their values); None when nothing is watched
    pub fn before(s: &mut xr_hle::System) -> Option<[u32; 16]> {
        let w = words();
        if w.is_empty() {
            return None;
        }
        let mut v = [0; 16];
        for (d, &a) in v.iter_mut().zip(w) {
            *d = read(s, a);
        }
        Some(v)
    }

    pub fn after(s: &mut xr_hle::System, pc: u32, before: [u32; 16]) {
        static LINES: AtomicU32 = AtomicU32::new(0);
        static MAX: OnceLock<u32> = OnceLock::new();
        let max = *MAX.get_or_init(|| std::env::var("XR_WATCH_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(400));
        for (&a, old) in words().iter().zip(before) {
            let new = read(s, a);
            if new != old && LINES.fetch_add(1, Ordering::Relaxed) < max {
                let r = s.cpu.regs;
                // words on the stack that look like code addresses: a rough backtrace
                let stack: Vec<String> = (0..64).map(|i| read(s, r[13] + 4 * i)).filter(|w| (0x0010_0000..0x0100_0000).contains(w)).take(12).map(|w| format!("{w:08X}")).collect();
                log::log(&format!(
                    "watch 0x{a:08X}: {old:08X} -> {new:08X} (f32 {} -> {}) pc 0x{pc:08X} lr 0x{:08X} r0-r3 {:08X} {:08X} {:08X} {:08X} sp 0x{:08X} stack {}",
                    f32::from_bits(old), f32::from_bits(new), r[14], r[0], r[1], r[2], r[3], r[13], stack.join(" ")
                ));
            }
        }
    }
}
