//! The character creator's live preview: the frames a `redex --preview` child writes to its stdout. Each is `RDXF`,
//! the look's seq (u32), width and height (u16, little endian), then width x height RGBA pixels, top row first:
//! - v1: a head-to-toe crop of the top screen, opaque, at most 400x240;
//! - v2 (the width's top bit, [`STUDIO`], set): the studio view, only the player's model, its alpha the model's
//!   coverage, at most 512x512, and 8 more header bytes before the pixels: the floor shadow's ellipse (centre x, y and
//!   radii x, y in pixels, i16 each).
//!
//! The start screen's side: the `--make-preview` child that makes a gender's preview state ([`StateMaker`]), the
//! `--preview` child itself ([`PreviewChild`]), the two together ([`Session`]: a missing or stale state is made first,
//! and remade once when the child stops before its first picture), and the character cards' pictures ([`Cards`]).
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use xr_net::x::field::{Appearance, Facing};

use super::characters::Character;
use super::library::{label, GameEntry, Library};

const MAGIC: &[u8; 4] = b"RDXF";
/// The header flag of protocol v2, in the width: a studio frame.
pub const STUDIO: u16 = 0x8000;
/// The studio's backdrop, top to bottom (the creator's stage and the cards).
pub const STUDIO_TOP: [u8; 3] = [0x2C, 0x30, 0x3B];
pub const STUDIO_BOTTOM: [u8; 3] = [0x14, 0x17, 0x1E];
/// The floor shadow's darkness at its middle.
pub const SHADOW: f32 = 0.5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// the last look worn (0: the save state's own)
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub rgba: Vec<u8>,
    /// a studio frame (v2): the floor shadow under the model, centre x, y and radii x, y in pixels; None: a crop (v1)
    pub floor: Option<[i16; 4]>,
}

pub fn write_frame(w: &mut impl Write, f: &Frame) -> io::Result<()> {
    // a short or long pixel buffer would put every later frame out of step
    if f.rgba.len() != usize::from(f.width) * usize::from(f.height) * 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the pixels are not width x height"));
    }
    if f.width & STUDIO != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too wide"));
    }
    w.write_all(MAGIC)?;
    w.write_all(&f.seq.to_le_bytes())?;
    w.write_all(&(f.width | if f.floor.is_some() { STUDIO } else { 0 }).to_le_bytes())?;
    w.write_all(&f.height.to_le_bytes())?;
    if let Some(e) = f.floor {
        e.iter().try_for_each(|v| w.write_all(&v.to_le_bytes()))?;
    }
    w.write_all(&f.rgba)
}

/// How dark the floor shadow `e` makes the point (x, y): SHADOW at its middle, fading to 0 at its rim.
pub fn shade(e: [i16; 4], x: f32, y: f32) -> f32 {
    let [cx, cy, rx, ry] = e.map(f32::from);
    if rx <= 0.0 || ry <= 0.0 {
        return 0.0;
    }
    let d = ((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2);
    // soft all the way out: (1 - d)^2 has no edge
    SHADOW * (1.0 - d).max(0.0).powi(2)
}

/// The backdrop's colour `t` of the way down.
pub fn backdrop(t: f32) -> [f32; 3] {
    std::array::from_fn(|c| f32::from(STUDIO_TOP[c]) + (f32::from(STUDIO_BOTTOM[c]) - f32::from(STUDIO_TOP[c])) * t.clamp(0.0, 1.0))
}

/// A studio frame as a finished picture: the model over the backdrop and its floor shadow, opaque. A crop as it is.
pub fn studio_card(f: &Frame) -> Vec<u8> {
    let Some(e) = f.floor else { return f.rgba.clone() };
    let (w, h) = (usize::from(f.width), usize::from(f.height));
    let mut out = Vec::with_capacity(f.rgba.len());
    for (i, p) in f.rgba.chunks_exact(4).enumerate() {
        let (x, y) = ((i % w) as f32 + 0.5, (i / w) as f32 + 0.5);
        let k = 1.0 - shade(e, x, y);
        let a = f32::from(p[3]) / 255.0;
        let bg = backdrop(y / h as f32);
        out.extend((0..3).map(|c| (f32::from(p[c]) * a + bg[c] * k * (1.0 - a)).round() as u8));
        out.push(255);
    }
    out
}

/// Refuses a bad magic, or a frame larger than the top screen (400x240), or a studio frame larger than 512x512.
pub fn read_frame(r: &mut impl Read) -> io::Result<Frame> {
    let mut h = [0u8; 12];
    r.read_exact(&mut h)?;
    let (seq, width, height) = (u32::from_le_bytes(h[4..8].try_into().unwrap()), u16::from_le_bytes([h[8], h[9]]), u16::from_le_bytes([h[10], h[11]]));
    let (studio, width) = (width & STUDIO != 0, width & !STUDIO);
    let (most_w, most_h) = if studio { (512, 512) } else { (400, 240) };
    if &h[..4] != MAGIC || width > most_w || height > most_h {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a preview frame"));
    }
    let floor = if studio {
        let mut e = [0u8; 8];
        r.read_exact(&mut e)?;
        Some(std::array::from_fn(|i| i16::from_le_bytes([e[2 * i], e[2 * i + 1]])))
    } else {
        None
    };
    let mut rgba = vec![0; usize::from(width) * usize::from(height) * 4];
    r.read_exact(&mut rgba)?;
    Ok(Frame { seq, width, height, rgba, floor })
}

/// No picture this long after the start: the child is stopped (the player was not found, or it hangs).
const FIRST_FRAME: Duration = Duration::from_secs(30);
/// Windows priority classes
const BELOW_NORMAL: u32 = 0x4000;
const IDLE: u32 = 0x40;

/// `<data>/preview/<build_id>/<boy|girl>.state`
pub fn state_path(data: &Path, build_id: &str, gender: u8) -> PathBuf {
    data.join("preview").join(build_id).join(if gender == 1 { "girl.state" } else { "boy.state" })
}

/// The game the preview runs on: X when it is Ready, else Y when it is.
pub fn preview_game<'a>(lib: &'a Library, data: &Path) -> Option<&'a GameEntry> {
    let ready = |game: char| lib.games.iter().find(|g| g.build_id.starts_with(game) && label(data, &g.build_id, None) == "Ready");
    ready('x').or_else(|| ready('y'))
}

