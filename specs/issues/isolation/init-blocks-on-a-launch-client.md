---
status: open
kind: defect
opened: 2026-08-10
---

# Any launcher client can wedge the machine's only way to start a process

`/bin/init`'s accept loop is one thread and one connection at a time:

```rust
loop {
    match launcher.accept() {
        Ok(conn) => serve_launch(&conn, ..),
        ...
    }
}
```

and `serve_launch`'s first two statements are `conn.recv_header()` and
`conn.recv_bytes(..)`. Both go through `toyos::ipc::read_exact`, which is the
**blocking** `syscall::read`. So a client that connects to `launcher` and then
sends nothing — or sends a header claiming a payload it never writes — parks
init in that read for ever. No further connection is accepted, and after that
no process on the machine can be created through the launcher.

The launcher connector is held by the compositor, every terminal, every shell,
`/bin/console` and sshd (`system.toml`, `console/system.toml`). sshd is the one
reachable from the network.

## Why this is not the acknowledged single point of failure

`specs/capability-endowment-spec.md` §4.5 already names the launcher as "a
single point of failure: init wedged means no process can be created". That
sentence is about init *crashing*. This is a client choosing to wedge it, with
two syscalls, from an unprivileged process, leaving init alive and apparently
healthy — Ctrl+Alt+D reports a parked thread and nothing says why.

It is also a plain violation of the rule `userland/CLAUDE.md` states for every
other server: *"A server never blocks on a client: accept and the first frame
are two events, a frame is buffered until whole before anything acts on it."*
`toyos::ipc::FrameRx` is the SDK's answer and every other server uses it. init,
the one server the machine cannot lose, does not.

## The fix

init's loop becomes an event loop: a poller over the `launcher` acceptor and
every connection it has accepted, `FrameRx` per connection, and a bound on how
many half-spoken launches it will hold at once — which is the same shape
`userland/netd` and `userland/compositor` already have, and the same
`MAX_*`-on-the-primitive rule. A connection that has said nothing costs a
`PendingConnection` and no ring page (§5.2), so the bound is about init's
handle table rather than about memory.

## What a gate looks like

`launcher_refusals` (`tests/toyos-rust-tests/src/bin/`) is the shape and the
boot: `tests/netcase`, whose test-runner receives a `launcher` connector. The
arm is a child that connects and sends nothing, and a parent that then launches
a declared program and requires an answer. It cannot be written until the fix
exists — today it would hang the boot rather than fail it, which is why this is
filed rather than gated.
