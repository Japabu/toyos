---
status: open
kind: finding
opened: 2026-08-16
---

# A thread's blocked time does not exist until the park ends

`ProcessStats`'s five `blocked_*_ns` counters are charged at the transition
*out* of `Blocked`: `Task::charge_residency(now, Residency::Blocked(class))`
runs from `BlockedTask::wake`, which is the wake or the deadline fire. A thread
that is parked **right now** therefore contributes nothing for the park it is
in, however long it has been in it.

`cpu_ns` does not have this shape. `TaskHandle::cpu_ns` adds the live slice
itself — `base + now − running_since` — with its own doc explaining why: "so the
number does not stand still between passes". Blocked time stands still.

**Why it matters is the question the counters exist for.** The breakdown was
built for the T14 wedge investigation, where "this process is blocked" was
already known and *what it is blocked on, and for how long* was the question. A
`ps` taken during the wedge reads zeroes for every thread still inside the wait —
which is every thread the reader cares about. The instrument answers about waits
that have already ended, and a wedge is a wait that has not.

Found while writing `process_stats`' fourth arm, whose first draft read the
counters off a live parked child and got `0` for every class. That arm now ends
the park before it asks, and says so; the gap is here rather than worked around
silently.

**Pre-existing, and not the completion cutover's.** `charge_residency` and its
three residencies predate it. What the cutover changed is which *class* the park
is charged to (`WaitClass` is the wait's now, named at the arm), not when.

The fix has the same shape as `cpu_ns`'s and is not free: a reader would need
the park's `since` stamp and its class from the owning CPU, and a `CpuSched` is
`!Sync` — `sched::dump`'s `for_each_parked` reaches only the calling CPU.
`TaskHandle` is the cross-CPU face and could publish `(class, since)` at the
park the way it publishes `running_since` at the dispatch, which is two relaxed
stores on the park path. Whether that is worth two stores per park is the
measurement whoever takes this owes.
