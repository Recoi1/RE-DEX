//! Per-instruction C emission (ARM and Thumb state; VFP in `vfp`). See the crate docs for the state and control-flow model.
use crate::flags::{self, Liveness, C, N, V, Z};
use crate::{cond_of, fn_name, vfp, writes_rd, FlagTable, Func, Kind};
use std::collections::BTreeSet;
use std::fmt::Write;
use xr_arm::{Addr, BlockMode, Cond, DpOp, Insn, MediaOp, MemWidth, MsrSrc, Offset, Operand2, ParKind, ParOp, Reg, Shift};
use xr_formats::Mode;

pub(crate) fn function(fx: &Func, name: &str, live: &Liveness, tab: &FlagTable) -> String {
    let mut e = Em { fx, live, tab, protos: BTreeSet::new() };
    let n = fx.insns.len();
    let dispatch = fx.has_dispatch();
    let mut label = vec![dispatch; n];
    for k in &fx.kinds {
        if let Kind::Goto(t) = *k {
            label[t] = true;
        }
    }
    let mut body = String::new();
    for i in 0..n {
        let (addr, word, _) = fx.insns[i];
        if label[i] {
            let _ = writeln!(body, "L_{addr:08X}:;");
        }
        let _ = writeln!(body, "    /* {addr:08X} {word:08X} */ {}", e.insn(i));
    }
    if n > 0 && fx.succ(n - 1).fall {
        let _ = writeln!(body, "    /* falls off the end */ {}", e.leave(fx.fall_target()));
    }
    if dispatch {
        // Every computed jump pays the back-edge budget (loops through the switch are back-edges). A jump to the
        // entry lr with the entry sp is a return (`pop {r3}; bx r3`, recursion into this function).
        let _ = writeln!(body, "dispatch:\n    {}", preempt(flags::ALL));
        let _ = writeln!(body, "    if (dt == e_lr && r13 == e_sp) {{ {}XR_SPILL(); return; }}", store_flags(e.ret_flags()));
        // Module mode: `dt` is absolute, the cases are offsets (a jump table's relocated words hold base + offset).
        body.push_str(if fx.module.is_some() { "    switch (dt - mb) {\n" } else { "    switch (dt) {\n" });
        let bit0 = fx.thumb() as u32;
        for &(addr, _, _) in &fx.insns {
            let _ = writeln!(body, "    case 0x{:08X}u: goto L_{addr:08X};", addr | bit0);
        }
        let _ = writeln!(
            body,
            "    }}\n    {{ XrFn fn = xr_lookup(dt); {}XR_SPILL(); {PASS_FRAME}if (fn) XR_TAIL return fn(c); xr_rt->interp(c, dt, e_lr, e_sp); return; }}",
            store_flags(flags::ALL)
        );
    }
    let mut s = String::new();
    for p in &e.protos {
        let _ = writeln!(s, "void {p}(XrCpu* c);");
    }
    let _ = writeln!(s, "void {name}(XrCpu* c) {{");
    s.push_str("    uint8_t* const m = xr_mem;\n    XR_REGS;\n    const uint32_t e_lr = c->ret_lr, e_sp = c->ret_sp;\n    uint32_t fl_n, fl_z, fl_c, fl_v");
    s.push_str(if dispatch { ", dt;\n" } else { ";\n" });
    if let Some(k) = fx.module {
        let _ = writeln!(s, "    const uint32_t mb = xr_mod_base[{k}];");
    }
    if live.entry != 0 {
        let _ = writeln!(s, "   {}", reload_flags(live.entry));
    }
    s.push_str(&body);
    s.push_str("}\n");
    s
}

struct Em<'a> {
    fx: &'a Func<'a>,
    live: &'a Liveness,
    tab: &'a FlagTable,
    /// Functions called directly, for prototypes.
    protos: BTreeSet<String>,
}

fn hex(v: u32) -> String { format!("0x{v:X}u") }

fn cond_expr(c: Cond) -> &'static str {
    use Cond::*;
    match c {
        Eq => "fl_z",
        Ne => "!fl_z",
        Cs => "fl_c",
        Cc => "!fl_c",
        Mi => "fl_n",
        Pl => "!fl_n",
        Vs => "fl_v",
        Vc => "!fl_v",
        Hi => "fl_c && !fl_z",
        Ls => "!fl_c || fl_z",
        Ge => "fl_n == fl_v",
        Lt => "fl_n != fl_v",
        Gt => "!fl_z && fl_n == fl_v",
        Le => "fl_z || fl_n != fl_v",
        Al => "1",
    }
}

