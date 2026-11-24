//! Steam login against the player's real Steam client (app 480): the world server runs the Steamworks game server in
//! this process (a listen server, as Spacewar does), the client side uses the logged-on Steam user.
//! Run: cargo test --release -p xr-net --test steam -- --ignored (Steam must be running and online).
use redex_proto::{Auth, Decoder, GEN_X, Msg, TITLE_X, VERSION, refuse};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Raw {
    s: TcpStream,
    d: Decoder,
}

impl Raw {
    fn hello(a: SocketAddr, auth: Auth) -> Raw {
        let mut r = Raw { s: TcpStream::connect(a).unwrap(), d: Decoder::default() };
        r.s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        r.s.write_all(&Msg::Hello { version: VERSION, auth, name: "Steamer".into(), game: GEN_X, title: TITLE_X, appearance: vec![] }.encode()).unwrap();
        r
    }
    /// Next message other than a Ping or the account's character list (sent after every Welcome), within `wait`.
    fn next(&mut self, wait: Duration) -> Option<Msg> {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            match self.d.next().unwrap() {
                Some(Msg::Ping(_) | Msg::Characters { .. }) => continue,
                Some(m) => return Some(m),
                None => {}
            }
            let mut b = [0; 512];
            match self.s.read(&mut b) {
                Ok(0) => return None,
                Ok(n) => self.d.push(&b[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => return None,
            }
        }
        None
    }
    fn code(&mut self) -> Option<u8> {
        match self.next(Duration::from_secs(15)) {
            Some(Msg::Refuse { code, .. }) => Some(code),
            Some(Msg::Welcome { .. }) => None,
            m => panic!("expected Welcome or Refuse, got {m:?}"),
        }
    }
}

#[test]
#[ignore = "needs the Steam client running and logged on"]
fn steam_tickets_log_in_once_bound_to_their_account() {
    let player = xr_net::steam::Steam::init().expect("Steam client");
    let me = player.steam_id();
    let steam = Arc::new(redex_world::steam::Steam::start(0).expect("Steam game server for app 480"));
    let ticket = || player.ticket(None, Duration::from_secs(10)).expect("a ticket");
    let tcp = |cfg: redex_world::Config| {
        let t = redex_world::transport::TcpTransport::bind("127.0.0.1:0").unwrap();
        let addr = t.local_addr().unwrap();
        (redex_world::Server::start(redex_world::Config { steam: Some(steam.clone()), ..cfg }, t), addr)
    };
    let gone = |srv: &redex_world::Server| {
        let until = Instant::now() + Duration::from_secs(10);
        while srv.players() > 0 {
            assert!(Instant::now() < until, "the session outlived its connection");
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // TCP without --dev logs in nobody: a dev id is unverified, and a Steam ticket over TCP is not bound to the server
    {
        let (_plain, addr) = tcp(Default::default());
        assert_eq!(Raw::hello(addr, Auth::DevId(1)).code(), Some(refuse::AUTH));
        let (h, real) = ticket();
        assert_eq!(Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: real }).code(), Some(refuse::AUTH));
        player.cancel(h);
    }
    // a --dev server over TCP (this PC) takes Steam logins; the login rate limit is tested on its own below
    let (srv, addr) = tcp(redex_world::Config { dev: true, max_logins_per_min: 100, ..Default::default() });
    // garbage is refused
    assert_eq!(Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: vec![7; 200] }).code(), Some(refuse::AUTH));
    // a real ticket claiming another account is refused
    let (h, other) = ticket();
    assert_eq!(Raw::hello(addr, Auth::SteamTicket { steam_id: me ^ 1, ticket: other }).code(), Some(refuse::AUTH));
    player.cancel(h);

    // a real ticket for this account logs in: the session is bound to the SteamID
    let (h1, t1) = ticket();
    let mut a = Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: t1.clone() });
    assert_eq!(a.code(), None, "the ticket was refused");
    assert_eq!(srv.players(), 1);
    // the same ticket again (a replay, here while its session is live) is refused, and the live session stays
    assert_eq!(Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: t1.clone() }).code(), Some(refuse::AUTH));
    assert_eq!(srv.players(), 1);
    // anyone can claim a public SteamID: a forged ticket claiming the live account is refused, and the live session
    // and its Steam auth session stay. Steam checks a ticket before it looks for a live session (what the server's
    // relogin path relies on): forged -> InvalidTicket, while a real one of the account -> DuplicateRequest.
    assert_eq!(Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: vec![7; 200] }).code(), Some(refuse::AUTH));
    let id = steamworks::SteamId::from_raw(me);
    assert!(matches!(steam.server().begin_authentication_session(id, &[7; 200]), Err(steamworks::AuthSessionError::InvalidTicket)));
    let (hp, probe) = ticket();
    assert!(matches!(steam.server().begin_authentication_session(id, &probe), Err(steamworks::AuthSessionError::DuplicateRequest)), "the live auth session is gone");
    player.cancel(hp);
    assert_eq!(srv.players(), 1);

    // a second login with the same SteamID (a fresh ticket) replaces the first: "logged in elsewhere"
    let (h2, t2) = ticket();
    let mut b = Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: t2 });
    assert_eq!(b.code(), None, "the relogin was refused");
    match a.next(Duration::from_secs(5)) {
        Some(Msg::Refuse { code, reason }) => assert_eq!((code, reason.as_str()), (refuse::ELSEWHERE, "logged in elsewhere")),
        m => panic!("the old session got {m:?}"),
    }
    assert_eq!(srv.players(), 1);
    player.cancel(h1);

    // the connection ends: EndAuthSession; the first ticket stays refused after its session is gone
    drop(b);
    let until = Instant::now() + Duration::from_secs(10);
    while srv.players() > 0 {
        assert!(Instant::now() < until, "the session outlived its connection");
        std::thread::sleep(Duration::from_millis(50));
    }
    player.cancel(h2);
    assert_eq!(Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: t1 }).code(), Some(refuse::AUTH));

    // a dropped connection and an immediate reconnect, as a client does: the old ticket's cancellation must not be
    // taken as the answer to the new login
    let (h5, t5) = ticket();
    let mut d = Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: t5 });
    assert_eq!(d.code(), None);
    drop(d);
    player.cancel(h5);
    let (h6, t6) = ticket();
    let mut e = Raw::hello(addr, Auth::SteamTicket { steam_id: me, ticket: t6 });
    assert_eq!(e.code(), None, "the reconnect was refused");
    drop(e);
    player.cancel(h6);
    gone(&srv);

    // at most 3 Steam logins an account a minute (a fresh server: the logins above were on another), then a retryable
    // FLOOD
    {
        let (lim, at) = tcp(redex_world::Config { dev: true, ..Default::default() });
        for k in 0..4 {
            let (h, real) = ticket();
            let code = Raw::hello(at, Auth::SteamTicket { steam_id: me, ticket: real }).code();
            player.cancel(h);
            gone(&lim);
            assert_eq!(code, if k < 3 { None } else { Some(refuse::FLOOD) }, "login {k}");
        }
        assert!(!refuse::lasting(refuse::FLOOD), "the client retries it");
    }

    // the real client: logs in with a fresh ticket per connection
    let mut c = xr_net::Client::start(xr_net::Options {
        steam: Some(player.clone()),
        ..xr_net::Options::new(addr.to_string(), Auth::SteamTicket { steam_id: me, ticket: vec![] }, TITLE_X, "Steamer")
    })
    .unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    while !c.online {
        assert!(Instant::now() < until && !c.stopped, "the client did not log in with Steam");
        c.frame(None);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(srv.players(), 1);
    // Steam's teardown of a client and a game server in one process fails (exit code 9, or a fail-fast from the exit
    // handlers): not what is tested, so the process ends here without running them
    println!("steam login test passed");
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn TerminateProcess(process: isize, code: u32) -> i32;
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
    unsafe { TerminateProcess(GetCurrentProcess(), 0) };
}
