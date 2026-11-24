//! The emulation's own thread: it boots the game and keeps it (System isn't Send), runs
//! its refreshes on the console's schedule, plays its sound and hands the window each refresh's pictures. The window
//! keeps winit, egui and the presenter, so it presents at the monitor's rate however long a refresh takes, and a menu of
//! the bar held open (Windows' own message loop) no longer stops the game.
//! The two meet only here: the controls and the newest pictures behind mutexes taken between refreshes, a channel of
//! requests to the game and one of events back.

use std::collections::VecDeque;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use xr_hle::services::hid::InputState;
use xr_hle::services::keyboard;
use xr_hle::{Cadence, Config, FrameOutcome};
use zakuro_common::Screen;
use zakuro_gpu::GpuScreen;

use crate::settings::MicrophoneSource;
use crate::{audio, frame_time, microphone, plays_sound, Boot, Game, CATCH_UP_LIMIT, FRAME_TIME};

/// what the window asks of the next refresh, written by it every step
#[derive(Clone, Copy, Default)]
pub(crate) struct Controls {
    pub input: InputState,
    /// not paused nor under Configure (a game waiting on its keyboard stops too, which the game itself knows)
    pub playing: bool,
    /// the speed limit in percent, none for unlocked or fast forward
    pub limit: Option<u32>,
    /// the right eye's picture is wanted, the 3D shows
    pub stereo: bool,
    /// the window is closing: the thread stops once the refresh it's in is done
    pub quit: bool,
}

/// the settings the running game follows as they change
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Live {
    /// Smooth motion: the monitor rate its pictures in between are made for (60 or the monitor's, up to 240), 0 off
    pub smooth_hz: u32,
    pub slider_3d: f32,
    pub microphone: MicrophoneSource,
    /// 0 to 1, 0 muted
    pub volume: f32,
}

pub(crate) enum Request {
    Live(Live),
    /// the text typed into the game's keyboard and the button that took it
    Keyboard(String, usize),
    /// save the screens as they are, into this folder
    Screenshot(PathBuf),
    /// Smooth motion's pace guard (the window's, crate::PaceGuard): K at most
    Pace(usize),
    /// what the player asked of multiplayer
    Online(crate::online::Request),
}

pub(crate) enum Event {
    /// the game ended: Ok when it exited or ran its --frames, else why it stopped
    Ended(Result<(), String>),
    /// where a screenshot went, or why it didn't
    Screenshot(Result<PathBuf, String>),
}

/// a screen's picture as the presenter takes it: on the GPU when it shares the renderer's device, else its pixels
pub(crate) type Picture = (xr_hle::Screen, Option<GpuScreen>);

/// a picture in between (Smooth motion), t of the way from the screens' picture b4 to theirs, on the GPU: the top
/// screen's, and the bottom's and the right eye's where they have one (else they show their own)
#[derive(Clone, Copy)]
pub(crate) struct Between {
    pub t: f32,
    pub top: GpuScreen,
    pub bottom: Option<GpuScreen>,
    pub right: Option<GpuScreen>,
}

/// a refresh's pictures, and what the window shows over them
#[derive(Clone)]
pub(crate) struct Pictures {
    /// refreshes run so far, a new number w/ each new set
    pub frame: u64,
    pub top: Picture,
    pub bottom: Picture,
    /// the top screen's right eye, while the 3D shows and the game draws one
    pub right: Option<Picture>,
    /// the pictures in between the top screen's picture and the one b4, oldest first, while it's the screens'; when
    /// the first set w/ them was made, and how long the game took to the picture (its refreshes at the speed it ran):
    /// the window shows each as its time comes (crate::pick); `since` tells one picture's pictures in between from the next
    pub between: Vec<Between>,
    pub since: Instant,
    pub span: Duration,
    /// the time in refreshes so far (guest code, GPU recording)
    pub game: Duration,
    /// what the game's keyboard asks for, while the game waits on it
    pub keyboard: Option<keyboard::Request>,
    /// the keyboard answers the game took so far: a set made b4 the window's last answer got there still asks
    pub answers: u64,
    /// XR_FRAMESTATS' counts, and the GPU's time recording command lists
    pub cadence: Cadence,
    pub busy: Duration,
}

