//! Backend abstraction for HTTP operations.
//!
//! Selects between curl (C library) and reqwest (pure Rust) at compile time
//! via the `curl-backend` feature flag. Both backends export the same
//! public API so callers need zero cfg annotations.

#[cfg(feature = "curl-backend")]
mod curl_backend;
#[cfg(feature = "curl-backend")]
pub use curl_backend::*;

#[cfg(not(feature = "curl-backend"))]
mod reqwest_backend;
#[cfg(not(feature = "curl-backend"))]
pub use reqwest_backend::*;
