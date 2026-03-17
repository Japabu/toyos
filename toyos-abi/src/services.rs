//! Service discovery via listen/connect/accept.

use crate::Fd;
use crate::syscall::{self, SyscallError};

pub fn listen(name: &str) -> Result<Fd, SyscallError> {
    syscall::listen(name)
}

pub fn accept(listener: Fd) -> Result<syscall::AcceptResult, SyscallError> {
    syscall::accept(listener)
}

pub fn connect(name: &str) -> Result<Fd, SyscallError> {
    syscall::connect(name)
}
