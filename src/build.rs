use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::assets;
use crate::image;
use crate::toolchain;
use crate::toolchain::ChangeSet;

#[derive(Deserialize)]
struct SystemConfig {
    programs: HashMap<String, ProgramConfig>,
    init: Vec<String>,
    #[serde(default)]
    symlinks: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
struct ProgramConfig {
    no_default_features: bool,
    warnings: Option<bool>,
}

pub fn build(root: &Path, debug: bool, release: bool, changes: &ChangeSet) {
    let profile = if release { "release" } else { "debug" };
    let rustflags = match std::env::var("RUSTFLAGS") {
        Ok(flags) => format!("{flags} -Dwarnings"),
        Err(_) => "-Dwarnings".to_string(),
    };

    // PATH with toyos-ld so rustc finds the linker
    let path_env = toolchain::path_with_toyos_ld(root);

    // Parse system config (needed by bootloader for init programs)
    let config: SystemConfig = toml::from_str(
        &fs::read_to_string(root.join("system.toml")).expect("Failed to read system.toml"),
    )
    .expect("Failed to parse system.toml");

    // Build kernel and bootloader in parallel (no dependency on each other)
    let init_programs = config.init.join(";");
    let kernel_handle = {
        let root = root.to_path_buf();
        let rustflags = rustflags.clone();
        let path_env = path_env.clone();
        std::thread::spawn(move || {
            build_kernel(&root, debug, release, &rustflags, &path_env);
        })
    };
    build_bootloader(root, release, &rustflags, &path_env, &init_programs);
    kernel_handle.join().expect("kernel build thread panicked");

    let userland_dir = root.join("userland");

    // Clean targets on toolchain or linker change.
    // Use a userland stamp to track whether a clean build succeeded —
    // if a previous build failed mid-way, the clean runs again.
    let userland_stamp = root.join("target/stamps/userland.stamp");
    let needs_clean = changes.std_rebuilt || !userland_stamp.exists();
    if needs_clean {
        // std changed — full clean needed (compiled artifacts are stale)
        for subdir in ["target/x86_64-unknown-toyos", "target/debug"] {
            let dir = userland_dir.join(subdir);
            if dir.exists() {
                eprintln!("toolchain changed: cleaning userland/{subdir}");
                fs::remove_dir_all(&dir).ok();
            }
        }
        for crate_name in ["toyos-ld", "toyos-cc"] {
            let dir = root.join(format!("{crate_name}/target/x86_64-unknown-toyos"));
            if dir.exists() {
                eprintln!("toolchain changed: cleaning {crate_name}/target/x86_64-unknown-toyos");
                fs::remove_dir_all(&dir).ok();
            }
        }
    } else if changes.linker_changed {
        // Linker changed — cargo doesn't track the linker binary, so we must
        // delete final executables to force re-linking without full recompilation.
        eprintln!("linker changed: forcing re-link of all binaries");
        let ws_target = userland_dir.join(format!("target/x86_64-unknown-toyos/{profile}"));
        for name in config.programs.keys() {
            let bin = ws_target.join(name);
            if bin.exists() {
                fs::remove_file(&bin).ok();
            }
            // Standalone builds
            let standalone_bin = userland_dir.join(format!(
                "{name}/target/x86_64-unknown-toyos/{profile}/{name}"
            ));
            if standalone_bin.exists() {
                fs::remove_file(&standalone_bin).ok();
            }
        }
        // Also force re-link of toyos-ld/toyos-cc ToyOS binaries
        for crate_name in ["toyos-ld", "toyos-cc"] {
            let bin = root.join(format!(
                "{crate_name}/target/x86_64-unknown-toyos/{profile}/{crate_name}"
            ));
            if bin.exists() {
                fs::remove_file(&bin).ok();
            }
        }
    }

    // Partition programs into workspace members vs standalone (own workspace / special config)
    let mut workspace_packages: Vec<&String> = Vec::new();
    let mut standalone: Vec<(&String, &ProgramConfig)> = Vec::new();
    for (name, prog_config) in &config.programs {
        let path = userland_dir.join(name);
        assert!(
            path.join("Cargo.toml").exists(),
            "Program '{name}' listed in system.toml but userland/{name}/Cargo.toml not found"
        );
        if prog_config.no_default_features || prog_config.warnings == Some(false) {
            standalone.push((name, prog_config));
        } else {
            workspace_packages.push(name);
        }
    }

    // Build workspace programs in one shot
    if !workspace_packages.is_empty() {
        let mut args = vec!["build", "--target", "x86_64-unknown-toyos"];
        if release {
            args.push("--release");
        }
        for pkg in &workspace_packages {
            args.push("-p");
            args.push(pkg);
        }
        if !Command::new("cargo")
            .args(&args)
            .current_dir(&userland_dir)
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("RUSTFLAGS", &rustflags)
            .env("PATH", &path_env)
            .env_remove("RUSTC")
            .status()
            .expect("Failed to run cargo")
            .success()
        {
            panic!("Failed to build userland workspace");
        }
    }

    // Build standalone programs individually (e.g. cargo — own workspace, special flags)
    for (name, prog_config) in &standalone {
        let path = userland_dir.join(name);

        if changes.std_rebuilt {
            for subdir in ["target/x86_64-unknown-toyos", "target/debug"] {
                let dir = path.join(subdir);
                if dir.exists() {
                    eprintln!("toolchain changed: cleaning userland/{name}/{subdir}");
                    fs::remove_dir_all(&dir).ok();
                }
            }
        }

        let mut args = vec!["build", "--target", "x86_64-unknown-toyos"];
        if release {
            args.push("--release");
        }
        if prog_config.no_default_features {
            args.push("--no-default-features");
        }
        let env_rustflags = if prog_config.warnings.unwrap_or(true) {
            rustflags.as_str()
        } else {
            ""
        };
        if !Command::new("cargo")
            .args(&args)
            .current_dir(&path)
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("RUSTFLAGS", env_rustflags)
            .env("PATH", &path_env)
            .env_remove("RUSTC")
            .status()
            .expect("Failed to run cargo")
            .success()
        {
            panic!("Failed to build userland/{name}");
        }
    }

    // Build toyos-ld and toyos-cc for ToyOS (standalone, not workspace members)
    for crate_name in ["toyos-ld", "toyos-cc"] {
        let mut args = vec!["build", "--target", "x86_64-unknown-toyos"];
        if release {
            args.push("--release");
        }
        if !Command::new("cargo")
            .args(&args)
            .current_dir(root.join(crate_name))
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("RUSTFLAGS", &rustflags)
            .env("PATH", &path_env)
            .env_remove("RUSTC")
            .status()
            .unwrap_or_else(|e| panic!("Failed to run cargo for {crate_name}: {e}"))
            .success()
        {
            panic!("Failed to build {crate_name} for ToyOS");
        }
    }

    // Collect initrd contents: binaries, rustc sysroot, assets, symlinks
    let mut initrd_files: Vec<(String, Vec<u8>)> = Vec::new();
    let workspace_target = userland_dir.join(format!("target/x86_64-unknown-toyos/{profile}"));

    for (name, prog_config) in &config.programs {
        let binary = if prog_config.no_default_features || prog_config.warnings == Some(false) {
            userland_dir.join(name).join(format!("target/x86_64-unknown-toyos/{profile}/{name}"))
        } else {
            workspace_target.join(name)
        };
        let data =
            fs::read(&binary).unwrap_or_else(|_| panic!("Failed to read binary for {name}"));
        initrd_files.push((format!("bin/{name}"), data));
    }

    // Add toyos-ld and toyos-cc ToyOS binaries to initrd
    for crate_name in ["toyos-ld", "toyos-cc"] {
        let binary = root.join(format!("{crate_name}/target/x86_64-unknown-toyos/{profile}/{crate_name}"));
        let data = fs::read(&binary)
            .unwrap_or_else(|_| panic!("Failed to read {crate_name} ToyOS binary"));
        initrd_files.push((format!("bin/{crate_name}"), data));
    }

    collect_hosted_rustc(root, &mut initrd_files);
    initrd_files.extend(assets::collect());

    let initrd_symlinks: Vec<(String, String)> = config.symlinks
        .into_iter().collect();

    // Mirror initrd to target/initrd/ for inspection
    let initrd_dir = root.join("target/initrd");
    if initrd_dir.exists() {
        fs::remove_dir_all(&initrd_dir).expect("Failed to clean initrd dir");
    }
    for (name, data) in &initrd_files {
        let path = initrd_dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, data).unwrap();
    }
    for (name, target) in &initrd_symlinks {
        let path = initrd_dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("-> {target}")).unwrap();
    }

    let initrd_bytes = image::create_initrd(&initrd_files, &initrd_symlinks);
    let disk_bytes = image::create_boot_image(&initrd_bytes, profile);

    fs::write(root.join("target/bootable.img"), disk_bytes).expect("Failed to write image");

    // Create empty NVMe disk image if it doesn't already exist
    let nvme_path = root.join("target/nvme.img");
    if !nvme_path.exists() {
        let nvme_bytes = vec![0u8; 1024 * 1024 * 1024];
        fs::write(&nvme_path, nvme_bytes).expect("Failed to write NVMe image");
    }

    // Mark userland build as successful — prevents redundant cleans on next run
    fs::write(&userland_stamp, "").ok();
}