/// A child of ours, its stderr drained on a thread that hands back the last line. Killed when dropped, and with us: it
/// is in a job that kills it when the job closes.
struct Spawned {
    child: Child,
    last: Option<JoinHandle<String>>,
    #[cfg(windows)]
    _job: redex_builder::Job,
}

/// `exe args` with no console window at `priority`; stdin piped or none, stdout piped (the caller drains it).
fn spawn(exe: &Path, args: &[&std::ffi::OsStr], priority: u32, stdin: bool) -> Result<Spawned, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(if stdin { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000 | priority); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().map_err(|e| format!("could not start the preview: {e}"))?;
    #[cfg(windows)]
    let job = redex_builder::Job::new();
    #[cfg(windows)]
    job.assign(&child);
    let err = child.stderr.take().expect("piped");
    let last = std::thread::spawn(move || {
        let mut last = String::new();
        each_line(err, |l| if !l.is_empty() { last = l.to_string() });
        last
    });
    Ok(Spawned {
        child,
        last: Some(last),
        #[cfg(windows)]
        _job: job,
    })
}

/// every line of `r` until its end, trimmed; a child blocks in its write when its pipe is not read
fn each_line(r: impl Read, mut f: impl FnMut(&str)) {
    let (mut r, mut buf) = (BufReader::new(r), Vec::new());
    while r.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
        f(String::from_utf8_lossy(&buf).trim());
        buf.clear();
    }
}

impl Spawned {
    /// Once it has ended (the first call after): whether it exited 0, and why it stopped (its last line, without
    /// "redex: ", else its exit status).
    fn ended(&mut self) -> Option<(bool, String)> {
        let (ok, status) = match self.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(s)) => (s.success(), s.to_string()),
            Err(e) => (false, e.to_string()),
        };
        let last = self.last.take().and_then(|h| h.join().ok()).unwrap_or_default();
        let why = last.strip_prefix("redex: ").unwrap_or(&last);
        Some((ok, if why.is_empty() { format!("the preview stopped ({status})") } else { why.to_string() }))
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `redex --make-preview boy|girl` at idle priority, into a file of its own that becomes the state once it is whole.
pub struct StateMaker {
    p: Spawned,
    part: PathBuf,
    state: PathBuf,
    /// the fraction's f32 bits, from the progress lines
    progress: Arc<AtomicU32>,
    result: Option<Result<(), String>>,
}

