//! `ChoiceStream` — the single source of nondeterminism in the simulator.
//! Every scheduling-relevant decision (which enabled step, whether to inject
//! an interfering wake) is drawn from this stream, so a run is fully
//! determined by its driver: identical seed or identical bytes ⇒ identical
//! run, always (spec §10.3).
//!
//! Four interchangeable drivers:
//!
//! * `Seeded` — CI seed sweeps.
//! * `Bytes` — the `cargo fuzz` shape, where the input bytes ARE the
//!   decisions, so libFuzzer's mutation engine becomes free interleaving
//!   search.
//! * `Pct` — random vcpu priorities plus `d` change points. Uniform random
//!   choice spends most of its budget on schedules that differ trivially;
//!   PCT's priority discipline gives a probabilistic bound on finding a bug
//!   of depth `d`, which is why it reaches orderings seeds do not.
//! * `Replay` — a recorded decision list, for corpus regressions and for the
//!   shrinker's candidate evaluation.
//!
//! Every driver records what it chose, so any run — however it was driven —
//! can be replayed exactly.

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

/// The step count PCT spreads its change points over — the order of
/// magnitude a scenario run takes.
const CHANGE_POINT_HORIZON: usize = 192;

enum Driver {
    Seeded(SmallRng),
    Bytes {
        data: Vec<u8>,
        pos: usize,
    },
    Pct {
        rng: SmallRng,
        /// Priority per vcpu; the highest-priority enabled actor moves.
        priorities: Vec<u32>,
        /// Step counts at which the running vcpu is demoted below every
        /// other — the "change points" that make deep orderings reachable.
        change_points: Vec<usize>,
        steps: usize,
        next_low: u32,
    },
    Replay {
        decisions: Vec<usize>,
        pos: usize,
    },
}

pub struct ChoiceStream {
    driver: Driver,
    record: Vec<usize>,
}

impl ChoiceStream {
    pub fn from_seed(seed: u64) -> Self {
        Self::new(Driver::Seeded(SmallRng::seed_from_u64(seed)))
    }

