use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU8, Ordering};

use super::MAX_HEAP_ALLOC;
use super::PHYS_OFFSET;
use super::PAGE_2M;
use super::pmm;

/// DESIGN RULE: nothing this type does may panic.
///
/// Every method here runs inside `KernelAllocator`'s `dlmalloc.lock()`, in the
/// middle of dlmalloc mutating its own chunk and segment lists, and the kernel
/// does not unwind — so a panic from in here abandons the heap in whatever
/// state it was in, with the lock held forever. The CPU that recovers from
/// that panic then spins `Lock::lock` on its next `alloc` or `free`, and the
/// machine goes quiet.
///
/// So a size this source cannot back is a `null`, not an assert: dlmalloc
/// hands the null back to the caller with its structures consistent, the lock
/// drops, and whatever the caller does about it — `handle_alloc_error`, an
/// `Option` — happens outside. The fail-fast for a caller asking the heap for
/// page-scale memory is [`MAX_HEAP_ALLOC`], checked in `KernelAllocator::alloc`
/// *before* the lock is taken.
///
/// `pmm::alloc_page` is the only thing reached from here, and it is
/// panic-free by the same rule.
struct KernelPageSource;

// SAFETY: `dlmalloc::Allocator` trusts its implementer to hand out memory the
// caller owns exclusively until it comes back through `free`/`free_part`, at
// the size `alloc` itself reported — dlmalloc never touches memory outside
// the `(ptr, size)` pair it was given. `alloc` here always answers in whole
// `PAGE_2M` pages from `pmm::alloc_page`, so `free`'s `PhysPage::from_raw`
// only ever reconstructs a page `pmm` actually handed out at that address —
// `free` is what returns it, never a bare drop. `remap`/`free_part`/
// `can_release_part` all refuse (null or `false`), so dlmalloc can never ask
// this source to reshape a live allocation into something that no longer
// matches a real page's bounds.
unsafe impl dlmalloc::Allocator for KernelPageSource {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        if size > PAGE_2M as usize {
            return (core::ptr::null_mut(), 0, 0);
        }
        if let Some(page) = pmm::alloc_page(pmm::Category::KernelHeap) {
            let ptr = page.direct_map().as_mut_ptr::<u8>();
            core::mem::forget(page); // dlmalloc manages the lifetime
            (ptr, PAGE_2M as usize, 0)
        } else {
            (core::ptr::null_mut(), 0, 0)
        }
    }

    fn remap(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize, _can_move: bool) -> *mut u8 {
        core::ptr::null_mut()
    }

    fn free_part(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize) -> bool {
        false
    }

    fn free(&self, ptr: *mut u8, _size: usize) -> bool {
        let phys = ptr as u64 - PHYS_OFFSET;
        drop(pmm::PhysPage::from_raw(phys));
        true
    }

    fn can_release_part(&self, _flags: u32) -> bool {
        false
    }

    fn allocates_zeros(&self) -> bool {
        true
    }

    fn page_size(&self) -> usize {
        PAGE_2M as usize
    }
}

/// The heap tripwire: a band of known bytes on each side of every allocation,
/// read back when it is freed and, for the allocations that matter most, while
/// they are still live.
///
/// **It exists because the allocator's own answer was too expensive.**
/// `dlmalloc` carries Doug Lea's full consistency checker behind its `debug`
/// cargo feature — `check_malloc_state` walks all 32 smallbins, all 32
/// treebins and every chunk of every segment at the head of *every* `malloc`
/// and `free`. That is the right oracle at the wrong price: this heap runs to
/// several MiB by the end of boot, so the walk is O(heap) per allocation and a
/// TCG guest carrying it does not finish a boot in a storm's lifetime. The
/// bands below cost two writes per allocation and two reads per free, and they
/// answer the question this class actually poses — *is something writing
/// outside an allocation it owns* — rather than the one the checker answers,
/// which is whether the free lists still hang together afterwards.
///
/// What each side catches:
///
/// * The **tail** catches an overrun off the end. 32 bytes, which is
///   `dlmalloc`'s `min_chunk_size` on x86-64, so a near miss lands here and not
///   in the next chunk's header where it would be indistinguishable from
///   allocator state.
/// * The **head** catches an underrun, and it is as wide as the request's
///   alignment. That is not a rounding accident: a task's kernel stack is
///   `OwnedAlloc::new(KERNEL_STACK_SIZE, 4096)`, so under this feature it gets
///   **4096 bytes of head band**, and a kernel stack that walks off its own
///   bottom writes into that band instead of into the neighbouring chunk. It is
///   the guard page `arch::percpu` gives every *idle* stack, in the one form
///   available to an allocation that comes out of the heap.
///
/// Neither band is swept — there is no registry of live allocations to sweep —
/// so a band is read at `dealloc`, which is late, and by [`check_live`] at
/// whatever site cares, which is not. `sched::driver`'s pass reads the running
/// task's bands every pass, which is the both-ends pattern `sched-tripwire`
/// established: a band broken at the entry to a pass was broken before that
/// pass ran a statement.
///
/// **The one behaviour it changes.** A request within `head + TAIL` bytes of
/// `MAX_HEAP_ALLOC` fits the ceiling and its banded form does not, so it is
/// answered `null` here and would have succeeded without the feature.
/// `OwnedAlloc::new` hands that back as `None`; a `Vec` would reach
/// `handle_alloc_error`. This is a diagnostic build and the window is 4 KiB
/// wide at the very top of a 2 MiB ceiling, so it is recorded rather than
/// papered over.
#[cfg(feature = "heap-tripwire")]
mod tripwire {
    use core::alloc::Layout;

