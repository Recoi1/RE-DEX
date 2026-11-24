// Stands in for xr-recomp and redex; behaviour follows the exe's own name, `mode.txt` beside it
// ("slow": stall after step 2; "big": print only "[12/400]"), and `done.txt` in --out (resume).
use std::{fs, path::PathBuf, time::Duration};

fn main() {
    let exe = std::env::current_exe().unwrap();
    if std::env::args().nth(1).as_deref() == Some("--sleep") {
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    let dir = exe.parent().unwrap();
    let name = exe.file_stem().unwrap().to_string_lossy().to_string();
    let args: Vec<String> = std::env::args().skip(1).collect();
    fs::write(dir.join(format!("{name}-args.txt")), args.join("\n")).unwrap();
    let mode = fs::read_to_string(dir.join("mode.txt")).unwrap_or_default();
    if name != "xr-recomp" {
        return;
    }
    let out = PathBuf::from(&args[args.iter().position(|a| a == "--out").unwrap() + 1]);
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("child.pid"), std::process::id().to_string()).unwrap();
    if mode.trim() != "big" {
        println!("xr-recomp: 1 code.bin functions + 0 in 0 CRO modules (0 patch sites checked, 0 thunk flag demands), 2 files, generated in 0.0 s; running ninja");
    }
    if mode.trim() == "grand" {
        let g = std::process::Command::new(&exe).arg("--sleep").spawn().unwrap();
        fs::write(out.join("grand.pid"), g.id().to_string()).unwrap();
        println!("[1/4] CC gen_1.c");
        std::thread::sleep(Duration::from_secs(60));
    }
    if mode.trim() == "big" {
        println!("[12/400] CC gen_12.c");
    } else {
        let done: u32 = fs::read_to_string(out.join("done.txt")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        for i in done + 1..=4 {
            fs::write(out.join("done.txt"), i.to_string()).unwrap();
            println!("[{i}/4] CC gen_{i}.c");
            if mode.trim() == "slow" && i == 2 {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
    fs::write(out.join("xgame.dll"), b"x").unwrap();
}
