//! Decoded ARMv6K instruction representation, shared by the ARM and Thumb decoders.
//!
//! Every field the architecture reads is kept (including "ignored" register fields as encoded),
//! so the code generator can reproduce each instruction's exact behaviour.

/// ARM condition code (the `cond` field; 0b1111 never reaches here, it selects the unconditional space).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cond { Eq, Ne, Cs, Cc, Mi, Pl, Vs, Vc, Hi, Ls, Ge, Lt, Gt, Le, Al }

impl Cond {
    /// Map a 4-bit field to a condition; 0b1111 maps to `Al` (callers handle the unconditional space first).
    pub fn from_bits(b: u32) -> Cond {
        use Cond::*;
        [Eq, Ne, Cs, Cc, Mi, Pl, Vs, Vc, Hi, Ls, Ge, Lt, Gt, Le, Al, Al][(b & 0xF) as usize]
    }
}

/// A core register number, 0..=15 (13 = sp, 14 = lr, 15 = pc).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Reg(pub u8);

impl Reg {
    pub const SP: Reg = Reg(13);
    pub const LR: Reg = Reg(14);
    pub const PC: Reg = Reg(15);
}

/// Data-processing opcode (bits 24:21 order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DpOp { And, Eor, Sub, Rsb, Add, Adc, Sbc, Rsc, Tst, Teq, Cmp, Cmn, Orr, Mov, Bic, Mvn }

/// Barrel-shifter operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shift { Lsl, Lsr, Asr, Ror, Rrx }

/// Data-processing operand 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand2 {
    /// Rotated immediate: `value` is already rotated; `rot` is the rotate-right amount (0,2,..,30).
    /// Shifter carry-out: bit 31 of `value` when `rot != 0`, else C unchanged.
    Imm { value: u32, rot: u8 },
    /// Register shifted by an immediate. Normalised: LSR/ASR #0 in the encoding are `amount` 32;
    /// ROR #0 is `Rrx` with `amount` 1; `Lsl` with `amount` 0 is a plain register.
    RegImmShift { rm: Reg, shift: Shift, amount: u8 },
    /// Register shifted by the low byte of `rs`.
    RegRegShift { rm: Reg, shift: Shift, rs: Reg },
}

/// Load/store transfer width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemWidth { Word, Byte, Half, SignedByte, SignedHalf, Double }

/// Load/store offset (added or subtracted per `Addr::up`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offset {
    Imm(u32),
    /// Shifted register offset (same normalisation as `Operand2::RegImmShift`).
    Reg { rm: Reg, shift: Shift, amount: u8 },
}

/// Load/store addressing: pre-indexed (`pre`) or post-indexed; `writeback` is true for post-index and `!` forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr { pub pre: bool, pub up: bool, pub writeback: bool, pub offset: Offset }

/// LDM/STM addressing mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockMode { Ia, Ib, Da, Db }

/// MSR source operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsrSrc { Imm(u32), Reg(Reg) }

/// Hint instructions (unallocated hints decode as `Nop`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HintKind { Nop, Yield, Wfe, Wfi, Sev }

/// Parallel add/sub flavour (prefix): S, Q, SH signed; U, UQ, UH unsigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParKind { S, Q, Sh, U, Uq, Uh }

/// Parallel add/sub operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParOp { Add16, Asx, Sax, Sub16, Add8, Sub8 }

