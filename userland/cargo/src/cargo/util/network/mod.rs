//! Utilities for networking.

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::task::Poll;

pub mod backend;
pub mod http;
#[cfg(feature = "curl-backend")]
pub mod http_curl;
pub mod http_async;
pub mod proxy;
pub mod retry;
pub mod sleep;

/// LOCALHOST constants for both IPv4 and IPv6.
pub const LOCALHOST: [SocketAddr; 2] = [
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)),
];

pub trait PollExt<T> {
    fn expect(self, msg: &str) -> T;
}

impl<T> PollExt<T> for Poll<T> {
    #[track_caller]
    fn expect(self, msg: &str) -> T {
        match self {
            Poll::Ready(val) => val,
            Poll::Pending => panic!("{}", msg),
        }
    }
}

/// When dynamically linked against libcurl, we want to ignore some failures
/// when using old versions that don't support certain features.
#[cfg(feature = "curl-backend")]
#[macro_export]
macro_rules! try_old_curl {
    ($e:expr, $msg:expr) => {
        let result = $e;
        if cfg!(target_os = "macos") {
            if let Err(e) = result {
                ::tracing::warn!(target: "network", "ignoring libcurl {} error: {}", $msg, e);
            }
        } else {
            if let Err(e) = &result {
                ::tracing::error!(target: "network", "failed to enable {}, is curl not built right? error: {}", $msg, e);
            }
            result?;
        }
    };
}

/// Enable HTTP/2 and pipewait to be used as it'll allow true multiplexing
/// which makes downloads much faster.
#[cfg(feature = "curl-backend")]
#[macro_export]
macro_rules! try_old_curl_http2_pipewait {
    ($multiplexing:expr, $handle:expr) => {
        if $multiplexing {
            $crate::try_old_curl!($handle.http_version(curl::easy::HttpVersion::V2), "HTTP/2");
        } else {
            $handle.http_version(curl::easy::HttpVersion::V11)?;
        }
        $crate::try_old_curl!($handle.pipewait(true), "pipewait");
    };
}
