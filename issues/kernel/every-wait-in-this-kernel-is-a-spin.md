---
status: open
kind: track
opened: 2026-08-12
---

# Every wait in this kernel is a spin, and a killed task dies by having its stack discarded

Four wait sites still spin — two in xHCI, one in NVMe, one in virtio — so a CPU
waits for a device. A kill is answered by throwing the task's kernel stack away
at five separate reap-in-place arms, and the parked arm is the one that strands
a held lock. Every duration in the kernel is a bare number. None of the work
below is on this branch; a branch carries the first part of it and is not merged.

**The commitment: one completion primitive, one inbox, one park site, one
recheck predicate, and a kill answered by `Cancelled` at the park rather than by
discarding the stack.** This kernel does not unwind, so a killed task holding a
live kernel stack must be schedulable at every safe point and must die by
returning through that stack. Cancellable waits come before sleep locks, and
both before any lock conversion; the order is forced, not preferred.

## The chunks, and the invariant each must preserve

- **Duration kinds.** Every duration carries a kind whose constructor demands
  what justifies it, and a number nobody can cite is a tripwire or does not
  exist.
- **The completion core behind the existing wait queue.** A poster stores its
  record under the subject's own lock before it claims the waiter, so a parker
  that publishes its intent and then reads its inbox can never miss a post.
- **The one park site, and the cancellable kill.** Inseparable. A killed task
  holding a live kernel stack is schedulable at every safe point and dies by
  returning through that stack, so no guard is ever abandoned.
- **The sleep lock.** A sleep-lock holder stays preemptible and raises no
  preempt count, so the baseline assertion keeps meaning exactly "a spinlock is
  held".
- **`usbd` and `iod` on the existing kernel-thread machinery.** No housekeeping
  thread's wait can stop another's, and a panic inside one is recoverable rather
  than a halted machine. Three threads, not one, because a stuck USB enumeration
  must not stop the log.
- **xHCI async, and the four lock conversions.** Inseparable. A CPU never waits
  for a device: the lock is dropped before the park, and a completion is matched
  to its asker by identity, never by arrival order.
- **The idle loop's declared end state.** A CPU halts only when nothing is
  runnable and no deadline is armed, and the halt condition is a declared set a
  test enumerates.
- **A poll kind for registers with no interrupt behind them.** Such a register
  is re-read at a declared cadence inside a declared bound, written once.
- **Blocking syscalls, absolute deadlines, the ring as an inbox.** A deadline is
  absolute and total over its whole range, so no value means "no timeout" and no
  site can silently turn block-forever into return-immediately.
- **The write-back queue.** A file's dirty pages outlive the handle that dirtied
  them until write-back reports complete, and a re-open before that sees the
  pages and not the device.
- **The deletion commit and its source gate.** A spin exists only at a named
  site with a stated reason, and shrinking that list is the only way to claim a
  spin was removed.
- **The interleaved A/B.** The wake number is believable only beside a positive
  assertion that the log still got written — a log that stopped improves it
  identically.
- **Widening the pass-cost window.** It starts where the pass starts, and it is
  turned on only against its own baseline.

## Decisions already made, so they are not re-argued

- A watch is a node the waiter lends to the object, and the subject is a
  borrowed reference, never an id. **Rejected:** a global registry, a slot
  arena, two park channels, posting from interrupt context, multishot polls,
  userspace-only blocking wrappers, a sleep lock that spins where it cannot
  park, poisoning, and shootdown-as-completion. A freed object cannot be named.
- The park token proves the *context* may park and never encodes which locks are
  held. **A `&mut` token is rejected**: it forbids the held-across-a-park shape
  the whole refactor exists to create, and two stacked sleep locks need the
  borrow to stack.
- A spinlock held across a park stays a *runtime* named panic. The type system
  cannot see it, and raising a baseline to clear a red converts a boot failure
  into a field investigation.
- Readiness is level or edge and the class belongs to the subject. **The
  machine's log is edge by necessity**: no reader cursor exists in the kernel,
  and one locked read-modify-write per log line costs **350 ms of boot under
  TCG** (497–504 ms became 812–839 ms), which forbids moving the post to the
  producer.
- The cancel is one-shot, consumed by the wait that reports it; the sticky kill
  bit is what terminates, and a second cancel to one thread panics at the call
  site. **Owed before implementation:** a killed thread cannot park at all
  today, so teardown needs a non-cancellable park, a scoped clear, or a commit
  that distinguishes the two.
- Four locks convert and there is no fifth. Blast radius: 29 filesystem sites,
  68 process-data sites, 259 lock calls over 45 statics. **The token cannot be
  threaded through the block-device trait**, which lives in a pure host-tested
  crate.
- **A bulk transfer has zero real cancellers once the transfer bound is
  deleted** — the reset-recovery path's only trigger *is* that bound. So the
  largest open decision is a tripwire on the transfer against a budget at the
  filesystem layer, and deleting the bound with nothing in its place makes a
  shipped daemon's give-up policy silently unreachable.
- Owner ruling on order: endowment, then the log, then completions.

Measured, and worth carrying: a 2 ms-per-transfer stick takes the worst wake
from **7,117 µs to 165,948 µs** at smp=1, and 6,174 µs to 250,912 µs under load
at smp=8. The audio period is 2.902 ms against a 23.219 ms pipeline. The
scheduler migration cost about **70 defects** in code whose own suites were
green, which is the calibration for how this one is landed.

Five entries under `issues/design-debt/` recorded that the deleted document's
own citations had rotted; they closed with it.
