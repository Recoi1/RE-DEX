//! virtual memory and the [Bus] the cpu talks to
//!
//! xr-hle: Zakuro's Memory kept physical buffers plus software page tables. Here guest memory is flat:
//! guest addr a is host `region.base() + a` (the 4 GB reserve native code runs over), map commits pages there.
//! Physical memory has no host pages of its own any more, a physical addr reaches its bytes thru the mappings showing it
//! (see host_slice_mut). PhysicalMemory stays the allocator, and its buffers keep the bytes of pages no mapping shows.
//! The OS gives no aliasing, so two mappings of the same physical page are two copies, synced at map and unmap.
//! The source stays mapped RW (as in Zakuro), so a write thru one copy isn't seen thru the other till an unmap;
//! map logs a warning when it makes such a copy.

pub mod config;
pub mod physical;
pub mod region;

use std::collections::{BTreeMap, HashMap};

use zakuro_common::memory_map::*;
use zakuro_common::VAddr;
use zakuro_cpu::Bus;

pub use physical::{MemoryRegion, PhysicalBlock, PhysicalMemory};
pub use region::{reserve, GuestRegion};

bitflags::bitflags! {
    /// page permissions as svcQueryMemory reports them
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct Permission: u32 {
        const READ = 1;
        const WRITE = 2;
        const EXECUTE = 4;
    }
}

impl Permission {
    pub const RW: Permission = Permission::READ.union(Permission::WRITE);
    pub const RX: Permission = Permission::READ.union(Permission::EXECUTE);
}

/// MemoryState as the kernel reports it thru svcQueryMemory
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(u32)]
pub enum MemoryState {
    Free = 0,
    Reserved = 1,
    Io = 2,
    Static = 3,
    Code = 4,
    Private = 5,
    Shared = 6,
    Continuous = 7,
    Aliased = 8,
    Alias = 9,
    AliasCode = 10,
    Locked = 11,
}

/// one contiguous virtual mapping
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Mapping {
    pub base: VAddr,
    pub size: u32,
    pub paddr: u32,
    pub permission: Permission,
    pub state: MemoryState,
}

/// what svcQueryMemory hands back
#[derive(Debug, Clone, Copy)]
pub struct MemoryInfo {
    pub base: VAddr,
    pub size: u32,
    pub permission: u32,
    pub state: u32,
}

pub struct Memory {
    /// the physical allocator, and the home of the bytes of physical pages no mapping shows right now
    pub phys: PhysicalMemory,

    /// the flat guest address space
    region: Box<dyn GuestRegion>,
    base: *mut u8,
    /// per 4 KiB page, Permission bits plus MAPPED, 0 when unmapped
    pages: Vec<u8>,
    /// ranges the host GPU drew that guest memory doesn't have yet, start to end, apart and in order
    /// written back right after the command list or transfer that drew them (System::flush_gpu_guards), TODO(phase 2): at the cpu access, thru page protection
    guards: BTreeMap<VAddr, VAddr>,

    /// mappings by base addr, for svcQueryMemory and unmapping
    mappings: BTreeMap<VAddr, Mapping>,

    /// accesses to unmapped addrs, deduped so one bad loop doesn't make a gigabyte of log
    faults: BTreeMap<VAddr, u32>,

    /// while a loop's watched for changing nothing, what each byte the cpu wrote held b4 its first write, see crate::idle
    write_log: Option<Box<WriteLog>>,
    /// (vaddr, paddr) of mappings already warned about being a second copy of physical memory
    aliases_warned: std::collections::HashSet<(VAddr, u32)>,
}

/// a page has a mapping, whatever its permissions
const MAPPED: u8 = 0x80;
const READ: u8 = Permission::READ.bits() as u8;
const WRITE: u8 = Permission::WRITE.bits() as u8;

/// what the cpu's writes wrote over, byte by byte
#[derive(Default)]
struct WriteLog {
    old: HashMap<VAddr, u8>,
    /// it wrote more than we keep, too much for a polling loop
    overflowed: bool,
}

/// max bytes a write log keeps
const MOST_LOGGED: usize = 1024;

// safe bc base points into the region the struct owns, which stays put till it drops
// whoever runs guest code (the executor's baton) is the only one touching it
unsafe impl Send for Memory {}

/// what a saved state keeps of the memory map, the memory's bytes aside
#[derive(serde::Serialize, serde::Deserialize)]
pub struct MapState {
    mappings: Vec<Mapping>,
    allocated: Vec<bool>,
    regions: [(u32, u32); 3],
}

impl Memory {
    /// the memory map as a saved state keeps it
    pub fn save_map(&self) -> MapState {
        let (allocated, regions) = self.phys.allocation();
        MapState { mappings: self.mappings.values().copied().collect(), allocated, regions }
    }

