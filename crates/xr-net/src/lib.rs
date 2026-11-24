//! RE:DEX multiplayer: what runs inside redex. `x` holds Pokemon X's game-specific layouts, `client` talks
//! to the world server, `id` keeps the player's stable identity, `steam` proves it is their Steam account, `account`
//! logs in to the account backend (central worlds).
pub mod account;
pub mod client;
pub mod friends;
pub mod id;
pub mod steam;
pub mod x;

pub use client::{Client, Local, Options, Remote};

/// Reads guest memory: the runtime implements it over its flat guest address space. False when any byte is unmapped.
pub trait Guest {
    fn read(&mut self, addr: u32, out: &mut [u8]) -> bool;

    fn u8(&mut self, addr: u32) -> Option<u8> {
        let mut b = [0; 1];
        self.read(addr, &mut b).then_some(b[0])
    }
    fn u16(&mut self, addr: u32) -> Option<u16> {
        let mut b = [0; 2];
        self.read(addr, &mut b).then(|| u16::from_le_bytes(b))
    }
    fn u32(&mut self, addr: u32) -> Option<u32> {
        let mut b = [0; 4];
        self.read(addr, &mut b).then(|| u32::from_le_bytes(b))
    }
    fn f32(&mut self, addr: u32) -> Option<f32> {
        self.u32(addr).map(f32::from_bits)
    }
}
