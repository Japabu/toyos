//! The kernel must survive a process that lies in its own submission ring header.
//!
//! `head`/`tail` live in the 2 MiB page the process maps and writes itself, so
//! `tail - head` is a userland-chosen number that must never size a kernel
//! allocation or index past the ring depth.

use core::sync::atomic::Ordering;

use toyos_abi::inbox::{RingHeader, Submission, OP_NOP, SUBMISSION_RING_OFF, SUBMISSIONS_OFF};
use toyos_abi::syscall::{self, SyscallError};

const DEPTH: u32 = 8;

fn main() {
    let (inbox, base) = unsafe { syscall::inbox_setup(DEPTH) }.expect("inbox_setup");
    let ring = unsafe { &*(base.add(SUBMISSION_RING_OFF as usize) as *const RingHeader) };

    // 4 million entries claimed in an 8-entry ring: 160 MB of Submission.
    ring.tail.store(4_000_000, Ordering::Release);
    let err = syscall::inbox_submit(inbox, 4_000_000, 0, 0)
        .expect_err("enter must reject a submission tail beyond the ring depth");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for bogus tail");

    // Saturated on both sides: the capacity computation itself overflows.
    ring.tail.store(u32::MAX, Ordering::Release);
    let err = syscall::inbox_submit(inbox, u32::MAX, 0, 0)
        .expect_err("enter must reject a saturated submission tail");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for saturated tail");

    // An honest ring with a to_submit larger than it could ever hold.
    ring.tail.store(ring.head.load(Ordering::Acquire), Ordering::Release);
    let err = syscall::inbox_submit(inbox, 1_000_000, 0, 0)
        .expect_err("enter must reject to_submit beyond the ring depth");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for bogus to_submit");

    // The ring still works: the rejections must not have advanced head or
    // left the instance in a state where honest submissions stop completing.
    let head = ring.head.load(Ordering::Acquire);
    ring.tail.store(head, Ordering::Release);
    let idx = head & (DEPTH - 1);
    let submission = unsafe {
        &mut *(base.add(SUBMISSIONS_OFF as usize + idx as usize * core::mem::size_of::<Submission>())
            as *mut Submission)
    };
    *submission = Submission::default();
    submission.op = OP_NOP;
    submission.token = 0xC0FFEE;
    ring.tail.store(head.wrapping_add(1), Ordering::Release);

    let completions = syscall::inbox_submit(inbox, 1, 1, 0).expect("honest NOP submission");
    assert_eq!(completions, 1, "NOP did not complete after the rejected batches");

    syscall::close(inbox);
    println!("submission ring header abuse rejected, inbox still usable");
}
