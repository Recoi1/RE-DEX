//! Runs `redex` against tiny C test DLLs (clang from env.ps1) and a synthetic .cxi whose code page holds real ARM
//! code for the interpreter.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PRELUDE: &str = r#"#include "xr_abi.h"
uint8_t* xr_mem; const XrRuntime* xr_rt;
__declspec(dllexport) uint32_t xr_game_init(const XrRuntime* rt) { xr_rt = rt; xr_mem = rt->mem; return XR_ABI_VERSION; }
/* no CRO modules */
__declspec(dllexport) const uint32_t xr_module_count = 0;
__declspec(dllexport) const char* const xr_module_names[1] = {0};
__declspec(dllexport) const uint64_t xr_module_hash[1] = {0};
__declspec(dllexport) uint32_t xr_place_module(uint32_t k, uint32_t base) { (void)k; (void)base; return 0; }
__declspec(dllexport) void xr_unplace_module(uint32_t k) { (void)k; }
static void svc(XrCpu* c, uint32_t n) { c->r[15] = 0x100000; xr_rt->svc(c, n); }
static volatile uint32_t* word(uint32_t a) { return (volatile uint32_t*)(xr_mem + a); }
/* Log "<tag>=<8 hex digits of v>" through OutputDebugString (buffer in the main stack's bottom page). */
static void say(XrCpu* c, const char* tag, uint32_t v) {
    char* p = (char*)xr_mem + 0x0FFFC800; int n = 0;
    while (*tag) p[n++] = *tag++;
    p[n++] = '=';
    for (int i = 28; i >= 0; i -= 4) p[n++] = "0123456789ABCDEF"[(v >> i) & 15];
    uint32_t r0 = c->r[0], r1 = c->r[1];
    c->r[0] = 0x0FFFC800; c->r[1] = n; svc(c, 0x3D);
    c->r[0] = r0; c->r[1] = r1;
}
"#;

/// A free word for flags: the bottom of the main stack (0x0FFFC000..0x10000000), far below what anything uses.
const FLAG: &str = "0x0FFFC000";

struct Run {
    code: i32,
    log: String,
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    out[at..at + bytes.len()].copy_from_slice(bytes);
}

/// A decrypted .cxi: NCCH header, exheader (text at 0x100000, one page, priority 0x30), ExeFS with `.code`.
fn cxi(code: &[u8]) -> Vec<u8> {
    let mut out = vec![0; 0x800];
    put(&mut out, 0x100, b"NCCH");
    put(&mut out, 0x118, &0x0004_0000_0FF3_DE02u64.to_le_bytes());
    out[0x18F] = 0x04;
    put(&mut out, 0x1A0, &3u32.to_le_bytes());
    put(&mut out, 0x210, &[0x0010_0000u32, 1, code.len() as u32].map(u32::to_le_bytes).concat());
    out[0x200 + 0x20F] = 0x30;
    put(&mut out, 0x600, b".code");
    put(&mut out, 0x60C, &(code.len() as u32).to_le_bytes());
    out.extend_from_slice(code);
    out
}

/// Build the DLL from PRELUDE + `c`, lay out `<tmp>/build/x` and the .cxi (code page = `words` at their addresses,
/// zero elsewhere), run redex; the log lands in `<tmp>/logs/latest.log`.
fn run(name: &str, c: &str, words: &[(u32, u32)], extra: &[&str]) -> Run {
    let tmp = std::env::temp_dir().join(format!("redex_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let build = tmp.join("build/x");
    std::fs::create_dir_all(&build).unwrap();
    let mut code = vec![0u8; 0x1000];
    for &(a, w) in words {
        put(&mut code, (a - 0x100000) as usize, &w.to_le_bytes());
    }
    // the dump check: a DLL made from this code.bin, unless the test brings its own xr_game_info
    let info = if c.contains("xr_game_info") { String::new() } else { game_info(&code, 0x0004_0000_0FF3_DE02, NO_HOOKS) };

    let src = tmp.join("t.c");
    std::fs::write(&src, format!("{PRELUDE}\n{info}\n{c}\n")).unwrap();
    let abi = concat!(env!("CARGO_MANIFEST_DIR"), "/../../abi");
    let o = Command::new("clang").args(["-O1", "-shared", "-I", abi]).arg(&src).arg("-o").arg(build.join("xgame.dll")).output().unwrap();
    assert!(o.status.success(), "clang: {}", String::from_utf8_lossy(&o.stderr));
    let rom = tmp.join("t.cxi");
    std::fs::write(&rom, cxi(&code)).unwrap();

    // stderr goes to a file, not a pipe nobody reads while we wait
    let err = tmp.join("stderr.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&rom).arg("--build").arg(&build).args(["--headless", "--frames", "600"]).args(extra)
        .stdout(Stdio::null()).stderr(std::fs::File::create(&err).unwrap())
        .spawn().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let log = std::fs::read_to_string(tmp.join("logs/latest.log")).unwrap_or_default();
    // stderr has the same lines, plus anything said before the log file opened
    let log = if log.is_empty() { std::fs::read_to_string(&err).unwrap_or_default() } else { log };
    let Some(status) = status else { panic!("{name}: redex still running after 60 s, killed\n{log}") };
    Run { code: status.code().unwrap_or(-1), log }
}

/// `XrGameInfo.hooks_hash` of a DLL that hooks nothing (FNV-1a 64 of no bytes).
const NO_HOOKS: u64 = 0xCBF2_9CE4_8422_2325;

/// An `xr_game_info` naming `code` (the .cxi's code.bin), `title` and the hook list's hash.
fn game_info(code: &[u8], title: u64, hooks: u64) -> String {
    use sha2::Digest;
    let sha: Vec<String> = sha2::Sha256::digest(code).iter().map(|b| b.to_string()).collect();
    format!("__declspec(dllexport) const XrGameInfo xr_game_info = {{{{{}}}, 0x{title:X}ull, \"test\", 0x{hooks:X}ull}};", sha.join(","))
}

/// xgame.dll must come from the dump being run: one made from other code, or for another title, or from before
/// the check, or with another hook list, is refused with a clear message; the matching one boots.
#[test]
fn dump_check() {
    let ok = run("dump_ok", &format!("{ENTRY_ONLY}\n{TWO_TICKS}"), &[], &[]);
    assert_eq!(ok.code, 0, "{}", ok.log);
    assert!(ok.log.contains("xgame.dll: build test, made from this dump's code.bin"), "{}", ok.log);
    let other = game_info(&[1; 0x1000], 0x0004_0000_0FF3_DE02, NO_HOOKS);
    let wrong_title = game_info(&[0; 0x1000], 0x0004_0000_0005_5D00, NO_HOOKS);
    let other_hooks = game_info(&[0; 0x1000], 0x0004_0000_0FF3_DE02, 0x1234);
    for (name, info, says) in [
        ("dump_code", other.as_str(), "was made from a different dump"),
        ("dump_title", wrong_title.as_str(), "title 0004000000055D00"),
        ("dump_none", "/* xr_game_info missing */", "has no xr_game_info"),
        ("dump_hooks", other_hooks.as_str(), "another hook list (hooks.txt, hash 0000000000001234)"),
    ] {
        let r = run(name, &format!("{info}\n{ENTRY_ONLY}\n{TWO_TICKS}"), &[], &[]);
        assert_eq!(r.code, 1, "{name}: {}", r.log);
        assert!(r.log.contains(says) && r.log.contains("rebuild it with xr-recomp"), "{name}: {}", r.log);
        assert!(said(&r.log, "tick").is_empty(), "{name}: the game ran\n{}", r.log);
    }
}

fn said(log: &str, tag: &str) -> Vec<u32> {
    log.lines()
        .filter_map(|l| l.split(&format!("[guest] {tag}=")).nth(1))
        .map(|h| u32::from_str_radix(h.trim(), 16).unwrap())
        .collect()
}

/// Only the entry is native.
const ENTRY_ONLY: &str = "static void entry(XrCpu* c);\n__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return a == 0x100000 ? entry : 0; }";

const TWO_TICKS: &str = "static void entry(XrCpu* c) { svc(c, 0x28); say(c, \"tick\", c->r[0]); svc(c, 0x28); say(c, \"tick\", c->r[0]); }";

#[test]
fn bridge_get_system_tick() {
    let r = run("tick", &format!("{ENTRY_ONLY}\n{TWO_TICKS}"), &[], &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    let t = said(&r.log, "tick");
    assert_eq!(t.len(), 2, "{}", r.log);
    assert!(t[1] > t[0], "{t:?}");
}

#[test]
fn virtual_clock_is_deterministic() {
    let a = run("fixed_a", &format!("{ENTRY_ONLY}\n{TWO_TICKS}"), &[], &[]);
    let b = run("fixed_b", &format!("{ENTRY_ONLY}\n{TWO_TICKS}"), &[], &[]);
    assert_eq!(said(&a.log, "tick").len(), 2, "{}", a.log);
    assert_eq!(said(&a.log, "tick"), said(&b.log, "tick"));
}

#[test]
fn create_signal_wait_event() {
    let body = r#"static void entry(XrCpu* c) {
    c->r[1] = 0; svc(c, 0x17); uint32_t h = c->r[1]; say(c, "create", c->r[0]);
    c->r[0] = h; svc(c, 0x18); say(c, "signal", c->r[0]);
    c->r[0] = h; c->r[1] = 0; c->r[2] = 0xFFFFFFFF; c->r[3] = 0xFFFFFFFF; svc(c, 0x24); say(c, "wait", c->r[0]);
    c->r[0] = h; c->r[1] = 0; c->r[2] = 0; c->r[3] = 0; svc(c, 0x24); say(c, "poll", c->r[0]);
    c->r[0] = h; svc(c, 0x23); say(c, "close", c->r[0]);
}"#;
    let r = run("event", &format!("{ENTRY_ONLY}\n{body}"), &[], &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    for t in ["create", "signal", "wait", "close"] {
        assert_eq!(said(&r.log, t), vec![0], "{t}: {}", r.log);
    }
    assert_eq!(said(&r.log, "poll"), vec![0x0940_1BFE], "timeout 0 on a cleared one-shot event: {}", r.log);
}

/// main creates a native thread that sets a flag and returns (THREAD_EXIT_MAGIC); main waits on its handle.
#[test]
fn second_thread_runs() {
    let c = format!(
        r#"static void entry(XrCpu* c);
static void worker(XrCpu* c) {{ say(c, "ret_lr", c->ret_lr); say(c, "ret_sp", c->ret_sp == c->r[13]); *word({FLAG}) = 0x1234 + c->r[0]; }}
__declspec(dllexport) XrFn xr_lookup(uint32_t a) {{ return a == 0x100000 ? entry : a == 0x100010 ? worker : 0; }}
static void entry(XrCpu* c) {{
    c->r[0] = 0x30; c->r[1] = 0x100010; c->r[2] = 1; c->r[3] = 0x0FFFD000; c->r[4] = 0xFFFFFFFE; svc(c, 0x08);
    say(c, "create", c->r[0]); uint32_t h = c->r[1];
    c->r[0] = h; c->r[2] = 0xFFFFFFFF; c->r[3] = 0xFFFFFFFF; svc(c, 0x24); say(c, "wait", c->r[0]);
    say(c, "flag", *word({FLAG}));
}}"#
    );
    let r = run("thread", &c, &[], &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    assert_eq!(said(&r.log, "create"), vec![0], "{}", r.log);
    assert_eq!(said(&r.log, "wait"), vec![0], "{}", r.log);
    assert_eq!(said(&r.log, "flag"), vec![0x1235], "{}", r.log);
    assert_eq!(said(&r.log, "ret_lr"), vec![0xFFFF_FFF0], "thread start returns to THREAD_EXIT_MAGIC: {}", r.log);
    assert_eq!(said(&r.log, "ret_sp"), vec![1], "{}", r.log);
}

/// main spins on a flag with no SVC (only back-edge budget checks); a more urgent thread sets it after a sleep.
#[test]
fn spinner_does_not_hang() {
    let c = format!(
        r#"static void entry(XrCpu* c);
static void worker(XrCpu* c) {{ c->r[0] = 2000000; c->r[1] = 0; svc(c, 0x0A); *word({FLAG}) = 1; }}
__declspec(dllexport) XrFn xr_lookup(uint32_t a) {{ return a == 0x100000 ? entry : a == 0x100010 ? worker : 0; }}
static void entry(XrCpu* c) {{
    c->r[0] = 0x2F; c->r[1] = 0x100010; c->r[2] = 0; c->r[3] = 0x0FFFD000; c->r[4] = 0xFFFFFFFE; svc(c, 0x08);
    uint32_t n = 0;
    while (!*word({FLAG})) {{ n++; if (__builtin_expect(--c->budget <= 0, 0)) xr_rt->preempt(c); }}
    say(c, "spun", n != 0);
}}"#
    );
    let r = run("spinner", &c, &[], &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    assert_eq!(said(&r.log, "spun"), vec![1], "{}", r.log);
}

/// Calls into functions the DLL lacks run in the interpreter from the code bytes: one plain, one that calls back
/// into a native function (whose guest bytes would give a different answer if interpreted).
#[test]
fn interp_runs_missing_function() {
    let c = r#"static void entry(XrCpu* c);
static void native41(XrCpu* c) { say(c, "ret_lr", c->ret_lr); say(c, "ret_sp", c->ret_sp == c->r[13]); c->r[0] = 41; }
__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return a == 0x100000 ? entry : a == 0x100300 ? native41 : 0; }
static void entry(XrCpu* c) {
    c->r[0] = 2; c->r[1] = 3; c->r[14] = 0x100008; xr_rt->interp(c, 0x100100, 0x100008, c->r[13]); say(c, "add", c->r[0]);
    uint32_t sp = c->r[13];
    c->r[14] = 0x10000C; xr_rt->interp(c, 0x100200, 0x10000C, c->r[13]); say(c, "nested", c->r[0]);
    say(c, "sp", c->r[13] == sp);
}"#;
    let words = [
        (0x100100, 0xE080_0001), // add r0, r0, r1
        (0x100104, 0xE12F_FF1E), // bx lr
        (0x100200, 0xE52D_E004), // push {lr}
        (0x100204, 0xEB00_003D), // bl 0x100300
        (0x100208, 0xE280_0001), // add r0, r0, #1
        (0x10020C, 0xE49D_F004), // pop {pc}
        (0x100300, 0xE3A0_0063), // mov r0, #99 (native code says 41)
        (0x100304, 0xE12F_FF1E), // bx lr
    ];
    let r = run("interp", c, &words, &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    assert_eq!(said(&r.log, "add"), vec![5], "{}", r.log);
    assert_eq!(said(&r.log, "nested"), vec![42], "{}", r.log);
    assert_eq!(said(&r.log, "sp"), vec![1], "{}", r.log);
    assert_eq!(said(&r.log, "ret_lr"), vec![0x100208], "the bl's return address: {}", r.log);
    assert_eq!(said(&r.log, "ret_sp"), vec![1], "{}", r.log);
    assert!(r.log.contains("interp: first entry at 0x00100100"), "{}", r.log);
}

/// A call-shaped interp entry (lr and sp are the frame's return) at a function the DLL has runs it natively (a
/// static->CRO call the interpreter reaches): the guest bytes there are NOPs up to the return, native code says 41.
/// A native function that hands its own entry straight back (a miss stub) is interpreted, not bounced forever.
#[test]
fn interp_call_shaped_entry_goes_native() {
    let c = r#"static void entry(XrCpu* c);
static void native41(XrCpu* c) { say(c, "native", c->ret_lr); c->r[0] = 41; }
static void stub(XrCpu* c) { say(c, "stub", c->r[14]); xr_rt->interp(c, 0x100400, c->r[14], c->r[13]); }
__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return a == 0x100000 ? entry : a == 0x100300 ? native41 : a == 0x100400 ? stub : 0; }
static void entry(XrCpu* c) {
    c->r[0] = 0; c->r[14] = 0x100F00; xr_rt->interp(c, 0x100300, 0x100F00, c->r[13]); say(c, "r0", c->r[0]);
    c->r[0] = 7; c->r[14] = 0x100F00; xr_rt->interp(c, 0x100400, 0x100F00, c->r[13]); say(c, "r0", c->r[0]);
    c->r[14] = 0x100008; xr_rt->interp(c, 0x100300, 0x100F00, c->r[13]); say(c, "r0", c->r[0]);
}"#;
    let r = run("call_shaped", c, &[], &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    // 1: native (r0 41); 2: the stub runs once, then its own bounce is interpreted (NOPs keep r0 = 7);
    // 3: lr is not until_pc (not a call), so interpreted NOPs keep r0 = 7 (native code would say 41)
    assert_eq!(said(&r.log, "native"), vec![0x100F00], "{}", r.log);
    assert_eq!(said(&r.log, "stub"), vec![0x100F00], "{}", r.log);
    assert_eq!(said(&r.log, "r0"), vec![41, 7, 7], "{}", r.log);
}

/// until_pc is a return address the interpreted callee's own recursive calls also return to, at a deeper sp: those
/// returns are not the stop. R(n): push {lr}; subs r0,#1; blne R; add r1,#1; pop {pc}, entered as if called from its
/// own `blne`. Stopping at pc alone would give r1 = 1.
#[test]
fn interp_until_pc_needs_its_sp() {
    let c = r#"static void entry(XrCpu* c);
__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return a == 0x100000 ? entry : 0; }
static void entry(XrCpu* c) {
    uint32_t sp = c->r[13];
    c->r[0] = 3; c->r[1] = 0; c->r[14] = 0x10060C; xr_rt->interp(c, 0x100600, 0x10060C, sp);
    say(c, "depth", c->r[1]); say(c, "sp", c->r[13] == sp);
}"#;
    let words = [
        (0x100600, 0xE52D_E004), // push {lr}
        (0x100604, 0xE250_0001), // subs r0, r0, #1
        (0x100608, 0x1BFF_FFFC), // blne 0x100600
        (0x10060C, 0xE281_1001), // add r1, r1, #1
        (0x100610, 0xE49D_F004), // pop {pc}
    ];
    let r = run("until_sp", c, &words, &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    assert_eq!(said(&r.log, "depth"), vec![3], "{}", r.log);
    assert_eq!(said(&r.log, "sp"), vec![1], "{}", r.log);
}

/// A plain `b` into a native function's entry (a tail call, or a jump into a split-function piece) carries no
/// return, so it stays interpreted: the guest bytes say 99, native code would say 41.
#[test]
fn interp_branch_into_native_entry_stays_interpreted() {
    let c = r#"static void entry(XrCpu* c);
static void native41(XrCpu* c) { c->r[0] = 41; }
__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return a == 0x100000 ? entry : a == 0x100300 ? native41 : 0; }
static void entry(XrCpu* c) {
    c->r[14] = 0x100008; xr_rt->interp(c, 0x100700, 0x100008, c->r[13]); say(c, "r0", c->r[0]);
}"#;
    let words = [
        (0x100300, 0xE3A0_0063), // mov r0, #99 (native code says 41)
        (0x100304, 0xE12F_FF1E), // bx lr
        (0x100700, 0xEAFF_FEFE), // b 0x100300
    ];
    let r = run("plain_b", c, &words, &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    assert_eq!(said(&r.log, "r0"), vec![99], "{}", r.log);
}

/// A Thumb function in the interpreter calls an ARM native function with a 32-bit BLX imm and returns to ARM.
#[test]
fn interp_thumb_function_calls_native() {
    let c = r#"static void entry(XrCpu* c);
static void native41(XrCpu* c) { say(c, "ret_lr", c->ret_lr); c->r[0] = 41; }
__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return a == 0x100000 ? entry : a == 0x100300 ? native41 : 0; }
static void entry(XrCpu* c) {
    uint32_t sp = c->r[13];
    c->r[14] = 0x100008; xr_rt->interp(c, 0x100801, 0x100008, sp); say(c, "r0", c->r[0]); say(c, "sp", c->r[13] == sp);
    say(c, "thumb", (c->cpsr >> 5) & 1);
}"#;
    let words = [
        (0x100300, 0xE3A0_0063), // mov r0, #99 (native code says 41)
        (0x100304, 0xE12F_FF1E), // bx lr
        (0x100800, 0xF7FF_B500), // push {lr}; blx 0x100300 (first half)
        (0x100804, 0x3001_ED7E), // (second half); adds r0, #1
        (0x100808, 0x0000_BD00), // pop {pc} (to ARM 0x100008)
    ];
    let r = run("thumb", c, &words, &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    assert_eq!(said(&r.log, "ret_lr"), vec![0x100807], "{}", r.log);
    assert_eq!(said(&r.log, "r0"), vec![42], "{}", r.log);
    assert_eq!(said(&r.log, "sp"), vec![1], "{}", r.log);
    assert_eq!(said(&r.log, "thumb"), vec![0], "{}", r.log);
}

/// An SVC in interpreted code goes to xr-hle like one from native code: GetSystemTick moves on.
#[test]
fn interp_svc() {
    let c = r#"static void entry(XrCpu* c);
__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return a == 0x100000 ? entry : 0; }
static void entry(XrCpu* c) {
    svc(c, 0x28); uint64_t t0 = c->r[0] | (uint64_t)c->r[1] << 32;
    c->r[0] = c->r[1] = 0; c->r[14] = 0x100008; xr_rt->interp(c, 0x100900, 0x100008, c->r[13]);
    uint64_t t1 = c->r[0] | (uint64_t)c->r[1] << 32;
    say(c, "later", t1 > t0);
}"#;
    let words = [
        (0x100900, 0xEF00_0028), // svc 0x28 (GetSystemTick)
        (0x100904, 0xE12F_FF1E), // bx lr
    ];
    let r = run("interp_svc", c, &words, &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    assert_eq!(said(&r.log, "later"), vec![1], "{}", r.log);
}

/// --deterministic charges interpreted code like native code (no cycle per instruction, 64 per SVC), so the clock
/// does not depend on what is compiled; without it the interpreter charges 1 per instruction.
#[test]
fn deterministic_runs_charge_interpreted_code_like_native() {
    let c = r#"static void entry(XrCpu* c);
__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return a == 0x100000 ? entry : 0; }
static void entry(XrCpu* c) {
    svc(c, 0x28); uint32_t t0 = c->r[0];
    c->r[14] = 0x100008; xr_rt->interp(c, 0x100B00, 0x100008, c->r[13]);
    say(c, "dt", c->r[0] - t0);
}"#;
    // 100 NOPs (straight-line: no back-edge to pay), then GetSystemTick
    let mut words: Vec<(u32, u32)> = (0..100).map(|i| (0x100B00 + 4 * i, 0xE1A0_0000)).collect();
    words.extend([(0x100C90, 0xEF00_0028), (0x100C94, 0xE12F_FF1E)]);
    let r = run("clock_det", c, &words, &["--deterministic"]);
    assert_eq!(r.code, 0, "{}", r.log);
    // the SVC's charge (64) and GetSystemTick's own cost (150, xr-hle SYSTEM_TICK_COST); nothing for the NOPs
    assert_eq!(said(&r.log, "dt"), vec![64 + 150], "{}", r.log);
    assert!(r.log.contains("clock: native charging model"), "{}", r.log);
    let r = run("clock_interp", c, &words, &[]);
    assert!(said(&r.log, "dt").first().is_some_and(|&dt| dt >= 150 + 100), "1 per instruction: {}", r.log);
}

/// --record-expects leaves the script alone (exit 4) when the run ends before a checkpoint: here the game exits at
/// once, long before frame 500.
#[test]
fn record_expects_needs_every_checkpoint() {
    let script = std::env::temp_dir().join(format!("redex_record_partial_{}.txt", std::process::id()));
    let text = "500 checkpoint late\nexpect late 0123\n";
    std::fs::write(&script, text).unwrap();
    let r = run("record_partial", &format!("{ENTRY_ONLY}\n{TWO_TICKS}"), &[], &["--script", script.to_str().unwrap(), "--record-expects"]);
    assert_eq!(r.code, 4, "{}", r.log);
    assert!(r.log.contains("before checkpoint late, so the script is left as it was"), "{}", r.log);
    assert_eq!(std::fs::read_to_string(&script).unwrap(), text);
    let _ = std::fs::remove_file(&script);
}

/// A window would ignore a script's checkpoints, expects and autobattle: --script without --headless is refused.
#[test]
fn script_needs_headless() {
    let script = std::env::temp_dir().join(format!("redex_script_windowed_{}.txt", std::process::id()));
    std::fs::write(&script, "10 checkpoint a\n").unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_redex")).arg("--script").arg(&script).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--script needs --headless"), "{}", String::from_utf8_lossy(&o.stderr));
    let _ = std::fs::remove_file(&script);
}

/// A thread whose entry the DLL lacks runs interpreted from its first instruction to THREAD_EXIT_MAGIC.
#[test]
fn interp_thread_start() {
    let c = format!(
        r#"static void entry(XrCpu* c);
__declspec(dllexport) XrFn xr_lookup(uint32_t a) {{ return a == 0x100000 ? entry : 0; }}
static void entry(XrCpu* c) {{
    c->r[0] = 0x30; c->r[1] = 0x100A00; c->r[2] = 0x5678; c->r[3] = 0x0FFFD000; c->r[4] = 0xFFFFFFFE; svc(c, 0x08);
    say(c, "create", c->r[0]); uint32_t h = c->r[1];
    c->r[0] = h; c->r[2] = 0xFFFFFFFF; c->r[3] = 0xFFFFFFFF; svc(c, 0x24); say(c, "wait", c->r[0]);
    say(c, "flag", *word({FLAG}));
}}"#
    );
    let words = [
        (0x100A00, 0xE59F_1004), // ldr r1, [pc, #4] (FLAG)
        (0x100A04, 0xE581_0000), // str r0, [r1] (the thread argument)
        (0x100A08, 0xE12F_FF1E), // bx lr (THREAD_EXIT_MAGIC)
        (0x100A0C, 0x0FFF_C000),
    ];
    let r = run("interp_thread", &c, &words, &[]);
    assert_eq!(r.code, 0, "{}", r.log);
    assert_eq!(said(&r.log, "create"), vec![0], "{}", r.log);
    assert_eq!(said(&r.log, "wait"), vec![0], "{}", r.log);
    assert_eq!(said(&r.log, "flag"), vec![0x5678], "{}", r.log);
    assert!(r.log.contains("starts at 0x00100A00 (interpreted)"), "{}", r.log);
}

/// The real game to its first GPU command list (M1): needs XR_CCI and XR_DATA (with build/x-usa-v0.2.0/xgame.dll),
/// skips without them. Runs in a data dir of its own (own_data).
#[test]
fn boot_x_to_first_command_list() {
    let Some((data, cci)) = own_data("boot_x") else { return };
    let build = data.join("build/x-usa-v0.2.0");
    let logs = data.join("logs");
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&cci).arg("--build").arg(&build).args(["--headless", "--frames", "300"])
        .output().unwrap();
    let log = std::fs::read_to_string(logs.join("latest.log")).unwrap_or_else(|_| String::from_utf8_lossy(&o.stderr).into_owned());
    assert_eq!(o.status.code(), Some(0), "{log}");
    assert!(log.contains("gsp::Gpu: first command list"), "{log}");
    assert!(!log.contains("panic") && !log.contains("guest memory fault"), "{log}");

    let p: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(logs.join("progress-run.json")).unwrap()).unwrap();
    let entries = p["interpreted_entries"].as_array().unwrap();
    assert!(entries.iter().all(|e| e["count"].as_u64() >= Some(1) && ["arm", "thumb"].contains(&e["mode"].as_str().unwrap())), "{p}");
    // services in the order the title first called them, each with its commands
    let services: Vec<&str> = p["services"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    let gsp = services.iter().position(|&s| s == "gsp::Gpu").expect("gsp::Gpu called");
    assert!(services.iter().position(|&s| s == "srv:") < Some(gsp), "{services:?}");
    assert!(p["services"][gsp]["commands"].as_array().is_some_and(|c| !c.is_empty()), "{p}");
    assert!(std::fs::read_to_string(logs.join("loadcro.csv")).unwrap().lines().count() >= 2, "a header and a module");

    // the software renderer's top screen, 400x240, and not one flat colour
    let png = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(logs.join("top.png")).unwrap()));
    let mut r = png.read_info().unwrap();
    let mut px = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut px).unwrap();
    assert_eq!((info.width, info.height), (400, 240));
    assert!(px.chunks(4).any(|p| p[..3] != px[..3]), "the top screen is one flat colour {:?}", &px[..4]);
    let _ = std::fs::remove_dir_all(&data);
}

