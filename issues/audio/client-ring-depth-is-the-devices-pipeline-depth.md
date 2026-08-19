---
status: open
kind: track
opened: 2026-08-01
---

# Every client's ring geometry is the device's pipeline depth

soundd sets `slot_count = num_buffers`, the kernel driver's `TX_INFLIGHT_MAX`.
So a device with a different pipeline depth silently changes the shm size, the
number of slots a client fills per wake, and the latency floor of *every*
client, through a constant no client sees and none can influence.

The reasoning behind the coupling is sound for the lower bound only: a wake gap
can free at most `num_buffers` device periods, so a ring shallower than that
cannot cover a worst-case gap. That gives `slot_count >= num_buffers`. Equality
was assumed.

**What to build.** `slot_count` becomes a client property negotiated in
`MSG_STREAM_OPEN`/`MSG_STREAM_OPENED`, which already carries the format
negotiation, with soundd clamping to `[num_buffers.next_power_of_two(),
MAX_SLOTS]`. A client that asks for nothing gets today's value, so the default
is unchanged. `num_buffers` stays a device property and stops leaking.

**Blocked on the audio gate, not on anything in soundd.** This changes the
latency and wake pattern gate A measures, so it lands in a quiet window with the
thorough tier behind it as a same-session A/B — the fast tier cannot see it, and
these counters drift between batches on one host with no code change at all.

Half of the original coupling is already gone: the `num_buffers > 5` startup
panic became `deferral_floor_nanos` returning `None`, and an unrenderable shape
is refused by name rather than asserted.
