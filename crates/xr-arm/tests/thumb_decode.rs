use xr_arm::*;

const PC: u32 = 0x0010_0000;
fn d(h: u16) -> (Insn, u8) { decode_thumb(h, 0, PC) }
fn imm(v: u32) -> Operand2 { Operand2::Imm { value: v, rot: 0 } }
fn r(n: u8) -> Operand2 { Operand2::RegImmShift { rm: Reg(n), shift: Shift::Lsl, amount: 0 } }

#[test]
fn thumb_vectors() {
    assert_eq!(d(0x2001), (Insn::DataProc { cond: Cond::Al, op: DpOp::Mov, s: true, rd: Reg(0), rn: Reg(0), op2: imm(1) }, 2));
    assert_eq!(d(0x1840), (Insn::DataProc { cond: Cond::Al, op: DpOp::Add, s: true, rd: Reg(0), rn: Reg(0), op2: r(1) }, 2));
    assert_eq!(d(0x4770), (Insn::BranchExchange { link: false, rm: Reg(14), cond: Cond::Al }, 2));
    assert_eq!(d(0xB510), (Insn::StoreMulti { cond: Cond::Al, rn: Reg::SP, list: 0x4010, mode: BlockMode::Db, writeback: true, user_or_psr: false }, 2));
    assert_eq!(d(0xBD10), (Insn::LoadMulti { cond: Cond::Al, rn: Reg::SP, list: 0x8010, mode: BlockMode::Ia, writeback: true, user_or_psr: false }, 2));
    assert_eq!(decode_thumb(0xF000, 0xF800, PC), (Insn::Branch { link: true, target: 0x0010_0004, cond: Cond::Al }, 4));
    assert_eq!(decode_thumb(0xF000, 0xE800, PC), (Insn::BlxImm { target: 0x0010_0004 }, 4));
    assert_eq!(d(0xDF32), (Insn::Svc { imm: 0x32, cond: Cond::Al }, 2));
    assert_eq!(d(0xD000), (Insn::Branch { link: false, target: PC + 4, cond: Cond::Eq }, 2));
    assert_eq!(d(0xD1FE), (Insn::Branch { link: false, target: PC, cond: Cond::Ne }, 2)); // beq . backwards
    assert_eq!(d(0xE7FE), (Insn::Branch { link: false, target: PC, cond: Cond::Al }, 2));
    assert!(matches!(d(0xDE00).0, Insn::Undefined { .. }));
    // lone prefix: Undefined, 2 bytes
    assert!(matches!(decode_thumb(0xF000, 0x1234, PC), (Insn::Undefined { .. }, 2)));
    // negative BL: prefix hi11 = 0x7FF, lo = 0x7FE -> pc+4-4... = pc
    assert_eq!(decode_thumb(0xF7FF, 0xFFFE, PC), (Insn::Branch { link: true, target: PC, cond: Cond::Al }, 4));
}

#[test]
fn thumb_misc() {
    // hi-reg MOV does not set flags; MOV r8, r1
    assert_eq!(d(0x4688), (Insn::DataProc { cond: Cond::Al, op: DpOp::Mov, s: false, rd: Reg(8), rn: Reg(0), op2: r(1) }, 2));
    // CMP hi sets flags
    assert!(matches!(d(0x4540).0, Insn::DataProc { op: DpOp::Cmp, s: true, .. }));
    // LDR r0,[pc,#4] at pc 0x100002: target = align(0x100006,4)+4 = 0x100008 ; base pc+8 = 0x10000A -> up=false, 2
    let (i, _) = decode_thumb(0x4801, 0, PC + 2);
    assert_eq!(i, Insn::Load { cond: Cond::Al, width: MemWidth::Word, rt: Reg(0), rn: Reg::PC, addr: Addr { pre: true, up: false, writeback: false, offset: Offset::Imm(2) }, user: false });
    // ADR r0,#4 -> absolute mov
    assert_eq!(d(0xA001), (Insn::DataProc { cond: Cond::Al, op: DpOp::Mov, s: false, rd: Reg(0), rn: Reg(0), op2: imm(PC + 8) }, 2));
    // REV r0,r1 ; UXTB r2,r3 ; CPSID i ; SETEND BE ; BKPT 5
    assert!(matches!(d(0xBA08).0, Insn::Media { op: MediaOp::Rev, rd: Reg(0), rm: Reg(1), .. }));
    assert!(matches!(d(0xB2DA).0, Insn::Media { op: MediaOp::Uxtb, rd: Reg(2), rm: Reg(3), .. }));
    assert_eq!(d(0xB672).0, Insn::Cps { imod: 3, change_mode: false, a: false, i: true, f: false, mode: 0 });
    assert_eq!(d(0xB658).0, Insn::Setend { big: true });
    assert_eq!(d(0xBE05).0, Insn::Bkpt { imm: 5 });
    // LSR r0,r1,#0 means #32
    assert_eq!(d(0x0808).0, Insn::DataProc { cond: Cond::Al, op: DpOp::Mov, s: true, rd: Reg(0), rn: Reg(0), op2: Operand2::RegImmShift { rm: Reg(1), shift: Shift::Lsr, amount: 32 } });
}

#[test]
fn thumb_never_panics() {
    for h in 0..=0xFFFFu32 {
        for n in [0u16, 0xF800, 0xE800, 0xFFFF, 0xF7FF] {
            for pc in [0x0010_0000u32, 0x0010_0002, 0xFFFF_FFFE, 0] {
                let (_, len) = decode_thumb(h as u16, n, pc);
                assert!(len == 2 || len == 4);
            }
        }
    }
}

#[test]
fn thumb_decode_stats() {
    let Ok(dump) = std::env::var("XR_DUMP") else { eprintln!("XR_DUMP unset: skipping"); return };
    let code = std::fs::read(format!("{dump}/exefs/code.bin")).expect("code.bin");
    let txt = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../functions/x-usa-v0.2.0/code.txt")).expect("code.txt");
    let funcs = xr_formats::parse_functab(&txt).expect("functab");
    const BASE: u32 = 0x0010_0000;
    let (mut total, mut undef, mut nfn) = (0u64, 0u64, 0);
    let mut halves: std::collections::HashMap<u16, u64> = Default::default();
    let rd = |pc: u32| -> u16 { let o = (pc - BASE) as usize; if o + 2 <= code.len() { u16::from_le_bytes([code[o], code[o + 1]]) } else { 0 } };
    for f in funcs.iter().filter(|f| f.mode == xr_formats::Mode::Thumb) {
        nfn += 1;
        let mut pc = f.start;
        while pc + 2 <= f.end {
            let h = rd(pc);
            let (i, len) = decode_thumb(h, rd(pc + 2), pc);
            total += 1;
            if let Insn::Undefined { .. } = i { undef += 1; *halves.entry(h).or_default() += 1; }
            pc += len as u32;
        }
    }
    let mut v: Vec<_> = halves.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (h, n) in v.iter().take(15) { eprintln!("undefined {h:04X} x{n}"); }
    let rate = undef as f64 / total.max(1) as f64 * 100.0;
    eprintln!("Thumb functions: {nfn}, halfwords decoded: {total}, Undefined: {undef} ({rate:.4}%)");
    assert!(rate < 0.5, "Undefined rate {rate:.4}% >= 0.5%");
}
