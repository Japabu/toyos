//! An inbox: the shared-memory pair of rings a process submits work on and
//! reads completions from.
//!
//! **The name is the mechanism's, not Linux's.** This was called `io_uring`
//! until 2026-08-20, which named a Linux mechanism this kernel does not
//! implement — no fixed files, no registered buffers, no SQPOLL, three op
//! codes. The owner ruled `inbox`, and the acronyms went with it: a submission
//! ring, a completion ring, a submission. Numbers 89 and 90 did not move, and
//! neither did a single struct layout — [`crate::syscall::SYS_INBOX_SETUP`]
//! says why a rename is not a retirement.
//!
//! **Two other things in this kernel are already called an inbox, and this is
//! meant to join one of them.** `completion::Inbox` is a *task's* bounded
//! record ring, which is what a waiter on this object ends up owning; the
//! kernel's own object for this ABI is to land at `crate::object::inbox`
//! beside it. A `ConnectionEnd`'s `inbox`/`outbox` pair is the common noun and
//! is unrelated.
//!
//! Op codes are raw `u8` constants because they cross shared memory. The
//! kernel converts to a type-safe enum at the syscall boundary.

use crate::RawHandle;

pub const OP_NOP: u8 = 0;
pub const OP_WATCH: u8 = 1;
// Op code 2 unused (formerly IORING_OP_POLL_REMOVE). It had no submitter
// anywhere either — and the selector that would have been its caller cancels
// nothing: mio's ToyOS selector keeps its own registration list, re-arms every
// registration on each `select`, and deregisters by dropping the entry. A watch
// this kernel takes is one-shot, consumed by the completion it posts, so the
// interest a remove would withdraw is gone before there is anything to name.
pub const OP_ACCEPT: u8 = 3;
// Op code 4 unused (formerly IORING_OP_CLOSE). It had no submitter anywhere —
// not in the SDK, not in userland, not in mio — and it was the one handle path
// that could not obey the bad-handle policy: it runs under the ring's own lock,
// where taking the process down is not available.

/// Readiness flags for [`OP_WATCH`], stored in `Submission::op_flags`.
///
/// Honest at both ends: the same two bits are the interest going in and the
/// result coming back in `Completion::result`.
pub const READABLE: u32 = 1;
pub const WRITABLE: u32 = 4;

/// One piece of work. Written by userspace into the submission array.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Submission {
    pub op: u8,
    pub flags: u8,
    pub _pad: u16,
    /// The handle this entry is about. Signed until 2026-08-09, which made it
    /// the one place a handle round-tripped through a type that can hold `-1`.
    pub handle: RawHandle,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub op_flags: u32,
    /// The caller's word, handed back untouched in `Completion::token`. The
    /// kernel's `completion::arm` takes a `Token`, and this is that value
    /// round-tripping through shared memory.
    pub token: u64,
}

impl Default for Submission {
    fn default() -> Self {
        Self { op: 0, flags: 0, _pad: 0, handle: RawHandle(0), off: 0, addr: 0, len: 0, op_flags: 0, token: 0 }
    }
}

/// One finished piece of work. Written by the kernel into the completion array.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Completion {
    pub token: u64,
    pub result: i32,
    pub flags: u32,
}

// Spelled out rather than derived, as `RingLayout` below is: `Submission`
// above cannot derive one — `RawHandle` has no `Default` — so a derive here
// would split one family of ABI structs across two idioms.
#[allow(clippy::derivable_impls)]
impl Default for Completion {
    fn default() -> Self {
        Self { token: 0, result: 0, flags: 0 }
    }
}

/// Shared ring header at the start of the submission and completion regions.
/// head/tail are atomic — kernel and userspace read/write concurrently.
#[repr(C)]
pub struct RingHeader {
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
    /// completions for the handles that are already ready while the caller is
    /// still registering the rest. `toyos`'s `Poller` sizes its rings so that
    /// cannot happen and reads this on every wait, so the reachable case now
    /// has a name and an owner.
    pub dropped: core::sync::atomic::AtomicU32,
}

