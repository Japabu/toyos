// Page tables and address spaces.
//
// The only code that writes page table entries. Manages the kernel direct map
// (all physical memory at PHYS_OFFSET) and per-process user address spaces.
//
// **An invalidation is derived from the entry that was replaced, never chosen
// by the caller.** Every write into a paging structure the hardware may already
// be walking goes through `PageTablePage::write` or `widen`, both of which
// return the `Owed` their prior value implies; `Owed` is `#[must_use]` and the
// three ways to end one are the three answers there are. The rule is the SDM's:
// a not-present entry creates no TLB entry and no paging-structure-cache entry
// (Vol. 3A §4.10.2.3), so a write over one can leave nothing stale and an
// `invlpg` there is pure cost — §4.10.4.3 says so as a permission. A write over
// a present entry is invalidated whatever the caller believes, because a missing
// invalidation is silent and a redundant one is only slow.
//
// **What is discharged here reaches this CPU and no other.** Telling the rest of
// the machine is `arch::tlb::shootdown`, and it stays the caller's: which other
// CPUs hold a translation is not knowable from the entry.
//
// **No mapping in this kernel is global.** There is no `PAGE_GLOBAL` and
// `CR4.PGE` is not in `arch::control_regs`'s declaration, which is what makes
// the single-address forms complete here: INVPCID type 0 and INVLPG both leave
// global translations alone. Adding PGE means revisiting every `discharge`.
//
// **The tagged branches are not exercised on the dev host.** Its QEMU is
// `-cpu qemu64`, which offers no PCID, so `pcid_active()` is false and a guest
// there boots holding `cr4=0x00310668` — read out of a harness UART log on
// 2026-08-19. Only a KVM runner with `-cpu host` executes an `INVPCID` at all,
// so what answers for those branches is the derivation and not the harness.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use hashbrown::HashMap;

use super::{UserAddr, PAGE_2M};
use crate::sync::Lock;
use crate::vma::{self, Region, RegionKind};
use crate::MemoryMapEntry;

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_WRITE_THROUGH: u64 = 1 << 3;
const PAGE_CACHE_DISABLE: u64 = 1 << 4;
/// Set by the CPU on any access and on any write, so a mapping that has been
/// used no longer equals the entry its mapper wrote.
const PAGE_ACCESSED: u64 = 1 << 5;
const PAGE_DIRTY: u64 = 1 << 6;
const PAGE_SIZE_BIT: u64 = 1 << 7;
/// The PAT bit of a 2 MiB PDE. In a 4 KiB PTE the same bit is bit 7, so a PDE's
/// flags cannot be carried across a split without moving it.
const PAGE_PAT_2M: u64 = 1 << 12;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const ADDR_MASK_2M: u64 = 0x000F_FFFF_FFE0_0000;

/// Which PAT entry a 2 MiB mapping selects, and so what memory type its pages
/// have.
///
/// Every mapping states one. PAT, PCD and PWT together choose the entry; this
/// kernel leaves PCD and PWT clear everywhere, so the PAT bit alone decides and
/// only two of the eight entries are ever reachable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CachePolicy {
    /// PAT entry 0, which is WB and never rewritten. A WB entry takes the
    /// MTRR's type for the physical range (SDM Vol. 3A Table 11-7), so these
    /// pages are whatever firmware decided they are —
    /// [`mtrr::range_type`](crate::arch::mtrr::range_type) is the answer.
    DeferToMtrr,
    /// PAT entry [`pat::WC_ENTRY`](crate::arch::pat::WC_ENTRY), which
    /// [`pat::init`](crate::arch::pat::init) programs to WC on every CPU.
    WriteCombining,
}

impl CachePolicy {
    fn pde_bits(self) -> u64 {
        match self {
            Self::DeferToMtrr => 0,
            Self::WriteCombining => PAGE_PAT_2M,
        }
    }

    /// The policy a 2 MiB PDE carries.
    ///
    /// PCD or PWT set means an entry this kernel did not write, and its memory
    /// type is not one of the two named here.
    fn from_pde(pde: u64) -> Self {
        assert!(
            pde & (PAGE_CACHE_DISABLE | PAGE_WRITE_THROUGH) == 0,
            "CachePolicy::from_pde: {pde:#x} selects a PAT entry outside 0 and {}",
            crate::arch::pat::WC_ENTRY
        );
        if pde & PAGE_PAT_2M != 0 { Self::WriteCombining } else { Self::DeferToMtrr }
    }
}

const _: () = assert!(
    crate::arch::pat::WC_ENTRY == 4,
    "WriteCombining sets the PAT bit and leaves PCD and PWT clear, which is entry 4",
);

/// A 4KB-aligned page of 512 entries, matching the hardware page table format.
#[repr(C, align(4096))]
struct PageTablePage([u64; 512]);

impl PageTablePage {
    fn phys(&self) -> u64 {
        super::DirectMap::phys_of(self)
    }

    unsafe fn from_phys<'a>(phys: u64) -> &'a PageTablePage {
        &*super::DirectMap::from_phys(phys).as_ptr::<PageTablePage>()
    }

    unsafe fn from_phys_mut<'a>(phys: u64) -> &'a mut PageTablePage {
        &mut *super::DirectMap::from_phys(phys).as_mut_ptr::<PageTablePage>()
    }

