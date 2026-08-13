---
status: open
kind: finding
opened: 2026-08-10
---

# A handle that can be moved can always be moved on

Both move paths — `SpawnArgs::endow` and `SYS_HANDLE_SEND` — require
`Rights::TRANSFER` on the handle being moved, and both carry the source's rights
into the destination unchanged. So every handle that arrives by a move arrives
carrying `TRANSFER`, and its new holder can move it on. There is no way to
express "you may map this and nobody else may".

soundd's per-client audio ring would ideally travel as a `shm_h_with_MAP_only`,
so that a client cannot re-send it on. `specs/capability-endowment-spec.md`
§5 rules that out: sending needs `TRANSFER`, so that handle cannot be sent.
The same hole is in spawn endowment.

## What it costs

The pid ACL this replaced was explicitly non-transitive — `shared_memory::grant`
was owner-only, and its doc said why: *"a grantee that could re-grant makes the
owner's ACL transitive, so soundd's per-client audio ring reaches anyone that
client names and the owner is never told."* Under handles that is no longer
true. A client can pass soundd's ring to anybody it can reach.

The practical exposure today is small — a client can memcpy the ring's contents
to a peer anyway, so what leaks is the ability to *write* it — but the property
the spec claims is not the property the code has, and that is the part worth
recording.

## What would fix it

A rights word per moved handle, on **both** paths: `EndowEntry` gains one and
`SYS_HANDLE_SEND` takes `[TransferEntry { handle, rights }]` instead of
`[RawHandle]`, with `RIGHTS_UNCHANGED` as the wire encoding
`SYS_HANDLE_DUP` already uses. `specs/capability-endowment-spec.md` §6 argues
against a rights word on the endow path — "a second place to shrink rights
and the first one already exists" — and that argument is wrong for exactly
this case: dup-then-move cannot express a set without `TRANSFER`, because the
move needs it.

Not done because doing it on the send path alone would make the two move
verbs disagree, and doing it on both is an ABI change to a struct already
shipped.
