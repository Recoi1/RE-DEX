//! The committed CRO function tables (`functions/x-usa-v0.2.0/cro/`), and with XR_DUMP set, the RomFS they come
//! from: every module parses, the tables are up to date, every export into code is an entry.
use std::collections::BTreeSet;
use std::path::PathBuf;
use xr_formats::{parse_functab, Mode};
use xr_recomp::crotab::{build, load, table_code_size, table_text};

fn dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../functions/x-usa-v0.2.0/cro"))
}

#[test]
fn committed_tables_are_sane() {
    let mut seen = 0;
    for e in std::fs::read_dir(dir()).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "txt") || p.file_name().unwrap() == "code_bin_imports.txt" {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        let name = p.file_stem().unwrap().to_string_lossy().into_owned();
        // parse_functab rejects misaligned and overlapping entries
        let t = parse_functab(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let size = table_code_size(&text).unwrap_or_else(|| panic!("{name}: no code_size header"));
        assert!(text.contains("OFFSETS"), "{name}: header must say these are offsets");
        assert!(t.iter().all(|f| f.end <= size), "{name}: entry past the code segment (0x{size:X})");
        if name == "DllTitle" || name == "DllLangSelect" {
            assert!(t.len() > 10, "{name}: {} entries", t.len());
            assert_eq!(t[0].start, 0, "{name}: code starts with a function");
            assert!(t.iter().all(|f| f.mode == Mode::Arm));
        }
        seen += 1;
    }
    assert_eq!(seen, 133, "one table per CRO");
}

#[test]
fn real_romfs() {
    let Ok(dump) = std::env::var("XR_DUMP") else {
        println!("skipping real_romfs: XR_DUMP not set");
        return;
    };
    let files = load(&PathBuf::from(dump).join("romfs")).unwrap();
    assert_eq!(files.len(), 134, "133 CROs + static.crs");
    let (parsed, built) = build(&files, false).unwrap();
    assert_eq!(parsed.iter().filter(|m| m.1.is_static).count(), 1);
    assert_eq!(built.len(), 133);
    let mut low = Vec::new();
    for b in &built {
        let t = &b.table;
        let starts: BTreeSet<u32> = t.funcs.iter().map(|f| f.start).collect();
        for e in &b.exports_in_code {
            assert!(starts.contains(e), "{}: export at +0x{e:X} is not an entry", b.name);
        }
        for f in &t.funcs {
            let unit = if f.mode == Mode::Thumb { 2 } else { 4 };
            assert!(f.start % unit == 0 && f.end % unit == 0 && f.end <= b.code_size, "{}: {f:?}", b.name);
        }
        let committed = std::fs::read_to_string(dir().join(format!("{}.txt", b.name))).unwrap_or_default();
        assert_eq!(
            committed,
            table_text(&b.name, &b.file, b.code_offset, b.code_size, &t.funcs, &t.origin),
            "{}: committed table is stale, rerun xr-crotab",
            b.name
        );
        if (t.covered as u64) * 100 < (b.code_size as u64) * 80 {
            low.push(b.name.clone());
        }
    }
    assert!(low.is_empty(), "modules under 80% coverage: {low:?}");
}
