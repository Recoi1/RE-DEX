"""Seeds for SeedFunctions.java: code addresses that ro/data point at (vtables, callback tables) but that no listed
function covers. A word counts when it points into text (ARM: 4-aligned, Thumb: odd) and a neighbouring word is
also such a pointer, which keeps lone integers that happen to look like addresses out. Also the static
initialiser table (armlink init_array) that ends ro: words holding offsets relative to their own address.

usage: python -I vtable_seeds.py <dump dir> <code.txt> <out seeds.txt>
"""
import bisect
import struct
import sys

dump, functab, out = sys.argv[1:4]
ex = open(f"{dump}/exheader.bin", "rb").read()
code = open(f"{dump}/exefs/code.bin", "rb").read()
u32 = lambda b, o: struct.unpack_from("<I", b, o)[0]
text, ro, data = [(u32(ex, o), u32(ex, o + 8)) for o in (0x10, 0x20, 0x30)]
base, tend = text[0], text[0] + text[1]

funcs = sorted((int(l.split()[0], 16), int(l.split()[1], 16), l.split()[2]) for l in open(functab) if l.startswith("0x"))
starts = [f[0] for f in funcs]


def listed(a):
    i = bisect.bisect_right(starts, a) - 1
    return i >= 0 and funcs[i][0] <= a < funcs[i][1]


def arm_gap(a):
    """True when `a` is in a gap whose neighbouring listed functions are both ARM: a Thumb seed there is a stray
    pointer into ARM code (2 of 9 such seeds split real functions)."""
    i = bisect.bisect_right(starts, a)
    return 0 < i < len(funcs) and funcs[i - 1][2] == "arm" and funcs[i][2] == "arm"


def codeptr(w):
    return base <= (w & ~1) < tend and (w & 1 or w & 3 == 0)


seeds = set()
for start, size in (ro, data):
    for o in range(start - base + 4, start - base + size - 4, 4):
        w = u32(code, o)
        if codeptr(w) and not listed(w & ~1) and not (w & 1 and arm_gap(w & ~1)) and (codeptr(u32(code, o - 4)) or codeptr(u32(code, o + 4))):
            seeds.add(w)
# init_array: walk back from the end of ro while each word, added to its own address, lands in text.
o = ro[0] + ro[1] - base - 4
while o >= ro[0] - base and codeptr((base + o + u32(code, o)) & 0xFFFFFFFF):
    w = (base + o + u32(code, o)) & 0xFFFFFFFF
    if not listed(w & ~1) and not (w & 1 and arm_gap(w & ~1)):
        seeds.add(w)
    o -= 4
with open(out, "w") as f:
    for w in sorted(seeds):
        f.write("0x%08X %s\n" % (w & ~1, "thumb" if w & 1 else "arm"))
print(f"{len(seeds)} seeds ({sum(w & 1 for w in seeds)} thumb)")
