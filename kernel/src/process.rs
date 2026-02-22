use alloc::alloc::{alloc_zeroed, Layout};
use alloc::vec::Vec;
use core::arch::asm;

use core::fmt::Write;
use crate::arch::{gdt, paging, syscall};
use crate::drivers::serial;
use crate::{elf, log, symbols};

const USER_STACK_SIZE: usize = 64 * 1024; // 64 KB

pub fn run(data: &[u8], args: &[&str]) -> i32 {
    let loaded = match elf::load(data) {
        Ok(l) => l,
        Err(msg) => {
            log::println(msg);
            return -1;
        }
    };

    // Mark ELF memory as user-accessible
    paging::map_user(loaded.base_ptr as u64, loaded.load_size as u64);

    // Allocate page-aligned stack for the program
    let stack_layout = Layout::from_size_align(USER_STACK_SIZE, 4096).unwrap();
    let stack_base = unsafe { alloc_zeroed(stack_layout) };
    if stack_base.is_null() {
        log::println("process: stack allocation failed");
        return -1;
    }
    let stack_top = stack_base as u64 + USER_STACK_SIZE as u64;
    paging::map_user(stack_base as u64, USER_STACK_SIZE as u64);

    // Load symbols for crash diagnostics
    symbols::load_process(data, loaded.base);
    symbols::set_process_ranges(
        loaded.base_ptr as u64,
        loaded.base_ptr as u64 + loaded.load_size as u64,
        stack_base as u64,
        stack_top,
    );

    // Set up user heap for syscall allocations
    crate::user_heap::init();

    // Write argc/argv onto the user stack (Linux-style layout).
    // Stack grows down from stack_top. We place:
    //   1. Null-terminated arg strings at the top
    //   2. argc (u64) + argv[] pointer array + NULL below, 16-byte aligned
    // RSP will point to argc on entry.
    let mut sp = stack_top;

    // 1. Write arg strings at top of stack, collect their user-space addresses
    let mut argv_ptrs: Vec<u64> = Vec::with_capacity(args.len());
    for arg in args.iter().rev() {
        sp -= (arg.len() + 1) as u64; // +1 for null terminator
        unsafe {
            core::ptr::copy_nonoverlapping(arg.as_ptr(), sp as *mut u8, arg.len());
            *((sp + arg.len() as u64) as *mut u8) = 0; // null terminator
        }
        argv_ptrs.push(sp);
    }
    argv_ptrs.reverse(); // restore original order

    // 2. Reserve space for metadata (argc + argv[0..n] + NULL), align to 16
    let metadata_qwords = args.len() + 2; // argc + argv pointers + NULL
    sp = (sp - metadata_qwords as u64 * 8) & !15;

    // 3. Write argc at sp, argv pointers at sp+8.., NULL terminator at end
    unsafe {
        *(sp as *mut u64) = args.len() as u64; // argc
        for (i, ptr) in argv_ptrs.iter().enumerate() {
            *((sp + 8 + i as u64 * 8) as *mut u64) = *ptr;
        }
        *((sp + 8 + args.len() as u64 * 8) as *mut u64) = 0; // NULL
    }

    let _ = writeln!(serial::SerialWriter, "process: entry={:#x}, stack={:#x}, argc={}", loaded.entry, sp, args.len());
    let exit_code = execute(loaded.entry, sp);

    // Clear symbols on process exit
    symbols::clear();

    // TODO: free program memory and stack
    exit_code
}

fn execute(entry: u64, stack_top: u64) -> i32 {
    let code: u64;
    unsafe {
        let krsp_ptr = syscall::SYSCALL_KERNEL_RSP.as_ptr();
        let tss_rsp0_ptr = gdt::tss_rsp0_ptr();
        *syscall::PROCESS_ACTIVE.get_mut() = true;

        // Save callee-saved registers on the kernel stack, then enter ring 3
        // via iretq. sys_exit restores the kernel RSP and `ret`s to label 2:.
        asm!(
            // Save callee-saved registers on kernel stack
            "push rbp",
            "push rbx",
            "push r12",
            "push r13",
            "push r14",
            "push r15",

            // Push return address (sys_exit will `ret` here)
            "lea rax, [rip + 2f]",
            "push rax",

            // Save kernel RSP for syscall entry, sys_exit, and ring-3 interrupts
            "mov [{krsp}], rsp",
            "mov [{tss_rsp0}], rsp",

            // Enter ring 3 via iretq
            "push 0x1B",        // SS:  user_data | RPL=3
            "push {stack}",     // RSP: user stack
            "push 0x202",       // RFLAGS: IF=1
            "push 0x23",        // CS:  user_code | RPL=3
            "push {entry}",     // RIP: entry point
            "iretq",

            // sys_exit restores kernel RSP and `ret`s here
            "2:",
            "sti",              // re-enable interrupts (FMASK cleared IF on last syscall)

            // Restore callee-saved registers
            "pop r15",
            "pop r14",
            "pop r13",
            "pop r12",
            "pop rbx",
            "pop rbp",

            krsp = in(reg) krsp_ptr,
            tss_rsp0 = in(reg) tss_rsp0_ptr,
            entry = in(reg) entry,
            stack = in(reg) stack_top,
            out("rax") code,
            clobber_abi("sysv64"),
        );
    }
    code as i32
}
