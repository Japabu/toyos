---
status: open
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

## What the fix is not

Widening the guard to accept any error is wrong in the direction that matters:
the comment above it says the panic exists because *"nothing supervises init's
children, so the message is the entire diagnostic"*, and a machine that has a
NIC and cannot bind must still be loud. What is wrong is that "the network
service is gone" and "the network service refused" arrive as the same error.

Two honest shapes, and neither is this branch's to choose:

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
