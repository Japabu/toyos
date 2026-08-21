---
status: open
kind: defect
opened: 2026-08-20
---

# `audio_idle_suspend` reds on a loaded host, on `main` as much as anywhere

`tests/toyos-rust-tests/src/bin/audio_idle_suspend.rs` asserts §5.8's strongest
claim: on a boot where no client ever connects, soundd's summed `cpu_ns` across
both its threads is **exactly** unchanged over ~1 s. It reds routinely on the
dev host, with a delta of one to three milliseconds.

It is **not** anybody's diff. Same-session A/B, 2026-08-20, one interleaved
session on the dev host, `cargo test --test toyos-build -- --nightly
audio_idle_suspend`, magnitudes in nanoseconds of the reported delta:

| tree | n | min | median | mean | max |
|---|---|---|---|---|---|
| `625afce1` (`main`) | 9 | 1,457,204 | 1,942,387 | 1,939,260 | 2,240,925 |
| `cf72c3dc` (`toyos-mixer` extraction) | 14 | 1,227,121 | 1,856,260 | 1,782,107 | 2,596,139 |

One population, and the branch's is if anything the lower of the two. The
extraction that prompted this measurement did not touch the idle path.

**The harness already names a diagnosis, twice.** In two of five fast-tier runs
the re-run alone went green and the suite reported:

    ALONE audio_idle_suspend: GREEN — it fails only beside other guests, so its
    Sched::Parallel is wrong. The run stays red on the classification.

That is the shape of the whole thing: the delta is **exactly zero** when the
guest runs with the host to itself, and one to three milliseconds when it does
not. A soundd that genuinely spun would never produce an exact zero.

It is also not selection-dependent, which was the first hypothesis and is wrong:
five consecutive fast-tier runs of the single test red 5/5, while the *full*
272-test fast run on the same commit passed it. Fewer guests is not quieter
here — a one-test run and a full suite differ in more than load.

**Two candidate causes, and the experiment that separates them.**

1. soundd takes real wakes while suspended that are cheap enough to round to
   zero on a quiet host. The device path watches the audio handle `READABLE`
   with `timeout = u64::MAX`; if that handle is ever readable with the stream
   stopped, the mix loop turns over.
2. The guest's `cpu_ns` accounting charges a blocked thread under contention —
   the delta is an artifact of when the scheduler samples, not of work done.

They separate on **whether soundd's own wake counter moves**: `MixStats::wakes`
is zeroed when the first client arrives and never reported while idle, so
nothing today can see an idle wake. A `SYS_DEBUG` counter, or a single line
soundd prints when it leaves the poller with no streams, decides it in one boot.
Until then the fix is unknown and the test is measuring two things at once.

Not on `src/redlist.rs` — `cargo run -- --known-red audio_idle_suspend` answers
`NOT ON THE LIST`. Adjudicating it there needs the rate above and a decision
about whether the row records a soundd defect or a `Sched::Parallel`
misclassification, and those are not the same row.

Gate A is unaffected and green throughout: `audio_tone` and `audio_tone_load` at
smp=1 and smp=8 all pass on `cf72c3dc` with 440.0 Hz, phase-breaks 0, gaps none,
0 underruns and 0 drains.

## 2026-08-21: it also fails by *position in the session*, which is what makes a two-arm reading of it worthless below n≈10

Confirmed again on the dev host against `fe41dbae` (`main`), 42 invocations in
one afternoon across three interleaved A/Bs, `cargo test --test toyos-build --
--nightly audio_idle_suspend`. The magnitudes are one population on both arms —
every reported delta in 1,231,172-2,561,459 ns, the band this file already
records.

The **rate** moves with where in a session an invocation falls, not with the
arm. Two A/Bs that held the within-round order fixed gave one arm the early
slots and produced 6 reds of 9 against 2 of 9; a third — twelve rounds with the
order alternating, so neither arm keeps the early slots — produced **4 of 12
against 3 of 12**, no difference at all. In all three the reds cluster in the
first four invocations of the session whichever arm holds them, which is the
same quiet-host dependence as the `ALONE:` line above rather than a property of
any diff.

Two consequences. A rate difference in this test at n<10 per arm is not evidence
about a diff, and an A/B of it must alternate the within-round order or it
measures session position instead. And a third failure mode belongs on the
record: one `main` invocation of the 42 failed differently — `expected soundd's
mix and control threads in sysinfo, found 1` — so this test also races soundd's
control thread into `sysinfo`, and a red carrying that sentence is not this
issue at all.
