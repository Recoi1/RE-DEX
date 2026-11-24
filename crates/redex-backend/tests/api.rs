//! The account API against a real Postgres (see common/mod.rs).
mod common;
use common::{Backend, PASSWORD, SECRET, admin_url};
use serde_json::json;

fn backend() -> Option<Backend> {
    admin_url().map(|a| Backend::start(&a, |_| {}))
}

#[test]
fn health_check_answers() {
    let Some(b) = backend() else { return };
    assert_eq!(b.get("/healthz", None), (200, json!({ "status": "ok" })));
}

#[test]
fn register_verify_login_logout() {
    let Some(b) = backend() else { return };
    let (s, v) = b.post("/v1/register", json!({ "email": " Ash@Example.com ", "username": "Ash", "password": PASSWORD, "trainer_name": "Ash\u{202E}" }), None);
    assert_eq!(s, 202, "{v}");
    let new_account = v.clone();
    // checks at the door: a weak password, a non-address, an empty name
    assert_eq!(b.post("/v1/register", json!({ "email": "b@example.com", "username": "bxyz", "password": "short", "trainer_name": "B" }), None).0, 400);
    assert_eq!(b.post("/v1/register", json!({ "email": "nope", "username": "nope", "password": PASSWORD, "trainer_name": "B" }), None).0, 400);
    assert_eq!(b.post("/v1/register", json!({ "email": "c@example.com", "username": "cxyz", "password": PASSWORD, "trainer_name": "\u{200B}" }), None).0, 400);
    // the same address in another case is the same account; the answer does not tell (the same as for a new account),
    // and the owner is told by mail instead
    let (s, v) = b.post("/v1/register", json!({ "email": "ASH@example.COM", "username": "ash2", "password": PASSWORD, "trainer_name": "X" }), None);
    assert_eq!((s, &v), (202, &new_account));
    let notice = (0..50).find_map(|_| b.mails("Ash@Example.com").get(1).cloned().or_else(|| { std::thread::sleep(std::time::Duration::from_millis(100)); None })).expect("a notice to the owner");
    assert!(notice.contains("already has an account") && !notice.contains("rdx_"), "{notice}");
    assert_eq!(b.count("SELECT count(*) FROM accounts"), 1);
    // and a username in another case is the same username
    let (s, v) = b.post("/v1/register", json!({ "email": "other@example.com", "username": "ASH", "password": PASSWORD, "trainer_name": "X" }), None);
    assert_eq!((s, &v["message"]), (409, &json!("this username is taken")));
    // usernames: the world's reserved names, characters outside the set, too short
    for bad in ["Admin", "M0derat0r", "re dex", "\u{e9}clair", "ab"] {
        assert_eq!(b.post("/v1/register", json!({ "email": "u@example.com", "username": bad, "password": PASSWORD, "trainer_name": "X" }), None).0, 400, "{bad}");
    }

    // logging in works before the address is confirmed, playing does not
    let (s, v) = b.post("/v1/login", json!({ "email": "ash@example.com", "password": PASSWORD, "device": "PC" }), None);
    assert_eq!((s, &v["email_verified"]), (200, &json!(false)), "{v}");
    let t = v["token"].as_str().unwrap().to_string();
    let id = v["account_id"].as_i64().unwrap();
    assert!(t.starts_with("rdx_s_"));
    assert_eq!(b.post("/v1/world-tickets", json!({ "world": "w" }), Some(&t)).0, 403);

    // the mail's code confirms the address, once
    let code = b.mail_code("Ash@Example.com", 1);
    assert_eq!(b.post("/v1/verify-email", json!({ "token": "rdx_m_wrong" }), None).0, 400);
    assert_eq!(b.post("/v1/verify-email", json!({ "token": code }), None).0, 200);
    assert_eq!(b.post("/v1/verify-email", json!({ "token": code }), None).0, 400, "a code works once");
    assert_eq!(b.post("/v1/verify-email/resend", json!({}), Some(&t)).0, 409, "already confirmed");

    let (s, v) = b.get("/v1/account", Some(&t));
    assert_eq!(s, 200);
    assert_eq!((v["account_id"].as_i64(), &v["email_verified"], &v["profile"]["trainer_name"], &v["username"]), (Some(id), &json!(true), &json!("Ash"), &json!("Ash")));
    assert_eq!(b.post("/v1/world-tickets", json!({ "world": "w" }), Some(&t)).0, 200);
    // the username logs in as well as the address, in any case
    let (s, v) = b.post("/v1/login", json!({ "username": "aSH", "password": PASSWORD, "device": "phone" }), None);
    assert_eq!((s, v["account_id"].as_i64()), (200, Some(id)), "{v}");

    assert_eq!(b.post("/v1/logout", json!({}), Some(&t)).0, 204);
    assert_eq!(b.get("/v1/account", Some(&t)).0, 401, "a logged-out token is dead");
    assert_eq!(b.get("/v1/account", None).0, 401);
    assert_eq!(b.get("/v1/account", Some("rdx_s_forged")).0, 401);
    // what happened is in the audit log, and no secret is stored in the clear
    assert_eq!(b.count(&format!("SELECT count(*) FROM audit_log WHERE account_id = {id} AND event IN ('register', 'email_verified', 'login', 'logout')")), 5);
    assert_eq!(b.count("SELECT count(*) FROM accounts WHERE password_hash LIKE '$argon2id$v=19$m=19456,t=2,p=1$%'"), 1);
    assert_eq!(b.count(&format!("SELECT count(*) FROM sessions WHERE token_hash = convert_to('{t}', 'UTF8')")), 0);
}

