use core::arch::naked_asm;

use alloc::vec::Vec;
use super::{apic, cpu, gdt};
use crate::drivers::acpi;
use crate::sync::Lock;
use crate::user_ptr::SyscallContext;
use crate::{device, fd, keyboard, listener, log, pipe, process, scheduler, shared_memory, vfs};
use crate::{DirectMap, UserAddr};

// MSR addresses
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;

use toyos_abi::syscall::*;

// ---------------------------------------------------------------------------
// Syscall entry point
// ---------------------------------------------------------------------------

pub fn init() {
    let efer = cpu::rdmsr(MSR_EFER);
    cpu::wrmsr(MSR_EFER, efer | 1);

    let star = (0x10u64 << 48) | ((gdt::KERNEL_CS as u64) << 32);
    cpu::wrmsr(MSR_STAR, star);
    cpu::wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
    cpu::wrmsr(MSR_FMASK, 0x40200); // mask IF (bit 9) + AC (bit 18) on SYSCALL entry
}

// Syscall entry: GS permanently points to kernel per-CPU data (no swapgs needed).
// PerCpu layout: offset 16 = kernel_rsp, offset 24 = user_rsp.
// Saves/restores XMM registers because blocking syscalls context-switch,
// and kernel Rust code is free to clobber caller-saved XMM registers.
#[unsafe(naked)]
extern "sysv64" fn syscall_entry() {
    naked_asm!(
        "mov gs:[24], rsp",     // save user RSP to percpu.user_rsp
        "mov gs:[216], rcx",    // save user RIP to percpu.syscall_rip
        "mov gs:[224], rdi",    // save syscall number to percpu.syscall_num
        "mov gs:[232], rbp",    // save user RBP to percpu.syscall_rbp
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

        // Save SSE state — kernel code may clobber XMM registers,
        // and blocking syscalls context-switch away.
        "sub rsp, 8",
        "stmxcsr [rsp]",
        "sub rsp, 256",
        "movdqu [rsp + 0*16], xmm0",
        "movdqu [rsp + 1*16], xmm1",
        "movdqu [rsp + 2*16], xmm2",
        "movdqu [rsp + 3*16], xmm3",
        "movdqu [rsp + 4*16], xmm4",
        "movdqu [rsp + 5*16], xmm5",
        "movdqu [rsp + 6*16], xmm6",
        "movdqu [rsp + 7*16], xmm7",
        "movdqu [rsp + 8*16], xmm8",
        "movdqu [rsp + 9*16], xmm9",
        "movdqu [rsp + 10*16], xmm10",
        "movdqu [rsp + 11*16], xmm11",
        "movdqu [rsp + 12*16], xmm12",
        "movdqu [rsp + 13*16], xmm13",
        "movdqu [rsp + 14*16], xmm14",
        "movdqu [rsp + 15*16], xmm15",

        "call {handler}",

        // Restore SSE state
        "movdqu xmm0,  [rsp + 0*16]",
        "movdqu xmm1,  [rsp + 1*16]",
        "movdqu xmm2,  [rsp + 2*16]",
        "movdqu xmm3,  [rsp + 3*16]",
        "movdqu xmm4,  [rsp + 4*16]",
        "movdqu xmm5,  [rsp + 5*16]",
        "movdqu xmm6,  [rsp + 6*16]",
        "movdqu xmm7,  [rsp + 7*16]",
        "movdqu xmm8,  [rsp + 8*16]",
        "movdqu xmm9,  [rsp + 9*16]",
        "movdqu xmm10, [rsp + 10*16]",
        "movdqu xmm11, [rsp + 11*16]",
        "movdqu xmm12, [rsp + 12*16]",
        "movdqu xmm13, [rsp + 13*16]",
        "movdqu xmm14, [rsp + 14*16]",
        "movdqu xmm15, [rsp + 15*16]",
        "add rsp, 256",
        "ldmxcsr [rsp]",
        "add rsp, 8",

        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r11",
        "pop rcx",
        // CLI before pop rsp / sysretq — an interrupt after pop rsp would
        // use the user RSP as the kernel stack.
        "cli",
        "pop rsp",              // restore user RSP from kernel stack
        "sysretq",
        handler = sym syscall_handler,
    );
}

extern "sysv64" fn syscall_handler(num: u64, a1: u64, a2: u64, _: u64, a3: u64, a4: u64) -> u64 {
    syscall_dispatch(num, a1, a2, a3, a4)
}

fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let t0 = crate::clock::nanos_since_boot();

    // Count syscalls per process
    process::with_current_data(|data| {
        data.syscall_total += 1;
        if (num as usize) < data.syscall_counts.len() {
            data.syscall_counts[num as usize] += 1;
        }
    });

    // SAFETY: current process's page tables remain active for the duration of this call.
    let ctx = unsafe { SyscallContext::new() };

    let bad_addr = SyscallError::BadAddress.to_u64();

    let result = match num {
        SYS_WRITE => {
            let Some(buf) = ctx.user_slice(UserAddr::new(a2), a3) else { return bad_addr };
            sys_write(a1 as u32, buf)
        }
        SYS_READ => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a2), a3) else { return bad_addr };
            sys_read(a1 as u32, buf)
        }
        SYS_THREAD_EXIT => sys_thread_exit(a1 as i32),
        SYS_RANDOM => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_random(buf)
        }
        SYS_SCREEN_SIZE => screen_size(),
        SYS_CLOCK => crate::clock::nanos_since_boot(),
        SYS_OPEN => {
            let Some(path) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_open(path, OpenFlags(a3))
        }
        SYS_CLOSE => sys_close(a1 as u32),
        SYS_SEEK => {
            let pos = match a3 {
                0 => SeekFrom::Start(a2),
                1 => SeekFrom::Current(a2 as i64),
                2 => SeekFrom::End(a2 as i64),
                _ => return SyscallError::InvalidArgument.to_u64(),
            };
            process::with_fd_owner_data(|data| fd::seek(&mut data.fds, a1 as u32, pos))
        }
        SYS_FSTAT => {
            let Some(stat) = ctx.user_mut::<fd::Stat>(UserAddr::new(a2)) else { return bad_addr };
            if process::with_fd_owner_data(|data| fd::fstat(&data.fds, a1 as u32, stat)) { 0 } else { SyscallError::NotFound.to_u64() }
        }
        SYS_FSYNC => process::with_fd_owner_data(|data| fd::fsync(&mut data.fds, &mut *vfs::lock(), a1 as u32)),
        SYS_READDIR => {
            let Some(path) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a3), a4) else { return bad_addr };
            sys_readdir(path, buf)
        }
        SYS_DELETE => {
            let Some(path) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_delete(path)
        }
        SYS_SHUTDOWN => {
            log!("Syncing filesystems...");
            crate::vfs::lock().sync_all();
            log!("Shutting down.");
            acpi::shutdown();
        }
        SYS_CHDIR => {
            let Some(path) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_chdir(path)
        }
        SYS_GETCWD => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_getcwd(buf)
        }
        SYS_SET_KEYBOARD_LAYOUT => {
            let Some(name) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_set_keyboard_layout(name)
        }
        SYS_PIPE => sys_pipe(),
        SYS_SPAWN => {
            let Some(args) = ctx.user_ref::<SpawnArgs>(UserAddr::new(a1)) else { return bad_addr };
            let Some(text) = ctx.user_str(UserAddr::new(args.argv_ptr), args.argv_len) else { return bad_addr };
            let fd_count = args.fd_map_count as usize;
            let fds = if fd_count > 0 {
                let Some(pairs) = ctx.user_slice_of::<[u32; 2]>(UserAddr::new(args.fd_map_ptr), fd_count) else { return bad_addr };
                process::build_child_fds(pairs)
            } else {
                fd::FdTable::new()
            };
            let env = if args.env_len > 0 {
                let Some(env_bytes) = ctx.user_slice(UserAddr::new(args.env_ptr), args.env_len) else { return bad_addr };
                env_bytes.to_vec()
            } else {
                alloc::vec::Vec::new()
            };
            sys_spawn(text, fds, env)
        }
        SYS_WAITPID => sys_waitpid(a1, a2),

        SYS_MARK_TTY => process::with_fd_owner_data(|data| fd::mark_tty(&mut data.fds, a1 as u32)),
        29 | 30 => SyscallError::NotSupported.to_u64(), // formerly SYS_SEND_MSG/SYS_RECV_MSG
        SYS_OPEN_DEVICE => sys_open_device(a1),
        32 | 33 => SyscallError::NotSupported.to_u64(), // formerly SYS_REGISTER_NAME/SYS_FIND_PID
        SYS_SET_SCREEN_SIZE => { set_screen_size(a1 as u32, a2 as u32); 0 }
        SYS_GPU_PRESENT => { crate::gpu::present_rect(a1 as u32, a2 as u32, a3 as u32, a4 as u32); 0 }
        SYS_GPU_SET_CURSOR => { crate::gpu::set_cursor(a1 as u32, a2 as u32); 0 }
        SYS_GPU_MOVE_CURSOR => { crate::gpu::move_cursor(a1 as u32, a2 as u32); 0 }
        SYS_ALLOC_SHARED => sys_alloc_shared(a1),
        SYS_GRANT_SHARED => sys_grant_shared(a1, a2),
        SYS_MAP_SHARED => sys_map_shared(a1),
        SYS_RELEASE_SHARED => sys_release_shared(a1),
        SYS_THREAD_SPAWN => process::spawn_thread(a1, a2, a3, a4).map_or(SyscallError::Unknown.to_u64(), |t| t.raw() as u64),
        SYS_THREAD_JOIN => sys_thread_join(a1),
        SYS_CLOCK_REALTIME => crate::rtc::read_time(),
        SYS_CLOCK_EPOCH => crate::rtc::read_epoch_secs(),
        SYS_SYSINFO => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_sysinfo(buf)
        }
        SYS_NET_INFO => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_net_info(buf)
        }
        SYS_NET_SEND => {
            let Some(buf) = ctx.user_slice(UserAddr::new(a1), a2) else { return bad_addr };
            crate::net::send(buf);
            0
        }
        SYS_NET_RECV => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_net_recv(buf, a3)
        }
        SYS_NANOSLEEP => sys_nanosleep(a1),
        SYS_DUP => sys_dup(a1 as u32),
        SYS_DUP2 => sys_dup2(a1 as u32, a2 as u32),
        SYS_GETPID => process::current_process().raw() as u64,
        SYS_RENAME => {
            let Some(old) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            let Some(new) = ctx.user_str(UserAddr::new(a3), a4) else { return bad_addr };
            sys_rename(old, new)
        }
        SYS_MKDIR => {
            let Some(path) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_mkdir(path)
        }
        SYS_RMDIR => {
            let Some(path) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_rmdir(path)
        }
        SYS_DLOPEN => {
            let Some(path) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_dlopen(path, a3)
        }
        SYS_DLSYM => {
            let Some(name) = ctx.user_str(UserAddr::new(a2), a3) else { return bad_addr };
            sys_dlsym(a1, name)
        }
        SYS_DLCLOSE => 0,
        SYS_FTRUNCATE => process::with_fd_owner_data(|data| fd::ftruncate(&mut data.fds, a1 as u32, a2)),
        SYS_STACK_INFO => {
            let Some(base_out) = ctx.user_mut::<u64>(UserAddr::new(a1)) else { return bad_addr };
            let Some(size_out) = ctx.user_mut::<u64>(UserAddr::new(a2)) else { return bad_addr };
            process::with_current_data(|data| {
                if data.user_stack_base.raw() > 0 {
                    *base_out = data.user_stack_base.raw();
                    *size_out = data.user_stack_size;
                    0
                } else {
                    SyscallError::NotFound.to_u64()
                }
            })
        }
        SYS_CPU_COUNT => super::smp::cpu_count() as u64,
        SYS_FUTEX_WAIT => {
            if ctx.user_ref::<u32>(UserAddr::new(a1)).is_none() { return bad_addr; }
            process::futex_wait(a1, a2 as u32, a3)
        }
        SYS_FUTEX_WAKE => {
            if ctx.user_ref::<u32>(UserAddr::new(a1)).is_none() { return bad_addr; }
            process::futex_wake(a1, a2)
        }
        SYS_MMAP => sys_mmap(a1, a2, a3, a4),
        SYS_MUNMAP => sys_munmap(a1, a2),
        SYS_KILL => process::kill_process(process::Pid::from_raw(a1 as u32)),
        SYS_READ_NONBLOCK => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a2), a3) else { return bad_addr };
            sys_read_nonblock(a1 as u32, buf)
        }
        SYS_WRITE_NONBLOCK => {
            let Some(buf) = ctx.user_slice(UserAddr::new(a2), a3) else { return bad_addr };
            sys_write_nonblock(a1 as u32, buf)
        }
        SYS_PIPE_OPEN => sys_pipe_open(a1, a2),
        SYS_PIPE_ID => sys_pipe_id(a1 as u32),
        SYS_AUDIO_SUBMIT => {
            if crate::audio::submit_buffer(a1 as usize, a2 as u32) { 0 } else { SyscallError::InvalidArgument.to_u64() }
        }
        SYS_AUDIO_POLL => {
            crate::audio::poll_completed() as u64
        }
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_GET_ENV => {
            let env = process::with_fd_owner_data(|d| d.env.clone());
            if a2 == 0 {
                env.len() as u64
            } else {
                let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
                let copy_len = env.len().min(buf.len());
                buf[..copy_len].copy_from_slice(&env[..copy_len]);
                copy_len as u64
            }
        }
        SYS_SOCKET_CREATE => sys_socket_create(a1, a2),
        SYS_PIPE_MAP => sys_pipe_map(a1 as u32),
        SYS_NIC_RX_POLL => {
            match crate::net::poll_rx() {
                Some((buf_idx, frame_len)) => ((buf_idx as u64) << 16) | (frame_len as u64),
                None => 0,
            }
        }
        SYS_NIC_RX_DONE => { crate::net::refill_rx_buf(a1 as usize); 0 }
        SYS_NIC_TX => { crate::net::submit_tx(a1 as usize); 0 }
        SYS_SYMLINK => {
            let Some(target) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            let Some(link) = ctx.user_str(UserAddr::new(a3), a4) else { return bad_addr };
            sys_symlink(target, link)
        }
        SYS_READLINK => {
            let Some(path) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a3), a4) else { return bad_addr };
            sys_readlink(path, buf)
        }
        SYS_GPU_SET_RESOLUTION => {
            let info_size = core::mem::size_of::<fd::FramebufferInfo>() as u64;
            let Some(out_buf) = ctx.user_slice_mut(UserAddr::new(a3), info_size) else { return bad_addr };
            match crate::gpu::set_resolution(a1 as u32, a2 as u32) {
                Ok(gpu_info) => {
                    let fb_info = fd::FramebufferInfo {
                        token: [gpu_info.tokens[0].raw(), gpu_info.tokens[1].raw()],
                        cursor_token: gpu_info.cursor_token.raw(),
                        width: gpu_info.width,
                        height: gpu_info.height,
                        stride: gpu_info.stride,
                        pixel_format: gpu_info.pixel_format,
                        flags: gpu_info.flags,
                    };
                    device::set_framebuffer_info(fb_info);
                    set_screen_size(gpu_info.width, gpu_info.height);
                    let pid = process::current_process();
                    for &token in &gpu_info.tokens {
                        if shared_memory::grant_kernel(token, pid).is_err() {
                            return SyscallError::Unknown.to_u64();
                        }
                    }
                    if shared_memory::grant_kernel(gpu_info.cursor_token, pid).is_err() {
                        return SyscallError::Unknown.to_u64();
                    }
                    out_buf.copy_from_slice(fb_info.as_bytes());
                    0
                }
                Err(()) => SyscallError::NotSupported.to_u64(),
            }
        }
        SYS_LISTEN => {
            let Some(name) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_listen(name)
        }
        SYS_ACCEPT => sys_accept(a1 as u32),
        SYS_CONNECT => {
            let Some(name) = ctx.user_str(UserAddr::new(a1), a2) else { return bad_addr };
            sys_connect(name)
        }
        SYS_TLS_ALLOC_BLOCK => sys_tls_alloc_block(a1),
        SYS_IO_URING_SETUP => sys_io_uring_setup(a1 as u32),
        SYS_IO_URING_ENTER => sys_io_uring_enter(a1 as u32, a2 as u32, a3 as u32, a4),
        SYS_QUERY_MODULES => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_query_modules(buf)
        }
        SYS_DEBUG => match a1 {
            0 => panic!("SYS_DEBUG: kernel panic triggered by userspace"),
            1 => { unsafe { core::ptr::read_volatile(core::ptr::null::<u64>()); } 0 }
            _ => SyscallError::InvalidArgument.to_u64(),
        },
        SYS_SCHED_INFO => {
            let info_size = core::mem::size_of::<toyos_abi::syscall::SchedInfo>() as u64;
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), info_size) else {
                return bad_addr;
            };
            let out = unsafe { &mut *(buf.as_mut_ptr() as *mut toyos_abi::syscall::SchedInfo) };
            sys_sched_info(out)
        },
        _ => SyscallError::InvalidArgument.to_u64(),
    };

    // Track wall-clock syscall time (includes preemption — see plan for documented limitation)
    let elapsed = crate::clock::nanos_since_boot() - t0;
    process::with_current_data(|data| {
        data.syscall_total_ns += elapsed;
    });

    result
}

