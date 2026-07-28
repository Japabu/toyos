//! The kernel must survive a process that hands `SYS_SPAWN` absurd argv/env.
//!
//! `argv_len` and `env_len` are unchecked `u64` fields of a user-supplied
//! `SpawnArgs`. The argv blob is split into a `Vec<&str>` (16 bytes per token)
//! and the env blob is copied with `to_vec()`, so either one used to be an
//! arbitrary kernel allocation and a panic in the 2 MiB allocation assert.
//! An argv that contains no non-empty token also used to index `argv[0]`.

use std::process::Command;

use toyos_abi::syscall::{self, MmapFlags, MmapProt, SpawnArgs, SyscallError};

/// One contiguous mmap region — `user_slice` requires physical contiguity,
/// so a heap `String` would risk a vacuous pass on `BadAddress`.
const REGION: usize = 4 * 1024 * 1024;
/// "a\0" repeated: 1,048,576 argv tokens at 16 bytes each.
const BLOB: usize = 2 * 1024 * 1024;

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
    for i in 0..BLOB / 2 {
        unsafe {
            region.add(i * 2).write(b'a');
            region.add(i * 2 + 1).write(0);
        }
    }

    let base = SpawnArgs {
        argv_ptr: region as u64,
        argv_len: 0,
        fd_map_ptr: 0,
        fd_map_count: 0,
        env_ptr: 0,
        env_len: 0,
    };

    let err = unsafe {
        syscall::spawn(&SpawnArgs { argv_len: BLOB as u64, ..base })
    }
    .expect_err("a 2 MiB argv blob must be rejected");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for oversized argv");

    let err = unsafe {
        syscall::spawn(&SpawnArgs {
            argv_len: 2,
            env_ptr: region as u64,
            env_len: BLOB as u64,
            ..base
        })
    }
    .expect_err("a 2 MiB env blob must be rejected");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for oversized env");

    // The bound is a limit, not a ban: an argv the kernel will accept still
    // has to reach the loader and fail there for its own reason.
    let err = unsafe {
        syscall::spawn(&SpawnArgs { argv_len: 2, ..base })
    }
    .expect_err("argv[0] = \"a\" is not a program");
    assert_eq!(err, SyscallError::NotFound, "wrong error for a short honest argv");

    // No non-empty token: there is no argv[0] to load.
    let err = unsafe { syscall::spawn(&base) }.expect_err("an empty argv must be rejected");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for empty argv");

    // The tail of the region past the blob is still mmap's zero fill.
    let nuls = SpawnArgs { argv_ptr: region as u64 + BLOB as u64, argv_len: 8, ..base };
    let err = unsafe { syscall::spawn(&nuls) }.expect_err("an all-NUL argv must be rejected");
    assert_eq!(err, SyscallError::InvalidArgument, "wrong error for all-NUL argv");

    // Spawn still works, so the rejections left the kernel intact.
    let status = Command::new("/bin/echo")
        .arg("spawn still works")
        .status()
        .expect("spawn /bin/echo");
    assert!(status.success(), "/bin/echo exited {status:?}");

    unsafe { syscall::munmap(region, REGION) }.expect("munmap");
    println!("oversized spawn argv/env rejected, spawn still usable");
}
