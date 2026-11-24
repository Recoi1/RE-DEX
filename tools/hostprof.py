#!/usr/bin/env python3
"""usage: hostprof.py LOGS/hostprof.txt [TOP] [XGAME_DLL] (the DLL to name when the run's copy is gone): names the functions of an XR_HOSTPROF run (llvm-nm and llvm-readobj from
llvm-mingw on PATH) and prints the hottest, with each module's share. Guest functions in xgame.dll are f_/t_ (code.bin)
and mK_f_ (CRO module K, offset)."""
import bisect, collections, re, subprocess, sys

lines = open(sys.argv[1]).read().splitlines()
top = int(sys.argv[2]) if len(sys.argv) > 2 else 40
total = int(lines[0].split()[1])
mods, syms = {}, {}
for l in lines[1:]:
    if l.startswith('module '):
        _, base, n, path = l.split(' ', 3)
        mods[int(base, 16)] = (path, int(n))
print(f'{total} samples')
for base, (path, n) in sorted(mods.items(), key=lambda m: -m[1][1]):
    print(f'{100 * n / total:5.1f}% {path or hex(base)}')


def table(path):
    if path.lower().endswith('xgame.dll') and len(sys.argv) > 3:
        path = sys.argv[3]
    if path not in syms:
        try:
            hdr = subprocess.run(['llvm-readobj', '--file-headers', path], capture_output=True, text=True).stdout
            image = int(re.search(r'ImageBase: 0x([0-9A-Fa-f]+)', hdr).group(1), 16)
            out = subprocess.run(['llvm-nm', '--defined-only', path], capture_output=True, text=True).stdout
            s = sorted((int(a, 16) - image, name) for a, t, name in (x.split(' ', 2) for x in out.splitlines() if x.count(' ') >= 2) if t in 'tT')
            if not s:  # a system DLL: its exports (ntdll's ZwWaitForMultipleObjects is a fence wait)
                ex = subprocess.run(['llvm-readobj', '--coff-exports', path], capture_output=True, text=True).stdout
                s = sorted((int(r, 16), n) for n, r in re.findall(r'Name: (\S+)\s+RVA: 0x([0-9A-Fa-f]+)', ex))
        except Exception:
            s = []
        syms[path] = ([a for a, _ in s], [n for _, n in s])
    return syms[path]


funcs = collections.Counter()
for l in lines[1:]:
    if l.startswith('module ') or '+' not in l:
        continue
    loc, n = l.split()
    base, off = (int(x, 16) for x in loc.split('+'))
    path = mods.get(base, ('?', 0))[0]
    addrs, names = table(path)
    i = bisect.bisect_right(addrs, off) - 1
    funcs[(path.rsplit('\\', 1)[-1], names[i] if i >= 0 else hex(off))] += int(n)
for (m, f), n in funcs.most_common(top):
    print(f'{100 * n / total:5.1f}% {n:7} {m} {f}')
