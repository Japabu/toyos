use alloc::vec::Vec;
use super::entry::{restore_user_state, ring3_naked_asm, save_user_state, Ring3Entry};
use super::{cpu, percpu};
use crate::drivers::acpi;
use crate::mm::paging::{CachePolicy, Occupancy, Prot};
use crate::user_ptr::{SyscallContext, UserBytes, UserBytesMut};
use toyos_sched::task::WaitClass;

use crate::completion;
use crate::object::{ops, port, KObjectRef};
use crate::time::{Cadence, Deadline, Duration};
use crate::{device, log, pipe, process, vfs};
use crate::UserAddr;

use toyos_untrusted::Untrusted;

// MSR addresses. `IA32_EFER` is not among them: `SCE` is one bit of a register
// `arch::control_regs` declares whole, and reading it back to OR a bit in here
// was the second place deciding what one register held.
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;

use toyos_abi::handle::{RawHandle, Rights};
#[cfg(feature = "test-actuators")]
use toyos_abi::syscall::debug_action as DA;
use toyos_abi::syscall::*;

/// One [`RawHandle`] on the wire, for a vector of them a syscall reads out of
/// user memory a handle at a time.
const HANDLE_LEN: usize = core::mem::size_of::<RawHandle>();

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
    26 => "SYS_WAITPID",
    29 => "SYS_SEND_MSG",
    30 => "SYS_RECV_MSG",
    31 => "SYS_OPEN_DEVICE",
    32 => "SYS_REGISTER_NAME",
    33 => "SYS_FIND_PID",
    36 => "SYS_ALLOC_SHARED",
    37 => "SYS_GRANT_SHARED",
    38 => "SYS_MAP_SHARED",
    39 => "SYS_RELEASE_SHARED",
    65 => "SYS_KILL",
    68 => "SYS_PIPE_OPEN",
    70 => "SYS_PIPE_ID",
    85 => "SYS_LISTEN",
    87 => "SYS_CONNECT",
    96 => "SYS_SET_RT_PRIORITY",
}

/// `SYS_DEBUG` action 2's lock, and nothing else's.
///
/// Action 2 takes it and then calls a switching scheduler entry — the shape
/// spec §6.4's tripwire exists to refuse. The assert fires while the guard is
/// still alive, so the guard never drops and this lock stays held for the rest
/// of the boot; that is why it is private to the one deliberate-panic action
/// and shared with nothing.
#[cfg(feature = "test-actuators")]
static LOCK_ACROSS_SWITCH: crate::sync::Lock<()> = crate::sync::Lock::new(());

/// One trip per boot, because the lock above is never released.
///
/// On a kernel that carries `SYS_DEBUG` at all, without this a process could
/// call action 2 a second time and spin `Lock::lock`'s full 500M iterations on a
/// lock nothing will ever hand over — with IF=0 (`MSR_FMASK` masks it on syscall
/// entry) and preemption disabled, so on a single-CPU machine the timer, the log
/// drains and every other thread are frozen for that whole window. Refusing the
/// second call keeps the tripwire testable and the stall unreachable.
#[cfg(feature = "test-actuators")]
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
#[cfg(feature = "test-actuators")]
pub const FATAL_HALT_NONCE: &str = "SYS_DEBUG: fatal halt 4b1d9e2c";

