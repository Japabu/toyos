#![no_std]

use core::arch::asm;

const SYS_WRITE: u64 = 0;
const SYS_READ: u64 = 1;
const SYS_ALLOC: u64 = 2;
const SYS_FREE: u64 = 3;
const SYS_REALLOC: u64 = 4;
const SYS_EXIT: u64 = 5;
const SYS_RANDOM: u64 = 6;

// --- Console I/O ---

#[no_mangle]
pub extern "C" fn toyos_write(buf: *const u8, len: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_WRITE as u64 => ret,
            in("rdi") buf,
            in("rsi") len,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

#[no_mangle]
pub extern "C" fn toyos_read(buf: *mut u8, len: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_READ as u64 => ret,
            in("rdi") buf,
            in("rsi") len,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

// --- Memory allocation ---

#[no_mangle]
pub extern "C" fn toyos_alloc(size: usize, align: usize) -> *mut u8 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_ALLOC as u64 => ret,
            in("rdi") size,
            in("rsi") align,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret as *mut u8
}

#[no_mangle]
pub extern "C" fn toyos_free(ptr: *mut u8, size: usize, align: usize) {
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_FREE as u64 => _,
            in("rdi") ptr,
            in("rsi") size,
            in("rdx") align,
            out("rcx") _,
            out("r11") _,
        );
    }
}

#[no_mangle]
pub extern "C" fn toyos_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_REALLOC as u64 => ret,
            in("rdi") ptr,
            in("rsi") size,
            in("rdx") align,
            in("r10") new_size,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret as *mut u8
}

// --- Process ---

#[no_mangle]
pub extern "C" fn toyos_exit(code: i32) -> ! {
    unsafe {
        asm!(
            "2: syscall",
            "jmp 2b",
            in("rax") SYS_EXIT,
            in("rdi") code as u64,
            options(noreturn),
        );
    }
}

// --- Random ---

#[no_mangle]
pub extern "C" fn toyos_random(buf: *mut u8, len: usize) {
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_RANDOM as u64 => _,
            in("rdi") buf,
            in("rsi") len,
            out("rcx") _,
            out("r11") _,
        );
    }
}

// --- Compiler builtins (needed by generated code) ---

#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        *dest.add(i) = *src.add(i);
        i += 1;
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, c: i32, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        *dest.add(i) = c as u8;
        i += 1;
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let a = *s1.add(i);
        let b = *s2.add(i);
        if a != b {
            return a as i32 - b as i32;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dest as usize) < (src as usize) {
        // Copy forward
        let mut i = 0;
        while i < n {
            *dest.add(i) = *src.add(i);
            i += 1;
        }
    } else {
        // Copy backward (overlapping, dest > src)
        let mut i = n;
        while i > 0 {
            i -= 1;
            *dest.add(i) = *src.add(i);
        }
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn bcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    memcmp(s1, s2, n)
}
