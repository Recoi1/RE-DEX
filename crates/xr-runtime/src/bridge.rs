//! The SVC and preempt callbacks native code calls, and the XrCpu <-> zakuro_cpu::Cpu copies around them.
use crate::abi::XrCpu;
use crate::game::Modules;
use crate::exec::{checkpoint, exec};
use crate::{clock, log, progress};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use zakuro_cpu::{Cpu, Psr};

/// XrCpu -> Cpu: r0-r15, NZCVQ/GE, VFP, FPSCR, TLS, the exclusive monitor. The Cpu keeps its mode and T bit.
pub fn load(cpu: &mut Cpu, c: &XrCpu) {
    cpu.regs = c.r;
    let (mode, thumb) = (cpu.cpsr.mode, cpu.cpsr.thumb);
    cpu.cpsr = Psr::from_bits(c.cpsr);
    (cpu.cpsr.mode, cpu.cpsr.thumb) = (mode, thumb);
    cpu.vfp.regs = unsafe { c.v.u };
    cpu.vfp.fpscr = c.fpscr;
    cpu.cp15.thread_id_ro = c.tls;
    cpu.exclusive_addr = (c.excl_valid != 0).then_some(c.excl_addr);
}

/// Cpu -> XrCpu (the reverse of `load`). A thread switch cleared the Cpu's monitor (Horizon's CLREX), so excl_valid
/// comes back 0 after one.
pub fn store(cpu: &Cpu, c: &mut XrCpu) {
    c.r = cpu.regs;
    c.cpsr = cpu.cpsr.to_bits();
    c.v.u = cpu.vfp.regs;
    c.fpscr = cpu.vfp.fpscr;
    c.tls = cpu.cp15.thread_id_ro;
    (c.excl_addr, c.excl_valid) = (cpu.exclusive_addr.unwrap_or(0), cpu.exclusive_addr.is_some() as u32);
}

/// Native code assumes FPSCR mode 0x03000000 (FZ+DN, round to nearest); a VMSR to anything else is worth a loud
/// line. Checked at every bridge crossing, so it names the first SVC/preempt/miss after the write.
pub fn check_fpscr(fpscr: u32) {
    static LAST: AtomicU32 = AtomicU32::new(0x0300_0000);
    let mode = fpscr & 0x0FFF_FF00;
    if mode != 0x0300_0000 && LAST.swap(mode, Ordering::Relaxed) != mode {
        log::log(&format!("WARNING: guest FPSCR mode is 0x{mode:08X}, native code assumes 0x03000000 (FZ+DN, round to nearest)"));
    }
}

/// Every way the runtime enters native code: a guest frame that returns to `ret_lr` with sp `ret_sp`. Native code
/// takes its frame's return from these fields (ABI v4), not from r14/r13.
pub unsafe fn enter_native(c: *mut XrCpu, f: unsafe extern "C" fn(*mut XrCpu), ret_lr: u32, ret_sp: u32) {
    unsafe {
        ((*c).ret_lr, (*c).ret_sp) = (ret_lr, ret_sp);
        f(c)
    }
}

pub unsafe extern "C" fn svc_entry(c: *mut XrCpu, imm: u32) {
    let (x, c) = (exec(), unsafe { &mut *c });
    let me = c.thread as usize;
    {
        let s = &mut unsafe { x.st() }.sys;
        clock::charge(s, c);
        s.cpu.cycles += clock::CYCLES_PER_SVC;
        load(&mut s.cpu, c);
        clock::trace_svc(s, imm);
        check_fpscr(c.fpscr);
        // native code ran, an idle-loop watch can't span it
        s.idle_interrupted(true);
        xr_hle::kernel::svc::dispatch(s, imm);
        progress::svc(imm, !s.unimplemented_svcs.contains(&imm));
        match imm {
            0x2D => progress::milestone("srv"),
            0x32 if !s.services_seen.is_empty() => progress::milestone("services"),
            _ => {}
        }
    }
    crate::exec::park(me, crate::exec::Park::Svc);
    checkpoint(me);
    store(&unsafe { x.st() }.sys.cpu, c);
}

pub unsafe extern "C" fn preempt_entry(c: *mut XrCpu) {
    let (x, c) = (exec(), unsafe { &mut *c });
    {
        let s = &mut unsafe { x.st() }.sys;
        clock::charge(s, c);
        load(&mut s.cpu, c);
        check_fpscr(c.fpscr);
        s.idle_interrupted(true);
    }
    crate::exec::park(c.thread as usize, crate::exec::Park::Preempt);
    checkpoint(c.thread as usize);
    store(&unsafe { x.st() }.sys.cpu, c);
}

