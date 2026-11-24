//! What the guest asked for during this run. At exit (`log::stop`) it goes to `<logs>/progress-run.json`, and
//! interpreted entry points are merged into `<logs>/misses-proposal.txt` for a person to review (never into code.txt).
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// In order; the run reports the furthest one reached.
const MILESTONES: [&str; 5] = ["none", "entry", "srv", "services", "command_list"];

/// What System::service_calls holds: per service in first-call order, (command id, count).
pub type ServiceCalls = Vec<(String, Vec<(u16, u64)>)>;

struct Run {
    milestone: usize,
    /// interpreter entry points in first-seen order: (start | T, lr at the first entry, entries)
    entries: Vec<(u32, u32, u64)>,
    index: Option<HashMap<u32, usize>>,
    /// (svc number, count, implemented)
    svcs: Vec<(u32, u32, bool)>,
    services: ServiceCalls,
}

static RUN: Mutex<Run> = Mutex::new(Run { milestone: 0, entries: Vec::new(), index: None, svcs: Vec::new(), services: Vec::new() });
static DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn init(logs: &Path) {
    let _ = DIR.set(logs.to_path_buf());
}

pub fn milestone(m: &str) {
    let i = MILESTONES.iter().position(|&x| x == m).expect("known milestone");
    let mut r = RUN.lock().unwrap();
    r.milestone = r.milestone.max(i);
}

pub fn svc(num: u32, implemented: bool) {
    let mut r = RUN.lock().unwrap();
    match r.svcs.iter_mut().find(|s| s.0 == num) {
        Some(s) => (s.1, s.2) = (s.1 + 1, s.2 && implemented),
        None => r.svcs.push((num, 1, implemented)),
    }
}

/// Copied from System between frames (the fault paths stop mid-frame, without the baton).
pub fn services(calls: &ServiceCalls) {
    RUN.lock().unwrap().services.clone_from(calls);
}

/// One interpreter entry at `start` (odd = Thumb); returns true the first time this start is seen.
pub fn entry(start: u32, lr: u32) -> bool {
    let mut r = RUN.lock().unwrap();
    let r = &mut *r;
    let index = r.index.get_or_insert_with(HashMap::new);
    match index.get(&start) {
        Some(&i) => {
            r.entries[i].2 += 1;
            false
        }
        None => {
            index.insert(start, r.entries.len());
            r.entries.push((start, lr, 1));
            true
        }
    }
}

/// Distinct interpreted entry points so far.
pub fn entry_count() -> usize {
    RUN.lock().unwrap().entries.len()
}

/// The T bit of the entry says the mode.
fn mode(start: u32) -> &'static str {
    if start & 1 != 0 { "thumb" } else { "arm" }
}

pub fn write(exit_code: i32) {
    let (Some(dir), Ok(r)) = (DIR.get(), RUN.lock()) else { return };
    let mut svcs = r.svcs.clone();
    svcs.sort();
    let json = serde_json::json!({
        "exit_code": exit_code,
        "milestone_reached": MILESTONES[r.milestone],
        "interpreted_entries": r.entries.iter().map(|&(a, lr, n)| serde_json::json!({"addr": format!("0x{:08X}", a & !1), "count": n, "mode": mode(a), "first_lr": format!("0x{lr:08X}")})).collect::<Vec<_>>(),
        "svcs": svcs.iter().map(|&(n, count, implemented)| serde_json::json!({"num": format!("0x{n:02X}"), "name": crate::bridge::name(n), "count": count, "implemented": implemented})).collect::<Vec<_>>(),
        "services": r.services.iter().map(|(name, cmds)| serde_json::json!({"name": name, "commands": cmds.iter().map(|&(id, n)| serde_json::json!({"id": format!("0x{id:04X}"), "count": n})).collect::<Vec<_>>()})).collect::<Vec<_>>(),
    });
    let _ = std::fs::write(dir.join("progress-run.json"), serde_json::to_string_pretty(&json).unwrap());

    // Merge this run's entries into the proposal, one line per address, counting entries across runs.
    let path = dir.join("misses-proposal.txt");
    let mut lines: Vec<String> = std::fs::read_to_string(&path).unwrap_or_default().lines().map(String::from).collect();
    for &(addr, lr, n) in &r.entries {
        let key = format!("0x{:08X} ", addr & !1);
        let hits = match lines.iter().position(|l| l.starts_with(&key)) {
            Some(i) => lines.remove(i).split("# ").nth(1).and_then(|t| t.split(' ').next()?.parse::<u64>().ok()).unwrap_or(0),
            None => 0,
        };
        lines.push(format!("{key}{}  # {} hits, lr 0x{lr:08X}", mode(addr), hits + n));
    }
    if !r.entries.is_empty() {
        let _ = std::fs::write(&path, lines.join("\n") + "\n");
    }
}
