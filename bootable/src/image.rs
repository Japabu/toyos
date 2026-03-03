use fatfs::FsOptions;
use std::fs;
use std::io::{Cursor, Read, Write};
use tyfs::SimpleFs;

struct VecDisk {
    data: Vec<u8>,
}

impl VecDisk {
    fn new(size: usize) -> Self {
        Self { data: vec![0u8; size] }
    }
}

impl tyfs::Disk for VecDisk {
    fn read(&mut self, offset: u64, buf: &mut [u8]) {
        let off = offset as usize;
        buf.copy_from_slice(&self.data[off..off + buf.len()]);
    }
    fn write(&mut self, offset: u64, buf: &[u8]) {
        let off = offset as usize;
        self.data[off..off + buf.len()].copy_from_slice(buf);
    }
    fn flush(&mut self) {}
}

pub fn create_initrd(files: &[(String, Vec<u8>)], symlinks: &[(String, String)]) -> Vec<u8> {
    let data_size: usize = files.iter().map(|(name, d)| name.len() + d.len()).sum::<usize>()
        + symlinks.iter().map(|(name, target)| name.len() + target.len()).sum::<usize>();
    let toc_size = (files.len() + symlinks.len()) * 64;
    let size = (64 + data_size + toc_size + 4095) & !4095;
    let size = size.max(4096);

    let vec_disk = VecDisk::new(size);
    let mut tyfs = SimpleFs::format(vec_disk, size as u64);

    for (name, data) in files {
        eprintln!("initrd: adding '{}' ({} bytes)", name, data.len());
        if !tyfs.create(name, data, 0) {
            panic!("Failed to add '{}' to initrd image", name);
        }
    }

    for (name, target) in symlinks {
        eprintln!("initrd: symlink '{}' -> '{}'", name, target);
        if !tyfs.create_symlink(name, target) {
            panic!("Failed to add symlink '{}' -> '{}' to initrd image", name, target);
        }
    }

    tyfs.into_disk().data
}

pub fn create_boot_image(initrd_bytes: &[u8]) -> Vec<u8> {
    let kernel_bytes = fs::read("../kernel/target/x86_64-unknown-none/debug/kernel")
        .expect("Failed to read kernel");
    let bl_bytes = fs::read("../bootloader/target/x86_64-unknown-uefi/debug/bootloader.efi")
        .expect("Failed to read bootloader");

    let esp_volume = create_fat_volume(&kernel_bytes, &bl_bytes, initrd_bytes);
    create_gpt_disk(esp_volume)
}

fn create_fat_volume(kernel: &[u8], bootloader: &[u8], initrd: &[u8]) -> Vec<u8> {
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

fn create_gpt_disk(esp_volume: Vec<u8>) -> Vec<u8> {
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