/// what the two threads share
#[derive(Default)]
pub(crate) struct Link {
    pub controls: Mutex<Controls>,
    /// the sets the window hasn't shown yet, oldest first, two at most, else the one it shows
    pub pictures: Mutex<VecDeque<Pictures>>,
    /// rung w/ each set put in pictures
    pub published: Condvar,
    /// multiplayer as of the last refresh, w/ the whispers, friends' frames, notes and typed text the window hasn't taken yet
    pub online: Mutex<Option<crate::online::View>>,
}

/// multiplayer as it is now for the window: the view, its whispers, notes and typed text taken (each shows once)
pub(crate) fn take_online(link: &Link) -> Option<crate::online::View> {
    let mut slot = lock(&link.online);
    let view = slot.as_mut()?;
    let now = view.clone();
    (view.whispers, view.notes, view.typed, view.friend_msgs, view.friend_failed) = (Vec::new(), Vec::new(), None, Vec::new(), Vec::new());
    Some(now)
}

/// a refresh's view into the slot: the one b4's whispers, notes and typed text the window didn't take go first
fn put_online(slot: &mut Option<crate::online::View>, mut now: crate::online::View) {
    if let Some(old) = slot.take() {
        now.whispers.splice(0..0, old.whispers);
        now.notes.splice(0..0, old.notes);
        now.friend_msgs.splice(0..0, old.friend_msgs);
        now.friend_failed.splice(0..0, old.friend_failed);
        now.typed = old.typed.or(now.typed);
    }
    *slot = Some(now);
}

/// a mutex's value, a thread that panicked holding it left it whole (each is set in one go)
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// what the thread starts w/
pub(crate) struct Start<G> {
    pub boot: Boot<G>,
    pub config: Config,
    /// stop after this many refreshes (--frames)
    pub frames: Option<u64>,
    pub mute: bool,
    pub live: Live,
    /// set while the blowing key is held, the mic hears it
    pub blowing: Arc<AtomicBool>,
    /// XR_FRAMESTATS is on, the game counts its cadence
    pub stats: bool,
}

/// the window's handle on the emulation thread
pub(crate) struct Emulation {
    pub link: Arc<Link>,
    pub requests: Sender<Request>,
    pub events: Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Emulation {
    pub fn start<G: Game + 'static>(start: Start<G>) -> Result<Emulation, String> {
        let link = Arc::new(Link::default());
        let (requests, asked) = std::sync::mpsc::channel();
        let (told, events) = std::sync::mpsc::channel();
        let shared = link.clone();
        // System is large and debug builds copy it on the stack while booting, like the runtime's main loop
        let thread = std::thread::Builder::new()
            .name("emulation".into())
            .stack_size(64 << 20)
            .spawn(move || run(start, &shared, &asked, &told))
            .map_err(|error| format!("start the emulation thread: {error}"))?;
        Ok(Emulation { link, requests, events, thread: Some(thread) })
    }

    /// stops the thread after the refresh it's in, and waits for it; why, when it panicked
    pub fn stop(&mut self) -> Result<(), String> {
        lock(&self.link.controls).quit = true;
        match self.thread.take().map(std::thread::JoinHandle::join) {
            Some(Err(_)) => Err("the emulation thread panicked".into()),
            _ => Ok(()),
        }
    }
}

