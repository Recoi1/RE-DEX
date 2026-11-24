//! Plan-1 function-table repairs, shared by `xr-recomp` (code.bin) and `xr-crotab` (CRO code segments).
use std::collections::{BTreeMap, HashSet};
use xr_arm::{decode_arm, decode_thumb, Cond, DpOp, Insn, Operand2, Reg, Shift};
use xr_codegen::{gen_function, GenOptions};
use xr_formats::{FuncEntry, Mode};

/// Linear decode of a function's range, the same sweep as xr-codegen (Thumb BL/BLX pairs take 4 bytes; bytes
/// outside `code` decode as Undefined).
pub fn sweep(code: &[u8], base: u32, f: &FuncEntry) -> Vec<(u32, Insn)> {
    let bytes = |a: u32, n: usize| a.checked_sub(base).and_then(|o| code.get(o as usize..(o as usize).checked_add(n)?));
    let half = |a: u32| bytes(a, 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let mut v = Vec::new();
    let mut a = f.start;
    while a < f.end && a >= f.start && v.len() < 1 << 20 {
        let (insn, size) = match f.mode {
            Mode::Arm => match bytes(a, 4) {
                Some(b) => (decode_arm(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), a), 4),
                None => (Insn::Undefined { word: 0 }, 4),
            },
            Mode::Thumb => match half(a) {
                Some(h) => decode_thumb(h, half(a.wrapping_add(2)).unwrap_or(0), a),
                None => (Insn::Undefined { word: 0 }, 2),
            },
        };
        v.push((a, insn));
        a = a.wrapping_add(size as u32);
    }
    v
}

/// Whether execution can continue to the next instruction (conditional instructions always can).
pub fn falls(i: &Insn) -> bool {
    use Insn::*;
    !matches!(*i,
        Undefined { .. }
        | Branch { link: false, cond: Cond::Al, .. }
        | BranchExchange { link: false, cond: Cond::Al, .. }
        | Load { cond: Cond::Al, rt: Reg::PC, .. })
        && !matches!(*i, LoadMulti { cond: Cond::Al, list, .. } if list & 0x8000 != 0)
        && !matches!(*i, DataProc { cond: Cond::Al, rd: Reg::PC, op, .. } if !matches!(op, DpOp::Tst | DpOp::Teq | DpOp::Cmp | DpOp::Cmn))
}

/// `mov lr, pc`: the PC write after it is a (pre-v5) call that returns.
pub fn is_mov_lr_pc(i: &Insn) -> bool {
    matches!(i, Insn::DataProc { cond: Cond::Al, op: DpOp::Mov, s: false, rd: Reg::LR,
        op2: Operand2::RegImmShift { rm: Reg::PC, shift: Shift::Lsl, amount: 0 }, .. })
}

/// Direct branch targets that leave `f`, from the instructions reachable from its entry: (insn address, target,
/// mode at the target). B and BL keep the mode, BLX imm toggles it.
fn exits(insns: &[(u32, Insn)], f: &FuncEntry) -> Vec<(u32, u32, Mode)> {
    let other = if f.mode == Mode::Thumb { Mode::Arm } else { Mode::Thumb };
    let (mut out, mut seen, mut stack) = (Vec::new(), vec![false; insns.len()], vec![0usize]);
    while let Some(i) = stack.pop() {
        if i >= insns.len() || std::mem::replace(&mut seen[i], true) {
            continue;
        }
        let (a, insn) = insns[i];
        match insn {
            Insn::Branch { link: false, target, .. } if (f.start..f.end).contains(&target) => {
                stack.extend(insns.binary_search_by_key(&target, |x| x.0).ok());
            }
            Insn::Branch { target, .. } => out.push((a, target, f.mode)),
            Insn::BlxImm { target } => out.push((a, target, other)),
            _ => {}
        }
        if falls(&insn) || (i > 0 && is_mov_lr_pc(&insns[i - 1].1)) {
            stack.push(i + 1);
        }
    }
    out
}

