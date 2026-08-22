//! The USB mass-storage gate.
//!
//! Ground truth is the backing file on the *host*: the harness writes bytes
//! into the image
//! before the boot and the guest has to find them, and the guest writes bytes
//! the harness finds afterwards. Neither half of the driver certifies the
//! other, which a read-back-what-you-wrote test would have let it do.
//!
//! Lives here rather than in `toyos.rs` so the registration hunk in that shared
//! file stays two lines: every agent edits it, and a wide diff there is how
//! work gets swept into somebody else's commit.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
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

/// The one actuator these boots need. A raw block device has no path to
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
    super::lane::dir()
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
///
/// **Every block, on every boot, the one a staged break interrupted included.**
/// `usb_transport_break` used to name that block as nobody's claim, which was
/// the driver's lost write written into the harness as an expectation.
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
    for bad in ["PANIC:", "panicked at"] {
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
            kernel_params: GATE,
            usb_images: vec![image.clone()],
            ..Default::default()
        },
    )?;
    gate_ran(&log, 2)?;
    check_geometry(&log, bytes, lba)?;
    if !log.contains("usb-gate: disk done reads=ok writes=ok refusal=true wr_err=0 healthy=true") {
        return Err(format!("the guest did not report a clean pass\n{log}"));
    }
    // The caller's own device-time budget, spent before the operation started.
    // Distinct from `refusal=true`, which is a *device* that cannot serve the
    // read: this one is the driver declining to issue a command the caller has
    // run out of time for, and the clean pass asserted above is what says the
    // disk was left exactly as it was by it. `kernel/src/block.rs`'s
    // `OPERATION` carries the number and why a device that answers needs one.
    if !log.contains("usb-gate: read with a spent budget refused=true budget=true") {
        return Err(format!(
            "the driver issued a command past the caller's budget, or reported one it \
             refused as a fact about the disk\n{log}"
        ));
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
            kernel_params: GATE,
            usb_images: vec![foreign.clone()],
            ..Default::default()
        },
    )?;
    gate_ran(&log, 2)?;
    // ` designated, blocks=` and not `usb-gate: disk designated`, which the
    // kernel has never printed — the disk index sits between the two words, so
    // the assertion could not fire whatever the guest did.
    if log.contains(" designated, blocks=") {
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
        kernel_params: GATE,
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

/// A data phase the **controller** cut short while the device's own CSW claims
/// it moved everything.
///
/// One number, counted twice. `bulk` returns the residue the xHC reports —
/// bytes it did not move into the buffer — and `bot` threw it away, so
/// `delivered` came from the CSW's `dCSWDataResidue` alone: the device's own
/// account of its own transfer. The `MSC_DATA` window is never cleared between
/// transfers, so a device that under-delivers a READ(10) and reports a residue
/// of zero handed the caller the *previous* transfer's bytes for the part that
/// never arrived — a different LBA's data, under this LBA's number, with no
/// error anywhere.
///
/// **The actuator corrupts the transfer, not the
/// verdict.** QEMU derives the CSW residue from the same transfer the xHC
/// completed, so the two accounts are one number there and can never
/// contradict each other; `rerror` fails the whole command instead. The
/// injection puts the tail of the window back to what it held *before* the
/// transfer — the previous read's bytes, read off that window rather than
/// invented — and adds those bytes to the controller's residue. The completion
/// code is the controller's, the CSW is the device's, and what a driver
/// discarding the controller's number is handed is byte-for-byte what a real
/// short data phase would have left.
///
/// The bytes compared against are the host's: `stage` wrote them before the
/// boot. What the guest reports is which of the two things happened to them —
/// a refusal, or another block's data delivered as this one's.
pub fn usb_short_read(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const PARAMS: &[&str] = &["usb-storage-gate", "usb-short-read"];
    /// Mirrors `short_read::SHORT_BY`. One wire format with the kernel's, in
    /// the same sense the stamp is: a change to either without the other stops
    /// the line below matching rather than passing silently.
    const SHORT_BY: u64 = 512;

    let (bytes, lba) = Profile::UsbDisk.usb_disk().expect("UsbDisk declares a disk");
    let image = test_dir().join("usb-short-read.img");
    let nonce = stage(&image, bytes);

    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::UsbDisk,
            kernel_params: PARAMS,
            usb_images: vec![image.clone()],
            ..Default::default()
        },
    )?;
    gate_ran(&log, 2)?;
    check_geometry(&log, bytes, lba)?;

    // The injection landed on the block the harness staged. Without this the
    // assertions below are about a boot in which nothing was injected.
    let block = block_of(bytes / BLOCK, HOST_BLOCKS[0]);
    let staged = format!("usb-gate: short read of block {block} ");
    let Some(verdict) = log.lines().find(|l| l.contains(&staged)) else {
        return Err(format!("the guest never attempted the short read ({staged:?})\n{log}"));
    };

    // **The finding.** `refused=false` is the defect: the caller was handed
    // bytes for a transfer the controller says did not finish, and `matched`
    // says whether they were this block's. They are not — the window's tail
    // still holds the block read into it before this one.
    if !verdict.contains("refused=true") {
        return Err(format!(
            "{verdict:?} — a data phase the controller cut short reached the caller as data. \
             The bytes past {} of {BLOCK} are the previous read's, from a different LBA\n{log}",
            BLOCK - SHORT_BY
        ));
    }

    // And the driver said so by name, with both numbers in it: a refusal with
    // no line is a disk that stops working for no stated reason, which is what
    // the machine this is for has no second channel to diagnose.
    let named = format!("usb-storage: {} of {BLOCK} B at block {block}", BLOCK - SHORT_BY);
    let shorts = log.matches(named.as_str()).count();
    if shorts != 1 {
        return Err(format!(
            "the driver named {shorts} short transfers ({named:?}); the injection is armed once, \
             so anything else is a transfer this test did not stage\n{log}"
        ));
    }

    // One refused read and nothing else disturbed: the rest of the sweep still
    // passed and everything the guest wrote is where the host expects it.
    if !log.contains("usb-gate: disk done reads=ok writes=ok refusal=true wr_err=0 healthy=true") {
        return Err(format!("one short read cost the disk the rest of its sweep\n{log}"));
    }
    verify(&image, bytes, nonce)?;
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish\n{log}"));
    }
    let _ = std::fs::remove_file(&image);

    eprintln!(
        "  [usb] a data phase {SHORT_BY} B short with a CSW claiming none: refused by name, and \
         the {BLOCK}-byte window's stale tail never reached the caller"
    );
    Ok(())
}

/// A disk plugged into a **different controller** must not renumber the one a
/// mount is holding.
///
/// The machine-wide disk index was `storage.len()` summed across controllers,
/// and that vector grows on every bind — hot-plug included. The T14 has two
/// xHCIs, the Thunderbolt block's at 00:0d.0 ahead of the PCH's at 00:14.0, and
/// it boots off a stick in a PCH port: with nothing on the first controller the
/// boot stick is disk 0, and plugging any USB storage into the USB-C side made
/// the *new* drive disk 0 and the boot stick disk 1. `FatDevice` holds its
/// `UsbBlockDevice` for the life of the mount and that handle is an index, so
/// every later `/log` append went into the middle of the new drive and every
/// `/boot` read served its bytes as the ESP's.
///
/// **The actuator is QEMU's own `device_add` and nothing about the driver is
/// modified.** Both verdicts are host-side and neither is a log line: the disk
/// that arrives late is a file the harness staged as zeros and must find as
/// zeros, and `/log/kernel.log` is read out of the boot image's own partition
/// and must carry a line the guest printed *after* the plug.
pub fn usb_disk_index_stable(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    /// The disk that arrives late: 48 GiB, sparse, and no size any other
    /// device in this suite reports.
    const LATE_BYTES: u64 = 48 * 1024 * 1024 * 1024;
    /// The line the guest prints once the late disk is up, which is therefore a
    /// line `/log` can only carry if it was still reaching the boot stick after
    /// the plug.
    const LATE_READY: &str = "usb-storage: disk 1 ready on slot";

    // The boot stick is on the second controller and the first carries
    // nothing, which is the laptop exactly — and the arrangement in which the
    // boot stick is disk 0 with a free controller ahead of it.
    let options = BootOptions {
        profile: Profile::MetalXhciSecond,
        qmp: true,
        ..Default::default()
    };
    let argv = qemu::profile_argv(&options);
    if !argv.iter().any(|a| a.contains("usb-storage,bus=xhci1.0")) {
        return Err(format!("the boot stick is not on the second controller: {argv:?}"));
    }
    if argv.iter().any(|a| a.starts_with("usb-storage,bus=xhci.0")) {
        return Err(format!("the first controller already carries storage: {argv:?}"));
    }

    // Built here rather than by the boot, because `/log` has to be read off the
    // partition afterwards and the image gets a fresh GUID every time it is
    // built.
    let image_path = test_dir().join("usb-index-stable.img");
    let image = qemu::build_boot_image(test_config, c_bins, rust_bins, &[]);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = super::volumes::log_extent(&image, &image_path)?;

    let late = test_dir().join("usb-index-late.img");
    drop(sparse(&late, LATE_BYTES));
    let before = fingerprint(&late, LATE_BYTES);

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            boot_image: Some(image_path.clone()),
            ..options
        },
    );
    let boot = qemu.boot_log().to_string();
    if !boot.contains("usb-storage: disk 0 ready on slot") {
        return Err(format!("the boot stick did not come up as disk 0\n{boot}"));
    }

    let mut devices = qemu::QmpDevices::open(qemu.qmp_socket());
    devices.blockdev_add("latedisk", &late);
    devices.add("usb-storage", "xhci.0", "latedisk0", &[("drive", "latedisk")]);
    drop(devices);
    // The driver's debounce is 100 ms and the enumeration behind it is
    // microseconds under TCG; this is that with room.
    thread::sleep(Duration::from_millis(1200));

    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let log = format!("{boot}{}", qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} after a disk arrived on the other controller\n{log}"));
        }
    }

    // The plug happened at all. Without this both host-side claims below hold
    // trivially on a boot where nothing was added.
    if !log.contains(LATE_READY) {
        return Err(format!(
            "nothing enumerated on the first controller; there is no renumbering to survive\n{log}"
        ));
    }

    // **The disk that arrived is not the disk anything was mounted on.** The
    // harness made this file and the guest was never told it was writable, so
    // a single changed byte is `/boot` or `/log` writing through a handle that
    // now names the wrong device.
    if fingerprint(&late, LATE_BYTES) != before {
        return Err(
            "the guest wrote to the disk plugged into the other controller — the index a mount \
             was holding moved onto it"
                .to_string(),
        );
    }

    // **And the log kept reaching the stick.** Read off the boot image's own
    // `/log` partition, so this is the device's view and not the guest's. The
    // sink names one file per boot, so the newest on the volume is this one's.
    let (name, on_device) = super::volumes::newest_log(&image_path, start, len)?;
    let on_device = String::from_utf8_lossy(&on_device).into_owned();
    if !on_device.contains(LATE_READY) {
        return Err(format!(
            "/log/{name} stops at {} bytes and never carries {LATE_READY:?} — the appends after \
             the plug went somewhere else\n{log}",
            on_device.len()
        ));
    }
    let _ = std::fs::remove_file(&late);
    let _ = std::fs::remove_file(&image_path);

    eprintln!(
        "  [usb] a {LATE_BYTES} B disk plugged into the empty first controller: it comes back \
         byte-identical, and {} bytes of /log/{name} on the boot stick carry the lines printed \
         after it",
        on_device.len()
    );
    Ok(())
}

