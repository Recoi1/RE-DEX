//! Two real game instances meet through a `redex-world` server (the multiplayer M1 proof). Each test
//! starts the server binary (`--dev`, loopback) and two headless games held to real time (`--realtime`), each in a data
//! dir of its own with its own dev id, and scripts them with `wait_until` (scripts/playthrough/mp-pair-*.txt): the
//! scripts wait on each other, never on frame numbers. Long (minutes each) and they need saves, so they are ignored by
//! default. Run them one at a time (at most two games at once):
//!
//! ```text
//! cargo build --release -p redex-world --features sim     # --sim for the jitter test
//! XR_DATA=<a data dir whose build/x-usa-v0.2.0/xgame.dll has the puppet hooks>
//! cargo test --release -p xr-runtime --test two_instances -- --ignored --test-threads 1 --nocapture
//! ```
//!
//! XR_CCI is the game; XR_SAVE_100PCT the 100% save (default `<XR_DATA>/../saves/x-100pct/main`) and XR_SAVE_TRAINER2 a
//! second trainer's `user` dir (default `<XR_DATA>/../data-p3t7/save-shauna/user`, the new game saved in Vaniville). Both
//! are only ever copied. XR_MP_OUT keeps the data dirs (screens, logs) there instead of the temp dir.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SCRIPTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/playthrough");
const TILE: f32 = 18.0;

enum Save {
    Full,
    Trainer2,
}

struct Pair {
    name: &'static str,
    a: &'static str,
    b: &'static str,
    b_save: Save,
    /// the server's `--sim MIN,MAX,LOSS`
    sim: Option<&'static str>,
    /// kill the server once both have met, start it again 3 s later
    restart: bool,
}

/// What a pair run saw: both logs, and when things happened.
struct Seen {
    a: PathBuf,
    a_log: String,
    b_log: String,
    /// B's map changed (its `wait_until map!=` held) -> A's puppet went ("): gone")
    leave: Option<Duration>,
    /// the server was killed -> each side's puppet went
    lost: [Option<Duration>; 2],
}

fn setup() -> Option<(String, PathBuf, PathBuf, PathBuf)> {
    let (Ok(data), Ok(cci)) = (std::env::var("XR_DATA"), std::env::var("XR_CCI")) else {
        println!("skipping: XR_DATA or XR_CCI not set");
        return None;
    };
    let data = PathBuf::from(data);
    let full = std::env::var("XR_SAVE_100PCT").map(PathBuf::from).unwrap_or_else(|_| data.join("../saves/x-100pct/main"));
    let trainer2 = std::env::var("XR_SAVE_TRAINER2").map(PathBuf::from).unwrap_or_else(|_| data.join("../data-p3t7/save-shauna/user"));
    for p in [&full, &trainer2] {
        assert!(p.exists(), "no save at {}", p.display());
    }
    Some((cci, data, full, trainer2))
}

/// A fresh data dir: the hooked DLL and a writable copy of the save.
fn data_dir(dir: &Path, dll_from: &Path, save: &Save, full: &Path, trainer2: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    let build = dir.join("build/x-usa-v0.2.0");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::copy(dll_from.join("build/x-usa-v0.2.0/xgame.dll"), build.join("xgame.dll")).unwrap();
    match save {
        Save::Full => {
            let sd = dir.join("user/savedata/0004000000055D00");
            std::fs::create_dir_all(&sd).unwrap();
            std::fs::copy(full, sd.join("main")).unwrap();
            // the archive and extdata format records X writes on its first boot (as exec.rs's with_100pct_save)
            std::fs::write(dir.join("user/savedata/0004000000055D00.format"), [0, 0, 4, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]).unwrap();
            for d in ["btvideo", "hgmail", "pss_fr", "reg", "trhouse"] {
                std::fs::create_dir_all(dir.join("user/extdata/000000000000055D").join(d)).unwrap();
            }
            std::fs::write(dir.join("user/extdata/000000000000055D.format"), [0, 0, 0, 0, 0xF, 0, 0, 0, 0x31, 3, 0, 0, 0, 0, 0, 0]).unwrap();
        }
        Save::Trainer2 => copy_tree(trainer2, &dir.join("user")),
    }
    writable(&dir.join("user"));
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().map(Result::unwrap) {
        let t = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &t);
        } else {
            std::fs::copy(e.path(), t).unwrap();
        }
    }
}

/// A read-only save fails to open and X boots as a new game.
fn writable(p: &Path) {
    let m = std::fs::metadata(p).unwrap();
    if m.is_dir() {
        for e in std::fs::read_dir(p).unwrap().map(Result::unwrap) {
            writable(&e.path());
        }
    } else {
        let mut perm = m.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(p, perm).unwrap();
    }
}

fn server(port: u16, sim: Option<&str>) -> Child {
    server_in(port, sim, None)
}

