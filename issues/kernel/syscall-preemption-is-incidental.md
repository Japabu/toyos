---
status: open
kind: defect
opened: 2026-07-31
---

# A syscall runs with interrupts masked, and only incidentally with preemption disabled

`syscall_entry` raises the preempt count before `call {handler}` and lowers it
after, so `preempt::enable`'s `count() == 0` slow path can never fire inside a
syscall — no matter how many locks the handler takes and drops. Spec §7.4 counts
that slow path as an RT-wake safe point and bounds wake latency by "the longest
preempt-disabled section"; in syscall context that section is *the entire
syscall*, and the real bound is the next `kernel_exit_to_user_check`.

The preempt count is the weaker of two independent blockers, and it is not the
one that decides the bound. `MSR_FMASK = 0x40200` (`arch/syscall.rs:57`) clears
IF on every SYSCALL entry, and nothing on the straight-line syscall path sets it
again — the only `sti`s in the kernel are `cpu::enable_interrupts`
(`arch/cpu.rs:113`, reached from `trap_dispatch`'s #PF arm and from init),
`kernel_exit_to_user_check`'s own yield window (`arch/idt/mod.rs:232`), and the
idle loop (`sched/driver.rs:494`). With IF=0 the CPU cannot even be *told* to
reschedule: `KernelHw::need_resched` (`hw.rs:96-108`) documents that a remote
CPU's `need_resched` byte is unreachable from here, so a remote request is only
deliverable as a kick IPI — an interrupt the masked target will not take until
it leaves the syscall.

That makes the entry level's fix ineffective on its own: dropping it around
blocking-capable handler regions cannot move remote RT wake latency at all
while IF stays 0. A fix has to unmask interrupts over those regions — which
means auditing what each one is safe to be interrupted in — or the bound is
accepted and §7.4 corrected.

This was masked until the preempt count was made conserved across a context
switch (§6.4's baselines needed it): before that the count drifted, so a lock
drop inside a syscall reached zero at random and preempted at random. The
behaviour is now deterministic, and deterministically weaker than §7.4 assumes.
Whether that matters is measurable — gate A's wake-lateness distribution is the
instrument — and it did not move at N=8.
