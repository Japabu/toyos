//! The boot stick's two partitions, mounted and written from inside ToyOS.
//!
//! The ESP holds what firmware and the bootloader read. The log partition
//! beside it holds the kernel's own log and exists for one reason: it is typed
//! so that a desktop OS mounts it on plug-in, which an EFI-typed partition is
//! not. Both are FAT32 and neither is found by being FAT32 — the kernel is
//! handed both by unique GUID, and `log_partition_identity` is the gate that
//! says so by moving the name and watching the mount disappear.
//!
//! Ground truth is the disk image the *device* received, read on the host by
//! implementations that are not the kernel's: the `fatfs` crate and
//! `toyos-fat32-check`. The guest's account of a write it made is exactly what
//! is in question, so it cannot also be the evidence; `esp_files` asserts what
//! only a process inside the machine can see, and everything it claims about
//! bytes is checked again here.
//!
//! **Where this stops.** `log_partition_layout` pins the image: type GUID,
//! attribute bits, labels, alignment, and that our own GPT parser finds the
//! partition the ESP names. It does not assert that any operating system
//! *mounts* it. Whether macOS attaches a Basic Data partition is
//! `diskarbitrationd`'s policy, not our contract — it moves between macOS
//! versions and host settings, it would put a volume on the owner's desktop
//! every test run, and it would race concurrent runs. That end of the contract
//! was verified once by hand, on 2026-08-02, and is re-verified when a stick is
//! flashed.
//!
//! The image is built and modified before the boot rather than after, because
//! the host-writes-guest-reads direction has no other staging point: a file the
//! guest itself created and read back would pass with the read path broken.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use toyos_fat32_check::{check, describe};

use fatfs::FsOptions;
use gpt::disk::LogicalBlockSize;
use gpt::partition_types;

use super::qemu::{self, BootOptions, QemuInstance};
use super::serial;

/// Mirrored in `tests/toyos-rust-tests/src/bin/esp_files.rs`. Two halves of one
/// fixture; a change to either alone fails loudly rather than passing quietly.
const HOST_NOTE: &str = "host-note.txt";
const HOST_TEXT: &str = "written by the host before this machine started\n";
/// On the *log* volume, not the ESP: `/boot` is read-only toward userland, so
/// the guest's own writes go where it is allowed to put them.
const GUEST_NOTE: &str = "guest-note.txt";
const GUEST_TEXT: &str = "written by ToyOS through the VFS\n";
const GUEST_BLOB: &str = "guest-blob.bin";
const BLOB_LEN: usize = 10 * 4096 + 137;

fn blob() -> Vec<u8> {
    (0..BLOB_LEN).map(|i| (i.wrapping_mul(97) ^ 0x5A) as u8).collect()
}

/// The files the build put on the ESP, which the guest must not have touched.
/// `BOOTx64.EFI` is the one firmware reads; the other two are what the
/// bootloader reads. Damaging any of them makes the stick unbootable, so
/// "still byte-identical" is the assertion that matters most here.
const UNTOUCHED: [&str; 3] = ["EFI/BOOT/BOOTx64.EFI", "toyos/kernel.elf", "toyos/initrd.img"];

fn test_dir() -> PathBuf {
    super::lane::dir()
}

/// Where a partition sits inside a GPT disk image, in bytes, and the unique
/// GUID the table gives it.
///
/// Selected by *type*, which is right in exactly one place and this is it: the
/// host has no handoff to be given, and it is the thing asking whether the
/// image builder produced the layout it claims. Exactly one partition of each
/// type, or this fails.
///
/// The GUID is drawn fresh by `create_boot_image` for every image, so it is a
/// per-run nonce that the host knows before the machine starts and that only
/// this boot's kernel can have logged. `kernel_log_file` uses it to tell this
/// boot's log from a file left behind by anything else.
struct Extent {
    start: usize,
    len: usize,
    guid: String,
}

fn extent(
    image: &[u8],
    path: &Path,
    kind: partition_types::Type,
    what: &str,
) -> Result<Extent, String> {
    let disk = gpt::GptConfig::new()
        .writable(false)
        .logical_block_size(LogicalBlockSize::Lb512)
        .open(path)
        .map_err(|e| format!("the built image has no readable GPT: {e}"))?;
    let found: Vec<_> =
        disk.partitions().values().filter(|p| p.part_type_guid == kind).collect();
    let [part] = found.as_slice() else {
        return Err(format!("the built image has {} of {what}, expected one", found.len()));
    };
    let start = part.first_lba as usize * 512;
    let len = (part.last_lba - part.first_lba + 1) as usize * 512;
    if start + len > image.len() {
        return Err(format!(
            "the {what} runs to {} in an image of {}",
            start + len,
            image.len()
        ));
    }
    Ok(Extent { start, len, guid: part.part_guid.to_string().to_uppercase() })
}

/// The ESP's byte range: what firmware and the bootloader read.
pub fn esp_extent(image: &[u8], path: &Path) -> Result<(usize, usize), String> {
    let e = extent(image, path, partition_types::EFI, "ESP")?;
    Ok((e.start, e.len))
}

/// The log partition's byte range: where `/log/kernel.log` lands.
pub fn log_extent(image: &[u8], path: &Path) -> Result<(usize, usize), String> {
    let e = extent(image, path, partition_types::BASIC, "log partition")?;
    Ok((e.start, e.len))
}

/// The unique partition GUID of a boot image's ESP, as the kernel prints it.
fn esp_guid(image: &[u8], path: &Path) -> Result<String, String> {
    Ok(extent(image, path, partition_types::EFI, "ESP")?.guid)
}

/// Read several files out of a FAT volume in one mount. `None` is a file that
/// is not there, which is an assertion in its own right here.
///
/// `fatfs` wants a writable, seekable device even to read, so the volume is
/// copied — once per call, which is why the callers ask for everything they
/// need at once rather than a file at a time.
pub fn read_files(volume: &[u8], paths: &[&str]) -> Result<Vec<Option<Vec<u8>>>, String> {
    let fs = fatfs::FileSystem::new(Cursor::new(volume.to_vec()), FsOptions::new())
        .map_err(|e| format!("the volume does not mount on the host: {e}"))?;
    let root = fs.root_dir();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        match root.open_file(path) {
            Ok(mut file) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map_err(|e| format!("reading {path}: {e}"))?;
                out.push(Some(bytes));
            }
            Err(_) => out.push(None),
        }
    }
    Ok(out)
}

/// One file that must be there.
fn need(got: Option<Vec<u8>>, path: &str) -> Result<Vec<u8>, String> {
    got.ok_or_else(|| format!("{path} is not on the volume"))
}

/// One directory entry, as the host's own FAT implementation reads it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub len: u64,
    /// The entry's modification time in seconds from the Unix epoch, read out
    /// of the directory entry rather than from anything the guest said about
    /// it. FAT stores local time by specification, so this is in whatever zone
    /// the machine that wrote it keeps.
    pub modified: i64,
}