impl StateMaker {
    pub fn start(exe: &Path, game: &GameEntry, data: &Path, gender: u8) -> Result<StateMaker, String> {
        static N: AtomicU32 = AtomicU32::new(0);
        let state = state_path(data, &game.build_id, gender);
        std::fs::create_dir_all(state.parent().expect("a folder")).map_err(|e| format!("could not make the preview folder: {e}"))?;
        // two makers (the cards' and the creator's) never write one file
        let part = state.with_extension(format!("{}-{}.part", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let build = data.join("build").join(&game.build_id);
        let g: &std::ffi::OsStr = if gender == 1 { "girl".as_ref() } else { "boy".as_ref() };
        let args = ["--make-preview".as_ref(), g, "--out".as_ref(), part.as_os_str(), "--cci".as_ref(), game.dump.as_os_str(), "--build".as_ref(), build.as_os_str()];
        let mut p = spawn(exe, &args, IDLE, false)?;
        let progress = Arc::new(AtomicU32::new(0));
        let (out, at) = (p.child.stdout.take().expect("piped"), progress.clone());
        std::thread::spawn(move || {
            each_line(out, |l| {
                let done = l.strip_prefix("progress ").and_then(|f| f.split_once('/')).and_then(|(n, t)| Some(n.parse::<f32>().ok()? / t.parse::<f32>().ok()?));
                if let Some(f) = done.filter(|f| f.is_finite()) {
                    at.store(f.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
                }
            })
        });
        Ok(StateMaker { p, part, state, progress, result: None })
    }

    /// None while it runs; then whether the state was made (why not: the child's last line)
    pub fn poll(&mut self) -> Option<Result<(), String>> {
        if self.result.is_none() {
            let (ok, why) = self.p.ended()?;
            let kept = || std::fs::rename(&self.part, &self.state).map_err(|e| format!("could not keep the preview state: {e}"));
            self.result = Some(if ok { kept() } else { Err(why) });
            let _ = std::fs::remove_file(&self.part);
        }
        self.result.clone()
    }

    /// 0 to 1, from its `progress N/T` lines
    pub fn fraction(&self) -> f32 {
        if matches!(self.result, Some(Ok(()))) { 1.0 } else { f32::from_bits(self.progress.load(Ordering::Relaxed)) }
    }
}

impl Drop for StateMaker {
    fn drop(&mut self) {
        let _ = self.p.child.kill();
        let _ = self.p.child.wait();
        let _ = std::fs::remove_file(&self.part);
    }
}

/// `redex --preview STATE` at below-normal priority: looks and turns in on its stdin, frames out of its stdout.
pub struct PreviewChild {
    p: Spawned,
    stdin: Option<ChildStdin>,
    /// the newest two frames (the reader drops the oldest), and whether one ever came
    frames: Arc<Mutex<(VecDeque<Frame>, bool)>>,
    gender: u8,
    started: Instant,
    /// no picture by then: stopped (tests shorten it)
    first_frame_by: Duration,
    stopped: Option<String>,
}

impl PreviewChild {
    /// Starts the child on the gender's state, which is not checked here (`Session` makes it first when it is stale).
    pub fn start(exe: &Path, game: &GameEntry, data: &Path, gender: u8) -> Result<PreviewChild, String> {
        let (state, build) = (state_path(data, &game.build_id, gender), data.join("build").join(&game.build_id));
        let args = ["--preview".as_ref(), state.as_os_str(), "--cci".as_ref(), game.dump.as_os_str(), "--build".as_ref(), build.as_os_str()];
        let mut p = spawn(exe, &args, BELOW_NORMAL, true)?;
        let (stdin, out) = (p.child.stdin.take(), p.child.stdout.take().expect("piped"));
        let frames = Arc::new(Mutex::new((VecDeque::new(), false)));
        let q = frames.clone();
        // read until the child's end, whatever the window does: a child whose frames are not read stops in its write
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            loop {
                match read_frame(&mut r) {
                    Ok(f) => {
                        let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
                        q.0.push_back(f);
                        if q.0.len() > 2 {
                            q.0.pop_front();
                        }
                        q.1 = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(e) => {
                        log::warn!("preview: {e}; its frames are ignored from here");
                        let _ = io::copy(&mut r, &mut io::sink());
                        break;
                    }
                }
            }
        });
        Ok(PreviewChild { p, stdin, frames, gender, started: Instant::now(), first_frame_by: FIRST_FRAME, stopped: None })
    }

    fn send(&mut self, line: &str) -> Result<(), String> {
        let w = self.stdin.as_mut().ok_or("the preview has stopped")?;
        writeln!(w, "{line}").and_then(|()| w.flush()).map_err(|e| format!("the preview has stopped: {e}"))
    }

    /// Puts the look on; frames carry `seq` once it is worn.
    pub fn look(&mut self, seq: u32, a: &Appearance) -> Result<(), String> {
        let hex: String = a.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        self.send(&format!("look {seq} {hex}"))
    }

    /// The studio view's camera: `yaw` degrees (0 faces us), `zoom` 1 (the whole body) to `zakuro_gpu::studio::ZOOM_MAX`.
    pub fn orbit(&mut self, yaw: f32, zoom: f32) -> Result<(), String> {
        self.send(&format!("orbit {yaw:.2} {zoom:.3}"))
    }

    /// Turns the player in place.
    pub fn face(&mut self, f: Facing) -> Result<(), String> {
        self.send(match f {
            Facing::Up => "face up",
            Facing::Down => "face down",
            Facing::Left => "face left",
            Facing::Right => "face right",
        })
    }

    /// The newest frame since the last call, if one came. The frame's seq is the last look worn: one older than the
    /// newest `look` is not that look yet (a dropped look leaves the state's own outfit on, under the older seq).
    pub fn latest(&mut self) -> Option<Frame> {
        let mut q = self.frames.lock().unwrap_or_else(|e| e.into_inner());
        let f = q.0.pop_back();
        q.0.clear();
        f
    }

    pub fn gender(&self) -> u8 {
        self.gender
    }

    /// the child's process id: a new child (a remade state) has another
    pub fn pid(&self) -> u32 {
        self.p.child.id()
    }

    fn shown(&self) -> bool {
        self.frames.lock().unwrap_or_else(|e| e.into_inner()).1
    }

    /// Why it stopped, once it did: its last line, or no picture in time (then it is stopped here).
    pub fn exited(&mut self) -> Option<String> {
        if self.stopped.is_none() {
            self.stopped = self.p.ended().map(|(_, why)| why);
            if self.stopped.is_none() && !self.shown() && self.started.elapsed() > self.first_frame_by {
                let _ = self.p.child.kill();
                let _ = self.p.child.wait();
                self.stopped = Some(format!("the preview showed no picture in {} s", self.first_frame_by.as_secs_f32()));
            }
            if self.stopped.is_some() {
                self.stdin = None;
            }
        }
        self.stopped.clone()
    }
}

impl Drop for PreviewChild {
    fn drop(&mut self) {
        let _ = self.send("quit");
        self.stdin = None; // stdin's end quits it too
        let t = Instant::now();
        while matches!(self.p.child.try_wait(), Ok(None)) && t.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(10));
        }
        // then `Spawned` kills what is left
    }
}

/// One gender's preview from the state to a running child. A missing or stale state (`state_matches`) is made first,
/// and a child that stops before its first picture gets its state made again once; after that its reason is the error.
pub struct Session {
    gender: u8,
    stage: Stage,
    remade: bool,
}

enum Stage {
    /// `state_matches`, on a thread: it hashes the 100 MB DLL
    Checking(std::sync::mpsc::Receiver<bool>),
    Making(StateMaker),
    Running(PreviewChild),
    Failed(String),
}

