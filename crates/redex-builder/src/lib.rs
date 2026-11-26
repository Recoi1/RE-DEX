//! The build pipeline behind the start screen: check -> unpack -> recompile -> build -> verify, with progress
//! events, cancel and resume (ninja rebuilds only what changed, so a rerun on the same folder resumes).
mod stages;

pub use stages::{free_bytes, Event, RunTool, Seams, Stage};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

/// Where the build tools live. `bin` goes first on the children's PATH and is their `XR_TOOLS`.
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub bin: PathBuf,
    pub functions: PathBuf,
    pub recomp: PathBuf,
    pub redex: PathBuf,
}

impl Toolchain {
    /// Reads the layout `tools/release/make-toolchain.ps1` builds: `<release>/toolchain/{bin,functions}`,
    /// `<release>/xr-recomp.exe`, `<release>/redex.exe`.
    pub fn from_release_dir(release: &Path) -> Result<Toolchain, String> {
        let t = Toolchain {
            bin: release.join("toolchain").join("bin"),
            functions: release.join("toolchain").join("functions"),
            recomp: release.join("xr-recomp.exe"),
            redex: release.join("redex.exe"),
        };
        let mut missing: Vec<String> = ["clang.exe", "ld.lld.exe", "ninja.exe", "ctrtool.exe"]
            .iter()
            .map(|f| t.bin.join(f))
            .chain([t.recomp.clone(), t.redex.clone()])
            .filter(|p| !p.is_file())
            .map(|p| p.display().to_string())
            .collect();
        // The clang .cfg files select compiler-rt and lld; without them linking fails.
        let has_cfg = std::fs::read_dir(&t.bin)
            .map(|d| d.flatten().any(|e| e.path().extension().is_some_and(|x| x == "cfg")))
            .unwrap_or(false);
        if !has_cfg {
            missing.push(format!("{}\\*.cfg (clang configuration files)", t.bin.display()));
        }
        if !t.functions.is_dir() {
            missing.push(t.functions.display().to_string());
        }
        if missing.is_empty() { Ok(t) } else { Err(format!("the build tools are incomplete, missing: {}", missing.join(", "))) }
    }

    /// What made a build: xr-recomp (its version, and its exe's size and time, which change with any new release) and
    /// the game DLL's ABI. A build verified with another stamp is `Outdated`.
    pub fn stamp(&self) -> String {
        let m = std::fs::metadata(&self.recomp).ok();
        let len = m.as_ref().map_or(0, |m| m.len());
        let at = m.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        // the hook list is data next to xr-recomp, so a release that only changes it must still rebuild games
        format!("xr-recomp {} {len} {at} {:016x}\nabi {}", env!("CARGO_PKG_VERSION"), hooks_hash(&self.functions), abi_version())
    }
}