/// One kernel heap allocation of `bytes` at `align`, taken and released.
/// `SYS_DEBUG` actions 5, 6 and 7 are its only callers.
///
/// Raw `alloc`/`dealloc` rather than a `Vec` that is immediately dropped:
/// LLVM is allowed to delete a malloc/free pair whose result is never
/// observed, and an actuator the optimiser can remove certifies nothing. The
/// null return is reported rather than unwrapped for the same reason — a
/// refusal and a success have to be distinguishable from userland.
#[cfg(feature = "test-actuators")]
fn debug_heap_alloc(bytes: usize, align: usize) -> u64 {
    let Ok(layout) = core::alloc::Layout::from_size_align(bytes, align) else {
        return SyscallError::InvalidArgument.to_u64();
    };
    // SAFETY: `layout` came from `Layout::from_size_align`, which refused a
    // zero size or a non-power-of-two alignment on the line above, and that is
    // `alloc`'s whole contract. **Irreducible on purpose**: the doc comment
    // above is the argument — a `Vec` dropped immediately is a malloc/free pair
    // whose result nothing observes, which LLVM may delete, and an actuator the
    // optimiser can remove certifies nothing about the allocator.
    let p = unsafe { alloc::alloc::alloc(layout) };
    if p.is_null() {
        return SyscallError::ResourceExhausted.to_u64();
    }
    // SAFETY: `p` is a live, non-null allocation of at least one byte, asserted
    // by the null check above. Volatile for the same reason the raw pair is raw.
    unsafe { core::ptr::write_volatile(p, 1u8) };
    // SAFETY: `p` came from `alloc` with this exact `layout` and has not been
    // freed, which is `dealloc`'s contract.
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
#[cfg(feature = "test-actuators")]
mod canary {
    use core::sync::atomic::{AtomicU64, Ordering};

    const VALUE: [u64; 2] = [0x_C0DE_1A55_0F17_1E55, 0x0005_EE7A_110F_1700];

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

/// Point `SYSCALL` at [`syscall_entry`] on this CPU.
///
/// `EFER.SCE` — the bit that makes the instruction exist at all — is not set
/// here: it is `arch::control_regs`'s, applied and asserted on this CPU before
/// this call on both the BSP's path and an AP's.
pub fn init() {
    let star = ((percpu::STAR_SYSRET_BASE as u64) << 48) | ((percpu::KERNEL_CS as u64) << 32);
    // SAFETY: `cpu::wrmsr` asks its caller to own the MSR it names and the value
    // it writes, and this function is that owner for all three of `SYSCALL`'s.
    // `STAR` is built from `percpu`'s own selector constants; `LSTAR` is a
    // [`Ring3Entry`] — a kernel text address this module classified, and the one
    // register whose wrong value would aim Ring 3 at somewhere else in the
    // kernel; `FMASK` is the literal below. None can `#GP` for being
    // unimplemented: `control_regs::declaration` asserts
    // `CPUID.80000001H:EDX[11]` on every CPU before this runs, which is what
    // makes `SYSCALL` exist at all.
    //
    // **One block, because the three are one declaration.** A CPU holding two of
    // them is a CPU whose `SYSCALL` gate is aimed by something this file did not
    // decide, and there is no point between these writes where that is a state
    // the machine may be left in.
    unsafe {
        cpu::wrmsr(MSR_STAR, star);
        // `LSTAR` is an IDT slot by another name: the one thing `syscall` can
        // reach.
        cpu::wrmsr(MSR_LSTAR, Ring3Entry::new(syscall_entry).addr());
        // The four `RFLAGS` bits a Ring 3 thread may not hand the kernel.
        //
        // **`SYSCALL` clears exactly what this word names and nothing else**, so
        // every bit left out of it is a Ring 3 thread's flag running Ring 0 code.
        // A thread's own copy survives either way: the CPU puts the pre-mask
        // `RFLAGS` in `r11` and `sysretq` restores it, so what this decides is
        // only what the *kernel* runs with.
        //
        // - `DF` — a kernel that inherits a set direction flag runs every
        //   `rep movs`/`rep stos` backwards, writing the `n` bytes *below* a
        //   destination instead of at it. `arch::entry::ring3_naked_asm`'s `cld`
        //   carries the whole argument; this is the same fix on the one entry
        //   where the hardware lets a mask word make it.
        // - `TF` — **without it, three Ring 3 instructions halt the machine.**
        //   The single-step trap after a `popfq` that set `TF` is deferred by
        //   exactly one instruction, and if that instruction is `syscall` the
        //   `#DB` is taken at `LSTAR` with CPL already 0 and `rsp` still the
        //   *user* stack, because the entry has not reached `mov rsp, gs:[16]`.
        //   The `#DB` gate has no IST, so the CPU builds its frame there — a
        //   supervisor write to a user page, which SMAP refuses — and the `#PF`
        //   lands on the same stack and escalates. Measured on this tree before
        //   the bit was added: `DOUBLE FAULT on CPU 1`, `rip=syscall_entry+0x0`,
        //   `cr2 = rsp - 8` on a `P=1 W=1 U=1` page, every CPU halted.
        //   `debug_trap`'s `tf-syscall` arm is the gate.
        // - `IF` and `AC` — interrupts stay masked for the whole of a syscall,
        //   and `RFLAGS.AC` clear is what makes SMAP bind at all.
        //
        // `entry-df-unclean` is `arch::entry`'s negative control and this is its
        // other half: it takes `DF` back out, so the arm stages the whole defect
        // rather than the gates' share of it. It takes nothing else out — a
        // control that removed two bits would be measuring two things.
        const TF: u64 = 1 << 8;
        const IF: u64 = 1 << 9;
        const DF: u64 = 1 << 10;
        const AC: u64 = 1 << 18;
        let df = if cfg!(feature = "entry-df-unclean") { 0 } else { DF };
        cpu::wrmsr(MSR_FMASK, TF | IF | AC | df);
    }
}

// Syscall entry: GS permanently points to kernel per-CPU data (no swapgs needed).
// PerCpu layout: offset 16 = kernel_rsp, offset 24 = user_rsp.
//
// The bracket spans the handler *and* the exit-to-user epilogue, because both
// can context-switch. The epilogue used to run with the user state already put
// back, so a switch there returned to Ring 3 carrying whatever the task that
// ran in between had left in the registers.
//
// **`SYSCALL` switches no stack, so the instructions before `mov rsp, gs:[16]`
// run at CPL 0 on the user's stack — and that is the whole of the window an
// exception may not land in.** It was six instructions and it is three: the
// three diagnostic stores below it — `syscall_rip`, `syscall_num`,
// `syscall_rbp` — are reads of `rcx`, `rdi` and `rbp`, which the stack switch
// does not touch, so they are the same stores done one instruction later on a
// stack the CPU may write. What is left cannot be shortened: `cld` is at offset
// 0 by `arch::entry`'s rule and every Ring 0 entry owes it, `rsp` must be parked
// before it is overwritten, and overwriting it is the fix. The exit has a
// one-instruction window of the same kind, between `pop rsp` and `sysretq`.
//
// Which is why the vectors that can arrive there have an IST (`arch::idt`'s
// table): the window is a floor, not a bug to be closed.
#[unsafe(naked)]
extern "sysv64" fn syscall_entry() {
    ring3_naked_asm!(
        "mov gs:[24], rsp",     // save user RSP to percpu.user_rsp
        "mov rsp, gs:[16]",     // load kernel RSP from percpu.kernel_rsp
        "mov gs:[216], rcx",    // save user RIP to percpu.syscall_rip
        "mov gs:[224], rdi",    // save syscall number to percpu.syscall_num
        "mov gs:[232], rbp",    // save user RBP to percpu.syscall_rbp
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
    #[cfg(feature = "df-witness")]
    cpu::df_witness("syscall_handler");
    syscall_dispatch(num, a1, a2, a3, a4)
}

/// What a syscall answers when its wait was cancelled.
///
/// **Nothing ever reads it.** The thread has been killed, so the return path
/// it is on ends at `kernel_exit_to_user_check`, which sees the kill bit and
/// exits instead of returning to Ring 3 (§7.2). The word exists because the
/// unwind has to carry *something* through the `u64` every syscall answers in,
/// and `Interrupted` is what it would mean if anything could read it.
fn cancelled() -> u64 {
    SyscallError::Gone.to_u64()
}

fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let t0 = crate::clock::nanos_since_boot();

    process::with_current_data(|data| {
        data.syscall_total += 1;
        // Clamped rather than guarded: a number this ABI does not issue is
        // still a call the total counted, so it lands in the last bin instead
        // of nowhere.
        data.syscall_counts[(num as usize).min(toyos_abi::syscall::SYSCALL_PROFILE_OTHER)] += 1;
    });

    // SAFETY: current process's page tables remain active for the duration of this call.
    let ctx = unsafe { SyscallContext::new() };

    let bad_addr = SyscallError::BadAddress.to_u64();

    let result = match num {
        SYS_WRITE => {
            let Some(buf) = ctx.user_bytes(UserAddr::new(a2), a3) else { return bad_addr };
            sys_write(RawHandle(a1 as u32), &buf)
        }
        SYS_READ => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a2), a3) else { return bad_addr };
            sys_read(RawHandle(a1 as u32), &mut buf)
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
        SYS_CLOSE => sys_close(RawHandle(a1 as u32)),
        SYS_SEEK => {
            let pos = match a3 {
                0 => SeekFrom::Start(a2),
                1 => SeekFrom::Current(a2 as i64),
                2 => SeekFrom::End(a2 as i64),
                _ => return SyscallError::InvalidArgument.to_u64(),
            };
            with_object(RawHandle(a1 as u32), Rights::READ, |o| ops::seek(o, pos))
        }
        // **No right.** `fstat` asks the handle what kind of thing it names
        // and how big it is; it moves no content in either direction, so
        // gating it on `READ` would make `isatty(1)` and libc's line-buffering
        // decision fail on every write-only handle they are asked about.
        SYS_FSTAT => {
            let stat = match with_object_ref(RawHandle(a1 as u32), Rights::NONE, ops::fstat) {
                Ok(stat) => stat,
                Err(e) => return e.refuse(),
            };
            match ctx.copy_out(UserAddr::new(a2), &stat) {
                Ok(()) => 0,
                Err(e) => e.to_u64(),
            }
        }
        SYS_FSYNC => with_object(RawHandle(a1 as u32), Rights::WRITE, ops::fsync),
        SYS_READDIR => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a3), a4) else { return bad_addr };
            sys_readdir(&path, &mut buf)
        }
        SYS_DELETE => {
            let path = match ctx.user_str(UserAddr::new(a1), a2) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_delete(&path)
        }
        SYS_SHUTDOWN => sys_shutdown(RawHandle(a1 as u32)),
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
            if args.endow_count as usize > toyos_abi::syscall::MAX_ENDOWMENTS
                || args.labels_len > toyos_abi::syscall::MAX_LABELS_LEN as u64
            {
                return SyscallError::InvalidArgument.to_u64();
            }
            // One pair read out of each window at a time rather than the
            // vectors copied wholesale: both counts are userland's, and a copy
            // would put them on the allocator for a loop that reads each entry
            // exactly once. The label blob is the exception — the child keeps
            // it, so it is copied in and bounded by `MAX_LABELS_LEN`.
            let Some(slot_map) = (args.slot_map_count as usize)
                .checked_mul(crate::loader::SLOT_PAIR_LEN)
                .and_then(|len| ctx.user_bytes(UserAddr::new(args.slot_map_ptr), len as u64))
            else {
                return bad_addr;
            };
            let Some(endow) = (args.endow_count as usize)
                .checked_mul(core::mem::size_of::<toyos_abi::syscall::EndowEntry>())
                .and_then(|len| ctx.user_bytes(UserAddr::new(args.endow_ptr), len as u64))
            else {
                return bad_addr;
            };
            let labels = match ctx.user_vec(UserAddr::new(args.labels_ptr), args.labels_len) {
                Ok(bytes) => bytes,
                Err(e) => return e.to_u64(),
            };
            let pending = match process::build_child_handles(&slot_map, &endow, &labels) {
                Ok(built) => built,
                Err(e) => return e.refuse(),
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
            sys_spawn(&text, pending, env)
        }
        SYS_PROCESS_WAIT => sys_process_wait(RawHandle(a1 as u32), a2),
        SYS_PROCESS_KILL => {
            match process::with_process_data(|data| {
                data.handles
                    .get::<crate::object::process::ProcessObject>(RawHandle(a1 as u32), Rights::MANAGE)
            }) {
                Ok(object) => {
                    // **Killing yourself is exiting, and `kill_process` cannot
                    // do it.** It retires every thread of its target, and
                    // `retire_task` asserts that a CPU never retires the task
                    // it is running on — so a process holding a `MANAGE`
                    // handle to itself panicked the kernel. Nothing stops one
                    // holding one: `Process` carries `TRANSFER`, so a parent
                    // may send a child the child's own handle.
                    if object.pid() == process::current_process() {
                        // `exit` does not come back and nothing unwinds past
                        // it, so the clone this match is holding is dropped
                        // where it can still be dropped.
                        drop(object);
                        process::exit(process::KILLED_EXIT_CODE);
                    }
                    process::kill_process(&object)
                }
                Err(e) => e.refuse(),
            }
        }
        SYS_PROCESS_OPEN => {
            sys_process_open(RawHandle(a1 as u32), process::Pid::from_raw(a2 as u32))
        }

        // No right either, and for the same reason: its one caller marks
        // both ends of a pair, so a right either end lacks would refuse one
        // of the two.
        SYS_MARK_TTY => with_object(RawHandle(a1 as u32), Rights::NONE, ops::mark_tty),
        // Display integrity, not memory access: framebuffer *contents* are
        // behind shared_memory grants either way. Ungated, any process could
        // scan out over the compositor's frames and move the cursor.
        SYS_GPU_PRESENT | SYS_GPU_SET_CURSOR | SYS_GPU_MOVE_CURSOR => {
            if let Err(e) = holds_claim(RawHandle(a1 as u32), device::DeviceType::Framebuffer) {
                return e.refuse();
            }
            let (hi2, lo2) = unpair(a2);
            let (hi3, lo3) = unpair(a3);
            match num {
                SYS_GPU_PRESENT => crate::gpu::present_rect(hi2, lo2, hi3, lo3),
                SYS_GPU_SET_CURSOR => crate::gpu::set_cursor(a2 as u32, a3 as u32),
                _ => crate::gpu::move_cursor(a2 as u32, a3 as u32),
            }
            0
        }
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
                let now = toyos_wallclock::Civil::from_unix_secs(secs);
                (now.hour << 16) | (now.min << 8) | now.sec
            },
        ),
        SYS_CLOCK_EPOCH => {
            crate::clock::utc_secs().map_or(SyscallError::NotSupported.to_u64(), |secs| secs)
        }
        // The capability is first, as it is at every other arm that takes one.
        // The buffer decides whether it is looked at: the header is a machine
        // fact like `SYS_CPU_COUNT` and stays ambient, and the roster after it
        // costs `Rights::ROSTER` because it is every process in the machine by
        // name.
        SYS_SYSINFO => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a2), a3) else { return bad_addr };
            sys_sysinfo(RawHandle(a1 as u32), &mut buf)
        }
        SYS_NANOSLEEP => sys_nanosleep(a1),
        SYS_HANDLE_DUP => sys_handle_dup(RawHandle(a1 as u32), a2),
        SYS_HANDLE_DUP_AT => sys_dup2(RawHandle(a1 as u32), a2),
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
        SYS_FTRUNCATE => {
            with_object(RawHandle(a1 as u32), Rights::WRITE, |o| ops::ftruncate(o, a2))
        }
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
        SYS_READ_NONBLOCK => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a2), a3) else { return bad_addr };
            sys_read_nonblock(RawHandle(a1 as u32), &mut buf)
        }
        SYS_WRITE_NONBLOCK => {
            let Some(buf) = ctx.user_bytes(UserAddr::new(a2), a3) else { return bad_addr };
            sys_write_nonblock(RawHandle(a1 as u32), &buf)
        }
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_GET_ENV => {
            let env = process::with_process_data(|d| d.env.clone());
            if a2 == 0 {
                env.len() as u64
            } else {
                let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a1), a2) else { return bad_addr };
                let copy_len = env.len().min(buf.len());
                buf.write_at(0, &env[..copy_len]);
                copy_len as u64
            }
        }
        SYS_CONNECTION_JOIN => {
            sys_connection_join(RawHandle(a1 as u32), RawHandle(a2 as u32))
        }
        SYS_PIPE_MAP => sys_pipe_map(RawHandle(a1 as u32)),
        // All three drive the NIC's rings, so without the claim any process
        // could pop frames out of the used ring before netd sees them and, by
        // never refilling, exhaust all 256 RX slots.
        SYS_NIC_RX_POLL => {
            if let Err(e) = holds_claim(RawHandle(a1 as u32), device::DeviceType::Nic) {
                return e.refuse();
            }
            match crate::net::poll_rx() {
                Some((buf_idx, frame_len)) => ((buf_idx as u64) << 16) | (frame_len as u64),
                None => 0,
            }
        }
        SYS_NIC_RX_DONE => {
            if let Err(e) = holds_claim(RawHandle(a1 as u32), device::DeviceType::Nic) {
                return e.refuse();
            }
            crate::net::refill_rx_buf(a2 as usize).map_or_else(|e| e.to_u64(), |()| 0)
        }
        SYS_NIC_TX => {
            if let Err(e) = holds_claim(RawHandle(a1 as u32), device::DeviceType::Nic) {
                return e.refuse();
            }
            match crate::net::submit_tx(a2 as usize) {
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
            // Checked before the driver, so a caller with no claim never gets
            // its two arbitrary u32s turned into a contiguous physical
            // allocation.
            let claim_h = RawHandle(a1 as u32);
            if let Err(e) = holds_claim(claim_h, device::DeviceType::Framebuffer) {
                return e.refuse();
            }
            // Checked before the allocation for the same reason the claim is: a
            // caller that named an address the kernel will not write to must
            // not be left with a resolution it is never told about.
            let Some(info_out) = UserAddr::checked(a3) else { return bad_addr };
            let (width, height) = unpair(a2);
            match crate::gpu::set_resolution(width, height) {
                Ok(gpu_info) => sys_gpu_reset_scanout(&ctx, claim_h, gpu_info, info_out),
                Err(e) => e.to_u64(),
            }
        }
        SYS_ENDOWMENTS => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_endowments(&mut buf)
        }
        SYS_DEVICE_CLAIM => sys_device_claim(RawHandle(a1 as u32), a2),
        SYS_RT_ENTER => sys_rt_enter(RawHandle(a1 as u32)),
        SYS_LOG_READ => {
            // The record count is userland's, so the byte length is computed
            // from it before anything is mapped: a product that does not fit is
            // a caller's argument being wrong, not the kernel's arithmetic
            // wrapping into a window it then trusts.
            let Some(bytes) = (a4 as usize).checked_mul(toyos_abi::log::RECORD_BYTES) else {
                return SyscallError::InvalidArgument.to_u64();
            };
            let Some(mut out) = ctx.user_bytes_mut(UserAddr::new(a3), bytes as u64) else {
                return bad_addr;
            };
            sys_log_read(&ctx, RawHandle(a1 as u32), UserAddr::new(a2), &mut out, a4 as usize)
        }
        SYS_ACCEPT => sys_accept(RawHandle(a1 as u32)),
        SYS_HANDLE_SEND => {
            if a3 as usize > MAX_TRANSFER_HANDLES {
                return SyscallError::InvalidArgument.to_u64();
            }
            let Some(handles) = (a3 as usize)
                .checked_mul(HANDLE_LEN)
                .and_then(|len| ctx.user_bytes(UserAddr::new(a2), len as u64))
            else {
                return bad_addr;
            };
            sys_handle_send(RawHandle(a1 as u32), &handles, a3 as usize)
        }
        SYS_HANDLE_RECV => {
            if a3 as usize > MAX_TRANSFER_HANDLES {
                return SyscallError::InvalidArgument.to_u64();
            }
            let Some(mut out) = (a3 as usize)
                .checked_mul(HANDLE_LEN)
                .and_then(|len| ctx.user_bytes_mut(UserAddr::new(a2), len as u64))
            else {
                return bad_addr;
            };
            sys_handle_recv(RawHandle(a1 as u32), &mut out, a3 as usize)
        }
        SYS_SHM_CREATE => sys_shm_create(a1),
        SYS_SHM_MAP => sys_shm_map(RawHandle(a1 as u32)),
        SYS_PORT_CREATE => sys_port_create(),
        SYS_NAMESPACE_BUILD => {
            let Ok(args) = ctx.copy_in::<NamespaceBuild>(UserAddr::new(a1)) else {
                return bad_addr;
            };
            sys_namespace_build(&ctx, &args)
        }
        SYS_NAMESPACE_OPEN => {
            let name = match ctx.user_str(UserAddr::new(a2), a3) { Ok(s) => s, Err(e) => return e.to_u64() };
            sys_namespace_open(RawHandle(a1 as u32), &name)
        }
        SYS_TLS_ALLOC_BLOCK => sys_tls_alloc_block(a1),
        SYS_INBOX_SETUP => sys_inbox_setup(&ctx, a1 as u32, a2),
        SYS_INBOX_SUBMIT => {
            sys_inbox_submit(RawHandle(a1 as u32), a2 as u32, a3 as u32, a4)
        }
        SYS_QUERY_MODULES => {
            let Some(mut buf) = ctx.user_bytes_mut(UserAddr::new(a1), a2) else { return bad_addr };
            sys_query_modules(&mut buf)
        }
        // **The whole of `SYS_DEBUG` is here or it is nowhere.** A shipping
        // kernel carries no debug syscall: the number falls to the dispatch's
        // default and answers `InvalidArgument`, which is what an unassigned
        // number answers, so there is nothing for a process to reach and
        // nothing for it to discover. Every action below is a *test's* only
        // route to a state the host cannot stage, and four of them cost the
        // caller its process or the machine its CPUs by design — which is why
        // the feature is the boundary rather than a capability check inside it.
        #[cfg(feature = "test-actuators")]
        SYS_DEBUG => match a1 {
            DA::PANIC => panic!("SYS_DEBUG: kernel panic triggered by userspace"),
            // SAFETY: **this one is unsound by design and that is the whole
            // action** — a null dereference in Ring 0, staged so a test can see
            // what the kernel does with a fault the kernel itself caused. It is
            // behind `test-actuators`, which no shipped kernel is built with.
            // Volatile so the read is actually emitted; a plain one the
            // optimiser may fold to `unreachable` and then nothing faults.
            DA::NULL_READ => { unsafe { core::ptr::read_volatile(core::ptr::null::<u64>()); } 0 }
            DA::LOCK_ACROSS_SWITCH => {
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
            DA::FATAL_HALT => { log!("{}", FATAL_HALT_NONCE); crate::arch::apic::halt_all_cpus(); }
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
            DA::DOUBLE_FAULT => {
                log!("SYS_DEBUG: provoking a double fault");
                // SAFETY: unsound by design, like `NULL_READ` above and behind
                // the same feature — the comment on this arm is the argument for
                // why only the hardware can produce the state under test.
                // `options(noreturn)` is honest: the `push` raises `#SS` on a
                // non-canonical `rsp`, delivering that needs another push to the
                // same `rsp`, and the `#DF` that follows never comes back here.
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
            DA::HEAP_AT_CEILING => debug_heap_alloc(crate::mm::MAX_HEAP_ALLOC, 8),
            DA::HEAP_OVER_CEILING => debug_heap_alloc(crate::mm::PAGE_2M as usize, 8),
            DA::HEAP_AT_CEILING_PAGE_ALIGNED => debug_heap_alloc(crate::mm::MAX_HEAP_ALLOC, 4096),
            // Returns, unlike every other action here: what is under test is
            // what the *console* does next, so the machine and the process
            // both have to survive being drawn over.
            DA::SCREEN_GRAFFITI => {
                crate::drivers::panic_console::graffiti();
                0
            }
            // A read, not a write: the property under test is that the page is
            // absent, and a read establishes it without the feature also
            // handing userland a kernel store. Returning 0 is the failure —
            // without a guard page that byte is dlmalloc's bookkeeping for the
            // chunk the idle stack lives in, and the read succeeds.
            DA::IDLE_GUARD_READ => {
                let addr = super::percpu::idle_guard_byte();
                log!("SYS_DEBUG: reading the idle stack guard at {addr:#x}");
                // SAFETY: the third of this feature's deliberate faults. The
                // address is `percpu::idle_guard_byte()`, the last byte of a page
                // `alloc_idle_stack` took out of the direct map, so the read is
                // the `#PF` the test is asserting on — and a kernel *without* the
                // guard returns from it, which is the failure. Volatile because
                // the value is discarded and a plain read would be deleted.
                unsafe { core::ptr::read_volatile(addr as *const u8) };
                0
            }
            DA::CANARY_ADDR => canary::address(),
            DA::CANARY_CHANGED => canary::changed() as u64,
            // Make the last CPU a shootdown waits for answer `a2` nanoseconds
            // late, take one, and answer with how long it took. The number is
            // the gate: an initiator that does not wait measures roughly the
            // cost of one ICR write however slow its siblings are. The arming
            // outlives the call, so the caller can then time an ordinary
            // syscall and learn whether *its* free path shoots down.
            DA::TLB_ACK_DELAY_ARM => crate::arch::tlb::debug_arm_ack_delay(a2),
            DA::TLB_ACK_DELAY_DISARM => crate::arch::tlb::debug_disarm_ack_delay(),
            // The live-object count for one kind. The two leaks this object
            // model accepts — an `Arc` stranded on a killed thread's stack, and
            // the cross-pair connection cycle — are visible in no other way, so
            // the census needs a reader or it is a counter nothing ever reads.
            //
            // Per kind and not a total: a total hides a leak of one kind behind
            // churn in another, and six of the thirteen kinds had no census
            // assertion anywhere in the estate.
            //
            // The names are checked rather than the order assumed: this is the
            // one place two declarations of the same list meet, and an index
            // that quietly names its neighbour is worse than no census.
            DA::CENSUS_KIND => {
                assert!(
                    crate::object::census::live()
                        .map(|(kind, _)| kind)
                        .eq(toyos_abi::syscall::OBJECT_KINDS.iter().copied()),
                    "kobject! and toyos_abi::syscall::OBJECT_KINDS declare different objects",
                );
                match crate::object::census::live().nth(a2 as usize) {
                    Some((_, live)) => live,
                    None => SyscallError::InvalidArgument.to_u64(),
                }
            }
            DA::IDLE_STACK_HIGH_WATER => super::percpu::idle_stack_high_water() as u64,
            DA::IDLE_STACK_SIZE => super::percpu::idle_stack_size() as u64,
            // Lower `SYS_SYSINFO`'s thread bound to `GATED_SYSINFO_THREADS`
            // for the rest of the boot. The bound itself is unreachable — no
            // guest can make 65,536 threads — and this is the only way to run
            // the refusal against the shipped count, comparison and error
            // return. Armed rather than compiled, because as a `#[cfg]` it
            // travelled into every kernel this suite booted.
            DA::LOWER_SYSINFO_BOUND => {
                SYSINFO_BOUND_LOWERED.store(true, core::sync::atomic::Ordering::Relaxed);
                0
            }
            // Put one of the caller's own free slots one lifecycle from the
            // end, and answer the handle its next install will carry. A slot's
            // counter is twenty bits, so the only other way to a table at the
            // end of one is 1,048,575 close/reopen round trips: the retirement
            // ruling of 2026-08-20 would be gated by a test nobody could afford
            // to run, which is how it went ungated. Nothing here is simulated —
            // what follows the call is the shipped install, the shipped close
            // and `HandleTable::retire`'s own decision.
            //
            // It acts on the caller's own table and confers nothing: a process
            // can already close its own handles, and all this says is which
            // generation the next one comes back at.
            DA::SLOT_TO_LAST_GENERATION => {
                let Ok(slot) = u16::try_from(a2) else {
                    return SyscallError::InvalidArgument.to_u64();
                };
                match process::with_process_data(|d| d.handles.stage_last_generation(slot)) {
                    Some(h) => u64::from(h.0),
                    None => SyscallError::InvalidArgument.to_u64(),
                }
            }
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
            sys_process_stats(&ctx, RawHandle(a1 as u32), addr)
        },
        SYS_SET_THREAD_NAME => {
            // `a2` used to be clamped to `THREAD_NAME_LEN` with `.min`, which
            // silently set the truncated prefix of a name too long to fit
            // rather than telling the caller its name did not fit —
            // `issues/isolation/untrusted-sites-not-yet-adopted.md`'s
            // pattern for the whole file. Refused by name instead: a length
            // past the bound is the caller's argument being wrong, not a
            // shorter name to go write.
            let Ok(len) = Untrusted::new(a2).at_most(process::THREAD_NAME_LEN as u64) else {
                return SyscallError::InvalidArgument.to_u64();
            };
            let len = len as usize;
            let Some(bytes) = ctx.user_bytes(UserAddr::new(a1), len as u64) else {
                return bad_addr;
            };
            let mut name = [0u8; process::THREAD_NAME_LEN];
            bytes.read_at(0, &mut name[..len]);
            process::set_current_thread_name(&name[..len]);
            0
        },
        SYS_DEVICE_REG_READ => sys_device_reg(RawHandle(a1 as u32), a2, a3, None),
        SYS_DEVICE_REG_WRITE => sys_device_reg(RawHandle(a1 as u32), a2, a3, Some(a4)),
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

/// Run `f` on the object a handle names, under the table's own guard.
///
/// The one shape every handle-taking syscall uses. `f` runs while the process
/// data is held, exactly where the descriptor dispatch used to run, so nothing
/// clones an `Arc` out for a call that is over before the guard is.
fn with_object_ref<R>(
    h: RawHandle,
    need: Rights,
    f: impl FnOnce(&KObjectRef) -> R,
) -> Result<R, crate::object::HandleError> {
    process::with_process_data(|data| data.handles.get_ref(h, need).map(f))
}

/// The gate on every syscall that drives a claimed device.
///
/// **The handle is the authority, and the class is what it is a claim on.** A
/// process holding the NIC has no more business setting the resolution than one
/// holding nothing, which is why the class is checked and not merely the type —
/// the same `PermissionDenied` a wrong-typed handle gets from the table.
fn holds_claim(
    h: RawHandle,
    class: device::DeviceType,
) -> Result<(), crate::object::HandleError> {
    // A claim on the wrong device is a wrong-typed handle and says so: what a
    // `DeviceClaim` *is* is a claim on one class, and a caller presenting the
    // NIC to `SYS_GPU_PRESENT` has the same bug as one presenting a pipe.
    let held = with_object_ref(h, Rights::WRITE, |object| match object {
        KObjectRef::Device(d) => Ok(d.class()),
        other => Err(crate::object::HandleError::WrongType {
            held: other.kind(),
            wanted: "Device",
        }),
    })??;
    if held == class {
        Ok(())
    } else {
        Err(crate::object::HandleError::WrongType {
            held: held.class_name(),
            wanted: class.class_name(),
        })
    }
}

/// The two `u32`s a device call packs into one argument word, taken apart at
/// the boundary and carried no further.
fn unpair(word: u64) -> (u32, u32) {
    ((word >> 32) as u32, word as u32)
}

/// The same, for the calls whose answer is already a raw syscall word.
fn with_object(h: RawHandle, need: Rights, f: impl FnOnce(&KObjectRef) -> u64) -> u64 {
    match with_object_ref(h, need, f) {
        Ok(v) => v,
        Err(e) => e.refuse(),
    }
}

fn sys_write(h: RawHandle, buf: &UserBytes) -> u64 {
    loop {
        let action = process::with_process_data(|data| {
            let object = match data.handles.get_ref(h, Rights::WRITE) {
                Ok(object) => object,
                Err(e) => return Err(WriteBlock::BadHandle(e)),
            };
            match ops::try_write(object, buf) {
                Some(n) => Ok((n, ops::pipe_id_write(object))),
                None => Err(match ops::pipe_id_write(object) {
                    Some(id) => WriteBlock::Pipe(id),
                    None => WriteBlock::Refused(SyscallError::NotFound.to_u64()),
                }),
            }
        });
        match action {
            Ok((n, pipe_id)) => {
                if let Some(id) = pipe_id { process::wake_pipe_readers(id); }
                return n;
            }
            Err(WriteBlock::Pipe(id)) => match pipe::writers_queue(id) {
                Some(end) => {
                    let parkable = crate::scheduler::Parkable::at_entry();
                    if completion::wait_until(
                        &parkable,
                        completion::Subject::of(&end.watch),
                        completion::Token::new(0),
                        WaitClass::Pipe,
                        Deadline::never(),
                        || pipe::has_space(id),
                    )
                    .is_err()
                    {
                        return cancelled();
                    }
                }
                None => return SyscallError::NotFound.to_u64(),
            },
            Err(WriteBlock::Refused(word)) => return word,
            Err(WriteBlock::BadHandle(e)) => return e.refuse(),
        }
    }
}

/// What `sys_write` does when the object took nothing.
enum WriteBlock {
    Pipe(pipe::PipeId),
    Refused(u64),
    /// Carried out of the process's lock rather than answered inside it:
    /// `HandleError::refuse` may take the process down and cannot run under a
    /// guard.
    BadHandle(crate::object::HandleError),
}

/// What `sys_read` parks on when the handle has nothing to give. Each variant
/// carries what its own re-check needs — the queue is registered on *before*
/// the condition is re-read, which is what closes the check-then-block window.
enum ReadBlock {
    Pipe(pipe::PipeEnd, pipe::PipeId),
    VirtioSound,
    Hda,
    /// A console read re-polls, because nothing posts a serial key; a claimed
    /// keyboard is woken by its own IRQ and waits with [`Deadline::never`].
    Keyboard(Deadline),
    /// Nothing to wait for: the answer is this word.
    Refused(u64),
    /// Carried out of the process's lock rather than answered inside it:
    /// `HandleError::refuse` may take the process down and cannot run under a
    /// guard.
    BadHandle(crate::object::HandleError),
}

/// Which stub a claimed device handle names, and nothing about what it drives.
enum RegTarget {
    Hda,
    VirtioSound,
}

/// One register of a claimed device, read or written.
///
/// The handle is the authorization and the device behind it owns the
/// allow-list, so
/// this function knows nothing about codecs or virtqueues — which is the test
/// for it being a device-register call rather than a device protocol smuggled
/// back into the syscall table. Two stubs answer it
/// now, which is the first evidence for that claim rather than a restatement of
/// it.
fn sys_device_reg(handle: RawHandle, offset: u64, width: u64, value: Option<u64>) -> u64 {
    let Some(width) = toyos_abi::syscall::RegWidth::from_raw(width) else {
        return SyscallError::InvalidArgument.to_u64();
    };
    // **The table's own rule, and not one invented here.** This answered
    // `NotFound` for every way the handle could fail to resolve, so a process
    // naming a slot it never held — or one it had closed — was told its device
    // was missing, where `SYS_DEVICE_CLAIM` beside it ends the caller for the
    // same mistake (`object::HandleError::refuse_as_error`). `get` is asked
    // for the type, so a pipe presented here is the `WrongType` that it is.
    let target = process::with_process_data(|data| {
        data.handles
            .get::<crate::object::device::DeviceClaim>(handle, Rights::NONE)
            .map(|claim| match claim.class() {
                device::DeviceType::HdaAudio => Some(RegTarget::Hda),
                device::DeviceType::VirtioSound => Some(RegTarget::VirtioSound),
                _ => None,
            })
    });
    // Nothing held: `with_process_data` has given the guard up, which is what
    // `refuse` requires of the three kinds that do not come back from it.
    let target = match target {
        Ok(t) => t,
        Err(e) => return e.refuse(),
    };
    // A claim of a class with no register window. A different fact from "no
    // such device", and the one word left here that is not a lie.
    let Some(target) = target else {
        return SyscallError::NotSupported.to_u64();
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
        Some(value) => match u32::try_from(value) {
            Ok(value) => {
                let written = match target {
                    RegTarget::Hda => crate::drivers::hda::reg_write(offset, width, value),
                    RegTarget::VirtioSound => {
                        crate::drivers::virtio_sound::reg_write(offset, width, value)
                    }
                };
                match written {
                    Ok(()) => 0,
                    Err(e) => e.to_u64(),
                }
            }
            Err(_) => SyscallError::InvalidArgument.to_u64(),
        },
    }
}

/// Where a read that came back empty parks, decided from the object itself.
///
/// Only these four wait. A mouse, a NIC and a framebuffer answer `NotFound` to
/// a blocking read that has nothing — which is what they did as descriptors,
/// and what their holders already build around.
fn read_block_device(claim: &crate::object::device::DeviceClaim) -> ReadBlock {
    match claim.class() {
        device::DeviceType::Keyboard => ReadBlock::Keyboard(Deadline::never()),
        device::DeviceType::VirtioSound if claim.info_read() => ReadBlock::VirtioSound,
        device::DeviceType::HdaAudio if claim.info_read() => ReadBlock::Hda,
        _ => ReadBlock::Refused(SyscallError::NotFound.to_u64()),
    }
}

fn read_block(object: &KObjectRef) -> ReadBlock {
    match object {
        KObjectRef::Device(_) => unreachable!("a device claim blocks via `read_block_device`"),
        KObjectRef::Console(_) => {
            /// **The only reason a serial-console read ever returns.**
            /// The park is on `waitqs::KEYBOARD`, whose only waker is the
            /// i8042/USB keyboard — a *different* device from the one this
            /// read is about. The 16550's IER is written to zero and
            /// `virtio_console` has no handler at all, so nothing posts a
            /// serial key and what ends this wait is the re-poll and nothing
            /// else. It is the [`Cadence`] of a poll on
            /// `serial::has_data`, which C10 makes explicit.
            const CONSOLE_REPOLL: Cadence = Cadence::every(
                Duration::from_millis(10),
                "nothing posts a serial-console key, so this rate is the whole of the wake",
            );
            ReadBlock::Keyboard(Deadline::at(crate::clock::now() + CONSOLE_REPOLL.duration()))
        }
        _ => match ops::pipe_id_read(object).and_then(|id| {
            pipe::readers_queue(id).map(|end| ReadBlock::Pipe(end, id))
        }) {
            Some(block) => block,
            None => ReadBlock::Refused(SyscallError::NotFound.to_u64()),
        },
    }
}

fn sys_read(h: RawHandle, buf: &mut UserBytesMut) -> u64 {
    loop {
        let action = process::with_process_data(|data| {
            let object = match data.handles.get_ref(h, Rights::READ) {
                Ok(object) => object,
                Err(e) => return Err(ReadBlock::BadHandle(e)),
            };
            // A device description installs handles for the buffers it names,
            // so that one arm needs the table mutably and the borrow above has
            // to end first. Resolving twice costs a slot lookup on a path that
            // runs once per device per boot; cloning the `Arc` on the common
            // path would cost an atomic read-modify-write on the hottest
            // syscall in the kernel.
            if matches!(object, KObjectRef::Device(_)) {
                let claim = data
                    .handles
                    .get::<crate::object::device::DeviceClaim>(h, Rights::READ)
                    .expect("a Device resolved a moment ago under this same hold");
                let blocked = read_block_device(&claim);
                return match ops::read_device(&claim, &mut data.handles, buf) {
                    Some(n) => Ok((n, None)),
                    None => Err(blocked),
                };
            }
            match ops::try_read(object, buf) {
                Some(n) => Ok((n, ops::pipe_id_read(object))),
                None => Err(read_block(object)),
            }
        });
        match action {
            Ok((n, pipe_id)) => {
                if let Some(id) = pipe_id { process::wake_pipe_writers(id); }
                return n;
            }
            Err(ReadBlock::Pipe(end, id)) => {
                let parkable = crate::scheduler::Parkable::at_entry();
                if completion::wait_until(
                    &parkable,
                    completion::Subject::of(&end.watch),
                    completion::Token::new(0),
                    WaitClass::Pipe,
                    Deadline::never(),
                    || pipe::has_data(id),
                )
                .is_err()
                {
                    return cancelled();
                }
            }
            Err(ReadBlock::VirtioSound) => {
                let parkable = crate::scheduler::Parkable::at_entry();
                if completion::wait_until(
                    &parkable,
                    completion::Subject::of(&crate::sched::waitqs::AUDIO_WATCH),
                    completion::Token::new(0),
                    WaitClass::Io,
                    Deadline::never(),
                    crate::drivers::virtio_sound::has_pending,
                )
                .is_err()
                {
                    return cancelled();
                }
            }
            Err(ReadBlock::Hda) => {
                let parkable = crate::scheduler::Parkable::at_entry();
                if completion::wait_until(
                    &parkable,
                    completion::Subject::of(&crate::sched::waitqs::AUDIO_WATCH),
                    completion::Token::new(0),
                    WaitClass::Io,
                    Deadline::never(),
                    crate::drivers::hda::has_pending,
                )
                .is_err()
                {
                    return cancelled();
                }
            }
            Err(ReadBlock::Keyboard(deadline)) => {
                let parkable = crate::scheduler::Parkable::at_entry();
                if completion::wait_until(
                    &parkable,
                    completion::Subject::of(&crate::sched::waitqs::KEYBOARD_WATCH),
                    completion::Token::new(0),
                    WaitClass::Io,
                    deadline,
                    crate::keyboard::has_data,
                )
                .is_err()
                {
                    return cancelled();
                }
            }
            Err(ReadBlock::Refused(word)) => return word,
            Err(ReadBlock::BadHandle(e)) => return e.refuse(),
        }
    }
}

/// Whether `flags` ask for anything that can change what is on the volume.
///
/// `WRITE` alone is not the question: `CREATE` makes a file, `TRUNCATE`
/// destroys one's contents, and `APPEND` is a write position. A read-only open
/// of a `KernelOnly` mount is fine and stays fine — the handle it hands back has
/// `writable` false, so nothing downstream needs a second check.
fn open_modifies(flags: OpenFlags) -> bool {
    flags.contains(OpenFlags::WRITE)
        || flags.contains(OpenFlags::CREATE)
        || flags.contains(OpenFlags::TRUNCATE)
        || flags.contains(OpenFlags::APPEND)
}

fn sys_open(path: &str, flags: OpenFlags) -> u64 {
    let cwd = process::with_process_data(|d| d.cwd.clone());
    let resolved = vfs::lock().resolve_absolute(&cwd, path);
    if open_modifies(flags) && !vfs::lock().user_may_modify(&resolved) {
        return SyscallError::PermissionDenied.to_u64();
    }
    process::with_process_data(|data| ops::open(&mut data.handles, &resolved, flags))
}

/// **Closing a handle wakes nobody, and that is the whole of it.**
///
/// A handle to a pipe end is not the end. `pipe::close_write` decrements the
/// reference count and wakes readers when it reaches *zero* — the one place
/// that knows whether the writer is gone — and the release that gets there runs
/// off this call's own zero-handle drain. A second wake here fired on *every*
/// close, so a pipe with a live writer and no bytes in it was announced
/// readable; a one-shot inbox watch consumed on that never fires again.
fn sys_close(h: RawHandle) -> u64 {
    let result = process::with_process_data(|data| {
        ops::close(&mut data.handles, h, &mut data.pipe_maps)
    });
    match result {
        Ok(()) => 0,
        Err(e) => e.refuse(),
    }
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
    let cwd = process::with_process_data(|d| d.cwd.clone());
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
    let cwd = process::with_process_data(|d| d.cwd.clone());
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
    let cwd = process::with_process_data(|d| d.cwd.clone());
    match vfs::lock().cd(&cwd, path) {
        Ok(new_cwd) => {
            process::with_process_data(|d| d.cwd = new_cwd);
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
    process::with_process_data(|data| {
        let cwd = data.cwd.as_bytes();
        if cwd.len() <= out.len() {
            out.write_at(0, cwd);
        }
        cwd.len() as u64
    })
}

fn handle_result(r: Result<RawHandle, SyscallError>) -> u64 {
    match r {
        Ok(h) => h.0 as u64,
        Err(e) => e.to_u64(),
    }
}

fn sys_pipe() -> u64 {
    let (reader, writer) = pipe::create();
    let read_end = KObjectRef::PipeRead(crate::object::pipe::PipeReadEnd::new(reader));
    let write_end = KObjectRef::PipeWrite(crate::object::pipe::PipeWriteEnd::new(writer));
    process::with_process_data(|data| {
        let Ok(read_h) = ops::install(&mut data.handles, read_end) else {
            return SyscallError::ResourceExhausted.to_u64();
        };
        let Ok(write_h) = ops::install(&mut data.handles, write_end) else {
            ops::close(&mut data.handles, read_h, &mut data.pipe_maps)
                .expect("the read end this call installed a moment ago");
            return SyscallError::ResourceExhausted.to_u64();
        };
        ((read_h.0 as u64) << 32) | write_h.0 as u64
    })
}

/// Map a pipe's ring page into the caller.
///
/// The window is recorded against the pipe (`process::PipeMap`) so that
/// closing the last descriptor for it takes the mapping away. It used to be
/// recorded nowhere: `SYS_PIPE`, `SYS_PIPE_MAP`, close both ends freed the ring
/// page back to the PMM with the caller's writable mapping of it still live,
/// and whatever the PMM handed that page to next — another process's pipe, a
/// kernel heap region, a DMA buffer — was readable and writable by a process
/// that owned nothing.
///
/// A second call for the same pipe returns the window the first one made,
/// rather than a second window onto the same page. That is what keeps
/// `pipe_maps` bounded by the descriptor table.
fn sys_pipe_map(h: RawHandle) -> u64 {
    let mapped = process::with_process_data(|data| {
        let pipe_id = match data.handles.get_ref(h, Rights::MAP) {
            Ok(object) => ops::pipe_id_read(object).or_else(|| ops::pipe_id_write(object)),
            Err(e) => return Err(e),
        };
        let Some(pipe_id) = pipe_id else {
            return Ok(SyscallError::InvalidArgument.to_u64());
        };
        if let Some(existing) = data.pipe_maps.iter().find(|m| m.pipe == pipe_id) {
            return Ok(existing.addr.raw());
        }
        let Some(phys) = pipe::map_page(pipe_id) else {
            return Ok(SyscallError::ResourceExhausted.to_u64());
        };
        let pt = crate::scheduler::current_address_space()
            .expect("sys_pipe_map: no address space");
        let Some((vaddr, _aligned)) = process::vma_map(&pt, phys.phys(), pipe::PIPE_SIZE as u64, Prot::ReadWrite) else {
            return Ok(SyscallError::ResourceExhausted.to_u64());
        };
        data.pipe_maps.push(process::PipeMap { pipe: pipe_id, addr: vaddr });

        Ok(vaddr.raw())
    });
    match mapped {
        Ok(word) => word,
        Err(e) => e.refuse(),
    }
}

/// Join a pipe read end and a pipe write end into one duplex connection.
///
/// The caller must already hold both, in the right direction, and keeps them:
/// this takes references of its own. It grants nothing — everything it can
/// reach is something the caller could already read or write — which is what
/// lets it be this simple where its id-addressed predecessor needed a rule
/// about who was entitled to a number.
///
/// `std`'s `TcpStream` is one handle and netd's data path is two pipes, and
/// that is the whole of why this exists.
fn sys_connection_join(rx_h: RawHandle, tx_h: RawHandle) -> u64 {
    let ends = process::with_process_data(|data| {
        let rx = data.handles.get::<crate::object::pipe::PipeReadEnd>(rx_h, Rights::READ)?;
        let tx = data.handles.get::<crate::object::pipe::PipeWriteEnd>(tx_h, Rights::WRITE)?;
        Ok::<_, crate::object::HandleError>((rx.reference(), tx.reference()))
    });
    let (rx, tx) = match ends {
        Ok(ends) => ends,
        Err(e) => return e.refuse(),
    };
    let object = KObjectRef::Connection(crate::object::service::ConnectionEnd::joined(rx, tx));
    process::with_process_data(|data| handle_result(ops::install(&mut data.handles, object)))
}

fn sys_read_nonblock(h: RawHandle, buf: &mut UserBytesMut) -> u64 {
    let result = process::with_process_data(|data| {
        let object = match data.handles.get_ref(h, Rights::READ) {
            Ok(object) => object,
            Err(e) => return Err(e),
        };
        // The two-step [`sys_read`] makes, for the same reason: a device
        // description installs handles for the buffers it names, so that arm
        // needs the table mutably and the borrow above has to end first. This
        // is the path the compositor reads its keyboard and its mouse on.
        if matches!(object, KObjectRef::Device(_)) {
            let claim = data
                .handles
                .get::<crate::object::device::DeviceClaim>(h, Rights::READ)
                .expect("a Device resolved a moment ago under this same hold");
            return Ok((ops::read_device(&claim, &mut data.handles, buf), None));
        }
        Ok((ops::try_read(object, buf), ops::pipe_id_read(object)))
    });
    match result {
        Ok((Some(n), wake)) => {
            if let Some(id) = wake { process::wake_pipe_writers(id); }
            n
        }
        Ok((None, _)) => SyscallError::WouldBlock.to_u64(),
        Err(e) => e.refuse(),
    }
}

fn sys_write_nonblock(h: RawHandle, buf: &UserBytes) -> u64 {
    let result = with_object_ref(h, Rights::WRITE, |object| {
        (ops::try_write(object, buf), ops::pipe_id_write(object))
    });
    match result {
        Ok((Some(n), wake)) => {
            if let Some(id) = wake { process::wake_pipe_readers(id); }
            n
        }
        Ok((None, _)) => SyscallError::WouldBlock.to_u64(),
        Err(e) => e.refuse(),
    }
}

/// Start a program and answer a handle to it.
///
/// The child is spawned before the handle is installed, so a caller whose table
/// is full has already made a process — and one nobody can name is one nobody
/// can wait for or kill. It is killed rather than left running, which is what
/// makes the answer "no process was started" true.
fn sys_spawn(
    text: &str,
    pending: crate::loader::PendingHandles,
    env: alloc::vec::Vec<u8>,
) -> u64 {
    let args: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
    let cwd = process::with_process_data(|data| data.cwd.clone());
    // Refused with this frame holding nothing: `spawn`'s own frame owned the
    // child's address space and its stacks, and the three handle kinds that end
    // the caller do so from `refuse`.
    let object = match process::spawn(&args, pending, cwd, env) {
        Ok(object) => object,
        Err(e) => return e.refuse(),
    };
    let installed = process::with_process_data(|data| {
        ops::install(&mut data.handles, KObjectRef::Process(object.clone()))
    });
    match installed {
        Ok(h) => h.0 as u64,
        Err(e) => {
            process::kill_process(&object);
            e.to_u64()
        }
    }
}

/// Take a process's exit code, blocking until there is one.
///
/// **The code is on the object, so this is a read and not a claim.** Two
/// threads may wait on one process and both get the code; a wait long after the
/// process is gone gets it too. `WNOHANG` is the same question with the park
/// taken out.
fn sys_process_wait(h: RawHandle, flags: u64) -> u64 {
    let object = match process::with_process_data(|data| {
        data.handles.get::<crate::object::process::ProcessObject>(h, Rights::WAIT)
    }) {
        Ok(object) => object,
        Err(e) => return e.refuse(),
    };
    if flags & WNOHANG == 0 {
        let parkable = crate::scheduler::Parkable::at_entry();
        if completion::wait_until(
            &parkable,
            completion::Subject::of(object.watch()),
            completion::Token::new(0),
            WaitClass::Other,
            Deadline::never(),
            || object.finished(),
        )
        .is_err()
        {
            return cancelled();
        }
    }
    match object.exit_code() {
        // Zero-extended: an exit code is an `i32`, and sign-extending -1 would
        // land on `SyscallError`'s encoding.
        Some(code) => code as u32 as u64,
        // One answer for both arms, and the blocking one used to `expect` here.
        // `publish_exit` fills the slot before it stores `finished`, and the
        // wait above returns only when `finished` holds, so this is now
        // unreachable — but it is reachable *from userland*, which is the whole
        // reason it may not be an assertion: a wait that came back without its
        // condition is a refusal the caller already handles (it is what
        // `WNOHANG` answers), never a kernel that dies holding a userland
        // thread's syscall.
        None => SyscallError::WouldBlock.to_u64(),
    }
}

/// A `Process` handle for a pid, presenting a `SysCap` that carries
/// [`Rights::MANAGE`].
///
/// The one place a pid becomes authority over anything, and the kernel mints
/// exactly one cap that carries the right — so what can reach a process it did
/// not start is exactly what init endowed.
fn sys_process_open(syscap: RawHandle, pid: process::Pid) -> u64 {
    if let Err(e) = process::with_process_data(|data| {
        data.handles.get::<crate::object::syscap::SysCap>(syscap, Rights::MANAGE)
    }) {
        return e.refuse();
    }
    let Some(object) = process::process_object(pid) else {
        return SyscallError::NotFound.to_u64();
    };
    process::with_process_data(|data| {
        handle_result(ops::install(&mut data.handles, KObjectRef::Process(object)))
    })
}

/// Mint the claim for a device class, presenting a `SysCap` that carries
/// [`Rights::DEVICE`].
///
/// The kernel makes one such cap, at boot, for `/bin/init`, so the set of
/// processes that can reach this at all is exactly what init endowed. What
/// arbitrates between two programs wanting the framebuffer is then the
/// manifest, checked before the image was built, rather than which of them
/// started first.
fn sys_device_claim(syscap: RawHandle, class: u64) -> u64 {
    let Some(class) = device::DeviceType::from_raw(class) else {
        return SyscallError::InvalidArgument.to_u64();
    };
    if let Err(e) = process::with_process_data(|data| {
        data.handles.get::<crate::object::syscap::SysCap>(syscap, Rights::DEVICE)
    }) {
        return e.refuse();
    }
    // `NotFound` is a machine with no such device and nothing else: init endows
    // what it got and logs what it did not, which is a different answer from
    // `AlreadyExists` — a config the build-time gate should have refused.
    let claim = match device::try_claim(class) {
        Ok(c) => c,
        Err(device::ClaimError::Absent) => return SyscallError::NotFound.to_u64(),
        Err(device::ClaimError::Owned) => return SyscallError::AlreadyExists.to_u64(),
    };
    process::with_process_data(|data| {
        handle_result(ops::install(&mut data.handles, KObjectRef::Device(claim)))
    })
}

/// Enter the real-time band, presenting a `SysCap` that carries
/// [`Rights::RT`].
///
/// The RT band has no priority above it, so unbounded threads in it starve
/// soundd's mix thread at its own level. It used to be gated on holding an
/// audio claim, which the dispatch's own comment called out as not a
/// privilege: whoever won the first-come race for the sound card got the band
/// with it. This is the privilege that comment asked for, and it is endowed
/// per manifest rather than won.
fn sys_rt_enter(syscap: RawHandle) -> u64 {
    if let Err(e) = process::with_process_data(|data| {
        data.handles.get::<crate::object::syscap::SysCap>(syscap, Rights::RT)
    }) {
        return e.refuse();
    }
    crate::scheduler::set_current_rt(true);
    0
}

/// Copy kernel log records into a caller's buffer, presenting a `SysCap` that
/// carries [`Rights::LOG`].
///
/// **Every record every CPU wrote, which is every process's business and no
/// process's right by default** — so it rides a right rather than being
/// ambient, exactly as minting a device claim and entering the RT band do.
///
/// The cursor is the caller's own memory in both directions: read once here,
/// walked by `log::user::read`, written back. **A cursor that cannot be written
/// back costs that caller the records this call took**, and nothing else — it
/// is the caller's own address, mapped a moment ago for the read, so a failure
/// is a process that unmapped its cursor under its own syscall.
fn sys_log_read(
    ctx: &SyscallContext,
    syscap: RawHandle,
    cursor_ptr: UserAddr,
    out: &mut UserBytesMut,
    capacity: usize,
) -> u64 {
    if let Err(e) = process::with_process_data(|data| {
        data.handles.get::<crate::object::syscap::SysCap>(syscap, Rights::LOG)
    }) {
        return e.refuse();
    }
    let mut cursor = match ctx.copy_in::<toyos_abi::log::LogCursor>(cursor_ptr) {
        Ok(cursor) => cursor,
        Err(e) => return e.to_u64(),
    };
    let count = match log::user::read(&mut cursor, out, capacity) {
        Ok(count) => count,
        Err(e) => return e.to_u64(),
    };
    match ctx.copy_out(cursor_ptr, &cursor) {
        Ok(()) => count as u64,
        Err(e) => e.to_u64(),
    }
}

/// Power the machine off, presenting a `SysCap` that carries
/// [`Rights::POWER`].
///
/// **The largest authority this kernel has, and the last one that was free.**
/// It took no argument at all: any process that could make a syscall could end
/// every other one, and a daemon endowed exactly one connector held this too.
/// It is checked the way the five beside it are — resolve the handle, demand
/// the right, refuse otherwise — so what can cut the power is exactly what
/// `/bin/init` endowed from `system.toml`, as minting a device claim, entering
/// the RT band, opening a process by pid and reading the log already were.
///
/// The refusal is `HandleError`'s ordinary one and not a special case: a
/// capability that resolves without the bit is `PermissionDenied` and the
/// caller carries on, and a handle the caller does not hold ends it.
///
/// Everything below the check is unchanged and does not come back.
fn sys_shutdown(syscap: RawHandle) -> u64 {
    if let Err(e) = process::with_process_data(|data| {
        data.handles.get::<crate::object::syscap::SysCap>(syscap, Rights::POWER)
    }) {
        return e.refuse();
    }
    log!("Syncing filesystems...");
    crate::vfs::lock().sync_all();
    log!("Shutting down.");
    // **§6.3, in order, and the order is the whole of it.** At the moment they
    // are written these last two lines exist nowhere but the shards, and
    // `acpi::shutdown` does not come back — so a shutdown that loses its own
    // last lines is the one nobody can diagnose, and on a machine with no
    // serial port they exist nowhere else at all.
    //
    // 1. Wait, bounded, for `/bin/logd` to make them durable. **This is
    //    ordinary thread context**, so it yields rather than spins: at
    //    `--smp 1` logd and this caller are the same CPU and a spin here would
    //    guarantee the bound expired every time.
    crate::log::wait_for_durable();
    // 2. The console, after logd has answered, so the last record — including
    //    logd's own — is on the wire before the power goes. Inline, because
    //    `klogd` has no guarantee of another turn.
    crate::log::console::drain_inline();
    acpi::shutdown();
}

/// Answer this process's endowment table.
///
/// An empty buffer asks how many bytes the answer needs, so a caller sizes once
/// and reads once. A short one is refused rather than truncated: half an
/// endowment table is not a smaller endowment table, it is a caller that would
/// go on to look up a label that is not in what it got.
fn sys_endowments(out: &mut crate::user_ptr::UserBytesMut) -> u64 {
    let data_arc = process::process_data();
    let data = data_arc.lock();
    let needed = data.endowments.encoded_len();
    if out.is_empty() {
        return needed as u64;
    }
    if out.len() < needed {
        return SyscallError::InvalidArgument.to_u64();
    }
    let mut buf = alloc::vec![0u8; needed];
    data.endowments.encode(&mut buf);
    drop(data);
    out.write_at(0, &buf);
    needed as u64
}

// Ports and namespaces

/// Make a port and install both ends.
///
/// Needs no right and grants none: a port with no clients is not authority.
/// The two handles come back packed, which cannot be read as an error — see
/// [`SYS_PORT_CREATE`].
fn sys_port_create() -> u64 {
    let (acceptor, connector) = port::create();
    process::with_process_data(|data| {
        let Ok(a) = ops::install(&mut data.handles, KObjectRef::Acceptor(acceptor)) else {
            return SyscallError::ResourceExhausted.to_u64();
        };
        let install_c =
            ops::install(&mut data.handles, KObjectRef::Connector(connector));
        let Ok(c) = install_c else {
            // The acceptor goes back, so a refused pair leaves no port half in
            // a table with nothing on the other side of it.
            drop(data.handles.remove(a));
            return SyscallError::ResourceExhausted.to_u64();
        };
        ((a.0 as u64) << 32) | c.0 as u64
    })
}

/// A namespace built from a base's kept names plus new bindings.
///
/// Every name is resolved against the base *before* anything is installed, and
/// every added connector is checked for `TRANSFER` first, so a refusal leaves
/// the caller's table exactly as it was.
fn sys_namespace_build(ctx: &SyscallContext, args: &NamespaceBuild) -> u64 {
    let total = args.keep_n.saturating_add(args.add_n);
    if total > MAX_NAMESPACE_ENTRIES as u64 {
        return SyscallError::InvalidArgument.to_u64();
    }
    if args.names_len > (MAX_NAMESPACE_ENTRIES * MAX_SERVICE_NAME) as u64 {
        return SyscallError::InvalidArgument.to_u64();
    }
    let names = match ctx.user_vec(UserAddr::new(args.names_ptr), args.names_len) {
        Ok(bytes) => bytes,
        Err(e) => return e.to_u64(),
    };
    let name_at = |off: u32, len: u32| -> Option<alloc::boxed::Box<str>> {
        let end = (off as usize).checked_add(len as usize)?;
        let bytes = names.get(off as usize..end)?;
        Some(alloc::string::String::from(core::str::from_utf8(bytes).ok()?).into_boxed_str())
    };

    let mut entries: Vec<(alloc::boxed::Box<str>, alloc::sync::Arc<port::Connector>)> =
        Vec::new();

    if args.keep_n > 0 {
        let Some(keep) = (args.keep_n as usize)
            .checked_mul(core::mem::size_of::<NameRef>())
            .and_then(|len| ctx.user_bytes(UserAddr::new(args.keep_ptr), len as u64))
        else {
            return SyscallError::BadAddress.to_u64();
        };
        let base = match process::with_process_data(|data| {
            data.handles.get::<crate::object::namespace::Namespace>(args.base, Rights::READ)
        }) {
            Ok(base) => base,
            Err(e) => return e.refuse(),
        };
        for i in 0..args.keep_n as usize {
            let mut raw = [0u8; 8];
            keep.read_at(i * 8, &mut raw);
            let off = u32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let len = u32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]);
            let Some(name) = name_at(off, len) else {
                return SyscallError::InvalidArgument.to_u64();
            };
            // A name the base does not carry is silently absent from the
            // child's: a parent narrowing a namespace is asking for an
            // intersection, and asking for a name it does not itself hold
            // grants nothing either way.
            if let Some(connector) = base.lookup(&name) {
                entries.push((name, connector.clone()));
            }
        }
    }

    if args.add_n > 0 {
        let Some(add) = (args.add_n as usize)
            .checked_mul(core::mem::size_of::<NamespaceEntry>())
            .and_then(|len| ctx.user_bytes(UserAddr::new(args.add_ptr), len as u64))
        else {
            return SyscallError::BadAddress.to_u64();
        };
        for i in 0..args.add_n as usize {
            let mut raw = [0u8; 16];
            add.read_at(i * 16, &mut raw);
            let off = u32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let len = u32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]);
            let handle = RawHandle(u32::from_ne_bytes([raw[8], raw[9], raw[10], raw[11]]));
            let Some(name) = name_at(off, len) else {
                return SyscallError::InvalidArgument.to_u64();
            };
            let connector = match process::with_process_data(|data| {
                data.handles.get::<port::Connector>(handle, Rights::TRANSFER)
            }) {
                Ok(c) => c,
                // **The one place a wrong type is not provably the caller's
                // bug.** An added connector is routinely one a *peer*
                // transferred — that is what a `provides` name is, and
                // `/bin/init`'s launcher builds a namespace out of handles a
                // client sent it. Faulting here let any process holding the
                // `launcher` connector end init by sending it a pipe.
                Err(crate::object::HandleError::WrongType { .. }) => {
                    return SyscallError::InvalidArgument.to_u64()
                }
                Err(e) => return e.refuse(),
            };
            entries.push((name, connector));
        }
    }

    let namespace = match crate::object::namespace::Namespace::build(entries) {
        Ok(ns) => ns,
        Err(crate::object::namespace::BuildError::TooMany) => {
            return SyscallError::InvalidArgument.to_u64()
        }
        Err(crate::object::namespace::BuildError::Duplicate) => {
            return SyscallError::AlreadyExists.to_u64()
        }
    };
    process::with_process_data(|data| {
        handle_result(ops::install(&mut data.handles, KObjectRef::Namespace(namespace)))
    })
}

