//! Steam on the player's side. Online, the player is their SteamID64: the Steam client must be running
//! and logged on, and every connection to the world server proves it with a fresh auth session ticket (Steamworks app
//! 480, Spacewar). A world server address `steam:<steamid64>` is reached over Steam's relays (`connect`). Needs
//! `steam_api64.dll` next to redex.exe (the build puts it there); redex.exe delay-loads it (xr-runtime/build.rs), so
//! without it offline play still starts and `init` says "Start Steam to play online".
//!
//! `XR_NO_STEAM=1` makes `init` act as if Steam were closed (tests take that path without closing the player's Steam).
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use steamworks::networking_sockets::NetConnection;
use steamworks::networking_types::{NetworkingConfigEntry, NetworkingConfigValue, NetworkingConnectionState, NetworkingIdentity};
use steamworks::{AuthTicket, CallbackResult, Client, SteamId};

/// Spacewar: Valve's public test app, which every Steam account can use.
pub const APP_ID: u32 = 480;
/// What the player is told when Steam is not there.
pub const START_STEAM: &str = "Start Steam to play online";

#[derive(Clone)]
pub struct Steam {
    client: Client,
    /// What Steam's callbacks said, whichever thread drained them (the net thread and the window both pump): the
    /// answers to our ticket requests and the "Join game" connect strings. Its lock also keeps two threads from
    /// running Steam's callbacks at once (the server list's callbacks are not thread-safe).
    inbox: Arc<Mutex<Inbox>>,
}

#[derive(Default)]
struct Inbox {
    tickets: Vec<(AuthTicket, Result<(), String>)>,
    joins: Vec<String>,
    /// Steam's gamepad text input was closed: what was typed (None: cancelled)
    typed: Option<Option<steamworks::GamepadTextInputDismissed>>,
}

/// A world server in Steam's server list.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    /// its SteamID: joined as `steam:<id>`
    pub id: u64,
    /// empty when Steam could not reach its query port (UDP)
    pub name: String,
    /// None when Steam had no answer from the server itself (the master's count is not trusted then)
    pub players: Option<u32>,
    pub max: Option<u32>,
    /// None when Steam had no answer from the server itself
    pub ping: Option<u32>,
    pub tags: String,
}

/// The game tags a world server of this protocol version carries (`redex,v4,<region>`): what the list asks Steam for.
pub fn wanted_tags() -> String {
    format!("redex,v{}", redex_proto::VERSION)
}

/// A listing is ours and speaks our protocol: its tags hold `redex` and `v<VERSION>` as whole tags (Steam's
/// `gametagsand` filter is checked again here: another app-480 game could carry any tags).
/// A server's name as listed: anyone can name a server, so it is cleaned as player names are (control, bidi and
/// zero-width characters out) and cut to Steam's 63 characters.
pub fn shown_name(raw: &str) -> String {
    crate::client::clean(raw, 63)
}

pub fn listed(tags: &str) -> bool {
    let has = |t: &str| tags.split(',').any(|x| x.trim() == t);
    has("redex") && has(&format!("v{}", redex_proto::VERSION))
}

/// One query of Steam's internet server list for app 480 under our tags. Its results come in as Steam answers
/// (through `pump`); `done` once Steam finished.
pub struct ServerQuery {
    request: Arc<Mutex<steamworks::ServerListRequest>>,
    found: Arc<Mutex<(Vec<Listed>, bool)>>,
    /// Steam's callback lock (`Steam::inbox`): released under it, never while another thread runs the callbacks
    inbox: Arc<Mutex<Inbox>>,
}

impl ServerQuery {
    /// What came in so far, and whether Steam is done.
    pub fn results(&self) -> (Vec<Listed>, bool) {
        self.found.lock().map(|f| f.clone()).unwrap_or_default()
    }
}

impl Drop for ServerQuery {
    fn drop(&mut self) {
        // shortcut: a request still refreshing cannot be released by the wrapper; it stays until the process ends
        // (one per Refresh pressed mid-query), upgrade if that ever adds up
        let _no_callbacks_meanwhile = self.inbox.lock();
        if let Ok(mut r) = self.request.lock() {
            let _ = r.release();
        }
    }
}

