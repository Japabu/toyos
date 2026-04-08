//! Extended pipe operations.

use toyos_abi::Fd;
use toyos_abi::syscall::{self, SyscallError};
use crate::{Pipe, Handle};

/// Get the internal pipe ID for a file descriptor.
pub fn id(fd: Fd) -> Result<u64, SyscallError> {
    syscall::pipe_id(fd)
}

/// Open an existing pipe by its internal ID.
/// `read`: `true` for the read end, `false` for the write end.
pub fn open_by_id(pipe_id: u64, read: bool) -> Result<Pipe, SyscallError> {
    let mode = if read { 0 } else { 1 };
    syscall::pipe_open(pipe_id, mode).map(|fd| Pipe(Handle(fd)))
}
