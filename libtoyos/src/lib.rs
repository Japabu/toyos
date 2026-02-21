#![no_std]

// Syscall numbers (must match kernel)
const SYS_WRITE: u64 = 0;
const SYS_READ: u64 = 1;
const SYS_ALLOC: u64 = 2;
const SYS_FREE: u64 = 3;
const SYS_REALLOC: u64 = 4;
const SYS_EXIT: u64 = 5;
const SYS_RANDOM: u64 = 6;
const SYS_SCREEN_SIZE: u64 = 7;
const SYS_CLOCK: u64 = 8;
const SYS_OPEN: u64 = 9;
const SYS_CLOSE: u64 = 10;
const SYS_READ_FILE: u64 = 11;
const SYS_WRITE_FILE: u64 = 12;
const SYS_SEEK: u64 = 13;
const SYS_FSTAT: u64 = 14;
const SYS_FSYNC: u64 = 15;
const SYS_EXEC: u64 = 16;

#[inline(always)]
fn syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
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

// --- stdio ---

#[unsafe(no_mangle)]
pub extern "C" fn toyos_write(buf: *const u8, len: usize) -> isize {
    syscall(SYS_WRITE, buf as u64, len as u64, 0, 0) as isize
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_read(buf: *mut u8, len: usize) -> isize {
    syscall(SYS_READ, buf as u64, len as u64, 0, 0) as isize
}

// --- alloc ---

#[unsafe(no_mangle)]
pub extern "C" fn toyos_alloc(size: usize, align: usize) -> *mut u8 {
    syscall(SYS_ALLOC, size as u64, align as u64, 0, 0) as *mut u8
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_free(ptr: *mut u8, size: usize, align: usize) {
    syscall(SYS_FREE, ptr as u64, size as u64, align as u64, 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8 {
    syscall(SYS_REALLOC, ptr as u64, size as u64, align as u64, new_size as u64) as *mut u8
}

// --- process ---

#[unsafe(no_mangle)]
pub extern "C" fn toyos_exit(code: i32) -> ! {
    loop { syscall(SYS_EXIT, code as u64, 0, 0, 0); }
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_exec(path: *const u8, path_len: usize, out_buf: *mut u8, out_buf_len: usize) -> u64 {
    syscall(SYS_EXEC, path as u64, path_len as u64, out_buf as u64, out_buf_len as u64)
}

// --- misc ---

#[unsafe(no_mangle)]
pub extern "C" fn toyos_random(buf: *mut u8, len: usize) {
    syscall(SYS_RANDOM, buf as u64, len as u64, 0, 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_clock() -> u64 {
    syscall(SYS_CLOCK, 0, 0, 0, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_screen_size() -> u64 {
    syscall(SYS_SCREEN_SIZE, 0, 0, 0, 0)
}

// --- fs ---

#[unsafe(no_mangle)]
pub extern "C" fn toyos_open(path: *const u8, path_len: usize, flags: u64) -> u64 {
    syscall(SYS_OPEN, path as u64, path_len as u64, flags, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_close(fd: u64) {
    syscall(SYS_CLOSE, fd, 0, 0, 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_read_file(fd: u64, buf: *mut u8, len: usize) -> u64 {
    syscall(SYS_READ_FILE, fd, buf as u64, len as u64, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_write_file(fd: u64, buf: *const u8, len: usize) -> u64 {
    syscall(SYS_WRITE_FILE, fd, buf as u64, len as u64, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_seek(fd: u64, offset: i64, whence: u64) -> u64 {
    syscall(SYS_SEEK, fd, offset as u64, whence, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_fstat(fd: u64) -> u64 {
    syscall(SYS_FSTAT, fd, 0, 0, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn toyos_fsync(fd: u64) {
    syscall(SYS_FSYNC, fd, 0, 0, 0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
