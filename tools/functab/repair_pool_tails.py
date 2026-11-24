"""Task 5 code.txt repair, round 2: Ghidra ends some ARM functions at a literal pool in their middle; the code after
the pool (reached by a B from the function, e.g. its shared epilogue) became separate repair-added functions, which
run with a stale entry lr and return through the interpreter. Extend such a function F over the gap after it when a
repair-added function in the gap was reached by a plain B from F: up to the next listed start, or the first address
in the gap that any BL/BLX word in the text segment targets (a real function), whichever comes first.
usage: python -I apply2.py <code.txt> <code.bin> <report.txt> <text_end>"""
import struct, sys, bisect

codetxt, codebin, report, text_end = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4], 16)
code = open(codebin, "rb").read()
BASE = 0x100000
lines = open(codetxt).read().splitlines()
head = [l for l in lines if not l.startswith("0x")]
funcs = sorted([int(s, 16), int(e, 16), m] for s, e, m in (l.split()[:3] for l in lines if l.startswith("0x")))
added = sorted((int(p[0], 16), int(p[5], 16)) for p in (l.split() for l in open(report))
               if len(p) == 6 and p[2] == "end" and p[4] == "from")


def word(a):
    return struct.unpack_from("<I", code, a - BASE)[0]


# every BL (any condition) / BLX imm target in the text, ARM encodings only
called = set()
for a in range(BASE, text_end, 4):
    w = word(a)
    if (w >> 24) & 0xF == 0xB or w >> 25 == 0x7D:
        off = (w & 0xFFFFFF) << 2
        if off & 0x2000000:
            off -= 1 << 26
        t = (a + 8 + off) & 0xFFFFFFFF
        if w >> 25 == 0x7D:
            t |= 1  # BLX imm goes to Thumb
        called.add(t)

ext = 0
for i, f in enumerate(funcs):
    s, e, m = f
    nxt = funcs[i + 1][0] if i + 1 < len(funcs) else e
    if m != "arm" or nxt <= e:
        continue
    gap = added[bisect.bisect_left(added, (e, 0)):bisect.bisect_left(added, (nxt, 0))]
    if not any(s <= frm < e and frm % 4 == 0 and (word(frm) >> 24) & 0xF == 0xA for _, frm in gap):
        continue  # no plain B from F into the gap
    cut = next((a for a in range(e, nxt, 4) if a in called), nxt)
    if cut > e:
        print("extend 0x%08X-0x%08X -> 0x%08X" % (s, e, cut))
        f[1] = cut
        ext += 1
for x, y in zip(funcs, funcs[1:]):
    assert x[1] <= y[0], (hex(x[0]), hex(y[0]))
open(codetxt, "w", newline="\n").write("\n".join(head + ["0x%08X 0x%08X %s" % tuple(f) for f in funcs]) + "\n")
print("extended %d" % ext)
