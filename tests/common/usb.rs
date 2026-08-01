//! The USB mass-storage gate.
//!
//! Ground truth is the backing file on the *host*, per
//! `specs/device-test-strategy.md`: the harness writes bytes into the image
//! before the boot and the guest has to find them, and the guest writes bytes
//! the harness finds afterwards. Neither half of the driver certifies the
//! other, which a read-back-what-you-wrote test would have let it do.
//!
//! Lives here rather than in `toyos.rs` so the registration hunk in that shared
//! file stays two lines: every agent edits it, and a wide diff there is how
//! work gets swept into somebody else's commit.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::qemu::{self, BootOptions, Profile, QemuInstance};
use super::serial;

/// Every constant below is mirrored in `kernel/src/usb_gate.rs`. They are two
/// halves of one wire format; a change to either without the other shows up as
/// "carries no stamp", not as a silent pass.
const MAGIC: &[u8; 16] = b"TOYOS-USB-GATE1\0";
const AT_BLOCKS: usize = 16;
const AT_NONCE: usize = 24;
const BLOCK: u64 = 4096;
const HOST_BLOCKS: [i64; 2] = [1, -1];
const GUEST_BLOCKS: [i64; 2] = [2, -2];
const RUN_START: u64 = 4;
const RUN_LEN: u64 = 9;

/// The one kernel feature these boots need. A raw block device has no path to
/// userland, so the kernel is the only in-guest actor that can drive one — the
/// same reason `xhci-one-slot` exists. What decides *which* disk gets written
/// is the stamp in block 0 and not this flag, which is why the unstamped boot
/// below is a real assertion and not a tautology.
const GATE: &[&str] = &["usb-storage-gate"];

fn pattern(nonce: u64, block: u64, i: usize) -> u8 {
    let n = (nonce >> ((i % 8) * 8)) as u8;
    let b = (block ^ (block >> 13) ^ (block >> 27)) as u8;
    n ^ b.wrapping_mul(37) ^ (i as u8).wrapping_mul(101)
}

