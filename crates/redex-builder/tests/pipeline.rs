use redex_builder::{build_state, BuildJob, BuildState, Event, RunTool, Seams, Stage, Toolchain};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const ID: &str = "x-usa-v0.2.0";

fn fake_exe() -> &'static PathBuf {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fake_tool.exe");
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_tool.rs");
        let st = std::process::Command::new("rustc").arg(&src).arg("-o").arg(&out).status().expect("rustc");
        assert!(st.success());
        out
    })
}

struct Env {
    root: PathBuf,
    tools: Toolchain,
    data: PathBuf,
    dump: PathBuf,
}

impl Env {
    fn state(&self) -> BuildState {
        build_state(&self.data, ID, Some(&self.tools.stamp()))
    }
}

fn setup(mode: &str) -> Env {
    setup_in("dump", "game.cci", mode)
}

fn setup_in(dump_dir: &str, dump_name: &str, mode: &str) -> Env {
    static N: AtomicU32 = AtomicU32::new(0);
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("p{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let tdir = root.join("tools");
    std::fs::create_dir_all(tdir.join("bin")).unwrap();
    std::fs::write(tdir.join("mode.txt"), mode).unwrap();
    for n in ["xr-recomp.exe", "redex.exe"] {
        std::fs::copy(fake_exe(), tdir.join(n)).unwrap();
    }
    let dump = root.join(dump_dir).join(dump_name);
    std::fs::create_dir_all(dump.parent().unwrap()).unwrap();
    std::fs::write(&dump, b"not a real dump").unwrap();
    Env {
        tools: Toolchain { bin: tdir.join("bin"), functions: root.join("functions"), recomp: tdir.join("xr-recomp.exe"), redex: tdir.join("redex.exe") },
        data: root.join("data"),
        dump,
        root,
    }
}

/// a fake unpack; with `<data>/sleep-unpack` it first runs a sleeping tool the way the real one runs ctrtool
fn fake_unpack(_: &Path, data: &Path, _: &xr_intake::KnownBuild, _: &str, _: &Path, run: RunTool) -> Result<PathBuf, xr_intake::Error> {
    if data.join("sleep-unpack").exists() {
        let mut c = std::process::Command::new(fake_exe());
        c.arg("--sleep");
        let (ok, said) = run(c).map_err(xr_intake::Error::Other)?;
        if !ok {
            return Err(xr_intake::Error::Other(said));
        }
    }
    let d = data.join("dumps").join("fake");
    std::fs::create_dir_all(&d).unwrap();
    Ok(d)
}

fn fake_seams() -> Seams {
    Seams { identify: |_| Ok((xr_intake::KNOWN[0], "0".repeat(64))), unpack: fake_unpack, free_bytes: |_| 100 << 30 }
}

fn start(e: &Env, seams: Seams) -> BuildJob {
    BuildJob::start_with(e.dump.clone(), e.data.clone(), e.tools.clone(), 2, seams)
}

fn next(job: &BuildJob) -> Event {
    job.events().recv_timeout(Duration::from_secs(60)).expect("event")
}

fn until_end(job: &BuildJob) -> Vec<Event> {
    let mut v = Vec::new();
    loop {
        let e = next(job);
        let end = matches!(e, Event::Done { .. } | Event::Failed { .. });
        v.push(e);
        if end {
            return v;
        }
    }
}

fn wait_for(job: &BuildJob, want: Event) {
    while next(job) != want {}
}

fn wait_progress(job: &BuildJob, n: u32) {
    wait_for(job, Event::Progress { done: n, total: 4 });
}

fn stages(ev: &[Event]) -> Vec<Stage> {
    ev.iter().filter_map(|e| if let Event::Stage(s) = e { Some(*s) } else { None }).collect()
}

#[test]
fn stages_report_in_order_and_done() {
    let e = setup("");
    let ev = until_end(&start(&e, fake_seams()));
    assert_eq!(stages(&ev), [Stage::Check, Stage::Unpack, Stage::Recompile, Stage::Build, Stage::Verify]);
    assert!(ev.contains(&Event::Identified { build_id: ID }));
    assert_eq!(ev.last(), Some(&Event::Done { build_id: ID, build_dir: e.data.join("build").join(ID) }));
    assert_eq!(e.state(), BuildState::Ready);
}

#[test]
fn ninja_progress_lines_become_progress_events() {
    let e = setup("big");
    let ev = until_end(&start(&e, fake_seams()));
    assert!(ev.contains(&Event::Progress { done: 12, total: 400 }), "{ev:?}");
}

#[test]
fn building_starts_with_running_ninja_and_its_total() {
    let e = setup("");
    let ev = until_end(&start(&e, fake_seams()));
    // the "running ninja" line switches to Build and gives the total (2 files + lookup + link) before any edge finishes
    let build = ev.iter().position(|x| *x == Event::Stage(Stage::Build)).unwrap();
    assert_eq!(ev[build + 1], Event::Progress { done: 0, total: 4 }, "{ev:?}");
}

#[test]
fn cancel_mid_build_leaves_partial_then_resume_finishes() {
    let e = setup("slow");
    let job = start(&e, fake_seams());
    wait_progress(&job, 2);
    job.cancel();
    let ev = until_end(&job);
    assert!(matches!(ev.last(), Some(Event::Failed { message, .. }) if message == "cancelled"), "{ev:?}");
    assert_eq!(e.state(), BuildState::Partial);

    std::fs::write(e.root.join("tools/mode.txt"), "").unwrap();
    let job = start(&e, fake_seams());
    let ev = until_end(&job);
    let first = ev.iter().find_map(|e| if let Event::Progress { done, .. } = e { Some(*done).filter(|&d| d > 0) } else { None });
    assert_eq!(first, Some(3), "resume should continue after step 2: {ev:?}");
    assert!(matches!(ev.last(), Some(Event::Done { .. })));
    assert_eq!(e.state(), BuildState::Ready);
}

#[test]
fn second_job_on_same_build_dir_is_refused() {
    let e = setup("slow");
    let first = start(&e, fake_seams());
    wait_progress(&first, 2);
    let ev = until_end(&start(&e, fake_seams()));
    assert!(matches!(ev.last(), Some(Event::Failed { stage: Stage::Check, message }) if message.contains("already building")), "{ev:?}");
    first.cancel();
    until_end(&first);
}

#[test]
fn stale_lock_file_is_ignored() {
    let e = setup("");
    let dir = e.data.join("build").join(ID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("building.lock"), "4000000").unwrap();
    assert!(matches!(until_end(&start(&e, fake_seams())).last(), Some(Event::Done { .. })));
}

#[test]
fn low_disk_space_fails_in_check_with_needed_gb() {
    let e = setup("");
    let seams = Seams { free_bytes: |_| 500_000_000, ..fake_seams() };
    let ev = until_end(&start(&e, seams));
    assert!(matches!(ev.last(), Some(Event::Failed { stage: Stage::Check, message }) if message.contains("GB")), "{ev:?}");
}

#[test]
fn low_disk_counts_what_the_build_already_has() {
    // 1 kB short of the 3 GB a build needs: a fresh build is refused, one that already wrote 2 kB goes on
    let seams = Seams { free_bytes: |_| 2_999_999_000, ..fake_seams() };
    let e = setup("");
    assert!(matches!(until_end(&start(&e, seams)).last(), Some(Event::Failed { stage: Stage::Check, .. })));
    let e = setup("");
    let dir = e.data.join("build").join(ID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("gen_000.o"), vec![0u8; 2000]).unwrap();
    assert!(matches!(until_end(&start(&e, seams)).last(), Some(Event::Done { .. })));
}

#[test]
fn cia_is_refused_with_intake_message() {
    let e = setup("");
    std::fs::write(&e.dump, 0x2020u32.to_le_bytes().repeat(64)).unwrap();
    let seams = Seams { identify: xr_intake::identify_hash, ..fake_seams() };
    let ev = until_end(&start(&e, seams));
    let want = xr_intake::Refusal::Cia.to_string();
    assert!(matches!(ev.last(), Some(Event::Failed { stage: Stage::Check, message }) if message.starts_with("We couldn't use this file") && message.contains(&want)), "{ev:?}");
}

#[test]
fn unicode_path_builds() {
    let e = setup_in("Zoë dir", "Pokémon X.cci", "");
    assert!(matches!(until_end(&start(&e, fake_seams())).last(), Some(Event::Done { .. })));
    let args = std::fs::read_to_string(e.root.join("tools/redex-args.txt")).unwrap();
    assert!(args.contains("Zoë dir") && args.contains("Pokémon X.cci") && args.contains("--headless"), "{args}");
}

#[test]
fn verify_runs_in_a_scratch_folder_inside_the_build() {
    let e = setup("");
    assert!(matches!(until_end(&start(&e, fake_seams())).last(), Some(Event::Done { .. })));
    let args = std::fs::read_to_string(e.root.join("tools/redex-args.txt")).unwrap();
    let build_dir = e.data.join("build").join(ID);
    // never the player's <data>/build/<id> (its saves and settings are <data>'s)
    let scratch = build_dir.join("test-run").join("build").join(ID);
    assert!(args.lines().any(|l| Path::new(l) == scratch), "{args}");
    assert!(!build_dir.join("test-run").exists(), "the scratch folder is removed");
}

#[test]
fn ready_build_is_not_rebuilt_and_an_outdated_one_is() {
    let e = setup("");
    until_end(&start(&e, fake_seams()));
    let ev = until_end(&start(&e, fake_seams()));
    assert_eq!(stages(&ev), [Stage::Check], "{ev:?}");
    assert!(matches!(ev.last(), Some(Event::Done { .. })));

    // a new xr-recomp: the build is outdated until it is built again
    let f = std::fs::File::options().write(true).open(&e.tools.recomp).unwrap();
    f.set_modified(std::time::SystemTime::now() + Duration::from_secs(3600)).unwrap();
    drop(f);
    assert_eq!(e.state(), BuildState::Outdated);
    let ev = until_end(&start(&e, fake_seams()));
    assert!(stages(&ev).contains(&Stage::Verify), "{ev:?}");
    assert_eq!(e.state(), BuildState::Ready);
}

fn alive(pid: u32) -> bool {
    let o = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\""))
}