/// Every file in the root of a FAT volume, sorted by name.
///
/// The ground truth for what a guest put on a volume and what it took off one:
/// the guest's own account of its directory is exactly what is in question when
/// the claim is about retention.
pub fn root_entries(volume: &[u8]) -> Result<Vec<Entry>, String> {
    let fs = fatfs::FileSystem::new(Cursor::new(volume.to_vec()), FsOptions::new())
        .map_err(|e| format!("the volume does not mount on the host: {e}"))?;
    let mut entries = Vec::new();
    for entry in fs.root_dir().iter() {
        let entry = entry.map_err(|e| format!("reading the root directory: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let t = entry.modified();
        entries.push(Entry {
            name: entry.file_name(),
            len: entry.len(),
            modified: unix_secs(
                t.date.year as i64,
                t.date.month as i64,
                t.date.day as i64,
                t.time.hour as i64,
                t.time.min as i64,
                t.time.sec as i64,
            ),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// Write files into the root of a FAT volume in place, before the machine that
/// will read them exists.
///
/// The host-writes-guest-reads direction, which has no other staging point: a
/// file the guest created itself would prove nothing about a guest that deletes
/// the wrong one.
pub fn stage_files(volume: &mut [u8], files: &[(String, Vec<u8>)]) -> Result<(), String> {
    let fs = fatfs::FileSystem::new(Cursor::new(volume), FsOptions::new())
        .map_err(|e| format!("the volume does not mount on the host: {e}"))?;
    let root = fs.root_dir();
    for (name, bytes) in files {
        let mut file =
            root.create_file(name).map_err(|e| format!("creating {name} on the volume: {e}"))?;
        file.write_all(bytes).map_err(|e| format!("writing {name}: {e}"))?;
    }
    Ok(())
}

/// Seconds from the Unix epoch, for comparing a directory entry against the
/// instant the host set the guest's clock to. Hinnant's algorithm.
fn unix_secs(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hour * 3_600 + min * 60 + sec
}

pub fn esp_filesystem(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let image_path = test_dir().join("esp-boot.img");
    let mut image = qemu::build_boot_image(test_config, c_bins, rust_bins, &[]);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = esp_extent(&image, &image_path)?;

    // The host's half of the fixture, put there before the machine exists.
    {
        let volume = &mut image[start..start + len];
        let fs = fatfs::FileSystem::new(Cursor::new(&mut *volume), FsOptions::new())
            .map_err(|e| format!("the built ESP does not mount on the host: {e}"))?;
        let dir = fs
            .root_dir()
            .open_dir("toyos")
            .map_err(|e| format!("the built ESP has no toyos directory: {e}"))?;
        let mut file = dir
            .create_file(HOST_NOTE)
            .map_err(|e| format!("creating {HOST_NOTE} on the ESP: {e}"))?;
        file.write_all(HOST_TEXT.as_bytes())
            .map_err(|e| format!("writing {HOST_NOTE}: {e}"))?;
    }
    std::fs::write(&image_path, &image).map_err(|e| format!("rewrite the boot image: {e}"))?;

    // What the build wrote, before the guest ever sees the volume. Read
    // through `fatfs` rather than from the artifact files, so a byte the image
    // builder mangled is not counted against the kernel.
    let before = read_files(&image[start..start + len], &UNTOUCHED)?;
    for (name, bytes) in UNTOUCHED.iter().zip(&before) {
        if bytes.is_none() {
            return Err(format!("the built image has no {name}"));
        }
    }
    // Including the file this test just wrote through `fatfs` above: the
    // fixture is part of what has to leave the volume clean, or the gate below
    // could only ever be as strong as the untidiest thing on the stick.
    let complaints_before = check(&image[start..start + len]);
    if !complaints_before.is_empty() {
        return Err(format!(
            "the boot volume breaks the format before the guest has run:\n{}",
            describe(&complaints_before)
        ));
    }

    // metal-sim, because that is the machine shape that gets flashed and the
    // one whose whole reason for having a log on the stick is that it has no
    // serial port.
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            boot_image: Some(image_path.clone()),
            ..Default::default()
        },
    );
    let boot = qemu.boot_log().to_string();
    serial::Serial::named("boot console", boot.as_str()).must_be_clean()?;
    if !boot.contains("boot-volume: partition mounted") {
        return Err(format!(
            "the kernel did not mount the boot partition:\n{}",
            volume_lines(&boot)
        ));
    }

    let result = qemu.run_test("test_rs_esp_files", Duration::from_secs(60));
    if let Some(err) = &result.error {
        return Err(format!("the guest stopped answering: {err}\nserial:\n{}", result.serial));
    }
    if result.exit_code != Some(0) {
        // **The kernel's own lines, not just the guest binary's.** Every failure
        // this test can produce on the write path — `write_page`, `set_len`,
        // `flush_meta`, the FSInfo write, the device cache flush — reaches
        // userland as one `SyscallError::Io`, which `std` flattens to
        // `Kind(Other)`; *which* layer refused is in a `log!` line and nowhere
        // else (`fat32_adapter::refused`, `usb_storage`'s three trait methods,
        // `xhci::wait::msc`'s `log_refusal` and its budget line). Reporting
        // `stdout` alone left `fsync the blob: Kind(Other)` as the whole of the
        // evidence for a real 2026-08-21 sighting, and the next author with no
        // more to go on than the one before.
        return Err(format!(
            "esp_files failed:\n{}\nkernel log while the test ran:\n{}{}",
            result.stdout, result.before, result.serial
        ));
    }
    serial::Serial::named("test serial", result.serial.as_str()).must_be_clean()?;
    for line in result.stdout.lines().filter(|l| l.contains("PASS")) {
        eprintln!("  [esp]{}", line.trim_start_matches("  PASS"));
    }

    // The shutdown is not politeness: it is what makes the host's view of the
    // backing file the device's view of it.
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let tail = qemu.drain_serial(Duration::from_secs(20));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if tail.contains(bad) {
            return Err(format!("{bad:?} on the way down\n{tail}"));
        }
    }

    let after = std::fs::read(&image_path).map_err(|e| format!("read the image back: {e}"))?;
    if after.len() != image.len() {
        return Err(format!("the image is {} bytes, was {}", after.len(), image.len()));
    }
    let volume = &after[start..start + len];

    // The strongest claim first: the volume is still a volume. A driver that
    // wrote the right file into a broken FAT would pass every byte comparison
    // below and leave a stick that cannot boot.
    // Silence, not sameness. While the image builder left complaints of its own
    // on every volume it wrote, all this could ask was that the guest add none
    // — which would have hidden a complaint the guest produced for its own
    // reason inside the ones it did not.
    let complaints_after = check(volume);
    if !complaints_after.is_empty() {
        return Err(format!(
            "the guest left the boot volume breaking the format:\n{}",
            describe(&complaints_after)
        ));
    }
    eprintln!("  [esp] the volume checker is silent on the boot volume before and after the boot");

    // Everything the host has to say about the boot volume, in one mount: what
    // the guest must not have left behind, and what it must not have touched.
    // Nothing on this volume is the guest's to write — that is what the mount
    // policy says and what `esp_files` attacked from inside.
    let wanted: Vec<String> = [
        format!("toyos/{HOST_NOTE}"),
        "toyos/new-file.txt".to_string(),
        "toyos/moved.txt".to_string(),
        "toyos/link".to_string(),
    ]
    .into_iter()
    .chain(UNTOUCHED.iter().map(|s| s.to_string()))
    .collect();
    let refs: Vec<&str> = wanted.iter().map(String::as_str).collect();
    let mut found = read_files(volume, &refs)?.into_iter();

    // The host's note, unchanged. A refusal that deleted the file first would
    // still satisfy the absences below.
    let got = need(found.next().flatten(), HOST_NOTE)?;
    if got != HOST_TEXT.as_bytes() {
        return Err("the guest changed the host's note".to_string());
    }
    for absent in ["toyos/new-file.txt", "toyos/moved.txt", "toyos/link"] {
        if found.next().flatten().is_some() {
            return Err(format!("{absent} reached the boot volume; a refusal wrote to it"));
        }
    }

    // The assertion this test exists for. A guest test once wrote five bytes
    // over `kernel.elf` through the VFS; `esp_files` tries exactly that, and
    // this is where the answer comes from — the image the device received,
    // not the guest's opinion of what it did.
    for (name, want) in UNTOUCHED.iter().zip(&before) {
        let got = need(found.next().flatten(), name)?;
        if Some(&got) != want.as_ref() {
            return Err(format!(
                "{name} is {} bytes on the volume and was {} — the boot stick has been damaged",
                got.len(),
                want.as_ref().map_or(0, Vec::len)
            ));
        }
    }
    eprintln!("  [esp] {} build artifacts byte-identical after the guest's attempts", UNTOUCHED.len());

    // The write direction, on the volume the guest is allowed to have. Same
    // adapter and same driver, so the refusals above cost the FAT32 write path
    // no coverage.
    let (log_start, log_len) = log_extent(&after, &image_path)?;
    let log = &after[log_start..log_start + log_len];
    let mut found = read_files(log, &[GUEST_NOTE, GUEST_BLOB, "doomed.txt", "link"])?.into_iter();

    let got = need(found.next().flatten(), GUEST_NOTE)?;
    if got != GUEST_TEXT.as_bytes() {
        return Err(format!(
            "{GUEST_NOTE} on the log volume is {:?}, not what the guest wrote",
            String::from_utf8_lossy(&got)
        ));
    }
    let got = need(found.next().flatten(), GUEST_BLOB)?;
    if got.len() != BLOB_LEN {
        return Err(format!("{GUEST_BLOB} is {} bytes on the volume, wrote {BLOB_LEN}", got.len()));
    }
    if let Some(at) = got.iter().zip(blob()).position(|(a, b)| *a != b) {
        return Err(format!("{GUEST_BLOB} differs from what the guest wrote at byte {at}"));
    }

    // A deleted file, and the symlink FAT32 cannot hold. Both are the half a
    // read-back-what-you-wrote test cannot see.
    for absent in ["doomed.txt", "link"] {
        if found.next().flatten().is_some() {
            return Err(format!("{absent} is still on the log volume"));
        }
    }

    let _ = std::fs::remove_file(&image_path);
    eprintln!("  [esp] {BLOB_LEN} bytes and two files verified host-side on the log volume");
    Ok(())
}

/// Everything the guest said about identifying and mounting its volumes.
///
/// Wider than the mount's own lines on purpose. A mount that does not happen is
/// usually not the mount's fault: the two recorded instances were `gpt::probe`
/// reporting an entry-array CRC mismatch, which is a *read* off the stick
/// coming back wrong, and a failure message showing only the mount line said
/// nothing about that.
fn volume_lines(log: &str) -> String {
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("-volume:") || l.contains("logd:") || l.contains("gpt:")
            || l.contains("shutdown") || l.contains("Shutting down") || l.contains("Syncing")
            || l.contains("usb-storage:"))
        .collect();
    if lines.is_empty() {
        return format!("the guest said nothing about its volumes at all\n{log}");
    }
    format!("what it said:\n{}", lines.join("\n"))
}

/// The kernel's own log, written to the log partition of the stick it booted
/// from — **by `/bin/logd` since L6, and this gate is what says the hand-over
/// kept its promise**.
///
/// The claim under test is *continuity*: not that a log file exists at the end,
/// but that the tail of what the kernel said is on the device while the machine
/// is still running — because the failure it is for is a machine that stops
/// without panicking, on a laptop with no serial port, where nothing else is
/// left. So the file is read **mid-run**, before any shutdown.
///
/// **What it is evidence for changed with the writer.** It used to prove the
/// idle loop's sink; it now proves a userland process holding `logread` reads a
/// cursor, renders, writes, `fsync`s and keeps up — the whole of the log's
/// userland writer, observed from outside the machine.
/// The positive log-content assertion is this, and without it the headline
/// idle-loop I/O number is unfalsifiable: the cheapest way to make an
/// idle-loop I/O measurement look good is for the log to stop being written.
///
/// Three things could make this green without logd working, and each has an
/// assertion aimed at it:
///
/// - **A file left over from something else.** The log must carry this image's
///   own unique ESP GUID, which `create_boot_image` draws fresh per build and
///   no earlier run can have.
/// - **A single write when the file was opened.** logd creates its file early,
///   so a logd that then did nothing would still produce one. `Boot: complete`
///   is logged after that, so requiring it requires a write after the open.
/// - **The shutdown path standing in for the continuous one.** The mid-run read
///   happens before `run shutdown` and must already have `Boot: complete`; the
///   post-shutdown read must additionally have the shutdown's own last line,
///   which only §6.3's bounded wait on `LOG_DURABLE_NS` can deliver.
///
/// A second boot, from `tests/logrotatecase`, drives the bound: rotation is
/// what stops the file filling the owner's stick, and at the shipped mebibyte
/// no test would ever reach it.
pub fn kernel_log_file(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let image_path = test_dir().join("kernel-log-boot.img");
    let image = qemu::build_boot_image(test_config, c_bins, rust_bins, &[]);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = log_extent(&image, &image_path)?;
    let guid = esp_guid(&image, &image_path)?;
    // Born clean, and asserted so rather than assumed: `create_log_volume`
    // formats an empty volume and records its free-cluster count, so unlike the
    // ESP there is nothing here for the guest's own complaints to hide behind.
    let complaints_before = check(&image[start..start + len]);
    if !complaints_before.is_empty() {
        return Err(format!(
            "the log partition was not born clean, so this gate cannot tell a complaint the \
             guest caused from one it inherited:\n{}",
            describe(&complaints_before)
        ));
    }

    // The line the kernel logs when firmware hands it the partition GUID. The
    // host knows it before the machine starts; the guest can only have it from
    // this boot.
    let nonce = format!("gpt: firmware booted us from partition {guid} ");

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            boot_image: Some(image_path.clone()),
            ..Default::default()
        },
    );
    let boot = qemu.boot_log().to_string();
    serial::Serial::named("boot console", boot.as_str()).must_be_clean()?;
    if !boot.contains("logd: this boot's kernel log is") {
        return Err(format!("logd never opened a file:\n{}", volume_lines(&boot)));
    }

    // Mid-run, with the guest still up and nothing shut down. Whatever is here
    // was put there by `/bin/logd` while the machine was running.
    //
    // Polled rather than read once, because the claim is "promptly", not
    // "instantly": the ready marker is printed by a userland process and logd
    // is another one, so a single read races a window the design does not
    // promise to close. Ten seconds is three orders of magnitude above what a
    // working logd needs — the measurement below says what it actually took —
    // so a broken one still reds.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let began = std::time::Instant::now();
    let mut running;
    let mut running_text;
    let mut running_name;
    loop {
        (running_name, running) = newest_log(&image_path, start, len)?;
        running_text = String::from_utf8_lossy(&running).into_owned();
        if running_text.contains("Boot: complete") || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let took = began.elapsed();
    if !running_text.contains(&nonce) {
        return Err(format!(
            "the log on the device does not carry this boot's partition GUID ({nonce:?}); it is \
             {} bytes and starts {:?}",
            running.len(),
            running_text.chars().take(120).collect::<String>()
        ));
    }
    if !running_text.contains("Boot: complete") {
        return Err(format!(
            "the log on the device stops before `Boot: complete` at {} bytes — logd wrote \
             once when it opened the file and never again",
            running.len()
        ));
    }
    if running_text.contains("Shutting down.") {
        return Err("the guest shut down before the mid-run read".to_string());
    }
    eprintln!(
        "  [log] {running_name}: {} bytes on the device {} ms after the ready marker, with the \
         machine still running and through `Boot: complete`",
        running.len(),
        took.as_millis()
    );

    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let tail = qemu.drain_serial(Duration::from_secs(20));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if tail.contains(bad) {
            return Err(format!("{bad:?} on the way down\n{tail}"));
        }
    }

    let after = std::fs::read(&image_path).map_err(|e| format!("read the image back: {e}"))?;
    let (final_name, final_log) = newest_log(&image_path, start, len)?;
    if final_name != running_name {
        return Err(format!(
            "the shutdown moved this boot's log from {running_name} to {final_name}, which at the \
             shipped bound means it wrote a megabyte on the way down"
        ));
    }
    let final_text = String::from_utf8_lossy(&final_log).into_owned();
    if !final_text.contains("Shutting down.") {
        return Err(format!(
            "the shutdown's own last line never reached the file: {} bytes, ending {:?}",
            final_log.len(),
            final_text.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
        ));
    }
    if final_log.len() <= running.len() {
        return Err(format!(
            "the file is {} bytes after the shutdown and was {} before it",
            final_log.len(),
            running.len()
        ));
    }

    let complaints_after = check(&after[start..start + len]);
    if !complaints_after.is_empty() {
        return Err(format!(
            "writing the log gave the checker something to say about a volume it had nothing to \
             say about:\n{}",
            describe(&complaints_after)
        ));
    }
    eprintln!(
        "  [log] {final_name}: {} bytes after the shutdown, carrying its last line; the checker \
         still silent",
        final_log.len()
    );
    let _ = std::fs::remove_file(&image_path);

    rotation(test_config, c_bins, rust_bins)
}

