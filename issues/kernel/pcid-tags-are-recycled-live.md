---
status: open
kind: track
opened: 2026-08-20
---

# PCID tags are recycled while their address space is live

What is left of the memory-boundary track. The validated boundary, the
copy-in/copy-out surface, the pure span arithmetic, the acknowledged TLB
shootdown and — on 2026-08-20 — W^X all landed; this did not.

**PCID tags wrap.** `alloc_pcid` still counts up and restarts at 1, so a tag is
reissued while its address space is live. It is mitigated rather than fixed: the
recycle now does an acknowledged shootdown outside the lock. Making the tag an
owned resource with a free list deletes the branch. Blocked on nothing except a
machine shape with `+pcid,+invpcid`, without which any test of it is vacuous —
the dev host boots `-cpu qemu64`, so `pcid_active()` is false and every
`INVPCID` path in the kernel is dead locally.

A related residue nothing records: **`AddressSpace` has no `Drop` at all**, so
teardown frees page tables with no shootdown. That is sound only because no PCID
means every CR3 write flushes — and PCID ownership is exactly the change that
removes the reason. W^X added one page table per process to what teardown drops:
the split window's, which `children` owns and which goes the same way as the
rest.

Two invariants the built half rests on, worth not breaking:

- The IF=0 deadlock class is closed **by the shootdown target polling, not by
  the initiator abstaining**. A spin lock that does not poll re-opens it.
- `+smep` is on in both harness arms and **nothing asserts it**, so deleting the
  argument is a silent regression
  (`issues/build/smep-unasserted-anywhere.md`). `EFER.NXE` is deliberately not
  in that class: `mmap_prot`'s `exec-heap` and `exec-stack` children die of that
  bit, so a kernel that stopped setting it reds instead of quietly losing half
  of W^X.
