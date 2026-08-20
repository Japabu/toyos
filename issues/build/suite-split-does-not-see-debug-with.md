---
status: open
kind: defect
opened: 2026-08-20
---

# `suite_split` reads `syscall::debug(` and not `syscall::debug_with(`

`needs_actuators` in `tests/toyos.rs` decides which shared-boot binaries are
held off the shipping kernel, on three spellings:

```
text.contains("SYS_DEBUG")
    || text.contains("syscall::debug(")
    || text.contains("census::Census")
```

`syscall::debug_with(` matches none of them. A binary whose only route to
`SYS_DEBUG` is `debug_with` — every action that takes an argument, so
`TLB_ACK_DELAY_ARM`, `CENSUS_KIND`, `LOWER_SYSINFO_BOUND` and
`SLOT_TO_LAST_GENERATION` — reads as innocent and is put on the shipping boot,
where the syscall answers `InvalidArgument`. That is the exact failure the check
exists to prevent, stated in its own doc comment: "a binary that gains a
`debug()` call and no entry would run on the shipping kernel ... and a test whose
verdict is that a process died would then fail for a reason with nothing to do
with what it is about."

Nothing is misclassified today. The two direct `debug_with` callers —
`handle_basic` and `handle_kill_policy` — also carry `census::Census`, which is
the third spelling, so both land on the actuator kernel by the marker that has
nothing to do with the call they make. The gap is that the coverage is a
coincidence: delete the census arm from either and the binary silently moves to
the shipping kernel.

The check's own negative control does not catch this, because the staged input
spells the call `syscall::debug(3)` — the one form the predicate matches.

Adding `syscall::debug_with(` to the alternation is the fix, and the staged
input wants a fifth row using that spelling so the control has teeth against
this hole rather than only against the first.

Found while adding `SLOT_TO_LAST_GENERATION`, whose two callers happen to be
covered.
