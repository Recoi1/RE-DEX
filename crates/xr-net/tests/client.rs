//! xr-net's world client against an in-process redex-world server (and a scripted fake one for the cooldown answer).
use redex_proto::{Auth, Character, Decoder, Msg, TITLE_OR, TITLE_X, TITLE_Y, VERSION};
use redex_world::transport::TcpTransport;
use redex_world::{Config, Server};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};
use xr_net::{Client, Local, Options};

fn local(map: u32, x: f32) -> Local {
    Local { map, pos: [x, 0.0, 0.0], facing: 1, mode: 0, appearance: None }
}

fn fast(addr: SocketAddr, id: u128, name: &str) -> Options {
    // odd ids play X, even ones Y: both are Kalos, so they meet
    let title = if id % 2 == 1 { TITLE_X } else { TITLE_Y };
    Options { backoff: Duration::from_millis(50), backoff_max: Duration::from_millis(200), ..Options::new(addr.to_string(), Auth::DevId(id), title, name) }
}

fn server_on(addr: &str) -> (Server, SocketAddr) {
    // the port of a server that just stopped can take a moment to free up
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpTransport::bind(addr) {
            Ok(t) => {
                let a = t.local_addr().unwrap();
                return (Server::start(Config { dev: true, ..Config::default() }, t), a); // dev ids
            }
            Err(e) if Instant::now() < end => {
                println!("bind {addr}: {e}; retrying");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("bind {addr}: {e}"),
        }
    }
}

