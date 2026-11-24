//! Transport seam: TCP (dev, LAN), TCP + TLS (the central world, feature `central`) and Steam Networking Sockets P2P
//! over Steam's relays (`SteamTransport`, feature `steam`). TLS carries the same frames as plain TCP.
use redex_proto::{Decoder, Msg};
#[cfg(feature = "steam")]
use steamworks::networking_sockets::{ListenSocket, NetConnection};
#[cfg(feature = "steam")]
use steamworks::networking_types::{ListenSocketEvent, NetworkingConfigEntry, NetworkingConfigValue, NetworkingConnectionState, SendFlags};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub enum Recv {
    Msg(Msg),
    /// Nothing arrived within the wait.
    Idle,
    Closed,
}

/// One connected peer. Protocol violations surface as `InvalidData` errors.
pub trait Link: Send {
    fn recv(&mut self, wait: Duration) -> io::Result<Recv>;
    fn send(&mut self, m: &Msg) -> io::Result<()>;
    /// Address used for per-peer connection limits.
    fn peer(&self) -> String;
    /// Called once after login: switches to the post-login frame limit.
    fn set_live(&mut self);
    /// The SteamID Steam's P2P layer authenticated for this connection (Steam transport only).
    fn steam_id(&self) -> Option<u64> {
        None
    }
    /// Whether a `Refuse` can be written now without waiting on the peer: false for TLS before its handshake (the
    /// write would first run the handshake, which a silent peer holds forever), so the accept thread just drops those.
    fn refusable(&self) -> bool {
        true
    }
}

/// What a peer address counts as for the per-peer connection cap: an IPv4 address itself, an IPv6 address by its /64
/// (one host gets a whole /64), an IPv4-mapped IPv6 address as its IPv4.
pub fn peer_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
        v4 => v4.to_string(),
    }
}

/// How long a TLS handshake may take in all, however its bytes trickle in.
pub const HANDSHAKE: Duration = Duration::from_secs(5);

pub trait Transport: Send + 'static {
    /// `None` when nothing connected within `wait`.
    fn accept(&self, wait: Duration) -> io::Result<Option<Box<dyn Link>>>;
}

impl Transport for Box<dyn Transport> {
    fn accept(&self, wait: Duration) -> io::Result<Option<Box<dyn Link>>> {
        (**self).accept(wait)
    }
}

/// Jitter and loss on the send side, so tests can check movement under 50-150 ms and drops.
/// Only relayed positions are dropped: TCP itself never loses data, so state messages must not be lost.
#[cfg(any(test, feature = "sim"))]
#[derive(Clone, Copy)]
pub struct Sim {
    pub min_ms: u64,
    pub max_ms: u64,
    /// Percent of `PeerPos` dropped.
    pub loss_pct: u64,
    pub seed: u64,
}

pub struct TcpTransport {
    l: TcpListener,
    #[cfg(any(test, feature = "sim"))]
    sim: Option<Sim>,
    /// Every connection speaks TLS first (`with_tls`).
    #[cfg(feature = "central")]
    tls: Option<std::sync::Arc<rustls::ServerConfig>>,
}

/// How often the certificate files are looked at again (`ReloadingCert`).
#[cfg(feature = "central")]
pub const CERT_CHECK: Duration = Duration::from_secs(600);

/// The TLS server setup for a PEM certificate chain and private key (Caddy-issued, or self-signed for tests), reloaded
/// when the files change (`ReloadingCert`, checked every `CERT_CHECK`). Only TLS 1.2 and 1.3 with rustls' safe
/// defaults (ring).
#[cfg(feature = "central")]
pub fn tls_config(cert_pem: &std::path::Path, key_pem: &std::path::Path) -> io::Result<std::sync::Arc<rustls::ServerConfig>> {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let certs = ReloadingCert::new(cert_pem, key_pem, provider.clone(), CERT_CHECK)?;
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_cert_resolver(std::sync::Arc::new(certs));
    Ok(std::sync::Arc::new(cfg))
}

