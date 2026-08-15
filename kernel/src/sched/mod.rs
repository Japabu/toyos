//! The kernel's half of the scheduler core (spec §4's `kernel/src/sched/`).
//!
//! * [`payload`] — what the kernel attaches to a task, and the two pieces the
//!   core crate refuses to implement itself.
//! * [`driver`] — percpu `CpuSched` slot, pass entry, asm switch, idle loop,
//!   trampoline. Decides nothing.
//! * [`waitqs`] — where the kernel's wait queues live.
//! * [`dump`] — Ctrl+Alt+D, the machine-wide blocked-task report.
//! * [`kthread`] — a task with no address space, and what its panic means.
//! * [`reap_gate`] — the flag that keeps the idle loop off the process table
//!   when there is nothing to reap.
//!
//! The kernel-facing API — everything the rest of the kernel calls — is
//! `crate::scheduler`.

pub mod driver;
pub mod dump;
pub mod kthread;
pub mod payload;
pub mod reap_gate;
pub mod waitqs;

/// Ceiling on CPUs the percpu arrays are sized for.
pub const MAX_CPUS: usize = 8;
