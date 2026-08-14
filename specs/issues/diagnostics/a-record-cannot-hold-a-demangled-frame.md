---
status: assigned
kind: defect
opened: 2026-08-11
---

# 224 bytes of message cannot hold a backtrace frame, and the corpus that sized it had no panic in it

`toyos_abi::log::MAX_RECORD_MESSAGE` is 224, derived in
`specs/log-architecture-spec.md` §2.1 from every committed T14 log: 12,497
lines, p50 59, p90 111, p99 154, p999 857, max 863, and *"everything above 200
characters is one call site — a `{:?}` of `KernelArgs`, 18 lines of the
12,497"*.

**Those logs are boots. None of them contains a panic backtrace**, and a
backtrace frame carries a demangled Rust symbol whose length is bounded by
nothing the kernel controls. The tree already has a test built on exactly that:
`screen_late_panic`'s stimulus is `late_panic::Nest`, *"a generic nested in
itself, so its demangled symbol is wider than any console grid and its head and
tail cannot share a display row"* (`tests/toyos.rs`, `check_wrap`). It exists to
prove the panel **wraps rather than clips**, and it asserts on the *tail* of the
symbol for that reason.

Measured, 2026-08-11: with `emit` rendering the byte ring from the truncated
record, `screen_late_panic` reds — `the tail of the demangled symbol never
reached the screen — clipped?` — and reds again alone. So the bound does not
merely trim a struct dump; it destroys the one diagnostic the panel exists for
on a machine with no serial port.

**Not the same defect as the `KernelArgs` dump §2.1 already names.** That one is
a call site the kernel can split, and L2 splits it. This one is a *symbol*: the
producer cannot split it into meaningful pieces, and two records would render as
two prefixed lines with the frame's name broken across them.

## What is in the tree now

L1 does **not** truncate what reaches the wire. `log::Tee` runs one format pass
into two sinks: the byte ring gets every byte, the record gets the first
`MAX_RECORD_MESSAGE` and an exact `elided` count. So the console, the panel and
the harness are byte-identical to `main` and `screen_late_panic` is green.

That is scoping, not a fix. **It stops working at L2**, which is where the panel
and `sched::dump` start rendering records instead of bytes — at that point the
tail is gone for real.

## The options, none of them free

1. **Raise `MAX_RECORD_MESSAGE`.** An ABI change, so another landing of its own,
   and there is no principled value: a demangled symbol has no bound. It also
   multiplies by `SHARD_RECORDS × MAX_CPUS` — every 32 bytes is 128 KiB at the
   shipped eight CPUs.
2. **Let a producer split one message across several records**, with a
   continuation flag so a reader can rejoin them. This is the descriptor-ring
   argument §14 rejection 2 turned down, in miniature and only for the overflow
   path — and it puts "is this record whole?" back into the reader.
3. **Give the backtrace path its own bound**, e.g. render a frame's symbol
   head-and-tail with the middle elided, in the producer. Cheap and honest on a
   panel; loses information a serial log would have kept.
4. **Accept the truncation for records and keep the untruncated text on the
   console.** That is what L1 does by accident of still having a byte ring, and
   it cannot survive L3 deleting it.

**Ruled 2026-08-14** (owner delegated the call to the orchestrator with
"best and most sustainable"): options 3 and 1 together, as recommended.
A backtrace frame renders its symbol head-and-tail with the middle
elided, at the producer — no fixed bound fixes an unbounded symbol, and
both ends are what `check_wrap` asserts matter. `MAX_RECORD_MESSAGE`
rises 224 → 992, keeping `RECORD_BYTES` a power of two (256 → 1024,
the shift-indexing invariant): the next power-of-two record that holds
the measured maximum (863), so every observed ordinary line fits
whole; the +3 MiB at eight CPUs is bought deliberately. The bump is
ABI and lands alone
ahead of L2; the elision is L2's. Option 2 stays rejected — a
continuation flag puts "is this record whole?" back into every reader,
the property the design exists to avoid.
