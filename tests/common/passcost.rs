//! The harness half of the scheduler's pass-cost instrument: read the check
//! build's published distribution and judge it against **what this accelerator
//! has been recorded producing**, not against an absolute line.
//!
//! **Why the judgement is here and not in the kernel.** A pass is measured with
//! the only clock either world has across one — wall clock, `rdtsc` in the
//! kernel — and a guest's wall clock advances while the host has taken its vCPU
//! away. The elapsed time of a pass is therefore a *composed* quantity: the
//! scheduler's own work plus an interval the host's scheduler sets, which this
//! CPU neither observes nor controls and which no constant bounds. A panic may
//! assert only what its own site observes and what no workload scales, so the
//! kernel records the distribution ([`toyos_sched::cpu::PassCostReport`]) and
//! this file decides what it means.
//!
//! **What that costs, stated rather than implied.** A single pass that really
//! ran long and a single pass whose CPU was descheduled are the same sample,
//! and nothing — here or in the kernel — can separate them. So the maximum is
//! printed and never gated, and a rare long pass is reported rather than
//! caught. That is the honest limit of this instrument and it has not changed.
//!
//! # Why `MAX_PASS_NS` is not the line any more
//!
//! This gate used to read the budget directly: nine passes in ten provably
//! under 200 000 ns. Its argument was that a host which deschedules a vCPU
//! touches the handful of passes it lands in, so it moves the maximum and not
//! the 90th percentile — *mass is the scheduler, extremes are the machine*.
//! That argument was an observed rate rather than a bound, and it was put to
//! the experiment on 2026-08-18.
//!
//! **It is false.** Six repetitions per arm, quiet and loaded, strictly
//! interleaved in one session so both arms share the ambient host; twelve
//! CPU-runs per arm at `smp: 2`. The load was fourteen pure-shell spin loops,
//! one per logical CPU (`sysctl -n hw.logicalcpu` answers 14 on the dev host),
//! measured standing alone at 90.0–94.9 % CPU each and taking the 1-minute load
//! average from 9.45 to 28.02. The arms separate on the harness's own boot-width
//! instrument with no overlap: 1.74x–2.34x quiet against 2.66x–2.78x loaded.
//!
//! | | quiet, 12 CPU-runs | loaded, 12 CPU-runs |
//! |---|---|---|
//! | p50 | 8 192 ×1, 16 384 ×1, 65 536 ×10 | 8 192 ×1, 32 768 ×1, 131 072 ×10 |
//! | p90 | 65 536 ×2, 131 072 ×10 | 131 072 ×3, 262 144 ×9 |
//! | max | 1 508 330 – 1 983 355 ns | 1 723 825 – 3 914 718 ns |
//! | over budget | 95 of 2 732 (3.48 %) | 187 of 2 619 (7.14 %) |
//! | p90 over budget | **0 of 12** | **9 of 12** |
//!
//! **The whole distribution translates up by one power-of-two bucket under host
//! load, and 200 000 ns sits between the quiet p90 and the loaded one** — which
//! is the entirety of why the verdict flips. Every order statistic moved, the
//! median as much as the tail, so no fraction chosen instead of nine-in-ten
//! would have survived either. There is no quantile of this quantity that host
//! load leaves alone.
//!
//! # What replaces it: the recorded sample of the accelerator in hand
//!
//! The same session recovered the other half. This file's line has printed on
//! every run whatever the verdict, so CI's own logs already held a KVM sample —
//! sixteen `ci` runs, 32 CPU-runs, 7 612 passes, **not one of them over the
//! budget**, p90 at 32 768 ns and the largest single pass in the whole set
//! 173 906 ns. The two accelerators are not one instrument: p90 differs by four
//! between KVM and a *quiet* dev host and by eight against a loaded one, and the
//! maxima by twenty. An absolute line cannot be right for both, and 200 000 ns
//! is right for neither — six times looser than anything KVM produces, and
//! inside the range host load alone sweeps on TCG.
//!
//! So a run is judged against its own accelerator's recorded sample
//! ([`KVM`], [`TCG`]) — the shape `tests/audio-baseline.toml` uses, made
//! environment-relative, which needs no separation of steal from work at all.
//! **And where a recorded sample cannot support a verdict it says so and does
//! not take one**: TCG's own sample spans four buckets on one unchanged tree,
//! so no line drawn from it would be a statement about the scheduler. That is
//! `tests/CLAUDE.md`'s standing rule for this host — what only CI's KVM shards
//! can decide is decided there — applied to a cost rather than to a vendor.
//!
//! **What was not done, so nobody re-derives it.** Reading KVM's paravirtual
//! steal-time MSR would let the guest gate a quantity it observes rather than
//! one it infers, and it is closed by owner ruling: a hypervisor-specific
//! facility cannot be the basis of a gate in a tree whose north star is
//! self-hosting on metal. Steal accounting may be a diagnostic and never a gate.

