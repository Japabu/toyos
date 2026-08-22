//! **What raising a wake storm costs the thread that raises it.**
//!
//! `completion::post_n` walks the waiters armed on one word, claims each one
//! and posts a message to its home CPU — one loop, on the caller's CPU, before
//! the syscall returns. `toyos-sched/sim`'s wakeup-storm case measures how long
//! the *waiters* then take to reach a CPU and can say nothing whatever about
//! that loop: the model's clock does not advance inside a step, so raising a
//! storm of any size is free there. The cost is a guest measurement, and this
//! is it.
//!
//! **The derivation, off the loop itself.** The body is a token comparison, a
//! record store and one `claim_wake` per waiter, so the cost is
//! `fixed + N × per-waiter` — linear in the waiters on the word, with a
//! constant that has nothing to do with N. That shape is what is asserted:
//! quadrupling the storm may cost four times the loop, and a quarter more on
//! top, which is the same allowance `a_wakeup_storm_drains_without_serializing`
//! states about the drain. Anything past it is a term that grows with the
//! storm and is not the walk — a lock somebody takes per waiter, a scan of the
//! whole bucket per claim.
//!
//! **The absolute numbers are printed and never asserted**, for
//! `syscall_cost`'s reason: a threshold measured under TCG is meaningless —
//! QEMU prices an uncontended atomic read-modify-write nothing like silicon,
//! and this loop is made of them — and one measured on metal drifts. What
//! transfers is the *ratio*, which is dimensionless.
//!
//! **The instrument is `rdtsc` around the syscall, and it is the tree's
//! own.** `SYS_PROCESS_STATS` answers per *process*, and the waker and its 64
//! waiters are one process here; the scheduler dump reports queues rather than
//! costs; and a per-waiter kernel counter would be a kernel addition. What a
//! syscall costs its caller is exactly what `syscall_cost` reads this way.
//!
//! **What it measured**, on this host's TCG guest at `--smp 2`, minimum of nine
//! timed calls per width, two runs of the same tree:
//!
//! | waiters | run 1 | run 2 |
//! |---|---|---|
//! |  1 |  7,000 |  7,000 |
//! |  2 |  7,000 |  8,000 |
//! |  4 |  7,000 | 11,000 |
//! |  8 | 10,000 | 18,000 |
//! | 16 | 14,000 | 20,000 |
//! | 32 | 23,000 | 21,000 |
//! | 64 | 33,000 | 38,000 |
//!
//! So **raising a 64-waiter storm costs its waker 33–38 µs of its own CPU** at
//! this guest's 998 MHz, which is a third of one percent of a 10 ms quantum,
//! and the marginal cost is 412–492 cycles per waiter claimed. Quadrupling the
//! storm from 16 to 64 costs 2.4× and 1.9× the walk rather than 4×, because the
//! fixed part of the call is a third of the 16-waiter figure.
//!
//! **The middle of the sweep is not monotone between runs** — 32 waiters read
//! under 8 in the second — and the reason is the instrument rather than the
//! kernel: this guest's TSC advances in steps of about a thousand cycles, so
//! the resolution is a microsecond and the whole per-waiter term below eight
//! waiters is inside it, and a minimum of nine calls on a two-CPU guest sharing
//! a host with eleven others still carries the host's descheduling. The two
//! endpoints are five times apart, which is why the assertions are stated at 1,
//! 16 and 64 and never between neighbours.
//!
//! **Every waiter is proved parked before the call that is timed**, by the
//! answer that call gives: a wake with no limit over `want` parked, unclaimed
//! waiters answers `want` under every arithmetic, and one that answers anything
//! else is discarded rather than measured. The word never changes while the
//! storms are raised, so a claimed waiter re-derives its predicate, finds it
//! false and re-parks — which is what makes the same word measurable at seven
//! widths in one run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use toyos_abi::syscall::{self, clock_nanos};

/// The widths measured. 64 is the storm the simulator's case raises, and the
/// powers of two below it are what makes the shape readable rather than a
/// single number.
const WIDTHS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];

/// Timed calls kept per width. The **minimum** is reported, for
/// `syscall_cost`'s reason: it is the repetition with the least interference,
/// and on a host running eleven other guests interference is all a mean
/// measures.
const REPS: usize = 9;

/// Calls to make per width before giving up on getting [`REPS`] of them to
/// answer `want`. A call that answers anything else means a waiter had not
/// re-parked yet, which costs a repetition and never a wrong number.
const ATTEMPTS: usize = 400;

/// How long a waiter is given to re-park between two attempts.
const REPARK: Duration = Duration::from_millis(2);

