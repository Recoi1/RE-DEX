//! what the user chose, kept between runs in the system's place for settings

use std::path::{Path, PathBuf};

use gilrs::Button;
use serde::{Deserialize, Serialize};
use winit::keyboard::KeyCode;
use xr_hle::services::hid::PadState;
use zakuro_gpu::{ScreenLayout, ScreenOrientation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Renderer {
    #[default]
    Vulkan,
    OpenGl,
}

/// how the screens are arranged in the window
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Screens {
    #[default]
    Stacked,
    SideBySide,
    TopOnly,
    BottomOnly,
    LargeTop,
}

impl Screens {
    pub const ALL: [Screens; 5] = [Screens::Stacked, Screens::SideBySide, Screens::LargeTop, Screens::TopOnly, Screens::BottomOnly];

    pub fn name(self) -> &'static str {
        match self {
            Screens::Stacked => "Default (top over bottom)",
            Screens::SideBySide => "Side by side",
            Screens::TopOnly => "Single screen (top)",
            Screens::BottomOnly => "Single screen (bottom)",
            Screens::LargeTop => "Large screen",
        }
    }

    /// the presenter's arrangement, w/ the screens swapped when swap asks (a single screen shows the other one)
    pub fn screens(self, swap: bool) -> ScreenLayout {
        match (self, swap) {
            (Screens::Stacked, false) => ScreenLayout::Stacked,
            (Screens::Stacked, true) => ScreenLayout::StackedSwapped,
            (Screens::SideBySide, false) => ScreenLayout::SideBySide,
            (Screens::SideBySide, true) => ScreenLayout::SideBySideSwapped,
            (Screens::TopOnly, false) | (Screens::BottomOnly, true) => ScreenLayout::TopOnly,
            (Screens::BottomOnly, false) | (Screens::TopOnly, true) => ScreenLayout::BottomOnly,
            (Screens::LargeTop, false) => ScreenLayout::LargeTop,
            (Screens::LargeTop, true) => ScreenLayout::LargeBottom,
        }
    }

    /// the one after it, going round
    pub fn next(self) -> Screens {
        let at = Screens::ALL.iter().position(|&layout| layout == self).unwrap_or(0);
        Screens::ALL[(at + 1) % Screens::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    #[default]
    North,
    East,
    South,
    West,
}

impl Orientation {
    pub const ALL: [Orientation; 4] = [Orientation::North, Orientation::East, Orientation::South, Orientation::West];

    pub fn name(self) -> &'static str {
        match self {
            Orientation::North => "Upright",
            Orientation::East => "90° clockwise",
            Orientation::South => "Upside down (180°)",
            Orientation::West => "90° counterclockwise",
        }
    }

    /// the next one a quarter turn clockwise, going round
    pub fn next(self) -> Orientation {
        Orientation::ALL[(self as usize + 1) % Orientation::ALL.len()]
    }

    pub fn screen_orientation(self) -> ScreenOrientation {
        match self {
            Orientation::North => ScreenOrientation::North,
            Orientation::East => ScreenOrientation::East,
            Orientation::South => ScreenOrientation::South,
            Orientation::West => ScreenOrientation::West,
        }
    }
}

/// how the screens' pixels look scaled up to the window
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    #[default]
    Smooth,
    Sharp,
    Pixels,
}

impl Filter {
    pub const ALL: [Filter; 3] = [Filter::Smooth, Filter::Sharp, Filter::Pixels];

    pub fn name(self) -> &'static str {
        match self {
            Filter::Smooth => "Linear (smooth)",
            Filter::Sharp => "Sharp bilinear",
            Filter::Pixels => "Nearest (pixels)",
        }
    }

    pub fn screens(self) -> zakuro_gpu::ScreenFilter {
        match self {
            Filter::Smooth => zakuro_gpu::ScreenFilter::Smooth,
            Filter::Sharp => zakuro_gpu::ScreenFilter::Sharp,
            Filter::Pixels => zakuro_gpu::ScreenFilter::Pixels,
        }
    }
}

/// the top screen's shape
/// a game draws into 400x240 whatever this is,
/// so anything but the console's own only looks right w/ a widescreen code on,
/// which makes the game draw a wider scene into those pixels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopShape {
    #[default]
    Console,
    Wide16x9,
    Wide21x9,
}

impl TopShape {
    pub const ALL: [TopShape; 3] = [TopShape::Console, TopShape::Wide16x9, TopShape::Wide21x9];

    pub fn name(self) -> &'static str {
        match self {
            TopShape::Console => "Console (5:3)",
            TopShape::Wide16x9 => "16:9",
            TopShape::Wide21x9 => "21:9",
        }
    }

    /// the top screen's width at its height of 240
    pub fn width(self) -> f32 {
        match self {
            TopShape::Console => 400.0,
            TopShape::Wide16x9 => 240.0 * 16.0 / 9.0,
            TopShape::Wide21x9 => 560.0,
        }
    }
}

/// how the top screen's 3D shows, for games that draw it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stereo3d {
    /// games draw flat, like w/ the console's 3D slider down
    #[default]
    Off,
    Anaglyph,
    SideBySide,
    HalfSideBySide,
    Interlaced,
}

