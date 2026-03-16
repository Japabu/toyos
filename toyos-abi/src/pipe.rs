//! Extended pipe operations.

use crate::Fd;
use crate::syscall::{self, SyscallError};

/// Get the internal pipe ID for a file descriptor.
/// Used to share pipe access across processes via [`open_by_id`].
pub fn id(fd: Fd) -> Result<u64, SyscallError> {
    syscall::pipe_id(fd)
}

/// Open an existing pipe by its internal ID.
/// `read`: `true` for the read end, `false` for the write end.
pub fn open_by_id(pipe_id: u64, read: bool) -> Result<Fd, SyscallError> {
    let mode = if read { 0 } else { 1 };
    syscall::pipe_open(pipe_id, mode)
}
