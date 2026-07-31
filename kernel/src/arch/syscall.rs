use core::arch::naked_asm;

use alloc::vec::Vec;
use super::{apic, cpu, gdt};
use crate::drivers::acpi;
use crate::user_ptr::SyscallContext;
use crate::{device, fd, keyboard, listener, log, pipe, process, shared_memory, vfs};
use crate::{DirectMap, UserAddr};

// MSR addresses
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;

use toyos_abi::syscall::*;

/// `SYS_DEBUG` action 2's lock, and nothing else's.
///
/// Action 2 takes it and then calls a switching scheduler entry — the shape
/// spec §6.4's tripwire exists to refuse. The assert fires while the guard is
/// still alive, so the guard never drops and this lock stays held for the rest
/// of the boot; that is why it is private to the one deliberate-panic action
/// and shared with nothing.
static LOCK_ACROSS_SWITCH: crate::sync::Lock<()> = crate::sync::Lock::new(());

/// One trip per boot, because the lock above is never released.
///
/// `SYS_DEBUG` is ungated, so without this any process could call action 2 a
/// second time and spin `Lock::lock`'s full 500M iterations on a lock nothing
/// will ever hand over — with IF=0 (`MSR_FMASK` masks it on syscall entry) and
/// preemption disabled, so on a single-CPU machine the timer, the log drains
/// and every other thread are frozen for that whole window. Refusing the
/// second call keeps the tripwire testable and the stall unreachable.
static LOCK_ACROSS_SWITCH_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(true);

/// The last line `SYS_DEBUG` action 3 puts in the log ring before halting.
///
/// Actions 0 and 1 both satisfy the panic handler's recovery predicate and
/// return to userland by design, so neither can exercise the fatal funnel.
/// Action 3 reaches `halt_all_cpus` directly, which is where the on-screen
/// panic console paints.
///
/// The string is the whole synchronisation mechanism for the screen test:
/// `halt_all_cpus` renders *before* it flushes serial, so a host that has
/// seen this line knows the paint already finished — no sleep, no polling.
#[cfg(feature = "test-fatal-halt")]
pub const FATAL_HALT_NONCE: &str = "SYS_DEBUG: fatal halt 4b1d9e2c";

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

        "lock add dword ptr gs:[240], 1",   // preempt_count++

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

        "lock sub dword ptr gs:[240], 1",   // preempt_count--
        // cli before exit_to_user and pop rsp / sysretq: an interrupt after
        // pop rsp would land on the user RSP as a kernel stack. Helper
        // preserves IF=0 across its return.
        "cli",
        // exit_to_user runs BEFORE restoring user GPRs — the sysv64 call
        // would otherwise clobber rcx/r11 (sysretq RIP/RFLAGS) and the
        // restored arg regs. push/pop rax saves the syscall return value
        // and re-aligns rsp to 0(mod 16) for the call.
        "push rax",
        "call {exit_to_user}",
        "pop rax",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r11",
        "pop rcx",
        "pop rsp",              // restore user RSP from kernel stack
        "sysretq",
        handler = sym syscall_handler,
        exit_to_user = sym crate::arch::idt::kernel_exit_to_user_check,
    );
}

extern "sysv64" fn syscall_handler(num: u64, a1: u64, a2: u64, _: u64, a3: u64, a4: u64) -> u64 {
    syscall_dispatch(num, a1, a2, a3, a4)
}

fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let t0 = crate::clock::nanos_since_boot();

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
        SYS_CLOCK => crate::clock::nanos_since_boot(),
        SYS_OPEN => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
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
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a3), a4) else { return bad_addr };
            sys_readdir(path, buf)
        }
        SYS_DELETE => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_delete(path)
        }
        SYS_SHUTDOWN => {
            log!("Syncing filesystems...");
            crate::vfs::lock().sync_all();
            log!("Shutting down.");
            acpi::shutdown();
        }
        SYS_CHDIR => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_chdir(path)
        }
        SYS_GETCWD => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_getcwd(buf)
        }
        SYS_SET_KEYBOARD_LAYOUT => {
            let name = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_set_keyboard_layout(name)
        }
        SYS_PIPE => sys_pipe(),
        SYS_SPAWN => {
            let Some(args) = ctx.user_ref::<SpawnArgs>(UserAddr::new(a1)) else { return bad_addr };
            let text = match ctx.user_str(UserAddr::new(args.argv_ptr), args.argv_len) { Ok(s) => s, Err(e) => return e.to_u64() };
            let fd_count = args.fd_map_count as usize;
            let fds = if fd_count > 0 {
                let Some(pairs) = ctx.user_slice_of::<[u32; 2]>(UserAddr::new(args.fd_map_ptr), fd_count) else { return bad_addr };
                match process::build_child_fds(pairs) {
                    Ok(fds) => fds,
                    Err(e) => return e.to_u64(),
                }
            } else {
                fd::FdTable::new()
            };
            // The env blob is copied onto the kernel heap and kept for the
            // child's whole life, so it needs the bound `user_slice` does not
            // carry. Same constant as argv: both are userland text the kernel
            // owns a copy of.
            let env = if args.env_len > 0 {
                if args.env_len > crate::user_ptr::MAX_USER_STR {
                    return SyscallError::InvalidArgument.to_u64();
                }
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
        // Display integrity, not memory access: framebuffer *contents* are
        // behind shared_memory grants either way. Ungated, any process could
        // scan out over the compositor's frames and move the cursor.
        SYS_GPU_PRESENT | SYS_GPU_SET_CURSOR | SYS_GPU_MOVE_CURSOR => {
            if !device::is_owner(device::DEVICE_FRAMEBUFFER, process::current_process()) {
                return SyscallError::PermissionDenied.to_u64();
            }
            match num {
                SYS_GPU_PRESENT => crate::gpu::present_rect(a1 as u32, a2 as u32, a3 as u32, a4 as u32),
                SYS_GPU_SET_CURSOR => crate::gpu::set_cursor(a1 as u32, a2 as u32),
                _ => crate::gpu::move_cursor(a1 as u32, a2 as u32),
            }
            0
        }
        SYS_ALLOC_SHARED => sys_alloc_shared(a1),
        SYS_GRANT_SHARED => sys_grant_shared(a1, a2),
        SYS_MAP_SHARED => sys_map_shared(a1),
        SYS_RELEASE_SHARED => sys_release_shared(a1),
        SYS_THREAD_SPAWN => sys_thread_spawn(a1, a2, a3, a4),
        SYS_THREAD_JOIN => sys_thread_join(a1),
        SYS_CLOCK_REALTIME => crate::rtc::read_time(),
        SYS_CLOCK_EPOCH => crate::rtc::read_epoch_secs(),
        SYS_SYSINFO => {
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_sysinfo(buf)
        }
        SYS_NANOSLEEP => sys_nanosleep(a1),
        SYS_DUP => sys_dup(a1 as u32),
        SYS_DUP2 => sys_dup2(a1 as u32, a2 as u32),
        SYS_GETPID => process::current_process().raw() as u64,
        SYS_RENAME => {
            let old = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            let new = match ctx.user_str(UserAddr::new(a3), a4) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_rename(old, new)
        }
        SYS_MKDIR => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_mkdir(path)
        }
        SYS_RMDIR => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_rmdir(path)
        }
        SYS_DLOPEN => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_dlopen(path, a3)
        }
        SYS_DLSYM => {
            let name = match ctx.user_str(UserAddr::new(a2), a3) { Ok(s) => s, Err(e) => return e.to_u64() };
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
            // Addressed by ambient authority, so without this any process
            // could put whatever soundd is mid-fill on the wire and drain
            // `tx_free_slots` out from under it. Checked here rather than in
            // `audio::submit_buffer`, which init paths reach with no current
            // process.
            if !device::is_owner(device::DEVICE_AUDIO, process::current_process()) {
                return SyscallError::PermissionDenied.to_u64();
            }
            if crate::audio::submit_buffer(a1 as usize, a2 as u32) { 0 } else { SyscallError::InvalidArgument.to_u64() }
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
        // Both address the NIC by ambient authority, so without this any
        // process could pop frames out of the used ring before netd sees them
        // and, by never refilling, exhaust all 256 RX slots.
        SYS_NIC_RX_POLL => {
            if !device::is_owner(device::DEVICE_NIC, process::current_process()) {
                return SyscallError::PermissionDenied.to_u64();
            }
            match crate::net::poll_rx() {
                Some((buf_idx, frame_len)) => ((buf_idx as u64) << 16) | (frame_len as u64),
                None => 0,
            }
        }
        SYS_NIC_RX_DONE => {
            if !device::is_owner(device::DEVICE_NIC, process::current_process()) {
                return SyscallError::PermissionDenied.to_u64();
            }
            crate::net::refill_rx_buf(a1 as usize).map_or_else(|e| e.to_u64(), |()| 0)
        }
        SYS_NIC_TX => {
            if !device::is_owner(device::DEVICE_NIC, process::current_process()) {
                return SyscallError::PermissionDenied.to_u64();
            }
            match crate::net::submit_tx(a1 as usize) {
                Ok(()) => 0,
                Err(e) => e.to_u64(),
            }
        }
        SYS_SYMLINK => {
            let target = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            let link = match ctx.user_str(UserAddr::new(a3), a4) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_symlink(target, link)
        }
        SYS_READLINK => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a3), a4) else { return bad_addr };
            sys_readlink(path, buf)
        }
        SYS_GPU_SET_RESOLUTION => {
            // Checked before the driver, so a non-claimant never gets its two
            // arbitrary u32s turned into a contiguous physical allocation.
            let pid = process::current_process();
            if !device::is_owner(device::DEVICE_FRAMEBUFFER, pid) {
                return SyscallError::PermissionDenied.to_u64();
            }
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
                Err(e) => e.to_u64(),
            }
        }
        SYS_LISTEN => {
            let name = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_listen(name)
        }
        SYS_ACCEPT => sys_accept(a1 as u32),
        SYS_CONNECT => {
            let name = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
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
            2 => {
                if !LOCK_ACROSS_SWITCH_ARMED.swap(false, core::sync::atomic::Ordering::Relaxed) {
                    return SyscallError::InvalidArgument.to_u64();
                }
                let _held = LOCK_ACROSS_SWITCH.lock();
                crate::scheduler::yield_now();
                0
            }
            // Not compiled into a kernel anyone ships. Every other action
            // costs the caller its own process; this one costs the machine,
            // and no latch fixes that — one call is already a permanent halt.
            #[cfg(feature = "test-fatal-halt")]
            3 => { log!("{}", FATAL_HALT_NONCE); crate::arch::apic::halt_all_cpus(); }
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
        SYS_PROCESS_STATS => {
            let stats_size = core::mem::size_of::<toyos_abi::syscall::ProcessStats>() as u64;
            if a3 < stats_size { return SyscallError::InvalidArgument.to_u64(); }
            let Some(buf) = ctx.user_slice_mut(UserAddr::new(a2), stats_size) else {
                return bad_addr;
            };
            let out = unsafe { &mut *(buf.as_mut_ptr() as *mut toyos_abi::syscall::ProcessStats) };
            sys_process_stats(process::Pid::from_raw(a1 as u32), out)
        },
        SYS_SET_THREAD_NAME => {
            let Some(name) = ctx.user_slice(UserAddr::new(a1), a2.min(28)) else {
                return bad_addr;
            };
            process::set_current_thread_name(name);
            0
        },
        SYS_SET_RT_PRIORITY => {
            // The RT band has no priority above it, so unbounded threads in it
            // starve soundd's mix thread at its own level. Gated on the audio
            // claim rather than in `scheduler::set_current_rt`, which must stay
            // callable from kernel init.
            //
            // Exactly as strong as the claim and no stronger: `SYS_OPEN_DEVICE`
            // is first-come and ungated, so whoever wins the race gets the RT
            // band with it. Spec §9.4 wants a privilege; a claim is not one.
            if !device::is_owner(device::DEVICE_AUDIO, process::current_process()) {
                return SyscallError::PermissionDenied.to_u64();
            }
            crate::scheduler::set_current_rt(a1 != 0);
            0
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

fn sys_write(fd_num: u32, buf: &[u8]) -> u64 {
    loop {
        let action = process::with_fd_owner_data(|data| {
            match fd::try_write(&mut data.fds, fd_num, buf) {
                Some(n) => {
                    let pipe_id = data.fds.get(fd_num).and_then(|d| d.pipe_id_write());
                    Ok((n, pipe_id))
                }
                None => Err(data.fds.get(fd_num).and_then(|d| d.pipe_id_write())),
            }
        });
        match action {
            Ok((n, pipe_id)) => {
                if let Some(id) = pipe_id { process::wake_pipe_readers(id); }
                return n;
            }
            Err(Some(id)) => match pipe::writers_queue(id) {
                Some(q) => crate::scheduler::wait_until(&q, 0, || pipe::has_space(id)),
                None => return SyscallError::NotFound.to_u64(),
            },
            Err(None) => return SyscallError::NotFound.to_u64(),
        }
    }
}

/// What `sys_read` parks on when the fd has nothing to give. Each variant
/// carries what its own re-check needs — the queue is registered on *before*
/// the condition is re-read, which is what closes the check-then-block window.
enum ReadBlock {
    Pipe(alloc::sync::Arc<crate::sched::payload::KWaitQueue>, pipe::PipeId),
    Audio,
    Keyboard(u64),
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
                        Err(Some(ReadBlock::Keyboard(0)))
                    } else if matches!(desc, Some(fd::Descriptor::Audio { info_read: true, .. })) {
                        Err(Some(ReadBlock::Audio))
                    } else if let Some(id) = desc.and_then(|d| d.pipe_id_read()) {
                        Err(pipe::readers_queue(id).map(|q| ReadBlock::Pipe(q, id)))
                    } else if matches!(desc, Some(fd::Descriptor::SerialConsole)) {
                        let deadline = crate::clock::nanos_since_boot() + 10_000_000;
                        Err(Some(ReadBlock::Keyboard(deadline)))
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
            Err(Some(ReadBlock::Pipe(queue, id))) => {
                crate::scheduler::wait_until(&queue, 0, || pipe::has_data(id))
            }
            Err(Some(ReadBlock::Audio)) => crate::scheduler::wait_until(
                &crate::sched::waitqs::AUDIO,
                0,
                crate::audio::has_pending,
            ),
            Err(Some(ReadBlock::Keyboard(deadline))) => crate::scheduler::wait_until(
                &crate::sched::waitqs::KEYBOARD,
                deadline,
                crate::keyboard::has_data,
            ),
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
    let (reader, writer) = pipe::create(process::current_process());
    process::with_fd_owner_data(|data| {
        let Ok(read_fd) = data.fds.insert(fd::Descriptor::PipeRead(reader)) else {
            return SyscallError::ResourceExhausted.to_u64();
        };
        let Ok(write_fd) = data.fds.insert(fd::Descriptor::PipeWrite(writer)) else {
            fd::close(&mut data.fds, &mut *vfs::lock(), read_fd, process::current_process());
            return SyscallError::ResourceExhausted.to_u64();
        };
        ((read_fd as u64) << 32) | write_fd as u64
    })
}

/// May `caller` attach to a pipe created by `creator`?
///
/// `PipeId`s are dense sequential integers with no rights attached, so without
/// a check `for id in 0.. { pipe_open(id) }` attaches to every pipe in the
/// system and `SYS_PIPE_MAP` then hands over the raw 2 MiB ring page.
///
/// The policy the API means is a delegation: an id is openable by a process it
/// was deliberately handed to over IPC. The kernel cannot see that hand-off —
/// the id travels inside a message payload — but it can see the channel it
/// travelled on. So: the creator itself, or a process holding a live socket to
/// the creator. Both real cross-process users satisfy it in opposite
/// directions — netd opens pipes its *client* created, an audio client opens
/// the signal pipe *soundd* created.
///
/// Already holding a descriptor for the pipe also qualifies: a child that
/// inherited a pipe fd through a spawn `fd_map` is not its creator and has no
/// socket to the parent, and re-opening what you already hold grants nothing.
///
/// What it cannot express: *which* of a peer's pipes. A peer entitled to one
/// id is entitled to all of them, so a compromised daemon can still walk its
/// clients' pipes. Narrowing that needs a right attached to the id itself,
/// which is what `specs/capability-handles-spec.md` replaces this syscall
/// with entirely. Stopgap until then.
fn may_open_pipe(caller: process::Pid, creator: process::Pid, id: pipe::PipeId) -> bool {
    if caller == creator {
        return true;
    }
    process::with_fd_owner_data(|data| {
        data.fds.iter().any(|(_, d)| {
            d.pipe_id_read() == Some(id)
                || d.pipe_id_write() == Some(id)
                || matches!(d, fd::Descriptor::Socket { peer, .. } if *peer == creator)
        })
    })
}

fn sys_pipe_open(pipe_id: u64, mode: u64) -> u64 {
    let id = pipe::PipeId::from_raw(pipe_id as usize);
    let Some(creator) = pipe::creator(id) else {
        return SyscallError::NotFound.to_u64();
    };
    if !may_open_pipe(process::current_process(), creator, id) {
        return SyscallError::PermissionDenied.to_u64();
    }
    match mode {
        0 => {
            let Some(reader) = pipe::open_reader(id) else { return SyscallError::NotFound.to_u64() };
            process::with_fd_owner_data(|data| fd_result(data.fds.insert(fd::Descriptor::PipeRead(reader))))
        }
        1 => {
            let Some(writer) = pipe::open_writer(id) else { return SyscallError::NotFound.to_u64() };
            process::with_fd_owner_data(|data| fd_result(data.fds.insert(fd::Descriptor::PipeWrite(writer))))
        }
        _ => SyscallError::InvalidArgument.to_u64(),
    }
}

fn sys_pipe_id(fd_num: u32) -> u64 {
    process::with_fd_owner_data(|data| {
        match data.fds.get(fd_num) {
            Some(fd::Descriptor::PipeRead(r)) | Some(fd::Descriptor::TtyRead(r)) => r.id().raw() as u64,
            Some(fd::Descriptor::PipeWrite(w)) | Some(fd::Descriptor::TtyWrite(w)) => w.id().raw() as u64,
            _ => SyscallError::InvalidArgument.to_u64(),
        }
    })
}

fn sys_pipe_map(fd_num: u32) -> u64 {
    process::with_fd_owner_data(|data| {
        let pipe_id = match data.fds.get(fd_num) {
            Some(fd::Descriptor::PipeRead(r)) | Some(fd::Descriptor::TtyRead(r)) => Some(r.id()),
            Some(fd::Descriptor::PipeWrite(w)) | Some(fd::Descriptor::TtyWrite(w)) => Some(w.id()),
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
        let Some((vaddr, _aligned)) = process::vma_map(&pt, phys.phys(), pipe::PIPE_SIZE as u64) else {
            return SyscallError::ResourceExhausted.to_u64();
        };

        vaddr.raw()
    })
}

/// Bundle two pipes the caller already holds into one socket descriptor.
///
/// Without a check this is a second, quieter route to any pipe in the system.
/// Its only caller is std's `make_socket_fd`, which wraps two pipes this same
/// process created and still holds fds for, so "you must already hold both
/// ends, in the right direction" is exact rather than conservative — and
/// unlike `SYS_PIPE_OPEN` it grants no new access, so it can be that strict.
fn sys_socket_create(rx_pipe_id_raw: u64, tx_pipe_id_raw: u64) -> u64 {
    let rx_id = pipe::PipeId::from_raw(rx_pipe_id_raw as usize);
    let tx_id = pipe::PipeId::from_raw(tx_pipe_id_raw as usize);
    let holds_both = process::with_fd_owner_data(|data| {
        let mut has_rx = false;
        let mut has_tx = false;
        for (_, d) in data.fds.iter() {
            has_rx |= d.pipe_id_read() == Some(rx_id);
            has_tx |= d.pipe_id_write() == Some(tx_id);
        }
        has_rx && has_tx
    });
    if !holds_both {
        return SyscallError::PermissionDenied.to_u64();
    }
    let Some(rx) = pipe::open_reader(rx_id) else { return SyscallError::NotFound.to_u64() };
    let Some(tx) = pipe::open_writer(tx_id) else { return SyscallError::NotFound.to_u64() };
    let peer = process::current_process();
    process::with_fd_owner_data(|data| {
        fd_result(data.fds.insert(fd::Descriptor::Socket { rx, tx, peer }))
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
    let queue = crate::scheduler::park_lot();
    loop {
        // Registered before the table is read, so a child that exits in the
        // park window claims the registration instead of aiming a wake at a
        // thread that is not parked yet.
        let ticket = crate::scheduler::prepare_wait(queue);
        match process::wait_child_zombie(child_pid, caller) {
            Ok(Some(code)) => {
                ticket.cancel();
                return code as u64;
            }
            Ok(None) => {
                if flags & WNOHANG != 0 {
                    ticket.cancel();
                    return SyscallError::WouldBlock.to_u64();
                }
                crate::scheduler::block_on(ticket, 0);
            }
            Err(()) => {
                ticket.cancel();
                return SyscallError::NotFound.to_u64();
            }
        }
    }
}

fn sys_open_device(device_type: u64) -> u64 {
    let pid = process::current_process();
    // NotFound means the machine has no such device and nothing else, because
    // that is the one answer a daemon is entitled to degrade on. Collapsing
    // Owned into it made soundd print "no audio device on this machine" and
    // exit 0 whenever another process held the claim.
    let desc = match device::try_claim(device_type, pid) {
        Ok(d) => d,
        Err(device::ClaimError::Absent) => return SyscallError::NotFound.to_u64(),
        Err(device::ClaimError::Owned) => return SyscallError::AlreadyExists.to_u64(),
        Err(device::ClaimError::UnknownType) => return SyscallError::InvalidArgument.to_u64(),
        Err(device::ClaimError::GrantFailed) => return SyscallError::ResourceExhausted.to_u64(),
    };
    process::with_fd_owner_data(|data| fd_result(data.fds.insert(desc)))
}

// Service IPC: listen / accept / connect

fn sys_listen(name: &str) -> u64 {
    let Some(_id) = crate::listener::listen(name, process::current_process()) else {
        return SyscallError::AlreadyExists.to_u64();
    };
    process::with_fd_owner_data(|data| {
        fd_result(data.fds.insert(fd::Descriptor::Listener(alloc::string::String::from(name))))
    })
}

fn sys_accept(fd_num: u32) -> u64 {
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
                data.fds.insert(fd::Descriptor::Socket { rx: conn.rx, tx: conn.tx, peer: client_pid })
            });
            return match fd {
                Ok(fd_num) => ((client_pid.raw() as u64) << 32) | (fd_num as u64),
                Err(e) => e.to_u64(),
            };
        }
        match crate::listener::acceptors(listener_id) {
            Some(q) => crate::scheduler::wait_until(&q, 0, || {
                crate::listener::has_pending_by_id(listener_id)
            }),
            None => return SyscallError::NotFound.to_u64(),
        }
    }
}

fn sys_connect(name: &str) -> u64 {
    // The owner lookup doubles as the existence check: a client knows only a
    // service name, and this is where it learns which process it is talking to.
    let Some(server_pid) = crate::listener::owner(name) else {
        return SyscallError::NotFound.to_u64();
    };

    let client_pid = process::current_process();
    let (cs_reader, cs_writer) = pipe::create(client_pid); // client → server
    let (sc_reader, sc_writer) = pipe::create(client_pid); // server → client

    // Queue the server's end. PipeReader/PipeWriter in the queue keep pipes
    // alive even if the client disconnects before accept.
    crate::listener::push_connection(name, listener::PendingConnection {
        rx: cs_reader,   // server reads from client→server
        tx: sc_writer,   // server writes to server→client
        client_pid,
    });
    wake_poll_waiters(name);

    process::with_fd_owner_data(|data| {
        fd_result(data.fds.insert(fd::Descriptor::Socket {
            rx: sc_reader,   // client reads from server→client
            tx: cs_writer,   // client writes to client→server
            peer: server_pid,
        }))
    })
}

/// Wake processes interested in this specific listener (direct blockers + io_uring watchers).
fn wake_poll_waiters(name: &str) {
    let Some(id) = crate::listener::listener_id(name) else { return };
    if let Some(queue) = crate::listener::acceptors(id) {
        crate::sched::waitqs::wake_all(&queue);
    }
    let event = crate::io_uring::Source::Listener(id);
    let watchers = crate::listener::io_uring_watchers(id);
    if !watchers.is_empty() {
        crate::io_uring::complete_pending_for_event(&watchers, event);
    }
}

fn sys_alloc_shared(size: u64) -> u64 {
    let pid = process::current_process();
    let addr_space = process::current_address_space();
    match shared_memory::alloc(size, pid, &addr_space) {
        Ok(token) => token.raw() as u64,
        Err(shared_memory::Error::InvalidSize) => SyscallError::InvalidArgument.to_u64(),
        Err(shared_memory::Error::OutOfMemory)
        | Err(shared_memory::Error::OutOfVirtualMemory) => SyscallError::ResourceExhausted.to_u64(),
        Err(shared_memory::Error::NotFound)
        | Err(shared_memory::Error::PermissionDenied) => unreachable!("alloc grants to its own caller"),
    }
}

fn sys_grant_shared(token: u64, target_pid: u64) -> u64 {
    let pid = process::current_process();
    let token = shared_memory::SharedToken::from_raw(token as u32);
    match shared_memory::grant(token, pid, process::Pid::from_raw(target_pid as u32)) {
        Ok(()) => 0,
        Err(shared_memory::Error::NotFound) => SyscallError::NotFound.to_u64(),
        Err(shared_memory::Error::PermissionDenied) => SyscallError::PermissionDenied.to_u64(),
        Err(shared_memory::Error::OutOfVirtualMemory)
        | Err(shared_memory::Error::InvalidSize)
        | Err(shared_memory::Error::OutOfMemory) => unreachable!("grant neither sizes nor maps"),
    }
}

fn sys_map_shared(token: u64) -> u64 {
    let pid = process::current_process();
    let addr_space = process::current_address_space();
    match shared_memory::map(shared_memory::SharedToken::from_raw(token as u32), pid, &addr_space) {
        Ok(addr) => addr,
        Err(shared_memory::Error::NotFound) => SyscallError::NotFound.to_u64(),
        Err(shared_memory::Error::PermissionDenied) => SyscallError::PermissionDenied.to_u64(),
        Err(shared_memory::Error::OutOfVirtualMemory) => SyscallError::ResourceExhausted.to_u64(),
        Err(shared_memory::Error::InvalidSize)
        | Err(shared_memory::Error::OutOfMemory) => unreachable!("map does not allocate"),
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
    // `size` crossed the trust boundary. Zero is a request for nothing and a
    // size whose 2 MiB rounding does not fit cannot be expressed at all;
    // neither is an allocation failure, so neither is ResourceExhausted. The
    // rounding must not be allowed to wrap — that would silently turn a huge
    // request into a small one. No policy ceiling is needed above that: the
    // PMM's own `free_count` check is a physical limit.
    if size == 0 || (size as usize).checked_add(crate::mm::PAGE_2M as usize - 1).is_none() {
        return SyscallError::InvalidArgument.to_u64();
    }
    let aligned = crate::mm::align_2m(size as usize);
    let fixed = flags & 4 != 0; // MmapFlags::FIXED

    // A fixed mapping bypasses `find_gap`, so it has to respect `find_gap`'s
    // range itself: `PageTables::remap` only asserts 2 MiB alignment, so a
    // kernel-half `req_addr` reaches `ensure_table`, which ORs PAGE_USER onto
    // the *shared* kernel PML4 entry (`new_user` shallow-copies PML4[256..512])
    // and writes a PDE into the shared kernel page directory — a user-writable
    // window visible to the kernel and every other process.
    //
    // A 2 MiB-page kernel cannot honour a finer-grained `req_addr`, and there
    // is nothing to clamp a request to when the granularity itself is what
    // cannot be met, so a misaligned one is refused rather than rounded. That
    // is also what `toyos-abi`'s `mmap` documents, and it keeps `start ==
    // req_addr`, so the address recorded in `mmap_regions` is the one handed
    // back and `munmap` can find it.
    let fixed_start = if fixed && req_addr != 0 {
        let Some(end) = req_addr.checked_add(aligned as u64) else {
            return SyscallError::InvalidArgument.to_u64();
        };
        if req_addr & (crate::mm::PAGE_2M - 1) != 0
            || req_addr < crate::vma::ALLOC_FLOOR
            || end > crate::vma::ALLOC_CEILING
            || !crate::user_ptr::check_user_range(UserAddr::new(req_addr), aligned as u64)
        {
            return SyscallError::InvalidArgument.to_u64();
        }
        Some(req_addr)
    } else {
        None
    };

    // Allocate only once the request is known to be satisfiable, so a refused
    // fixed mapping does not leak its pages.
    let Some(pages) = process::PageAlloc::new(aligned, crate::mm::pmm::Category::Mmap) else {
        return SyscallError::ResourceExhausted.to_u64();
    };

    if let Some(start) = fixed_start {
        let phys = pages.phys();
        let pt = process::current_address_space();
        let end = start + aligned as u64;
        let mut cur = start;
        let mut offset = 0u64;
        while cur < end {
            pt.lock().remap(UserAddr::new(cur), phys + offset, true);
            cur += crate::mm::PAGE_2M;
            offset += crate::mm::PAGE_2M;
        }
        crate::mm::paging::flush_tlb_all();
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

/// `spawn_thread` stores `stack_ptr - stack_base`, and both are raw syscall
/// arguments. A base above the pointer describes no stack at all, so there is
/// nothing to clamp it to and it is refused.
fn sys_thread_spawn(entry: u64, stack_ptr: u64, arg: u64, stack_base: u64) -> u64 {
    if stack_base > stack_ptr {
        return SyscallError::InvalidArgument.to_u64();
    }
    // Every `None` from `spawn_thread` is a resource failure (TLS, kernel
    // stack, virtual address space) or a teardown race, never a bad argument.
    process::spawn_thread(entry, stack_ptr, arg, stack_base)
        .map_or(SyscallError::ResourceExhausted.to_u64(), |t| t.raw() as u64)
}

fn sys_thread_join(tid: u64) -> u64 {
    let tid = process::Tid::from_raw(tid as u32);
    let caller = process::current_process();
    let queue = crate::scheduler::park_lot();
    loop {
        let ticket = crate::scheduler::prepare_wait(queue);
        match process::wait_thread_zombie(tid, caller) {
            Ok(Some(_)) => {
                ticket.cancel();
                return 0;
            }
            Ok(None) => crate::scheduler::block_on(ticket, 0),
            Err(()) => {
                ticket.cancel();
                return SyscallError::NotFound.to_u64();
            }
        }
    }
}

fn sys_sysinfo(buf: &mut [u8]) -> u64 {
    const HEADER_SIZE: usize = 48;
    const ENTRY_SIZE: usize = 64;
    if buf.len() < HEADER_SIZE {
        return SyscallError::InvalidArgument.to_u64();
    }

    let (total_mem, used_mem) = crate::mm::pmm::stats();
    let cpu_count = super::smp::cpu_count();
    let uptime = crate::clock::nanos_since_boot();
    let total_cpu_ns = crate::scheduler::total_cpu_ns();
    let total_available_ns = uptime * cpu_count as u64;

    let guard = process::PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();

    let entry_count: u32 = table.iter().flat_map(|(_, proc)| proc.threads().iter().map(move |(tid, thread)| (tid, proc, thread))).count() as u32;

    buf[0..8].copy_from_slice(&total_mem.to_le_bytes());
    buf[8..16].copy_from_slice(&used_mem.to_le_bytes());
    buf[16..20].copy_from_slice(&cpu_count.to_le_bytes());
    buf[20..24].copy_from_slice(&entry_count.to_le_bytes());
    buf[24..32].copy_from_slice(&uptime.to_le_bytes());
    buf[32..40].copy_from_slice(&total_cpu_ns.to_le_bytes());
    buf[40..48].copy_from_slice(&total_available_ns.to_le_bytes());

    let max_entries = (buf.len() - HEADER_SIZE) / ENTRY_SIZE;

    // Collect and sort by (pid, tid) for stable output
    let mut entries: Vec<(process::Tid, &process::ProcessEntry, &process::ThreadEntry)> =
        table.iter().flat_map(|(_, proc)| proc.threads().iter().map(move |(tid, thread)| (tid, proc, thread))).collect();
    entries.sort_by_key(|(tid, proc, _)| (proc.pid(), *tid));

    let mut pos = HEADER_SIZE;
    for (i, &(tid, proc, thread)) in entries.iter().enumerate() {
        if i >= max_entries {
            break;
        }

        let state: u8 = if matches!(thread.state(), process::ProcessState::Zombie(_)) {
            3
        } else {
            thread.sched().map_or(3, crate::scheduler::task_sched_state)
        };
        let is_thread: u8 = if tid != proc.main_tid() { 1 } else { 0 };
        let parent_pid = proc.parent().unwrap_or(process::Pid::MAX);

        let memory = if let Some(data) = proc.process_data().try_lock() {
            let demand = data.demand_pages.iter().map(|p| p.size() as u64).sum::<u64>();
            let mmap = data.mmap_regions.iter().map(|r| r._pages.size() as u64).sum::<u64>();
            let tls = data.elf.dynamic_tls_blocks.values().map(|p| p.size() as u64).sum::<u64>();
            let libs: u64 = data.elf.loaded_libs.iter().map(|l| match &l.memory {
                crate::elf::LibMemory::Owned(alloc) => alloc.size() as u64,
                crate::elf::LibMemory::Shared { rw_alloc, .. } => rw_alloc.size() as u64,
            }).sum();
            demand + mmap + tls + libs
        } else {
            0
        };
        let cpu_ns = thread.sched().map_or(0, crate::scheduler::task_cpu_ns);
        let pid = proc.pid();

        let name = if thread.name()[0] != 0 { thread.name() } else { proc.name() };

        buf[pos..pos + 4].copy_from_slice(&pid.raw().to_le_bytes());
        buf[pos + 4..pos + 8].copy_from_slice(&parent_pid.raw().to_le_bytes());
        buf[pos + 8..pos + 12].copy_from_slice(&tid.raw().to_le_bytes());
        buf[pos + 12] = state;
        buf[pos + 13] = is_thread;
        buf[pos + 14..pos + 16].copy_from_slice(&[0, 0]);
        buf[pos + 16..pos + 24].copy_from_slice(&memory.to_le_bytes());
        buf[pos + 24..pos + 32].copy_from_slice(&cpu_ns.to_le_bytes());
        buf[pos + 32..pos + 60].copy_from_slice(name);
        buf[pos + 60..pos + 64].copy_from_slice(&[0, 0, 0, 0]);

        pos += ENTRY_SIZE;
    }

    pos as u64
}

fn sys_nanosleep(nanos: u64) -> u64 {
    let deadline = crate::clock::nanos_since_boot().saturating_add(nanos);
    // No condition to re-check: the deadline is the wake, and one that has
    // already passed fires at the next scheduler entry.
    crate::scheduler::block_on(
        crate::scheduler::prepare_wait(crate::scheduler::park_lot()),
        deadline,
    );
    0
}

fn sys_dup(fd_num: u32) -> u64 {
    process::with_fd_owner_data(|data| {
        let desc = match data.fds.get(fd_num) {
            Some(d) => d.clone(),
            None => return SyscallError::NotFound.to_u64(),
        };
        fd_result(data.fds.insert(desc))
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
        match data.fds.insert_at(new_fd, desc) {
            Ok(()) => new_fd as u64,
            Err(e) => e.to_u64(),
        }
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

            let (lib, rw_offset, rw_size) = match crate::elf::load_shared_lib(backing.as_ref()) {
                Ok(result) => result,
                Err(msg) => {
                    log!("dlopen: {}", msg);
                    return SyscallError::Unknown.to_u64();
                }
            };

            crate::elf::cache_loaded_lib_pub(&resolved, lib, rw_offset, rw_size)
        }
    };

    // A process's virtual address space is a resource like any other, and
    // `SYS_DLOPEN` neither dedups a path nor frees anything on `SYS_DLCLOSE`,
    // so exhausting it is a loop any process can write. Exhaustion is an error
    // return, not an `.expect` in syscall context.
    let pt = process::current_address_space();
    let mapped = process::with_fd_owner_data(|_data| {
        match &lib.memory {
            crate::elf::LibMemory::Owned(alloc) => {
                let phys = DirectMap::phys_of(alloc.ptr());
                let Some((vaddr, _)) = process::vma_map(&pt, phys, alloc.size() as u64) else {
                    return Err(SyscallError::ResourceExhausted);
                };
                let delta = vaddr.raw() as i64 - lib.user_base.raw() as i64;
                if delta != 0 {
                    crate::elf::rebase_relative_relocs(&lib, delta);
                }
                lib.user_base = vaddr;
                lib.user_end = (lib.user_end as i64 + delta) as u64;
            }
            crate::elf::LibMemory::Shared { rw_alloc, cached_image, rw_offset, .. } => {
                let cached_phys = cached_image.phys();
                let Some((lib_vaddr, _)) = process::vma_map(&pt, cached_phys, cached_image.size() as u64) else {
                    return Err(SyscallError::ResourceExhausted);
                };
                let num_rw_pages = rw_alloc.size() / crate::mm::PAGE_2M as usize;
                let rw_phys = DirectMap::phys_of(rw_alloc.ptr());
                for i in 0..num_rw_pages {
                    let user_virt = lib_vaddr.raw() + *rw_offset as u64 + i as u64 * crate::mm::PAGE_2M;
                    let phys = rw_phys + i as u64 * crate::mm::PAGE_2M;
                    pt.lock().remap(UserAddr::new(user_virt), phys, true);
                }
                crate::mm::paging::flush_tlb_all();
                apic::tlb_shootdown();
                let delta = lib_vaddr.raw() as i64 - lib.user_base.raw() as i64;
                if delta != 0 {
                    crate::elf::rebase_relative_relocs(&lib, delta);
                }
                lib.user_base = lib_vaddr;
                lib.user_end = (lib.user_end as i64 + delta) as u64;
            }
        }
        Ok(())
    });
    if let Err(e) = mapped {
        log!("dlopen: {}: out of virtual address space", resolved);
        return e.to_u64();
    }

    let lib_has_tls = lib.tls_memsz > 0;

    let data_arc = process::fd_owner_data();
    {
        let mut data = data_arc.lock();
        crate::elf::resolve_dlopen_relocs(&lib, &data.elf.loaded_libs);

        // Apply TPOFF relocs for cross-module IE references (symbols from static-linked modules
        // like std/core whose TLS lives in the static block with known TP-relative offsets).
        if data.elf.tls_total_memsz > 0 {
            let tls_info = crate::elf::TlsModuleInfo {
                libs: &data.elf.loaded_libs,
                modules: &data.elf.tls_modules,
            };
            crate::elf::apply_tpoff_relocs(&lib, 0, data.elf.tls_total_memsz, &tls_info);
        }

        if lib_has_tls {
            let module_id = data.elf.next_tls_module_id;
            data.elf.next_tls_module_id += 1;
            data.elf.tls_modules.push(crate::elf::TlsModule {
                template: lib.tls_template,
                memsz: lib.tls_memsz, base_offset: 0, module_id,
                is_static: false,
            });
            // Apply DTPMOD64/DTPOFF64: write module_id + per-symbol offset into GOT slot pairs.
            // For cross-module GD TLS (r_sym != 0, symbol undefined), resolve to the
            // defining module's ID and TLS offset. DTV entries are left DTV_UNALLOCATED;
            // __tls_get_addr allocates on first access.
            let tls_info = crate::elf::TlsModuleInfo {
                libs: &data.elf.loaded_libs,
                modules: &data.elf.tls_modules,
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

    let mut data = data_arc.lock();
    let idx = data.elf.loaded_libs.len();
    data.elf.lib_paths.push(resolved);
    data.elf.loaded_libs.push(lib);
    idx as u64
}

/// Allocate a TLS block for the current thread's DTV entry for `module_id`.
/// Called by __tls_get_addr's slow path when the DTV entry is DTV_UNALLOCATED.
/// Returns the block's virtual address, also written into the DTV.
///
/// `module_id` crosses the trust boundary: every rejection here is an error
/// return, never a panic.
///
/// The DTV is found through the thread's own kernel-side TLS allocation, never
/// by chasing a pointer out of the FS base: CR4.FSGSBASE is on, so userland
/// owns that register, and a raw `AddressSpace::translate` of TCB[8] applies no
/// user-half check and resolves kernel addresses through the direct map
/// shallow-copied into every user PML4.
fn sys_tls_alloc_block(module_id: u64) -> u64 {
    match tls_alloc_block(module_id) {
        Ok(vaddr) => vaddr,
        Err(e) => e.to_u64(),
    }
}

fn tls_alloc_block(module_id: u64) -> Result<u64, SyscallError> {
    // The valid set is the process's own module list, which the kernel built.
    if module_id == 0 {
        return Err(SyscallError::InvalidArgument);
    }
    // The DTV is a fixed-capacity array the kernel wrote; a module past its
    // end has nowhere to be recorded. Bounded by the kernel's own constant,
    // never by the `len` field in the DTV, which the process can rewrite.
    if module_id > crate::loader::DTV_INITIAL_CAPACITY as u64 {
        return Err(SyscallError::ResourceExhausted);
    }

    let owner_arc = process::fd_owner_data();
    let (tls_memsz, tls_template) = {
        let data = owner_arc.lock();
        let m = data.elf.tls_modules.iter().find(|m| m.module_id == module_id)
            .ok_or(SyscallError::InvalidArgument)?;
        (m.memsz, m.template)
    };

    // A DTV entry leaves DTV_UNALLOCATED once and never returns, so a repeat
    // call for the same (thread, module) is the same block asked for twice.
    // Serving a fresh one frees pages userland still points into while the
    // first mapping stays present, USER and writable, over whatever the PMM
    // hands out next.
    let tid = process::current_tid();
    let existing = process::with_fd_owner_data(|data| {
        data.elf.dynamic_tls_blocks.get(&(tid, module_id)).map(|b| b.vaddr())
    });

    let tls_vaddr = match existing {
        Some(vaddr) => vaddr,
        None => {
            let page_alloc = process::PageAlloc::new(tls_memsz.max(1), crate::mm::pmm::Category::Tls)
                .ok_or(SyscallError::ResourceExhausted)?;
            unsafe {
                if let Some(template) = &tls_template {
                    core::ptr::copy_nonoverlapping(template.base(), page_alloc.ptr(), template.size());
                }
            }

            let block_phys = page_alloc.phys();
            let pt = process::current_address_space();
            process::with_fd_owner_data(|data| {
                let (vaddr, _) = process::vma_map(&pt, block_phys, page_alloc.size() as u64)
                    .ok_or(SyscallError::ResourceExhausted)?;
                data.alloc_count += 1;
                data.elf.dynamic_tls_blocks
                    .insert((tid, module_id), process::MappedPages::new(vaddr, page_alloc));
                Ok(vaddr)
            })?
        }
    };

    // The DTV lives at offset 0 of the thread's own TLS allocation. Every user
    // thread gets one from `setup_tls`/`setup_combined_tls`, so its absence is
    // a kernel bug.
    process::with_current_data(|data| {
        let tls = data.tls_pages.as_ref().expect("sys_tls_alloc_block: thread has no TLS allocation");
        let dtv_kern = tls.ptr() as *mut u64;
        unsafe { *dtv_kern.add(2 + (module_id - 1) as usize) = tls_vaddr.raw(); }
    });
    Ok(tls_vaddr.raw())
}

fn sys_dlsym(handle: u64, name: &str) -> u64 {
    let data_arc = process::fd_owner_data();
    let data = data_arc.lock();
    let idx = handle as usize;
    if idx >= data.elf.loaded_libs.len() {
        return SyscallError::NotFound.to_u64();
    }
    match crate::elf::dlsym(&data.elf.loaded_libs[idx], name) {
        Some(addr) => addr.raw(),
        None => u64::MAX,
    }
}

fn sys_io_uring_setup(depth: u32) -> u64 {
    let (ring_id, shm_token) = match crate::io_uring::create(depth) {
        Ok(v) => v,
        Err(e) => return e.to_u64(),
    };
    let fd = process::with_fd_owner_data(|data| {
        data.fds.insert(fd::Descriptor::IoUring(ring_id))
    });
    match fd {
        Ok(fd_num) => {
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
    out.vruntime = crate::scheduler::process_vruntime(pid);
    out.min_vruntime = crate::scheduler::global_min_vruntime();
    out.lag = crate::scheduler::process_lag(pid);
    0
}

fn sys_process_stats(child_pid: process::Pid, out: &mut toyos_abi::syscall::ProcessStats) -> u64 {
    let snap = process::with_fd_owner_data(|data| {
        let pos = data.child_stats.iter().position(|(pid, _)| *pid == child_pid);
        pos.map(|i| data.child_stats.remove(i).1)
    });
    match snap {
        Some(s) => { *out = s; 0 }
        None => SyscallError::NotFound.to_u64(),
    }
}

fn sys_query_modules(buf: &mut [u8]) -> u64 {
    use toyos_abi::syscall::ModuleInfo;
    let info_size = core::mem::size_of::<ModuleInfo>();

    process::with_fd_owner_data(|data| {
        let module_count = 1 + data.elf.loaded_libs.len();

        let exe_path_bytes = data.exe_path.as_bytes();
        let total_path_bytes: usize = exe_path_bytes.len()
            + data.elf.lib_paths.iter().map(|p| p.as_bytes().len()).sum::<usize>();

        let required = module_count * info_size + total_path_bytes;
        if buf.len() < required {
            return SyscallError::InvalidArgument.to_u64();
        }

        let mut path_offset = (module_count * info_size) as u32;

        let (eh_vaddr, eh_size) = (data.elf.exe_eh_frame_hdr_vaddr, data.elf.exe_eh_frame_hdr_size);
        let exe_info = ModuleInfo {
            base: data.elf.elf_base.raw(),
            text_end: data.elf.exe_vaddr_max,
            eh_frame_hdr: if eh_vaddr != 0 { data.elf.elf_base.raw() + eh_vaddr } else { 0 },
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

        for (i, lib) in data.elf.loaded_libs.iter().enumerate() {
            let lib_path_bytes = if i < data.elf.lib_paths.len() {
                data.elf.lib_paths[i].as_bytes()
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
