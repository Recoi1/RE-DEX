//! 3DS NCCH extended header (SCI + ACI), see 3dbrew "NCCH/Extended Header".
use crate::FormatError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub addr: u32,
    pub size: u32,
    pub pages: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exheader {
    pub name: String,
    pub compressed: bool,
    pub text: Segment,
    pub ro: Segment,
    pub data: Segment,
    pub bss_size: u32,
    pub stack_size: u32,
    pub main_priority: u8,
    pub services: Vec<String>,
}

const SERVICES_OFF: usize = 0x250; // ACI (0x200) + 0x50
const SERVICE_COUNT: usize = 34;
const MIN_LEN: usize = SERVICES_OFF + SERVICE_COUNT * 8;

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

// CodeSetInfo is {addr, physical region size in pages, size}.
fn segment(b: &[u8], o: usize) -> Segment {
    Segment { addr: u32_at(b, o), pages: u32_at(b, o + 4), size: u32_at(b, o + 8) }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

pub fn parse_exheader(bytes: &[u8]) -> Result<Exheader, FormatError> {
    if bytes.len() < MIN_LEN {
        return Err(FormatError::TooShort { need: MIN_LEN, got: bytes.len() });
    }
    let services = bytes[SERVICES_OFF..MIN_LEN]
        .chunks(8)
        .map(cstr)
        .filter(|s| !s.is_empty())
        .collect();
    Ok(Exheader {
        name: cstr(&bytes[0..8]),
        compressed: bytes[0x0D] & 1 != 0,
        text: segment(bytes, 0x10),
        stack_size: u32_at(bytes, 0x1C),
        ro: segment(bytes, 0x20),
        data: segment(bytes, 0x30),
        bss_size: u32_at(bytes, 0x3C),
        main_priority: bytes[0x20F],
        services,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exheader_too_short() {
        assert!(matches!(parse_exheader(&[0u8; 16]), Err(FormatError::TooShort { .. })));
    }

    #[test]
    fn exheader_synthetic() {
        let mut b = vec![0u8; 0x800];
        b[..4].copy_from_slice(b"test");
        b[0x0D] = 1;
        b[0x10..0x14].copy_from_slice(&0x100000u32.to_le_bytes());
        b[0x14..0x18].copy_from_slice(&2u32.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&0x1234u32.to_le_bytes());
        b[0x1C..0x20].copy_from_slice(&0x4000u32.to_le_bytes());
        b[0x3C..0x40].copy_from_slice(&7u32.to_le_bytes());
        b[0x20F] = 48;
        b[SERVICES_OFF..SERVICES_OFF + 6].copy_from_slice(b"fs:USE");
        let e = parse_exheader(&b).unwrap();
        assert_eq!(e.name, "test");
        assert!(e.compressed);
        assert_eq!(e.text, Segment { addr: 0x100000, size: 0x1234, pages: 2 });
        assert_eq!((e.stack_size, e.bss_size, e.main_priority), (0x4000, 7, 48));
        assert_eq!(e.services, vec!["fs:USE".to_string()]);
    }

    #[test]
    fn exheader_dump_values() {
        let Ok(dir) = std::env::var("XR_DUMP") else {
            eprintln!("SKIP exheader_dump_values: XR_DUMP not set");
            return;
        };
        let e = parse_exheader(&std::fs::read(format!("{dir}/exheader.bin")).unwrap()).unwrap();
        assert_eq!(e.name, "kujira-1");
        assert!(e.compressed);
        assert_eq!(e.text, Segment { addr: 0x00100000, size: 0x43BEDC, pages: 0x43C });
        assert_eq!(e.ro, Segment { addr: 0x0053C000, size: 0x69A44, pages: 0x6A });
        assert_eq!(e.data, Segment { addr: 0x005A6000, size: 0x440C0, pages: 0x45 });
        assert_eq!(e.bss_size, 0x76858);
        assert_eq!(e.stack_size, 0x40000);
        assert_eq!(e.main_priority, 48);
        for s in ["fs:USER", "gsp::Gpu", "APT:U", "dsp::DSP"] {
            assert!(e.services.iter().any(|x| x == s), "missing {s}: {:?}", e.services);
        }
    }
}
