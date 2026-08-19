---
status: open
kind: finding
opened: 2026-08-19
---

# One derived constraint on `on_zero_handles` hooks has no home after the specs teardown

Noticed while re-pointing citations across the `specs/` → `issues/` move (#127),
not while looking for it.

`specs/completion-architecture-spec.md` §21 row 9 carried a rule that was
**derived rather than restated** — it existed nowhere else, because it was the
conclusion of an interaction between two branches' designs:

> **The general rule, which is new and binds their chunks 1 and 2**: none of the
> three drain sites has a `Parkable` (`do_schedule` entry provably does not,
> §6.1), so after C5 **no `on_zero_handles` hook may take a `SleepLock` at
> all** — the compiler refuses it. `FileObject → writeback::push` is the shape
> *every* hook needing the VFS must take, not a one-off.

Recover it with `git show b27b947:specs/completion-architecture-spec.md`, line
2177.

The teardown's commitment was that *"every live commitment becomes an issue,
every citation becomes the fact it stood for"*, and the track that inherited the
completion work — `issues/kernel/every-wait-in-this-kernel-is-a-spin.md` — does
carry the sleep lock and the four lock conversions as chunks. It does **not**
carry this consequence of them. `rg 'on_zero_handles|SleepLock' issues/` finds
it in no file that main wrote.

## Why it is worth one line somewhere

It is a compile-time-enforceable rule about a hook table that is open for
extension: `kernel/src/object/mod.rs`'s `kobject!` macro makes adding a
`deferred` row a compile error until somebody writes its `ZeroHandles` impl, and
the natural thing to write in one is "take the lock the subsystem needs".
Whoever writes the next `deferred` row after the sleep lock lands has no way to
learn from the tree that the VFS is reachable from a hook only through a queue.

It is stated in `drain_zero_handles`'s own doc on this branch, which is the
narrowest correct home for it, and repeated in
`issues/kernel/deferred-release-outlives-its-syscall.md` because that is the
entry whose fix would have to honour it. Whether it also belongs in the track is
that track's owner's call — which is why this is a `finding` and not a defect,
and why nothing here edits the track.

## Not a claim that the teardown was wrong

Everything else this branch cited survived the move intact, including the whole
of the release protocol's ownership. This is one derived sentence out of a
document that was mostly restatement, and the cheapest honest response may well
be to decide it is already said well enough at the site and delete this file.
