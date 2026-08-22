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
            #[cfg(feature = "heap-sweep")]
            pages::add(ptr as u64);
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
        #[cfg(feature = "heap-sweep")]
        pages::remove(ptr as u64);
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

    /// The record: `OPEN`, the size, the alignment, `CLOSE`. It is what makes a
    /// band self-describing, which is what [`super::sweep`] needs — a walk that
    /// finds a magic word in a heap page learns from the next three what the
    /// allocation around it is, with no registry of live allocations anywhere.
    pub const RECORD: usize = 32;

    /// The record's two magic words are **tied to the address they are written
    /// at**, and that is what makes [`super::sweep`] sound rather than tidy.
    ///
    /// A bare constant is a value this module's own code holds in registers,
    /// spills to locals and reads back into locals — and a task kernel stack is
    /// 128 KiB of the same heap the sweep walks. Measured: the first sweep
    /// build died on its second walk of *every* boot, on a bare `CLOSE` sitting
    /// in pid 0's kernel stack, 429 live records into a boot that had not
    /// finished spawning `logd`. XORing the address in means a copy of the word
    /// anywhere but where it was written does not read as a record, so the walk
    /// cannot find its own reader's leftovers.
    const OPEN: u64 = 0x4845_4144_5a4f_4e45;
    const CLOSE: u64 = 0x5a4f_4e45_4441_4548;

    /// The word that stands at `record`, and the one 24 bytes above it.
    pub const fn open_word(record: u64) -> u64 { OPEN ^ record }
    pub const fn close_word(record: u64) -> u64 { CLOSE ^ record }

    /// Every band byte that is not the record.
    pub const FILL: u8 = 0x5a;
    pub const FILL_WORD: u64 = u64::from_ne_bytes([FILL; 8]);

    /// **The three shapes, and why there is more than one.**
    ///
    /// `heap-tripwire` put a band on *both* sides of every allocation and the
    /// class it was built to catch stopped happening — 7,205 boots against a
    /// rate that expected twelve deaths, and no band ever fired. Two readings
    /// survive that and one band on each side cannot separate them: either the
    /// bands *absorb* a bounded overrun that used to land in the neighbouring
    /// chunk, or they *displace* every allocation and a victim computed from a
    /// layout assumption no longer lands on anything that matters.
    ///
    /// A band cannot be made zero-width and keep its placement — the padding
    /// *is* the displacement — so the separation is per side instead:
    ///
    /// | arm | head | tail | absorbs under | absorbs over | displaces |
    /// |---|---|---|---|---|---|
    /// | default | `max(align, 32)` | 32 | yes | yes | +64/alloc |
    /// | `heap-band-notail` | `max(align, 32)` | 0 | yes | **no** | +32/alloc |
    /// | `heap-band-nohead` | 0 | 64 | **no** | yes | +64/alloc |
    ///
    /// `heap-band-nohead` moves the record above the payload instead of below
    /// it, so every arm still has one; for an alignment of 32 or less it asks
    /// `dlmalloc` for exactly the same outer size as the default, which makes
    /// the pair as close to a one-variable comparison as a heap admits — the
    /// same chunks at the same addresses, with the payload at the other end of
    /// its own chunk. `heap-band-notail`'s outer size is the payload plus the
    /// head alone, and `dlmalloc` rounds a request to a 16-byte granule either
    /// way, so the slack past the payload's end is byte-for-byte what an
    /// unbanded build has.
    ///
    /// **What the arms measured, 2026-08-21, and the durable half of it.**
    /// Twelve-wide boot storms of ~7,000 boots each, `sched-tripwire` on in
    /// every one, against a same-session unbanded baseline of 6 deaths in 7,251
    /// boots: default bands 0/7,211, `notail` 0/7,011, `nohead` 2/7,102 — and
    /// the default bands *with* [`super::sweep`] added 9/7,040. The absorb
    /// reading predicted that taking the tail slack away would bring the deaths
    /// back, and it did the opposite. **So the band width is not what governs
    /// this class's rate; the layout it happens to produce is.** Do not read a
    /// quiet arm here as a fix, and do not read a loud one as a regression.
    #[cfg(all(feature = "heap-band-notail", feature = "heap-band-nohead"))]
    compile_error!("heap-band-notail and heap-band-nohead are two arms of one sweep; build one");

    #[cfg(not(any(feature = "heap-band-notail", feature = "heap-band-nohead")))]
    mod shape {
        pub const HEAD_MIN: usize = 32;
        pub const TAIL: usize = 32;
    }
    #[cfg(all(feature = "heap-band-notail", not(feature = "heap-band-nohead")))]
    mod shape {
        pub const HEAD_MIN: usize = 32;
        pub const TAIL: usize = 0;
    }
    #[cfg(all(feature = "heap-band-nohead", not(feature = "heap-band-notail")))]
    mod shape {
        pub const HEAD_MIN: usize = 0;
        pub const TAIL: usize = super::RECORD + 32;
    }

    pub use shape::{HEAD_MIN, TAIL};

    /// Where the record sits inside the tail band, and how much fill follows
    /// it. With a head band the record is below the payload and the whole tail
    /// is fill; without one the record is the first 32 bytes above the payload.
    pub const TAIL_FILL_OFF: usize = if HEAD_MIN == 0 { RECORD } else { 0 };
    pub const TAIL_FILL: usize = TAIL - TAIL_FILL_OFF;
    /// Every band walk here steps eight bytes and stops on `!=`, which is only
    /// the same loop as `<` while the step divides the band.
    const _: () = assert!(TAIL_FILL.is_multiple_of(8), "the tail band's fill is walked in words");

    /// Head bytes for a request of this alignment. A head band is as wide as
    /// the alignment because that is what keeps the payload aligned, and it is
    /// why `alloc_kernel_stack`'s `(KERNEL_STACK_SIZE, 4096)` gets 4096 bytes
    /// of it — the guard page `arch::percpu` gives every *idle* stack, in the
    /// one form available to an allocation that comes out of the heap.
    pub const fn head(align: usize) -> usize {
        if HEAD_MIN == 0 {
            0
        } else if align > HEAD_MIN {
            align
        } else {
            HEAD_MIN
        }
    }

    /// What is actually asked of `dlmalloc`.
    pub fn outer(layout: Layout) -> Layout {
        let size = layout.size() + head(layout.align()) + TAIL;
        // The alignment is unchanged and the size only grew, so the only way
        // this fails is a size past `isize::MAX` — which the `MAX_HEAP_ALLOC`
        // assert in the caller has already refused.
        Layout::from_size_align(size, layout.align()).expect("heap-tripwire: banded layout")
    }

    /// The record's first byte, for an allocation whose payload is `payload`.
    ///
    /// # Safety
    /// `payload`/`size` describe an allocation this module armed.
    pub unsafe fn record_ptr(payload: *mut u8, size: usize) -> *mut u8 {
        if HEAD_MIN == 0 { payload.add(size) } else { payload.sub(RECORD) }
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
        let payload = base.add(head(layout.align()));
        core::ptr::write_bytes(base, FILL, head(layout.align()));
        core::ptr::write_bytes(payload.add(layout.size()), FILL, TAIL);
        let record = record_ptr(payload, layout.size());
        record.cast::<u64>().write_unaligned(open_word(record as u64));
        record.add(8).cast::<u64>().write_unaligned(layout.size() as u64);
        record.add(16).cast::<u64>().write_unaligned(layout.align() as u64);
        record.add(24).cast::<u64>().write_unaligned(close_word(record as u64));
        payload
    }

    /// Read the bands back, retire the record and hand back what `dlmalloc`
    /// was given.
    ///
    /// **The retirement is what makes [`super::sweep`] sound.** A freed chunk
    /// keeps whatever bytes it held until `dlmalloc` writes its free-list links
    /// over them, so a record left standing in one would be found by a later
    /// walk and read as a live allocation whose bands are now allocator state.
    /// Both magic words go, so neither half of a retired record can be
    /// mistaken for a live one.
    ///
    /// # Safety
    /// `ptr` and `layout` are a pair this module's [`arm`] produced.
    pub unsafe fn disarm(ptr: *mut u8, layout: Layout) -> (*mut u8, Layout) {
        check(ptr, layout, "dealloc");
        let head = head(layout.align());
        // The rest of the head band, which [`check`] deliberately skips. Only
        // here: it is `align - 32` bytes wide, 4064 of them on a kernel stack,
        // and a live site pays that on every visit.
        // A `while` and not a `for` over a range, in this loop and every other
        // band walk here: a shape whose band is zero wide makes that range
        // empty at compile time, and an empty range is a clippy denial rather
        // than a loop that runs no times.
        let fill_bytes = head.saturating_sub(RECORD);
        let mut i = 0;
        while i < fill_bytes {
            let byte = ptr.sub(head).add(i).read();
            assert!(
                byte == FILL,
                "HEAP TRIPWIRE (dealloc): {ptr:?} was written {} bytes BELOW its {}-byte \
                 allocation — head band byte +{i} of {fill_bytes} is {byte:#04x}, want \
                 {FILL:#04x}",
                head - i, layout.size(),
            );
            i += 1;
        }
        let record = record_ptr(ptr, layout.size());
        record.cast::<u64>().write_unaligned(0);
        record.add(24).cast::<u64>().write_unaligned(0);
        (ptr.sub(head), outer(layout))
    }

    /// The edges of an allocation that is still live: the record, and the fill
    /// that shares a band with it.
    ///
    /// **The record and not the whole head band, and that is the design.** Both
    /// are eight-word reads, so a caller on the scheduler's pass path can
    /// afford them — and in the default shape the record sits at the very top
    /// of the head band, immediately below the payload, which is the first
    /// thing a stack walking off its own bottom writes. Widening this to the
    /// full band would buy nothing a stack overflow does not already trip, at
    /// 508 more reads per pass.
    ///
    /// # Safety
    /// `ptr` and `layout` are a pair this module's [`arm`] produced, and
    /// nothing is freeing that allocation concurrently.
    pub unsafe fn check(ptr: *mut u8, layout: Layout, site: &str) {
        let record = record_ptr(ptr, layout.size());
        let open = record.cast::<u64>().read_unaligned();
        let size = record.add(8).cast::<u64>().read_unaligned();
        let align = record.add(16).cast::<u64>().read_unaligned();
        let close = record.add(24).cast::<u64>().read_unaligned();
        assert!(
            open == open_word(record as u64) && close == close_word(record as u64)
                && size == layout.size() as u64
                && align == layout.align() as u64,
            "HEAP TRIPWIRE ({site}): the record of {ptr:?} is not the one that was written \
             — open {open:#018x}, close {close:#018x}, recorded {size}/{align}, holder says \
             {}/{}. In the default band shape this is the first word a kernel stack running \
             off its own bottom reaches.",
            layout.size(), layout.align(),
        );
        check_tail_fill(ptr, layout.size(), site);
    }

    /// The fill bytes of the tail band, which is the whole of it in the default
    /// shape and the 32 bytes above the record without a head band.
    ///
    /// # Safety
    /// `ptr`/`size` describe an allocation this module armed.
    pub unsafe fn check_tail_fill(ptr: *mut u8, size: usize, site: &str) {
        let fill = ptr.add(size).add(TAIL_FILL_OFF);
        // `!=` and not `<`, which the `heap-band-notail` shape makes a
        // comparison against a type's minimum and clippy denies. The step
        // divides the band, asserted beside the constant, so the two are the
        // same loop.
        let mut off = 0;
        while off != TAIL_FILL {
            let word = fill.add(off).cast::<u64>().read_unaligned();
            if word != FILL_WORD {
                let band = band_words(fill);
                panic!(
                    "HEAP TRIPWIRE ({site}): {ptr:?} was written past its {size}-byte \
                     allocation — tail band word +{} is {word:#018x}; the band reads \
                     [{:#018x}, {:#018x}, {:#018x}, {:#018x}] and its first byte stands at \
                     {:#018x}",
                    TAIL_FILL_OFF + off,
                    band[0], band[1], band[2], band[3], fill as u64,
                );
            }
            off += 8;
        }
    }

    /// The first four words of a band, for the message a fire produces.
    ///
    /// **One dirty word does not say what wrote it and four often do.** The one
    /// band this instrument has ever caught firing held `stack_top + 16` at
    /// `stack_top` of a task kernel stack — which is what a `#GP`/interrupt
    /// frame's saved `RSP` looks like if the entry happened with `rsp` sixteen
    /// bytes above the stack, and if it is one then the word above it is a
    /// stack segment selector and the two above *that* are still fill. That
    /// reading was unfalsifiable from a report that printed one word. A shape
    /// whose band is shorter than four words reads `FILL_WORD` past the end of
    /// it, which is what an untouched word reads as anyway.
    ///
    /// # Safety
    /// `fill` is the first byte of a band this module armed.
    unsafe fn band_words(fill: *const u8) -> [u64; 4] {
        let mut out = [FILL_WORD; 4];
        let mut i = 0;
        while i < 4 && (i + 1) * 8 <= TAIL_FILL {
            out[i] = fill.add(i * 8).cast::<u64>().read_unaligned();
            i += 1;
        }
        out
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

/// The sweep: every live band in the heap, read at a point of the caller's
/// choosing rather than at the free that may never come.
///
/// **What it is for, and it is one question.** `heap-tripwire` bands both sides
/// of every allocation and the class stopped happening under it — 7,205 boots
/// against a rate that expected twelve deaths — while **no band ever fired**.
/// That silence proves nothing on its own, because a band is only ever read at
/// `dealloc` and, for the running task's kernel stack, at every pass: an
/// allocation live from early boot to `compositor: ready` is never freed inside
/// a boot, so its band is never read at all. A write that the bands *absorb* is
/// therefore indistinguishable from one they *displace*. This reads them, so
/// "no band fired" becomes either a named victim or a real negative.
///
/// **No registry, and that is the design.** [`KernelPageSource`] is the only
/// thing that hands `dlmalloc` a page, so the kernel already knows every 2 MiB
/// page the heap owns; a band is self-describing (`tripwire::RECORD`), so a
/// walk of those pages at eight-byte alignment finds every live band in the
/// machine with no per-allocation bookkeeping, no intrusive list and no second
/// lock. The alternative — a `prev`/`next` in a widened head band — costs a
/// list operation on every allocation in the kernel and changes the placement
/// the arms are comparing.
///
/// **The lock is held across the walk and dropped before the panic.** Held,
/// because `dlmalloc` carving or coalescing a chunk under the walk would read
/// as corruption; dropped, because `KernelAllocator`'s DESIGN RULE is exactly
/// that a panic under that lock abandons the heap with the lock held, and the
/// report would never reach the wire. So the walk answers with a value and the
/// panic happens outside it.
///
/// **This reader is not free of the machine it reads, and that is measured.**
/// Two twelve-wide boot storms, same tree, same bands, differing only in
/// whether this was compiled in: without it, 0 kernel deaths in 7,211 boots;
/// with it, 9 in 7,040, which is the unbanded baseline's own rate. The chance
/// that all nine of those events land on the swept side under one common rate
/// is 1.8e-3. It compiles no decision, so it is not causing corruption — what
/// it does is take `dlmalloc`'s lock on the pass path every 25 ms and hold it
/// for a walk of every heap page, and that is enough to move the rate from
/// "does not happen" to "happens". `sched::driver`'s `sched-tripwire` shadow
/// has the same shape and the same unexplained 7.2x. **A kernel carrying this
/// is not the kernel a rate was measured on**; hold the instrument set fixed
/// across any two arms that are being compared.
#[cfg(feature = "heap-sweep")]
pub fn sweep(site: &str) {
    SWEPT.0.fetch_add(1, Ordering::Relaxed);
    let outcome = {
        let _held = ALLOCATOR.dlmalloc.lock();
        walk()
    };
    let Some(bad) = outcome else { return };
    let payload = bad.payload as *const u8;
    panic!(
        "HEAP TRIPWIRE (sweep at {site}): {} of the live allocation at {payload:?} \
         ({} bytes, align {}) holds {:#018x} at byte +{}, want {:#018x}. Nothing freed this \
         allocation and nothing may write outside it, so a neighbour of it overran — the \
         dirty bytes are what the writer wrote and the size and alignment are the victim's. \
         Sweeps so far: {}, live bands on this walk: {}.",
        bad.what,
        bad.size,
        bad.align,
        bad.found,
        bad.off,
        bad.want,
        SWEPT.0.load(Ordering::Relaxed),
        SWEPT.1.load(Ordering::Relaxed),
    );
}

/// Hold `dlmalloc`'s lock for `ns` of guest time and do nothing with it.
///
/// **The sweep's cost without the sweep.** `heap-sweep` moved this class's rate
/// from nothing to the unbanded baseline while compiling no decision, and the
/// two things it does that a bandless kernel does not are *spend time on the
/// pass path* and *hold this lock while it does*. `sched-tripwire` already
/// amplifies by spending time on the same path and takes no lock at all, so the
/// lock has never been separated from the delay. This is the half with the lock;
/// `sched::driver`'s `pass-spin` is the same visit without it, and the pair is
/// one experiment in two arms.
///
/// Nothing is read or written through the guard, so a kernel carrying this
/// allocates exactly what one without it allocates — later.
#[cfg(feature = "heap-lockspin")]
pub fn hold_lock(ns: u64) {
    let _held = ALLOCATOR.dlmalloc.lock();
    crate::sched::driver::spin_for(ns);
}

/// How many sweeps have run and how many live records the last one saw, for
/// [`crate::hw::report_contexts`]. `None` from a kernel that carries no sweep.
#[cfg(feature = "heap-sweep")]
pub fn sweep_stats() -> Option<(u64, u64, bool)> {
    Some((
        SWEPT.0.load(Ordering::Relaxed),
        SWEPT.1.load(Ordering::Relaxed),
        pages::OVERFLOWED.load(Ordering::Relaxed),
    ))
}

/// See the `heap-sweep` arm above.
#[cfg(not(feature = "heap-sweep"))]
pub fn sweep_stats() -> Option<(u64, u64, bool)> {
    None
}

/// Sweeps run, and live records the last one walked.
#[cfg(feature = "heap-sweep")]
static SWEPT: (core::sync::atomic::AtomicU64, core::sync::atomic::AtomicU64) =
    (core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0));

