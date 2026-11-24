//! C emission for recompiled guest functions (ARM and Thumb state, VFPv2).
//!
//! Each guest function becomes `void f_XXXXXXXX(XrCpu* c)` (Thumb: `t_XXXXXXXX`). Output is a C fragment: the
//! prototypes it needs plus the definition; the caller puts `#include "xr_abi.h"` at the top of the file.
//!
//! # State
//! * r0..r14 are C locals (`XR_REGS`), loaded on entry and spilled to `c->r` (`XR_SPILL`) before every call, SVC,
//!   miss, preempt and exit, reloaded (`XR_RELOAD`) after calls, SVCs and preempts. PC is never a local: each
//!   instruction knows its own address, and reading PC as an operand gives `Func::pc_read(addr)` (ARM: insn + 8;
//!   Thumb: insn + 4). `e_lr`/`e_sp` = `c->ret_lr`/`c->ret_sp` at entry: the frame's return address and sp,
//!   which returns must match. Calls set `c->ret_lr`/`c->ret_sp` to their return address and sp; tail calls (and
//!   tail-shaped misses) pass `e_lr`/`e_sp` on, so a piece entered by a tail call after a call (split functions)
//!   still knows its frame.
//!   Loads with `rn = PC` always use insn + 8 (the Thumb decoder pre-folds its literal offsets to match).
//! * N, Z, C, V are locals `fl_n fl_z fl_c fl_v` (0/1), computed only where live (see `flags`). They cross
//!   function boundaries through `c->cpsr` only where the `FlagTable` (`flag_table`, a fixed point over the whole
//!   table) says something reads them: before a direct call/tail call/fall-off into a function whose entry
//!   flags are live, at returns of a function whose callers read flags after the call, and (all four) before
//!   indirect calls and the dispatch's `xr_lookup` tail call. A call defines them by reloading from `c->cpsr`
//!   when a later read needs them, and flags live at entry are loaded from `c->cpsr`. Without a `FlagTable`
//!   (`GenOptions::default()`), direct exits store nothing. Q and GE live in `c->cpsr` itself.
//! * VFP registers stay in `c->v` (never cached in locals); FPSCR is `c->fpscr` (see `vfp` for the FP mode).
//! * Literal loads `ldr rX, [pc, #i]` whose word lies inside a listed function (read-only text) and is not in
//!   `GenOptions::patched` (words ldr:ro rewrites when it links CROs) are folded to
//!   constants.
//!
//! # Thumb
//! Thumb functions are decoded by a linear halfword sweep (BL/BLX pairs are one 4-byte instruction) and use the
//! same IR and emitter. Calls write LR = next instruction | 1. Thumb `BX PC` and ALU writes of PC other than
//! `MOV PC, LR` (none occur in the game's Thumb code) are inline misses; `CMP PC, Rm` reads insn + 4.
//!
//! # Control flow
//! * `B` inside the function: `goto`; a backward one (target address <= its own) first pays the loop budget:
//!   `if (--c->budget <= 0)` store the flags live there, spill, `xr_rt->preempt(c)`, reload. `B` elsewhere: tail
//!   call (spill, `XR_TAIL return f(c)`: clang's musttail, a jump) when the target is a listed function start of
//!   the same mode, otherwise a tail-shaped miss; a backward one pays the budget too (loops of tail calls).
//! * `BL`: direct call of a function of the same mode; `BLX imm` toggles: ARM -> `t_` at the target, Thumb ->
//!   `f_` at the word-aligned target. `r14 = return address`, spill, call, reload, continue. A target that is not
//!   a listed start of the right mode is a call-shaped miss (Thumb targets passed with bit 0 set). Calls and tail
//!   calls to an address in `GenOptions::hooks` go through `xr_rt->hooks[i]` when the table and the slot are
//!   non-NULL.
//! * Returns: `BX LR`, `MOV PC, LR`, any `LDM`/`POP` with PC in the list (its other loads and writeback still
//!   happen) and `LDR PC, [SP], #4` spill and C-`return` when the target equals `e_lr`; the native return lands
//!   after the caller's call, whatever its instruction set. Any other target (longjmp, EHABI unwinding, a
//!   rewritten saved lr) goes to `xr_rt->interp(c, target, e_lr, e_sp)` and then returns.
//! * Other PC writes (`BX Rm`, `MOV/ADD PC, ...`, `LDR PC, [...]`) set `dt` and `goto dispatch`, which pays the
//!   loop budget, returns when `dt == e_lr && r13 == e_sp` (`pop {r3}; bx r3`), and otherwise is a `switch` over
//!   every instruction address of the function (Thumb: address | 1, so only a Thumb-state target matches;
//!   emitted only for functions that have such a jump), whose default asks `xr_lookup(dt)` (odd = Thumb `t_`,
//!   even = ARM `f_`) and tail calls the result, or is a tail-shaped miss on NULL. ARM data-processing writes
//!   clear bits 1:0 (`BranchWritePC`); Thumb `MOV PC, LR` stays Thumb; loads and BX keep bit 0 (interworking).
//! * `BLX Rm`, and a PC write right after an unconditional ARM `MOV LR, PC` (pre-v5 call idiom), are indirect
//!   calls: `xr_lookup`, spill, call (NULL: call-shaped miss), reload, continue.
//! * Falling off the end of the range continues into the function at `end` (tail call) or is a tail-shaped miss.
//!
//! # Misses (ABI 3: interpreter fallback)
//! Every miss stores all four flags into `c->cpsr` and spills before `xr_rt->interp`, which returns. Call-shaped
//! (BL/BLX/indirect call to code with no native function): `interp(c, target, return address, r13)`, reload
//! registers and live flags, continue. Tail-shaped (everything else): `interp(c, addr, e_lr, e_sp); return;`.
//! A reachable (from the entry, through direct edges) `Undefined` instruction turns the whole function into
//! `interp(c, start, c->r[14], c->r[13])` and lists it in `undecodable`; so do empty or absurdly long ranges. An
//! unconditional call's fall-through into an `Undefined` word is not followed (data after a noreturn call).
//! Tail-shaped misses at the instruction itself:
//! the same kinds when only reachable through a computed jump, `BKPT`, `SETEND BE`, `MRS/MSR SPSR`, MCR/MRC other
//! than the CP15 c7 barriers (the TLS read `mrc p15,0,rX,c13,c0,3` is wired: it reads `c->tls`, ABI 2),
//! UNPREDICTABLE PC writes (MUL/LDRB/LDREX/media/... with a PC destination, LDRD/STRD of r14/r15), and the VFP
//! cases listed in `vfp`.
//! No-ops: hints (NOP/YIELD/WFE/WFI/SEV), PLD, CPS (ignored in user mode), `SETEND LE`, CP15 c7 barriers, the
//! control/extension fields of `MSR CPSR` (user mode cannot write them). Exception-return forms (`MOVS PC, LR`,
//! `LDM ...^` with PC) behave as their plain forms.
//!
//! # Module mode (`GenOptions::module = Some(k)`: CRO code, placed where the game's heap puts it)
//! `code` is the module's code segment, `base` is 0, and function starts/ends, `all` and `patched` are offsets
//! into it. The code is compiled once and runs at any base: every function reads
//! `const uint32_t mb = xr_mod_base[k];` (`xr_abi.h`; the guest address of the code segment's offset 0, which
//! must be 4-aligned; defined and filled in xgame.dll when the module is placed) and every guest-visible address
//! it produces is `mb + offset`: PC operand reads (`mb + insn + 8`, so `add rX, pc, #i` and PC-relative loads come
//! out at the real place), the return addresses written to r14, the SVC PC, and every address passed to
//! `xr_rt->interp` (miss targets and their return addresses). Unchanged from code.bin mode: `B`/`BL` inside the
//! module are gotos and direct C calls (their targets are offsets of the same module), the frame contract
//! (`e_lr`/`e_sp` are absolute guest addresses, so checked returns compare like for like), the back-edge budget,
//! flag passing (build one `flag_table` per module, with no hooks) and the Thumb bit.
//! * Names are `m{k}_f_OOOOOO` / `m{k}_t_OOOOOO` (offset), unique across modules (`fn_name`).
//! * No literal folding: every PC-relative literal is loaded from `m` at `mb + offset` at run time (CRO
//!   relocations are absolute, so the loader writes base-dependent or foreign addresses into literal pools; loading
//!   them all costs one load each and cannot go wrong when a relocated word is missing from `patched`).
//! * Computed jumps: `dt` is absolute (a jump table after `ldr pc, [pc, rm, lsl #2]` holds relocated words =
//!   base + case offset), so the dispatch switches on `dt - mb` against the function's offsets; anything else
//!   (another function of this module, code.bin, another module) goes to `xr_lookup(dt)` (absolute), as do
//!   indirect calls (`blx rX`, `mov lr, pc` + PC write, the import thunks `ldr pc, [pc, #-4]` + patched word).
//!   `xr_lookup` must map addresses inside placed modules to their functions (plan 3 Task 3).
//! * `GenOptions::hooks` are ignored (they are code.bin addresses).
//! * Thumb (absent from the game's CROs, supported anyway): the decoder's PC folds (`ADR`, hi-register ops with
//!   Rm = PC) are turned back into PC reads (`rebase_thumb_pc`).
mod emit;
mod flags;
mod vfp;

