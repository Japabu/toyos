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

