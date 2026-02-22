use core::ptr::{read_volatile, write_volatile};

#[inline]
pub fn read_u8(base: u64, offset: u64) -> u8 {
    unsafe { read_volatile((base + offset) as *const u8) }
}

#[inline]
pub fn read_u32(base: u64, offset: u64) -> u32 {
    unsafe { read_volatile((base + offset) as *const u32) }
}

#[inline]
pub fn write_u32(base: u64, offset: u64, val: u32) {
    unsafe { write_volatile((base + offset) as *mut u32, val) }
}

#[inline]
pub fn read_u64(base: u64, offset: u64) -> u64 {
    unsafe { read_volatile((base + offset) as *const u64) }
}

#[inline]
pub fn write_u64(base: u64, offset: u64, val: u64) {
    unsafe { write_volatile((base + offset) as *mut u64, val) }
}
