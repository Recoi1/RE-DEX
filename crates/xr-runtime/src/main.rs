//! `redex --cci <game.cci> --build <XR_DATA>/build/<id> [--headless] [--frames N] [--screens-every N] [--deterministic]
//!       [--renderer software|vulkan] [--resolution N] [--press BUTTON@FRAME]... [--mute] [--room HOST:PORT] [--room-server PORT]
//!       [--world HOST:PORT] [--name NAME] [--dev-id] [--realtime] [+connect steam:ID]` (--room, --room-server and
//!       --dev-id only in an `mp-tests` build; `refusal` keeps dev and test runs, and modded games, out of worlds):
//! runs the recompiled game in a window (keyboard, controllers, sound; see xr-frontend), or with --headless without one:
//! then the screens go to `<XR_DATA>/logs/top.png` and `bottom.png` at exit (and `top-<frame>.png` every N frames with
//! --screens-every), the sound to `audio.wav`, and --press holds a console button (A, B, X, Y, L, R, START, SELECT, UP,
//! DOWN, LEFT, RIGHT, ZL, ZR, CIRCLE_UP/DOWN/LEFT/RIGHT) or TOUCH:X,Y for 8 frames (`@FRAME+HOLD` for longer), and
//! --script FILE reads `FRAME NAME [HOLD]`, `FRAME checkpoint NAME`, `FRAME wait_until COND [TIMEOUT]`, `expect NAME HASH` and `autobattle` lines (implies
//! --deterministic; needs --headless; --record-expects rewrites the expect lines and the header saying which DLL build
//! and renderer they were recorded with, once every checkpoint was reached; a mismatch, a run that ends before its last
//! checkpoint or an autobattle stuck 200k frames past it exits 4). --deterministic charges interpreted code like native
//! code (clock.rs; XR_NATIVE_CLOCK=0 opts out). --world joins a RE:DEX world server (xr-net; a window takes it from the
//! settings when not given) as --name (default: the settings' name, else the save's trainer name); `+connect ADDR` is
//! the same as --world (Steam starts a game that way for "Join game"). --realtime holds a
//! headless run to the console's 60 frames a second (instances meeting online play at the same speed). XR_FRAMETIMES=1 writes
//! each headless frame's host time to `<logs>/frametimes.csv`; XR_INBETWEENS=DIR saves Smooth motion's pictures in between
//! (inbetweens.rs). XR_STATE_SAVE_AT=FRAME:PATH writes a save state between frames at that script frame, and
//! XR_STATE_LOAD=PATH starts a headless run from one (frames and the script go on from its frame). `--preview STATE
//! --cci --build` is the character creator's preview child (preview.rs). `--character <16 hex>` plays as that character
//! of `<data>/characters.json`: its saves in `<data>/characters/<id>/user`, backed up before boot, and its look in the
//! game (character.rs). Exit codes: 0 the
//! game exited (or N frames ran, or the window closed), 5 svcBreak, 6 guest memory fault, 7 host stack overflow, 101
//! runtime panic.
// no console window of its own (a terminal it was run from still gets its output, see main)
#![windows_subsystem = "windows"]
mod abi;
mod ahead;
mod bridge;
mod character;
mod clock;
mod discord;
mod exec;
mod explore;
mod game;
mod hooks;
mod hostprof;
mod inbetweens;
mod interp;
mod intro;
mod lobby;
mod log;
mod mem;
mod preview;
mod progress;
mod rate;
mod script;
mod wav;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use xr_hle::FrameOutcome;
use xr_hle::services::hid::{InputState, PadState};

/// Host address of guest address 0 (0 before boot), for the fault handler.
pub static GUEST_BASE: AtomicUsize = AtomicUsize::new(0);

/// A host access violation inside guest memory is a guest fault (unless it hit a page lazy GPU coherency protects):
/// log the guest address and stop (exit 6). A host
/// stack overflow (deep guest recursion on a host thread) stops with exit 7.
unsafe extern "system" fn on_fault(info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    let rec = unsafe { &*(*info).ExceptionRecord };
    match rec.ExceptionCode as u32 {
        0xC000_0005 => {
            let base = GUEST_BASE.load(Ordering::Relaxed);
            let a = rec.ExceptionInformation[1].wrapping_sub(base);
            // a page the host GPU drew that memory doesn't have yet (xr_hle Memory::lazy): written back, then retried
            if base != 0 && a < 1 << 32 && exec::lazy_fault(a as u32) {
                return -1; // EXCEPTION_CONTINUE_EXECUTION
            }
            if base != 0 && a < 1 << 32 {
                let kind = ["read", "write"].get(rec.ExceptionInformation[0]).unwrap_or(&"execute");
                // a mapped page here: a write to a read-only page, or one lazy write-back could not unprotect (logged above)
                let what = match exec::page_bits(a as u32) {
                    Some(0) | None => "unmapped".to_string(),
                    Some(bits) => format!("mapped, page bits 0x{bits:02X}"),
                };
                log::stop(6, &format!("guest memory fault: {kind} at 0x{a:08X} ({what})"));
            }
        }
        0xC000_00FD => log::stop(7, &format!("host stack overflow on thread {:?}", std::thread::current().name())),
        _ => {}
    }
    0 // EXCEPTION_CONTINUE_SEARCH
}

/// Routes xr-hle's `log` records into our log (stderr + latest.log). XR_LOG=trace|debug|info|warn picks the level.
struct HleLog;

impl ::log::Log for HleLog {
    fn enabled(&self, _: &::log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &::log::Record) {
        match r.level() {
            ::log::Level::Info => log::log(&format!("{}", r.args())),
            l => log::log(&format!("{l} {}: {}", r.target(), r.args())),
        }
    }
    fn flush(&self) {}
}

/// Our log at `path` (truncated), with xr-hle's and the frontend's `log` records in it. XR_LOG picks the level.
fn init_log(path: &std::path::Path) -> Result<(), String> {
    log::init(path).map_err(|e| format!("log: {e}"))?;
    let level = std::env::var("XR_LOG").ok().and_then(|l| l.parse().ok()).unwrap_or(::log::LevelFilter::Info);
    ::log::set_logger(&HleLog).map_err(|e| e.to_string())?;
    ::log::set_max_level(level);
    Ok(())
}

/// A saves folder of this process's own, `<data>/preview/<build id>/user-<tag>-<pid>-<nanos>`, for `--make-preview` and
/// `--preview` (never a player's `<data>/user`). Nothing else uses it, so nothing is wiped and no maker, `Warm` or live
/// child can break another's; the name is new, so it starts empty. Folders a day old (earlier runs) are swept, best
/// effort: one still open fails to go and stays.
fn preview_user(data: &std::path::Path, build: &std::path::Path, tag: &str) -> std::path::PathBuf {
    let root = data.join("preview").join(build.file_name().unwrap_or_default());
    let day = std::time::Duration::from_secs(86_400);
    for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a > day);
        if old && e.file_name().to_string_lossy().starts_with("user-") {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    root.join(format!("user-{tag}-{}-{nanos}", std::process::id()))
}

/// Holds `<data>`'s named mutex for the rest of this thread's life: one game window per save folder. False when another
/// process still holds it after `wait_ms` (a restart waits for the old process to end).
fn own_data_folder(data: &std::path::Path, wait_ms: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    let name = xr_frontend::start::storage::data_lock_name(data);
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if h.is_null() {
            return true; // shortcut: no mutex (out of handles), no guard
        }
        let r = WaitForSingleObject(h, wait_ms);
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            return true; // the handle stays open until the process ends
        }
        CloseHandle(h);
        false
    }
}

fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
    let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    unsafe { MessageBoxW(std::ptr::null_mut(), w(text).as_ptr(), w("RE:DEX").as_ptr(), MB_OK | MB_ICONINFORMATION) };
}

