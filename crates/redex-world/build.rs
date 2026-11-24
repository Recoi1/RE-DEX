//! Puts steam_api64.dll (Valve's Steamworks SDK redistributable, see NOTICE) next to the binaries in target/<profile>/:
//! redex-world.exe loads it at start, redex.exe when Steam is first used. The DLL is the one in the steamworks-sys
//! source of the exact version Cargo.lock names (it matches that crate's bindings; the SDK 1.65 DLL lacks interfaces
//! those bindings import). Shared by xr-net (`build = ...`).
use std::path::PathBuf;

fn main() {
    if !std::env::var("TARGET").unwrap_or_default().contains("windows") {
        return;
    }
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("Cargo.lock");
    println!("cargo::rerun-if-changed={}", root.join("Cargo.lock").display());
    let version = lock
        .split("[[package]]")
        .find(|p| p.contains("name = \"steamworks-sys\""))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")?.strip_suffix('"')))
        .expect("steamworks-sys in Cargo.lock");
    let home = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("USERPROFILE").unwrap()).join(".cargo"));
    let dll = std::fs::read_dir(home.join("registry/src"))
        .expect("cargo's registry sources")
        .flatten()
        .map(|index| index.path().join(format!("steamworks-sys-{version}/lib/steam/redistributable_bin/win64/steam_api64.dll")))
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("steam_api64.dll of steamworks-sys {version} under {}", home.display()));
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // OUT_DIR is <profile>/build/<crate>[/<hash>]/out
    let build = out.ancestors().find(|a| a.file_name().is_some_and(|n| n == "build")).expect("OUT_DIR under <profile>/build");
    let to = build.parent().unwrap().join("steam_api64.dll");
    if std::fs::read(&to).ok() != std::fs::read(&dll).ok() {
        // a running redex holds the old copy open: keep it rather than fail the build
        if let Err(e) = std::fs::copy(&dll, &to) {
            println!("cargo::warning=could not copy {} to {}: {e}", dll.display(), to.display());
        }
    }
}
