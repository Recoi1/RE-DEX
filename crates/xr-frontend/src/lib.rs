//! RE:DEX's window: winit + a Vulkan (or OpenGL) presenter, a native menu bar like Azahar's, an egui Configure window
//! over the game, the keyboard and controllers as the console's buttons, and the console's sound thru cpal. Ported from
//! Zakuro's frontend (`zakuro/src/lib.rs`, gui.rs and menus.rs, commit fdf67b2), cut down to playing one game: the
//! library, recompile jobs, rooms, Download Play, cheats, cameras and save states stay behind (see NOTICE). The game
//! itself runs thru [`Game`], which the runtime implements over its executor, on a thread of its own (emulation.rs).

mod audio;
mod emulation;
mod framestats;
mod gamepad;
mod gui;
#[allow(dead_code)]
mod input;
mod menubar;
mod menus;
mod microphone;
/// multiplayer over the game: names, the Players list, whispers, reports, the server browser
pub mod online;
mod present;
mod pro_controller;
// the library's, rooms' and cameras' settings are kept in the file but nothing here uses them
#[allow(dead_code)]
mod settings;
/// the start screen and its liquid glass look
pub mod start;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use xr_hle::services::hid::{InputState, PadState};
use xr_hle::{Config, FrameOutcome, System};
use zakuro_common::Screen;
use zakuro_gpu::{layout, GpuScreen, Overlay, PresentError, RendererKind, ScreenImage, ScreenLayout};

use emulation::{lock, Controls, Emulation, Event, Live, Request};
use gui::Gui;
use input::Keyboard;
use menubar::{Command, MenuBar};
use menus::{Action, Menus};
use present::Backend;
use settings::{Orientation, Renderer, SmoothMotion, Theme};
/// the runtime reads the world server settings from it before the window opens
pub use settings::Settings;

/// the window's name, w/ " - Paused" while paused
const TITLE: &str = "RE:DEX \u{2014} Pok\u{e9}mon X";

/// the game the window plays, booted and run a frame at a time on the emulation thread, which holds the executor's
/// baton between frames
pub trait Game {
    fn run_frame(&mut self) -> FrameOutcome;
    /// the console, between frames
    fn system(&mut self) -> &mut System;
    /// multiplayer as it is now (who is near, their names' places, new whispers, Steam's server list); None in a run
    /// without multiplayer
    fn online(&mut self) -> Option<online::View> {
        None
    }
    /// does what the player asked of multiplayer; what the player should hear about it comes in the next View's notes
    fn online_request(&mut self, _request: online::Request) {}
}

/// a tap on Tab shorter than this opens the Players list; held longer, Tab fast forwards
const TAP: Duration = Duration::from_millis(250);

/// what the command line asks of the window over the saved settings
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// draw the 3D on the GPU (Some(true)) or in software (Some(false)), the settings' choice when none
    pub hardware: Option<bool>,
    /// times the console's resolution the GPU draws at
    pub resolution: Option<u32>,
    /// stop after this many frames, for unattended runs
    pub frames: Option<u64>,
    pub mute: bool,
    /// a run the command line drives (--frames, --press or --script), which leaves the player's settings file alone
    pub scripted: bool,
    /// shown in a message box once the window is up (e.g. why online play is off)
    pub notice: Option<String>,
}

/// opens the window, boots the game on the presenter's device thru `boot` once there's one, and plays it until the
/// window closes or the game ends. `config` is the console as the runtime has it; the window adds the renderer and the
/// player's language to it. True when the player chose Restart, for the caller to start the game again once it's done.
pub fn run<G: Game + 'static>(options: Options, config: Config, boot: impl FnOnce(Config) -> Result<G, String> + Send + 'static) -> Result<bool, String> {
    let mut settings = Settings::load();
    // the console's own number, made the first time and kept, it tells this one apart from others over local wireless
    if settings.console_id == 0 {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_nanos()));
        settings.console_id = hasher.finish() | 1;
        settings.save();
    }
    let notice = options.notice.clone();
    let mut app = App {
        keyboard: Keyboard::new(settings.keys.clone()),
        gamepads: gamepad::Gamepads::new(settings.pad.clone()),
        data: config.data_dir.clone(),
        user: config.user_dir.clone(),
        options,
        booted: settings.clone(),
        settings,
        boot: Some(Box::new(boot)),
        config: Some(config),
        emulation: None,
        pictures: None,
        answers: 0,
        ended: false,
        error: None,
        gui: None,
        menus: Menus::default(),
        online: online::Online::default(),
        tab_down: None,
        held: (false, PadState::empty()),
        menubar: None,
        backend: None,
        window: None,
        mouse_down: false,
        tilting_from: None,
        cursor: (0.0, 0.0),
        last_title_update: Instant::now(),
        paused: false,
        fast_forward_key: false,
        fast_forwarding: false,
        speed_told: false,
        stats: framestats::FrameStats::from_env(),
        vblank: vblank_of(None),
        monitor_hz: -1.0,
        repeats: std::env::var_os("XR_NO_REPEAT").is_none(),
        next_present: Instant::now(),
        last_present: Instant::now(),
        seen: (0, Duration::ZERO),
        between: None,
        pace: PaceGuard::new(),
        switch: std::env::var("XR_SMOOTH_SWITCH").ok().and_then(|v| {
            let (seconds, smooth) = v.split_once(':')?;
            let smooth = toml::Value::String(smooth.to_owned()).try_into::<settings::SmoothMotion>().ok()?;
            Some((Instant::now() + Duration::from_secs(seconds.parse().ok()?), smooth))
        }),
        motion: None,
        counted: (Instant::now(), 0, 0),
        rates: (0.0, 0.0),
        shape: None,
        dark: None,
        blowing: std::sync::Arc::default(),
        maximized_before_full_screen: false,
        restart: false,
    };
    app.menus.message = notice;

    // the runtime's main loop is not the process's first thread, Windows lets winit run on another
    #[cfg(windows)]
    let event_loop = {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        EventLoop::builder().with_any_thread(true).build()
    };
    #[cfg(not(windows))]
    let event_loop = EventLoop::new();
    let event_loop = event_loop.map_err(|error| format!("create an event loop: {error}"))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let ran = event_loop.run_app(&mut app).map_err(|error| error.to_string());
    // the game stops b4 the presenter goes (App's drop), and is torn down b4 a restart starts the next process
    if let Some(Err(error)) = app.emulation.as_mut().map(Emulation::stop) {
        app.error.get_or_insert(error);
    }
    ran?;
    match app.error.take() {
        Some(error) => Err(error),
        None => Ok(app.restart),
    }
}

type Boot<G> = Box<dyn FnOnce(Config) -> Result<G, String> + Send>;

/// what sizes the window: the layout, swapped or not, the rotation, the top screen's shape, the scale, whole numbers only
type Shape = (settings::Screens, bool, Orientation, settings::TopShape, u32, bool);

struct App<G: Game> {
    options: Options,
    settings: Settings,
    /// the settings the game started w/, what only applies at the start waits for a restart when they differ
    booted: Settings,
    /// <data>, where the saves, logs and screenshots go
    data: Option<PathBuf>,
    /// the saves folder the game runs on (a --character's own), None for `<data>/user`
    user: Option<PathBuf>,
    /// boots the game (on the emulation thread) once the presenter can lend the renderer its device
    boot: Option<Boot<G>>,
    config: Option<Config>,
    emulation: Option<Emulation>,
    /// the newest pictures the game handed over
    pictures: Option<emulation::Pictures>,
    /// keyboard answers sent to the game
    answers: u64,
    /// the game's thread said it ended
    ended: bool,
    /// why the window closed, when it wasn't the player or the game's end
    error: Option<String>,
    gui: Option<Gui>,
    menus: Menus,
    online: online::Online,
    /// since when Tab is held, while it is the Players key: a tap opens the list, a hold fast forwards
    tab_down: Option<Instant>,
    /// a multiplayer panel had the controls last frame, and the console buttons still held since one closed (each kept
    /// from the game until let go, so the A, B or Select that closed it never lands in the game)
    held: (bool, PadState),
    // the menu bar goes b4 the window it's on
    menubar: Option<MenuBar>,
    // backend must be declared (and so dropped) b4 window, Rust drops struct fields in declaration order,
    // and the GL surface's Drop calls eglDestroySurface,
    // which on Wayland does a protocol round trip against the window's wl_surface
    backend: Option<Backend>,
    window: Option<Window>,
    keyboard: Keyboard,
    gamepads: gamepad::Gamepads,
    /// the left button is down, and where the pointer is, in window pixels
    mouse_down: bool,
    /// where the pointer was when the right button went down, it tilts the console while held
    tilting_from: Option<(f32, f32)>,
    cursor: (f32, f32),
    last_title_update: Instant,
    /// stopped w/ Esc or F1 (or the menu's Pause)
    paused: bool,
    /// Tab is held down, for fast forward
    fast_forward_key: bool,
    /// fast forward is on (Tab or the trigger), the game running as fast as it can
    fast_forwarding: bool,
    /// "Speed is 100% in a world" was said (once a run)
    speed_told: bool,
    /// XR_FRAMESTATS, when on
    stats: Option<framestats::FrameStats>,
    /// the monitor's refresh interval and rate, from the window's monitor (60 Hz until it says)
    vblank: Duration,
    monitor_hz: f64,
    /// XR_NO_REPEAT unset: the picture goes up again between refreshes (with Smooth motion on)
    repeats: bool,
    /// when a presenter that doesn't wait for the display (mailbox, immediate) shows the next picture
    next_present: Instant,
    /// when the last present was
    last_present: Instant,
    /// XR_FRAMESTATS: the refreshes run and their time at the last present
    seen: (u64, Duration),
    /// Smooth motion's pace guard (see PaceGuard)
    pace: PaceGuard,
    /// XR_SMOOTH_SWITCH=SECONDS:off|60|monitor: the Smooth motion setting changes that long after the start, as the
    /// Configure window would change it (a check of how the guards follow it)
    switch: Option<(Instant, SmoothMotion)>,
    /// the picture in between (k, from 0) the last present showed, none the game's own; and the pictures in between
    /// last shown (Pictures::since tells them apart) and when the first of them went up
    between: Option<usize>,
    motion: Option<(Instant, Instant)>,
    /// since when frames are counted for the frame rate, and the frames run and shown since
    counted: (Instant, u64, u64),
    /// frames shown a second and the speed against the console's
    rates: (f32, f32),
    /// what the window was last sized for
    shape: Option<Shape>,
    /// the menus' theme in use, dark or light
    dark: Option<bool>,
    /// set while the blowing key is held, the mic hears it
    blowing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    maximized_before_full_screen: bool,
    /// Restart was chosen, the runtime starts the game again once this process is done
    restart: bool,
}

