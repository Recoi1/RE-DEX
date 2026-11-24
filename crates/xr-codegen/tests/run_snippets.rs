//! Compile-and-run tests: ARM snippets (raw words) -> gen_function -> C -> clang -> run -> registers.
//! Encodings come from `clang --target=armv6k-none-eabi`; the comment on each word is its disassembly.
use std::path::PathBuf;
use std::process::Command;
use xr_arm::{decode_arm, decode_thumb, Insn};
use xr_codegen::{flag_table, gen_function, GenOptions};
use xr_formats::{FuncEntry, Mode};

const BASE: u32 = 0x1000;
const CFLAGS: &[&str] = &["-O2", "-fno-strict-aliasing", "-fwrapv", "-Wall", "-Werror", "-Wno-unused-variable", "-Wno-unused-label", "-Wno-unused-but-set-variable"];

#[derive(Default)]
struct Case<'a> {
    name: &'a str,
    words: &'a [u32],
    /// (start, end) of each function; the first is the entry unless `entry` is set.
    funcs: &'a [(u32, u32)],
    /// Starts of the functions in `funcs` that are Thumb (the rest are ARM).
    thumb: &'a [u32],
    entry: Option<u32>,
    regs: &'a [(usize, u32)],
    mem: &'a [(u32, u32)],
    /// (guest address, C function name defined in `extra_c`) per hook slot.
    hooks: &'a [(u32, &'a str)],
    extra_c: &'a str,
    /// C names (defined in `extra_c`) replacing the default `interp` (logs, prints `miss`, dumps, exits) and
    /// `preempt` (logs, refills the budget); "" = default.
    interp: &'a str,
    preempt: &'a str,
    /// Initial and refill budget; 0 = 1 << 30.
    budget: i32,
    /// Generate with `hooks` but run with `xr_rt->hooks == NULL`.
    null_hooks: bool,
    /// `GenOptions::patched`: words the loader rewrites (here `mem` does).
    patched: &'a [u32],
}

#[derive(Debug)]
struct Out {
    r: [u32; 16],
    cpsr: u32,
    fpscr: u32,
    svc: Vec<(u32, u32)>,
    miss: Option<u32>,
    /// (start, until_pc, until_sp, cpsr) per `interp` call.
    interp: Vec<(u32, u32, u32, u32)>,
    /// (r0, cpsr) per `preempt` call.
    preempt: Vec<(u32, u32)>,
}

fn abi_dir() -> PathBuf { PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../abi")) }

fn entries(c: &Case) -> Vec<FuncEntry> {
    let mode = |s| if c.thumb.contains(&s) { Mode::Thumb } else { Mode::Arm };
    c.funcs.iter().map(|&(start, end)| FuncEntry { start, end, mode: mode(start) }).collect()
}

/// C name of a listed function.
fn cname(f: &FuncEntry) -> String { format!("{}_{:08X}", if f.mode == Mode::Thumb { "t" } else { "f" }, f.start) }

/// Thumb halfwords packed into little-endian words (an odd count is padded with a zero halfword).
fn th(h: &[u16]) -> Vec<u32> { h.chunks(2).map(|p| p[0] as u32 | (*p.get(1).unwrap_or(&0) as u32) << 16).collect() }

fn bytes(words: &[u32]) -> Vec<u8> { words.iter().flat_map(|w| w.to_le_bytes()).collect() }

/// Generated C for every function of the case, concatenated.
fn gen_all(c: &Case) -> (String, Vec<u32>) {
    let all = entries(c);
    let hooks: Vec<u32> = c.hooks.iter().map(|h| h.0).collect();
    let code = bytes(c.words);
    let opt = GenOptions { flags: flag_table(&code, BASE, &all, &hooks), hooks, patched: c.patched.to_vec(), module: None };
    let (mut src, mut undec) = (String::new(), Vec::new());
    for f in &all {
        let g = gen_function(&code, BASE, f, &all, &opt);
        src.push_str(&g.c_source);
        src.push('\n');
        undec.extend(g.undecodable);
    }
    (src, undec)
}

/// Unique scratch dir per test, outside the repo.
fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xr_codegen_{}_{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Compile `src` with clang; `None` (skip) when clang is not installed.
fn clang(name: &str, src: &str, out: &PathBuf, extra: &[&str]) -> Option<()> {
    let c = scratch(name).join("t.c");
    std::fs::write(&c, src).unwrap();
    let res = Command::new("clang").args(CFLAGS).args(extra).arg("-I").arg(abi_dir()).arg(&c).arg("-o").arg(out).output();
    match res {
        Err(e) => {
            println!("skipping {name}: clang not found ({e})");
            None
        }
        Ok(o) => {
            assert!(o.status.success(), "clang failed for {name}:\n{}\n--- source ---\n{src}", String::from_utf8_lossy(&o.stderr));
            Some(())
        }
    }
}

fn run(c: &Case) -> Option<Out> {
    let (gen_src, _) = gen_all(c);
    let all = entries(c);
    let entry = c.entry.unwrap_or(c.funcs[0].0);
    let entry = all.iter().find(|f| f.start == entry).map(cname).unwrap();
    let mut src = String::from(
        r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "xr_abi.h"
uint8_t* xr_mem;
const XrRuntime* xr_rt;
static XrCpu cpu;
static void dump(void) {
    for (int i = 0; i < 16; i++) printf("r%d %08x\n", i, cpu.r[i]);
    printf("cpsr %08x\n", cpu.cpsr);
    printf("fpscr %08x\n", cpu.fpscr);
    fflush(stdout);
}
static void svc_cb(XrCpu* c, uint32_t imm) { printf("svc %08x %08x\n", imm, c->r[15]); c->r[0] = 0x5E5E0000u | imm; }
static void note_interp(XrCpu* c, uint32_t s, uint32_t u, uint32_t sp) { printf("interp %08x %08x %08x %08x\n", s, u, sp, c->cpsr); }
void interp_cb(XrCpu* c, uint32_t s, uint32_t u, uint32_t sp) { note_interp(c, s, u, sp); printf("miss %08x\n", s); dump(); exit(0); }
"#,
    );
    let budget = if c.budget == 0 { 1 << 30 } else { c.budget };
    src.push_str(&format!("void preempt_cb(XrCpu* c) {{ printf(\"preempt %08x %08x\\n\", c->r[0], c->cpsr); c->budget = {budget}; }}\n"));
    src.push_str(&gen_src);
    src.push_str(c.extra_c);
    src.push_str("\nXrFn xr_lookup(uint32_t a) {\n");
    for f in &all {
        let a = if f.mode == Mode::Thumb { f.start | 1 } else { f.start };
        src.push_str(&format!("    if (a == 0x{a:X}u) return {};\n", cname(f)));
    }
    src.push_str("    return 0;\n}\nint main(void) {\n    uint8_t* mem = calloc(1, 0x10000);\n    xr_mem = mem;\n");
    src.push_str(&format!("    static const uint8_t code[] = {{{}}};\n", bytes(c.words).iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",")));
    src.push_str(&format!("    memcpy(mem + 0x{BASE:X}, code, sizeof code);\n"));
    for &(a, v) in c.mem {
        src.push_str(&format!("    {{ uint32_t v = 0x{v:X}u; memcpy(mem + 0x{a:X}, &v, 4); }}\n"));
    }
    let hooks: Vec<&str> = c.hooks.iter().map(|h| h.1).chain(["0"]).collect();
    if c.null_hooks {
        src.push_str("    static XrFn* hooks = 0;\n");
    } else {
        src.push_str(&format!("    static XrFn hooks[] = {{{}}};\n", hooks.join(", ")));
    }
    src.push_str(&format!(
        "    static XrRuntime rt; rt.abi_version = XR_ABI_VERSION; rt.mem = mem; rt.svc = svc_cb; rt.interp = {}; rt.preempt = {}; rt.hooks = hooks;\n",
        if c.interp.is_empty() { "interp_cb" } else { c.interp },
        if c.preempt.is_empty() { "preempt_cb" } else { c.preempt }
    ));
    src.push_str(&format!(
        "    xr_rt = &rt;\n    cpu.r[13] = 0x8000; cpu.r[14] = 0xDEAD0000u; cpu.cpsr = 0x10; cpu.fpscr = 0x03000000u; cpu.tls = 0x1FF82000u; cpu.budget = {budget};\n"
    ));
    for &(r, v) in c.regs {
        src.push_str(&format!("    cpu.r[{r}] = 0x{v:X}u;\n"));
    }
    src.push_str("    cpu.ret_lr = cpu.r[14]; cpu.ret_sp = cpu.r[13];\n");
    src.push_str(&format!("    {entry}(&cpu);\n    dump();\n    return 0;\n}}\n"));

    let exe = scratch(c.name).join("t.exe");
    clang(c.name, &src, &exe, &[])?;
    let o = Command::new(&exe).output().unwrap();
    assert!(o.status.success(), "{} crashed: {:?}\n{}", c.name, o.status, String::from_utf8_lossy(&o.stdout));
    let text = String::from_utf8(o.stdout).unwrap();
    let mut out = Out { r: [0; 16], cpsr: 0, fpscr: 0, svc: Vec::new(), miss: None, interp: Vec::new(), preempt: Vec::new() };
    let hex = |s: &str| u32::from_str_radix(s, 16).unwrap();
    for line in text.lines() {
        let p: Vec<&str> = line.split_whitespace().collect();
        match p.as_slice() {
            ["cpsr", v] => out.cpsr = hex(v),
            ["fpscr", v] => out.fpscr = hex(v),
            ["svc", i, pc] => out.svc.push((hex(i), hex(pc))),
            ["miss", a] => out.miss = Some(hex(a)),
            ["interp", s, u, sp, f] => out.interp.push((hex(s), hex(u), hex(sp), hex(f))),
            ["preempt", r0, f] => out.preempt.push((hex(r0), hex(f))),
            [r, v] if r.starts_with('r') => out.r[r[1..].parse::<usize>().unwrap()] = hex(v),
            _ => panic!("unexpected output line {line:?}"),
        }
    }
    Some(out)
}

/// One-function case starting at BASE covering all words.
fn one<'a>(name: &'a str, words: &'a [u32], funcs: &'a [(u32, u32)], regs: &'a [(usize, u32)]) -> Case<'a> {
    Case { name, words, funcs, regs, ..Default::default() }
}

fn span(words: &[u32]) -> [(u32, u32); 1] { [(BASE, BASE + 4 * words.len() as u32)] }

// ---- basic tests ----

#[test]
fn sum_loop() {
    let w = [
        0xE3A00000, // mov r0, #0
        0xE3A01001, // mov r1, #1
        0xE0800001, // loop: add r0, r0, r1
        0xE2811001, // add r1, r1, #1
        0xE351000B, // cmp r1, #11
        0x1AFFFFFB, // bne loop
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("sum_loop", &w, &span(&w), &[])) else { return };
    assert_eq!(o.r[0], 55);
    assert_eq!(o.r[1], 11);
    assert_eq!(o.miss, None);
}

#[test]
fn cond_exec() {
    let w = [
        0xE3500005, // cmp r0, #5
        0xC3A01001, // movgt r1, #1
        0xD3A01002, // movle r1, #2
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("cond_exec_gt", &w, &span(&w), &[(0, 7)])) else { return };
    assert_eq!(o.r[1], 1);
    let Some(o) = run(&one("cond_exec_le", &w, &span(&w), &[(0, 3)])) else { return };
    assert_eq!(o.r[1], 2);
    let Some(o) = run(&one("cond_exec_eq", &w, &span(&w), &[(0, 5)])) else { return };
    assert_eq!(o.r[1], 2);
}

#[test]
fn carry_chain() {
    let w = [
        0xE0900002, // adds r0, r0, r2
        0xE0A11003, // adc r1, r1, r3
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("carry_chain", &w, &span(&w), &[(0, 0xFFFF_FFFF), (1, 1), (2, 1), (3, 0)])) else { return };
    assert_eq!((o.r[0], o.r[1]), (0, 2));
}

#[test]
fn ldm_stm() {
    let w = [
        0xE92D4FF0, // push {r4-r11, lr}
        0xE3A04000, // mov r4, #0
        0xE3A05000, // mov r5, #0
        0xE3A06000, // mov r6, #0
        0xE3A07000, // mov r7, #0
        0xE3A08000, // mov r8, #0
        0xE3A09000, // mov r9, #0
        0xE3A0A000, // mov r10, #0
        0xE3A0B000, // mov r11, #0
        0xE8BD8FF0, // pop {r4-r11, pc}
        0xE3A00063, // mov r0, #99 (must not run)
    ];
    let regs: Vec<(usize, u32)> = (4..=11).map(|r| (r, 0x1111_1111 * r as u32)).chain([(0, 5)]).collect();
    let Some(o) = run(&one("ldm_stm", &w, &span(&w), &regs)) else { return };
    for r in 4..=11 {
        assert_eq!(o.r[r], 0x1111_1111 * r as u32, "r{r}");
    }
    assert_eq!(o.r[13], 0x8000, "sp restored");
    assert_eq!(o.r[0], 5, "returned at pop {{pc}}");
    assert_eq!(o.miss, None);
}

#[test]
fn ldrex_strex() {
    let w = [
        0xE1901F9F, // ldrex r1, [r0]
        0xE2811001, // add r1, r1, #1
        0xE1802F91, // strex r2, r1, [r0]
        0xE1901F9F, // ldrex r1, [r0]
        0xF57FF01F, // clrex
        0xE1803F91, // strex r3, r1, [r0]
        0xE5904000, // ldr r4, [r0]
        0xE12FFF1E, // bx lr
    ];
    let s = span(&w);
    let c = Case { mem: &[(0x2000, 5)], ..one("ldrex_strex", &w, &s, &[(0, 0x2000), (2, 9), (3, 9)]) };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[2], 0, "strex right after ldrex succeeds");
    assert_eq!(o.r[3], 1, "strex after clrex fails");
    assert_eq!(o.r[4], 6, "only the first strex stored");
}

