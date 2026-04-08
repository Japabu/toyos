//! Service discovery via listen/connect/accept.

use crate::OwnedFd;
use crate::syscall::{self, SyscallError};

pub fn listen(name: &str) -> Result<OwnedFd, SyscallError> {
    syscall::listen(name).map(OwnedFd::new)
}

pub struct AcceptResult {
    pub fd: OwnedFd,
    pub client_pid: u32,
}

pub fn accept(listener: &OwnedFd) -> Result<AcceptResult, SyscallError> {
    syscall::accept(listener.fd()).map(|r| AcceptResult {
        fd: OwnedFd::new(r.fd),
        client_pid: r.client_pid,
    })
}

pub fn connect(name: &str) -> Result<OwnedFd, SyscallError> {
    syscall::connect(name).map(OwnedFd::new)
}
