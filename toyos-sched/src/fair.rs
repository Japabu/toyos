//! Fairness policy: per-process vruntime/lag/frontier math (spec §9.1). The
//! simulator runs this exact arithmetic, so policy changes belong here and are
//! sim-gated.

use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::LeafLock;

/// Clamp for stored lag at the Runnable→NonRunnable transition: how far
/// behind (entitled catch-up) or ahead (throttled on wake) of the frontier a
/// process may be remembered as. 50ms.
pub const MAX_VRUNTIME_LAG_NS: u64 = 50_000_000;

/// Preemption quantum. 10ms.
pub const QUANTUM_NS: u64 = 10_000_000;

/// The requested operation needs the `Runnable` arm. A marker, not a
/// recoverable error: callers panic with their own context (the kernel adds
/// the pid) — charging or yield-reading a NonRunnable share is a
/// bookkeeping bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotRunnable;

/// The global vruntime frontier: a monotonic-non-decreasing baseline from
/// which non-runnable processes' lag is measured (see
/// [`ShareState::NonRunnable`]). A per-CPU frontier with epoch reconciliation
/// is a later, sim-gated stage (spec §11 Stage 9).
pub struct Frontier(AtomicU64);

impl Frontier {
    /// No `Default` beside it: every frontier in this tree is a `static`, which
    /// only a `const fn` can build, and `Default::default` cannot be one.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Advance the frontier to at least `vrt`, at dispatch. `fetch_max` is
    /// the only correct semantic on SMP: a plain `store(vrt)` lets a CPU
    /// picking a low-vrt task regress the frontier another CPU has already
    /// advanced, and lets RT picks (vrt=0) reset it to zero on every
    /// preemption.
    pub fn advance(&self, vrt: u64) {
        self.0.fetch_max(vrt, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Per-process fair-share state.
///
/// A non-runnable process stores no vruntime — a stored one would go stale as
/// the frontier moves. It stores `lag = frontier - vruntime` instead, and
/// re-derives `vruntime = frontier - lag` on wake, so it tracks the frontier
/// with no writes while blocked.
pub enum ShareState {
    Runnable {
        vruntime: u64,
        /// Threads of this process in a run queue (ready or current).
        /// `NonZeroU32` forbids "Runnable with 0 threads", which would mean a
        /// botched leave→NonRunnable transition.
        runnable_threads: NonZeroU32,
        /// Clamped lag frozen at the most recent NonRunnable→Runnable
        /// transition (0 for a process that has never blocked since spawn).
        ///
        /// This is the *contract* value [`ShareState::lag`] reports: bounded
        /// ±[`MAX_VRUNTIME_LAG_NS`] by construction. Live `frontier -
        /// vruntime` may exceed that bound between a wake and the pick, which
        /// is a diagnostic and not a contract.
        lag_at_wake: i64,
    },
    NonRunnable {
        /// `lag = frontier - vruntime` at the last transition, clamped to
        /// ±[`MAX_VRUNTIME_LAG_NS`]. Positive = behind the frontier (entitled
        /// to catch up); negative = ran ahead (throttled on wake).
        lag: i64,
    },
}

impl ShareState {
    /// First runnable thread of a never-scheduled process: starts at the
    /// current frontier with `lag_at_wake = 0`, so its insert vruntime is
    /// `frontier` itself.
    pub fn new_runnable(frontier: u64) -> Self {
        Self::Runnable {
            vruntime: frontier,
            runnable_threads: NonZeroU32::MIN,
            lag_at_wake: 0,
        }
    }

    /// A thread of this process is becoming runnable; returns the vruntime to
    /// insert with. All threads of one process share a vruntime, so a
    /// subsequent thread only bumps the refcount and `lag_at_wake` stays
    /// frozen at the most recent NonRunnable→Runnable edge.
    pub fn enter_runnable(&mut self, frontier: u64) -> u64 {
        match self {
            Self::Runnable {
                vruntime,
                runnable_threads,
                ..
            } => {
                *runnable_threads = runnable_threads
                    .checked_add(1)
                    .expect("runnable_threads overflow");
                *vruntime
            }
            Self::NonRunnable { lag } => {
                let lag = *lag;
                let vrt = vrt_from_lag(frontier, lag);
                *self = Self::Runnable {
                    vruntime: vrt,
                    runnable_threads: NonZeroU32::MIN,
                    lag_at_wake: lag,
                };
                vrt
            }
        }
    }

    /// A thread of this process is no longer runnable (blocked, exited,
    /// killed). Decrements the refcount; on the last runnable thread,
    /// stores clamped lag and transitions to NonRunnable.
    ///
    /// No-op if already NonRunnable — covers removal of a thread that is
    /// already blocked (its refcount was decremented when it blocked).
    pub fn leave_runnable(&mut self, frontier: u64) {
        if let Self::Runnable {
            vruntime,
            runnable_threads,
            ..
        } = self
        {
            if let Some(n) = NonZeroU32::new(runnable_threads.get() - 1) {
                *runnable_threads = n;
            } else {
                let lag = live_lag(frontier, *vruntime)
                    .clamp(-(MAX_VRUNTIME_LAG_NS as i64), MAX_VRUNTIME_LAG_NS as i64);
                *self = Self::NonRunnable { lag };
            }
        }
    }

    /// Charge `ns` of consumed CPU time to the share's vruntime.
    /// `Err(NotRunnable)`: the caller charged a process with no runnable
    /// threads — a bookkeeping bug it must report loudly.
    pub fn charge(&mut self, ns: u64) -> Result<(), NotRunnable> {
        match self {
            Self::Runnable { vruntime, .. } => {
                *vruntime = vruntime.saturating_add(ns);
                Ok(())
            }
            Self::NonRunnable { .. } => Err(NotRunnable),
        }
    }

    /// The stored Runnable vruntime, for the yield re-insert path where the
    /// thread stays runnable. `Err(NotRunnable)` means a yielding thread was
    /// wrongly counted out — the caller panics with its context.
    pub fn runnable_vruntime(&self) -> Result<u64, NotRunnable> {
        match self {
            Self::Runnable { vruntime, .. } => Ok(*vruntime),
            Self::NonRunnable { .. } => Err(NotRunnable),
        }
    }

    /// Live vruntime for external readers: the stored value while Runnable,
    /// `frontier - lag` while NonRunnable.
    pub fn vruntime(&self, frontier: u64) -> u64 {
        match self {
            Self::Runnable { vruntime, .. } => *vruntime,
            Self::NonRunnable { lag } => vrt_from_lag(frontier, *lag),
        }
    }

    /// Contract lag: the clamped lag established at the most recent
    /// transition in either direction. Bounded ±MAX_VRUNTIME_LAG_NS by
    /// construction — NOT the live `frontier - vruntime` drift, which can
    /// exceed the bound during the wake-to-pick gap on multi-CPU systems.
    pub fn lag(&self) -> i64 {
        match self {
            Self::Runnable { lag_at_wake, .. } => *lag_at_wake,
            Self::NonRunnable { lag } => *lag,
        }
    }
}

/// `lag = frontier - vruntime`, unclamped; the caller clamps before storing.
/// Wrapping subtraction is deliberate: vruntimes are dense in u64, and the
/// wrapped difference is the signed distance even once the frontier has
/// wrapped past `vrt`.
fn live_lag(frontier: u64, vrt: u64) -> i64 {
    (frontier as i64).wrapping_sub(vrt as i64)
}

/// `vrt = frontier - lag`, saturating at u64 bounds.
fn vrt_from_lag(frontier: u64, lag: i64) -> u64 {
    if lag >= 0 {
        frontier.saturating_sub(lag as u64)
    } else {
        frontier.saturating_add((-lag) as u64)
    }
}

/// One process's fair-share pot, reached through any thread that owns it
/// (spec §9.1). The cell is supplied by the environment for the reason stated
/// on [`LeafLock`]: the kernel's is a word-sized spin, the simulator's a mutex.
pub struct FairShare<L> {
    state: L,
}

impl<L: LeafLock<ShareState>> FairShare<L> {
    pub fn new(state: L) -> Self {
        Self { state }
    }

    /// A thread of this process is entering a run queue: returns the vruntime
    /// to insert with. Called at exactly the §5.2 transitions that add a task
    /// to the Ready+Running pair, which is what keeps `runnable_threads` and
    /// the containers in step (invariant I6).
    pub fn enter_runnable(&self, frontier: &Frontier) -> u64 {
        self.state.with(|s| s.enter_runnable(frontier.get()))
    }

    /// A thread left the Ready+Running pair (park, die, migrate away).
    pub fn leave_runnable(&self, frontier: &Frontier) {
        self.state.with(|s| s.leave_runnable(frontier.get()));
    }

    /// The re-insert vruntime for a thread that stays runnable (preempt and
    /// yield), which must not re-enter and double-count the refcount.
    pub fn runnable_vruntime(&self) -> Result<u64, NotRunnable> {
        self.state.with(|s| s.runnable_vruntime())
    }

    pub fn charge(&self, ns: u64) -> Result<(), NotRunnable> {
        self.state.with(|s| s.charge(ns))
    }

    pub fn vruntime(&self, frontier: &Frontier) -> u64 {
        self.state.with(|s| s.vruntime(frontier.get()))
    }

    pub fn lag(&self) -> i64 {
        self.state.with(|s| s.lag())
    }

    /// The refcount invariant I6 compares against the containers.
    pub fn runnable_threads(&self) -> u32 {
        self.state.with(|s| match s {
            ShareState::Runnable {
                runnable_threads, ..
            } => runnable_threads.get(),
            ShareState::NonRunnable { .. } => 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAMP: i64 = MAX_VRUNTIME_LAG_NS as i64;

    fn runnable_count(s: &ShareState) -> u32 {
        match s {
            ShareState::Runnable {
                runnable_threads, ..
            } => runnable_threads.get(),
            ShareState::NonRunnable { .. } => 0,
        }
    }

    #[test]
    fn new_process_starts_at_frontier_with_zero_lag() {
        let s = ShareState::new_runnable(1_000);
        assert_eq!(s.vruntime(1_000), 1_000);
        assert_eq!(s.lag(), 0);
        assert_eq!(runnable_count(&s), 1);
    }

    #[test]
    fn refcount_tracks_runnable_threads() {
        let mut s = ShareState::new_runnable(0);
        assert_eq!(s.enter_runnable(0), 0);
        assert_eq!(runnable_count(&s), 2);
        s.leave_runnable(0);
        assert_eq!(runnable_count(&s), 1);
        assert!(s.runnable_vruntime().is_ok());
        s.leave_runnable(0);
        assert_eq!(runnable_count(&s), 0);
        assert_eq!(s.runnable_vruntime(), Err(NotRunnable));
    }

    #[test]
    fn charge_accumulates_and_saturates() {
        let mut s = ShareState::new_runnable(10);
        s.charge(5).unwrap();
        assert_eq!(s.runnable_vruntime(), Ok(15));
        s.charge(u64::MAX).unwrap();
        assert_eq!(s.runnable_vruntime(), Ok(u64::MAX));
    }

    #[test]
    fn charge_nonrunnable_is_a_bug() {
        let mut s = ShareState::new_runnable(0);
        s.leave_runnable(0);
        assert_eq!(s.charge(1), Err(NotRunnable));
    }

    #[test]
    fn leave_stores_lag_and_wake_rederives_vruntime() {
        // Ran 10ms ahead of the frontier, then blocked: lag = -10ms.
        let mut s = ShareState::new_runnable(0);
        s.charge(10_000_000).unwrap();
        s.leave_runnable(0);
        assert_eq!(s.lag(), -10_000_000);
        // Frontier advanced to 50ms while blocked: wake resumes 10ms ahead
        // of the *current* frontier and freezes the lag as the contract.
        let vrt = s.enter_runnable(50_000_000);
        assert_eq!(vrt, 60_000_000);
        assert_eq!(s.lag(), -10_000_000);
    }

    #[test]
    fn lag_clamps_in_both_directions() {
        // Far behind the frontier: entitled catch-up clamps to +50ms.
        let mut s = ShareState::new_runnable(0);
        s.leave_runnable(10 * MAX_VRUNTIME_LAG_NS);
        assert_eq!(s.lag(), CLAMP);
        // Far ahead of the frontier: throttle clamps to -50ms.
        let mut s = ShareState::new_runnable(0);
        s.charge(10 * MAX_VRUNTIME_LAG_NS).unwrap();
        s.leave_runnable(0);
        assert_eq!(s.lag(), -CLAMP);
    }

    #[test]
    fn nonrunnable_vruntime_tracks_the_live_frontier() {
        let mut s = ShareState::new_runnable(1_000);
        s.leave_runnable(2_000); // lag = +1000
        assert_eq!(s.vruntime(2_000), 1_000);
        assert_eq!(s.vruntime(10_000), 9_000);
        // Saturates instead of wrapping below zero.
        assert_eq!(s.vruntime(500), 0);
    }

    #[test]
    fn leave_when_nonrunnable_is_a_noop() {
        let mut s = ShareState::new_runnable(0);
        s.leave_runnable(1_000);
        let lag = s.lag();
        s.leave_runnable(9_999_999);
        assert_eq!(s.lag(), lag);
    }

    #[test]
    fn frontier_is_monotonic() {
        let f = Frontier::new();
        f.advance(10);
        f.advance(5);
        assert_eq!(f.get(), 10);
        f.advance(11);
        assert_eq!(f.get(), 11);
    }
}
