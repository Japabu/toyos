---
status: owner
kind: question
opened: 2026-08-01
---

# The `bcachefs/` crate does not implement bcachefs — a question for the owner

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
