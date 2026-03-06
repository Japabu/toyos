use core::arch::naked_asm;

use alloc::vec::Vec;
use super::{cpu, gdt, paging};
use crate::drivers::acpi;
use crate::sync::Lock;
use crate::user_ptr::SyscallContext;
use crate::{allocator, device, fd, keyboard, log, message, pipe, process, shared_memory, user_heap, vfs};

// MSR addresses
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;

use toyos_abi::syscall::*;

// ---------------------------------------------------------------------------
// Heap owner routing (threads share parent's heap)
// ---------------------------------------------------------------------------

/// Run a closure with the heap owner's user_heap. For normal processes this is
/// the process itself; for threads it's the parent process.
/// Returns None if the current process or heap owner no longer exists
/// (e.g., parent exited while thread is still running).
fn with_heap_owner<R>(f: impl FnOnce(&mut user_heap::UserHeap) -> R) -> Option<R> {
    let mut guard = process::PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let pid = crate::arch::percpu::current_pid();
    let owner_pid = table.procs.get(pid)?.heap_owner;
    let owner = table.procs.get_mut(owner_pid)?;
    Some(f(&mut owner.user_heap))
}

// ---------------------------------------------------------------------------
// Syscall entry point
// ---------------------------------------------------------------------------

pub fn init() {
    let efer = cpu::rdmsr(MSR_EFER);
    cpu::wrmsr(MSR_EFER, efer | 1);

    let star = (0x10u64 << 48) | ((gdt::KERNEL_CS as u64) << 32);
    cpu::wrmsr(MSR_STAR, star);
    cpu::wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
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
    // SAFETY: current process's page tables remain active for the duration of this call.
    let ctx = unsafe { SyscallContext::new() };

    match num {
        SYS_WRITE => {
            let Some(buf) = ctx.user_slice(a2, a3) else { return u64::MAX };
            sys_write(a1, buf)
        }
        SYS_READ => {
            let Some(buf) = ctx.user_slice_mut(a2, a3) else { return u64::MAX };
            sys_read(a1, buf)
        }
        SYS_ALLOC => with_heap_owner(|heap| user_heap::alloc(heap, a1 as usize, a2 as usize)).unwrap_or(0),
        SYS_FREE => { with_heap_owner(|heap| { user_heap::free(heap, a1 as *mut u8, a2 as usize); 0 }).unwrap_or(0) }
        SYS_REALLOC => with_heap_owner(|heap| user_heap::realloc(heap, a1 as *mut u8, a2 as usize, a3 as usize, a4 as usize)).unwrap_or(0),
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_RANDOM => {
            let Some(buf) = ctx.user_slice_mut(a1, a2) else { return u64::MAX };
            sys_random(buf)
        }
        SYS_SCREEN_SIZE => screen_size(),
        SYS_CLOCK => crate::clock::nanos_since_boot(),
        SYS_OPEN => {
            let Some(path) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_open(path, a3)
        }
        SYS_CLOSE => sys_close(a1),
        SYS_SEEK => process::with_current_mut(|proc| fd::seek(&mut proc.fds, a1, a2 as i64, a3)),
        SYS_FSTAT => {
            let Some(stat) = ctx.user_mut::<fd::Stat>(a2) else { return u64::MAX };
            if process::with_current(|proc| fd::fstat(&proc.fds, a1, stat)) { 0 } else { u64::MAX }
        }
        SYS_FSYNC => process::with_current_mut(|proc| fd::fsync(&mut proc.fds, &mut *vfs::lock(), a1)),
        SYS_READDIR => {
            let Some(path) = ctx.user_str(a1, a2) else { return u64::MAX };
            let Some(buf) = ctx.user_slice_mut(a3, a4) else { return u64::MAX };
            sys_readdir(path, buf)
        }
        SYS_DELETE => {
            let Some(path) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_delete(path)
        }
        SYS_SHUTDOWN => {
            while !pipe::all_empty() {
                process::yield_now();
            }
            acpi::shutdown();
        }
        SYS_CHDIR => {
            let Some(path) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_chdir(path)
        }
        SYS_GETCWD => {
            let Some(buf) = ctx.user_slice_mut(a1, a2) else { return u64::MAX };
            sys_getcwd(buf)
        }
        SYS_SET_KEYBOARD_LAYOUT => {
            let Some(name) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_set_keyboard_layout(name)
        }
        SYS_PIPE => sys_pipe(),
        SYS_SPAWN => {
            let Some(text) = ctx.user_str(a1, a2) else { return u64::MAX };
            let count = a4 as usize;
            let fds = if count > 0 {
                let Some(pairs) = ctx.user_pod_slice::<[u32; 2]>(a3, count) else { return u64::MAX };
                process::build_child_fds(pairs)
            } else {
                fd::FdTable::new()
            };
            sys_spawn(text, fds)
        }
        SYS_WAITPID => sys_waitpid(a1),
        SYS_POLL => {
            if a2 > 63 { return u64::MAX; }
            let Some(fds) = ctx.user_pod_slice::<u64>(a1, a2 as usize) else { return u64::MAX };
            sys_poll(fds, a2, a3)
        }
        SYS_MARK_TTY => process::with_current_mut(|proc| fd::mark_tty(&mut proc.fds, a1)),
        SYS_SEND_MSG => {
            let Some(user_msg) = ctx.user_ref::<message::UserMessage>(a2) else { return u64::MAX };
            const MAX_MSG_PAYLOAD: u64 = 64 * 1024;
            let payload = if user_msg.data != 0 && user_msg.len != 0 {
                if user_msg.len > MAX_MSG_PAYLOAD { return u64::MAX; }
                let Some(data) = ctx.user_slice(user_msg.data, user_msg.len) else { return u64::MAX };
                data.to_vec()
            } else {
                Vec::new()
            };
            sys_send_msg(a1, user_msg, payload)
        }
        SYS_RECV_MSG => {
            let Some(out) = ctx.user_mut::<message::UserMessage>(a1) else { return u64::MAX };
            sys_recv_msg(out)
        }
        SYS_OPEN_DEVICE => sys_open_device(a1),
        SYS_REGISTER_NAME => {
            let Some(name) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_register_name(name)
        }
        SYS_FIND_PID => {
            let Some(name) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_find_pid(name)
        }
        SYS_SET_SCREEN_SIZE => { set_screen_size(a1 as u32, a2 as u32); 0 }
        SYS_GPU_PRESENT => { crate::gpu::present_rect(a1 as u32, a2 as u32, a3 as u32, a4 as u32); 0 }
        SYS_GPU_SET_CURSOR => { crate::gpu::set_cursor(a1 as u32, a2 as u32); 0 }
        SYS_GPU_MOVE_CURSOR => { crate::gpu::move_cursor(a1 as u32, a2 as u32); 0 }
        SYS_ALLOC_SHARED => sys_alloc_shared(a1),
        SYS_GRANT_SHARED => sys_grant_shared(a1, a2),
        SYS_MAP_SHARED => sys_map_shared(a1),
        SYS_RELEASE_SHARED => sys_release_shared(a1),
        SYS_THREAD_SPAWN => process::spawn_thread(a1, a2, a3).map_or(u64::MAX, |t| t as u64),
        SYS_THREAD_JOIN => sys_thread_join(a1),
        SYS_CLOCK_REALTIME => crate::rtc::read_time(),
        SYS_SYSINFO => {
            let Some(buf) = ctx.user_slice_mut(a1, a2) else { return u64::MAX };
            sys_sysinfo(buf)
        }
        SYS_NET_INFO => {
            let Some(buf) = ctx.user_slice_mut(a1, a2) else { return u64::MAX };
            sys_net_info(buf)
        }
        SYS_NET_SEND => {
            let Some(buf) = ctx.user_slice(a1, a2) else { return u64::MAX };
            crate::net::send(buf);
            0
        }
        SYS_NET_RECV => {
            let Some(buf) = ctx.user_slice_mut(a1, a2) else { return u64::MAX };
            sys_net_recv(buf, a3)
        }
        SYS_NANOSLEEP => sys_nanosleep(a1),
        SYS_DUP => sys_dup(a1),
        SYS_GETPID => crate::arch::percpu::current_pid() as u64,
        SYS_RENAME => {
            let Some(old) = ctx.user_str(a1, a2) else { return u64::MAX };
            let Some(new) = ctx.user_str(a3, a4) else { return u64::MAX };
            sys_rename(old, new)
        }
        SYS_MKDIR => {
            let Some(path) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_mkdir(path)
        }
        SYS_RMDIR => {
            let Some(path) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_rmdir(path)
        }
        SYS_DLOPEN => {
            let Some(path) = ctx.user_str(a1, a2) else { return u64::MAX };
            sys_dlopen(path)
        }
        SYS_DLSYM => {
            let Some(name) = ctx.user_str(a2, a3) else { return u64::MAX };
            sys_dlsym(a1, name)
        }
        SYS_DLCLOSE => 0,
        _ => u64::MAX,
    }
}

