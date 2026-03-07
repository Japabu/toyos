//! Download backend for package fetching.
//!
//! When the `curl-backend` feature is enabled, uses curl's multi interface
//! for parallel HTTP downloads. Otherwise, provides a stub that handles
//! locally-available packages and bails on network downloads.

#[cfg(feature = "curl-backend")]
mod curl_downloads;
#[cfg(feature = "curl-backend")]
pub use curl_downloads::*;

#[cfg(not(feature = "curl-backend"))]
mod stub_downloads;
#[cfg(not(feature = "curl-backend"))]
pub use stub_downloads::*;
