//! `xr-crotab --romfs <dir> --functions <dir> [--update-code-txt [--also <entries>]...]`: a function table per CRO
//! code module.
//!
//! Reads every `*.cro` and `static.crs` in the unpacked RomFS and writes into `<functions>/cro/`:
//! `<ModuleName>.txt` (offsets into the module's code segment, see `xr_recomp::crotab`), `summary.csv`, and
//! `code_bin_imports.txt` (the code.bin addresses the CROs import from `|static|`, and whether `code.txt` lists
//! each as a function start). Prints a report: per-module counts (biggest first), coverage, modules under 80%.
//! With --update-code-txt it first adds the code.bin functions the CROs import to `<functions>/code.txt` (entries for
//! imports in no listed function, splits for real entries inside one; `crotab::code_bin_additions`), reading
//! `<romfs>/../exheader.bin` and `<romfs>/../exefs/code.bin`, and prints the imports it leaves to the interpreter.
//! `--also` adds more code.bin entry points the same way: a file of `0xADDRESS arm|thumb ...` lines, such as the
//! runtime's reviewed `misses-proposal.txt` (code the game entered in the interpreter through function pointers).
//! Exit 0 when everything was written, 2 on bad input.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use xr_formats::{parse_exheader, parse_functab, write_functab, FuncEntry, Mode};
use xr_recomp::cro::ImportSymbol;
use xr_recomp::crotab::{build, code_bin_additions, load, table_text, Seed};

