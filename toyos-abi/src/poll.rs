//! Unified polling of file descriptors and the process message queue.

pub use crate::syscall::{PollResult, POLL_READABLE, POLL_WRITABLE, POLL_FD_MASK};
use crate::syscall;

/// Poll file descriptors and the message queue for readiness.
/// Blocks until at least one source has data.
///
/// Each entry is a fd number (as u64), optionally OR'd with
/// [`POLL_READABLE`] or [`POLL_WRITABLE`] interest flags.
pub fn poll(fds: &[u64]) -> PollResult {
    syscall::poll(fds)
}

/// Poll with a timeout in nanoseconds.
/// `None` blocks forever, `Some(nanos)` times out.
pub fn poll_timeout(fds: &[u64], timeout_nanos: Option<u64>) -> PollResult {
    syscall::poll_timeout(fds, timeout_nanos)
}