    fn child(&self, index: usize) -> Option<&PageTablePage> {
        let entry = self[index];
        if entry & PAGE_PRESENT != 0 {
            Some(unsafe { PageTablePage::from_phys(entry & ADDR_MASK) })
        } else {
            None
        }
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut PageTablePage> {
        let entry = self[index];
        if entry & PAGE_PRESENT != 0 {
            Some(unsafe { PageTablePage::from_phys_mut(entry & ADDR_MASK) })
        } else {
            None
        }
    }

    /// Write one entry of a table the hardware may already be walking, and say
    /// what the write owes the TLB.
    ///
    /// The only way to change a live entry, leaf or not: the answer to "does
    /// this need an invalidation" is the prior value, and this is the only place
    /// that still has it. `va` is any address the entry covers — for a 2 MiB PDE
    /// that is the page's own address, for an upper level any address below it.
    fn write(&mut self, idx: usize, va: u64, value: u64) -> Owed {
        Owed::of(core::mem::replace(&mut self.0[idx], value), va)
    }

    /// Widen an upper-level entry, which x86-64 requires to be at least as
    /// permissive as any leaf below it.
    ///
    /// A widening that changes nothing owes nothing, and today none of them
    /// changes anything: [`AddressSpace::ensure_table`] masks to `TABLE_FLAGS`
    /// and every live upper entry was created carrying them. One that did would
    /// owe a flush rather than a hope — a stale narrower paging-structure-cache
    /// entry raises a spurious fault, which this kernel resolves against no
    /// region and turns into a dead process.
    fn widen(&mut self, idx: usize, flags: u64) -> Owed {
        let before = self.0[idx];
        self.0[idx] = before | flags;
        if self.0[idx] == before { Owed::Nothing } else { Owed::Context }
    }

    /// Write into a table no CPU can be walking yet, because it has not been
    /// linked into a live paging structure. Nothing can be stale, so nothing is
    /// returned to discharge.
    fn init_entry(&mut self, idx: usize, value: u64) {
        self.0[idx] = value;
    }
}

/// What a write into a live paging structure owes this CPU's TLB.
///
/// A pure consequence of the entry that was there — [`Owed::of`] is the whole
/// decision, and it takes the prior entry and nothing else. The caller cannot
/// pick: it can only say which of the three true things is true, and each of the
/// three is a claim the site has to be able to defend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use = "an invalidation that is owed and not discharged is silent"]
enum Owed {
    /// Nothing. The slot held a not-present entry, from which no TLB entry and
    /// no paging-structure-cache entry is ever created (SDM Vol. 3A §4.10.2.3),
    /// or the write changed no bit.
    Nothing,
    /// One linear address, whose leaf changed under it.
    Address { va: u64, prior: u64 },
    /// Everything under an upper-level entry whose permissions widened.
    Context,
}

impl Owed {
    /// The decision, as a function of the prior entry alone.
    fn of(prior: u64, va: u64) -> Self {
        if prior & PAGE_PRESENT == 0 {
            Self::Nothing
        } else {
            Self::Address { va, prior }
        }
    }

    /// Pay it, on this CPU, in the address space the entry belongs to.
    ///
    /// `target` is the written address space's `CR3` and never the live one.
    /// With PCID a TLB entry carries the tag of the address space it came from,
    /// and `INVPCID` names that tag directly (SDM Vol. 3A §4.10.4.1) — so a
    /// kernel writing a *child's* tables from the parent's CPU, which
    /// `loader::map_libs` does on every spawn, invalidates the child's entries
    /// and not the parent's live ones. Reading `CR3` here instead did the
    /// opposite of what the site meant on exactly that path.
    ///
    /// Without PCID there is no tag to name and none is needed: every `CR3`
    /// write flushes the whole TLB, so a CPU holds entries for the address space
    /// it has loaded and for no other, and there is nothing to invalidate for
    /// one it has not.
    fn discharge(self, target: Cr3) {
        match self {
            Self::Nothing => {}
            Self::Address { va, .. } => {
                if pcid_active() {
                    crate::arch::cpu::invpcid(0, target.pcid() as u64, va);
                } else if Cr3::current().phys() == target.phys() {
                    crate::arch::cpu::invlpg(va);
                }
            }
            Self::Context => {
                if pcid_active() {
                    crate::arch::cpu::invpcid(1, target.pcid() as u64, 0);
                } else if Cr3::current().phys() == target.phys() {
                    flush_tlb_all();
                }
            }
        }
    }

    /// The write installed into a slot the caller has already proven empty, and
    /// an empty slot owes nothing.
    ///
    /// The proof is this value rather than a check the caller makes and throws
    /// away: `map_range` used to read the slot, assert it was absent, and then
    /// invalidate anyway.
    fn expect_install(self, what: &str) {
        match self {
            Self::Nothing => {}
            Self::Address { va, prior } => {
                panic!("{what}: an install at {va:#x} found the present entry {prior:#x}")
            }
            Self::Context => panic!("{what}: an install widened an upper-level entry"),
        }
    }