/// the thread: boots the game, then runs it until it ends or the window closes
fn run<G: Game>(start: Start<G>, link: &Link, requests: &Receiver<Request>, events: &Sender<Event>) {
    let mut game = match (start.boot)(start.config) {
        Ok(game) => game,
        Err(error) => {
            let _ = events.send(Event::Ended(Err(error)));
            return;
        }
    };
    let system = game.system();
    if system.config.hardware_renderer && !system.gpu.draws_on_gpu() {
        log::warn!("the 3D can't be drawn on the GPU (Vulkan isn't working), it's drawn in software");
    }
    system.cadence.on = start.stats;
    let audio = if start.mute {
        None
    } else {
        audio::Audio::open(xr_hle::AUDIO_SAMPLE_RATE).inspect_err(|error| log::warn!("no sound, {error}")).ok()
    };
    let mut runner = Runner {
        game,
        audio,
        blowing: start.blowing,
        live: None,
        frame: 0,
        game_time: Duration::ZERO,
        answers: 0,
        events: events.clone(),
        motion: None,
        steps_cap: MOST_STEPS,
        pace_cap: MOST_STEPS,
        rate: 0,
        load: (Duration::ZERO, 0),
    };
    runner.apply(start.live);
    let ended = runner.play(link, requests, start.frames);
    let _ = events.send(Event::Ended(ended));
}

struct Runner<G> {
    game: G,
    audio: Option<audio::Audio>,
    blowing: Arc<AtomicBool>,
    /// the settings followed so far
    live: Option<Live>,
    /// refreshes run, and the time they took
    frame: u64,
    game_time: Duration,
    /// keyboard answers taken
    answers: u64,
    events: Sender<Event>,
    /// the picture w/ pictures in between last published, and when its first set was
    motion: Option<(u64, Instant)>,
    /// Smooth motion's load guard: the most steps (K at 30 fps) the game keeps its pace with (see guard_steps), and
    /// the refreshes' time and count since it last looked
    steps_cap: usize,
    load: (Duration, u32),
    /// the window's pace guard's K at most, and the rate the game was last asked for (see follow_rate)
    pace_cap: usize,
    rate: u32,
}

/// K at most (seven pictures in between at 30 fps)
const MOST_STEPS: usize = 8;

/// Smooth motion's load guard: each picture in between costs the game its draws again (at 8x on this PC about 2 ms
/// a refresh), so where the refreshes take past 90% of a console frame on average (the game would fall behind its
/// pace and its sound run dry) K goes one down, to 2; under 60% one up again, to 8
fn guard_steps(steps: usize, average: Duration, frame: Duration) -> usize {
    if average * 10 > frame * 9 {
        steps.saturating_sub(1).max(2)
    } else if average * 10 < frame * 6 {
        (steps + 1).min(MOST_STEPS)
    } else {
        steps
    }
}

/// the monitor rate the pictures in between are made for: the setting's (hz), w/ at most `steps` at 30 fps
fn motion_rate(hz: u32, steps: usize) -> u32 {
    // steps_for(K x 29.9 Hz, 2) is K
    hz.min((steps as f64 * 29.9) as u32)
}

