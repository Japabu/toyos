pub struct RamDisk {
    ptr: *mut u8,
    len: usize,
}

impl RamDisk {
    /// # Safety
    /// The caller must ensure `ptr` points to a valid memory region of at least `len` bytes
    /// that remains valid for the lifetime of this RamDisk.
    pub unsafe fn new(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }
}

impl tyfs::Disk for RamDisk {
    fn read(&mut self, offset: u64, buf: &mut [u8]) {
        let off = offset as usize;
        let len = buf.len().min(self.len - off);
        unsafe {
            core::ptr::copy_nonoverlapping(self.ptr.add(off), buf.as_mut_ptr(), len);
        }
    }

    fn write(&mut self, offset: u64, buf: &[u8]) {
        let off = offset as usize;
        let len = buf.len().min(self.len - off);
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), self.ptr.add(off), len);
        }
    }

    fn flush(&mut self) {}
}