/// DllBattle is placed (a battle is on): a script's `autobattle` line plays battles while its clock waits.
pub static IN_BATTLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// xgame.dll's CRO modules, set at boot.
pub static MODULES: OnceLock<Modules> = OnceLock::new();

/// The modules placed natively, (index, code base, code size): the DLL's own list, mirrored so that a module the DLL
/// does not place (unknown, modded, refused) can still unplace the ones its code now covers.
static PLACED: std::sync::Mutex<Vec<(u32, u32, u32)>> = std::sync::Mutex::new(Vec::new());

/// The modules placed natively now, for a save state.
pub fn placed() -> Vec<(u32, u32, u32)> {
    PLACED.lock().unwrap().clone()
}

/// A loaded save state's placements (from `placed`; the code hashes were checked when they were first placed) and
/// whether a battle is on.
pub fn place_again(placed: &[(u32, u32, u32)], battle: bool) {
    IN_BATTLE.store(battle, Ordering::Relaxed);
    let Some(m) = MODULES.get() else { return };
    let mut list = PLACED.lock().unwrap();
    for &(k, code, size) in placed {
        if unsafe { (m.place)(k, code) } == 1 {
            list.push((k, code, size));
        } else {
            log::log(&format!("native: xr_place_module({k}, 0x{code:08X}) refused a save state's module, it runs interpreted"));
        }
    }
}

/// Takes every module whose code overlaps [base, base + size) out of `placed`; returns their indices.
fn unplace_range(placed: &mut Vec<(u32, u32, u32)>, base: u32, size: u32) -> Vec<u32> {
    let overlaps = |&(_, b, s): &(u32, u32, u32)| (base as u64) < b as u64 + s as u64 && (b as u64) < base as u64 + size as u64;
    let gone = placed.iter().filter(|p| overlaps(p)).map(|p| p.0).collect();
    placed.retain(|p| !overlaps(p));
    gone
}

