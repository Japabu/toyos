//! `cp` and `mv` against a real volume, judged on the disk image the device
//! received rather than on what the guest said it did.
//!
//! Two questions, one boot, one volume, because the second needs the first to
//! have already spent the space:
//!
//! 1. **A copy is byte-exact on the device.** The source is staged by the host
//!    before the machine exists, so a guest that read back its own writes
//!    cannot pass — and it is several times `cp`'s flush interval, so the copy
//!    reaches the device in a series of flushes rather than one.
//! 2. **A copy that runs out of volume refuses, and leaves nothing.** The
//!    volume is filled here to a hair over one copy's worth of free space, so
//!    the first `cp` succeeds and the second cannot. What the second must leave
//!    behind is *nothing*: no file under the destination's name, no `.part`
//!    sibling, and nothing new for the volume checker to say — a half-allocated
//!    cluster chain is what a "did the command exit non-zero" check cannot see.
//!
//! The log partition and not the ESP, which is where this started. Measured on
//! a freshly built test image: the ESP has **167 free clusters, 85,504 bytes**.
//! `create_esp_volume` sizes it at `content + 4 MiB`, and at that volume size
//! FAT32 uses 512-byte clusters, so the two FATs describing half a million of
//! them eat the whole four megabytes of slack. Nothing large can be written to
//! `/boot` at runtime; the log partition is the only writable FAT32 volume on
//! the stick with room, and it starts with 33 MiB free.
//!
//! Which means sharing with `/bin/logd`. [`LEAVE_FREE`] is set so that after the
//! copy that succeeds there are still about two megabytes for this boot's log —
//! a boot's log is a few tens of kilobytes, and logd stops and says so if a
//! write ever fails, so a squeeze here would be visible rather
//! than silent.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fatfs::FsOptions;

use super::qemu::{self, BootOptions, QemuInstance};
use super::serial;
use toyos_fat32_check::{check, describe};

use super::volumes::{log_extent, read_files};

/// The file the guest copies. Several times `cp`'s `FLUSH_BYTES` and not a page
/// multiple: the periodic flush fires repeatedly and the tail is partial, which
/// are the two shapes a single-`write_page` copy would get wrong.
const SRC_LEN: usize = 4 * 1024 * 1024 + 137;
const SRC: &str = "cp-src.bin";
const DST: &str = "cp-dst.bin";
/// Where the first copy ends up after `mv` renames it, on a FAT32 volume the
/// host can check.
const MOVED: &str = "cp-moved.bin";
/// The copy that must not fit.
const FULL: &str = "cp-full.bin";
/// What the host writes to eat the rest of the volume.
const FILLER: &str = "filler.bin";

/// Free bytes to leave once the source and the filler are staged.
///
/// Above one source so the first copy fits, below two so the second cannot —
/// and the half that is left over after the first copy is the room `kernel.log`
/// has for the rest of the boot. The band is asserted rather than assumed:
/// the volume's free space is read back after staging.
const LEAVE_FREE: usize = SRC_LEN + SRC_LEN / 2;

fn source_bytes() -> Vec<u8> {
    (0..SRC_LEN).map(|i| (i.wrapping_mul(2_654_435_761) >> 7) as u8).collect()
}

fn test_dir() -> PathBuf {
    super::lane::dir()
}

/// Free bytes, and the cluster size the volume counts them in.
fn free_space(volume: &[u8]) -> Result<(usize, usize), String> {
    let fs = fatfs::FileSystem::new(Cursor::new(volume.to_vec()), FsOptions::new())
        .map_err(|e| format!("the volume does not mount on the host: {e}"))?;
    let stats = fs.stats().map_err(|e| format!("counting free clusters: {e}"))?;
    let cluster = stats.cluster_size() as usize;
    Ok((stats.free_clusters() as usize * cluster, cluster))
}