    /// Bytes past the payload.
    const TAIL: usize = 32;
    /// The narrowest head band. Every alignment up to this divides it, so the
    /// payload stays aligned whichever arm `head` takes.
    const MIN_HEAD: usize = 32;

    /// The last 32 bytes of the head band, immediately before the payload.
    const OPEN: u64 = 0x4845_4144_5a4f_4e45;
    const CLOSE: u64 = 0x5a4f_4e45_4441_4548;
    /// Every head byte before those 32, and every tail byte.
    const FILL: u8 = 0x5a;

    /// Head bytes for a request of this alignment.
    const fn head(align: usize) -> usize {
        if align > MIN_HEAD { align } else { MIN_HEAD }
    }

    /// What is actually asked of `dlmalloc`.
    pub fn outer(layout: Layout) -> Layout {
        let size = layout.size() + head(layout.align()) + TAIL;
        // The alignment is unchanged and the size only grew, so the only way
        // this fails is a size past `isize::MAX` — which the `MAX_HEAP_ALLOC`
        // assert in the caller has already refused.
        Layout::from_size_align(size, layout.align()).expect("heap-tripwire: banded layout")
    }

    /// Write the bands and hand back the payload. A null base stays null: a
    /// refused allocation is still refused.
    ///
    /// # Safety
    /// `base` is null, or points at `outer(layout)` bytes the caller owns.
    pub unsafe fn arm(base: *mut u8, layout: Layout) -> *mut u8 {
        if base.is_null() {
            return base;
        }
        let head = head(layout.align());
        let payload = base.add(head);
        core::ptr::write_bytes(base, FILL, head - 32);
        payload.sub(32).cast::<u64>().write_unaligned(OPEN);
        payload.sub(24).cast::<u64>().write_unaligned(layout.size() as u64);
        payload.sub(16).cast::<u64>().write_unaligned(layout.align() as u64);
        payload.sub(8).cast::<u64>().write_unaligned(CLOSE);
        core::ptr::write_bytes(payload.add(layout.size()), FILL, TAIL);
        payload
    }

    /// Read the bands back and hand back what `dlmalloc` was given.
    ///
    /// # Safety
    /// `ptr` and `layout` are a pair this module's [`arm`] produced.
    pub unsafe fn disarm(ptr: *mut u8, layout: Layout) -> (*mut u8, Layout) {
        check(ptr, layout, "dealloc");
        // The rest of the head band, which [`check`] deliberately skips. Only
        // here: it is `align - 32` bytes wide, 4064 of them on a kernel stack,
        // and a live site pays that on every visit.
        let head = head(layout.align());
        for i in 0..head - 32 {
            let byte = ptr.sub(head).add(i).read();
            assert!(
                byte == FILL,
                "HEAP TRIPWIRE (dealloc): {ptr:?} was written {} bytes BELOW its {}-byte \
                 allocation — head band byte +{i} of {} is {byte:#04x}, want {FILL:#04x}",
                head - i, layout.size(), head - 32,
            );
        }
        (ptr.sub(head), outer(layout))
    }

