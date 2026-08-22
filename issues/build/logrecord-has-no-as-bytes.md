---
status: open
kind: finding
opened: 2026-08-22
---

# `LogRecord` is the one boundary-crossing ABI type with no safe `as_bytes`

Six `toyos-abi` types that cross the kernel/user boundary carry an inherent
`as_bytes(&self) -> &[u8]`, each with one `unsafe` block living beside the
layout it depends on and one `# Safety`/`SAFETY:` paragraph making the same
argument — every byte belongs to a field, so a gap would publish whatever the
kernel stack held:

- `toyos-abi/src/net.rs:23` — `NicInfo`
- `toyos-abi/src/virtio_sound.rs:159` — `VirtioSoundInfo`
- `toyos-abi/src/lib.rs:87` — `FramebufferInfo`
- `toyos-abi/src/input.rs:42` — `RawKeyEvent`
- `toyos-abi/src/input.rs:69` — `MouseEvent`
- `toyos-abi/src/hda.rs:58` — `HdaInfo`

`LogRecord` (`toyos-abi/src/log.rs:86`) is the seventh such type and has none,
so the kernel spells it by hand at
`kernel/src/log/user.rs`'s `RecordSink for UserRecords::put`:

```
core::slice::from_raw_parts((record as *const LogRecord).cast::<u8>(), RECORD_BYTES)
```

That block is the only `unsafe` left in `log::` that a safe abstraction would
remove, and the abstraction already exists six times over. It is sound as
written — `LogRecord` is `#[repr(C)]`, `Copy`, valid for any bit pattern, and
`const _: () = assert!(size_of::<LogRecord>() == RECORD_BYTES)` is in the ABI
crate — so this is a reduction, not a defect.

**Why it was not done in the sweep that found it.** Giving `LogRecord` an
`as_bytes` is an edit under `toyos-abi/src`, which the workflow rules put on
its own pull request and which costs a sysroot claim exactly like a layout
change. It is one line of value and would have made an
`undocumented_unsafe_blocks` area sweep into an ABI landing — so the block
carries a `SAFETY:` naming this entry instead, and the removal waits for a
pull request that is already claiming the sysroot for another reason.

Closing it: add `as_bytes` to `LogRecord` in the shape its six siblings use,
and delete the hand-rolled slice in `kernel/src/log/user.rs` — after which
`log::` has one `unsafe` block left (`shard.rs`'s `initialize_zeroed`, which
is irreducible).
