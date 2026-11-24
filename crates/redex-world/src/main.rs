use redex_world::transport::{SteamTransport, TcpTransport, Transport};
use redex_world::{Config, Server, steam::Steam};

fn main() {
    let mut bind = "127.0.0.1:7777".to_string(); // loopback unless --bind says otherwise
    #[cfg_attr(not(feature = "mp-tests"), allow(unused_mut))] // dev is only set by the tests' --dev
    let (mut dev, mut p2p) = (false, false);
    let mut name = "RE:DEX world".to_string();
    let (mut data, mut allow) = (None, None::<std::path::PathBuf>);
    let mut query_port: u16 = 27016; // Steam's default; open it (UDP) to be listed with a name
    #[cfg(feature = "sim")]
    let mut sim = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => bind = args.next().unwrap_or_else(|| usage()),
            // unverified dev ids: the MP test harness only (two instances on one PC)
            #[cfg(feature = "mp-tests")]
            "--dev" => dev = true,
            "--allow" => allow = Some(args.next().unwrap_or_else(|| usage()).into()),
            "--p2p" => p2p = true,
            "--name" => name = args.next().unwrap_or_else(|| usage()),
            "--query-port" => query_port = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--data" => data = Some(args.next().unwrap_or_else(|| usage())),
            // tests only (built with --features sim): MIN,MAX,LOSS = MIN..MAX ms on every send, LOSS% of positions dropped
            #[cfg(feature = "sim")]
            "--sim" => {
                let v: Vec<u64> = args.next().unwrap_or_default().split(',').filter_map(|v| v.parse().ok()).collect();
                let [min_ms, max_ms, loss_pct] = v[..] else { usage() };
                sim = Some(redex_world::transport::Sim { min_ms, max_ms, loss_pct, seed: 0x5EED });
            }
            _ => usage(),
        }
    }
    // dev ids are unverified: a dev server must not be reachable from other machines, and Steam P2P is reachable
    // from anywhere
    if dev && p2p {
        eprintln!("redex-world: --dev accepts unverified dev ids, so it cannot listen on Steam P2P (--p2p)");
        std::process::exit(2);
    }
    // moderation (M8: kick, mute, ban from inside) does not exist yet: a world anyone on Steam can reach is invite-only
    if p2p && allow.is_none() {
        eprintln!("redex-world: --p2p needs --allow FILE (the SteamID64s let in, one a line): there is no moderation yet, so a Steam world is invite-only");
        std::process::exit(2);
    }
    // a Steam ticket sent over TCP is not bound to the server it reaches, so only a --dev server takes one there
    if !dev && !p2p {
        eprintln!("redex-world: over TCP, Steam logins are taken by a --dev server only (localhost): use --p2p, or --dev");
        std::process::exit(2);
    }
    let tcp =(!p2p).then(|| {
        let t = TcpTransport::bind(&bind).unwrap_or_else(|e| {
            eprintln!("redex-world: cannot bind {bind}: {e}");
            std::process::exit(1);
        });
        let addr = t.local_addr().unwrap();
        #[cfg(feature = "sim")]
        let t = match sim {
            Some(s) => {
                eprintln!("redex-world: simulating {}-{} ms per send and {}% position loss", s.min_ms, s.max_ms, s.loss_pct);
                t.with_sim(s)
            }
            None => t,
        };
        if dev && !addr.ip().is_loopback() {
            eprintln!("redex-world: --dev accepts unverified dev ids, so it binds to localhost only (not {addr})");
            std::process::exit(2);
        }
        (t, addr)
    });
    // Steam login is how players get in; a dev server can go without it (dev ids only)
    let steam = match Steam::start_on(
        // over Steam P2P there is no game socket, but the master server lists a server only with a nonzero game port
        tcp.as_ref().map_or(27015, |t| t.1.port()),
        // a dev server is not listed, so it takes no UDP port of its own
        if dev { steamworks::QUERY_PORT_SHARED } else { query_port },
    ) {
        Ok(s) => Some(std::sync::Arc::new(s)),
        Err(e) if dev => {
            eprintln!("redex-world: {e}; Steam logins are refused, dev ids only");
            None
        }
        Err(e) => {
            eprintln!("redex-world: {e}. Players log in with Steam, so the server cannot run without it");
            std::process::exit(1);
        }
    };
    // listed in Steam's server browser (a dev server is a local test: not listed)
    if let (Some(s), false) = (&steam, dev) {
        s.advertise(&name, redex_world::Config::default().max_conns as i32);
    }
    let (t, at): (Box<dyn Transport>, String) = match (tcp, &steam) {
        (Some((t, addr)), _) => (Box::new(t), addr.to_string()),
        (None, Some(s)) => {
            let t = SteamTransport::listen(s).unwrap_or_else(|e| {
                eprintln!("redex-world: {e}");
                std::process::exit(1);
            });
            // an anonymous game server gets a new SteamID at every logon (a login token would keep one)
            (Box::new(t), format!("steam:{} (Steam relays only; this id changes when the server restarts)", s.server_id()))
        }
        (None, None) => unreachable!("without --dev a failed Steam start exits"),
    };
    eprintln!("redex-world: protocol v{} listening on {at}{}", redex_proto::VERSION, if dev { " (dev: dev ids accepted)" } else { "" });
    let server = Server::start(Config { dev, steam, data: data.map_or_else(default_data, Into::into), allow, ..Config::default() }, t);
    while server.running() {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    eprintln!("redex-world: accept thread ended; exiting");
    std::process::exit(1);
}

/// `redex-world-data/` next to the exe: where `reports.jsonl` and `bans.txt` go.
fn default_data() -> std::path::PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("redex-world-data"))).unwrap_or_else(|| "redex-world-data".into())
}

fn usage() -> ! {
    eprintln!("usage: redex-world --p2p --allow FILE [--name TEXT] [--query-port UDP] [--data DIR]");
    eprintln!("  --p2p: Steam relays, Steam logins only; --allow: the SteamID64s let in, one a line (read at each login);");
    eprintln!("  <data>/bans.txt: SteamID64s kept out (read at each login); --data: default redex-world-data next to the exe");
    #[cfg(feature = "mp-tests")]
    eprintln!("  MP-TESTS BUILD (check-release refuses it): --dev [--bind ADDR:PORT]: TCP on 127.0.0.1:7777, dev ids and Steam logins; --sim MIN,MAX,LOSS (--features sim)");
    std::process::exit(2)
}
