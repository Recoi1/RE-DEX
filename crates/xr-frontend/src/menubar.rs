//! the native menu bar across the top of the window, like Azahar's (File, Emulation, View, Tools, Multiplayer, Help),
//! thru muda's Win32 menus on the window's HWND. It sits in the window's frame, outside the area the presenter draws
//! in, so the screens fill the rest of the window and nothing of it reaches the 3DS screens or headless captures.
//! The shortcuts it shows are the window's own keys (lib.rs handles them); muda's accelerator table is left unused.

#[cfg(windows)]
use muda::accelerator::Accelerator;
#[cfg(windows)]
use muda::{AboutMetadata, CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU32};

use crate::settings::{Screens, Settings};

/// what an item asks for
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    OpenSaves,
    OpenLogs,
    Exit,
    Pause,
    Restart,
    /// a speed limit in percent, none for unlocked
    Speed(Option<u32>),
    Configure,
    Fullscreen,
    Layout(Screens),
    NextLayout,
    Swap,
    Rotate,
    ShowFps,
    Screenshot,
    TexturePacks,
    GitHub,
    /// Multiplayer > Browse servers
    Browse,
    /// Multiplayer > Players nearby
    Players,
    /// Multiplayer > Show whispers
    Whispers,
}

/// the speed limits the menu offers, Azahar's own 100% and 200% presets and unlocked
#[cfg_attr(not(windows), allow(dead_code))]
const SPEEDS: [Option<u32>; 3] = [Some(100), Some(200), None];

#[cfg(windows)]
pub struct MenuBar {
    menu: Menu,
    pause: CheckMenuItem,
    speeds: Vec<CheckMenuItem>,
    fullscreen: CheckMenuItem,
    layouts: Vec<CheckMenuItem>,
    swap: CheckMenuItem,
    fps: CheckMenuItem,
    texture_packs: CheckMenuItem,
}

#[cfg(windows)]
fn item(id: &str, text: &str, shortcut: Option<&str>) -> MenuItem {
    MenuItem::with_id(id, text, true, shortcut.and_then(|key| key.parse::<Accelerator>().ok()))
}

#[cfg(windows)]
fn check(id: &str, text: &str, shortcut: Option<&str>) -> CheckMenuItem {
    CheckMenuItem::with_id(id, text, true, false, shortcut.and_then(|key| key.parse::<Accelerator>().ok()))
}

#[cfg(windows)]
fn speed_name(speed: Option<u32>) -> String {
    speed.map_or("Unlocked".to_owned(), |percent| format!("{percent}%"))
}

