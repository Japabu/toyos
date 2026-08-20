---
status: open
kind: finding
opened: 2026-08-20
---

# `Ring::read`/`Ring::write` build `&[u8]`/`&mut [u8]` over a page the peer can write too

Writing `undocumented_unsafe_blocks`' required safety comments on
`toyos-abi/src/ring.rs` surfaced a question the module's own doc comment
doesn't quite answer. The header names the trust boundary precisely: `Ring`
(kernel-side: base pointer, capacity, both cursors) lives in kernel memory,
`RingHeader` is what `SYS_PIPE_MAP` hands a process, and "nothing the copies
are bounded by is in that page" — so an adversarial write to the mapped page
cannot turn into an out-of-bounds *kernel* access. `a_scribbled_header_leaves_the_stream_exact`
tests exactly that, for the header's `flags` word.

What the doc doesn't say, and what the test doesn't cover: `Ring::read` and
`Ring::write` build `core::slice::from_raw_parts[_mut]` over the **data**
region of that same mapped page (`read.rs:136`, `write.rs:162` in the current
line numbers), and hand the result to the caller as an ordinary `&[u8]` /
`&mut [u8]`. If the pipe's data bytes — not just the header — are reachable
through the same userland mapping, then constructing a Rust shared/exclusive
reference over memory another process can concurrently write violates the
no-concurrent-mutation guarantee those reference types carry, independent of
whether the *kernel's bounds* stay correct. That's a different question from
the one the module answers: not "can userland corrupt an out-of-bounds kernel
read", but "is a plain `&[u8]` the right type for memory that isn't
exclusively ours while the reference is live" — Rust's model gives the
optimizer license to assume no such write happens, which real hardware
tearing doesn't care about but a future codegen change could.

Whether this is real depends on something this file doesn't state and this
pass didn't chase down: is the pipe's *data* region actually part of what
`SYS_PIPE_MAP` maps writable, or does userland only ever see `RingHeader`
(with the data bytes moving exclusively through syscalls the kernel mediates
and copies for)? `kernel::pipe`'s `PIPES` table and whatever builds the
`SYS_PIPE_MAP` mapping are what would answer that, not this crate. If the
data region is shared, the fix is to route these copies through volatile or
relaxed-atomic byte access (or a type that says "outside memory" explicitly)
rather than `&[u8]`; if it isn't shared, the safety comments already written
should say so plainly instead of citing the header-only trust boundary and
the reader should not have had to ask.
