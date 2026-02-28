pub trait Disk {
    fn read(&mut self, offset: u64, buf: &mut [u8]);
    fn write(&mut self, offset: u64, buf: &[u8]);
    fn flush(&mut self);

    /// Returns the entire disk as a static byte slice, if backed by memory.
    /// Used for zero-copy file reads from ramdisks.
    fn as_static_bytes(&self) -> Option<&'static [u8]> { None }
}
