//! Event-driven I/O polling via io_uring.

use core::sync::atomic::Ordering;
use toyos_abi::Fd;
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
/// Registering more handles than the ring is deep is safe — the batch is
/// flushed and refilled — but it is not unlimited: each registration can
/// produce one completion, and the kernel's completion queue is twice the
/// submission ring. A caller that watches more than `2 × handles` distinct fds
/// between two [`wait`](Self::wait) calls is outside what this type can carry;
/// size it for the set instead.
///
/// This said the kernel "asserts rather than overflows", which stopped being
/// true when `post_cqe` switched to recording a drop and returning — prose
/// asserting a property of another component that nobody re-checked. Losing
/// the assert lost the diagnosis too: the overflow became silent. [`wait`](Self::wait)
/// reads the kernel's drop counter, so it is loud again.
pub struct Poller {
    ring_fd: Fd,
    base: *mut u8,
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

    /// Create a poller sized for `handles` concurrently watched handles.
    ///
    /// The kernel only builds power-of-two rings up to [`MAX_HANDLES`], so the
    /// request is rounded up and clamped rather than rejected. Registering more
    /// handles than the ring holds stays correct (see
    /// [`poll_add_fd`](Self::poll_add_fd)); the depth only decides how many
    /// reach the kernel per batch.
    pub fn new(handles: u32) -> Self {
        let entries = handles.clamp(1, Self::MAX_HANDLES).next_power_of_two();
        let (ring_fd, shm_token) = syscall::io_uring_setup(entries)
            .expect("Poller::new: io_uring_setup failed");
        let base = unsafe { syscall::try_map_shared(shm_token) }
            .expect("Poller::new: map_shared failed");
        let params = unsafe { &*(base as *const IoUringParams) };
        let sq_size = params.sq_ring_size;
        let cq_size = params.cq_ring_size;
        Self { ring_fd, base, sq_size, cq_size }
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
    pub fn poll_add_fd(&self, fd: Fd, flags: u32, token: u64) {
        // The SQ is a transport, not the caller's queue. Writing past its depth
        // would wrap onto entries the kernel has not read yet, and it refuses
        // an over-deep batch wholesale — so nothing would ever be submitted.
        // Flushing first costs one syscall per ring depth and keeps every entry.
        if self.pending() == self.sq_size {
            self.submit(0, 0);
        }
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
        sqe.fd = fd.0;
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

        // The kernel drops a completion it cannot post and records it here.
        // Nobody asked, so a dropped completion became a caller waiting forever
        // for readiness that was thrown away — a hang with no cause on the
        // machine that has it. Asking turns it into a diagnosable failure.
        //
        // It is reachable without corrupting anything: `poll_add_fd` flushes a
        // full SQ mid-registration, the kernel processes those entries at once,
        // and already-ready fds post completions while the caller is still
        // registering. Past `cq_size` they are dropped. The counter is
        // cumulative and the kernel never clears it, so this stays tripped.
        let dropped = cq_hdr.dropped.load(Ordering::Relaxed);
        assert_eq!(
            dropped, 0,
            "Poller: the kernel dropped {dropped} completion(s) — more fds were \
             registered between two wait() calls than the completion ring holds. \
             Size the Poller for the whole watched set.",
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
