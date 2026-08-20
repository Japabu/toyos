---
status: open
kind: defect
opened: 2026-08-20
---

# `kill_while_blocked` reds on `main`: a killed peer still takes a write

`tests/toyos-rust-tests/src/bin/kill_while_blocked.rs` kills a child that is
parked in a blocking read and then asks the child's peer end whether it knows.
Two of its four arms ask that, and **both ride one mechanism**, so either can be
the one that reds:

- arm 1, `kill_while_blocked.rs:152` — `a pipe whose only reader was killed
  mid-read still took a write`;
- arm 2, `kill_while_blocked.rs:178` — `a connection whose peer was killed
  mid-read still took a write`, `left: Ok(22)`, `right: Err(NotFound)`.

The mechanism under both is one chain: the victim's handle goes at
`ops::close_all`, `handle_count` reaches zero, `HandleEntry`'s drop queues the
object, and the object's `on_zero_handles` — `PipeReadEnd`'s or
`ConnectionEnd`'s — is what calls `Held::release` and gives the `PipeReader`
back. Only then is `pipe.readers` zero, which is the *only* thing
`pipe::try_write` reads before answering `PipeWrite::BrokenPipe`, which
`ops::write_pipe` turns into `NotFound`. Until the hook has run, the peer's
write is an ordinary successful write into a ring nobody will ever read.

## It is `issues/kernel/deferred-release-outlives-its-syscall.md`, in pipe shape

The hook does not run at the drop. It runs from `object::drain_zero_handles`,
and the drain the killing `SYS_PROCESS_KILL` makes on its way out of the kernel
is what normally runs it before the killer gets back to userland. That drain
clears `ZERO_PENDING` before it runs a single hook, so a batch another CPU has
taken is indistinguishable from an empty queue — the killer returns with the
victim's read end still held, and the next write is `Ok(22)`.

**This is the first consequence of that defect that is not a memory reading.**
Everything filed against it before was `SYS_SYSINFO` deficits and census
counts — quantities that settle. This one is a syscall answering the wrong
word: `Ok(n)` where the ABI says `NotFound`, with the bytes going nowhere. The
same shape reaches soundd, which `kill_while_blocked`'s own module header
names: a cpal client killed while parked in its signal-pipe read leaves soundd
writing into a ring nobody reads.

## Measured, 2026-08-20, dev host, tree `e4c2c8ff` (`main`'s tip), unmodified

The filing measurement, which established that it is `main`'s and not a
branch's — same-session A/B, `cargo test --test toyos-build --
kill_while_blocked`, five runs each:

| tree | runs | red |
|---|---|---|
| `e4c2c8ff` (`main`'s tip) | 5 | **1** |
| `47892284` (the `toyos-mixer` branch, main merged in) | 5 | 0 |

It also red once inside a full 272-test fast-tier run on that branch, which is
how it was found. Then, on the same tree with nothing modified:

| what was run | runs | red |
|---|---|---|
| `cargo test --test toyos-build -- kill_while_blocked` | 53 | **2** |
| `cargo test --test toyos-build` (full fast tier, 272 tests) | 4 | 0 |
| the same one-name run on a kernel carrying a log line per release | 20 | 0 |

**The two reds are one on each arm**, which is the strongest evidence that the
arms are one mechanism: the first was arm 1 at `:152`, the second arm 2 at
`:178` — and on the second boot arm 1 had already printed `pipe: the write end
learned its reader had gone` before arm 2 failed. Which end the steal catches is
luck.

Both were in the wide phase and the harness's own re-run was green both times.
That `ALONE` line reads *"GREEN — it fails only beside other guests, so its
`Sched::Parallel` is wrong"*, which is the wrong conclusion for this defect and
is already ruled out for its sibling name: `src/redlist.rs`'s `handle_lifetime`
row records **`Sched::Serial` would have retired nothing**.

The last row is worth as much as the first two: one `log!` per release closes
the window, so the residual between the kill returning and the peer's write is
of the order of a few log lines' work. **No count here is its real rate** — a
race shows up more often beside 271 other guests than in a one-test run, and the
four full tiers that did not fire are four, against a first sighting that was
inside one.

## The mechanism, staged and traced

Removing the syscall-exit drain (`kernel/src/arch/syscall.rs`, one statement)
stages exactly the state a stolen batch leaves: the killing syscall returns with
its objects unreleased. **4 red of 5**, against 2 of 53 unmodified, and the
trace on the same boot names the two CPUs:

```
[kernel 0.535 cpu0] KILLPROBE enter target=8
[kernel 0.541 cpu0] ZQPROBE enqueue PipeRead  koid=36
[kernel 0.541 cpu0] ZQPROBE enqueue PipeWrite koid=39
[kernel 0.542 cpu0] KILLPROBE done  target=8
[kernel 0.544 cpu1] ZQPROBE take batch=2
[kernel 0.544 cpu1] ZQPROBE ran PipeRead koid=36
```

`koid=36` is the victim's stdin read end. The kill returns on cpu0 at 0.542 and
the release runs on **cpu1** at 0.544; the parent's `write_all` lands in those
2 ms and is accepted. The probes were `log!`s in `kill_process` and in
`enqueue_zero_handles`/`drain_zero_handles`, and are not in the tree.

## Two things the first filing guessed, and what they turned out to be

**"The pipe path publishes the death before the connection path does."** No —
the arms are one mechanism, and **both of them reded** in the session above, the
pipe one first. Which arm reds is which release the steal caught, not which path
is faster. Arms 1 and 2 are separate children and separate kills, so one passing
beside the other failing says nothing about either path.

**"`main` moved twelve commits on 2026-08-20, three of them in the object and
memory layers."** `git diff 625afce1 e4c2c8ff -- kernel/` is comments, doc
comments and one `#![warn(clippy::undocumented_unsafe_blocks)]` attribute:
**no behaviour change at all**, in the kernel or anywhere else. The race cannot
be new there. `ZERO_QUEUE` and `ZERO_PENDING` arrived with `6c39b1b4`
(2026-08-09, the object layer); this test arrived with `8f74272d`
(2026-08-10). Nothing in the 2026-08-20 window is a suspect, and neither is any
other landing — the shape has been reachable since the queue existed.

## What is owed

The fix is not here. It is the release protocol in
`issues/kernel/deferred-release-outlives-its-syscall.md`, which carries the two
shapes it can take and why the second belongs to
`issues/kernel/every-wait-in-this-kernel-is-a-spin.md`. What is owed *here* is
that this name stays adjudicable: `src/redlist.rs` carries the row, so a
landing gate that hits `kill_while_blocked` has a rate to check the red
against, and nobody re-runs it away or re-classifies its `Sched`.
