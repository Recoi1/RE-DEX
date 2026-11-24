"""Port one build's curated code.bin function tables to another build of the same engine (Pokemon X -> Y).

An entry is ported only when its bytes match at the same address in the target code.bin:
- `exact`: every byte of the function is identical;
- `imm`: the only differing words are ARM data-processing-immediate instructions whose bits 31..12 (condition,
  opcode, registers) are identical, so only the constant changed (X/Y: the game version 24 -> 25). Such a change
  cannot move a function's start, end or mode.
Everything else is dropped and left to xr-recomp's repair (branch targets) and the interpreter.

code.txt and hooks.txt entries are functions; entry_points.txt lines are addresses, ported when the code.txt
function containing them is ported, or (outside every function) when the 32 bytes from the address match.
Comment and header lines are kept, with the source build id replaced by the target's.

usage: python -I port_tables.py <src code.bin> <dst code.bin> <src functions dir> <dst functions dir> <src id> <dst id>
"""
import bisect
import os
import struct
import sys

BASE = 0x100000


def match(a, b, mode):
    """'exact', 'imm' (only ARM data-processing immediates differ) or None, for two copies of one function."""
    if a == b:
        return "exact"
    if len(a) != len(b) or len(a) % 4 or mode != "arm":
        return None
    for o in range(0, len(a), 4):
        wa, wb = struct.unpack_from("<I", a, o)[0], struct.unpack_from("<I", b, o)[0]
        if wa != wb and not (wa >> 12 == wb >> 12 and (wa >> 25) & 7 == 1 and wa >> 28 != 0xF):
            return None
    return "imm"


def main():
    src_bin, dst_bin, src_dir, dst_dir, src_id, dst_id = sys.argv[1:7]
    x, y = open(src_bin, "rb").read(), open(dst_bin, "rb").read()

    def same(s, e, m):
        return match(x[s - BASE:e - BASE], y[s - BASE:e - BASE], m)

    os.makedirs(dst_dir, exist_ok=True)
    lines = open(os.path.join(src_dir, "code.txt")).read().splitlines()
    funcs, out, stats = [], [], {"exact": 0, "imm": 0, None: 0}
    dropped, imm = [], []
    for l in lines:
        if not l.startswith("0x"):
            out.append(l.replace(src_id, dst_id))
            continue
        s, e, m = l.split()[:3]
        s, e = int(s, 16), int(e, 16)
        k = same(s, e, m)
        stats[k] += 1
        funcs.append((s, e, k))
        if k:
            out.append(l)
            if k == "imm":
                imm.append(l)
        else:
            dropped.append(l)
    n = sum(1 for l in out if l.startswith("0x"))
    out = [("# count: %d" % n) if l.startswith("# count:") else l for l in out]
    out.insert(1, "# ported from %s by tools/functab/port_tables.py: %d exact, %d immediate-only, %d dropped"
               % (src_id, stats["exact"], stats["imm"], stats[None]))
    open(os.path.join(dst_dir, "code.txt"), "w", newline="\n").write("\n".join(out) + "\n")

    starts = [f[0] for f in funcs]

    def ported(a):
        i = bisect.bisect_right(starts, a) - 1
        if i >= 0 and funcs[i][0] <= a < funcs[i][1]:
            return funcs[i][2] is not None
        return x[a - BASE:a - BASE + 32] == y[a - BASE:a - BASE + 32]

    for name in ("entry_points.txt", "hooks.txt"):
        p = os.path.join(src_dir, name)
        if not os.path.exists(p):
            continue
        kept = drop = 0
        res = []
        for l in open(p).read().splitlines():
            if l.startswith("0x"):
                if not ported(int(l.split()[0], 16) & ~1):
                    drop += 1
                    continue
                kept += 1
            res.append(l.replace(src_id, dst_id))
        open(os.path.join(dst_dir, name), "w", newline="\n").write("\n".join(res) + "\n")
        print("%s: %d ported, %d dropped" % (name, kept, drop))
    print("code.txt: %d exact, %d immediate-only, %d dropped" % (stats["exact"], stats["imm"], stats[None]))
    for l in imm:
        print("  imm  " + l)
    for l in dropped:
        print("  drop " + l)


def _selftest():
    w = lambda *v: struct.pack("<%dI" % len(v), *v)
    ret = 0xE12FFF1E
    assert match(w(0xE3A00018, ret), w(0xE3A00018, ret), "arm") == "exact"
    assert match(w(0xE3A00018, ret), w(0xE3A00019, ret), "arm") == "imm"  # mov r0,#24 -> #25
    assert match(w(0xE3A00018, ret), w(0xE3A01019, ret), "arm") is None  # register changed
    assert match(w(0xEA000001, ret), w(0xEA000002, ret), "arm") is None  # branch target changed
    assert match(w(0x52563068, ret), w(0x37766C53, ret), "arm") is None  # literal data
    assert match(w(0xE3A00018), w(0xE3A00019), "thumb") is None


if __name__ == "__main__":
    _selftest()
    main()