/// More disks on one controller than its DMA pool has blocks for.
///
/// `MSC_BLOCKS` is 2 and the boot stick takes one, so the second data disk on
/// this bus is the first one past the ceiling. The bound is policy, which makes
/// what the caller sees when it is hit the whole question: the disk has to be
/// refused **by name** and left alone, never served out of somebody else's
/// block.
///
/// Ground truth is host-side and it is what a log line cannot say: both staged
/// disks are stamped and writable as far as the guest is concerned, and the one
/// the pool had no room for comes back byte-for-byte as the harness left it.
pub fn usb_pool_exhausted(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let disks = Profile::UsbDiskCrowd.usb_disks();
    if disks.len() != 2 {
        return Err(format!(
            "this gate needs two data disks beside the boot stick, the profile declares {}",
            disks.len()
        ));
    }
    let bytes = disks[0].bytes;

    // Both stamped, so what decides which one is written is the pool and not
    // the harness: the disk the driver refuses is whichever the controller
    // enumerated second, and it is the one the gate never designates.
    let bound = test_dir().join("usb-crowd-bound.img");
    let refused = test_dir().join("usb-crowd-refused.img");
    let nonce = stage(&bound, bytes);
    stage(&refused, bytes);
    let refused_before = fingerprint(&refused, bytes);

    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::UsbDiskCrowd,
            kernel_params: GATE,
            usb_images: vec![bound.clone(), refused.clone()],
            ..Default::default()
        },
    )?;

    // Two blocks, two disks, and the third refused by name with the pool's size
    // in the line. The pool runs out inside `bind`, so this refusal is the
    // driver's own and not a device's.
    if !log.contains("usb-storage: 2 device(s)") {
        return Err(format!("the driver did not bind exactly the pool's two blocks\n{log}"));
    }
    let over = log.matches("this driver serves 2").count();
    if over != 1 {
        return Err(format!(
            "{over} disk(s) were refused for want of a pool block, want the one past the \
             ceiling\n{log}"
        ));
    }
    gate_ran(&log, 2)?;

    // The disk that bound was written, so the ceiling did not cost the machine
    // the disk it does have room for.
    verify(&bound, bytes, nonce)?;

    // **And the disk it had no room for was not touched.** A driver that served
    // the refused disk out of somebody else's block would write these bytes
    // under that disk's number, and every line in the log would still read
    // correctly.
    if fingerprint(&refused, bytes) != refused_before {
        return Err("a disk the pool had no block for was written to".to_string());
    }
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish past a crowded bus\n{log}"));
    }
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
    for path in [&bound, &refused] {
        let _ = std::fs::remove_file(path);
    }

    eprintln!(
        "  [usb] three disks on a bus whose pool holds two: two bound and the staged one written, \
         {over} refused by name, and the stamped disk past the ceiling byte-identical host-side"
    );
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
            kernel_params: GATE,
            usb_images: vec![image.clone()],
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
            kernel_params: GATE,
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
        kernel_params: GATE,
        usb_images: vec![image.clone()],
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

/// The two answers a device can give to an *optional* SCSI command, and the
/// loop that reading them as one answer produced.
///
/// SYNCHRONIZE CACHE (0x35) is optional in SBC and a great many USB flash
/// drives answer ILLEGAL REQUEST / INVALID COMMAND OPERATION CODE. `msc_flush`
/// read that as a failed flush; `FatFs::sync` logged the failure and returned
/// `()`; the line it logged was new pending content in the shard `/bin/logd` was
/// draining, and `Sink::flush` still said `Ok`, so the sink's disable path
/// never ran. Every idle pass was then a file write, a FAT write and another
/// SYNCHRONIZE CACHE on the stick the machine booted from, forever — and
/// `MAX_LOG_BYTES` rotates the boot log off the stick while it happens.
///
/// Two boots, because the two halves of the fix are separately observable and
/// each is invisible to the other's boot:
///
/// - `usb-flush-unimplemented` — the refusal is an answer, and the log has to
///   keep reaching the device exactly as on an ordinary boot. Fixing
///   `sync_mount` alone cannot produce that: the returned error disables the
///   sink and the file stops before `Boot: complete`.
/// - `usb-flush-fails` — the same command really failing. The sink has to
///   notice once and stop. Fixing `msc_flush` alone cannot produce that: the
///   error is swallowed and the loop is the one above.
///
/// Neither boot can be green because the actuator was not armed: each asserts a
/// line that only the injected answer produces.
pub fn usb_flush_optional(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    optional_flush_keeps_the_log(test_config, c_bins, rust_bins)?;
    failed_flush_stops_once(test_config, c_bins, rust_bins)
}

/// Boot with a stick that has no write cache. Nothing about the log changes.
fn optional_flush_keeps_the_log(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const PARAMS: &[&str] = &["usb-flush-unimplemented"];
    const REPORTED: &str = "usb-storage: disk 0 does not implement SYNCHRONIZE CACHE";

    let image_path = test_dir().join("usb-flush-optional.img");
    let image = qemu::build_boot_image(test_config, c_bins, rust_bins, PARAMS);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = super::volumes::log_extent(&image, &image_path)?;

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::Metal,
            boot_image: Some(image_path.clone()),
            kernel_params: PARAMS,
            ..Default::default()
        },
    );
    let boot = qemu.boot_log().to_string();

    // Mid-run and polled, exactly as `kernel_log_file` does it: the claim is that
    // the sink is still running, and the only place that is visible is the
    // device while the machine is up.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut on_device;
    loop {
        on_device = String::from_utf8_lossy(
            &super::volumes::newest_log(&image_path, start, len)?.1,
        )
        .into_owned();
        if on_device.contains("Boot: complete") || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let log = format!("{boot}{}", qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} on a stick with no write cache\n{log}"));
        }
    }

    // The injection reached the driver, and the driver said so once. Once is
    // half the assertion: a line per flush is itself the loop, because this
    // log's own bytes are what the next flush writes.
    let said = log.matches(REPORTED).count();
    if said != 1 {
        return Err(format!(
            "the guest printed {REPORTED:?} {said} times, wanted exactly one\n{log}"
        ));
    }
    for wrong in ["usb-storage: cache flush failed", "usb-storage: SCSI 0x35 failed"] {
        if log.contains(wrong) {
            return Err(format!(
                "an optional command a device does not have was reported as a failure ({wrong:?})\
                 \n{log}"
            ));
        }
    }
    if log.contains("logd: /log has not answered") {
        return Err(format!("logd gave up on a stick that is working\n{log}"));
    }
    if !on_device.contains("Boot: complete") {
        return Err(format!(
            "the log on the device stops before `Boot: complete` at {} bytes — a stick with no \
             write cache cost the machine its log",
            on_device.len()
        ));
    }

    let after = super::volumes::newest_log(&image_path, start, len)?.1;
    let after = String::from_utf8_lossy(&after).into_owned();
    if !after.contains("Shutting down.") {
        return Err(format!(
            "the shutdown's last line never reached the file: {} bytes",
            after.len()
        ));
    }
    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [usb] SYNCHRONIZE CACHE refused as unimplemented: reported once, {} bytes of kernel \
         log still on the stick",
        after.len()
    );
    Ok(())
}

/// Boot with a stick whose flush genuinely fails. The writer says so once and
/// stops, rather than writing the device that just refused it.
///
/// **Re-pointed at `/bin/logd` at L6, and the policy it observes changed shape
/// with the writer.** The kernel sink disabled itself on the *first* error,
/// because the alternative from an idle loop was an error every pass. logd's
/// give-up is a *duration* — `LOG_WRITE_BUDGET`, five seconds — because a
/// userland writer can
/// afford to tell a stick that is busy apart from one that is gone, and a
/// device that answers slowly under load is not a device to abandon.
///
/// The probes below are what make "and stops" a claim rather than an absence:
/// each names a binary that is not there, so each commits a kernel record, so
/// each is something logd would write if it had not given up. Twelve of them
/// after the give-up and the failing-flush count still has to hold.
fn failed_flush_stops_once(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const PARAMS: &[&str] = &["usb-flush-fails"];
    /// Probes after the boot, each of which spawns a name that is not there and
    /// so commits a kernel record logd would write if it were still writing.
    const PROBES: usize = 12;
    /// A per-failure line, and the thing that has to stay bounded. Before the
    /// fix it is emitted by every pass of the idle loop for the life of the
    /// boot. After it: one by the write that gives up, and one per mount by the
    /// shutdown's `sync_all`, which is the last caller left.
    ///
    /// **This is the number that caught the retry**, and it is worth saying what
    /// it caught. A logd that retried inside `LOG_WRITE_BUDGET` measured
    /// **1,737** failing flushes here, because the driver logs each failure, the
    /// failure is a kernel record, and the record is something logd then tries
    /// to write. The loop is in the coupling and not in either half.
    const BOUND: usize = 4;

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::Metal,
            kernel_params: PARAMS,
            ..Default::default()
        },
    );
    let mut boot = qemu.boot_log().to_string();
    // Long enough for the give-up to be reachable, and *driven* rather than
    // waited out: each probe names a binary that is not there, which commits a
    // kernel record, which is what gives logd something to fail to write.
    for i in 0..PROBES {
        let _ = writeln!(qemu.stdin_mut(), "run flush-probe-{i}");
        qemu.flush_stdin();
        boot.push_str(&qemu.drain_serial(Duration::from_millis(500)));
    }
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let log = format!("{boot}{}", qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} on a stick that cannot flush\n{log}"));
        }
    }

    if !log.contains("usb-storage: SCSI 0x35 failed, sense 0x04/0x44/0x00") {
        return Err(format!("the injected flush failure never reached the driver\n{log}"));
    }
    // By step and not by code alone: logd names which of the two calls refused
    // it, and the one this test stages is the sync rather than the append ahead
    // of it.
    let gave_up = log.matches("logd: /log has not answered (the sync").count();
    if gave_up != 1 {
        return Err(format!(
            "logd gave up {gave_up} times, wanted exactly one — a failed `SYS_FSYNC` has to \
             reach it as an error, and once it has given up it must not start again\n{log}"
        ));
    }
    let failures = log.matches("usb-storage: cache flush failed").count();
    if failures > BOUND {
        return Err(format!(
            "the guest issued {failures} failing flushes, over the bound of {BOUND}: a failed \
             sync is still producing the log line that asks for the next one\n{log}"
        ));
    }
    eprintln!(
        "  [usb] a flush the device refuses: {failures} failing flushes with {PROBES} probes \
         after it, logd stopped once and never started again"
    );
    Ok(())
}

/// The kernel timestamp on the first line carrying `needle`, in seconds.
///
/// `[kernel 0.218 cpu0] ...`, and `[kernel 1.042 cpu0 tid=3] ...` — the field
/// is in the same place either way.
fn stamp_of(log: &str, needle: &str) -> Result<f64, String> {
    let line = log
        .lines()
        .find(|l| l.contains(needle))
        .ok_or_else(|| format!("no line carrying {needle:?}"))?;
    let rest = line.split_once("[kernel ").ok_or("line has no kernel timestamp")?.1;
    let secs = rest.split_once(' ').ok_or("timestamp is not followed by a field")?.0;
    secs.parse::<f64>().map_err(|e| format!("timestamp {secs:?}: {e}"))
}

/// That the wait between `from` and `refusal` really was the transfer budget.
///
/// Without this the gate would stay green for a `settles` that gave up on its
/// first read: QEMU answers every one of these registers before the deadline is
/// ever consulted, so no other test in the suite would notice either, and a
/// driver that refused every controller after zero nanoseconds would ship.
fn waited_out_the_budget(log: &str, from: &str, refusal: &str) -> Result<f64, String> {
    let waited = stamp_of(log, refusal)? - stamp_of(log, from)?;
    // The budget is 2 s and the serial stamps have millisecond resolution.
    if waited < 1.5 {
        return Err(format!(
            "the refusal came {waited:.3} s after {from:?}; the wait is supposed to be the 2 s \
             transfer budget, so this driver gave up without waiting"
        ));
    }
    Ok(waited)
}

/// A controller and a port that stop answering, which on the machine this is
/// for is a silent hang and nothing else.
///
/// The 2 s deadline covered `wait_command` and `wait_transfer` and nothing
/// around them: the port-reset spin in `init_device` and four register spins in
/// `init_one` — halt, HCRST, CNR and R/S — were bare `spin_loop`s. On a T14
/// that is `Boot: peripherals ready` painted on the panel forever, which is
/// also what a dead port, a dead controller and every other wedge look like.
///
/// Both boots assert the same shape: the thing that did not answer is named,
/// and the machine gets to the shell anyway. `arm_interrupt` already refuses a
/// controller by name; these waits bypassed that machinery entirely.
pub fn xhci_deaf_registers(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    // The whole machine boots off this controller's stick, so refusing it also
    // costs `/boot` — which is the honest cost and the reason the line has to
    // name the controller rather than the mount that went missing.
    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::Metal,
            kernel_params: &["xhci-deaf-controller"],
            ..Default::default()
        },
    )?;
    if !log.contains("it never halted, within 2000 ms of being asked to") {
        return Err(format!("the controller that would not halt was not named\n{log}"));
    }
    if !log.contains("xHCI: 1 controller(s) present, none of them usable, USB unavailable") {
        return Err(format!(
            "a refused controller did not reach `init`'s own summary — a machine with no xHC and \
             one whose xHC was refused are different machines\n{log}"
        ));
    }
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish without its USB controller\n{log}"));
    }
    let controller_wait = waited_out_the_budget(&log, "xHCI: found at PCI", "it never halted")
        .map_err(|e| format!("{e}\n{log}"))?;

    // And the port, which is the wait an ordinary machine can actually reach:
    // a device pulled between the port scan and the reset lands here.
    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::Metal,
            kernel_params: &["xhci-deaf-port"],
            ..Default::default()
        },
    )?;
    let skipped = log.matches("never finished its reset").count();
    if skipped == 0 {
        return Err(format!("no port was named as having failed its reset\n{log}"));
    }
    // The controller itself came up, which is what makes this a *port* refusal
    // and not the previous boot again.
    if !log.contains("xHCI: controller started") {
        return Err(format!("the controller did not start; this is not the port path\n{log}"));
    }
    if !log.contains("usb-storage: 0 device(s)") {
        return Err(format!("a port that never reset still bound a disk\n{log}"));
    }
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish past a port that would not reset\n{log}"));
    }
    let port_wait = waited_out_the_budget(&log, "port 1 connected", "never finished its reset")
        .map_err(|e| format!("{e}\n{log}"))?;
    eprintln!(
        "  [usb] a controller that will not halt is refused by name after {controller_wait:.3} s; \
         {skipped} port(s) that will not reset are skipped after {port_wait:.3} s; both machines \
         reach `Boot: complete`"
    );
    Ok(())
}

