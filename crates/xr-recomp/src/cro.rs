//! CRO/CRS modules read straight from their file bytes, for the offline tools. Layout, field offsets and types
//! come from xr-hle's `cro::format` (the runtime loader works on guest memory instead); nothing is rebased here,
//! so every segment offset stays a file offset and every segment tag stays (segment index, offset in it).
use xr_hle::cro::format::{
    header, ExternalRelocation, InternalRelocation, RelocationType, Segment, SegmentTag, SegmentType,
    EXPORT_INDEXED_SYMBOL_ENTRY_SIZE, EXPORT_NAMED_SYMBOL_ENTRY_SIZE, IMPORT_ANONYMOUS_SYMBOL_ENTRY_SIZE,
    IMPORT_INDEXED_SYMBOL_ENTRY_SIZE, IMPORT_MODULE_ENTRY_SIZE, IMPORT_NAMED_SYMBOL_ENTRY_SIZE, RELOCATION_ENTRY_SIZE,
    SEGMENT_ENTRY_SIZE,
};

/// Header segment tags of the module's special functions (3dbrew "CRO0"); 0xFFFFFFFF = none. `format` names
/// only the last one. In X every module's control object (0xA0, the export `nnroControlObject_`) is a function in
/// code: a jump table over the loader's commands.
pub const SPECIAL_TAGS: [(u32, &str); 4] = [
    (0x0A0, "control"),
    (0x0A4, "on_load"),
    (0x0A8, "on_exit"),
    (header::ON_UNRESOLVED_SEGMENT_TAG, "on_unresolved"),
];

/// What an import names in the module it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSymbol {
    /// By export name, from whichever module provides it (`module` is None).
    Named(String),
    /// By the exporter's indexed-export number.
    Indexed(u32),
    /// By the exporter's segment tag directly.
    Anonymous(SegmentTag),
}

/// One imported symbol and the patch sites (its relocation batch) that receive its address.
#[derive(Debug, Clone)]
pub struct Import {
    pub module: Option<String>,
    pub symbol: ImportSymbol,
    /// Value written at each site = symbol address + `addend` (all type 2, absolute, in X).
    pub sites: Vec<ExternalRelocation>,
}

#[derive(Debug, Clone)]
pub struct Cro {
    /// The module name from the header (`|static|` for static.crs; DllEshop.cro calls itself `DllEShop`).
    pub name: String,
    /// static.crs: its segment table describes code.bin (absolute addresses), and it holds no code itself.
    pub is_static: bool,
    pub segments: Vec<Segment>,
    pub named_exports: Vec<(String, SegmentTag)>,
    pub indexed_exports: Vec<SegmentTag>,
    /// Raw tags of `SPECIAL_TAGS`, same order.
    pub special: [u32; 4],
    pub imports: Vec<Import>,
    pub internal: Vec<InternalRelocation>,
    pub external_count: u32,
}

fn u32_at(b: &[u8], o: u32) -> Result<u32, String> {
    let o = o as usize;
    b.get(o..o.checked_add(4).ok_or("offset overflow")?)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .ok_or_else(|| format!("truncated: word at 0x{o:X} past the end (0x{:X} bytes)", b.len()))
}

fn byte_at(b: &[u8], o: u32) -> Result<u8, String> {
    b.get(o as usize).copied().ok_or_else(|| format!("truncated: byte at 0x{o:X}"))
}

fn cstr(b: &[u8], o: u32) -> Result<String, String> {
    let s = b.get(o as usize..).ok_or_else(|| format!("string at 0x{o:X} past the end"))?;
    let n = s.iter().take(512).position(|&c| c == 0).ok_or_else(|| format!("unterminated string at 0x{o:X}"))?;
    Ok(String::from_utf8_lossy(&s[..n]).into_owned())
}

/// (table offset, count) of a header table, with the whole table inside the file.
fn table(b: &[u8], field: u32, entry: u32) -> Result<(u32, u32), String> {
    let (t, n) = (u32_at(b, field)?, u32_at(b, field + 4)?);
    let end = (t as u64) + (n as u64) * (entry as u64);
    if n != 0 && end > b.len() as u64 {
        return Err(format!("table at header 0x{field:X} (0x{t:X} x {n}) runs past the end"));
    }
    Ok((t, n))
}

