use fatfs::FsOptions;
use std::fs;
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

pub fn create_boot_image(initrd_bytes: &[u8], profile: &str) -> Vec<u8> {
    let kernel_bytes = fs::read(format!("kernel/target/x86_64-unknown-none/{profile}/kernel"))
        .expect("Failed to read kernel");
    let bl_bytes = fs::read(format!("bootloader/target/x86_64-unknown-uefi/{profile}/bootloader.efi"))
        .expect("Failed to read bootloader");

    let esp_volume = create_fat_volume(&kernel_bytes, &bl_bytes, initrd_bytes);
    create_gpt_disk(esp_volume)
}

pub fn create_fat_volume(kernel: &[u8], bootloader: &[u8], initrd: &[u8]) -> Vec<u8> {
    let content_size = kernel.len() + bootloader.len() + initrd.len();
    // FAT32 requires at least 65525 clusters; ensure volume is large enough
    let total_size = (content_size + 4 * 1024 * 1024).max(34 * 1024 * 1024);

    let mut volume = vec![0u8; total_size];

    fatfs::format_volume(
        Cursor::new(&mut volume),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32),
    )
    .expect("Failed to format FAT volume");

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
    }

    volume
}

pub fn create_gpt_disk(esp_volume: Vec<u8>) -> Vec<u8> {
    let overhead = 100 * 1024; // 100 KiB for GPT headers
    let total_size = esp_volume.len() + overhead;
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
        .update_partitions(std::collections::BTreeMap::<u32, gpt::partition::Partition>::new())
        .expect("failed to initialize partition table");

    let esp_id = gdisk
        .add_partition("EFI System", esp_volume.len() as u64, gpt::partition_types::EFI, 0, None)
        .expect("failed to add ESP partition");

    let esp_start = gdisk.partitions().get(&esp_id).unwrap()
        .bytes_start(gpt::disk::LogicalBlockSize::Lb512)
        .expect("failed to get ESP start") as usize;

    let mut disk_device = gdisk.write().expect("failed to write GPT");

    disk_device.seek(std::io::SeekFrom::Start(0)).expect("failed to seek");
    let mut final_bytes = vec![0u8; total_size];
    disk_device.read_exact(&mut final_bytes).expect("failed to read disk");

    final_bytes[esp_start..esp_start + esp_volume.len()].copy_from_slice(&esp_volume);

    final_bytes
}
