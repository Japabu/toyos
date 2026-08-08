---
status: open
kind: defect
opened: 2026-08-01
---

# The ring's closed flags are userland's to forge, and netd believes them

The kernel no longer reads `RingHeader::flags`: its own `readers`/`writers`
counts decided every one of the four sites that used to consult them, and the
flag — unlike the count — is in the page `SYS_PIPE_MAP` maps writable.

netd still reads them. `bridge_piped` treats `rx_ring.is_reader_closed()` as "the
client died" and `tx_ring.is_writer_closed()` as "the client stopped writing, so
close the socket"; `cleanup_dead_listeners` aborts a listener's socket on the
same bit (`userland/netd/src/main.rs:1006`, `:1011`, `:1045`). Anyone who can map
one of those pipes can set the bit and make netd tear the connection down.

Today that is the connection's own client, so it is self-harm — but the bound on
*who* is `may_open_pipe`, which is a relationship check and not a capability, and
whose own stated residual is that a peer entitled to one of a creator's pipes is
entitled to all of them. netd's exposure is bounded by that residual, not by
anything netd does.

The general statement, since it is the same one the kernel just had to learn: a
publication is not a channel. netd is reading a value its peer writes and
treating it as a fact about its peer. The kernel's answer was to ask the side
that knows; netd has no such side to ask, which is the actual design gap.