fn external(b: &[u8], at: u32) -> Result<ExternalRelocation, String> {
    Ok(ExternalRelocation {
        target: SegmentTag(u32_at(b, at)?),
        kind: RelocationType::from_raw(byte_at(b, at + 4)?),
        is_batch_end: byte_at(b, at + 5)? != 0,
        addend: u32_at(b, at + 8)?,
    })
}

impl Cro {
    pub fn parse(b: &[u8], is_static: bool) -> Result<Cro, String> {
        if b.len() < header::SIZE as usize || &b[header::MAGIC as usize..header::MAGIC as usize + 4] != b"CRO0" {
            return Err("not a CRO0 module".into());
        }
        let name = if u32_at(b, header::MODULE_NAME_SIZE)? == 0 { String::new() } else { cstr(b, u32_at(b, header::MODULE_NAME_OFFSET)?)? };

        let (t, n) = table(b, header::SEGMENT_TABLE_OFFSET, SEGMENT_ENTRY_SIZE)?;
        let segments = (0..n)
            .map(|i| {
                let e = t + i * SEGMENT_ENTRY_SIZE;
                Ok(Segment { offset: u32_at(b, e)?, size: u32_at(b, e + 4)?, kind: SegmentType::from_raw(u32_at(b, e + 8)?) })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let (t, n) = table(b, header::EXPORT_NAMED_SYMBOL_TABLE_OFFSET, EXPORT_NAMED_SYMBOL_ENTRY_SIZE)?;
        let named_exports = (0..n)
            .map(|i| {
                let e = t + i * EXPORT_NAMED_SYMBOL_ENTRY_SIZE;
                Ok((cstr(b, u32_at(b, e)?)?, SegmentTag(u32_at(b, e + 4)?)))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let (t, n) = table(b, header::EXPORT_INDEXED_SYMBOL_TABLE_OFFSET, EXPORT_INDEXED_SYMBOL_ENTRY_SIZE)?;
        let indexed_exports = (0..n).map(|i| u32_at(b, t + i * 4).map(SegmentTag)).collect::<Result<Vec<_>, String>>()?;

        let mut special = [0; 4];
        for (s, (field, _)) in special.iter_mut().zip(SPECIAL_TAGS) {
            *s = u32_at(b, field)?;
        }

        let (ext_table, external_count) = table(b, header::EXTERNAL_RELOCATION_TABLE_OFFSET, RELOCATION_ENTRY_SIZE)?;
        let ext_end = ext_table + external_count * RELOCATION_ENTRY_SIZE;
        // A batch is a run of external relocations ending at the one marked as the end.
        let batch = |at: u32| -> Result<Vec<ExternalRelocation>, String> {
            let mut v = Vec::new();
            let mut a = at;
            while a >= ext_table && a < ext_end {
                let r = external(b, a)?;
                v.push(r);
                if r.is_batch_end {
                    return Ok(v);
                }
                a += RELOCATION_ENTRY_SIZE;
            }
            Err(format!("relocation batch at 0x{at:X} leaves the external relocation table"))
        };

        let mut imports = Vec::new();
        let (t, n) = table(b, header::IMPORT_NAMED_SYMBOL_TABLE_OFFSET, IMPORT_NAMED_SYMBOL_ENTRY_SIZE)?;
        for i in 0..n {
            let e = t + i * IMPORT_NAMED_SYMBOL_ENTRY_SIZE;
            let symbol = ImportSymbol::Named(cstr(b, u32_at(b, e)?)?);
            imports.push(Import { module: None, symbol, sites: batch(u32_at(b, e + 4)?)? });
        }
        let (t, n) = table(b, header::IMPORT_MODULE_TABLE_OFFSET, IMPORT_MODULE_ENTRY_SIZE)?;
        for i in 0..n {
            let e = t + i * IMPORT_MODULE_ENTRY_SIZE;
            let module = Some(cstr(b, u32_at(b, e)?)?);
            let (it, inum, at, anum) = (u32_at(b, e + 4)?, u32_at(b, e + 8)?, u32_at(b, e + 12)?, u32_at(b, e + 16)?);
            for j in 0..inum {
                let s = it + j * IMPORT_INDEXED_SYMBOL_ENTRY_SIZE;
                let symbol = ImportSymbol::Indexed(u32_at(b, s)?);
                imports.push(Import { module: module.clone(), symbol, sites: batch(u32_at(b, s + 4)?)? });
            }
            for j in 0..anum {
                let s = at + j * IMPORT_ANONYMOUS_SYMBOL_ENTRY_SIZE;
                let symbol = ImportSymbol::Anonymous(SegmentTag(u32_at(b, s)?));
                imports.push(Import { module: module.clone(), symbol, sites: batch(u32_at(b, s + 4)?)? });
            }
        }

        let (t, n) = table(b, header::INTERNAL_RELOCATION_TABLE_OFFSET, RELOCATION_ENTRY_SIZE)?;
        let internal = (0..n)
            .map(|i| {
                let e = t + i * RELOCATION_ENTRY_SIZE;
                Ok(InternalRelocation {
                    target: SegmentTag(u32_at(b, e)?),
                    kind: RelocationType::from_raw(byte_at(b, e + 4)?),
                    symbol_segment: byte_at(b, e + 5)? as u32,
                    addend: u32_at(b, e + 8)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let cro = Cro { name, is_static, segments, named_exports, indexed_exports, special, imports, internal, external_count };
        if let Some(c) = cro.code() {
            if !is_static && (c.offset as u64 + c.size as u64) > b.len() as u64 {
                return Err(format!("code segment 0x{:X}+0x{:X} runs past the end", c.offset, c.size));
            }
        }
        Ok(cro)
    }

    /// Segment 0, the code (text) segment: in every X module it is code, at file offset 0x180.
    pub fn code(&self) -> Option<Segment> {
        self.segments.first().copied().filter(|s| s.kind == SegmentType::Code)
    }

    /// The code segment's bytes (empty for static.crs, whose code is code.bin).
    pub fn code_bytes<'a>(&self, file: &'a [u8]) -> &'a [u8] {
        match self.code() {
            Some(c) if !self.is_static => &file[c.offset as usize..(c.offset + c.size) as usize],
            _ => &[],
        }
    }

    /// Offset into the code segment of a tag that points inside it.
    pub fn code_offset(&self, tag: SegmentTag) -> Option<u32> {
        let c = self.code()?;
        (tag.segment() == 0 && tag.offset() < c.size).then_some(tag.offset())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal module image: header, 3 segments (code 0x10 bytes at 0x180, rodata, data), one named export,
    /// one anonymous import from `|static|` with a 2-site batch, two internal relocations.
    pub(crate) fn tiny_cro(code: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 0x400];
        let put = |b: &mut Vec<u8>, o: u32, v: u32| b[o as usize..o as usize + 4].copy_from_slice(&v.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"CRO0");
        for (i, w) in code.iter().enumerate() {
            put(&mut b, 0x180 + 4 * i as u32, *w);
        }
        let csize = 4 * code.len() as u32;
        put(&mut b, header::CODE_OFFSET, 0x180);
        put(&mut b, header::CODE_SIZE, csize);
        for (f, _) in SPECIAL_TAGS {
            put(&mut b, f, 0xFFFF_FFFF);
        }
        put(&mut b, 0xA0, 4 << 4); // control object at code+4
        // strings
        b[0x300..0x305].copy_from_slice(b"Tiny\0");
        b[0x308..0x30C].copy_from_slice(b"fn0\0");
        b[0x310..0x319].copy_from_slice(b"|static|\0");
        put(&mut b, header::MODULE_NAME_OFFSET, 0x300);
        put(&mut b, header::MODULE_NAME_SIZE, 5);
        // segments at 0x200
        put(&mut b, header::SEGMENT_TABLE_OFFSET, 0x200);
        put(&mut b, header::SEGMENT_NUM, 3);
        for (i, (o, s, k)) in [(0x180, csize, 0), (0x2C0, 0x10, 1), (0x2D0, 0x8, 2)].into_iter().enumerate() {
            let e = 0x200 + 12 * i as u32;
            put(&mut b, e, o);
            put(&mut b, e + 4, s);
            put(&mut b, e + 8, k);
        }
        // named export "fn0" -> code+0
        put(&mut b, header::EXPORT_NAMED_SYMBOL_TABLE_OFFSET, 0x230);
        put(&mut b, header::EXPORT_NAMED_SYMBOL_NUM, 1);
        put(&mut b, 0x230, 0x308);
        put(&mut b, 0x234, 0);
        // import module |static|, 1 anonymous symbol (code.bin seg 0 + 0x40) patched at 2 sites
        put(&mut b, header::IMPORT_MODULE_TABLE_OFFSET, 0x240);
        put(&mut b, header::IMPORT_MODULE_NUM, 1);
        put(&mut b, 0x240, 0x310);
        put(&mut b, 0x24C, 0x258);
        put(&mut b, 0x250, 1);
        put(&mut b, 0x258, 0x40 << 4);
        put(&mut b, 0x25C, 0x260);
        put(&mut b, header::EXTERNAL_RELOCATION_TABLE_OFFSET, 0x260);
        put(&mut b, header::EXTERNAL_RELOCATION_NUM, 2);
        put(&mut b, 0x260, (csize - 4) << 4); // site: last code word (a literal)
        b[0x264] = 2;
        put(&mut b, 0x26C, 1); // site: rodata+0 (a vtable slot), addend 8
        b[0x270] = 2;
        b[0x271] = 1; // batch end
        put(&mut b, 0x274, 8);
        // internal relocs: rodata+0 = code+8 (a vtable slot), rodata+4 = data+0
        put(&mut b, header::INTERNAL_RELOCATION_TABLE_OFFSET, 0x280);
        put(&mut b, header::INTERNAL_RELOCATION_NUM, 2);
        put(&mut b, 0x280, 1);
        b[0x284] = 2;
        b[0x285] = 0;
        put(&mut b, 0x288, 8);
        put(&mut b, 0x28C, 4 << 4 | 1);
        b[0x290] = 2;
        b[0x291] = 2;
        b
    }

    #[test]
    fn parses_a_tiny_module() {
        let b = tiny_cro(&[0xE12FFF1E; 4]);
        let c = Cro::parse(&b, false).unwrap();
        assert_eq!(c.name, "Tiny");
        assert_eq!(c.segments.len(), 3);
        assert_eq!((c.code().unwrap().offset, c.code().unwrap().size), (0x180, 0x10));
        assert_eq!(c.named_exports, vec![("fn0".to_string(), SegmentTag(0))]);
        assert_eq!(c.special, [0x40, 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF]);
        assert_eq!(c.imports.len(), 1);
        let i = &c.imports[0];
        assert_eq!(i.module.as_deref(), Some("|static|"));
        assert_eq!(i.symbol, ImportSymbol::Anonymous(SegmentTag(0x400)));
        assert_eq!(i.sites.len(), 2, "batch runs to the end marker");
        assert_eq!((i.sites[1].target, i.sites[1].addend), (SegmentTag(1), 8));
        assert_eq!(c.internal.len(), 2);
        assert_eq!((c.internal[0].symbol_segment, c.internal[0].addend), (0, 8));
        assert_eq!(c.code_offset(SegmentTag(0xC0)), Some(0xC));
        assert_eq!(c.code_offset(SegmentTag(0x100)), None, "past the code segment");
        assert_eq!(c.code_offset(SegmentTag(0x1)), None, "another segment");
        assert_eq!(c.code_bytes(&b).len(), 0x10);
    }

    #[test]
    fn rejects_truncated_and_foreign_files() {
        let b = tiny_cro(&[0xE12FFF1E; 4]);
        assert!(Cro::parse(&b[..0x100], false).is_err());
        let mut bad = b.clone();
        bad[0x80] = b'X';
        assert!(Cro::parse(&bad, false).is_err());
        let mut runaway = b.clone();
        runaway[0x271] = 0; // batch never ends
        assert!(Cro::parse(&runaway, false).unwrap_err().contains("batch"));
    }
}