#[test]
fn svc_reaches_runtime() {
    let w = [
        0xEF000032, // svc #0x32
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("svc", &w, &span(&w), &[(0, 1)])) else { return };
    assert_eq!(o.svc, vec![(0x32, BASE)], "imm and guest pc of the svc");
    assert_eq!(o.r[0], 0x5E5E_0032, "registers reloaded after the svc");
}

#[test]
fn indirect_miss() {
    let w = [
        0xE12FFF10, // bx r0
    ];
    let Some(o) = run(&one("indirect_miss", &w, &span(&w), &[(0, 0x4444)])) else { return };
    assert_eq!(o.miss, Some(0x4444));
}

#[test]
fn undecodable_function() {
    let w = [
        0xE3A00001, // mov r0, #1
        0xFFFFFFFF, // undefined
        0xE12FFF1E, // bx lr
    ];
    let s = span(&w);
    let c = one("undecodable", &w, &s, &[(0, 7)]);
    let (src, undec) = gen_all(&c);
    assert_eq!(undec, vec![BASE]);
    assert!(src.contains("xr_rt->interp(c, 0x00001000u, c->ret_lr, c->ret_sp)"), "{src}");
    assert!(!src.contains("r0"), "stub must only call miss:\n{src}");
    let Some(o) = run(&c) else { return };
    assert_eq!(o.miss, Some(BASE));
    assert_eq!(o.r[0], 7, "nothing ran before the miss");
}

#[test]
fn unused_flags_not_computed() {
    let w = [
        0xE2900001, // adds r0, r0, #1
        0xE1A0F00E, // mov pc, lr
    ];
    let s = span(&w);
    let c = one("unused_flags", &w, &s, &[(0, 41)]);
    let (src, _) = gen_all(&c);
    assert!(!src.contains("_n ="), "{src}");
    assert!(!src.contains("_z ="), "{src}");
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[0], 42);
}

// ---- extra edge tests ----

#[test]
fn shifter_carry() {
    let w = [
        0xE1520002, // cmp r2, r2          C=1
        0xE1B00001, // movs r0, r1         (lsl #0: C unchanged)
        0xE3A03000, // mov r3, #0
        0xE2A33000, // adc r3, r3, #0      r3 = C = 1
        0xE1B00081, // lsls r0, r1, #1     C = bit31 of r1 = 0
        0xE3A04000, // mov r4, #0
        0xE2A44000, // adc r4, r4, #0      r4 = 0
        0xE1B050A1, // lsrs r5, r1, #1     C = bit0 = 1
        0xE3A06000, // mov r6, #0
        0xE2A66000, // adc r6, r6, #0      r6 = 1
        0xE10F7000, // mrs r7, apsr
        0xE1B08061, // rrxs r8, r1
        0xE10F9000, // mrs r9, apsr
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("shifter_carry", &w, &span(&w), &[(1, 0x4000_0001), (2, 0)])) else { return };
    assert_eq!(o.r[0], 0x8000_0002);
    assert_eq!((o.r[3], o.r[4], o.r[6]), (1, 0, 1));
    assert_eq!(o.r[5], 0x2000_0000);
    assert_eq!(o.r[7], 0x2000_0010, "NZCV after lsrs = 0010");
    assert_eq!(o.r[8], 0xA000_0000, "rrx shifts the old C in");
    assert_eq!(o.r[9], 0xA000_0010, "rrxs: N=1, C=old bit 0");
}

#[test]
fn rsbs_sbcs_borrow_and_overflow() {
    let w = [
        0xE3A00005, // mov r0, #5
        0xE2701003, // rsbs r1, r0, #3     3-5: borrow, C=0
        0xE2D02001, // sbcs r2, r0, #1     5-1-1 = 3, C=1
        0xE10F4000, // mrs r4, apsr
        0xE2C0300A, // sbc r3, r0, #10     5-10-0
        0xE0965006, // adds r5, r6, r6     0x40000000*2: V=1, N=1
        0xE10F7000, // mrs r7, apsr
        0xE059800A, // subs r8, r9, r10    0-1
        0xE2DCB000, // sbcs r11, r12, #0   0-0-1
        0xE10FC000, // mrs r12, apsr
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("rsbs_sbcs", &w, &span(&w), &[(6, 0x4000_0000), (9, 0), (10, 1), (12, 0)])) else { return };
    assert_eq!(o.r[1], 0xFFFF_FFFE);
    assert_eq!(o.r[2], 3);
    assert_eq!(o.r[4], 0x2000_0010);
    assert_eq!(o.r[3], 0xFFFF_FFFB);
    assert_eq!(o.r[5], 0x8000_0000);
    assert_eq!(o.r[7], 0x9000_0010, "N and V");
    assert_eq!((o.r[8], o.r[11]), (0xFFFF_FFFF, 0xFFFF_FFFF), "64-bit 0 - 1");
    assert_eq!(o.r[12], 0x8000_0010, "borrow out: C=0");
}

#[test]
fn multiplies() {
    let w = [
        0xE0810392, // umull r0, r1, r2, r3
        0xE0C54392, // smull r4, r5, r2, r3
        0xE0A54392, // umlal r4, r5, r2, r3
        0xE0060392, // mul r6, r2, r3
        0xE0278392, // mla r7, r2, r3, r8
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("multiplies", &w, &span(&w), &[(2, 0xFFFF_FFFE), (3, 3), (8, 5)])) else { return };
    assert_eq!((o.r[0], o.r[1]), (0xFFFF_FFFA, 2), "umull");
    assert_eq!((o.r[4], o.r[5]), (0xFFFF_FFF4, 2), "smull -6 then umlal");
    assert_eq!(o.r[6], 0xFFFF_FFFA);
    assert_eq!(o.r[7], 0xFFFF_FFFF);
}

#[test]
fn ldm_base_in_list() {
    let w = [
        0xE8900003, // ldm r0, {r0, r1}
        0xE92D0003, // push {r0, r1}
        0xE8BD000C, // pop {r2, r3}
        0xE12FFF1E, // bx lr
    ];
    let s = span(&w);
    let c = Case { mem: &[(0x2000, 0x11), (0x2004, 0x22)], ..one("ldm_base", &w, &s, &[(0, 0x2000)]) };
    let Some(o) = run(&c) else { return };
    assert_eq!((o.r[0], o.r[1], o.r[2], o.r[3]), (0x11, 0x22, 0x11, 0x22));
    assert_eq!(o.r[13], 0x8000);
}

#[test]
fn jump_table() {
    let w = [
        0xE3500002, // cmp r0, #2
        0x908FF100, // addls pc, pc, r0, lsl #2
        0xEA000008, // b default
        0xEA000001, // b case0
        0xEA000002, // b case1
        0xEA000003, // b case2
        0xE3A0100A, // case0: mov r1, #10
        0xE12FFF1E, // bx lr
        0xE3A0100B, // case1: mov r1, #11
        0xE12FFF1E, // bx lr
        0xE3A0100C, // case2: mov r1, #12
        0xE12FFF1E, // bx lr
        0xE3A01063, // default: mov r1, #99
        0xE12FFF1E, // bx lr
    ];
    for (r0, want) in [(0, 10), (1, 11), (2, 12), (5, 99)] {
        let name = format!("jump_table_{r0}");
        let Some(o) = run(&one(&name, &w, &span(&w), &[(0, r0)])) else { return };
        assert_eq!(o.r[1], want, "r0 = {r0}");
        assert_eq!(o.miss, None);
    }
}

#[test]
fn media_ops() {
    let w = [
        0xE6BF2F30, // rev r2, r0
        0xE6A13070, // sxtab r3, r1, r0
        0xE6FF4470, // uxth r4, r0, ror #8
        0xE16F5F11, // clz r5, r1
        0xE16601C0, // smulbt r6, r0, r1
        0xE1087058, // qadd r7, r8, r8
        0xE10F9000, // mrs r9, apsr
        0xE650AF91, // uadd8 r10, r0, r1
        0xE680BFB1, // sel r11, r0, r1
        0xE6E8C011, // usat r12, #8, r1
        0xE12FFF1E, // bx lr
    ];
    let (r0, r1) = (0x1234_5680u32, 0x80FF_7F01u32);
    let Some(o) = run(&one("media", &w, &span(&w), &[(0, r0), (1, r1), (8, 0x7FFF_FFFF)])) else { return };
    assert_eq!(o.r[2], 0x8056_3412, "rev");
    assert_eq!(o.r[3], 0x80FF_7E81, "sxtab");
    assert_eq!(o.r[4], 0x3456, "uxth ror 8");
    assert_eq!(o.r[5], 0, "clz");
    assert_eq!(o.r[6], (0x5680i32 * (0x80FFu16 as i16 as i32)) as u32, "smulbt");
    assert_eq!(o.r[7], 0x7FFF_FFFF, "qadd saturates");
    assert_eq!(o.r[9], 0x0800_0010, "Q set");
    assert_eq!(o.r[10], 0x9233_D581, "uadd8");
    assert_eq!(o.r[11], 0x8034_7F01, "sel uses GE = 0100");
    assert_eq!(o.r[12], 0, "usat of a negative value");
    assert_eq!(o.cpsr, 0x0804_0010, "GE and Q live in c->cpsr");
}

