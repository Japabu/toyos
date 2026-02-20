#[cfg(target_os = "toyos")]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    unsafe extern "C" {
        fn main() -> i32;
    }
    let code = unsafe { main() };

    const SYS_EXIT: u64 = 5;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_EXIT,
            in("rdi") code as u64,
            options(noreturn),
        );
    }
}
