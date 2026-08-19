---
status: open
kind: defect
opened: 2026-08-18
---

# The sites that still carry a boundary-crossing number in a plain integer

`toyos-untrusted`'s `Untrusted<T>` exists and the virtqueue used ring uses it:
`Virtqueue::used_ring_id` and `::used_ring_len` (`kernel/src/drivers/virtio.rs`)
are the only reads of a used ring in the kernel and hand back `Untrusted<u32>`,
so `Virtqueue::parse_used` is the only path to an index or a length and it names
its bound at both. The type has no arithmetic, no `Deref`, no `From`, no cast
and no accessor; the four `compile_fail` doctests on `Untrusted` are what that
sentence means, and `src/sourcegate.rs` bans the one shape typing cannot stop
(`at_most(u64::MAX)` and its four siblings).

**The rest of the class has not adopted it.** Until a site does, its bound is a
thing an author has to remember rather than a thing the compiler asks for. Each
of these is mechanical — find the read, wrap it there, name the bound at the use
— and none needs a design decision. **Each is cited by file and enclosing
symbol**, so whoever takes this does not have to re-derive which value is meant.

## The sites, hardest consequence first

- **`kernel/src/arch/syscall.rs`, the dispatch itself.** The largest site and the
  one closest to userland. `UserAddr::checked` (`kernel/src/mm/mod.rs`) already
  covers the *addresses*; the lengths beside them are bare. 32 `as usize` casts
  in that file, of which the ones that matter are the arguments that become a
  copy length or an allocation size — `SYS_SET_THREAD_NAME`'s `a2` is the
  pattern (it `min`s against `THREAD_NAME_LEN`, which is a clamp where the rest
  of the file refuses). Bounds already exist and are already named:
  `user_ptr::MAX_USER_STR` for anything string-shaped, `mm::MAX_HEAP_ALLOC` for
  anything that reaches the allocator. Exit: `at_most`. Start with the calls
  that size a `Vec` before they check it.

- **`kernel/src/drivers/xhci/mod.rs`, the event ring.** `XhciController::next_event`
  returns a `Trb` the *controller* wrote, and `dispatch_event` reads a slot id
  and a completion code out of it. `XhciController::device` already does the
  right thing by hand — `(slot_id as usize).checked_sub(1)?` and then an index —
  which is exactly the shape `index` replaces. The scratchpad fields near
  `Layout::new` are masked with `0x1F`, a clamp, which is the answer this type
  exists to stop being the answer. Bound: the controller's own `max_slots`.

- **`kernel/src/drivers/nvme.rs`, the completion queue.** `alloc_cid` hands out a
  command id and `wait_completion` never compares `cq.cid` against it —
  the kernel-drivers type-safety audit's §5 recorded this as
  "decorative until the queue goes asynchronous". Sound today only because every
  submission is synchronous, which is a property of the caller and not of the
  parse. `IdentifyNamespace` is read straight out of DMA in the same file; that
  struct's sector-size refusal is already there and is the model to copy.

- **`kernel/src/drivers/acpi.rs`.** Table lengths from firmware, with the two
  subtractions that underflow — F5 of the same assessment has the analysis.
  Note this one is *firmware*, not a device or a peer, and it runs once at boot:
  it is on this list because the bound is missing, not because userland can
  reach it.

- **`bcachefs/src/btree.rs`'s `collect_all`.** The residual
  `untrusted-input-panics` records. **This one is not a wrapper fix**: it
  materialises every entry in the tree before anything counts them, so what it
  needs is a count primitive that lets `BcacheFsAdapter::list` refuse *before* it
  allocates, the way `TmpFs` already does. `/home` is writable by userland, so it
  is a live path. It stays the bcachefs owner's.

## What this type does not answer

Recorded so nobody tries to make it. Two open entries in this area are **not**
this class, and wrapping something would touch neither:

- **`issues/isolation/netd-trusts-ring-closed-flags.md`** — a *predicate* a
  peer writes, not an index. `RingHeader::flags` lives in the page `SYS_PIPE_MAP`
  maps writable, and netd reads `is_reader_closed`/`is_writer_closed` as facts
  about its peer. There is no bound to compare a bit against. The fix is to stop
  reading a publication as a channel: the kernel's own `readers`/`writers` counts
  are the side that knows, and they already surface as EOF on a read and
  `BrokenPipe` on a write.
- **`issues/isolation/netd-writable-virtqueue.md`** — a *mapping*, not a
  value. `virtio_net::init` registers the whole 2 MiB `DmaPool` page as one
  shared region, so the RX descriptor table at offset 0 and the TX virtqueue at
  `0x3000` are inside what netd maps writable. No read-side type helps while
  netd can rewrite the descriptors the device DMAs into. The fix is the one
  `virtio_sound::init` demonstrates one file over: two pools, with the descriptor
  tables in the one that is never registered. `NicInfo` addresses everything by
  offset already, so it needs no ABI change.

Both are still open in their own files, and neither is blocked on anything here.

Also untouched and deliberately so:
**`issues/isolation/kernelslice-over-user-memory.md`** — its own text says
M2 (#159) closes the aliasing, and that converting the borrow first would
describe a hazard M2 removes.

## What this type is not about

It carries no *time* and no *rate*, and nothing in it reasons about how long
anything takes. The TCG-versus-KVM p90 divergence PR #119 measured (4–8× in p90,
20× in max for scheduler pass cost on the dev host) does not reach any bound
here: every one is a byte count, a table length or a register encoding, and each
is compared against a number the driver itself published in the same function.
Recorded so that nobody re-checks it.
