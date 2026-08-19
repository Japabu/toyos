---
status: open
kind: defect
opened: 2026-08-08
---

# `dosfstools`, which was refused, is installed by a committed workflow

`.github/workflows/probe-toolchain.yml:39` installs it beside `qemu-system-x86`
and `ovmf`. It only runs on `ci/probe-toolchain` pushes, so it is not on any
path `main` takes — but it is committed, and a refused dependency left in the
tree is how it comes back. The refusal is not in doubt: the owner turned down
`fsck.vfat` on 2026-08-08 together with the `/sbin/fsck_msdos` it would have
replaced, and the FAT32 gate's outside judge is `toyos-fat32-check/` — ours,
written from Microsoft's fatgen103 — precisely so that no host binary is needed
to judge a volume. This workflow is the one place in the tree that does not know
that.
