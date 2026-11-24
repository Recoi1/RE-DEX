//! GameBanana's public API (apiv11) for the start screen's Mods screen: the mods of "Pokemon X and Y" (game 7496,
//! one page for both games), what the RE:DEX developers tested (`data/mod_compat.json` in the public repository, read
//! from GitHub), and installing one: its archive downloaded from GameBanana, unpacked and put in place by
//! `xr_hle::modlist`. Nothing is hosted by RE:DEX.
//!
//! Requests go through the system's curl (Windows has had it since 2018) over https only, with a plain
//! `RE:DEX/<version>` User-Agent and nothing about the player. Downloads only ever come from GameBanana's own hosts:
//! redirects are followed here, one at a time, and one to anywhere else is refused.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::Value;
use xr_hle::modlist;

/// GameBanana's id for Pokémon X and Y
pub const GAME: u32 = 7496;
/// the list of tested mods, from the public repository
pub const COMPAT_URL: &str = "https://raw.githubusercontent.com/Recoi1/RE-DEX/main/data/mod_compat.json";
/// how many mods a page of the list asks for, and the most pages asked for
const PER_PAGE: u32 = 50;
pub const MAX_PAGES: u32 = 10;

pub fn user_agent() -> String {
    format!("RE:DEX/{}", env!("CARGO_PKG_VERSION"))
}

/// a page of the list, most liked first
pub fn list_url(page: u32) -> String {
    format!("https://gamebanana.com/apiv11/Mod/Index?_nPerpage={PER_PAGE}&_nPage={page}&_aFilters%5BGeneric_Game%5D={GAME}&_sSort=Generic_MostLiked")
}

/// a mod's own page in the API: its files and version
pub fn profile_url(id: u64) -> String {
    format!("https://gamebanana.com/apiv11/Mod/{id}/ProfilePage")
}

/// the host of an https URL, None for anything else (or one w/ a login or a port in it)
fn https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    (!host.is_empty() && !host.contains(['@', ':', '\\'])).then_some(host)
}

/// is url on GameBanana (gamebanana.com or one of its hosts, like files.gamebanana.com)
pub fn gamebanana(url: &str) -> bool {
    https_host(url).is_some_and(|h| {
        let h = h.to_ascii_lowercase();
        h == "gamebanana.com" || h.ends_with(".gamebanana.com")
    })
}

/// is url one this screen may ask: GameBanana, or the compatibility list
fn allowed(url: &str) -> bool {
    gamebanana(url) || url == COMPAT_URL
}

/// a mod in the list
#[derive(Debug, Clone, PartialEq)]
pub struct Mod {
    pub id: u64,
    pub name: String,
    pub author: String,
    pub category: String,
    pub likes: u64,
    /// its page on GameBanana
    pub page: String,
    /// its first picture, small (220 px wide)
    pub thumb: Option<String>,
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().trim().to_owned()
}

/// a page of the list: its mods, and whether it was the last page
pub fn parse_list(json: &[u8]) -> Result<(Vec<Mod>, bool), String> {
    let v: Value = serde_json::from_slice(json).map_err(|e| format!("GameBanana sent something unexpected ({e})"))?;
    let records = v["_aRecords"].as_array().ok_or("GameBanana sent something unexpected (no list of mods)")?;
    let mods = records
        .iter()
        .filter_map(|r| {
            let image = &r["_aPreviewMedia"]["_aImages"][0];
            let file = ["_sFile220", "_sFile100", "_sFile"].iter().map(|k| text(&image[*k])).find(|f| !f.is_empty());
            let thumb = file.map(|f| format!("{}/{f}", text(&image["_sBaseUrl"]))).filter(|u| gamebanana(u));
            let page = text(&r["_sProfileUrl"]);
            Some(Mod {
                id: r["_idRow"].as_u64()?,
                name: Some(text(&r["_sName"])).filter(|n| !n.is_empty())?,
                author: text(&r["_aSubmitter"]["_sName"]),
                category: text(&r["_aRootCategory"]["_sName"]),
                likes: r["_nLikeCount"].as_u64().unwrap_or(0),
                page: if gamebanana(&page) { page } else { String::new() },
                thumb,
            })
        })
        .collect();
    Ok((mods, v["_aMetadata"]["_bIsComplete"].as_bool().unwrap_or(true)))
}

