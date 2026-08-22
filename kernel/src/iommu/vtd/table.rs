//! The tables the unit walks: a root table, a context table per bus that
//! carries a device, and the second-level page tables of the one domain every
//! kernel-owned device is put in.
//!
//! A *passthrough* domain for those devices was the first design, and the
//! largest de-risking decision that was available. **`ECAP.PT` is clear on the
//! only unit anyone here can boot** — measured — so that decision is not
//! available and the fallback, an identity-mapped translated domain, is the
//! only path. This module therefore never writes a passthrough context entry,
//! even on a unit that offers one: that arm would be the one no machine in
//! reach executes.
//!
//! Every write here goes out of the cache before it returns, unconditionally.
//! `ECAP.C` — page-table walks snoop the CPU cache — is **clear** on QEMU, the
//! opposite of what the design first assumed, so on the machine this suite runs
//! an entry left in a dirty line is an entry the unit does not see.

use alloc::vec::Vec;

use crate::iommu::{AddressWidth, Iova, StreamId};
use crate::mm::pmm::{self, Category, PhysPage};
use crate::mm::{DirectMap, Mmio, PAGE_2M};

/// A remapping table is 4 KiB whatever it holds: 256 root or context entries
/// of 16 bytes, or 512 second-level entries of 8.
const TABLE_BYTES: usize = 4096;

/// The step a table's flush walks in. x86-64 reports its `clflush` line size
/// in `CPUID.01H:EBX[15:8]`, and every part reports 64; a part that reported
/// more would still have every line covered, because each `clflush` takes the
/// whole line containing the address. Stepping by the smallest line any part
/// has can over-flush and cannot under-flush.
const LINE_BYTES: usize = 64;

const SL_READ: u64 = 1 << 0;
const SL_WRITE: u64 = 1 << 1;
/// Bit 7 at a page-directory level: this entry is a 2 MiB leaf rather than a
/// pointer to the level below. §5.4 — the kernel is 2 MiB-page-only, so this
/// is the only leaf size, and `CAP.SPS` bit 0 is what a unit must advertise
/// for it.
const SL_LARGE: u64 = 1 << 7;

/// Root, context and second-level entries all carry their pointer in the same
/// field, bounded by x86-64's 52-bit physical ceiling.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const PRESENT: u64 = 1 << 0;

/// The one domain every kernel-owned device is put in.
///
/// Not 0: that is what an all-zero context entry names, so using it would make
/// "the entry this kernel wrote" and "the entry it did not" indistinguishable
/// in a fault record and in a domain-selective invalidation.
pub const KERNEL_DOMAIN: u16 = 1;

/// What the identity domain's leaves grant. Both, because the domain exists to
/// leave every kernel driver reaching exactly what it reaches today.
const LEAF_PERM: u64 = SL_READ | SL_WRITE;

/// 4 KiB tables carved out of 2 MiB PMM pages.
///
/// The PMM's granularity is 2 MiB and a table is 4 KiB, so a page per table
/// would waste 511 of every 512. The pages are never handed back: these tables
/// live as long as the machine does, and a stage that does release one has to
/// invalidate before it (§5.5) — an ordering `Unmapped`/`Flushed` expresses at
/// I4 and this allocator cannot.
pub struct Tables {
    pages: Vec<PhysPage>,
    /// Bytes taken from the newest page. Starts full, so the first call takes
    /// a page rather than needing an `Option` to say there is none yet.
    used: usize,
}

impl Tables {
    pub const fn new() -> Self {
        Self { pages: Vec::new(), used: PAGE_2M as usize }
    }

    /// One zeroed 4 KiB table, whatever it is about to be used as. The unit
    /// reads root tables, context tables, second-level tables and invalidation
    /// queues out of exactly the same shape of memory.
    pub fn alloc(&mut self) -> Table {
        if self.used + TABLE_BYTES > PAGE_2M as usize {
            let page = pmm::alloc_page(Category::Dma)
                .expect("iommu: no physical memory for a remapping table");
            self.pages.push(page);
            self.used = 0;
        }
        let base = self.pages.last().expect("iommu: table page just pushed").direct_map().phys();
        let table = Table { phys: base + self.used as u64 };
        self.used += TABLE_BYTES;
        // `alloc_page` zeroed the page through the direct map, so the zeroes
        // this table is about to be read as are still in the cache of a unit
        // that does not snoop it.
        table.flush_all();
        table
    }
}

