use core::alloc::Layout;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use alloc::alloc::{alloc_zeroed, dealloc};

use super::{apic, cpu};
use crate::{MemoryMapEntry, PHYS_OFFSET, PhysAddr, UserAddr};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_SIZE_BIT: u64 = 1 << 7; // 2MB large page
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const PAGE_4K: u64 = 4096;
pub const PAGE_2M: u64 = 2 * 1024 * 1024;
const MIN_PHYS_MAP: u64 = 4 * 1024 * 1024 * 1024; // 4GB minimum (covers MMIO regions)

/// Round `size` up to the next 2MB boundary.
pub const fn align_2m(size: usize) -> usize {
    (size + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1)
}

/// Whether addr is in the kernel's high-half direct map.
pub fn is_kernel_addr(addr: u64) -> bool {
    addr >= PHYS_OFFSET
}

/// Extract PML4, PDPT, and PD indices from a virtual address.
#[inline]
fn page_indices(addr: u64) -> (usize, usize, usize) {
    (
        ((addr >> 39) & 0x1FF) as usize,
        ((addr >> 30) & 0x1FF) as usize,
        ((addr >> 21) & 0x1FF) as usize,
    )
}

// ---------------------------------------------------------------------------
// PageTable — type-safe page table access
// ---------------------------------------------------------------------------

/// A page table accessible via the kernel direct map.
///
/// Internally holds a kernel *virtual* pointer for CPU access. All writes to
/// entries go through methods that enforce physical addresses — it is
/// structurally impossible to write a virtual address into a PTE.
struct PageTable(*mut u64);

impl PageTable {
    /// Allocate a new zeroed page table.
    fn alloc() -> Self {
        let layout = Layout::from_size_align(PAGE_4K as usize, PAGE_4K as usize).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "paging: out of memory for page table");
        Self(ptr as *mut u64)
    }

    /// Wrap an existing kernel virtual pointer as a PageTable.
    fn from_virt(ptr: *mut u64) -> Self {
        Self(ptr)
    }

    /// Wrap a physical address (from a PTE) as a PageTable.
    fn from_phys(phys: u64) -> Self {
        Self((phys + PHYS_OFFSET) as *mut u64)
    }

    /// Physical address of this page table (for writing into parent entries).
    fn phys(&self) -> u64 {
        self.0 as u64 - PHYS_OFFSET
    }

    /// Raw kernel virtual pointer (for CR3 writes and external APIs).
    fn as_ptr(&self) -> *mut u64 {
        self.0
    }

    /// Read an entry.
    fn read(&self, index: usize) -> u64 {
        unsafe { self.0.add(index).read() }
    }

    /// Read an entry (volatile, for crash diagnostics).
    fn read_volatile(&self, index: usize) -> u64 {
        unsafe { self.0.add(index).read_volatile() }
    }

    /// Write a raw entry value. Only used for copying kernel PML4 entries
    /// (which are already physical) and zeroing entries.
    fn write_raw(&self, index: usize, value: u64) {
        unsafe { self.0.add(index).write(value); }
    }

    /// Write a 2MB leaf entry pointing to a physical address.
    fn write_leaf_2m(&self, index: usize, phys: PhysAddr, flags: u64) {
        unsafe { self.0.add(index).write(phys.raw() | flags | PAGE_SIZE_BIT); }
    }

    /// Get or create a child page table at the given index.
    ///
    /// If the entry is present, returns the existing child (with flags upgraded).
    /// If absent, allocates a new zeroed page table and writes its *physical*
    /// address into the entry. The phys↔virt conversion is encapsulated here —
    /// callers never touch raw physical addresses.
    fn get_or_create(&self, index: usize, flags: u64) -> PageTable {
        let entry = self.read(index);
        if entry & PAGE_PRESENT != 0 {
            let updated = entry | (flags & (PAGE_PRESENT | PAGE_WRITE | PAGE_USER));
            if updated != entry {
                self.write_raw(index, updated);
            }
            PageTable::from_phys(entry & ADDR_MASK)
        } else {
            let child = PageTable::alloc();
            let expected = child.phys() | flags;
            self.write_raw(index, expected);
            child
        }
    }

    /// Read a child page table if present. Returns None if not present.
    fn child(&self, index: usize) -> Option<PageTable> {
        let entry = self.read(index);
        if entry & PAGE_PRESENT != 0 {
            Some(PageTable::from_phys(entry & ADDR_MASK))
        } else {
            None
        }
    }

    /// Free this page table's backing page.
    fn free(self) {
        let layout = Layout::from_size_align(PAGE_4K as usize, PAGE_4K as usize).unwrap();
        unsafe { dealloc(self.0 as *mut u8, layout); }
    }
}