/// xr-hle's ldr:ro hook (`System::module_hook`; runs inside the SVC, baton held): a module the DLL has, with the
/// code it was built from, is placed natively; anything else runs interpreted, with a log line saying why.
pub fn module_hook(name: &str, placed: Option<(u32, u32, u64)>) {
    if name == "DllBattle" {
        IN_BATTLE.store(placed.is_some(), std::sync::atomic::Ordering::Relaxed);
    }
    // XR_INTERP_MODULES=1 leaves every CRO to the interpreter: tells a native-module bug from any other;
    // XR_INTERP_MODULES=DllField,... only the named ones (e.g. for XR_WATCH)
    static INTERP: OnceLock<Option<String>> = OnceLock::new();
    if let Some(v) = INTERP.get_or_init(|| std::env::var("XR_INTERP_MODULES").ok()) {
        if v == "1" || v.split(',').any(|n| n == name) {
            return;
        }
    }
    let Some(m) = MODULES.get() else { return };
    let mut list = PLACED.lock().unwrap();
    // a module loaded over placed ones replaces them (an unload nobody reported): whether or not this one is placed,
    // xr_lookup must not find their functions there any more
    if let Some((code, size, _)) = placed {
        for k in unplace_range(&mut list, code, size) {
            unsafe { (m.unplace)(k) };
        }
    }
    let Some(&(k, hash)) = m.by_name.get(name) else {
        if placed.is_some() {
            log::log(&format!("native: xgame.dll has no module {name}, it runs interpreted"));
        }
        return;
    };
    list.retain(|p| p.0 != k);
    match placed {
        Some((_, _, h)) if h != hash => {
            log::log(&format!("native: {name} is not the module xgame.dll was built from (code hash {h:016X}, built {hash:016X}; a mod?), it runs interpreted"));
            unsafe { (m.unplace)(k) };
        }
        // xr_place_module refuses a base that is not 4-aligned; inside an SVC that is a log line, not a panic
        Some((code, _, _)) if unsafe { (m.place)(k, code) } != 1 => {
            log::log(&format!("native: xr_place_module({k}, 0x{code:08X}) refused {name} (code not 4-aligned?), it runs interpreted"));
            unsafe { (m.unplace)(k) };
        }
        Some((code, size, _)) => {
            log::log(&format!("native: {name} (module {k}) code at 0x{code:08X}"));
            list.push((k, code, size));
        }
        None => unsafe { (m.unplace)(k) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    unsafe extern "C" fn place(k: u32, base: u32) -> u32 {
        CALLS.lock().unwrap().push(format!("place {k}"));
        (base & 3 == 0) as u32
    }
    unsafe extern "C" fn unplace(k: u32) {
        CALLS.lock().unwrap().push(format!("unplace {k}"));
    }
    fn calls() -> Vec<String> {
        std::mem::take(&mut *CALLS.lock().unwrap())
    }

    #[test]
    fn overlap_range() {
        let mut p = vec![(0, 0x1000, 0x100), (1, 0x1100, 0x100), (2, 0x3000, 0x10)];
        assert_eq!(unplace_range(&mut p, 0x10F0, 0x20), vec![0, 1]);
        assert_eq!(unplace_range(&mut p, 0x2FF0, 0x10), Vec::<u32>::new(), "adjacent is not overlapping");
        assert_eq!(unplace_range(&mut p, 0xFFFF_FFF0, 0x10), Vec::<u32>::new(), "no u32 wrap");
        assert_eq!(p, vec![(2, 0x3000, 0x10)]);
    }

    /// A module loaded over a placed one whose unload was never reported unplaces it on every path: unknown name,
    /// modded code, a refused base, or a normal place.
    #[test]
    fn a_module_over_a_placed_one_unplaces_it() {
        let by_name = [("A".to_string(), (0, 1)), ("B".to_string(), (1, 2))].into_iter().collect();
        assert!(MODULES.set(Modules { place, unplace, by_name }).is_ok());
        module_hook("A", Some((0x1000, 0x100, 1)));
        assert_eq!(calls(), ["place 0"]);
        module_hook("Unknown", Some((0x1080, 0x10, 9)));
        assert_eq!(calls(), ["unplace 0"], "unknown over A");
        module_hook("A", Some((0x1000, 0x100, 1)));
        module_hook("B", Some((0x1000, 0x100, 99)));
        assert_eq!(calls(), ["place 0", "unplace 0", "unplace 1"], "modded B over A");
        module_hook("A", Some((0x1000, 0x100, 1)));
        module_hook("B", Some((0x1002, 0x100, 2)));
        assert_eq!(calls(), ["place 0", "unplace 0", "place 1", "unplace 1"], "refused B over A");
        module_hook("A", Some((0x1000, 0x100, 1)));
        module_hook("B", Some((0x10F0, 0x100, 2)));
        module_hook("Unknown", Some((0x2000, 0x10, 9)));
        assert_eq!(calls(), ["place 0", "unplace 0", "place 1"], "B over A; unknown elsewhere touches nothing");
        module_hook("B", None);
        assert_eq!(calls(), ["unplace 1"]);
        assert!(PLACED.lock().unwrap().is_empty());
    }
}

/// 3dbrew names, for logs and progress-run.json.
pub fn name(num: u32) -> &'static str {
    match num {
        0x01 => "ControlMemory", 0x02 => "QueryMemory", 0x03 => "ExitProcess", 0x08 => "CreateThread",
        0x09 => "ExitThread", 0x0A => "SleepThread", 0x0B => "GetThreadPriority", 0x0C => "SetThreadPriority",
        0x11 => "GetCurrentProcessorNumber", 0x13 => "CreateMutex", 0x14 => "ReleaseMutex",
        0x15 => "CreateSemaphore", 0x16 => "ReleaseSemaphore", 0x17 => "CreateEvent", 0x18 => "SignalEvent",
        0x19 => "ClearEvent", 0x1A => "CreateTimer", 0x1B => "SetTimer", 0x1C => "CancelTimer", 0x1D => "ClearTimer",
        0x1E => "CreateMemoryBlock", 0x1F => "MapMemoryBlock", 0x20 => "UnmapMemoryBlock",
        0x21 => "CreateAddressArbiter", 0x22 => "ArbitrateAddress", 0x23 => "CloseHandle",
        0x24 => "WaitSynchronization1", 0x25 => "WaitSynchronizationN", 0x27 => "DuplicateHandle",
        0x28 => "GetSystemTick", 0x2A => "GetSystemInfo", 0x2B => "GetProcessInfo", 0x2C => "GetThreadInfo",
        0x2D => "ConnectToPort", 0x32 => "SendSyncRequest", 0x35 => "GetProcessId", 0x37 => "GetThreadId",
        0x38 => "GetResourceLimit", 0x39 => "GetResourceLimitLimitValues", 0x3A => "GetResourceLimitCurrentValues",
        0x3C => "Break", 0x3D => "OutputDebugString",
        _ => "unknown",
    }
}
