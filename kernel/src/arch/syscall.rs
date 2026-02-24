use core::arch::naked_asm;

use alloc::vec::Vec;
use super::{cpu, gdt};
use crate::drivers::acpi;
use crate::sync::Lock;
use crate::{device, fd, keyboard, log, message, pipe, process, vfs};

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
const SYS_MARK_TTY: u64 = 28;
const SYS_SEND_MSG: u64 = 29;
const SYS_RECV_MSG: u64 = 30;
const SYS_OPEN_DEVICE: u64 = 31;
const SYS_REGISTER_NAME: u64 = 32;
const SYS_FIND_PID: u64 = 33;

pub fn init() {
    let efer = cpu::rdmsr(MSR_EFER);
    cpu::wrmsr(MSR_EFER, efer | 1);

    let star = (0x10u64 << 48) | ((gdt::KERNEL_CS as u64) << 32);
    cpu::wrmsr(MSR_STAR, star);
    cpu::wrmsr(MSR_LSTAR, syscall_entry as u64);
    cpu::wrmsr(MSR_FMASK, 0x200);
}

// Syscall entry: swapgs to get kernel GS, use GS-relative kernel/user RSP.
// PerCpu layout: offset 16 = kernel_rsp, offset 24 = user_rsp.
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    naked_asm!(
        "swapgs",
        "mov gs:[24], rsp",     // save user RSP to percpu.user_rsp
        "mov rsp, gs:[16]",     // load kernel RSP from percpu.kernel_rsp
        "push gs:[24]",         // user RSP on kernel stack
        "push rcx",             // return RIP
        "push r11",             // return RFLAGS
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
        "pop rsp",              // restore user RSP from kernel stack
        "swapgs",
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
        SYS_ALLOC => process::with_current_mut(|p| crate::user_heap::alloc(&mut p.user_heap, a1 as usize, a2 as usize)),
        SYS_FREE => { process::with_current_mut(|p| crate::user_heap::free(&mut p.user_heap, a1 as *mut u8, a2 as usize)); 0 }
        SYS_REALLOC => process::with_current_mut(|p| crate::user_heap::realloc(&mut p.user_heap, a1 as *mut u8, a2 as usize, a3 as usize, a4 as usize)),
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_RANDOM => { sys_random(a1 as *mut u8, a2 as usize); 0 }
        SYS_SCREEN_SIZE => screen_size(),
        SYS_CLOCK => crate::clock::nanos_since_boot(),
        SYS_OPEN => sys_open(a1, a2, a3),
        SYS_CLOSE => sys_close(a1),
        SYS_SEEK => process::with_current_mut(|proc| fd::seek(&mut proc.fds, a1, a2 as i64, a3)),
        SYS_FSTAT => process::with_current_mut(|proc| fd::fstat(&mut proc.fds, a1)),
        SYS_FSYNC => process::with_current_mut(|proc| fd::fsync(&mut proc.fds, &mut *vfs::lock(), a1)),
        SYS_READDIR => sys_readdir(a1, a2, a3, a4),
        SYS_DELETE => sys_delete(a1, a2),
        SYS_SHUTDOWN => {
            while !pipe::all_empty() {
                process::yield_now();
            }
            acpi::shutdown();
        }
        SYS_CHDIR => sys_chdir(a1, a2),
        SYS_GETCWD => sys_getcwd(a1, a2),
        SYS_SET_KEYBOARD_LAYOUT => sys_set_keyboard_layout(a1, a2),
        SYS_PIPE => sys_pipe(),
        SYS_SPAWN => sys_spawn(a1, a2, a3, a4),
        SYS_WAITPID => sys_waitpid(a1),
        SYS_POLL => sys_poll(a1, a2),
        SYS_MARK_TTY => process::with_current_mut(|proc| fd::mark_tty(&mut proc.fds, a1)),
        SYS_SEND_MSG => sys_send_msg(a1, a2),
        SYS_RECV_MSG => sys_recv_msg(a1),
        SYS_OPEN_DEVICE => sys_open_device(a1),
        SYS_REGISTER_NAME => sys_register_name(a1, a2),
        SYS_FIND_PID => sys_find_pid(a1, a2),
        _ => u64::MAX,
    }
}