fn block_of(blocks: u64, index: i64) -> u64 {
    if index >= 0 {
        index as u64
    } else {
        blocks.saturating_sub(index.unsigned_abs())
    }
}

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("toyos-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

fn sparse(path: &Path, bytes: u64) -> std::fs::File {
    let file = std::fs::File::create(path).expect("create the USB image");
    file.set_len(bytes).expect("size the USB image");
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("reopen the USB image")
}

fn write_block(file: &mut std::fs::File, block: u64, data: &[u8]) {
    file.seek(SeekFrom::Start(block * BLOCK)).expect("seek");
    file.write_all(data).expect("write");
}

fn read_block(file: &mut std::fs::File, block: u64) -> Vec<u8> {
    let mut buf = vec![0u8; BLOCK as usize];
    file.seek(SeekFrom::Start(block * BLOCK)).expect("seek");
    file.read_exact(&mut buf).expect("read");
    buf
}

/// Stage an image the guest is allowed to write: the stamp, then the blocks
/// the guest has to read back byte-for-byte. Returns the nonce.
fn stage(path: &Path, bytes: u64) -> u64 {
    let blocks = bytes / BLOCK;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
        | 1;
    let mut file = sparse(path, bytes);

    let mut head = vec![0u8; BLOCK as usize];
    head[..MAGIC.len()].copy_from_slice(MAGIC);
    head[AT_BLOCKS..AT_BLOCKS + 8].copy_from_slice(&blocks.to_le_bytes());
    head[AT_NONCE..AT_NONCE + 8].copy_from_slice(&nonce.to_le_bytes());
    write_block(&mut file, 0, &head);

    for index in HOST_BLOCKS {
        let block = block_of(blocks, index);
        let data: Vec<u8> = (0..BLOCK as usize).map(|i| pattern(nonce, block, i)).collect();
        write_block(&mut file, block, &data);
    }
    file.sync_all().expect("sync the staged image");
    nonce
}

/// Every claim the host can make about what the guest did to the disk.
fn verify(path: &Path, bytes: u64, nonce: u64) -> Result<(), String> {
    let blocks = bytes / BLOCK;
    let guest_nonce = !nonce;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open the USB image to verify");

    // What the guest wrote, at the LBAs it was told to write them.
    for index in GUEST_BLOCKS {
        let block = block_of(blocks, index);
        let got = read_block(&mut file, block);
        if let Some(at) = (0..BLOCK as usize).find(|&i| got[i] != pattern(guest_nonce, block, i)) {
            return Err(format!(
                "block {block} in the image is {:#04x} at byte {at}, not the {:#04x} the guest \
                 was told to write",
                got[at],
                pattern(guest_nonce, block, at)
            ));
        }
    }
    for i in 0..RUN_LEN {
        let block = RUN_START + i;
        let got = read_block(&mut file, block);
        if let Some(at) = (0..BLOCK as usize).find(|&j| got[j] != pattern(guest_nonce, block, j)) {
            return Err(format!(
                "block {block} of the {RUN_LEN}-block run is {:#04x} at byte {at}, not {:#04x}",
                got[at],
                pattern(guest_nonce, block, at)
            ));
        }
    }

    // And what it did not write. A driver whose LBA arithmetic is off by a
    // block passes every assertion above only if it is off by zero, but one
    // that writes a whole batch where it meant to write one block passes them
    // all — so the blocks on either side of the run have to still be nothing.
    if !read_block(&mut file, 0).starts_with(MAGIC) {
        return Err("the guest overwrote the stamp in block 0".to_string());
    }
    for index in HOST_BLOCKS {
        let block = block_of(blocks, index);
        let got = read_block(&mut file, block);
        if let Some(at) = (0..BLOCK as usize).find(|&i| got[i] != pattern(nonce, block, i)) {
            return Err(format!(
                "the guest wrote over the host's block {block} at byte {at}: {:#04x}",
                got[at]
            ));
        }
    }
    for block in [3, RUN_START + RUN_LEN, blocks - 3] {
        if read_block(&mut file, block).iter().any(|&b| b != 0) {
            return Err(format!("block {block} was written and should not have been"));
        }
    }
    Ok(())
}

/// The first 64 KiB and the last 16 KiB — everything the gate would touch on a
/// disk it decided it owned.
fn fingerprint(path: &Path, bytes: u64) -> Vec<u8> {
    let mut file = std::fs::File::open(path).expect("open the USB image to fingerprint");
    let mut out = vec![0u8; 64 * 1024];
    file.read_exact(&mut out).expect("read the head");
    file.seek(SeekFrom::Start(bytes - 16 * 1024)).expect("seek the tail");
    let mut tail = vec![0u8; 16 * 1024];
    file.read_exact(&mut tail).expect("read the tail");
    out.extend_from_slice(&tail);
    out
}

/// Boot, shut the guest down cleanly, and return everything it said.
///
/// The shutdown is not politeness: it is what makes the host's view of the
/// backing file the device's view of it, and `foreign_disk_untouched` records
/// what killing QEMU instead did to the equivalent NVMe assertion.
fn boot_and_shutdown(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
    options: BootOptions,
) -> Result<String, String> {
    let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
    let mut log = qemu.boot_log().to_string();
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    log.push_str(&qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["!!! PANIC !!!", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} during the USB gate boot\n{log}"));
        }
    }
    Ok(log)
}

/// What every gate boot must be able to say about itself before any assertion
/// about bytes means anything.
fn gate_ran(log: &str, disks: usize) -> Result<(), String> {
    let want = format!("usb-gate: {disks} disk(s) on the bus");
    if !log.contains(&want) {
        return Err(format!("the guest never printed {want:?}; did the gate run?\n{log}"));
    }
    if !log.contains("usb-gate: sweep complete") {
        return Err(format!("the gate did not finish its sweep\n{log}"));
    }
    // The boot stick is on this bus in every profile and is the disk the guest
    // is running from. It carries no stamp, so it must have been read once and
    // left alone -- and the gate must say so, because "it did not write it" is
    // not observable from an image the harness rewrites every boot.
    if !log.contains("carries no stamp, leaving it alone") {
        return Err(format!("the gate did not walk past the boot stick\n{log}"));
    }
    Ok(())
}

