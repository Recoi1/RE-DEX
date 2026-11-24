//! Pokémon Storage: PKVault (github.com/Chnapy/PKVault, a separate GPLv3 program by its own authors) run beside RE:DEX
//! as a program of its own, never linked into or shipped with RE:DEX. The first use downloads its pinned Windows release
//! from its GitHub releases into `<data>/tools/pkvault/<version>/` (the file's SHA-256 checked before it ever runs, and
//! at every start), points PKVault's settings at every character's saves (its database, storage and backups in
//! `<data>/tools/pkvault/data`), backs those saves up and starts it. A game and PKVault never have the saves at once:
//! Storage waits while a game holds the data folder's lock, and Play (and a game started any other way) waits while the
//! PKVault started from here runs.
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use egui::{pos2, vec2, Align, Align2, Color32, Layout, Rect, Ui};
use serde_json::{Map, Value};

use super::characters::{self, Store};
use super::glass::{Glass, BODY, TITLE};
use super::{at, dim, text, Start};

pub const VERSION: &str = "2.5.3";
const ASSET: &str = "PKVault-2.5.3-x64.exe";
const URL: &str = "https://github.com/Chnapy/PKVault/releases/download/v2.5.3/PKVault-2.5.3-x64.exe";
/// the official asset's SHA-256 (GitHub's digest for the release asset, and the file downloaded on 2026-11-24)
const SHA256: &str = "e17204a4c359c4589ba9953635f9eaff2495780f51910ec875ff48f89b9e2cae";
const SIZE: u64 = 64_582_220;
const REPO: &str = "https://github.com/Chnapy/PKVault";
/// where a download may come from: the release link, and the hosts GitHub sends release files from
const HOSTS: [&str; 3] = ["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];

/// `<data>/tools/pkvault`
fn root(data: &Path) -> PathBuf {
    data.join("tools").join("pkvault")
}

/// the pinned release's exe, alone in its version's folder (PKVault keeps its settings and logs beside it)
fn exe(data: &Path) -> PathBuf {
    root(data).join(VERSION).join(ASSET)
}

/// the download until its hash is checked
fn part(data: &Path) -> PathBuf {
    exe(data).with_extension("exe.part")
}

/// the pid of the PKVault started from here, while it may run
fn running_file(data: &Path) -> PathBuf {
    root(data).join("running")
}

/// PKVault reads `config/pkvault.json` in its working directory, which it makes its exe's folder
fn settings_file(data: &Path) -> PathBuf {
    root(data).join(VERSION).join("config").join("pkvault.json")
}

/// the host of an https URL, None for any other scheme
fn https_host(url: &str) -> Option<&str> {
    url.strip_prefix("https://")?.split(['/', '?', '#']).next()
}

/// an https URL on one of `HOSTS` (a user or a port in it makes the host match none)
fn allowed(url: &str) -> bool {
    https_host(url).is_some_and(|h| HOSTS.iter().any(|a| h.eq_ignore_ascii_case(a)))
}

/// what one request got: the file, or where to go next
#[derive(Debug)]
pub enum Hop {
    Done,
    Redirect(String),
}

/// `url` into `out`, a hop at a time (`hop` makes one request, never following a redirect itself), each hop to an
/// allowed host
fn fetch(url: &str, out: &Path, mut hop: impl FnMut(&str, &Path) -> Result<Hop, String>) -> Result<(), String> {
    let mut url = url.to_owned();
    for _ in 0..6 {
        if !allowed(&url) {
            log::warn!("pkvault: refused a download from {url}");
            return Err(format!("The download was sent to {}, not GitHub, so it was stopped.", https_host(&url).unwrap_or("a non-https address")));
        }
        match hop(&url, out)? {
            Hop::Done => return Ok(()),
            Hop::Redirect(next) => url = next,
        }
    }
    Err("The download was redirected too many times.".into())
}

/// one request with Windows' own curl.exe: https only, RE:DEX's user agent, no redirect followed
#[cfg(windows)]
fn curl(url: &str, out: &Path) -> Result<Hop, String> {
    use std::os::windows::process::CommandExt;
    let system = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let agent = format!("RE:DEX/{}", env!("CARGO_PKG_VERSION"));
    let o = std::process::Command::new(system.join(r"System32\curl.exe"))
        .args(["-sS", "--proto", "=https", "--tlsv1.2", "--connect-timeout", "30", "--max-time", "1800", "-A", &agent, "-w", "%{http_code} %{redirect_url}", "-o"])
        .arg(out)
        .arg(url)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("We couldn't start the download ({e})."))?;
    if !o.status.success() {
        return Err(format!("The download failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    let said = String::from_utf8_lossy(&o.stdout);
    let (code, next) = said.split_once(' ').unwrap_or((&said, ""));
    match code {
        "200" => Ok(Hop::Done),
        c if c.starts_with('3') && !next.is_empty() => Ok(Hop::Redirect(next.to_owned())),
        c => Err(format!("GitHub answered {c} to the download.")),
    }
}

#[cfg(not(windows))]
fn curl(_url: &str, _out: &Path) -> Result<Hop, String> {
    Err("Pokémon Storage is Windows only for now.".into())
}

fn sha256(file: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    std::io::copy(&mut std::fs::File::open(file)?, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// the file is the official release; else it is deleted
fn verify(file: &Path, want: &str) -> Result<(), String> {
    let got = sha256(file).map_err(|e| format!("We couldn't read the download ({e})."))?;
    if got != want {
        let _ = std::fs::remove_file(file);
        log::warn!("pkvault: {} has SHA-256 {got}, the release has {want}: deleted", file.display());
        return Err("The download didn't match PKVault's official release, so it was deleted.".into());
    }
    Ok(())
}

/// PKVault's exe: downloaded the first time, its hash checked every time (a file that changed is downloaded again)
fn install(data: &Path) -> Result<PathBuf, String> {
    install_from(data, URL, SHA256, curl)
}

fn install_from(data: &Path, url: &str, sha: &str, hop: impl FnMut(&str, &Path) -> Result<Hop, String>) -> Result<PathBuf, String> {
    let exe = exe(data);
    if exe.is_file() {
        match verify(&exe, sha) {
            Ok(()) => return Ok(exe),
            Err(_) => log::warn!("pkvault: {} changed; downloading it again", exe.display()),
        }
    }
    let part = part(data);
    std::fs::create_dir_all(root(data).join(VERSION)).map_err(|e| format!("We couldn't make PKVault's folder ({e})."))?;
    log::info!("pkvault: downloading {url}");
    if let Err(e) = fetch(url, &part, hop).and_then(|()| verify(&part, sha)) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &exe).map_err(|e| format!("We couldn't keep the download ({e})."))?;
    Ok(exe)
}

/// a path as PKVault's globs take it: absolute, forward slashes, an upper-case drive letter (its matcher groups globs
/// by an upper-case one)
fn glob_path(p: &Path) -> String {
    let mut s = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().replace('\\', "/");
    if s.as_bytes().get(1) == Some(&b':') {
        s[..1].make_ascii_uppercase();
    }
    s
}

/// PKVault's settings (`existing`: the file as it is, None when there is none) with every character's saves in
/// `SAVE_GLOBS`: the entries the player made stay as they are, and ours (the ones under `<data>/characters`) are made
/// again from `ids`, so a removed character's saves go. A setting missing is filled in: its database, storage and
/// backups under `<data>/tools/pkvault/data`. A file that is not a JSON object is left alone (an error).
fn merged_settings(existing: Option<&str>, data: &Path, ids: &[u64]) -> Result<String, String> {
    let mut v: Map<String, Value> = match existing {
        None => Map::new(),
        Some(t) => serde_json::from_str(t).map_err(|e| format!("PKVault's settings file can't be read ({e}). Fix or delete {}.", settings_file(data).display()))?,
    };
    let store = |d: &str| Value::String(glob_path(&root(data).join("data").join(d)));
    for (key, dir) in [("DB_PATH", "db"), ("STORAGE_PATH", "storage"), ("BACKUP_PATH", "backup")] {
        v.entry(key).or_insert_with(|| store(dir));
    }
    v.entry("PKM_EXTERNAL_GLOBS").or_insert_with(|| Value::Array(Vec::new()));
    // PKVault's cheat features stay hidden unless the player turns them on in its settings
    v.entry("HIDE_CHEATS").or_insert(Value::Bool(true));
    v.entry("SKIP_LEGALITY_CHECKS").or_insert(Value::Bool(false));
    let ours = glob_path(&data.join("characters")).to_lowercase() + "/";
    let mut globs: Vec<Value> = match v.remove("SAVE_GLOBS") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.into_iter().filter(|g| !g.as_str().is_some_and(|s| s.trim().replace('\\', "/").to_lowercase().starts_with(&ours))).collect(),
        Some(_) => return Err(format!("PKVault's save folders setting isn't a list. Fix or delete {}.", settings_file(data).display())),
    };
    globs.extend(ids.iter().map(|&id| Value::String(glob_path(&characters::user_dir(data, id)) + "/savedata/*/main")));
    v.insert("SAVE_GLOBS".into(), Value::Array(globs));
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

/// writes `merged_settings` whole (a temporary file, then a rename)
fn configure(data: &Path, ids: &[u64]) -> Result<(), String> {
    let file = settings_file(data);
    let existing = match std::fs::read_to_string(&file) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("We couldn't read PKVault's settings ({e}).")),
    };
    let text = merged_settings(existing.as_deref(), data, ids)?;
    let tmp = file.with_extension("json.new");
    std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))
        .and_then(|()| std::fs::write(&tmp, text))
        .and_then(|()| std::fs::rename(&tmp, &file))
        .map_err(|e| format!("We couldn't write PKVault's settings ({e})."))
}

