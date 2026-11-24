//! The virtual clock: Zakuro's cycle count (`System.cpu.cycles`, 268 MHz ticks). The interpreter adds 1 per
//! instruction itself (zakuro-cpu `step`); native code is charged here, from its back-edge budget and per SVC.
//!
//! The native charging model (the default for --deterministic and so --script runs; XR_NATIVE_CLOCK=1 turns it on
//! for any run, XR_NATIVE_CLOCK=0 off): interpreted code is charged like native code instead: no cycle
//! per instruction, the same `XrCpu.budget` paid at the same back-edges as the generated code (`pays_budget`), the
//! same SVC charge, and no idle-loop skips. The budget then runs on unbroken across native/interpreter switches.
//! Guest time is a function of the guest's own execution, the same whichever functions are compiled (with or
//! without XR_INTERP_ALL), so two such runs must match exactly unless the generated code computes something different.
use crate::abi::XrCpu;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use xr_arm::{decode_arm, decode_thumb, Cond, DpOp, Insn, MemWidth, Offset, Operand2, Reg, Shift};
use xr_hle::System;

/// Back-edges a native thread runs between `preempt` calls.
pub const BUDGET: i32 = 1024;
/// Cycles one native loop iteration is taken to cost. BUDGET * this = 8192 = Zakuro's preemption quantum.
pub const CYCLES_PER_BACKEDGE: u64 = 8;
/// Cycles charged per SVC from native code, standing in for the straight-line code around it (no back-edges).
pub const CYCLES_PER_SVC: u64 = 64;

static NATIVE: AtomicBool = AtomicBool::new(false);

/// Interpreted code is charged like native code (see the module docs).
#[inline(always)]
pub fn native_clock() -> bool {
    NATIVE.load(Ordering::Relaxed)
}

/// Picks the charging model, once at start, before boot: the native one for deterministic runs (a script's hashes
/// then do not depend on which functions are compiled), the interpreter's own otherwise; XR_NATIVE_CLOCK=1 or =0
/// overrides either.
pub fn init(deterministic: bool) -> bool {
    let on = match std::env::var("XR_NATIVE_CLOCK").as_deref() {
        Ok("0") => false,
        Ok(_) => true,
        Err(_) => deterministic,
    };
    NATIVE.store(on, Ordering::Relaxed);
    on
}

/// Charge the back-edges `c` used since its last refill to the clock and refill.
pub fn charge(sys: &mut System, c: &mut XrCpu) {
    let used = (BUDGET - c.budget.min(BUDGET)) as u64;
    sys.cpu.cycles += used * CYCLES_PER_BACKEDGE;
    c.budget = BUDGET;
}

/// `charge` at a switch between native code and the interpreter; under the native model the budget runs on.
pub fn charge_switch(sys: &mut System, c: &mut XrCpu) {
    if !native_clock() {
        charge(sys, c);
    }
}

/// XR_CLOCKLOG=FROM,TO (cycles, decimal): log the clock, thread, SVC and r0/lr at every SVC in that window, to find
/// where two runs' clocks part.
pub fn trace_svc(sys: &System, imm: u32) {
    let t = sys.cpu.cycles;
    if in_window(t) {
        let r = &sys.cpu.regs;
        crate::log::log(&format!("clocklog {t} t{:?} svc 0x{imm:02X} r0 0x{:08X} lr 0x{:08X}", sys.kernel.current_thread, r[0], r[14]));
    }
}

/// `t` is inside XR_CLOCKLOG's window.
fn in_window(t: u64) -> bool {
    static WIN: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    let win = WIN.get_or_init(|| {
        let v = std::env::var("XR_CLOCKLOG").ok()?;
        let (a, b) = v.split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    });
    win.is_some_and(|(a, b)| (a..b).contains(&t))
}

/// The interpreted instruction at `at` just ran (`jumped`: PC is not the next instruction; `cpsr`: before it ran):
/// would the generated code have paid the back-edge budget for it (xr-codegen `classify` + `emit::insn`)? A taken
/// backward `B` (target <= its own address); any computed jump except returns (`BX LR`, `MOV PC, LR`, `LDM`/`POP`
/// with PC, `LDR PC, [SP], #4`) and indirect calls (`BLX Rm`, ARM `MOV LR, PC` then a PC write), including one that
/// lands on the next instruction (jump-table entries for the default case do). Calls never pay.
pub fn pays_budget(mut word: impl FnMut(u32) -> Option<u32>, at: u32, thumb: bool, jumped: bool, cpsr: u32) -> bool {
    let w = word(at & !1).unwrap_or(0);
    if !jumped {
        // only a computed jump that ran can land on the next instruction: BX Rm, or an ARM data-processing or load
        // with Rd = PC whose condition passed
        let bx = if thumb { w as u16 & 0xFF87 == 0x4700 } else { w & 0x0FFF_FFF0 == 0x012F_FF10 };
        let rd_pc = !thumb && (w >> 26) & 3 <= 1 && (w >> 12) & 15 == 15;
        if !(bx || rd_pc) || !(thumb || cond_passes(w >> 28, cpsr)) {
            return false;
        }
    }
    let w = word(at & !1).unwrap_or(0);
    let insn = if thumb {
        let h = w as u16;
        // the second half of a BL/BLX pair: zakuro steps the pair as two halves, codegen decodes it as one call
        if h >> 11 == 0b11111 || h >> 11 == 0b11101 {
            return false;
        }
        decode_thumb(h, (w >> 16) as u16, at).0
    } else {
        decode_arm(w, at)
    };
    let mut mov_lr_pc_before = || {
        if thumb || cond_of(&insn) != Cond::Al {
            return false;
        }
        let p = at.wrapping_sub(4);
        word(p).is_some_and(|w| matches!(decode_arm(w, p), Insn::DataProc { cond: Cond::Al, op: DpOp::Mov, s: false, rd: Reg::LR,
            op2: Operand2::RegImmShift { rm: Reg::PC, shift: Shift::Lsl, amount: 0 }, .. }))
    };
    let mov_lr = |op2: Operand2| op2 == Operand2::RegImmShift { rm: Reg::LR, shift: Shift::Lsl, amount: 0 };
    match insn {
        Insn::Branch { link: false, target, .. } => target <= at & !1,
        Insn::Branch { link: true, .. } | Insn::BlxImm { .. } => false,
        Insn::BranchExchange { link: false, rm: Reg::LR, .. } => false,
        Insn::BranchExchange { link: false, .. } => !mov_lr_pc_before(),
        Insn::BranchExchange { link: true, .. } => false,
        Insn::DataProc { op: DpOp::Mov, rd: Reg::PC, op2, .. } if mov_lr(op2) => false,
        Insn::DataProc { op: DpOp::Tst | DpOp::Teq | DpOp::Cmp | DpOp::Cmn, .. } => false,
        Insn::DataProc { rd: Reg::PC, .. } => !mov_lr_pc_before(),
        Insn::Load { width: MemWidth::Word, rt: Reg::PC, rn, addr, .. } => {
            let pop = rn == Reg::SP && !addr.pre && addr.up && addr.offset == Offset::Imm(4);
            !pop && !mov_lr_pc_before()
        }
        Insn::LoadMulti { list, .. } if list & 0x8000 != 0 => false,
        _ => false,
    }
}

