/// Bounds-checked view into a contiguous kernel memory region.
/// Like Mmio but for RAM — prevents out-of-bounds reads/writes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KernelSlice {
    base: *mut u8,
    size: usize,
}

unsafe impl Send for KernelSlice {}
unsafe impl Sync for KernelSlice {}

impl KernelSlice {
    /// Wrap an existing kernel pointer + size. Caller must ensure the region is valid.
    pub unsafe fn from_raw(base: *mut u8, size: usize) -> Self {
        Self { base, size }
    }

    pub fn size(&self) -> usize { self.size }
    pub fn base(&self) -> *mut u8 { self.base }

    /// Physical address of the base via the direct map.
    pub fn phys(&self) -> u64 {
        super::DirectMap::phys_of(self.base)
    }

    pub fn subslice(&self, offset: usize, size: usize) -> KernelSlice {
        assert!(offset + size <= self.size,
            "KernelSlice OOB: offset={:#x} size={:#x} total={:#x}", offset, size, self.size);
        KernelSlice {
            base: unsafe { self.base.add(offset) },
            size,
        }
    }

    fn check(&self, offset: usize, len: usize) {
        assert!(offset + len <= self.size,
            "KernelSlice OOB: offset={:#x} len={} size={:#x}", offset, len, self.size);
    }

    pub unsafe fn read<T: Copy>(&self, offset: usize) -> T {
        self.check(offset, core::mem::size_of::<T>());
        core::ptr::read_unaligned(self.base.add(offset) as *const T)
    }

    pub unsafe fn write<T: Copy>(&self, offset: usize, value: T) {
        self.check(offset, core::mem::size_of::<T>());
        core::ptr::write_unaligned(self.base.add(offset) as *mut T, value);
    }

    pub unsafe fn as_slice(&self) -> &[u8] {
        core::slice::from_raw_parts(self.base, self.size)
    }

    pub unsafe fn copy_from(&self, offset: usize, src: &[u8]) {
        self.check(offset, src.len());
        core::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(offset), src.len());
    }

    pub unsafe fn zero(&self) {
        core::ptr::write_bytes(self.base, 0, self.size);
    }

    /// Pointer at offset, bounds-checked. For passing to APIs that need raw pointers.
    pub fn ptr_at(&self, offset: usize) -> *mut u8 {
        self.check(offset, 0);
        unsafe { self.base.add(offset) }
    }
}
