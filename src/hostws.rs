//! Who is in the host workspace, and the gate that keeps the answer honest.
//!
//! Every crate in this repository that is tested on the host is a member of the
//! workspace root `Cargo.toml` declares, and CI runs the lot with one
//! `cargo test --workspace --exclude toyos-build`. That is the whole point of
//! the arrangement. Before it, the set of host-tested crates was a list in the
//! workflow *and* a set of standalone workspace roots, and the two drifted
//! three times: four pure crates until 2026-08-08, `toyos-keymap` and
//! `bcachefs` until 2026-08-14, and `toyos-abi` and `toyos-manifest` — 23 tests
//! between them — which reached no workflow at all
//! (`issues/build/three-host-crates-are-tested-nowhere.md`, closed by the
//! commit that added this file).
//!
//! A third copy of the list would restore the defect, so there is exactly one:
//! the `[workspace]` table. This module is its only reader. [`target_dir`]
//! answers `src/build.rs`'s question about where cargo writes a crate's
//! artifacts — a member's land in the workspace root's `target/`, not its own —
//! and the gates below walk the tree and red on a `Cargo.toml` that joined
//! neither `members` nor `exclude`. **A new host crate that forgets to join is
//! a red, not a silent gap.**

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The `[workspace]` table of `root/Cargo.toml`.
fn workspace_table(root: &Path) -> toml::value::Table {
    let path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let doc: toml::Value = text
        .parse()
        .unwrap_or_else(|e| panic!("{} is not TOML: {e}", path.display()));
    doc.get("workspace")
        .and_then(|w| w.as_table())
        .unwrap_or_else(|| {
            panic!("{} declares no [workspace]; the host suite is that table", path.display())
        })
        .clone()
}