/// Repair (c): the end of `f`'s range moved up to just after an unconditional call when the call's fall-through
/// runs into an undefined word before any branch target (data after a no-return call, such as a literal pool).
/// Conservative: no direct branch in the range may target anything at or after the cut, and the cut must turn
/// a whole-function miss stub into a function that generates.
fn noreturn_end(code: &[u8], base: u32, f: &FuncEntry) -> Option<u32> {
    let insns = sweep(code, base, f);
    let mut targets = HashSet::new();
    for (_, i) in &insns {
        if let Insn::Branch { link: false, target, .. } = *i {
            targets.insert(target);
        }
    }
    let generates = |end: u32| {
        let g = FuncEntry { end, ..*f };
        gen_function(code, base, &g, &[g], &GenOptions::default()).undecodable.is_empty()
    };
    let mut stub = None;
    for i in 0..insns.len().saturating_sub(1) {
        if !matches!(insns[i].1, Insn::Branch { link: true, cond: Cond::Al, .. } | Insn::BlxImm { .. }) {
            continue;
        }
        let cut = insns[i + 1].0;
        if targets.iter().any(|&t| t >= cut && t < f.end) {
            continue;
        }
        let Some(j) = insns[i + 1..].iter().position(|x| !falls(&x.1)) else { continue };
        if !matches!(insns[i + 1 + j].1, Insn::Undefined { .. }) {
            continue;
        }
        if *stub.get_or_insert_with(|| !generates(f.end)) && generates(cut) {
            return Some(cut);
        }
    }
    None
}

pub struct Repair {
    /// The repaired table, sorted by start.
    pub funcs: Vec<FuncEntry>,
    /// (b) entries of length <= 1.
    pub dropped: Vec<FuncEntry>,
    /// (c) (entry as listed or added, new end).
    pub ended: Vec<(FuncEntry, u32)>,
    /// (a) (entry, address of the first branch found to it), sorted by start.
    pub added: Vec<(FuncEntry, u32)>,
    /// Branch targets not added: outside [base, seg_end), misaligned, or an added start of the other mode.
    pub skipped: usize,
}

