//! A process's `cwd` must not be able to grow without bound.
//!
//! `MAX_USER_STR` (64 KiB) bounds every path *argument*, and its derivation
//! says the number is set by the largest allocation derived from it. But
//! `Vfs::resolve_absolute` prepends `cwd` before handing the result to
//! `normalize`, and `cwd` was bounded by nothing — so the input the constant
//! was sized against stopped being the input `normalize` saw. A bound defeated
//! by composition: the check was real, the assumption behind it had quietly
//! stopped holding.
//!
//! The amplifier is that `mkdir` does no existence check — `create_dir` just
//! inserts the resolved string into a set, and `cd` accepts anything in that
//! set. So `mkdir(rel); chdir(rel)` appends ~64 KiB per round with no
//! filesystem involvement at all, and about thirty rounds put a `format!`
//! past the kernel heap's 2 MiB single-allocation ceiling — an assert taken
//! *inside the allocator lock*, which wedges the recovered CPU's next alloc.
//!
//! Roughly sixty syscalls from plain std, no crafted ELF, no large-RAM guest.
//! Before the fix this panics the kernel; after it, every over-long `chdir` is
//! a clean error and `cwd` stays bounded.

use std::fs;

/// `kernel/src/vfs.rs`'s `MAX_PATH`, by value. The test asserts against the
/// bound's *consequences*, not this number, so a change to it there does not
/// silently make this test vacuous — but the reachability checks below need to
/// know roughly where the wall is.
const MAX_PATH: usize = 4096;

const DIR: &str = "/home/abuse_cwd";

/// One component long enough that a single successful round would blow most of
/// the budget, and far longer than `MAX_PATH` on its own.
const HUGE_COMPONENT: usize = 60_000;

/// Enough rounds that, unfixed, `cwd` passes 2 MiB well before the loop ends.
/// At ~64 KiB a round the panic lands around round 32.
const ROUNDS: usize = 64;

/// The kernel's `cwd` length, read through the syscall rather than through
/// `std::env::current_dir`.
///
/// `current_dir` cannot be used to measure this: std's `getcwd` passes a fixed
/// `[u8; 256]` buffer and `sys_getcwd` copies `min(cwd.len(), buf.len())` with
/// no error, so any cwd past 256 bytes comes back *silently truncated to a
/// different path*. Measuring with it made this test report 256 bytes for a
/// 2 KiB cwd — a broken instrument, and a defect of its own worth fixing where
/// the ABI contract lives.
///
/// The buffer here is deliberately larger than `MAX_PATH`, so a cwd that
/// somehow exceeded the bound would be observed at its true length rather than
/// clamped to one that satisfies the assertions below.
fn cwd_len() -> usize {
    let mut buf = [0u8; 4 * MAX_PATH];
    toyos_abi::syscall::getcwd(&mut buf)
}

/// The Route B exploit: append a huge component per round and never stop.
///
/// Unfixed, `cwd` grows ~64 KiB a round until a `format!` in `resolve_absolute`
/// crosses the heap ceiling and the kernel dies. Fixed, every round is refused
/// and `cwd` never moves.
fn drive_cwd_growth() {
    let huge = "a".repeat(HUGE_COMPONENT);
    let start = cwd_len();
    let mut accepted = 0usize;

    for round in 0..ROUNDS {
        // `mkdir` may legitimately succeed — it is not the operation under
        // test, and it does no existence check. The bound belongs on `chdir`,
        // which is what stores a path in the kernel.
        let _ = fs::create_dir(&huge);

        if std::env::set_current_dir(&huge).is_err() {
            break;
        }
        accepted += 1;

        let len = cwd_len();
        assert!(
            len <= MAX_PATH,
            "round {round}: chdir accepted a path of {len} bytes, past MAX_PATH {MAX_PATH}",
        );
    }

    // A single 60 KiB component cannot fit in 4096 bytes, so not one round may
    // be accepted. If this ever passes, the bound is not on the path length.
    assert_eq!(
        accepted, 0,
        "a {HUGE_COMPONENT}-byte component was accepted {accepted} times; MAX_PATH is {MAX_PATH}",
    );
    assert_eq!(cwd_len(), start, "cwd moved despite every chdir being refused");
}

/// The same growth driven by *many small* components rather than one huge one.
///
/// This is the shape that drives `normalize`'s `Vec<&str>` (16 bytes per
/// component) rather than the joined `String`, and it is the one a path bound
/// must also cover: without it, `"a/a/a/…"` reaches the same ceiling through a
/// different allocation.
fn drive_component_growth() {
    let many = "b/".repeat(HUGE_COMPONENT / 2);

    let before = cwd_len();
    let _ = fs::create_dir_all(&many);
    let refused = std::env::set_current_dir(&many).is_err();

    assert!(refused, "chdir accepted {} bytes of components", many.len());
    assert_eq!(cwd_len(), before, "cwd moved despite the chdir being refused");
}

/// The bound must refuse over-long paths without refusing legal ones.
///
/// Without this the test would pass just as well against a `cd` that always
/// returned an error, which would be a broken kernel rather than a fixed one.
fn legal_nesting_still_works() {
    let root = format!("{DIR}/deep");
    fs::create_dir_all(&root).expect("create_dir_all deep");
    std::env::set_current_dir(&root).expect("chdir into a legal directory");

    let mut depth = 0usize;
    // Components small enough that several hundred still fit inside MAX_PATH.
    for _ in 0..32 {
        let name = "n".repeat(64);
        let _ = fs::create_dir(&name);
        if std::env::set_current_dir(&name).is_err() {
            break;
        }
        depth += 1;
    }

    assert!(
        depth >= 16,
        "only {depth} legal nested chdirs succeeded; the bound is refusing paths it should accept",
    );
    let len = cwd_len();
    assert!(len <= MAX_PATH, "legal nesting produced a {len}-byte cwd");
    assert!(len > 1024, "legal nesting only reached {len} bytes — not exercising the bound");
}

