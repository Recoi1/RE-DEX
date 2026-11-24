//! A real backend on a real Postgres for each test: a fresh database (dropped afterwards), migrations run, the server on
//! a loopback port, mails written to a folder. `REDEX_TEST_PG` names the admin connection, e.g.
//! `postgres://postgres@127.0.0.1:54329/postgres`; without it the tests say SKIPPED and pass.
#![allow(dead_code)]
use redex_backend::config::{Config, Limits, MailConfig};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub const SECRET: &str = "test-world-secret-0123456789abcdef0123456789";
pub const PASSWORD: &str = "correct horse battery";

pub struct Backend {
    pub url: String,
    pub mail_dir: PathBuf,
    pub db: sqlx::PgPool,
    pub rt: tokio::runtime::Runtime,
    admin: String,
    name: String,
    agent: ureq::Agent,
}

pub fn admin_url() -> Option<String> {
    let u = std::env::var("REDEX_TEST_PG").ok().filter(|u| !u.is_empty());
    if u.is_none() {
        eprintln!("SKIPPED: set REDEX_TEST_PG to a Postgres admin URL to run the backend tests");
    }
    u
}

/// The username the tests give an address: its local part.
pub fn username(email: &str) -> String {
    email.split('@').next().unwrap().to_string()
}

/// `admin` with its database replaced by `db`.
fn with_db(admin: &str, db: &str) -> String {
    let (base, _) = admin.rsplit_once('/').expect("a URL with a database");
    format!("{base}/{db}")
}