/// The word every waiter parks on. It never changes until the run is over, so
/// every wake is a claim the waiter re-parks from.
static WORD: AtomicU32 = AtomicU32::new(0);

/// Set once at the end, to send every waiter home.
static DONE: AtomicU32 = AtomicU32::new(0);

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

/// One waiter: park on the word, and come home when it finally changes.
fn waiter() {
    while DONE.load(Ordering::SeqCst) == 0 {
        unsafe { syscall::futex_wait(WORD.as_ptr(), 0, None) };
    }
}

/// The cheapest `futex_wake` that claimed exactly `want` waiters, in cycles.
///
/// `None` means the arrangement never held — every attempt found somebody not
/// yet re-parked — which is a measurement that did not happen and is reported
/// as one rather than averaged into the table.
fn storm_cycles(want: usize) -> Option<u64> {
    let mut best = None;
    let mut taken = 0;
    for _ in 0..ATTEMPTS {
        let start = rdtsc();
        let woken = unsafe { syscall::futex_wake(WORD.as_ptr(), u32::MAX) };
        let end = rdtsc();
        if woken as usize == want {
            best = Some(best.map_or(end - start, |b: u64| b.min(end - start)));
            taken += 1;
            if taken == REPS {
                return best;
            }
        }
        thread::sleep(REPARK);
    }
    best
}

/// Wait until a wake with no limit answers `want` — the proof that every
/// waiter has reached its park.
fn wait_until_parked(want: usize) {
    for _ in 0..500 {
        if unsafe { syscall::futex_wake(WORD.as_ptr(), u32::MAX) } as usize == want {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("{want} waiters never parked on the word");
}

fn main() {
    let mut waiters = Vec::new();
    let mut table: Vec<(usize, u64)> = Vec::new();

    for width in WIDTHS {
        while waiters.len() < width {
            waiters.push(thread::spawn(waiter));
        }
        wait_until_parked(width);
        let cycles = storm_cycles(width)
            .unwrap_or_else(|| panic!("no wake of {width} waiters ever found all of them parked"));
        println!("wake_storm_cost: {width} waiters, {cycles} cycles");
        table.push((width, cycles));
    }

    // The clock alongside the cycles, for `syscall_cost`'s reason: a TSC that
    // does not tick at a fixed rate makes the counts incomparable and this is
    // the only thing in the run that would show it.
    let t0 = clock_nanos();
    let c0 = rdtsc();
    while clock_nanos() - t0 < 20_000_000 {}
    let hz = (rdtsc() - c0) * 1_000_000_000 / (clock_nanos() - t0);
    println!("wake_storm_cost: tsc {} MHz", hz / 1_000_000);

    let at = |want: usize| {
        table
            .iter()
            .find(|&&(width, _)| width == want)
            .map(|&(_, cycles)| cycles)
            .unwrap_or_else(|| panic!("{want} waiters were not measured"))
    };
    let (one, small, large) = (at(1), at(16), at(64));
    let marginal = (large - one) / 63;
    println!(
        "wake_storm_cost: {one} cycles at one waiter, {large} at 64 — {marginal} cycles per \
         waiter over the walk, {} ns per waiter",
        marginal * 1_000_000_000 / hz,
    );

    // **The loop is not free**, which is the whole of what the model cannot
    // say: its clock does not advance inside a step, so a 64-waiter storm and a
    // one-waiter wake cost the same there. If they cost the same here, this
    // instrument is not reading the walk at all.
    assert!(
        large > one,
        "a wake claiming 64 waiters cost {large} cycles and one claiming a single waiter \
         {one} — the per-waiter term of the walk is not measurable through this instrument, \
         so nothing below is a statement about it",
    );
    // **And it is linear.** Four times the waiters may cost four times the
    // walk — that is what a loop over them *is* — and this allows a quarter
    // more on top, which is the allowance the simulator's storm case states
    // about the drain for the same reason.
    assert!(
        large <= small * 5,
        "quadrupling the storm from 16 waiters to 64 took the waker's own cost from {small} \
         cycles to {large}. `post_n` walks the waiters once and does a constant amount per \
         claim, so four times the waiters may cost four times the loop and no more; past \
         this, something in the claim grows with the size of the storm",
    );

    DONE.store(1, Ordering::SeqCst);
    WORD.store(1, Ordering::SeqCst);
    while unsafe { syscall::futex_wake(WORD.as_ptr(), u32::MAX) } > 0 {}
    for waiter in waiters {
        waiter.join().expect("a waiter panicked");
    }
    println!("raising a storm costs the waker one walk of its waiters, and it is linear");
}
