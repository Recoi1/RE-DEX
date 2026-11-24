//! ARM-state (A32) decoder for ARMv6K. Total: any `u32` yields an `Insn`, never a panic.
use crate::arm_vfp::decode_vfp;
use crate::insn::*;

pub(crate) fn bits(w: u32, hi: u32, lo: u32) -> u32 { (w >> lo) & ((1u32 << (hi - lo + 1)) - 1) }
pub(crate) fn bit(w: u32, n: u32) -> bool { (w >> n) & 1 != 0 }
pub(crate) fn reg(w: u32, lo: u32) -> Reg { Reg(((w >> lo) & 0xF) as u8) }

/// Decode one ARM instruction word fetched at `pc`.
pub fn decode_arm(w: u32, pc: u32) -> Insn {
    let cond = w >> 28;
    if cond == 0xF {
        return decode_uncond(w, pc);
    }
    let c = Cond::from_bits(cond);
    match bits(w, 27, 25) {
        0b000 => dec_000(w, c),
        0b001 => dec_001(w, c),
        0b010 => dec_ldst(w, c, false),
        0b011 => if bit(w, 4) { dec_media(w, c) } else { dec_ldst(w, c, true) },
        0b100 => dec_block(w, c),
        0b101 => Insn::Branch { link: bit(w, 24), target: branch_target(w, pc), cond: c },
        _ => dec_copro(w, c),
    }
}

fn undef(w: u32) -> Insn { Insn::Undefined { word: w } }

/// pc + 8 + (sign-extended imm24 << 2).
fn branch_target(w: u32, pc: u32) -> u32 {
    pc.wrapping_add(8).wrapping_add((((w << 8) as i32) >> 6) as u32)
}

fn shift_of(t: u32) -> Shift { [Shift::Lsl, Shift::Lsr, Shift::Asr, Shift::Ror][(t & 3) as usize] }

/// Normalise an immediate shift per the encoding rules (LSR/ASR #0 = #32, ROR #0 = RRX).
fn norm_shift(t: u32, imm5: u32) -> (Shift, u8) {
    match (shift_of(t), imm5) {
        (Shift::Lsr, 0) => (Shift::Lsr, 32),
        (Shift::Asr, 0) => (Shift::Asr, 32),
        (Shift::Ror, 0) => (Shift::Rrx, 1),
        (s, n) => (s, n as u8),
    }
}

fn dp_op(w: u32) -> DpOp {
    use DpOp::*;
    [And, Eor, Sub, Rsb, Add, Adc, Sbc, Rsc, Tst, Teq, Cmp, Cmn, Orr, Mov, Bic, Mvn][bits(w, 24, 21) as usize]
}

fn rot_imm(w: u32) -> (u32, u8) {
    let rot = bits(w, 11, 8) * 2;
    ((w & 0xFF).rotate_right(rot), rot as u8)
}

fn data_proc(w: u32, c: Cond, imm: bool) -> Insn {
    let op2 = if imm {
        let (value, rot) = rot_imm(w);
        Operand2::Imm { value, rot }
    } else if bit(w, 4) {
        Operand2::RegRegShift { rm: reg(w, 0), shift: shift_of(bits(w, 6, 5)), rs: reg(w, 8) }
    } else {
        let (shift, amount) = norm_shift(bits(w, 6, 5), bits(w, 11, 7));
        Operand2::RegImmShift { rm: reg(w, 0), shift, amount }
    };
    Insn::DataProc { cond: c, op: dp_op(w), s: bit(w, 20), rd: reg(w, 12), rn: reg(w, 16), op2 }
}

/// Bits 27:25 = 000.
fn dec_000(w: u32, c: Cond) -> Insn {
    if bit(w, 4) && bit(w, 7) {
        return if bits(w, 6, 5) == 0 { dec_mul_sync(w, c) } else { dec_extra_ldst(w, c) };
    }
    // TST/TEQ/CMP/CMN opcodes with S=0 are the miscellaneous space.
    if bits(w, 24, 23) == 2 && !bit(w, 20) { dec_misc(w, c) } else { data_proc(w, c, false) }
}

