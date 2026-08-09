use alloc::vec::Vec;
use super::entry::{restore_user_state, ring3_naked_asm, save_user_state, Ring3Entry};
use super::{cpu, gdt};
use crate::drivers::acpi;
use crate::mm::paging::CachePolicy;
use crate::user_ptr::{SyscallContext, UserBytes, UserBytesMut};
use crate::{device, fd, listener, log, pipe, process, shared_memory, vfs};
use crate::{DirectMap, UserAddr};

// MSR addresses
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;

use toyos_abi::syscall::*;

/// The numbers deleted syscalls used, and what each one was.
///
/// **A number a deleted syscall used is retired, never reused.** This was two
/// hand-written `29 | 30 =>` arms with the names in a trailing comment; a third
/// pair would have been a table, and thirteen more arrive with this branch.
///
/// The rows must be strictly ascending, which is checked rather than asked for:
/// it is the whole of what stops one number being retired twice.
macro_rules! retired_syscalls {
    ($($num:literal => $name:literal),+ $(,)?) => {
        const RETIRED_SYSCALLS: &[(u64, &str)] = &[$(($num, $name)),+];

        const _: () = {
            let mut i = 1;
            while i < RETIRED_SYSCALLS.len() {
                assert!(
                    RETIRED_SYSCALLS[i - 1].0 < RETIRED_SYSCALLS[i].0,
                    "the retired-syscall table is not strictly ascending, so a \
                     number is retired twice or the list is unreadable",
                );
                i += 1;
            }
        };

        fn retired_syscall(num: u64) -> Option<&'static str> {
            RETIRED_SYSCALLS.iter().find(|(n, _)| *n == num).map(|(_, name)| *name)
        }
    };
}

retired_syscalls! {
    29 => "SYS_SEND_MSG",
    30 => "SYS_RECV_MSG",
    32 => "SYS_REGISTER_NAME",
    33 => "SYS_FIND_PID",
}

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

/// One kernel heap allocation of `bytes` at `align`, taken and released.
/// `SYS_DEBUG` actions 5, 6 and 7 are its only callers.
///
/// Raw `alloc`/`dealloc` rather than a `Vec` that is immediately dropped:
/// LLVM is allowed to delete a malloc/free pair whose result is never
/// observed, and an actuator the optimiser can remove certifies nothing. The
/// null return is reported rather than unwrapped for the same reason — a
/// refusal and a success have to be distinguishable from userland.
#[cfg(feature = "test-heap-ceiling")]
fn debug_heap_alloc(bytes: usize, align: usize) -> u64 {
    let Ok(layout) = core::alloc::Layout::from_size_align(bytes, align) else {
        return SyscallError::InvalidArgument.to_u64();
    };
    let p = unsafe { alloc::alloc::alloc(layout) };
    if p.is_null() {
        return SyscallError::ResourceExhausted.to_u64();
    }
    unsafe { core::ptr::write_volatile(p, 1u8) };
    unsafe { alloc::alloc::dealloc(p, layout) };
    0
}

/// Sixteen bytes of kernel memory a test can name, and ask about afterwards.
///
/// A guest cannot read the kernel's address space, so a write that lands there
/// is invisible to every assertion a test can make from userland — which is
/// exactly the write `SYS_DLOPEN`'s `init_out` used to allow, and a gate that
/// could only check the syscall's *verdict* would pass against a kernel that
/// still made it. Nothing here is faked: the address is this static's own, the
/// write a broken kernel makes is a real one, and what is read back is the
/// memory itself.
#[cfg(feature = "test-kernel-canary")]
mod canary {
    use core::sync::atomic::{AtomicU64, Ordering};

    const VALUE: [u64; 2] = [0x_C0DE_1A55_0F17_1E55, 0x_5EE7_A11_0F_17_00];

    static WORDS: [AtomicU64; 2] =
        [AtomicU64::new(VALUE[0]), AtomicU64::new(VALUE[1])];

    /// The direct map is where the kernel's own statics live, so this is an
    /// address in it — the half `AddressSpace::translate` must refuse.
    pub fn address() -> u64 {
        WORDS.as_ptr() as u64
    }

    pub fn changed() -> bool {
        [WORDS[0].load(Ordering::Relaxed), WORDS[1].load(Ordering::Relaxed)] != VALUE
    }
}

pub fn init() {
    let efer = cpu::rdmsr(MSR_EFER);
    cpu::wrmsr(MSR_EFER, efer | 1);

    let star = ((gdt::STAR_SYSRET_BASE as u64) << 48) | ((gdt::KERNEL_CS as u64) << 32);
    cpu::wrmsr(MSR_STAR, star);
    // `LSTAR` is an IDT slot by another name: the one thing `syscall` can reach.
    cpu::wrmsr(MSR_LSTAR, Ring3Entry::new(syscall_entry).addr());
    cpu::wrmsr(MSR_FMASK, 0x40200); // mask IF (bit 9) + AC (bit 18) on SYSCALL entry
}

