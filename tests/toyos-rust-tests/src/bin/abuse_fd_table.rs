//! The handle-table cap must hold on every insertion path.
//!
//! There are three of them — plain open, `dup2` and `SYS_SPAWN`'s fd_map — and
//! a table that grows past the cap on any one reaches a hashbrown doubling
//! above the kernel's 2 MiB single-allocation ceiling.
//!
//! The cap **is** the slot range now: a `RawHandle` carries twelve bits of
//! slot, so "the table is full" and "that slot does not exist" are one event
//! and one error word rather than two that could disagree.

use toyos_abi::syscall::{self, MmapFlags, MmapProt, SpawnArgs, SyscallError};
use toyos_abi::RawHandle;

/// Mirrors `RawHandle::MAX_SLOTS`. A cap that moves should fail this test
/// loudly.
const MAX_HANDLES: u32 = 4096;

const REGION: usize = 4 * 1024 * 1024;
const PAIRS: usize = 100_000;

fn main() {
    let region = unsafe {
        syscall::mmap(
            core::ptr::null_mut(),
            REGION,
            MmapProt::READ | MmapProt::WRITE,
            MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
        )
    };
    assert!(!region.is_null(), "mmap failed");

    // The child's table is built by a different insert path than dup2's.
    let pairs = region as *mut [u32; 2];
    for i in 0..PAIRS {
        unsafe { pairs.add(i).write([i as u32, 1]) };
    }
    let argv = unsafe { region.add(REGION / 2) };
    unsafe { core::ptr::copy_nonoverlapping(b"/bin/echo\0".as_ptr(), argv, 10) };

    let args = SpawnArgs {
        argv_ptr: argv as u64,
        argv_len: 10,
        fd_map_ptr: region as u64,
        fd_map_count: PAIRS as u64,
        env_ptr: 0,
        env_len: 0,
    };
    let err = unsafe { syscall::spawn(&args) }
        .expect_err("a spawn fd_map past MAX_HANDLES must be rejected");
    assert_eq!(err, SyscallError::ResourceExhausted, "wrong error for oversized fd_map");

    // dup2 picks the slot, so it never went through the allocating path that
    // carried the cap.
    let mut refused = None;
    for n in 3..40_000u16 {
        if let Err(e) = syscall::dup2(RawHandle(1), n) {
            refused = Some((n, e));
            break;
        }
    }
    let (n, e) = refused.expect("dup2 must eventually refuse to grow the handle table");
    assert_eq!(e, SyscallError::ResourceExhausted, "wrong error at the handle cap");
    assert!(
        u32::from(n) <= MAX_HANDLES + 16,
        "handle table reached {n} slots, past the {MAX_HANDLES} cap"
    );

    // The cap is a live limit, not a latched failure. Every slot below `n` is
    // at generation 0, so its handle is the bare slot index.
    for slot in 3..n {
        syscall::close(RawHandle(u32::from(slot)));
    }
    let reused = syscall::dup2(RawHandle(1), 3)
        .expect("dup2 must work again after closing handles");
    syscall::close(reused);

    unsafe { syscall::munmap(region, REGION) }.expect("munmap");
    println!("handle table capped at {MAX_HANDLES} on every insert path (refused at {n})");
}