/// One 4 KiB remapping table, named by its physical address because that is
/// the only form the unit ever sees it in.
#[derive(Clone, Copy)]
pub struct Table {
    phys: u64,
}

impl Table {
    pub fn phys(self) -> u64 {
        self.phys
    }

    /// The table's 4 KiB, as a window that bounds every access in it.
    ///
    /// **The whole table and not a slot**, which is the difference between a
    /// bound on the offset and a bound on the offset *and* the length: an
    /// index, a byte offset and a 32-bit field are all checked against the
    /// same 4 KiB here, rather than each against an `assert!` of its own that
    /// the next accessor could be written without.
    fn window(self) -> Mmio {
        // SAFETY: `self.phys` is 4 KiB out of a 2 MiB `Category::Dma` page
        // `Tables::alloc` took and never hands back (its doc comment is why),
        // or that same address read back out of a table entry this module
        // wrote — the unit only ever *reads* these tables, so an entry is this
        // kernel's own store and not a device's. Either way the direct map
        // covers it for the life of the machine. Volatile is what the access
        // has to be, which is the other half of `over_phys`'s contract: the
        // unit walks this memory without snooping the CPU cache (`ECAP.C` is
        // clear on the only unit anyone here can boot — the module header), so
        // a store the compiler merged or elided is an entry the unit never
        // sees.
        unsafe { Mmio::over_phys(DirectMap::from_phys(self.phys), TABLE_BYTES as u64) }
    }

    /// The eight bytes slot `index` occupies, bounded before anything touches
    /// them — including the `clflush`, whose operand is an address and carries
    /// no length of its own.
    fn slot(self, index: usize) -> Mmio {
        self.window().subregion(index as u64 * 8, 8)
    }

    /// Write one 64-bit slot and push it out of the cache.
    ///
    /// The two are inseparable on purpose. A `write` that could be called
    /// without its flush is the `ECAP.C = 0` corruption §5.2 exists to prevent,
    /// left to review instead of to the compiler.
    fn write(self, index: usize, value: u64) {
        let slot = self.slot(index);
        slot.write_u64(0, value);
        flush(slot.addr() as usize);
    }

    fn read(self, index: usize) -> u64 {
        self.slot(index).read_u64(0)
    }

    /// A 16-byte entry — root, context, or invalidation descriptor — low half
    /// last: it is the half carrying the bits that make the entry live, so
    /// until it lands the unit sees something it will not act on.
    pub fn write_pair(self, index: usize, lo: u64, hi: u64) {
        self.write(index * 2 + 1, hi);
        self.write(index * 2, lo);
    }

    fn flush_all(self) {
        let base = self.window().addr() as usize;
        for offset in (0..TABLE_BYTES).step_by(LINE_BYTES) {
            flush(base + offset);
        }
    }

    /// Write a 32-bit field the *unit* will read — the invalidation queue's
    /// completion status, before it is armed.
    pub fn write_u32(self, byte_offset: usize, value: u32) {
        let field = self.window().subregion(byte_offset as u64, 4);
        field.write_u32(0, value);
        flush(field.addr() as usize);
    }

    /// Read a 32-bit field the unit wrote, from memory rather than from a
    /// cache that never saw it happen.
    ///
    /// The mirror of every write here: a unit whose walks do not snoop the CPU
    /// cache is a unit whose stores are not guaranteed to land in it either, so
    /// the line is dropped before the read instead of trusted.
    pub fn read_device_u32(self, byte_offset: usize) -> u32 {
        let field = self.window().subregion(byte_offset as u64, 4);
        flush(field.addr() as usize);
        field.read_u32(0)
    }
}

