//! Which tests are known to go red, on which instrument, at what rate, and on
//! whose evidence — as data rather than as prose.
//!
//! **The failure this exists to stop.** The list used to be paragraphs in
//! `specs/issues/hardware/eleven-names-red-on-ci.md` and
//! `specs/assessments/ci-plan-assessment-2026-08.md` §9.2, and a careful
//! reader can read a paragraph backwards: `rg`ing a test
//! name in it hits the sentence that names the twelve tests that came *off* the
//! list as readily as the table of the eleven that are on it, and the hit looks
//! the same either way. That happened, and the answer given to the owner was the
//! opposite of what the document said. **A list a grep can invert is not a
//! list.**
//!
//! **The shape.** A row is *one measurement*, never "a test". One name can carry
//! several — `xhci_hid_break` is 0 of 5 in one probe and red twice on `main`
//! since, on two different failures — and a schema keyed on the test name would
//! have to pick one of them to be the truth. [`Finding`] is what a measurement
//! said and [`Standing`] is whether anything has retired it; the two are
//! orthogonal, and neither is a sentence.
//!
//! **Why a zero cannot read as a red.** [`Finding::Quiet`] has no numerator: it
//! is not a `Fires` with a zero in it, it is a different variant, and
//! [`Finding::fires`] refuses a zero at compile time. So the row that says "this
//! came off the list" cannot be rendered, grepped or destructured into the row
//! that says "this reds".
//!
//! **Ask it, do not read it**: `cargo run -- --known-red <test>` prints every
//! row for one name, newest first, each with its instrument, its rate, its run
//! and the day it was taken; with no argument it prints the whole index one line
//! per name. A name with no rows answers `NOT ON THE LIST`, which is a claim
//! that nothing here has measured it and not a claim that it is green.
//!
//! **What this is not.** `EXPECTED_FAILURES` in `tests/toyos.rs` is a
//! *declaration*: a named red, with a task and a write-up, that makes a run exit
//! 0. This index declares nothing and exempts nothing — every row is a red that
//! is still a red, and a run that hits one is still red. The two overlap on
//! `hda_tone`, at two different assertions, and the query says so where they do.
//!
//! **The bound on honesty, stated plainly.** Nothing here watches a test run, so
//! no row can detect its own fix the way `Stale::OnAPass` does in
//! `tests/toyos.rs` — a rate is not falsified by one green, which is exactly why
//! that mechanism concedes a date for its intermittents. What this has instead is
//! [`SHELF_LIFE_DAYS`]: every row that still stands carries the day it was
//! measured, and a month after it the gate below reds. **The cheap honest
//! response to that red is to delete the row.** An observation nobody will
//! re-measure is not something anyone should be trusting, and an index that
//! shrinks to nothing is a true statement about how much is known.
//!
//! **What is deliberately not a row.** Gate A's thorough tier compares
//! distributions against a recorded sample (`tests/audio-baseline.toml`) and its
//! verdicts are `Fisher p=…`, not "this test went red"; those live with the
//! baseline. Metal is not an instrument here either — the suite does not run on
//! the T14.

use crate::day::Day;
use std::collections::BTreeSet;
use std::path::Path;

/// Which machine took the measurement. `specs/testing-strategy.md` §2 is the
/// table of what each one can and cannot answer; a row without this is a row
/// that will be read as being about whichever machine the reader is standing
/// at.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum Instrument {
    /// A `guest` shard: KVM on four native x86-64 cores, `-cpu host`, **one
    /// guest per machine**, `--jobs 1`, nothing else on the box.
    Ci,
    /// The dev host with the test run by itself. Cross-arch TCG on arm64.
    DevHostAlone,
    /// The dev host in the wide phase, or under another worktree's suite or
    /// build. The only instrument that can produce a contention red at all.
    DevHostLoaded,
}

impl Instrument {
    fn label(self) -> &'static str {
        match self {
            Instrument::Ci => "CI",
            Instrument::DevHostAlone => "dev host, alone",
            Instrument::DevHostLoaded => "dev host, loaded",
        }
    }

    /// What a verdict from this instrument cannot be about.
    fn cannot_say(self) -> &'static str {
        match self {
            Instrument::Ci => {
                "one guest per machine, so nothing here is about contention — the dev \
                 host's whole ALONE: GREEN class is invisible to it"
            }
            Instrument::DevHostAlone | Instrument::DevHostLoaded => {
                "cross-arch TCG, so nothing here is about which vendor's reading of an \
                 instruction the kernel depends on"
            }
        }
    }
}

/// What the measurement said. Three shapes, and they are not one shape with a
/// number in it.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Finding {
    /// It fired, at a rate. Build one with [`Finding::fires`].
    Fires { red: u32, of: u32 },
    /// It was measured `of` times and **did not fire once**. There is no
    /// numerator to misread. Build one with [`Finding::quiet`].
    Quiet { of: u32 },
    /// It fired, and nothing here is a rate — one run, or runs nobody counted.
    Seen,
}

impl Finding {
    /// `red` of `of` runs of one thing — one probe's reps, one branch's runs,
    /// one session's suites. [`Red::evidence`] says which.
    ///
    /// The three rules are compile errors rather than a test, because a `const`
    /// item's initialiser is evaluated on the way to the binary: a zero
    /// numerator is [`Finding::quiet`] and must not be able to wear this shape,
    /// one run is not a rate, and more reds than runs is a typo.
    pub const fn fires(red: u32, of: u32) -> Finding {
        assert!(
            red > 0,
            "a Fires row with no reds is a Quiet row, and the two must not be one shape"
        );
        assert!(
            of >= 2,
            "one run is not a rate — Finding::Seen is the shape for a single sample"
        );
        assert!(red <= of, "more reds than runs");
        Finding::Fires { red, of }
    }

    /// `of` runs of one thing, none of which fired.
    pub const fn quiet(of: u32) -> Finding {
        assert!(
            of >= 2,
            "one green is one sample of a rate and retires nothing — Finding::Seen"
        );
        Finding::Quiet { of }
    }

    /// Whether this measurement is a red at all.
    fn is_red(self) -> bool {
        matches!(self, Finding::Fires { .. } | Finding::Seen)
    }

    fn rendered(self) -> String {
        match self {
            Finding::Fires { red, of } => format!("FIRES {red} of {of}"),
            Finding::Quiet { of } => format!("QUIET 0 of {of}"),
            Finding::Seen => "SEEN  no rate".to_string(),
        }
    }
}

