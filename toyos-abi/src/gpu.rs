//! GPU and screen operations.

use crate::FramebufferInfo;
use crate::syscall::{self, SyscallError};

/// Get the screen size as (width, height) in pixels.
pub fn screen_size() -> (usize, usize) {
    syscall::screen_size()
}

/// Set the screen size from pixel dimensions.
pub fn set_screen_size(width: u32, height: u32) {
    syscall::set_screen_size(width, height);
}

/// Flush a screen region to the display. Pass `(0, 0, 0, 0)` for full screen.
pub fn present(x: u32, y: u32, w: u32, h: u32) {
    syscall::gpu_present(x, y, w, h);
}

/// Upload the cursor image from the cursor backing buffer and enable hardware cursor.
pub fn set_cursor(hot_x: u32, hot_y: u32) {
    syscall::gpu_set_cursor(hot_x, hot_y);
}

/// Move the hardware cursor to screen position (x, y).
pub fn move_cursor(x: u32, y: u32) {
    syscall::gpu_move_cursor(x, y);
}

/// Request a GPU resolution change. Returns the new [`FramebufferInfo`] on success.
pub fn set_resolution(width: u32, height: u32) -> Result<FramebufferInfo, SyscallError> {
    let mut info = unsafe { core::mem::zeroed::<FramebufferInfo>() };
    unsafe {
        syscall::gpu_set_resolution(width, height, &mut info as *mut FramebufferInfo as *mut u8)?;
    }
    Ok(info)
}
