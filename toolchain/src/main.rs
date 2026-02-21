use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let toolchain_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let rust_dir = toolchain_dir.parent().unwrap().join("rust");

    assert!(
        rust_dir.join("compiler").exists(),
        "Rust submodule not found at {}. Run: git submodule update --init",
        rust_dir.display()
    );

    let host = host_triple();

    // Step 1: Write config.toml
    write_config(&rust_dir, &host);

    // Step 2: Build
    println!("Building toolchain (this takes a while on first run)...");
    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2"])
        .current_dir(&rust_dir)
        .status()
        .expect("Failed to run x build");
    assert!(status.success(), "Toolchain build failed");

    // Step 3: Link the toolchain
    let stage2 = find_stage2(&rust_dir);
    run("rustup", &["toolchain", "link", "toyos", stage2.to_str().unwrap()]);

    // Step 4: Build libtoyos and install to sysroot
    println!("Building libtoyos...");
    let libtoyos_dir = toolchain_dir.parent().unwrap().join("libtoyos");
    let status = Command::new("cargo")
        .args(["+toyos", "build", "--release", "--target", "x86_64-unknown-toyos"])
        .current_dir(&libtoyos_dir)
        .status()
        .expect("Failed to build libtoyos");
    assert!(status.success(), "libtoyos build failed");

    let sysroot_lib = stage2.join("lib/rustlib/x86_64-unknown-toyos/lib");
    fs::create_dir_all(&sysroot_lib).unwrap();
    fs::copy(
        libtoyos_dir.join("target/x86_64-unknown-toyos/release/libtoyos.so"),
        sysroot_lib.join("libtoyos.so"),
    )
    .expect("Failed to copy libtoyos.so to sysroot");
    println!("  Installed libtoyos.so to sysroot");

    // Step 5: Write stamp so bootable/build.rs knows to rebuild userland
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    fs::write(toolchain_dir.join(".sysroot-stamp"), stamp).unwrap();

    println!();
    println!("Done! Toolchain 'toyos' is ready.");
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn write_config(rust_dir: &Path, host: &str) {
    let config = format!(
        r#"profile = "compiler"

[build]
host = ["{host}"]
target = ["{host}", "x86_64-unknown-toyos"]

[rust]
incremental = true
lld = true
"#
    );
    fs::write(rust_dir.join("config.toml"), config).unwrap();
    println!("  Wrote: config.toml");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_stage2(rust_dir: &Path) -> PathBuf {
    let build_dir = rust_dir.join("build");
    for entry in fs::read_dir(&build_dir).expect("build/ not found") {
        let path = entry.unwrap().path();
        let stage2 = path.join("stage2");
        if stage2.exists() {
            return stage2;
        }
    }
    panic!("stage2 sysroot not found in {}", build_dir.display());
}

fn run(cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("Failed to run {cmd}: {e}"));
    assert!(status.success(), "{cmd} failed");
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