#[test]
fn media_multiplies_and_simd() {
    let w = [
        // f1 at 0x1000
        0xE10BC180, // smlabb r11, r0, r1, r12
        0xE702C110, // smlad r2, r0, r1, r12
        0xE703F130, // smuadx r3, r0, r1
        0xE754F130, // smmulr r4, r0, r1
        0xE6BF5FB0, // rev16 r5, r0
        0xE6FF6FB0, // revsh r6, r0
        0xE6807411, // pkhbt r7, r0, r1, lsl #8
        0xE6808451, // pkhtb r8, r0, r1, asr #8
        0xE6CF9470, // uxtb16 r9, r0, ror #8
        0xE610AF11, // sadd16 r10, r0, r1
        0xE10FC000, // mrs r12, apsr
        0xE12FFF1E, // bx lr
        // f2 at 0x1030
        0xE1412050, // qdadd r2, r0, r1
        0xE6A73250, // ssat r3, #8, r0, asr #4
        0xE784F110, // usad8 r4, r0, r1
        0xE12501A0, // smulwb r5, r0, r1
        0xE1476180, // smlalbb r6, r7, r0, r1
        0xE0498190, // umaal r8, r9, r0, r1
        0xE610AFF1, // ssub8 r10, r0, r1
        0xE660BF91, // uqadd8 r11, r0, r1
        0xE630CF11, // shadd16 r12, r0, r1
        0xE12FFF1E, // bx lr
    ];
    let funcs = [(0x1000, 0x1030), (0x1030, 0x1058)];
    let (r0, r1, r12) = (0x8001_7FFFu32, 0x0002_0003u32, 0x7FFF_FFF0u32);
    let Some(o) = run(&one("media2_a", &w, &funcs, &[(0, r0), (1, r1), (12, r12)])) else { return };
    let (lo0, hi0, lo1, hi1) = (r0 as i16 as i64, (r0 >> 16) as i16 as i64, r1 as i16 as i64, (r1 >> 16) as i16 as i64);
    assert_eq!(o.r[11], (lo0 * lo1 + r12 as i32 as i64) as u32, "smlabb (overflows: sets Q)");
    assert_eq!(o.r[2], (lo0 * lo1 + hi0 * hi1 + r12 as i32 as i64) as u32, "smlad");
    assert_eq!(o.r[3], (lo0 * hi1 + hi0 * lo1) as u32, "smuadx");
    assert_eq!(o.r[4], (((r0 as i32 as i64 * r1 as i32 as i64) + 0x8000_0000) >> 32) as u32, "smmulr");
    assert_eq!(o.r[5], 0x0180_FF7F, "rev16");
    assert_eq!(o.r[6], 0xFFFF_FF7F, "revsh");
    assert_eq!(o.r[7], 0x0200_7FFF, "pkhbt");
    assert_eq!(o.r[8], 0x8001_0200, "pkhtb");
    assert_eq!(o.r[9], 0x0080_007F, "uxtb16 ror 8");
    assert_eq!(o.r[10], 0x8003_8002, "sadd16");
    assert_eq!(o.r[12], 0x0803_0010, "Q from the accumulates, GE = 0011 from sadd16");

    let (r0, r1) = (0x9C40_7F10u32, 0xF0E0_D0C0u32);
    let regs = [(0, r0), (1, r1), (6, 0xFFFF_FFFF), (7, 0), (8, 0xFFFF_FFFF), (9, 0xFFFF_FFFF)];
    let c = Case { entry: Some(0x1030), ..one("media2_b", &w, &funcs, &regs) };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[2], 0x8000_0000, "qdadd saturates low");
    assert_eq!(o.r[3], 0xFFFF_FF80, "ssat #8 of a large negative");
    assert_eq!(o.r[4], 0xB0 + 0x51 + 0xA0 + 0x54, "usad8");
    assert_eq!(o.r[5], ((r0 as i32 as i64 * (r1 as i16 as i64)) >> 16) as u32, "smulwb");
    let acc = 0x0000_0000_FFFF_FFFFu64.wrapping_add((r0 as i16 as i64 * r1 as i16 as i64) as u64);
    assert_eq!((o.r[6], o.r[7]), (acc as u32, (acc >> 32) as u32), "smlalbb");
    let um = r0 as u64 * r1 as u64 + 0xFFFF_FFFF + 0xFFFF_FFFF;
    assert_eq!((o.r[8], o.r[9]), (um as u32, (um >> 32) as u32), "umaal");
    assert_eq!(o.r[10], 0xAC60_AF50, "ssub8");
    assert_eq!(o.r[11], 0xFFFF_FFD0, "uqadd8");
    assert_eq!(o.r[12], 0xC690_27E8, "shadd16");
    assert_eq!(o.cpsr, 0x0807_0010, "Q, GE = 0111 from ssub8");
}

#[test]
fn literal_load() {
    let w = [
        0xE59F0000, // ldr r0, [pc]   (pc+8 = the word below)
        0xE12FFF1E, // bx lr
        0x12345678, // literal
    ];
    let Some(o) = run(&one("literal", &w, &span(&w), &[])) else { return };
    assert_eq!(o.r[0], 0x1234_5678);
}

/// An import stub (`ldr pc, [pc, #-4]` + literal) whose literal the loader rewrites at run time must load it:
/// ldr:ro points it at a CRO function. Folded, it kept the unresolved handler and X stopped on svcBreak.
#[test]
fn patched_literal_is_loaded() {
    let w = [
        0xE51FF004, // 1000 ldr pc, [pc, #-4]  (the literal at 1004; its range covers it, as an added stub's does)
        0x00001008, // 1004 literal: 1008 in the file, the loader rewrites it to 1010
        0xE3A00001, // 1008 mov r0, #1
        0xE12FFF1E, // 100c bx lr
        0xE3A00002, // 1010 mov r0, #2
        0xE12FFF1E, // 1014 bx lr
    ];
    let funcs = [(0x1000, 0x1008), (0x1008, 0x1010), (0x1010, 0x1018)];
    let stub = |name, patched| Case { mem: &[(0x1004, 0x1010)], patched, ..one(name, &w, &funcs, &[]) };
    let Some(o) = run(&stub("patched_literal", &[0x1004])) else { return };
    assert_eq!(o.r[0], 2, "the rewritten literal");
    let Some(o) = run(&stub("folded_literal", &[])) else { return };
    assert_eq!(o.r[0], 1, "without `patched` the file's word is folded (read-only text)");
}

const CALLS: [u32; 8] = [
    0xE92D4010, // 1000 f1: push {r4, lr}
    0xEB000001, // 1004 bl f2 (0x1010)
    0xE2800001, // 1008 add r0, r0, #1
    0xE8BD8010, // 100c pop {r4, pc}
    0xE3A00029, // 1010 f2: mov r0, #41
    0xE12FFF1E, // 1014 bx lr
    0xE3A01007, // 1018 f3: mov r1, #7
    0xEAFFFFFB, // 101c b f2 (tail call)
];
const CALL_FUNCS: [(u32, u32); 3] = [(0x1000, 0x1010), (0x1010, 0x1018), (0x1018, 0x1020)];

#[test]
fn direct_call_and_tail_call() {
    let Some(o) = run(&one("direct_call", &CALLS, &CALL_FUNCS, &[(4, 0x44)])) else { return };
    assert_eq!(o.r[0], 42);
    assert_eq!(o.r[4], 0x44);
    assert_eq!(o.r[14], 0x1008, "lr = return address set by bl");
    let c = Case { entry: Some(0x1018), ..one("tail_call", &CALLS, &CALL_FUNCS, &[]) };
    let Some(o) = run(&c) else { return };
    assert_eq!((o.r[0], o.r[1]), (41, 7));
}

#[test]
fn hook_replaces_direct_call() {
    let c = Case {
        hooks: &[(0x1010, "hook_f2")],
        extra_c: "static void hook_f2(XrCpu* c) { c->r[0] = 100; }\n",
        ..one("hook_call", &CALLS, &CALL_FUNCS, &[])
    };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[0], 101);
}

#[test]
fn bl_to_unlisted_misses() {
    let w = [
        0xEB000000, // bl 0x1008 (inside, but not a listed function start)
        0xE12FFF1E, // bx lr
        0xE3A00001, // mov r0, #1
    ];
    let Some(o) = run(&one("bl_unlisted", &w, &span(&w), &[])) else { return };
    assert_eq!(o.miss, Some(0x1008));
}

#[test]
fn undefined_after_call_is_inline_miss() {
    let w = [
        0xEB000000, // 1000 bl f2 (0x1008), as before a noreturn call
        0xFFFFFFFF, // 1004 data after the call
        0xE12FFF1E, // 1008 f2: bx lr (returns after all)
    ];
    let funcs = [(0x1000, 0x1008), (0x1008, 0x100C)];
    let c = one("undef_after_call", &w, &funcs, &[]);
    let (_, undec) = gen_all(&c);
    assert!(undec.is_empty(), "{undec:X?}");
    let Some(o) = run(&c) else { return };
    assert_eq!(o.miss, Some(0x1004), "the callee returned into the data word");
    assert_eq!(o.r[14], 0x1004);
}

#[test]
fn indirect_calls() {
    let w = [
        0xE92D4010, // 1000 push {r4, lr}
        0xE12FFF33, // 1004 blx r3
        0xE2800001, // 1008 add r0, r0, #1
        0xE1A0E00F, // 100c mov lr, pc
        0xE595F004, // 1010 ldr pc, [r5, #4]   (old-style call)
        0xE2800001, // 1014 add r0, r0, #1
        0xE8BD8010, // 1018 pop {r4, pc}
        0xE280000A, // 101c f2: add r0, r0, #10
        0xE12FFF1E, // 1020 bx lr
    ];
    let c = Case { mem: &[(0x2004, 0x101C)], ..one("indirect_calls", &w, &[(0x1000, 0x101C), (0x101C, 0x1024)], &[(0, 0), (3, 0x101C), (5, 0x2000)]) };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[0], 22);
    assert_eq!(o.miss, None);
}

#[test]
fn barrier_then_tls_read() {
    let w = [
        0xEE070FBA, // mcr p15, 0, r0, c7, c10, 5   (barrier: no-op)
        0xEE1D0F70, // mrc p15, 0, r0, c13, c0, 3   (TLS: c->tls)
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("tls", &w, &span(&w), &[])) else { return };
    assert_eq!((o.r[0], o.miss), (0x1FF8_2000, None));
}

#[test]
fn loads_stores_addressing() {
    let w = [
        0xE6910102, // ldr r0, [r1], r2, lsl #2   post-index
        0xE1E420F8, // strd r2, r3, [r4, #8]!
        0xE1020091, // swp r0, r1, [r2]  -> rewritten below
        0xE12FFF1E, // bx lr
    ];
    // swp r6, r7, [r8]
    let mut w = w;
    w[2] = 0xE1086097;
    let s = span(&w);
    let c = Case {
        mem: &[(0x2000, 0xAAAA), (0x3000, 0x5555)],
        ..one("ldst", &w, &s, &[(1, 0x2000), (2, 3), (3, 0x33), (4, 0x2800), (7, 0x77), (8, 0x3000)])
    };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[0], 0xAAAA);
    assert_eq!(o.r[1], 0x200C, "post-index writeback r1 + (3 << 2)");
    assert_eq!(o.r[4], 0x2808, "strd pre-index writeback");
    assert_eq!(o.r[6], 0x5555, "swp loads the old value");
}

/// Every decodable instruction kind must produce C that compiles (with -Wall -Werror, so a flag read before
/// any write would show up as an uninitialized-use error): thousands of random conditional words (every
/// second forced into the VFP coprocessor space), built at -O0 so none of it is dropped as dead code.
#[test]
fn random_insns_compile() {
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut words = Vec::new();
    while words.len() < 4000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let mut w = (x >> 16) as u32;
        if x & 1 == 0 {
            w = (w & 0xF0FF_F0FF) | 0x0C00_0A00 | (x as u32 & 0x0100_0100); // cp10/cp11, bits 27:24 = 1100..1111
            w |= (x >> 8) as u32 & 0x0200_0000;
        }
        let a = 0x10_0000 + 4 * words.len() as u32;
        if w >> 28 < 0xE && !matches!(decode_arm(w, a), Insn::Undefined { .. }) {
            words.push(w);
        }
    }
    let vfp = words.iter().filter(|&&w| matches!(decode_arm(w, 0), Insn::Vfp(_))).count();
    assert!(vfp > 400, "only {vfp} VFP words");
    let f = FuncEntry { start: 0x10_0000, end: 0x10_0000 + 4 * words.len() as u32, mode: Mode::Arm };
    let g = gen_function(&bytes(&words), 0x10_0000, &f, &[f], &GenOptions::default());
    assert!(g.undecodable.is_empty());
    let src = format!("#include \"xr_abi.h\"\n{}", g.c_source);
    let obj = scratch("random").join("r.o");
    if clang("random", &src, &obj, &["-c", "-O0"]).is_some() {
        let size = std::fs::metadata(&obj).unwrap().len();
        assert!(size > 50_000, "object only {size} bytes");
    }
}

/// Random Thumb halfwords (decodable ones, BL/BLX pairs included) generate C that compiles.
#[test]
fn random_thumb_compile() {
    let mut x: u32 = 0x9E37_79B9;
    let mut h: Vec<u16> = Vec::new();
    while h.len() < 6000 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let (a, b) = (x as u16, (x >> 16) as u16 | 0xF800);
        let pc = 0x10_0000 + 2 * h.len() as u32;
        match decode_thumb(a, b, pc) {
            (Insn::Undefined { .. }, _) => {}
            (_, 4) => h.extend([a, b]),
            _ => h.push(a),
        }
    }
    let words = th(&h);
    let f = FuncEntry { start: 0x10_0000, end: 0x10_0000 + 2 * h.len() as u32, mode: Mode::Thumb };
    let g = gen_function(&bytes(&words), 0x10_0000, &f, &[f], &GenOptions::default());
    assert!(g.undecodable.is_empty());
    assert!(g.c_source.contains("void t_00100000(XrCpu* c)"));
    let src = format!("#include \"xr_abi.h\"\n{}", g.c_source);
    let obj = scratch("random_thumb").join("r.o");
    clang("random_thumb", &src, &obj, &["-c", "-O0"]);
}

