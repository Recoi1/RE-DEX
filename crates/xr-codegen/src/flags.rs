//! Lazy NZCV: backward liveness of each flag over the function's control-flow graph.
//!
//! An instruction computes a flag only if that flag is live after it (some path reads it before it is
//! overwritten). Successors are the fall-through, direct `goto` targets and, for computed jumps, every
//! instruction (the dispatch switch). Exits (returns, tail calls, misses) have no successors; what they read
//! comes from the `FlagTable`: a direct call or tail call (or falling off the end) reads the target's entry
//! flags, a return reads the flags callers read after calling this function. Outside `strict` mode, indirect
//! calls, the dispatch's `xr_lookup` tail call and misses (inline, or a direct call/tail call/fall-off to an
//! unlisted target) read all four (stored for the unknown target or the interpreter). A conditional
//! instruction reads its condition's flags and does not kill what it defines.
use crate::{cond_of, emit, writes_rd, FlagTable, Func, Kind};
use xr_arm::{Cond, DpOp, Insn, Offset, Operand2, Reg, Shift, VfpInsn};

pub(crate) const N: u8 = 8;
pub(crate) const Z: u8 = 4;
pub(crate) const C: u8 = 2;
pub(crate) const V: u8 = 1;
pub(crate) const ALL: u8 = 15;

pub(crate) struct Liveness {
    /// Flags live after each instruction.
    pub out: Vec<u8>,
    /// Flags live at the entry (loaded from `c->cpsr`).
    pub entry: u8,
}

pub(crate) fn cond_use(c: Cond) -> u8 {
    use Cond::*;
    match c {
        Eq | Ne => Z,
        Cs | Cc => C,
        Mi | Pl => N,
        Vs | Vc => V,
        Hi | Ls => C | Z,
        Ge | Lt => N | V,
        Gt | Le => N | Z | V,
        Al => 0,
    }
}

pub(crate) fn is_logical(op: DpOp) -> bool {
    use DpOp::*;
    matches!(op, And | Eor | Tst | Teq | Orr | Mov | Bic | Mvn)
}

/// (flags read, flags written) by the instruction body, not counting its condition.
pub(crate) fn use_def(insn: &Insn, kind: Kind) -> (u8, u8) {
    use Insn::*;
    if matches!(kind, Kind::Call(..) | Kind::CallInd) {
        return (0, ALL); // the callee may change them: reloaded from c->cpsr when needed
    }
    if kind == Kind::Miss {
        return (0, 0);
    }
    match *insn {
        DataProc { op, s, rd, op2, .. } => {
            let mut u = if matches!(op, DpOp::Adc | DpOp::Sbc | DpOp::Rsc) { C } else { 0 };
            let (reads_c, carry_out) = match op2 {
                Operand2::Imm { rot, .. } => (false, rot != 0),
                Operand2::RegImmShift { shift, amount, .. } => (shift == Shift::Rrx, emit::imm_shift("v", shift, amount).1.is_some()),
                Operand2::RegRegShift { shift, .. } => (shift == Shift::Rrx, true),
            };
            if reads_c {
                u |= C;
            }
            if !s || (writes_rd(op) && rd == Reg::PC) {
                return (u, 0);
            }
            if !is_logical(op) {
                return (u, ALL);
            }
            if !carry_out {
                return (u, N | Z);
            }
            // A register-specified shift by 0 keeps C, so it reads C as well.
            if matches!(op2, Operand2::RegRegShift { .. }) {
                u |= C;
            }
            (u, N | Z | C)
        }
        Mul { s: true, .. } | MulLong { s: true, .. } => (0, N | Z),
        Load { addr, .. } | Store { addr, .. } if matches!(addr.offset, Offset::Reg { shift: Shift::Rrx, .. }) => (C, 0),
        Mrs { spsr: false, .. } => (ALL, 0),
        Msr { spsr: false, mask, .. } => (0, if mask & 8 != 0 { ALL } else { 0 }),
        Vfp(VfpInsn::Vmrs { reg: 1, rt: Reg::PC, .. }) => (0, ALL), // VMRS APSR_nzcv, FPSCR
        _ => (0, 0),
    }
}

/// `strict`: direct edges only (the inter-procedural `FlagTable` fixed point); otherwise indirect transfers
/// read all four flags too (emission).
pub(crate) fn liveness(fx: &Func, tab: &FlagTable, strict: bool) -> Liveness {
    let n = fx.insns.len();
    let ind = if strict { 0 } else { ALL };
    // An unlisted direct target is a miss: the interpreter reads all four flags.
    let entry_of = |t: u32, m| fx.target(t, m).map_or(ind, |e| tab.entry(e.start, e.mode));
    let info: Vec<(u8, u8)> = (0..n)
        .map(|i| {
            let insn = &fx.insns[i].2;
            let cond = cond_of(insn);
            let (u, d) = use_def(insn, fx.kinds[i]);
            let exit = match fx.kinds[i] {
                Kind::Call(t, m) => entry_of(t, m),
                Kind::Leave(t) => entry_of(t, fx.f.mode),
                Kind::CallInd | Kind::Miss => ind,
                Kind::Return => tab.ret(fx.f.start, fx.f.mode),
                _ => 0,
            };
            (u | exit | cond_use(cond), if cond == Cond::Al { d } else { 0 })
        })
        .collect();
    let fall_off = if fx.falls_off() { entry_of(fx.fall_target(), fx.f.mode) } else { 0 };
    let succ: Vec<_> = (0..n).map(|i| fx.succ(i)).collect();
    let mut live_in = vec![0u8; n];
    let mut out = vec![0u8; n];
    loop {
        let mut changed = false;
        let dispatch = live_in.iter().fold(ind, |a, &b| a | b);
        for i in (0..n).rev() {
            let s = &succ[i];
            let mut o = 0;
            if s.fall {
                o |= if i + 1 < n { live_in[i + 1] } else { fall_off };
            }
            if let Some(t) = s.goto {
                o |= live_in[t];
            }
            if s.dispatch {
                o |= dispatch;
            }
            let (u, kill) = info[i];
            let li = u | (o & !kill);
            if o != out[i] || li != live_in[i] {
                changed = true;
                out[i] = o;
                live_in[i] = li;
            }
        }
        if !changed {
            break;
        }
    }
    Liveness { entry: live_in.first().copied().unwrap_or(0), out }
}
