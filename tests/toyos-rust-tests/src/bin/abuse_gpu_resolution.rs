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
//!
//! **Both of those now end the caller rather than answering it**, which is why
//! every arm is a child. A handle a process does not hold and a handle of the
//! wrong type are bugs in that process, not conditions to report, so the
//! refusal is `exit 139` and a line in the kernel log — and a test that asserted
//! the old error words would be asserting that the bug is survivable. The child
//! prints its marker before the call, so an arm cannot pass by dying on the way
//! to it.
//!
//! One arm answers instead of killing, and it is here to keep the other four
//! honest: a handle that resolves and carries the wrong *rights* is a question
//! a process is allowed to ask, so it comes back as `PermissionDenied` and this
//! process carries on to the next arm.

use std::io::Write;
use std::process::{Command, Stdio};

use toyos_abi::handle::{Rights, HANDLE_INVALID};
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::{FramebufferInfo, RawHandle};

const SELF_PATH: &str = "/bin/test_rs_abuse_gpu_resolution";

/// `process::HANDLE_FAULT_EXIT_CODE`.
const HANDLE_FAULT: i32 = 139;

/// `(role, width, height, what the caller presented)`.
///
/// The two saturating sizes are here because the claim check must fire *ahead*
/// of the driver, and the sane one is here because it must fire *instead* of
/// nothing: an allocation guard that only rejects absurd numbers would pass the
/// first two and let the third through.
const ARMS: &[(&str, u32, u32, &str)] = &[
    // 20000x20000x4 = 1.6 GB of contiguous 2 MiB pages.
    ("absent-huge", 20_000, 20_000, "no handle at all, at 1.6 GB"),
    // width * height * 4 overflows u32 well before the allocator sees it.
    ("absent-saturated", u32::MAX, u32::MAX, "no handle at all, saturated"),
    ("absent-sane", 640, 480, "no handle at all, at a resolution the display has"),
    ("wrong-type", 640, 480, "a pipe where the call takes a framebuffer claim"),
];

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
    match std::env::args().nth(1).as_deref() {
        Some(role) => arm(role),
        None => test(),
    }
}

fn test() {
    for (role, _, _, presented) in ARMS {
        let child = Command::new(SELF_PATH)
            .arg(role)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {role}: {e}"));
        let out = child.wait_with_output().unwrap_or_else(|e| panic!("wait {role}: {e}"));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("reached {role}"),
            "{role} ({presented}) never reached the call, or answered past it",
        );
        assert_eq!(
            out.status.code(),
            Some(HANDLE_FAULT),
            "{role} ({presented}) did not end the caller",
        );
        println!("  {role}: {presented} — killed at the call");
    }

    // A handle that resolves, is of a type the call takes and carries the wrong
    // rights. The rights are checked before the type, so this never reaches the
    // claim comparison — and it is an answer, because probing what an
    // attenuated handle can still do is what attenuation is for.
    let blind = syscall::dup_narrowed(RawHandle(1), Rights::NONE)
        .expect("a handle carrying nothing is still a handle");
    assert_eq!(
        resize(blind, 640, 480),
        Err(SyscallError::PermissionDenied),
        "a handle with no rights reached the display",
    );
    syscall::close(blind);
    println!("  rights: refused with a word, and this process is still here");

    println!("gpu resolution changes are refused without a framebuffer claim");
}

fn arm(role: &str) -> ! {
    let Some(&(_, width, height, _)) = ARMS.iter().find(|(name, ..)| *name == role) else {
        panic!("unknown role {role:?}");
    };
    // Presented before the call and flushed, so "the kernel ended it here" is
    // distinguishable from "it never got here".
    println!("reached {role}");
    std::io::stdout().flush().expect("flush the marker");
    let claim = if role == "wrong-type" {
        // stdout: a handle this process certainly holds, and certainly not a
        // claim on the framebuffer.
        RawHandle(1)
    } else {
        HANDLE_INVALID
    };
    let answered = resize(claim, width, height);
    panic!("{role} was answered {answered:?} instead of ending the caller");
}
