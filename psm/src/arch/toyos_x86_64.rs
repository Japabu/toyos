//! PSM functions for ToyOS x86_64, implemented as naked Rust functions.
//! This avoids the need for a C assembler to compile the GAS `.s` files.

use core::arch::naked_asm;

const STACK_DIRECTION_DESCENDING: u8 = 2;

#[unsafe(no_mangle)]
#[unsafe(naked)]
unsafe extern "sysv64" fn rust_psm_stack_direction() -> u8 {
    naked_asm!(
        "mov al, {dir}",
        "ret",
        dir = const STACK_DIRECTION_DESCENDING,
    );
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
unsafe extern "sysv64" fn rust_psm_stack_pointer() -> *mut u8 {
    naked_asm!(
        "lea rax, [rsp + 8]",
        "ret",
    );
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
unsafe extern "sysv64" fn rust_psm_replace_stack() {
    // sysv64: rdi=data, rsi=callback, rdx=sp
    naked_asm!(
        "lea rsp, [rdx - 8]",
        "jmp rsi",
    );
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
unsafe extern "sysv64" fn rust_psm_on_stack() {
    // sysv64: rdi=data, rsi=return_ptr, rdx=callback, rcx=sp
    naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "mov rsp, rcx",
        "call rdx",
        "mov rsp, rbp",
        "pop rbp",
        "ret",
    );
}
