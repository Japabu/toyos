use core::ptr::{read_volatile, write_volatile};

use crate::PhysAddr;

/// MMIO base address wrapper for typed hardware register access.
#[derive(Clone, Copy)]
pub struct Mmio(PhysAddr);

impl Mmio {
    pub const fn new(base: PhysAddr) -> Self {
        Self(base)
    }

    pub fn addr(self) -> PhysAddr {
        self.0
    }

    pub fn offset(self, off: u64) -> Mmio {
        Mmio(self.0 + off)
    }

    #[inline]
    pub fn read_u8(self, offset: u64) -> u8 {
        unsafe { read_volatile((self.0 + offset).as_ptr()) }
    }

    #[inline]
    pub fn read_u16(self, offset: u64) -> u16 {
        unsafe { read_volatile((self.0 + offset).as_ptr()) }
    }

    #[inline]
    pub fn write_u16(self, offset: u64, val: u16) {
        unsafe { write_volatile((self.0 + offset).as_mut_ptr(), val) }
    }

    #[inline]
    pub fn read_u32(self, offset: u64) -> u32 {
        unsafe { read_volatile((self.0 + offset).as_ptr()) }
    }

    #[inline]
    pub fn write_u32(self, offset: u64, val: u32) {
        unsafe { write_volatile((self.0 + offset).as_mut_ptr(), val) }
    }

    #[inline]
    pub fn read_u64(self, offset: u64) -> u64 {
        unsafe { read_volatile((self.0 + offset).as_ptr()) }
    }

    #[inline]
    pub fn write_u64(self, offset: u64, val: u64) {
        unsafe { write_volatile((self.0 + offset).as_mut_ptr(), val) }
    }
}