impl<G: Game> Runner<G> {
    /// the refreshes, on the console's schedule (or the speed limit's), until the game ends (how) or the window closes (Ok)
    fn play(&mut self, link: &Link, requests: &Receiver<Request>, frames: Option<u64>) -> Result<(), String> {
        // when the next refresh is due: refreshes run to a schedule instead of one after another, so a slow one gets
        // made up by the ones after it and the sound (made as the console runs) keeps up
        let mut next_frame = Instant::now();
        // faster than the console (fast forward, unlocked or a limit over 100%), the sound left out
        let mut quiet = false;
        let mut looked = Instant::now();
        loop {
            for request in requests.try_iter() {
                self.request(request);
            }
            let controls = *lock(&link.controls);
            if controls.quit {
                return Ok(());
            }
            let now = Instant::now();
            if now > next_frame + CATCH_UP_LIMIT {
                next_frame = now;
            }
            let playing = controls.playing && self.game.system().keyboard_request().is_none();
            let limit = controls.limit;
            let fast = playing && !plays_sound(limit);
            if fast {
                // the output waits out the fast running instead of running dry
                self.audio.as_ref().inspect(|audio| audio.hold());
            }
            if fast != quiet {
                quiet = fast;
                if !quiet {
                    // back at the console's pace, w/ no sound piled up
                    self.audio.as_ref().inspect(|audio| audio.clear());
                    next_frame = Instant::now();
                }
            }
            if playing {
                let began = Instant::now();
                // the window's keys are stale while a menu of its bar is open (see menubar::modal)
                self.game.system().set_input(if crate::menubar::modal() { InputState::default() } else { controls.input });
                let outcome = self.game.run_frame();
                self.frame += 1;
                self.game_time += began.elapsed();
                self.load = (self.load.0 + began.elapsed(), self.load.1 + 1);
                if self.load.1 >= 60 {
                    self.guard(limit);
                }
                let sound = self.game.system().take_audio();
                // faster than the console plays no sound, there's too much of it too soon
                if let (Some(audio), false) = (&self.audio, quiet) {
                    audio.push(&sound);
                }
                match outcome {
                    FrameOutcome::Completed => {}
                    FrameOutcome::Faulted => return Err(format!("the game stopped on a fault. {}", self.game.system().fatal_errors.join("; "))),
                    _ => {
                        log::info!("the game exited");
                        return Ok(());
                    }
                }
                if frames.is_some_and(|frames| self.frame >= frames) {
                    log::info!("ran {} frames (--frames)", self.frame);
                    log::info!("state hash {}", crate::state_hash(self.game.system()));
                    return Ok(());
                }
                next_frame += frame_time(limit);
                if limit.is_none() {
                    // nothing to wait for, but the core goes to the window's and the sound's threads between refreshes
                    next_frame = Instant::now();
                    std::thread::yield_now();
                }
            } else {
                // the sound stops w/ the game, which isn't falling behind
                self.audio.as_ref().inspect(|audio| audio.hold());
                next_frame += FRAME_TIME;
            }
            self.publish(link, controls.stereo, if playing { frame_time(limit) } else { FRAME_TIME });
            // multiplayer goes on while paused, like the window's frames did
            if let Some(view) = self.game.online() {
                put_online(&mut lock(&link.online), view);
            }
            if looked.elapsed() >= Duration::from_millis(500) {
                looked = Instant::now();
                let underruns = self.audio.as_ref().map_or(0, audio::Audio::take_underruns);
                if underruns > 0 && playing {
                    log::warn!("the sound ran dry {underruns} times, the emulation is falling behind");
                }
            }
            let now = Instant::now();
            if now < next_frame {
                std::thread::sleep(next_frame - now);
            }
        }
    }

    /// hands the window the screens as they are now, w/ what goes over them
    fn publish(&mut self, link: &Link, stereo: bool, interval: Duration) {
        let system = self.game.system();
        let (top, bottom) = (crate::shown(system, Screen::Top), crate::shown(system, Screen::Bottom));
        let right = stereo.then(|| crate::shown_right(system)).flatten();
        let motion = system.motion();
        let mut between = Vec::new();
        for k in 0..motion.map_or(0, |motion| motion.steps - 1) {
            let Some(top) = system.gpu_between(Screen::Top, false, k) else { continue };
            let t = (k + 1) as f32 / motion.map_or(1, |motion| motion.steps) as f32;
            let right = if stereo { system.gpu_between(Screen::Top, true, k) } else { None };
            between.push(Between { t, top, bottom: system.gpu_between(Screen::Bottom, false, k), right });
        }
        let now = Instant::now();
        let since = match (motion, self.motion) {
            (Some(motion), Some((picture, since))) if picture == motion.picture => since,
            _ => now,
        };
        self.motion = motion.map(|motion| (motion.picture, since));
        let pictures = Pictures {
            frame: self.frame,
            top,
            bottom,
            right,
            between,
            since,
            span: interval * motion.map_or(1, |motion| motion.gap as u32),
            game: self.game_time,
            keyboard: system.keyboard_request().cloned(),
            answers: self.answers,
            cadence: system.cadence.clone(),
            busy: system.gpu.busy,
        };
        queue_up(&mut lock(&link.pictures), pictures, |pictures| pictures.frame);
        link.published.notify_all();
    }

