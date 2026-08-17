---
status: open
kind: finding
opened: 2026-08-16
---

# All cores are assumed equal, and ARM64 breaks that

Queued track from the 2026-08-16 owner conversation on scheduler direction.

Heterogeneous cores are the norm outside our current hardware: Intel P/E cores
(12th gen onward; the T14 Gen 2 predates the split) and ARM big.LITTLE, which is
universal on the ARM64 machines the roadmap names. A scheduler that assumes
equal cores places background churn on a fast core and a latency-sensitive wake
on a slow one, and its load arithmetic is wrong everywhere.

What the track needs when picked up: a per-core capacity model; placement by
task character (bursty/latency-sensitive work on big cores, throughput work on
efficient ones); and eventually an energy model deciding when saving power beats
finishing sooner (the EAS school on ARM).

What is already done so this does not have to be retrofitted: the reservations
spec states that admission sums against **each CPU's own capacity** — a
reservation is a fraction of a specific CPU, and "all cores equal" is never
baked into the arithmetic. That one sentence is the cheap half; this issue is
the expensive half, and it waits for ARM64.
