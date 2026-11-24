//! VFPv2 emission (ARM state only: ARMv6K Thumb has no coprocessor instructions).
//!
//! # FP mode
//! Horizon starts threads with FPSCR = 0x03C00000 (FZ, DN, RMode = 0b11 round toward zero), but the game's own
//! FP setup (`vmsr fpscr, #0x03000000` at 0x122A60, the only FPSCR write in code.bin) runs from its CRT init
//! (0x106F58, called from the entry point) and from its thread-entry wrapper (0x132AA4) before any FP code. So
//! the mode the game's FP code observes is 0x03000000: flush-to-zero (FZ), default NaN (DN), round to nearest,
//! short vectors LEN = 1 / STRIDE = 1 (assumed, never checked). The generated code implements exactly that mode
//! with explicit checks in the `xr_abi.h` helpers (not with MXCSR FTZ/DAZ, which flush after rounding and keep
//! NaN payloads):
//! * operands: a denormal input reads as a zero of the same sign (`xr_in32`/`xr_in64`);
//! * results: any NaN result is the default NaN (0x7FC00000 / 0x7FF8000000000000); a result that is tiny
//!   *before rounding* (|exact| < smallest normal) becomes a zero of the same sign (`xr_out32`/`xr_out64`).
//!   f32 arithmetic is done in double (exact or innocuously double-rounded, so the f32 result is correctly
//!   rounded, and a tiny exact value is seen as such); f64 multiplication checks a product that rounded up to
//!   the smallest normal with an exact `fma` (a library call unless built with `-mfma`; only on that edge).
//! * VABS/VNEG/VMOV are bit operations (no flushing, NaN payloads kept), as in the architecture. VNMUL, VMLA,
//!   VMLS, VNMLA, VNMLS are the unfused compositions of the ARM pseudocode (product rounded, then negated/added).
//! * float -> int (VCVT, VCVTR) saturates like ARM (NaN -> 0); VCVT rounds toward zero, VCVTR uses
//!   FPSCR.RMode (bits 23:22) read at run time. Everything else always uses the 0x03000000 mode: other
//!   FZ/DN/RMode values written by VMSR (including Horizon's initial round-toward-zero, before the game's
//!   setup runs), vector LEN/STRIDE and trap enables are ignored. Cumulative exception flags (FPSCR bits 7,
//!   4:0) are not maintained.
//! * The host must keep SSE2 arithmetic in its default state (MXCSR round-to-nearest, FTZ/DAZ off, exceptions
//!   masked) and the code must not be built with -ffast-math; `xr_abi.h` turns FP contraction off.
//!
//! # Instructions
//! VCMP/VCMPE set FPSCR.NZCV (unordered = 0011); `VMRS APSR_nzcv, FPSCR` copies them into the flag locals.
//! VLDR/VSTR/VLDM/VSTM/VPUSH/VPOP use the 32-bit load/store helpers, a double as two words (low word at the lower
//! address). Inline misses: VMRS/VMSR of registers other than FPSCR (FPSID/FPEXC), VMSR from PC, and the
//! UNPREDICTABLE forms (VMOV with PC, VMOV pair of S31, VMOV to the same register twice, VLDM/VSTM PC!).
use crate::flags::{C, N, V, Z};
use crate::Kind;
use std::fmt::Write;
use xr_arm::{Cond, Reg, VfpArith, VfpInsn, VfpUnary};

pub(crate) fn cond(v: &VfpInsn) -> Cond {
    use VfpInsn::*;
    match *v {
        Arith { cond, .. } | Unary { cond, .. } | MovImm { cond, .. } | Cmp { cond, .. } | CvtFloat { cond, .. }
        | CvtToFloat { cond, .. } | CvtToInt { cond, .. } | MovCoreSingle { cond, .. } | MovCoreScalar { cond, .. }
        | MovPair { cond, .. } | Mem { cond, .. } | Block { cond, .. } | Vmrs { cond, .. } | Vmsr { cond, .. } => cond,
    }
}