/// Whether anything has retired the measurement — orthogonal to what it said.
///
/// A `Fires` that is `Retired` is history and never a live red; a `Fires` that
/// `Stands` is the only thing that makes a name known-red.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Standing {
    /// Nothing has retired it.
    Stands,
    /// A landed fix or a later measurement retired it. Kept rather than deleted,
    /// because the next red under this name must not be read as this one.
    Retired(&'static str),
    /// **The sources disagree about whether it still stands, and this says how.**
    /// A disputed row never makes a name known-red by itself and never silences
    /// one either: the query prints the disagreement and a human decides. A row
    /// that overstates its confidence is worse than no row.
    Disputed(&'static str),
}

/// One measurement of one test on one instrument.
#[derive(Clone, Copy)]
pub struct Red {
    /// The registered test name, exactly. A renamed or deleted test takes its
    /// rows with it — the gate below refuses a name no list registers.
    pub test: &'static str,
    pub instrument: Instrument,
    pub finding: Finding,
    pub standing: Standing,
    /// What the failure said, quoted from the run wherever the source quotes it.
    pub what: &'static str,
    /// The run, probe or session this is a measurement of: a CI run id, a log
    /// file, a tree.
    pub evidence: &'static str,
    /// The write-up that carries the reasoning and the rest of the evidence, as
    /// a repository path optionally followed by a section. It must exist and it
    /// must name [`Red::test`] — a pointer that resolves to a document which has
    /// stopped being about this test is how the index and the prose drift apart.
    pub source: &'static str,
    /// `YYYY-MM-DD`, the day the measurement was taken — not the day the row was
    /// written. Every answer prints how long ago that was.
    pub measured: &'static str,
}

/// How long a standing row is worth anything without being re-taken.
///
/// A month, and the number is not invented: `tests/toyos.rs`'s two
/// `EXPECTED_FAILURES` entries use exactly this interval with exactly this
/// justification — long enough that a fix already in flight lands first, short
/// enough that nobody inherits it silently. The tree it is measured against
/// bears it out: `specs/assessments/ci-plan-assessment-2026-08.md` §10.10 has
/// nine consecutive `ci` runs on `main`, one of them green.
pub const SHELF_LIFE_DAYS: i64 = 31;

/// Every measurement, grouped by the campaign that took it.
///
/// Adding a row means answering all eight fields; there is no default and no
/// abbreviation. Retiring one means saying what retired it. Deleting one is
/// always allowed and is what an unmaintainable row should get.
pub const KNOWN_RED: &[Red] = &[
    // ---------------------------------------------------------------------
    // `probe-rate.yml` run 31258202923, tree f8f73e1, 2026-08-08: five reps of
    // the exact twelve-shard configuration `ci.yml` runs, sixty jobs, 292 tests
    // each, 1460 outcomes. 281 of the 292 names were green in all five.
    // ---------------------------------------------------------------------
    Red {
        test: "usb_transport_break",
        instrument: Instrument::Ci,
        finding: Finding::fires(5, 5),
        standing: Standing::Retired(
            "a driver defect, and it took both instruments: a Bulk-Only Reset issued while \
             the device could still answer the transfer it was recovering from. \
             `probe-xhci-break.yml` run 31264371902, control 3 of 3 red and fixed 3 of 3 green",
        ),
        what: "the transport broke 2 times; the injection is armed once per boot, so anything \
               else is a break this test did not stage",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    // ---------------------------------------------------------------------
    // A different assertion (`breaks > 2`, not the retired `breaks != 1`) on the
    // same test, `1cb11e7`'s re-issue budget. The injected disk never left its
    // two-break budget; the third line is the boot stick's own unrelated,
    // cleanly-recovered transport break, which the count does not scope out.
    // ---------------------------------------------------------------------
    Red {
        test: "usb_transport_break",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "the transport broke 3 times off one abandoned transfer, which can undo one \
               recovery and no more",
        evidence: "PR #41 (`wt/toyos-i8042tier`), run 31684437719, job 94397136494 \
                   (\"guest (4)\"), sha 711730204800d7173558f7dd96644c5910fb8cf0",
        source: "specs/issues/hardware/usb-transport-break-counts-the-boot-sticks-recovery.md",
        measured: "2026-08-13",
    },
    Red {
        test: "std_unwind",
        instrument: Instrument::Ci,
        finding: Finding::fires(5, 5),
        standing: Standing::Disputed(
            "`specs/assessments/ci-plan-assessment-2026-08.md` §9.3 says closed by `wt/toyos-fpu` \
             — `fxsave64`/`fxrstor64` on \
             all five Ring 3-reachable entries, with `fpu_isolation` as the gate that asks the \
             question on purpose. The write-up this row cites still counts it among the nine \
             that stand, and says the fix landed after this probe and has not been re-measured \
             on CI",
        ),
        what: "exit code Some(-1) — a #MF, vector 16, inside the unwinder on the spawned thread; \
               any Ring 3 process could leave a pending unmasked x87 exception behind and kill \
               the next unrelated process scheduled on that CPU",
        evidence: "probe-rate run 31258202923; isolated by probe-x87 run 31260763462, two arms \
                   of three reps differing only in `fault_gate_child`'s control word",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "std_unwind_so",
        instrument: Instrument::Ci,
        finding: Finding::fires(5, 5),
        standing: Standing::Disputed(
            "the same disagreement as `std_unwind`: \
             `specs/assessments/ci-plan-assessment-2026-08.md` §9.3 says closed by \
             `wt/toyos-fpu`, the cited write-up says not re-measured on CI",
        ),
        what: "the same #MF, on the same sub-test — the one that panics on a thread",
        evidence: "probe-rate run 31258202923; probe-x87 run 31260763462",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "metal_sim_null_audio",
        instrument: Instrument::Ci,
        finding: Finding::fires(5, 5),
        standing: Standing::Retired(
            "not soundd's device-less path, which was doing its job on every one of those boots: \
             the test read the line through a span of host wall clock. The null-sink probe, run \
             31263831141, three reps, caught it arriving 64 ms after a 500 ms window closed on one \
             rep and half a second before the ready marker on the other two. It waits on the guest \
             now",
        ),
        what: "soundd did not present a null sink on a device-less machine",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "hda_tone",
        instrument: Instrument::Ci,
        finding: Finding::fires(4, 5),
        standing: Standing::Stands,
        what: "1 mid-tone silence in the capture — gate A's harm verdict, which is not what #88's \
               `EXPECTED_FAILURES` entry covers: that entry names only \"the captured tone is not \
               one sine\"",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "late_storage_connect",
        instrument: Instrument::Ci,
        finding: Finding::fires(2, 5),
        standing: Standing::Retired(
            "what the actuator stages is an *ordering* — the disk arrives after the scan — so the \
             scan closes the window now and no host's boot speed can defeat it \
             (`specs/assessments/ci-plan-assessment-2026-08.md` §10.9)",
        ),
        what: "the boot scan bound a disk, so the port was not held empty",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "hda_two_live_refused",
        instrument: Instrument::Ci,
        finding: Finding::fires(2, 5),
        standing: Standing::Retired(
            "closed with `metal_sim_null_audio` and for the same reason: these two were the only \
             tests reading soundd's first line through a span of host wall clock",
        ),
        what: "\"presenting a null sink\" never reached the boot console",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "blocked_dump",
        instrument: Instrument::Ci,
        finding: Finding::fires(2, 5),
        standing: Standing::Stands,
        what: "two *different* reasons in the two reps — the census half, and /bin/terminal racing \
               the compositor",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "dump_nmi_probe",
        instrument: Instrument::Ci,
        finding: Finding::fires(1, 5),
        standing: Standing::Retired(
            "the actuator, not the guest's state: the deaf window spun on \
             `clock::nanos_since_boot`, whose 128-bit divide is an out-of-line call. It spins on \
             `rdtsc` against a `clock::tsc_deadline` now, so there is no address in the loop that \
             is not in `deaf_window` — 0 of 10 in run 31283095698",
        ),
        what: "the rip resolved to `u128_div_rem`, not to the spin",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "kernel_heartbeat",
        instrument: Instrument::Ci,
        finding: Finding::fires(1, 5),
        standing: Standing::Disputed(
            "two harness defects were fixed for it (the torn beat/pin pair, and a window that \
             opens at the first full mask), and \
             `specs/assessments/ci-plan-assessment-2026-08.md` §10.9's fixed arm was **1 of 10 \
             again**, on a *different* line — `cpu6 last reached one 0.349s ago`. The \
             no-CPU-missing-from-two-consecutive-lines rule was written for that line and its rate \
             has not been re-measured",
        ),
        what: "2 of 12 heartbeats dropped a healthy CPU from the mask",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "usb_disk_index_stable",
        instrument: Instrument::Ci,
        finding: Finding::fires(1, 5),
        standing: Standing::Stands,
        what: "nothing enumerated on the first controller",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    // The twelve that came off the list when `wt/toyos-clock` landed. **These are
    // the rows the prose gets read backwards on.** They are measurements that the
    // name did not fire, on the same sixty jobs as the eleven above.
    Red {
        test: "metal_sim_client_death",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits, and the \"a guest stops making \
               progress and pays its whole ceiling\" shape went with it",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "metal_sim_window_drag",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "metal_sim_pointer_churn",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits. Read the later row for this name \
               before treating it as retired",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "metal_sim_compositor_stall",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "desktop_audio_client",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits. A thirteenth name with one sample \
               each way on one tree — stalled in run 31264914759 and passed in 31266194663, same \
               commit, half an hour apart — which is a rate and not a reproduction",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "desktop_typing_damage",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits. Distinct from the QEMU 8.2.2 red \
               `specs/assessments/ci-plan-assessment-2026-08.md` §8.3 records, which was closed \
               by putting the dev host's own QEMU in the container",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "doom_sound_flood",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "i8042_health_cadence",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "sshd_fail_closed",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "came off the list with `wt/toyos-clock`'s waits",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "xhci_hotplug",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "0 of 5, and the write-up says only that it *coincides* with `wt/toyos-clock`'s \
               waits — nothing named a trigger for the KVM wedge it used to give",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/xhci-flap-wedges-under-kvm.md",
        measured: "2026-08-08",
    },
    Red {
        test: "xhci_hid_break",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "0 of 5, and the write-up is explicit about what this does and does not cover: it \
               is about the \"guest stops making progress and pays its whole ceiling\" shape, \
               which prints a timeout. It is **not** cover for the endpoint-count red under the \
               same name",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "screen_pager_keys",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "0 of 5 on CI — while the dev host has it reproducing alone on `main` in the same \
               week, and `main`'s own CI went red on it once since. Read all three rows",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-08",
    },
    Red {
        test: "xhci_flap",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "PASS 5 of 5, in 7–9 s, on the same image and the same accelerator that wedged it — \
               and nothing in `toyos_xhci` changed between the two runs. A defect that stopped \
               appearing under an unchanged driver is a defect whose trigger nobody has named",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/xhci-flap-wedges-under-kvm.md",
        measured: "2026-08-08",
    },
    Red {
        test: "xhci_slow_connect",
        instrument: Instrument::Ci,
        finding: Finding::quiet(5),
        standing: Standing::Stands,
        what: "0 of 5 in the probe — which the write-up says is not the reassurance it looks like, \
               because the margin is inside the *guest's* boot and running alone moves it by \
               milliseconds rather than by a verdict",
        evidence: "probe-rate run 31258202923, tree f8f73e1, five reps",
        source: "specs/issues/hardware/xhci-slow-connect-has-a-1ms-margin.md",
        measured: "2026-08-08",
    },
    Red {
        test: "xhci_slow_connect",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`ALONE: red again — the defect is real`. `SLOW_CONNECT_NS` holds the ports empty for \
               0.3 s and the controller starts at 0.296–0.311 s on a quiet host, so the gate reds \
               whenever anything moves boot by ten milliseconds. That sensitivity is why the \
               log-ring regression was caught at all — no other gate in the suite noticed 350 ms — \
               and its own message names the fix: widen `SLOW_CONNECT_NS`, not the gate",
        evidence: "run 31261669826, the first on a tree carrying the harness's re-run-alone work",
        source: "specs/issues/hardware/xhci-slow-connect-has-a-1ms-margin.md",
        measured: "2026-08-08",
    },
    // ---------------------------------------------------------------------
    // Run 31247206462: twelve shards on KVM at `--jobs 1`, 2026-08-08. Every one
    // of these was red again when re-run alone, and none reproduced on the dev
    // host. Recorded so that the next green run cannot quietly be read as their
    // absence.
    // ---------------------------------------------------------------------
    Red {
        test: "doom_sound_flood",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`timed out after 88s` alone, against 4–26 s on the dev host. Nothing here is \
               diagnosed",
        evidence: "run 31247206462, red again alone",
        source: "specs/issues/hardware/four-runner-reds-unclassified.md",
        measured: "2026-08-08",
    },
    Red {
        test: "hda_client_stall",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`the ring arm: timed out`, and `timed out after 9s` alone. The one of that run's \
               four that is still standing",
        evidence: "run 31247206462, red again alone",
        source: "specs/issues/hardware/four-runner-reds-unclassified.md",
        measured: "2026-08-08",
    },
    Red {
        test: "sshd_fail_closed",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "red alone in 22 s, having taken 152 s in the phase. Not diagnosed, and 0 of 5 in \
               the rate probe five days later",
        evidence: "run 31247206462, red again alone",
        source: "specs/issues/hardware/four-runner-reds-unclassified.md",
        measured: "2026-08-08",
    },
    Red {
        test: "xhci_hotplug",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`timed out after 66s` alone — `device_add`/`device_del` against a 100 ms debounce. \
               Green on the dev host in seconds and green under TCG on the same runner image and \
               the same QEMU",
        evidence: "run 31247206462, red again alone",
        source: "specs/issues/hardware/xhci-flap-wedges-under-kvm.md",
        measured: "2026-08-08",
    },
    Red {
        test: "xhci_hid_break",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`timed out after 75s` alone — a staged transfer error on a HID endpoint. Green on \
               the dev host and under TCG on the same runner image",
        evidence: "run 31247206462, red again alone",
        source: "specs/issues/hardware/xhci-flap-wedges-under-kvm.md",
        measured: "2026-08-08",
    },
    Red {
        test: "metal_sim_pointer_churn",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Retired(
            "closed — a console the test had counted before it caught up",
        ),
        what: "`bound 0 pointer sources` alone, over 8 plug/unplug cycles under a live compositor",
        evidence: "run 31247206462, red again alone",
        source: "specs/issues/hardware/xhci-flap-wedges-under-kvm.md",
        measured: "2026-08-08",
    },
    Red {
        test: "xhci_flap",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`timed out after 164s` alone: three collapsed replugs survive and the fourth never \
               answers — the guest goes silent at about 4.4 s and never speaks again. Green under \
               TCG on the same runner image and the same QEMU, and green on the dev host, because \
               KVM runs the guest ~50× further between the host's two QMP writes",
        evidence: "run 31246245541, `debian:sid`/QEMU 11.0.3/KVM, `--jobs 1`, alone",
        source: "specs/issues/hardware/xhci-flap-wedges-under-kvm.md",
        measured: "2026-08-08",
    },
    // ---------------------------------------------------------------------
    // `probe-green.yml` run 31282019974, tree 98e7247 (`main` 83ef8d1 plus the
    // workflow), 2026-08-09: ten reps, one job per rep and one `cargo test` per
    // name, aimed at the four names four consecutive red `main` runs had produced.
    // ---------------------------------------------------------------------
    Red {
        test: "desktop_window_child",
        instrument: Instrument::Ci,
        finding: Finding::fires(2, 10),
        standing: Standing::Stands,
        what: "the surface owner exited before it said it was ready — the /bin/terminal boot race, \
               2 of 10 **on a runner with one guest on it and nothing to contend with**. Rep 2 has \
               the compositor spawned at 0.347 s, the terminal at 0.349 s, and the terminal exiting \
               at 0.849 s one millisecond before the compositor maps its framebuffer",
        evidence: "probe-green run 31282019974, tree 98e7247, ten reps",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.9",
        measured: "2026-08-09",
    },
    Red {
        test: "dump_nmi_probe",
        instrument: Instrument::Ci,
        finding: Finding::fires(2, 10),
        standing: Standing::Retired(
            "the `rdtsc`/`clock::tsc_deadline` spin: 0 of 10 in run 31283095698",
        ),
        what: "`compiler_builtins::int::specialized_div_rem::u128_div_rem+0x99`",
        evidence: "probe-green run 31282019974, tree 98e7247, ten reps",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.9",
        measured: "2026-08-09",
    },
    Red {
        test: "kernel_heartbeat",
        instrument: Instrument::Ci,
        finding: Finding::fires(1, 10),
        standing: Standing::Disputed(
            "run 31283095698, the fixed arm of the same ten reps, was 1 of 10 again on a different \
             line. See the note on this name's probe-rate row",
        ),
        what: "2 of 11 beats dropped a CPU from the mask",
        evidence: "probe-green run 31282019974, tree 98e7247, ten reps",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.9",
        measured: "2026-08-09",
    },
    Red {
        test: "desktop_audio_client",
        instrument: Instrument::Ci,
        finding: Finding::fires(1, 10),
        standing: Standing::Retired(
            "soundd builds each line and issues one `write_all` (its local `say!`) now: 0 of 10 in \
             run 31283095698. The other 176 `eprintln!` sites in `userland/` still do not — and \
             the shape that left open is closed at the kernel by L5 of the log architecture: a \
             `ConsoleObject` per holder buffers its line and emits it whole under one \
             `BackendGuard`, so a kernel record cannot land inside one. `console_line_atomicity` \
             is the gate, 0 of 2000, and 8 of 8 red under `console-unbuffered`",
        ),
        what: "`STALLED` waiting for both clients to leave the mixer: `soundd: client ` and \
               `1 removed` came back either side of the kernel's four `exit:` accounting lines, so \
               the test counted one removal of two and waited out its 300 s guard. Systematic \
               rather than chance — soundd prints a client's removal exactly while the kernel \
               prints that client's exit",
        evidence: "probe-green run 31282019974 rep 10, and run 31271983043 on `main`",
        source: "specs/log-architecture-spec.md §4.4",
        measured: "2026-08-09",
    },
    // ---------------------------------------------------------------------
    // Run 31283095698, 2026-08-09: the fixed arm, ten reps of the same four names
    // on the same image with the same accelerator.
    // ---------------------------------------------------------------------
    Red {
        test: "dump_nmi_probe",
        instrument: Instrument::Ci,
        finding: Finding::quiet(10),
        standing: Standing::Stands,
        what: "0 of 10 against 2 of 10 in the arm before it",
        evidence: "probe-green fixed arm, run 31283095698, ten reps",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.9",
        measured: "2026-08-09",
    },
    Red {
        test: "desktop_audio_client",
        instrument: Instrument::Ci,
        finding: Finding::quiet(10),
        standing: Standing::Stands,
        what: "0 of 10 against 1 of 10 in the arm before it",
        evidence: "probe-green fixed arm, run 31283095698, ten reps",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.9",
        measured: "2026-08-09",
    },
    Red {
        test: "desktop_window_child",
        instrument: Instrument::Ci,
        finding: Finding::fires(2, 10),
        standing: Standing::Stands,
        what: "2 of 10, untouched by the three fixes that made the other names in the same arm \
               green. It is the one name left between `main` and §10.4's three-consecutive-greens \
               trigger",
        evidence: "probe-green fixed arm, run 31283095698, ten reps",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.9",
        measured: "2026-08-09",
    },
    // ---------------------------------------------------------------------
    // Single `ci` runs on `main`, `specs/assessments/ci-plan-assessment-2026-08.md`
    // §10.9 and §10.10.
    // ---------------------------------------------------------------------
    Red {
        test: "dump_nmi_probe",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Retired(
            "expiring the ask takes it back with a CAS, and a CAS that *fails* means the victim \
             went deaf on the boundary and the report is asked for after all — so the give-up was \
             repaired rather than the number tuned",
        ),
        what: "a signature neither probe produced — *the dump never ran*, both attempts. cpu0 waits \
               100 ms for the victim to reach its idle loop and on that runner it took 251 ms. \
               0 of 20 probe reps and 2 of 2 in one shard job",
        evidence: "run 31284962381, `main` at 1ed6f39",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.9",
        measured: "2026-08-09",
    },
    Red {
        test: "late_storage_connect",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Retired("the scan closes the window now, not a duration"),
        what: "`xhci-slow-storage-connect` hid the disk's port for 300 ms — a claim about how far \
               into a boot `scan_ports` runs, true at 253 ms on the dev host and false at 407 ms \
               on that runner",
        evidence: "run 31286199802, `main` at 8d3f5b7",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.9",
        measured: "2026-08-09",
    },
    Red {
        test: "screen_pager_keys",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "keystroke 14 of 30. Bisected on the dev host to `f96d52e`, a merge whose two parents \
               are both green — see `specs/issues/diagnostics/screen-pager-keys-red-on-main.md`",
        evidence: "run 31287853270, `main` at 53d29d5",
        source: "specs/assessments/ci-plan-assessment-2026-08.md §10.10",
        measured: "2026-08-09",
    },
    // ---------------------------------------------------------------------
    // `main`'s own `ci` runs, read off GitHub with `gh run view --log-failed` on
    // 2026-08-11. No write-up in the tree records these three, which is why they
    // are here: an index whose newest row predates the newest red is the thing
    // this file exists to stop.
    // ---------------------------------------------------------------------
    Red {
        test: "xhci_hid_break",
        instrument: Instrument::Ci,
        finding: Finding::fires(2, 15),
        standing: Standing::Stands,
        what: "`3 endpoint(s) were found Running after the break, want 2` — dci 3 is the first IN \
               endpoint of every USB device, so one transport recovery on the boot USB disk \
               anywhere in the boot reds a test whose failure is about HID. `ALONE: GREEN` both \
               times, which the harness itself calls a rate and not a classification",
        evidence: "`main`'s fifteen most recent completed `ci` runs, 2026-08-09 to 2026-08-11: red \
                   in 31289459932 (a76a078) and 31331494794 (0e48d2e), read with \
                   `gh run view --log-failed`",
        source: "specs/issues/hardware/xhci-hid-break-counts-any-endpoint-3.md",
        measured: "2026-08-11",
    },
    Red {
        test: "xhci_hid_break",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`STALLED: 133s of guard expired, and the guest had said nothing for the last 131s of \
               it` — the timeout shape the probe measured at 0 of 5, twice in one job, and \
               `ALONE: red again`. So that 0 of 5 is not cover for it either",
        evidence: "run 31422708833, `main` at 2572e4b, shard 10",
        source: "specs/issues/hardware/eleven-names-red-on-ci.md",
        measured: "2026-08-10",
    },
    // ---------------------------------------------------------------------
    // The endpoint-count shape, seen on two PR branches rather than on `main`.
    // Neither branch's diff touches the test or the xHCI driver, so this is
    // the same defect the row above measures, on a denominator that row's
    // "fifteen most recent `main` runs" does not cover — hence `Seen`, not a
    // bump to that row's rate.
    // ---------------------------------------------------------------------
    Red {
        test: "xhci_hid_break",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`3 endpoint(s) were found Running after the break, want 2` — the wide run's message. \
               The harness's re-run-alone failed too, but on a *different* assertion, the \
               `input never came back` shape `parallel-tests-red-under-other-suites.md` records on \
               the dev host — its first appearance on CI. `ALONE xhci_hid_break: red again` quoted \
               the wide run's message regardless, because that line always carries the original text",
        evidence: "PR #22 (`wt/toyos-endow`), run 31424496450 attempt 1, job 93586744461 \
                   (\"guest (5)\"), sha 73d0761b",
        source: "specs/issues/hardware/xhci-hid-break-counts-any-endpoint-3.md",
        measured: "2026-08-10",
    },
    Red {
        test: "xhci_hid_break",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`3 endpoint(s) were found Running after the break, want 2`, byte-identical between \
               the wide run (51s) and the alone re-run (9s) — the first occurrence where isolation \
               reproduces this exact assertion rather than going green or landing on the other \
               shape. 9s alone rules out host contention for this instance",
        evidence: "PR #35 (`codex/debug-wait-census`), run 31601325987, job 94129283847 \
                   (\"guest (5)\"), sha d522424e",
        source: "specs/issues/hardware/xhci-hid-break-counts-any-endpoint-3.md",
        measured: "2026-08-12",
    },
    Red {
        test: "metal_sim_pointer_churn",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`[qemu] Init process crashed during boot:`, 244 s in the phase, and \
               `ALONE: GREEN, and it was alone both times — nothing the harness controls differed, \
               so it failed once and passed once. That is a rate and not a classification`. The \
               name is 0 of 5 in the probe and declared closed in a write-up",
        evidence: "run 31396171916, `main` at 7af7c20, shard 2",
        source: "specs/issues/hardware/metal-sim-pointer-churn-red-again-on-main.md",
        measured: "2026-08-10",
    },
    // ---------------------------------------------------------------------
    // Invariant P on KVM. The dev-host row below predicted this case and said
    // what it would mean: "if invariant P ever fires on a KVM shard, this file
    // does not cover it". It has. The two rows are the same assert on two
    // accelerators and they are not one measurement — the magnitudes differ by
    // three orders and so do the call sites.
    // ---------------------------------------------------------------------
    Red {
        test: "sched_check_build",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`invariant P: a scheduler pass took 200569 ns, budget 200000 ns` — the assert \
               firing on native x86-64 under KVM, in `timer_handler` -> `driver::pass` -> \
               `SchedPass::finish` while `test_rs_sched_stress` pid=7 was in syscall 8, at 1.449 s \
               of guest uptime. `schedule_no_return: panicked inside a pass, cannot rejoin` halts \
               every CPU 1 ms later, which is the whole of the 383 s of silence the run then \
               reported as `STALLED:`. **The red is the panic and the stall is its shadow**: \
               `run_test_paced` ends early on `KERNEL PANIC` only, which the CPU-exception path \
               prints and a Rust `panic!` does not, so the wait ran its full 382 s ceiling on a \
               machine that had been halted since second two and the summary said the run \
               `established nothing about this tree`",
        evidence: "PR #95 (`wt/toyos-harness2`), run 31946183485, job 95162423932 (\"guest (8)\"), \
                   sha 4ec5d01, on an Azure 4-core EPYC 7763 with `/dev/kvm` — so KVM nested in a \
                   hypervisor guest. One firing, not a rate: the earlier STALL under this name \
                   (run 31890991692, job 95027203184, guest 8, 2026-08-15) printed an empty \
                   `serial:` because `in_test` never became true, so its lines went to \
                   `TestResult::before` and the caller drops that — its cause is unrecorded and is \
                   not counted here",
        source: "specs/issues/kernel/the-check-build-guest-stopped-answering-on-kvm-twice.md",
        measured: "2026-08-16",
    },
    // ---------------------------------------------------------------------
    // The dev host. Everything below is TCG on arm64, so none of it is evidence
    // about which vendor executes an instruction — and all of it is about a
    // machine CI has no way to construct.
    // ---------------------------------------------------------------------
    Red {
        test: "sched_check_build",
        instrument: Instrument::DevHostAlone,
        finding: Finding::fires(2, 2),
        standing: Standing::Stands,
        what: "`invariant P: a scheduler pass took 1684167 ns, budget 200000 ns`, panicking in \
               `driver::idle_loop` before userland — then 1749243 ns on cpu1 in the isolated \
               re-run. The dev host emulates x86-64 instruction by instruction while the guest \
               TSC advances with host wall clock, and eight to nine times the budget is what that \
               costs. Ruled out rather than assumed: removing `check_cpu` from inside the \
               measured window left 1705987 ns, and `pass` samples its clock after `drain_irqs`, \
               so the xHCI prologue is outside the window entirely",
        evidence: "`cargo test -- sched_check_build` on this branch, two boots (parallel phase \
                   then ALONE re-run); green on KVM the same day — twelve of twelve guest shards, \
                   run 31875856466, where it measured 5,879 ms. **What this row may no longer be \
                   read as saying is that the budget fits natively**: the same assert has since \
                   fired on a KVM shard at 200569 ns, which is the `Instrument::Ci` row above. \
                   The TCG explanation of *this* magnitude stands; the implied claim about the \
                   other accelerator does not",
        source: "specs/issues/kernel/invariant-p-cannot-hold-under-cross-arch-tcg.md",
        measured: "2026-08-15",
    },
    Red {
        test: "screen_pager_keys",
        instrument: Instrument::DevHostAlone,
        finding: Finding::fires(3, 3),
        standing: Standing::Stands,
        what: "`0 page moves over 30 keystrokes in 0.4s — an unattended deadline alone could have \
               produced 1.1 of them`. Not load: the landing gate that produced one of them ran at \
               1.05× the reference boot and the failure was byte-identical to the ones taken at \
               load 11–16. Bisected to `f96d52e`, a merge whose two parents are both green",
        evidence: "`main` at b36cf64, three runs alone in one session; seven boots across the bisect",
        source: "specs/issues/diagnostics/screen-pager-keys-red-on-main.md",
        measured: "2026-08-08",
    },
    Red {
        test: "hda_tone",
        instrument: Instrument::DevHostAlone,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`1 mid-tone silences in the capture: total 1 [1p×1]` — the harm assertion, which \
               #88's exemption is right not to cover, so any landing whose gate is `cargo test` is \
               red on `main` for this and an agent will read it as theirs",
        evidence: "`main` at 6d11938, alone",
        source: "specs/issues/audio/hda-tone-red-beyond-its-exemption.md",
        measured: "2026-08-07",
    },
    Red {
        test: "hda_tone",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(1, 3),
        standing: Standing::Retired(
            "the splice is unrepresentable since L5 of the log architecture: a `ConsoleObject` per \
             holder buffers its line and emits it whole under one `BackendGuard`, so the kernel \
             record that cut this needle open has nowhere to be acquired. `console_line_atomicity` \
             is the gate, 0 of 2000 with 8 of 8 red under `console-unbuffered` \
             (2026-08-15, at counts from 1 to 570 of 2000 — the magnitude is a race and only \
             the sign is a verdict)",
        ),
        what: "the needle `soundd: hda codec0 vendor=1af4` split in half by another writer, between \
               `codec` and `0`. Three full suites on one tree in one session, red on the third — so \
               it is not the audio path and not load in any way a re-run answers; it is which two \
               writers happen to collide",
        evidence: "landing-1786130703-71774.log, a documentation-only branch",
        source: "specs/log-architecture-spec.md §4.4",
        measured: "2026-08-07",
    },
    Red {
        test: "hda_tone",
        instrument: Instrument::DevHostAlone,
        finding: Finding::quiet(3),
        standing: Standing::Retired(
            "the loaded arm it was the control for is retired above; a quiet reading whose red \
             half has gone is not evidence of anything on its own",
        ),
        what: "green 3 of 3 alone on a quiet host, against the splice red in the same session",
        evidence: "the same session as the splice above",
        source: "specs/log-architecture-spec.md §4.4",
        measured: "2026-08-07",
    },
    Red {
        test: "audio_tone_load",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "gate A's fast tier, failing its own two-boot rule — dropouts on the first boot *and* \
               on the confirming re-boot — four times in one session at smp=1, on two different \
               trees. **The denominator is not readable**: the write-up says \"six runs in one \
               session\" and its own listing is four reds, one green and \"twice more GREEN\", \
               which is seven. smp=8 failed the same rule twice on 2026-08-07",
        evidence: "2026-08-04 session, 5408cfb with the bundle stashed and bundle D alternating",
        source: "specs/issues/audio/audio-tone-load-fast-tier-intermittent.md",
        measured: "2026-08-04",
    },
    Red {
        test: "audio_tone_load",
        instrument: Instrument::DevHostAlone,
        finding: Finding::quiet(3),
        standing: Standing::Stands,
        what: "`main`, alone, three times, green at both widths, with wake latencies of 6.5–54 ms \
               where every red carried 76–297 ms — soundd not being scheduled rather than a cost \
               per period",
        evidence: "task #58's A/B session, `main`'s tip against a branch, one host",
        source: "specs/issues/audio/audio-tone-load-fast-tier-intermittent.md",
        measured: "2026-08-07",
    },
    // The contention class. Every one of these is a verdict that expires on the
    // host's clock, and CI is structurally unable to produce or refute one.
    Red {
        test: "i8042_mouse",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`1003 pointer events reached userland out of 1004 packets injected, never more than \
               4 of them (12 bytes) outstanding against a 16-byte device queue` — inside the bound \
               the summing fix installed, so that mechanism is not what this is. A/B in one session \
               put `main`'s kernel red with the identical line and the branch green",
        evidence: "two full suites in one worktree while a second held six of the twelve guest slots",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-07",
    },
    Red {
        test: "i8042_absent",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`601ms without an i8042 and 287ms with one` against a 300 ms allowance. The absolute \
               figure moved 277→619 ms across three runs of one boot with no code change, and it is \
               already `Sched::Serial`, so intra-suite width is not what reaches it",
        evidence: "a landing gate, then alone minutes later on both trees",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-04",
    },
    Red {
        test: "desktop_locale_detect",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`nothing typed at the terminal window reached a shell`, `ALONE … GREEN`, on a branch \
               that touches neither the compositor nor the terminal",
        evidence: "one full suite on a host carrying three to four concurrent suites",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-05",
    },
    Red {
        test: "netd_connection_caps",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "red at 50 s inside a landing gate that was otherwise 257/259, green in 7 s alone on \
               the same tree moments later, on a branch that touches neither netd nor the network \
               stack",
        evidence: "a landing gate, then alone on the same tree",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-05",
    },
    Red {
        test: "dump_nmi_probe",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`the NMI went unanswered too` — its wall-clock verdict expiring on a host carrying \
               three other worktrees' suites. It is `Sched::Serial`, so it failed in the serial \
               tail and the harness never re-ran it alone; run alone moments later it passes in \
               23 s. Nothing should widen its millisecond",
        evidence: "one full suite; the run's `[host-slots]` lines name all three worktrees",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-07",
    },
    Red {
        test: "blocked_dump",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`nothing typed at the terminal window reached a shell`, `ALONE … GREEN` in 5 s. Its \
               verdict is the dump's content, but *reaching* the dump crosses a compositor, a \
               terminal and a shell, and that step is a wall-clock margin",
        evidence: "one full suite under load, and a second landing gate in the eight-landing regime",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-07",
    },
    Red {
        test: "fd_lifetime",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(4, 7),
        standing: Standing::Stands,
        what: "`a killed process kept 16777216 bytes of its io_urings`, `ALONE … GREEN` every \
               time. `kill_releases_ring` asks `SYS_SYSINFO` for the **machine's** free memory \
               either side of a kill, and it shares the `tests/testcases` boot with every other \
               Rust guest binary — so the verdict is only sound while nothing else in that guest \
               holds or releases a page across the window, which nothing arranges. `/bin/logd` \
               joining every image is what made it loud: it holds an `io_uring`, a 64 KiB record \
               buffer and a `File` whose page-cache pages come and go",
        evidence: "a same-session A/B of two seven-suite arms, 12 wide, on one dev host: 0 of 7 at \
                   a76ffd0 against 4 of 7 at 19ce5d0, whose diff is comment text, one \
                   caller-less kernel function deleted and a test-runner gate nothing on this \
                   boot invokes. Two earlier sevens on the same two trees gave 1 of 7 and 2 of 7, \
                   so the rate this row carries is the widest of four readings and not the only \
                   one",
        source: "specs/issues/build/free-memory-verdicts-share-a-boot.md",
        measured: "2026-08-15",
    },
    Red {
        test: "screen_console_scroll",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`round 1: the guest never printed CHURN-DONE 0 100`, **598 s** in the wide phase \
               against a phase that is ~45 s on a quiet host, `ALONE … GREEN`. The landing gate it \
               killed ran 778.9 s with four other `--land` processes on the host, on a branch whose \
               whole delta was two documentation lines",
        evidence: "a landing gate on a documentation-only branch",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-07",
    },
    Red {
        test: "xhci_hid_break",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`input never came back: no pointer event moved by (2560, -1920); deltas seen: \
               [(256, 256), (256, 256)]`, `ALONE … GREEN`. The two deltas it did see are the \
               boot-time absolute tablet, so what went missing is the relative mouse's event after \
               the staged break — a wall-clock margin on the recovery path, not a recovery that \
               failed",
        evidence: "a landing gate on a branch whose delta was one documentation commit",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-07",
    },
    Red {
        test: "screen_early_panic",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`ALONE … GREEN`. One branch's two consecutive landing gates died on two *different* \
               tests from this list — `blocked_dump`, then this — with eight `toyos-build --land` \
               processes queued on the integration lock at once. Guest slots bound guests, and a \
               landing storm is not made of guests",
        evidence: "the eight-landing regime, 2026-08-07",
        source: "specs/issues/build/parallel-tests-red-under-other-suites.md",
        measured: "2026-08-07",
    },
    Red {
        test: "screen_blocked_dump",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Retired(
            "the defect was in the kernel and closed 2026-08-08 — which is the caution the rest of \
             that list now carries: it reds at ~20% with the host to itself, so `ALONE: GREEN` on \
             it said \"this re-run was one of the four green ones\" and not \"the phase did it\"",
        ),
        what: "`ALONE: GREEN` twice and `ALONE: red again` once across four full suites in one \
               session",
        evidence: "four full suites on `wt/toyos-tlbfix`, 2026-08-07",
        source: "specs/issues/build/desktop-window-child-holds-a-lane.md",
        measured: "2026-08-07",
    },
    Red {
        test: "screen_blocked_dump",
        instrument: Instrument::Ci,
        finding: Finding::fires(1, 2),
        standing: Standing::Stands,
        what: "`the report the keystroke painted does not carry \"== VERDICT:\"`; the decoded \
               panel was the boot-log tail ending `[page 2/4]`, with none of the dump's three \
               summary markers. The isolated re-run painted `0 overdue, 0 absurd, 0 unheld, 0 \
               never ran` and passed in 6 s",
        evidence: "PR #33 run 31472702284, job 93736011023, merge ref \
                   1d19104d1b832da1aaad43906e0673cb87db93ba",
        source: "specs/issues/diagnostics/blocked-dump-cannot-fire-on-a-total-freeze.md",
        measured: "2026-08-11",
    },
    Red {
        test: "screen_blocked_dump",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`the report the keystroke painted does not carry \"== VERDICT:\"`, after 520 s in \
               the wide phase; the isolated re-run was green. This is the no-verdict shape, not \
               the retired compositor-overlay red under the same test name",
        evidence: "one 12-wide full suite on 2026-08-09 while a second worktree's suite was live, \
                   then the harness's isolated re-run",
        source: "specs/issues/diagnostics/blocked-dump-cannot-fire-on-a-total-freeze.md",
        measured: "2026-08-09",
    },
    Red {
        test: "desktop_typing_damage",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(3, 7),
        standing: Standing::Retired(
            "`shell_answers` retyped `echo <nonce>` against `qemu::budget(20 s)` because nothing \
             knew when the terminal was up, so \"how long does a desktop take to come up on the \
             host of the day\" *was* the verdict. The terminal prints `terminal: ready` now and the \
             coming-up half waits on the guest's own liveness",
        ),
        what: "`nothing typed at the terminal window reached a shell`, 243–255 s in the wide phase \
               against 16 s alone. The victim is positional: `desktop_window_child` held a lane for \
               ~250 s of every run and whichever desktop the duration profile ranked next went in \
               beside it",
        evidence: "seven full runs in one worktree, one session",
        source: "specs/issues/build/desktop-window-child-holds-a-lane.md",
        measured: "2026-08-06",
    },
    Red {
        test: "desktop_audio_client",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(1, 7),
        standing: Standing::Retired("`terminal: ready`, as for `desktop_typing_damage`"),
        what: "248 s in the wide phase against 14 s alone — the same lane, promoted into it by the \
               duration profile. It cost another landing on 2026-08-07 at 787 s wide against 14 s \
               alone, with its own verdict line rather than the typing one, so the message is not \
               the tell and the pair of durations is",
        evidence: "seven full runs in one worktree, one session",
        source: "specs/issues/build/desktop-window-child-holds-a-lane.md",
        measured: "2026-08-06",
    },
    Red {
        test: "desktop_window_child",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(10, 10),
        standing: Standing::Retired(
            "not a guest defect: `close_focused_window` looped on `log[new..]` but waited with \
             `serial_until`, which scans the whole capture, so the previous probe's `windows=1` \
             answered instantly and it re-sent GUI+Q at the speed of a QMP round trip — closing the \
             window under the one it meant. It waits on the compositor's `note_closed` event now. \
             **This retires the test's reds and not #156**, whose signature is a guest that goes \
             silent",
        ),
        what: "hit 10/10 across four invocations in the 12-wide parallel phase, with the harness's \
               re-run-alone pass reporting GREEN each time",
        evidence: "four invocations by an agent landing unrelated documentation",
        source: "specs/issues/kernel/desktop-window-child-freeze.md",
        measured: "2026-08-06",
    },
    Red {
        test: "metal_sim_window_caps",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Retired(
            "two CPUs shooting down at once — a mutual wait and not a bound, so no deadline value \
             was ever going to fix it. `kernel/src/shootdown.rs`, gated by \
             `an_initiator_answers_while_it_waits`",
        ),
        what: "FAIL 5 s in the wide phase three times, PASS 3 s alone on the branch and 36 s alone \
               on `main`. Its own work *completes* — `window caps: oversized refused, 62 windows \
               granted then refused` — and the process then exits `-1` after two CPUs have each \
               stalled five seconds on `tlb: cpu N has not flushed for generation …`",
        evidence: "two `--land` gates on `wt/toyos-boot` and five A/B runs against `main` at \
                   6d11938, one session",
        source: "specs/issues/audio/wide-phase-reds-under-load.md",
        measured: "2026-08-07",
    },
    Red {
        test: "null_sink_shipped_client",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Retired("the same shootdown deadlock"),
        what: "FAIL 10 s in the wide phase, PASS 4 s alone on the branch and 5 s alone on `main`, \
               with the same two `tlb:` lines in the capture",
        evidence: "two `--land` gates on `wt/toyos-boot`, one session",
        source: "specs/issues/audio/wide-phase-reds-under-load.md",
        measured: "2026-08-07",
    },
    // ---------------------------------------------------------------------
    // `wt/toyos-logd`, dev host, 2026-08-15: **fourteen** full suites in one
    // session — an interleaved A/B of L3's review finding F1, five suites with
    // the branch tip's single `BackendGuard` acquisition around a
    // userland-chosen length and nine with `write_console`'s window bounded
    // (five of the A/B and the four the landing gate then ran). **That the
    // rates do not move is the finding**: the branch's remaining suspicion for
    // both names was an interrupts-off window it owns, and bounding the last
    // one leaves each where it was — i8042 2 of 9 against 1 of 5, macro 3 of 9
    // against 1 of 5, which for counts this size is the same rate. So what is
    // left belongs to the two write-ups cited here. Adjudicated rather than
    // carried, per the root CLAUDE.md.
    // ---------------------------------------------------------------------
    Red {
        test: "i8042_undecoded_bytes",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(3, 14),
        standing: Standing::Retired(
            "the counters are one word, 2026-08-17 — and this retirement is a measurement where \
             the one it replaces was an argument. `kernel/src/drivers/i8042/tally.rs` is a single \
             `u64` the ISR writes **once, after the burst**, low half the interrupts that put a \
             byte in the ring and high half those that found none, and `Counts` can only be built \
             by one load: there is no subtraction left to be wrong and no instant at which the \
             halves disagree. Moving that write to the end also ends the producer this row's own \
             line actually had — `IRQS` moved on the way *in*, so a reader between the pin and the \
             first `push_isr` held a count of arrived bytes with no byte anywhere — and `RX_BYTES` \
             is now counted in `pop` rather than after the drain, so a byte in mid-decode is on \
             one side of the report's `has_bytes` guard rather than neither. Between them `N \
             interrupts and 0 bytes, nothing decoded` is unprintable. **Measured in both \
             directions**: `kernel-loom/tests/i8042_tally.rs` reds with the two counters put back \
             (`Counts { carried: 1, empty: 0 }` for an interrupt that carried nothing) and passes \
             with the word, and a third model asserts the old shape really is read torn so the \
             file cannot pass vacuously. **Nine full `cargo test` suites, 9 green.** Not cover for \
             the CI row under this name, which is a different producer and still stands",
        ),
        what: "`the line names no byte: [kernel 0.418 cpu1] i8042: 1 interrupts and 0 bytes, \
               nothing decoded — first seen at 418ms`. The test takes the *first* `nothing \
               decoded` line in the capture and assumes it is the one its injection produced, and \
               an interrupt whose byte the driver's own polling init already consumed produces an \
               earlier one. **The isolated re-run answered differently on the two arms** — `red \
               again` on one occurrence and `ALONE: GREEN` on the other — which is itself evidence \
               that the timing and not the arm decides it. \
               \n\n**Retired 2026-08-16, and the retirement is withdrawn 2026-08-17: the driver \
               half does not hold.** It claimed the driver says `nothing decoded` only when \
               something arrived to decode. It does not, because the two counters that decide are \
               read torn: the ISR adds to `IRQS` on entry (`i8042/mod.rs:663`) and to `EMPTY_IRQS` \
               only after draining and finding nothing (`:693`), with the port-drain loop between \
               them, while `report_health` computes `carried = IRQS - EMPTY_IRQS` (`:390`) and \
               prints whenever `carried > 0`. A reader landing inside that window sees \
               `carried = 1` for an interrupt that carried nothing. Observed \
               again 2 of 6 full suites on 2026-08-17 (`1 interrupts and 0 bytes … first seen at \
               449ms`, PR #106's author, on a tree containing the fix). Whether the test half — \
               anchoring on `===I8042_READY===` — holds is a separate question and is not decided \
               here. \
               \n\n**The withdrawal above went on to say the torn read `prints this row's line \
               exactly`, and that clause was wrong** (corrected 2026-08-17, PR #112). The torn \
               read is real and is fixed, but it is not what produced this line, and the boot \
               order says so: the reporting CPU is `cpu1`, an AP, and `i8042::init` runs on the \
               BSP *before* `smp::boot_aps` — so at the bring-up interrupt this row always named \
               there is no second CPU in existence to land inside the ISR's window. What did \
               produce it is a different window in the same handler: `IRQS` was incremented on \
               **entry**, ahead of the first `push_isr`, so a reader between the pin asserting and \
               the first byte reaching the ring held a count of arrived bytes with no byte \
               anywhere — `carried = 1`, `RX_BYTES = 0`, `has_bytes()` false, which is this line. \
               Both windows close the same way and did, in PR #111. **The withdrawal itself \
               stands**: retiring on reasoning alone was wrong whichever mechanism the reasoning \
               named, and that is the half worth keeping",
        evidence: "fourteen full `cargo test` suites in one session on `wt/toyos-logd`: 2 of the 9 \
                   with the window bounded and 1 of the 5 without; `main` (4d8c2e9) 0 of 7 and \
                   this branch 0 of 5 before the byte ring went, both recorded in the source below",
        source: "specs/issues/kernel/an-i8042-interrupt-arrives-with-no-byte-during-init.md",
        measured: "2026-08-15",
    },
    Red {
        test: "71_macro_empty_arg",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(4, 14),
        standing: Standing::Retired(
            "the capture path stopped attributing other processes' bytes to this program, \
             2026-08-15. Two causes, both measured rather than argued: a daemon's whole line \
             landing in the window, which `common::console::verdict` removes on the boot \
             config's own list of who may speak; and the one no rule over whole lines reaches \
             — this case is `printf(\"%d\", …)` with no newline, so its `17` reaches the wire \
             unterminated and the host's splitter appends whoever wrote next, giving \
             `17init: started test-runner` in one line, or `17===TEST_END …` and an empty \
             capture. After the fix: 10 targeted runs and 5 full suites, 0 reds, against 1 of \
             10 with only the first cause answered. `c_capture_ignores_daemon_lines` is the \
             gate and carries both captures verbatim",
        ),
        what: "`output mismatch`, expected `17` and the capture empty — the child's own line fell \
               outside the `===TEST_START===`/`===TEST_END===` window the C family compares whole. \
               Same shape and same test name the write-up records at `dbbdcbe`, which is before \
               this branch existed. **The log spec's §4.3 said bounding the console drain took \
               this to zero in five; fourteen suites here say roughly one in four whatever the \
               console lock does**, so that was a lucky five rather than a fix, and §4.3 is \
               corrected to say so. The console lock was never in it: what decided the rate was \
               whether the writer after this program was the kernel, whose `[kernel ` prefix the \
               capture already cut at",
        evidence: "the same fourteen suites as the row above: 3 of the 9 with the window bounded \
                   and 1 of the 5 without",
        source: "tests/common/console.rs",
        measured: "2026-08-15",
    },
    // ---------------------------------------------------------------------
    // The same session's landing gate, and a second measurement of one of the
    // names above rather than more of the first: ten full suites back to back
    // with no gap, where the fourteen were spaced. It is a different
    // instrument in everything but the label, and the rate says so.
    // ---------------------------------------------------------------------
    Red {
        test: "i8042_undecoded_bytes",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(6, 10),
        standing: Standing::Retired(
            "the same one word as the row above, 2026-08-17, and this rate is the one the nine \
             green suites were run against: back to back with no gap, on a host another \
             worktree's suite was taking guest slots from, and **two of the nine collapsed \
             machine-wide** — 160 and 172 reds on `Broken pipe` and `QEMU disconnected` — with \
             this name passing inside both. That is the load this row says the rate tracks, at \
             more of it than the row was measured under, with nothing to track",
        ),
        what: "the same line, and **the rate tracks host load** — 6 of 10 with the suites run back \
               to back and the load average never below 6.4, against the 3 of 14 above with the \
               host allowed to settle between them, on one tree in one session. The harness's \
               isolated re-run answered `ALONE: GREEN` on these, which is the class name for \
               exactly that. A bring-up race whose window is the driver's own polling init is what \
               a rate that moves with the host looks like; a defect in what this branch changed is \
               not. **Retired with the row above on 2026-08-16 and withdrawn with it on \
               2026-08-17** — a rate cannot be retired by a fix that does not reach its cause. \
               The withdrawal named the torn read as that cause and **that attribution was wrong** \
               (corrected 2026-08-17, PR #112): this is a rate of the entry-time increment, not of \
               the subtraction, for the reason the row above sets out — no AP exists to read \
               anything at the bring-up interrupt. The withdrawal was still right to be made, \
               which is the durable half of it",
        evidence: "ten consecutive full `cargo test` suites on `wt/toyos-logd`'s tip, loads \
                   6.4-9.7, immediately after the fourteen above",
        source: "specs/issues/kernel/an-i8042-interrupt-arrives-with-no-byte-during-init.md",
        measured: "2026-08-15",
    },
    Red {
        test: "boot_partition_identity",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::fires(1, 10),
        standing: Standing::Stands,
        what: "`\"panicked at\" during the boot` — and the panic is sshd's, not the kernel's: \
               `sshd: cannot bind 0.0.0.0:22: netd error`, on a boot where netd had already said \
               there is no NIC and exited 0. This test refuses any boot whose console carries a \
               panic, so its own subject is untouched and the red names the workload. \
               `ALONE: GREEN`",
        evidence: "the same ten consecutive suites as the row above, loads 6.4-9.7",
        source: "specs/issues/build/sshd-panics-when-netd-exits-before-it-binds.md",
        measured: "2026-08-15",
    },
    // ---------------------------------------------------------------------
    // `wt/toyos-ciwall`, dev host, 2026-08-15: the one-accumulator tree's full
    // suite (landed as `81cfe22`), 256 passed and 4 failed, with a second
    // worktree's suite holding guest slots beside it. Two of the four are the
    // same QEMU-exited-0 signature under two names, which is neither test's
    // subject — a shard's partitioning diff cannot reach a guest that never
    // booted.
    // ---------------------------------------------------------------------
    Red {
        test: "screen_fatal_halt",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`[qemu] QEMU died before ===READY=== (status: Ok(ExitStatus(unix_wait_status(0))))` \
               — QEMU exited *successfully* before the guest said anything, so the capture holds \
               nothing to bisect and the test's name is the whole of the evidence. \
               `ALONE: GREEN`, and green again in 3 s run by name",
        evidence: "one full `cargo test` on `wt/toyos-ciwall`, in its 106.4 s parallel phase, with \
                   `[host-slots]` naming `toyos-capwin`'s suite on the same host; the run's own \
                   width line was `fastest boot 1380 ms against the reference 1320 ms`, 1.05x, so \
                   this is not the slow-phase shape",
        source: "specs/issues/build/qemu-exits-clean-before-ready.md",
        measured: "2026-08-15",
    },
    Red {
        test: "double_fault_stack",
        instrument: Instrument::DevHostLoaded,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "the identical line, in the same phase as the row above — and the two names have \
               nothing in common but a boot. `ALONE: GREEN`, and green again in 2 s run by name",
        evidence: "the same full `cargo test` on `wt/toyos-ciwall`; the same day the signature also \
                   took `log_backing_read_error` on `wt/toyos-logd56` and, through the screendump \
                   wait rather than the ready marker, `screen_console_shell` on `wt/toyos-capwin`",
        source: "specs/issues/build/qemu-exits-clean-before-ready.md",
        measured: "2026-08-15",
    },
    // ---------------------------------------------------------------------
    // The nightly A/B of the one-accumulator fix: dispatches `31900045901`
    // (`main` at e064a96) and `31900050723` (the same tree plus the fix),
    // twelve KVM shards each, minutes apart on one runner pool. The trees
    // differ only in `src/testargs.rs`, `tests/toyos.rs` and a deleted issue
    // file, so nothing in either guest image moved — what the fix changes is
    // which shard a test lands in.
    // ---------------------------------------------------------------------
    Red {
        test: "screen_diag_boot",
        instrument: Instrument::Ci,
        finding: Finding::fires(2, 2),
        standing: Standing::Retired(
            "the string is the whole defect and it is now declared once: \
             `common::volumes::LOG_ON_CONSOLE_AND_FILE` is what the assertion reads, beside \
             `NO_LOG_ALERT`, which its Fast-tier sibling `screen_log_absent` already read that \
             way, and its doc names `report_log_destination` as the writer. \
             `specs/testing-strategy.md` §1.3 is the rule the copy broke (2026-08-16)",
        ),
        what: "`\"log: this boot is on the console and in\" is not on screen five seconds after the \
               boot finished`, and `red again` alone in both runs. The mode is not what is broken: \
               the screen printed beside the message carries \
               `log: this boot is on the console and on /log`, the wording `ecede44` gave the line \
               after `9ca7631` took the file off the kernel. A `Tier::Nightly` name, so no pull \
               request runs it, and the nightly's alarm job fires on `schedule` and not on the \
               dispatches these were",
        evidence: "runs 31900045901 (job 95049265216) and 31900050723 (job 95049280299), both \
                   `guest (12)`, read with `gh run view --job`",
        source: "tests/common/volumes.rs",
        measured: "2026-08-15",
    },
    Red {
        test: "boot_partition_identity",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`\"panicked at\" during the boot` — `sshd: cannot bind 0.0.0.0:22: netd error`, the \
               same panic the dev-host row above carries, on a KVM shard with one guest on the \
               machine. So the \"only above load average 6\" qualifier belongs to that session and \
               not to the race. `ALONE: GREEN, and it was alone both times — a rate and not a \
               classification`",
        evidence: "run 31900050723, job 95049280131 (`guest (3)`), `wt/toyos-ciwall`; green in the \
                   sibling dispatch 31900045901 minutes earlier on the same names and the same \
                   image",
        source: "specs/issues/build/sshd-panics-when-netd-exits-before-it-binds.md",
        measured: "2026-08-15",
    },
    Red {
        test: "usb_boot_stick_pulled",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`\"PANIC:\" after the boot stick was pulled` — and the panic is the kernel's: \
               `a task waits on at most one queue` (`toyos-sched/src/waitq.rs:124`) reached through \
               `Ticket::register` from `kernel::io_uring::enter`, in logd's `Poller::submit`, \
               4.7 s after the stick went and its writes started failing. Not the `sys_read` \
               keyboard-flood path already written up under that assertion. The machine did not \
               halt — the capture runs on to `pull-probe-91`. `ALONE: GREEN, and it was alone both \
               times — a rate and not a classification`",
        evidence: "run 31900050723, job 95049280131 (`guest (3)`), the serial phase; green in the \
                   sibling dispatch 31900045901 on the byte-identical kernel",
        source: "specs/issues/kernel/io-uring-enter-trips-the-one-queue-invariant.md",
        measured: "2026-08-15",
    },
    // ---------------------------------------------------------------------
    // This documentation branch's own pull-request run, adjudicated here
    // rather than re-run: the diff is prose, one caveat and this table, and
    // reaches nothing that boots.
    // ---------------------------------------------------------------------
    Red {
        test: "null_sink_client_exits",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Retired(
            "the number was never the race and it is unchanged: `settle_null_sink_client_exits` \
             (`tests/toyos.rs`) waits for both removals on the guest's own liveness, between the \
             test and its check, so the window no longer closes on the line soundd writes about \
             the exit that closed it. `expect: 2` stays exact — the departure vocabulary is \
             asserted per removal",
        ),
        what: "`soundd reported 1 client removals, expected 2` — and the capture shows the second \
               `soundd: client 0 removed (closed)` never arriving rather than arriving wrong: the \
               guest printed `null sink drained two clients in series` and exited, which is where \
               the window ends. Round 1's removal made it in because a whole second round followed \
               it. `ALONE: GREEN, and it was alone both times — a rate and not a classification`",
        evidence: "PR #85 run 31904338273, job 95059750268 (`guest (1)`), on a branch of \
                   documentation and this table",
        source: "tests/toyos.rs",
        measured: "2026-08-15",
    },
    // ---------------------------------------------------------------------
    // PR #94 (`wt/toyos-schedfuture`), run 31944633004, 2026-08-16: five
    // documentation files, two reds, both adjudicated here and fixed at their
    // owners rather than re-run.
    // ---------------------------------------------------------------------
    Red {
        test: "null_sink_client_exits",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Retired(
            "`settle_null_sink_client_exits` (`tests/toyos.rs`), landed with this row",
        ),
        what: "`soundd reported 1 client removals, expected 2` again, and **`ALONE: red again — \
               the defect is real`** where PR #85's occurrence went green: one guest on a KVM \
               runner with nothing to contend with \
               reproduces it, so the wide phase was never what produced it. Both captures carry \
               one removal and one `clients=0` — soundd flushes the window in the same mix-loop \
               iteration that prints the removal, so the `clients=` statistic the write-up offered \
               as the other way to buy non-vacuity is on the far side of the same close",
        evidence: "PR #94 run 31944633004, job 95158684501 (`guest (1)`), `wakes=484` in the wide \
                   run and `wakes=481` in the isolated re-run",
        source: "tests/toyos.rs",
        measured: "2026-08-16",
    },
    Red {
        test: "i8042_undecoded_bytes",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`the line names no byte: [kernel 2.494 cpu1] i8042: 1 interrupts and 4 bytes, \
               nothing decoded — first seen at 2494ms`. **Four bytes and not zero, so this is a \
               different producer from the two dev-host rows under this name**: it is the test's \
               own Pause, reported \
               after the first interrupt delivered four of its six bytes, with the decoder's run \
               still open and `Unexplained` therefore empty. Neither half that landed for those \
               rows reaches it — the line is after the injection and the interrupt carried bytes. \
               `ALONE: GREEN, and it was alone both times — a rate and not a classification`",
        evidence: "PR #94 run 31944633004, job 95158684534 (`guest (2)`); the isolated re-run in \
                   the same job reported the whole sequence, `2 interrupts and 6 bytes … no event \
                   from [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]`",
        source: "specs/issues/kernel/the-i8042-mute-verdict-cannot-revise-a-line-it-said-too-early.md",
        measured: "2026-08-16",
    },
    Red {
        test: "screen_console_shell",
        instrument: Instrument::Ci,
        finding: Finding::Seen,
        standing: Standing::Stands,
        what: "`no \\`i8042:\\` line above the prompt: \\`/boot/toyos/kernel.log\\` never reached the \
               scrollback` — **and the panel it printed disproves that sentence**: every line on \
               it is stamped `0.000` and comes from the first screenful of the boot, so the seed \
               reached the console and the view was at its *head*. The assertion wants the end of \
               the seed and `screendump_while` stops at the first frame carrying the prompt, so \
               nothing orders the seed's paint against it. `ALONE: GREEN, and it was alone both \
               times — a rate and not a classification`. **Not about the diff it was found on**, \
               which is the i8042 interrupt tally: that change writes no boot line and removes \
               none, so the set of `i8042:` lines this test looks for is identical either side of \
               it",
        evidence: "PR #111 run 32040411208, job 95418635461 (`guest (3)`); the isolated re-run in \
                   the same job was green",
        source: "specs/issues/diagnostics/console-scrollback-can-sit-at-the-head-of-the-seeded-log.md",
        measured: "2026-08-17",
    },
];

// ---------------------------------------------------------------------------
// The query.
// ---------------------------------------------------------------------------

/// `cargo run -- --known-red [<test>]`.
pub fn dispatch(root: &Path, args: &[String]) {
    let asked = args
        .iter()
        .position(|a| a == "--known-red")
        .and_then(|at| args.get(at + 1))
        .filter(|a| !a.starts_with("--"));
    let registry = Registry::read(root);
    print!("{}", answer(KNOWN_RED, &registry, Day::today(), asked.map(String::as_str)));
}

/// The whole answer, as text, so that the shape of it is a value a test can
/// assert on rather than something only a human ever sees.
fn answer(rows: &[Red], registry: &Registry, today: Day, asked: Option<&str>) -> String {
    match asked {
        Some(test) => one(rows, registry, today, test),
        None => everything(rows, today),
    }
}

/// What the index says about one name, and it is a sentence before it is a list.
#[derive(PartialEq, Eq, Debug)]
enum Verdict {
    /// A measurement says it reds and nothing has retired that measurement.
    KnownRed,
    /// No live red, but the sources disagree about whether one was retired.
    Disputed,
    /// Rows exist and none of them is a live red.
    NotKnownRed,
    /// No rows. **Not** a claim that the test is green.
    NotOnTheList,
}

fn verdict_for(mine: &[&Red]) -> Verdict {
    if mine.is_empty() {
        return Verdict::NotOnTheList;
    }
    if mine.iter().any(|r| r.finding.is_red() && r.standing == Standing::Stands) {
        return Verdict::KnownRed;
    }
    if mine.iter().any(|r| matches!(r.standing, Standing::Disputed(_))) {
        return Verdict::Disputed;
    }
    Verdict::NotKnownRed
}

fn headline(v: &Verdict) -> &'static str {
    match v {
        Verdict::KnownRed => "KNOWN-RED",
        Verdict::Disputed => "DISPUTED",
        Verdict::NotKnownRed => "NOT KNOWN-RED",
        Verdict::NotOnTheList => "NOT ON THE LIST",
    }
}

fn rows_for<'a>(rows: &'a [Red], test: &str) -> Vec<&'a Red> {
    let mut mine: Vec<&Red> = rows.iter().filter(|r| r.test == test).collect();
    // Newest first: what was measured last is what a reader wants at the top,
    // and the day is printed beside every row so the order is checkable.
    mine.sort_by_key(|r| std::cmp::Reverse(Day::parse(r.measured)));
    mine
}