/// The real code.bin (skips without XR_DUMP): every function (ARM and Thumb, VFP included) generates, the
/// undecodable ones are counted with the reason (the first undefined word they reach), and a sample of the
/// output (every Thumb and every VFP function, every 8th other one) compiles at -O2 -Wall -Werror.
#[test]
fn real_code_bin() {
    let Ok(dump) = std::env::var("XR_DUMP") else {
        println!("skipping real_code_bin: XR_DUMP not set");
        return;
    };
    let Ok(code) = std::fs::read(PathBuf::from(dump).join("exefs/code.bin")) else {
        println!("skipping real_code_bin: no exefs/code.bin");
        return;
    };
    let table = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../functions/x-usa-v0.2.0/code.txt")).unwrap();
    let all = xr_formats::parse_functab(&table).unwrap();
    let opt = GenOptions::default();
    let word = |a: u32| code.get((a - 0x10_0000) as usize..(a - 0x10_0000) as usize + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let has_vfp = |f: &FuncEntry| {
        f.mode == Mode::Arm && (f.start..f.end).step_by(4).any(|a| word(a).is_some_and(|w| matches!(decode_arm(w, a), Insn::Vfp(_))))
    };
    let (mut src, mut undec) = (String::from("#include \"xr_abi.h\"\n"), Vec::new());
    let (mut n_arm, mut n_thumb, mut n_vfp) = (0, 0, 0);
    for (k, f) in all.iter().enumerate() {
        let g = gen_function(&code, 0x10_0000, f, &all, &opt);
        let vfp = has_vfp(f);
        match f.mode {
            Mode::Thumb => n_thumb += 1,
            Mode::Arm => n_arm += 1,
        }
        n_vfp += vfp as usize;
        undec.extend(g.undecodable.iter().map(|&s| (s, f.mode)));
        if g.undecodable.is_empty() && (k % 8 == 0 || vfp || f.mode == Mode::Thumb) {
            src.push_str(&g.c_source);
        }
    }
    // Reason: the first Undefined instruction in the range (linear sweep).
    let mut reasons = std::collections::BTreeMap::<String, Vec<u32>>::new();
    for &(s, mode) in &undec {
        let f = all.iter().find(|f| f.start == s).unwrap();
        let mut a = f.start;
        let mut why = String::from("empty or oversized range");
        let unit = if mode == Mode::Thumb { 2 } else { 4 };
        while a < f.end && f.end - f.start >= unit {
            let (insn, size) = match mode {
                Mode::Arm => (word(a).map_or(Insn::Undefined { word: 0 }, |w| decode_arm(w, a)), 4),
                Mode::Thumb => {
                    let w = word(a & !3).unwrap_or(0);
                    let w2 = word((a + 2) & !3).unwrap_or(0);
                    let half = |w: u32, a: u32| (w >> (8 * (a & 2))) as u16;
                    let (i, n) = decode_thumb(half(w, a), half(w2, a + 2), a);
                    (i, n as u32)
                }
            };
            if let Insn::Undefined { word } = insn {
                why = match mode {
                    Mode::Arm => format!("ARM undefined, bits 27:20 = {:02X}, coproc {}", (word >> 20) & 0xFF, (word >> 8) & 0xF),
                    Mode::Thumb => format!("Thumb undefined {:04X}", word & 0xFF00),
                };
                break;
            }
            a += size;
        }
        reasons.entry(why).or_default().push(s);
    }
    println!("{} functions: {n_arm} ARM ({n_vfp} with VFP), {n_thumb} Thumb; {} undecodable", all.len(), undec.len());
    for (why, v) in &reasons {
        println!("  {:5} {why}: {:08X?}", v.len(), &v[..v.len().min(6)]);
    }
    assert!(undec.len() * 100 < all.len(), "more than 1% undecodable");
    let obj = scratch("real").join("real.o");
    clang("real", &src, &obj, &["-c"]);
}

/// Garbage in never panics: random words, odd ranges, code shorter than the function, Thumb entries.
#[test]
fn never_panics() {
    let mut x: u32 = 0x1234_5678;
    let words: Vec<u32> = (0..20_000).map(|_| { x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); x }).collect();
    let code = bytes(&words);
    let opt = GenOptions { hooks: vec![0x10_0040], ..Default::default() };
    let mut all: Vec<FuncEntry> = (0..500).map(|i| FuncEntry { start: 0x10_0000 + i * 160, end: 0x10_0000 + i * 160 + 160, mode: Mode::Arm }).collect();
    all.push(FuncEntry { start: 0x10_0000 + 500 * 160, end: 0x10_0000 + 500 * 160 + 2, mode: Mode::Thumb });
    for f in &all {
        gen_function(&code, 0x10_0000, f, &all, &opt);
    }
    for f in [
        FuncEntry { start: 0x0FFF_FFF0, end: 0x10_0010, mode: Mode::Arm },
        FuncEntry { start: 0x10_0002, end: 0x10_0040, mode: Mode::Arm },
        FuncEntry { start: 0xFFFF_FFF8, end: 0xFFFF_FFFF, mode: Mode::Arm },
        FuncEntry { start: 0x20_0000, end: 0x10_0000, mode: Mode::Arm },
    ] {
        let g = gen_function(&code, 0x10_0000, &f, &all, &opt);
        assert!(!g.c_source.is_empty());
    }
}

// ---- Thumb, interworking, VFP ----

const BX_LR: u32 = 0xE12FFF1E;

#[test]
fn thumb_sum_loop() {
    let w = th(&[
        0x2000, // 1000 movs r0, #0
        0x2101, // 1002 movs r1, #1
        0x1840, // 1004 loop: adds r0, r0, r1
        0x3101, // 1006 adds r1, #1
        0x290B, // 1008 cmp r1, #11
        0xD1FB, // 100a bne loop
        0x4770, // 100c bx lr
    ]);
    let f = [(BASE, BASE + 14)];
    let Some(o) = run(&Case { thumb: &[BASE], ..one("thumb_sum_loop", &w, &f, &[]) }) else { return };
    assert_eq!(o.r[0], 55);
    assert_eq!(o.r[1], 11);
    assert_eq!(o.miss, None);
}

#[test]
fn interwork_odd_address() {
    let w = [
        0xE12FFF10, // 1000 bx r0                          (ARM)
        0x4770_212A, // 1004 movs r1, #42 ; 1006 bx lr      (Thumb)
    ];
    let f = [(0x1000, 0x1004), (0x1004, 0x1008)];
    let c = Case { thumb: &[0x1004], ..one("interwork_odd", &w, &f, &[(0, 0x1005)]) };
    let (src, undec) = gen_all(&c);
    assert!(undec.is_empty(), "{undec:X?}");
    assert!(src.contains("void t_00001004(XrCpu* c)"), "{src}");
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[1], 42, "the Thumb function ran");
    assert_eq!(o.miss, None);
    // Bit 0 clear: the same address is looked up as ARM, which is not listed -> miss.
    let Some(o) = run(&Case { thumb: &[0x1004], ..one("interwork_even", &w, &f, &[(0, 0x1004)]) }) else { return };
    assert_eq!(o.miss, Some(0x1004));
}

#[test]
fn thumb_pop_pc_returns_to_arm_caller() {
    // Linked with ld.lld at 0x1000 (clang --target=armv6k-none-eabi).
    let mut w = vec![
        0xE92D4010, // 1000 push {r4, lr}
        0xFA000001, // 1004 blx tf (0x1010)
        0xE2800001, // 1008 add r0, r0, #1
        0xE8BD8010, // 100c pop {r4, pc}
    ];
    w.extend(th(&[
        0xB500, // 1010 tf: push {lr}
        0x4802, // 1012 ldr r0, [pc, #8]  (0x101c)
        0xF000, 0xF804, // 1014 bl tf2 (0x1020)
        0x3001, // 1018 adds r0, #1
        0xBD00, // 101a pop {pc}
        0x0028, 0x0000, // 101c .word 40
        0x4770, // 1020 tf2: bx lr
    ]));
    let f = [(0x1000, 0x1010), (0x1010, 0x1020), (0x1020, 0x1022)];
    let c = Case { thumb: &[0x1010, 0x1020], ..one("thumb_pop_pc", &w, &f, &[(4, 0x44)]) };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[0], 42, "40 + 1 (Thumb) + 1 (ARM after the return)");
    assert_eq!(o.r[4], 0x44);
    assert_eq!(o.r[13], 0x8000);
    assert_eq!(o.r[14], 0x1019, "lr from the Thumb bl: return address | 1");
    assert_eq!(o.miss, None);
}

#[test]
fn thumb_blx_imm_to_arm() {
    let mut w = th(&[
        0xB510, // 1000 push {r4, lr}
        0xF000, 0xE804, // 1002 blx af (0x100c)
        0x3001, // 1006 adds r0, #1
        0xBD10, // 1008 pop {r4, pc}
        0x0000, // 100a padding
    ]);
    w.extend([
        0xE3A00029, // 100c af: mov r0, #41
        BX_LR,      // 1010 bx lr
    ]);
    let f = [(0x1000, 0x100C), (0x100C, 0x1014)];
    let c = Case { thumb: &[0x1000], ..one("thumb_blx_arm", &w, &f, &[]) };
    let (src, _) = gen_all(&c);
    assert!(src.contains("f_0000100C(c);"), "direct call of the ARM function:\n{src}");
    let Some(o) = run(&c) else { return };
    assert_eq!(o.r[0], 42);
    assert_eq!(o.r[14], 0x1007, "blx from Thumb: lr = next Thumb insn | 1");
    assert_eq!(o.miss, None);
}

#[test]
fn vfp_add_mul() {
    let w = [
        0xEE000A10, // vmov s0, r0
        0xEE001A90, // vmov s1, r1
        0xEE301A20, // vadd.f32 s2, s0, s1
        0xEE601A20, // vmul.f32 s3, s0, s1
        0xEE112A10, // vmov r2, s2
        0xEE113A90, // vmov r3, s3
        BX_LR,
    ];
    let Some(o) = run(&one("vfp_add_mul", &w, &span(&w), &[(0, 0x3DCC_CCCD), (1, 0x4040_0000)])) else { return };
    assert_eq!(o.r[2], 0x4046_6666, "0.1f + 3.0f");
    assert_eq!(o.r[3], 0x3E99_999A, "0.1f * 3.0f");
    assert_eq!(o.miss, None);
}

#[test]
fn vfp_double() {
    let w = [
        0xEC410B10, // vmov d0, r0, r1
        0xEC432B11, // vmov d1, r2, r3
        0xEE302B01, // vadd.f64 d2, d0, d1
        0xEC554B12, // vmov r4, r5, d2
        BX_LR,
    ];
    let regs = [(0, 0x9999_999A), (1, 0x3FB9_9999), (2, 0x9999_999A), (3, 0x3FC9_9999)];
    let Some(o) = run(&one("vfp_double", &w, &span(&w), &regs)) else { return };
    assert_eq!((o.r[4], o.r[5]), (0x3333_3334, 0x3FD3_3333), "0.1 + 0.2 = 0x3FD3333333333334");
}

#[test]
fn vfp_compare_flags() {
    let w = [
        0xEE000A10, // vmov s0, r0
        0xEE001A90, // vmov s1, r1
        0xEEB40A60, // vcmp.f32 s0, s1
        0xEEF1FA10, // vmrs APSR_nzcv, fpscr
        0xC3A02001, // movgt r2, #1
        0xD3A02002, // movle r2, #2
        0x63A03003, // movvs r3, #3
        0xEEF14A10, // vmrs r4, fpscr
        BX_LR,
    ];
    // (s0, s1) -> (r2, r3, r4 = FPSCR)
    let cases = [
        ("gt", 0x4000_0000, 0x3F80_0000, 1, 0, 0x2300_0000),
        ("lt", 0x3F80_0000, 0x4000_0000, 2, 0, 0x8300_0000),
        ("eq", 0x3F80_0000, 0x3F80_0000, 2, 0, 0x6300_0000),
        ("nan", 0x7FC0_0000, 0x3F80_0000, 2, 3, 0x3300_0000),
        ("denormal_eq_zero", 0x0000_0001, 0x8000_0000, 2, 0, 0x6300_0000),
    ];
    for (tag, a, b, r2, r3, fpscr) in cases {
        let name = format!("vfp_cmp_{tag}");
        let Some(o) = run(&one(&name, &w, &span(&w), &[(0, a), (1, b)])) else { return };
        assert_eq!((o.r[2], o.r[3], o.r[4]), (r2, r3, fpscr), "{tag}");
        assert_eq!(o.cpsr, 0x10, "{tag}: VMRS APSR_nzcv only touches the flag locals");
    }
}

