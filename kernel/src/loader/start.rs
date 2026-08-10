//! Getting a freshly built process onto a CPU.
//!
//! The two trampolines are the only per-architecture code in the loader.
//! Everything else — the address space, the relocations, the TLS block — is
//! arch-neutral, so a second architecture adds this file and nothing more.

use alloc::vec::Vec;

use crate::arch::entry::{initial_user_state, ring3_trampoline_asm};
use crate::object::{HandleTable, Refusal};
use crate::process::{
    fd_owner_data, Endowments, OwnedAlloc, ENDOW_ENTRY_LEN, KERNEL_STACK_SIZE,
};
use crate::scheduler;
use crate::user_ptr::UserBytes;
use toyos_abi::handle::{RawHandle, Rights};
use toyos_abi::syscall::{EndowEntry, SyscallError, MAX_ENDOWMENTS, MAX_LABELS_LEN};

/// One `[child_slot, parent_handle]` pair of `SpawnArgs::slot_map_ptr`, in
/// bytes.
pub const SLOT_PAIR_LEN: usize = 8;

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

/// Build a child's table out of the two vectors `SpawnArgs` carries.
///
/// **Two verbs.** `slot_map` *duplicates* — the parent keeps its stdout — and
/// `endow` *moves*, which is what makes handing over a device claim expressible
/// at all: a claim carries no `DUP`, so a move is its only form and the parent
/// provably no longer holds it afterwards.
///
/// **All or nothing.** Everything is verified under one hold of the parent's
/// lock — the handles resolve, they carry `TRANSFER`, the labels are in range
/// and the child's table has room — and only then is anything removed. A
/// refusal leaves the parent's table exactly as it was.
pub fn build_child_handles(
    slot_map: &UserBytes,
    endow: &UserBytes,
    labels: &[u8],
) -> Result<(HandleTable, Endowments), Refusal> {
    if endow.len() / ENDOW_ENTRY_LEN > MAX_ENDOWMENTS {
        return Err(SyscallError::InvalidArgument.into());
    }
    if labels.len() > MAX_LABELS_LEN {
        return Err(SyscallError::InvalidArgument.into());
    }
    let data_arc = fd_owner_data();
    let mut data = data_arc.lock();
    let mut handles = HandleTable::new();
    for i in 0..slot_map.len() / SLOT_PAIR_LEN {
        let mut pair = [0u8; SLOT_PAIR_LEN];
        slot_map.read_at(i * SLOT_PAIR_LEN, &mut pair);
        let child_slot = u32::from_ne_bytes([pair[0], pair[1], pair[2], pair[3]]);
        let parent = RawHandle(u32::from_ne_bytes([pair[4], pair[5], pair[6], pair[7]]));
        // A pair naming a parent handle that does not resolve contributes
        // nothing: the child simply does not get it, which is what a caller
        // asking for a closed handle deserves and is not a reason to refuse the
        // spawn.
        let Ok(rights) = data.handles.rights_of(parent) else { continue };
        // A device claim carries no `DUP`, so it cannot come this way. The
        // refusal is by name rather than a skip, which would start the child
        // without a handle it asked for — the endowment vector below is the
        // move that *can* carry one.
        let entry = data.handles.duplicate_entry(parent, rights)?;
        let slot = u16::try_from(child_slot)
            .map_err(|_| SyscallError::ResourceExhausted)?;
        let (_, displaced) = handles
            .install_at(slot, entry)
            .map_err(|_| SyscallError::ResourceExhausted)?;
        drop(displaced);
    }

    let count = endow.len() / ENDOW_ENTRY_LEN;
    let mut moving: Vec<(EndowEntry, RawHandle)> = Vec::with_capacity(count);
    for i in 0..count {
        let mut raw = [0u8; ENDOW_ENTRY_LEN];
        endow.read_at(i * ENDOW_ENTRY_LEN, &mut raw);
        let label_off = u32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let label_len = u32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let handle = RawHandle(u32::from_ne_bytes([raw[8], raw[9], raw[10], raw[11]]));
        let end = (label_off as usize)
            .checked_add(label_len as usize)
            .ok_or(SyscallError::InvalidArgument)?;
        if end > labels.len() {
            return Err(SyscallError::InvalidArgument.into());
        }
        // Verified against the *parent's* rights here and removed below, so a
        // handle that is missing `TRANSFER` refuses the spawn rather than
        // leaving the child a hole where its parent said a capability would be.
        let rights = data.handles.rights_of(handle)?;
        if !rights.contains(Rights::TRANSFER) {
            return Err(SyscallError::PermissionDenied.into());
        }
        moving.push((EndowEntry { label_off, label_len, handle, _pad: 0 }, handle));
    }
    // The child's table must be able to take all of them before the parent's
    // gives any up: an install that failed halfway would have moved a handle
    // out of a table that is about to be told the spawn did not happen.
    if !handles.has_room(moving.len()) {
        return Err(SyscallError::ResourceExhausted.into());
    }

    let mut entries = Vec::with_capacity(moving.len());
    for (mut entry, parent_handle) in moving {
        let moved = data
            .handles
            .remove(parent_handle)
            .expect("an endowed handle verified under this lock stopped resolving");
        entry.handle = handles
            .install(moved)
            .expect("a child table with verified room refused an endowment");
        entries.push(entry);
    }
    Ok((handles, Endowments::new(entries, labels.to_vec())))
}
