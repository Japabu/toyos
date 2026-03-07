//! Network backend implementation using reqwest (pure Rust).
//!
//! When the `curl-backend` feature is disabled, this module provides
//! alternative implementations for HTTP utility functions.

/// Check if an error is a spurious HTTP error (transient/retryable).
/// Returns `None` — reqwest errors are not currently checked for spuriousness.
pub fn is_spurious_http_error(_err: &anyhow::Error) -> Option<bool> {
    // TODO: check for reqwest transient errors
    None
}

/// Get version information about the HTTP backend for display.
pub fn http_version_info() -> String {
    "reqwest (pure Rust)".to_string()
}

/// Disable HTTP/2 multiplexing for broken versions. No-op for reqwest.
pub fn maybe_disable_multiplexing_for_bad_version(
    _http: &mut crate::util::context::CargoHttpConfig,
    _gctx: &crate::util::GlobalContext,
) {
    // reqwest handles HTTP/2 multiplexing internally
}

/// Register HTTP transport with git. No-op for reqwest (gix handles its own transport).
pub fn init_git_http_transport(_gctx: &crate::util::GlobalContext) {}