// ---------------------------------------------------------------------------
// Syscall implementations
// ---------------------------------------------------------------------------

fn sys_write(fd_num: u32, buf: &[u8]) -> u64 {
    loop {
        let action = process::with_fd_owner_data(|data| {
            match fd::try_write(&mut data.fds, fd_num, buf) {
                Some(n) => {
                    let pipe_id = data.fds.get(fd_num).and_then(|d| d.pipe_id_write());
                    Ok((n, pipe_id))
                }
                None => {
                    let block = data.fds.get(fd_num).and_then(|d| d.pipe_id_write())
                        .map(|id| crate::scheduler::EventSource::PipeWritable(id));
                    Err(block)
                }
            }
        });
        match action {
            Ok((n, pipe_id)) => {
                if let Some(id) = pipe_id { process::wake_pipe_readers(id); }
                return n;
            }
            Err(Some(event)) => process::block(Some(event), 0),
            Err(None) => return SyscallError::NotFound.to_u64(),
        }
    }
}

enum ReadBlock {
    Event(crate::scheduler::EventSource),
    EventWithDeadline(crate::scheduler::EventSource, u64),
}

fn sys_read(fd_num: u32, buf: &mut [u8]) -> u64 {
    loop {
        let action = process::with_fd_owner_data(|data| {
            match fd::try_read(&mut data.fds, fd_num, buf) {
                Some(n) => {
                    let pipe_id = data.fds.get(fd_num).and_then(|d| d.pipe_id_read());
                    Ok((n, pipe_id))
                }
                None => {
                    let desc = data.fds.get(fd_num);
                    if matches!(desc, Some(fd::Descriptor::Keyboard)) {
                        Err(Some(ReadBlock::Event(crate::scheduler::EventSource::Keyboard)))
                    } else if let Some(id) = desc.and_then(|d| d.pipe_id_read()) {
                        Err(Some(ReadBlock::Event(crate::scheduler::EventSource::PipeReadable(id))))
                    } else if matches!(desc, Some(fd::Descriptor::SerialConsole)) {
                        let deadline = crate::clock::nanos_since_boot() + 10_000_000;
                        Err(Some(ReadBlock::EventWithDeadline(crate::scheduler::EventSource::Keyboard, deadline)))
                    } else {
                        Err(None)
                    }
                }
            }
        });
        match action {
            Ok((n, pipe_id)) => {
                if let Some(id) = pipe_id { process::wake_pipe_writers(id); }
                return n;
            }
            Err(Some(ReadBlock::Event(event))) => process::block(Some(event), 0),
            Err(Some(ReadBlock::EventWithDeadline(event, deadline))) => process::block(Some(event), deadline),
            Err(None) => return SyscallError::NotFound.to_u64(),
        }
    }
}

fn sys_open(path: &str, flags: OpenFlags) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let resolved = vfs::lock().resolve_absolute(&cwd, path);
    process::with_fd_owner_data(|data| fd::open(&mut data.fds, &mut *vfs::lock(), &resolved, flags))
}

fn sys_close(fd_num: u32) -> u64 {
    let pid = process::current_process();
    let (result, wake_readers, wake_writers) = process::with_fd_owner_data(|data| {
        // Grab pipe IDs before close drops the descriptor
        let wake_r = data.fds.get(fd_num).and_then(|d| d.pipe_id_write()); // writer closed → wake readers
        let wake_w = data.fds.get(fd_num).and_then(|d| d.pipe_id_read());  // reader closed → wake writers
        let r = fd::close(&mut data.fds, &mut *vfs::lock(), fd_num, pid);
        (r, wake_r, wake_w)
    });
    if let Some(id) = wake_readers { process::wake_pipe_readers(id); }
    if let Some(id) = wake_writers { process::wake_pipe_writers(id); }
    result
}

