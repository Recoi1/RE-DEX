//! `xr-recomp --dump <dir> --functions <dir> --out <dir> [--jobs N]`: code.bin + function table -> C -> `xgame.dll`.
//!
//! Writes into `--out`: `gen_NNN.c` (code.bin, FUNCS_PER_FILE functions each), `mod_KKK_N.c` (CRO module k, compiled
//! base-relative from `<functions>/cro/<name>.txt`), `lookup.c` (`xr_lookup`, `xr_game_init`, module placement,
//! the guest globals; see `xr_recomp::dll`), `xr_abi.h`, `build.ninja`, `report.txt`, `progress-build.json`; then
//! runs `ninja` there.
//! Exit 0 when the DLL was built, 1 when the build failed, 2 on bad input.
//!
//! The function table is repaired in memory before generation (`code.txt` is never rewritten), see `repair`.
use xr_recomp::cro::Cro;
use xr_recomp::repair::{repair, sweep};
use xr_recomp::{crotab, dll};
use std::path::{Path, PathBuf};
use std::time::Instant;
use xr_arm::Insn;
use xr_codegen::{flag_table_with, gen_function, GenOptions};
use xr_formats::{parse_exheader, parse_functab, FuncEntry, Mode};

const FUNCS_PER_FILE: usize = 2000;
/// CRO functions per module file: smaller files keep ninja's jobs busy (DllField alone has 8,298).
const MOD_FUNCS_PER_FILE: usize = 1500;
const CFLAGS: &str = "-O2 -march=native -fno-strict-aliasing -fwrapv -DXR_BUILDING_GAME";

fn main() {
    std::process::exit(match run() {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("xr-recomp: {e}");
            2
        }
    })
}

