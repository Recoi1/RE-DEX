//! The mods installed from the start screen's Mods screen. Each has its own folder, `<data>/mods/installed/<id>/` (the
//! GameBanana id): `romfs/` and `romfs_ext/` laid over the game the way Citra lays them, `textures/` a texture pack.
//! `<data>/mods/installed.json` lists them in the order they were installed, which is the order they are laid in: a
//! later mod's file wins over an earlier one's. A mod turned off stays installed and is left out.
//!
//! Archives come from the internet, so nothing in one is trusted: [`unpack`] lists it first and refuses paths that
//! leave its folder, links and devices, and more than [`MAX_UNPACKED`] bytes; nothing from one is ever run.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// the most an archive may unpack to
pub const MAX_UNPACKED: u64 = 2 << 30;

/// a mod installed from the Mods screen
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    /// its GameBanana id, also its folder's name
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    /// the titles it is for (its archive kept its files under their ids), empty for both X and Y
    #[serde(default)]
    pub titles: Vec<u64>,
}

impl Installed {
    pub fn for_title(&self, program_id: u64) -> bool {
        self.titles.is_empty() || self.titles.contains(&program_id)
    }
}

/// where the installed mods' folders are
pub fn root(data: &Path) -> PathBuf {
    data.join("mods").join("installed")
}

fn list_file(data: &Path) -> PathBuf {
    data.join("mods").join("installed.json")
}

/// an installed mod's folder
pub fn folder(data: &Path, id: u64) -> PathBuf {
    root(data).join(id.to_string())
}

/// installed.json has one writer at a time in this process (an install's thread, the Mods screen)
static LIST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn list_guard() -> std::sync::MutexGuard<'static, ()> {
    LIST.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// tidies what a killed install or update left:
/// - a mod moved aside to `.old-<id>` w/o its new copy in place is moved back; one whose new copy is in place loses it
/// - `.new-`, `.unpack-` and `.download-` folders and `.installing-` files no install holds are deleted
/// - a mod folder installed.json doesn't list (killed before the list was saved) is listed, off, so it can be removed
fn recover(data: &Path) {
    let Ok(entries) = std::fs::read_dir(root(data)) else { return };
    let mut unlisted = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(id) = name.parse::<u64>() {
            unlisted.push(id);
            continue;
        }
        let Some((kind, id)) = name.strip_prefix('.').and_then(|n| n.split_once('-')).and_then(|(k, id)| Some((k.to_owned(), id.parse::<u64>().ok()?))) else { continue };
        match kind.as_str() {
            "old" if folder(data, id).exists() => drop(std::fs::remove_dir_all(entry.path())),
            "old" => {
                if let Err(e) = std::fs::rename(entry.path(), folder(data, id)) {
                    log::warn!("mods: {} could not be put back: {e}", folder(data, id).display());
                }
                unlisted.push(id);
            }
            "new" | "unpack" | "download" | "installing" => {
                // held: an install runs (here or in another RE:DEX); claimed now: nobody's
                if let Ok(_claim) = claim(data, id) {
                    if entry.path().is_dir() {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
            _ => {}
        }
    }
    let _guard = list_guard();
    let mut mods = read_list(data);
    let before = mods.len();
    for id in unlisted.into_iter().filter(|id| folder(data, *id).is_dir()) {
        if !mods.iter().any(|m| m.id == id) {
            mods.push(Installed { id, name: format!("GameBanana mod {id}"), version: String::new(), enabled: false, titles: Vec::new() });
        }
    }
    if mods.len() != before {
        if let Err(e) = save(data, &mods) {
            log::warn!("mods: {} could not be saved: {e}", list_file(data).display());
        }
    }
}

/// a mod being installed, held for the install's length: a second install of it (here or in another RE:DEX) is refused
pub struct Claim {
    file: Option<std::fs::File>,
    path: PathBuf,
}

impl Drop for Claim {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

/// claims mod id for an install, or says it is being installed already
pub fn claim(data: &Path, id: u64) -> Result<Claim, String> {
    std::fs::create_dir_all(root(data)).map_err(|e| e.to_string())?;
    let path = root(data).join(format!(".installing-{id}"));
    let mut open = std::fs::OpenOptions::new();
    open.write(true);
    // Windows: a file no one else may open, let go of when the process ends however it ends
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open.create(true).share_mode(0);
    }
    // shortcut: elsewhere a crash leaves the file and blocks the mod's installs until it is deleted; use flock there
    #[cfg(not(windows))]
    open.create_new(true);
    let file = open.open(&path).map_err(|_| "This mod is already being installed.".to_owned())?;
    Ok(Claim { file: Some(file), path })
}

/// the installed mods in their order (what a killed install left tidied first, see `recover`); none when the list is
/// missing or unreadable, and one whose folder is gone is left out
pub fn load(data: &Path) -> Vec<Installed> {
    recover(data);
    read_list(data)
}

/// installed.json as it is, the mods whose folders are there
fn read_list(data: &Path) -> Vec<Installed> {
    let Ok(bytes) = std::fs::read(list_file(data)) else { return Vec::new() };
    let mods: Vec<Installed> = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        log::warn!("mods: {} is unreadable ({e}), no installed mod is used", list_file(data).display());
        Vec::new()
    });
    mods.into_iter().filter(|m| folder(data, m.id).is_dir()).collect()
}

/// writes the list (to a new file first, so a crash never leaves half a list)
pub fn save(data: &Path, mods: &[Installed]) -> std::io::Result<()> {
    let file = list_file(data);
    std::fs::create_dir_all(root(data))?;
    let new = file.with_extension("json.new");
    std::fs::write(&new, serde_json::to_vec_pretty(mods).map_err(std::io::Error::other)?)?;
    std::fs::rename(new, file)
}

