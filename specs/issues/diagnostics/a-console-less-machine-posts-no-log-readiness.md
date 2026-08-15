---
status: open
kind: finding
opened: 2026-08-15
---

# On a machine with no console, nothing ever posts the log's readiness source

`Source::Log` — the io_uring readiness a reader of `SYS_LOG_READ` arms on — is
posted by `klogd` after each drain batch, and by nothing else
(`kernel/src/log/console.rs`, `specs/log-architecture-spec.md` §3.2). That is
the right context and the right cost: it is the one place in the machine that
has just observed committed records and may take a lock, and it costs one wake
per batch rather than one per record.

**But `klogd` does not run at all on a machine with no console.** Its body
drains, then arms its waiter *only* while `serial::has_console()` — with no
backend the drain takes nothing, `DRAINED` never advances, and an armed waiter
would find a committed record on every rescan and never park, which is one
kernel thread spinning for the life of a T14. So it parks unarmed, no producer
pays a post, and nothing wakes it until a console arrives.

The consequence for a log *reader* is that it parks on a source that will never
fire. Nothing hits this today: the one caller is the L4 gate, which runs on a
profile that has a console. It becomes real at L6, when `/bin/logd` is the
program whose whole loop is read-then-park and `/log` is a file a machine with
no serial port still has to fill — the `--diag-boot` shape, and the T14 under
`--mute`.

**What it is not.** It is not an argument for posting from `emit`: §2.6a's
measurement stands, and the watcher list is a `Lock<Vec<RingId>>` the producer
may not touch. The shapes that would answer it are a `klogd` whose arming
condition is "a console **or** a registered log watcher" — which needs a rescan
predicate that is not `DRAINED.any_pending()`, since that one is permanently
true with no backend — or a second, cheaper post from the drain path that a
console-less machine also reaches.
