# soundd: client liveness, and decoupling ring geometry from the device

Two staged changes to soundd. Neither is landed. Both change behaviour that the
audio gate measures, and the first is blocked on a fork this session could not
edit. Written up now so the design survives the session that found it.

Read `specs/audio-subsystem-spec.md` first; section numbers below are its.

---

## 1. Client liveness

### What is wrong

soundd infers a dead client from a pipe error. Each cycle it writes one byte to
every client's signal pipe and reads the result:

- `Err(NotFound)` — the kernel's broken-pipe error. The client's process is
  gone. Caught, ramped down, removed.
- `Err(WouldBlock)` — the pipe is full. Treated as "merely behind".

That is the whole mechanism, and it detects exactly one failure: the client
*process exiting*. A client that stops making progress while still holding its
fd — a wedged thread, a deadlock, a callback that never returns — produces
`WouldBlock` forever and is indistinguishable from a slow one.

### Why it is not a soundd bug

The ambiguity is specified. §6.4 defines pause as:

> the stream thread stops reading from the signal pipe. soundd sees an empty
> slot ring each cycle and mixes silence. **No explicit coordination required.**

A paused client and a wedged client are therefore *defined* to look identical
from soundd: pipe fills, `write_idx` stops advancing, no data in the ring. No
change confined to soundd can separate them, because the information does not
exist on soundd's side of the boundary. Confirmed in the cpal backend, where
pause is purely local:

```rust
fn pause(&self) -> Result<(), PauseStreamError> {
    self.state.store(STATE_PAUSED, Ordering::Release);
    Ok(())
}
```

The stream thread then parks on a futex on that word. soundd is never told.

### What it costs today

`ClientStream::is_streaming()` is `delivered && !pending_removal` — latched by
the first period a client ever supplies and never cleared. So a client that has
delivered once and then wedges *or pauses* keeps `any_streaming` true forever,
and soundd never reaches the idle state. §5.8 promises of the no-client case:

> Zero overhead, zero wakes, device voice closed

A single paused client defeats that for the life of the process: a wake per
period forever, the DMA engine running, the codec voice held open. On the T14
that is battery. It also pins the client's shm, signal pipe and slot ring,
which compounds the per-client shm leak (audit defect 2 — no `destroy` syscall
exists, so those regions are already unreclaimable until soundd exits).

### Design

Liveness is `declared intent` × `observed progress`. soundd already has
progress: `write_idx` in the shared header. What is missing is intent, and it
has to come from the client.

Add one `AtomicU32` on its own cache line of `AudioSlotHeader`, written only by
the client and read only by soundd:

| value | meaning | soundd's deadline |
|---|---|---|
| `OPENING` (0) | mapped, not yet producing | bounded open timeout |
| `STREAMING` (1) | intends to supply every period | wedged if `write_idx` is unchanged for N periods |
| `PAUSED` (2) | deliberately silent | none; not counted by `any_streaming` |

Zero is `OPENING` so a freshly allocated (zeroed) region is correct before the
client touches it. Relaxed stores; the ordering that matters is already carried
by `write_idx`'s release.

This is a strict improvement on three axes at once:

- A wedged client is detected and removed instead of being carried forever.
- A paused client stops holding soundd out of idle, which is what §5.8 already
  promises and does not deliver.
- `ClientStream::signal_read_fd` — the read end soundd holds until the client's
  first delivery, purely so §5.7's crash detection cannot fire early — becomes
  unnecessary. `OPENING` says the same thing without a second fd, and says it
  without the "a client that fills without ever opening the pipe loses its
  stream" edge the current dance carries.

`N` should be expressed in periods, not milliseconds, and must exceed the
deferral window the mix loop already tolerates (§5.9). It is a real number to
justify against measurement, not to invent here.

### Why it is not landed

It needs `pause()`/`play()` in the cpal fork to store the new state, and the
fork could not be edited this session: it exists only as a cargo git checkout,
so editing it needs a clone beside the monorepo plus `.cargo/config.toml` path
overrides — a file shared by every agent in this working tree — and a push to
the fork repo.

**Landing the soundd and SDK halves alone is worse than not landing them.**
Every cpal client that pauses would declare `STREAMING`, stop producing, and be
killed by the new deadline. That is a regression, and an audible one.

Order: cpal fork first, then SDK, then soundd, then §6.4 and §7.3 of the spec.
The spec change is not cosmetic — §6.4's "no explicit coordination required" is
the sentence this whole design deletes.

### 1a. Suspending on no progress — measured, and the answer is no

The tempting half of §1 is that *suspending* does not need the paused/wedged
distinction: both are "not producing", and closing the voice serves both
correctly. Only *killing* needs the distinction, and only killing is blocked on
the fork. So: is suspend-on-no-progress landable on its own?

**No. There is no wake edge on resume.** soundd's wait has exactly two sources
plus a timeout:

```rust
let timeout = if streams.is_empty() { u64::MAX } else { /* DLL prediction */ };
poller.poll_add(&audio_dev, IORING_POLL_IN, TOKEN_AUDIO);   // device completions
poller.poll_add_fd(cmd_pipe_read, IORING_POLL_IN, TOKEN_CMD); // control thread
poller.wait(1, timeout, ...);
```

