//! What a transition out of Ring 3 costs, in cycles.
//!
//! Two paths, because `specs/user-machine-state.md` §11 needs them attributed
//! separately: the syscall entry, and the exception entry through a demand
//! page fault. Both are the paths `arch::entry`'s bracket sits on.
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

use toyos_abi::syscall::{clock_nanos, mmap, munmap, MmapFlags, MmapProt};

const REPS: usize = 9;
const SYSCALLS_PER_REP: u64 = 20_000;

/// 2 MiB pages, so this many faults is this many private-page allocations.
const FAULT_PAGES: usize = 64;
const PAGE_2M: usize = 2 * 1024 * 1024;

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

/// Cycles per demand page fault on a freshly mapped anonymous region.
///
/// One write per 2 MiB page, so every iteration is exactly one `#PF` through
/// `common_entry` plus the page the kernel allocates behind it.
fn page_fault_cycles() -> Option<u64> {
    let size = PAGE_2M * FAULT_PAGES;
    let base = unsafe {
        mmap(
            core::ptr::null_mut(),
            size,
            MmapProt::READ | MmapProt::WRITE,
            MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
        )
    };
    if base.is_null() {
        return None;
    }
    let start = rdtsc();
    for page in 0..FAULT_PAGES {
        unsafe { core::ptr::write_volatile(base.add(page * PAGE_2M), 1u8) };
    }
    let end = rdtsc();
    let _ = unsafe { munmap(base, size) };
    Some((end - start) / FAULT_PAGES as u64)
}

fn main() {
    // A first pass nobody reads: the loop's own pages have to be faulted in
    // before the number is about the syscall rather than about the text.
    syscall_cycles();

    let syscall = (0..REPS).map(|_| syscall_cycles()).min().unwrap();
    let fault = (0..REPS).filter_map(|_| page_fault_cycles()).min();

    // The clock alongside the cycles, because a TSC that does not tick at a
    // fixed rate makes the cycle counts incomparable and this is the only
    // thing in the run that would show it.
    let t0 = clock_nanos();
    let c0 = rdtsc();
    while clock_nanos() - t0 < 20_000_000 {}
    let hz = (rdtsc() - c0) * 1_000_000_000 / (clock_nanos() - t0);

    println!("syscall_cost: {syscall} cycles/syscall over {REPS}x{SYSCALLS_PER_REP}");
    match fault {
        Some(f) => println!("syscall_cost: {f} cycles/pagefault over {REPS}x{FAULT_PAGES}"),
        None => println!("syscall_cost: no pagefault measurement — mmap refused"),
    }
    println!("syscall_cost: tsc {} MHz", hz / 1_000_000);
}
