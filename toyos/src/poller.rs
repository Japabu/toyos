//! Event-driven I/O polling via io_uring.

use core::sync::atomic::Ordering;
use toyos_abi::RawHandle;
use toyos_abi::syscall;
use toyos_abi::io_uring::{
    IoUringSqe, IoUringCqe, IoUringRingHeader, IoUringParams,
    IORING_OP_POLL_ADD, SQ_RING_OFF, CQ_RING_OFF, SQES_OFF,
};
use crate::AsHandle;

pub use toyos_abi::io_uring::{IORING_POLL_IN, IORING_POLL_OUT};

/// An io_uring instance for polling fd readiness.
///
/// Owns the ring fd and shared memory mapping. Submissions are batched
/// and flushed on [`wait`](Self::wait).
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
    ring_fd: RawHandle,
    base: *mut u8,
    capacity: u32,
    sq_size: u32,
    cq_size: u32,
}

// Safety: the base pointer is process-local shared memory mapped from the
// kernel. It is only accessed through atomic operations on the ring headers.
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
        let (ring_fd, shm_token) = syscall::io_uring_setup(entries)
            .expect("Poller::new: io_uring_setup failed");
        let base = unsafe { syscall::try_map_shared(shm_token) }
            .expect("Poller::new: map_shared failed");
        let params = unsafe { &*(base as *const IoUringParams) };
        let sq_size = params.sq_ring_size;
        let cq_size = params.cq_ring_size;
        // The whole point of the sizing: `capacity` registrations fit the
        // submission ring with no mid-batch flush, and the completions they can
        // produce fit the completion ring.
        assert!(sq_size >= capacity && cq_size >= 2 * capacity,
            "Poller::new: kernel built {sq_size}/{cq_size} rings for {capacity} handles");
        Self { ring_fd, base, capacity, sq_size, cq_size }
    }

    /// Submit a poll request for the given handle.
    ///
    /// `flags` are [`IORING_POLL_IN`] / [`IORING_POLL_OUT`].
    /// `token` is returned in completions to identify which handle is ready.
    pub fn poll_add(&self, handle: &impl AsHandle, flags: u32, token: u64) {
        self.poll_add_fd(handle.as_handle(), flags, token);
    }

    /// Submit a poll request for a raw fd.
    ///
    /// Prefer [`poll_add`](Self::poll_add) when you have a typed handle.
    pub fn poll_add_fd(&self, fd: RawHandle, flags: u32, token: u64) {
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
        let sq_hdr = unsafe {
            &*(self.base.add(SQ_RING_OFF as usize) as *const IoUringRingHeader)
        };
        let tail = sq_hdr.tail.load(Ordering::Acquire);
        let idx = tail & (self.sq_size - 1);
        let sqe = unsafe {
            &mut *(self.base.add(SQES_OFF as usize + idx as usize * core::mem::size_of::<IoUringSqe>()) as *mut IoUringSqe)
        };
        *sqe = IoUringSqe::default();
        sqe.op = IORING_OP_POLL_ADD;
        sqe.fd = fd;
        sqe.op_flags = flags;
        sqe.user_data = token;
        sq_hdr.tail.store(tail.wrapping_add(1), Ordering::Release);
    }

    /// Number of pending submissions (not yet flushed to the kernel).
    pub fn pending(&self) -> u32 {
        let sq_hdr = unsafe {
            &*(self.base.add(SQ_RING_OFF as usize) as *const IoUringRingHeader)
        };
        let head = sq_hdr.head.load(Ordering::Acquire);
        let tail = sq_hdr.tail.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    /// Hand the queued submissions to the kernel.
    ///
    /// The `expect` is sound because every error `io_uring_enter` can report is
    /// about an argument this type owns — an over-deep batch, or a ring id that
    /// is not this poller's fd. Nothing a peer process does reaches it: a
    /// timeout or an empty completion queue is `Ok`.
    fn submit(&self, min_complete: u32, timeout_nanos: u64) {
        let to_submit = self.pending();
        syscall::io_uring_enter(self.ring_fd, to_submit, min_complete, timeout_nanos)
            .expect("Poller::submit: io_uring_enter rejected the batch");
    }

    /// Submit pending entries and wait for completions.
    ///
    /// Blocks until at least `min_complete` completions are ready or `timeout_nanos`
    /// elapses. Calls `f` for each completed token.
    pub fn wait(&self, min_complete: u32, timeout_nanos: u64, mut f: impl FnMut(u64)) {
        self.submit(min_complete, timeout_nanos);

        let cq_hdr = unsafe {
            &*(self.base.add(CQ_RING_OFF as usize) as *const IoUringRingHeader)
        };

        // Unreachable, and kept for that reason: `capacity` bounds the
        // registrations and the rings are sized from `capacity`, so nothing a
        // conforming caller does can make the kernel drop a completion here.
        // The counter is cumulative and never cleared, so if the reasoning is
        // wrong this fires and stays fired instead of turning into a caller
        // blocked forever on readiness that was thrown away — which is what it
        // was before anyone read this field at all.
        let dropped = cq_hdr.dropped.load(Ordering::Relaxed);
        assert_eq!(
            dropped, 0,
            "Poller: the kernel dropped {dropped} completion(s) with capacity {} \
             and rings {}/{} — the sizing rule is wrong, not the caller.",
            self.capacity, self.sq_size, self.cq_size,
        );

        loop {
            let head = cq_hdr.head.load(Ordering::Acquire);
            let tail = cq_hdr.tail.load(Ordering::Acquire);
            if head == tail {
                break;
            }
            let idx = head & (self.cq_size - 1);
            let cqe = unsafe {
                &*(self.base.add(CQ_RING_OFF as usize + 16 + idx as usize * core::mem::size_of::<IoUringCqe>()) as *const IoUringCqe)
            };
            // Do not filter on `cqe.result`. A negative result is the kernel
            // saying the registration is over and will never fire (`remove_fd`
            // posts `-NotFound` when a watched handle closes, i.e. on any peer
            // disconnect), and the caller must react to that exactly as to
            // readiness — by looking at the handle again. A zero result is
            // meaningful too: `IORING_OP_ACCEPT` reports fd 0 that way.
            f(cqe.user_data);
            cq_hdr.head.store(head.wrapping_add(1), Ordering::Release);
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        syscall::close(self.ring_fd);
    }
}
