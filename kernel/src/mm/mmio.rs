use core::ptr::{read_volatile, write_volatile};

use super::DirectMap;

/// Bounds-checked volatile window. Copy, no ownership, no lifetime.
///
/// Named for what it was built for and what nearly every value of it still is:
/// a device's register window, created by `paging::map_mmio`, which is also
/// what makes it readable on every CPU and not just the one that mapped it.
/// [`Mmio::over_phys`] is the other constructor, for memory this kernel owns
/// that a *device* also reads — same bound, same volatility, no mapping to do.
#[derive(Clone, Copy)]
pub struct Mmio {
    base: *mut u8,
    size: u64,
}

// SAFETY: a window is at a fixed address for the life of the machine, whether
// it is a register window `map_mmio` mapped or the direct map's view of pages
// the kernel never releases (`over_phys`), and it is not tied to any thread —
// `Mmio` is `Copy` and carries no lock, so `Send` costs nothing new.
unsafe impl Send for Mmio {}
// SAFETY: same fixed-address reasoning as `Send`. Every access goes through
// `read_volatile`/`write_volatile` below, which forces the ordering the
// hardware needs regardless of which CPU issues it — a shared `&Mmio` read
// concurrently from several CPUs is exactly what an MMIO window is for.
unsafe impl Sync for Mmio {}

impl Mmio {
    pub(super) fn new(base: DirectMap, size: u64) -> Self {
        Self { base: base.as_mut_ptr(), size }
    }

    /// The same bounded volatile window, over physical memory *this kernel*
    /// owns rather than over a device's registers.
    ///
    /// For the two readers that cannot be handed the value `paging::map_mmio`
    /// returned. The IOMMU's DMA-fault handler may take no lock, so what it
    /// keeps between the boot that programmed a unit and the interrupt that
    /// reports on one is an `AtomicU64` — a physical address, from which this
    /// rebuilds the window. The IOMMU's remapping tables are the other:
    /// ordinary PMM pages that the *unit* also walks, so every access to one
    /// is volatile for the same reason a register access is, and wants the
    /// same bound.
    ///
    /// The alternative at both sites is a raw pointer whose bound is the offset
    /// alone; this is the same window with the length in it.
    ///
    /// # Safety
    /// `base` must name `size` bytes of physical memory this kernel owns for
    /// the life of the machine — a window `map_mmio` mapped, or PMM pages that
    /// are never released — and reading and writing them volatilely must be
    /// what the caller means, because that is all this type does.
    pub unsafe fn over_phys(base: DirectMap, size: u64) -> Self {
        Self::new(base, size)
    }

    /// The window's base as an integer.
    ///
    /// For a caller that has to hand the address itself somewhere this type
    /// cannot go — a `clflush` operand, or a line of a log.
    pub fn addr(self) -> u64 {
        self.base as u64
    }

    pub fn subregion(self, offset: u64, size: u64) -> Mmio {
        assert!(offset + size <= self.size,
            "Mmio subregion OOB: offset={:#x} size={:#x} total={:#x}", offset, size, self.size);
        Mmio {
            // SAFETY: `offset + size <= self.size` was just asserted above, so
            // the returned pointer and every byte up to `size` past it stay
            // inside the single window `paging::map_mmio` mapped for `self`
            // — `add` never leaves that region.
            base: unsafe { self.base.add(offset as usize) },
            size,
        }
    }

    fn check(&self, offset: u64, len: u64) {
        assert!(offset + len <= self.size,
            "Mmio OOB: offset={:#x} len={} size={:#x}", offset, len, self.size);
    }

    #[inline]
    pub fn read_u8(self, offset: u64) -> u8 {
        self.check(offset, 1);
        // SAFETY: `check` just asserted `offset + 1 <= self.size`, so this
        // stays inside the window `map_mmio` mapped for `self`;
        // `read_volatile` (not a plain deref) is required because the
        // register behind it can have a read side effect the compiler must
        // not elide, reorder, or merge with a neighboring access.
        unsafe { read_volatile(self.base.add(offset as usize) as *const u8) }
    }

    #[inline]
    pub fn write_u8(self, offset: u64, val: u8) {
        self.check(offset, 1);
        // SAFETY: `check` just asserted `offset + 1 <= self.size`, so this
        // stays inside the window `map_mmio` mapped for `self`;
        // `write_volatile` is required so the store the register may act on
        // cannot be elided, reordered past another MMIO access, or merged.
        unsafe { write_volatile(self.base.add(offset as usize), val) }
    }

    #[inline]
    pub fn read_u16(self, offset: u64) -> u16 {
        self.check(offset, 2);
        // SAFETY: `check` just asserted `offset + 2 <= self.size`, so this
        // stays inside the window `map_mmio` mapped for `self`; volatile for
        // the same read-side-effect reason as `read_u8`.
        unsafe { read_volatile(self.base.add(offset as usize) as *const u16) }
    }

    #[inline]
    pub fn write_u16(self, offset: u64, val: u16) {
        self.check(offset, 2);
        // SAFETY: `check` just asserted `offset + 2 <= self.size`, so this
        // stays inside the window `map_mmio` mapped for `self`; volatile for
        // the same ordering reason as `write_u8`.
        unsafe { write_volatile(self.base.add(offset as usize) as *mut u16, val) }
    }

    #[inline]
    pub fn read_u32(self, offset: u64) -> u32 {
        self.check(offset, 4);
        // SAFETY: `check` just asserted `offset + 4 <= self.size`, so this
        // stays inside the window `map_mmio` mapped for `self`; volatile for
        // the same read-side-effect reason as `read_u8`.
        unsafe { read_volatile(self.base.add(offset as usize) as *const u32) }
    }

    #[inline]
    pub fn write_u32(self, offset: u64, val: u32) {
        self.check(offset, 4);
        // SAFETY: `check` just asserted `offset + 4 <= self.size`, so this
        // stays inside the window `map_mmio` mapped for `self`; volatile for
        // the same ordering reason as `write_u8`.
        unsafe { write_volatile(self.base.add(offset as usize) as *mut u32, val) }
    }

    #[inline]
    pub fn read_u64(self, offset: u64) -> u64 {
        self.check(offset, 8);
        // SAFETY: `check` just asserted `offset + 8 <= self.size`, so this
        // stays inside the window `map_mmio` mapped for `self`; volatile for
        // the same read-side-effect reason as `read_u8`.
        unsafe { read_volatile(self.base.add(offset as usize) as *const u64) }
    }

    #[inline]
    pub fn write_u64(self, offset: u64, val: u64) {
        self.check(offset, 8);
        // SAFETY: `check` just asserted `offset + 8 <= self.size`, so this
        // stays inside the window `map_mmio` mapped for `self`; volatile for
        // the same ordering reason as `write_u8`.
        unsafe { write_volatile(self.base.add(offset as usize) as *mut u64, val) }
    }
}