/// FNV-1a over every `<functions>/*/hooks.txt`, in name order
fn hooks_hash(functions: &std::path::Path) -> u64 {
    let mut files: Vec<_> = std::fs::read_dir(functions).into_iter().flatten().flatten().map(|e| e.path().join("hooks.txt")).filter(|p| p.is_file()).collect();
    files.sort();
    let mut h = 0xcbf29ce484222325u64;
    for b in files.iter().flat_map(|f| std::fs::read(f).unwrap_or_default()) {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

/// XR_ABI_VERSION from the header xr-recomp compiles the game against
fn abi_version() -> u32 {
    include_str!("../../../abi/xr_abi.h")
        .lines()
        .find_map(|l| l.strip_prefix("#define XR_ABI_VERSION ")?.split_whitespace().next()?.parse().ok())
        .expect("xr_abi.h defines XR_ABI_VERSION")
}

/// `<build>/verified.txt`, written last by the verify stage
pub(crate) fn verified_text(build_id: &str, stamp: &str) -> String {
    format!("{build_id}\n{stamp}\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildState {
    Missing,
    Partial,
    /// built and verified, but by older tools: it needs an update before it plays
    Outdated,
    Ready,
}

/// Ready = the DLL exists and the verify stage's marker was written (it is written last) by the tools whose `stamp`
/// is given (None: any tools, when there are none to build with).
pub fn build_state(data: &Path, build_id: &str, stamp: Option<&str>) -> BuildState {
    let dir = data.join("build").join(build_id);
    let verified = std::fs::read_to_string(dir.join("verified.txt")).ok();
    if let (true, Some(v)) = (dir.join("xgame.dll").is_file(), verified) {
        match stamp {
            Some(s) if v != verified_text(build_id, s) => BuildState::Outdated,
            _ => BuildState::Ready,
        }
    } else if dir.is_dir() {
        BuildState::Partial
    } else {
        BuildState::Missing
    }
}

/// A save state (xr-runtime `Machine::save_state`) starts with this magic and the SHA-256 of the xgame.dll that made it.
pub const STATE_MAGIC: &[u8; 8] = b"REDEXST2";

/// The SHA-256 of `dll`: a save state loads only into the xgame.dll whose fingerprint it carries.
pub fn dll_fingerprint(dll: &Path) -> Result<[u8; 32], String> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    std::fs::File::open(dll).and_then(|mut f| std::io::copy(&mut f, &mut h)).map_err(|e| format!("{}: {e}", dll.display()))?;
    Ok(h.finalize().into())
}

/// Whether the save state file `state` was made with `dll` (and in today's format).
pub fn state_matches(state: &Path, dll: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 40];
    let read = std::fs::File::open(state).and_then(|mut f| f.read_exact(&mut head)).is_ok();
    read && head[..8] == STATE_MAGIC[..] && dll_fingerprint(dll).is_ok_and(|fp| head[8..] == fp)
}

/// Windows job object that kills everything in it when closed (our exit or crash included).
#[cfg(windows)]
pub struct Job(pub(crate) windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for Job {}
#[cfg(windows)]
unsafe impl Sync for Job {}

#[cfg(windows)]
impl Job {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Job {
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(h, JobObjectExtendedLimitInformation, &info as *const _ as _, std::mem::size_of_val(&info) as u32);
            Job(h)
        }
    }

    /// shortcut: a grandchild spawned before this call escapes the job; the child needs ~ms to start any.
    pub fn assign(&self, child: &std::process::Child) {
        unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(self.0, std::os::windows::io::AsRawHandle::as_raw_handle(child) as _) };
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

pub(crate) struct Ctl {
    pub cancelled: AtomicBool,
    pub pid: Mutex<Option<u32>>,
    #[cfg(windows)]
    pub job: Job,
}

pub struct BuildJob {
    rx: Receiver<Event>,
    ctl: Arc<Ctl>,
}

impl BuildJob {
    pub fn start(dump: PathBuf, data: PathBuf, tools: Toolchain, jobs: u32) -> BuildJob {
        Self::start_with(dump, data, tools, jobs, Seams::default())
    }

    /// `start` with the identify/unpack/free-space steps replaceable (tests use fakes).
    pub fn start_with(dump: PathBuf, data: PathBuf, tools: Toolchain, jobs: u32, seams: Seams) -> BuildJob {
        let (tx, rx) = mpsc::channel();
        let ctl = Arc::new(Ctl {
            cancelled: AtomicBool::new(false),
            pid: Mutex::new(None),
            #[cfg(windows)]
            job: Job::new(),
        });
        let c = ctl.clone();
        std::thread::spawn(move || stages::run(&dump, &data, &tools, jobs, &seams, &c, &tx));
        BuildJob { rx, ctl }
    }

    pub fn events(&self) -> &Receiver<Event> {
        &self.rx
    }

    /// Stops the running tool and its children; the job ends with `Failed { message: "cancelled" }`.
    pub fn cancel(&self) {
        self.ctl.cancelled.store(true, Ordering::SeqCst);
        stages::kill(&self.ctl);
    }
}

impl Drop for BuildJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_names_the_abi_and_changes_with_the_recompiler() {
        let dir = std::env::temp_dir().join(format!("redex-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = Toolchain { bin: dir.clone(), functions: dir.clone(), recomp: dir.join("xr-recomp.exe"), redex: dir.join("redex.exe") };
        std::fs::write(&t.recomp, b"one").unwrap();
        let a = t.stamp();
        assert!(a.ends_with(&format!("abi {}", abi_version())) && abi_version() > 0, "{a}");
        std::fs::write(&t.recomp, b"a newer one").unwrap();
        let b = t.stamp();
        assert_ne!(a, b);
        // a new hook list alone also changes it
        std::fs::create_dir_all(dir.join("x")).unwrap();
        std::fs::write(dir.join("x").join("hooks.txt"), b"0x00100000\n").unwrap();
        assert_ne!(b, t.stamp());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn state_header_names_its_dll() {
        let dir = std::env::temp_dir().join(format!("redex-state-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (dll, state) = (dir.join("xgame.dll"), dir.join("a.state"));
        std::fs::write(&dll, b"abc").unwrap();
        let fp = dll_fingerprint(&dll).unwrap();
        assert_eq!(fp[..4], [0xBA, 0x78, 0x16, 0xBF], "SHA-256 of the file");
        std::fs::write(&state, [&b"REDEXST2"[..], &fp, b"x"].concat()).unwrap();
        assert!(state_matches(&state, &dll));
        std::fs::write(&dll, b"abd").unwrap();
        assert!(!state_matches(&state, &dll), "one DLL byte changed");
        std::fs::write(&state, [&b"REDEXST1"[..], &dll_fingerprint(&dll).unwrap(), b"x"].concat()).unwrap();
        assert!(!state_matches(&state, &dll), "an older state");
        let _ = std::fs::remove_dir_all(dir);
    }
}