/// one 3DS frame, about 16.71 ms (59.83 a second)
const FRAME_TIME: Duration = Duration::from_nanos(xr_hle::CYCLES_PER_FRAME * 1_000_000_000 / xr_hle::kernel::thread::CPU_CLOCK_HZ);
/// how far behind the schedule may fall b4 it starts over instead of running fast to catch up, after a pause say
const CATCH_UP_LIMIT: Duration = Duration::from_millis(200);

/// the monitor's refresh interval from what winit reports in millihertz, 60 Hz when it reports none
fn vblank_of(millihertz: Option<u32>) -> Duration {
    let mhz = u64::from(millihertz.filter(|&m| m > 0).unwrap_or(60_000)).clamp(30_000, 1_000_000);
    Duration::from_nanos(1_000_000_000_000 / mhz)
}

/// does the window present now: a new refresh's pictures; the same again at the monitor's rate while the game plays
/// w/ Smooth motion on (and XR_NO_REPEAT unset); and once a console frame after the last present, for the menus over a
/// game standing still. With Smooth motion off a playing game's presents stay at the refreshes' own rate
fn presents_now(fresh: bool, repeating: bool, playing: bool, since_present: Duration) -> bool {
    fresh || repeating || (!playing && since_present >= FRAME_TIME)
}

/// does the picture go up again between refreshes on a monitor w/ this vblank: only when a refresh spans 1.5 vblanks or
/// more (90 Hz and up); at 60 or 75 Hz a repeat has no spare vblank to fill and only queues ahead of the next picture
fn repeats_at(vblank: Duration) -> bool {
    vblank * 3 < FRAME_TIME * 2
}

/// the monitor rate Smooth motion makes its pictures in between for: 0 off, 60 for "60 fps" and wherever the picture
/// doesn't go up again between refreshes (a monitor under 90 Hz, XR_NO_REPEAT), else the monitor's, up to 240
fn smooth_hz(smooth: SmoothMotion, monitor_hz: f64, repeats: bool) -> u32 {
    match smooth {
        SmoothMotion::Off => 0,
        SmoothMotion::Monitor if repeats => (monitor_hz.round().clamp(60.0, 240.0)) as u32,
        SmoothMotion::Sixty | SmoothMotion::Monitor => 60,
    }
}

/// which picture in between goes up now (by its t, oldest first), none for the game's own: the latest whose time came,
/// `elapsed` after the first of them went up, for a game frame lasting `span`. Its time is t of the way along the frame
/// at the middle of the vblank the present shows in, plus 1/K: the first goes up at once, the game's own picture (K-1)/K
/// of a frame late, and each gets 1/K of the frame
fn pick(ts: &[f32], elapsed: Duration, vblank: Duration, span: Duration) -> Option<usize> {
    let first = *ts.first()?;
    if span.is_zero() {
        return None;
    }
    let t = (elapsed + vblank / 2).as_secs_f32() / span.as_secs_f32() + first;
    if t >= 1.0 {
        return None;
    }
    ts.iter().rposition(|&at| at <= t)
}

/// Smooth motion's pace guard: the GPU draws the pictures in between too, and where that's more than it can do and
/// still have each present on time (8x: a frame's full-screen draws again for each picture in between), presents come
/// a vblank late. Each second it counts the presents while pictures repeat and those later than 1.5 vblanks: more than
/// 2% late in the second, or more than 0.5% over the last 3 seconds, takes K a step down from what the game shows now
/// (to 2) and doubles how long it waits b4 trying a step up again (4 s at first, 16 at most: longer kept light scenes
/// at a heavy one's K for a minute); every other second counts
/// toward that wait. The 3 seconds let a second w/ a couple of late presents in a field running at 0.3% pass (the p99
/// line is 1%); the 2% catches a K the GPU can't keep up w/ at once. The window owns the cap and sends it whole
/// (Request::Pace); a new Smooth motion setting starts it over at 8
struct PaceGuard {
    /// the Smooth motion setting it learned for
    setting: SmoothMotion,
    cap: usize,
    presents: u32,
    late: u32,
    /// the last 3 seconds' presents and late ones, since K last came down
    window: std::collections::VecDeque<(u32, u32)>,
    since: Instant,
    calm: u32,
    wait: u32,
}

impl PaceGuard {
    fn new() -> PaceGuard {
        PaceGuard { setting: SmoothMotion::Off, cap: 8, presents: 0, late: 0, window: Default::default(), since: Instant::now(), calm: 0, wait: 4 }
    }

    /// a present `interval` after the one b4 while pictures repeat, w/ K what the game shows now; once a second, the new
    /// K at most when the second's counts change it
    fn present(&mut self, interval: Duration, vblank: Duration, now: usize) -> Option<usize> {
        // a pause or a held window is no late present
        if interval < Duration::from_millis(100) {
            self.presents += 1;
            self.late += (interval * 2 > vblank * 3) as u32;
        }
        if self.since.elapsed() < Duration::from_secs(1) {
            return None;
        }
        self.since = Instant::now();
        let second = (std::mem::take(&mut self.presents), std::mem::take(&mut self.late));
        self.window.push_back(second);
        if self.window.len() > 3 {
            self.window.pop_front();
        }
        let window = self.window.iter().fold((0, 0), |sum, &(presents, late)| (sum.0 + presents, sum.1 + late));
        let before = self.cap;
        (self.cap, self.calm, self.wait) = pace_step(self.cap.min(now.max(2)), second, window, self.calm, self.wait);
        if self.cap < before {
            // measured again at the new K
            self.window.clear();
        }
        (self.cap != before).then_some(self.cap)
    }
}

/// a second of the pace guard: (K at most, calm seconds, the wait for a step up) from K at most (or what the game shows
/// now, when that's less), the second's presents and late ones and the last 3 seconds'; only a step down starts the
/// calm count over
fn pace_step(cap: usize, second: (u32, u32), window: (u32, u32), calm: u32, wait: u32) -> (usize, u32, u32) {
    if (second.0 >= 50 && second.1 * 50 > second.0) || (window.0 >= 300 && window.1 * 200 > window.0) {
        return (cap.saturating_sub(1).max(2), 0, if cap > 2 { (wait * 2).min(16) } else { wait });
    }
    match calm + 1 >= wait && cap < 8 {
        true => (cap + 1, 0, wait),
        false => (cap, calm + 1, wait),
    }
}

/// does a present that took `took` leave the loop to wait for the next vblank estimate itself: a presenter that doesn't
/// wait for the display, or one that came back much sooner than a vblank (minimized, zero size, out of date, occluded),
/// which would spin
fn waits_itself(waits: bool, took: Duration, vblank: Duration) -> bool {
    !waits || took < vblank / 4
}

/// how long a frame takes at a speed limit in percent of the console's, none for unlocked (no wait at all)
fn frame_time(limit: Option<u32>) -> Duration {
    match limit {
        Some(percent) => FRAME_TIME * 100 / percent.clamp(menus::SPEED_MIN, menus::SPEED_MAX),
        None => Duration::ZERO,
    }
}

/// the speed limit the game runs at (none: unlocked) from fast forward and the Speed setting; in a world the console's
/// speed at most (no fast forward, nothing over 100%; slower is fine). And whether that cut what was asked for
fn world_speed(fast: bool, limit: Option<u32>, in_world: bool) -> (Option<u32>, bool) {
    let asked = if fast { None } else { limit };
    let speed = if in_world { Some(asked.unwrap_or(100).min(100)) } else { asked };
    (speed, speed != asked)
}

/// does the game's sound play at this speed: at the console's speed or slower (the queue stretches it, up to twice as
/// long), not faster, where it'd pile up faster than it plays
fn plays_sound(limit: Option<u32>) -> bool {
    limit.is_some_and(|percent| percent <= 100)
}