    /// the memory map as a saved state had it, the physical buffers already holding the state's bytes
    pub fn load_map(&mut self, state: MapState) -> Result<(), String> {
        if !self.phys.set_allocation(state.allocated, state.regions) {
            return Err("the console's memory is not the size the state has".into());
        }
        // drop every mapping w/o copying its bytes back, physical memory holds the state's now
        for mapping in std::mem::take(&mut self.mappings).into_values() {
            self.region.decommit(mapping.base, mapping.size);
        }
        self.pages.fill(0);
        self.guards.clear();
        self.faults.clear();
        for mapping in state.mappings {
            if mapping.base as u64 + mapping.size as u64 > 1 << 32 {
                return Err("a mapping of the state is past the memory map".into());
            }
            self.map(mapping.base, mapping.paddr, mapping.size, mapping.permission, mapping.state);
        }
        Ok(())
    }

    /// guest memory in a 4 GB reserve of its own
    pub fn new(new3ds: bool, app_bytes: u32) -> Memory {
        Memory::with_region(new3ds, app_bytes, reserve())
    }

    /// guest memory in region, the caller's flat address space
    pub fn with_region(new3ds: bool, app_bytes: u32, region: Box<dyn GuestRegion>) -> Memory {
        Memory {
            phys: PhysicalMemory::new(new3ds, app_bytes),
            base: region.base(),
            region,
            pages: vec![0; PAGE_TABLE_ENTRIES],
            guards: BTreeMap::new(),
            mappings: BTreeMap::new(),
            faults: BTreeMap::new(),
            write_log: None,
            aliases_warned: Default::default(),
        }
    }

    /// host addr of guest addr 0, guest addr a is base + a
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// starts noting what the cpu's writes overwrite, forgetting what was noted b4
    pub(crate) fn log_writes(&mut self) {
        self.write_log = Some(Box::default());
    }

    pub(crate) fn stop_logging_writes(&mut self) {
        self.write_log = None;
    }

    /// does every byte the cpu wrote since log_writes hold its old value again, except those in skip
    pub(crate) fn writes_undone(&mut self, skip: std::ops::Range<VAddr>) -> bool {
        let Some(log) = self.write_log.take() else { return false };
        let undone = !log.overflowed
            && log.old.iter().all(|(&addr, &old)| {
                let mut now = [0u8];
                skip.contains(&addr) || {
                    self.read_bytes(addr, &mut now);
                    now[0] == old
                }
            });
        self.write_log = Some(log);
        undone
    }

    /// notes what a cpu write is about to overwrite
    #[cold]
    fn note_write(&mut self, addr: VAddr, len: usize) {
        let mut old = [0u8; 4];
        self.read_bytes(addr, &mut old[..len]);
        let Some(log) = &mut self.write_log else { return };
        for (offset, byte) in old[..len].iter().enumerate() {
            if log.old.len() >= MOST_LOGGED {
                log.overflowed = true;
                return;
            }
            log.old.entry(addr.wrapping_add(offset as u32)).or_insert(*byte);
        }
    }

    /// the host bytes of the guest page vaddr falls in, whatever is mapped there
    #[inline(always)]
    fn host_page(&self, vaddr: VAddr) -> *mut u8 {
        self.base.wrapping_add((vaddr & !PAGE_MASK) as usize)
    }

    /// does the page vaddr falls in have all of bits
    #[inline(always)]
    fn has(&self, vaddr: VAddr, bits: u8) -> bool {
        self.pages[(vaddr >> PAGE_BITS) as usize] & bits == bits
    }

