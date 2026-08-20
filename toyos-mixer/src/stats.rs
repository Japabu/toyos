//! What the audio gate reads.
//!
//! The counters are the instrument `tests/audio-baseline.toml` thresholds are
//! written against, so each has to mean exactly one thing and keep meaning it.
//! The decision in here is [`MixStats::period`]: which periods count as
//! starvation and which are the design working. Emitting the report is soundd's
//! — one line, one `write` — and everything it needs is public below.

/// Counters for one reporting window. A window covers streaming only: zeroed
/// when the first client arrives, flushed when the last one leaves, so no
/// number here is diluted by the idle path — where soundd waits on raw
/// completion IRQs with no timer and a batched IRQ is indistinguishable from a
/// missed deadline. The audio gate reads these (`tests/audio-baseline.toml`),
/// so each has to mean exactly one thing.
#[derive(Default)]
pub struct MixStats {
    pub wakes: u32,
    pub completions: u32,
    /// Every period put on the wire in this window, underruns included.
    pub submitted: u32,
    /// Periods submitted with no client audio behind them *while at least one
    /// client was streaming* (`ClientStream::is_streaming`) — silence that
    /// interrupted a stream rather than preceding or following one. Strictly
    /// narrower than `submitted`, which like `wakes`/`completions`/`drains`
    /// covers the whole time soundd has clients.
    pub underruns: u32,
    /// The longest unbroken run of them, which is the silence a listener
    /// actually hears — 54 scattered singles and one gap of 54 are the same
    /// `underruns` and are not the same defect. It is also the only thing that
    /// separates a client that never had margin from one that lost it: the ring
    /// is eight periods deep, so a run past one is a producer that stopped for
    /// a measurable time rather than one that missed a deadline by a hair.
    pub starve_max: u32,
    /// The run [`starve_max`](Self::starve_max) is the maximum of. Working
    /// state, not a field of the report; a run crossing a window boundary is
    /// counted in both, which understates it and never invents one.
    pub starve_run: u32,
    /// Cycles that found the whole DMA pipeline free (§5.9) *and* could only
    /// have got there by soundd being late. A device that retires the pipeline
    /// faster than it plays it empties the free list without soundd having
    /// missed anything; see the count site.
    pub drains: u32,
    /// Worst overshoot of a DLL prediction soundd actually armed a timer on
    /// (§5.1). Waits that named no wake time contribute nothing; see the
    /// sample site.
    pub max_wake_lat_ns: u64,
    pub max_batch: u32,
    /// Free buffers left unfilled because a streaming client was still
    /// producing the period that belongs in them (§5.10) — an activity signal,
    /// not a fault, and so uncapped.
    pub deferred: u32,
}

impl MixStats {
    /// Account one period, whichever sink played it.
    pub fn period(&mut self, streaming: bool, covered: bool) {
        if !streaming {
            return;
        }
        if covered {
            self.starve_run = 0;
            return;
        }
        self.underruns += 1;
        self.starve_run += 1;
        self.starve_max = self.starve_max.max(self.starve_run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A period nobody was streaming through is not an underrun.** Silence
    /// before the first client has spawned its callback thread, and silence
    /// after a close while the ramp fades, are both the design working — and
    /// counting them would put every ordinary connect into the gate's threshold.
    #[test]
    fn silence_outside_a_stream_costs_nothing() {
        let mut stats = MixStats::default();
        for _ in 0..100 {
            stats.period(false, false);
        }
        assert_eq!(stats.underruns, 0);
        assert_eq!(stats.starve_max, 0);
    }

    /// The run length is what a listener hears: 54 scattered singles and one
    /// gap of 54 are the same `underruns` and are not the same defect.
    #[test]
    fn the_run_length_separates_a_gap_from_scattered_misses() {
        let mut scattered = MixStats::default();
        for _ in 0..54 {
            scattered.period(true, false);
            scattered.period(true, true);
        }
        assert_eq!(scattered.underruns, 54);
        assert_eq!(scattered.starve_max, 1);

        let mut one_gap = MixStats::default();
        for _ in 0..54 {
            one_gap.period(true, false);
        }
        one_gap.period(true, true);
        assert_eq!(one_gap.underruns, 54);
        assert_eq!(one_gap.starve_max, 54);
    }

    /// The maximum is a maximum: a later, shorter run does not lower it.
    #[test]
    fn a_later_shorter_run_does_not_lower_the_worst() {
        let mut stats = MixStats::default();
        for _ in 0..8 {
            stats.period(true, false);
        }
        stats.period(true, true);
        for _ in 0..3 {
            stats.period(true, false);
        }
        assert_eq!(stats.starve_max, 8);
        assert_eq!(stats.starve_run, 3);
        assert_eq!(stats.underruns, 11);
    }

    /// A covered period ends a run, which is what makes the run a measure of
    /// unbroken silence rather than of the whole window.
    #[test]
    fn one_covered_period_ends_a_run() {
        let mut stats = MixStats::default();
        stats.period(true, false);
        stats.period(true, false);
        stats.period(true, true);
        assert_eq!(stats.starve_run, 0);
        stats.period(true, false);
        assert_eq!(stats.starve_run, 1);
        assert_eq!(stats.starve_max, 2);
    }
}
