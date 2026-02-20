#![allow(unsafe_op_in_unsafe_fn)]

/// The configure builtins provides runtime support compiler-builtin features
/// which require dynamic initialization to work as expected, e.g. aarch64
/// outline-atomics.
mod configure_builtins;

/// The PAL (platform abstraction layer) contains platform-specific abstractions
/// for implementing the features in the other submodules, e.g. UNIX file
/// descriptors.
mod pal;

mod alloc;
mod entry;
mod personality;

pub mod anonymous_pipe;
pub mod args;
pub mod backtrace;
pub mod cmath;
pub mod env;
pub mod env_consts;
pub mod exit_guard;
pub mod fd;
pub mod fs;
pub mod io;
pub mod net;
pub mod os_str;
pub mod path;
pub mod platform_version;
pub mod process;
pub mod random;
pub mod stdio;
pub mod sync;
pub mod thread;
pub mod thread_local;

// ToyOS syscall ABI: SysV with RCX skipped (hardware clobbers it).
//   RDI=num, RSI=a1, RDX=a2, R8=a3, R9=a4, return in RAX.
#[cfg(target_os = "toyos")]
pub(crate) fn syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rdi") num,
            in("rsi") a1,
            in("rdx") a2,
            in("r8") a3,
            in("r9") a4,
            lateout("rax") ret,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

// FIXME(117276): remove this, move feature implementations into individual
//                submodules.
pub use pal::*;
