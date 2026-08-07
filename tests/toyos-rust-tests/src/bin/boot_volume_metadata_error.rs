//! What a mount says when its device stops answering the questions that used
//! to have no channel: is this file there, what is in this directory, when was
//! it written.
//!
//! `vfs::FileSystem` answered every one of those with an `Option`, a `bool` or
//! a bare `u64`, so a volume that refused a transfer was reported as a name
//! that is not there. That is the one degradation a caller cannot act on: it
//! creates a file over one that exists, reports a program missing off a stick
//! that is merely unhappy, and unlinks a name it believes is already gone.
//!
//! The kernel is built with `fat-boot-reads-fail`, which fails every read of
//! the boot volume *under `Fat32`* — a directory entry, a FAT chain, an extent
//! list — from the moment the mount finishes. Nothing else in the machine is
//! touched, and the `/log` check at the end is what says so: a refusal that
//! costs the whole machine is not a fix, and one that reads as machine-wide
//! would prove nothing about this volume.
//!
//! The kind is printed rather than asserted here, because the assertion belongs
//! on the host side beside the log it is judged against — see
//! `tests/common/volumes.rs`.

use std::fs;

/// Written by the image builder, so it is on every boot volume this project
/// produces and it is the file `esp_files` reads back byte for byte.
const KERNEL: &str = "/boot/toyos/kernel.elf";

fn main() {
    // The defect in one line: this used to be `Option`, and `None` was both
    // "no such file" and "the volume would not say".
    match fs::File::open(KERNEL) {
        Ok(_) => println!("boot-io: open succeeded"),
        Err(e) => println!("boot-io: open failed: kind={:?}: {e}", e.kind()),
    }

    // `FileSystem::list`'s refusal used to be `SyscallError::NotFound` on this
    // adapter, which reads to a caller as "there is no such directory".
    match fs::read_dir("/boot") {
        Ok(entries) => println!("boot-io: read_dir succeeded with {} entries", entries.count()),
        Err(e) => println!("boot-io: read_dir failed: kind={:?}: {e}", e.kind()),
    }

    // The volume that is *not* injected, from the same adapter over the same
    // driver, so a machine-wide breakage cannot be mistaken for the refusal
    // this test is about.
    match fs::read_dir("/log") {
        Ok(entries) => println!("boot-io: /log still lists {} entries", entries.count()),
        Err(e) => println!("boot-io: /log answered an error too: {e}"),
    }
}
