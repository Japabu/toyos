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
(`specs/issues/hardware/eleven-names-red-on-ci.md`, 1/5 on CI) is a different
thing entirely: beats that dropped a CPU from the mask, from a machine that
booted.

Not reproducible on that tree: green on the harness's own alone re-run and green
on five further runs of `cargo test --test toyos-build -- kernel_heartbeat`, six
for six. It landed immediately after `xhci_flap`'s red in the same serial phase,
with two other agents building on the host.

The defect is the diagnosis, not the boot. A QEMU that exits 0 before the ready
marker should say why — whether it never got its arguments, could not open a
device, or was reaped by something — and the harness should keep whatever it did
write. Today the test's name is all the evidence there is, which is how a real
one-in-N boot failure would be indistinguishable from a host hiccup.
