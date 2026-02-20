use crate::sys::syscall;

const SYS_RANDOM: u64 = 6;

pub fn fill_bytes(buf: &mut [u8]) {
    syscall(SYS_RANDOM, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
}