/// Read what the host wrote, write what the host will read, on a 512-byte
/// sector stick — plus the two negatives that make it mean something: a disk
/// the guest was not given comes back byte-identical, and a machine with one
/// USB disk reports one.
pub fn usb_storage_gate(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let (bytes, lba) = Profile::UsbDisk.usb_disk().expect("UsbDisk declares a disk");
    let image = test_dir().join("usb-gate-512.img");
    let nonce = stage(&image, bytes);

    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::UsbDisk,
            kernel_features: GATE,
            usb_image: Some(image.clone()),
            ..Default::default()
        },
    )?;
    gate_ran(&log, 2)?;
    check_geometry(&log, bytes, lba)?;
    if !log.contains("usb-gate: disk done reads=ok writes=ok refusal=true wr_err=0 healthy=true") {
        return Err(format!("the guest did not report a clean pass\n{log}"));
    }
    verify(&image, bytes, nonce)?;
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
    let _ = std::fs::remove_file(&image);

    // The interlock, on a disk the harness owns end to end: no stamp, no
    // writes. This is `foreign_disk_untouched`'s claim for the bus the machine
    // boots from, and it is what keeps the gate feature from being a licence
    // to write whatever disk happens to be plugged in.
    let foreign = test_dir().join("usb-gate-foreign.img");
    drop(sparse(&foreign, bytes));
    let before = fingerprint(&foreign, bytes);
    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::UsbDisk,
            kernel_features: GATE,
            usb_image: Some(foreign.clone()),
            ..Default::default()
        },
    )?;
    gate_ran(&log, 2)?;
    if log.contains("usb-gate: disk designated") {
        return Err(format!("the gate claimed an unstamped disk\n{log}"));
    }
    if fingerprint(&foreign, bytes) != before {
        return Err("the guest wrote to a USB disk it was not given".to_string());
    }
    let _ = std::fs::remove_file(&foreign);

    // And absence. The claim is about the bus, so it is checked against argv:
    // no console line can tell "the driver bound one disk" from "only one disk
    // was ever attached".
    let options = BootOptions {
        profile: Profile::Metal,
        kernel_features: GATE,
        ..Default::default()
    };
    let argv = qemu::profile_argv(&options);
    let sticks = argv
        .windows(2)
        .filter(|w| w[0] == "-device" && w[1].starts_with("usb-storage"))
        .count();
    if sticks != 1 {
        return Err(format!("metal-sim has {sticks} usb-storage devices, want just the boot stick"));
    }
    let log = boot_and_shutdown(test_config, c_bins, rust_bins, options)?;
    gate_ran(&log, 1)?;
    if !log.contains("usb-storage: 1 device(s)") {
        return Err(format!("the driver did not bind exactly the boot stick\n{log}"));
    }

    eprintln!("  [usb] {bytes} B / {lba} B sectors: host bytes read, guest bytes verified \
               host-side; unstamped disk untouched; one disk on metal-sim");
    Ok(())
}

/// The two device shapes that are not a 512-byte-sector stick: a 4 KiB-sector
/// one, which the whole stack above the sector layer has to divide by, and one
/// too large for the command this driver addresses it with.
pub fn usb_storage_shapes(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let (bytes, lba) = Profile::UsbDisk4k.usb_disk().expect("UsbDisk4k declares a disk");
    if lba != 4096 {
        return Err(format!("UsbDisk4k is a {lba}-byte-sector profile; it is the wrong one"));
    }
    let image = test_dir().join("usb-gate-4k.img");
    let nonce = stage(&image, bytes);
    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::UsbDisk4k,
            kernel_features: GATE,
            usb_image: Some(image.clone()),
            ..Default::default()
        },
    )?;
    gate_ran(&log, 2)?;
    check_geometry(&log, bytes, lba)?;
    if !log.contains("usb-gate: disk done reads=ok writes=ok refusal=true wr_err=0 healthy=true") {
        return Err(format!("the 4 KiB-sector disk did not pass\n{log}"));
    }
    verify(&image, bytes, nonce)?;
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
    let _ = std::fs::remove_file(&image);

    // A 3 TB disk has more sectors than READ(10) can address. The driver has
    // to say so and bind nothing: serving its first 2 TiB would be a silent
    // truncation of the device, and it is the only configuration in which
    // READ CAPACITY(16) runs at all.
    let (huge, _) = Profile::UsbDiskHuge.usb_disk().expect("UsbDiskHuge declares a disk");
    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::UsbDiskHuge,
            kernel_features: GATE,
            ..Default::default()
        },
    )?;
    let sectors = huge / 512;
    let refusal = format!("has {sectors} sectors; this driver issues READ(10)");
    if !log.contains(&refusal) {
        return Err(format!("the driver did not refuse the 3 TB disk by name ({refusal:?})\n{log}"));
    }
    // Refused, not dropped on the floor: the boot stick beside it still binds.
    if !log.contains("usb-storage: 1 device(s)") {
        return Err(format!("refusing the big disk cost the boot stick too\n{log}"));
    }
    gate_ran(&log, 1)?;

    eprintln!("  [usb] 4096 B sectors verified host-side; a {huge} B disk refused by name");
    Ok(())
}

