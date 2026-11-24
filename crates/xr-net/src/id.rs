//! Who the player is. Online that is the SteamID64, proven to the world server with a Steam auth ticket (`steam`).
//! For two instances on one PC (one Steam client cannot run two accounts) the second one uses the dev id (`--dev-id`):
//! 128 bits from the OS CSPRNG, made once per install in `<data>/user/redex-id` (32 hex digits). Only a `--dev`
//! world server bound to localhost accepts a dev id, so the file is not a credential and gets no access control.
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub const FILE: &str = "redex-id";

/// Reads `<user>/redex-id`, or makes it the first time. A file that is there but not an id is an error: a new id would
/// silently be a new player.
pub fn load_or_create(user: &Path) -> io::Result<u128> {
    let path = user.join(FILE);
    let bad = || {
        let p = path.display();
        io::Error::new(io::ErrorKind::InvalidData, format!("{p} is not 32 hex digits; delete {p} to make a new one (a new dev identity)"))
    };
    // another instance starting on the same data dir may be between creating the file and writing it (about 1 ms):
    // a short all-hex prefix is that, and is read again (no hard links or renames: they fail on some volumes)
    for _ in 0..200 {
        match fs::read_to_string(&path) {
            Ok(text) => {
                if let Some(id) = parse(&text) {
                    return Ok(id);
                }
                let t = text.trim();
                if t.len() >= 32 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(bad());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut b = [0u8; 16];
                getrandom::fill(&mut b).map_err(|e| io::Error::other(format!("the OS random generator failed: {e}")))?;
                let id = u128::from_le_bytes(b);
                fs::create_dir_all(user)?;
                // create_new: of instances starting together exactly one makes the file; the others read its id
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(mut f) => {
                        f.write_all(format!("{id:032x}\n").as_bytes())?;
                        f.sync_all()?;
                        return Ok(id);
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(bad())
}

fn parse(text: &str) -> Option<u128> {
    let t = text.trim();
    (t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u128::from_str_radix(t, 16).ok()).flatten()
}

/// The identity the console id is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// `<data>/user/redex-id`, for `--dev` servers.
    Dev(u128),
    /// The SteamID64: one Steam account is one console (one PSS identity) on any PC.
    Steam(u64),
}

/// The console id cfg reports (and mixes into GenHashConsoleUnique, by which X's PSS tells passersby apart): the
/// spike-0 "ZK" prefix plus 48 bits of SHA-256("redex-console" || identity). One identity (one Steam account, later)
/// is one PSS identity on any PC and across sessions, and other consoles learn nothing of the identity itself.
pub fn console_id(identity: Identity) -> u64 {
    let mut h = Sha256::new().chain_update(b"redex-console");
    // 16 bytes for a dev id, 8 for a SteamID: the two never hash the same input
    match identity {
        Identity::Dev(id) => h.update(id.to_le_bytes()),
        Identity::Steam(id) => h.update(id.to_le_bytes()),
    }
    let h = h.finalize();
    0x5A4B_0000_0000_0000 | (u64::from_le_bytes(h[..8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("xr_net_id_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn made_once_then_kept() {
        let d = dir("kept");
        let a = load_or_create(&d.join("user")).unwrap();
        assert_eq!(load_or_create(&d.join("user")).unwrap(), a, "the same install keeps its id");
        let text = fs::read_to_string(d.join("user").join(FILE)).unwrap();
        assert_eq!(text, format!("{a:032x}\n"));
        // another install gets another id; 128 random bits: a repeat, or a mostly-zero id, means no real randomness
        let e = dir("other");
        let b = load_or_create(&e).unwrap();
        assert_ne!(a, b);
        for id in [a, b] {
            assert!((16..=112).contains(&id.count_ones()), "{id:032x} does not look random");
        }
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&e);
    }

    /// Many instances starting at once on one data dir: one id is made, and every one of them gets it.
    #[test]
    fn concurrent_first_starts_agree() {
        let d = dir("race");
        let go = std::sync::Arc::new(std::sync::Barrier::new(16));
        let ids: Vec<u128> = (0..16)
            .map(|_| {
                let (d, go) = (d.clone(), go.clone());
                std::thread::spawn(move || {
                    go.wait();
                    load_or_create(&d).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert!(ids.iter().all(|&i| i == ids[0]), "{ids:x?}");
        assert_eq!(parse(&fs::read_to_string(d.join(FILE)).unwrap()), Some(ids[0]));
        // caught between another instance's create and its write: the empty file is read again, not an error
        fs::write(d.join(FILE), "").unwrap();
        let (f, id) = (d.join(FILE), 0x1234u128);
        let w = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            fs::write(f, format!("{id:032x}\n")).unwrap();
        });
        assert_eq!(load_or_create(&d).unwrap(), id);
        w.join().unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_damaged_file_is_an_error_not_a_new_player() {
        let d = dir("bad");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(FILE), "not an id").unwrap();
        assert_eq!(load_or_create(&d).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(d.join(FILE)).unwrap(), "not an id", "left as it was");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn console_id_hides_the_id() {
        let id = 0xABCD_0123_4567_89AB_CDEFu128;
        let c = console_id(Identity::Dev(id));
        assert_eq!(c >> 48, 0x5A4B);
        assert_eq!(c, console_id(Identity::Dev(id)), "stable");
        assert_ne!(c & 0xFFFF_FFFF_FFFF, id as u64 & 0xFFFF_FFFF_FFFF, "not the id's own bits");
        assert_ne!(console_id(Identity::Dev(1)), console_id(Identity::Dev(2)));
        let s = console_id(Identity::Steam(76561197960287930));
        assert_eq!((s >> 48, s), (0x5A4B, console_id(Identity::Steam(76561197960287930))));
        assert_ne!(console_id(Identity::Steam(7)), console_id(Identity::Dev(7)), "a SteamID is not the dev id of the same value");
    }

    /// An empty or cut file (not one being written right now) says how to get going again.
    #[test]
    fn a_short_file_says_to_delete_it() {
        let d = dir("short");
        fs::create_dir_all(&d).unwrap();
        for text in ["", "abc123"] {
            fs::write(d.join(FILE), text).unwrap();
            let e = load_or_create(&d).unwrap_err().to_string();
            assert!(e.contains(&format!("delete {}", d.join(FILE).display())), "{e}");
        }
        let _ = fs::remove_dir_all(&d);
    }
}
