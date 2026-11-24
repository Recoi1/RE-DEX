use xr_arm::*;

fn d(w: u32) -> Insn { decode_arm(w, 0x0010_0000) }
fn r(n: u8) -> Reg { Reg(n) }
fn imm_addr(pre: bool, up: bool, wb: bool, v: u32) -> Addr { Addr { pre, up, writeback: wb, offset: Offset::Imm(v) } }

#[test]
fn arm_vectors() {
    assert_eq!(d(0xE3A00001), Insn::DataProc { cond: Cond::Al, op: DpOp::Mov, s: false, rd: r(0), rn: r(0), op2: Operand2::Imm { value: 1, rot: 0 } });
    assert_eq!(d(0xE0810002), Insn::DataProc { cond: Cond::Al, op: DpOp::Add, s: false, rd: r(0), rn: r(1), op2: Operand2::RegImmShift { rm: r(2), shift: Shift::Lsl, amount: 0 } });
    assert_eq!(d(0xE12FFF1E), Insn::BranchExchange { link: false, rm: r(14), cond: Cond::Al });
    assert_eq!(d(0xEB000000), Insn::Branch { link: true, target: 0x0010_0008, cond: Cond::Al });
    assert_eq!(d(0xE5910004), Insn::Load { cond: Cond::Al, width: MemWidth::Word, rt: r(0), rn: r(1), addr: imm_addr(true, true, false, 4), user: false });
    assert_eq!(d(0xE92D4010), Insn::StoreMulti { cond: Cond::Al, rn: r(13), list: 0x4010, mode: BlockMode::Db, writeback: true, user_or_psr: false });
    assert_eq!(d(0xE1910F9F), Insn::Ldrex { cond: Cond::Al, width: MemWidth::Word, rt: r(0), rn: r(1) });
    assert_eq!(d(0xE1812F90), Insn::Strex { cond: Cond::Al, width: MemWidth::Word, rd: r(2), rt: r(0), rn: r(1) });
    assert_eq!(d(0xEF000032), Insn::Svc { imm: 0x32, cond: Cond::Al });
    assert_eq!(d(0xEE300A01), Insn::Vfp(VfpInsn::Arith { cond: Cond::Al, op: VfpArith::Add, dp: false, d: 0, n: 0, m: 2 }));
    assert!(matches!(d(0x0A000000), Insn::Branch { link: false, cond: Cond::Eq, .. }));
    assert_eq!(d(0xFFFFFFFF), Insn::Undefined { word: 0xFFFFFFFF });
}

