---
status: open
kind: defect
opened: 2026-08-01
---

# `probe()` mounts on a checksum, and a stamp over a used volume does not reformat

Two things, from reading `bcachefs_adapter::probe` against the crate:

**The threshold does not match the consequence.** `Storage::Ours` is a
read-write mount: `sync()` rewrites both superblocks, and any file operation
writes the bitmap, btree nodes and data. So mounting a stranger's disk modifies
it, which is a weaker form of the wrong the designation stamp exists to prevent.
Accidental collision is not the risk — random block-0 bytes satisfy 4 bytes of
magic, 4 of version and a 32-bit CRC with probability about 2^-64 — and neither
is a *genuine* upstream bcachefs volume, which does not begin with ASCII `BCFS`
(this crate shares the name and nothing else; `issues/kernel/`). The risk is a **deliberately
crafted block 0** on a disk somebody hands you, which is the metal track's
situation exactly.

Recommendation, for the owner to decide:

- **Now, nearly free:** tighten `Superblock::check` from
  `block_count <= device_blocks` to `==`. `format` already writes the device's
  own size, so a volume image copied onto a different disk stops mounting, the
  same property the designation stamp's block count gives a format. It is not
  authentication — an attacker who knows the disk size writes the right number —
  but it costs one character and removes the accidental cases.
- **Then:** close `bcachefs-untrusted-input-holes`' residuals 1–3. "Mounting a
  hostile volume is merely rude" is not true while an unchecked extent reaches a
  block read.
- **The real fix, if the threat model wants one:** read-write requires
  something the attacker cannot compute — a keyed MAC, or a designation-like
  stamp — and everything else mounts read-only. ToyOS has no key store and no
  TPM support, so this is a metal-track decision, not a patch.

**Separately, and reproduced:** a designation stamp written over a disk that
already held a ToyOS volume does **not** cause a reformat. `designate_for_format`
writes block 0 only, `Superblock::read` falls back to the backup superblock at
the last block when block 0 does not parse, and a stamp does not parse — so
`mount()` succeeds from the backup and `probe()` returns `Ours`, mounting the old
volume. Harmless for the harness, which stamps freshly created sparse files, but
it means "re-stamp the disk to reformat `/home`" is not a workflow that works.
`probe`'s doc comment claims the decision comes "from one read of block 0"; it
comes from two, and the second one wins.
