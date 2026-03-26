use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use crate::assets;
use crate::image;
use crate::toolchain;

// --- Config ---

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SystemConfig {
    init: Vec<String>,
    #[serde(default)]
    programs: HashMap<String, ProgramConfig>,
    #[serde(default)]
    symlinks: HashMap<String, String>,
    #[serde(default)]
    hosted_rustc: bool,
    #[serde(default)]
    assets: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
struct ProgramConfig {
    path: Option<String>,
    no_default_features: bool,
    warnings: Option<bool>,
}

impl ProgramConfig {
    /// Resolve the crate directory for this program.
    /// Defaults to `userland/<name>` if no explicit path is set.
    fn crate_dir(&self, root: &Path, name: &str) -> PathBuf {
        match &self.path {
            Some(p) => root.join(p),
            None => root.join("userland").join(name),
        }
    }

    /// Whether this program is a workspace member of the userland workspace.
    /// Programs with explicit paths or special flags are standalone.
    fn is_workspace_member(&self) -> bool {
        self.path.is_none() && !self.no_default_features && self.warnings != Some(false)
    }
}

fn parse_config(path: &Path) -> SystemConfig {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    toml::from_str(&text)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()))
}

// --- Freshness checking ---

/// Fingerprint all external build dependencies that cargo cannot track.
fn external_fingerprint(root: &Path) -> String {
    let host = toolchain::host_triple();
    let sysroot = root.join(format!("rust/build/{host}/stage2/lib/rustlib"));
    let mut entries = Vec::new();

    for triple in ["x86_64-unknown-toyos", "x86_64-unknown-none", "x86_64-unknown-uefi"] {
        let lib_dir = sysroot.join(format!("{triple}/lib"));
        let Ok(rd) = fs::read_dir(&lib_dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("rlib" | "rmeta")) {
                continue;
            }
            if let Ok(meta) = path.metadata() {
                let name = path.file_name().unwrap().to_string_lossy();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                entries.push(format!("{triple}/{name}:{}:{mtime}", meta.len()));
            }
        }
    }

    let linker = toolchain::toyos_ld_binary(root);
    if let Ok(meta) = linker.metadata() {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        entries.push(format!("toyos-ld:{}:{mtime}", meta.len()));
    }

    entries.sort();
    entries.join("\n")
}

/// Ensure a crate's build artifacts are fresh. If stale, run `cargo clean`.
fn ensure_fresh(crate_dir: &Path, fingerprint: &str) {
    let stamp = crate_dir.join("target/.deps-stamp");
    if stamp.exists() {
        if let Ok(stored) = fs::read_to_string(&stamp) {
            if stored == fingerprint {
                return;
            }
        }
    }

    eprintln!("external deps changed: cleaning {}", crate_dir.display());
    let _ = Command::new("cargo")
        .arg("clean")
        .current_dir(crate_dir)
        .status();

    fs::create_dir_all(crate_dir.join("target")).ok();
    fs::write(&stamp, fingerprint).ok();
}

/// Like ensure_fresh but only cleans the ToyOS cross-compilation target,
/// preserving host builds (e.g. the toyos-ld host binary used as the linker).
fn ensure_fresh_toyos_only(crate_dir: &Path, fingerprint: &str) {
    let stamp = crate_dir.join("target/.deps-stamp");
    if stamp.exists() {
        if let Ok(stored) = fs::read_to_string(&stamp) {
            if stored == fingerprint {
                return;
            }
        }
    }

    let toyos_dir = crate_dir.join("target/x86_64-unknown-toyos");
    if toyos_dir.exists() {
        eprintln!("external deps changed: cleaning {}", toyos_dir.display());
        fs::remove_dir_all(&toyos_dir).ok();
    }

    fs::create_dir_all(crate_dir.join("target")).ok();
    fs::write(&stamp, fingerprint).ok();
}

