#[cfg(target_os = "toyos")]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    unsafe extern "C" {
        fn main() -> i32;
    }
    let code = unsafe { main() };
    unsafe { crate::sys::toyos_exit(code) }
}
