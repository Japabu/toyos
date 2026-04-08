//! GPU and screen operations.

use toyos_abi::FramebufferInfo;
use toyos_abi::syscall::{self, SyscallError};

pub fn screen_size() -> (usize, usize) {
    syscall::screen_size()
}

pub fn set_screen_size(width: u32, height: u32) {
    syscall::set_screen_size(width, height);
}

pub fn present(x: u32, y: u32, w: u32, h: u32) {
    syscall::gpu_present(x, y, w, h);
}

pub fn set_cursor(hot_x: u32, hot_y: u32) {
    syscall::gpu_set_cursor(hot_x, hot_y);
}

pub fn move_cursor(x: u32, y: u32) {
    syscall::gpu_move_cursor(x, y);
}

pub fn set_resolution(width: u32, height: u32) -> Result<FramebufferInfo, SyscallError> {
    let mut info = unsafe { core::mem::zeroed::<FramebufferInfo>() };
    unsafe {
        syscall::gpu_set_resolution(width, height, &mut info as *mut FramebufferInfo as *mut u8)?;
    }
    Ok(info)
}