// ---------------------------------------------------------------------------
// Syscall implementations
// ---------------------------------------------------------------------------

fn sys_write(fd_num: u64, buf: &[u8]) -> u64 {
    loop {
        // Single lock: try write + determine wake/block action
        let action = process::with_current_mut(|proc| {
            match fd::try_write(&mut proc.fds, fd_num, buf) {
                Some(n) => {
                    let pipe_id = match proc.fds.get(fd_num) {
                        Some(fd::Descriptor::PipeWrite(id)) | Some(fd::Descriptor::TtyWrite(id)) => Some(*id),
                        _ => None,
                    };
                    Ok((n, pipe_id))
                }
                None => match proc.fds.get(fd_num) {
                    Some(fd::Descriptor::PipeWrite(id)) | Some(fd::Descriptor::TtyWrite(id)) =>
                        Err(Some(process::ProcessState::BlockedPipeWrite(*id))),
                    _ => Err(None),
                },
            }
        });
        match action {
            Ok((n, pipe_id)) => {
                if let Some(id) = pipe_id { process::wake_pipe_readers(id); }
                return n;
            }
            Err(Some(reason)) => process::block(reason),
            Err(None) => return u64::MAX,
        }
    }
}

fn sys_read(fd_num: u64, buf: &mut [u8]) -> u64 {
    loop {
        // Single lock: try read + determine wake/block action
        let action = process::with_current_mut(|proc| {
            match fd::try_read(&mut proc.fds, fd_num, buf) {
                Some(n) => {
                    let pipe_id = match proc.fds.get(fd_num) {
                        Some(fd::Descriptor::PipeRead(id)) | Some(fd::Descriptor::TtyRead(id)) => Some(*id),
                        _ => None,
                    };
                    Ok((n, pipe_id))
                }
                None => match proc.fds.get(fd_num) {
                    Some(fd::Descriptor::Keyboard) => Err(Some(process::ProcessState::BlockedKeyboard)),
                    Some(fd::Descriptor::PipeRead(id)) | Some(fd::Descriptor::TtyRead(id)) =>
                        Err(Some(process::ProcessState::BlockedPipeRead(*id))),
                    Some(fd::Descriptor::SerialConsole) => {
                        // Poll serial every 10ms
                        let deadline = crate::clock::nanos_since_boot() + 10_000_000;
                        let mut poll_fds = [0u64; 64];
                        poll_fds[0] = fd_num;
                        Err(Some(process::ProcessState::BlockedPoll { fds: poll_fds, len: 1, deadline }))
                    }
                    _ => Err(None),
                },
            }
        });
        match action {
            Ok((n, pipe_id)) => {
                if let Some(id) = pipe_id { process::wake_pipe_writers(id); }
                return n;
            }
            Err(Some(reason)) => process::block(reason),
            Err(None) => return u64::MAX,
        }
    }
}