/// A root hub that has not finished detecting its devices when the driver first
/// looks — which is every root hub that is made of copper.
///
/// HCRST puts the ports back to the state they have with nothing attached, so a
/// device firmware had already enumerated has to be detected again, and
/// detection takes milliseconds: power settling, a USB2 pull-up being debounced,
/// a USB3 link training. The T14 logged `controller started` and
/// `no HID devices` in the same millisecond, on both controllers, while running
/// off a stick plugged into one of them.
///
/// **The actuator is a boot parameter, and the reason is timing rather than
/// expressiveness.** QEMU *can* stage a late attach: `usb-bot` and `usb-uas`
/// are the two devices whose QOM `attached` property is settable, so
/// `qom-set /machine/peripheral/<id> attached false|true` detaches and
/// reattaches at runtime and does generate a Port Status Change Event
/// (`xhci_attach` → `xhci_port_update` → `xhci_port_notify`, QEMU 11.0.2
/// `hw/usb/hcd-xhci.c`). What it cannot do is *aim*: the port scan happens
/// ~0.1 s into a boot and the driver's detection window is bounded, so a
/// host-wall-clock QMP write would have to land inside a window the guest
/// opens. That makes the outcome a race rather than an assertion.
/// `xhci-slow-connect` replaces the *register* instead — during the window the
/// port reads CCS, PED and speed exactly as an unpopulated one does — so what
/// appears afterwards is QEMU's own device with its own descriptors and its own
/// bytes, and the host-side verification below is the same one the ordinary
/// gate runs.
pub fn xhci_slow_connect(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const PARAMS: &[&str] = &["usb-storage-gate", "xhci-slow-connect"];
    /// Mirrors `xhci/mod.rs`'s `SLOW_CONNECT_NS` and `PORT_DEBOUNCE_NS`. These
    /// are one wire format with the kernel's in the same sense the gate's stamp
    /// is: a change to either without the other shows up as a failed assertion,
    /// not as a silent pass.
    const HELD_EMPTY_S: f64 = 0.300;
    const DEBOUNCE_S: f64 = 0.100;
    /// The earliest instant the driver can name a port. The register reads empty
    /// until `SLOW_CONNECT_NS` of *boot*, and `await_connect_settle` then wants
    /// `PORT_DEBOUNCE_NS` of a connect set that has held still and is non-empty.
    const FIRST_CONNECT_S: f64 = HELD_EMPTY_S + DEBOUNCE_S;
    /// How much later than that the first port line may be.
    ///
    /// The shape it exists to catch is a settle that leaves by `EMPTY_BUS_NS`
    /// instead of on the device appearing — one second after port power, so
    /// ~1.1 s of boot — which would enumerate the same two sticks and leave
    /// every other assertion here green.
    ///
    /// 150 ms because the connect becomes visible at a fixed instant on the
    /// guest's own clock and the settle re-reads it every `PORT_POLL_NS`, so the
    /// spread is a millisecond of polling and not a share of the host. Six runs
    /// here — three sequential, three with four concurrent test processes on the
    /// machine — put the first port line at 0.400-0.402 s, and `issues/`
    /// records one at 0.413 under five-agent load. 13 ms of worst observed
    /// excursion against 150 of slack, and 700 of clearance to the shape above.
    const SETTLE_SLACK_S: f64 = 0.150;

    let (bytes, lba) = Profile::UsbDisk.usb_disk().expect("UsbDisk declares a disk");
    let image = test_dir().join("usb-slow-connect.img");
    let nonce = stage(&image, bytes);

    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::UsbDisk,
            kernel_params: PARAMS,
            usb_images: vec![image.clone()],
            ..Default::default()
        },
    )?;

    // The driver looked at an empty bus and kept looking. Without this the test
    // would be green on a driver that never waits and a QEMU that answers
    // instantly, which is exactly the pair that shipped.
    //
    // Both instants below are read on the guest's own boot clock, because that
    // is the clock the injection is written in — `read_portsc` hides the
    // register while `nanos_since_boot() < SLOW_CONNECT_NS`. Timing the wait as
    // a delta from `controller started` instead compared a delta against an
    // absolute window, which silently required the boot to reach its controller
    // within `PORT_DEBOUNCE_NS`: a budget it has since grown out of, and four
    // red runs and an afternoon in the driver (`issues/`).
    let started = stamp_of(&log, "xHCI: controller started")?;
    // The first line this driver prints about any port at all. Every other
    // per-port line is preceded by that port's connect line, so the first match
    // is the first connect whichever port register it lands on — which the
    // profile does not fix, since a SuperSpeed stick appears on a high one.
    let first_seen = stamp_of(&log, "xHCI: port ")?;

    // Non-vacuity, and it comes first because the floor below rests on it: a
    // driver that only reached its ports after the window closed read a
    // populated bus on its first look, and the wait it then did was the ordinary
    // debounce every boot does.
    if started >= HELD_EMPTY_S {
        return Err(format!(
            "the controller started at {started:.3} s, past the {HELD_EMPTY_S} s the ports are \
             held empty for, so nothing in this boot read a hidden port. The boot has outgrown \
             the injection window: widen SLOW_CONNECT_NS, not this gate\n{log}"
        ));
    }
    // The floor. Nothing can name a port before the register stops lying and the
    // debounce behind it has elapsed, so an earlier line is a driver that did
    // not wait or an injection that did not land.
    if first_seen < FIRST_CONNECT_S {
        return Err(format!(
            "the first port was named at {first_seen:.3} s, before the {FIRST_CONNECT_S} s the \
             held-empty window and the debounce behind it come to — the injection did not reach \
             the driver\n{log}"
        ));
    }
    // The ceiling.
    if first_seen > FIRST_CONNECT_S + SETTLE_SLACK_S {
        return Err(format!(
            "the first port was named at {first_seen:.3} s, {:.3} s after the connect became \
             visible — the settle did not end on the device appearing\n{log}",
            first_seen - FIRST_CONNECT_S
        ));
    }

    // And it found everything, and the bytes are the host's.
    if !log.contains("usb-storage: 2 device(s)") {
        return Err(format!("the driver did not bind both sticks after the wait\n{log}"));
    }
    gate_ran(&log, 2)?;
    check_geometry(&log, bytes, lba)?;
    if !log.contains("usb-gate: disk done reads=ok writes=ok refusal=true wr_err=0 healthy=true") {
        return Err(format!("the guest did not report a clean pass\n{log}"));
    }
    verify(&image, bytes, nonce)?;
    // The guest's own boot stamp, printed rather than asserted on.
    //
    // **This is the log architecture's own named boot instrument for the
    // producer path's cost, and until 2026-08-15 it could not be read off the
    // test that *is* it.** What it is for is an interleaved A/B of that cost
    // against this boot's `Boot: complete`, and the stamp reached only the
    // per-run UART file, which goes when the guest does. So the measurement had
    // to instrument something — and the lesson
    // `issues/hardware/one-rmw-per-log-line-cost-350ms.md` leaves is that
    // the reading taken on an instrumented build is the one that misleads. One
    // line of output, `i8042_absent`'s arrangement, and the obligation is
    // re-runnable by anybody. It decides nothing: what is asserted is that the
    // boot finished, which is the `else` below.
    let Some(boot_ms) = log
        .split("Boot: complete (")
        .nth(1)
        .and_then(|rest| rest.split("ms)").next())
    else {
        return Err(format!("the boot did not finish\n{log}"));
    };
    let boot_ms = boot_ms.to_string();
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
    let _ = std::fs::remove_file(&image);

    eprintln!(
        "  [usb] controller started at {started:.3} s and the ports read empty to \
         {HELD_EMPTY_S} s; first port named at {first_seen:.3} s, both sticks bound, host bytes \
         verified host-side; Boot: complete at {boot_ms} ms"
    );
    Ok(())
}

/// A controller on which PORTSC's write-1-to-clear bits mean what the spec says
/// they mean — which QEMU's does not, and which is why every test in this suite
/// was green while five devices on the T14 all reported "not enabled after
/// reset".
///
/// PED is bit 1 and it is RW1CS: "A port may be disabled by software writing a
/// '1' to this flag" (xHCI 1.2 §5.4.8 Table 5-27), and §4.19.1.1.6 takes the
/// port from Enabled to Disabled when that write lands. §4.19.5 leaves PED and
/// PRC both set after a successful reset, so a read-modify-write that cleared
/// PRC by handing back everything else it read disabled the port it had just
/// enabled — on every port, on every controller, on any machine whose PORTSC is
/// made of silicon.
///
/// **The actuator is a boot parameter because nothing on the host side can
/// reach it.** QEMU's `xhci_port_write` clears only
/// `CSC|PEC|WRC|OCC|PRC|PLC|CEC` on a written '1', and PED is in neither that
/// set nor its read/write set, so writing PED=1 there does nothing at all
/// (`hw/usb/hcd-xhci.c`). No device or machine property changes that, and no
/// sequence of register writes reaches a PED=0/CCS=1 port either — clearing PP
/// is the closest and leaves PP=0, a different register state and a different
/// diagnosis. `xhci-portsc-rw1c` replaces the *register*: after the driver
/// writes PED=1 that port reads PED clear for every reader, and only a reset
/// clears it, because a reset is what takes a real port out of Disabled
/// (§4.19.1.1.3).
///
/// The count line is what stops this from passing because nothing was armed.
/// Only the emulation prints it, and it has to say zero — so "the injection is
/// live" and "the driver never wrote PED" are separate assertions, and the
/// per-port ones below are the register's own consequence rather than a verdict.
pub fn xhci_portsc_rw1c(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    // Six devices rather than one, because the T14's failure was every port at
    // once: a machine with a single stick cannot tell "one port survived" from
    // "ports survive". The hub is a device the driver walks past, and the boot
    // stick attaches at SuperSpeed, so both protocols' reset paths run here.
    let options = BootOptions {
        profile: Profile::MetalUsb,
        kernel_params: &["xhci-portsc-rw1c"],
        ..Default::default()
    };
    let argv = qemu::profile_argv(&options);
    let usb = crate::usb_argv(&argv);
    if usb.len() < 4 {
        return Err(format!("this gate needs a crowded bus, argv has {usb:?}"));
    }

    let qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
    let log = qemu.boot_log().to_string();

    // The emulation ran and saw nothing. Without the first half a boot with the
    // feature accidentally off passes everything below it.
    const ACCOUNTED: &str = "xHCI: PED as RW1C, ";
    let Some(verdict) = log.lines().find(|l| l.contains(ACCOUNTED)) else {
        return Err(format!("the PED emulation never reported; was it compiled in?\n{log}"));
    };
    if !verdict.contains("0 port(s) disabled by a driver write") {
        return Err(format!("the driver wrote PED=1 to a port: {verdict:?}\n{log}"));
    }

    // And the register's own consequence: every port that connected came out of
    // its reset enabled. This is the pair of counts the T14 printed as 5 and 0.
    let mut connected = 0usize;
    let mut enabled = 0usize;
    let mut refused: Vec<&str> = Vec::new();
    for line in log.lines() {
        let Some(rest) = line.split("xHCI: port ").nth(1) else { continue };
        if rest.contains("connected") {
            connected += 1;
        }
        if rest.contains("enabled, speed=") {
            enabled += 1;
        }
        if rest.contains("not enabled") || rest.contains("never finished its reset") {
            refused.push(line);
        }
    }
    if !refused.is_empty() {
        return Err(format!("{} port(s) refused: {refused:?}\n{log}", refused.len()));
    }
    if connected != usb.len() {
        return Err(format!(
            "{connected} port(s) reported a device, {} on the bus:\n{log}",
            usb.len()
        ));
    }
    if enabled != connected {
        return Err(format!(
            "{connected} port(s) connected and {enabled} reached the Enabled state:\n{log}"
        ));
    }

    // Enabled is not enumerated. A port can read PED=1 and still produce
    // nothing, so the devices behind these ports have to come out the far end.
    let slots = crate::parse_xhci_slots(&log);
    if slots.len() != usb.len() {
        return Err(format!(
            "{} slots enabled for {} devices ({slots:?}):\n{log}",
            slots.len(),
            usb.len()
        ));
    }
    let binds = crate::parse_xhci_binds(&log);
    let keyboards = binds.iter().filter(|b| b.kind == "keyboard").count();
    if keyboards != 2 {
        return Err(format!("{keyboards} keyboards bound, want 2: {binds:?}\n{log}"));
    }
    let disks = log.matches("usb-storage: disk ").count();
    if disks != 1 {
        return Err(format!("{disks} disks bound, want the boot stick:\n{log}"));
    }
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish\n{log}"));
    }
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;

    eprintln!(
        "  [xhci] PED honoured as RW1C: {connected}/{connected} ports connected reached Enabled, \
         0 disabled by a driver write, {} slots, {keyboards} keyboards, {disks} disk",
        slots.len()
    );
    Ok(())
}

