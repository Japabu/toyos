use alloc::sync::Arc;

use crate::file_backing::FileBacking;
use crate::mm::PAGE_2M;

// ---------------------------------------------------------------------------
// Address space layout constants
// ---------------------------------------------------------------------------

/// Dynamic allocations grow top-down from this ceiling.
pub const ALLOC_CEILING: u64 = 0x0080_0000_0000; // 512 GB

/// Nothing allocated below this floor (guard against NULL-ish addresses).
pub const ALLOC_FLOOR: u64 = 0x0002_0000_0000; // 8 GB

/// Shared memory region base (16GB). Cross-process, managed by shared_memory.rs.
pub const SHM_BASE: u64 = 0x0004_0000_0000;

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
