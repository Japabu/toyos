---
status: open
kind: finding
opened: 2026-08-13
---

# The CI runner's QEMU moved to 11.1.0, and the declaration moves with it

`debian:sid` is a rolling release, and `.github/qemu-version`'s own header
names exactly this failure mode. PR #36's `ci` run (`31708143758`, 2026-08-13
14:03 UTC) was green on QEMU 11.0.3; PR #42's `ci` run (`31713425110`, 15:06
UTC) hit 11.1.0 instead, and every guest shard, `cache-writer` and
`guest-suite` refused by name — `.github/instrument.sh`'s version-drift
tripwire working exactly as designed, not a defect in either branch:

```
this job runs QEMU '11.1.0' and .github/qemu-version declares 11.0.3.
The QEMU version decides test outcomes, so a number taken on one is not a
number about the other, and the dev host's baseline is recorded on 11.0.3.
debian:sid is a rolling release and this is what it moving looks like —
nothing here is about the tree.
```

The declaration exists because the container was pinned to one QEMU version on
purpose, so that CI and the dev host differ in the accelerator and in nothing
else; a version that moves under it has to move by a deliberate act, and this
commit is that act: the
declaration moves to `11.1.0` in the same commit that records why, so no
number taken after it is silently read as comparable to one taken before —
any comparison spanning the two is cross-instrument. Whether any test's
outcome actually differs under 11.1.0 — the cited precedent is
`desktop_typing_damage`, red on 8.2.2 and green on 11.0.3 — surfaces through
the durations gate and the ordinary suite from here on, and is adjudicated
there, not by this entry.
