use fatfs::FsOptions;
use std::fs;
use std::io::{Cursor, Write};
use std::process::Command;

fn main() -> std::io::Result<()> {
    // Build the kernel and bootloader projects
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

    // Create a blank image in memory
    let mut img = Vec::new();
    img.resize(10 * 1024 * 1024, 0); // 10 MiB

    // Format as FAT32
    fatfs::format_volume(Cursor::new(&mut img), Default::default())
        .expect("Failed to format volume");

    {
        // Create the filesystem
        let fat = fatfs::FileSystem::new(Cursor::new(&mut img), FsOptions::new())
            .expect("Failed to create filesystem");

        // Copy the kernel and bootloader to the filesystem
        let kernel_bytes = fs::read("../kernel/target/x86_64-unknown-none/release/kernel")
            .expect("Failed to read kernel");
        let bootloader_bytes =
            fs::read("../bootloader/target/x86_64-unknown-uefi/release/bootloader.efi")
                .expect("Failed to read bootloader");

        fat.root_dir()
            .create_dir("BOOT")
            .expect("Failed to create boot directory")
            .create_file("BOOTX64.EFI")
            .expect("Failed to create bootx64.efi")
            .write_all(&bootloader_bytes)
            .expect("Failed to write bootloader");
        fat.root_dir()
            .create_dir("toyos")
            .expect("Failed to create toyos directory")
            .create_file("kernel.elf")
            .expect("Failed to create kernel.elf")
            .write_all(&kernel_bytes)
            .expect("Failed to write kernel");
    }

    fs::write("target/bootable.img", img).expect("Failed to write image");

    Ok(())
}
