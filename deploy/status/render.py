#!/usr/bin/env python3
"""Renders the public RE:DEX progress card (progress.svg, about 800x200) and status page (index.html) from progress.json.

    python3 render.py progress.json OUT_DIR

Caddy serves OUT_DIR at https://STATUS_HOST/status/ (and at / on a status host of its own) and the card at
https://STATUS_HOST/progress.svg; the page links the card relatively, so both places work.

The JSON is public: it may hold only `updated`, `release`, `milestones` (0-9: name + status) and `features` (3-5).
Anything else is refused, so nothing private can slip onto the page.
"""
import html
import json
import os
import re
import sys

STATUSES = ("done", "in progress", "next", "planned")
# fill, text on it
COLORS = {"done": ("#2ea043", "#ffffff"), "in progress": ("#d29922", "#1b1300"), "next": ("#388bfd", "#ffffff"), "planned": ("#30363d", "#9da5ae")}


def check(d):
    def text(v, what):
        assert isinstance(v, str) and 0 < len(v) <= 80, f"{what}: a string of 1-80 characters"
        return v

    assert isinstance(d, dict) and set(d) == {"updated", "release", "milestones", "features"}, "keys: updated, release, milestones, features"
    assert re.fullmatch(r"\d{4}-\d{2}-\d{2}", text(d["updated"], "updated")), "updated: YYYY-MM-DD"
    r = d["release"]
    assert isinstance(r, dict) and set(r) == {"current", "next", "next_status"}, "release keys: current, next, next_status"
    for k in r:
        text(r[k], f"release.{k}")
    m = d["milestones"]
    assert isinstance(m, list) and len(m) == 10, "milestones: exactly 10 (0-9)"
    for i, x in enumerate(m):
        assert isinstance(x, dict) and set(x) == {"name", "status"}, f"milestone {i}: name and status only"
        text(x["name"], f"milestone {i} name")
        assert x["status"] in STATUSES, f"milestone {i} status: one of {STATUSES}"
    f = d["features"]
    assert isinstance(f, list) and 3 <= len(f) <= 5, "features: 3-5"
    for i, x in enumerate(f):
        text(x, f"feature {i}")
    return d


def svg(d):
    e = html.escape
    m = d["milestones"]
    r = d["release"]
    font = "font-family=\"'Segoe UI',Helvetica,Arial,sans-serif\""
    out = [
        '<svg xmlns="http://www.w3.org/2000/svg" width="800" height="200" viewBox="0 0 800 200" role="img" aria-labelledby="t">',
        f'<title id="t">RE:DEX progress: {sum(x["status"] == "done" for x in m)} of 10 milestones done</title>',
        '<rect x="0.5" y="0.5" width="799" height="199" rx="12" fill="#14171c" stroke="#2b3038"/>',
        f'<text x="24" y="42" {font} font-size="22" font-weight="700" fill="#f0f3f6">RE:DEX <tspan font-weight="400" fill="#9da5ae">progress</tspan></text>',
        f'<text x="776" y="42" text-anchor="end" {font} font-size="14" fill="#c9d1d9">{e(r["current"])} released · {e(r["next"])} {e(r["next_status"])}</text>',
    ]
    gap, x0, w_all = 6, 24, 752
    w = (w_all - 9 * gap) / 10
    for i, x in enumerate(m):
        fill, ink = COLORS[x["status"]]
        x1 = x0 + i * (w + gap)
        out.append(f'<rect x="{x1:.1f}" y="62" width="{w:.1f}" height="30" rx="6" fill="{fill}"><title>{i}: {e(x["name"])} ({x["status"]})</title></rect>')
        out.append(f'<text x="{x1 + w / 2:.1f}" y="82" text-anchor="middle" {font} font-size="14" font-weight="700" fill="{ink}">{i}</text>')
    lx = 24
    for s in STATUSES:
        out.append(f'<rect x="{lx}" y="104" width="10" height="10" rx="2" fill="{COLORS[s][0]}"/><text x="{lx + 15}" y="113" {font} font-size="12" fill="#9da5ae">{s}</text>')
        lx += 22 + 7 * len(s) + 18
    done = [i for i, x in enumerate(m) if x["status"] == "done"]
    lines = []
    if done:
        lines.append(("Latest", done[-1]))
    lines += [("Now", i) for i, x in enumerate(m) if x["status"] == "in progress"]
    lines += [("Next", i) for i, x in enumerate(m) if x["status"] == "next"]
    for k, (label, i) in enumerate(lines[:2]):
        out.append(f'<text x="24" y="{146 + k * 26}" {font} font-size="15" fill="#f0f3f6"><tspan fill="#9da5ae">{label}:</tspan> {i} · {e(m[i]["name"])}</text>')
    out.append(f'<text x="776" y="184" text-anchor="end" {font} font-size="12" fill="#7d8590">updated {e(d["updated"])}</text>')
    out.append("</svg>\n")
    return "\n".join(out)


