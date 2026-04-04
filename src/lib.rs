pub mod assets;
pub mod build;
pub mod image;
pub mod libc;
pub mod stamps;
pub mod toolchain;

use std::path::Path;
use std::process::Command;

/// Ensure all git submodules in `repo_dir` are checked out.
/// Detects corrupted partial checkouts (`.git` exists but no content) and uses
/// `--force` only when needed. Initializes each missing submodule individually.
pub fn ensure_submodules(repo_dir: &Path) {
    let output = Command::new("git")
        .args(["config", "--file", ".gitmodules", "--get-regexp", r"submodule\..*\.path"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to parse .gitmodules");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(path) = line.split_whitespace().nth(1) {
            ensure_submodule(repo_dir, path);
        }
    }
}

/// Ensure a single git submodule is checked out.
pub fn ensure_submodule(repo_dir: &Path, path: &str) {
    let dir = repo_dir.join(path);
    let entry_count = std::fs::read_dir(&dir).map_or(0, |d| d.count());
    let needs_force = entry_count == 1 && dir.join(".git").exists();
    if entry_count == 0 || needs_force {
        eprintln!("Initializing submodule {path}...");
        let mut args = vec!["submodule", "update", "--init"];
        if needs_force {
            args.push("--force");
        }
        args.push(path);
        let status = Command::new("git")
            .args(&args)
            .current_dir(repo_dir)
            .status()
            .expect("Failed to run git");
        assert!(status.success(), "git submodule update failed for {path}");
    }
}
