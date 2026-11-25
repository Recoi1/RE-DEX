//! The start screen: RE:DEX's own front door when it opens with no arguments. Game cards (a built game shows a blurred,
//! dimmed picture of its own title screen, made on this PC), "Add your game" (a file picker, then the builder's
//! progress), a row for characters, online or offline, the world, and Play. It paints with the liquid glass
//! look (glass.rs) on the same winit window + presenter the game uses, with no game booted: Play hands a [`Launch`]
//! back and the runtime starts the game in a fresh process.
pub mod account;
pub mod characters;
pub mod creator;
pub mod glass;
pub mod library;
pub mod mods;
pub mod preview;
pub mod storage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use egui::{pos2, vec2, Stroke, Align, Align2, Color32, ColorImage, FontId, Key, Layout, Rect, Sense, TextureHandle, TextureOptions, Ui, UiBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};
use zakuro_gpu::{Overlay, PresentError, RendererKind, ScreenImage};

use crate::gamepad::Gamepads;
use crate::gui::Gui;
use crate::menus::{Action, Menus};
use crate::online::valid_address;
use crate::present::Backend;
use crate::settings::{PadButtons, Renderer, Settings};
use account::{Note, Servers, Session};
use characters::{has_save, now_ms, Store};
use glass::{Glass, Icon, BODY, TITLE};
use creator::{Creator, CreatorAction};
use preview::{preview_game, stale_cards, Cards, Warm};
use redex_proto::{MAX_CHARACTERS, TITLE_X, TITLE_Y};
use xr_net::x::field::Appearance;
use library::{label, GameEntry, Library};
use redex_builder::{build_state, BuildJob, BuildState, Event, Stage, Toolchain};

/// what the player chose to play: the runtime starts `redex --cci <dump> --build <build_dir> [--world W]
/// [--character <id>]`
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub dump: PathBuf,
    pub build_dir: PathBuf,
    pub world: Option<String>,
    pub offline: bool,
    /// played as this character (its own saves); None plays the shared `<data>/user` saves
    pub character: Option<u64>,
    /// an online character of the account signed in to this service (`--account-server`): the game joins `world`
    /// with world tickets from the session kept on this PC, never with a password
    pub account: Option<String>,
}

/// opens the start screen and returns what to play, None when the window was closed or Quit was pressed. `tools` is
/// the builder's toolchain, or why there is none (a dev tree): then "Add your game" shows disabled.
pub fn run_start(data: PathBuf, tools: Result<Toolchain, String>) -> Result<Option<Launch>, String> {
    let settings = Settings::load();
    if let Err(why) = &tools {
        // for the log, not the screen
        log::info!("start screen: building games is off, {why}");
    }
    let mut app = Start::new(data, tools, settings);
    app.session.check();
    #[cfg(feature = "test-hooks")]
    {
        // the MP test harness drives the account panel: XR_TEST_ACCOUNT_PANEL opens it, XR_TEST_SIGNIN=<login> fills
        // it with REDEX_PASSWORD and presses Sign in once the first check is back (never in a release: make-release
        // builds without mp-tests)
        if std::env::var_os("XR_TEST_ACCOUNT_PANEL").is_some() {
            app.panel = Some(Panel::SignIn);
        }
        if let (Ok(login), Ok(password)) = (std::env::var("XR_TEST_SIGNIN"), std::env::var("REDEX_PASSWORD")) {
            (app.panel, app.form.login, app.form.password, app.hook_submit) = (Some(Panel::SignIn), login, password, true);
        }
    }
    #[cfg(windows)]
    let event_loop = {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        EventLoop::builder().with_any_thread(true).build()
    };
    #[cfg(not(windows))]
    let event_loop = EventLoop::new();
    let event_loop = event_loop.map_err(|e| format!("create an event loop: {e}"))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    match app.failure.take() {
        Some(e) => Err(e),
        None => Ok(app.launch.take()),
    }
}

impl Start {
    /// the screen's state, before its window opens: the data folder's games and characters, the first character chosen
    fn new(data: PathBuf, tools: Result<Toolchain, String>, settings: Settings) -> Start {
    let storage = storage::Storage::new(&data);
    // nothing is asked until run_start says so (`check`): a test's Start never calls out
    let session = Session::idle(data.clone(), Servers::of(&settings));
    let mut app = Start {
        session,
        online_view: true,
        p2p: false,
        panel: None,
        form: Form::default(),
        hook_submit: false,
        library: Library::load(&data),
        store: Store::load(&data),
        exe: std::env::current_exe().unwrap_or_default(),
        data,
        stamp: tools.as_ref().ok().map(Toolchain::stamp),
        tools,
        offline: settings.world_address.is_empty(),
        settings,
        selected: 0,
        character: None,
        removing: None,
        removed: None,
        page: 0,
        cards: None,
        check_cards: true,
        faces: HashMap::new(),
        creator_request: None,
        creator: None,
        mods: None,
        installer: mods::Installer::new(),
        warm: None,
        storage,
        focus: 0,
        show_focus: false,
        job: None,
        error: None,
        editing: None,
        edit_error: None,
        nav: Nav::default(),
        menus: Menus::default(),
        gamepads: Gamepads::new(PadButtons::default()),
        launch: None,
        quit: false,
        failure: None,
        started: Instant::now(),
        pictures: HashMap::new(),
        logo: None,
        glass: None,
        gui: None,
        backend: None,
        window: None,
    };
    app.character = app.store.characters.first().map(|c| c.id);
    app
    }
}

/// one build in flight
struct Running {
    /// None until the Check stage has identified the dump (a game being added)
    build_id: Option<String>,
    dump: PathBuf,
    job: BuildJob,
    stage: &'static str,
    done: u32,
    total: u32,
    jobs: u32,
}

/// what the keyboard and controller asked this frame: d-pad or arrows move (left and up go back, right and down go
/// on), A or Enter selects, B or Esc goes back; the creator also takes the d-pad's own directions and L and R
#[derive(Default, Clone, Copy)]
struct Nav {
    prev: bool,
    next: bool,
    select: bool,
    back: bool,
    pad: creator::Pad,
}

/// the account panel's pages
#[derive(Debug, Clone, Copy, PartialEq)]
enum Panel {
    SignIn,
    Register,
    Forgot,
    Account,
}

/// what the account panel's fields hold; the password is taken out (so cleared) the moment it is sent
#[derive(Default)]
struct Form {
    login: String,
    password: String,
    email: String,
    username: String,
    trainer: String,
}

struct Start {
    /// the account (account.rs): who is signed in, the online characters, whether the official server answers
    session: Session,
    /// the row shows the account's characters (while signed in), else this PC's
    online_view: bool,
    /// "Play with friends (P2P)" was chosen while the official server is unreachable
    p2p: bool,
    /// the account panel, while it is open (it stands in for the character row)
    panel: Option<Panel>,
    form: Form,
    /// the test harness filled the sign-in form: it is sent once nothing else is out (test-hooks)
    hook_submit: bool,
    data: PathBuf,
    tools: Result<Toolchain, String>,
    /// the tools' stamp: a game built by other tools needs an update
    stamp: Option<String>,
    library: Library,
    settings: Settings,
    offline: bool,
    selected: usize,
    /// every character in characters.json (read when the window opens; never held across a game run)
    store: Store,
    exe: PathBuf,
    /// the character Play plays as
    character: Option<u64>,
    /// "Remove this character?" is up for it
    removing: Option<u64>,
    /// a character was removed: where its saves stayed, said until OK
    removed: Option<String>,
    /// the character row's page, three characters to a page
    page: usize,
    /// the card pictures being drawn (a preview child behind them), None when none are missing
    cards: Option<Cards>,
    /// a card picture may be missing: look for them again once a game is ready
    check_cards: bool,
    /// each character's card picture as made for the look in `.0`, None when it has none (yet)
    faces: HashMap<u64, (Vec<u8>, Option<TextureHandle>)>,
    /// the character creator was asked for: None-inside for a new character, Some(id) to edit one's look
    creator_request: Option<Option<u64>>,
    /// the creator, while it is open (it covers the screen under the logo)
    creator: Option<Creator>,
    /// the Mods screen, while it is open (it covers the screen under the logo, like the creator)
    mods: Option<mods::Mods>,
    /// a mod install, kept here so it outlives the Mods screen; Play waits for it
    installer: mods::Installer,
    /// both preview states being made after a build, so the creator opens on a ready one
    warm: Option<Warm>,
    /// Pokémon Storage (PKVault, a program of its own): its note, download, or the process while it runs
    storage: storage::Storage,
    focus: usize,
    show_focus: bool,
    job: Option<Running>,
    error: Option<String>,
    /// the world address being typed, while the World pill is open
    editing: Option<String>,
    edit_error: Option<&'static str>,
    nav: Nav,
    /// the game's own Configure window, opened by the Settings pill
    menus: Menus,
    gamepads: Gamepads,
    launch: Option<Launch>,
    quit: bool,
    failure: Option<String>,
    started: Instant,
    /// each built game's title picture, None when it has none (yet)
    pictures: HashMap<String, Option<TextureHandle>>,
    logo: Option<TextureHandle>,
    glass: Option<Glass>,
    gui: Option<Gui>,
    // the backend is dropped before the window it draws on
    backend: Option<Backend>,
    window: Option<Window>,
}

/// what Play starts: None until a game is selected and built, and a character is chosen
fn launch_for(data: &Path, game: Option<&GameEntry>, ready: bool, character: Option<u64>, offline: bool, world: &str) -> Option<Launch> {
    let (game, character) = (game.filter(|_| ready)?, character?);
    Some(Launch {
        dump: game.dump.clone(),
        build_dir: data.join("build").join(&game.build_id),
        world: (!offline && !world.is_empty()).then(|| world.to_owned()),
        offline,
        character: Some(character),
        account: None,
    })
}

