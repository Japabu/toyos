//! The harness half of the scheduler's pass-cost instrument: read the check
//! build's published distribution and judge it.
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
//! and nothing — here or in the kernel — can separate them. What separates a
//! defect from a busy host is *mass*: a scheduler pass that grew is a fixed
//! fraction of all passes, while a host that steals a vCPU touches the handful
//! of passes it lands in. So the gate below is a quantile, the maximum is
//! printed and never gated, and a rare long pass is reported rather than
//! caught. That last sentence is the honest limit of this instrument.
//!
//! **And the sample is small, which is what decides which quantile.** The only
//! workload that boots this kernel is `sched_check_build`'s, and a whole boot
//! plus `sched_stress` takes about 150 passes per CPU — a quantum is 10 ms, so
//! nearly every pass is a block or a wake rather than a tick. Measured on the
//! dev host, 2026-08-17, eighteen CPU-runs: 121 to 346, median around 148. A
//! 90th percentile over 150 samples has fifteen above it; a 99th has one and a
//! half, which is its own largest sample wearing a percentile's name.
//!
//! **The pair that says this works is on the dev host and not in a fixture.**
//! Alone on a quiet machine the guest reports `p50 < 16384 ns, p90 < 131072 ns,
//! max 1504209 ns` and passes; in the same suite's 12-wide phase, minutes
//! earlier and on the same tree, `p50 < 131072 ns, p90 < 262144 ns, max
//! 1745977 ns` and it reds. The two maxima are within 16 % of each other and the
//! verdicts differ. That is why `sched_check_build` is `Sched::Serial`, and it
//! is the demonstration this gate's whole design rests on.

use toyos_sched::cpu::{PassCostReport, MAX_PASS_NS};

/// Samples a report must carry before its quantiles mean anything: a 90th
/// percentile over this many has ten above it.
///
/// **The margin is 21 %, and it is stated rather than assumed.** Eighteen
/// CPU-runs on the dev host, 2026-08-17, ranged from 121 to 346 passes with a
/// median around 148; 121 is the closest any of them came to this floor. If a
/// run ever falls below it the gate reds *saying so* — a report that cannot
/// answer must not answer, and a percentile computed from forty samples is a
/// sample.
pub const MIN_SAMPLES: u64 = 100;

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
/// goes quiet.
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

/// Judge one CPU's distribution: **nine passes in ten fit `MAX_PASS_NS`.**
///
/// One claim, and every term in it comes from somewhere:
///
/// - The **magnitude is `MAX_PASS_NS` itself**, unrounded and unrelaxed. It is
///   the number the removed assert stood over and the number the simulator's I4
///   bound omits, and nothing here moves it.
/// - The **fraction is nine in ten and not all ten**, because there is no
///   magnitude a hypervisor cannot produce by taking a vCPU away, so "every
///   pass" is not a statement about the scheduler at all. That is the whole of
///   what removing the panic gave up.
/// - The **fraction is nine in ten and not ninety-nine in a hundred** because
///   of the sample size above, not because the tighter one was tried and found
///   inconvenient: 150 samples do not have a 99th percentile.
///
/// `quantile_upper_ns` answers with a bucket's upper bound, so "this fraction
/// cost less than X ns" is exact and X ≤ `MAX_PASS_NS` is a proof. A quantile
/// landing in the bucket that straddles the budget reds, which makes the gate
/// strictly conservative rather than approximately right.
///
/// **What it does not catch, said here rather than discovered later.** A pass
/// that grew but stayed under the budget — the median at 65 µs instead of 5 —
/// passes this gate. The budget's own doc claims an order of magnitude of
/// margin over such a pass, and that claim is *reported* (the median is in
/// every line [`describe`] prints) and not gated, because the one instrument
/// that boots this kernel puts that median anywhere from under 16 µs to under
/// 131 µs depending on what else the *host* is running: a gate at
/// `MAX_PASS_NS / 10` would be a statement about the host's load.
pub fn verdict(report: &PassCostReport) -> Result<(), String> {
    if report.count < MIN_SAMPLES {
        return Err(format!(
            "{} — a 90th percentile needs at least {MIN_SAMPLES} samples behind it and this \
             has {}, so the gate would be reading its own largest sample",
            describe(report),
            report.count,
        ));
    }
    let p90 = report.quantile_upper_ns(9, 10);
    if p90 > MAX_PASS_NS {
        return Err(format!(
            "{} — this distribution has mass over the budget: nine passes in ten must be \
             provably under {MAX_PASS_NS} ns and it cannot show that. A host that steals a \
             vCPU touches the handful of passes it lands in and moves the maximum; a \
             scheduler whose passes grew moves one in ten of them",
            describe(report),
        ));
    }
    Ok(())
}

