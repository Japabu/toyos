mod assets;
mod build;
mod image;
mod libc;
mod qemu;
mod stamps;
mod toolchain;

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let args: Vec<String> = env::args().collect();
    let debug = args.iter().any(|a| a == "--debug");
    let release = args.iter().any(|a| a == "--release");
    let build_only = args.iter().any(|a| a == "--build-only");

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    env::set_current_dir(&root).expect("Failed to cd to project root");

    // Auto-init git submodules
    if !root.join("rust/compiler").exists() {
        eprintln!("Initializing git submodules...");
        let status = Command::new("git")
            .args(["submodule", "update", "--init"])
            .status()
            .expect("Failed to run git");
        assert!(status.success(), "git submodule update failed");
    }

    // Ensure toolchain is up to date
    let toolchain_changed = toolchain::ensure(&root);

    // Build everything
    build::build(&root, debug, release, toolchain_changed);

    if !build_only {
        qemu::launch(debug);
    }
}
