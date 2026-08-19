//! `SYS_SET_THREAD_NAME` used to clamp an oversized name with `(a2 as
//! usize).min(THREAD_NAME_LEN)` and set the truncated prefix — a silent
//! clamp, and the shape
//! `issues/isolation/untrusted-sites-not-yet-adopted.md` named for the
//! whole of `kernel/src/arch/syscall.rs`. `Untrusted::at_most` replaced it
//! with a refusal, which is a behaviour change worth its own gate: this
//! proves the refusal actually fires, rather than the clamp it replaced.
//!
//! `toyos_abi::syscall::set_thread_name` throws its own return value away —
//! it always has, and that is not this test's to fix — so the return code
//! cannot be what proves the refusal. `SYS_SYSINFO` is the only other way
//! userland reads a thread's name back, and it is enough: this checks that a
//! call over the bound leaves the name exactly as it was, never a silently
//! truncated prefix nobody asked for. The old clamp would have this reading
//! back a truncated `over` after the second call; the refusal reads back
//! `at_bound`, unchanged.

use toyos_abi::syscall;

const THREAD_NAME_LEN: usize = 28;
const HEADER_SIZE: usize = 48;
const ENTRY_SIZE: usize = 64;
/// Comfortably past any thread count the shared boot runs concurrently — the
/// parallel phase widths seen in CI are single digits.
const SYSINFO_CAP: usize = 1024;

/// This process's own main-thread name field out of `SYS_SYSINFO`.
///
/// `None` only if this thread's own entry is missing from the answer, which
/// `SYSINFO_CAP` is sized well past — a `None` here is `SYSINFO_CAP` too
/// small, not evidence about the syscall under test.
fn own_name() -> Option<[u8; THREAD_NAME_LEN]> {
    let pid = syscall::getpid().0;
    let mut buf = vec![0u8; HEADER_SIZE + SYSINFO_CAP * ENTRY_SIZE];
    let n = syscall::sysinfo(&mut buf);
    assert!(n >= HEADER_SIZE, "sysinfo returned {n} bytes, need at least the {HEADER_SIZE}-byte header");

    let mut off = HEADER_SIZE;
    while off + ENTRY_SIZE <= n {
        let entry = &buf[off..off + ENTRY_SIZE];
        let entry_pid = u32::from_le_bytes(entry[0..4].try_into().unwrap());
        // `is_thread` is 0 for the main thread — this binary spawns no other,
        // so that is the only entry `entry_pid == pid` names.
        let is_thread = entry[9];
        if entry_pid == pid && is_thread == 0 {
            let mut name = [0u8; THREAD_NAME_LEN];
            name.copy_from_slice(&entry[32..32 + THREAD_NAME_LEN]);
            return Some(name);
        }
        off += ENTRY_SIZE;
    }
    None
}

fn main() {
    // At exactly the bound: still accepted — `at_most` is inclusive, and a
    // name that fills the field exactly is the ordinary case, not an edge one.
    let at_bound = [b'a'; THREAD_NAME_LEN];
    syscall::set_thread_name(&at_bound);
    assert_eq!(
        own_name().expect("this thread's own entry is in SYS_SYSINFO's answer"),
        at_bound,
        "SYS_SYSINFO did not read back the name just set",
    );
    println!("  at the bound: accepted, and reads back whole");

    // One byte over: before this conversion this call silently set the first
    // THREAD_NAME_LEN bytes of `over` — indistinguishable, from calling code
    // that does not check the (discarded) return value, from asking for
    // `at_bound` again. `Untrusted::at_most` refuses it instead, and this is
    // the only way to see that from userland: the name is still `at_bound`.
    let mut over = [b'b'; THREAD_NAME_LEN + 1];
    over[THREAD_NAME_LEN] = b'!';
    syscall::set_thread_name(&over);
    assert_eq!(
        own_name().expect("this thread's own entry is in SYS_SYSINFO's answer"),
        at_bound,
        "a SYS_SET_THREAD_NAME call one byte over THREAD_NAME_LEN changed the name anyway — \
         the clamp is back",
    );
    println!("  one byte over: refused, and the name it had is still the name it has");

    println!("SYS_SET_THREAD_NAME refuses a name over THREAD_NAME_LEN rather than truncating it");
}