/// The error channel, against a device that really refuses.
///
/// Every other assertion in this file is about bytes, and bytes only prove the
/// path that works. `BlockDevice` returned `()` until recently, so a driver
/// could fail a transfer and the caller could not tell -- and the page cache
/// then labelled a slot with a block number whose read had not happened and
/// served the previous tenant's bytes under it. What makes this a real gate
/// rather than a mock is that nothing here injects anything: QEMU answers
/// WRITE(10) on a write-protected LUN with a CHECK CONDITION, which reaches
/// the driver as a CSW status of 1 and takes the REQUEST SENSE path that no
/// other test in this suite touches.
pub fn usb_storage_write_error(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let (bytes, _) = Profile::UsbDiskReadOnly.usb_disk().expect("the profile declares a disk");
    let image = test_dir().join("usb-gate-ro.img");
    let nonce = stage(&image, bytes);
    let before = fingerprint(&image, bytes);

    let options = BootOptions {
        profile: Profile::UsbDiskReadOnly,
        kernel_features: GATE,
        usb_image: Some(image.clone()),
        ..Default::default()
    };
    // The claim is about how QEMU opened the file, and argv is the only place
    // it is visible: a console line cannot tell a refused write from a write
    // the guest never issued.
    let argv = qemu::profile_argv(&options);
    if !argv.iter().any(|a| a.contains("id=usbdisk") && a.contains("readonly=on")) {
        return Err(format!("the data stick is not read-only in argv: {argv:?}"));
    }

    let log = boot_and_shutdown(test_config, c_bins, rust_bins, options)?;
    gate_ran(&log, 2)?;

    // Reads work, writes do not, and the guest could tell them apart. Before
    // the trait carried a result this line read `writes=ok` on exactly this
    // machine, because a refused write was indistinguishable from a completed
    // one.
    // Three write calls, three refusals *reported through the trait*. Not
    // `writes=bad`, which this profile makes true anyway: the readback of a
    // write that never landed differs whether or not the driver said so, and
    // an assertion on it stayed green with `write_blocks` hard-wired to
    // `Ok(())`. `wr_err` is zero in that build and three in this one.
    if !log.contains("usb-gate: disk done reads=ok writes=bad refusal=true wr_err=3") {
        return Err(format!(
            "the guest did not see the device refuse its writes\n{log}"
        ));
    }
    // The refusal came from the device, not from the driver's own bound: the
    // sense data is what SCSI status 1 carries and nothing else in the driver
    // produces this line.
    if !log.contains("usb-storage: SCSI 0x2a failed, sense") {
        return Err(format!("no WRITE(10) refusal with sense data in the log\n{log}"));
    }
    // And the reads on the same disk still verified, which is what stops
    // "writes=bad" from being true because the whole device fell over.
    if !log.contains("usb-gate: host block 1 verified") {
        return Err(format!("reads failed too; this proves nothing about writes\n{log}"));
    }
    if fingerprint(&image, bytes) != before {
        return Err("a write the device refused reached the backing file".to_string());
    }
    let _ = nonce;
    let _ = std::fs::remove_file(&image);

    eprintln!("  [usb] write-protected LUN: CSW status 1 seen, refusal reached the caller, \
               reads on the same disk unaffected");
    Ok(())
}

/// The geometry the guest derived, against what the profile handed it. This is
/// where a driver that believed the wrong sector size shows up: at 4 KiB
/// sectors and at 512 the block count is the same number, and it is the
/// *sector* size in the line that says which one it read.
fn check_geometry(log: &str, bytes: u64, lba: u32) -> Result<(), String> {
    let blocks = bytes / BLOCK;
    let want = format!("blocks of {lba} B");
    if !log.contains(&want) {
        return Err(format!("the driver did not report {want:?}\n{log}"));
    }
    let want = format!("designated, blocks={blocks} ");
    if !log.contains(&want) {
        return Err(format!("the guest did not see {blocks} blocks ({want:?})\n{log}"));
    }
    // One stamped disk and one unstamped one, whichever order the controller
    // enumerated them in. Asserting the index instead would be asserting
    // QEMU's port assignment, which is not what this test is about.
    if log.matches("carries no stamp, leaving it alone").count() != 1 {
        return Err(format!("want exactly one unstamped disk, the boot stick\n{log}"));
    }
    Ok(())
}
