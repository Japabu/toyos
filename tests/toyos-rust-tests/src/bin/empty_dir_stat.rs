//! An empty directory and a path that names nothing are different answers.
//!
//! `Vfs::list` used to give both of them `NotFound`: a directory was visible
//! only through the files under it, so `mkdir("/tmp/d")` made something no
//! later call could see until something was written into it. What that cost is
//! not `readdir` — it is every tool whose two-argument form means "into this
//! directory", because `cp x d/` on a `d` that does not stat as a directory
//! silently writes a *file* named `d`.
//!
//! Asked at the syscall boundary and again through `std`, because it took a fix
//! at each: the kernel tells the two cases apart and `std`'s `is_dir` reads
//! anything the kernel accepted as a directory. A machine carrying one half and
//! not the other passes the first block below and reds in the second, which is
//! how they are kept from drifting apart again. `toybox_file_tools` is where
//! the pair is judged by a program rather than by assertions.

use std::fs;

use toyos_abi::syscall::{self, SyscallError};

/// `/tmp`, because it is a tmpfs whose directories exist only in the VFS's own
/// `created_dirs` — nothing on a disk can make one of these look like a
/// directory by accident, so a pass here is the VFS answering rather than a
/// filesystem remembering.
const EMPTY: &str = "/tmp/empty_dir_stat_empty";
const MISSING: &str = "/tmp/empty_dir_stat_missing";
const WITH_FILE: &str = "/tmp/empty_dir_stat_full";

fn readdir(path: &str) -> Result<usize, SyscallError> {
    // One byte on purpose: the kernel reports the size the listing *needs*
    // whether or not it fits, so the answer is the return value and the bytes
    // are not wanted. It is also what `std`'s `is_dir` passes.
    let mut buf = [0u8; 1];
    syscall::readdir(path.as_bytes(), &mut buf)
}

fn main() {
    fs::create_dir(EMPTY).expect("mkdir the empty directory");
    fs::create_dir(WITH_FILE).expect("mkdir the directory with a file in it");
    fs::write(format!("{WITH_FILE}/f"), b"x").expect("write the file");

    // The distinction itself, at the one layer that can make it.
    assert_eq!(
        readdir(EMPTY),
        Ok(0),
        "an empty directory must list as empty, not refuse"
    );
    assert_eq!(
        readdir(MISSING),
        Err(SyscallError::NotFound),
        "a path that names nothing must refuse, not list as empty"
    );
    // Non-vacuity: a kernel that answered `Ok` for everything would satisfy the
    // first assertion and be no distinction at all.
    let full = readdir(WITH_FILE).expect("a directory with a file in it must list");
    assert!(full > 0, "a directory holding a file listed {full} bytes");

    // The same question through `std`, which is how a program asks it.
    let listed = fs::read_dir(EMPTY)
        .expect("read_dir on an empty directory must succeed")
        .count();
    assert_eq!(listed, 0, "the empty directory yielded {listed} entries");

    let err = fs::read_dir(MISSING).expect_err("read_dir on a missing path must fail");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "read_dir on a missing path reported {err:?}"
    );

    // `stat`, which is the question `cp x d/` actually asks and the one
    // `read_dir` above does not answer: `is_dir` decided it from the *length*
    // of the listing, so an empty directory came back `NotFound`.
    let empty = fs::metadata(EMPTY).expect("metadata on an empty directory must succeed");
    assert!(empty.is_dir(), "an empty directory answered is_dir() == false");
    assert!(!empty.is_file(), "an empty directory also answered is_file()");
    assert!(fs::metadata(WITH_FILE).expect("metadata on an occupied directory").is_dir());
    let err = fs::metadata(MISSING).expect_err("metadata on a missing path must fail");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "metadata on a missing path reported {err:?}"
    );

    // A directory that has had a file removed from it is empty, not gone — the
    // state the `created_dirs` lookup exists for, reached by ordinary use
    // rather than by never writing anything.
    fs::remove_file(format!("{WITH_FILE}/f")).expect("remove the file");
    assert_eq!(
        readdir(WITH_FILE),
        Ok(0),
        "a directory emptied by a delete must still list as a directory"
    );
    assert!(
        fs::metadata(WITH_FILE).expect("metadata on an emptied directory").is_dir(),
        "a directory emptied by a delete stopped stat-ing as one"
    );

    println!("empty dir stat: empty stats as a directory, missing refuses, emptied stays one");
}
