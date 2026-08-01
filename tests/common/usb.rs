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

/// The two answers a device can give to an *optional* SCSI command, and the
/// loop that reading them as one answer produced.
///
/// SYNCHRONIZE CACHE (0x35) is optional in SBC and a great many USB flash
/// drives answer ILLEGAL REQUEST / INVALID COMMAND OPERATION CODE. `msc_flush`
/// read that as a failed flush; `EspFs::sync` logged the failure and returned
/// `()`; the line it logged was new pending content in the ring `esp_log` was
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
/// Neither boot can be green because the feature was not on: each asserts a
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
    const FEATURE: &[&str] = &["usb-flush-unimplemented"];
    const REPORTED: &str = "usb-storage: disk 0 does not implement SYNCHRONIZE CACHE";

    let image_path = test_dir().join("usb-flush-optional.img");
    let image = qemu::build_boot_image(test_config, c_bins, rust_bins, FEATURE);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = super::esp::esp_extent(&image, &image_path)?;

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::Metal,
            boot_image: Some(image_path.clone()),
            kernel_features: FEATURE,
            ..Default::default()
        },
    );
    let boot = qemu.boot_log().to_string();

    // Mid-run and polled, exactly as `esp_log_file` does it: the claim is that
    // the sink is still running, and the only place that is visible is the
    // device while the machine is up.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut on_device;
    loop {
        on_device = String::from_utf8_lossy(
            &super::esp::log_on_device(&image_path, start, len, "toyos/kernel.log")?,
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
    for bad in ["!!! PANIC !!!", "panicked at"] {
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
    if log.contains("stops at") {
        return Err(format!("the sink gave up on a stick that is working\n{log}"));
    }
    if !on_device.contains("Boot: complete") {
        return Err(format!(
            "the log on the device stops before `Boot: complete` at {} bytes — a stick with no \
             write cache cost the machine its log",
            on_device.len()
        ));
    }

    let after = super::esp::log_on_device(&image_path, start, len, "toyos/kernel.log")?;
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

/// Boot with a stick whose flush genuinely fails. The sink says so once and
/// stops, rather than writing the device that just refused it.
fn failed_flush_stops_once(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const FEATURE: &[&str] = &["usb-flush-fails"];
    /// A per-failure line, and the thing that has to stay bounded. Before the
    /// fix it is emitted by every pass of the idle loop for the life of the
    /// boot; after it, once by the flush that gives up and once by the
    /// shutdown's `sync_all`, which is the last caller left.
    const BOUND: usize = 4;

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::Metal,
            kernel_features: FEATURE,
            ..Default::default()
        },
    );
    let boot = qemu.boot_log().to_string();
    // Long enough for a loop to be a loop: the flush runs from the idle loop,
    // which on this machine goes round thousands of times a second.
    std::thread::sleep(Duration::from_secs(2));
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let log = format!("{boot}{}", qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["!!! PANIC !!!", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} on a stick that cannot flush\n{log}"));
        }
    }

    if !log.contains("usb-storage: SCSI 0x35 failed, sense 0x04/0x44/0x00") {
        return Err(format!("the injected flush failure never reached the driver\n{log}"));
    }
    let gave_up = log
        .matches("esp-log: the boot volume's device refused the sync — /boot/toyos/kernel.log \
                  stops at")
        .count();
    if gave_up != 1 {
        return Err(format!(
            "the sink gave up {gave_up} times, wanted exactly one — a failed sync has to reach \
             `Sink::flush` as an error\n{log}"
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
        "  [usb] a flush the device refuses: {failures} failing flushes over a 2 s idle run, sink \
         disabled once"
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
            kernel_features: &["xhci-deaf-controller"],
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
            kernel_features: &["xhci-deaf-port"],
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
