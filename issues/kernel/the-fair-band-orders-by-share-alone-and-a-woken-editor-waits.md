---
status: open
kind: finding
opened: 2026-08-16
---

# The fair band orders by share alone, and a just-woken editor waits for a compiler

Queued track from the 2026-08-16 owner conversation on scheduler direction; the
owner wants responsiveness over long-running compute, universally, not as an option.

Today the fair band orders purely by share. The state of the art (EEVDF, Linux's
default since 6.6, and the BORE line of work) keys on one insight: interactive
tasks betray themselves behaviorally — they sleep long and run in short bursts —
and the fair band can order by *lag and virtual deadline* so a just-woken task
gets the next slice without ever receiving more total CPU. A compiler loses the
race for the next 10 ms, never its share of the hour.

This is the intended occupant of the intra-fair policy seam: the reservation
layer is mechanism and is untouched; an intra-fair policy may reorder fair tasks
freely but cannot affect any reservation guarantee, and the simulator's
invariants gate the swap. One policy in the tree at any time — the seam exists so
the swap is a bounded, sim-gated change, never an A/B in the OS.

Blocked on: the reservation layer being built.