fn one(rows: &[Red], registry: &Registry, today: Day, test: &str) -> String {
    let mine = rows_for(rows, test);
    let verdict = verdict_for(&mine);
    let mut out = format!("{test}: {}\n", headline(&verdict));

    if verdict == Verdict::NotOnTheList {
        out += if registry.tests.contains(test) {
            "\n  No measurement in this index has ever named it. That is not a claim that it is\n  \
             green — it is a claim that nobody wrote down a rate for it.\n"
        } else {
            "\n  No test of that name is registered either, so this is a typo or a renamed test.\n  \
             `cargo test -- --list` is the registry.\n"
        };
        return out;
    }

    if verdict == Verdict::KnownRed {
        out += "\n  At least one measurement says it reds and nothing has retired that\n  \
                measurement. Read which instrument each row is about before acting on it.\n";
    }

    let mut instruments: BTreeSet<Instrument> = BTreeSet::new();
    for r in &mine {
        instruments.insert(r.instrument);
        let standing = match r.standing {
            Standing::Stands => String::new(),
            Standing::Retired(why) => wrapped("RETIRED   ", why),
            Standing::Disputed(how) => wrapped("DISPUTED  ", how),
        };
        out += &format!(
            "\n  {:<13}  {:<16}  {}, {}{}\n{}{standing}{}{}",
            r.finding.rendered(),
            r.instrument.label(),
            r.measured,
            age(r, today),
            expiry_note(r, today),
            wrapped("", r.what),
            wrapped("evidence  ", r.evidence),
            wrapped("write-up  ", r.source),
        );
    }

    out += "\n  What each instrument cannot say:\n";
    for i in instruments {
        out += &wrapped(&format!("{:<16}  ", i.label()), i.cannot_say());
    }

    if registry.exempted.contains(test) {
        out += "\n  ALSO DECLARED in EXPECTED_FAILURES (`tests/toyos.rs`), which is a different\n  \
                mechanism: a named red with a task and a write-up that makes the run exit 0, for\n  \
                one quoted assertion only. Nothing in this index exempts anything.\n";
    }
    out
}

