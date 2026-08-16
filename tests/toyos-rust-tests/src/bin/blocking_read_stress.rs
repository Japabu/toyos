//! The lost-wake canary: a pipe ping-pong across processes, counted.
//!
//! `specs/completion-architecture-spec.md` §20.4. Every round trip here is two
//! parks and two posts on the completion core — the reader blocks in `sys_read`
//! on an empty pipe, the writer's post wakes it, and the same happens back the
//! other way. **The verdict is a count inside a wall-clock bound**, never a
//! hang: §20.3 disqualifies a stall as a verdict, because the harness prints
//! "the guard expired, so this says nothing about the tree" beside one and
//! tells nobody to bisect it. A dropped completion reds as a number that is
//! short of `ROUNDS`, with the round it stopped at named.
//!
//! The echo half is this same binary with an argument, so the two ends are one
//! file and the child's own parks are the same parks the parent's are.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Round trips. Enough that a wake lost at some rate shows up, small enough
/// that the whole thing fits inside the harness's five seconds with room —
/// measured at 500 rounds in 68 ms on the dev host, so this is two orders of
/// magnitude of headroom.
const ROUNDS: u32 = 500;

/// What the round trips must fit in. Not a latency assertion: it is what turns
/// a lost wake into a *number* rather than into a stall the suite names apart.
const BOUND: Duration = Duration::from_secs(3);

fn echo() -> ! {
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            // The parent closed its end: the conversation is over.
            Ok(0) | Err(_) => std::process::exit(0),
            Ok(_) => {}
        }
        if stdout.write_all(&byte).is_err() || stdout.flush().is_err() {
            std::process::exit(0);
        }
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("echo") {
        echo();
    }

    let exe = std::env::current_exe().expect("current_exe failed");
    let mut child = Command::new(&exe)
        .arg("echo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the echo half");
    let mut to_child = child.stdin.take().expect("piped stdin");
    let mut from_child = child.stdout.take().expect("piped stdout");

    // **The watchdog is what makes a lost wake a number.** Without it a
    // dropped completion parks this process for ever, the harness's guard
    // expires, and the suite prints `STALL` — which §20.3 disqualifies as a
    // verdict, because it is the one class the harness names apart and tells
    // nobody to bisect. With it, the round the machine stopped at is the
    // failure message.
    static DONE: AtomicU32 = AtomicU32::new(0);
    thread::spawn(|| {
        thread::sleep(BOUND);
        // **Ends the process, rather than panicking this thread.** A thread
        // panic leaves `main` parked in the read that never came back and the
        // harness times the guest out — which is the stall this watchdog
        // exists to replace. Verified both ways: with the pipe's readable post
        // dropped, `panic!` here produced `timed out after 8s` and this
        // produces the count.
        eprintln!(
            "blocking_read_stress: only {} of {ROUNDS} round trips completed inside {BOUND:?} \
             — a wake was not delivered",
            DONE.load(Ordering::Relaxed),
        );
        std::process::exit(1);
    });

    let started = Instant::now();
    let mut completed = 0u32;
    for round in 0..ROUNDS {
        let sent = [(round % 251) as u8; 1];
        to_child.write_all(&sent).expect("write to the echo half");
        to_child.flush().expect("flush to the echo half");
        let mut got = [0u8; 1];
        from_child
            .read_exact(&mut got)
            .unwrap_or_else(|e| panic!("round {round} of {ROUNDS} never came back: {e}"));
        assert_eq!(got, sent, "round {round} came back as another byte");
        completed += 1;
        DONE.store(completed, Ordering::Relaxed);
    }
    let elapsed = started.elapsed();

    drop(to_child);
    let status = child.wait().expect("wait for the echo half");
    assert!(status.success(), "the echo half exited with {status}");

    assert_eq!(
        completed, ROUNDS,
        "only {completed} of {ROUNDS} round trips completed",
    );
    assert!(
        elapsed < BOUND,
        "{ROUNDS} round trips took {elapsed:?}, past the {BOUND:?} bound — a wake is being \
         waited out rather than delivered",
    );
    println!("blocking_read_stress: {completed} round trips in {elapsed:?}");
}