/// Open a connection to `name` in the namespace `ns_h` holds.
///
/// **Two facts, two words.** A name this namespace does not carry is
/// `NotFound` — a statement about this process. A name whose port has closed is
/// `Gone` — a statement about the machine. Only the kernel can tell them apart,
/// so only the kernel may collapse them, and it does not.
fn sys_namespace_open(ns_h: RawHandle, name: &str) -> u64 {
    let connector = match process::with_process_data(|data| {
        let ns = data
            .handles
            .get::<crate::object::namespace::Namespace>(ns_h, Rights::READ)?;
        Ok::<_, crate::object::HandleError>(ns.lookup(name).cloned())
    }) {
        Ok(Some(c)) => c,
        Ok(None) => return SyscallError::NotFound.to_u64(),
        Err(e) => return e.refuse(),
    };
    connect_through(&connector)
}

/// The client half of a connection, and the server half queued on the port.
fn connect_through(connector: &port::Connector) -> u64 {
    if connector.closed() {
        return SyscallError::Gone.to_u64();
    }
    let (cs_reader, cs_writer) = pipe::create(); // client → server
    let (sc_reader, sc_writer) = pipe::create(); // server → client
    // Cross-wired here and nowhere else: what the client sends is what the
    // server receives, and the server's end is built out of the same two
    // queues when it accepts.
    let (to_server, to_client) = crate::object::service::ConnectionEnd::pair_queues();

    // The client's own end first. Installing it can fail on a full handle
    // table, and a connection queued for a server whose client never got a
    // handle is one the server accepts and finds already dead.
    let object = KObjectRef::Connection(crate::object::service::ConnectionEnd::new(
        sc_reader,          // client reads from server→client
        cs_writer,          // client writes to client→server
        to_client.clone(),  // and receives what the server sent
        to_server.clone(),
    ));
    let h = match process::with_process_data(|data| ops::install(&mut data.handles, object)) {
        Ok(h) => h,
        Err(e) => return e.to_u64(),
    };

    let queued = connector.push(port::PendingConnection {
        rx: cs_reader, // server reads from client→server
        tx: sc_writer, // server writes to server→client
        inbox: to_server,
        outbox: to_client,
    });
    if let Err(e) = queued {
        process::with_process_data(|data| {
            ops::close(&mut data.handles, h, &mut data.pipe_maps)
                .expect("the connection this call installed a moment ago");
        });
        return match e {
            port::PushError::Closed => SyscallError::Gone.to_u64(),
            port::PushError::QueueFull => SyscallError::ResourceExhausted.to_u64(),
        };
    }
    let port = connector.port();
    completion::post(
        completion::Subject::of(port.watch()),
        completion::Outcome::Ready,
    );
    let watchers = port.watchers();
    if !watchers.is_empty() {
        crate::inbox::complete_pending_for_event(
            &watchers,
            crate::inbox::Source::Port(port),
        );
    }
    h.0 as u64
}