    /// The caller flushes more than this entry could owe before the mapping is
    /// used — a machine-wide shootdown, or a local full flush for an address
    /// space only this CPU can be holding. Both subsume a single address.
    fn subsumed_by_flush(self) {}
}

impl core::ops::Index<usize> for PageTablePage {
    type Output = u64;
    fn index(&self, idx: usize) -> &u64 {
        &self.0[idx]
    }
}

#[inline]
fn indices(addr: u64) -> (usize, usize, usize) {
    (
        ((addr >> 39) & 0x1FF) as usize,
        ((addr >> 30) & 0x1FF) as usize,
        ((addr >> 21) & 0x1FF) as usize,
    )
}

const CR3_NOFLUSH: u64 = 1 << 63;
const CR3_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Whether `CR4.PCIDE` is in the machine's control-register declaration, and
/// therefore whether a targeted flush exists. Without INVPCID there is no way
/// to flush all PCIDs, so `control_regs` declares neither without the other and
/// a context switch flushes the whole TLB instead.
pub fn pcid_active() -> bool {
    crate::arch::control_regs::pcid_active()
}

/// Flush all TLB entries on this CPU, all PCIDs.
pub fn flush_tlb_all() {
    if pcid_active() {
        crate::arch::cpu::invpcid(2, 0, 0);
    } else {
        unsafe {
            let cr3 = crate::arch::cpu::read_cr3();
            crate::arch::cpu::write_cr3(cr3);
        }
    }
}

/// CR3 register value: PML4 physical address | PCID.
#[derive(Clone, Copy)]
pub struct Cr3(u64);

impl Cr3 {
    pub fn current() -> Self {
        Self(crate::arch::cpu::read_cr3())
    }

    pub fn phys(self) -> u64 {
        self.0 & CR3_ADDR_MASK
    }
    pub fn pcid(self) -> u16 {
        (self.0 & 0xFFF) as u16
    }

    /// Switch to this address space. With PCID, sets NOFLUSH to preserve
    /// other processes' TLB entries. Without PCID, plain CR3 write.
    ///
    /// # Safety
    /// The underlying page tables must be valid and live.
    pub unsafe fn activate(self) {
        if pcid_active() {
            crate::arch::cpu::write_cr3(self.0 | CR3_NOFLUSH);
        } else {
            crate::arch::cpu::write_cr3(self.0);
        }
    }

    /// Load CR3 with a TLB flush. Used during boot before PCID is enabled.
    ///
    /// # Safety
    /// The underlying page tables must be valid and live.
    pub unsafe fn load_flush(self) {
        crate::arch::cpu::write_cr3(self.0);
    }
}

/// Next PCID to allocate. Range 1..4095. PCID 0 is reserved for the kernel.
static NEXT_PCID: Lock<u16> = Lock::new(1);

/// Allocate a unique PCID for a new user address space.
/// On wrap past 4095, flushes all TLBs on all CPUs before recycling.
///
/// The shootdown is outside the lock, because it waits for every other CPU and
/// may hold nothing while it does. That costs no atomicity: what has to be true
/// is that no CPU still carries a translation under a recycled tag by the time
/// the caller *activates* it, and the caller is `new_user`, which has not
/// returned an address space yet — so a second CPU that takes tag 2 out of the
/// same wrap is covered by a flush that names every tag on every CPU.
///
/// Recycling a live tag at all is a defect in its own right and stays open;
/// M4 makes the tag an owned resource and deletes this branch.
fn alloc_pcid() -> u16 {
    {
        let mut next = NEXT_PCID.lock();
        let pcid = *next;
        if pcid <= 4095 {
            *next = pcid + 1;
            return pcid;
        }
        *next = 2;
    }
    crate::arch::tlb::shootdown();
    1
}

/// Unified address space: hardware page tables + virtual memory region tracking.
///
/// PML4[0..255] = user mappings (per-process), PML4[256..511] = kernel direct map (shared).
/// `regions` tracks all mapped virtual memory areas (ELF segments, mmap, stack, etc.)
/// and serves as the source of truth for the virtual address allocator.
pub struct AddressSpace {
    root: Box<PageTablePage>,
    children: Vec<Box<PageTablePage>>,
    /// Physical data pages mapped into user space, keyed by physical address. Freed on drop.
    pages: HashMap<u64, super::pmm::PhysPage>,
    /// All virtual memory regions, keyed by start address.
    regions: BTreeMap<UserAddr, Region>,
    /// PCID for this address space. 0 = kernel, 1..4095 = user.
    pcid: u16,
}

unsafe impl Send for AddressSpace {}
unsafe impl Sync for AddressSpace {}

/// What a range already in `regions` runs into — [`AddressSpace::occupancy`].
///
/// A *placed* mapping names its own range instead of taking a free one from
/// `find_gap`, so the question the allocator answers silently for everybody
/// else has to be asked out loud for it. There is exactly one such caller, the
/// FIXED arm of `sys_mmap`, and the address it asks about came from userland.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Occupancy {
    /// Nothing is registered over any part of it.
    Free,
    /// One region covers it end for end, and that region is all it runs into.
    Whole,
    /// Anything else: part of a region, several regions, or a region that
    /// merely starts where this one does.
    Partial,
}

fn align_up_2m(v: u64) -> u64 {
    (v + PAGE_2M - 1) & !(PAGE_2M - 1)
}

impl AddressSpace {
    /// Create a new user address space with kernel entries shallow-copied.
    pub fn new_user() -> Self {
        let kernel_as = kernel().lock();
        let mut pml4 = Box::new(PageTablePage([0; 512]));

        for i in 256..512 {
            if kernel_as.root[i] & PAGE_PRESENT != 0 {
                pml4.init_entry(i, kernel_as.root[i]);
            }
        }

        Self {
            root: pml4,
            children: Vec::new(),
            pages: HashMap::new(),
            regions: BTreeMap::new(),
            pcid: alloc_pcid(),
        }
    }

    pub fn cr3(&self) -> Cr3 {
        Cr3(self.root.phys() | self.pcid as u64)
    }

