# Audio

## 1. Requirements

1. **Server-pull.** soundd owns the device clock and is the sole submitter of
   device buffers. Clients never initiate a cycle; they fill their rings when
   signalled.
2. **Multiple simultaneous clients**, each with its own sample rate, channel
   count and format; soundd converts and mixes additively. Bounded: at most
   `MAX_CONTROL_CLIENTS` (63) at once — the derivation lives on the constant
   in soundd (a power-of-two poller ring minus the acceptor, past what the
   mixer renders in one period, 189 of the kernel's 1024 fds) — and the
   64th connect is refused, never queued: an unbounded acceptor is a
   resource-exhaustion path.
3. **A missed deadline produces silence for the client that missed it**, and
   nothing else: no stall, no artifact for other clients, no effect on the
   pipeline.
4. **Stream transitions are inaudible.** Every connect, disconnect, volume
   change and device stop/start passes through a gain ramp or digital zero;
   no path steps the output discontinuously.
5. **Quantization to the device format is dithered.** The mix bus carries
   more precision than the device format, and the noise floor does not
   modulate with signal level.
6. **soundd always runs and always accepts streams.** A machine with no audio
   device gets a null sink (§6): stream negotiation, write timing and
   backpressure are identical with and without hardware.
7. **Glitch-free audio on one contended CPU is a supported configuration**,
   carried by the scheduler's real-time band and pipe priority inheritance
   (scheduler spec).

## 2. Architecture and timing

```
┌──────────┐  ┌──────────┐  ┌──────────┐
│ client A │  │ client B │  │ client C │   each with its own rate,
└────┬─────┘  └────┬─────┘  └────┬─────┘   channels and format
     │ shared ring + signal pipe (§3, §4) │
     ▼             ▼             ▼
┌──────────────────────────────────────────┐
│                  soundd                  │  owns the device clock;
│   converts · mixes · dithers · submits   │  signals, then reads (§4)
└─────────────────────┬────────────────────┘
                      │ kernel audio interface (§8)
                      ▼
┌──────────────────────────────────────────┐
│                  kernel                  │  buffer memory, submit,
│                                          │  timestamped completions
└─────────────────────┬────────────────────┘
                      ▼
               ┌──────────────┐
               │ audio device │   one native configuration
               └──────────────┘
```

The device runs at one native configuration — sample rate, channels, format,
period size, buffer count — negotiated once from its capabilities. The
**period** is the cycle: once per period soundd wakes, reads client rings,
mixes, and submits one buffer per free device slot. The device's in-flight
buffers form the **pipeline**; its depth absorbs scheduling jitter and does
not add to a stream's latency.

A client at another rate fills
`ceil(device_period_frames × client_rate / device_rate)` frames per cycle;
soundd resamples during mixing.

soundd wakes on both the device completion interrupt and a predicted timeout,
and the interrupt is the primary edge: the prediction covers a completion
that does not arrive, and an early completion wakes soundd immediately. The
prediction tracks the device's own completion timestamps, which the kernel
takes at interrupt time (§8). If all buffers ever drain, the prediction is
discarded and re-learned from the next completion: the device restarts its
period grid from whatever is submitted next.

Refill after a drain is proportional to what drained: free buffers are filled
through the ordinary mix path, so client audio already in the rings goes out
in the first refilled buffers, and silence is submitted only for periods no
client covers.

## 3. The shared ring

Each stream is one shared memory region mapped into the client and soundd: a
128-byte header followed by `slot_count` slots of one client period each.

| offset | size | content |
|---|---|---|
| 0x00 | 4 | `write_idx`: 32-bit atomic, written by the client only |
| 0x40 | 4 | `read_idx`: 32-bit atomic, written by soundd only |
| 0x80 | `slot_count × client_period_bytes` | PCM slots |

Each index sits alone in a 64-byte block so the two writers never share a
cache line. The indices are free-running; a slot's position is the index
modulo `slot_count`. Filled and unconsumed: `write_idx − read_idx`. Free for
the client: `slot_count − (write_idx − read_idx)`. The client advances
`write_idx` with release ordering after filling a slot; soundd reads it with
acquire ordering, and symmetrically for `read_idx`. One writer per index, so
neither side ever locks.

`slot_count` is not in the header: it is delivered once in
`MSG_STREAM_OPENED`. It equals the device pipeline depth, so a client that
keeps up can cover any stall the pipeline itself can absorb.

## 4. The cycle

```
time ──────────────────────────────────────────────────────────►

soundd:  wake ── signal clients ── wait ── consume ── mix ── submit ── sleep
                    │                ▲
                    │                │ fills happen inside soundd's wait,
client:             └── wake ── fill slots ── block   marked urgent by the wake
```

Once per period, in order:

1. soundd writes one byte to every client's signal pipe.
2. soundd waits up to one period for clients to fill.
3. soundd consumes filled slots — one per free device buffer per client —
   converting rate, channels and format, applying gain, and mixing
   additively.
4. soundd dithers, quantizes to the device format, and submits.

The signal precedes the read, which gives clients the whole period to fill.
soundd's wake **marks a signalled client urgent** (`specs/scheduler-core-spec.md`
§3, amended to the urgency mark by `specs/scheduling-reservations-spec.md`
§1.8): the client is dispatched ahead of unmarked threads inside its own
scheduling class, for a bounded window, so it starts filling promptly even on
one loaded CPU rather than queueing behind whatever else that class is running.
The mark moves no budget — the fill is charged to the client's own class, never
to soundd's reservation — so a client that spins in its window delays nobody but
its own class's threads, and soundd's own mix keeps the budget it was admitted
with. What the mark does *not* promise is the whole of a fill: it ends at the
first of the window expiring, the client blocking, or the wait that raised it
ending, and a client whose fill outlives it finishes at its class's ordinary
rate, inside the deferral window below.

