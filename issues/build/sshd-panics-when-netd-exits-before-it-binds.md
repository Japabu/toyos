---
status: assigned
kind: defect
opened: 2026-08-15
---

# sshd panics instead of leaving quietly when netd exits before its bind lands

`sshd` has one clean exit for a machine with no network and it is keyed on an
error kind:

```rust
// userland/sshd/src/main.rs:353
let listener = match tokio::net::TcpListener::bind("0.0.0.0:22").await {
    Ok(l) => l,
    Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {
        println!("sshd: no network on this machine, exiting");
        return;
    }
    Err(e) => panic!("sshd: cannot bind 0.0.0.0:22: {e}"),
};
```

**Which arm it takes is a race with netd's lifetime, not a fact about the
machine — but the losing path is narrower than first written.** The SDK
already folds the obvious gone-service shapes into the clean arm:
`toyos/src/net.rs` maps `IpcError::Disconnected` and
`EndowError::{NotEndowed,ServerGone}` to `NetError::NetdNotFound`, and the
std fork maps that to `ErrorKind::NotConnected` — the guard's own kind, built
after sshd "panicked across the boot of every NIC-less machine that lost the
race" (`hangup`'s doc, landed in `78df7a3`). The observed panic therefore came
through a path that is not any of those: `EndowError::Refused` with another
code, a non-`Disconnected` `IpcError` mid-handshake, `pipe_pair()` failing, or
netd answering an `ErrorResponse` other than `ERR_NOT_CONNECTED` while
tearing down. Which one was not captured — the message cannot say, because
the std fork flattens every kind to the same `"netd error"` string.

Caught in a full suite, dev host, 2026-08-15, on `wt/toyos-logd`, once in ten
consecutive runs and only with the host's load average above 6:

```
[kernel 0.402 cpu0] exit: netd pid=4 code=0
thread 'main' (1) panicked at sshd/src/main.rs:359:23:
sshd: cannot bind 0.0.0.0:22: netd error
```

The victim is `boot_partition_identity`, which refuses any boot whose console
carries `panicked at`. Its own subject — the partition signature the bootloader
reports — is untouched, so the red names the workload and never the cause; the
harness's isolated re-run answered `ALONE: GREEN`.

**And on CI, where there is no host load to blame.** Nightly dispatch
`31900050723`, job `95049280131` (`guest (3)`), `wt/toyos-ciwall`, 2026-08-15 —
a KVM shard with one guest on the machine and `--jobs 1`:

```
init: netd: no nic on this machine (NotFound)
[kernel 0.504 cpu0] spawn: /bin/netd pid=5 …
[kernel 0.517 cpu0] spawn: /bin/sshd pid=6 …
netd: no NIC on this machine, exiting
sshd: starting...
thread 'main' (1) panicked at sshd/src/main.rs:359:23:
sshd: cannot bind 0.0.0.0:22: netd error
[kernel 0.540 cpu0] exit: netd pid=5 code=0 cpu=11ms
[kernel 0.666 cpu0] exit: sshd pid=6 code=101 cpu=122ms
```

So "only with the host's load average above 6" is a property of the dev-host
session above and not of the race. The victim is `boot_partition_identity`
again, and the harness answered
`ALONE: GREEN, and it was alone both times — nothing the harness controls
differed, so it failed once and passed once. That is a rate and not a
classification.` The run minutes before it on the same runner pool
(`31900045901`, `main` at `e064a96`) was green on this name, and the two trees
differ only in `src/testargs.rs`, `tests/toyos.rs` and a deleted issue file —
nothing in the image.

**The same job contains one boot of each arm, and their timings are the
opposite way round from the heading.** `usb_boot_stick_pulled`, later in that
same shard, took the clean exit:

```
[kernel 0.767 cpu0] spawn: /bin/netd pid=5 …
netd: no NIC on this machine, exiting
[kernel 0.804 cpu6] exit: netd pid=5 code=0 cpu=37ms
[kernel 0.809 cpu0] spawn: /bin/sshd pid=6 …
sshd: no network on this machine, exiting
[kernel 0.833 cpu7] exit: sshd pid=6 code=0 cpu=24ms
```

sshd started 5 ms *after* netd was gone and got `NotConnected`; in the panic
above it started 23 ms *before* netd's exit and bound into the teardown. One
boot each way is not a proof, but it is the pair the write-up's shortlist
predicts: the losing path is mid-handshake against a netd that is still there,
not a netd that has already gone.

**A second CI sighting, two days and two trees later.** Run `32044008591`, job
`95428160739` (`guest (1)`), PR #116 on `wt/toyos-invariantp`, 2026-08-17 — the
same line at the same site, and the victim is `boot_partition_identity` for the
third time:

```
[kernel 0.529 cpu0] spawn: /bin/netd pid=5 …
netd: no NIC on this machine, exiting
[kernel 0.559 cpu0] spawn: /bin/sshd pid=6 …
init: started sshd
sshd: starting...
thread 'main' (1) panicked at sshd/src/main.rs:359:23:
sshd: cannot bind 0.0.0.0:22: netd error
init: started test-runner
[kernel 0.566 cpu0] exit: netd pid=5 code=0 cpu=16ms
[kernel 0.690 cpu1] exit: sshd pid=6 code=101 cpu=111ms
```

**The message is identical and that settles nothing about the producer**, which
is this file's own central finding: four candidate paths print `netd error`
because the std fork flattens every kind to that string, and this capture
distinguishes them no better than the last one did. It is recorded as *same
signature, producer unidentified*, and `src/redlist.rs`'s row says so in those
words.

**What it does add is a third point in the timing series, and it points the same
way.** sshd spawned at 0.559 s and netd exited at 0.566 s — a **7 ms** gap, so
the bind again went into a teardown that was in progress rather than finished.
The 2026-08-15 CI panic had 23 ms in the same direction; the clean-exit arm in
that same job had sshd start 5 ms *after* netd was gone. Three boots is not a
proof, but the sign has not yet come out the other way: **every observed panic
has sshd binding before netd's exit line, and every observed clean exit has it
binding after.** That is the shortlist's prediction holding for a third boot,
and it is still a correlation over three.

One reading deliberately *not* taken from this capture: `init: started
test-runner` sits between the panic message and the backtrace, and that is a
property of the console splitter rather than of time — several processes write
to one console and the harness's own caveat is that their lines interleave. It
carries nothing about the race, and init is in any case still walking its child
list whenever sshd starts, since sshd is not the last entry in it.

The branch it fired on is a diff of documentation and `src/redlist.rs` strings
with no code in it. The harness answered `ALONE: GREEN, and it was alone both
times` again, and shard 1's other 173 names passed.

**A fourth sighting, and the sign holds for a fourth boot.** Merge-queue run
`32550410305`, job `96976123706` (`guest (2)`), 2026-08-22 — the queue's own
composition, so no branch owns it:

```
[kernel 1.135 cpu0] spawn: /bin/sshd pid=8 …
thread 'main' (1) panicked at sshd/src/main.rs:359:23:
sshd: cannot bind 0.0.0.0:22: netd error
[kernel 1.153 cpu0] exit: netd pid=7 code=0
```

**18 ms**, in the same direction as the 23 ms and 7 ms before it. Four boots of
the panic arm, every one with sshd binding before netd's exit line, against one
recorded clean exit with it binding 5 ms after. `boot_partition_identity` is the
victim for the fourth time; `src/redlist.rs` carries three rows for it, and this
sighting is recorded here rather than as a fourth because the rows have nothing
new to say — what is new is below, and it is not a capture.

## The producer, read out of the code

**It is `toyos::net::hangup` (`toyos/src/net.rs:334`), and netd is not the
owner** — netd answers nothing at all on this path. The four-candidate
shortlist above collapses to two, and both are the same fact under two kernel
words.

netd on a machine with no NIC returns before it ever takes its acceptor:
`userland/netd/src/main.rs:1224`'s `else` arm says the line and returns, and
`endow::acceptor("netd")` is the statement after it. So there is no teardown
answer to be wrong — nothing accepts, nothing sends an `ErrorResponse`, and no
code is chosen anywhere. The acceptor handle is released by process teardown
(`HandleTable::drain`, `kernel/src/object/handle.rs:554`), which runs
`port::Acceptor::on_zero_handles` (`kernel/src/object/port.rs:195`):

```rust
let queued = { /* lock */ queue.closed = true; take(&mut queue.pending) };
for connection in &queued { connection.inbox.close_now(); }
drop(queued);
```

A client's own end was cross-wired to those very queues when it connected
(`kernel/src/arch/syscall.rs:1954-1975`): the `PendingConnection`'s `inbox` **is**
the client's `outbox`, and its `rx`/`tx` are the far ends of the client's two
pipes. So for a client that connected while netd was alive and sends after that
hook has run there are exactly two refusals, in this order:

1. **`SYS_HANDLE_SEND` answers `SyscallError::Gone`.** `HandleQueue::push` finds
   the queue `None` and refuses (`kernel/src/object/service.rs:44-52`), and
   `sys_handle_send` returns that word (`kernel/src/arch/syscall.rs:2141-2175`).
   `tcp_bind` hands netd the notify pipe end, and `Connection::send_with_handles`
   moves the handles *before* it writes the frame (`toyos/src/ipc.rs:235-243`) —
   so this is the first thing a bind can be refused at.
2. **`SYS_WRITE` answers `SyscallError::NotFound`.** `drop(queued)` took the
   server's read end with it, so the pipe has no reader and `ops::write_pipe`
   maps `PipeWrite::BrokenPipe` to `NotFound` (`kernel/src/object/ops.rs:442-449`,
   reached for a connection at `ops.rs:488`). That the word is `NotFound` and not
   `Gone` is `issues/isolation/a-broken-pipe-answers-not-found.md`.

