//! Dump intake: identify a decrypted US Pokémon X or Y v0.2.0 cartridge dump and unpack it with ctrtool.
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod known;
pub use known::{KnownBuild, KNOWN};

/// Expected size of the decompressed `.code`.
pub const CODE_LEN: u64 = 0x4EB000;

#[derive(Debug)]
pub enum Refusal {
    Cia,
    NotNcsd,
    Encrypted,
    Unknown { product: String, title_id: u64 },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Refusal::Cia => f.write_str("this is a CIA file; a decrypted cartridge dump (.cci/.3ds) is needed"),
            Refusal::NotNcsd => f.write_str("not a 3DS cartridge image (.cci/.3ds)"),
            Refusal::Encrypted => f.write_str("this dump is encrypted; a decrypted dump is needed"),
            Refusal::Unknown { product, title_id } => write!(
                f,
                "not a known Pokémon X or Y dump: found {product} (title {title_id:016X}), expected {} v0.2.0",
                KNOWN.iter().map(|k| k.product_code).collect::<Vec<_>>().join(" or ")
            ),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Refused(Refusal),
    Other(String),
}

impl From<Refusal> for Error {
    fn from(r: Refusal) -> Self {
        Error::Refused(r)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Other(format!("I/O error: {e}"))
    }
}

fn read_at(f: &mut File, off: u64, buf: &mut [u8]) -> Result<(), Error> {
    f.seek(SeekFrom::Start(off))?;
    match f.read_exact(buf) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Refusal::NotNcsd.into()),
        r => Ok(r?),
    }
}

/// Checks structure first (CIA, NCSD, NoCrypto), then hashes; returns the SHA-256 hex too.
pub fn identify_hash(path: &Path) -> Result<(KnownBuild, String), Error> {
    let mut f = File::open(path)?;
    let mut w = [0u8; 4];
    read_at(&mut f, 0, &mut w)?;
    if u32::from_le_bytes(w) == 0x2020 {
        return Err(Refusal::Cia.into());
    }
    read_at(&mut f, 0x100, &mut w)?;
    if &w != b"NCSD" {
        return Err(Refusal::NotNcsd.into());
    }
    let mut part = [0u8; 4];
    read_at(&mut f, 0x120, &mut part)?; // partition 0 offset, in media units of 0x200
    let mut flags = [0u8; 1];
    read_at(&mut f, u32::from_le_bytes(part) as u64 * 0x200 + 0x18F, &mut flags)?;
    let base = u32::from_le_bytes(part) as u64 * 0x200;
    if flags[0] & 0x04 == 0 {
        // Some decrypted dumps (our Pokémon Y) keep the "encrypted" flag; their ExHeader title is then plain ASCII.
        let mut name = [0u8; 8];
        let plain = read_at(&mut f, base + 0x200, &mut name).is_ok()
            && name[0].is_ascii_alphanumeric()
            && name.iter().all(|&c| c == 0 || c.is_ascii_graphic());
        if !plain {
            return Err(Refusal::Encrypted.into());
        }
    }
    let mut prod = [0u8; 16];
    read_at(&mut f, base + 0x150, &mut prod)?;
    let mut tid = [0u8; 8];
    read_at(&mut f, base + 0x118, &mut tid)?;
    f.seek(SeekFrom::Start(0))?;
    let hex = sha256(&mut f)?;
    match KNOWN.iter().find(|k| k.sha256 == hex) {
        Some(k) => Ok((*k, hex)),
        None => Err(Refusal::Unknown {
            product: String::from_utf8_lossy(&prod).trim_end_matches('\0').to_string(),
            title_id: u64::from_le_bytes(tid),
        }
        .into()),
    }
}

/// The SHA-256 of what is left in `f`, as hex.
fn sha256(f: &mut impl Read) -> io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `<data>/dumps/verified.txt`: the dumps hashed so far, a line each: size, modified time (ns since 1970), SHA-256, path.
fn verified(data: &Path) -> PathBuf {
    data.join("dumps").join("verified.txt")
}