fn main() {
    if let Err(e) = run() {
        eprintln!("xr-crotab: {e}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let (mut romfs, mut functions, mut update, mut also) = (None, None, false, Vec::new());
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        if a == "--update-code-txt" {
            update = true;
            continue;
        }
        if a == "--also" {
            let p = PathBuf::from(args.next().ok_or("--also needs a file")?);
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            for l in text.lines() {
                let w: Vec<&str> = l.split_whitespace().collect();
                let (Some(h), Some(m)) = (w.first().and_then(|h| h.strip_prefix("0x")), w.get(1)) else { continue };
                let a = u32::from_str_radix(h, 16).map_err(|_| format!("{}: bad line {l:?}", p.display()))?;
                also.push(a | (*m == "thumb") as u32);
            }
            continue;
        }
        let slot = match a.to_str() {
            Some("--romfs") => &mut romfs,
            Some("--functions") => &mut functions,
            _ => return Err(format!("unknown argument {a:?}\nusage: xr-crotab --romfs <dir> --functions <dir> [--update-code-txt]")),
        };
        *slot = Some(PathBuf::from(args.next().ok_or(format!("{a:?} needs a value"))?));
    }
    let romfs = romfs.ok_or("missing --romfs")?;
    let functions = functions.ok_or("missing --functions")?;
    let files = load(&romfs)?;
    let (parsed, built) = build(&files, true)?;

    let dir = functions.join("cro");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let write = |name: &str, text: &str| {
        let p = dir.join(name);
        std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))
    };
    let mut csv = String::from(
        "module,file,code_size,functions,arm,thumb,undecodable,coverage_pct,export,special,reloc,import,call,split,gap,jump_tables,implausible_seeds\n",
    );
    let (mut tot_size, mut tot_cov, mut tot_funcs, mut tot_undec, mut tot_jt, mut tot_fall) = (0u64, 0u64, 0usize, 0usize, 0usize, 0usize);
    let mut tot_origin: BTreeMap<Seed, usize> = BTreeMap::new();
    let (mut low, mut rows) = (Vec::new(), Vec::new());
    for b in &built {
        let t = &b.table;
        let text = table_text(&b.name, &b.file, b.code_offset, b.code_size, &t.funcs, &t.origin);
        parse_functab(&text).map_err(|e| format!("{}: generated table does not parse: {e}", b.name))?;
        write(&format!("{}.txt", b.name), &text)?;
        let thumb = t.funcs.iter().filter(|f| f.mode == Mode::Thumb).count();
        let cov = 100.0 * t.covered as f64 / b.code_size.max(1) as f64;
        let o = |s: Seed| t.origins.get(&s).copied().unwrap_or(0);
        csv += &format!(
            "{},{},{},{},{},{thumb},{},{cov:.2},{},{},{},{},{},{},{},{},{}\n",
            b.name,
            b.file,
            b.code_size,
            t.funcs.len(),
            t.funcs.len() - thumb,
            t.undecodable.len(),
            o(Seed::Export),
            o(Seed::Special),
            o(Seed::Reloc),
            o(Seed::Import),
            o(Seed::Call),
            o(Seed::Split),
            o(Seed::Gap),
            t.jump_tables,
            t.implausible.len()
        );
        tot_size += b.code_size as u64;
        tot_cov += t.covered as u64;
        tot_funcs += t.funcs.len();
        tot_undec += t.undecodable.len();
        tot_jt += t.jump_tables;
        tot_fall += t.falls_into_next;
        for (k, v) in &t.origins {
            *tot_origin.entry(*k).or_default() += v;
        }
        if cov < 80.0 {
            low.push(format!("{} {cov:.1}%", b.name));
        }
        rows.push((b.code_size, format!(
            "{:<24} code 0x{:06X} funcs {:>5} cov {cov:6.2}% undec {} falls-into-next {} jt {:>3} implausible {} conflicts {} stray {} repairs d/e/a {:?} origins {:?}",
            b.name, b.code_size, t.funcs.len(), t.undecodable.len(), t.falls_into_next, t.jump_tables, t.implausible.iter().map(|s| format!("+0x{s:X}")).collect::<Vec<_>>().join(" "), t.conflicts, t.stray_targets, t.repaired, t.origins
        )));
        for s in &t.undecodable {
            println!("  undecodable {}+0x{s:X}", b.name);
        }
    }
    write("summary.csv", &csv)?;
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (_, r) in &rows {
        println!("{r}");
    }

    // Who imports whose code: CROs -> code.bin (anonymous imports from |static|) and code.bin -> CROs.
    let code_txt_text = std::fs::read_to_string(functions.join("code.txt")).unwrap_or_default();
    let mut code_txt: Vec<FuncEntry> = parse_functab(&code_txt_text).unwrap_or_default();
    let stat = parsed.iter().find(|m| m.1.is_static).map(|m| &m.1);
    let (mut to_exe, mut exe_sites, mut from_exe, mut cro_to_cro) = (BTreeMap::new(), 0usize, BTreeSet::new(), 0usize);
    for (_, c) in &parsed {
        for imp in &c.imports {
            let (Some(from), ImportSymbol::Anonymous(tag)) = (&imp.module, &imp.symbol) else { continue };
            if let Some(s) = stat.filter(|s| &s.name == from) {
                if let Some(seg) = s.segments.first().filter(|_| tag.segment() == 0) {
                    for site in &imp.sites {
                        to_exe.entry((seg.offset + tag.offset()).wrapping_add(site.addend)).or_insert_with(BTreeSet::new).insert(c.name.clone());
                        exe_sites += 1;
                    }
                }
            } else if c.is_static {
                from_exe.insert((from.clone(), tag.0));
            } else {
                cro_to_cro += 1;
            }
        }
    }
    if update {
        let entries: BTreeSet<u32> = to_exe.keys().copied().chain(also).collect();
        code_txt = update_code_txt(&romfs, &functions.join("code.txt"), &code_txt_text, code_txt, &entries.into_iter().collect::<Vec<_>>())?;
    }
    let status = |a: u32| {
        let i = code_txt.partition_point(|f| f.start <= a);
        match i.checked_sub(1).map(|i| code_txt[i]) {
            Some(f) if f.start == a => "listed",
            Some(f) if a < f.end => "inside",
            _ => "gap",
        }
    };
    let mut imp_txt = String::from(
        "# code.bin addresses (absolute) the CROs import from |static| (static.crs exports by segment tag), from xr-crotab.\n\
         # mode: bit 0 of the imported value (set = Thumb; the address is printed without it). status vs code.txt: listed (a function start), inside (mid-function), gap (in no listed function).\n\
         # A CRO reaches these through patched literal-pool/vtable words, i.e. indirect calls through xr_lookup.\n\
         # address mode status importers\n",
    );
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (a, who) in &to_exe {
        let st = status(a & !1);
        *counts.entry(st).or_default() += 1;
        let mode = if a & 1 != 0 { "thumb" } else { "arm" };
        imp_txt += &format!("0x{:08X} {mode} {st} {}\n", a & !1, who.iter().cloned().collect::<Vec<_>>().join(","));
    }
    write("code_bin_imports.txt", &imp_txt)?;
    println!("imports: CROs import {} distinct code.bin addresses ({exe_sites} patch sites); vs code.txt {counts:?}", to_exe.len());
    println!("imports: static.crs (code.bin) imports {} distinct CRO symbols; CRO->CRO anonymous imports: {cro_to_cro}", from_exe.len());
    println!(
        "total: {} modules, code 0x{tot_size:X} bytes, {tot_funcs} functions, coverage {:.2}%, undecodable {tot_undec}, falls into next {tot_fall}, jump tables {tot_jt}, origins {tot_origin:?}",
        built.len(),
        100.0 * tot_cov as f64 / tot_size.max(1) as f64
    );
    println!("under 80% coverage: {}", if low.is_empty() { "none".to_string() } else { low.join(", ") });
    Ok(())
}

