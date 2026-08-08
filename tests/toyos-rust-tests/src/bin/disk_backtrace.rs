//! A program running from a disk gets a backtrace with names in it.
//!
//! The kernel used to read a process's `.symtab` through
//! `FileBacking::memory_ptr`, which only the initrd implemented — so this was
//! the one thing the initrd was load-bearing for, and every program run from
//! `/home` faulted into a report of bare `[exe+0x…]` offsets. Nothing said so
//! and nothing tested it: `panic_recovery`'s `deliberate_null_deref` assertion
//! was the only demangled-name gate in the tree and its child runs from the
//! initrd, so deleting the initrd would have taken the whole of that coverage
//! with it and left every test green.
//!
//! `/home` rather than `/tmp`: tmpfs has no `memory_ptr` either, so it would
//! prove the mechanism without proving the claim. The claim is about a device.

use std::fs;
use std::process::Command;

const DIR: &str = "/home/disk_backtrace";
const IN_INITRD: &str = "/bin/test_rs_disk_backtrace_child";
const ON_DISK: &str = "/home/disk_backtrace/child";

fn main() {
    let _ = fs::create_dir(DIR);

    let image = fs::read(IN_INITRD)
        .unwrap_or_else(|e| panic!("read {IN_INITRD}: {e}"));
    fs::write(ON_DISK, &image).unwrap_or_else(|e| panic!("write {ON_DISK}: {e}"));
    println!("  copied {} bytes to {ON_DISK}", image.len());

    // The verdict is on the serial, not here: `check_disk_backtrace` reads the
    // kernel's SEGFAULT report out of this test's own capture window. What this
    // side proves is that the child ran from the disk at all and died the way
    // it was supposed to — without which the serial check would be asserting on
    // a report that never happened.
    let status = Command::new(ON_DISK)
        .status()
        .unwrap_or_else(|e| panic!("spawn {ON_DISK}: {e}"));
    assert!(!status.success(), "a child that dereferences null should be killed");

    println!("  PASS: {ON_DISK} faulted (exit={})", status.code().unwrap_or(-1));
    println!("disk backtrace test passed");
}
