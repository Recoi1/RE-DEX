use crate::{build_state, verified_text, BuildState, Ctl, Toolchain};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use xr_intake::KnownBuild;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Check,
    Unpack,
    Recompile,
    Build,
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Stage(Stage),
    /// Check found which game the dump is
    Identified { build_id: &'static str },
    Progress { done: u32, total: u32 },
    Log(String),
    Done { build_id: &'static str, build_dir: PathBuf },
    /// the message's first line is for the player (what went wrong and one next step), the lines after it are details
    /// for the log; "cancelled" when the job was cancelled
    Failed { stage: Stage, message: String },
}

type Identify = fn(&Path) -> Result<(KnownBuild, String), xr_intake::Error>;

/// runs a tool's command in the build's job (cancel kills it): (success, what it said)
pub type RunTool<'a> = &'a mut dyn FnMut(Command) -> Result<(bool, String), String>;

/// Steps with outside effects, replaceable so tests need no real dump or disk.
#[derive(Clone, Copy)]
pub struct Seams {
    pub identify: Identify,
    /// (dump, data, its build, its SHA-256 hex, tools bin, runs ctrtool) -> unpacked dump folder
    pub unpack: fn(&Path, &Path, &KnownBuild, &str, &Path, RunTool) -> Result<PathBuf, xr_intake::Error>,
    pub free_bytes: fn(&Path) -> u64,
}

impl Default for Seams {
    fn default() -> Self {
        Seams {
            identify: xr_intake::identify_hash,
            unpack: |dump, data, build, hex, bin, run| xr_intake::unpack(dump, data, build, hex, Some(bin), run),
            free_bytes,
        }
    }
}

/// a game's unpacked dump and build folder take about 2.35 GB: 3 GB leaves room
const NEED_BYTES: u64 = 3_000_000_000;

const WRITE: &str = "We couldn't write to the RE:DEX data folder. Free up some disk space, then try again from the game's card.";
const AGAIN: &str = "Try again from the game's card.";

/// Free bytes on the volume holding `p` (or its nearest existing parent); u64::MAX when it can't be read.
#[cfg(windows)]
pub fn free_bytes(p: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    let mut dir = p;
    while !dir.exists() {
        match dir.parent() {
            Some(d) => dir = d,
            None => break,
        }
    }
    let w: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut avail = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
    if ok == 0 { u64::MAX } else { avail }
}

#[cfg(not(windows))]
pub fn free_bytes(_: &Path) -> u64 {
    u64::MAX // shortcut: the game is Windows-only, so only the Windows probe exists
}

/// bytes of the files under `p` (0 when it doesn't exist)
fn dir_bytes(p: &Path) -> u64 {
    std::fs::read_dir(p).map_or(0, |d| {
        d.flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => dir_bytes(&e.path()),
                _ => e.metadata().map_or(0, |m| m.len()),
            })
            .sum()
    })
}