/// The newest of the kernel's log files on the volume, with its name.
///
/// `logd` names one file per boot for the wall clock and continues a long boot
/// in `_0002` and up, both of which sort after everything older — so the last
/// name is this boot's most recent file. Read off the device, like
/// everything else here.
pub fn newest_log(image_path: &Path, start: usize, len: usize) -> Result<(String, Vec<u8>), String> {
    let image = std::fs::read(image_path).map_err(|e| format!("read the image: {e}"))?;
    if start + len > image.len() {
        return Err(format!("the image shrank to {} bytes", image.len()));
    }
    let volume = &image[start..start + len];
    let logs = log_names(volume)?;
    let newest = logs.last().ok_or("the log volume holds no .log file at all")?;
    let mut found = read_files(volume, &[newest.as_str()])?;
    Ok((newest.clone(), need(found.pop().flatten(), newest)?))
}

/// Every `.log` file on the volume, in the order their names sort.
fn log_names(volume: &[u8]) -> Result<Vec<String>, String> {
    Ok(root_entries(volume)?
        .into_iter()
        .filter(|e| e.name.ends_with(".log"))
        .map(|e| e.name)
        .collect())
}

/// The bound, from `tests/logrotatecase`: `/bin/logd` rotating at 256 bytes
/// rather than a mebibyte, which one boot's own log crosses many times over, so
/// both the continuation path and the retention path run on the shipped code.
///
/// **A config and no longer a kernel parameter.** The bound moved into a
/// userland program at L6, and the way a userland program is given a number is
/// its manifest row — so the arming is an image this repository builds rather
/// than a word on the kernel's command line. The other caller of that config is
/// `usb_boot_stick_pulled`, which wants the same rotation in flight for a
/// different reason.
fn rotation(
    _test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/logrotatecase");
    let image_path = test_dir().join("kernel-log-rotate.img");
    let image = qemu::build_boot_image(&config, c_bins, rust_bins, &[]);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = log_extent(&image, &image_path)?;

    let mut qemu = QemuInstance::boot_with_options(
        &config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            boot_image: Some(image_path.clone()),
            ..Default::default()
        },
    );
    let mut log = qemu.boot_log().to_string();
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    log.push_str(&qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);

    // At least twice, not at least once. One continuation proves only that the
    // bound is noticed; the second is the one that runs with an earlier part of
    // the same boot already on the volume, which is what the name has to stay
    // clear of.
    let continuations = log.matches("and this boot continues in").count();
    if continuations < 2 {
        return Err(format!(
            "the log continued into a new file {continuations} times, wanted at least two:\n{}",
            volume_lines(&log)
        ));
    }

    let image = std::fs::read(&image_path).map_err(|e| format!("read the image back: {e}"))?;
    let entries = root_entries(&image[start..start + len])?;
    let logs: Vec<&Entry> = entries.iter().filter(|e| e.name.ends_with(".log")).collect();
    // A part is a flush batch that crossed the bound rather than 256 bytes of
    // log — the sink drains everything pending before it looks at the size —
    // so a metal-sim boot makes a handful, measured at four. That is under the
    // retention bound, which is why this only requires the count to stay inside
    // it; deleting the oldest is `wall_clock_file`'s claim, staged with a full
    // volume rather than hoped for here.
    if logs.len() < 2 || logs.len() > super::wallclock::MAX_LOG_FILES {
        return Err(format!(
            "the volume holds {} log files, wanted 2..={}: {}",
            logs.len(),
            super::wallclock::MAX_LOG_FILES,
            logs.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    // Every part but the newest is one that *filled*, which is the only reason
    // a newer one exists. A part under the bound means something else started a
    // file.
    for entry in &logs[..logs.len() - 1] {
        if entry.len < 256 {
            return Err(format!(
                "{} is {} bytes and is not the newest part, so it did not fill before the next \
                 one started",
                entry.name, entry.len
            ));
        }
    }
    // **The claim is that the shutdown's own last line reached the volume**, so
    // the search is every part of this boot and not a guess at which one it
    // landed in. The image is built fresh for this arm, so every `.log` here is
    // this boot's.
    //
    // It used to look at the two newest and that was an assumption about the
    // *writer*: the kernel sink drained everything it was owed in one flush and
    // then looked at the size, so the tail was in the last part or in the one
    // before it. `/bin/logd` writes a batch, syncs it, publishes `durable` and
    // then looks at the size, and at a 256-byte bound a batch is a part — so
    // records the machine emits while `SYS_SHUTDOWN` is waiting push the line
    // several parts back. That is the bound doing what it is set to do, and an
    // assertion that reads it as a failure is an assertion about the old code.
    let names: Vec<&str> = logs.iter().map(|e| e.name.as_str()).collect();
    let tail_at = read_files(&image[start..start + len], &names)?
        .into_iter()
        .enumerate()
        .find(|(_, bytes)| {
            bytes.as_ref().is_some_and(|b| String::from_utf8_lossy(b).contains("Shutting down."))
        })
        .map(|(i, _)| i);
    let Some(tail_at) = tail_at else {
        let newest = read_files(&image[start..start + len], &names[names.len() - 1..])?
            .pop()
            .flatten()
            .unwrap_or_default();
        let newest = String::from_utf8_lossy(&newest).into_owned();
        return Err(format!(
            "the shutdown's last line is in none of the {} parts on the volume ({}), so §6.3's \
             bounded wait on LOG_DURABLE_NS did not deliver it.\nthe newest part ends:\n{}\nwhat \
             the guest said:\n{}",
            logs.len(),
            names.join(", "),
            newest.lines().rev().take(4).collect::<Vec<_>>().join("\n"),
            volume_lines(&log)
        ));
    };
    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [log] continued {continuations} times at the 256-byte bound, leaving {} parts at the \
         {}-file bound, newest {}; the shutdown's last line is in part {} of {}",
        logs.len(),
        super::wallclock::MAX_LOG_FILES,
        logs.last().map_or("none", |e| e.name.as_str()),
        tail_at + 1,
        logs.len()
    );
    Ok(())
}

/// The boot disk arrives *after* the port scan, and both mounts still happen.
///
/// The machine the T14 was on the boot it lost `/boot` and `/log`, and the one
/// `xhci_slow_connect` cannot be: that gate hides the whole bus, which is the
/// case `xhci::EMPTY_BUS_NS` already keeps looking through. Here the bus is
/// populated and only the disk is late — five HID devices settle,
/// `await_connect_settle` ends on *them* because its own condition is a connect
/// set that has held still and is non-empty, and `scan_ports` runs with no disk
/// on it. Everything downstream then behaves exactly as it did on the laptop:
/// the machine boots, userland comes up, and there is no `/log` to write to.
///
/// Three things have to hold together, and the first is what stops the other two
/// being vacuous:
///
/// - the boot scan really did finish with **no** disk (`usb-storage: 0
///   device(s)`) while really having found the HIDs, so the interleaving under
///   test is the one that happened rather than an ordinary boot;
/// - both volumes mount anyway;
/// - and `kernel.log` is on the device afterwards carrying *this* boot's
///   partition GUID, read off the image on the host rather than out of the
///   guest's own account of itself.
pub fn late_storage_connect(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const PARAMS: &[&str] = &["xhci-slow-storage-connect"];
    let image_path = test_dir().join("late-connect-boot.img");
    let image = qemu::build_boot_image(test_config, c_bins, rust_bins, PARAMS);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = log_extent(&image, &image_path)?;
    let guid = esp_guid(&image, &image_path)?;
    let nonce = format!("gpt: firmware booted us from partition {guid} ");

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            // The one profile with the boot stick on port register 1 *and* other
            // USB devices behind it. A profile with an empty bus would settle on
            // `EMPTY_BUS_NS` and never reach this interleaving.
            profile: qemu::Profile::MetalUsb,
            boot_image: Some(image_path.clone()),
            kernel_params: PARAMS,
            ..Default::default()
        },
    );
    let boot = qemu.boot_log().to_string();
    serial::Serial::named("boot console", boot.as_str()).must_be_clean()?;

    if !boot.contains("usb-storage: 0 device(s)") {
        return Err(format!(
            "the boot scan bound a disk, so the port was not held empty and this gate is \
             measuring an ordinary boot\n{}",
            volume_lines(&boot)
        ));
    }
    // The other half of non-vacuity: a bus that is empty as well as diskless is
    // the machine `xhci_slow_connect` already covers, and it takes a different
    // path out of the settle.
    if boot.contains("xHCI: 1 controller(s), 0 HID device(s)") {
        return Err(format!(
            "the whole bus read empty, not just the disk's port — this is \
             xhci_slow_connect's machine and the settle leaves it by the other door\n{}",
            volume_lines(&boot)
        ));
    }

    for want in [
        "boot-volume: partition mounted",
        "log-volume: partition mounted",
        "logd: this boot's kernel log is",
    ] {
        if !boot.contains(want) {
            return Err(format!(
                "the disk arrived after the port scan and {want:?} never happened — the probe \
                 stopped looking while the machine still had no boot volume\n{}",
                volume_lines(&boot)
            ));
        }
    }

    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let tail = qemu.drain_serial(Duration::from_secs(20));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if tail.contains(bad) {
            return Err(format!("{bad:?} on the way down\n{tail}"));
        }
    }

    // Ground truth is the device, not the line the guest printed about it.
    let log = newest_log(&image_path, start, len)?.1;
    let text = String::from_utf8_lossy(&log).into_owned();
    if !text.contains(&nonce) {
        return Err(format!(
            "the log on the device does not carry this boot's partition GUID ({nonce:?}); it is \
             {} bytes",
            log.len()
        ));
    }
    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [log] the disk was invisible to the port scan and both volumes mounted anyway; \
         {} bytes of kernel.log on the device",
        log.len()
    );
    Ok(())
}