use toyos_sched::cpu::{PassCostReport, MAX_PASS_NS};

/// Samples a report must carry before its quantiles mean anything: a 90th
/// percentile over this many has ten above it.
///
/// **The margin is 21 %, and it is stated rather than assumed.** Eighteen
/// CPU-runs on the dev host, 2026-08-17, ranged from 121 to 346 passes with a
/// median around 148; 121 is the closest any of them came to this floor. The
/// two recorded samples below bear it out from the other side — their smallest
/// counts are 135 (KVM) and 143 (TCG). If a run ever falls below it the gate
/// reds *saying so*: a report that cannot answer must not answer, and a
/// percentile computed from forty samples is a sample.
///
/// **The floor holds in both judgement modes**, because it is not about the
/// quantile — it is about the instrument having run at all, and a report of
/// forty passes is a broken instrument on any accelerator.
pub const MIN_SAMPLES: u64 = 100;

/// What a recorded sample supports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Judgement {
    /// Gate one bucket above the sample's own worst 90th percentile.
    Ceiling,
    /// Print the distribution and take no verdict on its magnitude. The string
    /// is why: a mode that judges nothing must say what stopped it.
    Report(&'static str),
}

/// One accelerator's recorded pass-cost sample, and the verdict it supports.
///
/// The sample is the observations themselves rather than a summary of them, for
/// `tests/toyos.rs`'s reason at `BaselineSample`: a summary cannot be re-read by
/// the next person to ask whether the line drawn from it was fair.
#[derive(Clone, Copy)]
pub struct Baseline {
    /// The accelerator, as the transcript names it.
    pub accelerator: &'static str,
    /// What was sampled, when, and with what — the sentence that has to survive
    /// beside the numbers, or they are a claim rather than a measurement.
    pub recorded: &'static str,
    /// Every per-CPU-run 90th percentile in the recorded sample, in ns. These
    /// are bucket ends, so every one is a power of two, and [`self_check`]
    /// refuses a sample where one is not: a number that is not a bucket end was
    /// not read off a run.
    pub p90_ns: &'static [u64],
    pub judgement: Judgement,
}

impl Baseline {
    /// The worst 90th percentile anywhere in the recorded sample.
    pub fn worst_p90_ns(&self) -> u64 {
        self.p90_ns.iter().copied().max().expect("a recorded sample with no observations in it")
    }

    /// The line a fresh run is held to, or `None` where the sample supports
    /// none.
    ///
    /// **One bucket above the sample's worst, and that margin is the smallest
    /// this instrument can express.** A quantile is answered as a bucket end,
    /// so the resolution is one power of two; a ceiling *at* the sample's own
    /// worst has no margin at all against the next draw from the same
    /// distribution, and that worst is itself a thin observation — 3 of 32 on
    /// KVM. One bucket is therefore the least that is not a coin flip.
    ///
    /// **What it costs, said here rather than discovered later.** A regression
    /// that moves the 90th percentile by less than four times the sample's mode
    /// passes. The instrument has no finer unit to do better with.
    pub fn ceiling_ns(&self) -> Option<u64> {
        match self.judgement {
            Judgement::Ceiling => Some(self.worst_p90_ns() * 2),
            Judgement::Report(_) => None,
        }
    }
}