/// The world's certificate and key from two PEM files, read again when either file's time or size changed (Caddy
/// renews well before expiry). A pair that fails to load or does not match (caught between the two files being
/// replaced) is not taken: the old one stays until the next look.
#[cfg(feature = "central")]
#[derive(Debug)]
pub struct ReloadingCert {
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
    provider: std::sync::Arc<rustls::crypto::CryptoProvider>,
    every: Duration,
    /// (last look, the files' stamps when the key in use was loaded, the key in use)
    state: std::sync::Mutex<(std::time::Instant, Stamps, std::sync::Arc<rustls::sign::CertifiedKey>)>,
}

#[cfg(feature = "central")]
type Stamps = Option<[(std::time::SystemTime, u64); 2]>;

#[cfg(feature = "central")]
impl ReloadingCert {
    pub fn new(cert: &std::path::Path, key: &std::path::Path, provider: std::sync::Arc<rustls::crypto::CryptoProvider>, every: Duration) -> io::Result<ReloadingCert> {
        let mut r = ReloadingCert { cert: cert.into(), key: key.into(), provider, every, state: std::sync::Mutex::new((std::time::Instant::now(), None, std::sync::Arc::new(rustls::sign::CertifiedKey::new(Vec::new(), std::sync::Arc::new(NoKey))))) };
        let first = (r.stamps(), r.load()?);
        let s = r.state.get_mut().unwrap_or_else(|e| e.into_inner());
        (s.1, s.2) = first;
        Ok(r)
    }

    fn stamps(&self) -> Stamps {
        let st = |p: &std::path::Path| std::fs::metadata(p).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        Some([st(&self.cert)?, st(&self.key)?])
    }

    fn load(&self) -> io::Result<std::sync::Arc<rustls::sign::CertifiedKey>> {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let bad = |what: &str, p: &std::path::Path, e: String| io::Error::new(io::ErrorKind::InvalidData, format!("{what} {}: {e}", p.display()));
        let certs = CertificateDer::pem_file_iter(&self.cert).and_then(|i| i.collect::<Result<Vec<_>, _>>()).map_err(|e| bad("certificate", &self.cert, e.to_string()))?;
        let key = PrivateKeyDer::from_pem_file(&self.key).map_err(|e| bad("key", &self.key, e.to_string()))?;
        let signer = self.provider.key_provider.load_private_key(key).map_err(|e| bad("key", &self.key, e.to_string()))?;
        let ck = rustls::sign::CertifiedKey::new(certs, signer);
        ck.keys_match().map_err(|e| bad("certificate and key", &self.cert, e.to_string()))?;
        Ok(std::sync::Arc::new(ck))
    }

    /// The key to serve: reloaded first when it is time to look and the files changed.
    pub fn current(&self) -> std::sync::Arc<rustls::sign::CertifiedKey> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.0.elapsed() >= self.every {
            s.0 = std::time::Instant::now();
            let now = self.stamps();
            if now.is_some() && now != s.1 {
                match self.load() {
                    Ok(k) => {
                        (s.1, s.2) = (now, k);
                        eprintln!("redex-world: TLS certificate reloaded from {}", self.cert.display());
                    }
                    Err(e) => eprintln!("redex-world: TLS certificate not reloaded, keeping the old one: {e}"),
                }
            }
        }
        s.2.clone()
    }
}

#[cfg(feature = "central")]
impl rustls::server::ResolvesServerCert for ReloadingCert {
    fn resolve(&self, _: rustls::server::ClientHello<'_>) -> Option<std::sync::Arc<rustls::sign::CertifiedKey>> {
        Some(self.current())
    }
}

/// The placeholder key before the first load (never served: `new` fails when the files do not load).
#[cfg(feature = "central")]
#[derive(Debug)]
struct NoKey;

#[cfg(feature = "central")]
impl rustls::sign::SigningKey for NoKey {
    fn choose_scheme(&self, _: &[rustls::SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        None
    }
    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        rustls::SignatureAlgorithm::ECDSA
    }
}

