use core::arch::naked_asm;

use alloc::vec::Vec;
use super::{cpu, gdt};
use crate::drivers::acpi;
use crate::sync::SyncCell;
use crate::{fd, keyboard, log, pipe, process, user_heap, vfs};

// MSR addresses
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;

// Syscall numbers
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
const SYS_SEEK: u64 = 13;
const SYS_FSTAT: u64 = 14;
const SYS_FSYNC: u64 = 15;
const SYS_EXEC: u64 = 16;
const SYS_READDIR: u64 = 17;
const SYS_DELETE: u64 = 18;
const SYS_SHUTDOWN: u64 = 19;
const SYS_CHDIR: u64 = 20;
const SYS_GETCWD: u64 = 21;
const SYS_SET_KEYBOARD_LAYOUT: u64 = 23;
const SYS_PIPE: u64 = 24;
const SYS_SPAWN: u64 = 25;
const SYS_WAITPID: u64 = 26;
const SYS_POLL: u64 = 27;

// Kernel/user RSP storage for stack switching
#[no_mangle]
pub static SYSCALL_KERNEL_RSP: SyncCell<u64> = SyncCell::new(0);
#[no_mangle]
static SYSCALL_USER_RSP: SyncCell<u64> = SyncCell::new(0);

pub fn init() {
    let efer = cpu::rdmsr(MSR_EFER);
    cpu::wrmsr(MSR_EFER, efer | 1);

    let star = (0x10u64 << 48) | ((gdt::KERNEL_CS as u64) << 32);
    cpu::wrmsr(MSR_STAR, star);
    cpu::wrmsr(MSR_LSTAR, syscall_entry as u64);
    cpu::wrmsr(MSR_FMASK, 0x200);
}

// syscall entry: save user RSP on kernel stack (survives context switches)
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    naked_asm!(
        "mov [rip + SYSCALL_USER_RSP], rsp",
        "mov rsp, [rip + SYSCALL_KERNEL_RSP]",
        "push [rip + SYSCALL_USER_RSP]",  // user RSP on kernel stack
        "push rcx",         // return RIP
        "push r11",         // return RFLAGS
        "push rdi",
        "push rsi",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",
        "call {handler}",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r11",
        "pop rcx",
        "pop rsp",          // restore user RSP from kernel stack
        "sysretq",
        handler = sym syscall_handler,
    );
}

extern "C" fn syscall_handler(num: u64, a1: u64, a2: u64, _: u64, a3: u64, a4: u64) -> u64 {
    syscall_dispatch(num, a1, a2, a3, a4)
}

fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    match num {
        SYS_WRITE => {
            let buf = unsafe { core::slice::from_raw_parts(a2 as *const u8, a3 as usize) };
            sys_write(a1, buf)
        }
        SYS_READ => {
            let buf = unsafe { core::slice::from_raw_parts_mut(a2 as *mut u8, a3 as usize) };
            sys_read(a1, buf)
        }
        SYS_ALLOC => user_heap::alloc(a1 as usize, a2 as usize),
        SYS_FREE => { user_heap::free(a1 as *mut u8, a2 as usize); 0 }
        SYS_REALLOC => user_heap::realloc(a1 as *mut u8, a2 as usize, a3 as usize, a4 as usize),
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_RANDOM => { sys_random(a1 as *mut u8, a2 as usize); 0 }
        SYS_SCREEN_SIZE => screen_size(),
        SYS_CLOCK => crate::clock::nanos_since_boot(),
        SYS_OPEN => sys_open(a1, a2, a3),
        SYS_CLOSE => {
            let proc = process::current();
            fd::close(&mut proc.fds, vfs::global(), a1)
        }
        SYS_SEEK => {
            let proc = process::current();
            fd::seek(&mut proc.fds, a1, a2 as i64, a3)
        }
        SYS_FSTAT => {
            let proc = process::current();
            fd::fstat(&mut proc.fds, a1)
        }
        SYS_FSYNC => {
            let proc = process::current();
            fd::fsync(&mut proc.fds, vfs::global(), a1)
        }
        SYS_EXEC => sys_exec(a1, a2, a3, a4),
        SYS_READDIR => sys_readdir(a1, a2, a3, a4),
        SYS_DELETE => sys_delete(a1, a2),
        SYS_SHUTDOWN => { acpi::shutdown(); }
        SYS_CHDIR => sys_chdir(a1, a2),
        SYS_GETCWD => sys_getcwd(a1, a2),
        SYS_SET_KEYBOARD_LAYOUT => sys_set_keyboard_layout(a1, a2),
        SYS_PIPE => sys_pipe(),
        SYS_SPAWN => sys_spawn(a1, a2, a3, a4),
        SYS_WAITPID => sys_waitpid(a1),
        SYS_POLL => sys_poll(a1, a2),
        _ => u64::MAX,
    }
}