fn sys_write(fd_num: u64, buf: &[u8]) -> u64 {
    loop {
        let result = process::with_current_mut(|proc| fd::try_write(&mut proc.fds, fd_num, buf));
        match result {
            Some(n) => {
                let pipe_id = process::with_current(|proc| match proc.fds.get(fd_num) {
                    Some(fd::Descriptor::PipeWrite(id)) | Some(fd::Descriptor::TtyWrite(id)) => Some(*id),
                    _ => None,
                });
                if let Some(pipe_id) = pipe_id {
                    process::wake_pipe_readers(pipe_id);
                }
                return n;
            }
            None => {
                let reason = process::with_current(|proc| match proc.fds.get(fd_num) {
                    Some(fd::Descriptor::PipeWrite(id)) | Some(fd::Descriptor::TtyWrite(id)) =>
                        Some(process::ProcessState::BlockedPipeWrite(*id)),
                    _ => None,
                });
                match reason {
                    Some(r) => process::block(r),
                    None => return u64::MAX,
                }
            }
        }
    }
}

fn sys_read(fd_num: u64, buf: &mut [u8]) -> u64 {
    loop {
        let result = process::with_current_mut(|proc| fd::try_read(&mut proc.fds, fd_num, buf));
        match result {
            Some(n) => {
                let pipe_id = process::with_current(|proc| match proc.fds.get(fd_num) {
                    Some(fd::Descriptor::PipeRead(id)) | Some(fd::Descriptor::TtyRead(id)) => Some(*id),
                    _ => None,
                });
                if let Some(pipe_id) = pipe_id {
                    process::wake_pipe_writers(pipe_id);
                }
                return n;
            }
            None => {
                let reason = process::with_current(|proc| match proc.fds.get(fd_num) {
                    Some(fd::Descriptor::Keyboard) => Some(process::ProcessState::BlockedKeyboard),
                    Some(fd::Descriptor::PipeRead(id)) | Some(fd::Descriptor::TtyRead(id)) =>
                        Some(process::ProcessState::BlockedPipeRead(*id)),
                    _ => None,
                });
                match reason {
                    Some(r) => process::block(r),
                    None => return u64::MAX,
                }
            }
        }
    }
}

fn sys_open(path_ptr: u64, path_len: u64, flags: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };
    let cwd = process::with_current(|p| p.cwd.clone());
    let resolved = vfs::lock().resolve_absolute(&cwd, path);
    process::with_current_mut(|proc| fd::open(&mut proc.fds, &mut *vfs::lock(), &resolved, flags))
}

fn sys_close(fd_num: u64) -> u64 {
    let pid = process::current_pid();
    let wake = process::with_current(|proc| match proc.fds.get(fd_num) {
        Some(fd::Descriptor::PipeWrite(id)) | Some(fd::Descriptor::TtyWrite(id)) => Some((true, *id)),
        Some(fd::Descriptor::PipeRead(id)) | Some(fd::Descriptor::TtyRead(id)) => Some((false, *id)),
        _ => None,
    });
    let result = process::with_current_mut(|proc| {
        fd::close(&mut proc.fds, &mut *vfs::lock(), fd_num, pid)
    });
    if let Some((is_write, pipe_id)) = wake {
        if is_write {
            process::wake_pipe_readers(pipe_id);
        } else {
            process::wake_pipe_writers(pipe_id);
        }
    }
    result
}

fn sys_exit(code: i32) -> u64 {
    process::exit(code);
}

fn sys_random(buf: *mut u8, len: usize) {
    for i in 0..len {
        unsafe { *buf.add(i) = cpu::rdrand() as u8; }
    }
}

fn sys_readdir(path_ptr: u64, path_len: u64, buf_ptr: u64, buf_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };

    let cwd = process::with_current(|p| p.cwd.clone());
    let entries = match vfs::lock().list(&cwd, path) {
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
    let cwd = process::with_current(|p| p.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    if vfs.delete(&resolved) { 0 } else { u64::MAX }
}

fn sys_chdir(path_ptr: u64, path_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(slice) else { return u64::MAX };
    let cwd = process::with_current(|p| p.cwd.clone());
    match vfs::lock().cd(&cwd, path) {
        Some(new_cwd) => {
            process::with_current_mut(|p| p.cwd = new_cwd);
            0
        }
        None => u64::MAX,
    }
}

fn sys_getcwd(buf_ptr: u64, buf_len: u64) -> u64 {
    process::with_current(|proc| {
        let cwd = &proc.cwd;
        let len = cwd.len().min(buf_len as usize);
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len) };
        buf.copy_from_slice(&cwd.as_bytes()[..len]);
        len as u64
    })
}

