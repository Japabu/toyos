// Every unsafe block under `mm::` carries a `SAFETY:` comment — measured and
// documented in full by `issues/build/clippy-has-never-run-here.md`'s
// per-area plan. `host-tests.yml`'s kernel clippy step already runs with
// `-D warnings`, so `warn` here is what actually gates: a new undocumented
// block anywhere in this module tree fails CI, while the rest of the kernel
// (not yet swept) stays silent.
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod pmm;
pub mod paging;
mod alloc;
mod dma;
mod mmio;
mod region;
mod unmapped;

/// Only the ladder in `sched::driver` reads a live allocation's bands, and it
/// is behind the same feature — so exporting this unconditionally would be a
/// name nothing in a shipping kernel can call.
#[cfg(feature = "heap-tripwire")]
pub use alloc::check_live as check_heap_bands;
/// The sweep, behind its own feature for the same reason: it reads every live
/// band in the heap, which is the only thing that tells a band that *absorbed*
/// a stray write from one that merely *displaced* it.
#[cfg(feature = "heap-sweep")]
pub use alloc::sweep as sweep_heap_bands;
/// The sweep's lock hold without the sweep, behind its own feature: the arm
/// that asks whether what amplifies this class is the allocator's lock or the
/// delay either instrument spends on the pass path.
#[cfg(feature = "heap-lockspin")]
pub use alloc::hold_lock as hold_heap_lock;
/// Unconditional, because `hw::report_contexts` runs on every kernel crash and
/// a kernel that carries no sweep answers `None` rather than failing to build.
pub use alloc::sweep_stats;
pub use dma::{Dma, DmaPool, Unaligned};
pub use mmio::Mmio;
pub use region::{Allocation, KernelSlice};
pub use unmapped::Unmapped;

use crate::MemoryMapEntry;
pub use pmm::Region;

/// All physical memory is mapped at this virtual offset.
pub const PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// The kernel's one user page size, and the granularity a translation answers
/// at. It lives in `toyos-userbound` with every refusal that turns on it; the
/// rest of the kernel names it `mm::PAGE_2M`.
pub use toyos_userbound::PAGE_2M;

/// The hardware page size — every block-device transfer is a whole multiple
/// of this, and it is what `paging::PAGE_SIZE_BIT` marks a PDE as mapping
/// directly instead of through a PT. `mm` did not export a 4 KiB constant
/// before this; a caller that reached for one and found nothing here is why
/// more than one private copy exists elsewhere in the kernel.
pub const PAGE_SIZE: u64 = 4096;

/// Round `size` up to the next 2MB boundary.
///
/// Only for a size the kernel computed. Use [`align_2m_checked`] for one that
/// came from outside it.
pub const fn align_2m(size: usize) -> usize {
    (size + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1)
}

/// [`align_2m`] for a size that crossed a trust boundary — an ELF field, a
/// syscall argument, an extent firmware reported.
///
/// The round-up wraps, and a wrapped size is the worst possible failure: an
/// allocation far *smaller* than the caller asked for, with every later offset
/// still computed from the request. `None` says the size cannot be expressed,
/// which is the honest answer and not the same as an allocation failure.
pub const fn align_2m_checked(size: usize) -> Option<usize> {
    match size.checked_add(PAGE_2M as usize - 1) {
        Some(sum) => Some(sum & !(PAGE_2M as usize - 1)),
        None => None,
    }
}

/// The largest single allocation the kernel heap can serve.
///
/// `KernelPageSource` hands out one 2 MiB page and can hand out no more.
/// dlmalloc rounds a request up to a whole granule *plus* its own chunk and
/// segment bookkeeping, so a request that merely fits in 2 MiB still asks the
/// page source for more than one page — which is why the ceiling is not
/// `PAGE_2M` itself. Measured: a 2,097,152-byte request asks for 2,162,688.
///
/// The 4 KiB of headroom is policy, in the same sense as
/// `user_ptr::MAX_USER_STR`: the number is chosen, the reason it exists is
/// not. It is enough for dlmalloc's own bookkeeping, which is tens of bytes —
/// a request of exactly this size is served — and it is *not* enough to
/// absorb an alignment: `memalign` pads by the alignment before asking for
/// backing, so this size with a 4096-byte alignment asks the page source for
/// 2,162,688 as well and is refused. Both figures are off the guest.
///
/// Anything sized from outside the kernel must be refused above this rather
/// than reaching the allocator. `KernelAllocator::alloc` asserts it for every
/// heap allocation, before it takes dlmalloc's lock, and `OwnedAlloc::new`
/// refuses its own; a bare `Vec::with_capacity` sized from untrusted input
/// still has to check, because the assert is a fail-fast for kernel bugs and
/// not an error return.
pub const MAX_HEAP_ALLOC: usize = PAGE_2M as usize - 4096;

/// Whether an address is in the kernel's high-half direct map.
pub fn is_kernel_addr(addr: u64) -> bool {
    addr >= PHYS_OFFSET
}

/// User-space virtual address. Not directly dereferenceable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct UserAddr(u64);

impl UserAddr {
    /// For an address the kernel computed — a region start, a stack top, a
    /// mapping this address space owns. It asserts nothing about `v`.
    pub const fn new(v: u64) -> Self { Self(v) }

    /// For an address that crossed the syscall boundary.
    ///
    /// The type's name is a claim, and this is the only constructor that makes
    /// it true of a number userland chose.
    pub fn checked(v: u64) -> Option<Self> {
        toyos_userbound::is_user_addr(v).then_some(Self(v))
    }

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
    pub fn phys(self) -> u64 { self.0 }

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
