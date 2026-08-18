---
status: open
kind: defect
opened: 2026-08-18
---

# The sites that still carry a boundary-crossing number in a plain integer

`toyos-untrusted`'s `Untrusted<T>` exists and two sites use it: the virtqueue
used ring (`kernel/src/drivers/virtio.rs`'s `used_ring_id`/`used_ring_len`,
which are the only reads of a used ring in the kernel and hand back
`Untrusted<u32>`) and, indirectly, everything downstream of them. The type has
no arithmetic, no `Deref`, no `From`, no cast and no accessor, so the only
functions on it returning a bare integer take the bound and answer `Result`.

**The rest of the class has not adopted it**, and until a site does, its bound
is a thing an author has to remember rather than a thing the compiler asks for.
Each of these is mechanical — find the read, wrap it, name the bound at the
use — and none of them needs a design decision:

- **`kernel/src/user_ptr.rs` and the syscall dispatch.** `UserAddr::checked` is
  the address half and is already right; the *values* beside it are not. Every
  `a2 as usize` length in `kernel/src/arch/syscall.rs` is a number userland
  chose, reaching a `Vec::with_capacity` or a copy length. `MAX_USER_STR` and
  `MAX_HEAP_ALLOC` are the bounds; `at_most` is the exit. This is the largest
  site and the one closest to userland.
- **`kernel/src/drivers/xhci/`.** The event ring's completion codes, slot ids
  and transfer residues are the controller's. `xhci/mod.rs`'s scratchpad fields
  are masked with `0x1F` — a clamp, which is what this type exists to stop
  being the answer.
- **`kernel/src/drivers/nvme.rs`.** The completion queue's `cid` is allocated by
  `alloc_cid` and never compared with what comes back (`wait_completion`), and
  `IdentifyNamespace` is read straight out of DMA. Sound today only because
  every submission is synchronous.
- **`kernel/src/acpi.rs`.** Table lengths from firmware, with the two
  subtractions `specs/assessments/type-safety-audit/kernel-drivers.md` F5 names.
- **`bcachefs`.** `collect_all` materialises a whole tree before anything
  counts it — the residual `untrusted-input-panics` records, and the one site
  here whose fix is a count primitive rather than a wrapper.

**What the type does not answer, recorded so nobody tries to make it.** Two of
the filed isolation entries are not this class:

- `netd-trusts-ring-closed-flags` is a *predicate* a peer writes, not an index.
  Wrapping the flag word changes nothing: the fix is to stop reading a
  publication as a channel and ask the side that knows.
- `netd-writable-virtqueue` is a *mapping*, not a value. No type on the read
  side helps while the descriptor table is inside the page netd maps writable;
  splitting the pool the way `virtio_sound::init` does is the whole fix.

Both are still open in their own files and neither is blocked on anything here.