fn pid_file(e: &Env, n: &str) -> u32 {
    std::fs::read_to_string(e.data.join("build").join(ID).join(n)).unwrap().trim().parse().unwrap()
}

fn wait_gone(pids: &[u32]) {
    for _ in 0..100 {
        if pids.iter().all(|p| !alive(*p)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("still running: {pids:?}");
}

#[test]
fn cancel_kills_grandchildren() {
    let e = setup("grand");
    let job = start(&e, fake_seams());
    wait_for(&job, Event::Progress { done: 1, total: 4 });
    let pids = [pid_file(&e, "child.pid"), pid_file(&e, "grand.pid")];
    assert!(pids.iter().all(|p| alive(*p)));
    job.cancel();
    until_end(&job);
    wait_gone(&pids);
}

#[test]
fn cancel_during_unpack_stops_the_unpacker() {
    let e = setup("");
    std::fs::create_dir_all(&e.data).unwrap();
    std::fs::write(e.data.join("sleep-unpack"), "").unwrap();
    let job = start(&e, fake_seams());
    wait_for(&job, Event::Stage(Stage::Unpack));
    std::thread::sleep(Duration::from_millis(500)); // the unpacker is running
    let t = Instant::now();
    job.cancel();
    let ev = until_end(&job);
    assert!(matches!(ev.last(), Some(Event::Failed { message, .. }) if message == "cancelled"), "{ev:?}");
    assert!(t.elapsed() < Duration::from_secs(10), "the unpacker (60 s) was not stopped");
}

#[test]
fn restart_right_after_a_cancel_takes_over() {
    // Build faster during the unpack: the old job is dropped and a new one starts at once on the same folder
    let e = setup("");
    std::fs::create_dir_all(&e.data).unwrap();
    std::fs::write(e.data.join("sleep-unpack"), "").unwrap();
    let job = start(&e, fake_seams());
    wait_for(&job, Event::Stage(Stage::Unpack));
    std::thread::sleep(Duration::from_millis(500));
    drop(job);
    std::fs::remove_file(e.data.join("sleep-unpack")).unwrap();
    let ev = until_end(&start(&e, fake_seams()));
    assert!(matches!(ev.last(), Some(Event::Done { .. })), "{ev:?}");
}

#[test]
fn dropping_job_cancels() {
    let e = setup("slow");
    let job = start(&e, fake_seams());
    wait_progress(&job, 2);
    let pid = pid_file(&e, "child.pid");
    drop(job);
    wait_gone(&[pid]);
    let lock = e.data.join("build").join(ID).join("building.lock");
    for _ in 0..100 {
        if std::fs::File::options().write(true).open(&lock).unwrap().try_lock().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("lock still held");
}