impl Stereo3d {
    pub const ALL: [Stereo3d; 5] = [Stereo3d::Off, Stereo3d::Anaglyph, Stereo3d::SideBySide, Stereo3d::HalfSideBySide, Stereo3d::Interlaced];

    pub fn name(self) -> &'static str {
        match self {
            Stereo3d::Off => "Off",
            Stereo3d::Anaglyph => "Red and cyan glasses",
            Stereo3d::SideBySide => "Side by side",
            Stereo3d::HalfSideBySide => "Side by side, half width",
            Stereo3d::Interlaced => "Alternate rows",
        }
    }

    pub fn mode(self) -> zakuro_gpu::StereoMode {
        match self {
            Stereo3d::Off => zakuro_gpu::StereoMode::Off,
            Stereo3d::Anaglyph => zakuro_gpu::StereoMode::Anaglyph,
            Stereo3d::SideBySide => zakuro_gpu::StereoMode::SideBySide,
            Stereo3d::HalfSideBySide => zakuro_gpu::StereoMode::HalfSideBySide,
            Stereo3d::Interlaced => zakuro_gpu::StereoMode::Interlaced,
        }
    }
}

/// Smooth motion (frame interpolation): off, pictures in between for 60 a second (a 30 fps game's in between pictures,
/// the least latency), or for each refresh of the monitor (up to 240 Hz)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum SmoothMotion {
    #[default]
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "60")]
    Sixty,
    #[serde(rename = "monitor")]
    Monitor,
}

impl SmoothMotion {
    pub const ALL: [SmoothMotion; 3] = [SmoothMotion::Off, SmoothMotion::Sixty, SmoothMotion::Monitor];

    pub fn name(self) -> &'static str {
        match self {
            SmoothMotion::Off => "Off",
            SmoothMotion::Sixty => "60 fps",
            SmoothMotion::Monitor => "Match monitor (up to 240)",
        }
    }
}

// a file from before the choice has true or false: on was one picture in between, which Match monitor goes on from
impl<'de> Deserialize<'de> for SmoothMotion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match toml::Value::deserialize(deserializer)? {
            toml::Value::Boolean(true) => SmoothMotion::Monitor,
            toml::Value::String(value) if value == "60" => SmoothMotion::Sixty,
            toml::Value::String(value) if value == "monitor" => SmoothMotion::Monitor,
            _ => SmoothMotion::Off,
        })
    }
}

/// the menus' look, Windows' own or chosen
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Dark, Theme::Light];

    pub fn name(self) -> &'static str {
        match self {
            Theme::System => "Same as Windows",
            Theme::Dark => "Dark",
            Theme::Light => "Light",
        }
    }
}

/// the console's system language, a title reads it from cfg:u and picks its localized text and files by it,
/// the codes the console numbers them by
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Japanese,
    #[default]
    English,
    French,
    German,
    Italian,
    Spanish,
    ChineseSimplified,
    Korean,
    Dutch,
    Portuguese,
    Russian,
    ChineseTraditional,
}

impl Language {
    pub const ALL: [Language; 12] = [
        Language::Japanese,
        Language::English,
        Language::French,
        Language::German,
        Language::Italian,
        Language::Spanish,
        Language::ChineseSimplified,
        Language::Korean,
        Language::Dutch,
        Language::Portuguese,
        Language::Russian,
        Language::ChineseTraditional,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Language::Japanese => "Japanese",
            Language::English => "English",
            Language::French => "French",
            Language::German => "German",
            Language::Italian => "Italian",
            Language::Spanish => "Spanish",
            Language::ChineseSimplified => "Chinese (Simplified)",
            Language::Korean => "Korean",
            Language::Dutch => "Dutch",
            Language::Portuguese => "Portuguese",
            Language::Russian => "Russian",
            Language::ChineseTraditional => "Chinese (Traditional)",
        }
    }

    /// the number the console's blocks give the language
    pub fn code(self) -> u8 {
        match self {
            Language::Japanese => 0,
            Language::English => 1,
            Language::French => 2,
            Language::German => 3,
            Language::Italian => 4,
            Language::Spanish => 5,
            Language::ChineseSimplified => 6,
            Language::Korean => 7,
            Language::Dutch => 8,
            Language::Portuguese => 9,
            Language::Russian => 10,
            Language::ChineseTraditional => 11,
        }
    }
}

/// what the console's cameras see
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CameraSource {
    /// the computer's camera, on only while a game takes pictures
    #[default]
    Webcam,
    /// a picture from a file, like a QR code's
    Picture,
    /// nothing, black
    Nothing,
}

impl CameraSource {
    pub const ALL: [CameraSource; 3] = [CameraSource::Webcam, CameraSource::Picture, CameraSource::Nothing];

    pub fn name(self) -> &'static str {
        match self {
            CameraSource::Webcam => "The computer's camera",
            CameraSource::Picture => "A picture",
            CameraSource::Nothing => "Nothing",
        }
    }
}

/// what the console's microphone hears
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MicrophoneSource {
    /// the computer's mic, only open while a game listens
    Computer,
    /// nothing except the blowing key
    /// the default, since some games listen all the time (Super Mario 3D Land from its start) and the computer's
    /// mic would stay open w/ them
    #[default]
    Nothing,
}