impl Steam {
    /// Connects to the running Steam client. Err (a message for the player) when Steam is closed or offline.
    pub fn init() -> Result<Steam, String> {
        if std::env::var("XR_NO_STEAM").is_ok_and(|v| v == "1") {
            return Err(format!("{START_STEAM} (XR_NO_STEAM=1: acting as if Steam were closed)"));
        }
        // Valve's DLLs print the player's SteamID to the console as they load and start (their C runtime takes the std
        // handles when it loads): keep that off. The first Steam call would load the delay-loaded DLL and abort the
        // process if it were missing, so it is loaded (and checked) here first.
        let client = quiet(|| dll_loads().then(|| Client::init_app(APP_ID)))
            .ok_or_else(|| format!("{START_STEAM} (steam_api64.dll is missing next to redex.exe)"))?
            .map_err(|e| format!("{START_STEAM} (Steam: {e})"))?;
        if !client.user().logged_on() {
            return Err(format!("{START_STEAM}: Steam is running but offline"));
        }
        Ok(Steam { client, inbox: Arc::default() })
    }

    /// Runs Steam's callbacks and keeps what we use; without `wait` it does nothing while another thread runs them.
    fn drain(&self, wait: bool) {
        let got = if wait { self.inbox.lock().ok() } else { self.inbox.try_lock().ok() };
        let Some(mut inbox) = got else { return };
        self.client.process_callbacks(|cb| match cb {
            CallbackResult::AuthSessionTicketResponse(r) => inbox.tickets.push((r.ticket, r.result.map_err(|e| format!("{e:?}")))),
            CallbackResult::GameRichPresenceJoinRequested(r) => inbox.joins.push(r.connect),
            CallbackResult::GamepadTextInputDismissed(d) => inbox.typed = Some(d.submitted_text_len.is_some().then_some(d)),
            _ => {}
        });
    }

    /// The "Join game" requests from the Steam friends list since the last call (their connect strings).
    pub fn take_joins(&self) -> Vec<String> {
        self.drain(false);
        self.inbox.try_lock().map(|mut i| std::mem::take(&mut i.joins)).unwrap_or_default()
    }

    /// Opens Steam's gamepad text input (Big Picture's on-screen keyboard), for typing with a controller alone. False
    /// when Steam cannot show it here (no overlay); the text comes from `take_typed`.
    pub fn type_with_gamepad(&self, prompt: &str, max: u32, text: &str) -> bool {
        use steamworks::{GamepadTextInputLineMode, GamepadTextInputMode};
        let utils = self.client.utils();
        utils.is_overlay_enabled() && utils.show_gamepad_text_input(GamepadTextInputMode::Normal, GamepadTextInputLineMode::SingleLine, prompt, max, Some(text), |_| {})
    }

    /// The gamepad text input was closed since the last call: Some(the text) when submitted, Some(None) when cancelled.
    pub fn take_typed(&self) -> Option<Option<String>> {
        self.drain(false);
        let typed = self.inbox.try_lock().ok()?.typed.take()?;
        Some(typed.and_then(|d| self.client.utils().get_entered_gamepad_text_input(&d)))
    }

    /// What friends see: `status` in the friends list, and `connect` (`steam:<server id>`), which makes "Join game"
    /// work; None clears it. The `redex` key (the protocol version) is always set: it is what makes a friend "in
    /// RE:DEX" (crate::friends), online or offline.
    pub fn set_presence(&self, status: Option<&str>, connect: Option<&str>) {
        let f = self.client.friends();
        f.set_rich_presence(crate::friends::PRESENCE_KEY, Some(&redex_proto::VERSION.to_string()));
        f.set_rich_presence("status", status);
        f.set_rich_presence("connect", connect);
    }

    /// Opens Steam's overlay on "add friend" for this account. False when the overlay is not there (redex not started
    /// by Steam, or the overlay turned off).
    pub fn add_friend(&self, id: u64) -> bool {
        if !self.client.utils().is_overlay_enabled() {
            return false;
        }
        self.client.friends().activate_game_overlay_to_user("friendadd", SteamId::from_raw(id));
        true
    }

