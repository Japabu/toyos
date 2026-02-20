use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let toolchain_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let rust_dir = toolchain_dir.join("rust");
    let patches_dir = toolchain_dir.join("patches");

    let commit = rustc_commit_hash();
    let host = host_triple();

    // Step 1: Clone Rust source
    clone_rust(&rust_dir, &commit);

    // Step 2: Apply patches
    println!("Applying patches...");
    apply_patches(&patches_dir, &rust_dir);

    // Step 3: Write config.toml
    write_config(&rust_dir, &host);

    // Step 4: Build
    println!("Building toolchain (this takes a while on first run)...");
    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2"])
        .current_dir(&rust_dir)
        .status()
        .expect("Failed to run x build");
    assert!(status.success(), "Toolchain build failed");

    // Step 5: Link the toolchain
    let stage2 = find_stage2(&rust_dir);
    run("rustup", &["toolchain", "link", "toyos", stage2.to_str().unwrap()]);

    println!();
    println!("Done! Toolchain 'toyos' is ready.");
    println!("Build userland with:");
    println!("  cd userland/hello && cargo +toyos build --target x86_64-unknown-toyos");
}

// ---------------------------------------------------------------------------
// Clone
// ---------------------------------------------------------------------------

fn clone_rust(rust_dir: &Path, commit: &str) {
    let marker = rust_dir.join(".toyos-commit");
    if marker.exists() && fs::read_to_string(&marker).unwrap().trim() == commit {
        println!("Rust source up to date ({commit}).");
        return;
    }

    if rust_dir.exists() {
        println!("Removing old Rust source...");
        fs::remove_dir_all(rust_dir).unwrap();
    }

    println!("Cloning Rust at {commit} (shallow)...");
    fs::create_dir_all(rust_dir).unwrap();
    git(rust_dir, &["init"]);
    git(rust_dir, &["remote", "add", "origin", "https://github.com/rust-lang/rust.git"]);
    git(rust_dir, &["fetch", "--depth", "1", "origin", commit]);
    git(rust_dir, &["checkout", "FETCH_HEAD"]);
    fs::write(&marker, commit).unwrap();
}

// ---------------------------------------------------------------------------
// Patches
// ---------------------------------------------------------------------------

/// Walk the patches/ directory. For each file:
/// - `.rs` files are copied to the same relative path in the rust tree
/// - `.patch` files are applied with `git apply`
fn apply_patches(patches_dir: &Path, rust_dir: &Path) {
    // Reset tracked files so patches are idempotent (preserves build/)
    git(rust_dir, &["checkout", "--", "."]);
    walk_patches(patches_dir, patches_dir, rust_dir);
}

fn walk_patches(base: &Path, dir: &Path, rust_dir: &Path) {
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            walk_patches(base, &path, rust_dir);
        } else if let Some(ext) = path.extension() {
            let rel = path.strip_prefix(base).unwrap();
            if ext == "rs" {
                let dest = rust_dir.join(rel);
                fs::create_dir_all(dest.parent().unwrap()).unwrap();
                fs::copy(&path, &dest).unwrap();
                println!("  Copied: {}", rel.display());
            } else if ext == "patch" {
                let status = Command::new("git")
                    .args(["apply", "--verbose", path.to_str().unwrap()])
                    .current_dir(rust_dir)
                    .status()
                    .unwrap_or_else(|e| panic!("Failed to run git apply: {e}"));
                assert!(status.success(), "git apply failed for {}", rel.display());
                println!("  Applied: {}", rel.display());
            }
        }
    }
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

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("Failed to run git");
    assert!(status.success(), "git {:?} failed", args);
}

fn run(cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("Failed to run {cmd}: {e}"));
    assert!(status.success(), "{cmd} failed");
}

fn rustc_commit_hash() -> String {
    let output = Command::new("rustc")
        .args(["--version", "--verbose"])
        .output()
        .expect("Failed to run rustc");
    let text = String::from_utf8(output.stdout).unwrap();
    text.lines()
        .find(|l| l.starts_with("commit-hash:"))
        .map(|l| l.strip_prefix("commit-hash: ").unwrap().to_string())
        .expect("Could not determine rustc commit hash")
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
