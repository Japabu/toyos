---
status: open
kind: finding
opened: 2026-08-22
---

# `ModuleInfo` is the second boundary-crossing ABI type with no safe `as_bytes`

The same shape as `issues/build/logrecord-has-no-as-bytes.md`, found by the
next area sweep and filed beside it rather than folded into it — one file per
issue, and the two close through different edits.

`toyos-abi/src/syscall.rs:1801`'s `ModuleInfo` is `#[repr(C)]` over four `u64`s
and two `u32`s, six fields with no padding. Six sibling types in the same crate
carry an inherent `as_bytes(&self) -> &[u8]` (the list is in `LogRecord`'s
entry); `ModuleInfo` does not, so `SYS_QUERY_MODULES` spells the conversion by
hand in the kernel at `kernel/src/arch/syscall.rs`'s `sys_query_modules`:

```
fn encode(info: &ModuleInfo) -> [u8; core::mem::size_of::<ModuleInfo>()] {
    unsafe { core::mem::transmute_copy(info) }
}
```

Sound as written — the destination is `[u8; size_of::<ModuleInfo>()]`, so the
sizes are equal by construction, every byte pattern is a valid `[u8; N]`, and
`#[repr(C)]` over six integers with no padding means no uninitialised byte is
read. So this is a reduction, not a defect.

**It is also the type where a gap would matter most of the six**, because the
buffer this writes into is a user address: `path_offset` and `path_len` are
`u32` at the end of four `u64`s, which is 8-byte aligned and 8 bytes long
together, so there is no tail padding today — and nothing says so. Adding a
field of any other width silently publishes kernel stack bytes to userland.
`as_bytes` is where that assertion belongs, beside the layout it is about, the
way the six siblings have it.

**Why it was not done in the `arch/` sweep that found it**: it is an edit under
`toyos-abi/src`, which the workflow rules put on its own pull request and which
costs a sysroot claim exactly like a layout change. The block carries a
`SAFETY:` naming this entry instead.

Closing it: add `as_bytes` to `ModuleInfo` in the shape its siblings use, with
the `size_of` assertion, and delete `sys_query_modules`'s local `encode`. The
same pull request can take `LogRecord`'s — both are one sysroot claim, and
neither changes a layout.