/// `redex` with no arguments: RE:DEX's own start screen. Play starts the game in a fresh process, as the command line would.
fn start_screen() -> Result<(), String> {
    let data = xr_intake::data_dir(std::env::var_os("XR_DATA"), std::env::var_os("APPDATA")).ok_or("no data folder: set XR_DATA or APPDATA")?;
    // the start screen's own log (a failed build's details land here); the game it starts keeps latest.log
    init_log(&data.join("logs/start.log"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // a dev tree has no toolchain next to the exe: the screen still opens, "Add your game" says why it is off
    let tools = redex_builder::Toolchain::from_release_dir(exe.parent().ok_or("no exe folder")?);
    let Some(launch) = xr_frontend::start::run_start(data, tools)? else { return Ok(()) };
    let mut cmd = std::process::Command::new(exe);
    // always one mode: --offline, --p2p or --world (P2P), or --world with --account-server (an online character, whose
    // game signs in with the session the start screen kept, never a password)
    cmd.args(launch.args());
    // no console of ours and no handles to it: closing a terminal RE:DEX was started from leaves the game running
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0000_0008); // DETACHED_PROCESS
    cmd.spawn().map_err(|e| format!("could not start the game: {e}"))?;
    Ok(())
}

/// `redex --help`. The MP test harness's flags (--dev-id, --room, --room-server) exist only in an mp-tests build.
const USAGE: &str = "usage: redex                      the start screen
       redex --cci GAME --build <data>/build/<id> [--character ID] [--world HOST:PORT|steam:ID | --p2p | --offline] [--name NAME]
             [--renderer software|vulkan] [--resolution N] [--mute] [--frames N] [--press BUTTON@FRAME]...
             [--headless [--screens-every N] [--deterministic] [--realtime] [--script FILE [--record-expects]]]
             [--make-preview boy|girl --out PATH] [--preview STATE] [+connect steam:ID]
             [--account-server URL --world tls:HOST:PORT]   (signs in with the session the start screen kept)
             [--lobby ROOM-CODE]   (joins the lobby that room code names, found through Steam)
  Without --world, --p2p (Steam worlds; the settings' world if any) or --account-server the game plays offline.
  Dev and test options (--headless, --script, --press, --deterministic, --make-preview, XR_STATE_*, XR_EXPLORE*,
  XR_FAKE_*, XR_MP_*, XR_SET_FASHION, XR_RAILPOS) keep the game offline, and so do mods that change the game.";

/// A deterministic run's console clock: 2000-01-01 12:00 (millis since 1900).
const FIXED_CLOCK: u64 = 36524 * 86_400_000 + 12 * 3_600_000;

fn run() -> Result<(), String> {
    unsafe { windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler(1, Some(on_fault)) };
    if std::env::args_os().len() == 1 {
        return start_screen();
    }
    let (mut cci, mut build, mut frames, mut deterministic, mut every) = (None, None, None, false, None);
    let (mut headless, mut window, mut presses) = (false, xr_frontend::Options::default(), Vec::new());
    let (mut checkpoints, mut script_file, mut record, mut auto, mut waits) = (Vec::new(), None, false, None, Vec::new());
    let mut realtime = false;
    let (mut make_preview, mut out, mut preview) = (None::<&'static str>, None::<PathBuf>, None::<PathBuf>);
    // --room, --room-server and --dev-id exist only in an mp-tests build (the MP test harness)
    #[cfg_attr(not(feature = "mp-tests"), allow(unused_mut))]
    let (mut room, mut room_server) = (None::<String>, None::<u16>);
    #[cfg_attr(not(feature = "mp-tests"), allow(unused_mut))]
    let (mut world, mut name, mut dev_id, mut offline) = (None::<String>, None::<String>, false, false);
    let (mut character, mut p2p) = (None::<u64>, false);
    // --lobby CODE: join the lobby a room code names (the start screen's Join), once Steam found it
    let mut lobby_code = None::<String>;
    // an account of the account backend (central worlds): the session the start screen kept, or (mp-tests) --login
    let mut account_server = None::<String>;
    #[cfg_attr(not(feature = "mp-tests"), allow(unused_mut))]
    let mut login = None::<String>;
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().ok_or(format!("{a:?} needs a value"));
        let number = |v: std::ffi::OsString| v.to_str().and_then(|n| n.parse::<u64>().ok()).filter(|&n| n > 0).ok_or(format!("{a:?} needs a number"));
        match a.to_str() {
            Some("--cci") => cci = Some(PathBuf::from(value()?)),
            Some("--build") => build = Some(PathBuf::from(value()?)),
            Some("--frames") => frames = Some(number(value()?)?),
            Some("--screens-every") => every = Some(number(value()?)?),
            Some("--deterministic") => deterministic = true,
            Some("--headless") => headless = true,
            Some("--renderer") => {
                window.hardware = Some(match value()?.to_str() {
                    Some("vulkan") => true,
                    Some("software") => false,
                    _ => return Err("--renderer is software or vulkan".into()),
                })
            }
            Some("--resolution") => window.resolution = Some(number(value()?)?.min(10) as u32), // 1 to 10 times the console's
            Some("--press") => presses.push(press(&value()?.to_string_lossy())?),
            Some("--script") => {
                let f = PathBuf::from(value()?);
                let text = std::fs::read_to_string(&f).map_err(|e| format!("{f:?}: {e}"))?;
                let (p, c, a, w) = script::parse(&text).map_err(|e| format!("{f:?}: {e}"))?;
                // a script's frame numbers only hold if runs repeat
                (presses, checkpoints, script_file, deterministic, auto, waits) = (p, c, Some((f, text)), true, a, w);
            }
            // --make-preview boy|girl --out PATH: the creator's bedroom save state for that gender (see PREVIEW_FRAMES)
            Some("--make-preview") => {
                make_preview = Some(match value()?.to_str() {
                    Some("boy") => "boy",
                    Some("girl") => "girl",
                    _ => return Err("--make-preview is boy or girl".into()),
                })
            }
            Some("--out") => out = Some(PathBuf::from(value()?)),
            // --preview STATE: the character creator's live preview child (preview.rs)
            Some("--preview") => preview = Some(PathBuf::from(value()?)),
            Some("--record-expects") => record = true,
            Some("--mute") => window.mute = true,
            #[cfg(feature = "mp-tests")]
            Some("--room") => room = Some(value()?.to_string_lossy().into_owned()),
            #[cfg(feature = "mp-tests")]
            Some("--room-server") => room_server = Some(u16::try_from(number(value()?)?).map_err(|_| "--room-server needs a port")?),
            Some("--world") => world = Some(value()?.to_string_lossy().into_owned()),
            // what Steam passes for "Join game": only a world by its SteamID, never a host:port someone's presence names
            Some("+connect") => {
                let addr = value()?.to_string_lossy().into_owned();
                if !steam_world(&addr) {
                    return Err(format!("+connect takes steam:<server id>, not {addr:?}"));
                }
                world = Some(addr);
            }
            Some("--offline") => offline = true,
            // Steam worlds with this PC's characters (P2P): the settings' world at boot if they name one
            Some("--p2p") => p2p = true,
            Some("--lobby") => lobby_code = Some(redex_proto::room::parse(&value()?.to_string_lossy())?),
            Some("--account-server") => account_server = Some(value()?.to_string_lossy().into_owned()),
            #[cfg(feature = "mp-tests")]
            Some("--login") => login = Some(value()?.to_string_lossy().into_owned()),
            Some("--name") => name = Some(value()?.to_string_lossy().into_owned()),
            // log in to a --dev world server with <data>/user/redex-id instead of Steam (a second instance on one PC)
            #[cfg(feature = "mp-tests")]
            Some("--dev-id") => dev_id = true,
            Some("--realtime") => realtime = true,
            Some("--character") => {
                let id = value()?.to_string_lossy().into_owned();
                character = Some((id.len() == 16).then(|| u64::from_str_radix(&id, 16).ok()).flatten().ok_or("--character needs a 16-hex id")?);
            }
            Some("--help" | "-h") => {
                println!("{USAGE}");
                #[cfg(feature = "mp-tests")]
                println!("  MP-TESTS BUILD (the MP test harness; check-release refuses it): [--dev-id] [--room HOST:PORT] [--room-server PORT]");
                #[cfg(feature = "mp-tests")]
                println!("    [--login EMAIL] with --account-server: a password login, the password in REDEX_PASSWORD");
                return Ok(());
            }
            _ => return Err(format!("unknown argument {a:?} (--help lists them)")),
        }
    }
    if let Some(state) = preview {
        return preview::run(cci.ok_or("missing --cci")?, build.ok_or("missing --build")?, state);
    }
    if let Some(g) = make_preview {
        if out.is_none() {
            return Err("--make-preview needs --out".into());
        }
        // headless and deterministic on its own script: the same state on every PC
        let text = if g == "boy" { include_str!("../../../scripts/preview/boy.txt") } else { include_str!("../../../scripts/preview/girl.txt") };
        (presses, checkpoints, auto, waits) = script::parse(text).map_err(|e| format!("scripts/preview/{g}.txt: {e}"))?;
        (headless, deterministic, frames) = (true, true, Some(PREVIEW_FRAMES));
    } else if out.is_some() {
        return Err("--out is for --make-preview".into());
    }
    if record && script_file.is_none() {
        return Err("--record-expects needs --script".into());
    }
    if script_file.is_some() && !headless {
        return Err("--script needs --headless: a window would ignore its checkpoints, expects and autobattle (--press works in a window)".into());
    }
    if realtime && !headless {
        return Err("--realtime is for --headless runs: a window keeps the console's time itself".into());
    }
    // a script runs to its last checkpoint, or past its last wait_until (so the wait is seen through)
    let end = checkpoints.last().map(|c| c.at).max(waits.last().map(|w: &script::Wait| w.at + 1));
    if frames.is_none() {
        frames = end;
    }
    if frames.is_some_and(|f| checkpoints.iter().any(|c| c.at > f)) {
        return Err("a checkpoint is after the last frame (--frames)".into());
    }
    let (cci, build) = (cci.ok_or("missing --cci")?, build.ok_or("missing --build")?);
    // <data>/build/<id> -> <data>/logs/latest.log; saves go to <data>/user (a --character's to its own folder)
    let data = build.parent().and_then(|p| p.parent()).ok_or("--build must be <data>/build/<id>")?;
    // two game windows on one save folder would write over each other's saves (checked before the log, which the other
    // window is writing)
    if !headless && !own_data_folder(data, 5000) {
        message_box("RE:DEX is already playing this game from the same save folder. Switch to its window to keep playing.");
        return Err("another game window is using this data folder".into());
    }
    // the start screen's Pokémon Storage (PKVault) writes these saves while it is open
    if !headless && xr_frontend::start::storage::running(data) {
        message_box("Pokémon Storage is open and may be changing your saves. Close it, then play.");
        return Err("Pokémon Storage (PKVault) is open on this data folder".into());
    }
    // a preview run goes beside a game that is playing: its own log, and the game's loadcro.csv left alone
    init_log(&data.join(if make_preview.is_some() { "logs/preview.log" } else { "logs/latest.log" }))?;
    let logs = data.join("logs");
    progress::init(&logs);
    if make_preview.is_none() {
        let _ = std::fs::remove_file(logs.join("loadcro.csv")); // xr-hle appends this run's LoadCRO rows
    }
    // a window with --p2p joins the world server its settings name when --world does not say; no mode at all is offline
    let settings = (!headless).then(xr_frontend::Settings::load);
    // --lobby is P2P, joining that lobby rather than the settings' world
    let settings_world = settings.as_ref().map(|s| s.world_address.as_str()).filter(|_| lobby_code.is_none());
    let (world, offline) = boot_world(world, p2p || lobby_code.is_some(), offline, account_server.is_some(), settings_world);
    let mut notice = None;
    // --account-server: the session the start screen kept on this PC (DPAPI), checked with the service; with --login
    // (mp-tests builds only) a password login instead. No session, a dead one or no answer plays offline and says why
    let account = match (&account_server, &login, &world) {
        (None, None, _) => None,
        (None, Some(_), _) => return Err("--login needs --account-server".into()),
        (Some(_), _, None) => return Err("--account-server needs a world: --world tls:HOST:PORT".into()),
        (Some(server), Some(email), Some(_)) => {
            match password_login(server, email)? {
                Ok(a) => {
                    log::log(&format!("account: logged in to {server} as account {}", a.account_id));
                    Some(a)
                }
                Err(e) => {
                    log::log(&format!("account: login to {server} failed: {e}; playing offline"));
                    notice = Some(format!("The account login failed: {e}.\n\nPlaying offline."));
                    None
                }
            }
        }
        (Some(server), None, Some(_)) => {
            use xr_net::account::{self, Account, Error};
            let said = match account::saved_session(data, server).map(|token| Account::resume(server, &token)) {
                Some(Ok((a, me))) => {
                    log::log(&format!("account: signed in to {server} with the kept session as account {} ({})", a.account_id, me.username));
                    Ok(a)
                }
                None => Err("You are not signed in on this PC. Sign in on the start screen to play online.".to_owned()),
                Some(Err(Error::SignedOut(m))) => {
                    account::forget_session(data);
                    Err(format!("You were signed out of your account ({m}). Sign in again on the start screen."))
                }
                Some(Err(e @ Error::Unreachable(_))) => Err(format!("The account server can't be reached: {e}.")),
                Some(Err(e)) => Err(format!("The account server did not take the session: {e}.")),
            };
            said.map_err(|why| {
                log::log(&format!("account: {why} Playing offline"));
                notice = Some(format!("{why}\n\nPlaying offline."));
            })
            .ok()
        }
    };
    // --character: the game runs on that character's own saves, backed up before it can write them. Signed in, it is
    // one of the account's (online) characters, read from the account; its saves live on this PC by its id
    let (user, played) = match character {
        Some(id) => {
            use xr_frontend::start::characters;
            let c = match &account {
                Some(a) => {
                    let list = a.characters().map_err(|e| format!("could not read the account's characters: {e}"))?;
                    list.into_iter().find(|c| c.id == id).ok_or(format!("character {id:016x} is not on this account"))?
                }
                None => {
                    // held by another program for a moment: not "no character", but try again
                    let store = characters::Store::try_load(data).ok_or("characters.json is in use by another program just now; try again")?;
                    let why = match &account_server {
                        // an online character whose account did not sign in (said in `notice`)
                        Some(_) => format!("character {id:016x} is an online character: sign in on the start screen to play it"),
                        None => format!("no character {id:016x} in characters.json"),
                    };
                    store.get(id).cloned().ok_or(why)?
                }
            };
            // an online character's saves have a root of their own per account, which offline and P2P play never open
            let user = match &account {
                Some(a) => characters::online_user_dir(data, a.public_id, id),
                None => characters::user_dir(data, id),
            };
            let n = characters::backup_saves(&user, characters::now_ms() / 1000, 10).map_err(|e| format!("could not back up character {id:016x}'s saves: {e}"))?;
            log::log(&format!("character {id:016x}: {n} save(s) backed up"));
            (user, Some((c, account.is_some())))
        }
        // the preview's scripted new game never runs on a player's saves (an old <data>/user would boot into Continue):
        // a new folder of its own, so the state is the same on every PC
        None if make_preview.is_some() => (preview_user(data, &build, make_preview.unwrap_or_default()), None),
        None => (data.join("user"), None),
    };
    // the character's look goes into the game (character.rs); built at boot, once the title is known
    let (data_dir, user_dir) = (data.to_path_buf(), user.clone());
    let played = move |title: u64| played.as_ref().and_then(|(c, online)| character::Played::new(c, &data_dir, &user_dir, title, *online));
    std::panic::set_hook(Box::new(|p| {
        // the world client's thread (redex-net) and a hosted lobby's server threads (redex-accept, redex-conn,
        // redex-steam, redex-lobby-start) are not the game: the thread unwinds into its own guards (the client goes
        // offline, the server drops that connection) and the game plays on
        if let Some(name) = std::thread::current().name().filter(|n| survives_panic(n)) {
            return log::log(&format!("{name}: panicked, the game plays on: {p}"));
        }
        log::stop(101, &format!("panic: {p}"))
    }));

    // deterministic runs charge interpreted code like native code, so their hashes do not depend on what is compiled
    let native_clock = clock::init(deterministic);
    if deterministic {
        log::log(&format!("clock: {} charging model (XR_NATIVE_CLOCK=0/1 picks the other)", if native_clock { "native" } else { "interpreter" }));
    }
    // a fixed console clock makes runs repeat exactly; the guest's clock is virtual either way. 2000-01-01 12:00 (millis
    // since 1900): the 3DS clock's first day (a console never shows 1900), at noon so the field is in daylight
    let config = xr_hle::Config { data_dir: Some(data.to_path_buf()), user_dir: Some(user.clone()), clock: deterministic.then_some(FIXED_CLOCK), skip_idle_loops: !native_clock, no_cro_log: make_preview.is_some(), ..Default::default() };
    if deterministic {
        // and draws wait for their own pipeline: drawn thru the generic one while it compiles, a few pixels depend on
        // how fast the host ran (zakuro-gpu's Hardware reads it at start; no other thread runs yet)
        unsafe { std::env::set_var("ZAKURO_WAIT_FOR_SHADERS", "1") };
    }
    // local wireless (nwm::UDS) thru a Zakuro room: --room joins host:port, --room-server opens one on 127.0.0.1 and
    // joins it; the nickname is the data dir's name (unique per instance). Both live until the process exits
    let server = room_server.map(|p| xr_hle::room::Server::start_on("127.0.0.1", p, "redex", "", 8)).transpose().map_err(|e| format!("room server: {e}"))?;
    let room = room.or(server.as_ref().map(|s| format!("127.0.0.1:{}", s.port())));
    let nickname = data.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "redex".into());
    let connection = room.map(|a| xr_hle::room::Connection::join(&a, &nickname, "")).transpose()?;
    let link = connection.as_ref().map(|c| c.link());
    // online, the player is their Steam account (SteamID64), which the world server checks; with --dev-id, and for the
    // local wireless room, the dev id made once in <data>/user/redex-id. The identity is also the console id: two
    // instances on one PC must be two consoles (X's PSS ignores a beacon carrying its own console hash, cfg
    // GenHashConsoleUnique mixed with console_id) and PSS records passersby by it, so it must not change between
    // sessions. Offline runs keep the default id (the playthrough hashes do not depend on a data dir)
    // an account login that did not happen plays offline (said above)
    let world = world.filter(|_| account.is_some() || account_server.is_none());
    let steam = (world.is_some() && !dev_id && account.is_none())
        .then(|| {
            xr_net::steam::Steam::init()
                .inspect_err(|e| {
                    // offline play goes on as before; the player is told why there is no online
                    log::log(&format!("world: {e}; playing offline"));
                    notice = Some(format!("{e}.\n\nPlaying offline."));
                })
                .ok()
        })
        .flatten();
    let world = world.filter(|_| steam.is_some() || dev_id || account.is_some());
    // a Steam login never goes over TCP to another machine (the ticket is not bound to the server): offline, not a boot failure
    let world = world.filter(|a| {
        let keep = account.is_some() || boot_world_kept(a, steam.is_some(), dev_id);
        if !keep {
            log::log(&format!("world: {a} is a remote TCP world and Steam is on; playing offline (use steam:<id>, or --dev-id)"));
            notice = Some(format!("A Steam login does not go over plain TCP to {a}: use a steam:<id> world.

Playing offline."));
        }
        keep
    });
    let identity = match &steam {
        _ if account.is_some() => account.as_ref().map(|a| xr_net::id::Identity::Account(a.account_id)),
        Some(s) => Some(xr_net::id::Identity::Steam(s.steam_id())),
        None if link.is_some() || world.is_some() => {
            Some(xr_net::id::Identity::Dev(xr_net::id::load_or_create(&data.join("user")).map_err(|e| format!("player id: {e}"))?))
        }
        None => None,
    };
    // an account login that did not happen plays offline (said above)
    let offline = offline || (account_server.is_some() && account.is_none());
    log::log(&match (&account, &world) {
        (Some(a), Some(w)) => format!("mode: online, account {} on {w}", a.account_id),
        (None, Some(w)) => format!("mode: P2P, world {w}"),
        _ if !offline => "mode: P2P, no world at boot (Steam's server list and invites)".to_owned(),
        _ => "mode: offline".to_owned(),
    });
    let console_id = identity.map(xr_net::id::console_id);
    let config = match (&link, console_id) {
        (Some(_), Some(c)) => xr_hle::Config { console_id: c, username: nickname.chars().take(10).collect(), ..config },
        (None, Some(c)) => xr_hle::Config { console_id: c, ..config },
        _ => config,
    };
    let name = name.or(settings.map(|s| s.world_name).filter(|n| !n.is_empty()));
    let world_shown = world.clone();
    // the boot's world is a P2P one: its host's answers never reach this PC's characters (headless runs track it here)
    let mut p2p_net = world.as_deref().is_some_and(|w| p2p_world(w, false));
    // what keeps this run out of worlds (Joiner::start refuses while any is on, unless built with mp-tests)
    let script = script_file.is_some();
    let flags = [("--headless", headless), ("--script", script), ("--press", !presses.is_empty() && !script), ("--deterministic", deterministic && !script), ("--make-preview", make_preview.is_some()), ("--room", link.is_some())];
    let dev = dev_inputs(&flags, std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()));
    // a window may join a world later (the server browser, Steam's "Join game"): Steam is started then if it is not yet
    let mut joiner = Joiner { steam, account, dev_id, name, user, id_dir: data.join("user"), data: data.to_path_buf(), identity, dev, cci: cci.clone() };
    // the world client starts once the game is loaded: the running title says which world (region) and version. A run
    // that may not join (`refusal`) plays offline and says why (a toast in a window)
    let start_net = move |j: &mut Joiner, m: &mut exec::Machine| -> Result<(Option<xr_net::Client>, Option<String>), String> {
        let Some(addr) = &world else { return Ok((None, None)) };
        let modded = m.system().game_modded.clone();
        if let Some(r) = refusal(&j.dev, modded.as_deref(), || j.dump_changed()) {
            log::log(&format!("world: not joining {addr}: {r}; playing offline"));
            return Ok((None, Some(r)));
        }
        j.start(addr, m.title_id, modded.as_deref()).map(|c| (Some(c), None))
    };
    if !headless {
        window.notice = notice;
        window.frames = frames;
        window.scripted = frames.is_some() || !presses.is_empty() || script_file.is_some();
        // one console everywhere: the stable identity's, else the default headless runs use. The window would set its
        // settings' own id, but X writes the saving console's hash (cfg GenHashConsoleUnique) into the save (MyStatus
        // +0x10), so a windowed save would differ from a headless one
        let console = config.console_id;
        let boot = move |config: xr_hle::Config| {
            let config = xr_hle::Config { console_id: console, ..config };
            exec::Machine::boot(&cci, &build, config).and_then(|mut m| {
                m.system().room = link;
                let (net, refused) = start_net(&mut joiner, &mut m)?;
                let world = net.as_ref().and(world_shown);
                let played = played(m.title_id);
                let discord = discord::game_of(m.title_id).map(|g| discord::Handle::start(g, windowed_discord_settings, |t| log::log(t)));
                // Steam from the start when playing P2P or online: friends see "in RE:DEX" and can write (never with
                // --dev-id). Offline is just the player: Steam starts only if they search or join from the game
                if !joiner.dev_id && !offline {
                    let _ = joiner.steam();
                }
                #[cfg(feature = "mp-tests")]
                let friends = xr_net::friends::FakeFriends::from_env().map(|f| Box::new(f) as Box<dyn xr_net::friends::FriendsLink>);
                #[cfg(not(feature = "mp-tests"))]
                let friends = None;
                let (friend_list, ignored, world_names) = (Vec::new(), None, Default::default());
                let (listed, friend_failed) = (None, Vec::new());
                let mut notes: Vec<String> = refused.into_iter().collect();
                // --lobby: the room code is looked up now, joined once found (online())
                let finding = lobby_code.and_then(|code| find_code(&mut joiner, &code).map_err(|why| notes.push(format!("Lobby {}: {why}", redex_proto::room::shown(&code)))).ok());
                Ok(Windowed { m, n: 0, presses, net, joiner, world, pending: None, query: None, presence: None, notes, discord, played, friends, friend_list, ignored, world_names, listed, friend_failed, lobby: lobby::Lobby::default(), finding, lobby_worlds: Default::default() })
            })
        };
        if xr_frontend::run(window, config, boot)? {
            // Restart: the new process truncates latest.log and loadcro.csv and loads the saves, so it starts only
            // once this one has torn the game down and written its last line
            log::log("the window closed, starting the game again");
            progress::write(0);
            if let Err(e) = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn()) {
                eprintln!("could not start the game again: {e}");
            }
            std::process::exit(0);
        }
        log::stop(0, "the window closed");
    }
    let config = xr_hle::Config { hardware_renderer: window.hardware.unwrap_or(false), resolution: window.resolution.unwrap_or(1), ..config };
    // XR_INBETWEENS=DIR: Smooth motion's pictures in between saved (see inbetweens.rs)
    let mut inbetweens = inbetweens::Inbetweens::from_env()?;
    // XR_AUDIO_NO_BGM=1 (headless captures): the sound w/o the game's background music, its effects, cries, fanfares and
    // ambience kept (xr-hle's dsp_bgm.rs); only what is heard changes, never the game
    if std::env::var_os("XR_AUDIO_NO_BGM").is_some_and(|v| v == "1") {
        xr_hle::services::dsp_bgm::set_no_bgm(true);
        log::log("XR_AUDIO_NO_BGM: the background music is left out of the sound");
    }
    let config = if inbetweens.is_some() { inbetweens::Inbetweens::config(config)? } else { config };
    let mut m = exec::Machine::boot(&cci, &build, config)?;
    if let Some(i) = &inbetweens {
        i.start(&mut m);
    }
    m.system().room = link;
    let mut net = start_net(&mut joiner, &mut m)?.0;
    let mut played = played(m.title_id);
    // XR_DISCORD=1 (tests): a headless run shows its status too, with the default settings
    let _discord = std::env::var_os("XR_DISCORD").filter(|v| v != "0").and_then(|_| discord::game_of(m.title_id)).map(|g| {
        let d = discord::Handle::start(g, || discord::Config { status: true, location: true }, |t| log::log(t));
        d.set_mode(if net.is_some() { discord::Mode::Official } else { discord::Mode::Offline });
        d
    });
    let mut fake = FakePeers::from_env();
    // XR_FAKE_FRIENDS (mp-tests): made-up Steam friends whose scripted chat and invites arrive here; a headless game has
    // no Friends tab, so a chat is answered with XR_MP_FRIEND_REPLY and an invite is joined (through Joiner::start)
    #[cfg(feature = "mp-tests")]
    let fake_friends = xr_net::friends::FakeFriends::from_env();
    // XR_SET_FASHION=HEX@FRAME (tests): at that script frame the save's outfit in memory (MyStatus +0x30) becomes the
    // 16 bytes HEX (17: the gender first); the game reads it when it next builds the player (a map change)
    let mut set_fashion = std::env::var("XR_SET_FASHION").ok().and_then(|v| {
        let (h, at) = v.split_once('@')?;
        Some((unhex(h), at.trim().parse::<u64>().ok()?))
    });
    // XR_MP_WHISPER=FRAME:TEXT and XR_MP_REPORT=FRAME:REASON:NOTE (tests; a headless game has no menu): from that script
    // frame on, as soon as another player is on the map, whisper to / report the one with the lowest session (once)
    let split = |v: String| -> Option<(u64, String)> { let (at, rest) = v.split_once(':')?; Some((at.parse().ok()?, rest.to_string())) };
    let mut mp_whisper = std::env::var("XR_MP_WHISPER").ok().and_then(split);
    let mut mp_report = std::env::var("XR_MP_REPORT").ok().and_then(split);
    // XR_MP_HOST=FRAME:MAX:ACCESS[:NAME] (tests; friends, code or public): at that script frame, the overlay's Host lobby;
    // XR_MP_KICK=FRAME: from then on, once someone is in the lobby, the host removes the one with the lowest session (once)
    let mut mp_host = std::env::var("XR_MP_HOST").ok().and_then(split);
    let mut mp_kick = std::env::var("XR_MP_KICK").ok().and_then(|v| v.trim().parse::<u64>().ok());
    let mut lobby = lobby::Lobby::default();
    // --lobby: the room code looked up now, joined once found
    let mut finding = lobby_code.and_then(|code| find_code(&mut joiner, &code).inspect_err(|why| log::log(&format!("lobby: room code {code}: {why}"))).ok());
    // XR_STATE_SAVE_AT=FRAME:PATH writes a save state to PATH between frames once the script clock reaches FRAME (a run
    // stops if a thread is where none can be saved); XR_STATE_LOAD=PATH starts from one before the first frame, the
    // frame count and the script clock going on from its frame (exec.rs Machine::save_state)
    let mut state_save = std::env::var("XR_STATE_SAVE_AT").ok().map(|v| split(v.clone()).ok_or(format!("XR_STATE_SAVE_AT is FRAME:PATH, not {v:?}"))).transpose()?;
    if let Some(p) = &out {
        state_save = Some((PREVIEW_SAVE_AT, p.to_string_lossy().into_owned()));
    }
    let state_load = std::env::var_os("XR_STATE_LOAD").map(PathBuf::from);
    // a state is tied to the xgame.dll that made it; its SHA-256 is read only when one is saved or loaded
    let dll = if state_save.is_some() || state_load.is_some() { redex_builder::dll_fingerprint(&build.join("xgame.dll"))? } else { [0; 32] };
    if fake.is_some() {
        *hooks::PUPPETS.lock().unwrap_or_else(|e| e.into_inner()) = Some(xr_net::x::puppet::Puppets::new());
    }
    // what the hashes depend on besides the script and the clock model: the DLL build and what draws the 3D
    let renderer = m.system().renderer_name.clone();
    let with = format!("build {}, renderer {renderer}", m.build);
    let recorded = script_file.as_ref().and_then(|(_, text)| script::recorded_with(text)).map(str::to_string);
    let differs = recorded.as_ref().filter(|r| **r != with).map(|r| format!(" (the expects were recorded with {r}, this run is {with})"));
    if !checkpoints.is_empty() {
        log::log(&format!("checkpoints: {with}"));
        if let (Some(d), false) = (&differs, record) {
            log::log(&format!("WARNING: hashes may differ{d}"));
        }
    }
    // a battle that never ends stops the script clock (t) forever: give up well after the last checkpoint (one
    // Elite Four battle took about 100k frames)
    let cap = frames.filter(|_| auto.is_some()).map(|f| f + 200_000);
    // XR_PROFILE=1: sample the interpreter's PCs, log the hottest at the end (where native code is still missing)
    let profile = std::env::var_os("XR_PROFILE").is_some();
    if profile {
        m.system().enable_profiler();
    }
    // XR_FRAMESTATS=1: each checkpoint logs how often the top screen got a new picture so far (cadence), see xr_hle::Cadence
    let framestats = std::env::var_os("XR_FRAMESTATS").is_some_and(|v| v != "0");
    // (XR_INBETWEENS tells the top's new pictures apart by it)
    m.system().cadence.on = framestats || inbetweens.is_some();
    // n counts frames; t is the script's clock: the same as n, except that with `autobattle` it waits while a battle is
    // on and the battle gets A (FIGHT, text) and, when the move screen is up, a touch at the move, every 40 frames
    let (mut n, mut t, mut sound, mut seen, mut next) = (0u64, 0u64, wav::Wav::create(&logs.join("audio.wav"))?, Vec::<(String, String)>::new(), 0);
    // autobattle: moves picked so far, this turn's touch, the frame a heal sequence started
    // and the HP last read on the move screen (the command screen's camera hides the HP box)
    // (healing: the frame a heal, PP or replacement sequence started, and which; pp_at: picks at the last PP one)
    // (blind: picks in a row with the HP bar out of view, e.g. another camera angle: after 3, heal to be safe)
    // (which: 0 heal, 1 PP, 2 a fainted lead's replacement)
    let (mut picks, mut touch, mut healing, mut last_hp, mut pp_at, mut blind) = (0u32, None::<(u16, u16)>, None::<(u64, u8)>, None::<u32>, 0u32, 0u32);
    // `faint`: picks since the command screen was last seen, replacements sent so far, the current one's touches
    let (mut since_cmd, mut faints, mut faint_seq) = (0u32, 0usize, Vec::<(u16, u16)>::new());
    // `alt`: this turn's move (repeated by the extra picks a fading move screen gets) and the main move's turns in a row
    let (mut turn_move, mut run) = (None::<(u16, u16)>, 0u32);
    // XR_FRAMETIMES=1: each frame's host time, the world client's part of it, and whether it was online (pacing check)
    let mut frame_times = std::env::var_os("XR_FRAMETIMES").map(|_| Vec::<(u32, u32, bool)>::new());
    // (TICKS at the last change, the frame then, logged): a puppet call that never returns
    let mut stall = (0u64, 0u64, false);
    let (mut explorer, script_end) = (explore::Explorer::from_env(), presses.iter().map(|p| p.at + p.hold).max().unwrap_or(0));
    // wait_until: the next one, and the frame it started waiting
    let (mut next_wait, mut wait_since) = (0usize, None::<u64>);
    let mut battle_since = None::<u64>; // the frame the current battle started
    let mut seen_at = 0u64; // the last frame the autobattle saw the command or the move screen
    let mut use_then = false; // this battle uses the `then` move (the main one had no PP left)
    // `catch`: this battle's foe (species, from its battle record), the balls thrown at it, catches made this run
    let (mut foe, mut throws, mut catches) = (None::<u16>, 0u32, 0u32);
    let input_log = std::env::var_os("XR_INPUTLOG").is_some();
    let console_clock = std::env::var("XR_SCRIPT_CLOCK").is_ok_and(|v| v == "console");
    let mut clock_cycles = 0u64;
    // XR_THUMBS=1 (dev, the Frame rate's timing): each refresh's console time in cycles and both screens shrunk to grey
    // 8x8 blocks (top 50x30, bottom 40x30), appended to logs/thumbs.bin, to line runs at two rates up in time
    let mut thumbs = std::env::var_os("XR_THUMBS").and_then(|_| std::fs::File::create(logs.join("thumbs.bin")).ok()).map(|f| (std::io::BufWriter::new(f), 0u64));
    let peek_at: Vec<u64> = std::env::var("XR_PEEK_AT").unwrap_or_default().split(',').filter_map(|v| v.trim().parse().ok()).collect();
    let pace = realtime.then(std::time::Instant::now);
    if let Some(p) = &state_load {
        let bytes = std::fs::read(p).map_err(|e| format!("XR_STATE_LOAD {p:?}: {e}"))?;
        (n, t) = m.load_state(&bytes, dll).map_err(|e| format!("XR_STATE_LOAD {p:?}: {e}"))?;
        // what the script did before the state's frame is done: its checkpoints up to it, its waits before it
        // shortcut: autobattle's and the explorer's own counters start fresh, so load a state outside a battle
        next = checkpoints.iter().take_while(|c| c.at <= t).count();
        next_wait = waits.iter().take_while(|w| w.at < t).count();
        log::log(&format!("XR_STATE_LOAD: state loaded from {p:?} at frame {n} (script frame {t})"));
    }
    loop {
        let battle = auto.is_some() && bridge::IN_BATTLE.load(Ordering::Relaxed);
        if battle && battle_since.is_none() {
            use_then = false; // a new battle: the main move again (a Pokemon Center may have restored its PP)
            (foe, throws) = (None, 0);
        }
        if !battle && battle_since.is_some() && throws > 0 {
            catches += 1;
            log::log(&format!("autobattle: catch: the battle ended after {throws} ball(s)"));
        }
        // `catch`: who the foe is (its record follows the lead's), read until known; a wanted one is not fled
        let catching = auto.as_ref().and_then(|a| a.catch.as_ref().map(|c| (a, c))).filter(|_| battle && catches == 0);
        if let Some((a, _)) = catching.filter(|_| foe.is_none() && n % 40 == 0) {
            foe = lead_record(&mut m.system().memory, &a.mon).and_then(|(at, _)| {
                let mut b = [0u8; 2];
                b.copy_from_slice(m.system().memory.slice_mut(at + 0x244, 2)?);
                Some(u16::from_le_bytes(b)).filter(|&f| f != 0)
            });
            if let Some(f) = foe {
                log::log(&format!("autobattle: catch: the foe is species {f}"));
            }
        }
        let wanted = catching.is_some_and(|(_, c)| foe.is_some_and(|f| c.0.contains(&f)));
        let unknown = catching.is_some() && foe.is_none() && battle_since.is_some_and(|b| n - b < 900);
        battle_since = if battle { battle_since.or(Some(n)) } else { None };
        if (explorer.is_some() || auto.as_ref().is_some_and(|a| a.flee)) && battle_since.is_some_and(|b| n - b == 1200) {
            log::log("autobattle: a battle still on after 20 s of RUN (a trainer's): fighting it");
        }
        // a pressed button stays down for 8 frames, long enough for any game to see it
        let input = match &mut explorer {
            // XR_EXPLORE_FIGHT=1: battles while exploring are fought with the autobattle line (trainers cannot be fled)
            // a battle still on after 20 s of RUN is a trainer's (RUN is refused): fight it
            // (`autobattle flee` does the same in a script, so that a walk the explorer found replays as it went)
            e if battle
                && (e.is_some() && t >= script_end && std::env::var_os("XR_EXPLORE_FIGHT").is_none() || auto.as_ref().is_some_and(|a| a.flee))
                && battle_since.is_some_and(|b| n - b < 1200)
                && !wanted =>
            {
                if unknown {
                    InputState::default() // the foe's record is not read yet: no RUN before it is
                } else {
                if let Some(e) = e {
                    e.interrupt();
                }
                explore::flee(n, battle_since.is_some_and(|b| n - b >= 1040))
                }
            }
            _ if battle => {
                let a = auto.as_ref().unwrap();
                let seq = match healing {
                    Some((_, 1)) => a.pp.as_ref().map(|p| &p.0[..]),
                    Some((_, 2)) => Some(&faint_seq[..]),
                    Some((_, 3)) => a.catch.as_ref().map(|c| &c.1[..]),
                    _ => a.heal.as_ref().map(|h| &h.0[..]),
                }
                .unwrap_or(&[]);
                match healing.map(|(s, _)| n - s) {
                    // 150 frames between touches: the bag's screens slide in
                    Some(d) if d < 150 * seq.len() as u64 + 60 => {
                        let i = (d / 150) as usize;
                        InputState { touch: (d % 150 < 8 && i < seq.len()).then(|| seq[i]), ..Default::default() }
                    }
                    _ => {
                        healing = None;
                        // before the turn's A (which would leave the command screen): heal if low, else restore PP
                        if n % 40 == 0 && command_screen(&m.screen(zakuro_common::Screen::Bottom)) {
                            (since_cmd, seen_at) = (0, n);
                            if let Some(used) = turn_move.take() {
                                run = if used == a.mv { run + 1 } else { 0 };
                            }
                            if wanted && throws < 5 {
                                healing = Some((n, 3));
                                throws += 1;
                                log::log(&format!("autobattle: catch: ball {throws} at species {} at frame {n} (script frame {t})", foe.unwrap_or(0)));
                            } else if a.heal.as_ref().is_some_and(|h| {
                                last_hp.is_some_and(|hp| hp < h.1) || blind >= 3 && (!a.newer() || last_hp.is_some_and(|hp| hp < h.1 + 30))
                            }) {
                                healing = Some((n, 0));
                                log::log(&format!("autobattle: hp {last_hp:?}% ({blind} picks blind) -> heal"));
                                (last_hp, blind) = (None, 0);
                            } else if a.pp.as_ref().is_some_and(|p| picks - pp_at >= p.1) {
                                healing = Some((n, 1));
                                log::log(&format!("autobattle: {} picks since the last PP restore -> restore", picks - pp_at));
                                pp_at = picks;
                            }
                        }
                        if n % 40 == 20 {
                            // no touch elsewhere: a touch on a screen that is about to change lands on the next one
                            touch = None;
                            // (`flee`/`mon` lines) a double battle's target screen (it passes the move screen's test):
                            // the first foe
                            if a.newer() && a.faint.is_none() && fainted_panel(&m.screen(zakuro_common::Screen::Bottom)) {
                                // (`flee`/`mon` lines without `faint`) the lead's "cannot battle" panel: it passes the move
                                // screen's test, and its touches had looped forever (Connecting Cave's hordes). The next
                                // party slot in turn; a fainted one shows this panel again and the one after is tried
                                let slot = PARTY_SLOTS[faints % PARTY_SLOTS.len()];
                                faint_seq = vec![(298, 220), (298, 220), slot, (160, 90)];
                                (healing, since_cmd, last_hp, blind) = (Some((n, 2)), 0, None, 0);
                                faints += 1;
                                use_then |= a.then.is_some();
                                log::log(&format!("autobattle: the lead cannot battle -> send out slot {slot:?}"));
                            } else if a.newer() && target_screen(&m.screen(zakuro_common::Screen::Bottom)) {
                                touch = Some(FIRST_FOE);
                                log::log("autobattle: a double battle's target screen -> the first foe");
                            } else if move_screen(&m.screen(zakuro_common::Screen::Bottom), a.newer(), a.newer() && a.heal.is_some()) {
                                touch = Some(match (a.then, a.alt) {
                                    // `alt X,Y N`: after N-1 turns of the main move (Torment forbids it twice in a row);
                                    // by turns, not picks: a move screen fading out gets a second pick
                                    (_, Some((m, every))) => *turn_move.get_or_insert(if run + 1 >= every { m } else { a.mv }),
                                    (Some((t, after)), _) if picks >= after || use_then => t,
                                    _ => a.mv,
                                });
                                (picks, seen_at) = (picks + 1, n);
                                let hp = match &a.mon[..] {
                                    [] => hp_bar(&m.screen(zakuro_common::Screen::Top)),
                                    species => hp_mem(&mut m.system().memory, species),
                                };
                                last_hp = hp.or(last_hp);
                                blind = if hp.is_some() { 0 } else { blind + 1 };
                                log::log(&format!("autobattle: hp {hp:?}% -> touch {touch:?} (pick {picks})"));
                                // picks with no command screen between: the lead fainted, and the A opened its "cannot
                                // battle" panel; back (twice: a message may cover the button), the next slot, send it out
                                since_cmd += 1;
                                if let Some((slots, after)) = a.faint.as_ref().filter(|f| since_cmd >= f.1 && !f.0.is_empty()) {
                                    let slot = slots[faints % slots.len()];
                                    faint_seq = vec![(298, 220), (298, 220), slot, (160, 90)];
                                    (healing, touch, since_cmd, last_hp, blind) = (Some((n, 2)), None, 0, None, 0);
                                    faints += 1;
                                    use_then |= a.newer() && a.then.is_some(); // (newer lines) the next Pokemon uses the `then` move
                                    log::log(&format!("autobattle: {after} picks without the command screen: fainted? -> send out slot {slot:?}"));
                                } else if a.newer() && since_cmd >= 6 {
                                    // a screen taken for the move screen (a Bag pocket left open when a heal lost a touch), or
                                    // a move with no PP left (its touch is refused): back out, and use the `then` move this battle
                                    (touch, since_cmd) = (Some((298, 220)), 0);
                                    use_then |= a.then.is_some();
                                    log::log("autobattle: 6 picks without the command screen -> back (and the `then` move, if any)");
                                }
                            }
                        }
                        match () {
                            _ if healing.is_some() => InputState::default(),
                            // (`flee`/`mon` lines) "Should a move be deleted and replaced with X?": KEEP OLD MOVES, then
                            // "Give up on learning X?": YES. The turn's A picks FORGET, and the move list after it took the
                            // autobattle's touches (a stall)
                            _ if a.newer() && n % 40 < 8 && learn_prompt(&m.screen(zakuro_common::Screen::Bottom)).is_some() => {
                                let touch = learn_prompt(&m.screen(zakuro_common::Screen::Bottom));
                                if n % 40 == 0 {
                                    log::log(&format!("autobattle: a move to learn and four known: {}", if touch == Some(KEEP_OLD_MOVES) { "keep the old moves" } else { "give up learning it" }));
                                }
                                InputState { touch, ..Default::default() }
                            }
                            // (`flee` lines) neither screen for 30 s, e.g. an empty Bag pocket after the last Potion: back
                            _ if a.newer() && n.saturating_sub(seen_at.max(battle_since.unwrap_or(n))) > 1800 && n % 80 < 8 => {
                                InputState { touch: Some((298, 220)), ..Default::default() }
                            }
                            // (scripts with `faint`) no A on the move screen: it picks the cursor's move, the last one
                            // used, which Torment refuses, and the refusal message then eats the touch: a loop
                            // (`flee` lines too: the cursor stays on the first move, Scratch, until a touch picks another)
                            _ if (a.faint.is_some() || a.newer()) && n % 40 < 8 && move_screen(&m.screen(zakuro_common::Screen::Bottom), a.newer(), a.newer() && a.heal.is_some()) => InputState::default(),
                            // after a ball: B (it answers the nickname question No and goes through the rest)
                            _ if throws > 0 => InputState { buttons: if n % 40 < 8 { PadState::B } else { PadState::empty() }, ..Default::default() },
                            _ => battle_input(n, touch),
                        }
                    }
                }
            }
            Some(e) if t >= script_end => {
                let mut b = [0u8; 12];
                if let Some(s) = m.system().memory.slice_mut(e.addr, 12) {
                    b.copy_from_slice(s);
                }
                let f = |i: usize| f32::from_le_bytes(b[i..i + 4].try_into().unwrap());
                // the map (zone) id sits 0x20 bytes before the position in the same snapshot (x-field-move-models.md)
                if let Some(s) = m.system().memory.slice_mut(e.addr.wrapping_sub(0x20), 2) {
                    e.map = u16::from_le_bytes([s[0], s[1]]);
                }
                // a `W` goal (world units, Lumiose's streets): the player's and the goal's top-screen pixels
                if let Some((gx, gz)) = e.world_goal() {
                    let (p, g) = ([f(0), f(4), f(8)], [gx, f(4), gz]);
                    let cam = field_data(&mut m).and_then(|d| xr_net::x::field::camera(&mut Mem(&mut m.system().memory), d));
                    e.screen = cam.and_then(|c| Some((c.project(p)?, c.project(g))));
                }
                e.input(Some((f(0), f(8))), t)
            }
            _ => input_at(&presses, t),
        };
        // XR_INPUTLOG=1: each frame's scripted input, to find where two runs part
        if input_log && (!input.buttons.is_empty() || input.touch.is_some()) {
            log::log(&format!("input {n} t{t} b{} {:?} {:?}", u8::from(battle), input.buttons, input.touch));
        }
        m.set_input(input);
        let start = frame_times.as_ref().map(|_| std::time::Instant::now());
        let outcome = m.run_frame();
        let hook = std::time::Instant::now();
        if let Some(c) = &mut net {
            net_frame(&mut m, c);
        }
        if let Some(c) = &mut net {
            keep_accounts(c, data, joiner.account.is_some() || p2p_net);
            while let Some(w) = c.whispers.pop_front() {
                log::log(&format!("whisper from session {} ({}): {}", w.from_session, w.name, w.text));
            }
            let first = c.peers.iter().min_by_key(|(s, _)| **s).map(|(s, r)| (*s, r.steam, r.name.clone()));
            if let (Some((to, _, name)), Some((_, text))) = (&first, mp_whisper.take_if(|(at, _)| first.is_some() && t >= *at)) {
                log::log(&format!("XR_MP_WHISPER to session {to} ({name}): {}", if c.whisper(*to, &text) { "sent" } else { "not sent" }));
            }
            if let (Some((to, steam, name)), Some((_, arg))) = (&first, mp_report.take_if(|(at, _)| first.is_some() && t >= *at)) {
                let (reason, note) = arg.split_once(':').unwrap_or((&arg, ""));
                let ok = c.report(*to, *steam, reason.parse().unwrap_or(0), note);
                log::log(&format!("XR_MP_REPORT session {to} ({name}, steam {steam}): {}", if ok { "sent" } else { "not sent" }));
            }
        }
        #[cfg(feature = "mp-tests")]
        if let Some(f) = &fake_friends {
            use xr_net::friends::{FriendMsg, FriendsLink};
            f.frame(t);
            for (from, msg) in f.take() {
                match msg {
                    FriendMsg::Chat(text) => {
                        log::log(&format!("friends: chat from {from}: {text}"));
                        if let Ok(reply) = std::env::var("XR_MP_FRIEND_REPLY") {
                            let sent = f.send(from, &FriendMsg::Chat(reply)).is_ok();
                            log::log(&format!("friends: reply to {from}: {}", if sent { "sent" } else { "not sent" }));
                        }
                    }
                    FriendMsg::Invite { world, connect } => {
                        log::log(&format!("friends: invite from {from} to {world:?} ({connect})"));
                        let modded = m.system().game_modded.clone();
                        match joiner.start(&connect, m.title_id, modded.as_deref()) {
                            Ok(c) => (net, p2p_net) = (Some(c), p2p_world(&connect, false)),
                            Err(e) => log::log(&format!("world: cannot join {connect}: {e}")),
                        }
                    }
                    FriendMsg::Decline => log::log(&format!("friends: {from} declined")),
                }
            }
        }
        if let Some((_, arg)) = mp_host.take_if(|(at, _)| t >= *at) {
            let mut p = arg.splitn(3, ':');
            let max = p.next().and_then(|v| v.parse().ok()).unwrap_or(4);
            let access = match p.next() {
                Some("public") => xr_frontend::online::Access::Public,
                Some("code") => xr_frontend::online::Access::Code,
                _ => xr_frontend::online::Access::Friends,
            };
            let s = xr_frontend::online::LobbySettings { name: p.next().unwrap_or_default().to_owned(), max, access };
            if let Err(why) = host_lobby(&mut lobby, &mut joiner, &mut m, net.is_some(), &s) {
                log::log(&format!("lobby: not hosting: {why}"));
            }
        }
        if let Some(Ok((address, _))) = lobby_news(&mut lobby) {
            let modded = m.system().game_modded.clone();
            match joiner.start(&address, m.title_id, modded.as_deref()) {
                Ok(c) => (net, p2p_net) = (Some(c), true),
                Err(e) => {
                    log::log(&format!("world: cannot join {address}: {e}"));
                    lobby.close();
                    log::log("lobby: closed (the host's own game could not join it)");
                }
            }
        }
        let first = lobby.view().and_then(|v| v.members.first().cloned());
        if let (Some((session, name)), Some(_)) = (&first, mp_kick.take_if(|at| first.is_some() && t >= *at)) {
            log::log(&format!("lobby: remove session {session} ({name}): {}", if lobby.kick(*session) { "removed" } else { "not in the lobby" }));
        }
        if let Some(r) = finding.as_ref().and_then(|f| lobby::answered(&f.1)) {
            let (code, _) = finding.take().unwrap();
            match r {
                Ok(addr) => {
                    log::log(&format!("lobby: room code {code} is {addr}"));
                    let modded = m.system().game_modded.clone();
                    match joiner.start(&addr, m.title_id, modded.as_deref()) {
                        Ok(c) => (net, p2p_net) = (Some(c), true),
                        Err(e) => log::log(&format!("world: cannot join {addr}: {e}")),
                    }
                }
                Err(why) => log::log(&format!("lobby: room code {code}: {why}")),
            }
        }
        // refused for good (removed from a lobby, the lobby closed, a ban): out of that world, offline
        if let Some(why) = net.as_ref().filter(|n| n.stopped).map(|n| n.refused.clone().unwrap_or_else(|| "the connection ended".into())) {
            log::log(&format!("world: left the world: {why}; playing offline"));
            net = None;
            clear_puppets(&mut m);
            if lobby.close() {
                log::log("lobby: closed (the host's own game left it)");
            }
        }
        if let Some((bytes, _)) = set_fashion.take_if(|(b, at)| t >= *at && matches!(b.len(), 16 | 17)) {
            let at = field_data(&mut m).and_then(|d| mystatus_fashion(&mut m.system().memory, d));
            let done = at.and_then(|a| m.system().memory.slice_mut(a, 16)).map(|s| s.copy_from_slice(&bytes[bytes.len() - 16..])).is_some();
            // 17 bytes: the gender first, with the sprite bytes the intro sets for it (a girl in a boy's save, for tests)
            if let (Some(a), [g, ..], 17) = (at, bytes.as_slice(), bytes.len()) {
                use xr_net::x::field::{MS_FASHION, MS_GENDER};
                if let Some(s) = m.system().memory.slice_mut(a - MS_FASHION + MS_GENDER, 3) {
                    s.copy_from_slice(&[*g, u8::from(*g == 0), u8::from(*g == 0)]);
                }
            }
            log::log(&format!("XR_SET_FASHION at frame {t}: {}", if done { "MyStatus outfit written" } else { "no MyStatus" }));
        }
        if let Some(p) = &mut played {
            put_character(p.frame(&mut m), net.as_ref());
        }
        if let Some(f) = &mut fake {
            let peers = player(&mut m).map(|(p, _)| (p.map, f.peers(t, &p)));
            if let (Some((map, peers)), Some(mut lock)) = (peers, hooks::try_puppets()) {
                if let Some(pp) = lock.as_mut() {
                    // online too (the pair tests' third player): beside the world's players, not instead of them
                    if net.is_some() { pp.want_also(map, peers) } else { pp.want(map, peers) }
                }
            }
        }
        if (net.is_some() || fake.is_some()) && n == 900 && hooks::TICKS.load(Ordering::Relaxed) == 0 {
            log::log("WARNING: puppets: xgame.dll was built without the puppet hooks (functions/<build>/hooks.txt); rebuild it to see other players");
        }
        // the game's main thread stuck inside a puppet call (the fatal loop is a hang): say where, once
        if net.is_some() || fake.is_some() {
            let ticks = hooks::TICKS.load(Ordering::Relaxed);
            let call = hooks::IN_CALL.load(Ordering::Relaxed);
            if ticks != stall.0 {
                stall = (ticks, n, false);
            } else if call != 0 && n - stall.1 > 300 && !stall.2 {
                stall.2 = true;
                log::log(&format!("WARNING: puppets: the game's main thread has been inside code.bin 0x{call:08X} (a puppet call) for {} frames", n - stall.1));
            }
        }
        if let (Some(f), Some(s)) = (&mut frame_times, start) {
            f.push((s.elapsed().as_micros() as u32, hook.elapsed().as_micros() as u32, net.as_ref().is_some_and(|c| c.online)));
        }
        if outcome == FrameOutcome::Completed {
            n += 1;
            // XR_PEEK_AT=N,...: XR_PEEK/XR_MEMDUMP at these frames too (a battle stops the script clock, so no checkpoint there)
            if peek_at.contains(&n) {
                peek(&mut m, &logs, &format!("f{n}"));
            }
            // a wait_until that holds lets the script clock go on; one that never does ends the run
            while let Some(w) = waits.get(next_wait).filter(|w| w.at <= t) {
                let since = *wait_since.get_or_insert(n);
                if holds(&mut m, w) {
                    log::log(&format!("wait_until {} at frame {n}: true after {} frames", w.text, n - since));
                    (next_wait, wait_since) = (next_wait + 1, None);
                } else if n - since >= w.timeout {
                    save_screens(&mut m, &logs, "");
                    log::stop(4, &format!("wait_until {}: still false after {} frames", w.text, w.timeout));
                } else {
                    break;
                }
            }
            let waiting = waits.get(next_wait).is_some_and(|w| w.at <= t);
            // XR_SCRIPT_CLOCK=console: the script's frames are console refreshes of time, so a Frame rate over 60 (its
            // refreshes shorter) runs a script at the same pace in time as 30 does
            if !battle && !waiting {
                if console_clock {
                    clock_cycles += m.system().frame_cycles;
                    t += clock_cycles / xr_hle::CYCLES_PER_FRAME;
                    clock_cycles %= xr_hle::CYCLES_PER_FRAME;
                } else {
                    t += 1;
                }
            }
            if let Some((f, at)) = thumbs.as_mut() {
                use std::io::Write;
                *at += m.system().frame_cycles;
                let mut out = at.to_le_bytes().to_vec();
                for (s, w) in [(zakuro_common::Screen::Top, 400usize), (zakuro_common::Screen::Bottom, 320)] {
                    let px = m.screen(s);
                    for by in 0..30 {
                        for bx in 0..w / 8 {
                            let mut sum = 0u32;
                            for y in by * 8..by * 8 + 8 {
                                for x in bx * 8..bx * 8 + 8 {
                                    let i = (y * w + x) * 4;
                                    sum += px.get(i..i + 3).map_or(0, |p| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]));
                                }
                            }
                            out.push((sum / 192) as u8);
                        }
                    }
                }
                let _ = f.write_all(&out);
            }
            if let Some(start) = pace {
                // 60 frames a second from the start, so a slow frame is made up and the run does not drift
                let due = start + std::time::Duration::from_secs_f64(n as f64 / 60.0);
                if let Some(d) = due.checked_duration_since(std::time::Instant::now()) {
                    std::thread::sleep(d);
                }
            }
        }
        if cap.is_some_and(|c| n >= c) {
            save_screens(&mut m, &logs, "");
            log::stop(4, &format!("autobattle: {n} frames ran and the script clock is still at {t} of {} (a battle that never ends?)", frames.unwrap_or(0)));
        }
        let audio = m.take_audio();
        sound.push(&audio);
        if let Some(i) = inbetweens.as_mut().filter(|_| outcome == FrameOutcome::Completed) {
            i.frame(&mut m, t, n, &audio);
        }
        let done = frames.is_some_and(|f| t >= f);
        status(&mut m, n, done || outcome != FrameOutcome::Completed);
        let last = done || outcome != FrameOutcome::Completed;
        log::WHERE.lock().unwrap().0 = n;
        while outcome == FrameOutcome::Completed && checkpoints.get(next).is_some_and(|c| c.at <= t) {
            let c = &checkpoints[next];
            next += 1;
            let h = script::hash(&m.screen(zakuro_common::Screen::Top), &m.screen(zakuro_common::Screen::Bottom));
            save_screens(&mut m, &logs, &format!("-cp-{}", c.name));
            log::log(&format!("checkpoint {} at frame {n}: {h}", c.name));
            if framestats {
                log::log(&format!("cadence {} at frame {n}: {}", c.name, m.system().cadence.line()));
            }
            log::WHERE.lock().unwrap().1 = c.name.clone();
            peek(&mut m, &logs, &c.name);
            match &c.expect {
                Some(e) if !record && *e != h => {
                    save_screens(&mut m, &logs, "");
                    log::stop(4, &format!("checkpoint {} at frame {n}: expected {e}, got {h}{}", c.name, differs.as_deref().unwrap_or("")));
                }
                _ => seen.push((c.name.clone(), h)),
            }
        }
        if let Some((at, p)) = state_save.take_if(|(at, _)| outcome == FrameOutcome::Completed && t >= *at) {
            let state = m.save_state((n, t), dll).map_err(|e| format!("XR_STATE_SAVE_AT {at}: no state at frame {n} (script frame {t}): {e}"))?;
            std::fs::write(&p, &state).map_err(|e| format!("XR_STATE_SAVE_AT {p:?}: {e}"))?;
            log::log(&format!("XR_STATE_SAVE_AT: state saved to {p:?} at frame {n} (script frame {t}), {} bytes", state.len()));
            if let Some(g) = make_preview {
                // the player's move model says who stands in the bedroom: 2 a boy, 1 a girl
                let want = if g == "boy" { 2 } else { 1 };
                let model = player(&mut m).and_then(|(p, mem)| xr_net::Guest::u16(&mut Mem(mem), p.move_model + xr_net::x::field::MM_MODEL));
                if model != Some(want) {
                    let _ = std::fs::remove_file(&p);
                    log::stop(1, &format!("the preview state is not in the bedroom as a {g} (move model {model:?}, wanted {want})"));
                }
                log::log(&format!("preview state: {g}, model {want}"));
            }
        }
        if make_preview.is_some() && n.is_multiple_of(100) {
            println!("progress {n}/{PREVIEW_FRAMES}");
        }
        if last || every.is_some_and(|e| n.is_multiple_of(e)) {
            let tag = if last { String::new() } else { format!("-{n:05}") };
            save_screens(&mut m, &logs, &tag);
        }
        // XR_THREADS=1: at the end, what every live guest thread waits on (an event that never fires shows up here)
        if last && lobby.close() {
            log::log("lobby: closed (the game is ending)");
        }
        if last && std::env::var_os("XR_THREADS").is_some() {
            let s = m.system();
            for id in 0..s.kernel.threads.len() {
                let t = &s.kernel.threads[id];
                if t.status != xr_hle::kernel::thread::ThreadStatus::Dead {
                    let (pc, lr) = (t.context.regs[15], t.context.regs[14]);
                    log::log(&format!("thread {} {:?} pc 0x{pc:08X} lr 0x{lr:08X}: {}", t.name, t.status, s.kernel.describe_wait(id as _)));
                }
            }
        }
        if last {
            sound.finish("audio");
            if let Some(i) = inbetweens.as_mut() {
                i.finish();
            }
            if let Some(f) = &frame_times {
                let rows = f.iter().enumerate().map(|(i, (us, net, on))| format!("{i},{us},{net},{}\n", *on as u8));
                let _ = std::fs::write(logs.join("frametimes.csv"), std::iter::once("frame,us,net_us,online\n".to_string()).chain(rows).collect::<String>());
            }
            for (thread, pc, hits) in m.system().hot_spots(if profile { 40 } else { 0 }) {
                log::log(&format!("profile: 0x{pc:08X} {hits} samples ({thread})"));
            }
        }
        if last && record {
            // a partial run would drop the expects of the checkpoints it never reached
            if let Some(c) = checkpoints.get(next) {
                log::stop(4, &format!("--record-expects: the run ended ({outcome:?}) before checkpoint {}, so the script is left as it was", c.name));
            }
            if let Some((f, text)) = &script_file {
                std::fs::write(f, script::record(text, &seen, &with)).map_err(|e| format!("{f:?}: {e}"))?;
                log::log(&format!("recorded {} expects in {f:?}", seen.len()));
            }
        }
        if outcome == FrameOutcome::Exited && next < checkpoints.len() {
            log::stop(4, &format!("the game exited before checkpoint {}", checkpoints[next].name));
        }
        // the run ends without dropping the renderer: its pipelines and translated programs are kept for the next one
        if outcome == FrameOutcome::Exited || (outcome == FrameOutcome::Completed && done) {
            m.system().gpu.save_caches();
        }
        match outcome {
            FrameOutcome::Exited => log::stop(0, "the game exited"),
            FrameOutcome::Faulted => log::stop(5, "the game stopped on svcBreak"),
            FrameOutcome::Completed if done => log::stop(0, &format!("ran {n} frames (--frames)")),
            FrameOutcome::Completed => {}
        }
    }
}

