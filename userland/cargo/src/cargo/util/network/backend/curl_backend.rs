//! Network backend implementation using curl/libcurl (C library).

use crate::util::errors::CargoResult;

/// Check if an error is a spurious curl error (transient/retryable).
/// Returns `Some(true)` if spurious, `None` if not a curl error.
pub fn is_spurious_http_error(err: &anyhow::Error) -> Option<bool> {
    let curl_err = err.downcast_ref::<curl::Error>()?;
    Some(
        curl_err.is_couldnt_connect()
            || curl_err.is_couldnt_resolve_proxy()
            || curl_err.is_couldnt_resolve_host()
            || curl_err.is_operation_timedout()
            || curl_err.is_recv_error()
            || curl_err.is_send_error()
            || curl_err.is_http2_error()
            || curl_err.is_http2_stream_error()
            || curl_err.is_ssl_connect_error()
            || curl_err.is_partial_file(),
    )
}

/// Get version information about the HTTP backend for display.
pub fn http_version_info() -> String {
    let curl_v = curl::Version::get();
    let vendored = if curl_v.vendored() {
        "vendored"
    } else {
        "system"
    };
    format!(
        "libcurl: {} (sys:{} {} ssl:{})",
        curl_v.version(),
        curl_sys::rust_crate_version(),
        vendored,
        curl_v.ssl_version().unwrap_or("none")
    )
}

/// Disable HTTP/2 multiplexing for broken curl versions when proxy is in use.
pub fn maybe_disable_multiplexing_for_bad_version(
    http: &mut crate::util::context::CargoHttpConfig,
    gctx: &crate::util::GlobalContext,
) {
    use crate::util::network;

    if network::proxy::http_proxy_exists(http, gctx) && http.multiplexing.is_none() {
        let curl_v = curl::Version::get();
        let curl_version = curl_v.version();
        let bad_curl_versions = ["7.87.0", "7.88.0", "7.88.1"];
        if bad_curl_versions
            .iter()
            .any(|v| curl_version.starts_with(v))
        {
            tracing::info!("disabling multiplexing with proxy, curl version is {curl_version}");
            http.multiplexing = Some(false);
        }
    }
}

/// Register the libcurl HTTP transport with libgit2 if needed.
///
/// This is called during startup to configure libgit2 to use libcurl
/// for HTTP operations when custom network configuration is detected.
pub fn init_git_http_transport(gctx: &crate::util::GlobalContext) {
    use crate::util::network::http_curl::{http_handle, needs_custom_http_transport};

    match needs_custom_http_transport(gctx) {
        Ok(true) => {}
        _ => return,
    }

    let handle = match http_handle(gctx) {
        Ok(handle) => handle,
        Err(..) => return,
    };

    unsafe {
        git2_curl::register(handle);
    }
}

/// Create a new HTTP handle configured for Cargo.
pub fn create_http_handle(gctx: &crate::util::GlobalContext) -> CargoResult<curl::easy::Easy> {
    crate::util::network::http_curl::http_handle(gctx)
}

/// Configure an existing HTTP handle and return its timeout settings.
pub fn configure_http_handle(
    gctx: &crate::util::GlobalContext,
    handle: &mut curl::easy::Easy,
) -> CargoResult<crate::util::network::http::HttpTimeout> {
    crate::util::network::http_curl::configure_http_handle(gctx, handle)
}