impl MicrophoneSource {
    pub const ALL: [MicrophoneSource; 2] = [MicrophoneSource::Computer, MicrophoneSource::Nothing];

    pub fn name(self) -> &'static str {
        match self {
            MicrophoneSource::Computer => "The computer's microphone",
            MicrophoneSource::Nothing => "Nothing",
        }
    }
}

/// the keys the console's buttons, circle pad and C stick are on
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Keys {
    pub a: KeyCode,
    pub b: KeyCode,
    pub x: KeyCode,
    pub y: KeyCode,
    pub l: KeyCode,
    pub r: KeyCode,
    pub start: KeyCode,
    pub select: KeyCode,
    pub up: KeyCode,
    pub down: KeyCode,
    pub left: KeyCode,
    pub right: KeyCode,
    pub circle_up: KeyCode,
    pub circle_down: KeyCode,
    pub circle_left: KeyCode,
    pub circle_right: KeyCode,
    pub zl: KeyCode,
    pub zr: KeyCode,
    pub c_up: KeyCode,
    pub c_down: KeyCode,
    pub c_left: KeyCode,
    pub c_right: KeyCode,
    /// opens the Players list (a window key, not a console button: tapped when it is Tab, held Tab fast forwards)
    pub players: KeyCode,
}

impl Default for Keys {
    fn default() -> Self {
        // a PC layout: WASD walks (circle pad), the arrows are the D-pad, the face buttons sit around WASD,
        // and none of them is a window hotkey (Tab, M, F1, F9, F10, F11, Esc)
        // (Zakuro's were X/Z/S/A, Q/W and IJKL; a settings file saved before keeps its own)
        Keys {
            a: KeyCode::Space,
            b: KeyCode::KeyQ,
            x: KeyCode::KeyF,
            y: KeyCode::KeyR,
            l: KeyCode::Digit1,
            r: KeyCode::Digit3,
            start: KeyCode::Enter,
            select: KeyCode::Backspace,
            up: KeyCode::ArrowUp,
            down: KeyCode::ArrowDown,
            left: KeyCode::ArrowLeft,
            right: KeyCode::ArrowRight,
            circle_up: KeyCode::KeyW,
            circle_down: KeyCode::KeyS,
            circle_left: KeyCode::KeyA,
            circle_right: KeyCode::KeyD,
            zl: KeyCode::KeyZ,
            zr: KeyCode::KeyC,
            c_up: KeyCode::KeyI,
            c_down: KeyCode::KeyK,
            c_left: KeyCode::KeyJ,
            c_right: KeyCode::KeyL,
            players: KeyCode::Tab,
        }
    }
}

impl Keys {
    /// is a console button on key
    pub fn binds(&self, key: KeyCode) -> bool {
        self.clone().all_mut().into_iter().any(|(_, bound)| *bound == key)
    }

    /// every binding w/ the name it goes by, to show and change them
    pub fn all_mut(&mut self) -> [(&'static str, &mut KeyCode); 22] {
        [
            ("A", &mut self.a),
            ("B", &mut self.b),
            ("X", &mut self.x),
            ("Y", &mut self.y),
            ("L", &mut self.l),
            ("R", &mut self.r),
            ("Start", &mut self.start),
            ("Select", &mut self.select),
            ("D-pad up", &mut self.up),
            ("D-pad down", &mut self.down),
            ("D-pad left", &mut self.left),
            ("D-pad right", &mut self.right),
            ("Circle pad up", &mut self.circle_up),
            ("Circle pad down", &mut self.circle_down),
            ("Circle pad left", &mut self.circle_left),
            ("Circle pad right", &mut self.circle_right),
            ("ZL", &mut self.zl),
            ("ZR", &mut self.zr),
            ("C-stick up", &mut self.c_up),
            ("C-stick down", &mut self.c_down),
            ("C-stick left", &mut self.c_left),
            ("C-stick right", &mut self.c_right),
        ]
    }
}

/// the controller buttons the console's buttons are on, the face buttons where the 3DS has them instead of by their letters
/// the circle pad is the left stick and the C stick the right one
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PadButtons {
    pub a: Button,
    pub b: Button,
    pub x: Button,
    pub y: Button,
    pub l: Button,
    pub r: Button,
    pub start: Button,
    pub select: Button,
    pub up: Button,
    pub down: Button,
    pub left: Button,
    pub right: Button,
    pub zl: Button,
    pub zr: Button,
    /// opens the Players list: a button no console button is on (the left stick's click by default), so the list never
    /// takes one from the game
    pub players: Button,
}

impl Default for PadButtons {
    fn default() -> Self {
        PadButtons {
            a: Button::East,
            b: Button::South,
            x: Button::North,
            y: Button::West,
            l: Button::LeftTrigger,
            r: Button::RightTrigger,
            start: Button::Start,
            select: Button::Select,
            up: Button::DPadUp,
            down: Button::DPadDown,
            left: Button::DPadLeft,
            right: Button::DPadRight,
            zl: Button::LeftTrigger2,
            zr: Button::RightTrigger2,
            players: Button::LeftThumb,
        }
    }
}

impl PadButtons {
    /// A button the Players list cannot be bound to: Home (Configure) and every button the console layout uses (the
    /// game never gets a button that opens the list too).
    pub fn players_refuses(&self, button: Button) -> bool {
        button == Button::Mode || self.map().iter().any(|&(_, b)| b == button)
    }

