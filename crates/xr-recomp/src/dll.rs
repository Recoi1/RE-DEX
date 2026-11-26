//! What xgame.dll holds besides the code.bin functions: the CRO modules, compiled once in xr-codegen's module mode
//! (base-relative, so each runs wherever ldr:ro places it), and `lookup.c`: `xr_lookup` over code.bin and the
//! placed modules, `xr_place_module`/`xr_unplace_module`, and the module list the runtime maps names with (ABI 5).
use crate::cro::{Cro, ImportSymbol};
use crate::crotab;
use std::collections::HashSet;
use xr_codegen::{flag_table, flag_table_with, fn_name, gen_function, FlagTable, GenOptions};
use xr_formats::{FuncEntry, Mode};
use xr_hle::cro::format::RelocationType;

/// One CRO's code as xgame.dll compiles it; its index in the module list is its `k` (`xr_mod_base[k]`).
pub struct Module<'a> {
    /// The header's module name, which ldr:ro reports when it places the module.
    pub name: String,
    /// The code segment (segment 0) as in the file, before relocation.
    pub code: &'a [u8],
    /// The function table: offsets into `code` (`functions/<build>/cro/<name>.txt`).
    pub funcs: Vec<FuncEntry>,
    /// Import thunks into code.bin: (offset of a thunk function `ldr pc, [pc, #-4]`, the code.bin address its
    /// patched word receives, Thumb bit included).
    pub thunks: Vec<(u32, u32)>,
}

const LDR_PC_PC_M4: u32 = 0xE51F_F004;

pub fn module_flags(m: &Module) -> FlagTable { flag_table(m.code, 0, &m.funcs, &[]) }

/// `module_flags` plus return flags callers outside the module read (`into_cro_demands`).
pub fn module_flags_with(m: &Module, extra_ret: &[(u32, Mode, u8)]) -> FlagTable { flag_table_with(m.code, 0, &m.funcs, &[], extra_ret) }

/// What module code reads after calling code.bin through an import thunk, as (code.bin start, mode, flags) for
/// `flag_table_with`: the thunk is an indirect tail call, so only this tells the code.bin function to store the
/// flags at its returns (flag-returning helpers such as `__aeabi_cfcmple` called from a CRO).
// shortcut: thunks are ARM (every X CRO function is), a Thumb thunk would need its own mode here
pub fn thunk_flag_demands(mods: &[Module], flags: &[FlagTable]) -> Vec<(u32, Mode, u8)> {
    let mode = |a: u32| if a & 1 != 0 { Mode::Thumb } else { Mode::Arm };
    let mut by: std::collections::BTreeMap<u32, u8> = Default::default();
    for (m, f) in mods.iter().zip(flags) {
        for &(t, a) in &m.thunks {
            *by.entry(a).or_default() |= f.ret(t, Mode::Arm);
        }
    }
    by.into_iter().filter(|d| d.1 != 0).map(|(a, r)| (a & !1, mode(a), r)).collect()
}

/// Module `k`'s C sources (`per_file` functions each) and the starts of its whole-function interpreter stubs.
pub fn module_sources(k: u32, m: &Module, flags: FlagTable, per_file: usize) -> (Vec<String>, Vec<u32>) {
    let opt = GenOptions { flags, module: Some(k), ..Default::default() };
    let mut undec = Vec::new();
    let srcs = m
        .funcs
        .chunks(per_file)
        .map(|chunk| {
            let mut src = String::from("#include \"xr_abi.h\"\n");
            for f in chunk {
                let g = gen_function(m.code, 0, f, &m.funcs, &opt);
                src.push_str(&g.c_source);
                src.push('\n');
                undec.extend(g.undecodable);
            }
            src
        })
        .collect();
    (srcs, undec)
}