// Syscall entry: GS permanently points to kernel per-CPU data (no swapgs needed).
// PerCpu layout: offset 16 = kernel_rsp, offset 24 = user_rsp.
//
// The bracket spans the handler *and* the exit-to-user epilogue, because both
// can context-switch. The epilogue used to run with the user state already put
// back, so a switch there returned to Ring 3 carrying whatever the task that
// ran in between had left in the registers — `specs/user-machine-state.md` §3.
#[unsafe(naked)]
extern "sysv64" fn syscall_entry() {
    ring3_naked_asm!(
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

        save_user_state!(),

        "lock add dword ptr gs:[240], 1",   // preempt_count++

        "call {handler}",

        "lock sub dword ptr gs:[240], 1",   // preempt_count--
        // cli before exit_to_user and pop rsp / sysretq: an interrupt after
        // pop rsp would land on the user RSP as a kernel stack. Helper
        // preserves IF=0 across its return.
        "cli",
        // exit_to_user runs BEFORE restoring user GPRs — the sysv64 call
        // would otherwise clobber rcx/r11 (sysretq RIP/RFLAGS) and the
        // restored arg regs. The 16 bytes park the syscall return value and
        // keep rsp aligned for the call, which the bracket left it.
        "sub rsp, 16",
        "mov [rsp], rax",
        "call {exit_to_user}",
        "mov rax, [rsp]",
        "add rsp, 16",

        restore_user_state!(),

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
            let Some(buf) = ctx.user_bytes(UserAddr::new(a2), a3) else { return bad_addr };
            sys_write(a1 as u32, &buf)
        }
        SYS_READ => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a2), a3) else { return bad_addr };
            sys_read(a1 as u32, &mut buf)
        }
        SYS_THREAD_EXIT => sys_thread_exit(a1 as i32),
        SYS_RANDOM => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_random(&mut buf)
        }
        SYS_CLOCK => crate::clock::nanos_since_boot(),
        SYS_OPEN => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_open(&path, OpenFlags(a3))
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
            let mut stat = fd::Stat { file_type: 0, size: 0, mtime: 0 };
            if !process::with_fd_owner_data(|data| fd::fstat(&data.fds, a1 as u32, &mut stat)) {
                return SyscallError::NotFound.to_u64();
            }
            match ctx.copy_out(UserAddr::new(a2), &stat) {
                Ok(()) => 0,
                Err(e) => e.to_u64(),
            }
        }
        SYS_FSYNC => process::with_fd_owner_data(|data| fd::fsync(&mut data.fds, a1 as u32)),
        SYS_READDIR => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a3), a4) else { return bad_addr };
            sys_readdir(&path, &mut buf)
        }
        SYS_DELETE => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_delete(&path)
        }
        SYS_SHUTDOWN => {
            log!("Syncing filesystems...");
            crate::vfs::lock().sync_all();
            log!("Shutting down.");
            // After that line and before power goes: on a machine with no
            // serial port these last two lines exist nowhere but the ring, and
            // `acpi::shutdown` does not come back.
            crate::log_file::flush_final();
            acpi::shutdown();
        }
        SYS_CHDIR => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_chdir(&path)
        }
        SYS_GETCWD => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_getcwd(&mut buf)
        }
        SYS_PIPE => sys_pipe(),
        SYS_SPAWN => {
            let Ok(args) = ctx.copy_in::<SpawnArgs>(UserAddr::new(a1)) else { return bad_addr };
            let text = match ctx.user_str(UserAddr::new(args.argv_ptr), args.argv_len) { Ok(s) => s, Err(e) => return e.to_u64() };
            let fd_count = args.fd_map_count as usize;
            let fds = if fd_count > 0 {
                // One pair read out of the window at a time rather than the map
                // copied wholesale: `fd_map_count` is userland's, and a copy
                // would put it on the allocator for a loop that reads each pair
                // exactly once.
                let Some(bytes) = fd_count
                    .checked_mul(crate::loader::FD_PAIR_LEN)
                    .and_then(|len| ctx.user_bytes(UserAddr::new(args.fd_map_ptr), len as u64))
                else {
                    return bad_addr;
                };
                match process::build_child_fds(&bytes) {
                    Ok(fds) => fds,
                    Err(e) => return e.to_u64(),
                }
            } else {
                fd::FdTable::new()
            };
            // The env blob is kept for the child's whole life, so it needs a
            // bound of its own — `user_vec` is the one accessor that puts a
            // userland-chosen size on the allocator. Same constant as argv:
            // both are userland text the kernel owns a copy of.
            let env = if args.env_len > 0 {
                if args.env_len > crate::user_ptr::MAX_USER_STR {
                    return SyscallError::InvalidArgument.to_u64();
                }
                match ctx.user_vec(UserAddr::new(args.env_ptr), args.env_len) {
                    Ok(bytes) => bytes,
                    Err(e) => return e.to_u64(),
                }
            } else {
                alloc::vec::Vec::new()
            };
            sys_spawn(&text, fds, env)
        }
        SYS_WAITPID => sys_waitpid(a1, a2),

        SYS_MARK_TTY => process::with_fd_owner_data(|data| fd::mark_tty(&mut data.fds, a1 as u32)),
        SYS_OPEN_DEVICE => sys_open_device(a1),
        // Display integrity, not memory access: framebuffer *contents* are
        // behind shared_memory grants either way. Ungated, any process could
        // scan out over the compositor's frames and move the cursor.
        SYS_GPU_PRESENT | SYS_GPU_SET_CURSOR | SYS_GPU_MOVE_CURSOR => {
            if !device::is_owner(device::DeviceType::Framebuffer, process::current_process()) {
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
        // Both answer out of the anchor `clock` took at boot, so neither
        // touches the CMOS: this used to be a port handshake per call that
        // could block on the update flag for as long as a second, which made
        // `SystemTime::now()` in a loop pathological. `NotSupported` is a
        // machine that never said what time it is — for the life of this boot
        // it does not support being asked, and the alternative is serving a
        // number from 1970 that a caller cannot tell from a real one.
        //
        // Local time in the first and UTC in the second, which is what each
        // has always claimed to be: the wall clock on a screen wants the
        // machine's own zone, and seconds since the epoch are UTC by
        // definition.
        SYS_CLOCK_REALTIME => crate::clock::local_secs().map_or(
            SyscallError::NotSupported.to_u64(),
            |secs| {
                let now = crate::clock::Civil::from_unix_secs(secs);
                (now.hour << 16) | (now.min << 8) | now.sec
            },
        ),
        SYS_CLOCK_EPOCH => {
            crate::clock::utc_secs().map_or(SyscallError::NotSupported.to_u64(), |secs| secs)
        }
        SYS_SYSINFO => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_sysinfo(&mut buf)
        }
        SYS_NANOSLEEP => sys_nanosleep(a1),
        SYS_DUP => sys_dup(a1 as u32),
        SYS_DUP2 => sys_dup2(a1 as u32, a2 as u32),
        SYS_GETPID => process::current_process().raw() as u64,
        SYS_RENAME => {
            let old = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            let new = match ctx.user_str(UserAddr::new(a3), a4) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_rename(&old, &new)
        }
        SYS_MKDIR => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_mkdir(&path)
        }
        SYS_RMDIR => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_rmdir(&path)
        }
        SYS_DLOPEN => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            // Refused here rather than at the write, so a process that named an
            // address the kernel will not write to is not left holding a
            // library it was never told about.
            let init_out = match a3 {
                0 => None,
                raw => match UserAddr::checked(raw) {
                    Some(addr) => Some(addr),
                    None => return bad_addr,
                },
            };
            sys_dlopen(&ctx, &path, init_out)
        }
        SYS_DLSYM => {
            let name = match ctx.user_str(UserAddr::new(a2), a3) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_dlsym(a1, &name)
        }
        SYS_DLCLOSE => 0,
        SYS_FTRUNCATE => process::with_fd_owner_data(|data| fd::ftruncate(&mut data.fds, a1 as u32, a2)),
        SYS_STACK_INFO => {
            let stack = process::with_current_data(|data| {
                (data.user_stack_base.raw() > 0)
                    .then_some((data.user_stack_base.raw(), data.user_stack_size))
            });
            let Some((base, size)) = stack else { return SyscallError::NotFound.to_u64() };
            match ctx
                .copy_out(UserAddr::new(a1), &base)
                .and_then(|()| ctx.copy_out(UserAddr::new(a2), &size))
            {
                Ok(()) => 0,
                Err(e) => e.to_u64(),
            }
        }
        SYS_CPU_COUNT => super::smp::cpu_count() as u64,
        SYS_FUTEX_WAIT => match UserAddr::checked(a1) {
            Some(addr) => process::futex_wait(addr, a2 as u32, a3),
            None => bad_addr,
        },
        SYS_FUTEX_WAKE => match UserAddr::checked(a1) {
            Some(addr) => process::futex_wake(addr, a2),
            None => bad_addr,
        },
        SYS_MMAP => sys_mmap(a1, a2, MmapProt(a3), MmapFlags(a4)),
        SYS_MUNMAP => sys_munmap(a1, a2),
        SYS_KILL => process::kill_process(process::Pid::from_raw(a1 as u32)),
        SYS_READ_NONBLOCK => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a2), a3) else { return bad_addr };
            sys_read_nonblock(a1 as u32, &mut buf)
        }
        SYS_WRITE_NONBLOCK => {
            let Some(buf) = ctx.user_bytes(UserAddr::new(a2), a3) else { return bad_addr };
            sys_write_nonblock(a1 as u32, &buf)
        }
        SYS_PIPE_OPEN => sys_pipe_open(a1, a2),
        SYS_PIPE_ID => sys_pipe_id(a1 as u32),
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_GET_ENV => {
            let env = process::with_fd_owner_data(|d| d.env.clone());
            if a2 == 0 {
                env.len() as u64
            } else {
                let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a1), a2) else { return bad_addr };
                let copy_len = env.len().min(buf.len());
                buf.write_at(0, &env[..copy_len]);
                copy_len as u64
            }
        }
        SYS_SOCKET_CREATE => sys_socket_create(a1, a2),
        SYS_PIPE_MAP => sys_pipe_map(a1 as u32),
        // Both address the NIC by ambient authority, so without this any
        // process could pop frames out of the used ring before netd sees them
        // and, by never refilling, exhaust all 256 RX slots.
        SYS_NIC_RX_POLL => {
            if !device::is_owner(device::DeviceType::Nic, process::current_process()) {
                return SyscallError::PermissionDenied.to_u64();
            }
            match crate::net::poll_rx() {
                Some((buf_idx, frame_len)) => ((buf_idx as u64) << 16) | (frame_len as u64),
                None => 0,
            }
        }
        SYS_NIC_RX_DONE => {
            if !device::is_owner(device::DeviceType::Nic, process::current_process()) {
                return SyscallError::PermissionDenied.to_u64();
            }
            crate::net::refill_rx_buf(a1 as usize).map_or_else(|e| e.to_u64(), |()| 0)
        }
        SYS_NIC_TX => {
            if !device::is_owner(device::DeviceType::Nic, process::current_process()) {
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
            sys_symlink(&target, &link)
        }
        SYS_READLINK => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a3), a4) else { return bad_addr };
            sys_readlink(&path, &mut buf)
        }
        SYS_GPU_SET_RESOLUTION => {
            // Checked before the driver, so a non-claimant never gets its two
            // arbitrary u32s turned into a contiguous physical allocation.
            let pid = process::current_process();
            if !device::is_owner(device::DeviceType::Framebuffer, pid) {
                return SyscallError::PermissionDenied.to_u64();
            }
            // Checked before the allocation for the same reason the ownership
            // is: a caller that named an address the kernel will not write to
            // must not be left with a resolution it is never told about.
            let Some(info_out) = UserAddr::checked(a3) else { return bad_addr };
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
                    match ctx.copy_out(info_out, &fb_info) {
                        Ok(()) => 0,
                        Err(e) => e.to_u64(),
                    }
                }
                Err(e) => e.to_u64(),
            }
        }
        SYS_LISTEN => {
            let name = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_listen(&name)
        }
        SYS_ACCEPT => sys_accept(a1 as u32),
        SYS_CONNECT => {
            let name = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_connect(&name)
        }
        SYS_TLS_ALLOC_BLOCK => sys_tls_alloc_block(a1),
        SYS_IO_URING_SETUP => sys_io_uring_setup(a1 as u32),
        SYS_IO_URING_ENTER => sys_io_uring_enter(a1 as u32, a2 as u32, a3 as u32, a4),
        SYS_QUERY_MODULES => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_query_modules(&mut buf)
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
            // A real double fault, produced the way the hardware produces one:
            // fault while RSP cannot be pushed to. The push below raises #SS on
            // a non-canonical stack address, and delivering *that* needs another
            // push to the same RSP, which is the #DF condition. Nothing
            // simulated — the point is to run the IST1 stack, and only the CPU
            // can put us there.
            //
            // Non-canonical rather than merely unmapped, because "unmapped" is
            // a claim about this machine's memory map that a bigger machine
            // falsifies quietly: an address inside the direct map would simply
            // be written to, and the test would pass having faulted nothing.
            // Only #DF has an IST, so every other vector on the way is
            // delivered onto this same unusable stack.
            #[cfg(feature = "test-double-fault")]
            4 => {
                log!("SYS_DEBUG: provoking a double fault");
                unsafe {
                    core::arch::asm!(
                        "mov rsp, {bad}",
                        "push 0",
                        bad = in(reg) 0x0000_8000_0000_0000u64,
                        options(noreturn),
                    );
                }
            }
            // Both sides of `mm::MAX_HEAP_ALLOC`, and the alignment corner
            // between them. 5 must succeed, 6 must panic, and 7 — the same
            // size page-aligned, which `memalign` pads past what one page can
            // back — must come back as an error rather than as a panic taken
            // inside the allocator's lock, which is what it used to be.
            #[cfg(feature = "test-heap-ceiling")]
            5 => debug_heap_alloc(crate::mm::MAX_HEAP_ALLOC, 8),
            #[cfg(feature = "test-heap-ceiling")]
            6 => debug_heap_alloc(crate::mm::PAGE_2M as usize, 8),
            #[cfg(feature = "test-heap-ceiling")]
            7 => debug_heap_alloc(crate::mm::MAX_HEAP_ALLOC, 4096),
            // Returns, unlike every other action here: what is under test is
            // what the *console* does next, so the machine and the process
            // both have to survive being drawn over.
            #[cfg(feature = "test-screen-graffiti")]
            8 => {
                crate::drivers::panic_console::graffiti();
                0
            }
            // A read, not a write: the property under test is that the page is
            // absent, and a read establishes it without the feature also
            // handing userland a kernel store. Returning 0 is the failure —
            // without a guard page that byte is dlmalloc's bookkeeping for the
            // chunk the idle stack lives in, and the read succeeds.
            #[cfg(feature = "test-idle-guard")]
            9 => {
                let addr = super::percpu::idle_guard_byte();
                log!("SYS_DEBUG: reading the idle stack guard at {addr:#x}");
                unsafe { core::ptr::read_volatile(addr as *const u8) };
                0
            }
            #[cfg(feature = "test-kernel-canary")]
            10 => canary::address(),
            #[cfg(feature = "test-kernel-canary")]
            11 => canary::changed() as u64,
            // Make the last CPU a shootdown waits for answer `a2` nanoseconds
            // late, take one, and answer with how long it took. The number is
            // the gate: an initiator that does not wait measures roughly the
            // cost of one ICR write however slow its siblings are. The arming
            // outlives the call, so the caller can then time an ordinary
            // syscall and learn whether *its* free path shoots down.
            #[cfg(feature = "test-tlb-ack-delay")]
            12 => crate::arch::tlb::debug_arm_ack_delay(a2),
            #[cfg(feature = "test-tlb-ack-delay")]
            13 => crate::arch::tlb::debug_disarm_ack_delay(),
            _ => SyscallError::InvalidArgument.to_u64(),
        },
        SYS_SCHED_INFO => match ctx.copy_out(UserAddr::new(a1), &sys_sched_info()) {
            Ok(()) => 0,
            Err(e) => e.to_u64(),
        },
        SYS_PROCESS_STATS => {
            let stats_size = core::mem::size_of::<toyos_abi::syscall::ProcessStats>() as u64;
            if a3 < stats_size { return SyscallError::InvalidArgument.to_u64(); }
            let Some(addr) = UserAddr::checked(a2) else { return bad_addr };
            sys_process_stats(&ctx, process::Pid::from_raw(a1 as u32), addr)
        },
        SYS_SET_THREAD_NAME => {
            let len = (a2 as usize).min(process::THREAD_NAME_LEN);
            let Some(bytes) = ctx.user_bytes(UserAddr::new(a1), len as u64) else {
                return bad_addr;
            };
            let mut name = [0u8; process::THREAD_NAME_LEN];
            bytes.read_at(0, &mut name[..len]);
            process::set_current_thread_name(&name[..len]);
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
            let me = process::current_process();
            if !device::is_owner(device::DeviceType::VirtioSound, me)
                && !device::is_owner(device::DeviceType::HdaAudio, me)
            {
                return SyscallError::PermissionDenied.to_u64();
            }
            crate::scheduler::set_current_rt(a1 != 0);
            0
        },
        SYS_DEVICE_REG_READ => sys_device_reg(a1 as u32, a2, a3, None),
        SYS_DEVICE_REG_WRITE => sys_device_reg(a1 as u32, a2, a3, Some(a4)),
        // A number a deleted syscall used is retired, never reused, so an old
        // binary is told which call it is asking for rather than that its
        // number is nonsense.
        _ => match retired_syscall(num) {
            Some(name) => {
                crate::log!("syscall {num} is retired (formerly {name})");
                SyscallError::NotSupported.to_u64()
            }
            None => SyscallError::InvalidArgument.to_u64(),
        },
    };

    // The first of the object layer's three drain sites. Here rather than at
    // the drop that queued it: a hook must not run under whatever guard the
    // syscall was holding when the last handle went (`object::drain_zero_handles`).
    crate::object::drain_zero_handles();

    // Track wall-clock syscall time (includes preemption — see plan for documented limitation)
    let elapsed = crate::clock::nanos_since_boot() - t0;
    process::with_current_data(|data| {
        data.syscall_total_ns += elapsed;
    });

    result
}