/// `data`: the server's `--data` dir (where reports.jsonl goes).
fn server_in(port: u16, sim: Option<&str>, data: Option<&Path>) -> Child {
    let exe = Path::new(env!("CARGO_BIN_EXE_redex")).with_file_name("redex-world.exe");
    assert!(exe.exists(), "{} is missing: cargo build --release -p redex-world --features sim", exe.display());
    let mut c = Command::new(&exe);
    c.args(["--dev", "--bind", &format!("127.0.0.1:{port}")]).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(s) = sim {
        c.args(["--sim", s]);
    }
    if let Some(d) = data {
        c.arg("--data").arg(d);
    }
    let mut child = c.spawn().unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        if let Some(s) = child.try_wait().unwrap() {
            panic!("redex-world exited ({s}); --sim needs a build with --features sim");
        }
        assert!(Instant::now() < end, "redex-world did not listen on {port}");
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

fn game(cci: &str, dir: &Path, script: &str, extra: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_redex"));
    c.arg("--cci").arg(cci).arg("--build").arg(dir.join("build/x-usa-v0.2.0"));
    c.args(["--headless", "--renderer", "vulkan", "--script", &format!("{SCRIPTS}/{script}")]).args(extra);
    c.env("XR_FRAMETIMES", "1").stdout(Stdio::null()).stderr(Stdio::null());
    c
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("logs/latest.log")).unwrap_or_default()
}

fn count(log: &str, what: &str) -> usize {
    log.lines().filter(|l| l.contains(what)).count()
}