/// One `[workspace]` string array, normalised to forward slashes with no
/// trailing separator.
///
/// A glob is refused by name rather than half-understood: cargo expands one and
/// this module would not, and two readers of one list that disagree is the
/// defect this file exists to end.
fn list(root: &Path, key: &str) -> BTreeSet<String> {
    workspace_table(root)
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|v| {
                    let s = v
                        .as_str()
                        .unwrap_or_else(|| panic!("[workspace] {key} holds a non-string: {v}"));
                    assert!(
                        !s.contains('*') && !s.contains('?'),
                        "[workspace] {key} holds the glob {s:?}. Cargo expands one and \
                         src/hostws.rs does not, so the two would read one list differently — \
                         which is the drift this gate exists to refuse. Name the crates."
                    );
                    s.trim_end_matches('/').to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The member paths, relative to the repository root. `"."` is the root package
/// and is a member without being listed, so it is added here.
pub fn members(root: &Path) -> BTreeSet<String> {
    let mut m = list(root, "members");
    m.insert(".".to_string());
    m
}

/// The excluded paths, relative to the repository root.
pub fn excluded(root: &Path) -> BTreeSet<String> {
    list(root, "exclude")
}

/// `path` relative to `root`, with forward slashes.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Whether `crate_dir` is a member of the host workspace.
pub fn is_member(root: &Path, crate_dir: &Path) -> bool {
    let relative = rel(root, crate_dir);
    let relative = if relative.is_empty() { ".".to_string() } else { relative };
    members(root).contains(&relative)
}

/// The `target/` cargo writes what it builds in `crate_dir` into.
///
/// **A workspace member has no `target/` of its own**: cargo writes every
/// member's artifacts to the workspace root's. `toyos-ld` and `toyos-cc` are
/// the two this matters for — each is a host tool *and* a `[programs]` entry in
/// `system.toml`, so `src/build.rs` builds it a second time for
/// `x86_64-unknown-toyos` and then reads the result back off disk. Reading the
/// old per-crate path after they joined the workspace would have been a
/// `Failed to read binary for toyos-ld` on every image build.
///
/// Everything else this is asked about — `kernel/`, `bootloader/`, `userland/`,
/// the guest crates under `tests/` — is excluded from the workspace and keeps
/// its own.
pub fn target_dir(root: &Path, crate_dir: &Path) -> PathBuf {
    if is_member(root, crate_dir) {
        root.join("target")
    } else {
        crate_dir.join("target")
    }
}

/// Every directory under `root` holding a `Cargo.toml`, as paths relative to
/// `root`, with the excluded subtrees pruned exactly as cargo prunes them.
///
/// `target`, `.git` and every other dotted directory go too: what is in them is
/// build output and history, not a crate somebody has to have declared.
///
/// Only the gate reads this, so it is not compiled into the build system.
#[cfg(test)]
fn crate_dirs(root: &Path) -> BTreeSet<String> {
    let prune = excluded(root);
    let mut found = BTreeSet::new();
    walk(root, root, &prune, &mut found);
    found
}

#[cfg(test)]
fn walk(root: &Path, dir: &Path, prune: &BTreeSet<String>, found: &mut BTreeSet<String>) {
    if dir.join("Cargo.toml").is_file() {
        let relative = rel(root, dir);
        found.insert(if relative.is_empty() { ".".to_string() } else { relative });
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if prune.contains(&rel(root, &path)) {
            continue;
        }
        walk(root, &path, prune, found);
    }
}

/// The crate directories that joined neither list.
///
/// Excluded subtrees never reach here — they are pruned during the walk, which
/// is what excluding one means — so this is the whole question: a `Cargo.toml`
/// the tree contains and the `[workspace]` table does not account for.
#[cfg(test)]
fn unclaimed(members: &BTreeSet<String>, found: &BTreeSet<String>) -> Vec<String> {
    found.difference(members).cloned().collect()
}

/// The tables in `manifest` that cargo reads only from a workspace root.
///
/// Parsed as TOML and not scanned as text, for `src/sourcegate.rs`'s reason:
/// `toyos-cc/Cargo.toml` and `toyos-ld/Cargo.toml` each carry a comment saying
/// where their `[profile.toyos]` went and why, and a substring scan reads that
/// explanation as the declaration it is warning about.
#[cfg(test)]
fn tables_cargo_would_ignore(manifest: &str) -> Vec<&'static str> {
    let doc: toml::Value = manifest.parse().expect("a member's manifest is TOML");
    ["profile", "patch"].into_iter().filter(|key| doc.get(key).is_some()).collect()
}

/// Every member of every workspace in this repository, as paths relative to the
/// root.
///
/// The host workspace's own, plus those of the workspaces it excludes: a
/// workspace excluded from this one is still a workspace, and its members still
/// have no target directory of their own. `userland/` is the one that matters —
/// `sshd` and `calc` are members of it, and `host-tests.yml` cached
/// `userland/sshd/target`, a directory that has never existed.
#[cfg(test)]
fn every_workspace_member(root: &Path) -> BTreeSet<String> {
    let mut all: BTreeSet<String> = members(root).into_iter().filter(|m| m != ".").collect();
    for dir in excluded(root) {
        let Ok(text) = std::fs::read_to_string(root.join(&dir).join("Cargo.toml")) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Value>() else { continue };
        let nested = doc
            .get("workspace")
            .and_then(|w| w.get("members"))
            .and_then(|m| m.as_array())
            .map(|a| a.as_slice())
            .unwrap_or_default();
        for member in nested.iter().filter_map(|m| m.as_str()) {
            all.insert(format!("{dir}/{}", member.trim_end_matches('/')));
        }
    }
    all
}

/// Every `<member>/target` `text` names.
///
/// A member has no target directory of its own, so naming one is a path that
/// cannot exist. This found `userland/doom/build.rs` reaching for
/// `../../toyos-cc/target/<host>/release/toyos-cc` after `toyos-cc` joined the
/// workspace, and CI did not: the guest jobs restored a cache that still held
/// the old binary at the old path, so the build was green on a file the tree no
/// longer produces.
#[cfg(test)]
fn dead_member_target_paths(members: &BTreeSet<String>, text: &str) -> Vec<String> {
    members
        .iter()
        .filter(|m| *m != ".")
        .map(|m| format!("{m}/target"))
        .filter(|needle| text.contains(needle.as_str()))
        .collect()
}

/// `text` with its line comments removed and its string literals kept.
///
/// `src/sourcegate.rs` strips both; this one may not. The paths that matter
/// here live *inside* string literals — `root.join("../../toyos-cc/target/…")`
/// is the whole defect — while the explanations of where those paths went live
/// in comments, in this file and in `.github/workflows/ci.yml` and in the build
/// script the gate first caught. A scan that read the explanation as the
/// offence would be unable to say why it was complaining.
#[cfg(test)]
fn code_only(text: &str, line_comment: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut in_string = false;
        let mut escaped = false;
        for (at, c) in line.char_indices() {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' if in_string => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = !in_string;
                    out.push(c);
                }
                _ if !in_string && line[at..].starts_with(line_comment) => break,
                _ => out.push(c),
            }
        }
        out.push('\n');
    }
    out
}