fn sys_open(path: &str, flags: u64) -> u64 {
    let cwd = process::with_current(|p| p.cwd.clone());
    let resolved = vfs::lock().resolve_absolute(&cwd, path);
    process::with_current_mut(|proc| fd::open(&mut proc.fds, &mut *vfs::lock(), &resolved, flags))
}

fn sys_close(fd_num: u64) -> u64 {
    let pid = process::current_pid();
    // Single lock: determine wake target + close
    let (result, wake) = process::with_current_mut(|proc| {
        let wake = match proc.fds.get(fd_num) {
            Some(fd::Descriptor::PipeWrite(id)) | Some(fd::Descriptor::TtyWrite(id)) => Some((true, *id)),
            Some(fd::Descriptor::PipeRead(id)) | Some(fd::Descriptor::TtyRead(id)) => Some((false, *id)),
            _ => None,
        };
        let r = fd::close(&mut proc.fds, &mut *vfs::lock(), fd_num, pid);
        (r, wake)
    });
    if let Some((is_write, pipe_id)) = wake {
        if is_write { process::wake_pipe_readers(pipe_id); }
        else { process::wake_pipe_writers(pipe_id); }
    }
    result
}

fn sys_exit(code: i32) -> u64 {
    process::exit(code);
}

fn sys_random(buf: &mut [u8]) -> u64 {
    let mut i = 0;
    while i + 8 <= buf.len() {
        buf[i..i + 8].copy_from_slice(&cpu::rdrand().to_ne_bytes());
        i += 8;
    }
    let remaining = buf.len() - i;
    if remaining > 0 {
        let bytes = cpu::rdrand().to_ne_bytes();
        buf[i..].copy_from_slice(&bytes[..remaining]);
    }
    0
}

