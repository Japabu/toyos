/// Bounds-checked view into a contiguous kernel memory region.
/// Like Mmio but for RAM — prevents out-of-bounds reads/writes.
///
/// **Not for DMA memory.** That was this type's third caller and it is
/// [`super::Dma`] now: a view the pool hands out, bounded for the length and not
/// only the offset, safe at every accessor, and carrying the pool's lifetime so
/// the residual below cannot arise. What is left here is the loader's and the
/// process's, where the size and the allocation still travel separately.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KernelSlice {
    base: *mut u8,
    size: usize,
}

// SAFETY: `KernelSlice` is `Copy` and carries no lock, so moving or sharing
// the `(base, size)` pair itself is inert — it is only ever a bounds-checked
// address and a length, never a claim of ownership or of who else may touch
// the memory behind it. Every method that actually reads or writes through
// `base` (`read`, `write`, `as_slice`, `copy_from`, `zero`) is itself an
// `unsafe fn`, so the aliasing/synchronization discipline for a *use* is the
// caller's, not something `Send`/`Sync` promises here — same shape as
// `Mmio`. What this impl does *not* cover is whether `base`/`size` describe
// real memory at all: `from_raw` cannot check that against the allocation it
// came from, which is the open, tracked gap in
// `issues/design-debt/kernelslice-from-raw-cannot-check-itself.md`.
unsafe impl Send for KernelSlice {}
// SAFETY: see the `Send` impl above — same reasoning, same caveat.
unsafe impl Sync for KernelSlice {}

// TODO: who intantiates this? if its always consumers from raw then nothing prevents it from being wrong
impl KernelSlice {
    /// Wrap an existing kernel pointer + size.
    ///
    /// # Safety
    /// `base` must be valid for reads and writes of `size` bytes for as long
    /// as the returned `KernelSlice` (or any value `subslice`d from it) is
    /// used, and nothing outside the discipline the caller itself keeps may
    /// alias that range while a `write`/`copy_from`/`zero` through it is in
    /// flight. Not checked here against the allocation `base` came from —
    /// see the open, tracked gap this leaves:
    /// `issues/design-debt/kernelslice-from-raw-cannot-check-itself.md`.
    /// TODO: should not exist
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
            // SAFETY: `offset + size <= self.size` was just asserted above,
            // so the result stays within whatever range `self.base` is valid
            // for — which is exactly as far as `self` itself was ever
            // verified to extend (see the type-level `SAFETY` above: `Send`
            // does not vouch for `self`'s own validity, only `from_raw`'s
            // caller does).
            base: unsafe { self.base.add(offset) },
            size,
        }
    }

    fn check(&self, offset: usize, len: usize) {
        assert!(offset + len <= self.size,
            "KernelSlice OOB: offset={:#x} len={} size={:#x}", offset, len, self.size);
    }

    /// # Safety
    /// Same requirement `from_raw` places on `self`, for `size_of::<T>()`
    /// bytes at `offset` — `check` bounds `offset` against `self.size`, not
    /// against real memory, so this inherits `from_raw`'s open gap.
    pub unsafe fn read<T>(&self, offset: usize) -> T {
        self.check(offset, core::mem::size_of::<T>());
        core::ptr::read_unaligned(self.base.add(offset) as *const T)
    }

    /// # Safety
    /// Same as `read`, and additionally that nothing else is concurrently
    /// reading or writing this range while the write lands.
    pub unsafe fn write<T: Copy>(&self, offset: usize, value: T) {
        self.check(offset, core::mem::size_of::<T>());
        core::ptr::write_unaligned(self.base.add(offset) as *mut T, value);
    }

    /// # Safety
    /// `self`'s whole range must satisfy `from_raw`'s requirement, and the
    /// returned `&[u8]` must not alias a live `&mut` (through `write`,
    /// `copy_from` or `zero`) for as long as it is held.
    pub unsafe fn as_slice(&self) -> &[u8] {
        core::slice::from_raw_parts(self.base, self.size)
    }

    /// # Safety
    /// Same as `write`, for `src.len()` bytes at `offset`.
    pub unsafe fn copy_from(&self, offset: usize, src: &[u8]) {
        self.check(offset, src.len());
        core::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(offset), src.len());
    }

    /// # Safety
    /// Same as `write`, for the whole range.
    pub unsafe fn zero(&self) {
        core::ptr::write_bytes(self.base, 0, self.size);
    }
}
