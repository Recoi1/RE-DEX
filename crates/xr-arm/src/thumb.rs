//! Thumb (ARMv6K: Thumb-1 + v6 additions + BL/BLX pair) decoder producing the shared `Insn`.
//!
//! Conventions for codegen (so ARM and Thumb share one path):
//! * Thumb PC reads as insn address + 4. Operands that read PC are folded into constants here:
//!   `ADR`/`ADD Rd,PC,#i` become `Mov Rd,#absolute`; hi-reg `MOV/ADD/CMP` with Rm = PC use `Imm(pc+4)`.
//!   (`CMP PC,Rm` and `ADD PC,PC` are the only leftovers: `CMP PC,Rm` keeps `rn = PC`, which codegen reads as
//!   insn+8; it never occurs in practice.)
//! * `LDR Rd,[PC,#i]` is `Load` with `rn = PC` whose offset is pre-folded so that the effective address is
//!   `(insn_addr + 8) +/- offset` with NO alignment applied to the base (the Thumb base `Align(pc+4,4)+i` is
//!   already baked into the offset). Codegen must treat `rn = PC` as the plain `insn_addr + 8`.
//! * `MOV/ADD PC,Rm` (hi-reg) is a plain `DataProc` with `rd = PC` (Thumb: no interworking, bit 0 ignored).
//! * `BlxImm` from Thumb code is Thumb->ARM (target word-aligned); from ARM code it is ARM->Thumb.
//! * `POP {..,PC}` is `LoadMulti` with PC in the list (ARMv6: interworking, bit 0 selects state).
use crate::insn::*;

const AL: Cond = Cond::Al;

fn reg(n: u32) -> Reg { Reg((n & 0xF) as u8) }
fn imm(v: u32) -> Operand2 { Operand2::Imm { value: v, rot: 0 } }
fn rop(rm: Reg) -> Operand2 { Operand2::RegImmShift { rm, shift: Shift::Lsl, amount: 0 } }
fn dp(op: DpOp, s: bool, rd: Reg, rn: Reg, op2: Operand2) -> Insn { Insn::DataProc { cond: AL, op, s, rd, rn, op2 } }
fn addr_imm(off: u32) -> Addr { Addr { pre: true, up: true, writeback: false, offset: Offset::Imm(off) } }
fn load(width: MemWidth, rt: Reg, rn: Reg, addr: Addr) -> Insn { Insn::Load { cond: AL, width, rt, rn, addr, user: false } }
fn store(width: MemWidth, rt: Reg, rn: Reg, addr: Addr) -> Insn { Insn::Store { cond: AL, width, rt, rn, addr, user: false } }
fn sext(v: u32, bits: u32) -> i32 { ((v << (32 - bits)) as i32) >> (32 - bits) }
fn media(op: MediaOp, rd: Reg, rm: Reg) -> Insn {
    Insn::Media { cond: AL, op, rd, rn: Reg(15), rm, ra: Reg(0), imm: 0, amount: 0, x: false, y: false }
}

