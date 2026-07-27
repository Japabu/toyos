//! `ChoiceStream` — the single source of nondeterminism in the simulator.
//! Every scheduling-relevant decision (which enabled step, which delay
//! within bounds) is drawn from this stream, so a run is fully determined by
//! its driver: identical seed or identical bytes ⇒ identical run, always
//! (spec §10.3).
//!
//! Drivers: `Seeded` for CI seed sweeps and `Bytes` for `cargo fuzz`, where
//! the input bytes ARE the decisions — libFuzzer mutation becomes free
//! interleaving search. The PCT driver (random vcpu priorities + d change
//! points) lands with the explorer at migration Stage 4.

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

pub enum ChoiceStream {
    Seeded(SmallRng),
    Bytes { data: Vec<u8>, pos: usize },
}

impl ChoiceStream {
    pub fn from_seed(seed: u64) -> Self {
        Self::Seeded(SmallRng::seed_from_u64(seed))
    }

    /// The bytes are the decision sequence. An exhausted stream keeps
    /// answering 0, so a truncated fuzz input is still a complete,
    /// deterministic run.
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self::Bytes { data, pos: 0 }
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
        assert!(n <= u16::MAX as usize + 1, "choose: step space exceeds two bytes");
        match self {
            Self::Seeded(rng) => rng.random_range(0..n),
            Self::Bytes { data, pos } => {
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
        }
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
    #[should_panic(expected = "no enabled steps")]
    fn zero_options_is_a_bug() {
        ChoiceStream::from_seed(0).choose(0);
    }
}
