use super::{DirectMap, PAGE_2M};
use crate::sync::Lock;
use crate::MemoryMapEntry;

/// Region of physical memory to exclude from the free list.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

/// Proof of ownership of one 2MB physical page. Non-Copy, non-Clone.
/// Drop returns the page to the free list.
pub struct PhysPage(u64); // raw physical address, 2MB-aligned

impl PhysPage {
    /// Reconstruct from a raw physical address. Caller must ensure this
    /// is a valid 2MB-aligned page that was previously allocated.
    pub(super) fn from_raw(phys: u64) -> Self {
        Self(phys)
    }

    /// Physical address (for page table entries, internal use only).
    pub(super) fn phys(&self) -> u64 {
        self.0
    }

    /// Access this page through the kernel direct map.
    pub fn direct_map(&self) -> super::DirectMap {
        super::DirectMap::from_phys(self.0)
    }

    /// DMA address for device descriptors.
    pub fn dma_addr(&self) -> super::DmaAddr {
        super::DmaAddr(self.0)
    }
}

impl Drop for PhysPage {
    fn drop(&mut self) {
        free_page_raw(self.0);
    }
}

// --- Free list ---

struct FreeList {
    head: u64,       // physical address of first free page, 0 = empty
    free_count: u64,
    total_count: u64,
}

static FREE_LIST: Lock<FreeList> = Lock::new(FreeList {
    head: 0,
    free_count: 0,
    total_count: 0,
});

/// Sentinel value: no next page.
const NULL_PAGE: u64 = 0;

/// Read the `next` pointer from a free page (stored in its first 8 bytes).
fn read_next(phys: u64) -> u64 {
    unsafe { *super::DirectMap::from_phys(phys).as_ptr::<u64>() }
}

/// Write the `next` pointer into a free page.
fn write_next(phys: u64, next: u64) {
    unsafe { *super::DirectMap::from_phys(phys).as_mut_ptr::<u64>() = next; }
}

/// Initialize the free list from the UEFI memory map.
/// Extracts all 2MB-aligned chunks from usable regions, skipping reserved areas.
pub(super) fn init(entries: &[MemoryMapEntry], reserved: &[Region]) {
    let mut list = FREE_LIST.lock();

    for entry in entries.iter().filter(|e| is_usable(e)) {
        // Align region start up to 2MB, end down to 2MB
        let start = (entry.start + PAGE_2M - 1) & !(PAGE_2M - 1);
        let end = entry.end & !(PAGE_2M - 1);

        let mut addr = start;
        while addr + PAGE_2M <= end {
            // Skip if this 2MB chunk overlaps any reserved region
            if !overlaps_reserved(addr, addr + PAGE_2M, reserved) {
                // Zero the first 8 bytes (next pointer) and push onto list
                write_next(addr, list.head);
                list.head = addr;
                list.free_count += 1;
                list.total_count += 1;
            }
            addr += PAGE_2M;
        }
    }
}

/// Allocate one 2MB physical page. Returns None if out of memory.
pub(super) fn alloc_page() -> Option<PhysPage> {
    let mut list = FREE_LIST.lock();
    let phys = list.head;
    if phys == NULL_PAGE {
        return None;
    }
    list.head = read_next(phys);
    list.free_count -= 1;
    drop(list); // release lock before zeroing

    unsafe { core::ptr::write_bytes(DirectMap::from_phys(phys).as_mut_ptr::<u8>(), 0, PAGE_2M as usize); }

    Some(PhysPage(phys))
}

/// Return a page to the free list (called by PhysPage::drop).
fn free_page_raw(phys: u64) {
    let mut list = FREE_LIST.lock();
    write_next(phys, list.head);
    list.head = phys;
    list.free_count += 1;
}

/// Return (total_bytes, used_bytes).
pub fn stats() -> (u64, u64) {
    let list = FREE_LIST.lock();
    let total = list.total_count * PAGE_2M;
    let used = (list.total_count - list.free_count) * PAGE_2M;
    (total, used)
}

const EFI_LOADER_CODE: u32 = 1;
const EFI_LOADER_DATA: u32 = 2;
const EFI_BOOT_SERVICES_CODE: u32 = 3;
const EFI_BOOT_SERVICES_DATA: u32 = 4;
const EFI_CONVENTIONAL_MEMORY: u32 = 7;

fn is_usable(entry: &MemoryMapEntry) -> bool {
    matches!(entry.uefi_type,
        EFI_LOADER_CODE | EFI_LOADER_DATA |
        EFI_BOOT_SERVICES_CODE | EFI_BOOT_SERVICES_DATA |
        EFI_CONVENTIONAL_MEMORY)
}

fn overlaps_reserved(start: u64, end: u64, reserved: &[Region]) -> bool {
    reserved.iter().any(|r| start < r.end && end > r.start)
}