fn sys_write(fd_num: u64, buf: &[u8]) -> u64 {
    loop {
        let proc = process::current();
        match fd::try_write(&mut proc.fds, fd_num, buf) {
            Some(n) => return n,
            None => {
                let proc = process::current();
                let reason = match proc.fds.get(fd_num as usize).and_then(|s| s.as_ref()) {
                    Some(fd::Descriptor::PipeWrite(id)) => process::ProcessState::BlockedPipeWrite(*id),
                    _ => return u64::MAX,
                };
                process::block(reason);
            }
        }
    }
}

fn sys_read(fd_num: u64, buf: &mut [u8]) -> u64 {
    loop {
        let proc = process::current();
        match fd::try_read(&mut proc.fds, fd_num, buf) {
            Some(n) => return n,
            None => {
                let proc = process::current();
                let reason = match proc.fds.get(fd_num as usize).and_then(|s| s.as_ref()) {
                    Some(fd::Descriptor::Keyboard) => process::ProcessState::BlockedKeyboard,
                    Some(fd::Descriptor::PipeRead(id)) => process::ProcessState::BlockedPipeRead(*id),
                    _ => return u64::MAX,
                };
                process::block(reason);
            }
        }
    }
}

fn sys_open(path_ptr: u64, path_len: u64, flags: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };
    let proc = process::current();
    let resolved = vfs::global().resolve_absolute(&proc.cwd, path);
    let proc = process::current();
    fd::open(&mut proc.fds, vfs::global(), &resolved, flags)
}

fn sys_exit(code: i32) -> u64 {
    process::exit(code);
}

fn sys_random(buf: *mut u8, len: usize) {
    for i in 0..len {
        unsafe { *buf.add(i) = cpu::rdrand() as u8; }
    }
}

/// Spawn child, optionally capture stdout, wait for exit.
fn sys_exec(argv_ptr: u64, argv_len: u64, out_buf_ptr: u64, out_buf_max: u64) -> u64 {
    let buf = unsafe { core::slice::from_raw_parts(argv_ptr as *const u8, argv_len as usize) };
    let Ok(text) = core::str::from_utf8(buf) else { return u64::MAX };
    let args: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();

    let capture = out_buf_max > 0;

    if capture {
        // Create a pipe for stdout capture
        let pipe_id = match pipe::create() {
            Some(id) => id,
            None => return u64::MAX,
        };

        // Allocate pipe FDs in parent
        let proc = process::current();
        let read_fd = fd::alloc(&mut proc.fds, fd::Descriptor::PipeRead(pipe_id));
        let write_fd = fd::alloc(&mut proc.fds, fd::Descriptor::PipeWrite(pipe_id));
        if read_fd == u64::MAX || write_fd == u64::MAX {
            return u64::MAX;
        }

        // Spawn child with pipe as stdout, inherit stdin
        let child_pid = process::spawn(&args, u64::MAX, write_fd);
        if child_pid == u64::MAX {
            return u64::MAX;
        }

        // Close write end in parent (child has its own reference)
        let proc = process::current();
        fd::close(&mut proc.fds, vfs::global(), write_fd);

        // Read all output from pipe
        let mut output = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let proc = process::current();
            match fd::try_read(&mut proc.fds, read_fd, &mut tmp) {
                Some(0) => break, // EOF
                Some(n) => output.extend_from_slice(&tmp[..n as usize]),
                None => {
                    // Block on pipe read
                    let proc = process::current();
                    let pipe_id = match proc.fds.get(read_fd as usize).and_then(|s| s.as_ref()) {
                        Some(fd::Descriptor::PipeRead(id)) => *id,
                        _ => break,
                    };
                    process::block(process::ProcessState::BlockedPipeRead(pipe_id));
                }
            }
        }

        // Close read end
        let proc = process::current();
        fd::close(&mut proc.fds, vfs::global(), read_fd);

        // Wait for child
        let exit_code = loop {
            if let Some(code) = process::collect_zombie(child_pid as u32) {
                break code;
            }
            process::block(process::ProcessState::BlockedWaitPid(child_pid as u32));
        };

        // Copy output to caller's buffer
        let copy_len = output.len().min(out_buf_max as usize);
        if copy_len > 0 && out_buf_ptr != 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    output.as_ptr(),
                    out_buf_ptr as *mut u8,
                    copy_len,
                );
            }
        }

        ((exit_code as u64) << 32) | (copy_len as u64)
    } else {
        // No capture — spawn child with inherited FDs and wait
        let child_pid = process::spawn(&args, u64::MAX, u64::MAX);
        if child_pid == u64::MAX {
            return u64::MAX;
        }

        // Wait for child to exit
        let exit_code = loop {
            if let Some(code) = process::collect_zombie(child_pid as u32) {
                break code;
            }
            process::block(process::ProcessState::BlockedWaitPid(child_pid as u32));
        };

        ((exit_code as u64) << 32) | 0u64
    }
}

