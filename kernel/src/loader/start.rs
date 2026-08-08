//! Getting a freshly built process onto a CPU.
//!
//! The two trampolines are the only per-architecture code in the loader.
//! Everything else — the address space, the relocations, the TLS block — is
//! arch-neutral, so a second architecture adds this file and nothing more.

use crate::arch::entry::{initial_user_state, ring3_trampoline_asm};
use crate::fd::FdTable;
use crate::process::{fd_owner_data, OwnedAlloc, KERNEL_STACK_SIZE};
use crate::scheduler;
use toyos_abi::syscall::SyscallError;

/// One `[child_fd, parent_fd]` pair of `SpawnArgs::fd_map_ptr`, in bytes.
pub const FD_PAIR_LEN: usize = 8;

/// Allocate a kernel stack and lay out the frame `context_switch` will restore.
pub(crate) fn alloc_kernel_stack(
    trampoline: unsafe extern "C" fn(),
    user_entry: u64,
    user_sp: u64,
    arg: u64,
) -> Option<(OwnedAlloc, u64)> {
    let alloc = OwnedAlloc::new(KERNEL_STACK_SIZE, 4096)?;
    scheduler::write_stack_canary(&alloc);
    let top = alloc.ptr() as u64 + KERNEL_STACK_SIZE as u64;
    // Must match context_switch: pushfq, push rbp..r15 (8 values), then the
    // return address.
    let frame = (top - 8 * 8) as *mut u64;
    unsafe {
        *frame.add(0) = 0; // r15
        *frame.add(1) = arg; // r14
        *frame.add(2) = user_sp; // r13
        *frame.add(3) = user_entry; // r12
        *frame.add(4) = 0; // rbx
        *frame.add(5) = 0; // rbp
        *frame.add(6) = 0x002; // RFLAGS (IF=0, AC=0)
        *frame.add(7) = trampoline as u64; // return address
    }
    Some((alloc, frame as u64))
}

/// Entry point for new processes, reached through `context_switch`'s `ret`.
/// r12 = entry point, r13 = user stack pointer.
///
/// The state is loaded after the unlock and not before: what it displaces is
/// whatever the CPU's previous tenant left in the registers, and the unlock is
/// still that tenant's kernel code.
#[unsafe(naked)]
pub(crate) extern "C" fn process_start() {
    ring3_trampoline_asm!(
        "push r12",
        "push r13",
        "call {unlock}",
        "pop r13",
        "pop r12",
        initial_user_state!(),
        "push {user_ss}",
        "push r13",         // RSP: user stack
        "push 0x202",       // RFLAGS: IF=1
        "push {user_cs}",
        "push r12",         // RIP: entry point
        "iretq",
        unlock = sym crate::sched::driver::trampoline_entry,
        user_ss = const crate::arch::gdt::USER_DS,
        user_cs = const crate::arch::gdt::USER_CS,
    );
}

/// Entry point for new threads. r14 carries the argument, which lands in rdi.
#[unsafe(naked)]
pub(crate) extern "C" fn thread_start() {
    ring3_trampoline_asm!(
        "push r12",
        "push r13",
        "push r14",
        "call {unlock}",
        "pop r14",
        "pop r13",
        "pop r12",
        initial_user_state!(),
        "mov rdi, r14",
        "sub r13, 8",       // ABI: RSP must be 16n+8 at function entry
        "push {user_ss}",
        "push r13",
        "push 0x202",
        "push {user_cs}",
        "push r12",
        "iretq",
        unlock = sym crate::sched::driver::trampoline_entry,
        user_ss = const crate::arch::gdt::USER_DS,
        user_cs = const crate::arch::gdt::USER_CS,
    );
}

/// The last path component, truncated to what a process entry can hold.
pub(crate) fn make_name(path: &str) -> [u8; crate::process::THREAD_NAME_LEN] {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let mut name = [0u8; crate::process::THREAD_NAME_LEN];
    let len = filename.len().min(crate::process::THREAD_NAME_LEN - 1);
    name[..len].copy_from_slice(&filename.as_bytes()[..len]);
    name
}

/// Build a child's `FdTable` by duplicating the parent descriptors it names.
///
/// A pair naming a parent fd that does not exist contributes nothing: the
/// child simply does not get that descriptor, which is what a caller asking
/// for a closed fd deserves and is not a reason to refuse the spawn.
pub fn build_child_fds(pairs: &crate::user_ptr::UserBytes) -> Result<FdTable, SyscallError> {
    let data_arc = fd_owner_data();
    let data = data_arc.lock();
    let mut fds = FdTable::new();
    for i in 0..pairs.len() / FD_PAIR_LEN {
        let mut pair = [0u8; FD_PAIR_LEN];
        pairs.read_at(i * FD_PAIR_LEN, &mut pair);
        let child_fd = u32::from_ne_bytes([pair[0], pair[1], pair[2], pair[3]]);
        let parent_fd = u32::from_ne_bytes([pair[4], pair[5], pair[6], pair[7]]);
        if let Some(desc) = data.fds.get(parent_fd) {
            // A device claim admits one descriptor, so it cannot be given to a
            // child: that would be a transfer, and there is no transfer
            // operation — `capability-handles-spec.md` §6.5 scopes spawn-time
            // device grants out of v1. Refused by name rather than skipped,
            // which would start the child without an fd it asked for.
            let child_desc = desc.duplicate().ok_or(SyscallError::PermissionDenied)?;
            fds.insert_at(child_fd, child_desc)?;
        }
    }
    Ok(fds)
}