/// Run the real game headless for `frames` with `extra` arguments; its log, and its top and bottom screens (RGBA).
fn run_x(data: &std::path::Path, cci: &str, frames: u32, extra: &[&str]) -> (String, Vec<u8>, Vec<u8>) {
    let logs = data.join("logs");
    for f in ["top.png", "bottom.png", "audio.wav"] {
        let _ = std::fs::remove_file(logs.join(f));
    }
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--frames", &frames.to_string()]).args(extra)
        .output().unwrap();
    let log = std::fs::read_to_string(logs.join("latest.log")).unwrap_or_else(|_| String::from_utf8_lossy(&o.stderr).into_owned());
    assert_eq!(o.status.code(), Some(0), "{log}");
    assert!(!log.contains("panic") && !log.contains("guest memory fault"), "{log}");
    (log, rgba(&logs.join("top.png")), rgba(&logs.join("bottom.png")))
}

fn rgba(path: &std::path::Path) -> Vec<u8> {
    let mut r = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap())).read_info().unwrap();
    let mut px = vec![0; r.output_buffer_size().unwrap()];
    r.next_frame(&mut px).unwrap();
    px
}

/// A fresh data dir of its own under the temp dir (no saves or extdata; the shared one keeps its own, and tests running
/// at the same time, in this process or another worktree's, never share one), holding a copy of
/// <XR_DATA>/build/x-usa-v0.2.0/xgame.dll; None (skip) without XR_DATA/XR_CCI. A test removes it once it passed.
fn own_data(name: &str) -> Option<(std::path::PathBuf, String)> {
    let (Ok(data), Ok(cci)) = (std::env::var("XR_DATA"), std::env::var("XR_CCI")) else {
        println!("skipping {name}: XR_DATA or XR_CCI not set");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("redex_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let build = dir.join("build/x-usa-v0.2.0");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::copy(std::path::Path::new(&data).join("build/x-usa-v0.2.0/xgame.dll"), build.join("xgame.dll")).unwrap();
    Some((dir, cci))
}

const SCRIPTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/playthrough");

/// Runs a playthrough script in a data dir of its own (`--script` implies --deterministic and runs to the last
/// checkpoint); every `expect` in it must match or the run exits non-zero. Returns the checkpoint lines of the log,
/// or None (the test skips, saying why) when Vulkan did not start here: the expects were recorded on Vulkan hardware,
/// and software drawing gives other pixels.
fn run_script(data: &std::path::Path, cci: &str, script: &str) -> Option<String> {
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--renderer", "vulkan", "--script", script]).output().unwrap();
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
    if log.contains("renderer software") || log.contains("drawing in software") {
        println!("skipping {script}: no Vulkan hardware renderer here (the run drew in software), and its expects need one");
        return None;
    }
    assert_eq!(o.status.code(), Some(0), "{log}");
    Some(log.lines().filter(|l| l.starts_with("checkpoint ")).collect::<Vec<_>>().join("
"))
}

/// A new game (scripts/playthrough/new-game.txt): English, title, Sycamore's intro, the boy, the name "Red", the
/// wake-up cutscene, first control in the bedroom, the menu. Every checkpoint's screen hash must match its expect.
#[test]
fn scripted_new_game() {
    let Some((data, cci)) = own_data("scripted_new_game") else { return };
    let Some(cps) = run_script(&data, &cci, &format!("{SCRIPTS}/new-game.txt")) else { return };
    assert!(cps.contains("checkpoint menu"), "{cps}");
    let _ = std::fs::remove_dir_all(&data);
}

/// The same script twice, in two data dirs, gives the same screen hashes (a --script run is deterministic).
#[test]
fn script_runs_repeat() {
    let Some((a, cci)) = own_data("script_repeat_a") else { return };
    let Some((b, _)) = own_data("script_repeat_b") else { return };
    let script = format!("{SCRIPTS}/short.txt");
    let (Some(x), Some(y)) = (run_script(&a, &cci, &script), run_script(&b, &cci, &script)) else { return };
    assert!(x.contains("checkpoint late"), "{x}");
    assert_eq!(x, y);
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

/// Task 3b fix: redex.exe delay-loads steam_api64.dll. A copy of redex.exe alone in a folder (no DLL there or on PATH)
/// plays short.txt with every expect matching, and with --world it says "Start Steam to play online" instead of dying.
#[test]
fn plays_offline_without_the_steam_dll() {
    let Some((data, cci)) = own_data("no_steam_dll") else { return };
    let bin = data.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_redex"), bin.join("redex.exe")).unwrap();
    // cargo puts the DLL's folder on a test's PATH: take every folder holding one off
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::env::split_paths(&path).filter(|p| !p.join("steam_api64.dll").exists())).unwrap();
    let o = Command::new(bin.join("redex.exe"))
        .current_dir(&bin)
        .env("PATH", path)
        .arg("--cci").arg(&cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--renderer", "vulkan", "--script", &format!("{SCRIPTS}/short.txt"), "--world", "127.0.0.1:9"])
        .output().unwrap();
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
    assert_ne!(o.status.code(), Some(0xC000_0135_u32 as i32), "redex.exe did not start without steam_api64.dll");
    if log.contains("renderer software") || log.contains("drawing in software") {
        return println!("skipping the expects: no Vulkan hardware renderer here");
    }
    assert_eq!(o.status.code(), Some(0), "{log}");
    assert!(log.contains("Start Steam to play online") && log.contains("steam_api64.dll is missing"), "{log}");
    let _ = std::fs::remove_dir_all(&data);
}

/// plan 4 Task 3b: --world with Steam closed (XR_NO_STEAM=1 takes that path without closing the player's Steam) says
/// "Start Steam to play online" and plays offline exactly as without --world: short.txt's expects all match.
#[test]
fn world_without_steam_plays_offline_unchanged() {
    let Some((data, cci)) = own_data("world_no_steam") else { return };
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--renderer", "vulkan", "--script", &format!("{SCRIPTS}/short.txt"), "--world", "127.0.0.1:9"])
        .env("XR_NO_STEAM", "1").output().unwrap();
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
    if log.contains("renderer software") || log.contains("drawing in software") {
        return println!("skipping: no Vulkan hardware renderer here, and short.txt's expects need one");
    }
    assert_eq!(o.status.code(), Some(0), "{log}");
    assert!(log.contains("Start Steam to play online") && log.contains("playing offline"), "{log}");
    assert!(!log.contains("world: joining") && !data.join("user/redex-id").exists(), "no online identity offline\n{log}");
    let _ = std::fs::remove_dir_all(&data);
}

/// The 100% save (XR_SAVE_100PCT, or <XR_DATA>/../saves/x-100pct/main; skips without it; copied, never changed) in
/// the savedata dir: title, Continue, the Pokemon Center, out of the door, the menu; checkpoints match their expects.
#[test]
fn load_100pct_save() {
    let Some((data, cci)) = own_data("load_100pct_save") else { return };
    if !with_100pct_save(&data, "load_100pct_save") {
        return;
    }
    let Some(cps) = run_script(&data, &cci, &format!("{SCRIPTS}/save-100pct.txt")) else { return };
    assert!(cps.contains("checkpoint menu"), "{cps}");
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap();
    assert!(!log.contains("DllLangSelect"), "X booted as new (language select)");
    let _ = std::fs::remove_dir_all(&data);
}

/// Copies the 100% save (XR_SAVE_100PCT, or <XR_DATA>/../saves/x-100pct/main) into `data` with the format records
/// and extdata X expects; false (the test skips) without it.
fn with_100pct_save(data: &std::path::Path, test: &str) -> bool {
    let save = std::env::var("XR_SAVE_100PCT").map(std::path::PathBuf::from).unwrap_or_else(|_| {
        std::path::Path::new(&std::env::var("XR_DATA").unwrap()).join("../saves/x-100pct/main")
    });
    if !save.exists() {
        println!("skipping {test}: no save at {}", save.display());
        return false;
    }
    let dir = data.join("user/savedata/0004000000055D00");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&save, dir.join("main")).unwrap();
    // the original is read-only (it must never change); a read-only copy fails to open and X boots as new
    let mut p = std::fs::metadata(dir.join("main")).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    p.set_readonly(false);
    std::fs::set_permissions(dir.join("main"), p).unwrap();
    // the archive's format record as X writes it on its first boot (copied from such a run)
    std::fs::write(data.join("user/savedata/0004000000055D00.format"), [0, 0, 4, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]).unwrap();
    // and the extdata X makes on its first boot (a console with this save has it; without it X asks for the
    // language first): its format record and empty folders
    let ext = data.join("user/extdata/000000000000055D");
    for d in ["btvideo", "hgmail", "pss_fr", "reg", "trhouse"] {
        std::fs::create_dir_all(ext.join(d)).unwrap();
    }
    std::fs::write(data.join("user/extdata/000000000000055D.format"), [0, 0, 0, 0, 0xF, 0, 0, 0, 0x31, 3, 0, 0, 0, 0, 0, 0]).unwrap();
    true
}

/// xr-net's field accessors (plan 4 Task 1) follow a scripted walk from the 100% save: the map id changes at the
/// Pokemon Center door, the facing follows the d-pad, the mode is Run while B is held. The script has no screen
/// hashes, so it runs on any renderer.
#[test]
fn field_accessors_follow_a_walk() {
    let Some((data, cci)) = own_data("field_accessors") else { return };
    if !with_100pct_save(&data, "field_accessors_follow_a_walk") {
        return;
    }
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--script", &format!("{SCRIPTS}/mp-field-walk.txt")]).output().unwrap();
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
    assert_eq!(o.status.code(), Some(0), "{log}");
    let field = |cp: &str| log.lines().find(|l| l.starts_with(&format!("field {cp}: "))).unwrap_or_else(|| panic!("no field line for {cp}
{log}")).to_string();
    let has = |cp: &str, what: &str| assert!(field(cp).contains(what), "{cp}: wanted {what:?} in {:?}", field(cp));
    has("pokecenter", "map 202 ");
    has("outside", "map 200 ");
    for (cp, f) in [("face_r", "Right"), ("face_u", "Up"), ("face_d", "Down"), ("face_l", "Left")] {
        has(cp, &format!("facing {f} mode Walk moving false"));
    }
    has("walk", "facing Right mode Walk moving true");
    has("run", "facing Right mode Run moving true");
    // the player is one of the pool's move models, alongside the zone's NPCs
    assert!(field("outside").contains("/32 slots used") && !field("outside").contains("?/32"), "{}", field("outside"));
    let _ = std::fs::remove_dir_all(&data);
}

/// start-screen plan 1 Task 1: a state saved between frames (XR_STATE_SAVE_AT=N:PATH) and loaded by a new process
/// (XR_STATE_LOAD=PATH) runs on exactly like the run it came from. The field walk (100% save) straight to frame N+600
/// (its last checkpoint, `run`), and loaded at N and run the 600 frames left: the same last hash and RAM digest.
#[test]
#[ignore]
fn state_round_trip_matches_straight_run() {
    const N: u64 = 1020; // + 600 = 1620, mp-field-walk.txt's last checkpoint
    let walk = |name: &str, env: &[(&str, &str)]| -> Option<(String, Vec<u8>, std::path::PathBuf)> {
        let (data, cci) = own_data(name)?;
        if !with_100pct_save(&data, "state_round_trip_matches_straight_run") {
            return None;
        }
        let o = Command::new(env!("CARGO_BIN_EXE_redex"))
            .arg("--cci").arg(&cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
            .args(["--headless", "--script", &format!("{SCRIPTS}/mp-field-walk.txt")])
            .env("XR_MEMDUMP", "1").envs(env.iter().copied()).output().unwrap();
        let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
        assert_eq!(o.status.code(), Some(0), "{name}: {log}");
        let hash = log.lines().find(|l| l.starts_with("checkpoint run at frame ")).unwrap_or_else(|| panic!("{name}: no checkpoint run\n{log}"));
        let ram = <sha2::Sha256 as sha2::Digest>::digest(std::fs::read(data.join("logs/mem-cp-run.bin")).unwrap()).to_vec();
        Some((hash.to_string(), ram, data))
    };
    let Some((a, a_ram, a_dir)) = walk("state_a", &[]) else { return };
    let state = std::env::temp_dir().join(format!("redex_state_{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let Some((_, _, b_dir)) = walk("state_b", &[("XR_STATE_SAVE_AT", &format!("{N}:{}", state.display()))]) else { return };
    assert!(state.exists(), "XR_STATE_SAVE_AT wrote no state at frame {N}");
    let Some((c, c_ram, c_dir)) = walk("state_c", &[("XR_STATE_LOAD", &state.display().to_string())]) else { return };
    let log = std::fs::read_to_string(c_dir.join("logs/latest.log")).unwrap();
    assert!(log.contains("XR_STATE_LOAD: state loaded"), "C did not start from the state\n{log}");
    assert_eq!(c, a, "the last checkpoint after loading the state");
    assert!(c_ram == a_ram, "RAM differs at the last checkpoint after loading the state");
    for d in [a_dir, b_dir, c_dir] {
        let _ = std::fs::remove_dir_all(d);
    }
    let _ = std::fs::remove_file(&state);
}

/// start-screen plan 2 Task 1: after XR_STATE_LOAD the main loop's frame is interpreted, and its call to the per-frame
/// hook's address still runs the hook: a fake peer (XR_FAKE_PEERS) becomes a puppet in the 600 frames after the load.
#[test]
#[ignore]
fn hooks_fire_after_a_state_load() {
    let state = std::env::temp_dir().join(format!("redex_hooks_state_{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let Some((data, _)) = puppet_run("hooks_after_load", None, "mp-field-walk.txt", &[("XR_STATE_SAVE_AT", &format!("1020:{}", state.display()))]) else { return };
    let load = state.display().to_string();
    let env = [("XR_STATE_LOAD", load.as_str()), ("XR_FAKE_PEERS", "1"), ("XR_FAKE_STILL", "1")];
    let (data, log) = puppet_run("hooks_after_load", Some(data), "mp-field-walk.txt", &env).unwrap();
    assert!(log.contains("XR_STATE_LOAD: state loaded"), "{log}");
    assert!(log.contains("the per-frame hook runs on guest thread"), "no per-frame hook after the load\n{log}");
    assert!(log.contains("puppets: spawn session"), "no puppet after the load\n{log}");
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_file(&state);
}

/// start-screen plan 2 Task 1: a save state names the xgame.dll that made it (its SHA-256 after the magic); one whose
/// fingerprint differs is refused before anything loads.
#[test]
#[ignore]
fn a_state_from_another_dll_is_refused() {
    let state = std::env::temp_dir().join(format!("redex_other_dll_{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let Some((data, _)) = puppet_run("state_other_dll", None, "mp-field-walk.txt", &[("XR_STATE_SAVE_AT", &format!("1020:{}", state.display()))]) else { return };
    let mut bytes = std::fs::read(&state).unwrap();
    bytes[8] ^= 1; // the fingerprint's first byte
    std::fs::write(&state, &bytes).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(std::env::var("XR_CCI").unwrap()).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--script", &format!("{SCRIPTS}/mp-field-walk.txt")])
        .env("XR_STATE_LOAD", &state).output().unwrap();
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default() + &String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{log}");
    assert!(log.contains("made with another xgame.dll"), "{log}");
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_file(&state);
}

/// plan 5 Task 1 (scripts/playthrough/mp-field-modes.txt, 100% save): the live appearance is the save's gender and
/// fashion (MyStatus, save 0x14005 and 0x14030) and passes the game's ranges; the mode follows walk, run and the bike
/// (Y, then the bike on the item wheel); and the camera puts the player's feet on the player's sprite within 4 px. The
/// sprite is found where two screens a turn on the spot apart differ (nothing else moves near the player then). No
/// screen hashes: any renderer.
#[test]
fn field_appearance_bike_and_camera() {
    let Some((data, cci)) = own_data("field_modes") else { return };
    if !with_100pct_save(&data, "field_appearance_bike_and_camera") {
        return;
    }
    let save = std::fs::read(data.join("user/savedata/0004000000055D00/main")).unwrap();
    let hex: String = save[0x14030..0x14040].iter().map(|b| format!("{b:02x}")).collect();
    let want = format!("gender {} fashion {hex} valid true", save[0x14005]);
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--script", &format!("{SCRIPTS}/mp-field-modes.txt")]).output().unwrap();
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
    assert_eq!(o.status.code(), Some(0), "{log}");
    let line = |kind: &str, cp: &str| log.lines().find(|l| l.starts_with(&format!("{kind} {cp}: "))).unwrap_or_else(|| panic!("no {kind} line for {cp}\n{log}")).to_string();
    for cp in ["face_d", "bike"] {
        assert!(line("appearance", cp).ends_with(&want), "{} vs the save's {want}", line("appearance", cp));
    }
    for (cp, mode) in [("walk", "mode Walk moving true"), ("run", "mode Run moving true"), ("wheel", "mode Walk moving false"), ("bike", "mode Bike moving true"), ("bike_still", "mode Bike")] {
        assert!(line("field", cp).contains(mode), "{cp}: wanted {mode:?} in {:?}", line("field", cp));
    }
    // the projection against the sprite: the pixels that changed between facing down and facing left, near the point
    let cam = line("camera", "face_d");
    let at: Vec<f32> = cam.rsplit(' ').next().unwrap().split(',').map(|v| v.parse().unwrap_or_else(|_| panic!("{cam}"))).collect();
    assert_eq!(line("field", "face_d").split(" pos ").nth(1), line("field", "face_l").split(" pos ").nth(1), "turned on the spot");
    let (a, b) = (rgba(&data.join("logs/top-cp-face_d.png")), rgba(&data.join("logs/top-cp-face_l.png")));
    let bpp = a.len() / (400 * 240);
    let sprite: Vec<(f32, f32)> = (0..400 * 240)
        .filter(|i| (0..3).any(|c| a[i * bpp + c].abs_diff(b[i * bpp + c]) > 24))
        .map(|i| ((i % 400) as f32, (i / 400) as f32))
        .filter(|p| (p.0 - at[0]).abs() < 40.0 && (p.1 - at[1]).abs() < 60.0)
        .collect();
    assert!(sprite.len() > 50, "the turn changed only {} pixels near {at:?}", sprite.len());
    // the sprite's middle and its top and bottom, by percentiles (a few far pixels flicker from frame to frame)
    let pct = |mut v: Vec<f32>, q: f32| {
        v.sort_by(f32::total_cmp);
        v[((v.len() - 1) as f32 * q) as usize]
    };
    let (mid, top, bottom) = (pct(sprite.iter().map(|p| p.0).collect(), 0.5), pct(sprite.iter().map(|p| p.1).collect(), 0.05), pct(sprite.iter().map(|p| p.1).collect(), 0.95));
    let nearest = sprite.iter().map(|p| (p.0 - at[0]).hypot(p.1 - at[1])).fold(f32::MAX, f32::min);
    println!("camera: player at {at:?}, sprite middle x {mid}, y {top}..{bottom}, nearest sprite pixel {nearest:.1} px");
    assert!(nearest <= 4.0, "the projected feet {at:?} are {nearest:.1} px from the sprite (middle x {mid}, y {top}..{bottom})");
    assert!((at[0] - mid).abs() <= 4.0, "{at:?} not on the sprite's middle x {mid}");
    // the feet, not just any point of the sprite (Task 4 anchors names from it): in the sprite's lowest quarter and
    // within 12 px of its bottom (the bottom rows include the shadow under the feet)
    assert!(at[1] >= top + 0.75 * (bottom - top) && (at[1] - bottom).abs() <= 12.0, "{at:?} not at the sprite's feet: y {top}..{bottom}");
    let _ = std::fs::remove_dir_all(&data);
}

/// A puppet test run: the 100% save in a data dir of its own (unless `data` is given), `script` from the playthrough
/// folder, `env` set (XR_FAKE_PEERS and co.). Returns the data dir and the log; a run that does not exit 0, or a
/// xgame.dll without the puppet hooks, fails the test.
fn puppet_run(name: &str, data: Option<std::path::PathBuf>, script: &str, env: &[(&str, &str)]) -> Option<(std::path::PathBuf, String)> {
    let cci = std::env::var("XR_CCI").ok()?;
    let data = match data {
        Some(d) => d,
        None => {
            let (d, _) = own_data(name)?;
            if !with_100pct_save(&d, name) {
                return None;
            }
            d
        }
    };
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--script", &format!("{SCRIPTS}/{script}")])
        .envs(env.iter().copied()).output().unwrap();
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
    assert_eq!(o.status.code(), Some(0), "{log}");
    assert!(!log.contains("built without the puppet hooks"), "XR_DATA's xgame.dll lacks hooks.txt's puppet hooks: rebuild it\n{log}");
    Some((data, log))
}

/// The checkpoint's "puppets CP: N shown: 0xID model M unsaved at X,Y,Z; ..." line: (id, x, z) per puppet.
fn puppets_at(log: &str, cp: &str) -> Vec<(String, f32, f32)> {
    let line = log.lines().find(|l| l.starts_with(&format!("puppets {cp}: "))).unwrap_or_else(|| panic!("no puppets line for {cp}\n{log}"));
    let list = line.split_once("known)").unwrap().1.trim_start_matches(':');
    list.split(';')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let w: Vec<&str> = p.split_whitespace().collect();
            assert_eq!(w[3], "unsaved", "a puppet without the not-saved bit: {line}");
            let xyz: Vec<f32> = w[5].split(',').map(|v| v.parse().unwrap()).collect();
            (w[0].to_string(), xyz[0], xyz[2])
        })
        .collect()
}

fn field_line<'a>(log: &'a str, cp: &str) -> &'a str {
    log.lines().find(|l| l.starts_with(&format!("field {cp}: "))).unwrap_or_else(|| panic!("no field line for {cp}\n{log}"))
}

/// plan 4 Task 4: two made-up remote players (XR_FAKE_PEERS, no server) become puppets in the Pokemon Center through
/// the game's own Create, walk (positions change between checkpoints), are gone with the pool the game empties at the
/// door, appear again outside on the new map, and are deleted when the feed stops (XR_FAKE_TO).
#[test]
fn puppets_follow_a_fake_feed() {
    let env = [("XR_FAKE_PEERS", "2"), ("XR_FAKE_TO", "1360")];
    let Some((data, log)) = puppet_run("puppets_feed", None, "mp-puppets.txt", &env) else { return };
    let (inside, later) = (puppets_at(&log, "inside"), puppets_at(&log, "inside_later"));
    assert_eq!(inside.len(), 2, "{log}");
    assert!(field_line(&log, "inside").contains("map 202 ") && field_line(&log, "inside").contains("11/32 slots used"));
    let at = |v: &[(String, f32, f32)]| v.iter().map(|p| (p.1, p.2)).collect::<Vec<_>>();
    assert_ne!(at(&inside), at(&later), "they walk");
    // out of the door: the game deleted the old ones (never deleted twice), new ones on map 200
    let outside = puppets_at(&log, "outside");
    assert_eq!(outside.len(), 2, "{log}");
    assert!(field_line(&log, "outside").contains("map 200 ") && field_line(&log, "outside").contains("13/32 slots used"));
    assert!(outside.iter().all(|p| !inside.iter().any(|q| q.0 == p.0)), "new puppets (new ids) after the map change");
    assert!(!log.contains("(0x8000): map change") && !log.contains("(0x8001): map change"), "the game's DeleteAll took the inside ones: {log}");
    assert_eq!(puppets_at(&log, "outside_later").len(), 2);
    // the feed stops: both deleted through the game, the pool back to the zone's NPCs and the player
    assert_eq!(log.matches("): gone").count(), 2, "{log}");
    assert!(puppets_at(&log, "gone").is_empty() && field_line(&log, "gone").contains("11/32 slots used"), "{log}");
    let _ = std::fs::remove_dir_all(&data);
}

/// Review Focus 3: 30 remote players, more than the game can hold. At most MAX_PUPPETS (8) are shown (17 exhausted the
/// field heap in the Pokemon Center and hung the game), the rest are not, the zone's NPCs are all there, and the game
/// neither hangs nor faults: the player still walks out of the door.
#[test]
fn puppet_overflow_keeps_room_for_the_game() {
    let Some((data, log)) = puppet_run("puppets_overflow", None, "mp-puppets.txt", &[("XR_FAKE_PEERS", "30")]) else { return };
    // inside: 9 used (8 NPCs, the player); outside: 11
    assert_eq!(puppets_at(&log, "inside").len(), 8, "{log}");
    assert!(field_line(&log, "inside").contains("map 202 ") && field_line(&log, "inside").contains("17/32 slots used"));
    assert_eq!(puppets_at(&log, "outside").len(), 8, "{log}");
    assert!(field_line(&log, "outside").contains("map 200 ") && field_line(&log, "outside").contains("19/32 slots used"), "{}", field_line(&log, "outside"));
    assert!(log.contains("22 players not shown: at most 8 puppets"), "{log}");
    // the screen still changes: the game runs (a hang in the fatal loop freezes it)
    let hash = |cp: &str| log.lines().find(|l| l.starts_with(&format!("checkpoint {cp} "))).unwrap().rsplit(' ').next().unwrap().to_string();
    assert_ne!(hash("inside"), hash("outside"), "{log}");
    let _ = std::fs::remove_dir_all(&data);
}

/// Review Focus 2: a puppet standing on the tile left of the player. Facing it and pressing A starts nothing, and the
/// player walks left through it (past its tile, x 8649) as if it were not there.
#[test]
fn puppets_do_not_block_or_talk() {
    let env = [("XR_FAKE_PEERS", "1"), ("XR_FAKE_STILL", "1"), ("XR_FAKE_AT", "480,507"), ("XR_FAKE_FROM", "1260")];
    let Some((data, log)) = puppet_run("puppets_pass", None, "mp-puppet-pass.txt", &env) else { return };
    let p = puppets_at(&log, "talked");
    assert_eq!(p.len(), 1);
    assert!((p[0].1 - 8649.0).abs() < 1.0, "the puppet stands at tile 480: {p:?}");
    assert!(field_line(&log, "talked").contains("pos 8667.00,"), "{}", field_line(&log, "talked"));
    // a talk that ran a script would hold the player; a puppet that blocked would stop it at 8667
    assert!(field_line(&log, "walked").contains("pos 8577.00,"), "walked through: {}", field_line(&log, "walked"));
    let _ = std::fs::remove_dir_all(&data);
}

/// Review Focus 1: a save made with two puppets in the room. At the save, StoreAll's block with the puppets in the
/// pool equals the block without them, byte for byte (XR_PUPPET_SAVECHECK); the file holds no puppet entry; and the
/// save loads with the room's 8 NPCs and the player only, as the original does. Since plan 5 Task 3 the two wear an
/// outfit (Red's: the model's +0x998, which the save entry's +0x9C would hold) and ride the bike.
#[test]
fn a_save_with_puppets_reloads_clean() {
    let env = [("XR_FAKE_PEERS", "2"), ("XR_PUPPET_SAVECHECK", "1"), ("XR_FAKE_LOOK", RED), ("XR_FAKE_MODE", "bike")];
    let Some((data, log)) = puppet_run("puppets_save", None, "mp-puppet-save.txt", &env) else { return };
    assert_eq!(puppets_at(&log, "before").len(), 2, "{log}");
    assert!(log.lines().filter(|l| l.starts_with("puppets: spawn session") && l.contains("dressed 48640901")).count() >= 2, "{log}");
    assert!(log.contains("save check: FieldMoveModelSave with 2 puppets in the pool equals the one without them"), "{log}");
    let main = std::fs::read(data.join("user/savedata/0004000000055D00/main")).unwrap();
    let ids: Vec<u16> = (0..32)
        .map(|i| 0x2000 + 0x108 * i)
        .filter(|&e| main[e] & 1 != 0)
        .map(|e| u16::from_le_bytes([main[e + 4], main[e + 5]]))
        .collect();
    assert_eq!(ids.len(), 9, "{ids:X?}");
    assert!(ids.iter().all(|&id| id < 0x8000), "a puppet in the save: {ids:X?}");
    let (data, log) = puppet_run("puppets_save", Some(data), "mp-puppet-reload.txt", &[]).unwrap();
    assert!(field_line(&log, "continued").contains("map 202 ") && field_line(&log, "continued").contains("9/32 slots used"), "{log}");
    assert!(!log.lines().any(|l| l.starts_with("puppets continued")), "{log}");
    let _ = std::fs::remove_dir_all(&data);
}

/// Red's outfit (the new-game boy, save bytes 0x14030) and the pyjamas he wears in the bedroom, as the 19 wire bytes
/// (gen 6, version 1, gender 0, the fashion); a girl's (the new-game girl's third look); and bytes that fail the game's
/// ranges (a peer sending them is the plain boy).
const RED: &str = "0601004864090101cb08034100000000000000";
const PYJAMAS: &str = "060100480409010a6a00004100000000000000";
const GIRL: &str = "060101c00024024c0590020004000000000000";
const HOSTILE: &str = "0601007fffffffffffffffffffffffffffffff";

/// The "dress NAME: N slots in use" line's N.
fn dress_used(log: &str, cp: &str) -> usize {
    let l = log.lines().find(|l| l.starts_with(&format!("dress {cp}: "))).unwrap_or_else(|| panic!("no dress line for {cp}\n{log}"));
    l.split_whitespace().nth(2).unwrap().parse().unwrap()
}

/// The "heap NAME: model heaps 0x14B T free largest L, 0x14C ..." line: (total, largest) per heap.
fn heap_at(log: &str, cp: &str) -> Vec<(u32, u32)> {
    let l = log.lines().find(|l| l.starts_with(&format!("heap {cp}: "))).unwrap_or_else(|| panic!("no heap line for {cp}\n{log}"));
    l.split(", ").map(|h| {
        let w: Vec<&str> = h.split_whitespace().collect();
        let n = |s: &str| s.trim_end_matches(',').parse::<u32>().unwrap_or_else(|_| panic!("{l}"));
        (n(w[w.len() - 4]), n(w[w.len() - 1]))
    }).collect()
}

/// plan 5 Task 3, with a fake feed: one remote player in the Laverre Pokemon Center changes outfit and mode every 400
/// frames. Red's outfit walking: a dressed puppet (an outfit slot more than the local player's); the girl on the bike:
/// the girl model is not loaded in this room, so the stand-in boy wears the plain look; bytes that fail the ranges
/// on skates: the plain boy; Red surfing: no puppet (until surf is traced) and his outfit released; Red running:
/// dressed again. Each outfit change is a Delete and a Create through the game, never two puppets for one player.
#[test]
fn puppets_wear_outfits_and_modes() {
    let looks = [RED, GIRL, HOSTILE, RED, RED].join(",");
    // standing two tiles right of the player, in view for the screens
    let env = [("XR_FAKE_PEERS", "1"), ("XR_FAKE_FROM", "700"), ("XR_FAKE_EVERY", "400"), ("XR_FAKE_LOOK", &looks), ("XR_FAKE_MODE", "walk,bike,skates,surf,run"), ("XR_FAKE_STILL", "1"), ("XR_FAKE_AT", "22,26")];
    let Some((data, log)) = puppet_run("puppets_looks", None, "mp-puppet-looks.txt", &env) else { return };
    let spawns: Vec<&str> = log.lines().filter(|l| l.starts_with("puppets: spawn session")).collect();
    println!("{}", spawns.join("\n"));
    assert_eq!(spawns.len(), 4, "{log}");
    assert!(spawns[0].contains("(model 2) dressed 48640901"), "{}", spawns[0]);
    assert!(spawns[1].contains("(model 2) plain (its outfit did not fit") || spawns[1].contains("(model 1) dressed c0002402"), "{}", spawns[1]);
    assert!(spawns[2].contains("(model 2) plain at"), "{}", spawns[2]);
    assert!(spawns[3].contains("(model 2) dressed 48640901"), "{}", spawns[3]);
    for (cp, n) in [("phase0", 1), ("phase1", 1), ("phase2", 1), ("phase3", 0), ("phase4", 1)] {
        assert_eq!(puppets_at(&log, cp).len(), n, "{cp}\n{log}");
        assert!(field_line(&log, cp).contains("map 202 ") && field_line(&log, cp).contains(&format!("{}/32 slots used", 9 + n)), "{}", field_line(&log, cp));
    }
    // the dress-up slots: the local player's, plus Red's while he is shown
    assert_eq!([0, 1, 2, 3, 4].map(|k| dress_used(&log, &format!("phase{k}"))), [2, 1, 1, 1, 2], "{log}");
    assert_eq!(log.matches("): outfit change").count(), 2, "{log}");
    assert!(log.contains("): gone"), "the surfer goes: {log}");
    // Red wore it in phase 0 only until phase 4 (the plain looks between ask for nothing)
    assert_eq!(log.matches("puppets: released the outfit 48640901").count(), 1, "{log}");
    let _ = std::fs::remove_dir_all(&data);
}

/// plan 5 Task 3: 50 outfit changes mid-map (Red, the pyjamas, Red, ...; XR_FAKE_EVERY=200 frames: a change, a second of respawn gap and the parts loading, each change a
/// Delete and a Create under the spawn gap and the heap gate). Afterwards, in Red's outfit again, the pool's slots, the
/// dress-up slots and both model heaps are as they were before the changes; once he leaves, the pool and the dress-up
/// slots are as before any puppet.
#[test]
fn fifty_outfit_swaps_leak_nothing() {
    let looks = [RED, PYJAMAS].join(",");
    let env = [("XR_FAKE_PEERS", "1"), ("XR_FAKE_FROM", "900"), ("XR_FAKE_EVERY", "200"), ("XR_FAKE_TO", "11090"), ("XR_FAKE_LOOK", &looks), ("XR_FAKE_STILL", "1")];
    let Some((data, log)) = puppet_run("puppets_swaps", None, "mp-puppet-swaps.txt", &env) else { return };
    for cp in ["none", "base", "after", "gone"] {
        println!("{}\n{}\n{}", field_line(&log, cp), log.lines().find(|l| l.starts_with(&format!("dress {cp}: "))).unwrap(), log.lines().find(|l| l.starts_with(&format!("heap {cp}: "))).unwrap());
    }
    assert_eq!(log.matches("): outfit change").count(), 50, "{log}");
    assert_eq!((puppets_at(&log, "base").len(), puppets_at(&log, "after").len()), (1, 1), "{log}");
    let slots = |cp: &str| field_line(&log, cp).split(", ").last().unwrap().to_string();
    assert_eq!(slots("base"), slots("after"));
    assert_eq!(slots("none"), slots("gone"));
    assert_eq!((dress_used(&log, "base"), dress_used(&log, "after")), (2, 2));
    assert_eq!((dress_used(&log, "none"), dress_used(&log, "gone")), (1, 1));
    let free = |cp: &str| heap_at(&log, cp).iter().map(|h| h.0).collect::<Vec<_>>();
    assert_eq!(free("base"), free("after"), "the model heaps' free bytes after 50 changes");
    assert_eq!(free("none"), free("gone"), "the model heaps' free bytes once he left");
    let _ = std::fs::remove_dir_all(&data);
}

/// plan 5 Task 3 fix round 1, the twin: a puppet in the local player's exact outfit (the save trainer's, the 100% save's). Its
/// parameter carries PUPPET_MARK, so it has a dress-up slot of its own beside the player's (2 slots, one outfit).
/// Then the local player's outfit changes (XR_SET_FASHION writes Red's into MyStatus) and the game builds the player
/// in it at the door, in a new model system: no hang, the twin is there again outside in the save trainer's outfit, and once it
/// leaves, the pool, the dress-up slots and both model heaps are as in the same run without any puppet.
#[test]
fn a_twin_survives_the_local_players_change_of_clothes() {
    const TOBIN: &str = "06010088b91905d00932031100000000000000";
    let set = ("XR_SET_FASHION", "4864090101cb08034100000000000000@1060");
    let env = [("XR_FAKE_PEERS", "1"), ("XR_FAKE_FROM", "700"), ("XR_FAKE_TO", "1700"), ("XR_FAKE_STILL", "1"), ("XR_FAKE_LOOK", TOBIN), set];
    let Some((data, log)) = puppet_run("puppets_twin", None, "mp-puppet-twin.txt", &env) else { return };
    for l in log.lines().filter(|l| l.starts_with("dress ") || l.starts_with("heap ") || l.starts_with("appearance ") || l.starts_with("puppets")) {
        println!("{l}");
    }
    let spawns: Vec<&str> = log.lines().filter(|l| l.starts_with("puppets: spawn session")).collect();
    assert_eq!(spawns.len(), 2, "{log}");
    assert!(spawns.iter().all(|s| s.contains("dressed 88b91905d0093203")), "{spawns:?}");
    assert_eq!((puppets_at(&log, "twin").len(), dress_used(&log, "twin")), (1, 2), "the twin's own slot beside the player's");
    assert!(log.contains("appearance changed: gender 0 fashion 4864090101cb0803"), "{log}");
    assert_eq!((puppets_at(&log, "outside").len(), dress_used(&log, "outside")), (1, 2), "{log}");
    assert!(!log.contains("parts are going") && !log.contains("left the outfit"), "{log}");
    assert_eq!(dress_used(&log, "gone"), 1);
    // (no fake player, but the puppet hooks on, so the checkpoints log the dress-up slots and heaps)
    let base = puppet_run("puppets_twin_base", None, "mp-puppet-twin.txt", &[("XR_FAKE_PEERS", "0"), set]).unwrap();
    let slots = |l: &str| field_line(l, "gone").split(", ").last().unwrap().to_string();
    assert_eq!(slots(&log), slots(&base.1));
    assert_eq!(heap_at(&log, "gone"), heap_at(&base.1, "gone"), "the model heaps once the twin left vs no puppet at all");
    assert_eq!(dress_used(&base.1, "gone"), 1);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&base.0);
}

/// plan 5 Task 3 fix round 2: a dressed puppet across a wild battle. At the field close the game takes every model's
/// draw binding down and then frees every dress-up slot (ReleaseAll), with no puppet tick in between; xr-net never
/// deletes a dressed puppet whose outfit's slot is gone or unloading: back on the field it asks for the outfit again,
/// and the puppet is shown dressed. Every Delete logs its outfit's slot state, and none ran over freed parts. Once the
/// player left, the pool, the dress-up slots and both model heaps equal the same script's run without any puppet.
#[test]
fn a_dressed_puppet_survives_a_battle_without_a_delete_over_freed_parts() {
    let env = [("XR_FAKE_PEERS", "1"), ("XR_FAKE_LOOK", RED), ("XR_FAKE_FROM", "3700"), ("XR_FAKE_TO", "4750")];
    let Some((data, log)) = puppet_run("puppets_battle_dressed", None, "mp-puppet-battle-dressed.txt", &env) else { return };
    for l in log.lines().filter(|l| l.starts_with("puppets: ") || l.starts_with("dress ") || l.starts_with("heap ") || l.contains("DllBattle placed")) {
        println!("{l}");
    }
    let battle = log.find("DllBattle placed").unwrap_or_else(|| panic!("no battle: {log}"));
    let spawn = log.find("puppets: spawn session").unwrap();
    assert!(spawn < battle, "the puppet was there before the battle: {log}");
    assert!(log[battle..].contains("puppets: asked again for the outfit 48640901"), "{log}");
    assert_eq!(puppets_at(&log, "after").len(), 1, "back after the battle: {log}");
    let after = log.lines().find(|l| l.starts_with("dress after: ")).unwrap();
    assert!(dress_used(&log, "after") == 2 && after.matches(":0/3").count() == 2, "its outfit loaded again beside the player's: {after}");
    let deletes: Vec<&str> = log.lines().filter(|l| l.starts_with("puppets: delete session")).collect();
    assert!(!deletes.is_empty());
    for d in &deletes {
        assert!(d.contains("; its outfit in slot") && !d.contains("request 4") && !d.contains("state 5"), "a Delete over parts that were gone or going: {d}");
    }
    assert_eq!((puppets_at(&log, "left").len(), dress_used(&log, "left")), (0, 1));
    let base = puppet_run("puppets_battle_base", None, "mp-puppet-battle-dressed.txt", &[("XR_FAKE_PEERS", "0")]).unwrap();
    assert!(base.1.contains("DllBattle placed"), "the baseline met its battle too");
    let slots = |l: &str| field_line(l, "left").split(", ").last().unwrap().to_string();
    assert_eq!(slots(&log), slots(&base.1));
    assert_eq!(dress_used(&base.1, "left"), 1);
    assert_eq!(heap_at(&log, "left"), heap_at(&base.1, "left"), "the model heaps after the battle vs no puppet at all");
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&base.0);
}

/// plan 5 Task 5: eight peers, each in an outfit of its own (the looks list goes one further per peer: only the
/// dress-up slots the game leaves free get dressed (3), the rest wear the plain look), present before a wild battle and
/// after it. Back on the field there are eight puppets again, none was deleted over freed parts, and once the player
/// left the pool, the dress-up slots and both model heaps equal the same script's run without any puppet.
#[test]
fn eight_dressed_peers_survive_a_battle_clean() {
    const TOBIN: &str = "06010088b91905d00932031100000000000000";
    let looks = [RED, PYJAMAS, TOBIN, GIRL, RED, PYJAMAS, TOBIN, GIRL].join(",");
    // (from frame 3300: with them from 3700 the timing moves and the script walks into a map change, not a battle)
    let env = [("XR_FAKE_PEERS", "8"), ("XR_FAKE_LOOK", &looks), ("XR_FAKE_FROM", "3300"), ("XR_FAKE_TO", "4800")];
    let Some((data, log)) = puppet_run("puppets_battle_eight", None, "mp-puppet-battle-dressed.txt", &env) else { return };
    for l in log.lines().filter(|l| l.starts_with("puppets: ") || l.starts_with("puppets ") || l.starts_with("dress ") || l.starts_with("heap ") || l.contains("DllBattle placed")) {
        println!("{l}");
    }
    let battle = log.find("DllBattle placed").unwrap_or_else(|| panic!("no battle: {log}"));
    assert_eq!(puppets_at(&log, "before").len(), 8, "{log}");
    assert!(log[..battle].matches("puppets: spawn session").count() >= 8, "{log}");
    assert!(log[battle..].contains("puppets: asked again for the outfit"), "{log}");
    assert_eq!(puppets_at(&log, "after").len(), 8, "eight again after the battle: {log}");
    assert!(log.lines().find(|l| l.starts_with("puppets after: ")).unwrap().contains("(8 known)"), "an orphan: {log}");
    // the dress-up slots: the player's and the three the game leaves, all loaded again
    assert_eq!((dress_used(&log, "before"), dress_used(&log, "after")), (4, 4), "{log}");
    for d in log.lines().filter(|l| l.starts_with("puppets: delete session") && l.contains("its outfit in slot")) {
        assert!(!d.contains("request 4") && !d.contains("state 5"), "a Delete over parts that were gone or going: {d}");
    }
    // once they left (the feed stopped at 4800): the pool, the dress-up slots and both model heaps equal the same
    // script's run without any peer
    assert_eq!((puppets_at(&log, "left").len(), dress_used(&log, "left")), (0, 1), "{log}");
    let base = puppet_run("puppets_battle_eight_base", None, "mp-puppet-battle-dressed.txt", &[("XR_FAKE_PEERS", "0")]).unwrap();
    assert!(base.1.contains("DllBattle placed"), "the baseline met its battle too");
    let slots = |l: &str| field_line(l, "left").split(", ").last().unwrap().to_string();
    assert_eq!(slots(&log), slots(&base.1));
    assert_eq!(heap_at(&log, "left"), heap_at(&base.1, "left"), "the model heaps after the battle vs no puppet at all");
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&base.0);
}

/// Fix round 1: puppets across wild battles (DllField unloads and comes back). After each, the pool holds at most 8
/// puppets and every one is known to xr-net: none is left behind beside new ones (which at 17 hung the game).
#[test]
fn puppets_survive_battles_without_orphans() {
    // (plain: dressed puppets shift the game's timing and the script meets one wild battle instead of two; a dressed
    // run across a battle is in the plan 5 Task 3 report, fix round 1)
    let Some((data, log)) = puppet_run("puppets_battle", None, "mp-puppet-battle.txt", &[("XR_FAKE_PEERS", "10")]) else { return };
    assert!(log.matches("DllBattle placed").count() >= 2, "no battle happened: {log}");
    for l in log.lines().filter(|l| l.starts_with("dress ") || l.starts_with("heap ") || l.starts_with("field ") || l.contains("puppets forgotten") || l.contains("left the outfit") || l.contains("released the outfit")) {
        println!("{l}");
    }
    for cp in ["aquacorde", "route2-grass", "wild3", "got-away", "field"] {
        let line = log.lines().find(|l| l.starts_with(&format!("puppets {cp}: "))).unwrap_or_else(|| panic!("{cp}\n{log}"));
        let n = puppets_at(&log, cp).len();
        assert!(n <= 8, "{line}");
        assert!(line.contains(&format!("{n} shown ({n} known)")), "an orphan (in the pool, not known): {line}");
    }
    let _ = std::fs::remove_dir_all(&data);
}

/// Fix round 1, the heap worst case: 30 players and a Fly tour through six cities. Each city shows 8 puppets, the
/// field's model heaps each keep a free block of more than HEAP_KEEP (128 KB), and the game runs to the end.
#[test]
fn puppet_heap_holds_on_a_fly_tour() {
    let Some((data, log)) = puppet_run("puppets_fly", None, "mp-puppet-fly.txt", &[("XR_FAKE_PEERS", "30")]) else { return };
    for cp in ["lumiose", "coumarine", "anistar", "shalour", "snowbelle", "vaniville"] {
        assert_eq!(puppets_at(&log, cp).len(), 8, "{cp}\n{log}");
        let heap = log.lines().find(|l| l.starts_with(&format!("heap {cp}: "))).unwrap();
        println!("{heap}");
        for id in ["0x14B ", "0x14C "] {
            let largest: u32 = heap.split(id).nth(1).and_then(|r| r.split(" largest ").nth(1)).and_then(|r| r.split([',', ' ']).next()).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("{heap}"));
            assert!(largest > xr_net::x::puppet::HEAP_KEEP, "{heap}");
        }
    }
    let _ = std::fs::remove_dir_all(&data);
}

/// plan 4 Task 4 over the network: a Y player (girl) stands two tiles right of the walk script's player on map 200,
/// through an in-process world server. The game shows them as a puppet (the boy model: the girl's is not loaded on
/// this map), and deletes it within 2 s of them leaving.
#[test]
#[cfg(feature = "mp-tests")] // --dev-id, and a headless run in a world
fn a_world_peer_becomes_a_puppet() {
    let Some((data, cci)) = own_data("world_puppet") else { return };
    if !with_100pct_save(&data, "a_world_peer_becomes_a_puppet") {
        return;
    }
    let t = redex_world::transport::TcpTransport::bind("127.0.0.1:0").unwrap();
    let addr = t.local_addr().unwrap();
    let _server = redex_world::Server::start(redex_world::Config { dev: true, ..Default::default() }, t);
    let mut child = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--script", &format!("{SCRIPTS}/mp-field-walk.txt"), "--frames", "4000"])
        .args(["--world", &addr.to_string(), "--name", "Host", "--dev-id"])
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let o = xr_net::Options { appearance: vec![1], ..xr_net::Options::new(addr.to_string(), redex_proto::Auth::DevId(0x6E57), redex_proto::TITLE_Y, "Guest") };
    let mut guest = Some(xr_net::Client::start(o).unwrap());
    let here = xr_net::Local { map: 200, pos: [8703.0, 144.0, 9135.0], facing: 1, mode: 0, appearance: None };
    let log = || std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
    let end = Instant::now() + Duration::from_secs(600);
    let (mut left, mut gone_after) = (None::<Instant>, None);
    let code = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s.code();
        }
        assert!(Instant::now() < end, "the run took over 10 minutes");
        if let Some(g) = &mut guest {
            g.frame(Some(here));
        }
        let l = log();
        if left.is_none() && l.contains("puppets: spawn session") {
            guest.take(); // Bye: the server tells the game the player left
            left = Some(Instant::now());
        }
        if let (Some(t), None) = (left, gone_after) {
            if l.contains("): gone") {
                gone_after = Some(t.elapsed());
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    };
    let l = log();
    assert_eq!(code, Some(0), "{l}");
    assert!(left.is_some(), "no puppet for the guest\n{l}");
    let spawn = l.lines().find(|x| x.starts_with("puppets: spawn session")).unwrap();
    assert!(spawn.contains("(model 2)"), "{spawn}");
    let gone = gone_after.unwrap_or_else(|| panic!("the puppet stayed after the guest left\n{l}"));
    assert!(gone < Duration::from_secs(2), "deleted {gone:?} after the guest left");
    let _ = std::fs::remove_dir_all(&data);
}

/// plan 4 Task 3: a headless run with --world reports the player to a world server as the walk script moves it. An
/// observer client waits on map 200 (outside the Pokemon Center) and sees the game's player arrive under its --name,
/// walk and run. Then the server is dropped mid-run: the game goes on to its last frame, and its frames take no longer
/// offline than online (XR_FRAMETIMES; the net thread owns the socket, the frame only hands state over).
#[test]
#[cfg(feature = "mp-tests")] // --dev-id, and a headless run in a world
fn world_server_sees_the_walk() {
    let Some((data, cci)) = own_data("world_walk") else { return };
    if !with_100pct_save(&data, "world_server_sees_the_walk") {
        return;
    }
    let t = redex_world::transport::TcpTransport::bind("127.0.0.1:0").unwrap();
    let addr = t.local_addr().unwrap();
    let mut server = Some(redex_world::Server::start(redex_world::Config { dev: true, ..Default::default() }, t));
    let mut child = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&cci).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--script", &format!("{SCRIPTS}/mp-field-walk.txt"), "--frames", "4000"])
        .args(["--world", &addr.to_string(), "--name", "Walker", "--dev-id"])
        .env("XR_FRAMETIMES", "1")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let mut eye = xr_net::Client::start(xr_net::Options::new(addr.to_string(), redex_proto::Auth::DevId(0xE7E), redex_proto::TITLE_Y, "Eye")).unwrap();
    let here = xr_net::Local { map: 200, pos: [0.0, 0.0, 0.0], facing: 1, mode: 0, appearance: None };
    let (mut seen, mut name, mut title, mut ran_at) = (Vec::<redex_proto::Pos>::new(), None, None, None::<Instant>);
    let end = Instant::now() + Duration::from_secs(600);
    let code = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s.code();
        }
        assert!(Instant::now() < end, "the run took over 10 minutes");
        eye.frame(Some(here));
        for r in eye.peers.values() {
            (name, title) = (Some(r.name.clone()), Some(r.title));
            if let Some((_, p)) = r.track.back() {
                if seen.last() != Some(p) {
                    seen.push(p.clone());
                }
            }
        }
        if ran_at.is_none() && seen.iter().any(|p| p.mode == redex_proto::MODE_RUN) {
            ran_at = Some(Instant::now());
        }
        // standing still online for a while after the run, then the server goes away
        if ran_at.is_some_and(|t| t.elapsed() > Duration::from_secs(10)) {
            server.take();
        }
        std::thread::sleep(Duration::from_millis(16));
    };
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(name.as_deref(), Some("Walker"), "the observer never saw the game's player\n{log}");
    assert_eq!(title, Some(redex_proto::TITLE_X), "an X game says so, from its loaded title");
    assert!(server.is_none(), "the run ended before the server was dropped (no Run seen?): {seen:?}");
    let xs: Vec<f32> = seen.iter().map(|p| p.x).collect();
    assert!(seen.iter().all(|p| p.map == 200), "{seen:?}");
    assert!(xs.windows(2).filter(|w| w[1] > w[0]).count() >= 3, "the walk right shows as rising x: {xs:?}");
    assert!(seen.iter().any(|p| p.mode == redex_proto::MODE_WALK) && seen.iter().any(|p| p.mode == redex_proto::MODE_RUN), "{seen:?}");
    assert!(log.contains("world: logged in"), "{log}");

    // frame pacing, standing still after the walk: online (server up) against offline (server gone, reconnecting)
    let csv = std::fs::read_to_string(data.join("logs/frametimes.csv")).unwrap();
    let rows: Vec<(u64, u32, bool, u32)> = csv.lines().skip(1).map(|l| {
        let f: Vec<&str> = l.split(',').collect();
        (f[0].parse().unwrap(), f[1].parse().unwrap(), f[3] == "1", f[2].parse().unwrap())
    }).collect();
    // the world client's own part of each frame (reading the player, the hand-off) is what the network could add to a
    // frame: it must stay small with the server up, killed, and reconnecting. Whole frames are only printed: on a
    // shared host they swing with whatever else runs (one contended run had a 1.1 s p99 offline while this stayed
    // under 0.5 ms)
    let hook = |online: bool| rows.iter().filter(|r| r.0 > 100 && r.2 == online).map(|r| r.3).max().unwrap_or(0);
    let (hon, hoff) = (hook(true), hook(false));
    println!("world client per frame: max {hon} us online, {hoff} us with the server killed, over {} frames", rows.len());
    assert!(hon < 2000 && hoff < 2000, "the world client took {hon}/{hoff} us in one frame");
    let mut on: Vec<u32> = rows.iter().filter(|r| r.0 > 1700 && r.2).map(|r| r.1).collect();
    let mut off: Vec<u32> = rows.iter().filter(|r| r.0 > 1700 && !r.2).map(|r| r.1).collect();
    assert!(on.len() > 100 && off.len() > 100, "online {} frames, offline {} frames after the walk", on.len(), off.len());
    // X draws at 30 fps, so host frames alternate cheap and dear: the mean and the tail, not the median, compare
    let q = |v: &mut Vec<u32>| {
        v.sort();
        (v.iter().map(|&x| x as u64).sum::<u64>() / v.len() as u64, v[v.len() * 9 / 10] as u64, v[v.len() * 99 / 100] as u64, v[v.len() - 1] as u64)
    };
    let ((nm, n90, n99, nmax), (fm, f90, f99, fmax)) = (q(&mut on), q(&mut off));
    println!("frame us after the walk: online {} frames mean {nm} p90 {n90} p99 {n99} max {nmax}; offline {} frames mean {fm} p90 {f90} p99 {f99} max {fmax}", on.len(), off.len());
    let _ = std::fs::remove_dir_all(&data);
}

/// X on the hardware renderer (headless Vulkan; software when Vulkan won't start), with scripted input:
/// - A at frame 400, once the language screen is up (by frame 300), moves the game on to its "Begin game" screen
///   (next-top.png), which a run without input never leaves;
/// - A again at frame 600 begins the game: DllTitle loads and by frame 1500 the title screen is up (title.png, not one
///   flat colour) with the title music playing (audio.wav, not all silence).
/// Needs XR_CCI and XR_DATA (with build/x-usa-v0.2.0/xgame.dll), skips without them. Runs in a data dir of its own
/// (own_data: no extdata, so X asks for the language).
#[test]
fn title_screen_renders() {
    let Some((data, cci)) = own_data("title_screen") else { return };
    let data = data.as_path();
    let logs = data.join("logs");
    let vulkan = ["--renderer", "vulkan", "--resolution", "1"];

    let (log, language, _) = run_x(data, &cci, 500, &vulkan);
    println!("renderer: {}", log.lines().find(|l| l.contains("through Vulkan") || l.contains("drawing in software")).unwrap_or("software"));
    std::fs::copy(logs.join("top.png"), logs.join("language-top.png")).unwrap();
    let (_, next, _) = run_x(data, &cci, 500, &[&vulkan[..], &["--press", "A@400"]].concat());
    std::fs::copy(logs.join("top.png"), logs.join("next-top.png")).unwrap();
    assert!(next != language, "pressing A left the language screen as it was");

    let (log, title, _) = run_x(data, &cci, 1500, &[&vulkan[..], &["--press", "A@400", "--press", "A@600"]].concat());
    std::fs::copy(logs.join("top.png"), logs.join("title.png")).unwrap();
    assert!(log.contains("DllTitle placed"), "the title screen's module never loaded
{log}");
    assert!(title.chunks(4).any(|p| p[..3] != title[..3]), "the top screen is one flat colour {:?}", &title[..4]);
    // "audio: <samples> samples, <not silent> not silent, peak <peak>"
    let line = log.lines().find_map(|l| l.strip_prefix("audio: ")).unwrap_or_else(|| panic!("no audio line
{log}"));
    let n: Vec<u64> = line.split(|c: char| !c.is_ascii_digit()).filter_map(|w| w.parse().ok()).collect();
    assert!(n[0] > 0 && n[1] > 0, "no sound: {line}");
    assert!(std::fs::metadata(logs.join("audio.wav")).unwrap().len() > 44, "audio.wav");
    let _ = std::fs::remove_dir_all(data);
}

/// start-screen plan 2 Task 2: `--make-preview boy|girl` makes the bedroom save state for each gender (headless,
/// deterministic, from scripts/preview/), logs to preview.log, prints only `progress N/6520`; each state then loads and
/// runs 60 frames. Final review I1: with a player's real save in `<data>/user` (the 100% save, which would boot into
/// Continue) the states are still made, in a folder of their own under `<data>/preview/<build>/`, and the save is not touched.
#[test]
#[ignore]
fn preview_states_for_both_genders() {
    let Some((data, cci)) = own_data("preview_states") else { return };
    if !with_100pct_save(&data, "preview_states_for_both_genders") {
        return;
    }
    let main = data.join("user/savedata/0004000000055D00/main");
    let (bytes, mtime) = (std::fs::read(&main).unwrap(), std::fs::metadata(&main).unwrap().modified().unwrap());
    let build = data.join("build/x-usa-v0.2.0");
    for (gender, model) in [("boy", 2), ("girl", 1)] {
        let state = data.join(format!("{gender}.state"));
        // a folder another maker (or a live preview) is using is left alone: each process has its own
        let stale = data.join("preview/x-usa-v0.2.0/user-other-1-1/stale");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"x").unwrap();
        let o = Command::new(env!("CARGO_BIN_EXE_redex"))
            .args(["--make-preview", gender, "--out"]).arg(&state).arg("--cci").arg(&cci).arg("--build").arg(&build).output().unwrap();
        let log = std::fs::read_to_string(data.join("logs/preview.log")).unwrap_or_default();
        assert_eq!(o.status.code(), Some(0), "{gender}: {log}");
        assert!(stale.exists(), "{gender}: another process's preview folder was wiped");
        assert_eq!((std::fs::read(&main).unwrap(), std::fs::metadata(&main).unwrap().modified().unwrap()), (bytes.clone(), mtime), "{gender}: the player's save changed");
        assert!(redex_builder::state_matches(&state, &build.join("xgame.dll")), "{gender}: the state does not match the DLL");
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(out.contains("progress 6500/6520"), "{gender}: stdout {out:?}");
        assert!(out.lines().all(|l| l.starts_with("progress ")), "{gender}: stdout has more than progress lines: {out:?}");
        assert!(log.contains(&format!("preview state: {gender}, model {model}")), "{gender}: {log}");
        assert!(!data.join("logs/latest.log").exists() && !data.join("logs/loadcro.csv").exists(), "{gender}: touched latest.log or loadcro.csv");
        let o = Command::new(env!("CARGO_BIN_EXE_redex"))
            .args(["--headless", "--frames", "6570", "--cci"]).arg(&cci).arg("--build").arg(&build)
            .env("XR_STATE_LOAD", &state).output().unwrap();
        let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_default();
        assert_eq!(o.status.code(), Some(0), "{gender} load: {log}");
        let _ = std::fs::remove_file(data.join("logs/latest.log"));
    }
    let _ = std::fs::remove_dir_all(&data);
}

/// `redex --preview` on a bedroom state of `gender`: XR_PREVIEW_BOY / XR_PREVIEW_GIRL name one made with XR_DATA's
/// xgame.dll, else one is made in `data` (about 60 s). The child, its stdin and its stdout. Without `studio` it sends
/// the v1 crops (`XR_STUDIO=0`).
fn preview_child(data: &std::path::Path, cci: &str, gender: &str, studio: bool) -> (std::process::Child, std::process::ChildStdin, std::io::BufReader<std::process::ChildStdout>) {
    let build = data.join("build/x-usa-v0.2.0");
    let state = std::env::var_os(format!("XR_PREVIEW_{}", gender.to_uppercase())).map(std::path::PathBuf::from).unwrap_or_else(|| {
        let state = data.join(format!("{gender}.state"));
        let o = Command::new(env!("CARGO_BIN_EXE_redex")).args(["--make-preview", gender, "--out"]).arg(&state).arg("--cci").arg(cci).arg("--build").arg(&build).output().unwrap();
        assert_eq!(o.status.code(), Some(0), "--make-preview {gender}");
        state
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--preview").arg(&state).arg("--cci").arg(cci).arg("--build").arg(&build)
        .env("XR_STUDIO", if studio { "1" } else { "0" })
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let (stdin, out) = (child.stdin.take().unwrap(), std::io::BufReader::new(child.stdout.take().unwrap()));
    (child, stdin, out)
}

/// XR_PREVIEW_SHOTS=DIR: a preview frame saved as DIR/redex-preview-child-NAME.png.
fn save_frame(f: &xr_frontend::start::preview::Frame, name: &str) {
    let Some(dir) = std::env::var_os("XR_PREVIEW_SHOTS").map(std::path::PathBuf::from) else { return };
    let mut e = png::Encoder::new(std::fs::File::create(dir.join(format!("redex-preview-child-{name}.png"))).unwrap(), f.width.into(), f.height.into());
    e.set_color(png::ColorType::Rgba);
    e.write_header().unwrap().write_image_data(&f.rgba).unwrap();
}

/// start-screen plan 2 Task 5: `redex --preview STATE` from the boy's bedroom state streams head-to-toe crops on
/// stdout: the state's own look (seq 0) first, then a look sent on stdin (seq 1, other pixels), a turn by `face left`,
/// and an exit 0 once stdin closes. XR_PREVIEW_BOY names a boy state made with XR_DATA's xgame.dll (else one is made
/// here, about 60 s); XR_PREVIEW_SHOTS=DIR saves the frames compared as PNGs. Prints the wall time per change.
#[test]
#[ignore]
fn the_preview_child_changes_look_and_turns() {
    use std::io::Write;
    use xr_frontend::start::preview::{read_frame, Frame};
    let Some((data, cci)) = own_data("preview_child") else { return };
    let (mut child, mut stdin, mut out) = preview_child(&data, &cci, "boy", false);
    let log = || std::fs::read_to_string(data.join("logs/preview.log")).unwrap_or_default();
    let save = |f: &Frame, name: &str| save_frame(f, name);
    let mut next = || read_frame(&mut out).unwrap_or_else(|e| panic!("no frame ({e})\n{}", log()));

    // the state's own look first, within 120 frames of the load
    let mut last = next();
    assert_eq!(last.seq, 0);
    let frame_at = |l: &str, what: &str| -> u64 { l.lines().find_map(|x| x.split_once(what)?.1.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).unwrap_or_else(|| panic!("no {what:?}\n{l}")) };
    let l = log();
    let (loaded, first) = (frame_at(&l, "loaded at frame "), frame_at(&l, "first frame at frame "));
    assert!(first - loaded <= 120, "the first frame came {} frames after the load", first - loaded);
    for _ in 0..10 {
        last = next();
    }
    save(&last, "0-state");

    // a look: seq 1 within 300 frames (a frame out is every second one), other pixels
    let t = Instant::now();
    writeln!(stdin, "look 1 {RED}").unwrap();
    let mut got = None;
    for _ in 0..150 {
        let f = next();
        if f.seq == 1 {
            got = Some(f);
            break;
        }
        assert_eq!(f.seq, 0);
        last = f;
    }
    let red = got.unwrap_or_else(|| panic!("no seq-1 frame within 300 frames\n{}", log()));
    println!("look change: {:.2} s wall", t.elapsed().as_secs_f64());
    assert_eq!((red.width, red.height), (last.width, last.height));
    assert_ne!(red.rgba, last.rgba, "the new look's frame is the old one's");
    // settled (the first new picture may still be coming in)
    let mut before = red;
    for _ in 0..10 {
        before = next();
    }
    save(&before, "1-red");

    // a turn: a frame soon after differs from the one before (the frame out ahead in the pipe, and the game reads the
    // d-pad at its next update: measured 2 frames out, about 0.1 s)
    let t = Instant::now();
    writeln!(stdin, "face left").unwrap();
    let k = (1..=10).find(|_| next().rgba != before.rgba).unwrap_or_else(|| panic!("face left: 10 frames the same
{}", log()));
    println!("face left: frame {k} out differs, {:.3} s wall", t.elapsed().as_secs_f64());
    let mut turned = before;
    for _ in 0..10 {
        turned = next();
    }
    save(&turned, "2-left");

    // stdin closed: exit 0 within 2 s (stdout drained meanwhile, so a write never blocks it)
    drop(stdin);
    let drain = std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
    let t = Instant::now();
    let code = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s.code();
        }
        if t.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            panic!("the child still runs 2 s after stdin closed\n{}", log());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = drain.join();
    assert_eq!(code, Some(0), "{}", log());
    let l = log();
    assert!(!l.contains("panic") && !l.contains("guest memory fault"), "{l}");
    let _ = std::fs::remove_dir_all(&data);
}

/// start-screen plan 2 Task 5 fix round 1: a girl's look changed in place is drawn as the game draws her own.
/// - The default girl outfit (long hair, style 2, under the hat) keeps her long hair down her back: at least a quarter of
///   the pale blonde pixels of her own pyjama look (both facing right), and almost none in the top 40 rows. Loaded with
///   the NPC part set (header byte 3 = 0) her hair was a brown plank: 7 % of them.
/// - Back in her own outfit (the state's, which the game built), the frame matches the state's own (95 % of the pixels
///   within 24 levels): nothing of the look before stays behind.
/// - The same in the studio view (v2): every frame that carries a look's seq shows that look, never the one before it
///   (the model cached through the pictures without the player is the old look's).
#[test]
#[ignore]
fn a_girls_look_changed_in_place_is_drawn_as_her_own() {
    use std::io::Write;
    use xr_frontend::start::preview::{read_frame, Frame};
    let Some((data, cci)) = own_data("preview_girl") else { return };
    let (mut child, mut stdin, mut out) = preview_child(&data, &cci, "girl", false);
    let log = || std::fs::read_to_string(data.join("logs/preview.log")).unwrap_or_default();
    let mut next = || read_frame(&mut out).unwrap_or_else(|e| panic!("no frame ({e})\n{}", log()));
    // pale blonde pixels (her hair), and those in the top 40 rows (above the hat)
    let blonde = |f: &Frame| {
        let px = |i: usize| (i32::from(f.rgba[4 * i]), i32::from(f.rgba[4 * i + 1]), i32::from(f.rgba[4 * i + 2]));
        let hair: Vec<usize> = (0..f.rgba.len() / 4)
            .filter(|&i| {
                let (r, g, b) = px(i);
                r > 190 && g > 170 && b > 110 && r - b > 40 && (r - g).abs() < 40
            })
            .collect();
        (hair.len(), hair.iter().filter(|&&i| i / usize::from(f.width) < 40).count())
    };
    let same = |a: &Frame, b: &Frame| {
        let n = a.rgba.chunks(4).zip(b.rgba.chunks(4)).filter(|(p, q)| p.iter().zip(q.iter()).all(|(x, y)| x.abs_diff(*y) <= 24)).count();
        n as f64 / (a.rgba.len() / 4) as f64
    };
    let mut own = next();
    for _ in 0..5 {
        own = next();
    }
    assert_eq!(own.seq, 0);
    save_frame(&own, "girl-0-own");
    let wait = |next: &mut dyn FnMut() -> Frame, seq: u32| -> Frame { (0..300).map(|_| next()).find(|f| f.seq == seq).unwrap_or_else(|| panic!("no seq-{seq} frame")) };
    writeln!(stdin, "look 1 060101403c2402002013034105200000000000").unwrap();
    let mut dressed = wait(&mut next, 1);
    for _ in 0..10 {
        dressed = next();
    }
    save_frame(&dressed, "girl-1-default");
    let ((h0, _), (h1, top)) = (blonde(&own), blonde(&dressed));
    println!("blonde pixels: own {h0}, default outfit {h1} ({top} in the top 40 rows)");
    assert!(h1 * 4 >= h0 && top < 30, "the default outfit's long hair is not drawn as hers: {h1} of {h0} blonde pixels, {top} in the top rows");
    // her own outfit again (no load: its slot is the game's)
    writeln!(stdin, "look 2 060101400424024c0590020004200000000000").unwrap();
    let mut back = wait(&mut next, 2);
    for _ in 0..3 {
        back = next();
    }
    save_frame(&back, "girl-2-own-again");
    let s = same(&own, &back);
    println!("back in her own outfit: {:.1} % of the pixels as before", 100.0 * s);
    assert!(s >= 0.95, "her own outfit after the round trip differs: {:.1} % the same", 100.0 * s);
    drop(stdin);
    let drain = std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
    assert_eq!(child.wait().unwrap().code(), Some(0), "{}", log());
    let _ = drain.join();
    let l = log();
    assert!(!l.contains("panic") && !l.contains("guest memory fault"), "{l}");

    // the studio (v2): the pixels where either frame has the model, the share alike
    let like = |a: &Frame, b: &Frame| {
        let (mut n, mut ok) = (0, 0);
        for (p, q) in a.rgba.chunks(4).zip(b.rgba.chunks(4)) {
            if p[3] != 0 || q[3] != 0 {
                n += 1;
                ok += usize::from(p[3] == q[3] && p.iter().zip(q.iter()).all(|(x, y)| x.abs_diff(*y) <= 24));
            }
        }
        ok as f64 / n.max(1) as f64
    };
    let (mut child, mut stdin, mut out) = preview_child(&data, &cci, "girl", true);
    let mut next = || read_frame(&mut out).unwrap_or_else(|e| panic!("no studio frame ({e})\n{}", log()));
    let mut own = next();
    for _ in 0..10 {
        own = next();
    }
    assert!(own.floor.is_some() && own.seq == 0, "not a studio frame of her own look");
    // every frame of a look from its first on, then the last as the look itself
    let mut look = |next: &mut dyn FnMut() -> Frame, seq: u32, hex: &str| -> Vec<Frame> {
        writeln!(stdin, "look {seq} {hex}").unwrap();
        let mut got: Vec<Frame> = Vec::new();
        for _ in 0..600 {
            let f = next();
            assert!(f.floor.is_some(), "a crop among the studio frames");
            if f.seq == seq {
                got.push(f);
            }
            if got.len() == 40 {
                break;
            }
        }
        assert_eq!(got.len(), 40, "seq {seq}: too few frames\n{}", log());
        got
    };
    let dressed = look(&mut next, 1, "060101403c2402002013034105200000000000");
    let (first, last) = (&dressed[0], &dressed[39]);
    save_frame(first, "girl-studio-1-first");
    println!("studio: the default outfit's first frame is {:.2} like itself settled, {:.2} like her own", like(first, last), like(first, &own));
    for (i, f) in dressed.iter().enumerate() {
        assert!(like(f, last) > like(f, &own) && like(f, last) >= 0.7, "seq-1 frame {i} shows the look before: {:.2} like the look, {:.2} like her own", like(f, last), like(f, &own));
    }
    let back = look(&mut next, 2, "060101400424024c0590020004200000000000");
    for (i, f) in back.iter().enumerate() {
        assert!(like(f, &own) > like(f, last), "seq-2 frame {i} still shows the default outfit: {:.2} like her own, {:.2} like it", like(f, &own), like(f, last));
    }
    println!("studio: back in her own outfit, {:.2} like the state's own", like(&back[39], &own));
    drop(stdin);
    let drain = std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
    assert_eq!(child.wait().unwrap().code(), Some(0), "{}", log());
    let _ = drain.join();
    let l = log();
    assert!(!l.contains("panic") && !l.contains("guest memory fault") && !l.contains("cropped frames go out"), "{l}");
    let _ = std::fs::remove_dir_all(&data);
}

/// start-screen plan 2 Task 10: the start screen's side against the real children. `Cards` finds no boy state, makes it
/// with `--make-preview`, starts `--preview` on it and writes the card of Red's outfit, facing the front: the studio's
/// 360x480 on its backdrop (the studio view), and not the state's own pyjamas. About 90 s.
#[test]
#[ignore]
fn cards_are_drawn_by_the_real_preview_child() {
    use xr_frontend::start::{characters::Character, library::GameEntry, preview};
    let Some((data, cci)) = own_data("cards_real") else { return };
    let (game, exe) = (GameEntry { build_id: "x-usa-v0.2.0".into(), dump: cci.into() }, std::path::Path::new(env!("CARGO_BIN_EXE_redex")));
    let look = (0..19).map(|i| u8::from_str_radix(&RED[2 * i..2 * i + 2], 16).unwrap()).collect();
    let red = Character { id: 0x7ed, name: "Red".into(), look, created: 1, changed: 1 };
    let mut cards = preview::Cards::new(vec![red.clone()]);
    let t = Instant::now();
    while cards.pump(exe, &game, &data) != Some(0x7ed) {
        assert!(t.elapsed() < Duration::from_secs(300), "no card in 300 s\n{}", std::fs::read_to_string(data.join("logs/preview.log")).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(15));
    }
    println!("card in {:.1} s", t.elapsed().as_secs_f64());
    let card = preview::card_path(&data, &red);
    let mut r = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&card).unwrap())).read_info().unwrap();
    let mut px = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut px).unwrap();
    assert_eq!((info.width, info.height), (360, 480));
    if let Some(dir) = std::env::var_os("XR_PREVIEW_SHOTS") {
        std::fs::copy(&card, std::path::Path::new(&dir).join("redex-preview-card-red.png")).unwrap();
    }
    // the state Cards made, and its own outfit's frame
    let state = preview::state_path(&data, "x-usa-v0.2.0", 0);
    assert!(redex_builder::state_matches(&state, &data.join("build/x-usa-v0.2.0/xgame.dll")));
    let mut child = Command::new(exe).arg("--preview").arg(&state).arg("--cci").arg(&game.dump).arg("--build").arg(data.join("build/x-usa-v0.2.0"))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let own = preview::read_frame(&mut out).unwrap();
    drop(child.stdin.take());
    let _ = std::io::copy(&mut out, &mut std::io::sink());
    let _ = child.wait();
    assert_ne!(preview::studio_card(&own), px, "the card shows the state's own outfit");
    let _ = std::fs::remove_dir_all(&data);
}

/// The studio view against the real child, for a boy and a girl: only the player's model, with alpha.
/// - The first frames are studio frames (v2): 360x480, alpha only 0 or 255, nothing but black where it is 0.
/// - In a look (Red's outfit; the girl's hat, bag and ponytail), an orbit sweep of 8 yaws at zoom 1 shows the whole
///   model at every yaw: 0.75 to 0.95 of the height, inside the picture with a margin, its feet on the floor shadow;
///   the yaws differ from each other, and 360 is 0 again.
/// - The closest zoom (2.5 asked, clamped to ZOOM_MAX) is the face: the model passes the picture's bottom and covers more
///   of it.
/// - A spin (a new yaw every 8 ms for 2 s, so every one of the child's frames has one) comes back at 55 frames a second or
///   more.
/// XR_PREVIEW_BOY / XR_PREVIEW_GIRL name states made with XR_DATA's xgame.dll; XR_PREVIEW_SHOTS=DIR saves the sweep.
#[test]
#[ignore]
fn the_studio_spins_a_whole_model_with_alpha() {
    use std::io::Write;
    use xr_frontend::start::preview::{read_frame, Frame};
    let Some((data, cci)) = own_data("studio") else { return };
    let log = || std::fs::read_to_string(data.join("logs/preview.log")).unwrap_or_default();
    // (covered pixels, top row, bottom row, left, right) of the model
    let shape = |f: &Frame| {
        let (w, h) = (usize::from(f.width), usize::from(f.height));
        let (mut n, mut t, mut b, mut l, mut r) = (0, h, 0, w, 0);
        for (i, p) in f.rgba.chunks(4).enumerate() {
            assert!(p[3] == 0 || p[3] == 255, "alpha {}", p[3]);
            if p[3] == 0 {
                assert_eq!(&p[..3], [0, 0, 0], "a clear pixel with colour");
                continue;
            }
            let (x, y) = (i % w, i / w);
            (n, t, b, l, r) = (n + 1, t.min(y), b.max(y), l.min(x), r.max(x));
        }
        (n, t, b, l, r)
    };
    for (gender, look) in [("boy", RED), ("girl", "060101c03a4402002013034105100000000000")] {
        let (mut child, mut stdin, out) = preview_child(&data, &cci, gender, true);
        // frames on a thread: the spin's rate is measured as they arrive
        let (tx, rx) = std::sync::mpsc::channel::<(Instant, Frame)>();
        let reader = std::thread::spawn(move || {
            let mut out = out;
            while let Ok(f) = read_frame(&mut out) {
                if tx.send((Instant::now(), f)).is_err() {
                    break;
                }
            }
        });
        let next = || rx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|e| panic!("{gender}: no frame ({e})\n{}", log())).1;
        let first = next();
        assert!(first.floor.is_some(), "{gender}: the first frame is a crop\n{}", log());
        assert_eq!((first.width, first.height), (360, 480));
        writeln!(stdin, "look 1 {look}").unwrap();
        let mut f = first;
        for _ in 0..400 {
            f = next();
            if f.seq == 1 {
                break;
            }
        }
        assert_eq!(f.seq, 1, "{gender}: the look never came\n{}", log());
        for _ in 0..10 {
            f = next();
        }
        // the sweep: three frames after each orbit (the frame in the pipe, then the new yaw)
        let mut seen = Vec::new();
        for yaw in (0..=360).step_by(45) {
            writeln!(stdin, "orbit {yaw} 1").unwrap();
            for _ in 0..3 {
                f = next();
            }
            while let Ok((_, g)) = rx.try_recv() {
                f = g;
            }
            let (n, t, b, l, r) = shape(&f);
            println!("{gender} yaw {yaw}: {n} pixels, rows {t}..{b}, columns {l}..{r}, floor {:?}", f.floor);
            assert!(n > 8000, "{gender} yaw {yaw}: {n} pixels");
            let tall = (b - t) as f32 / 480.0;
            assert!((0.75..=0.95).contains(&tall), "{gender} yaw {yaw}: {tall} of the height");
            assert!(t >= 4 && b <= 476 && l >= 4 && r <= 356, "{gender} yaw {yaw}: cut at the edge ({t}..{b}, {l}..{r})");
            let [cx, cy, rx_, ry] = f.floor.unwrap().map(i32::from);
            assert!(rx_ > 4 && ry > 0 && (cx - 180).abs() < 8 && (cy - b as i32).abs() < 24, "{gender} yaw {yaw}: floor {:?} under rows ..{b}", f.floor);
            save_frame(&f, &format!("studio-{gender}-{yaw:03}"));
            seen.push(f.rgba.clone());
        }
        let differs = |a: &[u8], b: &[u8]| a.chunks(4).zip(b.chunks(4)).filter(|(p, q)| p != q).count();
        for k in 1..8 {
            assert!(differs(&seen[k], &seen[k - 1]) > 1000, "{gender}: yaw {} draws as {}", k * 45, (k - 1) * 45);
        }
        // a whole turn is the front again (the idle animation may have moved a little)
        assert!(differs(&seen[8], &seen[0]) < 4000, "{gender}: 360 is not 0: {}", differs(&seen[8], &seen[0]));
        // the face
        writeln!(stdin, "orbit 0 2.5").unwrap();
        for _ in 0..3 {
            f = next();
        }
        let (n, t, b, ..) = shape(&f);
        println!("{gender} closest zoom: {n} pixels, rows {t}..{b}");
        let whole = shape(&Frame { rgba: seen[0].clone(), ..f.clone() }).0;
        assert!(b >= 478 && n > whole, "{gender}: the closest zoom is not closer ({n} pixels against {whole}, rows {t}..{b})");
        save_frame(&f, &format!("studio-{gender}-zoom"));
        // the spin: a new yaw every 8 ms for 2 s (at 16 ms, a sender beating against the child's 60 Hz leaves some frames
        // without a new yaw, and those are not sent)
        while rx.try_recv().is_ok() {}
        let t0 = Instant::now();
        let mut k = 0;
        while t0.elapsed() < Duration::from_secs(2) {
            writeln!(stdin, "orbit {} 1", k * 3 % 360).unwrap();
            k += 1;
            std::thread::sleep(Duration::from_millis(8));
        }
        let got: Vec<Instant> = rx.try_iter().map(|(t, _)| t).filter(|t| *t > t0 + Duration::from_millis(250) && *t <= t0 + Duration::from_secs(2)).collect();
        let fps = got.len() as f64 / 1.75;
        println!("{gender} spin: {fps:.1} frames a second");
        drop(stdin);
        assert_eq!(child.wait().unwrap().code(), Some(0), "{}", log());
        let _ = reader.join();
        let l = log();
        for line in l.lines().filter(|l| l.contains("studio")) {
            println!("{gender} log: {line}");
        }
        assert!(!l.contains("panic") && !l.contains("guest memory fault") && !l.contains("cropped frames go out"), "{l}");
        assert!(fps >= 55.0, "{gender}: the spin came back at {fps:.1} fps");
    }
    let _ = std::fs::remove_dir_all(&data);
}

// ---- start-screen plan 2 Task 7: the character's look in the game --------------------------------------------------

/// A playthrough script cut after `checkpoint <cp>` and without its expects (another look draws other pixels).
fn script_until(name: &str, script: &str, cp: &str) -> std::path::PathBuf {
    let text = std::fs::read_to_string(format!("{SCRIPTS}/{script}")).unwrap();
    let mut out = Vec::new();
    for l in text.lines().filter(|l| !l.starts_with("expect ")) {
        out.push(l);
        if l.split_whitespace().nth(1) == Some("checkpoint") && l.split_whitespace().nth(2) == Some(cp) {
            break;
        }
    }
    let path = std::env::temp_dir().join(format!("redex_{name}_{}.txt", std::process::id()));
    std::fs::write(&path, out.join("\n")).unwrap();
    path
}

/// A character in `data`'s characters.json (created at time 1) with this look and name; with `save`, the 100% save in
/// its user folder (as `with_100pct_save` puts it in `<data>/user`). None (skip) without the save.
fn character_in(data: &std::path::Path, look: xr_net::x::look::Look, name: &str, save: bool, test: &str) -> Option<u64> {
    use xr_frontend::start::characters::{user_dir, Store};
    let mut store = Store::load(data);
    let id = store.create(name, look.appearance().unwrap(), 1).unwrap();
    store.save(data).unwrap();
    if save {
        if !with_100pct_save(data, test) {
            return None;
        }
        std::fs::create_dir_all(user_dir(data, id).parent().unwrap()).unwrap();
        std::fs::rename(data.join("user"), user_dir(data, id)).unwrap();
    }
    Some(id)
}

/// `redex --character <id>` on a script (no expects), with `env`; the log. Exits 0 or the test fails.
fn character_run(data: &std::path::Path, cci: &str, build: &str, id: u64, script: &std::path::Path, env: &[(&str, &str)]) -> String {
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(cci).arg("--build").arg(data.join("build").join(build))
        .args(["--headless", "--character", &format!("{id:016x}"), "--script"]).arg(script)
        .envs(env.iter().copied()).output().unwrap();
    let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_else(|_| String::from_utf8_lossy(&o.stderr).into_owned());
    assert_eq!(o.status.code(), Some(0), "{log}");
    assert!(!log.contains("panic") && !log.contains("guest memory fault"), "{log}");
    log
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// The character whose look differs from the 100% save's boy (skin, hair and eyes).
const CONTINUE_LOOK: xr_net::x::look::Look = xr_net::x::look::Look { gender: 0, skin: 2, hair_style: 3, hair_colour: 1, eyes: 2 };

/// start-screen plan 2 Task 7: Continue as a character wears the character's look: the log says it was applied, and the
/// field reads it at the Pokemon Center's first checkpoint.
#[test]
#[ignore]
fn continue_wears_the_character_look() {
    let Some((data, cci)) = own_data("character_continue") else { return };
    let Some(id) = character_in(&data, CONTINUE_LOOK, "Calem", true, "continue_wears_the_character_look") else { return };
    let look = CONTINUE_LOOK.appearance().unwrap();
    let script = script_until("character_continue", "mp-field-walk.txt", "pokecenter");
    let log = character_run(&data, &cci, "x-usa-v0.2.0", id, &script, &[]);
    assert!(log.contains(&format!("character: look applied {} (continue)", hex(&look.to_bytes()))), "{log}");
    let line = log.lines().find(|l| l.starts_with("appearance pokecenter: ")).unwrap_or_else(|| panic!("no appearance line\n{log}"));
    assert!(line.ends_with(&format!("gender 0 fashion {} valid true", hex(&look.fashion))), "{line}");
    assert!(!log.contains("the game changed the look"), "{log}");
    let _ = std::fs::remove_dir_all(&data);
}

/// start-screen plan 2 Task 13 (Done 5): the 100% save imported as the start screen does (`Store::import`) is copied,
/// never touched (its SHA-256 the same after the import and after a game played on the copy), and the launch backs
/// the copy up first: `<character>/backups/<title>/` holds the save as it was before the game ran.
#[test]
#[ignore]
fn an_imported_save_is_never_touched_and_backed_up_before_play() {
    use sha2::{Digest, Sha256};
    use xr_frontend::start::characters::{user_dir, Store};
    let Some((data, cci)) = own_data("import_backup") else { return };
    let full = std::env::var("XR_SAVE_100PCT").map(std::path::PathBuf::from).unwrap_or_else(|_| std::path::Path::new(&std::env::var("XR_DATA").unwrap()).join("../saves/x-100pct/main"));
    let Ok(before) = std::fs::read(&full) else { return println!("skipping: no 100% save at {}", full.display()) };
    // the player's own save where an emulator keeps it: `.../savedata/<title>/main`
    let full = data.join("players-pc/savedata/0004000000055D00/main");
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(&full, &before).unwrap();
    let sha = |b: &[u8]| hex(&Sha256::digest(b));
    let mut store = Store::load(&data);
    let id = store.import(&data, &full, 1).unwrap();
    store.save(&data).unwrap();
    let main = user_dir(&data, id).join("savedata/0004000000055D00/main");
    assert_eq!(sha(&std::fs::read(&full).unwrap()), sha(&before), "the import changed the player's save");
    assert_eq!(sha(&std::fs::read(&main).unwrap()), sha(&before), "the copy differs");
    let log = character_run(&data, &cci, "x-usa-v0.2.0", id, &script_until("import_backup", "mp-field-walk.txt", "pokecenter"), &[]);
    assert!(log.contains(&format!("character {id:016x}: 1 save(s) backed up")), "{log}");
    let backups: Vec<_> = std::fs::read_dir(user_dir(&data, id).parent().unwrap().join("backups/0004000000055D00")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(sha(&std::fs::read(&backups[0]).unwrap()), sha(&before), "the backup is not the save as it was before the launch");
    assert_eq!(sha(&std::fs::read(&full).unwrap()), sha(&before), "playing changed the player's own save");
    println!("100% save {} unchanged; backup {}", sha(&before), backups[0].display());
    let _ = std::fs::remove_dir_all(&data);
}

/// start-screen plan 2 Task 13 (Done 2): the creator's picture of a look (the preview child's studio view, front)
/// and the game's own picture of the same look: a girl as a new game to first control in the bedroom, a boy on the
/// 100% save (Continue) outside the Pokemon Center, facing the camera. Saves XR_PREVIEW_SHOTS/redex-preview-child-vs-game-<gender>.png and
/// .../redex-game-vs-creator-<gender>.png (skips without XR_PREVIEW_SHOTS) for the side-by-side compare.
#[test]
#[ignore]
fn creator_preview_and_the_game_agree() {
    use std::io::Write;
    use xr_frontend::start::preview::read_frame;
    let Some(dir) = std::env::var_os("XR_PREVIEW_SHOTS").map(std::path::PathBuf::from) else { return println!("skipping: XR_PREVIEW_SHOTS not set") };
    let girl = xr_net::x::look::Look { gender: 1, skin: 3, hair_style: 4, hair_colour: 2, eyes: 3 };
    for (gender, look) in [("girl", girl), ("boy", CONTINUE_LOOK)] {
        let test = format!("creator_vs_game_{gender}");
        let Some((data, cci)) = own_data(&test) else { return };
        // the studio view (what the creator shows) and the head-to-toe crop of the bedroom's own camera
        for (studio, kind) in [(true, "studio"), (false, "crop")] {
            let (mut child, mut stdin, mut out) = preview_child(&data, &cci, gender, studio);
            writeln!(stdin, "look 1 {}", hex(&look.appearance().unwrap().to_bytes())).unwrap();
            let mut f = read_frame(&mut out).unwrap();
            for _ in 0..600 {
                if f.seq == 1 {
                    break;
                }
                f = read_frame(&mut out).unwrap();
            }
            assert_eq!(f.seq, 1, "{gender}: the look never came");
            for _ in 0..20 {
                f = read_frame(&mut out).unwrap();
            }
            save_frame(&f, &format!("vs-game-{gender}-{kind}"));
            drop(stdin);
            while read_frame(&mut out).is_ok() {}
            assert_eq!(child.wait().unwrap().code(), Some(0));
        }
        let top = if gender == "girl" {
            let id = character_in(&data, look, "Mira", false, &test).unwrap();
            character_run(&data, &cci, "x-usa-v0.2.0", id, &script_until(&test, "new-game.txt", "control"), &[]);
            data.join("logs/top.png")
        } else {
            let Some(id) = character_in(&data, look, "Calem", true, &test) else { return };
            // outside the Pokemon Center, facing the camera
            character_run(&data, &cci, "x-usa-v0.2.0", id, &script_until(&test, "mp-field-walk.txt", "face_d"), &[]);
            data.join("logs/top-cp-face_d.png")
        };
        std::fs::copy(&top, dir.join(format!("redex-game-vs-creator-{gender}.png"))).unwrap();
        let _ = std::fs::remove_dir_all(&data);
    }
}

/// start-screen plan 2 Task 7: a new game (new-game.txt picks the boy and types "Red") as a girl character named Mira
/// is Mira the girl: at first control MyStatus says so and the player's model is the girl's. Keeps the top screen
/// there as redex_character_new_game_top.png in the temp dir (Task 13's compare).
#[test]
#[ignore]
fn new_game_takes_the_character() {
    let Some((data, cci)) = own_data("character_new_game") else { return };
    let look = xr_net::x::look::Look { gender: 1, skin: 3, hair_style: 4, hair_colour: 2, eyes: 3 };
    let id = character_in(&data, look, "Mira", false, "new_game_takes_the_character").unwrap();
    let script = script_until("character_new_game", "new-game.txt", "control");
    let log = character_run(&data, &cci, "x-usa-v0.2.0", id, &script, &[("XR_SLOTS", "1")]);
    let line = |kind: &str| log.lines().find(|l| l.starts_with(&format!("{kind} control: "))).unwrap_or_else(|| panic!("no {kind} line\n{log}")).to_string();
    assert!(line("appearance").contains(": gender 1 "), "{}", line("appearance"));
    assert!(line("trainer").ends_with(": name \"Mira\""), "{}", line("trainer"));
    assert!(line("slots").split_whitespace().any(|s| s == "255/1"), "the player (255) is not model 1: {}", line("slots"));
    // gen 6, version 1, the girl (the field may show the bedroom's pyjamas already)
    let applied = log.lines().find(|l| l.starts_with("character: look applied ")).unwrap_or_else(|| panic!("{log}"));
    assert!(applied.starts_with("character: look applied 060101") && applied.ends_with(" (new game)"), "{applied}");
    let top = std::env::temp_dir().join("redex_character_new_game_top.png");
    std::fs::copy(data.join("logs/top.png"), &top).unwrap();
    println!("the top screen at control: {}", top.display());
    let _ = std::fs::remove_dir_all(&data);
}

/// start-screen plan 2 Task 7 (Done 3): a look the game changes in X (XR_SET_FASHION stands in for a boutique) is kept
/// in characters.json, and Y played as the same character wears it. Y as in the pair tests (XR_CCI_Y, XR_DATA_Y,
/// XR_SAVE_Y; skips without them): its user folder with the 100% save in Y's title folder.
#[test]
#[ignore]
fn a_boutique_change_reaches_the_other_game() {
    use xr_frontend::start::characters::{user_dir, Store};
    let Some((data, cci)) = own_data("character_boutique") else { return };
    let Some(id) = character_in(&data, CONTINUE_LOOK, "Calem", true, "a_boutique_change_reaches_the_other_game") else { return };
    let root = std::path::Path::new(&std::env::var("XR_DATA").unwrap()).join("..");
    let cci_y = std::env::var("XR_CCI_Y").unwrap_or_else(|_| std::path::Path::new(&cci).with_file_name("Pokemon Y (USA) (En,Ja,Fr,De,Es,It,Ko) Decrypted.3ds").to_string_lossy().into_owned());
    let data_y = std::env::var("XR_DATA_Y").map(std::path::PathBuf::from).unwrap_or_else(|_| root.join("play-online/y-online"));
    let ysave = std::env::var("XR_SAVE_Y").map(std::path::PathBuf::from).unwrap_or_else(|_| root.join("work/xy-cross/ysave/user"));
    let dll_y = data_y.join("build/y-usa-v0.2.0/xgame.dll");
    if ![std::path::PathBuf::from(&cci_y), dll_y.clone(), ysave.clone()].iter().all(|p| p.exists()) {
        return println!("skipping: Y's game, xgame.dll or save is missing ({cci_y}, {}, {})", dll_y.display(), ysave.display());
    }
    // 1. X: the outfit changes in the field (Red's pyjamas, not the character's clothes)
    let new = xr_net::x::field::Appearance { gender: 0, fashion: [0x48, 0x04, 0x09, 0x01, 0x0A, 0x6A, 0x00, 0x00, 0x41, 0, 0, 0, 0, 0, 0, 0] };
    assert!(new.validate() && new != CONTINUE_LOOK.appearance().unwrap());
    let script = script_until("character_boutique", "mp-field-walk.txt", "pokecenter");
    let log = character_run(&data, &cci, "x-usa-v0.2.0", id, &script, &[("XR_SET_FASHION", &format!("{}@1000", hex(&new.fashion)))]);
    assert!(log.contains(&format!("character: the game changed the look to {}", hex(&new.to_bytes()))), "{log}");
    // 2. kept, whole
    let kept = new;
    assert_ne!(kept, CONTINUE_LOOK.appearance().unwrap());
    let c = Store::load(&data).get(id).cloned().unwrap();
    assert_eq!((hex(&c.look), c.created), (hex(&kept.to_bytes()), 1));
    assert!(c.changed > c.created, "{c:?}");
    // 3. Y as the same character: the 100% save in Y's folder
    let user = user_dir(&data, id);
    let mut stack = vec![(ysave.clone(), user.clone())];
    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to).unwrap();
        for e in std::fs::read_dir(&from).unwrap().map(Result::unwrap) {
            if e.file_type().unwrap().is_dir() {
                stack.push((e.path(), to.join(e.file_name())));
            } else {
                std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
            }
        }
    }
    let main_y = user.join("savedata/0004000000055E00/main");
    let full = std::fs::read(user.join("savedata/0004000000055D00/main")).unwrap();
    let _ = std::fs::remove_file(&main_y);
    std::fs::write(&main_y, full).unwrap();
    std::fs::create_dir_all(data.join("build/y-usa-v0.2.0")).unwrap();
    std::fs::copy(&dll_y, data.join("build/y-usa-v0.2.0/xgame.dll")).unwrap();
    let log = character_run(&data, &cci_y, "y-usa-v0.2.0", id, &script, &[]);
    assert!(log.contains(&format!("character: look applied {} (continue)", hex(&kept.to_bytes()))), "{log}");
    let _ = std::fs::remove_dir_all(&data);
}

// ---- start-screen plan 2 Task 9: characters on the account ----------------------------------------------------------

/// start-screen plan 2 Task 9, the X+Y pair: a character made on X (in X's characters.json, no save yet) goes to the
/// account when X logs in to the world; Y on another PC (its own data folder, no characters.json) logged in as the same
/// player (the dev id copied over) gets it into its characters.json. Y as in the pair tests (XR_CCI_Y, XR_DATA_Y;
/// skips without them).
#[test]
#[ignore]
#[cfg(feature = "mp-tests")] // --dev-id, and a headless run in a world
fn a_character_made_on_x_shows_on_y_after_login() {
    use xr_frontend::start::characters::Store;
    let Some((data, cci)) = own_data("account_x") else { return };
    let root = std::path::Path::new(&std::env::var("XR_DATA").unwrap()).join("..");
    let cci_y = std::env::var("XR_CCI_Y").unwrap_or_else(|_| std::path::Path::new(&cci).with_file_name("Pokemon Y (USA) (En,Ja,Fr,De,Es,It,Ko) Decrypted.3ds").to_string_lossy().into_owned());
    let dll_y = std::env::var("XR_DATA_Y").map(std::path::PathBuf::from).unwrap_or_else(|_| root.join("play-online/y-online")).join("build/y-usa-v0.2.0/xgame.dll");
    if !std::path::Path::new(&cci_y).exists() || !dll_y.exists() {
        return println!("skipping: Y's game or xgame.dll is missing ({cci_y}, {})", dll_y.display());
    }
    let accounts = std::env::temp_dir().join(format!("redex_account_server_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&accounts);
    let t = redex_world::transport::TcpTransport::bind("127.0.0.1:0").unwrap();
    let addr = t.local_addr().unwrap().to_string();
    let _server = redex_world::Server::start(redex_world::Config { dev: true, data: accounts.clone(), ..Default::default() }, t);
    let id = character_in(&data, CONTINUE_LOOK, "Calem", false, "a_character_made_on_x_shows_on_y_after_login").unwrap();
    let run = |data: &std::path::Path, cci: &str, build: &str, extra: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_redex"))
            .arg("--cci").arg(cci).arg("--build").arg(data.join("build").join(build))
            .args(["--headless", "--frames", "1200", "--world", &addr, "--dev-id"]).args(extra)
            .output().unwrap();
        let log = std::fs::read_to_string(data.join("logs/latest.log")).unwrap_or_else(|_| String::from_utf8_lossy(&o.stderr).into_owned());
        assert_eq!(o.status.code(), Some(0), "{log}");
        assert!(log.contains("world: logged in") && !log.contains("panic"), "{log}");
        log
    };
    // 1. X, played as the character: its login puts it to the account
    run(&data, &cci, "x-usa-v0.2.0", &["--character", &format!("{id:016x}")]);
    let key = xr_net::id::load_or_create(&data.join("user")).unwrap() & !(1 << 127);
    let stored = redex_world::accounts::load(&accounts, key).unwrap();
    assert_eq!(stored.iter().map(|c| (c.id, c.name.as_str())).collect::<Vec<_>>(), [(id, "Calem")], "the account after X");
    // 2. Y on another PC, the same player: the account's list comes down into its characters.json
    let data_y = std::env::temp_dir().join(format!("redex_account_y_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_y);
    std::fs::create_dir_all(data_y.join("build/y-usa-v0.2.0")).unwrap();
    std::fs::create_dir_all(data_y.join("logs")).unwrap();
    std::fs::create_dir_all(data_y.join("user")).unwrap();
    std::fs::copy(&dll_y, data_y.join("build/y-usa-v0.2.0/xgame.dll")).unwrap();
    std::fs::copy(data.join("user/redex-id"), data_y.join("user/redex-id")).unwrap();
    assert!(!data_y.join("characters.json").exists());
    let log = run(&data_y, &cci_y, "y-usa-v0.2.0", &[]);
    assert!(log.contains("characters: the account's newer copies kept (1 character(s))"), "{log}");
    let c = Store::load(&data_y).get(id).cloned().unwrap_or_else(|| panic!("no character {id:016x} on Y\n{log}"));
    assert_eq!((c.name.as_str(), hex(&c.look)), ("Calem", hex(&CONTINUE_LOOK.appearance().unwrap().to_bytes())));
    assert_eq!(c, Store::load(&data).get(id).cloned().unwrap(), "the same character, created and changed included");
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&data_y);
    let _ = std::fs::remove_dir_all(&accounts);
}
