use std::path::PathBuf;
use std::process::Command;

fn main() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let libc_dir = repo_root.join("userland/libc");

    // Track all source directories that affect the test build.
    // Directories always appear "changed" to Cargo, so pointing rerun-if-changed
    // at them ensures the build script re-runs every time.
    for dir in [
        "userland/libc/src",
        "userland/libc/include",
        "toyos-ld/src",
        "toyos-cc/src",
        "toyos-abi/src",
        "kernel/src",
        "src",
    ] {
        println!("cargo:rerun-if-changed={}", repo_root.join(dir).display());
    }

    // Build toyos-libc for x86_64-unknown-toyos
    let toyos_archive = build_libc_toyos(&libc_dir);
    println!("cargo:rustc-env=TOYOS_LIBC_TOYOS={}", toyos_archive.display());
}

fn build_libc_toyos(libc_dir: &std::path::Path) -> PathBuf {
    let target = "x86_64-unknown-toyos";
    let target_dir = libc_dir.join("target");
    // Remove all CARGO_* env vars from the outer build to prevent interference
    let mut cmd = Command::new("cargo");
    for (key, _) in std::env::vars() {
        if key.starts_with("CARGO") || key == "RUSTC" || key == "RUSTFLAGS" {
            cmd.env_remove(&key);
        }
    }
    let output = cmd
        .env("RUSTUP_TOOLCHAIN", "toyos")
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
