use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let toolchain_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = toolchain_dir.parent().unwrap();
    let rust_dir = root_dir.join("rust");

    assert!(
        rust_dir.join("compiler").exists(),
        "Rust submodule not found at {}. Run: git submodule update --init",
        rust_dir.display()
    );

    let host = host_triple();

    // Step 1: Build toyos-ld for the host (used as cross-linker)
    println!("Building toyos-ld for host...");
    let toyos_ld = build_toyos_ld(root_dir);
    println!("  Built: {}", toyos_ld.display());

    // Step 2: Write bootstrap.toml
    write_config(&rust_dir, &host, &toyos_ld);

    // Step 3: Build full toolchain via bootstrap
    //
    // ToyOS is listed as both host and target, so bootstrap builds the complete
    // compiler (rustc + cranelift codegen backend) for ToyOS, plus std for both
    // host and ToyOS targets.
    println!("Building toolchain (this takes a while on first run)...");
    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2", "--warnings", "warn"])
        .env("BOOTSTRAP_SKIP_TARGET_SANITY", "1")
        .current_dir(&rust_dir)
        .status()
        .expect("Failed to run x build");
    assert!(status.success(), "Toolchain build failed");

    // Step 4: Link the toolchain so cargo can use it
    let stage2 = find_stage2(&rust_dir);
    run("rustup", &["toolchain", "link", "toyos", stage2.to_str().unwrap()]);

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

fn write_config(rust_dir: &Path, host: &str, toyos_ld: &Path) {
    let linker = toyos_ld.display();
    let config = format!(
        r#"change-id = "ignore"
profile = "compiler"

[build]
host = ["{host}", "x86_64-unknown-toyos"]
target = ["{host}", "x86_64-unknown-toyos"]

[rust]
incremental = true

[target.x86_64-unknown-toyos]
linker = "{linker}"
codegen-backends = ["cranelift"]
"#
    );
    fs::write(rust_dir.join("bootstrap.toml"), config).unwrap();
    println!("  Wrote: bootstrap.toml");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_toyos_ld(root_dir: &Path) -> PathBuf {
    let toyos_ld_dir = root_dir.join("userland/toyos-ld");
    let host = host_triple();
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", &host])
        .current_dir(&toyos_ld_dir)
        .status()
        .expect("Failed to build toyos-ld");
    assert!(status.success(), "toyos-ld build failed");
    toyos_ld_dir.join(format!("target/{host}/release/toyos-ld"))
}

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