/// Checkpoint diagnostics for scripting: XR_MEMDUMP=1 (or =BASE in hex) writes 128 MiB from 0x08000000, the app heap
/// (or from BASE; zeros where unmapped) to `<logs>/mem-cp-NAME.bin`, for diffing two checkpoints; XR_PEEK=ADDR,... (hex) logs 4 words at each
/// address as u32 and f32 (e.g. the player's position once a diff has found it).
fn peek(m: &mut exec::Machine, logs: &std::path::Path, name: &str) {
    let mem = &mut m.system().memory;
    if let Ok(v) = std::env::var("XR_MEMDUMP") {
        let base = u32::from_str_radix(v.trim_start_matches("0x"), 16).ok().filter(|&b| b > 1).unwrap_or(0x0800_0000);
        let mut out = vec![0u8; 0x0800_0000];
        for (i, page) in out.chunks_mut(0x1000).enumerate() {
            if let Some(s) = mem.slice_mut(base.wrapping_add((i as u32) * 0x1000), 0x1000) {
                page.copy_from_slice(s);
            }
        }
        let _ = std::fs::write(logs.join(format!("mem-cp-{name}.bin")), out);
    }
    for a in std::env::var("XR_PEEK").unwrap_or_default().split(',').filter_map(|a| u32::from_str_radix(a.trim().trim_start_matches("0x"), 16).ok()) {
        let mut b = [0u8; 16];
        if let Some(s) = mem.slice_mut(a, 16) {
            b.copy_from_slice(s);
        }
        let w: Vec<u32> = b.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let f: Vec<String> = w.iter().map(|&x| format!("{:.2}", f32::from_bits(x))).collect();
        log::log(&format!("peek {name} 0x{a:08X}: {:08X?} f32 {}", w, f.join(" ")));
    }
    field(m, name);
}

