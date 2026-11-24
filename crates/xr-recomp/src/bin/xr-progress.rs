//! `xr-progress --build <progress-build.json> --run <progress-run.json> --out <dir>`: writes `<dir>/progress.json`
//! and `<dir>/badge.svg` (`recomp | 99.99% · services`). The percentage is the share of generated functions that
//! decode completely; the milestone is the furthest point the last run reached.
use serde_json::{json, Value};
use std::path::PathBuf;

fn read(p: &PathBuf) -> Result<Value, String> {
    let s = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))
}

/// A flat shields.io-style badge; text width is estimated at 7 px per character (Verdana 11).
fn badge(label: &str, value: &str) -> String {
    let w = |s: &str| 10 + 7 * s.chars().count();
    let (lw, vw) = (w(label), w(value));
    let t = lw + vw;
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{t}" height="20" role="img" aria-label="{label}: {value}"><title>{label}: {value}</title><linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient><clipPath id="r"><rect width="{t}" height="20" rx="3" fill="#fff"/></clipPath><g clip-path="url(#r)"><rect width="{lw}" height="20" fill="#555"/><rect x="{lw}" width="{vw}" height="20" fill="#007ec6"/><rect width="{t}" height="20" fill="url(#s)"/></g><g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11"><text x="{}" y="14">{label}</text><text x="{}" y="14">{value}</text></g></svg>
"##,
        lw / 2,
        lw + vw / 2
    )
}

fn run() -> Result<(), String> {
    let (mut build, mut runf, mut out) = (None, None, None);
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        let slot = match a.to_str() {
            Some("--build") => &mut build,
            Some("--run") => &mut runf,
            Some("--out") => &mut out,
            _ => return Err(format!("unknown argument {a:?}")),
        };
        *slot = Some(PathBuf::from(args.next().ok_or(format!("{a:?} needs a value"))?));
    }
    let (build, runf, out) = (build.ok_or("missing --build")?, runf.ok_or("missing --run")?, out.ok_or("missing --out")?);
    let (b, r) = (read(&build)?, read(&runf)?);
    let generated = b["functions_generated"].as_u64().ok_or("progress-build.json: no functions_generated")?;
    let undecodable = b["undecodable"].as_array().map_or(0, |a| a.len()) as u64;
    let pct = if generated == 0 { 0.0 } else { 100.0 * (generated - undecodable) as f64 / generated as f64 };
    let milestone = r["milestone_reached"].as_str().unwrap_or("none");
    let progress = json!({
        "build": b["build"],
        "recomp_percent": (pct * 100.0).round() / 100.0,
        "functions_generated": generated,
        "undecodable": undecodable,
        "milestone_reached": milestone,
        "exit_code": r["exit_code"],
        "interpreted_entries": r["interpreted_entries"],
        "svcs": r["svcs"],
        "services": r["services"],
    });
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let write = |name: &str, s: String| std::fs::write(out.join(name), s).map_err(|e| format!("{name}: {e}"));
    write("progress.json", serde_json::to_string_pretty(&progress).unwrap() + "\n")?;
    write("badge.svg", badge("recomp", &format!("{pct:.2}% · {milestone}")))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("xr-progress: {e}");
        std::process::exit(1);
    }
}
