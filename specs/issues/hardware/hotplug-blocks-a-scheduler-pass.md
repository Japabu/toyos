---
status: open
kind: defect
opened: 2026-08-03
---

# The hotplug enumeration blocks a scheduler pass, and its debounce keeps a CPU awake

Both are the price of `poll_if_pending` being the only context the driver has,
and both are bounded and paid only by a machine somebody has just plugged into.

**The enumeration.** `device::configure` runs inline: Enable Slot, Address
Device, three or four control transfers, Configure Endpoint. Under TCG it is
microseconds — the whole hotplug sequence in `xhci_hotplug` is inside one
millisecond of guest time — so nothing in the suite can measure the real cost.
The one hardware figure there is says the T14's five boot-time devices took
346 ms including 5×55 ms of port reset, so roughly **14 ms each** for everything
`configure` does (`specs/metal-hardware-inventory.md`). That is a scheduler pass
of that length on the CPU that services the plug, with preemption disabled under
the `XHCI` lock — the same order as `log_file`'s flush, which `specs/issues/boot-media/` measures at
2.0–9.7 ms and calls out for the same reason. The port reset was the dominant
term and is already out of it; taking the rest out means a state machine over
the control transfers, which is the whole enumeration path rewritten.

**The debounce.** `PORT_WORK_AT` keeps a CPU with nothing to run out of `hlt`
until the port's deadline, because nothing else would bring it back: the connect
edge was the last interrupt the controller had to give, and the scheduler arms
its one-shot for parked *tasks*. It is a deadline rather than a flag, so the
`XHCI` lock is taken once when it expires and not by every CPU on every pass for
the length of it — but every *idle* CPU declines to halt for the interval, which
is 100 ms for an ordinary plug and up to the 2 s transfer deadline behind a port
that will not reset. Power, never latency: `Action::Idle` is reached only when
there is nothing runnable, and this decides whether to sleep and nothing else.

What would remove both is a way for a driver to ask the scheduler for a deferred
callback at a deadline — which is also what `i8042::verdict_due` and
`log_ring::file_has_pending` are working around in the same condition. That is a
scheduler-core addition and wants the owner's sign-off.
