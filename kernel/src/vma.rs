use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::UserAddr;
use crate::file_backing::FileBacking;

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

/// Sorted list of non-overlapping VMAs for a process.
pub struct VmaList {
    vmas: Vec<Vma>,
}

impl VmaList {
    pub fn new() -> Self {
        Self { vmas: Vec::new() }
    }

    /// Find the VMA containing `addr`, if any.
    pub fn find(&self, addr: UserAddr) -> Option<&Vma> {
        // Binary search: find rightmost VMA where start <= addr
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

    /// Remove all VMAs.
    pub fn clear(&mut self) {
        self.vmas.clear();
    }
}