impl<G: Game + 'static> ApplicationHandler for App<G> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // hidden till it has its place, a size that may not fit the screen never shows
        let attributes = Window::default_attributes().with_title(TITLE).with_inner_size(self.window_size()).with_window_icon(icon(32)).with_visible(false);
        #[cfg(windows)]
        let attributes = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attributes.with_taskbar_icon(icon(256))
        };
        let renderer = match self.settings.renderer {
            Renderer::Vulkan => RendererKind::Vulkan,
            Renderer::OpenGl => RendererKind::OpenGl,
        };
        let backend = Backend::create(event_loop, attributes.clone(), renderer).or_else(|error| {
            if renderer != RendererKind::Vulkan {
                return Err(error);
            }
            // a machine w/o a working Vulkan driver still gets a window
            log::warn!("could not start the Vulkan backend, {error}, presenting with OpenGL instead");
            Backend::create(event_loop, attributes, RendererKind::OpenGl)
        });
        let (window, backend) = match backend {
            Ok(created) => created,
            Err(error) => return self.fail(event_loop, format!("could not start the {renderer:?} backend: {error}")),
        };
        log::info!("presenting with the {} backend", backend.name());
        // the bar takes its height from the window's area, which gets it back
        match MenuBar::new().map_err(|error| error.to_string()).and_then(|bar| bar.attach(&window).map(|()| bar)) {
            Ok(bar) => self.menubar = Some(bar),
            Err(error) => log::warn!("no menu bar, {error}"),
        }
        self.place(&window);
        window.set_visible(true);
        self.gui = Some(Gui::new(&window));
        self.menus.motion = animations();
        self.online.motion = self.menus.motion;
        self.window = Some(window);
        self.backend = Some(backend);
        self.apply_theme();
        self.apply_presenter();
        self.sync_menu();
        // the game starts on the device the presenter may lend the renderer, once there's one
        let (Some(boot), Some(config)) = (self.boot.take(), self.config.take()) else { return };
        let start = emulation::Start {
            boot,
            config: self.game_config(config),
            frames: self.options.frames,
            mute: self.options.mute,
            live: self.live(),
            blowing: self.blowing.clone(),
            stats: self.stats.is_some(),
        };
        match Emulation::start(start) {
            Ok(emulation) => self.emulation = Some(emulation),
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Tab is fast forward while the game runs, not a key for moving the UI's focus around
        let tab = matches!(&event, WindowEvent::KeyboardInput { event, .. } if event.physical_key == PhysicalKey::Code(KeyCode::Tab));
        let withheld = tab && !self.menus.over_game() && !self.typing();
        let consumed = match (&mut self.gui, &self.window) {
            (Some(gui), Some(window)) if !withheld => gui.event(window, &event),
            _ => false,
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            // keys let go while another window has the keyboard never come
            WindowEvent::Focused(false) => self.release_keys(),
            WindowEvent::Resized(size) => {
                if let Some(backend) = &mut self.backend {
                    backend.resize(size.width, size.height);
                }
                self.remember_place();
            }
            WindowEvent::Moved(_) => self.remember_place(),
            WindowEvent::KeyboardInput { event, .. } => self.key(event_loop, event, consumed),
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    self.mouse_down = state == ElementState::Pressed && !self.pointer_taken(consumed);
                    self.touch();
                }
                if button == MouseButton::Right {
                    let pressed = state == ElementState::Pressed && !self.pointer_taken(consumed);
                    self.tilting_from = pressed.then_some(self.cursor);
                    self.tilt();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                self.touch();
                self.tilt();
            }
            WindowEvent::ThemeChanged(_) => self.apply_theme(),
            // while a menu of the bar is open Windows runs its own message loop, w/o redraws: the picture holds, the game
            // (on its own thread) and its sound go on
            WindowEvent::RedrawRequested => self.step(event_loop),
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // the window's last place and size go in the file, when something changed and the player, not a script, ran it
        if !self.options.scripted && Settings::load() != self.settings {
            self.settings.save();
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

impl<G: Game> App<G> {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
        self.error = Some(error);
        event_loop.exit();
    }

    /// the console as the runtime gave it, w/ the renderer, its device and what the settings ask for
    fn game_config(&self, config: Config) -> Config {
        Config {
            language: self.settings.language.code(),
            hardware_renderer: self.options.hardware.unwrap_or(self.settings.hardware_rasterizer),
            resolution: self.options.resolution.unwrap_or(self.settings.resolution).max(1),
            device: self.backend.as_ref().and_then(Backend::shared_device),
            texture_packs: self.settings.texture_packs,
            interpolate: self.settings.frame_interpolation != SmoothMotion::Off,
            console_id: self.settings.console_id,
            slider_3d: self.settings.slider_3d(),
            ..config
        }
    }

    /// the game waits on its keyboard for text typed in
    fn typing(&self) -> bool {
        self.pictures.as_ref().is_some_and(|pictures| pictures.keyboard.is_some())
    }

    /// asks the game for something, once it runs
    fn ask(&self, request: Request) {
        if let Some(emulation) = &self.emulation {
            let _ = emulation.requests.send(request);
        }
    }

    /// the settings the running game follows as they change
    fn live(&self) -> Live {
        let settings = &self.settings;
        Live {
            smooth_hz: smooth_hz(settings.frame_interpolation, self.monitor_hz, self.repeats && repeats_at(self.vblank)),
            slider_3d: settings.slider_3d(),
            microphone: settings.microphone,
            volume: if settings.mute { 0.0 } else { settings.volume },
        }
    }

    fn key(&mut self, event_loop: &ActiveEventLoop, event: KeyEvent, consumed: bool) {
        let pressed = event.state == ElementState::Pressed;
        let PhysicalKey::Code(code) = event.physical_key else { return };
        // Escape leaves a controller binding waiting for a button as it was
        if pressed && code == KeyCode::Escape && self.menus.rebinding_pad.take().is_some() {
            return;
        }
        // a binding waiting for a key takes it, Escape leaves it as it was, the window's own keys stay theirs
        if let (true, Some(index)) = (pressed, self.menus.rebinding) {
            self.menus.rebinding = None;
            if matches!(code, KeyCode::F1 | KeyCode::F9 | KeyCode::F10 | KeyCode::F11 | KeyCode::F12) {
                self.menus.toast(format!("{} is a window key, pick another", menus::key_name(code)));
            } else if index == menus::PLAYERS {
                if code == KeyCode::KeyM {
                    self.menus.toast("M blows into the microphone, pick another");
                } else if code != KeyCode::Escape {
                    let mut keys = self.settings.keys.clone();
                    let taken = keys.all_mut().into_iter().find(|(_, bound)| **bound == code).map(|(button, _)| button);
                    match taken {
                        Some(button) => self.menus.toast(format!("{} is on the console's {button}, pick another", menus::key_name(code))),
                        None => {
                            self.settings.keys.players = code;
                            self.menus.toast(format!("Players list: {}", menus::key_name(code)));
                            self.apply_settings();
                        }
                    }
                }
            } else if code == KeyCode::Tab {
                // bound to a button it would stop fast forwarding (and M blowing into the mic)
                self.menus.toast("Tab is fast forward, pick another");
            } else if code == KeyCode::KeyM {
                self.menus.toast("M blows into the microphone, pick another");
            } else if code == self.settings.keys.players && code != KeyCode::Escape {
                self.menus.toast(format!("{} opens the Players list, pick another", menus::key_name(code)));
            } else if code != KeyCode::Escape {
                if let Some(moved) = rebind(&mut self.settings.keys, index, code) {
                    let name = |at: usize| self.settings.keys.clone().all_mut()[at].0;
                    self.menus.toast(format!("{} moved from {} to {}", menus::key_name(code), name(moved), name(index)));
                    self.menus.shake(moved);
                }
                self.apply_settings();
            }
            return;
        }
        if code == KeyCode::Tab && !event.repeat && !self.settings.keys.binds(KeyCode::Tab) {
            // fast forward while it's held
            self.fast_forward_key = pressed;
        }
        // the Players list: a tap on Tab (held, Tab fast forwards), or its own key; not while typing into a box
        let typing_box = self.gui.as_ref().is_some_and(Gui::wants_keyboard);
        if code == self.settings.keys.players && !event.repeat && !self.menus.over_game() && !typing_box {
            if code == KeyCode::Tab {
                if pressed {
                    self.tab_down = Some(Instant::now());
                } else if self.tab_down.take().is_some_and(|since| since.elapsed() < TAP) {
                    self.online.toggle_players();
                }
            } else if pressed {
                self.online.toggle_players();
            }
        }
        if code == KeyCode::Tab && !pressed {
            self.tab_down = None;
        }
        if pressed && !event.repeat {
            match code {
                // Escape closes Configure, then a multiplayer panel (egui sees it), otherwise it pauses like Azahar's
                KeyCode::Escape if self.menus.configure_open => self.menus.configure_open = false,
                KeyCode::Escape if self.online.takes_input() => {}
                KeyCode::F1 | KeyCode::Escape => self.command(event_loop, Command::Pause),
                KeyCode::F9 => self.command(event_loop, Command::NextLayout),
                KeyCode::F10 => self.command(event_loop, Command::Swap),
                KeyCode::F11 => self.command(event_loop, Command::Fullscreen),
                KeyCode::F12 => self.command(event_loop, Command::Screenshot),
                _ => {}
            }
        }
        let to_game = !self.paused && !self.menus.over_game() && !self.online.takes_input() && !consumed && !self.gui.as_ref().is_some_and(Gui::wants_keyboard);
        // blowing into the mic while M is held
        if code == KeyCode::KeyM && !event.repeat && !self.settings.keys.binds(KeyCode::KeyM) && (to_game || !pressed) {
            self.blow(pressed);
        }
        // letting go always gets thru, so no button stays held
        if to_game || !pressed {
            self.keyboard.key(event.physical_key, pressed);
        }
    }

    /// does what a menu item or its key asks
    fn command(&mut self, event_loop: &ActiveEventLoop, command: Command) {
        match command {
            Command::OpenSaves => self.open_saves(),
            Command::OpenLogs => self.open_folder("logs"),
            Command::Exit => event_loop.exit(),
            Command::Pause => {
                self.paused = !self.paused;
                self.keyboard.release();
                self.blow(false);
                self.menus.toast(if self.paused { "Paused" } else { "Resumed" });
                log::info!("{}", if self.paused { "paused" } else { "resumed" });
            }
            Command::Restart => self.restart(event_loop),
            Command::Speed(speed) => {
                self.settings.limit_speed = speed.is_some();
                self.settings.speed_limit = speed.unwrap_or(self.settings.speed_limit);
                self.menus.pulse(format!("Speed: {}", speed.map_or("Unlocked".to_owned(), |percent| format!("{percent}%"))));
            }
            Command::Configure => self.menus.configure_open = true,
            Command::Fullscreen => self.toggle_fullscreen(),
            Command::Layout(layout) => self.set_layout(layout),
            Command::NextLayout => self.set_layout(self.settings.layout.next()),
            Command::Swap => {
                self.settings.swap_screens = !self.settings.swap_screens;
                self.menus.toast(format!("Swap screens: {}", if self.settings.swap_screens { "on" } else { "off" }));
            }
            Command::Rotate => {
                self.settings.orientation = self.settings.orientation.next();
                self.menus.toast(format!("Rotation: {}", self.settings.orientation.name()));
            }
            Command::ShowFps => self.settings.show_fps = !self.settings.show_fps,
            Command::Screenshot => self.screenshot(),
            Command::TexturePacks => {
                self.settings.texture_packs = !self.settings.texture_packs;
                self.menus.toast("Texture packs: applies after restart (Emulation > Restart)");
            }
            Command::GitHub => open(menubar::GITHUB),
            Command::Browse => self.online.open_browser(),
            Command::Players => self.online.toggle_players(),
            Command::Whispers => self.online.log_pinned = !self.online.log_pinned,
        }
        self.apply_settings();
    }

    fn set_layout(&mut self, layout: settings::Screens) {
        self.settings.layout = layout;
        self.menus.toast(format!("Layout: {}", layout.name()));
    }

    /// the menus dark or light, as chosen or as Windows is, the menu bar along
    fn apply_theme(&mut self) {
        let (Some(window), Some(gui)) = (&self.window, &self.gui) else { return };
        let chosen = match self.settings.theme {
            Theme::System => None,
            Theme::Dark => Some(true),
            Theme::Light => Some(false),
        };
        let dark = chosen.unwrap_or(window.theme() != Some(winit::window::Theme::Light));
        if self.dark != Some(dark) {
            self.dark = Some(dark);
            menus::style(&gui.ctx, dark, self.menus.motion);
            if let Some(bar) = &self.menubar {
                bar.set_theme(window, chosen);
            }
        }
    }

    /// shows a folder of <data> in the file manager, making it when it isn't there yet
    fn open_folder(&mut self, name: &str) {
        let Some(folder) = self.data.as_ref().map(|data| data.join(name)) else { return };
        let _ = std::fs::create_dir_all(&folder);
        open(&folder.to_string_lossy());
    }

    /// shows the saves the game runs on: the character's own folder with --character, else <data>/user
    fn open_saves(&mut self) {
        let Some(folder) = self.user.clone().or_else(|| self.data.as_ref().map(|d| d.join("user"))) else { return };
        let _ = std::fs::create_dir_all(&folder);
        open(&folder.to_string_lossy());
    }

    /// closes the window w/ the settings saved as they are now, for the runtime to start the game over in a new process
    /// once this one is done (`run` returns true), so the new one's log and saves don't race this one's
    fn restart(&mut self, event_loop: &ActiveEventLoop) {
        self.settings.save();
        log::info!("restarting");
        self.restart = true;
        event_loop.exit();
    }

    /// saves the screens as the game drew them, the top one over the bottom one, to <data>/screenshots (the game's thread
    /// does, between refreshes; Event::Screenshot says how it went)
    fn screenshot(&mut self) {
        if let Some(data) = &self.data {
            self.ask(Request::Screenshot(data.join("screenshots")));
        }
    }

    /// takes in the controllers' buttons, a binding waiting for one takes it, and Home opens and closes Configure; the
    /// Players button opens the list, and while a multiplayer panel is up the d-pad, A and B work it
    fn poll_gamepads(&mut self) {
        for button in self.gamepads.poll() {
            if let Some(index) = self.menus.rebinding_pad.take() {
                let name = gamepad::button_name(button);
                if index == menus::PLAYERS {
                    // every console button (A and B work the list too) and Home (Configure): none can open it as well
                    if self.settings.pad.players_refuses(button) {
                        self.menus.toast(format!("{name} is a console button or Home, pick another for the Players list"));
                    } else {
                        self.settings.pad.players = button;
                        self.menus.toast(format!("Players list: {name}"));
                    }
                } else if button == self.settings.pad.players {
                    self.menus.toast(format!("{name} opens the Players list, pick another"));
                } else if let Some((_, bound)) = self.settings.pad.all_mut().into_iter().nth(index) {
                    *bound = button;
                }
                self.apply_settings();
            } else if button == gilrs::Button::Mode {
                self.menus.configure_open = !self.menus.configure_open;
                self.keyboard.release();
            } else if self.menus.configure_open {
            } else if button == self.settings.pad.players {
                self.online.toggle_players();
            } else if self.online.takes_input() {
                let pad = &self.settings.pad;
                let nav = [(pad.up, online::Nav::Up), (pad.down, online::Nav::Down), (pad.a, online::Nav::Accept), (pad.b, online::Nav::Back)];
                if let Some(&(_, nav)) = nav.iter().find(|(bound, _)| *bound == button) {
                    self.online.nav(nav);
                }
            }
        }
    }

    /// is the pointer busy w/ something other than the game
    fn pointer_taken(&self, consumed: bool) -> bool {
        consumed || self.menus.over_game() || self.gui.as_ref().is_some_and(Gui::wants_pointer)
    }

    /// the presenter's arrangement of the screens
    fn arrangement(&self) -> ScreenLayout {
        self.settings.layout.screens(self.settings.swap_screens)
    }

    /// touches the bottom screen while the button is down over it
    fn touch(&mut self) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let orientation = self.settings.orientation;
        let (_, bottom) = layout(size.width, size.height, self.arrangement(), orientation.screen_orientation(), self.settings.top_shape.width(), self.settings.integer_scale);
        // w/ the top screen alone there's nothing to touch
        let Some(bottom) = bottom else {
            self.keyboard.touch(None);
            return;
        };
        let x = self.cursor.0 - bottom.x;
        let y = self.cursor.1 - bottom.y;
        let inside = x >= 0.0 && y >= 0.0 && x < bottom.width && y < bottom.height;
        self.keyboard.touch((self.mouse_down && inside).then(|| {
            let (u, v) = (x / bottom.width, y / bottom.height);
            let (u, v) = match orientation {
                Orientation::North => (u, v),
                Orientation::East => (v, 1.0 - u),
                Orientation::South => (1.0 - u, 1.0 - v),
                Orientation::West => (1.0 - v, u),
            };
            (((u * 320.0) as u16).min(319), ((v * 240.0) as u16).min(239))
        }));
    }

    /// tilts the console while the right button's held, toward where the pointer went and by how far,
    /// like Citra's motion emulation, letting go stands it upright again
    fn tilt(&mut self) {
        // radians a pixel, and the furthest it goes, a quarter turn
        const PER_PIXEL: f32 = 0.01;
        let tilt = match (self.tilting_from, &self.window) {
            (Some((x, y)), Some(window)) => {
                let scale = window.scale_factor() as f32;
                let (dx, dy) = ((self.cursor.0 - x) / scale, (self.cursor.1 - y) / scale);
                let distance = dx.hypot(dy);
                let angle = (distance * PER_PIXEL).min(std::f32::consts::FRAC_PI_2);
                if distance > 0.0 { [dx / distance * angle, dy / distance * angle] } else { [0.0; 2] }
            }
            _ => [0.0; 2],
        };
        self.keyboard.tilt(tilt);
    }

    fn toggle_fullscreen(&mut self) {
        if let Some(window) = &self.window {
            let full = window.fullscreen().is_some();
            if !full {
                self.maximized_before_full_screen = window.is_maximized();
            }
            // fullscreen is the screens alone, like Azahar's, the menu bar comes back w/ the window
            if let Some(bar) = &self.menubar {
                bar.show(window, full);
            }
            window.set_fullscreen((!full).then_some(Fullscreen::Borderless(None)));
            self.menus.dip();
            if full && !self.maximized_before_full_screen {
                self.fit_window();
            }
        }
    }

    /// the window's first place: where it was last time when that's still on a screen and fits there, otherwise the
    /// size the settings ask for, made smaller (same shape) when it doesn't fit the screen's work area, centered on it
    fn place(&self, window: &Window) {
        let chrome = chrome(window);
        let saved = self.settings.window.map(|[x, y, width, height]| (x, y, width.max(1) as u32, height.max(1) as u32));
        let back = saved.and_then(|(x, y, width, height)| {
            let work = work_area(window, Some((x, y, width + chrome.0, height + chrome.1)))?;
            let fitted = fit((width, height), chrome, work);
            ((fitted.2, fitted.3) == (width, height)).then(|| {
                let (x, y) = inside((x, y, width, height), chrome, work);
                (x, y, width, height)
            })
        });
        let place = back.or_else(|| Some(fit(self.wanted(window), chrome, work_area(window, None)?)));
        if let Some((x, y, width, height)) = place {
            let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(width, height));
            window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
        }
    }

    /// sizes the window to the screens at the chosen scale, made to fit its screen's work area, kept on it
    fn fit_window(&self) {
        let Some(window) = &self.window else { return };
        let chrome = chrome(window);
        let want = self.wanted(window);
        let Some(work) = work_area(window, None) else {
            let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(want.0, want.1));
            return;
        };
        let (_, _, width, height) = fit(want, chrome, work);
        let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(width, height));
        if let Ok(at) = window.outer_position() {
            let (x, y) = inside((at.x, at.y, width, height), chrome, work);
            if (x, y) != (at.x, at.y) {
                window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
        }
    }

    /// the size the settings ask for, in the window's pixels
    fn wanted(&self, window: &Window) -> (u32, u32) {
        let size: winit::dpi::PhysicalSize<u32> = self.window_size().to_physical(window.scale_factor());
        (size.width.max(1), size.height.max(1))
    }

    /// keeps where the window is and its size for next time, while it's a plain window
    fn remember_place(&mut self) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        if window.fullscreen().is_some() || window.is_maximized() || window.is_minimized() == Some(true) || size.width == 0 || !window.is_visible().unwrap_or(true) {
            return;
        }
        if let Ok(at) = window.outer_position() {
            self.settings.window = Some([at.x, at.y, size.width as i32, size.height as i32]);
        }
    }

    /// makes the running game, the presenter, the window and the sound follow the settings, and saves them unless
    /// Configure is open, which saves once when it closes instead of on every tick of a slider being dragged
    fn apply_settings(&mut self) {
        self.settings.speed_limit = self.settings.speed_limit.clamp(menus::SPEED_MIN, menus::SPEED_MAX);
        if self.menus.configure_open {
            self.menus.changed = true;
        } else {
            self.settings.save();
        }
        // interpolation, the 3D slider, the mic and the volume, which the game's thread applies where they changed
        self.ask(Request::Live(self.live()));
        // another Smooth motion setting: the pace guard starts over (it learned the old one's load)
        if self.pace.setting != self.settings.frame_interpolation {
            self.pace = PaceGuard { setting: self.settings.frame_interpolation, ..PaceGuard::new() };
            self.ask(Request::Pace(self.pace.cap));
        }
        self.keyboard.set_keys(self.settings.keys.clone());
        self.gamepads.set_buttons(self.settings.pad.clone());
        self.apply_theme();
        self.apply_presenter();
        self.sync_menu();
    }

    /// gives the presenter the settings' arrangement, look and vsync, and sizes the window to a new arrangement
    fn apply_presenter(&mut self) {
        let arrangement = self.arrangement();
        let settings = &self.settings;
        if let Some(backend) = &mut self.backend {
            backend.set_layout(arrangement);
            backend.set_orientation(settings.orientation.screen_orientation());
            backend.set_top_width(settings.top_shape.width());
            backend.set_scaling(settings.filter.screens(), settings.integer_scale);
            backend.set_stereo(settings.stereo());
            backend.set_vsync(settings.vsync);
        }
        let shape = (settings.layout, settings.swap_screens, settings.orientation, settings.top_shape, settings.scale, settings.integer_scale);
        if self.shape.replace(shape).is_some_and(|before| before != shape) {
            self.menus.dip();
            if self.window.as_ref().is_some_and(|window| window.fullscreen().is_none() && !window.is_maximized()) {
                self.fit_window();
            }
        }
    }

    /// ticks the menu bar's items as things are
    fn sync_menu(&self) {
        if let (Some(bar), Some(window)) = (&self.menubar, &self.window) {
            bar.sync(&self.settings, self.paused, window.fullscreen().is_some());
        }
    }

    /// starts or stops blowing into the mic
    fn blow(&mut self, on: bool) {
        if self.blowing.swap(on, std::sync::atomic::Ordering::Relaxed) != on {
            log::debug!("{} into the microphone", if on { "blowing" } else { "no longer blowing" });
        }
    }

    /// the window size that fits the screens at the chosen scale
    fn window_size(&self) -> winit::dpi::Size {
        let (width, height) = self.arrangement().size(self.settings.top_shape.width());
        let (width, height) = match self.settings.orientation.screen_orientation().is_portrait() {
            true => (height, width),
            false => (width, height),
        };
        let scale = self.settings.scale.max(1);
        // scaled by whole numbers, a desktop scaled by 125% say would leave a border,
        // so the window gets whole multiples of the screens' pixels
        if let (true, Some(window)) = (self.settings.integer_scale, &self.window) {
            let times = (scale as f64 * window.scale_factor()).round().max(1.0) as u32;
            return winit::dpi::PhysicalSize::new(width * times, height * times).into();
        }
        winit::dpi::LogicalSize::new(width * scale, height * scale).into()
    }

    /// every key counts as let go, as when another window takes the keyboard
    fn release_keys(&mut self) {
        self.fast_forward_key = false;
        self.blow(false);
        self.keyboard.release();
    }

    fn step(&mut self, event_loop: &ActiveEventLoop) {
        for command in MenuBar::commands() {
            self.command(event_loop, command);
        }
        if self.emulation.is_none() || !self.take_events(event_loop) {
            return;
        }
        self.poll_gamepads();
        // and so do those let go while a menu of the bar was open
        if menubar::modal_ended() {
            self.release_keys();
        }
        if let Some((_, smooth)) = self.switch.filter(|(at, _)| Instant::now() >= *at) {
            self.switch = None;
            log::info!("XR_SMOOTH_SWITCH: Smooth motion {smooth:?}");
            self.settings.frame_interpolation = smooth;
            self.apply_settings();
        }

        // a game waiting on its keyboard waits for the user, not running, and so does one w/ Configure open
        let playing = !self.paused && !self.menus.over_game() && !self.typing();
        // Tab or a controller's right trigger, or the right stick's click while ZR has the trigger, whichever no console button is on
        let free = |button: gilrs::Button| self.settings.pad.map().iter().all(|&(_, bound)| bound != button);
        let trigger = [gilrs::Button::RightTrigger2, gilrs::Button::RightThumb].into_iter().find(|&button| free(button));
        // Tab as the Players key fast forwards only once held past a tap
        let tab = self.fast_forward_key && (self.settings.keys.players != KeyCode::Tab || self.tab_down.is_some_and(|since| since.elapsed() >= TAP));
        let fast = playing && (tab || trigger.is_some_and(|trigger| self.gamepads.holding(trigger)));
        // in a world (joined, even while reconnecting: a dropped connection is no way round it) no faster than the console
        let in_world = self.online.in_world();
        let (limit, cut) = world_speed(fast, self.settings.limit_speed.then_some(self.settings.speed_limit), in_world);
        self.fast_forwarding = fast && !in_world;
        if cut && !std::mem::replace(&mut self.speed_told, true) {
            self.menus.toast("Speed is 100% in a world");
        }
        let Some(emulation) = &self.emulation else { return };
        // what the game's next refresh runs w/; it runs them on its own schedule
        *lock(&emulation.link.controls) = Controls {
            // a multiplayer panel takes the keys and the controller; the game runs on, nobody at its controls
            input: masked(self.gamepads.apply(self.keyboard.state()), self.online.takes_input(), &mut self.held),
            playing: !self.paused && !self.menus.over_game(),
            limit,
            stereo: self.settings.stereo != settings::Stereo3d::Off,
            quit: false,
        };

        // the picture goes up at the monitor's rate (w/ FIFO the display's vblank paces it, otherwise wait for it here),
        // the newest refresh's, or the same again while none came since
        let waits = self.backend.as_ref().is_some_and(Backend::waits_for_display);
        let repeating = self.repeats && self.settings.frame_interpolation != SmoothMotion::Off && playing && repeats_at(self.vblank);
        let shown = self.pictures.as_ref().map_or(u64::MAX, |pictures| pictures.frame);
        let mut sets = lock(&emulation.link.pictures);
        let next = |sets: &mut std::collections::VecDeque<emulation::Pictures>| emulation::next_of(sets, shown, |pictures| pictures.frame);
        if !presents_now(next(&mut sets), repeating, playing, self.last_present.elapsed()) {
            // nothing to show yet: wait for the game's next refresh, a console frame at most (standing still, until a
            // console frame after the last present)
            let wait = if playing { FRAME_TIME } else { FRAME_TIME.saturating_sub(self.last_present.elapsed()) };
            sets = emulation.link.published.wait_timeout_while(sets, wait, |sets| !next(sets)).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
        let fresh = next(&mut sets);
        // the front set goes up; it stays for the repeats while no later one waits
        let front = if sets.len() > 1 { sets.pop_front() } else { sets.front().cloned() };
        drop(sets);
        if front.is_some() {
            self.pictures = front;
        }
        // answered: the keyboard stays gone till the game took the answer
        if let Some(pictures) = self.pictures.as_mut().filter(|pictures| pictures.answers < self.answers) {
            pictures.keyboard = None;
        }
        if !presents_now(fresh, repeating, playing, self.last_present.elapsed()) {
            return;
        }
        if repeating && !waits && !fresh {
            std::thread::sleep(self.next_present.saturating_duration_since(Instant::now()));
        }
        // between the presents' returns, as XR_FRAMESTATS counts it: a present blocked for a missed vblank is the long one
        let before = self.last_present;
        let took = self.show(event_loop, fresh);
        let interval = self.last_present - before;
        // K as the pictures shown now have it (the load guard may have it lower than the pace guard's cap)
        let now = self.pictures.as_ref().and_then(|pictures| pictures.between.first()).map_or(8, |between| (1.0 / between.t).round() as usize);
        if let Some(cap) = repeating.then(|| self.pace.present(interval, self.vblank, now)).flatten() {
            self.ask(Request::Pace(cap));
        }
        if repeating && waits && waits_itself(waits, took, self.vblank) {
            std::thread::sleep(self.next_present.saturating_duration_since(Instant::now()).min(self.vblank));
        }

        let (since, ran, shown) = self.counted;
        let elapsed = since.elapsed();
        let frame = self.pictures.as_ref().map_or(0, |pictures| pictures.frame);
        if elapsed >= Duration::from_millis(500) {
            let seconds = elapsed.as_secs_f32();
            self.rates = (shown as f32 / seconds, frame.saturating_sub(ran) as f32 / seconds / (1.0 / FRAME_TIME.as_secs_f32()));
            self.counted = (Instant::now(), frame, 0);
        }
        if self.monitor_hz < 0.0 {
            self.look_at_monitor();
        }
        self.tick_stats();
        if self.last_title_update.elapsed() >= Duration::from_millis(500) {
            self.last_title_update = Instant::now();
            self.look_at_monitor();
            if let Some(window) = &self.window {
                window.set_title(&if self.paused { format!("{TITLE} \u{2014} Paused") } else { TITLE.to_owned() });
            }
        }
    }

    /// what the game's thread told: false once the game ended, and the window closes
    fn take_events(&mut self, event_loop: &ActiveEventLoop) -> bool {
        loop {
            // a redraw after the end finds the thread gone, which is no error
            let Some(emulation) = self.emulation.as_ref().filter(|_| !self.ended) else { return false };
            match emulation.events.try_recv() {
                Ok(Event::Ended(Ok(()))) => {
                    self.ended = true;
                    event_loop.exit();
                    return false;
                }
                Ok(Event::Ended(Err(error))) => {
                    self.ended = true;
                    self.fail(event_loop, error);
                    return false;
                }
                Ok(Event::Screenshot(Ok(path))) => {
                    log::info!("screenshot saved to {}", path.display());
                    self.menus.flash();
                    self.menus.toast("Screenshot saved");
                }
                Ok(Event::Screenshot(Err(error))) => self.menus.toast(format!("Screenshot: could not save, {error}")),
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.fail(event_loop, "the emulation thread stopped".into());
                    return false;
                }
            }
        }
    }

    /// presents once, with the newest pictures (again, when no refresh came since), and counts it
    fn show(&mut self, event_loop: &ActiveEventLoop, fresh: bool) -> Duration {
        let began = Instant::now();
        self.present(event_loop);
        let took = began.elapsed();
        self.counted.2 += 1;
        self.last_present = Instant::now();
        self.next_present = (self.next_present + self.vblank).max(Instant::now());
        if let (Some(stats), Some(pictures)) = (&mut self.stats, &self.pictures) {
            // the refreshes since the last present, those the window never showed too, and their time
            let (frame, game) = std::mem::replace(&mut self.seen, (pictures.frame, pictures.game));
            let between = self.between.map_or(0, |k| k as u8 + 1);
            stats.present(fresh as u8, pictures.cadence.pictures, between, pictures.frame.saturating_sub(frame) as u32, pictures.game.saturating_sub(game), took);
        }
        took
    }

    /// the monitor's refresh rate, logged when it changes (and on the first look); every half second, not every step
    /// XR_MONITOR_HZ=N pretends the monitor runs at N Hz (to check another rate's pacing)
    fn look_at_monitor(&mut self) {
        if let Some(window) = &self.window {
            let forced = std::env::var("XR_MONITOR_HZ").ok().and_then(|v| v.parse::<f64>().ok()).map(|hz| (hz * 1000.0) as u32);
            let mhz = forced.or_else(|| window.current_monitor().and_then(|monitor| monitor.refresh_rate_millihertz()));
            let hz = f64::from(mhz.unwrap_or(0)) / 1000.0;
            if (hz - self.monitor_hz).abs() > 0.01 {
                log::info!("monitor refresh rate {hz:.3} Hz (winit; 0 means unknown, 60 is assumed), presenting {}", self.backend.as_ref().map_or("without a window", |b| if b.waits_for_display() { "in FIFO, the display paces" } else { "without waiting for the display" }));
                self.monitor_hz = hz;
                self.vblank = vblank_of(mhz);
                // Smooth motion's pictures in between follow the monitor
                self.ask(Request::Live(self.live()));
            }
        }
    }

    /// the stats' second
    fn tick_stats(&mut self) {
        if let (Some(stats), Some(pictures)) = (&mut self.stats, &self.pictures) {
            stats.tick(&pictures.cadence, pictures.busy, self.monitor_hz);
        }
    }

    /// runs the interface for a frame and does what it asks, the overlay it draws over the screens
    fn interface(&mut self, event_loop: &ActiveEventLoop) -> Overlay {
        let request = self.pictures.as_ref().and_then(|pictures| pictures.keyboard.clone());
        let playing = !self.paused && !self.menus.over_game() && request.is_none();
        let view = self.emulation.as_ref().and_then(|emulation| emulation::take_online(&emulation.link));
        // where the top screen shows, in the window's physical pixels, for the names over the players
        let top = self.window.as_ref().and_then(|window| {
            let size = window.inner_size();
            let s = &self.settings;
            layout(size.width, size.height, self.arrangement(), s.orientation.screen_orientation(), s.top_shape.width(), s.integer_scale).0
        });
        let (Some(gui), Some(window)) = (&mut self.gui, &self.window) else { return Overlay::default() };
        let points = gui.ctx.pixels_per_point();
        let top = top.map(|v| egui::Rect::from_min_size(egui::pos2(v.x, v.y) / points, egui::vec2(v.width, v.height) / points));
        let (orientation, names, active) = (self.settings.orientation, self.settings.show_names, !self.menus.configure_open);
        let status = menus::Status {
            fps: self.settings.show_fps.then_some(self.rates),
            playing,
            paused: self.paused,
            fast: self.fast_forwarding,
        };
        let (menus, settings, booted, online) = (&mut self.menus, &mut self.settings, &self.booted, &mut self.online);
        let mut actions = Vec::new();
        let overlay = gui.frame(window, |ui| {
            let ctx = ui.ctx().clone();
            menus.game(&ctx, &status);
            if let Some(view) = &view {
                online.ui(&ctx, view, top, orientation, names, active, menus);
            }
            actions.extend(menus.configure(&ctx, settings, booted));
            if let Some(request) = &request {
                actions.extend(menus.keyboard(&ctx, request));
            }
            menus.message(&ctx);
        });
        for action in actions {
            match action {
                Action::Settings => self.apply_settings(),
                Action::Save => self.settings.save(),
                Action::Restart => self.restart(event_loop),
                Action::Keyboard(text, button) => {
                    self.ask(Request::Keyboard(text, button));
                    // answered: the keyboard goes now, not once the game's next pictures say so
                    self.answers += 1;
                    if let Some(pictures) = &mut self.pictures {
                        pictures.keyboard = None;
                    }
                }
            }
        }
        for request in std::mem::take(&mut self.online.requests) {
            self.ask(Request::Online(request));
        }
        overlay
    }

    fn present(&mut self, event_loop: &ActiveEventLoop) {
        let overlay = self.interface(event_loop);
        let (Some(pictures), Some(backend)) = (&self.pictures, &mut self.backend) else { return };
        // the right eye's picture only while the 3D shows
        let stereo = self.settings.stereo != settings::Stereo3d::Off;
        let blank = || ((std::sync::Arc::new(Vec::new()), Screen::Top.width(), Screen::Top.height()), None);
        let right = pictures.right.clone().filter(|_| stereo).unwrap_or_else(blank);
        let (((top, top_width, top_height), top_gpu), ((bottom, bottom_width, bottom_height), bottom_gpu)) = (&pictures.top, &pictures.bottom);
        let ((right, right_width, right_height), right_gpu) = right;
        // Smooth motion: the picture in between whose time came, the screens w/o one of their own show theirs
        let ts: Vec<f32> = pictures.between.iter().map(|between| between.t).collect();
        // timed from the first present of them, not from when they were made: a present blocked on the vblank when
        // they came would otherwise start them a picture late
        let first = match self.motion {
            Some((since, first)) if since == pictures.since => first,
            _ => self.motion.insert((pictures.since, Instant::now())).1,
        };
        self.between = pick(&ts, first.elapsed(), self.vblank, pictures.span);
        let (top_gpu, bottom_gpu, right_gpu) = match self.between.map(|k| pictures.between[k]) {
            Some(between) => (Some(between.top), between.bottom.or(*bottom_gpu), between.right.or(right_gpu).filter(|_| stereo)),
            None => (*top_gpu, *bottom_gpu, right_gpu),
        };
        let result = backend.present(
            ScreenImage { width: *top_width, height: *top_height, pixels: top, gpu: top_gpu },
            ScreenImage { width: *bottom_width, height: *bottom_height, pixels: bottom, gpu: bottom_gpu },
            ScreenImage { width: right_width, height: right_height, pixels: &right, gpu: right_gpu },
            &overlay,
        );
        match result {
            Ok(()) | Err(PresentError::OutOfDate) => {}
            Err(error) => self.fail(event_loop, format!("presentation failed: {error}")),
        }
    }
}