/// Decode one Thumb instruction at `pc`. `next` is the following halfword (used only by the BL/BLX pair).
/// Returns the instruction and the bytes consumed (2, or 4 for a BL/BLX pair). Never panics.
pub fn decode_thumb(half: u16, next: u16, pc: u32) -> (Insn, u8) {
    let h = half as u32;
    let undef = (Insn::Undefined { word: h }, 2u8);
    let one = |i: Insn| (i, 2u8);
    let rd0 = reg(h & 7);
    let rn3 = reg((h >> 3) & 7);
    let rm6 = reg((h >> 6) & 7);
    match h >> 11 {
        // shift by immediate
        0b00000..=0b00010 => {
            let imm5 = ((h >> 6) & 31) as u8;
            let (shift, amount) = match h >> 11 {
                0 => (Shift::Lsl, imm5),
                1 => (Shift::Lsr, if imm5 == 0 { 32 } else { imm5 }),
                _ => (Shift::Asr, if imm5 == 0 { 32 } else { imm5 }),
            };
            one(dp(DpOp::Mov, true, rd0, Reg(0), Operand2::RegImmShift { rm: rn3, shift, amount }))
        }
        // add/sub register or imm3
        0b00011 => {
            let op = if h & 0x200 != 0 { DpOp::Sub } else { DpOp::Add };
            let op2 = if h & 0x400 != 0 { imm((h >> 6) & 7) } else { rop(rm6) };
            one(dp(op, true, rd0, rn3, op2))
        }
        // MOV/CMP/ADD/SUB rd, #imm8
        0b00100..=0b00111 => {
            let rd = reg((h >> 8) & 7);
            let i = imm(h & 0xFF);
            one(match (h >> 11) & 3 {
                0 => dp(DpOp::Mov, true, rd, Reg(0), i),
                1 => dp(DpOp::Cmp, true, Reg(0), rd, i),
                2 => dp(DpOp::Add, true, rd, rd, i),
                _ => dp(DpOp::Sub, true, rd, rd, i),
            })
        }
        0b01000 => one(decode_4000(h, pc)),
        // LDR literal
        0b01001 => {
            let target = (pc.wrapping_add(4) & !3).wrapping_add((h & 0xFF) << 2);
            let delta = target.wrapping_sub(pc.wrapping_add(8)) as i32;
            let addr = Addr { pre: true, up: delta >= 0, writeback: false, offset: Offset::Imm(delta.unsigned_abs()) };
            one(load(MemWidth::Word, reg((h >> 8) & 7), Reg::PC, addr))
        }
        // load/store register offset
        0b01010 | 0b01011 => {
            let a = Addr { pre: true, up: true, writeback: false, offset: Offset::Reg { rm: rm6, shift: Shift::Lsl, amount: 0 } };
            use MemWidth::*;
            one(match (h >> 9) & 7 {
                0 => store(Word, rd0, rn3, a),
                1 => store(Half, rd0, rn3, a),
                2 => store(Byte, rd0, rn3, a),
                3 => load(SignedByte, rd0, rn3, a),
                4 => load(Word, rd0, rn3, a),
                5 => load(Half, rd0, rn3, a),
                6 => load(Byte, rd0, rn3, a),
                _ => load(SignedHalf, rd0, rn3, a),
            })
        }
        // load/store word/byte imm5
        0b01100..=0b01111 => {
            let byte = h & 0x1000 != 0;
            let a = addr_imm(if byte { (h >> 6) & 31 } else { ((h >> 6) & 31) << 2 });
            let w = if byte { MemWidth::Byte } else { MemWidth::Word };
            one(if h & 0x800 != 0 { load(w, rd0, rn3, a) } else { store(w, rd0, rn3, a) })
        }
        // load/store halfword imm5
        0b10000 | 0b10001 => {
            let a = addr_imm(((h >> 6) & 31) << 1);
            one(if h & 0x800 != 0 { load(MemWidth::Half, rd0, rn3, a) } else { store(MemWidth::Half, rd0, rn3, a) })
        }
        // SP-relative load/store
        0b10010 | 0b10011 => {
            let rt = reg((h >> 8) & 7);
            let a = addr_imm((h & 0xFF) << 2);
            one(if h & 0x800 != 0 { load(MemWidth::Word, rt, Reg::SP, a) } else { store(MemWidth::Word, rt, Reg::SP, a) })
        }
        // ADR (absolute Mov) / ADD Rd,SP,#imm8*4
        0b10100 => {
            let target = (pc.wrapping_add(4) & !3).wrapping_add((h & 0xFF) << 2);
            one(dp(DpOp::Mov, false, reg((h >> 8) & 7), Reg(0), imm(target)))
        }
        0b10101 => one(dp(DpOp::Add, false, reg((h >> 8) & 7), Reg::SP, imm((h & 0xFF) << 2))),
        0b10110 | 0b10111 => decode_misc(h).map(|i| (i, 2u8)).unwrap_or(undef),
        // STMIA / LDMIA Rn!, list
        0b11000 | 0b11001 => {
            let rn = reg((h >> 8) & 7);
            let list = (h & 0xFF) as u16;
            if list == 0 { return undef; }
            if h & 0x800 != 0 {
                let wb = list & (1 << rn.0) == 0;
                one(Insn::LoadMulti { cond: AL, rn, list, mode: BlockMode::Ia, writeback: wb, user_or_psr: false })
            } else {
                one(Insn::StoreMulti { cond: AL, rn, list, mode: BlockMode::Ia, writeback: true, user_or_psr: false })
            }
        }
        // conditional branch / SVC
        0b11010 | 0b11011 => match (h >> 8) & 0xF {
            0xE => undef,
            0xF => one(Insn::Svc { imm: h & 0xFF, cond: AL }),
            c => {
                let target = pc.wrapping_add(4).wrapping_add((sext(h & 0xFF, 8) << 1) as u32);
                one(Insn::Branch { link: false, target, cond: Cond::from_bits(c) })
            }
        },
        // unconditional branch
        0b11100 => one(Insn::Branch { link: false, target: pc.wrapping_add(4).wrapping_add((sext(h & 0x7FF, 11) << 1) as u32), cond: AL }),
        // BL/BLX prefix
        0b11110 => {
            let n = next as u32;
            let base = pc.wrapping_add(4).wrapping_add((sext(h & 0x7FF, 11) << 12) as u32).wrapping_add((n & 0x7FF) << 1);
            match n >> 11 {
                0b11111 => (Insn::Branch { link: true, target: base, cond: AL }, 4),
                0b11101 if n & 1 == 0 => (Insn::BlxImm { target: base & !3 }, 4),
                _ => undef,
            }
        }
        // 0b11101 (BLX suffix alone) and 0b11111 (BL suffix alone)
        _ => undef,
    }
}

