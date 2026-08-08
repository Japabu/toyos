---
status: open
kind: defect
opened: 2026-08-08
---

# This suite still formats and populates through two macOS binaries

**The judge is ours as of 2026-08-08.** `fsck_msdos` is gone from all three
places it was used — `src/image.rs`, `tests/common/volumes.rs` and
`toyos-fat32/tests/common/mod.rs` — replaced by `toyos-fat32-check/`, written
from fatgen103 and derived from neither our writer nor our reader. The owner's
rule that made that mandatory: "no dependencies on binaries that dont come with
rust or qemu". `what-fsck-msdos-does-not-check` — the FAT mirror and duplicate 8.3 names,
both of which `fsck_msdos` silently accepted — are among the twelve corruptions
the new checker catches and it did not.

**What is left is `newfs_msdos` and `hdiutil`**, and they are the harder half
because they are not a judge. `Image::formatted` shells out to
`/sbin/newfs_msdos -F 32` through a `hdiutil` device node, and the volumes are
populated through a real macOS `msdosfs` mount. That is the *point* of this
suite — our reader against bytes we did not write, and our writer's output read
back by a driver that is not ours — so replacing them is not "write a
formatter", it is deciding what independent implementation takes their place.
Consequence today: `cargo test` in this crate runs on the owner's laptop and
nowhere else, `host-tests.yml` is on `macos-latest` for this reason alone, and a
Linux contributor cannot run the FAT32 suite at all.

Scope, if someone takes it. `fatfs` is an ordinary crates.io crate already in
this tree's dependency graph and is a genuinely independent FAT32
implementation: it can format a volume and write files into one, which covers
both roles. What it does *not* give is what `msdosfs` gives — a second reader
written by people with no sight of our code — so a suite built on `fatfs` alone
tests our writer against one other implementation rather than against the
platform. Whether that is enough is the decision, and it is the owner's. Note
that `src/image.rs` already uses `fatfs` for exactly one thing (formatting the
empty volume), so the precedent exists and its limits are recorded there.

Nothing in the *guest* suite is affected: `tests/common/volumes.rs` needs no
formatter, only the judge.
