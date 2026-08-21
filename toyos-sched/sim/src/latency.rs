//! How long a task that was owed the CPU waited for it — the instrument the
//! measured policy suite (`sim/tests/policy.rs`) states its bounds in.
//!
//! The invariant walks answer *whether* a rule held. A policy claim needs the
//! number as well: "interactive wakes are prompt" and "nothing starves" are
//! worth exactly what the distribution behind them is, and a bound asserted
//! without one is a bound nobody can watch move. So every interval a task spends
//! owed a dispatch and not getting one is folded in here, split by how it came
//! to be owed one — see [`ReadyCause`], whose three arms carry three different
//! bounds.
//!
//! **Four exact numbers rather than a histogram, and the fourth is the reason.**
//! A power-of-two histogram — [`toyos_sched::cpu::PassCosts`]' scheme, the one
//! thing here that could have been borrowed — answers a quantile to within a
//! factor of two, and every bound this suite states is within a factor of two of
//! the quantity it bounds: one quantum against one quantum plus 2.2 ms of
//! granularity. So the quantile would decide nothing. What the workloads *do*
//! have is a known one-off — the spawn burst, whose stale fair-band keys delay
//! exactly one wake per run — and the exact statement about that is
//! [`Latency::runner_up_ns`]: the second-largest wait in the run, which is a
//! bound on **every wake but one** and needs no interpolation at all.
//!
//! **What the resolution is, stated rather than assumed.** The measurement is
//! taken at step boundaries, so an interval that opens and closes inside one VM
//! step reads as a *zero* rather than as no sample:
//! [`crate::vm::Vm::awaiting`] stamps the moment a wake claims a parked task,
//! which is at the earliest one step before the pass that could dispatch it.

/// One population of waits. Every field is exact — there is no bucketing here to
/// round anything off.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Latency {
    count: u64,
    sum_ns: u64,
    max_ns: u64,
    runner_up_ns: u64,
}

impl Latency {
    pub fn note(&mut self, ns: u64) {
        self.count += 1;
        self.sum_ns += ns;
        if ns > self.max_ns {
            self.runner_up_ns = self.max_ns;
            self.max_ns = ns;
        } else if ns > self.runner_up_ns {
            self.runner_up_ns = ns;
        }
    }

    /// Fold another population in, exactly: the result is what one population
    /// holding both sets of samples would hold.
    ///
    /// Note what that means for [`Self::runner_up_ns`] across a *sweep*: the
    /// union's second-largest sample is one number about all the seeds together,
    /// not the worst any single seed produced. A gate that means "every wake but
    /// one **per run**" folds the per-run figure itself, and `tests/policy.rs`
    /// does.
    pub fn merge(&mut self, other: &Self) {
        self.count += other.count;
        self.sum_ns += other.sum_ns;
        // The losing maximum is an ordinary sample of the union, so it competes
        // with both runners-up for second place.
        let (max, runner_up) = if other.max_ns > self.max_ns {
            (other.max_ns, other.runner_up_ns.max(self.max_ns))
        } else {
            (self.max_ns, self.runner_up_ns.max(other.max_ns))
        };
        self.max_ns = max;
        self.runner_up_ns = runner_up;
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn max_ns(&self) -> u64 {
        self.max_ns
    }

    /// The second-largest wait: a bound on every sample in this population but
    /// one. Zero for a population of fewer than two.
    pub fn runner_up_ns(&self) -> u64 {
        if self.count < 2 {
            return 0;
        }
        self.runner_up_ns
    }

    /// Zero samples answer 0, so a caller that gates on this checks
    /// [`Self::count`] first — every gate in `tests/policy.rs` does, because a
    /// measurement of nothing sailing under a bound is the instrument-defect
    /// shape the whole harness exists to refuse.
    pub fn mean_ns(&self) -> u64 {
        if self.count == 0 {
            return 0;
        }
        self.sum_ns / self.count
    }

    /// The one line a failing assertion prints. A maximum on its own cannot say
    /// whether a bound was brushed once or lived against.
    pub fn summary(&self) -> String {
        format!(
            "n={} max={} 2nd={} mean={}",
            self.count,
            self.max_ns,
            self.runner_up_ns(),
            self.mean_ns(),
        )
    }
}

/// Why a task was owed a dispatch — three arms because they are three different
/// claims with three different bounds.
///
/// * [`Self::Woken`] is the interactive one: a wake claimed a parked task, and
///   `mailbox::Urgency::Normal` promises a busy target drains it "at its next
///   safe point (≤ one quantum)". That sentence is the bound.
/// * [`Self::Preempted`] is the starvation one: a task that lost the CPU at its
///   quantum and waits for its next turn behind whatever else the fair band
///   holds. Its bound carries the rival count, exactly as invariant I13's does.
/// * [`Self::Fresh`] is a task that has never run: an `Adopt` landing in a run
///   queue, which is spawn placement rather than either of the above.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadyCause {
    Woken,
    Preempted,
    Fresh,
}