`TOKEN_CMD` carries exactly three commands — `AddClient`, `RemoveClient`,
`SetVolume` — submitted by the control thread when it receives
`MSG_STREAM_OPEN`, `MSG_STREAM_CLOSE` or `MSG_STREAM_SET_VOLUME`. **None of them
corresponds to resume.**

The client side confirms it. `AudioStream::wait_and_fill` *blocks reading the
soundd→client signal pipe* and then fills slots with a shared-memory store;
there is no client→soundd traffic in the steady state at all. cpal's `play()`
stores `STATE_PLAYING` and futex-wakes its own thread, which then blocks on
that read. Nothing crosses to soundd.

So a suspended soundd, with the device stopped and `timeout = u64::MAX`, waits
for completions that will never come and a command nobody will send, while the
resumed client blocks forever on a signal byte soundd is no longer writing.
That is a battery defect traded for permanent silence — strictly worse, and
exactly the trade that needs the owner's sign-off and would not get it.

**The missing edge, written down and not built:** a client→soundd resume
notification. The natural home is the control connection, because the wake path
already exists — `TOKEN_CMD` wakes a *fully idle* mix loop today, which is how a
new client connecting wakes soundd out of `timeout = u64::MAX`. The mechanism is
there; only the message is missing, and it must be sent from cpal's `play()`.

That consolidates the roadmap: **killing wedged clients, suspending on no
progress, and resuming from suspend all need the same one client→soundd edge.**
One fork change unblocks all three. They are not three problems.

#### The fork-independent variant, and why it is still not landable today

There is one shape that keeps resume working without any client change: stop the
*device voice* but keep the periodic timer wake, so soundd goes on writing
signal bytes and sees the resumed client's slots on the next cycle. That
recovers the DMA engine and the codec — the battery-relevant hardware — and
gives up only the wake itself.

It is soundd-only, so it is not blocked on the fork. It is blocked on the gate:
stopping and restarting the device mid-session introduces a restart transient
and a DLL re-lock, which is audible output changing, which is the thorough tier
and a quiet tree. Recorded here as the option that becomes available first if
the quiet window arrives before fork access.

---

## 2. Decoupling client ring depth from the device pipeline

### What is wrong

```rust
// Client ring depth matches the DMA pipeline depth: a wake gap can free
// at most num_buffers periods, so a full client ring always covers it.
let slot_count = num_buffers as u32;
```

`num_buffers` is the kernel driver's `TX_INFLIGHT_MAX`, handed over in
`AudioInfo`. So a device with a different pipeline depth silently changes the
ring geometry of every client — the shm size, the number of slots a client
fills per wake, and the latency floor — through a constant no client ever sees
and no client can influence.

The reasoning quoted in the comment is sound for the *lower bound*: a wake gap
can free at most `num_buffers` device periods, so a ring shallower than that
cannot cover a worst-case gap. It does not justify *equality*. It gives
`slot_count >= num_buffers`, and equality was assumed.

Two asserts sit on the same coupling and are worth naming, because they turn a
device property into a soundd startup panic:

```rust
assert!(num_buffers.is_power_of_two(), ...);   // ring indices wrap mod 2^32
assert!(num_buffers > 5, "soundd: pipeline too shallow to defer safely");
```

A device offering six buffers is fine; one offering four takes soundd down at
startup. That is a device shape, not a bug in the device.

### Design

Separate the two numbers and let each be derived from what actually constrains
it:

- **`slot_count`** is a *client* property: how much buffering the client wants
  between its callback and soundd. Its constraints are `>= num_buffers` (cover
  a worst-case wake gap), a power of two (ring indices wrap mod 2^32), and an
  upper bound from the shm the client is willing to pin.
- **`num_buffers`** stays a *device* property and stops leaking into client
  geometry.

`slot_count` is then negotiated in `MSG_STREAM_OPEN`/`MSG_STREAM_OPENED`, which
already carries the format negotiation, with soundd clamping the client's
request to `[num_buffers.next_power_of_two(), MAX_SLOTS]`. A client that asks
for nothing gets today's value, so the default is unchanged.

The two asserts become clamps against the same bound, and `num_buffers <= 5`
stops being fatal: the deferral window is what needs the depth (§5.9), and with
`slot_count` decoupled the client ring can supply it on a shallow device.

### Why it is not landed

Instructed not to, and the instruction is right: this changes ring geometry and
therefore the latency and wake pattern the audio gate measures. It lands in a
quiet window with the gate behind it, and it is a **scheduler-migration-shaped
change** — the thorough tier (`--audio-gate N`) is the honest instrument for
it, not the fast tier. `specs/assessments/audio-gate-history.md` is explicit that these
counters drift between batches on one host with no code change, so only
same-session A/B numbers mean anything.

---

## Order

1. cpal fork: `pause`/`play` store the shared state. Unblocks everything in §1.
2. SDK + soundd + spec §6.4/§7.3: the liveness state machine.
3. Ring-depth negotiation, with the thorough audio gate as its A/B.

§2 is independent of §1 and can go first if the quiet window arrives before
fork access does.