**Deferral.** soundd may hold a free buffer back for a client that has not
finished filling, but only while at least five periods of unplayed audio
remain in the pipeline: of eight periods it waits on clients for at most
three and keeps five in reserve. On a pipeline of five or fewer buffers the
deferral policy is disabled and every free buffer is mixed immediately.

**Gain.** Every gain change — connect (0 to target), disconnect (current to
0), volume — is a linear ramp over 5 ms, interpolated per sample. A
disconnecting client is removed only after its ramp completes. A gain
received outside [0.0, 1.0], or not a finite number, is clamped to the
range's nearest bound.

## 5. Idle

soundd quiesces the device when no stream exists. The mix loop is always in
exactly one of three states:

| state | predicate | submits | waits on |
|---|---|---|---|
| STREAMING | streams exist | one buffer per free slot | completions, control messages |
| DRAINING | no streams, buffers in flight | nothing | remaining completions, control messages |
| SUSPENDED | no streams, pipeline empty | nothing; the device is stopped | control messages only |

- STREAMING → DRAINING when the last stream's ramp-out completes; the
  in-flight periods are dithered silence.
- DRAINING → SUSPENDED on the completion that frees the last buffer; the
  prediction state is discarded and the device stopped. The device is never
  stopped with buffers in flight.
- DRAINING → STREAMING when a stream arrives before the drain finishes; the
  device was never stopped.
- SUSPENDED → STREAMING when a stream arrives: the ordinary mix path refills
  the pipeline and the first submit restarts the device. Every stop/start
  passes through digital zero.
- Boot enters SUSPENDED.

While SUSPENDED soundd holds no timer and takes no wakes. The idle predicate
is "no stream open", not "all streams silent": a paused stream keeps the
device running, because resuming writes only shared memory and produces no
event to wake on.

## 6. The null sink

When no audio device exists, soundd serves streams from a null sink instead
of exiting: exiting would leave every client's connect refused for the
machine's lifetime.

