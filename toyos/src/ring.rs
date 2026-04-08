//! Ergonomic io_uring wrapper.

use core::sync::atomic::Ordering;
use toyos_abi::Fd;
use toyos_abi::syscall;
use toyos_abi::io_uring::{
    IoUringSqe, IoUringCqe, IoUringRingHeader, IoUringParams,
    IORING_OP_POLL_ADD, SQ_RING_OFF, CQ_RING_OFF, SQES_OFF,
};

/// An io_uring instance for event-driven I/O.
///
/// Owns the ring fd and shared memory mapping. Submissions are batched
/// and flushed on `wait()`.
pub struct Ring {
    ring_fd: Fd,
    base: *mut u8,
    sq_size: u32,
    cq_size: u32,
}

impl Ring {
    /// Create a new io_uring with the given number of entries.
    pub fn new(entries: u32) -> Self {
        let (ring_fd, shm_token) = syscall::io_uring_setup(entries)
            .expect("Ring::new: io_uring_setup failed");
        let base = unsafe { syscall::map_shared(shm_token) };
        let params = unsafe { &*(base as *const IoUringParams) };
        let sq_size = params.sq_ring_size;
        let cq_size = params.cq_ring_size;
        Self { ring_fd, base, sq_size, cq_size }
    }

    /// Submit a poll request for the given fd.
    ///
    /// `flags` are `IORING_POLL_IN` / `IORING_POLL_OUT` from `toyos_abi::io_uring`.
    /// `token` is returned in completions to identify which fd is ready.
    pub fn poll_add(&self, fd: Fd, flags: u32, token: u64) {
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

    /// Submit pending entries and wait for completions.
    ///
    /// Blocks until at least `min_complete` completions are ready or `timeout_nanos`
    /// elapses. Calls `f` for each completed token.
    pub fn wait(&self, min_complete: u32, timeout_nanos: u64, mut f: impl FnMut(u64)) {
        let to_submit = self.pending();
        let _ = syscall::io_uring_enter(self.ring_fd, to_submit, min_complete, timeout_nanos);

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
            if cqe.result > 0 {
                f(cqe.user_data);
            }
            cq_hdr.head.store(head.wrapping_add(1), Ordering::Release);
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        syscall::close(self.ring_fd);
    }
}