/// Publish a new mode: fresh buffer handles, and the claim's description
/// replaced so a later read answers with them.
///
/// **The handles the old description named keep working.** Their objects hold
/// the old pages, so a compositor can keep blitting the screen it has until it
/// has mapped the one it just asked for — where the token registry took the
/// mapping away on this CPU and shot down before the pages were reissued,
/// which is a revocation this design does not have and does not need.
fn sys_gpu_reset_scanout(
    ctx: &SyscallContext,
    claim_h: RawHandle,
    gpu_info: crate::gpu::GpuInfo,
    info_out: UserAddr,
) -> u64 {
    let crate::gpu::GpuInfo { scanout, cursor, width, height, stride, pixel_format, flags } =
        gpu_info;
    let screen = device::Screen {
        info: toyos_abi::FramebufferInfo {
            scanout: [toyos_abi::HANDLE_INVALID; 2],
            cursor: toyos_abi::HANDLE_INVALID,
            width,
            height,
            stride,
            pixel_format,
            flags,
        },
        scanout,
        cursor,
    };
    device::set_framebuffer_info(screen.clone());
    let minted = process::with_process_data(|data| {
        let claim = data
            .handles
            .get::<crate::object::device::DeviceClaim>(claim_h, Rights::WRITE)?;
        Ok::<_, crate::object::Refusal>(
            claim.remint(&mut data.handles, device::framebuffer_info(screen))?,
        )
    });
    let minted = match minted {
        Ok(bytes) => bytes,
        Err(e) => return e.refuse(),
    };
    let Some(mut out) = ctx.user_bytes_mut(info_out, minted.len() as u64) else {
        return SyscallError::BadAddress.to_u64();
    };
    out.write_at(0, &minted);
    0
}

