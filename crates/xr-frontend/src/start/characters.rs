//! The player's characters: `<data>/characters.json` (a name and a look each, the same in every game) and one save
//! folder per character, `<data>/characters/<id>/user`, which the game runs on with `redex --character <id>`.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use redex_proto::Character;
use redex_proto::{TITLE_X, TITLE_Y};
use xr_net::x::field::Appearance;

/// the size of X's and Y's `main` save file
/// how far ahead of the local clock a remote `changed` may be (clock skew) before the copy is dropped
const FUTURE_SLACK_MS: u64 = 5 * 60_000;
const MAIN_LEN: usize = 0x65600;

#[derive(Debug, Default)]
pub struct Store {
    pub characters: Vec<Character>,
    /// ids the player removed: the account's copies of them are not taken back in
    pub removed: Vec<u64>,
}

/// `characters.json` as written; a field this version does not know means a newer RE:DEX wrote it
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileForm {
    characters: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    name: String,
    look: String,
    created: u64,
    changed: u64,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len() % 2 == 0 && s.is_ascii()).then_some(())?;
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// an entry as read, None when its id, name or look is not one this version writes
fn character(e: Entry) -> Option<Character> {
    let id = (e.id.len() == 16).then(|| u64::from_str_radix(&e.id, 16).ok()).flatten()?;
    let look = unhex(&e.look).filter(|b| Appearance::from_bytes(b).is_some())?;
    (check_name(&e.name).as_deref() == Ok(e.name.as_str())).then_some(Character { id, name: e.name, look, created: e.created, changed: e.changed })
}

impl Store {
    /// missing: empty. Broken, unreadable or newer: renamed to characters.json.bad-<ms>, then empty. An entry with a bad
    /// id, name or look is dropped (logged; the file's bytes copied to characters.json.bad-<ms> and the rest saved over
    /// it, so they are copied once) and the rest stay. A caller that saves uses `try_load`.
    pub fn load(data: &Path) -> Store {
        Self::read(data).0
    }

    /// `load`, or None when characters.json is there but could not be read or kept aside now (another program may hold
    /// it for a moment): saving over it then would lose it.
    pub fn try_load(data: &Path) -> Option<Store> {
        let (store, safe) = Self::read(data);
        safe.then_some(store)
    }