fn sys_thread_exit(code: i32) -> u64 {
    process::thread_exit(code);
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
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let entries = match vfs::lock().list(&cwd, path) {
        Ok(e) => e,
        Err(_) => return SyscallError::NotFound.to_u64(),
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
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    if vfs.delete(&resolved) { 0 } else { SyscallError::NotFound.to_u64() }
}

fn sys_chdir(path: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    match vfs::lock().cd(&cwd, path) {
        Some(new_cwd) => {
            process::with_fd_owner_data(|d| d.cwd = new_cwd);
            0
        }
        None => SyscallError::NotFound.to_u64(),
    }
}

fn sys_getcwd(buf: &mut [u8]) -> u64 {
    process::with_fd_owner_data(|data| {
        let cwd = &data.cwd;
        let len = cwd.len().min(buf.len());
        buf[..len].copy_from_slice(&cwd.as_bytes()[..len]);
        len as u64
    })
}

fn sys_set_keyboard_layout(name: &str) -> u64 {
    if keyboard::set_layout(name) {
        0
    } else {
        SyscallError::NotFound.to_u64()
    }
}

fn fd_result(r: Result<u32, SyscallError>) -> u64 {
    match r {
        Ok(fd) => fd as u64,
        Err(e) => e.to_u64(),
    }
}

fn sys_pipe() -> u64 {
    let (reader, writer) = pipe::create();
    process::with_fd_owner_data(|data| {
        let Ok(read_fd) = fd::alloc(&mut data.fds, fd::Descriptor::PipeRead(reader, None)) else {
            return SyscallError::ResourceExhausted.to_u64();
        };
        let Ok(write_fd) = fd::alloc(&mut data.fds, fd::Descriptor::PipeWrite(writer, None)) else {
            fd::close(&mut data.fds, &mut *vfs::lock(), read_fd, process::current_process());
            return SyscallError::ResourceExhausted.to_u64();
        };
        ((read_fd as u64) << 32) | write_fd as u64
    })
}

fn sys_pipe_open(pipe_id: u64, mode: u64) -> u64 {
    let id = pipe::PipeId::from_raw(pipe_id as usize);
    match mode {
        0 => {
            let Some(reader) = pipe::open_reader(id) else { return SyscallError::NotFound.to_u64() };
            process::with_fd_owner_data(|data| fd_result(fd::alloc(&mut data.fds, fd::Descriptor::PipeRead(reader, None))))
        }
        1 => {
            let Some(writer) = pipe::open_writer(id) else { return SyscallError::NotFound.to_u64() };
            process::with_fd_owner_data(|data| fd_result(fd::alloc(&mut data.fds, fd::Descriptor::PipeWrite(writer, None))))
        }
        _ => SyscallError::InvalidArgument.to_u64(),
    }
}

fn sys_pipe_id(fd_num: u32) -> u64 {
    process::with_fd_owner_data(|data| {
        match data.fds.get(fd_num) {
            Some(fd::Descriptor::PipeRead(r, _)) | Some(fd::Descriptor::TtyRead(r)) => r.id().raw() as u64,
            Some(fd::Descriptor::PipeWrite(w, _)) | Some(fd::Descriptor::TtyWrite(w)) => w.id().raw() as u64,
            _ => SyscallError::InvalidArgument.to_u64(),
        }
    })
}

fn sys_pipe_map(fd_num: u32) -> u64 {
    // Get the pipe's physical address and allocate a vaddr, map it, store in FD
    process::with_fd_owner_data(|data| {
        let pipe_id = match data.fds.get(fd_num) {
            Some(fd::Descriptor::PipeRead(r, _)) | Some(fd::Descriptor::TtyRead(r)) => Some(r.id()),
            Some(fd::Descriptor::PipeWrite(w, _)) | Some(fd::Descriptor::TtyWrite(w)) => Some(w.id()),
            Some(fd::Descriptor::Socket { rx, .. }) => Some(rx.id()),
            _ => None,
        };
        let Some(pipe_id) = pipe_id else {
            return SyscallError::InvalidArgument.to_u64();
        };
        let Some(phys) = pipe::phys_addr(pipe_id) else {
            return SyscallError::NotFound.to_u64();
        };
        let pt = crate::scheduler::current_address_space()
            .expect("sys_pipe_map: no address space");
        let Some((vaddr, aligned)) = process::vma_map(&pt, phys.phys(), pipe::PIPE_SIZE as u64) else {
            return SyscallError::ResourceExhausted.to_u64();
        };

        // Store the mapping in the FD so it's unmapped on close
        let mapping = fd::PipeMapping { vaddr, size: aligned };
        match data.fds.get_mut(fd_num) {
            Some(fd::Descriptor::PipeRead(_, ref mut m)) => *m = Some(mapping),
            Some(fd::Descriptor::PipeWrite(_, ref mut m)) => *m = Some(mapping),
            _ => {}
        }

        vaddr.raw()
    })
}

fn sys_socket_create(rx_pipe_id_raw: u64, tx_pipe_id_raw: u64) -> u64 {
    let rx_id = pipe::PipeId::from_raw(rx_pipe_id_raw as usize);
    let tx_id = pipe::PipeId::from_raw(tx_pipe_id_raw as usize);
    let Some(rx) = pipe::open_reader(rx_id) else { return SyscallError::NotFound.to_u64() };
    let Some(tx) = pipe::open_writer(tx_id) else { return SyscallError::NotFound.to_u64() };
    process::with_fd_owner_data(|data| {
        fd_result(fd::alloc(&mut data.fds, fd::Descriptor::Socket { rx, tx }))
    })
}

fn sys_read_nonblock(fd_num: u32, buf: &mut [u8]) -> u64 {
    let result = process::with_fd_owner_data(|data| {
        let r = fd::try_read(&mut data.fds, fd_num, buf);
        let wake = data.fds.get(fd_num).and_then(|d| d.pipe_id_read());
        (r, wake)
    });
    match result {
        (Some(n), wake) => {
            if let Some(id) = wake { process::wake_pipe_writers(id); }
            n
        }
        (None, _) => SyscallError::WouldBlock.to_u64(),
    }
}

fn sys_write_nonblock(fd_num: u32, buf: &[u8]) -> u64 {
    let result = process::with_fd_owner_data(|data| {
        let r = fd::try_write(&mut data.fds, fd_num, buf);
        let wake = data.fds.get(fd_num).and_then(|d| d.pipe_id_write());
        (r, wake)
    });
    match result {
        (Some(n), wake) => {
            if let Some(id) = wake { process::wake_pipe_readers(id); }
            n
        }
        (None, _) => SyscallError::WouldBlock.to_u64(),
    }
}

fn sys_spawn(text: &str, fds: fd::FdTable, env: alloc::vec::Vec<u8>) -> u64 {
    let args: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
    let parent = process::current_process();
    match process::spawn(&args, fds, Some(parent), env) {
        Ok(pid) => pid.raw() as u64,
        Err(e) => e.to_u64(),
    }
}

fn sys_waitpid(pid: u64, flags: u64) -> u64 {
    const WNOHANG: u64 = 1;
    let child_pid = process::Pid::from_raw(pid as u32);
    let caller = process::current_process();
    loop {
        match process::collect_child_zombie(child_pid, caller) {
            Ok(Some(code)) => return code as u64,
            Ok(None) => {
                if flags & WNOHANG != 0 {
                    return SyscallError::WouldBlock.to_u64();
                }
                process::block(None, 0); // woken by wake_tid from exit path
            }
            Err(()) => return SyscallError::NotFound.to_u64(),
        }
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
    let pid = process::current_process();
    let desc = match device::try_claim(device_type, pid) {
        Some(d) => d,
        None => return SyscallError::NotFound.to_u64(),
    };
    process::with_fd_owner_data(|data| fd_result(fd::alloc(&mut data.fds, desc)))
}

// ---------------------------------------------------------------------------
// Service IPC: listen / accept / connect
// ---------------------------------------------------------------------------

fn sys_listen(name: &str) -> u64 {
    let Some(_id) = crate::listener::listen(name) else {
        return SyscallError::AlreadyExists.to_u64();
    };
    process::with_fd_owner_data(|data| {
        fd_result(fd::alloc(&mut data.fds, fd::Descriptor::Listener(alloc::string::String::from(name))))
    })
}

fn sys_accept(fd_num: u32) -> u64 {
    // Get the listener name from the fd
    let name = process::with_fd_owner_data(|data| {
        match data.fds.get(fd_num) {
            Some(fd::Descriptor::Listener(name)) => Some(name.clone()),
            _ => None,
        }
    });
    let Some(name) = name else {
        return SyscallError::InvalidArgument.to_u64();
    };

    let Some(listener_id) = crate::listener::listener_id(&name) else {
        return SyscallError::InvalidArgument.to_u64();
    };

    loop {
        if let Some(conn) = crate::listener::pop_connection(&name) {
            let client_pid = conn.client_pid;
            // PipeReader/PipeWriter move from the queue into the Socket descriptor.
            // No refcount change — ownership transfers.
            let fd = process::with_fd_owner_data(|data| {
                fd::alloc(&mut data.fds, fd::Descriptor::Socket { rx: conn.rx, tx: conn.tx })
            });
            return match fd {
                Ok(fd_num) => ((client_pid.raw() as u64) << 32) | (fd_num as u64),
                Err(e) => e.to_u64(),
            };
        }
        process::block(Some(scheduler::EventSource::Listener(listener_id)), 0);
    }
}

fn sys_connect(name: &str) -> u64 {
    if !crate::listener::exists(name) {
        return SyscallError::NotFound.to_u64();
    }

    let (cs_reader, cs_writer) = pipe::create(); // client → server
    let (sc_reader, sc_writer) = pipe::create(); // server → client

    // Queue the server's end. PipeReader/PipeWriter in the queue keep pipes
    // alive even if the client disconnects before accept.
    let client_pid = process::current_process();
    crate::listener::push_connection(name, listener::PendingConnection {
        rx: cs_reader,   // server reads from client→server
        tx: sc_writer,   // server writes to server→client
        client_pid,
    });
    wake_poll_waiters(name);

    // Client's end
    process::with_fd_owner_data(|data| {
        fd_result(fd::alloc(&mut data.fds, fd::Descriptor::Socket {
            rx: sc_reader,   // client reads from server→client
            tx: cs_writer,   // client writes to client→server
        }))
    })
}

/// Wake processes interested in this specific listener (direct blockers + io_uring watchers).
fn wake_poll_waiters(name: &str) {
    let Some(id) = crate::listener::listener_id(name) else { return };
    let event = crate::scheduler::EventSource::Listener(id);
    crate::scheduler::wake_by_event(event);
    let watchers = crate::listener::io_uring_watchers(id);
    if !watchers.is_empty() {
        crate::io_uring::complete_pending_for_event(&watchers, event);
    }
}

fn sys_alloc_shared(size: u64) -> u64 {
    let pid = process::current_process();
    let addr_space = process::current_address_space();
    shared_memory::alloc(size, pid, &addr_space).raw() as u64
}

fn sys_grant_shared(token: u64, target_pid: u64) -> u64 {
    let pid = process::current_process();
    let token = shared_memory::SharedToken::from_raw(token as u32);
    match shared_memory::grant(token, pid, process::Pid::from_raw(target_pid as u32)) {
        Ok(()) => 0,
        Err(shared_memory::Error::NotFound) => SyscallError::NotFound.to_u64(),
        Err(shared_memory::Error::PermissionDenied) => SyscallError::PermissionDenied.to_u64(),
    }
}

fn sys_map_shared(token: u64) -> u64 {
    let pid = process::current_process();
    let addr_space = process::current_address_space();
    match shared_memory::map(shared_memory::SharedToken::from_raw(token as u32), pid, &addr_space) {
        Ok(addr) => addr,
        Err(shared_memory::Error::NotFound) => SyscallError::NotFound.to_u64(),
        Err(shared_memory::Error::PermissionDenied) => SyscallError::PermissionDenied.to_u64(),
    }
}

fn sys_release_shared(token: u64) -> u64 {
    let pid = process::current_process();
    let token = shared_memory::SharedToken::from_raw(token as u32);
    match shared_memory::release(token, pid) {
        Ok(()) => 0,
        Err(_) => SyscallError::NotFound.to_u64(),
    }
}

fn sys_mmap(req_addr: u64, size: u64, _prot: u64, flags: u64) -> u64 {
    let aligned = crate::mm::align_2m(size as usize);
    let fixed = flags & 4 != 0; // MmapFlags::FIXED

    let Some(pages) = process::PageAlloc::new(aligned) else {
        return SyscallError::Unknown.to_u64();
    };

    if fixed && req_addr != 0 {
        let phys = pages.phys();
        let pt = process::current_address_space();
        let start = req_addr & !(crate::mm::PAGE_2M - 1);
        let end = (req_addr + aligned as u64 + crate::mm::PAGE_2M - 1) & !(crate::mm::PAGE_2M - 1);
        let mut cur = start;
        let mut offset = 0u64;
        while cur < end {
            pt.lock().remap(UserAddr::new(cur), phys + offset, true);
            cur += crate::mm::PAGE_2M;
            offset += crate::mm::PAGE_2M;
        }
        cpu::flush_tlb();
        apic::tlb_shootdown();
        process::with_fd_owner_data(|data| {
            data.mmap_regions.push(process::MmapRegion {
                addr: UserAddr::new(start), size: aligned, _pages: pages, fixed: true,
            });
            data.alloc_count += 1;
            let mem = data.mmap_regions.iter().map(|r| r.size as u64).sum::<u64>();
            if mem > data.peak_memory { data.peak_memory = mem; }
        });
        req_addr
    } else {
        let phys = pages.phys();
        let pt = process::current_address_space();
        let vaddr = process::with_fd_owner_data(|data| {
            let Some((vaddr, _)) = process::vma_map(&pt, phys, aligned as u64) else {
                return Err(());
            };
            data.mmap_regions.push(process::MmapRegion {
                addr: vaddr, size: aligned, _pages: pages, fixed: false,
            });
            data.alloc_count += 1;
            let mem = data.mmap_regions.iter().map(|r| r.size as u64).sum::<u64>();
            if mem > data.peak_memory { data.peak_memory = mem; }
            Ok(vaddr)
        });
        match vaddr {
            Ok(v) => v.raw(),
            Err(()) => SyscallError::ResourceExhausted.to_u64(),
        }
    }
}

fn sys_munmap(addr: u64, _size: u64) -> u64 {
    let pt = process::current_address_space();
    process::with_fd_owner_data(|data| {
        let idx = data.mmap_regions.iter().position(|r| r.addr.raw() == addr);
        if let Some(idx) = idx {
            let region = data.mmap_regions.swap_remove(idx);
            data.free_count += 1;
            if region.fixed {
                let mut cur = region.addr.raw();
                let end = region.addr.raw() + region.size as u64;
                while cur < end {
                    pt.lock().unmap(UserAddr::new(cur));
                    cur += crate::mm::PAGE_2M;
                }
            } else {
                let mut as_guard = pt.lock();
                as_guard.unmap_range(region.addr, region.size as u64);
                as_guard.free_region(region.addr);
            }
            0
        } else {
            SyscallError::NotFound.to_u64()
        }
    })
}

fn sys_thread_join(tid: u64) -> u64 {
    let tid = process::Tid::from_raw(tid as u32);
    let caller = process::current_process();
    loop {
        match process::collect_thread_zombie(tid, caller) {
            Ok(Some(_)) => return 0,
            Ok(None) => process::block(None, 0), // woken by wake_tid from exit path
            Err(()) => return SyscallError::NotFound.to_u64(),
        }
    }
}

fn sys_sysinfo(buf: &mut [u8]) -> u64 {
    const HEADER_SIZE: usize = 48;
    const ENTRY_SIZE: usize = 56;
    if buf.len() < HEADER_SIZE {
        return SyscallError::InvalidArgument.to_u64();
    }

    let (total_mem, used_mem) = crate::mm::pmm::stats();
    let cpu_count = super::smp::cpu_count();
    let uptime = crate::clock::nanos_since_boot();
    let (busy_ticks, total_ticks) = super::idt::cpu_ticks();

    let guard = process::PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();

    let process_count = table.iter().count() as u32;

    buf[0..8].copy_from_slice(&total_mem.to_le_bytes());
    buf[8..16].copy_from_slice(&used_mem.to_le_bytes());
    buf[16..20].copy_from_slice(&cpu_count.to_le_bytes());
    buf[20..24].copy_from_slice(&process_count.to_le_bytes());
    buf[24..32].copy_from_slice(&uptime.to_le_bytes());
    buf[32..40].copy_from_slice(&busy_ticks.to_le_bytes());
    buf[40..48].copy_from_slice(&total_ticks.to_le_bytes());

    let max_entries = (buf.len() - HEADER_SIZE) / ENTRY_SIZE;
    let mut sorted_tids: Vec<process::Tid> = table.iter().map(|(tid, _)| tid).collect();
    sorted_tids.sort();

    let mut pos = HEADER_SIZE;
    for (i, &tid) in sorted_tids.iter().enumerate() {
        if i >= max_entries {
            break;
        }
        let Some(entry) = table.get(tid) else { continue };

        let state: u8 = if matches!(entry.state(), process::ProcessState::Zombie(_)) {
            3
        } else {
            crate::scheduler::thread_sched_state(tid)
        };
        let (is_thread, parent_pid) = match entry.kind() {
            process::Kind::Thread { parent } => (1u8, *parent),
            process::Kind::Process { parent } => (0u8, parent.unwrap_or(process::Pid::MAX)),
        };
        let memory = entry.memory_size();
        let cpu_ns = entry.cpu_ns();
        // Report the process Pid to userland (not the internal Tid)
        let pid = entry.process();

        buf[pos..pos + 4].copy_from_slice(&pid.raw().to_le_bytes());
        buf[pos + 4..pos + 8].copy_from_slice(&parent_pid.raw().to_le_bytes());
        buf[pos + 8] = state;
        buf[pos + 9] = is_thread;
        buf[pos + 10..pos + 12].copy_from_slice(&[0, 0]);
        buf[pos + 12..pos + 20].copy_from_slice(&memory.to_le_bytes());
        buf[pos + 20..pos + 28].copy_from_slice(&cpu_ns.to_le_bytes());
        buf[pos + 28..pos + 56].copy_from_slice(entry.name());

        pos += ENTRY_SIZE;
    }

    pos as u64
}

fn sys_net_info(buf: &mut [u8]) -> u64 {
    let Some(mac) = crate::net::mac() else { return SyscallError::NotFound.to_u64() };
    if buf.len() < 6 { return SyscallError::InvalidArgument.to_u64(); }
    buf[..6].copy_from_slice(&mac);
    0
}

fn sys_net_recv(buf: &mut [u8], timeout_nanos: u64) -> u64 {
    let deadline = if timeout_nanos != u64::MAX {
        crate::clock::nanos_since_boot().saturating_add(timeout_nanos)
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
        process::block(Some(crate::scheduler::EventSource::Network), deadline);
    }
}

fn sys_nanosleep(nanos: u64) -> u64 {
    let deadline = crate::clock::nanos_since_boot().saturating_add(nanos);
    process::block(None, deadline); // woken by deadline expiry only
    0
}

fn sys_dup(fd_num: u32) -> u64 {
    process::with_fd_owner_data(|data| {
        let desc = match data.fds.get(fd_num) {
            Some(d) => d.clone(),
            None => return SyscallError::NotFound.to_u64(),
        };
        fd_result(fd::alloc(&mut data.fds, desc))
    })
}

fn sys_dup2(old_fd: u32, new_fd: u32) -> u64 {
    let mut wake_read = None;
    let mut wake_write = None;
    let result = process::with_fd_owner_data(|data| {
        let desc = match data.fds.get(old_fd) {
            Some(d) => d.clone(),
            None => return SyscallError::NotFound.to_u64(),
        };
        if let Some(existing) = data.fds.get(new_fd) {
            wake_read = existing.pipe_id_read();
            wake_write = existing.pipe_id_write();
            let mut vfs = vfs::lock();
            fd::close(&mut data.fds, &mut vfs, new_fd, process::current_process());
        }
        data.fds.insert_at(new_fd, desc);
        new_fd as u64
    });
    if let Some(id) = wake_read { process::wake_pipe_readers(id); }
    if let Some(id) = wake_write { process::wake_pipe_writers(id); }
    result
}

fn sys_rename(old: &str, new: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let old_abs = vfs.resolve_absolute(&cwd, old);
    let new_abs = vfs.resolve_absolute(&cwd, new);
    match vfs.rename(&old_abs, &new_abs) {
        Ok(()) => 0,
        Err(_) => SyscallError::NotFound.to_u64(),
    }
}

fn sys_mkdir(path: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    vfs.create_dir(&resolved);
    0
}

fn sys_rmdir(path: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    vfs.remove_dir(&resolved);
    0
}

fn sys_symlink(target: &str, link: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, link);
    match vfs.create_symlink(&resolved, target) {
        Ok(()) => 0,
        Err(e) => {
            log!("symlink({target} -> {link}): {e}");
            SyscallError::Unknown.to_u64()
        }
    }
}

fn sys_readlink(path: &str, buf: &mut [u8]) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    match vfs.read_link(&resolved) {
        Some(target) => {
            let bytes = target.as_bytes();
            let len = bytes.len().min(buf.len());
            buf[..len].copy_from_slice(&bytes[..len]);
            len as u64
        }
        None => SyscallError::NotFound.to_u64(),
    }
}