fn sys_write(fd_num: u32, buf: &UserBytes) -> u64 {
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
    VirtioSound,
    Hda,
    Keyboard(u64),
}

/// Which stub a claimed fd names, and nothing about what it drives.
enum RegTarget {
    Hda,
    VirtioSound,
}

/// One register of a claimed device, read or written.
///
/// The fd is the authorization and the device behind it owns the allow-list, so
/// this function knows nothing about codecs or virtqueues — which is the test
/// `specs/hda-driver-plan.md` §4.4 sets for it being a device-register call
/// rather than a device protocol back in the syscall table. Two stubs answer it
/// now, which is the first evidence for that claim rather than a restatement of
/// it.
fn sys_device_reg(fd_num: u32, offset: u64, width: u64, value: Option<u64>) -> u64 {
    let Some(width) = toyos_abi::syscall::RegWidth::from_raw(width) else {
        return SyscallError::InvalidArgument.to_u64();
    };
    let target = process::with_fd_owner_data(|data| match data.fds.get(fd_num) {
        Some(fd::Descriptor::Hda { .. }) => Some(RegTarget::Hda),
        Some(fd::Descriptor::VirtioSound { .. }) => Some(RegTarget::VirtioSound),
        _ => None,
    });
    let Some(target) = target else {
        return SyscallError::NotFound.to_u64();
    };
    match value {
        None => {
            let read = match target {
                RegTarget::Hda => crate::drivers::hda::reg_read(offset, width),
                RegTarget::VirtioSound => crate::drivers::virtio_sound::reg_read(offset, width),
            };
            match read {
                Ok(v) => v as u64,
                Err(e) => e.to_u64(),
            }
        }
        Some(value) if value <= u32::MAX as u64 => {
            let written = match target {
                RegTarget::Hda => crate::drivers::hda::reg_write(offset, width, value as u32),
                RegTarget::VirtioSound => {
                    crate::drivers::virtio_sound::reg_write(offset, width, value as u32)
                }
            };
            match written {
                Ok(()) => 0,
                Err(e) => e.to_u64(),
            }
        }
        Some(_) => SyscallError::InvalidArgument.to_u64(),
    }
}

