use alloc::sync::Arc;

use crate::file_backing::FileBacking;
use crate::mm::PAGE_2M;

// ---------------------------------------------------------------------------
// Address space layout constants
// ---------------------------------------------------------------------------

/// Dynamic allocations (mmap, shared memory) grow top-down from this ceiling.
/// The stack at STACK_BASE is tracked in the regions BTreeMap, so find_gap
/// avoids it. ALLOC_CEILING equals STACK_BASE because the stack extends upward
/// to the PIE base — no usable VA space exists above it.
pub const ALLOC_CEILING: u64 = STACK_BASE;

/// Nothing allocated below this floor (guard against NULL-ish addresses).
pub const ALLOC_FLOOR: u64 = 0x0002_0000_0000; // 8 GB

/// Main thread stack base. RSP starts at STACK_BASE + USER_STACK_SIZE.
pub const STACK_BASE: u64 = 0x00FF_FF80_0000;

/// 2MB guard page between allocations.
pub const GUARD_SIZE: u64 = PAGE_2M;

// ---------------------------------------------------------------------------
// Region — what a virtual memory area is backed by
// ---------------------------------------------------------------------------

/// What backs a virtual memory region.
pub enum RegionKind {
    /// File-backed region. On fault: read page from backing store.
    FileBacked {
        backing: Arc<dyn FileBacking>,
        file_offset: u64,
        file_size: u64,
    },
    /// Anonymous memory (stack, BSS, heap). On fault: allocate zeroed page.
    Anonymous,
    /// Eagerly mapped (mmap with physical backing already assigned).
    Mapped,
}

/// A contiguous region of virtual address space.
pub struct Region {
    /// Size in bytes (2MB-aligned for allocated regions, 4KB-aligned for VMAs).
    pub size: u64,
    /// Whether userspace can write to this region.
    pub writable: bool,
    /// What backs this region.
    pub kind: RegionKind,
}
