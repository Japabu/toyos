/// io_uring op codes. Raw u8 constants for shared memory SQEs.
/// The kernel converts to a type-safe enum at the syscall boundary.
use crate::RawHandle;

pub const IORING_OP_NOP: u8 = 0;
pub const IORING_OP_POLL_ADD: u8 = 1;
// Op code 2 unused (formerly IORING_OP_POLL_REMOVE). It had no submitter
// anywhere either — and the selector that would have been its caller cancels
// nothing: mio's ToyOS selector keeps its own registration list, re-arms every
// registration on each `select`, and deregisters by dropping the entry. A poll
// this kernel takes is one-shot, consumed by the completion it posts, so the
// interest a remove would withdraw is gone before there is anything to name.
pub const IORING_OP_ACCEPT: u8 = 3;
// Op code 4 unused (formerly IORING_OP_CLOSE). It had no submitter anywhere —
// not in the SDK, not in userland, not in mio — and it was the one handle path
// that could not obey the bad-handle policy: it runs under the ring's own lock,
// where taking the process down is not available.

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
    /// The handle this entry is about. Signed until 2026-08-09, which made it
    /// the one place a handle round-tripped through a type that can hold `-1`.
    pub fd: RawHandle,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub op_flags: u32,
    pub user_data: u64,
}

impl Default for IoUringSqe {
    fn default() -> Self {
        Self { op: 0, flags: 0, _pad: 0, fd: RawHandle(0), off: 0, addr: 0, len: 0, op_flags: 0, user_data: 0 }
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

// Spelled out rather than derived, as `IoUringParams` below is: `IoUringSqe`
// above cannot derive one — `RawHandle` has no `Default` — so a derive here
// would split one family of ABI structs across two idioms.
#[allow(clippy::derivable_impls)]
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
    /// Completions the kernel could not post because the completion ring
    /// reported itself full. Cumulative, and never cleared.
    ///
    /// 2x sizing makes this unreachable only for a process that keeps its
    /// registrations within the depth it asked for. It said a non-zero value
    /// meant the process had corrupted its own ring head; honest
    /// over-registration reaches it with no corruption at all, because
    /// flushing a full submission ring mid-registration makes the kernel post
    /// completions for the fds that are already ready while the caller is
    /// still registering the rest. `toyos`'s `Poller` sizes its rings so that
    /// cannot happen and reads this on every wait, so the reachable case now
    /// has a name and an owner.
    pub dropped: core::sync::atomic::AtomicU32,
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

// Spelled out for the reason `IoUringCqe`'s is.
#[allow(clippy::derivable_impls)]
impl Default for IoUringParams {
    fn default() -> Self {
        Self { sq_off: 0, cq_off: 0, sqes_off: 0, sq_ring_size: 0, cq_ring_size: 0, features: 0, _pad: 0 }
    }
}

/// Shared memory page layout offsets.
pub const SQ_RING_OFF: u64 = 0x1000;
pub const CQ_RING_OFF: u64 = 0x2000;
pub const SQES_OFF: u64 = 0x4000;

