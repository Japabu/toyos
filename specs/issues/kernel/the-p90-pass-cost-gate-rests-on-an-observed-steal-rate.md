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