#[test]
fn wrong_password_and_unknown_address_look_the_same() {
    let Some(b) = backend() else { return };
    b.player("misty@example.com");
    let wrong = b.post("/v1/login", json!({ "email": "misty@example.com", "password": "wrong password!" }), None);
    let unknown = b.post("/v1/login", json!({ "email": "nobody@example.com", "password": "wrong password!" }), None);
    assert_eq!(wrong.0, 401);
    assert_eq!(wrong, unknown);
    assert_eq!(b.count("SELECT count(*) FROM audit_log WHERE event = 'login_failed'"), 2);
}

#[test]
fn logins_and_registrations_are_rate_limited() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |c| {
        c.limits.login_account.max = 3;
        c.limits.login_ip.max = 8;
        c.limits.register_ip.max = 3;
    });
    b.player("brock@example.com"); // registration 1, login 1
    b.player("dawn@example.com"); // registration 2, login 2
    // three failures lock the account for the window, even against the right password; another account is fine
    for _ in 0..3 {
        assert_eq!(b.post("/v1/login", json!({ "email": "brock@example.com", "password": "wrong password!" }), None).0, 401);
    }
    assert_eq!(b.post("/v1/login", json!({ "email": "BROCK@example.com", "password": PASSWORD }), None).0, 429);
    assert_eq!(b.post("/v1/login", json!({ "email": "dawn@example.com", "password": PASSWORD }), None).0, 200); // login 7
    // the address: 8 attempts in the window, then refused whatever the account
    assert_eq!(b.post("/v1/login", json!({ "email": "dawn@example.com", "password": PASSWORD }), None).0, 200); // 8
    assert_eq!(b.post("/v1/login", json!({ "email": "dawn@example.com", "password": PASSWORD }), None).0, 429);
    // registrations from one address
    assert_eq!(b.post("/v1/register", json!({ "email": "c@example.com", "username": "cxyz", "password": PASSWORD, "trainer_name": "C" }), None).0, 202);
    assert_eq!(b.post("/v1/register", json!({ "email": "d@example.com", "username": "dxyz", "password": PASSWORD, "trainer_name": "D" }), None).0, 429);
}

#[test]
fn sessions_per_device_expire_and_revoke() {
    let Some(b) = backend() else { return };
    let (_, pc) = b.player("serena@example.com");
    let (s, v) = b.post("/v1/login", json!({ "email": "serena@example.com", "password": PASSWORD, "device": "laptop" }), None);
    assert_eq!(s, 200);
    let laptop = v["token"].as_str().unwrap().to_string();
    let laptop_id = v["session_id"].as_i64().unwrap();
    let (_, v) = b.get("/v1/sessions", Some(&pc));
    let list = v["sessions"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.iter().filter(|s| s["current"] == json!(true)).count(), 1);
    assert!(list.iter().any(|s| s["device"] == json!("laptop") && s["current"] == json!(false)));

    // another account cannot end this one's sessions (nor learn they exist)
    let (_, other) = b.player("calem@example.com");
    assert_eq!(b.delete(&format!("/v1/sessions/{laptop_id}"), Some(&other)).0, 404);
    assert_eq!(b.delete(&format!("/v1/sessions/{laptop_id}"), Some(&pc)).0, 204);
    assert_eq!(b.get("/v1/account", Some(&laptop)).0, 401, "revoked from another device");
    assert_eq!(b.get("/v1/account", Some(&pc)).0, 200);

    // expiry
    b.sql("UPDATE sessions SET expires_at = now() - interval '1 second' WHERE account_id = (SELECT id FROM accounts WHERE email_norm = 'serena@example.com')");
    assert_eq!(b.get("/v1/account", Some(&pc)).0, 401, "an expired session is dead");

    // logout-all ends every device
    let tokens: Vec<String> = (0..3)
        .map(|i| b.post("/v1/login", json!({ "email": "calem@example.com", "password": PASSWORD, "device": format!("d{i}") }), None).1["token"].as_str().unwrap().to_string())
        .collect();
    let (s, v) = b.post("/v1/logout-all", json!({}), Some(&tokens[0]));
    assert_eq!((s, v["revoked"].as_i64()), (200, Some(4)));
    assert!(tokens.iter().chain([&other]).all(|t| b.get("/v1/account", Some(t)).0 == 401));
}

