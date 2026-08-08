//! A sleep shorter than the LAPIC one-shot can express used to stop the CPU
//! that granted it, permanently, from Ring 3 and with no privilege.
//!
//! `sys_nanosleep` parks with an absolute deadline; the pass arms
//! `deadline - now` on the one-shot. For a deadline already past — and every
//! deadline here is past by the time the pass reaches its arming — that
//! subtraction is zero, and the register cannot hold zero, so the CPU armed
//! the one-tick minimum. On the owner's T14 that tick is 26 ns, which the
//! `wrmsr` doing the arming does not itself finish inside: the interrupt is
//! taken before the next instruction, the Ring 0 stub reloads the *same* count,
//! and the CPU never executes another instruction of anything. Not a hang the
//! scheduler could notice — the machine keeps taking interrupts and the other
//! CPUs keep running, so it looks like a desktop that stopped.
//!
//! The verdict is this process finishing. A CPU that livelocks takes the thread
//! parked on it with it, and no sibling can pick the thread up: only its home
//! CPU owns its deadline.
//!
//! Threads because the wedge is per-CPU: one thread exercises one CPU per park,
//! and a machine-wide claim needs the sweep spread across the machine.
//!
//! **What it certifies on this host, exactly.** TCG checks pending interrupts at
//! translation-block boundaries, so a guest whose one-shot expires before its own
//! arming instruction still retires a whole block per interrupt and walks out of
//! the loop. Measured: this file passes on the tree that has the defect. So here
//! it certifies the syscall path — that every one of these deadlines is honoured
//! and every thread comes back — and it is a T14 boot that certifies the rest.

use std::thread;

/// Every value at or below one LAPIC tick, plus the band just above it where
/// the interrupt still costs more than the interval it was armed for.
const NANOS: &[u64] = &[0, 1, 26, 100, 1_000, 10_000, 100_000];

const ROUNDS: usize = 8;

fn sweep(tag: &str) {
    for &nanos in NANOS {
        for _ in 0..ROUNDS {
            toyos_abi::syscall::nanosleep(nanos);
        }
        println!("  {tag}: {ROUNDS} sleeps of {nanos} ns returned");
    }
}

fn main() {
    let workers: Vec<_> = (0..7)
        .map(|i| thread::spawn(move || sweep(&format!("thread {i}"))))
        .collect();
    sweep("main");
    for (i, worker) in workers.into_iter().enumerate() {
        worker.join().unwrap_or_else(|_| panic!("worker {i} did not come back"));
    }

    // The machine still schedules, not merely this process. A CPU wedged in the
    // timer path answers interrupts and runs nothing, so a thread that has
    // never been dispatched is the shape the freeze leaves behind.
    let late = thread::spawn(|| {
        toyos_abi::syscall::nanosleep(0);
        42u32
    });
    assert_eq!(late.join().expect("a thread spawned after the sweep never ran"), 42);

    println!("a sleep the one-shot cannot express does not stop the CPU that grants it");
}