/// ARMv5TE/v6 media operation selector for `Insn::Media`.
///
/// Field use (`rd, rn, rm, ra, imm, amount, x, y`), names as in the ARMv7 manual:
/// * Multiplies (`Smulxy Smlaxy Smulwy Smlawy Smlalxy Smuad Smusd Smlad Smlsd Smlald Smlsld Smmul Smmla Smmls`,
///   `Usad8 Usada8`): rd = bits 19:16 (RdHi for the long forms), ra = bits 15:12 (RdLo for the long forms),
///   rm = bits 11:8, rn = bits 3:0. `x`: operand-swap (SMUAD..SMLSLD), bit 5 (SMULxy/SMLAxy: select Rn top half),
///   round (SMMxx). `y`: select Rm top half (SMULxy/SMLAxy/SMLALxy, SMULWy/SMLAWy).
///   `Smuad`/`Smusd`/`Smmul`/`Usad8` mean "Ra = 15, no accumulate".
/// * `Sxtb Sxth Sxtb16 Uxtb Uxth Uxtb16`: rd 15:12, rn 19:16 (15 = no accumulate, i.e. the non-A form), rm 3:0,
///   `imm` = rotate-right amount (0,8,16,24).
/// * `Rev Rev16 Revsh Clz`: rd 15:12, rm 3:0.
/// * `Qadd Qsub Qdadd Qdsub`: rd 15:12, rm 3:0, rn 19:16 (result = sat(Rm op Rn)).
/// * `Sel`, `Par(..)`: rd 15:12, rn 19:16, rm 3:0.
/// * `Pkhbt Pkhtb`: rd, rn, rm as above, `amount` = shift (PKHTB #0 is normalised to 32).
/// * `Ssat Usat`: rd 15:12, rn = bits 3:0 (source), `imm` = saturate-to bit count (1..=32 / 0..=31),
///   `amount` shift 0..=32, `x` true = ASR (else LSL). `Ssat16 Usat16`: rd, rn, imm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaOp {
    Smulxy, Smlaxy, Smulwy, Smlawy, Smlalxy,
    Smuad, Smusd, Smlad, Smlsd, Smlald, Smlsld, Smmul, Smmla, Smmls,
    Usad8, Usada8,
    Sxtb, Sxth, Sxtb16, Uxtb, Uxth, Uxtb16,
    Rev, Rev16, Revsh, Sel, Clz,
    Qadd, Qsub, Qdadd, Qdsub,
    Pkhbt, Pkhtb,
    Ssat, Usat, Ssat16, Usat16,
    Par(ParKind, ParOp),
}

/// VFP data-processing op with three operands: `Vd = f(Vd, Vn, Vm)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VfpArith { Mla, Mls, Nmla, Nmls, Mul, Nmul, Add, Sub, Div }

/// VFP two-operand op: `Vd = f(Vm)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VfpUnary { Mov, Abs, Neg, Sqrt }

/// VFP instruction. Register indices: single = S0..S31 (0..=31), double = D0..D15 (0..=15).
/// The decoder rejects (as Undefined) D16..D31, which VFPv2 does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfpInsn {
    Arith { cond: Cond, op: VfpArith, dp: bool, d: u8, n: u8, m: u8 },
    Unary { cond: Cond, op: VfpUnary, dp: bool, d: u8, m: u8 },
    /// VMOV #imm: `bits` is the expanded value (f32 bits in the low 32 for single).
    MovImm { cond: Cond, dp: bool, d: u8, bits: u64 },
    /// VCMP/VCMPE (`e` = raise on quiet NaN); `zero` compares with +0.0 (`m` is 0 then).
    Cmp { cond: Cond, dp: bool, e: bool, d: u8, m: u8, zero: bool },
    /// VCVT f32<->f64: `to_dp` true = single source (S[m]) to double dest (D[d]).
    CvtFloat { cond: Cond, to_dp: bool, d: u8, m: u8 },
    /// VCVT int->float: source is S[m] holding s32 (`signed`) or u32; dest float of width `dp`.
    CvtToFloat { cond: Cond, dp: bool, signed: bool, d: u8, m: u8 },
    /// VCVT float->int: source float of width `dp` at m; dest S[d]. `round_zero` = VCVT (else VCVTR: FPSCR rounding).
    CvtToInt { cond: Cond, dp: bool, signed: bool, round_zero: bool, d: u8, m: u8 },
    /// VMOV Rt<->Sn.
    MovCoreSingle { cond: Cond, to_core: bool, rt: Reg, n: u8 },
    /// VMOV Rt<->Dn[hi] (32-bit half of a double).
    MovCoreScalar { cond: Cond, to_core: bool, rt: Reg, d: u8, hi: bool },
    /// VMOV Rt,Rt2<->(S[m],S[m+1]) when `!dp`, or <->D[m] (rt = low word) when `dp`.
    MovPair { cond: Cond, to_core: bool, dp: bool, rt: Reg, rt2: Reg, m: u8 },
    /// VLDR/VSTR: address = rn +/- `offset` (bytes); rn = 15 means word-aligned pc+8 base.
    Mem { cond: Cond, load: bool, dp: bool, d: u8, rn: Reg, offset: u16, up: bool },
    /// VLDM/VSTM (also VPOP/VPUSH when rn = sp with writeback): `count` registers from `first`;
    /// `increment` true = IA, false = DB (DB always has writeback).
    Block { cond: Cond, load: bool, dp: bool, rn: Reg, first: u8, count: u8, increment: bool, writeback: bool },
    /// VMRS Rt,<reg> (reg 1 = FPSCR; rt = 15 with reg 1 means APSR_nzcv).
    Vmrs { cond: Cond, reg: u8, rt: Reg },
    /// VMSR <reg>,Rt.
    Vmsr { cond: Cond, reg: u8, rt: Reg },
}