    /// The store, and whether saving over characters.json would lose nothing.
    fn read(data: &Path) -> (Store, bool) {
        let file = data.join("characters.json");
        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Store::default(), true),
            Err(e) => return (Store::default(), aside(&file, &format!("unreadable ({e})"), true)),
        };
        let form: FileForm = match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(e) => return (Store::default(), aside(&file, &format!("broken or from a newer RE:DEX ({e})"), true)),
        };
        let mut store = Store::default();
        // a removed id that is not 16 hex digits stays out of the list anyway: it is no character's
        store.removed = form.removed.iter().filter(|id| id.len() == 16).filter_map(|id| u64::from_str_radix(id, 16).ok()).collect();
        let mut dropped = false;
        for e in form.characters {
            let what = format!("{} {:?}", e.id, e.name);
            match character(e).filter(|c| store.get(c.id).is_none()) {
                Some(c) => store.characters.push(c),
                None => {
                    log::warn!("characters: dropped {what}: a bad or repeated id, name or look");
                    dropped = true;
                }
            }
        }
        if dropped {
            if !aside(&file, "some entries were dropped", false) {
                return (store, false);
            }
            if let Err(e) = store.save(data) {
                log::warn!("characters: could not save characters.json without them: {e}");
            }
        }
        (store, true)
    }

    /// whole or not at all: a temporary file, then a rename. Refuses a look that fails `Appearance::from_bytes`.
    pub fn save(&self, data: &Path) -> Result<(), String> {
        if let Some(c) = self.characters.iter().find(|c| Appearance::from_bytes(&c.look).is_none()) {
            return Err(format!("{:?}'s look is out of the game's ranges", c.name));
        }
        let form = FileForm {
            characters: self
                .characters
                .iter()
                .map(|c| Entry { id: format!("{:016x}", c.id), name: c.name.clone(), look: hex(&c.look), created: c.created, changed: c.changed })
                .collect(),
            removed: self.removed.iter().map(|id| format!("{id:016x}")).collect(),
        };
        std::fs::create_dir_all(data).map_err(|e| e.to_string())?;
        let text = serde_json::to_string_pretty(&form).map_err(|e| e.to_string())?;
        // a temporary name of this process's own (a start screen and a game may save at once), on the disk before the
        // rename (else a power loss can leave an empty characters.json)
        let (tmp, file) = (data.join(format!("characters.json.new-{}", std::process::id())), data.join("characters.json"));
        let written = std::fs::File::create(&tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(text.as_bytes())?;
            f.sync_all()
        });
        written.and_then(|()| std::fs::rename(&tmp, &file)).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })
    }

    /// Takes the character off the list and remembers its id, so the account's copy (a world sends it at each login)
    /// does not bring it back. Its saves stay in `<data>/characters/<id>`. False for an unknown id. Not saved here.
    pub fn remove(&mut self, id: u64) -> bool {
        let n = self.characters.len();
        self.characters.retain(|c| c.id != id);
        if self.characters.len() == n {
            return false;
        }
        if !self.removed.contains(&id) {
            self.removed.push(id);
        }
        true
    }

    /// a new character under a random id; the name passes `check_name` and the look `validate`
    pub fn create(&mut self, name: &str, look: Appearance, now: u64) -> Result<u64, String> {
        let name = check_name(name)?;
        if !look.validate() {
            return Err("that look is out of the game's ranges".into());
        }
        let id = loop {
            let id = getrandom::u64().map_err(|e| format!("the OS random generator failed: {e}"))?;
            if id != 0 && self.get(id).is_none() {
                break id;
            }
        };
        self.characters.push(Character { id, name, look: look.to_bytes().to_vec(), created: now, changed: now });
        Ok(id)
    }

    /// The account's characters as the world server sent them, merged in by `redex_proto::merge` (the newest `changed`
    /// wins, a tie keeps ours). One whose look fails `Appearance::from_bytes`, whose name fails `check_name`, whose id
    /// is 0 or whose `changed` is more than `FUTURE_SLACK_MS` past `now` (a hostile one could otherwise win
    /// every merge) is dropped, and any other has `changed` clamped to `now`; a new id past 8 is ignored (logged), and one the player removed is left out. Whether the store
    /// changed; it is not saved here.
    pub fn merge_remote(&mut self, list: Vec<Character>, now: u64) -> bool {
        let mut changed = false;
        for c in list {
            if self.removed.contains(&c.id) {
                continue;
            }
            if c.id == 0 || c.changed > now.saturating_add(FUTURE_SLACK_MS) || Appearance::from_bytes(&c.look).is_none() || check_name(&c.name).as_deref() != Ok(c.name.as_str()) {
                log::warn!("characters: the account's {:016x} {:?} dropped: a bad id, name, look or time", c.id, c.name);
                continue;
            }
            // a copy never claims to be newer than now, so it cannot outrank an edit the player makes after the sync
            let c = Character { changed: c.changed.min(now), ..c };
            if self.get(c.id).is_none() && self.characters.len() >= redex_proto::MAX_CHARACTERS {
                log::warn!("characters: the account's {:016x} ignored: {} characters already", c.id, self.characters.len());
                continue;
            }
            changed |= redex_proto::merge(&mut self.characters, c);
        }
        changed
    }

    pub fn get(&self, id: u64) -> Option<&Character> {
        self.characters.iter().find(|c| c.id == id)
    }

    /// false for an unknown id or a look that fails `validate` (then nothing changes)
    pub fn set_look(&mut self, id: u64, look: Appearance, now: u64) -> bool {
        let Some(c) = self.characters.iter_mut().find(|c| c.id == id).filter(|_| look.validate()) else { return false };
        c.look = look.to_bytes().to_vec();
        c.changed = now;
        true
    }

    /// a new character from an X or Y save: its trainer's look and name ("Trainer" when there is none), and a copy of
    /// the folder holding `main` (with RE:DEX's `<title>.format` beside it) as the new character's. Takes RE:DEX's
    /// layout (`savedata/<title>/main`), Citra's and Azahar's (`title/00040000/<00055d00|00055e00>/data/00000001/main`)
    /// and Checkpoint's (`saves/0x0055D <name>/<backup>/main`). The source is only read; nothing is overwritten. The
    /// store is not saved here.
    pub fn import(&mut self, data: &Path, main: &Path, now: u64) -> Result<u64, String> {
        let folder = main.parent().unwrap_or(Path::new(""));
        let title = save_title(main)?;
        let bytes = std::fs::read(main).map_err(|e| format!("We couldn't read that file: {e}"))?;
        if bytes.len() != MAIN_LEN {
            log::warn!("characters: {} has {} bytes, a save has {MAIN_LEN}", main.display(), bytes.len());
            return Err("That file isn't a whole Pokémon X or Y save (its size is wrong).".into());
        }
        let look = xr_net::x::appearance(&bytes).ok_or("That save's trainer can't be read.")?;
        if !look.validate() {
            return Err("That save can't be imported: its outfit is out of the game's ranges.".into());
        }
        let name = xr_net::x::trainer_name(&bytes).and_then(|n| check_name(&n).ok()).unwrap_or_else(|| "Trainer".into());
        let id = self.create(&name, look, now)?;
        let root = data.join("characters").join(format!("{id:016x}"));
        // create_dir fails on a folder already there: an old character's saves are never written into
        let made = std::fs::create_dir_all(data.join("characters")).and_then(|()| std::fs::create_dir(&root));
        let savedata = user_dir(data, id).join("savedata");
        // RE:DEX keeps the archive's format info beside the folder; without it the copy reads the default info
        let redex = folder.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(&format!("{title:016X}")));
        let format = folder.with_file_name(format!("{title:016X}.format"));
        let format = Some(format).filter(|f| redex && f.is_file());
        let copied = made.and_then(|()| {
            copy_dir(folder, &savedata.join(format!("{title:016X}")))
                .and_then(|()| format.map_or(Ok(()), |f| std::fs::copy(f, savedata.join(format!("{title:016X}.format"))).map(drop)))
                .inspect_err(|_| {
                    let _ = std::fs::remove_dir_all(&root);
                })
        });
        copied.map(|()| id).map_err(|e| {
            self.characters.retain(|c| c.id != id);
            format!("We couldn't copy the save: {e}")
        })
    }
}

