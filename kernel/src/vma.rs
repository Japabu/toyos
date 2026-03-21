use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::mm::PAGE_2M;
use crate::file_backing::FileBacking;
use crate::UserAddr;

// ---------------------------------------------------------------------------
// Address space layout constants
// ---------------------------------------------------------------------------

/// Dynamic allocations start here. Placed at 8GB — above any physical RAM
/// address that libraries might be mapped at (libraries still use virt=phys
/// until they get proper virtual address assignment).
pub const MMAP_BASE: u64 = 0x0002_0000_0000;

/// Shared memory region base (16GB). Cross-process, managed by shared_memory.rs.
#[allow(dead_code)]
pub const SHM_BASE: u64 = 0x0004_0000_0000;

/// Main thread stack guard page (unmapped).
#[allow(dead_code)]
pub const STACK_GUARD: u64 = 0x00FF_FF60_0000;

/// Main thread stack base. RSP starts at STACK_BASE + USER_STACK_SIZE.
pub const STACK_BASE: u64 = 0x00FF_FF80_0000;

/// 2MB guard page between allocations.
pub const GUARD_SIZE: u64 = PAGE_2M;

// ---------------------------------------------------------------------------
// VmaKind — what backs a virtual memory area
// ---------------------------------------------------------------------------

/// What a virtual memory area is backed by.
pub enum VmaKind {
    /// File-backed region. On fault: read page from backing store, copy into frame.
    FileBacked {
        /// The backing store that provides file pages (NVMe or initrd).
        backing: Arc<dyn FileBacking>,
        /// Byte offset within the file where this VMA starts.
        file_offset: u64,
        /// Number of valid file bytes in this VMA (from start). Bytes beyond
        /// this within the VMA should be zeroed (BSS portion of partial page).
        file_size: u64,
    },
    /// Anonymous memory (stack, BSS, heap). On fault: allocate zeroed page.
    Anonymous,
}

// ---------------------------------------------------------------------------
// Vma — a contiguous virtual memory region
// ---------------------------------------------------------------------------

/// A contiguous region of virtual address space with uniform permissions.
pub struct Vma {
    /// Start virtual address (4KB-aligned).
    pub start: UserAddr,
    /// End virtual address, exclusive (4KB-aligned).
    pub end: UserAddr,
    /// Whether userspace can write to this region.
    pub writable: bool,
    /// What backs this region.
    pub kind: VmaKind,
}

// ---------------------------------------------------------------------------
// VmaList — sorted VMAs + virtual address allocator
// ---------------------------------------------------------------------------

/// Sorted list of non-overlapping VMAs for a process, plus a bump allocator
/// for dynamic virtual address allocation (mmap, pipes, TLS, thread stacks).
pub struct VmaList {
    /// Demand-paged file-backed VMAs (ELF segments).
    vmas: Vec<Vma>,
    /// Bump pointer for dynamic allocations. Starts at MMAP_BASE.
    next_alloc: u64,
    /// Active allocations: vaddr → size. O(log n) lookup for free_region.
    regions: BTreeMap<UserAddr, u64>,
}

fn align_up_2m(v: u64) -> u64 {
    (v + PAGE_2M - 1) & !(PAGE_2M - 1)
}

/// Virtual address space exhausted — allocation would overlap shared memory region.
pub struct VmaError;

impl VmaList {
    pub fn new() -> Self {
        Self {
            vmas: Vec::new(),
            next_alloc: MMAP_BASE,
            regions: BTreeMap::new(),
        }
    }

    /// Allocate a 2MB-aligned virtual address range with a trailing guard page.
    /// Returns the start address. The guard page after the allocation is left unmapped.
    /// Returns `Err(VmaError)` if the allocation would exceed `SHM_BASE`.
    ///
    /// Known limitation: bump allocator does not reclaim freed ranges.
    /// Acceptable for short-lived processes. Future: free list.
    pub fn alloc_region(&mut self, size: u64) -> Result<UserAddr, VmaError> {
        let aligned = align_up_2m(size);
        let addr = align_up_2m(self.next_alloc);
        let new_next = addr + aligned + GUARD_SIZE;
        if new_next > SHM_BASE {
            return Err(VmaError);
        }
        self.next_alloc = new_next;
        let vaddr = UserAddr::new(addr);
        self.regions.insert(vaddr, aligned);
        Ok(vaddr)
    }

    /// Allocate a stack region with guard pages on BOTH sides.
    /// Guard below catches stack overflow (stack grows down).
    /// Guard above catches upward corruption.
    ///
    /// Layout: [guard 2MB][stack (grows ↓)][guard 2MB]
    #[allow(dead_code)]
    pub fn alloc_stack(&mut self, size: u64) -> Result<UserAddr, VmaError> {
        let aligned = align_up_2m(size);
        let guard_below = align_up_2m(self.next_alloc);
        let stack_start = guard_below + GUARD_SIZE;
        let new_next = stack_start + aligned + GUARD_SIZE;
        if new_next > SHM_BASE {
            return Err(VmaError);
        }
        self.next_alloc = new_next;
        let vaddr = UserAddr::new(stack_start);
        self.regions.insert(vaddr, aligned);
        Ok(vaddr)
    }

    /// Free a previously allocated region. Returns the size for unmapping.
    /// O(log n) via BTreeMap.
    pub fn free_region(&mut self, addr: UserAddr) -> Option<u64> {
        self.regions.remove(&addr)
    }

    /// Check if a virtual address falls within an allocated region.
    #[allow(dead_code)]
    pub fn contains(&self, addr: UserAddr) -> bool {
        // Check dynamic allocations
        if let Some((&base, &size)) = self.regions.range(..=addr).next_back() {
            if addr.raw() < base.raw() + size {
                return true;
            }
        }
        // Check demand-paged VMAs
        self.find(addr).is_some()
    }

    // --- Demand-paged VMA management (ELF segments) ---

    /// Find the VMA containing `addr`, if any.
    pub fn find(&self, addr: UserAddr) -> Option<&Vma> {
        let idx = self.vmas.partition_point(|v| v.start.raw() <= addr.raw());
        if idx == 0 { return None; }
        let vma = &self.vmas[idx - 1];
        if addr.raw() < vma.end.raw() { Some(vma) } else { None }
    }

    /// Insert a VMA, maintaining sorted order by start address.
    pub fn insert(&mut self, vma: Vma) {
        let idx = self.vmas.partition_point(|v| v.start.raw() < vma.start.raw());
        self.vmas.insert(idx, vma);
    }

    /// Iterate all VMAs that overlap the range [start, end).
    pub fn overlapping(&self, start: UserAddr, end: UserAddr) -> impl Iterator<Item = &Vma> {
        self.vmas.iter().filter(move |v| v.start.raw() < end.raw() && v.end.raw() > start.raw())
    }

    /// Remove all VMAs and dynamic allocations.
    pub fn clear(&mut self) {
        self.vmas.clear();
        self.regions.clear();
    }
}