/// the mods on for a title, in the order they are laid, each w/ its folder
pub fn enabled(data: &Path, program_id: u64) -> Vec<(Installed, PathBuf)> {
    load(data).into_iter().filter(|m| m.enabled && m.for_title(program_id)).map(|m| {
        let dir = folder(data, m.id);
        (m, dir)
    })
    .collect()
}

/// changes the list, read and written under the one writer's lock
fn update(data: &Path, change: impl FnOnce(&mut Vec<Installed>)) -> std::io::Result<()> {
    recover(data);
    let _guard = list_guard();
    let mut mods = read_list(data);
    change(&mut mods);
    save(data, &mods)
}

/// turns an installed mod on or off
pub fn set_enabled(data: &Path, id: u64, on: bool) -> std::io::Result<()> {
    update(data, |mods| mods.iter_mut().filter(|m| m.id == id).for_each(|m| m.enabled = on))
}

/// uninstalls a mod: off the list, then its folder deleted
pub fn remove(data: &Path, id: u64) -> std::io::Result<()> {
    update(data, |mods| mods.retain(|m| m.id != id))?;
    match std::fs::remove_dir_all(folder(data, id)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// every file under dir (not following links), relative to it, / separated
fn walk(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                if let Ok(rel) = entry.path().strip_prefix(dir) {
                    found.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    found.sort();
    found
}

/// the RomFS files a mod's folder replaces, adds, patches or removes, as RomFS paths in lower case
pub fn files(dir: &Path) -> Vec<String> {
    let mut paths: Vec<String> = walk(&dir.join("romfs")).into_iter().map(|p| p.to_ascii_lowercase()).collect();
    for patch in walk(&dir.join("romfs_ext")) {
        let patch = patch.to_ascii_lowercase();
        // a patch or a stub is for the file it is named after
        let target = [".ips", ".bps", ".stub"].iter().find_map(|end| patch.strip_suffix(end)).unwrap_or(&patch);
        paths.push(target.to_owned());
    }
    paths.sort();
    paths.dedup();
    paths
}

/// the first file a mod's folder changes that is not on the cosmetic list: what keeps a game w/ it offline
pub fn offline_file(dir: &Path) -> Option<String> {
    files(dir).into_iter().find(|path| !crate::cosmetic::cosmetic(path))
}

/// the RomFS files more than one of the enabled mods changes, each w/ those mods' ids in the order they are laid
/// (the last one's file is used)
pub fn clashes(data: &Path, mods: &[Installed]) -> Vec<(String, Vec<u64>)> {
    let mut by_file: std::collections::BTreeMap<String, Vec<u64>> = Default::default();
    for m in mods.iter().filter(|m| m.enabled) {
        for file in files(&folder(data, m.id)) {
            by_file.entry(file).or_default().push(m.id);
        }
    }
    by_file.into_iter().filter(|(_, ids)| ids.len() > 1).collect()
}

/// what an unpacked archive holds that RE:DEX can use
#[derive(Debug, PartialEq)]
pub struct Found {
    /// the folder whose contents are RomFS files (`romfs/`, `<title id>/romfs/`, or the one around a loose `a/`)
    pub romfs: Option<PathBuf>,
    /// whether that was a loose `a/` (only it gets taken, not what lies beside it)
    pub loose: bool,
    pub romfs_ext: Option<PathBuf>,
    /// how many texture pack pictures (tex1_*.png) it has
    pub textures: usize,
    /// the title its RomFS files are kept under, when they are
    pub titles: Vec<u64>,
}

/// the archive's files that change the game's code, which RE:DEX can't install (its code is recompiled)
const CODE: [&str; 4] = ["code.bin", "code.ips", "code.bps", "exheader.bin"];

/// a folder name that is a 3DS title id (16 hex digits)
fn title_id(name: &str) -> Option<u64> {
    (name.len() == 16).then(|| u64::from_str_radix(name, 16).ok()).flatten().filter(|id| id >> 32 == 0x0004_0000)
}

/// the folders under dir, breadth first and by name, at most `depth` deep
fn folders(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut level = vec![dir.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for at in &level {
            let Ok(entries) = std::fs::read_dir(at) else { continue };
            let mut dirs: Vec<PathBuf> = entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
            dirs.sort();
            next.extend(dirs);
        }
        found.extend(next.iter().cloned());
        level = next;
    }
    found
}

fn named(path: &Path, name: &str) -> bool {
    path.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
}

/// what an unpacked archive holds, or why RE:DEX can't use it (said to the player)
pub fn classify(unpacked: &Path) -> Result<Found, String> {
    let files = walk(unpacked);
    let base = |p: &String| p.rsplit('/').next().unwrap_or(p).to_ascii_lowercase();
    if let Some(code) = files.iter().find(|p| CODE.contains(&base(p).as_str()) || p.to_ascii_lowercase().split('/').any(|part| part == "exefs")) {
        return Err(format!("This mod changes the game's code ({}), which RE:DEX can't install. Open its page to see what it does.", base(code)));
    }
    let textures = files.iter().filter(|p| base(p).starts_with("tex1_") && base(p).ends_with(".png")).count();
    let dirs = folders(unpacked, 6);
    let mut found = Found { romfs: None, loose: false, romfs_ext: None, textures, titles: Vec::new() };
    if let Some(romfs) = dirs.iter().find(|d| named(d, "romfs")) {
        let parent = romfs.parent().unwrap_or(unpacked);
        found.titles = parent.file_name().and_then(|n| title_id(&n.to_string_lossy())).into_iter().collect();
        found.romfs_ext = std::fs::read_dir(parent).ok().and_then(|entries| entries.flatten().map(|e| e.path()).find(|p| p.is_dir() && named(p, "romfs_ext")));
        found.romfs = Some(romfs.clone());
    } else if let Some(a) = dirs.iter().find(|d| named(d, "a") && folders(d, 1).iter().any(|c| c.file_name().is_some_and(|n| n.len() == 1 && n.to_string_lossy().chars().all(|c| c.is_ascii_digit())))) {
        // the game's own files lie under a/0/0/0 .. a/2/7/x: a mod w/o a romfs folder starts at the a
        (found.romfs, found.loose) = (a.parent().map(Path::to_path_buf), true);
    }
    if found.romfs.is_none() && found.textures == 0 {
        return Err("This archive has no game files (a romfs folder) and no texture pack, so RE:DEX can't use it. Open its page to see how it is meant to be used.".into());
    }
    Ok(found)
}

/// installs what `unpacked` (an archive just unpacked, under [`root`]) holds as mod `id`, replacing it if it was
/// installed, and turns it on; `unpacked` is used up. Returns the mod as listed, or why it can't be used
pub fn install(data: &Path, id: u64, name: &str, version: &str, unpacked: &Path) -> Result<Installed, String> {
    let found = classify(unpacked)?;
    let io = |e: std::io::Error| format!("We couldn't put the mod in place: {e}. Check that the RE:DEX data folder can be written to.");
    let staging = root(data).join(format!(".new-{id}"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(io)?;
    if let Some(romfs) = &found.romfs {
        if found.loose {
            let a = std::fs::read_dir(romfs).map_err(io)?.flatten().map(|e| e.path()).find(|p| p.is_dir() && named(p, "a")).ok_or("the a folder went away")?;
            std::fs::create_dir(staging.join("romfs")).map_err(io)?;
            std::fs::rename(a, staging.join("romfs/a")).map_err(io)?;
        } else {
            std::fs::rename(romfs, staging.join("romfs")).map_err(io)?;
        }
    }
    if let Some(ext) = &found.romfs_ext {
        std::fs::rename(ext, staging.join("romfs_ext")).map_err(io)?;
    }
    if found.textures > 0 {
        // the pack reader takes the tex1_ pictures (and the pack.json) from any folder in it
        std::fs::rename(unpacked, staging.join("textures")).map_err(io)?;
    }
    // an update swaps through .old-<id>: killed at any point, a working copy stays (see recover)
    let (target, old) = (folder(data, id), root(data).join(format!(".old-{id}")));
    if target.exists() {
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(&target, &old).map_err(io)?;
    }
    if let Err(e) = std::fs::rename(&staging, &target) {
        let _ = std::fs::rename(&old, &target);
        return Err(io(e));
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_dir_all(unpacked);
    let installed = Installed { id, name: name.to_owned(), version: version.to_owned(), enabled: true, titles: found.titles };
    update(data, |mods| match mods.iter_mut().find(|m| m.id == id) {
        // installed again (an update) keeps its place in the order
        Some(m) => *m = installed.clone(),
        None => mods.push(installed.clone()),
    })
    .map_err(io)?;
    Ok(installed)
}

/// the system's tar (bsdtar: it reads zip, 7z and rar), taken from the system folder on Windows so no tar.exe
/// lying in the PATH or the current folder is run instead
pub fn tar() -> PathBuf {
    if cfg!(windows) {
        let system = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        system.join("System32").join("tar.exe")
    } else {
        PathBuf::from("tar")
    }
}

/// the device names Windows keeps for itself, whatever comes after a dot
const DEVICES: [&str; 33] = [
    "con", "prn", "aux", "nul", "conin$", "conout$", "clock$", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
    "com¹", "com²", "com³", "lpt0", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "lpt¹", "lpt²", "lpt³",
];

/// why an archive entry may not be unpacked, if it may not: `name` as the archive has it, `line` its line in
/// `tar -tv` (the kind is its first letter: only files and folders are taken)
pub fn refused(name: &str, line: &str) -> Option<String> {
    let name_shown = name.chars().take(80).collect::<String>();
    match line.chars().next() {
        Some('-' | 'd') => {}
        _ => return Some(format!("it has a link or a device ({name_shown})")),
    }
    let parts: Vec<&str> = name.split(['/', '\\']).collect();
    let absolute = name.starts_with(['/', '\\']) || name.contains(':');
    if absolute || parts.iter().any(|p| *p == "..") {
        return Some(format!("a file in it would land outside its folder ({name_shown})"));
    }
    if parts.iter().any(|p| DEVICES.contains(&p.split('.').next().unwrap_or("").trim_end().to_ascii_lowercase().as_str())) {
        return Some(format!("it has a file named after a device ({name_shown})"));
    }
    None
}

/// the size `tar -tv` gives an entry (its fifth field), None when the line doesn't have one
fn listed_size(line: &str) -> Option<u64> {
    line.split_whitespace().nth(4)?.parse().ok()
}

/// the most entries an archive may hold: mods have hundreds, big texture packs a few thousand
pub const MAX_ENTRIES: usize = 100_000;
/// room left free on the drive beside what an archive unpacks to
const SPARE: u64 = 512 << 20;

/// the child dies when this process ends, however it ends (a Windows job that kills what is in it when its last handle
/// closes, which is ours, held for the process's life). Only tar and curl go in it, never the game a start screen starts
pub fn tie_to_us(child: &std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        static JOB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let job = *JOB.get_or_init(|| {
            // safe: a nameless job w/ default security; the limits struct is plain data of the size given
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return 0;
                }
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
                SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size);
                job as usize
            }
        });
        // safe: our job and a child's handle, both open; a child that ended already just isn't added
        if job != 0 && unsafe { AssignProcessToJobObject(job as _, child.as_raw_handle() as _) } == 0 {
            log::warn!("mods: a child process could not be tied to this one");
        }
    }
    #[cfg(not(windows))]
    let _ = child;
}

/// a reader that stops w/ an error once `cancel` is set
struct Cancellable<'a, R> {
    inner: R,
    cancel: &'a std::sync::atomic::AtomicBool,
}

impl<R: std::io::Read> std::io::Read for Cancellable<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::other("cancelled"));
        }
        self.inner.read(buf)
    }
}

