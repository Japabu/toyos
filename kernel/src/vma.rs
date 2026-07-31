use alloc::sync::Arc;

use crate::file_backing::FileBacking;
use crate::mm::PAGE_2M;

/// Dynamic allocations (mmap, shared memory) grow top-down from this ceiling.
/// The stack at STACK_BASE is tracked in the regions BTreeMap, so find_gap
/// avoids it. ALLOC_CEILING equals STACK_BASE because the stack extends upward
/// to the PIE base — no usable VA space exists above it.
pub const ALLOC_CEILING: u64 = STACK_BASE;

/// Nothing allocated below this floor (guard against NULL-ish addresses).
#[cfg(not(feature = "test-tiny-va"))]
pub const ALLOC_FLOOR: u64 = 0x0002_0000_0000; // 8 GB

/// `test-tiny-va` raises the floor to leave a 256 MiB arena.
///
/// Why an actuator rather than a program. The shipped arena is
/// `ALLOC_CEILING - ALLOC_FLOOR`, about 1015 GB. Every region in it costs
/// `align_up_2m(size) + GUARD_SIZE` of address space against at least
/// `align_up_2m(size)` of *physical* memory — mmap, shared memory, io_uring
/// and TLS all allocate through the PMM, and `dlopen`'s shared-image arm still
/// allocates its own writable window. The worst ratio is therefore 2:1, at a
/// 4 KiB request: 4 MiB of address space for 2 MiB of RAM. Exhausting 1015 GB
/// of address space needs upwards of 500 GB of RAM, 126 times what the harness
/// gives a guest — so the PMM refuses first, down a path that already returns
/// an error, and `find_gap` never fails. No workload this harness can express
/// reaches it.
///
/// 256 MiB is ~64 mappings, so a test exhausts it in a fraction of a second,
/// and it is wide enough that every process in the boot still maps its TLS and
/// its heap (measured: the boot completes and the guest reaches its ready
/// marker). The code under test is the shipped code; only this number moves.
#[cfg(feature = "test-tiny-va")]
pub const ALLOC_FLOOR: u64 = ALLOC_CEILING - 256 * 1024 * 1024;

/// Main thread stack base. RSP starts at STACK_BASE + USER_STACK_SIZE.
pub const STACK_BASE: u64 = 0x00FF_FF80_0000;

/// 2MB guard page between allocations.
pub const GUARD_SIZE: u64 = PAGE_2M;

// Region — what a virtual memory area is backed by

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
