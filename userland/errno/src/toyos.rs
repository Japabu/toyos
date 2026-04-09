use core::sync::atomic::{AtomicI32, Ordering};

use crate::Errno;

static ERRNO: AtomicI32 = AtomicI32::new(0);

const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EBADF: i32 = 9;
const ECHILD: i32 = 10;
const EAGAIN: i32 = 11;
const ENOMEM: i32 = 12;
const EACCES: i32 = 13;
const EEXIST: i32 = 17;
const EINVAL: i32 = 22;
const ENOSPC: i32 = 28;
const EPIPE: i32 = 32;
const ENOSYS: i32 = 38;

pub fn with_description<F, T>(err: Errno, callback: F) -> T
where
    F: FnOnce(Result<&str, Errno>) -> T,
{
    let desc = match err.0 {
        EPERM => "Operation not permitted",
        ENOENT => "No such file or directory",
        EIO => "I/O error",
        EBADF => "Bad file descriptor",
        ECHILD => "No child processes",
        EAGAIN => "Resource temporarily unavailable",
        ENOMEM => "Out of memory",
        EACCES => "Permission denied",
        EEXIST => "File exists",
        EINVAL => "Invalid argument",
        ENOSPC => "No space left on device",
        EPIPE => "Broken pipe",
        ENOSYS => "Function not implemented",
        _ => "Unknown error",
    };
    callback(Ok(desc))
}

pub const STRERROR_NAME: &str = "errno::description";

pub fn errno() -> Errno {
    Errno(ERRNO.load(Ordering::Relaxed))
}

pub fn set_errno(Errno(errno): Errno) {
    ERRNO.store(errno, Ordering::Relaxed);
}