fn dec_misc(w: u32, c: Cond) -> Insn {
    let (r22, r21) = (bit(w, 22), bit(w, 21));
    let media = |op, ra: u32, x, y| Insn::Media { cond: c, op, rd: reg(w, 16), rn: reg(w, 0), rm: reg(w, 8), ra: reg(w, ra), imm: 0, amount: 0, x, y };
    match bits(w, 7, 4) {
        0b0000 => if !r21 { Insn::Mrs { cond: c, rd: reg(w, 12), spsr: r22 } } else {
            Insn::Msr { cond: c, spsr: r22, mask: bits(w, 19, 16) as u8, src: MsrSrc::Reg(reg(w, 0)) }
        },
        0b0001 if !r22 && r21 => Insn::BranchExchange { link: false, rm: reg(w, 0), cond: c },
        0b0001 if r22 && r21 => Insn::Media { cond: c, op: MediaOp::Clz, rd: reg(w, 12), rn: Reg(0), rm: reg(w, 0), ra: Reg(0), imm: 0, amount: 0, x: false, y: false },
        0b0010 if !r22 && r21 => Insn::BranchExchange { link: false, rm: reg(w, 0), cond: c }, // BXJ acts as BX
        0b0011 if !r22 && r21 => Insn::BranchExchange { link: true, rm: reg(w, 0), cond: c },
        0b0101 => {
            let op = [MediaOp::Qadd, MediaOp::Qsub, MediaOp::Qdadd, MediaOp::Qdsub][bits(w, 22, 21) as usize];
            Insn::Media { cond: c, op, rd: reg(w, 12), rn: reg(w, 16), rm: reg(w, 0), ra: Reg(0), imm: 0, amount: 0, x: false, y: false }
        }
        0b0111 if !r22 && r21 && c == Cond::Al => Insn::Bkpt { imm: ((bits(w, 19, 8) << 4) | bits(w, 3, 0)) as u16 },
        s if s & 9 == 8 => {
            let (x, y) = (bit(w, 5), bit(w, 6));
            match bits(w, 22, 21) {
                0 => media(MediaOp::Smlaxy, 12, x, y),
                1 => if x { media(MediaOp::Smulwy, 12, false, y) } else { media(MediaOp::Smlawy, 12, false, y) },
                2 => media(MediaOp::Smlalxy, 12, x, y),
                _ => media(MediaOp::Smulxy, 12, x, y),
            }
        }
        _ => undef(w),
    }
}

/// Multiplies and synchronisation primitives (bits 27:25 = 000, 7:4 = 1001).
fn dec_mul_sync(w: u32, c: Cond) -> Insn {
    let s = bit(w, 20);
    if !bit(w, 24) {
        let long = |signed, accumulate| Insn::MulLong { cond: c, signed, accumulate, s, rdhi: reg(w, 16), rdlo: reg(w, 12), rm: reg(w, 0), rs: reg(w, 8) };
        return match bits(w, 23, 21) {
            0 | 1 => Insn::Mul { cond: c, accumulate: bit(w, 21), s, rd: reg(w, 16), rn: reg(w, 12), rm: reg(w, 0), rs: reg(w, 8) },
            2 if !s => Insn::Umaal { cond: c, rdhi: reg(w, 16), rdlo: reg(w, 12), rm: reg(w, 0), rs: reg(w, 8) },
            4 => long(false, false),
            5 => long(false, true),
            6 => long(true, false),
            7 => long(true, true),
            _ => undef(w),
        };
    }
    if !bit(w, 23) {
        return if bits(w, 21, 20) == 0 {
            Insn::Swp { cond: c, byte: bit(w, 22), rd: reg(w, 12), rm: reg(w, 0), rn: reg(w, 16) }
        } else { undef(w) };
    }
    let width = [MemWidth::Word, MemWidth::Double, MemWidth::Byte, MemWidth::Half][bits(w, 22, 21) as usize];
    if bit(w, 20) {
        Insn::Ldrex { cond: c, width, rt: reg(w, 12), rn: reg(w, 16) }
    } else {
        Insn::Strex { cond: c, width, rd: reg(w, 12), rt: reg(w, 0), rn: reg(w, 16) }
    }
}

