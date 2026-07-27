//! Service discovery via listen/connect/accept.

use toyos_abi::syscall::{self, SyscallError};
use crate::{Listener, Handle};
use crate::ipc::Connection;

pub struct AcceptResult {
    pub conn: Connection,
    pub client_pid: u32,
}

pub fn listen(name: &str) -> Result<Listener, SyscallError> {
    syscall::listen(name).map(|fd| Listener(Handle(fd)))
}

pub fn accept(listener: &Listener) -> Result<AcceptResult, SyscallError> {
    syscall::accept(listener.fd()).map(|r| AcceptResult {
        conn: Connection(Handle(r.fd)),
        client_pid: r.client_pid,
    })
}

pub fn connect(name: &str) -> Result<Connection, SyscallError> {
    syscall::connect(name).map(|fd| Connection(Handle(fd)))
}
