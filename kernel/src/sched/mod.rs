//! The kernel's half of the scheduler core (spec §4's `kernel/src/sched/`).
//!
//! * [`payload`] — what the kernel attaches to a task, and the two pieces the
//!   core crate refuses to implement itself.
//! * [`driver`] — percpu `CpuSched` slot, pass entry, asm switch, idle loop,
//!   trampoline. Decides nothing.
//! * [`waitqs`] — where the kernel's wait queues live.
//!
//! The kernel-facing API — everything the rest of the kernel calls — is
//! `crate::scheduler`.

pub mod driver;
pub mod payload;
pub mod waitqs;

/// Ceiling on CPUs the percpu arrays are sized for.
pub const MAX_CPUS: usize = 8;
