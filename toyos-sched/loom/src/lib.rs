//! Loom harness for the `toyos-sched` primitives (spec §10.6).
//!
//! This crate is `toyos-sched` compiled a second time: the module list below
//! includes the very same source files, with `feature = "loom"` on, so
//! `crate::sync` resolves to loom's instrumented atomics and `Arc`. The
//! models live in `tests/`; loom explores the interleavings of the *real*
//! primitives, not of a re-implementation — a re-implementation is exactly
//! the divergence risk this crate is meant to remove.
//!
//! Division of labour, stated honestly (spec §10.6): loom owns the
//! primitives the simulator's step granularity assumes correct — mailbox
//! push/drain, doorbell edges, the ticket CAS protocol, kill-bit vs wake
//! ordering, retire-node re-post, the sleep handshake. The simulator (Stage
//! 4) owns the protocol above them. Loom does not scale to the whole
//! scheduler; the simulator does not model weak memory.
//!
//! Keep this module list identical to `../src/lib.rs`. `fair` is pure math
//! with no atomics worth modelling beyond the frontier's `fetch_max`, but it
//! is compiled all the same: `cpu.rs` calls it, and a divergent module list
//! would mean loom checking a different crate than the one that ships.

#![deny(unsafe_code)]

extern crate alloc;

#[path = "../../src/cpu.rs"]
pub mod cpu;
#[path = "../../src/fair.rs"]
pub mod fair;
#[path = "../../src/hw.rs"]
pub mod hw;
#[path = "../../src/invariants.rs"]
pub mod invariants;
#[path = "../../src/mailbox.rs"]
pub mod mailbox;
#[path = "../../src/msg.rs"]
pub mod msg;
#[path = "../../src/queue.rs"]
pub mod queue;
#[path = "../../src/retire.rs"]
pub mod retire;
#[path = "../../src/sync.rs"]
pub mod sync;
#[path = "../../src/task.rs"]
pub mod task;
#[path = "../../src/timer.rs"]
pub mod timer;
#[path = "../../src/waitq.rs"]
pub mod waitq;

pub mod model;
