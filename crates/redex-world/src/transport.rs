//! Transport seam: TCP (dev, LAN) and Steam Networking Sockets P2P over Steam's relays (`SteamTransport`).
use redex_proto::{Decoder, Msg};
use steamworks::networking_sockets::{ListenSocket, NetConnection};
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
}

pub trait Transport: Send + 'static {
    /// `None` when nothing connected within `wait`.
    fn accept(&self, wait: Duration) -> io::Result<Option<Box<dyn Link>>>;
}

impl Transport for Box<dyn Transport> {
    fn accept(&self, wait: Duration) -> io::Result<Option<Box<dyn Link>>> {
        (**self).accept(wait)
    }
}

/// Jitter and loss on the send side, so tests can check movement under 50-150 ms and drops (Review Focus 5).
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
}

impl TcpTransport {
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let l = TcpListener::bind(addr)?;
        l.set_nonblocking(true)?;
        Ok(Self { l, #[cfg(any(test, feature = "sim"))] sim: None })
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
                Ok(Some(Box::new(TcpLink {
                    s,
                    peer: a.ip().to_string(),
                    dec: Decoder::default(),
                    #[cfg(any(test, feature = "sim"))]
                    sim: self.sim.map(|s| (s, s.seed | 1)),
                })))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(wait.min(Duration::from_millis(20)));
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

pub struct TcpLink {
    s: TcpStream,
    peer: String,
    dec: Decoder,
    #[cfg(any(test, feature = "sim"))]
    sim: Option<(Sim, u64)>,
}

fn bad(e: redex_proto::ProtoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

impl Link for TcpLink {
    fn recv(&mut self, wait: Duration) -> io::Result<Recv> {
        if let Some(m) = self.dec.next().map_err(bad)? {
            return Ok(Recv::Msg(m));
        }
        self.s.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
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
        self.s.write_all(&m.encode())
    }

    fn peer(&self) -> String {
        self.peer.clone()
    }

    fn set_live(&mut self) {
        self.dec.set_limit(redex_proto::MAX_FRAME_LIVE);
    }
}

/// Only Steam's relays (SDR) carry a connection: with ICE (direct UDP) off, neither side learns the other's IP.
pub fn relay_only() -> [NetworkingConfigEntry; 1] {
    [NetworkingConfigEntry::new_int32(NetworkingConfigValue::P2PTransportICEEnable, 0)]
}

/// Steam Networking Sockets P2P on the game server (virtual port 0): players connect to the server's SteamID through
/// Steam's relays (`steam:<steamid64>` on the client), so the server's address is never exposed. Each Steam message
/// carries whole frames (the same length-prefixed bytes as TCP).
pub struct SteamTransport {
    server: steamworks::Server,
    listen: ListenSocket,
}

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

pub struct SteamLink {
    conn: NetConnection,
    server: steamworks::Server,
    peer: String,
    remote: Option<u64>,
    dec: Decoder,
}

/// Still connected or getting there, as Steam has it.
pub fn steam_live(sockets: &steamworks::networking_sockets::NetworkingSockets, conn: &NetConnection) -> bool {
    let state = sockets.get_connection_info(conn).ok().and_then(|i| i.state().ok());
    matches!(state, Some(NetworkingConnectionState::Connecting | NetworkingConnectionState::FindingRoute | NetworkingConnectionState::Connected))
}

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