    /// The two edges of an allocation that is still live: the record
    /// immediately below the payload, and the whole tail band.
    ///
    /// **The record and not the whole head band, and that is the design.** Both
    /// are eight-word reads, so a caller on the scheduler's pass path can
    /// afford them — and the record sits at the very top of the head band,
    /// immediately below the payload, which is the first thing a stack walking
    /// off its own bottom writes. Widening this to the full band would buy
    /// nothing a stack overflow does not already trip, at 508 more reads per
    /// pass.
    ///
    /// # Safety
    /// `ptr` and `layout` are a pair this module's [`arm`] produced, and
    /// nothing is freeing that allocation concurrently.
    pub unsafe fn check(ptr: *mut u8, layout: Layout, site: &str) {
        let open = ptr.sub(32).cast::<u64>().read_unaligned();
        let size = ptr.sub(24).cast::<u64>().read_unaligned();
        let align = ptr.sub(16).cast::<u64>().read_unaligned();
        let close = ptr.sub(8).cast::<u64>().read_unaligned();
        assert!(
            open == OPEN && close == CLOSE
                && size == layout.size() as u64
                && align == layout.align() as u64,
            "HEAP TRIPWIRE ({site}): the head record of {ptr:?} is not the one that was written \
             — open {open:#018x} (want {OPEN:#018x}), close {close:#018x} (want \
             {CLOSE:#018x}), recorded {size}/{align}, holder says {}/{}. On a 4096-aligned \
             allocation this is the first word a kernel stack running off its own bottom \
             reaches.",
            layout.size(), layout.align(),
        );
        for i in 0..TAIL / 8 {
            let word = ptr.add(layout.size()).cast::<u64>().add(i).read_unaligned();
            assert!(
                word == u64::from_ne_bytes([FILL; 8]),
                "HEAP TRIPWIRE ({site}): {ptr:?} was written past its {}-byte allocation — \
                 tail band word +{} is {word:#018x}",
                layout.size(), i * 8,
            );
        }
    }
}

/// The tripwire's absent half: every entry point, costing nothing.
#[cfg(not(feature = "heap-tripwire"))]
mod tripwire {
    use core::alloc::Layout;

    #[inline(always)]
    pub fn outer(layout: Layout) -> Layout { layout }

    /// # Safety
    /// Trivially sound: it hands back its argument.
    #[inline(always)]
    pub unsafe fn arm(base: *mut u8, _layout: Layout) -> *mut u8 { base }

    /// # Safety
    /// Trivially sound: it hands back its arguments.
    #[inline(always)]
    pub unsafe fn disarm(ptr: *mut u8, layout: Layout) -> (*mut u8, Layout) { (ptr, layout) }
}

/// Read the bands of a *live* heap allocation.
///
/// For an allocation whose corruption would otherwise not be found until it is
/// freed, and whose freeing is far too late — the kernel stacks, which are
/// freed only once the task that ran off one is already gone.
///
/// Behind the feature rather than a no-op without it, because its one caller is
/// too: a shipping kernel that could call this could only be told "no".
///
/// # Safety
/// `ptr` and `layout` are a pair `GlobalAlloc::alloc` returned, and nothing is
/// freeing that allocation concurrently.
#[cfg(feature = "heap-tripwire")]
pub unsafe fn check_live(ptr: *mut u8, layout: Layout, site: &str) {
    tripwire::check(ptr, layout, site)
}

struct KernelAllocator {
    dlmalloc: Lock<dlmalloc::Dlmalloc<KernelPageSource>>,
    phase: AtomicU8,
}

const PHASE_UNINIT: u8 = 0;
const PHASE_EARLY: u8 = 1;
const PHASE_READY: u8 = 2;

use crate::sync::Lock;

impl KernelAllocator {
    const fn new() -> Self {
        Self {
            dlmalloc: Lock::new(dlmalloc::Dlmalloc::new_with_allocator(KernelPageSource)),
            phase: AtomicU8::new(PHASE_UNINIT),
        }
    }
}

