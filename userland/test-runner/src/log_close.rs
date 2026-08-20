//! A poll on the machine's log outlives a *different* handle to the capability
//! that named it.
//!
//! **The defect this is aimed at was cancellation by source.**
//! `object::ops::close` handed `io_uring::cancel_by_source` whatever sources the
//! closing object named, and `cancel_by_source` walks the source's watcher list across
//! every ring in the machine — which is right for a pipe, whose other end really
//! has gone, and wrong for the log, which outlives every handle that can name
//! it. Every `SysCap` maps to `Source::Log`, so any process closing any
//! capability posted `-NotFound` into every pending log poll there was. Latent
//! while nothing parked on one; live the moment `/bin/logd`'s whole loop is
//! read-then-park.
//!
//! The two processes in the real failure need not know about each other at all,
//! which is why this runs with one: a duplicate of this program's own capability
//! is a second handle to the same object, and closing it is exactly the event
//! `cancel_by_source` acted on. What it proves is that the *handle* is not what the
//! source's lifetime is tied to.
//!
//! It runs inside `test-runner` for `log-gate`'s reason — a `SysCap` dup is not
//! a namespace entry, so a spawned binary has none — and it needs `dup` in its
//! manifest row on top of `logread`, which `tests/testcases/system.toml` has.

use std::process::Command;

use toyos::log::{LogTail, Record};
use toyos::poller::{Poller, READABLE};
use toyos::syscap::SysCap;

/// The poll's token. One handle is watched, so one number.
const TOKEN: u64 = 1;

/// A cursor read never returns more than this at once here — the buffer only
/// has to be at least `MAX_LOG_SHARDS` for the kernel to answer at all, and
/// draining to empty is a loop either way.
const BATCH: usize = 32;

/// How long the positive half waits for a record to complete the poll.
///
/// A liveness bound and not a verdict: what has to happen is one child's
/// `exit:` record committing and `klogd` posting after it drains, which is two
/// scheduler passes. Three seconds is four orders of magnitude above that, and
/// a poll that was cancelled answers instantly rather than late.
const READINESS_WAIT_NANOS: u64 = 3_000_000_000;

/// How many times a spurious completion is allowed to send this round again.
///
/// The immediate check asks "did closing a sibling handle complete the poll",
/// and a record committing in the same handful of microseconds would complete
/// it honestly. That is distinguishable — an honest completion leaves records
/// in the cursor — so it is retried rather than tolerated, and a bound keeps a
/// busy machine from retrying for ever.
const ROUNDS: usize = 5;

pub fn run(cap: Option<&SysCap>) -> i32 {
    let Some(cap) = cap else {
        println!("log-close: this program holds no system capability, so it holds no `logread`");
        return 1;
    };
    match probe(cap) {
        Ok(()) => {
            println!("log-close: OK");
            0
        }
        Err(e) => {
            println!("log-close: FAILED: {e}");
            1
        }
    }
}

fn probe(cap: &SysCap) -> Result<(), String> {
    let mut tail = LogTail::new();
    let mut buf = [Record::EMPTY; BATCH];

    for round in 1..=ROUNDS {
        // Start from a cursor with nothing owing, so a completion in the
        // window below is either the close's or a record's, and the two are
        // told apart by asking the cursor afterwards.
        drain(cap, &mut tail, &mut buf)?;

        let poller = Poller::new(2);
        poller.watch(cap, READABLE, TOKEN);
        // **Submitted before the close, and this is the whole of what the gate
        // has to get right.** `watch` only queues a submission entry;
        // `wait` is what enters the kernel. A round that closed the sibling
        // handle first would stage nothing at all — the ring is not a watcher
        // of the log yet, so there is nothing for a cancellation to reach, and
        // the gate passes on a tree with the defect. `min_complete` of zero
        // submits and returns without waiting.
        poller.wait(0, 0, |_| {});
        if poller.pending() != 0 {
            return Err(format!(
                "{} submission(s) never reached the kernel",
                poller.pending()
            ));
        }
        // **The baseline, and it is the source's own contract.** `Source::Log`
        // is edge-triggered: `is_ready` answers `false` and every completion
        // comes from `klogd`'s post, so a poll submitted on a quiet cursor must
        // be pending and not complete. A completion here is a record that
        // arrived in the meantime, which sends the round again rather than
        // being tolerated.
        if completions(&poller) != 0 {
            continue;
        }

        // A second handle to the same object, closed. This is the whole
        // stimulus: nothing about the machine's log has changed.
        let dup = cap
            .duplicate()
            .map_err(|e| format!("the capability would not duplicate ({e:?}); this gate needs `dup` beside `logread`"))?;
        drop(dup);

        if completions(&poller) == 0 {
            // The poll survived the close. Now show it is still a poll.
            return still_armed(cap, &poller, &mut tail, &mut buf);
        }
        let owed = read(cap, &mut tail, &mut buf)?;
        if owed == 0 {
            return Err(format!(
                "closing a second handle to the same capability completed the log poll with no \
                 record behind it — a handle going away cancelled a source that outlives every \
                 handle (round {round} of {ROUNDS})"
            ));
        }
    }
    Err(format!(
        "{ROUNDS} rounds each had a record commit inside the window; this machine is too busy for \
         the immediate check to say anything"
    ))
}

/// The positive half: a record still completes the poll the close did not take.
fn still_armed(
    cap: &SysCap,
    poller: &Poller,
    tail: &mut LogTail,
    buf: &mut [Record; BATCH],
) -> Result<(), String> {
    // A child that runs and exits, because `process::exit_process` logs
    // `exit: <name> pid=… code=…` — one kernel record, from userland, with no
    // actuator and no privilege.
    let mut child = Command::new("/bin/echo")
        .arg("log-close")
        .spawn()
        .map_err(|e| format!("the record-making child would not start: {e}"))?;
    let _ = child.wait();

    let mut completed = 0usize;
    poller.wait(1, READINESS_WAIT_NANOS, |token| {
        if token == TOKEN {
            completed += 1;
        }
    });
    if completed == 0 {
        return Err(
            "the poll outlived the close and then never completed on a record either, so what it \
             outlived may have been its own arming"
                .to_string(),
        );
    }
    let owed = read(cap, tail, buf)?;
    if owed == 0 {
        return Err("the poll completed with no record to read behind it".to_string());
    }
    println!("log-close: survived=1 records_after={owed}");
    Ok(())
}

/// Completions waiting on `poller` right now, without blocking.
fn completions(poller: &Poller) -> usize {
    let mut seen = 0usize;
    poller.wait(1, 0, |token| {
        if token == TOKEN {
            seen += 1;
        }
    });
    seen
}

fn read(cap: &SysCap, tail: &mut LogTail, buf: &mut [Record; BATCH]) -> Result<usize, String> {
    tail.read(cap, buf)
        .map(|records| records.len())
        .map_err(|e| format!("SYS_LOG_READ refused: {e:?}"))
}

fn drain(cap: &SysCap, tail: &mut LogTail, buf: &mut [Record; BATCH]) -> Result<(), String> {
    while read(cap, tail, buf)? > 0 {}
    Ok(())
}
