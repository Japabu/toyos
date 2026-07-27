//! Host-side deterministic simulator for `toyos-sched` (spec §10).
//!
//! Migration state (spec §11): Stage 4. The virtual machine, the explorer, the
//! shrinker, the corpus and the scenario library are here, and the exit
//! criterion they exist to serve is stated in one line: **`old_steal_port`
//! must fail** while every other scenario passes. A fuzzer that has never
//! caught the bug class it was written for is decoration.
//!
//! What is real and shared with the kernel, and what is mocked, is the whole
//! contract — see [`hw_impl`]. Nothing in here re-implements a scheduling
//! decision.
//!
//! The full exit criterion runs from the CLI rather than from `cargo test`,
//! which keeps a budget it can afford:
//!
//! ```text
//! cargo run --release -p toyos-sched-sim -- gate 10000        # 10^4 seeds/scenario
//! cargo run --release -p toyos-sched-sim -- fuzz-sweep 10000000  # 10^7 steps/scenario
//! ```

// Three exemptions, each a single item and each unavoidable: implementing the
// `unsafe fn Hw::switch` (twice — the declaration and the call), and asserting
// the `PreemptGuard` contract. Nothing here dereferences a raw pointer.
#![deny(unsafe_code)]

pub mod choice;
pub mod explore;
pub mod hw_impl;
pub mod invariants;
pub mod msg;
pub mod payload;
pub mod scenarios;
pub mod shrink;
pub mod sweep;
pub mod vm;
pub mod workload;
