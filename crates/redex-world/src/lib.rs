//! `redex-world`: relays player positions per map and channel. See NOTICE for the metaxy credit.
//! Later: trades, battles, market. Transports plug in behind `transport::Transport`.
//! Features: `steam` (default: Steam login and P2P relays, community worlds) and `central` (TCP + TLS and account world
//! tickets checked with the account backend; builds without Steam).
pub mod accounts;
pub mod lobby;
pub mod server;
#[cfg(feature = "steam")]
pub mod steam;
#[cfg(feature = "central")]
pub mod tickets;
pub mod transport;
pub mod world;

pub use server::{Config, Server};
