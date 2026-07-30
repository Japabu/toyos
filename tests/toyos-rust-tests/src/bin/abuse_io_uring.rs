//! The kernel must survive a process that lies in its own io_uring SQ header.
//!
//! `head`/`tail` live in the 2 MiB page the process maps and writes itself, so
//! `tail - head` is a userland-chosen number that must never size a kernel
//! allocation or index past the ring depth.

use core::sync::atomic::Ordering;

use toyos_abi::io_uring::{IoUringRingHeader, IoUringSqe, IORING_OP_NOP, SQ_RING_OFF, SQES_OFF};
use toyos_abi::syscall::{self, SyscallError};

const DEPTH: u32 = 8;

fn main() {
    let (ring_fd, shm_token) = syscall::io_uring_setup(DEPTH).expect("io_uring_setup");
    let base = unsafe { syscall::map_shared(shm_token) };
    let sq = unsafe { &*(base.add(SQ_RING_OFF as usize) as *const IoUringRingHeader) };

    // 4 million entries claimed in an 8-entry ring: 160 MB of IoUringSqe.
    sq.tail.store(4_000_000, Ordering::Release);
    let err = syscall::io_uring_enter(ring_fd, 4_000_000, 0, 0)
        .expect_err("enter must reject an SQ tail beyond the ring depth");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for bogus tail");

    // Saturated on both sides: the capacity computation itself overflows.
    sq.tail.store(u32::MAX, Ordering::Release);
    let err = syscall::io_uring_enter(ring_fd, u32::MAX, 0, 0)
        .expect_err("enter must reject a saturated SQ tail");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for saturated tail");

    // An honest ring with a to_submit larger than it could ever hold.
    sq.tail.store(sq.head.load(Ordering::Acquire), Ordering::Release);
    let err = syscall::io_uring_enter(ring_fd, 1_000_000, 0, 0)
        .expect_err("enter must reject to_submit beyond the ring depth");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for bogus to_submit");

    // The ring still works: the rejections must not have advanced head or
    // left the instance in a state where honest submissions stop completing.
    let head = sq.head.load(Ordering::Acquire);
    sq.tail.store(head, Ordering::Release);
    let idx = head & (DEPTH - 1);
    let sqe = unsafe {
        &mut *(base.add(SQES_OFF as usize + idx as usize * core::mem::size_of::<IoUringSqe>())
            as *mut IoUringSqe)
    };
    *sqe = IoUringSqe::default();
    sqe.op = IORING_OP_NOP;
    sqe.user_data = 0xC0FFEE;
    sq.tail.store(head.wrapping_add(1), Ordering::Release);

    let completions = syscall::io_uring_enter(ring_fd, 1, 1, 0).expect("honest NOP submission");
    assert_eq!(completions, 1, "NOP did not complete after the rejected batches");

    syscall::close(ring_fd);
    println!("io_uring SQ header abuse rejected, ring still usable");
}
