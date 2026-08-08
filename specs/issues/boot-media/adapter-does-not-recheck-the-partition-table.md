---
status: open
kind: finding
opened: 2026-08-01
---

# What the adapter does *not* re-check about the partition table

`toyos-gpt`'s own residuals (a `last_usable_lba` that may cover the backup GPT,
and two entries in one table sharing a unique GUID resolving first-wins) are in
`specs/type-safety-audit/storage-stack.md` and are the parser's to fix. The
adapter deliberately does not duplicate them: it cannot know whether an extent
is *right*, only whether it is being respected, and two copies of a rule that
can disagree is worse than one. What it does enforce is that no I/O leaves the
extent it was given — and, tighter, that none leaves the FAT volume inside it,
since `Fat32::probe` reads the sector count before anything can write.