impl Session {
    pub fn start(game: &GameEntry, data: &Path, gender: u8) -> Session {
        let (state, dll) = (state_path(data, &game.build_id, gender), data.join("build").join(&game.build_id).join("xgame.dll"));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(redex_builder::state_matches(&state, &dll)));
        Session { gender, stage: Stage::Checking(rx), remade: false }
    }

    /// Moves it on: the child once it runs, None while the state is checked or made, Err once it failed for good.
    pub fn pump(&mut self, exe: &Path, game: &GameEntry, data: &Path) -> Result<Option<&mut PreviewChild>, String> {
        let g = self.gender;
        let make = || StateMaker::start(exe, game, data, g).map_or_else(Stage::Failed, Stage::Making);
        let run = || PreviewChild::start(exe, game, data, g).map_or_else(Stage::Failed, Stage::Running);
        let next = match &mut self.stage {
            Stage::Checking(rx) => match rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Ok(true) => Some(run()),
                _ => Some(make()),
            },
            Stage::Making(m) => m.poll().map(|r| r.map_or_else(Stage::Failed, |()| run())),
            Stage::Running(c) => match c.exited() {
                Some(_) if !c.shown() && !self.remade => {
                    self.remade = true;
                    Some(make())
                }
                why => why.map(Stage::Failed),
            },
            Stage::Failed(_) => None,
        };
        if let Some(s) = next {
            self.stage = s;
        }
        match &mut self.stage {
            Stage::Running(c) => Ok(Some(c)),
            Stage::Failed(e) => Err(e.clone()),
            _ => Ok(None),
        }
    }

    /// how far the state is made, while it is
    pub fn making(&self) -> Option<f32> {
        match &self.stage {
            Stage::Making(m) => Some(m.fraction()),
            _ => None,
        }
    }

    pub fn gender(&self) -> u8 {
        self.gender
    }
}

/// `<data>/characters/<id:016x>/studio-<look hex>.png`: the character's picture in that look, the studio's front view
/// (the older `card-` pictures were crops of the bedroom: drawn again, so the cards look alike)
pub fn card_path(data: &Path, c: &Character) -> PathBuf {
    let hex: String = c.look.iter().map(|b| format!("{b:02x}")).collect();
    data.join("characters").join(format!("{:016x}", c.id)).join(format!("studio-{hex}.png"))
}

/// the characters whose look has no card picture yet
pub fn stale_cards(data: &Path, all: &[Character]) -> Vec<Character> {
    all.iter().filter(|c| !card_path(data, c).is_file()).cloned().collect()
}

/// The frame as a PNG, whole or not at all (a studio frame on its backdrop, [`studio_card`]).
pub fn save_card(path: &Path, f: &Frame) -> Result<(), String> {
    let tmp = path.with_extension("png.new");
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
        let mut e = png::Encoder::new(std::fs::File::create(&tmp)?, f.width.into(), f.height.into());
        e.set_color(png::ColorType::Rgba);
        let mut w = e.write_header().map_err(io::Error::other)?;
        w.write_image_data(&studio_card(f)).and_then(|()| w.finish()).map_err(io::Error::other)?;
        std::fs::rename(&tmp, path)
    });
    written.map_err(|e| format!("{}: {e}", path.display()))
}

/// Draws the card pictures of the characters given, one preview child at a time.
pub struct Cards {
    todo: VecDeque<Character>,
    session: Option<Session>,
    seq: u32,
    /// the child (pid) and the character whose look it was sent
    sent: Option<(u32, u64)>,
}

impl Cards {
    pub fn new(mut stale: Vec<Character>) -> Cards {
        // the boys, then the girls: one child per gender
        stale.sort_by_key(|c| Appearance::gender_of(&c.look));
        Cards { todo: stale.into(), session: None, seq: 0, sent: None }
    }

    /// pictures still to draw
    pub fn pending(&self) -> bool {
        !self.todo.is_empty()
    }

    /// Moves it on (call it every frame): Some(id) when that character's card was written. A preview that fails ends it
    /// (logged). The child goes once every card is drawn.
    pub fn pump(&mut self, exe: &Path, game: &GameEntry, data: &Path) -> Option<u64> {
        let Some(c) = self.todo.front().cloned() else {
            self.session = None;
            return None;
        };
        let Some(a) = Appearance::from_bytes(&c.look) else {
            self.todo.pop_front(); // the store never keeps one
            return None;
        };
        if self.session.as_ref().is_none_or(|s| s.gender() != a.gender) {
            self.session = None; // the other gender's child goes first
            self.session = Some(Session::start(game, data, a.gender));
        }
        let child = match self.session.as_mut()?.pump(exe, game, data) {
            Ok(Some(child)) => child,
            Ok(None) => return None,
            Err(e) => {
                log::warn!("cards: no pictures this time: {e}");
                (self.todo, self.session) = (VecDeque::new(), None);
                return None;
            }
        };
        let pid = child.p.child.id();
        if self.sent != Some((pid, c.id)) {
            self.seq += 1;
            // a child that is gone: the session says so on the next pump. A new child turns to the front first (the
            // states face right; a turn shows in 0.1 s, a look in 2 s)
            if self.sent.is_none_or(|(p, _)| p != pid) {
                child.face(Facing::Down).ok()?;
            }
            child.look(self.seq, &a).ok()?;
            self.sent = Some((pid, c.id));
        }
        // the look worn, not a frame from before it
        let f = child.latest().filter(|f| f.seq == self.seq)?;
        self.todo.pop_front();
        if self.todo.is_empty() {
            self.session = None;
        }
        if let Err(e) = keep_card(data, &c, &f) {
            log::warn!("cards: {e}");
            return None;
        }
        Some(c.id)
    }
}

