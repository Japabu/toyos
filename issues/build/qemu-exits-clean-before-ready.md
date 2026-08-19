---
status: open
kind: defect
opened: 2026-08-11
---

# A guest QEMU exited 0 before `===READY===`, and the harness has no account of it

Seen once on 2026-08-11, on `wt/toyos-std` at 7a8c12d, in the serial phase of a
full `cargo test`:

```
FAIL kernel_heartbeat: [qemu] QEMU died before ===READY=== (status: Ok(ExitStatus(unix_wait_status(0))))
  FAIL  kernel_heartbeat  (2s)
```

`qemu.rs:2852` is the panic. What makes it worth a file is the **status**: QEMU
exited *successfully*, two seconds in, before the guest said anything at all —
so this is neither a crash nor a wedge nor a timeout, and the capture holds no
serial output to bisect. The recorded `kernel_heartbeat` red
(`issues/hardware/eleven-names-red-on-ci.md`, 1/5 on CI) is a different
thing entirely: beats that dropped a CPU from the mask, from a machine that
booted.

Not reproducible on that tree: green on the harness's own alone re-run and green
on five further runs of `cargo test --test toyos-build -- kernel_heartbeat`, six
for six. It landed immediately after `xhci_flap`'s red in the same serial phase,
with two other agents building on the host.

**Four names in one day, on three worktrees, 2026-08-15.** The signature is not
`kernel_heartbeat`'s and it is not one tree's:

- `screen_fatal_halt` and `double_fault_stack`, both in the 106.4 s parallel
  phase of one full `cargo test` on `wt/toyos-ciwall` (the one-accumulator
  tree, landed as `81cfe22`), each `[qemu] QEMU died before ===READY===
  (status: Ok(ExitStatus(unix_wait_status(0))))` from `tests/common/qemu.rs`,
  in a run that was 256 passed and 4 failed. Both `ALONE: GREEN`, and both
  green again when run by name minutes later — 3 s and 2 s.
- `log_backing_read_error`, the identical message, on `wt/toyos-logd56`'s suite
  the same afternoon. `ALONE: GREEN`.
- `screen_console_shell`, the same *exit status* through a different wait —
  `[qemu] QEMU died before the screendump (status: exit status: 0)` — on
  `wt/toyos-capwin`'s suite. `ALONE: GREEN`.

**Neither the guest nor the phase's width explains it.** The four names have
nothing in common but a boot: two panic-screen tests, a fault test, a log-device
test and a console test, on three different configs. The ciwall run's own load
proxy says the host was not slow — `host: fastest boot 1380 ms against the
reference 1320 ms — liveness ceilings paid at 1.05x width` — while its
`[host-slots]` lines name a second worktree's suite holding guest slots beside
it (`all 12 held by 2 holder(s): pid 103 (toyos-ciwall: sched_check_build),
pid 3077 (toyos-capwin: c_capture_ignores_daemon_lines)`). So what is shared is
another suite on the machine, not a margin any one guest was near.

The defect is the diagnosis, not the boot. A QEMU that exits 0 before the ready
marker should say why — whether it never got its arguments, could not open a
device, or was reaped by something — and the harness should keep whatever it did
write. Today the test's name is all the evidence there is, which is how a real
one-in-N boot failure would be indistinguishable from a host hiccup.
