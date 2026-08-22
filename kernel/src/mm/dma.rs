//! DMA memory, as a view rather than as a pointer.
//!
//! [`DmaPool`] owns physical pages; [`Dma`] is the only way to touch what is in
//! them, and every one of its accessors is **safe**. That is the same claim
//! [`Mmio`](super::Mmio) makes for a memory-mapped register window and it rests
//! on the same three things: construction is private to this module, so no
//! caller can name a region the pool did not hand out; every access is bounded
//! for `size_of::<T>()` and not for the offset alone; and the view carries the
//! pool's lifetime, so the region cannot outlive the pages behind it.
//!
//! Before this, `DmaPool::slice()` handed out a `KernelSlice` whose every
//! accessor was an `unsafe fn`, so 35 blocks across ten drivers spelled the
//! unsafety at the call site and argued the same three sentences by hand
//! (measured by the `undocumented_unsafe_blocks` sweep of `drivers/`,
//! 2026-08-22). Five drivers had each grown a local approximation of this type —
//! `virtio::Ring`, `xhci::zero_dma`, `xhci::wait::msc::{read_dma, write_dma}`,
//! `nvme::zero_dma`, `virtio_gpu::{put, answer}` — which is what says the
//! abstraction belongs on the pool once.
//!
//! # Two disciplines, and a driver cannot take the wrong one by accident
//!
//! One accessor set cannot serve both kinds of DMA memory, so there are two and
//! the difference is in the type:
//!
//! - [`Volatile`] — **memory the device may be touching at this instant.**
//!   Descriptor tables, available and used rings, transfer and event rings,
//!   completion queues, device and input contexts, the xHCI DCBAA, its
//!   scratchpad array and its ERST. Every access is `read_volatile` /
//!   `write_volatile`, so it cannot be elided, merged, split or reordered
//!   against its neighbours — which is what makes a poll observe a Cycle or
//!   Phase bit flip rather than reading it once — and it is asserted naturally
//!   aligned for `T`, because that is what those two intrinsics require.
//!
//! - [`Unaligned`] — **memory the protocol has already fenced.** A structure
//!   written before the device is told where it is, or read after the device has
//!   said it is done with it: a Command Block Wrapper and its status block
//!   (`xhci::wait::msc`), an NVMe PRP list and its Identify Namespace answer, an
//!   HDA buffer descriptor list, a virtio-gpu command header, one byte out of a
//!   virtio-console RX buffer. Their fields sit where a specification put them
//!   rather than where an ABI would, so every access is `read_unaligned` /
//!   `write_unaligned` and carries no alignment requirement — and none of them
//!   needs volatile, because nothing else is looking at the bytes while the
//!   access runs.
//!
//! [`DmaPool::view`] and [`DmaPool::leak`] hand out the volatile discipline,
//! because racing the device is what DMA memory is *for*; the other is reached
//! by naming [`Dma::unaligned`], which is a word a driver has to write. There is
//! no way back: a region that has been declared quiescent stays that way for the
//! expression that declared it.
//!
//! # The lifetime, and what it closes
//!
//! `Dma<'pool>` borrows the [`DmaPool`] it came out of, so a view can never
//! outlive the pages it names. That is the residual
//! `issues/design-debt/kernelslice-outlives-its-allocation.md` still records for
//! [`super::KernelSlice`] — closed for DMA memory here by construction rather
//! than by adjacency.
//!
//! A driver whose device outlives every scope is served by [`DmaPool::leak`],
//! which consumes the pool, never gives its pages back, and answers with
//! `Dma<'static>`. That is the honest form of what four drivers used to spell as
//! a `static` holding a pool nobody ever read again — and it is a stronger
//! statement than the `static` was, because a `Dma<'static>` cannot be built any
//! other way.

use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ptr::{copy_nonoverlapping, read_unaligned, read_volatile, write_bytes,
                write_unaligned, write_volatile};

use super::pmm::PhysPage;
use super::DirectMap;

mod sealed {
    pub trait Sealed {}
}

/// Which accessor set a [`Dma`] carries. Sealed: the two below are the whole
/// set, and a third would be a third answer to a question with two.
pub trait Discipline: sealed::Sealed {}

/// The discipline for memory that races the device. See the module header.
pub enum Volatile {}
/// The discipline for memory the protocol has fenced. See the module header.
pub enum Unaligned {}

