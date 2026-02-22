use core::arch::{asm, naked_asm};

use alloc::vec::Vec;
use super::{cpu, gdt};
use crate::drivers::serial;
use crate::sync::SyncCell;
use crate::{console, keyboard, user_heap, vfs::Vfs};

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

// Global VFS pointer (set once in main, lives for the duration of the kernel)
pub(crate) static VFS_PTR: SyncCell<*mut Vfs> = SyncCell::new(core::ptr::null_mut());

pub fn set_vfs(vfs: &mut Vfs) {
    *VFS_PTR.get_mut() = vfs as *mut _;
}

fn vfs() -> &'static mut Vfs {
    unsafe { &mut **VFS_PTR.get_mut() }
}

// Output capture buffer for SYS_EXEC (redirects SYS_WRITE to buffer instead of console)
static CAPTURE_BUF: SyncCell<Option<Vec<u8>>> = SyncCell::new(None);

// Kernel/user RSP storage for stack switching (single-core, no swapgs needed)
#[no_mangle]
pub static SYSCALL_KERNEL_RSP: SyncCell<u64> = SyncCell::new(0);
#[no_mangle]
static SYSCALL_USER_RSP: SyncCell<u64> = SyncCell::new(0);

pub fn init() {
    // Enable syscall/sysret in EFER (set SCE bit 0)
    let efer = cpu::rdmsr(MSR_EFER);
    cpu::wrmsr(MSR_EFER, efer | 1);

    // STAR: bits 47:32 = kernel CS (for syscall), bits 63:48 = user base (for sysret)
    // syscall:  CS = STAR[47:32] = 0x08, SS = STAR[47:32]+8 = 0x10
    // sysretq:  CS = STAR[63:48]+16 = 0x20 (RPL=3 → 0x23), SS = STAR[63:48]+8 = 0x18 (RPL=3 → 0x1B)
    let star = (0x10u64 << 48) | ((gdt::KERNEL_CS as u64) << 32);
    cpu::wrmsr(MSR_STAR, star);

    // LSTAR: syscall entry point
    cpu::wrmsr(MSR_LSTAR, syscall_entry as u64);

    // FMASK: mask IF (bit 9) on syscall entry to disable interrupts
    cpu::wrmsr(MSR_FMASK, 0x200);
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
pub static PROCESS_ACTIVE: SyncCell<bool> = SyncCell::new(false);

extern "C" fn syscall_handler(num: u64, a1: u64, a2: u64, _: u64, a3: u64, a4: u64) -> u64 {
    match num {
        SYS_WRITE => sys_write(a1 as *const u8, a2 as usize),
        SYS_READ => sys_read(a1 as *mut u8, a2 as usize),
        SYS_ALLOC => user_heap::alloc(a1 as usize, a2 as usize),
        SYS_FREE => { user_heap::free(a1 as *mut u8, a2 as usize); 0 }
        SYS_REALLOC => user_heap::realloc(a1 as *mut u8, a2 as usize, a3 as usize, a4 as usize),
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_RANDOM => { sys_random(a1 as *mut u8, a2 as usize); 0 }
        SYS_SCREEN_SIZE => {
            let (cols, rows) = console::screen_size();
            ((rows as u64) << 32) | (cols as u64)
        }
        SYS_CLOCK => crate::clock::nanos_since_boot(),
        SYS_OPEN => {
            let path = unsafe {
                let slice = core::slice::from_raw_parts(a1 as *const u8, a2 as usize);
                core::str::from_utf8_unchecked(slice)
            };
            crate::fd::open(vfs(), path, a3)
        }
        SYS_CLOSE => crate::fd::close(vfs(), a1),
        SYS_READ_FILE => {
            let buf = unsafe { core::slice::from_raw_parts_mut(a2 as *mut u8, a3 as usize) };
            crate::fd::read(a1, buf)
        }
        SYS_WRITE_FILE => {
            let buf = unsafe { core::slice::from_raw_parts(a2 as *const u8, a3 as usize) };
            crate::fd::write(a1, buf)
        }
        SYS_SEEK => crate::fd::seek(a1, a2 as i64, a3),
        SYS_FSTAT => crate::fd::fstat(a1),
        SYS_FSYNC => crate::fd::fsync(vfs(), a1),
        SYS_EXEC => sys_exec(a1, a2, a3, a4),
        _ => u64::MAX, // unknown syscall
    }
}

fn sys_write(buf: *const u8, len: usize) -> u64 {
    let s = unsafe { core::slice::from_raw_parts(buf, len) };
    serial::print(unsafe { core::str::from_utf8_unchecked(s) });
    if let Some(ref mut capture) = *CAPTURE_BUF.get_mut() {
        capture.extend_from_slice(s);
    } else {
        for &byte in s {
            console::putchar(byte);
        }
    }
    len as u64
}

fn sys_read(buf: *mut u8, len: usize) -> u64 {
    // Line-buffered read with echo and backspace handling.
    // Blocks until '\n' is received or buffer is full.
    let mut count = 0usize;
    loop {
        if count >= len { break; }
        crate::drivers::xhci::poll_global(); // pump USB keyboard events
        if let Some(ch) = keyboard::try_read_char() {
            match ch {
                b'\n' => {
                    console::putchar(b'\n');
                    unsafe { *buf.add(count) = b'\n'; }
                    count += 1;
                    break;
                }
                0x08 | 0x7F => {
                    if count > 0 {
                        count -= 1;
                        console::putchar(0x08);
                        console::putchar(b' ');
                        console::putchar(0x08);
                    }
                }
                ch => {
                    console::putchar(ch);
                    unsafe { *buf.add(count) = ch; }
                    count += 1;
                }
            }
        } else {
            core::hint::spin_loop();
        }
    }
    count as u64
}

fn sys_exit(code: i32) -> u64 {
    let active = *PROCESS_ACTIVE.get();
    if !active {
        cpu::halt();
    }
    kill_process(code)
}

/// Terminate the current userspace process and return to execute()'s landing label.
/// Used by sys_exit and exception handlers.
pub fn kill_process(code: i32) -> ! {
    *PROCESS_ACTIVE.get_mut() = false;
    let krsp = *SYSCALL_KERNEL_RSP.get();
    unsafe {
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
        unsafe { *buf.add(i) = cpu::rdrand() as u8; }
    }
}

fn sys_exec(path_ptr: u64, path_len: u64, out_buf_ptr: u64, out_buf_max: u64) -> u64 {
    let path = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        core::str::from_utf8_unchecked(slice)
    };

    // Load binary from VFS
    let binary = match vfs().read_file(path) {
        Some(data) => data,
        None => return u64::MAX,
    };

    // Save parent process state
    let saved_heap = user_heap::save();
    let saved_user_rsp = *SYSCALL_USER_RSP.get();
    let saved_kernel_rsp = *SYSCALL_KERNEL_RSP.get();
    let saved_active = *PROCESS_ACTIVE.get();
    let saved_tss_rsp0 = unsafe { *gdt::tss_rsp0_ptr() };

    // Enable output capture
    *CAPTURE_BUF.get_mut() = Some(Vec::new());

    // Init fresh heap for child and run
    user_heap::init();
    let exit_code = crate::process::run(&binary, &[path]);

    // Collect captured output
    let captured = core::mem::replace(CAPTURE_BUF.get_mut(), None).unwrap_or_default();

    // Restore parent process state
    *PROCESS_ACTIVE.get_mut() = saved_active;
    *SYSCALL_USER_RSP.get_mut() = saved_user_rsp;
    *SYSCALL_KERNEL_RSP.get_mut() = saved_kernel_rsp;
    unsafe { *gdt::tss_rsp0_ptr() = saved_tss_rsp0; }
    user_heap::restore(saved_heap);

    // Copy captured output to parent's buffer
    let copy_len = captured.len().min(out_buf_max as usize);
    if copy_len > 0 && out_buf_ptr != 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(
                captured.as_ptr(),
                out_buf_ptr as *mut u8,
                copy_len,
            );
        }
    }

    ((exit_code as u64) << 32) | (copy_len as u64)
}
