pub mod assets;
pub mod build;
pub mod buildlock;
pub mod durations;
pub mod forkcheck;
pub mod image;
pub mod land;
pub mod libc;
pub mod soundfont;
pub mod stamps;
pub mod testargs;
pub mod toolchain;
pub mod wallpaper;
pub mod worktree;

use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether this host will actually let a guest run on KVM.
///
/// **Presence is not permission**, and `Path::exists` cannot tell the two
/// apart. A GitHub runner ships `/dev/kvm` as `crw-rw---- root:kvm` with the
/// build user outside the group, so a check on existence puts `-accel kvm` on
/// every boot and every boot dies on `failed to initialize kvm: Permission
/// denied` — a whole suite red for a reason no test names. Any Linux box whose
/// user is not in `kvm` is that machine. Opening it is the question QEMU is
/// about to ask.
pub fn kvm_usable() -> bool {
    cfg!(target_arch = "x86_64")
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
}

/// The `.git` directory every worktree of this repository shares.
///
/// `git rev-parse --git-common-dir` answers relatively from the primary
/// checkout and absolutely from a linked worktree, so the answer is resolved
/// against `root` before it is canonicalised. Two worktrees must arrive at one
/// byte-identical path or the locks keyed on it serialise nothing.
pub fn git_common_dir(root: &Path) -> PathBuf {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("git rev-parse in {}: {e}", root.display()));
    assert!(
        output.status.success(),
        "{} is not inside a git repository",
        root.display()
    );
    let raw = String::from_utf8(output.stdout).expect("git printed non-UTF-8");
    let answer = Path::new(raw.trim());
    let resolved = if answer.is_absolute() { answer.to_path_buf() } else { root.join(answer) };
    std::fs::canonicalize(&resolved)
        .unwrap_or_else(|e| panic!("canonicalise {}: {e}", resolved.display()))
}

/// The checkout holding the state every worktree shares: the object store, the
/// `rust/` submodule, and the toolchain built from it.
///
/// `git worktree add` puts the common directory inside the primary checkout, so
/// its parent *is* that checkout and no worktree has to be told where it is.
/// A pointer file would be one more thing that can disagree with the tree.
pub fn primary_checkout(root: &Path) -> PathBuf {
    let common = git_common_dir(root);
    common
        .parent()
        .unwrap_or_else(|| panic!("{} has no parent", common.display()))
        .to_path_buf()
}

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
