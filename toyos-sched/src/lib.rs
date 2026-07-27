//! ToyOS scheduler core — a sans-IO state machine driven by both the kernel
//! and the host simulator through one [`hw::Hw`] boundary. The authoritative
//! design is `specs/scheduler-core-spec.md`.
//!
//! Migration state (spec §11): Stage 4. [`fair`] carries the production
//! fairness policy, called by `kernel/src/scheduler.rs` (Stage 1). The
//! concurrency primitives are loom-verified: [`mailbox`] (the intrusive MPSC,
//! the doorbell and the sleep handshake), [`task`] (the rendezvous state word
//! and its CAS protocol), [`waitq`] (the two-phase wait ticket and the single
//! wake path) and [`retire`] (kill bit + message chase). Stage 4 adds the
//! machine those primitives serve: the linear task value and its five
//! lifecycle types ([`task`]), the run queue ([`queue`]), the deadline heap
//! and timer plan ([`timer`]), the message set ([`msg`]) and the per-CPU
//! [`cpu::CpuSched`] with its [`cpu::SchedPass`] type-state.
//!
//! The kernel does not call any of it yet: the per-source conversions land at
//! Stage 5, the `Hw` implementation at Stage 6 and the cutover at Stage 7.
//! What drives it today is the host simulator (`toyos-sched/sim/`), which
//! explores the protocol against invariants I1–I12 — including the deliberate
//! port of the old steal-during-exit algorithm, which the simulator must
//! refuse before its green runs mean anything (spec §10.3).
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
pub mod invariants;
pub mod mailbox;
pub mod msg;
pub mod queue;
pub mod retire;
pub mod sync;
pub mod task;
pub mod timer;
pub mod waitq;
