//! The kernel heap's ceiling, and what it costs the machine to cross it.
//!
//! `KernelPageSource` hands dlmalloc one 2 MiB page and can hand it no more,
//! so `mm::MAX_HEAP_ALLOC` is the largest single allocation the kernel heap
//! can serve. Asking for more is a kernel bug and dies loudly — but the check
//! used to live *inside* `KernelAllocator::alloc`'s lock, and the kernel does
//! not unwind, so the heap stayed locked and the CPU that recovered from the
//! panic spun forever on its next allocation or free. Reporting the bug cost
//! the machine.
//!
//! Three cases, because no one of them says it alone: the ceiling is servable,
//! a request the page source cannot back is refused rather than fatal, and one
//! past the ceiling kills its caller and nothing else.

use std::process::Command;

/// `SYS_DEBUG` actions the `test-heap-ceiling` kernel provides. Each takes one
/// kernel heap allocation and releases it: at `mm::MAX_HEAP_ALLOC`, at
/// `mm::PAGE_2M`, and at `MAX_HEAP_ALLOC` with 4096-byte alignment.
const AT_CEILING: u64 = 5;
const OVER_CEILING: u64 = 6;
const AT_CEILING_PAGE_ALIGNED: u64 = 7;

/// `SyscallError::ResourceExhausted`, as `SyscallError::to_u64` encodes it.
const RESOURCE_EXHAUSTED: u64 = u64::MAX - 7;

fn main() {
    at_ceiling_is_servable();
    aligned_at_ceiling_is_refused_not_fatal();
    over_ceiling_kills_only_the_caller();
    heap_still_works();
    println!("all heap ceiling tests passed");
}

/// The documented ceiling is a size the heap actually serves.
///
/// This process makes the call itself, so a kernel that asserts here, or an
/// allocation that comes back null, kills this test. `MAX_HEAP_ALLOC` is
/// `PAGE_2M - 4096` and the 4 KiB is headroom for dlmalloc's own chunk and
/// segment bookkeeping — arithmetic that was reasoned about and never run.
///
/// It is also the negative side of the case below it: an assert that simply
/// refused every large allocation would satisfy that one and fail this.
fn at_ceiling_is_servable() {
    let rc = toyos_abi::syscall::debug(AT_CEILING);
    assert_eq!(
        rc, 0,
        "an allocation at MAX_HEAP_ALLOC was refused (rc={rc:#x}) — the documented \
         ceiling is above the real one"
    );
    println!("  PASS: MAX_HEAP_ALLOC is servable");
}

/// The same size, page-aligned, is more than the page source can back — and
/// that is an error return, not a dead machine.
///
/// This is the case that proves the ceiling and the lock were two defects and
/// not one. `memalign` pads by the alignment before it asks for backing, so
/// this request satisfies `MAX_HEAP_ALLOC` and still reaches the page source
/// asking for 2,162,688 bytes. Measured against the old code: it panicked
/// inside `Dlmalloc::malloc`, with the allocator lock held, and the guest went
/// silent — so no bound at the entry could ever have closed it.
fn aligned_at_ceiling_is_refused_not_fatal() {
    let rc = toyos_abi::syscall::debug(AT_CEILING_PAGE_ALIGNED);
    assert_eq!(
        rc, RESOURCE_EXHAUSTED,
        "a page-aligned allocation at MAX_HEAP_ALLOC returned {rc:#x}; expected the \
         page source to refuse it"
    );
    println!("  PASS: an allocation the page source cannot back is refused, not fatal");
}

/// One page over the ceiling: the caller dies, and nothing else does.
fn over_ceiling_kills_only_the_caller() {
    let status = Command::new("/bin/test_rs_test_panic_child")
        .arg(OVER_CEILING.to_string())
        .status()
        .expect("failed to spawn child");
    assert!(
        !status.success(),
        "a 2 MiB kernel heap allocation should have panicked the kernel and killed the child"
    );
    println!("  PASS: over-ceiling allocation killed the caller (exit={})",
        status.code().unwrap_or(-1));
}

/// The property the whole test exists for: the CPU that recovered from that
/// panic can still allocate and free.
///
/// Reaching this line is already most of the evidence — `status()` above only
/// returns once the kernel has reaped the dead child, which takes the idle
/// loop through `reap_poisoned`. A spawn is the loudest confirmation userland
/// can give: process table entry, fd table, ELF load and the whole teardown,
/// all of it kernel heap traffic, and on this guest all of it on the one CPU
/// that recovered.
fn heap_still_works() {
    let output = Command::new("/bin/echo")
        .arg("still alive")
        .output()
        .expect("failed to run echo after the over-ceiling panic");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "still alive");
    println!("  PASS: the kernel heap still allocates and frees after recovery");
}
