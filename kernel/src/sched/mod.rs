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

// Every `unsafe` block under `sched::` has either stopped existing or carries a
// `SAFETY:` saying why it could not — the reduction-before-documentation sweep
// `issues/build/clippy-has-never-run-here.md` records. `host-tests.yml`'s two
// kernel clippy invocations both run with `-D warnings`, so `warn` here is what
// gates: a new undocumented block anywhere in this module tree fails CI.
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod driver;
pub mod dump;
pub mod kthread;
pub mod payload;
pub mod reap_gate;
pub mod waitqs;

/// Ceiling on CPUs the percpu arrays are sized for.
pub const MAX_CPUS: usize = 8;

/// `shootdown::MAX_CPUS` is a second copy, kept for the reason its own doc
/// comment gives — `kernel-loom` compiles that file with no `crate::`
/// reference at all, in every feature state it builds that file under. This
/// file is never one of the files `kernel-loom` compiles, so it is where the
/// two constants can be pinned together without a `cfg`.
const _: () = assert!(MAX_CPUS == crate::shootdown::MAX_CPUS);
