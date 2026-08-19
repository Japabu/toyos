---
status: open
kind: track
opened: 2026-08-01
---

# A client cannot tell soundd it paused, so soundd cannot tell paused from wedged

soundd infers a dead client from a pipe error: `NotFound` is a gone process,
`WouldBlock` is "merely behind". That detects exactly one failure. A client that
stops making progress while holding its fd — a wedged thread, a deadlock, a
callback that never returns — produces `WouldBlock` forever and is
indistinguishable from one that deliberately paused. The ambiguity is not a
soundd bug: pause is defined as the client's stream thread simply not reading,
with no coordination, and the cpal backend confirms it — `pause()` stores a
state word and parks on a futex, and soundd is never told.

The cost is that `is_streaming` latches on the first delivered period and never
clears, so one paused client holds soundd out of idle for the life of the
process: a wake per period forever, the DMA engine running, the codec voice
open, and the client's shm, signal pipe and slot ring pinned.

**Design, as far as it is settled.** Liveness is declared intent × observed
progress. Progress already exists as `write_idx`; intent does not. One
`AtomicU32` on its own cache line of `AudioSlotHeader`, written only by the
client: `OPENING` (0, so a zeroed region is correct before the client touches
it), `STREAMING` (wedged if `write_idx` is unchanged for N periods), `PAUSED`
(not counted by `any_streaming`). Relaxed stores; `write_idx`'s release already
carries the ordering. N is in periods, not milliseconds, and must exceed the
deferral window the mix loop tolerates — a number to measure, not to invent.
`OPENING` also makes `signal_read_fd` unnecessary.

**Blocked on the cpal fork**, which needs a clone beside the monorepo and
`.cargo/config.toml` path overrides — a file every agent in a working tree
shares. Landing the soundd and SDK halves alone is *worse* than not landing
them: every cpal client that pauses would declare `STREAMING`, stop producing,
and be killed by the new deadline. That is an audible regression.

**One edge unblocks three things.** Suspending on no progress is not landable on
its own either, and the reason is worth keeping: soundd's wait has exactly two
sources plus a timeout, and its command channel carries only add/remove/set-volume
— **there is no resume message**. A suspended soundd with the device stopped and
`timeout = u64::MAX` waits for completions that never come while the resumed
client blocks forever on a signal byte nobody writes: a battery defect traded
for permanent silence. Killing wedged clients, suspending on no progress and
resuming from suspend all need the same client→soundd notification, sent from
cpal's `play()`.

The one variant that needs no client change is filed apart, at
`issues/audio/stop-the-device-voice-keep-the-wake.md`, because it could land
first.
