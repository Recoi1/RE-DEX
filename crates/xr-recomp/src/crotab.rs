//! Function tables for CRO code segments (`xr-crotab`). Every start/end here is an OFFSET into the module's code
//! segment (segment 0), never an address: the game picks where a CRO lives at run time.
//!
//! Entries come from seeds (exports, the header's special functions, relocated code pointers, other modules'
//! anonymous imports of this module's code), then a fixed point: each function is the range up to the next entry,
//! walked from its entry through direct edges (and `ldr pc, [pc, rm, lsl #2]` jump tables, whose relocated words
//! give the cases); BL/BLX targets, B targets in another function's range, and the first plausible ARM code after
//! a function's last reachable instruction and literal pool (a function only reached indirectly, separated by
//! the pool) become entries. Finally the repairs (`repair::repair`) run on the table, as for code.bin.
//!
//! Mode: an entry is Thumb only when the evidence says so: a relocated pointer or import with bit 0 set, or a
//! BLX imm from ARM code (BL/B keep the caller's mode). Everything else is ARM (interworking: a pointer without
//! bit 0 entered through BX/BLX/LDR PC runs in ARM state). X's CROs have no odd code pointer and no BLX imm, so
//! every entry is ARM; a seed whose first word does not decode in its mode is reported as implausible.
use std::collections::{BTreeMap, HashMap, HashSet};
use xr_arm::{decode_arm, decode_thumb, Insn, MemWidth, Offset, Reg, VfpInsn};
use xr_codegen::{gen_function, GenOptions};
use xr_formats::{FuncEntry, Mode};
use xr_hle::cro::format::RelocationType;

use crate::cro::{Cro, ImportSymbol};
use crate::repair::{falls, is_mov_lr_pc, repair, sweep};

/// Where an entry came from (the first reason found).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Seed {
    /// A named or indexed export of the module.
    Export,
    /// The header's control object / on-load / on-exit / on-unresolved tag.
    Special,
    /// An internal relocation whose symbol is the code segment (vtables, function pointer tables, literals).
    Reloc,
    /// Another module's anonymous import of this module's code (static.crs: code.bin calling the CRO).
    Import,
    /// A BL/BLX target.
    Call,
    /// A B target inside another function's range (that function is split there).
    Split,
    /// The first plausible ARM code after a function's last reachable instruction and literal pool.
    Gap,
}

/// One module's inputs to discovery.
pub struct Module<'a> {
    pub name: String,
    pub code: &'a [u8],
    pub seeds: BTreeMap<u32, (Mode, Seed)>,
    /// Code words that hold data the loader patches: internal relocation targets and import patch sites.
    pub data_words: HashSet<u32>,
    /// `ldr pc, [pc, rm, lsl #2]` at the key: the case offsets (its relocated table starts 8 bytes later).
    pub jump_tables: HashMap<u32, Vec<u32>>,
    /// Seeds that came with a mode the other seed source disagrees with (kept the first).
    pub conflicts: usize,
}