fn sys_read(fd_num: u32, buf: &mut UserBytesMut) -> u64 {
    loop {
        let action = process::with_fd_owner_data(|data| {
            match fd::try_read(&mut data.fds, fd_num, buf) {
                Some(n) => {
                    let pipe_id = data.fds.get(fd_num).and_then(|d| d.pipe_id_read());
                    Ok((n, pipe_id))
                }
                None => {
                    let desc = data.fds.get(fd_num);
                    if matches!(desc, Some(fd::Descriptor::Keyboard(_))) {
                        Err(Some(ReadBlock::Keyboard(0)))
                    } else if matches!(
                        desc,
                        Some(fd::Descriptor::VirtioSound { info_read: true, .. })
                    ) {
                        Err(Some(ReadBlock::VirtioSound))
                    } else if matches!(desc, Some(fd::Descriptor::Hda { info_read: true, .. })) {
                        Err(Some(ReadBlock::Hda))
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
            Err(Some(ReadBlock::VirtioSound)) => crate::scheduler::wait_until(
                &crate::sched::waitqs::AUDIO,
                0,
                crate::drivers::virtio_sound::has_pending,
            ),
            Err(Some(ReadBlock::Hda)) => crate::scheduler::wait_until(
                &crate::sched::waitqs::AUDIO,
                0,
                crate::drivers::hda::has_pending,
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

/// Whether `flags` ask for anything that can change what is on the volume.
///
/// `WRITE` alone is not the question: `CREATE` makes a file, `TRUNCATE`
/// destroys one's contents, and `APPEND` is a write position. A read-only open
/// of a `KernelOnly` mount is fine and stays fine — the fd it hands back has
/// `writable` false, so nothing downstream needs a second check.
fn open_modifies(flags: OpenFlags) -> bool {
    flags.contains(OpenFlags::WRITE)
        || flags.contains(OpenFlags::CREATE)
        || flags.contains(OpenFlags::TRUNCATE)
        || flags.contains(OpenFlags::APPEND)
}

fn sys_open(path: &str, flags: OpenFlags) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let resolved = vfs::lock().resolve_absolute(&cwd, path);
    if open_modifies(flags) && !vfs::lock().user_may_modify(&resolved) {
        return SyscallError::PermissionDenied.to_u64();
    }
    process::with_fd_owner_data(|data| fd::open(&mut data.fds, &resolved, flags))
}

fn sys_close(fd_num: u32) -> u64 {
    let (result, wake_readers, wake_writers) = process::with_fd_owner_data(|data| {
        // Grab pipe IDs before close drops the descriptor
        let wake_r = data.fds.get(fd_num).and_then(|d| d.pipe_id_write()); // writer closed → wake readers
        let wake_w = data.fds.get(fd_num).and_then(|d| d.pipe_id_read());  // reader closed → wake writers
        let r = fd::close(&mut data.fds, fd_num, &mut data.pipe_maps);
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

fn sys_random(out: &mut UserBytesMut) -> u64 {
    let mut i = 0;
    while i + 8 <= out.len() {
        out.write_at(i, &cpu::rdrand().to_ne_bytes());
        i += 8;
    }
    let remaining = out.len() - i;
    if remaining > 0 {
        let bytes = cpu::rdrand().to_ne_bytes();
        out.write_at(i, &bytes[..remaining]);
    }
    0
}

/// Encode a directory listing into `buf`; return the length it *needs*.
///
/// Same contract as `sys_getcwd`, for the same reason and after the same
/// defect: this used to fill the buffer, stop, and report the bytes it had
/// written, which is indistinguishable from a complete listing. Measured
/// before the change: `std::fs::read_dir` reported **4125** entries of
/// **34,816**, as success. A caller enumerating a directory to delete it, or
/// to check a name is absent, acts on that.
///
/// So the listing is written only when all of it fits, and the return is the
/// size either way: `n <= buf.len()` means the entries are in the buffer,
/// `n > buf.len()` means nothing was written and `n` is what to allocate.
/// Refusing to write a partial answer is the point — a caller that ignores
/// the return still gets zeroes rather than a plausible short listing.
fn sys_readdir(path: &str, out: &mut UserBytesMut) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let entries = match vfs::lock().list(&cwd, path) {
        Ok(e) => e,
        Err(e) => return e.to_u64(),
    };

    // A directory name is stored with its trailing slash and encoded without.
    let encoded = |name: &alloc::string::String| 1 + name.trim_end_matches('/').len() + 1 + 8;
    let needed: usize = entries.iter().map(|(name, _)| encoded(name)).sum();
    if needed > out.len() {
        return needed as u64;
    }

    let mut pos = 0;
    for (name, size) in &entries {
        let is_dir = name.ends_with('/');
        let clean_name = if is_dir { &name[..name.len() - 1] } else { name.as_str() };
        out.write_at(pos, &[if is_dir { 2 } else { 1 }]);
        pos += 1;
        out.write_at(pos, clean_name.as_bytes());
        pos += clean_name.len();
        out.write_at(pos, &[0]);
        pos += 1;
        out.write_at(pos, &size.to_le_bytes());
        pos += 8;
    }
    debug_assert_eq!(pos, needed);
    pos as u64
}

fn sys_delete(path: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    if !vfs.user_may_modify(&resolved) {
        return SyscallError::PermissionDenied.to_u64();
    }
    match vfs.delete(&resolved) {
        Ok(()) => 0,
        Err(e) => e.to_u64(),
    }
}

fn sys_chdir(path: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    match vfs::lock().cd(&cwd, path) {
        Ok(new_cwd) => {
            process::with_fd_owner_data(|d| d.cwd = new_cwd);
            0
        }
        Err(e) => e.to_u64(),
    }
}

/// Copy the cwd into `buf`; return the length the cwd *needs*.
///
/// The return is the required length, not the number of bytes written, so a
/// caller compares it against the buffer it passed: `n <= buf.len()` means the
/// path is in the buffer, `n > buf.len()` means nothing was written and `n` is
/// the size to allocate before retrying.
///
/// That distinction is the whole point. The old contract returned
/// `min(cwd.len(), buf.len())` and wrote a prefix, so "fit exactly" and
/// "silently truncated" were the same answer — and `std::env::current_dir`,
/// which passes a fixed 256-byte buffer, handed back a *different, valid-
/// looking* path for any longer cwd. A wrong answer that looks right is worse
/// than an error: it propagates into every path the program derives from it.
///
/// Nothing is written when the buffer is too small. A partial path names the
/// wrong directory, and leaving one in the caller's buffer invites its use.
///
/// An empty buffer is therefore a size query, which falls out rather than
/// being bolted on: the dispatch hands `user_bytes_mut` a zero length back as
/// an empty window, so `getcwd(NULL, 0)` reports the length and touches nothing.
///
/// `vfs::MAX_PATH` bounds `cwd`, so the required length is always far below the
/// range `SyscallError` encodes and can never be misread as one.
fn sys_getcwd(out: &mut UserBytesMut) -> u64 {
    process::with_fd_owner_data(|data| {
        let cwd = data.cwd.as_bytes();
        if cwd.len() <= out.len() {
            out.write_at(0, cwd);
        }
        cwd.len() as u64
    })
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
            fd::close(&mut data.fds, read_fd, &mut data.pipe_maps);
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
///
/// Takes the caller's table rather than locking it: this runs inside the
/// `open_reader`/`open_writer` acquisition of `PIPES`, and both callers already
/// hold the table across that.
fn may_open_pipe(
    caller: process::Pid,
    creator: process::Pid,
    id: pipe::PipeId,
    fds: &fd::FdTable,
) -> bool {
    if caller == creator {
        return true;
    }
    fds.iter().any(|(_, d)| {
        d.pipe_id_read() == Some(id)
            || d.pipe_id_write() == Some(id)
            || matches!(d, fd::Descriptor::Socket { peer, .. } if *peer == creator)
    })
}

fn not_opened(e: pipe::NotOpened) -> u64 {
    match e {
        pipe::NotOpened::NoSuchPipe => SyscallError::NotFound.to_u64(),
        pipe::NotOpened::NotPermitted => SyscallError::PermissionDenied.to_u64(),
    }
}

fn sys_pipe_open(pipe_id: u64, mode: u64) -> u64 {
    let id = pipe::PipeId::from_raw(pipe_id as usize);
    let caller = process::current_process();
    if mode > 1 {
        return SyscallError::InvalidArgument.to_u64();
    }
    // The whole syscall under one acquisition of the caller's table, so the
    // descriptors `may_open_pipe` weighs are the descriptors the new one joins.
    process::with_fd_owner_data(|data| {
        let permitted = |creator| may_open_pipe(caller, creator, id, &data.fds);
        let descriptor = if mode == 0 {
            pipe::open_reader(id, permitted).map(fd::Descriptor::PipeRead)
        } else {
            pipe::open_writer(id, permitted).map(fd::Descriptor::PipeWrite)
        };
        match descriptor {
            Ok(d) => fd_result(data.fds.insert(d)),
            Err(e) => not_opened(e),
        }
    })
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

/// Map a pipe's ring page into the caller.
///
/// The window is recorded against the pipe (`process::PipeMap`) so that
/// closing the last descriptor for it takes the mapping away. It used to be
/// recorded nowhere: `SYS_PIPE`, `SYS_PIPE_MAP`, close both fds freed the ring
/// page back to the PMM with the caller's writable mapping of it still live,
/// and whatever the PMM handed that page to next — another process's pipe, a
/// kernel heap region, a DMA buffer — was readable and writable by a process
/// that owned nothing.
///
/// A second call for the same pipe returns the window the first one made,
/// rather than a second window onto the same page. That is what keeps
/// `pipe_maps` bounded by the descriptor table.
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
        if let Some(existing) = data.pipe_maps.iter().find(|m| m.pipe == pipe_id) {
            return existing.addr.raw();
        }
        let Some(phys) = pipe::map_page(pipe_id) else {
            return SyscallError::ResourceExhausted.to_u64();
        };
        let pt = crate::scheduler::current_address_space()
            .expect("sys_pipe_map: no address space");
        let Some((vaddr, _aligned)) = process::vma_map(&pt, phys.phys(), pipe::PIPE_SIZE as u64) else {
            return SyscallError::ResourceExhausted.to_u64();
        };
        data.pipe_maps.push(process::PipeMap { pipe: pipe_id, addr: vaddr });

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
    let peer = process::current_process();
    process::with_fd_owner_data(|data| {
        let rx = pipe::open_reader(rx_id, |_| {
            data.fds.iter().any(|(_, d)| d.pipe_id_read() == Some(rx_id))
        });
        let rx = match rx {
            Ok(rx) => rx,
            Err(e) => return not_opened(e),
        };
        // A refusal here drops `rx`, which gives its count straight back. That
        // is the whole reason the two ends need not be taken together.
        let tx = pipe::open_writer(tx_id, |_| {
            data.fds.iter().any(|(_, d)| d.pipe_id_write() == Some(tx_id))
        });
        let tx = match tx {
            Ok(tx) => tx,
            Err(e) => return not_opened(e),
        };
        fd_result(data.fds.insert(fd::Descriptor::Socket { rx, tx, peer }))
    })
}

fn sys_read_nonblock(fd_num: u32, buf: &mut UserBytesMut) -> u64 {
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

fn sys_write_nonblock(fd_num: u32, buf: &UserBytes) -> u64 {
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
    let Some(class) = device::class_of(device_type) else {
        return SyscallError::InvalidArgument.to_u64();
    };
    let pid = process::current_process();
    // NotFound means the machine has no such device and nothing else, because
    // that is the one answer a daemon is entitled to degrade on. soundd now
    // routes NotFound to a null sink, so collapsing Owned into it is worse than
    // before: soundd would silently discard all audio whenever another process
    // held the claim, instead of failing loudly on a real conflict.
    let desc = match device::try_claim(class, pid) {
        Ok(d) => d,
        Err(device::ClaimError::Absent) => return SyscallError::NotFound.to_u64(),
        Err(device::ClaimError::Owned) => return SyscallError::AlreadyExists.to_u64(),
        Err(device::ClaimError::GrantFailed) => return SyscallError::ResourceExhausted.to_u64(),
    };
    // A refused insert drops the descriptor, which gives the claim straight
    // back — the device is not left owned by a process that got no fd for it.
    process::with_fd_owner_data(|data| fd_result(data.fds.insert(desc)))
}

// Service IPC: listen / accept / connect

fn sys_listen(name: &str) -> u64 {
    let Some(listener) = crate::listener::listen(name, process::current_process()) else {
        return SyscallError::AlreadyExists.to_u64();
    };
    // A refused insert drops the reference, which unbinds the name again.
    process::with_fd_owner_data(|data| fd_result(data.fds.insert(fd::Descriptor::Listener(listener))))
}

fn sys_accept(fd_num: u32) -> u64 {
    let listener_id = process::with_fd_owner_data(|data| {
        match data.fds.get(fd_num) {
            Some(fd::Descriptor::Listener(l)) => Some(l.id()),
            _ => None,
        }
    });
    let Some(listener_id) = listener_id else {
        return SyscallError::InvalidArgument.to_u64();
    };

    loop {
        if let Some(conn) = crate::listener::pop_connection(listener_id) {
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

    // The client's own end first. Installing it can fail on a full fd table,
    // and a connection queued for a server whose client never got a
    // descriptor is one the server accepts and finds already dead.
    let fd = match process::with_fd_owner_data(|data| {
        data.fds.insert(fd::Descriptor::Socket {
            rx: sc_reader,   // client reads from server→client
            tx: cs_writer,   // client writes to client→server
            peer: server_pid,
        })
    }) {
        Ok(fd) => fd,
        Err(e) => return e.to_u64(),
    };

    // Queue the server's end. PipeReader/PipeWriter in the queue keep pipes
    // alive even if the client disconnects before accept — which is also why
    // the queue needs a depth, and this return value used to be discarded.
    let queued = crate::listener::push_connection(name, listener::PendingConnection {
        rx: cs_reader,   // server reads from client→server
        tx: sc_writer,   // server writes to server→client
        client_pid,
    });
    if let Err(e) = queued {
        process::with_fd_owner_data(|data| {
            fd::close(&mut data.fds, fd, &mut data.pipe_maps);
        });
        return match e {
            listener::PushError::NoListener => SyscallError::NotFound.to_u64(),
            listener::PushError::QueueFull => SyscallError::ResourceExhausted.to_u64(),
        };
    }
    wake_poll_waiters(name);
    fd as u64
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
    let target = process::Pid::from_raw(target_pid as u32);

    // `target` is a raw integer from userland and `allowed` is a kernel `Vec`
    // that grows one entry per accepted grant, so an unchecked target is an
    // unbounded allocation any process can drive by counting: `Pid`s never
    // repeat, so no two of them collapse. Requiring the target to name a
    // process the table knows bounds `allowed` by the number of processes
    // that have ever been alive at once. Taken before the region lock, so the
    // two are never held together.
    let target_known = {
        let guard = process::PROCESS_TABLE.lock();
        guard.as_ref().is_some_and(|table| table.get(target).is_some())
    };
    if !target_known {
        return SyscallError::InvalidArgument.to_u64();
    }

    match shared_memory::grant(token, pid, target) {
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

/// Map anonymous memory, honouring `prot`.
///
/// `prot` used to be `_prot`: every mapping was readable and writable whatever
/// the caller asked for, so `userland/libc`'s translation of POSIX
/// `PROT_NONE` produced a writable guard page and the stack-overflow detection
/// built on it silently did not exist.
///
/// With 2 MiB pages and no `mprotect`, protection is decided once, here. A
/// mapping without `WRITE` gets a read-only PDE, and `MmapProt::NONE` gets no
/// PDE at all: the range is reserved so nothing else lands in it, no physical
/// memory is pinned behind a page whose purpose is to fault, and
/// `process::handle_page_fault` refuses to fill a `RegionKind::Mapped` region
/// so the reservation cannot be demand-paged back into existence.
fn sys_mmap(req_addr: u64, size: u64, prot: MmapProt, flags: MmapFlags) -> u64 {
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
    let fixed = flags.contains(MmapFlags::FIXED);
    let writable = prot.contains(MmapProt::WRITE);

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
            || !crate::mm::user_span::in_user_half(req_addr, aligned as u64)
        {
            return SyscallError::InvalidArgument.to_u64();
        }
        Some(req_addr)
    } else {
        None
    };

    // Allocate only once the request is known to be satisfiable, so a refused
    // fixed mapping does not leak its pages.
    let pages = if prot == MmapProt::NONE {
        None
    } else {
        match process::PageAlloc::new(aligned, crate::mm::pmm::Category::Mmap) {
            Some(pages) => Some(pages),
            None => return SyscallError::ResourceExhausted.to_u64(),
        }
    };

    if let Some(start) = fixed_start {
        let pt = process::current_address_space();
        let end = start + aligned as u64;
        let mut cur = start;
        let mut offset = 0u64;
        while cur < end {
            match &pages {
                Some(pages) => pt.lock().remap(UserAddr::new(cur), pages.phys() + offset, writable),
                // A fixed request over a range that already carries a mapping
                // must take it away, or the caller gets an accessible page
                // exactly where it asked for a fault.
                None => pt.lock().unmap(UserAddr::new(cur)),
            }
            cur += crate::mm::PAGE_2M;
            offset += crate::mm::PAGE_2M;
        }
        crate::arch::tlb::shootdown();
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
        let pt = process::current_address_space();
        let vaddr = process::with_fd_owner_data(|data| {
            let placed = match &pages {
                Some(pages) => pt.lock().alloc_and_map(pages.phys(), aligned as u64, writable, CachePolicy::DeferToMtrr).map(|(v, _)| v),
                None => pt.lock().alloc_region(aligned as u64, crate::vma::RegionKind::Mapped, false),
            };
            let Some(vaddr) = placed else { return Err(()) };
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

/// The pages go back to the PMM here, so this is the syscall the shootdown
/// matters most on: a sibling thread of the same process holds translations for
/// exactly this range, and until M3 nothing told it otherwise.
fn sys_munmap(addr: u64, _size: u64) -> u64 {
    let pt = process::current_address_space();
    let taken = process::with_fd_owner_data(|data| {
        let idx = data.mmap_regions.iter().position(|r| r.addr.raw() == addr)?;
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
        Some(crate::mm::Unmapped::new(region))
    });
    let Some(unmapped) = taken else {
        return SyscallError::NotFound.to_u64();
    };
    // Dropped out here, not inside the closure: the drop shoots down and waits,
    // and the fd-owner lock the closure holds is one a sibling can be spinning
    // on with `IF` clear.
    drop(unmapped);
    0
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

/// The most live threads `SYS_SYSINFO` will describe.
///
/// A *derived* collection, in the sense the loader's relocation index is: the
/// caller's buffer bounds what is written and bounds nothing about what is
/// built, because the sort needs every entry before the first one can be
/// chosen. One `(Tid, &ProcessEntry, &ThreadEntry)` is 24 bytes and this is
/// one allocation, so it has to stay under `mm::MAX_HEAP_ALLOC` (2,093,056) —
/// which it did not: nothing caps the thread count, and any process may call
/// this, so ~87,000 threads turned an ordinary syscall into the allocator's
/// fail-fast assert.
///
/// 65,536 leaves the allocation at 1,572,864 bytes, a factor of 1.3 under the
/// ceiling, and the reservation below is exact so there is no growth-by-
/// doubling overshoot to absorb. A machine with more live threads than this
/// gets `ResourceExhausted` from `ps`, which is a refusal rather than a
/// kernel panic — the bound is policy, the ceiling it is derived from is not.
#[cfg(not(feature = "test-heap-ceiling"))]
const MAX_SYSINFO_THREADS: usize = 65_536;

/// Sixteen, so the refusal above has a gate.
///
/// The bound is a function of `MAX_HEAP_ALLOC` and nothing in this harness can
/// make 65,536 threads — each carries a 128 KiB kernel stack, which is 8 GiB
/// of a guest given 128 MiB. Only the constant can move, and moving it runs
/// the whole refusal: the count, the comparison and the error return are the
/// shipped ones.
#[cfg(feature = "test-heap-ceiling")]
const MAX_SYSINFO_THREADS: usize = 16;

fn sys_sysinfo(out: &mut UserBytesMut) -> u64 {
    const HEADER_SIZE: usize = 48;
    const ENTRY_SIZE: usize = 64;
    if out.len() < HEADER_SIZE {
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
    if entry_count as usize > MAX_SYSINFO_THREADS {
        return SyscallError::ResourceExhausted.to_u64();
    }

    let mut header = [0u8; HEADER_SIZE];
    header[0..8].copy_from_slice(&total_mem.to_le_bytes());
    header[8..16].copy_from_slice(&used_mem.to_le_bytes());
    header[16..20].copy_from_slice(&cpu_count.to_le_bytes());
    header[20..24].copy_from_slice(&entry_count.to_le_bytes());
    header[24..32].copy_from_slice(&uptime.to_le_bytes());
    header[32..40].copy_from_slice(&total_cpu_ns.to_le_bytes());
    header[40..48].copy_from_slice(&total_available_ns.to_le_bytes());
    out.write_at(0, &header);

    let max_entries = (out.len() - HEADER_SIZE) / ENTRY_SIZE;

    // Collect and sort by (pid, tid) for stable output. Reserved exactly from
    // the count above, so the buffer is `entry_count * 24` and not whatever
    // the next doubling step would have been.
    let mut entries: Vec<(process::Tid, &process::ProcessEntry, &process::ThreadEntry)> =
        Vec::with_capacity(entry_count as usize);
    entries.extend(table.iter().flat_map(|(_, proc)| proc.threads().iter().map(move |(tid, thread)| (tid, proc, thread))));
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
            let mmap = data.mmap_regions.iter().filter_map(|r| r._pages.as_ref()).map(|p| p.size() as u64).sum::<u64>();
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

        let mut entry = [0u8; ENTRY_SIZE];
        entry[0..4].copy_from_slice(&pid.raw().to_le_bytes());
        entry[4..8].copy_from_slice(&parent_pid.raw().to_le_bytes());
        entry[8..12].copy_from_slice(&tid.raw().to_le_bytes());
        entry[12] = state;
        entry[13] = is_thread;
        entry[16..24].copy_from_slice(&memory.to_le_bytes());
        entry[24..32].copy_from_slice(&cpu_ns.to_le_bytes());
        entry[32..60].copy_from_slice(name);
        out.write_at(pos, &entry);

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

/// A second descriptor for the object `fd_num` names.
///
/// `PermissionDenied` is the answer for a device claim: it is the one object
/// that admits a single descriptor, and `Descriptor::duplicate` says so at the
/// only place that can. Before this, `dup` handed back a claim's exclusivity
/// while leaving the caller a working descriptor.
fn sys_dup(fd_num: u32) -> u64 {
    process::with_fd_owner_data(|data| {
        let desc = match data.fds.get(fd_num).map(fd::Descriptor::duplicate) {
            Some(Some(d)) => d,
            Some(None) => return SyscallError::PermissionDenied.to_u64(),
            None => return SyscallError::NotFound.to_u64(),
        };
        fd_result(data.fds.insert(desc))
    })
}

fn sys_dup2(old_fd: u32, new_fd: u32) -> u64 {
    let mut wake_read = None;
    let mut wake_write = None;
    let result = process::with_fd_owner_data(|data| {
        let desc = match data.fds.get(old_fd).map(fd::Descriptor::duplicate) {
            Some(Some(d)) => d,
            Some(None) => return SyscallError::PermissionDenied.to_u64(),
            None => return SyscallError::NotFound.to_u64(),
        };
        if let Some(existing) = data.fds.get(new_fd) {
            wake_read = existing.pipe_id_read();
            wake_write = existing.pipe_id_write();
            fd::close(&mut data.fds, new_fd, &mut data.pipe_maps);
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
    if !vfs.user_may_modify(&old_abs) || !vfs.user_may_modify(&new_abs) {
        return SyscallError::PermissionDenied.to_u64();
    }
    match vfs.rename(&old_abs, &new_abs) {
        Ok(()) => 0,
        Err(e) => e.to_u64(),
    }
}

fn sys_mkdir(path: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    if !vfs.user_may_modify(&resolved) {
        return SyscallError::PermissionDenied.to_u64();
    }
    match vfs.create_dir(&resolved) {
        Ok(()) => 0,
        Err(e) => e.to_u64(),
    }
}

fn sys_rmdir(path: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    if !vfs.user_may_modify(&resolved) {
        return SyscallError::PermissionDenied.to_u64();
    }
    vfs.remove_dir(&resolved);
    0
}

fn sys_symlink(target: &str, link: &str) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, link);
    if !vfs.user_may_modify(&resolved) {
        return SyscallError::PermissionDenied.to_u64();
    }
    match vfs.create_symlink(&resolved, target) {
        Ok(()) => 0,
        Err(e) => {
            log!("symlink({target} -> {link}): {e}");
            e.to_u64()
        }
    }
}

fn sys_readlink(path: &str, out: &mut UserBytesMut) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    match vfs.read_link(&resolved) {
        Ok(Some(target)) => {
            let bytes = target.as_bytes();
            let len = bytes.len().min(out.len());
            out.write_at(0, &bytes[..len]);
            len as u64
        }
        Ok(None) => SyscallError::NotFound.to_u64(),
        Err(e) => e.to_u64(),
    }
}

fn sys_dlopen(ctx: &crate::user_ptr::SyscallContext, path: &str, init_out: Option<UserAddr>) -> u64 {
    let cwd = process::with_fd_owner_data(|d| d.cwd.clone());
    let resolved = vfs::lock().resolve_absolute(&cwd, path);

    let lib = crate::elf::try_clone_cached(&resolved);
    let mut lib = match lib {
        Some(lib) => lib,
        None => {
            let backing = match vfs::lock().open_backing(&resolved) {
                Ok(b) => b,
                Err(e) => {
                    log!("dlopen: {}: {e}", resolved);
                    return e.to_u64();
                }
            };

            let (lib, rw_offset, rw_size) = match crate::elf::load_shared_lib(backing.as_ref()) {
                Ok(result) => result,
                Err(msg) => {
                    log!("dlopen: {}", msg);
                    return SyscallError::Unknown.to_u64();
                }
            };

            crate::elf::cache_loaded_lib(&resolved, lib, rw_offset, rw_size)
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
                crate::arch::tlb::shootdown();
                let delta = lib_vaddr.raw() as i64 - lib.user_base.raw() as i64;
                if delta != 0 {
                    crate::elf::rebase_relative_relocs(&lib, delta);
                }
                lib.user_base = lib_vaddr;
                
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

    // Format: [init_array_vaddr: u64, init_array_count: u64], the vaddr rebased
    // to the library's user_base.
    let init_info = [
        if lib.init_array_vaddr != 0 { lib.user_base.raw() + lib.init_array_vaddr } else { 0 },
        lib.init_array_size / 8,
    ];

    let idx = {
        let mut data = data_arc.lock();
        let idx = data.elf.loaded_libs.len();
        data.elf.lib_paths.push(resolved);
        data.elf.loaded_libs.push(lib);
        idx
    };

    // After the library is registered, because it is mapped either way: a
    // failure here is the caller losing its handle, not the address space
    // losing track of a mapping.
    if let Some(out) = init_out {
        if ctx.copy_out(out, &init_info).is_err() {
            return SyscallError::BadAddress.to_u64();
        }
    }
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
    let (ring, shm_token) = match crate::io_uring::create(depth) {
        Ok(v) => v,
        Err(e) => return e.to_u64(),
    };
    // A refused insert drops the reference, which tears the ring down again.
    let fd = process::with_fd_owner_data(|data| {
        data.fds.insert(fd::Descriptor::IoUring(ring))
    });
    match fd {
        Ok(fd_num) => ((shm_token.raw() as u64) << 32) | (fd_num as u64),
        Err(e) => e.to_u64(),
    }
}

fn sys_io_uring_enter(ring_fd: u32, to_submit: u32, min_complete: u32, timeout_nanos: u64) -> u64 {
    let ring_id = process::with_fd_owner_data(|data| {
        match data.fds.get(ring_fd) {
            Some(fd::Descriptor::IoUring(r)) => Some(r.id()),
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

fn sys_sched_info() -> toyos_abi::syscall::SchedInfo {
    let pid = process::current_process();
    toyos_abi::syscall::SchedInfo {
        vruntime: crate::scheduler::process_vruntime(pid),
        min_vruntime: crate::scheduler::global_min_vruntime(),
        lag: crate::scheduler::process_lag(pid),
    }
}

/// Hand the caller its exited child's accounting snapshot, which it may read
/// exactly once.
///
/// Copied out before it is removed, because the removal is what makes this the
/// only chance to read it: a write the kernel refused after taking the snapshot
/// would leave nobody able to ask again.
fn sys_process_stats(
    ctx: &crate::user_ptr::SyscallContext,
    child_pid: process::Pid,
    out: UserAddr,
) -> u64 {
    let snap = process::with_fd_owner_data(|data| {
        data.child_stats.iter().find(|(pid, _)| *pid == child_pid).map(|(_, s)| *s)
    });
    let Some(stats) = snap else { return SyscallError::NotFound.to_u64() };
    if let Err(e) = ctx.copy_out(out, &stats) {
        return e.to_u64();
    }
    process::with_fd_owner_data(|data| data.child_stats.retain(|(pid, _)| *pid != child_pid));
    0
}

/// Describe every loaded module into `buf`; return the length it *needs*.
///
/// Same contract as `sys_getcwd` and `sys_readdir`, and for the same reason.
/// This used to answer a too-small buffer with a bare `InvalidArgument` while
/// the ABI wrapper's doc comment claimed the required size was "encoded" in it
/// — a claim `SyscallError` cannot carry, so a caller had no way to size a
/// retry and no way to learn that was why it failed.
///
/// The answer is a byte length and never a module count: the records carry
/// packed path strings, so a count cannot size the buffer. Nothing is written
/// unless all of it fits, which makes an empty buffer a size query.
///
/// The record array is `buf[..records[0].path_offset]` — every module writes
/// its path after the last record, so the first module's `path_offset` is
/// where the array ends.
///
/// Every module holds address space for as long as it is loaded, so the count
/// is bounded by the process's own arena and the required length stays far
/// below the range `SyscallError` encodes — it can never be misread as one.
fn sys_query_modules(out: &mut UserBytesMut) -> u64 {
    use toyos_abi::syscall::ModuleInfo;
    let info_size = core::mem::size_of::<ModuleInfo>();

    // The record is `#[repr(C)]` over five integers with no padding, so its
    // bytes are its fields — the alternative is a per-field encoder for a
    // layout the ABI already fixes.
    fn encode(info: &ModuleInfo) -> [u8; core::mem::size_of::<ModuleInfo>()] {
        unsafe { core::mem::transmute_copy(info) }
    }

    process::with_fd_owner_data(|data| {
        let module_count = 1 + data.elf.loaded_libs.len();

        let exe_path_bytes = data.exe_path.as_bytes();
        let total_path_bytes: usize = exe_path_bytes.len()
            + data.elf.lib_paths.iter().map(|p| p.as_bytes().len()).sum::<usize>();

        let required = module_count * info_size + total_path_bytes;
        if out.len() < required {
            return required as u64;
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
        out.write_at(0, &encode(&exe_info));
        out.write_at(path_offset as usize, exe_path_bytes);
        path_offset += exe_path_bytes.len() as u32;

        for (i, lib) in data.elf.loaded_libs.iter().enumerate() {
            let lib_path_bytes = if i < data.elf.lib_paths.len() {
                data.elf.lib_paths[i].as_bytes()
            } else {
                b""
            };
            let lib_info = ModuleInfo {
                base: lib.user_base.raw(),
                text_end: lib.user_end(),
                eh_frame_hdr: if lib.eh_frame_hdr_vaddr != 0 {
                    lib.user_base.raw() + lib.eh_frame_hdr_vaddr
                } else { 0 },
                eh_frame_hdr_size: lib.eh_frame_hdr_size,
                path_offset,
                path_len: lib_path_bytes.len() as u32,
            };
            out.write_at((1 + i) * info_size, &encode(&lib_info));
            out.write_at(path_offset as usize, lib_path_bytes);
            path_offset += lib_path_bytes.len() as u32;
        }

        required as u64
    })
}

/// Terminate the current userspace process (called from exception handlers).
pub fn kill_process(code: i32) -> ! {
    process::exit(code);
}
