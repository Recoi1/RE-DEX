//! The world server shows up in Steam's server list (app 480) under its game tags. Needs the Steam client running
//! and logged on, and Steam's master server answering; a listing takes a while to appear, so it polls for 2 minutes.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
#[ignore = "needs the Steam client running and logged on"]
fn the_server_is_listed_under_its_tags() {
    let player = xr_net::steam::Steam::init().expect("Steam client");
    let name = format!("RE:DEX test {}", std::process::id());
    let server = redex_world::steam::Steam::start_on(27500, 27501).expect("Steam game server for app 480");
    server.advertise(&name, 256);
    let until = Instant::now() + Duration::from_secs(120);
    while Instant::now() < until {
        let found: Arc<Mutex<Vec<(u64, String, String)>>> = Arc::default();
        let f = found.clone();
        let filters = HashMap::from([("gametagsand", "redex")]);
        let cbs = steamworks::ServerListCallbacks::new(
            Box::new(|_, _| {}),
            Box::new(|_, _| {}),
            Box::new(move |req, _| {
                let mut r = req.lock().unwrap();
                for i in 0..r.get_server_count().unwrap_or(0) {
                    if let Ok(s) = r.get_server_details(i) {
                        f.lock().unwrap().push((s.steamid, s.server_name, s.tags));
                    }
                }
                let _ = r.release();
            }),
        );
        let _req = player.client().matchmaking_servers().internet_server_list(480u32, &filters, cbs).expect("query");
        let round = Instant::now() + Duration::from_secs(12);
        while Instant::now() < round {
            player.pump();
            std::thread::sleep(Duration::from_millis(100));
        }
        let list = found.lock().unwrap().clone();

        // the master server's entry carries the tags and the SteamID; the name comes from the server's own answer to
        // Steam's query (a UDP query port that must be reachable), so it may be empty on a machine behind a firewall
        if let Some((_, n, tags)) = list.iter().find(|(id, ..)| *id == server.server_id()) {
            eprintln!("listed: name {n:?} (asked for {name:?}), tags {tags:?}");
            assert_eq!(tags, "redex,v4,kalos");
            return;
        }
    }
    panic!("the server was not listed within 120 s");
}