/// Whether a `wait_until` condition holds now (a field condition is false without the field).
fn holds(m: &mut exec::Machine, w: &script::Wait) -> bool {
    use script::What;
    match &w.what {
        What::Puppets(v) => player(m)
            .and_then(|(p, mem)| xr_net::x::field::slots(&mut Mem(mem), p.manager))
            .is_some_and(|s| w.op.holds(s.iter().filter(|s| s.id >= xr_net::x::puppet::ID_BASE).count() as u64, *v)),
        What::Map(v) => player(m).is_some_and(|(p, _)| w.op.holds(u64::from(p.map), *v)),
        What::Word(a, v) => m.system().memory.slice_mut(*a, 4).is_some_and(|s| w.op.holds(u64::from(u32::from_le_bytes([s[0], s[1], s[2], s[3]])), *v)),
        What::Hash(h) => w.op.holds(&script::hash(&m.screen(zakuro_common::Screen::Top), &m.screen(zakuro_common::Screen::Bottom)), h),
    }
}

/// Logs the player's field state at a checkpoint through xr-net's accessors (read only), when DllField is loaded.
fn field(m: &mut exec::Machine, name: &str) {
    let Some(data) = field_data(m) else {
        // no field (a battle, the boutique's fitting room): the dress-up loader's two player heaps still say who holds one
        let mut mm = Mem(&mut m.system().memory);
        let base = xr_net::Guest::u32(&mut mm, xr_net::x::wardrobe::PLAYER_HEAPS).unwrap_or(0);
        let ph = [0x2C, 0x30].map(|o| xr_net::Guest::u32(&mut mm, base + o).unwrap_or(0));
        log::log(&format!("no field {name}: player heaps 0x{:08X} 0x{:08X} (globals 0x{base:08X})", ph[0], ph[1]));
        return;
    };
    let mem = &mut m.system().memory;
    let Some(p) = xr_net::x::field::player(&mut Mem(mem), data) else { return };
    let slots = xr_net::x::field::slots(&mut Mem(mem), p.manager).map_or("?".into(), |s| s.len().to_string());
    log::log(&format!(
        "field {name}: map {} facing {:?} mode {:?} moving {} pos {:.2},{:.2},{:.2} move model 0x{:08X}, {slots}/32 slots used",
        p.map, p.facing, p.mode, p.moving, p.pos[0], p.pos[1], p.pos[2], p.move_model
    ));
    // XR_SLOTS=1: every field object's id, model and tile (who stands where: Parfum Palace's Furfrou, Shauna)
    if std::env::var_os("XR_SLOTS").is_some() {
        use xr_net::Guest;
        let s: Vec<String> = xr_net::x::field::slots(&mut Mem(mem), p.manager).unwrap_or_default().iter().filter_map(|s| {
            let at = s.addr + xr_net::x::field::MM_POS;
            let (x, z) = (Mem(mem).f32(at)?, Mem(mem).f32(at + 8)?);
            Some(format!("{}/{} {},{}", s.id, s.model, (x / 18.0).floor(), (z / 18.0).floor()))
        }).collect();
        log::log(&format!("slots {name}: {}", s.join(" ")));
    }
    if let Some(a) = xr_net::x::field::appearance(&mut Mem(mem), data) {
        let hex: String = a.fashion.iter().map(|b| format!("{b:02x}")).collect();
        log::log(&format!("appearance {name}: gender {} fashion {hex} valid {}", a.gender, a.validate()));
    }
    // the trainer name in MyStatus (what a --character's new game writes)
    if let Some(at) = mystatus_fashion(mem, data) {
        let mut b = [0u8; 26];
        if xr_net::Guest::read(&mut Mem(mem), at - xr_net::x::field::MS_FASHION + xr_net::x::field::MS_NAME, &mut b) {
            let units: Vec<u16> = b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
            log::log(&format!("trainer {name}: name {:?}", String::from_utf16_lossy(&units)));
        }
    }
    // where the camera puts the player's feet on the top screen (1x pixels)
    let at = xr_net::x::field::camera(&mut Mem(mem), data).and_then(|c| c.project(p.pos));
    log::log(&format!("camera {name}: player at {}", at.map_or("?".into(), |s| format!("{:.1},{:.1}", s[0], s[1]))));
    // the puppets in the pool (ids from 0x8000), as the game has them
    let slots = xr_net::x::field::slots(&mut Mem(mem), p.manager).unwrap_or_default();
    // where xr-net last put each puppet (the game can move a model itself between our moves)
    let set = hooks::try_puppets().and_then(|p| p.as_ref().map(|p| p.shown())).unwrap_or_default();
    let shown: Vec<String> = slots
        .iter()
        .filter(|s| s.id >= xr_net::x::puppet::ID_BASE)
        .map(|s| {
            let mut m = Mem(mem);
            let pos = [0, 4, 8].map(|o| xr_net::Guest::f32(&mut m, s.addr + xr_net::x::field::MM_POS + o).unwrap_or(f32::NAN));
            let ours = set.iter().find(|k| k.1 == s.addr).map_or(String::new(), |k| format!(" (set {:.1},{:.1},{:.1})", k.2[0], k.2[1], k.2[2]));
            format!("0x{:04X} model {} {} at {:.1},{:.1},{:.1}{ours}", s.id, s.model, if s.saved { "SAVED" } else { "unsaved" }, pos[0], pos[1], pos[2])
        })
        .collect();
    if !shown.is_empty() || hooks::try_puppets().is_none_or(|p| p.is_some()) {
        // pool puppets beside the ones xr-net knows: more in the pool than known is an orphan
        let known = hooks::try_puppets().and_then(|p| p.as_ref().map(|p| p.shown().len().to_string())).unwrap_or("?".into());
        log::log(&format!("puppets {name}: {} shown ({known} known){}{}", shown.len(), if shown.is_empty() { "" } else { ": " }, shown.join("; ")));
        if let Some(mo) = hooks::try_puppets().and_then(|p| p.as_ref().map(|p| p.motion)) {
            log::log(&format!("motion {name}: {} steps, longest {:.2} a frame (walk {:.2}, run {:.2}), {} jumps, {} reversals faster than walking", mo.steps, mo.max_step, xr_net::x::puppet::speed(redex_proto::MODE_WALK), xr_net::x::puppet::speed(redex_proto::MODE_RUN), mo.jumps, mo.reversals));
        }
        // the dress-up slots (request/state per slot; the local player holds one): outfits xr-net asked for and released
        let mut mm = Mem(mem);
        let sys = xr_net::Guest::u32(&mut mm, p.manager + xr_net::x::puppet::MGR_MODEL_SYS).unwrap_or(0);
        let used: Vec<String> = (0..xr_net::x::puppet::DRESS_SLOTS)
            .filter_map(|i| xr_net::Guest::u16(&mut mm, sys + xr_net::x::puppet::SYS_DRESS_SLOTS + i * xr_net::x::puppet::DRESS_SLOT_SIZE).filter(|&w| w != 0).map(|w| format!("{i}:{}/{}", w & 0xFF, w >> 8)))
            .collect();
        // the dress-up loader's two player heaps (0: free; the local player's outfit holds one, a girl puppet's the other)
        let base = xr_net::Guest::u32(&mut mm, xr_net::x::wardrobe::PLAYER_HEAPS).unwrap_or(0);
        let ph = [0x2C, 0x30].map(|o| xr_net::Guest::u32(&mut mm, base + o).unwrap_or(0));
        log::log(&format!("dress {name}: {} slots in use (slot:request/state) {} (model system 0x{sys:08X}), player heaps 0x{:08X} 0x{:08X} (globals 0x{base:08X})", used.len(), used.join(" "), ph[0], ph[1]));
        let heap = xr_net::x::heap::free(&mut Mem(mem), &xr_net::x::heap::MODEL_HEAPS);
        let each = |h: Vec<xr_net::x::heap::Free>| h.iter().zip(xr_net::x::heap::MODEL_HEAPS).map(|(f, id)| format!("0x{id:X} {} free largest {}", f.total, f.largest)).collect::<Vec<_>>().join(", ");
        log::log(&format!("heap {name}: model heaps {}", heap.map_or("not found".into(), each)));
    }
    // XR_FIELD_SLOTS=1: every model in the pool (id, model, script, position): where the zone's NPCs stand
    if std::env::var_os("XR_FIELD_SLOTS").is_some() {
        for s in &slots {
            let mut m = Mem(mem);
            let pos = [0, 4, 8].map(|o| xr_net::Guest::f32(&mut m, s.addr + xr_net::x::field::MM_POS + o).unwrap_or(f32::NAN));
            let script = xr_net::Guest::u16(&mut m, s.addr + 0x18).unwrap_or(0);
            let facing = xr_net::Guest::u16(&mut m, s.addr + 0x8FE).unwrap_or(9);
            log::log(&format!("slot {name}: 0x{:08X} id 0x{:04X} model {} script {script} facing {facing} at {:.1},{:.1},{:.1}", s.addr, s.id, s.model, pos[0], pos[1], pos[2]));
        }
    }
}

