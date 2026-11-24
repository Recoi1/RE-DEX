use crate::FormatError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Arm,
    Thumb,
}

/// `end` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuncEntry {
    pub start: u32,
    pub end: u32,
    pub mode: Mode,
}

pub fn parse_functab(text: &str) -> Result<Vec<FuncEntry>, FormatError> {
    let err = |n: usize, m: &str| FormatError::Other(format!("functab line {}: {m}", n + 1));
    let hex = |n: usize, s: Option<&str>| -> Result<u32, FormatError> {
        let s = s.ok_or_else(|| err(n, "missing field"))?;
        let h = s.strip_prefix("0x").ok_or_else(|| err(n, "expected 0x-prefixed hex"))?;
        u32::from_str_radix(h, 16).map_err(|_| err(n, "bad hex"))
    };
    let mut out: Vec<FuncEntry> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let mut f = line.split_whitespace();
        let (start, end) = (hex(n, f.next())?, hex(n, f.next())?);
        let mode = match f.next() {
            Some("arm") => Mode::Arm,
            Some("thumb") => Mode::Thumb,
            _ => return Err(err(n, "mode must be arm or thumb")),
        };
        if f.next().is_some() || end <= start {
            return Err(err(n, "malformed entry"));
        }
        let unit = if mode == Mode::Arm { 4 } else { 2 };
        if start % unit != 0 || end % unit != 0 {
            return Err(err(n, &format!("0x{start:08X}..0x{end:08X} misaligned for {mode:?}")));
        }
        out.push(FuncEntry { start, end, mode });
    }
    out.sort_by_key(|e| e.start);
    if let Some(w) = out.windows(2).find(|w| w[1].start < w[0].end) {
        return Err(FormatError::Other(format!(
            "functab: entry at 0x{:08X} overlaps entry at 0x{:08X}",
            w[1].start, w[0].start
        )));
    }
    Ok(out)
}

pub fn write_functab(entries: &[FuncEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            let m = if e.mode == Mode::Arm { "arm" } else { "thumb" };
            format!("0x{:08X} 0x{:08X} {m}
", e.start, e.end)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(start: u32, end: u32, mode: Mode) -> FuncEntry {
        FuncEntry { start, end, mode }
    }

    #[test]
    fn roundtrip() {
        let v = vec![e(0x100000, 0x10003C, Mode::Arm), e(0x10003C, 0x100050, Mode::Thumb)];
        assert_eq!(parse_functab(&write_functab(&v)).unwrap(), v);
    }

    #[test]
    fn rejects_misaligned() {
        let err = parse_functab("0x4C003C 0x4C0042 arm
").unwrap_err();
        assert!(err.to_string().contains("0x004C003C"), "{err}");
        assert!(parse_functab("0x4C0042 0x4C0045 thumb
").is_err());
        assert!(parse_functab("0x4C0042 0x4C0044 thumb
").is_ok());
    }

    #[test]
    fn rejects_overlap() {
        let err = parse_functab("0x10 0x20 arm\n0x18 0x30 arm\n").unwrap_err();
        assert!(err.to_string().contains("0x00000018"), "{err}");
    }

    #[test]
    fn rejects_bad_mode() {
        assert!(parse_functab("0x10 0x20 mips\n").is_err());
    }

    #[test]
    fn dump_table_has_entry() {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../functions/x-usa-v0.2.0/code.txt");
        let text = std::fs::read_to_string(p).unwrap_or_default();
        if text.trim().is_empty() {
            return;
        }
        let t = parse_functab(&text).unwrap();
        assert_eq!(t[0].start, 0x0010_0000);
        assert!(t.iter().all(|f| f.start >= 0x0010_0000 && f.end <= 0x0053_BEDC));
    }
}
