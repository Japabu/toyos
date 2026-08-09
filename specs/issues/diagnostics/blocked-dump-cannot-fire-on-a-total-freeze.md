---
status: open
kind: defect
opened: 2026-08-06
---

# The blocked-task dump cannot fire when no CPU reaches a scheduler pass

A `CpuSched` is `!Sync` and reachable only from its own CPU, so walking a
sibling's queues is unwritable rather than racy. `task_cpu_ns` and
`task_sched_state` were rebuilt on values the owning CPU *publishes* —
`TaskHandle`'s counters, republished at each end of a pass, plus the core's
rendezvous word — so they are accurate and lock-free, which also closes the old
`try_lock`-and-skip misreport. `dump_blocked` had no such substitute: it printed
the calling CPU's parked map alone, by `TaskKey` and `WaitClass`, with no
process name, so it could confirm a park and never rule one out.

`kernel/src/sched/dump.rs` replaces it, built for #172 and for #142's family.
It walks nothing remote: the asking CPU marks every sibling, kicks it, and each
prints its own tasks from `drain_irqs` at the top of its next pass. **A CPU that
does not reach a pass inside 250 ms is named, and that is a finding** — it is
the only way this report can say a CPU is not scheduling at all.

Two halves, because neither sees what the other does. The CPUs give the
deadline, the wait class and how long the park has lasted; the process table
gives every thread's name and its published state, which is the only place a
task **no CPU was ever given** appears at all. `Ready` with `cpu_ns == 0` is
#142's signature, and the summary's `unheld` count is what the state words claim
minus what the CPUs hold.

The three failure modes degrade rather than stop the report: a silent CPU is
named and the verdict says its counts are of what answered; the process table is
retried for 20 ms and then reported as held, with the deadline half of the
verdict still printed; and a CPU inside a pass says so. Nothing allocates,
nothing waits on a lock it could find held, and truncation takes only ordinary
lines — an overdue or absurd deadline is never dropped.

It ends by painting the panel (`panic_console::paint_report`), taking the screen
from whoever holds it, because the machine it is for has no serial port and a
wedged one may never flush its log file. The verdict is the last line printed
and the console paints the newest page, so the screenful a phone camera catches
is the one carrying it.

Gates: `blocked_dump` (eight CPUs, every one present, and the two halves' counts
must agree) and `screen_blocked_dump` (muted metal-sim, a compositor holding the
screen, the verdict read back off the decoded panel). Negative controls run:
without the per-CPU answer the report reaches 1 of 8, and with the userland
check restored on the paint the panel carries nothing.

**A deadline that has passed and whose pass has not yet run is a real,
microsecond-wide state** — `blocked_dump` has seen `1 OVERDUE` on a healthy
guest. The count is evidence, not a verdict; what condemns a machine is one that
stays.

**KNOWN BLIND SPOT: the dump cannot fire when no CPU reaches a scheduler pass.**
It is dispatched from `drain_irqs` at the top of a pass, so a *partial* wedge
answers and a *total* freeze is silent — the owner pressed it after pulling the
USB stick and got nothing, which is itself weak evidence that no CPU was
passing. Whether an interrupt-dispatched variant can close that is the
scheduler agent's (the NMI-on-timeout proposal); what follows is what the
author of this facility established while building it, so nobody rediscovers it
or removes it by accident.

- **`Lock::lock` disables preemption, not interrupts** (`sync.rs`) — a ticket
  spin after `preempt::disable()`, no `cli`.
- **That fact is load-bearing for the 250 ms sibling wait.** Spinning there is
  safe *because a preempt-driven pass provably holds no `Lock`*: taking one
  raises the preempt count, so a pass cannot be entered while one is held.
  **The argument does not survive being moved to an interrupt**, which can land
  on a CPU that *is* holding a lock — the waiter would then block the sibling
  it is waiting for. `request`'s `assert!(depth <= 1)` encodes exactly this
  entry condition and is not decoration.
- **`log!` takes the serial lock.** It is the single reason the report is not
  ISR-safe today: an interrupt landing on a CPU that holds it deadlocks on the
  first line. The panic console's paint is the lock-free counter-example and is
  ISR-tolerable; `paint_report`'s `PAINTING` is a swap latch that self-releases,
  not a `Lock`.
- **The per-CPU half is already reentrancy-safe by construction.** The
  schedulers are not behind a `Lock` at all: `SCHEDS[cpu]` is guarded by the
  `IN_PASS[cpu]` flag, mutation happens only inside `with_cpu` which sets it,
  and `try_with_cpu` refuses when it is set. An interrupt that lands mid-pass
  therefore reads nothing rather than reading a torn map.
- **The process table is `try_lock` retried to a 20 ms ceiling, and the retry
  is not belt-and-braces.** Bare `try_lock` was the first version and
  `screen_blocked_dump` caught it losing the whole census to a *transient*
  holder — a spawn in flight 0.75 s into boot. The ceiling separates "someone
  is mid-spawn" (microseconds) from "the holder is what is wedged" (never).
- **Truncation may never hide what the report is for.** `LINES_PER_CPU` bounds
  ordinary parked lines; a line whose verdict is `Overdue` or `Absurd` is not
  counted against the budget, and `UNPRINTED` says how many ordinary ones went.
- **The summary is last for a causal reason, not a cosmetic one**: it is the
  only part that needs every CPU to have answered, and the console paints the
  *newest* page, so last is what a photograph catches. Any variant that
  reprints must keep that order.
- **The deadline arithmetic is guarded by its own classification.**
  `Verdict::of` matches `at <= now` before it ever evaluates `at - now`, and
  `Deadline`'s `Display` only subtracts in the arms that verdict produced.
  Overflow checks are on in the kernel, so reordering those arms turns a
  diagnostic into a panic.

What is left of this entry: `ps` and `stats` still have no cross-CPU view of
anything the handles do not publish.

`screen_blocked_dump` is intermittent **alone**, and at the same rate on an
untouched tree: five runs a side on 2026-08-07 gave three green and two red on
`main` at `48147c2` and the same on `wt/toyos-wedge`. A green of it is one
sample. The mechanism first recorded for that red — the ring tail lifting the
summary off the page — was wrong: userland repaints over the report, and the
failing string is the one that sat under the window rather than beside or below
it. That is what `panic_console::hold_report` answers.