/// Guest memory for xr-net's read accessors.
struct Mem<'a>(&'a mut xr_hle::memory::Memory);
impl xr_net::Guest for Mem<'_> {
    fn read(&mut self, addr: u32, out: &mut [u8]) -> bool {
        self.0.slice_mut(addr, out.len() as u32).map(|s| out.copy_from_slice(s)).is_some()
    }
}

/// The player through DllField's data segment (CRO segment 2, which moves with the module); None outside the field.
fn player(m: &mut exec::Machine) -> Option<(xr_net::x::field::Player, &mut xr_hle::memory::Memory)> {
    let data = field_data(m)?;
    let sys = m.system();
    let p = xr_net::x::field::player(&mut Mem(&mut sys.memory), data)?;
    Some((p, &mut sys.memory))
}

/// DllField's data segment (CRO segment 2), None when DllField is not loaded.
fn field_data(m: &mut exec::Machine) -> Option<u32> {
    hooks::dll_of(m.system()).map(|d| d.data)
}

/// Once per frame, after it ran: the local player (read only) to the world client, and what it heard back.
fn net_frame(m: &mut exec::Machine, net: &mut xr_net::Client) {
    use xr_net::x::field::{Facing, Mode};
    let data = field_data(m);
    let local = player(m).map(|(p, mem)| xr_net::Local {
        map: u32::from(p.map),
        pos: p.pos,
        facing: match p.facing {
            Facing::Up => 0,
            Facing::Down => 1,
            Facing::Left => 2,
            Facing::Right => 3,
        },
        mode: match p.mode {
            Mode::Walk => redex_proto::MODE_WALK,
            Mode::Run => redex_proto::MODE_RUN,
            Mode::Skates => redex_proto::MODE_SKATES,
            Mode::Bike => redex_proto::MODE_BIKE,
        },
        // what we wear now (a boutique changes it): sent on when it differs from the last one
        appearance: data.and_then(|d| xr_net::x::field::appearance(&mut Mem(mem), d)),
    });
    net.frame(local);
    // the players on our map, as puppets (a hook holding the lock gets them next frame)
    if let (Some(l), Some(mut lock)) = (local, hooks::try_puppets()) {
        let peers = net
            .peers
            .iter()
            .filter_map(|(&session, r)| {
                let (at, p) = r.track.back()?;
                // their look was validated when it came (xr_net::Client): none is the plain look of their gender
                let gender = r.appearance.map_or(r.gender, |a| a.gender);
                // a player sends at least once a second, standing or not: 5 s of nothing is a timeout
                (p.map == l.map && at.elapsed() < std::time::Duration::from_secs(5)).then_some((session, xr_net::x::puppet::Target { pos: [p.x, p.y, p.z], facing: p.facing, mode: p.mode, gender, look: r.appearance, rank: 0 }))
            })
            .collect();
        if let Some(pp) = lock.as_mut() {
            pp.want(l.map as u16, peers);
        }
    }
}

/// XR_FAKE_PEERS=N: N remote players made up around the local player, with no server (the puppet tests' feed).
/// They appear 2 tiles right of the player (XR_FAKE_AT=TX,TZ: at that tile), one tile apart downward (XR_FAKE_ROW=1:
/// rightward, side by side for the camera), from script
/// frame XR_FAKE_FROM (default 0) until XR_FAKE_TO; each walks a 3x3-tile square (every third runs; XR_FAKE_STILL=1:
/// they stand), every second is a girl, and they follow the player to each new map. Positions change at 10 Hz, as a
/// server relays them. XR_FAKE_LOOK=HEX,... gives them all an appearance (the 19 wire bytes, through
/// `Appearance::from_bytes` as a real peer's: bad bytes are the plain boy) and XR_FAKE_MODE=walk|run|bike|skates|surf,...
/// a mode; with XR_FAKE_EVERY=N they take the next one in each list every N frames (from XR_FAKE_FROM).
struct FakePeers {
    n: u32,
    from: u64,
    to: u64,
    still: bool,
    row: bool,
    at: Option<(u16, u16)>,
    looks: Vec<(Option<xr_net::x::field::Appearance>, u8)>,
    modes: Vec<u8>,
    every: u64,
    /// (map, the first peer's place there)
    anchor: Option<(u16, [f32; 3])>,
    last: Vec<(u32, xr_net::x::puppet::Target)>,
}

impl FakePeers {
    fn from_env() -> Option<FakePeers> {
        let var = |k: &str| std::env::var(k).ok();
        let n = var("XR_FAKE_PEERS")?.parse().ok()?;
        let at = var("XR_FAKE_AT").and_then(|v| {
            let (x, z) = v.split_once(',')?;
            Some((x.trim().parse().ok()?, z.trim().parse().ok()?))
        });
        let from = var("XR_FAKE_FROM").and_then(|v| v.parse().ok()).unwrap_or(0);
        let to = var("XR_FAKE_TO").and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
        let look = |h: &str| {
            let b = unhex(h);
            (xr_net::x::field::Appearance::from_bytes(&b), xr_net::x::field::Appearance::gender_of(&b))
        };
        let looks = var("XR_FAKE_LOOK").map_or(Vec::new(), |v| v.split(',').map(look).collect());
        let mode = |m: &str| match m.trim() {
            "run" => redex_proto::MODE_RUN,
            "skates" => redex_proto::MODE_SKATES,
            "bike" => redex_proto::MODE_BIKE,
            "surf" => redex_proto::MODE_SURF,
            _ => redex_proto::MODE_WALK,
        };
        let modes = var("XR_FAKE_MODE").map_or(Vec::new(), |v| v.split(',').map(mode).collect());
        let every = var("XR_FAKE_EVERY").and_then(|v| v.parse().ok()).filter(|&e| e > 0).unwrap_or(u64::MAX);
        log::log(&format!("puppets: {n} fake peers from frame {from} to {to}, looks {looks:?}, modes {modes:?}, next every {every} frames"));
        Some(FakePeers { n, from, to, still: var("XR_FAKE_STILL").is_some(), row: var("XR_FAKE_ROW").is_some(), at, looks, modes, every, anchor: None, last: Vec::new() })
    }

    fn peers(&mut self, frame: u64, p: &xr_net::x::field::Player) -> Vec<(u32, xr_net::x::puppet::Target)> {
        use xr_net::x::puppet::{TILE, Target};
        if !(self.from..self.to).contains(&frame) {
            self.anchor = None;
            return Vec::new();
        }
        if self.anchor.is_none_or(|a| a.0 != p.map) {
            let base = match self.at {
                Some((x, z)) => [f32::from(x) * TILE + TILE / 2.0, p.pos[1], f32::from(z) * TILE + TILE / 2.0],
                None => [p.pos[0] + 2.0 * TILE, p.pos[1], p.pos[2]],
            };
            self.anchor = Some((p.map, base));
        }
        // 10 Hz: the newest place only every 6th frame (the game runs at 30 per 60 frames)
        if frame % 6 != 0 && self.last.len() == self.n as usize {
            return self.last.clone();
        }
        let base = self.anchor.unwrap().1;
        let phase = ((frame - self.from) / self.every) as usize;
        let forced = (!self.modes.is_empty()).then(|| self.modes[phase % self.modes.len()]);
        self.last = (0..self.n)
            .map(|i| {
                // each peer one look further along the list (the first peer's is the list's current one)
                let look = (!self.looks.is_empty()).then(|| self.looks[(phase + i as usize) % self.looks.len()]);
                let mode = forced.unwrap_or(if i % 3 == 1 { redex_proto::MODE_RUN } else { redex_proto::MODE_WALK });
                // frames a tile: walking 14 (18/7 a game tick, 2 frames a tick), running 8, on skates about 7, on the bike 4
                let per_tile = match mode {
                    redex_proto::MODE_WALK => 14,
                    redex_proto::MODE_SKATES => 7,
                    redex_proto::MODE_BIKE => 4,
                    _ => 8,
                };
                let s = if self.still { 0 } else { (frame - self.from) / per_tile % 12 };
                // the square: right 3, down 3, left 3, up 3; facing 0 up, 1 down, 2 left, 3 right
                let (dx, dz, facing) = match s {
                    0..3 => (s, 0, 3),
                    3..6 => (3, s - 3, 1),
                    6..9 => (9 - s, 3, 2),
                    _ => (0, 12 - s, 0),
                };
                let (rx, rz) = if self.row { (u64::from(i), 0) } else { (0, u64::from(i)) };
                let pos = [base[0] + (dx + rx) as f32 * TILE, base[1], base[2] + (dz + rz) as f32 * TILE];
                // with XR_FAKE_LOOK the gender is the bytes' (a bad outfit: the plain look of that gender), as for a real peer
                let (gender, look) = match look {
                    Some((l, g)) => (l.map_or(g, |a| a.gender), l),
                    None => ((i % 2) as u8, None),
                };
                (0x1000 + i, Target { pos, facing: if self.still { 1 } else { facing }, mode, gender, look, rank: 0 })
            })
            .collect();
        self.last.clone()
    }
}

/// MyStatus's outfit bytes (the save's +0x14030), through the field context's GameData as `field::appearance` reads it.
fn mystatus_fashion(mem: &mut xr_hle::memory::Memory, data: u32) -> Option<u32> {
    use xr_net::Guest;
    use xr_net::x::field::*;
    let mut g = Mem(mem);
    let ctx = g.u32(data + SNAPSHOT + SNAP_FIELDCTX)?;
    let gd = g.u32(ctx)?;
    Some(mystatus_of(&mut g, gd)? + MS_FASHION)
}

/// Hex digits to bytes (a pair that is not hex reads as 0; an odd last digit is dropped): XR_FAKE_LOOK.
fn unhex(h: &str) -> Vec<u8> {
    let h = h.trim().as_bytes();
    h.chunks_exact(2).map(|c| std::str::from_utf8(c).ok().and_then(|c| u8::from_str_radix(c, 16).ok()).unwrap_or(0)).collect()
}

/// The name other players see when neither --name nor the settings give one: the trainer name in the title's save.
fn trainer_name(user: &std::path::Path, title: u64) -> String {
    let main = std::fs::read(user.join(format!("savedata/{title:016X}/main"))).ok();
    main.and_then(|b| xr_net::x::trainer_name(&b)).unwrap_or_else(|| "Trainer".into())
}

/// One scripted input: buttons and/or a touch, held from frame `at` for `hold` frames.
#[derive(Debug, PartialEq)]
struct Press {
    buttons: PadState,
    touch: Option<(u16, u16)>,
    at: u64,
    hold: u64,
}

/// `A@400` (held 8 frames, long enough for any game to see it), `UP@900+30` (30 frames), `TOUCH:160,120@500`
/// (bottom-screen pixel). CIRCLE_UP/DOWN/LEFT/RIGHT push the circle pad all the way.
fn press(v: &str) -> Result<Press, String> {
    let (name, when) = v.split_once('@').ok_or(format!("--press {v}: NAME@FRAME[+HOLD]"))?;
    let (at, hold) = when.split_once('+').unwrap_or((when, "8"));
    let number = |s: &str| s.parse::<u64>().map_err(|_| format!("--press {v}: {s} is not a number"));
    let (at, hold) = (number(at)?, number(hold)?);
    if let Some(xy) = name.to_uppercase().strip_prefix("TOUCH:") {
        let (x, y) = xy.split_once(',').ok_or(format!("--press {v}: TOUCH:X,Y"))?;
        let xy = |s: &str| s.parse::<u16>().map_err(|_| format!("--press {v}: {s} is not a pixel"));
        return Ok(Press { buttons: PadState::empty(), touch: Some((xy(x)?, xy(y)?)), at, hold });
    }
    let buttons = PadState::from_name(&name.to_uppercase()).ok_or(format!("--press {v}: no button {name}"))?;
    Ok(Press { buttons, touch: None, at, hold })
}

/// The player's HP bar on the battle screen (top, 400 wide): 49 pixels from x 76 on row 222, filled green, yellow or
/// red; the filled fraction in percent, or None when no bar pixel is filled.
fn hp_bar(top: &[u8]) -> Option<u32> {
    if top.len() < 400 * 240 * 4 {
        return None;
    }
    let px = |x: usize| &top[(222 * 400 + x) * 4..][..3];
    let filled = |p: &[u8]| (p[1] > 150 && p[0] < 150 && p[2] < 120) || (p[0] > 180 && p[2] < 80); // green / yellow, red
    let n = (76..125).filter(|&x| filled(px(x))).count() as u32;
    (n > 0).then_some(n * 100 / 49)
}

/// The lead's HP in % from the battle's record of it (`autobattle ... mon SPECIES`): the first u16 SPECIES in the app
/// heap followed by max HP, HP (1..=max: other records with this species have 0), three words and the level. Seen at
/// 0x082059E4 (and a copy 0x488 bytes on) in Santalune's battles: 653 (Fennekin), 28, 24, 0, 0, 66, 9.
fn hp_mem(mem: &mut xr_hle::memory::Memory, species: &[u16]) -> Option<u32> {
    lead_record(mem, species).map(|(_, pct)| pct)
}

/// The lead's battle record (see `hp_mem`): its address and HP in %. The foe's record follows it, 0x244 bytes on
/// (species at +0, seen for a wild Burmy and Bidoof on Route 3).
fn lead_record(mem: &mut xr_hle::memory::Memory, species: &[u16]) -> Option<(u32, u32)> {
    let mut page = [0u8; 0x1000];
    for base in (0x0810_0000u32..0x0860_0000).step_by(0x1000) {
        let Some(s) = mem.slice_mut(base, 0x1000) else { continue };
        page.copy_from_slice(s);
        let w = |i: usize| u16::from_le_bytes([page[i], page[i + 1]]);
        // shortcut: a record that straddles a page boundary is missed; the next copy (0x488 on) is found instead
        for i in (0..0x1000 - 14).step_by(2) {
            let (max, hp, level) = (w(i + 2), w(i + 4), w(i + 12));
            if species.contains(&w(i)) && (1..1000).contains(&max) && (1..=max).contains(&hp) && (1..=100).contains(&level) {
                return Some((base + i as u32, u32::from(hp) * 100 / u32::from(max)));
            }
        }
    }
    None
}

/// The command screen is up: BAG's orange at 30,225 and the grey at 20,60 (the move screen shows both dimmed).
fn command_screen(bottom: &[u8]) -> bool {
    let px = |x: usize, y: usize| bottom.get((y * 320 + x) * 4..(y * 320 + x) * 4 + 3);
    match (px(30, 225), px(20, 60)) {
        (Some(b), Some(g)) => b[0] > 220 && b[2] < 70 && g.iter().max().unwrap() - g.iter().min().unwrap() < 20,
        _ => false,
    }
}

/// The move screen is up (bottom screen, 320 wide): the first move's button, type-coloured, at 20,60 (the command
/// screen is grey there), or, with `normal_first` (`flee` lines; older scripts keep the old test, and so their hashes),
/// a grey (Normal) first move and the light blue back button at 298,220 (the command screen's POKEMON button is green
/// there). With `backdrop` (newer lines that heal from the Bag) the move screen's light backdrop between the top two
/// buttons, at 160,25, is needed too: the Bag's screens are dark red there, and its first screen (pockets) is coloured
/// at 20,60 like a typed move, so an empty pocket after the last Potion and that screen were taken for the move screen
/// in turn, a loop.
fn move_screen(bottom: &[u8], normal_first: bool, backdrop: bool) -> bool {
    let px = |x: usize, y: usize| bottom.get((y * 320 + x) * 4..(y * 320 + x) * 4 + 3);
    let (Some(p), Some(b), Some(gap)) = (px(20, 60), px(298, 220), px(160, 25)) else { return false };
    let grey = p.iter().max().unwrap() - p.iter().min().unwrap() < 20 && *p.iter().min().unwrap() > 180; // not a Bag slot (165)
    if normal_first {
        let light = !backdrop || *gap.iter().min().unwrap() > 200;
        return light && (p.iter().max().unwrap() - p.iter().min().unwrap() > 40 || b[2] > 180 && b[2] > b[0] + 100 && grey);
    }
    p.iter().max().unwrap() - p.iter().min().unwrap() > 40
}

/// The party list's slots 2-6 (two columns, three rows), the replacements for a fainted lead.
/// --make-preview: the state is saved at script frame PREVIEW_SAVE_AT (the player has control from about 6520), and the
/// run goes on to PREVIEW_FRAMES so the progress line reaches 6500
const PREVIEW_SAVE_AT: u64 = 6510;
const PREVIEW_FRAMES: u64 = 6520;
const PARTY_SLOTS: [(u16, u16); 5] = [(240, 45), (80, 95), (240, 95), (80, 145), (240, 145)];

/// The lead's "cannot battle" panel after it fainted: the panel's green at 40,80 and 280,80 and the dark red FAINTED
/// badge across 136-187 x 110-120 (280 dark red pixels; a Bag pocket has about 40, the dimmed screen green nowhere).
fn fainted_panel(bottom: &[u8]) -> bool {
    let px = |x: usize, y: usize| bottom.get((y * 320 + x) * 4..(y * 320 + x) * 4 + 3);
    let green = |p: Option<&[u8]>| p.is_some_and(|p| p[1] > 180 && p[0] < 120 && p[2] < 100);
    let badge = (136..188).flat_map(|x| (110..121).map(move |y| (x, y))).filter(|&(x, y)| px(x, y).is_some_and(|p| p[0] >= 30 && p[1] < 30 && p[2] < 30));
    green(px(40, 80)) && green(px(280, 80)) && badge.count() >= 150
}

/// A double battle's first foe on its target screen.
const FIRST_FOE: (u16, u16) = (100, 70);

/// A double battle's target screen: the foes' red buttons on top (60,75 and 250,80), the party's green ones below
/// (60,155 and 250,155).
fn target_screen(bottom: &[u8]) -> bool {
    let px = |x: usize, y: usize| bottom.get((y * 320 + x) * 4..(y * 320 + x) * 4 + 3);
    let red = |p: Option<&[u8]>| p.is_some_and(|p| p[0] > 80 && u16::from(p[0]) > 2 * u16::from(p[1]));
    let green = |p: Option<&[u8]>| p.is_some_and(|p| p[1] > 60 && u16::from(p[1]) > 2 * u16::from(p[0]));
    red(px(60, 75)) && red(px(250, 80)) && green(px(60, 155)) && green(px(250, 155))
}

/// The bottom button of the move-learning questions.
const KEEP_OLD_MOVES: (u16, u16) = (160, 180);