/// One band byte that is not what [`tripwire::arm`] wrote. Plain data, because
/// the walk that produces it runs under `dlmalloc.lock()` and may not panic.
#[cfg(feature = "heap-sweep")]
#[derive(Clone, Copy)]
struct Bad {
    payload: u64,
    size: u64,
    align: u64,
    what: &'static str,
    off: usize,
    found: u64,
    want: u64,
}

/// Every heap page the kernel has handed `dlmalloc`.
///
/// Maintained by [`KernelPageSource`], so it is written under `dlmalloc.lock()`
/// and read by the sweep under the same one.
#[cfg(feature = "heap-sweep")]
mod pages {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// 512 pages is 1 GiB of kernel heap. Measured, this boot reaches single
    /// digits; the table is sized so the walk stays complete under any
    /// workload this kernel has, not so it is tight.
    const MAX: usize = 512;

    pub static BASES: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];

    /// Set if the table ever filled, and reported by
    /// [`crate::hw::report_contexts`]: past that point the sweep covers some of
    /// the heap rather than all of it, which is a weaker instrument and worth
    /// knowing about, but it is not a reason to kill the machine.
    pub static OVERFLOWED: AtomicBool = AtomicBool::new(false);

    /// DESIGN RULE, inherited from [`super::KernelPageSource`]: this runs
    /// inside `dlmalloc.lock()` and may not panic.
    pub fn add(base: u64) {
        for slot in &BASES {
            if slot.compare_exchange(0, base, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                return;
            }
        }
        OVERFLOWED.store(true, Ordering::Relaxed);
    }

    /// A page the table never took is simply not there to remove.
    pub fn remove(base: u64) {
        for slot in &BASES {
            if slot.compare_exchange(base, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                return;
            }
        }
    }
}

