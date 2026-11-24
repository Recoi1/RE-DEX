//! redex.exe loads steam_api64.dll only when Steam is first used (`--world`), so offline play starts without it.
//! xr-net's `Steam::init` checks the DLL loads before the first Steam call (a missing DLL is "Start Steam ...").
fn main() {
    // gnullvm only: lld takes --delayload; GNU ld (windows-gnu) and MSVC do not
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("windows-gnullvm") {
        // lld's MinGW driver; the delay-load helper (__delayLoadHelper2) comes from mingw-w64's crt
        println!("cargo::rustc-link-arg-bins=-Wl,--delayload=steam_api64.dll");
    } else {
        println!("cargo::warning=redex is linked without delay-loading steam_api64.dll on {target}: it will need the DLL next to it even for offline play");
    }
}
