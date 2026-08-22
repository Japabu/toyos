use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use bcachefs::{Formatted, VecBlockIO};
use toyos_fat32::{BlockAccess, Fat32, FatTime, IoError};

pub fn create_initrd(
    files: &[(String, Vec<u8>)],
    symlinks: &[(String, String)],
    quiet: bool,
) -> Vec<u8> {
    let data_size: usize = files.iter().map(|(_, d)| d.len()).sum::<usize>();
    let total_entries = files.len() + symlinks.len();
    // Estimate: superblock(1) + bitmap + btree nodes + data blocks + backup(1) + 10% padding
    let data_blocks = data_size.div_ceil(4096);
    let btree_blocks = (total_entries / 30).max(2);
    let overhead = 64;
    let total_blocks = (1 + overhead + btree_blocks + data_blocks) * 11 / 10;
    let total_blocks = total_blocks.max(64) as u64;

    let io = VecBlockIO::new(total_blocks);
    let mut fs = Formatted::format(io).expect("format an in-memory image");

    for (name, data) in files {
        if !quiet {
            eprintln!("initrd: adding '{}' ({} bytes)", name, data.len());
        }
        fs.create(name, data, 0)
            .unwrap_or_else(|e| panic!("initrd: failed to add '{}': {:?}", name, e));
    }

    for (name, target) in symlinks {
        if !quiet {
            eprintln!("initrd: symlink '{}' -> '{}'", name, target);
        }
        fs.create_symlink(name, target, 0)
            .unwrap_or_else(|e| panic!("initrd: failed to symlink '{}' -> '{}': {:?}", name, target, e));
    }

    fs.into_io().expect("write an in-memory image").into_vec()
}

/// Takes the artifacts as bytes rather than reading them: the caller stages them
/// under a build-key-derived name first, because cargo's own path is shared by
/// every config and is overwritten by any concurrent build (see `build.rs`).
pub fn create_boot_image(
    kernel_bytes: &[u8],
    bl_bytes: &[u8],
    initrd_bytes: &[u8],
    cmdline: &str,
) -> Vec<u8> {
    // Drawn here and written twice: into the GPT entry that *is* the log
    // partition, and into a file on the ESP that the bootloader hands the
    // kernel. The kernel is given the partition by name; nothing anywhere goes
    // looking for one by type or by format.
    let log_guid = uuid::Uuid::new_v4();
    let esp_volume = create_esp_volume(kernel_bytes, bl_bytes, initrd_bytes, log_guid, cmdline);
    let log_volume = create_log_volume();
    create_gpt_disk(esp_volume, log_volume, log_guid)
}

/// The file on the ESP that says which actuators an image is armed with.
///
/// Written by [`create_esp_volume`] and read back by [`params_of`], from this
/// one name: the writer and its inverse cannot drift apart while they share it.
/// The bootloader spells it `\toyos\cmdline`.
const CMDLINE: &str = "toyos/cmdline";

/// Why a boot may not arm `asked` on the image at `path`, or `None` because
/// that image is armed with exactly that list.
///
/// **An image carries the actuators it was built with**, in [`CMDLINE`] on its
/// own ESP — the file the bootloader reads and hands the kernel in
/// `KernelArgs`. So what a guest will be armed with is a fact about the image,
/// answerable before anything starts and without asking the guest; a caller
/// that has an image and a list can be told it is holding two different boots.
///
/// Pure, and every input a parameter, so both directions can be staged without
/// a guest — which is what `an_image_says_what_it_is_armed_with` does.
pub fn param_conflict(path: &Path, asked: &[&str]) -> Option<String> {
    let baked = match params_of(path) {
        Ok(baked) => baked,
        Err(why) => return Some(why),
    };
    if baked.iter().map(String::as_str).eq(asked.iter().copied()) {
        return None;
    }
    Some(format!(
        "the image {} is armed with {baked:?} and the boot asks for {asked:?}",
        path.display()
    ))
}

