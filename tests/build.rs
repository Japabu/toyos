use std::path::PathBuf;
use std::process::Command;

fn main() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let libc_dir = repo_root.join("userland/libc");

    // Build toyos-libc for host target
    let host_target = host_target();
    let host_archive = build_libc(&libc_dir, host_target);
    println!("cargo:rustc-env=TOYOS_LIBC_HOST={}", host_archive.display());

    // Build toyos-libc for x86_64-apple-darwin (cross target for Rosetta)
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let x86_archive = build_libc(&libc_dir, "x86_64-apple-darwin");
        println!("cargo:rustc-env=TOYOS_LIBC_X86_64_APPLE={}", x86_archive.display());
    }

    // Build toyos-libc for x86_64-unknown-toyos
    let ld_binary = build_toyos_ld(&repo_root);
    let toyos_archive = build_libc_toyos(&libc_dir, &ld_binary);
    println!("cargo:rustc-env=TOYOS_LIBC_TOYOS={}", toyos_archive.display());
    println!("cargo:rustc-env=TOYOS_LD_BINARY={}", ld_binary.display());

    // Re-run if libc source changes
    println!("cargo:rerun-if-changed={}", libc_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", libc_dir.join("Cargo.toml").display());

    // Re-run if the built artifacts disappear (e.g. libc target/ was cleaned)
    println!("cargo:rerun-if-changed={}", host_archive.display());
    println!("cargo:rerun-if-changed={}", toyos_archive.display());
}

fn host_target() -> &'static str {
    if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else if cfg!(target_arch = "x86_64") && cfg!(target_os = "macos") {
        "x86_64-apple-darwin"
    } else if cfg!(target_arch = "x86_64") && cfg!(target_os = "linux") {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(target_arch = "aarch64") && cfg!(target_os = "linux") {
        "aarch64-unknown-linux-gnu"
    } else {
        panic!("unsupported host")
    }
}

fn build_libc(libc_dir: &std::path::Path, target: &str) -> PathBuf {
    let target_dir = libc_dir.join("target");
    let output = Command::new("cargo")
        .args(["+nightly", "rustc", "--release", "--target", target, "--crate-type", "staticlib"])
        .arg("--manifest-path")
        .arg(libc_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .output()
        .expect("failed to build toyos-libc");
    assert!(
        output.status.success(),
        "toyos-libc build for {target} failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let path = target_dir.join(format!("{target}/release/libtoyos_libc.a"));
    assert!(path.exists(), "expected staticlib at {}", path.display());
    path
}

fn build_toyos_ld(repo_root: &std::path::Path) -> PathBuf {
    let ld_dir = repo_root.join("userland/toyos-ld");
    let output = Command::new("rustc")
        .args(["--version", "--verbose"])
        .output()
        .expect("Failed to run rustc");
    let text = String::from_utf8(output.stdout).unwrap();
    let host = text.lines()
        .find(|l| l.starts_with("host:"))
        .map(|l| l.strip_prefix("host: ").unwrap().to_string())
        .expect("Could not determine host triple");
    let output = Command::new("cargo")
        .args(["build", "--release", "--target", &host])
        .current_dir(&ld_dir)
        .output()
        .expect("failed to build toyos-ld");
    assert!(
        output.status.success(),
        "toyos-ld build failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    ld_dir.join(format!("target/{host}/release/toyos-ld"))
        .canonicalize()
        .expect("toyos-ld binary not found")
}

fn build_libc_toyos(libc_dir: &std::path::Path, ld_binary: &std::path::Path) -> PathBuf {
    let target = "x86_64-unknown-toyos";
    let target_dir = libc_dir.join("target");
    let output = Command::new("cargo")
        .env("RUSTUP_TOOLCHAIN", "toyos")
        .env("CARGO_TARGET_X86_64_UNKNOWN_TOYOS_LINKER", ld_binary.to_str().unwrap())
        .env_remove("RUSTC")
        .args(["rustc", "--release", "--target", target, "--crate-type", "staticlib"])
        .arg("--manifest-path")
        .arg(libc_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .output()
        .expect("failed to build toyos-libc for toyos");
    assert!(
        output.status.success(),
        "toyos-libc build for {target} failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let path = target_dir.join(format!("{target}/release/libtoyos_libc.a"));
    assert!(path.exists(), "expected staticlib at {}", path.display());
    path
}
