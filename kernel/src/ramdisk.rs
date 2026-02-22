pub struct RamDisk {
    data: &'static mut [u8],
}

impl RamDisk {
    /// # Safety
    /// The caller must ensure `ptr` points to a valid memory region of at least `len` bytes
    /// that remains valid for the static lifetime.
    pub unsafe fn new(ptr: *mut u8, len: usize) -> Self {
        Self { data: core::slice::from_raw_parts_mut(ptr, len) }
    }
}

impl tyfs::Disk for RamDisk {
    fn read(&mut self, offset: u64, buf: &mut [u8]) {
        let off = offset as usize;
        let len = buf.len().min(self.data.len() - off);
        buf[..len].copy_from_slice(&self.data[off..off + len]);
    }

    fn write(&mut self, offset: u64, buf: &[u8]) {
        let off = offset as usize;
        let len = buf.len().min(self.data.len() - off);
        self.data[off..off + len].copy_from_slice(&buf[..len]);
    }

    fn flush(&mut self) {}
}