use xr_arm::{decode_arm, decode_thumb, Cond, DpOp, Insn, MemWidth, Offset, Operand2, Reg, Shift};
use std::collections::HashMap;
use xr_formats::{FuncEntry, Mode};

#[derive(Default)]
pub struct GenOptions {
    pub hooks: Vec<u32>,
    /// Flags passed between functions (see `flag_table`); empty = none.
    pub flags: FlagTable,
    /// Sorted addresses of text words the loader rewrites at run time (the static module's import stubs'
    /// literals, which ldr:ro points at CRO functions; in module mode every relocated or import-patched word of
    /// the module, as offsets): literal loads from them stay loads.
    pub patched: Vec<u32>,
    /// Module mode (a CRO code segment): the module's index `k` into `xr_mod_base[]`. See "Module mode".
    pub module: Option<u32>,
}

pub struct GenOutput {
    pub c_source: String,
    pub undecodable: Vec<u32>,
}

/// Largest function accepted (instructions); anything bigger is a bad table entry and becomes a miss stub.
const MAX_INSNS: u64 = 1 << 20;

/// Translate one guest function. `all` must be sorted by `start` (as `parse_functab` returns it); lookups
/// into an unsorted table only miss (calls become misses, literals stay loads), they never go wrong.
pub fn gen_function(code: &[u8], base: u32, f: &FuncEntry, all: &[FuncEntry], opt: &GenOptions) -> GenOutput {
    let name = fn_name(f.start, f.mode, opt.module);
    // Hooks are code.bin addresses; module offsets never match them.
    let hooks: &[u32] = if opt.module.is_some() { &[] } else { &opt.hooks };
    let Some(mut fx) = build(code, base, f, all, hooks) else { return stub(&name, f.start, f.mode, opt.module) };
    fx.patched = &opt.patched;
    fx.module = opt.module;
    if opt.module.is_some() && f.mode == Mode::Thumb {
        for (addr, h, insn) in &mut fx.insns {
            rebase_thumb_pc(*addr, *h, insn);
        }
    }
    let live = flags::liveness(&fx, &opt.flags, false);
    GenOutput { c_source: emit::function(&fx, &name, &live, &opt.flags), undecodable: Vec::new() }
}