// SAFETY: `GlobalAlloc` requires `alloc(layout)` to return either null or
// `layout.size()` bytes valid for reads/writes at `layout.align()`, and
// `dealloc(ptr, layout)` to only ever be called with the exact `(ptr,
// layout)` pair a prior `alloc` on this same allocator returned — Rust's
// allocation machinery (`Box`, `Vec`, `Arc`, …) is the caller and upholds
// that pairing by construction. The three phases keep the contract true
// across boot: a pointer `alloc` minted during `PHASE_EARLY` (out of
// `EARLY_BUF`, bump-allocated) is recognized on `dealloc` by address range
// (`is_early_ptr`), not by re-reading the current phase, so it is freed
// correctly even after `init()` has since switched the allocator to
// `PHASE_READY`; a `PHASE_READY` pointer is always one `dlm.malloc` itself
// returned, freed through the same `dlmalloc` instance, per dlmalloc's own
// bookkeeping. The struct's own DESIGN RULE is what keeps every path here
// panic-free, which is load-bearing: `dlmalloc.lock()` cannot be poisoned
// and recovered from.
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.phase.load(Ordering::Acquire) {
            PHASE_UNINIT => core::ptr::null_mut(),
            PHASE_EARLY => early_alloc(layout),
            _ => {
                assert!(layout.align() < PAGE_2M as usize,
                    "GlobalAlloc: {:#x} bytes with {:#x} align — use PageAlloc", layout.size(), layout.align());
                // Before the lock, deliberately. This is the ceiling every
                // bound upstream of the heap is derived against, and it used
                // to be enforced one level down in `KernelPageSource::alloc`
                // — inside `dlmalloc.lock()`, which is where a panic costs
                // the machine rather than the process.
                //
                // `MAX_HEAP_ALLOC` rather than whatever dlmalloc's padding
                // happens to permit, so the documented number is the enforced
                // number — the way `MAX_HANDLES` and `MAX_USER_STR` are at their
                // own primitives. Measured: 2,097,152 asks the page source
                // for 2,162,688, which it cannot back.
                //
                // Being past this is sufficient for a request to fail and not
                // necessary, which is why the page source is total rather
                // than merely unreachable: 2,093,056 with 4096-byte alignment
                // satisfies the check and still asks for 2,162,688, because
                // `memalign` pads by the alignment first.
                assert!(layout.size() <= MAX_HEAP_ALLOC,
                    "GlobalAlloc: {} bytes exceeds MAX_HEAP_ALLOC ({}) — a caller is using alloc for page-scale memory",
                    layout.size(), MAX_HEAP_ALLOC);
                // The bands are written after the lock is dropped and read
                // before it is taken, so the tripwire never runs inside
                // `dlmalloc.lock()` — the DESIGN RULE above is what makes that
                // placement load-bearing rather than tidy: a band that fails
                // inside the lock would abandon the heap with the lock held,
                // and the report would never reach the wire.
                let outer = tripwire::outer(layout);
                let base = {
                    let mut dlm = self.dlmalloc.lock();
                    dlm.malloc(outer.size(), outer.align())
                };
                tripwire::arm(base, layout)
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if is_early_ptr(ptr) { return; }
        let (base, outer) = tripwire::disarm(ptr, layout);
        let mut dlm = self.dlmalloc.lock();
        dlm.free(base, outer.size(), outer.align());
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator::new();

const EARLY_SIZE: usize = 512 * 1024;

#[repr(C, align(4096))]
struct EarlyBuffer([u8; EARLY_SIZE]);

static mut EARLY_BUF: EarlyBuffer = EarlyBuffer([0; EARLY_SIZE]);
static mut EARLY_POS: usize = 0;

unsafe fn early_alloc(layout: Layout) -> *mut u8 {
    let buf = core::ptr::addr_of_mut!(EARLY_BUF) as *mut u8;
    let aligned = (EARLY_POS + layout.align() - 1) & !(layout.align() - 1);
    let new_pos = aligned + layout.size();
    if new_pos > EARLY_SIZE {
        return core::ptr::null_mut();
    }
    EARLY_POS = new_pos;
    buf.add(aligned)
}

fn is_early_ptr(ptr: *mut u8) -> bool {
    let buf_start = core::ptr::addr_of!(EARLY_BUF) as usize;
    let p = ptr as usize;
    p >= buf_start && p < buf_start + EARLY_SIZE
}

/// Phase 1: Enable early bump allocator (before paging).
pub(super) fn init_early() {
    ALLOCATOR.phase.store(PHASE_EARLY, Ordering::Release);
}

/// Phase 2: Switch to dlmalloc (after pmm + paging are ready).
pub(super) fn init() {
    ALLOCATOR.phase.store(PHASE_READY, Ordering::Release);
}
