//! Making a linked worktree buildable, and saying why when it cannot be.
//!
//! `git worktree add` alone leaves a tree that does not build and, worse, one
//! that builds *wrongly*: `rust/` comes out an empty stub, so the build system
//! reads it as a missing submodule, clones 913 MiB from the network, bootstraps
//! a second 47 GiB toolchain, and finally points the machine-global rustup
//! `toyos` name at it — taking the toolchain out from under every other
//! checkout. Measured, in that order, on this host.
//!
//! So the shared state stays shared and nothing here copies it: `rust/` is left
//! the stub it was, and [`crate::toolchain::rust_dir`] sends every read to the
//! primary checkout. What this module does is the small remainder — create the
//! worktree, carry over the one file git cannot, and refuse by name when the
//! result would not be usable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::toolchain;

/// Roughly what a worktree's crate target directories reach once it has built
/// and tested everything: 23 GiB, measured on the primary checkout with `du`.
/// Refusing below double that leaves room for the build that fills them.
const NEEDED_BYTES: u64 = 46 * 1024 * 1024 * 1024;

pub fn dispatch(root: &Path, args: &[String]) {
    let mut rest = args.iter().skip_while(|a| *a != "--worktree").skip(1);
    let verb = rest.next().map(String::as_str);
    let operand = rest.next().cloned();
    match verb {
        Some("add") => add(root, &operand.expect("--worktree add needs a path")),
        Some("list") => list(root),
        Some("remove") => remove(root, &operand.expect("--worktree remove needs a path")),
        other => panic!(
            "--worktree takes add <path>, list, or remove <path>; got {other:?}"
        ),
    }
}

/// Create a worktree and leave it in a state where `cargo run -- --build-only`
/// works.
fn add(root: &Path, path: &str) {
    let path = PathBuf::from(path);
    assert!(!path.exists(), "{} already exists", path.display());
    let name = path
        .file_name()
        .unwrap_or_else(|| panic!("{} has no final component", path.display()))
        .to_string_lossy()
        .to_string();

    // Everything that would make the result unusable, asked before anything is
    // created: a half-made worktree is worse than none, because the next agent
    // finds it and believes it.
    let primary = match toolchain::owner(root) {
        toolchain::Owner::Us => root.to_path_buf(),
        toolchain::Owner::Elsewhere(p) => p,
    };
    let stage2 = primary.join(format!(
        "rust/build/{}/stage2",
        toolchain::host_triple()
    ));
    assert!(
        stage2.join("bin/rustc").exists(),
        "the shared toolchain does not exist yet ({} is missing).\n\
         Run `cargo run -- --build-only` in {} before making worktrees of it.",
        stage2.display(),
        primary.display()
    );
    let free = free_bytes(path.parent().unwrap_or(Path::new("/")));
    assert!(
        free >= NEEDED_BYTES,
        "{} has {:.1} GiB free and a worktree's target directories reach about \
         {:.0} GiB.\nThe shared toolchain is not copied, but the crate targets are \
         its own.",
        path.parent().unwrap_or(Path::new("/")).display(),
        free as f64 / 1024.0_f64.powi(3),
        NEEDED_BYTES as f64 / 1024.0_f64.powi(3) / 2.0,
    );

    let branch = format!("wt/{name}");
    git(root, &["worktree", "add", "-b", &branch, &path.to_string_lossy(), "main"]);

    // `rust/` is deliberately left the empty stub `git worktree add` made. It is
    // not an oversight the next reader should fix: initialising it is the
    // 913 MiB clone, and a symlink in its place makes git error out of `status`,
    // `diff` and `submodule` alike rather than just ignoring it.

    // The one file git cannot carry: it is gitignored, and a worktree that
    // silently loses the fork redirects would build different code from the
    // checkout it was made from and report the difference as a result.
    let redirects = root.join(".cargo/config.toml");
    if redirects.exists() {
        fs::copy(&redirects, path.join(".cargo/config.toml"))
            .unwrap_or_else(|e| panic!("copy {}: {e}", redirects.display()));
        eprintln!("carried over .cargo/config.toml (fork redirects)");
    }

    eprintln!();
    eprintln!("worktree   {}", path.display());
    eprintln!("branch     {branch}");
    eprintln!("toolchain  {} (shared, not copied)", stage2.display());
    eprintln!();
    eprintln!("Build it with `cargo run -- --build-only` from {}.", path.display());
}

fn list(root: &Path) {
    let primary = match toolchain::owner(root) {
        toolchain::Owner::Us => root.to_path_buf(),
        toolchain::Owner::Elsewhere(p) => p,
    };
    eprintln!("toolchain owner  {}", primary.display());
    eprintln!(
        "rustup toyos     {}",
        fs::read_link(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".rustup/toolchains/toyos")
        )
        .map_or_else(|_| "unlinked".to_string(), |p| p.display().to_string())
    );
    eprintln!();
    git(root, &["worktree", "list"]);
}

/// Remove a worktree and the branch it was made with.
///
/// Deliberately not `--force`: a worktree with uncommitted work in it is a
/// refusal, because the work in a worktree is the only copy of itself.
fn remove(root: &Path, path: &str) {
    git(root, &["worktree", "remove", path]);
    eprintln!("removed {path}; its branch is still there, and `git branch -d` will say if it is unmerged");
}

fn free_bytes(dir: &Path) -> u64 {
    let output = Command::new("df")
        .args(["-k", &dir.to_string_lossy()])
        .output()
        .expect("run df");
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().nth(3))
        .and_then(|k| k.parse::<u64>().ok())
        .map(|k| k * 1024)
        .unwrap_or_else(|| panic!("could not read free space for {}", dir.display()))
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("run git: {e}"));
    assert!(status.success(), "git {args:?} failed");
}
