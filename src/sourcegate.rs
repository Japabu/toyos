//! Identifiers a tree may not name, and the exceptions that are named instead.
//!
//! `specs/assessments/capability-handles-spec.md` §12.2 asks for this as
//! `kernel/clippy.toml`'s `disallowed-methods`. **Nothing in this repository
//! runs clippy** — not CI, not `cargo test`, not the build — so a `clippy.toml`
//! would be a wall with nothing behind it. A scan in `cargo test --lib` runs on
//! every machine that builds this tree, in milliseconds, and can carry its
//! exceptions with the reason each one is allowed.
//!
//! The exceptions are per file and per line count, so an *added* `forget`
//! beside a permitted one is a red rather than a silence.
//!
//! The second scan is `specs/capability-endowment-spec.md` §8.4: the global
//! registry's names must be gone from the code, not merely unused. It reads
//! **code only** — comments and string literals are stripped first — because
//! the history of what a name used to mean is worth keeping and the
//! retired-syscall gravestone table names every one of them as a string on
//! purpose.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// One banned identifier: what it is, why, and where it is nonetheless allowed.
struct Ban {
    needle: &'static str,
    why: &'static str,
    /// `(path relative to the repository root, how many times)`.
    allowed: &'static [(&'static str, usize)],
}

/// Trees the object layer's lifetime rules govern.
const TREES: &[&str] = &["kernel/src", "toyos-sched/src"];

const BANS: &[Ban] = &[
    Ban {
        needle: "Arc::into_raw",
        why: "an object's lifetime is its Arc's; a raw pointer out of one is a \
              refcount nobody owns",
        allowed: &[],
    },
    Ban {
        needle: "Arc::from_raw",
        why: "the other half of the same hole",
        allowed: &[],
    },
    Ban {
        needle: "Arc::increment_strong_count",
        why: "hand-rolled refcounting is the bug class the object layer deletes",
        allowed: &[],
    },
    Ban {
        needle: "Arc::decrement_strong_count",
        why: "as above, and this one is the half that frees",
        allowed: &[],
    },
    Ban {
        // Not a refcount hazard by itself — the ban is about intent. A `forget`
        // is a statement that something is never given back, and in a kernel
        // that does not unwind it reads exactly like a `Drop` somebody meant to
        // rely on.
        needle: "mem::forget",
        why: "a resource with no giver-back is a leak unless the reason is at \
              the call site",
        allowed: &[
            // The GPU is never torn down, so the cursor pages outlive every
            // process that could name them.
            ("kernel/src/drivers/gop.rs", 1),
            // dlmalloc owns the page from here on.
            ("kernel/src/mm/alloc.rs", 1),
        ],
    },
];

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

fn rust_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `(file, count)` where `needle` appears, over [`TREES`].
fn occurrences(needle: &str) -> Vec<(String, usize)> {
    let root = repo_root();
    let mut files = Vec::new();
    for tree in TREES {
        rust_files(&root, &root.join(tree), &mut files);
    }
    let mut found = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let n = text.matches(needle).count();
        if n > 0 {
            found.push((rel(&root, &path), n));
        }
    }
    found
}

/// The names the global registry left behind, and one that is not a name at
/// all: `services::connect` was the call that resolved one.
///
/// Each is retired rather than renamed — `SYS_CONNECTION_JOIN` keeps number 76
/// and is a different call, addressed by handle, granting nothing. A word
/// boundary is what tells the two apart here.
const RETIRED_REGISTRY: &[&str] = &[
    "SYS_CONNECT",
    "SYS_LISTEN",
    "SYS_PIPE_OPEN",
    "SYS_PIPE_ID",
    "SYS_SOCKET_CREATE",
    "SharedToken",
    "services::connect",
];

/// Everything this repository compiles into the guest.
const GUEST_TREES: &[&str] =
    &["kernel/src", "toyos/src", "toyos-abi/src", "userland", "tests"];

/// `line` with its comment and its string literals removed.
///
/// What is left is the part that names things. Prose explaining what a deleted
/// call used to do is legal and worth keeping; a gravestone table mapping a
/// retired number to the string `"SYS_LISTEN"` is the point of the table.
fn code_only(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_string => {
                chars.next();
            }
            '"' => in_string = !in_string,
            '/' if !in_string && chars.peek() == Some(&'/') => break,
            _ if !in_string => out.push(c),
            _ => {}
        }
    }
    out
}