/// How long ago the measurement was taken, which is half of what a row is worth.
fn age(r: &Red, today: Day) -> String {
    match Day::parse(r.measured).map(|d| d.until(today)) {
        None => "and that date does not parse".to_string(),
        Some(0) => "today".to_string(),
        Some(1) => "1 day ago".to_string(),
        Some(n) => format!("{n} days ago"),
    }
}

/// One field of a row, indented under it and folded so that a paragraph is
/// readable in a terminal. The whole point of the file is that somebody reads
/// the answer instead of grepping the prose.
fn wrapped(field: &str, text: &str) -> String {
    const INDENT: &str = "      ";
    const WIDTH: usize = 86;
    let mut out = String::from(INDENT) + field;
    let mut column = INDENT.len() + field.len();
    let hang = " ".repeat(column);
    for (n, word) in text.split_whitespace().enumerate() {
        if n > 0 && column + 1 + word.len() > WIDTH {
            out += "\n";
            out += &hang;
            column = hang.len();
        } else if n > 0 {
            out += " ";
            column += 1;
        }
        out += word;
        column += word.len();
    }
    out + "\n"
}

fn expiry_note(r: &Red, today: Day) -> String {
    if r.standing != Standing::Stands {
        return String::new();
    }
    let Some(due) = Day::parse(r.measured).map(|d| d.plus_days(SHELF_LIFE_DAYS)) else {
        return String::new();
    };
    let left = today.until(due);
    if left <= 0 {
        "  ** EXPIRED: nobody has measured this since **".to_string()
    } else if left <= 7 {
        format!("  ** expires in {left} days **")
    } else {
        String::new()
    }
}

