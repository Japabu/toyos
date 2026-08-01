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

fn main() {
    fs::create_dir_all(DIR).expect("create /home/abuse_cwd");
    std::env::set_current_dir(DIR).expect("chdir into DIR");

    drive_cwd_growth();
    drive_component_growth();
    legal_nesting_still_works();
    kernel_still_healthy();

    let _ = fs::remove_dir_all(DIR);
    println!("cwd growth bounded, kernel intact");
}
