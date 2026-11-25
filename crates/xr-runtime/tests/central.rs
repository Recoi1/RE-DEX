//! Two real game instances meet on a central world: the account backend (`redex-backend`) on a real Postgres, the world
//! (`redex-world --central`) on TCP + TLS with a self-signed certificate for `localhost`, and two headless games that
//! join `tls:localhost:PORT` with world tickets. A signs in the way the start screen does (its account `Session`, which
//! keeps the session token encrypted in A's data folder), then the game is relaunched with `--account-server` alone: no
//! password anywhere, only the kept token. B logs in with `--login` (the password in REDEX_PASSWORD, the tests' path)
//! and plays an online character, read from its account. Both start from one save state on the field (made first from
//! the 100% save), so the run takes about a minute.
//!
//! ```text
//! cargo build --release -p redex-backend -p redex-world --features redex-world/central
//! XR_DATA=<data dir with a hooked build/x-usa-v0.2.0/xgame.dll>  XR_CCI=<Pokemon X>  XR_SAVE_100PCT=<main>
//! REDEX_E2E_DATABASE_URL=<an empty Postgres database the backend may migrate>
//! cargo test --release -p xr-runtime --features mp-tests --test central -- --ignored --nocapture
//! ```
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SCRIPTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/playthrough");
const SECRET: &str = "e2e-world-secret-0123456789abcdef0123456789";
const PASSWORD: &str = "a long test password";