fn everything(rows: &[Red], today: Day) -> String {
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for r in rows {
        names.insert(r.test);
    }
    let mut out = format!(
        "{} measurements of {} tests. `--known-red <test>` for the rows.\n\n",
        rows.len(),
        names.len()
    );
    for test in &names {
        let mine = rows_for(rows, test);
        let v = verdict_for(&mine);
        let newest = mine.first().map_or("", |r| r.measured);
        let live: Vec<String> = mine
            .iter()
            .filter(|r| r.standing == Standing::Stands && r.finding.is_red())
            .map(|r| format!("{} on {}", r.finding.rendered(), r.instrument.label()))
            .collect();
        let line =
            format!("  {:<30}  {:<16}  newest {newest}  {}", test, headline(&v), live.join("; "));
        out += line.trim_end();
        out += "\n";
    }
    let oldest = rows
        .iter()
        .filter(|r| r.standing == Standing::Stands)
        .filter_map(|r| Day::parse(r.measured).map(|d| (d.until(today), r.measured)))
        .max();
    if let Some((age, day)) = oldest {
        out += &format!(
            "\nThe oldest standing measurement is {day}, {age} days old; a standing row expires \
             {SHELF_LIFE_DAYS} days after it was taken.\n"
        );
    }
    out
}

// ---------------------------------------------------------------------------
// What the tree itself says, which is what the rows are checked against.
// ---------------------------------------------------------------------------

