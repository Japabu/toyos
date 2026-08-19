---
status: open
kind: defect
opened: 2026-08-01
---

# THE CLASS: a client's request is an allocation request

> **A client's request is an allocation request, and every one of them needs an
> owner who can say no.**

Three instances, and the statement is here because none of the three says it
alone — it is what predicts the fourth:

- the compositor's windows (`compositor-and-netd-unbounded-accept`),
- netd's piped connections (the same entry),
- `SYS_CONNECT` pinning 4 MiB into an unbounded pending queue.

**The third was worse than it looks, because the attacker did not need to find a
service to abuse — `SYS_LISTEN` was ungated, so it could be its own.** Register a
name, connect to yourself, never accept. No victim required and nothing to guess.
**That clause is dead**: there is no registry and no name, a port's two ends are
two types, and an attacker must be *given* a connector by somebody who holds one.
What is left of the third instance is an ordinary bound on a port's queue.

**The third is closed, and the shape of the close is the reusable part** (read
against `ba612c6`, 2026-08-04). `listener::push_connection` returns
`Result<(), PushError>` (`listener.rs:120`) with a queue depth behind it, and
`sys_connect` (`syscall.rs:1152`) now takes the answer: on `QueueFull` it closes
the client's own fd and returns `ResourceExhausted`, on `NoListener` it returns
`NotFound`. That is the pair this class asks for — a bound *and* a caller that
hears the refusal — and it is why the cap could be added at all. The same pair
survives the endowment rewrite: `MAX_PENDING_CONNECTIONS` is per port now and
`SYS_NAMESPACE_OPEN` answers `ResourceExhausted` on a full one.

**The first two instances are what is left**, and they are the ones a bound alone
does not answer: a compositor cannot refuse a window without deciding whose.

**And the 4 MiB is gone with it.** A pipe now allocates its 2 MiB ring page on
first use — `pipe::create` is infallible because a pipe with no traffic owns no
physical memory, and `try_write`/`map_page` are where exhaustion is met and
answered. Measured in `abuse_connect_flood`: 32 unaccepted connections cost
**0 KiB**, against the 128 MiB the eager allocation charged for the same
allowance, and the first byte written on one buys **2048 KiB**. So the depth is
now a bound on the queue and not on memory, which is what the entry it guards
was about; `MAX_PENDING_CONNECTIONS` says so in its own comment.
