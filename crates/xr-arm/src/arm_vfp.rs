//! VFPv2 decoding for coprocessors 10 (single) and 11 (double). Called with bits 11:8 in {0xA, 0xB}.
use crate::arm::{bit, bits, reg};
use crate::insn::*;

/// Register index from a 4-bit field at `lo` plus extension bit `ext`. Doubles reject D16..D31 (not in VFPv2).
fn vreg(w: u32, dp: bool, lo: u32, ext: u32) -> Option<u8> {
    let (f, e) = (bits(w, lo + 3, lo), bit(w, ext));
    if dp { if e { None } else { Some(f as u8) } } else { Some((f << 1 | e as u32) as u8) }
}

pub(crate) fn decode_vfp(w: u32, c: Cond) -> Insn {
    let dp = bit(w, 8);
    let v = match bits(w, 27, 24) {
        0xE if !bit(w, 4) => data(w, c, dp),
        0xE => xfer(w, c, dp),
        0xC | 0xD => ldst(w, c, dp),
        _ => None,
    };
    v.map_or(Insn::Undefined { word: w }, Insn::Vfp)
}

fn expand_imm(imm8: u32, dp: bool) -> u64 {
    let (sign, b6, rest) = ((imm8 >> 7) as u64, (imm8 >> 6) & 1, (imm8 & 0x3F) as u64);
    if dp {
        sign << 63 | ((b6 ^ 1) as u64) << 62 | (if b6 != 0 { 0xFF } else { 0 }) << 54 | rest << 48
    } else {
        sign << 31 | ((b6 ^ 1) as u64) << 30 | (if b6 != 0 { 0x1F } else { 0 }) << 25 | rest << 19
    }
}

fn data(w: u32, c: Cond, dp: bool) -> Option<VfpInsn> {
    let d = vreg(w, dp, 12, 22); // None for D16+ doubles; single-dest conversions ignore it
    let n = vreg(w, dp, 16, 7);
    let m = vreg(w, dp, 0, 5);
    let s = bit(w, 6);
    let sel = (bit(w, 23) as u32) << 2 | (bits(w, 21, 20));
    if sel != 7 {
        use VfpArith::*;
        let op = match (sel, s) {
            (0, false) => Mla, (0, true) => Mls,
            (1, false) => Nmls, (1, true) => Nmla,
            (2, false) => Mul, (2, true) => Nmul,
            (3, false) => Add, (3, true) => Sub,
            (4, false) => Div,
            _ => return None,
        };
        return Some(VfpInsn::Arith { cond: c, op, dp, d: d?, n: n?, m: m? });
    }
    if !s {
        return Some(VfpInsn::MovImm { cond: c, dp, d: d?, bits: expand_imm(bits(w, 19, 16) << 4 | bits(w, 3, 0), dp) });
    }
    let (opc2, b7) = (bits(w, 19, 16), bit(w, 7));
    let unary = |op| Some(VfpInsn::Unary { cond: c, op, dp, d: d?, m: m? });
    match opc2 {
        0 => unary(if b7 { VfpUnary::Abs } else { VfpUnary::Mov }),
        1 => unary(if b7 { VfpUnary::Sqrt } else { VfpUnary::Neg }),
        4 => Some(VfpInsn::Cmp { cond: c, dp, e: b7, d: d?, m: m?, zero: false }),
        5 => Some(VfpInsn::Cmp { cond: c, dp, e: b7, d: d?, m: 0, zero: true }),
        7 if b7 => {
            // sz=1: double source to single dest; sz=0: single source to double dest.
            let (dd, mm) = if dp { (vreg(w, false, 12, 22)?, vreg(w, true, 0, 5)?) } else { (vreg(w, true, 12, 22)?, vreg(w, false, 0, 5)?) };
            Some(VfpInsn::CvtFloat { cond: c, to_dp: !dp, d: dd, m: mm })
        }
        8 => Some(VfpInsn::CvtToFloat { cond: c, dp, signed: b7, d: d?, m: vreg(w, false, 0, 5)? }),
        12 | 13 => Some(VfpInsn::CvtToInt { cond: c, dp, signed: opc2 == 13, round_zero: b7, d: vreg(w, false, 12, 22)?, m: m? }),
        _ => None,
    }
}

/// Register transfers (bits 27:24 = 1110, bit 4 = 1).
fn xfer(w: u32, c: Cond, dp: bool) -> Option<VfpInsn> {
    let (to_core, rt) = (bit(w, 20), reg(w, 12));
    if bits(w, 6, 5) != 0 || bits(w, 3, 0) != 0 { return None; }
    if !dp {
        match bits(w, 23, 21) {
            0 => Some(VfpInsn::MovCoreSingle { cond: c, to_core, rt, n: vreg(w, false, 16, 7)? }),
            7 if !bit(w, 7) => {
                let r = bits(w, 19, 16) as u8;
                Some(if to_core { VfpInsn::Vmrs { cond: c, reg: r, rt } } else { VfpInsn::Vmsr { cond: c, reg: r, rt } })
            }
            _ => None,
        }
    } else if bits(w, 23, 22) == 0 {
        Some(VfpInsn::MovCoreScalar { cond: c, to_core, rt, d: vreg(w, true, 16, 7)?, hi: bit(w, 21) })
    } else { None }
}

/// Loads/stores and 64-bit transfers (bits 27:25 = 110).
fn ldst(w: u32, c: Cond, dp: bool) -> Option<VfpInsn> {
    let (p, u, wb, load) = (bit(w, 24), bit(w, 23), bit(w, 21), bit(w, 20));
    let rn = reg(w, 16);
    if bits(w, 27, 21) == 0b1100010 {
        if bits(w, 7, 6) != 0 || !bit(w, 4) { return None; }
        return Some(VfpInsn::MovPair { cond: c, to_core: load, dp, rt: reg(w, 12), rt2: rn, m: vreg(w, dp, 0, 5)? });
    }
    let d = vreg(w, dp, 12, 22)?;
    let imm8 = bits(w, 7, 0);
    match (p, u, wb) {
        (true, _, false) => Some(VfpInsn::Mem { cond: c, load, dp, d, rn, offset: (imm8 * 4) as u16, up: u }),
        (false, true, _) | (true, false, true) => {
            let count = if dp { if imm8 & 1 != 0 { return None; } imm8 / 2 } else { imm8 };
            let limit = if dp { 16 } else { 32 };
            if count == 0 || d as u32 + count > limit { return None; }
            Some(VfpInsn::Block { cond: c, load, dp, rn, first: d, count: count as u8, increment: !p, writeback: wb })
        }
        _ => None,
    }
}