#[test]
fn more_vectors() {
    // blx imm with H=1: pc+8+(1<<2)+2
    assert_eq!(d(0xFB000001), Insn::BlxImm { target: 0x0010_000E });
    assert_eq!(d(0xF57FF01F), Insn::Clrex);
    // mov r0,r1,lsr #32 (LSR #0) ; mov r0,r1,rrx
    assert!(matches!(d(0xE1A00021), Insn::DataProc { op2: Operand2::RegImmShift { shift: Shift::Lsr, amount: 32, .. }, .. }));
    assert!(matches!(d(0xE1A00061), Insn::DataProc { op2: Operand2::RegImmShift { shift: Shift::Rrx, amount: 1, .. }, .. }));
    // mov r0,#0xFF000000 (rot 8) keeps rot
    assert_eq!(d(0xE3A004FF), Insn::DataProc { cond: Cond::Al, op: DpOp::Mov, s: false, rd: r(0), rn: r(0), op2: Operand2::Imm { value: 0xFF00_0000, rot: 8 } });
    // cmp ; clz ; qadd ; blx r3 ; mrs ; msr ; bkpt ; wfi
    assert!(matches!(d(0xE3500000), Insn::DataProc { op: DpOp::Cmp, s: true, .. }));
    assert!(matches!(d(0xE16F0F11), Insn::Media { op: MediaOp::Clz, rd: Reg(0), rm: Reg(1), .. }));
    assert!(matches!(d(0xE1020051), Insn::Media { op: MediaOp::Qadd, rd: Reg(0), rm: Reg(1), rn: Reg(2), .. }));
    assert_eq!(d(0xE12FFF33), Insn::BranchExchange { link: true, rm: r(3), cond: Cond::Al });
    assert_eq!(d(0xE10F0000), Insn::Mrs { cond: Cond::Al, rd: r(0), spsr: false });
    assert_eq!(d(0xE129F000), Insn::Msr { cond: Cond::Al, spsr: false, mask: 9, src: MsrSrc::Reg(r(0)) });
    assert_eq!(d(0xE1200070), Insn::Bkpt { imm: 0 });
    assert_eq!(d(0xE320F003), Insn::Hint { cond: Cond::Al, kind: HintKind::Wfi });
    // mul / umull
    assert!(matches!(d(0xE0000291), Insn::Mul { accumulate: false, rd: Reg(0), rm: Reg(1), rs: Reg(2), .. }));
    assert!(matches!(d(0xE0810392), Insn::MulLong { signed: false, accumulate: false, rdhi: Reg(1), rdlo: Reg(0), rm: Reg(2), rs: Reg(3), .. }));
    // ldrh r0,[r1,#2] ; ldrd r2,[r1] ; ldrsb r0,[r1,r2]
    assert_eq!(d(0xE1D100B2), Insn::Load { cond: Cond::Al, width: MemWidth::Half, rt: r(0), rn: r(1), addr: imm_addr(true, true, false, 2), user: false });
    assert!(matches!(d(0xE1C120D0), Insn::Load { width: MemWidth::Double, rt: Reg(2), .. }));
    assert!(matches!(d(0xE19100D2), Insn::Load { width: MemWidth::SignedByte, addr: Addr { offset: Offset::Reg { .. }, .. }, .. }));
    // ldr r0,[r1],#4 (post-index => writeback)
    assert_eq!(d(0xE4910004), Insn::Load { cond: Cond::Al, width: MemWidth::Word, rt: r(0), rn: r(1), addr: imm_addr(false, true, true, 4), user: false });
    // ldmia sp!,{r4,pc}
    assert_eq!(d(0xE8BD8010), Insn::LoadMulti { cond: Cond::Al, rn: r(13), list: 0x8010, mode: BlockMode::Ia, writeback: true, user_or_psr: false });
    // uxtb r0,r1,ror #8 ; rev ; sel ; usat ; pkhbt ; sadd8
    assert!(matches!(d(0xE6EF0471), Insn::Media { op: MediaOp::Uxtb, rd: Reg(0), rn: Reg(15), rm: Reg(1), imm: 8, .. }));
    assert!(matches!(d(0xE6BF0F31), Insn::Media { op: MediaOp::Rev, rd: Reg(0), rm: Reg(1), .. }));
    assert!(matches!(d(0xE6810FB2), Insn::Media { op: MediaOp::Sel, .. }));
    assert!(matches!(d(0xE6E50011), Insn::Media { op: MediaOp::Usat, imm: 5, .. }));
    assert!(matches!(d(0xE6810012), Insn::Media { op: MediaOp::Pkhbt, .. }));
    assert!(matches!(d(0xE6110F92), Insn::Media { op: MediaOp::Par(ParKind::S, ParOp::Add8), .. }));
    assert!(matches!(d(0xE6510F92), Insn::Media { op: MediaOp::Par(ParKind::U, ParOp::Add8), .. }));
    // mrc p15,0,r0,c13,c0,3
    assert_eq!(d(0xEE1D0F70), Insn::Mrc { cond: Cond::Al, coproc: 15, opc1: 0, crn: 13, crm: 0, opc2: 3, rt: r(0) });
    // pld [r0]
    assert!(matches!(d(0xF5D0F000), Insn::Pld { rn: Reg(0), .. }));
    // vpush {d8-d9} ; vldr s0,[r0,#8] ; vmrs APSR_nzcv ; vmov s0,r1 ; vcvt.f64.f32 d0,s2 ; vcvt.f32.f64 s0,d0
    assert_eq!(d(0xED2D8B04), Insn::Vfp(VfpInsn::Block { cond: Cond::Al, load: false, dp: true, rn: r(13), first: 8, count: 2, increment: false, writeback: true }));
    assert_eq!(d(0xED900A02), Insn::Vfp(VfpInsn::Mem { cond: Cond::Al, load: true, dp: false, d: 0, rn: r(0), offset: 8, up: true }));
    assert_eq!(d(0xEEF1FA10), Insn::Vfp(VfpInsn::Vmrs { cond: Cond::Al, reg: 1, rt: r(15) }));
    assert_eq!(d(0xEE001A10), Insn::Vfp(VfpInsn::MovCoreSingle { cond: Cond::Al, to_core: false, rt: r(1), n: 0 }));
    assert_eq!(d(0xEEB70AC1), Insn::Vfp(VfpInsn::CvtFloat { cond: Cond::Al, to_dp: true, d: 0, m: 2 }));
    assert_eq!(d(0xEEB70BC0), Insn::Vfp(VfpInsn::CvtFloat { cond: Cond::Al, to_dp: false, d: 0, m: 0 }));
    // f64 source, single dest with odd S index (D bit set)
    assert_eq!(d(0xEEF70BC0), Insn::Vfp(VfpInsn::CvtFloat { cond: Cond::Al, to_dp: false, d: 1, m: 0 }));
    assert_eq!(d(0xEEFD0BC0), Insn::Vfp(VfpInsn::CvtToInt { cond: Cond::Al, dp: true, signed: true, round_zero: true, d: 1, m: 0 }));
    assert_eq!(d(0xEEFC0BC0), Insn::Vfp(VfpInsn::CvtToInt { cond: Cond::Al, dp: true, signed: false, round_zero: true, d: 1, m: 0 }));
    assert_eq!(d(0xEEFD0B40), Insn::Vfp(VfpInsn::CvtToInt { cond: Cond::Al, dp: true, signed: true, round_zero: false, d: 1, m: 0 }));
}