    /// Map a contiguous physical region into user space as 2MB pages.
    /// Asserts: vaddr and phys are 2MB-aligned, all PDE slots are empty.
    ///
    /// The empty slots are what makes this free of the TLB: nothing was present,
    /// so nothing anywhere can be stale, and no CPU is told anything.
    pub fn map_range(
        &mut self,
        vaddr: UserAddr,
        phys: u64,
        size: u64,
        writable: bool,
        cache: CachePolicy,
    ) {
        assert!(
            vaddr.raw() & (PAGE_2M - 1) == 0,
            "map_range: vaddr not 2MB-aligned"
        );
        assert!(
            phys & (PAGE_2M - 1) == 0,
            "map_range: phys {phys:#x} not 2MB-aligned"
        );
        let mut offset = 0u64;
        while offset < size {
            let va = vaddr.raw() + offset;
            let pa = phys + offset;
            let mut flags = PAGE_PRESENT | PAGE_USER | cache.pde_bits();
            if writable {
                flags |= PAGE_WRITE;
            }
            let pd_idx = indices(va).2;
            let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
            let pd = self.ensure_table(va, user_flags, user_flags);
            pd.write(pd_idx, va, pa | flags | PAGE_SIZE_BIT)
                .expect_install("map_range");
            offset += PAGE_2M;
        }
    }

    /// Unmap a contiguous range of 2MB pages.
    ///
    /// Private: unmapping a range without unregistering it is what left a
    /// placed `mmap` invisible to the placement search, so the only way out of
    /// this module is [`free_and_unmap`](Self::free_and_unmap), which does both.
    fn unmap_range(&mut self, vaddr: UserAddr, size: u64) {
        let mut offset = 0u64;
        while offset < size {
            self.unmap(UserAddr::new(vaddr.raw() + offset));
            offset += PAGE_2M;
        }
    }

    /// Map a single 2MB page, replacing any existing mapping.
    /// Used by demand paging and shared library RW overlay.
    ///
    /// **The invalidation is this method's, and it is derived.** A demand-paging
    /// fault replaces a not-present PDE — the kernel's hottest paging path — and
    /// pays nothing for the TLB; a replacement of a live mapping is invalidated
    /// in *this* address space, whatever this CPU happens to have loaded. What
    /// the other CPUs are owed is not derivable from the entry and stays with the
    /// caller: `sys_dlopen` replaces a mapping a sibling thread can be running
    /// on and shoots down, `loader::map_libs` writes a child no CPU has ever
    /// loaded and does not.
    pub fn remap(&mut self, vaddr: UserAddr, phys: u64, writable: bool) {
        let va = vaddr.raw();
        assert!(
            va & (PAGE_2M - 1) == 0,
            "remap: vaddr {va:#x} not 2MB-aligned"
        );
        assert!(
            phys & (PAGE_2M - 1) == 0,
            "remap: phys {phys:#x} not 2MB-aligned"
        );

        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable {
            flags |= PAGE_WRITE;
        }

        let pd_idx = indices(va).2;
        let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
        let target = self.cr3();
        let pd = self.ensure_table(va, user_flags, user_flags);
        pd.write(pd_idx, va, phys | flags | PAGE_SIZE_BIT).discharge(target);
    }

    /// Unmap one 2MB page and free its physical memory.
    ///
    /// **A wait armed on a word in this frame ends here**, because there is
    /// nothing left for it to be woken by. A futex waiter's completion token is
    /// its word's *physical* address — that is what makes a futex in shared
    /// memory work across processes — and nothing pins the frame behind it, so
    /// the moment the entry below goes and the page reaches the PMM that token
    /// names whatever is mapped there next. `revoke_futex_range` is the
    /// deletion of that hazard rather than bookkeeping against it: the waiter is
    /// told `Gone(Closed)` and taken off the bucket, so no later `futex_wake`
    /// can find it. See `sched::waitqs::revoke_futex_range`.
    ///
    /// It runs on every present entry rather than only on the frames this
    /// address space owns. A shared-memory page is unmapped here too and its
    /// frame outlives the unmap, so a waiter in *another* process is ended
    /// where it did not have to be — a spurious futex return, which every park
    /// site is allowed and every futex loop in userland already re-checks.
    /// The converse mistake is not
    /// survivable, and telling the two apart would put the ownership question
    /// on a path whose only wrong answer is a use-after-free.
    pub fn unmap(&mut self, vaddr: UserAddr) {
        let va = vaddr.raw();
        assert!(
            va & (PAGE_2M - 1) == 0,
            "unmap: vaddr {va:#x} not 2MB-aligned"
        );

        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let target = self.cr3();

        if let Some(pdpt) = self.root.child_mut(pml4_idx) {
            if let Some(pd) = pdpt.child_mut(pdpt_idx) {
                let pde = pd[pd_idx];
                if pde & PAGE_PRESENT != 0 {
                    pd.write(pd_idx, va, 0).discharge(target);
                    let phys = pde & ADDR_MASK_2M;
                    // Before the page can reach the PMM, and after the entry is
                    // gone: a waiter that translated this word already is on
                    // the bucket for this walk to find, and one arriving after
                    // it has nothing to translate.
                    crate::sched::waitqs::revoke_futex_range(phys, PAGE_2M);
                    // Remove the page from our owned list — Drop frees it.
                    // No-op for shared memory pages (not in this map).
                    self.pages.remove(&phys);
                }
            }
        }
    }

