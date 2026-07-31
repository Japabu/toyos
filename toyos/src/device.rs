//! Hardware device access.
//!
//! Typed device wrappers. Each type claims exclusive access to its device
//! and provides typed read methods.

use toyos_abi::syscall::{self, DeviceType, SyscallError};
use crate::{Device, Handle, AsHandle};
use toyos_abi::Fd;

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

pub struct Keyboard(pub(crate) Device);

impl Keyboard {
    pub fn open() -> Result<Self, SyscallError> {
        syscall::open_device(DeviceType::Keyboard).map(|fd| Keyboard(Device(Handle(fd))))
    }

    pub fn fd(&self) -> Fd { self.0.fd() }

    /// Non-blocking read of pending key events; empty surfaces as `Err(WouldBlock)`.
    ///
    /// Event loops must only ever read this fd non-blocking. The kernel wakes
    /// keyboard watchers only when a report queued an event, so readiness and
    /// "there is data" agree today — but a blocking read that loses the race
    /// with another reader parks the caller until the next real key, and an
    /// event loop that stops pumping is a frozen window.
    pub fn read_nonblock(&self, buf: &mut [u8]) -> Result<usize, SyscallError> {
        self.0.0.read_nonblock(buf)
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

    /// Non-blocking read of pending mouse events; empty surfaces as `Err(WouldBlock)`.
    ///
    /// Same rationale as [`Keyboard::read_nonblock`]: an event loop that can
    /// park on an empty queue is a frozen window.
    pub fn read_nonblock(&self, buf: &mut [u8]) -> Result<usize, SyscallError> {
        self.0.0.read_nonblock(buf)
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

    /// Drain all pending completion records in one nonblocking read.
    ///
    /// Returns the number of records written to `records`. The kernel ring
    /// holds at most 16 records, so a 16-entry buffer always drains fully.
    /// Empty ring surfaces as `Err(WouldBlock)`.
    pub fn read_completions(
        &self,
        records: &mut [toyos_abi::audio::AudioCompletionRecord],
    ) -> Result<usize, SyscallError> {
        const REC_SIZE: usize = toyos_abi::audio::AudioCompletionRecord::SIZE;
        let buf = unsafe {
            core::slice::from_raw_parts_mut(
                records.as_mut_ptr() as *mut u8,
                records.len() * REC_SIZE,
            )
        };
        let n = syscall::read_nonblock(self.0.0.0, buf)?;
        assert_eq!(n % REC_SIZE, 0, "partial audio completion record ({n} bytes)");
        Ok(n / REC_SIZE)
    }

    /// PCM STOP. The reverse transition needs no method: the kernel starts a
    /// stopped stream inside the next buffer submit.
    pub fn stop(&self) -> Result<(), SyscallError> {
        syscall::write(self.0.0.0, &[0])?;
        Ok(())
    }
}

impl AsHandle for AudioDev {
    fn as_handle(&self) -> Fd { self.0.fd() }
}