// VFP register transfers (checked against the clang listing: vmov s1, r7 = EE007A90, vmov r7, s1 = EE107A90,
// vmov d1, r6, r7 = EC476B11, vmov r6, r7, d1 = EC576B11).
fn vmov_s_r(s: u32, r: u32) -> u32 { 0xEE00_0A10 | (s >> 1) << 16 | (s & 1) << 7 | r << 12 }
fn vmov_r_s(r: u32, s: u32) -> u32 { 0xEE10_0A10 | (s >> 1) << 16 | (s & 1) << 7 | r << 12 }
fn vmov_d_rr(d: u32, a: u32, b: u32) -> u32 { 0xEC40_0B10 | b << 16 | a << 12 | d }
fn vmov_rr_d(a: u32, b: u32, d: u32) -> u32 { 0xEC50_0B10 | b << 16 | a << 12 | d }

/// Runs `words` + `bx lr` as one ARM function with `regs`.
fn vrun(name: &str, mut words: Vec<u32>, regs: &[(usize, u32)]) -> Option<Out> {
    words.push(BX_LR);
    let o = run(&one(name, &words, &span(&words), regs))?;
    assert_eq!(o.miss, None, "{name}");
    Some(o)
}

/// `pre`, then `op` (s0 <- f(s0)) on each input in r0..; results come back in the same registers.
fn f32_unary(name: &str, pre: &[u32], op: u32, inputs: &[u32]) -> Option<Vec<u32>> {
    assert!(inputs.len() <= 13);
    let mut w = pre.to_vec();
    for k in 0..inputs.len() as u32 {
        w.extend([vmov_s_r(0, k), op, vmov_r_s(k, 0)]);
    }
    let regs: Vec<(usize, u32)> = inputs.iter().copied().enumerate().collect();
    let o = vrun(name, w, &regs)?;
    Some(o.r[..inputs.len()].to_vec())
}

/// `op` (s0 <- f(d0)) on each f64 input, passed in r(2k), r(2k+1); result in r(2k).
fn f64_to_32(name: &str, op: u32, inputs: &[u64]) -> Option<Vec<u32>> {
    assert!(inputs.len() <= 6);
    let mut w = Vec::new();
    let mut regs = Vec::new();
    for (k, &v) in inputs.iter().enumerate() {
        let k = k as u32;
        w.extend([vmov_d_rr(0, 2 * k, 2 * k + 1), op, vmov_r_s(2 * k, 0)]);
        regs.extend([(2 * k as usize, v as u32), (2 * k as usize + 1, (v >> 32) as u32)]);
    }
    let o = vrun(name, w, &regs)?;
    Some((0..inputs.len()).map(|k| o.r[2 * k]).collect())
}

/// `op` (d0 <- f(s0)) on each f32 input in r(2k); result in r(2k), r(2k+1).
fn f32_to_64(name: &str, op: u32, inputs: &[u32]) -> Option<Vec<u64>> {
    assert!(inputs.len() <= 6);
    let mut w = Vec::new();
    for k in 0..inputs.len() as u32 {
        w.extend([vmov_s_r(0, 2 * k), op, vmov_rr_d(2 * k, 2 * k + 1, 0)]);
    }
    let regs: Vec<(usize, u32)> = inputs.iter().enumerate().map(|(k, &v)| (2 * k, v)).collect();
    let o = vrun(name, w, &regs)?;
    Some((0..inputs.len()).map(|k| o.r[2 * k] as u64 | (o.r[2 * k + 1] as u64) << 32).collect())
}

/// `op` (s0 <- s0 op s1) on each pair, passed in r(2k), r(2k+1); result in r(2k).
fn f32_bin(name: &str, op: u32, pairs: &[(u32, u32)]) -> Option<Vec<u32>> {
    assert!(pairs.len() <= 6);
    let mut w = Vec::new();
    let mut regs = Vec::new();
    for (k, &(a, b)) in pairs.iter().enumerate() {
        let k = k as u32;
        w.extend([vmov_s_r(0, 2 * k), vmov_s_r(1, 2 * k + 1), op, vmov_r_s(2 * k, 0)]);
        regs.extend([(2 * k as usize, a), (2 * k as usize + 1, b)]);
    }
    let o = vrun(name, w, &regs)?;
    Some((0..pairs.len()).map(|k| o.r[2 * k]).collect())
}

/// `op` (d0 <- d0 op d1) on each pair, passed in r(4k)..r(4k+3); result in r(4k), r(4k+1).
fn f64_bin(name: &str, op: u32, pairs: &[(u64, u64)]) -> Option<Vec<u64>> {
    assert!(pairs.len() <= 3);
    let mut w = Vec::new();
    let mut regs = Vec::new();
    for (k, &(a, b)) in pairs.iter().enumerate() {
        let k = 4 * k as u32;
        w.extend([vmov_d_rr(0, k, k + 1), vmov_d_rr(1, k + 2, k + 3), op, vmov_rr_d(k, k + 1, 0)]);
        let k = k as usize;
        regs.extend([(k, a as u32), (k + 1, (a >> 32) as u32), (k + 2, b as u32), (k + 3, (b >> 32) as u32)]);
    }
    let o = vrun(name, w, &regs)?;
    Some((0..pairs.len()).map(|k| o.r[4 * k] as u64 | (o.r[4 * k + 1] as u64) << 32).collect())
}

const NAN32: u32 = 0x7FC0_0000;
const NAN64: u64 = 0x7FF8_0000_0000_0000;

#[test]
fn vfp_ftoi_saturates() {
    let inputs = [
        0x7FC0_0000, // NaN
        0x7F80_0001, // signalling NaN
        0x7F80_0000, // +inf
        0xFF80_0000, // -inf
        0x4F00_0000, // 2^31
        0xCF00_0000, // -2^31
        0xCF00_0001, // -2^31 - 256
        0x4EFF_FFFF, // 2147483520
        0xC02C_CCCD, // -2.7
        0x4F7F_FFFF, // 4294967040
        0x0000_0001, // denormal
        0x4F80_0000, // 2^32
        0xBF80_0000, // -1.0
    ];
    let Some(s) = f32_unary("ftoi_s32", &[], 0xEEBD_0AC0, &inputs) else { return }; // vcvt.s32.f32 s0, s0
    assert_eq!(
        s,
        [0, 0, 0x7FFF_FFFF, 0x8000_0000, 0x7FFF_FFFF, 0x8000_0000, 0x8000_0000, 0x7FFF_FF80, 0xFFFF_FFFE, 0x7FFF_FFFF, 0, 0x7FFF_FFFF, 0xFFFF_FFFF]
    );
    let Some(u) = f32_unary("ftoi_u32", &[], 0xEEBC_0AC0, &inputs) else { return }; // vcvt.u32.f32 s0, s0
    assert_eq!(u, [0, 0, 0xFFFF_FFFF, 0, 0x8000_0000, 0, 0, 0x7FFF_FF80, 0, 0xFFFF_FF00, 0, 0xFFFF_FFFF, 0]);

    let d = [
        0xC1E0_0000_0020_0000, // -2147483649.0
        0x41DF_FFFF_FFE0_0000, // 2147483647.5
        0xC1E0_0000_0010_0000, // -2147483648.5
        0x7FF0_0000_0000_0001, // signalling NaN
        0x41EF_FFFF_FFF0_0000, // 4294967295.5
        0xBFEC_CCCC_CCCC_CCCD, // -0.9
    ];
    let Some(s) = f64_to_32("ftoi_s32_f64", 0xEEBD_0BC0, &d) else { return }; // vcvt.s32.f64 s0, d0
    assert_eq!(s, [0x8000_0000, 0x7FFF_FFFF, 0x8000_0000, 0, 0x7FFF_FFFF, 0]);
    // vcvtr.s32.f64 s0, d0 with the game's FPSCR (0x03000000: nearest); other modes: vfp_ftoi_fpscr_rounding
    let Some(r) = f64_to_32("ftoi_s32r_f64", 0xEEBD_0B40, &d) else { return };
    assert_eq!(r, [0x8000_0000, 0x7FFF_FFFF, 0x8000_0000, 0, 0x7FFF_FFFF, 0xFFFF_FFFF]);
    let Some(u) = f64_to_32("ftoi_u32_f64", 0xEEBC_0BC0, &d) else { return }; // vcvt.u32.f64 s0, d0
    assert_eq!(u, [0, 0x7FFF_FFFF, 0, 0, 0xFFFF_FFFF, 0]);
}

#[test]
fn vfp_ftoi_fpscr_rounding() {
    // 2.5 3.5 -2.5 -3.5 0.5 -0.5 0.1 -0.1 2.7 -2.7 1e10 -1e10
    let inputs = [
        0x4020_0000, 0x4060_0000, 0xC020_0000, 0xC060_0000, 0x3F00_0000, 0xBF00_0000, 0x3DCC_CCCD, 0xBDCC_CCCD, 0x402C_CCCD,
        0xC02C_CCCD, 0x5015_02F9, 0xD015_02F9,
    ];
    let (max, min) = (0x7FFF_FFFFu32, 0x8000_0000u32);
    let n = |v: i32| v as u32;
    let expect = [
        [n(2), n(4), n(-2), n(-4), 0, 0, 0, 0, n(3), n(-3), max, min],         // RN (ties to even)
        [n(3), n(4), n(-2), n(-3), 1, 0, 1, 0, n(3), n(-2), max, min],         // RP
        [n(2), n(3), n(-3), n(-4), 0, n(-1), 0, n(-1), n(2), n(-3), max, min], // RM
        [n(2), n(3), n(-2), n(-3), 0, 0, 0, 0, n(2), n(-2), max, min],         // RZ
    ];
    for (mode, want) in expect.iter().enumerate() {
        let name = format!("ftoi_rmode{mode}");
        let mut w = vec![0xEEE1_CA10]; // vmsr fpscr, r12
        for k in 0..12 {
            w.extend([vmov_s_r(0, k), 0xEEBD_0A40, vmov_r_s(k, 0)]); // vcvtr.s32.f32 s0, s0
        }
        let mut regs: Vec<(usize, u32)> = inputs.iter().copied().enumerate().collect();
        let fpscr = 0x0300_0000 | (mode as u32) << 22; // FZ, DN, RMode
        regs.push((12, fpscr));
        let Some(o) = vrun(&name, w, &regs) else { return };
        assert_eq!(&o.r[..12], want, "rounding mode {mode}");
        assert_eq!(o.fpscr, fpscr, "VMSR stored FPSCR");
    }
}

#[test]
fn vfp_default_nan_and_flush_to_zero_f32() {
    // vdiv.f32 s0, s0, s1
    let Some(r) = f32_bin("f32_div", 0xEE80_0A20, &[(0, 0), (0x3F80_0000, 0), (0x3F80_0000, 0x4040_0000), (0x0040_0000, 0x3F80_0000)]) else { return };
    assert_eq!(r, [NAN32, 0x7F80_0000, 0x3EAA_AAAB, 0], "0/0 = default NaN (x86 gives FFC00000), 1/0 = inf, 1/3, denormal/1 = +0");
    // vsub.f32 s0, s0, s1
    let Some(r) = f32_bin("f32_sub", 0xEE30_0A60, &[(0x7F80_0000, 0x7F80_0000), (0x0080_0000, 0x0080_0001), (0x3F80_0000, 0x3F80_0000)]) else { return };
    assert_eq!(r, [NAN32, 0x8000_0000, 0], "inf-inf; FLT_MIN-(FLT_MIN+ulp) = -denormal -> -0; 1-1 = +0");
    // vadd.f32 s0, s0, s1
    let pairs = [(0xFFC1_2345, 0x3F80_0000), (0x7F80_0001, 0), (0x8000_0000, 0x8000_0000), (0x807F_FFFF, 0), (0x3F80_0000, 0xA180_0000)];
    let Some(r) = f32_bin("f32_add", 0xEE30_0A20, &pairs) else { return };
    assert_eq!(r, [NAN32, NAN32, 0x8000_0000, 0, 0x3F80_0000], "NaN payloads -> default NaN; -0 + -0; -denormal + 0 = +0; 1 - 2^-60 = 1");
    // vmul.f32 s0, s0, s1
    let pairs = [
        (0x0040_0000, 0x4E80_0000), // denormal input * 2^30 -> +0
        (0x8040_0000, 0x4E80_0000), // -denormal * 2^30 -> -0
        (0x3F7F_FFFF, 0x0080_0000), // (1 - 2^-24) * FLT_MIN: tiny before rounding -> 0 (x86 rounds to FLT_MIN)
        (0x8080_0000, 0x3F00_0000), // -FLT_MIN * 0.5 -> -0
        (0x7F7F_FFFF, 0x4000_0000), // overflow -> +inf
        (0xFF7F_FFFF, 0x4000_0000), // -overflow -> -inf
    ];
    let Some(r) = f32_bin("f32_mul", 0xEE20_0A20, &pairs) else { return };
    assert_eq!(r, [0, 0x8000_0000, 0, 0x8000_0000, 0x7F80_0000, 0xFF80_0000]);
    let Some(r) = f32_bin("f32_mul2", 0xEE20_0A20, &[(0x3DCC_CCCD, 0x4040_0000), (0x7F80_0000, 0x3F80_0000)]) else { return };
    assert_eq!(r, [0x3E99_999A, 0x7F80_0000], "0.1f * 3; inf * 1");
    // vnmul.f32 s0, s0, s1: negation after the default NaN
    let Some(r) = f32_bin("f32_nmul", 0xEE20_0A60, &[(0x7FC0_0001, 0x3F80_0000), (0x4000_0000, 0x4040_0000)]) else { return };
    assert_eq!(r, [0xFFC0_0000, 0xC0C0_0000]);
}