/// Walk every heap page for band records and read each one's bands back.
///
/// **It answers rather than asserts.** Its caller holds `dlmalloc.lock()`.
#[cfg(feature = "heap-sweep")]
fn walk() -> Option<Bad> {
    let mut seen = 0u64;
    let mut bad = None;
    for slot in &pages::BASES {
        let base = slot.load(Ordering::Relaxed);
        if base == 0 {
            continue;
        }
        // SAFETY: `base` is a 2 MiB page `pmm` handed this heap and has not
        // taken back — `KernelPageSource::free` clears the slot before it
        // returns the page — so the whole span is mapped in the direct map and
        // this reads it as words, which any byte pattern is a valid one of.
        let words = unsafe {
            core::slice::from_raw_parts(base as *const u64, PAGE_2M as usize / 8)
        };
        for (i, word) in words.iter().enumerate() {
            let record = base + (i * 8) as u64;
            if *word != tripwire::open_word(record) {
                continue;
            }
            // SAFETY: `record` is a word inside this page, and `identify`
            // reads no further than the three words above it before it has
            // decided the four are a record.
            let Some(found) = (unsafe { identify(record, base) }) else { continue };
            seen += 1;
            if bad.is_none() {
                // SAFETY: `identify` has placed the whole banded allocation
                // inside this page.
                bad = unsafe { inspect(found) };
            }
        }
    }
    SWEPT.1.store(seen, Ordering::Relaxed);
    bad
}

