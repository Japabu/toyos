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
   against a number. **Done — see below.**

## Instrument, and the baseline (2026-08-22)

`kernel/src/irq_census.rs` counts every delivery per CPU per source in
`PerCpu`, one `add qword ptr gs:[<off>], 1` for the machine's total and one for
the source. `irq: cpuN total=… timer=… …` is printed per CPU beside the
process-exit census, on `SYS_SHUTDOWN` and on the blocked-task dump;
`common::irqcensus` aggregates every guest's newest line into the suite's own
summary, so a CI shard's log carries the number without `--nocapture`.
`irq_census_conservation` gates both the arithmetic and the present-state fact.

A guest that boots and runs no program reaches no process exit and prints no
census, which is why the reporting counts are short of the boots. Both columns
are one run: an interrupt count is a function of timing, so the totals move
between runs and the *distribution* is what to compare.

| | dev host, TCG, 12-wide | hosted CI, KVM, twelve shards (run 32585458505) |
|---|---|---|
| guests reporting / booted | 43 / 80 | 42 / 80 |
| interrupts | 18,314 | 39,096 |
| on cpu0 | 12,239 (**66.8%**) | not summed — that field postdates this run; per shard cpu0's per-guest share ran median **54.6%–98.0%**, max 87.3%–100.0% |
| per guest, cpu0's share | median **87.9%**, p90 96.3%, max 100.0% | |
| device interrupts | 7,962 (43.5%), **100% on cpu0** | 27,817 (71.2%), **100% on cpu0 in all twelve shards** |

Per source, `count (% of the machine's traffic, % of them on cpu0)`:

| source | dev host | hosted CI |
|---|---|---|
| timer | 8,796 (48.0%, 38.6%) | 9,721 (24.9%, 1.0–88.3% by shard) |
| xhci | 7,181 (39.2%, **100%**) | 26,948 (68.9%, **100%**) |
| tlb | 1,555 (8.5%, 56.9%) | 1,557 (4.0%, 0–75% by shard) |
| i8042 | 442 (2.4%, **100%**) | 430 (1.1%, **100%**) |
| sound | 338 (1.8%, **100%**) | 438 (1.1%, **100%**) |
| net | 1 (**100%**) | 1 (**100%**) |
| nmi | 1 (0%) | 1 (0%) |
| hda, dmafault | 0 | 0 |

**The two machines agree on the thing the track is about.** Every device
interrupt is cpu0's on both, and they are between two fifths and seven tenths of
all the interrupts the machine takes. What is already spread is the timer and
the shootdown IPI, which are per-CPU by construction and not by policy — so the
headroom the change is after is the device half, and 100% → the fair share is
the number to beat. The hosted sum of the per-source counts equals the hosted
total exactly (39,096), which is the census's own conservation law holding
across twelve independent machines.
