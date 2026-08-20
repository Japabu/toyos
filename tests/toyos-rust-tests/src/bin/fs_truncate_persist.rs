//! `set_len` on a persistent filesystem, and running a binary out of tmpfs.
//!
//! Both are the same shape of gap: something that works in memory and stops at
//! the boundary where a mount has to describe itself to someone else. A
//! truncate dirties no page, so the flush had nothing to write and returned
//! before it reached the metadata; and tmpfs had no `open_backing`, so the
//! loader could not read a file whose pages are the file.
//!
//! /tmp is the control in both directions: it passed the truncate case before
//! the fix because its authority is the file cache itself.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;

const PAGE: usize = 4096;
const GROWN: u64 = 3 * 1024 * 1024;
const SHRUNK: u64 = 2 * PAGE as u64;

fn seed() -> Vec<u8> {
    (0..PAGE).map(|i| (i * 7 + 11) as u8).collect()
}

/// Grow, shrink, and check across a close in every case — an in-memory size
/// that never reached the filesystem looks correct until the handle goes away.
fn truncate_round_trip(path: &str) {
    {
        let mut f = fs::File::create(path).unwrap_or_else(|e| panic!("create {path}: {e}"));
        f.write_all(&seed()).expect("write the first page");
        f.sync_all().expect("fsync");
    }

    {
        let f = fs::OpenOptions::new().write(true).open(path).expect("reopen to grow");
        f.set_len(GROWN).expect("grow");
        f.sync_all().expect("fsync the grow");
    }
    let len = fs::metadata(path).expect("stat after grow").len();
    assert_eq!(len, GROWN, "{path}: grew to {GROWN}, reports {len} after close");

    // The grown region is a hole: it must read as zeros, not as whatever the
    // blocks behind it happen to hold.
    {
        let mut f = fs::File::open(path).expect("reopen to read the hole");
        f.seek(SeekFrom::Start(GROWN - PAGE as u64)).expect("seek into the hole");
        let mut tail = vec![0u8; PAGE];
        f.read_exact(&mut tail).expect("read the hole");
        assert!(tail.iter().all(|&b| b == 0), "{path}: the grown region is not zero-filled");

        f.seek(SeekFrom::Start(0)).expect("rewind");
        let mut head = vec![0u8; PAGE];
        f.read_exact(&mut head).expect("read the first page");
        assert_eq!(head, seed(), "{path}: growing the file changed data already in it");
    }

    {
        let f = fs::OpenOptions::new().write(true).open(path).expect("reopen to shrink");
        f.set_len(SHRUNK).expect("shrink");
        f.sync_all().expect("fsync the shrink");
    }
    let len = fs::metadata(path).expect("stat after shrink").len();
    assert_eq!(len, SHRUNK, "{path}: shrank to {SHRUNK}, reports {len} after close");

    fs::remove_file(path).expect("cleanup");
}

/// A binary the loader has to demand-page out of tmpfs. Every mount that has
/// held an executable until now is backed by a device or by the initrd image.
///
/// This binary is its own child: spawning a copy of itself needs no second
/// program in the initrd, and guarantees the thing being loaded is a real
/// std PIE rather than something small enough to avoid the interesting path.
fn spawn_from_tmpfs() {
    const SRC: &str = "/bin/test_rs_fs_truncate_persist";
    const DST: &str = "/tmp/spawned_from_tmpfs";

    let image = fs::read(SRC).expect("read this binary out of the initrd");
    assert!(image.len() > PAGE, "the image is too small to demand-page");
    {
        let mut f = fs::File::create(DST).expect("create in tmpfs");
        f.write_all(&image).expect("copy the binary into tmpfs");
        f.sync_all().expect("fsync tmpfs");
    }
    assert_eq!(
        fs::metadata(DST).expect("stat the copy").len(),
        image.len() as u64,
        "the copy is the wrong length",
    );

    let status = Command::new(DST)
        .arg("child")
        .status()
        .expect("spawn a binary living in tmpfs");
    assert!(status.success(), "tmpfs-resident binary exited {:?}", status.code());

    fs::remove_file(DST).expect("cleanup");
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("child") {
        println!("  child ran from tmpfs");
        return;
    }
    truncate_round_trip("/tmp/trunc_control.bin");
    truncate_round_trip("/home/trunc_persist.bin");
    spawn_from_tmpfs();
    println!("truncate persists on /home and /tmp; tmpfs binaries are loadable");
}
