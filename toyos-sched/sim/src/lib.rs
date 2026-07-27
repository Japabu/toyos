//! Host-side deterministic simulator for `toyos-sched` (spec §10).
//!
//! Migration state (spec §11): Stage 0 scaffolding. [`choice`] provides the
//! seed/fuzz-byte decision plumbing; the virtual machine, explorer, shrinker
//! and scenario library land at Stage 4.

pub mod choice;
