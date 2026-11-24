//! The characters on each account: `<dir>/accounts/<key:032x>.bin` holds the encoded `Characters` payload (at most
//! `MAX_CHARACTERS`, so at most `MAX_FRAME_LIVE` bytes). The key is the verified login's (server.rs), never one a
//! client sends. A file that is too big, unreadable or broken is renamed aside (`.bad-<ms>`), never overwritten.
use redex_proto::appearance::Appearance;
use redex_proto::{Character, MAX_FRAME_LIVE, Msg};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn file(dir: &Path, key: u128) -> PathBuf {
    dir.join("accounts").join(format!("{key:032x}.bin"))
}

/// The account's characters; none for an account without a file. A file that is too big, unreadable or does not
/// decode is renamed aside; a character whose look fails `Appearance::from_bytes` (or with id 0) is dropped, the file's
/// bytes copied aside first and the rest saved over it (so it is copied once). Each is logged. None when the file is
/// there but could not be read and could not be kept aside (another program may hold it for a moment): it is no
/// account to save over, so the caller must not.
pub fn load(dir: &Path, key: u128) -> Option<Vec<Character>> {
    let f = file(dir, key);
    let read = std::fs::metadata(&f).and_then(|m| {
        if m.len() > MAX_FRAME_LIVE as u64 {
            return Err(io::Error::other(format!("{} bytes", m.len())));
        }
        std::fs::read(&f)
    });
    let bytes = match read {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(e) => return aside(&f, &e.to_string(), true).then(Vec::new),
    };
    let Ok(Msg::Characters { list }) = Msg::decode(&bytes) else { return aside(&f, "it does not decode", true).then(Vec::new) };
    let n = list.len();
    let list: Vec<Character> = list.into_iter().filter(|c| c.id != 0 && Appearance::from_bytes(&c.look).is_some()).collect();
    if list.len() < n {
        if !aside(&f, &format!("{} character(s) with a bad look dropped", n - list.len()), false) {
            return None;
        }
        if let Err(e) = save(dir, key, &list) {
            eprintln!("redex-world: account file {}: could not save it without them: {e}", f.display());
        }
    }
    Some(list)
}

/// Keeps `f` as `<f>.bad-<ms>` (a name not yet taken): moved, or with `moved == false` copied. Logged; whether it was
/// kept.
fn aside(f: &Path, why: &str, moved: bool) -> bool {
    let mut ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let mut bad = f.with_extension(format!("bin.bad-{ms}"));
    while bad.exists() {
        ms += 1;
        bad = f.with_extension(format!("bin.bad-{ms}"));
    }
    let kept = if moved { std::fs::rename(f, &bad) } else { std::fs::copy(f, &bad).map(drop) };
    match &kept {
        Ok(()) => eprintln!("redex-world: account file {}: {why}; kept as {}", f.display(), bad.display()),
        Err(e) => eprintln!("redex-world: account file {}: {why}; could not keep it aside, leaving it alone: {e}", f.display()),
    }
    kept.is_ok()
}

/// Whole or not at all: a temporary file, flushed to disk, then a rename over the old one.
pub fn save(dir: &Path, key: u128, list: &[Character]) -> io::Result<()> {
    let f = file(dir, key);
    std::fs::create_dir_all(dir.join("accounts"))?;
    let tmp = f.with_extension("bin.tmp");
    let frame = Msg::Characters { list: list.to_vec() }.encode();
    let mut t = std::fs::File::create(&tmp)?;
    t.write_all(&frame[4..])?;
    t.sync_all()?;
    drop(t);
    std::fs::rename(&tmp, &f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use redex_proto::{MAX_FRAME_LIVE, Msg};

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("redex-accounts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn ch(id: u64, look: Vec<u8>) -> Character {
        Character { id, name: "Serena".into(), look, created: 1, changed: 2 }
    }

    const GIRL: [u8; 19] = [6, 1, 1, 0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0];

    fn bad_files(dir: &Path) -> Vec<Vec<u8>> {
        std::fs::read_dir(dir.join("accounts"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_str().unwrap().contains(".bin.bad-"))
            .map(|p| std::fs::read(p).unwrap())
            .collect()
    }

    #[test]
    fn an_account_is_its_own_file_and_survives_a_reload() {
        let dir = temp("reload");
        assert_eq!(load(&dir, 7), Some(vec![]), "no file: no characters");
        let list = vec![ch(1, GIRL.to_vec()), ch(2, GIRL.to_vec())];
        save(&dir, 7, &list).unwrap();
        assert_eq!(load(&dir, 7), Some(list.clone()));
        assert_eq!(load(&dir, 8), Some(vec![]), "another account");
        // the encoded Characters payload, named by the key
        let file = dir.join("accounts").join(format!("{:032x}.bin", 7u128));
        assert_eq!(std::fs::read(&file).unwrap(), Msg::Characters { list: list.clone() }.encode()[4..]);
        // a save replaces the file whole, leaving no temporary file behind
        save(&dir, 7, &list[..1]).unwrap();
        assert_eq!(load(&dir, 7), Some(list[..1].to_vec()));
        assert_eq!(std::fs::read_dir(dir.join("accounts")).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_or_oversized_account_file_is_kept_aside() {
        let dir = temp("broken");
        std::fs::create_dir_all(dir.join("accounts")).unwrap();
        let file = dir.join("accounts").join(format!("{:032x}.bin", 7u128));
        std::fs::write(&file, b"\x14\x09garbage").unwrap();
        assert_eq!(load(&dir, 7), Some(vec![]));
        assert!(!file.exists());
        assert_eq!(bad_files(&dir), [b"\x14\x09garbage".to_vec()]);
        // a second broken file (even in the same millisecond) does not replace the first one kept aside
        let big = vec![0x14; MAX_FRAME_LIVE + 1];
        std::fs::write(&file, &big).unwrap();
        assert_eq!(load(&dir, 7), Some(vec![]));
        assert_eq!(bad_files(&dir).len(), 2);
        assert!(bad_files(&dir).contains(&big));
        // a stored look that fails the game's ranges is never sent: dropped, the rest kept, the bytes kept aside
        let mut hacked = GIRL.to_vec();
        hacked[3..].fill(0xFF);
        let bytes = Msg::Characters { list: vec![ch(1, hacked), ch(2, GIRL.to_vec())] }.encode()[4..].to_vec();
        std::fs::write(&file, &bytes).unwrap();
        assert_eq!(load(&dir, 7), Some(vec![ch(2, GIRL.to_vec())]));
        assert!(bad_files(&dir).contains(&bytes));
        // kept aside once: the file now holds the rest, so the next load copies nothing more
        assert_eq!(std::fs::read(&file).unwrap(), Msg::Characters { list: vec![ch(2, GIRL.to_vec())] }.encode()[4..]);
        assert_eq!(load(&dir, 7), Some(vec![ch(2, GIRL.to_vec())]));
        assert_eq!(bad_files(&dir).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that cannot be read now and cannot be moved aside either (another program holds it, as an antivirus or a
    /// backup can) is no account to save over: `load` says None, and the file is still whole afterwards.
    #[cfg(windows)]
    #[test]
    fn a_file_that_cannot_be_read_or_kept_aside_is_left_alone() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp("locked");
        let list = vec![ch(1, GIRL.to_vec())];
        save(&dir, 7, &list).unwrap();
        let file = dir.join("accounts").join(format!("{:032x}.bin", 7u128));
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&file).unwrap();
        assert_eq!(load(&dir, 7), None);
        assert!(bad_files(&dir).is_empty(), "nothing could be kept aside");
        drop(lock);
        assert_eq!(load(&dir, 7), Some(list));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