/// A dump's size and modified time, as `verified.txt` keeps them.
fn stamp(dump: &Path) -> io::Result<(u64, u128)> {
    let m = std::fs::metadata(dump)?;
    let t = m.modified()?.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    Ok((m.len(), t))
}

/// Keeps `sha256` as the dump's hash at its size and time now (the builder's "Add your game" hashes it anyway).
pub fn remember(data: &Path, dump: &Path, sha256: &str) -> io::Result<()> {
    let (len, t) = stamp(dump)?;
    let path = dump.to_string_lossy();
    let file = verified(data);
    let old = std::fs::read_to_string(&file).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().filter(|l| l.splitn(4, ' ').nth(3) != Some(&*path)).collect();
    let line = format!("{len} {t} {sha256} {path}");
    lines.push(&line);
    std::fs::create_dir_all(file.parent().expect("dumps/"))?;
    let tmp = file.with_extension("txt.new");
    std::fs::write(&tmp, lines.join("\n") + "\n")?;
    std::fs::rename(tmp, file)
}

/// Whether the dump is still a known build's (`KNOWN`), as hashed when it was added: its size and time say it is
/// the file that was hashed, else it is hashed again now (once: the answer is kept). An edited dump (a rebuilt RomFS)
/// is a modded game, which a world refuses.
pub fn unchanged(data: &Path, dump: &Path) -> io::Result<bool> {
    let (len, t) = stamp(dump)?;
    let path = dump.to_string_lossy();
    let kept = std::fs::read_to_string(verified(data)).unwrap_or_default();
    let known = |sha: &str| KNOWN.iter().any(|k| k.sha256 == sha);
    for l in kept.lines() {
        let f: Vec<&str> = l.splitn(4, ' ').collect();
        if let [l2, t2, sha, p] = f[..] {
            if p == path && l2 == len.to_string() && t2 == t.to_string() {
                return Ok(known(sha));
            }
        }
    }
    let sha = sha256(&mut File::open(dump)?)?;
    remember(data, dump, &sha)?;
    Ok(known(&sha))
}

pub fn identify(path: &Path) -> Result<KnownBuild, Refusal> {
    match identify_hash(path) {
        Ok((k, _)) => Ok(k),
        Err(Error::Refused(r)) => Err(r),
        // shortcut: I/O errors surface as NotNcsd here; the CLI uses identify_hash for the real message.
        Err(Error::Other(_)) => Err(Refusal::NotNcsd),
    }
}

/// Output root: XR_DATA if set, else %APPDATA%\RE-DEX.
pub fn data_dir(xr_data: Option<std::ffi::OsString>, appdata: Option<std::ffi::OsString>) -> Option<PathBuf> {
    xr_data.map(PathBuf::from).or_else(|| appdata.map(|a| PathBuf::from(a).join("RE-DEX")))
}

fn is_complete(dir: &Path) -> bool {
    dir.join("build.txt").is_file()
        && std::fs::metadata(dir.join("exefs/code.bin")).map(|m| m.len()).ok() == Some(CODE_LEN)
}

/// Full intake. Returns the output folder (`<data>/dumps/<16 hex>`).
pub fn intake(cci: &Path, data: &Path) -> Result<PathBuf, Error> {
    intake_with(cci, data, None)
}

/// Like `intake`, with ctrtool's folder given directly (None: `XR_TOOLS`, else `ctrtool` on PATH).
pub fn intake_with(cci: &Path, data: &Path, tools: Option<&Path>) -> Result<PathBuf, Error> {
    let (build, hex) = identify_hash(cci)?;
    unpack(cci, data, &build, &hex, tools, |mut cmd| {
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0040); // idle priority, no console window
        let o = cmd.output().map_err(|e| e.to_string())?;
        Ok((o.status.success(), format!("{}: {}", o.status, String::from_utf8_lossy(&o.stderr).trim())))
    })
}

