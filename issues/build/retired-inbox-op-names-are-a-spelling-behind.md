---
status: open
kind: defect
opened: 2026-08-20
---

# `RETIRED_ABI_NAMES` carries one retired inbox op and spells both the old way

`src/sourcegate.rs`'s `RETIRED_ABI_NAMES` exists so a retired *number* cannot
be reissued by accident: a number cannot be scanned for, so the name that
carried it is, and a name back in code reds. Two inbox op codes are retired and
the table protects neither properly.

**Op code 4 is missing entirely.** `toyos-abi/src/inbox.rs` records it as
retired — it was `IORING_OP_CLOSE`, it had no submitter in the SDK, in
userland or in mio, and it was the one handle path that could not obey the
bad-handle policy because it ran under the ring's own lock. The comment
recording that is all there is; nothing stops code naming it again and taking
4 back. Op code 2 (`IORING_OP_POLL_REMOVE`) is in the table. This gap predates
the inbox rename — it was found while reading the file that rename moved.

**Both are spelled in a vocabulary the tree no longer uses.** After 2026-08-20
the module is `toyos_abi::inbox` and its live ops are `OP_NOP`, `OP_WATCH` and
`OP_ACCEPT`. An agent re-adding a cancel op today writes `OP_POLL_REMOVE` or
`OP_CANCEL`, and an agent re-adding a close op writes `OP_CLOSE` — none of
which the table names. The scan matches identifiers exactly, so the one row it
carries now protects a spelling nobody would type rather than the number 2.

So the table has to say what is retired in the vocabulary somebody would use to
bring it back. The shape is a decision, not a lookup: the rows could carry both
spellings, or the old ones could be dropped for the new ones on the ground that
`IORING_*` is itself unreachable now, and each answer has a different failure.
Whichever it is, op code 4 gains a row.

Neither op code may be reused whichever way this lands: 2 and 4 are retired,
and `toyos-abi/src/inbox.rs` is where they are recorded beside the live three.

**Two doc citations in the sysroot crates still say `kernel/src/io_uring.rs`**
(`toyos-abi/src/ring.rs:10`, `toyos/src/poller.rs:61`): the kernel file they
cite renamed to `inbox.rs` in the internal-vocabulary pass, but a doc change
under those crates costs a sysroot claim and may not ride a kernel PR — they
ride the next PR that lawfully claims the sysroot.