/// One file read out of a partition inside a disk image on the host.
pub fn log_on_device(
    image_path: &Path,
    start: usize,
    len: usize,
    name: &str,
) -> Result<Vec<u8>, String> {
    let image = std::fs::read(image_path).map_err(|e| format!("read the image: {e}"))?;
    if start + len > image.len() {
        return Err(format!("the image shrank to {} bytes", image.len()));
    }
    let mut found = read_files(&image[start..start + len], &[name])?;
    need(found.pop().flatten(), name)
}

/// A page of a `/log` file that the device will not give back, and the partial
/// write that used to merge into the hole and persist it.
///
/// `file_cache::write_page` re-reads a page it is about to partially overwrite,
/// through the file's backing, and merges the new bytes into what comes back.
/// `FatBacking::read_page` returned `()`, so a failed read was indistinguishable
/// from a page of zeros: the new bytes went into those zeros and `flush_file`
/// wrote the result back over data that was already on the stick.
///
/// Three separate claims, and none of them is the others:
///
/// - the failure is **reported** (`serving zeros`, the marker triage greps for,
///   which this path could not emit at all);
/// - the failure **propagates** to the caller — `FatBacking` →
///   `file_cache::write_page` → `ops::try_write` → the process, every one of which
///   returned `()` or swallowed on some link of the chain;
/// - the file on the device is **not corrupted**, checked on the host against
///   the bytes the host itself wrote. This is the claim the other two exist to
///   serve, and the one that stays meaningful if the log lines are reworded.
///
/// **The trigger moved with task #140 and the coverage narrowed with it.** The
/// kernel's log sink used to reach this path on its own: a boot reopened the
/// `kernel.log` the boot before it left, and the first append was a partial
/// write into a page that had to come off the stick. One file per boot ended
/// that — the sink always creates now, and its own pages stay resident for the
/// whole boot because every append sets the CLOCK reference bit on the page it
/// is appending to. So `Sink::append`'s error return is still correct and is no
/// longer reachable from a boot; what is exercised here is the same
/// `write_page` hazard through the path that *can* still reach it, which is any
/// process appending to a file that already has bytes on the volume.
pub fn log_backing_read_error(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    // `test-small-caches` is what makes the page actually get evicted: at the
    // shipped ceiling the log's few pages stay resident for the whole boot and
    // the re-read the injection targets never happens. The eviction code is the
    // shipped code; only the bound moves.
    const PARAMS: &[&str] = &["fat-backing-read-fails"];
    const SERVING_ZEROS: &str = "failed; serving zeros";
    /// Mirrored in `tests/toyos-rust-tests/src/bin/log_volume_reread.rs`.
    const STAGED: &str = "staged-reread.txt";
    /// Printable and longer than the offset the guest writes at, so the page is
    /// fetched rather than extended, and so a merge into zeros shows up as a
    /// run of NULs in a file that is otherwise entirely text.
    const STAGED_TEXT: &[u8] = b"written by the host onto the log volume before this machine \
                                 started, and not to be changed by it\n";

    let image_path = test_dir().join("fat-backing-read-fails.img");
    let mut image = qemu::build_boot_image(test_config, c_bins, rust_bins, PARAMS);
    // Written before the extent is asked for: `log_extent` parses the GPT off
    // the file, not the buffer.
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = log_extent(&image, &image_path)?;
    // The host's half of the fixture, on the device before there is a guest.
    // This is what makes the trigger deterministic rather than a matter of
    // whether some page happened to be evicted: none of this file's pages can
    // be resident, because the machine has never seen it.
    stage_files(
        &mut image[start..start + len],
        &[(STAGED.to_string(), STAGED_TEXT.to_vec())],
    )?;
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            boot_image: Some(image_path.clone()),
            kernel_params: PARAMS,
            ..Default::default()
        },
    );
    let mut log = qemu.boot_log().to_string();
    let attempt = qemu.run_test("test_rs_log_volume_reread", Duration::from_secs(30));
    // Both streams: the kernel's own line about the refused read is on the
    // serial console, and the process's account of what it was told is on
    // stdout. The claims below need one of each.
    log.push_str(&attempt.stdout);
    log.push_str(&attempt.serial);
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    log.push_str(&qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} on the boot\n{log}"));
        }
    }

    // 1. The injection reached the code, and the code said so. Before this
    //    landed there was no string here to find: the two sibling backings
    //    print `serving zeros` and this one returned in silence.
    let reported = log.matches(SERVING_ZEROS).count();
    if reported == 0 {
        return Err(format!(
            "no {SERVING_ZEROS:?} line — either the injection never reached a page re-read (so \
             this boot proves nothing) or the FAT backing is still failing silently\n{log}"
        ));
    }

    // 2. It propagated the whole way, to the one caller that can be asked. A
    //    write reported as succeeding is the defect: the process has no way to
    //    know its bytes went into a page invented out of a failed read.
    if !log.contains("reread: the write failed") {
        return Err(format!(
            "the process was not told: a refused page has to reach `ops::try_write` as an error \
             instead of being merged into zeros\n{log}"
        ));
    }

    // 2b. The read of the same page, which is the sharper half and was the
    //     later fix: `file_cache::read_page` returned `()`, so the process got
    //     the page zeroed and a success. Nothing above it — not this test, not
    //     a `cat`, not the ELF loader — can tell that from a file that really
    //     is zeros there, which is why the count is in the guest's line and
    //     why this refuses the success rather than checking the bytes.
    if !log.contains("reread: the read failed") {
        let said = log
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("reread: the read"))
            .unwrap_or("(the guest never reported its read)");
        return Err(format!(
            "a page the device would not give back reached the process as data: {said}\n\
             A failed read has to be distinguishable from a hole.\n{log}"
        ));
    }

    // 3. And the machine is fine. A refusal that costs the boot is not a fix.
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish\n{log}"));
    }
    if !log.contains("Shutting down.") {
        return Err(format!("the guest did not shut down cleanly\n{log}"));
    }

    // 4. Ground truth: the file on the device, against the bytes the host put
    //    there. A page merged into a failed re-fetch is zeros where the text
    //    was, so this catches the corruption whether or not anything was said
    //    about it — the console being exactly what the guest would be wrong
    //    about.
    let after = std::fs::read(&image_path).map_err(|e| format!("read the image back: {e}"))?;
    let on_device = need(read_files(&after[start..start + len], &[STAGED])?.pop().flatten(), STAGED)?;
    if on_device != STAGED_TEXT {
        let at = on_device.iter().zip(STAGED_TEXT).position(|(a, b)| a != b);
        return Err(format!(
            "the guest changed {STAGED} on the device: {} bytes became {}, first differing at \
             {at:?} — a partial write was merged into a page the device would not give back, and \
             flushed",
            STAGED_TEXT.len(),
            on_device.len()
        ));
    }

    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [log] {reported} page re-read(s) refused by the device: reported, propagated to the \
         process that asked, and the {} bytes the host staged are intact",
        STAGED_TEXT.len()
    );
    Ok(())
}

