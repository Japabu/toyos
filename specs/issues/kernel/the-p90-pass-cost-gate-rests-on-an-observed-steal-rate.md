---
status: open
kind: finding
opened: 2026-08-17
---

# The pass-cost gate's 90th percentile rests on an observed steal rate, not on a bound

`tests/common/passcost.rs` gates the check build's pass-cost distribution at
**nine passes in ten provably under `MAX_PASS_NS`**. The magnitude is derived —
it is the budget itself — and the *fraction* is what this file is about.

The gate's argument is that a host which deschedules a vCPU touches the handful
of passes it lands in, so it moves the maximum and not the 90th percentile.
**That is an observation about this infrastructure over a sampled period. It is
not a property of hypervisors, and nothing here bounds it.** A host under load
does not deschedule a vCPU once; it deschedules repeatedly, and the passes it
delays are not independent draws.

## What the record actually says, in both directions

**For the gate.** Invariant P asserted that *every* pass fits 200 000 ns and was
green in 89 of 91 KVM runs. Green means zero crossings, not one — about 27 000
passes with no sample over the budget at all, so the 90th percentile of every one
of those runs was far under it. That is a real and fairly strong observation, and
it is why the gate was pitched at nine in ten rather than lower.

**Against the gate, and this is the sharper half.** The two runs that did fire
are **censored exactly where the count lives**: `schedule_no_return` halted every
CPU at the first crossing, so a second, third or fifteenth crossing in those runs
could never have been observed. The record can therefore distinguish "steal
reached one pass" from "steal reached fifteen" in no run at all — the same
truncation artefact
`specs/issues/kernel/the-check-build-guest-stopped-answering-on-kvm-twice.md`
identified in the *magnitudes*, now in the counts.

**And the correlated shape is not hypothetical.** On the dev host under
contention the same instrument measured 14 of 134 and 19 of 140 passes over the
budget — 10 % and 13 %, which reds a 90th percentile outright. That was
cross-arch emulation rather than hypervisor steal, but it is the same class of
time and this gate cannot tell the two apart. `src/redlist.rs`'s
`Instrument::DevHostLoaded` row for `sched_check_build` is that measurement.

So a p90 red on a busy CI day is something this gate can produce for reasons the
kernel did not cause. That is a much cheaper failure than a halted machine and it
is adjudicated where composed-quantity reds are adjudicated, but it is a standing
weakness in the gate's warrant rather than a closed question.

## The two instruments that would settle it, neither chosen here

**1. Read steal time and subtract it.** KVM publishes per-vCPU steal time
through a paravirtual MSR, and a guest that reads it can gate a quantity it
actually observes rather than one it infers. **Nothing in this tree reads any
hypervisor facility at all** — there is no `CPUID` hypervisor leaf, no KVM MSR,
and the only MSRs the kernel names are `GS_BASE`, `EFER`, `STAR`, `LSTAR` and
`FMASK` (`kernel/src/arch/percpu.rs`, `kernel/src/arch/syscall.rs`). Weigh that
against what this project is for: it is a hypervisor-specific facility in a
kernel whose north star is metal, it is dead weight on the T14, and it makes the
check build's verdict depend on which accelerator ran it. It is also the only
thing that turns the guest's inference into the guest's observation.

**2. Run the gate against a deliberately loaded host and measure whether p90
moves.** Far cheaper, needs no kernel facility, and converts the assumption into
a measurement: put a known load on the machine, run `sched_check_build` alone
against it, and record what fraction of passes cross the budget as a function of
that load. The dev host has already produced two points of that curve by
accident (quiet: 5–9 of ~150; contended: 14–19 of ~140); what is missing is the
same sweep on **KVM**, which is the accelerator the gate is really about and
which the dev host cannot boot.

The second is worth doing first whatever is decided about the first, because it
prices the risk. The first is a design decision and belongs to the owner.

## The owner closed the first and ruled the second: measure

**Instrument 1 is closed and will not be reopened.** KVM's paravirtual
steal-time MSR is a hypervisor-specific facility, this project's north star is
self-hosting on metal, and a correctness gate that depends on a KVM interface
cannot come along. If steal accounting is ever wanted it is a diagnostic and
never the basis of a gate.

**Instrument 2 was run on 2026-08-18 and the section below is what it measured.**

## The measurement: a quiet arm and a loaded arm, interleaved

The dev host boots no KVM guest, so hypervisor steal cannot be produced here.
What can be produced is the same *class* of quantity — time the guest neither
observes nor controls — by taking the host's CPUs away from QEMU.

**The load, stated.** Fourteen concurrent pure-shell spin loops
(`( while :; do :; done ) &`), one per logical CPU: `sysctl -n hw.logicalcpu`
answers 14 on this machine. Measured standing alone before the session, the
fourteen ran at 90.0–94.9 % CPU each and took the 1-minute load average from
9.45 to 28.02. They were started 20 s before each loaded run and killed the
moment it returned; the quiet arm ran with nothing added.

**The design.** Six repetitions per arm, strictly interleaved
quiet-loaded-quiet-loaded in one session (10:42–10:48, 2026-08-18) so both arms
share the ambient host rather than being compared across hours. One repetition
is one `cargo test --test toyos-build -- sched_check_build`, which is
`Sched::Serial` and boots the check kernel at `smp: 2` — so **twelve CPU-runs
per arm**. The ambient host was not idle in either arm (another worktree's suite
had just finished; 1-minute load 18–42 throughout), which is why the arms are
separated by an instrument-side measure rather than by a claim: the harness's
own `fastest boot ... paid at N.NNx width` line reads **1.74x–2.34x on every
quiet run and 2.66x–2.78x on every loaded one**, with no overlap.

