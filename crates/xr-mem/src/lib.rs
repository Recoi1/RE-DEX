//! Guest address space: 4 GB reserved up front, committed on map. Guest address a is host `base + a`.
//! Shared by xr-runtime (native code, exception handler) and xr-hle (its `Memory` lives in it).
use std::io;
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE,
};

pub struct GuestMem {
    pub base: *mut u8,
}

// The base pointer is fixed for the life of the value; guest threads share the memory by design.
unsafe impl Send for GuestMem {}
unsafe impl Sync for GuestMem {}

/// [addr, addr+size) widened to whole 4 KiB pages, as (start, len)
fn pages(addr: u32, size: u32) -> (usize, usize) {
    let start = addr as u64 & !0xFFF;
    let end = (addr as u64 + size as u64 + 0xFFF) & !0xFFF;
    (start as usize, (end - start) as usize)
}

impl GuestMem {
    pub fn reserve() -> io::Result<Self> {
        let p = unsafe { VirtualAlloc(std::ptr::null(), 1 << 32, MEM_RESERVE, PAGE_NOACCESS) };
        if p.is_null() { Err(io::Error::last_os_error()) } else { Ok(Self { base: p as *mut u8 }) }
    }

    /// Commit zeroed read/write pages covering [addr, addr+size). Committing committed pages keeps their contents.
    pub fn map(&self, addr: u32, size: u32) -> io::Result<()> {
        let (start, len) = pages(addr, size);
        let p = unsafe { VirtualAlloc(self.base.add(start) as _, len, MEM_COMMIT, PAGE_READWRITE) };
        if p.is_null() { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Give the pages covering [addr, addr+size) back; they read as zero once committed again.
    pub fn decommit(&self, addr: u32, size: u32) -> io::Result<()> {
        let (start, len) = pages(addr, size);
        if len == 0 {
            return Ok(());
        }
        let ok = unsafe { VirtualFree(self.base.add(start) as _, len, MEM_DECOMMIT) };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    pub fn write(&self, addr: u32, bytes: &[u8]) {
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(addr as usize), bytes.len()) }
    }

    pub fn read_u32(&self, addr: u32) -> u32 {
        unsafe { (self.base.add(addr as usize) as *const u32).read_unaligned() }
    }

    pub fn read(&self, addr: u32, len: u32) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base.add(addr as usize), len as usize) }
    }
}

impl Drop for GuestMem {
    fn drop(&mut self) {
        unsafe { VirtualFree(self.base as _, 0, MEM_RELEASE) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_and_read() {
        let m = GuestMem::reserve().unwrap();
        m.map(0x0010_0000, 0x1000).unwrap();
        m.write(0x0010_0000, &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(m.read_u32(0x0010_0000), 0x1234_5678);
    }

    #[test]
    fn decommit_gives_zero_pages_back() {
        let m = GuestMem::reserve().unwrap();
        m.map(0x0010_0000, 0x2000).unwrap();
        m.write(0x0010_1000, &[1, 2, 3, 4]);
        m.decommit(0x0010_1000, 0x1000).unwrap();
        m.map(0x0010_1000, 0x1000).unwrap();
        assert_eq!(m.read_u32(0x0010_1000), 0);
    }
}