def page(d):
    e = html.escape
    r = d["release"]
    rows = "\n".join(f'<li class="{x["status"].replace(" ", "-")}"><span class="n">{i}</span><span class="name">{e(x["name"])}</span><span class="s">{x["status"]}</span></li>' for i, x in enumerate(d["milestones"]))
    feats = "\n".join(f"<li>{e(f)}</li>" for f in d["features"])
    return f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>RE:DEX progress</title>
<style>
:root{{--bg:#f6f8fa;--card:#fff;--ink:#1f2328;--dim:#59636e;--line:#d1d9e0;--done:#1a7f37;--prog:#9a6700;--next:#0969da;--plan:#818b98}}
@media (prefers-color-scheme:dark){{:root{{--bg:#0d1117;--card:#14171c;--ink:#f0f3f6;--dim:#9da5ae;--line:#2b3038;--done:#3fb950;--prog:#d29922;--next:#58a6ff;--plan:#7d8590}}}}
body{{margin:0;background:var(--bg);color:var(--ink);font:16px/1.5 'Segoe UI',Helvetica,Arial,sans-serif}}
main{{max-width:820px;margin:0 auto;padding:24px 16px}}
h1{{margin:0 0 4px;font-size:28px}} h2{{font-size:18px;margin:28px 0 8px}} p{{margin:0;color:var(--dim)}}
img{{display:block;width:100%;height:auto;margin:20px 0;border-radius:12px}}
ul{{list-style:none;padding:0;margin:0;background:var(--card);border:1px solid var(--line);border-radius:12px}}
li{{display:flex;gap:12px;align-items:baseline;padding:10px 14px;border-top:1px solid var(--line)}} li:first-child{{border-top:0}}
.n{{font-weight:700;width:1.2em;color:var(--dim)}} .name{{flex:1}} .s{{font-size:13px;font-weight:600}}
.done .s{{color:var(--done)}} .in-progress .s{{color:var(--prog)}} .next .s{{color:var(--next)}} .planned .s{{color:var(--plan)}}
.f li{{display:list-item;list-style:disc inside}}
</style></head>
<body><main>
<h1>RE:DEX progress</h1>
<p>{e(r["current"])} released · {e(r["next"])} {e(r["next_status"])} · updated {e(d["updated"])}</p>
<img src="progress.svg" alt="Milestone progress card" width="800" height="200">
<h2>Milestones</h2>
<ul>
{rows}
</ul>
<h2>Highlights</h2>
<ul class="f">
{feats}
</ul>
</main></body></html>
"""


def write(path, text):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(text)
    os.chmod(tmp, 0o644)
    os.replace(tmp, path)  # Caddy never serves a half-written file


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit("usage: render.py progress.json OUT_DIR")
    with open(sys.argv[1], encoding="utf-8") as f:
        data = check(json.load(f))
    os.makedirs(sys.argv[2], exist_ok=True)
    write(os.path.join(sys.argv[2], "progress.svg"), svg(data))
    write(os.path.join(sys.argv[2], "index.html"), page(data))
    print(f"rendered {sys.argv[2]}/progress.svg and index.html ({data['updated']})")