// ---------------------------------------------------------------------------
// Kernel PML4
// ---------------------------------------------------------------------------

static KERNEL_PML4: AtomicPtr<u64> = AtomicPtr::new(core::ptr::null_mut());
static KERNEL_PHYS_START: AtomicU64 = AtomicU64::new(0);
static KERNEL_PHYS_END: AtomicU64 = AtomicU64::new(0);

/// Register the kernel's physical memory range for safety assertions.
pub fn set_kernel_phys_range(start: u64, size: u64) {
    KERNEL_PHYS_START.store(start, Ordering::Release);
    KERNEL_PHYS_END.store(start + size, Ordering::Release);
}

/// Panic if a physical address range overlaps the kernel's memory.
/// Called before writing any PDE into a user page table.
fn assert_user_phys_safe(phys_start: u64, size: u64) {
    let k_start = KERNEL_PHYS_START.load(Ordering::Acquire);
    let k_end = KERNEL_PHYS_END.load(Ordering::Acquire);
    if k_start == 0 && k_end == 0 { return; } // not yet initialized
    let p_end = phys_start + size;
    if phys_start < k_end && p_end > k_start {
        panic!(
            "map_user: physical range {:#x}..{:#x} overlaps kernel {:#x}..{:#x}",
            phys_start, p_end, k_start, k_end
        );
    }
}


/// Physical address of the kernel PML4 template. Used for idle/exit CR3.
pub fn kernel_cr3() -> PhysAddr {
    PhysAddr::from_ptr(KERNEL_PML4.load(Ordering::Acquire))
}

/// Build kernel page tables: map all physical memory in the high half
/// (PML4[256+]) using 2MB large pages. No identity map — PML4[0..255] is empty.
pub fn init(memory_map: &[MemoryMapEntry]) {
    let mut max_addr: u64 = MIN_PHYS_MAP;
    for entry in memory_map {
        if entry.end > max_addr {
            max_addr = entry.end;
        }
    }
    max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let pml4 = PageTable::alloc();

    let mut addr: u64 = 0;
    while addr < max_addr {
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(PHYS_OFFSET + addr);
        let pdpt = pml4.get_or_create(pml4_idx, PAGE_PRESENT | PAGE_WRITE);
        let pd = pdpt.get_or_create(pdpt_idx, PAGE_PRESENT | PAGE_WRITE);
        pd.write_leaf_2m(pd_idx, PhysAddr::new(addr), PAGE_PRESENT | PAGE_WRITE);
        addr += PAGE_2M;
    }

    KERNEL_PML4.store(pml4.as_ptr(), Ordering::Release);
    unsafe { cpu::write_cr3(PhysAddr::from_ptr(pml4.as_ptr())); }
}

// ---------------------------------------------------------------------------
// Per-process page table operations
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// PML4 corruption detection
// ---------------------------------------------------------------------------

const PML4_CANARY_SLOT: usize = 255;
const PML4_CANARY_MAGIC: u64 = 0xCAFE_BABE_DEAD_0000;
const MAX_ACTIVE_PML4S: usize = 64;

static ACTIVE_PML4_COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static ACTIVE_PML4S: [core::sync::atomic::AtomicU64; MAX_ACTIVE_PML4S] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_ACTIVE_PML4S];

fn register_active_pml4(phys: PhysAddr) {
    let idx = ACTIVE_PML4_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if idx < MAX_ACTIVE_PML4S {
        ACTIVE_PML4S[idx].store(phys.raw(), core::sync::atomic::Ordering::Relaxed);
    }
}

