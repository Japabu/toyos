---
status: open
kind: defect
opened: 2026-08-03
---

# The xHCI driver's waits are spins with preemption disabled, wherever they run

`bdf2596` moved the *boundary* — an input read no longer drives the driver — so
the only thread that runs enumeration and recovery now is the one inside
`drain_irqs`. That fixes who pays; it does not change what is paid.

Every wait in this driver is a spin against a wall-clock deadline, taken while
holding `XHCI`, which is a ticket spinlock and therefore preemption off for its
whole life:

- `settles()` — controller halt, HCRST, CNR, R/S, and the port reset. Bound
  `USB_TIMEOUT_NS`, 2 s.
- `wait_command()` and `wait_transfer()` — every command and every transfer.
  Same bound.

**X2a took the two that ran inside a scheduler pass out of that list.** A
teardown's Disable Slot and an endpoint recovery's three-in-a-row (Reset or
Stop Endpoint, Set TR Dequeue, CLEAR_FEATURE(HALT)) are submit-and-return now,
so the six seconds above are reachable only from the boot path and from
`storage_read`/`storage_write` — the first has no scheduler to give a pass back
to, and the second is the case named below that this conversion does not fix.
`device::configure` is the one blocking caller `poll_if_pending` still reaches
and it is X2b's.

So a worst case is a CPU that does not reschedule for **six seconds**, and an
ordinary hot-plug enumeration on the T14 is ~14 ms of it (`hotplug-blocks-a-scheduler-pass`).
Nothing in the suite can measure the bad case: QEMU answers every one of these
in microseconds, which is why a driver built entirely out of them passed
everything here for a season.

**The conversion is the same idiom `PortWork` already uses** — the debounce and
the port reset were spins until #94 and are now states the poll returns to — so
the shape is known and the work is mechanical rather than novel. What makes it
big is its extent: `configure` is a straight line of control transfers, and it
has to become a state machine that gives the pass back between steps.
`restart_endpoint`'s half of that is done: the route is
`toyos_xhci::recovery`'s, driven twice — a blocking loop for a disk's bulk pair,
which runs on the thread that faulted, and a stepped one for HID. **The sequence
is shared and only the drive loop is not**, which is the shape `configure`
should take too.

One case is *not* fixed by that and needs its own answer: `storage_read` and
`storage_write` are called by the page cache on a faulting thread, so a thread
touching a file on a USB disk drives a SCSI command under the same lock. The
input poll was gratuitous and could simply be deleted; this one is inherent, and
the choice is between an I/O thread and making the block layer asynchronous.
