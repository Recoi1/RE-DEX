//! The Steam transport (Task 3b): a world server listening on Steam Networking Sockets P2P only (`SteamTransport`, the
//! game server's SteamID, Steam's relays, ICE off) and the client connecting with `steam:<steamid64>`, logging in
//! with its Steam ticket and entering a map. The server is a second process (this test binary with
//! STEAM_P2P_ROLE=server): in one process Steam short-cuts the connection instead of using the relays.
//! Needs Steam running and online. Run: cargo test --release -p xr-net --test steam_p2p -- --ignored --nocapture
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const NAME: &str = "a_client_reaches_the_world_by_steamid_over_relays";

fn server() -> ! {
    let steam = Arc::new(redex_world::steam::Steam::start(0).expect("Steam game server"));
    let t = redex_world::transport::SteamTransport::listen(&steam).expect("P2P listen socket");
    let srv = redex_world::Server::start(redex_world::Config { steam: Some(steam.clone()), ..Default::default() }, t);
    println!("SERVER {}", steam.server_id());
    let until = Instant::now() + Duration::from_secs(120);
    while Instant::now() < until && srv.running() {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::process::exit(3)
}

#[test]
#[ignore = "needs the Steam client running and logged on"]
fn a_client_reaches_the_world_by_steamid_over_relays() {
    if std::env::var_os("STEAM_P2P_ROLE").is_some() {
        server();
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", NAME, "--nocapture"])
        .env("STEAM_P2P_ROLE", "server")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let id: u64 = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
        .find_map(|l| l.strip_prefix("SERVER ").map(|id| id.trim().parse().unwrap()))
        .expect("the server process printed its SteamID");

    let player = xr_net::steam::Steam::init().expect("Steam client");
    let auth = redex_proto::Auth::SteamTicket { steam_id: player.steam_id(), ticket: vec![] };
    let mut c = xr_net::Client::start(xr_net::Options { steam: Some(player), ..xr_net::Options::new(format!("steam:{id}"), auth, redex_proto::TITLE_X, "Relayed") }).unwrap();
    let here = xr_net::Local { map: 7, pos: [1.0, 0.0, 2.0], facing: 1, mode: 0, appearance: None };
    let (started, until) = (Instant::now(), Instant::now() + Duration::from_secs(60));
    while c.room.is_none() {
        assert!(Instant::now() < until && !c.stopped, "no login and map over the relays (online {})", c.online);
        c.frame(Some(here));
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("logged in over Steam relays and entered {:?} after {:?}", c.room, started.elapsed());
    assert_eq!(c.room, Some((7, 0)));
    let _ = child.kill();
    let _ = child.wait();
    drop(c);
    std::process::exit(0) // Steam's teardown in this process is not what is tested
}
