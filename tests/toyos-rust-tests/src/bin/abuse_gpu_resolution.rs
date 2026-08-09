//! `SYS_GPU_SET_RESOLUTION` must be reachable only through the framebuffer
//! claim.
//!
//! It turns two arbitrary `u32`s into `width * height * 4` bytes of contiguous
//! physical memory, so an ungated caller both reconfigures the display and
//! names a kernel allocation size.
//!
//! **The gate used to be a pid comparison** — "is the caller the process that
//! opened the device?" — and the claim is the argument now, so what this binary
//! presents is the two things a process without one can present: nothing, and
//! a handle that is not a claim.

use toyos_abi::handle::HANDLE_INVALID;
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::{FramebufferInfo, RawHandle};

fn resize(claim: RawHandle, width: u32, height: u32) -> Result<(), SyscallError> {
    let mut info = unsafe { core::mem::zeroed::<FramebufferInfo>() };
    unsafe {
        syscall::gpu_set_resolution(
            claim,
            width,
            height,
            &mut info as *mut FramebufferInfo as *mut u8,
        )
    }
}

fn main() {
    // **Two refusals, and they are different facts.** A handle that resolves to
    // nothing is `NotFound` — the table saying there is no such handle — and a
    // handle that resolves to something other than a framebuffer claim is
    // `PermissionDenied`. Both are checked, because a caller with no claim can
    // present either.

    // 20000x20000x4 = 1.6 GB of contiguous 2 MiB pages.
    let err = resize(HANDLE_INVALID, 20_000, 20_000)
        .expect_err("a process with no framebuffer claim must not resize the framebuffer");
    assert_eq!(err, SyscallError::NotFound, "wrong error for a huge resize");

    // width * height * 4 overflows u32 well before the allocator sees it.
    let err = resize(HANDLE_INVALID, u32::MAX, u32::MAX)
        .expect_err("a saturated resize must be rejected");
    assert_eq!(err, SyscallError::NotFound, "wrong error for a saturated resize");

    // The claim check must fire ahead of any driver work, not instead of it:
    // a perfectly sane resolution from a non-claimant is refused too.
    let err =
        resize(HANDLE_INVALID, 640, 480).expect_err("a sane resize from a non-claimant is refused");
    assert_eq!(err, SyscallError::NotFound, "wrong error for a sane resize");

    // A handle that resolves and is not a claim: stdout. The type is what the
    // kernel checks, so a caller cannot reach the display by presenting
    // whatever it happens to hold.
    let err = resize(RawHandle(1), 640, 480).expect_err("stdout is not a framebuffer claim");
    assert_eq!(err, SyscallError::PermissionDenied, "wrong error for a wrong-typed handle");

    println!("gpu resolution changes refused without a framebuffer claim");
}