/// Keeps `file` as `<file>.bad-<ms>` (a name not yet taken, so an earlier one is never replaced): moved, or with
/// `moved == false` copied. Logged; whether it was kept.
fn aside(file: &Path, why: &str, moved: bool) -> bool {
    let mut ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let mut bad = file.with_extension(format!("json.bad-{ms}"));
    while bad.exists() {
        ms += 1;
        bad = file.with_extension(format!("json.bad-{ms}"));
    }
    let kept = if moved { std::fs::rename(file, &bad) } else { std::fs::copy(file, &bad).map(drop) };
    match &kept {
        Ok(()) => log::warn!("characters: {} {why}; kept as {}", file.display(), bad.display()),
        Err(e) => log::warn!("characters: {} {why}; could not keep it aside, leaving it alone: {e}", file.display()),
    }
    kept.is_ok()
}

/// The title a save file named `main` belongs to, from the folders above it: RE:DEX's `savedata/0004000000055D00`,
/// Citra's and Azahar's `00055d00`, Checkpoint's `0x0055D Pokémon X` (X; Y is 55E). Else why not, naming the layouts.
fn save_title(main: &Path) -> Result<u64, String> {
    const LAYOUTS: &str = "That isn't a save folder we know. Pick the file named main from RE:DEX's savedata\\0004000000055D00 \
        (Y: ...55E00), from Citra's or Azahar's title\\00040000\\00055d00\\data\\00000001 (Y: 00055e00), or from a \
        Checkpoint backup (saves\\0x0055D Pokémon X\\...).";
    if !main.file_name().is_some_and(|n| n == "main") {
        return Err(LAYOUTS.into());
    }
    // the save's own folder and up to three above it (Citra's data\00000001 sits under the title's folder)
    for dir in main.ancestors().skip(1).take(4) {
        let Some(n) = dir.file_name().and_then(|n| n.to_str()).map(str::to_ascii_lowercase) else { continue };
        for t in [TITLE_X, TITLE_Y] {
            let low = t & 0xFFFF_FFFF;
            if n == format!("{t:016x}") || n == format!("{low:08x}") || n.starts_with(&format!("0x{:05x} ", low >> 8)) {
                return Ok(t);
            }
        }
    }
    // RE:DEX's layout for another game
    if main.parent().and_then(Path::parent).and_then(Path::file_name).is_some_and(|n| n.eq_ignore_ascii_case("savedata")) {
        return Err("That file isn't a Pokémon X or Y save. Pick the file named main in the game's savedata folder.".into());
    }
    Err(LAYOUTS.into())
}