/// Kills the job (the child and everything it spawned). Elsewhere only the direct child is killed.
pub(crate) fn kill(ctl: &Ctl) {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::JobObjects::TerminateJobObject(ctl.job.0, 1);
    }
    #[cfg(not(windows))]
    if let Some(pid) = *ctl.pid.lock().unwrap() {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

/// `<build_dir>/building.lock`, locked by the OS while a build runs: the lock goes when the process does, however it ends
struct Lock {
    _file: std::fs::File,
}

impl Lock {
    /// waits up to 5 s for a build that is still stopping (a restart right after a cancel)
    fn acquire(dir: &Path, ctl: &Ctl) -> Result<Lock, String> {
        let p = dir.join("building.lock");
        for _ in 0..50 {
            let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&p).map_err(|e| format!("{WRITE}\n{}: {e}", p.display()))?;
            match f.try_lock() {
                Ok(()) => return Ok(Lock { _file: f }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(format!("{WRITE}\n{}: {e}", p.display())),
            }
            if ctl.cancelled.load(Ordering::SeqCst) {
                return Err("cancelled".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("Another RE:DEX window is already building this game. Let it finish, then try again.".into())
    }
}

/// `[12/400] ...` -> (12, 400)
fn parse_progress(line: &str) -> Option<(u32, u32)> {
    let inner = line.trim_start().strip_prefix('[')?.split(']').next()?;
    let (d, t) = inner.split_once('/')?;
    Some((d.trim().parse().ok()?, t.trim().parse().ok()?))
}

/// xr-recomp's last line before ninja, `..., N files, generated in S s; running ninja` -> ninja's edge count: N
/// compiles, lookup.c's and the link (ninja itself prints `[n/M]` only as each edge finishes)
fn ninja_total(line: &str) -> Option<u32> {
    let head = line.trim_end().strip_suffix("running ninja")?;
    let n: u32 = head.rsplit_once(" files,")?.0.rsplit(' ').next()?.parse().ok()?;
    Some(n + 2)
}

fn identify_failed(e: xr_intake::Error) -> String {
    match e {
        xr_intake::Error::Refused(r) => format!("We couldn't use this file: {r}."),
        xr_intake::Error::Other(m) => format!("We couldn't read your game file. Check that it is still there, then add it again.\n{m}"),
    }
}

/// what identify answered in this process, by (path, size, time): a restart (Build faster, Resume) doesn't hash the
/// 2 GB dump again
#[allow(clippy::type_complexity)]
static IDENTIFIED: Mutex<Vec<((PathBuf, u64, Option<SystemTime>), (KnownBuild, String))>> = Mutex::new(Vec::new());

fn identify(seams: &Seams, dump: &Path) -> Result<(KnownBuild, String), xr_intake::Error> {
    let key = std::fs::metadata(dump).map(|m| (dump.to_path_buf(), m.len(), m.modified().ok()));
    if let Ok(k) = &key {
        if let Some((_, v)) = IDENTIFIED.lock().unwrap().iter().find(|(c, _)| c == k) {
            return Ok(v.clone());
        }
    }
    let v = (seams.identify)(dump)?;
    if let Ok(k) = key {
        IDENTIFIED.lock().unwrap().push((k, v.clone()));
    }
    Ok(v)
}

fn pump(r: impl Read + Send + 'static, tx: Sender<String>) {
    std::thread::spawn(move || {
        let mut r = BufReader::new(r);
        let mut buf = Vec::new();
        while r.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
            let _ = tx.send(String::from_utf8_lossy(&buf).trim_end().to_string());
            buf.clear();
        }
    });
}

/// Runs a tool at idle priority in the job, with the toolchain on PATH, feeding each output line to `on_line`.
/// Returns the exit status and the last few lines.
fn run_child(ctl: &Ctl, tools: &Toolchain, mut cmd: Command, on_line: &mut dyn FnMut(&str)) -> Result<(ExitStatus, String), String> {
    let path = std::env::join_paths(std::iter::once(tools.bin.clone()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())))
        .map_err(|e| e.to_string())?;
    cmd.env("PATH", path).env("XR_TOOLS", &tools.bin).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0040); // idle priority (ninja's compilers inherit it), no console
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("A part of RE:DEX that builds games is missing or couldn't start. Download RE:DEX again to put it back.\ncannot run {:?}: {e}", cmd.get_program()))?;
    *ctl.pid.lock().unwrap() = Some(child.id());
    #[cfg(windows)]
    ctl.job.assign(&child);
    if ctl.cancelled.load(Ordering::SeqCst) {
        kill(ctl);
    }
    let (tx, rx) = mpsc::channel();
    pump(child.stdout.take().unwrap(), tx.clone());
    pump(child.stderr.take().unwrap(), tx);
    let mut tail: Vec<String> = Vec::new();
    for line in rx {
        on_line(&line);
        tail.push(line);
        if tail.len() > 5 {
            tail.remove(0);
        }
    }
    let status = child.wait().map_err(|e| e.to_string());
    *ctl.pid.lock().unwrap() = None;
    Ok((status?, tail.join(" | ")))
}

pub(crate) fn run(dump: &Path, data: &Path, tools: &Toolchain, jobs: u32, seams: &Seams, ctl: &Ctl, tx: &Sender<Event>) {
    let mut stage = Stage::Check;
    let _ = tx.send(Event::Stage(stage));
    let ev = match pipeline(dump, data, tools, jobs, seams, ctl, tx, &mut stage) {
        Ok((build_id, build_dir)) => Event::Done { build_id, build_dir },
        Err(message) => Event::Failed { stage, message },
    };
    let _ = tx.send(ev);
}

