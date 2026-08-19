---
status: open
kind: track
opened: 2026-08-06
---

# Every user mapping is writable and executable, and PCID tags are still recycled live

Two staged pieces of the memory boundary never started. The validated boundary,
the copy-in/copy-out surface, the pure span arithmetic and the acknowledged TLB
shootdown all did.

**W^X does not exist.** `vma_map` hands `writable = true` to every mapping,
including every `dlopen` and every pipe region; there is no protection type and
`EFER.NXE` is set nowhere. The cheap version is ruled out by measurement:
2 MiB-granular W^X protects **39.2 % of 30.64 MiB of text and 0 % on 11 of 15
binaries**, because `toyos-ld` emits exactly three `PT_LOAD`s at page alignment,
which gives every binary exactly one mixed 2 MiB window. The measured
alternative is a hybrid — 4 KiB tables for the mixed window only, costing **one
page table per window: 15 windows, 60 KiB across the boot set** — against
2 MiB-aligning the linker's segments, which would cost **+4 MiB physical per
process**. The 4 KiB guard-page path already in `mm/paging.rs` is the reuse
target. Blocked on nothing in the tree; blocked on the T14 for the one number
TCG cannot give, which is what the extra TLB pressure costs.

**PCID tags wrap.** `alloc_pcid` still counts up and restarts at 1, so a tag is
reissued while its address space is live. It is mitigated rather than fixed: the
recycle now does an acknowledged shootdown outside the lock. Making the tag an
owned resource with a free list deletes the branch. Blocked on nothing except a
machine shape with `+pcid,+invpcid`, without which any test of it is vacuous.

A related residue nothing records: **`AddressSpace` has no `Drop` at all**, so
teardown frees page tables with no shootdown. That is sound only because no PCID
means every CR3 write flushes — and PCID ownership is exactly the change that
removes the reason.

Two invariants the built half rests on, worth not breaking:

- The IF=0 deadlock class is closed **by the shootdown target polling, not by
  the initiator abstaining**. A spin lock that does not poll re-opens it.
- `+smep` is on in both harness arms and **nothing asserts it**, so deleting the
  argument is a silent regression
  (`issues/build/smep-unasserted-anywhere.md`).
