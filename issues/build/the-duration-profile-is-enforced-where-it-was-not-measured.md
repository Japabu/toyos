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
same test measured 10,166–12,156 ms.

## What this leaves undecided

The remedy is a choice nobody has made, and it is not this issue's to make:

* re-measure the profile on the instrument that now enforces it, and accept
  that a 1/1 lane whose totals swing 429 s → 549 s will red on a rotating cast
  of names anyway;
* keep the profile hosted — the merge queue still measures that shape — and
  stop letting the T14 lane write a verdict against it;
* or price the ceiling per instrument.

Two module headers assert the old world and are now false at the site:

* `src/durations.rs` says the profile's numbers come from "KVM on four Azure
  cores". They did; the run that judges them no longer does.
* `src/redlist.rs` describes `Instrument::Ci` as "KVM on four native x86-64
  cores" and says "metal is not an instrument here, because the suite does not
  run on the T14". The suite runs on the T14 for every trusted event.

Until then `cargo run -- --known-red xhci_full_speed_device` answers with the
row this issue is the source of, so an author who meets the red does not
re-derive the above.