/// The input the game gets: none while a multiplayer panel takes the controls; once it closes, every console button
/// held at that moment (keys or controller) stays out until it is let go, then counts again. `held` is (a panel had
/// the controls last frame, the buttons still masked).
fn masked(mut input: InputState, panel: bool, held: &mut (bool, PadState)) -> InputState {
    if panel {
        *held = (true, PadState::empty());
        return InputState::default();
    }
    if std::mem::replace(&mut held.0, false) {
        held.1 = input.buttons;
    }
    held.1 &= input.buttons;
    input.buttons -= held.1;
    input
}

/// puts key on the binding at index, the binding that had it gets this one's old key, so no key does two things;
/// the binding that had it, when one did
fn rebind(keys: &mut settings::Keys, index: usize, key: KeyCode) -> Option<usize> {
    let mut all = keys.all_mut();
    let old = **all.get(index).map(|(_, bound)| bound)?;
    let mut moved = None;
    for (i, (_, bound)) in all.iter_mut().enumerate() {
        if i != index && **bound == key {
            **bound = old;
            moved = Some(i);
        }
    }
    *all[index].1 = key;
    moved
}

/// a place on the screen: x, y, width, height in pixels
type Area = (i32, i32, u32, u32);

/// where a window whose inside is want and whose frame adds chrome goes in a work area: as asked when it fits, else
/// the biggest of the same shape that does, centered
fn fit(want: (u32, u32), chrome: (u32, u32), work: Area) -> Area {
    let room = (work.2.saturating_sub(chrome.0).max(1), work.3.saturating_sub(chrome.1).max(1));
    let scale = (room.0 as f64 / want.0 as f64).min(room.1 as f64 / want.1 as f64).min(1.0);
    let size = (((want.0 as f64 * scale).floor() as u32).max(1), ((want.1 as f64 * scale).floor() as u32).max(1));
    let x = work.0 + (work.2 as i32 - (size.0 + chrome.0) as i32) / 2;
    let y = work.1 + (work.3 as i32 - (size.1 + chrome.1) as i32) / 2;
    (x.max(work.0), y.max(work.1), size.0, size.1)
}

