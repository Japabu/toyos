//! Event-driven I/O polling on an [inbox](toyos_abi::inbox).

use core::sync::atomic::{AtomicU32, Ordering};
use toyos_abi::RawHandle;
use toyos_abi::syscall;
use toyos_abi::inbox::{
    Submission, Completion, RingHeader, RingLayout,
    OP_WATCH, SUBMISSION_RING_OFF, COMPLETION_RING_OFF, SUBMISSIONS_OFF,
};
use crate::AsHandle;

pub use toyos_abi::inbox::{READABLE, WRITABLE};

/// The inbox page, and the only thing in this crate that touches it.
///
/// **The kernel is the second writer of every byte below, so no Rust reference
/// covers any of it.** `inbox::post_completion` stores a whole [`Completion`]
/// and publishes the tail; `claim_submission` reads a submission slot and
/// advances the submission head. A `&T` carries `dereferenceable` into LLVM —
/// and for a `T` with no interior mutability `noalias` and `readonly` too — so
/// a `&RingHeader` here was a data race on `ring_size` whatever was then read
/// through it, and `&mut Submission`/`&Completion` (all integers, therefore
/// `Freeze`) were borrows the compiler was entitled to fold, hoist or split
/// against a kernel that was writing the same bytes.
///
/// So a ring header is reached one atomic word at a time (`AtomicU32::from_ptr`,
/// which is the only way to do an atomic operation on memory Rust does not own
/// and is *sound* over shared memory: an atomic's `UnsafeCell` is what withdraws
/// `noalias`/`readonly`), and an entry is one `ptr::write` or one
/// `read_volatile` of the whole struct — one access, not one the compiler may
/// split, fold or repeat.
///
/// This is the mirror of `kernel/src/inbox.rs`'s accessor block, fixed the same
/// way on 2026-08-22; the field offsets come from `offset_of!` at both ends and
/// `toyos_abi::inbox`'s `RING_*_OFF` constants are what a reordering meets.
/// `ring_size` has no accessor at either end: both hold the sizes themselves
/// rather than reading them back out of a page the other side can write.
struct Rings {
    base: *mut u8,
    submission_ring_size: u32,
    completion_ring_size: u32,
}

impl Rings {
    /// Read the layout the kernel wrote at offset 0, and take the ring sizes
    /// from it.
    ///
    /// One `read_volatile` of the whole struct rather than a `&RingLayout`.
    /// The kernel writes this before the page is mapped and never again
    /// (`SharedMemObject::phys_before_mapping` is what enforces the order), so
    /// there is no race to lose here — but a rule with an exception "for the
    /// field nobody rewrites" is a rule that has to be re-derived every time
    /// somebody adds a field.
    ///
    /// # Safety
    ///
    /// `base` must be the address of a live inbox page, laid out by the kernel
    /// and mapped for this process's lifetime — which is what
    /// [`syscall::inbox_setup`] answers with.
    unsafe fn over(base: *mut u8) -> Self {
        // SAFETY: the caller guarantees `base` is a live, kernel-laid-out inbox
        // page. `RingLayout` is `#[repr(C)]` over integers, so every bit
        // pattern is a value, and offset 0 of a 2 MiB page is aligned for it.
        let layout = unsafe { (base as *const RingLayout).read_volatile() };
        Self {
            base,
            submission_ring_size: layout.submission_ring_size,
            completion_ring_size: layout.completion_ring_size,
        }
    }

    /// One atomic word of one ring header.
    ///
    /// `&AtomicU32` and never `&RingHeader` — the type's own header says why.
    fn ring_word(&self, ring_off: u64, field_off: usize) -> &AtomicU32 {
        // SAFETY: `base` is the whole 2 MiB inbox page, live for this
        // `Poller`'s lifetime, and both ring offsets (0x1000 and 0x2000) plus
        // `size_of::<RingHeader>()` are far inside it. The offsets are
        // page-aligned and `field_off` is `offset_of!` over a `#[repr(C)]`
        // struct of `u32`-sized fields, so the result is 4-aligned, which is
        // what `AtomicU32` needs. The `&AtomicU32` is sound over a page the
        // kernel writes because an atomic is exactly the type that says so:
        // its `UnsafeCell` withdraws `noalias`/`readonly`, and every access
        // through it is an atomic operation.
        //
        // Irreducible: `AtomicU32::from_ptr` is the only way to perform an
        // atomic operation on memory Rust does not own, and a shared ring is
        // memory Rust cannot own.
        unsafe { AtomicU32::from_ptr(self.base.add(ring_off as usize + field_off) as *mut u32) }
    }