impl ReadyCause {
    pub const ALL: [ReadyCause; 3] = [Self::Woken, Self::Preempted, Self::Fresh];

    pub fn index(self) -> usize {
        match self {
            Self::Woken => 0,
            Self::Preempted => 1,
            Self::Fresh => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Woken => "woken",
            Self::Preempted => "preempted",
            Self::Fresh => "fresh",
        }
    }
}

/// One process's run-queue waits, kept apart by how each was incurred.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct RunWait {
    by_cause: [Latency; 3],
}

impl RunWait {
    pub fn note(&mut self, cause: ReadyCause, ns: u64) {
        self.by_cause[cause.index()].note(ns);
    }

    pub fn get(&self, cause: ReadyCause) -> &Latency {
        &self.by_cause[cause.index()]
    }

    pub fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.by_cause.iter_mut().zip(other.by_cause.iter()) {
            mine.merge(theirs);
        }
    }

    /// The longest wait of any kind. The starvation number, for a caller that
    /// does not care how the task came to be owed the CPU.
    pub fn worst_ns(&self) -> u64 {
        self.by_cause.iter().map(Latency::max_ns).max().unwrap_or(0)
    }

    pub fn samples(&self) -> u64 {
        self.by_cause.iter().map(Latency::count).sum()
    }

    pub fn summary(&self) -> String {
        ReadyCause::ALL
            .iter()
            .map(|&cause| format!("{}[{}]", cause.name(), self.get(cause).summary()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_up_is_the_second_largest_sample() {
        let mut l = Latency::default();
        for ns in [5, 100, 7, 99, 1] {
            l.note(ns);
        }
        assert_eq!((l.count(), l.max_ns(), l.runner_up_ns()), (5, 100, 99));
        assert_eq!(l.mean_ns(), (5 + 100 + 7 + 99 + 1) / 5);
    }

    /// A population of one has no second sample, and answering its own maximum
    /// would turn "every wait but one" into "the worst wait" — the gate would
    /// then pass on a single catastrophic wake.
    #[test]
    fn one_sample_has_no_runner_up() {
        let mut l = Latency::default();
        l.note(9_999);
        assert_eq!((l.count(), l.max_ns(), l.runner_up_ns()), (1, 9_999, 0));
    }

    #[test]
    fn merging_is_exactly_the_union() {
        let samples = [[3u64, 40, 7], [50, 2, 9]];
        let mut merged = Latency::default();
        for set in samples {
            let mut one = Latency::default();
            for ns in set {
                one.note(ns);
            }
            merged.merge(&one);
        }
        let mut flat = Latency::default();
        for ns in samples.into_iter().flatten() {
            flat.note(ns);
        }
        assert_eq!(merged, flat);
    }

    /// Merging a population whose maximum loses: the loser is an ordinary
    /// sample of the union and must be able to become its runner-up.
    #[test]
    fn a_losing_maximum_becomes_the_runner_up() {
        let mut a = Latency::default();
        a.note(100);
        a.note(1);
        let mut b = Latency::default();
        b.note(90);
        b.note(2);
        a.merge(&b);
        assert_eq!((a.max_ns(), a.runner_up_ns()), (100, 90));
    }

    /// A distribution nobody fed answers zero everywhere, which is why every
    /// gate checks the count before it reads the bound.
    #[test]
    fn an_empty_distribution_measures_nothing() {
        let empty = Latency::default();
        assert_eq!((empty.count(), empty.max_ns(), empty.mean_ns()), (0, 0, 0));
        assert_eq!(empty.runner_up_ns(), 0);
    }

    #[test]
    fn causes_are_kept_apart() {
        let mut wait = RunWait::default();
        wait.note(ReadyCause::Woken, 5);
        wait.note(ReadyCause::Preempted, 50);
        assert_eq!(wait.get(ReadyCause::Woken).max_ns(), 5);
        assert_eq!(wait.get(ReadyCause::Preempted).max_ns(), 50);
        assert_eq!(wait.get(ReadyCause::Fresh).count(), 0);
        assert_eq!((wait.worst_ns(), wait.samples()), (50, 2));
    }
}