/// A bulk transfer that breaks **without halting the endpoint**, which is the
/// shape the recovery path had no answer for.
///
/// The first metal boot with a working USB stack mounted `/boot` off a stick and
/// then lost it: a WRITE(10) broke, `clear_stall` opened with a Reset Endpoint
/// command, and the controller answered **completion code 19, Context State
/// Error** — twice, once per endpoint. xHCI 1.2 §4.6.8 defines Reset Endpoint
/// only for a Halted endpoint; §4.6.9's Stop Endpoint is the command for a
/// Running one. `reset_recovery` returned false, `dev.failed` was set, nothing
/// in this driver ever clears that flag, and the machine's own boot disk was
/// offline for the rest of the boot with `/boot/toyos/kernel.log` — the only
/// diagnostic channel that machine has — stopped where it stood.
///
/// **The actuator is a boot parameter, and it replaces no verdict.** QEMU's
/// `usb-storage` answers every CBW, data phase and CSW it is handed; nothing on
/// the host side makes one bulk transfer not complete, and `rerror`/`werror`
/// fail a whole drive rather than leaving a transfer in flight.
/// `usb-transport-break` skips the *wait* on one data phase and nothing else:
/// the TRB is really on the ring, the endpoint is really left Running, and the
/// controller really completes the transfer afterwards. That is the state a
/// transfer which ran out `USB_TIMEOUT_NS` leaves behind, byte for byte, so the
/// recovery under test runs against a real endpoint state rather than a flag.
///
/// **One injection is one abandoned transfer, and not one broken transfer.** The
/// device answers that transfer after the driver has stopped listening, and
/// where that answer lands relative to the Bulk-Only Reset decides whether the
/// reset holds: the guest wins the race under KVM and loses it under TCG, so
/// this boot produces one break on the dev host and two on CI, off the same
/// tree. Counting breaks is therefore a count of who won a race. What the driver
/// owes either way is that the caller's write survives, and that is what is
/// asserted here.
///
/// The assertion that decides it is host-side and is about bytes: **every** block
/// the guest was told to write is byte-correct in the backing file, the broken
/// one included. Before the recovery existed the disk is offline from the break
/// onward, so the nine-block run and the second guest block never leave the guest
/// at all; before the command was re-issued over the transport the recovery gave
/// back, the broken block is missing from the image and `wr_err` is not zero.
pub fn usb_transport_break(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const PARAMS: &[&str] = &["usb-storage-gate", "usb-transport-break"];

    let (bytes, lba) = Profile::UsbDisk.usb_disk().expect("UsbDisk declares a disk");
    let image = test_dir().join("usb-transport-break.img");
    let nonce = stage(&image, bytes);

    let log = boot_and_shutdown(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::UsbDisk,
            kernel_params: PARAMS,
            usb_images: vec![image.clone()],
            ..Default::default()
        },
    )?;
    gate_ran(&log, 2)?;
    check_geometry(&log, bytes, lba)?;

    // The injection reached the driver, and the driver said *what* broke. Both
    // halves are assertions: without the first this is a boot with nothing
    // injected, and without the second the log says only that something went
    // wrong — which is the line the T14 produced, and the reason the cause of
    // that break cannot be read out of its log today.
    const BROKE: &str = "transport broke on SCSI 0x2a: no answer in the data phase";
    let staged: Vec<&str> = log.lines().filter(|l| l.contains(BROKE)).collect();
    if staged.len() != 1 {
        return Err(format!(
            "the staged break happened {} times, want the one the injection arms per \
             boot; did it run?\n{log}",
            staged.len()
        ));
    }
    // **And every count below is about that one device.** The actuator abandons
    // the first WRITE(10) of the boot, so the line above names the disk under
    // test; this profile carries a second one — the stick the machine booted
    // from — whose own transport can break in the same boot for reasons that
    // have nothing to do with the injection. `log.matches("transport broke")`
    // summed both, and on CI run 31684437719 the boot stick's clean
    // status-phase recovery at 2.616 s (one break, one retry, `SCSI 0x35`,
    // slot 1) pushed the total from the injected disk's real 2 to 3 and reddened
    // the run (`issues/hardware/usb-transport-break-counts-the-boot-sticks-recovery.md`).
    let under_test = broke_on(staged[0])?;

    // And the driver got over it. Two attempts are explained by the fault — the
    // fault itself, and the recovery the device's late answer to the abandoned
    // transfer can undo — so a third that also breaks is the transport failing
    // to come back rather than this test's doing.
    let mine = format!("usb-storage: {under_test} transport broke");
    let breaks = log.matches(mine.as_str()).count();
    if breaks > 2 {
        let all: Vec<&str> = log.lines().filter(|l| l.contains("transport broke")).collect();
        return Err(format!(
            "{under_test}'s transport broke {breaks} times off one abandoned transfer, which \
             can undo one recovery and no more; every break this boot: {all:?}\n{log}"
        ));
    }
    // Left counting every device on the machine on purpose, and it is not the
    // shape above: a break the driver recovers from belongs to the disk it
    // happened on, but a transport that never comes back takes the boot with it
    // whichever disk it was.
    if let Some(gave_up) = log.lines().find(|l| l.contains("times running; the transport is not")) {
        return Err(format!("{gave_up:?}\n{log}"));
    }

    // The endpoint state the recovery had to be chosen for, read out of the
    // controller's own output device context. `Halted` here would mean the
    // injection staged the other shape and everything below proves nothing.
    // Scoped to the disk under test for the same reason the count is: another
    // device's endpoint being found Running says nothing about this one's.
    let recovered = format!("xHCI: {under_test} endpoint");
    let states: Vec<&str> = log.lines().filter(|l| l.contains(", recovering")).collect();
    if !states.iter().any(|l| l.contains(recovered.as_str()) && l.contains("is Running,")) {
        return Err(format!(
            "no endpoint of {under_test} was found Running after the break, so this is not the \
             non-halt shape: {states:?}\n{log}"
        ));
    }

    // `run_command` logs only failures, so each of these lines is the
    // controller refusing a command the driver should not have sent — which is
    // exactly what the T14 printed twice.
    for illegal in [
        "Reset Endpoint failed",
        "Stop Endpoint failed",
        "Set TR Dequeue failed",
        "reset recovery failed; disk is offline",
    ] {
        if log.contains(illegal) {
            return Err(format!(
                "{illegal:?}: the recovery did not pick a command the endpoint's state \
                 permits\n{log}"
            ));
        }
    }

    // Not one write reported a failure, and the disk stayed online. Before the
    // recovery existed this line reads `wr_err=3 healthy=false`; before the
    // command was re-issued it reads `writes=bad ... wr_err=1`.
    if !log.contains("usb-gate: disk done reads=ok writes=ok refusal=true wr_err=0 healthy=true") {
        return Err(format!("a write did not survive one broken transfer\n{log}"));
    }

    // And the bytes, which is the claim nothing in the guest can make for
    // itself: every block the guest was told to write is in the backing file,
    // the host's own blocks are unchanged, and the blocks either side of the
    // run are still zero.
    verify(&image, bytes, nonce)?;
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish after the break\n{log}"));
    }
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
    let _ = std::fs::remove_file(&image);

    eprintln!(
        "  [usb] a bulk transfer abandoned mid-flight on {under_test}: the endpoint was found \
         Running and stopped rather than reset, that device's transport came back in {breaks} \
         break(s) of the {} this boot, and every block the guest was told to write verified \
         host-side",
        log.matches("transport broke").count()
    );
    Ok(())
}

/// Which device a `usb-storage: <bdf> slot <n> transport broke …` line is about.
///
/// **Refused rather than widened if the line stops naming one.** A count of
/// broken transports is evidence about a disk, and a machine that boots off USB
/// always has at least two: the answer to "how many times did *this* disk's
/// transport break" is not recoverable from a line that does not say which disk
/// it was, and matching every disk's line instead is how this test came to red
/// on a boot stick's own clean recovery.
fn broke_on(line: &str) -> Result<&str, String> {
    line.split_once("usb-storage: ")
        .and_then(|(_, rest)| rest.split_once(" transport broke"))
        .map(|(who, _)| who)
        .ok_or_else(|| {
            format!("{line:?} does not name the device whose transport broke, so nothing can \
                    count that device's breaks apart from another's")
        })
}

/// A SuperSpeed port is not reset into existence, and the driver knows which
/// ports are which because it read the controller's own description of itself.
///
/// The Supported Protocol capability (§7.2) was never parsed, so every port
/// register looked alike and every one got the USB2 treatment: write PR, wait
/// for PRC. A USB3 link trains itself and reaches Enabled with nothing done to
/// it (§4.19.1.2), so that write is a *hot reset of a working link* — and a
/// link that cannot take one lands Inactive, which only a warm reset this
/// driver did not have would have left. On the T14 that is a USB-A socket that
/// mounts nothing, two boots out of two, while the same stick through a Type-C
/// adapter mounts every time: the adapter lands it on the connector's USB2
/// pins, and a USB2 port is the one shape the old driver knew.
///
/// **What this gate can and cannot say.** QEMU's xHC publishes real Supported
/// Protocol capabilities, so the decode and the branch are certified here
/// against a controller's own bytes. It has no link training and no Inactive
/// state, so the warm-reset recovery is unreachable and is certified by the
/// host model instead (`toyos-xhci/sim/tests/superspeed.rs`). This says: the
/// driver read the split correctly and stopped resetting the ports that did not
/// need it. It says nothing about what happens when a link falls over.
pub fn xhci_superspeed_ports(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let options = BootOptions { profile: Profile::MetalUsb, ..Default::default() };
    let devices = crate::usb_argv(&qemu::profile_argv(&options)).len();
    let log = boot_and_shutdown(test_config, c_bins, rust_bins, options)?;

    // The controller described itself and the driver read it. `nec-usb-xhci`
    // with `p2=8` is four SuperSpeed registers and eight USB2 ones, and the
    // driver has to name that split without being told it.
    const SPLIT: &str = "xHCI: 8 USB2 and 4 USB3 port register(s) of 12 named, \
                         0 capability(ies) refused";
    if !log.contains(SPLIT) {
        let got = log.lines().find(|l| l.contains("port register(s) of"));
        return Err(format!(
            "the driver did not read the controller's protocol split; got {got:?}\n{log}"
        ));
    }

    // The boot stick is the only SuperSpeed device this profile attaches, so it
    // takes a USB3 register and every HID takes a USB2 one. Exactly one port
    // must therefore have been brought up with no reset at all.
    let trained: Vec<&str> = log.lines().filter(|l| l.contains("link already trained")).collect();
    if trained.len() != 1 {
        return Err(format!(
            "{} port(s) came up on an already-trained link, want the SuperSpeed stick alone: \
             {trained:?}\n{log}",
            trained.len()
        ));
    }

    // And every device still reached Enabled, so not resetting cost nothing.
    let enabled = log.matches("enabled, speed=").count();
    if enabled != devices {
        return Err(format!(
            "{enabled} port(s) reached Enabled, {devices} devices on the bus\n{log}"
        ));
    }
    for wrong in ["never finished its reset", "would not train", "warm reset"] {
        if let Some(line) = log.lines().find(|l| l.contains(wrong)) {
            return Err(format!("{line:?} on a bus where every link is healthy\n{log}"));
        }
    }
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish\n{log}"));
    }
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;

    eprintln!(
        "  [xhci] the controller's own capability names 8 USB2 and 4 USB3 registers; the \
         SuperSpeed stick is enumerated on a link that was already trained, and {enabled} \
         port(s) reached Enabled"
    );
    Ok(())
}