/// The frame as the character's card picture; the pictures of its older looks go.
pub fn keep_card(data: &Path, c: &Character, f: &Frame) -> Result<(), String> {
    let path = card_path(data, c);
    save_card(&path, f)?;
    for e in path.parent().and_then(|d| std::fs::read_dir(d).ok()).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if (name.starts_with("card-") || name.starts_with("studio-")) && e.path() != path {
            let _ = std::fs::remove_file(e.path());
        }
    }
    Ok(())
}

/// Both preview states made in the background after a build, one at a time (the boy's, then the girl's), so the
/// creator opens on a ready state. A state that is there and matches the build is left alone.
pub struct Warm {
    /// the genders whose state is missing or stale, from a thread (it hashes the DLL)
    check: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    todo: VecDeque<u8>,
    maker: Option<StateMaker>,
}

impl Warm {
    pub fn start(game: &GameEntry, data: &Path) -> Warm {
        let (data, id) = (data.to_path_buf(), game.build_id.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let dll = data.join("build").join(&id).join("xgame.dll");
            tx.send((0..2).filter(|&g| !redex_builder::state_matches(&state_path(&data, &id, g), &dll)).collect())
        });
        Warm { check: Some(rx), todo: VecDeque::new(), maker: None }
    }

    /// Moves it on (call it every frame); false once both states are there. A state that could not be made is logged
    /// and skipped (the creator makes it again when it opens).
    pub fn pump(&mut self, exe: &Path, game: &GameEntry, data: &Path) -> bool {
        if let Some(rx) = &self.check {
            match rx.try_recv() {
                Ok(todo) => (self.todo, self.check) = (todo.into(), None),
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(_) => return false,
            }
        }
        if let Some(m) = &mut self.maker {
            match m.poll() {
                None => return true,
                Some(r) => {
                    if let Err(e) = r {
                        log::warn!("preview: the {} state was not made: {e}", if self.todo.front() == Some(&1) { "girl's" } else { "boy's" });
                    }
                    self.maker = None;
                    self.todo.pop_front();
                }
            }
        }
        let Some(&g) = self.todo.front() else { return false };
        match StateMaker::start(exe, game, data, g) {
            Ok(m) => self.maker = Some(m),
            Err(e) => {
                log::warn!("preview: {e}");
                self.todo.pop_front();
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let a = Frame { seq: 7, width: 3, height: 2, rgba: (0..24).collect(), floor: None };
        let b = Frame { seq: 0, width: 400, height: 240, rgba: vec![9; 400 * 240 * 4], floor: None };
        let mut buf = Vec::new();
        write_frame(&mut buf, &a).unwrap();
        write_frame(&mut buf, &b).unwrap();
        assert_eq!(&buf[..12], b"RDXF\x07\0\0\0\x03\0\x02\0");
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), a);
        assert_eq!(read_frame(&mut r).unwrap(), b);
        assert!(r.is_empty());
    }

    #[test]
    fn studio_frames_carry_alpha_and_the_floor() {
        // v2: the flag in the width, the floor after the header, larger than the top screen
        let s = Frame { seq: 9, width: 360, height: 480, rgba: (0..360 * 480 * 4).map(|i| i as u8).collect(), floor: Some([180, 452, 60, -14]) };
        let v1 = Frame { seq: 9, width: 2, height: 1, rgba: vec![1; 8], floor: None };
        let mut buf = Vec::new();
        write_frame(&mut buf, &s).unwrap();
        write_frame(&mut buf, &v1).unwrap();
        assert_eq!(&buf[..20], b"RDXF\x09\0\0\0\x68\x81\xE0\x01\xB4\0\xC4\x01\x3C\0\xF2\xFF");
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), s);
        assert_eq!(read_frame(&mut r).unwrap(), v1, "a crop after a studio frame");
        assert!(r.is_empty());
        // too large for v2 too; a v1 frame is still held to the top screen
        for (w, h) in [(513u16, 1u16), (1, 513)] {
            let mut b = b"RDXF\x01\0\0\0".to_vec();
            b.extend_from_slice(&(w | STUDIO).to_le_bytes());
            b.extend_from_slice(&h.to_le_bytes());
            b.extend_from_slice(&[0; 8]);
            assert_eq!(read_frame(&mut &b[..]).unwrap_err().kind(), io::ErrorKind::InvalidData, "{w}x{h}");
        }
        let mut b = b"RDXF\x01\0\0\0".to_vec();
        b.extend_from_slice(&360u16.to_le_bytes());
        b.extend_from_slice(&480u16.to_le_bytes());
        assert_eq!(read_frame(&mut &b[..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_studio_card_is_the_model_on_the_backdrop() {
        // 4x8: a model pixel at (1, 2), the shadow around (2, 6)
        let mut rgba = vec![0u8; 4 * 8 * 4];
        rgba[(2 * 4 + 1) * 4..][..4].copy_from_slice(&[200, 100, 50, 255]);
        let f = Frame { seq: 1, width: 4, height: 8, rgba, floor: Some([2, 6, 2, 1]) };
        let c = studio_card(&f);
        let px = |x: usize, y: usize| &c[(y * 4 + x) * 4..][..4];
        assert_eq!(px(1, 2), [200, 100, 50, 255], "the model as it is");
        assert!(c.chunks(4).all(|p| p[3] == 255), "opaque");
        // the backdrop: the top colour at the top, darker toward the bottom
        assert!(px(0, 0)[..3].iter().zip(STUDIO_TOP).all(|(a, b)| a.abs_diff(b) <= 3), "{:?}", px(0, 0));
        assert!(px(0, 7)[0] < px(0, 0)[0]);
        // the shadow darkens the floor under the feet, most in its middle and not outside it
        assert!(px(2, 6)[0] < px(0, 6)[0], "{:?} {:?}", px(2, 6), px(0, 6));
        assert!((shade([2, 6, 2, 1], 2.0, 6.0) - SHADOW).abs() < 1e-6);
        assert_eq!(shade([2, 6, 2, 1], 5.0, 6.0), 0.0);
        assert_eq!(shade([2, 6, 0, 0], 2.0, 6.0), 0.0, "no floor");
        // a crop is kept as it was
        let crop = Frame { seq: 1, width: 1, height: 1, rgba: vec![1, 2, 3, 255], floor: None };
        assert_eq!(studio_card(&crop), crop.rgba);
    }

    #[test]
    fn a_bad_or_huge_frame_is_refused() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame { seq: 1, width: 1, height: 1, rgba: vec![0; 4], floor: None }).unwrap();
        buf[0] = b'X';
        assert_eq!(read_frame(&mut &buf[..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
        for (w, h) in [(401u16, 1u16), (1, 241)] {
            let mut b = b"RDXF\x01\0\0\0".to_vec();
            b.extend_from_slice(&w.to_le_bytes());
            b.extend_from_slice(&h.to_le_bytes());
            assert_eq!(read_frame(&mut &b[..]).unwrap_err().kind(), io::ErrorKind::InvalidData, "{w}x{h}");
        }
        // cut short: the pixels are not all there
        let mut b = Vec::new();
        write_frame(&mut b, &Frame { seq: 1, width: 2, height: 2, rgba: vec![0; 16], floor: None }).unwrap();
        assert!(read_frame(&mut &b[..b.len() - 1]).is_err());
    }

    // ---- the fake children ----
    //
    // The test binary, started by the code under test as it would start redex.exe, is the fake child when
    // XR_FAKE_PREVIEW / XR_FAKE_MAKER is set. It answers from a CRT initializer, before libtest's main: libtest prints
    // "running N tests" to stdout first, which would put the frame stream out of step.
    //
    // The fake `--preview` acts on its state's body (after the 40-byte header): "die" exits 1 before a frame as a stale
    // state does, "mute" sends no frame until stdin closes (the player not found), "deaf" ignores stdin and never exits,
    // anything else sends a frame of seq 0, then one per `look` (19x1 pixels, each the look's byte) and `face`. The fake
    // `--make-preview` prints progress lines and writes a state for the build's xgame.dll whose body is
    // `<data>/fake-maker-body` (else "ok"). Both append "start <what> <gender>" and "end <what>" to `<data>/fake.log`.

    #[used]
    #[link_section = ".CRT$XCU"]
    static FAKE_CHILD: extern "C" fn() = fake_child;

    extern "C" fn fake_child() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let fake = |var: &str, flag: &str| std::env::var_os(var).is_some() && args.first().is_some_and(|a| a == flag);
        if fake("XR_FAKE_PREVIEW", "--preview") {
            fake_preview(&args)
        } else if fake("XR_FAKE_MAKER", "--make-preview") {
            fake_maker(&args)
        }
    }

    fn arg<'a>(args: &'a [String], flag: &str) -> &'a str {
        &args[args.iter().position(|a| a == flag).unwrap() + 1]
    }

    fn fake_log(args: &[String], line: &str) {
        let data = Path::new(arg(args, "--build")).parent().unwrap().parent().unwrap();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(data.join("fake.log")).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn fake_preview(args: &[String]) -> ! {
        let state = std::fs::read(arg(args, "--preview")).unwrap_or_default();
        let g = if arg(args, "--preview").ends_with("girl.state") { 1 } else { 0 };
        fake_log(args, &format!("start preview {g}"));
        let end = |code| -> ! {
            fake_log(args, "end preview");
            std::process::exit(code)
        };
        match state.get(40..).unwrap_or_default() {
            b"die" => {
                eprintln!("preview: loading");
                eprintln!("redex: the save state was made with another xgame.dll; make it again");
                end(1)
            }
            b"mute" => {
                let _ = io::copy(&mut io::stdin(), &mut io::sink());
                end(0)
            }
            b"deaf" => loop {
                std::thread::sleep(Duration::from_secs(1))
            },
            _ => {}
        }
        let mut out = io::stdout().lock();
        let mut send = |seq: u32, rgba: Vec<u8>| {
            let f = Frame { seq, width: (rgba.len() / 4) as u16, height: 1, rgba, floor: None };
            write_frame(&mut out, &f).and_then(|()| out.flush()).is_ok()
        };
        let (mut seq, mut pixels) = (0, vec![0u8; 19 * 4]);
        send(seq, pixels.clone());
        for line in io::stdin().lock().lines() {
            let line = line.unwrap_or_default();
            let w: Vec<&str> = line.split_whitespace().collect();
            match w[..] {
                ["quit"] => break,
                ["look", s, hex] => {
                    seq = s.parse().unwrap();
                    pixels = (0..19).flat_map(|i| [u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap(); 4]).collect();
                }
                ["face", _] | ["orbit", _, _] => {}
                _ => panic!("the fake preview got {line:?}"),
            }
            if !send(seq, pixels.clone()) {
                break;
            }
        }
        end(0)
    }

    fn fake_maker(args: &[String]) -> ! {
        let g = arg(args, "--make-preview");
        fake_log(args, &format!("start maker {g}"));
        let build = Path::new(arg(args, "--build"));
        for n in (100..=6500).step_by(100) {
            println!("progress {n}/6520");
        }
        std::thread::sleep(Duration::from_millis(100)); // the test sees the progress
        let body = std::fs::read(build.parent().unwrap().parent().unwrap().join("fake-maker-body")).unwrap_or(b"ok".to_vec());
        std::fs::write(arg(args, "--out"), state_for(&build.join("xgame.dll"), &body)).unwrap();
        fake_log(args, "end maker");
        std::process::exit(0)
    }

    /// a save state header for `dll`, then `body`
    fn state_for(dll: &Path, body: &[u8]) -> Vec<u8> {
        [&redex_builder::STATE_MAGIC[..], &redex_builder::dll_fingerprint(dll).unwrap(), body].concat()
    }

    /// a data folder with a built X (xgame.dll "dll-<name>") and the fakes on; the game and the exe to start
    fn setup(name: &str) -> (PathBuf, GameEntry, PathBuf) {
        // every test sets the same values: the fakes act on their files, not on these
        std::env::set_var("XR_FAKE_PREVIEW", "1");
        std::env::set_var("XR_FAKE_MAKER", "1");
        let data = std::env::temp_dir().join(format!("redex-preview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let game = GameEntry { build_id: "x-usa-v0.2.0".into(), dump: data.join("x.cci") };
        std::fs::create_dir_all(data.join("build/x-usa-v0.2.0")).unwrap();
        std::fs::write(data.join("build/x-usa-v0.2.0/xgame.dll"), format!("dll-{name}")).unwrap();
        (data, game, std::env::current_exe().unwrap())
    }

    /// writes the gender's state with `body`, made for the build's xgame.dll
    fn put_state(data: &Path, gender: u8, body: &[u8]) {
        let p = state_path(data, "x-usa-v0.2.0", gender);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, state_for(&data.join("build/x-usa-v0.2.0/xgame.dll"), body)).unwrap();
    }

    fn fake_log_lines(data: &Path) -> Vec<String> {
        std::fs::read_to_string(data.join("fake.log")).unwrap_or_default().lines().map(String::from).collect()
    }

    fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let t = Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "timed out: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// the default boy with hair colour `hair`
    fn boy(hair: u8) -> Appearance {
        xr_net::x::look::Look { hair_colour: hair, ..xr_net::x::look::Look::default_for(0) }.appearance().unwrap()
    }

    fn pixels_of(a: &Appearance) -> Vec<u8> {
        a.to_bytes().iter().flat_map(|&b| [b; 4]).collect()
    }

    fn running(pid: u32) -> bool {
        let o = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().unwrap();
        String::from_utf8_lossy(&o.stdout).contains(&format!(" {pid} "))
    }

    // ---- the client ----

    #[test]
    fn frames_come_back_for_each_look() {
        let (data, game, exe) = setup("looks");
        put_state(&data, 0, b"ok");
        let mut c = PreviewChild::start(&exe, &game, &data, 0).unwrap();
        assert_eq!(c.gender(), 0);
        assert_eq!(until("the state's own frame", || c.latest()).seq, 0);
        for (seq, a) in [(1, boy(0)), (2, boy(1))] {
            c.look(seq, &a).unwrap();
            let f = until("the look's frame", || c.latest().filter(|f| f.seq == seq));
            assert_eq!(f.rgba, pixels_of(&a), "seq {seq}");
        }
        c.face(Facing::Left).unwrap();
        assert_eq!(until("a frame after the turn", || c.latest()).seq, 2);
        assert_eq!(c.exited(), None);
        drop(c);
        assert_eq!(fake_log_lines(&data), ["start preview 0", "end preview"], "quit on drop");
    }

    #[test]
    fn drop_kills_the_child() {
        let (data, game, exe) = setup("drop");
        put_state(&data, 0, b"deaf");
        let c = PreviewChild::start(&exe, &game, &data, 0).unwrap();
        let pid = c.p.child.id();
        until("the child started", || fake_log_lines(&data).first().cloned());
        let t = Instant::now();
        drop(c);
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert!(!running(pid), "pid {pid} still runs");
    }

    #[test]
    fn a_dead_child_is_reported() {
        let (data, game, exe) = setup("dead");
        // dies before a frame, saying why
        put_state(&data, 0, b"die");
        let mut c = PreviewChild::start(&exe, &game, &data, 0).unwrap();
        let why = until("the child's end", || c.exited());
        assert_eq!(why, "the save state was made with another xgame.dll; make it again");
        assert_eq!(c.latest(), None);
        // never a frame (the player not found): stopped and reported once the first frame is late
        put_state(&data, 1, b"mute");
        let mut c = PreviewChild::start(&exe, &game, &data, 1).unwrap();
        c.first_frame_by = Duration::from_millis(300);
        let pid = c.p.child.id();
        let why = until("the late first frame", || c.exited());
        assert!(why.contains("no picture"), "{why}");
        assert!(!running(pid), "stopped");
        // a child that is gone refuses looks with a reason, never a panic or a hang
        assert!(c.look(1, &boy(0)).is_err());
    }

    #[test]
    fn a_stale_state_is_remade_once() {
        let (data, game, exe) = setup("stale");
        // made with another xgame.dll: made again first, then the child runs on it
        put_state(&data, 0, b"ok");
        std::fs::write(data.join("build/x-usa-v0.2.0/xgame.dll"), "a rebuilt dll").unwrap();
        let mut s = Session::start(&game, &data, 0);
        assert_eq!(s.gender(), 0);
        let mut seen = 0.0f32;
        until("the child after the remake", || {
            seen = seen.max(s.making().unwrap_or(0.0));
            s.pump(&exe, &game, &data).unwrap().and_then(|c| c.latest())
        });
        assert!(seen > 0.0, "progress was shown");
        assert!(redex_builder::state_matches(&state_path(&data, "x-usa-v0.2.0", 0), &data.join("build/x-usa-v0.2.0/xgame.dll")));
        assert_eq!(fake_log_lines(&data), ["start maker boy", "end maker", "start preview 0"]);
        drop(s);

        // the header matches but the child still stops before its first picture: made again once, then the reason
        let (data, game, exe) = setup("refused");
        put_state(&data, 1, b"die");
        std::fs::write(data.join("fake-maker-body"), "die").unwrap();
        let mut s = Session::start(&game, &data, 1);
        let why = until("the error", || s.pump(&exe, &game, &data).err());
        assert_eq!(why, "the save state was made with another xgame.dll; make it again");
        for _ in 0..20 {
            assert!(s.pump(&exe, &game, &data).is_err());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fake_log_lines(&data), ["start preview 1", "end preview", "start maker girl", "end maker", "start preview 1", "end preview"]);
    }

    // ---- the cards ----

    fn character(id: u64, a: Appearance) -> Character {
        Character { id, name: format!("c{id}"), look: a.to_bytes().to_vec(), created: 1, changed: 1 }
    }

    fn girl() -> Appearance {
        xr_net::x::look::Look::default_for(1).appearance().unwrap()
    }

    fn read_png(p: &Path) -> Vec<u8> {
        let mut r = png::Decoder::new(BufReader::new(std::fs::File::open(p).unwrap())).read_info().unwrap();
        let mut px = vec![0; r.output_buffer_size().unwrap()];
        r.next_frame(&mut px).unwrap();
        px
    }

    #[test]
    fn stale_cards_are_those_without_a_picture_of_their_look() {
        let (data, ..) = setup("stale-cards");
        let (mut a, b) = (character(0xA, boy(0)), character(0xB, girl()));
        let p = card_path(&data, &a);
        let hex: String = a.look.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(p, data.join("characters/000000000000000a").join(format!("studio-{hex}.png")));
        let f = Frame { seq: 1, width: 2, height: 1, rgba: vec![1, 2, 3, 255, 4, 5, 6, 255], floor: None };
        save_card(&p, &f).unwrap();
        assert_eq!(read_png(&p), f.rgba);
        assert_eq!(stale_cards(&data, &[a.clone(), b.clone()]), [b.clone()]);
        // a new look has no picture yet
        a.look = boy(1).to_bytes().to_vec();
        assert_eq!(stale_cards(&data, &[a.clone(), b.clone()]), [a, b]);
    }

    #[test]
    fn cards_are_drawn_one_child_at_a_time() {
        let (data, game, exe) = setup("cards");
        // the boy's state is there, the girl's is made on the way
        put_state(&data, 0, b"ok");
        let chars = [character(1, boy(0)), character(2, girl()), character(3, boy(1))];
        // an older look's picture goes once the new one is there
        let old = data.join("characters/0000000000000001/card-00.png");
        save_card(&old, &Frame { seq: 0, width: 1, height: 1, rgba: vec![0; 4], floor: None }).unwrap();
        let mut cards = Cards::new(chars.to_vec());
        let mut done = Vec::new();
        until("every card", || {
            done.extend(cards.pump(&exe, &game, &data));
            (done.len() == 3).then_some(())
        });
        done.sort();
        assert_eq!(done, [1, 2, 3]);
        for c in &chars {
            assert_eq!(read_png(&card_path(&data, c)), pixels_of(&Appearance::from_bytes(&c.look).unwrap()), "{:x}", c.id);
        }
        assert!(!old.exists());
        assert_eq!(cards.pump(&exe, &game, &data), None);
        // one child at a time, the boys' on one child
        let log = fake_log_lines(&data);
        assert_eq!(log, ["start preview 0", "end preview", "start maker girl", "end maker", "start preview 1", "end preview"]);
    }

    #[test]
    fn the_preview_runs_on_x_when_ready_else_y() {
        let (data, ..) = setup("game");
        let mut lib = Library::default();
        for id in ["x-usa-v0.2.0", "y-usa-v0.2.0"] {
            lib.games.push(GameEntry { build_id: id.into(), dump: data.join(id) });
        }
        // X has a DLL but no verify marker: not Ready
        assert_eq!(preview_game(&lib, &data), None);
        let verify = |id: &str| {
            std::fs::create_dir_all(data.join("build").join(id)).unwrap();
            std::fs::write(data.join("build").join(id).join("xgame.dll"), "").unwrap();
            std::fs::write(data.join("build").join(id).join("verified.txt"), id).unwrap();
        };
        verify("y-usa-v0.2.0");
        assert_eq!(preview_game(&lib, &data).unwrap().build_id, "y-usa-v0.2.0");
        verify("x-usa-v0.2.0");
        assert_eq!(preview_game(&lib, &data).unwrap().build_id, "x-usa-v0.2.0");
        assert_eq!(state_path(&data, "x-usa-v0.2.0", 1), data.join("preview/x-usa-v0.2.0/girl.state"));
    }

    #[test]
    fn warm_makes_both_states_one_at_a_time() {
        let (data, game, exe) = setup("warm");
        let mut w = Warm::start(&game, &data);
        until("both states", || (!w.pump(&exe, &game, &data)).then_some(()));
        let dll = data.join("build/x-usa-v0.2.0/xgame.dll");
        assert!((0..2).all(|g| redex_builder::state_matches(&state_path(&data, "x-usa-v0.2.0", g), &dll)));
        assert_eq!(fake_log_lines(&data), ["start maker boy", "end maker", "start maker girl", "end maker"]);
        // both there: nothing is made
        let mut w = Warm::start(&game, &data);
        until("the check", || (!w.pump(&exe, &game, &data)).then_some(()));
        assert_eq!(fake_log_lines(&data).len(), 4);
    }
}
