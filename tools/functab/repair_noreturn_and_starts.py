"""Apply the reviewed code.txt repairs.
1. ARM functions whose listed range ends right after `bl <callee>` for callees Ghidra wrongly took as no-return
   (each checked in rizin: they return) get their end moved to the next listed start, or to the first function the
   last build's repair added in between that is a BL/BLX target (a real function), whichever comes first.
2. Real function starts the interpreter was entered at (checked in rizin): split or added.
usage: python -I apply.py <code.txt> <code.bin> <report.txt>  (rewrites code.txt, prints what changed)"""
import struct, sys, bisect

codetxt, codebin, report = sys.argv[1:4]
RETURNING = {0x00301A9C, 0x0014EB04, 0x0013A3B4}  # memset (bxeq lr / bx lr), two getters (bx lr)
# (start, mode): split the listed function containing it, or add it up to `end` when outside every function
STARTS = {
    0x001339B8: None,  # push {r4,lr}; prev function pops lr and falls through (refcount release), 977k entries
    0x00301AA0: None,  # stmdb sp!,{lr}; memset body, 0x301A9C only sets r2=0 first
    0x0014C248: None,  # nop padding before; setter ldr/str/bx lr
    0x003148A0: None,  # nop padding before; called via blx r2
    0x0042304C: 0x00423054,  # mov r0,0; bx lr, called via blx r1 (virtual)
    0x004233AC: 0x004233B4,  # mov r0,1; bx lr, called via blx r1 (virtual)
    0x0012A1C4: 0x0012A1C8,  # bx lr after a pool word, called via blx r6 (init array)
}
code = open(codebin, "rb").read()
BASE = 0x100000
lines = open(codetxt).read().splitlines()
head = [l for l in lines if not l.startswith("0x")]
funcs = sorted([int(s, 16), int(e, 16), m] for s, e, m in (l.split()[:3] for l in lines if l.startswith("0x")))
added = []
for l in open(report):
    p = l.split()
    if len(p) == 6 and p[2] == "end" and p[4] == "from":
        added.append((int(p[0], 16), int(p[5], 16)))
added.sort()


def word(a):
    return struct.unpack_from("<I", code, a - BASE)[0]


def bl_target(a):
    w = word(a)
    if w >> 24 != 0xEB:
        return None
    off = (w & 0xFFFFFF) << 2
    return (a + 8 + off - (1 << 26 if off & 0x2000000 else 0)) & 0xFFFFFFFF


def is_call(frm):
    if frm & 1 or frm >= BASE + len(code):
        return frm & 1 == 1  # Thumb origin: only BL/BLX make cross-function targets the repair adds; treat as call
    w = word(frm)
    return (w >> 24) & 0xF == 0xB or w >> 25 == 0x7D  # BL (any cond) or BLX imm


starts = [f[0] for f in funcs]
ext = 0
for i, f in enumerate(funcs):
    s, e, m = f
    if m != "arm" or e - s < 8 or bl_target(e - 4) not in RETURNING:
        continue
    nxt = funcs[i + 1][0] if i + 1 < len(funcs) else e
    if nxt <= e:
        continue
    cut = next((a for a, frm in added[bisect.bisect_left(added, (e + 1, 0)):] if a < nxt and is_call(frm)), nxt)
    if cut > e:
        print("extend 0x%08X-0x%08X -> 0x%08X" % (s, e, cut))
        f[1] = cut
        ext += 1
for a, end in STARTS.items():
    j = bisect.bisect_right([f[0] for f in funcs], a) - 1
    f = funcs[j]
    if f[0] < a < f[1]:
        print("split 0x%08X-0x%08X at 0x%08X" % (f[0], f[1], a))
        funcs.insert(j + 1, [a, f[1], f[2]])
        f[1] = a
    elif not (f[0] <= a < f[1]):
        assert end and (j + 1 >= len(funcs) or end <= funcs[j + 1][0]), hex(a)
        print("add 0x%08X-0x%08X arm" % (a, end))
        funcs.insert(j + 1, [a, end, "arm"])
for x, y in zip(funcs, funcs[1:]):
    assert x[1] <= y[0], (hex(x[0]), hex(y[0]))
head = [("# count: %d" % len(funcs)) if h.startswith("# count:") else h for h in head]
open(codetxt, "w", newline="\n").write("\n".join(head + ["0x%08X 0x%08X %s" % tuple(f) for f in funcs]) + "\n")
print("extended %d, now %d functions" % (ext, len(funcs)))
