---
status: closed
kind: finding
opened: 2026-08-15
closed: 2026-08-15
---

# On a machine with no console, nothing ever posts the log's readiness source

`Source::Log` — the io_uring readiness a reader of `SYS_LOG_READ` arms on — is
posted by `klogd` after each drain batch, and by nothing else
(`kernel/src/log/console.rs`). That is the right context and the right cost: it
is the one place in the machine that has just observed committed records and may
take a lock, and it costs one wake per batch rather than one per record.

**But `klogd` did not run at all on a machine with no console.** Its body
drained, then armed its waiter *only* while `serial::has_console()` — with no
backend the drain took nothing, `DRAINED` never advanced, and an armed waiter
would find a committed record on every rescan and never park, which is one
kernel thread spinning for the life of a T14. So it parked unarmed, no producer
paid a post, and nothing woke it until a console arrived.

The consequence for a log *reader* is that it parks on a source that will never
fire. Nothing hit it when the syscall first landed: its one caller was that
work's own gate, which runs on a profile that has a console. It became real
with `/bin/logd`, the program whose whole loop is read-then-park, and `/log` a
file a machine with no serial port still has to fill — the `--diag-boot` shape,
and the T14 under `--mute`.

## Closed, and by the shape this entry did not name

This entry listed two candidate answers: an arming condition of "a console
**or** a registered log watcher", which needs a rescan predicate that is not
`DRAINED.any_pending()`; and a second, cheaper post from a path a console-less
machine also reaches. **What was taken is neither — it removes the premise.**

`DRAINED` was permanently behind only because a machine with no backend never
advanced it. `klogd` now advances it anyway (`log::console::discard_pending`):
the records are walked, the position moves, and nothing is rendered because
there is nothing to render to. So `DRAINED.any_pending()` goes false exactly as
it does on a machine with a console, `arm_waiter` is called unconditionally, and
`klogd` parks armed — which means a commit wakes it, which means it reaches
`user::post_readiness` on every machine shape.

**Advancing costs that machine nothing it had.** The records stay in their
shards for the panel, which reads them through `snapshot_committed` and not
through this position; `panic_flush` refuses on `has_console()` before it looks
at the position at all; and a backend arriving later rewinds the whole window
(`backend_changed`). What it adds is one `LOG_WAITER` swap and one `wake_direct`
per `klogd` park on a machine that previously paid neither — the ordinary
steady-state cost of having a reader, now paid without a console because there
is now a reader without one.

The discard is deliberately **not** in `drain_inline`, whose other two callers
are a producer mid-`emit` and a panicking machine: a `Drain::Inline` boot with
no console would then walk every shard per record, which is the cost that mode
is gated on `has_console()` to avoid.

**What is not closed with it.** Nothing here says a userland reader on a
console-less machine has been *observed* keeping up — the shipped reader is
`/bin/logd` and the machine shapes that have no console are the `--diag-boot`
image and the T14 under `--mute`, neither of which is a gate on this host. That
is the metal session's
(`issues/hardware/a-metal-session-runs-a-pre-flash-gate-first.md`).