fn stage_name(s: Stage) -> &'static str {
    match s {
        Stage::Check => "Checking your game",
        Stage::Unpack => "Unpacking",
        Stage::Recompile => "Translating the code",
        Stage::Build => "Building",
        Stage::Verify => "Making sure it runs",
    }
}

fn game_name(build_id: &str) -> String {
    match build_id.chars().next() {
        Some('x') => "Pokémon X".into(),
        Some('y') => "Pokémon Y".into(),
        _ => build_id.to_owned(),
    }
}

fn dim(c: Color32, k: f32) -> Color32 {
    c.gamma_multiply(k)
}

/// a failure said as ours, with one next step
const RED: Color32 = Color32::from_rgb(255, 176, 168);
const GREEN: Color32 = Color32::from_rgb(88, 214, 141);
const AMBER: Color32 = Color32::from_rgb(240, 180, 80);
/// 13 px text on a card or the backdrop's red corner: lighter than BODY so it keeps 4.5:1
const CARD_BODY: Color32 = Color32::from_rgb(0xD0, 0xD4, 0xDC);

fn text(ui: &Ui, pos: egui::Pos2, align: Align2, s: &str, size: f32, color: Color32) {
    ui.painter().text(pos, align, s, FontId::proportional(size), color);
}

fn at<R>(ui: &mut Ui, rect: Rect, layout: Layout, f: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope_builder(UiBuilder::new().max_rect(rect).layout(layout), f).inner
}

fn logical(window: &Window) -> [u32; 2] {
    let s = window.inner_size().to_logical::<f32>(window.scale_factor());
    [s.width.max(1.0) as u32, s.height.max(1.0) as u32]
}

/// text in bold (the font has one weight: drawn a hair apart), centred on `pos.y`
fn bold(ui: &Ui, pos: egui::Pos2, s: &str, size: f32, color: Color32) {
    let g = ui.painter().layout_no_wrap(s.to_owned(), FontId::proportional(size), color);
    for dx in [0.0, 0.4, 0.8] {
        ui.painter().galley(pos2(pos.x + dx, pos.y - g.size().y / 2.0), g.clone(), color);
    }
}

/// `s` cut with an ellipsis to fit `max` px
fn fit(ui: &Ui, s: &str, size: f32, max: f32) -> String {
    let width = |t: &str| ui.painter().layout_no_wrap(t.to_owned(), FontId::proportional(size), Color32::WHITE).size().x;
    if width(s) <= max {
        return s.to_owned();
    }
    let mut t = s.to_owned();
    while t.chars().count() > 1 && width(&format!("{t}…")) > max {
        t.pop();
    }
    t + "…"
}

/// a card picture (an RGBA PNG) as the 64 x 104 figure, area-averaged so it stays crisp and uploaded as is (1:1 on screen)
fn load_card(ctx: &egui::Context, name: &str, bytes: &[u8]) -> Option<TextureHandle> {
    const W: usize = 64;
    const H: usize = 104;
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.bit_depth != png::BitDepth::Eight || info.color_type != png::ColorType::Rgba {
        return None;
    }
    let (w, h) = (info.width as usize, info.height as usize);
    // the figure's shape, cut from the middle of the picture (a crop is taller than it, a studio picture wider)
    let (sw, sh) = ((h * W / H).min(w), (w * H / W).min(h));
    let (left, top) = ((w - sw) / 2, (h - sh) / 2);
    let mut pixels = Vec::with_capacity(W * H);
    for y in 0..H {
        let y0 = top + y * sh / H;
        let y1 = (top + (y + 1) * sh / H).max(y0 + 1).min(h);
        for x in 0..W {
            let x0 = left + x * sw / W;
            let x1 = (left + (x + 1) * sw / W).max(x0 + 1).min(w);
            let mut acc = [0u32; 3];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    (0..3).for_each(|c| acc[c] += buf[(sy * w + sx) * 4 + c] as u32);
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            pixels.push(Color32::from_rgb((acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8));
        }
    }
    let img = ColorImage { size: [W, H], source_size: vec2(W as f32, H as f32), pixels };
    Some(ctx.load_texture(name, img, TextureOptions::NEAREST))
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max - 1).collect::<String>() + "…"
    }
}

/// a PNG as a texture, thinned to at most `max_w` wide, blurred and dimmed (a game's title screen); None if unreadable
fn load_png(ctx: &egui::Context, name: &str, bytes: &[u8], max_w: usize, blur: usize, dim: f32) -> Option<TextureHandle> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let ch = info.color_type.samples();
    let (w, h) = (info.width as usize, info.height as usize);
    let step = (w / max_w).max(1);
    let (ow, oh) = (w / step, h / step);
    let mut pixels = Vec::with_capacity(ow * oh);
    for y in 0..oh {
        for x in 0..ow {
            // the average of a step x step block, premultiplied so a transparent edge stays clean
            let mut acc = [0f32; 4];
            for (dy, dx) in (0..step).flat_map(|dy| (0..step).map(move |dx| (dy, dx))) {
                let i = ((y * step + dy) * w + x * step + dx) * ch;
                let (r, g, b, a) = match ch {
                    4 => (buf[i], buf[i + 1], buf[i + 2], buf[i + 3]),
                    3 => (buf[i], buf[i + 1], buf[i + 2], 255),
                    2 => (buf[i], buf[i], buf[i], buf[i + 1]),
                    _ => (buf[i], buf[i], buf[i], 255),
                };
                let k = a as f32 / 255.0;
                acc[0] += r as f32 * k;
                acc[1] += g as f32 * k;
                acc[2] += b as f32 * k;
                acc[3] += a as f32;
            }
            let n = (step * step) as f32;
            pixels.push(Color32::from_rgba_premultiplied((acc[0] / n).round() as u8, (acc[1] / n).round() as u8, (acc[2] / n).round() as u8, (acc[3] / n).round() as u8));
        }
    }
    let mut img = ColorImage { size: [ow, oh], source_size: vec2(ow as f32, oh as f32), pixels };
    if blur > 0 {
        glass::box_blur(&mut img, blur, 2);
    }
    if dim < 1.0 {
        for p in &mut img.pixels {
            *p = Color32::from_rgb((p.r() as f32 * dim) as u8, (p.g() as f32 * dim) as u8, (p.b() as f32 * dim) as u8);
        }
    }
    Some(ctx.load_texture(name, img, TextureOptions::LINEAR))
}

impl Start {
    fn cores() -> u32 {
        std::thread::available_parallelism().map_or(2, |n| n.get() as u32)
    }

    fn state(&self, build_id: &str) -> BuildState {
        build_state(&self.data, build_id, self.stamp.as_deref())
    }

    fn start_build(&mut self, dump: PathBuf, build_id: Option<String>, jobs: u32) {
        let Ok(tools) = &self.tools else { return };
        self.error = None;
        let job = BuildJob::start(dump.clone(), self.data.clone(), tools.clone(), jobs);
        self.job = Some(Running { build_id, dump, job, stage: "Starting", done: 0, total: 0, jobs });
    }

    /// takes in what the build thread sent
    fn pump(&mut self) {
        let Some(run) = &mut self.job else { return };
        let (mut finished, mut identified, mut built) = (None, false, false);
        while let Ok(event) = run.job.events().try_recv() {
            match event {
                Event::Stage(s) => (run.stage, run.done, run.total) = (stage_name(s), 0, 0),
                Event::Identified { build_id } => (run.build_id, identified) = (Some(build_id.to_owned()), true),
                Event::Progress { done, total } => (run.done, run.total) = (done, total),
                Event::Log(line) => log::info!("build: {line}"),
                Event::Done { .. } => (finished, built) = (Some(None), true),
                Event::Failed { stage, message } => {
                    log::warn!("build stopped at {stage:?}: {message}");
                    // the first line is the player's (what went wrong and one next step), the rest is for the log
                    finished = Some((message != "cancelled").then(|| message.lines().next().unwrap_or_default().to_owned()))
                }
            }
        }
        let (id, dump) = (run.build_id.clone(), run.dump.clone());
        if let (true, Some(id)) = (identified, &id) {
            self.remember(dump, id);
        }
        if let Some(error) = finished {
            if let Some(id) = &id {
                self.pictures.remove(id);
            }
            self.error = error.or(self.error.take());
            self.job = None;
        }
        if let (true, Some(game)) = (built, preview_game(&self.library, &self.data)) {
            self.warm = Some(Warm::start(game, &self.data));
        }
    }

    /// the identified game joins the list (or its entry follows the file to where it is now) and is selected
    fn remember(&mut self, dump: PathBuf, id: &str) {
        let _ = self.library.add_with(dump, |_| Ok(id.to_owned()));
        self.selected = self.library.games.iter().position(|g| g.build_id == id).unwrap_or(0);
        if let Err(e) = self.library.save(&self.data) {
            log::warn!("could not save the games list: {e}");
            self.error = Some("We couldn't save your games list. Check that the RE:DEX data folder can be written to.".to_owned());
        }
    }

    fn add_game(&mut self) {
        let Some(path) = pick_dump() else { return };
        // the builder's Check stage identifies the file off this thread; the game joins the list once it is known, and
        // one already built is done right there
        self.start_build(path, None, (Self::cores() / 2).max(1));
    }

    /// the selected game is built and nothing is building
    fn ready(&self) -> bool {
        let game = self.library.games.get(self.selected);
        self.job.is_none() && game.is_some_and(|g| self.state(&g.build_id) == BuildState::Ready && g.dump.is_file())
    }

    /// what Play would start, None until the selected game is built and a character is chosen
    fn playable(&self) -> Option<Launch> {
        if self.online_view() {
            // an online character plays on the official world only, with the session kept on this PC
            let ready = self.ready() && self.session.can_play_online() && self.character.is_some_and(|id| self.shown_get(id).is_some());
            return launch_for(&self.data, self.library.games.get(self.selected), ready, self.character, false, &self.session.servers.world)
                .map(|l| Launch { account: Some(self.session.servers.accounts.clone()), ..l });
        }
        launch_for(&self.data, self.library.games.get(self.selected), self.ready(), self.character, self.offline, &self.settings.world_address)
    }