fn word(code: &[u8], a: u32) -> Option<u32> {
    let b = code.get(a as usize..(a as usize).checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn decode(code: &[u8], a: u32, mode: Mode) -> Option<(Insn, u32)> {
    let half = |a: u32| code.get(a as usize..a as usize + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    match mode {
        Mode::Arm => word(code, a).map(|w| (decode_arm(w, a), 4)),
        Mode::Thumb => {
            let h = half(a)?;
            let (i, n) = decode_thumb(h, half(a.wrapping_add(2)).unwrap_or(0), a);
            Some((i, n as u32))
        }
    }
}

fn unit(m: Mode) -> u32 {
    if m == Mode::Thumb { 2 } else { 4 }
}

fn other(m: Mode) -> Mode {
    if m == Mode::Thumb { Mode::Arm } else { Mode::Thumb }
}

/// A code pointer's offset and mode: bit 0 selects Thumb.
fn pointer(v: u32) -> (u32, Mode) {
    (v & !1, if v & 1 != 0 { Mode::Thumb } else { Mode::Arm })
}

/// Records a seed inside the code segment and aligned for its mode; the first mode seen wins.
fn add_seed(m: &mut Module, (off, mode): (u32, Mode), why: Seed) {
    if off >= m.code.len() as u32 || off % unit(mode) != 0 {
        return;
    }
    match m.seeds.get(&off) {
        Some(&(had, _)) if had != mode => m.conflicts += 1,
        Some(_) => {}
        None => {
            m.seeds.insert(off, (mode, why));
        }
    }
}

/// Builds every module's discovery input. `mods` holds every CRO and static.crs (file name, parsed module, code
/// bytes), because seeds also come from other modules' imports. static.crs gets no `Module` (its code is code.bin).
pub fn prepare<'a>(mods: &[(String, Cro, &'a [u8])]) -> Vec<Module<'a>> {
    let mut out: Vec<Module<'a>> = Vec::new();
    for (_, c, code) in mods.iter().filter(|m| !m.1.is_static) {
        let mut m = Module {
            name: c.name.clone(),
            code,
            seeds: BTreeMap::new(),
            data_words: HashSet::new(),
            jump_tables: HashMap::new(),
            conflicts: 0,
        };
        let size = code.len() as u32;
        // Code pointers: (where the relocated word is when it is in code, the code offset it holds).
        let mut ptr_words: BTreeMap<u32, u32> = BTreeMap::new();
        let mut far_ptrs = Vec::new();
        for r in &c.internal {
            let site = c.code_offset(r.target);
            if let Some(o) = site {
                m.data_words.insert(o & !3);
            }
            if r.symbol_segment == 0 && r.kind == RelocationType::AbsoluteAddress {
                match site {
                    Some(o) => {
                        ptr_words.insert(o, r.addend);
                    }
                    None => far_ptrs.push(r.addend),
                }
            }
        }
        for s in c.imports.iter().flat_map(|i| &i.sites) {
            if let Some(o) = c.code_offset(s.target) {
                m.data_words.insert(o & !3);
            }
        }
        // Jump tables: their words are case labels inside the function, not entries.
        let mut case_words = HashSet::new();
        for a in (0..size).step_by(4) {
            let jt = matches!(decode(code, a, Mode::Arm),
                Some((Insn::Load { width: MemWidth::Word, rt: Reg::PC, rn: Reg::PC, addr, .. }, _))
                    if addr.pre && matches!(addr.offset, Offset::Reg { .. }));
            if !jt {
                continue;
            }
            let cases: Vec<u32> = (0..).map(|i| a + 8 + 4 * i).map_while(|w| ptr_words.get(&w).copied()).collect();
            if !cases.is_empty() {
                case_words.extend((0..cases.len() as u32).map(|i| a + 8 + 4 * i));
                m.jump_tables.insert(a, cases.iter().map(|&v| v & !1).collect());
            }
        }
        let exports = c.named_exports.iter().map(|e| e.1).chain(c.indexed_exports.iter().copied());
        for o in exports.filter_map(|t| c.code_offset(t)) {
            add_seed(&mut m, pointer(o), Seed::Export);
        }
        for t in c.special.iter().filter(|&&t| t != 0xFFFF_FFFF) {
            if let Some(o) = c.code_offset(xr_hle::cro::format::SegmentTag(*t)) {
                add_seed(&mut m, pointer(o), Seed::Special);
            }
        }
        let ptrs = ptr_words.iter().filter(|(w, _)| !case_words.contains(*w)).map(|(_, &v)| v);
        for v in ptrs.chain(far_ptrs).collect::<Vec<_>>() {
            add_seed(&mut m, pointer(v), Seed::Reloc);
        }
        out.push(m);
    }
    // Anonymous imports of a module's code from every other module, static.crs included. Named and indexed
    // imports resolve to exports, which are seeds already (X has neither kind).
    for (_, c, _) in mods {
        for imp in &c.imports {
            let (Some(from), ImportSymbol::Anonymous(tag)) = (&imp.module, &imp.symbol) else { continue };
            let Some(i) = out.iter().position(|m| &m.name == from) else { continue };
            let Some(e) = mods.iter().find(|x| &x.1.name == from).map(|x| &x.1) else { continue };
            let Some(o) = e.code_offset(*tag) else { continue };
            for s in &imp.sites {
                add_seed(&mut out[i], pointer(o.wrapping_add(s.addend)), Seed::Import);
            }
        }
    }
    out
}

/// What a walk of one function's range found.
#[derive(Default, Clone)]
pub struct Walk {
    /// Reachable instructions (offset, size).
    pub insns: Vec<(u32, u32)>,
    /// Literal/table bytes read PC-relatively (offset, length).
    pub lits: Vec<(u32, u32)>,
    /// Direct branch targets outside the range, and BL/BLX targets: (target, mode, is a call).
    pub exits: Vec<(u32, Mode, bool)>,
    /// One past the last reachable byte (instruction or literal) inside the range.
    pub hi: u32,
}