#[test]
fn password_reset_by_mail() {
    let Some(b) = backend() else { return };
    let (_, old_session) = b.player("may@example.com");
    // an unknown address gets the same answer and no mail
    assert_eq!(b.post("/v1/password/forgot", json!({ "email": "ghost@example.com" }), None).0, 202);
    assert_eq!(b.post("/v1/password/forgot", json!({ "email": "MAY@example.com" }), None).0, 202);
    let code = b.mail_code("may@example.com", 2);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(b.mails("ghost@example.com").is_empty());
    assert_eq!(b.post("/v1/password/reset", json!({ "token": code, "password": "short" }), None).0, 400);
    assert_eq!(b.post("/v1/password/reset", json!({ "token": code, "password": "a brand new password" }), None).0, 200);
    assert_eq!(b.post("/v1/password/reset", json!({ "token": code, "password": "yet another password" }), None).0, 400, "a code works once");
    assert_eq!(b.get("/v1/account", Some(&old_session)).0, 401, "a reset ends every session");
    assert_eq!(b.post("/v1/login", json!({ "email": "may@example.com", "password": PASSWORD }), None).0, 401);
    let (s, v) = b.post("/v1/login", json!({ "email": "may@example.com", "password": "a brand new password" }), None);
    assert_eq!(s, 200);
    let t = v["token"].as_str().unwrap().to_string();
    // an expired code does nothing
    assert_eq!(b.post("/v1/password/forgot", json!({ "email": "may@example.com" }), None).0, 202);
    let late = b.mail_code("may@example.com", 3);
    b.sql("UPDATE email_tokens SET expires_at = now() - interval '1 second' WHERE purpose = 'reset' AND used_at IS NULL");
    assert_eq!(b.post("/v1/password/reset", json!({ "token": late, "password": "too late password" }), None).0, 400);
    // a logged-in change needs the current password and keeps only this session
    assert_eq!(b.post("/v1/password/change", json!({ "current_password": "wrong one!", "new_password": "another new password" }), Some(&t)).0, 401);
    assert_eq!(b.post("/v1/password/change", json!({ "current_password": "a brand new password", "new_password": "another new password" }), Some(&t)).0, 200);
    assert_eq!(b.get("/v1/account", Some(&t)).0, 200);
    assert_eq!(b.count("SELECT count(*) FROM audit_log WHERE event IN ('password_reset_requested', 'password_reset', 'password_changed')"), 4);
}

