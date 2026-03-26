/// This function dereferences null, triggering a page fault that the kernel
/// cannot resolve (address 0 is unmapped). The kernel should print a SEGFAULT
/// with a backtrace that includes this function name.
#[inline(never)]
fn deliberate_null_deref() -> u64 {
    unsafe { core::ptr::read_volatile(core::ptr::null::<u64>()) }
}

fn main() {
    let _ = deliberate_null_deref();
}
