---
status: open
kind: defect
opened: 2026-08-01
---

# The `bcachefs/` crate does not implement bcachefs

ToyOS's `bcachefs/` crate implements a ToyOS-native on-disk format written from scratch.
It shares a name with Linux bcachefs and nothing else: ours is `MAGIC = b"BCFS"` plus
`DESIGNATION_MAGIC = b"TOYOS-FORMAT-ME\0"` (`superblock.rs:5,24`) and `NODE_MAGIC = b"BTND"`
(`btree.rs:7`), against upstream's UUID-based `BCHFS_MAGIC` / `BSET_MAGIC ^ sb.uuid` /
`JSET_MAGIC`.

`specs/reference/bcachefs-reference.md` — real research into the *upstream* format — now carries a
warning saying so at the top, because its filename in this repo is a trap. That fixes the
document; it does not fix the collision. A crate that does not implement the format it is
named after is a hazard we keep paying for, in exactly this way. Renaming it is the owner's
call, not something to do in a docs pass.

## Answered by the owner, 2026-08-15

> *"bcachefs is the default filesystem for toyos. the crate must be an
> implementation of the spec."*

The resolution is neither a rename nor a deletion. **`bcachefs/` must become a
real implementation of the bcachefs on-disk format, and that format is ToyOS's
default filesystem.** The name stops being wrong by the crate growing into it.

This entry therefore stops being a question and becomes work: what is owed is the
implementation, and the gap between the two formats recorded above is the measure
of it. `specs/reference/bcachefs-reference.md` changes standing with the ruling —
it stops being research into a format we do not implement and becomes the
description of the one we must; whoever picks this up reads it as a source rather
than as a trap, and retires the warning at its top when that is true.

The track itself is not planned here.

Two facts about the tree that a real-bcachefs track inherits whichever way it is
sequenced, both measured on 2026-08-15 and recorded in
`specs/assessments/2026-08-15-mechanism-consolidation-audit.md` §1.4: the kernel
must parse the root format to reach `/bin/init` at all (`kernel/src/main.rs:591`,
and `kernel/src/bcachefs_adapter.rs:543` `.expect()`s the mount), and
`specs/plans/boot-image-split.md` stage 2 is not done. That same section carries
the defect history of the current format and the observation the ruling inverts —
a home-grown format has no second implementation to be judged against, and
upstream bcachefs is exactly such a judge.