/// copies a folder's files and folders into a new folder
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()))?;
        } else {
            std::fs::copy(e.path(), to.join(e.file_name()))?;
        }
    }
    Ok(())
}

/// the name as the game can hold it: trimmed, 1 to 12 UTF-16 units, every character in the Basic Multilingual Plane, no
/// control or bidi characters
pub fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let bidi = |c: char| matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    if name.is_empty() {
        Err("Type a name.".into())
    } else if let Some(c) = name.chars().find(|&c| c as u32 > 0xFFFF) {
        Err(format!("The game can't show {c} in a name."))
    } else if name.chars().any(|c| c.is_control() || bidi(c)) {
        Err("A name can't have line breaks or hidden control characters.".into())
    } else if name.encode_utf16().count() > 12 {
        Err("A name is at most 12 characters.".into())
    } else {
        Ok(name.to_string())
    }
}

/// `<data>/characters/<id:016x>/user`, the character's saves folder
pub fn user_dir(data: &Path, id: u64) -> PathBuf {
    data.join("characters").join(format!("{id:016x}")).join("user")
}

/// the character has a `main` save for this title
pub fn has_save(data: &Path, id: u64, title: u64) -> bool {
    user_dir(data, id).join("savedata").join(format!("{title:016X}")).join("main").is_file()
}