fn sys_readdir(path_ptr: u64, path_len: u64, buf_ptr: u64, buf_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };

    let cwd = process::current().cwd.clone();
    let entries = match vfs::global().list(&cwd, path) {
        Ok(e) => e,
        Err(_) => return u64::MAX,
    };

    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize) };
    let mut pos = 0;
    for (name, size) in &entries {
        let is_dir = name.ends_with('/');
        let clean_name = if is_dir { &name[..name.len() - 1] } else { name.as_str() };
        let needed = 1 + clean_name.len() + 1 + 8;
        if pos + needed > buf.len() {
            break;
        }
        buf[pos] = if is_dir { 2 } else { 1 };
        pos += 1;
        buf[pos..pos + clean_name.len()].copy_from_slice(clean_name.as_bytes());
        pos += clean_name.len();
        buf[pos] = 0;
        pos += 1;
        buf[pos..pos + 8].copy_from_slice(&size.to_le_bytes());
        pos += 8;
    }
    pos as u64
}

fn sys_delete(path_ptr: u64, path_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };
    let cwd = process::current().cwd.clone();
    let resolved = vfs::global().resolve_absolute(&cwd, path);
    if vfs::global().delete(&resolved) { 0 } else { u64::MAX }
}

fn sys_chdir(path_ptr: u64, path_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };
    let proc = process::current();
    let cwd = proc.cwd.clone();
    match vfs::global().cd(&cwd, path) {
        Some(new_cwd) => {
            process::current().cwd = new_cwd;
            0
        }
        None => u64::MAX,
    }
}

fn sys_getcwd(buf_ptr: u64, buf_len: u64) -> u64 {
    let proc = process::current();
    let cwd = &proc.cwd;
    let len = cwd.len().min(buf_len as usize);
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len) };
    buf.copy_from_slice(&cwd.as_bytes()[..len]);
    len as u64
}

fn sys_set_keyboard_layout(name_ptr: u64, name_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let Ok(name) = core::str::from_utf8(slice) else { return u64::MAX };
    if keyboard::set_layout(name) {
        if !vfs::global().write_file("/nvme/config/keyboard_layout", name.as_bytes()) {
            log!("warning: failed to persist keyboard layout");
        }
        0
    } else {
        u64::MAX
    }
}

fn sys_pipe() -> u64 {
    let pipe_id = match pipe::create() {
        Some(id) => id,
        None => return u64::MAX,
    };
    let proc = process::current();
    let read_fd = fd::alloc(&mut proc.fds, fd::Descriptor::PipeRead(pipe_id));
    let write_fd = fd::alloc(&mut proc.fds, fd::Descriptor::PipeWrite(pipe_id));
    if read_fd == u64::MAX || write_fd == u64::MAX {
        return u64::MAX;
    }
    (read_fd << 32) | write_fd
}

fn sys_spawn(argv_ptr: u64, argv_len: u64, stdin_fd: u64, stdout_fd: u64) -> u64 {
    let buf = unsafe { core::slice::from_raw_parts(argv_ptr as *const u8, argv_len as usize) };
    let Ok(text) = core::str::from_utf8(buf) else { return u64::MAX };
    let args: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
    process::spawn(&args, stdin_fd, stdout_fd)
}

fn sys_waitpid(pid: u64) -> u64 {
    let child_pid = pid as u32;
    loop {
        if let Some(code) = process::collect_zombie(child_pid) {
            return code as u64;
        }
        process::block(process::ProcessState::BlockedWaitPid(child_pid));
    }
}

fn sys_poll(fd1: u64, fd2: u64) -> u64 {
    loop {
        crate::drivers::xhci::poll_global();
        let proc = process::current();
        let r1 = fd::has_data(&proc.fds, fd1);
        let r2 = fd::has_data(&proc.fds, fd2);
        let mask = (r1 as u64) | ((r2 as u64) << 1);
        if mask != 0 {
            return mask;
        }
        process::block(process::ProcessState::BlockedPoll(fd1 as u32, fd2 as u32));
    }
}

// Screen size globals (set during kernel init, font is always 8x16)
static SCREEN_COLS: SyncCell<usize> = SyncCell::new(80);
static SCREEN_ROWS: SyncCell<usize> = SyncCell::new(24);

pub fn set_screen_size(width: u32, height: u32) {
    *SCREEN_COLS.get_mut() = width as usize / 8;
    *SCREEN_ROWS.get_mut() = height as usize / 16;
}

fn screen_size() -> u64 {
    let cols = *SCREEN_COLS.get() as u64;
    let rows = *SCREEN_ROWS.get() as u64;
    (rows << 32) | cols
}

/// Terminate the current userspace process (called from exception handlers).
pub fn kill_process(code: i32) -> ! {
    process::exit(code);
}