fn walk_once(m: &Module, start: u32, end: u32, mode: Mode, stop: &HashSet<u32>) -> Walk {
    let mut w = Walk { hi: start, ..Default::default() };
    let mut seen = HashSet::new();
    let mut stack = vec![start];
    while let Some(a) = stack.pop() {
        if a < start || a >= end || !seen.insert(a) || stop.contains(&(a & !3)) {
            continue;
        }
        let Some((insn, n)) = decode(m.code, a, mode) else { continue };
        if matches!(insn, Insn::Undefined { .. }) {
            continue;
        }
        w.insns.push((a, n));
        let next = a + n;
        let local = |t: u32, stack: &mut Vec<u32>, w: &mut Walk| {
            if (start..end).contains(&t) { stack.push(t) } else { w.exits.push((t, mode, false)) }
        };
        match insn {
            Insn::Branch { link: false, target, .. } => local(target, &mut stack, &mut w),
            Insn::Branch { link: true, target, .. } => w.exits.push((target, mode, true)),
            Insn::BlxImm { target } => w.exits.push((target, other(mode), true)),
            Insn::Load { rt: Reg::PC, rn: Reg::PC, .. } if m.jump_tables.contains_key(&a) => {
                let cases = &m.jump_tables[&a];
                w.lits.push((a + 8, 4 * cases.len() as u32));
                for &t in cases {
                    local(t, &mut stack, &mut w);
                }
            }
            Insn::Load { rn: Reg::PC, width, addr: xr_arm::Addr { offset: Offset::Imm(i), up, .. }, .. } => {
                let at = if up { (a + 8).wrapping_add(i) } else { (a + 8).wrapping_sub(i) };
                let len = match width {
                    MemWidth::Double => 8,
                    MemWidth::Word => 4,
                    MemWidth::Half | MemWidth::SignedHalf => 2,
                    _ => 1,
                };
                w.lits.push((at, len));
            }
            Insn::Vfp(VfpInsn::Mem { rn: Reg::PC, offset, up, dp, load: true, .. }) => {
                let b = (a + 8) & !3;
                let at = if up { b.wrapping_add(offset as u32) } else { b.wrapping_sub(offset as u32) };
                w.lits.push((at, if dp { 8 } else { 4 }));
            }
            _ => {}
        }
        let call_idiom = a >= start + 4 && mode == Mode::Arm
            && decode(m.code, a - 4, mode).is_some_and(|(p, _)| is_mov_lr_pc(&p));
        if falls(&insn) || call_idiom {
            stack.push(next);
        }
    }
    w.insns.sort_unstable();
    for &(a, n) in w.insns.iter().chain(&w.lits) {
        if a >= start && a < end {
            w.hi = w.hi.max((a + n).min(end));
        }
    }
    w
}

/// Walks until the literal words it found stop changing (a literal pool after a noreturn call must not be
/// decoded as code).
pub fn walk(m: &Module, start: u32, end: u32, mode: Mode) -> Walk { walk_from(m, start, end, mode, m.data_words.clone()) }

/// `walk` that starts out stopping only at `stop` (then also at the literals it finds).
pub fn walk_from(m: &Module, start: u32, end: u32, mode: Mode, mut stop: HashSet<u32>) -> Walk {
    let mut w = walk_once(m, start, end, mode, &stop);
    for _ in 0..4 {
        let before = stop.len();
        for &(a, n) in &w.lits {
            stop.extend((a & !3..a.saturating_add(n)).step_by(4));
        }
        stop.retain(|&s| s != start);
        if stop.len() == before {
            break;
        }
        w = walk_once(m, start, end, mode, &stop);
    }
    w
}

/// The first offset in [from, to) where plausible ARM code starts: an unconditional instruction, not data, whose
/// linear run reaches a branch/return (or the end of the gap) before anything undecodable.
// shortcut: ARM only (X's CROs have no Thumb code); a Thumb module would need a Thumb prologue check here.
fn gap_entry(m: &Module, from: u32, to: u32, lits: &[(u32, u32)]) -> Option<u32> {
    let pool: HashSet<u32> = lits.iter().flat_map(|&(a, n)| (a & !3..a.saturating_add(n)).step_by(4)).collect();
    let data = |a: u32| m.data_words.contains(&a) || pool.contains(&a);
    let mut c = (from + 3) & !3;
    while c < to {
        let w = word(m.code, c)?;
        if w != 0 && w >> 28 == 0xE && !data(c) {
            let mut a = c;
            let ok = loop {
                if a >= to {
                    break true;
                }
                if data(a) {
                    break false;
                }
                match decode(m.code, a, Mode::Arm) {
                    None | Some((Insn::Undefined { .. }, _)) => break false,
                    Some((i, _)) if !falls(&i) => {
                        let idiom = a > c && decode(m.code, a - 4, Mode::Arm).is_some_and(|(p, _)| is_mov_lr_pc(&p));
                        if !idiom {
                            break true;
                        }
                    }
                    _ => {}
                }
                a += 4;
            };
            if ok {
                return Some(c);
            }
        }
        c += 4;
    }
    None
}