    fn submission_head(&self) -> &AtomicU32 {
        self.ring_word(SUBMISSION_RING_OFF, core::mem::offset_of!(RingHeader, head))
    }

    fn submission_tail(&self) -> &AtomicU32 {
        self.ring_word(SUBMISSION_RING_OFF, core::mem::offset_of!(RingHeader, tail))
    }

    fn completion_head(&self) -> &AtomicU32 {
        self.ring_word(COMPLETION_RING_OFF, core::mem::offset_of!(RingHeader, head))
    }

    fn completion_tail(&self) -> &AtomicU32 {
        self.ring_word(COMPLETION_RING_OFF, core::mem::offset_of!(RingHeader, tail))
    }

    fn completion_dropped(&self) -> &AtomicU32 {
        self.ring_word(COMPLETION_RING_OFF, core::mem::offset_of!(RingHeader, dropped))
    }

    /// Put one whole submission in the slot `index` names.
    ///
    /// One store of the whole entry, before the tail publishes it. A
    /// `&mut Submission` was a borrow the compiler could assume exclusive over
    /// a page the kernel also maps, and the five field assignments behind it
    /// were five stores it was free to reorder against each other.
    fn write_submission(&self, index: u32, entry: Submission) {
        // SAFETY: `index` is masked by `submission_ring_size` at the one call
        // site, and that size is a power of two no greater than
        // `MAX_HANDLES` (256), so the furthest entry ends at `SUBMISSIONS_OFF`
        // (0x4000) + 256 * `size_of::<Submission>()`, inside the 2 MiB page.
        // `SUBMISSIONS_OFF` is page-aligned and `Submission` is 8-aligned with
        // a size that is a multiple of 8, so every entry is aligned. Nothing
        // else in this process writes the page — `Poller` owns it — and the
        // kernel only reads a slot the tail below has published.
        //
        // Irreducible: the entry is at a fixed offset in shared memory and the
        // safe spelling of a store into one is a reference, which is the
        // borrow this type refuses.
        unsafe {
            (self.base.add(
                SUBMISSIONS_OFF as usize + index as usize * core::mem::size_of::<Submission>(),
            ) as *mut Submission)
                .write(entry);
        }
    }

    /// One completion entry, copied out.
    ///
    /// By value and by `read_volatile`, so what the caller goes on to decide
    /// with is a snapshot it took once — the mirror of the kernel's
    /// `submission_at`.
    fn completion_at(&self, index: u32) -> Completion {
        // SAFETY: `index` is masked by `completion_ring_size` at the one call
        // site, which is twice the submission ring and so at most 512 entries
        // past `COMPLETION_RING_OFF` + `size_of::<RingHeader>()` — inside the
        // 2 MiB page, and 8-aligned because that offset is 16 past a page
        // boundary and `Completion` is 16 bytes. `Completion` is all integers,
        // so every bit pattern the kernel could have left is a value.
        unsafe {
            (self.base.add(
                COMPLETION_RING_OFF as usize
                    + core::mem::size_of::<RingHeader>()
                    + index as usize * core::mem::size_of::<Completion>(),
            ) as *const Completion)
                .read_volatile()
        }
    }

    /// Number of pending submissions (not yet flushed to the kernel).
    fn pending(&self) -> u32 {
        let head = self.submission_head().load(Ordering::Acquire);
        let tail = self.submission_tail().load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }
}

