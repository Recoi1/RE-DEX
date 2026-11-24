//! The recompiled game: xgame.dll.
use crate::abi::{XrFn, XrGameInfo, XrRuntime, XR_ABI_VERSION};
use std::path::Path;

pub struct Game {
    lib: libloading::Library,
}

/// `XrGameInfo.hooks_hash`: FNV-1a 64 of the hooked addresses (u32 LE, in hooks.txt order), as xr-recomp makes it.
pub fn hooks_hash(hooks: &[u32]) -> u64 {
    xr_hle::cro::code_hash(&hooks.iter().flat_map(|a| a.to_le_bytes()).collect::<Vec<u8>>())
}

/// The CRO modules compiled into xgame.dll: `xr_place_module`, `xr_unplace_module`, and each module's index and
/// code hash by its name.
pub struct Modules {
    pub place: unsafe extern "C" fn(u32, u32) -> u32,
    pub unplace: unsafe extern "C" fn(u32),
    pub by_name: std::collections::HashMap<String, (u32, u64)>,
}

impl Game {
    /// `rt` must outlive the Game: the DLL keeps the pointer.
    pub fn open(path: &Path, rt: &XrRuntime) -> Result<Self, String> {
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| format!("{}: {e}", path.display()))?;
        let init: libloading::Symbol<unsafe extern "C" fn(*const XrRuntime) -> u32> =
            unsafe { lib.get(b"xr_game_init\0") }.map_err(|e| format!("xr_game_init: {e}"))?;
        let v = unsafe { init(rt) };
        if v != XR_ABI_VERSION {
            return Err(format!("{}: ABI version {v}, expected {XR_ABI_VERSION}; rebuild it with xr-recomp for this redex", path.display()));
        }
        drop(init);
        Ok(Self { lib })
    }

    /// Refuses a DLL compiled from another dump: its `xr_game_info` must name this `code` (code.bin) and title.
    /// Returns its build id.
    pub fn check(&self, code: &[u8], title_id: u64) -> Result<String, String> {
        use sha2::Digest;
        let rebuild = "rebuild it with xr-recomp from this dump";
        let info = unsafe { self.lib.get::<*const XrGameInfo>(b"xr_game_info\0") }
            .map_err(|_| format!("xgame.dll has no xr_game_info (built before the dump check); {rebuild}"))?;
        let info = unsafe { &**info };
        let build = if info.build.is_null() { "?".into() } else { unsafe { std::ffi::CStr::from_ptr(info.build) }.to_string_lossy() };
        let sha: [u8; 32] = sha2::Sha256::digest(code).into();
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        if info.code_sha256 != sha || info.title_id != title_id {
            let game = |t: u64| match t {
                0x0004000000055D00 => "Pokémon X",
                0x0004000000055E00 => "Pokémon Y",
                _ => "an unknown game",
            };
            return Err(format!(
                "this game build ({build}: {}, title {:016X}, code.bin SHA-256 {}) was made from a different dump than the game \
                 given ({}, title {title_id:016X}, code.bin SHA-256 {}); use the build made for this game, or {rebuild}",
                game(info.title_id),
                info.title_id,
                hex(&info.code_sha256),
                game(title_id),
                hex(&sha)
            ));
        }
        // the DLL calls hook k for the k-th function of its hooks.txt, and the runtime fills slot k for HOOKED[k]: the
        // lists must be the same (a DLL that hooks nothing calls no slot)
        let ours = hooks_hash(&crate::hooks::HOOKED);
        if info.hooks_hash != ours && info.hooks_hash != hooks_hash(&[]) {
            return Err(format!(
                "this game build ({build}) was made with another hook list (hooks.txt, hash {:016X}) than this redex fills ({} hooks, \
                 hash {ours:016X}); use the build made with this redex, or {rebuild} and this redex's functions/{build}/hooks.txt",
                info.hooks_hash,
                crate::hooks::HOOKED.len()
            ));
        }
        crate::log::log(&format!("xgame.dll: build {build}, made from this dump's code.bin (SHA-256 {})", hex(&sha)));
        Ok(build.into_owned())
    }

    /// The DLL's CRO modules (ABI 5), valid while the DLL stays loaded.
    pub fn modules(&self) -> Result<Modules, String> {
        let get = |n: &[u8]| unsafe { self.lib.get::<*const u8>(n) }.map(|s| *s).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(&n[..n.len() - 1])));
        let count = unsafe { *(get(b"xr_module_count\0")? as *const u32) } as usize;
        let names = get(b"xr_module_names\0")? as *const *const std::ffi::c_char;
        let hashes = get(b"xr_module_hash\0")? as *const u64;
        let place = unsafe { self.lib.get::<unsafe extern "C" fn(u32, u32) -> u32>(b"xr_place_module\0") }.map(|f| *f).map_err(|e| format!("xr_place_module: {e}"))?;
        let unplace = unsafe { self.lib.get::<unsafe extern "C" fn(u32)>(b"xr_unplace_module\0") }.map(|f| *f).map_err(|e| format!("xr_unplace_module: {e}"))?;
        let by_name = (0..count)
            .map(|k| unsafe {
                let name = std::ffi::CStr::from_ptr(*names.add(k)).to_string_lossy().into_owned();
                (name, (k as u32, *hashes.add(k)))
            })
            .collect();
        Ok(Modules { place, unplace, by_name })
    }

    /// The DLL's `xr_lookup`, valid while the DLL stays loaded.
    pub fn lookup_fn(&self) -> Result<unsafe extern "C" fn(u32) -> XrFn, String> {
        unsafe { self.lib.get::<unsafe extern "C" fn(u32) -> XrFn>(b"xr_lookup\0") }.map(|f| *f).map_err(|e| format!("xr_lookup: {e}"))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    /// Compile `src` (C, includes xr_abi.h) into a fresh temp dir; returns the dir. Needs clang (env.ps1).
    pub fn build_dll(name: &str, src: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("redex_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = dir.join("t.c");
        std::fs::write(&c, src).unwrap();
        let abi = concat!(env!("CARGO_MANIFEST_DIR"), "/../../abi");
        let o = Command::new("clang").args(["-O1", "-shared", "-I", abi]).arg(&c).arg("-o").arg(dir.join("xgame.dll")).output().unwrap();
        assert!(o.status.success(), "clang: {}", String::from_utf8_lossy(&o.stderr));
        dir
    }

    #[test]
    fn abi_version_mismatch() {
        let src = r#"#include "xr_abi.h"
uint8_t* xr_mem; const XrRuntime* xr_rt;
__declspec(dllexport) uint32_t xr_game_init(const XrRuntime* rt) { return 99; }
__declspec(dllexport) XrFn xr_lookup(uint32_t a) { return 0; }"#;
        let dir = build_dll("mismatch", src);
        let rt = XrRuntime { abi_version: XR_ABI_VERSION, mem: std::ptr::null_mut(), svc: None, interp: None, preempt: None, hooks: std::ptr::null_mut() };
        let e = Game::open(&dir.join("xgame.dll"), &rt).err().expect("must fail");
        assert!(e.ends_with(&format!("xgame.dll: ABI version 99, expected {XR_ABI_VERSION}; rebuild it with xr-recomp for this redex")), "{e}");
    }
}
