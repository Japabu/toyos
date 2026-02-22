use core::ptr::{read_volatile, write_volatile};

/// MMIO base address wrapper for typed hardware register access.
#[derive(Clone, Copy)]
pub struct Mmio(u64);

impl Mmio {
    pub const fn new(base: u64) -> Self {
        Self(base)
    }

    pub fn addr(self) -> u64 {
        self.0
    }

    pub fn offset(self, off: u64) -> Mmio {
        Mmio(self.0 + off)
    }

    #[inline]
    pub fn read_u8(self, offset: u64) -> u8 {
        unsafe { read_volatile((self.0 + offset) as *const u8) }
    }

    #[inline]
    pub fn read_u32(self, offset: u64) -> u32 {
        unsafe { read_volatile((self.0 + offset) as *const u32) }
    }

    #[inline]
    pub fn write_u32(self, offset: u64, val: u32) {
        unsafe { write_volatile((self.0 + offset) as *mut u32, val) }
    }

    #[inline]
    pub fn read_u64(self, offset: u64) -> u64 {
        unsafe { read_volatile((self.0 + offset) as *const u64) }
    }

    #[inline]
    pub fn write_u64(self, offset: u64, val: u64) {
        unsafe { write_volatile((self.0 + offset) as *mut u64, val) }
    }
}