fn ranges(entries: &BTreeMap<u32, (Mode, Seed)>, size: u32) -> Vec<FuncEntry> {
    let starts: Vec<(u32, Mode)> = entries.iter().map(|(&s, &(m, _))| (s, m)).collect();
    starts
        .iter()
        .enumerate()
        .map(|(i, &(start, mode))| FuncEntry { start, end: starts.get(i + 1).map_or(size, |n| n.0), mode })
        .collect()
}

/// A module's finished table.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub funcs: Vec<FuncEntry>,
    /// Why each entry of `funcs` is one (`repair` additions count as Call).
    pub origin: Vec<Seed>,
    /// Entries by origin.
    pub origins: BTreeMap<Seed, usize>,
    /// Seeds whose first word is undecodable or data in their mode (left out of the table).
    pub implausible: Vec<u32>,
    pub jump_tables: usize,
    pub conflicts: usize,
    /// Branch targets outside the code segment or misaligned, ignored.
    pub stray_targets: usize,
    /// Repairs: (dropped, ended at a noreturn call, added).
    pub repaired: (usize, usize, usize),
    /// Functions whose last reachable instruction runs on into the next entry (a split inside straight-line code;
    /// codegen makes that a tail call).
    pub falls_into_next: usize,
    /// Bytes of the code segment that reachable instructions, literals and jump tables of listed functions cover.
    pub covered: u32,
    /// Functions that codegen turns into a whole-function interpreter stub (a reachable undecodable word).
    pub undecodable: Vec<u32>,
}

/// Discovers a module's functions (fixed point), applies the repairs and measures coverage.
pub fn discover(m: &Module) -> Table {
    let size = m.code.len() as u32;
    let mut entries = m.seeds.clone();
    let mut t = Table { jump_tables: m.jump_tables.len(), conflicts: m.conflicts, ..Default::default() };
    for (&s, &(mode, _)) in &m.seeds {
        let bad = m.data_words.contains(&(s & !3)) || matches!(decode(m.code, s, mode), None | Some((Insn::Undefined { .. }, _)));
        // A relocated pointer to a data word in the code segment (const objects the compiler put there) is not code.
        if bad {
            t.implausible.push(s);
            entries.remove(&s);
        }
    }
    let mut cache: HashMap<(u32, u32, bool), Walk> = HashMap::new();
    let mut stray = HashSet::new();
    loop {
        let funcs = ranges(&entries, size);
        let mut new: Vec<(u32, Mode, Seed)> = Vec::new();
        if let Some(c) = gap_entry(m, 0, funcs.first().map_or(size, |f| f.start), &[]) {
            new.push((c, Mode::Arm, Seed::Gap));
        }
        for f in &funcs {
            let w = cache.entry((f.start, f.end, f.mode == Mode::Thumb)).or_insert_with(|| walk(m, f.start, f.end, f.mode));
            for &(target, mode, call) in &w.exits {
                if target >= size || target % unit(mode) != 0 {
                    stray.insert(target);
                } else if !entries.contains_key(&target) {
                    new.push((target, mode, if call { Seed::Call } else { Seed::Split }));
                }
            }
            if w.hi < f.end {
                if let Some(c) = gap_entry(m, w.hi, f.end, &w.lits) {
                    new.push((c, Mode::Arm, Seed::Gap));
                }
            }
        }
        let mut grew = false;
        for (s, mode, why) in new {
            match entries.get(&s) {
                Some(&(had, _)) if had != mode => t.conflicts += 1,
                Some(_) => {}
                None => {
                    entries.insert(s, (mode, why));
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    t.stray_targets = stray.len();

    let r = repair(m.code, 0, size, &ranges(&entries, size));
    t.repaired = (r.dropped.len(), r.ended.len(), r.added.len());
    t.funcs = r.funcs;
    t.origin = t.funcs.iter().map(|f| entries.get(&f.start).map_or(Seed::Call, |e| e.1)).collect();
    for o in &t.origin {
        *t.origins.entry(*o).or_default() += 1;
    }

    let mut cov = vec![false; size as usize];
    for f in &t.funcs {
        let w = cache.remove(&(f.start, f.end, f.mode == Mode::Thumb)).unwrap_or_else(|| walk(m, f.start, f.end, f.mode));
        for &(a, n) in w.insns.iter().chain(&w.lits) {
            for b in a..a.saturating_add(n).min(size) {
                cov[b as usize] = true;
            }
        }
        let last = w.insns.last().filter(|&&(a, n)| a + n == f.end);
        if last.is_some_and(|&(a, _)| decode(m.code, a, f.mode).is_some_and(|(i, _)| falls(&i))) {
            t.falls_into_next += 1;
        }
    }
    t.covered = cov.iter().filter(|&&b| b).count() as u32;
    t
}

/// One module's result, as `build` returns it.
pub struct Built {
    pub name: String,
    pub file: String,
    /// The code segment's file offset (0x180 in every X module) and size.
    pub code_offset: u32,
    pub code_size: u32,
    pub table: Table,
    /// Offsets of the module's exports (named and indexed) that point into its code segment.
    pub exports_in_code: Vec<u32>,
}

/// Every `*.cro` and `static.crs` in a RomFS directory, sorted by file name.
pub fn load(romfs: &std::path::Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for e in std::fs::read_dir(romfs).map_err(|e| format!("{}: {e}", romfs.display()))? {
        let p = e.map_err(|e| e.to_string())?.path();
        let n = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if n.ends_with(".cro") || n == "static.crs" {
            files.push((n, std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?));
        }
    }
    files.sort();
    Ok(files)
}

/// Parses every module and builds every CRO's table (in parallel, biggest first). Returns the parsed modules
/// (static.crs included) and one `Built` per CRO, in file order. `with_undecodable` also runs codegen over each
/// table to list whole-function interpreter stubs (slow in debug builds).
pub fn build(files: &[(String, Vec<u8>)], with_undecodable: bool) -> Result<(Vec<(String, Cro)>, Vec<Built>), String> {
    let parsed: Vec<(String, Cro)> = files
        .iter()
        .map(|(n, b)| Cro::parse(b, n.ends_with(".crs")).map(|c| (n.clone(), c)).map_err(|e| format!("{n}: {e}")))
        .collect::<Result<_, _>>()?;
    let mods: Vec<(String, Cro, &[u8])> =
        parsed.iter().zip(files).map(|((n, c), (_, b))| (n.clone(), c.clone(), c.code_bytes(b))).collect();
    let prepared = prepare(&mods);
    let mut order: Vec<usize> = (0..prepared.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(prepared[i].code.len()));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut tables: Vec<Option<Table>> = vec![None; prepared.len()];
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    while let Some(&i) = order.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        let mut t = discover(&prepared[i]);
                        if with_undecodable {
                            t.undecodable = undecodable(prepared[i].code, &t.funcs);
                        }
                        done.push((i, t));
                    }
                    done
                })
            })
            .collect();
        for h in hs {
            for (i, t) in h.join().unwrap() {
                tables[i] = Some(t);
            }
        }
    });
    let cros = mods.iter().filter(|m| !m.1.is_static);
    let built = cros
        .zip(tables)
        .map(|((file, c, _), t)| {
            let code = c.code().unwrap_or(xr_hle::cro::format::Segment { offset: 0, size: 0, kind: xr_hle::cro::format::SegmentType::Code });
            let exports = c.named_exports.iter().map(|e| e.1).chain(c.indexed_exports.iter().copied());
            Built {
                name: c.name.clone(),
                file: file.clone(),
                code_offset: code.offset,
                code_size: code.size,
                table: t.unwrap(),
                exports_in_code: exports.filter_map(|t| c.code_offset(t)).collect(),
            }
        })
        .collect();
    Ok((parsed, built))
}