/// How a line comment starts in `path`, or `None` where this does not know.
#[cfg(test)]
fn line_comment(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("//"),
        Some("yml" | "yaml" | "sh") => Some("#"),
        _ => None,
    }
}

/// Every `build.rs` under `dir`, wherever it lives — a build script runs, so a
/// path in one is a path something acts on.
#[cfg(test)]
fn build_scripts(dir: &Path, out: &mut Vec<PathBuf>) {
    let script = dir.join("build.rs");
    if script.is_file() {
        out.push(script);
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir()) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" || name == "rust" {
            continue;
        }
        build_scripts(&path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// **The gate.** A crate added to this repository joins the host workspace
    /// or is excluded from it with a reason, and there is no third option.
    ///
    /// This is the defect `issues/build/three-host-crates-are-tested-nowhere.md`
    /// recorded, and it had already recurred twice before that: a host-testable
    /// crate arrives, nobody adds it to the workflow's loop, and its tests run
    /// nowhere while reading as though they run everywhere. There is one list
    /// now and the tree is held against it.
    #[test]
    fn every_crate_in_the_tree_joined_the_workspace_or_was_excluded_by_name() {
        let root = repo_root();
        let missing = unclaimed(&members(&root), &crate_dirs(&root));
        assert!(
            missing.is_empty(),
            "these directories hold a `Cargo.toml` and the [workspace] table in \
             Cargo.toml accounts for none of them:\n  {}\n\
             Add each to `members` (so `cargo test --workspace` runs its tests) or to \
             `exclude` with the reason it keeps its own resolution. A host crate that \
             joins neither is tested by nothing, which is how `toyos-abi` and \
             `toyos-manifest` went 23 tests unrun.",
            missing.join("\n  "),
        );
    }

    /// Teeth, run rather than argued: a well-formed tree exercises none of the
    /// cases above, so the classifier is shown refusing one.
    #[test]
    fn the_gate_refuses_a_crate_that_joined_neither_list() {
        let members: BTreeSet<String> =
            [".", "toyos-elf", "toyos-sched", "toyos-sched/sim"].iter().map(|s| s.to_string()).collect();

        let all_declared: BTreeSet<String> = members.clone();
        assert!(unclaimed(&members, &all_declared).is_empty());

        // The shape this exists to catch: a new host crate in the tree that
        // nobody added to the list.
        let with_a_newcomer: BTreeSet<String> =
            members.union(&["toyos-newthing".to_string()].into()).cloned().collect();
        assert_eq!(unclaimed(&members, &with_a_newcomer), ["toyos-newthing"]);

        // And a nested one, which is how `toyos-sched/loom` could have been
        // lost when its parent stopped being a workspace of its own.
        let with_a_nested_newcomer: BTreeSet<String> =
            members.union(&["toyos-sched/loom".to_string()].into()).cloned().collect();
        assert_eq!(unclaimed(&members, &with_a_nested_newcomer), ["toyos-sched/loom"]);
    }

    /// The walk has teeth only if it can find anything: it must reach the real
    /// tree, and it must see past the first level, or the gate above reports a
    /// clean tree it never looked at.
    #[test]
    fn the_walk_reaches_the_tree_and_descends_into_it() {
        let root = repo_root();
        let found = crate_dirs(&root);
        assert!(found.contains("."), "the walk did not find the root package");
        assert!(found.contains("toyos-elf"), "the walk did not find toyos-elf: {found:?}");
        assert!(
            found.contains("toyos-sched/loom"),
            "the walk did not descend past the first level, so a nested member could \
             go missing without this gate noticing: {found:?}"
        );
        // With nothing declared a member, every one of them is a complaint —
        // the gate above is silent because the table accounts for the tree, not
        // because the walk found nothing to account for.
        assert!(
            unclaimed(&BTreeSet::new(), &found).len() >= 20,
            "the walk found {} crate directories; the tree has more than that",
            found.len(),
        );
    }

    /// An exclusion that has gone stale is a permission nobody re-argued, and
    /// worse here than in most places: an entry naming a directory that no
    /// longer exists silently stops pruning anything, and one naming a
    /// directory that has become host-testable hides it from the whole suite.
    #[test]
    fn every_member_and_every_exclusion_still_names_a_directory() {
        let root = repo_root();
        for member in members(&root) {
            let dir = root.join(&member);
            assert!(
                dir.join("Cargo.toml").is_file(),
                "[workspace] members names {member:?} and there is no Cargo.toml there",
            );
        }
        for excluded in excluded(&root) {
            // Not "holds a Cargo.toml": a linked worktree's `rust/` is the empty
            // stub `git worktree add` leaves (src/CLAUDE.md), and excluding it
            // is right in both checkouts.
            assert!(
                root.join(&excluded).is_dir(),
                "[workspace] exclude names {excluded:?} and there is no such directory. \
                 An excuse may not outlive what it excused.",
            );
        }
    }

    /// **`cargo test` at the root is the QEMU harness and `cargo run` is the dev
    /// loop.** Both are documented law (root `CLAUDE.md`), and both hold only
    /// because `default-members` is the root package alone: widening it would
    /// make a bare `cargo test` build and run twenty-one crates' worth of host
    /// tests first, and a bare `cargo run` ambiguous.
    #[test]
    fn the_root_package_is_the_only_default_member() {
        let root = repo_root();
        let table = workspace_table(&root);
        let default: Vec<String> = table
            .get("default-members")
            .and_then(|v| v.as_array())
            .expect("[workspace] declares default-members")
            .iter()
            .map(|v| v.as_str().expect("a string").to_string())
            .collect();
        assert_eq!(
            default,
            ["."],
            "default-members must be the root package alone, so a bare `cargo test` stays \
             the QEMU harness and a bare `cargo run` stays the dev loop",
        );
    }

    /// Cargo reads `[profile]` and `[patch]` from the workspace root and
    /// **silently ignores both in a member** — it warns, into output nobody
    /// reads on a green build. For `toyos-ld` and `toyos-cc` that is not
    /// cosmetic: each is a `[programs]` guest binary as well as a host tool, and
    /// the `[profile.toyos]` they used to declare is what puts `overflow-checks`
    /// into the image. Both crafted-ELF kernel panics in `issues/` were
    /// *found* by an overflow check.
    #[test]
    fn no_member_declares_a_profile_or_a_patch_cargo_would_ignore() {
        let root = repo_root();
        let mut bad = Vec::new();
        for member in members(&root) {
            if member == "." {
                continue;
            }
            let path = root.join(&member).join("Cargo.toml");
            let text = std::fs::read_to_string(&path).expect("a member's manifest is readable");
            for key in tables_cargo_would_ignore(&text) {
                bad.push(format!("{member}/Cargo.toml declares a `[{key}]` table"));
            }
        }
        assert!(
            bad.is_empty(),
            "cargo honours neither in a workspace member and says so only in a warning:\n  {}\n\
             Move it to the root Cargo.toml, where it reaches the member it was written for.",
            bad.join("\n  "),
        );
    }

    /// Teeth for the rule above, and the reason it parses rather than greps:
    /// the two manifests it was written against now explain in a comment where
    /// their tables went, and the first draft of this gate read the explanation
    /// as the offence.
    #[test]
    fn the_ignored_table_scan_reads_toml_and_not_prose() {
        assert_eq!(
            tables_cargo_would_ignore("[package]\nname = \"a\"\n\n[profile.toyos]\nopt-level = 2\n"),
            ["profile"],
        );
        assert_eq!(
            tables_cargo_would_ignore("[package]\nname = \"a\"\n\n[patch.crates-io]\nx = \"1\"\n"),
            ["patch"],
        );
        assert!(tables_cargo_would_ignore(
            "# `[profile.toyos]` used to be declared here; it lives in the root now.\n\
             [package]\nname = \"a\"\n"
        )
        .is_empty());
        // And a value whose *name* contains one, which a looser match would
        // take for a table header.
        assert!(tables_cargo_would_ignore(
            "[package]\nname = \"a\"\n\n[dependencies]\nprofile-thing = \"1\"\n"
        )
        .is_empty());
    }

    /// **Nothing that executes may name `<member>/target`** — a member builds
    /// into its workspace root's target directory, so such a path is one that
    /// cannot exist.
    ///
    /// Every workspace in the tree, not just this one: the first thing this
    /// found after `userland/doom/build.rs` was `host-tests.yml` caching
    /// `userland/sshd/target`, which has never been a directory — `sshd` is a
    /// member of `userland/`'s workspace and builds into `userland/target`. A
    /// cache path that matches nothing fails silently and forever, which is why
    /// it survived.
    ///
    /// The files scanned are the ones that *act* on a path: the workflows, this
    /// build system, and every `build.rs` in the tree. Prose is left alone —
    /// `issues/hardware/pre-flash-gate-missed-the-milestone.md` records a
    /// flashed artifact built when `toyos-ld/target` was a real directory, and
    /// that record is not made truer by editing it.
    #[test]
    fn nothing_that_runs_names_a_target_directory_a_member_does_not_have() {
        let root = repo_root();
        let members = every_workspace_member(&root);
        let mut files: Vec<PathBuf> = Vec::new();
        for dir in [".github/workflows", "src"] {
            let Ok(entries) = std::fs::read_dir(root.join(dir)) else { continue };
            files.extend(entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_file()));
        }
        let mut scripts = Vec::new();
        build_scripts(&root, &mut scripts);
        files.extend(scripts);
        files.sort();

        let mut bad = Vec::new();
        for path in &files {
            // This file names both paths in string literals on purpose: they
            // are the fixtures the scan is proved against just below. Reading
            // its own test data would make the gate permanently red, and
            // skipping it costs nothing — `dead_member_target_paths` is
            // exercised directly there, on exactly those literals.
            if path.ends_with("hostws.rs") {
                continue;
            }
            let Some(marker) = line_comment(path) else { continue };
            let Ok(text) = std::fs::read_to_string(path) else { continue };
            for dead in dead_member_target_paths(&members, &code_only(&text, marker)) {
                bad.push(format!("{}: names {dead}", rel(&root, path)));
            }
        }
        assert!(
            bad.is_empty(),
            "a workspace member builds into the workspace root's `target/`, so these name a \
             directory that does not exist:\n  {}\n\
             `src/toolchain.rs`'s `toyos_ld_binary` and `hostws::target_dir` are where the \
             real path comes from.",
            bad.join("\n  "),
        );
    }

    /// Teeth for the rule above.
    #[test]
    fn the_dead_path_scan_names_the_member_and_ignores_an_excluded_crate() {
        let members: BTreeSet<String> =
            [".", "toyos-cc", "toyos-ld"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            dead_member_target_paths(&members, "let cc = root.join(\"../../toyos-cc/target/x\");"),
            ["toyos-cc/target"],
        );
        // `kernel/` and `userland/` are workspace *roots*, so naming one of
        // their target directories is correct and must not be reported.
        assert!(dead_member_target_paths(&members, "kernel/target/x86_64-unknown-none").is_empty());
        assert!(dead_member_target_paths(&members, "userland/target").is_empty());
        // A member of the userland workspace, which is the real find above.
        let with_userland: BTreeSet<String> =
            members.union(&["userland/sshd".to_string()].into()).cloned().collect();
        assert_eq!(
            dead_member_target_paths(&with_userland, "            userland/sshd/target\n"),
            ["userland/sshd/target"],
        );
        // And the root's own `target` is where members build *to*.
        assert!(dead_member_target_paths(&members, "root.join(\"target\")").is_empty());
    }

    /// The scan reads what runs and not what explains it — and this file, the
    /// workflow and the build script it caught all now carry exactly such an
    /// explanation, so the distinction is load-bearing rather than theoretical.
    #[test]
    fn the_dead_path_scan_reads_code_and_not_the_comment_beside_it() {
        assert_eq!(
            code_only("let p = \"toyos-cc/target/x\"; // was toyos-ld/target\n", "//").trim(),
            "let p = \"toyos-cc/target/x\";",
        );
        assert_eq!(code_only("            target\n", "#").trim(), "target");
        assert_eq!(
            code_only("            # toyos-ld/target used to be here\n", "#").trim(),
            "",
        );
        // A `//` inside a string is not a comment, so what follows it on the
        // line is still code and still read.
        assert_eq!(
            code_only("let u = \"https://x/\"; let p = \"toyos-ld/target\";\n", "//").trim(),
            "let u = \"https://x/\"; let p = \"toyos-ld/target\";",
        );
    }

    /// [`target_dir`] is what `src/build.rs` reads a guest binary back from, so
    /// it has to answer differently for the two kinds of crate — and be shown
    /// doing it, because reading the wrong one is a build failure with a
    /// message that names a file rather than a cause.
    #[test]
    fn a_member_builds_into_the_workspace_target_and_an_excluded_crate_into_its_own() {
        let root = repo_root();
        assert_eq!(target_dir(&root, &root.join("toyos-ld")), root.join("target"));
        assert_eq!(target_dir(&root, &root.join("toyos-cc")), root.join("target"));
        assert_eq!(target_dir(&root, &root), root.join("target"));
        assert_eq!(
            target_dir(&root, &root.join("kernel")),
            root.join("kernel/target"),
            "kernel/ is excluded and keeps its own target directory",
        );
        assert_eq!(
            target_dir(&root, &root.join("userland/snake")),
            root.join("userland/snake/target"),
        );
    }
}
