use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(file: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xr-intake"))
        .arg(file)
        .env("XR_DATA", std::env::temp_dir().join("xr-intake-test-data"))
        .output()
        .unwrap()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xr-intake-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Minimal NCSD: magic at 0x100, partition 0 at media unit 1, NCCH flags byte 7 at 0x200+0x18F.
fn ncsd(flags7: u8) -> Vec<u8> {
    let mut b = vec![0u8; 0x400];
    b[0x100..0x104].copy_from_slice(b"NCSD");
    b[0x120..0x124].copy_from_slice(&1u32.to_le_bytes());
    b[0x124..0x128].copy_from_slice(&1u32.to_le_bytes());
    b[0x200 + 0x18F] = flags7;
    b[0x200 + 0x150..0x200 + 0x15A].copy_from_slice(b"CTR-P-TEST");
    b
}

fn cia() -> Vec<u8> {
    let mut b = vec![0u8; 0x400];
    b[..4].copy_from_slice(&0x2020u32.to_le_bytes());
    b
}

#[test]
fn refuses_cia() {
    let f = tmp("cia").join("x.cia");
    std::fs::write(&f, cia()).unwrap();
    let o = run(&f);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("CIA"), "{}", stderr(&o));
}

#[test]
fn refuses_unknown_cci() {
    let f = tmp("unknown").join("x.cci");
    std::fs::write(&f, ncsd(0x04)).unwrap();
    let o = run(&f);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("not a known Pokémon X or Y dump"), "{}", stderr(&o));
    assert!(stderr(&o).contains("CTR-P-TEST"), "{}", stderr(&o));
}

#[test]
fn refuses_encrypted() {
    let f = tmp("enc").join("x.cci");
    std::fs::write(&f, ncsd(0x00)).unwrap();
    let o = run(&f);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("encrypted"), "{}", stderr(&o));
}

/// A decrypted dump whose header still says encrypted (our Pokémon Y): the plain ExHeader title wins.
#[test]
fn decrypted_despite_flag() {
    let f = tmp("flag").join("y.3ds");
    let mut b = ncsd(0x00);
    b.resize(0x600, 0);
    b[0x400..0x408].copy_from_slice(b"kujira-2");
    std::fs::write(&f, b).unwrap();
    let o = run(&f);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("not a known"), "{}", stderr(&o));
}

#[test]
fn path_with_spaces_and_accents() {
    let f = tmp("acc").join("Pokémon X (USA)");
    std::fs::create_dir_all(&f).unwrap();
    let f = f.join("small file.cia");
    std::fs::write(&f, cia()).unwrap();
    let o = run(&f);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("CIA"), "{}", stderr(&o));
}

#[test]
fn accepts_real_dump() {
    let (Some(_), Some(cci)) = (std::env::var_os("XR_DUMP"), std::env::var_os("XR_CCI")) else {
        eprintln!("skipping accepts_real_dump: XR_DUMP/XR_CCI not set");
        return;
    };
    let data = PathBuf::from(std::env::var_os("XR_DATA").expect("XR_DATA"));
    let o = Command::new(env!("CARGO_BIN_EXE_xr-intake")).arg(&cci).output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let dumps = std::fs::read_dir(data.join("dumps")).unwrap();
    let ok = dumps.filter_map(Result::ok).any(|d| {
        std::fs::metadata(d.path().join("exefs/code.bin")).map(|m| m.len()).ok() == Some(0x4EB000)
            && std::fs::read_to_string(d.path().join("build.txt")).map(|s| s.trim() == "x-usa-v0.2.0").ok() == Some(true)
    });
    assert!(ok, "no complete unpack under {}", data.display());
}
