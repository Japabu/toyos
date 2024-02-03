use fatfs::FsOptions;
use std::fs;
use std::io::{Cursor, Write};
use std::process::Command;

fn create_fat_fs_with_bl_and_kernel() -> Vec<u8> {
    let kernel_path = "../kernel/target/x86_64-unknown-none/release/kernel";
    let bl_path = "../bootloader/target/x86_64-unknown-uefi/release/bootloader.efi";

    let kernel_bytes = fs::read(kernel_path).expect("Failed to read kernel");
    let bl_bytes = fs::read(bl_path).expect("Failed to read bootloader");

    let fatfs_overhead_estimate = 100 * 1024; // 100 KiB
    let total_size = kernel_bytes.len() + bl_bytes.len() + fatfs_overhead_estimate;

    let mut volume_bytes = Vec::new();
    volume_bytes.resize(total_size, 0);

    // Format as FAT32
    fatfs::format_volume(
        Cursor::new(&mut volume_bytes),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32),
    )
    .expect("Failed to format volume");

    {
        // Create the filesystem
        let fat = fatfs::FileSystem::new(Cursor::new(&mut volume_bytes), FsOptions::new())
            .expect("Failed to create filesystem");

        // Copy the kernel and bootloader to the filesystem
        fat.root_dir()
            .create_dir("EFI")
            .expect("Failed to create EFI directory")
            .create_dir("BOOT")
            .expect("Failed to create boot directory")
            .create_file("BOOTx64.EFI")
            .expect("Failed to create bootx64.efi")
            .write_all(&bl_bytes)
            .expect("Failed to write bootloader");
        fat.root_dir()
            .create_dir("toyos")
            .expect("Failed to create toyos directory")
            .create_file("kernel.elf")
            .expect("Failed to create kernel.elf")
            .write_all(&kernel_bytes)
            .expect("Failed to write kernel");
    }

    volume_bytes
}

fn create_gpt_disk_with_esp_partition(esp_volume_bytes: Vec<u8>) -> Vec<u8> {
    let overhead_estimate = 100 * 1024; // 100 KiB
    let total_size = esp_volume_bytes.len() + overhead_estimate;
    let mut disk_bytes = Vec::new();
    disk_bytes.resize(total_size, 0);

    let mut cursor = Cursor::new(&mut disk_bytes);

    // Create a protective MBR at LBA0
    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
        u32::try_from((total_size / 512) - 1).unwrap_or(0xFF_FF_FF_FF),
    );
    mbr.overwrite_lba0(&mut cursor)
        .expect("failed to write MBR");

    let mut gdisk = gpt::GptConfig::default()
        .initialized(false)
        .writable(true)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .create_from_device(Box::new(cursor), None)
        .expect("failed to create GptDisk");

    // Initialize the headers using a blank partition table
    gdisk
        .update_partitions(std::collections::BTreeMap::<u32, gpt::partition::Partition>::new())
        .expect("failed to initialize blank partition table");

    // Add EFI system partition
    let esp_partition_id = gdisk
        .add_partition(
            "EFI System",
            esp_volume_bytes.len() as u64,
            gpt::partition_types::EFI,
            0,
            None,
        )
        .expect("failed to add EFI System partition");

    let esp_partition = gdisk
        .partitions()
        .get(&esp_partition_id)
        .expect("failed to get ESP partition");

    let esp_partition_start = esp_partition
        .bytes_start(gpt::disk::LogicalBlockSize::Lb512)
        .expect("failed to get ESP partition start") as usize;

    let esp_partition_len = esp_partition
        .bytes_len(gpt::disk::LogicalBlockSize::Lb512)
        .expect("failed to get ESP partition length") as usize;

    assert!(esp_partition_len >= esp_volume_bytes.len());

    // Write the partition table and take ownership of
    // the underlying memory buffer-backed block device
    let mut disk_bytes = gdisk.write().expect("failed to write partition table");

    // Read the written bytes out of the memory buffer device
    disk_bytes
        .seek(std::io::SeekFrom::Start(0))
        .expect("failed to seek");
    let mut final_bytes = vec![0u8; total_size];
    disk_bytes
        .read_exact(&mut final_bytes)
        .expect("failed to read contents of memory device");

    final_bytes[esp_partition_start..esp_partition_start + esp_volume_bytes.len()]
        .copy_from_slice(&esp_volume_bytes);

    final_bytes
}

fn main() -> std::io::Result<()> {
    // Build the kernel and bootloader projects
    println!("cargo:rerun-if-changed=./src/");
    println!("cargo:rerun-if-changed=../bootloader/src/");
    println!("cargo:rerun-if-changed=../kernel/src/");
    Command::new("cargo")
        .args(&["build", "--release"])
        .current_dir("../kernel")
        .status()
        .expect("Failed to build kernel");
    Command::new("cargo")
        .args(&["build", "--release"])
        .current_dir("../bootloader")
        .status()
        .expect("Failed to build bootloader");

    let volume_bytes = create_fat_fs_with_bl_and_kernel();
    let disk_bytes = create_gpt_disk_with_esp_partition(volume_bytes);

    fs::write("target/bootable.img", disk_bytes).expect("Failed to write image");

    Ok(())
}