fn tar_command() -> Command {
    let mut command = Command::new(tar());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // no console window flashing up over the start screen
        command.creation_flags(0x0800_0000);
    }
    command
}

fn run_tar(args: &[&std::ffi::OsStr]) -> Result<std::process::Output, String> {
    use std::process::Stdio;
    let start = |e: std::io::Error| format!("the system's tar could not be started: {e}");
    let child = tar_command().args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(start)?;
    tie_to_us(&child);
    child.wait_with_output().map_err(start)
}

/// the free bytes on the drive holding dir, None when that can't be told
fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0u64;
        // safe: a NUL-ended path and a u64 to write to; the totals it can also give are not asked for
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        None
    }
}

/// what an entry of a zip is, as the first letter of a `tar -tv` line (see [`refused`])
fn zip_kind(symlink: bool, dir: bool) -> &'static str {
    if symlink {
        "l"
    } else if dir {
        "d"
    } else {
        "-"
    }
}

/// the entries a zip says it has, each checked (see [`refused`]), and the bytes they say they unpack to
fn list_zip(archive: &Path, max_entries: usize) -> Result<u64, String> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).map_err(|e| e.to_string())?).map_err(|e| format!("it could not be read ({e})"))?;
    if zip.len() > max_entries {
        return Err(format!("it holds more than {max_entries} files"));
    }
    let mut total = 0u64;
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(|e| format!("it could not be read ({e})"))?;
        if let Some(reason) = refused(entry.name(), zip_kind(entry.is_symlink(), entry.is_dir())) {
            return Err(reason);
        }
        total = total.saturating_add(entry.size());
    }
    Ok(total)
}

