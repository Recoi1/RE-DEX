use redex_proto::{Auth, Character, DAY_MS, Decoder, GEN_X, MAX_CHARACTERS, Msg, Pos, TITLE_X, VERSION};
use redex_world::transport::{Sim, TcpTransport};
use redex_world::{Config, Server};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

struct Client {
    s: TcpStream,
    d: Decoder,
}

impl Client {
    fn connect(a: SocketAddr) -> Client {
        let s = TcpStream::connect(a).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        Client { s, d: Decoder::default() }
    }
    fn send(&mut self, m: Msg) {
        let _ = self.s.write_all(&m.encode()); // the server may already have closed
    }
    /// Next message within `wait`; `None` on silence or close.
    fn recv(&mut self, wait: Duration) -> Option<Msg> {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            if let Some(m) = self.d.next().unwrap() {
                return Some(m);
            }
            let mut b = [0; 256];
            match self.s.read(&mut b) {
                Ok(0) => return None,
                Ok(n) => self.d.push(&b[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => return None,
            }
        }
        None
    }
    /// Skips heartbeats and the account's character lists (`list` reads those).
    fn next(&mut self, wait: Duration) -> Option<Msg> {
        let end = Instant::now() + wait;
        while let Some(m) = self.recv(end.saturating_duration_since(Instant::now())) {
            if !matches!(m, Msg::Ping(_) | Msg::Characters { .. }) {
                return Some(m);
            }
        }
        None
    }
    /// The next `Characters` list within `wait`, skipping everything else.
    fn list(&mut self, wait: Duration) -> Option<Vec<Character>> {
        let end = Instant::now() + wait;
        while let Some(m) = self.recv(end.saturating_duration_since(Instant::now())) {
            if let Msg::Characters { list } = m {
                return Some(list);
            }
        }
        None
    }
    /// Logs in with a dev id; the account's list that follows `Welcome`.
    fn login(&mut self, id: u128) -> Vec<Character> {
        self.hello(id, "x");
        self.list(W).expect("the account's characters after Welcome")
    }
    fn hello(&mut self, id: u128, name: &str) -> u32 {
        self.send(Msg::Hello { version: VERSION, auth: Auth::DevId(id), name: name.into(), game: GEN_X, title: TITLE_X, appearance: vec![6, 1, 0] });
        match self.next(Duration::from_secs(2)) {
            Some(Msg::Welcome { session }) => session,
            m => panic!("expected Welcome, got {m:?}"),
        }
    }
}

fn pos(map: u32, x: f32) -> Pos {
    Pos { game: GEN_X, map, channel: 0, x, y: 0.0, z: 0.0, facing: 1, mode: 0, t: 0 }
}

const W: Duration = Duration::from_secs(2);

/// A dev server (the clients here log in with dev ids).
fn start(cfg: Config) -> (Server, SocketAddr) {
    let t = TcpTransport::bind("127.0.0.1:0").unwrap();
    let a = t.local_addr().unwrap();
    (Server::start(Config { dev: true, ..cfg }, t), a)
}

#[test]
fn two_clients_end_to_end() {
    let (srv, addr) = start(Config::default());
    let (mut a, mut b, mut c) = (Client::connect(addr), Client::connect(addr), Client::connect(addr));
    let sa = a.hello(1, "Ash");
    let sb = b.hello(2, "Brock");
    c.hello(3, "Cilan");
    assert_ne!(sa, sb);
    a.send(Msg::MapEnter { game: GEN_X, map: 10 });
    assert_eq!(a.next(W), Some(Msg::Entered { map: 10, channel: 0, repeat: false }));
    b.send(Msg::MapEnter { game: GEN_X, map: 10 });
    assert_eq!(b.next(W), Some(Msg::Entered { map: 10, channel: 0, repeat: false }));
    c.send(Msg::MapEnter { game: GEN_X, map: 11 });
    assert!(matches!(c.next(W), Some(Msg::Entered { map: 11, .. })));
    // each sees the other appear
    assert!(matches!(b.next(W), Some(Msg::Peer { session, ref name, .. }) if session == sa && name == "Ash"));
    assert!(matches!(a.next(W), Some(Msg::Peer { session, ref name, .. }) if session == sb && name == "Brock"));
    a.send(Msg::Pos(pos(10, 4.5)));
    match b.next(W) {
        Some(Msg::PeerPos { session, pos }) => assert_eq!((session, pos.x, pos.channel), (sa, 4.5, 0)),
        m => panic!("{m:?}"),
    }
    assert_eq!(c.next(Duration::from_millis(300)), None, "other map gets nothing");
    a.send(Msg::Bye);
    assert_eq!(b.next(W), Some(Msg::PeerGone { session: sa }));
    drop((a, b, c));
    srv.stop();
}

/// An in-game lobby (the dev path: TCP, dev ids): the host and two players log in; the host sees both as members and
/// removes one, who is told why, is gone from the others and is turned away when it comes back; a third past the
/// lobby's size is refused as full; closing tells everyone left that the host closed it.
#[test]
fn a_lobby_kicks_keeps_out_and_closes() {
    use redex_proto::refuse;
    use redex_world::lobby::{CLOSED, Door, Host, REMOVED, dev_key};
    let dir = std::env::temp_dir().join(format!("redex-lobby-e2e-{}", std::process::id()));
    let host = Host::dev(3, std::sync::Arc::new(Door::new(dev_key(1), false)), dev_key(1), dir.clone()).unwrap();
    let addr: SocketAddr = host.address().parse().unwrap();
    let (mut h, mut a, mut b) = (Client::connect(addr), Client::connect(addr), Client::connect(addr));
    h.hello(1, "Host");
    let sa = a.hello(2, "Ash");
    let sb = b.hello(3, "Brock");
    assert_eq!(host.members().iter().map(|m| (m.0, m.1.as_str(), m.2)).collect::<Vec<_>>(), [(sa, "Ash", 0), (sb, "Brock", 0)], "the host is not its own member");
    for (c, map) in [(&mut h, 10), (&mut a, 10), (&mut b, 10)] {
        c.send(Msg::MapEnter { game: GEN_X, map });
    }
    std::thread::sleep(Duration::from_millis(300));
    while h.next(Duration::from_millis(100)).is_some() {}
    let mut full = Client::connect(addr);
    full.send(Msg::Hello { version: VERSION, auth: Auth::DevId(9), name: "late".into(), game: GEN_X, title: TITLE_X, appearance: vec![] });
    assert!(matches!(full.next(W), Some(Msg::Refuse { code: refuse::FULL, .. })), "a 4th in a lobby of 3");

    assert!(!host.kick(9999) && !host.kick(1), "no such session; the host's own is session 1 here and never kicked");
    assert!(host.kick(sb));
    let told = std::iter::from_fn(|| b.next(W)).find(|m| matches!(m, Msg::Refuse { .. }));
    assert_eq!(told, Some(Msg::Refuse { code: refuse::CLOSED, reason: REMOVED.into() }));
    assert!(std::iter::from_fn(|| h.next(W)).any(|m| m == Msg::PeerGone { session: sb }), "the host sees B go");
    assert_eq!(host.members().len(), 1);
    // back again: turned away at the door
    let mut again = Client::connect(addr);
    again.send(Msg::Hello { version: VERSION, auth: Auth::DevId(3), name: "Brock".into(), game: GEN_X, title: TITLE_X, appearance: vec![] });
    assert_eq!(again.next(W), Some(Msg::Refuse { code: refuse::CLOSED, reason: REMOVED.into() }));

    host.close();
    for c in [&mut a, &mut h] {
        let told = std::iter::from_fn(|| c.next(W)).find(|m| matches!(m, Msg::Refuse { .. }));
        assert_eq!(told, Some(Msg::Refuse { code: refuse::CLOSED, reason: CLOSED.into() }));
    }
    // the server stays up half a second after its last connection closed (its last frames go out), then stops
    let until = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
        assert!(Instant::now() < until, "the lobby never stopped listening");
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Links whose peer is named by the test, as Steam names a P2P peer by its account (`steam:<id>`): over TCP on one PC
/// every client would be the same peer, 127.0.0.1.
struct Named {
    inner: TcpTransport,
    next: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    taken: std::sync::mpsc::Sender<()>,
}

impl redex_world::transport::Transport for Named {
    fn accept(&self, wait: Duration) -> std::io::Result<Option<Box<dyn redex_world::transport::Link>>> {
        let Some(link) = self.inner.accept(wait)? else { return Ok(None) };
        let peer = self.next.lock().unwrap().pop_front().unwrap_or_else(|| "?".into());
        let _ = self.taken.send(());
        // "steam:<n>": known before Hello as dev id n (as Steam knows a P2P peer's account)
        let account = peer.strip_prefix("steam:").and_then(|n| n.parse::<u128>().ok()).map(redex_world::lobby::dev_key);
        Ok(Some(Box::new(Peered(link, peer, account))))
    }
}

struct Peered(Box<dyn redex_world::transport::Link>, String, Option<u128>);

impl redex_world::transport::Link for Peered {
    fn recv(&mut self, wait: Duration) -> std::io::Result<redex_world::transport::Recv> {
        self.0.recv(wait)
    }
    fn send(&mut self, m: &Msg) -> std::io::Result<()> {
        self.0.send(m)
    }
    fn peer(&self) -> String {
        self.1.clone()
    }
    fn set_live(&mut self) {
        self.0.set_live()
    }
    fn account(&self) -> Option<u128> {
        self.2
    }
}

/// One stranger cannot keep a lobby full: it gets two connections however many it opens, they are dropped soon if they
/// never log in, and a friends-only door turns its login away; the friends come in up to the lobby's size, and the host
/// always does, last as well.
#[test]
fn a_stranger_cannot_crowd_out_the_host_or_friends() {
    use redex_proto::refuse;
    use redex_world::lobby::{Door, Host, dev_key};
    use std::sync::{Arc, Mutex, mpsc};
    let dir = std::env::temp_dir().join(format!("redex-lobby-crowd-{}", std::process::id()));
    let t = TcpTransport::bind("127.0.0.1:0").unwrap();
    let addr = t.local_addr().unwrap();
    let (names, (tx, rx)) = (Arc::new(Mutex::new(std::collections::VecDeque::new())), mpsc::channel());
    let door = Arc::new(Door::new(dev_key(1), true));
    door.set_friends([2, 3, 4, 5].map(dev_key));
    let host = Host::on(Named { inner: t, next: names.clone(), taken: tx }, addr.to_string(), 4, door, dev_key(1), dir.clone());
    let as_peer = |peer: &str| {
        names.lock().unwrap().push_back(peer.into());
        let c = Client::connect(addr);
        rx.recv_timeout(W).expect("accepted");
        c
    };
    let refused = |c: &mut Client, wait: Duration| match c.next(wait) {
        Some(Msg::Refuse { code, .. }) => Some(code),
        _ => None,
    };
    // eight connections from one account that never say Hello: two are kept, the rest refused at once
    // a peer whose account is not known before Hello (as over TCP): the connection caps are what stop it
    let mut stranger: Vec<Client> = (0..8).map(|_| as_peer("peer:666")).collect();
    let opened = Instant::now();
    let full = stranger.iter_mut().skip(2).filter_map(|c| refused(c, Duration::from_millis(500))).filter(|&c| c == refuse::FULL).count();
    assert_eq!(full, 6, "one account holds two connections at most");
    // its login: not a friend
    stranger[0].send(Msg::Hello { version: VERSION, auth: Auth::DevId(66), name: "x".into(), game: GEN_X, title: TITLE_X, appearance: vec![] });
    assert_eq!(refused(&mut stranger[0], W), Some(refuse::AUTH));
    // friends come in while the stranger holds those two
    let _friends: Vec<Client> = [2, 3, 4]
        .map(|id| {
            let mut c = as_peer(&format!("steam:{id}"));
            c.hello(id, "friend");
            c
        })
        .into();
    // the lobby (4 with the host) is full for a fourth friend; the host's seat is kept
    let mut late = as_peer("steam:5");
    late.send(Msg::Hello { version: VERSION, auth: Auth::DevId(5), name: "late".into(), game: GEN_X, title: TITLE_X, appearance: vec![] });
    assert_eq!(refused(&mut late, W), Some(refuse::FULL), "a fifth player in a lobby of 4");
    let mut me = as_peer("steam:1");
    me.hello(1, "Host");
    assert_eq!(host.members().len(), 3);
    // its silent connection is dropped soon: by 2.5 s it was refused (the lobby's wait for Hello is 2 s, a server's 5)
    std::thread::sleep(Duration::from_millis(2500).saturating_sub(opened.elapsed()));
    assert_eq!(refused(&mut stranger[1], Duration::from_millis(200)), Some(refuse::TIMEOUT), "still waiting after 2.5 s");
    host.close();
    let _ = std::fs::remove_dir_all(dir);
}

/// Two free accounts that keep opening connections and never log in hold every handshake slot of an open lobby, but:
/// under friends only they are turned away at once (a lasting refusal, before they take a slot), so a friend still
/// comes in; and the host's own account always comes in, in an unlisted lobby too, where strangers are let try.
#[test]
fn two_strangers_cannot_hold_the_handshake_slots() {
    use redex_proto::refuse;
    use redex_world::lobby::{Door, Host, dev_key};
    use std::sync::{Arc, Mutex, mpsc};
    for friends_only in [true, false] {
        let dir = std::env::temp_dir().join(format!("redex-lobby-slots-{friends_only}-{}", std::process::id()));
        let t = TcpTransport::bind("127.0.0.1:0").unwrap();
        let addr = t.local_addr().unwrap();
        let (names, (tx, rx)) = (Arc::new(Mutex::new(std::collections::VecDeque::new())), mpsc::channel());
        let door = Arc::new(Door::new(dev_key(1), friends_only));
        door.set_friends([2].map(dev_key));
        let host = Host::on(Named { inner: t, next: names.clone(), taken: tx }, addr.to_string(), 4, door, dev_key(1), dir.clone());
        let as_peer = |peer: &str| {
            names.lock().unwrap().push_back(peer.into());
            let c = Client::connect(addr);
            rx.recv_timeout(W).expect("accepted");
            c
        };
        let refused = |c: &mut Client, wait: Duration| match c.next(wait) {
            Some(Msg::Refuse { code, .. }) => Some(code),
            _ => None,
        };
        // two accounts, two silent connections each: all four handshake slots
        let mut held: Vec<Client> = [66, 66, 67, 67].map(|n| as_peer(&format!("steam:{n}"))).into();
        let codes: Vec<Option<u8>> = held.iter_mut().map(|c| refused(c, Duration::from_millis(300))).collect();
        if friends_only {
            assert_eq!(codes, [Some(refuse::AUTH); 4], "strangers turned away before they take a slot");
            let mut friend = as_peer("steam:2");
            friend.hello(2, "Friend");
        } else {
            assert_eq!(codes, [None; 4], "an unlisted lobby lets them try");
            let mut late = as_peer("steam:68");
            assert_eq!(refused(&mut late, Duration::from_millis(500)), Some(refuse::FULL), "no slot left for a third stranger");
        }
        // the host's own account, past every slot
        let mut me = as_peer("steam:1");
        me.hello(1, "Host");
        // but still two connections at most (a reconnect loop in the host's own game is a bug, not a reason for more):
        // its game plus one silent connection, then refused
        let _second = as_peer("steam:1");
        let mut third = as_peer("steam:1");
        assert_eq!(refused(&mut third, Duration::from_millis(500)), Some(refuse::FULL), "a third connection of the host's account");
        host.close();
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn version_mismatch_is_refused_clearly() {
    let (srv, addr) = start(Config::default());
    let mut a = Client::connect(addr);
    a.send(Msg::Hello { version: VERSION + 1, auth: Auth::DevId(1), name: "x".into(), game: GEN_X, title: TITLE_X, appearance: vec![] });
    match a.next(W) {
        Some(Msg::Refuse { code, reason }) => assert!(code == redex_proto::refuse::VERSION && reason.contains("update RE:DEX"), "{reason}"),
        m => panic!("{m:?}"),
    }
    assert_eq!(a.next(W), None, "closed after refusal");
    assert_eq!(srv.players(), 0);
    srv.stop();
}

#[test]
fn silent_client_times_out_and_peers_see_it_go() {
    let cfg = Config { timeout: Duration::from_millis(600), heartbeat: Duration::from_millis(200), ..Config::default() };
    let (srv, addr) = start(cfg);
    let mut a = Client::connect(addr);
    let mut b = Client::connect(addr);
    let sa = a.hello(1, "Ash");
    b.hello(2, "Brock");
    a.send(Msg::MapEnter { game: GEN_X, map: 1 });
    b.send(Msg::MapEnter { game: GEN_X, map: 1 });
    // b keeps answering pings; a goes silent
    let end = Instant::now() + Duration::from_secs(3);
    let mut gone = false;
    while Instant::now() < end && !gone {
        match b.recv(Duration::from_millis(100)) {
            Some(Msg::Ping(n)) => b.send(Msg::Pong(n)),
            Some(Msg::PeerGone { session }) if session == sa => gone = true,
            _ => {}
        }
    }
    assert!(gone, "silent client must be dropped after the timeout");
    drop((a, b));
    srv.stop();
}

#[test]
fn idle_connection_before_hello_times_out() {
    let cfg = Config { timeout: Duration::from_millis(400), ..Config::default() };
    let (srv, addr) = start(cfg);
    let mut a = Client::connect(addr);
    assert!(matches!(a.recv(Duration::from_secs(2)), Some(Msg::Refuse { .. })));
    assert_eq!(a.recv(Duration::from_secs(2)), None);
    srv.stop();
}

#[test]
fn oversize_frame_drops_connection() {
    let (srv, addr) = start(Config::default());
    let mut a = Client::connect(addr);
    a.s.write_all(&(1u32 << 30).to_le_bytes()).unwrap();
    assert!(matches!(a.recv(W), Some(Msg::Refuse { .. })), "a reason, not a silent drop");
    assert_eq!(a.recv(W), None);
    srv.stop();
}

#[test]
fn per_peer_limit() {
    let cfg = Config { max_per_peer: 2, ..Config::default() };
    let (srv, addr) = start(cfg);
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    a.hello(1, "a");
    b.hello(2, "b");
    let mut c = Client::connect(addr);
    assert!(matches!(c.next(W), Some(Msg::Refuse { .. })));
    srv.stop();
}

#[test]
fn survives_jitter_and_loss() {
    let t = TcpTransport::bind("127.0.0.1:0")
        .unwrap()
        .with_sim(Sim { min_ms: 50, max_ms: 150, loss_pct: 30, seed: 7 });
    let addr = t.local_addr().unwrap();
    let srv = Server::start(Config { dev: true, ..Config::default() }, t);
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    a.hello(1, "a");
    b.hello(2, "b");
    a.send(Msg::MapEnter { game: GEN_X, map: 1 });
    b.send(Msg::MapEnter { game: GEN_X, map: 1 });
    let mut xs = vec![];
    for i in 0..10 {
        a.send(Msg::Pos(pos(1, i as f32)));
        std::thread::sleep(Duration::from_millis(110));
        while let Some(m) = b.recv(Duration::from_millis(1)) {
            if let Msg::PeerPos { pos, .. } = m {
                xs.push(pos.x);
            }
        }
    }
    while let Some(m) = b.recv(Duration::from_millis(400)) {
        if let Msg::PeerPos { pos, .. } = m {
            xs.push(pos.x);
        }
    }
    assert!(!xs.is_empty() && xs.len() < 10, "some lost, some delivered: {xs:?}");
    assert!(xs.windows(2).all(|w| w[0] < w[1]), "in order: {xs:?}");
    srv.stop();
}

#[test]
fn garbage_before_hello_gets_a_reason() {
    let (srv, addr) = start(Config::default());
    let mut a = Client::connect(addr);
    a.s.write_all(&[2, 0, 0, 0, 99, 99]).unwrap(); // unknown tag
    assert!(matches!(a.recv(W), Some(Msg::Refuse { code: redex_proto::refuse::BAD_FRAME, reason }) if reason.contains("bad frame")));
    let mut b = Client::connect(addr);
    b.send(Msg::Bye); // valid, but not Hello
    assert!(matches!(b.recv(W), Some(Msg::Refuse { .. })));
    srv.stop();
}

#[test]
fn same_id_login_replaces_and_tells_the_old_one() {
    let (srv, addr) = start(Config::default());
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    a.hello(5, "a");
    b.hello(5, "a");
    assert!(matches!(a.next(W), Some(Msg::Refuse { code: redex_proto::refuse::ELSEWHERE, reason }) if reason == "logged in elsewhere"));
    assert_eq!(a.next(W), None);
    assert_eq!(srv.players(), 1);
    srv.stop();
}

#[test]
fn map_enter_flood_closes_the_connection() {
    let (srv, addr) = start(Config::default());
    let mut a = Client::connect(addr);
    a.hello(1, "a");
    for _ in 0..8 {
        a.send(Msg::MapEnter { game: GEN_X, map: 1 });
    }
    let mut refused = false;
    while let Some(m) = a.recv(W) {
        refused |= matches!(m, Msg::Refuse { .. });
    }
    assert!(refused);
    srv.stop();
}

#[test]
fn game_mismatch_on_enter_is_refused() {
    let (srv, addr) = start(Config::default());
    let mut a = Client::connect(addr);
    a.hello(1, "a");
    a.send(Msg::MapEnter { game: 7, map: 1 });
    assert!(matches!(a.next(W), Some(Msg::Refuse { .. })));
    srv.stop();
}

#[test]
fn stopping_the_server_says_bye() {
    let (srv, addr) = start(Config::default());
    let mut a = Client::connect(addr);
    a.hello(1, "a");
    srv.stop();
    assert_eq!(a.next(W), Some(Msg::Bye));
}

#[test]
fn final_position_after_a_burst_arrives() {
    let (srv, addr) = start(Config::default());
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    a.hello(1, "a");
    b.hello(2, "b");
    a.send(Msg::MapEnter { game: GEN_X, map: 1 });
    b.send(Msg::MapEnter { game: GEN_X, map: 1 });
    for x in 0..5 {
        a.send(Msg::Pos(pos(1, x as f32)));
    }
    let mut last = None;
    while let Some(m) = b.recv(Duration::from_millis(500)) {
        if let Msg::PeerPos { pos, .. } = m {
            last = Some(pos.x);
        }
    }
    assert_eq!(last, Some(4.0));
    srv.stop();
}

/// Without --dev a dev id is refused (with the AUTH code), and a server without Steam refuses every ticket.
#[test]
fn dev_ids_only_on_a_dev_server() {
    let t = TcpTransport::bind("127.0.0.1:0").unwrap();
    let addr = t.local_addr().unwrap();
    let _srv = Server::start(Config::default(), t);
    for auth in [Auth::DevId(1), Auth::SteamTicket { steam_id: 76561197960287930, ticket: vec![1; 240] }] {
        let mut a = Client::connect(addr);
        a.send(Msg::Hello { version: VERSION, auth, name: "x".into(), game: GEN_X, title: TITLE_X, appearance: vec![] });
        assert!(matches!(a.next(W), Some(Msg::Refuse { code: redex_proto::refuse::AUTH, .. })));
    }
}

#[test]
fn a_v4_client_is_refused_with_update_redex() {
    let (srv, addr) = start(Config::default());
    // a real v4 Hello (its layout is v5's), and a real v3 one: tag 1, version 3 (u16), dev-id auth, then v3's fields
    let v4 = Msg::Hello { version: 4, auth: Auth::DevId(7), name: "A".into(), game: GEN_X, title: TITLE_X, appearance: vec![] }.encode();
    let mut f = vec![1, 3, 0, 1];
    f.extend([7u8; 16]);
    f.extend([1, b'A', 6, 0, 0x5D, 5, 0, 0, 4, 0, 0, 0]);
    let mut v3 = (f.len() as u32).to_le_bytes().to_vec();
    v3.extend(f);
    for frame in [v4, v3] {
        let mut a = Client::connect(addr);
        a.s.write_all(&frame).unwrap();
        match a.next(W) {
            Some(Msg::Refuse { code, reason }) => assert!(code == redex_proto::refuse::VERSION && reason.contains("update RE:DEX"), "{reason}"),
            m => panic!("{m:?}"),
        }
    }
    srv.stop();
}

#[test]
fn name_and_appearance_caps() {
    let (srv, addr) = start(Config::default());
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    // 30 characters with a bidi override: cut to 16 after stripping; a wrong-generation appearance is dropped
    a.send(Msg::Hello { version: VERSION, auth: Auth::DevId(1), name: "\u{202E}0123456789abcdefghijklmnopqrs".into(), game: GEN_X, title: TITLE_X, appearance: vec![5, 1, 0] });
    assert!(matches!(a.next(W), Some(Msg::Welcome { .. })));
    let girl = ch(1, "", 0).look;
    b.send(Msg::Hello { version: VERSION, auth: Auth::DevId(2), name: "B".into(), game: GEN_X, title: TITLE_X, appearance: girl.clone() });
    assert!(matches!(b.next(W), Some(Msg::Welcome { .. })));
    a.send(Msg::MapEnter { game: GEN_X, map: 3 });
    a.next(W);
    b.send(Msg::MapEnter { game: GEN_X, map: 3 });
    assert!(matches!(b.next(W), Some(Msg::Entered { .. })));
    match b.next(W) {
        Some(Msg::Peer { name, appearance, .. }) => assert!(name == "0123456789abcdef" && appearance.is_empty(), "{name:?} {appearance:?}"),
        m => panic!("{m:?}"),
    }
    match a.next(W) {
        Some(Msg::Peer { appearance, .. }) => assert_eq!(appearance, girl),
        m => panic!("{m:?}"),
    }
    // a mid-map change is checked the same way (a part of a look is dropped) and reaches the room
    b.send(Msg::AppearanceUpdate { appearance: vec![6, 1, 0, 4] });
    assert_eq!(a.next(Duration::from_millis(400)), None);
    let mut other = girl.clone();
    other[4] = 1; // another hair colour
    b.send(Msg::AppearanceUpdate { appearance: other.clone() });
    assert!(matches!(a.next(W), Some(Msg::PeerAppearance { appearance, .. }) if appearance == other));
    srv.stop();
}

#[test]
fn whispers_reach_the_room_or_a_reply_and_are_capped() {
    let (srv, addr) = start(Config::default());
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    let sa = a.hello(1, "Ash");
    let sb = b.hello(2, "Brock");
    a.send(Msg::MapEnter { game: GEN_X, map: 1 });
    b.send(Msg::MapEnter { game: GEN_X, map: 2 });
    a.next(W);
    b.next(W);
    // another map, and b never whispered a: dropped
    a.send(Msg::Whisper { to_session: sb, text: "over there?".into() });
    assert_eq!(b.next(Duration::from_millis(500)), None);
    b.send(Msg::MapEnter { game: GEN_X, map: 1 });
    assert!(matches!(b.next(W), Some(Msg::Entered { .. })));
    assert!(matches!(b.next(W), Some(Msg::Peer { .. })));
    assert!(matches!(a.next(W), Some(Msg::Peer { .. })));
    // the same map: delivered, text cleaned and cut to 120 characters
    a.send(Msg::Whisper { to_session: sb, text: format!("hi\u{202E}{}", "x".repeat(200)) });
    match b.next(W) {
        Some(Msg::PeerWhisper { from_session, name, text }) => {
            assert!(from_session == sa && name == "Ash" && text.chars().count() == 120 && text.starts_with("hix"), "{text:?}")
        }
        m => panic!("{m:?}"),
    }
    // an unknown session: dropped, the sender hears nothing
    a.send(Msg::Whisper { to_session: 999, text: "anyone".into() });
    assert_eq!(a.next(Duration::from_millis(400)), None);
    // 5 per 10 s: 1 (to another map, counted) + 1 + 1 (unknown, counted) + 2 more get through, the 6th is dropped
    // without a word
    for i in 0..3 {
        a.send(Msg::Whisper { to_session: sb, text: format!("m{i}") });
    }
    let got: Vec<_> = std::iter::from_fn(|| b.next(Duration::from_millis(400))).collect();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(a.next(Duration::from_millis(200)), None);
    srv.stop();
}

#[test]
fn a_report_is_written_with_its_context() {
    let dir = std::env::temp_dir().join(format!("redex-world-report-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (srv, addr) = start(Config { data: dir.clone(), ..Config::default() });
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    let sa = a.hello(1, "Ash");
    let sb = b.hello(2, "Brock \"B\"");
    a.send(Msg::MapEnter { game: GEN_X, map: 4 });
    b.send(Msg::MapEnter { game: GEN_X, map: 4 });
    a.next(W);
    b.next(W);
    b.send(Msg::Pos(pos(4, 12.5)));
    b.send(Msg::Whisper { to_session: sa, text: "you suck".into() });
    a.send(Msg::Pos(pos(4, 1.0)));
    std::thread::sleep(Duration::from_millis(300));
    // naming another account than the session's: dropped
    a.send(Msg::Report { session: sb, who: redex_proto::PlayerId::Steam(76561197960287930), reason: redex_proto::REPORT_CHAT, note: "wrong".into() });
    a.send(Msg::Report { session: sb, who: redex_proto::PlayerId::Dev, reason: redex_proto::REPORT_CHAT, note: "rude".into() });
    let file = dir.join("reports.jsonl");
    let until = Instant::now() + Duration::from_secs(3);
    while !file.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(200));
    let text = std::fs::read_to_string(&file).expect("reports.jsonl");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text}");
    let l = lines[0];
    for want in [
        format!("\"reporter\":\"dev:{:032x}\"", 1),
        format!("\"target\":\"dev:{:032x}\"", 2),
        format!("\"reporter_session\":{sa}"),
        format!("\"target_session\":{sb}"),
        "\"target_name\":\"Brock \\\"B\\\"\"".to_string(),
        "\"reason\":2".to_string(),
        "\"note\":\"rude\"".to_string(),
        "\"target_steam_id\":0".to_string(),
        "\"x\":12.5".to_string(),
        "\"x\":1".to_string(),
        "\"text\":\"you suck\"".to_string(),
        "\"time\":".to_string(),
        "\"trails\":\"client-claimed\"".to_string(),
    ] {
        assert!(l.contains(&want), "missing {want} in {l}");
    }
    // the cap is 3 a minute per reporter: one was written, 3 more are sent, only 2 of them are
    for _ in 0..3 {
        a.send(Msg::Report { session: sb, who: redex_proto::PlayerId::Dev, reason: 0, note: "again".into() });
    }
    std::thread::sleep(Duration::from_millis(500));
    let all = std::fs::read_to_string(&file).unwrap();
    assert_eq!(all.lines().count(), 3, "{all}");
    assert!(all.lines().nth(1).unwrap().contains("\"note\":\"again\""));
    drop((a, b));
    srv.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- protocol v5: characters on the account ------------------------------------------------------------------------

fn temp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("redex-world-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

/// A character with the new-game girl's look (19 bytes that pass `Appearance::from_bytes`).
fn ch(id: u64, name: &str, changed: u64) -> Character {
    Character { id, name: name.into(), look: vec![6, 1, 1, 0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0], created: 1, changed }
}

fn put(c: &Character) -> Msg {
    Msg::CharacterPut { character: c.clone() }
}

#[test]
fn characters_are_kept_per_account() {
    let dir = temp("per-account");
    let (srv, addr) = start(Config { data: dir.clone(), ..Config::default() });
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    assert_eq!(a.login(1), [], "a new account has no characters");
    assert_eq!(b.login(2), []);
    let (serena, calem) = (ch(10, "Serena", now_ms()), ch(20, "Calem", now_ms()));
    a.send(put(&serena));
    assert_eq!(a.list(W), Some(vec![serena.clone()]), "a put is answered with the account's list");
    b.send(put(&calem));
    assert_eq!(b.list(W), Some(vec![calem.clone()]), "only B's own");
    // the same character id from another account is that account's own copy: A's is untouched
    b.send(put(&ch(10, "Mine", now_ms() + 1000)));
    assert_eq!(b.list(W).map(|l| l.len()), Some(2));
    drop((a, b));
    let (mut a, mut b) = (Client::connect(addr), Client::connect(addr));
    assert_eq!(a.login(1), [serena]);
    assert_eq!(b.login(2).iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Calem", "Mine"]);
    srv.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// a PC clock far ahead cannot pin a character forever (a `changed` more than a day ahead of the
/// server is dropped), and only looks that pass `Appearance::from_bytes` are kept. Dropped puts get no answer.
#[test]
fn a_put_from_the_future_or_with_a_bad_look_is_dropped() {
    let dir = temp("dropped");
    let (srv, addr) = start(Config { data: dir.clone(), ..Config::default() });
    let mut a = Client::connect(addr);
    a.login(1);
    let now = now_ms();
    let mut hacked = ch(11, "Hacked", now);
    hacked.look[3..].fill(0xFF); // the right tag and version around an outfit out of the game's ranges
    let mut short = ch(12, "Short", now);
    short.look.pop();
    let mut long = ch(13, "Long", now);
    long.look.push(0);
    let mut other_gen = ch(14, "Gen5", now);
    other_gen.look[0] = 5;
    for c in [ch(15, "Future", now + DAY_MS + 60_000), hacked, short, long, other_gen, ch(0, "Zero", now)] {
        a.send(put(&c));
    }
    assert_eq!(a.list(Duration::from_millis(500)), None, "dropped without an answer");
    // a clock a little ahead (under a day) is kept; the name is cleaned like a Hello's
    a.send(put(&ch(16, "\u{202E}Serena\u{200B}", now + DAY_MS - 60_000)));
    let kept = ch(16, "Serena", now + DAY_MS - 60_000);
    assert_eq!(a.list(W), Some(vec![kept.clone()]));
    drop(a);
    let mut a = Client::connect(addr);
    assert_eq!(a.login(1), [kept], "nothing else was stored");
    srv.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// An account holds `MAX_CHARACTERS`; a session puts at most 16 in 10 s (a login's 8 and then some).
#[test]
fn the_ninth_character_is_dropped() {
    let dir = temp("ninth");
    let (srv, addr) = start(Config { data: dir.clone(), ..Config::default() });
    let mut a = Client::connect(addr);
    a.login(1);
    let now = now_ms();
    let eight: Vec<Character> = (1..=MAX_CHARACTERS as u64).map(|id| ch(id, &format!("C{id}"), now)).collect();
    for c in &eight {
        a.send(put(c));
        assert!(a.list(W).unwrap().contains(c));
    }
    // a ninth character is answered with the eight, without it; a newer copy of one still goes in
    a.send(put(&ch(9, "Ninth", now)));
    assert_eq!(a.list(W), Some(eight.clone()));
    for k in 1..=7 {
        a.send(put(&ch(1, &format!("Newer{k}"), now + k)));
        let l = a.list(W).unwrap();
        assert_eq!((l.len(), l[0].name.clone()), (MAX_CHARACTERS, format!("Newer{k}")));
    }
    // the 17th put in 10 s is over the session's rate: no answer
    a.send(put(&ch(1, "Again", now + 100)));
    assert_eq!(a.list(Duration::from_millis(500)), None);
    drop(a);
    let mut a = Client::connect(addr);
    assert_eq!(a.login(1)[0].name, "Newer7");
    srv.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// An account file another program holds (unreadable, and it cannot be moved aside) is never saved over: the login
/// gets an empty list, a put changes nothing, and once it is free the account is all there.
#[cfg(windows)]
#[test]
fn a_locked_account_file_is_never_saved_over() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = temp("locked");
    let (srv, addr) = start(Config { data: dir.clone(), ..Config::default() });
    let mut a = Client::connect(addr);
    a.login(1);
    let c = ch(10, "Serena", now_ms());
    a.send(put(&c));
    assert_eq!(a.list(W), Some(vec![c.clone()]));
    drop(a);
    let file = dir.join("accounts").join(format!("{:032x}.bin", 1u128));
    let before = std::fs::read(&file).unwrap();
    let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&file).unwrap();
    let mut a = Client::connect(addr);
    assert_eq!(a.login(1), []);
    a.send(put(&ch(11, "Calem", now_ms())));
    assert_eq!(a.list(W), Some(vec![]));
    drop((a, lock));
    assert_eq!(std::fs::read(&file).unwrap(), before);
    let mut a = Client::connect(addr);
    assert_eq!(a.login(1), [c]);
    srv.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn characters_survive_a_server_restart() {
    let dir = temp("restart");
    let (srv, addr) = start(Config { data: dir.clone(), ..Config::default() });
    let mut a = Client::connect(addr);
    a.login(1);
    let c = ch(10, "Serena", now_ms());
    a.send(put(&c));
    assert_eq!(a.list(W), Some(vec![c.clone()]));
    srv.stop();
    drop(a);
    let (srv, addr) = start(Config { data: dir.clone(), ..Config::default() });
    let mut a = Client::connect(addr);
    assert_eq!(a.login(1), [c]);
    srv.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
