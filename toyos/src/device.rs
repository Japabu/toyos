//! Hardware device access.
//!
//! Typed device wrappers. Each type claims exclusive access to its device
//! and provides typed read methods.

use toyos_abi::syscall::{self, DeviceType, SyscallError};
use crate::{Device, Handle, AsHandle};
use toyos_abi::Fd;

// ---------------------------------------------------------------------------
// Generic read_info — reads a Copy struct from a device fd
// ---------------------------------------------------------------------------

pub(crate) fn read_info<T: Copy>(dev: &Device) -> Result<T, SyscallError> {
    let size = core::mem::size_of::<T>();
    let mut val = unsafe { core::mem::zeroed::<T>() };
    let buf = unsafe {
        core::slice::from_raw_parts_mut(&mut val as *mut T as *mut u8, size)
    };
    let n = syscall::read(dev.0.0, buf)?;
    assert_eq!(n, size, "device info size mismatch");
    Ok(val)
}

// ---------------------------------------------------------------------------
// Typed devices
// ---------------------------------------------------------------------------

pub struct Keyboard(pub(crate) Device);

impl Keyboard {
    pub fn open() -> Result<Self, SyscallError> {
        syscall::open_device(DeviceType::Keyboard).map(|fd| Keyboard(Device(Handle(fd))))
    }

    pub fn fd(&self) -> Fd { self.0.fd() }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, SyscallError> {
        self.0.read(buf)
    }
}

impl AsHandle for Keyboard {
    fn as_handle(&self) -> Fd { self.0.fd() }
}

pub struct Mouse(pub(crate) Device);

impl Mouse {
    pub fn open() -> Result<Self, SyscallError> {
        syscall::open_device(DeviceType::Mouse).map(|fd| Mouse(Device(Handle(fd))))
    }

    pub fn fd(&self) -> Fd { self.0.fd() }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, SyscallError> {
        self.0.read(buf)
    }
}

impl AsHandle for Mouse {
    fn as_handle(&self) -> Fd { self.0.fd() }
}

pub struct FramebufferDev(pub(crate) Device);

impl FramebufferDev {
    pub fn open() -> Result<Self, SyscallError> {
        syscall::open_device(DeviceType::Framebuffer).map(|fd| FramebufferDev(Device(Handle(fd))))
    }

    pub fn info(&self) -> Result<toyos_abi::FramebufferInfo, SyscallError> {
        read_info(&self.0)
    }
}

impl AsHandle for FramebufferDev {
    fn as_handle(&self) -> Fd { self.0.fd() }
}

pub struct Nic(pub(crate) Device);

impl Nic {
    pub fn open() -> Result<Self, SyscallError> {
        syscall::open_device(DeviceType::Nic).map(|fd| Nic(Device(Handle(fd))))
    }

    pub fn fd(&self) -> Fd { self.0.fd() }

    pub fn info(&self) -> Result<toyos_abi::net::NicInfo, SyscallError> {
        read_info(&self.0)
    }
}

impl AsHandle for Nic {
    fn as_handle(&self) -> Fd { self.0.fd() }
}

pub struct AudioDev(pub(crate) Device);

impl AudioDev {
    pub fn open() -> Result<Self, SyscallError> {
        syscall::open_device(DeviceType::Audio).map(|fd| AudioDev(Device(Handle(fd))))
    }

    pub fn info(&self) -> Result<toyos_abi::audio::AudioInfo, SyscallError> {
        read_info(&self.0)
    }

    /// Read completed DMA buffer bitmask. Blocks until at least one buffer completes.
    pub fn read_completions(&self) -> Result<u32, SyscallError> {
        let mut mask = 0u32;
        let buf = unsafe {
            core::slice::from_raw_parts_mut(&mut mask as *mut u32 as *mut u8, 4)
        };
        syscall::read(self.0.0.0, buf)?;
        Ok(mask)
    }
}

impl AsHandle for AudioDev {
    fn as_handle(&self) -> Fd { self.0.fd() }
}