fn sys_accept(h: RawHandle) -> u64 {
    let acceptor = match process::with_process_data(|data| {
        data.handles.get::<port::Acceptor>(h, Rights::READ)
    }) {
        Ok(a) => a,
        Err(e) => return e.refuse(),
    };

    loop {
        if let Some(conn) = acceptor.pop() {
            // PipeReader/PipeWriter move from the queue into the connection. No
            // refcount change — ownership transfers.
            let object = KObjectRef::Connection(
                crate::object::service::ConnectionEnd::new(
                    conn.rx,
                    conn.tx,
                    conn.inbox,
                    conn.outbox,
                ),
            );
            let installed = process::with_process_data(|data| {
                ops::install(&mut data.handles, object)
            });
            return handle_result(installed);
        }
        // The last handle to this acceptor has gone — another thread of this
        // process closed it — so nothing will ever be queued again and the
        // condition below has become permanently false. Answering is the only
        // alternative to parking forever.
        if acceptor.closed() {
            return SyscallError::Gone.to_u64();
        }
        let parkable = crate::scheduler::Parkable::at_entry();
        if completion::wait_until(
            &parkable,
            completion::Subject::of(acceptor.watch()),
            completion::Token::new(0),
            WaitClass::Ipc,
            Deadline::never(),
            || acceptor.has_pending() || acceptor.closed(),
        )
        .is_err()
        {
            return cancelled();
        }
    }
}

