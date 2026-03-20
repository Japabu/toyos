// Memory management subsystem.
//
// pmm      — free list of 2MB physical pages
// paging   — page tables, kernel direct map, per-process address spaces
// alloc    — dlmalloc GlobalAlloc backed by pmm pages

pub mod pmm;
pub mod paging;
mod alloc;

use crate::MemoryMapEntry;
pub use pmm::{PhysPage, Region};
// pub use paging::AddressSpace;  // TODO: enable once consumers migrate

// ---------------------------------------------------------------------------
// Address types
// ---------------------------------------------------------------------------

/// All physical memory is mapped at this virtual offset.
pub const PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// 2MB large page size — the only user page size in ToyOS.
pub const PAGE_2M: u64 = 2 * 1024 * 1024;

/// Round `size` up to the next 2MB boundary.
pub const fn align_2m(size: usize) -> usize {
    (size + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1)
}

/// Whether an address is in the kernel's high-half direct map.
pub fn is_kernel_addr(addr: u64) -> bool {
    addr >= PHYS_OFFSET
}

/// User-space virtual address. Not directly dereferenceable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct UserAddr(u64);

impl UserAddr {
    pub const fn new(v: u64) -> Self { Self(v) }
    pub const fn raw(self) -> u64 { self.0 }
}

impl core::ops::Add<u64> for UserAddr {
    type Output = Self;
    fn add(self, rhs: u64) -> Self { Self(self.0 + rhs) }
}

impl core::ops::Sub<u64> for UserAddr {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self { Self(self.0 - rhs) }
}

impl core::ops::Sub for UserAddr {
    type Output = u64;
    fn sub(self, rhs: Self) -> u64 { self.0 - rhs.0 }
}

impl core::fmt::Debug for UserAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "UserAddr({:#x})", self.0)
    }
}

impl core::fmt::Display for UserAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl core::fmt::LowerHex for UserAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::LowerHex::fmt(&self.0, f)
    }
}

/// Physical address for DMA device access. Only constructable from PhysPage.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct DmaAddr(u64);

impl DmaAddr {
    pub const fn raw(self) -> u64 { self.0 }
}

impl core::fmt::Debug for DmaAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DmaAddr({:#x})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/// Initialize the memory subsystem. Call once at boot.
/// Order: pmm (physical pages) → paging (direct map) → alloc (heap).
pub fn init(memory_map: &[MemoryMapEntry], reserved: &[Region]) {
    alloc::init_early(); // enable early bump allocator for paging::init
    pmm::init(memory_map, reserved);
    paging::init(memory_map);
    alloc::init(); // switch to dlmalloc
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn alloc_page() -> Option<PhysPage> { pmm::alloc_page() }
pub fn memory_stats() -> (u64, u64) { pmm::stats() }
pub fn kernel_cr3() -> u64 { paging::kernel_cr3() }
pub fn map_mmio(phys: u64, size: u64) { paging::map_mmio(phys, size) }
pub fn debug_page_walk(addr: u64) { paging::debug_page_walk(addr) }
