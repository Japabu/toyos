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

/// Deepest submission ring the kernel will create — mirrors `MAX_SQ_DEPTH` in
/// `kernel/src/io_uring.rs`. A larger request is clamped rather than refused;
/// see [`Poller::new`].
const MAX_ENTRIES: u32 = 256;

/// An io_uring instance for polling fd readiness.
///
/// Owns the ring fd and shared memory mapping. Submissions are batched
/// and flushed on [`wait`](Self::wait).
///
/// Registering more handles than the ring is deep is safe — the batch is
/// flushed and refilled — but it is not unlimited: each registration can
/// produce one completion, the kernel's completion queue is twice the
/// submission ring, and it asserts rather than overflows. A caller that
/// watches more than `2 × handles` distinct fds between two [`wait`](Self::wait)
/// calls is outside what this type can carry; size it for the set instead.
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
    /// Create a poller sized for `handles` concurrently watched handles.
    ///
    /// The kernel only builds power-of-two rings up to [`MAX_ENTRIES`], so the
    /// request is rounded up and clamped rather than rejected. Registering more
    /// handles than the ring holds stays correct (see
    /// [`poll_add_fd`](Self::poll_add_fd)); the depth only decides how many
    /// reach the kernel per batch.
    pub fn new(handles: u32) -> Self {
        let entries = handles.clamp(1, MAX_ENTRIES).next_power_of_two();
        let (ring_fd, shm_token) = syscall::io_uring_setup(entries)
            .expect("Poller::new: io_uring_setup failed");
        let base = unsafe { syscall::map_shared(shm_token) }
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