fn sys_set_keyboard_layout(name_ptr: u64, name_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let Ok(name) = core::str::from_utf8(slice) else { return u64::MAX };
    if keyboard::set_layout(name) {
        if !vfs::lock().write_file("/nvme/config/keyboard_layout", name.as_bytes()) {
            log!("warning: failed to persist keyboard layout");
        }
        0
    } else {
        u64::MAX
    }
}

fn sys_pipe() -> u64 {
    let pipe_id = pipe::create();
    process::with_current_mut(|proc| {
        let read_fd = fd::alloc(&mut proc.fds, fd::Descriptor::PipeRead(pipe_id));
        let write_fd = fd::alloc(&mut proc.fds, fd::Descriptor::PipeWrite(pipe_id));
        (read_fd << 32) | write_fd
    })
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

fn sys_poll(fds_ptr: u64, fds_len: u64) -> u64 {
    let fds = unsafe { core::slice::from_raw_parts(fds_ptr as *const u64, fds_len as usize) };
    loop {
        crate::drivers::xhci::poll_global();
        let result = process::with_current(|proc| {
            let mut mask: u64 = 0;
            for (i, &fd) in fds.iter().enumerate() {
                if fd::has_data(&proc.fds, fd) {
                    mask |= 1 << i;
                }
            }
            if proc.messages.has_messages() {
                mask |= 1 << fds_len;
            }
            mask
        });
        if result != 0 {
            return result;
        }
        process::block(process::ProcessState::BlockedPoll(fds_ptr, fds_len as u32));
    }
}

fn sys_send_msg(target_pid: u64, msg_ptr: u64) -> u64 {
    let sender = process::current_pid();
    let user_msg = unsafe { &*(msg_ptr as *const message::Message) };
    let msg = message::Message {
        sender,
        msg_type: user_msg.msg_type,
        data: user_msg.data,
        len: user_msg.len,
    };
    if process::send_message(target_pid as u32, msg) { 0 } else { u64::MAX }
}

fn sys_recv_msg(msg_ptr: u64) -> u64 {
    loop {
        let msg = process::with_current_mut(|proc| proc.messages.pop());
        if let Some(msg) = msg {
            unsafe { *(msg_ptr as *mut message::Message) = msg; }
            return 0;
        }
        process::block(process::ProcessState::BlockedRecvMsg);
    }
}

// Screen size globals (set during kernel init, font is always 8x16)
static SCREEN_COLS: Lock<usize> = Lock::new(80);
static SCREEN_ROWS: Lock<usize> = Lock::new(24);

pub fn set_screen_size(width: u32, height: u32) {
    *SCREEN_COLS.lock() = width as usize / 8;
    *SCREEN_ROWS.lock() = height as usize / 16;
}

fn screen_size() -> u64 {
    let cols = *SCREEN_COLS.lock() as u64;
    let rows = *SCREEN_ROWS.lock() as u64;
    (rows << 32) | cols
}

fn sys_open_device(device_type: u64) -> u64 {
    let pid = process::current_pid();
    let desc = match device::try_claim(device_type, pid) {
        Some(d) => d,
        None => return u64::MAX,
    };
    process::with_current_mut(|proc| fd::alloc(&mut proc.fds, desc))
}

fn sys_register_name(name_ptr: u64, name_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let Ok(name) = core::str::from_utf8(slice) else { return u64::MAX };
    let pid = process::current_pid();
    if process::register_name(name, pid) { 0 } else { u64::MAX }
}

fn sys_find_pid(name_ptr: u64, name_len: u64) -> u64 {
    let slice = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let Ok(name) = core::str::from_utf8(slice) else { return u64::MAX };
    match process::find_pid(name) {
        Some(pid) => pid as u64,
        None => u64::MAX,
    }
}

/// Terminate the current userspace process (called from exception handlers).
pub fn kill_process(code: i32) -> ! {
    process::exit(code);
}
