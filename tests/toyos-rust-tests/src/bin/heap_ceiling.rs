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

// `SYS_DEBUG` actions a `test-actuators` kernel provides. The first three take
// one kernel heap allocation each and release it again — at
// `mm::MAX_HEAP_ALLOC`, at `mm::PAGE_2M`, and at `MAX_HEAP_ALLOC` with
// 4096-byte alignment; the last lowers `SYS_SYSINFO`'s thread bound to a count
// this guest can reach.
use toyos_abi::syscall::debug_action::{
    HEAP_AT_CEILING, HEAP_AT_CEILING_PAGE_ALIGNED, HEAP_OVER_CEILING, LOWER_SYSINFO_BOUND,
};

/// `SyscallError::ResourceExhausted`, as `SyscallError::to_u64` encodes it.
const RESOURCE_EXHAUSTED: u64 = u64::MAX - 7;

fn main() {
    at_ceiling_is_servable();
    aligned_at_ceiling_is_refused_not_fatal();
    sysinfo_refuses_rather_than_allocating_past_the_ceiling();
    over_ceiling_kills_only_the_caller();
    heap_still_works();
    println!("all heap ceiling tests passed");
}

/// A syscall whose allocation is derived from something userland grows.
///
/// `SYS_SYSINFO` collects one 24-byte entry per live thread so it can sort
/// them, and the caller's buffer bounds what is *written*, not what is built.
/// Nothing caps the thread count, so ~87,000 threads made an ordinary syscall
/// ask the heap for more than `MAX_HEAP_ALLOC` and trip the assert three
/// functions above — from any process, with no privilege.
///
/// [`LOWER_SYSINFO_BOUND`] puts 16 in `MAX_SYSINFO_THREADS`'s place, because
/// 65,536 threads is 8 GiB of kernel stacks and no guest can make them. The
/// count, the comparison and the refusal are the shipped ones.
///
/// **Armed here rather than compiled in, and the arming is itself an
/// assertion**: the bound is the shipped 65,536 until this call, so a kernel
/// that answered it and did nothing would fail at the loop below rather than
/// pass. As a `#[cfg]` the 16 rode into every kernel the suite booted, and
/// `SYS_SYSINFO` answered against it in every guest.
fn sysinfo_refuses_rather_than_allocating_past_the_ceiling() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    assert!(sysinfo_answers(), "sysinfo already refuses with no threads of ours");
    let rc = toyos_abi::syscall::debug(LOWER_SYSINFO_BOUND);
    assert_eq!(rc, 0, "SYS_DEBUG {LOWER_SYSINFO_BOUND} did not lower the bound (rc={rc:#x})");

    let stop = Arc::new(AtomicBool::new(false));
    let mut parked = Vec::new();
    let mut refused_at = None;
    // Past the bound with room, and far short of anything that would matter
    // to a guest with one CPU.
    for i in 0..64 {
        let flag = Arc::clone(&stop);
        parked.push(std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }));
        if !sysinfo_answers() {
            refused_at = Some(i + 1);
            break;
        }
    }

    let at = refused_at.unwrap_or_else(|| {
        stop.store(true, Ordering::Relaxed);
        panic!("64 extra threads and sysinfo never refused — its collection is unbounded")
    });

    stop.store(true, Ordering::Relaxed);
    for t in parked {
        t.join().expect("join a parked thread");
    }
    // A bound, not a one-way door: with the threads gone it answers again.
    assert!(sysinfo_answers(), "sysinfo stayed refused after the threads exited");
    println!("  PASS: sysinfo refused past its bound at {at} extra threads, and recovered");
}

/// Whether `SYS_SYSINFO` filled its header. The ABI wrapper reports an error
/// as `0`, and the header is the smallest buffer it accepts.
fn sysinfo_answers() -> bool {
    let mut buf = [0u8; 48];
    toyos_abi::syscall::sysinfo(&mut buf) == buf.len()
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
    let rc = toyos_abi::syscall::debug(HEAP_AT_CEILING);
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
    let rc = toyos_abi::syscall::debug(HEAP_AT_CEILING_PAGE_ALIGNED);
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
        .arg(HEAP_OVER_CEILING.to_string())
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