/// Runs A and B against a fresh server until both end; then checks what every pair run must hold.
fn pair(p: &Pair) -> Option<Seen> {
    let (cci, data, full, trainer2) = setup()?;
    let out = std::env::var("XR_MP_OUT").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("redex_pair")).join(p.name);
    let (a, b) = (out.join("a"), out.join("b"));
    data_dir(&a, &data, &Save::Full, &full, &trainer2);
    data_dir(&b, &data, &p.b_save, &full, &trainer2);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut srv = Some(server(port, p.sim));
    let world = format!("127.0.0.1:{port}");
    let online = |name: &str| ["--realtime", "--world", world.as_str(), "--dev-id", "--name", name].map(String::from);
    let mut ga = game(&cci, &a, p.a, &[]).args(online("PlayerA")).env("XR_PUPPET_SAVECHECK", "1").spawn().unwrap();
    let mut gb = game(&cci, &b, p.b, &[]).args(online("PlayerB")).spawn().unwrap();
    let mut seen = Seen { a: a.clone(), a_log: String::new(), b_log: String::new(), leave: None, lost: [None; 2] };
    let (mut left, mut gone_before, mut killed, mut lost_before) = (None::<Instant>, 0, None::<Instant>, [0usize; 2]);
    let (mut ca, mut cb) = (None, None);
    let end = Instant::now() + Duration::from_secs(25 * 60);
    while ca.is_none() || cb.is_none() {
        if Instant::now() > end {
            let _ = (ga.kill(), gb.kill());
            if let Some(s) = srv.as_mut() {
                let _ = s.kill();
            }
            panic!("the pair ran over 25 minutes\nA:\n{}\nB:\n{}", tail(&log(&a)), tail(&log(&b)));
        }
        ca = ca.or(ga.try_wait().unwrap().map(|s| s.code()));
        cb = cb.or(gb.try_wait().unwrap().map(|s| s.code()));
        let (la, lb) = (log(&a), log(&b));
        let (gone_a, gone_b) = (count(&la, "): gone"), count(&lb, "): gone"));
        if left.is_none() && lb.contains("wait_until map") {
            (left, gone_before) = (Some(Instant::now()), gone_a);
        }
        if let (Some(t), None) = (left, seen.leave) {
            if gone_a > gone_before {
                seen.leave = Some(t.elapsed());
            }
        }
        if p.restart {
            if killed.is_none() && la.contains("checkpoint met") && lb.contains("checkpoint met") {
                if let Some(mut s) = srv.take() {
                    let _ = s.kill();
                }
                (killed, lost_before) = (Some(Instant::now()), [gone_a, gone_b]);
                println!("{}: server killed", p.name);
            }
            if let Some(k) = killed {
                for (i, g) in [gone_a, gone_b].into_iter().enumerate() {
                    if seen.lost[i].is_none() && g > lost_before[i] {
                        seen.lost[i] = Some(k.elapsed());
                    }
                }
                if srv.is_none() && k.elapsed() > Duration::from_secs(3) {
                    srv = Some(server(port, p.sim));
                    println!("{}: server started again", p.name);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    if let Some(mut s) = srv.take() {
        let _ = s.kill();
    }
    (seen.a_log, seen.b_log) = (log(&a), log(&b));
    // the reload run starts a new latest.log in A's dir: keep this run's as pair.log
    for d in [&a, &b] {
        let _ = std::fs::copy(d.join("logs/latest.log"), d.join("logs/pair.log"));
    }
    for (who, code, l, dir) in [("A", ca, &seen.a_log, &a), ("B", cb, &seen.b_log, &b)] {
        assert_eq!(code, Some(Some(0)), "{who} exited {code:?}\n{}", tail(l));
        for bad in ["panic", "guest memory fault", "WARNING: puppets", "spawn of session"] {
            assert!(!l.contains(bad), "{who}'s log has {bad:?}\n{}", tail(l));
        }
        // no stall: the slowest frame (host time, the pacing sleep not counted)
        let ft = std::fs::read_to_string(dir.join("logs/frametimes.csv")).unwrap();
        let worst = ft.lines().skip(1).filter_map(|r| r.split(',').nth(1)?.parse::<u32>().ok()).max().unwrap_or(0);
        println!("{}: {who}'s slowest frame {} ms", p.name, worst / 1000);
        assert!(worst < 500_000, "{who} stalled: a frame took {} ms", worst / 1000);
    }
    Some(seen)
}

fn tail(l: &str) -> String {
    l.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// The puppet's places at the watch checkpoints, as the game has it and as xr-net put it ("puppets watchNN: 1 shown
/// (1 known): 0x8000 model 2 unsaved at X,Y,Z (set X,Y,Z)").
fn watched(a_log: &str) -> Vec<([f32; 3], [f32; 3])> {
    let xyz = |s: &str| {
        let v: Vec<f32> = s.split(',').filter_map(|v| v.trim().trim_end_matches(')').parse().ok()).collect();
        (v.len() == 3).then(|| [v[0], v[1], v[2]])
    };
    a_log
        .lines()
        .filter(|l| l.starts_with("puppets watch"))
        .filter_map(|l| {
            let (game, set) = l.split(" at ").nth(1)?.split_once(" (set ")?;
            Some((xyz(game)?, xyz(set)?))
        })
        .collect()
}

/// (slots used, puppets) at a checkpoint: the field line and the puppets line.
fn slots(log: &str, cp: &str) -> (usize, usize) {
    let used = log
        .lines()
        .find(|l| l.starts_with(&format!("field {cp}: ")))
        .and_then(|l| l.rsplit(", ").next()?.split('/').next()?.parse().ok())
        .unwrap_or_else(|| panic!("no field line at {cp}"));
    let puppets = log.lines().find(|l| l.starts_with(&format!("puppets {cp}: "))).and_then(|l| l.split(' ').nth(2)?.parse().ok()).unwrap_or(0);
    (used, puppets)
}

/// B walked and A's puppet of B moved with it; B left and the puppet went within 2 s; A saved with the puppet in the
/// room, the move-model save equals the one without it, and the save reloads (offline) with the zone's NPCs only.
fn meet_walk_leave_save(seen: &Seen, min_span: f32) {
    let both = watched(&seen.a_log);
    // the game moves a model by itself while a walk motion plays: the puppet must stay where xr-net put it (at most a
    // walking tick off), or it overshoots and snaps back (a rubber band)
    for (game, set) in &both {
        let off = (game[0] - set[0]).abs().max((game[2] - set[2]).abs());
        assert!(off <= TILE / 7.0 + 0.01, "the game has the puppet at {game:?}, xr-net put it at {set:?}");
    }
    let w: Vec<[f32; 3]> = both.iter().map(|b| b.0).collect();
    println!("puppet places at A's watch checkpoints: {w:?}");
    let mut places = w.clone();
    places.dedup();
    let span = |i: usize| w.iter().map(|p| p[i]).fold(f32::MIN, f32::max) - w.iter().map(|p| p[i]).fold(f32::MAX, f32::min);
    assert!(places.len() >= 3 && span(0).max(span(2)) >= min_span, "the puppet did not follow B's walk: {w:?}");
    let leave = seen.leave.expect("B never left the map, or A's puppet never went");
    println!("B left the map; A's puppet went {leave:?} later");
    assert!(leave < Duration::from_secs(2), "the puppet went {leave:?} after B left");
    let check = seen.a_log.lines().find(|l| l.contains("save check:")).expect("A saved with no puppet in the room");
    assert!(check.contains(" equals "), "{check}");
    assert!(seen.a_log.contains("checkpoint again"), "the puppet did not come back after the save");
    // the zone's NPCs and the player, as A saw them before the save
    let (used, puppets) = slots(&seen.a_log, "landed");
    let alone = used - puppets;
    // reload offline: Continue in the same data dir
    let cci = std::env::var("XR_CCI").unwrap();
    let o = game(&cci, &seen.a, "mp-pair-a-reload.txt", &[]).env("XR_FIELD_SLOTS", "1").env_remove("XR_FRAMETIMES").status().unwrap();
    let l = log(&seen.a);
    assert_eq!(o.code(), Some(0), "{}", tail(&l));
    for cp in ["continued", "settled"] {
        let (u, p) = slots(&l, cp);
        println!("reload {cp}: {u} slots used (before the puppet came: {alone})");
        assert_eq!((u, p), (alone, 0), "the reload at {cp} has {u} slots ({p} puppets); the map alone had {alone}\n{}", tail(&l));
    }
    let stray = l.lines().filter(|x| x.starts_with("slot ") && x.contains(" id 0x8")).collect::<Vec<_>>();
    assert!(stray.is_empty(), "puppet ids in the reloaded pool: {stray:?}");
}

/// The puppet's motion over the run ("motion NAME: ..."), the last line.
fn motion(a_log: &str) -> String {
    a_log.lines().filter(|l| l.starts_with("motion ")).last().unwrap_or("no motion line").to_string()
}

/// No jump (a snap over two tiles) and no step against the one before (a rubber band), as long as B walked.
fn smooth(a_log: &str) {
    let m = motion(a_log);
    println!("{m}");
    let num = |key: &str| m.split(", ").find(|p| p.ends_with(key)).and_then(|p| p.split(' ').next()?.parse::<u32>().ok());
    assert_eq!((num(" jumps"), num(" reversals faster than walking")), (Some(0), Some(0)), "{m}");
}

#[test]
#[ignore = "two games for minutes; run with --ignored (see the file's doc)"]
fn vaniville() {
    let Some(seen) = pair(&Pair { name: "vaniville", a: "mp-pair-a-vaniville.txt", b: "mp-pair-b-vaniville.txt", b_save: Save::Trainer2, sim: None, restart: false }) else { return };
    meet_walk_leave_save(&seen, 2.0 * TILE);
    smooth(&seen.a_log);
}

#[test]
#[ignore = "two games for minutes; run with --ignored (see the file's doc)"]
fn lumiose() {
    let Some(seen) = pair(&Pair { name: "lumiose", a: "mp-pair-a-lumiose.txt", b: "mp-pair-b-lumiose.txt", b_save: Save::Full, sim: None, restart: false }) else { return };
    meet_walk_leave_save(&seen, TILE);
    smooth(&seen.a_log);
}

/// the server delays every send by 50-150 ms and drops 10% of the positions.
#[test]
#[ignore = "two games for minutes; run with --ignored (see the file's doc)"]
fn vaniville_jitter() {
    let p = Pair { name: "vaniville-jitter", a: "mp-pair-a-vaniville.txt", b: "mp-pair-b-vaniville.txt", b_save: Save::Trainer2, sim: Some("50,150,10"), restart: false };
    let Some(seen) = pair(&p) else { return };
    meet_walk_leave_save(&seen, 2.0 * TILE);
    smooth(&seen.a_log);
}

/// the server is killed with both players on the map and started again 3 s later.
#[test]
#[ignore = "two games for minutes; run with --ignored (see the file's doc)"]
fn server_restart() {
    let p = Pair { name: "server-restart", a: "mp-pair-a-restart.txt", b: "mp-pair-b-restart.txt", b_save: Save::Trainer2, sim: None, restart: true };
    let Some(seen) = pair(&p) else { return };
    for (who, lost, l) in [("A", seen.lost[0], &seen.a_log), ("B", seen.lost[1], &seen.b_log)] {
        let lost = lost.unwrap_or_else(|| panic!("{who}'s puppet stayed after the server died"));
        println!("server killed: {who}'s puppet went {lost:?} later");
        assert!(lost < Duration::from_secs(2), "{who}'s puppet went {lost:?} after the server died");
        assert!(l.contains("checkpoint back"), "{who} never saw the other player again");
        assert!(count(l, "puppets: spawn session") >= 2, "{who}: no second spawn after the restart");
    }
}

/// Mods from the Mods screen and worlds: a game with a skin on (a player model GARC, on the cosmetic list) joins; one
/// with a mod that changes game data (the English text) plays offline and says which mod. The mods are copies of the
/// game's own files (XR_DUMP, the unpacked RomFS), so the game runs as it would without them.
#[test]
#[ignore = "boots the game twice; run with --ignored (see the file's doc)"]
fn a_skin_joins_and_a_data_mod_does_not() {
    let Some((cci, data, full, trainer2)) = setup() else { return };
    let Ok(dump) = std::env::var("XR_DUMP") else { return println!("skipping: XR_DUMP not set") };
    let out = std::env::var("XR_MP_OUT").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("redex_pair")).join("mods");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut srv = server(port, None);
    for (id, name, file) in [(610259, "Delta over Serena", "a/0/8/8"), (724531, "A text mod", "a/0/7/4")] {
        let dir = out.join(id.to_string());
        data_dir(&dir, &data, &Save::Full, &full, &trainer2);
        let unpacked = dir.join("mods/installed/.unpack");
        std::fs::create_dir_all(unpacked.join("romfs").join(file).parent().unwrap()).unwrap();
        std::fs::copy(Path::new(&dump).join("romfs").join(file), unpacked.join("romfs").join(file)).unwrap();
        xr_hle::modlist::install(&dir, id, name, "", &unpacked).unwrap();
        let mut g = Command::new(env!("CARGO_BIN_EXE_redex"));
        g.arg("--cci").arg(&cci).arg("--build").arg(dir.join("build/x-usa-v0.2.0"));
        g.args(["--headless", "--frames", "60", "--world", &format!("127.0.0.1:{port}"), "--dev-id", "--name", "Modder"]);
        let status = g.stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        let l = log(&dir);
        assert!(status.success(), "{name}: exited {status}\n{}", tail(&l));
        if file == "a/0/8/8" {
            assert!(l.contains(&format!("world: joining 127.0.0.1:{port}")) && !l.contains("not joining"), "{name} did not join\n{}", tail(&l));
        } else {
            assert!(l.contains(&format!("not joining 127.0.0.1:{port}: Turn off mods to play online: {name} changes the game")), "{name} was not refused\n{}", tail(&l));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    let _ = srv.kill();
}

// ---- X and Y together -------------------------------------------------------------------------------
//
// Every multiplayer test pairs X with Y. B (the biker) wears an outfit that is not its save's (XR_SET_FASHION, written
// once the field is up) and rides the bike; A watches B's puppet (the observer scripts); B whispers to A and A reports B
// (XR_MP_WHISPER / XR_MP_REPORT: a headless game has no menu). The scripts are driven by these tests (they have
// checkpoints but no expects: the asserts are here). Needs the Y game too, else the tests skip:
//   XR_CCI_Y (default: Pokemon Y (USA) ... Decrypted.3ds beside XR_CCI), XR_DATA_Y (a data dir with
//   build/y-usa-v0.2.0/xgame.dll, default <XR_DATA>/../play-online/y-online), XR_SAVE_Y (a Y `user` dir: the new game saved in
//   the Vaniville house, default <XR_DATA>/../work/xy-cross/ysave/user). Y's biker is the X 100% save placed in Y's folder.

const LOOK: &str = "4864090101cb08034100000000000000";
const WHISPER: &str = "hello from the biker";

#[derive(Clone, Copy, PartialEq)]
enum G {
    X,
    Y,
}

struct Xy {
    cci_x: String,
    cci_y: String,
    data_x: PathBuf,
    data_y: PathBuf,
    full: PathBuf,
    ysave: PathBuf,
}

fn xy_setup() -> Option<Xy> {
    let (cci_x, data_x, full, _) = setup()?;
    let root = data_x.join("..");
    let cci_y = std::env::var("XR_CCI_Y").unwrap_or_else(|_| {
        Path::new(&cci_x).with_file_name("Pokemon Y (USA) (En,Ja,Fr,De,Es,It,Ko) Decrypted.3ds").to_string_lossy().into_owned()
    });
    let data_y = std::env::var("XR_DATA_Y").map(PathBuf::from).unwrap_or_else(|_| root.join("play-online/y-online"));
    let ysave = std::env::var("XR_SAVE_Y").map(PathBuf::from).unwrap_or_else(|_| root.join("work/xy-cross/ysave/user"));
    for (what, p) in [("the Y game (XR_CCI_Y)", PathBuf::from(&cci_y)), ("Y's xgame.dll (XR_DATA_Y)", data_y.join("build/y-usa-v0.2.0/xgame.dll")), ("Y's save (XR_SAVE_Y)", ysave.clone())] {
        if !p.exists() {
            println!("skipping: {what} is missing at {}", p.display());
            return None;
        }
    }
    Some(Xy { cci_x, cci_y, data_x, data_y, full, ysave })
}

fn xy_dir(x: &Xy, dir: &Path, g: G, biker: bool) {
    match g {
        G::X => data_dir(dir, &x.data_x, &Save::Full, &x.full, &x.full),
        G::Y => {
            let _ = std::fs::remove_dir_all(dir);
            let build = dir.join("build/y-usa-v0.2.0");
            std::fs::create_dir_all(&build).unwrap();
            std::fs::create_dir_all(dir.join("logs")).unwrap();
            std::fs::copy(x.data_y.join("build/y-usa-v0.2.0/xgame.dll"), build.join("xgame.dll")).unwrap();
            copy_tree(&x.ysave, &dir.join("user"));
            if biker {
                std::fs::copy(&x.full, dir.join("user/savedata/0004000000055E00/main")).unwrap();
            }
            writable(&dir.join("user"));
        }
    }
}

fn xy_game(x: &Xy, dir: &Path, g: G, script: &str, world: &str, name: &str, env: &[(&str, String)], extra: &[String]) -> Child {
    let (cci, build) = if g == G::X { (&x.cci_x, "x-usa-v0.2.0") } else { (&x.cci_y, "y-usa-v0.2.0") };
    let mut c = Command::new(env!("CARGO_BIN_EXE_redex"));
    c.arg("--cci").arg(cci).arg("--build").arg(dir.join("build").join(build));
    c.args(["--headless", "--renderer", "vulkan", "--script", &format!("{SCRIPTS}/{script}"), "--realtime", "--world", world, "--dev-id", "--name", name]).args(extra);
    c.envs(env.iter().map(|(k, v)| (*k, v.as_str()))).stdout(Stdio::null()).stderr(Stdio::null());
    c.spawn().unwrap()
}

/// Waits for both games (25 minutes at most) and stops the server; both exited 0, with none of the puppet faults in
/// their logs. Their logs.
fn xy_wait(mut ga: Child, mut gb: Child, mut srv: Child, da: &Path, db: &Path) -> (String, String) {
    let (mut ca, mut cb) = (None, None);
    let end = Instant::now() + Duration::from_secs(25 * 60);
    while ca.is_none() || cb.is_none() {
        if Instant::now() > end {
            let _ = (ga.kill(), gb.kill(), srv.kill());
            panic!("the pair ran over 25 minutes\nA:\n{}\nB:\n{}", tail(&log(da)), tail(&log(db)));
        }
        ca = ca.or(ga.try_wait().unwrap().map(|s| s.code()));
        cb = cb.or(gb.try_wait().unwrap().map(|s| s.code()));
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = srv.kill();
    let (la, lb) = (log(da), log(db));
    for (who, code, l) in [("A", ca, &la), ("B", cb, &lb)] {
        assert_eq!(code, Some(Some(0)), "{who} exited {code:?}\n{}", tail(l));
        for bad in ["panic", "guest memory fault", "WARNING: puppets", " skipped: model", "failed: Create returned"] {
            assert!(!l.contains(bad), "{who}'s log has {bad:?}\n{}", tail(l));
        }
    }
    (la, lb)
}

/// A (observer) and B (biker) against a fresh `--dev` server: B in another outfit on the bike, B whispers to A, A
/// reports B.
fn xy_pair(name: &str, a: G, b: G) {
    let Some(x) = xy_setup() else { return };
    let out = std::env::var("XR_MP_OUT").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("redex_pair")).join(name);
    let (da, db, world_dir) = (out.join("a"), out.join("b"), out.join("world"));
    xy_dir(&x, &da, a, false);
    xy_dir(&x, &db, b, true);
    let _ = std::fs::remove_dir_all(&world_dir);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let srv = server_in(port, None, Some(&world_dir));
    let world = format!("127.0.0.1:{port}");
    let (script_a, report_at) = if a == G::X { ("mp-xy-a-x-observer.txt", 3500) } else { ("mp-xy-a-y-observer.txt", 3300) };
    let ga = xy_game(&x, &da, a, script_a, &world, "PlayerA", &[("XR_MP_REPORT", format!("{report_at}:3:xy test report"))], &[]);
    let gb = xy_game(&x, &db, b, "mp-xy-b-biker.txt", &world, "PlayerB", &[("XR_SET_FASHION", format!("{LOOK}@1300")), ("XR_MP_WHISPER", format!("3300:{WHISPER}"))], &[]);
    let (la, lb) = xy_wait(ga, gb, srv, &da, &db);
    // A sees B in B's outfit (never the save's own) ...
    let spawns: Vec<&str> = la.lines().filter(|l| l.starts_with("puppets: spawn session")).collect();
    assert!(spawns.last().is_some_and(|s| s.contains(&format!("dressed {LOOK}"))), "A's last puppet is not in B's outfit: {spawns:?}\n{}", tail(&la));
    // ... and on the bike: B's own mode, and A's puppet covers more ground between two watch checkpoints (30 frames
    // apart) than running could (2.25 a frame); the motion line's longest step is no help, a respawn is a jump
    assert!(lb.lines().any(|l| l.starts_with("field bike") && l.contains("mode Bike")), "B never rode the bike
{}", tail(&lb));
    let places: Vec<[f32; 3]> = watched(&la).iter().map(|w| w.0).collect();
    let longest = places.windows(2).map(|w| (w[0][0] - w[1][0]).abs().max((w[0][2] - w[1][2]).abs())).fold(0.0, f32::max);
    assert!(longest > 30.0 * 2.25 && longest < 30.0 * 4.5 + 1.0, "A's puppet moved {longest} between checkpoints, not at the bike's speed: {places:?}");
    // the whisper arrived, the report went out
    assert!(la.contains(&format!("): {WHISPER}")) && la.contains("whisper from session"), "A did not hear B's whisper\n{}", tail(&la));
    assert!(lb.contains("XR_MP_WHISPER") && lb.contains(": sent"), "{}", tail(&lb));
    assert!(la.contains("XR_MP_REPORT session") && la.contains(": sent"), "{}", tail(&la));
    // the server wrote one line about B, with the reason and the whisper as context
    let reports = std::fs::read_to_string(world_dir.join("reports.jsonl")).expect("the server wrote no reports.jsonl");
    let lines: Vec<&str> = reports.lines().collect();
    assert_eq!(lines.len(), 1, "{reports}");
    let r = lines[0];
    println!("{name}: {}", &r[..r.len().min(260)]);
    assert!(r.contains("\"reason\":3") && r.contains("\"note\":\"xy test report\""), "{r}");
    assert!(r.contains("\"target_name\":\"PlayerB\"") && r.contains("\"reporter_name\":\"PlayerA\""), "{r}");
    // a dev-id login has no Steam account
    assert!(r.contains("\"target\":\"dev:") && r.contains("\"target_steam_id\":0"), "{r}");
    assert!(r.contains(&format!("\"text\":\"{WHISPER}\"")), "the report lacks B's whisper: {r}");
    println!("{name}: A's puppet covered {longest} in 30 frames (run 67.5, bike up to 135); {}", spawns.last().unwrap());
}

/// A = Pokemon X (100% save, Fly to Vaniville), B = Pokemon Y (the biker).
#[test]
#[ignore]
fn xy_a_x_b_y() {
    xy_pair("xy-a-x-b-y", G::X, G::Y);
}

/// A = Pokemon Y (the new-game trainer in the Vaniville house), B = Pokemon X (the biker).
#[test]
#[ignore]
fn xy_a_y_b_x() {
    xy_pair("xy-a-y-b-x", G::Y, G::X);
}

// ---- made looks, X with Y ---------------------------------------------------------------
//
// Each player plays a character made as the creator makes one (characters.json and the 100% save in the character's
// folder, Y's in Y's title folder) with `--character`, not XR_SET_FASHION. The two looks are boys whose skin, hair and
// eyes differ from the save's (the save trainer: skin 2, style 3, colour 1, eyes 2) and from each other's. Mid-meeting A's outfit
// changes (XR_SET_FASHION stands in for a boutique: the game keeps it as a change of the character's look).

use xr_net::x::look::Look;

const LOOK_A: Look = Look { gender: 0, skin: 1, hair_style: 1, hair_colour: 3, eyes: 4 };
const LOOK_B: Look = Look { gender: 0, skin: 3, hair_style: 4, hair_colour: 2, eyes: 0 };
/// the save trainer's clothes (the 100% save's), which A changes into
const TOBIN: [u8; 16] = [0x88, 0xb9, 0x19, 0x05, 0xd0, 0x09, 0x32, 0x03, 0x11, 0, 0, 0, 0, 0, 0, 0];

fn bytes_hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// A character in `dir`'s characters.json, its folder holding the `user` folder xy_dir made. Its id.
fn xy_character(dir: &Path, name: &str, look: Look) -> u64 {
    use xr_frontend::start::characters::{user_dir, Store};
    let mut store = Store::load(dir);
    let id = store.create(name, look.appearance().unwrap(), 1).unwrap();
    store.save(dir).unwrap();
    std::fs::create_dir_all(user_dir(dir, id).parent().unwrap()).unwrap();
    std::fs::rename(dir.join("user"), user_dir(dir, id)).unwrap();
    id
}

/// A (observer) and B (biker), each as a character of its own: each sees the other's made look on the puppet, then A's
/// change of clothes; the server keeps each one's character on its account, A's in the changed clothes.
fn xy_created(name: &str, a: G, b: G) {
    let Some(x) = xy_setup() else { return };
    let out = std::env::var("XR_MP_OUT").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("redex_pair")).join(name);
    let (da, db, world_dir) = (out.join("a"), out.join("b"), out.join("world"));
    xy_dir(&x, &da, a, true);
    xy_dir(&x, &db, b, true);
    let (id_a, id_b) = (xy_character(&da, "Alex", LOOK_A), xy_character(&db, "Blair", LOOK_B));
    let (look_a, look_b) = (LOOK_A.appearance().unwrap(), LOOK_B.appearance().unwrap());
    let changed = LOOK_A.wearing(TOBIN).unwrap();
    assert!(changed != look_a && look_a.fashion != look_b.fashion);
    let _ = std::fs::remove_dir_all(&world_dir);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let srv = server_in(port, None, Some(&world_dir));
    let world = format!("127.0.0.1:{port}");
    let character = |id: u64| ["--character".to_string(), format!("{id:016x}")];
    let ga = xy_game(&x, &da, a, "mp-xy-a-x-observer.txt", &world, "PlayerA", &[("XR_SET_FASHION", format!("{}@3500", bytes_hex(&changed.fashion)))], &character(id_a));
    let gb = xy_game(&x, &db, b, "mp-xy-b-biker.txt", &world, "PlayerB", &[], &character(id_b));
    let (la, lb) = xy_wait(ga, gb, srv, &da, &db);
    for (who, l, look) in [("A", &la, look_a), ("B", &lb, look_b)] {
        let applied = format!("character: look applied {} (continue)", bytes_hex(&look.to_bytes()));
        assert!(l.contains(&applied), "{who}'s log lacks {applied:?}\n{}", tail(l));
    }
    let spawns = |l: &str| l.lines().filter(|l| l.starts_with("puppets: spawn session")).map(String::from).collect::<Vec<_>>();
    let (sa, sb) = (spawns(&la), spawns(&lb));
    println!("{name}: A's spawns {sa:#?}\n{name}: B's spawns {sb:#?}");
    // A sees B's made look; B sees A's, then A's new clothes
    let dressed = |a: &xr_net::x::field::Appearance| format!("dressed {}", bytes_hex(&a.fashion));
    assert!(sa.last().is_some_and(|s| s.contains(&dressed(&look_b))), "A's last puppet is not B's made look: {sa:?}");
    assert!(sb.iter().any(|s| s.contains(&dressed(&look_a))), "B never saw A's made look: {sb:?}");
    assert!(la.contains(&format!("character: the game changed the look to {}", bytes_hex(&changed.to_bytes()))), "{}", tail(&la));
    assert!(sb.iter().any(|s| s.contains(&dressed(&changed))), "B never saw A's new clothes: {sb:?}");
    // the server holds each dev id's character, A's in its new clothes
    for (who, dir, id, look) in [("A", &da, id_a, changed), ("B", &db, id_b, look_b)] {
        let key = xr_net::id::load_or_create(&dir.join("user")).unwrap() & !(1 << 127);
        let stored = redex_world::accounts::load(&world_dir, key).unwrap();
        let c = stored.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{who}'s account lacks its character: {stored:?}"));
        assert_eq!(bytes_hex(&c.look), bytes_hex(&look.to_bytes()), "{who}'s character on the account");
    }
}

/// A = Pokemon X, B = Pokemon Y, each a made character on the 100% save.
#[test]
#[ignore]
fn xy_created_looks() {
    xy_created("xy-created-a-x-b-y", G::X, G::Y);
}

/// A = Pokemon Y, B = Pokemon X.
#[test]
#[ignore]
fn xy_created_looks_swapped() {
    xy_created("xy-created-a-y-b-x", G::Y, G::X);
}

/// Two girls (a girl's outfit must load the player's part set, or her long hair is a plank): A on X and B on Y,
/// each a new game as a girl character to first control in the bedroom (the same map in both games). Each sees the
/// other on the girl model, dressed in her made look; the screens at `look2` are each side's view
/// (`XR_MP_OUT/<name>/{a,b}/logs/top-cp-look2.png`).
fn xy_girls(name: &str, a: G, b: G, third: Option<Look>) {
    use xr_frontend::start::characters::Store;
    let Some(x) = xy_setup() else { return };
    let out = std::env::var("XR_MP_OUT").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("redex_pair")).join(name);
    let (da, db, world_dir) = (out.join("a"), out.join("b"), out.join("world"));
    // long hair (style 2, the plank's) and another style, other skins, colours and eyes
    let looks = [Look { gender: 1, skin: 1, hair_style: 2, hair_colour: 0, eyes: 0 }, Look { gender: 1, skin: 3, hair_style: 4, hair_colour: 3, eyes: 3 }];
    let mut ids = Vec::new();
    for (dir, g, look, who) in [(&da, a, looks[0], "Ana"), (&db, b, looks[1], "Bea")] {
        let _ = std::fs::remove_dir_all(dir);
        let (data, build) = if g == G::X { (&x.data_x, "x-usa-v0.2.0") } else { (&x.data_y, "y-usa-v0.2.0") };
        std::fs::create_dir_all(dir.join("build").join(build)).unwrap();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::copy(data.join("build").join(build).join("xgame.dll"), dir.join("build").join(build).join("xgame.dll")).unwrap();
        let mut store = Store::load(dir);
        ids.push(store.create(who, look.appearance().unwrap(), 1).unwrap());
        store.save(dir).unwrap();
    }
    let _ = std::fs::remove_dir_all(&world_dir);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let srv = server_in(port, None, Some(&world_dir));
    let world = format!("127.0.0.1:{port}");
    let character = |id: u64| ["--character".to_string(), format!("{id:016x}")];
    // a third girl beside A (a fake peer, once B is met): she wears hers on the NPC set, as B holds the free player heap
    let fake: Vec<(&str, String)> = third.map_or(Vec::new(), |t| {
        vec![("XR_FAKE_PEERS", "1".into()), ("XR_FAKE_LOOK", bytes_hex(&t.appearance().unwrap().to_bytes())), ("XR_FAKE_STILL", "1".into()), ("XR_FAKE_FROM", "6502".into())]
    });
    let ga = xy_game(&x, &da, a, "mp-xy-girl-a.txt", &world, "PlayerA", &fake, &character(ids[0]));
    let gb = xy_game(&x, &db, b, "mp-xy-girl-b.txt", &world, "PlayerB", &[], &character(ids[1]));
    let (la, lb) = xy_wait(ga, gb, srv, &da, &db);
    for (who, l, mine, theirs) in [("A", &la, looks[0], looks[1]), ("B", &lb, looks[1], looks[0])] {
        let (mine, theirs) = (mine.appearance().unwrap(), theirs.appearance().unwrap());
        assert!(l.contains(&format!("character: look applied {} (new game)", bytes_hex(&mine.to_bytes()))), "{who}\n{}", tail(l));
        let spawns: Vec<&str> = l.lines().filter(|l| l.starts_with("puppets: spawn session")).collect();
        println!("{name}: {who}'s spawns {spawns:#?}");
        let want = format!("(model 1) dressed {}", bytes_hex(&theirs.fashion));
        assert!(spawns.iter().any(|s| s.contains(&want)), "{who} never saw the other girl dressed: {spawns:?}");
        // the girl's outfit holds the second player heap at the checkpoints
        let dress = l.lines().find(|l| l.starts_with("dress look2: ")).unwrap_or_else(|| panic!("{who}: no dress line at look2\n{}", tail(l)));
        println!("{name}: {who} {dress}");
        assert!(!dress.ends_with("0x00000000 (globals") && !dress.contains(" 0x00000000 (globals"), "{who}: the puppet's outfit holds no player heap: {dress}");
    }
    if let Some(t) = third {
        // all three girls are seen; none is left out for want of an outfit
        let want = format!("(model 1) dressed {}", bytes_hex(&t.appearance().unwrap().fashion));
        assert!(la.lines().any(|l| l.starts_with("puppets: spawn session") && l.contains(&want)), "A never saw the third girl dressed\n{}", tail(&la));
        assert!(!la.contains("a girl model with no outfit"), "A left a girl out\n{}", tail(&la));
        println!("{name}: A's view of B and the third girl: {}", da.join("logs/top-cp-seen.png").display());
    }
    println!("{name}: the views: {} and {}", da.join("logs/top-cp-look2.png").display(), db.join("logs/top-cp-look2.png").display());
}

/// A = Pokemon X, B = Pokemon Y, two new-game girls in the bedroom.
#[test]
#[ignore]
fn xy_created_girls() {
    xy_girls("xy-girls-a-x-b-y", G::X, G::Y, None);
}

/// three girls on one map, X + Y + a fake peer beside A: A sees both, dressed (B on the player set, the
/// third on the NPC set), none left out.
#[test]
#[ignore]
fn xy_created_girls_and_a_third() {
    xy_girls("xy-girls-three", G::X, G::Y, Some(Look { gender: 1, skin: 2, hair_style: 1, hair_colour: 2, eyes: 1 }));
}