/// 0x4000..=0x47FF: ALU ops, hi-register ops, BX/BLX.
fn decode_4000(h: u32, pc: u32) -> Insn {
    if h & 0x400 == 0 {
        let (rd, rm) = (reg(h & 7), reg((h >> 3) & 7));
        let rr = |op| dp(op, true, rd, rd, rop(rm));
        let sh = |shift| dp(DpOp::Mov, true, rd, Reg(0), Operand2::RegRegShift { rm: rd, shift, rs: rm });
        return match (h >> 6) & 0xF {
            0 => rr(DpOp::And),
            1 => rr(DpOp::Eor),
            2 => sh(Shift::Lsl),
            3 => sh(Shift::Lsr),
            4 => sh(Shift::Asr),
            5 => rr(DpOp::Adc),
            6 => rr(DpOp::Sbc),
            7 => sh(Shift::Ror),
            8 => dp(DpOp::Tst, true, Reg(0), rd, rop(rm)),
            9 => dp(DpOp::Rsb, true, rd, rm, imm(0)),
            10 => dp(DpOp::Cmp, true, Reg(0), rd, rop(rm)),
            11 => dp(DpOp::Cmn, true, Reg(0), rd, rop(rm)),
            12 => rr(DpOp::Orr),
            13 => Insn::Mul { cond: AL, accumulate: false, s: true, rd, rn: Reg(0), rm, rs: rd },
            14 => rr(DpOp::Bic),
            _ => dp(DpOp::Mvn, true, rd, Reg(0), rop(rm)),
        };
    }
    let rd = reg((h & 7) | ((h >> 4) & 8));
    let rm = reg((h >> 3) & 0xF);
    let rm_op = if rm == Reg::PC { imm(pc.wrapping_add(4)) } else { rop(rm) };
    match (h >> 8) & 3 {
        0 if rd == Reg::PC && rm == Reg::PC => dp(DpOp::Mov, false, rd, Reg(0), imm(pc.wrapping_add(4).wrapping_mul(2))),
        0 if rd == Reg::PC => dp(DpOp::Add, false, rd, rm, imm(pc.wrapping_add(4))), // pc = rm + (pc+4)
        0 => dp(DpOp::Add, false, rd, rd, rm_op),
        1 => dp(DpOp::Cmp, true, Reg(0), rd, rm_op),
        2 => dp(DpOp::Mov, false, rd, Reg(0), rm_op),
        _ if h & 7 != 0 => Insn::Undefined { word: h },
        _ => Insn::BranchExchange { link: h & 0x80 != 0, rm, cond: AL },
    }
}

/// 0xB000..=0xBFFF.
fn decode_misc(h: u32) -> Option<Insn> {
    let (rd, rm) = (reg(h & 7), reg((h >> 3) & 7));
    Some(match (h >> 8) & 0xF {
        0x0 => {
            let op = if h & 0x80 != 0 { DpOp::Sub } else { DpOp::Add };
            dp(op, false, Reg::SP, Reg::SP, imm((h & 0x7F) << 2))
        }
        0x2 => {
            let op = match (h >> 6) & 3 { 0 => MediaOp::Sxth, 1 => MediaOp::Sxtb, 2 => MediaOp::Uxth, _ => MediaOp::Uxtb };
            media(op, rd, rm)
        }
        0x4 | 0x5 => {
            let list = (h & 0xFF) | if h & 0x100 != 0 { 1 << 14 } else { 0 };
            Insn::StoreMulti { cond: AL, rn: Reg::SP, list: list as u16, mode: BlockMode::Db, writeback: true, user_or_psr: false }
        }
        0x6 => {
            if h & 0xFFE8 == 0xB660 {
                Insn::Cps { imod: if h & 0x10 != 0 { 3 } else { 2 }, change_mode: false, a: h & 4 != 0, i: h & 2 != 0, f: h & 1 != 0, mode: 0 }
            } else if h & 0xFFF7 == 0xB650 {
                Insn::Setend { big: h & 8 != 0 }
            } else {
                return None;
            }
        }
        0xA => match (h >> 6) & 3 {
            0 => media(MediaOp::Rev, rd, rm),
            1 => media(MediaOp::Rev16, rd, rm),
            3 => media(MediaOp::Revsh, rd, rm),
            _ => return None,
        },
        0xC | 0xD => {
            let list = (h & 0xFF) | if h & 0x100 != 0 { 1 << 15 } else { 0 };
            Insn::LoadMulti { cond: AL, rn: Reg::SP, list: list as u16, mode: BlockMode::Ia, writeback: true, user_or_psr: false }
        }
        0xE => Insn::Bkpt { imm: (h & 0xFF) as u16 },
        0xF if h & 0xF == 0 => {
            let kind = match (h >> 4) & 0xF {
                0 => HintKind::Nop,
                1 => HintKind::Yield,
                2 => HintKind::Wfe,
                3 => HintKind::Wfi,
                4 => HintKind::Sev,
                _ => return None,
            };
            Insn::Hint { cond: AL, kind }
        }
        _ => return None, // CBZ/CBNZ/IT/etc. are Thumb-2
    })
}
