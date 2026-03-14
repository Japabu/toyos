//! Process name registry for service discovery.

use crate::Pid;
use crate::syscall::{self, SyscallError};

/// Register the current process under a name so other processes can find it.
pub fn register(name: &str) -> Result<(), SyscallError> {
    syscall::register_name(name)
}

/// Find the PID of a process registered under the given name.
pub fn find(name: &str) -> Option<Pid> {
    syscall::find_pid(name)
}