/// A device that attaches at **full speed**, where EP0's max packet size is a
/// thing only the device knows.
///
/// Low, High and SuperSpeed each fix it at 8, 64 and 512, and every USB device
/// in this suite was one of those — so a driver that answered 64 for full speed
/// and read all 18 bytes of the device descriptor in one go passed everything
/// here. A T14's port 9 came up at speed 1 and answered
/// `GET_DESCRIPTOR(Config) failed, code=Some(4)` — USB Transaction Error — after
/// the driver had already logged `vendor=0000 product=0000` off a buffer no
/// transfer had filled.
///
/// Two things are asserted and they are separate. The **sequence**: the driver
/// reads eight bytes, takes `bMaxPacketSize0` from them, and only then reads the
/// rest. The **error channel**: what it prints about a device is what the device
/// sent, so a read that delivered nothing can never be logged as a device whose
/// identifiers are zero.
///
/// Ground truth is host-side in the sense that matters here — QEMU's descriptor
/// tables are the host's bytes and a guest cannot invent them. `usb-wacom-tablet`
/// is full-speed only: QEMU gives it a `.full` descriptor set and no `.high` one,
/// so `usb_desc_attach` has no faster speed to choose.
pub fn xhci_full_speed_device(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    /// Each device's own `idVendor`, out of QEMU's descriptor tables: PenPartner
    /// for the tablet, Gemalto for the reader. The host's bytes, and the thing in
    /// a device descriptor a guest cannot have guessed.
    const VENDORS: [&str; 2] = ["vendor=056a", "vendor=08e6"];
    /// What the reader answers to the eight-byte prefix, measured on QEMU
    /// 11.0.2. The tablet answers 8 and so needs no correction — which is the
    /// other half of the claim, because a driver that wrote a constant would
    /// produce this line for both devices or for neither.
    const CORRECTED: &str = "EP0 packet size 8 -> 64";

    let options = BootOptions {
        profile: Profile::MetalFullSpeed,
        ..Default::default()
    };
    // The claim is that full-speed devices are on the bus, and argv is where a
    // device's presence is visible: no console line distinguishes "the driver
    // did not enumerate it" from "it was never attached".
    let argv = qemu::profile_argv(&options);
    let usb = crate::usb_argv(&argv);
    for want in ["usb-wacom-tablet", "usb-ccid"] {
        if !usb.iter().any(|d| d.starts_with(want)) {
            return Err(format!("this gate needs {want} on the bus, argv has {usb:?}"));
        }
    }

    let log = boot_and_shutdown(test_config, c_bins, rust_bins, options)?;

    // Speed 1 is Full Speed in PORTSC, and it is the premise of the whole test:
    // on a bus of high- and SuperSpeed devices EP0's packet size is fixed by
    // the specification and nothing below here has anything to measure.
    let full_speed: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("xHCI: port ") && l.contains("enabled, speed=1"))
        .collect();
    if full_speed.len() != 2 {
        return Err(format!(
            "{} port(s) came up at full speed, want both: {full_speed:?}\n{log}",
            full_speed.len()
        ));
    }

    // **The sequence.** 64 is a number the driver can only have got by reading
    // the first eight bytes of the reader's device descriptor and issuing
    // Evaluate Context with what it found — the eighth byte is `bMaxPacketSize0`
    // and QEMU's `desc_device_ccid` is where this 64 comes from. Exactly one
    // such line, because the tablet on the same bus answers 8: a driver that
    // wrote a constant for full speed produces this line twice or not at all,
    // and the shipped one produced it never.
    let corrected: Vec<&str> = log.lines().filter(|l| l.contains("EP0 packet size")).collect();
    match corrected.as_slice() {
        [only] if only.contains(CORRECTED) => {}
        other => {
            return Err(format!(
                "want exactly one endpoint resized, to the {CORRECTED:?} the reader asked \
                 for; got {other:?}\n{log}"
            ));
        }
    }

    // **The error channel.** What the driver prints about a device is what the
    // device sent. Both identities are the host's bytes; an all-zero one is what
    // an unfilled buffer looks like, and it is what a T14 port printed off a
    // transfer that had delivered no descriptor at all.
    for vendor in VENDORS {
        if !log.contains(vendor) {
            return Err(format!(
                "the driver never reported {vendor:?}; a device descriptor that was not \
                 delivered must not be logged as one that was\n{log}"
            ));
        }
    }
    if log.contains("vendor=0000 product=0000") {
        return Err(format!(
            "a device was logged with an all-zero identity, which is what an unfilled \
             descriptor buffer looks like\n{log}"
        ));
    }
    for wrong in ["GET_DESCRIPTOR(Device)", "GET_DESCRIPTOR(Config)", "code=Some("] {
        if let Some(line) = log.lines().find(|l| l.contains(wrong)) {
            return Err(format!("{line:?}\n{log}"));
        }
    }

    // And one came out the far end: a full-speed HID enumerated, bound, and took
    // a button-merge source. `Enabled` is not `enumerated`, and neither is
    // `addressed`. The reader is not a HID and is walked past by name.
    let binds = crate::parse_xhci_binds(&log);
    if binds.len() != 1 || binds[0].kind != "mouse" {
        return Err(format!("want exactly the full-speed pointer bound, got {binds:?}\n{log}"));
    }
    if !log.contains("xHCI: no HID boot interface found") {
        return Err(format!("the reader was not walked past\n{log}"));
    }
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish\n{log}"));
    }
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;

    eprintln!(
        "  [xhci] two full-speed devices enumerated: one EP0 resized to 64 from the reader's \
         own bMaxPacketSize0 and the tablet's 8 left alone, both identities read off the wire"
    );
    Ok(())
}

/// A HID interrupt endpoint whose transfer completes with a code the driver did
/// not expect.
///
/// `dispatch_event` requeued a bound device's interrupt TRB only for Success and
/// Short Packet. **Every other code was dropped where it was read** — no log
/// line, no requeue, no fault — and that endpoint carries exactly one TRB, so
/// the device went silent for the rest of the boot with every bind-time line
/// reading perfectly. A Logitech mouse hot-plugged into the T14 did exactly
/// that: `HID mouse ready on slot 6 … merges as source 1` at 30.485 s and not
/// one motion event until it was unplugged at 58.659 s.
///
/// Both timings, because they are different states of the driver and neither is
/// a weaker version of the other: the **fourth** completion is a device that has
/// been delivering and stops, and the **first** is a freshly configured endpoint
/// that never delivered at all — which is the shape the T14 showed and the one
/// whose recovery has to work before any report has ever arrived.
///
/// The actuator is a boot parameter and `xhci/hid.rs`'s `stage_break` says why
/// nothing on the host side can reach it. What it replaces is the completion
/// code **and the report that transfer delivered**: QEMU really moved a mouse
/// report into the buffer, so a driver that dispatched it despite the error
/// would publish a delta it never earned and this gate would pass against the
/// defect it names. Everything the recovery reads is the controller's own — the
/// Endpoint State out of the output device context, and three commands the
/// controller really answers.
///
/// Ground truth is host-side: the keys and the pointer delta injected **after**
/// the staged failure arrive in the guest's own event stream, on a machine
/// (`i8042=off`, boot-time HID absolute-only) where no other device can produce
/// either.
pub fn xhci_hid_break(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    for (features, which) in [
        (
            &["xhci-hid-break-first"][..],
            "the very first completion, before the device ever delivered",
        ),
        (
            &["xhci-hid-break-late"][..],
            "the fourth completion, after the device had been delivering",
        ),
    ] {
        hid_break_boot(test_config, c_bins, rust_bins, features, which)?;
    }
    Ok(())
}

/// One boot of [`xhci_hid_break`], with the break staged at whichever
/// completion `feature` names.
fn hid_break_boot(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
    params: &'static [&'static str],
    which: &str,
) -> Result<(), String> {
    /// The delta the assertion is about, injected after the break is spent.
    /// Neither component is a number any other move in this boot produces.
    const DX: i32 = 40;
    const DY: i32 = -30;
    /// Typed after the break is spent, and nowhere else in the boot.
    const WORD: [&str; 5] = ["h", "e", "l", "l", "o"];
    /// Both QEMU HID devices carry one IN interrupt endpoint at address 1, so
    /// this is the device's own number for it and the controller's, and a line
    /// naming either wrongly stops matching.
    const ENDPOINT: &str = "interrupt endpoint 0x81 (dci 3)";

    let options = BootOptions {
        profile: Profile::MetalHotplug,
        qmp: true,
        // The only keyboard on the machine has to be the one plugged in, or
        // QEMU delivers the keystrokes over PS/2 and every assertion below
        // passes with the interrupt endpoint dead.
        i8042: false,
        kernel_params: params,
        ..Default::default()
    };
    let argv = qemu::profile_argv(&options);
    let usb = crate::usb_argv(&argv);
    for absent in ["usb-kbd", "usb-mouse"] {
        if usb.iter().any(|d| d.starts_with(absent)) {
            return Err(format!("{absent} is on the bus at boot; argv has {usb:?}"));
        }
    }
    if !argv.iter().any(|a| a.contains("i8042=off")) {
        return Err("the i8042 is on; a PS/2 keyboard could deliver instead".to_string());
    }

    let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
    let boot = qemu.boot_log().to_string();
    let Some((scale_x, scale_y)) = crate::parse_rel_scale(&boot) else {
        return Err(format!("the kernel never said what pointer scale it used:\n{boot}"));
    };

    let result = qemu.run_test_hooked(
        "test_rs_input_events",
        Duration::from_secs(60),
        "===INPUT_READY===",
        move |socket| {
            let mut devices = qemu::QmpDevices::open(socket);
            devices.add("usb-mouse", "xhci1.0", "hidmouse", &[]);
            devices.add("usb-kbd", "xhci1.0", "hidkbd", &[]);
            drop(devices);
            thread::sleep(Duration::from_millis(800));

            // Spend the break. Ten pointer completions and six keyboard ones,
            // against an injection that strikes the first or the fourth: the
            // margin is for QEMU coalescing rel events it has not been polled
            // for, which can only make the count smaller.
            let mut input = qemu::QmpInput::open(socket);
            for _ in 0..10 {
                input.mouse(4, 4, None);
                thread::sleep(Duration::from_millis(60));
            }
            for key in ["a", "b", "c"] {
                input.keys(&[(key, true), (key, false)]);
                thread::sleep(Duration::from_millis(60));
            }
            drop(input);
            thread::sleep(Duration::from_millis(300));

            // And the measured phase, every event of which is after the break.
            let mut input = qemu::QmpInput::open(socket);
            // The accumulated position clamps at 0, so a move up or left from
            // the origin is invisible — and with the first completion eaten the
            // pointer may still be sitting there.
            input.mouse(200, 200, None);
            thread::sleep(Duration::from_millis(150));
            input.mouse(DX, DY, None);
            thread::sleep(Duration::from_millis(150));
            for key in WORD {
                input.keys(&[(key, true), (key, false)]);
                thread::sleep(Duration::from_millis(30));
            }
            crate::input_events_end(&mut input);
            drop(input);
            thread::sleep(Duration::from_millis(200));
        },
    );
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}\n{}", result.serial, result.stdout));
    }
    let log = format!("{boot}{}", result.serial);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} with the break staged at {which}\n{log}"));
        }
    }

    // **Delivery first**, because it is what the gate is about and what the
    // pre-fix driver cannot do: an endpoint whose completion was dropped holds
    // no TRB and nothing ever puts one back, so everything injected after the
    // break stays on the host side of the wire.
    let word: String = WORD.concat();
    hotplug_delivered(&result.stdout, &word, (DX * scale_x, DY * scale_y)).map_err(|e| {
        format!("with the break staged at {which}, input never came back: {e}\n{log}")
    })?;

    // Then that a break was staged at all, and that the line names the device,
    // the endpoint and the code. Without this the boot above is one where
    // nothing failed — and the line itself is the instrument: the T14's log
    // cannot name the code its mouse died of, because the driver discarded it.
    let named: Vec<&str> = log.lines().filter(|l| l.contains(ENDPOINT)).collect();
    let want = format!("{ENDPOINT} completed with code 6 (Stall Error); failure 1 of 8");
    let staged: Vec<&&str> = named.iter().filter(|l| l.contains(want.as_str())).collect();
    if staged.len() != 2 {
        return Err(format!(
            "{} endpoint(s) reported a broken completion, want the mouse and the \
             keyboard: {named:?}\n{log}",
            staged.len()
        ));
    }
    // Which two devices those were, by the controller they are on and the slot
    // they hold on it — and both halves are needed. **A slot id is one
    // controller's numbering and this machine has two**: the boot disk is slot 1
    // on `00:02.0` and the mouse plugged in below is slot 1 on `00:03.0`.
    let mut broken: Vec<(&str, &str)> = Vec::new();
    for line in &staged {
        broken.push(hid_broke_on(line)?);
    }
    for kind in ["pointer", "keyboard"] {
        if !broken.iter().any(|(k, _)| *k == kind) {
            return Err(format!("no {kind:?} among the broken completions {broken:?}\n{log}"));
        }
    }
    if broken[0].1 == broken[1].1 {
        return Err(format!(
            "both broken completions are on {}, so this boot broke one device twice rather \
             than the mouse and the keyboard once each\n{log}",
            broken[0].1
        ));
    }

    // The endpoint state the recovery had to be chosen for, read out of the
    // controller's own output device context. The transfer really completed, so
    // the endpoint really is Running — `Halted` here would mean the injection
    // staged a shape this boot cannot produce and everything above proves
    // something else.
    //
    // **Once for each of the two devices the injection struck, and no longer a
    // count over the whole boot.** `endpoint 3` is the first IN endpoint of
    // *every* USB device — the boot disk's bulk IN as much as a HID interrupt
    // endpoint — so one transport recovery on the boot disk anywhere in the boot
    // used to red this test with a failure about HID: three CI runs did exactly
    // that, e.g. `31405969578` shard 10, where the disk's own
    // `slot 1 endpoint 3` and `slot 1 endpoint 4` at 2.639 s were counted beside
    // the mouse's and the keyboard's
    // (`issues/hardware/xhci-hid-break-counts-any-endpoint-3.md`).
    let mut recovered: Vec<(&str, usize)> = Vec::new();
    for (_, who) in &broken {
        let running = format!("xHCI: {who} endpoint 3 is Running, recovering");
        recovered.push((who, log.matches(running.as_str()).count()));
    }
    if recovered.iter().any(|(_, n)| *n != 1) {
        let states: Vec<&str> = log.lines().filter(|l| l.contains(", recovering")).collect();
        return Err(format!(
            "the two devices the injection struck were found Running {recovered:?} time(s), \
             want once each; every recovery this boot: {states:?}\n{log}"
        ));
    }

    // `run_command` logs only refusals, so each of these is the controller
    // declining a command the endpoint's state did not permit.
    for illegal in [
        "Reset Endpoint failed",
        "Stop Endpoint failed",
        "Set TR Dequeue failed",
        "would not clear the halt",
        "is being let go",
    ] {
        if log.contains(illegal) {
            return Err(format!("{illegal:?} after a single staged failure\n{log}"));
        }
    }
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;

    eprintln!(
        "  [xhci] a HID interrupt endpoint broken at {which}: {broken:?} named the code, were \
         each found Running once and restarted (of {} recoveries in the boot), and {word:?} \
         plus a {:?} pointer delta crossed them afterwards",
        log.matches("is Running, recovering").count(),
        (DX * scale_x, DY * scale_y)
    );
    Ok(())
}

