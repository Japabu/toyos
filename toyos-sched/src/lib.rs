//! ToyOS scheduler core — a sans-IO state machine driven by both the kernel
//! and the host simulator through one [`hw::Hw`] boundary. The authoritative
//! design is `specs/scheduler-core-spec.md`.
//!
//! Migration state (spec §11): Stage 3. [`fair`] carries the production
//! fairness policy, called by `kernel/src/scheduler.rs` (Stage 1). The
//! concurrency primitives are now here and loom-verified: [`mailbox`] (the
//! intrusive MPSC, the doorbell and the sleep handshake), [`task`] (the
//! rendezvous state word and its CAS protocol), [`waitq`] (the two-phase wait
//! ticket and the single wake path) and [`retire`] (kill bit + message
//! chase). The kernel does not call them yet — the per-CPU machine that ties
//! them together lands at Stage 4, the per-source conversions at Stage 5 and
//! the cutover at Stage 7.
//!
//! Loom coverage lives in the sibling `toyos-sched-loom` package
//! (`toyos-sched/loom/`), which compiles *these* source files against loom's
//! instrumented atomics; see [`sync`] for why it is a separate package
//! rather than a `cfg(loom)` dependency.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

pub mod cpu;
pub mod fair;
pub mod hw;
pub mod mailbox;
pub mod retire;
pub mod sync;
pub mod task;
pub mod waitq;