/// Every name in the volume's root, which is where everything here lands.
fn names(volume: &[u8]) -> Result<Vec<String>, String> {
    let fs = fatfs::FileSystem::new(Cursor::new(volume.to_vec()), FsOptions::new())
        .map_err(|e| format!("the volume does not mount on the host: {e}"))?;
    let mut out = Vec::new();
    for entry in fs.root_dir().iter() {
        let entry = entry.map_err(|e| format!("listing the volume root: {e}"))?;
        out.push(entry.file_name());
    }
    out.sort();
    Ok(out)
}

fn write_file(volume: &mut [u8], path: &str, bytes: &[u8]) -> Result<(), String> {
    let fs = fatfs::FileSystem::new(Cursor::new(volume), FsOptions::new())
        .map_err(|e| format!("the built volume does not mount on the host: {e}"))?;
    let mut file = fs
        .root_dir()
        .create_file(path)
        .map_err(|e| format!("creating {path}: {e}"))?;
    file.write_all(bytes).map_err(|e| format!("writing {path}: {e}"))
}

/// Drive one command through the in-guest runner and say what it did.
///
/// `run_test` sends the whole string after `run `, and the runner splits it
/// into a binary and its arguments — so this is the real `/bin/cp`, invoked the
/// way a user invokes it, and the exit code is the one it returned.
///
/// The serial window is appended to `log` rather than judged here: a `cp` that
/// takes a second produces no kernel line of its own, and `must_be_clean` on a
/// window with none in it is a claim about nothing. The shutdown at the end
/// supplies the liveness for all four windows at once.
fn run(
    qemu: &mut QemuInstance,
    log: &mut serial::Serial,
    command: &str,
) -> Result<(Option<i32>, String), String> {
    let result = qemu.run_test(command, Duration::from_secs(120));
    log.push(&result.serial);
    if let Some(err) = &result.error {
        return Err(format!("`{command}` never finished: {err}\nserial:\n{}", result.serial));
    }
    Ok((result.exit_code, result.stdout))
}