#[cfg(windows)]
impl MenuBar {
    pub fn new() -> Result<MenuBar, muda::Error> {
        let pause = check("pause", "&Pause", Some("Escape"));
        let speeds: Vec<CheckMenuItem> = SPEEDS.iter().map(|&speed| check(&format!("speed {speed:?}"), &speed_name(speed), None)).collect();
        let fullscreen = check("fullscreen", "&Fullscreen", Some("F11"));
        let layouts: Vec<CheckMenuItem> = Screens::ALL.iter().map(|layout| check(&format!("layout {layout:?}"), layout.name(), None)).collect();
        let swap = check("swap", "&Swap Screens", Some("F10"));
        let fps = check("fps", "Show F&PS", None);
        let texture_packs = check("texture packs", "&Use texture packs (applies after restart)", None);

        let speed_menu = Submenu::with_items("&Speed Limit", true, &speeds.iter().map(|item| item as &dyn muda::IsMenuItem).collect::<Vec<_>>())?;
        let layout_menu = Submenu::with_items("Screen &Layout", true, &layouts.iter().map(|item| item as &dyn muda::IsMenuItem).collect::<Vec<_>>())?;
        layout_menu.append_items(&[&PredefinedMenuItem::separator(), &item("next layout", "&Next layout", Some("F9"))])?;
        let about = AboutMetadata {
            name: Some("RE:DEX".to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            comments: Some(
                "A native PC port of Pokemon X by static recompilation.\n\nIts window, menus, input and sound are ported from Zakuro by fearkov (MIT license).".to_owned(),
            ),
            website: Some(GITHUB.to_owned()),
            ..Default::default()
        };
        let disabled = |text: &str| MenuItem::new(text, false, None);
        let menu = Menu::with_items(&[
            &Submenu::with_items(
                "&File",
                true,
                &[&item("saves", "Open &save folder", None), &item("logs", "Open &log folder", None), &PredefinedMenuItem::separator(), &item("exit", "E&xit", Some("Alt+F4"))],
            )?,
            &Submenu::with_items(
                "&Emulation",
                true,
                &[&pause, &item("restart", "&Restart", None), &PredefinedMenuItem::separator(), &speed_menu, &PredefinedMenuItem::separator(), &item("configure", "&Configure...", None)],
            )?,
            &Submenu::with_items(
                "&View",
                true,
                &[&fullscreen, &PredefinedMenuItem::separator(), &layout_menu, &swap, &item("rotate", "&Rotate Clockwise", None), &PredefinedMenuItem::separator(), &fps],
            )?,
            &Submenu::with_items(
                "&Tools",
                true,
                &[
                    &item("screenshot", "Capture &Screenshot", Some("F12")),
                    &disabled("&Cheats... (coming later)"),
                    &Submenu::with_items("&Texture Packs", true, &[&texture_packs])?,
                ],
            )?,
            &Submenu::with_items(
                "&Multiplayer",
                true,
                &[&item("browse", "&Browse servers...", None), &PredefinedMenuItem::separator(), &item("players", "&Players nearby", None), &item("whispers", "Show &whispers", None)],
            )?,
            &Submenu::with_items("&Help", true, &[&PredefinedMenuItem::about(Some("&About RE:DEX"), Some(about)), &item("github", "Open &GitHub page", None)])?,
        ])?;
        Ok(MenuBar { menu, pause, speeds, fullscreen, layouts, swap, fps, texture_packs })
    }

    /// puts the bar on the window, dark or light as Windows is
    pub fn attach(&self, window: &winit::window::Window) -> Result<(), String> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = window.window_handle().map_err(|error| error.to_string())?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else { return Err("not a Win32 window".into()) };
        // safe: a live HWND, and the thread that owns it (see modal)
        let thread = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(handle.hwnd.get() as _, std::ptr::null_mut()) };
        WINDOW_THREAD.store(thread, Relaxed);
        // safe bc the HWND is winit's live window, which outlives the menu (App drops the bar first)
        unsafe { self.menu.init_for_hwnd_with_theme(handle.hwnd.get(), muda::MenuTheme::Auto) }.map_err(|error| error.to_string())
    }

    /// dark or light like the menus, or as Windows is
    pub fn set_theme(&self, window: &winit::window::Window, dark: Option<bool>) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = window.window_handle() else { return };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
        let theme = match dark {
            Some(true) => muda::MenuTheme::Dark,
            Some(false) => muda::MenuTheme::Light,
            None => muda::MenuTheme::Auto,
        };
        // safe bc it's the window the bar was put on
        let _ = unsafe { self.menu.set_theme_for_hwnd(handle.hwnd.get(), theme) };
    }

    /// shows or hides the bar, it goes away in fullscreen
    pub fn show(&self, window: &winit::window::Window, visible: bool) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = window.window_handle() else { return };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
        let hwnd = handle.hwnd.get();
        // safe bc it's the window the bar was put on
        let _ = unsafe { if visible { self.menu.show_for_hwnd(hwnd) } else { self.menu.hide_for_hwnd(hwnd) } };
    }

    /// the items clicked since the last call
    pub fn commands() -> Vec<Command> {
        std::iter::from_fn(|| MenuEvent::receiver().try_recv().ok()).filter_map(|event| command(event.id.as_ref())).collect()
    }

    /// ticks what's on, after the settings or the window changed
    pub fn sync(&self, settings: &Settings, paused: bool, fullscreen: bool) {
        self.pause.set_checked(paused);
        let speed = settings.limit_speed.then_some(settings.speed_limit);
        for (item, &offered) in self.speeds.iter().zip(&SPEEDS) {
            item.set_checked(offered == speed);
        }
        self.fullscreen.set_checked(fullscreen);
        for (item, &layout) in self.layouts.iter().zip(&Screens::ALL) {
            item.set_checked(layout == settings.layout);
        }
        self.swap.set_checked(settings.swap_screens);
        self.fps.set_checked(settings.show_fps);
        self.texture_packs.set_checked(settings.texture_packs);
    }
}