#[test]
fn never_panics() {
    let mut w = 0u32;
    loop {
        let _ = decode_arm(w, 0x0010_0000);
        match w.checked_add(0x0001_0001) { Some(n) => w = n, None => break }
    }
    for cond in [0x0u32, 0x1, 0xE, 0xF] {
        for filler in [0u32, 0x000F_FF0F, 0x000A_AA0A, 0x000F_0000, 0x0000_FFFF] {
            for hi in 0..256u32 {
                for lo in 0..16u32 {
                    let _ = decode_arm(cond << 28 | hi << 20 | filler | lo << 4, 0x0010_0000);
                    let _ = decode_arm(cond << 28 | hi << 20 | (filler ^ 0xF_FF0F) | lo << 4, 0xFFFF_FFFC);
                }
            }
        }
    }
}

#[test]
fn decode_stats() {
    let Ok(dump) = std::env::var("XR_DUMP") else { eprintln!("XR_DUMP unset: skipping"); return };
    let code = std::fs::read(format!("{dump}/exefs/code.bin")).expect("code.bin");
    let txt = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../functions/x-usa-v0.2.0/code.txt")).expect("code.txt");
    let funcs = xr_formats::parse_functab(&txt).expect("functab");
    const BASE: u32 = 0x0010_0000;
    // code.txt ranges include mid-function literal pools (data), so only words a sweep from the entry reaches count:
    // fall-through and direct branch targets inside the range, stopping at unconditional transfers, at PC-relative
    // load targets, and (as codegen does) at the fall-through of a call into an Undefined word (data after a noreturn
    // call). All words are counted too, for the record.
    let (mut all, mut all_undef, mut total, mut undef) = (0u64, 0u64, 0u64, 0u64);
    let mut words: std::collections::HashMap<u32, u64> = Default::default();
    let word = |pc: u32| { let off = (pc - BASE) as usize; u32::from_le_bytes(code[off..off + 4].try_into().unwrap()) };
    for f in funcs.iter().filter(|f| f.mode == xr_formats::Mode::Arm) {
        let mut pc = f.start;
        while pc + 4 <= f.end {
            all += 1;
            all_undef += matches!(decode_arm(word(pc), pc), Insn::Undefined { .. }) as u64;
            pc += 4;
        }
        let inside = |a: u32| a >= f.start && a + 4 <= f.end && a % 4 == 0;
        let (mut seen, mut literals) = (std::collections::HashSet::new(), std::collections::HashSet::new());
        let mut stack = vec![f.start];
        while let Some(pc) = stack.pop() {
            if !inside(pc) || literals.contains(&pc) || !seen.insert(pc) {
                continue;
            }
            let insn = decode_arm(word(pc), pc);
            total += 1;
            let next = pc + 4;
            let fall = match insn {
                Insn::Undefined { word: w } => { undef += 1; *words.entry(w).or_default() += 1; false }
                Insn::Branch { link: false, target, cond } => { stack.push(target); cond != Cond::Al }
                Insn::Branch { link: true, .. } | Insn::BlxImm { .. } | Insn::BranchExchange { link: true, .. } =>
                    inside(next) && !matches!(decode_arm(word(next), next), Insn::Undefined { .. }),
                Insn::BranchExchange { link: false, cond, .. } => cond != Cond::Al,
                Insn::DataProc { cond, op, rd: Reg(15), .. } if !matches!(op, DpOp::Tst | DpOp::Teq | DpOp::Cmp | DpOp::Cmn) => cond != Cond::Al,
                Insn::Load { cond, rt, rn, addr, .. } => {
                    if let (Reg(15), Offset::Imm(v), true) = (rn, addr.offset, addr.pre) {
                        literals.insert(if addr.up { pc + 8 + v } else { (pc + 8).wrapping_sub(v) } & !3);
                    }
                    rt != Reg(15) || cond != Cond::Al
                }
                Insn::LoadMulti { cond, list, .. } => list & 0x8000 == 0 || cond != Cond::Al,
                _ => true,
            };
            if fall {
                stack.push(next);
            }
        }
    }
    let mut by_group: std::collections::BTreeMap<u32, Vec<(u32, u64)>> = Default::default();
    for (&w, &n) in &words { by_group.entry((w >> 20) & 0xFF).or_default().push((w, n)); }
    let mut groups: Vec<_> = by_group.into_iter().map(|(g, mut v)| { v.sort_by(|a, b| b.1.cmp(&a.1)); (g, v.iter().map(|x| x.1).sum::<u64>(), v) }).collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1));
    for (g, n, v) in groups.iter().take(25) {
        let top: Vec<String> = v.iter().take(4).map(|(w, c)| format!("{w:08X}x{c}")).collect();
        eprintln!("bits27-20={g:02X}  undefined={n}  top: {}", top.join(" "));
    }
    let all_rate = all_undef as f64 / all as f64 * 100.0;
    eprintln!("ARM words in code.txt ranges: {all}, Undefined: {all_undef} ({all_rate:.4}%)");
    let rate = undef as f64 / total as f64 * 100.0;
    eprintln!("ARM words reachable from entries: {total}, Undefined: {undef} ({rate:.4}%)");
    assert!(rate < 0.1, "Undefined rate {rate:.4}% >= 0.1%");
}