/// An inbox, for watching handles for readiness.
///
/// Owns the inbox handle and shared memory mapping. Submissions are batched
/// and flushed on [`wait`](Self::wait).
///
/// **Deliberately not called `Inbox`.** The ABI object is the kernel's
/// `object::inbox` and a task's bounded record ring is `completion::inbox`;
/// a third `Inbox` in userland would undo exactly the separation those two
/// names keep. `Poller` is what this type does for its caller.
///
/// **A poller has a declared capacity and cannot lose a completion inside it.**
/// [`new`](Self::new) takes the number of handles the caller will watch at once
/// and sizes both rings from it: the submission ring holds them all, so no
/// batch is ever flushed mid-registration, and the kernel's completion ring —
/// always twice the submission ring — holds the most completions that can exist
/// between two [`wait`](Self::wait) calls, which is two per watched handle (a
/// registration left over from the previous round firing, and this round's
/// registration finding the handle ready).
///
/// Going past the capacity is a contract violation and panics, because it is
/// the caller's own bug and the alternative is the failure this replaced: the
/// kernel silently dropping a completion and the caller blocking forever on
/// readiness that was thrown away. The capacity is the number of handles, not
/// the number of calls — re-registering the same handle within a round is
/// deduplicated by the kernel but still counts here, so declare the set.
///
/// The doc that used to be here said the kernel "asserts rather than
/// overflows", which stopped being true when `post_cqe` switched to recording
/// a drop and returning: prose asserting a property of another component that
/// nobody re-checked. [`wait`](Self::wait) still reads the kernel's drop
/// counter — an assert that should now be unreachable, kept because that is
/// the shape a fail-fast check is supposed to have.
pub struct Poller {
    inbox: RawHandle,
    rings: Rings,
    capacity: u32,
}

// Safety: the base pointer is process-local shared memory mapped from the
// kernel. It is only ever reached through `Rings`, which takes no reference
// over it: atomics for the shared words, whole-value volatile copies for
// everything else.
unsafe impl Send for Poller {}
unsafe impl Sync for Poller {}

impl Poller {
    /// Widest handle set one poller can carry — the kernel's deepest
    /// submission ring, `MAX_SQ_DEPTH` in `kernel/src/io_uring.rs`. A caller
    /// that must bound its own watched set has to bound it below this.
    pub const MAX_HANDLES: u32 = 256;

    /// Create a poller for `capacity` simultaneously watched handles.
    ///
    /// `capacity` is a declaration, not a hint: the rings are rounded up to the
    /// power of two that holds it, and registering past it panics. A capacity
    /// above [`MAX_HANDLES`] is refused for the same reason — it used to be
    /// clamped, which handed the caller a ring smaller than the set it just
    /// said it had and made the loss reachable while looking like a success.
    pub fn new(capacity: u32) -> Self {
        assert!(
            capacity >= 1 && capacity <= Self::MAX_HANDLES,
            "Poller::new: {capacity} handles is outside 1..={}; \
             bound the watched set below the kernel's deepest ring",
            Self::MAX_HANDLES,
        );
        let entries = capacity.next_power_of_two();
        // The inbox owns its page and the kernel maps it: one call, and no
        // second lifetime for a mapping that is only ever this inbox's.
        let (inbox, base) = unsafe { syscall::inbox_setup(entries) }
            .expect("Poller::new: inbox_setup failed");
        // SAFETY: `base` is what `inbox_setup` just answered with — the
        // address of this process's own inbox page, laid out by the kernel
        // before it was mapped and unmapped only when the handle closes, which
        // is this `Poller`'s `Drop`.
        let rings = unsafe { Rings::over(base) };
        // The whole point of the sizing: `capacity` registrations fit the
        // submission ring with no mid-batch flush, and the completions they can
        // produce fit the completion ring.
        assert!(
            rings.submission_ring_size >= capacity && rings.completion_ring_size >= 2 * capacity,
            "Poller::new: kernel built {}/{} rings for {capacity} handles",
            rings.submission_ring_size,
            rings.completion_ring_size,
        );
        Self { inbox, rings, capacity }
    }

    /// Watch the given handle for readiness.
    ///
    /// `flags` are [`READABLE`] / [`WRITABLE`].
    /// `token` is returned in completions to identify which handle is ready.
    pub fn watch(&self, handle: &impl AsHandle, flags: u32, token: u64) {
        self.watch_raw(handle.as_handle(), flags, token);
    }