A device whose **shape** the mixer cannot render a period into gets the same
answer for the same reason, and never a panic: a pipeline outside 2..16 periods
or one that is not a power of two (§3's indices are free-running mod 2^32), a
device that is neither mono nor stereo, or a period that is not a whole number
of frames. soundd names which of those it found and presents the null sink.

- It presents one fixed configuration — 44100 Hz stereo i16, 128-frame
  periods, an 8-slot ring — and negotiates streams identically to a device.
- One period is consumed per period of wall clock, so a client's ring drains
  and its writes backpressure at exactly the audio rate; the samples are then
  discarded. Writing N seconds of audio takes N seconds of wall clock.
- There is no device pipeline, no prediction, no dither and no submit. A wake
  late by more than the ring depth re-anchors the period grid.
- It does not enter the real-time band: the band protects audible output and
  a discard has none.
- It follows §5's idle discipline and reports discarded streams in the same
  statistics a real sink reports.

A real sink replaces the null sink only across a reboot; there is no
hot attach.

## 7. The stream protocol

**Open.** The client opens its endowed connection and sends
`MSG_STREAM_OPEN { sample_rate, channels, format }`. soundd allocates the
ring, creates the signal pipe, and answers with the shared memory and the
pipe's read end followed by
`MSG_STREAM_OPENED { client_period_frames, client_period_bytes,
device_sample_rate, device_channels, slot_count }`. The stream starts at gain
0.0 and ramps to 1.0. A format the mixer cannot convert is refused with an
error; soundd never accepts a format it would render incorrectly.

**The client's obligations.** On each signal byte the client may fill any
free slots, advancing `write_idx` per slot as §3 requires; it must fill
within one period per slot to avoid its own silence (requirement 3). Missing
a signal is harmless: the unread byte returns the next read immediately.
Pausing is simply not reading the signal pipe — soundd mixes silence from an
empty ring; resuming is reading again. Closing is `MSG_STREAM_CLOSE`, after
which soundd ramps out, then releases the ring and pipe.

**Volume.** `MSG_STREAM_SET_VOLUME { gain }`, applied through the ramp with
§4's validation.

**Departure.** A stream ends in one of four ways, and soundd reports the one
it established rather than the one it guessed: the client's own
`MSG_STREAM_CLOSE`; a refusal soundd issued; the control connection ending
without a close; or the signal pipe breaking under the next signal write. The
last two are the same event seen by soundd's two threads and they race, so the
word waits until the stream is dropped and the strongest witness by then wins
— the control thread read the peer, the mix loop only found a descriptor gone.
Whichever fires first ramps the stream out, draining any audio still in the
ring, and removes it. No other stream is affected; soundd never blocks on a
client.

**A crash and a clean exit are not distinguishable here**, and soundd does not
claim to tell them apart: they close the same descriptors the same way, and the
exit code is the kernel's `exit:` line to report.

## 8. The kernel interface

The kernel exposes an audio device to soundd as:

- **Open**: returns the native configuration and the device buffer geometry,
  and maps the physically contiguous buffer memory into soundd. soundd writes
  mixed periods directly into these buffers.
- **Start / stop.**
- **Submit**: enqueue one filled buffer by index and length.
- **Completions**: delivered to a waiting soundd before it can re-enter its
  wait, each carrying the finished buffer set and a monotonic timestamp taken
  at interrupt time.

The scheduler supplies the real-time band (entry capability-gated), pipe
priority inheritance, and the completion-before-re-block delivery above
(scheduler spec).

## 9. Failure semantics

| failure | behavior |
|---|---|
| Client misses its deadline | Silence for that client's missed periods; automatic catch-up next cycle |
| Client leaves, however | Ramp-out, removal named by §7's strongest witness; others unaffected |
| Completions arrive batched | Multiple slots consumed that cycle; the ring absorbs it |
| Scheduling jitter | Absorbed by the prediction and the pipeline depth |
| Every buffer drains | Prediction re-learned; refill proportional (§2); client audio resumes in the first refilled buffer |
| Last client leaves | Drain, then device stop (§5); zero wakes while idle |
| No device at boot | Null sink (§6); clients play to completion |
| A device shape the mixer cannot render | Named and refused; null sink (§6). Never a panic |
| Device reports an error | Logged; the device is reopened; streams persist through the reopen |
| Gain out of range or not finite | Clamped (§4) |

No failure mode deadlocks or permanently stalls: the worst case is bounded
audible silence, recovered without intervention on a later cycle.
