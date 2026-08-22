//! What the audio gate reads.
//!
//! The counters are the instrument `tests/audio-baseline.toml` thresholds are
//! written against, so each has to mean exactly one thing and keep meaning it.
//! Emitting the report is soundd's — one line, one `write` — and everything it
//! needs is public below.
//!
//! Two decisions live here, and both are about what one number may stand for.
//! [`MixStats::period`]: which periods count as starvation and which are the
//! design working. [`WorstWake`]: that a late wake is attributed to the half of
//! the path it happened in, because "soundd was late" and "the device was late"
//! are different defects with different owners and `max_wake_lat_ns` on its own
//! says neither.

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
    /// [`max_wake_lat_ns`](Self::max_wake_lat_ns) taken apart — see
    /// [`WorstWake`]. Set by the same call that sets the maximum, so the four
    /// numbers describe one wake and not four different ones.
    pub worst: WorstWake,
    /// Wakes since the last completion that found none. Working state, not a
    /// field of the report; published as [`WorstWake::empty`].
    empty_run: u32,
    /// How many wakes in this window were a whole device period or more past
    /// the grid point they armed on.
    ///
    /// **The maximum alone cannot say whether a window held one stall or a
    /// thousand**, and those are different defects: one is an event that
    /// happened to the machine, a thousand is a pipeline that never keeps its
    /// grid. A period is the threshold because it is the step the grid is made
    /// of — a wake later than one has cost the pipeline a whole period of its
    /// margin, and one shorter has cost it none.
    pub late_wakes: u32,
    pub max_batch: u32,
    /// Free buffers left unfilled because a streaming client was still
    /// producing the period that belongs in them (§5.10) — an activity signal,
    /// not a fault, and so uncapped.
    pub deferred: u32,
}

/// The worst wake of a window, taken apart into the two delays it is the sum
/// of, plus what soundd was doing in between.
///
/// **One number was standing for two unrelated failures.** A wake is late
/// either because the *interrupt* arrived after the grid point soundd armed on
/// — the device, or the machine hosting it, produced nothing when it was due —
/// or because soundd did not get a CPU after the interrupt landed. Those are
/// different defects with different owners, they are fixed in different places,
/// and `max_wake_lat_ns` alone cannot tell an investigator which one it saw. So
/// the sum is decomposed at the one instant where both halves are known.
///
/// The identity is exact and is what makes the decomposition readable:
/// `irq_late_ns + pickup_ns == max_wake_lat_ns` for the wake this describes.
#[derive(Default, Clone, Copy)]
pub struct WorstWake {
    /// From the armed grid point to the completion interrupt's own timestamp,
    /// which the kernel stamps in the ISR. This is the device being late, and
    /// nothing about it is soundd's.
    pub irq_late_ns: u64,
    /// From that timestamp to soundd reading the record. This is soundd being
    /// late: a CPU it did not get, a wake that did not reach it.
    pub pickup_ns: u64,
    /// Wakes between the grid point and this one that carried no completion at
    /// all. A large count is soundd waking punctually, repeatedly, at a device
    /// that had produced nothing — the shape a stalled *host* leaves, and the
    /// one a single overlong sleep cannot.
    pub empty: u32,
    /// Periods this wake retired. A full pipeline in one wake after a long
    /// silence is the device catching up, not the device running early.
    pub batch: u32,
}

impl MixStats {
    /// Account one wake that carried completions, `lateness_ns` past the grid
    /// point it armed on.
    ///
    /// `>=` rather than `>`: a window whose worst wake is zero still gets a
    /// decomposition, and the last wake to reach the maximum owns it.
    ///
    /// `period_ns` is the grid's own step, and the only thing it decides is
    /// what counts toward [`late_wakes`](Self::late_wakes).
    pub fn wake(
        &mut self,
        lateness_ns: u64,
        irq_late_ns: u64,
        pickup_ns: u64,
        batch: u32,
        period_ns: u64,
    ) {
        if lateness_ns >= self.max_wake_lat_ns {
            self.max_wake_lat_ns = lateness_ns;
            self.worst = WorstWake { irq_late_ns, pickup_ns, empty: self.empty_run, batch };
        }
        if lateness_ns >= period_ns {
            self.late_wakes += 1;
        }
        self.empty_run = 0;
    }

