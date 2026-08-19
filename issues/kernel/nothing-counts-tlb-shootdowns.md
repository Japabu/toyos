---
status: open
kind: finding
opened: 2026-08-19
---

# Nothing counts TLB shootdowns, so every quantitative claim about them is unmeasured

`arch::tlb::shootdown` sends an IPI to every CPU and spins with `IF` clear until
all of them acknowledge, under a 5 s panic deadline
(`kernel/src/arch/tlb.rs`). Nothing counts how often that happens, how long it
takes, or which path issued it. `tests/toyos-rust-tests/src/bin/tlb_shootdown_waits.rs`
times one *staged* shootdown through a test actuator; there is no number for an
ordinary boot or an ordinary workload.

The consequence is not that the paths are wrong — it is that no proposal to
narrow them can be judged. Two are already waiting on this:

**Per-address-space CPU residency masks**, so an unmap interrupts only the CPUs
that ran the process. It is the obvious next idea and it is not obviously the
next move: it is a new lock-free mechanism — a CPU adopting an address space
races an unmapper reading the mask — which in this tree means a `kernel-loom`
model and its negative controls before it may be trusted. Whether that is worth
building is a question about how many shootdowns a machine actually takes.

**Deferred reclamation.** Narrowing the entry set does not touch what makes
`Unmapped::drop` expensive: the IPI and the wait stay exactly where they were.
The lever for that path is not waiting at all, which is
`issues/kernel/every-wait-in-this-kernel-is-a-spin.md` applied to memory.

An instrument, not a decision: nobody has to rule on this, somebody has to
count.
