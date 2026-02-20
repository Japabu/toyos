#[cfg(target_os = "toyos")]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    unsafe extern "C" {
        fn main() -> i32;
    }
    let code = unsafe { main() };

    const SYS_EXIT: u64 = 5;
    loop { crate::sys::syscall(SYS_EXIT, code as u64, 0, 0, 0); }
}