fn sys_dlopen(path: &str, init_out: u64) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let resolved = vfs::lock().resolve_absolute(&cwd, path);

    // Check shared library cache first
    let lib = crate::elf::try_clone_cached(&resolved);
    let mut lib = match lib {
        Some(lib) => lib,
        None => {
            let backing = match vfs::lock().open_backing(&resolved) {
                Some(b) => b,
                None => {
                    log!("dlopen: {}: not found", resolved);
                    return SyscallError::NotFound.to_u64();
                }
            };

            let (lib, rw_vaddr, rw_end_vaddr) = match crate::elf::load_shared_lib(backing.as_ref()) {
                Ok(result) => result,
                Err(msg) => {
                    log!("dlopen: {}", msg);
                    return SyscallError::Unknown.to_u64();
                }
            };

            crate::elf::cache_loaded_lib_pub(&resolved, lib, rw_vaddr, rw_end_vaddr)
        }
    };

    // Map library into current process's virtual address space
    let pt = process::current_address_space();
    process::with_fd_owner_data(|_data| {
        match &lib.memory {
            crate::elf::LibMemory::Owned(alloc) => {
                let phys = DirectMap::phys_of(alloc.ptr());
                let (vaddr, _) = process::vma_map(&pt, phys, alloc.size() as u64)
                    .expect("dlopen: out of virtual address space");
                let delta = vaddr.raw() as i64 - lib.user_base.raw() as i64;
                if delta != 0 {
                    crate::elf::fixup_relative_relocs(&lib, delta);
                }
                lib.user_base = vaddr;
                lib.user_end = (lib.user_end as i64 + delta) as u64;
            }
            crate::elf::LibMemory::Shared { rw_alloc, cached_image, rw_offset, .. } => {
                let cached_phys = cached_image.phys();
                let (lib_vaddr, _) = process::vma_map(&pt, cached_phys, cached_image.size() as u64)
                    .expect("dlopen: out of virtual address space");
                let num_rw_pages = rw_alloc.size() / crate::mm::PAGE_2M as usize;
                let rw_phys = DirectMap::phys_of(rw_alloc.ptr());
                for i in 0..num_rw_pages {
                    let user_virt = lib_vaddr.raw() + *rw_offset as u64 + i as u64 * crate::mm::PAGE_2M;
                    let phys = rw_phys + i as u64 * crate::mm::PAGE_2M;
                    pt.lock().remap(UserAddr::new(user_virt), phys, true);
                }
                cpu::flush_tlb();
                apic::tlb_shootdown();
                let delta = lib_vaddr.raw() as i64 - lib.user_base.raw() as i64;
                if delta != 0 {
                    crate::elf::fixup_relative_relocs(&lib, delta);
                }
                lib.user_base = lib_vaddr;
                lib.user_end = (lib.user_end as i64 + delta) as u64;
            }
        }
    });

    let lib_has_tls = lib.tls_memsz > 0;

    // Resolve relocations and apply DTV relocs (ProcessData lock)
    let data_arc = process::fd_owner_data();
    {
        let mut data = data_arc.lock();
        crate::elf::resolve_dlopen_relocs(&lib, &data.loaded_libs);

        // Apply TPOFF relocs for cross-module IE references (symbols from static-linked modules
        // like std/core whose TLS lives in the static block with known TP-relative offsets).
        if data.tls_total_memsz > 0 {
            let tls_info = crate::elf::TlsModuleInfo {
                libs: &data.loaded_libs,
                modules: &data.tls_modules,
            };
            crate::elf::apply_tpoff_relocs(&lib, 0, data.tls_total_memsz, &tls_info);
        }

        if lib_has_tls {
            let module_id = data.next_tls_module_id;
            data.next_tls_module_id += 1;
            data.tls_modules.push(crate::elf::TlsModule {
                template: lib.tls_template,
                memsz: lib.tls_memsz, base_offset: 0, module_id,
                is_static: false,
            });
            // Apply DTPMOD64/DTPOFF64: write module_id + per-symbol offset into GOT slot pairs.
            // For cross-module GD TLS (r_sym != 0, symbol undefined), resolve to the
            // defining module's ID and TLS offset. DTV entries are left DTV_UNALLOCATED;
            // __tls_get_addr allocates on first access.
            let tls_info = crate::elf::TlsModuleInfo {
                libs: &data.loaded_libs,
                modules: &data.tls_modules,
            };
            crate::elf::apply_dtpmod_relocs(&lib, module_id, &tls_info);
        }
    }

    // Write init_array info to user-provided buffer if requested.
    // Format: [init_array_vaddr: u64, init_array_count: u64]
    // The vaddr is rebased to the library's user_base.
    if init_out != 0 {
        let init_vaddr = if lib.init_array_vaddr != 0 {
            lib.user_base.raw() + lib.init_array_vaddr
        } else {
            0
        };
        let init_count = lib.init_array_size / 8;
        if let Some(phys) = process::current_address_space().lock().translate(UserAddr::new(init_out)) {
            let ptr = phys.as_mut_ptr::<u64>();
            unsafe {
                core::ptr::write(ptr, init_vaddr);
                core::ptr::write(ptr.add(1), init_count);
            }
        }
    }

    // Store the lib in the owner process
    let mut data = data_arc.lock();
    let idx = data.loaded_libs.len();
    data.lib_paths.push(resolved);
    data.loaded_libs.push(lib);
    idx as u64
}