fn unregister_active_pml4(phys: u64) {
    let count = ACTIVE_PML4_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    for i in 0..count.min(MAX_ACTIVE_PML4S) {
        if ACTIVE_PML4S[i].load(core::sync::atomic::Ordering::Relaxed) == phys {
            ACTIVE_PML4S[i].store(0, core::sync::atomic::Ordering::Relaxed);
            return;
        }
    }
}

/// Inline canary check before write_cr3. Lock-free — reads directly via physical map.
pub fn assert_pml4_canary(phys: PhysAddr, tid: crate::process::Tid) {
    let pml4 = PageTable::from_phys(phys.raw());
    let canary = pml4.read_volatile(PML4_CANARY_SLOT);
    let expected = pml4_canary(phys.raw());
    if canary != expected {
        let kernel = PageTable::from_virt(KERNEL_PML4.load(Ordering::Acquire));
        let entry_256 = pml4.read_volatile(256);
        let expected_256 = kernel.read(256);
        let mut dump = [0u64; 8];
        for j in 0..8 {
            dump[j] = pml4.read_volatile(j);
        }
        panic!(
            "PML4 CORRUPTION before write_cr3: phys={:#x} tid={}\n\
             canary[255]: got {:#x}, expected {:#x}\n\
             entry[256]: got {:#x}, expected {:#x}\n\
             [0..8]: {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}",
            phys.raw(), tid, canary, expected, entry_256, expected_256,
            dump[0], dump[1], dump[2], dump[3], dump[4], dump[5], dump[6], dump[7],
        );
    }
}

/// Check all active PML4s for canary corruption. Called from timer interrupt.
pub fn check_pml4_canaries() {
    let count = ACTIVE_PML4_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    let kernel = PageTable::from_virt(KERNEL_PML4.load(Ordering::Acquire));
    if kernel.as_ptr().is_null() { return; }
    let expected_256 = kernel.read(256);

    for i in 0..count.min(MAX_ACTIVE_PML4S) {
        let phys = ACTIVE_PML4S[i].load(core::sync::atomic::Ordering::Relaxed);
        if phys == 0 { continue; }

        let pml4 = PageTable::from_phys(phys);
        let canary = pml4.read_volatile(PML4_CANARY_SLOT);
        let expected_canary = pml4_canary(phys);

        if canary != expected_canary {
            let entry_256 = pml4.read_volatile(256);
            // Dump first 8 entries to identify the corrupting data
            let mut dump = [0u64; 8];
            for j in 0..8 {
                dump[j] = pml4.read_volatile(j);
            }
            panic!(
                "PML4 CORRUPTION: phys={:#x}\n\
                 canary[255]: got {:#x}, expected {:#x}\n\
                 entry[256]: got {:#x}, expected {:#x}\n\
                 [0..8]: {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}",
                phys, canary, expected_canary, entry_256, expected_256,
                dump[0], dump[1], dump[2], dump[3], dump[4], dump[5], dump[6], dump[7],
            );
        }
    }
}

fn pml4_canary(phys: u64) -> u64 {
    PML4_CANARY_MAGIC | (phys & 0xFFFF_FFFF_F000)
}

/// Create a per-process PML4.
/// PML4[0..255]: empty. PML4[256..511]: shallow-copy kernel high-half entries.
pub fn create_user_pml4() -> PhysAddr {
    let kernel = PageTable::from_virt(KERNEL_PML4.load(Ordering::Acquire));
    assert!(!kernel.as_ptr().is_null(), "paging: not initialized");

    let pml4 = PageTable::alloc();
    for i in 256..512 {
        let entry = kernel.read(i);
        if entry & PAGE_PRESENT != 0 {
            pml4.write_raw(i, entry);
        }
    }

    let phys = PhysAddr::from_ptr(pml4.as_ptr());
    // Write canary into unused slot 255 for corruption detection
    pml4.write_raw(PML4_CANARY_SLOT, pml4_canary(phys.raw()));
    register_active_pml4(phys);
    phys
}

