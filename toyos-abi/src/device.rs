//! Hardware device access.
//!
//! Each function claims exclusive access to a device and returns a file
//! descriptor. Read the device info struct from the fd before using the device.

use crate::Fd;
use crate::syscall::{self, DeviceType, SyscallError};

/// Claim exclusive access to the keyboard device.
pub fn open_keyboard() -> Result<Fd, SyscallError> {
    syscall::open_device(DeviceType::Keyboard)
}

/// Claim exclusive access to the mouse device.
pub fn open_mouse() -> Result<Fd, SyscallError> {
    syscall::open_device(DeviceType::Mouse)
}

/// Claim exclusive access to the framebuffer device.
pub fn open_framebuffer() -> Result<Fd, SyscallError> {
    syscall::open_device(DeviceType::Framebuffer)
}

/// Claim exclusive access to the network interface.
pub fn open_nic() -> Result<Fd, SyscallError> {
    syscall::open_device(DeviceType::Nic)
}

/// Claim exclusive access to the audio output device.
pub fn open_audio() -> Result<Fd, SyscallError> {
    syscall::open_device(DeviceType::Audio)
}