/// KVM on native x86-64 — CI's `guest` shards, and the accelerator every claim
/// about what a scheduler pass costs is really about.
///
/// Harvested with `gh run view --job <id> --log` over every `ci` workflow run
/// between the instrument landing (#113, 2026-08-17) and 2026-08-18: sixteen
/// runs, 32 CPU-runs, 135–1 533 passes each and 7 612 pooled. **Not one pass in
/// any of them reached `MAX_PASS_NS`**, and the largest single pass in the whole
/// set was 173 906 ns — so this sample is also, independently, the strongest
/// statement anything has made about the budget on the machine it was derived
/// for.
pub const KVM: Baseline = Baseline {
    accelerator: "KVM, native x86-64",
    recorded: "16 CI runs / 32 CPU-runs, 2026-08-17..18, runs 32043101865 32043658251 \
               32044008591 32044311347 32044665350 32044756253 32044758468 32044760536 \
               32044762195 32044763748 32045857575 32047352064 32050586046 32096188866 \
               32116842348 32117779110; 7612 passes pooled, 0 over MAX_PASS_NS, max 173906 ns",
    // Two per run, cpu0 then cpu1, in the run order above.
    p90_ns: &[
        32_768, 32_768, // 32043101865
        32_768, 32_768, // 32043658251
        32_768, 32_768, // 32044008591
        32_768, 32_768, // 32044311347
        32_768, 32_768, // 32044665350
        32_768, 32_768, // 32044756253
        65_536, 65_536, // 32044758468
        32_768, 32_768, // 32044760536
        32_768, 32_768, // 32044762195
        32_768, 32_768, // 32044763748
        32_768, 32_768, // 32045857575
        32_768, 32_768, // 32047352064
        32_768, 32_768, // 32050586046
        65_536, 32_768, // 32096188866
        32_768, 32_768, // 32116842348
        32_768, 32_768, // 32117779110
    ],
    judgement: Judgement::Ceiling,
};

/// Cross-arch TCG on the arm64 dev host, emulating x86-64 instruction by
/// instruction while the guest TSC advances with host wall clock.
///
/// The quiet arm then the loaded arm of the 2026-08-18 experiment — **one
/// unchanged tree, one session, twelve CPU-runs each.** The spread is the whole
/// content of this entry: 65 536 to 262 144 ns, a factor of four, with nothing
/// but the host's other processes separating the halves. A ceiling at the quiet
/// arm's worst reds every loaded run; one at the loaded arm's worst accepts a
/// fourfold regression. Neither is a statement about the scheduler, so this
/// accelerator takes no verdict on magnitude at all.
///
/// The sample is kept rather than dropped because it *is* that argument, and
/// because the next person to propose a dev-host pass-cost gate should have to
/// read it first. **It also understates the range**, which is the confirming
/// run rather than a caveat: the same test under the same load at 4.09x boot
/// width — harder than either arm — reported `p50 < 131072 ns, p90 < 524288 ns,
/// max 22666022 ns` on a tree whose `sched_stress` passed and whose boot was
/// clean. That is 2.6 times the budget at the 90th percentile and a hundred
/// times it at the maximum, from nothing but fourteen shell loops.
pub const TCG: Baseline = Baseline {
    accelerator: "cross-arch TCG, arm64 dev host",
    recorded: "12 quiet + 12 loaded CPU-runs, 2026-08-18, one interleaved session; the load is \
               14 pure-shell spin loops, one per logical CPU, and the arms separate on boot \
               width 1.74x-2.34x against 2.66x-2.78x with no overlap",
    p90_ns: &[
        // quiet, six runs, cpu0 then cpu1
        65_536, 131_072, 131_072, 131_072, 65_536, 131_072, 131_072, 131_072, 131_072, 131_072,
        131_072, 131_072, // loaded, six runs, cpu0 then cpu1
        262_144, 262_144, 262_144, 262_144, 262_144, 262_144, 131_072, 262_144, 131_072, 262_144,
        131_072, 262_144,
    ],
    judgement: Judgement::Report(
        "the recorded sample spans four buckets on one unchanged tree, moved by nothing but \
         what else the host was running, so no line drawn from it separates a scheduler that \
         grew from a laptop that was busy. What this accelerator still gates is everything \
         else `sched_check_build` asks: a clean boot, the three check-build asserts not \
         firing, and `sched_stress` running to completion",
    ),
};

