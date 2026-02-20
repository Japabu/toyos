use core::arch::{asm, naked_asm};

use crate::{console, keyboard, gdt};

// MSR addresses
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;

// Syscall numbers (must match toolchain patches)
const SYS_WRITE: u64 = 0;
const SYS_READ: u64 = 1;
const SYS_ALLOC: u64 = 2;
const SYS_FREE: u64 = 3;
const SYS_REALLOC: u64 = 4;
const SYS_EXIT: u64 = 5;
const SYS_RANDOM: u64 = 6;

// Kernel/user RSP storage for stack switching (single-core, no swapgs needed)
#[no_mangle]
pub static mut SYSCALL_KERNEL_RSP: u64 = 0;
#[no_mangle]
static mut SYSCALL_USER_RSP: u64 = 0;

#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high);
}

#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high);
    (high as u64) << 32 | low as u64
}

pub fn init() {
    unsafe {
        // Enable syscall/sysret in EFER (set SCE bit 0)
        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | 1);

        // STAR: bits 47:32 = kernel CS (for syscall), bits 63:48 = user base (for sysret)
        // syscall:  CS = STAR[47:32] = 0x08, SS = STAR[47:32]+8 = 0x10
        // sysretq:  CS = STAR[63:48]+16 = 0x20 (RPL=3 → 0x23), SS = STAR[63:48]+8 = 0x18 (RPL=3 → 0x1B)
        let star = (0x10u64 << 48) | ((gdt::KERNEL_CS as u64) << 32);
        wrmsr(MSR_STAR, star);

        // LSTAR: syscall entry point
        wrmsr(MSR_LSTAR, syscall_entry as u64);

        // FMASK: mask IF (bit 9) on syscall entry to disable interrupts
        wrmsr(MSR_FMASK, 0x200);
    }
}

// Low-level syscall entry point (called by `syscall` instruction from ring 3)
// Syscall ABI matches SysV with RCX skipped (hardware clobbers it):
//   RDI=num, RSI=a1, RDX=a2, R8=a3, R9=a4
//   RCX=return RIP (set by hardware), R11=return RFLAGS (set by hardware)
//   RSP=user stack (CPU does NOT switch stacks on syscall)
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    naked_asm!(
        "mov [rip + SYSCALL_USER_RSP], rsp",
        "mov rsp, [rip + SYSCALL_KERNEL_RSP]",
        "push rcx",
        "push r11",
        "call {handler}",
        "pop r11",
        "pop rcx",
        "mov rsp, [rip + SYSCALL_USER_RSP]",
        "sysretq",
        handler = sym syscall_handler,
    );
}

// Whether a userspace process is active (checked by sys_exit)
pub static mut PROCESS_ACTIVE: bool = false;

extern "C" fn syscall_handler(num: u64, a1: u64, a2: u64, _: u64, a3: u64, a4: u64) -> u64 {
    match num {
        SYS_WRITE => sys_write(a1 as *const u8, a2 as usize),
        SYS_READ => sys_read(a1 as *mut u8, a2 as usize),
        SYS_ALLOC => sys_alloc(a1 as usize, a2 as usize),
        SYS_FREE => { sys_free(a1 as *mut u8, a2 as usize, a3 as usize); 0 }
        SYS_REALLOC => sys_realloc(a1 as *mut u8, a2 as usize, a3 as usize, a4 as usize),
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_RANDOM => { sys_random(a1 as *mut u8, a2 as usize); 0 }
        _ => u64::MAX, // unknown syscall
    }
}

fn sys_write(buf: *const u8, len: usize) -> u64 {
    for i in 0..len {
        let byte = unsafe { *buf.add(i) };
        console::putchar(byte);
    }
    len as u64
}

fn sys_read(buf: *mut u8, len: usize) -> u64 {
    let mut count = 0usize;
    while count < len {
        if let Some(ch) = keyboard::try_read_char() {
            unsafe { *buf.add(count) = ch; }
            count += 1;
            // Return after first char (non-blocking-ish) for line-based input
            if ch == b'\n' {
                break;
            }
        } else {
            if count > 0 {
                break; // return what we have
            }
            // Spin-wait for at least one character
            core::hint::spin_loop();
        }
    }
    count as u64
}

fn sys_alloc(size: usize, align: usize) -> u64 {
    use alloc::alloc::{alloc, Layout};
    if size == 0 {
        return 0;
    }
    let layout = match Layout::from_size_align(size, align) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    unsafe { alloc(layout) as u64 }
}

fn sys_free(ptr: *mut u8, size: usize, align: usize) {
    use alloc::alloc::{dealloc, Layout};
    if ptr.is_null() || size == 0 {
        return;
    }
    if let Ok(layout) = Layout::from_size_align(size, align) {
        unsafe { dealloc(ptr, layout); }
    }
}

fn sys_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> u64 {
    use alloc::alloc::{realloc, Layout};
    if ptr.is_null() {
        return sys_alloc(new_size, align);
    }
    let layout = match Layout::from_size_align(size, align) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    unsafe { realloc(ptr, layout, new_size) as u64 }
}

fn sys_exit(code: i32) -> u64 {
    unsafe {
        let active = (&raw const PROCESS_ACTIVE).read();
        if !active {
            loop { asm!("hlt"); }
        }
        (&raw mut PROCESS_ACTIVE).write(false);

        // Restore kernel RSP and `ret` to execute()'s landing label.
        // Callee-saved registers are restored there via stack pops.
        let krsp = (&raw const SYSCALL_KERNEL_RSP).read();
        asm!(
            "mov rsp, {krsp}",
            "mov rax, {code}",
            "ret",
            krsp = in(reg) krsp,
            code = in(reg) code as u64,
            options(noreturn),
        );
    }
}

fn sys_random(buf: *mut u8, len: usize) {
    for i in 0..len {
        let val: u64;
        unsafe {
            asm!(
                "2: rdrand {val}",
                "jnc 2b",
                val = out(reg) val,
            );
            *buf.add(i) = val as u8;
        }
    }
}