    fn request(&mut self, request: Request) {
        match request {
            Request::Live(live) => self.apply(live),
            Request::Pace(cap) => {
                let late = cap < self.pace_cap;
                self.pace_cap = cap;
                self.follow_rate(if late { "presents came late" } else { "presents are on time" });
            }
            Request::Keyboard(text, button) => {
                self.game.system().answer_keyboard(&text, button);
                self.answers += 1;
            }
            Request::Online(request) => self.game.online_request(request),
            Request::Screenshot(folder) => {
                let saved = self.screenshot(&folder);
                let _ = self.events.send(Event::Screenshot(saved));
            }
        }
    }

    /// the load guard, once a second of refreshes at the console's pace or slower: fewer pictures in between while the
    /// refreshes take too long, more again once they're quick (see guard_steps)
    fn guard(&mut self, limit: Option<u32>) {
        let (time, count) = std::mem::take(&mut self.load);
        let hz = self.live.map_or(0, |live| live.smooth_hz);
        if hz <= 60 || !plays_sound(limit) {
            return;
        }
        self.steps_cap = guard_steps(self.steps_cap, time / count.max(1), frame_time(limit)).min(xr_hle::steps_for(hz, 2));
        self.follow_rate(&format!("the refreshes take {:.1} ms on average", (time / count.max(1)).as_secs_f64() * 1000.0));
    }

    /// asks the game for pictures in between at the rate the setting allows and both guards (the load's, the window's
    /// pace) leave, where that changed
    fn follow_rate(&mut self, why: &str) {
        let hz = self.live.map_or(0, |live| live.smooth_hz);
        let rate = motion_rate(hz, self.steps_cap.min(self.pace_cap));
        if hz == 0 || rate == self.rate {
            return;
        }
        log::info!("frame interpolation for {hz} Hz: {} pictures in between at 30 fps from now on ({why})", xr_hle::steps_for(rate, 2) - 1);
        self.rate = rate;
        self.game.system().set_motion_rate(rate);
    }

    /// makes the running game and its sound follow the settings, where they changed
    fn apply(&mut self, live: Live) {
        let before = self.live.replace(live);
        let system = self.game.system();
        if system.gpu.interpolates() != (live.smooth_hz > 0) {
            let on = system.set_interpolation(live.smooth_hz > 0);
            log::info!("frame interpolation {}", if on { "on" } else { "off" });
        }
        if before.map(|before| before.smooth_hz) != Some(live.smooth_hz) && live.smooth_hz > 0 {
            self.rate = 0;
            self.follow_rate("the setting");
        }
        let system = self.game.system();
        // games read the 3D slider every frame
        if system.config.slider_3d != live.slider_3d {
            system.set_slider_3d(live.slider_3d);
        }
        if before.map(|before| before.microphone) != Some(live.microphone) {
            let mut microphone = microphone::HostMicrophone::new(live.microphone == MicrophoneSource::Computer, self.blowing.clone());
            // a game already listening goes on listening to the new one
            if system.services.mic.is_sampling() {
                xr_hle::microphone::Microphone::sampling(&mut microphone, true);
            }
            system.microphone = Some(Box::new(microphone));
        }
        if let Some(audio) = &self.audio {
            audio.set_volume(live.volume);
        }
    }

    /// saves the screens as the game drew them, the top one over the bottom one, into folder
    // reads the screens between refreshes, and only on the player's keypress or menu click, never in scripted runs
    fn screenshot(&mut self, folder: &std::path::Path) -> Result<PathBuf, String> {
        let system = self.game.system();
        let top = system.read_screen_scaled(Screen::Top);
        let bottom = system.read_screen_scaled(Screen::Bottom);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_millis());
        let path = folder.join(format!("redex-{stamp}.png"));
        let (width, height, pixels) = crate::stack(&top, &bottom);
        let written = std::fs::create_dir_all(folder).and_then(|()| {
            let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
            let mut encoder = png::Encoder::new(file, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.write_header().and_then(|mut writer| writer.write_image_data(&pixels)).map_err(std::io::Error::other)
        });
        written.map(|()| path).map_err(|error| error.to_string())
    }
}