    /// Watch a raw handle for readiness.
    ///
    /// Prefer [`watch`](Self::watch) when you have a typed handle.
    pub fn watch_raw(&self, handle: RawHandle, flags: u32, token: u64) {
        // A panic, because this is first-party code exceeding a bound it
        // declared itself. There used to be a mid-batch flush here instead;
        // that is what made completions reachable while the caller was still
        // registering, and past the completion ring the kernel dropped them
        // and the caller blocked forever on readiness it had been told about.
        // With the ring sized for `capacity` this is unreachable.
        assert!(
            self.pending() < self.capacity,
            "Poller: {} handles registered since the last wait(), capacity is {}",
            self.pending(),
            self.capacity,
        );
        let tail = self.rings.submission_tail().load(Ordering::Acquire);
        let idx = tail & (self.rings.submission_ring_size - 1);
        self.rings.write_submission(
            idx,
            Submission {
                op: OP_WATCH,
                handle,
                op_flags: flags,
                token,
                ..Submission::default()
            },
        );
        self.rings.submission_tail().store(tail.wrapping_add(1), Ordering::Release);
    }

    /// Number of pending submissions (not yet flushed to the kernel).
    pub fn pending(&self) -> u32 {
        self.rings.pending()
    }

    /// Hand the queued submissions to the kernel.
    ///
    /// The `expect` is sound because every error `inbox_submit` can report is
    /// about an argument this type owns — an over-deep batch, or a handle that
    /// is not this poller's inbox. Nothing a peer process does reaches it: a
    /// timeout or an empty completion ring is `Ok`.
    fn submit(&self, min_complete: u32, timeout_nanos: u64) {
        let to_submit = self.pending();
        syscall::inbox_submit(self.inbox, to_submit, min_complete, timeout_nanos)
            .expect("Poller::submit: inbox_submit rejected the batch");
    }

    /// Submit pending entries and wait for completions.
    ///
    /// Blocks until at least `min_complete` completions are ready or `timeout_nanos`
    /// elapses. Calls `f` for each completed token.
    pub fn wait(&self, min_complete: u32, timeout_nanos: u64, mut f: impl FnMut(u64)) {
        self.submit(min_complete, timeout_nanos);
        self.drain(&mut f);
    }