/// The actuator list an image is armed with, read back off the image.
///
/// The ESP is located through the partition table rather than at the offset
/// [`create_gpt_disk`] happens to place it at: the writer asks `add_partition`
/// where the partition went, and so does this.
fn params_of(path: &Path) -> Result<Vec<String>, String> {
    let (start, len) = {
        let disk = gpt::GptConfig::new()
            .writable(false)
            .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
            .open(path)
            .map_err(|e| format!("{} has no readable GPT: {e}", path.display()))?;
        let esps: Vec<_> = disk
            .partitions()
            .values()
            .filter(|p| p.part_type_guid == gpt::partition_types::EFI)
            .collect();
        let [esp] = esps.as_slice() else {
            return Err(format!(
                "{} has {} ESPs, and a boot image has one",
                path.display(),
                esps.len()
            ));
        };
        (esp.first_lba * 512, (esp.last_lba - esp.first_lba + 1) * 512)
    };

    // The ESP alone, and never the whole image: a test image is a quarter of a
    // gigabyte and this runs once per boot that stages one.
    let len = usize::try_from(len).map_err(|_| format!("{} has a {len}-byte ESP", path.display()))?;
    let mut volume = vec![0u8; len];
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| format!("seeking to the ESP of {}: {e}", path.display()))?;
    file.read_exact(&mut volume)
        .map_err(|e| format!("reading the ESP of {}: {e}", path.display()))?;

    // Read with the driver the kernel mounts this very volume with, rather than
    // with the crate that formatted it — [`populate`]'s argument, in the other
    // direction.
    let mut fs = Fat32::mount(VolumeIo(&mut volume))
        .map_err(|e| format!("the ESP of {} does not mount: {e}", path.display()))?;
    let mut file = fs
        .open(CMDLINE)
        .map_err(|e| format!("{} has no {CMDLINE} on its ESP: {e}", path.display()))?;
    let mut text = vec![0u8; usize::try_from(file.len()).unwrap_or(usize::MAX)];
    fs.read(&mut file, 0, &mut text)
        .map_err(|e| format!("reading {CMDLINE} from {}: {e}", path.display()))?;
    let text = String::from_utf8(text)
        .map_err(|e| format!("{CMDLINE} on {} is not text: {e}", path.display()))?;
    Ok(if text.is_empty() {
        Vec::new()
    } else {
        text.split(',').map(str::to_string).collect()
    })
}

/// A raw block device rejects a write that is not a whole number of sectors, so
/// an image whose length is not sector-aligned cannot be `dd`'d to a USB stick —
/// the final partial write fails with `EINVAL` and the tail, including the
/// backup GPT, never lands. QEMU reads the image as a file and never noticed.
const SECTOR: usize = 4096;

fn round_up_sectors(n: usize) -> usize {
    n.div_ceil(SECTOR) * SECTOR
}

/// Where each partition is made to start.
///
/// A correctness requirement rather than tidiness. The kernel's `BlockDevice`
/// transfers whole 4 KiB blocks and each mounted volume keeps its own resident
/// copies of the blocks it has touched (`fat32_adapter::FatDevice`); two
/// partitions sharing one device block would make each other's copies stale
/// with nothing able to notice. Unaligned, the ESP ended 1024 bytes into a
/// device block that the log partition then began in.
///
/// 1 MiB rather than the 4096 the kernel needs, because that is what every
/// partitioner uses and what a flash device's erase block wants.
const PARTITION_ALIGN: usize = 1024 * 1024;

/// The smallest volume there is a FAT32 for.
///
/// FAT32 *is* the format with at least 65,525 clusters, and `fatfs` gives a
/// volume this size 512-byte clusters — so the data area alone is 33.5 MiB
/// before the two FATs and the reserved sectors. Measured at exactly this size:
/// the format succeeds and `fsck_msdos` reports 68,551 free clusters.
const FAT32_MIN_BYTES: usize = 34 * 1024 * 1024;

