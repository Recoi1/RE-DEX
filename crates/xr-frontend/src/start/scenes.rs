//! Test hooks for screenshots (never in a release: make-release builds without test-hooks). `XR_TEST_SCENE` opens the
//! start screen on one state: `signed-in`, `creator`, `mods`, `storage`, `configure`, or one of the in-game panels
//! drawn over a still of the game (`lobby`, `lobby-host`, `players`, `friends`, `chat`; the still's PNG in
//! `XR_TEST_SCENE_BG`), with made-up players. Nothing here is saved or sent.
use egui::{pos2, Color32, Rect, TextureHandle, Ui};

use super::account::{Servers, Session};
use super::{characters, Panel, Start};
use crate::menus::{Menus, Status};
use crate::online::{Online, View};
use crate::settings::Orientation;

/// an in-game panel over the game's picture
pub(super) struct Overlay {
    online: Online,
    view: View,
    still: Option<TextureHandle>,
    loaded: bool,
    /// the toast scene: when its toasts were last put up (they are put up again before they go)
    toasts: Option<std::time::Instant>,
}

impl Overlay {
    pub(super) fn ui(&mut self, ui: &mut Ui, menus: &mut Menus) {
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, Color32::BLACK);
        if !self.loaded {
            self.loaded = true;
            self.still = std::env::var_os("XR_TEST_SCENE_BG")
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| super::load_png(ui.ctx(), "scene-still", &b, 4096, 0, 1.0));
        }
        if let Some(t) = &self.still {
            // the game's screens, fitted to the window's height like the presenter's default layout
            let [w, h] = t.size().map(|v| v as f32);
            let scale = (full.height() / h).min(full.width() / w);
            let rect = Rect::from_center_size(full.center(), egui::vec2(w, h) * scale);
            ui.painter().image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        if self.toasts.is_some_and(|t| t.elapsed().as_secs_f32() > 1.2) {
            menus.toast("Room code K7M-Q2X copied");
            menus.toast("Ava joined your lobby");
            menus.toast("Can't host a lobby with test options on");
            self.toasts = Some(std::time::Instant::now());
        }
        self.online.ui(ui.ctx(), &self.view, None, Orientation::North, false, true, menus);
        menus.game(ui.ctx(), &Status { fps: None, playing: true, paused: false, fast: false });
    }
}

impl Start {
    /// puts the screen in the state `XR_TEST_SCENE` names
    pub(super) fn scene(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        // no account service is asked: signed out, the server answering
        self.session = Session::idle(self.data.clone(), Servers::of(&self.settings));
        match name {
            "signed-out" => {}
            "signed-in" | "account" => {
                let me = xr_net::account::Me { account_id: 7, public_id: 4242, username: "Mira".into(), email_verified: true };
                self.session.assume(Some(me), None);
                // the account's characters: this PC's, as if they were the account's own
                self.session.characters = self.store.characters.iter().take(2).cloned().map(|mut c| {
                    let from = super::preview::card_path(&self.data, &c);
                    c.id ^= 0x5eed_0000_0000_0000;
                    // the same picture for the made-up online twin (test data only)
                    let to = super::preview::card_path(&self.data, &c);
                    let _ = to.parent().map(std::fs::create_dir_all).and_then(|_| std::fs::copy(&from, &to).ok());
                    c
                }).collect::<Vec<characters::Character>>();
                (self.online_view, self.sign_in_tab) = (true, false);
                self.character = self.session.characters.first().map(|c| c.id);
                if name == "account" {
                    self.panel = Some(Panel::Account);
                }
            }
            "sign-in" => self.panel = Some(Panel::SignIn),
            "creator" => self.creator_request = Some(None),
            "mods" => self.mods = Some(super::mods::Mods::new(self.data.clone())),
            "storage" => self.storage = super::storage::Storage::Asking,
            "configure" => self.menus.configure_open = true,
            // the controller's focus on Play (the focus list's last fixed item before the account pill)
            "focus" => (self.show_focus, self.focus) = (true, self.library.games.len() + 15),
            "remove" => self.removing = self.character,
            "error" => self.error = Some("We couldn't read that file. Pick a decrypted .cci or .3ds dump of Pokémon X or Y.".into()),
            other => {
                let mut online = Online::default();
                online.motion = false;
                let view = online.demo(other);
                let toasts = (other == "toast").then(|| std::time::Instant::now() - std::time::Duration::from_secs(2));
                self.overlay = Some(Overlay { online, view, still: None, loaded: false, toasts });
            }
        }
    }
}
