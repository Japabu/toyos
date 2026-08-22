---
status: open
kind: defect
opened: 2026-08-21
---

# `tests/test-durations` is measured on one instrument and enforced on another

`src/tiers.rs`'s `FAST_CEILING_MS` reds any `Tier::Fast` name a run measures
over 10,000 ms. The profile it is judged against was measured by **twelve
GitHub-hosted shards**; the run that does the judging is now **one lane on the
T14**. The two do not price the same tests alike, so the gate reds on `main`'s
own tip, on a different set of names every run.

## What reds

`xhci_full_speed_device` is `Tier::Fast`, committed at `6900` ms.

| run | tree | where | shape | ms | names over the ceiling |
|---|---|---|---|---|---|
| 32444411794 (nightly, 03:43Z) | `da98b18b` | hosted, EPYC 7763/9V74, 4 cores | `--shard 6/12` | 6,900 | — (this run *is* the profile) |
| 32505371471 (merge queue, 16:54Z) | `13953023` | hosted, same | `--shard i/12` | **6,845** | **0** — green |
| 32498159547 (push, 15:32Z) | `07f89c8b` | T14, i5-1135G7, 8 cores | `--shard 1/1` | **11,076** | 9 |
| 32506479551 (push, 17:07Z) | `13953023` | T14, same | `--shard 1/1` | **12,156** | 5 |
| 32513441183 (PR #199, 18:27Z) | `c670ea27` | T14, same | `--shard 1/1` | **10,166** | 1 |
| 32524769419 (PR #201, 20:40Z) | `84bf0861` | T14, same | `--shard 1/1` | **9,052** | 1 — `i8042_health` at 15,122 |

**The last row is the whole argument.** PR #201 is this file and one
`src/redlist.rs` data row — a Markdown document and a `const` array entry,
nothing a kernel compiles. On it `xhci_full_speed_device` came in under the
ceiling, and the gate reded anyway: `i8042_health` at 15,122 ms against a
committed 9,509. Four consecutive lanes, four different casts over the line, on
trees whose differences cannot reach a boot.

The cast over the line rotates run to run — `dump_nmi_probe`,
`esp_filesystem`, `i8042_health`, `log_conservation_smp4`,
`log_partition_identity`, `sched_check_build`, `screen_console_clear`,
`screen_console_panic`, `xhci_full_speed_device`, `xhci_superspeed_ports`,
`xhci_two_controllers` — and the whole lane's measured test time with it: the
same 1/1 shape measured 429.2 s, 483.6 s and 548.8 s of tests on three
consecutive runs. The names that cross are simply the ones the profile already
prices nearest the line; the lane runs longest-first, so they are also the ones
that happen to sit at positions 174–202 of 275.

## What changed, and it is not the tree

`985f3834` ("Route trusted Linux CI to the T14 runner", #187, 10:50Z) made
`.github/workflows/route.yml` the one place that decides where a Linux job
runs. A trusted event — a push to `main`, a same-repository pull request, the
nightly — now takes `runner=toyos` with `matrix.shard: [1]` and
`SHARD_COUNT: 1`. Only a fork's pull request and a **merge-queue** ref stay on
`ubuntu-24.04` with the twelve-way matrix. `route.yml` did not exist at
`da98b18b`, which is why the 03:43Z nightly that recorded this profile ran
twelve hosted shards.

So the 6,900 ms and the 10,166 ms were taken on different silicon in different
partitions. Three independent lines say the tree between them is innocent:

1. **The same tree in the baseline's own shape.** Merge-queue run
   `32505371471` has `headSha` `13953023` — `main`'s tip, carrying every
   landing in the window — and ran twelve hosted shards. It measured
   `xhci_full_speed_device` at **6,845 ms**, 0.8% *under* the commitment, put
   **no** Fast name over the ceiling, and was green. Over the 84 names the
   profile prices at ≥ 1 s, that run's ratio to the committed profile has
   median 1.00.
2. **The window, sampled nine times in that shape.** The merge-queue runs from
   11:24Z to 16:54Z measured 8021, 8047, 8084, 7248, 7141, 7198, 7073, 7160 and
   6845 ms. The series trends *down*, and never approaches 10,000.
3. **The code.** `git diff da98b18b..13953023 -- kernel/src/drivers/
   toyos-xhci/ bootloader/` is empty: the xHCI driver and the whole device
   layer are byte-identical across the window. The only shipping-kernel changes
   are `kernel/src/object/handle.rs` (#171), `kernel/src/arch/syscall.rs`
   (#172's `POWER` demand on `SYS_SHUTDOWN`, and #171's debug action), and
   #192/#195's tripwires — every one of the latter behind
   `#[cfg(feature = "heap-tripwire")]` or `#[cfg(feature = "heap-sweep")]`, and
   neither feature is in `src/build.rs`'s `TEST_SUITE_KERNEL_BUILDS`
   (`["", "boot-actuators,test-actuators", "fpu-save-nothing", "sched-check"]`).
   What is left un-`cfg`'d is two `if let Some(..)` blocks in
   `hw::report_contexts` — the kernel crash report, not a boot path — whose
   callees are `#[cfg(not(..))] -> None`, plus the no-op
   `tripwire::{outer,arm,disarm}` shims in `GlobalAlloc::{alloc,dealloc}`.

## The T14, both trees, interleaved

The test alone, `--jobs 1 --host-slots 0`, in the CI image by digest on an idle
T14, twenty reps per arm taken as four interleaved blocks of five. No block was
discarded and no CI job container appeared during any of them.

| arm | tree | n | min | p25 | median | p75 | max | mean | sd |
|---|---|---|---|---|---|---|---|---|---|
| MAIN | `13953023` | 20 | 8,858 | 9,217 | **9,305** | 9,607 | 10,756 | 9,425 | 426 |
| NIGHT | `da98b18b` | 20 | 8,810 | 9,207 | **9,431** | 9,783 | 10,156 | 9,467 | 350 |

Median difference −126 ms (−1.3%), Mann-Whitney U = 170 against µ = 200,
z = −0.81, two-sided **p = 0.42**. By the gate's own statistic the two arms are
identical: each put 2 of 20 reps over the 10,000 ms ceiling.

**The tree that recorded 6,900 ms costs 9,431 ms on the T14.** Both arms are
1.35–1.37× the committed price, alone and uncontended — so roughly a third of
the gap is the machine, and the rest is the lane: inside the 1/1 partition the
same test measured 10,166–12,156 ms in the three CI runs above.

The old tree was **not** measured inside a 1/1 lane. That arm was started and
abandoned: one lane rep is ~8 minutes, the T14 has one worker, and CI wanted
the machine — a measuring container that holds the lane against a `guest` job
is the stuck slot, so it yielded. Nothing here rests on it. The alone arm is
the controlled tree-versus-tree comparison, and the lane multiplier is a
property of the partition rather than of a tree.

## The ruling: one profile, one instrument

**The orchestrator chose the second of the three options below, 2026-08-21: the
profile has one instrument, so only that instrument renders the tier verdict.**
`.github/workflows/ci.yml`'s `durations` job now reads
`needs.route.outputs.trusted` — the same output `guest` keys its `runs-on`,
matrix and shard count on, and never `runner.name`, since `durations` is
GitHub-hosted on every event and its own runner says nothing about where the
measurement was taken. On a T14 lane (`trusted == 'yes'`) it still merges, still
prints the critical-path line and the unpriced/unrun names into the step
summary, and still uploads `test-durations-merged`; the price verdict is printed
as a `::warning::` naming the reason and the job exits 0. On a merge-queue ref
or a fork's pull request — hosted, the shape the profile was taken in — nothing
changes and the verdict fails exactly as before. The queue is required before
`main` moves, so the verdict is still rendered on every landing.

The three options as they stood, for the record:

* re-measure the profile on the instrument that now enforces it, and accept
  that a 1/1 lane whose totals swing 429 s → 549 s will red on a rotating cast
  of names anyway;
* **keep the profile hosted — the merge queue still measures that shape — and
  stop letting the T14 lane write a verdict against it;** ← taken
* or price the ceiling per instrument.

`src/durations.rs`'s module header now carries the rule and the measurements
behind it, and `src/ci.rs`'s
`the_softened_duration_verdict_is_the_one_the_merge_actually_raises` holds the
workflow's match string against `durations::TIER_DISAGREEMENT`, because the
telling-apart is a shell string against a Rust string and rewording the panic
would otherwise leave both files reading perfectly while the workflow stopped
recognising the one verdict it may soften.

**Only the price verdict moved.** The T14 lane's suite run gates every test's
verdict exactly as before, `tests/test-durations` and `src/tiers.rs`'s rules are
untouched, and there is no second profile. Every other refusal
`--merge-durations` can raise — a duplicate execution label, a short shard set,
an erased Fast label, a committed `UNMEASURED` marker past its one bought run —
is a fact about the tree or the partition rather than about machine speed, and
still reds on either instrument. Six arms of the step were driven against a
stubbed merge before landing: a T14 tier refusal exits 0 with the warning; the
same refusal hosted, a `may not land` marker refusal on the T14, a duplicate
label on the T14, and a tier refusal with the routing output empty (a failed or
skipped `route`) all still exit 101.

## What remains

* **`src/redlist.rs`'s `xhci_full_speed_device` row is now about a warning.**
  Its evidence — four consecutive T14 lanes reding four different names — is
  unchanged and it is still what a reader of the warning should check; the
  refusal it describes simply no longer fails the job on a T14 lane. That row's
  `source` is this file.
* **`src/redlist.rs`'s `Instrument::Ci` doc is fixed and the profile's own
  instrument is not.** The enum now says two machines wear that name and do not
  price alike. `tests/test-durations` still holds one number per test with no
  record of which machine took it.
* **Nothing hosted measures the nightly tier any more, and this change does not
  create that — it exposes it.** `route.yml` sends `schedule` and
  `workflow_dispatch` to the T14, and those are the only two events that pass
  `--nightly`; a merge-queue ref and a fork's pull request are hosted and never
  do. So every fresh Nightly measurement from now on is a T14 number entering a
  hosted profile — the table above measures that gap at 1.35–1.37× for
  `xhci_full_speed_device` alone and uncontended — and after this
  change the nightly's own `durations` job no longer renders a verdict on it
  either — the `Why::Cost` return rule and the over-the-line rule stop firing
  there. What the hosted merge queue still checks is Nightly *placement* against
  whatever numbers are committed, since `merged_profile` preserves rows a fast
  run did not measure. **The gap is that no instrument matching the profile
  produces a fresh Nightly number at all.** This is prospective, not yet
  observed: `985f3834` landed in `a4e0ab75` at 2026-08-21 10:41Z and the nightly
  that recorded the current profile ran at 03:43Z that morning, before it — the
  first T14 nightly is 2026-08-22 03:00Z.
* **The `UNMEASURED` round trip now takes its one bought measurement on the
  T14.** A new Fast name's marker still reds on a T14 lane by design, and the
  measured value the author is told to commit comes from that lane's artifact —
  a T14 number entering a hosted profile. A branch in this repository cannot get
  a hosted twelve-shard measurement at all except by entering the merge queue,
  so the number is adjudicated there rather than on the pull request.

`cargo run -- --known-red xhci_full_speed_device` answers with the row this
issue is the source of, so an author who meets the refusal does not re-derive
the above.
