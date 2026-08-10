//! Screen operations, on the framebuffer claim that authorizes them.
//!
//! These were four free functions and the kernel decided whether the caller was
//! the display's owner by comparing pids. A pid is not authority: the claim is,
//! so it is an argument — and a program with no framebuffer claim cannot write
//! the call at all.

use toyos_abi::FramebufferInfo;
use toyos_abi::syscall::{self, SyscallError};

use crate::device::FramebufferDev;
use crate::AsHandle;

impl FramebufferDev {
    /// Transfer a region of the scanout to the GPU and flush it. `(0, 0, 0, 0)`
    /// is the whole screen.
    pub fn present(&self, x: u32, y: u32, w: u32, h: u32) -> Result<(), SyscallError> {
        syscall::gpu_present(self.as_handle(), x, y, w, h)
    }

    /// Upload the cursor image from the cursor buffer and enable the hardware
    /// cursor.
    pub fn set_cursor(&self, hot_x: u32, hot_y: u32) -> Result<(), SyscallError> {
        syscall::gpu_set_cursor(self.as_handle(), hot_x, hot_y)
    }

    pub fn move_cursor(&self, x: u32, y: u32) -> Result<(), SyscallError> {
        syscall::gpu_move_cursor(self.as_handle(), x, y)
    }

    /// Ask for a mode change. The answer describes the new scanout, whose
    /// buffers are fresh handles — the old ones stay mapped and valid until
    /// their holder closes them, because a capability system may not take a
    /// mapping away.
    pub fn set_resolution(&self, width: u32, height: u32) -> Result<FramebufferInfo, SyscallError> {
        let mut info = unsafe { core::mem::zeroed::<FramebufferInfo>() };
        // SAFETY: `info` is this frame's own storage and outlives the call.
        unsafe {
            syscall::gpu_set_resolution(
                self.as_handle(),
                width,
                height,
                &mut info as *mut FramebufferInfo as *mut u8,
            )?;
        }
        Ok(info)
    }
}
