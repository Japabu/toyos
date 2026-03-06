pub fn munlock(_ptr: *const u8, _len: usize) -> Result<(), std::io::Error> {
    Ok(())
}

pub fn mlock(_ptr: *const u8, _len: usize) -> Result<(), std::io::Error> {
    Ok(())
}
