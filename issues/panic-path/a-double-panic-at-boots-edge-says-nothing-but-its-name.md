---
status: open
kind: defect
opened: 2026-08-19
---

# A double panic at boot's edge, and it says nothing but its name

Two findings in one sighting, dev host under load (two worktrees' full
suites interleaved over the shared twelve guest slots), 2026-08-19 22:21 UTC.
`src/redlist.rs` carries the row.

**1. The kernel double-panicked under load.** `log_poll_outlives_a_close`,
parallel phase, 25 s: the guest went quiet with every CPU halted, and the
harness's verdict names it — "the panic is the finding and the guard never got
to be one." Alone the same test is green; the harness notes the run stays red
on the classification. Reclassifying the test would bury the real story: some
first panic fired near `t=0.991 s` on cpu0 — boot's edge, where the log ring,
its drains and the poll registration all come up — and then the panic path
panicked too. The log subsystem's known sins live in exactly that
neighbourhood (an unbounded `BackendGuard::lock` spin with interrupts off is
one of the redesign track's own exhibits), and the redesign is approved and
sequenced; this sighting is evidence for it, and a reproduction recipe:
`cargo test` in two worktrees at once.

**2. The double-panic path reports nothing.** The kernel's complete last words
were `[kernel 0.991 cpu0] DOUBLE PANIC` — not what the first panic said, not
where, not what the second one was. A report that names a kernel death is the
tree's own fresh standard for the harness side; the kernel side of a *double*
panic has no report at all, so the one class of crash that is by definition
two bugs deep is the one class that leaves no evidence. Even a fixed-size,
pre-reserved line naming the first panic's location would have turned this
sighting from a mystery into a lead.

What the sighting does not establish: what the first panic was. The capture is
`scratchpad/hkpfix-harness.log` in the 2026-08-20 orchestrator session; the
durable evidence is quoted here and in the redlist row.