/// What a guest can write into `/boot` after the three files are on it.
///
/// The `64/63` beside this at the one call site is headroom for the
/// filesystem's own metadata, and it is why this cannot be the flat slack it
/// used to be. `fatfs` picks the cluster size from the volume size, and at the
/// 512-byte clusters it gives the smaller ones the two FAT copies cost one
/// byte of table per 64 bytes of volume — 4.1 MiB of the test image's 257 MiB,
/// more than the 4 MiB the size used to add. So the slack was entirely
/// metadata, and what a guest could write was whatever rounding happened to
/// leave: measured before the change at **48,640 bytes**, against `esp_files`'
/// own 41,097-byte blob. One more guest test binary in the initrd took it
/// negative, and the symptom is an fsync that fails while the host-side volume
/// still reports megabytes free.
///
/// `64/63` is the 512-byte-cluster worst case, so where `fatfs` picks larger
/// clusters it over-provisions rather than under: the same image measures
/// **7,901,184 bytes** free after the change, on 4096-byte clusters.
const ESP_FREE_BYTES: usize = 4 * 1024 * 1024;

fn align_up(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

/// A FAT volume label: eleven bytes of space-padded OEM text.
///
/// Without one every host calls the volume `NO NAME`, which is what the ESP
/// showed as. `format_volume` writes it into both places the format keeps it,
/// the BPB field and a `VOLUME_ID` entry in the root directory, and the mount
/// on macOS is named from it — measured, `/Volumes/TOYOS-LOG`.
fn fat_label(text: &str) -> [u8; 11] {
    let mut label = [b' '; 11];
    assert!(
        text.len() <= label.len(),
        "a FAT volume label is 11 bytes and {text:?} is {}",
        text.len()
    );
    label[..text.len()].copy_from_slice(text.as_bytes());
    label
}

/// An empty FAT32 volume of `bytes`, under `label`.
fn format_fat32(bytes: usize, label: &str) -> Vec<u8> {
    let mut volume = vec![0u8; bytes];
    fatfs::format_volume(
        Cursor::new(&mut volume),
        fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .volume_label(fat_label(label)),
    )
    .unwrap_or_else(|e| panic!("failed to format the {label} volume: {e}"));
    volume
}

/// The volume being built, as [`toyos_fat32`] sees it.
///
/// Byte-addressed with no block size of its own, because there is none: the
/// volume is a `Vec` in this process. The kernel's adapter is where the
/// read-modify-write against 4 KiB device blocks lives, and `toyos-fat32`'s own
/// host suite is where that shape is exercised.
struct VolumeIo<'a>(&'a mut [u8]);

impl VolumeIo<'_> {
    fn range(&self, offset: u64, len: usize) -> Result<core::ops::Range<usize>, IoError> {
        let start = usize::try_from(offset).map_err(|_| IoError::Device)?;
        let end = start.checked_add(len).ok_or(IoError::Device)?;
        if end > self.0.len() {
            return Err(IoError::Device);
        }
        Ok(start..end)
    }
}