/// A move-learning question is up (a level-up with four moves known), and the touch that declines it: both have a green
/// button across y 114 and a blue one across y 180. "Should a move be deleted and replaced with X?" (FORGET, its text
/// dark at 135,108): KEEP OLD MOVES, the blue one; then "Give up on learning X?" (YES, its text light green there): YES.
fn learn_prompt(bottom: &[u8]) -> Option<(u16, u16)> {
    let px = |x: usize, y: usize| bottom.get((y * 320 + x) * 4..(y * 320 + x) * 4 + 3);
    let green = |p: Option<&[u8]>| p.is_some_and(|p| p[1] > 170 && p[0] < 100 && p[2] < 120);
    let blue = |p: Option<&[u8]>| p.is_some_and(|p| p[2] > 230 && p[0] < 100 && (140..220).contains(&p[1]));
    if !(green(px(40, 114)) && green(px(280, 114)) && blue(px(40, 180)) && blue(px(280, 180))) {
        return None;
    }
    let text = px(135, 108).is_some_and(|p| p.iter().map(|&v| u32::from(v)).sum::<u32>() < 200);
    Some(if text { KEEP_OLD_MOVES } else { (160, 114) })
}

/// `autobattle`'s input during a battle at frame n: A, then a touch at the move (if the move screen is up).
fn battle_input(n: u64, mv: Option<(u16, u16)>) -> InputState {
    match n % 40 {
        0..8 => InputState { buttons: PadState::A, ..Default::default() },
        20..28 => InputState { touch: mv, ..Default::default() },
        _ => InputState::default(),
    }
}

/// What the scripted presses hold down at frame `n`.
fn input_at(presses: &[Press], n: u64) -> InputState {
    let mut input = InputState::default();
    for p in presses.iter().filter(|p| (p.at..p.at.saturating_add(p.hold)).contains(&n)) {
        input.buttons |= p.buttons;
        input.touch = p.touch.or(input.touch);
    }
    // the circle pad is an axis; hid derives the CIRCLE_* bits from it
    let axis = |plus, minus| (input.buttons.contains(plus) as i8 - input.buttons.contains(minus) as i8) as f32;
    (input.circle_x, input.circle_y) = (axis(PadState::CIRCLE_RIGHT, PadState::CIRCLE_LEFT), axis(PadState::CIRCLE_UP, PadState::CIRCLE_DOWN));
    input.buttons -= PadState::CIRCLE_RIGHT | PadState::CIRCLE_LEFT | PadState::CIRCLE_UP | PadState::CIRCLE_DOWN;
    input
}