/// A decoded instruction (ARM state; the Thumb decoder produces the same type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insn {
    /// Data processing. rd/rn hold the encoded fields even when the opcode ignores them (Mov: rn; Tst..Cmn: rd).
    DataProc { cond: Cond, op: DpOp, s: bool, rd: Reg, rn: Reg, op2: Operand2 },
    /// MUL / MLA: `rd = rm*rs (+ rn if accumulate)`.
    Mul { cond: Cond, accumulate: bool, s: bool, rd: Reg, rn: Reg, rm: Reg, rs: Reg },
    /// UMULL/UMLAL/SMULL/SMLAL: `{rdhi,rdlo} = rm*rs (+ {rdhi,rdlo})`.
    MulLong { cond: Cond, signed: bool, accumulate: bool, s: bool, rdhi: Reg, rdlo: Reg, rm: Reg, rs: Reg },
    /// UMAAL: `{rdhi,rdlo} = rm*rs + rdlo + rdhi` (unsigned).
    Umaal { cond: Cond, rdhi: Reg, rdlo: Reg, rm: Reg, rs: Reg },
    /// Load; `rn = PC` is read as the plain insn address + 8 (Thumb literal-pool offsets are pre-folded to match).
    /// `user` = LDRT/LDRBT/LDRHT forms (post-indexed, user-mode access). `Double` loads rt and rt+1.
    Load { cond: Cond, width: MemWidth, rt: Reg, rn: Reg, addr: Addr, user: bool },
    /// Store; same conventions as `Load`.
    Store { cond: Cond, width: MemWidth, rt: Reg, rn: Reg, addr: Addr, user: bool },
    /// LDM; `user_or_psr` is the S bit (user-bank registers, or CPSR<-SPSR when pc is in the list and W/..).
    LoadMulti { cond: Cond, rn: Reg, list: u16, mode: BlockMode, writeback: bool, user_or_psr: bool },
    /// STM; `user_or_psr` is the S bit (store user-bank registers).
    StoreMulti { cond: Cond, rn: Reg, list: u16, mode: BlockMode, writeback: bool, user_or_psr: bool },
    /// B/BL with absolute target (pc + 8 + offset).
    Branch { link: bool, target: u32, cond: Cond },
    /// BX/BLX(register) (BXJ decodes as BX). Bit 0 of the register selects Thumb.
    BranchExchange { link: bool, rm: Reg, cond: Cond },
    /// BLX(immediate): unconditional, toggles the instruction set. Decoded from ARM code it switches ARM->Thumb
    /// (`target` is the Thumb entry); decoded from Thumb code it switches Thumb->ARM (`target` is word-aligned).
    BlxImm { target: u32 },
    Svc { imm: u32, cond: Cond },
    Bkpt { imm: u16 },
    /// LDREX/LDREXB/LDREXH/LDREXD (`width` Word/Byte/Half/Double).
    Ldrex { cond: Cond, width: MemWidth, rt: Reg, rn: Reg },
    /// STREX*: `rd` receives the status.
    Strex { cond: Cond, width: MemWidth, rd: Reg, rt: Reg, rn: Reg },
    Clrex,
    /// SWP/SWPB: `rd = [rn]; [rn] = rm`.
    Swp { cond: Cond, byte: bool, rd: Reg, rm: Reg, rn: Reg },
    Mrs { cond: Cond, rd: Reg, spsr: bool },
    /// MSR; `mask` is the 4-bit field mask (bit0 c, bit1 x, bit2 s, bit3 f).
    Msr { cond: Cond, spsr: bool, mask: u8, src: MsrSrc },
    Mcr { cond: Cond, coproc: u8, opc1: u8, crn: u8, crm: u8, opc2: u8, rt: Reg },
    Mrc { cond: Cond, coproc: u8, opc1: u8, crn: u8, crm: u8, opc2: u8, rt: Reg },
    Hint { cond: Cond, kind: HintKind },
    /// PLD preload hint (no architectural effect); addr.offset as for loads.
    Pld { rn: Reg, addr: Addr },
    Setend { big: bool },
    /// CPS: `imod` 2 = enable, 3 = disable, 0 = none; `mode` valid when `change_mode`.
    Cps { imod: u8, change_mode: bool, a: bool, i: bool, f: bool, mode: u8 },
    /// v5TE/v6 media instruction (see `MediaOp` for the field mapping).
    Media { cond: Cond, op: MediaOp, rd: Reg, rn: Reg, rm: Reg, ra: Reg, imm: u8, amount: u8, x: bool, y: bool },
    Vfp(VfpInsn),
    /// Anything unhandled or undefined (never a panic).
    Undefined { word: u32 },
}