    /// maps size bytes of physical memory at vaddr
    /// the pages get the bytes the physical memory holds, from a live mapping of it or from its buffer
    pub fn map(
        &mut self,
        vaddr: VAddr,
        paddr: u32,
        size: u32,
        permission: Permission,
        state: MemoryState,
    ) {
        // services hand out blocks sized to their contents not to a page, so round up instead of refusing
        let vaddr = vaddr & !PAGE_MASK;
        let size = (size + PAGE_MASK) & !PAGE_MASK;
        if size == 0 {
            return;
        }
        // whatever was mapped here goes first, its bytes back where they belong
        if (0..size / PAGE_SIZE).any(|page| self.has(vaddr + page * PAGE_SIZE, MAPPED)) {
            self.unmap(vaddr, size);
        }
        if !self.region.commit(vaddr, size) {
            log::error!("map: could not commit 0x{vaddr:08X}+0x{size:X}");
            return;
        }

        let mut aliased_from = None;
        for page in 0..size / PAGE_SIZE {
            let page_vaddr = vaddr + page * PAGE_SIZE;
            let page_paddr = paddr + page * PAGE_SIZE;
            let to = self.host_page(page_vaddr);
            if let Some(from) = aliases(&self.mappings, page_paddr).next() {
                aliased_from.get_or_insert(from);
                // safe bc both are committed pages of the region, apart since one's mapped and the other isn't yet
                unsafe { std::ptr::copy_nonoverlapping(self.host_page(from), to, PAGE_SIZE as usize) };
                // the buffer keeps what both copies started from, unmap merges their changes against it
                if let Some(home) = self.phys.host_slice_mut(page_paddr, PAGE_SIZE) {
                    // safe bc to is a committed page of the region
                    unsafe { std::ptr::copy_nonoverlapping(to, home.as_mut_ptr(), PAGE_SIZE as usize) };
                }
            } else if let Some(home) = self.phys.host_slice_mut(page_paddr, PAGE_SIZE) {
                // the page was committed just now or decommitted when unmapped, so it's zero already
                if home.iter().any(|&byte| byte != 0) {
                    // safe bc to is a committed page of the region
                    unsafe { std::ptr::copy_nonoverlapping(home.as_ptr(), to, PAGE_SIZE as usize) };
                }
            } else {
                log::error!("map: no physical memory at 0x{page_paddr:08X}");
            }
            self.pages[(page_vaddr >> PAGE_BITS) as usize] = permission.bits() as u8 | MAPPED;
        }
        if let Some(from) = aliased_from {
            if self.aliases_warned.insert((vaddr, paddr)) {
                log::warn!(
                    "map: 0x{vaddr:08X}+0x{size:X} ({state:?}) shows physical 0x{paddr:08X}, already mapped at 0x{from:08X}; \
                     the two are copies, writes thru one don't reach the other till an unmap"
                );
            }
        }

        self.mappings.insert(
            vaddr,
            Mapping {
                base: vaddr,
                size,
                paddr,
                permission,
                state,
            },
        );
    }

    /// maps the physical pages behind source at destination too
    /// xr-hle: a copy, kept in step w/ the source at map and unmap (see the module doc)
    pub fn mirror(&mut self, destination: VAddr, source: VAddr, size: u32) -> bool {
        let size = (size + PAGE_MASK) & !PAGE_MASK;
        let mut done = 0;
        while done < size {
            let from = source + done;
            let Some(mapping) = self.mapping_at(from).copied() else {
                log::warn!("mirror: source 0x{from:08X} is not mapped");
                return false;
            };
            let offset = from - mapping.base;
            let chunk = (mapping.size - offset).min(size - done);
            self.map(
                destination + done,
                mapping.paddr + offset,
                chunk,
                mapping.permission | Permission::RW,
                MemoryState::Alias,
            );
            done += chunk;
        }
        true
    }

