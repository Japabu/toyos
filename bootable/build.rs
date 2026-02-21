use fatfs::FsOptions;
use fontdue::{Font, FontSettings};
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use std::process::Command;
use tyfs::SimpleFs;

const FONT_WIDTH: usize = 8;
const FONT_HEIGHT: usize = 16;
const FONT_GLYPHS: usize = 256;

fn generate_font_bitmap(initrd_dir: &str) {
    let font_bytes = include_bytes!("assets/JetBrainsMono-Regular.ttf");
    let font = Font::from_bytes(font_bytes as &[u8], FontSettings::default())
        .expect("Failed to parse font");

    // Find the largest px_size where every glyph fits in the cell
    let mut px_size = FONT_HEIGHT as f32;
    loop {
        let lm = font.horizontal_line_metrics(px_size).unwrap();
        let asc = lm.ascent.ceil() as i32;
        let mut fits = true;
        for ch in 0..FONT_GLYPHS {
            let c = ch as u8 as char;
            let (m, _) = font.rasterize(c, px_size);
            let glyph_top = asc - m.height as i32 - m.ymin;
            if glyph_top < 0
                || (glyph_top as usize) + m.height > FONT_HEIGHT
                || m.width > FONT_WIDTH
            {
                fits = false;
                break;
            }
        }
        if fits {
            break;
        }
        px_size -= 0.25;
        assert!(px_size > 4.0, "Could not find a font size that fits in {}x{}", FONT_WIDTH, FONT_HEIGHT);
    }
    eprintln!("font: selected px_size={:.2} for {}x{} cell", px_size, FONT_WIDTH, FONT_HEIGHT);
    let line_metrics = font.horizontal_line_metrics(px_size).unwrap();
    let ascent = line_metrics.ascent.ceil() as i32;

    let mut output = vec![0u8; FONT_GLYPHS * FONT_WIDTH * FONT_HEIGHT];

    for ch in 0..FONT_GLYPHS {
        let c = ch as u8 as char;
        let (metrics, bitmap) = font.rasterize(c, px_size);

        let x_offset = if metrics.width < FONT_WIDTH {
            ((FONT_WIDTH as i32 - metrics.width as i32) / 2).max(0) as usize
        } else {
            0
        };
        let glyph_top = ascent - metrics.height as i32 - metrics.ymin;
        let y_offset = glyph_top.max(0) as usize;

        assert!(glyph_top >= 0,
            "glyph '{}' (0x{:02x}) clipped at top", c, ch);
        assert!(y_offset + metrics.height <= FONT_HEIGHT,
            "glyph '{}' (0x{:02x}) clipped at bottom", c, ch);
        assert!(x_offset + metrics.width <= FONT_WIDTH,
            "glyph '{}' (0x{:02x}) clipped at right", c, ch);

        let glyph_base = ch * FONT_WIDTH * FONT_HEIGHT;

        for gy in 0..metrics.height {
            let cell_y = y_offset + gy;
            for gx in 0..metrics.width {
                let cell_x = x_offset + gx;
                let alpha = bitmap[gy * metrics.width + gx];
                output[glyph_base + cell_y * FONT_WIDTH + cell_x] = alpha;
            }
        }
    }

    let font_path = Path::new(initrd_dir).join("font.bin");
    fs::write(&font_path, &output).expect("Failed to write font bitmap");
    eprintln!(
        "initrd: generated font.bin ({} bytes, grayscale) from JetBrains Mono",
        output.len()
    );
}

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

fn create_initrd_image(initrd_dir: &str) -> Vec<u8> {
    let initrd = Path::new(initrd_dir);
    assert!(initrd.is_dir(), "initrd directory not found: {}", initrd_dir);

    // Collect files and calculate needed size
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in fs::read_dir(initrd).expect("Failed to read initrd") {
        let entry = entry.expect("Failed to read dir entry");
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            let data = fs::read(&path).expect("Failed to read file");
            eprintln!("initrd: adding '{}' ({} bytes)", name, data.len());
            files.push((name, data));
        }
    }

    // Size: header(64) + file data + toc entries(64 each) + padding
    let data_size: usize = files.iter().map(|(_, d)| d.len()).sum();
    let toc_size = files.len() * 64;
    let size = (64 + data_size + toc_size + 4095) & !4095; // round up to 4K
    let size = size.max(4096);

    let vec_disk = VecDisk::new(size);
    let mut tyfs = SimpleFs::format(vec_disk, size as u64);

    for (name, data) in &files {
        if !tyfs.create(name, data) {
            panic!("Failed to add '{}' to rootfs image", name);
        }
    }

    tyfs.into_disk().data
}

fn create_fat_fs_with_bl_and_kernel(initrd_bytes: &[u8]) -> Vec<u8> {
    let kernel_path = "../kernel/target/x86_64-unknown-none/debug/kernel";
    let bl_path = "../bootloader/target/x86_64-unknown-uefi/debug/bootloader.efi";

    let kernel_bytes = fs::read(kernel_path).expect("Failed to read kernel");
    let bl_bytes = fs::read(bl_path).expect("Failed to read bootloader");

    let content_size = kernel_bytes.len() + bl_bytes.len() + initrd_bytes.len();
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
            .create_file("initrd.img")
            .expect("Failed to create initrd.img")
            .write_all(initrd_bytes)
            .expect("Failed to write initrd");
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
    println!("cargo:rerun-if-changed=../initrd/");
    println!("cargo:rerun-if-changed=../userland/");
    println!("cargo:rerun-if-changed=../toolchain/.sysroot-stamp");
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

    fs::create_dir_all("../initrd").ok();

    // Build all userland apps (any directory under ../userland with a Cargo.toml)
    for entry in fs::read_dir("../userland")? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() || !path.join("Cargo.toml").exists() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        if !Command::new("cargo")
            .args(&["build", "--target", "x86_64-unknown-toyos"])
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env_remove("RUSTC")
            .current_dir(&path)
            .status()?
            .success()
        {
            panic!("Failed to build userland/{name}");
        }
        let binary = path.join(format!("target/x86_64-unknown-toyos/debug/{name}"));
        fs::copy(&binary, format!("../initrd/{name}"))?;
    }

    generate_font_bitmap("../initrd");
    let initrd_bytes = create_initrd_image("../initrd");
    let volume_bytes = create_fat_fs_with_bl_and_kernel(&initrd_bytes);
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
