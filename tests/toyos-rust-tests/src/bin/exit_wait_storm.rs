//! The park class nothing else exercises: waiting for a child, and joining a
//! thread, under volume.
//!
//! `specs/completion-architecture-spec.md` §20.4. §4.1 collapses P7 (child
//! exit) and P8 (thread exit) onto parks on the process object and on the
//! thread's own watch, and **no other gate reaches either**:
//! `blocking_read_stress` is pipes, `cancel_while_parked` and
//! `killed_holder_releases` are disk and VFS. The tree's existing coverage is
//! ordering rather than volume — `process_lifecycle` has one arm on the wake
//! and `std_threading` joins four threads.
//!
//! **The verdict is a count of collected exit codes inside a bound**, the same
//! shape `blocking_read_stress` takes and for the same reason: a lost publish
//! must red as a number rather than as a stall the suite names apart and
//! nobody bisects.
//!
//! Every child is this binary with an argument, so what the parent waits for is
//! a real process exit and not a stub.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Children spawned before any of them is waited for. They exit while the
/// parent is parked, which is the ordering the park is about — a child that
/// had already exited would be answered from the object's published value and
/// never park at all.
const CHILDREN: u32 = 24;

/// Threads joined in a fan-in. Each exits on its own schedule, so the joiner
/// parks on some of them and not on others, and the count is what says every
/// one of those parks ended.
const THREADS: u32 = 24;

/// What the whole storm must fit in.
const BOUND: Duration = Duration::from_secs(3);

fn main() {
    if let Some(code) = std::env::args().nth(1) {
        // The child half: exit with the code the parent chose, after a moment
        // that is long enough for the parent to be parked on it.
        thread::sleep(Duration::from_millis(2));
        std::process::exit(code.parse::<i32>().expect("the parent passes an integer"));
    }

    let exe = std::env::current_exe().expect("current_exe failed");

    // As `blocking_read_stress`: a lost publish must red as a count and never
    // as the `STALL` §20.3 disqualifies.
    static COLLECTED: AtomicU32 = AtomicU32::new(0);
    static JOINED: AtomicU32 = AtomicU32::new(0);
    thread::spawn(|| {
        thread::sleep(BOUND);
        // Ends the process rather than this thread, for
        // `blocking_read_stress`'s reason: a thread panic leaves `main` parked
        // on the publish that never came, and the harness reports a stall.
        eprintln!(
            "exit_wait_storm: {} of {CHILDREN} exits collected and {} of {THREADS} threads \
             joined inside {BOUND:?} — a publish was not delivered",
            COLLECTED.load(Ordering::Relaxed),
            JOINED.load(Ordering::Relaxed),
        );
        std::process::exit(1);
    });

    let started = Instant::now();

    // Every child is spawned before any is waited for, so the waits find them
    // running and park.
    let mut children = Vec::new();
    for i in 0..CHILDREN {
        children.push((
            i,
            Command::new(&exe)
                .arg((i % 100).to_string())
                .spawn()
                .unwrap_or_else(|e| panic!("spawn child {i}: {e}")),
        ));
    }

    let mut collected = 0u32;
    for (i, mut child) in children {
        let status = child.wait().unwrap_or_else(|e| panic!("wait for child {i}: {e}"));
        assert_eq!(
            status.code(),
            Some((i % 100) as i32),
            "child {i} answered with another process's code",
        );
        collected += 1;
        COLLECTED.store(collected, Ordering::Relaxed);
    }

    // The thread half: each thread returns its own number, and the join is the
    // park.
    let joins: Vec<_> = (0..THREADS)
        .map(|i| {
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(u64::from(i % 3)));
                i
            })
        })
        .collect();
    let mut joined = 0u32;
    for (i, handle) in joins.into_iter().enumerate() {
        let got = handle.join().unwrap_or_else(|_| panic!("join thread {i}"));
        assert_eq!(got, i as u32, "thread {i} answered for another one");
        joined += 1;
        JOINED.store(joined, Ordering::Relaxed);
    }

    let elapsed = started.elapsed();
    assert_eq!(collected, CHILDREN, "only {collected} of {CHILDREN} exits were collected");
    assert_eq!(joined, THREADS, "only {joined} of {THREADS} threads were joined");
    assert!(
        elapsed < BOUND,
        "the storm took {elapsed:?}, past the {BOUND:?} bound — a publish is being waited out \
         rather than delivered",
    );
    println!("exit_wait_storm: {collected} exits collected and {joined} threads joined in {elapsed:?}");
}