/// a window's frame position moved just enough to have the whole window in the work area
fn inside(place: Area, chrome: (u32, u32), work: Area) -> (i32, i32) {
    let (width, height) = ((place.2 + chrome.0) as i32, (place.3 + chrome.1) as i32);
    let x = place.0.min(work.0 + work.2 as i32 - width).max(work.0);
    let y = place.1.min(work.1 + work.3 as i32 - height).max(work.1);
    (x, y)
}

/// what the window's frame (title bar, menu bar, borders) adds to its inside
fn chrome(window: &Window) -> (u32, u32) {
    let (outer, inner) = (window.outer_size(), window.inner_size());
    (outer.width.saturating_sub(inner.width), outer.height.saturating_sub(inner.height))
}

/// the work area (the screen less the taskbar) of the monitor a rectangle is on, or the window's when none is given;
/// none when the rectangle is on no monitor
#[cfg(windows)]
fn work_area(window: &Window, on: Option<Area>) -> Option<Area> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL};
    // safe bc these only read the rectangle and the window's handle, and write the info they're given
    let monitor = match on {
        Some((x, y, width, height)) => {
            let rect = RECT { left: x, top: y, right: x + width as i32, bottom: y + height as i32 };
            unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) }
        }
        None => {
            let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else { return None };
            unsafe { MonitorFromWindow(handle.hwnd.get() as _, MONITOR_DEFAULTTONEAREST) }
        }
    };
    if monitor.is_null() {
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    let work = info.rcWork;
    Some((work.left, work.top, (work.right - work.left) as u32, (work.bottom - work.top) as u32))
}