fn sys_readdir(path: &str, buf: &mut [u8]) -> u64 {
    let cwd = process::with_current(|p| p.cwd.clone());
    let entries = match vfs::lock().list(&cwd, path) {
        Ok(e) => e,
        Err(_) => return u64::MAX,
    };

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

fn sys_delete(path: &str) -> u64 {
    let cwd = process::with_current(|p| p.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    if vfs.delete(&resolved) { 0 } else { u64::MAX }
}

fn sys_chdir(path: &str) -> u64 {
    let cwd = process::with_current(|p| p.cwd.clone());
    match vfs::lock().cd(&cwd, path) {
        Some(new_cwd) => {
            process::with_current_mut(|p| p.cwd = new_cwd);
            0
        }
        None => u64::MAX,
    }
}

fn sys_getcwd(buf: &mut [u8]) -> u64 {
    process::with_current(|proc| {
        let cwd = &proc.cwd;
        let len = cwd.len().min(buf.len());
        buf[..len].copy_from_slice(&cwd.as_bytes()[..len]);
        len as u64
    })
}

fn sys_set_keyboard_layout(name: &str) -> u64 {
    if keyboard::set_layout(name) {
        if !vfs::lock().write_file("/nvme/config/keyboard_layout", name.as_bytes(), crate::clock::nanos_since_boot()) {
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

fn sys_spawn(text: &str, fds: fd::FdTable) -> u64 {
    let args: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
    process::spawn(&args, fds, Some(crate::arch::percpu::current_pid())).map_or(u64::MAX, |p| p as u64)
}

fn sys_waitpid(pid: u64) -> u64 {
    let child_pid = pid as u32;
    let caller = crate::arch::percpu::current_pid();

    // Validate that the target is actually our child process
    let valid = {
        let guard = process::PROCESS_TABLE.lock();
        let table = guard.as_ref().expect("process table not initialized");
        table.procs.get(child_pid).map_or(false, |p| {
            matches!(p.kind, process::Kind::Process { parent: Some(ppid) } if ppid == caller)
        })
    };
    if !valid {
        return u64::MAX;
    }

    loop {
        if let Some(code) = process::collect_zombie(child_pid) {
            return code as u64;
        }
        process::block(process::ProcessState::BlockedWaitPid(child_pid));
    }
}

fn sys_poll(fds: &[u64], fds_len: u64, timeout_nanos: u64) -> u64 {
    let deadline = if timeout_nanos > 0 {
        crate::clock::nanos_since_boot() + timeout_nanos
    } else {
        0
    };
    loop {
        crate::drivers::xhci::poll_if_pending();
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
        if deadline > 0 && crate::clock::nanos_since_boot() >= deadline {
            return 0;
        }
        let mut poll_fds = [0u64; 64];
        let copy_len = (fds_len as usize).min(64);
        poll_fds[..copy_len].copy_from_slice(&fds[..copy_len]);
        process::block(process::ProcessState::BlockedPoll { fds: poll_fds, len: copy_len as u32, deadline });
    }
}

fn sys_send_msg(target_pid: u64, user_msg: &message::UserMessage, payload: Vec<u8>) -> u64 {
    let msg = message::Message {
        sender: process::current_pid(),
        msg_type: user_msg.msg_type,
        payload,
    };
    if process::send_message(target_pid as u32, msg) { 0 } else { u64::MAX }
}

fn sys_recv_msg(out: &mut message::UserMessage) -> u64 {
    loop {
        let msg = process::with_current_mut(|proc| proc.messages.pop());
        if let Some(msg) = msg {
            let (data, len) = if !msg.payload.is_empty() {
                // Allocate in receiver's user heap and copy payload
                let addr = with_heap_owner(|heap| {
                    user_heap::alloc(heap, msg.payload.len(), 8)
                }).unwrap_or(0);
                if addr == 0 {
                    return u64::MAX;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        msg.payload.as_ptr(),
                        addr as *mut u8,
                        msg.payload.len(),
                    );
                }
                (addr, msg.payload.len() as u64)
            } else {
                (0, 0)
            };

            out.sender = msg.sender;
            out.msg_type = msg.msg_type;
            out.data = data;
            out.len = len;
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

fn sys_register_name(name: &str) -> u64 {
    let pid = process::current_pid();
    if process::register_name(name, pid) { 0 } else { u64::MAX }
}

fn sys_find_pid(name: &str) -> u64 {
    match process::find_pid(name) {
        Some(pid) => pid as u64,
        None => u64::MAX,
    }
}

fn sys_alloc_shared(size: u64) -> u64 {
    let pid = process::current_pid();
    let pml4 = process::with_current(|p| p.cr3 as *mut u64);
    let (token, _addr) = shared_memory::alloc(size, pid, pml4);
    token as u64
}

fn sys_grant_shared(token: u64, target_pid: u64) -> u64 {
    let pid = process::current_pid();
    if shared_memory::grant(token as u32, pid, target_pid as u32) { 0 } else { u64::MAX }
}

fn sys_map_shared(token: u64) -> u64 {
    let pid = process::current_pid();
    let pml4 = process::with_current(|p| p.cr3 as *mut u64);
    match shared_memory::map(token as u32, pid, pml4) {
        Some(addr) => addr,
        None => u64::MAX,
    }
}

fn sys_release_shared(token: u64) -> u64 {
    let pid = process::current_pid();
    let pml4 = process::with_current(|p| p.cr3 as *mut u64);
    if shared_memory::release(token as u32, pid, pml4) { 0 } else { u64::MAX }
}

fn sys_thread_join(tid: u64) -> u64 {
    let tid = tid as u32;
    let caller = crate::arch::percpu::current_pid();

    // Validate that the target is actually our child thread
    let valid = {
        let guard = process::PROCESS_TABLE.lock();
        let table = guard.as_ref().expect("process table not initialized");
        table.procs.get(tid).map_or(false, |p| {
            matches!(p.kind, process::Kind::Thread { parent } if parent == caller)
        })
    };
    if !valid {
        return u64::MAX;
    }

    loop {
        if let Some(_code) = process::collect_zombie(tid) {
            return 0;
        }
        process::block(process::ProcessState::BlockedThreadJoin(tid));
    }
}

fn sys_sysinfo(buf: &mut [u8]) -> u64 {
    const HEADER_SIZE: usize = 48;
    const ENTRY_SIZE: usize = 48;
    if buf.len() < HEADER_SIZE {
        return u64::MAX;
    }

    let (total_mem, used_mem) = allocator::memory_stats();
    let cpu_count = super::smp::cpu_count();
    let uptime = crate::clock::nanos_since_boot();
    let (busy_ticks, total_ticks) = super::idt::cpu_ticks();

    let guard = process::PROCESS_TABLE.lock();
    let table = guard.as_ref().expect("process table not initialized");

    let process_count = table.procs.iter().count() as u32;

    // Write header
    buf[0..8].copy_from_slice(&total_mem.to_le_bytes());
    buf[8..16].copy_from_slice(&used_mem.to_le_bytes());
    buf[16..20].copy_from_slice(&cpu_count.to_le_bytes());
    buf[20..24].copy_from_slice(&process_count.to_le_bytes());
    buf[24..32].copy_from_slice(&uptime.to_le_bytes());
    buf[32..40].copy_from_slice(&busy_ticks.to_le_bytes());
    buf[40..48].copy_from_slice(&total_ticks.to_le_bytes());

    // Write process entries
    let max_entries = (buf.len() - HEADER_SIZE) / ENTRY_SIZE;
    let mut sorted_pids: Vec<u32> = table.procs.iter().map(|(pid, _)| pid).collect();
    sorted_pids.sort();

    let mut pos = HEADER_SIZE;
    for (i, &pid) in sorted_pids.iter().enumerate() {
        if i >= max_entries {
            break;
        }
        let proc = table.procs.get(pid).unwrap();

        let state: u8 = match proc.state {
            process::ProcessState::Running => 0,
            process::ProcessState::Ready => 1,
            process::ProcessState::Zombie(_) => 3,
            _ => 2, // all Blocked variants
        };
        let (is_thread, parent_pid) = match proc.kind {
            process::Kind::Thread { parent } => (1u8, parent),
            process::Kind::Process { parent } => (0u8, parent.unwrap_or(u32::MAX)),
        };
        let memory = (proc.elf_alloc.as_ref().map_or(0, |a| a.size())
            + proc.stack_alloc.as_ref().map_or(0, |a| a.size())) as u64;

        buf[pos..pos + 4].copy_from_slice(&pid.to_le_bytes());
        buf[pos + 4..pos + 8].copy_from_slice(&parent_pid.to_le_bytes());
        buf[pos + 8] = state;
        buf[pos + 9] = is_thread;
        buf[pos + 10..pos + 12].copy_from_slice(&[0, 0]); // padding
        buf[pos + 12..pos + 20].copy_from_slice(&memory.to_le_bytes());
        buf[pos + 20..pos + 48].copy_from_slice(&proc.name);

        pos += ENTRY_SIZE;
    }

    pos as u64
}

fn sys_net_info(buf: &mut [u8]) -> u64 {
    let Some(mac) = crate::net::mac() else { return u64::MAX };
    if buf.len() < 6 { return u64::MAX; }
    buf[..6].copy_from_slice(&mac);
    0
}

fn sys_net_recv(buf: &mut [u8], timeout_nanos: u64) -> u64 {
    let deadline = if timeout_nanos > 0 {
        crate::clock::nanos_since_boot() + timeout_nanos
    } else {
        0
    };
    loop {
        if let Some(n) = crate::net::recv(buf) {
            return n as u64;
        }
        if deadline > 0 && crate::clock::nanos_since_boot() >= deadline {
            return 0;
        }
        process::block(process::ProcessState::BlockedNetRecv { deadline });
    }
}

fn sys_nanosleep(nanos: u64) -> u64 {
    let deadline = crate::clock::nanos_since_boot() + nanos;
    process::block(process::ProcessState::BlockedSleep { deadline });
    0
}

fn sys_dup(fd_num: u64) -> u64 {
    process::with_current_mut(|proc| {
        let desc = match proc.fds.get(fd_num) {
            Some(d) => fd::dup(d),
            None => return u64::MAX,
        };
        fd::alloc(&mut proc.fds, desc)
    })
}

fn sys_rename(old: &str, new: &str) -> u64 {
    let cwd = process::with_current(|p| p.cwd.clone());
    let mut vfs = vfs::lock();
    let old_abs = vfs.resolve_absolute(&cwd, old);
    let new_abs = vfs.resolve_absolute(&cwd, new);
    if vfs.rename(&old_abs, &new_abs) { 0 } else { u64::MAX }
}

fn sys_mkdir(path: &str) -> u64 {
    let cwd = process::with_current(|p| p.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    vfs.create_dir(&resolved);
    0
}

fn sys_rmdir(path: &str) -> u64 {
    let cwd = process::with_current(|p| p.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    vfs.remove_dir(&resolved);
    0
}

fn sys_dlopen(path: &str) -> u64 {
    let cwd = process::with_current(|p| p.cwd.clone());
    let resolved = vfs::lock().resolve_absolute(&cwd, path);

    let data = match vfs::lock().read_file(&resolved) {
        Ok(d) => d,
        Err(e) => {
            log!("dlopen: {}: {}", resolved, e);
            return u64::MAX;
        }
    };

    let lib = match crate::elf::load_shared_lib(&data) {
        Ok(l) => l,
        Err(msg) => {
            log!("dlopen: {}", msg);
            return u64::MAX;
        }
    };

    // Map loaded library memory into the current process's address space
    paging::map_user(lib.alloc.ptr() as u64, lib.alloc.size() as u64);

    // Resolve GLOB_DAT/JUMP_SLOT and TLS relocations, then store the lib.
    // Threads have empty loaded_libs — use the parent process's libs instead.
    let lib_has_tls = lib.tls_memsz > 0;
    let lib_tls_memsz = lib.tls_memsz;

    // Phase 1: resolve relocations (needs process table lock)
    let (new_total, modules) = {
        let mut guard = crate::process::PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table");
        let current_pid = crate::process::current_pid();
        let proc = table.procs.get(current_pid).expect("current process");
        let owner_pid = match proc.kind {
            crate::process::Kind::Thread { parent } => parent,
            _ => current_pid,
        };
        let owner = table.procs.get_mut(owner_pid).expect("owner process");
        log!("dlopen: pid={} owner={} resolving against {} existing libs",
            current_pid, owner_pid, owner.loaded_libs.len());
        crate::elf::resolve_dlopen_relocs(&lib, &owner.loaded_libs);

        // If the new lib has its own TLS, insert at offset 0 and shift existing
        // modules up. This preserves all existing TPOFF values since:
        // TPOFF = (old_base + shift) + sym_offset - (old_total + shift) = old TPOFF
        let lib_base_offset = if lib_has_tls {
            let new_memsz = lib.tls_memsz;
            // Shift all existing modules up by new_memsz
            for module in owner.tls_modules.iter_mut() {
                module.3 += new_memsz;
            }
            // Insert new module at offset 0
            owner.tls_modules.insert(0, (lib.tls_template, lib.tls_filesz, lib.tls_memsz, 0));
            owner.tls_total_memsz += new_memsz;
            log!("dlopen: new TLS module at offset 0, memsz={}, total_memsz={}",
                new_memsz, owner.tls_total_memsz);
            0
        } else {
            0
        };

        // Apply TPOFF relocations for cross-library and own TLS references
        if owner.tls_total_memsz > 0 {
            let tls_info = crate::elf::TlsModuleInfo {
                libs: &owner.loaded_libs,
                modules: &owner.tls_modules,
            };
            crate::elf::apply_tpoff_relocs(&lib, lib_base_offset, owner.tls_total_memsz, &tls_info);
        }

        (owner.tls_total_memsz, owner.tls_modules.clone())
    }; // guard dropped here

    // Phase 2: reallocate TLS if the new lib added a TLS module (no lock held)
    // New module is at offset 0, existing modules shifted up by lib_tls_memsz.
    // Copy existing TLS data to the shifted position in the new block.
    if lib_has_tls {
        if let Some((new_alloc, new_tp)) = crate::process::setup_combined_tls(&modules, new_total) {
            crate::arch::paging::map_user(new_alloc.ptr() as u64, new_alloc.size() as u64);
            // Copy existing TLS data shifted up by lib_tls_memsz
            let old_tp = crate::arch::read_fs_base();
            let old_total = new_total - lib_tls_memsz;
            if old_total > 0 {
                unsafe {
                    // Old data: [old_tp - old_total .. old_tp)
                    // New position: shifted up by lib_tls_memsz within new block
                    // New block: [new_tp - new_total .. new_tp)
                    // New module at [new_tp - new_total .. new_tp - new_total + lib_tls_memsz)
                    // Existing data at [new_tp - new_total + lib_tls_memsz .. new_tp)
                    core::ptr::copy_nonoverlapping(
                        (old_tp - old_total as u64) as *const u8,
                        (new_tp - new_total as u64 + lib_tls_memsz as u64) as *mut u8,
                        old_total,
                    );
                }
            }
            // Set new FS base
            crate::arch::set_fs_base(new_tp);
            // Store new TLS alloc, free old one
            crate::process::with_current_mut(|p| {
                p.tls_alloc = Some(new_alloc);
                p.tls_total_memsz = new_total;
                p.tls_modules = modules;
            });
        }
    }

    // Phase 3: store the lib in the owner process
    {
        let mut guard = crate::process::PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table");
        let current_pid = crate::process::current_pid();
        let proc = table.procs.get(current_pid).expect("current process");
        let owner_pid = match proc.kind {
            crate::process::Kind::Thread { parent } => parent,
            _ => current_pid,
        };
        let owner = table.procs.get_mut(owner_pid).expect("owner process");
        let idx = owner.loaded_libs.len();
        owner.loaded_libs.push(lib);
        idx as u64
    }
}

fn sys_dlsym(handle: u64, name: &str) -> u64 {
    let mut guard = crate::process::PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table");
    let current_pid = crate::process::current_pid();
    let proc = table.procs.get(current_pid).expect("current process");
    let owner_pid = match proc.kind {
        crate::process::Kind::Thread { parent } => parent,
        _ => current_pid,
    };
    let owner = table.procs.get(owner_pid).expect("owner process");
    let idx = handle as usize;
    if idx >= owner.loaded_libs.len() {
        return u64::MAX;
    }
    let addr = crate::elf::dlsym(&owner.loaded_libs[idx], name);
    if addr == 0 { u64::MAX } else { addr }
}

/// Terminate the current userspace process (called from exception handlers).
pub fn kill_process(code: i32) -> ! {
    process::exit(code);
}