    /// Translate a user virtual address to a DirectMap handle.
    /// Returns None if the page is not mapped, or if `vaddr` is not a user
    /// address at all.
    ///
    /// The bound is here rather than at the eight callers because a user
    /// address space shallow-copies the kernel's PML4 half: a kernel address
    /// walks to the direct map's own 2 MiB leaf, and the caller gets a
    /// writable pointer into kernel memory.
    pub fn translate(&self, vaddr: UserAddr) -> Option<super::DirectMap> {
        let va = vaddr.raw();
        if !toyos_userbound::is_user_addr(va) {
            return None;
        }
        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let pdpt = self.root.child(pml4_idx)?;
        let pd = pdpt.child(pdpt_idx)?;
        let pde = pd[pd_idx];
        if pde & PAGE_PRESENT == 0 {
            return None;
        }
        let page_phys = pde & ADDR_MASK_2M;
        let offset = va & (PAGE_2M - 1);
        Some(super::DirectMap::from_phys(page_phys + offset))
    }

    /// Find a free gap of at least `size` bytes (2MB-aligned), searching top-down.
    fn find_gap(&self, size: u64) -> Option<UserAddr> {
        let aligned = align_up_2m(size);
        let total = aligned + vma::GUARD_SIZE;

        let mut top = vma::ALLOC_CEILING;
        for (&start, region) in self
            .regions
            .range(..UserAddr::new(vma::ALLOC_CEILING))
            .rev()
        {
            let region_end = align_up_2m(start.raw() + region.size);
            if region_end > top {
                top = start.raw();
                continue;
            }
            let gap = top - region_end;
            if gap >= total {
                return Some(UserAddr::new(top - total));
            }
            top = start.raw();
        }
        // Gap below all regions
        if top >= total + vma::alloc_floor() {
            return Some(UserAddr::new(top - total));
        }
        None
    }

    /// Allocate a virtual address range and register the region.
    pub fn alloc_region(
        &mut self,
        size: u64,
        kind: RegionKind,
        writable: bool,
    ) -> Option<UserAddr> {
        let aligned = align_up_2m(size);
        let addr = self.find_gap(aligned)?;
        self.regions.insert(
            addr,
            Region {
                size: aligned,
                writable,
                kind,
            },
        );
        Some(addr)
    }

    /// Allocate a region and map physical memory into it.
    pub fn alloc_and_map(
        &mut self,
        phys: u64,
        size: u64,
        writable: bool,
        cache: CachePolicy,
    ) -> Option<(UserAddr, u64)> {
        let aligned = align_up_2m(size);
        assert!(
            phys & (PAGE_2M - 1) == 0,
            "alloc_and_map: phys {phys:#x} not 2MB-aligned"
        );
        let addr = self.find_gap(aligned)?;
        self.regions.insert(
            addr,
            Region {
                size: aligned,
                writable,
                kind: RegionKind::Mapped,
            },
        );
        self.map_range(addr, phys, aligned, writable, cache);
        Some((addr, aligned))
    }

    /// Free a previously allocated region and unmap it.
    pub fn free_and_unmap(&mut self, addr: UserAddr) -> Option<u64> {
        let size = self.regions.remove(&addr)?.size;
        self.unmap_range(addr, size);
        Some(size)
    }

    /// Insert a region at a specific address (for ELF segments, stack, etc.)
    pub fn insert_region(&mut self, addr: UserAddr, region: Region) {
        assert!(
            self.find_region(addr).is_none(),
            "insert_region: address {:#x} already occupied",
            addr.raw()
        );
        self.regions.insert(addr, region);
    }

    /// Find the region containing `addr`. Returns (start_addr, region).
    pub fn find_region(&self, addr: UserAddr) -> Option<(UserAddr, &Region)> {
        let (&start, region) = self.regions.range(..=addr).next_back()?;
        if addr.raw() < start.raw() + region.size {
            Some((start, region))
        } else {
            None
        }
    }

    /// What `[addr, addr + size)` runs into, as the one question a placed
    /// mapping has to ask before it can be registered.
    ///
    /// `find_gap` answers it by construction for every allocated region, and
    /// a range that is `Free` here is one `insert_region` can take. The end is
    /// saturating so that no caller's arithmetic can wrap into a smaller range
    /// than it asked about; every real call has already bounded its own end at
    /// [`vma::ALLOC_CEILING`].
    pub fn occupancy(&self, addr: UserAddr, size: u64) -> Occupancy {
        let end = UserAddr::new(addr.raw().saturating_add(size));
        let mut over = self.overlapping_regions(addr, end);
        let Some((&start, region)) = over.next() else {
            return Occupancy::Free;
        };
        if over.next().is_none() && start == addr && region.size == size {
            Occupancy::Whole
        } else {
            Occupancy::Partial
        }
    }

    /// Iterate all regions that overlap the range [start, end).
    pub fn overlapping_regions(
        &self,
        start: UserAddr,
        end: UserAddr,
    ) -> impl Iterator<Item = (&UserAddr, &Region)> {
        // A region starting at s with size n overlaps [start, end) iff s < end && s+n > start.
        // Use range(..end) to skip regions starting at or after end, then filter the lower bound.
        self.regions
            .range(..end)
            .filter(move |(&s, r)| s.raw() + r.size > start.raw())
    }

    /// Map a physical region into the direct map using 2MB pages.
    /// Returns an Mmio handle for bounds-checked register access.
    ///
    /// Private, because the mapping is not usable until every CPU has been told
    /// about it and this method holds the lock that stops it saying so — the
    /// free function [`map_mmio`] is the whole operation.
    fn map_mmio(&mut self, phys: u64, size: u64, cache: CachePolicy) -> super::Mmio {
        let start = phys & !(PAGE_2M - 1);
        let end = (phys + size + PAGE_2M - 1) & !(PAGE_2M - 1);
        let mut cur = start;
        while cur < end {
            self.map_2m(cur, PAGE_PRESENT | PAGE_WRITE | cache.pde_bits());
            cur += PAGE_2M;
        }
        super::Mmio::new(super::DirectMap::from_phys(phys), size)
    }

