---
status: open
kind: track
opened: 2026-08-20
---

# Every interrupt lands on the boot CPU

**The owner directed on 2026-08-20**: all CPUs handle interrupts, with a good
policy choosing the right CPU for each — an approved optimization track, not a
question.

**What the tree does today, at the sites.** `kernel/src/drivers/pci.rs`'s
`MSG_ADDR` says it outright: physical destination 0, fixed delivery — "every
device interrupt lands on the boot CPU and is spread from there by `irq_ring`
plus the scheduler rather than by the interrupt controller." That was a chosen
design, and its virtue is simplicity: one delivery CPU means single-producer
rings (`i8042`'s `IRQ_CPU` invariants lean on exactly that) and no cross-CPU
races in ISR paths. Its cost is the ceiling: one CPU's interrupt bandwidth is
the machine's, and every device shares it.

**The shape of the work.**

1. Per-vector destinations: MSI and MSI-X carry a destination per vector;
   devices with multiple queues (NVMe most of all) get one vector per CPU or
   per queue, delivered where the completion is consumed.
2. A placement policy, derived not guessed: the CPU that submitted the work
   takes its completion where the device allows; devices with one vector get
   a home CPU chosen against load, and the policy is a pure decision the
   host can test (the `toyos-desktop`/`toyos-hda` pattern — a pure crate for
   the decision, the kernel applies it).
3. Every single-producer invariant that leans on one-CPU delivery is found
   and either kept (by keeping that device's delivery pinned) or rebuilt for
   its new producer story — the `i8042` module header lists its own; the
   audit finds the rest. This is the dangerous half, and the reason this
   track sequences AFTER pipeline 2's lock conversions: the drain and wait
   machinery under the ISRs must be settled ground first.
4. The instrument before the change: measure interrupt distribution and the
   boot CPU's share under the loaded suites, so the improvement is a number
   against a number.