/// Halfword/signed/doubleword loads and stores (bits 27:25 = 000, bit 7 = bit 4 = 1, bits 6:5 != 0).
fn dec_extra_ldst(w: u32, c: Cond) -> Insn {
    let (p, load) = (bit(w, 24), bit(w, 20));
    let user = !p && bit(w, 21);
    let width = match (load, bits(w, 6, 5)) {
        (false, 1) => MemWidth::Half,
        (false, 2) if !user => MemWidth::Double, // LDRD
        (false, 3) if !user => MemWidth::Double, // STRD
        (true, 1) => MemWidth::Half,
        (true, 2) => MemWidth::SignedByte,
        (true, 3) => MemWidth::SignedHalf,
        _ => return undef(w),
    };
    let is_load = load || bits(w, 6, 5) == 2; // L=0, SH=10 is LDRD
    let offset = if bit(w, 22) {
        Offset::Imm((bits(w, 11, 8) << 4) | bits(w, 3, 0))
    } else if bits(w, 11, 8) == 0 {
        Offset::Reg { rm: reg(w, 0), shift: Shift::Lsl, amount: 0 }
    } else { return undef(w) };
    let addr = Addr { pre: p, up: bit(w, 23), writeback: !p || bit(w, 21), offset };
    let (rt, rn) = (reg(w, 12), reg(w, 16));
    if is_load { Insn::Load { cond: c, width, rt, rn, addr, user } } else { Insn::Store { cond: c, width, rt, rn, addr, user } }
}

/// Bits 27:25 = 001.
fn dec_001(w: u32, c: Cond) -> Insn {
    if bits(w, 24, 23) == 2 && !bit(w, 20) {
        if !bit(w, 21) { return undef(w); } // MOVW (ARMv6T2)
        let mask = bits(w, 19, 16) as u8;
        if mask == 0 {
            if bits(w, 15, 12) != 0xF { return undef(w); }
            let kind = match w & 0xFF { 1 => HintKind::Yield, 2 => HintKind::Wfe, 3 => HintKind::Wfi, 4 => HintKind::Sev, _ => HintKind::Nop };
            return Insn::Hint { cond: c, kind };
        }
        return Insn::Msr { cond: c, spsr: bit(w, 22), mask, src: MsrSrc::Imm(rot_imm(w).0) };
    }
    data_proc(w, c, true)
}

/// LDR/STR word/byte (bits 27:26 = 01).
fn dec_ldst(w: u32, c: Cond, reg_off: bool) -> Insn {
    let (p, byte, load) = (bit(w, 24), bit(w, 22), bit(w, 20));
    let offset = if reg_off {
        let (shift, amount) = norm_shift(bits(w, 6, 5), bits(w, 11, 7));
        Offset::Reg { rm: reg(w, 0), shift, amount }
    } else { Offset::Imm(w & 0xFFF) };
    let addr = Addr { pre: p, up: bit(w, 23), writeback: !p || bit(w, 21), offset };
    let width = if byte { MemWidth::Byte } else { MemWidth::Word };
    let (rt, rn, user) = (reg(w, 12), reg(w, 16), !p && bit(w, 21));
    if load { Insn::Load { cond: c, width, rt, rn, addr, user } } else { Insn::Store { cond: c, width, rt, rn, addr, user } }
}

fn dec_block(w: u32, c: Cond) -> Insn {
    let mode = match bits(w, 24, 23) { 0 => BlockMode::Da, 1 => BlockMode::Ia, 2 => BlockMode::Db, _ => BlockMode::Ib };
    let (rn, list, writeback, user_or_psr) = (reg(w, 16), w as u16, bit(w, 21), bit(w, 22));
    if bit(w, 20) { Insn::LoadMulti { cond: c, rn, list, mode, writeback, user_or_psr } }
    else { Insn::StoreMulti { cond: c, rn, list, mode, writeback, user_or_psr } }
}