/// A mounted volume that stops answering, and the questions `vfs::FileSystem`
/// used to fold into "no such file".
///
/// `open` and `read_dir` reached filesystem methods returning an `Option`, a
/// `bool` and a bare `u64`, so a device that refused a transfer was reported to
/// userland as a name that is not there. Nothing downstream can act on that: it
/// creates a file over one that exists, reports a program missing off a stick
/// that is merely unhappy, and unlinks a name it believes is already gone.
///
/// `fat-boot-reads-fail` is the actuator and its sibling
/// `fat-backing-read-fails` is not: that one injects at
/// `FatBacking::read_page`, which is the page-fault path and reaches no
/// directory entry, so with it armed every question below still succeeds. This
/// one is under `Fat32` itself. Neither can be staged from the host — both
/// partitions live on the disk the guest is running from, so `readonly=on` is
/// writes only and `rerror` takes the whole drive.
///
/// **The mount line is a load-bearing assertion and not decoration.** A `/boot`
/// that failed to mount is not a mount at all: `Vfs::resolve_fs` falls through
/// to the root filesystem, the initrd has no `boot/` in it, and every question
/// below would then answer `NotFound` for an honest reason — which is precisely
/// the string this test exists to refuse.
pub fn boot_volume_metadata_error(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const PARAMS: &[&str] = &["fat-boot-reads-fail"];
    /// What the injection prints from under `Fat32`, once per refused read.
    const REFUSED: &str = "boot-volume: read of";

    let image_path = test_dir().join("fat-boot-reads-fail.img");
    let image = qemu::build_boot_image(test_config, c_bins, rust_bins, PARAMS);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            boot_image: Some(image_path.clone()),
            kernel_params: PARAMS,
            ..Default::default()
        },
    );
    let mut log = qemu.boot_log().to_string();
    if !log.contains("boot-volume: partition mounted") {
        return Err(format!(
            "the boot partition did not mount, so every question below would answer NotFound \
             for an honest reason and this boot proves nothing:\n{}",
            volume_lines(&log)
        ));
    }

    let attempt = qemu.run_test("test_rs_boot_volume_metadata_error", Duration::from_secs(30));
    log.push_str(&attempt.stdout);
    log.push_str(&attempt.serial);
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    log.push_str(&qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} on the boot\n{log}"));
        }
    }

    // 1. The injection reached the device layer under the filesystem, so what
    //    follows is a volume that was asked and would not answer, rather than a
    //    code path that never ran.
    let refused = log.matches(REFUSED).count();
    if refused == 0 {
        return Err(format!(
            "no {REFUSED:?} line — nothing read the boot volume after it was mounted, so this \
             boot exercises none of the metadata path\n{log}"
        ));
    }

    // 2. and 3. The two questions, each judged on the word it came back with.
    //    `NotFound` is named explicitly because it is the *old* answer and the
    //    whole defect: a check for "an error" alone would have passed before the
    //    change, since a missing file is an error too.
    for (what, prefix) in [("open", "boot-io: open"), ("read_dir", "boot-io: read_dir")] {
        let Some(said) = log.lines().map(str::trim).find(|l| l.starts_with(prefix)) else {
            return Err(format!("the guest never reported its {what}\n{log}"));
        };
        if said.contains("succeeded") {
            return Err(format!(
                "{what} of a volume that refused every read succeeded: {said}\n{log}"
            ));
        }
        if said.contains("kind=NotFound") {
            return Err(format!(
                "{what} reported a device that would not answer as a missing file: {said}\n\
                 That is the conflation this gate exists for — `vfs::FileSystem` folding a \
                 refused read into NotFound.\n{log}"
            ));
        }
    }

    // 4. And it is this volume's refusal and not the machine's. The other FAT
    //    mount is the same adapter over the same driver, so a break that
    //    reached both would look identical in the two lines above.
    if !log.contains("boot-io: /log still lists") {
        return Err(format!(
            "/log stopped answering too, so the refusal above is not the boot volume's\n{log}"
        ));
    }

    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not finish\n{log}"));
    }
    if !log.contains("Shutting down.") {
        return Err(format!("the guest did not shut down cleanly\n{log}"));
    }

    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [boot] {refused} filesystem read(s) refused by the mounted boot volume: open and \
         read_dir both reported the device rather than a missing file, and /log kept answering"
    );
    Ok(())
}