/// A record the walk has decided really is one, resolved to the allocation
/// around it.
#[cfg(feature = "heap-sweep")]
#[derive(Clone, Copy)]
struct Found {
    payload: *mut u8,
    size: usize,
    align: usize,
}

/// Is the word at `record` the start of a real record, and where is the
/// allocation it describes?
///
/// **Every test here exists to make a false positive impossible, not merely
/// unlikely.** The walk has no registry to check a candidate against: what it
/// has is a word that matched, and the walk runs over payload bytes as well as
/// band bytes — over kernel stacks, which are 128 KiB heap allocations, and
/// over anything else the kernel stores. So a candidate has to be structurally
/// a record before its bands are read:
///
/// * both magic words, each tied to the address it stands at, so a copy of one
///   somewhere else is not one;
/// * a size no larger than a heap allocation can be, and an alignment that is a
///   power of two the allocator can serve;
/// * a payload actually aligned to the alignment the record claims;
/// * the whole banded allocation inside the page the walk is in.
///
/// A candidate that fails any of these is **skipped and not reported**. A
/// record whose own magic a stray write destroyed is invisible to this walk,
/// which is a real limit and the right trade: `dealloc` and the pass-time check
/// read the record against the layout its *holder* names, which is the check
/// that catches that case, and the thing this walk exists to find is a dirty
/// *fill* band on an allocation nobody will free before the boot ends.
///
/// # Safety
/// `record` is a word inside the 2 MiB page at `base`.
#[cfg(feature = "heap-sweep")]
unsafe fn identify(record: u64, base: u64) -> Option<Found> {
    let page = base..base + PAGE_2M;
    if record + tripwire::RECORD as u64 > page.end {
        return None;
    }
    let r = record as *const u64;
    if r.add(3).read_unaligned() != tripwire::close_word(record) {
        return None;
    }
    let size = r.add(1).read_unaligned();
    let align = r.add(2).read_unaligned();
    if size > MAX_HEAP_ALLOC as u64 || !align.is_power_of_two() || align > PAGE_2M {
        return None;
    }
    let (size, align) = (size as usize, align as usize);
    let head = tripwire::head(align);
    let payload = if tripwire::HEAD_MIN == 0 {
        record.checked_sub(size as u64)?
    } else {
        record + tripwire::RECORD as u64
    };
    if payload % align as u64 != 0 {
        return None;
    }
    let bottom = payload.checked_sub(head as u64)?;
    let top = payload.checked_add((size + tripwire::TAIL) as u64)?;
    if bottom < page.start || top > page.end {
        return None;
    }
    Some(Found { payload: payload as *mut u8, size, align })
}