impl sealed::Sealed for Volatile {}
impl sealed::Sealed for Unaligned {}
impl Discipline for Volatile {}
impl Discipline for Unaligned {}

/// A bounds-checked, safe view of DMA memory, valid for as long as the
/// [`DmaPool`] it came out of.
///
/// `Copy`, like [`Mmio`](super::Mmio): it is an address, a length and a
/// discipline, and copying one grants nothing the original did not have. The
/// lifetime travels with the copy, which is the whole difference from the
/// `KernelSlice` this replaced.
pub struct Dma<'pool, D: Discipline = Volatile> {
    base: *mut u8,
    size: usize,
    /// What the view may not outlive.
    pool: PhantomData<&'pool DmaPool>,
    /// Which accessor set is in scope, and nothing at run time.
    how: PhantomData<D>,
}

impl<D: Discipline> Clone for Dma<'_, D> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<D: Discipline> Copy for Dma<'_, D> {}

// SAFETY: `Dma` is `Copy` and carries no lock, so moving or sharing the
// `(base, size)` pair itself is inert — it is a bounds-checked address, a
// length and a marker, never a claim of ownership or of who else may touch the
// memory behind it. The pages it names are physical memory reachable through
// the direct map on every CPU, so the address means the same thing wherever it
// is read. What the impl does *not* promise is a discipline for concurrent
// *use*: that is the driver's, exactly as it is for `Mmio`, whose `Send`/`Sync`
// rest on the same sentence. It is sound to write here where it is not for an
// arbitrary `*mut u8` because the region cannot be freed under the view — the
// lifetime says so, and `DmaPool::leak` is the only way to reach `'static`.
unsafe impl<D: Discipline> Send for Dma<'_, D> {}
// SAFETY: see the `Send` impl above — same reasoning.
unsafe impl<D: Discipline> Sync for Dma<'_, D> {}

impl<'pool, D: Discipline> Dma<'pool, D> {
    /// The one constructor, private to `mm::dma`: a `Dma` can only come from a
    /// [`DmaPool`] that is holding the pages, or from a [`subview`](Self::subview)
    /// of one that already does.
    #[inline]
    fn new(base: *mut u8, size: usize) -> Self {
        Self { base, size, pool: PhantomData, how: PhantomData }
    }

    /// How many bytes this view covers.
    #[inline]
    pub fn size(self) -> usize {
        self.size
    }

    /// The physical address of the first byte, which is what a descriptor,
    /// a PRP entry or a base-address register is programmed with.
    #[inline]
    pub fn phys(self) -> u64 {
        DirectMap::phys_of(self.base)
    }

    /// The `size` bytes at `offset`, as a view of their own.
    ///
    /// Refuses anything that is not wholly inside `self`, so a region carved out
    /// of a pool is never larger than the pool and a structure carved out of a
    /// region is never larger than the region.
    #[inline]
    pub fn subview(self, offset: usize, size: usize) -> Self {
        self.check(offset, size);
        // SAFETY: `check` just refused anything but `offset + size <= self.size`,
        // so every byte of the result is inside the region `self` covers — which
        // is inside the pool that constructed it, since this is the only way to
        // narrow one.
        Self::new(unsafe { self.base.add(offset) }, size)
    }

    /// Clear the whole view.
    #[inline]
    pub fn zero(self) {
        // SAFETY: exactly `self.size` bytes from `self.base`, which is the
        // region this view was constructed for and nothing else. `u8` has no
        // alignment requirement and no drop glue.
        unsafe { write_bytes(self.base, 0, self.size) }
    }

    /// Copy `src` into the view at `offset`.
    #[inline]
    pub fn copy_from(self, offset: usize, src: &[u8]) {
        self.check(offset, src.len());
        // SAFETY: `check` bounded the destination for `src.len()` bytes, and
        // `src` is a live `&[u8]` of exactly that length. The two cannot overlap:
        // `src` is kernel heap or stack and `self` is a physical page out of the
        // DMA category, which the heap is never allocated from.
        unsafe { copy_nonoverlapping(src.as_ptr(), self.base.add(offset), src.len()) }
    }

