---
status: open
kind: finding
opened: 2026-08-03
---

# A QMP-driven test cannot share a boot with another one

The kernel's log ring sits one line behind on an idle machine
(`specs/issues/kernel/log-ring-flushes-one-line-behind.md`), so a guest
that exits the instant it has its answer leaves its last lines — including the
runner's `===TEST_END===` — in the ring until something else runs. On a shared
boot the next member then opens its console window over output the previous one
is still draining into, and reads the wrong thing: measured 2026-08-03 as the
first member passing, the second timing out with its own complete and correct
output visible in the serial, and the third failing instantly on an empty window.

Two workarounds are in the tree, and they are workarounds. `keep_the_ring_moving`
in `tests/toyos.rs` injects keys nothing is listening for, purely so the ring
keeps draining; and the four layout tests take a boot each rather than a group,
which costs three boots. The fix is
`specs/issues/kernel/log-ring-flushes-one-line-behind.md`'s — a drain that does
not need the machine to be busy.