/// The image side of the whole exercise, with nothing mounted and nothing
/// booted: the log partition is what a desktop OS will pick up on plug-in.
///
/// Every claim here is about bytes this build produced, which is the boundary
/// the suite tests to. What another operating system then *does* with those
/// bytes is that OS's policy and is deliberately not asserted anywhere — see
/// this module's header.
///
/// The type GUID is written out in full rather than compared against
/// `partition_types::BASIC`, because the image builder used that same constant
/// and a comparison against it would agree with any value it held.
pub fn log_partition_layout(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    /// Microsoft Basic Data. Every desktop OS treats a partition of this type
    /// as one of its own to mount; an EFI-typed one macOS will not touch, which
    /// is why the log moved off the ESP.
    const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
    const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

    let image_path = test_dir().join("log-layout.img");
    let image = qemu::build_boot_image(test_config, c_bins, rust_bins, &[]);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;

    let disk = gpt::GptConfig::new()
        .writable(false)
        .logical_block_size(LogicalBlockSize::Lb512)
        .open(&image_path)
        .map_err(|e| format!("the built image has no readable GPT: {e}"))?;
    let table: Vec<_> = disk.partitions().values().collect();
    let [esp, log] = table.as_slice() else {
        return Err(format!("the built image has {} partitions, wanted two", table.len()));
    };

    let types = [
        (esp.part_type_guid.guid.to_uppercase(), ESP_TYPE, "ESP"),
        (log.part_type_guid.guid.to_uppercase(), BASIC_DATA, "log partition"),
    ];
    for (got, want, what) in types {
        if got != want {
            return Err(format!("the {what} is typed {got}, wanted {want}"));
        }
    }

    // The attribute field, spelled out. Bit 0 marks a partition the firmware
    // requires and bit 62 marks one hidden from mounting, and either would
    // undo the type: an installer that set them would leave a partition that
    // parses correctly and never appears.
    if log.flags != 0 {
        return Err(format!(
            "the log partition carries attributes {:#018x}; bit 0 (required) is {}, bit 62 \
             (hidden) is {} — both stop a host mounting it and neither is ever wanted here",
            log.flags,
            log.flags & 1,
            (log.flags >> 62) & 1
        ));
    }

    let (esp_guid, log_guid) = (esp.part_guid, log.part_guid);
    if esp_guid.is_nil() || log_guid.is_nil() {
        return Err("a partition was given the all-zero GUID, which GPT reads as unused".to_string());
    }
    if esp_guid == log_guid {
        return Err(format!("both partitions carry the unique GUID {esp_guid}"));
    }

    // The alignment `create_gpt_disk` asserts, checked again from the table:
    // the kernel mounts both of these over one 4 KiB block device and caches
    // device blocks per volume, so a block belonging to both would be held
    // twice and go stale on the other's write.
    let (esp_start, esp_end) = (esp.first_lba * 512, (esp.last_lba + 1) * 512);
    let (log_start, log_end) = (log.first_lba * 512, (log.last_lba + 1) * 512);
    for (what, start, end) in
        [("ESP", esp_start, esp_end), ("log partition", log_start, log_end)]
    {
        if start % 4096 != 0 || end % 4096 != 0 {
            return Err(format!(
                "the {what} spans bytes {start}..{end}, which is not whole 4 KiB device blocks"
            ));
        }
    }
    if esp_end > log_start {
        return Err(format!("the ESP runs to {esp_end} and the log partition starts at {log_start}"));
    }

    // What the bootloader will read, and the kernel will be given. The file and
    // the entry are the same sixteen bytes in the same order or the handoff is
    // pointing at nothing.
    let (start, len) = esp_extent(&image, &image_path)?;
    let named = log_on_device(&image_path, start, len, "toyos/log.guid")?;
    let named: [u8; 16] = named
        .as_slice()
        .try_into()
        .map_err(|_| format!("toyos/log.guid is {} bytes, wanted 16", named.len()))?;
    if named != log_guid.to_bytes_le() {
        return Err(format!(
            "toyos/log.guid holds {named:02x?}, and the log partition's entry holds {:02x?}",
            log_guid.to_bytes_le()
        ));
    }

    // And the parser that will actually do this on the machine agrees, run
    // here over the same bytes: `toyos_gpt::locate` is the kernel's, and it is
    // given the GUID exactly as the file carries it.
    let located = toyos_gpt::locate(&mut ImageSectors { bytes: &image }, toyos_gpt::Guid(named))
        .map_err(|e| format!("the kernel's own GPT parser cannot find the log partition: {e:?}"))?;
    if located.partition.first_lba != log.first_lba
        || located.partition.last_lba != log.last_lba
    {
        return Err(format!(
            "the kernel's parser puts the log partition at LBA {}..{} and the table says {}..{}",
            located.partition.first_lba, located.partition.last_lba, log.first_lba, log.last_lba
        ));
    }

    // Both places a FAT label lives. The boot-sector field is what a mount
    // reads without walking the root directory; the `VOLUME_ID` entry is what a
    // tool that walks it reads. Written by one call, checked as two, because a
    // volume with one of them is a volume called `NO NAME` somewhere.
    let (log_start, log_len) = log_extent(&image, &image_path)?;
    for (what, at, size, label) in [
        ("ESP", start, len, "TOYOS-BOOT"),
        ("log partition", log_start, log_len, "TOYOS-LOG"),
    ] {
        let fs = fatfs::FileSystem::new(Cursor::new(image[at..at + size].to_vec()), FsOptions::new())
            .map_err(|e| format!("the built {what} does not mount on the host: {e}"))?;
        if fs.volume_label() != label {
            return Err(format!(
                "the {what}'s boot sector calls it {:?}, wanted {label:?}",
                fs.volume_label()
            ));
        }
        let root = fs
            .read_volume_label_from_root_dir()
            .map_err(|e| format!("reading the {what}'s root-directory label: {e}"))?;
        if root.as_deref() != Some(label) {
            return Err(format!(
                "the {what}'s root directory carries the label {root:?}, wanted {label:?}"
            ));
        }
    }

    // Born clean. The ESP is not and cannot be until `fatfs` is forked (known
    // issues §10); this volume has no subdirectory for either of those defects
    // to arise in, and its free-cluster count is recorded at format time.
    let complaints = check(&image[log_start..log_start + log_len]);
    if !complaints.is_empty() {
        return Err(format!("the log partition is not born clean:\n{}", describe(&complaints)));
    }

    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [log] {BASIC_DATA} with attributes 0, labelled TOYOS-LOG in both places, 4 KiB-aligned \
         and disjoint from the ESP, format-clean, and named by toyos/log.guid"
    );
    Ok(())
}

