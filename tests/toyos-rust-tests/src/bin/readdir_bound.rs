//! `read_dir` must return every entry or an error, never a short listing.
//!
//! Two defects, and the second is what makes the first's bound honest. The
//! kernel built a directory listing with one entry per file and no cap, so a
//! `read_dir` over 32,769 files in one tmpfs directory panicked it — measured,
//! 1.8 s, from `fs::write` in a loop. And `SYS_READDIR` filled the caller's
//! buffer, stopped, and reported the bytes it had written, which is
//! indistinguishable from a complete listing: 4125 entries of 34,816, as
//! success. A bound plus a silent truncation is a quieter version of the same
//! defect, so both are asserted here.
//!
//! Everything below is an ordinary workload — no kernel feature, no injection.

use std::fs;
use std::process::Command;

/// `vfs::MAX_LIST_ENTRIES`. The kernel refuses a listing above it; this test
/// exists partly to prove the number is not above the real ceiling, so it must
/// track the kernel's.
const MAX_LIST_ENTRIES: usize = 16_384;

/// Enough plain entries that the encoded listing cannot fit std's first
/// buffer (65,536 bytes). At ~15 bytes an entry this is ~90 KB, so a
/// truncating kernel returns about 4,000 of them.
const PLAIN_ENTRIES: usize = 6_000;

fn main() {
    plain_entries_are_all_returned();
    subdirectories_at_the_limit();
    one_past_the_limit_is_refused();
    system_alive();
    println!("all readdir bound tests passed");
}

/// A listing larger than the caller's buffer comes back whole.
///
/// Plain file entries, in their own subdirectory, so this exercises the
/// `result` path rather than the dedup one. The count is the assertion: a
/// truncating kernel returns a valid-looking prefix of it.
fn plain_entries_are_all_returned() {
    for i in 0..PLAIN_ENTRIES {
        fs::write(format!("/tmp/a/f{i}"), b"").expect("create failed");
    }
    let n = fs::read_dir("/tmp/a").expect("read_dir failed").count();
    assert_eq!(n, PLAIN_ENTRIES, "read_dir truncated: {n} entries of {PLAIN_ENTRIES}");
    println!("  PASS: {n} entries returned whole, past a 65,536-byte buffer");

    for i in 0..PLAIN_ENTRIES {
        fs::remove_file(format!("/tmp/a/f{i}")).expect("remove failed");
    }
}

/// Exactly `MAX_LIST_ENTRIES` entries, in the shape that costs the most.
///
/// Every name is `d<i>/f`, so listing `/tmp` puts one entry per distinct
/// subdirectory into the kernel's dedup set — the allocation the bound is
/// derived against, at the count it is derived for. It succeeding is what says
/// the bound is not above the real ceiling; deriving it on paper does not.
fn subdirectories_at_the_limit() {
    for i in 0..MAX_LIST_ENTRIES {
        fs::write(format!("/tmp/d{i}/f"), b"").expect("create failed");
    }
    let n = fs::read_dir("/tmp").expect("read_dir at the limit failed").count();
    assert_eq!(n, MAX_LIST_ENTRIES, "listing at the limit returned {n}");
    println!("  PASS: {n} distinct subdirectories listed at the limit");
}

/// One more, and the answer is an error rather than a panic or a short list.
fn one_past_the_limit_is_refused() {
    fs::write(format!("/tmp/d{MAX_LIST_ENTRIES}/f"), b"").expect("create failed");
    match fs::read_dir("/tmp") {
        Ok(it) => panic!("listing past the limit returned {} entries", it.count()),
        Err(e) => println!("  PASS: one past the limit refused ({e})"),
    }
}

/// The refusal cost the caller an error and nothing else. A panic inside
/// `Vfs::list` would have stranded the VFS lock, so a spawn — which reads the
/// binary through the same lock — is the check that the filesystem still
/// works, not just that this process is still running.
fn system_alive() {
    let output = Command::new("/bin/echo")
        .arg("still alive")
        .output()
        .expect("failed to run echo after the refusal");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "still alive");
    println!("  PASS: the VFS still serves after the refusal");
}
