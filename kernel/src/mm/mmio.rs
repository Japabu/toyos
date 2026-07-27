use core::ptr::{read_volatile, write_volatile};

use super::DirectMap;

/// Bounds-checked MMIO handle. Copy, no ownership, no lifetime.
/// Created by `AddressSpace::map_mmio()`. Accessing after unmap causes a page fault.
#[derive(Clone, Copy)]
pub struct Mmio {
    base: *mut u8,
    size: u64,
}

// SAFETY: MMIO registers are at fixed physical addresses, not tied to any thread.
unsafe impl Send for Mmio {}
unsafe impl Sync for Mmio {}

impl Mmio {
    pub(super) fn new(base: DirectMap, size: u64) -> Self {
        Self { base: base.as_mut_ptr(), size }
    }

    pub fn subregion(self, offset: u64, size: u64) -> Mmio {
        assert!(offset + size <= self.size,
            "Mmio subregion OOB: offset={:#x} size={:#x} total={:#x}", offset, size, self.size);
        Mmio {
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
        unsafe { read_volatile(self.base.add(offset as usize) as *const u8) }
    }

    #[inline]
    pub fn read_u16(self, offset: u64) -> u16 {
        self.check(offset, 2);
        unsafe { read_volatile(self.base.add(offset as usize) as *const u16) }
    }

    #[inline]
    pub fn write_u16(self, offset: u64, val: u16) {
        self.check(offset, 2);
        unsafe { write_volatile(self.base.add(offset as usize) as *mut u16, val) }
    }

    #[inline]
    pub fn read_u32(self, offset: u64) -> u32 {
        self.check(offset, 4);
        unsafe { read_volatile(self.base.add(offset as usize) as *const u32) }
    }

    #[inline]
    pub fn write_u32(self, offset: u64, val: u32) {
        self.check(offset, 4);
        unsafe { write_volatile(self.base.add(offset as usize) as *mut u32, val) }
    }

    #[inline]
    pub fn read_u64(self, offset: u64) -> u64 {
        self.check(offset, 8);
        unsafe { read_volatile(self.base.add(offset as usize) as *const u64) }
    }

    #[inline]
    pub fn write_u64(self, offset: u64, val: u64) {
        self.check(offset, 8);
        unsafe { write_volatile(self.base.add(offset as usize) as *mut u64, val) }
    }
}