    /// The bytes are the decision sequence. An exhausted stream keeps
    /// answering 0, so a truncated fuzz input is still a complete,
    /// deterministic run.
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self::new(Driver::Bytes { data, pos: 0 })
    }

    /// PCT with `cpus` vcpus and `depth` change points.
    pub fn pct(seed: u64, cpus: usize, depth: usize) -> Self {
        let mut rng = SmallRng::seed_from_u64(seed);
        let priorities = (0..cpus.max(1))
            .map(|_| rng.random_range(0..1024u32) + 1024)
            .collect();
        // Drawn from the range these scenarios actually run in (tens to a few
        // hundred steps). Change points beyond the end of a run are change
        // points that never happen, which would quietly reduce PCT to a fixed
        // priority order.
        let mut change_points: Vec<usize> = (0..depth)
            .map(|_| rng.random_range(1..CHANGE_POINT_HORIZON))
            .collect();
        change_points.sort_unstable();
        Self::new(Driver::Pct {
            rng,
            priorities,
            change_points,
            steps: 0,
            next_low: 1023,
        })
    }

    /// Replay a recorded decision list. Past its end it answers 0, so a
    /// shrunk prefix still runs to quiescence deterministically.
    pub fn replay(decisions: Vec<usize>) -> Self {
        Self::new(Driver::Replay { decisions, pos: 0 })
    }

    fn new(driver: Driver) -> Self {
        Self {
            driver,
            record: Vec::new(),
        }
    }

    pub fn recorded(&self) -> &[usize] {
        &self.record
    }

    /// Draw a decision in `0..n`. `n` is the number of currently enabled
    /// steps — asking for a choice among zero options is an explorer bug.
    ///
    /// Byte encoding: one byte when `n <= 256`, two little-endian bytes
    /// otherwise (128 vcpus × ~5 step kinds exceeds one byte's range).
    /// The width depends only on `n`, which is itself a deterministic
    /// function of the decisions so far — replays stay exact.
    pub fn choose(&mut self, n: usize) -> usize {
        assert!(n > 0, "choose: no enabled steps");
        assert!(
            n <= u16::MAX as usize + 1,
            "choose: step space exceeds two bytes"
        );
        let chosen = match &mut self.driver {
            Driver::Seeded(rng) => rng.random_range(0..n),
            Driver::Bytes { data, pos } => {
                let mut next = || {
                    let b = data.get(*pos).copied().unwrap_or(0);
                    *pos += 1;
                    b as usize
                };
                let raw = if n <= u8::MAX as usize + 1 {
                    next()
                } else {
                    next() | (next() << 8)
                };
                raw % n
            }
            Driver::Pct { rng, .. } => rng.random_range(0..n),
            Driver::Replay { decisions, pos } => {
                let chosen = decisions.get(*pos).copied().unwrap_or(0);
                *pos += 1;
                chosen % n
            }
        };
        self.record.push(chosen);
        chosen
    }

    /// Choose among enabled steps, given which vcpu each belongs to. Only the
    /// PCT driver looks at the actors; for every other driver this is exactly
    /// [`Self::choose`], which is what keeps replays driver-independent.
    pub fn choose_step(&mut self, actors: &[Option<usize>]) -> usize {
        assert!(!actors.is_empty(), "choose_step: no enabled steps");
        let Driver::Pct {
            priorities,
            change_points,
            steps,
            next_low,
            ..
        } = &mut self.driver
        else {
            return self.choose(actors.len());
        };

        *steps += 1;
        // A change point demotes whoever is currently top: the schedule
        // switches actor at a pseudo-random depth, which is what gives PCT
        // its bound.
        while change_points.first().is_some_and(|at| *at <= *steps) {
            change_points.remove(0);
            if let Some(top) = priorities
                .iter()
                .enumerate()
                .max_by_key(|(_, p)| **p)
                .map(|(i, _)| i)
            {
                priorities[top] = *next_low;
                *next_low = next_low.saturating_sub(1);
            }
        }
        let best = actors
            .iter()
            .enumerate()
            .max_by_key(|(index, actor)| {
                // Steps that belong to no vcpu (device IRQs, clock jumps)
                // rank below every vcpu, so they fill in when nothing else
                // can move — the same order a real machine imposes.
                let priority = actor.map(|a| priorities[a]).unwrap_or(0);
                (priority, usize::MAX - index)
            })
            .map(|(index, _)| index)
            .expect("non-empty");
        self.record.push(best);
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_is_deterministic() {
        let mut a = ChoiceStream::from_seed(0xC0FFEE);
        let mut b = ChoiceStream::from_seed(0xC0FFEE);
        let seq_a: Vec<usize> = (0..1000).map(|_| a.choose(7)).collect();
        let seq_b: Vec<usize> = (0..1000).map(|_| b.choose(7)).collect();
        assert_eq!(seq_a, seq_b);
        assert!(seq_a.iter().all(|&c| c < 7));
        assert_eq!(a.recorded(), &seq_a[..]);
    }

    #[test]
    fn seeds_differ() {
        let mut a = ChoiceStream::from_seed(1);
        let mut b = ChoiceStream::from_seed(2);
        let seq_a: Vec<usize> = (0..100).map(|_| a.choose(1000)).collect();
        let seq_b: Vec<usize> = (0..100).map(|_| b.choose(1000)).collect();
        assert_ne!(seq_a, seq_b);
    }

    #[test]
    fn bytes_are_the_decisions() {
        let mut s = ChoiceStream::from_bytes(vec![0, 1, 5, 255]);
        assert_eq!(s.choose(4), 0);
        assert_eq!(s.choose(4), 1);
        assert_eq!(s.choose(4), 1); // 5 % 4
        assert_eq!(s.choose(4), 3); // 255 % 4
                                    // Exhausted: keeps answering 0 deterministically.
        assert_eq!(s.choose(4), 0);
        assert_eq!(s.choose(9), 0);
    }

    #[test]
    fn bytes_wide_draw_uses_two_bytes() {
        // n > 256 consumes two LE bytes: 0x0201 = 513, then one byte for a
        // narrow draw to prove the cursor advanced by exactly two.
        let mut s = ChoiceStream::from_bytes(vec![0x01, 0x02, 3]);
        assert_eq!(s.choose(1000), 513);
        assert_eq!(s.choose(4), 3);
    }

    #[test]
    fn bytes_full_range_reachable() {
        for target in 0..=255usize {
            let mut s = ChoiceStream::from_bytes(vec![target as u8]);
            assert_eq!(s.choose(256), target);
        }
    }

    #[test]
    fn replay_reproduces_a_recording() {
        let mut original = ChoiceStream::from_seed(99);
        let seq: Vec<usize> = (0..200).map(|_| original.choose(5)).collect();
        let mut replay = ChoiceStream::replay(original.recorded().to_vec());
        let again: Vec<usize> = (0..200).map(|_| replay.choose(5)).collect();
        assert_eq!(seq, again);
    }

    #[test]
    fn pct_prefers_the_highest_priority_actor_and_is_stable() {
        let mut a = ChoiceStream::pct(7, 4, 3);
        let mut b = ChoiceStream::pct(7, 4, 3);
        let actors = [Some(0), Some(1), Some(2), None];
        let seq_a: Vec<usize> = (0..256).map(|_| a.choose_step(&actors)).collect();
        let seq_b: Vec<usize> = (0..256).map(|_| b.choose_step(&actors)).collect();
        assert_eq!(seq_a, seq_b, "PCT is seeded, so it replays");
        assert!(
            seq_a.iter().any(|&c| c != seq_a[0]),
            "change points must move the choice off one actor",
        );
    }

    #[test]
    fn pct_never_starves_a_lone_step() {
        let mut s = ChoiceStream::pct(3, 2, 2);
        // Only a device IRQ is enabled: it must be chosen even though no vcpu
        // owns it.
        assert_eq!(s.choose_step(&[None]), 0);
    }

    #[test]
    #[should_panic(expected = "no enabled steps")]
    fn zero_options_is_a_bug() {
        ChoiceStream::from_seed(0).choose(0);
    }
}