/// Backs up every character's saves, points PKVault at them and starts it, holding the data folder's lock meanwhile (a
/// game that starts now waits for it, then sees PKVault running). Refused while a game holds the lock.
fn launch(data: &Path, exe: &Path) -> Result<Proc, String> {
    let _lock = DataLock::take(data, 0).ok_or("A game is open. Close it, then open Pokémon Storage.")?;
    if exe_in_use(exe) {
        return Err("PKVault is open already. Close it, then open Pokémon Storage.".into());
    }
    let store = Store::try_load(data).ok_or("We couldn't open your characters just now. Close anything else using RE:DEX, then try again.")?;
    let now = characters::now_ms() / 1000;
    for c in &store.characters {
        let n = characters::backup_saves(&characters::user_dir(data, c.id), now, 10)
            .map_err(|e| format!("We couldn't back up {}'s saves ({e}), so Pokémon Storage stayed closed.", c.name))?;
        log::info!("pkvault: character {:016x}: {n} save(s) backed up", c.id);
    }
    configure(data, &store.characters.iter().map(|c| c.id).collect::<Vec<_>>())?;
    let p = Proc::spawn(exe)?;
    // shortcut: without the file a start screen opened later doesn't see this PKVault (this one still does)
    if let Err(e) = std::fs::write(running_file(data), p.pid.to_string()) {
        log::warn!("pkvault: could not write {}: {e}", running_file(data).display());
    }
    log::info!("pkvault: started {} (pid {})", exe.display(), p.pid);
    Ok(p)
}

