//! Two real game instances meet on a central world: the account backend (`redex-backend`) on a real Postgres, the world
//! (`redex-world --central`) on TCP + TLS with a self-signed certificate for `localhost`, and two headless games that log
//! in with `--account-server` and `--login` (the password in REDEX_PASSWORD) and join `tls:localhost:PORT` with world
//! tickets. Both start from one save state on the field (made first from the 100% save), so the run takes about a minute.
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

    // both games from the state, held to real time, 40 s in all
    let game = |dir: &Path, email: &str, name: &str| {
        Command::new(env!("CARGO_BIN_EXE_redex"))
            .arg("--cci").arg(&cci).arg("--build").arg(dir.join("build/x-usa-v0.2.0"))
            .args(["--headless", "--realtime", "--renderer", "vulkan", "--frames", "3420", "--script", &format!("{SCRIPTS}/mp-field-walk.txt")])
            .args(["--world", &format!("tls:{world_id}"), "--account-server", &api, "--login", email, "--name", name])
            .env("XR_STATE_LOAD", &state).env("REDEX_PASSWORD", PASSWORD).env("REDEX_WORLD_CA", &cert_pem)
            .stdout(Stdio::null()).stderr(std::fs::File::create(dir.join("stderr.txt")).unwrap())
            .spawn().unwrap()
    };
    let mut ga = Kill(game(&a, &emails[0], "PlayerA"));
    let mut gb = Kill(game(&b, &emails[1], "PlayerB"));
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
        assert!(l.contains("account: logged in to"), "{who} did not log in to the backend\n{}", tail(l));
        assert!(l.contains(&format!("world: logged in to tls:{world_id}")), "{who} did not get into the TLS world\n{}", tail(l));
        assert!(l.contains("puppets: spawn session"), "{who} never saw the other player\n{}", tail(l));
        for bad in ["panic", "guest memory fault", "WARNING: puppets"] {
            assert!(!l.contains(bad), "{who}'s log has {bad:?}\n{}", tail(l));
        }
    }
    drop((world, backend));
    let _ = std::fs::remove_dir_all(&out);
}
