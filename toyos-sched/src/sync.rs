//! Atomics and `Arc`, resolved to whichever world compiles these sources.
//!
//! The kernel and the simulator get `core`/`alloc`; the `toyos-sched-loom`
//! harness (`toyos-sched/loom/`) compiles the *same* source files into its
//! own crate with `feature = "loom"` on, so every atomic in the primitives
//! becomes a loom-instrumented one and loom explores the interleavings.
//!
//! Why a feature and a second package instead of the usual `--cfg loom`:
//! a `[target.'cfg(loom)'.dependencies]` entry lands in every lockfile that
//! resolves this crate — including `kernel/Cargo.lock`, which would gain
//! loom and its 30 transitive host crates. The kernel's dependency graph
//! stays exactly as it was; `loom` is declared here purely so `cfg` checking
//! knows the name (this package never enables it).

#[cfg(not(feature = "loom"))]
pub use alloc::sync::Arc;
#[cfg(not(feature = "loom"))]
pub use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

#[cfg(feature = "loom")]
pub use loom::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
#[cfg(feature = "loom")]
pub use loom::sync::Arc;
