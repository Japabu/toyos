---
status: open
kind: finding
opened: 2026-08-17
---

# `completion-architecture-spec.md` §14.2 quotes a `[u32; 64]` comment in `toyos-abi/src/syscall.rs`; it is not there

`specs/completion-architecture-spec.md` §14.2 says `ProcessData`'s syscall
profile array "is sized from the ABI rather than at `[u32; 64]` — its own
comment records *'It was `[u32; 64]` while the ABI reached 98'*", citing
`toyos-abi/src/syscall.rs`.

That sentence does not appear in `toyos-abi/src/syscall.rs`, nor in
`kernel/src/process.rs` where `ProcessData` is defined:

```
$ grep -rn '\[u32; 64\]' toyos-abi/src/syscall.rs kernel/src/process.rs
(no output)
```

The array in the current tree is `syscall_counts: [u32;
toyos_abi::syscall::SYSCALL_PROFILE_BINS]`
(`kernel/src/process.rs`), where `SYSCALL_PROFILE_BINS: usize = 128`
(`toyos-abi/src/syscall.rs`) is a fixed constant — not obviously "sized from
the ABI" (i.e. tracking the highest syscall number) the way the spec's prose
claims. Whether the sizing relationship the spec describes ever held, or the
comment was reworded/removed independently, is worth someone checking against
the commit history; this entry only establishes that the quoted text and the
"sized from the ABI" characterization do not match the current source.

Filed as a finding rather than a defect because nothing misbehaves; it is a
planning document's description of a data layout that needs reconciling with
the field's current shape.

Found 2026-08-17 during a citation-accuracy pass over
`specs/completion-architecture-spec.md`; verified at the tree's tip that day.