/// A disk image in 512-byte LBAs, for the kernel's own GPT parser.
struct ImageSectors<'a> {
    bytes: &'a [u8],
}

impl toyos_gpt::Sectors for ImageSectors<'_> {
    fn lba_bytes(&self) -> u32 {
        512
    }

    fn lba_count(&self) -> u64 {
        (self.bytes.len() / 512) as u64
    }

    fn read_lba(&mut self, lba: u64, out: &mut [u8]) -> bool {
        let at = lba as usize * 512;
        match self.bytes.get(at..at + out.len()) {
            Some(src) => {
                out.copy_from_slice(src);
                true
            }
            None => false,
        }
    }
}

/// A GUID no table this build produces can contain: `create_boot_image` draws
/// v4 UUIDs, whose version nibble is 4 and whose variant bits are `10`. Written
/// in GPT entry byte order, which is the order everything from the file to the
/// comparison uses.
const FORGED: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
];
/// As `Guid`'s Display prints it: three little-endian fields then raw bytes.
const FORGED_TEXT: &str = "33221100-5544-7766-8899-AABBCCDDEEFF";

/// An image on disk and the `(offset, len)` of its ESP and its log partition,
/// in that order.
pub type ImageWithExtents = (PathBuf, (usize, usize), (usize, usize));

