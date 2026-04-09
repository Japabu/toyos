use crate::error::Error as CtrlcError;
use std::fmt;

/// Platform-specific error type (ToyOS has no signal support).
#[derive(Debug, PartialEq)]
pub struct Error;

impl Error {
    pub const EEXIST: Error = Error;
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "signals not supported on ToyOS")
    }
}

impl std::error::Error for Error {}

/// Platform-specific signal type (placeholder — ToyOS has no signals).
#[derive(Debug)]
pub struct Signal;

/// No-op: ToyOS has no signal infrastructure.
#[inline]
pub unsafe fn init_os_handler(_overwrite: bool) -> Result<(), Error> {
    Ok(())
}

/// Parks forever: the signal handler thread has nothing to wait for on ToyOS.
#[inline]
pub unsafe fn block_ctrl_c() -> Result<(), CtrlcError> {
    std::thread::park();
    Ok(())
}
