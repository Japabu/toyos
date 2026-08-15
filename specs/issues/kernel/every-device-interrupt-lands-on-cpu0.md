---
status: open
kind: finding
opened: 2026-08-15
---

# Every device interrupt lands on cpu0, and the plan for when that stops being free

`kernel/src/drivers/pci.rs`'s `MSG_ADDR` says it plainly: MSI and MSI-X
alike are pointed at `0xFEE0_0000` — physical destination 0, fixed
delivery — "so every device interrupt lands on the boot CPU and is
spread from there by `irq_ring` plus the scheduler rather than by the
interrupt controller." Hardware delivers everything to cpu0; software
spreads the *work*, never the *delivery*.

This is a deliberate design and this entry does not call it wrong. What
no file states is what it costs, and what has to change before those
costs are paid:

- **A per-interrupt tax on whatever cpu0 is running.** Every device
  interrupt preempts cpu0's thread for the ISR, regardless of which CPU
  will do the actual work. A latency-critical thread placed on cpu0
  pays for every USB transfer and network frame in the machine.
- **A delivery ceiling.** One CPU's ISR throughput bounds the whole
  machine's interrupt rate. At today's device set under QEMU nothing
  approaches it; a fast NVMe queue or a real network under load is a
  different regime, and neither has been measured against it.
- **A single point of silence.** A cpu0 wedged with IF clear stops
  every device interrupt in the machine at once, which couples "one CPU
  is stuck" to "all I/O is stuck" in a way the other CPUs' wedges do
  not share.

The exception that stays: the i8042 pin (`drivers/i8042/mod.rs`) is a
separate decision with its own argument — a one-byte port register
needs exactly one reader, input is ~100 Hz, and there is no load
argument for spreading it. Nothing here touches it.

**Why this is not taken as standalone work**: distributing interrupts
twice is churn. The completion architecture
(`specs/completion-architecture-spec.md`, pipeline 2) decides where a
completion should be *processed* — and the delivery target that makes
that efficient (per-queue MSI-X vectors aimed at the CPU that consumes
the queue) falls out of that design, not out of a round-robin patch
applied before it. Interrupt remapping under the IOMMU track
(`specs/iommu-spec.md`) changes what destinations are even expressible
— `drivers/ioapic.rs` already notes the 8-bit physical-destination
limit without it — and the userspace-driver track moves some vector
ownership out of the kernel entirely. The distribution question is
those tracks' question, and it should be answered once, there.

**The escalation trigger, stated so it is checkable**: a measurement
that attributes a missed audio deadline, a scheduler-pass overrun, or a
saturated cpu0 to interrupt concentration — on metal or under QEMU —
moves this from finding to defect and it stops waiting for pipeline 2.
Until then, the numbers this entry deliberately does not contain are
the ones nobody has measured.
