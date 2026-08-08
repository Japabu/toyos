---
status: open
kind: defect
opened: 2026-08-08
---

# The HDA ring fix is unverified on the T14

Filed out of the `repeated completion for free buffer` entry when that closed in QEMU.

QEMU's `intel-hda` and the T14's controller are different devices, and the fix
was measured on the first. What the next metal boot must show: `tone` playing to
completion with soundd alive, `deferred=0`, and `underruns` nonzero only if the
client stalls.

A `the engine completed 0x.., which is no walk of an 8-period ring` panic would
be **new**, and would be the first evidence that `SDnLPIB` is the wrong position
source there — `specs/hda-driver-plan.md` §2.4's `position_fix` paragraph is
what it would put in question.
