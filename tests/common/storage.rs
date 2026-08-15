//! The interlock that keeps ToyOS off a disk it was not given.
//!
//! The claim under test is not "formatting works" -- `nvme_large_device` has
//! that -- but its negative: **a device the kernel was not given comes back
//! byte-for-byte unchanged.** That is asserted against the backing file, on
//! the host, because the guest's account of what it did to a disk is exactly
//! the thing in question.
//!
//! The stimulus is the state that caused this: a disk that holds something,
//! mounts as nothing, and belongs to someone. The kernel used to read "mount
//! returned None" as permission to format, so the first boot on the T14 would
//! have taken the owner's disk -- and the only reason the real first boot did
//! not is that an unrelated panic in `page_cache::init` came first, which has
//! since been fixed.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use super::qemu::{self, BootOptions, QemuInstance};

/// Boot the guest against a disk that belongs to somebody else, and prove it
/// comes back untouched.
///
/// Lives here rather than in `toyos.rs` so that the registration hunk in that
/// shared file stays one line: every agent edits it, and a wide diff there is
/// how work gets swept into somebody else's commit.
pub fn foreign_disk_untouched(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const BYTES: u64 = 128 * 1024 * 1024;
    // The same directory `boot_with_options` uses, named here because this
    // image has to exist before the boot that must not touch it.
    let dir = super::lane::dir();
    let image = dir.join("foreign-disk.img");
    foreign_disk_image(&image, BYTES);
    let before = write_fingerprint(&image, BYTES);

    // The premise, checked before the boot rather than assumed: if this image
    // somehow already parsed as a ToyOS volume, the kernel would mount it and
    // the assertion below would pass for the wrong reason.
    if before.starts_with(b"BCFS") {
        return Err("the foreign image starts with a bcachefs superblock".to_string());
    }

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            nvme_image: Some(image.clone()),
            ..Default::default()
        },
    );

    // The boot log, not a post-ready drain: every line this test cares about
    // is printed in the storage phase, long before the ready marker.
    let log = qemu.boot_log().to_string();
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?}: refusing a disk must not be fatal\n{log}"));
        }
    }
    // The refusal is stated, not inferred. A kernel that never reached the
    // storage phase would also leave the image untouched.
    const REFUSED: &str = "this disk is not ours";
    if !log.contains(REFUSED) {
        return Err(format!("the kernel never said {REFUSED:?} — did it reach storage?\n{log}"));
    }
    // And the machine still came up, because a refusal that costs the boot is
    // a refusal nobody will leave switched on.
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not complete on a disk it refused\n{log}"));
    }
    // Independent of anything that reaches the platter, and deliberately so:
    // the byte comparison below can only see writes that were flushed, and a
    // format that is still sitting in the page cache has already destroyed the
    // disk as far as the next sync is concerned.
    if log.contains("formatting it") {
        return Err(format!("the kernel decided to format a disk it was not given\n{log}"));
    }

    // Shut down rather than kill. `PageCache::sync` at shutdown is the only
    // thing that moves a format from the cache to the device, so killing QEMU
    // here would fingerprint an image that a formatting kernel had also left
    // untouched -- which is exactly what the first version of this test did,
    // and the negative gate caught it passing against a kernel that formatted.
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let tail = qemu.drain_serial(Duration::from_secs(20));
    for bad in ["PANIC:", "panicked at"] {
        if tail.contains(bad) {
            return Err(format!("{bad:?} during shutdown\n{tail}"));
        }
    }
    drop(qemu);

    let after = write_fingerprint(&image, BYTES);
    if let Some(diff) = first_difference(&before, &after) {
        return Err(format!("the kernel wrote to a disk it was not given: {diff}"));
    }
    let _ = std::fs::remove_file(&image);
    Ok(())
}

/// A GPT protective MBR and a plausible partition header: what the front of a
/// disk with another operating system on it looks like. Nothing in it is a
/// bcachefs superblock and nothing in it is a designation stamp, which is the
/// only property that matters -- but a recognisable layout is what makes a
/// failure legible, since the diff below prints where the bytes changed.
pub fn foreign_disk_image(path: &Path, len: u64) {
    use std::io::{Seek, SeekFrom, Write};

    let file = std::fs::File::create(path).expect("create foreign image");
    file.set_len(len).expect("size foreign image");

    let mut mbr = [0u8; 512];
    // One 0xEE partition spanning the disk, then the MBR signature.
    mbr[446] = 0x00;
    mbr[450] = 0xEE;
    mbr[454..458].copy_from_slice(&1u32.to_le_bytes());
    mbr[458..462].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let mut gpt = [0u8; 512];
    gpt[..8].copy_from_slice(b"EFI PART");
    gpt[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());

    let mut file = std::fs::OpenOptions::new().write(true).open(path).expect("open foreign image");
    file.seek(SeekFrom::Start(0)).expect("seek");
    file.write_all(&mbr).expect("write mbr");
    file.write_all(&gpt).expect("write gpt header");
}

/// Every byte the guest could plausibly have touched, as a fingerprint.
///
/// The whole image is 128 MiB and sparse; hashing it would materialize
/// nothing but would read it all. The regions below are where a format
/// writes: the superblock and allocation bitmap at the front, and the backup
/// superblock at the very end. `Formatted::format` cannot leave those alone
/// and still have produced a filesystem, so a match over them is a proof and
/// not a sample.
pub fn write_fingerprint(path: &Path, len: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).expect("open image to fingerprint");
    let mut out = Vec::new();
    // The first 1 MiB: protective MBR, superblock, bitmap, root node.
    let mut head = vec![0u8; 1024 * 1024];
    file.read_exact(&mut head).expect("read head");
    out.extend_from_slice(&head);
    // The last 4 KiB: the backup superblock.
    file.seek(SeekFrom::Start(len - 4096)).expect("seek tail");
    let mut tail = vec![0u8; 4096];
    file.read_exact(&mut tail).expect("read tail");
    out.extend_from_slice(&tail);
    out
}

/// Where two fingerprints first differ, rendered for a failure message.
pub fn first_difference(before: &[u8], after: &[u8]) -> Option<String> {
    if before == after {
        return None;
    }
    let at = before.iter().zip(after).position(|(a, b)| a != b);
    Some(match at {
        Some(at) => format!(
            "byte {at} of the fingerprinted region changed: {:#04x} -> {:#04x}",
            before[at], after[at]
        ),
        None => format!("the region changed length: {} -> {}", before.len(), after.len()),
    })
}
