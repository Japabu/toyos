mod assets;
mod image;

use std::fs;
use std::path::Path;
use std::process::Command;

fn build(debug: bool, release: bool) {
    let profile = if release { "release" } else { "debug" };
    let rustflags = match std::env::var("RUSTFLAGS") {
        Ok(flags) => format!("{flags} -Dwarnings"),
        Err(_) => "-Dwarnings".to_string(),
    };

    // Detect toolchain changes — clean all targets to avoid stale incremental artifacts
    let toolchain_stamp = Path::new("../toolchain/.sysroot-stamp");
    let toolchain_changed = detect_change(&toolchain_stamp, "target/.toolchain-stamp");

    let mut kernel_args = vec!["build", "--target", "x86_64-unknown-none"];
    if release {
        kernel_args.push("--release");
    }
    if debug {
        kernel_args.push("--features");
        kernel_args.push("debug-wait");
    }
    if !Command::new("cargo")
        .args(&kernel_args)
        .current_dir("../kernel")
        .env("RUSTUP_TOOLCHAIN", "toyos")
        .env("RUSTFLAGS", &rustflags)
        .env_remove("RUSTC")
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build kernel");
    }

    let mut bl_args = vec!["build", "--target", "x86_64-unknown-uefi"];
    if release {
        bl_args.push("--release");
    }
    if !Command::new("cargo")
        .args(&bl_args)
        .current_dir("../bootloader")
        .env("RUSTUP_TOOLCHAIN", "toyos")
        .env("RUSTFLAGS", &rustflags)
        .env("INIT_PROGRAMS", "/bin/locale --load;/bin/compositor;/bin/netd;/bin/sshd")
        .env_remove("RUSTC")
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build bootloader");
    }

    // Ensure the ToyOS sysroot has host target libraries so proc-macros can compile.
    ensure_host_target_in_sysroot();

    let mut initrd_files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut symlinks: Vec<(String, String)> = Vec::new();

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
            let toyos_target_dir = path.join("target/x86_64-unknown-toyos");
            if toyos_target_dir.exists() {
                eprintln!("toolchain changed: cleaning userland/{name} (target)");
                fs::remove_dir_all(&toyos_target_dir).ok();
            }
            let host_deps_dir = path.join("target/debug");
            if host_deps_dir.exists() {
                eprintln!("toolchain changed: cleaning userland/{name} (host deps)");
                fs::remove_dir_all(&host_deps_dir).ok();
            }
        }
        let mut ul_args = vec!["build", "--target", "x86_64-unknown-toyos"];
        if release {
            ul_args.push("--release");
        }
        if !Command::new("cargo")
            .args(&ul_args)
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("RUSTFLAGS", &rustflags)
            .env_remove("RUSTC")
            .current_dir(&path)
            .status()
            .expect("Failed to run cargo")
            .success()
        {
            panic!("Failed to build userland/{name}");
        }
        let binary = path.join(format!("target/x86_64-unknown-toyos/{profile}/{name}"));
        let data = fs::read(&binary).expect("Failed to read binary");
        initrd_files.push((format!("bin/{name}"), data));
    }

    // Build cargo (non-standard layout: src/bin/cargo/main.rs, needs --no-default-features)
    {
        let cargo_dir = Path::new("../userland/cargo");
        if toolchain_changed {
            let toyos_target_dir = cargo_dir.join("target/x86_64-unknown-toyos");
            if toyos_target_dir.exists() {
                eprintln!("toolchain changed: cleaning userland/cargo (target)");
                fs::remove_dir_all(&toyos_target_dir).ok();
            }
        }
        let mut cargo_args = vec![
            "build",
            "--target", "x86_64-unknown-toyos",
            "--no-default-features",
        ];
        if release {
            cargo_args.push("--release");
        }
        // Cargo is a large upstream codebase — don't use -Dwarnings
        if !Command::new("cargo")
            .args(&cargo_args)
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env_remove("RUSTFLAGS")
            .env_remove("RUSTC")
            .current_dir(cargo_dir)
            .status()
            .expect("Failed to run cargo")
            .success()
        {
            panic!("Failed to build userland/cargo");
        }
        let binary = cargo_dir.join(format!("target/x86_64-unknown-toyos/{profile}/cargo"));
        let data = fs::read(&binary).expect("Failed to read cargo binary");
        initrd_files.push(("bin/cargo".to_string(), data));
    }

    // Write toolchain stamp after successful builds
    write_stamp(&toolchain_stamp, "target/.toolchain-stamp");

    // Add rustc compiler with sysroot directory structure.
    let sysroot = Path::new("../rust/build/x86_64-unknown-toyos/stage2");
    if sysroot.exists() {
        let rustc = sysroot.join("bin/rustc");
        if rustc.exists() {
            initrd_files.push(("bin/rustc".to_string(), fs::read(&rustc).unwrap()));
        }
        // Shared libraries
        for entry in fs::read_dir(sysroot.join("lib")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "so") {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                let data = fs::read(&path).unwrap();
                initrd_files.push((format!("lib/{name}"), data));
            }
        }
        // Codegen backends
        let backends = sysroot.join("lib/rustlib/x86_64-unknown-toyos/codegen-backends");
        if backends.exists() {
            for entry in fs::read_dir(&backends).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|e| e == "so") {
                    let name = path.file_name().unwrap().to_str().unwrap().to_string();
                    let data = fs::read(&path).unwrap();
                    initrd_files.push((
                        format!("lib/rustlib/x86_64-unknown-toyos/codegen-backends/{name}"),
                        data,
                    ));
                }
            }
        }
        // Target .rlib files — bootstrap puts these in the host sysroot, not the ToyOS one
        let host_rlibs = find_host_rlibs();
        for entry in fs::read_dir(&host_rlibs).into_iter().flatten() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "rlib" || e == "rmeta") {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                initrd_files.push((
                    format!("lib/rustlib/x86_64-unknown-toyos/lib/{name}"),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }

    initrd_files.extend(assets::collect());

    // Generate symlinks for toybox commands by scanning its source modules
    for entry in fs::read_dir("../userland/toybox/src").expect("Failed to read toybox/src") {
        let entry = entry.expect("Failed to read dir entry");
        let name = entry.file_name();
        let name = name.to_str().unwrap().to_string();
        if name == "main.rs" || !name.ends_with(".rs") {
            continue;
        }
        let cmd = name.strip_suffix(".rs").unwrap().to_string();
        symlinks.push((format!("bin/{cmd}"), "bin/toybox".to_string()));
    }

    // Write initrd contents to target/initrd/ for inspection
    let initrd_dir = Path::new("target/initrd");
    if initrd_dir.exists() {
        fs::remove_dir_all(initrd_dir).expect("Failed to clean initrd dir");
    }
    for (name, data) in &initrd_files {
        let path = initrd_dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, data).unwrap();
    }
    for (name, target) in &symlinks {
        let path = initrd_dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &path).unwrap();
    }
    eprintln!("initrd contents written to target/initrd/");

    let initrd_bytes = image::create_initrd(&initrd_files, &symlinks);
    let disk_bytes = image::create_boot_image(&initrd_bytes, profile);

    fs::write("target/bootable.img", disk_bytes).expect("Failed to write image");

    // Create empty NVMe disk image if it doesn't already exist (persistent across rebuilds)
    let nvme_path = "target/nvme.img";
    if !Path::new(nvme_path).exists() {
        let nvme_bytes = vec![0u8; 1024 * 1024 * 1024];
        fs::write(nvme_path, nvme_bytes).expect("Failed to write NVMe image");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let debug = args.iter().any(|a| a == "--debug");
    build(debug, cfg!(release));

    let mut qemu = Command::new("qemu-system-x86_64");
    qemu
        .arg("-machine").arg("q35")
        .arg("-cpu").arg("qemu64,+rdrand")
        .arg("-smp").arg("2")
        .arg("-m").arg("8G")
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
        .arg("-netdev").arg("user,id=net0,hostfwd=tcp::2222-:22")
        .arg("-device").arg("virtio-net-pci-non-transitional,netdev=net0")

        // VirtIO sound
        .arg("-audiodev").arg("coreaudio,id=audio0")
        .arg("-device").arg("virtio-sound-pci,audiodev=audio0,streams=1")

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

/// Find the .rlib files for x86_64-unknown-toyos in the host sysroot.
/// Bootstrap places target libraries in the host's sysroot, not the ToyOS-hosted one.
fn find_host_rlibs() -> std::path::PathBuf {
    let build_dir = Path::new("../rust/build");
    for entry in fs::read_dir(build_dir).expect("rust/build/ not found") {
        let path = entry.unwrap().path();
        // Skip the ToyOS-hosted sysroot (its lib dir is empty)
        if path.file_name().is_some_and(|n| n == "x86_64-unknown-toyos") {
            continue;
        }
        let rlib_dir = path.join("stage2/lib/rustlib/x86_64-unknown-toyos/lib");
        if rlib_dir.exists() {
            return rlib_dir;
        }
    }
    panic!("Could not find host sysroot with ToyOS .rlib files in rust/build/");
}

/// Ensure the ToyOS sysroot contains host target libraries for proc-macro compilation.
fn ensure_host_target_in_sysroot() {
    let host = host_triple();
    let toyos_sysroot = Path::new("../rust/build/x86_64-unknown-toyos/stage2/lib/rustlib");
    let host_target_dir = toyos_sysroot.join(&host);
    if host_target_dir.exists() {
        return;
    }

    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("Failed to run rustc");
    let stable_sysroot = String::from_utf8(output.stdout).unwrap();
    let stable_sysroot = stable_sysroot.trim();
    let source = Path::new(stable_sysroot)
        .join("lib/rustlib")
        .join(&host);
    assert!(
        source.exists(),
        "Host target {} not found in stable toolchain at {}",
        host,
        source.display()
    );

    #[cfg(unix)]
    std::os::unix::fs::symlink(&source, &host_target_dir).unwrap_or_else(|e| {
        panic!(
            "Failed to symlink {} -> {}: {}",
            host_target_dir.display(),
            source.display(),
            e
        )
    });
    #[cfg(not(unix))]
    panic!("Symlinking host target not supported on this platform");

    eprintln!(
        "Symlinked host target {} into ToyOS sysroot",
        host
    );
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