/// the entries tar lists (7z, rar), each checked (see [`refused`]), and the bytes they say they unpack to
fn list_tar(archive: &Path, max_entries: usize) -> Result<u64, String> {
    let names = run_tar(&["-tf".as_ref(), archive.as_os_str()])?;
    let lines = run_tar(&["-tvf".as_ref(), archive.as_os_str()])?;
    if !names.status.success() || !lines.status.success() {
        return Err(format!("it could not be read ({})", String::from_utf8_lossy(&names.stderr).lines().next().unwrap_or("")));
    }
    let names = String::from_utf8_lossy(&names.stdout).lines().map(str::to_owned).collect::<Vec<_>>();
    let lines = String::from_utf8_lossy(&lines.stdout).lines().map(str::to_owned).collect::<Vec<_>>();
    if names.len() > max_entries {
        return Err(format!("it holds more than {max_entries} files"));
    }
    if names.len() != lines.len() {
        return Err("its list of files could not be read".into());
    }
    let mut total = 0u64;
    for (name, line) in names.iter().zip(&lines) {
        if let Some(reason) = refused(name, line) {
            return Err(reason);
        }
        total = total.saturating_add(listed_size(line).ok_or("its list of files could not be read")?);
    }
    Ok(total)
}

/// the bytes of the files under dir; an error naming anything there that is not a file or a folder (a link, a device)
fn landed(dir: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at).map_err(|e| e.to_string())?.flatten() {
            let meta = std::fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            } else {
                return Err(format!("it has a link or a device ({})", entry.file_name().to_string_lossy()));
            }
        }
    }
    Ok(total)
}

/// unpacks archive (zip, 7z or rar) into dest, which must not exist yet. Every entry is checked first (see
/// [`refused`]), at most [`MAX_ENTRIES`] of them, and the sizes it gives must fit in `cap` and in the drive's free
/// space. Those sizes come from the archive, so they are not trusted: a zip is unpacked here w/ the bytes counted as
/// they are written, anything else by the system's tar w/ the folder measured as it fills, stopped at the cap either
/// way. What landed is checked once more after. On a refusal nothing is left in dest. `cancel` stops it ("cancelled")
pub fn unpack(archive: &Path, dest: &Path, cap: u64, cancel: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    use std::io::Read;
    let why = |reason: String| format!("This archive can't be installed: {reason}.");
    let mut magic = [0u8; 2];
    let zipped = std::fs::File::open(archive).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == b"PK";
    let listed = if zipped { list_zip(archive, MAX_ENTRIES) } else { list_tar(archive, MAX_ENTRIES) }.map_err(why)?;
    if listed > cap {
        return Err(why(format!("it unpacks to more than {} MB", cap >> 20)));
    }
    let free = free_space(dest.parent().unwrap_or(dest));
    if free.is_some_and(|free| free < listed.saturating_add(SPARE)) {
        return Err(why(format!("the drive needs {} MB free for it", (listed + SPARE) >> 20)));
    }
    // an archive that lies about its sizes is stopped before it fills the drive, too
    let cap = free.map_or(cap, |free| cap.min(free.saturating_sub(SPARE)));
    std::fs::create_dir(dest).map_err(|e| why(format!("its folder could not be made ({e})")))?;
    let unpacked = if zipped { unzip(archive, dest, cap, MAX_ENTRIES, cancel) } else { untar(archive, dest, cap, cancel) }.and_then(|()| landed(dest));
    match unpacked {
        Ok(total) if total <= cap => Ok(()),
        result => {
            let _ = std::fs::remove_dir_all(dest);
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            Err(why(result.err().unwrap_or_else(|| format!("it unpacks to more than {} MB", cap >> 20))))
        }
    }
}