Both arrive at the SDK as `IpcError::Syscall(_)` (`toyos/src/ipc.rs:241` and
`:629`), and `hangup` had one arm: `IpcError::Disconnected => NetdNotFound`,
everything else `NetError::Io`. `Disconnected` is raised in exactly one place —
`ipc::read_exact` on a `read` that answered zero (`toyos/src/ipc.rs:601-611`) —
so it only ever covered the netd that left while this endpoint was waiting for
the **response**. The two writes ahead of that read had no arm at all.
`NetError::Io` is `ErrorKind::Other` in the std fork
(`rust/library/std/src/sys/net/connection/toyos.rs:25`), which is sshd's
`panic!` arm.

**Which of the two ran is not decidable from any capture, and does not need to
be**: they are two syscalls of one `send_with_handles`, microseconds apart, and
one change fixes both. The `Gone` window is the wider of the two — a
`SYS_PIPE` sits between `NetdConn::connect` and the transfer
(`toyos/src/net.rs:424-437`) — so it is the likelier producer, but that is an
inference and the fix does not rest on it.

**The clean arm is the same mechanism a few microseconds later**, which is what
explains the timing sign every sighting has. Once the hook has run, a *new*
`SYS_NAMESPACE_OPEN` finds `connector.closed()` and answers `Gone`
(`kernel/src/arch/syscall.rs:1946-1948`) → `EndowError::ServerGone`
(`toyos/src/endow.rs:151-155`) → `NetError::NetdNotFound`
(`toyos/src/net.rs:287-288`) → `ErrorKind::NotConnected`. Binding *after* netd's
exit line takes that path and always did; binding *before* it took `hangup`'s
`_` arm. The two now agree.

One thing the reading adds that no capture could: `on_zero_handles` is
**deferred** to a drain site (`kernel/src/object/mod.rs`'s `ZERO_QUEUE`, drained
at syscall exit, at each scheduler pass and in the idle loop). So there is a
third window in which netd has exited and the hook has not run yet — in it the
send succeeds, the response read hits EOF, and the client gets `Disconnected`
and exits cleanly. That is why the race is a rate rather than a certainty, and
why it does not correlate with anything the harness controls.

## What was done

- **The SDK is the owner.** `hangup` maps both gone-shaped kernel words to
  `NetError::NetdNotFound`; nothing else is widened, so the "What the fix is
  not" section below still holds — every other refusal is still `NetError::Io`
  and still panics. `toyos/src` is a shared-sysroot source, so it is its own
  branch and its own pull request: `wt/toyos-sshdbind-sdk`, PR #217, draft and
  unbuilt until the sysroot frees.
- **`netd_gone_mid_bind`** (`tests/toyos-rust-tests/src/bin/`) stages the
  sequence deterministically against a port of its own — no netd, no NIC and no
  clock. Three arms: refused at `SYS_NAMESPACE_OPEN` (the arm that already
  worked, asserted through `std::net::TcpListener` because
  `ErrorKind::NotConnected` is the literal thing sshd matches on), refused at
  `SYS_HANDLE_SEND`, refused at `SYS_WRITE`. The ordering edge is a second
  connection to the same port read with a blocking read: the hook closes every
  queued inbox and only *then* drops the connections, so an EOF on one proves
  the other's outbox is already closed. The last two arms are red without PR
  #217 and are that PR's negative control in the guest.
- **No netd delay-exit knob was added, deliberately.** `kernel/src/actuator.rs`
  admits an instrument on the claim that the state under it cannot be staged
  otherwise, and this state can: the test above stages it with the same kernel
  objects and no timing at all. A netd that sleeps on a parameter would be a
  weaker instrument that also had to be maintained.

## What the fix is not

Widening the guard to accept any error is wrong in the direction that matters:
the comment above it says the panic exists because *"nothing supervises init's
children, so the message is the entire diagnostic"*, and a machine that has a
NIC and cannot bind must still be loud. What is wrong is that "the network
service is gone" and "the network service refused" arrive as the same error.

Two honest shapes were written down here before the producer was known, and
**neither is what the fix turned out to be.** Both assumed the distinction
already reached the SDK and that what was lost was downstream of it — the
message, or the ordering. The reading above says the distinction never reached
the SDK at all on the two write paths: `hangup` had no arm for either kernel
word, so there was nothing for a message to carry. Kept as written, because a
shortlist that was wrong is the more useful record now that the answer is
beside it:

- **The error's kind survives to the message.** The distinction largely
  exists already — `toyos`'s net client separates gone-service from refusal —
  and what flattens is the *string*: the std fork's
  `io::Error::new(kind, "netd error")` prints the same words for every kind.
  Making the message carry the kind (and netd's teardown answer carry a
  gone-not-refused code) is what would have named the losing path above.
- **sshd does not race a service it needs.** A program whose whole function
  needs netd could wait for it rather than binding into whatever state init's
  ordering left, which is the shape `[boot] start` ordering already implies but
  does not enforce.