    /// every binding w/ the name it goes by, to show and change them
    pub fn all_mut(&mut self) -> [(&'static str, &mut Button); 14] {
        [
            ("A", &mut self.a),
            ("B", &mut self.b),
            ("X", &mut self.x),
            ("Y", &mut self.y),
            ("L", &mut self.l),
            ("R", &mut self.r),
            ("Start", &mut self.start),
            ("Select", &mut self.select),
            ("D-pad up", &mut self.up),
            ("D-pad down", &mut self.down),
            ("D-pad left", &mut self.left),
            ("D-pad right", &mut self.right),
            ("ZL", &mut self.zl),
            ("ZR", &mut self.zr),
        ]
    }

    /// each of the console's buttons and the controller button it's on
    pub fn map(&self) -> [(PadState, Button); 14] {
        [
            (PadState::A, self.a),
            (PadState::B, self.b),
            (PadState::X, self.x),
            (PadState::Y, self.y),
            (PadState::L, self.l),
            (PadState::R, self.r),
            (PadState::START, self.start),
            (PadState::SELECT, self.select),
            (PadState::UP, self.up),
            (PadState::DOWN, self.down),
            (PadState::LEFT, self.left),
            (PadState::RIGHT, self.right),
            (PadState::ZL, self.zl),
            (PadState::ZR, self.zr),
        ]
    }
}

/// what a game has of its own over the general settings, each one left out follows them
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameSettings {
    pub hardware_rasterizer: Option<bool>,
    pub resolution: Option<u32>,
    pub texture_packs: Option<bool>,
    /// written as on/off (see Settings::to_text), the choice itself goes in smooth_motion
    #[serde(serialize_with = "switch_of", skip_serializing_if = "Option::is_none")]
    pub frame_interpolation: Option<SmoothMotion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smooth_motion: Option<SmoothMotion>,
    pub scale: Option<u32>,
    #[serde(deserialize_with = "or_default")]
    pub layout: Option<Screens>,
    #[serde(deserialize_with = "or_default")]
    pub orientation: Option<Orientation>,
    #[serde(deserialize_with = "or_default")]
    pub filter: Option<Filter>,
    pub integer_scale: Option<bool>,
    #[serde(deserialize_with = "or_default")]
    pub top_shape: Option<TopShape>,
    #[serde(deserialize_with = "or_default")]
    pub stereo: Option<Stereo3d>,
    pub depth_3d: Option<u32>,
    #[serde(deserialize_with = "or_default")]
    pub language: Option<Language>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// the folder the library lists games from
    pub games: Option<PathBuf>,
    /// the console's system language, titles pick their localized text and files by it
    #[serde(deserialize_with = "or_default")]
    pub language: Language,
    #[serde(deserialize_with = "or_default")]
    pub renderer: Renderer,
    /// draw the 3D on the GPU instead of in software
    pub hardware_rasterizer: bool,
    /// how many times the console's res the GPU draws the 3D at
    pub resolution: u32,
    /// draw the texture packs put in games' mods folders
    pub texture_packs: bool,
    /// Smooth motion: draw pictures in between each two of a game at 30 fps (or 20)
    /// written as on/off, which builds from before the choice read (see to_text)
    #[serde(serialize_with = "switch")]
    pub frame_interpolation: SmoothMotion,
    /// the file's Smooth motion choice, only while reading or writing it (see from_text)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smooth_motion: Option<SmoothMotion>,
    /// Responsive movement, w/ Smooth motion on: the trainer and the field camera drawn ahead from the input in the
    /// pictures in between (render side only); off they're Smooth motion's, bit for bit. Experimental: off by default
    pub responsive_movement: bool,
    /// window size, times the console's
    pub scale: u32,
    /// where the window was and its size, the frame's top left and the inside's width and height in pixels, for next time
    pub window: Option<[i32; 4]>,
    #[serde(deserialize_with = "or_default")]
    pub layout: Screens,
    /// the bottom screen where the top one goes and the other way round (Azahar's swap screens)
    pub swap_screens: bool,
    #[serde(deserialize_with = "or_default")]
    pub orientation: Orientation,
    /// present w/o tearing, off presents right away (lower latency, may tear); frames keep the console's pace either way
    pub vsync: bool,
    /// keep the game to speed_limit percent of the console's speed (Azahar's limit speed), off runs it unlocked
    pub limit_speed: bool,
    pub speed_limit: u32,
    /// how the screens' pixels look scaled up
    #[serde(deserialize_with = "or_default")]
    pub filter: Filter,
    /// only scale the screens by whole numbers, leaving a border
    pub integer_scale: bool,
    /// how wide the top screen shows
    #[serde(deserialize_with = "or_default")]
    pub top_shape: TopShape,
    /// how the 3D shows
    #[serde(deserialize_with = "or_default")]
    pub stereo: Stereo3d,
    /// how deep games draw the 3D, the console's slider, 0 to 100
    pub depth_3d: u32,
    /// each eye gets the other's picture
    pub swap_eyes: bool,
    /// 0 to 1
    pub volume: f32,
    pub mute: bool,
    pub show_fps: bool,
    /// the menus' look
    #[serde(deserialize_with = "or_default")]
    pub theme: Theme,
    /// a picture behind the library
    pub background: Option<PathBuf>,
    /// how much of it shows, 0 to 1
    pub background_opacity: f32,
    pub keys: Keys,
    pub pad: PadButtons,
    /// the enhancements turned on, by name, for each game by its title id
    pub enhancements: std::collections::BTreeMap<String, Vec<String>>,
    /// what each game has of its own, by its title id
    pub per_game: std::collections::BTreeMap<String, GameSettings>,
    /// what the console's cameras see, and the picture when it's one
    #[serde(deserialize_with = "or_default")]
    pub camera: CameraSource,
    pub camera_picture: Option<PathBuf>,
    /// what the console's mic hears
    #[serde(deserialize_with = "or_default")]
    pub microphone: MicrophoneSource,
    /// the name the player goes by in rooms, the console's own too
    pub nickname: String,
    /// the number that tells this console apart over local wireless, made once
    pub console_id: u64,
    /// the room last joined, and the one opened, its name and port
    pub room_address: String,
    pub room_name: String,
    pub room_port: u16,
    /// the RE:DEX world server joined at boot (host:port), none when empty; --world overrides it
    pub world_address: String,
    /// the name other players see there; the save's trainer name when empty; --name overrides it
    pub world_name: String,
    /// names over the other players' heads
    pub show_names: bool,
    /// the account service and its world (`tls:host:port`), the official ones when empty (tests name others)
    pub account_server: String,
    pub account_world: String,
    /// show the game, the mode and the play time as the Discord status (the local Discord app only)
    pub discord_status: bool,
    /// the area (the region, later the place) in that status
    pub discord_location: bool,
    /// the interface's own sounds (clicks, panels opening and closing, toasts)
    pub ui_sounds: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            games: None,
            language: Language::English,
            renderer: Renderer::Vulkan,
            hardware_rasterizer: true,
            resolution: 2,
            texture_packs: true,
            frame_interpolation: SmoothMotion::Off,
            smooth_motion: None,
            responsive_movement: false,
            scale: 2,
            window: None,
            layout: Screens::Stacked,
            swap_screens: false,
            orientation: Orientation::North,
            vsync: true,
            limit_speed: true,
            speed_limit: 100,
            filter: Filter::Smooth,
            integer_scale: false,
            top_shape: TopShape::Console,
            stereo: Stereo3d::Off,
            depth_3d: 50,
            swap_eyes: false,
            volume: 1.0,
            mute: false,
            show_fps: false,
            theme: Theme::System,
            background: None,
            background_opacity: 0.35,
            keys: Keys::default(),
            pad: PadButtons::default(),
            enhancements: std::collections::BTreeMap::new(),
            per_game: std::collections::BTreeMap::new(),
            camera: CameraSource::Webcam,
            camera_picture: None,
            microphone: MicrophoneSource::Nothing,
            nickname: String::new(),
            console_id: 0,
            room_address: String::new(),
            room_name: String::new(),
            room_port: xr_hle::room::DEFAULT_PORT,
            world_address: String::new(),
            world_name: String::new(),
            show_names: true,
            account_server: String::new(),
            account_world: String::new(),
            discord_status: true,
            discord_location: true,
            ui_sounds: true,
        }
    }
}

