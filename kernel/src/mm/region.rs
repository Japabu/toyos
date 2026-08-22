//! A bounds-checked window onto kernel RAM, and the one rule that makes it
//! mean anything.
//!
//! **A `KernelSlice` is built from an [`Allocation`] and from nothing else.**
//! There is no way to name a base and a length separately: `whole` reads both
//! off the value that owns the pages, and [`KernelSlice::subslice`] is the only
//! way to narrow one. That is what every `check` in this file rests on — a
//! length written out beside a pointer at a call site is checked by nobody, and
//! the constructor that took one (`from_raw`) is gone. Every past
//! out-of-bounds in the ELF loader came through that constructor.
//!
//! What this type still does *not* say is how long the allocation lives:
//! `KernelSlice` is `Copy`, carries no lifetime, and can therefore outlive the
//! `Allocation` it was built from. [`super::Dma`] closed that for DMA memory by
//! borrowing its pool; here it is
//! `issues/design-debt/kernelslice-outlives-its-allocation.md`.

/// A kernel allocation that can vouch for its own extent.
///
/// The promise `KernelSlice`'s deleted `from_raw` used to ask of every
/// construction site, asked once per allocator type instead — next to the code
/// that owns the pages, where the size is the allocation's own and not a number
/// somebody wrote beside a pointer.
///
/// # Safety
/// `ptr()` must be valid for reads and writes of `size()` bytes for as long as
/// `self` is alive, and `size()` must be derived from the allocation itself —
/// never from a value handed in beside it.
pub unsafe trait Allocation {
    /// First byte of the allocation, addressable by the kernel.
    fn ptr(&self) -> *mut u8;
    /// How many bytes were allocated.
    fn size(&self) -> usize;
}

/// Bounds-checked view into a contiguous kernel memory region.
/// Like Mmio but for RAM — prevents out-of-bounds reads/writes.
///
/// **Not for DMA memory.** That was this type's third caller and it is
/// [`super::Dma`] now: a view the pool hands out, bounded for the length and not
/// only the offset, safe at every accessor, and carrying the pool's lifetime so
/// the residual the module header names cannot arise. What is left here is the
/// loader's and the process's.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KernelSlice {
    base: *mut u8,
    size: usize,
}

// SAFETY: `KernelSlice` is `Copy` and carries no lock, so moving or sharing
// the `(base, size)` pair itself is inert — it is only ever a bounds-checked
// address and a length, never a claim of ownership or of who else may touch
// the memory behind it. The pair describes real memory because `whole` is the
// only constructor and it reads both halves off an `Allocation`, whose own
// `# Safety` is that promise. Every method that reads or writes through `base`
// (`read`, `write`, `as_slice`, `copy_from`, `zero`) is itself an `unsafe fn`,
// so the aliasing/synchronization discipline for a *use* is the caller's, not
// something `Send`/`Sync` promises here — same shape as `Mmio`.
unsafe impl Send for KernelSlice {}
// SAFETY: see the `Send` impl above — same reasoning.
unsafe impl Sync for KernelSlice {}

impl KernelSlice {
    /// The whole of an allocation, sized by the allocation.
    ///
    /// **The only constructor**, and safe because of it: the base and the size
    /// come off the same value, so the bound every access is checked against is
    /// what was actually allocated. A caller that wants less takes a
    /// [`subslice`](Self::subslice) of this, which is checked against it.
    pub fn whole(alloc: &impl Allocation) -> Self {
        Self { base: alloc.ptr(), size: alloc.size() }
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
            // SAFETY: `offset + size <= self.size` was just asserted above, so
            // the result stays within the allocation `self` covers — which is a
            // real one, because `whole` is the only way `self` can have come
            // into existence.
            base: unsafe { self.base.add(offset) },
            size,
        }
    }

    fn check(&self, offset: usize, len: usize) {
        assert!(offset + len <= self.size,
            "KernelSlice OOB: offset={:#x} len={} size={:#x}", offset, len, self.size);
    }

    /// # Safety
    /// Nothing may be concurrently writing `size_of::<T>()` bytes at `offset`
    /// while the read runs. That the range is inside a real allocation is
    /// `check` plus [`whole`](Self::whole), not the caller's to argue.
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
    /// The returned `&[u8]` must not alias a live `&mut` (through `write`,
    /// `copy_from` or `zero`) for as long as it is held, and the `Allocation`
    /// `self` was built from must outlive it.
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
