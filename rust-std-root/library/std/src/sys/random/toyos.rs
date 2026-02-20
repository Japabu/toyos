unsafe extern "C" {
    fn toyos_random(buf: *mut u8, len: usize);
}

pub fn fill_bytes(buf: &mut [u8]) {
    unsafe { toyos_random(buf.as_mut_ptr(), buf.len()) }
}