    pub fn unmap(&mut self, vaddr: VAddr, size: u32) {
        // map rounds a size up to pages, so unmapping the same size has to too
        let size = size.saturating_add(PAGE_MASK) & !PAGE_MASK;
        let end = vaddr as u64 + size as u64;
        let inside = |other: VAddr| (vaddr as u64..end).contains(&(other as u64));
        // the mapped pages of the range by the physical page they show
        let mut shown: BTreeMap<u32, Vec<VAddr>> = BTreeMap::new();
        for page in 0..size / PAGE_SIZE {
            let page_vaddr = vaddr.wrapping_add(page * PAGE_SIZE);
            let index = (page_vaddr >> PAGE_BITS) as usize;
            if self.pages[index] & MAPPED == 0 {
                continue;
            }
            self.pages[index] = 0;
            if let Some(paddr) = self.translate(page_vaddr) {
                shown.entry(paddr).or_default().push(page_vaddr);
            }
        }
        // each physical page's bytes go where it still shows, mappings of it outside the range, else its buffer
        for (paddr, copies) in shown {
            let survivors: Vec<VAddr> = aliases(&self.mappings, paddr).filter(|&other| !inside(other)).collect();
            let mut bytes = vec![0u8; PAGE_SIZE as usize];
            let base = self.base;
            // safe bc a mapped page is committed
            let page = |at: VAddr| unsafe { std::slice::from_raw_parts(base.wrapping_add(at as usize), PAGE_SIZE as usize) };
            if let [only] = copies[..] {
                bytes.copy_from_slice(page(only));
            } else {
                // copies going at once: each one's changes since they split (the buffer has that, see map) all land
                let split = self.phys.host_slice_mut(paddr, PAGE_SIZE).map(|home| home.to_vec()).unwrap_or_else(|| bytes.clone());
                bytes.copy_from_slice(&split);
                for &copy in copies.iter().chain(&survivors) {
                    for ((out, &now), &was) in bytes.iter_mut().zip(page(copy)).zip(&split) {
                        if now != was {
                            *out = now;
                        }
                    }
                }
            }
            if survivors.is_empty() {
                if let Some(home) = self.phys.host_slice_mut(paddr, PAGE_SIZE) {
                    home.copy_from_slice(&bytes);
                }
            }
            for other in survivors {
                // safe bc other is a committed page of the region
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.host_page(other), PAGE_SIZE as usize) };
            }
        }
        self.region.decommit(vaddr, size);
        // a mapping the range only partly covers keeps the rest at the pages behind it, so freeing them later frees the right ones
        let touched: Vec<Mapping> = self
            .mappings
            .values()
            .filter(|m| (m.base as u64) < end && (vaddr as u64) < m.base as u64 + m.size as u64)
            .copied()
            .collect();
        for m in touched {
            self.mappings.remove(&m.base);
            if m.base < vaddr {
                self.mappings.insert(m.base, Mapping { size: vaddr - m.base, ..m });
            }
            let m_end = m.base as u64 + m.size as u64;
            if m_end > end {
                let base = end as u32;
                self.mappings.insert(
                    base,
                    Mapping { base, size: (m_end - end) as u32, paddr: m.paddr + (base - m.base), ..m },
                );
            }
        }
    }

    pub fn mapping_at(&self, vaddr: VAddr) -> Option<&Mapping> {
        self.mappings
            .range(..=vaddr)
            .next_back()
            .map(|(_, m)| m)
            .filter(|m| vaddr < m.base.wrapping_add(m.size))
    }

    /// the physical addr a mapped virtual one shows
    pub fn translate(&self, vaddr: VAddr) -> Option<u32> {
        self.mapping_at(vaddr).map(|m| m.paddr + (vaddr - m.base))
    }

    /// svcQueryMemory, describes the mapping holding vaddr or the free gap it sits in
    pub fn query(&self, vaddr: VAddr) -> MemoryInfo {
        if let Some(m) = self.mapping_at(vaddr) {
            return MemoryInfo {
                base: m.base,
                size: m.size,
                permission: m.permission.bits(),
                state: m.state as u32,
            };
        }

        // report the free gap between the mappings around it, what allocators walking the address space expect
        let start = self
            .mappings
            .range(..=vaddr)
            .next_back()
            .map(|(_, m)| m.base.wrapping_add(m.size))
            .unwrap_or(0);
        let end = self
            .mappings
            .range(vaddr..)
            .next()
            .map(|(&base, _)| base)
            .unwrap_or(0xFFFF_F000);

        MemoryInfo {
            base: start,
            size: end.wrapping_sub(start),
            permission: 0,
            state: MemoryState::Free as u32,
        }
    }

    pub fn mappings(&self) -> impl Iterator<Item = &Mapping> {
        self.mappings.values()
    }

    /// writes a word to any mapped page whatever its permissions say
    pub fn write32_privileged(&mut self, vaddr: VAddr, value: u32) -> bool {
        if !self.has(vaddr, MAPPED) || !self.has(vaddr.wrapping_add(3), MAPPED) {
            return false;
        }
        // safe bc both pages it touches are mapped, so committed
        unsafe { (self.base.wrapping_add(vaddr as usize) as *mut u32).write_unaligned(value.to_le()) };
        true
    }

    /// can instrs be fetched from vaddr
    pub fn is_executable(&self, vaddr: VAddr) -> bool {
        self.has(vaddr, READ)
    }

    /// is every page in the range mapped and readable
    pub fn is_readable(&self, vaddr: VAddr, size: u32) -> bool {
        (0..size.div_ceil(PAGE_SIZE)).all(|page| self.has(vaddr.wrapping_add(page * PAGE_SIZE), READ))
    }

    /// is every page in the range mapped and writable
    pub fn is_writable(&self, vaddr: VAddr, size: u32) -> bool {
        (0..size.div_ceil(PAGE_SIZE)).all(|page| self.has(vaddr.wrapping_add(page * PAGE_SIZE), WRITE))
    }

    // bulk access

    /// copies a block of guest memory out page by page so it can straddle mappings, unmapped pages read as zero
    pub fn read_bytes(&mut self, vaddr: VAddr, out: &mut [u8]) {
        let mut done = 0usize;
        while done < out.len() {
            let addr = vaddr.wrapping_add(done as u32);
            let offset = (addr & PAGE_MASK) as usize;
            let chunk = (PAGE_SIZE as usize - offset).min(out.len() - done);
            if self.has(addr, READ) {
                // safe bc the page is mapped, so committed, and the chunk stays in it
                unsafe { std::ptr::copy_nonoverlapping(self.host_page(addr).add(offset), out[done..].as_mut_ptr(), chunk) };
            } else {
                self.note_fault(addr);
                out[done..done + chunk].fill(0);
            }
            done += chunk;
        }
    }

    pub fn write_bytes(&mut self, vaddr: VAddr, data: &[u8]) {
        let mut done = 0usize;
        while done < data.len() {
            let addr = vaddr.wrapping_add(done as u32);
            let offset = (addr & PAGE_MASK) as usize;
            let chunk = (PAGE_SIZE as usize - offset).min(data.len() - done);
            if self.has(addr, WRITE) {
                // safe, same as in read_bytes
                unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), self.host_page(addr).add(offset), chunk) };
            } else {
                self.note_fault(addr);
            }
            done += chunk;
        }
    }

    /// the bytes at a guest addr in host memory, when every page of them is mapped
    pub fn slice_mut(&mut self, vaddr: VAddr, len: u32) -> Option<&mut [u8]> {
        if len == 0 || vaddr as u64 + len as u64 > 1 << 32 {
            return None;
        }
        let mut pages = (vaddr >> PAGE_BITS)..=((vaddr + (len - 1)) >> PAGE_BITS);
        if !pages.all(|page| self.pages[page as usize] & MAPPED != 0) {
            return None;
        }
        // safe bc every page of it is committed, &mut self keeps other borrows off
        Some(unsafe { std::slice::from_raw_parts_mut(self.base.wrapping_add(vaddr as usize), len as usize) })
    }

    /// the bytes at a physical addr where they live now: a mapping showing all of them in order, or the physical buffer
    /// when no mapping shows any, none when they're split between the two or nothing backs them
    pub fn host_slice_mut(&mut self, paddr: u32, size: u32) -> Option<&mut [u8]> {
        let first = paddr & !PAGE_MASK;
        let last = paddr.checked_add(size.max(1) - 1)? & !PAGE_MASK;
        let shown_at = aliases(&self.mappings, first).next();
        match shown_at {
            Some(vaddr) => {
                let in_order = (first..=last)
                    .step_by(PAGE_SIZE as usize)
                    .all(|page| self.translate(vaddr.wrapping_add(page - first)) == Some(page));
                if in_order { self.slice_mut(vaddr + (paddr - first), size) } else { None }
            }
            None => {
                let shown = (first..=last).step_by(PAGE_SIZE as usize).any(|page| aliases(&self.mappings, page).next().is_some());
                if shown { None } else { self.phys.host_slice_mut(paddr, size) }
            }
        }
    }

    /// the kernel's readonly config page, where it lives now
    pub fn config_mem_mut(&mut self) -> &mut [u8] {
        self.host_slice_mut(CONFIG_MEM_VADDR, CONFIG_MEM_SIZE).expect("the config page is in AXI WRAM")
    }

    /// the page the kernel keeps updating w/ time, battery and wifi state, where it lives now
    pub fn shared_page_mut(&mut self) -> &mut [u8] {
        self.host_slice_mut(SHARED_PAGE_VADDR, SHARED_PAGE_SIZE).expect("the shared page is in AXI WRAM")
    }

    /// writes straight to physical memory (every mapping of it), skipping permissions
    pub fn write_physical(&mut self, paddr: u32, data: &[u8]) -> bool {
        let mut done = 0usize;
        while done < data.len() {
            let at = paddr.wrapping_add(done as u32);
            let offset = (at & PAGE_MASK) as usize;
            let chunk = (PAGE_SIZE as usize - offset).min(data.len() - done);
            let shown: Vec<VAddr> = aliases(&self.mappings, at).collect();
            if shown.is_empty() {
                match self.phys.host_slice_mut(at, chunk as u32) {
                    Some(slice) => slice.copy_from_slice(&data[done..done + chunk]),
                    None => {
                        log::error!("write of {} bytes to unbacked physical address 0x{at:08X}", data.len());
                        return false;
                    }
                }
            }
            for vaddr in shown {
                // safe bc a mapped page is committed and the chunk stays in it
                unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), self.host_page(vaddr).add(offset), chunk) };
            }
            done += chunk;
        }
        true
    }

    /// reads straight from physical memory, zeroes where nothing backs it
    pub fn read_physical(&mut self, paddr: u32, out: &mut [u8]) -> bool {
        let mut done = 0usize;
        while done < out.len() {
            let at = paddr.wrapping_add(done as u32);
            let offset = (at & PAGE_MASK) as usize;
            let chunk = (PAGE_SIZE as usize - offset).min(out.len() - done);
            if let Some(vaddr) = aliases(&self.mappings, at).next() {
                // safe, same as in write_physical
                unsafe { std::ptr::copy_nonoverlapping(self.host_page(vaddr).add(offset), out[done..].as_mut_ptr(), chunk) };
            } else if let Some(slice) = self.phys.host_slice_mut(at, chunk as u32) {
                out[done..done + chunk].copy_from_slice(slice);
            } else {
                out[done..].fill(0);
                return false;
            }
            done += chunk;
        }
        true
    }

    /// zeroes a physical range, for BSS and fresh pages
    pub fn zero_physical(&mut self, paddr: u32, size: u32) -> bool {
        let zeroed = self.write_physical(paddr, &vec![0; size as usize]);
        if !zeroed {
            log::error!("zeroing unbacked physical address 0x{paddr:08X}");
        }
        zeroed
    }

    /// every mapped page's bytes into the physical buffers, for a saved state; a page several mappings show keeps the
    /// bytes its copies split from there (see map, unmap merges against them), each copy's own are `alias_copies`
    pub fn flush_to_physical(&mut self) {
        let mappings: Vec<Mapping> = self.mappings.values().copied().collect();
        for mapping in mappings {
            for page in 0..mapping.size / PAGE_SIZE {
                if aliases(&self.mappings, mapping.paddr + page * PAGE_SIZE).nth(1).is_some() {
                    continue;
                }
                let from = self.host_page(mapping.base + page * PAGE_SIZE);
                if let Some(home) = self.phys.host_slice_mut(mapping.paddr + page * PAGE_SIZE, PAGE_SIZE) {
                    // safe bc a mapped page is committed
                    unsafe { std::ptr::copy_nonoverlapping(from, home.as_mut_ptr(), PAGE_SIZE as usize) };
                }
            }
        }
    }

    /// each mapped page that is one of several copies of a physical page, w/ its bytes, for a saved state (physical
    /// memory holds one page's worth, see flush_to_physical)
    pub fn alias_copies(&self) -> Vec<(VAddr, Vec<u8>)> {
        let mut copies = Vec::new();
        for mapping in self.mappings.values() {
            for page in 0..mapping.size / PAGE_SIZE {
                if aliases(&self.mappings, mapping.paddr + page * PAGE_SIZE).nth(1).is_some() {
                    let at = mapping.base + page * PAGE_SIZE;
                    // safe bc a mapped page is committed
                    copies.push((at, unsafe { std::slice::from_raw_parts(self.host_page(at), PAGE_SIZE as usize) }.to_vec()));
                }
            }
        }
        copies
    }

    /// puts back the copies alias_copies had, once load_map mapped their pages again
    pub fn restore_alias_copies(&mut self, copies: &[(VAddr, Vec<u8>)]) -> Result<(), String> {
        for (at, bytes) in copies {
            if at & PAGE_MASK != 0 || bytes.len() != PAGE_SIZE as usize || !self.has(*at, MAPPED) {
                return Err(format!("a copy of a page in the state, at 0x{at:08X}, is not a mapped page"));
            }
            // safe bc a mapped page is committed
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.host_page(*at), PAGE_SIZE as usize) };
        }
        Ok(())
    }

    /// reads a NUL terminated ASCII string, capped so a corrupt ptr can't make us read forever
    pub fn read_cstring(&mut self, vaddr: VAddr, max: usize) -> String {
        let mut bytes = Vec::new();
        for i in 0..max {
            let b = self.read8(vaddr.wrapping_add(i as u32));
            if b == 0 {
                break;
            }
            bytes.push(b);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// the host GPU drew over a range guest memory doesn't have yet
    /// xr-hle phase 1: it comes down right after the command list or transfer (System::flush_gpu_guards), not at the cpu's next read
    pub fn guard_cpu_reads(&mut self, vaddr: VAddr, len: u32) {
        add_guard(&mut self.guards, vaddr, len);
    }

    /// same for writes, see guard_cpu_reads
    pub fn guard_cpu_writes(&mut self, vaddr: VAddr, len: u32) {
        add_guard(&mut self.guards, vaddr, len);
    }

    /// the guarded ranges, start to end, unguarded now
    pub fn take_guards(&mut self) -> Vec<(VAddr, VAddr)> {
        std::mem::take(&mut self.guards).into_iter().collect()
    }

    /// the word at addr when a cpu read there would just read it
    pub fn plain_read32(&self, addr: VAddr) -> Option<u32> {
        // safe bc both pages it touches are mapped readable, so committed
        (self.has(addr, READ) && self.has(addr.wrapping_add(3), READ))
            .then(|| u32::from_le(unsafe { (self.base.wrapping_add(addr as usize) as *const u32).read_unaligned() }))
    }

    /// a word of guest memory as anything but the cpu reads it
    pub fn peek32(&mut self, addr: VAddr) -> u32 {
        let mut buf = [0u8; 4];
        self.read_bytes(addr, &mut buf);
        u32::from_le_bytes(buf)
    }

    fn note_fault(&mut self, addr: VAddr) {
        let page = addr & !PAGE_MASK;
        let count = self.faults.entry(page).or_insert(0);
        *count += 1;
        if *count == 1 {
            log::warn!("access to unmapped guest address 0x{addr:08X}");
        }
    }

    /// unmapped pages touched so far, for the diagnostics overlay
    pub fn fault_summary(&self) -> Vec<(VAddr, u32)> {
        self.faults.iter().map(|(&a, &c)| (a, c)).collect()
    }

    /// the host addr of a cpu access of N bytes at addr, when it stays in one page that has bits
    #[inline(always)]
    fn fast<const N: usize>(&self, addr: VAddr, bits: u8) -> Option<*mut u8> {
        ((addr & PAGE_MASK) as usize <= PAGE_SIZE as usize - N && self.has(addr, bits)).then(|| self.base.wrapping_add(addr as usize))
    }
}

/// adds a range to the guarded ones (one range out of those it overlaps)
fn add_guard(guards: &mut BTreeMap<VAddr, VAddr>, vaddr: VAddr, len: u32) {
    let (mut start, mut end) = (vaddr, vaddr.saturating_add(len));
    if start >= end {
        return;
    }
    let joined: Vec<(VAddr, VAddr)> =
        guards.range(..end).rev().take_while(|(_, &to)| to > start).map(|(&from, &to)| (from, to)).collect();
    for (from, to) in joined {
        guards.remove(&from);
        (start, end) = (start.min(from), end.max(to));
    }
    guards.insert(start, end);
}

/// virtual addrs of the physical page paddr falls in, one per mapping of it
fn aliases(mappings: &BTreeMap<VAddr, Mapping>, paddr: u32) -> impl Iterator<Item = VAddr> + '_ {
    let paddr = paddr & !PAGE_MASK;
    mappings
        .values()
        .filter(move |mapping| paddr.wrapping_sub(mapping.paddr) < mapping.size)
        .map(move |mapping| mapping.base + (paddr - mapping.paddr))
}