#[test]
fn vfp_default_nan_and_flush_to_zero_f64() {
    let pairs = [
        (0x3FEF_FFFF_FFFF_FFFF, 0x0010_0000_0000_0000), // (1 - 2^-53) * DBL_MIN: tiny before rounding -> 0
        (0x0000_0000_0000_0001, 0x4330_0000_0000_0000), // denormal * 2^52 -> 0
        (0xBFF0_0000_0000_0000, 0x0010_0000_0000_0001), // -1 * (DBL_MIN + ulp): normal, kept
    ];
    let Some(r) = f64_bin("f64_mul", 0xEE20_0B01, &pairs) else { return }; // vmul.f64 d0, d0, d1
    assert_eq!(r, [0, 0, 0x8010_0000_0000_0001]);
    let Some(r) = f64_bin("f64_mul2", 0xEE20_0B01, &[(0x3FB9_9999_9999_999A, 0x3FB9_9999_9999_999A), (0xFFEF_FFFF_FFFF_FFFF, 0x4000_0000_0000_0000)]) else { return };
    assert_eq!(r, [0x3F84_7AE1_47AE_147C, 0xFFF0_0000_0000_0000], "0.1 * 0.1; -overflow -> -inf");
    // vsqrt.f64 d0, d1
    let Some(r) = f64_bin("f64_sqrt", 0xEEB1_0BC1, &[(0, 0x4000_0000_0000_0000), (0, 0xBFF0_0000_0000_0000), (0, 0x8000_0000_0000_0000)]) else { return };
    assert_eq!(r, [0x3FF6_A09E_667F_3BCD, NAN64, 0x8000_0000_0000_0000], "sqrt 2; sqrt -1; sqrt -0");
    let Some(r) = f64_bin("f64_div", 0xEE80_0B01, &[(0, 0), (0x3FF0_0000_0000_0000, 0x4008_0000_0000_0000)]) else { return };
    assert_eq!(r, [NAN64, 0x3FD5_5555_5555_5555], "0/0 = default NaN (x86 gives FFF8...), 1/3");
    let Some(r) = f64_bin("f64_sub", 0xEE30_0B41, &[(0x7FF0_0000_0000_0000, 0x7FF0_0000_0000_0000), (0x0010_0000_0000_0000, 0x0010_0000_0000_0001)]) else { return };
    assert_eq!(r, [NAN64, 0x8000_0000_0000_0000]);
    let Some(r) = f64_bin("f64_add", 0xEE30_0B01, &[(0xFFF1_2345_0000_0000, 0), (0x3FB9_9999_9999_999A, 0x3FC9_9999_9999_999A)]) else { return };
    assert_eq!(r, [NAN64, 0x3FD3_3333_3333_3334], "0.1 + 0.2");
    // overflow -> inf; 1 - 2^-60 rounds to 1
    let Some(r) = f64_bin("f64_add2", 0xEE30_0B01, &[(0x7FEF_FFFF_FFFF_FFFF, 0x7FEF_FFFF_FFFF_FFFF), (0x3FF0_0000_0000_0000, 0xBC30_0000_0000_0000)]) else { return };
    assert_eq!(r, [0x7FF0_0000_0000_0000, 0x3FF0_0000_0000_0000]);
    let Some(r) = f64_bin("f64_div2", 0xEE80_0B01, &[(0x3FF0_0000_0000_0000, 0x4024_0000_0000_0000), (0x3FF0_0000_0000_0000, 0)]) else { return };
    assert_eq!(r, [0x3FB9_9999_9999_999A, 0x7FF0_0000_0000_0000], "1/10, 1/0 = inf");
}

#[test]
fn vfp_conversions() {
    // vcvt.f32.f64 s0, d0
    let d = [0x380F_FFFF_F000_0000, 0x3FB9_9999_9999_999A, 0x7FF0_0000_0000_0001, 0x7E37_E43C_8800_759C, 0x8000_0000_0000_0001];
    let Some(r) = f64_to_32("cvt_f32_f64", 0xEEB7_0BC0, &d) else { return };
    assert_eq!(r, [0, 0x3DCC_CCCD, NAN32, 0x7F80_0000, 0x8000_0000], "tiny before rounding -> 0 (x86 gives FLT_MIN); 0.1; NaN; 1e300 -> inf; -denormal");
    // vcvt.f64.f32 d0, s0
    let Some(r) = f32_to_64("cvt_f64_f32", 0xEEB7_0AC0, &[0x0000_0001, 0xFFC1_2345, 0x3DCC_CCCD, 0xBF80_0000]) else { return };
    assert_eq!(r, [0, NAN64, 0x3FB9_9999_A000_0000, 0xBFF0_0000_0000_0000]);
    // vcvt.f32.s32 s0, s0 / vcvt.f32.u32 s0, s0
    let ints = [0x8000_0000, 0x7FFF_FFFF, 16_777_217, 0xFFFF_FFFF];
    let Some(r) = f32_unary("cvt_f32_s32", &[], 0xEEB8_0AC0, &ints) else { return };
    assert_eq!(r, [0xCF00_0000, 0x4F00_0000, 0x4B80_0000, 0xBF80_0000], "2^31-1 rounds to 2^31; 2^24+1 ties to even");
    let Some(r) = f32_unary("cvt_f32_u32", &[], 0xEEB8_0A40, &ints) else { return };
    assert_eq!(r, [0x4F00_0000, 0x4F00_0000, 0x4B80_0000, 0x4F80_0000]);
    // vcvt.f64.s32 d0, s0
    let Some(r) = f32_to_64("cvt_f64_s32", 0xEEB8_0BC0, &[0xFFFF_FFFF, 0x8000_0000]) else { return };
    assert_eq!(r, [0xBFF0_0000_0000_0000, 0xC1E0_0000_0000_0000]);
}

#[test]
fn vfp_unary_ops() {
    let inputs = [0xC080_0000, 0x7FC1_2345, 0x0000_0001, 0x8000_0000, 0xBF80_0000];
    // vsqrt.f32 s0, s0: -4 -> NaN; NaN -> default; denormal -> +0; -0 -> -0; -1 -> NaN
    let Some(r) = f32_unary("vsqrt", &[], 0xEEB1_0AC0, &inputs) else { return };
    assert_eq!(r, [NAN32, NAN32, 0, 0x8000_0000, NAN32]);
    let Some(r) = f32_unary("vsqrt2", &[], 0xEEB1_0AC0, &[0x4080_0000, 0x4000_0000]) else { return };
    assert_eq!(r, [0x4000_0000, 0x3FB5_04F3]);
    // vabs / vneg are bit operations: NaN payloads and denormals pass through
    let Some(r) = f32_unary("vabs", &[], 0xEEB0_0AC0, &inputs) else { return };
    assert_eq!(r, [0x4080_0000, 0x7FC1_2345, 0x0000_0001, 0, 0x3F80_0000]);
    let Some(r) = f32_unary("vneg", &[], 0xEEB1_0A40, &inputs) else { return };
    assert_eq!(r, [0x4080_0000, 0xFFC1_2345, 0x8000_0001, 0, 0x3F80_0000]);
}

#[test]
fn vfp_multiply_accumulate_is_not_fused() {
    // s2 = r(3k), s0 = r(3k+1), s1 = r(3k+2); op; r(3k) = s2.  n = m = 1 + 2^-12, d = -(1 + 2^-11):
    // the product rounds to 1 + 2^-11 first (fused would leave 2^-24).
    let ops = [
        (0xEE00_1A20u32, 0x0000_0000u32), // vmla.f32 s2, s0, s1: d + n*m
        (0xEE00_1A60, 0xC000_1000),       // vmls: d - n*m
        (0xEE10_1A60, 0x0000_0000),       // vnmla: -d - n*m
        (0xEE10_1A20, 0x4000_1000),       // vnmls: -d + n*m
    ];
    let mut w = Vec::new();
    let mut regs = Vec::new();
    for (k, &(op, _)) in ops.iter().enumerate() {
        let r = 3 * k as u32;
        w.extend([vmov_s_r(2, r), vmov_s_r(0, r + 1), vmov_s_r(1, r + 2), op, vmov_r_s(r, 2)]);
        regs.extend([(r as usize, 0xBF80_1000), (r as usize + 1, 0x3F80_0800), (r as usize + 2, 0x3F80_0800)]);
    }
    let Some(o) = vrun("vmla", w, &regs) else { return };
    for (k, &(op, want)) in ops.iter().enumerate() {
        assert_eq!(o.r[3 * k], want, "op {op:08X}");
    }
}

#[test]
fn vfp_loads_stores() {
    let w = [
        0xED900B00, // 1000 vldr d0, [r0]
        0xED800B02, // 1004 vstr d0, [r0, #8]
        0xED102A01, // 1008 vldr s4, [r0, #-4]
        0xED802A04, // 100c vstr s4, [r0, #16]
        0xECB18B04, // 1010 vldmia r1!, {d8-d9}
        0xED2D8B04, // 1014 vpush {d8-d9}
        0xECBDAA04, // 1018 vpop {s20-s23}
        0xED21AA02, // 101c vstmdb r1!, {s20-s21}
        0xEDDF2A08, // 1020 vldr s5, [pc, #32]   (0x1048)
        0xEC532B10, // 1024 vmov r2, r3, d0
        0xEE124A10, // 1028 vmov r4, s4
        0xE5905008, // 102c ldr r5, [r0, #8]
        0xE590600C, // 1030 ldr r6, [r0, #12]
        0xE5907010, // 1034 ldr r7, [r0, #16]
        0xE51D8010, // 1038 ldr r8, [sp, #-16]
        0xEC5A9B1B, // 103c vmov r9, r10, d11
        0xE591B000, // 1040 ldr r11, [r1]
        0xEE12CA90, // 1044 vmov r12, s5
        BX_LR,      // 1048 (also the literal: e12fff1e)
    ];
    let mem = [(0x2000, 0x1111_1111), (0x2004, 0x2222_2222), (0x1FFC, 0x3333_3333), (0x3000, 0xA0), (0x3004, 0xA1), (0x3008, 0xA2), (0x300C, 0xA3)];
    let s = span(&w);
    let c = Case { mem: &mem, ..one("vfp_ldst", &w, &s, &[(0, 0x2000), (1, 0x3000)]) };
    let Some(o) = run(&c) else { return };
    assert_eq!((o.r[2], o.r[3]), (0x1111_1111, 0x2222_2222), "d0: low word at the lower address");
    assert_eq!(o.r[4], 0x3333_3333, "vldr s, negative offset");
    assert_eq!((o.r[5], o.r[6], o.r[7]), (0x1111_1111, 0x2222_2222, 0x3333_3333), "vstr d / s");
    assert_eq!(o.r[8], 0xA0, "vpush stored d8 low word at sp-16");
    assert_eq!((o.r[9], o.r[10]), (0xA2, 0xA3), "vpop into singles: d11 = s22:s23");
    assert_eq!(o.r[11], 0xA0, "vstmdb stored s20 at r1-8");
    assert_eq!(o.r[1], 0x3008, "vldmia! then vstmdb! writeback");
    assert_eq!(o.r[12], BX_LR, "vldr [pc] literal");
    assert_eq!(o.r[13], 0x8000, "vpush/vpop balance");
}

// ---- flags across function boundaries ----