/// Every name the suite can produce a verdict for, and the names
/// `EXPECTED_FAILURES` declares — read out of the harness rather than restated,
/// because a restatement is the thing that drifts.
pub struct Registry {
    pub tests: BTreeSet<String>,
    pub exempted: BTreeSet<String>,
}

impl Registry {
    pub fn read(root: &Path) -> Registry {
        let harness = std::fs::read_to_string(root.join("tests/toyos.rs"))
            .expect("tests/toyos.rs is what registers a test name");
        let mut tests = BTreeSet::new();
        // `MACHINE_TESTS` and `SCREEN_TESTS`: `("name", Sched::…)`, a shape
        // nothing else in that file has.
        for (at, _) in harness.match_indices("\", Sched::") {
            let head = &harness[..at];
            if let Some(open) = head.rfind("(\"") {
                let name = &head[open + 2..];
                if !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                    tests.insert(name.to_string());
                }
            }
        }
        // `AUDIO_TESTS`, whose tuples carry the same explicit Tier as the
        // machine and screen registries. Read the value block rather than a
        // line: formatting it across lines must not make known-red rows stale.
        if let Some(at) = harness.find("const AUDIO_TESTS:") {
            let block = &harness[at..];
            let end = block.find("];").unwrap_or(block.len());
            for tuple in block[..end].split("(\"").skip(1) {
                if let Some(name) = tuple.split('"').next() {
                    tests.insert(name.to_string());
                }
            }
        }
        // The shared boot's binaries are discovered from what is built, and the
        // sources are what they are built from.
        for (dir, ext) in
            [("tests/toyos-rust-tests/src/bin", "rs"), ("tests/testcases/tinycc", "c")]
        {
            let path = root.join(dir);
            for entry in std::fs::read_dir(&path)
                .unwrap_or_else(|e| panic!("{} is where a guest test comes from: {e}", path.display()))
                .flatten()
            {
                let p = entry.path();
                if p.extension().is_some_and(|e| e == ext) {
                    tests.insert(p.file_stem().unwrap().to_string_lossy().into_owned());
                }
            }
        }

