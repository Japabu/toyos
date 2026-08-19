---
status: open
kind: defect
opened: 2026-08-19
---

# `hda_two_live_refused`: QEMU exited 0 before `===READY===`, on the dev host, once

One sighting, recorded because it is an audio guest dying at boot and this tree
does not read those as noise.

```
thread '<unnamed>' panicked at tests/common/qemu.rs:3242:17:
[qemu] QEMU died before ===READY=== (status: Ok(ExitStatus(unix_wait_status(0))))
FAIL hda_two_live_refused: ... (8s)
ALONE hda_two_live_refused: GREEN
```

**Exit status 0 is the interesting half.** `screendump`'s own comment says it:
"a guest that triple-faults exits QEMU (`-no-reboot`)". A guest that was killed
would carry a signal, and a QEMU that could not start would carry an error and a
non-zero status. So the most likely reading is that this guest reset itself
before init ever spoke.

**What it correlates with.** Three full `cargo test` runs on the same dev host,
2026-08-19, in this order:

| tree | host | result |
|---|---|---|
| with the TLB-derivation change | cold: 110 C tests and 3 kernels compiling beside the guests | 267 passed, 1 failed (113.3 s) |
| without it (`git stash`) | warm | 268 passed (52.9 s) |
| with it again | warm | 268 passed (47.6 s) |

So it followed the load and not the diff — but one sighting under load is not a
diagnosis, and "it was busy" is not one either. The guest's own UART log is
`$TMPDIR/toyos-tests-<pid>/lane-N/uart-<seq>.log` and is the first thing to read
if it happens again: a triple fault before `===READY===` leaves whatever the
kernel had already printed.

`cargo run -- --known-red hda_two_live_refused` says NOT KNOWN-RED; its one
redlist entry is retired and carries a different signature ("presenting a null
sink" never reached the boot console, closed 2026-08-08).