fn collect_hosted_rustc(root: &Path, initrd_files: &mut Vec<(String, Vec<u8>)>) {
    let sysroot = root.join("rust/build/x86_64-unknown-toyos/stage2");
    assert!(sysroot.exists(), "Hosted rustc sysroot missing: {}", sysroot.display());

    let rustc = sysroot.join("bin/rustc");
    assert!(rustc.exists(), "Hosted rustc binary missing: {}", rustc.display());
    initrd_files.push(("bin/rustc".to_string(), fs::read(&rustc).unwrap()));

    // Shared libraries
    if let Ok(entries) = fs::read_dir(sysroot.join("lib")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "so") {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                let data = fs::read(&path).unwrap();
                initrd_files.push((format!("lib/{name}"), data));
            }
        }
    }

    // Codegen backends
    let backends = sysroot.join("lib/rustlib/x86_64-unknown-toyos/codegen-backends");
    if backends.exists() {
        for entry in fs::read_dir(&backends).into_iter().flatten().flatten() {
            let path = entry.path();
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

    // Target .rlib files
    if let Some(host_rlibs) = find_host_rlibs(root) {
        for entry in fs::read_dir(&host_rlibs).into_iter().flatten().flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|e| e == "rlib" || e == "rmeta")
            {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                initrd_files.push((
                    format!("lib/rustlib/x86_64-unknown-toyos/lib/{name}"),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }
}

fn build_kernel(root: &Path, debug: bool, release: bool, rustflags: &str, path_env: &str) {
    let mut args = vec!["build", "--target", "x86_64-unknown-none"];
    if release {
        args.push("--release");
    }
    if debug {
        args.push("--features");
        args.push("debug-wait");
    }
    if !Command::new("cargo")
        .args(&args)
        .current_dir(root.join("kernel"))
        .env("RUSTUP_TOOLCHAIN", "toyos")
        .env("RUSTFLAGS", rustflags)
        .env("PATH", path_env)
        .env_remove("RUSTC")
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build kernel");
    }
}

fn build_bootloader(root: &Path, release: bool, rustflags: &str, path_env: &str, init_programs: &str) {
    let mut args = vec!["build", "--target", "x86_64-unknown-uefi"];
    if release {
        args.push("--release");
    }
    if !Command::new("cargo")
        .args(&args)
        .current_dir(root.join("bootloader"))
        .env("RUSTUP_TOOLCHAIN", "toyos")
        .env("RUSTFLAGS", rustflags)
        .env("PATH", path_env)
        .env("INIT_PROGRAMS", init_programs)
        .env_remove("RUSTC")
        .status()
        .expect("Failed to run cargo")
        .success()
    {
        panic!("Failed to build bootloader");
    }
}

fn find_host_rlibs(root: &Path) -> Option<PathBuf> {
    let build_dir = root.join("rust/build");
    let entries = fs::read_dir(&build_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n == "x86_64-unknown-toyos")
        {
            continue;
        }
        let rlib_dir = path.join("stage2/lib/rustlib/x86_64-unknown-toyos/lib");
        if rlib_dir.exists() {
            return Some(rlib_dir);
        }
    }
    None
}