    /// Asks Steam's master server for the world servers of our protocol version (app 480). Err when Steam refused.
    /// Every server the master lists counts, answered or not: a server whose UDP query port Steam cannot reach still
    /// has its SteamID and tags from the master (no name, no ping).
    pub fn query_servers(&self) -> io::Result<ServerQuery> {
        type Found = Arc<Mutex<(Vec<Listed>, bool)>>;
        let found: Found = Arc::default();
        let keep = |found: &Found, r: &steamworks::ServerListRequest, i: i32| {
            let Ok(s) = r.get_server_details(i) else { return };
            if !listed(&s.tags) {
                return;
            }
            let l = Listed {
                id: s.steamid,
                name: shown_name(&s.server_name),
                players: s.successful_response.then(|| s.players.max(0) as u32),
                max: s.successful_response.then(|| s.max_players.max(0) as u32),
                ping: s.successful_response.then(|| s.ping.as_millis() as u32),
                tags: s.tags,
            };
            if let Ok(mut f) = found.lock() {
                f.0.retain(|x| x.id != l.id);
                f.0.push(l);
            }
        };
        let (answered, silent, done) = (found.clone(), found.clone(), found.clone());
        let cbs = steamworks::ServerListCallbacks::new(
            Box::new(move |req, i| {
                if let Ok(r) = req.lock() {
                    keep(&answered, &r, i);
                }
            }),
            Box::new(move |req, i| {
                if let Ok(r) = req.lock() {
                    keep(&silent, &r, i);
                }
            }),
            Box::new(move |req, _| {
                if let Ok(r) = req.lock() {
                    (0..r.get_server_count().unwrap_or(0)).for_each(|i| keep(&done, &r, i));
                }
                if let Ok(mut f) = done.lock() {
                    f.1 = true;
                }
            }),
        );
        let tags = wanted_tags();
        let filters = HashMap::from([("gametagsand", tags.as_str())]);
        let _no_callbacks_meanwhile = self.inbox.lock();
        let request = self.client.matchmaking_servers().internet_server_list(APP_ID, &filters, cbs).map_err(|()| io::Error::other("Steam refused the server list query"))?;
        Ok(ServerQuery { request, found, inbox: self.inbox.clone() })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn steam_id(&self) -> u64 {
        self.client.user().steam_id().raw()
    }

    /// A new ticket once Steam has it ready (up to `wait`). `cancel` it when its connection ends, so it cannot be used
    /// again. With the world server's SteamID (`steam:` addresses) the ticket is bound to it: Steam refuses it on any
    /// other server. Over TCP the client does not know the server's SteamID, so the ticket is unbound and only the
    /// server's replay memory stops a reuse; fine for our own server on loopback/LAN, a blocker before any third-party
    /// TCP server exists.
    pub fn ticket(&self, server: Option<u64>, wait: Duration) -> io::Result<(AuthTicket, Vec<u8>)> {
        let user = self.client.user();
        let to = server.map_or_else(NetworkingIdentity::new, |s| NetworkingIdentity::new_steam_id(SteamId::from_raw(s)));
        let (t, bytes) = user.authentication_session_ticket(to);
        let until = Instant::now() + wait;
        loop {
            self.drain(true);
            let answer = self.inbox.lock().ok().and_then(|mut i| {
                let at = i.tickets.iter().position(|(x, _)| *x == t)?;
                Some(i.tickets.swap_remove(at).1)
            });
            match answer {
                Some(Ok(())) if !bytes.is_empty() => return Ok((t, bytes)),
                Some(r) => {
                    user.cancel_authentication_ticket(t);
                    return Err(io::Error::other(format!("Steam made no ticket ({r:?})")));
                }
                None if Instant::now() > until => {
                    user.cancel_authentication_ticket(t);
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "Steam made no ticket in time"));
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    pub fn cancel(&self, t: AuthTicket) {
        self.client.user().cancel_authentication_ticket(t);
    }

    /// Connects to a world server by its SteamID over Steam's relays only (ICE off: no IP is exchanged either way).
    pub fn connect(&self, server: u64, wait: Duration) -> io::Result<NetConnection> {
        self.client.networking_utils().init_relay_network_access();
        let relay_only = [NetworkingConfigEntry::new_int32(NetworkingConfigValue::P2PTransportICEEnable, 0)];
        let conn = self
            .client
            .networking_sockets()
            .connect_p2p(NetworkingIdentity::new_steam_id(SteamId::from_raw(server)), 0, relay_only)
            .map_err(|_| io::Error::other("Steam refused to start the P2P connection"))?;
        let until = Instant::now() + wait;
        loop {
            self.pump();
            match self.state(&conn) {
                Some(NetworkingConnectionState::Connected) => return Ok(conn),
                Some(NetworkingConnectionState::Connecting | NetworkingConnectionState::FindingRoute) if Instant::now() < until => {}
                s => return Err(io::Error::new(io::ErrorKind::ConnectionRefused, format!("no Steam P2P connection to the world server ({s:?})"))),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn state(&self, conn: &NetConnection) -> Option<NetworkingConnectionState> {
        self.client.networking_sockets().get_connection_info(conn).ok().and_then(|i| i.state().ok())
    }

    /// The P2P connection is still up (or coming up).
    pub fn live(&self, conn: &NetConnection) -> bool {
        matches!(self.state(conn), Some(NetworkingConnectionState::Connecting | NetworkingConnectionState::FindingRoute | NetworkingConnectionState::Connected))
    }

    /// Drains Steam's callbacks (see `inbox`); call it now and then so they do not pile up. Never waits.
    pub fn pump(&self) {
        self.drain(false);
    }
}

/// steam_api64.dll can be loaded (beside redex.exe or on the DLL path). It stays loaded: Steam is used next.
fn dll_loads() -> bool {
    let name: Vec<u16> = "steam_api64.dll\0".encode_utf16().collect();
    // SAFETY: a NUL-terminated name; loading the DLL runs only its own initialisation
    !unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(name.as_ptr()) }.is_null()
}

/// Runs `f` with the process's stdout and stderr pointed at NUL (Valve's DLLs pick them up as they load), then restores
/// them.
fn quiet<T>(f: impl FnOnce() -> T) -> T {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    let Ok(nul) = std::fs::OpenOptions::new().write(true).open("NUL") else { return f() };
    // never closed: a DLL that kept this handle must not later write into whatever reuses its number
    let nul = nul.into_raw_handle();
    // SAFETY: swaps the process's stdout and stderr handles for a valid one and puts the originals back
    unsafe {
        let (out, err) = (GetStdHandle(STD_OUTPUT_HANDLE), GetStdHandle(STD_ERROR_HANDLE));
        SetStdHandle(STD_OUTPUT_HANDLE, nul as _);
        SetStdHandle(STD_ERROR_HANDLE, nul as _);
        let r = f();
        SetStdHandle(STD_OUTPUT_HANDLE, out);
        SetStdHandle(STD_ERROR_HANDLE, err);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only listings tagged `redex` and this protocol's `v<VERSION>`, as whole tags, are shown.
    #[test]
    fn a_server_name_is_cleaned_like_a_player_name() {
        assert_eq!(shown_name("My\u{202E}world\n\u{200B}!"), "Myworld!");
        assert_eq!(shown_name(&"x".repeat(100)).chars().count(), 63);
    }

    #[test]
    fn the_list_keeps_our_tags_only() {
        assert_eq!(wanted_tags(), format!("redex,v{}", redex_proto::VERSION));
        assert!(listed(&format!("redex,v{},kalos", redex_proto::VERSION)));
        assert!(listed(&format!("kalos, v{} ,redex", redex_proto::VERSION)));
        assert!(!listed("redex,v3,kalos"), "another protocol version");
        assert!(!listed(&format!("redexx,v{}", redex_proto::VERSION)), "a tag that merely starts the same");
        assert!(!listed(&format!("v{}", redex_proto::VERSION)));
        assert!(!listed(""));
    }
}