/// a PKVault is running from this data folder's tools: the one RE:DEX started, or one started any other way (the
/// taskbar, a double-click on the exe)
pub fn running(data: &Path) -> bool {
    find(&running_file(data), &exe(data)).is_some() || exe_in_use(&exe(data))
}

/// some process runs exe, however it was started: Windows refuses to open a running exe for writing (a sharing
/// violation). Opening it changes nothing
fn exe_in_use(exe: &Path) -> bool {
    const SHARING_VIOLATION: i32 = 32;
    matches!(std::fs::OpenOptions::new().write(true).open(exe), Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION))
}

/// the process named in `pid_file`, while it runs `exe` (a pid used again by another program is not it)
fn find(pid_file: &Path, exe: &Path) -> Option<Proc> {
    let pid: u32 = std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()?;
    let p = Proc::open(pid)?;
    let same = |a: &Path, b: &Path| std::fs::canonicalize(a).ok().zip(std::fs::canonicalize(b).ok()).is_some_and(|(a, b)| a.as_os_str().eq_ignore_ascii_case(b.as_os_str()));
    (p.alive() && p.image().is_some_and(|i| same(&i, exe))).then_some(p)
}

/// a game window holds this data folder's lock (the runtime's one-window-per-save-folder mutex)
pub(super) fn game_running(data: &Path) -> bool {
    DataLock::take(data, 0).is_none()
}

