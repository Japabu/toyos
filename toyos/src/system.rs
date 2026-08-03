//! System information and control.

pub use toyos_abi::syscall::RealTime;
use toyos_abi::syscall;

pub const SYSINFO_HEADER_SIZE: usize = 48;
pub const SYSINFO_ENTRY_SIZE: usize = 64;

pub fn clock_realtime() -> RealTime {
    syscall::clock_realtime()
}

pub fn sysinfo(buf: &mut [u8]) -> usize {
    syscall::sysinfo(buf)
}

pub fn cpu_count() -> u32 {
    syscall::cpu_count()
}

pub fn shutdown() -> ! {
    syscall::shutdown()
}