/// ARM condition `c` against the N Z C V bits of `cpsr`.
fn cond_passes(c: u32, cpsr: u32) -> bool {
    let (n, z, cy, v) = (cpsr >> 31 & 1 != 0, cpsr >> 30 & 1 != 0, cpsr >> 29 & 1 != 0, cpsr >> 28 & 1 != 0);
    match c {
        0 => z,
        1 => !z,
        2 => cy,
        3 => !cy,
        4 => n,
        5 => !n,
        6 => v,
        7 => !v,
        8 => cy && !z,
        9 => !cy || z,
        10 => n == v,
        11 => n != v,
        12 => !z && n == v,
        13 => z || n != v,
        _ => true,
    }
}

fn cond_of(i: &Insn) -> Cond {
    match *i {
        Insn::DataProc { cond, .. } | Insn::Load { cond, .. } | Insn::BranchExchange { cond, .. } | Insn::Branch { cond, .. } => cond,
        _ => Cond::Al,
    }
}

#[cfg(test)]
mod tests {
    use super::pays_budget;

    /// One word of code at 0x1000 (and `before` at 0xFFC): does it pay, as xr-codegen's emitter would?
    fn pays(before: u32, w: u32, thumb: bool) -> bool {
        pays_budget(|a| Some(if a == 0x1000 { w } else { before }), 0x1000, thumb, true, 0)
    }

    #[test]
    fn back_edges_as_generated_code_pays_them() {
        let nop = 0xE1A0_0000;
        assert!(pays(nop, 0xEAFF_FFFE, false), "b . (backward B)");
        assert!(pays(nop, 0x1AFF_FFF0, false), "bne back");
        assert!(!pays(nop, 0xEA00_0010, false), "forward B");
        assert!(!pays(nop, 0xEBFF_FFF0, false), "BL back is a call");
        assert!(!pays(nop, 0xE12F_FF1E, false), "bx lr returns");
        assert!(pays(nop, 0xE12F_FF13, false), "bx r3 is a computed jump");
        assert!(!pays(nop, 0xE12F_FF33, false), "blx r3 is a call");
        assert!(!pays(nop, 0xE1A0_F00E, false), "mov pc, lr returns");
        assert!(pays(nop, 0xE79F_F100, false), "ldr pc, [pc, r0, lsl #2] (jump table)");
        assert!(!pays(0xE1A0_E00F, 0xE59F_F000, false), "mov lr, pc; ldr pc, [..] is a call");
        assert!(!pays(nop, 0xE49D_F004, false), "ldr pc, [sp], #4 returns");
        assert!(!pays(nop, 0xE8BD_8010, false), "pop {{r4, pc}} returns");
        assert!(pays(nop, 0xE7FE, true), "thumb b .");
        assert!(!pays(nop, 0xBD10, true), "thumb pop {{r4, pc}}");
        assert!(!pays(nop, 0x4770, true), "thumb bx lr");
        assert!(pays(nop, 0x4718, true), "thumb bx r3");
        assert!(!pays(nop, 0xF800, true), "thumb BL second half");
    }

    #[test]
    fn a_computed_jump_to_the_next_instruction_pays_too() {
        let at = |w: u32, cpsr: u32| pays_budget(|a| Some(if a == 0x1000 { w } else { 0xE1A0_0000 }), 0x1000, false, false, cpsr);
        assert!(at(0xE79F_F100, 0), "ldr pc, [pc, r0, lsl #2] landing on the default case");
        assert!(at(0x379F_F100, 0), "ldrlo (C clear) ran");
        assert!(!at(0x379F_F100, 1 << 29), "ldrlo with C set did not run");
        assert!(!at(0x1AFF_FFF0, 0), "a backward bne that was not taken");
        assert!(!at(0xE12F_FF1E, 0), "bx lr is a return");
        assert!(!at(0xE150_F000, 0), "a compare never writes PC");
    }
}