/// elsewhere there's no native bar yet (muda's GTK and macOS menus aren't wired up), the window's keys and Configure
/// still do everything it does
#[cfg(not(windows))]
pub struct MenuBar;

#[cfg(not(windows))]
impl MenuBar {
    pub fn new() -> Result<MenuBar, String> {
        Err("no native menu bar on this system".to_owned())
    }

    pub fn attach(&self, _window: &winit::window::Window) -> Result<(), String> {
        Ok(())
    }

    pub fn set_theme(&self, _window: &winit::window::Window, _dark: Option<bool>) {}

    pub fn show(&self, _window: &winit::window::Window, _visible: bool) {}

    pub fn commands() -> Vec<Command> {
        Vec::new()
    }

    pub fn sync(&self, _settings: &Settings, _paused: bool, _fullscreen: bool) {}
}

/// the thread the bar's window runs on, 0 before attach (see modal)
#[cfg_attr(not(windows), allow(dead_code))]
static WINDOW_THREAD: AtomicU32 = AtomicU32::new(0);
/// modal said yes since the window last asked modal_ended
static MODAL_SEEN: AtomicBool = AtomicBool::new(false);

/// a menu of the window's is open, or it's being moved or sized: Windows runs its own message loop meanwhile, the
/// window's steps stop and keys let go never reach it, so the game (whose thread asks, it runs on) gets no input
pub fn modal() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetGUIThreadInfo, GUITHREADINFO, GUI_INMENUMODE, GUI_INMOVESIZE};
        let thread = WINDOW_THREAD.load(Relaxed);
        // safe: a plain struct filled in, for a thread id (a gone thread just fails)
        let mut info: GUITHREADINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if thread != 0 && unsafe { GetGUIThreadInfo(thread, &mut info) } != 0 && info.flags & (GUI_INMENUMODE | GUI_INMOVESIZE) != 0 {
            MODAL_SEEN.store(true, Relaxed);
            return true;
        }
    }
    false
}

/// a modal loop (see modal) happened since the last call: the keys the window thinks are down may have been let go
pub fn modal_ended() -> bool {
    MODAL_SEEN.swap(false, Relaxed)
}

pub const GITHUB: &str = "https://github.com/Recoi1/redex";

/// what the item w/ this id asks for
#[cfg_attr(not(windows), allow(dead_code))]
fn command(id: &str) -> Option<Command> {
    Some(match id {
        "saves" => Command::OpenSaves,
        "logs" => Command::OpenLogs,
        "exit" => Command::Exit,
        "pause" => Command::Pause,
        "restart" => Command::Restart,
        "configure" => Command::Configure,
        "fullscreen" => Command::Fullscreen,
        "swap" => Command::Swap,
        "rotate" => Command::Rotate,
        "fps" => Command::ShowFps,
        "screenshot" => Command::Screenshot,
        "texture packs" => Command::TexturePacks,
        "github" => Command::GitHub,
        "next layout" => Command::NextLayout,
        "browse" => Command::Browse,
        "players" => Command::Players,
        "whispers" => Command::Whispers,
        _ => {
            if let Some(speed) = SPEEDS.iter().find(|speed| id == format!("speed {speed:?}")) {
                Command::Speed(*speed)
            } else {
                Command::Layout(*Screens::ALL.iter().find(|layout| id == format!("layout {layout:?}"))?)
            }
        }
    })
}
