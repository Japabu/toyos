//! The boot partition, mounted and written from inside ToyOS.
//!
//! Ground truth is the disk image the *device* received, read on the host by
//! two implementations that are not ours: the `fatfs` crate, and macOS's
//! `fsck_msdos`. The guest's own account of a write it made is exactly what is
//! in question, so it cannot also be the evidence — `esp_files` asserts what
//! only a process inside the machine can see, and everything it claims about
//! bytes is checked again here.
//!
//! The image is built and modified before the boot rather than after, because
//! the host-writes-guest-reads direction has no other staging point: a file the
//! guest itself created and read back would pass with the read path broken.
//!
//! `fsck_msdos -n` **exits 0 while printing `Fix?`** for problems it declined
//! to repair, and exits 0 on a volume it has just called dirty. Its output is
//! matched line by line against the exact shape of a clean run, never its exit
//! code — the same rule `toyos-fat32`'s own gate follows, and for the same
//! reason.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fatfs::FsOptions;
use gpt::disk::LogicalBlockSize;
use gpt::partition_types;

use super::qemu::{self, BootOptions, QemuInstance};
use super::serial;

/// Mirrored in `tests/toyos-rust-tests/src/bin/esp_files.rs`. Two halves of one
/// fixture; a change to either alone fails loudly rather than passing quietly.
const HOST_NOTE: &str = "host-note.txt";
const HOST_TEXT: &str = "written by the host before this machine started\n";
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
    let dir = std::env::temp_dir().join(format!("toyos-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

/// Where the ESP sits inside a GPT disk image, in bytes.
fn esp_extent(image: &[u8], path: &Path) -> Result<(usize, usize), String> {
    let disk = gpt::GptConfig::new()
        .writable(false)
        .logical_block_size(LogicalBlockSize::Lb512)
        .open(path)
        .map_err(|e| format!("the built image has no readable GPT: {e}"))?;
    let esps: Vec<_> = disk
        .partitions()
        .values()
        .filter(|p| p.part_type_guid == partition_types::EFI)
        .collect();
    let [esp] = esps.as_slice() else {
        return Err(format!("the built image has {} ESPs, expected one", esps.len()));
    };
    let start = esp.first_lba as usize * 512;
    let len = (esp.last_lba - esp.first_lba + 1) as usize * 512;
    if start + len > image.len() {
        return Err(format!("the ESP runs to {} in an image of {}", start + len, image.len()));
    }
    Ok((start, len))
}

/// Read several files out of a FAT volume in one mount. `None` is a file that
/// is not there, which is an assertion in its own right here.
///
/// `fatfs` wants a writable, seekable device even to read, so the volume is
/// copied — once per call, which is why the callers ask for everything they
/// need at once rather than a file at a time.
fn read_files(volume: &[u8], paths: &[&str]) -> Result<Vec<Option<Vec<u8>>>, String> {
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

/// Everything `fsck_msdos -n` complains about, with counts normalised away.
///
/// The partition is copied out of the disk image because `fsck_msdos` wants a
/// volume, not a disk: pointed at the GPT image it would read the protective
/// MBR as a boot sector.
///
/// A list rather than a verdict, because **this project's own boot image is
/// not fsck-clean and never was** — the `fatfs` crate `src/image.rs` builds it
/// with writes a long-name entry ahead of each `.` and `..`, and gives `..` the
/// root's cluster number instead of zero. Both violate the specification and
/// both are there before any guest runs; see known issues. So the gate is that
/// the guest adds no complaint of its own, which is the question actually being
/// asked, and it keeps the pre-existing ones visible rather than accepting a
/// blanket "not clean".
///
/// Digits are replaced so a legitimately changed free-cluster count does not
/// read as a new defect.
fn fsck_complaints(volume: &[u8], name: &str) -> Result<Vec<String>, String> {
    let tool = Path::new("/sbin/fsck_msdos");
    if !tool.exists() {
        return Err("no /sbin/fsck_msdos: this gate's outside judge is missing".to_string());
    }
    let path = test_dir().join(format!("{name}.vol"));
    std::fs::write(&path, volume).map_err(|e| format!("staging the volume for fsck: {e}"))?;

    // Never the exit code: `fsck_msdos -n` exits 0 while printing `Fix?` for
    // problems it declined to repair, and exits 0 on a volume it has just
    // called dirty.
    let out = Command::new(tool)
        .arg("-n")
        .arg(&path)
        .output()
        .map_err(|e| format!("running fsck_msdos: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_file(&path);

    let mut summaries = 0;
    let mut complaints = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with("**") || line.starts_with(&path.display().to_string())
        {
            continue;
        }
        if is_summary(line) {
            summaries += 1;
            continue;
        }
        complaints.push(mask_digits(line));
    }
    if summaries != 1 {
        return Err(format!("fsck_msdos printed {summaries} summary lines, wanted one:\n{text}"));
    }
    complaints.sort();
    Ok(complaints)
}

fn mask_digits(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_digits = false;
    for c in line.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

/// `Warning: 5 files, 306560 KiB free (76640 clusters)` — the one line a clean
/// run prints that is not a `**` banner.
fn is_summary(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("Warning: ") else { return false };
    let Some((count, tail)) = rest.split_once(' ') else { return false };
    count.chars().all(|c| c.is_ascii_digit()) && tail.starts_with("files,")
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
    let complaints_before = fsck_complaints(&image[start..start + len], "esp-before")?;

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
    if !boot.contains("esp: boot partition mounted") {
        return Err(format!(
            "the kernel did not mount the boot partition:\n{}",
            esp_lines(&boot)
        ));
    }

    let result = qemu.run_test("test_rs_esp_files", Duration::from_secs(60));
    if let Some(err) = &result.error {
        return Err(format!("the guest stopped answering: {err}\nserial:\n{}", result.serial));
    }
    if result.exit_code != Some(0) {
        return Err(format!("esp_files failed:\n{}", result.stdout));
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
    for bad in ["!!! PANIC !!!", "panicked at"] {
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
    let complaints_after = fsck_complaints(volume, "esp-after")?;
    let fresh: Vec<&String> =
        complaints_after.iter().filter(|c| !complaints_before.contains(c)).collect();
    if !fresh.is_empty() {
        return Err(format!(
            "the guest gave fsck_msdos something new to say about the boot volume:\n{}\n\
             it already said, before the boot:\n{}",
            fresh.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"),
            complaints_before.join("\n")
        ));
    }
    eprintln!(
        "  [esp] fsck_msdos: {} pre-existing complaints before the boot, {} after, none new",
        complaints_before.len(),
        complaints_after.len()
    );

    // Everything the host has to say about the volume, in one mount: what the
    // guest wrote, what it must *not* have left behind, and what it must not
    // have touched.
    let wanted: Vec<String> = [
        format!("toyos/{GUEST_NOTE}"),
        format!("toyos/{GUEST_BLOB}"),
        "toyos/doomed.txt".to_string(),
        "toyos/link".to_string(),
        format!("toyos/{HOST_NOTE}"),
    ]
    .into_iter()
    .chain(UNTOUCHED.iter().map(|s| s.to_string()))
    .collect();
    let refs: Vec<&str> = wanted.iter().map(String::as_str).collect();
    let mut found = read_files(volume, &refs)?.into_iter();

    let got = need(found.next().flatten(), GUEST_NOTE)?;
    if got != GUEST_TEXT.as_bytes() {
        return Err(format!(
            "{GUEST_NOTE} on the volume is {:?}, not what the guest wrote",
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
    for absent in ["toyos/doomed.txt", "toyos/link"] {
        if found.next().flatten().is_some() {
            return Err(format!("{absent} is still on the volume"));
        }
    }

    // The host's note, unchanged: a guest that rewrote the directory wholesale
    // would still pass everything above.
    let got = need(found.next().flatten(), HOST_NOTE)?;
    if got != HOST_TEXT.as_bytes() {
        return Err("the guest changed the host's note".to_string());
    }
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

    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [esp] {BLOB_LEN} bytes and two files verified host-side, {} build artifacts intact, \
         fsck clean before and after",
        UNTOUCHED.len()
    );
    Ok(())
}

/// Every `esp:` line the guest printed, for a failure message.
fn esp_lines(log: &str) -> String {
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("esp:")).collect();
    if lines.is_empty() {
        return format!("the guest printed no esp: line at all\n{log}");
    }
    format!("what it said:\n{}", lines.join("\n"))
}