pub(crate) fn kind(v: &VfpInsn) -> Kind {
    use VfpInsn::*;
    let pc = |r: Reg| r == Reg::PC;
    match *v {
        MovCoreSingle { rt, .. } | MovCoreScalar { rt, .. } if pc(rt) => Kind::Miss,
        MovPair { rt, rt2, .. } if pc(rt) || pc(rt2) => Kind::Miss,
        MovPair { dp: false, m: 31, .. } => Kind::Miss,
        MovPair { to_core: true, rt, rt2, .. } if rt == rt2 => Kind::Miss,
        Block { rn: Reg::PC, writeback: true, .. } => Kind::Miss,
        Vmrs { reg, .. } if reg != 1 => Kind::Miss,
        Vmsr { reg, rt, .. } if reg != 1 || pc(rt) => Kind::Miss,
        _ => Kind::Plain,
    }
}

/// Accessors for one precision: (read register i, write expression to register i, sign bit, op helper).
struct P {
    dp: bool,
}

impl P {
    fn get(&self, i: u8) -> String { if self.dp { format!("xr_vd(c, {i})") } else { format!("c->v.u[{i}]") } }
    fn set(&self, i: u8, e: &str) -> String { if self.dp { format!("xr_vd_set(c, {i}, {e});") } else { format!("c->v.u[{i}] = {e};") } }
    fn sign(&self) -> &'static str { if self.dp { "0x8000000000000000ull" } else { "0x80000000u" } }
    /// op: 0 add, 1 sub, 2 mul, 3 div, 4 sqrt (of a).
    fn op(&self, op: u32, a: &str, b: &str) -> String { format!("xr_fop{}({op}, {a}, {b})", if self.dp { 64 } else { 32 }) }
    fn unpack(&self, a: &str) -> String { format!("xr_in{}({a})", if self.dp { 64 } else { 32 }) }
}