// the cpu facing bus, straight to base + addr where the page allows it

impl Bus for Memory {
    #[inline(always)]
    fn read8(&mut self, addr: u32) -> u8 {
        match self.fast::<1>(addr, READ) {
            // safe bc fast found the page mapped readable, so committed
            Some(at) => unsafe { at.read() },
            None => {
                self.note_fault(addr);
                0
            }
        }
    }

    #[inline(always)]
    fn read16(&mut self, addr: u32) -> u16 {
        match self.fast::<2>(addr, READ) {
            // safe, same as read8
            Some(at) => u16::from_le(unsafe { (at as *const u16).read_unaligned() }),
            None => {
                let mut buf = [0u8; 2];
                self.read_bytes(addr, &mut buf);
                u16::from_le_bytes(buf)
            }
        }
    }

    #[inline(always)]
    fn read32(&mut self, addr: u32) -> u32 {
        match self.fast::<4>(addr, READ) {
            // safe, same as read8
            Some(at) => u32::from_le(unsafe { (at as *const u32).read_unaligned() }),
            None => {
                let mut buf = [0u8; 4];
                self.read_bytes(addr, &mut buf);
                u32::from_le_bytes(buf)
            }
        }
    }

    #[inline(always)]
    fn write8(&mut self, addr: u32, value: u8) {
        if self.write_log.is_some() {
            self.note_write(addr, 1);
        }
        match self.fast::<1>(addr, WRITE) {
            // safe bc fast found the page mapped writable, so committed
            Some(at) => unsafe { at.write(value) },
            None => self.note_fault(addr),
        }
    }

