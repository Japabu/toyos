---
status: open
kind: defect
opened: 2026-08-06
---

# `i8042_mouse`: the host outran QEMU's PS/2 queue, twice over

Both red modes are fixed and both were the harness. Neither was ever a packet
the driver lost.

**The count.** `MOUSE_LEAD` let the host hold 32 packets — 96 bytes — injected
but unreported, and justified it against "a 256-byte ring in the kernel and
QEMU's PS/2 buffer above it". That 256 is `PS2_BUFFER_SIZE`, the migration
array; the enforced capacity is `PS2_QUEUE_SIZE`, **16 bytes**
(`hw/input/ps2.c`, checked against QEMU v11.0.0, the version this host runs).
`ps2_mouse_send_packet` emits only while `PS2_QUEUE_SIZE - count >= 3` and
returns 0 otherwise, and `ps2_mouse_event` keeps accumulating `mouse_dx` while
it does — so a host past the queue does not lose a packet, it makes QEMU **sum**
several into one. The burst alternates +1/-1, so a summed pair is a packet with
`dx == 0`, and `mouse::handle_motion` queues nothing for a report that moves the
pointer nowhere with no button change. **Two injected, none delivered** — which
is why every observed shortfall was even (996/1004, 1002/1004) and why the
stalls sat at a deficit of exactly 32: losses accumulate until the lead is full
and the host never injects again.

Reproduced by pipelining QMP commands without awaiting their replies, which is
what an oversubscribed host does to the vCPU thread by accident. Floods of
4/8/16/32/64/128/256 back-to-back packets, two sweeps in one boot:

    [4, 8, 16, 32, 62, 128, 256, 4, 8, 16, 32, 64, 126, 256]
    [4, 8, 16, 32, 62, 128, 256, 4, 8, 16, 32, 64, 100, 256]

and in an earlier boot a 32 that delivered 18. Paced injection on a quiet host
never merged at any lead, including no pacing at all — which is exactly why this
only ever reddened under contention, and why a branch's kernel had nothing to do
with it. The last sighting before the fix was a landing gate on a branch whose
only diff was a crate nothing compiles and two documents: a byte-identical
kernel, red when re-run **alone**.

The fix is `MOUSE_LEAD = 4`, with the device's queue named in code and a `const`
assert that `MOUSE_PACKET * MOUSE_LEAD <= QEMU_PS2_QUEUE`. Raising it to 6 stops
the harness compiling, with that sentence as the message. The premise it rests
on — that QEMU sums motion between syncs rather than dropping it — is staged in
the run itself: `MERGE_MOTIONS` moves in one `input-send-event` must come back
as one packet of that many steps. Making `mouse_merged` send one command per
move instead reds it (1012 events for 1009 packets), so the stage has teeth.
Cost: the injection takes about 2× the guest time it did (578 ms → 1313 ms
measured alone), against a 60 s failure mode removed.

**The lost edge.** `service` reads the source's `irq_ring` record, then reads
the byte ring. The ISR fills the byte ring *before* it publishes its record, so
an interrupt landing between those two reads leaves the pass holding bytes it
has been told nothing about — and it counts a lost edge that never happened. The
record it left standing is taken by the next pass, which finds nothing to drain,
so nothing ever corrects the count. `service` now asks again once the bytes are
in hand.

Unwidened the window is a handful of instructions on one CPU, so nothing outside
the kernel reaches it — hence the `i8042-edge-race` actuator, which holds the
pass between the two reads. With it on and the fix reverted the counter reports
**116 and 127** false lost edges on one run of `i8042_mouse` and the test reds;
with the fix, 0 across every i8042 test. It is bundled into `i8042-trace`, so
the group that reads those counters is the group that runs it.

Both `Sched::Parallel` classifications stand: neither verdict is a rate, now for
a reason that is checked rather than argued.
