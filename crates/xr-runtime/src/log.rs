//! Log lines go to stderr and `<data>/logs/latest.log`.
use std::io::Write;
use std::sync::Mutex;

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn init(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    *FILE.lock().unwrap() = Some(std::fs::File::create(path)?);
    Ok(())
}

pub fn log(msg: &str) {
    eprintln!("{msg}");
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{msg}");
    }
}

/// (frame, last checkpoint) of a script run, appended to a failing final message so a crash says where it was.
pub static WHERE: Mutex<(u64, String)> = Mutex::new((0, String::new()));

/// Log, then end the process (the log file is unbuffered, so nothing is lost).
pub fn stop(code: i32, msg: &str) -> ! {
    let (n, cp) = &*WHERE.lock().unwrap_or_else(|e| e.into_inner());
    log(&if code != 0 && *n > 0 { format!("{msg} [frame {n}, last checkpoint {cp:?}]") } else { msg.into() });
    crate::progress::write(code);
    std::process::exit(code)
}