/// Prove the instrument in both directions, with no guest.
///
/// `serial::self_check`'s shape and its reason: a gate nothing checks is a gate
/// nobody knows is broken. Two of the cases below are the two the whole change
/// turns on — a host that took the CPU away must pass, and a scheduler whose
/// passes grew must not — and neither can be staged on a booted machine. **They
/// are the same distribution to a maximum and to an `over` count**, which is
/// the point: the pair is what says this gate reads mass rather than extremes,
/// and any repair that starts gating the maximum again fails one of them.
///
/// The last two are the limits, asserted deliberately so that a later tightening
/// turns them red rather than passing unnoticed: a pass grown to a third of the
/// budget is *accepted*, and so is one distribution in which one pass in fifty
/// went over.
pub fn self_check() -> Result<(), String> {
    let mut bulk = PassCostReport::empty(toyos_sched::hw::CpuId(0));
    // 100 000 passes at 2048..4096 ns — the shape a scheduler doing scheduling
    // rather than work produces.
    bulk.buckets[12] = 100_000;
    bulk.count = 100_000;

    // The case the change exists for: the same scheduler on a host that took
    // the vCPU away, at 1–2 ms a time — which is ten times the budget and about
    // what a whole cross-arch TCG pass cost when invariant P last fired on the
    // dev host.
    let mut stolen = bulk;
    stolen.buckets[12] -= 2_000;
    stolen.buckets[21] = 2_000;
    stolen.max_ns = 2_000_000;
    stolen.over = 2_000;

    // The case it must still catch, and it is `stolen` with the mass moved:
    // one pass in five is now over the budget. Same maximum, same shape of
    // outlier, ten times the mass — and only the quantile can tell them apart.
    let mut grown = bulk;
    grown.buckets[12] -= 20_000;
    grown.buckets[21] = 20_000;
    grown.max_ns = 2_000_000;
    grown.over = 20_000;

    // A limit, asserted on purpose: every pass at 32..64 µs is three times the
    // margin `MAX_PASS_NS` claims to hold over a pass doing scheduling, and this
    // gate accepts it because it is under the budget. The median in `describe`
    // is what reports it.
    let mut slow = PassCostReport::empty(toyos_sched::hw::CpuId(0));
    slow.buckets[16] = 100_000;
    slow.count = 100_000;
    slow.max_ns = 65_000;

    let mut short = bulk;
    short.buckets[12] = 99;
    short.count = 99;

    let cases: &[(&str, bool, PassCostReport)] = &[
        ("a scheduler doing scheduling", true, bulk),
        ("the same one on a host that stole its vCPU", true, stolen),
        ("the same maximum with one pass in five over the budget", false, grown),
        ("every pass at three times the budget's own margin", true, slow),
        ("too few samples to have a 90th percentile", false, short),
    ];
    for (name, want_ok, report) in cases {
        let got = verdict(report);
        if got.is_ok() != *want_ok {
            return Err(format!(
                "pass-cost gate self-check: `{name}` should have been {}, and it was {}: {got:?}",
                if *want_ok { "accepted" } else { "refused" },
                if got.is_ok() { "accepted" } else { "refused" },
            ));
        }
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
