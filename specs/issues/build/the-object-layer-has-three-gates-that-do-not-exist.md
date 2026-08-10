---
status: open
kind: defect
opened: 2026-08-10
---

# Three of §8.6's required object-layer gates were never written

`specs/capability-endowment-spec.md` §8.6 lists what the object layer is gated
by, "adopted from `capability-handles-spec.md` §12.4 and **required here**":

| named | exists |
|---|---|
| `handle_basic` | **no** |
| `handle_transfer` | **no** |
| `handle_kill_policy` | yes |
| `kill_while_blocked` | **no** |
| `device_claim_crash_release` | as `device_claim_lifetime` |
| `process_lifecycle` | yes |
| the per-variant `LIVE_*` census asserted back to baseline after every churn | partly — see below |

`ls tests/toyos-rust-tests/src/bin/` carries no binary under any of the three
missing names, and no other test asserts their properties.

**`kill_while_blocked` is the one §8.6 itself calls "the one that matters most
for this architecture":** *"an audio client killed while blocked in its
signal-pipe read must make soundd see `Gone`, and that is only true because
`handle_count` is not the Arc count."* That is the load-bearing claim of §1.1 —
the whole reason the handle count is a separate number from the refcount — and
nothing executes it.

## What the census does and does not cover

`handle_kill_policy`'s `the_kills_release_what_they_held` is real and has teeth:
two samples of sixteen kill-rounds, red if the second exceeds the first. Its
`holder` role holds a pipe pair and a shared-memory region, plus whatever the
SDK endowed — so the kinds it exercises are `PipeRead`, `PipeWrite`,
`SharedMem`, `Namespace`, `Connector`, `SysCap` and `Process`.

Not exercised by any census assertion: **`File`, `Device`, `Acceptor`,
`Connection`, `IoUring`, `Console`.** The per-variant `LIVE_*` breakdown exists
(`SYS_DEBUG` action 15) but writes to the kernel log, so no guest test can
assert on one — every assertion is against the machine-wide total, where a leak
of one kind is hidden by churn in another.

`fd_lifetime` and `shm_release_reclaims` assert on *free physical memory*, which
is blind to every object holding no pages: `Namespace`, `Connector`, `Acceptor`,
`Process`, `SysCap`, `Console`, and a `File` whose pages are in the file cache.

## Why it matters now

`specs/issues/kernel/a-refused-handle-send-destroys-the-batch.md`,
`specs/issues/kernel/an-immediate-object-can-be-released-on-the-idle-stack.md`
and `specs/issues/kernel/a-connect-can-queue-onto-a-closing-port.md` all live in
`Connection`, `File` and `Acceptor` — the three kinds nothing counts. The suite
being green is not evidence about them, and `launcher_refusals`' census arm is
the only test in the tree that watches the total across a *non-kill* churn.

## What to build

1. `kill_while_blocked`, as §8.6 words it.
2. `handle_transfer`: a batch across a connection, both directions, with the
   peer dying at each of the four points a batch can be at.
3. A per-kind census readable from the guest — `SYS_DEBUG` action 15 answering a
   count for a kind rather than logging all of them, or the total plus a kind
   argument. One arm, and it turns every existing census assertion from
   "nothing leaked overall" into "nothing of this kind leaked".