/// Make a shared region and hand back the one handle to it.
///
/// The creator's handle carries `MAP`, `DUP` and `TRANSFER`: mapping is what a
/// region is for, and giving one away is the whole point of the object. There
/// is no grant list — being able to name it *is* being allowed to map it.
fn sys_shm_create(size: u64) -> u64 {
    let object = match crate::object::shm::SharedMemObject::create(size) {
        Ok(shm) => KObjectRef::SharedMem(shm),
        Err(e) => return e.to_u64(),
    };
    process::with_process_data(|data| handle_result(ops::install(&mut data.handles, object)))
}

fn sys_shm_map(h: RawHandle) -> u64 {
    let shm = match process::with_process_data(|data| {
        data.handles.get::<crate::object::shm::SharedMemObject>(h, Rights::MAP)
    }) {
        Ok(shm) => shm,
        Err(e) => return e.refuse(),
    };
    let pt = process::current_address_space();
    match shm.map_into(process::current_process(), &pt) {
        Ok(vaddr) => vaddr,
        Err(e) => e.to_u64(),
    }
}

/// Move handles to the peer of a connection, all or nothing.
///
/// Every handle is verified — it resolves, it carries `TRANSFER`, it is named
/// once, and it is not the connection itself — before any of them is removed,
/// and a peer's queue that refuses the batch afterwards hands it back. **So
/// every refusal leaves the caller's table exactly as it was**, which is what
/// makes `Gone` and `ResourceExhausted` honest: they are answers about the
/// peer, and a caller retrying or closing what it still holds is right rather
/// than fatal. Refusing to send the connection over itself is what keeps a
/// cross-pair reference cycle to two objects rather than one.
///
/// **Rights travel unchanged, `TRANSFER` included.** A move requires it and
/// carries it, so everything that can be moved can be moved on: the
/// non-transitive grant the pid ACL had is not expressible, and making it so
/// is a rights word on *both* move paths rather than on this one
/// (`issues/isolation/a-moved-handle-is-always-re-movable.md`).
fn sys_handle_send(conn_h: RawHandle, handles: &crate::user_ptr::UserBytes, count: usize) -> u64 {
    let mut wanted = [RawHandle(0); MAX_TRANSFER_HANDLES];
    for (i, slot) in wanted.iter_mut().enumerate().take(count) {
        let mut raw = [0u8; HANDLE_LEN];
        handles.read_at(i * HANDLE_LEN, &mut raw);
        *slot = RawHandle(u32::from_ne_bytes(raw));
    }
    let wanted = &wanted[..count];

    let sent = process::with_process_data(|data| {
        let conn = data
            .handles
            .get::<crate::object::service::ConnectionEnd>(conn_h, Rights::TRANSFER)?;
        for (i, h) in wanted.iter().enumerate() {
            if *h == conn_h || wanted[..i].contains(h) {
                return Err(SyscallError::InvalidArgument.into());
            }
            let rights = data.handles.rights_of(*h)?;
            if !rights.contains(Rights::TRANSFER) {
                return Err(SyscallError::PermissionDenied.into());
            }
        }
        // The peer's queue can still refuse, and both of its refusals are ones
        // a caller reads as "the handles did not go" — so they must not have
        // gone. `transfer` puts every entry back at its own number, under this
        // same hold, where nothing can observe the gap.
        data.handles
            .transfer(wanted, |batch| conn.send(batch))
            .map_err(crate::object::Refusal::Error)
    });
    match sent {
        Ok(()) => 0,
        Err(e) => e.refuse(),
    }
}