/// elsewhere the whole monitor stands in for its work area
#[cfg(not(windows))]
fn work_area(window: &Window, on: Option<Area>) -> Option<Area> {
    let monitor = match on {
        Some((x, y, ..)) => window.available_monitors().find(|monitor| {
            let (at, size) = (monitor.position(), monitor.size());
            x >= at.x && y >= at.y && x < at.x + size.width as i32 && y < at.y + size.height as i32
        })?,
        None => window.current_monitor()?,
    };
    let (at, size) = (monitor.position(), monitor.size());
    Some((at.x, at.y, size.width, size.height))
}

/// the RE:DEX gear, side pixels square, from the repo's icon
fn icon(side: u32) -> Option<winit::window::Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(&include_bytes!("../../../assets/icon.png")[..]));
    let mut reader = decoder.read_info().ok()?;
    let mut pixels = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut pixels).ok()?;
    let channels = info.color_type.samples();
    let (width, height) = (info.width, info.height);
    // each pixel the average of the block of the big one it covers
    let block = (width / side).max(1);
    let mut out = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let mut sum = [0u32; 4];
            for dy in 0..block {
                for dx in 0..block {
                    let at = (((y * block + dy).min(height - 1) * width + (x * block + dx).min(width - 1)) as usize) * channels;
                    for c in 0..4 {
                        sum[c] += if c < channels { pixels[at + c] as u32 } else { 255 };
                    }
                }
            }
            out.extend(sum.map(|value| (value / (block * block)) as u8));
        }
    }
    winit::window::Icon::from_rgba(out, side, side).ok()
}

