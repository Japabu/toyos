use fatfs::FsOptions;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::process::Command;
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

fn build() {
    if !Command::new("cargo")
        .args(&["build"])
        .current_dir("../kernel")
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build kernel");
    }

    if !Command::new("cargo")
        .args(&["build"])
        .current_dir("../bootloader")
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build bootloader");
    }

    fs::create_dir_all("../initrd").ok();

    // Detect toolchain changes — clean userland targets to avoid stale incremental artifacts
    let sysroot_stamp = fs::read_to_string("../toolchain/.sysroot-stamp").unwrap_or_default();
    let last_stamp_path = "target/.toolchain-stamp";
    let last_stamp = fs::read_to_string(last_stamp_path).unwrap_or_default();
    let toolchain_changed = sysroot_stamp != last_stamp && !sysroot_stamp.is_empty();

    // Build all userland apps (any directory under ../userland with a Cargo.toml)
    for entry in fs::read_dir("../userland").expect("Failed to read userland") {
        let entry = entry.expect("Failed to read dir entry");
        let path = entry.path();
        if !path.is_dir() || !path.join("Cargo.toml").exists() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        if toolchain_changed {
            eprintln!("toolchain changed: cleaning userland/{name}");
            Command::new("cargo")
                .args(&["clean"])
                .current_dir(&path)
                .status()
                .ok();
        }
        if !Command::new("cargo")
            .args(&["build", "--target", "x86_64-unknown-toyos"])
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env_remove("RUSTC")
            .current_dir(&path)
            .status()
            .expect("Failed to run cargo")
            .success()
        {
            panic!("Failed to build userland/{name}");
        }
        let binary = path.join(format!("target/x86_64-unknown-toyos/debug/{name}"));
        fs::copy(&binary, format!("../initrd/{name}")).expect("Failed to copy binary");
    }
    if !sysroot_stamp.is_empty() {
        fs::write(last_stamp_path, &sysroot_stamp).ok();
    }

    let initrd_bytes = create_initrd_image("../initrd");
    let volume_bytes = create_fat_fs_with_bl_and_kernel(&initrd_bytes);
    let disk_bytes = create_gpt_disk_with_esp_partition(volume_bytes);

    fs::write("target/bootable.img", disk_bytes).expect("Failed to write image");

    // Create empty NVMe disk image if it doesn't already exist (persistent across rebuilds)
    let nvme_path = "target/nvme.img";
    if !Path::new(nvme_path).exists() {
        let nvme_size = 128 * 1024 * 1024; // 128 MB
        let nvme_bytes = vec![0u8; nvme_size];
        fs::write(nvme_path, nvme_bytes).expect("Failed to write NVMe image");
    }
}

fn main() {
    build();

    Command::new("qemu-system-x86_64")
        .arg("-machine").arg("q35")
        .arg("-cpu").arg("qemu64,+rdrand")
        .arg("-m").arg("1G")
        // Flash the OVMF UEFI firmware
        .arg("-drive").arg("if=pflash,format=raw,unit=0,file=ovmf/OVMF_CODE-pure-efi.fd,readonly=on")
        .arg("-drive").arg("if=pflash,format=raw,unit=1,file=ovmf/OVMF_VARS-pure-efi.fd,readonly=on")

        // Create xHCI controller for USB
        .arg("-device").arg("nec-usb-xhci,id=xhci")

        // Create a USB stick with the bootable image
        .arg("-drive").arg("if=none,id=stick,format=raw,file=target/bootable.img")
        .arg("-device").arg("usb-storage,bus=xhci.0,drive=stick,bootindex=0")

        // USB keyboard
        .arg("-device").arg("usb-kbd,bus=xhci.0")

        // NVMe SSD
        .arg("-drive").arg("if=none,id=nvme0,format=raw,file=target/nvme.img")
        .arg("-device").arg("nvme,serial=deadbeef,drive=nvme0")

        .arg("-serial").arg("stdio")

        .arg("-no-reboot")

        // Enable gdb at port 1234
        .arg("-s")

        .status()
        .expect("failed to execute process");
}