/// A split function: the flags set before a B across a literal pool are read by the next piece (a separate
/// function, as repair (a) adds it). The tail call must store them into c->cpsr.
#[test]
fn flags_cross_tail_call() {
    let w = [
        0xE3500005, // 1000 cmp r0, #5
        0xEA000000, // 1004 b 0x100c (tail call to the next piece)
        0xDEADBEEF, // 1008 literal pool
        0x0A000001, // 100c L: beq taken (0x1018)
        0xE3A01001, // 1010 mov r1, #1
        BX_LR,      // 1014
        0xE3A01002, // 1018 taken: mov r1, #2
        BX_LR,      // 101c
    ];
    let f = [(0x1000, 0x100C), (0x100C, 0x1020)];
    let (src, _) = gen_all(&one("flags_tail_src", &w, &f, &[]));
    assert!(src.contains("c->cpsr = (c->cpsr & ~0x40000000u) | fl_z << 30; XR_SPILL(); c->ret_lr = e_lr; c->ret_sp = e_sp; XR_TAIL return f_0000100C(c);"), "{src}");
    for (r0, want) in [(5, 2), (6, 1)] {
        let Some(o) = run(&one(&format!("flags_tail_{r0}"), &w, &f, &[(0, r0)])) else { return };
        assert_eq!(o.r[1], want, "r0 = {r0}");
        assert_eq!(o.miss, None);
    }
}

/// A caller with live flags tail-calls a whole-function stub (undecodable): the interpreter gets the caller's
/// NZCV in c->cpsr, since the stub's entry needs all four.
#[test]
fn flags_into_undecodable_stub() {
    let w = [
        0xE3500005, // 1000 cmp r0, #5
        0xEA000000, // 1004 b 0x100c (tail call)
        0xDEADBEEF, // 1008 literal pool
        0xFFFFFFFF, // 100c undefined: the whole function is a stub
        BX_LR,      // 1010
    ];
    let f = [(0x1000, 0x100C), (0x100C, 0x1014)];
    for (r0, nzcv) in [(4, 0x8), (5, 0x6), (6, 0x2)] {
        let Some(o) = run(&one(&format!("flags_stub_{r0}"), &w, &f, &[(0, r0)])) else { return };
        assert_eq!(o.miss, Some(0x100C));
        assert_eq!(o.interp[0].3, 0x10 | nzcv << 28, "r0 = {r0}: {:?}", o.interp);
    }
}

/// A flag-returning helper (cmp; bx lr) called through a pass-through function (push; bl; pop {pc}); the
/// outer caller branches on the flags after its bl.
#[test]
fn flags_returned_by_helper() {
    let w = [
        0xE92D4010, // 1000 push {r4, lr}
        0xEB000002, // 1004 bl mid (0x1014)
        0x03A01001, // 1008 moveq r1, #1
        0x13A01002, // 100c movne r1, #2
        0xE8BD8010, // 1010 pop {r4, pc}
        0xE92D4010, // 1014 mid: push {r4, lr}
        0xEB000000, // 1018 bl helper (0x1020)
        0xE8BD8010, // 101c pop {r4, pc}
        0xE1500002, // 1020 helper: cmp r0, r2
        BX_LR,      // 1024
    ];
    let f = [(0x1000, 0x1014), (0x1014, 0x1020), (0x1020, 0x1028)];
    let (src, _) = gen_all(&one("flags_ret_src", &w, &f, &[]));
    assert!(src.contains("c->cpsr = (c->cpsr & ~0x40000000u) | fl_z << 30; XR_SPILL(); if (t != e_lr)"), "helper stores Z:\n{src}");
    for (r2, want) in [(7, 1), (8, 2)] {
        let Some(o) = run(&one(&format!("flags_ret_{r2}"), &w, &f, &[(0, 7), (2, r2)])) else { return };
        assert_eq!(o.r[1], want, "r2 = {r2}");
        assert_eq!(o.miss, None);
    }
}

/// Functions nobody reads flags from store none (fast path).
#[test]
fn no_flag_stores_when_unneeded() {
    let (src, _) = gen_all(&one("no_flag_stores", &CALLS, &CALL_FUNCS, &[]));
    assert!(!src.contains("c->cpsr ="), "{src}");
}

#[test]
fn tls_read() {
    let w = [
        0xEE1D3F70, // mrc p15, 0, r3, c13, c0, 3
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("tls_read", &w, &span(&w), &[])) else { return };
    assert_eq!((o.r[3], o.miss), (0x1FF8_2000, None));
    let w = [
        0xEE1D3F50, // mrc p15, 0, r3, c13, c0, 2 (not the TLS register)
        0xE12FFF1E, // bx lr
    ];
    let Some(o) = run(&one("tls_other", &w, &span(&w), &[])) else { return };
    assert_eq!(o.miss, Some(BASE));
}

#[test]
fn fall_off_targets_after_last_instruction() {
    // ARM entry whose end is mid-word: the last whole instruction is at 0x1004, so fall-off is 0x1008... of a listed fn.
    let w = [0xE3A00001, 0xE3A01002, 0xE12FFF1E];
    let funcs = [(0x1000, 0x1006), (0x1008, 0x100C)];
    let (src, _) = gen_all(&one("fall_off_misaligned", &w, &funcs, &[]));
    assert!(src.contains("falls off the end"), "{src}");
    assert!(src.contains("0x00001008") || src.contains("f_00001008"), "{src}");
    assert!(!src.contains("0x00001006"), "{src}");
}

#[test]
fn thumb_inline_miss_keeps_thumb_bit() {
    let w = th(&[0xF000, 0xF802, 0xDEFF, 0, 0x4770, 0]); // 1000 bl 0x1008; 1004 udf; 1008 bx lr
    let funcs = [(0x1000, 0x1008), (0x1008, 0x100A)];
    let c = Case { thumb: &[0x1000, 0x1008], ..one("thumb_undef_after_call", &w, &funcs, &[]) };
    let (src, undec) = gen_all(&c);
    assert!(undec.is_empty(), "{undec:X?}");
    assert!(src.contains("xr_rt->interp(c, 0x00001005u, e_lr, e_sp)"), "{src}");
}

// ---- ABI v3: interpreter fallback, back-edge preemption, checked returns ----

/// A fake `interp` that only logs (the guest "returns" at once with what it left in `c`).
const NOTE_ONLY: &str = "static void it_note(XrCpu* c, uint32_t s, uint32_t u, uint32_t sp) { note_interp(c, s, u, sp); }\n";

/// One function covering `words`, register `reg` set, with the logging-only interp.
fn run_noted(name: &str, words: &[u32], reg: (usize, u32)) -> Option<Out> {
    let (s, regs) = (span(words), [reg]);
    run(&Case { interp: "it_note", extra_c: NOTE_ONLY, ..one(name, words, &s, &regs) })
}

#[test]
fn miss_call_shaped_resumes() {
    let w = [
        0xE92D4010, // 1000 push {r4, lr}
        0xEB0000FD, // 1004 bl 0x1400 (unlisted)
        0xE2800001, // 1008 add r0, r0, #1
        0xE8BD8010, // 100c pop {r4, pc}
    ];
    let s = span(&w);
    let c = Case {
        interp: "it7",
        extra_c: "static void it7(XrCpu* c, uint32_t s, uint32_t u, uint32_t sp) { note_interp(c, s, u, sp); c->r[0] = 7; }\n",
        ..one("miss_call_shaped", &w, &s, &[])
    };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.interp.iter().map(|x| (x.0, x.1, x.2)).collect::<Vec<_>>(), vec![(0x1400, 0x1008, 0x7FF8)], "start, return address, sp at the call");
    assert_eq!((o.r[0], o.miss), (8, None), "native code continued after interp");
}

#[test]
fn miss_tail_shaped_returns() {
    let w = [
        0xE92D4010, // 1000 push {r4, lr}
        0xEB000001, // 1004 bl f2 (0x1010)
        0xE2800001, // 1008 add r0, r0, #1
        0xE8BD8010, // 100c pop {r4, pc}
        0xE3A00005, // 1010 f2: mov r0, #5
        0xEA0000F9, // 1014 b 0x1400 (unlisted)
    ];
    let c = Case {
        interp: "it10",
        extra_c: "static void it10(XrCpu* c, uint32_t s, uint32_t u, uint32_t sp) { note_interp(c, s, u, sp); c->r[0] += 10; }\n",
        ..one("miss_tail_shaped", &w, &[(0x1000, 0x1010), (0x1010, 0x1018)], &[])
    };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.interp.iter().map(|x| (x.0, x.1, x.2)).collect::<Vec<_>>(), vec![(0x1400, 0x1008, 0x7FF8)], "f2's entry lr/sp");
    assert_eq!((o.r[0], o.miss), (16, None), "f2 returned to f1 after interp");
}

#[test]
fn backedge_preempts() {
    let w = [
        0xE3A00000, // 1000 mov r0, #0
        0xE3E01FFA, // 1004 mvn r1, #1000      (r1 = -1001)
        0xE3510000, // 1008 cmp r1, #0         (N = 1)
        0x42800002, // 100c loop: addmi r0, r0, #2   (reads N: live across the back-edge)
        0xE2911001, // 1010 adds r1, r1, #1
        0x4AFFFFFC, // 1014 bmi loop           (taken 1000 times)
        0xE12FFF1E, // 1018 bx lr
    ];
    let s = span(&w);
    let Some(o) = run(&Case { budget: 100, ..one("backedge_preempts", &w, &s, &[]) }) else { return };
    assert_eq!((o.r[0], o.miss), (2002, None));
    assert_eq!(o.preempt.len(), 10);
    for (k, &(r0, cpsr)) in o.preempt.iter().enumerate() {
        assert_eq!(r0, 200 * (k as u32 + 1), "registers spilled before preempt");
        assert_eq!(cpsr >> 31, 1, "live N stored before preempt");
    }
    // Forward branches and straight-line code never touch the budget.
    let (src, _) = gen_all(&one("forward_only", &[0x0A000000, 0xE3A00001, BX_LR], &[(0x1000, 0x100C)], &[]));
    assert!(!src.contains("budget"), "{src}");
}

#[test]
fn flags_survive_interp() {
    let w = [
        0xE92D4010, // 1000 push {r4, lr}
        0xE3500005, // 1004 cmp r0, #5
        0xEB0000FC, // 1008 bl 0x1400 (unlisted; the fake interp leaves the flags)
        0x03A01001, // 100c moveq r1, #1
        0x13A01002, // 1010 movne r1, #2
        0xE8BD8010, // 1014 pop {r4, pc}
    ];
    for (r0, want) in [(5, 1), (4, 2)] {
        let Some(o) = run_noted("flags_survive_interp", &w, (0, r0)) else { return };
        assert_eq!((o.r[1], o.miss), (want, None), "r0 = {r0}");
    }
}

#[test]
fn flags_reach_interp() {
    let w = [
        0xE1500001, // 1000 cmp r0, r1
        0xEA0000FD, // 1004 b 0x1400 (unlisted)
    ];
    let s = span(&w);
    let c = Case { interp: "it_note", extra_c: NOTE_ONLY, ..one("flags_reach_interp", &w, &s, &[(0, 3), (1, 5)]) };
    let Some(o) = run(&c) else { return };
    assert_eq!(o.interp, vec![(0x1400, 0xDEAD_0000, 0x8000, 0x8000_0010)], "3 - 5: N set, C clear");
}

#[test]
fn thumb_miss_has_t_bit() {
    let w = th(&[
        0xB500, // 1000 push {lr}
        0xF000, 0xF87D, // 1002 bl 0x1100 (unlisted)
        0xE0FB, // 1006 b.n 0x1200 (unlisted)
    ]);
    let c = Case { thumb: &[0x1000], interp: "it_note", extra_c: NOTE_ONLY, ..one("thumb_miss_t_bit", &w, &[(0x1000, 0x1008)], &[]) };
    let Some(o) = run(&c) else { return };
    let got: Vec<_> = o.interp.iter().map(|x| (x.0, x.1, x.2)).collect();
    assert_eq!(got, vec![(0x1101, 0x1007, 0x7FFC), (0x1201, 0xDEAD_0000, 0x8000)]);
}

