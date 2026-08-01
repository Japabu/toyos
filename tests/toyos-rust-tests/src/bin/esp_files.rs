//! The boot partition as an ordinary mount, from inside the machine.
//!
//! Every claim here is checked again on the host against the disk image the
//! *device* received — see `tests/common/esp.rs`. This half exists because the
//! host cannot ask the guest's VFS anything: whether `/boot` reads as a
//! directory tree, whether a file the host wrote arrives byte-for-byte, and
//! whether the two things FAT32 cannot represent are refused rather than
//! silently accepted, are all questions only a process can put.

use std::fs;
use std::io::Write;

/// Mirrored in `tests/common/esp.rs`. Two halves of one fixture; a change to
/// either without the other shows up as a mismatch here, not as a silent pass.
const HOST_NOTE: &str = "/boot/toyos/host-note.txt";
const HOST_TEXT: &str = "written by the host before this machine started\n";
const GUEST_NOTE: &str = "/boot/toyos/guest-note.txt";
const GUEST_TEXT: &str = "written by ToyOS through the VFS\n";
const GUEST_BLOB: &str = "/boot/toyos/guest-blob.bin";
/// Ten pages and a partial eleventh: more than one `write_page` call, more
/// than one cluster on any ESP, and a tail that is the case an off-by-one in
/// the size bookkeeping gets wrong.
const BLOB_LEN: usize = 10 * 4096 + 137;

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

    // Now the write direction. Small first, so a failure says which of the two
    // shapes broke.
    fs::write(GUEST_NOTE, GUEST_TEXT).expect("write a note to /boot");
    let back = fs::read_to_string(GUEST_NOTE).expect("read the note back");
    assert_eq!(back, GUEST_TEXT, "the note changed between write and read");

    let data = blob();
    {
        let mut f = fs::File::create(GUEST_BLOB).expect("create the blob on /boot");
        f.write_all(&data).expect("write the blob");
        f.sync_all().expect("fsync the blob");
    }
    let back = fs::read(GUEST_BLOB).expect("read the blob back");
    assert_eq!(back.len(), data.len(), "the blob is {} bytes, wrote {}", back.len(), data.len());
    let bad = back.iter().zip(&data).position(|(a, b)| a != b);
    assert!(bad.is_none(), "the blob differs at byte {}", bad.unwrap_or(0));
    println!("  PASS {BLOB_LEN} bytes written and read back on /boot");

    // FAT32 has no symlink, and the contract is that this fails rather than
    // leaving a regular file the caller believes is a link.
    let err = toyos_abi::syscall::symlink(b"/boot/toyos/kernel.elf", b"/boot/toyos/link");
    assert!(err.is_err(), "creating a symlink on FAT32 reported success");
    assert!(!names("/boot/toyos").iter().any(|n| n == "link"), "a refused symlink left a file");
    println!("  PASS a symlink on /boot is refused, and leaves nothing behind");

    // Delete has to reach the volume, not just the name cache: the host checks
    // afterwards that this file is gone from the image.
    fs::write("/boot/toyos/doomed.txt", "deleted before shutdown\n").expect("write doomed.txt");
    fs::remove_file("/boot/toyos/doomed.txt").expect("remove doomed.txt");
    assert!(fs::read("/boot/toyos/doomed.txt").is_err(), "the deleted file still reads");
    println!("  PASS a file created and deleted on /boot is gone");
}
