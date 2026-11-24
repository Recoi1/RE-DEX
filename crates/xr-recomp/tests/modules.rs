//! CRO modules in xgame.dll (`xr_recomp::dll`): a tiny code.bin and two modules generated, compiled with the real
//! `lookup.c`, and run through `xr_place_module`/`xr_unplace_module`/`xr_lookup` (needs clang, from env.ps1).
//! A module placed at a second base still runs right; code.bin calls into a module through
//! a function pointer and the module calls code.bin through its import thunk, both natively (the interpreter
//! callback fails the run); a module unloaded and another placed at the same address never yields the old one's
//! functions. Also: a code.bin helper that returns flags to a module caller stores them (thunk flag demands).
use std::process::Command;
use xr_codegen::{flag_table_with, gen_function, GenOptions};
use xr_formats::{FuncEntry, Mode};
use xr_recomp::dll::{lookup_c, module_flags, module_sources, thunk_flag_demands, Module};

fn bytes(words: &[u32]) -> Vec<u8> { words.iter().flat_map(|w| w.to_le_bytes()).collect() }
fn arm(start: u32, end: u32) -> FuncEntry { FuncEntry { start, end, mode: Mode::Arm } }
fn c_bytes(b: &[u8]) -> String { b.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",") }

#[test]
fn modules_place_lookup_and_call_natively() {
    let base = 0x0010_0000;
    let exe_words = [
        0xE3500005, // 100000 cmp r0, #5          (a helper returning Z to its caller)
        0xE12FFF1E, // 100004 bx lr
        0xE92D4010, // 100008 push {r4, lr}       (calls a module function through a pointer at 0x200000)
        0xE59F4008, // 10000C ldr r4, [pc, #8]
        0xE5944000, // 100010 ldr r4, [r4]
        0xE12FFF34, // 100014 blx r4
        0xE8BD8010, // 100018 pop {r4, pc}
        0x00200000, // 10001C
    ];
    let a_words = [
        0xE51FF004, // 00 thunk: ldr pc, [pc, #-4]
        0x00000000, // 04 import word, patched to 0x100000 when placed
        0xE92D4010, // 08 push {r4, lr}
        0xEBFFFFFB, // 0C bl 00 (thunk)
        0x03A01001, // 10 moveq r1, #1
        0x13A01002, // 14 movne r1, #2
        0xE28F2000, // 18 add r2, pc, #0     (base + 0x20)
        0xE8BD8010, // 1C pop {r4, pc}
    ];
    let b_words = [0xE3A01077, 0xE12FFF1E]; // mov r1, #0x77; bx lr
    let (exe, a, b) = (bytes(&exe_words), bytes(&a_words), bytes(&b_words));
    let mods = [
        Module { name: "DllA".into(), code: &a, funcs: vec![arm(0, 8), arm(8, 0x20)], thunks: vec![(0, 0x100000)] },
        Module { name: "DllB".into(), code: &b, funcs: vec![arm(0, 8)], thunks: vec![] },
    ];
    let flags: Vec<_> = mods.iter().map(module_flags).collect();
    let demands = thunk_flag_demands(&mods, &flags);
    assert_eq!(demands, vec![(0x100000, Mode::Arm, 4)], "fA reads Z after calling the helper through its thunk");
    let funcs = [arm(0x100000, 0x100008), arm(0x100008, 0x100020)];
    let opt = GenOptions { flags: flag_table_with(&exe, base, &funcs, &[], &demands), ..Default::default() };
    let mut src = String::from("#include \"xr_abi.h\"\n");
    src += &funcs.iter().map(|f| gen_function(&exe, base, f, &funcs, &opt).c_source + "\n").collect::<String>();
    for (k, (m, f)) in mods.iter().zip(flags).enumerate() {
        src += &module_sources(k as u32, m, f, 1).0.concat();
    }
    src += &lookup_c(base, base + 0x20, &funcs, "", &mods);
    src += &format!(
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static void st(uint32_t a, uint32_t v) {{ memcpy(xr_mem + a, &v, 4); }}
static void svc_cb(XrCpu* c, uint32_t imm) {{ (void)c; (void)imm; }}
static void interp_cb(XrCpu* c, uint32_t s, uint32_t u, uint32_t sp) {{ (void)c; printf("interp %08x %08x %08x\n", s, u, sp); exit(3); }}
static void preempt_cb(XrCpu* c) {{ c->budget = 1 << 30; }}
static const uint8_t exe[] = {{{exe}}}, a[] = {{{a}}}, b[] = {{{b}}};
static XrCpu cpu;
static void call(XrFn f, uint32_t r0) {{
    if (!f) {{ printf("null\n"); exit(4); }}
    memset(&cpu, 0, sizeof cpu);
    cpu.r[0] = r0; cpu.r[13] = 0x8000; cpu.r[14] = 0xDEAD0000u; cpu.budget = 1 << 30; cpu.ret_lr = cpu.r[14]; cpu.ret_sp = cpu.r[13];
    f(&cpu);
}}
static void place_a(uint32_t at) {{ memcpy(xr_mem + at, a, sizeof a); st(at + 4, 0x100000u); printf("placeA %u\n", xr_place_module(0, at)); }}
int main(void) {{
    static XrRuntime rt; rt.abi_version = XR_ABI_VERSION; rt.svc = svc_cb; rt.interp = interp_cb; rt.preempt = preempt_cb;
    rt.mem = calloc(1, 0x1000000);
    printf("init %u\n", xr_game_init(&rt));
    memcpy(xr_mem + 0x100000, exe, sizeof exe);
    printf("count %u %s %s\n", xr_module_count, xr_module_names[0], xr_module_names[1]);
    printf("unplaced %d\n", xr_lookup(0x300188u) == 0);
    uint32_t b1 = 0x300180u, b2 = 0xC45000u;
    place_a(b1);
    for (uint32_t r0 = 5; r0 <= 6; r0++) {{ call(xr_lookup(b1 + 8), r0); printf("a1 %u %08x %08x\n", r0, cpu.r[1], cpu.r[2]); }}
    st(0x200000, b1 + 8);
    call(xr_lookup(0x100008), 5); printf("ptr %08x\n", cpu.r[1]);
    printf("thumbbit %d\n", xr_lookup((b1 + 8) | 1) == 0);
    /* a second session: the same module at another base */
    xr_unplace_module(0);
    printf("gone %d\n", xr_lookup(b1 + 8) == 0);
    place_a(b2);
    for (uint32_t r0 = 5; r0 <= 6; r0++) {{ call(xr_lookup(b2 + 8), r0); printf("a2 %u %08x %08x\n", r0, cpu.r[1], cpu.r[2]); }}
    /* unloaded, and another module at the same address */
    xr_unplace_module(0);
    memcpy(xr_mem + b2, b, sizeof b);
    printf("placeB %u\n", xr_place_module(1, b2));
    printf("b_start %d b_old %d\n", xr_lookup(b2) != 0, xr_lookup(b2 + 8) == 0);
    call(xr_lookup(b2), 0); printf("b %08x\n", cpu.r[1]);
    /* placed over a module nobody unplaced: the old one's functions go */
    place_a(b1);
    memcpy(xr_mem + b1, b, sizeof b);
    printf("over %u %d\n", xr_place_module(1, b1), xr_lookup(b1 + 8) == 0);
    printf("refused %u %u\n", xr_place_module(0, b2 + 2), xr_place_module(2, b2));
    return 0;
}}
"#,
        exe = c_bytes(&exe),
        a = c_bytes(&a),
        b = c_bytes(&b)
    );
    let dir = std::env::temp_dir().join(format!("xr_recomp_modules_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.c"), &src).unwrap();
    let abi = concat!(env!("CARGO_MANIFEST_DIR"), "/../../abi");
    let o = match Command::new("clang").args(["-O2", "-fno-strict-aliasing", "-fwrapv", "-I", abi]).arg(dir.join("t.c")).arg("-o").arg(dir.join("t.exe")).output() {
        Err(e) => return println!("skipping: clang not found ({e})"),
        Ok(o) => o,
    };
    assert!(o.status.success(), "clang: {}\n{src}", String::from_utf8_lossy(&o.stderr));
    let r = Command::new(dir.join("t.exe")).output().unwrap();
    let out = String::from_utf8_lossy(&r.stdout).into_owned();
    assert!(r.status.success(), "{out}");
    let want = "init 6\ncount 2 DllA DllB\nunplaced 1\nplaceA 1\n\
a1 5 00000001 003001a0\na1 6 00000002 003001a0\nptr 00000001\nthumbbit 1\ngone 1\nplaceA 1\n\
a2 5 00000001 00c45020\na2 6 00000002 00c45020\nplaceB 1\nb_start 1 b_old 1\nb 00000077\nplaceA 1\nover 1 1\nrefused 0 0\n";
    assert_eq!(out.replace("\r\n", "\n"), want);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Flags returned into CRO code: a code.bin caller reading flags after a static.crs stub, and a module caller
/// reading them after a thunk to another module's function, are unwired edges until the target module's table
/// gets `into_cro_demands`; then none is left. Without the flag reads there is nothing to wire.
#[test]
fn flag_edges_into_cro_code() {
    use xr_codegen::flag_table;
    use xr_recomp::dll::{into_cro_demands, module_flags_with, unwired_flag_edges, IntoCro};
    let base = 0x0010_0000;
    let caller = |read: u32| {
        bytes(&[
            0xE51FF004, // 00 stub/thunk: ldr pc, [pc, #-4]
            0x00000000, // 04 the patched word
            0xE92D4010, // 08 push {r4, lr}
            0xEBFFFFFB, // 0C bl 00
            read,       // 10 moveq r1, #1 (reads Z) or mov r1, #1
            0xE8BD8010, // 14 pop {r4, pc}
        ])
    };
    let target = bytes(&[0xE3500005, 0xE12FFF1E]); // cmp r0, #5; bx lr
    for (read, want) in [(0x03A01001u32, 2usize), (0xE3A01001, 0)] {
        let exe = caller(read);
        let funcs = [arm(base, base + 8), arm(base + 8, base + 0x18)];
        let exe_flags = flag_table(&exe, base, &funcs, &[]);
        let a = caller(read);
        let mods = [
            Module { name: "DllA".into(), code: &a, funcs: vec![arm(0, 8), arm(8, 0x18)], thunks: vec![] },
            Module { name: "DllB".into(), code: &target, funcs: vec![arm(0, 8)], thunks: vec![] },
        ];
        let edges = [IntoCro { from: None, stub: base, to: Some((1, 0)) }, IntoCro { from: Some(0), stub: 0, to: Some((1, 0)) }];
        let mut mflags: Vec<_> = mods.iter().map(module_flags).collect();
        let before = unwired_flag_edges(&edges, &mods, &exe_flags, &mflags);
        assert_eq!(before.len(), want, "{before:?}");
        if want > 0 {
            assert!(before[0].starts_with("code.bin stub 0x00100000 -> DllB+0x0") && before[1].starts_with("DllA thunk +0x0 -> DllB+0x0"), "{before:?}");
            assert!(before.iter().all(|e| e.ends_with("wire extra_ret for this edge")));
        }
        let extra = into_cro_demands(&edges, &exe_flags, &mflags);
        mflags = mods.iter().zip(&extra).map(|(m, x)| module_flags_with(m, x)).collect();
        assert!(unwired_flag_edges(&edges, &mods, &exe_flags, &mflags).is_empty());
    }
}