/// a setting's value, or its default when the file holds one this build doesn't know (a newer Zakuro's say),
/// so one value doesn't lose all the others
fn or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    let value = toml::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

impl GameSettings {
    /// the settings w/ these over them
    fn over(&self, settings: &mut Settings) {
        settings.hardware_rasterizer = self.hardware_rasterizer.unwrap_or(settings.hardware_rasterizer);
        settings.resolution = self.resolution.unwrap_or(settings.resolution);
        settings.texture_packs = self.texture_packs.unwrap_or(settings.texture_packs);
        settings.frame_interpolation = self.frame_interpolation.unwrap_or(settings.frame_interpolation);
        settings.scale = self.scale.unwrap_or(settings.scale);
        settings.layout = self.layout.unwrap_or(settings.layout);
        settings.orientation = self.orientation.unwrap_or(settings.orientation);
        settings.filter = self.filter.unwrap_or(settings.filter);
        settings.integer_scale = self.integer_scale.unwrap_or(settings.integer_scale);
        settings.top_shape = self.top_shape.unwrap_or(settings.top_shape);
        settings.stereo = self.stereo.unwrap_or(settings.stereo);
        settings.depth_3d = self.depth_3d.unwrap_or(settings.depth_3d);
        settings.language = self.language.unwrap_or(settings.language);
    }
}

/// what a game goes by in the settings file, its title id
pub fn game_key(program_id: u64) -> String {
    format!("{program_id:016X}")
}

impl Settings {
    /// the settings a game plays w/, or the general ones w/o a game
    pub fn for_game(&self, program_id: Option<u64>) -> Settings {
        let mut settings = self.clone();
        if let Some(own) = program_id.and_then(|id| self.per_game.get(&game_key(id))) {
            own.over(&mut settings);
        }
        settings
    }

