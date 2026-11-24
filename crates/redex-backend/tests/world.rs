//! The central world (redex-world `central`) against a real backend: TLS on loopback with a self-signed certificate for
//! `localhost`, logins by world tickets the world checks with the backend. Nothing the client says about itself counts.
mod common;
use common::{Backend, SECRET, admin_url};
use redex_proto::{Auth, Decoder, GEN_X, Msg, PlayerId, TITLE_X, VERSION, refuse};
use redex_world::transport::{TcpTransport, tls_config};
use redex_world::{Config, Server, tickets::Tickets};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct World {
    _srv: Server,
    addr: SocketAddr,
    /// the world id tickets are asked for
    id: String,
    roots: Arc<rustls::RootCertStore>,
    dir: std::path::PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn world(b: &Backend) -> World {
    world_with(b, |_| {})
}

/// A world whose `Config` `tune` changes first.
fn world_with(b: &Backend, tune: impl FnOnce(&mut Config)) -> World {
    let dir = std::env::temp_dir().join(format!("redex-central-{}-{}", std::process::id(), b.url.rsplit(':').next().unwrap()));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    std::fs::write(dir.join("cert.pem"), cert.cert.pem()).unwrap();
    std::fs::write(dir.join("key.pem"), cert.key_pair.serialize_pem()).unwrap();
    let t = TcpTransport::bind("127.0.0.1:0").unwrap().with_tls(tls_config(&dir.join("cert.pem"), &dir.join("key.pem")).unwrap());
    let addr = t.local_addr().unwrap();
    let id = format!("localhost:{}", addr.port());
    let tickets = Tickets::new(&b.url, SECRET, &id);
    let mut cfg = Config { data: dir.join("data"), tickets: Some(Arc::new(tickets)), ..Config::default() };
    tune(&mut cfg);
    let srv = Server::start(cfg, t);
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    World { _srv: srv, addr, id, roots: Arc::new(roots), dir }
}

/// A raw protocol client over TLS (as xr-net's `tls:` connection).
struct Raw {
    s: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    d: Decoder,
}

impl Raw {
    fn connect(w: &World) -> Raw {
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(w.roots.clone())
            .with_no_client_auth();
        let c = rustls::ClientConnection::new(Arc::new(cfg), "localhost".try_into().unwrap()).unwrap();
        let s = TcpStream::connect(w.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        Raw { s: rustls::StreamOwned::new(c, s), d: Decoder::default() }
    }

    fn hello(w: &World, auth: Auth, name: &str) -> Raw {
        let mut r = Raw::connect(w);
        r.send(Msg::Hello { version: VERSION, auth, name: name.into(), game: GEN_X, title: TITLE_X, appearance: vec![] });
        r
    }

    fn send(&mut self, m: Msg) {
        let _ = self.s.write_all(&m.encode()).and_then(|_| self.s.flush());
    }

    /// The next message other than Ping and Characters within `wait`.
    fn next(&mut self, wait: Duration) -> Option<Msg> {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            if let Some(m) = self.d.next().unwrap() {
                if !matches!(m, Msg::Ping(_) | Msg::Characters { .. }) {
                    return Some(m);
                }
                continue;
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

    /// The next `Characters` list within `wait`, skipping everything else.
    fn list(&mut self, wait: Duration) -> Option<Vec<redex_proto::Character>> {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            if let Some(m) = self.d.next().unwrap() {
                if let Msg::Characters { list } = m {
                    return Some(list);
                }
                continue;
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
        match self.next(Duration::from_secs(5)) {
            Some(Msg::Refuse { code, .. }) => Some(code),
            _ => None,
        }
    }
}

const W: Duration = Duration::from_secs(5);

#[test]
fn the_central_world_takes_ticketed_players_only() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |_| {});
    let w = world(&b);
    let (ash, ta) = b.player("ash@example.com");
    let (misty, tm) = b.player("misty@example.com");
    let ticket = |t: &str, world: &str| b.post("/v1/world-tickets", json!({ "world": world }), Some(t)).1["ticket"].as_str().unwrap().to_string();

    // a ticket for this world logs its account in; the name in Hello is only a name
    let ka = ticket(&ta, &w.id);
    let mut a = Raw::hello(&w, Auth::WorldTicket(ka.clone()), "Ash");
    let Some(Msg::Welcome { session: sa }) = a.next(W) else { panic!("no Welcome for a valid ticket") };
    let mut m = Raw::hello(&w, Auth::WorldTicket(ticket(&tm, &w.id)), "Ash");
    let Some(Msg::Welcome { session: sm }) = m.next(W) else { panic!("no Welcome for Misty") };
    // on one map they see each other as the accounts the backend named
    a.send(Msg::MapEnter { game: GEN_X, map: 4 });
    m.send(Msg::MapEnter { game: GEN_X, map: 4 });
    let peer = |r: &mut Raw| {
        let end = Instant::now() + W;
        while Instant::now() < end {
            if let Some(Msg::Peer { session, who, .. }) = r.next(W) {
                return Some((session, who));
            }
        }
        None
    };
    // each sees the other's random public id, never the sequential account id
    let public = |t: &str| b.get("/v1/account", Some(t)).1["public_id"].as_u64().unwrap();
    let (pa, pm) = (public(&ta), public(&tm));
    assert!(pa != ash as u64 && pm != misty as u64 && pa != pm);
    assert_eq!(peer(&mut a), Some((sm, PlayerId::Account(pm))));
    assert_eq!(peer(&mut m), Some((sa, PlayerId::Account(pa))));

    // refused: no ticket, the same ticket again, a forged one, another world's, an expired one, a dev id, a Steam ticket
    assert_eq!(Raw::hello(&w, Auth::WorldTicket(String::new()), "x").code(), Some(refuse::AUTH), "no ticket");
    assert_eq!(Raw::hello(&w, Auth::WorldTicket(ka), "x").code(), Some(refuse::AUTH), "replayed");
    assert_eq!(Raw::hello(&w, Auth::WorldTicket(format!("rdx_t_{}", "B".repeat(43))), "x").code(), Some(refuse::AUTH), "forged");
    assert_eq!(Raw::hello(&w, Auth::WorldTicket("garbage".into()), "x").code(), Some(refuse::AUTH), "not a ticket");
    assert_eq!(Raw::hello(&w, Auth::WorldTicket(ticket(&ta, "elsewhere:7777")), "x").code(), Some(refuse::AUTH), "another world's");
    let late = ticket(&ta, &w.id);
    b.sql("UPDATE world_tickets SET expires_at = now() - interval '1 second' WHERE used_at IS NULL");
    assert_eq!(Raw::hello(&w, Auth::WorldTicket(late), "x").code(), Some(refuse::AUTH), "expired");
    assert_eq!(Raw::hello(&w, Auth::DevId(1), "x").code(), Some(refuse::AUTH), "dev id");
    assert_eq!(Raw::hello(&w, Auth::SteamTicket { steam_id: 76561197960287930, ticket: vec![1; 240] }, "x").code(), Some(refuse::AUTH), "Steam");
    // the two players are still on
    a.send(Msg::Ping(9));
    assert_eq!(a.next(W), Some(Msg::Pong(9)));

    // a second login of one account replaces the first ("logged in elsewhere")
    // the account's characters live in the backend: a put through the world lands there
    let look = vec![6, 1, 1, 0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0];
    let c = redex_proto::Character { id: 0xABCD, name: "Serena".into(), look, created: 1, changed: 2 };
    a.send(Msg::CharacterPut { character: c.clone() });
    let end = Instant::now() + W;
    let mut got = Vec::new();
    while got.is_empty() && Instant::now() < end {
        got = a.list(W).unwrap_or_default();
    }
    assert_eq!(got, [c.clone()]);
    let (_, v) = b.get("/v1/characters", Some(&ta));
    assert_eq!((&v["characters"][0]["id"], &v["characters"][0]["name"]), (&json!("000000000000abcd"), &json!("Serena")));

    // a second login of one account replaces the first ("logged in elsewhere") and gets the account's list after Welcome
    let mut a2 = Raw::hello(&w, Auth::WorldTicket(ticket(&ta, &w.id)), "Ash");
    assert!(matches!(a2.next(W), Some(Msg::Welcome { .. })));
    assert_eq!(a.code(), Some(refuse::ELSEWHERE));
    assert_eq!(a2.list(W), Some(vec![c]));

    // plain TCP (no TLS) gets nowhere: the server reads a TLS handshake and drops it
    let mut plain = TcpStream::connect(w.addr).unwrap();
    plain.set_read_timeout(Some(Duration::from_secs(7))).unwrap();
    plain.write_all(&Msg::Hello { version: VERSION, auth: Auth::DevId(1), name: "x".into(), game: GEN_X, title: TITLE_X, appearance: vec![] }.encode()).unwrap();
    let mut d = Decoder::default();
    let mut buf = [0u8; 256];
    let n = plain.read(&mut buf).unwrap_or(0);
    d.push(&buf[..n]);
    assert!(!matches!(d.next(), Ok(Some(Msg::Welcome { .. }))), "no login over plain TCP");
}

/// The backend down (or our secret wrong): a retryable refusal, never a login.
#[test]
fn a_world_whose_backend_does_not_answer_refuses_retryably() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |_| {});
    let (_, t) = b.player("gladion@example.com");
    let w = world(&b);
    let k = b.post("/v1/world-tickets", json!({ "world": w.id }), Some(&t)).1["ticket"].as_str().unwrap().to_string();
    // a world with the wrong secret
    let wrong = {
        let dir = w.dir.join("wrong");
        let t = TcpTransport::bind("127.0.0.1:0").unwrap().with_tls(tls_config(&w.dir.join("cert.pem"), &w.dir.join("key.pem")).unwrap());
        let addr = t.local_addr().unwrap();
        let srv = Server::start(Config { data: dir, tickets: Some(Arc::new(Tickets::new(&b.url, "not-the-secret", &w.id))), ..Config::default() }, t);
        (srv, addr)
    };
    let w2 = World { _srv: wrong.0, addr: wrong.1, id: w.id.clone(), roots: w.roots.clone(), dir: w.dir.join("wrong") };
    let code = Raw::hello(&w2, Auth::WorldTicket(k.clone()), "x").code();
    assert_eq!(code, Some(refuse::FULL));
    assert!(!refuse::lasting(refuse::FULL));
    // the ticket was not used up by the failed check: the real world takes it
    let mut ok = Raw::hello(&w, Auth::WorldTicket(k), "x");
    assert!(matches!(ok.next(W), Some(Msg::Welcome { .. })));
}

/// A ticket for `w` from session token `t`.
fn ticket_for(b: &Backend, t: &str, w: &World) -> String {
    b.post("/v1/world-tickets", json!({ "world": w.id }), Some(t)).1["ticket"].as_str().unwrap().to_string()
}

/// Logs in with `ticket`, trying again (the same ticket: a refused connection never uses it) until `Welcome` or `until`.
fn login_by(w: &World, ticket: &str, until: Instant) -> bool {
    while Instant::now() < until {
        let mut r = Raw::hello(w, Auth::WorldTicket(ticket.into()), "x");
        if matches!(r.next(Duration::from_secs(2)), Some(Msg::Welcome { .. })) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Connections that never speak, past the per-address cap (8): the one refused on the accept thread must not stop the
/// world from accepting anyone after it. Once the silent ones time out, a real player gets in.
#[test]
fn silent_connections_over_the_per_ip_cap_do_not_stop_logins() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |_| {});
    let w = world(&b);
    let (_, t) = b.player("hala@example.com");
    let silent: Vec<TcpStream> = (0..9).map(|_| TcpStream::connect(w.addr).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(500));
    let k = ticket_for(&b, &t, &w);
    assert!(login_by(&w, &k, Instant::now() + Duration::from_secs(20)), "nobody can join after 9 silent connections");
    drop(silent);
}

/// A TLS handshake fed one byte at a time is cut off by its overall deadline (5 s), not kept alive by each byte.
#[test]
fn a_slow_drip_handshake_is_cut_off_within_its_deadline() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |_| {});
    let w = world(&b);
    let mut s = TcpStream::connect(w.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    // a ClientHello record header announcing 512 bytes, then its body, one byte about every second
    let head = [0x16u8, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00, 0x01, 0xFC, 0x03, 0x03];
    let start = Instant::now();
    let mut closed = None;
    for i in 0.. {
        if start.elapsed() > Duration::from_secs(14) {
            break;
        }
        if s.write_all(&[head.get(i).copied().unwrap_or(0)]).is_err() {
            closed = Some(start.elapsed());
            break;
        }
        let mut buf = [0u8; 64];
        match s.read(&mut buf) {
            Ok(0) => {
                closed = Some(start.elapsed());
                break;
            }
            Err(e) if !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                closed = Some(start.elapsed());
                break;
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(800));
    }
    println!("slow drip closed after {closed:?}");
    assert!(closed.is_some_and(|t| t < Duration::from_secs(9)), "a slow-drip handshake was still open after {:?}", start.elapsed());
}