/// The data folder's named mutex, as the runtime names it: one game window per save folder, and no PKVault start meanwhile.
pub fn data_lock_name(data: &Path) -> Vec<u16> {
    let key = std::fs::canonicalize(data).unwrap_or_else(|_| data.to_path_buf()).to_string_lossy().to_lowercase();
    format!("Local\\redex-data-{:016x}", xr_hle::cro::code_hash(key.as_bytes())).encode_utf16().chain([0]).collect()
}

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CreateMutexW, OpenProcess, QueryFullProcessImageNameW, ReleaseMutex, WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};

/// the data folder's mutex, held until dropped (on the thread that took it)
#[cfg(windows)]
struct DataLock(HANDLE);

#[cfg(windows)]
impl DataLock {
    /// None when another process holds it after `wait_ms`, or it can't be made
    fn take(data: &Path, wait_ms: u32) -> Option<DataLock> {
        let name = data_lock_name(data);
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if h.is_null() {
            return None;
        }
        let r = unsafe { WaitForSingleObject(h, wait_ms) };
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            return Some(DataLock(h));
        }
        unsafe { CloseHandle(h) };
        None
    }
}

#[cfg(windows)]
impl Drop for DataLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

/// a process, by a handle of ours (its pid stays its own while the handle is open)
#[cfg(windows)]
pub(super) struct Proc {
    h: HANDLE,
    pid: u32,
}

#[cfg(windows)]
impl Proc {
    fn spawn(exe: &Path) -> Result<Proc, String> {
        use std::os::windows::io::IntoRawHandle;
        let child = std::process::Command::new(exe)
            .current_dir(exe.parent().unwrap_or(Path::new(".")))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("We couldn't start PKVault ({e})."))?;
        let pid = child.id();
        Ok(Proc { h: child.into_raw_handle() as HANDLE, pid })
    }

    fn open(pid: u32) -> Option<Proc> {
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        (!h.is_null()).then_some(Proc { h, pid })
    }

    fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.h, 0) == WAIT_TIMEOUT }
    }

    fn image(&self) -> Option<PathBuf> {
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(self.h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) } != 0;
        ok.then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
    }
}

#[cfg(windows)]
impl Drop for Proc {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.h) };
    }
}

// elsewhere there is no PKVault start, and no data folder lock to see
#[cfg(not(windows))]
struct DataLock;
#[cfg(not(windows))]
impl DataLock {
    fn take(_data: &Path, _wait_ms: u32) -> Option<DataLock> {
        Some(DataLock)
    }
}
#[cfg(not(windows))]
pub(super) struct Proc {
    pid: u32,
}
#[cfg(not(windows))]
impl Proc {
    fn spawn(_exe: &Path) -> Result<Proc, String> {
        Err("Pokémon Storage is Windows only for now.".into())
    }
    fn open(_pid: u32) -> Option<Proc> {
        None
    }
    fn alive(&self) -> bool {
        false
    }
    fn image(&self) -> Option<PathBuf> {
        None
    }
}

/// Pokémon Storage on the start screen
pub(super) enum Storage {
    Idle,
    /// the first use's note (what PKVault is, where it comes from) is up
    Asking,
    /// PKVault being downloaded, or its hash checked
    Getting(JoinHandle<Result<PathBuf, String>>),
    Open(Proc),
}

impl Storage {
    /// a PKVault started earlier (the start screen closed and opened again meanwhile) is still followed
    pub(super) fn new(data: &Path) -> Storage {
        find(&running_file(data), &exe(data)).map_or(Storage::Idle, Storage::Open)
    }

    /// why Play waits, None when it doesn't
    pub(super) fn blocks_play(&self) -> Option<&'static str> {
        match self {
            Storage::Open(_) => Some("Close Pokémon Storage to play"),
            Storage::Getting(_) => Some("Pokémon Storage is opening"),
            _ => None,
        }
    }

    /// the panel stands in for the character row
    pub(super) fn panel_up(&self) -> bool {
        matches!(self, Storage::Asking | Storage::Getting(_))
    }

    pub(super) fn asking(&self) -> bool {
        matches!(self, Storage::Asking)
    }
}