/// The import thunks of `cro` into code.bin (`stat` is static.crs): listed functions `ldr pc, [pc, #-4]` whose
/// next word an anonymous import from `|static|` patches.
pub fn thunks(cro: &Cro, code: &[u8], stat: &Cro, funcs: &[FuncEntry]) -> Vec<(u32, u32)> {
    let Some(seg0) = stat.segments.first() else { return Vec::new() };
    let word = |o: u32| code.get(o as usize..o as usize + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let mut out = Vec::new();
    for imp in cro.imports.iter().filter(|i| i.module.as_deref() == Some(stat.name.as_str())) {
        let ImportSymbol::Anonymous(tag) = imp.symbol else { continue };
        if tag.segment() != 0 {
            continue;
        }
        for s in &imp.sites {
            let Some(t) = cro.code_offset(s.target).and_then(|o| o.checked_sub(4)) else { continue };
            if word(t) == Some(LDR_PC_PC_M4) && funcs.binary_search_by_key(&t, |f| f.start).is_ok() {
                out.push((t, (seg0.offset + tag.offset()).wrapping_add(s.addend)));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// A call into CRO code through a patched literal whose caller may read flags after it: a code.bin static.crs stub
/// or a CRO thunk to another CRO's import (both `ldr pc, [pc, #-4]` + the word ldr:ro patches).
#[derive(Debug, Clone, PartialEq)]
pub struct IntoCro {
    /// The calling side: None = code.bin, Some(k) = module k.
    pub from: Option<usize>,
    /// The stub/thunk function (code.bin address, or offset in module `from`).
    pub stub: u32,
    /// The CRO function the word receives: (module k, code offset | Thumb bit); None when it does not resolve to
    /// a module's code (named/indexed imports, X has none).
    pub to: Option<(usize, u32)>,
}

/// Every such call: static.crs's imports (`stat`) patched into code.bin stubs (`exe` at `base`), and each module's
/// thunks to other modules' imports. `cros` are the modules' parsed CROs, in `mods` order (index = k).
pub fn into_cro_edges(exe: &[u8], base: u32, stat: &Cro, cros: &[&Cro], mods: &[Module]) -> Vec<IntoCro> {
    let wd = |b: &[u8], o: u32| b.get(o as usize..o as usize + 4).map(|w| u32::from_le_bytes(w.try_into().unwrap()));
    let resolve = |imp: &crate::cro::Import, addend: u32| {
        let ImportSymbol::Anonymous(tag) = imp.symbol else { return None };
        let k = cros.iter().position(|c| Some(c.name.as_str()) == imp.module.as_deref())?;
        Some((k, cros[k].code_offset(tag)?.wrapping_add(addend)))
    };
    let mut out = Vec::new();
    for imp in &stat.imports {
        for s in &imp.sites {
            let Some(seg) = stat.segments.get(s.target.segment() as usize) else { continue };
            let stub = (seg.offset + s.target.offset()).wrapping_sub(4);
            if stub.checked_sub(base).and_then(|o| wd(exe, o)) == Some(LDR_PC_PC_M4) {
                out.push(IntoCro { from: None, stub, to: resolve(imp, s.addend) });
            }
        }
    }
    for (k, (c, m)) in cros.iter().zip(mods).enumerate() {
        for imp in c.imports.iter().filter(|i| i.module.as_deref() != Some(stat.name.as_str())) {
            for s in &imp.sites {
                let Some(t) = c.code_offset(s.target).and_then(|o| o.checked_sub(4)) else { continue };
                if wd(m.code, t) == Some(LDR_PC_PC_M4) && m.funcs.binary_search_by_key(&t, |f| f.start).is_ok() {
                    out.push(IntoCro { from: Some(k), stub: t, to: resolve(imp, s.addend) });
                }
            }
        }
    }
    out.sort_by_key(|e| (e.from, e.stub));
    out.dedup();
    out
}

fn caller_mask(e: &IntoCro, exe_flags: &FlagTable, mflags: &[FlagTable]) -> u8 {
    match e.from {
        None => exe_flags.ret(e.stub, Mode::Arm),
        Some(k) => mflags[k].ret(e.stub, Mode::Arm),
    }
}

/// The return flags each module function must store for callers into it (`edges`), per module, for
/// `flag_table_with` (the mirror of `thunk_flag_demands`).
pub fn into_cro_demands(edges: &[IntoCro], exe_flags: &FlagTable, mflags: &[FlagTable]) -> Vec<Vec<(u32, Mode, u8)>> {
    let mut by: Vec<std::collections::BTreeMap<u32, u8>> = vec![Default::default(); mflags.len()];
    for e in edges {
        if let Some((k, off)) = e.to {
            *by[k].entry(off).or_default() |= caller_mask(e, exe_flags, mflags);
        }
    }
    let mode = |a: u32| if a & 1 != 0 { Mode::Thumb } else { Mode::Arm };
    by.into_iter().map(|m| m.into_iter().filter(|d| d.1 != 0).map(|(a, r)| (a & !1, mode(a), r)).collect()).collect()
}

/// Edges whose caller reads flags the CRO target does not store (its module FlagTable's return mask lacks them):
/// one line each. xr-recomp wires `into_cro_demands` to a fixed point first and fails the build on any left.
pub fn unwired_flag_edges(edges: &[IntoCro], mods: &[Module], exe_flags: &FlagTable, mflags: &[FlagTable]) -> Vec<String> {
    let mut out = Vec::new();
    for e in edges {
        let mask = caller_mask(e, exe_flags, mflags);
        let stored = e.to.map_or(0, |(k, off)| mflags[k].ret(off & !1, if off & 1 != 0 { Mode::Thumb } else { Mode::Arm }));
        if mask & !stored != 0 {
            let from = e.from.map_or_else(|| format!("code.bin stub 0x{:08X}", e.stub), |k| format!("{} thunk +0x{:X}", mods[k].name, e.stub));
            let to = e.to.map_or_else(|| "an import that is no module's code".into(), |(k, off)| format!("{}+0x{off:X}", mods[k].name));
            out.push(format!("{from} -> {to}: callers read flags 0x{mask:X}, it stores 0x{stored:X}: wire extra_ret for this edge"));
        }
    }
    out
}

/// Every relocation and import patch site in `cro`'s code segment must be an absolute data word that no listed
/// function executes: native code has its instructions compiled in, so a patched instruction would be missed.
/// `m` is the module's discovery input (`crotab::prepare`). Returns the number of sites checked.
pub fn check_patch_sites(cro: &Cro, m: &crotab::Module, funcs: &[FuncEntry]) -> Result<usize, String> {
    let name = &cro.name;
    let internal = cro.internal.iter().map(|r| (r.target, r.kind));
    let imports = cro.imports.iter().flat_map(|i| &i.sites).map(|s| (s.target, s.kind));
    let mut sites = HashSet::new();
    for (target, kind) in internal.chain(imports) {
        let Some(o) = cro.code_offset(target) else { continue };
        match kind {
            RelocationType::Nothing => {}
            RelocationType::AbsoluteAddress if o % 4 == 0 => {
                sites.insert(o);
            }
            k => return Err(format!("{name}: the patch site at code +0x{o:X} is {k:?}; native modules support only aligned absolute data words")),
        }
    }
    for f in funcs {
        // the walk that stops only at literals it loads itself, not at the patch sites
        let w = crotab::walk_from(m, f.start, f.end, f.mode, HashSet::new());
        if let Some(&(a, _)) = w.insns.iter().find(|&&(a, n)| (a & !3..a + n).step_by(4).any(|x| sites.contains(&x))) {
            return Err(format!("{name}: the patch site at code +0x{:X} is an instruction of function +0x{:X}", a & !3, f.start));
        }
    }
    Ok(sites.len())
}

/// `lookup.c`. code.bin: one `XrFn` slot per 2 bytes of the text segment, slot `(a - base) >> 1` holding the
/// function that starts at `a`, and a parallel byte saying whether it is Thumb (so `xr_lookup(start | 1)` finds only
/// Thumb functions and `xr_lookup(start)` only ARM ones). Modules: the same per 2 bytes of each code segment, all in
/// one table (module k's slots from `xr_mod_slot[k]`), searched for the placed modules only. The tables are zeroed
/// bss filled by `xr_game_init` from compact lists, which keeps the DLL small.
pub fn lookup_c(base: u32, seg_end: u32, funcs: &[FuncEntry], info: &str, mods: &[Module], virt: &[(u32, usize)]) -> String {
    let n = mods.len();
    let name = |f: &FuncEntry, k: Option<u32>| fn_name(f.start, f.mode, k);
    let mut s = String::from("#include \"xr_abi.h\"\nuint8_t* xr_mem;\nconst XrRuntime* xr_rt;\n");
    s += info;
    for f in funcs {
        s += &format!("void {}(XrCpu* c);\n", name(f, None));
    }
    for (k, m) in mods.iter().enumerate() {
        for f in &m.funcs {
            s += &format!("void {}(XrCpu* c);\n", name(f, Some(k as u32)));
        }
    }
    s += &format!("#define XR_TEXT_BASE 0x{base:08X}u\n#define XR_TAB_N 0x{:X}u\n", (seg_end - base).div_ceil(2));
    s += "static XrFn xr_tab[XR_TAB_N];\nstatic uint8_t xr_tab_thumb[XR_TAB_N];\n";
    s += "static const struct { uint32_t addr; XrFn fn; } xr_funcs[] = {\n";
    for f in funcs {
        s += &format!("    {{0x{:08X}u, {}}},\n", f.start | (f.mode == Mode::Thumb) as u32, name(f, None));
    }
    s += "};\n";

    // C has no empty arrays: a DLL without modules still gets one (unused) element
    let q = |t: &str| format!("\"{}\"", t.chars().filter(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\\')).collect::<String>());
    let list = |v: Vec<String>| if v.is_empty() { "0".to_string() } else { v.join(", ") };
    let (mut slot, mut slots) = (0u32, Vec::new());
    for m in mods {
        slots.push(slot);
        slot += (m.code.len() as u32).div_ceil(2);
    }
    s += &format!("#define XR_MOD_N {n}u\n#define XR_MOD_DIM {}\n#define XR_MTAB_N 0x{:X}u\n", n.max(1), slot.max(1));
    s += "uint32_t xr_mod_base[XR_MOD_DIM];\n";
    s += "XR_EXPORT const uint32_t xr_module_count = XR_MOD_N;\n";
    s += &format!("XR_EXPORT const char* const xr_module_names[XR_MOD_DIM] = {{{}}};\n", list(mods.iter().map(|m| q(&m.name)).collect()));
    s += &format!(
        "XR_EXPORT const uint64_t xr_module_hash[XR_MOD_DIM] = {{{}}};\n",
        list(mods.iter().map(|m| format!("0x{:016X}ull", xr_hle::cro::code_hash(m.code))).collect())
    );
    s += &format!("static const uint32_t xr_mod_size[XR_MOD_DIM] = {{{}}};\n", list(mods.iter().map(|m| format!("0x{:X}u", m.code.len())).collect()));
    s += &format!("static const uint32_t xr_mod_slot[XR_MOD_DIM] = {{{}}};\n", list(slots.iter().map(|v| format!("0x{v:X}u")).collect()));
    s += "static XrFn xr_mtab[XR_MTAB_N];\nstatic uint8_t xr_mtab_thumb[XR_MTAB_N];\n";
    s += "static const struct { uint32_t k, off; XrFn fn; } xr_mfuncs[] = {\n";
    for (k, m) in mods.iter().enumerate() {
        for f in &m.funcs {
            s += &format!("    {{{k}u, 0x{:06X}u, {}}},\n", f.start | (f.mode == Mode::Thumb) as u32, name(f, Some(k as u32)));
        }
    }
    if mods.iter().all(|m| m.funcs.is_empty()) {
        s += "    {0u, 0u, 0},\n";
    }
    s += "};\n\
/* the placed modules (indices into xr_mod_base), in no order */\n\
static uint32_t xr_placed[XR_MOD_DIM];\n\
static uint32_t xr_placed_n;\n\
XR_EXPORT uint32_t xr_game_init(const XrRuntime* rt) {\n\
    xr_mem = rt->mem;\n\
    xr_rt = rt;\n\
    for (uint32_t i = 0; i < sizeof xr_funcs / sizeof xr_funcs[0]; i++) {\n\
        uint32_t s = (xr_funcs[i].addr - XR_TEXT_BASE) >> 1;\n\
        xr_tab[s] = xr_funcs[i].fn;\n\
        xr_tab_thumb[s] = (uint8_t)(xr_funcs[i].addr & 1);\n\
    }\n\
    for (uint32_t i = 0; i < sizeof xr_mfuncs / sizeof xr_mfuncs[0]; i++) {\n\
        if (!xr_mfuncs[i].fn) continue;\n\
        uint32_t s = xr_mod_slot[xr_mfuncs[i].k] + (xr_mfuncs[i].off >> 1);\n\
        xr_mtab[s] = xr_mfuncs[i].fn;\n\
        xr_mtab_thumb[s] = (uint8_t)(xr_mfuncs[i].off & 1);\n\
    }\n\
    return XR_ABI_VERSION;\n\
}\n\
XR_EXPORT XrFn xr_lookup_native(uint32_t a) {\n\
    uint32_t s = (a - XR_TEXT_BASE) >> 1;\n\
    if (s < XR_TAB_N) return xr_tab_thumb[s] == (a & 1) ? xr_tab[s] : 0;\n\
    for (uint32_t i = 0; i < xr_placed_n; i++) {\n\
        uint32_t k = xr_placed[i], o = a - xr_mod_base[k];\n\
        if (o < xr_mod_size[k]) {\n\
            s = xr_mod_slot[k] + (o >> 1);\n\
            return xr_mtab_thumb[s] == (a & 1) ? xr_mtab[s] : 0;\n\
        }\n\
    }\n\
    return 0;\n\
}\n\
";
    // the hooks the game also reaches through pointers: xr_lookup hands out the hook (the runtime calls the function
    // itself through xr_lookup_native)
    s += "XR_EXPORT XrFn xr_lookup(uint32_t a) {\n";
    if !virt.is_empty() {
        s += "    if (xr_rt && xr_rt->hooks) switch (a) {\n";
        for (addr, i) in virt {
            s += &format!("        case 0x{addr:08X}u: if (xr_rt->hooks[{i}]) return xr_rt->hooks[{i}]; break;\n");
        }
        s += "    }\n";
    }
    s += "    return xr_lookup_native(a);\n}\n\
XR_EXPORT void xr_unplace_module(uint32_t k) {\n\
    for (uint32_t i = 0; i < xr_placed_n; i++)\n\
        if (xr_placed[i] == k) { xr_placed[i] = xr_placed[--xr_placed_n]; return; }\n\
}\n\
XR_EXPORT uint32_t xr_place_module(uint32_t k, uint32_t base) {\n\
    if (k >= XR_MOD_N || (base & 3)) return 0;\n\
    xr_unplace_module(k);\n\
    /* a module placed over another's memory replaces it (an unload nobody reported): lookups never return the old one */\n\
    for (uint32_t i = 0; i < xr_placed_n;) {\n\
        uint32_t j = xr_placed[i];\n\
        if ((uint64_t)base < (uint64_t)xr_mod_base[j] + xr_mod_size[j] && (uint64_t)xr_mod_base[j] < (uint64_t)base + xr_mod_size[k]) xr_unplace_module(j);\n\
        else i++;\n\
    }\n\
    xr_mod_base[k] = base;\n\
    xr_placed[xr_placed_n++] = k;\n\
    return 1;\n\
}\n";
    s
}