/// Adds the CROs' code.bin imports to code.txt (see `code_bin_additions`), keeping its header (count updated, plus
/// a line saying what was added); returns the new table.
fn update_code_txt(romfs: &std::path::Path, path: &std::path::Path, text: &str, mut table: Vec<FuncEntry>, imports: &[u32]) -> Result<Vec<FuncEntry>, String> {
    let dump = romfs.parent().ok_or("--romfs has no parent (the dump)")?;
    let read = |p: PathBuf| std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()));
    let exh = parse_exheader(&read(dump.join("exheader.bin"))?).map_err(|e| format!("exheader.bin: {e}"))?;
    let code = read(dump.join("exefs/code.bin"))?;
    let text_seg = code.get(..exh.text.size as usize).ok_or("code.bin is shorter than its text segment")?;
    let adds = code_bin_additions(text_seg, exh.text.addr, &table, imports);
    if adds.added.is_empty() && adds.split.is_empty() {
        println!("code.txt: nothing to add ({} left to the interpreter)", adds.left.len());
        return Ok(table);
    }
    // in address order, so a second split of the same function finds its upper part
    for (_, a) in &adds.split {
        let i = table.iter().position(|x| x.start < *a && *a < x.end).unwrap();
        let f = table[i];
        table[i].end = *a;
        table.insert(i + 1, FuncEntry { start: *a, ..f });
    }
    table.extend(adds.added.iter().copied());
    table.sort_by_key(|f| f.start);
    for w in table.windows(2) {
        if w[0].end > w[1].start {
            return Err(format!("code.txt additions overlap: {:?} and {:?}", w[0], w[1]));
        }
    }
    let note = format!(
        "# CRO imports: {} entries added and {} functions split by xr-crotab --update-code-txt (functions only the CROs call, and --also entry points)",
        adds.added.len(),
        adds.split.len()
    );
    let head: Vec<String> = text
        .lines()
        .filter(|l| !l.starts_with("0x") && !l.starts_with("# CRO imports:"))
        .map(|l| if l.starts_with("# count:") { format!("# count: {}", table.len()) } else { l.to_string() })
        .chain([note])
        .collect();
    std::fs::write(path, head.join("\n") + "\n" + &write_functab(&table)).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("code.txt: added {}, split {}, left to the interpreter {}", adds.added.len(), adds.split.len(), adds.left.len());
    for (a, why) in &adds.left {
        println!("  left 0x{:08X} {}: {why}", a & !1, if a & 1 != 0 { "thumb" } else { "arm" });
    }
    Ok(table)
}