/// "Log out everywhere" ends the player's world session too: the world re-checks its account sessions every 30 s.
#[test]
fn logout_all_drops_the_player_from_the_world() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |_| {});
    let w = world(&b);
    let (_, t) = b.player("nanu@example.com");
    let mut p = Raw::hello(&w, Auth::WorldTicket(ticket_for(&b, &t, &w)), "Nanu");
    assert!(matches!(p.next(W), Some(Msg::Welcome { .. })));
    assert_eq!(b.post("/v1/logout-all", json!({}), Some(&t)).0, 200);
    let since = Instant::now();
    let mut got = None;
    let mut pinged = Instant::now();
    while since.elapsed() < Duration::from_secs(40) {
        // a sign of life once a second (the world drops a silent player after 5 s, and a flood at once)
        if pinged.elapsed() >= Duration::from_secs(1) {
            p.send(Msg::Ping(1));
            pinged = Instant::now();
        }
        match p.next(Duration::from_millis(500)) {
            Some(Msg::Refuse { code, .. }) => {
                got = Some((code, since.elapsed()));
                break;
            }
            Some(_) => {}
            None if p.s.sock.peer_addr().is_err() => break,
            None => {}
        }
    }
    println!("logout-all ended the world session: {got:?}");
    let (code, after) = got.expect("still in the world 40 s after logout-all");
    assert_eq!(code, refuse::AUTH);
    assert!(after <= Duration::from_secs(35), "{after:?}");
}