/// Flags (NZCV) that cross function boundaries, per function: `entry` = flags live at its entry that a direct
/// caller or tail-caller must store into `c->cpsr` before transferring (split functions: a B across a literal
/// pool into code that reads the flags); `ret` = flags some direct call site reads after it returns (flag-
/// returning helpers such as `__aeabi_cfcmple`), stored into `c->cpsr` at every return. Masks use `flags::N..V`.
#[derive(Default, Clone)]
pub struct FlagTable {
    masks: HashMap<u32, (u8, u8)>,
    pub stats: FlagStats,
}

#[derive(Default, Clone, Debug)]
pub struct FlagStats {
    /// Functions whose entry flags callers must store.
    pub entry_live: usize,
    /// Functions that store flags at their returns.
    pub returns_flags: usize,
    /// Direct tail calls and fall-off-the-end edges that store flags.
    pub tail_edges: usize,
    /// Direct calls that store flags before the call.
    pub call_edges: usize,
    /// Indirect calls and computed-jump dispatchers (store all four flags).
    pub indirect_sites: usize,
    /// Passes over the table until the fixed point.
    pub passes: usize,
}

fn key(start: u32, mode: Mode) -> u32 { start | (mode == Mode::Thumb) as u32 }

impl FlagTable {
    pub fn entry(&self, start: u32, mode: Mode) -> u8 { self.masks.get(&key(start, mode)).map_or(0, |m| m.0) }
    pub fn ret(&self, start: u32, mode: Mode) -> u8 { self.masks.get(&key(start, mode)).map_or(0, |m| m.1) }
    fn or(&mut self, k: u32, entry: u8, ret: u8) -> bool {
        let m = self.masks.entry(k).or_default();
        let old = *m;
        *m = (m.0 | entry, m.1 | ret);
        *m != old
    }
}