/// Kills the child when the test ends, however it ends.
struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn exe(name: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_BIN_EXE_redex")).with_file_name(format!("{name}.exe"));
    assert!(p.exists(), "{} is missing: cargo build --release -p redex-backend -p redex-world --features redex-world/central", p.display());
    p
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn wait_port(port: u16, child: &mut Child, what: &str) {
    let end = Instant::now() + Duration::from_secs(60);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        if let Some(s) = child.try_wait().unwrap() {
            panic!("{what} exited ({s})");
        }
        assert!(Instant::now() < end, "{what} did not listen on {port}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A data dir with the hooked DLL and a writable copy of the 100% save (as tests/exec.rs's with_100pct_save).
fn data_dir(dir: &Path, dll_from: &Path, save: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    let build = dir.join("build/x-usa-v0.2.0");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::copy(dll_from.join("build/x-usa-v0.2.0/xgame.dll"), build.join("xgame.dll")).unwrap();
    let sd = dir.join("user/savedata/0004000000055D00");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::copy(save, sd.join("main")).unwrap();
    let mut p = std::fs::metadata(sd.join("main")).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    p.set_readonly(false);
    std::fs::set_permissions(sd.join("main"), p).unwrap();
    std::fs::write(dir.join("user/savedata/0004000000055D00.format"), [0, 0, 4, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]).unwrap();
    for d in ["btvideo", "hgmail", "pss_fr", "reg", "trhouse"] {
        std::fs::create_dir_all(dir.join("user/extdata/000000000000055D").join(d)).unwrap();
    }
    std::fs::write(dir.join("user/extdata/000000000000055D.format"), [0, 0, 0, 0, 0xF, 0, 0, 0, 0x31, 3, 0, 0, 0, 0, 0, 0]).unwrap();
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("logs/latest.log")).unwrap_or_default()
}

fn tail(l: &str) -> String {
    l.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

fn http(agent: &ureq::Agent, url: &str, body: serde_json::Value, bearer: Option<&str>) -> (u16, serde_json::Value) {
    let mut q = agent.post(url);
    if let Some(t) = bearer {
        q = q.header("Authorization", &format!("Bearer {t}"));
    }
    let mut r = q.send_json(&body).unwrap();
    (r.status().as_u16(), r.body_mut().read_json().unwrap_or_default())
}

/// The code in the newest mail to `to` (the backend writes mails to `dir`).
fn mail_code(dir: &Path, to: &str) -> String {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let mut files: Vec<_> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        if let Some(m) = files.iter().rev().filter_map(|p| std::fs::read_to_string(p).ok()).find(|m| m.starts_with(&format!("To: {to}\r\n"))) {
            let i = m.rfind("rdx_m_").unwrap();
            return m[i..i + 49].to_string();
        }
        assert!(Instant::now() < end, "no mail to {to}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "two games, a backend, a world and Postgres; run with --ignored (see the file's doc)"]
fn two_games_meet_on_a_central_tls_world() {
    let (Ok(data), Ok(cci), Ok(db)) = (std::env::var("XR_DATA"), std::env::var("XR_CCI"), std::env::var("REDEX_E2E_DATABASE_URL")) else {
        println!("skipping: XR_DATA, XR_CCI or REDEX_E2E_DATABASE_URL not set");
        return;
    };
    let data = PathBuf::from(data);
    let save = std::env::var("XR_SAVE_100PCT").map(PathBuf::from).unwrap_or_else(|_| data.join("../saves/x-100pct/main"));
    let out = std::env::var("XR_MP_OUT").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("redex_central")).join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&out);
    let (a, b, mail, certs) = (out.join("a"), out.join("b"), out.join("mail"), out.join("certs"));
    data_dir(&a, &data, &save);
    data_dir(&b, &data, &save);
    std::fs::create_dir_all(&certs).unwrap();

    // a save state on the field, out of the Pokemon Center (mp-field-walk.txt's frame 1020), made offline once
    let state = out.join("field.state");
    let o = Command::new(env!("CARGO_BIN_EXE_redex"))
        .arg("--cci").arg(&cci).arg("--build").arg(a.join("build/x-usa-v0.2.0"))
        .args(["--headless", "--script", &format!("{SCRIPTS}/mp-field-walk.txt")])
        .env("XR_STATE_SAVE_AT", format!("1020:{}", state.display()))
        .output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}\n{}", tail(&log(&a)), String::from_utf8_lossy(&o.stderr));
    assert!(state.exists(), "no state written\n{}", tail(&log(&a)));
    data_dir(&a, &data, &save); // A starts over with a fresh log and save

    // the backend, migrating the given database
    let api_port = free_port();
    let api = format!("http://127.0.0.1:{api_port}");
    let mut backend = Kill(
        Command::new(exe("redex-backend"))
            .env("DATABASE_URL", &db).env("REDEX_WORLD_SECRET", SECRET).env("REDEX_MAIL_DIR", &mail)
            .env("REDEX_BIND", format!("127.0.0.1:{api_port}")).env("REDEX_LOG", "warn")
            .stdout(Stdio::null()).stderr(Stdio::inherit())
            .spawn().unwrap(),
    );
    wait_port(api_port, &mut backend.0, "redex-backend");

    // two players: registered, confirmed by the mailed code
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let run = std::process::id();
    let emails = [format!("a{run}@example.com"), format!("b{run}@example.com")];
    for e in &emails {
        let (s, v) = http(&agent, &format!("{api}/v1/register"), serde_json::json!({ "email": e, "username": e.split('@').next().unwrap(), "password": PASSWORD, "trainer_name": "E2E" }), None);
        assert_eq!(s, 202, "{v}");
        let code = mail_code(&mail, e);
        assert_eq!(http(&agent, &format!("{api}/v1/verify-email"), serde_json::json!({ "token": code }), None).0, 200);
    }

    // the central world on TLS
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let (cert_pem, key_pem) = (certs.join("cert.pem"), certs.join("key.pem"));
    std::fs::write(&cert_pem, cert.cert.pem()).unwrap();
    std::fs::write(&key_pem, cert.key_pair.serialize_pem()).unwrap();
    let world_port = free_port();
    let world_id = format!("localhost:{world_port}");
    let mut world = Kill(
        Command::new(exe("redex-world"))
            .args(["--central", "--bind", &format!("127.0.0.1:{world_port}"), "--data"]).arg(out.join("world"))
            .env("REDEX_TLS_CERT", &cert_pem).env("REDEX_TLS_KEY", &key_pem).env("REDEX_BACKEND_URL", &api)
            .env("REDEX_WORLD_SECRET", SECRET).env("REDEX_WORLD_ID", &world_id)
            .stdout(Stdio::null()).stderr(Stdio::inherit())
            .spawn().unwrap(),
    );
    wait_port(world_port, &mut world.0, "redex-world --central");

    // A signs in as the start screen does (by username), and the session is kept in A's data folder, encrypted
    use xr_frontend::start::account::{Servers, Session};
    let servers = Servers { accounts: api.clone(), world: format!("tls:{world_id}") };
    let mut s = Session::start(a.clone(), servers.clone());
    s.wait();
    assert!(s.unreachable.is_none() && s.me().is_none(), "the start screen's first look: up, signed out");
    s.sign_in(emails[0].split('@').next().unwrap().to_string(), PASSWORD.to_string());
    s.wait();
    assert!(s.can_play_online(), "signed in through the start screen's session: {:?}", s.note);
    assert!(xr_net::account::session_file(&a).is_file());
    // a relaunch of the start screen signs in with the kept token alone
    let mut again = Session::start(a.clone(), servers);
    again.wait();
    assert!(again.can_play_online(), "{:?}", again.note);
    // B's online character: on its account (put the way the start screen's Create does), its saves on this PC by its id
    let b_account = xr_net::account::Account::login(&api, &emails[1], PASSWORD, "e2e").unwrap();
    let main = std::fs::read(b.join("user/savedata/0004000000055D00/main")).unwrap();
    let look = xr_net::x::appearance(&main).unwrap().to_bytes().to_vec();
    let online = redex_proto::Character { id: 0x0e2e_0000_0000_0b0b, name: "Online B".into(), look, created: 1, changed: 1 };
    assert!(b_account.put_character(&online).unwrap().iter().any(|c| c.id == online.id));
    // an online character's saves live under the account's own root on this PC, never characters/<id>
    let b_user = xr_frontend::start::characters::online_user_dir(&b, b_account.public_id, online.id);
    std::fs::create_dir_all(b_user.parent().unwrap()).unwrap();
    std::fs::rename(b.join("user"), &b_user).unwrap();

    // both games from the state, held to real time, 40 s in all: A with the kept session only (no password in its
    // environment), B with --login and its online character
    let game = |dir: &Path, name: &str, extra: &[&str], password: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_redex"));
        c.arg("--cci").arg(&cci).arg("--build").arg(dir.join("build/x-usa-v0.2.0"))
            .args(["--headless", "--realtime", "--renderer", "vulkan", "--frames", "3420", "--script", &format!("{SCRIPTS}/mp-field-walk.txt")])
            .args(["--world", &format!("tls:{world_id}"), "--account-server", &api, "--name", name]).args(extra)
            .env("XR_STATE_LOAD", &state).env("REDEX_WORLD_CA", &cert_pem).env_remove("REDEX_PASSWORD")
            .stdout(Stdio::null()).stderr(std::fs::File::create(dir.join("stderr.txt")).unwrap());
        if password {
            c.env("REDEX_PASSWORD", PASSWORD);
        }
        c.spawn().unwrap()
    };
    let character = format!("{:016x}", online.id);
    let mut ga = Kill(game(&a, "PlayerA", &[], false));
    let mut gb = Kill(game(&b, "PlayerB", &["--login", &emails[1], "--character", &character], true));
    let (mut ca, mut cb) = (None, None);
    let end = Instant::now() + Duration::from_secs(10 * 60);
    while ca.is_none() || cb.is_none() {
        assert!(Instant::now() < end, "the games ran over 10 minutes\nA:\n{}\nB:\n{}", tail(&log(&a)), tail(&log(&b)));
        ca = ca.or(ga.0.try_wait().unwrap().map(|s| s.code()));
        cb = cb.or(gb.0.try_wait().unwrap().map(|s| s.code()));
        std::thread::sleep(Duration::from_millis(250));
    }
    let (la, lb) = (log(&a), log(&b));
    for (who, code, l) in [("A", ca, &la), ("B", cb, &lb)] {
        println!("{who}: {}", l.lines().filter(|l| l.starts_with("account:") || l.starts_with("world:") || l.starts_with("puppets: spawn")).collect::<Vec<_>>().join("\n   "));
        let err = std::fs::read_to_string(if who == "A" { a.join("stderr.txt") } else { b.join("stderr.txt") }).unwrap_or_default();
        assert_eq!(code, Some(Some(0)), "{who} exited {code:?}\n{}\n{}", tail(l), tail(&err));
        let signed = if who == "A" { "account: signed in to" } else { "account: logged in to" };
        assert!(l.contains(signed), "{who} did not sign in to the backend\n{}", tail(l));
        assert!(l.contains(&format!("world: logged in to tls:{world_id}")), "{who} did not get into the TLS world\n{}", tail(l));
        assert!(l.contains("puppets: spawn session"), "{who} never saw the other player\n{}", tail(l));
        for bad in ["panic", "guest memory fault", "WARNING: puppets"] {
            assert!(!l.contains(bad), "{who}'s log has {bad:?}\n{}", tail(l));
        }
    }
    assert!(la.contains("with the kept session"), "A did not use the kept session\n{}", tail(&la));
    assert!(lb.contains(&format!("character {character}: ")), "B did not play its online character\n{}", tail(&lb));
    assert!(!b.join(format!("characters/{character}")).exists(), "the online character's saves went to the local root");
    // nothing of the account came into B's characters.json, and A's folder holds no password
    assert!(!b.join("characters.json").exists(), "the account's characters landed on this PC's list");
    let mut dirs = vec![a.clone()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                dirs.push(e.path());
            } else if e.metadata().unwrap().len() < 64 << 20 {
                let bytes = std::fs::read(e.path()).unwrap();
                assert!(!bytes.windows(PASSWORD.len()).any(|w| w == PASSWORD.as_bytes()), "{} holds the password", e.path().display());
            }
        }
    }
    drop((world, backend));
    let _ = std::fs::remove_dir_all(&out);
}
