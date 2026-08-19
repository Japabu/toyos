---
status: open
kind: finding
opened: 2026-08-01
---

# A handle's fingerprint cannot survive a delete-and-recreate under the same name

`File` identifies its directory entry by the 8.3 field plus the five creation
stamp bytes, and `Fat32::live_entry` checks it on every `write`, `set_len` and
`flush_meta`. That catches the slot being taken by a *different* file, which is
the dangerous case and the one the audit demonstrated (F2: the guard compared
first clusters, and 0 is every unwritten file's first cluster, so a slot
refilled by another empty file matched and the stale handle rewrote the
newcomer's entry — with `fsck_msdos` calling the volume clean).

What it still cannot distinguish is a file deleted and recreated under the same
name with the same creation timestamp, because FAT has nowhere to put a
generation number. The stamp's resolution is 10 ms, so a caller that stamps
from a real clock is safe and a caller that passes a constant — every test in
this crate does — is not. The kernel adapter should hold handles for as long as
its own file objects live and no longer, rather than relying on this to be a
generation counter, which it is not.