    /// Copy `dst.len()` bytes out of the view at `offset` into `dst`.
    ///
    /// A copy and not a `&[u8]`: a reference into DMA memory would outlive the
    /// instant at which the driver knows the device is not writing it, and every
    /// caller here wants the bytes rather than the borrow.
    #[inline]
    pub fn copy_to(self, offset: usize, dst: &mut [u8]) {
        self.check(offset, dst.len());
        // SAFETY: `check` bounded the source for `dst.len()` bytes, `dst` is a
        // live `&mut [u8]` of exactly that length, and the two cannot overlap for
        // the reason `copy_from` gives.
        unsafe { copy_nonoverlapping(self.base.add(offset), dst.as_mut_ptr(), dst.len()) }
    }

    /// Refuse an access that is not wholly inside this view.
    ///
    /// A panic and not a `Result`: every offset and length that reaches here is
    /// the driver's own arithmetic over its own layout constants, so a refusal is
    /// a kernel bug — the one thing this tree answers with a panic rather than
    /// with a refusal. Nothing a device chose reaches it: a device-chosen number
    /// is an [`toyos_untrusted::Untrusted`] and is bounded before it becomes an
    /// offset.
    #[inline]
    fn check(self, offset: usize, len: usize) {
        if let Err(why) = toyos_dma::within(offset, len, self.size) {
            refuse(why, self.base);
        }
    }
}

/// The refusal, out of line and marked cold, so the accessors above stay small
/// enough for the inliner.
///
/// **Measured, not assumed.** With the panic in the accessor body LLVM declined
/// to inline `Dma::read` at all: `Virtqueue::poll_used` compiled to three
/// `callq _R…Dma4read…` per turn of its loop where the driver-local `Ring` it
/// replaced had the load inline. This restores that — the emitted `poll_used` is
/// one `movzwl` per volatile read again, and the comparison is in the pull
/// request that introduced this type.
#[cold]
#[inline(never)]
fn refuse(why: toyos_dma::Refused, base: *mut u8) -> ! {
    panic!("DMA: {why}, in the region at {base:p}");
}

impl<'pool> Dma<'pool, Volatile> {
    /// Read the `T` at `offset` with a volatile load.
    ///
    /// Bounded for all of `T` and asserted naturally aligned for it — the two
    /// things `read_volatile` needs and the two the raw form at each of these
    /// call sites argued in prose.
    #[inline]
    pub fn read<T: Copy>(self, offset: usize) -> T {
        // SAFETY: `at` refused anything but a naturally aligned `size_of::<T>()`
        // bytes inside this view, which is the whole of what `read_volatile`
        // requires of the pointer. Volatile because the device may be writing
        // these bytes concurrently — that is what this discipline names — so the
        // load may not be elided, merged or reordered against its neighbours.
        // The value is a `T: Copy`, so nothing is duplicated that has drop glue.
        unsafe { read_volatile(self.at::<T>(offset) as *const T) }
    }

    /// Write `value` to the `T` at `offset` with a volatile store.
    #[inline]
    pub fn write<T: Copy>(self, offset: usize, value: T) {
        // SAFETY: `at` refused anything but a naturally aligned `size_of::<T>()`
        // bytes inside this view, which is what `write_volatile` requires.
        // Volatile because the device may be reading these bytes concurrently —
        // a Cycle bit, an available index or a doorbell's worth of descriptor —
        // so the store may not be elided, split, merged or reordered.
        unsafe { write_volatile(self.at::<T>(offset), value) }
    }

    /// The same memory, under the discipline for a region the protocol has
    /// fenced: `read_unaligned`/`write_unaligned`, no alignment requirement, no
    /// volatile.
    ///
    /// A driver has to write the word, and the header says which regions have
    /// earned it. There is no way back: nothing in this tree wants both
    /// disciplines over the same expression.
    #[inline]
    pub fn unaligned(self) -> Dma<'pool, Unaligned> {
        Dma::new(self.base, self.size)
    }

    /// A pointer to a naturally aligned `T` wholly inside this view.
    #[inline]
    fn at<T>(self, offset: usize) -> *mut T {
        self.check(offset, core::mem::size_of::<T>());
        if let Err(why) =
            toyos_dma::aligned(self.base as usize, offset, core::mem::align_of::<T>())
        {
            refuse_unaligned(why, core::any::type_name::<T>());
        }
        // SAFETY: `check` on the line above refused anything but
        // `offset + size_of::<T>() <= self.size`, so the whole `T` is inside the
        // region this view covers.
        unsafe { self.base.add(offset) as *mut T }
    }
}

