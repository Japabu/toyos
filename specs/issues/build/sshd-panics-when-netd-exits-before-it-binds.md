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
machine.** On a boot with no NIC, netd says so and exits 0. If sshd's bind
reaches netd first it gets `NotConnected` and leaves with a line. If netd has
already gone, the bind fails with a generic `netd error`, the guard does not
match, and sshd panics — on a machine whose only fault is having no network
card, which is the exact situation the clean arm was written for.

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

- **netd's answer carries the reason.** A refusal from a live netd and a
  connection to a netd that has exited are different failures and should not
  both flatten to `netd error` at the SDK boundary — `toyos`'s net client is
  where that distinction would live.
- **sshd does not race a service it needs.** A program whose whole function
  needs netd could wait for it rather than binding into whatever state init's
  ordering left, which is the shape `[boot] start` ordering already implies but
  does not enforce.