/// Codegen's view: functions whose whole body becomes an interpreter stub.
pub fn undecodable(code: &[u8], funcs: &[FuncEntry]) -> Vec<u32> {
    funcs
        .iter()
        .filter(|f| !gen_function(code, 0, f, funcs, &GenOptions::default()).undecodable.is_empty())
        .map(|f| f.start)
        .collect()
}

/// The table file: a header that says these are offsets, then xr-formats' functab lines, each with its origin
/// as a comment.
pub fn table_text(name: &str, file: &str, code_offset: u32, code_size: u32, funcs: &[FuncEntry], origin: &[Seed]) -> String {
    let lines: String = xr_formats::write_functab(funcs)
        .lines()
        .zip(origin.iter().map(Some).chain(std::iter::repeat(None)))
        .map(|(l, o)| match o {
            Some(o) => format!("{l} # {}
", format!("{o:?}").to_lowercase()),
            None => format!("{l}
"),
        })
        .collect();
    format!(
        "# {name} ({file}) function table, generated by xr-crotab (crates/xr-recomp) from the game's RomFS.\n\
         # Starts and ends are OFFSETS from the start of the module's code segment (segment 0, file offset 0x{code_offset:X}),\n\
         # not addresses: address at run time = where ldr:ro mapped the module + 0x{code_offset:X} + offset. See README.md.\n\
         # code_size: 0x{code_size:08X}\n\
         # count: {}\n{}",
        funcs.len(),
        lines
    )
}

/// `code_size` from a table file's header.
pub fn table_code_size(text: &str) -> Option<u32> {
    let l = text.lines().find_map(|l| l.strip_prefix("# code_size: 0x"))?;
    u32::from_str_radix(l.trim(), 16).ok()
}

/// code.bin functions the CROs import that `code.txt` lacks, as table changes.
#[derive(Debug, Default, PartialEq)]
pub struct CodeBinAdds {
    /// Entries for imports in no listed function: from the import to one past its last reachable instruction or
    /// literal (the rest of the gap stays free for `repair`'s branch-target additions), or to the next listed or
    /// added start when it has a computed jump or is Thumb.
    pub added: Vec<FuncEntry>,
    /// (listed function, import inside it): split there; it is a real entry (the previous word does not fall into
    /// it, no direct branch crosses it, no jump table before it).
    pub split: Vec<(FuncEntry, u32)>,
    /// (import | Thumb bit, why it is left to the interpreter).
    pub left: Vec<(u32, &'static str)>,
}

/// The changes for `imports` (code.bin addresses, bit 0 = Thumb). `code` is the text segment at `base`, `listed`
/// code.txt sorted by start.
pub fn code_bin_additions(code: &[u8], base: u32, listed: &[FuncEntry], imports: &[u32]) -> CodeBinAdds {
    let m = Module { name: "code.bin".into(), code, seeds: BTreeMap::new(), data_words: HashSet::new(), jump_tables: HashMap::new(), conflicts: 0 };
    let size = code.len() as u32;
    let containing = |a: u32| listed.get(listed.partition_point(|f| f.start <= a).checked_sub(1)?).filter(|f| a < f.end).copied();
    let is_start = |a: u32| listed.binary_search_by_key(&a, |f| f.start).is_ok();
    let gaps: Vec<u32> = imports.iter().map(|&a| a & !1).filter(|&a| containing(a).is_none()).collect();
    let insn = |a: u32, mode: Mode| decode(code, a.wrapping_sub(base), mode).map(|d| d.0);
    let computed = |i: &Insn| matches!(*i, Insn::Load { rt: Reg::PC, addr: xr_arm::Addr { offset: Offset::Reg { .. }, .. }, .. });
    let mut out = CodeBinAdds::default();
    for &imp in imports {
        let (a, mode) = (imp & !1, if imp & 1 != 0 { Mode::Thumb } else { Mode::Arm });
        let unit = if mode == Mode::Thumb { 2 } else { 4 };
        if a % unit != 0 || a < base || a >= base + size {
            out.left.push((imp, "misaligned or outside the text segment"));
        } else if is_start(a) {
            // listed already
        } else if let Some(f) = containing(a) {
            let prev = insn(a - unit, f.mode);
            let pad = is_padding(code, a - unit - base, f.mode) || matches!(prev, Some(Insn::Undefined { .. }));
            let crossing = sweep(code, base, &f).iter().any(|(at, i)| match *i {
                Insn::Branch { link: false, target, .. } => (*at < a) != (target < a) && (f.start..f.end).contains(&target),
                ref i => *at < a && computed(i),
            });
            if f.mode != mode {
                out.left.push((imp, "inside a function of the other mode"));
            } else if !(pad || prev.as_ref().is_some_and(|p| !falls(p))) {
                out.left.push((imp, "the previous instruction falls into it (a second entry of its function)"));
            } else if crossing {
                out.left.push((imp, "a branch or jump table of its function crosses it"));
            } else {
                out.split.push((f, a));
            }
        } else {
            let next = listed.get(listed.partition_point(|f| f.start <= a)).map_or(base + size, |f| f.start);
            // the next start, rounded down to this mode (a Thumb entry may follow ARM code at a halfword)
            let limit = gaps.iter().copied().filter(|&g| g > a).min().map_or(next, |g| g.min(next)) & !(unit - 1);
            if limit < a + unit {
                out.left.push((imp, "no room before the next function"));
                continue;
            }
            let w = walk(&m, a - base, limit - base, mode);
            let jumps = w.insns.iter().any(|&(o, _)| insn(o + base, mode).as_ref().is_some_and(computed));
            let end = if mode == Mode::Thumb || jumps { limit } else { (w.hi + base).next_multiple_of(unit).clamp(a + unit, limit) };
            out.added.push(FuncEntry { start: a, end, mode });
        }
    }
    out.added.sort_by_key(|f| f.start);
    out.added.dedup();
    out.split.sort_by_key(|s| s.1);
    out.split.dedup();
    out
}

/// Zero or NOP padding (`mov r0, r0`, ARM `nop`, Thumb `mov r8, r8`) at offset `o`.
fn is_padding(code: &[u8], o: u32, mode: Mode) -> bool {
    match mode {
        Mode::Arm => matches!(word(code, o), Some(0 | 0xE1A0_0000 | 0xE320_F000)),
        Mode::Thumb => matches!(code.get(o as usize..o as usize + 2), Some([0, 0] | [0xC0, 0x46])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cro::tests::tiny_cro;

    fn module(code: &[u32]) -> Vec<u8> {
        code.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn bare<'a>(code: &'a [u8], seeds: &[u32]) -> Module<'a> {
        Module {
            name: "t".into(),
            code,
            seeds: seeds.iter().map(|&s| (s, (Mode::Arm, Seed::Export))).collect(),
            data_words: HashSet::new(),
            jump_tables: HashMap::new(),
            conflicts: 0,
        }
    }

    fn starts(t: &Table) -> Vec<u32> {
        t.funcs.iter().map(|f| f.start).collect()
    }

    #[test]
    fn bl_targets_become_entries() {
        let code = module(&[
            0xE92D4010, // 0  push {r4, lr}
            0xEB000001, // 4  bl 0x10
            0xE8BD8010, // 8  pop {r4, pc}
            0xE12FFF1E, // c  bx lr (unreachable from 0)
            0xE3A00001, // 10 mov r0, #1
            0xE12FFF1E, // 14 bx lr
        ]);
        let t = discover(&bare(&code, &[0]));
        // the dead `bx lr` after the pop is plausible code past the function's end: a gap entry
        assert_eq!(starts(&t), vec![0, 0xC, 0x10]);
        assert_eq!(t.origins.get(&Seed::Call), Some(&1));
        assert_eq!(t.origins.get(&Seed::Gap), Some(&1));
        assert_eq!(t.funcs[0].end, 0xC);
        assert_eq!(t.funcs[2].end, 0x18);
    }

    /// A function only reached indirectly sits after the previous one's literal pool.
    #[test]
    fn code_after_a_literal_pool_is_a_function() {
        let code = module(&[
            0xE59F0000, // 0  ldr r0, [pc, #0] -> 8
            0xE12FFF1E, // 4  bx lr
            0x12345678, // 8  literal
            0xE3A00002, // c  mov r0, #2
            0xE12FFF1E, // 10 bx lr
        ]);
        let t = discover(&bare(&code, &[0]));
        assert_eq!(starts(&t), vec![0, 0xC]);
        assert_eq!(t.origins.get(&Seed::Gap), Some(&1));
        assert_eq!(t.covered, 0x14);
    }

    /// Jump table cases are local labels: no entries, all reachable; the table words count as covered.
    #[test]
    fn jump_table_cases_stay_inside() {
        let code = module(&[
            0xE3510002, // 0  cmp r1, #2
            0x379FF101, // 4  ldrlo pc, [pc, r1, lsl #2]
            0xEA000003, // 8  b 0x1c (default)
            0x00000014, // c  case 0 -> 0x14 (relocated word)
            0x00000018, // 10 case 1 -> 0x18
            0xE3A00000, // 14 mov r0, #0   (falls into case 1)
            0xE3A00001, // 18 mov r0, #1
            0xE12FFF1E, // 1c bx lr
        ]);
        let mut m = bare(&code, &[0]);
        m.jump_tables.insert(4, vec![0x14, 0x18]);
        m.data_words.extend([0xC, 0x10]);
        let t = discover(&m);
        assert_eq!(starts(&t), vec![0]);
        assert_eq!(t.covered, 0x20);
    }

    /// A B into the middle of another function splits it there.
    #[test]
    fn branch_into_another_function_splits_it() {
        let code = module(&[
            0xE3A00000, // 0  mov r0, #0
            0xEA000002, // 4  b 0x14
            0xE3A00001, // 8  (seed) mov r0, #1
            0xE3A00002, // c  mov r0, #2
            0xE12FFF1E, // 10 bx lr
            0xE3A00003, // 14 mov r0, #3
            0xE12FFF1E, // 18 bx lr
        ]);
        let t = discover(&bare(&code, &[0, 8]));
        assert_eq!(starts(&t), vec![0, 8, 0x14]);
        assert_eq!(t.origins.get(&Seed::Split), Some(&1));
    }

    #[test]
    fn literal_after_noreturn_call_is_not_code() {
        let code = module(&[
            0xE59F0004, // 0  ldr r0, [pc, #4] -> 0xc
            0xEB000002, // 4  bl 0x14 (noreturn)
            0xEB00000F, // 8  literal that decodes as bl (must not seed 0x4c)
            0x00000001, // c  literal
            0xE12FFF1E, // 10 bx lr (dead)
            0xEAFFFFFE, // 14 b . (abort)
        ]);
        let mut m = bare(&code, &[0]);
        m.data_words.insert(8); // a relocated word
        let t = discover(&m);
        assert_eq!(starts(&t), vec![0, 0x10, 0x14]);
    }

    #[test]
    fn seeds_from_exports_relocs_specials_and_imports() {
        let code = [0xE12FFF1E, 0xE12FFF1E, 0xE12FFF1E, 0x00000000];
        let b = tiny_cro(&code);
        let c = Cro::parse(&b, false).unwrap();
        // A second module importing Tiny's code+0xC anonymously.
        let mut imp = c.clone();
        imp.name = "Other".into();
        imp.imports = vec![crate::cro::Import {
            module: Some("Tiny".into()),
            symbol: ImportSymbol::Anonymous(xr_hle::cro::format::SegmentTag(0xC << 4)),
            sites: vec![xr_hle::cro::format::ExternalRelocation {
                target: xr_hle::cro::format::SegmentTag(1),
                kind: RelocationType::AbsoluteAddress,
                is_batch_end: true,
                addend: 0,
            }],
        }];
        let code_bytes = c.code_bytes(&b);
        let mods = vec![("Tiny.cro".to_string(), c.clone(), code_bytes), ("Other.cro".to_string(), imp, code_bytes)];
        let p = prepare(&mods);
        let s: Vec<(u32, Seed)> = p[0].seeds.iter().map(|(&o, &(_, w))| (o, w)).collect();
        assert_eq!(s, vec![(0, Seed::Export), (4, Seed::Special), (8, Seed::Reloc), (0xC, Seed::Import)]);
        assert!(p[0].data_words.contains(&0xC), "the import patch site in code is data");
    }

    /// A relocated pointer to a data word inside the code segment is reported, not listed.
    #[test]
    fn pointer_to_data_in_code_is_not_an_entry() {
        let code = module(&[0xE12FFF1E, 0x00000000]);
        let mut m = bare(&code, &[0, 4]);
        m.data_words.insert(4);
        let t = discover(&m);
        assert_eq!(starts(&t), vec![0]);
        assert_eq!(t.implausible, vec![4]);
    }

    #[test]
    fn table_text_round_trips() {
        let f = vec![FuncEntry { start: 0, end: 0x10, mode: Mode::Arm }, FuncEntry { start: 0x10, end: 0x14, mode: Mode::Thumb }];
        let s = table_text("T", "T.cro", 0x180, 0x14, &f, &[Seed::Export, Seed::Gap]);
        assert!(s.contains("0x00000010 0x00000014 thumb # gap
"), "{s}");
        assert_eq!(xr_formats::parse_functab(&s).unwrap(), f);
        assert_eq!(table_code_size(&s), Some(0x14));
        assert!(s.contains("OFFSETS"));
    }

    /// Imports into code.bin: one in a gap gets an entry up to its literal pool; inside a listed function, a real
    /// entry (after a return, nothing crossing) splits it, one that the previous instruction falls into or that a
    /// branch crosses is left to the interpreter; a listed start needs nothing.
    #[test]
    fn code_bin_imports_become_entries() {
        let code = module(&[
            0xE92D4010, 0xE8BD8010, // 1000 A: push {r4, lr}; pop {r4, pc}
            0xE92D4010, 0xE8BD8010, // 1008 import, after a return: split
            0xE3A00000, 0xE2800001, // 1010 mov r0, #0; 1014 import that mov falls into: left
            0xE12FFF1E, 0,          // 1018 bx lr; padding
            0xE59F0004, 0xE12FFF1E, // 1020 gap import: ldr r0, [pc, #4]; bx lr
            0, 0x12345678,          // 1028; 102C its literal
            0xE92D4010, 0xE8BD8010, // 1030 unlisted, not imported: stays a gap
            0, 0,                   // 1038
            0xE12FFF1E, 0,          // 1040 B (listed, imported)
            0xE3500000, 0x0A000001, // 1048 C: cmp r0, #0; beq 1058
            0xE12FFF1E, 0xE3A00001, // 1050 bx lr; 1054 import the beq crosses: left
            0xE12FFF1E, 0,          // 1058 bx lr
        ]);
        let e = |start, end| FuncEntry { start, end, mode: Mode::Arm };
        let listed = [e(0x1000, 0x1020), e(0x1040, 0x1048), e(0x1048, 0x1060)];
        let r = code_bin_additions(&code, 0x1000, &listed, &[0x1008, 0x1014, 0x1020, 0x1040, 0x1054]);
        assert_eq!(r.added, vec![e(0x1020, 0x1030)]);
        assert_eq!(r.split, vec![(e(0x1000, 0x1020), 0x1008)]);
        assert_eq!(r.left.iter().map(|l| l.0).collect::<Vec<_>>(), vec![0x1014, 0x1054]);
    }
}