impl Backend {
    pub fn start(admin: &str, tune: impl FnOnce(&mut Config)) -> Backend {
        static N: AtomicU32 = AtomicU32::new(0);
        let name = format!("redex_t_{}_{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let mail_dir = std::env::temp_dir().join(format!("redex-mail-{name}"));
        let _ = std::fs::remove_dir_all(&mail_dir);
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.block_on(async {
            use sqlx::Connection;
            let mut c = sqlx::PgConnection::connect(admin).await.expect("REDEX_TEST_PG reachable");
            sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).execute(&mut c).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE {name}")).execute(&mut c).await.unwrap();
        });
        let (db, url) = Self::launch(&rt, admin, &name, &mail_dir, tune);
        let agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(20))).build().into();
        Backend { url, mail_dir, db, rt, admin: admin.into(), name, agent }
    }

    /// The backend stopped (its runtime, pool and listener gone) and started again on the same database, as after a
    /// restart: a new process would see exactly this. The URL changes (a new port).
    pub fn restart(&mut self) {
        let old = std::mem::replace(&mut self.rt, tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap());
        let db = self.db.clone();
        old.block_on(db.close());
        old.shutdown_timeout(Duration::from_secs(5));
        (self.db, self.url) = Self::launch(&self.rt, &self.admin, &self.name, &self.mail_dir, |_| {});
    }

    /// Migrations (again: a no-op after the first) and the server on a loopback port.
    fn launch(rt: &tokio::runtime::Runtime, admin: &str, name: &str, mail_dir: &std::path::Path, tune: impl FnOnce(&mut Config)) -> (sqlx::PgPool, String) {
        rt.block_on(async {
            let db = sqlx::postgres::PgPoolOptions::new().max_connections(20).connect(&with_db(admin, name)).await.unwrap();
            redex_backend::MIGRATOR.run(&db).await.unwrap();
            let mut cfg = Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                database_url: with_db(admin, name),
                world_secret: SECRET.into(),
                public_url: "http://example.invalid".into(),
                mail: MailConfig::Dir(mail_dir.to_path_buf()),
                mail_from: "RE:DEX <no-reply@example.invalid>".into(),
                trust_proxy: false,
                session_ttl_secs: 3600,
                ticket_ttl_secs: 60,
                verify_ttl_secs: 3600,
                reset_ttl_secs: 3600,
                limits: Limits::default(),
                hash_slots: 4,
            };
            tune(&mut cfg);
            let app = redex_backend::App::new(db.clone(), cfg).unwrap();
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            tokio::spawn(async move {
                axum::serve(l, redex_backend::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
            });
            (db, url)
        })
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>, bearer: Option<&str>) -> (u16, Value) {
        let url = format!("{}{path}", self.url);
        let auth = bearer.map(|t| format!("Bearer {t}"));
        let r = match (method, body) {
            ("GET", _) => {
                let mut q = self.agent.get(&url);
                if let Some(a) = &auth { q = q.header("Authorization", a) }
                q.call()
            }
            ("DELETE", _) => {
                let mut q = self.agent.delete(&url);
                if let Some(a) = &auth { q = q.header("Authorization", a) }
                q.call()
            }
            (m, b) => {
                let mut q = match m { "PATCH" => self.agent.patch(&url), "PUT" => self.agent.put(&url), _ => self.agent.post(&url) };
                if let Some(a) = &auth { q = q.header("Authorization", a) }
                q.send_json(b.unwrap_or(json!({})))
            }
        };
        let mut r = r.expect("backend reachable");
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub fn post(&self, path: &str, body: Value, bearer: Option<&str>) -> (u16, Value) {
        self.call("POST", path, Some(body), bearer)
    }
    pub fn get(&self, path: &str, bearer: Option<&str>) -> (u16, Value) {
        self.call("GET", path, None, bearer)
    }
    pub fn delete(&self, path: &str, bearer: Option<&str>) -> (u16, Value) {
        self.call("DELETE", path, None, bearer)
    }
    pub fn send(&self, method: &str, path: &str, body: Value, bearer: Option<&str>) -> (u16, Value) {
        self.call(method, path, Some(body), bearer)
    }

    /// A GET as text: (status, its Content-Security-Policy header, body).
    pub fn get_raw(&self, path: &str) -> (u16, Option<String>, String) {
        let mut r = self.agent.get(&format!("{}{path}", self.url)).call().expect("backend reachable");
        let csp = r.headers().get("content-security-policy").and_then(|v| v.to_str().ok()).map(String::from);
        (r.status().as_u16(), csp, r.body_mut().read_to_string().unwrap_or_default())
    }

    /// A POST as if from `ip` (an `X-Forwarded-For` the backend trusts when started with `trust_proxy`).
    pub fn post_from(&self, ip: &str, path: &str, body: Value) -> (u16, Value) {
        let mut r = self.agent.post(&format!("{}{path}", self.url)).header("X-Forwarded-For", ip).send_json(body).expect("backend reachable");
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Runs SQL on the test database (to age a session, a ticket or a mail token).
    pub fn sql(&self, q: &str) -> u64 {
        self.rt.block_on(sqlx::query(q).execute(&self.db)).unwrap().rows_affected()
    }
    pub fn count(&self, q: &str) -> i64 {
        self.rt.block_on(sqlx::query_scalar::<_, i64>(q).fetch_one(&self.db)).unwrap()
    }

    /// Mails sent to `to` so far, oldest first.
    pub fn mails(&self, to: &str) -> Vec<String> {
        let mut files: Vec<_> = std::fs::read_dir(&self.mail_dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        files.into_iter().filter_map(|p| std::fs::read_to_string(p).ok()).filter(|m| m.starts_with(&format!("To: {to}\r\n"))).collect()
    }

    /// The code in the `n`th mail (from 1) to `to`, waiting up to 5 s for it (mail goes out in the background).
    pub fn mail_code(&self, to: &str, n: usize) -> String {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(m) = self.mails(to).get(n - 1) {
                let i = m.rfind("rdx_m_").expect("a code in the mail");
                return m[i..i + 49].to_string();
            }
            assert!(Instant::now() < until, "no mail {n} to {to}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Registers, confirms the address and logs in; (account id, session token).
    pub fn player(&self, email: &str) -> (i64, String) {
        let (s, v) = self.post("/v1/register", json!({ "email": email, "username": username(email), "password": PASSWORD, "trainer_name": "Calem" }), None);
        assert_eq!(s, 202, "{v}");
        let code = self.mail_code(email, 1);
        assert_eq!(self.post("/v1/verify-email", json!({ "token": code }), None).0, 200);
        let (s, v) = self.post("/v1/login", json!({ "email": email, "password": PASSWORD, "device": "test" }), None);
        assert_eq!(s, 200, "{v}");
        (v["account_id"].as_i64().unwrap(), v["token"].as_str().unwrap().to_string())
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let (admin, name) = (self.admin.clone(), self.name.clone());
        let db = self.db.clone();
        self.rt.block_on(async move {
            use sqlx::Connection;
            db.close().await;
            if let Ok(mut c) = sqlx::PgConnection::connect(&admin).await {
                let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).execute(&mut c).await;
            }
        });
        let _ = std::fs::remove_dir_all(&self.mail_dir);
    }
}