/// Invalidate all crates referenced by a config.
fn invalidate_stale(root: &Path, config: &SystemConfig, fp: &str) {
    ensure_fresh(&root.join("kernel"), fp);
    ensure_fresh(&root.join("bootloader"), fp);
    ensure_fresh(&root.join("userland"), fp);
    for (name, cfg) in &config.programs {
        if !cfg.is_workspace_member() {
            // Crates with explicit paths (e.g. toyos-ld, toyos-cc) may also have
            // host builds we must preserve. Only clean the ToyOS target.
            ensure_fresh_toyos_only(&cfg.crate_dir(root, name), fp);
        }
    }
}

// --- Cargo helpers ---

fn build_kernel(root: &Path, debug_wait: bool, release: bool, rustflags: &str, path_env: &str) {
    let mut args = vec!["build", "--target", "x86_64-unknown-none"];
    if release {
        args.push("--release");
    }
    if debug_wait {
        args.push("--features");
        args.push("debug-wait");
    }
    assert!(
        Command::new("cargo")
            .args(&args)
            .current_dir(root.join("kernel"))
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("RUSTFLAGS", rustflags)
            .env("PATH", path_env)
            .env_remove("RUSTC")
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build kernel"
    );
}

fn build_bootloader(
    root: &Path,
    release: bool,
    rustflags: &str,
    path_env: &str,
    init_programs: &str,
) {
    let mut args = vec!["build", "--target", "x86_64-unknown-uefi"];
    if release {
        args.push("--release");
    }
    assert!(
        Command::new("cargo")
            .args(&args)
            .current_dir(root.join("bootloader"))
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("RUSTFLAGS", rustflags)
            .env("PATH", path_env)
            .env("INIT_PROGRAMS", init_programs)
            .env_remove("RUSTC")
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build bootloader"
    );
}

fn cargo_build_toyos(crate_dir: &Path, extra_args: &[&str], rustflags: &str, path_env: &str) {
    let mut args = vec!["build", "--target", "x86_64-unknown-toyos"];
    args.extend_from_slice(extra_args);
    assert!(
        Command::new("cargo")
            .args(&args)
            .current_dir(crate_dir)
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("RUSTFLAGS", rustflags)
            .env("PATH", path_env)
            .env_remove("RUSTC")
            .status()
            .unwrap_or_else(|e| panic!("cargo build failed in {}: {e}", crate_dir.display()))
            .success(),
        "cargo build failed in {}",
        crate_dir.display()
    );
}

// --- Shared initrd assembly ---

/// Build all programs from a config and assemble an initrd.
fn build_and_assemble(
    root: &Path,
    config: &SystemConfig,
    profile: &str,
    rustflags: &str,
    path_env: &str,
    extra_files: &[(String, Vec<u8>)],
) -> Vec<u8> {
    let userland_dir = root.join("userland");

    // Partition into workspace members vs standalone
    let mut workspace_packages: Vec<&str> = Vec::new();
    let mut standalone: Vec<(&String, &ProgramConfig)> = Vec::new();
    for (name, cfg) in &config.programs {
        let crate_dir = cfg.crate_dir(root, name);
        assert!(
            crate_dir.join("Cargo.toml").exists(),
            "Program '{name}' crate not found at {}",
            crate_dir.display()
        );
        if cfg.is_workspace_member() {
            workspace_packages.push(name);
        } else {
            standalone.push((name, cfg));
        }
    }

    // Build workspace programs in one shot
    if !workspace_packages.is_empty() {
        let mut extra: Vec<&str> = Vec::new();
        if profile == "release" {
            extra.push("--release");
        }
        for pkg in &workspace_packages {
            extra.push("-p");
            extra.push(pkg);
        }
        cargo_build_toyos(&userland_dir, &extra, rustflags, path_env);
    }

    // Build standalone programs individually
    for (name, cfg) in &standalone {
        let crate_dir = cfg.crate_dir(root, name);
        let mut extra: Vec<&str> = Vec::new();
        if profile == "release" {
            extra.push("--release");
        }
        if cfg.no_default_features {
            extra.push("--no-default-features");
        }
        let flags = if cfg.warnings.unwrap_or(true) {
            rustflags
        } else {
            ""
        };
        cargo_build_toyos(&crate_dir, &extra, flags, path_env);
    }

    // Collect binaries
    let mut initrd_files: Vec<(String, Vec<u8>)> = Vec::new();
    let ws_target = userland_dir.join(format!("target/x86_64-unknown-toyos/{profile}"));

    for (name, cfg) in &config.programs {
        let binary = if cfg.is_workspace_member() {
            ws_target.join(name)
        } else {
            let crate_dir = cfg.crate_dir(root, name);
            crate_dir.join(format!("target/x86_64-unknown-toyos/{profile}/{name}"))
        };
        let data =
            fs::read(&binary).unwrap_or_else(|_| panic!("Failed to read binary for {name}"));
        initrd_files.push((format!("bin/{name}"), data));
    }

    // Hosted rustc
    if config.hosted_rustc {
        collect_hosted_rustc(root, &mut initrd_files);
    }

    // Assets
    if !config.assets.is_empty() {
        initrd_files.extend(assets::collect(&config.assets));
    }

    // Extra files (test binaries, shared libs)
    for (name, data) in extra_files {
        initrd_files.push((name.clone(), data.clone()));
    }

    let symlinks: Vec<(String, String)> = config.symlinks.iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    image::create_initrd(&initrd_files, &symlinks)
}