fn run() -> Result<bool, String> {
    let (mut dump, mut functions, mut out, mut jobs) = (None, None, None, None::<String>);
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        let slot = match a.to_str() {
            Some("--dump") => &mut dump,
            Some("--functions") => &mut functions,
            Some("--out") => &mut out,
            Some("--jobs") => {
                let n = args.next().and_then(|v| v.into_string().ok()).filter(|v| v.parse::<u32>().is_ok_and(|n| n > 0));
                jobs = Some(n.ok_or("--jobs needs a positive number")?);
                continue;
            }
            _ => return Err(format!("unknown argument {a:?}\nusage: xr-recomp --dump <dir> --functions <dir> --out <dir> [--jobs N]")),
        };
        *slot = Some(PathBuf::from(args.next().ok_or(format!("{a:?} needs a value"))?));
    }
    let need = |p: Option<PathBuf>, n: &str| p.ok_or(format!("missing --{n}"));
    let (dump, functions, out) = (need(dump, "dump")?, need(functions, "functions")?, need(out, "out")?);
    let read = |p: PathBuf| std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()));

    let exh_bytes = read(dump.join("exheader.bin"))?;
    let exh = parse_exheader(&exh_bytes).map_err(|e| format!("exheader.bin: {e}"))?;
    let code = read(dump.join("exefs/code.bin"))?;
    let (base, seg_end) = (exh.text.addr, exh.text.addr + exh.text.size);
    if code.len() < exh.text.size as usize {
        return Err(format!("code.bin is {} bytes, shorter than the text segment ({})", code.len(), exh.text.size));
    }
    let table = String::from_utf8_lossy(&read(functions.join("code.txt"))?).into_owned();
    let listed = parse_functab(&table).map_err(|e| format!("code.txt: {e}"))?;
    // a line is the address, then `virtual` for a function the game also calls through pointers (vtables): xr_lookup
    // hands out its hook too
    let listed_hooks: Vec<(u32, bool)> = std::fs::read_to_string(functions.join("hooks.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.split('#').next().unwrap().trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut words = l.split_whitespace();
            let addr = words.next().and_then(|w| w.strip_prefix("0x")).and_then(|h| u32::from_str_radix(h, 16).ok());
            let virt = match words.next() {
                None => Some(false),
                Some("virtual") => Some(true),
                Some(_) => None,
            };
            addr.zip(virt).filter(|_| words.next().is_none()).ok_or(format!("hooks.txt: bad line {l:?}"))
        })
        .collect::<Result<_, _>>()?;
    let hooks: Vec<u32> = listed_hooks.iter().map(|h| h.0).collect();
    let virt: Vec<(u32, usize)> = listed_hooks.iter().enumerate().filter(|(_, h)| h.1).map(|(i, h)| (h.0, i)).collect();
    // the runtime fills the hook table by these indices: it checks the DLL was made with its list
    let hooks_hash = xr_hle::cro::code_hash(&hooks.iter().flat_map(|a| a.to_le_bytes()).collect::<Vec<u8>>());
    let build = std::fs::read_to_string(dump.join("build.txt"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| functions.file_name().unwrap_or_default().to_string_lossy().into_owned());
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;

    let t0 = Instant::now();
    let r = repair(&code, base, seg_end, &listed);
    // ldr:ro rewrites the static module's import literals when CROs load, so they must not fold to constants
    let patched = crs_patched(&read(dump.join("romfs/static.crs")).map_err(|e| format!("{e} (the dump must hold romfs/static.crs: its import words must not fold to constants)"))?);

    // the CRO modules: every *.cro of the RomFS, in file-name order (= their k), with its committed table
    let files = crotab::load(&dump.join("romfs"))?;
    let parsed: Vec<(String, Cro, &[u8])> = files
        .iter()
        .map(|(n, b)| {
            let c = Cro::parse(b, n.ends_with(".crs")).map_err(|e| format!("{n}: {e}"))?;
            let code = c.code_bytes(b);
            Ok((n.clone(), c, code))
        })
        .collect::<Result<_, String>>()?;
    let stat = &parsed.iter().find(|m| m.1.is_static).ok_or("romfs/static.crs: not a static module")?.1;
    let prepared = crotab::prepare(&parsed);
    let mut mods = Vec::new();
    let mut sites = 0;
    for ((file, c, code), pm) in parsed.iter().filter(|m| !m.1.is_static).zip(&prepared) {
        let tp = functions.join("cro").join(format!("{}.txt", c.name));
        let text = String::from_utf8_lossy(&read(tp.clone())?).into_owned();
        if crotab::table_code_size(&text) != Some(code.len() as u32) {
            return Err(format!("{}: its code_size is not {file}'s code segment (0x{:X} bytes); rerun xr-crotab", tp.display(), code.len()));
        }
        let funcs = parse_functab(&text).map_err(|e| format!("{}: {e}", tp.display()))?;
        sites += dll::check_patch_sites(c, pm, &funcs)?;
        let thunks = dll::thunks(c, code, stat, &funcs);

        mods.push(dll::Module { name: c.name.clone(), code, funcs, thunks });
    }
    // Flags returned across code.bin <-> CRO and CRO <-> CRO calls (all through patched literals, so no FlagTable
    // sees them): module callers' needs go into code.bin's table, code.bin's and other modules' into each module's,
    // to a fixed point.
    let cros: Vec<&Cro> = parsed.iter().filter(|m| !m.1.is_static).map(|m| &m.1).collect();
    let edges = dll::into_cro_edges(&code, base, stat, &cros, &mods);
    let mut mextra: Vec<Vec<(u32, xr_formats::Mode, u8)>> = vec![Vec::new(); mods.len()];
    let mut rounds = 0;
    let (mflags, demands, exe_flags) = loop {
        rounds += 1;
        let mflags: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = mods.iter().zip(&mextra).map(|(m, x)| s.spawn(move || dll::module_flags_with(m, x))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let demands = dll::thunk_flag_demands(&mods, &mflags);
        let exe_flags = flag_table_with(&code, base, &r.funcs, &hooks, &demands);
        let next = dll::into_cro_demands(&edges, &exe_flags, &mflags);
        if next == mextra || rounds == 6 {
            break (mflags, demands, exe_flags);
        }
        mextra = next;
    };
    let wired: usize = mextra.iter().map(Vec::len).sum();
    let unwired = dll::unwired_flag_edges(&edges, &mods, &exe_flags, &mflags);

    let opt = GenOptions { flags: exe_flags, hooks, patched, module: None };
    let fs = &opt.flags.stats;
    // flags returned into CRO code to callers nothing wires (code.bin -> CRO stubs, CRO -> CRO thunks)
    if !unwired.is_empty() {
        let msg = format!("{} flag edges into CRO code are not wired:\n{}\n", unwired.len(), unwired.join("\n"));
        std::fs::write(out.join("report.txt"), format!("unwired_flag_edges: {msg}")).map_err(|e| e.to_string())?;
        return Err(msg);
    }
    // (file name, source, undecodable starts): code.bin's files, then module k's
    let chunks: Vec<(String, String, Vec<u32>)> = std::thread::scope(|s| {
        let mut hs: Vec<_> = r
            .funcs
            .chunks(FUNCS_PER_FILE)
            .enumerate()
            .map(|(i, chunk)| {
                let (code, all, opt) = (&code, &r.funcs, &opt);
                s.spawn(move || {
                    let mut src = String::from("#include \"xr_abi.h\"\n");
                    let mut undec = Vec::new();
                    for f in chunk {
                        let g = gen_function(code, base, f, all, opt);
                        src.push_str(&g.c_source);
                        src.push('\n');
                        undec.extend(g.undecodable);
                    }
                    vec![(format!("gen_{i:03}"), src, undec)]
                })
            })
            .collect();
        for (k, (m, fl)) in mods.iter().zip(mflags).enumerate() {
            hs.push(s.spawn(move || {
                let (srcs, undec) = dll::module_sources(k as u32, m, fl, MOD_FUNCS_PER_FILE);
                // a module's interpreter stubs are reported with its first file
                srcs.into_iter().enumerate().map(|(j, src)| (format!("mod_{k:03}_{j}"), src, if j == 0 { undec.clone() } else { Vec::new() })).collect()
            }));
        }
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let mut objs = Vec::new();
    for (name, src, _) in &chunks {
        write_if_changed(&out.join(format!("{name}.c")), src.as_bytes())?;
        objs.push(name.clone());
    }
    objs.push("lookup".into());
    // the ARM11 local system capabilities' program id
    let title_id = u64::from_le_bytes(exh_bytes[0x200..0x208].try_into().unwrap());
    write_if_changed(&out.join("lookup.c"), dll::lookup_c(base, seg_end, &r.funcs, &game_info(&code, title_id, &build, hooks_hash), &mods, &virt).as_bytes())?;
    write_if_changed(&out.join("xr_abi.h"), include_bytes!("../../../abi/xr_abi.h"))?;
    let mut ninja = format!(
        "# generated by xr-recomp\ncflags = {CFLAGS}\nrule cc\n  command = clang $cflags -c $in -o $out\n  description = CC $in\n\
         rule link\n  command = clang -shared -o $out $in\n  description = LINK $out\n"
    );
    for o in &objs {
        ninja.push_str(&format!("build {o}.o: cc {o}.c | xr_abi.h\n"));
    }
    ninja.push_str(&format!("build xgame.dll: link {}\ndefault xgame.dll\n", objs.iter().map(|o| format!("{o}.o")).collect::<Vec<_>>().join(" ")));
    write_if_changed(&out.join("build.ninja"), ninja.as_bytes())?;
    let gen_seconds = t0.elapsed().as_secs_f64();
    let mod_funcs: usize = mods.iter().map(|m| m.funcs.len()).sum();
    println!(
        "xr-recomp: {} code.bin functions + {mod_funcs} in {} CRO modules ({sites} patch sites checked, {} thunk flag demands), {} files, generated in {gen_seconds:.1} s; running ninja",
        r.funcs.len(),
        mods.len(),
        demands.len(),
        chunks.len()
    );

    let t1 = Instant::now();
    let mut ninja_cmd = std::process::Command::new("ninja");
    ninja_cmd.arg("-C").arg(&out);
    if let Some(n) = &jobs {
        ninja_cmd.args(["-j", n]);
    }
    let status = ninja_cmd.status();
    let build_seconds = t1.elapsed().as_secs_f64();
    let dll = out.join("xgame.dll");
    let dll_built = matches!(&status, Ok(s) if s.success()) && dll.is_file();
    if let Err(e) = &status {
        eprintln!("xr-recomp: cannot run ninja: {e}");
    }

    let undec: Vec<u32> = chunks.iter().filter(|c| c.0.starts_with("gen_")).flat_map(|c| c.2.iter().copied()).collect();
    let mod_undec: Vec<String> = chunks
        .iter()
        .filter_map(|c| Some((c.0.strip_prefix("mod_")?.get(..3)?.parse::<usize>().ok()?, &c.2)))
        .flat_map(|(k, u)| {
            let name = &mods[k].name;
            u.iter().map(move |o| format!("{name}+0x{o:X}"))
        })
        .collect();
    let undec: Vec<(FuncEntry, String)> = undec
        .iter()
        .map(|&a| {
            let f = *r.funcs.iter().find(|f| f.start == a).unwrap();
            (f, undecodable_reason(&code, base, &f))
        })
        .collect();
    let hex = |a: u32| format!("0x{a:08X}");
    let mode = |m: Mode| if m == Mode::Thumb { "thumb" } else { "arm" };
    let mut rep = format!("build: {build}\nlisted: {}\n", listed.len());
    rep += &format!("dropped: {} (entries of length <= 1)\n", r.dropped.len());
    for f in &r.dropped {
        rep += &format!("  {} {} end {}\n", hex(f.start), mode(f.mode), hex(f.end));
    }
    rep += &format!("ended: {} (range ends after a no-return call whose fall-through reaches an undecodable word)\n", r.ended.len());
    for (f, e) in &r.ended {
        rep += &format!("  {} {} end {} -> {}\n", hex(f.start), mode(f.mode), hex(f.end), hex(*e));
    }
    rep += &format!("added: {} (direct B/BL/BLX targets outside all listed functions)\n", r.added.len());
    for (f, from) in &r.added {
        rep += &format!("  {} {} end {} from {}\n", hex(f.start), mode(f.mode), hex(f.end), hex(*from));
    }
    rep += &format!("skipped_targets: {} (outside the text segment, misaligned, or an added start in the other mode)\n", r.skipped);
    rep += &format!(
        "flags: {} functions take flags at entry, {} return flags; stores at {} tail/fall-off edges, {} direct calls, {} indirect sites ({} passes)\n",
        fs.entry_live, fs.returns_flags, fs.tail_edges, fs.call_edges, fs.indirect_sites, fs.passes
    );
    rep += &format!("functions: {}\nundecodable: {}\n", r.funcs.len(), undec.len());
    for (f, why) in &undec {
        rep += &format!("  {} {} {why}\n", hex(f.start), mode(f.mode));
    }
    rep += &format!(
        "modules: {} ({mod_funcs} functions, 0x{:X} bytes of code, {sites} patch sites, {} import thunks; {} code.bin functions return flags to module callers)\n",
        mods.len(),
        mods.iter().map(|m| m.code.len()).sum::<usize>(),
        mods.iter().map(|m| m.thunks.len()).sum::<usize>(),
        demands.len()
    );
    rep += &format!(
        "unwired_flag_edges: {} (of {} calls into CRO code through code.bin stubs and CRO->CRO thunks; {wired} CRO functions store flags for them, {rounds} rounds)\n",
        unwired.len(),
        edges.len()
    );
    for u in &unwired {
        rep += &format!("  {u}\n");
    }
    rep += &format!("module_undecodable: {} (whole-function interpreter stubs)\n", mod_undec.len());
    for u in &mod_undec {
        rep += &format!("  {u}\n");
    }
    rep += &format!("generate_seconds: {gen_seconds:.1}\nbuild_seconds: {build_seconds:.1}\n");
    rep += &match std::fs::metadata(&dll) {
        Ok(m) if dll_built => format!("dll: xgame.dll {} bytes\n", m.len()),
        _ => "dll: not built (see the ninja output)\n".into(),
    };
    std::fs::write(out.join("report.txt"), &rep).map_err(|e| e.to_string())?;
    let json = serde_json::json!({
        "build": build,
        "functions_listed": listed.len(),
        "functions_added_from_branch_targets": r.added.len(),
        "functions_generated": r.funcs.len(),
        "modules": mods.len(),
        "module_functions": mod_funcs,
        "module_undecodable": mod_undec,
        "undecodable": undec.iter().map(|(f, why)| serde_json::json!({"addr": hex(f.start), "reason": why})).collect::<Vec<_>>(),
        "dll_built": dll_built,
        "build_seconds": (build_seconds * 10.0).round() / 10.0,
    });
    std::fs::write(out.join("progress-build.json"), serde_json::to_string_pretty(&json).unwrap()).map_err(|e| e.to_string())?;
    println!(
        "xr-recomp: listed {}, dropped {}, ended {}, added {}, generated {}, undecodable {}; build {build_seconds:.1} s; dll {}",
        listed.len(),
        r.dropped.len(),
        r.ended.len(),
        r.added.len(),
        r.funcs.len(),
        undec.len(),
        if dll_built { "built" } else { "FAILED" }
    );
    Ok(dll_built)
}

/// Unchanged files keep their timestamp, so a rerun only rebuilds what changed.
/// Sorted addresses of the words a static module (`static.crs`) has ldr:ro patch: its external relocations'
/// targets (CRO imports; it has no internal ones). Its segment table holds absolute addresses.
fn crs_patched(crs: &[u8]) -> Vec<u32> {
    let u = |o: u32| crs.get(o as usize..o as usize + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    let (segments, nseg, relocs, nrel) = (u(0xC8), u(0xCC), u(0xF8), u(0xFC));
    let mut out: Vec<u32> = (0..nrel)
        .map(|i| u(relocs + 12 * i))
        .filter(|tag| tag & 0xF < nseg)
        .map(|tag| u(segments + 12 * (tag & 0xF)).wrapping_add(tag >> 4))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn write_if_changed(p: &Path, data: &[u8]) -> Result<(), String> {
    if std::fs::read(p).ok().as_deref() == Some(data) {
        return Ok(());
    }
    std::fs::write(p, data).map_err(|e| format!("{}: {e}", p.display()))
}

/// The `xr_game_info` definition: which code.bin (SHA-256), title, build and hook list the DLL was compiled from.
fn game_info(code: &[u8], title_id: u64, build: &str, hooks_hash: u64) -> String {
    use sha2::Digest;
    let sha: Vec<String> = sha2::Sha256::digest(code).iter().map(|b| format!("0x{b:02X}")).collect();
    let build: String = build.chars().filter(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\\')).collect();
    format!("XR_EXPORT const XrGameInfo xr_game_info = {{{{{}}}, 0x{title_id:016X}ull, \"{build}\", 0x{hooks_hash:016X}ull}};\n", sha.join(","))
}

/// The first undefined instruction of the range (linear sweep), as the reason for a miss stub.
fn undecodable_reason(code: &[u8], base: u32, f: &FuncEntry) -> String {
    let undef = sweep(code, base, f).into_iter().find(|i| matches!(i.1, Insn::Undefined { .. }));
    match (undef, f.mode) {
        (Some((a, Insn::Undefined { word })), Mode::Arm) => format!("undefined ARM word 0x{word:08X} at 0x{a:08X}"),
        (Some((a, Insn::Undefined { word })), Mode::Thumb) => format!("undefined Thumb halfword 0x{word:04X} at 0x{a:08X}"),
        _ => "empty or oversized range".into(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crs_patched_reads_external_relocation_targets() {
        let mut crs = vec![0u8; 0x200];
        let mut put = |o: usize, v: u32| crs[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(0xC8, 0x150); // segment table: text at 0x100000, data at 0x5A6000
        put(0xCC, 2);
        put(0x150, 0x100000);
        put(0x15C, 0x5A6000);
        put(0xF8, 0x180); // three relocations, the last naming a segment that does not exist
        put(0xFC, 3);
        put(0x180, (0x9A718 << 4) | 0);
        put(0x18C, (0x10 << 4) | 1);
        put(0x198, (0x10 << 4) | 7);
        assert_eq!(super::crs_patched(&crs), vec![0x19A718, 0x5A6010]);
    }
}
