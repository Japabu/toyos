---
status: open
kind: defect
opened: 2026-08-11
---

# A guest QEMU exited 0 before `===READY===`, and the harness has no account of it

`wait_for_ready`'s `RecvTimeoutError::Disconnected` arm in `tests/common/qemu.rs`
reports a QEMU that went away before the ready marker as

```
FAIL <name>: [qemu] QEMU died before ===READY=== (status: Ok(ExitStatus(unix_wait_status(0))))
```

and nothing else. It formats the exit status and drops both `seen` — every line
the guest had already written, held in that same function — and the UART log
the guest's early boot goes to. Both exist at the moment it fires, and neither
reaches the message. `TestResult::error` carries a kernel's death report since
#125, but this panic is not that field. The capture holds nothing to bisect and
the test's name is the whole of the evidence, which is how a real one-in-N boot
failure and a host hiccup read alike.

**That is the whole defect, and it is the harness's.** The sightings that opened
this file are explained, and the explanation is why the arm's silence matters.

## What the exit status says, and what the sightings were

The harness passes `-no-reboot`, and its own `screendump` comment says what that
buys: a guest that triple-faults exits QEMU. So a status-0 exit before the
marker is a guest that **reset itself** during boot having said nothing — not a
QEMU that could not start or open a device, which exits non-zero with a message,
and not a QEMU the host reaped, which exits on a signal.

A guest resetting silently during a loaded boot was the class PR #202 closed on
2026-08-22: no Ring 0 entry cleared the direction flag, so an interrupt taken
inside `compiler_builtins::mem::memmove`'s `std` window handed the kernel a set
`DF` and every later `memcpy`/`memset` wrote its bytes *below* its destination.
37 deaths in 13,960 twelve-wide `bootable.img` boots without the `cld`, 25 of
them silent — QEMU gone, no marker — and 0 of any kind in 7,418 with it. PR #198
parked one such death under `-action shutdown=pause` and read `RFL=[D--Z-P-]`
with a non-canonical RIP off it: `DF` set, at a `ret` off a frame that was no
stack. Under this harness's `-no-reboot` the same reset is a status-0 exit.

The sightings, all on the dev host with other builds or suites on the machine,
every one `ALONE: GREEN` — `kernel_heartbeat` (2026-08-11, `wt/toyos-std`, six
for six green afterwards), `screen_fatal_halt` and `double_fault_stack` (both
2026-08-15 in one 106.4 s phase on `wt/toyos-ciwall`, `1.05x` width, a second
worktree's suite holding slots), `log_backing_read_error` (`wt/toyos-logd56`,
same day), `screen_console_shell` through the screendump wait (`wt/toyos-capwin`,
same day), and `hda_two_live_refused` (2026-08-19, `wt/toyos-tlb`, red only on
the cold run that compiled 110 C tests and three kernels beside the guests). Six
names with nothing in common but a boot, on five trees, following the host's
build load and not the tree — which is what a class whose rate rises with
interrupts per unit of guest work looks like from outside. The two redlist rows
(`screen_fatal_halt`, `double_fault_stack`) are retired on this reading, as is
`diskless_boot`'s identical 2026-08-19 row.

## What is owed

The arm keeps what it has. A QEMU that exits 0 before the ready marker should
report the lines the guest did write and the UART log — a silent reset then
arrives with the last thing the kernel said before it, which for this class was
the spawn burst and for the next class will be whatever that class's last line
is. `issues/build/a-failure-message-drops-the-lines-before-the-test-started.md`
is the general form of the same hole on the other side of the marker.
