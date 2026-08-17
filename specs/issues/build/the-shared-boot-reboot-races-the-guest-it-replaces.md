---
status: open
kind: defect
opened: 2026-08-17
---

# The shared-boot reboot starts its replacement before dropping the guest it replaces, and the two want one NVMe image

`tests/toyos.rs`'s shared-boot block answers a guest that stopped answering with
a new one, which is right and is what stops 150 tests each paying a full ceiling
for a machine that is gone. The line that does it is

```rust
qemu = boot();
```

and Rust evaluates the right-hand side first. So the replacement QEMU is
launched — and `wait_for_ready` waits on it — while the old `QemuInstance` is
still alive, still holding its lane's `test-nvme-*.img` open for write. The new
one cannot open the image, exits 1 before it says anything, and
`wait_for_ready` panics `QEMU died before ===READY===`. That panic escapes the
block's `catching`, so **every test still owed a verdict is reported red on it**.

Measured 2026-08-17, dev host, `wt/toyos-panicstall`, one full `cargo test`:

```
FAIL sched_stress: STALLED: 88s of guard expired, and the guest had said nothing
  for the last 88s of it
  ---- the shared boot stopped answering before shm_release_reclaims; rebooting (1/3) ----
qemu-system-x86_64: -device nvme-ns,drive=nvme0,bus=nvme0ctl,logical_block_size=512,
  physical_block_size=512: Failed to get "write" lock
Is another process using the image
  [/var/folders/.../toyos-tests-94831/lane-0/test-nvme-134217728.img]?

thread '<unnamed>' panicked at tests/common/qemu.rs:3107:17:
[qemu] QEMU died before ===READY=== (status: Ok(ExitStatus(unix_wait_status(256))))
```

**129 of the run's 131 reds carry that one sentence** — `132 passed, 131 failed
… 263 total (511.1s)` — against two real ones: `sched_stress`, whose death is
`specs/issues/kernel/the-shared-boot-jumped-to-null-spawning-sched-stress.md`,
and `sched_check_build`'s known dev-host invariant-P red. So the recovery
mechanism turned one lost guest into a lost suite, which is the outcome it was
written to prevent.

The fix is to end the old instance before the new one opens anything —
`drop(qemu)` first, or a `qemu = { drop(qemu); boot() }` that says so. What it
must not become is a fresh lane per reboot: the lane is what bounds the disk a
suite uses.

**Distinct from `specs/issues/build/qemu-exits-clean-before-ready.md`**, which is
a QEMU that exits *0* with no reason given at all. This one exits 1 and says
exactly why on stderr; what is missing is not diagnosis but ordering.
