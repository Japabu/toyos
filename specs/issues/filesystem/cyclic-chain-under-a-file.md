---
status: open
kind: finding
opened: 2026-08-01
---

# A cyclic chain under a *file* is bounded, not detected

`fat.rs::advance` walks a chain by a step count derived from something the
chain cannot influence — a file's size field, `MAX_DIR_ENTRIES`, the volume's
cluster count — so a cycle costs a bounded number of FAT reads and never a
hang or an unbounded allocation. A self-loop (`c → c`) is rejected because the
comparison is free. A longer cycle under a file is not: the read returns that
file's own earlier bytes again, within its declared size.

Detecting it on the read path needs either a tortoise-and-hare, which doubles
the FAT reads on every sequential access, or a full walk from the head at open
time — and the second is incompatible with the position hint that makes
sequential access O(1) rather than O(n²) in the first place.

**The write path does detect it, and this entry used to claim more than was
true.** The original wording said the damaging cases were all covered by
`free_chain`, `chain_len` and `chain_last`. `free_chain`'s cycle detection is
"a revisited cluster reads as free" — it needs the walk to *revisit*, and
`truncate_chain` writes an end-of-chain marker at the cluster it is keeping,
which is an exit the walk takes instead. The audit
(`specs/type-safety-audit/storage-stack.md` F3) demonstrated `set_len`
returning `Ok(())` having freed every cluster the truncated file still needed,
with the directory entry still naming the first of them. A residual that
overstates what is detected is worse than one that admits the gap, because
nobody re-checks it.

What holds now: `truncate_chain` is preceded by `Fat32::verify_acyclic`, a
tortoise-and-hare that runs **before** anything is written, so a cyclic chain
leaves the volume untouched. It is the only cycle detection in the crate and
it is affordable exactly where the read path's is not — truncation is one
operation that already walks the whole chain, rather than one per page.
`free_chain` also takes an anchor now, which guards the last retained cluster;
that alone was not enough, because the audit's cycle closed above it.
`chain_len` and `chain_last` do bound a directory that never ends, and that
part of the original claim was correct.

Two tests pin the split: `a_longer_cycle_is_bounded_rather_than_endless` (read
path, bounded, no error) and
`truncating_a_cyclic_chain_does_not_free_the_clusters_it_keeps` (write path,
refused with nothing freed).