/// 8 registrations at once, all through the HTTP API; the status codes.
fn race(b: &std::sync::Arc<Backend>, body: impl Fn(usize) -> serde_json::Value + Send + Sync + 'static) -> Vec<u16> {
    let go = std::sync::Arc::new(std::sync::Barrier::new(8));
    let body = std::sync::Arc::new(body);
    (0..8)
        .map(|i| {
            let (b, go, body) = (b.clone(), go.clone(), body.clone());
            std::thread::spawn(move || {
                go.wait();
                b.post("/v1/register", body(i), None).0
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect()
}

#[test]
fn concurrent_registrations_of_one_address_or_username_make_one_account() {
    let Some(a) = admin_url() else { return };
    let b = std::sync::Arc::new(Backend::start(&a, |c| c.limits.register_ip.max = 100));
    // one address (in two spellings), eight usernames
    let codes = race(&b, |i| {
        let email = if i % 2 == 0 { "Lillie@example.com" } else { "lillie@EXAMPLE.com" };
        json!({ "email": email, "username": format!("lillie{i}"), "password": PASSWORD, "trainer_name": format!("L{i}") })
    });
    // a taken address answers like a new account: all eight say 202, one account was made
    assert!(codes.iter().all(|&c| c == 202), "{codes:?}");
    assert_eq!(b.count("SELECT count(*) FROM accounts"), 1);
    // eight addresses, one username (in two cases)
    let codes = race(&b, |i| {
        let name = if i % 2 == 0 { "Gladion" } else { "gladion" };
        json!({ "email": format!("g{i}@example.com"), "username": name, "password": PASSWORD, "trainer_name": "G" })
    });
    assert_eq!(codes.iter().filter(|&&c| c == 202).count(), 1, "{codes:?}");
    assert_eq!(codes.iter().filter(|&&c| c == 409).count(), 7, "{codes:?}");
    assert_eq!(b.count("SELECT count(*) FROM accounts"), 2);
    assert_eq!(b.count("SELECT count(*) FROM trainer_profiles"), 2);
    assert_eq!(b.count("SELECT count(*) FROM account_settings"), 2);
}

#[test]
fn settings_and_trainer_profile() {
    let Some(b) = backend() else { return };
    let (_, t) = b.player("hau@example.com");
    let (s, v) = b.send("PATCH", "/v1/account/settings", json!({ "language": "fr-CA", "show_online": false }), Some(&t));
    assert_eq!((s, v), (200, json!({ "language": "fr-CA", "show_online": false, "allow_whispers": true })));
    assert_eq!(b.send("PATCH", "/v1/account/settings", json!({ "language": "<script>" }), Some(&t)).0, 400);
    assert_eq!(b.send("PATCH", "/v1/account/settings", json!({ "admin": true }), Some(&t)).0, 422, "unknown fields are refused");
    let (s, v) = b.send("PUT", "/v1/profile", json!({ "trainer_name": "  Hau\u{200B} the great and powerful" }), Some(&t));
    assert_eq!((s, &v["trainer_name"]), (200, &json!("Hau the great an")));
    let (_, v) = b.get("/v1/profile", Some(&t));
    assert_eq!(v["trainer_name"], json!("Hau the great an"));
    assert_eq!(b.get("/v1/account", Some(&t)).1["settings"]["show_online"], json!(false));
    assert_eq!(b.send("PUT", "/v1/profile", json!({ "trainer_name": "" }), Some(&t)).0, 400);
}

/// World tickets: one use, short-lived, for one world, dead with their session; only a world server (the secret) checks.
#[test]
fn world_tickets_verify_once() {
    let Some(b) = backend() else { return };
    let (id, t) = b.player("lana@example.com");
    let ticket = |world: &str| b.post("/v1/world-tickets", json!({ "world": world }), Some(&t)).1["ticket"].as_str().unwrap().to_string();
    let verify = |ticket: &str, world: &str, secret: &str| b.post("/internal/world-tickets/verify", json!({ "ticket": ticket, "world": world }), Some(secret));

    let k = ticket("world.example:7777");
    assert!(k.starts_with("rdx_t_") && k.len() == 49);
    assert_eq!(verify(&k, "world.example:7777", "not the secret").0, 401, "a stranger cannot check tickets");
    let (s, v) = verify(&k, "World.Example:7777", SECRET); // host names in any case
    // the world learns the public id only, never the sequential account id
    let public = b.get("/v1/account", Some(&t)).1["public_id"].as_i64().unwrap();
    assert_ne!(public, id);
    assert_eq!((s, &v["public_id"], &v["trainer_name"], &v["account_id"]), (200, &json!(public), &json!("Calem"), &serde_json::Value::Null), "valid");
    let session = v["session_id"].as_i64().expect("the backend session, for the world's re-checks");
    let check = |s: i64| b.post("/internal/sessions/check", json!({ "sessions": [s, 999_999] }), Some(SECRET)).1["dead"].clone();
    assert_eq!(check(session), json!([999_999]), "a live session is not dead; an unknown one is");
    assert_eq!(b.post("/internal/sessions/check", json!({ "sessions": [session] }), Some("not the secret")).0, 401);
    assert_eq!(verify(&k, "world.example:7777", SECRET).1["reason"], json!("used"), "replayed");

    let k = ticket("world.example:7777");
    b.sql("UPDATE world_tickets SET expires_at = now() - interval '1 second' WHERE used_at IS NULL");
    assert_eq!(verify(&k, "world.example:7777", SECRET).1["reason"], json!("expired"));

    let forged = format!("rdx_t_{}", "A".repeat(43));
    assert_eq!(verify(&forged, "world.example:7777", SECRET), (401, json!({ "error": "unauthorized", "reason": "unknown" })), "forged");

    let k = ticket("world.example:7777");
    assert_eq!(verify(&k, "other.example:7777", SECRET).1["reason"], json!("world"), "another world's");
    assert_eq!(verify(&k, "world.example:7777", SECRET).0, 200, "a refusal for the wrong world does not use it up");

    // tickets die with their session
    let k = ticket("world.example:7777");
    assert_eq!(b.post("/v1/logout", json!({}), Some(&t)).0, 204);
    assert_eq!(verify(&k, "world.example:7777", SECRET).1["reason"], json!("session"));
    assert_eq!(b.post("/v1/world-tickets", json!({ "world": "w" }), Some(&t)).0, 401);
}

/// Two worlds checking one ticket at once: exactly one gets the account.
#[test]
fn a_ticket_verified_concurrently_logs_in_once() {
    let Some(a) = admin_url() else { return };
    let b = std::sync::Arc::new(Backend::start(&a, |_| {}));
    let (_, t) = b.player("kiawe@example.com");
    let k = b.post("/v1/world-tickets", json!({ "world": "w:1" }), Some(&t)).1["ticket"].as_str().unwrap().to_string();
    let go = std::sync::Arc::new(std::sync::Barrier::new(8));
    let ok = (0..8)
        .map(|_| {
            let (b, go, k) = (b.clone(), go.clone(), k.clone());
            std::thread::spawn(move || {
                go.wait();
                b.post("/internal/world-tickets/verify", json!({ "ticket": k, "world": "w:1" }), Some(SECRET)).0
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .filter(|_| true)
        .map(|t| t.join().unwrap())
        .filter(|&s| s == 200)
        .count();
    assert_eq!(ok, 1);
}

#[test]
fn tickets_are_rate_limited_per_account() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |c| c.limits.tickets.max = 3);
    let (_, t) = b.player("mallow@example.com");
    for _ in 0..3 {
        assert_eq!(b.post("/v1/world-tickets", json!({ "world": "w" }), Some(&t)).0, 200);
    }
    assert_eq!(b.post("/v1/world-tickets", json!({ "world": "w" }), Some(&t)).0, 429);
}

/// Two devices log in to one account at the same moment: two sessions, both live, each revocable alone.
#[test]
fn concurrent_logins_on_two_devices() {
    let Some(a) = admin_url() else { return };
    let b = std::sync::Arc::new(Backend::start(&a, |_| {}));
    b.player("sophocles@example.com");
    let go = std::sync::Arc::new(std::sync::Barrier::new(2));
    let tokens: Vec<String> = ["pc", "laptop"]
        .map(|device| {
            let (b, go) = (b.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                let (s, v) = b.post("/v1/login", json!({ "email": "sophocles@example.com", "password": PASSWORD, "device": device }), None);
                assert_eq!(s, 200, "{v}");
                v["token"].as_str().unwrap().to_string()
            })
        })
        .map(|t| t.join().unwrap())
        .to_vec();
    assert_ne!(tokens[0], tokens[1]);
    let (_, v) = b.get("/v1/sessions", Some(&tokens[0]));
    assert_eq!(v["sessions"].as_array().unwrap().len(), 3, "the player() login and the two devices: {v}");
    assert_eq!(b.post("/v1/logout", json!({}), Some(&tokens[0])).0, 204);
    assert_eq!(b.get("/v1/account", Some(&tokens[0])).0, 401);
    assert_eq!(b.get("/v1/account", Some(&tokens[1])).0, 200, "the other device stays logged in");
}

/// Everything lives in Postgres: after a restart the account, its session, its characters and its settings are there.
#[test]
fn accounts_and_sessions_survive_a_backend_restart() {
    let Some(a) = admin_url() else { return };
    let mut b = Backend::start(&a, |_| {});
    let (id, t) = b.player("olivia@example.com");
    let girl = "060101c00024024c0590020004000000000000";
    let c = json!({ "name": "Olivia", "look": girl, "created": 1, "changed": 2 });
    assert_eq!(b.send("PUT", "/v1/characters/00000000000000aa", c, Some(&t)).0, 200);
    assert_eq!(b.send("PATCH", "/v1/account/settings", json!({ "language": "de" }), Some(&t)).0, 200);
    let old_url = b.url.clone();
    b.restart();
    assert_ne!(b.url, old_url, "a new server");
    assert_eq!(b.get("/healthz", None).0, 200);
    let (s, v) = b.get("/v1/account", Some(&t));
    assert_eq!((s, v["account_id"].as_i64(), &v["settings"]["language"]), (200, Some(id), &json!("de")), "the session outlived the restart: {v}");
    assert_eq!(b.get("/v1/characters", Some(&t)).1["characters"][0]["name"], json!("Olivia"));
    assert_eq!(b.post("/v1/login", json!({ "email": "olivia", "password": PASSWORD }), None).0, 200);
}

/// The account's characters: newest `changed` wins, at most eight, checked like the world checks them; the world
/// servers reach them only with the secret.
#[test]
fn account_characters_merge_like_the_world() {
    let Some(b) = backend() else { return };
    let (id, t) = b.player("acerola@example.com");
    let girl = "060101c00024024c0590020004000000000000";
    let put = |cid: &str, name: &str, changed: u64| b.send("PUT", &format!("/v1/characters/{cid}"), json!({ "name": name, "look": girl, "created": 1, "changed": changed }), Some(&t));
    assert_eq!(put("0000000000000001", "First", 10).0, 200);
    let (_, v) = put("0000000000000001", "Older", 9);
    assert_eq!(v["characters"][0]["name"], json!("First"), "an older copy changes nothing");
    let (_, v) = put("0000000000000001", "Newer", 11);
    assert_eq!(v["characters"][0]["name"], json!("Newer"));
    for i in 2..=8 {
        assert_eq!(put(&format!("{i:016x}"), "x", 1).0, 200);
    }
    let (_, v) = put("0000000000000063", "Ninth", 1);
    assert_eq!(v["characters"].as_array().unwrap().len(), 8, "a ninth does not go in");
    assert_eq!(put("0000000000000000", "Zero", 1).0, 400);
    assert_eq!(b.send("PUT", "/v1/characters/0000000000000002", json!({ "name": "x", "look": "ff", "created": 1, "changed": 1 }), Some(&t)).0, 400, "a bad look");
    assert_eq!(b.delete("/v1/characters/0000000000000002", Some(&t)).0, 204);
    assert_eq!(b.delete("/v1/characters/0000000000000002", Some(&t)).0, 404);
    // the world's side: only with the secret
    // the world names accounts by their public id
    let public = b.get("/v1/account", Some(&t)).1["public_id"].as_i64().unwrap();
    assert_eq!(b.get(&format!("/internal/accounts/{public}/characters"), Some("nope")).0, 401);
    assert_eq!(b.get(&format!("/internal/accounts/{id}/characters"), Some(SECRET)).0, 404, "not by the sequential id");
    let (s, v) = b.get(&format!("/internal/accounts/{public}/characters"), Some(SECRET));
    assert_eq!((s, v["characters"].as_array().unwrap().len()), (200, 7));
    let w = b.send("PUT", &format!("/internal/accounts/{public}/characters"), json!({ "id": "0000000000000002", "name": "Back", "look": girl, "created": 1, "changed": 5 }), Some(SECRET));
    assert_eq!((w.0, w.1["characters"].as_array().unwrap().len()), (200, 8));
    assert_eq!(b.send("PUT", "/internal/accounts/999999/characters", json!({ "id": "0000000000000002", "name": "x", "look": girl, "created": 1, "changed": 5 }), Some(SECRET)).0, 404);
}

/// Wrong passwords fired in parallel from one address cannot get more guesses than the limit (each is counted before
/// its password is hashed), and they do not lock out the owner logging in from another address.
#[test]
fn failed_logins_are_counted_before_hashing_and_do_not_lock_out_the_owner() {
    let Some(a) = admin_url() else { return };
    let b = std::sync::Arc::new(Backend::start(&a, |c| {
        c.trust_proxy = true;
        c.limits.login_account.max = 3;
        c.limits.login_ip.max = 1000;
    }));
    b.player("guzma@example.com");
    let go = std::sync::Arc::new(std::sync::Barrier::new(12));
    let codes: Vec<u16> = (0..12)
        .map(|_| {
            let (b, go) = (b.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                b.post_from("203.0.113.7", "/v1/login", json!({ "email": "guzma", "password": "wrong password!" })).0
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    println!("12 parallel wrong passwords: {codes:?}");
    assert!(codes.iter().filter(|&&c| c == 401).count() <= 3, "more guesses than the limit: {codes:?}");
    assert!(codes.iter().all(|&c| c == 401 || c == 429), "{codes:?}");
    // the owner, from another address, still gets in
    assert_eq!(b.post_from("198.51.100.9", "/v1/login", json!({ "email": "guzma@example.com", "password": PASSWORD })).0, 200);
}

/// Accounts never confirmed within 7 days are deleted by `prune`, and their usernames can be taken again.
#[test]
fn unconfirmed_accounts_are_pruned_after_seven_days() {
    let Some(b) = backend() else { return };
    let reg = |email: &str| b.post("/v1/register", json!({ "email": email, "username": "Kukui", "password": PASSWORD, "trainer_name": "K" }), None).0;
    assert_eq!(reg("kukui@example.com"), 202);
    b.player("burnet@example.com"); // confirmed: stays however old
    assert_eq!(reg("other@example.com"), 409, "the username is taken while the account lives");
    b.sql("UPDATE accounts SET created_at = now() - interval '6 days'");
    b.rt.block_on(redex_backend::prune(&b.db)).unwrap();
    assert_eq!(b.count("SELECT count(*) FROM accounts"), 2, "six days: kept");
    b.sql("UPDATE accounts SET created_at = now() - interval '8 days'");
    b.rt.block_on(redex_backend::prune(&b.db)).unwrap();
    assert_eq!(b.count("SELECT count(*) FROM accounts WHERE username_norm = 'kukui'"), 0, "pruned");
    assert_eq!(b.count("SELECT count(*) FROM accounts WHERE username_norm = 'burnet'"), 1, "a confirmed account stays");
    assert_eq!(b.count("SELECT count(*) FROM email_tokens e LEFT JOIN accounts a ON a.id = e.account_id WHERE a.id IS NULL"), 0);
    assert_eq!(reg("other@example.com"), 202, "the username is free again");
}

/// Login limits count an IPv6 /64 as one address: 2^64 addresses are not 2^64 times the guesses.
#[test]
fn an_ipv6_64_counts_as_one_address() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |c| {
        c.trust_proxy = true;
        c.limits.login_ip.max = 3;
    });
    for i in 1..=3 {
        assert_eq!(b.post_from(&format!("2001:db8:5:6::{i:x}"), "/v1/login", json!({ "email": "nobody", "password": "wrong password!" })).0, 401);
    }
    assert_eq!(b.post_from("2001:db8:5:6:ffff::9", "/v1/login", json!({ "email": "nobody", "password": "wrong password!" })).0, 429, "the same /64");
    assert_eq!(b.post_from("2001:db8:5:7::1", "/v1/login", json!({ "email": "nobody", "password": "wrong password!" })).0, 401, "another /64");
}
/// Guesses from many addresses push an account over its total limit; a new address is then refused, but the owner
/// logging in from an address one of their sessions used still gets in.
#[test]
fn the_owner_still_logs_in_from_a_known_address_when_the_total_is_over() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |c| {
        c.trust_proxy = true;
        c.limits.login_account.max = 2;
        c.limits.login_account_total.max = 4;
        c.limits.login_ip.max = 1000;
    });
    b.player("mina@example.com");
    let right = json!({ "email": "mina", "password": PASSWORD });
    let wrong = json!({ "email": "mina", "password": "wrong password!" });
    assert_eq!(b.post_from("198.51.100.20", "/v1/login", right.clone()).0, 200, "the owner's usual address");
    for net in 1..=3 {
        for _ in 0..2 {
            b.post_from(&format!("2001:db8:{net}::1"), "/v1/login", wrong.clone());
        }
    }
    assert_eq!(b.post_from("2001:db8:99::1", "/v1/login", right.clone()).0, 429, "a new address, the total over");
    assert_eq!(b.post_from("198.51.100.20", "/v1/login", right).0, 200, "the owner from a known address");
}

/// With every hashing slot busy, a request waits about 2 s for one at most and then gets 503, instead of queueing
/// without end behind everyone else's.
#[test]
fn waits_for_a_hashing_slot_are_bounded() {
    let Some(a) = admin_url() else { return };
    let b = std::sync::Arc::new(Backend::start(&a, |c| {
        c.trust_proxy = true;
        c.hash_slots = 1;
        c.limits.login_ip.max = 10_000;
    }));
    // what one hash takes here (debug builds are slow)
    let t0 = std::time::Instant::now();
    assert_eq!(b.post_from("192.0.2.1", "/v1/login", json!({ "email": "nobody0", "password": "wrong password!" })).0, 401);
    let one = t0.elapsed();
    let go = std::sync::Arc::new(std::sync::Barrier::new(40));
    let answers: Vec<(u16, std::time::Duration)> = (0..40)
        .map(|i| {
            let (b, go) = (b.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                let t = std::time::Instant::now();
                let s = b.post_from(&format!("192.0.2.{}", i + 2), "/v1/login", json!({ "email": format!("nobody{}", i + 1), "password": "wrong password!" })).0;
                (s, t.elapsed())
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let busy = answers.iter().filter(|a| a.0 == 503).count();
    let longest = answers.iter().map(|a| a.1).max().unwrap();
    println!("one hash {one:?}; 40 at once on one slot: {busy} got 503, the longest answer took {longest:?}");
    assert!(busy > 0, "nobody was told the backend is busy: {answers:?}");
    assert!(longest < std::time::Duration::from_secs(3) + one * 3, "an answer took {longest:?} (one hash {one:?})");
    assert!(answers.iter().all(|a| a.0 == 401 || a.0 == 503), "{answers:?}");
}

/// The pages behind the mail links: plain HTML on this origin, a CSP that allows only their own inline script and
/// style (by hash) and requests to this origin; the code is read from the fragment, so it never reaches a server log.
#[test]
fn the_mail_link_pages_are_strict() {
    let Some(b) = backend() else { return };
    for path in ["/verify-email", "/reset-password"] {
        let (s, csp, body) = b.get_raw(path);
        assert_eq!(s, 200, "{path}");
        let csp = csp.expect("a CSP header");
        assert!(csp.starts_with("default-src 'none'; script-src 'sha256-") && csp.contains("connect-src 'self'") && csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(!body.contains(" src=") && !body.contains("http"), "{path} loads nothing from elsewhere");
        assert!(body.contains("location.hash") && body.contains("history.replaceState"), "{path} reads the fragment and clears it");
    }
    // the verification mail links to the page and says what happens to unconfirmed accounts
    b.post("/v1/register", json!({ "email": "lusamine@example.com", "username": "lusamine", "password": PASSWORD, "trainer_name": "L" }), None);
    b.mail_code("lusamine@example.com", 1);
    let mail = &b.mails("lusamine@example.com")[0];
    assert!(mail.contains("/verify-email#token=rdx_m_") && mail.contains("deleted"), "{mail}");
}
/// A login name that is just a number never counts against the account with that id: wrong passwords typed as "<id>"
/// do not lock that account out of the address they came from.
#[test]
fn a_numeric_login_name_does_not_hit_an_account_ids_counters() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |c| {
        c.trust_proxy = true;
        c.limits.login_account.max = 2;
        c.limits.login_ip.max = 1000;
    });
    let (id, _) = b.player("plumeria@example.com");
    for _ in 0..3 {
        b.post_from("203.0.113.50", "/v1/login", json!({ "email": id.to_string(), "password": "wrong password!" }));
    }
    assert_eq!(b.post_from("203.0.113.50", "/v1/login", json!({ "email": "plumeria", "password": PASSWORD })).0, 200);
}

/// "Log out everywhere" also forgets the addresses the account's sessions came from: one a hijacker used is no longer
/// let in past the per-account total, while the owner's address from a login after it is.
#[test]
fn logout_all_forgets_known_addresses() {
    let Some(a) = admin_url() else { return };
    let b = Backend::start(&a, |c| {
        c.trust_proxy = true;
        c.limits.login_account.max = 2;
        c.limits.login_account_total.max = 4;
        c.limits.login_ip.max = 1000;
    });
    b.player("faba@example.com");
    let right = json!({ "email": "faba", "password": PASSWORD });
    let (s, v) = b.post_from("198.51.100.30", "/v1/login", right.clone()); // the hijacker, with the stolen password
    assert_eq!(s, 200);
    let stolen = v["token"].as_str().unwrap().to_string();
    // the owner logs out everywhere, then logs in again from home
    assert_eq!(b.post("/v1/logout-all", json!({}), Some(&stolen)).0, 200);
    assert_eq!(b.post_from("192.0.2.40", "/v1/login", right.clone()).0, 200);
    for net in 1..=3 {
        for _ in 0..2 {
            b.post_from(&format!("2001:db8:a{net}::1"), "/v1/login", json!({ "email": "faba", "password": "wrong password!" }));
        }
    }
    assert_eq!(b.post_from("198.51.100.30", "/v1/login", right.clone()).0, 429, "the hijacker's address is forgotten");
    assert_eq!(b.post_from("192.0.2.40", "/v1/login", right).0, 200, "the owner's address since is known");
}
