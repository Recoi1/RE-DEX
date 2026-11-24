//! Full recompile of the real code.bin into xgame.dll (skips without XR_DUMP). Output goes to
//! `<XR_DATA>/build/<dump dir name>` (temp dir without XR_DATA), never into the repo.
use std::path::PathBuf;
use std::process::Command;

#[test]
fn recompile_code_bin() {
    let Ok(dump) = std::env::var("XR_DUMP") else {
        println!("skipping recompile_code_bin: XR_DUMP not set");
        return;
    };
    let dump = PathBuf::from(dump);
    let functions = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../functions/x-usa-v0.2.0"));
    let data = std::env::var("XR_DATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("xr_recomp_test"));
    let out = data.join("build").join(dump.file_name().unwrap());

    let st = Command::new(env!("CARGO_BIN_EXE_xr-recomp"))
        .arg("--dump").arg(&dump)
        .arg("--functions").arg(&functions)
        .arg("--out").arg(&out)
        .status()
        .unwrap();
    assert!(st.success(), "xr-recomp exited with {st:?}");
    assert!(out.join("xgame.dll").is_file(), "no xgame.dll in {}", out.display());

    let listed = std::fs::read_to_string(functions.join("code.txt"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .count();
    let report = std::fs::read_to_string(out.join("report.txt")).unwrap();
    let field = |k: &str| -> usize {
        let line = report.lines().find(|l| l.starts_with(&format!("{k}: "))).unwrap_or_else(|| panic!("no '{k}:' in report.txt"));
        line[k.len() + 2..].split_whitespace().next().unwrap().parse().unwrap()
    };
    assert_eq!(field("listed"), listed);
    assert_eq!(field("functions"), listed - field("dropped") + field("added"));
    assert_eq!(field("dropped"), 0, "the three one-byte Ghidra entries are now corrected in code.txt");
    assert!(field("added") > 0);

    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("progress-build.json")).unwrap()).unwrap();
    assert_eq!(json["dll_built"], true);
    assert_eq!(json["functions_listed"], listed);
    assert_eq!(json["functions_generated"], field("functions"));
    assert_eq!(json["functions_added_from_branch_targets"], field("added"));
    let undec = json["undecodable"].as_array().unwrap();
    assert!(undec.iter().all(|u| u["addr"] != "0x00100134"), "0x00100134 still undecodable");
    assert!(undec.iter().all(|u| u["reason"].as_str().is_some_and(|r| !r.is_empty())));
    assert_eq!(undec.len(), field("undecodable"));

    // Load the DLL and probe the lookup table: odd = Thumb only, even = ARM only, mid-function and out of range NULL.
    let smoke = r#"#include <windows.h>
#include <stdio.h>
#include <string.h>
#include "xr_abi.h"
int main(void) {
    HMODULE m = LoadLibraryA("xgame.dll");
    if (!m) return 10;
    uint32_t (*init)(const XrRuntime*) = (void*)GetProcAddress(m, "xr_game_init");
    XrFn (*look)(uint32_t) = (void*)GetProcAddress(m, "xr_lookup");
    if (!init || !look) return 11;
    static XrRuntime rt;
    if (init(&rt) != XR_ABI_VERSION) return 12;
    const XrGameInfo* gi = (const XrGameInfo*)GetProcAddress(m, "xr_game_info");
    if (!gi || gi->title_id != 0x0004000000055D00ull || strcmp(gi->build, "x-usa-v0.2.0")) return 14;
    int ok = look(0x00100000) && !look(0x00100001)      /* ARM 0x00100000 */
        && look(0x00100049) && !look(0x00100048)        /* Thumb 0x00100048 */
        && look(0x001001AC)                             /* added from a branch target */
        && !look(0x00100004) && !look(0) && !look(0xFFFFFFFF) && !look(0x0053BEDC);
    puts(ok ? "lookup ok" : "lookup wrong");
    return ok ? 0 : 13;
}
"#;
    std::fs::write(out.join("smoke.c"), smoke).unwrap();
    let st = Command::new("clang").current_dir(&out).args(["-O1", "smoke.c", "-o", "smoke.exe"]).status().unwrap();
    assert!(st.success(), "clang smoke.c");
    let st = Command::new(out.join("smoke.exe")).current_dir(&out).status().unwrap();
    assert_eq!(st.code(), Some(0), "lookup probe failed");
}

/// `--jobs N` must reach ninja as `-j N`: a stub `ninja.exe` first on PATH records its args.
#[test]
fn jobs_flag_reaches_ninja() {
    let Ok(dump) = std::env::var("XR_DUMP") else {
        println!("skipping jobs_flag_reaches_ninja: XR_DUMP not set");
        return;
    };
    let functions = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../functions/x-usa-v0.2.0"));
    let tmp = std::env::temp_dir().join("xr_recomp_jobs_test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("stub.rs"), r#"fn main() { std::fs::write("ninja-args.txt", std::env::args().skip(1).collect::<Vec<_>>().join(" ")).unwrap(); }"#).unwrap();
    let st = Command::new("rustc").current_dir(&tmp).args(["stub.rs", "-o", "ninja.exe"]).status().unwrap();
    assert!(st.success(), "rustc stub");

    let out = tmp.join("out");
    let path = format!("{};{}", tmp.display(), std::env::var("PATH").unwrap_or_default());
    Command::new(env!("CARGO_BIN_EXE_xr-recomp"))
        .env("PATH", path)
        .current_dir(&tmp)
        .arg("--dump").arg(&dump).arg("--functions").arg(&functions).arg("--out").arg(&out)
        .args(["--jobs", "3"])
        .status()
        .unwrap(); // exits 1 (the stub builds no DLL); only the recorded args matter
    let args = std::fs::read_to_string(tmp.join("ninja-args.txt")).expect("ninja was not run");
    assert!(args.contains("-j 3"), "ninja args were {args:?}");
}