/// Which device an `xHCI: USB <kind> on <bdf> slot <n>: interrupt endpoint …`
/// line is about, as the kind and the device.
///
/// **Refused rather than widened if the line stops naming one.** Recovery lines
/// carry `<bdf> slot <n>` and nothing else that identifies a device, so a test
/// that cannot read this pair off the completion has no way to tell its own
/// device's recovery from another device's — and the only alternative to
/// refusing is the count over every device that reddened this test three times.
fn hid_broke_on(line: &str) -> Result<(&str, &str), String> {
    line.split_once("xHCI: USB ")
        .and_then(|(_, rest)| rest.split_once(": interrupt endpoint"))
        .and_then(|(who, _)| who.split_once(" on "))
        .ok_or_else(|| {
            format!("{line:?} does not name the device and the controller its endpoint broke \
                    on, so nothing can tell that device's recovery from another's")
        })
}

/// Devices plugged in **after** the machine has booted.
///
/// The driver enumerated once, from `init`, and `dispatch_event` advanced past
/// every TRB that was not a transfer completion — Port Status Change Events
/// included. So the set of USB devices was whatever was connected at boot,
/// forever, and a keyboard plugged into a machine with no input did nothing at
/// all: no port line, no slot, no event, and a compositor already holding a
/// keyboard claim that would never produce anything. That machine is
/// indistinguishable from hung, and it is the first thing a person tries.
///
/// **The actuator is QEMU's own `device_add`, and nothing about the driver is
/// modified to run this.** A USB device attached at runtime goes through the
/// same `usb_device_attach` → `xhci_port_update` → `xhci_port_notify` path a
/// device attached at startup does, so what the guest sees is a real Port
/// Status Change Event with a real device behind it. That is the whole
/// difference from `xhci_slow_connect`, which needs an actuator because
/// it has to aim at a window the boot opens and closes in milliseconds; here
/// the window is the entire life of the machine.
///
/// Every claim below is host-side in the sense that matters:
///
/// - **the keyboard** is the only one on the machine — `i8042=off`, and the
///   profile's boot-time HID is a tablet — so a keystroke that reaches
///   userland can only have crossed a device that was added after the boot;
/// - **the pointer** is the only *relative* one, so QEMU has no handler for an
///   injected `rel` event until it is plugged in, and the boot-time tablet
///   cannot stand in for it;
/// - **the disk's block count** is the size of a file the harness made, which
///   the guest can only have learned by running READ CAPACITY over the wire.
pub fn xhci_hotplug(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    /// The disk that arrives late: 48 GiB, which is no other device in this
    /// suite and no round number the driver could have printed by accident.
    /// Sparse, so the host pays for nothing.
    const HOT_DISK_BYTES: u64 = 48 * 1024 * 1024 * 1024;
    /// What the boot-time tablet takes, so what a late pointer must not.
    const BOOT_SOURCE: u32 = 1;
    const LATE_SOURCE: u32 = 2;
    const DX: i32 = 40;
    const DY: i32 = -30;

    let options = BootOptions {
        profile: Profile::MetalHotplug,
        qmp: true,
        // With an i8042 on the machine QEMU would deliver the injected
        // keystrokes over PS/2 and every assertion below would pass with the
        // hot-plug path dead.
        i8042: false,
        ..Default::default()
    };
    // The claim is about what is *not* on the machine at boot, and argv is the
    // only place absence is visible: no console line distinguishes "the driver
    // never enumerated a keyboard" from "there was never one to enumerate".
    let argv = qemu::profile_argv(&options);
    let usb = crate::usb_argv(&argv);
    for absent in ["usb-kbd", "usb-mouse"] {
        if usb.iter().any(|d| d.starts_with(absent)) {
            return Err(format!("{absent} is on the bus at boot; argv has {usb:?}"));
        }
    }
    if !usb.iter().any(|d| d.starts_with("usb-tablet")) {
        return Err(format!("this gate needs the boot-time tablet, argv has {usb:?}"));
    }
    if !argv.iter().any(|a| a.contains("i8042=off")) {
        return Err("the i8042 is on; a PS/2 keyboard could deliver instead".to_string());
    }

    let image = test_dir().join("usb-hotplug.img");
    drop(sparse(&image, HOT_DISK_BYTES));

    let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
    let boot = qemu.boot_log().to_string();

    // Both controllers came up and exactly one of them found nothing — the
    // T14's Thunderbolt xHC exactly, and the controller everything below is
    // plugged into. Without this the test could not tell a device enumerated
    // late from one enumerated at boot on a port nobody looked at.
    let found = boot.matches("xHCI: found at PCI ").count();
    if found != 2 {
        return Err(format!("{found} controller(s) initialised, want 2:\n{boot}"));
    }
    let empty = boot.matches("xHCI: no HID devices on the controller").count();
    if empty != 1 {
        return Err(format!(
            "{empty} controller(s) reported an empty bus at boot, want the second one only:\n{boot}"
        ));
    }
    let at_boot = crate::parse_xhci_binds(&boot);
    if at_boot.len() != 1 || at_boot[0].kind != "tablet" {
        return Err(format!("want exactly the tablet bound at boot, got {at_boot:?}\n{boot}"));
    }
    let booted: Vec<u32> = crate::parse_pointer_sources(&boot).iter().map(|(_, s)| *s).collect();
    if booted != vec![BOOT_SOURCE] {
        return Err(format!(
            "the boot-time tablet did not take source {BOOT_SOURCE} alone: {booted:?}\n{boot}"
        ));
    }
    let Some((scale_x, scale_y)) = crate::parse_rel_scale(&boot) else {
        return Err(format!("the kernel never said what pointer scale it used:\n{boot}"));
    };

    let hot_image = image.clone();
    let result = qemu.run_test_hooked(
        "test_rs_input_events",
        Duration::from_secs(60),
        "===INPUT_READY===",
        move |socket| {
            // One monitor at a time: a `-qmp unix:…,server` socket serves one
            // connection, so each phase opens, does its work and closes.
            let mut devices = qemu::QmpDevices::open(socket);
            devices.blockdev_add("hotdisk", &hot_image);
            devices.add("usb-mouse", "xhci1.0", "hotmouse", &[]);
            devices.add("usb-kbd", "xhci1.0", "hotkbd", &[]);
            devices.add("usb-storage", "xhci1.0", "hotdisk0", &[("drive", "hotdisk")]);
            drop(devices);
            // The driver's own debounce is 100 ms and the enumeration behind it
            // is microseconds under TCG; this is that with room, not a settling
            // time the assertions depend on.
            thread::sleep(Duration::from_millis(800));

            let mut input = qemu::QmpInput::open(socket);
            // Off the origin first: the accumulated position clamps at 0, so a
            // move up or left from there is invisible.
            input.mouse(100, 100, None);
            thread::sleep(Duration::from_millis(100));
            input.mouse(DX, DY, None);
            thread::sleep(Duration::from_millis(100));
            for key in ["h", "e", "l", "l", "o"] {
                input.keys(&[(key, true), (key, false)]);
                thread::sleep(Duration::from_millis(20));
            }
            drop(input);
            thread::sleep(Duration::from_millis(200));

            let mut devices = qemu::QmpDevices::open(socket);
            devices.del("hotmouse");
            devices.del("hotdisk0");
            drop(devices);
            thread::sleep(Duration::from_millis(800));

            // The keyboard is still there, and still the only one.
            let mut input = qemu::QmpInput::open(socket);
            for key in ["w", "o", "r", "l", "d"] {
                input.keys(&[(key, true), (key, false)]);
                thread::sleep(Duration::from_millis(20));
            }
            drop(input);

            // And a pointer plugged in where the last one was unplugged, which
            // is the only thing that can show the button-table entry came back:
            // a driver that leaked it binds this one as source 3.
            let mut devices = qemu::QmpDevices::open(socket);
            devices.add("usb-mouse", "xhci1.0", "hotmouse2", &[]);
            drop(devices);
            thread::sleep(Duration::from_millis(800));
            crate::input_events_end(&mut qemu::QmpInput::open(socket));
        },
    );
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}\n{}", result.serial, result.stdout));
    }
    let log = format!("{boot}{}", result.serial);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} while devices came and went\n{log}"));
        }
    }

    hotplug_bound(&log)?;
    hotplug_delivered(&result.stdout, "hello", (DX * scale_x, DY * scale_y))?;
    hotplug_unbound(&log)?;
    // The keyboard was untouched by the mouse's teardown, and the merge it
    // shares with the pointer that went away still works. `world` is typed
    // after the unplug and nothing else in this boot can produce it.
    let typed: String = crate::parse_key_events(&result.stdout)
        .iter()
        .filter(|e| e.modifiers & 0x10 == 0)
        .map(|e| e.translated.as_str())
        .collect();
    if !typed.contains("world") {
        return Err(format!(
            "typed {typed:?} — the keyboard beside the unplugged pointer stopped delivering\n{}",
            result.stdout
        ));
    }

    // The replug. Source 2 twice is the assertion: an entry that is leaked and
    // one that is handed back read the same from every other angle.
    let sources: Vec<u32> = crate::parse_pointer_sources(&log).iter().map(|(_, s)| *s).collect();
    if sources != vec![BOOT_SOURCE, LATE_SOURCE, LATE_SOURCE] {
        return Err(format!(
            "pointer sources were {sources:?}, want the boot tablet's {BOOT_SOURCE} and then \
             {LATE_SOURCE} twice — the second late pointer took a fresh entry, so the first \
             one's was never given back\n{log}"
        ));
    }

    let _ = std::fs::remove_file(&image);
    eprintln!(
        "  [xhci] after boot: a keyboard, a pointer and a {HOT_DISK_BYTES} B disk enumerated on a \
         controller that had nothing on it; typed keys and a {:?} pointer delta delivered \
         host-side; unplug released source {LATE_SOURCE}, disabled the slots and took the disk \
         offline; the replug took source {LATE_SOURCE} again",
        (DX * scale_x, DY * scale_y)
    );
    Ok(())
}

/// Everything the guest has to say about three devices that were not there
/// when it booted.
fn hotplug_bound(log: &str) -> Result<(), String> {
    // 48 GiB in the 4 KiB blocks the driver counts in, which is a number that
    // exists only because the guest asked the device.
    let geometry = format!("{} blocks of 512 B", 48u64 * 1024 * 1024 * 1024 / 4096);
    for want in [
        "xHCI: USB keyboard ready on slot",
        "xHCI: USB mouse ready on slot",
        "usb-storage: disk 1 ready on slot",
        geometry.as_str(),
    ] {
        if !log.contains(want) {
            return Err(format!("nothing enumerated after the boot: no {want:?}\n{log}"));
        }
    }
    Ok(())
}

