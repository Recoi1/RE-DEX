//! Rust mirror of abi/xr_abi.h. Keep in sync; `abi_layout` checks it against clang.
use std::ffi::c_void;

pub const XR_ABI_VERSION: u32 = 6; // v4: XrCpu.ret_lr/ret_sp; v5: CRO modules (xr_place_module & co.); v6: XrGameInfo.hooks_hash

#[repr(C)]
#[derive(Clone, Copy)]
pub union XrVfp {
    pub s: [f32; 32],
    pub d: [f64; 16],
    pub u: [u32; 32],
}

#[repr(C)]
pub struct XrCpu {
    pub r: [u32; 16],
    pub cpsr: u32,
    pub v: XrVfp,
    pub fpscr: u32,
    pub excl_addr: u32,
    pub excl_valid: u32,
    pub thread: *mut c_void,
    pub tls: u32,
    pub budget: i32,
    pub ret_lr: u32,
    pub ret_sp: u32,
}

pub type XrFn = Option<unsafe extern "C" fn(*mut XrCpu)>;

#[repr(C)]
pub struct XrRuntime {
    pub abi_version: u32,
    pub mem: *mut u8,
    pub svc: Option<unsafe extern "C" fn(*mut XrCpu, u32)>,
    pub interp: Option<unsafe extern "C" fn(*mut XrCpu, u32, u32, u32)>,
    pub preempt: Option<unsafe extern "C" fn(*mut XrCpu)>,
    pub hooks: *mut XrFn,
}

#[repr(C)]
pub struct XrGameInfo {
    pub code_sha256: [u8; 32],
    pub title_id: u64,
    pub build: *const std::ffi::c_char,
    pub hooks_hash: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    use std::process::Command;

    #[test]
    fn abi_layout() {
        let dir = std::env::temp_dir().join(format!("xr_abi_layout_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let abi = concat!(env!("CARGO_MANIFEST_DIR"), "/../../abi");
        let src = format!(
            r#"#include <stdio.h>
#include <stddef.h>
#include "xr_abi.h"
int main(void) {{
#define P(n, v) printf(n " %zu\n", (size_t)(v))
P("size_XrCpu", sizeof(XrCpu)); P("size_XrRuntime", sizeof(XrRuntime));
P("cpu.r", offsetof(XrCpu, r)); P("cpu.cpsr", offsetof(XrCpu, cpsr)); P("cpu.v", offsetof(XrCpu, v));
P("cpu.fpscr", offsetof(XrCpu, fpscr)); P("cpu.excl_addr", offsetof(XrCpu, excl_addr));
P("cpu.excl_valid", offsetof(XrCpu, excl_valid)); P("cpu.thread", offsetof(XrCpu, thread)); P("cpu.tls", offsetof(XrCpu, tls));
P("cpu.budget", offsetof(XrCpu, budget));
P("cpu.ret_lr", offsetof(XrCpu, ret_lr)); P("cpu.ret_sp", offsetof(XrCpu, ret_sp));
P("rt.abi_version", offsetof(XrRuntime, abi_version)); P("rt.mem", offsetof(XrRuntime, mem));
P("rt.svc", offsetof(XrRuntime, svc)); P("rt.interp", offsetof(XrRuntime, interp));
P("rt.preempt", offsetof(XrRuntime, preempt));
P("rt.hooks", offsetof(XrRuntime, hooks));
P("size_XrGameInfo", sizeof(XrGameInfo)); P("info.title_id", offsetof(XrGameInfo, title_id)); P("info.build", offsetof(XrGameInfo, build)); P("info.hooks_hash", offsetof(XrGameInfo, hooks_hash));
return 0; }}
"#
        );
        let c = dir.join("layout.c");
        std::fs::write(&c, src).unwrap();
        let exe = dir.join("layout.exe");
        match Command::new("clang").arg("-I").arg(abi).arg(&c).arg("-o").arg(&exe).output() {
            Err(e) => {
                println!("skipping abi_layout: clang not found ({e})");
                return;
            }
            Ok(o) => assert!(o.status.success(), "clang failed: {}", String::from_utf8_lossy(&o.stderr)),
        }
        let out = String::from_utf8(Command::new(&exe).output().unwrap().stdout).unwrap();
        println!("{out}");
        let got = |k: &str| -> usize {
            out.lines()
                .find_map(|l| l.strip_prefix(k).and_then(|r| r.trim().parse().ok()))
                .unwrap_or_else(|| panic!("missing {k}"))
        };
        let pairs = [
            ("size_XrCpu ", size_of::<XrCpu>()),
            ("size_XrRuntime ", size_of::<XrRuntime>()),
            ("cpu.r ", offset_of!(XrCpu, r)),
            ("cpu.cpsr ", offset_of!(XrCpu, cpsr)),
            ("cpu.v ", offset_of!(XrCpu, v)),
            ("cpu.fpscr ", offset_of!(XrCpu, fpscr)),
            ("cpu.excl_addr ", offset_of!(XrCpu, excl_addr)),
            ("cpu.excl_valid ", offset_of!(XrCpu, excl_valid)),
            ("cpu.thread ", offset_of!(XrCpu, thread)),
            ("cpu.tls ", offset_of!(XrCpu, tls)),
            ("cpu.budget ", offset_of!(XrCpu, budget)),
            ("cpu.ret_lr ", offset_of!(XrCpu, ret_lr)),
            ("cpu.ret_sp ", offset_of!(XrCpu, ret_sp)),
            ("rt.abi_version ", offset_of!(XrRuntime, abi_version)),
            ("rt.mem ", offset_of!(XrRuntime, mem)),
            ("rt.svc ", offset_of!(XrRuntime, svc)),
            ("rt.interp ", offset_of!(XrRuntime, interp)),
            ("rt.preempt ", offset_of!(XrRuntime, preempt)),
            ("rt.hooks ", offset_of!(XrRuntime, hooks)),
            ("size_XrGameInfo ", size_of::<XrGameInfo>()),
            ("info.title_id ", offset_of!(XrGameInfo, title_id)),
            ("info.build ", offset_of!(XrGameInfo, build)),
            ("info.hooks_hash ", offset_of!(XrGameInfo, hooks_hash)),
        ];
        for (k, rust) in pairs {
            assert_eq!(got(k), rust, "{k}");
        }
    }
}