/// Allocate a TLS block for the current thread's DTV entry for `module_id`.
/// Called by __tls_get_addr slow path when the DTV entry is DTV_UNALLOCATED.
/// Returns the physical address of the allocated TLS block (stored in DTV and returned to caller).
fn sys_tls_alloc_block(module_id: u64) -> u64 {
    if module_id == 0 {
        panic!("sys_tls_alloc_block: invalid module_id=0");
    }

    // Read module info from the process-level data (shared across threads via heap owner).
    let owner_arc = process::fd_owner_data();
    let (tls_memsz, tls_template) = {
        let data = owner_arc.lock();
        let m = data.tls_modules.iter().find(|m| m.module_id == module_id)
            .unwrap_or_else(|| panic!("sys_tls_alloc_block: module_id={} not found", module_id));
        (m.memsz, m.template)
    };

    // Allocate TLS block from PMM (needs 2MB alignment for user mapping)
    let page_alloc = process::PageAlloc::new(tls_memsz.max(1))
        .unwrap_or_else(|| panic!("sys_tls_alloc_block: failed to allocate {} bytes", tls_memsz));
    let block_ptr = page_alloc.ptr();

    // Initialize: copy template, zero BSS (PageAlloc is already zeroed)
    unsafe {
        if let Some(template) = &tls_template {
            core::ptr::copy_nonoverlapping(template.base(), block_ptr, template.size());
        }
    }

    // Map into current process's virtual address space via VmaList
    let block_phys = page_alloc.phys();
    let pt = process::current_address_space();
    let tls_vaddr = process::with_fd_owner_data(|data| {
        let (vaddr, _) = process::vma_map(&pt, block_phys, page_alloc.size() as u64)
            .expect("sys_tls_alloc_block: out of virtual address space");
        data.alloc_count += 1;
        vaddr
    });

    // Store in the process-level (fd-owner) data alongside the VMA allocation.
    let tid = process::current_tid();
    process::with_fd_owner_data(|data| {
        data.dynamic_tls_blocks.insert((tid, module_id), page_alloc);
    });

    // Write block address into current thread's DTV.
    // FS base = TP (user-visible virtual address). TCB[8] = DTV pointer (virtual).
    // We need to translate user virtual addresses to kernel direct-map pointers.
    let tp_virt = super::cpu::rdfsbase();
    let tp_phys = pt.lock().translate(UserAddr::new(tp_virt))
        .expect("sys_tls_alloc_block: TP not mapped");
    let tp_kern = tp_phys.as_ptr::<u64>();
    let dtv_virt = unsafe { *tp_kern.add(1) }; // TCB[8] = DTV pointer (virtual)
    assert!(dtv_virt != 0, "sys_tls_alloc_block: no DTV for module_id={}", module_id);
    let dtv_phys = pt.lock().translate(UserAddr::new(dtv_virt))
        .expect("sys_tls_alloc_block: DTV not mapped");
    let dtv_kern = dtv_phys.as_mut_ptr::<u64>();
    let dtv_len = unsafe { *dtv_kern.add(1) } as u64;
    assert!(module_id <= dtv_len, "sys_tls_alloc_block: module_id={} exceeds DTV len={}", module_id, dtv_len);
    unsafe { *dtv_kern.add(2 + (module_id - 1) as usize) = tls_vaddr.raw(); }
    tls_vaddr.raw()
}

