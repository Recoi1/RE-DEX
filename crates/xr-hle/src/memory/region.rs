//! xr-hle: the host memory guest addrs live in, guest addr a at host base + a
//! the caller (xr-runtime) owns the 4 GB reserve native code runs over, Memory only commits and decommits in it

use super::Permission;

/// a flat guest address space, guest addr a is host `base() + a` for every a below 4 GiB
pub trait GuestRegion: Send {
    /// host addr of guest addr 0
    fn base(&self) -> *mut u8;
    /// makes [addr, addr+size) (widened to pages) readable and writable, zero where never committed, false when it can't
    fn commit(&self, addr: u32, size: u32) -> bool;
    /// gives [addr, addr+size) back, it reads as zero once committed again
    fn decommit(&self, addr: u32, size: u32);
    /// TODO(phase 2): host page protection for GPU coherency (guarded pages fault into the runtime's handler)
    fn protect(&self, _addr: u32, _size: u32, _permission: Permission) {}
}

impl GuestRegion for xr_mem::GuestMem {
    fn base(&self) -> *mut u8 {
        self.base
    }

    fn commit(&self, addr: u32, size: u32) -> bool {
        self.map(addr, size).map_err(|error| log::error!("commit 0x{addr:08X}+0x{size:X}: {error}")).is_ok()
    }

    fn decommit(&self, addr: u32, size: u32) {
        if let Err(error) = xr_mem::GuestMem::decommit(self, addr, size) {
            log::error!("decommit 0x{addr:08X}+0x{size:X}: {error}");
        }
    }
}

/// a fresh 4 GB reserve of its own, for a System nobody hands a region (tests, tools)
pub fn reserve() -> Box<dyn GuestRegion> {
    Box::new(xr_mem::GuestMem::reserve().expect("reserve 4 GB of address space for guest memory"))
}