/// unpacks a zip into dest w/ the zip reader: names checked again (see [`refused`]), at most `max_entries`, files and
/// folders only, and at most `cap` bytes, counted as they are written whatever the zip says
fn unzip(archive: &Path, dest: &Path, cap: u64, max_entries: usize, cancel: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    use std::io::Read;
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("it could not be read ({e})"))?;
    if zip.len() > max_entries {
        return Err(format!("it holds more than {max_entries} files"));
    }
    let mut left = cap;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("it could not be read ({e})"))?;
        if let Some(reason) = refused(entry.name(), zip_kind(entry.is_symlink(), entry.is_dir())) {
            return Err(reason);
        }
        let path = entry.enclosed_name().ok_or_else(|| format!("a file in it would land outside its folder ({})", entry.name()))?;
        let out = dest.join(path);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        std::fs::create_dir_all(out.parent().unwrap_or(dest)).map_err(|e| e.to_string())?;
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        let mut reader = Cancellable { inner: (&mut entry).take(left + 1), cancel };
        let written = std::io::copy(&mut reader, &mut file).map_err(|e| format!("it could not be unpacked ({e})"))?;
        left = left.checked_sub(written).ok_or_else(|| format!("it unpacks to more than {} MB", cap >> 20))?;
    }
    Ok(())
}