/// Push one cache line out, and order it against what follows.
///
/// `clflush` rather than `clflushopt`: the latter needs its own CPUID bit, and
/// the machines this runs on include QEMU's `qemu64`, which does not have one.
/// The fence is what makes the flush globally visible before the MMIO write
/// that tells the unit to look.
fn flush(addr: usize) {
    // SAFETY: irreducible — there is no safe spelling of `clflush`, and no
    // narrower one either: the instruction takes an address and no length, so
    // the bound has to be somewhere else. It is: every caller above takes its
    // operand from an [`Mmio`] subregion already checked against the table's
    // 4 KiB, and `flush_all` walks a range it derives from `TABLE_BYTES`.
    // Neither instruction reads or writes memory the compiler can see —
    // `clflush` moves a line the CPU already holds and `mfence` orders what is
    // in flight — so a line outside the table would cost a flush and change
    // nothing, which is why `nostack, preserves_flags` is the whole option set.
    unsafe {
        core::arch::asm!(
            "clflush [{addr}]",
            "mfence",
            addr = in(reg) addr,
            options(nostack, preserves_flags),
        );
    }
}

/// How many levels of second-level page table a width takes. The context
/// entry's `AW` field is this less two, which is the only place the two
/// encodings meet.
fn levels(width: AddressWidth) -> u8 {
    match width {
        AddressWidth::Bits39 => 3,
        AddressWidth::Bits48 => 4,
    }
}

/// Build the identity domain's second-level tables over `[0, top)` and return
/// the root and how many leaves it took.
///
/// One rule, and it is what makes this stage behaviour-unchanged by
/// construction: every address a kernel driver can hand a
/// device today is an address that device could already reach with no unit on
/// the machine, and `[0, top)` is that set. It is not isolation and does not
/// pretend to be — isolation arrives with per-driver domains, where an IOVA is
/// *allocated* out of a space that starts above the top of RAM rather than
/// inherited from a physical address.
///
/// `top` comes from the memory manager rather than from the firmware map: the
/// map's own buffer is ordinary free RAM by the time this runs, so reading it
/// here would be reading whatever the allocator has since put there.
pub fn identity_domain(tables: &mut Tables, width: AddressWidth, top: u64) -> (Table, u64) {
    let levels = levels(width);
    let root = tables.alloc();
    let mut frames = 0u64;
    let mut phys = 0u64;
    while phys < top {
        map_2m(tables, root, levels, Iova::identity(phys), phys);
        phys += PAGE_2M;
        frames += 1;
    }
    (root, frames)
}

fn map_2m(tables: &mut Tables, root: Table, levels: u8, at: Iova, phys: u64) {
    let mut table = root;
    let mut level = levels;
    while level > 2 {
        let index = ((at.raw() >> (12 + 9 * (level as u64 - 1))) & 0x1FF) as usize;
        let entry = table.read(index);
        table = if entry & (SL_READ | SL_WRITE) != 0 {
            Table { phys: entry & ADDR_MASK }
        } else {
            let next = tables.alloc();
            // Every level above the leaf grants both, because the unit ANDs
            // the permissions down the walk: a narrowing here would be a
            // narrowing of every mapping under it, which is not what any
            // caller means.
            table.write(index, next.phys | SL_READ | SL_WRITE);
            next
        };
        level -= 1;
    }
    let index = ((at.raw() >> 21) & 0x1FF) as usize;
    table.write(index, (phys & !(PAGE_2M - 1)) | SL_LARGE | LEAF_PERM);
}

/// Give `stream` a context entry naming the identity domain.
///
/// A function that never reaches here has no context entry, and faults on its
/// first transaction — which is §5.1's stated consequence, and what the
/// `iommu-context-absent` actuator stages deliberately.
pub fn bind(tables: &mut Tables, root: Table, stream: StreamId, domain: Table, width: AddressWidth) {
    let bus = stream.bus() as usize;
    let entry = root.read(bus * 2);
    let context = if entry & PRESENT != 0 {
        Table { phys: entry & ADDR_MASK }
    } else {
        let table = tables.alloc();
        root.write_pair(bus, table.phys | PRESENT, 0);
        table
    };
    // Translation type 00: an untranslated request is translated through the
    // second-level table this entry names. The other two encodings are
    // device-TLB (§10.1 rejects it) and passthrough (unavailable, above).
    let lo = domain.phys | PRESENT;
    let hi = ((KERNEL_DOMAIN as u64) << 8) | (levels(width) as u64 - 2);
    context.write_pair(stream.devfn() as usize, lo, hi);
}
