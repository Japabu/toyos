//! Backend abstraction for git operations.
//!
//! Selects between git2 (C library) and gix (pure Rust) at compile time
//! via the `git2-backend` feature flag. Both backends export the same
//! public API so callers need zero cfg annotations.

#[cfg(feature = "git2-backend")]
mod git2_backend;
#[cfg(feature = "git2-backend")]
pub use git2_backend::*;

#[cfg(not(feature = "git2-backend"))]
mod gix_backend;
#[cfg(not(feature = "git2-backend"))]
pub use gix_backend::*;