/// does Windows show animations (Settings, Accessibility, Visual effects), the menus keep still when it doesn't
fn animations() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};
        let mut on: windows_sys::Win32::Foundation::BOOL = 1;
        // safe bc it writes one BOOL to the one given
        let read = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
        read == 0 || on != 0
    }
    #[cfg(not(windows))]
    true
}

/// the top screen over the bottom one, centered under it, as RGBA, for a screenshot
fn stack(top: &xr_hle::Screen, bottom: &xr_hle::Screen) -> (u32, u32, Vec<u8>) {
    let ((top, top_width, top_height), (bottom, bottom_width, bottom_height)) = (top, bottom);
    let width = (*top_width).max(*bottom_width);
    let height = top_height + bottom_height;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for (y, x, screen, screen_width) in [(0, (width - top_width) / 2, top, *top_width), (*top_height, (width - bottom_width) / 2, bottom, *bottom_width)] {
        for (row, line) in screen.chunks_exact((screen_width * 4) as usize).enumerate() {
            let at = (((y + row as u32) * width + x) * 4) as usize;
            pixels[at..at + line.len()].copy_from_slice(line);
        }
    }
    // the console's screens have no see-thru pixels
    for alpha in pixels.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    (width, height, pixels)
}

/// opens a folder or a web page the way the system does
fn open(target: &str) {
    #[cfg(windows)]
    let opened = std::process::Command::new("explorer").arg(target).spawn();
    #[cfg(target_os = "macos")]
    let opened = std::process::Command::new("open").arg(target).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let opened = std::process::Command::new("xdg-open").arg(target).spawn();
    if let Err(error) = opened {
        log::warn!("could not open {target}, {error}");
    }
}

/// a hash of the guest's heap (64 MiB from 0x08000000) and both screens' pixels, to compare two runs of one script (display only changes
/// must leave it the same)
fn state_hash(system: &mut System) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    let mut eat = |bytes: &[u8]| bytes.iter().for_each(|&b| hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3));
    let mut chunk = vec![0u8; 1 << 20];
    for i in 0..64u32 {
        system.memory.read_bytes(0x0800_0000 + (i << 20), &mut chunk);
        eat(&chunk);
    }
    for screen in [Screen::Top, Screen::Bottom] {
        eat(&system.read_screen_scaled(screen).0);
    }
    format!("{hash:016x}")
}

/// a screen as the presenter takes it, straight from the GPU when it shares the renderer's device and the GPU drew
/// what the screen shows, otherwise its pixels
fn shown(system: &mut System, screen: Screen) -> (xr_hle::Screen, Option<GpuScreen>) {
    if let Some(gpu) = system.gpu_screen(screen) {
        let scale = system.gpu.scale();
        return ((std::sync::Arc::new(Vec::new()), screen.width() * scale, screen.height() * scale), Some(gpu));
    }
    (system.read_screen_scaled(screen), None)
}

