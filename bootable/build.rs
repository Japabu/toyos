use fatfs::FsOptions;
use fontdue::{Font, FontSettings};
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use std::process::Command;
use tyfs::{Disk, SimpleFs, VecDisk};

const FONT_WIDTH: usize = 8;
const FONT_HEIGHT: usize = 16;
const FONT_GLYPHS: usize = 256;

fn generate_font_bitmap(rootfs_dir: &str) {
    let font_bytes = include_bytes!("assets/JetBrainsMono-Regular.ttf");
    let font = Font::from_bytes(font_bytes as &[u8], FontSettings::default())
        .expect("Failed to parse font");

    // Find a px_size where the line height fits in FONT_HEIGHT.
    // fontdue rasterizes individual glyphs; we pick a size where ascent+descent <= 16.
    let px_size = 14.0f32;
    // Use the font's line metrics to position glyphs consistently.
    let line_metrics = font.horizontal_line_metrics(px_size).unwrap();
    let ascent = line_metrics.ascent.ceil() as i32;

    let mut output = vec![0u8; FONT_GLYPHS * FONT_HEIGHT];

    for ch in 0..FONT_GLYPHS {
        let c = ch as u8 as char;
        let (metrics, bitmap) = font.rasterize(c, px_size);

        // Position glyph in the 8x16 cell
        // x offset: center horizontally if narrower than 8
        let x_offset = if metrics.width < FONT_WIDTH {
            ((FONT_WIDTH as i32 - metrics.width as i32) / 2).max(0) as usize
        } else {
            0
        };
        // y offset: baseline is at `ascent` pixels from top, glyph top is baseline - ymin
        let glyph_top = ascent - metrics.height as i32 - metrics.ymin;
        let y_offset = glyph_top.max(0) as usize;

        let glyph_base = ch * FONT_HEIGHT;

        for gy in 0..metrics.height {
            let cell_y = y_offset + gy;
            if cell_y >= FONT_HEIGHT {
                break;
            }
            let mut row_byte: u8 = 0;
            for gx in 0..metrics.width {
                let cell_x = x_offset + gx;
                if cell_x >= FONT_WIDTH {
                    break;
                }
                let pixel = bitmap[gy * metrics.width + gx];
                if pixel > 100 {
                    row_byte |= 0x80 >> cell_x;
                }
            }
            output[glyph_base + cell_y] |= row_byte;
        }
    }

    let font_path = Path::new(rootfs_dir).join("font8x16.bin");
    fs::write(&font_path, &output).expect("Failed to write font bitmap");
    eprintln!(
        "rootfs: generated font8x16.bin ({} bytes) from JetBrains Mono",
        output.len()
    );
}

fn create_rootfs_image(rootfs_dir: &str) -> Vec<u8> {
    let rootfs = Path::new(rootfs_dir);
    assert!(rootfs.is_dir(), "rootfs directory not found: {}", rootfs_dir);

    // Collect files and calculate needed size
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in fs::read_dir(rootfs).expect("Failed to read rootfs") {
        let entry = entry.expect("Failed to read dir entry");
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            let data = fs::read(&path).expect("Failed to read file");
            eprintln!("rootfs: adding '{}' ({} bytes)", name, data.len());
            files.push((name, data));
        }
    }

    // Size: header(64) + file data + toc entries(64 each) + padding
    let data_size: usize = files.iter().map(|(_, d)| d.len()).sum();
    let toc_size = files.len() * 64;
    let size = (64 + data_size + toc_size + 4095) & !4095; // round up to 4K
    let size = size.max(4096);

    let vec_disk = VecDisk::new(size, 512);
    let mut tyfs = SimpleFs::format(Disk::new(vec_disk), size as u64);

    for (name, data) in &files {
        if !tyfs.create(name, data) {
            panic!("Failed to add '{}' to rootfs image", name);
        }
    }

    tyfs.into_disk().into_inner().data
}

fn create_fat_fs_with_bl_and_kernel(rootfs_bytes: &[u8]) -> Vec<u8> {
    let kernel_path = "../kernel/target/x86_64-unknown-none/debug/kernel";
    let bl_path = "../bootloader/target/x86_64-unknown-uefi/debug/bootloader.efi";

    let kernel_bytes = fs::read(kernel_path).expect("Failed to read kernel");
    let bl_bytes = fs::read(bl_path).expect("Failed to read bootloader");

    let content_size = kernel_bytes.len() + bl_bytes.len() + rootfs_bytes.len();
    // FAT32 requires at least 65525 clusters; ensure volume is large enough
    let total_size = (content_size + 1024 * 1024).max(34 * 1024 * 1024);

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
        let toyos_dir = fat.root_dir()
            .create_dir("toyos")
            .expect("Failed to create toyos directory");
        toyos_dir
            .create_file("kernel.elf")
            .expect("Failed to create kernel.elf")
            .write_all(&kernel_bytes)
            .expect("Failed to write kernel");
        toyos_dir
            .create_file("rootfs.img")
            .expect("Failed to create rootfs.img")
            .write_all(rootfs_bytes)
            .expect("Failed to write rootfs");
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
    println!("cargo:rerun-if-changed=../rootfs/");
    println!("cargo:rerun-if-changed=./assets/");
    if !Command::new("cargo")
        .args(&["build"])
        .current_dir("../kernel")
        .status()?
        .success()
    {
        panic!("Failed to build kernel");
    }

    if !Command::new("cargo")
        .args(&["build"])
        .current_dir("../bootloader")
        .status()?
        .success()
    {
        panic!("Failed to build bootloader");
    }

    generate_font_bitmap("../rootfs");
    let rootfs_bytes = create_rootfs_image("../rootfs");
    let volume_bytes = create_fat_fs_with_bl_and_kernel(&rootfs_bytes);
    let disk_bytes = create_gpt_disk_with_esp_partition(volume_bytes);

    fs::write("target/bootable.img", disk_bytes).expect("Failed to write image");

    // Create empty NVMe disk image if it doesn't already exist (persistent across rebuilds)
    let nvme_path = "target/nvme.img";
    if !std::path::Path::new(nvme_path).exists() {
        let nvme_size = 128 * 1024 * 1024; // 128 MB
        let nvme_bytes = vec![0u8; nvme_size];
        fs::write(nvme_path, nvme_bytes).expect("Failed to write NVMe image");
    }

    Ok(())
}
