use std::fs;
use std::path::Path;
use std::process::Command;

use crate::stamps;

/// Ensure the toolchain is up to date. Returns true if std was rebuilt
/// (callers must clean userland targets to avoid stale artifacts).
pub fn ensure(root: &Path, force_rebuild: bool) -> bool {
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
    let compiler_changed = stamps::dir_changed(&rust_dir.join("compiler"), &compiler_stamp);
    let std_changed = stamps::dir_changed(&rust_dir.join("library"), &std_stamp);
    // toyos-abi is a dependency of std — changes to it require an std rebuild
    let abi_changed = stamps::dir_changed(&root.join("toyos-abi/src"), &abi_stamp);

    let rebuilt = if !toolchain_exists || compiler_changed || force_rebuild {
        // Full bootstrap needed
        eprintln!("Building full toolchain (this takes a while on first run)...");
        full_bootstrap(root, &rust_dir);
        stamps::write_dir_stamp(&rust_dir.join("compiler"), &compiler_stamp);
        stamps::write_dir_stamp(&rust_dir.join("library"), &std_stamp);
        stamps::write_dir_stamp(&root.join("toyos-abi/src"), &abi_stamp);
        true
    } else if std_changed || abi_changed {
        // Fast path: only rebuild std
        eprintln!("Rebuilding std (fast path)...");
        rebuild_std(&rust_dir);
        stamps::write_dir_stamp(&rust_dir.join("library"), &std_stamp);
        stamps::write_dir_stamp(&root.join("toyos-abi/src"), &abi_stamp);
        true
    } else {
        eprintln!("Toolchain up to date.");
        false
    };

    // Ensure toyos-ld is built and installed
    ensure_toyos_ld(root, &rust_dir);

    // Ensure hosted rustc (ToyOS binary) exists — rebuild if missing
    let hosted_rustc = rust_dir.join("build/x86_64-unknown-toyos/stage2/bin/rustc");
    if !hosted_rustc.exists() {
        let host = host_triple();
        let toyos_ld = root.join(format!("userland/target/{host}/release/toyos-ld"));
        build_hosted_rustc(&rust_dir, &toyos_ld);
        assert!(hosted_rustc.exists(), "Failed to build hosted rustc");
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

    rebuilt
}

fn full_bootstrap(root: &Path, rust_dir: &Path) {
    // Build toyos-ld first (needed as cross-linker)
    let toyos_ld = build_toyos_ld(root);

    // Write bootstrap.toml — ToyOS as target only, not host (fast rebuilds)
    write_config(rust_dir, &host_triple(), &toyos_ld, false);

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
        let host = host_triple();
        let stage2 = rust_dir.join(format!("build/{host}/stage2"));
        assert!(
            stage2.join("bin/rustc").exists(),
            "Toolchain build failed and rustc artifacts are missing"
        );
        eprintln!("Note: some targets may have failed to link (expected), but rustc built successfully.");
    }

    // Now build hosted rustc (ToyOS as host) if compiler changed
    build_hosted_rustc(rust_dir, &toyos_ld);
}

fn rebuild_std(rust_dir: &Path) {
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

    // Restore config without ToyOS as host for future fast rebuilds
    write_config(rust_dir, &host_triple(), toyos_ld, false);
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

[target.x86_64-unknown-toyos]
linker = "{linker}"{codegen_backends}

"#
    );
    fs::write(rust_dir.join("bootstrap.toml"), config).unwrap();
}

fn ensure_toyos_ld(root: &Path, rust_dir: &Path) {
    let ld_src = root.join("userland/toyos-ld/src");
    let stamp = root.join("target/stamps/toyos-ld.stamp");
    let host = host_triple();
    let stage2 = rust_dir.join(format!("build/{host}/stage2"));
    let sysroot_bin = stage2.join(format!("lib/rustlib/{host}/bin"));
    let dest = sysroot_bin.join("toyos-ld");

    // Rebuild if source changed OR if the installed binary is missing (e.g. after std rebuild clears sysroot)
    let source_changed = stamps::dir_changed(&ld_src, &stamp);
    let missing = !dest.exists();

    if !source_changed && !missing {
        return;
    }

    let toyos_ld = if source_changed {
        eprintln!("Building toyos-ld...");
        build_toyos_ld(root)
    } else {
        eprintln!("Reinstalling toyos-ld into sysroot...");
        // toyos-ld is a workspace member, so output goes to the workspace target dir
        root.join(format!("userland/target/{host}/release/toyos-ld"))
    };

    // Install into sysroot
    fs::create_dir_all(&sysroot_bin)
        .unwrap_or_else(|e| panic!("Failed to create {}: {e}", sysroot_bin.display()));
    fs::copy(&toyos_ld, &dest)
        .unwrap_or_else(|e| panic!("Failed to copy toyos-ld to {}: {e}", dest.display()));

    stamps::write_dir_stamp(&ld_src, &stamp);
}

fn build_toyos_ld(root: &Path) -> std::path::PathBuf {
    let toyos_ld_dir = root.join("userland/toyos-ld");
    let host = host_triple();
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", &host])
        .current_dir(&toyos_ld_dir)
        .status()
        .expect("Failed to build toyos-ld");
    assert!(status.success(), "toyos-ld build failed");
    // toyos-ld is a workspace member, so output goes to the workspace target dir
    root.join(format!("userland/target/{host}/release/toyos-ld"))
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

