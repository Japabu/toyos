use core::ptr::{read_volatile, write_volatile};

use super::DirectMap;

/// Bounds-checked MMIO handle. Copy, no ownership, no lifetime.
/// Created by `paging::map_mmio`, which is also what makes the window readable
/// on every CPU and not just the one that mapped it.
#[derive(Clone, Copy)]
pub struct Mmio {
    base: *mut u8,
    size: u64,
}

// SAFETY: MMIO registers are at fixed physical addresses, not tied to any
// thread — `Mmio` is `Copy` and carries no lock, so `Send` costs nothing new.
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

    /// The window's base as an integer.
    ///
    /// For the one reader that cannot hold the handle: the IOMMU's DMA-fault
    /// interrupt handler may take no lock, so the
    /// windows it reads live in `AtomicU64`s and are dereferenced there rather
    /// than through this type. Everything that *can* hold an `Mmio` should.
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