#[cfg(test)]
mod tests {
    /// every preview process gets a folder nothing else uses (a sibling's stays untouched, a
    /// stale one from an old run is swept, a player's `<data>/user` is never involved).
    #[test]
    fn each_preview_gets_its_own_folder() {
        let data = std::env::temp_dir().join(format!("redex-pv-{}", std::process::id()));
        let build = data.join("build/x-usa-v0.2.0");
        let root = data.join("preview/x-usa-v0.2.0");
        let sibling = root.join("user-girl-1-1");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("live"), b"x").unwrap();
        std::fs::create_dir_all(data.join("user")).unwrap();
        std::fs::write(data.join("user/save"), b"s").unwrap();
        let (a, b) = (super::preview_user(&data, &build, "boy"), super::preview_user(&data, &build, "boy"));
        assert_ne!(a, b, "two makers of one gender never share");
        assert_ne!(a, super::preview_user(&data, &build, "girl"));
        assert!(a.starts_with(&root) && !a.exists());
        assert!(sibling.join("live").exists(), "a running sibling's folder is not wiped");
        assert!(data.join("user/save").exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn one_game_window_per_data_folder() {
        let base = std::env::temp_dir().join(format!("redex-own-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        assert!(super::own_data_folder(&a, 0));
        // another process (here: another thread, the mutex's owner is a thread) can't take it; another folder it can
        let (a2, b2) = (a.clone(), b.clone());
        std::thread::spawn(move || {
            assert!(!super::own_data_folder(&a2, 0));
            assert!(super::own_data_folder(&b2, 0));
        })
        .join()
        .unwrap();
    }

    /// Dev and test inputs keep a run out of worlds (unless built with mp-tests): the flags that are on, and the XR_
    /// variables that drive or change the game, whatever their case; read-only diagnostics (XR_LOG, XR_MEMDUMP) don't.
    /// A lobby (whatever its address) and every steam: world are P2P: their answers never reach this PC's characters.
    #[test]
    fn lobby_and_steam_worlds_never_merge_characters() {
        assert!(super::p2p_world("steam:90297831083063301", false));
        assert!(super::p2p_world("127.0.0.1:7777", true), "a dev lobby");
        assert!(!super::p2p_world("127.0.0.1:7777", false), "a test server on this PC (mp-tests)");
    }

    /// The world client's and a hosted lobby's server threads end on a panic; any other thread's panic ends the game.
    #[test]
    fn server_and_client_threads_survive_a_panic() {
        for t in ["redex-net", "redex-accept", "redex-conn", "redex-steam", "redex-lobby-start"] {
            assert!(super::survives_panic(t), "{t}");
        }
        for t in ["main loop", "emulation", "discord", "redexconn"] {
            assert!(!super::survives_panic(t), "{t}");
        }
    }

    #[test]
    fn dev_inputs_keep_a_run_offline() {
        let vars = ["PATH", "XR_DATA", "XR_STATE_LOAD", "xr_fake_row", "XR_LOG", "XR_MEMDUMP", "XR_MP_WHISPER", "XR_SET_FASHION", "XR_EXPLORE_GOAL", "XR_RAILPOS", "XR_FAKE_PEERS", "XR_STATE_SAVE_AT"];
        let got = super::dev_inputs(&[("--headless", true), ("--script", false), ("--room", true)], vars.map(String::from));
        assert_eq!(got, ["--headless", "--room", "XR_STATE_LOAD", "xr_fake_row", "XR_MP_WHISPER", "XR_SET_FASHION", "XR_EXPLORE_GOAL", "XR_RAILPOS", "XR_FAKE_PEERS", "XR_STATE_SAVE_AT"]);
        assert!(super::dev_inputs(&[("--headless", false)], ["XR_DATA".to_string(), "XR_LOG".into()]).is_empty());
        // the refusal: test inputs (not in an mp-tests build), then mods that change the game, then an edited dump
        // (asked last: it may hash the dump)
        let dev = vec!["XR_STATE_LOAD".to_string()];
        let unasked = || panic!("the dump is checked last, and only when nothing else refused");
        if !cfg!(feature = "mp-tests") {
            assert!(super::refusal(&dev, None, unasked).is_some());
        }
        assert_eq!(super::refusal(&[], Some("Shiny Lock Removal"), unasked).as_deref(), Some("Turn off mods to play online: Shiny Lock Removal changes the game (skins and texture packs are fine)"));
        assert_eq!(super::refusal(&[], None, || true).as_deref(), Some("This game file was changed since it was added; online is off"));
        assert_eq!(super::refusal(&[], None, || false), None);
    }

    #[test]
    fn a_steam_login_keeps_only_steam_and_local_worlds() {
        use super::boot_world_kept as k;
        assert!(k("steam:123", true, false) && k("127.0.0.1:7777", true, false) && k("localhost:1", true, false));
        assert!(!k("203.0.113.5:7777", true, false) && !k("example.org:7777", true, false));
        assert!(k("203.0.113.5:7777", true, true) && k("example.org:7777", false, false));
    }

    /// A local character with no mode on the command line (an old shortcut, or the start screen's old toggle on
    /// "Online") plays offline, even when the settings name a world; only --world, --p2p or --account-server go online,
    /// and the explicit dev and test forms stay as they were.
    #[test]
    fn no_mode_on_the_command_line_plays_offline() {
        use super::boot_world as b;
        let w = |s: &str| Some(s.to_owned());
        // `redex --cci X --build B --character ID`, settings with and without a world, window or headless
        for settings in [Some("steam:90297831083063301"), Some(""), None] {
            assert_eq!(b(None, false, false, false, settings), (None, true), "{settings:?}");
            assert_eq!(b(None, false, true, false, settings), (None, true), "--offline, {settings:?}");
        }
        // --p2p: the settings' world, else none at boot (the server browser and invites), never offline
        assert_eq!(b(None, true, false, false, Some("steam:9")), (w("steam:9"), false));
        assert_eq!(b(None, true, false, false, Some("")), (None, false));
        assert_eq!(b(None, true, false, false, None), (None, false));
        assert_eq!(b(None, true, true, false, Some("steam:9")), (None, true), "--offline wins over the settings' world");
        // explicit worlds (dev, tests, Steam's +connect) and online characters as before
        assert_eq!(b(w("127.0.0.1:7777"), false, false, false, Some("steam:9")), (w("127.0.0.1:7777"), false));
        assert_eq!(b(w("tls:h:7777"), false, false, true, None), (w("tls:h:7777"), false));
        assert_eq!(b(None, false, false, true, Some("steam:9")), (None, false), "--account-server alone: its own error, not a P2P world");
    }

    use super::*;

    /// Presence is recorded as set only once it was: a window that booted without Steam sets it (the `redex` key with it)
    /// as soon as Steam is up, though what it wants did not change.
    #[test]
    fn presence_waits_for_steam() {
        let mut last = None;
        let offline = (Some("Playing offline".to_string()), None::<String>);
        assert_eq!(presence_to_set(&mut last, offline.clone(), false), None);
        assert_eq!(last, None, "nothing was sent");
        assert_eq!(presence_to_set(&mut last, offline.clone(), true), Some(offline.clone()));
        assert_eq!(presence_to_set(&mut last, offline.clone(), true), None, "unchanged");
    }

    /// Fake friends are a test input: a release run with XR_FAKE_FRIENDS (or its script or reply) stays out of worlds.
    #[test]
    fn fake_friends_is_a_dev_input() {
        let got = dev_inputs(&[], ["XR_FAKE_FRIENDS", "XR_FAKE_FRIEND_SAYS", "XR_MP_FRIEND_REPLY"].map(String::from));
        assert_eq!(got, ["XR_FAKE_FRIENDS", "XR_FAKE_FRIEND_SAYS", "XR_MP_FRIEND_REPLY"]);
        let refused = refusal(&got, None, || false);
        assert_eq!(refused.is_some(), !cfg!(feature = "mp-tests"), "{refused:?}");
    }

    /// Only `steam:<digits>` comes in from Steam's "Join game" and +connect; a Steam login reaches TCP on this PC only.
    #[test]
    fn join_addresses_from_steam_are_steam_ids_only() {
        assert!(steam_world("steam:90297831083063301"));
        for bad in ["127.0.0.1:7777", "evil.example:7777", "steam:", "steam:12ab", "steam:-1", "steam:1:2", " steam:1", "steam:99999999999999999999"] {
            assert!(!steam_world(bad), "{bad}");
        }
        assert!(loopback("127.0.0.1:7777") && loopback("localhost:7777") && loopback("[::1]:7777") && loopback("127.0.0.2:1"));
        assert!(!loopback("192.168.1.5:7777") && !loopback("evil.example:7777") && !loopback("10.0.0.1:7777"));
    }

    #[test]
    fn scripted_presses() {
        assert_eq!(press("a@400").unwrap(), Press { buttons: PadState::A, touch: None, at: 400, hold: 8 });
        assert_eq!(press("UP@900+30").unwrap().hold, 30);
        assert_eq!(press("TOUCH:160,120@5").unwrap().touch, Some((160, 120)));
        assert!(press("Q@1").is_err() && press("A").is_err() && press("TOUCH:1@2").is_err());
        let p = [press("A@10").unwrap(), press("CIRCLE_UP@12+2").unwrap(), press("TOUCH:3,4@12").unwrap()];
        assert_eq!(input_at(&p, 9).buttons, PadState::empty());
        let i = input_at(&p, 13);
        assert_eq!((i.buttons, i.circle_y, i.touch), (PadState::A, 1.0, Some((3, 4))));
        assert_eq!(input_at(&p, 18).buttons, PadState::empty());
        // a press at the very end of time neither overflows nor holds anything earlier
        assert_eq!(input_at(&[press(&format!("A@{}+8", u64::MAX - 2)).unwrap()], u64::MAX - 1).buttons, PadState::A);
    }

    #[test]
    fn move_screen_with_a_normal_move_first() {
        // pixels sampled from Fennekin's battles (Santalune Forest, Viola's gym): (20,60), (30,225), (298,220), (160,25)
        let screen = |a: [u8; 3], b: [u8; 3], c: [u8; 3], d: [u8; 3]| {
            let mut s = vec![0u8; 320 * 240 * 4];
            for ((x, y), p) in [((20, 60), a), ((30, 225), b), ((298, 220), c), ((160, 25), d)] {
                s[(y * 320 + x) * 4..][..3].copy_from_slice(&p);
            }
            s
        };
        let scratch_first = screen([197, 192, 186], [192, 156, 103], [65, 186, 230], [243, 243, 243]);
        let command = screen([212, 212, 212], [241, 159, 41], [33, 127, 50], [242, 140, 140]);
        let dimmed = screen([41, 41, 41], [46, 30, 6], [13, 35, 14], [26, 19, 19]);
        let bag_pocket = screen([165, 165, 165], [90, 111, 92], [65, 186, 230], [105, 78, 78]); // the empty HP pocket too
        let bag_pockets = screen([239, 161, 11], [205, 163, 100], [65, 186, 230], [105, 78, 78]); // the Bag's first screen
        assert!(!move_screen(&bag_pocket, true, true) && !command_screen(&bag_pocket));
        assert!(!move_screen(&bag_pockets, true, true) && !command_screen(&bag_pockets));
        assert!(move_screen(&bag_pockets, true, false)); // the old test's mistake, kept for lines that never open the Bag
        assert!(move_screen(&scratch_first, true, true) && !move_screen(&scratch_first, false, false));
        assert!(!command_screen(&scratch_first));
        assert!(!move_screen(&command, true, true) && command_screen(&command));
        assert!(!move_screen(&dimmed, true, true) && !command_screen(&dimmed));
    }

    #[test]
    fn fainted_lead_panel() {
        // Braixen's "cannot battle" panel in Connecting Cave (green 91,215,70; the badge dark red 51..102,0,0)
        let mut s = vec![0u8; 320 * 240 * 4];
        let put = |s: &mut Vec<u8>, x: usize, y: usize, c: [u8; 3]| s[(y * 320 + x) * 4..][..3].copy_from_slice(&c);
        put(&mut s, 40, 80, [91, 215, 70]);
        put(&mut s, 280, 80, [91, 215, 70]);
        let panel_only = s.clone();
        for x in 137..186 {
            for y in 111..118 {
                put(&mut s, x, y, [102, 0, 0]);
            }
        }
        assert!(fainted_panel(&s));
        assert!(!fainted_panel(&panel_only)); // a healthy Pokemon's panel: no badge
        let mut no_green = s.clone();
        put(&mut no_green, 40, 80, [196, 196, 196]);
        assert!(!fainted_panel(&no_green));
    }

    #[test]
    fn double_battle_target_screen() {
        // pixels sampled at (60,75), (250,80), (60,155), (250,155): the target screen (Route 5's twins, Plusle and
        // Minun), Fennekin's move screen, the command screen and the move-learning question
        let screen = |p: [[u8; 3]; 4]| {
            let mut s = vec![0u8; 320 * 240 * 4];
            for ((x, y), c) in [(60, 75), (250, 80), (60, 155), (250, 155)].into_iter().zip(p) {
                s[(y * 320 + x) * 4..][..3].copy_from_slice(&c);
            }
            s
        };
        assert!(target_screen(&screen([[108, 35, 19], [97, 31, 17], [21, 85, 48], [21, 85, 48]])));
        assert!(!target_screen(&screen([[79, 77, 74], [149, 147, 146], [67, 12, 5], [62, 61, 63]])));
        assert!(!target_screen(&screen([[205, 42, 34], [156, 119, 117], [142, 142, 142], [153, 153, 153]])));
        assert!(!target_screen(&screen([[71, 35, 31], [81, 42, 38], [67, 67, 67], [64, 64, 64]])));
    }

    #[test]
    fn move_learning_question() {
        // pixels sampled from Viola's gym (Fennekin at Lv 20, Fire Spin): (40,114), (280,114), (40,180), (280,180),
        // and the top button's text at (135,108)
        let screen = |p: [[u8; 3]; 5]| {
            let mut s = vec![0u8; 320 * 240 * 4];
            for ((x, y), c) in [(40, 114), (280, 114), (40, 180), (280, 180), (135, 108)].into_iter().zip(p) {
                s[(y * 320 + x) * 4..][..3].copy_from_slice(&c);
            }
            s
        };
        let buttons = [[54, 200, 70], [54, 200, 70], [57, 180, 252], [55, 180, 252]];
        let ask = |text| screen([buttons[0], buttons[1], buttons[2], buttons[3], text]);
        assert_eq!(learn_prompt(&ask([15, 30, 16])), Some(KEEP_OLD_MOVES)); // FORGET / KEEP OLD MOVES
        assert_eq!(learn_prompt(&ask([109, 227, 120])), Some((160, 114))); // give up learning it? YES / NO
        // the move list FORGET opens, a move's page, the move screen, the command screen, the dimmed screen
        for p in [
            [[149, 180, 141], [149, 180, 141], [117, 94, 46], [56, 102, 56], [0, 0, 0]],
            [[71, 72, 70], [74, 75, 73], [81, 74, 33], [41, 79, 40], [0, 0, 0]],
            [[188, 33, 13], [174, 169, 163], [154, 141, 115], [121, 146, 121], [0, 0, 0]],
            [[171, 171, 171], [171, 171, 171], [183, 128, 16], [40, 173, 43], [0, 0, 0]],
            [[30, 30, 30], [31, 31, 31], [29, 23, 11], [14, 25, 14], [0, 0, 0]],
        ] {
            assert_eq!(learn_prompt(&screen(p)), None, "{p:?}");
        }
    }
}

/// The game as the window plays it: the executor's machine, with the status line logged every 60 frames, and the
/// scripted presses (--press/--script) held on top of the keyboard and controllers. Booted (by `boot` in `run`) and run on
/// the window's emulation thread, which holds the executor's baton between frames (`Exec::wait` registers its caller).
struct Windowed {
    m: exec::Machine,
    n: u64,
    presses: Vec<Press>,
    net: Option<xr_net::Client>,
    joiner: Joiner,
    /// the world joined (its address), with `net`
    world: Option<String>,
    /// a world being joined: (address, its client, since when). `net` stays in the old world until this one logs in,
    /// and it is given up after JOIN_WAIT
    pending: Option<(String, xr_net::Client, std::time::Instant)>,
    /// Steam's server list being asked for
    query: Option<(xr_net::steam::ServerQuery, bool)>,
    /// the rich presence last set: (status, connect)
    presence: Option<(Option<String>, Option<String>)>,
    /// the Discord status (its own thread), none for a title it has no art for
    discord: Option<discord::Handle>,
    /// what the player should hear (a toast each)
    notes: Vec<String>,
    /// --character's look in the game
    played: Option<character::Played>,
    /// Steam friends (once Steam is up), or fakes (mp-tests, XR_FAKE_FRIENDS)
    friends: Option<Box<dyn xr_net::friends::FriendsLink>>,
    /// friends online, as last listed (on opening the tab, every FRIENDS_AGAIN while open, and when a stranger to the
    /// list writes)
    friend_list: Vec<xr_net::friends::Friend>,
    /// the Steam accounts ignored, once the window read its list: the Steam link starts only then, so no session from an
    /// ignored friend is ever accepted
    ignored: Option<Vec<u64>>,
    /// when the friends were last listed (again every FRIENDS_AGAIN, tab open or not: a chat needs to know who left)
    listed: Option<std::time::Instant>,
    /// sends that failed, for the view: (to, chat text, why)
    friend_failed: Vec<(u64, Option<String>, String)>,
    /// worlds' names from Steam's server list, by address: what an invite names
    world_names: std::collections::HashMap<String, String>,
    /// the lobby this game hosts ("Host lobby"), if it does
    lobby: lobby::Lobby,
    /// a room code being looked up (Join, --lobby): the code, and Steam's answer to come
    finding: Option<(String, xr_net::steam::Answer<String>)>,
    /// worlds that are lobbies (hosted here, or found by a room code): P2P worlds whatever their address (`p2p_world`)
    lobby_worlds: std::collections::HashSet<String>,
}

/// How long a world being joined may take to log us in before it is given up (the old world kept).
const JOIN_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

use redex_proto::steam_world;

/// The world joined at boot, and whether the run is offline. The game never goes online by itself: only `--world` (or
/// Steam's `+connect`), `--p2p` (with the settings' world, if they name one) or `--account-server` choose it. Anything
/// else, an old shortcut or a settings file from the start screen's old online/offline switch, plays offline.
fn boot_world(world: Option<String>, p2p: bool, offline: bool, account: bool, settings_world: Option<&str>) -> (Option<String>, bool) {
    let world = world.or_else(|| settings_world.filter(|a| p2p && !offline && !a.is_empty()).map(str::to_owned));
    let offline = offline || (world.is_none() && !p2p && !account);
    (world, offline)
}

/// Whether a boot-time world address is kept: with a Steam login (no `--dev-id`) only `steam:` and this PC's own TCP
/// world are; anything else would send the ticket over TCP to another machine.
fn boot_world_kept(addr: &str, steam: bool, dev_id: bool) -> bool {
    dev_id || !steam || addr.starts_with("steam:") || loopback(addr)
}

/// A world's name as friends' presence shows it: 24 characters at most.
fn short_name(name: &str) -> String {
    name.chars().take(24).collect()
}

/// `--login` (mp-tests builds only): a password login, the password from REDEX_PASSWORD (used once, never logged).
#[cfg(feature = "mp-tests")]
fn password_login(server: &str, email: &str) -> Result<Result<xr_net::account::Account, xr_net::account::Error>, String> {
    let password = std::env::var("REDEX_PASSWORD").map_err(|_| "--login takes the password from REDEX_PASSWORD")?;
    let device = format!("RE:DEX on {}", std::env::var("COMPUTERNAME").unwrap_or_else(|_| "a PC".into()));
    Ok(xr_net::account::Account::login(server, email, &password, &device))
}

/// A normal build has no password path at all.
#[cfg(not(feature = "mp-tests"))]
fn password_login(_: &str, _: &str) -> Result<Result<xr_net::account::Account, xr_net::account::Error>, String> {
    Err("--login exists only in an mp-tests build".into())
}

/// A TCP world on this PC: the only place a Steam login goes over TCP (a ticket there is not bound to the server).
fn loopback(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h).trim_matches(['[', ']']);
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A character whose look the game changed (`Played::frame`), to the account: now when online, else at the next login.
fn put_character(c: Option<redex_proto::Character>, net: Option<&xr_net::Client>) {
    if let (Some(c), Some(net)) = (c, net) {
        if !net.put_character(c) {
            log::log("character: the new look did not reach the world client (queue full or offline for good); the next start puts it");
        }
    }
}

/// The account's character lists the world sent, into characters.json: the newest `changed` wins (`merge_remote`, which
/// drops bad looks), saved only when that changed something. While characters.json can't be read (nor kept aside) the
/// lists wait, and the file is left alone. `discard`: nothing is kept. An official-account login keeps nothing (its
/// characters are the account's, the start screen shows them, never this PC's), and neither does a lobby or any other
/// P2P (Steam) world: a player's host never changes the joiner's own characters (`p2p_world`).
fn keep_accounts(net: &mut xr_net::Client, data: &std::path::Path, discard: bool) {
    if discard {
        net.accounts.clear();
    }
    if net.accounts.is_empty() {
        return;
    }
    let Some(mut store) = xr_frontend::start::characters::Store::try_load(data) else { return };
    let now = xr_frontend::start::characters::now_ms();
    let changed = net.accounts.drain(..).fold(false, |changed, list| store.merge_remote(list, now) | changed);
    if changed {
        match store.save(data) {
            Ok(()) => log::log(&format!("characters: the account's newer copies kept ({} character(s))", store.characters.len())),
            Err(e) => log::log(&format!("characters: the account's copies were not kept: {e}")),
        }
    }
}

/// A world whose host is another player (a lobby, joined by code or hosted, or any `steam:` world): its answers never
/// reach this PC's characters.json or saves.
fn p2p_world(addr: &str, lobby: bool) -> bool {
    lobby || steam_world(addr)
}

/// The XR_ variables of tests and dev tools that drive or change the game (by prefix): save states, the outfit
/// writer, the explorer, fake peers, scripted whispers and reports, the rail-position probe.
const DEV_VARS: [&str; 6] = ["XR_STATE_", "XR_SET_FASHION", "XR_EXPLORE", "XR_FAKE_", "XR_MP_", "XR_RAILPOS"];

/// The dev and test inputs this run has: the `flags` that are on, then the `vars` (environment names) of `DEV_VARS`.
fn dev_inputs(flags: &[(&str, bool)], vars: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut on: Vec<String> = flags.iter().filter(|f| f.1).map(|f| f.0.to_string()).collect();
    on.extend(vars.into_iter().filter(|v| DEV_VARS.iter().any(|d| v.to_uppercase().starts_with(d))));
    on
}

/// Why this run may not join a world, if it may not: dev or test inputs (bots, save states, scripted input; allowed
/// only in an `mp-tests` build, the MP test harness's), a mod that changes the game (`modded`, its name), or a game file edited since it was
/// added (`changed`, asked last: it may hash the file). Client-side: it keeps dev tools and casual edits out of a
/// release; the server's checks are the real fence.
fn refusal(dev: &[String], modded: Option<&str>, changed: impl FnOnce() -> bool) -> Option<String> {
    if !cfg!(feature = "mp-tests") && !dev.is_empty() {
        return Some(format!("Online is off while test options are on ({})", dev.join(", ")));
    }
    if let Some(name) = modded {
        return Some(format!("Turn off mods to play online: {name} changes the game (skins and texture packs are fine)"));
    }
    changed().then(|| "This game file was changed since it was added; online is off".into())
}

/// How the player gets into a world: who they are and as which name. Steam is started the first time it is needed
/// (a window that boots offline and then joins from the server browser), and tried again at each Join or search
/// while it is not up.
struct Joiner {
    steam: Option<xr_net::steam::Steam>,
    /// logged in to the account backend (`--account-server`): worlds are joined with world tickets, never Steam
    account: Option<xr_net::account::Account>,
    dev_id: bool,
    name: Option<String>,
    /// the saves folder: the character's with --character, else `<data>/user`
    user: PathBuf,
    /// where the dev id lives, `<data>/user` for every character (one player per PC)
    id_dir: PathBuf,
    /// the data folder: its characters.json goes to the account at each login, and the account's newer copies into it
    data: PathBuf,
    identity: Option<xr_net::id::Identity>,
    /// this run's dev and test inputs (`dev_inputs`): no world while any is on (`refusal`)
    dev: Vec<String>,
    /// the game file (--cci): no world once it was edited (`dump_changed`)
    cci: PathBuf,
}

impl Joiner {
    /// Steam, started now if it was not (never with --dev-id); None when it cannot run (the reason logged).
    fn steam(&mut self) -> Option<&xr_net::steam::Steam> {
        if self.steam.is_none() && !self.dev_id {
            self.steam = xr_net::steam::Steam::init().inspect_err(|e| log::log(&format!("world: {e}"))).ok();
        }
        self.steam.as_ref()
    }

    /// The game file was edited since it was added: no longer a known build's (`xr_intake::unchanged`, which hashes it
    /// again only when its size or time changed). One that can't be read counts as edited.
    fn dump_changed(&self) -> bool {
        match xr_intake::unchanged(&self.data, &self.cci) {
            Ok(same) => !same,
            Err(e) => {
                log::log(&format!("world: cannot check the game file {}: {e}", self.cci.display()));
                true
            }
        }
    }

    /// A world client for `addr`, the running game being `title` (`modded`: the mod that changes it); puppets on from then on.
    /// Every join comes here (the boot's world, the server browser, Steam's "Join game"), so this is where a run that
    /// may not join (`refusal`) is refused.
    fn start(&mut self, addr: &str, title: u64, modded: Option<&str>) -> Result<xr_net::Client, String> {
        // Steam worlds and lobbies take this PC's characters only: an online character stays on the official world
        if let (Some(why), true) = (lobby::local_only(self.account.is_some()), steam_world(addr)) {
            return Err(why.into());
        }
        if let Some(r) = refusal(&self.dev, modded, || self.dump_changed()) {
            return Err(r);
        }
        let steam = if self.account.is_some() { None } else { self.steam().cloned() };
        let identity = match (self.identity, &steam) {
            (Some(i), _) => i,
            (None, _) if self.account.is_some() => xr_net::id::Identity::Account(self.account.as_ref().map_or(0, |a| a.account_id)),
            (None, Some(s)) => xr_net::id::Identity::Steam(s.steam_id()),
            (None, None) if self.dev_id => xr_net::id::Identity::Dev(xr_net::id::load_or_create(&self.id_dir).map_err(|e| format!("player id: {e}"))?),
            (None, None) => return Err(xr_net::steam::START_STEAM.into()),
        };
        self.identity = Some(identity);
        // --name or the settings first; else the Steam persona name when Steam is up (the server cuts it to 16 characters), else the save's trainer name
        let name = self.name.clone().or_else(|| steam.as_ref().map(|s| s.client().friends().name()).filter(|n| !n.is_empty())).unwrap_or_else(|| trainer_name(&self.user, title));
        // other players draw us with our gender and outfit: the save's until the field is read (net_frame sends a newer one)
        let main = std::fs::read(self.user.join(format!("savedata/{title:016X}/main"))).ok();
        let appearance = main.as_deref().and_then(xr_net::x::appearance).map_or(Vec::new(), |a| a.to_bytes().to_vec());
        let auth = match identity {
            xr_net::id::Identity::Dev(id) => redex_proto::Auth::DevId(id),
            xr_net::id::Identity::Steam(steam_id) => redex_proto::Auth::SteamTicket { steam_id, ticket: Vec::new() },
            // a fresh ticket per connection (xr_net::account)
            xr_net::id::Identity::Account(_) => redex_proto::Auth::WorldTicket(String::new()),
        };
        // a dev id logs in without Steam even when Steam runs (a second instance on one PC)
        let steam = steam.filter(|_| matches!(identity, xr_net::id::Identity::Steam(_)));
        if steam.is_some() && !addr.starts_with("steam:") && !loopback(addr) {
            return Err("a Steam login goes over TCP only to this PC (127.0.0.1); join other worlds as steam:<server id>".into());
        }
        let kind = if self.account.is_some() { "account" } else if steam.is_some() { "Steam" } else { "dev id" };
        log::log(&format!("world: joining {addr} as {name:?} (title {title:016X}, {kind} login)"));
        // the player's characters go to the account after every login (the server keeps the newest copy of each); an
        // official-account login sends none of this PC's: online characters are the account's own, and nothing played
        // offline or in P2P worlds flows into them
        let characters = match self.account {
            Some(_) => Vec::new(),
            None => xr_frontend::start::characters::Store::load(&self.data).characters.into_iter().filter(|c| xr_net::x::field::Appearance::from_bytes(&c.look).is_some()).collect(),
        };
        let account = self.account.clone();
        let net = xr_net::Client::start(xr_net::Options { steam, account, appearance, characters, ..xr_net::Options::new(addr, auth, title, name) }).map_err(|e| format!("world: {e}"))?;
        // online: other players on our map become puppets (hooks.rs)
        let mut puppets = hooks::PUPPETS.lock().unwrap_or_else(|e| e.into_inner());
        if puppets.is_none() {
            *puppets = Some(xr_net::x::puppet::Puppets::new());
        }
        Ok(net)
    }
}

impl xr_frontend::Game for Windowed {
    fn run_frame(&mut self) -> FrameOutcome {
        if !self.presses.is_empty() {
            let s = input_at(&self.presses, self.n);
            let input = &mut self.m.system().services.hid.input;
            input.buttons |= s.buttons;
            input.touch = s.touch.or(input.touch);
            if (s.circle_x, s.circle_y) != (0.0, 0.0) {
                (input.circle_x, input.circle_y) = (s.circle_x, s.circle_y);
            }
        }
        let outcome = self.m.run_frame();
        if let Some(p) = &mut self.played {
            put_character(p.frame(&mut self.m), self.net.as_ref());
        }
        if let Some(c) = &mut self.net {
            net_frame(&mut self.m, c);
            let p2p = self.world.as_deref().is_some_and(|w| p2p_world(w, self.lobby_worlds.contains(w)));
            keep_accounts(c, &self.joiner.data, self.joiner.account.is_some() || p2p);
        }
        // refused for good (a lobby's host removed us or closed it, a ban): out of that world, offline
        if let Some(why) = self.net.as_ref().filter(|n| n.stopped).map(|n| n.refused.clone().unwrap_or_else(|| "the connection ended".into())) {
            log::log(&format!("world: left {}: {why}; playing offline", self.world.as_deref().unwrap_or("the world")));
            let own = self.lobby.address().is_some() && self.lobby.address() == self.world.as_deref();
            self.leave(format!("Left the world: {}", capitalized(&why)));
            if own && self.lobby.close() {
                log::log("lobby: closed (the host's own game left it)");
                self.notes.push("Your lobby closed: your game was no longer in it".into());
            }
        }
        self.settle_join();
        self.n += 1;
        status(&mut self.m, self.n, outcome != FrameOutcome::Completed);
        outcome
    }
    fn system(&mut self) -> &mut xr_hle::System {
        self.m.system()
    }

    fn online(&mut self) -> Option<xr_frontend::online::View> {
        use xr_frontend::online::{Peer, Server, View, Whisper};
        // Steam's "Join game" from the friends list while we run
        let joins = self.joiner.steam.as_ref().map(|s| s.take_joins()).unwrap_or_default();
        for addr in joins {
            if steam_world(&addr) {
                log::log(&format!("world: Steam asks to join {addr}"));
                self.join(&addr);
            } else {
                log::log(&format!("world: a Steam join request for {addr:?} was ignored (only steam:<server id>)"));
            }
        }
        self.presence();
        self.lobby_frame();
        for secret in self.discord.as_ref().map(discord::Handle::take_joins).unwrap_or_default() {
            // Discord's Join on a friend's profile: the same join as an invite (steam:<server id> only)
            if steam_world(&secret) {
                log::log(&format!("world: Discord asks to join {secret}"));
                self.join(&secret);
            } else {
                log::log(&format!("world: a Discord join for {secret:?} was ignored (only steam:<server id>)"));
            }
        }
        if let Some(d) = &self.discord {
            d.set_lobby(self.discord_lobby());
            d.set_mode(discord_mode(self.net.as_ref().is_some_and(|n| n.online), self.world.as_deref(), self.lobby.hosting()));
        }
        let typed = self.joiner.steam.as_ref().and_then(|s| s.take_typed()).flatten();
        let joining = self.pending.as_ref().map(|p| p.0.clone());
        let lobby_blocked = lobby_blocked(&self.joiner, &mut self.m);
        let mut view = View { world: self.world.clone(), notes: std::mem::take(&mut self.notes), joining, typed, lobby: self.lobby.view(), lobby_blocked, ..View::default() };
        if let Some((q, logged)) = &mut self.query {
            let (found, done) = q.results();
            if done && !*logged {
                *logged = true;
                log::log(&format!("world: Steam's server list: {} world(s) {:?}", found.len(), found.iter().map(|l| (l.id, &l.name, &l.tags)).collect::<Vec<_>>()));
            }
            view.searching = !done;
            view.servers = found.into_iter().map(|l| Server { address: format!("steam:{}", l.id), name: l.name, players: l.players, max: l.max, ping: l.ping }).collect();
            self.world_names.extend(view.servers.iter().filter(|s| !s.name.is_empty()).map(|s| (s.address.clone(), s.name.clone())));
        }
        self.friends_frame(&mut view);
        let Some(net) = &mut self.net else { return Some(view) };
        view.online = net.online;
        view.whispers = net.whispers.drain(..).map(|w| Whisper { from: w.from_session, steam: w.steam, name: w.name, text: w.text }).collect();
        // where the local player is, the field camera, and the puppets the game draws now
        let data = field_data(&mut self.m);
        let me = player(&mut self.m).map(|(p, _)| p.pos);
        let mem = &mut self.m.system().memory;
        let camera = data.and_then(|d| xr_net::x::field::camera(&mut Mem(mem), d));
        let drawn = hooks::try_puppets().and_then(|p| p.as_ref().map(|p| p.drawn(&mut Mem(mem)))).unwrap_or_default();
        view.peers = net
            .peers
            .iter()
            .map(|(&session, r)| {
                let at = r.track.back().map(|(_, p)| [p.x, p.y, p.z]);
                let tiles = match (me, at) {
                    (Some(m), Some(a)) => (m[0] - a[0]).hypot(m[2] - a[2]) / xr_net::x::puppet::TILE,
                    _ => f32::INFINITY,
                };
                // a name only over a puppet the game draws, with the field camera up (no field in a battle or a menu)
                let label = drawn.iter().find(|d| d.0 == session).zip(camera.as_ref()).and_then(|(d, c)| c.project([d.1[0], d.1[1] + xr_net::x::field::LABEL_HEIGHT, d.1[2]]));
                let version = match r.title {
                    redex_proto::TITLE_X => "X",
                    redex_proto::TITLE_Y => "Y",
                    _ => "",
                };
                Peer { session, name: r.name.clone(), steam: r.steam, version, tiles, label }
            })
            .collect();
        Some(view)
    }

    fn online_request(&mut self, request: xr_frontend::online::Request) {
        use xr_frontend::online::{Reason, Request};
        match request {
            Request::Whisper { to, name, text } => {
                if self.net.as_ref().is_some_and(|n| n.whisper(to, &text)) {
                    self.notes.push(format!("Whisper sent to {name}"));
                } else {
                    self.notes.push("Whisper not sent: not connected to a world".into());
                }
            }
            Request::Report { session, steam_id, name, reason, note } => {
                let reason = match reason {
                    Reason::Cheating => redex_proto::REPORT_CHEAT,
                    Reason::Harassment => redex_proto::REPORT_CHAT,
                    Reason::Name => redex_proto::REPORT_NAME,
                    Reason::Other => redex_proto::REPORT_OTHER,
                };
                if self.net.as_ref().is_some_and(|n| n.report(session, steam_id, reason, &note)) {
                    log::log(&format!("world: reported session {session} (reason {reason})"));
                    self.notes.push(format!("Report on {name} sent. Thank you"));
                } else {
                    self.notes.push("Report not sent: not connected to the world they are in".into());
                }
            }
            Request::AddFriend(id) => match self.joiner.steam() {
                Some(s) if s.add_friend(id) => {}
                Some(_) => self.notes.push("The Steam overlay is not available here: add them from Steam's friends list".into()),
                None => self.notes.push(format!("{}: Steam is needed to add friends", xr_net::steam::START_STEAM)),
            },
            Request::Search => match (self.joiner.dev_id, self.joiner.steam()) {
                (_, Some(s)) => match s.query_servers() {
                    Ok(q) => self.query = Some((q, false)),
                    Err(e) => self.notes.push(format!("Servers: {e}")),
                },
                (true, None) => self.notes.push("Servers: Steam's list is not asked with --dev-id (a test id); use Direct connect".into()),
                (false, None) => self.notes.push(format!("{} to see the servers", xr_net::steam::START_STEAM)),
            },
            Request::Join(addr) => self.join(&addr),
            Request::Type(text) => {
                let shown = self.joiner.steam().is_some_and(|s| s.type_with_gamepad("Message", xr_frontend::online::WHISPER_CHARS as u32, &text));
                if !shown {
                    self.notes.push("Connect a keyboard to type (Steam's on-screen keyboard is not available here)".into());
                }
            }
            Request::FriendChat { to, text } => {
                let sent = self.friends.as_ref().map_or(Err("Steam isn't running".to_string()), |f| f.send(to, &xr_net::friends::FriendMsg::Chat(text.clone())));
                if let Err(why) = sent {
                    log::log(&format!("friends: chat to {to} not sent: {why}"));
                    self.notes.push(format!("Message to {} not sent: {}", self.friend_name(to), why_for(&self.friend_name(to), &why)));
                    self.friend_failed.push((to, Some(text), why));
                }
            }
            Request::Invite { to } => {
                let online = self.net.as_ref().is_some_and(|n| n.online);
                let Some(connect) = self.world.clone().filter(|w| online && steam_world(w)) else {
                    self.notes.push("Invites need a Steam world: join one, then invite friends into it".into());
                    return;
                };
                let world = self.world_names.get(&connect).cloned().unwrap_or_else(|| format!("my world (\u{2026}{})", &connect[connect.len().saturating_sub(4)..]));
                let sent = self.friends.as_ref().map_or(Err("Steam isn't running".to_string()), |f| f.send(to, &xr_net::friends::FriendMsg::Invite { world, connect: connect.clone() }));
                let name = self.friend_name(to);
                log::log(&format!("friends: invite to {to} for {connect}: {}", sent.as_ref().map_or_else(|e| format!("not sent ({e})"), |()| "sent".into())));
                self.notes.push(match sent {
                    Ok(()) => format!("Invite sent to {name}"),
                    Err(why) => format!("Invite to {name} not sent: {}", why_for(&name, &why)),
                });
            }
            // the usual join: every check of Joiner::start (dev inputs, mods, an edited game, the world's lists)
            Request::AcceptInvite { from, connect } if steam_world(&connect) => {
                log::log(&format!("friends: joining {connect} on {from}'s invite"));
                self.join(&connect);
            }
            Request::AcceptInvite { connect, .. } => log::log(&format!("friends: an invite to {connect:?} was ignored (only steam:<server id>)")),
            Request::DeclineInvite { from } => {
                if let Some(Err(why)) = self.friends.as_ref().map(|f| f.send(from, &xr_net::friends::FriendMsg::Decline)) {
                    log::log(&format!("friends: Not now to {from} not sent: {why}"));
                }
            }
            Request::FriendsOpened => {
                if self.friends.is_none() && !self.joiner.dev_id {
                    // Steam may have been started since
                    let _ = self.joiner.steam();
                }
                self.link_friends();
                if let Some(f) = &self.friends {
                    f.refresh();
                    self.friend_list = f.list();
                    self.listed = Some(std::time::Instant::now());
                }
            }
            Request::Ignored(ids) => {
                if let Some(f) = &self.friends {
                    f.set_ignored(&ids);
                }
                self.ignored = Some(ids);
            }
            Request::HostLobby(s) => {
                let in_world = self.world.is_some() || self.pending.is_some();
                if let Err(why) = host_lobby(&mut self.lobby, &mut self.joiner, &mut self.m, in_world, &s) {
                    log::log(&format!("lobby: not hosting: {why}"));
                    self.notes.push(format!("Cannot host a lobby: {why}"));
                }
            }
            Request::Kick { session, name } => {
                let done = self.lobby.kick(session);
                log::log(&format!("lobby: remove session {session} ({name}): {}", if done { "removed" } else { "not in the lobby" }));
                self.notes.push(if done { format!("{name} was removed from your lobby") } else { format!("{name} is no longer in your lobby") });
            }
            Request::CloseLobby => {
                // our own game leaves first, so it is not told its own lobby closed
                if self.lobby.address().is_some_and(|a| self.world.as_deref() == Some(a)) {
                    self.leave(String::new());
                }
                if self.lobby.close() {
                    log::log("lobby: closed by the host");
                    self.notes.push("Your lobby is closed: everyone in it was told. Playing offline".into());
                }
            }
            Request::JoinCode(code) => match find_code(&mut self.joiner, &code) {
                Ok(f) => self.finding = Some(f),
                Err(why) => self.notes.push(format!("Lobby {}: {why}", redex_proto::room::shown(&code))),
            },
            Request::Leave => {
                log::log(&format!("world: left {} (the player chose to)", self.world.as_deref().unwrap_or("no world")));
                self.leave("You left the world: playing offline".into());
            }
        }
    }
}

impl Windowed {
    /// Out of the world we are in (and any being joined): its client says Bye, its puppets go; offline from now on.
    /// `note` for the player (none when empty).
    fn leave(&mut self, note: String) {
        (self.net, self.world, self.pending) = (None, None, None);
        clear_puppets(&mut self.m);
        if !note.is_empty() {
            self.notes.push(note);
        }
    }

    /// The lobby's news (an opened lobby is joined by its own game, the player staying where they are) and a room code's
    /// answer (joined when found).
    fn lobby_frame(&mut self) {
        let name = self.lobby.view().map(|v| v.settings.name);
        match lobby_news(&mut self.lobby) {
            Some(Ok((address, code))) => {
                self.world_names.insert(address.clone(), name.unwrap_or_default());
                self.lobby_worlds.insert(address.clone());
                self.notes.push(format!("Your lobby is open. Room code: {}", redex_proto::room::shown(&code)));
                self.join(&address);
            }
            Some(Err(why)) => self.notes.push(format!("The lobby did not start: {why}")),
            None => {}
        }
        if let Some(r) = self.finding.as_ref().and_then(|f| lobby::answered(&f.1)) {
            let (code, _) = self.finding.take().unwrap();
            match r {
                Ok(addr) => {
                    log::log(&format!("lobby: room code {code} is {addr}"));
                    self.lobby_worlds.insert(addr.clone());
                    self.join(&addr);
                }
                Err(why) => {
                    log::log(&format!("lobby: room code {code}: {why}"));
                    self.notes.push(format!("Lobby {}: {why}", redex_proto::room::shown(&code)));
                }
            }
        }
    }

    /// What Discord's Join button gets: the open lobby's world, when friends may come in by it (not a code-only lobby).
    fn discord_lobby(&self) -> Option<discord::Lobby> {
        let v = self.lobby.view().filter(|v| v.open && v.settings.access != xr_frontend::online::Access::Code)?;
        let secret = self.lobby.address().filter(|a| steam_world(a))?.to_owned();
        Some(discord::Lobby { secret, party: v.code, size: v.members.len() as u32 + 1, max: u32::from(v.settings.max) })
    }
    /// The Steam friends link, once Steam is up and the Ignore list was read (a fake one is there from the start).
    fn link_friends(&mut self) {
        use xr_net::friends::FriendsLink;
        if let (None, Some(s), Some(ignored)) = (&self.friends, &self.joiner.steam, &self.ignored) {
            let link = xr_net::friends::SteamFriends::new(s);
            link.set_ignored(ignored);
            self.friend_list = link.list();
            self.friends = Some(Box::new(link));
        }
    }

    fn friend_name(&self, steam: u64) -> String {
        self.friend_list.iter().find(|f| f.steam == steam).map_or_else(|| "that friend".into(), |f| f.name.clone())
    }

    /// Friends into the view: the list, the frames that came in, and where this account's Ignore list is kept.
    fn friends_frame(&mut self, view: &mut xr_frontend::online::View) {
        // per own Steam account; fakes (no Steam) keep theirs apart. The window reads it first (Request::Ignored)
        let own = self.joiner.steam.as_ref().map(|s| s.steam_id().to_string()).or_else(|| self.friends.as_ref().map(|_| "fake".into()));
        view.ignore_file = own.map(|own| self.joiner.id_dir.join(format!("ignored-{own}.txt")));
        self.link_friends();
        let Some(link) = &self.friends else { return };
        link.frame(self.n);
        let got = link.take();
        // someone not listed yet wrote, or the list is old: listed again (presence itself is asked only with the tab open)
        let stale = self.listed.is_none_or(|t| t.elapsed() >= xr_frontend::online::FRIENDS_AGAIN);
        if stale || got.iter().any(|(from, _)| !self.friend_list.iter().any(|f| f.steam == *from)) {
            self.friend_list = link.list();
            self.listed = Some(std::time::Instant::now());
        }
        for peer in link.take_failed() {
            let name = self.friend_name(peer);
            log::log(&format!("friends: Steam gave up on the session with {peer}"));
            self.notes.push(format!("Couldn't reach {name}: your last message may not have arrived"));
            self.friend_failed.push((peer, None, "the connection to them failed".into()));
        }
        for (from, m) in &got {
            let kind = match m {
                xr_net::friends::FriendMsg::Chat(_) => "chat",
                xr_net::friends::FriendMsg::Invite { .. } => "an invite",
                xr_net::friends::FriendMsg::Decline => "a decline",
            };
            log::log(&format!("friends: {kind} from {from}"));
        }
        (view.friends, view.friend_msgs, view.friend_failed) = (self.friend_list.clone(), got, std::mem::take(&mut self.friend_failed));
    }

    /// Starts joining `addr`; the world we are in stays until the new one logs us in (`settle_join`).
    fn join(&mut self, addr: &str) {
        let addr = addr.trim();
        if self.lobby.hosting() && self.lobby.address() != Some(addr) {
            self.notes.push("You are hosting a lobby: close it first to join another world".into());
            return;
        }
        if self.world.as_deref() == Some(addr) && self.net.is_some() || self.pending.as_ref().is_some_and(|p| p.0 == addr) {
            self.notes.push("Already in that world".into());
            return;
        }
        let (title, modded) = (self.m.title_id, self.m.system().game_modded.clone());
        match self.joiner.start(addr, title, modded.as_deref()) {
            // a join under way for another world is dropped (it says Bye)
            Ok(net) => self.pending = Some((addr.to_owned(), net, std::time::Instant::now())),
            Err(e) => {
                log::log(&format!("world: cannot join {addr}: {e}"));
                if self.lobby.address() == Some(addr) && self.lobby.close() {
                    log::log("lobby: closed (the host's own game could not join it)");
                    self.notes.push(format!("Your lobby closed: your game could not join it ({e})"));
                    return;
                }
                self.notes.push(format!("Cannot join {addr}: {e}"));
            }
        }
    }

    /// The world being joined logged us in: it replaces the old one (whose client says Bye as it drops; the old
    /// world's puppets go as its players vanish). Refused for good, or silent for JOIN_WAIT: given up, the old world kept.
    fn settle_join(&mut self) {
        let Some((_, net, since)) = &mut self.pending else { return };
        net.frame(None);
        let (online, gone) = (net.online, net.stopped || since.elapsed() >= JOIN_WAIT);
        if online {
            let (addr, net, _) = self.pending.take().unwrap();
            log::log(&format!("world: joined {addr}"));
            self.notes.push(format!("Joined {addr}"));
            self.net = Some(net);
            self.world = Some(addr);
        } else if gone {
            // a refusal says why (the world's allow or ban list, another version); silence is just unreachable
            let (addr, net, _) = self.pending.take().unwrap();
            let refused = net.refused.clone();
            log::log(&format!("world: {addr} did not log us in ({}); staying where we were", refused.as_deref().unwrap_or("no answer")));
            // our own lobby that our own game cannot get into: nobody would host it, so it closes
            if self.lobby.address() == Some(addr.as_str()) && self.lobby.close() {
                log::log("lobby: closed (the host's own game could not join it)");
                self.notes.push(format!("Your lobby closed: your game could not join it ({})", refused.as_deref().unwrap_or("no answer")));
                return;
            }
            self.notes.push(refused.map_or("Couldn't reach that world".into(), |r| format!("Cannot join: {}", capitalized(&r))));
        }
    }

    /// Steam's rich presence: "In Kalos" (the region; map names would need the game's text) and the world's
    /// `steam:<id>` as `connect`, so friends' "Join game" works; cleared offline. Set only when it changes.
    fn presence(&mut self) {
        let online = self.net.as_ref().is_some_and(|n| n.online);
        let region = redex_proto::title_info(self.m.title_id).map_or("Kalos", |(_, r)| match r {
            redex_proto::Region::Kalos => "Kalos",
            redex_proto::Region::Hoenn => "Hoenn",
        });
        // the world's name when Steam's list gave it (a friend's Friends tab shows it), and the region
        let shown = |w: &String| self.world_names.get(w).map_or(format!("In {region}"), |n| format!("{} \u{b7} In {region}", short_name(n)));
        let wanted = if online {
            (self.world.as_ref().map(shown), self.world.clone().filter(|w| w.starts_with("steam:")))
        } else {
            (Some("Playing offline".to_string()), None)
        };
        if let (Some(s), Some(wanted)) = (&self.joiner.steam, presence_to_set(&mut self.presence, wanted, self.joiner.steam.is_some())) {
            s.set_presence(wanted.0.as_deref(), wanted.1.as_deref());
            log::log(&format!("world: Steam presence {:?}, connect {:?}", wanted.0, wanted.1));
        }
    }
}

/// A panic on this thread ends the thread, not the game: the world client's and the lobby server's threads.
fn survives_panic(thread: &str) -> bool {
    thread.starts_with("redex-")
}

/// The Discord status's mode: a lobby or a Steam world is P2P, a TLS world the official one.
fn discord_mode(online: bool, world: Option<&str>, hosting: bool) -> discord::Mode {
    match world {
        _ if hosting => discord::Mode::P2p,
        Some(w) if online && w.starts_with("tls:") => discord::Mode::Official,
        Some(_) if online => discord::Mode::P2p,
        _ => discord::Mode::Offline,
    }
}

/// The puppets of a world we left go now (no world client is left to say so).
fn clear_puppets(m: &mut exec::Machine) {
    let map = player(m).map(|(p, _)| p.map);
    if let (Some(map), Some(mut lock)) = (map, hooks::try_puppets()) {
        if let Some(pp) = lock.as_mut() {
            pp.want(map, Vec::new());
        }
    }
}

/// Why this game may not host or join a lobby now: an online character (lobbies take this PC's characters only), or
/// what keeps it out of every world (test options, a mod that changes the game).
fn lobby_blocked(joiner: &Joiner, m: &mut exec::Machine) -> Option<String> {
    let modded = m.system().game_modded.clone();
    lobby::local_only(joiner.account.is_some()).map(str::to_owned).or_else(|| refusal(&joiner.dev, modded.as_deref(), || false))
}

/// Who hosts: a dev id on TCP (mp-tests), else Steam.
fn lobby_how(joiner: &mut Joiner) -> Result<lobby::How, String> {
    #[cfg(feature = "mp-tests")]
    if joiner.dev_id {
        let id = xr_net::id::load_or_create(&joiner.id_dir).map_err(|e| format!("player id: {e}"))?;
        return Ok(lobby::How::Dev { id, rooms: lobby::fake_rooms() });
    }
    joiner.steam().cloned().map(lobby::How::Steam).ok_or_else(|| format!("{} to host a lobby", xr_net::steam::START_STEAM))
}

/// "Host lobby" (the overlay's, and XR_MP_HOST's in a headless test): the same lockdown as any world first (shown in
/// the panel before hosting), then the server starts; `lobby_news` says when it is up.
fn host_lobby(lobby: &mut lobby::Lobby, joiner: &mut Joiner, m: &mut exec::Machine, in_world: bool, s: &xr_frontend::online::LobbySettings) -> Result<(), String> {
    if in_world {
        return Err("leave the world you are in first: a lobby starts from offline play".into());
    }
    if let Some(why) = lobby_blocked(joiner, m) {
        return Err(why);
    }
    if joiner.dump_changed() {
        return Err("this game file was changed since it was added; online is off".into());
    }
    let fallback = joiner.name.clone().unwrap_or_else(|| trainer_name(&joiner.user, m.title_id));
    let s = lobby::checked(s, &fallback);
    let how = lobby_how(joiner)?;
    log::log(&format!("lobby: starting {:?}, {} players, {:?}", s.name, s.max, s.access));
    lobby.host(s, how, joiner.data.join("lobby-world"))
}

/// The lobby's news: Ok(address, code) when it opened (logged), Err(why) when it did not start.
fn lobby_news(lobby: &mut lobby::Lobby) -> Option<Result<(String, String), String>> {
    match lobby.poll()? {
        lobby::Event::Opened { address, code } => {
            log::log(&format!("lobby: hosting on {address}, room code {code}"));
            Some(Ok((address, code)))
        }
        lobby::Event::Failed(why) => {
            log::log(&format!("lobby: did not start: {why}"));
            Some(Err(why))
        }
    }
}

/// Looks a room code up (the start screen's Join, the Lobby panel's): through Steam, or the fake rooms in mp-tests.
fn find_code(joiner: &mut Joiner, code: &str) -> Result<(String, xr_net::steam::Answer<String>), String> {
    if let Some(why) = lobby::local_only(joiner.account.is_some()) {
        return Err(why.into());
    }
    let fake = lobby::fake_rooms();
    let steam = if fake.is_none() { joiner.steam().cloned() } else { None };
    let found = lobby::find(code, steam.as_ref(), fake.as_deref())?;
    log::log(&format!("lobby: looking up room code {}", found.0));
    Ok(found)
}

/// What presence to send now: `wanted` when it differs from what was last sent and Steam is up to take it, recorded as
/// sent only then (so Steam coming up later still gets it, `redex` key included).
fn presence_to_set<P: PartialEq + Clone>(last: &mut Option<P>, wanted: P, steam: bool) -> Option<P> {
    (steam && last.as_ref() != Some(&wanted)).then(|| {
        *last = Some(wanted.clone());
        wanted
    })
}

/// A friend's send failure as a sentence: "isn't running RE:DEX" reads after their name, the others on their own.
fn why_for(name: &str, why: &str) -> String {
    if why.starts_with("isn't") || why.starts_with("runs ") { format!("{name} {why}") } else { why.to_owned() }
}

/// A server's reason with its first letter up ("Cannot join: You are banned ...").
fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// The Discord status's settings, read from the settings file again each time (a toggle in the menu shows within a minute).
fn windowed_discord_settings() -> discord::Config {
    let s = xr_frontend::Settings::load();
    discord::Config { status: s.discord_status, location: s.discord_location }
}

/// The status line, every 60 frames and at the end.
fn status(m: &mut exec::Machine, n: u64, last: bool) {
    if n.is_multiple_of(60) || last {
        // host frames per second since the previous status line
        static LAST: std::sync::Mutex<Option<(u64, std::time::Instant)>> = std::sync::Mutex::new(None);
        let now = std::time::Instant::now();
        let fps = LAST.lock().unwrap().replace((n, now)).map_or(0.0, |(n0, t0)| n.saturating_sub(n0) as f64 / (now - t0).as_secs_f64().max(1e-9));
        let s = m.system();
        log::log(&format!(
            "frame {n}: {fps:.1} fps, cycle {}, {} interpreted instructions from {} entry points, {} command lists | {} | services seen: {}",
            s.cpu.cycles,
            s.interpreted_instructions,
            progress::entry_count(),
            s.command_lists,
            s.status_line(),
            s.services_seen.iter().cloned().collect::<Vec<_>>().join(" ")
        ));
    }
}

/// The software renderer's screens as `<logs>/top<tag>.png` and `bottom<tag>.png`, logging any that is one flat colour.
fn save_screens(m: &mut exec::Machine, logs: &std::path::Path, tag: &str) {
    use zakuro_common::Screen;
    for (screen, name) in [(Screen::Top, "top"), (Screen::Bottom, "bottom")] {
        let rgba = m.screen(screen);
        let path = logs.join(format!("{name}{tag}.png"));
        let flat = rgba.chunks(4).all(|p| p[..3] == rgba[..3]);
        let r = std::fs::File::create(&path).map_err(|e| e.to_string()).and_then(|f| {
            let mut e = png::Encoder::new(std::io::BufWriter::new(f), screen.width(), screen.height());
            e.set_color(png::ColorType::Rgba);
            e.write_header().and_then(|mut w| w.write_image_data(&rgba)).map_err(|e| e.to_string())
        });
        match r {
            Err(e) => log::log(&format!("{}: {e}", path.display())),
            Ok(()) if flat => log::log(&format!("{}: one flat colour {:?}", path.display(), &rgba[..4])),
            Ok(()) => log::log(&format!("{}: saved", path.display())),
        }
    }
}

fn main() {
    // a windows-subsystem exe has no console; run from a terminal (dev, scripts) its output goes there. Handles a parent
    // gave it (pipes, files) are kept
    unsafe {
        use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE};
        if GetStdHandle(STD_ERROR_HANDLE).is_null() {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    // System is large and debug builds copy it on the stack while booting: the main loop gets a big stack too
    let r = std::thread::Builder::new().name("main loop".into()).stack_size(64 << 20).spawn(run).expect("spawn the main loop").join();
    if let Err(e) = r.unwrap_or_else(|_| Err("the main loop panicked".into())) {
        log::log(&format!("redex: {e}")); // stderr, and latest.log once it is open
        std::process::exit(1);
    }
}