/// The world stays open to players through a short backend outage, but a session it cannot confirm for longer than
/// `account_unchecked_max` is ended (a banned or hijacked player cannot stay in by keeping the checks failing).
#[test]
fn a_session_the_world_cannot_confirm_for_too_long_is_ended() {
    let Some(a) = admin_url() else { return };
    let mut b = Backend::start(&a, |_| {});
    let w = world_with(&b, |c| {
        c.account_recheck = Duration::from_secs(1);
        c.account_unchecked_max = Duration::from_secs(4);
    });
    let (_, t) = b.player("hapu@example.com");
    let mut p = Raw::hello(&w, Auth::WorldTicket(ticket_for(&b, &t, &w)), "Hapu");
    assert!(matches!(p.next(W), Some(Msg::Welcome { .. })));
    // the backend the world knows goes away (a restart on another port): every check fails from now on
    b.restart();
    let since = Instant::now();
    let (mut got, mut pinged) = (None, Instant::now());
    while since.elapsed() < Duration::from_secs(15) {
        if pinged.elapsed() >= Duration::from_secs(1) {
            p.send(Msg::Ping(1));
            pinged = Instant::now();
        }
        if let Some(Msg::Refuse { code, .. }) = p.next(Duration::from_millis(500)) {
            got = Some((code, since.elapsed()));
            break;
        }
    }
    println!("an unconfirmable session ended: {got:?}");
    let (code, after) = got.expect("still in the world 15 s after its checks started failing");
    assert_eq!(code, refuse::AUTH);
    assert!(after >= Duration::from_secs(2), "ended at once ({after:?}): a short outage must not end sessions");
}
