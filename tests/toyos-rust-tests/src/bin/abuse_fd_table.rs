//! The fd-table cap must hold on every insertion path.
//!
//! `MAX_FDS` used to be checked in exactly one of three insert paths, so both
//! `dup2` and `SYS_SPAWN`'s fd_map grew a process's fd table without limit
//! until hashbrown's next doubling exceeded the kernel's 2 MiB allocation
//! assert.

use toyos_abi::syscall::{self, MmapFlags, MmapProt, SpawnArgs, SyscallError};
use toyos_abi::Fd;

/// Mirrors `kernel/src/fd.rs`. A cap that moves should fail this test loudly.
const MAX_FDS: u32 = 1024;

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
        .expect_err("a spawn fd_map past MAX_FDS must be rejected");
    assert_eq!(err, SyscallError::ResourceExhausted, "wrong error for oversized fd_map");

    // dup2 picks the fd number, so it never went through the allocating path
    // that carried the cap.
    let mut refused = None;
    for n in 3..40_000u32 {
        if let Err(e) = syscall::dup2(Fd(1), Fd(n as i32)) {
            refused = Some((n, e));
            break;
        }
    }
    let (n, e) = refused.expect("dup2 must eventually refuse to grow the fd table");
    assert_eq!(e, SyscallError::ResourceExhausted, "wrong error at the fd cap");
    assert!(n <= MAX_FDS + 16, "fd table reached {n} descriptors, past the {MAX_FDS} cap");

    // The cap is a live limit, not a latched failure.
    for fd in 3..n {
        syscall::close(Fd(fd as i32));
    }
    syscall::dup2(Fd(1), Fd(3)).expect("dup2 must work again after closing fds");
    syscall::close(Fd(3));

    unsafe { syscall::munmap(region, REGION) }.expect("munmap");
    println!("fd table capped at {MAX_FDS} on every insert path (refused at {n})");
}