#[test]
fn wrong_return_goes_to_interp() {
    let w = [
        0xE52DE004, // 1000 push {lr}
        0xE58D0000, // 1004 str r0, [sp]     (overwrite the saved lr)
        0xE49DF004, // 1008 pop {pc}
    ];
    let Some(o) = run_noted("wrong_return_pop", &w, (0, 0x3000)) else { return };
    assert_eq!(o.interp, vec![(0x3000, 0xDEAD_0000, 0x8000, 0x10)]);
    let Some(o) = run_noted("right_return_pop", &w, (0, 0xDEAD_0000)) else { return };
    assert_eq!((o.interp.len(), o.miss), (0, None), "matching return is a plain return");

    let w = [0xE1A0E000, BX_LR]; // mov lr, r0; bx lr
    let Some(o) = run_noted("wrong_return_bx", &w, (0, 0x3001)) else { return };
    assert_eq!(o.interp, vec![(0x3001, 0xDEAD_0000, 0x8000, 0x10)]);

    // A computed jump to the entry lr with the entry sp is a return; anything else unknown goes to interp.
    let w = [0xE12FFF13]; // bx r3
    let Some(o) = run_noted("dispatch_return", &w, (3, 0xDEAD_0000)) else { return };
    assert_eq!((o.interp.len(), o.miss), (0, None));
    let Some(o) = run_noted("dispatch_unknown", &w, (3, 0x5000)) else { return };
    assert_eq!(o.interp, vec![(0x5000, 0xDEAD_0000, 0x8000, 0x10)]);
}

#[test]
fn null_hooks_ok() {
    let c = Case { hooks: &[(0x1010, "hook_f2")], null_hooks: true, ..one("null_hooks", &CALLS, &CALL_FUNCS, &[]) };
    let (src, _) = gen_all(&c);
    assert!(src.contains("hooks["), "the call is hooked: {src}");
    let Some(o) = run(&c) else { return };
    assert_eq!((o.r[0], o.miss), (42, None), "NULL hooks table: the direct call runs");
}

// ---- the frame's return travels through tail calls; backward tail calls pay the budget ----

/// Split function: piece 1 calls, then tail-calls piece 2, which returns with piece 1's frame.
#[test]
fn piece_after_call_returns_natively() {
    let w = [
        0xE52DE004, // 1000 push {lr}
        0xEB000001, // 1004 bl 0x1010
        0xEAFFFFFF, // 1008 b 0x100c (piece 2)
        0xE49DF004, // 100c piece 2: pop {pc}
        BX_LR,      // 1010 callee
    ];
    let f = [(0x1000, 0x100C), (0x100C, 0x1010), (0x1010, 0x1014)];
    let c = Case { interp: "it_note", extra_c: NOTE_ONLY, ..one("piece_after_call", &w, &f, &[]) };
    let Some(o) = run(&c) else { return };
    assert_eq!((o.interp.clone(), o.miss), (vec![], None), "piece 2 returns to the entry lr natively");
    assert_eq!(o.r[13], 0x8000);
}

#[test]
fn piece_after_indirect_call_returns_natively() {
    let w = [
        0xE52DE004, // 1000 push {lr}
        0xE12FFF33, // 1004 blx r3 (0x1010)
        0xEAFFFFFF, // 1008 b 0x100c (piece 2)
        0xE49DF004, // 100c piece 2: pop {pc}
        BX_LR,      // 1010 callee
    ];
    let f = [(0x1000, 0x100C), (0x100C, 0x1010), (0x1010, 0x1014)];
    let regs = [(3, 0x1010)];
    let c = Case { interp: "it_note", extra_c: NOTE_ONLY, ..one("piece_after_blx", &w, &f, &regs) };
    let Some(o) = run(&c) else { return };
    assert_eq!((o.interp.clone(), o.miss), (vec![], None));
}

/// f1 and f2 tail-call each other; only f2's B is backward, so each round pays the budget once.
#[test]
fn tail_call_loop_preempts() {
    let w = [
        0xE2500001, // 1000 f1: subs r0, r0, #1
        0x012FFF1E, // 1004 bxeq lr
        0xEAFFFFFF, // 1008 b f2 (0x100c, forward)
        0xE2811001, // 100c f2: add r1, r1, #1
        0xEAFFFFFA, // 1010 b f1 (0x1000, backward)
    ];
    let f = [(0x1000, 0x100C), (0x100C, 0x1014)];
    let regs = [(0, 1001), (1, 0)];
    let Some(o) = run(&Case { budget: 100, ..one("tail_call_loop", &w, &f, &regs) }) else { return };
    assert_eq!((o.r[0], o.r[1], o.miss), (0, 1000, None));
    let r0s: Vec<u32> = o.preempt.iter().map(|p| p.0).collect();
    assert_eq!(r0s, (1..=10).map(|k| 1001 - 100 * k).collect::<Vec<_>>(), "one charge per backward tail call");
}

// ---- module mode (CRO code, base-relative) ----

/// A CRO-shaped module compiled once and run at two bases: a relocated literal (pointer into the module), a jump
/// table of absolute relocated cases after `ldrls pc, [pc, r3, lsl #2]`, an import thunk (`ldr pc, [pc, #-4]` +
/// patched word) calling a code.bin function through `xr_lookup`, return addresses (r14 = base + offset, checked
/// by the callee's return), a PC read, a literal not in `patched` that is still loaded at run time (module mode never folds). Offsets are the module's.
#[test]
fn module_runs_at_any_base() {
    let w: [u32; 33] = [
        0xE51FF004, // 00 thunk: ldr pc, [pc, #-4]
        0x00000000, // 04 import word, patched to code.bin 0x00100000
        0xE92D4010, // 08 push {r4, lr}
        0xE59F1050, // 0c ldr r1, [pc, #0x50]   (64: relocated, base + 0x80)
        0xE5912000, // 10 ldr r2, [r1]
        0xE2003003, // 14 and r3, r0, #3
        0xE3530002, // 18 cmp r3, #2
        0x979FF103, // 1c ldrls pc, [pc, r3, lsl #2]
        0xEA000008, // 20 b 48
        0x00000030, // 24 case 0 (relocated)
        0x00000038, // 28 case 1 (relocated)
        0x00000040, // 2c case 2 (relocated)
        0xE3A0A010, // 30 mov r10, #0x10
        0xEA000004, // 34 b 4c
        0xE3A0A020, // 38 mov r10, #0x20
        0xEA000002, // 3c b 4c
        0xE3A0A030, // 40 mov r10, #0x30
        0xEA000000, // 44 b 4c
        0xE3A0A040, // 48 mov r10, #0x40
        0xEBFFFFEB, // 4c bl 00 (thunk)
        0xE1A0600E, // 50 mov r6, lr
        0xE28F5000, // 54 add r5, pc, #0
        0xE59F7008, // 58 ldr r7, [pc, #8]      (68: not relocated, still loaded at run time)
        0xEB000003, // 5c bl 70
        0xE8BD8010, // 60 pop {r4, pc}
        0x00000080, // 64 relocated literal
        0xCAFEF00D, // 68 literal, not relocated; main() overwrites it
        0x00000000, // 6c
        0xE2800001, // 70 helper: add r0, r0, #1
        BX_LR,      // 74
        0x00000000, // 78
        0x00000000, // 7c
        0x12345678, // 80 data
    ];
    let relocated = [0x24u32, 0x28, 0x2C, 0x64];
    let patched: Vec<u32> = [0x04].iter().chain(&relocated).copied().collect();
    let all = [(0x00, 0x08), (0x08, 0x6C), (0x70, 0x78)].map(|(start, end)| FuncEntry { start, end, mode: Mode::Arm });
    let code = bytes(&w);
    let opt = GenOptions { flags: flag_table(&code, 0, &all, &[]), patched, module: Some(3), ..Default::default() };
    let csrc: String = all.iter().map(|f| gen_function(&code, 0, f, &all, &opt).c_source).collect();
    assert!(csrc.contains("void m3_f_000008(XrCpu* c)") && csrc.contains("switch (dt - mb)"), "{csrc}");
    let lookups: String = all.iter().map(|f| format!("    if (o == 0x{:X}u) return m3_f_{:06X};\n", f.start, f.start)).collect();
    let relocs: String = relocated.iter().map(|o| format!("    st(b + 0x{o:X}u, b + ld(b + 0x{o:X}u));\n")).collect();
    let src = format!(
        r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "xr_abi.h"
uint8_t* xr_mem;
const XrRuntime* xr_rt;
uint32_t xr_mod_base[4];
static XrCpu cpu;
static uint32_t ld(uint32_t a) {{ uint32_t v; memcpy(&v, xr_mem + a, 4); return v; }}
static void st(uint32_t a, uint32_t v) {{ memcpy(xr_mem + a, &v, 4); }}
static void svc_cb(XrCpu* c, uint32_t imm) {{ (void)c; (void)imm; }}
static void interp_cb(XrCpu* c, uint32_t s, uint32_t u, uint32_t sp) {{ (void)c; printf("interp %08x %08x %08x\n", s, u, sp); exit(3); }}
static void preempt_cb(XrCpu* c) {{ c->budget = 1 << 30; }}
/* a code.bin function the module imports: notes its return address and frame */
void f_00100000(XrCpu* c) {{ c->r[0] += 0x100; c->r[8] = c->r[14]; c->r[9] = c->ret_lr; }}
{csrc}
XrFn xr_lookup(uint32_t a) {{
    if (a == 0x100000u) return f_00100000;
    uint32_t o = a - xr_mod_base[3];
{lookups}    return 0;
}}
int main(int argc, char** argv) {{
    uint32_t b = (uint32_t)strtoul(argv[1], 0, 16);
    xr_mem = calloc(1, 0x1000000);
    static const uint8_t code[] = {{{code}}};
    memcpy(xr_mem + b, code, sizeof code);
{relocs}    st(b + 4, 0x100000u);
    st(b + 0x68, 0xFEEDFACEu); /* module mode never folds literals: r7 must see this */
    xr_mod_base[3] = b;
    static XrRuntime rt; rt.abi_version = XR_ABI_VERSION; rt.mem = xr_mem; rt.svc = svc_cb; rt.interp = interp_cb; rt.preempt = preempt_cb; xr_rt = &rt;
    cpu.r[0] = (uint32_t)strtoul(argv[2], 0, 16); cpu.r[13] = 0x8000; cpu.r[14] = 0xDEAD0000u; cpu.budget = 1 << 30;
    cpu.ret_lr = cpu.r[14]; cpu.ret_sp = cpu.r[13];
    m3_f_000008(&cpu);
    for (int i = 0; i < 16; i++) printf("%08x\n", cpu.r[i]);
    return 0;
}}
"#,
        code = code.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",")
    );
    let exe = scratch("module_bases").join("t.exe");
    if clang("module_bases", &src, &exe, &[]).is_none() {
        return;
    }
    for base in [0x0030_0180u32, 0x00C4_5000] {
        for (r0, case) in [(0u32, 0x10u32), (1, 0x20), (2, 0x30), (3, 0x40), (6, 0x30)] {
            let o = Command::new(&exe).arg(format!("{base:x}")).arg(format!("{r0:x}")).output().unwrap();
            assert!(o.status.success(), "base {base:x} r0 {r0}: {}", String::from_utf8_lossy(&o.stdout));
            let r: Vec<u32> = String::from_utf8(o.stdout).unwrap().lines().map(|l| u32::from_str_radix(l, 16).unwrap()).collect();
            let want = [
                r0 + 0x101,     // r0: + import + helper
                base + 0x80,    // r1: relocated literal, loaded at run time
                0x1234_5678,    // r2: through it
                r0 & 3,         // r3
                0,              // r4: pushed and popped
                base + 0x5C,    // r5: add r5, pc, #0
                base + 0x50,    // r6: return address of bl thunk
                0xFEED_FACE,    // r7: a literal not in `patched`, still loaded from memory
                base + 0x50,    // r8: r14 seen by the code.bin function
                base + 0x50,    // r9: its ret_lr
            ];
            assert_eq!(&r[..10], &want, "base {base:x} r0 {r0}");
            assert_eq!((r[10], r[13], r[14]), (case, 0x8000, base + 0x60), "jump table case (absolute relocated targets); sp; lr from bl helper");
        }
    }
}

/// Module mode, Thumb: the decoder folds PC reads (`adr`, hi-register ops with Rm = PC) into constants of the
/// decode offset; module code must read them as `mb + offset` instead.
#[test]
fn module_thumb_pc_reads_are_base_relative() {
    let code = bytes(&th(&[
        0xA001, // 0 adr r0, #4   (Align(0 + 4, 4) + 4 = 8)
        0x4679, // 2 mov r1, pc   (2 + 4)
        0x4770, // 4 bx lr
    ]));
    let all = [FuncEntry { start: 0, end: 6, mode: Mode::Thumb }];
    let c = gen_function(&code, 0, &all[0], &all, &GenOptions { module: Some(1), ..Default::default() }).c_source;
    assert!(c.contains("void m1_t_000000(XrCpu* c)"), "{c}");
    assert!(c.contains("uint32_t a = (mb + 0x4u), b = 0x4u") && c.contains("uint32_t b = (mb + 0x6u)"), "{c}");
}
