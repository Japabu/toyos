use fatfs::FsOptions;
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

use bcachefs::{Formatted, VecBlockIO};

pub fn create_initrd(
    files: &[(String, Vec<u8>)],
    symlinks: &[(String, String)],
    quiet: bool,
) -> Vec<u8> {
    let data_size: usize = files.iter().map(|(_, d)| d.len()).sum::<usize>();
    let total_entries = files.len() + symlinks.len();
    // Estimate: superblock(1) + bitmap + btree nodes + data blocks + backup(1) + 10% padding
    let data_blocks = (data_size + 4095) / 4096;
    let btree_blocks = (total_entries / 30).max(2);
    let overhead = 64;
    let total_blocks = (1 + overhead + btree_blocks + data_blocks) * 11 / 10;
    let total_blocks = total_blocks.max(64) as u64;

    let io = VecBlockIO::new(total_blocks);
    let mut fs = Formatted::format(io);

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

    fs.into_io().into_vec()
}

/// Takes the artifacts as bytes rather than reading them: the caller stages them
/// under a build-key-derived name first, because cargo's own path is shared by
/// every config and is overwritten by any concurrent build (see `build.rs`).
pub fn create_boot_image(kernel_bytes: &[u8], bl_bytes: &[u8], initrd_bytes: &[u8]) -> Vec<u8> {
    // Drawn here and written twice: into the GPT entry that *is* the log
    // partition, and into a file on the ESP that the bootloader hands the
    // kernel. The kernel is given the partition by name; nothing anywhere goes
    // looking for one by type or by format.
    let log_guid = uuid::Uuid::new_v4();
    let esp_volume = create_esp_volume(kernel_bytes, bl_bytes, initrd_bytes, log_guid);
    let log_volume = create_log_volume();
    create_gpt_disk(esp_volume, log_volume, log_guid)
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

/// Put the volume's free-cluster count in its FSInfo sector.
///
/// `format_volume` leaves it 0xFFFFFFFF, which FAT32 defines as "unknown" and
/// which `fsck_msdos` reports as `Free space in FSInfo block is unset`. Counting
/// it here is what makes the log volume *born* clean rather than clean apart
/// from one complaint, and `toyos-fat32` maintains the count from there — it
/// only tracks a free count it was given one to track (`fat.rs`'s
/// `free_count.map(...)`).
///
/// `stats` marks FSInfo dirty and `FileSystem`'s drop flushes it, so this is
/// the last thing done to an open volume and nothing reads its return.
fn record_free_clusters<IO: fatfs::ReadWriteSeek>(fat: &fatfs::FileSystem<IO>, label: &str) {
    fat.stats()
        .unwrap_or_else(|e| panic!("failed to count the {label} volume's free clusters: {e}"));
}

/// The partition firmware boots from: the bootloader, the kernel, the initrd,
/// and the name of the partition the kernel's log goes on.
fn create_esp_volume(
    kernel: &[u8],
    bootloader: &[u8],
    initrd: &[u8],
    log_guid: uuid::Uuid,
) -> Vec<u8> {
    let content_size = kernel.len() + bootloader.len() + initrd.len();
    let total_size = round_up_sectors((content_size + 4 * 1024 * 1024).max(FAT32_MIN_BYTES));

    let mut volume = format_fat32(total_size, "TOYOS-BOOT");

    {
        let fat = fatfs::FileSystem::new(Cursor::new(&mut volume), FsOptions::new())
            .expect("Failed to open FAT filesystem");

        fat.root_dir()
            .create_dir("EFI")
            .expect("Failed to create EFI directory")
            .create_dir("BOOT")
            .expect("Failed to create BOOT directory")
            .create_file("BOOTx64.EFI")
            .expect("Failed to create BOOTx64.EFI")
            .write_all(bootloader)
            .expect("Failed to write bootloader");

        let toyos_dir = fat.root_dir()
            .create_dir("toyos")
            .expect("Failed to create toyos directory");
        toyos_dir
            .create_file("kernel.elf")
            .expect("Failed to create kernel.elf")
            .write_all(kernel)
            .expect("Failed to write kernel");
        toyos_dir
            .create_file("initrd.img")
            .expect("Failed to create initrd.img")
            .write_all(initrd)
            .expect("Failed to write initrd");
        // Mirrored in `bootloader/src/main.rs` as `\toyos\log.guid`, which
        // reads it beside the two files above and refuses the volume if it is
        // not there. The sixteen bytes are the GPT entry's own, in the entry's
        // own order: nothing converts them on the way to the kernel and nothing
        // converts the table's, so the comparison that decides which partition
        // holds the log cannot be got backwards.
        toyos_dir
            .create_file("log.guid")
            .expect("Failed to create log.guid")
            .write_all(&log_guid.to_bytes_le())
            .expect("Failed to write log.guid");

        record_free_clusters(&fat, "TOYOS-BOOT");
    }

    volume
}

/// The partition the kernel's log lives on, empty until a machine boots.
///
/// Exactly [`FAT32_MIN_BYTES`], because the floor is not ours to choose and the
/// log cannot use much of it: two generations of `kernel.log` come to 8 MiB at
/// `log_file::MAX_LOG_BYTES`, under a quarter of what this volume has free, and
/// there is no smaller FAT32 to cut it down to.
fn create_log_volume() -> Vec<u8> {
    let mut volume = format_fat32(FAT32_MIN_BYTES, "TOYOS-LOG");
    {
        let fat = fatfs::FileSystem::new(Cursor::new(&mut volume), FsOptions::new())
            .expect("Failed to open the log volume");
        record_free_clusters(&fat, "TOYOS-LOG");
    }
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