/// Fixed point of `FlagTable` over the whole table (`all` sorted, as for `gen_function`). Direct edges only:
/// an indirect call or computed jump stores all four flags at the site (emission) without making them live
/// upstream, so the unknown target only sees what the code really computed (or reloaded) there.
pub fn flag_table(code: &[u8], base: u32, all: &[FuncEntry], hooks: &[u32]) -> FlagTable { flag_table_with(code, base, all, hooks, &[]) }

/// `flag_table` plus return flags that callers outside this table read: (start, mode, mask) per function. A CRO
/// calls code.bin through an import thunk (an indirect tail call), so code.bin's table never sees that the module
/// reads flags after the call (`xr_recomp::dll::thunk_flag_demands` collects them).
pub fn flag_table_with(code: &[u8], base: u32, all: &[FuncEntry], hooks: &[u32], extra_ret: &[(u32, Mode, u8)]) -> FlagTable {
    let mut t = FlagTable::default();
    for &(start, mode, mask) in extra_ret {
        t.or(key(start, mode), 0, mask);
    }
    let mut funcs = Vec::new();
    for f in all {
        match build(code, base, f, all, hooks) {
            Some(fx) => funcs.push(fx),
            // a whole-function stub hands the interpreter c->cpsr as is: callers must store all four flags
            None => {
                t.or(key(f.start, f.mode), flags::ALL, 0);
            }
        }
    }
    loop {
        t.stats.passes += 1;
        let mut changed = false;
        for fx in &funcs {
            let lv = flags::liveness(fx, &t, true);
            let ret = t.ret(fx.f.start, fx.f.mode);
            changed |= t.or(key(fx.f.start, fx.f.mode), lv.entry, 0);
            for (i, k) in fx.kinds.iter().enumerate() {
                let edge = match *k {
                    Kind::Call(tg, m) => fx.target(tg, m).map(|e| (e, lv.out[i])),
                    Kind::Leave(tg) => fx.target(tg, fx.f.mode).map(|e| (e, ret)),
                    _ => None,
                };
                if let Some((e, r)) = edge {
                    changed |= t.or(key(e.start, e.mode), 0, r);
                }
            }
            if let Some(e) = fx.falls_off().then(|| fx.target(fx.fall_target(), fx.f.mode)).flatten() {
                changed |= t.or(key(e.start, e.mode), 0, ret);
            }
        }
        if !changed {
            break;
        }
    }
    let mut st = FlagStats { passes: t.stats.passes, ..Default::default() };
    st.entry_live = t.masks.values().filter(|m| m.0 != 0).count();
    st.returns_flags = t.masks.values().filter(|m| m.1 != 0).count();
    for fx in &funcs {
        let need = |e: Option<&FuncEntry>| e.is_some_and(|e| t.entry(e.start, e.mode) != 0) as usize;
        for k in &fx.kinds {
            match *k {
                Kind::Call(tg, m) => st.call_edges += need(fx.target(tg, m)),
                Kind::Leave(tg) => st.tail_edges += need(fx.target(tg, fx.f.mode)),
                Kind::CallInd => st.indirect_sites += 1,
                _ => {}
            }
        }
        if fx.falls_off() {
            st.tail_edges += need(fx.target(fx.fall_target(), fx.f.mode));
        }
        st.indirect_sites += fx.has_dispatch() as usize;
    }
    t.stats = st;
    t
}