    /// Account one wake that armed on a grid point and found no completion.
    pub fn empty_wake(&mut self) {
        self.empty_run += 1;
    }

    /// The null sink's grid is soundd's own monotonic one: there is no device
    /// and no interrupt, so the grid point *is* the instant soundd should have
    /// run and every nanosecond past it is soundd's own.
    pub fn wake_on_software_grid(&mut self, lateness_ns: u64, period_ns: u64) {
        self.wake(lateness_ns, 0, lateness_ns, 1, period_ns);
    }

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

    /// The shipped device period in nanoseconds — 256 frames at 44100 Hz, and
    /// the step [`MixStats::late_wakes`] counts against. The wakes below are
    /// microseconds apart precisely so none of them reaches it by accident.
    const PERIOD: u64 = 2_902_494;

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

    /// The decomposition describes **the** worst wake, not the worst of each
    /// half separately. A window holding one late-interrupt wake and one
    /// slow-pickup wake must report whichever was worse *whole*, with its own
    /// two halves — mixing the maxima would invent a wake that never happened.
    #[test]
    fn the_halves_come_from_one_wake() {
        let mut stats = MixStats::default();
        stats.wake(9_000, 8_000, 1_000, 3, PERIOD);
        stats.wake(5_000, 100, 4_900, 1, PERIOD);
        assert_eq!(stats.max_wake_lat_ns, 9_000);
        assert_eq!(stats.worst.irq_late_ns, 8_000);
        assert_eq!(stats.worst.pickup_ns, 1_000);
        assert_eq!(stats.worst.batch, 3);
    }

    /// The identity the decomposition exists for: the two halves sum to the
    /// number the gate has always read.
    #[test]
    fn the_halves_sum_to_the_whole() {
        let mut stats = MixStats::default();
        stats.wake(20_000, 19_500, 500, 8, PERIOD);
        assert_eq!(stats.worst.irq_late_ns + stats.worst.pickup_ns, stats.max_wake_lat_ns);
    }

    /// Empty wakes are counted against the wake that ends the run, and the
    /// count restarts after it — a later, punctual wake must not inherit the
    /// stall an earlier one already reported.
    #[test]
    fn empty_wakes_belong_to_the_wake_that_ends_them() {
        let mut stats = MixStats::default();
        for _ in 0..6 {
            stats.empty_wake();
        }
        stats.wake(20_000, 19_000, 1_000, 8, PERIOD);
        assert_eq!(stats.worst.empty, 6);
        stats.empty_wake();
        stats.wake(30_000, 29_000, 1_000, 8, PERIOD);
        assert_eq!(stats.worst.empty, 1);
    }

    /// **One stall and a thousand are different defects**, and the maximum is
    /// the same number for both. A window of punctual wakes with one overshoot
    /// past a period counts one; a window where every wake is past one counts
    /// them all.
    #[test]
    fn a_late_wake_is_one_past_a_whole_period() {
        let mut once = MixStats::default();
        for _ in 0..500 {
            once.wake(PERIOD - 1, 0, PERIOD - 1, 1, PERIOD);
        }
        once.wake(20 * PERIOD, 19 * PERIOD, PERIOD, 8, PERIOD);
        assert_eq!(once.late_wakes, 1);

        let mut always = MixStats::default();
        for _ in 0..500 {
            always.wake(PERIOD, 0, PERIOD, 1, PERIOD);
        }
        assert_eq!(always.late_wakes, 500);
        assert_eq!(always.max_wake_lat_ns, once.max_wake_lat_ns / 20);
    }

    /// The null sink has no interrupt to be late, so all of its lateness is
    /// its own and the identity still holds.
    #[test]
    fn a_software_grid_blames_only_itself() {
        let mut stats = MixStats::default();
        stats.wake_on_software_grid(7_000, PERIOD);
        assert_eq!(stats.worst.irq_late_ns, 0);
        assert_eq!(stats.worst.pickup_ns, 7_000);
        assert_eq!(stats.max_wake_lat_ns, 7_000);
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