/// a file of a mod's
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub name: String,
    pub size: u64,
    pub url: String,
}

/// a mod's files and version, from its ProfilePage
pub fn parse_files(json: &[u8]) -> Result<(Vec<File>, String), String> {
    let v: Value = serde_json::from_slice(json).map_err(|e| format!("GameBanana sent something unexpected ({e})"))?;
    let files = v["_aFiles"].as_array().map(Vec::as_slice).unwrap_or_default();
    let files = files.iter().map(|f| File { name: text(&f["_sFile"]), size: f["_nFilesize"].as_u64().unwrap_or(0), url: text(&f["_sDownloadUrl"]) }).collect();
    Ok((files, text(&v["_sVersion"])))
}

/// the file to install: the first archive (.zip, .7z, .rar) GameBanana serves
pub fn archive(files: &[File]) -> Option<&File> {
    files.iter().find(|f| {
        let name = f.name.to_ascii_lowercase();
        [".zip", ".7z", ".rar"].iter().any(|ext| name.ends_with(ext)) && gamebanana(&f.url)
    })
}

/// what the RE:DEX developers found
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Verified,
    Partial,
    Broken,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tested {
    pub status: Status,
    pub note: String,
}

/// the compatibility list: `{"mods": {"<GameBanana id>": {"status": "verified|partial|broken", "note": "..."}}}`;
/// anything else in it is left out
pub fn parse_compat(json: &[u8]) -> HashMap<u64, Tested> {
    let Ok(v) = serde_json::from_slice::<Value>(json) else { return HashMap::new() };
    let Some(mods) = v["mods"].as_object() else { return HashMap::new() };
    mods.iter()
        .filter_map(|(id, t)| {
            let status = match t["status"].as_str()? {
                "verified" => Status::Verified,
                "partial" => Status::Partial,
                "broken" => Status::Broken,
                _ => return None,
            };
            Some((id.parse().ok()?, Tested { status, note: text(&t["note"]) }))
        })
        .collect()
}

/// the system's curl, from the system folder on Windows so no curl.exe in the PATH or the current folder runs instead
fn curl() -> Command {
    let exe = if cfg!(windows) {
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("System32").join("curl.exe")
    } else {
        PathBuf::from("curl")
    };
    let mut c = Command::new(exe);
    // https only, no redirects (download follows them itself), no [] {} expansion, a plain User-Agent, no config file read
    c.args(["-q", "--silent", "--show-error", "--globoff", "--proto", "=https", "--connect-timeout", "15", "--user-agent"]).arg(user_agent());
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // no console window flashing up over the start screen
        c.creation_flags(0x0800_0000);
    }
    c
}

/// a small answer from url (the API, a picture, the compatibility list), at most `max` bytes
pub fn fetch(url: &str, max: u64) -> Result<Vec<u8>, String> {
    if !allowed(url) {
        return Err(format!("refused to ask {url}"));
    }
    let child = curl()
        .args(["--fail", "--max-time", "30", "--max-filesize"])
        .arg(max.to_string())
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl could not be started ({e})"))?;
    modlist::tie_to_us(&child);
    let out = child.wait_with_output().map_err(|e| format!("curl could not be started ({e})"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(if why.is_empty() { "could not reach GameBanana".into() } else { format!("could not reach GameBanana ({why})") });
    }
    Ok(out.stdout)
}

/// downloads url into dest (at most `max` bytes), following GameBanana's redirects one at a time and refusing one to
/// any other host; `got` counts the bytes as they come, `cancel` stops it
pub fn download(url: &str, dest: &Path, max: u64, cancel: &AtomicBool, got: &AtomicU64) -> Result<(), String> {
    let mut url = url.to_owned();
    for _ in 0..5 {
        if !gamebanana(&url) {
            return Err("the download pointed away from GameBanana, so it was stopped".into());
        }
        let mut child = curl()
            .args(["--fail", "--max-time", "3600", "--write-out", "%{http_code} %{redirect_url}", "--max-filesize"])
            .arg(max.to_string())
            .arg("--output")
            .arg(dest)
            .arg(&url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("curl could not be started ({e})"))?;
        // closing the window takes the download w/ it
        modlist::tie_to_us(&child);
        loop {
            got.store(std::fs::metadata(dest).map_or(0, |m| m.len()), Ordering::Relaxed);
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("cancelled".into());
            }
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("the download failed ({})", String::from_utf8_lossy(&out.stderr).trim()));
        }
        let written = String::from_utf8_lossy(&out.stdout).into_owned();
        let (code, next) = written.split_once(' ').unwrap_or((&written, ""));
        match code {
            "200" => return Ok(()),
            "301" | "302" | "303" | "307" | "308" if !next.trim().is_empty() => url = next.trim().to_owned(),
            _ => return Err(format!("GameBanana answered {code}")),
        }
    }
    Err("too many redirects".into())
}

