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

// ToyOS syscall wrappers — provided by libtoyos.so at runtime.
#[cfg(target_os = "toyos")]
#[link(name = "toyos")]
unsafe extern "C" {
    // stdio
    pub(crate) fn toyos_write(buf: *const u8, len: usize) -> isize;
    pub(crate) fn toyos_read(buf: *mut u8, len: usize) -> isize;
    // alloc
    pub(crate) fn toyos_alloc(size: usize, align: usize) -> *mut u8;
    pub(crate) fn toyos_free(ptr: *mut u8, size: usize, align: usize);
    pub(crate) fn toyos_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8;
    // process
    pub(crate) fn toyos_exit(code: i32) -> !;
    pub(crate) fn toyos_exec(path: *const u8, path_len: usize, out: *mut u8, out_len: usize) -> u64;
    // misc
    pub(crate) fn toyos_random(buf: *mut u8, len: usize);
    pub(crate) fn toyos_clock() -> u64;
    // fs
    pub(crate) fn toyos_open(path: *const u8, path_len: usize, flags: u64) -> u64;
    pub(crate) fn toyos_close(fd: u64);
    pub(crate) fn toyos_read_file(fd: u64, buf: *mut u8, len: usize) -> u64;
    pub(crate) fn toyos_write_file(fd: u64, buf: *const u8, len: usize) -> u64;
    pub(crate) fn toyos_seek(fd: u64, offset: i64, whence: u64) -> u64;
    pub(crate) fn toyos_fstat(fd: u64) -> u64;
    pub(crate) fn toyos_fsync(fd: u64);
}

// FIXME(117276): remove this, move feature implementations into individual
//                submodules.
pub use pal::*;