/// Free a per-process PML4 and all its user page table structures.
/// Only frees PML4[0..255]. PML4[256+] are shared kernel entries.
pub fn free_user_page_tables(pml4_ptr: *mut u64) {
    let pml4 = PageTable::from_virt(pml4_ptr);
    unregister_active_pml4(pml4.phys());
    // Clear canary before freeing so the page can be safely reused.
    pml4.write_raw(PML4_CANARY_SLOT, 0);
    for pml4_idx in 0..256 {
        let Some(pdpt) = pml4.child(pml4_idx) else { continue };
        for pdpt_idx in 0..512 {
            let Some(pd) = pdpt.child(pdpt_idx) else { continue };
            pd.free();
        }
        pdpt.free();
    }
    pml4.free();
}

/// Map 2MB pages: vaddr → phys for `size` bytes. Creates page table structures on demand.
pub(crate) fn map_user_at(pml4_ptr: *mut u64, vaddr: UserAddr, phys: PhysAddr, size: u64) {
    let pml4 = PageTable::from_virt(pml4_ptr);
    let vstart = vaddr.raw() & !(PAGE_2M - 1);
    let pstart = phys.raw() & !(PAGE_2M - 1);
    let total = ((phys.raw() + size + PAGE_2M - 1) & !(PAGE_2M - 1)) - pstart;

    assert_user_phys_safe(pstart, total);

    let mut off = 0u64;
    while off < total {
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(vstart + off);
        let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
        let pdpt = pml4.get_or_create(pml4_idx, user_flags);
        let pd = pdpt.get_or_create(pdpt_idx, user_flags);
        pd.write_leaf_2m(pd_idx, PhysAddr::new(pstart + off), user_flags);
        off += PAGE_2M;
    }
}

