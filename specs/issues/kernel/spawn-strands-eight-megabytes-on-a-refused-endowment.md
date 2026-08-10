---
status: open
kind: defect
opened: 2026-08-10
---

# `spawn`'s handle refusal ends the caller with the child's memory on its stack

`kernel/src/loader/mod.rs`:

```rust
let (handles, endowments) = pending.commit().map_err(|e| match e {
    Refusal::Error(e) => e,
    // "…the process it would have started does not exist yet, so this
    //  reports rather than ending it here."
    Refusal::Handle(e) => e.refuse_as_error(),
})?;
```

The comment is false. `refuse_as_error` maps `BadHandle`/`Stale`/`WrongType` to
`crate::process::handle_fault`, which is `-> !` and exits the process
(`kernel/src/process.rs`). It does end the caller, and it ends it from a point
where `spawn` owns:

| live at that call | size |
|---|---|
| `child_pt` | a whole `AddressSpace` with the ELF regions mapped |
| `stack_pages` | `USER_STACK_SIZE`, 8 MB (`loader/mod.rs`) |
| `ks_alloc` | `KERNEL_STACK_SIZE`, 128 KiB (`process.rs`) |
| `tls_pages`, `loaded_libs`, `syms`, `backing`, `reloc_index` | — |

**Nothing unwinds**, so none of it is given back. This is the exact shape
`c29bb8a` fixed one layer up — `exit_current` returning `!` with three `Arc`s
stranded, found by the census — reproduced where the stranded values are eight
megabytes rather than three refcounts.

## Reaching it

`PendingHandles::commit` verifies every endowed handle and answers a
`Refusal::Handle` for one that does not resolve. A single-threaded caller
cannot get there: `SpawnArgs`' handles were valid when it wrote them. A
*multi-threaded* one can — one thread closes an endowed handle while a sibling
is inside `SYS_SPAWN`, and `Stale` comes back. That is a bug in the caller and
the kill is right; the leak is not.

## The fix

The same one `c29bb8a` used: make the refusal a value that travels out of
`spawn`, so every owned thing on that stack drops on the way, and reach
`handle_fault` from the dispatcher with nothing held. The dispatcher already has
that shape for every other handle refusal (`Refusal::refuse` is called on the
result of `with_fd_owner_data`, never inside it).

## Instrument

`handle_kill_policy`'s census is the right one and does not cover this: its
`holder` role dies on a bad *read*, not inside a spawn. The arm is a thread that
closes an endowed handle while a sibling spawns, run `CHURN_ROUNDS` times, with
free physical memory rather than the object count as the verdict — 8 MB a round
is visible in `SYS_SYSINFO` and invisible in the object census, because pages
are not objects.
