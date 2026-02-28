mod assets;
mod image;

use std::fs;
use std::path::Path;
use std::process::Command;

fn build(debug: bool) {
    let toyos_ld = build_toyos_ld();
    let rustflags = match std::env::var("RUSTFLAGS") {
        Ok(flags) => format!("{flags} -Dwarnings"),
        Err(_) => "-Dwarnings".to_string(),
    };

    // Detect linker changes — clean all link targets to avoid stale binaries
    let linker_changed = detect_change(&toyos_ld, "target/.linker-stamp");
    if linker_changed {
        for (dir, target) in [
            ("../kernel", "x86_64-unknown-none"),
            ("../bootloader", "x86_64-unknown-uefi"),
        ] {
            let target_dir = Path::new(dir).join(format!("target/{target}"));
            if target_dir.exists() {
                eprintln!("linker changed: cleaning {dir}");
                fs::remove_dir_all(&target_dir).ok();
            }
        }
    }

    // Detect toolchain changes — clean userland targets to avoid stale incremental artifacts
    let toolchain_stamp = Path::new("../toolchain/.sysroot-stamp");
    let toolchain_changed = detect_change(&toolchain_stamp, "target/.toolchain-stamp");

    let mut kernel_args = vec!["build"];
    if debug {
        kernel_args.push("--features");
        kernel_args.push("debug-wait");
    }
    if !Command::new("cargo")
        .args(&kernel_args)
        .current_dir("../kernel")
        .env("RUSTFLAGS", &rustflags)
        .env("CARGO_TARGET_X86_64_UNKNOWN_NONE_LINKER", toyos_ld.to_str().unwrap())
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build kernel");
    }

    if !Command::new("cargo")
        .args(&["build"])
        .current_dir("../bootloader")
        .env("RUSTFLAGS", &rustflags)
        .env("CARGO_TARGET_X86_64_UNKNOWN_UEFI_LINKER", toyos_ld.to_str().unwrap())
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build bootloader");
    }

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
        if toolchain_changed || linker_changed {
            let toyos_target_dir = path.join("target/x86_64-unknown-toyos");
            if toyos_target_dir.exists() {
                let reason = if toolchain_changed { "toolchain" } else { "linker" };
                eprintln!("{reason} changed: cleaning userland/{name}");
                fs::remove_dir_all(&toyos_target_dir).ok();
            }
        }
        if !Command::new("cargo")
            .args(&["build", "--target", "x86_64-unknown-toyos"])
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("RUSTFLAGS", &rustflags)
            .env("CARGO_TARGET_X86_64_UNKNOWN_TOYOS_LINKER", toyos_ld.to_str().unwrap())
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

    // Write stamps after successful builds
    write_stamp(&toyos_ld, "target/.linker-stamp");
    write_stamp(&toolchain_stamp, "target/.toolchain-stamp");

    // Add rustc compiler from bootstrap sysroot
    let sysroot = Path::new("../rust/build/x86_64-unknown-toyos/stage2");
    if sysroot.exists() {
        // rustc binary
        let rustc = sysroot.join("bin/rustc");
        if rustc.exists() {
            initrd_files.push(("rustc".to_string(), fs::read(&rustc).unwrap()));
        }
        // Shared libraries (rustc_driver, proc macros, etc.)
        for entry in fs::read_dir(sysroot.join("lib")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "so") {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                initrd_files.push((name, fs::read(&path).unwrap()));
            }
        }
        // Codegen backends
        let backends = sysroot.join("lib/rustlib/x86_64-unknown-toyos/codegen-backends");
        if backends.exists() {
            for entry in fs::read_dir(&backends).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|e| e == "so") {
                    let name = path.file_name().unwrap().to_str().unwrap().to_string();
                    initrd_files.push((name, fs::read(&path).unwrap()));
                }
            }
        }
    }

    initrd_files.extend(assets::collect());

    // Generate symlinks for toybox commands by scanning its source modules
    let mut symlinks: Vec<(String, String)> = Vec::new();
    for entry in fs::read_dir("../userland/toybox/src").expect("Failed to read toybox/src") {
        let entry = entry.expect("Failed to read dir entry");
        let name = entry.file_name();
        let name = name.to_str().unwrap().to_string();
        if name == "main.rs" || !name.ends_with(".rs") {
            continue;
        }
        let cmd = name.strip_suffix(".rs").unwrap().to_string();
        symlinks.push((cmd, "toybox".to_string()));
    }

    let initrd_bytes = image::create_initrd(&initrd_files, &symlinks);
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
    let debug = std::env::args().any(|a| a == "--debug");
    build(debug);

    let mut qemu = Command::new("qemu-system-x86_64");
    qemu
        .arg("-machine").arg("q35")
        .arg("-cpu").arg("qemu64,+rdrand")
        .arg("-smp").arg("2")
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

        // VirtIO GPU (no legacy VGA)
        .arg("-vga").arg("none")
        .arg("-device").arg("virtio-gpu-pci")

        // VirtIO networking with user-mode (SLIRP) backend
        .arg("-netdev").arg("user,id=net0")
        .arg("-device").arg("virtio-net-pci-non-transitional,netdev=net0")

        .arg("-serial").arg("stdio")

        .arg("-no-reboot")

        // Enable gdb at port 1234
        .arg("-s")

        // QMP socket for programmatic control
        .arg("-qmp").arg("unix:/tmp/toyos-qmp.sock,server,nowait");

    if debug {
        eprintln!("Debug mode: kernel will wait for debugger before entering userland");
    }

    qemu.status().expect("failed to execute process");
}

fn build_toyos_ld() -> std::path::PathBuf {
    let toyos_ld_dir = Path::new("../userland/toyos-ld");
    let host = host_triple();
    if !Command::new("cargo")
        .args(["build", "--release", "--target", &host])
        .current_dir(toyos_ld_dir)
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build toyos-ld");
    }
    toyos_ld_dir
        .join(format!("target/{host}/release/toyos-ld"))
        .canonicalize()
        .expect("toyos-ld binary not found after build")
}

fn host_triple() -> String {
    let output = Command::new("rustc")
        .args(["--version", "--verbose"])
        .output()
        .expect("Failed to run rustc");
    let text = String::from_utf8(output.stdout).unwrap();
    text.lines()
        .find(|l| l.starts_with("host:"))
        .map(|l| l.strip_prefix("host: ").unwrap().to_string())
        .expect("Could not determine host triple")
}

/// Compare a file's mtime against a stored stamp. Returns true if changed.
fn detect_change(path: &Path, stamp_path: &str) -> bool {
    let mtime = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                .to_string()
        })
        .unwrap_or_default();
    let last = fs::read_to_string(stamp_path).unwrap_or_default();
    !mtime.is_empty() && mtime != last
}

fn write_stamp(path: &Path, stamp_path: &str) {
    if let Ok(meta) = fs::metadata(path) {
        if let Ok(mtime) = meta.modified() {
            let stamp = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                .to_string();
            fs::write(stamp_path, stamp).ok();
        }
    }
}