/// unpacks a 7z or rar into dest w/ the system's tar, measuring dest every 250 ms and stopping tar once it holds more
/// than `cap` bytes or `cancel` is set
fn untar(archive: &Path, dest: &Path, cap: u64, cancel: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    use std::process::Stdio;
    let mut child = tar_command()
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("the system's tar could not be started: {e}"))?;
    tie_to_us(&child);
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
        let done = child.try_wait().map_err(|e| e.to_string())?;
        let size = landed(dest);
        if size.as_ref().map_or(true, |&total| total > cap) {
            let _ = child.kill();
            let _ = child.wait();
            return size.and(Err(format!("it unpacks to more than {} MB", cap >> 20)));
        }
        match done {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("the system's tar could not unpack it ({status})")),
            None => std::thread::sleep(std::time::Duration::from_millis(250)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// not cancelled
    static NO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("redex-modlist-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(dir: &Path, path: &str, data: &[u8]) {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    /// a ustar archive of (name, kind, data) entries, by hand: tar won't write the bad ones these tests need
    fn ustar(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, kind, data) in entries {
            let mut header = [0u8; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[156] = *kind;
            if *kind == b'2' {
                header[157..163].copy_from_slice(b"target");
            }
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].copy_from_slice(b"        ");
            let sum: u32 = header.iter().map(|&b| b as u32).sum();
            header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            out.extend_from_slice(&header);
            out.extend_from_slice(data);
            out.resize(out.len().next_multiple_of(512), 0);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    #[test]
    fn entries_leaving_the_folder_links_and_devices_are_refused() {
        let file = "-rw-r--r--  0 0      0           5 Jan  1  2020 x";
        assert_eq!(refused("romfs/a/0/8/8", file), None);
        assert_eq!(refused("My Mod/tex1_8x8_00_0.png", file), None);
        assert_eq!(refused("romfs/", "drwxr-xr-x  0 0 0 0 Jan  1  2020 romfs/"), None);
        for bad in ["../evil", "romfs/../../evil", "/etc/evil", "\\evil", "C:\\evil", "C:evil", "a\\..\\..\\evil", "x:stream"] {
            assert!(refused(bad, file).unwrap().contains("outside"), "{bad}");
        }
        for bad in ["CON", "romfs/nul.txt", "aux.png", "a/COM1", "LPT9.x"] {
            assert!(refused(bad, file).unwrap().contains("device"), "{bad}");
        }
        assert!(refused("link", "lrwxr-xr-x  0 0 0 0 Jan  1  2020 link -> /etc").unwrap().contains("link"));
        assert!(refused("hard", "hrw-r--r--  0 0 0 0 Jan  1  2020 hard link to x").is_some());
        assert!(refused("dev", "crw-r--r--  0 0 0 0 Jan  1  2020 dev").is_some());
        assert_eq!(listed_size(file), Some(5));
    }

    /// real archives through the system's tar: a good one unpacks; one w/ a path out of its folder, a link, or too
    /// many bytes is refused and leaves nothing behind
    #[test]
    fn unpack_takes_good_archives_and_refuses_bad_ones() {
        let dir = temp("unpack");
        let good = dir.join("good.tar");
        std::fs::write(&good, ustar(&[("Mod/romfs/a/0/8/8", b'0', b"model"), ("Mod/README.txt", b'0', b"hi")])).unwrap();
        unpack(&good, &dir.join("good"), MAX_UNPACKED, &NO).unwrap();
        assert_eq!(std::fs::read(dir.join("good/Mod/romfs/a/0/8/8")).unwrap(), b"model");
        for (name, entries) in [
            ("escape", vec![("ok.txt", b'0', &b"a"[..]), ("../escaped.txt", b'0', b"b")]),
            ("link", vec![("link", b'2', &b""[..])]),
            ("device", vec![("nul", b'0', &b"x"[..])]),
        ] {
            let archive = dir.join(format!("{name}.tar"));
            std::fs::write(&archive, ustar(&entries)).unwrap();
            let error = unpack(&archive, &dir.join(name), MAX_UNPACKED, &NO).unwrap_err();
            assert!(error.starts_with("This archive can't be installed"), "{name}: {error}");
            assert!(!dir.join(name).exists() && !dir.join("escaped.txt").exists(), "{name}");
        }
        assert!(unpack(&good, &dir.join("small"), 4, &NO).unwrap_err().contains("more than"));
        assert!(!dir.join("small").exists());
        // a zip made by the same tar (as most mods come)
        let zip = dir.join("pack.zip");
        put(&dir, "src/Pack/tex1_8x8_00000000000000AA_13.png", b"png");
        let made = Command::new(tar()).arg("-a").arg("-cf").arg(&zip).arg("-C").arg(dir.join("src")).arg("Pack").status().unwrap();
        assert!(made.success());
        unpack(&zip, &dir.join("zip"), MAX_UNPACKED, &NO).unwrap();
        assert!(dir.join("zip/Pack/tex1_8x8_00000000000000AA_13.png").is_file());
        // the zip reader keeps to its cap whatever the zip says
        std::fs::create_dir(dir.join("unzip-small")).unwrap();
        assert!(unzip(&zip, &dir.join("unzip-small"), 2, MAX_ENTRIES, &NO).unwrap_err().contains("more than"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// a zip by hand: (name, method 0 stored or 8 deflated, the bytes it holds, the size its headers claim)
    fn zip_by_hand(entries: &[(&str, u16, Vec<u8>, u32)]) -> Vec<u8> {
        let (mut out, mut central) = (Vec::new(), Vec::new());
        for (name, method, data, claimed) in entries {
            let crc = crc32fast::hash(data);
            let stored = if *method == 8 {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                std::io::Write::write_all(&mut e, data).unwrap();
                e.finish().unwrap()
            } else {
                data.clone()
            };
            let at = out.len() as u32;
            let common = |v: &mut Vec<u8>| {
                v.extend_from_slice(&20u16.to_le_bytes());
                v.extend_from_slice(&0u16.to_le_bytes());
                v.extend_from_slice(&method.to_le_bytes());
                v.extend_from_slice(&[0; 4]);
                v.extend_from_slice(&crc.to_le_bytes());
                v.extend_from_slice(&(stored.len() as u32).to_le_bytes());
                v.extend_from_slice(&claimed.to_le_bytes());
                v.extend_from_slice(&(name.len() as u16).to_le_bytes());
                v.extend_from_slice(&0u16.to_le_bytes());
            };
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            common(&mut out);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&stored);
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            common(&mut central);
            // comment length, disk, internal attributes, external attributes
            central.extend_from_slice(&[0; 6]);
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&at.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let (at, len) = (out.len() as u32, central.len() as u32);
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// a zip whose headers say 10 bytes for a megabyte is stopped at the cap, leaving nothing; one w/ too many entries
    /// is refused before anything is written; a name the zip reader would take but tar's rules refuse is refused too
    #[test]
    fn zips_that_lie_or_hold_too_much_are_stopped() {
        let dir = temp("bombs");
        let liar = dir.join("liar.zip");
        std::fs::write(&liar, zip_by_hand(&[("romfs/a/0/8/8", 8, vec![0; 1 << 20], 10)])).unwrap();
        assert!(list_zip(&liar, MAX_ENTRIES).unwrap() == 10, "the headers do lie");
        std::fs::create_dir(dir.join("liar")).unwrap();
        assert!(unzip(&liar, &dir.join("liar"), 1000, MAX_ENTRIES, &NO).is_err());
        assert!(std::fs::metadata(dir.join("liar/romfs/a/0/8/8")).map_or(0, |m| m.len()) <= 1001);
        let error = unpack(&liar, &dir.join("liar-unpack"), 1000, &NO).unwrap_err();
        assert!(error.starts_with("This archive can't be installed"), "{error}");
        assert!(!dir.join("liar-unpack").exists());

        let many: Vec<(String, u16, Vec<u8>, u32)> = (0..11).map(|i| (format!("f{i}"), 0, Vec::new(), 0)).collect();
        let many: Vec<(&str, u16, Vec<u8>, u32)> = many.iter().map(|(n, m, d, c)| (n.as_str(), *m, d.clone(), *c)).collect();
        let bomb = dir.join("many.zip");
        std::fs::write(&bomb, zip_by_hand(&many)).unwrap();
        assert!(list_zip(&bomb, 10).unwrap_err().contains("more than 10 files"));
        std::fs::create_dir(dir.join("many")).unwrap();
        assert!(unzip(&bomb, &dir.join("many"), MAX_UNPACKED, 10, &NO).unwrap_err().contains("more than 10 files"));
        assert_eq!(std::fs::read_dir(dir.join("many")).unwrap().count(), 0);
        assert!(list_zip(&bomb, 11).is_ok());

        let stream = dir.join("stream.zip");
        std::fs::write(&stream, zip_by_hand(&[("romfs/a/0/8/8:hidden", 0, b"x".to_vec(), 1)])).unwrap();
        assert!(unpack(&stream, &dir.join("stream"), MAX_UNPACKED, &NO).unwrap_err().contains("outside"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// an update killed between its two renames keeps a working mod: the old copy is put back, or dropped once the new
    /// one is in place
    #[test]
    fn an_update_cut_short_keeps_a_working_mod() {
        let data = temp("swap");
        let unpacked = root(&data).join(".unpack");
        put(&unpacked, "romfs/a/0/8/8", b"v1");
        install(&data, 7, "Skin", "1", &unpacked).unwrap();
        // killed after the old copy was moved aside, before the new one was put in place
        std::fs::rename(folder(&data, 7), root(&data).join(".old-7")).unwrap();
        assert_eq!(load(&data).iter().map(|m| m.id).collect::<Vec<_>>(), [7]);
        assert_eq!(std::fs::read(folder(&data, 7).join("romfs/a/0/8/8")).unwrap(), b"v1");
        // killed after the new copy was put in place, before the old one was deleted
        put(&root(&data), ".old-7/romfs/a/0/8/8", b"v1");
        put(&unpacked, "romfs/a/0/8/8", b"v2");
        install(&data, 7, "Skin", "2", &unpacked).unwrap();
        put(&root(&data), ".old-7/romfs/a/0/8/8", b"v1");
        assert_eq!(load(&data)[0].version, "2");
        assert!(!root(&data).join(".old-7").exists());
        assert_eq!(std::fs::read(folder(&data, 7).join("romfs/a/0/8/8")).unwrap(), b"v2");
        std::fs::remove_dir_all(data).unwrap();
    }

    /// a tar of `n` small files, slow to unpack (each a file to make)
    fn many_files(path: &Path, n: usize) {
        let names: Vec<String> = (0..n).map(|i| format!("romfs/f{i:06}")).collect();
        let entries: Vec<(&str, u8, &[u8])> = names.iter().map(|n| (n.as_str(), b'0', &b"x"[..])).collect();
        std::fs::write(path, ustar(&entries)).unwrap();
    }

    /// stands in for a start screen closed mid-install: unpacks the archive XR_TEST_UNPACK names and exits the process
    /// as soon as the first files land, w/o anything stopping tar
    #[test]
    #[ignore = "run by a_closed_window_takes_tar_with_it"]
    fn exits_mid_unpack() {
        let Some(archive) = std::env::var_os("XR_TEST_UNPACK").map(PathBuf::from) else { return };
        let dest = archive.with_file_name(".unpack-9");
        std::thread::spawn({
            let (archive, dest) = (archive.clone(), dest.clone());
            move || unpack(&archive, &dest, MAX_UNPACKED, &NO)
        });
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::fs::read_dir(dest.join("romfs")).map_or(true, |mut d| d.next().is_none()) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::process::exit(0);
    }

    /// closing the window mid-install takes tar w/ it (the job tie): the folder stops growing at once, and the next
    /// start's tidy-up deletes it; a cancel stops tar the same way while the process goes on
    #[test]
    fn a_closed_window_takes_tar_with_it() {
        let data = temp("orphan");
        std::fs::create_dir_all(root(&data)).unwrap();
        let archive = root(&data).join("many.tar");
        many_files(&archive, 20_000);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "modlist::tests::exits_mid_unpack", "--test-threads=1"])
            .env("XR_TEST_UNPACK", &archive)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let count = || std::fs::read_dir(root(&data).join(".unpack-9/romfs")).map_or(0, |d| d.count());
        let first = count();
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(first > 0 && first < 20_000, "{first} files: the run was not cut short mid-unpack");
        assert_eq!(count(), first, "tar went on after its parent was gone");
        load(&data);
        assert!(!root(&data).join(".unpack-9").exists(), "the next start deletes what it left");

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = std::thread::spawn({
            let (archive, dest, cancel) = (archive.clone(), root(&data).join(".unpack-8"), cancel.clone());
            move || unpack(&archive, &dest, MAX_UNPACKED, &cancel)
        });
        while std::fs::read_dir(root(&data).join(".unpack-8/romfs")).map_or(true, |mut d| d.next().is_none()) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(worker.join().unwrap(), Err("cancelled".to_owned()));
        assert!(!root(&data).join(".unpack-8").exists());
        std::fs::remove_dir_all(data).unwrap();
    }

    /// a mod folder the list doesn't name (an install killed before it saved the list) is listed, off, and leftovers no
    /// install holds go; one an install holds stays
    #[test]
    fn leftovers_are_tidied_and_unlisted_mods_listed() {
        let data = temp("tidy");
        put(&root(&data), "42/romfs/a/0/8/8", b"skin");
        put(&root(&data), ".new-5/romfs/x", b"x");
        put(&root(&data), ".download-6/archive.zip", b"x");
        put(&root(&data), ".unpack-7/x", b"x");
        let held = claim(&data, 7).unwrap();
        let mods = load(&data);
        assert_eq!(mods.iter().map(|m| (m.id, m.enabled)).collect::<Vec<_>>(), [(42, false)]);
        assert!(!root(&data).join(".new-5").exists() && !root(&data).join(".download-6").exists());
        assert!(root(&data).join(".unpack-7").exists(), "an install holds it");
        drop(held);
        load(&data);
        assert!(!root(&data).join(".unpack-7").exists());
        remove(&data, 42).unwrap();
        assert!(load(&data).is_empty() && !folder(&data, 42).exists());
        std::fs::remove_dir_all(data).unwrap();
    }

    /// the list's writers take turns: toggles from one thread while installs run on another lose nothing
    #[test]
    fn list_writers_take_turns() {
        let data = temp("turns");
        for id in [1, 2] {
            put(&root(&data), &format!(".unpack-{id}/romfs/a/0/8/8"), b"skin");
            install(&data, id, "Skin", "", &root(&data).join(format!(".unpack-{id}"))).unwrap();
        }
        let toggler = std::thread::spawn({
            let data = data.clone();
            move || (0..50).for_each(|i| set_enabled(&data, 1, i % 2 == 1).unwrap())
        });
        // as gamebanana's install does, the install holds its claim (the tidy-up leaves its folders alone)
        let held = claim(&data, 2).unwrap();
        for i in 0..20 {
            put(&root(&data), ".unpack-2/romfs/a/0/8/8", b"skin");
            install(&data, 2, "Skin", &i.to_string(), &root(&data).join(".unpack-2")).unwrap();
        }
        toggler.join().unwrap();
        drop(held);
        let mods = load(&data);
        assert_eq!(mods.iter().map(|m| (m.id, m.enabled)).collect::<Vec<_>>(), [(1, true), (2, true)]);
        assert_eq!(mods[1].version, "19");
        std::fs::remove_dir_all(data).unwrap();
    }

    /// one install of a mod at a time; another mod's is not held up, and the claim goes w/ the install
    #[test]
    fn one_install_per_mod_at_a_time() {
        let data = temp("claim");
        let first = claim(&data, 5).unwrap();
        assert_eq!(claim(&data, 5).err().as_deref(), Some("This mod is already being installed."));
        let other = claim(&data, 6).unwrap();
        drop(first);
        drop(claim(&data, 5).unwrap());
        drop(other);
        assert!(std::fs::read_dir(root(&data)).unwrap().next().is_none(), "no claim file is left");
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn archives_are_told_apart_by_what_they_hold() {
        let dir = temp("classify");
        // romfs under a title id, beside a readme
        put(&dir, "skin/Mod 1.0/0004000000055D00/romfs/a/0/8/8", b"m");
        put(&dir, "skin/Mod 1.0/readme.txt", b"r");
        let found = classify(&dir.join("skin")).unwrap();
        assert!(found.romfs.unwrap().ends_with("0004000000055D00/romfs") && !found.loose);
        assert_eq!((found.titles, found.textures), (vec![0x0004_0000_0005_5D00], 0));
        // loose RomFS paths, no romfs folder
        put(&dir, "loose/Blastoise Shirt/a/0/8/9", b"m");
        let found = classify(&dir.join("loose")).unwrap();
        assert!(found.loose && found.romfs.unwrap().ends_with("Blastoise Shirt"));
        // Citra's romfs + romfs_ext
        put(&dir, "ext/romfs/a/0/0/7", b"m");
        put(&dir, "ext/romfs_ext/a/1/1/2.ips", b"p");
        assert!(classify(&dir.join("ext")).unwrap().romfs_ext.unwrap().ends_with("romfs_ext"));
        // a texture pack, with preview pictures that aren't textures
        put(&dir, "pack/load/textures/0004000000055D00/tex1_128x256_AB83936179A3D677_1_mip0.png", b"p");
        put(&dir, "pack/Preview Image 1.png", b"p");
        let found = classify(&dir.join("pack")).unwrap();
        assert_eq!((found.textures, found.romfs), (1, None));
        // code, a save, loose GARC entries: not installable, each says why
        put(&dir, "code/exefs/code.bin", b"c");
        put(&dir, "code/romfs/DllField.cro", b"c");
        put(&dir, "ips/Pokeradar ++/code.ips", b"c");
        put(&dir, "save/main", b"s");
        put(&dir, "bins/3783.bin", b"b");
        for (name, says) in [("code", "code"), ("ips", "code.ips"), ("save", "no game files"), ("bins", "no game files")] {
            assert!(classify(&dir.join(name)).unwrap_err().contains(says), "{name}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn install_enable_disable_and_remove() {
        let data = temp("install");
        let unpacked = |name: &str| {
            let at = root(&data).join(format!(".unpack-{name}"));
            std::fs::create_dir_all(&at).unwrap();
            at
        };
        let skin = unpacked("skin");
        put(&skin, "MOD/romfs/a/0/8/8", b"skin");
        put(&skin, "MOD/romfs/a/0/7/4", b"text");
        put(&skin, "MOD/README.txt", b"r");
        let m = install(&data, 538241, "Hoodie Mod", "1.0", &skin).unwrap();
        assert!(m.enabled && m.titles.is_empty() && !skin.exists());
        let dir = folder(&data, 538241);
        assert_eq!(files(&dir), ["a/0/7/4", "a/0/8/8"]);
        assert_eq!(offline_file(&dir).as_deref(), Some("a/0/7/4"), "the English text is game data");

        let pack = unpacked("pack");
        put(&pack, "Pokemon/tex1_128x256_4779B02AE4D6AD81_0.png", b"p");
        install(&data, 508114, "Galaxy Chespin", "", &pack).unwrap();
        assert!(folder(&data, 508114).join("textures/Pokemon/tex1_128x256_4779B02AE4D6AD81_0.png").is_file());
        assert_eq!(offline_file(&folder(&data, 508114)), None);

        let loose = unpacked("loose");
        put(&loose, "Blastoise Shirt/a/0/8/8", b"shirt");
        put(&loose, "Blastoise Shirt/readme.txt", b"r");
        install(&data, 518343, "Blastoise Shirt", "", &loose).unwrap();
        assert_eq!(files(&folder(&data, 518343)), ["a/0/8/8"], "only the a folder is taken");
        assert_eq!(offline_file(&folder(&data, 518343)), None);

        let order = |data: &Path| load(data).iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(order(&data), [538241, 508114, 518343]);
        assert_eq!(clashes(&data, &load(&data)), [("a/0/8/8".to_owned(), vec![538241, 518343])]);

        // installed again: its place kept, its files replaced
        let again = unpacked("again");
        put(&again, "romfs/a/0/8/8", b"skin 2");
        install(&data, 538241, "Hoodie Mod", "2.0", &again).unwrap();
        assert_eq!(order(&data), [538241, 508114, 518343]);
        assert_eq!(files(&folder(&data, 538241)), ["a/0/8/8"]);

        set_enabled(&data, 518343, false).unwrap();
        assert!(clashes(&data, &load(&data)).is_empty());
        assert_eq!(enabled(&data, 0x0004_0000_0005_5D00).iter().map(|(m, _)| m.id).collect::<Vec<_>>(), [538241, 508114]);
        remove(&data, 508114).unwrap();
        assert!(!folder(&data, 508114).exists());
        assert_eq!(order(&data), [538241, 518343]);
        // a code mod is refused and leaves nothing
        let code = unpacked("code");
        put(&code, "exefs/code.bin", b"c");
        assert!(install(&data, 486567, "Shiny Lock Removal", "", &code).is_err());
        assert!(!folder(&data, 486567).exists() && order(&data).len() == 2);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn a_mod_for_one_title_is_left_out_of_the_other() {
        let m = Installed { id: 1, name: "x only".into(), version: String::new(), enabled: true, titles: vec![0x0004_0000_0005_5D00] };
        assert!(m.for_title(0x0004_0000_0005_5D00) && !m.for_title(0x0004_0000_0005_5E00));
        assert!(Installed { titles: Vec::new(), ..m }.for_title(0x0004_0000_0005_5E00));
    }
}
