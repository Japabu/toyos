//! `SYS_GPU_SET_RESOLUTION` must be reachable only by the process that holds
//! the framebuffer device.
//!
//! It used to take two arbitrary `u32`s from any process and turn them into
//! `width * height * 4` bytes of contiguous physical memory behind an
//! `expect("framebuffer alloc failed")`.

use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::FramebufferInfo;

fn resize(width: u32, height: u32) -> Result<(), SyscallError> {
    let mut info = unsafe { core::mem::zeroed::<FramebufferInfo>() };
    unsafe { syscall::gpu_set_resolution(width, height, &mut info as *mut FramebufferInfo as *mut u8) }
}

fn main() {
    // 20000x20000x4 = 1.6 GB of contiguous 2 MiB pages.
    let err = resize(20_000, 20_000)
        .expect_err("an unprivileged process must not resize the framebuffer");
    assert_eq!(err, SyscallError::PermissionDenied, "wrong error for a huge resize");

    // width * height * 4 overflows u32 well before the allocator sees it.
    let err = resize(u32::MAX, u32::MAX).expect_err("a saturated resize must be rejected");
    assert_eq!(err, SyscallError::PermissionDenied, "wrong error for a saturated resize");

    // The claim check must fire ahead of any driver work, not instead of it:
    // a perfectly sane resolution from a non-claimant is refused too.
    let err = resize(640, 480).expect_err("a sane resize from a non-claimant must be refused");
    assert_eq!(err, SyscallError::PermissionDenied, "wrong error for a sane resize");

    println!("gpu resolution changes refused without a framebuffer claim");
}