    /// The policy the 2 MiB entry covering `virt` carries, or `None` where
    /// nothing is mapped.
    ///
    /// Read out of the page table rather than remembered, so a caller that
    /// reports a mapping's memory type reports the installed one.
    fn policy_at(&self, virt: u64) -> Option<CachePolicy> {
        let (pml4_idx, pdpt_idx, pd_idx) = indices(virt);
        let pde = self.root.child(pml4_idx)?.child(pdpt_idx)?[pd_idx];
        (pde & PAGE_PRESENT != 0).then(|| CachePolicy::from_pde(pde))
    }

    pub fn direct_map_policy(&self, phys: u64) -> Option<CachePolicy> {
        self.policy_at(super::DirectMap::from_phys(phys).as_ptr::<u8>() as u64)
    }

    pub fn user_policy(&self, addr: UserAddr) -> Option<CachePolicy> {
        self.policy_at(addr.raw())
    }

    /// Take one 4 KiB page of the direct map away, splitting the 2 MiB leaf
    /// that covers it into 4 KiB entries first.
    ///
    /// The direct map is the kernel's only view of physical memory, so this is
    /// the only way to make a kernel address fault. It exists for stack guard
    /// pages: a kernel stack that runs off its bottom otherwise writes into
    /// whatever the allocator put underneath and the damage surfaces
    /// somewhere else entirely.
    ///
    /// The split is permanent — the replacement entries are never coalesced
    /// back — so the caller must own `phys` for the life of the machine.
    /// Handing the enclosing 2 MiB page back to the PMM would reissue memory
    /// with a hole in its direct map.
    pub fn guard_4k(&mut self, phys: u64) {
        assert!(phys & 0xFFF == 0, "guard_4k: phys {phys:#x} not 4 KiB-aligned");
        let virt = super::DirectMap::from_phys(phys).as_ptr::<u8>() as u64;
        let (pml4_idx, pdpt_idx, pd_idx) = indices(virt);
        let pd_phys = {
            let pdpt = self.root.child(pml4_idx).expect("guard_4k: no PDPT over the direct map");
            let entry = pdpt[pdpt_idx];
            assert!(entry & PAGE_PRESENT != 0, "guard_4k: no PD over the direct map");
            entry & ADDR_MASK
        };
        let pd = unsafe { PageTablePage::from_phys_mut(pd_phys) };
        let pde = pd[pd_idx];
        assert!(pde & PAGE_PRESENT != 0, "guard_4k: {phys:#x} is not in the direct map");

        // Already split by an earlier guard in the same 2 MiB region — which
        // is the ordinary case on a wide machine, where several CPUs' idle
        // stacks come out of one heap segment.
        if pde & PAGE_SIZE_BIT != 0 {
            let base = pde & ADDR_MASK_2M;
            let flags = pde & !ADDR_MASK_2M & !PAGE_SIZE_BIT;
            assert!(
                flags & PAGE_PAT_2M == 0,
                "guard_4k: {phys:#x} carries a PAT bit that is an address bit in a 4 KiB PTE"
            );
            let mut pt = Box::new(PageTablePage([0; 512]));
            for i in 0..512 {
                pt.init_entry(i, (base + i as u64 * 4096) | flags);
            }
            let pt_phys = pt.phys();
            self.children.push(pt);
            // Both writes are covered by the flush below, which is this CPU's
            // whole TLB and so wider than either could owe.
            pd.write(pd_idx, virt, pt_phys | PAGE_PRESENT | PAGE_WRITE)
                .subsumed_by_flush();
        }

        let pt = unsafe { PageTablePage::from_phys_mut(pd[pd_idx] & ADDR_MASK) };
        let idx = ((phys >> 12) & 0x1FF) as usize;
        assert!(pt[idx] & PAGE_PRESENT != 0, "guard_4k: {phys:#x} is already unmapped");
        pt.write(idx, virt, 0).subsumed_by_flush();

        // This CPU only, and that is sufficient rather than lucky: the page is
        // one CPU's guard, `alloc_idle_stack` runs on the BSP for every CPU,
        // and an AP's TLB is empty until it starts. The 511 pages around the
        // hole keep the mapping they had, so a sibling's stale 2 MiB entry
        // stays correct for all of them.
        flush_tlb_all();
    }

    /// Map one 2 MiB page of the direct map, replacing whatever was there.
    ///
    /// Replacing and not skipping, because the boot map blankets every physical
    /// address the memory map reaches and an MMIO window inside that span is
    /// already mapped by the time its driver asks for it. Only the boot map's
    /// own 2 MiB leaf may be overwritten that way. Two windows in one page
    /// asking for different memory types would be decided by call order, and
    /// the loser would write through a type it did not ask for; an entry
    /// `guard_4k` has split into a page table would lose the guard and leak the
    /// table.
    fn map_2m(&mut self, phys: u64, flags: u64) {
        let virt = super::DirectMap::from_phys(phys).as_ptr::<u8>() as u64;
        let pd_idx = indices(virt).2;
        let pd = self.ensure_table(virt, flags, flags);
        let entry = phys | flags | PAGE_SIZE_BIT;
        let existing = pd[pd_idx];
        assert!(
            existing & PAGE_PRESENT == 0
                || existing & !(PAGE_ACCESSED | PAGE_DIRTY) == entry
                || (existing & PAGE_SIZE_BIT != 0
                    && CachePolicy::from_pde(existing) == CachePolicy::DeferToMtrr),
            "map_2m: {phys:#x} is mapped {existing:#x} and cannot also be {entry:#x}"
        );
        // Neither caller wants the single address this could owe. [`map_mmio`]
        // flushes every CPU because the memory type may have changed under a
        // sibling, which the entry cannot tell it; `init` runs before any TLB
        // exists and ends by loading `CR3` with a flush.
        pd.write(pd_idx, virt, entry).subsumed_by_flush();
    }