impl Start {
    /// each frame: a finished download or check starts PKVault; once PKVault closes, the characters are read again
    pub(super) fn pump_storage(&mut self) {
        match std::mem::replace(&mut self.storage, Storage::Idle) {
            Storage::Getting(job) if job.is_finished() => {
                let got = job.join().unwrap_or_else(|_| Err("Getting PKVault stopped unexpectedly.".into()));
                match got.and_then(|exe| launch(&self.data, &exe)) {
                    Ok(p) => self.storage = Storage::Open(p),
                    Err(e) => {
                        log::warn!("pkvault: {e}");
                        self.error = Some(e);
                    }
                }
            }
            Storage::Open(p) if !p.alive() => {
                let _ = std::fs::remove_file(running_file(&self.data));
                log::info!("pkvault: closed; reading the characters again");
                // PKVault changes saves, never characters.json; the cards and save ticks are read again all the same
                self.store = Store::load(&self.data);
                if self.character.is_none_or(|id| self.store.get(id).is_none()) {
                    self.character = self.store.characters.first().map(|c| c.id);
                }
                self.page = self.page.min(self.store.characters.len().saturating_sub(1) / 3);
                (self.faces, self.check_cards) = (Default::default(), true);
            }
            s => self.storage = s,
        }
    }

    fn get_storage(&mut self) {
        let data = self.data.clone();
        self.error = None;
        self.storage = Storage::Getting(std::thread::spawn(move || install(&data)));
    }

    /// "Pokémon Storage" beside the character row's title; greyed while a game or PKVault is open
    /// (`base`: the row's first focus item, the note's first button; the pill is `base + 9`)
    pub(super) fn storage_pill(&mut self, ui: &mut Ui, glass: &Glass, rect: Rect, base: usize) {
        if !cfg!(windows) {
            return;
        }
        let item = base + 9;
        let game = game_running(&self.data);
        let open = matches!(self.storage, Storage::Open(_));
        let label = if open { "Storage is open" } else { "Pokémon Storage" };
        if game || open {
            ui.painter().rect_filled(rect, rect.height() / 2.0, Color32::from_white_alpha(16));
            text(ui, rect.center(), Align2::CENTER_CENTER, label, 14.0, dim(BODY, 0.8));
            let why = if open { "Close it to play" } else { "Close the game to open it" };
            text(ui, pos2(rect.max.x + 12.0, rect.center().y), Align2::LEFT_CENTER, why, 13.0, BODY);
            self.focus_ring(ui, rect, item);
            return;
        }
        let hit = self.flat_pill(ui, glass, rect, 103, label);
        if hit | self.focus_ring(ui, rect, item) {
            if exe(&self.data).is_file() {
                self.get_storage();
            } else {
                (self.storage, self.focus) = (Storage::Asking, base);
            }
        }
    }

