//! Hardware device access.
//!
//! Each function claims exclusive access to a device and returns a typed
//! `Device` handle. Read the device info struct before using the device.

use toyos_abi::syscall::{self, DeviceType, SyscallError};
use crate::{Device, Handle};

pub fn open_keyboard() -> Result<Device, SyscallError> {
    syscall::open_device(DeviceType::Keyboard).map(|fd| Device(Handle(fd)))
}

pub fn open_mouse() -> Result<Device, SyscallError> {
    syscall::open_device(DeviceType::Mouse).map(|fd| Device(Handle(fd)))
}

pub fn open_framebuffer() -> Result<Device, SyscallError> {
    syscall::open_device(DeviceType::Framebuffer).map(|fd| Device(Handle(fd)))
}

pub fn open_nic() -> Result<Device, SyscallError> {
    syscall::open_device(DeviceType::Nic).map(|fd| Device(Handle(fd)))
}

pub fn open_audio() -> Result<Device, SyscallError> {
    syscall::open_device(DeviceType::Audio).map(|fd| Device(Handle(fd)))
}
