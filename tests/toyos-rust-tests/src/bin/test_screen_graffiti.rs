//! Ask the kernel to paint over the screen the console owns, and survive it.
//!
//! Separate from `test_panic_child` because that one reports it as an error
//! when `debug` returns — every action it was written for kills the caller.
//! This action must return, or the console would have nothing left to be asked
//! to clean up.
fn main() {
    toyos_abi::syscall::debug(8);
}