impl BlockAccess for VolumeIo<'_> {
    fn capacity(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let at = self.range(offset, buf.len())?;
        buf.copy_from_slice(&self.0[at]);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        let at = self.range(offset, buf.len())?;
        self.0[at].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

/// When the build ran, which is what the files on the stick are dated.
///
/// A host with a clock behind 1980 or past 2107 gets FAT's nearest
/// representable instant rather than a wrapped one; `FatTime` clamps and says
/// so.
fn build_time() -> FatTime {
    FatTime::from_unix_secs(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
    )
}

/// Write `files` onto a formatted volume, creating the directories they name,
/// and leave the free-cluster count recorded.
///
/// Written with **our** FAT32 driver rather than with the crate that formatted
/// the volume, and that is the whole of the fix for a defect every image this
/// project ever produced carried: `fatfs`'s `create_dir` writes a long-name
/// entry ahead of each `.` and `..`, which the format requires to be a
/// subdirectory's first two entries, and gives `..` the root's cluster number
/// where the format requires zero. Twelve `fsck_msdos` complaints, present
/// before any guest ran, and neither reachable through that crate's API.
///
/// Using `toyos-fat32` is not a way round them. It deletes the second FAT32
/// writer from the project: this is the one the kernel appends `kernel.log`
/// with, the one `toyos-fat32`'s host suite runs the volume checker and a real
/// macOS mount against on every `cargo test` in that crate, and now the one the
/// claim "the image we build is clean" is a claim about. `fatfs` keeps the
/// format call, where it has never had a complaint against it — an empty volume
/// has no subdirectory for either bug to live in.
///
/// The free-cluster count is the third thing the checker asks for:
/// `format_volume` leaves FSInfo's field 0xFFFFFFFF, which FAT32 defines as
/// "unknown" and every host then reports this volume's free space from.
/// `free_bytes` counts the FAT when the volume arrived without a hint and
/// `sync` writes it.
fn populate(volume: &mut [u8], label: &str, files: &[(&str, &[u8])]) {
    let time = build_time();
    let mut fs = Fat32::mount(VolumeIo(volume))
        .unwrap_or_else(|e| panic!("the freshly formatted {label} volume does not mount: {e}"));
    for (path, data) in files {
        if let Some((dir, _)) = path.rsplit_once('/') {
            fs.create_dir_all(dir, time)
                .unwrap_or_else(|e| panic!("creating {dir}/ on {label}: {e}"));
        }
        let mut file = fs
            .create(path, time)
            .unwrap_or_else(|e| panic!("creating {path} on {label}: {e}"));
        fs.write(&mut file, 0, data)
            .unwrap_or_else(|e| panic!("writing {} bytes to {path} on {label}: {e}", data.len()));
        fs.flush_meta(&mut file, time)
            .unwrap_or_else(|e| panic!("recording {path} on {label}: {e}"));
    }
    fs.free_bytes()
        .unwrap_or_else(|e| panic!("counting the {label} volume's free clusters: {e}"));
    fs.sync().unwrap_or_else(|e| panic!("syncing the {label} volume: {e}"));
}

/// The partition firmware boots from: the bootloader, the kernel, the initrd,
/// and the name of the partition the kernel's log goes on.
fn create_esp_volume(
    kernel: &[u8],
    bootloader: &[u8],
    initrd: &[u8],
    log_guid: uuid::Uuid,
    cmdline: &str,
) -> Vec<u8> {
    let content_size = kernel.len() + bootloader.len() + initrd.len();
    let total_size = round_up_sectors(
        ((content_size + ESP_FREE_BYTES) * 64 / 63).max(FAT32_MIN_BYTES),
    );

    let mut volume = format_fat32(total_size, "TOYOS-BOOT");

    populate(
        &mut volume,
        "TOYOS-BOOT",
        &[
            ("EFI/BOOT/BOOTx64.EFI", bootloader),
            ("toyos/kernel.elf", kernel),
            ("toyos/initrd.img", initrd),
            // Mirrored in `bootloader/src/main.rs` as `\toyos\log.guid`, which
            // reads it beside the two files above and refuses the volume if it
            // is not there. The sixteen bytes are the GPT entry's own, in the
            // entry's own order: nothing converts them on the way to the kernel
            // and nothing converts the table's, so the comparison that decides
            // which partition holds the log cannot be got backwards.
            ("toyos/log.guid", &log_guid.to_bytes_le()),
            // The actuators this boot arms, comma-separated and empty on every
            // image anyone ships. Read by the bootloader beside the three above
            // and handed to the kernel in `KernelArgs`, because the earliest
            // actuator fires before `mm::init` and there is nowhere later to
            // fetch it from. `kernel/src/actuator.rs` is the list, and
            // [`params_of`] is how the host asks a finished image which of them
            // a guest booting it would arm.
            (CMDLINE, cmdline.as_bytes()),
        ],
    );

    volume
}

/// The partition the kernel's log lives on, empty until a machine boots.
///
/// Exactly [`FAT32_MIN_BYTES`], because the floor is not ours to choose and the
/// log cannot use much of it: sixteen boots at `/bin/logd`'s `MAX_LOG_BYTES`
/// come to 16 MiB, under half of what this volume has free, and there is no
/// smaller FAT32 to cut it down to.
fn create_log_volume() -> Vec<u8> {
    let mut volume = format_fat32(FAT32_MIN_BYTES, "TOYOS-LOG");
    populate(&mut volume, "TOYOS-LOG", &[]);
    volume
}

fn create_gpt_disk(esp_volume: Vec<u8>, log_volume: Vec<u8>, log_guid: uuid::Uuid) -> Vec<u8> {
    // `add_partition` places each partition itself; this is the size the disk
    // has to be for it to have somewhere to put them — an aligned gap before
    // the ESP, an aligned gap between the two, and one after the log partition
    // for the backup table.
    let log_at = align_up(PARTITION_ALIGN + esp_volume.len(), PARTITION_ALIGN);
    let total_size = round_up_sectors(log_at + log_volume.len() + PARTITION_ALIGN);
    assert_eq!(total_size % 512, 0, "image must be a whole number of 512-byte sectors to be flashable");
    let mut disk = vec![0u8; total_size];

    let mut cursor = Cursor::new(&mut disk);

    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
        u32::try_from((total_size / 512) - 1).unwrap_or(0xFF_FF_FF_FF),
    );
    mbr.overwrite_lba0(&mut cursor).expect("failed to write MBR");

    let mut gdisk = gpt::GptConfig::default()
        .initialized(false)
        .writable(true)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .create_from_device(Box::new(cursor), None)
        .expect("failed to create GPT disk");

    gdisk
        .update_partitions(BTreeMap::<u32, gpt::partition::Partition>::new())
        .expect("failed to initialize partition table");

    let align = Some((PARTITION_ALIGN / 512) as u64);
    let esp_id = gdisk
        .add_partition("EFI System", esp_volume.len() as u64, gpt::partition_types::EFI, 0, align)
        .expect("failed to add ESP partition");
    // Microsoft Basic Data, and that type is the whole reason this is a second
    // partition at all: macOS never auto-mounts an EFI-typed partition and this
    // host refuses even a manual non-root mount of one, so a log on the ESP is
    // unreachable without the admin account. This type mounts in Finder, in
    // Windows and in Linux on plug-in, with nothing configured.
    let log_id = gdisk
        .add_partition("ToyOS log", log_volume.len() as u64, gpt::partition_types::BASIC, 0, align)
        .expect("failed to add the log partition");

    // The GUID `add_partition` drew for the log partition is discarded for the
    // one already written to the ESP. Both name the same partition and only one
    // of them can be chosen second.
    let mut table = gdisk.partitions().clone();
    table
        .get_mut(&log_id)
        .expect("the log partition was just added")
        .part_guid = log_guid;
    gdisk
        .update_partitions(table)
        .expect("failed to stamp the log partition's unique GUID");

    let start_of = |id: u32| {
        gdisk
            .partitions()
            .get(&id)
            .expect("a partition that was just added")
            .bytes_start(gpt::disk::LogicalBlockSize::Lb512)
            .expect("failed to get a partition's start") as usize
    };
    let esp_start = start_of(esp_id);
    let log_start = start_of(log_id);

    // The invariant [`PARTITION_ALIGN`] exists for, checked rather than
    // assumed: the kernel mounts both of these at once over one 4 KiB block
    // device, and a device block belonging to both volumes would be cached
    // twice.
    for (what, start, len) in
        [("ESP", esp_start, esp_volume.len()), ("log partition", log_start, log_volume.len())]
    {
        assert_eq!(start % SECTOR, 0, "the {what} starts at byte {start}, off a {SECTOR}-byte block");
        assert_eq!(len % SECTOR, 0, "the {what} is {len} bytes, not whole {SECTOR}-byte blocks");
    }
    assert!(
        esp_start + esp_volume.len() <= log_start,
        "the ESP runs to {} and the log partition starts at {log_start}",
        esp_start + esp_volume.len()
    );

    let mut disk_device = gdisk.write().expect("failed to write GPT");

    disk_device.seek(std::io::SeekFrom::Start(0)).expect("failed to seek");
    let mut final_bytes = vec![0u8; total_size];
    disk_device.read_exact(&mut final_bytes).expect("failed to read disk");

    final_bytes[esp_start..esp_start + esp_volume.len()].copy_from_slice(&esp_volume);
    final_bytes[log_start..log_start + log_volume.len()].copy_from_slice(&log_volume);

    final_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two volumes this build writes break no rule of the format, and the
    /// gate is silence rather than sameness.
    ///
    /// The ESP did break some, from the first image this project ever built
    /// until [`populate`] stopped writing it with `fatfs` — twelve complaints,
    /// before any guest ran, from two format violations that crate's
    /// `create_dir` has. The consequence was not only a dirty volume:
    /// `esp_filesystem` could only ask that the guest add no *new* complaint,
    /// so a complaint the guest produced for its own reason would have hidden
    /// inside the twelve.
    ///
    /// Here rather than in the boot suite because it needs no guest, no QEMU
    /// and no kernel: it is a claim about the writer, and it fails in seconds
    /// on `cargo test --lib`.
    #[test]
    fn the_volumes_this_build_writes_break_no_format_rule() {
        for (what, volume) in [
            ("ESP", create_esp_volume(b"kernel", b"bootloader", b"initrd", uuid::Uuid::new_v4(), "")),
            ("log volume", create_log_volume()),
        ] {
            let complaints = toyos_fat32_check::check(&volume);
            assert!(
                complaints.is_empty(),
                "the {what} this build writes is not a clean FAT32 volume:\n{}",
                toyos_fat32_check::describe(&complaints)
            );
        }
    }

    /// And it is clean because it is right, not because it is empty: a
    /// `populate` that wrote nothing at all would satisfy the gate above.
    #[test]
    fn the_esp_carries_what_the_bootloader_looks_for() {
        let mut esp =
            create_esp_volume(b"kernel", b"bootloader", b"initrd", uuid::Uuid::new_v4(), "");
        let mut fs = Fat32::mount(VolumeIo(&mut esp)).expect("mount the ESP we just built");
        let found: Vec<String> =
            fs.walk(64).expect("walk the ESP").into_iter().map(|(path, _)| path).collect();
        for want in [
            "EFI/BOOT/BOOTx64.EFI",
            "toyos/kernel.elf",
            "toyos/initrd.img",
            "toyos/log.guid",
            "toyos/cmdline",
        ]
        {
            assert!(found.iter().any(|p| p.trim_start_matches('/') == want), "{want} is not on the ESP; it holds {found:?}");
        }
    }

    /// An image says which actuators a guest booting it would arm, and the
    /// answer comes out of the image rather than from whoever built it.
    ///
    /// **This is what makes a staged boot image answerable.** The actuators are
    /// baked in at build time, so a caller that supplies its own image cannot
    /// arm anything by asking — and until [`param_conflict`] existed nothing
    /// refused the pair: the harness took the parameters, built a kernel for
    /// them, booted the supplied image instead, and reported the arm as taken.
    /// A green run with an inert arm is the worst kind of harness defect,
    /// because every negative control staged through one proves nothing.
    ///
    /// Both directions, over images this file's own writer produced: the reader
    /// is the writer's inverse and a round trip is the only thing that says so.
    #[test]
    fn an_image_says_what_it_is_armed_with() {
        let dir = std::env::temp_dir().join(format!("toyos-image-params-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let write = |name: &str, cmdline: &str| {
            let path = dir.join(name);
            std::fs::write(&path, create_boot_image(b"kernel", b"bootloader", b"initrd", cmdline))
                .expect("write an image");
            path
        };

        // What every shipping image is: nothing armed at all.
        let shipping = write("shipping.img", "");
        // Two, because a reader that handed back the whole file as one name
        // would answer every one-actuator question correctly.
        let armed = write("armed.img", "usb-flush-fails,fat-boot-reads-fail");

        for (image, asked) in [
            (&shipping, &[][..]),
            (&armed, &["usb-flush-fails", "fat-boot-reads-fail"][..]),
        ] {
            assert_eq!(
                param_conflict(image, asked),
                None,
                "an image was refused the list it was built with: {asked:?}"
            );
        }

        // The recorded defect, 2026-08-22: an actuator armed beside an image
        // built without it. It has to name both sides — the reader gets the
        // message and nothing else.
        for (image, asked, name) in [
            (&shipping, &["usb-flush-fails"][..], "usb-flush-fails"),
            (&armed, &[][..], "fat-boot-reads-fail"),
            (&armed, &["usb-flush-fails"][..], "fat-boot-reads-fail"),
        ] {
            let why = param_conflict(image, asked)
                .unwrap_or_else(|| panic!("{asked:?} was accepted on {}", image.display()));
            assert!(
                why.contains(name),
                "the refusal does not name {name}, which is the whole of what it is about: {why}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
