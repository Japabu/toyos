//! System information and control.

pub use toyos_abi::syscall::RealTime;
use toyos_abi::syscall;

pub const SYSINFO_HEADER_SIZE: usize = 48;
pub const SYSINFO_ENTRY_SIZE: usize = 64;

/// The time of day in the machine's own zone, or `None` on a machine whose
/// clock never answered.
pub fn clock_realtime() -> Option<RealTime> {
    syscall::clock_realtime()
}

/// Seconds since the Unix epoch, which is UTC, or `None` for the same machine.
///
/// Cheap: the kernel serves it from an anchor it took at boot plus the
/// monotonic clock, so calling it in a loop costs a syscall each and no device
/// access at all.
pub fn clock_epoch() -> Option<u64> {
    syscall::clock_epoch()
}

pub fn sysinfo(buf: &mut [u8]) -> usize {
    syscall::sysinfo(buf)
}

// Powering the machine off used to be a free function here, over a syscall that
// took no argument. It is [`crate::syscap::SysCap::shutdown`] now: it is an
// authority over the whole machine, and every one of those is a bit on a
// capability rather than a call anything can make.