/// Unpacks a dump `identify_hash` already identified (`build`, `hex`). `run` runs ctrtool's command and returns
/// (success, what it said), so a caller can run it in its own job.
pub fn unpack(
    cci: &Path,
    data: &Path,
    build: &KnownBuild,
    hex: &str,
    tools: Option<&Path>,
    run: impl FnOnce(Command) -> Result<(bool, String), String>,
) -> Result<PathBuf, Error> {
    let out = data.join("dumps").join(&hex[..16]);
    if is_complete(&out) {
        return Ok(out);
    }
    std::fs::create_dir_all(&out)?;
    let ctrtool = match tools.map(PathBuf::from).or_else(|| std::env::var_os("XR_TOOLS").map(PathBuf::from)) {
        Some(d) => d.join("ctrtool.exe"),
        None => PathBuf::from("ctrtool"),
    };
    let mut cmd = Command::new(&ctrtool);
    cmd.arg(format!("--exheader={}", out.join("exheader.bin").display()))
        .arg(format!("--exefsdir={}", out.join("exefs").display()))
        .arg(format!("--romfsdir={}", out.join("romfs").display()))
        .arg("--decompresscode")
        .arg(cci);
    let (ok, said) = run(cmd).map_err(|e| Error::Other(format!("cannot run ctrtool ({}): {e}; set XR_TOOLS to its folder", ctrtool.display())))?;
    if !ok {
        return Err(Error::Other(format!("ctrtool failed ({said})")));
    }
    let len = std::fs::metadata(out.join("exefs/code.bin")).map(|m| m.len()).unwrap_or(0);
    if len != CODE_LEN {
        return Err(Error::Other(format!("exefs/code.bin is {len:#x} bytes, expected {CODE_LEN:#x}")));
    }
    let ex = std::fs::read(out.join("exheader.bin"))?;
    xr_formats::parse_exheader(&ex).map_err(|e| Error::Other(format!("exheader.bin unreadable: {e:?}")))?;
    std::fs::write(out.join("build.txt"), build.id)?; // written last: marks the unpack complete
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A dump's hash is kept with its size and time (`remember`, as "Add your game" hashes it): unchanged, it is taken
    /// as it was without hashing again; edited (size or time changed), it is hashed again once, and a hash that is no
    /// known build's is an edited dump, kept so too.
    #[test]
    fn an_edited_dump_is_seen_by_its_size_or_time() {
        let data = std::env::temp_dir().join(format!("xr-intake-verified-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        let dump = data.join("x game.cci");
        std::fs::write(&dump, b"a dump").unwrap();
        // remembered with X's hash: taken as X without reading it (its bytes are not X's)
        remember(&data, &dump, KNOWN[0].sha256).unwrap();
        assert!(unchanged(&data, &dump).unwrap());
        // never added: hashed now, not a known build
        let other = data.join("other.cci");
        std::fs::write(&other, b"another").unwrap();
        assert!(!unchanged(&data, &other).unwrap());
        assert!(unchanged(&data, &dump).unwrap(), "the other entry stays");
        // the dump edited in place: hashed again, not X any more, and remembered so
        std::fs::write(&dump, b"an edited dump").unwrap();
        assert!(!unchanged(&data, &dump).unwrap());
        let kept = std::fs::read_to_string(data.join("dumps/verified.txt")).unwrap();
        assert_eq!(kept.lines().count(), 2, "{kept}");
        assert!(!kept.contains(KNOWN[0].sha256), "{kept}");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn data_dir_prefers_xr_data_then_appdata() {
        assert_eq!(data_dir(Some("a".into()), Some("b".into())), Some(PathBuf::from("a")));
        assert_eq!(data_dir(None, Some("b".into())), Some(PathBuf::from("b").join("RE-DEX")));
        assert_eq!(data_dir(None, None), None);
    }
}