/// The recorded sample for the accelerator this run is actually using.
pub fn baseline() -> &'static Baseline {
    if toyos_build::kvm_usable() {
        &KVM
    } else {
        &TCG
    }
}

/// The last report each CPU published in `capture`, ordered by CPU.
///
/// The counters are cumulative since boot, so the last line is the whole run
/// and the earlier ones are prefixes of it.
pub fn reports(capture: &str) -> Vec<PassCostReport> {
    let mut last: Vec<PassCostReport> = Vec::new();
    for report in capture.lines().filter_map(PassCostReport::parse) {
        match last.iter().position(|r| r.cpu == report.cpu) {
            Some(at) => last[at] = report,
            None => last.push(report),
        }
    }
    last.sort_by_key(|r| r.cpu.0);
    last
}

/// One line per CPU, for the test's own transcript. Printed whatever the
/// verdict: a green run that stops publishing what it measured is how a gate
/// goes quiet. `over` is still counted against `MAX_PASS_NS` — the budget stays
/// the kernel's policy number and stays reported; it has only stopped deciding.
pub fn describe(report: &PassCostReport) -> String {
    format!(
        "cpu{}: {} passes, p50 < {} ns, p90 < {} ns, p99 < {} ns, max {} ns, \
         {} over the {} ns budget",
        report.cpu.0,
        report.count,
        report.quantile_upper_ns(1, 2),
        report.quantile_upper_ns(9, 10),
        report.quantile_upper_ns(99, 100),
        report.max_ns,
        report.over,
        MAX_PASS_NS,
    )
}

/// The one line that says which sample judged this run and how, printed before
/// the per-CPU lines and whatever the verdict.
///
/// A verdict taken against a recorded sample is unreadable without naming the
/// sample, and a run that judged nothing has to say *that* loudest of all.
pub fn judgement_line(baseline: &Baseline) -> String {
    match baseline.judgement {
        Judgement::Ceiling => format!(
            "judged against the {} sample: 9 passes in 10 under {} ns, one bucket above that \
             sample's own worst of {} ns. Sample: {}",
            baseline.accelerator,
            baseline.ceiling_ns().expect("Judgement::Ceiling has a ceiling"),
            baseline.worst_p90_ns(),
            baseline.recorded,
        ),
        Judgement::Report(why) => format!(
            "NOT judged on magnitude — the {} sample supports no line: {}. Sample: {}",
            baseline.accelerator, why, baseline.recorded,
        ),
    }
}

/// Judge one CPU's distribution against `baseline`.
///
/// Two claims, and every term in both comes from somewhere:
///
/// - **The sample floor holds on every accelerator.** A report of fewer than
///   [`MIN_SAMPLES`] passes is a broken instrument, not a distribution, and
///   nothing below it is worth reading.
/// - **The magnitude claim is the recorded sample's, one bucket up** — see
///   [`Baseline::ceiling_ns`] — and only where the sample supports one.
///
/// The **fraction is still nine in ten and not ninety-nine in a hundred**,
/// because a whole boot plus `sched_stress` is about 150 passes per CPU: a 90th
/// percentile over 150 samples has fifteen above it; a 99th has one and a half,
/// which is its own largest sample wearing a percentile's name. Moving the line
/// from a policy number to a recorded one does not touch that reasoning, and
/// both recorded samples confirm the count from below.
///
/// `quantile_upper_ns` answers with a bucket's upper bound, so "this fraction
/// cost less than X ns" is exact and X ≤ ceiling is a proof. A quantile landing
/// in the bucket that straddles the ceiling reds, which makes the gate strictly
/// conservative rather than approximately right.
///
/// **What it does not catch, said here rather than discovered later.** The
/// maximum is not read at all, for the reason the module header gives. A
/// regression smaller than the instrument's own resolution — one power of two —
/// passes on any accelerator. And on a report-only accelerator nothing about
/// magnitude is caught at all, which is the price of not making a claim the
/// environment cannot support.
pub fn verdict(report: &PassCostReport, baseline: &Baseline) -> Result<(), String> {
    if report.count < MIN_SAMPLES {
        return Err(format!(
            "{} — a 90th percentile needs at least {MIN_SAMPLES} samples behind it and this \
             has {}, so the gate would be reading its own largest sample",
            describe(report),
            report.count,
        ));
    }
    let Some(ceiling) = baseline.ceiling_ns() else {
        return Ok(());
    };
    let p90 = report.quantile_upper_ns(9, 10);
    if p90 > ceiling {
        return Err(format!(
            "{} — this distribution has mass the {} sample never showed: nine passes in ten \
             must be provably under {ceiling} ns and it cannot show that. That line is one \
             bucket above the worst 90th percentile in the recorded sample ({}), and the \
             budget itself ({} ns) is deliberately *not* the line — it is six times looser \
             than anything that sample contains",
            describe(report),
            baseline.accelerator,
            baseline.recorded,
            MAX_PASS_NS,
        ));
    }
    Ok(())
}

