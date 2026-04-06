//! System information and control.

pub use crate::syscall::RealTime;
use crate::syscall::{self, SyscallError};

/// Sysinfo header size in bytes.
pub const SYSINFO_HEADER_SIZE: usize = 48;
/// Per-process/thread entry size in bytes.
pub const SYSINFO_ENTRY_SIZE: usize = 64;

/// Read the wall-clock time from the hardware RTC.
pub fn clock_realtime() -> RealTime {
    syscall::clock_realtime()
}

/// Query system information (memory, CPU, processes) into `buf`.
/// Returns the number of bytes written.
pub fn sysinfo(buf: &mut [u8]) -> usize {
    syscall::sysinfo(buf)
}

/// Return the number of available CPUs.
pub fn cpu_count() -> u32 {
    syscall::cpu_count()
}

/// Shut down the machine. Does not return.
pub fn shutdown() -> ! {
    syscall::shutdown()
}

/// Set the active keyboard layout by name.
pub fn set_keyboard_layout(name: &str) -> Result<(), SyscallError> {
    syscall::set_keyboard_layout(name)
}