/// The bands of one identified allocation.
///
/// # Safety
/// `found` came from [`identify`], which placed its bands inside a mapped page.
#[cfg(feature = "heap-sweep")]
unsafe fn inspect(found: Found) -> Option<Bad> {
    let Found { payload, size, align } = found;
    let bad = |what, off, word| {
        Some(Bad {
            payload: payload as u64,
            size: size as u64,
            align: align as u64,
            what,
            off,
            found: word,
            want: tripwire::FILL_WORD,
        })
    };
    // The tail band's fill, then the head band's. Both are what a bounded
    // overrun lands in, and neither is read anywhere else for an allocation
    // that is never freed.
    let tail = payload.add(size).add(tripwire::TAIL_FILL_OFF);
    // `!=` for `check_tail_fill`'s reason.
    let mut off = 0;
    while off != tripwire::TAIL_FILL {
        let word = tail.add(off).cast::<u64>().read_unaligned();
        if word != tripwire::FILL_WORD {
            return bad("the tail band", tripwire::TAIL_FILL_OFF + off, word);
        }
        off += 8;
    }
    let head = tripwire::head(align);
    let bottom = payload.sub(head);
    let fill_bytes = head.saturating_sub(tripwire::RECORD);
    let mut off = 0;
    while off < fill_bytes {
        let word = bottom.add(off).cast::<u64>().read_unaligned();
        if word != tripwire::FILL_WORD {
            return bad("the head band", off, word);
        }
        off += 8;
    }
    None
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