/// where an install is
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Asking,
    Downloading,
    Unpacking,
}

/// installs mod m: its first archive downloaded into a new folder, unpacked into another (both under the installed
/// mods' folder, removed after), then put in place and turned on. `stage` hears where it is; `got` counts the bytes
pub fn install(data: &Path, m: &Mod, stage: &dyn Fn(Stage, u64), cancel: &AtomicBool, got: &AtomicU64) -> Result<modlist::Installed, String> {
    let _claim = modlist::claim(data, m.id)?;
    stage(Stage::Asking, 0);
    let (files, version) = parse_files(&fetch(&profile_url(m.id), 4 << 20)?)?;
    let file = archive(&files).ok_or("This mod has no .zip, .7z or .rar file on GameBanana to install. Open its page to see how to get it.")?;
    let root = modlist::root(data);
    let (fetched, unpacked) = (root.join(format!(".download-{}", m.id)), root.join(format!(".unpack-{}", m.id)));
    for dir in [&fetched, &unpacked] {
        let _ = std::fs::remove_dir_all(dir);
    }
    std::fs::create_dir_all(&fetched).map_err(|e| format!("We couldn't make a folder for the download: {e}"))?;
    let result = (|| {
        let ext = file.name.rsplit('.').next().unwrap_or("zip").to_ascii_lowercase();
        let archive = fetched.join(format!("archive.{ext}"));
        stage(Stage::Downloading, file.size);
        download(&file.url, &archive, modlist::MAX_UNPACKED, cancel, got)?;
        stage(Stage::Unpacking, file.size);
        modlist::unpack(&archive, &unpacked, modlist::MAX_UNPACKED, cancel)?;
        modlist::install(data, m.id, &m.name, &version, &unpacked)
    })();
    // the archive and anything left of it go, whatever happened
    for dir in [&fetched, &unpacked] {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

/// opens a mod's page in the default browser: only https://gamebanana.com/ pages, and only plain URL characters
pub fn open_page(url: &str) {
    let plain = url.chars().all(|c| c.is_ascii_alphanumeric() || ":/._-?=&%#".contains(c));
    if !url.starts_with("https://gamebanana.com/") || !plain {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // explorer opens a URL in the default browser and takes nothing else from it
        let _ = Command::new("explorer.exe").arg(url).creation_flags(0x0800_0000).spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// made-up answers in the shape GameBanana's apiv11 gives
    const LIST: &str = r#"{"_aMetadata": {"_nRecordCount": 3, "_bIsComplete": false, "_nPerpage": 50},
      "_aRecords": [
        {"_idRow": 111, "_sName": " A Skin ", "_sProfileUrl": "https://gamebanana.com/mods/111", "_nLikeCount": 78,
         "_aSubmitter": {"_sName": "Someone"}, "_aRootCategory": {"_sName": "Skins"},
         "_aPreviewMedia": {"_aImages": [{"_sBaseUrl": "https://images.gamebanana.com/img/ss/mods", "_sFile": "a.jpg", "_sFile220": "220-90_a.jpg"}]}},
        {"_idRow": 222, "_sName": "No picture", "_sProfileUrl": "https://evil.example/mods/222", "_aPreviewMedia": []},
        {"_sName": "no id"},
        {"_idRow": 333, "_sName": ""},
        {"_idRow": 444, "_sName": "Elsewhere picture", "_sProfileUrl": "https://gamebanana.com/mods/444",
         "_aPreviewMedia": {"_aImages": [{"_sBaseUrl": "https://evil.example", "_sFile100": "b.jpg"}]}}
      ]}"#;

    #[test]
    fn the_list_parses_and_keeps_only_gamebanana_links() {
        let (mods, complete) = parse_list(LIST.as_bytes()).unwrap();
        assert!(!complete);
        assert_eq!(mods.iter().map(|m| m.id).collect::<Vec<_>>(), [111, 222, 444]);
        let a = &mods[0];
        assert_eq!((a.name.as_str(), a.author.as_str(), a.category.as_str(), a.likes), ("A Skin", "Someone", "Skins", 78));
        assert_eq!(a.thumb.as_deref(), Some("https://images.gamebanana.com/img/ss/mods/220-90_a.jpg"));
        assert_eq!((mods[1].page.as_str(), mods[1].thumb.as_ref(), mods[1].likes), ("", None, 0));
        assert_eq!(mods[2].thumb, None);
        assert!(parse_list(b"<html>").is_err());
        assert!(parse_list(b"{}").is_err());
    }

    #[test]
    fn a_mods_files_give_the_first_archive_on_gamebanana() {
        let page = r#"{"_sVersion": "V1.0", "_aFiles": [
            {"_sFile": "readme.txt", "_nFilesize": 10, "_sDownloadUrl": "https://gamebanana.com/dl/1"},
            {"_sFile": "mod.ZIP", "_nFilesize": 6910021, "_sDownloadUrl": "https://elsewhere.example/dl/2"},
            {"_sFile": "mod_lite.rar", "_nFilesize": 100, "_sDownloadUrl": "https://gamebanana.com/dl/3"},
            {"_sFile": "mod.7z", "_nFilesize": 5, "_sDownloadUrl": "https://gamebanana.com/dl/4"}]}"#;
        let (files, version) = parse_files(page.as_bytes()).unwrap();
        assert_eq!(version, "V1.0");
        assert_eq!(archive(&files).map(|f| f.url.as_str()), Some("https://gamebanana.com/dl/3"));
        assert_eq!(archive(&files[..2]), None);
        assert_eq!(parse_files(b"{}").unwrap().0, vec![]);
    }

    #[test]
    fn the_compat_list_parses_and_skips_what_it_cant_read() {
        let json = r#"{"version": 1, "mods": {
            "610259": {"status": "verified", "note": "skin works"},
            "578206": {"status": "partial"},
            "1": {"status": "great"}, "x": {"status": "broken"}, "2": {"status": "broken", "note": "crashes"}}}"#;
        let tested = parse_compat(json.as_bytes());
        assert_eq!(tested.len(), 3);
        assert_eq!(tested[&610259], Tested { status: Status::Verified, note: "skin works".into() });
        assert_eq!(tested[&578206].status, Status::Partial);
        assert_eq!(tested[&2].status, Status::Broken);
        assert!(parse_compat(b"404: Not Found").is_empty());
    }

    #[test]
    fn only_gamebanana_hosts_over_https() {
        for ok in ["https://gamebanana.com/dl/1", "https://files.gamebanana.com/mods/x.zip", "https://images.gamebanana.com/a.jpg", "https://GameBanana.com/mods/1"] {
            assert!(gamebanana(ok), "{ok}");
        }
        for bad in [
            "http://gamebanana.com/dl/1",
            "https://gamebanana.com.evil.example/x",
            "https://evilgamebanana.com/x",
            "https://user@gamebanana.com/x",
            "https://evil.example/?gamebanana.com",
            "https://gamebanana.com:8443/x",
            "file:///C:/x",
            "https://",
        ] {
            assert!(!gamebanana(bad), "{bad}");
        }
        assert!(allowed(COMPAT_URL) && !allowed("https://raw.githubusercontent.com/someone/else/main/x.json"));
        assert!(fetch("https://evil.example/x", 10).unwrap_err().contains("refused"));
        let (cancel, got) = (AtomicBool::new(false), AtomicU64::new(0));
        assert!(download("https://evil.example/x.zip", Path::new("x.zip"), 10, &cancel, &got).unwrap_err().contains("away from GameBanana"));
        assert!(user_agent().starts_with("RE:DEX/") && !user_agent().contains(' '));
    }
}