#[allow(clippy::too_many_arguments)]
fn pipeline(dump: &Path, data: &Path, tools: &Toolchain, jobs: u32, seams: &Seams, ctl: &Ctl, tx: &Sender<Event>, stage: &mut Stage) -> Result<(&'static str, PathBuf), String> {
    let cancelled = || if ctl.cancelled.load(Ordering::SeqCst) { Err("cancelled".to_string()) } else { Ok(()) };
    let (build, hex) = identify(seams, dump).map_err(identify_failed)?;
    // kept with the dump's size and time: at a world's door the game is taken as this build without hashing again
    let _ = xr_intake::remember(data, dump, &hex);
    let _ = tx.send(Event::Identified { build_id: build.id });
    cancelled()?; // before the lock: a cancelled job never holds it
    let build_dir = data.join("build").join(build.id);
    std::fs::create_dir_all(&build_dir).map_err(|e| format!("{WRITE}\n{}: {e}", build_dir.display()))?;
    let _lock = Lock::acquire(&build_dir, ctl)?;
    let stamp = tools.stamp();
    if build_state(data, build.id, Some(&stamp)) == BuildState::Ready {
        return Ok((build.id, build_dir)); // nothing to do (the same game added again)
    }
    // what an earlier run of this build already wrote counts as space it has
    let need = NEED_BYTES.saturating_sub(dir_bytes(&data.join("dumps").join(&hex[..16])) + dir_bytes(&build_dir));
    let free = (seams.free_bytes)(data);
    if free < need {
        let gb = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
        return Err(format!(
            "There isn't enough free disk space to build the game: it needs {} more. Free up some space, then try again from the game's card.\nneed {}, free {}",
            gb(need - free),
            gb(need),
            gb(free)
        ));
    }

    *stage = Stage::Unpack;
    let _ = tx.send(Event::Stage(*stage));
    cancelled()?;
    let mut run_tool = |cmd: Command| run_child(ctl, tools, cmd, &mut |_| {}).map(|(st, tail)| (st.success(), format!("{st}: {tail}")));
    let dumps = (seams.unpack)(dump, data, &build, &hex, &tools.bin, &mut run_tool);
    cancelled()?;
    let dumps = dumps.map_err(|e| match e {
        xr_intake::Error::Refused(r) => format!("We couldn't unpack your game file. {AGAIN}\n{r}"),
        xr_intake::Error::Other(m) => format!("We couldn't unpack your game file. {AGAIN}\n{m}"),
    })?;

    *stage = Stage::Recompile;
    let _ = tx.send(Event::Stage(*stage));
    cancelled()?;
    let _ = std::fs::remove_file(build_dir.join("verified.txt")); // a rebuild is not verified until verify says so
    let mut cmd = Command::new(&tools.recomp);
    cmd.arg("--dump").arg(&dumps).arg("--functions").arg(tools.functions.join(build.id)).arg("--out").arg(&build_dir).arg("--jobs").arg(jobs.to_string());
    let to_build = |stage: &mut Stage| {
        if *stage == Stage::Recompile {
            *stage = Stage::Build; // ninja started: C generation is over
            let _ = tx.send(Event::Stage(Stage::Build));
        }
    };
    let (st, tail) = run_child(ctl, tools, cmd, &mut |line| {
        if let Some(total) = ninja_total(line) {
            to_build(stage);
            let _ = tx.send(Event::Progress { done: 0, total });
            let _ = tx.send(Event::Log(line.to_string()));
        } else if let Some((done, total)) = parse_progress(line) {
            to_build(stage);
            let _ = tx.send(Event::Progress { done, total });
        } else {
            let _ = tx.send(Event::Log(line.to_string()));
        }
    })?;
    cancelled()?;
    if !st.success() {
        return Err(format!("We couldn't finish building the game. {AGAIN}\nxr-recomp failed ({st}): {tail}"));
    }
    if !build_dir.join("xgame.dll").is_file() {
        return Err(format!("We couldn't finish building the game. {AGAIN}\nxr-recomp finished but xgame.dll was not built"));
    }
    to_build(stage); // nothing to compile (already up to date)

    *stage = Stage::Verify;
    let _ = tx.send(Event::Stage(*stage));
    test_run(ctl, tools, dump, &build_dir, build.id, tx)?;
    cancelled()?;
    std::fs::write(build_dir.join("verified.txt"), verified_text(build.id, &stamp)).map_err(|e| format!("{WRITE}\n{e}"))?; // last
    Ok((build.id, build_dir))
}

/// frames to run before the title screen is up and still (the language screen takes two A presses first, then the
/// logo animation; "PRESS START" shows from about frame 1200)
const TITLE_FRAME: &str = "1300";

/// The verify stage: the built game runs headless up to its title screen, in a scratch data folder inside the build
/// (the player's saves, settings and language stay untouched). Its top screen becomes `<build>/title.png`, the game
/// card's picture (made on this PC, never shipped; best effort).
fn test_run(ctl: &Ctl, tools: &Toolchain, dump: &Path, build_dir: &Path, id: &str, tx: &Sender<Event>) -> Result<(), String> {
    let scratch = build_dir.join("test-run");
    let _ = std::fs::remove_dir_all(&scratch);
    let _ = std::fs::remove_file(build_dir.join("title.png"));
    let run_dir = scratch.join("build").join(id);
    let result = (|| {
        std::fs::create_dir_all(&run_dir).map_err(|e| format!("{WRITE}\n{e}"))?;
        std::fs::copy(build_dir.join("xgame.dll"), run_dir.join("xgame.dll")).map_err(|e| format!("{WRITE}\n{e}"))?;
        let mut cmd = Command::new(&tools.redex);
        cmd.arg("--cci").arg(dump).arg("--build").arg(&run_dir);
        cmd.args(["--headless", "--offline", "--frames", TITLE_FRAME, "--press", "A@400", "--press", "A@440"]);
        let (st, tail) = run_child(ctl, tools, cmd, &mut |line| {
            let _ = tx.send(Event::Log(line.to_string()));
        })?;
        if ctl.cancelled.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        if !st.success() {
            return Err(format!("The built game didn't start in its test run. {AGAIN}\n{st}: {tail}"));
        }
        let _ = std::fs::copy(scratch.join("logs").join("top.png"), build_dir.join("title.png"));
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ninja_total_counts_files_plus_lookup_and_link() {
        let line = "xr-recomp: 9000 code.bin functions + 400 in 30 CRO modules (12 patch sites checked, 3 thunk flag demands), 250 files, generated in 41.2 s; running ninja";
        assert_eq!(ninja_total(line), Some(252));
        assert_eq!(ninja_total("[3/252] CC gen_003.c"), None);
        assert_eq!(parse_progress("[3/252] CC gen_003.c"), Some((3, 252)));
    }
}
