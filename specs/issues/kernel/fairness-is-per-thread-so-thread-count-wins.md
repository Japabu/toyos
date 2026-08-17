---
status: open
kind: finding
opened: 2026-08-16
---

# Fairness is per-thread, so thread count wins

Queued track from the 2026-08-16 owner conversation on scheduler direction.

Fair scheduling today shares the CPU between *threads*, so a process with 100
threads out-schedules a process with 8 just by showing up more — a browser beats
a compiler on head count, not on merit. Production schedulers make shares
hierarchical: groups first, threads within their group.

ToyOS has a better grouping handle than Unix's cgroups bolt-on ever was: the
capability domain. A process's share can hang off what its parent endowed it
with, in the same `system.toml` that endows everything else — hierarchical
reservations, since reservations nest naturally. The reservation model from
`specs/scheduling-reservations-spec.md` is the substrate; this issue is the
hierarchy on top of it.

Blocked on: the reservations implementation. Design question to settle at pick-up:
whether the hierarchy's nodes are processes, endowment parents, or named domains
in `system.toml`, and what the default share of an unconfigured process is.