/// The alignment refusal, out of line and cold for [`refuse`]'s reason.
#[cold]
#[inline(never)]
fn refuse_unaligned(why: toyos_dma::Refused, what: &str) -> ! {
    panic!("DMA: {why}, and a volatile access of {what} needs it");
}

impl Dma<'_, Unaligned> {
    /// Read the `T` at `offset`, whatever it is aligned to.
    #[inline]
    pub fn read<T: Copy>(self, offset: usize) -> T {
        self.check(offset, core::mem::size_of::<T>());
        // SAFETY: `check` refused anything but `size_of::<T>()` bytes inside this
        // view, and `read_unaligned` asks nothing else of the pointer. Not
        // volatile, and this discipline is what says so: every caller reads a
        // structure the device has finished writing — the transfer that filled it
        // completed, and the completion is what ordered the two.
        unsafe { read_unaligned(self.base.add(offset) as *const T) }
    }

    /// Write `value` to the `T` at `offset`, whatever it is aligned to.
    #[inline]
    pub fn write<T: Copy>(self, offset: usize, value: T) {
        self.check(offset, core::mem::size_of::<T>());
        // SAFETY: `check` refused anything but `size_of::<T>()` bytes inside this
        // view, and `write_unaligned` asks nothing else of the pointer. Not
        // volatile, and this discipline is what says so: every caller writes a
        // structure before the device is told where it is.
        unsafe { write_unaligned(self.base.add(offset) as *mut T, value) }
    }
}

/// Contiguous DMA memory backed by 2 MiB physical pages from the PMM.
///
/// The pages are the pool's; [`view`](Self::view) is how a driver reaches them
/// and [`Dma`] is the only thing that can. A pool dropped gives every page back,
/// which is what makes a device this kernel refuses cost no physical memory.
///
/// **`Send` is derived, not asserted.** `PhysPage` and `DirectMap` are integers,
/// so the auto trait already holds — the manual `unsafe impl Send for DmaPool {}`
/// that stood here was redundant and is gone.
pub struct DmaPool {
    pages: Vec<PhysPage>,
    base: DirectMap,
    size: usize,
}

impl DmaPool {
    /// Take enough contiguous 2 MiB pages to cover `size` bytes.
    pub fn alloc(size: usize) -> Self {
        let pages_2m = size.div_ceil(super::PAGE_2M as usize);
        let pages = super::pmm::alloc_contiguous(pages_2m, super::pmm::Category::Dma)
            .expect("DmaPool: out of physical memory");
        let base = pages[0].direct_map();
        Self { pages, base, size: pages_2m * super::PAGE_2M as usize }
    }

    /// The whole pool, as a view that may not outlive it.
    ///
    /// This is where the deleted `KernelSlice::from_raw`'s justification used to
    /// be argued and where it is now enforced: `alloc_contiguous` returned physically
    /// contiguous pages, `pages[0].direct_map()` is their first byte in the
    /// direct map, `self` is holding every one of them, and the borrow is what
    /// says the caller may not keep the view past that.
    pub fn view(&self) -> Dma<'_> {
        Dma::new(self.base.as_mut_ptr(), self.size)
    }

    /// Consume the pool, never give its pages back, and answer with a view that
    /// outlives everything.
    ///
    /// **For a device that outlives every scope**, which is every device this
    /// kernel binds: nothing here is ever unbound, so the alternative is a
    /// `static` holding a pool no code reads again — which is what
    /// `virtio_console`, `virtio_net`, `virtio_gpu` and `nvme` each had, one per
    /// driver, purely to keep the pages alive. This says the same thing once and
    /// says it in the type.
    ///
    /// It is called at the point where the driver has committed, so every refusal
    /// *above* it still drops the pool and gives the pages back. The `Vec`'s own
    /// heap allocation is not leaked; the pages are, deliberately and for good.
    pub fn leak(self) -> Dma<'static> {
        let Self { pages, base, size } = self;
        for page in pages {
            core::mem::forget(page);
        }
        Dma::new(base.as_mut_ptr(), size)
    }
}
