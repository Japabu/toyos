---
status: open
kind: finding
opened: 2026-08-06
---

# `Lock::lock`'s spin is the half of the ticket lock loom cannot reach

`kernel-loom` compiles `kernel/src/sync.rs` a second time against loom's
atomics, so the models drive the real primitive rather than a transliteration.
They drive `try_lock` and `LockGuard::drop`. They do not drive `lock()`: loom
explores a spin as an unbounded branch and gives up — `Model exceeded maximum
number of branches`, which is what the first draft of
`try_lock_observes_the_previous_owners_writes` produced — and the
`loom::thread::yield_now()` that would bound it belongs to loom, not to a kernel
that really does spin.

What that leaves unmodelled is contention on `lock()` itself: the ticket
ordering, and the FIFO fairness the ticket exists to buy. The *release* edge is
shared — both acquire paths end at the guard's `now.fetch_add(1, Release)` — so
the models do exercise the publication side; the waiting side is still certified
by reading.

Nothing in the guest suite can substitute. x86's TSO gives every load acquire
and every store release semantics, so a missing acquire edge in this primitive is
invisible on the only architecture ToyOS currently boots, and becomes observable
on ARM64, which is planned and not built. That is why `try_lock`'s acquire edge
sat on the wrong atomic through every green suite run until a model checker was
pointed at it.