impl TcpTransport {
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let l = TcpListener::bind(addr)?;
        l.set_nonblocking(true)?;
        Ok(Self {
            l,
            #[cfg(any(test, feature = "sim"))]
            sim: None,
            #[cfg(feature = "central")]
            tls: None,
        })
    }

    fn link<S: Stream>(&self, s: S, a: std::net::SocketAddr) -> TcpLink<S> {
        TcpLink {
            s,
            peer: peer_key(a.ip()),
            dec: Decoder::default(),
            ready: false,
            #[cfg(any(test, feature = "sim"))]
            sim: self.sim.map(|s| (s, s.seed | 1)),
        }
    }

    /// Every connection is TLS (the handshake runs on the connection's own thread, inside the `Hello` wait).
    #[cfg(feature = "central")]
    pub fn with_tls(mut self, cfg: std::sync::Arc<rustls::ServerConfig>) -> Self {
        self.tls = Some(cfg);
        self
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.l.local_addr()
    }

    #[cfg(any(test, feature = "sim"))]
    pub fn with_sim(mut self, sim: Sim) -> Self {
        self.sim = Some(sim);
        self
    }
}

impl Transport for TcpTransport {
    fn accept(&self, wait: Duration) -> io::Result<Option<Box<dyn Link>>> {
        match self.l.accept() {
            Ok((s, a)) => {
                s.set_nonblocking(false)?;
                s.set_nodelay(true)?;
                s.set_write_timeout(Some(Duration::from_secs(5)))?; // a peer that stops reading is dropped
                // no read waits forever either, whatever reads before the connection's own timeouts are set
                s.set_read_timeout(Some(HANDSHAKE))?;
                #[cfg(feature = "central")]
                if let Some(cfg) = &self.tls {
                    let conn = rustls::ServerConnection::new(cfg.clone()).map_err(io::Error::other)?;
                    return Ok(Some(Box::new(self.link(rustls::StreamOwned::new(conn, s), a))));
                }
                Ok(Some(Box::new(self.link(s, a))))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(wait.min(Duration::from_millis(20)));
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

/// A TCP connection's byte stream: the socket itself, or TLS over it.
pub trait Stream: Read + Write + Send {
    /// Writes go straight to the socket (no handshake to finish first): `Link::refusable`.
    const PLAIN: bool = true;
    fn tcp(&self) -> &TcpStream;
    /// Before the first frame: finishes a TLS handshake (within `HANDSHAKE` in all).
    fn ready(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stream for TcpStream {
    fn tcp(&self) -> &TcpStream {
        self
    }
}

#[cfg(feature = "central")]
impl Stream for rustls::StreamOwned<rustls::ServerConnection, TcpStream> {
    const PLAIN: bool = false;

    fn tcp(&self) -> &TcpStream {
        &self.sock
    }

    /// One socket read at a time, each allowed only what is left of `HANDSHAKE`: a peer feeding a byte now and then
    /// cannot stretch it (rustls' own `complete_io` would read on as long as bytes come).
    fn ready(&mut self) -> io::Result<()> {
        let deadline = std::time::Instant::now() + HANDSHAKE;
        while self.conn.is_handshaking() {
            while self.conn.wants_write() {
                self.conn.write_tls(&mut self.sock)?;
            }
            if !self.conn.is_handshaking() {
                break;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "TLS handshake took too long"));
            }
            self.sock.set_read_timeout(Some(left))?;
            if self.conn.read_tls(&mut self.sock)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if let Err(e) = self.conn.process_new_packets() {
                let _ = self.conn.write_tls(&mut self.sock); // the alert, best effort
                return Err(io::Error::new(io::ErrorKind::InvalidData, e));
            }
        }
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.sock)?;
        }
        Ok(())
    }
}

pub struct TcpLink<S: Stream = TcpStream> {
    s: S,
    peer: String,
    dec: Decoder,
    /// `Stream::ready` done
    ready: bool,
    #[cfg(any(test, feature = "sim"))]
    sim: Option<(Sim, u64)>,
}

fn bad(e: redex_proto::ProtoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

impl<S: Stream> Link for TcpLink<S> {
    fn recv(&mut self, wait: Duration) -> io::Result<Recv> {
        if !self.ready {
            self.s.ready()?;
            self.ready = true;
        }
        if let Some(m) = self.dec.next().map_err(bad)? {
            return Ok(Recv::Msg(m));
        }
        self.s.tcp().set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        let mut buf = [0u8; 512];
        match self.s.read(&mut buf) {
            Ok(0) => Ok(Recv::Closed),
            Ok(n) => {
                self.dec.push(&buf[..n]);
                Ok(self.dec.next().map_err(bad)?.map_or(Recv::Idle, Recv::Msg))
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(Recv::Idle),
            Err(e) => Err(e),
        }
    }

    fn send(&mut self, m: &Msg) -> io::Result<()> {
        #[cfg(any(test, feature = "sim"))]
        if let Some((sim, rng)) = &mut self.sim {
            // xorshift64: deterministic per seed
            let mut next = || {
                *rng ^= *rng << 13;
                *rng ^= *rng >> 7;
                *rng ^= *rng << 17;
                *rng
            };
            if matches!(m, Msg::PeerPos { .. }) && next() % 100 < sim.loss_pct {
                return Ok(());
            }
            let span = sim.max_ms.saturating_sub(sim.min_ms) + 1;
            std::thread::sleep(Duration::from_millis(sim.min_ms + next() % span));
        }
        self.s.write_all(&m.encode())?;
        self.s.flush() // TLS keeps what the socket did not take yet
    }

    fn peer(&self) -> String {
        self.peer.clone()
    }

    fn set_live(&mut self) {
        self.dec.set_limit(redex_proto::MAX_FRAME_LIVE);
    }

    fn refusable(&self) -> bool {
        // TLS only once its handshake finished: a refusal must never run (or wait on) the handshake
        S::PLAIN || self.ready
    }
}

#[cfg(feature = "steam")]
/// Only Steam's relays (SDR) carry a connection: with ICE (direct UDP) off, neither side learns the other's IP.
pub fn relay_only() -> [NetworkingConfigEntry; 1] {
    [NetworkingConfigEntry::new_int32(NetworkingConfigValue::P2PTransportICEEnable, 0)]
}

#[cfg(feature = "steam")]
/// Steam Networking Sockets P2P on the game server (virtual port 0): players connect to the server's SteamID through
/// Steam's relays (`steam:<steamid64>` on the client), so the server's address is never exposed. Each Steam message
/// carries whole frames (the same length-prefixed bytes as TCP).
pub struct SteamTransport {
    server: steamworks::Server,
    listen: ListenSocket,
}

#[cfg(feature = "steam")]
impl SteamTransport {
    /// `steam` must stay alive as long as this (its thread runs the callbacks that deliver connections).
    pub fn listen(steam: &crate::steam::Steam) -> io::Result<Self> {
        let server = steam.server().clone();
        let listen = server
            .networking_sockets()
            .create_listen_socket_p2p(0, relay_only())
            .map_err(|_| io::Error::other("Steam refused the P2P listen socket"))?;
        Ok(Self { server, listen })
    }
}

#[cfg(feature = "steam")]
impl Transport for SteamTransport {
    fn accept(&self, wait: Duration) -> io::Result<Option<Box<dyn Link>>> {
        let until = std::time::Instant::now() + wait;
        loop {
            while let Some(e) = self.listen.try_receive_event() {
                match e {
                    ListenSocketEvent::Connecting(r) => {
                        let _ = r.accept(); // limits and the handshake are the server's, as for TCP
                    }
                    ListenSocketEvent::Connected(c) => {
                        // the peer's SteamID, authenticated by Steam's P2P layer: the per-peer limit's key
                        let remote = c.remote().steam_id().map(|id| id.raw());
                        let peer = remote.map_or_else(|| "steam:?".into(), |id| format!("steam:{id}"));
                        let link = SteamLink { conn: c.take_connection(), server: self.server.clone(), peer, remote, dec: Decoder::default() };
                        return Ok(Some(Box::new(link)));
                    }
                    ListenSocketEvent::Disconnected(_) => {} // its link sees the closed state
                }
            }
            if std::time::Instant::now() >= until {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(feature = "steam")]
pub struct SteamLink {
    conn: NetConnection,
    server: steamworks::Server,
    peer: String,
    remote: Option<u64>,
    dec: Decoder,
}

#[cfg(feature = "steam")]
/// Still connected or getting there, as Steam has it.
pub fn steam_live(sockets: &steamworks::networking_sockets::NetworkingSockets, conn: &NetConnection) -> bool {
    let state = sockets.get_connection_info(conn).ok().and_then(|i| i.state().ok());
    matches!(state, Some(NetworkingConnectionState::Connecting | NetworkingConnectionState::FindingRoute | NetworkingConnectionState::Connected))
}

#[cfg(feature = "steam")]
impl Link for SteamLink {
    fn recv(&mut self, wait: Duration) -> io::Result<Recv> {
        let until = std::time::Instant::now() + wait;
        loop {
            if let Some(m) = self.dec.next().map_err(bad)? {
                return Ok(Recv::Msg(m));
            }
            let msgs = self.conn.receive_messages(16).map_err(|_| io::Error::from(io::ErrorKind::ConnectionReset))?;
            if msgs.is_empty() {
                if !steam_live(&self.server.networking_sockets(), &self.conn) {
                    return Ok(Recv::Closed);
                }
                if std::time::Instant::now() >= until {
                    return Ok(Recv::Idle);
                }
                std::thread::sleep(Duration::from_millis(5).min(wait));
            }
            for m in msgs {
                // Steam takes messages up to 512 KB: one bigger than any frame is refused before it is buffered
                if m.data().len() > 4 + redex_proto::MAX_FRAME_LIVE.max(redex_proto::MAX_FRAME_PRE_HELLO) {
                    return Err(bad(redex_proto::ProtoError::TooLarge(m.data().len())));
                }
                self.dec.push(m.data());
            }
        }
    }

    fn send(&mut self, m: &Msg) -> io::Result<()> {
        self.conn.send_message(&m.encode(), SendFlags::RELIABLE_NO_NAGLE).map(|_| ()).map_err(io::Error::other)
    }

    fn peer(&self) -> String {
        self.peer.clone()
    }

    fn set_live(&mut self) {
        self.dec.set_limit(redex_proto::MAX_FRAME_LIVE);
    }

    fn steam_id(&self) -> Option<u64> {
        // a connection Steam could not tie to an account gets 0, which no ticket's SteamID matches
        Some(self.remote.unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::peer_key;

    #[test]
    fn ipv6_peers_count_by_their_64() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert_eq!(peer_key(ip("2001:db8:1:2::1")), peer_key(ip("2001:db8:1:2:ffff:ffff:ffff:ffff")));
        assert_ne!(peer_key(ip("2001:db8:1:2::1")), peer_key(ip("2001:db8:1:3::1")));
        assert_eq!(peer_key(ip("203.0.113.7")), "203.0.113.7");
        assert_eq!(peer_key(ip("::ffff:203.0.113.7")), "203.0.113.7");
    }

    /// A renewed certificate is served once its files change, without a restart; a half-replaced pair (a key that
    /// does not match the certificate) is not taken.
    #[cfg(feature = "central")]
    #[test]
    fn the_certificate_is_reloaded_when_its_files_change() {
        use super::ReloadingCert;
        let dir = std::env::temp_dir().join(format!("redex-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        let write = |names: &[&str]| {
            let c = rcgen::generate_simple_self_signed(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
            std::fs::write(&cert, c.cert.pem()).unwrap();
            std::fs::write(&key, c.key_pair.serialize_pem()).unwrap();
            c
        };
        let first = write(&["a.example"]);
        let r = ReloadingCert::new(&cert, &key, std::sync::Arc::new(rustls::crypto::ring::default_provider()), std::time::Duration::ZERO).unwrap();
        assert_eq!(r.current().cert[0].as_ref(), first.cert.der().as_ref());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = write(&["a.example", "renewed.example"]);
        assert_eq!(r.current().cert[0].as_ref(), second.cert.der().as_ref(), "the renewed certificate");
        // only the key replaced so far: not a pair, the renewed one stays
        std::thread::sleep(std::time::Duration::from_millis(20));
        let other = rcgen::generate_simple_self_signed(vec!["x.example".to_string()]).unwrap();
        std::fs::write(&key, other.key_pair.serialize_pem()).unwrap();
        assert_eq!(r.current().cert[0].as_ref(), second.cert.der().as_ref(), "a mismatched pair is not taken");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