    #[inline(always)]
    fn write16(&mut self, addr: u32, value: u16) {
        if self.write_log.is_some() {
            self.note_write(addr, 2);
        }
        match self.fast::<2>(addr, WRITE) {
            // safe, same as write8
            Some(at) => unsafe { (at as *mut u16).write_unaligned(value.to_le()) },
            None => self.write_bytes(addr, &value.to_le_bytes()),
        }
    }

    #[inline(always)]
    fn write32(&mut self, addr: u32, value: u32) {
        if self.write_log.is_some() {
            self.note_write(addr, 4);
        }
        match self.fast::<4>(addr, WRITE) {
            // safe, same as write8
            Some(at) => unsafe { (at as *mut u32).write_unaligned(value.to_le()) },
            None => self.write_bytes(addr, &value.to_le_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_with_block(size: u32) -> (Memory, PhysicalBlock) {
        let mut memory = Memory::new(false, 0x0400_0000);
        let block = memory.phys.allocate(MemoryRegion::Application, size).unwrap();
        (memory, block)
    }

    /// guest addr a is host base + a, so native code and the bus see the same bytes
    #[test]
    fn guest_memory_is_flat() {
        let (mut memory, block) = memory_with_block(0x2000);
        memory.map(0x0010_0000, block.addr, 0x2000, Permission::RW, MemoryState::Private);
        memory.write32(0x0010_0FFE, 0x1234_5678);
        let host = unsafe { (memory.base().add(0x0010_0FFE) as *const u32).read_unaligned() };
        assert_eq!(host, 0x1234_5678);
        assert_eq!(memory.read32(0x0010_0FFE), 0x1234_5678);
        // unmapped reads are zero and counted, not a host fault
        assert_eq!(memory.read32(0x0020_0000), 0);
        assert_eq!(memory.fault_summary(), vec![(0x0020_0000, 1)]);
        // a second mapping of a page starts w/ its bytes, readonly it drops the cpu's writes
        memory.map(0x0030_0000, block.addr + 0x1000, 0x1000, Permission::READ, MemoryState::Code);
        assert_eq!(memory.read16(0x0030_0000), 0x1234);
        memory.write8(0x0030_0000, 9);
        assert_eq!(memory.read16(0x0030_0000), 0x1234);
    }

    /// physical ops reach the bytes thru the mapping showing them, or the buffer when none does, and map brings those in
    #[test]
    fn physical_memory_follows_its_mappings() {
        let (mut memory, block) = memory_with_block(0x3000);
        assert!(memory.write_physical(block.addr + 0x1FFE, b"home"));
        memory.map(0x0800_0000, block.addr, 0x3000, Permission::RW, MemoryState::Private);
        let mut bytes = [0u8; 4];
        memory.read_bytes(0x0800_1FFE, &mut bytes);
        assert_eq!(&bytes, b"home");
        memory.write_physical(block.addr + 4, b"live");
        memory.read_bytes(0x0800_0004, &mut bytes);
        assert_eq!(&bytes, b"live");
        memory.write_bytes(0x0800_2000, b"cpu!");
        memory.read_physical(block.addr + 0x2000, &mut bytes);
        assert_eq!(&bytes, b"cpu!");
        assert_eq!(&memory.host_slice_mut(block.addr + 0x2000, 4).unwrap()[..], b"cpu!");
        // unmapped, the bytes go back to the buffer, and a mapping elsewhere gets them
        memory.unmap(0x0800_0000, 0x3000);
        assert_eq!(memory.read32(0x0800_2000), 0);
        assert_eq!(&memory.host_slice_mut(block.addr + 0x2000, 4).unwrap()[..], b"cpu!");
        memory.map(0x0900_0000, block.addr, 0x3000, Permission::RW, MemoryState::Private);
        memory.read_bytes(0x0900_2000, &mut bytes);
        assert_eq!(&bytes, b"cpu!");
    }

    /// a mirror starts as a copy of its source, physical writes reach both, and unmapping it hands its bytes back
    #[test]
    fn a_mirror_copies_and_hands_back() {
        let (mut memory, block) = memory_with_block(0x2000);
        memory.map(0x0800_0000, block.addr, 0x2000, Permission::RW, MemoryState::Private);
        memory.write32(0x0800_1000, 7);
        assert!(memory.mirror(0x0010_0000, 0x0800_0000, 0x2000));
        assert_eq!(memory.read32(0x0010_1000), 7);
        memory.write_physical(block.addr, &[1]);
        assert_eq!((memory.read8(0x0800_0000), memory.read8(0x0010_0000)), (1, 1));
        memory.write32(0x0010_1000, 8);
        memory.unmap(0x0010_0000, 0x2000);
        assert_eq!(memory.read32(0x0800_1000), 8);
    }

    /// one unmap over both copies of a page keeps what was written thru either, in the buffer
    #[test]
    fn unmapping_both_copies_at_once_keeps_the_writes() {
        for (a, b) in [(0x0800_0000, 0x0800_3000), (0x0800_3000, 0x0800_0000)] {
            let (mut memory, block) = memory_with_block(0x1000);
            memory.write_physical(block.addr, &[5; 8]);
            memory.map(a, block.addr, 0x1000, Permission::RW, MemoryState::Private);
            memory.map(b, block.addr, 0x1000, Permission::RW, MemoryState::Aliased);
            memory.write32(a, 0x1111_1111);
            memory.write8(b + 7, 0x22);
            memory.unmap(0x0800_0000, 0x4000);
            assert!(memory.mapping_at(a).is_none() && memory.mapping_at(b).is_none());
            let mut bytes = [0u8; 8];
            memory.read_physical(block.addr, &mut bytes);
            assert_eq!(bytes, [0x11, 0x11, 0x11, 0x11, 5, 5, 5, 0x22], "a at 0x{a:08X}");
        }
    }

    /// unmap rounds a size up to pages like map does, a block sized to its contents goes whole
    #[test]
    fn unmap_rounds_its_size_up() {
        let (mut memory, block) = memory_with_block(0x1000);
        memory.map(0x1000_0000, block.addr, 0x2B0, Permission::RW, MemoryState::Shared);
        memory.write8(0x1000_0004, 3);
        memory.unmap(0x1000_0000, 0x2B0);
        assert!(memory.mapping_at(0x1000_0000).is_none());
        assert!(!memory.is_readable(0x1000_0000, 1));
        let mut byte = [0u8];
        memory.read_physical(block.addr + 4, &mut byte);
        assert_eq!(byte, [3]);
    }

    /// what the GPU drew is noted by range, joined where they overlap, and taken once
    #[test]
    fn guards_join_and_drain() {
        let (mut memory, _) = memory_with_block(0x1000);
        memory.guard_cpu_reads(0x1F00_1000, 0x800);
        memory.guard_cpu_writes(0x1F00_1400, 0x800);
        memory.guard_cpu_reads(0x1F00_4000, 0x10);
        assert_eq!(memory.take_guards(), vec![(0x1F00_1000, 0x1F00_1C00), (0x1F00_4000, 0x1F00_4010)]);
        assert!(memory.take_guards().is_empty());
    }
}