/// Whether `code` names `needle` as an identifier rather than as a fragment of
/// a longer one.
fn names(code: &str, needle: &str) -> bool {
    let bytes = code.as_bytes();
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    code.match_indices(needle).any(|(at, _)| {
        !word(at.checked_sub(1).and_then(|j| bytes.get(j)))
            && !word(bytes.get(at + needle.len()))
    })
}

/// `(file, line number)` for every place `needle` is named in code, over
/// [`GUEST_TREES`].
fn named_in_code(needle: &str) -> Vec<String> {
    let root = repo_root();
    let mut files = Vec::new();
    for tree in GUEST_TREES {
        rust_files(&root, &root.join(tree), &mut files);
    }
    let mut found = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for (n, line) in text.lines().enumerate() {
            if names(&code_only(line), needle) {
                found.push(format!("{}:{}", rel(&root, &path), n + 1));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_in_the_kernel_counts_a_reference_by_hand() {
        let mut complaints = Vec::new();
        for ban in BANS {
            for (file, count) in occurrences(ban.needle) {
                let allowed = ban
                    .allowed
                    .iter()
                    .find(|(f, _)| *f == file)
                    .map_or(0, |(_, n)| *n);
                if count > allowed {
                    complaints.push(format!(
                        "{file}: {count} × `{}`, {allowed} allowed — {}",
                        ban.needle, ban.why,
                    ));
                }
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// An exception that has gone stale is a permission nobody re-argued.
    #[test]
    fn every_named_exception_is_still_there() {
        for ban in BANS {
            let found = occurrences(ban.needle);
            for (file, allowed) in ban.allowed {
                let count = found.iter().find(|(f, _)| f == file).map_or(0, |(_, n)| *n);
                assert_eq!(
                    count, *allowed,
                    "{file} is allowed {allowed} × `{}` and has {count}. \
                     An exception is a decision, so it goes when its call site does.",
                    ban.needle,
                );
            }
        }
    }

    /// **There is no global registry.** `specs/capability-endowment-spec.md`
    /// §8.4: a name a process could present and have resolved for it is the
    /// thing this architecture deletes, so its identifiers may not be reachable
    /// from any code the guest compiles.
    #[test]
    fn no_name_resolves_through_a_registry_any_more() {
        let mut complaints = Vec::new();
        for needle in RETIRED_REGISTRY {
            for at in named_in_code(needle) {
                complaints.push(format!("{at}: names `{needle}`"));
            }
        }
        assert!(
            complaints.is_empty(),
            "the registry is deleted, and these still name it:\n  {}",
            complaints.join("\n  "),
        );
    }

    /// What the scan above can and cannot see, stated as cases, because a
    /// well-formed tree exercises none of them.
    #[test]
    fn the_registry_scan_reads_code_and_not_prose() {
        assert!(names(&code_only("    let x = syscall(SYS_LISTEN, 0);"), "SYS_LISTEN"));
        assert!(names(&code_only("pub const SYS_PIPE_ID: u64 = 70;"), "SYS_PIPE_ID"));
        assert!(!names(&code_only("/// `SYS_LISTEN` used to register a name."), "SYS_LISTEN"));
        assert!(!names(&code_only("    // SYS_PIPE_ID was 70"), "SYS_PIPE_ID"));
        assert!(!names(&code_only("    85 => \"SYS_LISTEN\","), "SYS_LISTEN"));
        // The live call keeps the retired one's number and must not be read as
        // it: this is the whole reason the match is on a word boundary.
        assert!(!names(&code_only("SYS_CONNECTION_JOIN => join(a, b),"), "SYS_CONNECT"));
        assert!(names(&code_only("SYS_CONNECT => connect(a),"), "SYS_CONNECT"));
        // And the walk reaches real code: a live name it is capable of finding
        // must actually be found.
        assert!(
            !named_in_code("SYS_CONNECTION_JOIN").is_empty(),
            "the scan found no `SYS_CONNECTION_JOIN` in code, so it is not reading the guest trees",
        );
    }

    /// The scan has teeth only if it can find anything: this file names every
    /// banned identifier in its own table, and the walk must not be looking at
    /// a tree where none of them can occur.
    #[test]
    fn the_scan_reaches_the_trees_it_claims_to() {
        let root = repo_root();
        for tree in TREES {
            let mut files = Vec::new();
            rust_files(&root, &root.join(tree), &mut files);
            assert!(!files.is_empty(), "{tree} has no .rs files — the walk is looking elsewhere");
        }
        assert!(
            !occurrences("mem::forget").is_empty(),
            "the two permitted `mem::forget` call sites are not being found, so a \
             third would not be either",
        );
    }
}