/// Decode and classify `f`; None when it is a whole-function miss stub.
fn build<'a>(code: &'a [u8], base: u32, f: &'a FuncEntry, all: &'a [FuncEntry], hooks: &'a [u32]) -> Option<Func<'a>> {
    let unit = if f.mode == Mode::Thumb { 2 } else { 4 };
    let count = (f.end as u64).saturating_sub(f.start as u64) / unit;
    if count == 0 || count > MAX_INSNS {
        return None;
    }
    let insns: Vec<(u32, u32, Insn)> = match f.mode {
        Mode::Arm => (0..count as u32)
            .map(|i| {
                let addr = f.start.wrapping_add(4 * i);
                match word_at(code, base, addr) {
                    Some(w) => (addr, w, decode_arm(w, addr)),
                    None => (addr, 0, Insn::Undefined { word: 0 }),
                }
            })
            .collect(),
        // Linear sweep; a BL/BLX pair takes 4 bytes (`word` = both halves). A pair can only swallow a halfword
        // that is a BL/BLX suffix, which never starts an instruction, so data (literal pools) cannot desync code.
        Mode::Thumb => {
            let mut v = Vec::new();
            let mut addr = f.start;
            while addr < f.end && addr >= f.start {
                let Some(h) = half_at(code, base, addr) else {
                    v.push((addr, 0, Insn::Undefined { word: 0 }));
                    addr = addr.wrapping_add(2);
                    continue;
                };
                let next = half_at(code, base, addr.wrapping_add(2)).unwrap_or(0);
                let (insn, size) = decode_thumb(h, next, addr);
                let word = if size == 4 { h as u32 | (next as u32) << 16 } else { h as u32 };
                v.push((addr, word, insn));
                addr = addr.wrapping_add(size as u32);
            }
            v
        }
    };
    let mut fx = Func { f, all, hooks, patched: &[], module: None, code, base, insns, kinds: Vec::new() };
    fx.kinds = (0..fx.insns.len()).map(|i| fx.classify(i)).collect();
    if fx.reachable_undecodable() {
        return None;
    }
    Some(fx)
}

fn stub(name: &str, start: u32, mode: Mode, module: Option<u32>) -> GenOutput {
    let a = abs_addr(module, start | (mode == Mode::Thumb) as u32);
    let mb = module.map(|k| format!("const uint32_t mb = xr_mod_base[{k}]; ")).unwrap_or_default();
    GenOutput { c_source: format!("void {name}(XrCpu* c) {{ {mb}xr_rt->interp(c, {a}, c->ret_lr, c->ret_sp); }}\n"), undecodable: vec![start] }
}

/// Module mode, Thumb: the decoder folds PC reads into constants of the decode address (here an offset):
/// `ADR` is `Mov rd, #target` and hi-register `ADD/CMP/MOV` with Rm = PC use `Imm(insn + 4)`. Turn them back into
/// PC reads, which emit as `mb + insn + 4`. (`LDR rd, [pc, #i]` and branches are already PC-relative.)
fn rebase_thumb_pc(addr: u32, h: u32, insn: &mut Insn) {
    let Insn::DataProc { op, rn, op2, .. } = insn else { return };
    if h >> 11 == 0b10100 {
        if let Operand2::Imm { value, .. } = *op2 {
            (*op, *rn) = (DpOp::Add, Reg::PC);
            *op2 = Operand2::Imm { value: value.wrapping_sub(addr.wrapping_add(4)), rot: 0 };
        }
    } else if h & 0xFC78 == 0x4478 && *op2 == (Operand2::Imm { value: addr.wrapping_add(4), rot: 0 }) {
        *op2 = Operand2::RegImmShift { rm: Reg::PC, shift: Shift::Lsl, amount: 0 };
    }
}

/// C name of a function: `f_XXXXXXXX` / `t_XXXXXXXX` (code.bin), `m{k}_f_OOOOOO` / `m{k}_t_OOOOOO` (module k).
pub fn fn_name(start: u32, mode: Mode, module: Option<u32>) -> String {
    let t = if mode == Mode::Thumb { "t" } else { "f" };
    match module {
        None => format!("{t}_{start:08X}"),
        Some(k) => format!("m{k}_{t}_{start:06X}"),
    }
}