    /// Everything an upper-level entry may carry besides the address of the
    /// table below it. A leaf's remaining flags are address bits here — bit 12
    /// of a PML4E or PDPTE is part of the next table's physical address, not
    /// the PAT bit it is in a 2 MiB PDE — so both branches below mask, and a
    /// caller passing leaf flags cannot move a page table 4 KiB.
    fn ensure_table(&mut self, va: u64, pml4_flags: u64, pdpt_flags: u64) -> &mut PageTablePage {
        const TABLE_FLAGS: u64 = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
        let (pml4_idx, pdpt_idx, _) = indices(va);
        let target = self.cr3();

        if self.root[pml4_idx] & PAGE_PRESENT == 0 {
            let child = Box::new(PageTablePage([0; 512]));
            self.root
                .write(pml4_idx, va, child.phys() | (pml4_flags & TABLE_FLAGS))
                .expect_install("ensure_table: pml4");
            self.children.push(child);
        } else {
            // x86-64: upper-level entries must be at least as permissive as any
            // leaf entry below them. Widen only (OR), never narrow.
            self.root.widen(pml4_idx, pml4_flags & TABLE_FLAGS).discharge(target);
        }

        let pdpt = unsafe { PageTablePage::from_phys_mut(self.root[pml4_idx] & ADDR_MASK) };

        if pdpt[pdpt_idx] & PAGE_PRESENT == 0 {
            let child = Box::new(PageTablePage([0; 512]));
            pdpt.write(pdpt_idx, va, child.phys() | (pdpt_flags & TABLE_FLAGS))
                .expect_install("ensure_table: pdpt");
            self.children.push(child);
        } else {
            pdpt.widen(pdpt_idx, pdpt_flags & TABLE_FLAGS).discharge(target);
        }

        unsafe { PageTablePage::from_phys_mut(pdpt[pdpt_idx] & ADDR_MASK) }
    }
}

const MIN_PHYS_MAP: u64 = 4 * 1024 * 1024 * 1024;

/// The kernel address space, in the same shape a process's is.
///
/// **An `Arc<Lock<AddressSpace>>` and not a `Lock<Option<AddressSpace>>`,
/// because a task has to be able to *name* it.** A kernel thread runs here —
/// `driver::spawn` gives it the `cr3` every CPU is already in between two user
/// threads — and while it could not name the same thing a process names,
/// `KernelPayload.address_space` had to stay an `Option` with a fallback branch
/// deciding which `cr3` a task gets. It does not now: the field is *the address
/// space this task runs in*, for every task, with no second answer.
///
/// Published once at boot from a leaked `Arc` and read with one acquire load —
/// `log::console`'s `KLOGD` has the same shape for the same reason. Leaked
/// deliberately: the kernel address space outlives every task by construction,
/// so no task's payload may hold the last reference to it.
static KERNEL: core::sync::atomic::AtomicPtr<alloc::sync::Arc<Lock<AddressSpace>>> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Kernel CR3, cached for lock-free access from panic/crash paths.
static KERNEL_CR3: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The kernel address space. Mapped once at boot, lives forever.
pub fn kernel() -> &'static alloc::sync::Arc<Lock<AddressSpace>> {
    let ptr = KERNEL.load(core::sync::atomic::Ordering::Acquire);
    assert!(!ptr.is_null(), "paging not initialized");
    // SAFETY: written once in `init` from a leaked `Box`, with the `Release`
    // this `Acquire` pairs with, and never cleared — so the pointer is live for
    // the rest of the machine's life.
    unsafe { &*ptr }
}

/// Kernel CR3. Lock-free — safe to call from panic context.
pub fn kernel_cr3() -> Cr3 {
    Cr3(KERNEL_CR3.load(core::sync::atomic::Ordering::Relaxed))
}

/// Map a device's registers into the kernel's direct map and tell every CPU.
///
/// The lock and the shootdown are separate statements, and that is the whole
/// reason this is a free function rather than a method: the shootdown waits for
/// siblings and may hold nothing, while the mapping needs the address space.
/// Eleven callers used to write the lock-and-map incantation themselves, and
/// none of them could have got the second half right.
///
/// **The shootdown is not bookkeeping on this path.** `map_2m` is allowed to
/// replace the boot map's own leaf, so a window inside a range the memory map
/// covers changes memory type here — write-combining for the framebuffer, most
/// of all. A sibling holding the old write-back entry for the same physical page
/// is SDM Vol. 3A §11.12.4 undefined behaviour, and hanging is a permitted
/// outcome.
pub fn map_mmio(phys: u64, size: u64, cache: CachePolicy) -> super::Mmio {
    let mmio = kernel().lock().map_mmio(phys, size, cache);
    crate::arch::tlb::shootdown();
    mmio
}


