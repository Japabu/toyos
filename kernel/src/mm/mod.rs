pub mod pmm;
pub mod paging;
mod alloc;
mod mmio;
mod region;

pub use mmio::Mmio;
pub use region::KernelSlice;

use crate::MemoryMapEntry;
pub use pmm::Region;

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

/// Physical address for DMA device access.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct DmaAddr(u64);

impl DmaAddr {
    pub const fn raw(self) -> u64 { self.0 }

    /// Convert a kernel direct-map pointer to a DMA address.
    pub fn from_ptr<T>(ptr: *const T) -> Self {
        Self(DirectMap::phys_of(ptr))
    }
}

impl core::fmt::Debug for DmaAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DmaAddr({:#x})", self.0)
    }
}

/// Converts between physical addresses and kernel virtual pointers.
/// Use at the boundary between physical and virtual — not for storing pointers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DirectMap(u64);

impl DirectMap {
    pub fn from_phys(phys: u64) -> Self { Self(phys) }

    /// Wrap a kernel direct-map pointer as a DirectMap.
    pub fn from_ptr<T>(ptr: *const T) -> Self {
        Self(ptr as u64 - PHYS_OFFSET)
    }

    /// The raw physical address.
    pub fn raw(self) -> u64 { self.0 }

    pub fn as_ptr<T>(&self) -> *const T { (self.0 + PHYS_OFFSET) as *const T }
    pub fn as_mut_ptr<T>(&self) -> *mut T { (self.0 + PHYS_OFFSET) as *mut T }

    /// Convert a kernel direct-map pointer to its physical address.
    pub fn phys_of<T>(ptr: *const T) -> u64 {
        ptr as u64 - PHYS_OFFSET
    }
}

impl core::fmt::Display for DirectMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl core::fmt::Debug for DirectMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DirectMap({:#x})", self.0)
    }
}

/// Initialize the memory subsystem. Call once at boot.
/// Order: pmm (physical pages) → paging (direct map) → alloc (heap).
pub fn init(memory_map: &[MemoryMapEntry], reserved: &[Region]) {
    alloc::init_early();
    pmm::init(memory_map, reserved);
    paging::init(memory_map);
    alloc::init();
}
