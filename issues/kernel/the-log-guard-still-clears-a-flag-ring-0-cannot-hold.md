---
status: open
kind: finding
opened: 2026-08-22
---

# `LogCommitGuard::close` clears `TF`, which Ring 0 can no longer have set

`kernel/src/arch/mod.rs`'s `LogCommitGuard::close` reads `RFLAGS`, clears `IF`,
and then — `if rflags & TF != 0` — writes a second masked word back with `TF`
cleared too. The branch was load-bearing: `IA32_FMASK` did not name `TF`, so a
Ring 3 thread that set it entered the kernel single-stepping, and the `#DB`
handler logged and returned, which let a trap reserve a whole newer log
generation while the interrupted writer was halfway through its slot body.

Neither half is true since 2026-08-22. `arch::syscall::init` puts `TF` in the
`SYSCALL` mask and every interrupt and trap gate clears it, so no Ring 0 code in
this kernel runs with `TF` set; and `#DB` from Ring 3 ends the process rather
than reporting. The branch is therefore unreachable — a `pushfq` read, a test,
and a `push`/`popfq` pair that never executes, on the path every `log!` takes.

**Not deleted with the change that made it dead**, deliberately: it is inside
the log's publication bracket, whose negative control is the `log-unbracketed-reserve`
actuator and whose gates are `log_nested_emit` and the three `log_conservation_*`
names — a different subsystem from the one that PR was about, and the wrong
place to make an unmeasured edit.

**Exit condition**: delete the `if rflags & TF != 0` branch and its `masked`
`popfq`, leaving `close` as the `pushfq`/`cli` pair it was before `TF` mattered,
with `log_nested_emit` and `log_conservation_smp{1,4,8}` green. The claim to
check first is the one this rests on: that nothing reaches `LogCommitGuard` from
a context with `TF` set — `emit` is kernel-only, and the two ways in are a
`SYSCALL` whose mask now names the bit and a gate that clears it.