/// C value of guest address `v`: a constant, or in module mode `mb + v` (`mb` = the function's copy of
/// `xr_mod_base[k]`, `v` an offset).
fn abs_addr(module: Option<u32>, v: u32) -> String {
    match module {
        None => format!("0x{v:08X}u"),
        Some(_) => format!("(mb + 0x{v:X}u)"),
    }
}

fn word_at(code: &[u8], base: u32, addr: u32) -> Option<u32> {
    let off = addr.checked_sub(base)? as usize;
    let b = code.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn half_at(code: &[u8], base: u32, addr: u32) -> Option<u16> {
    let off = addr.checked_sub(base)? as usize;
    let b = code.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// How an instruction affects control flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Falls through (no PC write).
    Plain,
    /// Direct branch to the instruction at this index.
    Goto(usize),
    /// Direct branch out of the function: tail call or miss.
    Leave(u32),
    /// BL / BLX imm: direct call, then continue.
    Call(u32, Mode),
    /// BLX Rm or `MOV LR, PC` + PC write: call through `xr_lookup`, then continue.
    CallInd,
    Return,
    /// Computed PC write: through the dispatch switch.
    Jump,
    /// Emitted as a tail-shaped miss (`xr_rt->interp`) at this address.
    Miss,
}

pub(crate) struct Func<'a> {
    pub f: &'a FuncEntry,
    pub all: &'a [FuncEntry],
    pub hooks: &'a [u32],
    pub patched: &'a [u32],
    pub module: Option<u32>,
    pub code: &'a [u8],
    pub base: u32,
    /// (address, raw word, decoded) for each word of the range.
    pub insns: Vec<(u32, u32, Insn)>,
    pub kinds: Vec<Kind>,
}

/// Successors of one instruction inside the function.
pub(crate) struct Succ {
    pub fall: bool,
    pub goto: Option<usize>,
    pub dispatch: bool,
}

pub(crate) fn cond_of(i: &Insn) -> Cond {
    use Insn::*;
    match *i {
        DataProc { cond, .. } | Mul { cond, .. } | MulLong { cond, .. } | Umaal { cond, .. } | Load { cond, .. }
        | Store { cond, .. } | LoadMulti { cond, .. } | StoreMulti { cond, .. } | Branch { cond, .. }
        | BranchExchange { cond, .. } | Svc { cond, .. } | Ldrex { cond, .. } | Strex { cond, .. } | Swp { cond, .. }
        | Mrs { cond, .. } | Msr { cond, .. } | Mcr { cond, .. } | Mrc { cond, .. } | Hint { cond, .. }
        | Media { cond, .. } => cond,
        Vfp(v) => vfp::cond(&v),
        BlxImm { .. } | Bkpt { .. } | Clrex | Pld { .. } | Setend { .. } | Cps { .. } | Undefined { .. } => Cond::Al,
    }
}

pub(crate) fn writes_rd(op: DpOp) -> bool { !matches!(op, DpOp::Tst | DpOp::Teq | DpOp::Cmp | DpOp::Cmn) }

/// CP15 c7 barriers usable from user mode (DSB, DMB, flush prefetch buffer): no-ops here.
/// `mrc p15, 0, Rt, c13, c0, 3`: read the user-read-only thread ID register (the TLS pointer, `c->tls`).
pub(crate) fn is_tls_read(coproc: u8, opc1: u8, crn: u8, crm: u8, opc2: u8) -> bool {
    (coproc, opc1, crn, crm, opc2) == (15, 0, 13, 0, 3)
}

pub(crate) fn is_barrier(coproc: u8, opc1: u8, crn: u8, crm: u8, opc2: u8) -> bool {
    coproc == 15 && opc1 == 0 && crn == 7 && matches!((crm, opc2), (10, 4) | (10, 5) | (5, 4))
}

