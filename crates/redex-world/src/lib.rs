//! `redex-world`: relays player positions per map and channel. See NOTICE for the metaxy credit.
//! Later: trades, battles, market. Transports plug in behind `transport::Transport`.
pub mod accounts;
pub mod server;
pub mod steam;
pub mod transport;
pub mod world;

pub use server::{Config, Server};
