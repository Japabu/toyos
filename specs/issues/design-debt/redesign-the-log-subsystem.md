---
status: owner
kind: question
opened: 2026-08-08
---

# Redesign the log subsystem, and re-shape `kernel/src`?

Recorded verbatim because it is the owner asking, not the owner deciding, and
because an agent must not answer it on his behalf: *"should we redesign and
rewrite the log subsystem and rethink if the current file/folder structure of
the kernel makes sense?"* (`kernel/src/log.rs`). What follows is the evidence
that makes it decidable, and nothing else.

**The log subsystem, as it exists.** Six places, no core:

| File | Lines | What it is |
|---|---|---|
| `log.rs` | 64 | the `log!` macro and a GS-validity flag |
| `drivers/log_ring.rs` | 549 | the ring, 64 KiB, and its drain policy |
| `log_file.rs` | 564 | the `/log` sink and its flush |
| `drivers/serial.rs` | 467 | the 16550 sink |
| `drivers/panic_console/mod.rs` | 1,188 | the screen sink, panic-only |
| `drivers/virtio_console.rs` | 221 | the second serial-shaped sink |

Its known sins are already entries here and are the argument for the question:
the flush is unbounded, uninterruptible and in front of the scheduler pass, and
userland `println!` shares the ring — both
`specs/issues/boot-media/log-flush-is-unbounded.md`; a boot that wedges before
the idle loop produces no output at all because the ring's only drains are the
timer tick and the idle loop
(`specs/issues/diagnostics/pre-idle-wedge-says-nothing.md`); the ring's occupancy
is one of the pre-`hlt` recheck's conditions, so a CPU with bytes pending
declines to sleep; and `drain_serial`'s `BackendGuard::lock` spins with
interrupts disabled with no bound and no deadlock panic (both, CLAUDE.md's
idle-loop warning). Each has been
patched where it hurt. None has been fixed by a design.

The shape the review reached, **if** the owner wants it: a log core (ring +
context stamping, once) with serial, file and screen as independent sinks
carrying explicit backpressure — a slow sink drops-and-counts, never blocks,
does no unbounded work in a scheduler-adjacent path, and fails alone.

**The layout half.** `kernel/src` is **39 flat `.rs` files** beside seven
directories (`arch/`, `drivers/`, `elf/`, `iommu/`, `loader/`, `mm/`,
`sched/`). Three of those seven were flat files a month ago — `elf.rs` and
`loader.rs` became directories in `42b29c9`, which is the precedent. The flat
set mixes a filesystem adapter, an IPC primitive, two input devices, a page
cache, io_uring, the process table and two cfg-gated test actuators at one
level. The review's target was subsystem directories (fs/, ipc/, input/,
proc/, log/, time/), and the `syscall.rs` split already forces at least one.

Cost, so the question is priced: a directory move is `git mv` plus `mod` lines,
it touches no logic, and it collides with every worktree in flight — which is
why it is a scheduling decision and not a technical one.

Two smaller layout items ride the same answer. `usb_gate.rs` (225 lines) and
`input_merge_test.rs` (202) are correctly `#[cfg]`-gated at declaration and
call (`main.rs:27-30`, `:446`, `:566`) and are never in an ordinary build, but
they sit interleaved with production sources; the review's target is one
`kernel/src/gates/` directory so that what test machinery exists is auditable
in one listing. And `input_merge_test` is the tell that pure logic is trapped
in the kernel: the merge state machine (one held-set, one button-merge, both
bounded) is host-testable with synthetic multi-source streams, after which the
gate shrinks or goes.