/// copies each title's `main` under `user` to `<user>/../backups/<title>/main-<now_s>` and keeps the newest `keep` of
/// that title's backups; the number of saves backed up (0 when there are none)
pub fn backup_saves(user: &Path, now_s: u64, keep: usize) -> std::io::Result<usize> {
    let backups = user.parent().ok_or_else(|| std::io::Error::other("a saves folder with no parent"))?.join("backups");
    let titles = match std::fs::read_dir(user.join("savedata")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut n = 0;
    for t in titles {
        let t = t?;
        let main = t.path().join("main");
        if !main.is_file() {
            continue;
        }
        let dir = backups.join(t.file_name());
        std::fs::create_dir_all(&dir)?;
        std::fs::copy(&main, dir.join(format!("main-{now_s}")))?;
        n += 1;
        // the one just made is never pruned, even below a backup dated ahead by a wrong clock
        let mut kept: Vec<(u64, PathBuf)> = std::fs::read_dir(&dir)?
            .filter_map(|e| {
                let e = e.ok()?;
                let s = e.file_name().to_str()?.strip_prefix("main-")?.parse().ok().filter(|&s| s != now_s)?;
                Some((s, e.path()))
            })
            .collect();
        kept.sort();
        for (_, old) in &kept[..kept.len().saturating_sub(keep.saturating_sub(1))] {
            std::fs::remove_file(old)?;
        }
    }
    Ok(n)
}

/// ms since the Unix epoch
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use redex_proto::{TITLE_OR, TITLE_X};
    use xr_net::x::look::Look;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redex-chars-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn girl() -> Appearance {
        Look::default_for(1).appearance().unwrap()
    }

    /// a fashion no gender allows (every unused bit set)
    fn hacked() -> Appearance {
        Appearance { gender: 1, fashion: [0xFF; 16] }
    }

    /// an X `main` with this trainer's gender, fashion and name, in `<dir>/savedata/<title>/main`
    fn save_file(dir: &Path, title: u64, look: Appearance, name: &str, len: usize) -> PathBuf {
        let mut main = vec![0u8; len];
        main[0x14005] = look.gender;
        main[0x14030..0x14040].copy_from_slice(&look.fashion);
        for (i, u) in name.encode_utf16().enumerate() {
            main[0x14048 + 2 * i..][..2].copy_from_slice(&u.to_le_bytes());
        }
        // a byte past the name so the copy is checked whole
        main[len - 1] = 0x5A;
        let folder = dir.join(format!("savedata/{title:016X}"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("main"), &main).unwrap();
        folder.join("main")
    }

    #[test]
    fn create_checks_name_and_look() {
        let mut store = Store::default();
        let id = store.create("  Serena ", girl(), 5).unwrap();
        let c = store.get(id).unwrap();
        assert_eq!((c.name.as_str(), c.look.clone(), c.created, c.changed), ("Serena", girl().to_bytes().to_vec(), 5, 5));
        // two characters get two ids
        let other = store.create("Calem", Look::default_for(0).appearance().unwrap(), 6).unwrap();
        assert_ne!(id, other);
        assert!(store.create("", girl(), 7).is_err());
        assert!(store.create("Shauna", hacked(), 7).is_err());
        assert_eq!(store.characters.len(), 2);
        // a new look moves `changed`, never `created`; a bad one or an unknown id changes nothing
        let boy = Look::default_for(0).appearance().unwrap();
        assert!(store.set_look(id, boy, 9));
        let c = store.get(id).unwrap();
        assert_eq!((c.look.clone(), c.created, c.changed), (boy.to_bytes().to_vec(), 5, 9));
        assert!(!store.set_look(id, hacked(), 10));
        let unknown = (0..).find(|n| store.get(*n).is_none()).unwrap();
        assert!(!store.set_look(unknown, boy, 10));
        assert_eq!(store.get(id).unwrap().changed, 9);
    }

    /// The account's list (a hostile server can send anything): a look that fails `from_bytes`, a name the game cannot
    /// hold or id 0 is dropped; the rest merge by `changed` (the newest wins, a tie keeps ours).
    #[test]
    fn merge_remote_drops_bad_looks() {
        let mut store = Store::default();
        let id = store.create("Serena", girl(), 5).unwrap();
        let boy = Look::default_for(0).appearance().unwrap().to_bytes().to_vec();
        let calem = Character { id: id ^ 1, name: "Calem".into(), look: boy.clone(), created: 1, changed: 2 };
        let hacked = Character { id: id ^ 2, look: hacked().to_bytes().to_vec(), ..calem.clone() };
        let short = Character { id: id ^ 3, look: boy[..18].to_vec(), ..calem.clone() };
        let bidi = Character { id: id ^ 4, name: "A\u{202E}B".into(), ..calem.clone() };
        let zero = Character { id: 0, ..calem.clone() };
        let ours = store.get(id).cloned().unwrap();
        let tie = Character { look: boy.clone(), ..ours.clone() };
        assert!(store.merge_remote(vec![hacked, calem.clone(), short, bidi, zero, tie], 10));
        assert_eq!(store.characters, [ours.clone(), calem.clone()], "the bad ones dropped, the tie kept ours");
        let newer = Character { look: boy, changed: 6, ..ours };
        assert!(store.merge_remote(vec![newer.clone()], 10));
        assert_eq!(store.characters, [newer, calem.clone()]);
        assert!(!store.merge_remote(vec![calem], 10), "nothing new");
    }

    /// Final review I2: a hostile world can't win every merge with a `changed` far ahead (more than a day past now is
    /// dropped, as the server does), and a removed character stays removed across a restart, whatever the account sends.
    #[test]
    fn far_future_copies_are_dropped_and_removed_ones_stay_out() {
        let data = temp("remove");
        let now = 1_700_000_000_000;
        let mut store = Store::default();
        let id = store.create("Serena", girl(), now).unwrap();
        let other = store.create("Calem", Look::default_for(0).appearance().unwrap(), now).unwrap();
        let ours = store.get(id).cloned().unwrap();
        let boy = Look::default_for(0).appearance().unwrap().to_bytes().to_vec();
        let rename = |changed| Character { name: "Junk".into(), look: boy.clone(), changed, ..ours.clone() };
        let junk = |n: u64| Character { id: 0x5000 + n, name: format!("Junk{n}"), look: boy.clone(), created: 1, changed: now };
        assert!(!store.merge_remote(vec![rename(u64::MAX), rename(now + redex_proto::DAY_MS), rename(now + FUTURE_SLACK_MS + 1)], now), "more than 5 minutes ahead: dropped");
        assert_eq!(store.get(id), Some(&ours));
        // a clock a little fast is let in but clamped to now: a local edit made now still wins the tie
        assert!(!store.merge_remote(vec![rename(now + FUTURE_SLACK_MS)], now), "clamped to now: a tie, ours wins");
        assert_eq!(store.get(id), Some(&ours));
        assert!(store.set_look(id, Look::default_for(0).appearance().unwrap(), now + 1_000));
        let mine = store.get(id).cloned().unwrap();
        assert!(!store.merge_remote(vec![rename(now + 1_000 + FUTURE_SLACK_MS)], now + 1_000), "a hostile copy 5 minutes ahead does not beat an edit made just now");
        assert_eq!(store.get(id), Some(&mine));
        let mut full = Store::default();
        assert!(full.merge_remote((1..=20).map(junk).collect(), now));
        assert_eq!(full.characters.len(), 8, "at most 8 per account file");
        // junk from a hostile world, then removed: it never comes back, and the saves folder is not touched
        assert!(store.merge_remote((1..=6).map(junk).collect(), now));
        assert_eq!(store.characters.len(), 8);
        std::fs::create_dir_all(user_dir(&data, 0x5001)).unwrap();
        for n in 1..=6 {
            assert!(store.remove(0x5000 + n));
        }
        assert!(!store.remove(0x5001), "already gone");
        store.save(&data).unwrap();
        let mut back = Store::load(&data);
        assert_eq!(back.characters.iter().map(|c| c.id).collect::<Vec<_>>(), [id, other]);
        assert!(!back.merge_remote((1..=6).map(junk).collect(), now), "the account's copies of removed ones stay out");
        assert_eq!(back.characters.len(), 2);
        assert!(user_dir(&data, 0x5001).is_dir(), "its saves folder stays");
    }

    /// Final review I5/M4: the save goes through a temporary file of this process's own, synced, and none is left.
    #[test]
    fn a_save_leaves_no_temporary_file() {
        let data = temp("save-tmp");
        let mut store = Store::default();
        store.create("Serena", girl(), 1).unwrap();
        store.save(&data).unwrap();
        store.save(&data).unwrap();
        let names: Vec<String> = std::fs::read_dir(&data).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(names, ["characters.json"]);
        assert_eq!(Store::load(&data).characters, store.characters);
        // no "removed" field until something is removed (the file reads the same in older builds of this branch)
        assert!(!std::fs::read_to_string(data.join("characters.json")).unwrap().contains("removed"));
    }

    #[test]
    fn names_follow_the_games_rules() {
        assert_eq!(check_name("Ash").as_deref(), Ok("Ash"));
        assert_eq!(check_name("  Serena  ").as_deref(), Ok("Serena"));
        let twelve = "é".repeat(12);
        assert_eq!(check_name(&twelve), Ok(twelve.clone()));
        for bad in ["", "   ", "ABCDEFGHIJKLM", "A😀", "A\u{202E}B", "A\nB"] {
            let why = check_name(bad).expect_err(bad);
            assert!(!why.is_empty(), "{bad:?} needs a reason");
        }
    }

    #[test]
    fn the_store_survives_a_restart() {
        let data = temp("restart");
        let mut store = Store::load(&data);
        assert!(store.characters.is_empty());
        let a = store.create("Serena", girl(), 1).unwrap();
        let b = store.create("Calem", Look::default_for(0).appearance().unwrap(), 2).unwrap();
        store.save(&data).unwrap();
        let back = Store::load(&data);
        assert_eq!(back.characters, store.characters);
        assert_eq!((back.get(a).unwrap().name.as_str(), back.get(b).unwrap().name.as_str()), ("Serena", "Calem"));
        // the file's own form: a 16-hex id and a 38-hex look
        let text = std::fs::read_to_string(data.join("characters.json")).unwrap();
        assert!(text.contains(&format!("\"{a:016x}\"")), "{text}");
        assert!(text.contains(&format!("\"{}\"", girl().to_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>())), "{text}");
        // an entry whose look fails is dropped, the rest stay, and the file's bytes are kept aside
        let bad = text.replacen(&girl().to_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(), &"ff".repeat(19), 1);
        std::fs::write(data.join("characters.json"), &bad).unwrap();
        let back = Store::load(&data);
        assert_eq!(back.characters.iter().map(|c| c.id).collect::<Vec<_>>(), [b]);
        assert_eq!(bad_files(&data), [bad]);
        // kept aside once: characters.json now holds the rest, so the next load copies nothing more
        assert_eq!(Store::load(&data).characters, back.characters);
        assert_eq!(bad_files(&data).len(), 1);
    }

    /// what was kept aside as `characters.json.bad-<ms>`, oldest first
    fn bad_files(data: &Path) -> Vec<String> {
        let mut names: Vec<PathBuf> = std::fs::read_dir(data)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("characters.json.bad-"))
            .collect();
        names.sort_by_key(|p| p.to_str().unwrap().rsplit('-').next().unwrap().parse::<u128>().unwrap());
        names.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect()
    }

    /// characters.json held by another program (an antivirus, a backup): it can't be read or moved aside now, so it is
    /// no store to save over: `try_load` says None (`load` shows none), and the file is whole once it is free.
    #[cfg(windows)]
    #[test]
    fn a_store_that_cannot_be_read_or_kept_aside_is_left_alone() {
        use std::os::windows::fs::OpenOptionsExt;
        let data = temp("locked");
        let mut store = Store::default();
        store.create("Serena", girl(), 1).unwrap();
        store.save(&data).unwrap();
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(data.join("characters.json")).unwrap();
        assert!(Store::try_load(&data).is_none());
        assert!(Store::load(&data).characters.is_empty());
        assert!(bad_files(&data).is_empty());
        drop(lock);
        assert_eq!(Store::try_load(&data).unwrap().characters, store.characters);
    }

    #[test]
    fn a_broken_store_is_kept_aside() {
        let data = temp("broken");
        std::fs::write(data.join("characters.json"), "{not json").unwrap();
        let mut store = Store::load(&data);
        assert!(store.characters.is_empty());
        assert_eq!(bad_files(&data), ["{not json"]);
        assert!(!data.join("characters.json").exists());
        store.create("Serena", girl(), 1).unwrap();
        store.save(&data).unwrap();
        assert_eq!(Store::load(&data).characters, store.characters);
        // the broken bytes are still there
        assert_eq!(bad_files(&data), ["{not json"]);
        // a newer file (a field this version does not know) is kept aside too, never rewritten without it, and the
        // first broken one is not replaced
        let newer = r#"{"characters":[],"accounts":[]}"#;
        std::fs::write(data.join("characters.json"), newer).unwrap();
        assert!(Store::load(&data).characters.is_empty());
        assert_eq!(bad_files(&data), ["{not json", newer]);
    }

    #[test]
    fn import_copies_never_moves_and_makes_a_new_character() {
        let (data, src) = (temp("import-data"), temp("import-src"));
        let main = save_file(&src, TITLE_X, girl(), "Serena", 0x65600);
        let bytes = std::fs::read(&main).unwrap();
        let mtime = std::fs::metadata(&main).unwrap().modified().unwrap();
        let mut store = Store::default();
        let id = store.import(&data, &main, 3).unwrap();
        let c = store.get(id).unwrap();
        assert_eq!((c.name.as_str(), c.look.clone(), c.created), ("Serena", girl().to_bytes().to_vec(), 3));
        assert_eq!(std::fs::read(user_dir(&data, id).join("savedata/0004000000055D00/main")).unwrap(), bytes);
        assert!(has_save(&data, id, TITLE_X));
        assert!(!has_save(&data, id, redex_proto::TITLE_Y));
        assert_eq!(std::fs::read(&main).unwrap(), bytes);
        assert_eq!(std::fs::metadata(&main).unwrap().modified().unwrap(), mtime);
        let again = store.import(&data, &main, 4).unwrap();
        assert_ne!(again, id);
        assert_eq!(store.characters.len(), 2);
        assert_eq!(std::fs::read(user_dir(&data, again).join("savedata/0004000000055D00/main")).unwrap(), bytes);
        // a save with no name is "Trainer"
        let nameless = save_file(&temp("import-nameless"), TITLE_X, girl(), "", 0x65600);
        let id = store.import(&data, &nameless, 5).unwrap();
        assert_eq!(store.get(id).unwrap().name, "Trainer");
    }

    /// Final review M1: RE:DEX's `<title>.format` beside the save's folder comes along (a copy without it reads the
    /// default format info).
    #[test]
    fn import_copies_the_format_info() {
        let (data, src) = (temp("import-format-data"), temp("import-format-src"));
        let main = save_file(&src, TITLE_X, girl(), "Serena", 0x65600);
        let info = [0u8, 0, 4, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0];
        std::fs::write(src.join("savedata/0004000000055D00.format"), info).unwrap();
        let mut store = Store::default();
        let id = store.import(&data, &main, 3).unwrap();
        assert_eq!(std::fs::read(user_dir(&data, id).join("savedata/0004000000055D00.format")).unwrap(), info);
    }

    /// Final review M7: Citra's and Azahar's layout and a Checkpoint backup are taken too (the title from the folder
    /// names); a `main` anywhere else is refused with a message naming the layouts taken.
    #[test]
    fn import_takes_emulator_and_checkpoint_layouts() {
        let data = temp("import-layouts-data");
        let mut store = Store::default();
        let put = |at: PathBuf| {
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            let src = save_file(&temp("import-layouts-tmp"), TITLE_Y, girl(), "Serena", 0x65600);
            std::fs::copy(&src, &at).unwrap();
            at
        };
        let citra = put(temp("import-citra").join("sdmc/Nintendo 3DS/0/0/title/00040000/00055e00/data/00000001/main"));
        let id = store.import(&data, &citra, 1).unwrap();
        assert!(has_save(&data, id, redex_proto::TITLE_Y));
        let checkpoint = put(temp("import-checkpoint").join("3ds/Checkpoint/saves/0x0055D Pokémon X/20261123-101010/main"));
        let id = store.import(&data, &checkpoint, 2).unwrap();
        assert!(has_save(&data, id, TITLE_X));
        let loose = put(temp("import-loose").join("my saves/main"));
        let why = store.import(&data, &loose, 3).unwrap_err();
        assert!(why.contains("Citra") && why.contains("Checkpoint") && !why.contains("isn't a Pokémon"), "{why}");
        assert_eq!(store.characters.len(), 2);
    }

    #[test]
    fn import_refuses_an_unknown_title_or_a_bad_look() {
        let data = temp("refuse-data");
        let mut store = Store::default();
        let or = save_file(&temp("refuse-or"), TITLE_OR, girl(), "May", 0x65600);
        let why = store.import(&data, &or, 1).unwrap_err();
        assert!(why.contains("isn't a Pokémon X or Y save"), "{why}");
        let short = save_file(&temp("refuse-short"), TITLE_X, girl(), "Serena", 0x65000);
        let why = store.import(&data, &short, 1).unwrap_err();
        assert!(why.contains("size"), "{why}");
        let hacked = save_file(&temp("refuse-hacked"), TITLE_X, hacked(), "Serena", 0x65600);
        let why = store.import(&data, &hacked, 1).unwrap_err();
        assert!(why.contains("its outfit is out of the game's ranges"), "{why}");
        // nothing was made
        assert!(store.characters.is_empty());
        assert!(!data.join("characters").exists());
    }

    #[test]
    fn backups_keep_the_newest_ten() {
        let data = temp("backups");
        let user = user_dir(&data, 0xABCD);
        let main = save_file(&user, TITLE_X, girl(), "Serena", 0x65600);
        let bytes = std::fs::read(&main).unwrap();
        for s in 1..=12 {
            assert_eq!(backup_saves(&user, s, 10).unwrap(), 1);
        }
        let dir = user.join("../backups/0004000000055D00");
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        let mut want: Vec<String> = (3..=12).map(|s| format!("main-{s}")).collect();
        want.sort();
        assert_eq!(names, want);
        for n in names {
            assert_eq!(std::fs::read(dir.join(n)).unwrap(), bytes);
        }
        // no saves yet: nothing to back up, and no error
        assert_eq!(backup_saves(&user_dir(&data, 1), 1, 10).unwrap(), 0);
    }

    #[test]
    fn a_backup_from_a_clock_far_ahead_never_prunes_the_new_one() {
        let data = temp("backups-ahead");
        let user = user_dir(&data, 0xABCD);
        save_file(&user, TITLE_X, girl(), "Serena", 0x65600);
        let dir = user.join("../backups/0004000000055D00");
        std::fs::create_dir_all(&dir).unwrap();
        for s in [u64::MAX / 2, u64::MAX / 2 + 1] {
            std::fs::write(dir.join(format!("main-{s}")), b"ahead").unwrap();
        }
        assert_eq!(backup_saves(&user, 100, 2).unwrap(), 1);
        assert!(dir.join("main-100").is_file(), "the backup just made is kept");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    }
}
