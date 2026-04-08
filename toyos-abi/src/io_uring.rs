/// io_uring op codes. Raw u8 constants for shared memory SQEs.
/// The kernel converts to a type-safe enum at the syscall boundary.
pub const IORING_OP_NOP: u8 = 0;
pub const IORING_OP_POLL_ADD: u8 = 1;
pub const IORING_OP_POLL_REMOVE: u8 = 2;
pub const IORING_OP_ACCEPT: u8 = 3;
pub const IORING_OP_CLOSE: u8 = 4;

/// Poll interest flags for IORING_OP_POLL_ADD (stored in sqe.op_flags).
pub const IORING_POLL_IN: u32 = 1;
pub const IORING_POLL_OUT: u32 = 4;

/// Submission queue entry. Written by userspace into the SQE array.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoUringSqe {
    pub op: u8,
    pub flags: u8,
    pub _pad: u16,
    pub fd: i32,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub op_flags: u32,
    pub user_data: u64,
}

impl Default for IoUringSqe {
    fn default() -> Self {
        Self { op: 0, flags: 0, _pad: 0, fd: 0, off: 0, addr: 0, len: 0, op_flags: 0, user_data: 0 }
    }
}

/// Completion queue entry. Written by the kernel into the CQ array.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoUringCqe {
    pub user_data: u64,
    pub result: i32,
    pub flags: u32,
}

impl Default for IoUringCqe {
    fn default() -> Self {
        Self { user_data: 0, result: 0, flags: 0 }
    }
}

/// Shared ring header at the start of SQ and CQ regions.
/// head/tail are atomic — kernel and userspace read/write concurrently.
#[repr(C)]
pub struct IoUringRingHeader {
    pub head: core::sync::atomic::AtomicU32,
    pub tail: core::sync::atomic::AtomicU32,
    pub ring_size: u32,
    pub _pad: u32,
}

/// Parameters returned by io_uring_setup. Describes the layout of the
/// shared memory page so userspace can locate the rings and SQE array.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoUringParams {
    pub sq_off: u64,
    pub cq_off: u64,
    pub sqes_off: u64,
    pub sq_ring_size: u32,
    pub cq_ring_size: u32,
    pub features: u32,
    pub _pad: u32,
}

impl Default for IoUringParams {
    fn default() -> Self {
        Self { sq_off: 0, cq_off: 0, sqes_off: 0, sq_ring_size: 0, cq_ring_size: 0, features: 0, _pad: 0 }
    }
}

/// Shared memory page layout offsets.
pub const SQ_RING_OFF: u64 = 0x1000;
pub const CQ_RING_OFF: u64 = 0x2000;
pub const SQES_OFF: u64 = 0x4000;

/// Interest flags for [`poll_fds`] entries.
pub const POLL_READABLE: u64 = 1 << 62;
pub const POLL_WRITABLE: u64 = 1 << 63;
pub const POLL_FD_MASK: u64 = !(POLL_READABLE | POLL_WRITABLE);

/// Result of [`poll_fds`].
pub struct PollResult {
    mask: u64,
}

impl PollResult {
    /// Whether the fd at `index` is ready.
    pub fn fd(&self, index: usize) -> bool {
        self.mask & (1 << index) != 0
    }
}

/// Poll file descriptors for readiness using io_uring.
///
/// Each entry is an fd number (as u64), optionally OR'd with
/// [`POLL_READABLE`] or [`POLL_WRITABLE`].
///
/// Creates and destroys a temporary io_uring per call (2 syscalls + shared
/// memory allocation). Acceptable for cold paths (libc poll(), cargo read2,
/// socket timeouts). Hot paths (compositor, mio/tokio) should use a persistent
/// ring via [`crate::syscall::io_uring_setup`] directly.
pub fn poll_fds(fds: &[u64], timeout_nanos: Option<u64>) -> PollResult {
    use core::sync::atomic::Ordering;

    let len = fds.len().min(63);
    if len == 0 {
        if let Some(t) = timeout_nanos {
            crate::syscall::nanosleep(t);
        }
        return PollResult { mask: 0 };
    }

    let (ring_fd, shm_token) = crate::syscall::io_uring_setup(64)
        .expect("poll_fds: io_uring_setup failed");
    let base = unsafe { crate::syscall::map_shared(shm_token) };
    let params = unsafe { &*(base as *const IoUringParams) };
    let sq_size = params.sq_ring_size;
    let cq_size = params.cq_ring_size;

    // Submit POLL_ADDs
    let sq_hdr = unsafe { &*(base.add(SQ_RING_OFF as usize) as *const IoUringRingHeader) };
    for (i, &entry) in fds[..len].iter().enumerate() {
        let fd_num = (entry & POLL_FD_MASK) as i32;
        let want_write = entry & POLL_WRITABLE != 0;
        let want_read = (entry & POLL_READABLE != 0) || !want_write;

        let mut op_flags = 0u32;
        if want_read { op_flags |= IORING_POLL_IN; }
        if want_write { op_flags |= IORING_POLL_OUT; }

        let tail = sq_hdr.tail.load(Ordering::Acquire);
        let idx = tail & (sq_size - 1);
        let sqe = unsafe {
            &mut *(base.add(SQES_OFF as usize + idx as usize * core::mem::size_of::<IoUringSqe>()) as *mut IoUringSqe)
        };
        *sqe = IoUringSqe::default();
        sqe.op = IORING_OP_POLL_ADD;
        sqe.fd = fd_num;
        sqe.op_flags = op_flags;
        sqe.user_data = i as u64;
        sq_hdr.tail.store(tail.wrapping_add(1), Ordering::Release);
    }

    let timeout = match timeout_nanos {
        None => u64::MAX,
        Some(n) => n,
    };
    let _ = crate::syscall::io_uring_enter(ring_fd, len as u32, 1, timeout);

    // Drain CQEs
    let cq_hdr = unsafe { &*(base.add(CQ_RING_OFF as usize) as *const IoUringRingHeader) };
    let mut mask: u64 = 0;
    loop {
        let head = cq_hdr.head.load(Ordering::Acquire);
        let tail = cq_hdr.tail.load(Ordering::Acquire);
        if head == tail { break; }
        let idx = head & (cq_size - 1);
        let cqe = unsafe {
            &*(base.add(CQ_RING_OFF as usize + 16 + idx as usize * core::mem::size_of::<IoUringCqe>()) as *const IoUringCqe)
        };
        if cqe.result > 0 && cqe.user_data < 64 {
            mask |= 1 << cqe.user_data;
        }
        cq_hdr.head.store(head.wrapping_add(1), Ordering::Release);
    }
    crate::syscall::close(ring_fd);
    PollResult { mask }
}