/// Function-table repairs, in order: (b) drop entries of length <= 1; (c) end ranges at no-return calls
/// (`noreturn_end`); (a) add every direct B/BL/BLX target that lies outside all listed functions as a new entry
/// (mode from the branch kind, end at the next listed or added start or `seg_end`, then (c)), rescanning the
/// added entries until no new target appears. A B inside the scanning function's own range is a local jump.
pub fn repair(code: &[u8], base: u32, seg_end: u32, listed: &[FuncEntry]) -> Repair {
    let (mut funcs, dropped): (Vec<FuncEntry>, Vec<FuncEntry>) = listed.iter().partition(|f| f.end.saturating_sub(f.start) > 1);
    let mut ended = Vec::new();
    for f in &mut funcs {
        if let Some(e) = noreturn_end(code, base, f) {
            ended.push((*f, e));
            f.end = e;
        }
    }
    let inside_listed = |t: u32| {
        let i = funcs.partition_point(|e| e.start <= t);
        i > 0 && t < funcs[i - 1].end
    };
    let mut found: BTreeMap<u32, (Mode, u32)> = BTreeMap::new();
    let mut skipped = HashSet::new();
    let mut todo: Vec<(u32, u32, Mode)> = funcs.iter().flat_map(|f| exits(&sweep(code, base, f), f)).collect();
    let mut added: Vec<(FuncEntry, u32, Option<u32>)> = Vec::new();
    loop {
        let mut new = false;
        for (from, t, m) in todo.drain(..) {
            let unit = if m == Mode::Thumb { 2 } else { 4 };
            if inside_listed(t) || found.get(&t).is_some_and(|x| x.0 == m) {
                continue;
            }
            if t < base || t >= seg_end || t % unit != 0 || found.contains_key(&t) {
                skipped.insert((t, m == Mode::Thumb));
                continue;
            }
            found.insert(t, (m, from));
            new = true;
        }
        if !new {
            break;
        }
        added.clear();
        for (&start, &(mode, from)) in &found {
            let next_listed = funcs.get(funcs.partition_point(|e| e.start <= start)).map_or(seg_end, |e| e.start);
            let next_added = found.range(start + 1..).next().map_or(seg_end, |(&s, _)| s);
            let mut f = FuncEntry { start, end: next_listed.min(next_added).min(seg_end), mode };
            let cut = noreturn_end(code, base, &f);
            let orig_end = f.end;
            f.end = cut.unwrap_or(f.end);
            todo.extend(exits(&sweep(code, base, &f), &f));
            added.push((f, from, cut.map(|_| orig_end)));
        }
    }
    for (f, _, orig_end) in &added {
        if let Some(e) = orig_end {
            ended.push((FuncEntry { end: *e, ..*f }, f.end));
        }
    }
    funcs.extend(added.iter().map(|a| a.0));
    funcs.sort_by_key(|f| f.start);
    Repair { funcs, dropped, ended, added: added.into_iter().map(|a| (a.0, a.1)).collect(), skipped: skipped.len() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xr_codegen::flag_table;

    const BASE: u32 = 0x1000;
    fn bytes(words: &[u32]) -> Vec<u8> { words.iter().flat_map(|w| w.to_le_bytes()).collect() }
    fn th(h: &[u16]) -> Vec<u8> { h.iter().flat_map(|w| w.to_le_bytes()).collect() }
    fn e(start: u32, end: u32, mode: Mode) -> FuncEntry { FuncEntry { start, end, mode } }

    #[test]
    fn drops_tiny_entries() {
        let code = bytes(&[0, 0xE12FFF1E]);
        let r = repair(&code, BASE, 0x1008, &[e(0x1000, 0x1001, Mode::Arm), e(0x1004, 0x1008, Mode::Arm)]);
        assert_eq!(r.dropped, vec![e(0x1000, 0x1001, Mode::Arm)]);
        assert_eq!(r.funcs, vec![e(0x1004, 0x1008, Mode::Arm)]);
    }

    #[test]
    fn adds_branch_targets_to_fixed_point() {
        let mut code = bytes(&[
            0xEB000001, // 1000 bl 0x100c      (unlisted: added, ARM)
            0xEBFFFFFE, // 1004 bl 0x1004      (inside a listed function: not added)
            0xE12FFF1E, // 1008 bx lr
            0xFA000000, // 100c blx 0x1014     (found only by scanning the added function: Thumb)
            0xE12FFF1E, // 1010 bx lr
        ]);
        code.extend(th(&[0x4770, 0x46C0])); // 1014 bx lr; nop
        let r = repair(&code, BASE, 0x1018, &[e(0x1000, 0x100C, Mode::Arm)]);
        let added: Vec<FuncEntry> = r.added.iter().map(|a| a.0).collect();
        assert_eq!(added, vec![e(0x100C, 0x1014, Mode::Arm), e(0x1014, 0x1018, Mode::Thumb)]);
        assert_eq!(r.added[0].1, 0x1000, "from the bl");
        assert_eq!(r.funcs, vec![e(0x1000, 0x100C, Mode::Arm), e(0x100C, 0x1014, Mode::Arm), e(0x1014, 0x1018, Mode::Thumb)]);
    }

    /// The piece after a literal pool is added as a tail-call target, and the tail call stores the Z it reads.
    #[test]
    fn added_piece_gets_flags() {
        let code = bytes(&[
            0xE3500005, // 1000 cmp r0, #5
            0xEA000000, // 1004 b 0x100c
            0xDEADBEEF, // 1008 literal pool
            0x0A000001, // 100c beq 0x1018
            0xE3A01001, // 1010 mov r1, #1
            0xE12FFF1E, // 1014 bx lr
            0xE3A01002, // 1018 mov r1, #2
            0xE12FFF1E, // 101c bx lr
        ]);
        let r = repair(&code, BASE, 0x1020, &[e(0x1000, 0x100C, Mode::Arm)]);
        assert_eq!(r.added, vec![(e(0x100C, 0x1020, Mode::Arm), 0x1004)]);
        let opt = GenOptions { flags: flag_table(&code, BASE, &r.funcs, &[]), ..Default::default() };
        let src = gen_function(&code, BASE, &r.funcs[0], &r.funcs, &opt).c_source;
        assert!(src.contains("c->cpsr = (c->cpsr & ~0x40000000u) | fl_z << 30; XR_SPILL(); c->ret_lr = e_lr; c->ret_sp = e_sp; XR_TAIL return f_0000100C(c);"), "{src}");
    }

    /// Thumb: a call whose fall-through runs through decodable data into an undefined halfword.
    const NORETURN: [u16; 10] = [
        0xB500, // 1000 push {lr}
        0xF000, 0xF805, // 1002 bl 0x1010 (noreturn)
        0x2000, // 1006 data that decodes (movs r0, #0)
        0xDEFE, // 1008 data: undefined
        0, 0, 0, // 100a
        0x4770, // 1010 bx lr
        0x46C0, // 1012
    ];

    #[test]
    fn ends_range_at_noreturn_call() {
        let code = th(&NORETURN);
        let listed = [e(0x1000, 0x1010, Mode::Thumb), e(0x1010, 0x1012, Mode::Thumb)];
        let r = repair(&code, BASE, 0x1014, &listed);
        assert_eq!(r.ended, vec![(listed[0], 0x1006)]);
        assert_eq!(r.funcs, vec![e(0x1000, 0x1006, Mode::Thumb), listed[1]]);
        assert!(r.added.is_empty());
    }

    #[test]
    fn keeps_range_when_fallthrough_is_a_branch_target() {
        let mut h = NORETURN;
        h[0] = 0xD001; // 1000 beq 0x1006: the word after the call is code after all
        let code = th(&h);
        let listed = [e(0x1000, 0x1010, Mode::Thumb), e(0x1010, 0x1012, Mode::Thumb)];
        let r = repair(&code, BASE, 0x1014, &listed);
        assert!(r.ended.is_empty());
        assert_eq!(r.funcs, listed.to_vec());
    }
}