/// Where the two rings and the submission array sit in the page
/// [`crate::syscall::inbox_setup`] maps. Written by the kernel at offset 0.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RingLayout {
    pub submission_ring_off: u64,
    pub completion_ring_off: u64,
    pub submissions_off: u64,
    pub submission_ring_size: u32,
    pub completion_ring_size: u32,
    pub features: u32,
    pub _pad: u32,
}

// Spelled out for the reason `Completion`'s is.
#[allow(clippy::derivable_impls)]
impl Default for RingLayout {
    fn default() -> Self {
        Self {
            submission_ring_off: 0,
            completion_ring_off: 0,
            submissions_off: 0,
            submission_ring_size: 0,
            completion_ring_size: 0,
            features: 0,
            _pad: 0,
        }
    }
}

/// Where each word of a [`RingHeader`] sits inside it.
///
/// **Literals, and they are the ABI claim.** Neither end of this protocol may
/// hold a Rust reference over the page — the kernel writes it whenever it
/// likes, and a `Freeze` `T` hands the compiler `noalias` it does not have — so
/// both ends reach a header one `AtomicU32` at a time, at an offset each
/// computes with `offset_of!`. Those two computations agree with each other by
/// construction and with *nothing else*: a field reordered here would move both
/// in step and no test on either side could see it. The `const _`s below are
/// what the reordering meets instead, in the crate both ends compile against,
/// so a kernel build and an SDK build red on the same line.
pub const RING_HEAD_OFF: usize = 0;
pub const RING_TAIL_OFF: usize = 4;
pub const RING_SIZE_OFF: usize = 8;
pub const RING_DROPPED_OFF: usize = 12;

const _: () = assert!(RING_HEAD_OFF == core::mem::offset_of!(RingHeader, head));
const _: () = assert!(RING_TAIL_OFF == core::mem::offset_of!(RingHeader, tail));
const _: () = assert!(RING_SIZE_OFF == core::mem::offset_of!(RingHeader, ring_size));
const _: () = assert!(RING_DROPPED_OFF == core::mem::offset_of!(RingHeader, dropped));
const _: () = assert!(core::mem::size_of::<RingHeader>() == 16);
/// The completion array starts immediately after its ring's header, which is
/// the one region of the page that has no `_OFF` constant of its own — both
/// ends spell it as the ring offset plus this.
const _: () = assert!(core::mem::size_of::<Completion>() == 16);
const _: () = assert!(core::mem::size_of::<Submission>() == 40);

/// Shared memory page layout offsets.
pub const SUBMISSION_RING_OFF: u64 = 0x1000;
pub const COMPLETION_RING_OFF: u64 = 0x2000;
pub const SUBMISSIONS_OFF: u64 = 0x4000;

#[cfg(test)]
mod tests {
    use super::*;

    /// The four offsets and three sizes both ends index the page by. A change
    /// to [`RingHeader`], [`Submission`] or [`Completion`] that moves any of
    /// them is a change to the kernel's `inbox.rs` and to `toyos`'s `Poller`
    /// at once, and this is where it is stated once.
    #[test]
    fn the_ring_layout_is_what_both_ends_index_by() {
        assert_eq!(core::mem::offset_of!(RingHeader, head), RING_HEAD_OFF);
        assert_eq!(core::mem::offset_of!(RingHeader, tail), RING_TAIL_OFF);
        assert_eq!(core::mem::offset_of!(RingHeader, ring_size), RING_SIZE_OFF);
        assert_eq!(core::mem::offset_of!(RingHeader, dropped), RING_DROPPED_OFF);
        assert_eq!(core::mem::size_of::<RingHeader>(), 16);
        assert_eq!(core::mem::align_of::<RingHeader>(), 4);
        assert_eq!(core::mem::size_of::<Submission>(), 40);
        assert_eq!(core::mem::align_of::<Submission>(), 8);
        assert_eq!(core::mem::size_of::<Completion>(), 16);
        assert_eq!(core::mem::align_of::<Completion>(), 8);
    }
}