    /// in place of the character row: the first use's note (Download and open, Not now, PKVault on GitHub), or the
    /// download's progress
    pub(super) fn storage_panel(&mut self, ui: &mut Ui, glass: &Glass, low: f32, base: usize) {
        if let Storage::Getting(_) = self.storage {
            let have = std::fs::metadata(part(&self.data)).map_or(0, |m| m.len());
            let (line, fraction) = match have {
                0 => (format!("Checking PKVault {VERSION}"), None),
                n => (format!("Downloading PKVault {VERSION} from GitHub: {} of {} MB", n / 1_000_000, SIZE / 1_000_000), Some(n as f32 / SIZE as f32)),
            };
            glass.title(ui.painter(), pos2(32.0, low), "Opening Pokémon Storage", 14.0);
            at(ui, Rect::from_min_size(pos2(32.0, low + 32.0), vec2(560.0, 120.0)), Layout::top_down(Align::Min), |ui| {
                ui.label(egui::RichText::new(line).size(16.0).color(TITLE));
                ui.add_space(8.0);
                glass.progress(ui, fraction, true);
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Your characters' saves are backed up before it opens.").size(13.0).color(BODY));
            });
            return;
        }
        glass.title(ui.painter(), pos2(32.0, low), "Pokémon Storage", 14.0);
        let (mut go, mut later, mut repo) = (false, false, false);
        at(ui, Rect::from_min_size(pos2(32.0, low + 32.0), vec2(720.0, 140.0)), Layout::top_down(Align::Min), |ui| {
            let body = format!(
                "Pokémon Storage is PKVault, a separate GPLv3 program by its authors, downloaded from its official releases \
                 ({}\u{a0}MB, once). It shows every character's boxes and a Pokédex of them all. Your saves are backed up \
                 before it opens, and Play waits until you close it.",
                SIZE / 1_000_000
            );
            ui.label(egui::RichText::new(body).size(14.0).color(TITLE));
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                let r = glass.pill(ui, "Download and open", Some(184.0), true);
                go = self.item(ui, &r, base, 20.0);
                let r = glass.pill(ui, "Not now", Some(120.0), true);
                later = self.item(ui, &r, base + 1, 20.0);
                let r = glass.pill(ui, "PKVault on GitHub", Some(184.0), true);
                repo = self.item(ui, &r, base + 2, 20.0);
            });
        });
        if go {
            self.get_storage();
        } else if later || self.nav.back {
            self.storage = Storage::Idle;
        } else if repo {
            // the default browser, through Explorer (egui here opens no links itself)
            if let Err(e) = std::process::Command::new("explorer.exe").arg(REPO).spawn() {
                log::warn!("pkvault: could not open {REPO}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redex-pkvault-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn only_https_github_hosts_are_allowed() {
        assert!(allowed(URL));
        assert!(allowed("https://release-assets.githubusercontent.com/github-production-release-asset/1?sig=x"));
        assert!(allowed("https://objects.githubusercontent.com/a"));
        for bad in [
            "http://github.com/Chnapy/PKVault",
            "https://github.com.evil.example/x",
            "https://evil.example/github.com/x",
            "https://user@github.com/x",
            "https://github.com:8443/x",
            "ftp://github.com/x",
            "github.com/x",
        ] {
            assert!(!allowed(bad), "{bad}");
        }
    }

    /// the official bytes, through a redirect, become the exe; anything else is refused, and nothing is left behind
    #[test]
    fn a_download_is_kept_only_when_its_hash_and_hosts_are_right() {
        let data = temp("download");
        let good = b"the official release".to_vec();
        let sha = sha_of(&good);
        let asked = RefCell::new(Vec::new());
        let serve = |body: Vec<u8>, to: &'static str| {
            let asked = &asked;
            move |url: &str, out: &Path| {
                asked.borrow_mut().push(url.to_owned());
                if url == URL {
                    return Ok(Hop::Redirect(to.to_owned()));
                }
                std::fs::write(out, &body).unwrap();
                Ok(Hop::Done)
            }
        };
        // a redirect off GitHub: refused before it is asked for
        let why = install_from(&data, URL, &sha, serve(good.clone(), "https://evil.example/PKVault.exe")).unwrap_err();
        assert!(why.contains("evil.example"), "{why}");
        assert_eq!(*asked.borrow(), [URL], "the other host was never asked");
        // or to plain http
        assert!(install_from(&data, URL, &sha, serve(good.clone(), "http://release-assets.githubusercontent.com/x")).is_err());
        // the wrong bytes from the right place: deleted
        let why = install_from(&data, URL, &sha, serve(b"something else".to_vec(), "https://release-assets.githubusercontent.com/x")).unwrap_err();
        assert!(why.contains("didn't match"), "{why}");
        assert!(!exe(&data).exists() && !part(&data).exists());
        // the right ones: kept as the exe, and the next start only checks them
        let exe = install_from(&data, URL, &sha, serve(good.clone(), "https://release-assets.githubusercontent.com/x")).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), good);
        assert!(!part(&data).exists());
        let calls = asked.borrow().len();
        install_from(&data, URL, &sha, serve(good.clone(), "https://release-assets.githubusercontent.com/x")).unwrap();
        assert_eq!(asked.borrow().len(), calls, "a good exe is not downloaded again");
        // an exe changed on disk is not run: downloaded again
        std::fs::write(&exe, b"tampered").unwrap();
        install_from(&data, URL, &sha, serve(good.clone(), "https://release-assets.githubusercontent.com/x")).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), good);
        // a redirect loop ends
        let looped = |url: &str, _: &Path| Ok(Hop::Redirect(url.to_owned()));
        std::fs::remove_file(&exe).unwrap();
        assert!(install_from(&data, URL, &sha, looped).unwrap_err().contains("too many"));
        let _ = std::fs::remove_dir_all(&data);
    }

    /// a fresh file gets every setting; a file the player has keeps their entries and anything we don't know, and our
    /// own glob list is made again from the characters there are now
    #[test]
    fn settings_merge_keeps_the_players_config() {
        let data = temp("settings");
        let (a, b) = (0x1111_2222_3333_4444u64, 0xaaaa_bbbb_cccc_ddddu64);
        let fresh: Map<String, Value> = serde_json::from_str(&merged_settings(None, &data, &[a, b]).unwrap()).unwrap();
        let globs: Vec<&str> = fresh["SAVE_GLOBS"].as_array().unwrap().iter().map(|g| g.as_str().unwrap()).collect();
        assert_eq!(globs.len(), 2);
        assert!(globs[0].ends_with("/characters/1111222233334444/user/savedata/*/main") && !globs[0].contains('\\'), "{}", globs[0]);
        assert!(globs[0].as_bytes()[0].is_ascii_uppercase() && globs[0].as_bytes()[1] == b':', "a drive letter, upper case");
        assert!(fresh["DB_PATH"].as_str().unwrap().ends_with("/tools/pkvault/data/db"));
        assert_eq!((fresh["HIDE_CHEATS"].clone(), fresh["PKM_EXTERNAL_GLOBS"].clone()), (Value::Bool(true), Value::Array(vec![])));

        // the player's own: their globs, paths and language stay; our glob for b (since removed) goes, a's is made again
        let theirs = serde_json::json!({
            "DB_PATH": "D:/mine/db",
            "SAVE_GLOBS": ["E:/Citra/sdmc/**/main", globs[1], globs[0].to_lowercase().replace('/', "\\")],
            "STORAGE_PATH": "./storage",
            "BACKUP_PATH": "./backup",
            "HIDE_CHEATS": true,
            "SKIP_LEGALITY_CHECKS": true,
            "LANGUAGE": "fr",
            "SAVE_VERSION_OVERRIDES": {"12345": 24},
            "SOMETHING_NEWER": [1, 2]
        });
        let merged: Map<String, Value> = serde_json::from_str(&merged_settings(Some(&theirs.to_string()), &data, &[a]).unwrap()).unwrap();
        assert_eq!(merged["SAVE_GLOBS"], serde_json::json!(["E:/Citra/sdmc/**/main", globs[0]]));
        for key in ["DB_PATH", "STORAGE_PATH", "BACKUP_PATH", "HIDE_CHEATS", "SKIP_LEGALITY_CHECKS", "LANGUAGE", "SAVE_VERSION_OVERRIDES", "SOMETHING_NEWER"] {
            assert_eq!(merged[key], theirs[key], "{key}");
        }
        // a file that isn't settings is never written over
        assert!(merged_settings(Some("{not json"), &data, &[a]).is_err());
        assert!(merged_settings(Some(r#"{"SAVE_GLOBS": "C:/x"}"#), &data, &[a]).is_err());
        // configure writes it whole and leaves no temporary file
        configure(&data, &[a]).unwrap();
        configure(&data, &[a, b]).unwrap();
        let text = std::fs::read_to_string(settings_file(&data)).unwrap();
        assert_eq!(serde_json::from_str::<Map<String, Value>>(&text).unwrap()["SAVE_GLOBS"].as_array().unwrap().len(), 2);
        assert_eq!(std::fs::read_dir(settings_file(&data).parent().unwrap()).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&data);
    }

    fn character(data: &Path, store: &mut Store, name: &str) -> u64 {
        let look = xr_net::x::look::Look::default_for(1).appearance().unwrap();
        let id = store.create(name, look, 1).unwrap();
        let dir = characters::user_dir(data, id).join("savedata/0004000000055D00");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main"), name.repeat(100)).unwrap();
        id
    }

    /// Storage is refused while a game holds the data folder's lock (nothing backed up, nothing started); else every
    /// character's saves are backed up before PKVault starts, and it is followed until it ends
    #[cfg(windows)]
    #[test]
    fn launch_backs_up_first_and_never_beside_a_game() {
        let data = temp("launch");
        let mut store = Store::default();
        let ids = [character(&data, &mut store, "Serena"), character(&data, &mut store, "Calem")];
        store.save(&data).unwrap();
        // a stand-in for PKVault that ends at once: cmd with no input
        let exe = exe(&data);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::copy(Path::new(&std::env::var_os("SystemRoot").unwrap()).join(r"System32\cmd.exe"), &exe).unwrap();

        // a game window holds the lock (in another thread, as another process would: the mutex is the thread's)
        let (held, release) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
        let d = data.clone();
        let game = std::thread::spawn(move || {
            let lock = DataLock::take(&d, 0).unwrap();
            held.0.send(()).unwrap();
            release.1.recv().unwrap();
            drop(lock);
        });
        held.1.recv().unwrap();
        assert!(game_running(&data));
        let why = launch(&data, &exe).err().unwrap();
        assert!(why.contains("A game is open"), "{why}");
        assert!(!characters::user_dir(&data, ids[0]).with_file_name("backups").exists(), "nothing backed up");
        assert!(!running_file(&data).exists() && !settings_file(&data).exists(), "nothing configured or started");
        release.0.send(()).unwrap();
        game.join().unwrap();
        assert!(!game_running(&data));

        let p = launch(&data, &exe).unwrap();
        for id in ids {
            let backups = characters::user_dir(&data, id).with_file_name("backups").join("0004000000055D00");
            assert_eq!(std::fs::read_dir(backups).unwrap().count(), 1, "{id:016x} backed up");
        }
        let text = std::fs::read_to_string(settings_file(&data)).unwrap();
        assert!(ids.iter().all(|id| text.contains(&format!("{id:016x}/user/savedata/*/main"))), "{text}");
        assert_eq!(std::fs::read_to_string(running_file(&data)).unwrap(), p.pid.to_string());
        while p.alive() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!running(&data), "ended: Play is free again");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// the pid file is believed only while that process runs the exe it names, so Play waits for the PKVault started
    /// earlier and never for a stranger that got its pid
    #[cfg(windows)]
    #[test]
    fn a_running_pkvault_is_found_by_its_pid_and_exe() {
        let data = temp("find");
        let system = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32");
        let mut child = std::process::Command::new(system.join("cmd.exe"))
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let file = data.join("running");
        std::fs::write(&file, child.id().to_string()).unwrap();
        assert!(find(&file, &system.join("cmd.exe")).is_some());
        assert!(find(&file, &exe(&data)).is_none(), "another program's pid");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(find(&file, &system.join("cmd.exe")).is_none(), "ended");
        std::fs::write(&file, "junk").unwrap();
        assert!(find(&file, &system.join("cmd.exe")).is_none());
        assert!(!running(&data), "no file there");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// a PKVault started by hand is seen through its exe being in use: here a copy of cmd.exe stands in for it
    #[cfg(windows)]
    #[test]
    fn a_pkvault_started_by_hand_is_seen() {
        let data = temp("by-hand");
        let system = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32");
        let copy = data.join("PKVault.exe");
        std::fs::copy(system.join("cmd.exe"), &copy).unwrap();
        assert!(!exe_in_use(&copy), "not running");
        let mut child = std::process::Command::new(&copy).args(["/c", "ping -n 30 127.0.0.1 >nul"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        assert!(exe_in_use(&copy), "running, w/o a pid file");
        assert!(!exe_in_use(&data.join("missing.exe")));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!exe_in_use(&copy), "ended");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn play_waits_while_storage_opens_or_is_open() {
        assert_eq!(Storage::Idle.blocks_play(), None);
        assert_eq!(Storage::Asking.blocks_play(), None);
        assert!(Storage::Getting(std::thread::spawn(|| Err(String::new()))).blocks_play().is_some());
    }
}
