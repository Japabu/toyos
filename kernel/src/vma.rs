use alloc::sync::Arc;
use alloc::vec::Vec;

/// What a virtual memory area is backed by.
pub enum VmaKind {
    /// File-backed region. On RO fault: map page cache page directly (zero copy).
    /// On RW fault: allocate private page, copy from page cache.
    FileBacked {
        /// Maps file block index to disk block number.
        block_map: Arc<Vec<u64>>,
        /// Byte offset within the file where this VMA starts.
        file_offset: u64,
        /// Number of valid file bytes in this VMA (from start). Bytes beyond
        /// this within the VMA should be zeroed (BSS portion of partial page).
        file_size: u64,
    },
    /// Anonymous memory (stack, BSS, heap). On fault: allocate zeroed page.
    Anonymous,
}

/// A contiguous region of virtual address space with uniform permissions.
pub struct Vma {
    /// Start virtual address (4KB-aligned).
    pub start: u64,
    /// End virtual address, exclusive (4KB-aligned).
    pub end: u64,
    /// Whether userspace can write to this region.
    pub writable: bool,
    /// What backs this region.
    pub kind: VmaKind,
}

/// Sorted list of non-overlapping VMAs for a process.
pub struct VmaList {
    vmas: Vec<Vma>,
}

impl VmaList {
    pub fn new() -> Self {
        Self { vmas: Vec::new() }
    }

    /// Find the VMA containing `addr`, if any.
    pub fn find(&self, addr: u64) -> Option<&Vma> {
        // Binary search: find rightmost VMA where start <= addr
        let idx = self.vmas.partition_point(|v| v.start <= addr);
        if idx == 0 { return None; }
        let vma = &self.vmas[idx - 1];
        if addr < vma.end { Some(vma) } else { None }
    }

    /// Insert a VMA, maintaining sorted order by start address.
    pub fn insert(&mut self, vma: Vma) {
        let idx = self.vmas.partition_point(|v| v.start < vma.start);
        self.vmas.insert(idx, vma);
    }

    /// Iterate over all VMAs.
    pub fn iter(&self) -> impl Iterator<Item = &Vma> {
        self.vmas.iter()
    }

    /// Remove all VMAs.
    pub fn clear(&mut self) {
        self.vmas.clear();
    }
}