/// Take the 4 KiB page holding `addr` out of the kernel's direct map.
///
/// `addr` must be a direct-map address whose page the caller owns forever —
/// see [`AddressSpace::guard_4k`].
pub fn guard_kernel_page(addr: u64) {
    assert!(super::is_kernel_addr(addr), "guard_kernel_page: {addr:#x} is not a kernel address");
    kernel().lock().guard_4k(super::DirectMap::phys_of(addr as *const u8));
}

/// Build kernel page tables: map all physical memory in the high half using 2MB large pages.
pub(super) fn init(memory_map: &[MemoryMapEntry]) {
    let mut max_addr: u64 = MIN_PHYS_MAP;
    for entry in memory_map {
        if entry.end > max_addr {
            max_addr = entry.end;
        }
    }
    max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let mut kernel = AddressSpace {
        root: Box::new(PageTablePage([0; 512])),
        children: Vec::new(),
        pages: HashMap::new(),
        regions: BTreeMap::new(),
        pcid: 0, // Kernel always uses PCID 0
    };

    let mut addr: u64 = 0;
    while addr < max_addr {
        kernel.map_2m(addr, PAGE_PRESENT | PAGE_WRITE);
        addr += PAGE_2M;
    }

    let cr3 = kernel.cr3();
    KERNEL_CR3.store(cr3.0, core::sync::atomic::Ordering::Release);
    // Leaked, and `Release` after the space is built: see [`KERNEL`].
    let published: &'static alloc::sync::Arc<Lock<AddressSpace>> = Box::leak(Box::new(
        alloc::sync::Arc::new(Lock::new(kernel)),
    ));
    KERNEL.store(
        published as *const _ as *mut _,
        core::sync::atomic::Ordering::Release,
    );
    // Boot path: load CR3 with flush (PCID not yet enabled).
    unsafe {
        cr3.load_flush();
    }
}

fn has(entry: u64, flag: u64) -> u8 {
    if entry & flag != 0 {
        1
    } else {
        0
    }
}

/// Whether `addr` resolves to a present page in the *currently loaded* CR3.
/// Lock-free, allocation-free, and silent, for the panic path to prove a
/// mapping still exists before writing through it.
///
/// The current CR3 and not `kernel_cr3()`: a panic in syscall context runs on
/// a user address space, which copies the kernel's top-half PML4 entries at
/// creation. Shared page directories propagate later `map_2m`s, but a crash
/// handler should prove that rather than assume it.
pub fn present_in_current_cr3(addr: u64) -> bool {
    let mut table = unsafe { PageTablePage::from_phys(Cr3::current().phys()) };
    for level in 0..3 {
        let entry = table[((addr >> (39 - level * 9)) & 0x1FF) as usize];
        if entry & PAGE_PRESENT == 0 {
            return false;
        }
        if level > 0 && entry & PAGE_SIZE_BIT != 0 {
            return true;
        }
        table = unsafe { PageTablePage::from_phys(entry & ADDR_MASK) };
    }
    table[((addr >> 12) & 0x1FF) as usize] & PAGE_PRESENT != 0
}

/// Dump page table entries for an address. Lock-free for crash safety.
pub fn debug_page_walk(addr: u64) {
    let cr3 = Cr3::current();
    let pml4 = unsafe { PageTablePage::from_phys(cr3.phys()) };
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((addr >> 21) & 0x1FF) as usize;
    let pt_idx = ((addr >> 12) & 0x1FF) as usize;

    log!(
        "  Page walk for {:#x} [PML4={:#x} PCID={} PML4[{}] PDPT[{}] PD[{}] PT[{}]]:",
        addr,
        cr3.phys(),
        cr3.pcid(),
        pml4_idx,
        pdpt_idx,
        pd_idx,
        pt_idx
    );

    let pml4e = pml4[pml4_idx];
    log!(
        "    PML4E: {:#018x} P={} W={} U={}",
        pml4e,
        has(pml4e, PAGE_PRESENT),
        has(pml4e, PAGE_WRITE),
        has(pml4e, PAGE_USER)
    );
    if pml4e & PAGE_PRESENT == 0 {
        return;
    }

    let pdpt = unsafe { PageTablePage::from_phys(pml4e & ADDR_MASK) };
    let pdpte = pdpt[pdpt_idx];
    log!(
        "    PDPTE: {:#018x} P={} W={} U={}",
        pdpte,
        has(pdpte, PAGE_PRESENT),
        has(pdpte, PAGE_WRITE),
        has(pdpte, PAGE_USER)
    );
    if pdpte & PAGE_PRESENT == 0 {
        return;
    }

    let pd = unsafe { PageTablePage::from_phys(pdpte & ADDR_MASK) };
    let pde = pd[pd_idx];
    log!(
        "    PDE:   {:#018x} P={} W={} U={} PS={}",
        pde,
        has(pde, PAGE_PRESENT),
        has(pde, PAGE_WRITE),
        has(pde, PAGE_USER),
        has(pde, PAGE_SIZE_BIT)
    );
    if pde & PAGE_PRESENT == 0 {
        return;
    }
    if pde & PAGE_SIZE_BIT != 0 {
        log!("    -> 2MB large page at {:#x}", pde & ADDR_MASK_2M);
        return;
    }

    let pt = unsafe { PageTablePage::from_phys(pde & ADDR_MASK) };
    let pte = pt[pt_idx];
    log!(
        "    PTE:   {:#018x} P={} W={} U={}",
        pte,
        has(pte, PAGE_PRESENT),
        has(pte, PAGE_WRITE),
        has(pte, PAGE_USER)
    );
    if pte & PAGE_PRESENT == 0 {
        return;
    }
    log!("    -> 4KB page at {:#x}", pte & ADDR_MASK);
}