/// Prove the instrument in both directions, with no guest.
///
/// `serial::self_check`'s shape and its reason: a gate nothing checks is a gate
/// nobody knows is broken. Two of the cases below are the two this design turns
/// on — a host that took the CPU away must pass, and a scheduler whose passes
/// grew must not — and neither can be staged on a booted machine. **They are the
/// same distribution to a maximum and to an `over` count**, which is the point:
/// the pair is what says this gate reads mass rather than extremes, and any
/// repair that starts gating the maximum again fails one of them.
///
/// The recorded samples are checked against themselves as well, because a
/// ceiling that has drifted from the sample it claims to come from is the one
/// failure this design can suffer silently.
pub fn self_check() -> Result<(), String> {
    check_recorded(&KVM)?;
    check_recorded(&TCG)?;

    // The line KVM's sample draws, spelled out so that re-recording the sample
    // has to move this number deliberately rather than as a side effect.
    if KVM.ceiling_ns() != Some(131_072) {
        return Err(format!(
            "pass-cost gate self-check: the KVM sample's ceiling is {:?} and it was 131072 ns \
             when this gate was written — re-record deliberately or not at all",
            KVM.ceiling_ns(),
        ));
    }
    // And the fact that made TCG report-only: its own sample spans four
    // buckets. A future sample narrow enough to draw a line from would have to
    // come through here to do it.
    let tcg_span = TCG.worst_p90_ns() / TCG.p90_ns.iter().copied().min().unwrap_or(1);
    if tcg_span < 4 {
        return Err(format!(
            "pass-cost gate self-check: TCG takes no verdict because host load alone sweeps \
             its recorded sample, and that sample now spans only {tcg_span}x — the reason and \
             the evidence have come apart"
        ));
    }

    let mut bulk = PassCostReport::empty(toyos_sched::hw::CpuId(0));
    // 100 000 passes at 2048..4096 ns — the shape a scheduler doing scheduling
    // rather than work produces.
    bulk.buckets[12] = 100_000;
    bulk.count = 100_000;

    // The case removing the panic exists for: the same scheduler on a host that
    // took the vCPU away, at 1–2 ms a time — ten times the budget, and about
    // what a whole cross-arch TCG pass cost when invariant P last fired on the
    // dev host.
    let mut stolen = bulk;
    stolen.buckets[12] -= 2_000;
    stolen.buckets[21] = 2_000;
    stolen.max_ns = 2_000_000;
    stolen.over = 2_000;

    // The case it must still catch, and it is `stolen` with the mass moved:
    // one pass in five is now over. Same maximum, same shape of outlier, ten
    // times the mass — and only the quantile can tell them apart.
    let mut grown = bulk;
    grown.buckets[12] -= 20_000;
    grown.buckets[21] = 20_000;
    grown.max_ns = 2_000_000;
    grown.over = 20_000;

    // A limit, asserted on purpose: every pass at 32..64 µs is the worst 90th
    // percentile KVM's whole recorded sample contains, and it is accepted
    // because the ceiling sits one bucket above it.
    let mut sample_worst = PassCostReport::empty(toyos_sched::hw::CpuId(0));
    sample_worst.buckets[16] = 100_000;
    sample_worst.count = 100_000;
    sample_worst.max_ns = 65_000;

    // Two buckets above that, and still four times *under* `MAX_PASS_NS`.
    // **This is the case the budget-shaped gate passed and this one refuses**,
    // and it is the whole gain from making the line the sample's rather than
    // the policy number's.
    let mut over_recorded = PassCostReport::empty(toyos_sched::hw::CpuId(0));
    over_recorded.buckets[18] = 100_000; // every pass in 131 072..262 144 ns
    over_recorded.count = 100_000;
    over_recorded.max_ns = 260_000;

    let mut short = bulk;
    short.buckets[12] = 99;
    short.count = 99;

    let cases: &[(&str, bool, PassCostReport)] = &[
        ("a scheduler doing scheduling", true, bulk),
        ("the same one on a host that stole its vCPU", true, stolen),
        ("the same maximum with one pass in five over the budget", false, grown),
        ("every pass at the worst 90th percentile KVM has shown", true, sample_worst),
        ("every pass two buckets over that, and still under the budget", false, over_recorded),
        ("too few samples to have a 90th percentile", false, short),
    ];
    for (name, want_ok, report) in cases {
        let got = verdict(report, &KVM);
        if got.is_ok() != *want_ok {
            return Err(format!(
                "pass-cost gate self-check: `{name}` should have been {} against the KVM \
                 sample, and it was {}: {got:?}",
                if *want_ok { "accepted" } else { "refused" },
                if got.is_ok() { "accepted" } else { "refused" },
            ));
        }
    }

    // Report-only judges no magnitude and still refuses a broken instrument.
    // Both halves, because a mode that refused nothing would hide a kernel that
    // had stopped measuring.
    if verdict(&grown, &TCG).is_err() {
        return Err(
            "pass-cost gate self-check: a report-only accelerator refused a distribution on \
             its magnitude, which is the one thing it must not do"
                .to_string(),
        );
    }
    if verdict(&short, &TCG).is_ok() {
        return Err(
            "pass-cost gate self-check: a report-only accelerator accepted a report of 99 \
             passes — the sample floor is about the instrument having run, not about the \
             quantile, and it holds in both modes"
                .to_string(),
        );
    }

    // The reader, on the wire form the kernel actually prints: the last line
    // per CPU wins, and a CPU that never reported is absent rather than zero.
    let mut other = bulk;
    other.cpu = toyos_sched::hw::CpuId(1);
    let capture = format!(
        "[kernel 1.001 cpu0] {}\n\
         [kernel 1.002 cpu1] {}\n\
         [kernel 1.003 cpu0] {}\n\
         hello from userland\n",
        short, other, bulk,
    );
    let read = reports(&capture);
    if read.len() != 2 || read[0] != bulk || read[1] != other {
        return Err(format!(
            "pass-cost gate self-check: the reader took {} report(s) off a capture with two \
             CPUs and three lines: {read:?}",
            read.len(),
        ));
    }
    if !reports("hello from userland\n").is_empty() {
        return Err("pass-cost gate self-check: a capture with no report yielded one".to_string());
    }
    Ok(())
}

/// A recorded sample must be in the instrument's own units, and must pass the
/// line it is used to draw. Either failure makes every verdict below it
/// meaningless while looking exactly like a working gate.
fn check_recorded(baseline: &Baseline) -> Result<(), String> {
    if baseline.p90_ns.is_empty() {
        return Err(format!(
            "pass-cost gate self-check: the {} sample has no observations in it",
            baseline.accelerator,
        ));
    }
    for &p90 in baseline.p90_ns {
        if !p90.is_power_of_two() {
            return Err(format!(
                "pass-cost gate self-check: the {} sample records a 90th percentile of \
                 {p90} ns, and a quantile off this instrument is a bucket end — so that \
                 number was not read off a run",
                baseline.accelerator,
            ));
        }
    }
    if let Some(ceiling) = baseline.ceiling_ns() {
        if baseline.worst_p90_ns() > ceiling {
            return Err(format!(
                "pass-cost gate self-check: the {} sample's own worst 90th percentile ({} ns) \
                 is over the ceiling drawn from it ({ceiling} ns), so the recorded runs would \
                 red the gate they justify",
                baseline.accelerator,
                baseline.worst_p90_ns(),
            ));
        }
    }
    Ok(())
}