fn sys_dlsym(handle: u64, name: &str) -> u64 {
    let data_arc = process::fd_owner_data();
    let data = data_arc.lock();
    let idx = handle as usize;
    if idx >= data.loaded_libs.len() {
        return SyscallError::NotFound.to_u64();
    }
    match crate::elf::dlsym(&data.loaded_libs[idx], name) {
        Some(addr) => addr.raw(),
        None => u64::MAX,
    }
}

// ---------------------------------------------------------------------------
// io_uring syscalls
// ---------------------------------------------------------------------------

fn sys_io_uring_setup(depth: u32) -> u64 {
    let (ring_id, shm_token) = match crate::io_uring::create(depth) {
        Ok(v) => v,
        Err(e) => return e.to_u64(),
    };
    let fd = process::with_fd_owner_data(|data| {
        fd::alloc(&mut data.fds, fd::Descriptor::IoUring(ring_id))
    });
    match fd {
        Ok(fd_num) => {
            // Pack fd and shm_token into return value
            ((shm_token.raw() as u64) << 32) | (fd_num as u64)
        }
        Err(e) => {
            crate::io_uring::destroy(ring_id);
            e.to_u64()
        }
    }
}

fn sys_io_uring_enter(ring_fd: u32, to_submit: u32, min_complete: u32, timeout_nanos: u64) -> u64 {
    let ring_id = process::with_fd_owner_data(|data| {
        match data.fds.get(ring_fd) {
            Some(fd::Descriptor::IoUring(id)) => Some(*id),
            _ => None,
        }
    });
    let Some(ring_id) = ring_id else {
        return SyscallError::InvalidArgument.to_u64();
    };
    match crate::io_uring::enter(ring_id, to_submit, min_complete, timeout_nanos) {
        Ok(n) => n as u64,
        Err(e) => e.to_u64(),
    }
}