    /// does a game have anything of its own
    pub fn has_own(&self, program_id: u64) -> bool {
        self.per_game.contains_key(&game_key(program_id))
    }

    /// where the console's 3D slider is, down while the 3D is off, so games don't draw a picture nobody sees
    pub fn slider_3d(&self) -> f32 {
        if self.stereo == Stereo3d::Off {
            0.0
        } else {
            self.depth_3d.min(100) as f32 / 100.0
        }
    }

    /// the 3D the presenter shows
    pub fn stereo(&self) -> zakuro_gpu::Stereo {
        zakuro_gpu::Stereo { mode: self.stereo.mode(), swap: self.swap_eyes }
    }

    /// the saved settings, or the defaults when there are none
    pub fn load() -> Settings {
        let Some(path) = path() else { return Settings::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => Settings::from_text(&text).unwrap_or_else(|error| {
                // kept aside, the next save would write over it
                let aside = path.with_extension("toml.bad");
                let _ = std::fs::copy(&path, &aside);
                log::warn!("could not read {}, {error}, using the defaults, it is kept as {}", path.display(), aside.display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    /// the file's text: Smooth motion's choice goes in smooth_motion and frame_interpolation keeps the on/off switch, so
    /// a build from before the choice still reads the file (it sets the whole file aside over a value it doesn't know)
    fn to_text(&self) -> Result<String, toml::ser::Error> {
        let mut file = self.clone();
        file.smooth_motion = Some(file.frame_interpolation);
        for game in file.per_game.values_mut() {
            game.smooth_motion = game.frame_interpolation;
        }
        toml::to_string_pretty(&file)
    }

    /// the settings in a file's text, smooth_motion over frame_interpolation where it's there
    fn from_text(text: &str) -> Result<Settings, toml::de::Error> {
        let mut settings: Settings = toml::from_str(text)?;
        settings.frame_interpolation = settings.smooth_motion.take().unwrap_or(settings.frame_interpolation);
        for game in settings.per_game.values_mut() {
            game.frame_interpolation = game.smooth_motion.take().or(game.frame_interpolation);
        }
        Ok(settings)
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let written = self
            .to_text()
            .map_err(|error| error.to_string())
            .and_then(|text| write(&path, &text).map_err(|error| error.to_string()));
        if let Err(error) = written {
            log::warn!("could not save the settings to {}, {error}", path.display());
        }
    }
}

/// writes the file whole or not at all: into a file beside it, then renamed over it, so a crash or a full disk
/// mid-write leaves the old settings, never half of the new ones
fn write(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("toml.new");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

fn switch<S: serde::Serializer>(smooth: &SmoothMotion, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bool(*smooth != SmoothMotion::Off)
}

fn switch_of<S: serde::Serializer>(smooth: &Option<SmoothMotion>, serializer: S) -> Result<S::Ok, S::Error> {
    match smooth {
        Some(smooth) => switch(smooth, serializer),
        None => serializer.serialize_none(),
    }
}

/// the system's place for a program's settings
fn path() -> Option<PathBuf> {
    path_from(|name| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from))
}

/// `path` with the environment given: REDEX_SETTINGS, else `<XR_DATA>/settings.toml` (a data folder of its own keeps
/// its own settings, as a clean-room run needs), else the system's place
fn path_from(var: impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    if let Some(data) = var("XR_DATA").filter(|_| var("REDEX_SETTINGS").is_none()) {
        return Some(data.join("settings.toml"));
    }
    let dir = if cfg!(windows) {
        var("APPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library/Application Support"))
    } else {
        var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|home| home.join(".config")))
    };
    // REDEX_SETTINGS points tests and scripted checks at a file of their own, so the player's stays untouched
    if let Some(file) = var("REDEX_SETTINGS") {
        return Some(file);
    }
    dir.map(|dir| dir.join("redex").join("settings.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xr_data_keeps_its_own_settings() {
        let env = |vars: &'static [(&'static str, &'static str)]| move |name: &str| vars.iter().find(|v| v.0 == name).map(|v| PathBuf::from(v.1));
        assert_eq!(path_from(env(&[("XR_DATA", "D:/clean/data"), ("APPDATA", "C:/a")])), Some(PathBuf::from("D:/clean/data/settings.toml")));
        assert_eq!(path_from(env(&[("XR_DATA", "D:/clean/data"), ("REDEX_SETTINGS", "t.toml")])), Some(PathBuf::from("t.toml")));
        if cfg!(windows) {
            assert_eq!(path_from(env(&[("APPDATA", "C:/a")])), Some(PathBuf::from("C:/a/redex/settings.toml")));
        }
    }

    /// the file is replaced whole, w/ nothing left beside it
    #[test]
    fn the_file_is_written_whole() {
        let dir = std::env::temp_dir().join(format!("redex-settings-{}", std::process::id()));
        let file = dir.join("settings.toml");
        write(&file, "old = 1\n").unwrap();
        write(&file, "new = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new = 2\n");
        assert!(!file.with_extension("toml.new").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_come_back_as_they_were_saved() {
        let mut settings =
            Settings { games: Some("/games".into()), show_fps: true, layout: Screens::TopOnly, language: Language::German, ..Settings::default() };
        settings.keys.a = KeyCode::KeyK;
        settings.pad.a = Button::South;
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
    }

    #[test]
    fn a_value_this_build_does_not_know_keeps_the_others() {
        let settings: Settings = toml::from_str("show_fps = true\nlayout = \"three_screens\"\nfilter = \"crt\"\n").unwrap();
        assert!(settings.show_fps);
        assert_eq!(settings.layout, Screens::default());
        assert_eq!(settings.filter, Filter::default());
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let settings: Settings = toml::from_str("show_fps = true\n[keys]\na = \"KeyK\"\n").unwrap();
        assert!(settings.show_fps);
        assert_eq!(settings.keys.a, KeyCode::KeyK);
        assert_eq!(settings.keys.b, Keys::default().b);
        assert_eq!(settings.scale, 2);
        assert_eq!(settings.layout, Screens::Stacked);
        // a file from before the Players list: names shown, the list on Tab and the left stick's click
        assert!(settings.show_names);
        assert!(settings.discord_status && settings.discord_location, "on for a file from before the Discord status");
        assert_eq!((settings.keys.players, settings.pad.players), (KeyCode::Tab, Button::LeftThumb));
        // only buttons the console layout does not use can open the list
        let pad = PadButtons::default();
        assert!(pad.players_refuses(pad.a) && pad.players_refuses(pad.up) && pad.players_refuses(pad.start) && pad.players_refuses(Button::Mode));
        assert!(!pad.players_refuses(Button::LeftThumb));
        // which no console button is on, and the fast forward trigger is not either
        let pad = PadButtons::default();
        assert!(pad.map().iter().all(|&(_, b)| b != pad.players) && pad.players != Button::RightThumb && pad.players != Button::RightTrigger2);
    }

    /// a game plays w/ what it has of its own and w/ the general settings in everything else, and no other game gets touched
    #[test]
    fn a_game_has_its_own_settings_over_the_general_ones() {
        let mut settings = Settings { resolution: 2, layout: Screens::SideBySide, ..Settings::default() };
        settings.per_game.insert(
            game_key(0x0004_0000_0012_3400),
            GameSettings { orientation: Some(Orientation::East), resolution: Some(5), ..GameSettings::default() },
        );
        let game = settings.for_game(Some(0x0004_0000_0012_3400));
        assert_eq!(game.orientation, Orientation::East);
        assert_eq!(game.resolution, 5);
        assert_eq!(game.layout, Screens::SideBySide);
        for other in [Some(0x0004_0000_0012_3500), None] {
            let general = settings.for_game(other);
            assert_eq!(general, settings);
        }
        assert!(settings.has_own(0x0004_0000_0012_3400));
        assert!(!settings.has_own(0x0004_0000_0012_3500));
    }

    #[test]
    fn a_games_settings_come_back_as_they_were_saved() {
        let mut settings = Settings::default();
        settings.per_game.insert(
            game_key(0x0004_0000_0012_3400),
            GameSettings { orientation: Some(Orientation::West), language: Some(Language::Japanese), texture_packs: Some(false), ..GameSettings::default() },
        );
        let text = toml::to_string_pretty(&settings).unwrap();
        let own = text.split("[per_game.").nth(1).unwrap();
        assert!(!own.contains("resolution"), "only what it has of its own is written for a game");
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
    }

    /// a value from a newer Zakuro leaves the game following the general setting, and takes nothing else w/ it
    #[test]
    fn a_value_this_build_does_not_know_follows_the_general_setting() {
        let text = "[per_game.0004000000123400]\nlayout = \"three_screens\"\nresolution = 4\n";
        let settings: Settings = toml::from_str(text).unwrap();
        let own = &settings.per_game[&game_key(0x0004_0000_0012_3400)];
        assert_eq!(own.layout, None);
        assert_eq!(own.resolution, Some(4));
    }

    /// every setting the menus change comes back from the file as it was saved
    #[test]
    fn every_menu_setting_round_trips() {
        let mut settings = Settings {
            renderer: Renderer::OpenGl,
            hardware_rasterizer: false,
            resolution: 7,
            texture_packs: false,
            frame_interpolation: SmoothMotion::Sixty,
            scale: 4,
            window: Some([-1200, 40, 1110, 1333]),
            layout: Screens::LargeTop,
            swap_screens: true,
            orientation: Orientation::West,
            vsync: false,
            limit_speed: false,
            speed_limit: 250,
            filter: Filter::Pixels,
            integer_scale: true,
            top_shape: TopShape::Wide16x9,
            stereo: Stereo3d::Interlaced,
            depth_3d: 77,
            swap_eyes: true,
            volume: 0.25,
            mute: true,
            show_fps: true,
            theme: Theme::Light,
            language: Language::French,
            microphone: MicrophoneSource::Computer,
            show_names: false,
            discord_status: false,
            discord_location: false,
            ..Settings::default()
        };
        settings.keys.players = KeyCode::KeyP;
        settings.pad.players = Button::North;
        settings.keys.circle_up = KeyCode::KeyT;
        settings.pad.select = Button::Mode;
        let text = settings.to_text().unwrap();
        let back = Settings::from_text(&text).unwrap();
        assert_eq!(back, settings);
        assert_ne!(back, Settings::default());
    }

    /// the PC layout's keys are each on one button, and none is a window hotkey
    #[test]
    fn default_keys_are_distinct_and_leave_the_hotkeys_alone() {
        let mut keys = Keys::default();
        let bound: Vec<KeyCode> = keys.all_mut().into_iter().map(|(_, key)| *key).collect();
        for (i, key) in bound.iter().enumerate() {
            assert!(!bound[i + 1..].contains(key), "{key:?} twice");
        }
        for hotkey in [KeyCode::Tab, KeyCode::KeyM, KeyCode::F1, KeyCode::F9, KeyCode::F10, KeyCode::F11, KeyCode::Escape] {
            assert!(!keys.binds(hotkey), "{hotkey:?} is a hotkey");
        }
        assert_eq!((keys.circle_up, keys.circle_left, keys.circle_down, keys.circle_right), (KeyCode::KeyW, KeyCode::KeyA, KeyCode::KeyS, KeyCode::KeyD));
        assert_eq!((keys.a, keys.b, keys.start), (KeyCode::Space, KeyCode::KeyQ, KeyCode::Enter));
    }

    /// a settings file from before the PC layout keeps the keys it saved
    #[test]
    fn saved_keys_stay_as_they_were() {
        let mut old = Settings::default();
        old.keys = Keys { a: KeyCode::KeyX, b: KeyCode::KeyZ, y: KeyCode::KeyA, circle_up: KeyCode::KeyI, c_left: KeyCode::KeyF, ..Keys::default() };
        // as a build w/o the speed limit, vsync and swap screens wrote it
        let text: String = toml::to_string_pretty(&old)
            .unwrap()
            .lines()
            .filter(|line| !["vsync", "limit_speed", "speed_limit", "swap_screens"].iter().any(|name| line.starts_with(name)))
            .map(|line| format!("{line}\n"))
            .collect();
        let settings: Settings = toml::from_str(&text).unwrap();
        assert_eq!(settings.keys, old.keys);
        // and plays at the console's speed w/o tearing
        assert!(settings.vsync && settings.limit_speed && settings.speed_limit == 100 && !settings.swap_screens);
    }

    /// Smooth motion's choice reads back, and a file from when it was on or off keeps it (on goes to Match monitor)
    #[test]
    fn smooth_motion_reads_the_old_switch() {
        let read = |line: &str| toml::from_str::<Settings>(line).unwrap().frame_interpolation;
        assert_eq!(read("frame_interpolation = true"), SmoothMotion::Monitor);
        assert_eq!(read("frame_interpolation = false"), SmoothMotion::Off);
        assert_eq!(read("frame_interpolation = \"60\""), SmoothMotion::Sixty);
        assert_eq!(read("frame_interpolation = \"monitor\""), SmoothMotion::Monitor);
        assert_eq!(read("frame_interpolation = \"off\""), SmoothMotion::Off);
        assert_eq!(read(""), SmoothMotion::Off);
        // a build from before the choice reads the switch from the file, the choice comes back
        #[derive(Deserialize)]
        struct Old {
            frame_interpolation: bool,
            per_game: std::collections::BTreeMap<String, OldGame>,
        }
        #[derive(Deserialize)]
        struct OldGame {
            frame_interpolation: Option<bool>,
        }
        for smooth in SmoothMotion::ALL {
            let mut settings = Settings { frame_interpolation: smooth, ..Settings::default() };
            settings.per_game.insert(game_key(1), GameSettings { frame_interpolation: Some(smooth), ..GameSettings::default() });
            settings.per_game.insert(game_key(2), GameSettings::default());
            let text = settings.to_text().unwrap();
            let old: Old = toml::from_str(&text).unwrap();
            assert_eq!(old.frame_interpolation, smooth != SmoothMotion::Off);
            assert_eq!(old.per_game[&game_key(1)].frame_interpolation, Some(smooth != SmoothMotion::Off));
            assert_eq!(old.per_game[&game_key(2)].frame_interpolation, None);
            assert_eq!(Settings::from_text(&text).unwrap(), settings);
        }
        let game: GameSettings = toml::from_str("frame_interpolation = true").unwrap();
        assert_eq!(game.frame_interpolation, Some(SmoothMotion::Monitor));
    }

    /// swapping the screens shows the other one alone in the single screen layouts
    #[test]
    fn swapping_a_single_screen_shows_the_other() {
        assert_eq!(Screens::TopOnly.screens(true), ScreenLayout::BottomOnly);
        assert_eq!(Screens::BottomOnly.screens(true), ScreenLayout::TopOnly);
        assert_eq!(Screens::Stacked.screens(true), ScreenLayout::StackedSwapped);
        assert_eq!(Orientation::West.next(), Orientation::North);
    }

    #[test]
    fn layouts_go_round() {
        let mut layout = Screens::Stacked;
        for _ in 0..Screens::ALL.len() {
            layout = layout.next();
        }
        assert_eq!(layout, Screens::Stacked);
    }
}
