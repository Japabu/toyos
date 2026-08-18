---
status: open
kind: defect
opened: 2026-08-03
---

# A boot that wedges before the idle loop says nothing at all

Not "says less": **nothing**, including everything it logged before it wedged. The
log ring is drained by exactly two callers — the timer tick
(`arch/idt/timer.rs:138`) and the scheduler/idle loop (`sched/driver.rs:649`) —
and during the boot phases neither runs: `apic::init_timer` calibrates the LAPIC
timer but does not start it (the scheduler arms one-shot timers on demand), and
`enter_idle_loop` is the last line of `kernel_main`. So a boot's output reaches
the wire only when something takes a fatal path, because `apic::halt_all_cpus`
and the panic handler call `serial::panic_flush` and `acpi`'s power path calls
`serial::flush_final`.

A wedge with no panic therefore looks identical to a kernel that never started.
Found at IOMMU stage I2, from a deliberately mis-programmed unit that stopped
NVMe mid-`init`: the guest had logged sixty lines and the harness saw the
bootloader's output and then a ten-second timeout. It costs an hour the first
time and it will cost it again — a wedged boot is exactly the case where the log
matters most.

**Bisecting one meanwhile:** put `$crate::drivers::serial::flush_final();` at the
end of the `log!` macro (`log.rs`), rebuild, and every line arrives as it is
written. `flush_final` is `try_lock` with a bounded spin, so it cannot deadlock
against a holder. A per-phase version — the same call at the end of
`boot_phase!` — narrows it to a phase for a fraction of the output.

The fix is not that patch. A boot-time drain is a decision about where the kernel
may spend microseconds during boot and who owns the backend lock before the
scheduler exists, and the on-screen console already answers the *phase* question
for a machine with a panel (`boot_checkpoint`). Recorded rather than fixed
because the choice belongs with whoever owns the log ring.
