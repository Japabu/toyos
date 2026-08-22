//! A Ring 3 loop that is inside `SYSCALL` as often as a program can be.
//!
//! **The victim half of `syscall_window_nmi`.** The kernel's storm sends NMIs
//! from another CPU; where each one lands is decided by this loop's timing, and
//! the window it has to land in is the three instructions of `arch::syscall`'s
//! entry that run at CPL 0 on this stack plus the one between its `pop rsp` and
//! its `sysretq`. Nothing here asserts: the counts are the kernel's, and
//! `tests/common/faults.rs` holds the verdict.
//!
//! **The loop is written as assembly because its instruction count is part of
//! the derivation.** Four instructions per iteration — `mov`, `syscall`, `dec`,
//! `jnz` — so the boundaries at which an NMI can be delivered while this program
//! is in Ring 3 are four per iteration, exactly as many as the window has. The
//! expected number of window arrivals is therefore the number of Ring 3
//! arrivals, and the gate asserts against that ratio rather than against a
//! measurement. A `for` loop over `getpid()` would put the count at the mercy of
//! whatever the optimiser did that day.
//!
//! `SYS_GETPID` because it is the cheapest thing the kernel answers that takes
//! no argument and touches no state: what should dominate the iteration is the
//! entry and the exit, which is what the window is part of.

use toyos_abi::syscall::{clock_nanos, SYS_GETPID};

/// Iterations between two clock reads. Large enough that the clock's own
/// syscall is a rounding error in the mix, small enough to stop promptly.
const CHUNK: u64 = 50_000;

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(10);

    println!("nmi-window-spin: spinning on SYS_GETPID for {secs}s");

    let started = clock_nanos();
    let until = started + secs * 1_000_000_000;
    let mut done: u64 = 0;
    while clock_nanos() < until {
        chunk();
        done += CHUNK;
    }
    let elapsed = clock_nanos() - started;

    println!(
        "nmi-window-spin: {done} syscalls in {elapsed} ns ({} ns each)",
        elapsed / done.max(1),
    );
}

/// [`CHUNK`] iterations of exactly four instructions.
fn chunk() {
    let mut n = CHUNK;
    // SAFETY: the ABI's own `syscall` sequence for `SYS_GETPID` — the number in
    // `rdi`, the answer in `rax`, `rcx` and `r11` clobbered by the instruction
    // itself — around a counted loop. Every register it writes is declared, the
    // three the syscall clobbers by name so the allocator cannot put the counter
    // in one, and it touches no memory: `nostack` is true because this kernel's
    // entry parks `rsp` in per-CPU data rather than pushing on it. Irreducible
    // for the reason the module header gives: the instruction count is the
    // derivation, and no Rust loop has a stated one.
    unsafe {
        core::arch::asm!(
            "2:",
            "mov edi, {num}",
            "syscall",
            "dec {n}",
            "jnz 2b",
            num = const SYS_GETPID,
            n = inout(reg) n,
            out("rax") _,
            out("rcx") _,
            out("r11") _,
            out("rdi") _,
            options(nostack),
        );
    }
    assert_eq!(n, 0, "the counted loop did not run to zero");
}