/// Unmap 2MB pages at a virtual address range.
pub(crate) fn unmap_user_at(pml4_ptr: *mut u64, vaddr: UserAddr, size: u64) {
    let pml4 = PageTable::from_virt(pml4_ptr);
    let start = vaddr.raw() & !(PAGE_2M - 1);
    let end = (vaddr.raw() + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;
    while cur < end {
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(cur);
        if let Some(pdpt) = pml4.child(pml4_idx) {
            if let Some(pd) = pdpt.child(pdpt_idx) {
                pd.write_raw(pd_idx, 0);
            }
        }
        cur += PAGE_2M;
    }
}

/// Map a single 2MB page at vaddr → phys with explicit write control.
pub(crate) fn remap_user_2m(pml4_ptr: *mut u64, virt_addr: UserAddr, phys_addr: PhysAddr, writable: bool) -> bool {
    if virt_addr.raw() & (PAGE_2M - 1) != 0 || phys_addr.raw() & (PAGE_2M - 1) != 0 {
        return false;
    }
    assert_user_phys_safe(phys_addr.raw(), PAGE_2M);
    let pml4 = PageTable::from_virt(pml4_ptr);
    let (pml4_idx, pdpt_idx, pd_idx) = page_indices(virt_addr.raw());
    let mut flags = PAGE_PRESENT | PAGE_USER;
    if writable { flags |= PAGE_WRITE; }
    let pdpt = pml4.get_or_create(pml4_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
    let pd = pdpt.get_or_create(pdpt_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
    pd.write_leaf_2m(pd_idx, phys_addr, flags);
    true
}

/// Clear a single 2MB user PDE.
pub(crate) fn clear_user_2m(pml4_ptr: *mut u64, virt_addr: UserAddr) {
    let pml4 = PageTable::from_virt(pml4_ptr);
    let (pml4_idx, pdpt_idx, pd_idx) = page_indices(virt_addr.raw());
    if let Some(pdpt) = pml4.child(pml4_idx) {
        if let Some(pd) = pdpt.child(pdpt_idx) {
            pd.write_raw(pd_idx, 0);
        }
    }
    cpu::flush_tlb();
    apic::tlb_shootdown();
}

/// Identity-map an MMIO region as kernel-only using 2MB large pages.
pub fn map_kernel(addr: PhysAddr, size: u64) {
    let pml4 = PageTable::from_virt(KERNEL_PML4.load(Ordering::Acquire));
    assert!(!pml4.as_ptr().is_null(), "paging: not initialized");

    let start = addr.raw() & !(PAGE_2M - 1);
    let end = (addr.raw() + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;
    while cur < end {
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(PHYS_OFFSET + cur);
        let pdpt = pml4.get_or_create(pml4_idx, PAGE_PRESENT | PAGE_WRITE);
        let pd = pdpt.get_or_create(pdpt_idx, PAGE_PRESENT | PAGE_WRITE);
        if pd.read(pd_idx) & PAGE_PRESENT == 0 {
            pd.write_leaf_2m(pd_idx, PhysAddr::new(cur), PAGE_PRESENT | PAGE_WRITE);
        }
        cur += PAGE_2M;
    }
    cpu::flush_tlb();
    apic::tlb_shootdown();
}

/// Translate a virtual address to physical by walking the page tables.
pub fn virt_to_phys(pml4_ptr: *const u64, virt_addr: UserAddr) -> Option<PhysAddr> {
    let pml4 = PageTable::from_virt(pml4_ptr as *mut u64);
    let va = virt_addr.raw();
    let (pml4_idx, pdpt_idx, pd_idx) = page_indices(va);
    let pdpt = pml4.child(pml4_idx)?;
    let pd = pdpt.child(pdpt_idx)?;
    let pde = pd.read(pd_idx);
    if pde & PAGE_PRESENT == 0 { return None; }
    Some(PhysAddr::new((pde & ADDR_MASK) + (va & (PAGE_2M - 1))))
}

// ---------------------------------------------------------------------------
// Debug
// ---------------------------------------------------------------------------

fn has(entry: u64, flag: u64) -> u8 {
    if entry & flag != 0 { 1 } else { 0 }
}

/// Dump page table entries for an address. Lock-free for crash safety.
pub fn debug_page_walk(addr: u64) {
    use crate::log;

    let pml4 = PageTable::from_virt(cpu::read_cr3().as_mut_ptr());
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((addr >> 21) & 0x1FF) as usize;
    let pt_idx = ((addr >> 12) & 0x1FF) as usize;

    log!("  Page walk for {:#x} [PML4={:#x} PML4[{}] PDPT[{}] PD[{}] PT[{}]]:",
        addr, pml4.as_ptr() as u64, pml4_idx, pdpt_idx, pd_idx, pt_idx);

    let pml4e = pml4.read_volatile(pml4_idx);
    log!("    PML4E: {:#018x} P={} W={} U={}", pml4e,
        has(pml4e, PAGE_PRESENT), has(pml4e, PAGE_WRITE), has(pml4e, PAGE_USER));
    if pml4e & PAGE_PRESENT == 0 { return; }

    let pdpt = PageTable::from_phys(pml4e & ADDR_MASK);
    let pdpte = pdpt.read_volatile(pdpt_idx);
    log!("    PDPTE: {:#018x} P={} W={} U={}", pdpte,
        has(pdpte, PAGE_PRESENT), has(pdpte, PAGE_WRITE), has(pdpte, PAGE_USER));
    if pdpte & PAGE_PRESENT == 0 { return; }

    let pd = PageTable::from_phys(pdpte & ADDR_MASK);
    let pde = pd.read_volatile(pd_idx);
    log!("    PDE:   {:#018x} P={} W={} U={} PS={}", pde,
        has(pde, PAGE_PRESENT), has(pde, PAGE_WRITE), has(pde, PAGE_USER), has(pde, PAGE_SIZE_BIT));
    if pde & PAGE_PRESENT == 0 { return; }
    if pde & PAGE_SIZE_BIT != 0 {
        log!("    -> 2MB large page at {:#x}", pde & ADDR_MASK);
        return;
    }

    let pt = PageTable::from_phys(pde & ADDR_MASK);
    let pte = pt.read_volatile(pt_idx);
    log!("    PTE:   {:#018x} P={} W={} U={}", pte,
        has(pte, PAGE_PRESENT), has(pte, PAGE_WRITE), has(pte, PAGE_USER));
    if pte & PAGE_PRESENT == 0 { return; }
    log!("    -> 4KB page at {:#x}", pte & ADDR_MASK);
}
