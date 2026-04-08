//! Hardware device access.
//!
//! Each function claims exclusive access to a device and returns a file
//! descriptor. Read the device info struct from the fd before using the device.

use crate::OwnedFd;
use crate::syscall::{self, DeviceType, SyscallError};

/// Claim exclusive access to the keyboard device.
pub fn open_keyboard() -> Result<OwnedFd, SyscallError> {
    syscall::open_device(DeviceType::Keyboard).map(OwnedFd::new)
}

/// Claim exclusive access to the mouse device.
pub fn open_mouse() -> Result<OwnedFd, SyscallError> {
    syscall::open_device(DeviceType::Mouse).map(OwnedFd::new)
}

/// Claim exclusive access to the framebuffer device.
pub fn open_framebuffer() -> Result<OwnedFd, SyscallError> {
    syscall::open_device(DeviceType::Framebuffer).map(OwnedFd::new)
}

/// Claim exclusive access to the network interface.
pub fn open_nic() -> Result<OwnedFd, SyscallError> {
    syscall::open_device(DeviceType::Nic).map(OwnedFd::new)
}

/// Claim exclusive access to the audio output device.
pub fn open_audio() -> Result<OwnedFd, SyscallError> {
    syscall::open_device(DeviceType::Audio).map(OwnedFd::new)
}
