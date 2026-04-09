//! Implementation for ToyOS using the SYS_RANDOM syscall.
use crate::Error;
use core::mem::MaybeUninit;

pub fn getrandom_inner(dest: &mut [MaybeUninit<u8>]) -> Result<(), Error> {
    let buf = unsafe { &mut *(dest as *mut [MaybeUninit<u8>] as *mut [u8]) };
    toyos_abi::syscall::random(buf);
    Ok(())
}