/// Bits 27:26 = 11: coprocessor space (VFP on cp10/11, MCR/MRC otherwise) and SVC.
fn dec_copro(w: u32, c: Cond) -> Insn {
    if bits(w, 27, 24) == 0xF {
        return Insn::Svc { imm: w & 0xFF_FFFF, cond: c };
    }
    let cp = bits(w, 11, 8);
    if cp == 10 || cp == 11 {
        return decode_vfp(w, c);
    }
    if bits(w, 27, 24) == 0xE && bit(w, 4) {
        let (coproc, opc1, crn, crm, opc2, rt) = (cp as u8, bits(w, 23, 21) as u8, bits(w, 19, 16) as u8, bits(w, 3, 0) as u8, bits(w, 7, 5) as u8, reg(w, 12));
        return if bit(w, 20) { Insn::Mrc { cond: c, coproc, opc1, crn, crm, opc2, rt } } else { Insn::Mcr { cond: c, coproc, opc1, crn, crm, opc2, rt } };
    }
    undef(w) // CDP, LDC, STC: unused by this target
}

/// Unconditional space (cond = 0b1111).
fn decode_uncond(w: u32, pc: u32) -> Insn {
    if bits(w, 27, 25) == 0b101 {
        return Insn::BlxImm { target: branch_target(w, pc).wrapping_add(if bit(w, 24) { 2 } else { 0 }) };
    }
    if w == 0xF57F_F01F { return Insn::Clrex; }
    let b = bits(w, 27, 20);
    if b == 0x10 {
        if bit(w, 16) {
            return if w & 0xFFFF_FDFF == 0xF101_0000 { Insn::Setend { big: bit(w, 9) } } else { undef(w) };
        }
        let imod = bits(w, 19, 18) as u8;
        if imod == 1 || (imod == 0 && !bit(w, 17)) { return undef(w); }
        return Insn::Cps { imod, change_mode: bit(w, 17), a: bit(w, 8), i: bit(w, 7), f: bit(w, 6), mode: bits(w, 4, 0) as u8 };
    }
    // PLD (PLDW is v7 but shares the encoding; decoded as PLD).
    if bits(w, 15, 12) == 0xF {
        let up = bit(w, 23);
        if b & 0xF3 == 0x51 {
            return Insn::Pld { rn: reg(w, 16), addr: Addr { pre: true, up, writeback: false, offset: Offset::Imm(w & 0xFFF) } };
        }
        if b & 0xF3 == 0x71 && !bit(w, 4) {
            let (shift, amount) = norm_shift(bits(w, 6, 5), bits(w, 11, 7));
            return Insn::Pld { rn: reg(w, 16), addr: Addr { pre: true, up, writeback: false, offset: Offset::Reg { rm: reg(w, 0), shift, amount } } };
        }
    }
    undef(w)
}