    /// the row shows the account's characters: signed in, with the Online tab chosen
    fn online_view(&self) -> bool {
        self.online_view && self.session.me().is_some()
    }

    /// the characters the row shows: the account's (online) or this PC's
    fn shown(&self) -> &[characters::Character] {
        if self.online_view() {
            &self.session.characters
        } else {
            &self.store.characters
        }
    }

    fn shown_get(&self, id: u64) -> Option<&characters::Character> {
        self.shown().iter().find(|c| c.id == id)
    }

    /// why an online character cannot play just now
    fn online_blocker(&self) -> &'static str {
        match (self.session.busy(), self.session.me()) {
            (Some(b), _) => b,
            _ if self.session.offers_p2p() => "The official server is unreachable",
            (None, Some(m)) if !m.email_verified => "Confirm your email to play online",
            _ => "Make a character to play",
        }
    }

    /// takes in what the account's call brought back; a sign-in the test harness asked for goes out once nothing is
    fn pump_session(&mut self) {
        if !self.session.poll() {
            return;
        }
        if self.session.me().is_some() && self.panel == Some(Panel::SignIn) {
            self.close_panel();
        }
        if self.session.unreachable.is_none() {
            // the server is back: "Play with friends" is offered again next time it is down
            self.p2p = false;
        }
        // the row follows what the account has now
        let (pages, _) = self.row();
        self.page = self.page.min(pages - 1);
        if self.character.is_none_or(|id| self.shown_get(id).is_none()) {
            self.character = self.shown().first().map(|c| c.id);
        }
        self.check_cards = true;
    }

    /// closes the account panel and clears its password field (however it closes); whether it was open
    fn close_panel(&mut self) -> bool {
        self.form.password.clear();
        self.panel.take().is_some()
    }

    /// what the creator did: a local character saved (new or an edit) stays on this PC; a new online one (decided when
    /// the creator opened) goes to the account and was never in characters.json
    fn creator_done(&mut self, action: CreatorAction) {
        match action {
            CreatorAction::None => {}
            CreatorAction::Cancel => self.creator = None,
            CreatorAction::Saved(id) => {
                self.creator = None;
                self.store = Store::load(&self.data);
                self.faces.remove(&id);
                self.character = Some(id);
                self.page = self.shown().iter().position(|c| c.id == id).unwrap_or(0) / 3;
            }
            CreatorAction::SavedOnline(c) => {
                self.creator = None;
                self.faces.remove(&c.id);
                (self.character, self.page) = (Some(c.id), 0);
                self.session.create_online(c);
            }
        }
    }

    /// loads a built game's title picture once
    fn ensure_title(&mut self, ctx: &egui::Context, id: &str) {
        if self.pictures.contains_key(id) {
            return;
        }
        let file = self.data.join("build").join(id).join("title.png");
        let tex = std::fs::read(&file).ok().and_then(|b| load_png(ctx, &format!("title-{id}"), &b, 240, 2, 0.75));
        self.pictures.insert(id.to_owned(), tex);
    }

    /// a click, or A on the focused item; the focus ring is the hover rim, held
    fn item(&mut self, ui: &Ui, r: &egui::Response, item: usize, radius: f32) -> bool {
        if r.clicked() {
            self.focus = item;
            self.show_focus = false;
        }
        if self.show_focus && self.focus == item {
            ui.painter().add(egui::Shape::mesh(glass::rim_mesh(r.rect, radius, 1.0)));
        }
        r.clicked() || (self.nav.select && self.focus == item)
    }

    /// a plain pill on a card (no glass inside glass)
    fn flat_pill(&self, ui: &mut Ui, glass: &Glass, rect: Rect, id: usize, s: &str) -> bool {
        let r = ui.interact(rect, ui.id().with(("flat", id)), Sense::click());
        let hover = glass.hover_of(ui, &r);
        ui.painter().rect_filled(rect, rect.height() / 2.0, Color32::from_white_alpha((36.0 + 30.0 * hover) as u8));
        text(ui, rect.center(), Align2::CENTER_CENTER, s, 14.0, TITLE);
        r.clicked()
    }

    fn ui(&mut self, ui: &mut Ui, glass: &Glass) {
        let full = ui.max_rect();
        let (w, h) = (full.width(), full.height());
        glass.paint_backdrop(ui.painter(), full);
        let n = self.library.games.len();
        // focus order: the games, Add, the character row (Import, three cards, New, the page arrows, Edit look, Remove,
        // Pokémon Storage), online or offline, World, Settings, Mods, Play, then the account pill, Try again, Play with
        // friends (P2P) and the Online / On this PC tabs
        let (add, chars) = (n, n + 1);
        let (toggle, world, settings, mods_item, play) = (n + 11, n + 12, n + 13, n + 14, n + 15);
        let (acct, retry, p2p, tabs) = (n + 16, n + 17, n + 18, n + 19);
        let count = n + 21;
        let (pages, visible) = self.row();
        let selected_here = self.character.and_then(|id| self.shown().iter().position(|c| c.id == id)).is_some_and(|i| i / 3 == self.page);
        let mut present = vec![true; count];
        (0..3).for_each(|k| present[chars + 1 + k] = k < visible);
        present[chars + 4] = self.page + 1 == pages && self.shown().len() < MAX_CHARACTERS;
        (present[chars + 5], present[chars + 6]) = (self.page > 0, self.page + 1 < pages);
        present[chars + 7] = selected_here;
        present[chars + 8] = self.character.is_some();
        if self.online_view() {
            // this PC's own tools stay with this PC's characters (Import, Edit look, Remove, Pokémon Storage), and an
            // online character plays on the official world only
            for i in [chars, chars + 7, chars + 8, chars + 9, toggle, world] {
                present[i] = false;
            }
        }
        let down = self.session.unreachable.is_some();
        (present[retry], present[p2p]) = (down && self.session.busy().is_none(), down && !self.p2p);
        let row_up = self.job.is_none() && self.error.is_none() && self.panel.is_none() && self.removing.is_none() && self.removed.is_none() && !self.storage.panel_up();
        (present[tabs], present[tabs + 1]) = (row_up && self.session.me().is_some(), row_up && self.session.me().is_some());
        if self.removing.is_some() || self.removed.is_some() {
            // the confirm (or its answer) stands in for the row: Keep (or OK) at the row's head, Remove after it
            (chars + 1..=chars + 9).for_each(|i| present[i] = false);
            present[chars + 1] = self.removing.is_some();
        } else if self.storage.asking() {
            // its note's three buttons stand in for the row
            (chars + 3..=chars + 9).for_each(|i| present[i] = false);
            (present[chars + 1], present[chars + 2]) = (true, true);
        }
        if self.menus.configure_open {
            // Configure has the keys while it is open
        } else if self.editing.is_none() && self.panel.is_none() {
            // the keyboard moves the same way the d-pad does
            let (prev, next, enter, esc) = ui.input(|i| {
                (
                    i.key_pressed(Key::ArrowUp) || i.key_pressed(Key::ArrowLeft),
                    i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::ArrowRight),
                    i.key_pressed(Key::Enter),
                    i.key_pressed(Key::Escape),
                )
            });
            self.nav.prev |= prev;
            self.nav.next |= next;
            self.nav.select |= enter;
            self.nav.back |= esc;
        } else if ui.input(|i| i.key_pressed(Key::Escape)) {
            self.nav.back = true;
        }
        if self.menus.configure_open {
            self.nav = Nav::default();
        } else if self.panel.is_some() {
            // the account panel's fields have the keys; B or Esc closes it
            self.nav = Nav { back: self.nav.back, ..Nav::default() };
        } else if self.creator.is_some() || self.creator_request.is_some() {
            // the creator reads the keys and the pad itself
            self.nav = Nav { pad: self.nav.pad, ..Nav::default() };
        }
        // the next or previous item that is on screen
        let step = |from: usize, forward: bool| {
            let mut f = from;
            loop {
                f = if forward { (f + 1) % count } else { (f + count - 1) % count };
                if present[f] {
                    return f;
                }
            }
        };
        if self.nav.next {
            (self.focus, self.show_focus) = (step(self.focus, true), true);
        }
        if self.nav.prev {
            (self.focus, self.show_focus) = (step(self.focus, false), true);
        }
        if self.nav.back && self.editing.take().is_none() && !self.close_panel() {
            (self.error, self.removing, self.removed) = (None, None, None);
        }
        let ids: Vec<String> = self.library.games.iter().map(|g| g.build_id.clone()).collect();
        for id in &ids {
            if matches!(self.state(id), BuildState::Ready | BuildState::Outdated) {
                self.ensure_title(ui.ctx(), id);
            }
        }

        // header: the logo, and Quit as a plain icon
        if let Some(logo) = &self.logo {
            // the RE:DEX mark straight on the backdrop (256 x 183, background keyed out), then the wordmark with the colon in red
            let r = Rect::from_min_size(pos2(32.0, 32.0), vec2(48.0 * 256.0 / 183.0, 48.0));
            ui.painter().image(logo.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            let font = FontId::proportional(32.0);
            let mut x = 112.0;
            for (part, color) in [("RE", TITLE), (":", Color32::from_rgb(0xf0, 0x3c, 0x56)), ("DEX", TITLE)] {
                let g = ui.painter().layout_no_wrap(part.to_owned(), font.clone(), color);
                let w = g.size().x;
                // painted twice, offset a hair, for a bold weight (the font has only one)
                for dx in [0.0, 0.7, 1.4] {
                    ui.painter().galley(pos2(x + dx, 56.0 - g.size().y / 2.0), g.clone(), color);
                }
                x += w + 2.0;
            }
        }
        let quit = Rect::from_min_size(pos2(w - 32.0 - 40.0, 40.0), vec2(40.0, 40.0));
        let resp = ui.interact(quit, ui.id().with("quit"), Sense::click());
        let hover = glass.hover_of(ui, &resp);
        ui.painter().circle_filled(quit.center(), 20.0, Color32::from_white_alpha((28.0 * hover) as u8));
        glass::draw_icon(ui.painter(), Icon::Close, quit.center(), 9.0 + 1.0 * hover);
        if resp.clicked() {
            self.quit = true;
        }
        self.account_header(ui, glass, w, acct);

        if let Some(editing) = self.creator_request.take() {
            // its preview child is the only one: the cards wait until it closes, the background states give way
            (self.cards, self.warm) = (None, None);
            let mut c = Creator::new(&self.store, editing);
            // decided now, not when it closes (a sign-in may land meanwhile): only a NEW character made on the Online
            // tab goes to the account; an edit never moves a character between the lists
            c.online = editing.is_none() && self.online_view();
            self.creator = Some(c);
        }
        if let Some(screen) = &mut self.mods {
            // the Mods screen takes the moves (up and down its list), A and B
            let nav = mods::ModsNav { up: self.nav.prev, down: self.nav.next, select: self.nav.select, back: self.nav.back };
            self.nav = Nav::default();
            if screen.ui(ui, glass, nav, &mut self.installer) == mods::ModsAction::Close {
                self.mods = None;
            }
            return;
        }
        if let Some(c) = &mut self.creator {
            c.pad = self.nav.pad;
            let action = c.ui(ui, glass, &self.data, &self.library);
            self.creator_done(action);
            if self.creator.is_none() {
                self.check_cards = true;
            }
            return;
        }

        // games
        let (card_w, card_h, gap) = (280.0, 160.0, 24.0);
        let top = 148.0;
        glass.title(ui.painter(), pos2(32.0, top - 28.0), "Games", 14.0);
        let busy = self.job.as_ref().and_then(|r| r.build_id.clone());
        for (i, id) in ids.iter().enumerate() {
            let rect = Rect::from_min_size(pos2(32.0 + i as f32 * (card_w + gap), top), vec2(card_w, card_h));
            let state = self.state(id);
            let resp = ui.interact(rect, ui.id().with(("game", i)), Sense::click());
            let hover = glass.hover_of(ui, &resp);
            let tex = self.pictures.get(id).and_then(|t| t.as_ref()).map(|t| t.id());
            let wash = if id.starts_with('y') { Color32::from_rgb(0xd0, 0x30, 0x30) } else { Color32::from_rgb(0x2f, 0x5f, 0xd0) };
            glass.card(ui.painter(), rect, 24.0, i == self.selected, hover, tex, wash);
            glass.title(ui.painter(), rect.min + vec2(24.0, card_h - 76.0), &game_name(id), 16.0);
            let (status, dot) = match (state, busy.as_deref() == Some(id.as_str())) {
                (_, true) => ("Building now", AMBER),
                (BuildState::Ready, _) => ("Ready to play", GREEN),
                (BuildState::Outdated, _) => ("Update available", AMBER),
                (BuildState::Partial, _) => ("Build paused", AMBER),
                (BuildState::Missing, _) => ("Not built yet", BODY),
            };
            ui.painter().circle_filled(rect.min + vec2(28.0, card_h - 40.0), 4.0, dot);
            text(ui, rect.min + vec2(40.0, card_h - 40.0), Align2::LEFT_CENTER, status, 14.0, BODY);
            let can_build = self.tools.is_ok() && self.job.is_none() && state != BuildState::Ready;
            if can_build {
                let b = Rect::from_min_size(pos2(rect.max.x - 24.0 - 80.0, rect.max.y - 24.0 - 32.0), vec2(80.0, 32.0));
                if self.flat_pill(ui, glass, b, i, label(&self.data, id, self.stamp.as_deref())) {
                    self.selected = i;
                    let dump = self.library.games[i].dump.clone();
                    self.start_build(dump, Some(id.clone()), (Self::cores() / 2).max(1));
                }
            }
            if self.item(ui, &resp, i, 24.0) {
                self.selected = i;
            }
        }
        // + Add your game
        let rect = Rect::from_min_size(pos2(32.0 + n as f32 * (card_w + gap), top), vec2(card_w, card_h));
        let enabled = self.tools.is_ok() && self.job.is_none();
        let resp = ui.interact(rect, ui.id().with("add"), if enabled { Sense::click() } else { Sense::hover() });
        let hover = if enabled { glass.hover_of(ui, &resp) } else { 0.0 };
        glass.dashed_card(ui.painter(), rect, 24.0, hover, if enabled { 1.0 } else { 0.6 });
        let tone = if enabled { TITLE } else { dim(BODY, 0.7) };
        glass::draw_icon(ui.painter(), Icon::Plus, rect.center() - vec2(0.0, 30.0), 14.0);
        text(ui, rect.center() + vec2(0.0, 4.0), Align2::CENTER_CENTER, "Add your game", 16.0, tone);
        let hint = match (&self.tools, n) {
            (Err(_), _) => "Building games needs the full RE:DEX download",
            (_, 0) => "Add your Pokémon X or Y dump to get started.",
            _ => "A decrypted .cci or .3ds dump",
        };
        let hint_rect = Rect::from_center_size(rect.center() + vec2(0.0, 46.0), vec2(card_w - 48.0, 36.0));
        at(ui, hint_rect, Layout::top_down(Align::Center), |ui| {
            ui.label(egui::RichText::new(hint).size(12.0).color(BODY));
        });
        if self.item(ui, &resp, add, 24.0) && enabled {
            self.add_game();
        }

        // characters, or the build's progress while one runs
        let low = top + card_h + 32.0;
        if let Some(run) = &self.job {
            let heading = run.build_id.as_deref().map_or("Adding your game".to_owned(), |id| format!("Building {}", game_name(id)));
            glass.title(ui.painter(), pos2(32.0, low), &heading, 14.0);
            let (stage, done, total, jobs) = (run.stage, run.done, run.total, run.jobs);
            let (mut cancel, mut faster) = (false, false);
            at(ui, Rect::from_min_size(pos2(32.0, low + 32.0), vec2(560.0, 120.0)), Layout::top_down(Align::Min), |ui| {
                let shown = if total > 0 { format!("{stage}  ({done} of {total})") } else { stage.to_owned() };
                ui.label(egui::RichText::new(shown).size(16.0).color(TITLE));
                ui.add_space(8.0);
                // no total yet (checking, unpacking, translating): a moving bar says it is working
                glass.progress(ui, (total > 0).then(|| done as f32 / total as f32), true);
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    cancel = glass.pill_button(ui, "Cancel").clicked();
                    if jobs < Self::cores() {
                        faster = glass.pill_button(ui, "Build faster").clicked();
                    }
                });
            });
            if cancel {
                self.job = None; // dropping the job cancels it and kills its tools
            } else if faster {
                // ninja resumes where it was and the dump is not hashed again, so a restart with every core loses
                // almost nothing; the old job is dropped (cancelled) first
                if let Some(old) = self.job.take() {
                    let Running { dump, build_id, job, .. } = old;
                    drop(job);
                    self.start_build(dump, build_id, Self::cores());
                }
            }
        } else if let Some(e) = self.error.clone() {
            glass.title(ui.painter(), pos2(32.0, low), "That didn't work", 14.0);
            let mut dismiss = false;
            at(ui, Rect::from_min_size(pos2(32.0, low + 32.0), vec2(640.0, 120.0)), Layout::top_down(Align::Min), |ui| {
                ui.label(egui::RichText::new(e).size(14.0).color(RED));
                ui.add_space(16.0);
                dismiss = glass.pill_button(ui, "Dismiss").clicked();
            });
            if dismiss {
                self.error = None;
            }
        } else if self.panel.is_some() {
            self.account_panel(ui, glass, low);
        } else if self.removing.is_some() || self.removed.is_some() {
            self.remove_panel(ui, glass, low, chars);
        } else if self.storage.panel_up() {
            self.storage_panel(ui, glass, low, chars);
        } else {
            glass.title(ui.painter(), pos2(32.0, low), "Characters", 14.0);
            if self.session.me().is_some() {
                self.view_tabs(ui, glass, w, low, tabs);
            }
            self.characters(ui, glass, low, chars);
        }

        // bottom bar: three 40 px pills, and the one big Play
        let cy = h - 32.0 - 48.0;
        let mut commit = false;
        let mut editing = self.editing.take();
        let official = self.online_view();
        let bar = Rect::from_min_max(pos2(32.0, cy - 20.0), pos2(w - 32.0 - 96.0 - 200.0, cy + 20.0));
        at(ui, bar, Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            if official {
                // online characters play on the official world only
                glass.pill(ui, "Official world", Some(304.0), false);
            } else {
                // Steam's login belongs to the running game (it needs one to ask Steam): online just means "sign in when you play"
                let r = glass.pill(ui, if self.offline { "Offline" } else { "Online" }, Some(144.0), true);
                if self.item(ui, &r, toggle, 20.0) {
                    self.offline = !self.offline;
                }
            }
            if let Some(buf) = &mut editing {
                let edit = ui.add_sized([232.0, 40.0], egui::TextEdit::singleline(buf).hint_text("host:port or steam:<id>").font(FontId::proportional(16.0)));
                commit |= edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                let r = glass.pill(ui, "Save", Some(80.0), true);
                commit |= self.item(ui, &r, world, 20.0);
                if let Some(e) = self.edit_error {
                    ui.label(egui::RichText::new(e).size(12.0).color(RED));
                }
            } else if !official {
                let addr = &self.settings.world_address;
                let shown = match (addr.is_empty(), self.settings.world_name.is_empty()) {
                    (true, _) => "World: none".to_owned(),
                    (false, true) => format!("World: {}", short(addr, 12)),
                    (false, false) => format!("World: {}", short(&self.settings.world_name, 12)),
                };
                let r = glass.pill(ui, &shown, Some(144.0), true);
                if self.item(ui, &r, world, 20.0) {
                    editing = Some(self.settings.world_address.clone());
                    self.edit_error = None;
                }
            }
            if editing.is_none() {
                // the game's own Configure window, the same settings the game uses
                let r = glass.pill(ui, "Settings", Some(144.0), true);
                if self.item(ui, &r, settings, 20.0) {
                    self.menus.configure_open = true;
                }
                let r = glass.pill(ui, "Mods", Some(112.0), true);
                if self.item(ui, &r, mods_item, 20.0) {
                    self.mods = Some(mods::Mods::new(self.data.clone()));
                }
            }
        });
        self.editing = editing;
        if commit {
            let value = self.editing.clone().unwrap_or_default().trim().to_owned();
            if value.is_empty() || valid_address(&value) {
                self.settings.world_address = value.clone();
                self.settings.save();
                self.offline = value.is_empty();
                self.editing = None;
            } else {
                self.edit_error = Some("That address doesn't look right. Use host:port or steam:<id>.");
            }
        }

        // Play: the only primary action
        // never beside Pokémon Storage: the game and PKVault would both write the saves
        let blocked = self.storage.blocks_play().or_else(|| self.installer.busy().then_some("Wait for the mod install"));
        let playable = self.playable().filter(|_| blocked.is_none());
        let ready = self.ready();
        let selected = self.library.games.get(self.selected).map(|g| g.build_id.clone());
        let selected_name = selected.as_deref().map(game_name);
        let outdated = selected.as_deref().is_some_and(|id| self.state(id) == BuildState::Outdated);
        let c = pos2(w - 32.0 - 48.0, cy);
        let who = self.character.and_then(|id| self.shown_get(id)).map(|c| c.name.clone());
        let caption = match (&playable, &selected_name) {
            (Some(_), Some(_)) => ("Play".to_owned(), TITLE),
            (None, Some(_)) if ready && self.online_view() => (self.online_blocker().to_owned(), BODY),
            (None, Some(_)) if ready => ("Make a character to play".to_owned(), BODY),
            (None, Some(name)) if outdated => (format!("Update {name} to play"), BODY),
            (None, Some(name)) => (format!("Build {name} to play"), BODY),
            _ => ("Add a game to play".to_owned(), BODY),
        };
        let caption = blocked.filter(|_| ready).map_or(caption, |why| (why.to_owned(), BODY));
        text(ui, pos2(c.x - 48.0 - 16.0, cy), Align2::RIGHT_CENTER, &caption.0, 16.0, caption.1);
        if let Some(name) = &selected_name {
            let building = self.job.as_ref().is_some_and(|r| r.build_id.is_some() && r.build_id == selected);
            let state = match (ready, building) {
                (true, _) => "ready",
                (_, true) => "building",
                _ if outdated => "needs an update",
                _ => "not built yet",
            };
            let net = if self.online_view() { "official world" } else if self.offline { "offline" } else { "online" };
            let line = match &who {
                Some(who) => format!("{name} · {who} · {state} · {net}"),
                None => format!("{name} · {state} · {net}"),
            };
            text(ui, pos2(w - 32.0, cy - 48.0 - 20.0), Align2::RIGHT_CENTER, &line, 13.0, CARD_BODY);
        }
        let resp = ui.put(Rect::from_center_size(c, vec2(96.0, 96.0)), |ui: &mut Ui| glass.primary_button(ui, Icon::Play, 96.0, playable.is_some()));
        let pressed = self.item(ui, &resp, play, 48.0);
        if let (true, Some(launch)) = (pressed, playable) {
            // the preview child and a state maker go before the game starts
            (self.cards, self.warm) = (None, None);
            self.launch = Some(launch);
            self.quit = true;
        }

        let booted = self.settings.clone();
        for action in self.menus.configure(ui.ctx(), &mut self.settings, &booted) {
            if matches!(action, Action::Save) {
                self.settings.save();
            }
        }
    }

    /// how many pages the character row has and how many characters this page shows
    fn row(&self) -> (usize, usize) {
        let n = self.shown().len();
        let pages = n.div_ceil(3).max(1);
        (pages, n.saturating_sub(self.page.min(pages - 1) * 3).min(3))
    }

    /// the focus ring of something drawn by hand; true when A is pressed on it
    fn focus_ring(&self, ui: &Ui, rect: Rect, item: usize) -> bool {
        if self.show_focus && self.focus == item {
            ui.painter().add(egui::Shape::mesh(glass::rim_mesh(rect, rect.height() / 2.0, 1.0)));
        }
        self.nav.select && self.focus == item
    }

    /// a character's card picture, loaded once for the look it was drawn in
    fn ensure_face(&mut self, ctx: &egui::Context, c: &characters::Character) {
        if self.faces.get(&c.id).is_some_and(|(look, _)| *look == c.look) {
            return;
        }
        let tex = std::fs::read(preview::card_path(&self.data, c)).ok().and_then(|b| load_card(ctx, &format!("card-{:016x}", c.id), &b));
        self.faces.insert(c.id, (c.look.clone(), tex));
    }

    /// the character row: Import a save at its head, cards, the New character slot, and arrows when there are more than
    /// three
    fn characters(&mut self, ui: &mut Ui, glass: &Glass, low: f32, base: usize) {
        let (pages, visible) = self.row();
        self.page = self.page.min(pages - 1);
        let ry = low + 28.0;
        let n = self.shown().len();
        let (online, signed_in) = (self.online_view(), self.session.me().is_some());

        // this PC's own tools, for this PC's characters only
        if !online {
            // the secondary action, beside the row's title, on the grid and 8 px above the cards
            let imp = Rect::from_min_size(pos2(152.0, low - 4.0), vec2(120.0, 24.0));
            let hit = self.flat_pill(ui, glass, imp, 100, "Import a save");
            if hit | self.focus_ring(ui, imp, base) {
                self.import_save();
                return;
            }
            // the selected character off the list (after a confirm), e.g. junk a hostile world sent
            if let Some(id) = self.character {
                let rm = Rect::from_min_size(pos2(280.0, low - 4.0), vec2(144.0, 24.0));
                if self.flat_pill(ui, glass, rm, 102, "Remove character") | self.focus_ring(ui, rm, base + 8) {
                    // Keep is the default
                    (self.removing, self.focus) = (Some(id), base);
                    return;
                }
            }
            // every character's boxes in PKVault (storage.rs), after Remove
            let x = if self.character.is_some() { 432.0 } else { 280.0 };
            self.storage_pill(ui, glass, Rect::from_min_size(pos2(x, low - 4.0), vec2(136.0, 24.0)), base);
            if self.storage.asking() {
                return;
            }
        }

        for k in 0..visible {
            let ch = self.shown()[self.page * 3 + k].clone();
            self.ensure_face(ui.ctx(), &ch);
            let rect = Rect::from_min_size(pos2(32.0 + k as f32 * 216.0, ry), vec2(200.0, 120.0));
            let resp = ui.interact(rect, ui.id().with(("char", k)), Sense::click());
            let hover = glass.hover_of(ui, &resp);
            let selected = self.character == Some(ch.id);
            let girl = Appearance::gender_of(&ch.look) == 1;
            let wash = if girl { Color32::from_rgb(0xd6, 0x4c, 0x8c) } else { Color32::from_rgb(0x3d, 0x6b, 0xff) };
            glass.card(ui.painter(), rect, 24.0, selected, hover, None, wash);
            // the figure, 1:1 on screen
            let fig = Rect::from_min_size(rect.min + vec2(16.0, 8.0), vec2(64.0, 104.0));
            match self.faces.get(&ch.id).and_then(|(_, t)| t.as_ref()) {
                Some(t) => glass.picture(ui.painter(), fig, 12.0, t.id(), Color32::WHITE, false),
                None => {
                    // frosted glass with a trainer's outline until the picture is drawn
                    ui.painter().rect_filled(fig, 12.0, Color32::from_white_alpha(14));
                    ui.painter().add(egui::Shape::mesh(glass::rim_mesh(fig, 12.0, 0.0)));
                    creator::silhouette(ui, fig.shrink2(vec2(14.0, 16.0)));
                }
            }
            glass.dot(ui.painter(), rect, 24.0);
            let x = rect.min.x + 96.0;
            bold(ui, pos2(x, rect.min.y + 24.0), &fit(ui, &ch.name, 16.0, 88.0), 16.0, TITLE);
            // "X ✓ · Y –": a tick where there is a save (the font has no tick), a dash where there is not
            let mut sx = x;
            for (i, (label, title)) in [("X", TITLE_X), ("Y", TITLE_Y)].into_iter().enumerate() {
                let cy = rect.min.y + 48.0;
                let g = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(13.0), CARD_BODY);
                ui.painter().galley(pos2(sx, cy - g.size().y / 2.0), g.clone(), CARD_BODY);
                sx += g.size().x + 6.0;
                if has_save(&self.data, ch.id, title) {
                    let tick = Stroke::new(1.6, GREEN);
                    ui.painter().line_segment([pos2(sx, cy), pos2(sx + 3.0, cy + 3.5)], tick);
                    ui.painter().line_segment([pos2(sx + 3.0, cy + 3.5), pos2(sx + 9.0, cy - 4.0)], tick);
                } else {
                    ui.painter().line_segment([pos2(sx + 1.0, cy), pos2(sx + 8.0, cy)], Stroke::new(1.4, CARD_BODY));
                }
                sx += 9.0 + 6.0;
                if i == 0 {
                    text(ui, pos2(sx, cy), Align2::LEFT_CENTER, "·", 13.0, CARD_BODY);
                    sx += 6.0 + 6.0;
                }
            }
            if signed_in {
                // which list it is on, said on the card too
                let (tag, color) = if online { ("Online", GREEN) } else { ("On this PC", CARD_BODY) };
                text(ui, pos2(x, rect.min.y + 68.0), Align2::LEFT_CENTER, tag, 12.0, color);
            }
            if selected && !online {
                let b = Rect::from_min_size(pos2(x, rect.max.y - 8.0 - 32.0), vec2(88.0, 32.0));
                if self.flat_pill(ui, glass, b, 101, "Edit look") | self.focus_ring(ui, b, base + 7) {
                    self.creator_request = Some(Some(ch.id));
                }
            }
            if self.item(ui, &resp, base + 1 + k, 24.0) {
                self.character = Some(ch.id);
            }
        }

        // New character, after the cards on the last page
        if self.page + 1 == pages {
            let rect = Rect::from_min_size(pos2(32.0 + visible as f32 * 216.0, ry), vec2(200.0, 120.0));
            let full = n >= MAX_CHARACTERS;
            let resp = ui.interact(rect, ui.id().with("new"), if full { Sense::hover() } else { Sense::click() });
            let hover = if full { 0.0 } else { glass.hover_of(ui, &resp) };
            glass.dashed_card(ui.painter(), rect, 24.0, hover, if full { 0.6 } else { 1.0 });
            let c = rect.center();
            if full {
                text(ui, c - vec2(0.0, 12.0), Align2::CENTER_CENTER, "All 8 characters made", 16.0, TITLE);
                text(ui, c + vec2(0.0, 16.0), Align2::CENTER_CENTER, "That's the most you can have", 13.0, CARD_BODY);
            } else {
                glass::draw_icon(ui.painter(), Icon::Plus, c - vec2(0.0, 28.0), 12.0);
                text(ui, c + vec2(0.0, 4.0), Align2::CENTER_CENTER, "New character", 16.0, TITLE);
                text(ui, c + vec2(0.0, 28.0), Align2::CENTER_CENTER, "Pick a look and a name", 13.0, CARD_BODY);
                if self.item(ui, &resp, base + 4, 24.0) {
                    self.creator_request = Some(None);
                }
            }
        }

        // the page arrows
        if pages > 1 {
            for (i, (icon, x, go)) in [(Icon::Prev, 936.0, self.page > 0), (Icon::Next, 976.0, self.page + 1 < pages)].into_iter().enumerate() {
                let r = Rect::from_min_size(pos2(x, ry + 44.0), vec2(32.0, 32.0));
                let resp = ui.put(r, |ui: &mut Ui| glass.circle_button(ui, icon, 32.0));
                if !go {
                    ui.painter().circle_filled(r.center(), 17.0, Color32::from_rgba_unmultiplied(0x10, 0x16, 0x28, 166));
                }
                if self.item(ui, &resp, base + 5 + i, 16.0) && go {
                    self.page = if i == 0 { self.page - 1 } else { self.page + 1 };
                }
            }
        }
    }

    /// top right, left of Quit: who is signed in (or Sign in), which opens the account panel; under the header, a calm
    /// line when the official server does not answer, with Try again and Play with friends (P2P)
    fn account_header(&mut self, ui: &mut Ui, glass: &Glass, w: f32, acct: usize) {
        let label = match (self.session.busy(), self.session.me()) {
            (Some(b), _) => b.to_owned(),
            (None, Some(me)) => format!("Signed in: {}", short(&me.username, 16)),
            (None, None) => "Sign in".to_owned(),
        };
        let r = Rect::from_min_size(pos2(w - 32.0 - 40.0 - 16.0 - 248.0, 40.0), vec2(248.0, 40.0));
        let resp = at(ui, r, Layout::right_to_left(Align::Center), |ui| glass.pill(ui, &label, Some(248.0), true));
        if self.item(ui, &resp, acct, 20.0) {
            match (self.panel, self.session.me()) {
                (Some(_), _) => drop(self.close_panel()),
                (None, Some(_)) => self.panel = Some(Panel::Account),
                (None, None) => self.panel = Some(Panel::SignIn),
            }
        }
        if self.session.unreachable.is_none() {
            return;
        }
        let y = 100.0;
        ui.painter().circle_filled(pos2(36.0, y), 4.0, AMBER);
        let said = if self.session.servers.is_official() { "Official server unreachable".to_owned() } else { format!("Server {} unreachable", short(self.session.servers.host(), 28)) };
        let shown = ui.painter().layout_no_wrap(said.clone(), FontId::proportional(14.0), TITLE).size().x;
        text(ui, pos2(48.0, y), Align2::LEFT_CENTER, &said, 14.0, TITLE);
        // Try again is off while a call is out (the next answer says it all)
        let mut x = (48.0 + shown + 16.0_f32).max(256.0);
        if self.session.busy().is_none() {
            let r = Rect::from_min_size(pos2(x, y - 12.0), vec2(96.0, 24.0));
            if self.flat_pill(ui, glass, r, 120, "Try again") | self.focus_ring(ui, r, acct + 1) {
                self.session.check();
            }
            x += 112.0;
        }
        if self.p2p {
            text(ui, pos2(x, y), Align2::LEFT_CENTER, "Playing with friends (P2P): characters on this PC, worlds over Steam", 13.0, CARD_BODY);
            return;
        }
        let r = Rect::from_min_size(pos2(x, y - 12.0), vec2(184.0, 24.0));
        if self.flat_pill(ui, glass, r, 121, "Play with friends (P2P)") | self.focus_ring(ui, r, acct + 2) {
            // the existing Steam worlds, with this PC's characters only: nothing played there reaches the account
            self.close_panel();
            (self.p2p, self.online_view, self.offline) = (true, false, false);
            (self.page, self.character) = (0, self.store.characters.first().map(|c| c.id));
        }
    }

    /// Online or On this PC: which characters the row shows, while signed in
    fn view_tabs(&mut self, ui: &mut Ui, glass: &Glass, w: f32, low: f32, first: usize) {
        let tabs = [(format!("Online ({})", self.session.characters.len()), true), (format!("On this PC ({})", self.store.characters.len()), false)];
        for (i, (label, online)) in tabs.into_iter().enumerate() {
            let r = Rect::from_min_size(pos2(w - 32.0 - 2.0 * 144.0 - 8.0 + i as f32 * 152.0, low - 4.0), vec2(144.0, 24.0));
            if self.online_view == online {
                ui.painter().add(egui::Shape::mesh(glass::rim_mesh(r, 12.0, 1.0)));
            }
            if (self.flat_pill(ui, glass, r, 110 + i, &label) | self.focus_ring(ui, r, first + i)) && self.online_view != online {
                self.online_view = online;
                (self.page, self.character) = (0, self.shown().first().map(|c| c.id));
            }
        }
    }

    /// the account panel in place of the character row: Sign in, Create account, Forgot password, or (signed in) who
    /// it is and Sign out; under the buttons what the service said, or what is out
    fn account_panel(&mut self, ui: &mut Ui, glass: &Glass, low: f32) {
        let Some(panel) = self.panel else { return };
        let busy = self.session.busy();
        let title = match panel {
            Panel::SignIn => "Sign in",
            Panel::Register => "Create an account",
            Panel::Forgot => "Forgot your password",
            Panel::Account => "Your account",
        };
        // a server that is not the official one is named, so a password never goes somewhere unseen
        let title = match self.session.servers.is_official() {
            true => title.to_owned(),
            false => format!("{title} on {} (not the official server)", short(self.session.servers.host(), 40)),
        };
        glass.title(ui.painter(), pos2(32.0, low), &title, 14.0);
        let (mut next, mut submit) = (Some(panel), false);
        // the test harness's filled form goes once nothing is out (test-hooks)
        if self.hook_submit && busy.is_none() {
            (submit, self.hook_submit) = (true, false);
        }
        let (me, note) = (self.session.me().cloned(), self.session.note.clone());
        let form = &mut self.form;
        at(ui, Rect::from_min_size(pos2(32.0, low + 28.0), vec2(760.0, 200.0)), Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 10.0);
            // a field; true when Enter is pressed in it
            let field = |ui: &mut Ui, buf: &mut String, hint: &str, secret: bool| {
                let r = ui.add_sized([280.0, 36.0], egui::TextEdit::singleline(buf).hint_text(hint).password(secret).font(FontId::proportional(16.0)).vertical_align(Align::Center).margin(vec2(10.0, 4.0)));
                r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))
            };
            match panel {
                Panel::SignIn => {
                    ui.horizontal(|ui| {
                        submit |= field(ui, &mut form.login, "Email or username", false);
                        submit |= field(ui, &mut form.password, "Password", true);
                    });
                }
                Panel::Register => {
                    ui.horizontal(|ui| {
                        field(ui, &mut form.email, "Email address", false);
                        field(ui, &mut form.username, "Username (3 to 16 characters)", false);
                    });
                    ui.horizontal(|ui| {
                        field(ui, &mut form.password, "Password (10 characters or more)", true);
                        submit |= field(ui, &mut form.trainer, "Trainer name", false);
                    });
                    ui.label(egui::RichText::new("A confirmation email is required: you can play online once you open its link.").size(13.0).color(CARD_BODY));
                }
                Panel::Forgot => {
                    submit |= field(ui, &mut form.email, "Email address", false);
                }
                Panel::Account => {
                    let line = me.as_ref().map_or_else(String::new, |m| format!("Signed in as {}{}", m.username, if m.email_verified { "" } else { " (email not confirmed yet)" }));
                    ui.label(egui::RichText::new(line).size(16.0).color(TITLE));
                }
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let main = match panel {
                    Panel::SignIn => "Sign in",
                    Panel::Register => "Create account",
                    Panel::Forgot => "Send reset email",
                    Panel::Account => "Sign out",
                };
                submit |= glass.pill(ui, main, None, busy.is_none()).clicked();
                let others: &[(&str, Option<Panel>)] = match panel {
                    Panel::SignIn => &[("Create account", Some(Panel::Register)), ("Forgot password", Some(Panel::Forgot)), ("Close", None)],
                    Panel::Register | Panel::Forgot => &[("Back to sign in", Some(Panel::SignIn)), ("Close", None)],
                    Panel::Account => &[("Close", None)],
                };
                for (label, to) in others {
                    if glass.pill(ui, label, None, true).clicked() {
                        next = *to;
                    }
                }
            });
            match (busy, &note) {
                (Some(b), _) => ui.label(egui::RichText::new(b).size(14.0).color(TITLE)),
                (None, Some(Note::Error(e))) => ui.label(egui::RichText::new(e).size(14.0).color(RED)),
                (None, Some(Note::Info(i))) => ui.label(egui::RichText::new(i).size(14.0).color(GREEN)),
                (None, None) => ui.label(""),
            };
        });
        if submit && busy.is_none() {
            let f = &mut self.form;
            match panel {
                Panel::SignIn => self.session.sign_in(f.login.clone(), std::mem::take(&mut f.password)),
                Panel::Register => self.session.register(f.email.clone(), f.username.clone(), std::mem::take(&mut f.password), f.trainer.clone()),
                Panel::Forgot => self.session.forgot(f.email.clone()),
                Panel::Account => {
                    self.session.sign_out();
                    next = None;
                }
            }
        }
        if next != Some(panel) {
            self.close_panel();
        }
        self.panel = next;
    }

    /// "Remove <name>?" with Keep and Remove in place of the character row, then where the saves stayed
    fn remove_panel(&mut self, ui: &mut Ui, glass: &Glass, low: f32, base: usize) {
        let confirming = self.removing.is_some();
        let (title, body) = match (self.removing, &self.removed) {
            (Some(id), _) => {
                let name = self.store.get(id).map_or_else(String::new, |c| c.name.clone());
                let folder = self.data.join("characters").join(format!("{id:016x}"));
                (format!("Remove {name}?"), format!("{name} comes off your characters. Their saves stay on this PC, in {}.", folder.display()))
            }
            (None, Some(said)) => ("Character removed".to_owned(), said.clone()),
            (None, None) => return,
        };
        glass.title(ui.painter(), pos2(32.0, low), &title, 14.0);
        let (mut keep, mut remove) = (false, false);
        at(ui, Rect::from_min_size(pos2(32.0, low + 32.0), vec2(640.0, 120.0)), Layout::top_down(Align::Min), |ui| {
            ui.label(egui::RichText::new(body).size(14.0).color(TITLE));
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                let r = glass.pill(ui, if confirming { "Keep" } else { "OK" }, Some(120.0), true);
                keep = self.item(ui, &r, base, 20.0);
                if confirming {
                    let r = glass.pill(ui, "Remove", Some(120.0), true);
                    remove = self.item(ui, &r, base + 1, 20.0);
                }
            });
        });
        if keep {
            (self.removing, self.removed) = (None, None);
        } else if let (true, Some(id)) = (remove, self.removing.take()) {
            // the list is read again for the save: another RE:DEX may have changed it
            let Some(mut store) = Store::try_load(&self.data) else {
                self.error = Some("We couldn't open your characters just now. Close anything else using RE:DEX, then try again.".to_owned());
                return;
            };
            let name = store.get(id).map_or_else(String::new, |c| c.name.clone());
            store.remove(id);
            match store.save(&self.data) {
                Ok(()) => {
                    let folder = self.data.join("characters").join(format!("{id:016x}"));
                    log::info!("characters: removed {id:016x} {name:?}; its saves stay in {}", folder.display());
                    self.removed = Some(format!("{name} is off your characters. Their saves are still on this PC, in {}.", folder.display()));
                    self.faces.remove(&id);
                    self.store = store;
                    (self.character, self.page, self.focus) = (self.store.characters.first().map(|c| c.id), 0, base);
                }
                Err(e) => {
                    log::warn!("characters: could not save without {id:016x}: {e}");
                    self.error = Some("We couldn't save your characters. Check that the RE:DEX data folder can be written to.".to_owned());
                }
            }
        }
    }

    /// Import a save: the picked `main` becomes a new character (copied, the file stays where it is)
    fn import_save(&mut self) {
        let Some(main) = pick_save() else { return };
        self.error = None;
        // the list is read again for the save: another RE:DEX may have changed it
        let Some(mut store) = Store::try_load(&self.data) else {
            self.error = Some("We couldn't open your characters just now. Close anything else using RE:DEX, then try again.".to_owned());
            return;
        };
        if store.characters.len() >= MAX_CHARACTERS {
            self.error = Some("You already have 8 characters, the most there can be.".to_owned());
            return;
        }
        match store.import(&self.data, &main, now_ms()).and_then(|id| store.save(&self.data).map(|()| id)) {
            Ok(id) => {
                self.page = store.characters.iter().position(|c| c.id == id).unwrap_or(0) / 3;
                (self.character, self.store, self.check_cards) = (Some(id), store, true);
            }
            Err(e) => {
                log::warn!("import a save {}: {e}", main.display());
                self.error = Some(e);
            }
        }
    }

    /// draws missing card pictures once a game is ready, a preview child at a time, never while a build runs
    fn pump_cards(&mut self) {
        // one preview child or state maker at a time: the creator's, then the background states, then the cards
        if self.creator.is_some() {
            return;
        }
        if let Some(warm) = &mut self.warm {
            let going = self.job.is_none() && preview_game(&self.library, &self.data).cloned().is_some_and(|g| warm.pump(&self.exe, &g, &self.data));
            if !going {
                self.warm = None;
            }
            return;
        }
        if self.check_cards && self.job.is_none() {
            let all: Vec<characters::Character> = self.store.characters.iter().chain(&self.session.characters).cloned().collect();
            let stale = stale_cards(&self.data, &all);
            if stale.is_empty() {
                self.check_cards = false;
            } else if preview_game(&self.library, &self.data).is_some() {
                (self.cards, self.check_cards) = (Some(Cards::new(stale)), false);
            }
        }
        let Some(cards) = &mut self.cards else { return };
        if self.job.is_none() {
            if let Some(game) = preview_game(&self.library, &self.data).cloned() {
                if let Some(id) = cards.pump(&self.exe, &game, &self.data) {
                    self.faces.remove(&id);
                }
            }
        }
        if !cards.pending() {
            self.cards = None;
        }
    }

    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        self.pump();
        self.installer.pump();
        self.pump_cards();
        self.pump_storage();
        self.pump_session();
        use gilrs::Button;
        // the pad is read always (gilrs keeps its own state) but only heard while this window has the focus: a game
        // played meanwhile with the same controller must not change anything here
        let heard = self.window.as_ref().is_some_and(Window::has_focus);
        for b in self.gamepads.poll().into_iter().filter(|_| heard) {
            let p = &mut self.nav.pad;
            match b {
                Button::DPadUp => (self.nav.prev, p.up) = (true, true),
                Button::DPadLeft => (self.nav.prev, p.left) = (true, true),
                Button::DPadDown => (self.nav.next, p.down) = (true, true),
                Button::DPadRight => (self.nav.next, p.right) = (true, true),
                Button::South => (self.nav.select, p.confirm) = (true, true),
                Button::East => (self.nav.back, p.cancel) = (true, true),
                Button::LeftTrigger => p.turn_left = true,
                Button::RightTrigger => p.turn_right = true,
                _ => {}
            }
        }
        let (Some(mut gui), Some(mut glass), Some(window)) = (self.gui.take(), self.glass.take(), self.window.take()) else { return };
        let now = Instant::now();
        glass.request_size(logical(&window), now);
        glass.settle(now);
        glass.animate(self.started.elapsed().as_secs_f64());
        let overlay: Overlay = gui.frame(&window, |ui| self.ui(ui, &glass));
        self.nav = Nav::default();
        if let Some(backend) = &mut self.backend {
            let blank = || ScreenImage { width: 0, height: 0, pixels: &[], gpu: None };
            match backend.present(blank(), blank(), blank(), &overlay) {
                Ok(()) | Err(PresentError::OutOfDate) => {}
                Err(e) => self.failure = Some(format!("presentation failed: {e}")),
            }
        }
        (self.gui, self.glass, self.window) = (Some(gui), Some(glass), Some(window));
        if self.quit || self.failure.is_some() {
            event_loop.exit();
        }
    }
}

