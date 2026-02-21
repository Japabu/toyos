pub fn fill_bytes(buf: &mut [u8]) {
    unsafe { crate::sys::toyos_random(buf.as_mut_ptr(), buf.len()) }
}
