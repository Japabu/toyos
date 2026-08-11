---
status: open
kind: defect
opened: 2026-08-05
---

# A scheduler pass may spend two seconds in xHCI before it drains its mailbox

`sched::driver::pass` and `pass_block` both open with `drain_irqs()`, and
`drain_irqs` calls `xhci::poll_if_pending()` — **before** `with_cpu(...)`, and
therefore before the CPU's mailbox drain, its deadline fires and its pick. That
call is not bookkeeping. Its own doc-comment says so:

> it enumerates hot-plugged devices and recovers broken endpoints, and both spin
> on deadlines measured in seconds while holding `XHCI`, which is a ticket
> spinlock and therefore preemption off for its whole life.

The deadline is `xhci::USB_TIMEOUT_NS` = 2 s. `cpu::MAX_PASS_NS`, the budget the
scheduler core asserts against in `feature = "check"` builds, is 200 µs. The two
numbers disagree by four orders of magnitude, and the driver's prologue sits on
the wrong side of the boundary the budget describes.

What a CPU inside that recovery holds is *every message addressed to it*: an
`Adopt` carrying a task, a `Wake` for a parked thread, a `Retire`. Nothing in the
scheduler can shorten it — every reap and every wake is bounded by the owning
CPU's pass latency by design, which is exactly why the design is sound. The one
thing in the tree that notices is `scheduler::retire_task`'s 1 s guard, and it
notices by panicking:

```
retire_task: task not released after 1s: InTransit(CpuId(1))
```

That panic fired on the owner's T14 at 949.792 s of uptime with doom exiting. The
*balance*-path half of it is fixed (spec §7.6.4: `hand_off` reaps a killed task
rather than handing it on, gated by simulator invariant I14). This half is not,
and it would produce the same panic with `Blocked(CpuId(n))` in the message
instead — the guard cannot tell a lost message from a busy CPU, which is what it
is written as if it could.

The second instance of the same shape is the idle loop, which runs
`log_file::poll()` — already recorded in CLAUDE.md as "unbounded and
uninterruptible" — before its `pass()`. On a machine whose log partition is on
the USB stick it booted from, that flush is USB mass-storage I/O on the same 2 s
transfer deadline, and a task adopted onto an idle CPU waits behind it.

Closing this means making xHCI enumeration and endpoint recovery asynchronous, so
that `drain_irqs` only ever does work it can finish: drain the event ring,
dispatch HID reports, note that a port or an endpoint owes work. The debounce and
the port reset were already moved off this path for exactly this reason (CLAUDE.md,
USB hotplug); the control transfers inside `configure` and `recover_endpoints`
were not. Until then, `retire_task`'s bound is measuring the USB bus.

**And the budget cannot see it, twice over.** `cpu::MAX_PASS_NS` is asserted by
`check_pass_duration`, which measures from `SchedPass::begin`'s `now` to the end
of `finish()` — and `drain_irqs()` runs *before* `SchedPass::begin`. The
prologue is outside the window the budget covers, so invariant P would report a
200 µs pass while the CPU had been in the driver for two seconds. Separately,
the assertion is behind `feature = "check"`, whose kernel switch is
`sched-check` (`kernel/Cargo.toml:228`) — and **nothing in `src/` or `tests/`
ever turns it on**, so invariant P has never executed against the kernel in any
image or any test run. Both halves want fixing together: the measured window has
to start where the scheduler entry starts, and the gate has to run somewhere.
