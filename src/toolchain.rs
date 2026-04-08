use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::stamps;

#[allow(dead_code)]
pub struct ChangeSet {
    pub std_rebuilt: bool,
    pub linker_changed: bool,
    pub compiler_changed: bool,
}

/// Ensure the toolchain is up to date. Returns a ChangeSet describing what changed.
pub fn ensure(root: &Path, force_rebuild: bool) -> ChangeSet {
    let rust_dir = root.join("rust");
    let stamps_dir = root.join("target/stamps");
    fs::create_dir_all(&stamps_dir).ok();

    // Check if the toolchain exists at all
    let toolchain_exists = Command::new("rustup")
        .args(["run", "toyos", "rustc", "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let compiler_stamp = stamps_dir.join("compiler.stamp");
    let std_stamp = stamps_dir.join("std.stamp");
    let abi_stamp = stamps_dir.join("abi.stamp");
    let net_stamp = stamps_dir.join("net.stamp");
    let linker_stamp = stamps_dir.join("linker.stamp");
    let compiler_changed = stamps::dir_changed(&rust_dir.join("compiler"), &compiler_stamp);
    let std_changed = stamps::dir_changed(&rust_dir.join("library"), &std_stamp);
    // toyos-abi and toyos are dependencies of std — changes require an std rebuild
    let abi_changed = stamps::dir_changed(&root.join("toyos-abi/src"), &abi_stamp);
    let net_changed = stamps::dir_changed(&root.join("toyos/src"), &net_stamp);
    let linker_changed = stamps::dir_changed(&root.join("toyos-ld/src"), &linker_stamp);

    // Ensure toyos-ld is built (needed as cross-linker for bootstrap and all builds)
    let toyos_ld = toyos_ld_binary(root);
    if linker_changed || !toyos_ld.exists() {
        eprintln!("Building toyos-ld...");
        build_toyos_ld(root);
        stamps::write_dir_stamp(&root.join("toyos-ld/src"), &linker_stamp);
    }

    // Ensure toyos-cc is built as a host tool (used by doom's build.rs)
    let cc_stamp = stamps_dir.join("toyos-cc.stamp");
    let cc_src_changed = stamps::dir_changed(&root.join("toyos-cc/src"), &cc_stamp);
    let cc_inc_stamp = stamps_dir.join("toyos-cc-include.stamp");
    let cc_inc_changed = stamps::dir_changed(&root.join("toyos-cc/include"), &cc_inc_stamp);
    let toyos_cc = toyos_cc_binary(root);
    if cc_src_changed || cc_inc_changed || !toyos_cc.exists() {
        eprintln!("Building toyos-cc...");
        build_toyos_cc(root);
        stamps::write_dir_stamp(&root.join("toyos-cc/src"), &cc_stamp);
        stamps::write_dir_stamp(&root.join("toyos-cc/include"), &cc_inc_stamp);
    }

    let rebuilt = if !toolchain_exists || compiler_changed || force_rebuild {
        // Full bootstrap needed
        eprintln!("Building full toolchain (this takes a while on first run)...");
        full_bootstrap(root, &rust_dir);
        stamps::write_dir_stamp(&rust_dir.join("compiler"), &compiler_stamp);
        stamps::write_dir_stamp(&rust_dir.join("library"), &std_stamp);
        stamps::write_dir_stamp(&root.join("toyos-abi/src"), &abi_stamp);
        stamps::write_dir_stamp(&root.join("toyos/src"), &net_stamp);
        true
    } else if std_changed || abi_changed || net_changed {
        // Fast path: only rebuild std
        eprintln!("Rebuilding std (fast path)...");
        rebuild_std(root, &rust_dir);
        stamps::write_dir_stamp(&rust_dir.join("library"), &std_stamp);
        stamps::write_dir_stamp(&root.join("toyos-abi/src"), &abi_stamp);
        stamps::write_dir_stamp(&root.join("toyos/src"), &net_stamp);
        true
    } else {
        false
    };

    // Ensure hosted rustc (ToyOS binary) is up to date.
    // Only invalidate when the compiler changed — std-only changes are picked up
    // by rebuild_std which updates the sysroot libraries that hosted rustc uses.
    let hosted_stamp = stamps_dir.join("hosted-rustc.stamp");
    if compiler_changed || force_rebuild {
        let _ = fs::remove_file(&hosted_stamp);
    }
    let hosted_rustc = rust_dir.join("build/x86_64-unknown-toyos/stage2/bin/rustc");
    if !hosted_stamp.exists() || !hosted_rustc.exists() {
        let toyos_ld = toyos_ld_binary(root);
        build_hosted_rustc(&rust_dir, &toyos_ld);
        assert!(hosted_rustc.exists(), "Failed to build hosted rustc");
        fs::write(&hosted_stamp, "").unwrap();
    }

    // Link the toolchain so cargo can use it
    let host = host_triple();
    let stage2 = rust_dir.join(format!("build/{host}/stage2"));
    run("rustup", &["toolchain", "link", "toyos", stage2.to_str().unwrap()]);

    // Ensure the ToyOS sysroot has host target libraries so proc-macros can compile.
    // This must happen before any cargo builds use the toolchain, otherwise cargo
    // may fingerprint an incomplete sysroot on first run.
    ensure_host_target_in_sysroot(root);

    // Ensure toyos-libc is built and installed in the sysroot.
    // Invalidate stamp when toolchain was rebuilt (sysroot rlibs replaced).
    if rebuilt {
        let _ = fs::remove_file(stamps_dir.join("toyos-libc.stamp"));
    }
    crate::libc::ensure(root, &rust_dir);

    ChangeSet {
        std_rebuilt: rebuilt,
        linker_changed,
        compiler_changed,
    }
}

fn full_bootstrap(root: &Path, rust_dir: &Path) {
    let toyos_ld = toyos_ld_binary(root);

    // Ensure library/backtrace is checked out — std depends on it.
    // Other rust submodules (llvm, docs, cargo) are handled by bootstrap on demand.
    crate::ensure_submodule(rust_dir, "library/backtrace");

    // Write bootstrap.toml — ToyOS as target only, not host (fast rebuilds)
    let host = host_triple();
    write_config(rust_dir, &host, &toyos_ld, false);

    // Clean cached std for all ToyOS targets so bootstrap picks up compiler changes
    // (e.g. target spec changes like default_uwtable that affect codegen).
    for target in ["x86_64-unknown-toyos", "x86_64-unknown-none", "x86_64-unknown-uefi"] {
        let stage1_std = rust_dir.join(format!("build/{host}/stage1-std/{target}"));
        if stage1_std.exists() {
            fs::remove_dir_all(&stage1_std).ok();
        }
    }

    // Run full bootstrap
    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2", "--warnings", "warn"])
        .env("BOOTSTRAP_SKIP_TARGET_SANITY", "1")
        .current_dir(rust_dir)
        .status()
        .expect("Failed to run x build");

    if !status.success() {
        // Check if essential artifacts exist (rustdoc for ToyOS may fail, that's ok)
        let stage2 = rust_dir.join(format!("build/{host}/stage2"));
        assert!(
            stage2.join("bin/rustc").exists(),
            "Toolchain build failed and rustc artifacts are missing"
        );
        eprintln!("Note: some targets may have failed to link (expected), but rustc built successfully.");
    }
}

fn rebuild_std(root: &Path, rust_dir: &Path) {
    // Ensure cross-only config (no hosted rustc) — if a previous hosted build
    // was interrupted, bootstrap.toml may still have ToyOS as host.
    let toyos_ld = toyos_ld_binary(root);
    write_config(rust_dir, &host_triple(), &toyos_ld, false);

    // Clean bootstrap's cached std for ToyOS targets so it picks up toyos-abi changes.
    // Bootstrap caches compiled std artifacts and won't notice external dep changes.
    let host = host_triple();
    for target in ["x86_64-unknown-toyos", "x86_64-unknown-none", "x86_64-unknown-uefi"] {
        let stage1_std = rust_dir.join(format!("build/{host}/stage1-std/{target}"));
        if stage1_std.exists() {
            fs::remove_dir_all(&stage1_std).ok();
        }
    }

    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2", "library", "--warnings", "warn"])
        .env("BOOTSTRAP_SKIP_TARGET_SANITY", "1")
        .current_dir(rust_dir)
        .status()
        .expect("Failed to run x build library");
    assert!(status.success(), "std rebuild failed");
}

fn build_hosted_rustc(rust_dir: &Path, toyos_ld: &Path) {
    eprintln!("Building ToyOS-hosted rustc...");
    write_config(rust_dir, &host_triple(), toyos_ld, true);

    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2", "--warnings", "warn"])
        .env("BOOTSTRAP_SKIP_TARGET_SANITY", "1")
        .current_dir(rust_dir)
        .status()
        .expect("Failed to run x build for hosted rustc");

    // rustdoc for ToyOS may fail to link (expected), but rustc + librustc_driver must exist
    let toyos_stage2 = rust_dir.join("build/x86_64-unknown-toyos/stage2");
    assert!(
        toyos_stage2.join("bin/rustc").exists(),
        "Hosted rustc build failed: {} missing", toyos_stage2.join("bin/rustc").display()
    );
    assert!(
        fs::read_dir(toyos_stage2.join("lib"))
            .map(|d| d.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("librustc_driver")))
            .unwrap_or(false),
        "Hosted rustc build failed: librustc_driver*.so missing"
    );
    if !status.success() {
        eprintln!("Note: rustdoc for ToyOS failed to link (expected), but rustc built successfully.");
    }
    // No config restore needed — full_bootstrap and rebuild_std write the
    // cross-only config before they run, so the next non-hosted build
    // always starts with the correct config regardless of what's on disk.
}

fn write_config(rust_dir: &Path, host: &str, toyos_ld: &Path, with_hosted_rustc: bool) {
    let linker = toyos_ld.display();
    let host_line = if with_hosted_rustc {
        format!("host = [\"{host}\", \"x86_64-unknown-toyos\"]")
    } else {
        format!("host = [\"{host}\"]")
    };
    let codegen_backends = if with_hosted_rustc {
        "\ncodegen-backends = [\"cranelift\"]"
    } else {
        ""
    };
    let config = format!(
        r#"change-id = "ignore"
profile = "compiler"

[build]
{host_line}
target = ["{host}", "x86_64-unknown-toyos", "x86_64-unknown-none", "x86_64-unknown-uefi"]

[rust]
incremental = true
lld = false

[target.x86_64-unknown-toyos]
linker = "{linker}"{codegen_backends}

"#
    );
    fs::write(rust_dir.join("bootstrap.toml"), config).unwrap();
}

/// Path to the host toyos-ld binary (stable location, never wiped by sysroot rebuilds).
pub fn toyos_ld_binary(root: &Path) -> PathBuf {
    let host = host_triple();
    root.join(format!("toyos-ld/target/{host}/release/toyos-ld"))
}

fn build_toyos_ld(root: &Path) {
    let host = host_triple();
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", &host])
        .current_dir(root.join("toyos-ld"))
        .status()
        .expect("Failed to build toyos-ld");
    assert!(status.success(), "toyos-ld build failed");
}

/// Path to the host toyos-cc binary.
pub fn toyos_cc_binary(root: &Path) -> PathBuf {
    let host = host_triple();
    root.join(format!("toyos-cc/target/{host}/release/toyos-cc"))
}

fn build_toyos_cc(root: &Path) {
    let host = host_triple();
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", &host])
        .current_dir(root.join("toyos-cc"))
        .status()
        .expect("Failed to build toyos-cc");
    assert!(status.success(), "toyos-cc build failed");
}

pub fn host_triple() -> String {
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

/// PATH with toyos-ld's build directory prepended, so rustc finds it for linking.
pub fn path_with_toyos_ld(root: &Path) -> String {
    let host = host_triple();
    let ld_dir = root.join(format!("toyos-ld/target/{host}/release"));
    match std::env::var("PATH") {
        Ok(p) => format!("{}:{p}", ld_dir.display()),
        Err(_) => ld_dir.display().to_string(),
    }
}

fn ensure_host_target_in_sysroot(root: &Path) {
    let host = host_triple();
    let toyos_sysroot = root.join("rust/build/x86_64-unknown-toyos/stage2/lib/rustlib");
    if !toyos_sysroot.exists() {
        return;
    }
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
    let source = Path::new(stable_sysroot).join("lib/rustlib").join(&host);
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
}

fn run(cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("Failed to run {cmd}: {e}"));
    assert!(status.success(), "{cmd} failed");
}
