---
status: open
kind: finding
opened: 2026-08-01
---

# Two things `fsck_msdos` does not check, found by breaking the code on purpose

Recorded because it generalises past this crate: **a host validator's silence
is evidence about the validator, not only about the code.** Sixteen deliberate
breakages were run against the suite. Fourteen went red. Two did not, and
neither was harmless:

- **A stale FAT mirror.** `fsck_msdos -n` does not compare the FAT copies, and
  a mount reads only the active one — so a driver that updates FAT 0 and
  leaves FAT 1 behind passes fsck, passes a real mount, and passes every
  read-back test, while leaving a volume that reads differently the moment
  anything consults the mirror.
- **Duplicate 8.3 names.** Neither fsck nor a mount looks at short names; both
  use the long ones. Dropping short-name uniquification entirely was invisible.

Both now have a test that reads the raw bytes off the device
(`every_fat_copy_stays_in_step`, and the tail of
`colliding_short_names_stay_unique`), and both mutations go red.

Related, and the reason the gate does not read an exit code: **`fsck_msdos -n`
exits 0 while printing `Fix?` for problems it declined to repair, and exits 0
on a volume it has just declared dirty.** `common::Image::fsck` matches the
output line by line against the exact shape of a clean run instead. A gate
written the obvious way would have been green on a corrupt volume.

**The 2026-08-01 audit is the sequel to that paragraph and the sharper version
of it.** Sixteen breakages of the *code* caught fourteen; an independent
auditor attacking the *state space* instead — a file that is empty, a chain
that is cyclic, an entry that is crafted — found six more, four of them on the
write path and one that wrote 256 GiB outside the volume and returned `Ok(())`.
Every one of them was reachable through the public API on a volume this suite
already had. The lesson that generalises past FAT32: **mutating the
implementation tests the paths you wrote; it says nothing about the states you
did not think to construct.** Both are needed, and the second is the one a
green suite hides.