/// The kernel heap is intact and the VFS still resolves paths correctly.
///
/// The defect's signature is a panic taken while holding the allocator lock, so
/// "the kernel is still allocating" is the assertion that matters — and a
/// resolved path that still finds its file proves the bound did not truncate
/// anything into naming a different one.
fn kernel_still_healthy() {
    std::env::set_current_dir("/").expect("chdir back to /");

    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for i in 0..256 {
        blocks.push(vec![(i % 251) as u8; 4096]);
    }
    for (i, b) in blocks.iter().enumerate() {
        assert!(b.iter().all(|&x| x == (i % 251) as u8), "kernel heap corrupted block {i}");
    }

    let probe = format!("{DIR}/probe.txt");
    fs::write(&probe, b"resolved").expect("write probe");
    std::env::set_current_dir(DIR).expect("chdir into DIR");
    let via_relative = fs::read("probe.txt").expect("read probe by relative path");
    assert_eq!(via_relative, b"resolved", "relative resolution found the wrong file");
    std::env::set_current_dir("/").expect("chdir back to /");
}

/// `getcwd` reports the length the path needs, and writes nothing when it does
/// not fit.
///
/// The old contract returned `min(cwd.len(), buf.len())` after writing a
/// prefix, so "fit exactly" and "silently truncated" were the same answer.
/// That is how `std::env::current_dir` — which passes a fixed 256-byte buffer —
/// came to hand back a shorter, valid-looking path to a *different* directory.
fn getcwd_contract() {
    let root = format!("{DIR}/getcwd");
    fs::create_dir_all(&root).expect("create getcwd dir");
    std::env::set_current_dir(&root).expect("chdir into getcwd dir");

    let truth = cwd_len();
    assert!(truth > 8, "cwd is implausibly short: {truth}");

    // An empty buffer is a pure size query: same answer, nothing touched.
    let mut empty: [u8; 0] = [];
    assert_eq!(
        toyos_abi::syscall::getcwd(&mut empty),
        truth,
        "an empty buffer must still report the required length",
    );

    // A buffer one byte short must report the requirement and write *nothing*:
    // a partial path names the wrong directory.
    let mut small = vec![0xAAu8; truth - 1];
    let n = toyos_abi::syscall::getcwd(&mut small);
    assert_eq!(n, truth, "a short buffer must report the required length, got {n}");
    assert!(
        small.iter().all(|&b| b == 0xAA),
        "a short buffer was written to; a truncated path is a path to the wrong directory",
    );

    // Exactly enough succeeds, and the answer round-trips.
    let mut exact = vec![0u8; truth];
    assert_eq!(toyos_abi::syscall::getcwd(&mut exact), truth);
    assert_eq!(
        core::str::from_utf8(&exact).expect("cwd is utf-8"),
        root,
        "getcwd did not return the directory we chdir'd into",
    );
}

/// The bug the contract change exists to fix, end to end.
///
/// A cwd past std's fixed 256-byte buffer used to come back truncated, so
/// `current_dir()` named a directory the process was not in and every path
/// built from it pointed somewhere wrong. Now std allocates and retries.
fn current_dir_survives_a_long_cwd() {
    std::env::set_current_dir(DIR).expect("chdir into DIR");
    let mut depth = 0;
    while cwd_len() <= 300 {
        let name = "d".repeat(64);
        let _ = fs::create_dir(&name);
        if std::env::set_current_dir(&name).is_err() {
            break;
        }
        depth += 1;
    }
    let truth_len = cwd_len();
    assert!(
        truth_len > 256,
        "could not build a cwd past std's 256-byte buffer (got {truth_len} at depth {depth})",
    );

    let via_std = std::env::current_dir().expect("current_dir with a long cwd");
    assert_eq!(
        via_std.to_string_lossy().len(),
        truth_len,
        "current_dir returned a {}-byte path for a {truth_len}-byte cwd — still truncating",
        via_std.to_string_lossy().len(),
    );
    std::env::set_current_dir("/").expect("chdir back to /");
}

/// `mkdir` reports a path it refuses instead of returning success.
///
/// `sys_mkdir` discarded `create_dir`'s outcome and always returned 0, so a
/// bound added without changing that return would have been a silent failure:
/// the caller told nothing, the directory simply absent.
fn mkdir_refuses_overlong() {
    std::env::set_current_dir("/").expect("chdir to /");
    let huge = format!("{DIR}/{}", "m".repeat(HUGE_COMPONENT));
    let err = fs::create_dir(&huge)
        .expect_err("mkdir accepted a path longer than MAX_PATH");
    // The point is that it is reported at all, rather than reported as success.
    let _ = err;
}

fn main() {
    fs::create_dir_all(DIR).expect("create /home/abuse_cwd");
    std::env::set_current_dir(DIR).expect("chdir into DIR");

    drive_cwd_growth();
    drive_component_growth();
    legal_nesting_still_works();
    getcwd_contract();
    current_dir_survives_a_long_cwd();
    mkdir_refuses_overlong();
    kernel_still_healthy();

    let _ = fs::remove_dir_all(DIR);
    println!("cwd growth bounded, kernel intact");
}
