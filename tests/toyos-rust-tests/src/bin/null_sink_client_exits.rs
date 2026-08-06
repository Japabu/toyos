//! A client that plays through the null sink must finish and exit.
//!
//! `/bin/tone` and not this crate's own tone: the T14 hangs on the shipped
//! binary, which reaches soundd through `cpal`, while the raw-API tone that
//! `metal_sim_null_audio` runs drains perfectly on the same sink. Whatever the
//! defect is, only the path a user actually takes shows it — which is why this
//! spawns the program the shell spawns rather than linking the SDK directly.
//!
//! Two of them in series, because one hung client is what the T14 log shows
//! blocking the *next* connect: if the first exits and the second hangs, the
//! sink stops draining after a run rather than never starting.

use std::process::Command;
use std::time::{Duration, Instant};

/// One second of tone. Long enough that a sink discarding instantly is visible
/// as an implausibly fast exit, short enough that two fit in a test.
const SECONDS: &str = "1";

/// A tone that has not finished in this long is not slow, it is stuck: the
/// sink drains one period per 2.902 ms and the whole run is 1 s of audio.
const PATIENCE: Duration = Duration::from_secs(15);

fn play(round: u32) {
    let start = Instant::now();
    let mut child = Command::new("/bin/tone")
        .arg("440")
        .arg(SECONDS)
        .spawn()
        .unwrap_or_else(|e| panic!("round {round}: /bin/tone did not spawn: {e}"));

    // `try_wait` in a loop rather than `wait`, so the failure is this test's
    // own message and a wall-clock number rather than the harness's timeout —
    // a hang and a slow guest look identical in a timeout and different here.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let took = start.elapsed();
                assert!(
                    status.success(),
                    "round {round}: /bin/tone exited {status:?} after {took:?}"
                );
                println!("round {round}: tone exited after {:.2?}", took);
                return;
            }
            Ok(None) => {}
            Err(e) => panic!("round {round}: try_wait failed: {e}"),
        }
        assert!(
            start.elapsed() < PATIENCE,
            "round {round}: /bin/tone has not exited after {:?} — the null sink is not \
             draining its client",
            start.elapsed()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn main() {
    play(1);
    play(2);
    println!("null sink drained two clients in series");
}
