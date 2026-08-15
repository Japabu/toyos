---
status: open
kind: defect
opened: 2026-08-15
---

# Closing the keyboard claim cancels every process's poll on stdin

`io_uring::remove_fd` cancels **by source, across every ring in the machine**,
so `object::ops::close` asks `ends_its_sources` whether releasing one handle
really ends what that object names (`kernel/src/object/ops.rs`). Two rows answer
`false` for exactly this reason — `SysCap`, whose `Source::Log` outlives every
handle, and `Console`, because every process that has a console has its own
object over the one keyboard.

**`Device(_)` answers `true`, and `Device(Keyboard)` names `Source::Keyboard`
too.** `read_source` maps the keyboard claim and every `Console` to the same
variant, so the claim's holder closing its handle posts `-NotFound` into every
pending `POLL_ADD` on stdin in the machine — a process that holds no device, was
not consulted, and whose read is simply cancelled from under it. libc's terminal
read is what arms those polls, so the blast radius is "every program waiting for
a keystroke".

The comment beside the `true` rows argued from the *object* — "a claim admits
exactly one handle by construction, so every ring watching it is the one
holder's" — and the condition the cancellation actually needs is about the
*source*: that no other **kind** of object names it. The keyboard is the one
place that fails, and it is now stated at the site as the function's residual.

## Why it is quiet today, which is not the same as fixed

The compositor claims the keyboard during boot and holds it until the machine
stops; nothing else can, because a claim is exclusive and carries no `DUP`. So
the only close is at process teardown, by which time the polls being cancelled
belong to a machine that is going down anyway. A compositor that ever
relinquished its claim and re-took it — a restart, a handoff, `SYS_PORT_REARM`
for a daemon — would cancel the terminal reads of every program on the machine
in between.

## What the fix looks like, and why it is not on the branch that found this

The mechanism the rest of the system already has: a source that names its
object. `Source::PipeReadable(PipeId)` is per pipe, so a pipe's cancellation
reaches exactly the rings watching that pipe; `Source::Keyboard` is one global
name for a device every console also reads. Either the keyboard grows a
per-object source the way pipes have one, or `remove_fd` learns to cancel only
the rings whose submission named *this* object.

Found by the strict review of the log branch (PR #82), whose subject is the
console line buffer and `/bin/logd`; the keyboard side is neither. Nothing in
that branch changes the behaviour described here — `Console` answering `false`
is what it added, and this is the row beside it.
