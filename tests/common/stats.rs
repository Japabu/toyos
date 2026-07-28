//! Two-sample tests for the audio gate's thorough tier.
//!
//! Every thorough-tier decision compares the **fresh sample** against the
//! **recorded baseline sample** — never against a fitted constant. That matters
//! more than it sounds: a threshold derived from 30 clean runs carries the
//! sampling error of those 30 runs, and a one-sample test against it states a
//! confidence it does not have. Measured on this tree, a sign test against the
//! recorded median of `max_wake_lat_us` has a nominal false-red rate of 0.07%
//! and a real one near 1.5%, purely because the reference median moves by an
//! order statistic. A two-sample test carries that uncertainty in the maths.
//!
//! Two tests, one per kind of observation:
//!
//!   * `mann_whitney_z` for the counters (continuous, heavy right tails, no
//!     distributional assumption survives them).
//!   * `fisher_greater` for the yes/no outcomes (did this run drop audio, did
//!     it breach a per-run ceiling, did it fail to complete).
//!
//! Both are one-sided in the direction of "worse". A scheduler change that
//! improves audio timing must not fail the gate.

/// Per-test significance level. One in a thousand, so the ~19 tests a thorough
/// run performs still union-bound under 2%, and the measured family-wise
/// false-red rate on a clean tree is 0.25% (2000 simulated runs against the
/// recorded distributions). Deliberately not a tunable: a gate whose alpha can
/// be raised is a gate that will be raised.
pub const ALPHA: f64 = 0.001;

/// One-sided standard-normal critical value for `ALPHA`.
pub const Z_CRIT: f64 = 3.0902;

/// Mann-Whitney U, one-sided, as a normal-approximation z score: how strongly
/// `test` is stochastically *greater* than `base`. Ties get midranks and the
/// tie-corrected variance, which matters here — `underruns` and `drains` are
/// small integers with many repeats.
///
/// The normal approximation is used rather than the exact permutation
/// distribution because at n1 = n2 = 30 it is already accurate and, measured by
/// bootstrap against the recorded samples, conservative: 3 rejections in 240000
/// trials under H0 against a nominal 0.1%.
pub fn mann_whitney_z(base: &[f64], test: &[f64]) -> f64 {
    let n1 = base.len() as f64;
    let n2 = test.len() as f64;
    assert!(n1 >= 2.0 && n2 >= 2.0, "Mann-Whitney needs both samples");

    let mut all: Vec<f64> = base.iter().chain(test.iter()).copied().collect();
    all.sort_by(|a, b| a.partial_cmp(b).expect("audio counters are never NaN"));

    // Midrank of each distinct value, and the tie-group sizes.
    let mut rank_of: Vec<(f64, f64)> = Vec::new();
    let mut tie_term = 0.0;
    let mut i = 0;
    while i < all.len() {
        let mut j = i;
        while j + 1 < all.len() && all[j + 1] == all[i] {
            j += 1;
        }
        let group = (j - i + 1) as f64;
        rank_of.push((all[i], (i + j) as f64 / 2.0 + 1.0));
        tie_term += group * group * group - group;
        i = j + 1;
    }
    let rank = |v: f64| {
        rank_of
            .binary_search_by(|(k, _)| k.partial_cmp(&v).unwrap())
            .map(|idx| rank_of[idx].1)
            .expect("value came from the pooled sample")
    };

    let r2: f64 = test.iter().map(|&v| rank(v)).sum();
    let u2 = r2 - n2 * (n2 + 1.0) / 2.0;
    let n = n1 + n2;
    let var = n1 * n2 / 12.0 * ((n + 1.0) - tie_term / (n * (n - 1.0)));
    if var <= 0.0 {
        // Every observation identical: no evidence of anything.
        return 0.0;
    }
    (u2 - n1 * n2 / 2.0) / var.sqrt()
}

/// Fisher's exact test, one-sided: the probability of seeing at least `k1`
/// events in `n1` fresh runs when the fresh runs share the baseline's rate,
/// given `k0` of `n0` in the baseline. Exact, so it stays honest at the tiny
/// counts these rates produce (4 dropped runs in 117).
pub fn fisher_greater(k1: u32, n1: u32, k0: u32, n0: u32) -> f64 {
    let (k1, n1, k0, n0) = (k1 as usize, n1 as usize, k0 as usize, n0 as usize);
    assert!(k1 <= n1 && k0 <= n0);
    let total = n1 + n0;
    let events = k1 + k0;
    let ln_fact = ln_factorials(total);
    let ln_choose = |n: usize, k: usize| ln_fact[n] - ln_fact[k] - ln_fact[n - k];
    let denom = ln_choose(total, events);
    let hi = n1.min(events);
    let mut p = 0.0;
    for x in k1..=hi {
        if events - x > n0 {
            continue;
        }
        p += (ln_choose(n1, x) + ln_choose(n0, events - x) - denom).exp();
    }
    p.min(1.0)
}

/// Smallest event count in `n1` fresh runs that Fisher rejects at `ALPHA`.
/// `None` when even `n1` of `n1` would not reject — which is itself worth
/// printing, because it means the sample is too small to test that rate at all.
pub fn fisher_reject_at(n1: u32, k0: u32, n0: u32) -> Option<u32> {
    (0..=n1).find(|&k| fisher_greater(k, n1, k0, n0) <= ALPHA)
}

fn ln_factorials(n: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(n + 1);
    out.push(0.0);
    let mut acc = 0.0;
    for i in 1..=n {
        acc += (i as f64).ln();
        out.push(acc);
    }
    out
}