/// Load the flags in `mask` from `c->cpsr` (entry, after calls).
fn reload_flags(mask: u8) -> String {
    let mut s = String::new();
    for (f, name, bit) in [(N, "n", 31), (Z, "z", 30), (C, "c", 29), (V, "v", 28)] {
        if mask & f != 0 {
            let _ = write!(s, " fl_{name} = (c->cpsr >> {bit}) & 1;");
        }
    }
    s
}

/// Store the flags in `mask` into `c->cpsr` for another function (callee entry, caller after a return).
fn store_flags(mask: u8) -> String {
    if mask == 0 {
        return String::new();
    }
    let mut s = format!("c->cpsr = (c->cpsr & ~0x{:08X}u)", (mask as u32) << 28);
    for (f, name, bit) in [(N, "n", 31), (Z, "z", 30), (C, "c", 29), (V, "v", 28)] {
        if mask & f != 0 {
            let _ = write!(s, " | fl_{name} << {bit}");
        }
    }
    s + "; "
}

/// Back-edge budget check: on exhaustion, put the guest state (registers and the flags in `live`) in `c`, let
/// the runtime preempt (it may switch guest threads), then reload. The fast path is a decrement and a compare.
fn preempt(live: u8) -> String {
    format!(
        "if (__builtin_expect(--c->budget <= 0, 0)) {{ {}XR_SPILL(); xr_rt->preempt(c); XR_RELOAD();{} }}",
        store_flags(live),
        reload_flags(live)
    )
}

/// A tail transfer keeps this frame: the next function (or interp) returns where this one would.
const PASS_FRAME: &str = "c->ret_lr = e_lr; c->ret_sp = e_sp; ";

/// A call opens a frame returning to r14 (already set) with the current sp.
const CALL_FRAME: &str = "c->ret_lr = r14; c->ret_sp = r13; ";

/// Tail-shaped miss: interpret `t` (odd = Thumb) until this function's own return, then return.
fn miss_tail(t: &str) -> String {
    format!("{}XR_SPILL(); {PASS_FRAME}xr_rt->interp(c, {t}, e_lr, e_sp); return;", store_flags(flags::ALL))
}

/// N and Z of a 32-bit `res`.
fn nz(need: u8) -> String {
    let mut s = String::new();
    if need & N != 0 {
        s.push_str(" fl_n = res >> 31;");
    }
    if need & Z != 0 {
        s.push_str(" fl_z = res == 0;");
    }
    s
}

/// `base + off` for a signed byte offset.
fn add_off(base: &str, off: i64) -> String {
    if off >= 0 { format!("{base} + {off}u") } else { format!("{base} - {}u", -off) }
}

/// Immediate shift of `v` (a register name or constant): (value, carry-out or None when C is unchanged).
/// Any amount is accepted; the decoder's normalisation (LSR/ASR #32, RRX) is handled like the manual says.
pub(crate) fn imm_shift(v: &str, shift: Shift, n: u8) -> (String, Option<String>) {
    let n = n as u32;
    match (shift, n) {
        (Shift::Rrx, _) => (format!("(fl_c << 31 | {v} >> 1)"), Some(format!("({v} & 1)"))),
        (_, 0) => (v.to_string(), None),
        (Shift::Lsl, 1..=31) => (format!("({v} << {n})"), Some(format!("(({v} >> {}) & 1)", 32 - n))),
        (Shift::Lsl, 32) => ("0u".into(), Some(format!("({v} & 1)"))),
        (Shift::Lsl, _) | (Shift::Lsr, 33..) => ("0u".into(), Some("0u".into())),
        (Shift::Lsr, 1..=31) => (format!("({v} >> {n})"), Some(format!("(({v} >> {}) & 1)", n - 1))),
        (Shift::Lsr, _) => ("0u".into(), Some(format!("({v} >> 31)"))),
        (Shift::Asr, 1..=31) => (format!("(uint32_t)((int32_t){v} >> {n})"), Some(format!("(({v} >> {}) & 1)", n - 1))),
        (Shift::Asr, _) => (format!("(uint32_t)((int32_t){v} >> 31)"), Some(format!("({v} >> 31)"))),
        (Shift::Ror, _) => match n % 32 {
            0 => (v.to_string(), Some(format!("({v} >> 31)"))),
            r => (format!("({v} >> {r} | {v} << {})", 32 - r), Some(format!("(({v} >> {}) & 1)", r - 1))),
        },
    }
}

/// Media ops whose `ra` field is a destination (RdLo of the long multiplies).
pub(crate) fn media_writes_ra(op: MediaOp) -> bool { matches!(op, MediaOp::Smlalxy | MediaOp::Smlald | MediaOp::Smlsld) }

