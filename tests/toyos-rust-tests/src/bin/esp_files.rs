//! The two FAT32 partitions as ordinary mounts, from inside the machine.
//!
//! Every claim here is checked again on the host against the disk image the
//! *device* received — see `tests/common/volumes.rs`. This half exists because
//! the host cannot ask the guest's VFS anything: whether `/boot` reads as a
//! directory tree, whether a file the host wrote arrives byte-for-byte, and
//! whether the things the mount will not do are refused rather than silently
//! accepted, are all questions only a process can put.
//!
//! `/boot` is read-only toward userland and the write direction is exercised
//! on `/log`, which is the same adapter over the same driver. That split is
//! the point rather than an accident of where the files went: `/boot` is what
//! firmware and the bootloader read the machine out of, and it had no
//! permission model at all — `fs::write("/boot/toyos/kernel.elf", "TEETH")`
//! from an ordinary process truncated the kernel image to five bytes. The
//! host's byte-for-byte check of the build artifacts is the other half of
//! that; this half is the attack.

use std::fs;
use std::io::Write;

/// Mirrored in `tests/common/volumes.rs`. Two halves of one fixture; a change
/// to either without the other shows up as a mismatch here, not as a silent
/// pass.
const HOST_NOTE: &str = "/boot/toyos/host-note.txt";
const HOST_TEXT: &str = "written by the host before this machine started\n";
const GUEST_NOTE: &str = "/log/guest-note.txt";
const GUEST_TEXT: &str = "written by ToyOS through the VFS\n";
const GUEST_BLOB: &str = "/log/guest-blob.bin";
/// Ten pages and a partial eleventh: more than one `write_page` call, more
/// than one cluster on any FAT32 volume, and a tail that is the case an
/// off-by-one in the size bookkeeping gets wrong.
const BLOB_LEN: usize = 10 * 4096 + 137;

/// The file whose truncation is the reason `/boot` has a permission model.
const KERNEL: &str = "/boot/toyos/kernel.elf";

fn blob() -> Vec<u8> {
    (0..BLOB_LEN).map(|i| (i.wrapping_mul(97) ^ 0x5A) as u8).collect()
}

