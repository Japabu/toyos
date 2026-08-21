---
status: open
kind: track
opened: 2026-08-04
---

# Nothing charges a kernel allocation to the process that caused it

No allocation the kernel makes on a process's behalf is attributed to it, so
there is no per-process memory in `ps`, no `meminfo`, no reap-time assertion
that a dead process's charges went to zero, and no way for the suite to assert
the absence of a leak.

**The security case that once justified this is mostly gone, and that is the
news.** It was staged as an ownership discipline — a purpose-carrying allocator
type, aliases per subsystem, a charge on the four physical-memory chokepoints —
against a table of bugs that a different mechanism has since closed. Handle and
object lifetimes are refcount-driven with real `Drop` impls, so "a handle that
is not a value with a destructor" is unrepresentable; the descriptor type and
its un-refcounted clone are gone; the file cache installs a real budget and a
budget that was never installed is now a loud kernel bug; and the unbounded
user-string copy has a ceiling. What survives is the *accounting*, which nothing
has touched, plus two items: panic recovery still runs no teardown, and peak
memory is written by two paths that overwrite each other.

Blocked on nothing. Two things worth knowing before it is restarted, because
both cost a day to discover:

- **Omitting `#[global_allocator]` enforces nothing** — rustc errors whenever
  `alloc` is in the graph at all. The enforcement is a type alias with no
  default parameter, about 50 lines, not the Rust-for-Linux approach of dropping
  `alloc` entirely.
- **`#[must_use]` is inadequate for an obligation like this**: binding, `let _
  =`, `drop()` and burial in a collection all pass silently. A drop bomb is the
  state of the art, and `Unmapped<T>` is already exactly such an obligation.

`issues/kernel/no-alloc-error-handler.md` is the terminal state of every
unbounded grower and closes with this.