// --- Public API ---

/// Full build: kernel, bootloader, all programs, boot image.
pub fn build(root: &Path, debug: bool, release: bool) {
    let profile = if release { "release" } else { "debug" };
    let rustflags = match std::env::var("RUSTFLAGS") {
        Ok(flags) => format!("{flags} -Dwarnings"),
        Err(_) => "-Dwarnings".to_string(),
    };
    let path_env = toolchain::path_with_toyos_ld(root);
    let config = parse_config(&root.join("system.toml"));
    let fp = external_fingerprint(root);

    invalidate_stale(root, &config, &fp);

    // Build kernel and bootloader in parallel
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

    let initrd_bytes = build_and_assemble(root, &config, profile, &rustflags, &path_env, &[]);

    // Mirror initrd to target/initrd/ for inspection
    let initrd_dir = root.join("target/initrd");
    if initrd_dir.exists() {
        fs::remove_dir_all(&initrd_dir).expect("Failed to clean initrd dir");
    }
    // Re-parse the initrd files for mirroring (we only have the raw bytes now).
    // Instead, we mirror before creating the initrd. But build_and_assemble already
    // created it. For simplicity, skip the mirror or refactor later.
    // TODO: The mirror was a debug aid. Skipping for now since initrd is a bcachefs image.

    let disk_bytes = image::create_boot_image(&initrd_bytes, profile);
    fs::write(root.join("target/bootable.img"), disk_bytes).expect("Failed to write image");

    // Create empty NVMe disk image if it doesn't already exist
    let nvme_path = root.join("target/nvme.img");
    if !nvme_path.exists() {
        let nvme_bytes = vec![0u8; 1024 * 1024 * 1024];
        fs::write(&nvme_path, nvme_bytes).expect("Failed to write NVMe image");
    }
}

/// Build a test image from a system.toml config. Returns the raw disk image bytes.
/// The caller writes these to a temp file for QEMU.
pub fn build_test_image(
    root: &Path,
    config_path: &Path,
    debug_wait: bool,
    extra_files: &[(String, Vec<u8>)],
) -> Vec<u8> {
    crate::toolchain::ensure(root, false);
    let path_env = toolchain::path_with_toyos_ld(root);
    let rustflags = "-Dwarnings";
    let config = parse_config(config_path);
    let fp = external_fingerprint(root);

    invalidate_stale(root, &config, &fp);

    let init_programs = config.init.join(";");
    build_kernel(root, debug_wait, false, rustflags, &path_env);
    build_bootloader(root, false, rustflags, &path_env, &init_programs);

    let initrd_bytes = build_and_assemble(root, &config, "debug", rustflags, &path_env, extra_files);

    let kernel_bytes = fs::read(root.join("kernel/target/x86_64-unknown-none/debug/kernel"))
        .expect("Failed to read kernel");
    let bl_bytes =
        fs::read(root.join("bootloader/target/x86_64-unknown-uefi/debug/bootloader.efi"))
            .expect("Failed to read bootloader");

    let esp = image::create_fat_volume(&kernel_bytes, &bl_bytes, &initrd_bytes);
    image::create_gpt_disk(esp)
}