impl Func<'_> {
    pub fn thumb(&self) -> bool { self.f.mode == Mode::Thumb }

    /// C value of the guest address `v` (an offset in module mode).
    pub fn abs(&self, v: u32) -> String { abs_addr(self.module, v) }

    pub fn index_of(&self, target: u32) -> Option<usize> { self.insns.binary_search_by_key(&target, |x| x.0).ok() }

    /// The value instruction `addr` reads for PC as an operand: ARM insn + 8, Thumb insn + 4.
    pub fn pc_read(&self, addr: u32) -> u32 { addr.wrapping_add(if self.thumb() { 4 } else { 8 }) }

    /// LR written by a call at instruction `i`: the next instruction, with bit 0 set in Thumb.
    pub fn ret_addr(&self, i: usize) -> u32 {
        let (addr, _, insn) = self.insns[i];
        match (self.thumb(), insn) {
            (false, _) => addr.wrapping_add(4),
            (true, Insn::Branch { link: true, .. } | Insn::BlxImm { .. }) => addr.wrapping_add(4) | 1,
            (true, _) => addr.wrapping_add(2) | 1,
        }
    }

    /// The listed function starting exactly at `addr`.
    pub fn listed(&self, addr: u32) -> Option<&FuncEntry> {
        let i = self.all.partition_point(|e| e.start < addr);
        self.all.get(i).filter(|e| e.start == addr)
    }

    /// The constant at `addr` when it lies inside a listed function (read-only text), in `code`, and is not a word
    /// the loader patches.
    pub fn literal(&self, addr: u32) -> Option<u32> {
        // Module mode never folds: a relocated word missing from `patched` would silently become a wrong constant.
        if self.module.is_some() || self.patched.binary_search(&addr).is_ok() {
            return None;
        }
        let i = self.all.partition_point(|e| e.start <= addr);
        let e = self.all.get(i.checked_sub(1)?)?;
        if addr.checked_add(4)? > e.end {
            return None;
        }
        word_at(self.code, self.base, addr)
    }

    /// The listed function of `mode` starting at `t` (direct call or tail-call target).
    pub fn target(&self, t: u32, mode: Mode) -> Option<&FuncEntry> { self.listed(t).filter(|e| e.mode == mode) }

    /// The last instruction falls off the end of the range (into the function at `end`).
    /// Address after the last decoded instruction (not `f.end`, which can be misaligned or mid-BL-pair).
    pub fn fall_target(&self) -> u32 { self.insns.last().map_or(self.f.end, |_| self.ret_addr(self.insns.len() - 1) & !1) }

    pub fn falls_off(&self) -> bool { !self.insns.is_empty() && self.succ(self.insns.len() - 1).fall }

    pub fn hook_index(&self, addr: u32) -> Option<usize> { self.hooks.iter().position(|&h| h == addr) }

    fn classify(&self, i: usize) -> Kind {
        use Insn::*;
        let insn = self.insns[i].2;
        let pc = |r: Reg| r == Reg::PC;
        let other = if self.thumb() { Mode::Arm } else { Mode::Thumb };
        let k = match insn {
            Undefined { .. } | Bkpt { .. } | Setend { big: true } => Kind::Miss,
            Vfp(v) => vfp::kind(&v),
            // Thumb `BX PC` and ALU writes of PC other than `MOV PC, LR` never occur in the game's Thumb code.
            BranchExchange { rm: Reg::PC, .. } if self.thumb() => Kind::Miss,
            DataProc { op, rd: Reg::PC, op2, .. }
                if self.thumb() && writes_rd(op) && !(op == DpOp::Mov && op2 == Operand2::RegImmShift { rm: Reg::LR, shift: Shift::Lsl, amount: 0 }) =>
            {
                Kind::Miss
            }
            Mrs { spsr: true, .. } | Msr { spsr: true, .. } => Kind::Miss,
            Mcr { coproc, opc1, crn, crm, opc2, .. } if is_barrier(coproc, opc1, crn, crm, opc2) => Kind::Plain,
            Mrc { coproc, opc1, crn, crm, opc2, rt, .. } if is_tls_read(coproc, opc1, crn, crm, opc2) && rt != Reg::PC => Kind::Plain,
            Mcr { .. } | Mrc { .. } => Kind::Miss,
            Branch { link: false, target, .. } => self.index_of(target).map_or(Kind::Leave(target), Kind::Goto),
            Branch { link: true, target, .. } => Kind::Call(target, self.f.mode),
            BlxImm { target } => Kind::Call(target, other),
            BranchExchange { link: false, rm: Reg::LR, .. } => Kind::Return,
            BranchExchange { link: false, .. } => Kind::Jump,
            BranchExchange { link: true, .. } => Kind::CallInd,
            DataProc { op, rd: Reg::PC, rn: _, op2, .. } if writes_rd(op) => {
                let mov_lr = op == DpOp::Mov && op2 == Operand2::RegImmShift { rm: Reg::LR, shift: Shift::Lsl, amount: 0 };
                if mov_lr { Kind::Return } else { Kind::Jump }
            }
            Load { width: MemWidth::Word, rt: Reg::PC, rn, addr, .. } => {
                let pop = rn == Reg::SP && !addr.pre && addr.up && addr.offset == Offset::Imm(4);
                if pop { Kind::Return } else { Kind::Jump }
            }
            Load { width: MemWidth::Double, rt, .. } | Store { width: MemWidth::Double, rt, .. } if rt.0 >= 14 => Kind::Miss,
            Ldrex { width: MemWidth::Double, rt, .. } | Strex { width: MemWidth::Double, rt, .. } if rt.0 >= 14 => Kind::Miss,
            Load { rt, .. } | Ldrex { rt, .. } if pc(rt) => Kind::Miss,
            Strex { rd, .. } | Swp { rd, .. } | Mul { rd, .. } | Mrs { rd, .. } if pc(rd) => Kind::Miss,
            MulLong { rdhi, rdlo, .. } | Umaal { rdhi, rdlo, .. } if pc(rdhi) || pc(rdlo) => Kind::Miss,
            Media { rd, ra, op, .. } if pc(rd) || (emit::media_writes_ra(op) && pc(ra)) => Kind::Miss,
            LoadMulti { list, .. } if list & 0x8000 != 0 => Kind::Return,
            _ => Kind::Plain,
        };
        // Pre-v5 call idiom: `mov lr, pc` then a PC write.
        let mov_lr_pc = |p: &Insn| {
            matches!(p, DataProc { cond: Cond::Al, op: DpOp::Mov, s: false, rd: Reg::LR,
                op2: Operand2::RegImmShift { rm: Reg::PC, shift: Shift::Lsl, amount: 0 }, .. })
        };
        if k == Kind::Jump && i > 0 && cond_of(&insn) == Cond::Al && mov_lr_pc(&self.insns[i - 1].2) {
            return Kind::CallInd;
        }
        k
    }

    pub fn succ(&self, i: usize) -> Succ {
        let cond = cond_of(&self.insns[i].2);
        let k = self.kinds[i];
        let fall = matches!(k, Kind::Plain | Kind::Call(..) | Kind::CallInd) || cond != Cond::Al;
        Succ { fall, goto: if let Kind::Goto(t) = k { Some(t) } else { None }, dispatch: k == Kind::Jump }
    }

    pub fn has_dispatch(&self) -> bool { self.kinds.contains(&Kind::Jump) }

    /// An Undefined instruction reachable from the entry through direct edges. The fall-through of a call
    /// into an Undefined word is not followed: compilers put data after calls to noreturn functions (seen in
    /// code.bin at 0x53BD04); that word stays an inline miss.
    fn reachable_undecodable(&self) -> bool {
        let n = self.insns.len();
        let mut seen = vec![false; n];
        let mut stack = vec![0usize];
        while let Some(i) = stack.pop() {
            if i >= n || std::mem::replace(&mut seen[i], true) {
                continue;
            }
            if matches!(self.insns[i].2, Insn::Undefined { .. }) {
                return true;
            }
            let s = self.succ(i);
            let call = matches!(self.kinds[i], Kind::Call(..) | Kind::CallInd) && cond_of(&self.insns[i].2) == Cond::Al;
            let into_undefined = matches!(self.insns.get(i + 1), Some((_, _, Insn::Undefined { .. })));
            if s.fall && !(call && into_undefined) {
                stack.push(i + 1);
            }
            stack.extend(s.goto);
        }
        false
    }
}