impl Em<'_> {
    /// Register as an rvalue: PC reads as `pc_read(addr)`.
    fn r(&self, reg: Reg, addr: u32) -> String {
        if reg == Reg::PC { self.fx.abs(self.fx.pc_read(addr)) } else { format!("r{}", reg.0 & 15) }
    }

    fn insn(&mut self, i: usize) -> String {
        let (addr, _, insn) = self.fx.insns[i];
        let kind = self.fx.kinds[i];
        let need = flags::use_def(&insn, kind).1 & self.live.out[i];
        let body = match kind {
            Kind::Miss => miss_tail(&self.fx.abs(addr | self.fx.thumb() as u32)),
            Kind::Goto(t) if self.fx.insns[t].0 <= addr => format!("{} goto L_{:08X};", preempt(self.live.out[i]), self.fx.insns[t].0),
            Kind::Goto(t) => format!("goto L_{:08X};", self.fx.insns[t].0),
            // A backward tail call can close a loop of tail calls between functions: it pays the budget too.
            Kind::Leave(t) if t <= addr => format!("{} {}", preempt(self.leave_flags(t)), self.leave(t)),
            Kind::Leave(t) => self.leave(t),
            Kind::Call(t, mode) => self.call(i, t, mode),
            _ => self.body(i, addr, &insn, need),
        };
        match cond_of(&insn) {
            _ if body.is_empty() => body,
            Cond::Al => body,
            c => format!("if ({}) {{ {body} }}", cond_expr(c)),
        }
    }

    /// Flags a call defines that are live after it: reloaded from c->cpsr.
    fn after_call(&self, i: usize) -> String { reload_flags(flags::ALL & self.live.out[i]) }

    /// Name of the listed function of `mode` at `t` (prototype recorded), or None.
    fn callee(&mut self, t: u32, mode: Mode) -> Option<String> {
        let n = fn_name(self.fx.target(t, mode)?.start, mode, self.fx.module);
        self.protos.insert(n.clone());
        Some(n)
    }

    /// `xr_rt->hooks[i]` for a hooked `t` as a declaration of `h` (a NULL table means no hooks).
    fn hook(&self, t: u32) -> Option<String> {
        self.fx.hook_index(t).map(|h| format!("XrFn h = xr_rt->hooks ? xr_rt->hooks[{h}] : 0;"))
    }

    /// BL / BLX imm at `i` to `t`: a direct call, or (unlisted) a call-shaped miss: interpret `t` until it
    /// returns here, then continue natively. A miss stores all four flags; the live ones are reloaded after.
    fn call(&mut self, i: usize, t: u32, mode: Mode) -> String {
        let ret = self.fx.abs(self.fx.ret_addr(i));
        let (put, direct) = match self.callee(t, mode) {
            Some(n) => (self.entry_of(t, mode), format!("{n}(c);")),
            None => (flags::ALL, format!("xr_rt->interp(c, {}, {ret}, r13);", self.fx.abs(if mode == Mode::Thumb { t | 1 } else { t }))),
        };
        let call = match self.hook(t) {
            Some(h) => format!("{{ {h} if (h) h(c); else {direct} }}"),
            None => direct,
        };
        format!("r14 = {ret}; {}XR_SPILL(); {CALL_FRAME}{call} XR_RELOAD();{}", store_flags(put), self.after_call(i))
    }

    /// Leave the function for `t`: tail call, or (unlisted) a tail-shaped miss.
    fn leave(&mut self, t: u32) -> String {
        let mode = self.fx.f.mode;
        let hook = self.hook(t).map(|h| format!("{{ {h} if (h) XR_TAIL return h(c); }} ")).unwrap_or_default();
        let put = store_flags(self.leave_flags(t));
        match self.callee(t, mode) {
            Some(n) => format!("{put}XR_SPILL(); {PASS_FRAME}{hook}XR_TAIL return {n}(c);"),
            None => {
                let t = self.fx.abs(if mode == Mode::Thumb { t | 1 } else { t });
                format!("{put}XR_SPILL(); {PASS_FRAME}{hook}xr_rt->interp(c, {t}, e_lr, e_sp); return;")
            }
        }
    }

    /// Flags a tail transfer to `t` stores: the target's entry flags, or all four for a miss.
    fn leave_flags(&self, t: u32) -> u8 {
        let mode = self.fx.f.mode;
        if self.fx.target(t, mode).is_some() { self.entry_of(t, mode) } else { flags::ALL }
    }

    /// Flags this function's callers read after it returns.
    fn ret_flags(&self) -> u8 { self.tab.ret(self.fx.f.start, self.fx.f.mode) }

    /// Entry flags of the direct target `t` (0 when it is not a listed start of that mode).
    fn entry_of(&self, t: u32, mode: Mode) -> u8 { self.fx.target(t, mode).map_or(0, |e| self.tab.entry(e.start, e.mode)) }

    /// Indirect call; an unlisted target is a call-shaped miss (interpret until it returns to r14).
    fn call_ind(&self, i: usize, t: &str, pre: &str) -> String {
        format!(
            "{{ uint32_t t = {t}; {pre}{}XR_SPILL(); {CALL_FRAME}XrFn fn = xr_lookup(t); if (fn) fn(c); else xr_rt->interp(c, t, r14, r13); XR_RELOAD(); }}{}",
            store_flags(flags::ALL),
            self.after_call(i)
        )
    }

    /// A write of `v` to PC by instruction `i`: return, indirect call or computed jump. Returns are checked: a
    /// target other than the entry lr (longjmp, EHABI unwinding, a rewritten saved lr) is interpreted until this
    /// function's own return.
    fn pc_write(&self, i: usize, v: &str) -> String {
        match self.fx.kinds[i] {
            Kind::Return => format!(
                "{{ uint32_t t = {v}; {}XR_SPILL(); if (t != e_lr) {{ {PASS_FRAME}xr_rt->interp(c, t, e_lr, e_sp); }} return; }}",
                store_flags(self.ret_flags())
            ),
            Kind::CallInd => self.call_ind(i, v, ""),
            _ => format!("dt = {v}; goto dispatch;"),
        }
    }

    fn body(&mut self, i: usize, addr: u32, insn: &Insn, need: u8) -> String {
        use Insn::*;
        let d = |r: Reg| format!("r{}", r.0 & 15);
        match *insn {
            DataProc { op, rd, rn, op2, .. } => self.dataproc(i, addr, op, rd, rn, op2, need),
            Mul { accumulate, rd, rn, rm, rs, .. } => {
                let acc = if accumulate { format!(" + {}", self.r(rn, addr)) } else { String::new() };
                format!("{{ uint32_t res = {} * {}{acc};{} {} = res; }}", self.r(rm, addr), self.r(rs, addr), nz(need), d(rd))
            }
            MulLong { signed, accumulate, rdhi, rdlo, rm, rs, .. } => {
                let (m, s) = (self.r(rm, addr), self.r(rs, addr));
                let prod = if signed { format!("(uint64_t)((int64_t)(int32_t){m} * (int32_t){s})") } else { format!("(uint64_t){m} * {s}") };
                let acc = if accumulate { format!(" + ((uint64_t){} << 32 | {})", d(rdhi), d(rdlo)) } else { String::new() };
                let mut f = String::new();
                if need & N != 0 {
                    f.push_str(" fl_n = (uint32_t)(p >> 63);");
                }
                if need & Z != 0 {
                    f.push_str(" fl_z = p == 0;");
                }
                format!("{{ uint64_t p = {prod}{acc}; {} = (uint32_t)p; {} = (uint32_t)(p >> 32);{f} }}", d(rdlo), d(rdhi))
            }
            Umaal { rdhi, rdlo, rm, rs, .. } => format!(
                "{{ uint64_t p = (uint64_t){} * {} + {lo} + {hi}; {lo} = (uint32_t)p; {hi} = (uint32_t)(p >> 32); }}",
                self.r(rm, addr),
                self.r(rs, addr),
                lo = d(rdlo),
                hi = d(rdhi)
            ),
            Load { width, rt, rn, addr: a, .. } => self.mem(i, addr, true, width, rt, rn, a),
            Store { width, rt, rn, addr: a, .. } => self.mem(i, addr, false, width, rt, rn, a),
            LoadMulti { rn, list, mode, writeback, .. } => self.block(i, addr, true, rn, list, mode, writeback),
            StoreMulti { rn, list, mode, writeback, .. } => self.block(i, addr, false, rn, list, mode, writeback),
            BranchExchange { link: true, rm, .. } => self.call_ind(i, &self.r(rm, addr), &format!("r14 = {}; ", self.fx.abs(self.fx.ret_addr(i)))),
            BranchExchange { rm, .. } => self.pc_write(i, &self.r(rm, addr)),
            Svc { imm, .. } => format!("c->r[15] = {}; XR_SPILL(); xr_rt->svc(c, 0x{imm:X}u); XR_RELOAD();", self.fx.abs(addr)),
            Ldrex { width, rt, rn, .. } => {
                let ld = match width {
                    MemWidth::Double => format!("{} = xr_ld32(m, a); {} = xr_ld32(m, a + 4);", d(rt), d(Reg(rt.0 + 1))),
                    w => format!("{} = {}(m, a);", d(rt), load_fn(w)),
                };
                format!("{{ uint32_t a = {}; {ld} c->excl_addr = a; c->excl_valid = 1; }}", self.r(rn, addr))
            }
            Strex { width, rd, rt, rn, .. } => {
                let st = match width {
                    MemWidth::Double => format!("xr_st32(m, a, {}); xr_st32(m, a + 4, {});", self.r(rt, addr), self.r(Reg(rt.0 + 1), addr)),
                    w => format!("{}(m, a, {});", store_fn(w), self.r(rt, addr)),
                };
                format!(
                    "{{ uint32_t a = {}; if (c->excl_valid && c->excl_addr == a) {{ {st} {rd} = 0; }} else {rd} = 1; c->excl_valid = 0; }}",
                    self.r(rn, addr),
                    rd = d(rd)
                )
            }
            Clrex => "c->excl_valid = 0;".into(),
            Swp { byte, rd, rm, rn, .. } => {
                let w = if byte { MemWidth::Byte } else { MemWidth::Word };
                format!(
                    "{{ uint32_t a = {}, v = {}, t = {}(m, a); {}(m, a, v); {} = t; }}",
                    self.r(rn, addr),
                    self.r(rm, addr),
                    load_fn(w),
                    store_fn(w),
                    d(rd)
                )
            }
            Mrs { rd, .. } => format!("{} = (c->cpsr & 0x0FFFFFFFu) | fl_n << 31 | fl_z << 30 | fl_c << 29 | fl_v << 28;", d(rd)),
            Msr { mask, src, .. } => {
                let v = match src {
                    MsrSrc::Imm(x) => hex(x),
                    MsrSrc::Reg(r) => self.r(r, addr),
                };
                let mut s = format!("{{ uint32_t v = {v};");
                if mask & 8 != 0 {
                    for (f, name, bit) in [(N, "n", 31), (Z, "z", 30), (C, "c", 29), (V, "v", 28)] {
                        if need & f != 0 {
                            let _ = write!(s, " fl_{name} = (v >> {bit}) & 1;");
                        }
                    }
                    s.push_str(" c->cpsr = (c->cpsr & ~XR_Q) | (v & XR_Q);");
                }
                if mask & 4 != 0 {
                    s.push_str(" c->cpsr = (c->cpsr & ~0x000F0000u) | (v & 0x000F0000u);");
                }
                s.push_str(" }");
                s
            }
            Media { op, rd, rn, rm, ra, imm, amount, x, y, .. } => self.media(addr, op, rd, rn, rm, ra, imm, amount, x, y),
            Vfp(v) => vfp::emit(&v, &self.fx.abs(pc_load_base(addr)), need),
            // Barrier MCRs, hints, PLD, CPS (user mode), SETEND LE: no effect.
            Mrc { rt, .. } => format!("{} = c->tls;", d(rt)),
            Mcr { .. } | Hint { .. } | Pld { .. } | Cps { .. } | Setend { big: false } => String::new(),
            _ => miss_tail(&self.fx.abs(addr | self.fx.thumb() as u32)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dataproc(&mut self, i: usize, addr: u32, op: DpOp, rd: Reg, rn: Reg, op2: Operand2, need: u8) -> String {
        use DpOp::*;
        let (b, carry) = self.operand2(op2, addr);
        let mut s = if matches!(op, Mov | Mvn) {
            format!("{{ uint32_t b = {b}")
        } else {
            format!("{{ uint32_t a = {}, b = {b}", self.r(rn, addr))
        };
        let arith = match op {
            Add | Cmn => Some(("a", "b", "0")),
            Adc => Some(("a", "b", "fl_c")),
            Sub | Cmp => Some(("a", "~b", "1")),
            Sbc => Some(("a", "~b", "fl_c")),
            Rsb => Some(("b", "~a", "1")),
            Rsc => Some(("b", "~a", "fl_c")),
            _ => None,
        };
        match arith {
            Some((x, y, k)) => {
                let _ = write!(s, ", x = {x}, y = {y}, k = {k}, res = x + y + k;{}", nz(need));
                if need & C != 0 {
                    s.push_str(" fl_c = (uint32_t)(((uint64_t)x + y + k) >> 32);");
                }
                if need & V != 0 {
                    s.push_str(" fl_v = (~(x ^ y) & (x ^ res)) >> 31;");
                }
            }
            None => {
                let res = match op {
                    And | Tst => "a & b",
                    Eor | Teq => "a ^ b",
                    Orr => "a | b",
                    Bic => "a & ~b",
                    Mvn => "~b",
                    _ => "b",
                };
                let _ = write!(s, ", res = {res};{}", nz(need));
                if let (true, Some(cexpr)) = (need & C != 0, carry) {
                    let _ = write!(s, " fl_c = {cexpr};");
                }
            }
        }
        if writes_rd(op) {
            if rd == Reg::PC {
                // BranchWritePC: ARM clears bits 1:0; Thumb (only `MOV PC, LR` gets here) stays in Thumb state.
                let _ = write!(s, " {}", self.pc_write(i, if self.fx.thumb() { "res | 1u" } else { "res & ~3u" }));
            } else {
                let _ = write!(s, " r{} = res;", rd.0 & 15);
            }
        }
        s.push_str(" }");
        s
    }

    /// Operand 2: (value, shifter carry-out or None when C is unchanged).
    fn operand2(&self, op2: Operand2, addr: u32) -> (String, Option<String>) {
        match op2 {
            Operand2::Imm { value, rot } => (hex(value), (rot != 0).then(|| (value >> 31).to_string())),
            Operand2::RegImmShift { rm, shift, amount } => imm_shift(&self.r(rm, addr), shift, amount),
            Operand2::RegRegShift { rm, shift: Shift::Rrx, .. } => imm_shift(&self.r(rm, addr), Shift::Rrx, 1),
            Operand2::RegRegShift { rm, shift, rs } => {
                let k = match shift {
                    Shift::Lsl => 0,
                    Shift::Lsr => 1,
                    Shift::Asr => 2,
                    _ => 3,
                };
                let (v, n) = (self.r(rm, addr), format!("({} & 0xFF)", self.r(rs, addr)));
                (format!("xr_shift({k}, {v}, {n})"), Some(format!("xr_shift_c({k}, {v}, {n}, fl_c)")))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mem(&self, i: usize, addr: u32, load: bool, width: MemWidth, rt: Reg, rn: Reg, a: Addr) -> String {
        let pc_base = pc_load_base(addr);
        // Literal pool word in read-only text: a constant.
        if let (true, Reg::PC, true, MemWidth::Word, Offset::Imm(off)) = (load, rn, a.pre, width, a.offset) {
            let ea = if a.up { pc_base.wrapping_add(off) } else { pc_base.wrapping_sub(off) };
            if let Some(v) = self.fx.literal(ea) {
                return if rt == Reg::PC { self.pc_write(i, &hex(v)) } else { format!("r{} = {}; /* [{ea:08X}] */", rt.0 & 15, hex(v)) };
            }
        }
        let base = if rn == Reg::PC { self.fx.abs(pc_base) } else { self.r(rn, addr) };
        let off = match a.offset {
            Offset::Imm(v) => hex(v),
            Offset::Reg { rm, shift, amount } => imm_shift(&self.r(rm, addr), shift, amount).0,
        };
        let mut s = format!(
            "{{ uint32_t b = {base}, o = {off}, ea = b {} o, a = {};",
            if a.up { "+" } else { "-" },
            if a.pre { "ea" } else { "b" }
        );
        let wb = if a.writeback && rn != Reg::PC { format!(" r{} = ea;", rn.0 & 15) } else { String::new() };
        let rt2 = Reg(rt.0.wrapping_add(1));
        if load {
            if width == MemWidth::Double {
                let _ = write!(s, " uint32_t v0 = xr_ld32(m, a), v1 = xr_ld32(m, a + 4);{wb} r{} = v0; r{} = v1;", rt.0 & 15, rt2.0 & 15);
            } else {
                let _ = write!(s, " uint32_t v = {}(m, a);{wb}", load_fn(width));
                if rt == Reg::PC {
                    let _ = write!(s, " {}", self.pc_write(i, "v"));
                } else {
                    let _ = write!(s, " r{} = v;", rt.0 & 15);
                }
            }
        } else if width == MemWidth::Double {
            let _ = write!(s, " xr_st32(m, a, {}); xr_st32(m, a + 4, {});{wb}", self.r(rt, addr), self.r(rt2, addr));
        } else {
            let _ = write!(s, " {}(m, a, {});{wb}", store_fn(width), self.r(rt, addr));
        }
        s.push_str(" }");
        s
    }

    #[allow(clippy::too_many_arguments)]
    fn block(&self, i: usize, addr: u32, load: bool, rn: Reg, list: u16, mode: BlockMode, writeback: bool) -> String {
        let n = list.count_ones() as i64;
        let (start, wbv) = match mode {
            BlockMode::Ia => (0, 4 * n),
            BlockMode::Ib => (4, 4 * n),
            BlockMode::Da => (4 - 4 * n, -4 * n),
            BlockMode::Db => (-4 * n, -4 * n),
        };
        let mut s = format!("{{ uint32_t b = {}, a = {};", self.r(rn, addr), add_off("b", start));
        let wb = if writeback && rn != Reg::PC { format!(" r{} = {};", rn.0 & 15, add_off("b", wbv)) } else { String::new() };
        let regs = (0..16u8).filter(|r| list & (1 << r) != 0);
        if load {
            // Writeback first so a loaded base register wins.
            s.push_str(&wb);
            for (k, r) in regs.enumerate() {
                let v = format!("xr_ld32(m, a + {}u)", 4 * k);
                if r == 15 {
                    let _ = write!(s, " {}", self.pc_write(i, &v));
                } else {
                    let _ = write!(s, " r{r} = {v};");
                }
            }
        } else {
            for (k, r) in regs.enumerate() {
                let _ = write!(s, " xr_st32(m, a + {}u, {});", 4 * k, self.r(Reg(r), addr));
            }
            s.push_str(&wb);
        }
        s.push_str(" }");
        s
    }

    #[allow(clippy::too_many_arguments)]
    fn media(&self, addr: u32, op: MediaOp, rd: Reg, rn: Reg, rm: Reg, ra: Reg, imm: u8, amount: u8, x: bool, y: bool) -> String {
        use MediaOp::*;
        let (n, m, a) = (self.r(rn, addr), self.r(rm, addr), self.r(ra, addr));
        let d = format!("r{}", rd.0 & 15);
        let lo = format!("r{}", ra.0 & 15); // RdLo of the long forms
        let h = |v: &str, top: bool| if top { format!("(int32_t)(int16_t)({v} >> 16)") } else { format!("(int32_t)(int16_t){v}") };
        let q = "if (t != (int32_t)t) c->cpsr |= XR_Q;";
        let ror = |v: &str, k: u32| if k == 0 { v.to_string() } else { format!("({v} >> {k} | {v} << {})", 32 - k) };
        let amount = amount as u32;
        match op {
            Smulxy => format!("{d} = (uint32_t)({} * {});", h(&n, x), h(&m, y)),
            Smlaxy => format!("{{ int64_t t = (int64_t)({} * {}) + (int32_t){a}; {q} {d} = (uint32_t)t; }}", h(&n, x), h(&m, y)),
            Smulwy => format!("{d} = (uint32_t)(((int64_t)(int32_t){n} * {}) >> 16);", h(&m, y)),
            Smlawy => format!("{{ int64_t t = (((int64_t)(int32_t){n} * {}) >> 16) + (int32_t){a}; {q} {d} = (uint32_t)t; }}", h(&m, y)),
            Smlalxy => format!(
                "{{ uint64_t t = ((uint64_t){d} << 32 | {lo}) + (uint64_t)(int64_t)({} * {}); {lo} = (uint32_t)t; {d} = (uint32_t)(t >> 32); }}",
                h(&n, x),
                h(&m, y)
            ),
            Smuad | Smusd | Smlad | Smlsd | Smlald | Smlsld => {
                let sign = if matches!(op, Smuad | Smlad | Smlald) { "+" } else { "-" };
                let sum = format!("(int64_t)({} * {}) {sign} (int64_t)({} * {})", h(&n, false), h("m2", false), h(&n, true), h("m2", true));
                let m2 = format!("uint32_t m2 = {};", ror(&m, if x { 16 } else { 0 }));
                match op {
                    Smlald | Smlsld => format!(
                        "{{ {m2} uint64_t t = ((uint64_t){d} << 32 | {lo}) + (uint64_t)({sum}); {lo} = (uint32_t)t; {d} = (uint32_t)(t >> 32); }}"
                    ),
                    Smlad | Smlsd => format!("{{ {m2} int64_t t = {sum} + (int32_t){a}; {q} {d} = (uint32_t)t; }}"),
                    _ => format!("{{ {m2} int64_t t = {sum}; {q} {d} = (uint32_t)t; }}"),
                }
            }
            Smmul | Smmla | Smmls => {
                let p = format!("(uint64_t)((int64_t)(int32_t){n} * (int32_t){m})");
                let round = if x { " + 0x80000000u" } else { "" };
                let t = match op {
                    Smmul => format!("{p}{round}"),
                    Smmla => format!("((uint64_t){a} << 32) + {p}{round}"),
                    _ => format!("((uint64_t){a} << 32) - {p}{round}"),
                };
                format!("{d} = (uint32_t)(({t}) >> 32);")
            }
            Usad8 => format!("{d} = xr_usad8({n}, {m});"),
            Usada8 => format!("{d} = xr_usad8({n}, {m}) + {a};"),
            Sxtb | Sxth | Uxtb | Uxth => {
                let v = ror(&m, imm as u32 & 31);
                let ext = match op {
                    Sxtb => format!("(uint32_t)(int32_t)(int8_t){v}"),
                    Sxth => format!("(uint32_t)(int32_t)(int16_t){v}"),
                    Uxtb => format!("({v} & 0xFFu)"),
                    _ => format!("({v} & 0xFFFFu)"),
                };
                if rn == Reg::PC { format!("{d} = {ext};") } else { format!("{d} = {n} + {ext};") }
            }
            Sxtb16 | Uxtb16 => {
                let e = |v: &str| if op == Sxtb16 { format!("(uint32_t)(int32_t)(int8_t)({v})") } else { format!("(({v}) & 0xFFu)") };
                let acc = if rn == Reg::PC { String::new() } else { format!(" lo += {n}; hi += {n} >> 16;") };
                format!(
                    "{{ uint32_t v = {}, lo = {}, hi = {};{acc} {d} = (lo & 0xFFFFu) | hi << 16; }}",
                    ror(&m, imm as u32 & 31),
                    e("v"),
                    e("v >> 16")
                )
            }
            Rev => format!("{d} = __builtin_bswap32({m});"),
            Rev16 => format!("{d} = ({m} >> 8 & 0x00FF00FFu) | ({m} << 8 & 0xFF00FF00u);"),
            Revsh => format!("{d} = (uint32_t)(int32_t)(int16_t)(({m} >> 8 & 0xFFu) | ({m} & 0xFFu) << 8);"),
            Clz => format!("{d} = {m} ? (uint32_t)__builtin_clz({m}) : 32u;"),
            Qadd => format!("{d} = xr_ssat((int64_t)(int32_t){m} + (int32_t){n}, 32, &c->cpsr);"),
            Qsub => format!("{d} = xr_ssat((int64_t)(int32_t){m} - (int32_t){n}, 32, &c->cpsr);"),
            Qdadd | Qdsub => format!(
                "{d} = xr_ssat((int64_t)(int32_t){m} {} (int32_t)xr_ssat(2 * (int64_t)(int32_t){n}, 32, &c->cpsr), 32, &c->cpsr);",
                if op == Qdadd { "+" } else { "-" }
            ),
            Sel => format!("{d} = xr_sel({n}, {m}, c->cpsr);"),
            Pkhbt if amount < 32 => format!("{d} = ({n} & 0xFFFFu) | ({m} << {amount} & 0xFFFF0000u);"),
            Pkhbt => format!("{d} = {n} & 0xFFFFu;"),
            Pkhtb => format!("{d} = ({n} & 0xFFFF0000u) | ((uint32_t)((int32_t){m} >> {}) & 0xFFFFu);", amount.min(31)),
            Ssat | Usat => {
                let v = if x {
                    format!("(int32_t){n} >> {}", amount.min(31))
                } else if amount < 32 {
                    format!("(int32_t)({n} << {amount})")
                } else {
                    "0".into()
                };
                if op == Ssat {
                    format!("{d} = xr_ssat({v}, {}, &c->cpsr);", imm.clamp(1, 32))
                } else {
                    format!("{d} = xr_usat({v}, {}, &c->cpsr);", imm.min(31))
                }
            }
            Ssat16 => format!("{d} = xr_ssat16({n}, {}, &c->cpsr);", imm.clamp(1, 16)),
            Usat16 => format!("{d} = xr_usat16({n}, {}, &c->cpsr);", imm.min(15)),
            Par(kind, pop) => {
                let k = match kind {
                    ParKind::S => 0,
                    ParKind::Q => 1,
                    ParKind::Sh => 2,
                    ParKind::U => 3,
                    ParKind::Uq => 4,
                    ParKind::Uh => 5,
                };
                let o = match pop {
                    ParOp::Add16 => 0,
                    ParOp::Asx => 1,
                    ParOp::Sax => 2,
                    ParOp::Sub16 => 3,
                    ParOp::Add8 => 4,
                    ParOp::Sub8 => 5,
                };
                format!("{d} = xr_par({k}, {o}, {n}, {m}, &c->cpsr);")
            }
        }
    }
}

/// Base for loads/stores with `rn = PC`: always insn + 8 (the Thumb decoder pre-folds its offsets to match).
pub(crate) fn pc_load_base(addr: u32) -> u32 { addr.wrapping_add(8) }

fn load_fn(w: MemWidth) -> &'static str {
    match w {
        MemWidth::Word | MemWidth::Double => "xr_ld32",
        MemWidth::Byte => "xr_ld8",
        MemWidth::Half => "xr_ld16",
        MemWidth::SignedByte => "xr_lds8",
        MemWidth::SignedHalf => "xr_lds16",
    }
}

fn store_fn(w: MemWidth) -> &'static str {
    match w {
        MemWidth::Word | MemWidth::Double => "xr_st32",
        MemWidth::Byte | MemWidth::SignedByte => "xr_st8",
        MemWidth::Half | MemWidth::SignedHalf => "xr_st16",
    }
}
