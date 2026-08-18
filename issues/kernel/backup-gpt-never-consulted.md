---
status: open
kind: defect
opened: 2026-08-01
---

# The backup GPT is never consulted

`toyos_gpt::locate` reads the protective MBR, the primary header at LBA 1 and
the primary entry array, and refuses if any of them fails its checks. UEFI puts
a full second copy at the end of the device precisely so that a torn write to
the front is recoverable, and nothing here looks at it: a single bad block at
LBA 1 makes a perfectly good disk unidentifiable.

Not a safety hole — the failure mode is a refusal, which is the safe direction —
but it is the difference between "this stick is worn" and "this stick is
unusable", on the machine class ToyOS boots from. The cost is a second
`parse_header` call against `lba_count - 1` and a second array walk; the design
question is what to do when the two copies disagree, and the answer is almost
certainly to refuse rather than to pick, since a disagreement means one of them
describes a disk this is not.
