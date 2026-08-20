//! Identifiers a tree may not name, and the exceptions that are named instead.
//!
//! The first scan bans, over `kernel/src` and `toyos-sched/src`, the methods
//! that take an object's lifetime out of its `Arc`'s hands — `Arc::into_raw`,
//! `Arc::from_raw`, the two strong-count adjusters — and `mem::forget`. The
//! natural home for that would be
//! `kernel/clippy.toml`'s `disallowed-methods`, but **nothing in this
//! repository runs clippy** — not CI, not `cargo test`, not the build — so a
//! `clippy.toml` would be a wall with nothing behind it. A scan in
//! `cargo test --lib` runs on every machine that builds this tree, in
//! milliseconds, and can carry its exceptions with the reason each one is
//! allowed.
//!
//! The exceptions are per file and per line count, so an *added* `forget`
//! beside a permitted one is a red rather than a silence.
//!
//! The second scan enforces that there is no global registry: a name a process
//! could present and have resolved for it is the thing this architecture
//! deleted, so the registry's identifiers must be gone from the code rather
//! than merely unused. It reads
//! **code only** — comments and string literals are stripped first — because
//! the history of what a name used to mean is worth keeping and the
//! retired-syscall gravestone table names every one of them as a string on
//! purpose.
//!
//! The third runs the same walk over [`RETIRED_ABI_NAMES`]: every syscall,
//! `SYS_DEBUG` action and io_uring op code this project has deleted. Its
//! *number* is what is retired, and a number is not a thing a scan can look
//! for — the name that used to carry it is.

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
            // Both in `cpu.rs`'s test module, and both are the drop bomb
            // rather than a leak: `Task`'s "the only legal death is
            // `DeadTask::finalize`" is a scheduler invariant, so a test that
            // deliberately ends with a live task — which is most of the arms
            // §7.2 rewrote — may not drop its world, and a registration held
            // past a park it staged by hand is the same statement.
            ("toyos-sched/src/cpu.rs", 2),
        ],
    },
    // `toyos_untrusted::Untrusted` has no accessor, no cast, no arithmetic and
    // no `From`, so a number that crossed a trust boundary cannot reach an
    // index or a length except through `index(len)` or `at_most(bound)`. That
    // closes every *accidental* way out, which is every one of the filed sites:
    // each was a cast or a bare comparison somebody wrote without noticing they
    // were deciding anything.
    //
    // What typing alone cannot close is a bound that is not a bound. A caller
    // may write `at_most(u64::MAX)` and get the value straight back, and no
    // signature distinguishes that from a real ceiling. So the deliberate form
    // is banned here instead — which is the difference between "nothing stops
    // the ninth site" and "one scan does". These five needles are the whole of
    // it, because a bound written as a type's own maximum is the only shape
    // that admits every value.
    Ban {
        needle: "at_most(u64::MAX",
        why: "a bound of u64::MAX admits every value: it is an unwrap wearing a \
              check's name. If there is genuinely no ceiling then the value is \
              not a length, and it does not want this exit",
        allowed: &[],
    },
    Ban {
        needle: "at_most(u32::MAX",
        why: "as above, for a value already narrower than the bound it names",
        allowed: &[],
    },
    Ban {
        needle: "at_most(u16::MAX",
        why: "as above",
        allowed: &[],
    },
    Ban {
        needle: "at_most(u8::MAX",
        why: "as above",
        allowed: &[],
    },
    Ban {
        needle: "index(usize::MAX",
        why: "the same hole in the other exit: every value indexes a table of \
              usize::MAX entries, and no such table exists",
        allowed: &[],
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

/// Every other ABI name this project has retired: a deleted syscall, debug
/// action or io_uring op code, whose *number* is retired with it and never
/// reused (`CLAUDE.md`, "Syscall ABI").
///
/// The number is what the rule protects and a number cannot be scanned for —
/// so the name is, and a name back in code is how a number gets reissued by
/// accident. Retired numbers themselves are recorded where they can be read
/// beside the live ones: the comments in `toyos-abi/src/syscall.rs` and
/// `toyos-abi/src/io_uring.rs`, which this scan is blind to by construction
/// because it strips comments.
const RETIRED_ABI_NAMES: &[&str] = &[
    // Syscall 107. Nothing called it; a region's mappings go with its last
    // handle, so the handle is the whole of letting go.
    "SYS_SHM_UNMAP",
    // `SYS_DEBUG` actions 14 and 15. A total hides a leak of one kind behind
    // churn in another, and a breakdown in the kernel log is a reading no guest
    // test can see; every leak assertion in the estate is `CENSUS_KIND`.
    "CENSUS_TOTAL",
    "CENSUS_BREAKDOWN",
    // io_uring op code 2. No submitter anywhere: this kernel's polls are
    // one-shot and mio re-arms rather than cancels.
    "IORING_OP_POLL_REMOVE",
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

/// Every line of `kernel/src` under a relative path, with its number.
fn kernel_lines() -> Vec<(String, usize, String)> {
    let root = repo_root();
    let mut files = Vec::new();
    rust_files(&root, &root.join("kernel/src"), &mut files);
    let mut out = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for (n, line) in text.lines().enumerate() {
            out.push((rel(&root, &path), n + 1, line.to_string()));
        }
    }
    out
}

/// The three log macros and the function they all expand to.
const LOG_PRODUCERS: &[&str] = &["log!(", "alert!(", "boot_phase!(", "log::emit("];

/// The two places in `kernel/` a `!!!` may still appear, and how many times.
///
/// **Named by file and count rather than tested against the same line as a
/// `log!`.** A macro invocation is not a line — `rustfmt` puts a long one's
/// format string on its own — so "this line has `!!!` and a `log!` on it" is
/// defeated by a line break, and defeated silently. Every `!!!` in the tree is
/// listed here instead, so a new one is a red wherever it is written and
/// whatever it is written next to.
///
/// Both of these write raw bytes straight to the UART. They never enter the
/// ring, so `panic_console`'s deleted scan could not see them either and the
/// record's typed `Level` was never their business.
/// Counted in occurrences of `!!!` and not in lines, because each of these
/// writes one at each end of its message.
const SENTINEL_ALLOWED: &[(&str, usize)] = &[
    // `\n!!! PANIC REENTRY: CPU halted !!!\n`, written with the IDT possibly
    // gone.
    ("kernel/src/main.rs", 2),
    // `\n!!! DB TRAP !!!\n`, from the #DB handler before it clears DR7.
    ("kernel/src/arch/idt/exceptions.rs", 2),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// **Two clauses, one rule each, and neither is checkable any other way.**
    ///
    /// The first: the NMI handler must not log. It would reenter its own CPU's
    /// log shard — the reservation is sound only because the CPU that owns the
    /// shard has `IF` and `TF` masked through publication, and an NMI is the one
    /// interrupt that ignores `IF`. `dump_nmi_probe` is what makes the handler
    /// *useful*; this is what keeps it silent.
    ///
    /// The second: no log producer in `kernel/` carries `!!!` in its format
    /// string. `panic_console::has_alert` used to scan every display row for
    /// three exclamation marks, and its own comment enumerated the messages that
    /// happened to match; the panel reads `Level` off the record now, so a `!!!`
    /// put back into a message would be a marker marking nothing — a second,
    /// silent alert channel beside the typed one. The two `!!!` still in
    /// `kernel/` write raw bytes straight to the UART, never enter the ring, and
    /// were never `has_alert`'s business either.
    #[test]
    fn nmi_does_not_log() {
        let lines = kernel_lines();
        assert!(
            lines.iter().any(|(file, _, _)| file == "kernel/src/arch/idt/nmi.rs"),
            "the NMI handler moved: this gate is scanning a file that is not there"
        );

        let silent: Vec<_> = lines
            .iter()
            .filter(|(file, _, line)| {
                file == "kernel/src/arch/idt/nmi.rs"
                    && LOG_PRODUCERS.iter().any(|p| code_only(line).contains(p))
            })
            .map(|(file, n, line)| format!("{file}:{n}: {}", line.trim()))
            .collect();
        assert!(
            silent.is_empty(),
            "the NMI handler logs, and it reenters its own CPU's shard to do it:\n{}",
            silent.join("\n")
        );

        let mut found: Vec<(String, usize)> = Vec::new();
        for (file, _, line) in &lines {
            let n = line.matches("!!!").count();
            if n == 0 {
                continue;
            }
            match found.last_mut() {
                Some((last, count)) if last == file => *count += n,
                _ => found.push((file.clone(), n)),
            }
        }
        let mut complaints = Vec::new();
        for (file, count) in &found {
            match SENTINEL_ALLOWED.iter().find(|(allowed, _)| allowed == file) {
                Some((_, want)) if want == count => {}
                Some((_, want)) => complaints.push(format!(
                    "{file} has {count} `!!!` where this gate exempts {want} raw-UART ones"
                )),
                None => complaints.push(format!("{file} has {count} `!!!`")),
            }
        }
        for (file, want) in SENTINEL_ALLOWED {
            if !found.iter().any(|(f, _)| f == file) {
                complaints.push(format!(
                    "{file} no longer has the {want} raw-UART `!!!` this gate exempts, so the \
                     exemption is stale"
                ));
            }
        }
        assert!(
            complaints.is_empty(),
            "the `!!!` sentinel is deleted: the panel paints a red row from `Level::Alert` and \
             reads nothing out of the text, so a marker put back into a message marks \
             nothing.\n{}",
            complaints.join("\n")
        );
    }

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

    /// **There is no global registry.** A name a process could present and have
    /// resolved for it is the thing this architecture deletes, so its
    /// identifiers may not be reachable from any code the guest compiles.
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

    /// **A retired ABI number is never reused**, and the name is the only part
    /// of it a scan can hold on to. A retired name back in guest-compiled code
    /// is either the number coming back or a new call wearing a dead one's
    /// identity, and the two are indistinguishable from the outside.
    #[test]
    fn a_retired_abi_name_is_gone_from_the_code() {
        let mut complaints = Vec::new();
        for needle in RETIRED_ABI_NAMES {
            for at in named_in_code(needle) {
                complaints.push(format!("{at}: names `{needle}`"));
            }
        }
        assert!(
            complaints.is_empty(),
            "these names are retired and their numbers with them:\n  {}",
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
            "the permitted `mem::forget` call sites are not being found, so one \
             more would not be either",
        );
    }
}