pub fn cp_volume(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let image_path = test_dir().join("toybox-cp-boot.img");
    let mut image = qemu::build_boot_image(test_config, c_bins, rust_bins, &[]);
    std::fs::write(&image_path, &image).map_err(|e| format!("write the boot image: {e}"))?;
    let (start, len) = log_extent(&image, &image_path)?;

    let source = source_bytes();
    write_file(&mut image[start..start + len], SRC, &source)?;

    // The filler is sized from what the volume says it has left, not from a
    // number picked here — a constant would go stale the moment the partition
    // or its cluster size moved, and would do it silently.
    let (free, cluster) = free_space(&image[start..start + len])?;
    if free <= LEAVE_FREE {
        return Err(format!(
            "the log volume has {free} bytes free after staging a {SRC_LEN}-byte source, \
             already at or under the {LEAVE_FREE} this test must leave — there is no room \
             to set the full-volume half up"
        ));
    }
    let filler = (free - LEAVE_FREE) / cluster * cluster;
    if filler > 0 {
        write_file(&mut image[start..start + len], FILLER, &vec![0x5A; filler])?;
    }

    let (left, _) = free_space(&image[start..start + len])?;
    if left < SRC_LEN + cluster || left >= 2 * SRC_LEN {
        return Err(format!(
            "staging left {left} bytes free; one copy needs {SRC_LEN} and two need {}, so \
             this boot would answer only one of the two questions",
            2 * SRC_LEN
        ));
    }
    std::fs::write(&image_path, &image).map_err(|e| format!("rewrite the boot image: {e}"))?;
    let complaints_before = check(&image[start..start + len]);
    eprintln!("  [toybox] staged {SRC_LEN} bytes and a {filler}-byte filler, {left} bytes free");

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions { boot_image: Some(image_path.clone()), ..Default::default() },
    );
    let boot = qemu.boot_log().to_string();
    let boot = serial::Serial::named("boot console", boot.as_str());
    boot.must_be_clean()?;
    boot.must_say("log-volume: partition mounted")?;
    let mut log = serial::Serial::named("the three commands and the shutdown", "");

    // One: the copy that fits.
    let (code, _) = run(&mut qemu, &mut log, &format!("cp /log/{SRC} /log/{DST}"))?;
    if code != Some(0) {
        return Err(format!("cp of a {SRC_LEN}-byte file onto {left} free bytes exited {code:?}"));
    }

    // Two: the same copy again, onto what is left, which is not enough. The
    // refusal has to name the file — an exit code alone leaves the caller
    // guessing which half of the command failed.
    let (code, said) = run(&mut qemu, &mut log, &format!("cp /log/{SRC} /log/{FULL}"))?;
    if code == Some(0) {
        return Err(format!(
            "cp claimed to copy {SRC_LEN} bytes onto a volume with under {SRC_LEN} free"
        ));
    }
    if !said.contains("cp:") || !said.contains(FULL) {
        return Err(format!("cp exited {code:?} without naming what it refused:\n{said}"));
    }
    eprintln!("  [toybox] {}", said.lines().find(|l| l.contains("cp:")).unwrap_or("").trim());

    // Three: a rename on a FAT32 volume, so the host sees the move rather than
    // the guest's account of it.
    let (code, _) = run(&mut qemu, &mut log, &format!("mv /log/{DST} /log/{MOVED}"))?;
    if code != Some(0) {
        return Err(format!("mv within the log volume exited {code:?}"));
    }

    // The shutdown is not politeness: it is what makes the host's view of the
    // backing file the device's view of it.
    writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
    qemu.flush_stdin();
    log.push(&qemu.drain_serial(Duration::from_secs(20)));
    drop(qemu);
    log.must_be_clean()?;

    let after = std::fs::read(&image_path).map_err(|e| format!("read the image back: {e}"))?;
    if after.len() != image.len() {
        return Err(format!("the image is {} bytes, was {}", after.len(), image.len()));
    }
    let volume = &after[start..start + len];

    // Strongest first: a failed allocation that damaged the FAT would still
    // pass every byte comparison below. The staging above is `fatfs`'s work
    // rather than the guest's, so what is asked is that the boot add nothing —
    // and a complaint carries its numbers as fields, so a moved free-cluster
    // count is a different complaint of the same kind rather than a string that
    // has to have its digits blanked before the two lists can be compared.
    let complaints_after = check(volume);
    let fresh: Vec<&toyos_fat32_check::Complaint> =
        complaints_after.iter().filter(|c| !complaints_before.contains(c)).collect();
    if !fresh.is_empty() {
        return Err(format!(
            "cp left the log volume breaking the format:\n{}\n\
             before the boot the checker said:\n{}",
            fresh.iter().map(|c| c.to_string()).collect::<Vec<_>>().join("\n"),
            describe(&complaints_before)
        ));
    }

    let found = names(volume)?;
    for absent in [DST, FULL] {
        if found.iter().any(|n| n.eq_ignore_ascii_case(absent)) {
            return Err(format!("{absent} is on the volume; its root holds {found:?}"));
        }
    }
    // The one a "did it exit non-zero" test cannot see: a refused copy that
    // left its working file behind eats the volume for good.
    let partials: Vec<&String> =
        found.iter().filter(|n| n.to_lowercase().contains("part")).collect();
    if !partials.is_empty() {
        return Err(format!("a refused cp left {partials:?} on the volume"));
    }

    let mut got = read_files(volume, &[MOVED, SRC])?.into_iter();
    let moved = got
        .next()
        .flatten()
        .ok_or_else(|| format!("{MOVED} is not on the volume; its root holds {found:?}"))?;
    if moved.len() != source.len() {
        return Err(format!("the copy is {} bytes on the device, source is {SRC_LEN}", moved.len()));
    }
    if let Some(at) = moved.iter().zip(&source).position(|(a, b)| a != b) {
        return Err(format!("the copy differs from the source at byte {at}"));
    }
    let src_back = got.next().flatten().ok_or_else(|| format!("{SRC} is gone from the volume"))?;
    if src_back != source {
        return Err("the source changed under a copy that only reads it".to_string());
    }

    let _ = std::fs::remove_file(&image_path);
    eprintln!(
        "  [toybox] {SRC_LEN} bytes copied, moved and verified host-side; the copy that did \
         not fit left nothing"
    );
    Ok(())
}