/// puts a refresh's set after the others; a set made while the game stood still (same frame) takes the place of the one
/// b4 it. Two wait at most: two refreshes done within one vblank (a quick one catching up after a slow one) both get
/// shown, a vblank apart, instead of the first never, which was Smooth motion's in between picture
fn queue_up<T>(queue: &mut VecDeque<T>, set: T, frame: impl Fn(&T) -> u64) {
    if queue.back().is_some_and(|last| frame(last) == frame(&set)) {
        queue.pop_back();
    }
    queue.push_back(set);
    if queue.len() > 2 {
        queue.pop_front();
    }
}

/// drops the sets already shown (frame `shown`) from the front while a later one waits; does the front one want showing
pub(crate) fn next_of<T>(queue: &mut VecDeque<T>, shown: u64, frame: impl Fn(&T) -> u64) -> bool {
    while queue.len() > 1 && queue.front().is_some_and(|first| frame(first) == shown) {
        queue.pop_front();
    }
    queue.front().is_some_and(|first| frame(first) != shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// multiplayer's notes reach the window once each, in order, however many refreshes come between its frames; the
    /// rest of the view is the newest
    #[test]
    fn online_notes_show_once_each() {
        use crate::online::View;
        let link = Link::default();
        assert!(take_online(&link).is_none());
        let view = |note: &str, peers_online: bool| View { notes: vec![note.into()], online: peers_online, ..View::default() };
        put_online(&mut lock(&link.online), view("a", false));
        put_online(&mut lock(&link.online), view("b", true));
        let seen = take_online(&link).unwrap();
        assert_eq!((seen.notes, seen.online), (vec!["a".to_string(), "b".to_string()], true));
        let again = take_online(&link).unwrap();
        assert!(again.notes.is_empty() && again.online);
    }

    /// Smooth motion's load guard lowers K while the refreshes take past 90% of a console frame, and raises it again
    /// under 60%, between 2 and 8; the rate it asks of the game gives that K
    #[test]
    fn the_load_guard_keeps_the_games_pace() {
        let frame = crate::FRAME_TIME;
        let ms = Duration::from_millis;
        assert_eq!(guard_steps(8, ms(19), frame), 7, "falling behind");
        assert_eq!(guard_steps(2, ms(19), frame), 2, "never under one picture in between");
        assert_eq!(guard_steps(5, ms(12), frame), 5, "in between: stays");
        assert_eq!(guard_steps(5, ms(9), frame), 6, "quick again");
        assert_eq!(guard_steps(8, ms(5), frame), 8);
        for steps in 2..=8 {
            assert_eq!(xr_hle::steps_for(motion_rate(240, steps), 2), steps);
        }
        assert_eq!(motion_rate(200, 8), 200, "never past the setting's");
    }

    /// each refresh's set gets shown in turn, even two made within one vblank; a game standing still adds none
    #[test]
    fn sets_wait_their_turn() {
        let mut queue = VecDeque::new();
        let id = |set: &(u64, u8)| set.0;
        assert!(!next_of(&mut queue, 0, id), "nothing yet");
        queue_up(&mut queue, (1, 0), id);
        queue_up(&mut queue, (2, 0), id);
        assert!(next_of(&mut queue, 0, id));
        assert_eq!(queue.pop_front(), Some((1, 0)), "the older first");
        assert!(next_of(&mut queue, 1, id));
        assert_eq!(queue.front(), Some(&(2, 0)), "then the newer, kept for the repeats");
        assert!(!next_of(&mut queue, 2, id), "shown already: the same again");
        queue_up(&mut queue, (2, 1), id);
        assert_eq!(queue.len(), 1, "a set of the same refresh (keyboard news while paused) takes the old one's place");
        assert_eq!(queue.front(), Some(&(2, 1)));
        queue_up(&mut queue, (3, 0), id);
        assert!(next_of(&mut queue, 2, id) && queue.len() == 1, "the shown one goes once a new one waits");
        for frame in 4..9 {
            queue_up(&mut queue, (frame, 0), id);
        }
        assert_eq!(queue, VecDeque::from([(7, 0), (8, 0)]), "a window held up (a menu open) finds the newest two");
    }
}
