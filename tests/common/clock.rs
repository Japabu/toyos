//! Telling a suspended host from a slow one.
//!
//! The dev machine is a laptop and the owner closes the lid. A run that spans
//! that is not a slow run, it is an **invalid measurement**: QEMU's virtual
//! clock, the guest's own millisecond stamps and every device timing in it jump
//! by however long the machine was away, and every wall-clock verdict in the
//! serial tail and in gate A is taken against one of those. CLAUDE.md already
//! documents the signature — a tight cluster of durations plus a few enormous
//! outliers — and documents it as something an agent must check *before*
//! recording a finding, which is to say the harness has never been able to.
//!
//! It can, and with no new source of truth: the two clocks the harness already
//! reads disagree by exactly the suspended time.
//!
//! - `Instant` is `CLOCK_UPTIME_RAW` on this platform and `CLOCK_MONOTONIC` on
//!   Linux. Neither advances while the machine is asleep — the first by its
//!   documented definition, which `library/std/src/sys/time/unix.rs` quotes in
//!   full beside the constant.
//! - `SystemTime` is the wall clock and does.
//!
//! So `wall − monotonic` over an interval **is** the time the host spent
//! stopped, and every deadline the suite takes is on the monotonic one — which
//! is why a suspended run does not report a timeout. It reports whatever the
//! guest made of two hours in the middle, and calls it a defect.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// A reading of both clocks at one moment.
#[derive(Clone, Copy)]
pub struct Mark {
    wall: SystemTime,
    mono: Instant,
}

/// Below this, the pair is drifting rather than jumping.
///
/// A suspend is minutes; NTP slews rather than steps in ordinary operation, and
/// a step large enough to reach this is itself a reason to distrust a timing
/// verdict. Two seconds is generous against both and small against the thing it
/// detects.
pub const SUSPENDED_AT_LEAST: Duration = Duration::from_secs(2);

/// Seconds of host suspend to stage, for [`self_check`].
///
/// **Nothing else can reach this state.** The two clocks diverge only when the
/// machine actually stops, and a process cannot suspend the host it runs on —
/// the lid is what produces it and there is no API that asks for it. So the
/// actuator is the clock source, and it moves the wall clock alone, which is
/// precisely and only what a suspend does to this pair: a staged reading is
/// indistinguishable from a real one for every consumer downstream, verdict and
/// message alike.
static STAGED: AtomicU64 = AtomicU64::new(0);

pub fn stage_suspend(how_long: Duration) {
    STAGED.store(how_long.as_millis() as u64, Ordering::SeqCst);
}

pub fn mark() -> Mark {
    let staged = Duration::from_millis(STAGED.load(Ordering::SeqCst));
    Mark { wall: SystemTime::now() + staged, mono: Instant::now() }
}

impl Mark {
    /// Monotonic time since this mark: the duration a test actually took, with
    /// any suspend in the middle already excluded because the clock stopped too.
    pub fn elapsed(&self) -> Duration {
        self.mono.elapsed()
    }

    /// How long the host was stopped between this mark and now.
    ///
    /// Saturating in both directions on purpose: a wall clock that went
    /// *backwards* is not a suspend and has nothing to say here.
    ///
    /// Reads both clocks, so it is never exactly zero — the two are two
    /// syscalls and the gap between them lands in the answer. That is what
    /// [`SUSPENDED_AT_LEAST`] is a threshold against, and asking this twice
    /// gives two different sub-microsecond answers.
    pub fn suspended(&self) -> Duration {
        let now = mark();
        let wall = now.wall.duration_since(self.wall).unwrap_or(Duration::ZERO);
        wall.saturating_sub(now.mono.duration_since(self.mono))
    }
}

/// Host-side gate, registered in `MACHINE_TESTS` and booting nothing.
///
/// It is a check of the detector alone. That the harness *reports* what the
/// detector says is `suspend_invalidates_a_verdict` in `tests/toyos.rs`, beside
/// the classification it exercises.
pub fn self_check() -> Result<(), String> {
    let quiet = mark();
    std::thread::sleep(Duration::from_millis(20));
    let idle = quiet.suspended();
    if idle > Duration::from_millis(1) {
        return Err(format!("a host that did not stop reports {idle:?} of suspend"));
    }
    if idle >= SUSPENDED_AT_LEAST {
        return Err("an ordinary 20 ms of waiting reads as a suspend".to_string());
    }

    let across = mark();
    stage_suspend(Duration::from_secs(90));
    // Read once and reset: every reading takes the clocks afresh, so a second
    // one after the reset would be of a host that never stopped.
    let seen = across.suspended();
    stage_suspend(Duration::ZERO);

    if !(Duration::from_secs(89)..=Duration::from_secs(91)).contains(&seen) {
        return Err(format!("staged 90 s of suspend and the detector saw {seen:?}"));
    }
    if seen < SUSPENDED_AT_LEAST {
        return Err("90 s of host suspend did not invalidate the interval".to_string());
    }

    // And the reading is of the *interval*, not of the process: a mark taken
    // after the host came back must be clean, or every later test in the run
    // would inherit one lid closing.
    let after = mark();
    if after.suspended() >= SUSPENDED_AT_LEAST {
        return Err("a suspend before an interval invalidated it anyway".to_string());
    }
    Ok(())
}
