//! XR_HOSTPROF=1 or =FROM-TO (refreshes run, not script frames) (dev): every millisecond, samples where the host thread holding the baton (the emulation: guest code,
//! HLE, GPU command recording and its waits) is, and rewrites `<logs>/hostprof.txt` every 5 s: one line per module
//! (base, path, samples) and per address (`module+offset samples`). `tools/hostprof.py` names the functions.
use std::collections::HashMap;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Diagnostics::Debug::{GetThreadContext, CONTEXT, CONTEXT_CONTROL_AMD64};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_SUSPEND_RESUME};

/// refreshes run so far
pub static FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// per baton turn key: the OS thread that waits for it
static THREADS: Mutex<Option<HashMap<u64, usize>>> = Mutex::new(None);

pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XR_HOSTPROF").is_some_and(|v| v != "0"))
}

/// The calling thread is the one that runs turn `key`.
pub fn register(key: u64) {
    let mut t = THREADS.lock().unwrap();
    let map = t.get_or_insert_with(HashMap::new);
    map.entry(key).or_insert_with(|| unsafe { OpenThread(THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME, 0, GetCurrentThreadId()) } as usize);
}

/// Starts the sampler: `turn` says whose turn it is.
pub fn start(logs: std::path::PathBuf, turn: fn() -> u64) {
    let v = std::env::var("XR_HOSTPROF").unwrap_or_default();
    let (from, to) = v.split_once('-').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))).unwrap_or((0, u64::MAX));
    std::thread::spawn(move || {
        let mut hits: HashMap<u64, u64> = HashMap::new();
        let mut last = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1));
            if last.elapsed().as_secs() >= 5 {
                last = std::time::Instant::now();
                let _ = std::fs::write(logs.join("hostprof.txt"), report(&hits));
            }
            let f = FRAME.load(std::sync::atomic::Ordering::Relaxed);
            if f < from || f >= to {
                continue;
            }
            let h = THREADS.lock().unwrap().as_ref().and_then(|m| m.get(&turn()).copied());
            if let Some(h) = h {
                let h = h as HANDLE;
                // CONTEXT must be 16-aligned
                #[repr(align(16))]
                struct Ctx(CONTEXT);
                let mut c: Ctx = unsafe { std::mem::zeroed() };
                c.0.ContextFlags = CONTEXT_CONTROL_AMD64;
                unsafe {
                    // nothing that can allocate while it's suspended: it may hold the heap's lock
                    if SuspendThread(h) != u32::MAX {
                        let got = GetThreadContext(h, &mut c.0) != 0;
                        ResumeThread(h);
                        if got {
                            *hits.entry(c.0.Rip).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
    });
}

fn report(hits: &HashMap<u64, u64>) -> String {
    let mut mods: HashMap<u64, (String, u64)> = HashMap::new();
    let mut rows = Vec::new();
    for (&rip, &n) in hits {
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        unsafe { VirtualQuery(rip as *const _, &mut mbi, size_of::<MEMORY_BASIC_INFORMATION>()) };
        let base = mbi.AllocationBase as u64;
        let m = mods.entry(base).or_insert_with(|| {
            let mut name = [0u16; 260];
            let len = unsafe { GetModuleFileNameW(base as _, name.as_mut_ptr(), 260) } as usize;
            (String::from_utf16_lossy(&name[..len]), 0)
        });
        m.1 += n;
        rows.push((base, rip - base, n));
    }
    let total: u64 = hits.values().sum();
    let mut out = format!("samples {total}\n");
    let mut ms: Vec<_> = mods.iter().collect();
    ms.sort_by_key(|(_, (_, n))| std::cmp::Reverse(*n));
    for (base, (name, n)) in ms {
        out += &format!("module 0x{base:X} {n} {name}\n");
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    for (base, off, n) in rows {
        out += &format!("0x{base:X}+0x{off:X} {n}\n");
    }
    out
}