        let mut exempted = BTreeSet::new();
        if let Some(at) = harness.find("const EXPECTED_FAILURES: &[ExpectedFailure] = &[") {
            let block = &harness[at..];
            let end = block.find("\n}];").map_or(block.len(), |e| e + 4);
            for piece in block[..end].split("test: \"").skip(1) {
                if let Some(name) = piece.split('"').next() {
                    exempted.insert(name.to_string());
                }
            }
        }
        Registry { tests, exempted }
    }
}

/// Everything a row has to be able to say about itself, against the tree.
///
/// A function over a slice rather than over [`KNOWN_RED`], because a
/// well-formed index cannot exercise a rejection and a gate nobody has watched
/// refuse anything is a gate nobody has watched.
#[cfg(test)]
fn refusals(rows: &[Red], registry: &Registry, root: &Path, today: Day) -> Vec<String> {
    let mut bad = Vec::new();
    let mut seen: BTreeSet<(&str, Instrument, &str)> = BTreeSet::new();

    for r in rows {
        let at = format!("{} ({})", r.test, r.evidence);

        if !registry.tests.contains(r.test) {
            bad.push(format!(
                "{at}: no list registers `{}` — a renamed or deleted test takes its rows with it, \
                 or the index is answering about whatever gets that name next",
                r.test
            ));
        }
        for (field, value) in
            [("what", r.what), ("evidence", r.evidence), ("source", r.source), ("measured", r.measured)]
        {
            if value.trim().is_empty() {
                bad.push(format!("{at}: `{field}` is empty, and there is no default"));
            }
        }
        if let Standing::Retired(why) | Standing::Disputed(why) = r.standing {
            if why.trim().is_empty() {
                bad.push(format!("{at}: a row that is not standing has to say what did that"));
            }
        }
        if let Finding::Fires { red, of } = r.finding {
            // `Finding::fires` refuses these at compile time; a hand-built
            // `Finding::Fires { .. }` is what this catches.
            if red == 0 || of < 2 || red > of {
                bad.push(format!("{at}: {red} of {of} is not a rate"));
            }
        }
        if let Finding::Quiet { of } = r.finding {
            if of < 2 {
                bad.push(format!("{at}: 0 of {of} is one sample and retires nothing"));
            }
        }

        let path = r.source.split_whitespace().next().unwrap_or("");
        let full = root.join(path);
        match std::fs::read_to_string(&full) {
            Err(_) => bad.push(format!(
                "{at}: its write-up `{path}` does not resolve, and an evidence pointer that misses \
                 reads as checked"
            )),
            Ok(text) => {
                if !text.contains(r.test) {
                    bad.push(format!(
                        "{at}: `{path}` never names `{}`, so the row and the prose behind it have \
                         drifted apart",
                        r.test
                    ));
                }
            }
        }

        match Day::parse(r.measured) {
            None => bad.push(format!(
                "{at}: `measured: {}` is not a YYYY-MM-DD date, so the row would never expire",
                r.measured
            )),
            Some(day) => {
                if day > today {
                    bad.push(format!(
                        "{at}: `measured: {}` is in the future, which is a fuse set forward",
                        r.measured
                    ));
                } else if r.standing == Standing::Stands
                    && today >= day.plus_days(SHELF_LIFE_DAYS)
                {
                    bad.push(format!(
                        "{at}: measured {}, more than {SHELF_LIFE_DAYS} days ago, and still \
                         standing. It says nothing about whether the defect is there — it says \
                         nobody has measured since. Re-take it, retire it with what retired it, \
                         or **delete it**: a rate nobody will re-measure is not something anyone \
                         should be trusting",
                        r.measured
                    ));
                }
            }
        }

        if !seen.insert((r.test, r.instrument, r.evidence)) {
            bad.push(format!(
                "{at}: the same test, instrument and evidence twice — one measurement is one row"
            ));
        }
    }
    bad
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn registry() -> Registry {
        Registry::read(&repo_root())
    }

    /// The registry is what every other gate here is measured against, so a
    /// parse that quietly found nothing would wave the whole index through.
    #[test]
    fn the_registry_is_read_out_of_the_harness_and_is_not_empty() {
        let r = registry();
        for name in [
            "desktop_window_child",
            "hda_tone",
            "screen_pager_keys",
            "audio_tone",
            "audio_tone_load",
        ] {
            assert!(r.tests.contains(name), "the test-name scan missed `{name}`");
        }
        for name in ["std_unwind", "fpu_isolation"] {
            assert!(r.tests.contains(name), "the guest-binary scan missed `{name}`");
        }
        assert!(r.tests.len() > 100, "only {} test names found", r.tests.len());
        // CLAUDE.md: two expected failures stand, and neither may be
        // reclassified or deleted. This index is not allowed to be the reason
        // nobody notices one going.
        for name in ["desktop_window_child", "hda_tone"] {
            assert!(
                r.exempted.contains(name),
                "`{name}` is no longer in EXPECTED_FAILURES; the root CLAUDE.md says both entries \
                 stand, so either that rule changed or this parse did"
            );
        }
    }

    #[test]
    fn every_row_can_say_what_it_claims() {
        let bad = refusals(KNOWN_RED, &registry(), &repo_root(), Day::today());
        assert!(
            bad.is_empty(),
            "the known-red index is what `--known-red` answers from:\n  {}",
            bad.join("\n  ")
        );
    }

    /// Printed on the way past, because what a reader wants before trusting a
    /// row is how old it is, and the only honest place for that is a run.
    #[test]
    fn the_index_prints_what_it_is_carrying() {
        let today = Day::today();
        let standing = KNOWN_RED.iter().filter(|r| r.standing == Standing::Stands).count();
        let live = KNOWN_RED
            .iter()
            .filter(|r| r.standing == Standing::Stands && r.finding.is_red())
            .count();
        let due_soon = KNOWN_RED
            .iter()
            .filter(|r| r.standing == Standing::Stands)
            .filter(|r| {
                Day::parse(r.measured)
                    .is_some_and(|d| today.until(d.plus_days(SHELF_LIFE_DAYS)) <= 7)
            })
            .count();
        println!(
            "known-red index: {} rows, {standing} standing, {live} live reds, {due_soon} expiring \
             within 7 days",
            KNOWN_RED.len()
        );
    }

    /// The four things the gate exists to refuse, run rather than argued. The
    /// rate rules are absent because they are compile errors:
    /// `Finding::fires(0, 5)` does not build.
    #[test]
    fn the_gate_refuses_what_it_is_written_against() {
        let root = repo_root();
        let today = Day::parse("2026-08-11").unwrap();
        let reg = Registry { tests: ["a_real_test".into()].into(), exempted: BTreeSet::new() };
        let ok = Red {
            test: "a_real_test",
            instrument: Instrument::Ci,
            finding: Finding::fires(1, 5),
            standing: Standing::Stands,
            what: "x",
            evidence: "run 1",
            source: "src/redlist.rs",
            measured: "2026-08-10",
        };
        assert!(refusals(&[ok], &reg, &root, today).is_empty(), "a well-formed row is not refused");

        let cases: [(&str, Red, &str); 6] = [
            (
                "a name no list registers",
                Red { test: "gone_away", ..ok },
                "no list registers",
            ),
            (
                "a write-up that does not resolve",
                Red { source: "specs/nowhere-at-all.md", ..ok },
                "does not resolve",
            ),
            (
                "a write-up that has stopped being about this test",
                Red { source: "Cargo.toml", ..ok },
                "never names",
            ),
            (
                "a date nothing can read",
                Red { measured: "last Tuesday", ..ok },
                "never expire",
            ),
            (
                "a measurement older than its shelf life, still standing",
                Red { measured: "2026-01-01", ..ok },
                "nobody has measured since",
            ),
            (
                "a hand-built rate the constructor would have refused",
                Red { finding: Finding::Fires { red: 0, of: 5 }, ..ok },
                "is not a rate",
            ),
        ];
        for (what, row, says) in cases {
            let bad = refusals(&[row], &reg, &root, today);
            assert!(
                bad.iter().any(|b| b.contains(says)),
                "{what}: expected a refusal naming {says:?}, got {bad:?}"
            );
        }

        // An expired row that has been *retired* is history and does not red:
        // only a standing claim has a shelf life.
        let old_and_retired = Red {
            measured: "2026-01-01",
            standing: Standing::Retired("something landed"),
            ..ok
        };
        assert!(refusals(&[old_and_retired], &reg, &root, today).is_empty());
    }

    /// The distinction the owner got wrong, asked of the answer rather than of
    /// the data: a name measured and found quiet may not read as a red, and a
    /// name nothing has measured may not read as either.
    #[test]
    fn a_zero_never_reads_as_a_red() {
        let root = repo_root();
        let today = Day::parse("2026-08-11").unwrap();
        let reg = Registry {
            tests: ["came_off".into(), "still_reds".into(), "unmeasured".into()].into(),
            exempted: BTreeSet::new(),
        };
        let base = Red {
            test: "came_off",
            instrument: Instrument::Ci,
            finding: Finding::quiet(5),
            standing: Standing::Stands,
            what: "0 of 5 in the probe",
            evidence: "run 1",
            source: "src/redlist.rs",
            measured: "2026-08-10",
        };
        let rows = [
            Red { ..base },
            Red { test: "still_reds", finding: Finding::fires(2, 5), what: "2 of 5", ..base },
        ];
        assert!(refusals(&rows, &reg, &root, today).is_empty());

        let came_off = answer(&rows, &reg, today, Some("came_off"));
        assert!(came_off.starts_with("came_off: NOT KNOWN-RED"), "{came_off}");
        assert!(came_off.contains("QUIET 0 of 5"), "{came_off}");
        assert!(!came_off.contains(": KNOWN-RED"), "{came_off}");

        let reds = answer(&rows, &reg, today, Some("still_reds"));
        assert!(reds.starts_with("still_reds: KNOWN-RED"), "{reds}");

        let never = answer(&rows, &reg, today, Some("unmeasured"));
        assert!(never.starts_with("unmeasured: NOT ON THE LIST"), "{never}");
        assert!(never.contains("not a claim that it is\n  green"), "{never}");

        let typo = answer(&rows, &reg, today, Some("no_such_test"));
        assert!(typo.contains("No test of that name is registered"), "{typo}");
    }

    /// A retired measurement is history and must not silence the live one
    /// beside it, nor be counted as one.
    #[test]
    fn retired_and_disputed_rows_do_not_decide_a_name_by_themselves() {
        let today = Day::parse("2026-08-11").unwrap();
        let reg = Registry {
            tests: ["t".into(), "d".into()].into(),
            exempted: BTreeSet::new(),
        };
        let base = Red {
            test: "t",
            instrument: Instrument::Ci,
            finding: Finding::fires(5, 5),
            standing: Standing::Retired("a fix landed"),
            what: "x",
            evidence: "run 1",
            source: "src/redlist.rs",
            measured: "2026-08-10",
        };
        let only_retired = [Red { ..base }];
        assert!(answer(&only_retired, &reg, today, Some("t")).starts_with("t: NOT KNOWN-RED"));

        let retired_and_live =
            [Red { ..base }, Red { evidence: "run 2", standing: Standing::Stands, ..base }];
        assert!(answer(&retired_and_live, &reg, today, Some("t")).starts_with("t: KNOWN-RED"));

        let disputed = [Red { test: "d", standing: Standing::Disputed("two sources"), ..base }];
        assert!(answer(&disputed, &reg, today, Some("d")).starts_with("d: DISPUTED"));
    }

    /// The one property `--known-red` has that prose does not: the answer says
    /// which machine it is about, and what that machine cannot be asked.
    #[test]
    fn every_answer_names_its_instrument_and_what_that_instrument_cannot_say() {
        let out = answer(KNOWN_RED, &registry(), Day::today(), Some("screen_pager_keys"));
        // Folded for a terminal, so the assertion is against the words and not
        // against where the fold happened to land.
        let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("QUIET 0 of 5 CI"), "{out}");
        assert!(flat.contains("FIRES 3 of 3 dev host, alone"), "{out}");
        assert!(flat.contains("nothing here is about contention"), "{out}");
        assert!(flat.contains("which vendor's reading of an instruction"), "{out}");
    }

    /// `hda_tone` is the one name both mechanisms carry, at two different
    /// assertions, and the answer has to say so or somebody will read the
    /// exemption as covering the row.
    #[test]
    fn a_name_that_is_also_exempted_says_so() {
        let out = answer(KNOWN_RED, &registry(), Day::today(), Some("hda_tone"));
        assert!(out.contains("ALSO DECLARED in EXPECTED_FAILURES"), "{out}");
        assert!(out.contains("Nothing in this index exempts anything"), "{out}");
    }
}
