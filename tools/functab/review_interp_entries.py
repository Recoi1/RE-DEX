"""List interpreted entries (progress-run.json) with code.txt context and rizin disassembly for review.
usage: python -I entries.py <progress-run.json> <code.txt> <code.bin> [min_count]"""
import json, subprocess, sys, bisect

prog, codetxt, codebin = sys.argv[1:4]
minc = int(sys.argv[4]) if len(sys.argv) > 4 else 0
RZ = r"D:\Roms-Recomps\PokemonX\tools\rizin\bin\rizin.exe"
funcs = []
for l in open(codetxt):
    if l.startswith("0x"):
        s, e, m = l.split()[:3]
        funcs.append((int(s, 16), int(e, 16), m))
funcs.sort()
starts = [f[0] for f in funcs]
entries = json.load(open(prog))["interpreted_entries"]
entries.sort(key=lambda e: -e["count"])


def rz(cmds):
    args = [RZ, "-a", "arm", "-b", "32", "-m", "0x100000", "-q", "-e", "asm.bytes=false", "-e", "scr.color=0",
            "-e", "asm.comments=false"]
    for c in cmds:
        args += ["-c", c]
    return subprocess.run(args + [codebin], capture_output=True, text=True).stdout


for e in entries:
    if e["count"] < minc:
        continue
    a = int(e["addr"], 16)
    j = bisect.bisect_right(starts, a)
    inside = funcs[j - 1] if j > 0 and funcs[j - 1][0] <= a < funcs[j - 1][1] else None
    nxt = funcs[j][0] if j < len(funcs) else 0
    bits = 16 if e["mode"] == "thumb" else 32
    w = 2 if bits == 16 else 4
    lr = int(e["first_lr"], 16)
    where = "inside 0x%08X-0x%08X %s" % inside if inside else "outside (next listed 0x%08X)" % nxt
    print("%s %-5s count %d lr %s %s" % (e["addr"], e["mode"], e["count"], e["first_lr"], where))
    body = rz(["e asm.bits=%d" % bits, "pd 1 @ 0x%x" % (a - w), "pd 3 @ 0x%x" % a])
    caller = rz(["e asm.bits=%d" % (16 if lr & 1 else 32), "pd 1 @ 0x%x" % ((lr & ~1) - (2 if lr & 1 else 4))])
    for line in body.splitlines():
        print("    " + line.strip())
    print("    caller: " + caller.strip())
