---
status: assigned
kind: defect
opened: 2026-08-01
---

# The compositor and netd do not bound what they accept

Neither program has a `MAX_` constant of any kind. The compositor calls
`Poller::new(256)` (`compositor/src/main.rs:747`) and registers three fixed fds
plus one per window, with `windows.push` unguarded (`:127`, `:1377`). netd calls
`Poller::new(64)` (`netd/src/main.rs:1060`) and registers two plus one per tx
pipe. **The 256 and the 64 are guesses, not caps derived from anything.**

This is the same class as the two bounds CLAUDE.md holds up as policy —
`user_ptr::MAX_USER_STR` and `fd.rs`'s `MAX_FDS`, each sized against a stated
ceiling and enforced at the one primitive that can breach it. Nobody wrote these
two. A defect on its own terms, not poller plumbing: the poller capacity is where
it happens to surface first.

**It compounds `no-physical-memory-fairness`, and the pair is worse than
either alone.** An unbounded window count is a memory-growth path any client can
drive, on a system with no per-process limits, no pressure signal and no OOM
killer. Neither entry is alarming by itself; together a single misbehaving client
takes the machine.

Fix in progress, with two requirements on its shape: the bound must state what it
is a function of, and refusing past it must be an error return — not a panic, and
not a silent drop.

**netd's half is closed, and the two numbers quoted above are both stale.** It
derives `max_piped_connections` from physical memory, caps it at what one poller
can watch, and refuses past it with `ERR_RESOURCE_EXHAUSTED` — gated by
`netd_connection_caps`, which makes the daemon announce the cap and the guest
measure where the refusals start. The `accept` rework added the second bound the
entry did not know it needed, `MAX_PENDING_CONNS`, for connections accepted and
not yet identified; `netd_hostile_peer` measures that one. `Poller::new(64)` is
now `Poller::new(FIXED_POLL_FDS + MAX_PIPED_SLOTS + MAX_PENDING_CONNS)`
(`netd/src/main.rs:1228`). The compositor's half is not this agent's to close and
its numbers here want the same re-reading.
