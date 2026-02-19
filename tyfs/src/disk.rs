pub trait Disk {
    fn read(&mut self, offset: u64, buf: &mut [u8]);
    fn write(&mut self, offset: u64, buf: &[u8]);
    fn flush(&mut self);
}
