---
status: open
kind: defect
opened: 2026-08-22
---

# The inbox rings are reached through `&`/`&mut` over a page the process maps writable

`kernel/src/inbox.rs` turns the inbox's 2 MiB shared page into Rust references
at seven sites. All seven name memory the owning process has mapped writable,
so another thread of that process can write the same bytes while the reference
exists — which is the shape `kernel/src/user_ptr.rs`'s [`UserBytes`] header
argues is the bug, not a style preference: `&`/`&mut` carry `dereferenceable`
(and, for `&mut`, `noalias`) into LLVM, and the compiler is then entitled to
fold, hoist or duplicate reads the process can change between.

The sites, found by the `undocumented_unsafe_blocks` sweep of the kernel's root
files (2026-08-22) while writing their justifications:

| site | shape | what it names |
|---|---|---|
| `Inbox::submission_header` | `&RingHeader` | `SUBMISSION_RING_OFF` |
| `Inbox::completion_header` | `&RingHeader` | `COMPLETION_RING_OFF` |
| `Inbox::submission_at` | `&Submission` | one submission entry |
| `create`, params | `&mut RingLayout` | offset 0 |
| `create`, submission header | `&mut RingHeader` | `SUBMISSION_RING_OFF` |
| `create`, completion header | `&mut RingHeader` | `COMPLETION_RING_OFF` |

(`Inbox::completion_at` is the seventh unsafe block and is **already** the right
shape — it hands back a `*mut Completion` and its doc comment says why.)

## Why nothing is broken today

Every read through the three `&` accessors is either an atomic (`head`, `tail`,
`dropped`) or a whole-struct copy taken once (`claim_submission` does `*
instance.submission_at(..)` and never looks at the borrow again), and the one
plain field in `RingHeader` — `ring_size` — is written once in `create` and
never read back by the kernel, which uses its own `submission_size` instead.
That is a property of every current caller, not of the types.

## The three `&mut`s are sharper

`create` calls `shm.map_into(pid, &addr_space)` **before** it builds the
headers, so the page is already in the process's address space while the kernel
holds `&mut` to it. Nothing has been observed going wrong, because the caller
has not returned from `SYS_INBOX_SETUP` yet and so nothing in userland knows the
address — but "no thread knows the address" is not "no thread may write it".

**The cheap half of the fix is a reordering**: compute `shm.phys()`, write the
three headers, and call `map_into` afterwards. That closes the window entirely
and changes no ABI.

## The rest is the same question the ABI side already carries

`issues/build/ring-rs-shared-slice-over-a-userland-writable-page.md` asks
whether `toyos-abi`'s `ring.rs` may hand out `&[u8]`/`&mut [u8]` over a page
userland can write. This is that question on the kernel side of the same
boundary, and the two want one answer — most likely a bounded window type with
copy-in/copy-out and no reference, i.e. `UserBytes`/`UserBytesMut`'s shape
applied to a `DirectMap` the kernel owns.

Not fixed in the sweep that found it: this is the user/kernel boundary's
semantics, which the sweep's brief puts out of scope.