/// Take the oldest batch the peer sent. Never blocks; zero means none queued.
///
/// The whole thing runs under one hold of this process's table, so the batch
/// whose size was checked is the batch that is installed — the peer can only
/// add to the far end of the queue, and a sibling thread of this process is
/// serialised by the same lock.
///
/// **Every refusal is answered outside that hold**, per [`HandleError::refuse`]:
/// three of the five kinds end the caller, and ending it takes the same
/// non-reentrant lock this closure is running under.
///
/// [`HandleError::refuse`]: crate::object::HandleError::refuse
fn sys_handle_recv(
    conn_h: RawHandle,
    out: &mut crate::user_ptr::UserBytesMut,
    cap: usize,
) -> u64 {
    let taken = process::with_process_data(|data| {
        let conn = data
            .handles
            .get::<crate::object::service::ConnectionEnd>(conn_h, Rights::READ)?;
        // **Measured before it is taken, and both refusals leave it queued.**
        // A batch popped and then dropped is capabilities nobody can ask for
        // again, reported as an error a caller reads as "they did not arrive".
        // Only the peer pushes, and only to the far end, so the front this saw
        // is the front the pop takes — or the queue closed under it, which is
        // the same answer as an empty one.
        let Some(width) = conn.peek_width() else { return Ok(0) };
        if width > cap {
            return Err(SyscallError::InvalidArgument.into());
        }
        if !data.handles.has_room(width) {
            return Err(SyscallError::ResourceExhausted.into());
        }
        let Some(batch) = conn.recv_bounded(cap)? else { return Ok(0) };
        let count = batch.len();
        for (i, entry) in batch.into_iter().enumerate() {
            let h = data.handles.install(entry).expect("room was asked for first");
            out.write_at(i * HANDLE_LEN, &h.0.to_ne_bytes());
        }
        Ok::<_, crate::object::Refusal>(count as u64)
    });
    match taken {
        Ok(n) => n,
        Err(e) => e.refuse(),
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
///
/// `MmapFlags::FIXED` places the mapping at exactly `req_addr` rather than
/// wherever the placement search would put it, and the range it names is its
/// own to answer for: it may replace exactly one whole mapping this same
/// syscall made, and every other overlap — part of a region, several regions,
/// a range belonging to the loader or a device claim — is refused with
/// `InvalidArgument`. POSIX unmaps whatever is in the way and says nothing;
/// this kernel does not have that silence to give, and the address a C program
/// passes is as untrusted as any other syscall argument.
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
    // **Anonymous memory is never executable, and `MmapProt` has no bit that
    // asks for it.** There is no JIT in this system and no `mprotect` to turn
    // a page into code afterwards, so the heap, every guard page and every
    // `MAP_ANONYMOUS` arena a libc hands out are data — which is what makes a
    // stack or heap overflow a fault instead of a foothold.
    let mapping_prot = if prot.contains(MmapProt::WRITE) { Prot::ReadWrite } else { Prot::Read };

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
            || req_addr < crate::vma::alloc_floor()
            || end > crate::vma::ALLOC_CEILING
            || !toyos_userbound::in_user_half(req_addr, aligned as u64)
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
        let start = UserAddr::new(start);
        // Both ledgers move together, under both locks, in the order the
        // arm below established: the process data, then the address space.
        let replaced = process::with_process_data(|data| {
            let mut as_guard = pt.lock();
            // A placed mapping names its own range, so the question `find_gap`
            // answers for every other mapping has to be asked here — and it
            // was not asked at all. The mapping went into `mmap_regions` and
            // nowhere near `regions`, which is what the placement search
            // reads, so the next anonymous `mmap` was handed the range this
            // one was living in and `map_range` asserted on a present PDE:
            // three ordinary syscalls from any C program that passes
            // `MAP_FIXED`, and the machine was gone.
            //
            // One whole mapping of this process's own making is replaced — the
            // address keeps its meaning and changes what it names. Every other
            // overlap is refused: taking part of a region would need a split
            // the address space has no machinery for, and a range an ELF
            // segment, a library image, the stack or a shared window owns is
            // not `mmap`'s to take. Neither is honoured halfway, and neither
            // reaches `map_range`, whose assert is a kernel-bug assert again
            // rather than one syscall away.
            let replacing = match as_guard.occupancy(start, aligned as u64) {
                Occupancy::Free => None,
                Occupancy::Whole => {
                    let mine = data
                        .mmap_regions
                        .iter()
                        .position(|r| r.addr == start && r.size == aligned);
                    match mine {
                        Some(idx) => Some(idx),
                        None => return Err(SyscallError::InvalidArgument),
                    }
                }
                Occupancy::Partial => return Err(SyscallError::InvalidArgument),
            };
            // Out of both ledgers before the new mapping goes into either, so
            // `insert_region` is never asked to overlap and the pages of what
            // was there leave with it.
            let old = replacing.map(|idx| {
                let old = data.mmap_regions.swap_remove(idx);
                as_guard
                    .free_and_unmap(old.addr)
                    .expect("an mmap region is registered in the address space it was placed in");
                old
            });
            as_guard.insert_region(
                start,
                crate::vma::Region {
                    size: aligned as u64,
                    kind: crate::vma::RegionKind::Mapped,
                },
            );
            if let Some(pages) = &pages {
                as_guard.map_range(
                    start,
                    pages.phys(),
                    aligned as u64,
                    mapping_prot,
                    CachePolicy::DeferToMtrr,
                );
            }
            data.mmap_regions.push(process::MmapRegion {
                addr: start, size: aligned, _pages: pages,
            });
            data.alloc_count += 1;
            let mem = data.mmap_regions.iter().map(|r| r.size as u64).sum::<u64>();
            if mem > data.peak_memory { data.peak_memory = mem; }
            Ok(old.map(crate::mm::Unmapped::new))
        });
        match replaced {
            // Dropped out here, with nothing held: the drop shoots down and
            // waits, and a replacement is what owes that wait — a sibling
            // thread holds a translation for exactly this range and the pages
            // behind the old mapping are on their way back to the PMM. A
            // mapping placed where nothing was owes none, which is why the arm
            // below shoots down nowhere either.
            Ok(old) => {
                drop(old);
                req_addr
            }
            Err(e) => e.to_u64(),
        }
    } else {
        let pt = process::current_address_space();
        let vaddr = process::with_process_data(|data| {
            let placed = match &pages {
                Some(pages) => pt.lock().alloc_and_map(pages.phys(), aligned as u64, mapping_prot, CachePolicy::DeferToMtrr).map(|(v, _)| v),
                None => pt.lock().alloc_region(aligned as u64, crate::vma::RegionKind::Mapped),
            };
            let Some(vaddr) = placed else { return Err(()) };
            data.mmap_regions.push(process::MmapRegion {
                addr: vaddr, size: aligned, _pages: pages,
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
///
/// One path for every mapping. A placed one used to be freed by a second,
/// which cleared its page-table entries and left it registered nowhere — the
/// half of the FIXED defect that outlived the mapping.
fn sys_munmap(addr: u64, _size: u64) -> u64 {
    let pt = process::current_address_space();
    let taken = process::with_process_data(|data| {
        let idx = data.mmap_regions.iter().position(|r| r.addr.raw() == addr)?;
        let region = data.mmap_regions.swap_remove(idx);
        data.free_count += 1;
        pt.lock()
            .free_and_unmap(region.addr)
            .expect("an mmap region is registered in the address space it was placed in");
        Some(crate::mm::Unmapped::new(region))
    });
    let Some(unmapped) = taken else {
        return SyscallError::NotFound.to_u64();
    };
    // Dropped out here, not inside the closure: the drop shoots down and waits,
    // and the process-data lock the closure holds is one a sibling can be spinning
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

/// Wait for a thread of this process to die.
///
/// **It arms on the thread it names**, which is what replaced the parking lot:
/// the target's own `TaskHandle` carries the watch, `thread_exit` posts to it,
/// and the `ThreadSched` held across the park is what keeps that watch alive.
/// A `wake_task(TaskId)` to the process's main thread — a wake by name, into a
/// hashed bucket, re-checked by whoever happened to be woken — is gone with it.
fn sys_thread_join(tid: u64) -> u64 {
    let tid = process::Tid::from_raw(tid as u32);
    let caller = process::current_process();
    // Resolved once. `None` is a thread that never existed or is already
    // collected, and the predicate below answers both.
    let target = process::thread_sched(caller, tid);
    let parkable = crate::scheduler::Parkable::at_entry();
    loop {
        match process::wait_thread_zombie(tid, caller) {
            Ok(Some(_)) => return 0,
            Ok(None) => {}
            Err(()) => return SyscallError::NotFound.to_u64(),
        }
        let Some(sched) = target.as_ref() else {
            // Nothing to arm on and the zombie is not there: the thread is
            // gone in a way `wait_thread_zombie` will keep answering the same
            // way, so waiting cannot change it.
            return SyscallError::NotFound.to_u64();
        };
        if completion::wait_until(
            &parkable,
            completion::Subject::of(sched.handle.watch()),
            completion::Token::new(tid.raw() as u64),
            WaitClass::Other,
            Deadline::never(),
            || matches!(process::wait_thread_zombie(tid, caller), Ok(Some(_)) | Err(())),
        )
        .is_err()
        {
            return cancelled();
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
const MAX_SYSINFO_THREADS: usize = 65_536;

/// The bound `SYS_DEBUG` action 14 puts in its place, so the refusal has a gate.
///
/// Nothing in this harness can make 65,536 threads — each carries a 128 KiB
/// kernel stack, which is 8 GiB of a guest given 128 MiB — so only the number
/// can move, and moving it runs the whole refusal: the count, the comparison
/// and the error return are the shipped ones.
///
/// **Armed at runtime rather than compiled in, and that is the whole of why the
/// action exists.** As a `#[cfg]` it rode into every kernel the suite booted on
/// `test-actuators`' coat-tails, so `SYS_SYSINFO` answered against 16 in every
/// guest and the shipped 65,536 was executed by nothing.
#[cfg(feature = "test-actuators")]
const GATED_SYSINFO_THREADS: usize = 16;

#[cfg(feature = "test-actuators")]
static SYSINFO_BOUND_LOWERED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// What [`sys_sysinfo`] compares against on this boot.
fn sysinfo_thread_bound() -> usize {
    #[cfg(feature = "test-actuators")]
    if SYSINFO_BOUND_LOWERED.load(core::sync::atomic::Ordering::Relaxed) {
        return GATED_SYSINFO_THREADS;
    }
    MAX_SYSINFO_THREADS
}

/// The machine's header, then the process roster for as much of `out` as is
/// left, presenting a `SysCap` that carries [`Rights::ROSTER`] for the second.
///
/// **Two answers under one number, and only the second is authority.** The
/// header is total and used memory, the CPU count, the live-thread count, the
/// uptime and the two accumulators a CPU percentage comes out of — a machine
/// fact like `SYS_CPU_COUNT`, and `free`, the compositor's taskbar and netd's
/// memory budget all read it and nothing else. The entries after it are one per
/// live thread, each carrying a pid, a scheduler state, a resident size, an
/// accumulated CPU time and a 28-byte **name**: a census of everything the
/// machine is running, which was ambient until the owner ruled on 2026-08-20
/// that it rides a right. A process endowed one connector learned the name,
/// size and CPU share of every daemon and every program the user had open.
///
/// **The buffer says which of the two is being asked for**, because that is
/// what it already said: a buffer with no room for an entry cannot be told
/// anything about another process, so nothing is demanded of `syscap` and a
/// header-only caller passes `HANDLE_INVALID`. `max_entries` is the whole of
/// that decision and it is taken here, above the demand, so the two can never
/// disagree about which call this is.
///
/// The refusal is `HandleError`'s ordinary one, as at the five arms beside
/// this: a capability that resolves without the bit is `PermissionDenied` and
/// the caller carries on, and a handle the caller does not hold ends it. It is
/// demanded before the table lock, because `refuse` takes the process down and
/// needs that lock itself.
///
/// [`Rights::ROSTER`]: toyos_abi::handle::Rights::ROSTER
fn sys_sysinfo(syscap: RawHandle, out: &mut UserBytesMut) -> u64 {
    const HEADER_SIZE: usize = toyos_abi::syscall::SYSINFO_HEADER_SIZE;
    const ENTRY_SIZE: usize = toyos_abi::syscall::SYSINFO_ENTRY_SIZE;
    if out.len() < HEADER_SIZE {
        return SyscallError::InvalidArgument.to_u64();
    }
    let max_entries = (out.len() - HEADER_SIZE) / ENTRY_SIZE;
    if max_entries > 0 {
        if let Err(e) = process::with_process_data(|data| {
            data.handles.get::<crate::object::syscap::SysCap>(syscap, Rights::ROSTER)
        }) {
            return e.refuse();
        }
    }

    let (total_mem, used_mem) = crate::mm::pmm::stats();
    let cpu_count = super::smp::cpu_count();
    let uptime = crate::clock::nanos_since_boot();
    let total_cpu_ns = crate::scheduler::total_cpu_ns();
    let total_available_ns = uptime * cpu_count as u64;

    let guard = process::PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();

    let entry_count: u32 = table.iter().flat_map(|(_, proc)| proc.threads().iter().map(move |(tid, thread)| (tid, proc, thread))).count() as u32;
    if entry_count as usize > sysinfo_thread_bound() {
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

    // **The ambient call ends here, having built no roster at all.** Every
    // header-only caller in the tree — `free`, the compositor's taskbar, netd's
    // memory budget — used to pay for a `Vec` of every thread in the machine
    // and a sort of it, to write nothing out of either. It is also what makes
    // the demand above a fact about the whole path rather than about the write
    // loop: with no room for an entry, nothing about another process is
    // collected, let alone copied.
    if max_entries == 0 {
        return HEADER_SIZE as u64;
    }

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

        let state: u8 = if matches!(thread.state(), process::ThreadLocation::Zombie(_)) {
            3
        } else {
            thread.sched().map_or(3, crate::scheduler::task_sched_state)
        };
        let is_thread: u8 = if tid != proc.main_tid() { 1 } else { 0 };

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
        entry[4..8].copy_from_slice(&tid.raw().to_le_bytes());
        entry[8] = state;
        entry[9] = is_thread;
        entry[16..24].copy_from_slice(&memory.to_le_bytes());
        entry[24..32].copy_from_slice(&cpu_ns.to_le_bytes());
        entry[32..60].copy_from_slice(name);
        out.write_at(pos, &entry);

        pos += ENTRY_SIZE;
    }

    pos as u64
}

fn sys_nanosleep(nanos: u64) -> u64 {
    // The caller's own arithmetic, which is exactly what a `Deadline` is: the
    // ABI still carries a relative span, and this is the one place it becomes
    // an instant.
    let deadline = Deadline::at(crate::clock::now() + Duration::from_nanos(nanos));
    // No condition to re-check: the deadline is the wake, and one that has
    // already passed fires at the next scheduler entry.
    // **Armed on nothing but time.** A sleep has no subject — what ends it is
    // the deadline the caller chose — so it arms on its own thread, where
    // nothing posts, and the park's own deadline is the whole of the wait.
    let parkable = crate::scheduler::Parkable::at_entry();
    let Some(handle) = crate::sched::driver::current_handle() else {
        return 0;
    };
    let _ = completion::wait_until(
        &parkable,
        completion::Subject::of(handle.watch()),
        completion::Token::new(0),
        WaitClass::Other,
        deadline,
        || false,
    );
    0
}

/// A second handle to the same object, carrying no more than the first.
///
/// `PermissionDenied` is the answer for a device claim: it is the one object
/// that admits a single handle, and `ops::initial_rights` says so by
/// withholding `Rights::DUP` — exclusivity is a property of the type rather
/// than of a check here. Before that, `dup` handed back a claim's exclusivity
/// while leaving the caller a working handle.
///
/// `want` is the wire form of `Option<Rights>` — [`RIGHTS_UNCHANGED`] for the
/// source's own set — decoded here and nowhere else. A set with a bit no right
/// uses is a caller with a bug, and so is one the source does not hold: rights
/// only shrink, and the refusal names which.
fn sys_handle_dup(h: RawHandle, want: u64) -> u64 {
    let duplicated = process::with_process_data(|data| {
        let held = data.handles.rights_of(h)?;
        let rights = if want == RIGHTS_UNCHANGED {
            held
        } else {
            let bits = u32::try_from(want).map_err(|_| SyscallError::InvalidArgument)?;
            Rights::from_bits(bits).ok_or(SyscallError::InvalidArgument)?
        };
        Ok::<_, crate::object::Refusal>(data.handles.duplicate(h, rights)?)
    });
    match duplicated {
        Ok(new_h) => new_h.0 as u64,
        Err(e) => e.refuse(),
    }
}

/// A second handle to the same object, at a slot the caller picks.
///
/// The second argument is a **slot**, not a handle: a handle carries a
/// generation this call has no business being told, and the one it hands back
/// is the slot's own. Whatever was at that slot is closed first, and the slot's
/// generation moves — so an older handle to it is `Stale` rather than a name
/// for whatever landed there.
///
/// Displacing a handle wakes nobody, for [`sys_close`]'s reason: the reference
/// the entry held is what a wake is owed for, and dropping it is what gives it
/// back.
fn sys_dup2(old: RawHandle, slot: u64) -> u64 {
    let Ok(slot) = u16::try_from(slot) else {
        return SyscallError::ResourceExhausted.to_u64();
    };
    let result = process::with_process_data(|data| {
        let rights = data.handles.rights_of(old)?;
        let entry = data.handles.duplicate_entry(old, rights)?;
        data.handles
            .install_at(slot, entry)
            .map_err(|_| SyscallError::ResourceExhausted.into())
            .map(|(new_h, displaced)| (new_h, Displaced(displaced)))
    });
    match result {
        // **Dropped here, and `install_at` is `#[must_use]` to say so.** A
        // `File` is an `immediate` row, so a `dup2` over the last handle to a
        // modified file runs `vfs::lock()` and a device round trip in this
        // statement. Inside the closure that would happen holding the process's
        // own lock — the one every sibling thread's page-fault handler takes —
        // four ticket spinlocks deep, on a path userland reaches with one
        // syscall.
        Ok((new_h, displaced)) => {
            drop(displaced);
            new_h.0 as u64
        }
        Err(e) => crate::object::Refusal::refuse(e),
    }
}

/// A handle a call displaced, on its way out of the guard that displaced it.
///
/// It exists to make the obligation survive a `?`: a bare `Option<HandleEntry>`
/// carried out of a closure is easy to drop at the wrong statement, and
/// `install_at`'s contract is about *where* the decrement happens rather than
/// whether it happens.
// **Never read, and being dropped is the whole of what it does** — the
// decrement is `HandleEntry`'s own `Drop`, so a reader would be a second way
// to spend the obligation. `expect` rather than `allow`: the day something
// does read it, this line reds and whoever wrote the reader has to say why the
// drop was not enough.
#[expect(dead_code)]
struct Displaced(Option<crate::object::HandleEntry>);

fn sys_rename(old: &str, new: &str) -> u64 {
    let cwd = process::with_process_data(|d| d.cwd.clone());
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
    let cwd = process::with_process_data(|d| d.cwd.clone());
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
    let cwd = process::with_process_data(|d| d.cwd.clone());
    let mut vfs = vfs::lock();
    let resolved = vfs.resolve_absolute(&cwd, path);
    if !vfs.user_may_modify(&resolved) {
        return SyscallError::PermissionDenied.to_u64();
    }
    vfs.remove_dir(&resolved);
    0
}

fn sys_symlink(target: &str, link: &str) -> u64 {
    let cwd = process::with_process_data(|d| d.cwd.clone());
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
    let cwd = process::with_process_data(|d| d.cwd.clone());
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
    let cwd = process::with_process_data(|d| d.cwd.clone());
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
    let mapped = process::with_process_data(|_data| {
        // One `map_into` for both ownership modes, and the module's own program
        // headers decide which of its pages may be written and which may be
        // executed. This used to be two arms that each mapped the whole image
        // writable — which is to say executable *and* writable, for every
        // library in every process.
        let Some(vaddr) = lib.map_into(&pt) else {
            return Err(SyscallError::ResourceExhausted);
        };
        // A `Shared` module's windows are written over a range this address
        // space may already have handed out and reused, and a sibling thread
        // can be running in it: what `map_window` discharged reaches this CPU
        // only, and the rest of the machine is told here.
        if matches!(lib.memory, crate::elf::LibMemory::Shared { .. }) {
            crate::arch::tlb::shootdown();
        }
        let delta = vaddr.raw() as i64 - lib.user_base.raw() as i64;
        if delta != 0 {
            crate::elf::rebase_relative_relocs(&lib, delta);
        }
        lib.user_base = vaddr;
        Ok(())
    });
    if let Err(e) = mapped {
        log!("dlopen: {}: out of virtual address space", resolved);
        return e.to_u64();
    }

    let lib_has_tls = lib.tls_memsz > 0;

    let data_arc = process::process_data();
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

    let owner_arc = process::process_data();
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
    let existing = process::with_process_data(|data| {
        data.elf.dynamic_tls_blocks.get(&(tid, module_id)).map(|b| b.vaddr())
    });

    let tls_vaddr = match existing {
        Some(vaddr) => vaddr,
        None => {
            let page_alloc = process::PageAlloc::new(tls_memsz.max(1), crate::mm::pmm::Category::Tls)
                .ok_or(SyscallError::ResourceExhausted)?;
            // SAFETY: `page_alloc` is a fresh `PageAlloc` of at least
            // `tls_memsz.max(1)` bytes that nothing else has a pointer to yet —
            // it is mapped into the process below, not above. `template` is the
            // module's TLS image out of the loaded ELF, live for as long as the
            // module is, and `template.size()` is its own length, which
            // `elf::tls_modules` derives from the same program header as
            // `m.memsz`. The two regions are a fresh physical page and kernel
            // image data, so they cannot overlap.
            //
            // Irreducible only for want of a bounded window over `PageAlloc`:
            // the length checked here is the *source's*, and nothing types the
            // destination's — the root-file sweep filed exactly that
            // (`issues/kernel/pagealloc-has-no-checked-window.md`), and this is a
            // third site of the same shape.
            unsafe {
                if let Some(template) = &tls_template {
                    core::ptr::copy_nonoverlapping(template.base(), page_alloc.ptr(), template.size());
                }
            }

            let block_phys = page_alloc.phys();
            let pt = process::current_address_space();
            process::with_process_data(|data| {
                let (vaddr, _) = process::vma_map(&pt, block_phys, page_alloc.size() as u64, Prot::ReadWrite)
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
        // SAFETY: `module_id` crossed the trust boundary and is bounded at the
        // top of `tls_alloc_block` — non-zero and at most
        // `loader::DTV_INITIAL_CAPACITY`, checked against the kernel's own
        // constant and never against the `len` word in the DTV, which the
        // process can rewrite. `loader` lays the DTV out at offset 0 of the
        // thread's kernel-side TLS allocation with `DTV_INITIAL_CAPACITY` entries
        // after the two header words, so `2 + (module_id - 1)` is in bounds. The
        // allocation is this thread's own and this thread is the one running.
        //
        // **The bound and the write are fifty lines and one function apart**,
        // which is the same missing type as the `copy_nonoverlapping` above:
        // nothing here would notice the check moving.
        unsafe { *dtv_kern.add(2 + (module_id - 1) as usize) = tls_vaddr.raw(); }
    });
    Ok(tls_vaddr.raw())
}

fn sys_dlsym(handle: u64, name: &str) -> u64 {
    let data_arc = process::process_data();
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

/// Make an inbox and tell the caller where it is.
///
/// The inbox owns its page and this maps it. An inbox is not something two
/// processes share, so nothing else may name that page.
fn sys_inbox_setup(ctx: &SyscallContext, depth: u32, out: u64) -> u64 {
    let out = match UserAddr::checked(out) {
        Some(addr) => addr,
        None => return SyscallError::InvalidArgument.to_u64(),
    };
    let (inbox, vaddr) = match crate::inbox::create(depth) {
        Ok(v) => v,
        Err(e) => return e.to_u64(),
    };
    // A refused install drops the reference, which tears the inbox down again.
    let object = KObjectRef::Inbox(crate::object::inbox::InboxObject::new(inbox));
    let installed = process::with_process_data(|data| ops::install(&mut data.handles, object));
    let handle = match installed {
        Ok(h) => h,
        Err(e) => return e.to_u64(),
    };
    let answer = toyos_abi::syscall::InboxSetup { handle, _pad: 0, vaddr };
    match ctx.copy_out(out, &answer) {
        Ok(()) => 0,
        Err(e) => {
            process::with_process_data(|data| {
                ops::close(&mut data.handles, handle, &mut data.pipe_maps)
                    .expect("the inbox this call installed a moment ago");
            });
            e.to_u64()
        }
    }
}

fn sys_inbox_submit(
    inbox_h: RawHandle,
    to_submit: u32,
    min_complete: u32,
    timeout_nanos: u64,
) -> u64 {
    // The table's own words, not one invented here: a handle that is gone is
    // `NotFound` and one of the wrong type is `PermissionDenied`, the same as
    // every other call. Collapsing both into `InvalidArgument` made "this
    // inbox was closed" indistinguishable from "this argument is nonsense".
    let inbox_id = process::with_process_data(|data| {
        data.handles
            .get::<crate::object::inbox::InboxObject>(
                inbox_h,
                Rights::READ.union(Rights::WRITE),
            )
            .map(|r| r.id())
    });
    let inbox_id = match inbox_id {
        Ok(id) => id,
        Err(e) => return e.refuse(),
    };
    match crate::inbox::submit(inbox_id, to_submit, min_complete, timeout_nanos) {
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

/// Accounting for the process a handle names, alive or exited.
///
/// **Repeatable, and not a claim on anything.** It used to hand the caller a
/// snapshot its exited child had stashed on it, deleting it on the way out —
/// so the numbers could be read once, by one process, and only after the child
/// was dead. With a handle there is nothing to stash: a live process is sampled
/// from its own data and an exited one from the object, and neither reading
/// spends anything.
fn sys_process_stats(
    ctx: &crate::user_ptr::SyscallContext,
    h: RawHandle,
    out: UserAddr,
) -> u64 {
    let object = match process::with_process_data(|data| {
        data.handles.get::<crate::object::process::ProcessObject>(h, Rights::READ)
    }) {
        Ok(object) => object,
        Err(e) => return e.refuse(),
    };
    let Some(stats) = process::stats_of(&object) else {
        return SyscallError::NotFound.to_u64();
    };
    match ctx.copy_out(out, &stats) {
        Ok(()) => 0,
        Err(e) => e.to_u64(),
    }
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
        // SAFETY: `transmute_copy` needs the destination to be no larger than
        // the source and the bytes to be a valid value of it. The destination is
        // `[u8; size_of::<ModuleInfo>()]`, so the sizes are equal by
        // construction, and every byte pattern is a valid `[u8; N]`.
        // `ModuleInfo` is `#[repr(C)]` over five integers with no padding, so no
        // uninitialised byte is read either.
        //
        // Reducible, and not reduced here: six other boundary-crossing types
        // carry a safe `as_bytes` in `toyos-abi` and this is one that does not,
        // which makes the fix an edit under `toyos-abi/src` — its own pull
        // request and a sysroot claim. Filed as
        // `issues/build/moduleinfo-has-no-as-bytes.md`, beside `LogRecord`'s.
        unsafe { core::mem::transmute_copy(info) }
    }

    process::with_process_data(|data| {
        let module_count = 1 + data.elf.loaded_libs.len();

        let exe_path_bytes = data.exe_path.as_bytes();
        let total_path_bytes: usize = exe_path_bytes.len()
            + data.elf.lib_paths.iter().map(|p| p.len()).sum::<usize>();

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
