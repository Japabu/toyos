mod assets;
mod image;

use std::fs;
use std::path::Path;
use std::process::Command;

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

    // Detect toolchain changes — clean userland targets to avoid stale incremental artifacts
    let sysroot_stamp = fs::read_to_string("../toolchain/.sysroot-stamp").unwrap_or_default();
    let last_stamp_path = "target/.toolchain-stamp";
    let last_stamp = fs::read_to_string(last_stamp_path).unwrap_or_default();
    let toolchain_changed = sysroot_stamp != last_stamp && !sysroot_stamp.is_empty();

    let mut initrd_files: Vec<(String, Vec<u8>)> = Vec::new();

    // Build all userland apps (any directory under ../userland with a Cargo.toml and main.rs)
    for entry in fs::read_dir("../userland").expect("Failed to read userland") {
        let entry = entry.expect("Failed to read dir entry");
        let path = entry.path();
        if !path.is_dir() || !path.join("Cargo.toml").exists() || !path.join("src/main.rs").exists() {
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
        let data = fs::read(&binary).expect("Failed to read binary");
        initrd_files.push((name.to_string(), data));
    }
    if !sysroot_stamp.is_empty() {
        fs::write(last_stamp_path, &sysroot_stamp).ok();
    }

    initrd_files.push(("font.bin".to_string(), assets::rasterize_font()));
    initrd_files.push(("cursor.bin".to_string(), assets::generate_cursor()));

    let initrd_bytes = image::create_initrd(&initrd_files);
    let disk_bytes = image::create_boot_image(&initrd_bytes);

    fs::write("target/bootable.img", disk_bytes).expect("Failed to write image");

    // Create empty NVMe disk image if it doesn't already exist (persistent across rebuilds)
    let nvme_path = "target/nvme.img";
    if !Path::new(nvme_path).exists() {
        let nvme_bytes = vec![0u8; 128 * 1024 * 1024];
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

        // USB keyboard + mouse
        .arg("-device").arg("usb-kbd,bus=xhci.0")
        .arg("-device").arg("usb-mouse,bus=xhci.0")

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