/// A stick as the build made it, with one file changed: the sixteen bytes of
/// `\toyos\log.guid` now name a partition no machine has.
///
/// Everything about the log partition stays as it was — still second in the
/// table, still typed Microsoft Basic Data, still the only other FAT32 on the
/// stick, still exactly where it was — so a kernel that found the volume by
/// type, by format or by position would mount it anyway and every gate built on
/// this would go green on the defect it exists for.
///
/// Returns the image's path and its two partition extents.
pub fn image_with_unnamed_log_partition(
    name: &str,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<ImageWithExtents, String> {
    let image_path = test_dir().join(name);
    let mut image = qemu::build_boot_image(test_config, c_bins, rust_bins, &[]);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let esp = esp_extent(&image, &image_path)?;
    let log = log_extent(&image, &image_path)?;

    {
        let volume = &mut image[esp.0..esp.0 + esp.1];
        let fs = fatfs::FileSystem::new(Cursor::new(&mut *volume), FsOptions::new())
            .map_err(|e| format!("the built ESP does not mount on the host: {e}"))?;
        let dir = fs
            .root_dir()
            .open_dir("toyos")
            .map_err(|e| format!("the built ESP has no toyos directory: {e}"))?;
        let mut file = dir
            .create_file("log.guid")
            .map_err(|e| format!("opening log.guid on the ESP: {e}"))?;
        file.truncate().map_err(|e| format!("truncating log.guid: {e}"))?;
        file.write_all(&FORGED).map_err(|e| format!("writing log.guid: {e}"))?;
    }
    std::fs::write(&image_path, &image).map_err(|e| format!("rewrite the boot image: {e}"))?;
    Ok((image_path, esp, log))
}

/// What the kernel says on the panel when this boot leaves nothing to read
/// afterwards. It is an `alert!`, so the panel paints the row red off the
/// record's `Level` — nothing in the text says so — and `screen_log_absent` is
/// the gate that it does.
pub const NO_LOG_ALERT: &str = "log: no /log";

/// The other arm of the same table: what the kernel says when both halves are
/// there. `screen_diag_boot` is the gate on it.
///
/// **One declaration, beside [`NO_LOG_ALERT`], because the last hand-copied
/// spelling of this line outlived the kernel's by two commits and a nightly.**
/// `screen_diag_boot` carried `"log: this boot is on the console and in"` inline
/// at its assertion — the shape the line had while the kernel opened the log
/// file and could name it. `9ca7631` cut it when `/bin/logd` took the file over
/// and `ecede44` restored the half the kernel still knows, correcting
/// `screen_log_absent` and not this one; the gate then reded on `main` through
/// two nightly dispatches with nothing to say why, and `src/redlist.rs` carries
/// the two runs. The writer is `report_log_destination` in `kernel/src/main.rs`,
/// whose `(true, true)` arm formats exactly this — a test asserting on a log
/// line reads it from one named declaration that cites its writer, never from
/// a literal copied at the assertion.
pub const LOG_ON_CONSOLE_AND_FILE: &str = "log: this boot is on the console and on /log";

/// The log partition is named, never discovered — proved by moving the name.
///
/// The refusal has three halves and each is separately checkable:
///
/// - it is **named**: the `gpt:` line says which GUID it could not find, which
///   is what a person holding the stick needs;
/// - it costs **nothing else**: `/boot` still mounts and the boot still
///   completes, because a missing diagnostic is not worth a machine;
/// - and it is not a **fallback**: the log partition is read back on the host
///   afterwards and must still be empty. Falling back to the ESP would also
///   leave it empty, so `logd` must not have opened a file either.
pub fn log_partition_identity(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let (image_path, _, (log_start, log_len)) = image_with_unnamed_log_partition(
        "log-identity-boot.img",
        test_config,
        c_bins,
        rust_bins,
    )?;

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            boot_image: Some(image_path.clone()),
            ..Default::default()
        },
    );
    let mut log = qemu.boot_log().to_string();
    log.push_str(&qemu.drain_serial(Duration::from_millis(500)));
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    log.push_str(&qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    for bad in ["PANIC:", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} on a stick whose log partition is not named\n{log}"));
        }
    }

    // The bootloader read the forged file and handed it on unconverted. A
    // mixed-endian slip anywhere on that path shows up here as a different
    // GUID rather than as a mysteriously absent partition.
    let named = format!("gpt: the boot volume names {FORGED_TEXT} as the log partition");
    if !log.contains(&named) {
        return Err(format!(
            "the handoff did not carry the bytes the ESP holds.\nwanted: {named}\n{}",
            volume_lines(&log)
        ));
    }
    let refused = format!("nothing with the log partition's GUID {FORGED_TEXT}");
    if !log.contains(&refused) {
        return Err(format!(
            "the kernel did not refuse the log partition by name.\nwanted: {refused}\n{}",
            volume_lines(&log)
        ));
    }
    if !log.contains("log-volume: not mounted") {
        return Err(format!("the kernel mounted a log volume it was never given:\n{}", volume_lines(&log)));
    }
    if log.contains("logd: this boot's kernel log is") {
        return Err(format!(
            "logd opened a file with no log partition — a fallback is exactly what this must not \
             do:\n{}",
            volume_lines(&log)
        ));
    }
    if !log.contains("logd: no /log on this machine") {
        return Err(format!(
            "logd said nothing about a machine with no /log, so §5.6's other half is missing:\n{}",
            volume_lines(&log)
        ));
    }

    // And nothing else was lost. The stick is a working stick with one file
    // changed on it.
    if !log.contains("boot-volume: partition mounted") {
        return Err(format!("a missing log partition cost the machine /boot:\n{}", volume_lines(&log)));
    }
    if !log.contains("Boot: complete") {
        return Err(format!("a missing log partition cost the boot:\n{log}"));
    }

    // Ground truth: the partition itself. It is still there, still FAT32, and
    // the kernel wrote nothing to it.
    let after = std::fs::read(&image_path).map_err(|e| format!("read the image back: {e}"))?;
    let volume = &after[log_start..log_start + log_len];
    // Any log at all, rather than two names: the kernel picks this boot's from
    // the wall clock, so what has to be absent is the whole family.
    let found = log_names(volume)?;
    if !found.is_empty() {
        return Err(format!(
            "the kernel wrote to a partition it had just refused to identify: {}",
            found.join(", ")
        ));
    }
    let complaints = check(volume);
    if !complaints.is_empty() {
        return Err(format!("the untouched log partition is not clean:\n{}", describe(&complaints)));
    }

    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [log] the name moved and the mount went with it: refused {FORGED_TEXT} by name, /boot \
         and the boot unaffected, nothing written to the partition"
    );
    Ok(())
}
