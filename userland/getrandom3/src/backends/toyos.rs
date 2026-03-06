//! Implementation for ToyOS using the SYS_RANDOM syscall.
use crate::Error;
use core::mem::MaybeUninit;

pub use crate::util::{inner_u32, inner_u64};

pub fn fill_inner(dest: &mut [MaybeUninit<u8>]) -> Result<(), Error> {
    let buf = unsafe { &mut *(dest as *mut [MaybeUninit<u8>] as *mut [u8]) };
    toyos_abi::syscall::random(buf);
    Ok(())
}