fn sys_sched_info(out: &mut toyos_abi::syscall::SchedInfo) -> u64 {
    let pid = process::current_process();
    let vruntime = crate::scheduler::process_vruntime(pid);
    let min_vruntime = crate::scheduler::global_min_vruntime();
    out.vruntime = vruntime;
    out.min_vruntime = min_vruntime;
    0
}

fn sys_query_modules(buf: &mut [u8]) -> u64 {
    use toyos_abi::syscall::ModuleInfo;
    let info_size = core::mem::size_of::<ModuleInfo>();

    process::with_fd_owner_data(|data| {
        // Count modules: 1 (exe) + loaded_libs
        let module_count = 1 + data.loaded_libs.len();

        // Calculate total path bytes
        let exe_path_bytes = data.exe_path.as_bytes();
        let total_path_bytes: usize = exe_path_bytes.len()
            + data.lib_paths.iter().map(|p| p.as_bytes().len()).sum::<usize>();

        let required = module_count * info_size + total_path_bytes;
        if buf.len() < required {
            return SyscallError::InvalidArgument.to_u64();
        }

        let mut path_offset = (module_count * info_size) as u32;

        // Write exe module info
        let (eh_vaddr, eh_size) = (data.exe_eh_frame_hdr_vaddr, data.exe_eh_frame_hdr_size);
        let exe_info = ModuleInfo {
            base: data.elf_base.raw(),
            text_end: data.exe_vaddr_max,
            eh_frame_hdr: if eh_vaddr != 0 { data.elf_base.raw() + eh_vaddr } else { 0 },
            eh_frame_hdr_size: eh_size,
            path_offset,
            path_len: exe_path_bytes.len() as u32,
        };
        buf[..info_size].copy_from_slice(unsafe {
            core::slice::from_raw_parts(&exe_info as *const ModuleInfo as *const u8, info_size)
        });
        buf[path_offset as usize..path_offset as usize + exe_path_bytes.len()]
            .copy_from_slice(exe_path_bytes);
        path_offset += exe_path_bytes.len() as u32;

        // Write library module infos
        for (i, lib) in data.loaded_libs.iter().enumerate() {
            let lib_path_bytes = if i < data.lib_paths.len() {
                data.lib_paths[i].as_bytes()
            } else {
                b""
            };
            let lib_info = ModuleInfo {
                base: lib.user_base.raw(),
                text_end: lib.user_end,
                eh_frame_hdr: if lib.eh_frame_hdr_vaddr != 0 {
                    lib.user_base.raw() + lib.eh_frame_hdr_vaddr
                } else { 0 },
                eh_frame_hdr_size: lib.eh_frame_hdr_size,
                path_offset,
                path_len: lib_path_bytes.len() as u32,
            };
            let off = (1 + i) * info_size;
            buf[off..off + info_size].copy_from_slice(unsafe {
                core::slice::from_raw_parts(&lib_info as *const ModuleInfo as *const u8, info_size)
            });
            buf[path_offset as usize..path_offset as usize + lib_path_bytes.len()]
                .copy_from_slice(lib_path_bytes);
            path_offset += lib_path_bytes.len() as u32;
        }

        module_count as u64
    })
}

/// Terminate the current userspace process (called from exception handlers).
pub fn kill_process(code: i32) -> ! {
    process::exit(code);
}