fn names(dir: &str) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {dir}: {e}"))
        .map(|e| e.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn main() {
    // What the host put there before the machine booted. A guest that read
    // its own writes back could pass without the read path working at all.
    let got = fs::read_to_string(HOST_NOTE).expect("read the host's note off /boot");
    assert_eq!(got, HOST_TEXT, "the host's note did not survive the trip");
    println!("  PASS host note read back through /boot");

    // The bootloader's own directory, which firmware and the build both put
    // there — so a listing that misses it is a listing, not a namespace.
    let toyos = names("/boot/toyos");
    for want in ["kernel.elf", "initrd.img", "host-note.txt"] {
        assert!(toyos.iter().any(|n| n == want), "/boot/toyos has {toyos:?}, wanted {want}");
    }
    let root = names("/boot");
    for want in ["toyos", "EFI"] {
        assert!(root.iter().any(|n| n == want), "/boot lists {root:?}, wanted {want}");
    }
    println!("  PASS /boot and /boot/toyos list what the image holds");

    // Two levels down, and a file nothing here wrote: the path the bootloader
    // itself was loaded from.
    let loader = fs::read("/boot/EFI/BOOT/BOOTx64.EFI").expect("read the bootloader off /boot");
    assert!(loader.len() > 4096, "BOOTx64.EFI is {} bytes", loader.len());
    assert_eq!(&loader[..2], b"MZ", "BOOTx64.EFI does not start with a PE header");
    println!("  PASS BOOTx64.EFI reads back, {} bytes", loader.len());

    boot_refuses_every_way_of_changing_it();
    log_takes_writes();
}

/// Every syscall that can change what is on a volume, aimed at `/boot`.
///
/// One per syscall rather than one representative, because they are separate
/// entry points and a gate on `open` alone would have said nothing about
/// `unlink` or `rename`. The truncation of `kernel.elf` is first because it is
/// the one that was actually done, by a guest test, to a real image.
fn boot_refuses_every_way_of_changing_it() {
    let before = fs::metadata(KERNEL).expect("stat the kernel image").len();
    assert!(before > 4096, "the kernel image is {before} bytes before we start");

    let err = fs::write(KERNEL, "TEETH").expect_err("truncating the kernel image was permitted");
    println!("  PASS writing {KERNEL} is refused: {err}");
    let after = fs::metadata(KERNEL).expect("stat the kernel image again").len();
    assert_eq!(after, before, "the refused write changed the kernel image's length");

    fs::write("/boot/toyos/new-file.txt", "x").expect_err("creating a file on /boot was permitted");
    fs::remove_file(HOST_NOTE).expect_err("deleting a file on /boot was permitted");
    fs::create_dir("/boot/toyos/newdir").expect_err("mkdir on /boot was permitted");
    fs::rename(HOST_NOTE, "/boot/toyos/moved.txt").expect_err("rename on /boot was permitted");
    assert!(
        toyos_abi::syscall::symlink(b"/boot/toyos/kernel.elf", b"/boot/toyos/link").is_err(),
        "symlink on /boot was permitted"
    );

    // A read-only open still works, and reads. The refusal is of changes, not
    // of the mount.
    let still = fs::read_to_string(HOST_NOTE).expect("read /boot after the refusals");
    assert_eq!(still, HOST_TEXT, "the host's note changed under the refused operations");

    let toyos = names("/boot/toyos");
    for absent in ["new-file.txt", "newdir", "moved.txt", "link"] {
        assert!(!toyos.iter().any(|n| n == absent), "a refused operation left {absent} behind");
    }
    println!("  PASS create, delete, mkdir, rename and symlink are all refused on /boot");
}

/// The write direction, on the volume userland is allowed to have.
///
/// Same adapter and same driver as `/boot`, so nothing about the FAT32 write
/// path goes untested by the refusals above.
fn log_takes_writes() {
    fs::write(GUEST_NOTE, GUEST_TEXT).expect("write a note to /log");
    let back = fs::read_to_string(GUEST_NOTE).expect("read the note back");
    assert_eq!(back, GUEST_TEXT, "the note changed between write and read");

    let data = blob();
    {
        let mut f = fs::File::create(GUEST_BLOB).expect("create the blob on /log");
        f.write_all(&data).expect("write the blob");
        f.sync_all().expect("fsync the blob");
    }
    let back = fs::read(GUEST_BLOB).expect("read the blob back");
    assert_eq!(back.len(), data.len(), "the blob is {} bytes, wrote {}", back.len(), data.len());
    let bad = back.iter().zip(&data).position(|(a, b)| a != b);
    assert!(bad.is_none(), "the blob differs at byte {}", bad.unwrap_or(0));
    println!("  PASS {BLOB_LEN} bytes written and read back on /log");

    // FAT32 has no symlink, and the contract is that this fails rather than
    // leaving a regular file the caller believes is a link. On a mount that
    // permits writes, so what is being refused is the format and not the
    // policy.
    let err = toyos_abi::syscall::symlink(b"/log/guest-note.txt", b"/log/link");
    assert!(err.is_err(), "creating a symlink on FAT32 reported success");
    assert!(!names("/log").iter().any(|n| n == "link"), "a refused symlink left a file");
    println!("  PASS a symlink on /log is refused, and leaves nothing behind");

    // Delete has to reach the volume, not just the name cache: the host checks
    // afterwards that this file is gone from the image.
    fs::write("/log/doomed.txt", "deleted before shutdown\n").expect("write doomed.txt");
    fs::remove_file("/log/doomed.txt").expect("remove doomed.txt");
    assert!(fs::read("/log/doomed.txt").is_err(), "the deleted file still reads");
    println!("  PASS a file created and deleted on /log is gone");
}