/// The C statement(s) for one VFP instruction (its condition is applied by the caller). `need`: flags live
/// after it that it defines (only `VMRS APSR_nzcv` defines any).
/// `pc_base`: the C value of the PC base for `rn = PC` (insn + 8, base-relative in module mode).
pub(crate) fn emit(v: &VfpInsn, pc_base: &str, need: u8) -> String {
    use VfpInsn::*;
    let r = |reg: Reg| format!("r{}", reg.0 & 15);
    match *v {
        Arith { op, dp, d, n, m, .. } => {
            let p = P { dp };
            let (vd, vn, vm, s) = (p.get(d), p.get(n), p.get(m), p.sign());
            let prod = p.op(2, &vn, &vm);
            let e = match op {
                VfpArith::Add => p.op(0, &vn, &vm),
                VfpArith::Sub => p.op(1, &vn, &vm),
                VfpArith::Mul => prod,
                VfpArith::Div => p.op(3, &vn, &vm),
                VfpArith::Nmul => format!("({prod} ^ {s})"),
                VfpArith::Mla => p.op(0, &vd, &prod),
                VfpArith::Mls => p.op(0, &vd, &format!("{prod} ^ {s}")),
                VfpArith::Nmla => p.op(0, &format!("{vd} ^ {s}"), &format!("{prod} ^ {s}")),
                VfpArith::Nmls => p.op(0, &format!("{vd} ^ {s}"), &prod),
            };
            p.set(d, &e)
        }
        Unary { op, dp, d, m, .. } => {
            let p = P { dp };
            let (vm, s) = (p.get(m), p.sign());
            let e = match op {
                VfpUnary::Mov => vm,
                VfpUnary::Abs => format!("({vm} & ~{s})"),
                VfpUnary::Neg => format!("({vm} ^ {s})"),
                VfpUnary::Sqrt => p.op(4, &vm, "0"),
            };
            p.set(d, &e)
        }
        MovImm { dp: true, d, bits, .. } => P { dp: true }.set(d, &format!("0x{bits:X}ull")),
        MovImm { d, bits, .. } => P { dp: false }.set(d, &format!("0x{:X}u", bits as u32)),
        Cmp { dp, d, m, zero, .. } => {
            let p = P { dp };
            let b = if zero { p.unpack("0") } else { p.unpack(&p.get(m)) };
            format!("c->fpscr = (c->fpscr & 0x0FFFFFFFu) | xr_fcmp({}, {b}) << 28;", p.unpack(&p.get(d)))
        }
        CvtFloat { to_dp: true, d, m, .. } => format!("xr_vd_set(c, {d}, xr_out64(xr_in32(c->v.u[{m}])));"),
        CvtFloat { d, m, .. } => format!("c->v.u[{d}] = xr_out32(xr_in64(xr_vd(c, {m})));"),
        CvtToFloat { dp, signed, d, m, .. } => {
            let src = if signed { format!("(int32_t)c->v.u[{m}]") } else { format!("c->v.u[{m}]") };
            if dp { format!("xr_vd_set(c, {d}, xr_du((double){src}));") } else { format!("c->v.u[{d}] = xr_fu((float){src});") }
        }
        CvtToInt { dp, signed, round_zero, d, m, .. } => {
            let p = P { dp };
            let rm = if round_zero { "3u" } else { "(c->fpscr >> 22) & 3u" };
            format!("c->v.u[{d}] = xr_ftoi({}, {}, {rm});", p.unpack(&p.get(m)), signed as u32)
        }
        MovCoreSingle { to_core, rt, n, .. } => mov_core(to_core, &r(rt), n as u32),
        MovCoreScalar { to_core, rt, d, hi, .. } => mov_core(to_core, &r(rt), 2 * d as u32 + hi as u32),
        MovPair { to_core, dp, rt, rt2, m, .. } => {
            let lo = if dp { 2 * m as u32 } else { m as u32 };
            if to_core {
                format!("{{ uint32_t a = c->v.u[{lo}], b = c->v.u[{}]; {} = a; {} = b; }}", lo + 1, r(rt), r(rt2))
            } else {
                format!("c->v.u[{lo}] = {}; c->v.u[{}] = {};", r(rt), lo + 1, r(rt2))
            }
        }
        Mem { load, dp, d, rn, offset, up, .. } => {
            let base = if rn == Reg::PC { pc_base.to_string() } else { r(rn) };
            let first = if dp { 2 * d as u32 } else { d as u32 };
            let mut s = format!("{{ uint32_t a = {base} {} {offset}u;", if up { "+" } else { "-" });
            for k in 0..(1 + dp as u32) {
                if load {
                    let _ = write!(s, " c->v.u[{}] = xr_ld32(m, a + {}u);", first + k, 4 * k);
                } else {
                    let _ = write!(s, " xr_st32(m, a + {}u, c->v.u[{}]);", 4 * k, first + k);
                }
            }
            s.push_str(" }");
            s
        }
        Block { load, dp, rn, first, count, increment, writeback, .. } => {
            let words = count as u32 * if dp { 2 } else { 1 };
            let first = if dp { 2 * first as u32 } else { first as u32 };
            let base = if rn == Reg::PC { pc_base.to_string() } else { r(rn) };
            let start = if increment { "b".to_string() } else { format!("b - {}u", 4 * words) };
            let mut s = format!("{{ uint32_t b = {base}, a = {start};");
            if writeback {
                let _ = write!(s, " {} = b {} {}u;", r(rn), if increment { "+" } else { "-" }, 4 * words);
            }
            let body = if load { format!("c->v.u[{first} + k] = xr_ld32(m, a + 4 * k)") } else { format!("xr_st32(m, a + 4 * k, c->v.u[{first} + k])") };
            let _ = write!(s, " for (uint32_t k = 0; k < {words}u; k++) {body}; }}");
            s
        }
        Vmrs { rt: Reg::PC, .. } => {
            let mut s = String::new();
            for (f, name, bit) in [(N, "n", 31), (Z, "z", 30), (C, "c", 29), (V, "v", 28)] {
                if need & f != 0 {
                    let _ = write!(s, " fl_{name} = (c->fpscr >> {bit}) & 1;");
                }
            }
            s.trim_start().to_string()
        }
        Vmrs { rt, .. } => format!("{} = c->fpscr;", r(rt)),
        Vmsr { rt, .. } => format!("c->fpscr = {};", r(rt)),
    }
}

fn mov_core(to_core: bool, rt: &str, i: u32) -> String {
    if to_core { format!("{rt} = c->v.u[{i}];") } else { format!("c->v.u[{i}] = {rt};") }
}
