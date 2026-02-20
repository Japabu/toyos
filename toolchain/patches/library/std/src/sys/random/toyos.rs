use core::arch::asm;

const SYS_RANDOM: u64 = 6;

pub fn fill_bytes(buf: &mut [u8]) {
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_RANDOM => _,
            in("rdi") buf.as_mut_ptr(),
            in("rsi") buf.len(),
            out("rcx") _,
            out("r11") _,
        );
    }
}