/// Media instructions (bits 27:25 = 011, bit 4 = 1).
fn dec_media(w: u32, c: Cond) -> Insn {
    let (b, lo) = (bits(w, 27, 20), bits(w, 7, 4));
    let (rd, rn, rm) = (reg(w, 12), reg(w, 16), reg(w, 0));
    // Plain constructor: rd/rn/rm as given, everything else zero.
    let mk = |op, rd, rn, rm| Insn::Media { cond: c, op, rd, rn, rm, ra: Reg(0), imm: 0, amount: 0, x: false, y: false };
    let ext = |op| Insn::Media { cond: c, op, rd, rn, rm, ra: Reg(0), imm: (bits(w, 11, 10) * 8) as u8, amount: 0, x: false, y: false };
    // Saturate: rd 15:12, rn = source (bits 3:0); `sat` is the manual's saturate_to value.
    let sat = |op, sat: u32, wide: bool| {
        let (shift, amount) = if wide { (false, 0) } else {
            let n = bits(w, 11, 7);
            if bit(w, 6) { (true, if n == 0 { 32 } else { n as u8 }) } else { (false, n as u8) }
        };
        Insn::Media { cond: c, op, rd, rn: rm, rm: Reg(0), ra: Reg(0), imm: sat as u8, amount, x: shift, y: false }
    };
    // Multiply layout A: rd 19:16, ra 15:12, rm 11:8, rn 3:0.
    let mul = |op, x| Insn::Media { cond: c, op, rd: reg(w, 16), rn: reg(w, 0), rm: reg(w, 8), ra: reg(w, 12), imm: 0, amount: 0, x, y: false };
    let x = bit(w, 5);
    let ra15 = bits(w, 15, 12) == 0xF;
    let sh = lo & 3 == 1; // bit 4 = 1, bit 5 = 0: shifted-operand forms (PKH, SSAT, USAT)
    match b {
        0x61 | 0x62 | 0x63 | 0x65 | 0x66 | 0x67 => {
            let kind = match b { 0x61 => ParKind::S, 0x62 => ParKind::Q, 0x63 => ParKind::Sh, 0x65 => ParKind::U, 0x66 => ParKind::Uq, _ => ParKind::Uh };
            let op = match bits(w, 7, 5) { 0 => ParOp::Add16, 1 => ParOp::Asx, 2 => ParOp::Sax, 3 => ParOp::Sub16, 4 => ParOp::Add8, 7 => ParOp::Sub8, _ => return undef(w) };
            mk(MediaOp::Par(kind, op), rd, rn, rm)
        }
        0x68 if sh => {
            let tb = bit(w, 6);
            let n = bits(w, 11, 7);
            Insn::Media { cond: c, op: if tb { MediaOp::Pkhtb } else { MediaOp::Pkhbt }, rd, rn, rm, ra: Reg(0), imm: 0, amount: if tb && n == 0 { 32 } else { n as u8 }, x: false, y: false }
        }
        0x68 if lo == 0x7 => ext(MediaOp::Sxtb16),
        0x68 if lo == 0xB => mk(MediaOp::Sel, rd, rn, rm),
        0x6A | 0x6B if sh => sat(MediaOp::Ssat, bits(w, 20, 16) + 1, false),
        0x6A if lo == 3 => sat(MediaOp::Ssat16, bits(w, 19, 16) + 1, true),
        0x6A if lo == 7 => ext(MediaOp::Sxtb),
        0x6B if lo == 7 => ext(MediaOp::Sxth),
        0x6B if lo == 3 => mk(MediaOp::Rev, rd, Reg(0), rm),
        0x6B if lo == 0xB => mk(MediaOp::Rev16, rd, Reg(0), rm),
        0x6C if lo == 7 => ext(MediaOp::Uxtb16),
        0x6E | 0x6F if sh => sat(MediaOp::Usat, bits(w, 20, 16), false),
        0x6E if lo == 3 => sat(MediaOp::Usat16, bits(w, 19, 16), true),
        0x6E if lo == 7 => ext(MediaOp::Uxtb),
        0x6F if lo == 7 => ext(MediaOp::Uxth),
        0x6F if lo == 0xB => mk(MediaOp::Revsh, rd, Reg(0), rm),
        0x70 if lo & 0xC == 0 => mul(if ra15 { MediaOp::Smuad } else { MediaOp::Smlad }, x),
        0x70 if lo & 0xC == 4 => mul(if ra15 { MediaOp::Smusd } else { MediaOp::Smlsd }, x),
        0x74 if lo & 0xC == 0 => mul(MediaOp::Smlald, x),
        0x74 if lo & 0xC == 4 => mul(MediaOp::Smlsld, x),
        0x75 if lo & 0xC == 0 => mul(if ra15 { MediaOp::Smmul } else { MediaOp::Smmla }, x),
        0x75 if lo & 0xC == 0xC => mul(MediaOp::Smmls, x),
        0x78 if lo == 1 => mul(if ra15 { MediaOp::Usad8 } else { MediaOp::Usada8 }, false),
        _ => undef(w),
    }
}