### What it found

| | quiet, 12 CPU-runs | loaded, 12 CPU-runs |
|---|---|---|
| p50 bucket | 8 192 ×1, 16 384 ×1, 65 536 ×10 | 8 192 ×1, 32 768 ×1, 131 072 ×10 |
| **p90 bucket** | **65 536 ×2, 131 072 ×10** | **131 072 ×3, 262 144 ×9** |
| max, ns | 1 508 330 – 1 983 355 | 1 723 825 – 3 914 718 |
| passes, pooled | 2 732 | 2 619 |
| over budget, pooled | 95 (3.48 %) | 187 (7.14 %) |
| over budget, per CPU-run | 1.2 % – 6.0 % | 4.2 % – 12.6 % |
| **p90 over the budget** | **0 of 12** | **9 of 12** |
| gate verdict | **6 of 6 runs green** | **6 of 6 runs red** |

**p90 moves with host load, and so does everything else.** The answer is not
close: the whole distribution translates up by exactly one power-of-two bucket
— the median from 65 536 to 131 072 and the 90th percentile from 131 072 to
262 144 — and `MAX_PASS_NS` at 200 000 sits *between those two buckets*, which
is the whole of why the verdict flips. Twelve quiet CPU-runs and not one over;
twelve loaded and nine over. The prior this was run against — #113's own
`DevHostLoaded` row at 10–13 % of passes over budget — is confirmed rather than
refuted.

The maximum moved too (worst quiet 1 983 355 ns against worst loaded
3 914 718 ns), so this is not a load that touches only the tail. **Every order
statistic moved.** There is no quantile of this distribution that host load
leaves alone, which is the sharpest form the finding can take: no fraction
chosen instead of nine-in-ten would have survived.

### And the same instrument on KVM, which is what the gate is really about

The describe line has been printed on every run since #113, green or red, so
CI's own logs already hold the sample the file above said was missing. Harvested
with `gh run view --job <id> --log` over every `ci` workflow run since the
instrument landed:

```
32043101865 guest (11)  cpu0: 157 passes,  p50 < 32768, p90 < 32768, max   64584 ns, 0 over
32043101865 guest (11)  cpu1: 148 passes,  p50 < 32768, p90 < 32768, max   81569 ns, 0 over
32044763748 guest (11)  cpu0: 173 passes,  p50 < 32768, p90 < 32768, max   72012 ns, 0 over
32044763748 guest (11)  cpu1: 155 passes,  p50 < 32768, p90 < 32768, max   34729 ns, 0 over
32045857575 guest (11)  cpu0: 1533 passes, p50 < 16384, p90 < 32768, max   85541 ns, 0 over
32045857575 guest (11)  cpu1: 150 passes,  p50 < 16384, p90 < 32768, max   41678 ns, 0 over
32047352064 guest (11)  cpu0: 156 passes,  p50 < 32768, p90 < 32768, max   36408 ns, 0 over
32047352064 guest (11)  cpu1: 148 passes,  p50 < 32768, p90 < 32768, max   42772 ns, 0 over
32050586046 guest (11)  cpu0: 170 passes,  p50 < 16384, p90 < 32768, max   36250 ns, 0 over
32050586046 guest (11)  cpu1: 153 passes,  p50 < 16384, p90 < 32768, max   32073 ns, 0 over
32096188866 guest (11)  cpu0: 1454 passes, p50 < 32768, p90 < 65536, max  173906 ns, 0 over
32096188866 guest (11)  cpu1: 166 passes,  p50 < 32768, p90 < 32768, max   57384 ns, 0 over
32116842348 guest  (4)  cpu0: 163 passes,  p50 < 32768, p90 < 32768, max   71607 ns, 0 over
32116842348 guest  (4)  cpu1: 154 passes,  p50 < 32768, p90 < 32768, max   40062 ns, 0 over
```

**On KVM the 90th percentile is 32 768 ns and the maximum never reaches the
budget at all.** Not one pass over 200 000 ns in any of them, and the largest
single pass across the whole set is 173 906 ns. The two accelerators are not one
instrument: p90 differs by a factor of four between KVM and a quiet dev host and
by eight against a loaded one, and the maxima differ by a factor of twenty.

### What follows, and it was decided before the measurement was run

The gate cannot stand as a threshold. `MAX_PASS_NS` is a policy number derived
for native hardware, and on the one accelerator where it is meaningful it has a
factor of six of headroom that gates nothing, while on the other it sits inside
the range host load alone sweeps. The replacement is a report plus a comparison
against a **recorded distribution**, per accelerator — the shape
`tests/audio-baseline.toml` already uses, where a run is judged against its own
recorded sample rather than against an absolute line. The maximum stays ungated,
for the reason it always was.

## What a red under this name means

`src/redlist.rs`'s `sched_check_build` rows carry the discriminator; it is
repeated here because this is the file a reader of a red is sent to.

A p90 red is a claim about the *scheduler* only when the isolated re-run reds
too **and** a same-session A/B against `main` comes back green — the harness's
standing law for the `ALONE: GREEN` class (`tests/CLAUDE.md`). The cheap tell in
the failure line itself is the **median**, which the same line prints: every red
measured so far carried `p50 < 131072 ns` against `p50 < 16384` or `< 32768 ns`
on the green runs, because contention moved the whole distribution. **A p90 red
whose median sits with the green baseline is the shape host load has not been
observed to produce, and is the one to bisect.** That is a heuristic with its
evidence attached, not a proof, and this file exists because the difference
matters.
