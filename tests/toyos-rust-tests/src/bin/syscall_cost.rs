//! What a transition out of Ring 3 costs, in cycles.
//!
//! The syscall entry, which is the busiest of the five places `arch::entry`'s
//! bracket sits. The exception entry pays the same two instructions, and there
//! is no cheap way to measure it here: the only demand-paged memory a userland
//! program can reach is its own file-backed pages — `sys_mmap` allocates and
//! maps its whole region up front — so a workload of N faults costs N × 2 MiB
//! of test image.
//!
//! **It prints and never asserts.** A threshold measured under TCG is
//! meaningless — QEMU implements `FXSAVE` as a helper call and prices nothing
//! like silicon — and one measured on metal drifts. The number is for a
//! same-session A/B against another build of this tree, which is the only
//! comparison that says anything.
//!
//! Reported as the minimum over repetitions rather than the mean: the minimum
//! is the run with the least interference, and on a host running eleven other
//! guests interference is all the mean measures.

use toyos_abi::syscall::clock_nanos;

const REPS: usize = 9;
const SYSCALLS_PER_REP: u64 = 20_000;

fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "lfence",
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack),
        );
    }
    ((hi as u64) << 32) | lo as u64
}

/// Cycles per `SYS_CLOCK`, the cheapest syscall there is: it reads one counter
/// and returns, so what it measures is the entry and the exit.
fn syscall_cycles() -> u64 {
    let start = rdtsc();
    for _ in 0..SYSCALLS_PER_REP {
        std::hint::black_box(clock_nanos());
    }
    let end = rdtsc();
    (end - start) / SYSCALLS_PER_REP
}

fn main() {
    // A first pass nobody reads: the loop's own pages have to be faulted in
    // before the number is about the syscall rather than about the text.
    syscall_cycles();

    let syscall = (0..REPS).map(|_| syscall_cycles()).min().unwrap();

    // The clock alongside the cycles, because a TSC that does not tick at a
    // fixed rate makes the cycle counts incomparable and this is the only
    // thing in the run that would show it.
    let t0 = clock_nanos();
    let c0 = rdtsc();
    while clock_nanos() - t0 < 20_000_000 {}
    let hz = (rdtsc() - c0) * 1_000_000_000 / (clock_nanos() - t0);

    println!("syscall_cost: {syscall} cycles/syscall over {REPS}x{SYSCALLS_PER_REP}");
    println!("syscall_cost: tsc {} MHz", hz / 1_000_000);
}
