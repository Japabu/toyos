//! ToyOS scheduler core — a sans-IO state machine driven by both the kernel
//! and the host simulator through one [`hw::Hw`] boundary. The authoritative
//! design is `specs/scheduler-core-spec.md`.
//!
//! Two host-side harnesses compile against these same sources: the simulator
//! (`toyos-sched/sim/`), which explores the protocol against invariants
//! I1–I13, and the `toyos-sched-loom` package (`toyos-sched/loom/`), which
//! swaps in loom's instrumented atomics — see [`sync`] for why loom is a
//! separate package rather than a `cfg(loom)` dependency.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

pub mod cpu;
pub mod fair;
pub mod hw;
pub mod invariants;
pub mod mailbox;
pub mod msg;
pub mod queue;
pub mod retire;
pub mod sync;
pub mod task;
pub mod timer;
pub mod waitq;
