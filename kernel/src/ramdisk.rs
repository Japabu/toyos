pub struct RamDisk {
    data: &'static [u8],
}

impl RamDisk {
    /// # Safety
    /// The caller must ensure `ptr` points to a valid memory region of at least `len` bytes
    /// that remains valid for the static lifetime.
    pub unsafe fn new(ptr: *const u8, len: usize) -> Self {
        Self { data: core::slice::from_raw_parts(ptr, len) }
    }
}

impl tyfs::Disk for RamDisk {
    fn read(&mut self, offset: u64, buf: &mut [u8]) {
        let off = offset as usize;
        let len = buf.len().min(self.data.len() - off);
        buf[..len].copy_from_slice(&self.data[off..off + len]);
    }

    fn write(&mut self, _offset: u64, _buf: &[u8]) {
        panic!("ramdisk is read-only");
    }

    fn flush(&mut self) {
        panic!("ramdisk is read-only");
    }

    fn as_static_bytes(&self) -> Option<&'static [u8]> {
        Some(self.data)
    }
}
