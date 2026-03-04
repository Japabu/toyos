//! ToyOS implementation using the SYS_RANDOM syscall.
use crate::Error;
use core::mem::MaybeUninit;

pub use crate::util::{inner_u32, inner_u64};

#[inline]
pub fn fill_inner(dest: &mut [MaybeUninit<u8>]) -> Result<(), Error> {
    let buf = unsafe { core::slice::from_raw_parts_mut(dest.as_mut_ptr().cast::<u8>(), dest.len()) };
    toyos_abi::syscall::random(buf);
    Ok(())
}