    /// Read every completion the kernel has published, oldest first.
    ///
    /// Split from [`wait`](Self::wait) because it is the half that is a pure
    /// function of the page: a host test can hand it a fake one.
    fn drain(&self, f: &mut impl FnMut(u64)) {
        // Unreachable, and kept for that reason: `capacity` bounds the
        // registrations and the rings are sized from `capacity`, so nothing a
        // conforming caller does can make the kernel drop a completion here.
        // The counter is cumulative and never cleared, so if the reasoning is
        // wrong this fires and stays fired instead of turning into a caller
        // blocked forever on readiness that was thrown away — which is what it
        // was before anyone read this field at all.
        let dropped = self.rings.completion_dropped().load(Ordering::Relaxed);
        assert_eq!(
            dropped, 0,
            "Poller: the kernel dropped {dropped} completion(s) with capacity {} \
             and rings {}/{} — the sizing rule is wrong, not the caller.",
            self.capacity, self.rings.submission_ring_size, self.rings.completion_ring_size,
        );

        loop {
            let head = self.rings.completion_head().load(Ordering::Acquire);
            let tail = self.rings.completion_tail().load(Ordering::Acquire);
            if head == tail {
                break;
            }
            let idx = head & (self.rings.completion_ring_size - 1);
            let completion = self.rings.completion_at(idx);
            // Do not filter on `completion.result`. A negative result is the
            // kernel saying the registration is over and will never fire
            // (`cancel_by_source` posts `-NotFound` when a watched handle
            // closes, i.e. on any peer disconnect), and the caller must react
            // to that exactly as to readiness — by looking at the handle again.
            // A zero result is meaningful too: `OP_ACCEPT` reports handle 0
            // that way.
            f(completion.token);
            self.rings.completion_head().store(head.wrapping_add(1), Ordering::Release);
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        syscall::close(self.inbox);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toyos_abi::inbox::{
        RING_DROPPED_OFF, RING_HEAD_OFF, RING_SIZE_OFF, RING_TAIL_OFF,
    };

    /// A page the "kernel" and the test both write, standing in for the one
    /// `SYS_INBOX_SETUP` maps. `u64` cells so it is 8-aligned, which is what
    /// `Submission`, `Completion` and `RingLayout` need.
    struct FakePage(Vec<u64>);

    /// Big enough for the layout, both ring headers and a submission array of
    /// `MAX_HANDLES` entries: 0x4000 + 256 * 40, rounded up.
    const PAGE_BYTES: usize = 0x8000;

    impl FakePage {
        fn new(submission_ring_size: u32, completion_ring_size: u32) -> Self {
            let mut page = Self(vec![0u64; PAGE_BYTES / 8]);
            // The kernel's `write_ring_page`, in the test's own words: the
            // layout at 0 and a `ring_size` in each header.
            let base = page.base();
            // SAFETY: `base` is this `Vec`'s own storage, 8-aligned and
            // `PAGE_BYTES` long, and nothing else refers to it here.
            unsafe {
                (base as *mut RingLayout).write(RingLayout {
                    submission_ring_off: SUBMISSION_RING_OFF,
                    completion_ring_off: COMPLETION_RING_OFF,
                    submissions_off: SUBMISSIONS_OFF,
                    submission_ring_size,
                    completion_ring_size,
                    features: 0,
                    _pad: 0,
                });
            }
            page.put(SUBMISSION_RING_OFF as usize + RING_SIZE_OFF, submission_ring_size);
            page.put(COMPLETION_RING_OFF as usize + RING_SIZE_OFF, completion_ring_size);
            page
        }

        fn base(&mut self) -> *mut u8 {
            self.0.as_mut_ptr() as *mut u8
        }

        /// Write one `u32` at a byte offset, the way the kernel would.
        fn put(&mut self, at: usize, value: u32) {
            let base = self.base();
            // SAFETY: `at` is inside `PAGE_BYTES` at every call site below and
            // 4-aligned, and the storage is this `Vec`'s.
            unsafe { (base.add(at) as *mut u32).write_volatile(value) };
        }

        /// Read one `u32` at a byte offset.
        fn get(&mut self, at: usize) -> u32 {
            let base = self.base();
            // SAFETY: as `put`.
            unsafe { (base.add(at) as *const u32).read_volatile() }
        }

        /// Post a completion the way `inbox::post_completion` does: the whole
        /// entry, then a release store of the tail.
        fn post(&mut self, index: u32, entry: Completion) {
            let base = self.base();
            // SAFETY: `index` is under the ring size at every call site, so
            // the entry is inside `PAGE_BYTES`; `COMPLETION_RING_OFF` is
            // 8-aligned and `Completion` is 16 bytes.
            unsafe {
                (base.add(
                    COMPLETION_RING_OFF as usize
                        + core::mem::size_of::<RingHeader>()
                        + index as usize * core::mem::size_of::<Completion>(),
                ) as *mut Completion)
                    .write(entry);
            }
        }
    }

    fn rings(page: &mut FakePage) -> Rings {
        // SAFETY: the page is laid out exactly as the kernel lays one out and
        // outlives the `Rings` at every call site.
        unsafe { Rings::over(page.base()) }
    }

    /// The accessors land on the offsets `toyos_abi` states, which is what the
    /// kernel's own `offset_of!`s resolve to.
    ///
    /// The two ends never meet in one binary — the kernel is not in the host
    /// workspace — so this is the SDK half of that agreement: the constants
    /// are the claim, `toyos_abi::inbox`'s `const _`s hold the kernel's
    /// `offset_of!` to them, and this holds the SDK's accessors to them.
    #[test]
    fn every_accessor_lands_on_the_abi_offset() {
        let mut page = FakePage::new(4, 8);
        let base = page.base() as usize;
        let r = rings(&mut page);
        let at = |w: &AtomicU32| w as *const AtomicU32 as usize - base;
        assert_eq!(at(r.submission_head()), SUBMISSION_RING_OFF as usize + RING_HEAD_OFF);
        assert_eq!(at(r.submission_tail()), SUBMISSION_RING_OFF as usize + RING_TAIL_OFF);
        assert_eq!(at(r.completion_head()), COMPLETION_RING_OFF as usize + RING_HEAD_OFF);
        assert_eq!(at(r.completion_tail()), COMPLETION_RING_OFF as usize + RING_TAIL_OFF);
        assert_eq!(at(r.completion_dropped()), COMPLETION_RING_OFF as usize + RING_DROPPED_OFF);
    }

    /// A submission is one whole entry at the slot the tail names, in the
    /// bytes the kernel's `submission_at` reads.
    #[test]
    fn a_submission_is_the_whole_entry_where_the_kernel_reads_it() {
        let mut page = FakePage::new(4, 8);
        {
            let r = rings(&mut page);
            r.write_submission(
                2,
                Submission {
                    op: OP_WATCH,
                    handle: RawHandle(9),
                    op_flags: READABLE,
                    token: 0xfeed,
                    ..Submission::default()
                },
            );
            r.submission_tail().store(3, Ordering::Release);
        }
        let at = SUBMISSIONS_OFF as usize + 2 * core::mem::size_of::<Submission>();
        let base = page.base();
        // SAFETY: the slot is inside `PAGE_BYTES` and 8-aligned.
        let entry = unsafe { (base.add(at) as *const Submission).read_volatile() };
        assert_eq!(entry.op, OP_WATCH);
        assert_eq!(entry.handle, RawHandle(9));
        assert_eq!(entry.op_flags, READABLE);
        assert_eq!(entry.token, 0xfeed);
        assert_eq!(page.get(SUBMISSION_RING_OFF as usize + RING_TAIL_OFF), 3);
    }

    /// **The negative control for the borrow this file removed.**
    ///
    /// A "kernel" writes the completion tail *between* the two reads a drain
    /// makes, which is exactly what `post_completion` does on another CPU. The
    /// header is reached one `&AtomicU32` at a time, so the second read sees
    /// the new value and the second batch is drained. Restore the
    /// `&*(base.add(COMPLETION_RING_OFF) as *const RingHeader)` this replaced
    /// and the drain holds one snapshot of the header across the loop: the
    /// borrow is `Freeze`, LLVM is entitled to keep the first `tail` in a
    /// register, and the second batch is never seen.
    #[test]
    fn a_tail_the_kernel_publishes_mid_drain_is_observed() {
        let mut page = FakePage::new(4, 8);
        page.post(0, Completion { token: 11, result: 1, flags: 0 });
        page.put(COMPLETION_RING_OFF as usize + RING_TAIL_OFF, 1);

        let mut seen: Vec<u64> = Vec::new();
        {
            let r = rings(&mut page);
            loop {
                let head = r.completion_head().load(Ordering::Acquire);
                let tail = r.completion_tail().load(Ordering::Acquire);
                if head == tail {
                    break;
                }
                let idx = head & (r.completion_ring_size - 1);
                seen.push(r.completion_at(idx).token);
                r.completion_head().store(head.wrapping_add(1), Ordering::Release);
                // The kernel, on another CPU, one completion later.
                if seen.len() == 1 {
                    // SAFETY: slot 1 is inside the 8-entry ring.
                    unsafe {
                        (r.base.add(
                            COMPLETION_RING_OFF as usize
                                + core::mem::size_of::<RingHeader>()
                                + core::mem::size_of::<Completion>(),
                        ) as *mut Completion)
                            .write(Completion { token: 22, result: 1, flags: 0 });
                    }
                    r.completion_tail().store(2, Ordering::Release);
                }
            }
        }
        assert_eq!(seen, vec![11, 22]);
        // The head the kernel reads back is the one the drain published.
        assert_eq!(page.get(COMPLETION_RING_OFF as usize + RING_HEAD_OFF), 2);
    }

    /// The drop counter is where the kernel says it dropped one, and it is
    /// read out of the page rather than out of a snapshot of the header.
    #[test]
    fn the_drop_counter_is_read_from_the_page() {
        let mut page = FakePage::new(4, 8);
        page.put(COMPLETION_RING_OFF as usize + RING_DROPPED_OFF, 3);
        let r = rings(&mut page);
        assert_eq!(r.completion_dropped().load(Ordering::Relaxed), 3);
    }

    /// `pending` is the submission ring's own arithmetic, and it wraps.
    #[test]
    fn pending_counts_what_the_kernel_has_not_claimed() {
        let mut page = FakePage::new(4, 8);
        page.put(SUBMISSION_RING_OFF as usize + RING_HEAD_OFF, u32::MAX - 1);
        page.put(SUBMISSION_RING_OFF as usize + RING_TAIL_OFF, 1);
        let r = rings(&mut page);
        assert_eq!(r.pending(), 3);
    }
}