/// Build all binaries in a multi-binary crate. Returns vec of (binary_name, bytes).
/// Also builds any cdylib subcrates and includes their .so files.
pub fn build_toyos_bins(root: &Path, crate_path: &Path) -> Vec<(String, Vec<u8>)> {
    crate::toolchain::ensure(root, false);
    let path_env = toolchain::path_with_toyos_ld(root);
    let fp = external_fingerprint(root);

    ensure_fresh(crate_path, &fp);
    for entry in fs::read_dir(crate_path).into_iter().flatten().flatten() {
        let sub_path = entry.path();
        if sub_path.is_dir() && sub_path.join("Cargo.toml").exists() {
            ensure_fresh(&sub_path, &fp);
        }
    }

    let mut results = Vec::new();

    // Build cdylib subcrates first
    let mut lib_search_dirs = Vec::new();
    for entry in fs::read_dir(crate_path).unwrap() {
        let entry = entry.unwrap();
        let sub_path = entry.path();
        if !sub_path.is_dir() {
            continue;
        }
        let cargo_toml = sub_path.join("Cargo.toml");
        if !cargo_toml.exists() {
            continue;
        }
        let toml_text = fs::read_to_string(&cargo_toml).unwrap();
        if !toml_text.contains("cdylib") {
            continue;
        }

        let lib_name = sub_path.file_name().unwrap().to_str().unwrap();
        eprintln!("[build] Building cdylib subcrate: {lib_name}");
        cargo_build_toyos(&sub_path, &[], "", &path_env);

        let lib_out = sub_path.join("target/x86_64-unknown-toyos/debug");
        lib_search_dirs.push(lib_out.clone());

        for so_entry in fs::read_dir(&lib_out).unwrap() {
            let so_entry = so_entry.unwrap();
            let name = so_entry.file_name().to_str().unwrap().to_string();
            if name.ends_with(".so") {
                let data = fs::read(so_entry.path()).unwrap();
                results.push((name, data));
            }
        }
    }

    // Build test binaries
    let mut rustflags = String::new();
    for dir in &lib_search_dirs {
        rustflags.push_str(&format!("-L {} ", dir.display()));
    }
    cargo_build_toyos(crate_path, &["--bins"], &rustflags, &path_env);

    let bin_dir = crate_path.join("target/x86_64-unknown-toyos/debug");
    let bin_src = crate_path.join("src/bin");
    if bin_src.exists() {
        for entry in fs::read_dir(&bin_src).unwrap() {
            let entry = entry.unwrap();
            let name = entry
                .file_name()
                .to_str()
                .unwrap()
                .strip_suffix(".rs")
                .unwrap()
                .to_string();
            let binary = bin_dir.join(&name);
            if binary.exists() {
                let data = fs::read(&binary).unwrap();
                results.push((name, data));
            }
        }
    }

    results
}

// --- Internal helpers ---

fn collect_hosted_rustc(root: &Path, initrd_files: &mut Vec<(String, Vec<u8>)>) {
    let sysroot = root.join("rust/build/x86_64-unknown-toyos/stage2");
    assert!(
        sysroot.exists(),
        "Hosted rustc sysroot missing: {}",
        sysroot.display()
    );

    let rustc = sysroot.join("bin/rustc");
    assert!(
        rustc.exists(),
        "Hosted rustc binary missing: {}",
        rustc.display()
    );
    initrd_files.push(("bin/rustc".to_string(), fs::read(&rustc).unwrap()));

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
