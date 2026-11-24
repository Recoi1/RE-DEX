# CRO function tables (x-usa-v0.2.0)

One table per CRO code module in the RomFS (133 files), written by `xr-crotab`:

```
cargo run --release -p xr-recomp --bin xr-crotab -- --romfs <dump>/romfs --functions functions/x-usa-v0.2.0
```

Only offsets are stored here, never game bytes.

## Offsets, not addresses

`<ModuleName>.txt` uses the `code.txt` line format (`start end arm|thumb`, end exclusive, parsed by
`xr_formats::parse_functab`). Every start and end is an **offset from the start of the module's code segment**
(segment 0, the `.text` segment). They are not addresses. The game decides where a CRO lives: ldr:ro maps the buffer
the game allocated from its own heap, so the address of an entry at run time is

    module base (the address ldr:ro mapped the CRO at) + code segment file offset + offset

In every X module the code segment starts at file offset `0x180`. Each file's header says so and gives
`code_size`; every entry ends at or before it. The file name is the module name from the CRO header, which is
also the name ldr:ro reports. One module differs from its file name: `DllEshop.cro` calls itself `DllEShop`.
The comment after each entry gives its origin (see below).

## How entries are found

Seeds:
- `export`: named and indexed exports that point into code. In X every module exports `nnroControlObject_`
  (code), and a few also export `SetIsCallDraw`.
- `special`: the header's control-object, on-load, on-exit and on-unresolved segment tags (0xA0–0xAC). The
  control object is set in all 133 modules; it is the same as the export. The other three are set in 5 modules.
- `reloc`: internal relocations whose symbol is the code segment. These are vtables and function-pointer tables
  in rodata/data, plus literal words in code. Words in a jump table are left out (see below).
- `import`: another module's anonymous import of this module's code. This includes static.crs, which means
  code.bin calls into the CRO through a patched pointer.

Import patch sites are never entries. They are data words: literal-pool words in code, or vtable slots that
receive the imported address.

The seeds then go through a fixed point. A function is the range from its entry up to the next entry. It is
walked from its entry through direct edges, falls and the cases of `ldr pc, [pc, rm, lsl #2]` jump tables
(whose words are relocated). The walk never decodes relocated words, patch sites or literal words the function
loads PC-relatively. The walk adds these as entries:
- `call`: BL/BLX targets.
- `split`: a B whose target lies in another function's range. That function is split there.
- `gap`: the first plausible ARM code after a function's last reachable instruction and literal pool. Plausible
  means an unconditional instruction, not data, whose linear run reaches a branch or return before anything
  undecodable. This catches functions that are only reached indirectly and are separated by a literal pool.

A seed that points at a data word in code (const objects the compiler put in `.text`, usually the last word of
the segment) is reported as implausible and left out. After the fixed point the repairs from
`xr_recomp::repair` run: drop tiny entries, end ranges at no-return calls, add outside branch targets.

## ARM or Thumb

An entry is Thumb only on evidence: a relocated pointer or import with bit 0 set, or a BLX imm from ARM (BL and B
keep the caller's mode). Everything else is ARM, because by interworking rules a pointer without bit 0 that is
entered through BX/BLX/LDR PC runs in ARM state. X's CROs have no odd code pointer, no BLX imm and no
word-misaligned export. Every entry is ARM. If a seed's first word does not decode in its mode, it is reported.

## Other files

- `summary.csv` has one row per module. `coverage_pct` is the share of the code segment covered by reachable
  instructions, literal words and jump tables of listed functions. The origin columns count entries by origin.
- `code_bin_imports.txt` lists the code.bin addresses that CROs import from `|static|`, and whether `code.txt`
  lists each one as a function start (`listed`), places it mid-function (`inside`), or does not cover it (`gap`).