impl ApplicationHandler for Start {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("RE:DEX")
            .with_inner_size(winit::dpi::LogicalSize::new(1040.0, 640.0))
            .with_window_icon(crate::icon(32));
        // the test harness's captures: far off every screen, never activated, not on the taskbar (XR_TEST_OFFSCREEN)
        #[cfg(all(feature = "test-hooks", windows))]
        let attributes = match std::env::var_os("XR_TEST_OFFSCREEN") {
            Some(_) => {
                use winit::platform::windows::WindowAttributesExtWindows;
                attributes.with_position(winit::dpi::PhysicalPosition::new(-20000, -20000)).with_active(false).with_skip_taskbar(true)
            }
            None => attributes,
        };
        let renderer = match self.settings.renderer {
            Renderer::Vulkan => RendererKind::Vulkan,
            Renderer::OpenGl => RendererKind::OpenGl,
        };
        let made = Backend::create(event_loop, attributes.clone(), renderer).or_else(|e| {
            if renderer != RendererKind::Vulkan {
                return Err(e);
            }
            // a machine w/o a working Vulkan driver still gets a window
            Backend::create(event_loop, attributes, RendererKind::OpenGl)
        });
        let (window, backend) = match made {
            Ok(m) => m,
            Err(e) => {
                self.failure = Some(format!("could not open the start screen: {e}"));
                return event_loop.exit();
            }
        };
        let gui = Gui::new(&window);
        crate::menus::style(&gui.ctx, true, self.menus.motion);
        self.glass = Some(Glass::new(&gui.ctx, logical(&window)));
        self.logo = load_png(&gui.ctx, "logo", include_bytes!("../../../../assets/start-logo.png"), 256, 0, 1.0);
        self.gui = Some(gui);
        self.backend = Some(backend);
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let (Some(gui), Some(window)) = (&mut self.gui, &self.window) {
            gui.event(window, &event);
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(backend) = &mut self.backend {
                    backend.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => self.frame(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

/// the system's open dialog for a cartridge dump
#[cfg(windows)]
fn pick_dump() -> Option<PathBuf> {
    pick_file("3DS cartridge dumps\0*.cci;*.3ds\0All files\0*.*\0\0")
}

/// the system's open dialog for an X or Y save file, named `main`
#[cfg(windows)]
fn pick_save() -> Option<PathBuf> {
    pick_file("Pokémon X or Y save (main)\0main\0All files\0*.*\0\0")
}

#[cfg(windows)]
fn pick_file(filter: &str) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW};
    let filter: Vec<u16> = filter.encode_utf16().collect();
    let mut file = vec![0u16; 2048];
    // safe: an all-zero OPENFILENAMEW is the documented blank, and the buffers outlive the call
    let mut o: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    o.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    o.lpstrFilter = filter.as_ptr();
    o.lpstrFile = file.as_mut_ptr();
    o.nMaxFile = file.len() as u32;
    o.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if unsafe { GetOpenFileNameW(&mut o) } == 0 {
        return None;
    }
    let end = file.iter().position(|&c| c == 0)?;
    Some(PathBuf::from(String::from_utf16_lossy(&file[..end])))
}

#[cfg(not(windows))]
fn pick_dump() -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
fn pick_save() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playable_needs_a_ready_game_and_a_character() {
        let data = Path::new("data");
        let game = GameEntry { build_id: "x-abc".into(), dump: "x.cci".into() };
        assert_eq!(launch_for(data, Some(&game), true, None, true, ""), None, "no character");
        assert_eq!(launch_for(data, Some(&game), false, Some(7), true, ""), None, "game not built");
        assert_eq!(launch_for(data, None, true, Some(7), true, ""), None, "no game");
        let l = launch_for(data, Some(&game), true, Some(7), false, "host:1").unwrap();
        assert_eq!((l.character, l.world.as_deref(), l.offline), (Some(7), Some("host:1"), false));
        assert_eq!(l.build_dir, data.join("build").join("x-abc"));
        // offline plays offline even when a world is saved
        assert_eq!(launch_for(data, Some(&game), true, Some(7), true, "host:1").unwrap().world, None);
    }

    /// the focus list (Mods and Pokémon Storage both in it) is built and stepped through all the way round, w/ and w/o
    /// a character chosen and while Storage's note is up, without an index past its end
    #[test]
    fn the_focus_list_holds_every_item() {
        let data = std::env::temp_dir().join(format!("redex-start-focus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(data.join("build/x-usa-v0.2.0")).unwrap();
        std::fs::write(data.join("library.json"), r#"{"games":[{"build_id":"x-usa-v0.2.0","dump":"x.cci"}]}"#).unwrap();
        let mut store = Store::load(&data);
        let look = xr_net::x::look::Look { gender: 1, skin: 1, hair_style: 2, hair_colour: 0, eyes: 0 };
        for name in ["Ana", "Bea", "Cy", "Dee"] {
            store.create(name, look.appearance().unwrap(), 1).unwrap();
        }
        store.save(&data).unwrap();
        let mut app = Start::new(data.clone(), Err("a test".into()), Settings::default());
        let ctx = egui::Context::default();
        let glass = Glass::new(&ctx, [1040, 640]);
        let frame = |app: &mut Start, next: bool| {
            app.nav.next = next;
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1040.0, 640.0))), ..Default::default() };
            // no painter takes the textures here: epaint asserts (debug) that a delta is not dropped unhandled
            ctx.run_ui(input, |ui| app.ui(ui, &glass)).textures_delta.clear();
            app.nav = Nav::default();
        };
        for (character, asking) in [(app.character, false), (None, false), (app.character, true)] {
            app.character = character;
            if asking {
                app.storage = storage::Storage::Asking;
            }
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..24 {
                frame(&mut app, true);
                seen.insert(app.focus);
            }
            // one game: Pokémon Storage (chars + 9 = 11), online or offline, World, Settings, Mods and Play (16) are reached
            // (while Storage's note is up its buttons stand in for the row, the pill among them)
            let first = if asking { 12 } else { 11 };
            assert!((first..=16).all(|i| seen.contains(&i)), "{seen:?}");
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    fn frame_with(ctx: &egui::Context, glass: &Glass, app: &mut Start, nav: Nav) {
        app.nav = nav;
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1040.0, 640.0))), ..Default::default() };
        ctx.run_ui(input, |ui| app.ui(ui, glass)).textures_delta.clear();
        app.nav = Nav::default();
    }

    /// a start screen on `data` whose account service is the fake on `port`; nothing is asked yet
    fn screen(data: &Path, port: u16) -> (Start, egui::Context, Glass) {
        let mut app = Start::new(data.to_path_buf(), Err("a test".into()), Settings::default());
        app.session = Session::idle(data.to_path_buf(), account::testing::servers(port, port));
        let ctx = egui::Context::default();
        let glass = Glass::new(&ctx, [1040, 640]);
        (app, ctx, glass)
    }

    fn boy() -> xr_net::x::field::Appearance {
        xr_net::x::look::Look::default_for(0).appearance().unwrap()
    }

    /// a sign-in that lands while the creator is open never sends its character to the account: a new one made from
    /// the local row and an edit of a local character both stay in characters.json
    #[test]
    fn a_sign_in_while_the_creator_is_open_keeps_its_character_local() {
        use account::testing::*;
        let data = temp("c1");
        let mut store = Store::load(&data);
        let old = store.create("Old", boy(), 1).unwrap();
        store.save(&data).unwrap();
        let state = std::sync::Arc::new(std::sync::Mutex::new(State::default()));
        let port = backend(true, state.clone());
        for (editing, name) in [(None, "Ana"), (Some(old), "Old")] {
            let (mut app, ctx, glass) = screen(&data, port);
            app.creator_request = Some(editing);
            frame_with(&ctx, &glass, &mut app, Nav::default());
            assert!(app.creator.is_some() && !app.online_view(), "opened signed out");
            app.session.sign_in("tester".into(), PASSWORD.into());
            app.session.wait();
            assert!(app.online_view(), "signed in while it is open: the Online tab is up now");
            let action = app.creator.as_mut().unwrap().create_now(&data, name).expect("saved");
            app.creator_done(action);
            app.session.wait();
            assert!(Store::load(&data).characters.iter().any(|c| c.name == name), "{name} left this PC's list");
            assert!(!state.lock().unwrap().seen.iter().any(|r| r.starts_with("PUT")), "{name} was put on the account");
            assert!(app.session.characters.iter().all(|c| c.name != name));
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    /// a new character made with the Online tab up goes to the account and never into characters.json
    #[test]
    fn a_new_character_on_the_online_tab_goes_to_the_account_only() {
        use account::testing::*;
        let data = temp("online-new");
        let state = std::sync::Arc::new(std::sync::Mutex::new(State::default()));
        let port = backend(true, state.clone());
        let (mut app, ctx, glass) = screen(&data, port);
        app.session.sign_in("tester".into(), PASSWORD.into());
        app.session.wait();
        app.creator_request = Some(None);
        frame_with(&ctx, &glass, &mut app, Nav::default());
        let action = app.creator.as_mut().unwrap().create_now(&data, "Neo").expect("made");
        assert!(matches!(action, CreatorAction::SavedOnline(_)));
        app.creator_done(action);
        app.session.wait();
        assert!(app.session.characters.iter().any(|c| c.name == "Neo"), "{:?}", app.session.characters);
        assert!(state.lock().unwrap().seen.iter().any(|r| r.starts_with("PUT /v1/characters/")));
        assert!(Store::load(&data).characters.is_empty() && !data.join("characters.json").exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    /// with the pad alone, a signed-in player whose server is down reaches the account pill, Try again, Play with
    /// friends (P2P) and both tabs, and choosing On this PC and P2P works
    #[test]
    fn the_pad_reaches_the_account_the_fallback_and_the_tabs() {
        let data = account::testing::temp("pad");
        std::fs::create_dir_all(data.join("build/x-usa-v0.2.0")).unwrap();
        std::fs::write(data.join("library.json"), r#"{"games":[{"build_id":"x-usa-v0.2.0","dump":"x.cci"}]}"#).unwrap();
        let mut store = Store::load(&data);
        store.create("Here", boy(), 1).unwrap();
        store.save(&data).unwrap();
        let (mut app, ctx, glass) = screen(&data, account::testing::closed_port());
        let me = xr_net::account::Me { account_id: 7, public_id: 4242, username: "tester".into(), email_verified: true };
        app.session.assume(Some(me), Some("down".into()));
        let next = Nav { next: true, ..Nav::default() };
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..30 {
            frame_with(&ctx, &glass, &mut app, next);
            seen.insert(app.focus);
        }
        // one game: the account pill 17, Try again 18, P2P 19, Online 20, On this PC 21
        assert!((17..=21).all(|i| seen.contains(&i)), "{seen:?}");
        let press = |app: &mut Start, item: usize| {
            for _ in 0..30 {
                if app.focus == item {
                    break;
                }
                frame_with(&ctx, &glass, app, next);
            }
            assert_eq!(app.focus, item);
            frame_with(&ctx, &glass, app, Nav { select: true, ..Nav::default() });
        };
        assert!(app.online_view());
        press(&mut app, 21);
        assert!(!app.online_view(), "On this PC was not chosen");
        press(&mut app, 19);
        assert!(app.p2p && !app.offline, "P2P was not chosen");
        press(&mut app, 17);
        assert_eq!(app.panel, Some(Panel::Account));
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Esc (or B) closes the account panel and empties its password field
    #[test]
    fn closing_the_panel_clears_the_password() {
        let data = account::testing::temp("close");
        let (mut app, ctx, glass) = screen(&data, account::testing::closed_port());
        (app.panel, app.form.password) = (Some(Panel::SignIn), "typed but not sent".into());
        frame_with(&ctx, &glass, &mut app, Nav { back: true, ..Nav::default() });
        assert_eq!((app.panel, app.form.password.as_str()), (None, ""));
        let _ = std::fs::remove_dir_all(&data);
    }
}
