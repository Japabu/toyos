use core::arch::{asm, naked_asm};

use alloc::vec::Vec;
use crate::{console, keyboard, gdt, paging, serial, vfs::Vfs};

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
pub(crate) static mut VFS_PTR: *mut Vfs = core::ptr::null_mut();

pub fn set_vfs(vfs: &mut Vfs) {
    unsafe { (&raw mut VFS_PTR).write(vfs as *mut _); }
}

unsafe fn vfs() -> &'static mut Vfs {
    &mut *(&raw mut VFS_PTR).read()
}

// Output capture buffer for SYS_EXEC (redirects SYS_WRITE to buffer instead of console)
static mut CAPTURE_BUF: Option<Vec<u8>> = None;

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
            crate::fd::open(unsafe { vfs() }, path, a3)
        }
        SYS_CLOSE => crate::fd::close(unsafe { vfs() }, a1),
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
        SYS_FSYNC => crate::fd::fsync(unsafe { vfs() }, a1),
        SYS_EXEC => sys_exec(a1, a2, a3, a4),
        _ => u64::MAX, // unknown syscall
    }
}

fn sys_write(buf: *const u8, len: usize) -> u64 {
    let s = unsafe { core::slice::from_raw_parts(buf, len) };
    serial::print(unsafe { core::str::from_utf8_unchecked(s) });
    unsafe {
        if let Some(ref mut capture) = *(&raw mut CAPTURE_BUF) {
            capture.extend_from_slice(s);
        } else {
            for &byte in s {
                console::putchar(byte);
            }
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
        crate::xhci::poll_global(); // pump USB keyboard events
        if let Some(ch) = keyboard::try_read_char() {
            match ch {
                b'\n' => {
                    console::putchar(b'\n');
                    unsafe { *buf.add(count) = b'\n'; }
                    count += 1;
                    break;
                }
                0x08 | 0x7F => {
                    // Backspace: erase last character from buffer and screen
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

// --- User heap (free-list allocator) ---
// Page-aligned chunks from the kernel allocator, mapped USER.
// Free regions tracked in a sorted Vec; first-fit alloc, merge-on-free.

const USER_HEAP_CHUNK: usize = 1024 * 1024; // 1MB

// Sorted list of free regions: (start, end)
static mut USER_HEAP_FREE: Vec<(u64, u64)> = Vec::new();

#[inline]
unsafe fn heap_free() -> &'static mut Vec<(u64, u64)> {
    &mut *(&raw mut USER_HEAP_FREE)
}

/// Allocate initial user heap. Called from elf.rs before executing a program.
pub fn init_user_heap() {
    unsafe { heap_free().clear(); }
    grow_user_heap(USER_HEAP_CHUNK);
}

fn grow_user_heap(min_size: usize) {
    use alloc::alloc::{alloc_zeroed, Layout};
    let size = (min_size.max(USER_HEAP_CHUNK) + 4095) & !4095;
    let layout = Layout::from_size_align(size, 4096).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "user heap: out of memory");
    paging::map_user(ptr as u64, size as u64);
    // Insert into free list (sorted)
    let start = ptr as u64;
    let end = start + size as u64;
    unsafe {
        let fl = heap_free();
        let pos = fl.iter().position(|&(s, _)| s > start).unwrap_or(fl.len());
        fl.insert(pos, (start, end));
    }
}

/// First-fit search across free regions.
unsafe fn try_alloc(size: u64, align: u64) -> Option<u64> {
    let fl = heap_free();
    for i in 0..fl.len() {
        let (start, end) = fl[i];
        let aligned = (start + align - 1) & !(align - 1);
        let alloc_end = aligned + size;

        if alloc_end <= end {
            if aligned > start && alloc_end < end {
                fl[i] = (start, aligned);
                fl.insert(i + 1, (alloc_end, end));
            } else if aligned > start {
                fl[i] = (start, aligned);
            } else if alloc_end < end {
                fl[i] = (alloc_end, end);
            } else {
                fl.remove(i);
            }
            return Some(aligned);
        }
    }
    None
}

fn sys_alloc(size: usize, align: usize) -> u64 {
    if size == 0 { return 0; }
    let align = align.max(1) as u64;
    let sz = size as u64;

    unsafe {
        if let Some(addr) = try_alloc(sz, align) {
            serial::println(&alloc::format!("alloc({}, {}) = {:#x}", size, align, addr));
            return addr;
        }
    }
    // Grow and retry
    grow_user_heap(size + align as usize);
    let addr = unsafe { try_alloc(sz, align).expect("user heap: alloc failed after grow") };
    serial::println(&alloc::format!("alloc({}, {}) = {:#x}", size, align, addr));
    addr
}

fn sys_free(ptr: *mut u8, size: usize, _align: usize) {
    if ptr.is_null() || size == 0 { return; }
    let addr = ptr as u64;
    serial::println(&alloc::format!("free({:#x}, {})", addr, size));
    let end = addr + size as u64;
    unsafe {
        let fl = heap_free();
        let pos = fl.iter().position(|&(s, _)| s > addr).unwrap_or(fl.len());
        fl.insert(pos, (addr, end));
        // Merge with next
        if pos + 1 < fl.len() && fl[pos].1 >= fl[pos + 1].0 {
            fl[pos].1 = fl[pos + 1].1;
            fl.remove(pos + 1);
        }
        // Merge with prev
        if pos > 0 && fl[pos - 1].1 >= fl[pos].0 {
            fl[pos - 1].1 = fl[pos].1;
            fl.remove(pos);
        }
    }
}

fn sys_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> u64 {
    if ptr.is_null() {
        return sys_alloc(new_size, align);
    }
    if new_size <= size {
        return ptr as u64;
    }
    let new_ptr = sys_alloc(new_size, align);
    if new_ptr == 0 { return 0; }
    unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr as *mut u8, size); }
    sys_free(ptr, size, align);
    new_ptr
}

fn sys_exit(code: i32) -> u64 {
    unsafe {
        let active = (&raw const PROCESS_ACTIVE).read();
        if !active {
            loop { asm!("hlt"); }
        }
    }
    kill_process(code)
}

/// Terminate the current userspace process and return to execute()'s landing label.
/// Used by sys_exit and exception handlers.
pub fn kill_process(code: i32) -> ! {
    unsafe {
        (&raw mut PROCESS_ACTIVE).write(false);
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

fn sys_exec(path_ptr: u64, path_len: u64, out_buf_ptr: u64, out_buf_max: u64) -> u64 {
    let path = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        core::str::from_utf8_unchecked(slice)
    };

    // Load binary from VFS
    let binary = match unsafe { vfs() }.read_file(path) {
        Some(data) => data,
        None => return u64::MAX,
    };

    // Save parent process state
    let saved_heap = unsafe { heap_free().clone() };
    let saved_user_rsp = unsafe { (&raw const SYSCALL_USER_RSP).read() };
    let saved_kernel_rsp = unsafe { (&raw const SYSCALL_KERNEL_RSP).read() };
    let saved_active = unsafe { (&raw const PROCESS_ACTIVE).read() };
    let saved_tss_rsp0 = unsafe { *gdt::tss_rsp0_ptr() };

    // Enable output capture
    unsafe { *(&raw mut CAPTURE_BUF) = Some(Vec::new()); }

    // Init fresh heap for child and run
    init_user_heap();
    let exit_code = crate::elf::run(&binary);

    // Collect captured output
    let captured = unsafe {
        core::mem::replace(&mut *(&raw mut CAPTURE_BUF), None)
    }.unwrap_or_default();

    // Restore parent process state
    unsafe {
        (&raw mut PROCESS_ACTIVE).write(saved_active);
        (&raw mut SYSCALL_USER_RSP).write(saved_user_rsp);
        (&raw mut SYSCALL_KERNEL_RSP).write(saved_kernel_rsp);
        *gdt::tss_rsp0_ptr() = saved_tss_rsp0;
        *heap_free() = saved_heap;
    }

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