/// the top screen's picture for the right eye, like shown gives the screens, when the game draws in 3D
fn shown_right(system: &mut System) -> Option<(xr_hle::Screen, Option<GpuScreen>)> {
    if let Some(gpu) = system.gpu_right_eye() {
        let scale = system.gpu.scale();
        return Some(((std::sync::Arc::new(Vec::new()), Screen::Top.width() * scale, Screen::Top.height() * scale), Some(gpu)));
    }
    system.read_right_eye_scaled().map(|screen| (screen, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the window presents each new refresh, the same picture again only while repeating, and the menus over a game
    /// standing still once a console frame
    #[test]
    fn presents_new_pictures_and_repeats_only_with_smooth_motion() {
        let ms = Duration::from_millis;
        assert!(presents_now(true, false, true, ms(0)), "a new refresh goes up at once");
        assert!(presents_now(false, true, true, ms(0)), "repeating: every vblank, FIFO paces it");
        assert!(!presents_now(false, false, true, ms(5)), "Smooth motion off: no repeats between refreshes");
        assert!(!presents_now(false, false, true, ms(30)), "nor while a slow refresh runs: one present a refresh, as before");
        assert!(presents_now(false, false, false, FRAME_TIME), "a paused game's menus still get a console frame's presents");
        assert!(!presents_now(false, false, false, ms(5)));
        // a 60 or 75 Hz monitor presents once a refresh, as before M0; 90 Hz and up repeat
        assert!(!repeats_at(vblank_of(Some(60_000))) && !repeats_at(vblank_of(Some(75_000))));
        assert!(repeats_at(vblank_of(Some(90_000))) && repeats_at(vblank_of(Some(200_000))) && repeats_at(vblank_of(Some(240_000))));
    }

    /// Smooth motion makes pictures for the monitor (up to 240 Hz) only where the picture goes up again between
    /// refreshes; "60 fps", and any monitor under 90 Hz, make them for 60 (one in between at 30 fps, as before K-way)
    #[test]
    fn smooth_motion_follows_the_monitor() {
        assert_eq!(smooth_hz(SmoothMotion::Off, 200.0, true), 0);
        assert_eq!(smooth_hz(SmoothMotion::Sixty, 200.0, true), 60);
        assert_eq!(smooth_hz(SmoothMotion::Monitor, 200.0, true), 200);
        assert_eq!(smooth_hz(SmoothMotion::Monitor, 360.0, true), 240, "capped");
        assert_eq!(smooth_hz(SmoothMotion::Monitor, 143.9, true), 144);
        assert_eq!(smooth_hz(SmoothMotion::Monitor, 60.0, repeats_at(vblank_of(Some(60_000)))), 60, "a 60 Hz monitor: K = 2");
        assert_eq!(smooth_hz(SmoothMotion::Monitor, 75.0, repeats_at(vblank_of(Some(75_000)))), 60);
        assert_eq!(smooth_hz(SmoothMotion::Monitor, -1.0, true), 60, "not looked at yet");
        assert_eq!(xr_hle::steps_for(smooth_hz(SmoothMotion::Monitor, 60.0, false), 2), 2);
        assert_eq!(xr_hle::steps_for(smooth_hz(SmoothMotion::Monitor, 240.0, true), 2), 8);
    }

    /// the pictures in between go up in turn as their time comes, each for 1/K of the frame, the game's own last;
    /// at 60 Hz w/ one in between (K = 2) it's one refresh each, as before K-way
    #[test]
    fn pictures_in_between_go_up_as_their_time_comes() {
        let ms = |ms: f64| Duration::from_secs_f64(ms / 1000.0);
        let frame = FRAME_TIME * 2;
        // 60 Hz, K = 2: the refresh the picture came at shows the one in between, the next the game's own
        let sixty = vblank_of(Some(60_000));
        assert_eq!(pick(&[0.5], ms(0.0), sixty, frame), Some(0));
        assert_eq!(pick(&[0.5], ms(0.5), sixty, frame), Some(0));
        assert_eq!(pick(&[0.5], FRAME_TIME - ms(4.0), sixty, frame), None, "a refresh on, even a bit early");
        assert_eq!(pick(&[0.5], FRAME_TIME + ms(4.0), sixty, frame), None);
        // 240 Hz, K = 8: one a vblank, in order, then the game's own and it stays
        let ts: Vec<f32> = (1..8).map(|k| k as f32 / 8.0).collect();
        let vblank = vblank_of(Some(240_000));
        let shown: Vec<Option<usize>> = (0..10).map(|n| pick(&ts, vblank * n, vblank, frame)).collect();
        assert_eq!(shown, [Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), None, None, None]);
        // 200 Hz, K = 7: never backwards, every one shown, the game's own after the last
        let ts: Vec<f32> = (1..7).map(|k| k as f32 / 7.0).collect();
        let vblank = vblank_of(Some(200_000));
        let shown: Vec<Option<usize>> = (0..9).map(|n| pick(&ts, vblank * n, vblank, frame)).collect();
        let order: Vec<usize> = shown.iter().map(|k| k.unwrap_or(6)).collect();
        assert!(order.windows(2).all(|pair| pair[0] <= pair[1]), "{shown:?}");
        assert!((0..6).all(|k| shown.contains(&Some(k))), "{shown:?}");
        // 20 fps (gap 3): the same pictures over a longer frame
        assert_eq!(pick(&ts, vblank * 2, vblank, FRAME_TIME * 3), Some(1));
        // the GPU drew fewer (VRAM guard): the latest whose time came of those there are
        assert_eq!(pick(&[0.125, 0.25], vblank * 5, vblank, frame), Some(1));
        // none in between, or a game running unlocked (no frame length): its own
        assert_eq!(pick(&[], ms(0.0), vblank, frame), None);
        assert_eq!(pick(&[0.5], ms(0.0), vblank, Duration::ZERO), None);
    }

    /// the pace guard takes K down a step a second while more than 0.5% of the presents come 1.5 vblanks late, to 2,
    /// and up again a step after a calm wait that doubles w/ each step down
    #[test]
    fn the_pace_guard_trades_pictures_for_presents_on_time() {
        assert_eq!(pace_step(8, (200, 5), (200, 5), 0, 4), (7, 0, 8), "2.5% in a second: down at once, wait longer");
        assert_eq!(pace_step(7, (200, 2), (600, 2), 0, 8), (7, 1, 8), "a cluster of 2 in a field at 0.3%: holds, calm");
        assert_eq!(pace_step(7, (200, 2), (600, 4), 0, 8), (6, 0, 16), "0.67% over 3 seconds: down");
        assert_eq!(pace_step(7, (200, 2), (250, 3), 0, 8), (7, 1, 8), "under 3 seconds to tell");
        // isolated late presents don't keep K down: only a step down starts the calm count over
        let mut state = (5, 0, 8);
        for second in 0..8 {
            state = pace_step(state.0, (200, (second % 3 == 0) as u32), (600, 1), state.1, state.2);
        }
        assert_eq!(state, (6, 0, 8), "up after 8 seconds w/ a late present every third");
        assert_eq!(pace_step(2, (200, 9), (1000, 30), 0, 16), (2, 0, 16), "never under one picture in between");
        assert_eq!(pace_step(6, (20, 9), (20, 9), 0, 4), (6, 1, 4), "too few presents to tell: calm");
        assert_eq!(pace_step(6, (200, 0), (1000, 0), 6, 8), (6, 7, 8), "calm, still waiting");
        assert_eq!(pace_step(6, (200, 0), (1000, 0), 7, 8), (7, 0, 8), "calm long enough: up");
        assert_eq!(pace_step(6, (200, 1), (1000, 4), 7, 8), (7, 0, 8), "w/ a late present too");
        assert_eq!(pace_step(8, (200, 0), (1000, 0), 70, 8), (8, 71, 8), "at most 8");
        assert_eq!(pace_step(7, (200, 9), (200, 9), 0, 16).2, 16, "the wait stops doubling at 16 s");
        assert_eq!(pace_step(7, (200, 9), (200, 9), 0, 8).2, 16);
        let mut guard = PaceGuard::new();
        let vblank = vblank_of(Some(200_000));
        assert_eq!(guard.present(Duration::from_secs(5), vblank, 8), None, "a pause isn't late");
        assert_eq!((guard.presents, guard.late), (0, 0));
        guard.present(vblank * 2, vblank, 8);
        assert_eq!((guard.presents, guard.late), (1, 1));
        // the cap is absolute, down from what the game shows now: K at 3 (the load guard, or "60" w/ 2) and a late
        // second sends 2, not 7; calm seconds bring it back up a step at a time, to 8
        let mut guard = PaceGuard { since: Instant::now() - Duration::from_secs(2), presents: 199, late: 5, ..PaceGuard::new() };
        assert_eq!(guard.present(vblank, vblank, 3), Some(2));
        let mut sent = Vec::new();
        for _ in 0..200 {
            guard.since = Instant::now() - Duration::from_secs(2);
            guard.presents = 199;
            sent.extend(guard.present(vblank, vblank, 8));
        }
        assert_eq!(sent, [3, 4, 5, 6, 7, 8], "back to 8 in steps");
    }

    /// a present that comes back in a fraction of a vblank (minimized, zero size) leaves the loop to wait, a blocking one doesn't
    #[test]
    fn waits_when_present_returns_early() {
        let (vblank, ms) = (vblank_of(Some(60_000)), Duration::from_millis);
        assert!(!waits_itself(true, ms(16), vblank), "FIFO blocked a vblank");
        assert!(!waits_itself(true, ms(5), vblank), "queued ahead, still paced");
        assert!(waits_itself(true, Duration::from_micros(50), vblank), "returned at once: don't spin");
        assert!(waits_itself(false, ms(16), vblank), "mailbox and immediate never wait");
    }

    /// the monitor's rate in millihertz gives its refresh interval, 60 Hz when it isn't known
    #[test]
    fn vblank_from_the_monitor() {
        assert_eq!(vblank_of(None), vblank_of(Some(60_000)));
        assert_eq!(vblank_of(Some(0)), vblank_of(Some(60_000)));
        assert_eq!(vblank_of(Some(240_000)).as_micros(), 4166);
        assert_eq!(vblank_of(Some(59_940)).as_micros(), 16683);
        assert_eq!(vblank_of(Some(u32::MAX)), vblank_of(Some(1_000_000)), "clamped");
    }

    /// the window paces frames at the console's 59.83 a second
    #[test]
    fn a_frame_is_the_consoles() {
        assert_eq!(FRAME_TIME.as_micros(), 16713);
    }

    /// a speed limit stretches or shrinks the frame's time, sound plays only up to the console's speed
    #[test]
    fn the_speed_limit_paces_frames_and_sound() {
        assert_eq!(frame_time(Some(100)), FRAME_TIME);
        assert_eq!(frame_time(Some(200)), FRAME_TIME / 2);
        assert_eq!(frame_time(Some(60)), FRAME_TIME * 100 / 60);
        assert_eq!(frame_time(Some(50)), FRAME_TIME * 100 / 60, "no slower than the sound keeps up with");
        assert_eq!(frame_time(Some(10)), FRAME_TIME * 100 / 60);
        assert_eq!(frame_time(None), Duration::ZERO);
        assert!(plays_sound(Some(100)) && plays_sound(Some(50)));
        assert!(!plays_sound(Some(200)) && !plays_sound(None));
    }

    /// in a world the game runs at the console's speed at most: no fast forward, no limit over 100% or unlocked;
    /// slower stays. Says when the player's wish was cut (for the toast)
    #[test]
    fn a_world_caps_the_speed_at_the_consoles() {
        assert_eq!(world_speed(false, Some(300), false), (Some(300), false), "offline: as asked");
        assert_eq!(world_speed(true, Some(100), false), (None, false), "offline: fast forward");
        assert_eq!(world_speed(true, Some(100), true), (Some(100), true), "no fast forward in a world");
        assert_eq!(world_speed(false, None, true), (Some(100), true), "unlocked");
        assert_eq!(world_speed(false, Some(200), true), (Some(100), true));
        assert_eq!(world_speed(false, Some(100), true), (Some(100), false));
        assert_eq!(world_speed(false, Some(60), true), (Some(60), false), "slower is fine");
    }

    /// a key already on another button moves over, that button gets the old key
    #[test]
    fn rebinding_a_taken_key_swaps_them() {
        let mut keys = settings::Keys::default();
        let (a, b) = (keys.a, keys.b);
        assert_eq!(rebind(&mut keys, 0, b), Some(1));
        assert_eq!((keys.a, keys.b), (b, a));
        assert_eq!(rebind(&mut keys, 0, KeyCode::KeyP), None);
        assert_eq!(keys.a, KeyCode::KeyP);
        assert_eq!(keys.b, a);
    }

    /// the default 2x window at 1080p and 1440p work areas (taskbar 48 high), at 100%, 125% and 150% display scale:
    /// it keeps its size where it fits, otherwise shrinks to the biggest of its shape that does, centered either way
    #[test]
    fn the_window_fits_the_screen() {
        // title bar and menu bar 59, borders 16, as on Windows 11
        let chrome = (16, 59);
        let hd = (0, 0, 1920, 1032);
        let qhd = (0, 0, 2560, 1392);
        // 800x960 is the stacked screens at 2x
        assert_eq!(fit((800, 960), chrome, hd), (552, 6, 800, 960), "fits at 100%, centered");
        assert_eq!(fit((1000, 1200), chrome, hd), (547, 0, 810, 973), "125% doesn't, shrinks to the work area's height");
        assert_eq!(fit((1200, 1440), chrome, hd), (547, 0, 810, 973));
        assert_eq!(fit((800, 960), chrome, qhd), (872, 186, 800, 960));
        assert_eq!(fit((1200, 1440), chrome, qhd), (717, 0, 1110, 1333), "150% at 1440p");
        let (_, _, width, height) = fit((1200, 1440), chrome, qhd);
        assert!(((width as f64 / height as f64) - 1200.0 / 1440.0).abs() < 0.002, "same shape");
        // a monitor left of the main one, w/ its taskbar on top
        assert_eq!(fit((800, 960), chrome, (-1920, 40, 1920, 1040)), (-1368, 50, 800, 960));
        // a window hanging off the bottom right comes back in, one inside stays put
        assert_eq!(inside((1500, 300, 800, 960), chrome, hd), (1104, 13));
        assert_eq!(inside((100, 10, 800, 960), chrome, hd), (100, 10));
    }

    /// A button held as a multiplayer panel closes (the A, B or Select that closed it) never reaches the game until it is
    /// let go; one pressed afresh after that does, and the others go thru meanwhile.
    #[test]
    fn buttons_held_as_a_panel_closes_stay_out_of_the_game() {
        let held_down = |buttons: PadState| InputState { buttons, ..InputState::default() };
        let mut held = (false, PadState::empty());
        // the panel has the controls: nothing reaches the game
        assert_eq!(masked(held_down(PadState::A | PadState::UP), true, &mut held).buttons, PadState::empty());
        // it closes with A (and the d-pad) still down: masked
        assert_eq!(masked(held_down(PadState::A | PadState::UP), false, &mut held).buttons, PadState::empty());
        // still held, plus a fresh B: B goes thru, A and up stay out
        assert_eq!(masked(held_down(PadState::A | PadState::UP | PadState::B), false, &mut held).buttons, PadState::B);
        // A let go, up still down
        assert_eq!(masked(held_down(PadState::UP), false, &mut held).buttons, PadState::empty());
        // A pressed again afresh: the game gets it now
        assert_eq!(masked(held_down(PadState::A | PadState::UP), false, &mut held).buttons, PadState::A);
        assert_eq!(masked(held_down(PadState::A), false, &mut held).buttons, PadState::A);
        // sticks are not buttons: the circle pad goes thru (the player keeps walking where they held it)
        let walking = InputState { circle_x: 1.0, ..InputState::default() };
        assert_eq!(masked(walking, false, &mut held).circle_x, 1.0);
    }

    /// the window's icon is the gear, at the sizes Windows asks for
    #[test]
    fn the_icon_is_there() {
        assert!(icon(32).is_some() && icon(256).is_some());
    }

    /// a screenshot has the top screen over the bottom one, centered, w/ every pixel solid
    #[test]
    fn screenshots_stack_the_screens() {
        let screen = |width: u32, height: u32, value: u8| (std::sync::Arc::new(vec![value; (width * height * 4) as usize]), width, height);
        let (width, height, pixels) = stack(&screen(4, 2, 9), &screen(2, 2, 7));
        assert_eq!((width, height), (4, 4));
        let at = |x: u32, y: u32| &pixels[((y * width + x) * 4) as usize..][..4];
        assert_eq!(at(0, 0), [9, 9, 9, 255]);
        assert_eq!(at(0, 2), [0, 0, 0, 255], "beside the narrower bottom screen");
        assert_eq!(at(1, 3), [7, 7, 7, 255]);
    }
}