/// That the devices which arrived late are the ones delivering.
fn hotplug_delivered(stdout: &str, word: &str, want: (i32, i32)) -> Result<(), String> {
    let typed: String = crate::parse_key_events(stdout)
        .iter()
        .filter(|e| e.modifiers & 0x10 == 0)
        .map(|e| e.translated.as_str())
        .collect();
    if !typed.contains(word) {
        return Err(format!(
            "typed {typed:?}, want it to contain {word:?} — this machine has no keyboard but the \
             one plugged in after it booted\n{stdout}"
        ));
    }
    let pointer = crate::parse_mouse_events(stdout);
    let deltas: Vec<(i32, i32)> = pointer
        .windows(2)
        .map(|w| (w[1].x as i32 - w[0].x as i32, w[1].y as i32 - w[0].y as i32))
        .collect();
    if !deltas.contains(&want) {
        return Err(format!(
            "no pointer event moved by {want:?}; deltas seen: {deltas:?} — the boot-time tablet \
             is absolute, so a relative move can only have come from the mouse that was plugged \
             in\n{stdout}"
        ));
    }
    Ok(())
}

/// A device pulled and pushed back before the driver has looked at the port
/// twice — which is what a person replugging a mouse does.
///
/// **`PORTSC.CCS` is a level and `PORTSC.CSC` is the edge, and only the edge can
/// report a gap.** The driver debounces a disconnect for 100 ms before acting on
/// it, so a device back in the port inside that window reads connected again at
/// the next look, matching what the driver already believed. Comparing CCS
/// against that belief therefore sees nothing at all: the old slot stays bound
/// to a device that is gone, the new one is never enumerated, and the port is
/// dead until something else disturbs it. xHCI 1.2 §5.4.8 sets CSC on a
/// '0'→'1' *or* a '1'→'0' transition, so a connected port with CSC set is the
/// only evidence that the connection was broken in between.
///
/// The T14 showed the other half of the same race: the transfers outstanding
/// when the mouse was pulled completed with a transaction error, and that code
/// is indistinguishable from a bad cable's. The driver spent a failure out of
/// the budget, ran Reset Endpoint and a CLEAR_FEATURE(HALT) control transfer
/// against a device the owner was holding, and then printed advice to unplug it.
/// Four times, once per ordinary unplug.
///
/// The actuator is QEMU's own `device_del`/`device_add` with no wait between
/// them, which lands both edges inside one debounce. No actuator: the
/// window is 100 ms wide and two QMP commands on a unix socket cross it easily.
pub fn xhci_flap(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    /// Enough cycles that a driver leaking one slot per cycle is unmistakable,
    /// and few enough that the guest's input window covers them.
    const CYCLES: usize = 4;
    const DX: i32 = 40;
    const DY: i32 = -30;
    /// **The device has to come back to the port it left.** Without this QEMU
    /// hands each `device_add` the next free root-hub port, so a del/add pair is
    /// a clean disconnect on one port and a clean connect on another — two
    /// ordinary events, and never the state under test. Measured: the first
    /// shape of this gate walked ports 5, 6, 7, 8 and staged nothing.
    const PORT: &str = "1";

    let options = BootOptions {
        profile: Profile::MetalHotplug,
        qmp: true,
        i8042: false,
        ..Default::default()
    };
    let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
    let boot = qemu.boot_log().to_string();
    let Some((scale_x, scale_y)) = crate::parse_rel_scale(&boot) else {
        return Err(format!("the kernel never said what pointer scale it used:\n{boot}"));
    };

    let result = qemu.run_test_hooked(
        "test_rs_input_events",
        Duration::from_secs(60),
        "===INPUT_READY===",
        move |socket| {
            let mut devices = qemu::QmpDevices::open(socket);
            devices.add("usb-mouse", "xhci1.0", "flap0", &[("port", PORT)]);
            drop(devices);
            thread::sleep(Duration::from_millis(600));

            for cycle in 0..CYCLES {
                let mut devices = qemu::QmpDevices::open(socket);
                // No sleep between the two: both edges have to land inside one
                // 100 ms debounce, which is the whole point. A fresh id each
                // cycle because `device_del` releases the old one
                // asynchronously and a reused one races with that.
                devices.del(&format!("flap{cycle}"));
                devices.add(
                    "usb-mouse",
                    "xhci1.0",
                    &format!("flap{}", cycle + 1),
                    &[("port", PORT)],
                );
                drop(devices);
                // Long enough for the driver to finish acting on the cycle
                // before the next one starts, so what the log shows is
                // CYCLES collapsed replugs and not one long blur.
                thread::sleep(Duration::from_millis(600));
            }

            // The pointer that is in the port now has to work. Off the origin
            // first: the accumulated position clamps at 0.
            let mut input = qemu::QmpInput::open(socket);
            input.mouse(100, 100, None);
            thread::sleep(Duration::from_millis(100));
            input.mouse(DX, DY, None);
            thread::sleep(Duration::from_millis(200));
            crate::input_events_end(&mut input);
        },
    );
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}\n{}", result.serial, result.stdout));
    }
    let log = &result.serial;
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} while the port was flapped\n{log}"));
        }
    }

    // The race was actually staged. Without this the gate would pass on a run
    // where every replug happened to be seen as two distinct states, which is
    // the easy case and not the one under test.
    let collapsed = log.matches("was unplugged and plugged back in between two looks").count();
    if collapsed == 0 {
        // The two ways this fires read alike and are not alike, so the counts
        // that tell them apart are in the message. A driver that saw every
        // cycle as a distinct disconnect enumerated once per cycle; one that
        // could not see a collapsed replug at all enumerated **once**, left the
        // slot bound to the device that had gone, and delivered nothing — which
        // is what the pre-fix driver does here, and a good deal worse than the
        // slot march the same defect produces when the replugs are slow enough
        // to be seen.
        return Err(format!(
            "no replug collapsed inside a debounce, so this run never staged the race. The guest \
             bound {} pointer(s) across {CYCLES} cycles and delivered {} pointer event(s): one \
             bind and no events is a dead port, one bind per cycle is a run whose replugs were \
             all seen as distinct.\n{log}",
            crate::parse_pointer_sources(log).len(),
            crate::parse_mouse_events(&result.stdout).len(),
        ));
    }

    // **Bounded slots.** Each cycle's device must give its slot back before the
    // next takes one. A driver that enumerates on top of the old slot marches
    // through fresh ids — 6, 7, 8, 9 on the T14 — and leaves every one enabled.
    let enabled = slot_ids(log, "enabled");
    let disabled = slot_ids(log, "disabled");
    let live: Vec<u8> = {
        let mut left = disabled.clone();
        enabled.iter().copied().filter(|id| !take_one(&mut left, *id)).collect()
    };
    if live.len() != 1 {
        return Err(format!(
            "{} slot(s) enabled and never disabled ({live:?}) after {CYCLES} replugs; exactly \
             the one in the port now should be live\nenabled {enabled:?}\ndisabled \
             {disabled:?}\n{log}",
            live.len()
        ));
    }
    let mut distinct = enabled.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() > 2 {
        return Err(format!(
            "the driver used {} distinct slot ids across {CYCLES} replugs ({distinct:?}) — a slot \
             is not being reaped before the next enumeration takes one\n{log}",
            distinct.len()
        ));
    }

    // **Sources reclaimed.** One pointer is in the port at a time, so every
    // bind must print the same button-table entry. A leak marches 2, 3, 4, 5.
    let sources: Vec<u32> = crate::parse_pointer_sources(log).iter().map(|(_, s)| *s).collect();
    if sources.is_empty() {
        return Err(format!("no pointer bound during the flap at all\n{log}"));
    }
    if sources.iter().any(|s| *s != sources[0]) {
        return Err(format!(
            "pointer sources were {sources:?} — a replugged pointer took a fresh button-table \
             entry, so the one its predecessor held was never given back\n{log}"
        ));
    }

    // **No recovery against a device that is not there.** Every one of these is
    // the driver treating an unplug as a broken cable: a failure out of the
    // budget, a control transfer that spends the deadline failing, and advice
    // to unplug something already in the owner's hand.
    for wrong in [
        "is being let go",
        "could not be restarted",
        "endpoint 3 is Halted, recovering",
    ] {
        if let Some(line) = log.lines().find(|l| l.contains(wrong)) {
            return Err(format!(
                "the driver ran recovery against a device that had been unplugged: {line:?}\n{log}"
            ));
        }
    }
    // And the line that says it declined to, which is the positive half: the
    // errors really did arrive and really were attributed to the disconnect.
    let superseded = log.matches("as its port went away; leaving it to the disconnect").count();

    // The device in the port now delivers. This is what stops every assertion
    // above from passing on a driver that reaped everything and enumerated
    // nothing.
    let pointer = crate::parse_mouse_events(&result.stdout);
    let want = (DX * scale_x, DY * scale_y);
    let deltas: Vec<(i32, i32)> = pointer
        .windows(2)
        .map(|w| (w[1].x as i32 - w[0].x as i32, w[1].y as i32 - w[0].y as i32))
        .collect();
    if !deltas.contains(&want) {
        return Err(format!(
            "no pointer event moved by {want:?} after {CYCLES} replugs; deltas seen: {deltas:?} — \
             the port is bound to a device that is no longer in it\n{}",
            result.stdout
        ));
    }

    eprintln!(
        "  [xhci] {CYCLES} replugs collapsed inside the debounce ({collapsed} seen as such): \
         {} slot(s) enabled and all but one reaped, one button-table entry reused throughout, \
         {superseded} transfer error(s) attributed to the disconnect instead of recovery, and \
         the pointer in the port still delivers",
        enabled.len()
    );
    Ok(())
}

/// Every slot id in an `xHCI: slot 3 enabled …` or `… disabled` line, in order.
fn slot_ids(log: &str, verb: &str) -> Vec<u8> {
    log.lines()
        .filter_map(|line| {
            let rest = line.split("xHCI: slot ").nth(1)?;
            let (id, tail) = rest.split_once(' ')?;
            tail.starts_with(verb).then(|| id.parse().ok())?
        })
        .collect()
}

/// Remove one occurrence of `id`, and say whether there was one.
fn take_one(pool: &mut Vec<u8>, id: u8) -> bool {
    match pool.iter().position(|x| *x == id) {
        Some(at) => {
            pool.remove(at);
            true
        }
        None => false,
    }
}

/// Everything a device that has been pulled has to leave behind.
fn hotplug_unbound(log: &str) -> Result<(), String> {
    for want in [
        "xHCI: port ",
        "disconnected",
        "unplugged from port",
        "source 2 released",
        "xHCI: slot ",
        "disabled",
        "usb-storage: disk 1 unplugged",
        "it is offline",
    ] {
        if !log.contains(want) {
            return Err(format!("an unplugged device left {want:?} unsaid\n{log}"));
        }
    }
    // The teardown ran the commands the controller's state permits. Every one
    // of these lines is `run_command` reporting one it refused.
    for illegal in ["Disable Slot failed", "Disable Slot timed out"] {
        if log.contains(illegal) {
            return Err(format!("{illegal:?} during the teardown\n{log}"));
        }
    }
    Ok(())
}