/// Runs both clients' frames at about 60 Hz until `done` holds; panics after `wait`.
fn until(a: &mut Client, la: Local, b: &mut Client, lb: Local, wait: Duration, what: &str, done: impl Fn(&Client, &Client) -> bool) {
    let end = Instant::now() + wait;
    while !done(a, b) {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        a.frame(Some(la));
        b.frame(Some(lb));
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// The track's newest x for the only peer, if any.
fn seen_x(c: &Client) -> Option<f32> {
    c.peers.values().next().and_then(|r| r.track.back()).map(|(_, p)| p.x)
}

/// A refusal for good says why (the window shows "Cannot join: <reason>", e.g. a world's allow list), not "couldn't reach".
#[test]
fn a_lasting_refusal_keeps_its_reason() {
    let t = TcpTransport::bind("127.0.0.1:0").unwrap();
    let addr = t.local_addr().unwrap();
    let _srv = Server::start(Config::default(), t); // no --dev: a dev id is refused (AUTH, lasting)
    let mut c = Client::start(fast(addr, 7, "Refused")).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !c.stopped {
        assert!(Instant::now() < end, "never refused");
        c.frame(None);
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(c.refused.as_deref().is_some_and(|r| !r.is_empty()), "{:?}", c.refused);
    assert!(!c.online);
}

#[test]
fn two_clients_exchange_positions() {
    let (_srv, addr) = server_on("127.0.0.1:0");
    let mut a = Client::start(fast(addr, 1, "Ash")).unwrap();
    let long = "Brock".repeat(8); // over the 16-character name cap: cut, not refused
    let mut b = Client::start(fast(addr, 2, &long)).unwrap();
    let w = Duration::from_secs(5);
    until(&mut a, local(10, 1.0), &mut b, local(10, 50.0), w, "both on map 10, seeing each other", |a, b| {
        a.room.is_some_and(|r| r.0 == 10) && b.room.is_some_and(|r| r.0 == 10) && seen_x(a) == Some(50.0) && seen_x(b) == Some(1.0)
    });
    assert_eq!(b.peers.values().next().unwrap().name, "Ash");
    assert_eq!((b.peers.values().next().unwrap().title, a.peers.values().next().unwrap().title), (TITLE_X, TITLE_Y), "an X and a Y player meet");
    assert_eq!(a.peers.values().next().unwrap().name, long[..16]);
    // A walks: B's track follows to the last step
    for x in 2..=20 {
        a.frame(Some(local(10, x as f32)));
        b.frame(Some(local(10, 50.0)));
        std::thread::sleep(Duration::from_millis(16));
    }
    until(&mut a, local(10, 20.0), &mut b, local(10, 50.0), w, "B sees A's last step", |_, b| seen_x(b) == Some(20.0));
    let track = &b.peers.values().next().unwrap().track;
    assert!(track.len() >= 3, "a walk arrives as several positions: {track:?}");
    assert!(track.iter().all(|(_, p)| p.map == 10 && p.game == redex_proto::GEN_X));
    // A goes to another map: B loses it, A sees nobody
    until(&mut a, local(11, 0.0), &mut b, local(10, 50.0), w, "A off B's map", |a, b| {
        a.room.is_some_and(|r| r.0 == 11) && b.peers.is_empty() && a.peers.is_empty()
    });
}

/// the look goes in the Hello (Options.appearance), and a changed one the game reads (Local.appearance)
/// goes out as an AppearanceUpdate: the other player sees the first at once and the second after the server's 1 s gap.
/// A look whose outfit fails the game's ranges still carries the gender: the other player shows the plain look.
#[test]
fn a_changed_look_reaches_the_other_player() {
    use xr_net::x::field::Appearance;
    let boy = Appearance { gender: 0, fashion: [0x88, 0xB9, 0x19, 0x05, 0xD0, 0x09, 0x32, 0x03, 0x11, 0, 0, 0, 0, 0, 0, 0] };
    let red = Appearance { gender: 0, fashion: [0x48, 0x64, 0x09, 0x01, 0x01, 0xCB, 0x08, 0x03, 0x41, 0, 0, 0, 0, 0, 0, 0] };
    let (_srv, addr) = server_on("127.0.0.1:0");
    let mut a = Client::start(Options { appearance: boy.to_bytes().to_vec(), ..fast(addr, 21, "Ash") }).unwrap();
    let mut b = Client::start(fast(addr, 22, "Brock")).unwrap();
    let w = Duration::from_secs(10);
    let look = |c: &Client| c.peers.values().next().and_then(|r| r.appearance);
    let la = |a: Option<Appearance>| Local { appearance: a, ..local(5, 1.0) };
    until(&mut a, la(Some(boy)), &mut b, local(5, 2.0), w, "B sees A's Hello look", |_, b| look(b) == Some(boy));
    assert_eq!(b.peers.values().next().unwrap().appearance, Some(boy));
    until(&mut a, la(Some(red)), &mut b, local(5, 2.0), w, "B sees A's new look", |_, b| look(b) == Some(red));
    // an out-of-range outfit: the server drops it (the full Appearance::from_bytes check), B keeps A's last good look
    let mut bad = Appearance { gender: 1, ..red };
    bad.fashion[15] = 0xFF;
    let end = Instant::now() + Duration::from_millis(2500); // past the server's 1 s gap between relayed looks
    while Instant::now() < end {
        a.frame(Some(la(Some(bad))));
        b.frame(Some(local(5, 2.0)));
        std::thread::sleep(Duration::from_millis(16));
    }
    assert_eq!(look(&b), Some(red));
}

#[test]
fn reconnects_after_a_server_restart() {
    let (srv, addr) = server_on("127.0.0.1:0");
    let mut a = Client::start(fast(addr, 11, "Ash")).unwrap();
    let mut b = Client::start(fast(addr, 12, "Brock")).unwrap();
    let w = Duration::from_secs(10);
    until(&mut a, local(5, 1.0), &mut b, local(5, 2.0), w, "seeing each other", |a, b| seen_x(a) == Some(2.0) && seen_x(b) == Some(1.0));
    drop(srv);
    until(&mut a, local(5, 1.0), &mut b, local(5, 2.0), w, "peers cleared on the drop", |a, b| {
        !a.online && !b.online && a.peers.is_empty() && b.peers.is_empty() && a.room.is_none()
    });
    let (_srv, _) = server_on(&addr.to_string());
    // Hello and MapEnter again, by themselves: back on map 5 and seeing each other under new sessions
    until(&mut a, local(5, 3.0), &mut b, local(5, 4.0), w, "back after the restart", |a, b| {
        a.online && a.room.is_some_and(|r| r.0 == 5) && seen_x(a) == Some(4.0) && seen_x(b) == Some(3.0)
    });
    assert_eq!((a.peers.len(), b.peers.len()), (1, 1), "each sees the other once, no ghost of the old session");
}

/// A MapEnter inside the server's cooldown is answered with where the player still is; the client keeps that map
/// (it never assumes the move worked) and asks again after its gap.
#[test]
fn ignored_map_enter_is_retried() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let fake = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let mut d = Decoder::default();
        let mut next = |s: &mut TcpStream| loop {
            if let Some(m) = d.next().unwrap() {
                if !matches!(m, Msg::Pos(_) | Msg::Pong(_)) {
                    return (Instant::now(), m);
                }
                continue;
            }
            let mut b = [0; 256];
            match s.read(&mut b) {
                Ok(0) => panic!("client closed"),
                Ok(n) => d.push(&b[..n]),
                Err(_) => {}
            }
        };
        let send = |s: &mut TcpStream, m: Msg| s.write_all(&m.encode()).unwrap();
        assert!(matches!(next(&mut s).1, Msg::Hello { version: VERSION, auth: Auth::DevId(21), game: 6, title: TITLE_X, .. }));
        send(&mut s, Msg::Welcome { session: 1 });
        assert_eq!(next(&mut s).1, Msg::MapEnter { game: 6, map: 1 });
        send(&mut s, Msg::Entered { map: 1, channel: 0, repeat: false });
        let (t1, m) = next(&mut s);
        assert_eq!(m, Msg::MapEnter { game: 6, map: 2 });
        send(&mut s, Msg::Entered { map: 1, channel: 0, repeat: true }); // "ignored: you are still on map 1"
        let (t2, m) = next(&mut s);
        assert_eq!(m, Msg::MapEnter { game: 6, map: 2 }, "asked again");
        send(&mut s, Msg::Entered { map: 2, channel: 0, repeat: false });
        (t2 - t1, s)
    });
    let mut c = Client::start(Options { move_gap: Duration::from_millis(300), ..fast(addr, 21, "Ash") }).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while c.room != Some((1, 0)) {
        assert!(Instant::now() < end);
        c.frame(Some(local(1, 0.0)));
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut stayed = false;
    while c.room != Some((2, 0)) {
        assert!(Instant::now() < end, "never entered map 2");
        assert!(c.room == Some((1, 0)), "{:?}: a move counts only on its Entered", c.room);
        stayed = true;
        c.frame(Some(local(2, 0.0)));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(stayed);
    let (gap, _s) = fake.join().unwrap();
    assert!(gap >= Duration::from_millis(250), "retried after {gap:?}, before its gap");
}

/// The same against the real server: a map change right after entering lands inside its cooldown and still arrives.
#[test]
fn quick_map_change_lands_on_the_real_server() {
    let (_srv, addr) = server_on("127.0.0.1:0");
    let mut a = Client::start(Options { move_gap: Duration::from_millis(150), ..fast(addr, 31, "Ash") }).unwrap();
    let mut b = Client::start(fast(addr, 32, "Brock")).unwrap();
    let w = Duration::from_secs(5);
    until(&mut a, local(1, 0.0), &mut b, local(2, 0.0), w, "A on 1", |a, _| a.room.is_some_and(|r| r.0 == 1));
    let t = Instant::now();
    until(&mut a, local(2, 7.0), &mut b, local(2, 0.0), w, "A on 2, seen by B", |a, b| {
        a.room.is_some_and(|r| r.0 == 2) && seen_x(b) == Some(7.0)
    });
    // the first retry falls inside the server's 450 ms cooldown, so getting there takes at least that long
    assert!(t.elapsed() >= Duration::from_millis(400), "{:?}", t.elapsed());
    assert!(a.online);
}

/// The game thread never waits on the network: frame() stays as fast while the server is killed, while the client
/// reconnects against a closed port, and against a server that accepts and then never answers.
#[test]
fn frames_never_wait_on_the_network() {
    let (srv, addr) = server_on("127.0.0.1:0");
    let hole = TcpListener::bind("127.0.0.1:0").unwrap(); // accepts (the OS backlog) and never reads or writes
    let mut a = Client::start(fast(addr, 41, "Ash")).unwrap();
    let mut b = Client::start(fast(addr, 42, "Brock")).unwrap();
    let mut h = Client::start(fast(hole.local_addr().unwrap(), 43, "Hole")).unwrap();
    until(&mut a, local(3, 0.0), &mut b, local(3, 1.0), Duration::from_secs(5), "connected", |a, b| seen_x(a) == Some(1.0) && seen_x(b) == Some(0.0));
    let (mut before, mut after) = (Vec::new(), Vec::new());
    let mut srv = Some(srv);
    let start = Instant::now();
    let mut n = 0;
    while start.elapsed() < Duration::from_secs(8) {
        let killed = start.elapsed() > Duration::from_secs(2);
        if killed {
            srv.take(); // the server goes away mid-run
        }
        let t = Instant::now();
        n += 1;
        a.frame(Some(local(3, n as f32)));
        b.frame(Some(local(3, -(n as f32))));
        h.frame(Some(local(3, n as f32)));
        let d = t.elapsed();
        if killed { after.push(d) } else { before.push(d) }
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(!a.online && !h.online);
    let stats = |v: &mut Vec<Duration>| {
        v.sort();
        (v[v.len() / 2], v[v.len() * 99 / 100], *v.last().unwrap())
    };
    let (b50, b99, bmax) = stats(&mut before);
    let (a50, a99, amax) = stats(&mut after);
    println!("frame() for 3 clients, connected: p50 {b50:?} p99 {b99:?} max {bmax:?}; server killed: p50 {a50:?} p99 {a99:?} max {amax:?}");
    // a frame is 16.7 ms; the hook takes microseconds whatever the network does (p99: a lone max is the scheduler's)
    assert!(a99 < Duration::from_millis(1) && b99 < Duration::from_millis(1), "{b99:?} {a99:?}");
    drop(hole);
}

#[test]
fn a_title_without_a_world_is_refused_before_connecting() {
    let e = Client::start(Options::new("127.0.0.1:1", Auth::DevId(1), 0x0004_0000_0012_3400, "x")).err().unwrap();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    // Omega Ruby is known (Hoenn) though the server does not let it in yet: the client starts, the server says no
    assert!(Client::start(Options::new("127.0.0.1:1", Auth::DevId(1), TITLE_OR, "x")).is_ok());
}

/// The game stops draining (a long pause) while a peer walks: past the event cap the client drops the backlog, and
/// once the game drains again it gets a clean slate (no peers) and then everyone re-announced by a fresh MapEnter.
#[test]
fn a_backlog_resyncs_instead_of_growing() {
    let (_srv, addr) = server_on("127.0.0.1:0");
    let mut a = Client::start(Options { events: 8, ..fast(addr, 51, "Ash") }).unwrap();
    let mut b = Client::start(fast(addr, 52, "Brock")).unwrap();
    let w = Duration::from_secs(5);
    until(&mut a, local(4, 0.0), &mut b, local(4, 0.0), w, "seeing each other", |a, b| !a.peers.is_empty() && !b.peers.is_empty());
    // A's game "pauses" (no frame() for 3 s) while B walks: far more than 8 events wait for A
    let t = Instant::now();
    let mut x = 0.0;
    while t.elapsed() < Duration::from_secs(3) {
        x += 1.0;
        b.frame(Some(local(4, x)));
        std::thread::sleep(Duration::from_millis(16));
    }
    until(&mut a, local(4, 0.0), &mut b, local(4, x), w, "B re-announced to A with its newest position", |a, _| seen_x(a) == Some(x));
    assert_eq!(a.peers.len(), 1);
    // replayed, the 3 s walk would fill the track (about 30 positions); re-announced, it starts again from B's newest
    let track = &a.peers.values().next().unwrap().track;
    assert!(track.len() <= 4, "the backlog was replayed: {} positions", track.len());
}

/// A whisper from X reaches the Y player on another map as text from X's name (cut to 120 characters, control
/// characters gone); a report reaches the server's log. Nothing goes before login.
#[test]
fn whispers_and_reports_reach_the_server() {
    let dir = std::env::temp_dir().join(format!("xr_net_whisper_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let t = TcpTransport::bind("127.0.0.1:0").unwrap();
    let addr = t.local_addr().unwrap();
    let _srv = Server::start(Config { dev: true, data: dir.clone(), ..Config::default() }, t);
    let mut a = Client::start(fast(addr, 1, "Ash")).unwrap();
    let mut b = Client::start(fast(addr, 2, "Serena")).unwrap();
    assert!(!a.whisper(0, "too early"), "not logged in yet");
    let w = Duration::from_secs(5);
    until(&mut a, local(10, 1.0), &mut b, local(10, 2.0), w, "both on map 10", |a, b| !a.peers.is_empty() && !b.peers.is_empty());
    let (sa, sb) = (*b.peers.keys().next().unwrap(), *a.peers.keys().next().unwrap());
    // on one map: delivered
    assert!(!a.whisper(sb, "   "), "an empty whisper is not sent");
    assert!(a.whisper(sb, &format!("hi\u{7}{}", "x".repeat(200))));
    until(&mut a, local(10, 1.0), &mut b, local(10, 2.0), w, "B hears A", |_, b| !b.whispers.is_empty());
    let got = b.whispers.pop_front().unwrap();
    assert_eq!((got.name.as_str(), got.text.chars().count()), ("Ash", redex_proto::WHISPER_CHARS));
    assert!(got.text.starts_with("hixxx"), "{:?}", got.text);
    // B walks off to another map: B's reply still reaches A (A whispered B lately)
    until(&mut a, local(10, 1.0), &mut b, local(11, 2.0), w, "B on map 11", |a, _| a.peers.is_empty());
    assert!(b.whisper(sa, "bye"));
    until(&mut a, local(10, 1.0), &mut b, local(11, 2.0), w, "A hears B's reply", |a, _| !a.whispers.is_empty());
    assert!(a.report(sb, 0, redex_proto::REPORT_CHAT, "rude"));
    assert!(!a.report(sb, 0, 9, "no such reason"));
    let log = dir.join("reports.jsonl");
    until(&mut a, local(10, 1.0), &mut b, local(11, 2.0), w, "the report written", |_, _| std::fs::read_to_string(&log).is_ok_and(|l| l.contains("\"note\":\"rude\"")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scripted remote player for checking the window's multiplayer UI by eye (names, the Players list, whispers),
/// without a second game: logs in to XR_PEER_ADDR (a --dev server) with a dev id as XR_PEER_NAME, playing Y, stands
/// at XR_PEER_POS (x,y,z) on map XR_PEER_MAP for XR_PEER_SECS, and whispers each line of XR_PEER_SAY (separated by
/// `|`) to the first player it meets, one every 4 s.
#[test]
#[ignore = "a manual helper: needs a running --dev world server and a game on the same map"]
fn scripted_peer() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
    let addr = var("XR_PEER_ADDR");
    let map: u32 = var("XR_PEER_MAP").parse().unwrap();
    let pos: Vec<f32> = var("XR_PEER_POS").split(',').map(|v| v.trim().parse().unwrap()).collect();
    let secs: u64 = var("XR_PEER_SECS").parse().unwrap();
    let say: Vec<String> = std::env::var("XR_PEER_SAY").unwrap_or_default().split('|').filter(|s| !s.is_empty()).map(str::to_owned).collect();
    let id = u128::from_le_bytes(std::array::from_fn(|i| (std::process::id() as u8).wrapping_add((i as u8).wrapping_mul(37))));
    let mut c = Client::start(Options::new(addr, Auth::DevId(id & !(1 << 127)), TITLE_Y, var("XR_PEER_NAME"))).unwrap();
    let me = Local { map, pos: [pos[0], pos[1], pos[2]], facing: 1, mode: 0, appearance: None };
    let end = Instant::now() + Duration::from_secs(secs);
    let (mut said, mut next) = (0, None::<Instant>);
    while Instant::now() < end {
        c.frame(Some(me));
        if let Some(&to) = c.peers.keys().next() {
            let due = *next.get_or_insert_with(|| Instant::now() + Duration::from_secs(6));
            if said < say.len() && Instant::now() >= due {
                println!("whisper to {to}: {} ({})", say[said], c.whisper(to, &say[said]));
                said += 1;
                next = Some(Instant::now() + Duration::from_secs(4));
            }
        }
        for w in c.whispers.drain(..) {
            println!("heard from {}: {}", w.name, w.text);
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

// ---- protocol v5: characters on the account ------------------------------------------------------------------------

/// A server keeping accounts in a fresh folder of its own (removed by the caller).
fn account_server(name: &str) -> (Server, SocketAddr, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("xr_net_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let t = TcpTransport::bind("127.0.0.1:0").unwrap();
    let addr = t.local_addr().unwrap();
    (Server::start(Config { dev: true, data: dir.clone(), ..Config::default() }, t), addr, dir)
}

/// The same player (one dev id) on X or on Y.
fn on(addr: SocketAddr, id: u128, title: u64) -> Options {
    Options { backoff: Duration::from_millis(50), backoff_max: Duration::from_millis(200), ..Options::new(addr.to_string(), Auth::DevId(id), title, "Ash") }
}

fn character(look: [u8; 19], changed: u64) -> Character {
    Character { id: 0x5E7E_4A00_0000_0077, name: "Serena".into(), look: look.to_vec(), created: 1, changed }
}

/// The new-game girl (third look) and the 100% save's boy, as the 19 wire bytes.
const GIRL: [u8; 19] = [6, 1, 1, 0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0];
const BOY: [u8; 19] = [6, 1, 0, 0x88, 0xB9, 0x19, 0x05, 0xD0, 0x09, 0x32, 0x03, 0x11, 0, 0, 0, 0, 0, 0, 0];

/// Runs one client's frames (off the field) until `done` holds; panics after 5 s.
fn wait(c: &mut Client, what: &str, done: impl Fn(&Client) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done(c) {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        c.frame(None);
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Done 4 and the X+Y pair: a character made offline on X is put to the account at the next login, and the same
/// player logging in on Y (another PC, no characters there) gets it in the account's list.
#[test]
fn an_offline_character_reaches_the_account() {
    let (srv, addr, dir) = account_server("offline_character");
    let c = character(GIRL, 5);
    let mut x = Client::start(Options { characters: vec![c.clone()], ..on(addr, 61, TITLE_X) }).unwrap();
    wait(&mut x, "X's character on the account", |x| x.accounts.iter().any(|l| l.contains(&c)));
    drop(x);
    let mut y = Client::start(on(addr, 61, TITLE_Y)).unwrap();
    wait(&mut y, "the account's list on Y", |y| !y.accounts.is_empty());
    assert_eq!(y.accounts.pop_front().unwrap(), [c], "the list after Welcome holds X's character");
    drop((y, srv));
    let _ = std::fs::remove_dir_all(&dir);
}

/// One character changed on two PCs: the account keeps the newest `changed` and sends it down to the PC with the
/// older copy; a change made there later (`put_character`) goes up and comes back as the account's.
#[test]
fn a_newer_copy_on_the_account_comes_down() {
    let (srv, addr, dir) = account_server("newer_copy");
    let (newer, older) = (character(GIRL, 20), character(BOY, 10));
    let mut a = Client::start(Options { characters: vec![newer.clone()], ..on(addr, 62, TITLE_X) }).unwrap();
    wait(&mut a, "A's copy on the account", |a| a.accounts.iter().any(|l| l.contains(&newer)));
    drop(a);
    let mut b = Client::start(Options { characters: vec![older.clone()], ..on(addr, 62, TITLE_Y) }).unwrap();
    // the list after Welcome, then the answer to B's own (older) put: both hold the newer copy
    wait(&mut b, "two lists on B", |b| b.accounts.len() >= 2);
    assert!(b.accounts.iter().all(|l| l == &[newer.clone()]), "{:?}", b.accounts);
    let now = character(BOY, 30);
    assert!(b.put_character(now.clone()));
    wait(&mut b, "B's change on the account", |b| b.accounts.back() == Some(&vec![now.clone()]));
    drop((b, srv));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A character put while offline is kept by the net thread and goes up at the next login.
#[test]
fn a_character_put_offline_goes_up_at_login() {
    let hole = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = hole.local_addr().unwrap().port();
    drop(hole); // nothing listens there yet
    let c = character(GIRL, 7);
    let mut a = Client::start(on(format!("127.0.0.1:{port}").parse().unwrap(), 63, TITLE_X)).unwrap();
    assert!(a.put_character(c.clone()), "queued while offline");
    for _ in 0..20 {
        a.frame(None);
        std::thread::sleep(Duration::from_millis(16));
    }
    let t = loop {
        match TcpTransport::bind(&format!("127.0.0.1:{port}")) {
            Ok(t) => break t,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let dir = std::env::temp_dir().join(format!("xr_net_put_offline_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let srv = Server::start(Config { dev: true, data: dir.clone(), ..Config::default() }, t);
    wait(&mut a, "the offline put on the account", |a| a.accounts.iter().any(|l| l.contains(&c)));
    drop((a, srv));
    let _ = std::fs::remove_dir_all(&dir);
}