/// A disk the driver refuses, on the port the controller enumerates *first*.
///
/// `bind` claims a 64 KiB DMA pool block, issues Configure Endpoint — which
/// puts the device's two bulk endpoints into the Running state with their
/// transfer rings inside that block — and only then asks the disk how big it
/// is. A disk refused at that last step never joins `ctrl.storage`, so a block
/// keyed on `ctrl.storage.len()` was handed straight to the next disk, while
/// the first device's slot was still enabled, its endpoint contexts still named
/// that memory, and any transfer `wait_transfer` had abandoned on its 2 s
/// deadline was still outstanding on a Running endpoint. The late completion
/// lands in the next disk's `MSC_SCRATCH` — where READ CAPACITY's block size
/// and last LBA arrive.
///
/// Every other USB profile puts the boot stick on port 1, where it binds and
/// the reuse cannot happen; that is why a full gate boot never reached this.
/// The actuator is not a boot parameter: QEMU can already stage a disk this
/// driver refuses (3 TB, more sectors than READ(10) addresses) and it assigns
/// ports in device-creation order, so attaching it ahead of the boot stick is
/// the whole injection. Nothing about the driver is modified to run this.
///
/// The assertion is the *block offset in the log line*, because that is the
/// only place the reuse is visible from outside: both boots bind one disk,
/// both print `1 device(s)`, and both reach the shell.
///
/// **The second half of the same finding is what happens when that disk is
/// pulled.** Keeping the block is right for as long as the device is on the
/// bus; `teardown_port` gave one back only for entries in the disk list, and a
/// refused disk is not in it. `MSC_BLOCKS` is 2, so one unsupported stick
/// plugged and pulled beside the boot stick left the pool with nothing for any
/// later disk, for the life of the boot. The actuator for that half is QEMU's
/// own `device_del`, and the verdict is that the disk plugged in afterwards
/// binds — at the block the refused one had.
pub fn usb_refused_disk_first(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    /// The disk that arrives after the refused one is pulled: 48 GiB, sparse,
    /// and a size no other device in this suite reports.
    const REPLACEMENT_BYTES: u64 = 48 * 1024 * 1024 * 1024;

    let (huge, _) = Profile::UsbDiskRefusedFirst.usb_disk().expect("the profile declares a disk");

    // The claim is about which device QEMU creates first, and argv is the only
    // place it is visible — a console line cannot distinguish "the refused disk
    // was enumerated first" from "the driver happened to bind them in that
    // order".
    let options = BootOptions {
        profile: Profile::UsbDiskRefusedFirst,
        kernel_params: GATE,
        qmp: true,
        ..Default::default()
    };
    let argv = qemu::profile_argv(&options);
    let sticks: Vec<&String> = argv
        .iter()
        .filter(|a| a.starts_with("usb-storage,"))
        .collect();
    match sticks.as_slice() {
        [first, second] if first.contains("drive=usbdisk") && second.contains("drive=stick") => {}
        other => {
            return Err(format!(
                "want the data disk created before the boot stick, got {other:?}"
            ));
        }
    }

    let replacement = test_dir().join("usb-refused-replacement.img");
    drop(sparse(&replacement, REPLACEMENT_BYTES));

    let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
    let boot = qemu.boot_log().to_string();

    // Pull the disk the driver refused, and put a disk it can use where it was.
    // Nothing else on this machine can free that pool block, so the bind below
    // is the whole assertion.
    let mut devices = qemu::QmpDevices::open(qemu.qmp_socket());
    devices.del(&qemu::usb_device_id(0));
    drop(devices);
    thread::sleep(Duration::from_millis(1200));
    let mut devices = qemu::QmpDevices::open(qemu.qmp_socket());
    devices.blockdev_add("replacement", &replacement);
    devices.add("usb-storage", "xhci.0", "replacement0", &[("drive", "replacement")]);
    drop(devices);
    thread::sleep(Duration::from_millis(1200));

    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let log = format!("{boot}{}", qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} during the USB gate boot\n{log}"));
        }
    }

    // The refusal happened, and it happened on slot 1 — the first device the
    // controller enumerated. Without this the test would pass on a boot where
    // the ordering silently went back to stick-first.
    let sectors = huge / 512;
    let refusal = format!(
        "usb-storage: slot 1 has {sectors} sectors; this driver issues READ(10)"
    );
    if !log.contains(&refusal) {
        return Err(format!(
            "the first disk enumerated was not the one the driver refuses ({refusal:?})\n{log}"
        ));
    }

    // And the boot stick behind it got the *second* pool block. `MSC_STRIDE` is
    // 0x10000 and `msc_base` is where block 0 starts, so `+0x10000` is the
    // block the refused disk's endpoint contexts still name and `+0x20000` is
    // the next one. This is the whole finding: before the fix the line below
    // reads `+0x10000`.
    if !boot.contains("msc_block +0x20000") {
        let got = boot
            .lines()
            .find(|l| l.contains("msc_block +"))
            .unwrap_or("<no disk bound at all>");
        return Err(format!(
            "the disk after the refused one was given the refused one's pool block: {got:?}"
        ));
    }
    if boot.matches("msc_block +").count() != 1 {
        return Err(format!("want exactly one disk bound at boot\n{log}"));
    }

    // Refused, not fatal: the stick still binds, still carries /boot, and the
    // machine still comes up. A fix that leaked the whole pool would fail here.
    if !boot.contains("usb-storage: 1 device(s)") {
        return Err(format!("the boot stick did not bind behind the refused disk\n{log}"));
    }
    gate_ran(&boot, 1)?;

    // **Then the block came back.** The refused disk was unplugged, so its slot
    // is disabled and the memory its endpoint contexts named is nobody's — and
    // the disk plugged in afterwards binds, at that block. Before the fix this
    // pool is out for the life of the boot: the line is the refusal below
    // instead, and `MSC_BLOCKS` is 2, so it takes exactly one unsupported stick
    // to cost a machine every disk it is given from then on.
    if !log.contains("usb-storage: disk 1 ready on slot") {
        return Err(format!(
            "the disk plugged in after the refused one was pulled never bound; the pool block a \
             refused device holds is not given back when it leaves\n{log}"
        ));
    }
    if !log.contains("msc_block +0x10000") {
        let blocks: Vec<&str> = log.lines().filter(|l| l.contains("msc_block +")).collect();
        return Err(format!(
            "the replacement disk did not take the refused disk's block: {blocks:?}\n{log}"
        ));
    }
    if log.contains("this driver serves 2") {
        return Err(format!(
            "the pool refused a disk on a machine with two blocks and one disk on it\n{log}"
        ));
    }
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish\n{log}"));
    }
    serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
    let _ = std::fs::remove_file(&replacement);

    eprintln!(
        "  [usb] a {huge} B disk refused on slot 1, enumerated first: the boot stick behind it \
         binds at msc_block +0x20000, not the refused disk's block — and once the refused disk \
         is unplugged a {REPLACEMENT_BYTES} B one binds at +0x10000, which is that block back"
    );
    Ok(())
}

/// The stick the machine booted from, pulled while the desktop is up.
///
/// **The instrument for #152, and the reason it exists is that the failure has
/// no other witness.** `/log` is on the stick, so the recording of the event
/// dies with the event; the machine has no serial port; it is not a panic, so
/// the on-screen console never paints; and Ctrl+Alt+D answers nothing. Three
/// investigations ran on the owner's description alone.
///
/// The pull is `device_del` on the boot stick, which had no device id until
/// this gate needed one — every earlier unplug test names a *data* disk, and a
/// data disk carries neither `/boot` nor `/log` nor the mount the log sink
/// writes through. That difference is the whole scenario.
///
/// The liveness signal is `compositor: frames=`, for the reason
/// `metal_sim_pointer_churn` picked it: it comes from a composited frame, so
/// its absence is a desktop that stopped drawing rather than an instrument that
/// stopped counting — which is exactly what the owner reports, a clock that
/// stops advancing. The second probe is the serial console, which reaches
/// userland through a different path: `run` makes test-runner print a line and
/// then walk the VFS looking for a binary.
///
/// **A green run does not certify the machine survives an unplug.** It
/// certifies that this shape of unplug, on this emulated controller, leaves the
/// guest drawing and answering. What it *is* good for is red: a red here is the
/// first reproduction of the owner's freeze anywhere but his desk.
pub fn usb_boot_stick_pulled(
    test_config: &Path,
    _c_bins: &[(String, Vec<u8>)],
    _rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let _ = test_config;
    /// Probes sent before the pull, and after it. The drumbeat is the liveness
    /// signal as well as the load: each one is a userland `println!` into the
    /// ring the log sink drains to the stick, and a VFS walk for a binary that
    /// is not there.
    const BEFORE: usize = 12;
    const AFTER: usize = 40;
    /// How many of the probes after the pull have to come back. Not all of
    /// them: a machine that pauses while the driver tears the port down and
    /// then carries on has survived, and this gate's subject is a machine that
    /// never carries on.
    const ANSWERED: usize = 8;

    // metalcase's machine shape with `/bin/logd` rotating at 256 bytes rather
    // than a mebibyte, so the log writer is not just appending when the device
    // goes: every few probes it creates a file, sweeps the volume, deletes the
    // oldest and syncs the mount. That is FAT allocation and directory writes
    // in flight at the moment of the pull, which is the state the owner's
    // machine is in and the one a quiet idle desktop never reaches. It was a
    // kernel parameter until L6 and is a manifest row now, because the writer
    // is a userland program.
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/logrotatecase");
    let options = BootOptions {
        profile: Profile::Metal,
        qmp: true,
        // The T14's core count. How many CPUs are in the idle loop when the
        // device goes is the whole question on one hypothesis.
        smp: 8,
        ..Default::default()
    };
    let argv = qemu::profile_argv(&options);
    if !argv.iter().any(|a| a.contains(&format!("id={}", qemu::BOOT_STICK_ID))) {
        return Err(format!("the boot stick has no device id, so it cannot be pulled: {argv:?}"));
    }

    let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
    let socket = qemu.qmp_socket().to_path_buf();
    let mut console = qemu.boot_log().to_string();
    let frames = |text: &str| text.matches("compositor: frames=").count();

    // The machine has to be drawing before it is asked to survive anything, or
    // a green run is a boot that never started.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && frames(&console) < 1 {
        console.push_str(&qemu.drain_serial(Duration::from_millis(250)));
    }
    if frames(&console) < 1 {
        return Err(format!("the compositor never composited a frame:\n{console}"));
    }

    // And it has to be writing to the stick, or the pull is a disconnect with
    // nothing in flight — which is not the state the owner's machine is in.
    // `/bin/logd` names the file it opened on `/log`, and that volume is on the
    // device this test is about to take away.
    if !console.contains("logd: this boot's kernel log is /log/") {
        return Err(format!(
            "logd opened no file on /log, so the stick is not being written to and this gate \
             stages nothing:\n{console}"
        ));
    }

    let probe = |qemu: &mut QemuInstance, console: &mut String, i: usize| {
        let _ = writeln!(qemu.stdin_mut(), "run pull-probe-{i}");
        qemu.flush_stdin();
        console.push_str(&qemu.drain_serial(Duration::from_millis(120)));
    };

    for i in 0..BEFORE {
        probe(&mut qemu, &mut console, i);
    }
    let answered_before = console.matches("===TEST_END pull-probe-").count();
    if answered_before < BEFORE / 2 {
        return Err(format!(
            "only {answered_before} of {BEFORE} probes came back *before* the pull, so the \
             drumbeat this gate measures does not work on a healthy machine:\n{console}"
        ));
    }
    // The rotation actually ran, so "the writer was busy" is a fact rather than
    // a manifest row that might have been dropped.
    if !console.contains("logd: /log/") || !console.contains("and this boot continues in") {
        return Err(format!(
            "logd never rotated, so the pull below lands on a writer that is only \
             appending:\n{console}"
        ));
    }

    let pulled_at = console.len();
    let mut devices = qemu::QmpDevices::open(&socket);
    devices.del(qemu::BOOT_STICK_ID);
    drop(devices);

    for i in BEFORE..BEFORE + AFTER {
        probe(&mut qemu, &mut console, i);
    }
    let after = &console[pulled_at.min(console.len())..];
    let answered = after.matches("===TEST_END pull-probe-").count();
    let drawn = frames(after);
    if answered < ANSWERED || drawn < 2 {
        return Err(format!(
            "the boot stick was pulled and the machine answered {answered} of {AFTER} console \
             probes and composited {drawn} frame batches after it (want {ANSWERED} and 2) — it \
             stopped:\n{console}\n{}",
            crate::freeze_report(&mut qemu, &mut console)
        ));
    }

    // And the same stick put back. The owner reports the freeze from a replug
    // as well as from a pull, and the two are different states: a replug binds
    // a new disk under mounts that still name the old one.
    let replug = test_dir().join("usb-replug.img");
    drop(sparse(&replug, 512 * 1024 * 1024));
    let replugged_at = console.len();
    let mut devices = qemu::QmpDevices::open(&socket);
    devices.blockdev_add("replug", &replug);
    devices.add("usb-storage", "xhci.0", "replug0", &[("drive", "replug")]);
    drop(devices);

    for i in BEFORE + AFTER..BEFORE + 2 * AFTER {
        probe(&mut qemu, &mut console, i);
    }
    let after_replug = &console[replugged_at.min(console.len())..];
    let answered_replug = after_replug.matches("===TEST_END pull-probe-").count();
    let drawn_replug = frames(after_replug);
    if answered_replug < ANSWERED || drawn_replug < 2 {
        return Err(format!(
            "a stick was plugged back into the port the boot stick was pulled from and the \
             machine answered {answered_replug} of {AFTER} console probes and composited \
             {drawn_replug} frame batches after it (want {ANSWERED} and 2) — it stopped:\
             \n{console}\n{}",
            crate::freeze_report(&mut qemu, &mut console)
        ));
    }

    for bad in ["PANIC:", "panicked at"] {
        if console.contains(bad) {
            return Err(format!("{bad:?} after the boot stick was pulled\n{console}"));
        }
    }
    let _ = std::fs::remove_file(&replug);

    eprintln!(
        "  [usb] the boot stick was pulled out from under a running desktop with the log sink \
         rotating: {answered}/{AFTER} console probes answered and {drawn} frame batches after \
         the pull, {answered_replug}/{AFTER} and {drawn_replug} after a stick went back in"
    );
    Ok(())
}
