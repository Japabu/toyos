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
use toyos_abi::syscall::{EndowEntry, SyscallError, MAX_ENDOWMENTS, MAX_LABELS_LEN, MAX_SLOT_MAP};

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

/// Entry point for a **kernel** thread: the one trampoline that never reaches
/// Ring 3.
///
/// It is strictly simpler than the two above and the absences are the point —
/// no `initial_user_state!`, no `iretq`, no `USER_CS`/`USER_DS` — because
/// nothing about this context is a user context. r12 carries the body, r14 its
/// argument, and `alloc_kernel_stack` put both there.
///
/// **The `sti` is load-bearing and there is nowhere else to put it.**
/// `alloc_kernel_stack` writes RFLAGS with `IF` clear into the frame
/// `context_switch` pops, and the two Ring 3 trampolines set `IF` in the
/// `iretq` frame instead. A kernel thread that skipped this would run with
/// interrupts masked forever: no timer, no preemption, no wake. It comes
/// *after* `trampoline_entry`, whose `kernel_exit_to_user_check` requires
/// `IF` clear on entry and returns with it clear.
///
/// A body that returns is a kernel bug, and it dies as one rather than falling
/// off the end of a stack that has no return address on it.
#[unsafe(naked)]
pub(crate) extern "C" fn kernel_start() {
    core::arch::naked_asm!(
        "call {unlock}",
        "sti",
        "mov rdi, r14",
        "call r12",
        "call {returned}",
        unlock = sym crate::sched::driver::trampoline_entry,
        returned = sym kernel_thread_returned,
    );
}

/// What [`kernel_start`] calls when a kernel thread's body returns.
///
/// Loud rather than a `hlt`: a body that returns has left the machine without
/// whatever it was there to do, and a silent halt of one CPU is exactly the
/// kind of quiet this branch exists to remove.
extern "C" fn kernel_thread_returned() -> ! {
    panic!("a kernel thread's body returned; nothing runs on this stack now");
}

/// The last path component, truncated to what a process entry can hold.
pub(crate) fn make_name(path: &str) -> [u8; crate::process::THREAD_NAME_LEN] {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let mut name = [0u8; crate::process::THREAD_NAME_LEN];
    let len = filename.len().min(crate::process::THREAD_NAME_LEN - 1);
    name[..len].copy_from_slice(&filename.as_bytes()[..len]);
    name
}

/// A child's table, and the endowments that have not left the parent yet.
///
/// **The move is the last thing a spawn does.** `SYS_SPAWN` can still fail long
/// after its arguments are read — the program may not exist, its ELF may not
/// parse, its stack may not fit — and a parent told "that did not happen" while
/// its endowed handles have already gone holds numbers that name nothing. Under
/// the bad-handle policy its own `close` of one is then fatal, which is how a
/// `spawn: /bin/pull-probe-0: not found` killed `test-runner`.
pub enum PendingHandles {
    /// Built by the kernel and owing nobody anything — the boot's `/bin/init`.
    Ready(HandleTable, Endowments),
    /// A caller's request: the table holds the `slot_map` duplicates, and the
    /// blob is the `endow` vector that still has to leave the caller's table.
    Moving { table: HandleTable, endow: Vec<u8>, labels: Vec<u8> },
}

impl PendingHandles {
    /// Take the endowed handles out of the parent's table.
    ///
    /// **All or nothing, under one hold of the parent's lock**: the handles
    /// resolve, they carry `TRANSFER`, the labels are in range and the child's
    /// table has room — and only then is anything removed. A refusal leaves the
    /// parent's table exactly as it was.
    pub fn commit(self) -> Result<(HandleTable, Endowments), Refusal> {
        let (mut table, endow, labels) = match self {
            Self::Ready(table, endowments) => return Ok((table, endowments)),
            Self::Moving { table, endow, labels } => (table, endow, labels),
        };
        let data_arc = fd_owner_data();
        let mut data = data_arc.lock();

        let count = endow.len() / ENDOW_ENTRY_LEN;
        let mut moving: Vec<(EndowEntry, RawHandle)> = Vec::with_capacity(count);
        for raw in endow.chunks_exact(ENDOW_ENTRY_LEN) {
            let label_off = u32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let label_len = u32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]);
            let handle = RawHandle(u32::from_ne_bytes([raw[8], raw[9], raw[10], raw[11]]));
            let end = (label_off as usize)
                .checked_add(label_len as usize)
                .ok_or(SyscallError::InvalidArgument)?;
            if end > labels.len() {
                return Err(SyscallError::InvalidArgument.into());
            }
            // Verified against the *parent's* rights here and removed below, so
            // a handle that is missing `TRANSFER` refuses the spawn rather than
            // leaving the child a hole where its parent said a capability would
            // be.
            let rights = data.handles.rights_of(handle)?;
            if !rights.contains(Rights::TRANSFER) {
                return Err(SyscallError::PermissionDenied.into());
            }
            // **Named twice is refused here, because the preflight cannot see
            // it later.** Every check above runs against a table nothing has
            // been taken out of yet, so a repeat passes both times; the first
            // removal then retires the slot and the second answers `Stale`,
            // which the `expect` below turns into a kernel panic a caller
            // reaches with one argument. `sys_handle_send` refuses the same
            // shape for the same reason.
            if moving.iter().any(|(_, seen)| *seen == handle) {
                return Err(SyscallError::InvalidArgument.into());
            }
            moving.push((EndowEntry { label_off, label_len, handle, _pad: 0 }, handle));
        }
        // The child's table must be able to take all of them before the
        // parent's gives any up: an install that failed halfway would have
        // moved a handle out of a table that is about to be told the spawn did
        // not happen.
        if !table.has_room(moving.len()) {
            return Err(SyscallError::ResourceExhausted.into());
        }

        let mut entries = Vec::with_capacity(moving.len());
        for (mut entry, parent_handle) in moving {
            let moved = data
                .handles
                .remove(parent_handle)
                .expect("an endowed handle verified under this lock stopped resolving");
            entry.handle = table
                .install(moved)
                .expect("a child table with verified room refused an endowment");
            entries.push(entry);
        }
        Ok((table, Endowments::new(entries, labels)))
    }
}

/// Read the two vectors `SpawnArgs` carries.
///
/// **Two verbs.** `slot_map` *duplicates* — the parent keeps its stdout — and
/// `endow` *moves*. The duplicates are taken here, because a copy costs the
/// parent nothing if the spawn then fails; the moves are checked for shape and
/// carried to [`PendingHandles::commit`], which is the point of no return.
pub fn build_child_handles(
    slot_map: &UserBytes,
    endow: &UserBytes,
    labels: &[u8],
) -> Result<PendingHandles, Refusal> {
    if endow.len() / ENDOW_ENTRY_LEN > MAX_ENDOWMENTS {
        return Err(SyscallError::InvalidArgument.into());
    }
    // **Before the loop, because the loop is the cost.** `install_at`'s cap
    // refuses a *slot* past the table and says nothing about how many pairs
    // name the same one: a caller repeating `(0, h)` keeps the child's table at
    // one entry while every iteration duplicates a handle under the parent's
    // lock and hands back a live entry this call has to hold until the lock is
    // released.
    if slot_map.len() / SLOT_PAIR_LEN > MAX_SLOT_MAP {
        return Err(SyscallError::InvalidArgument.into());
    }
    if labels.len() > MAX_LABELS_LEN {
        return Err(SyscallError::InvalidArgument.into());
    }
    let data_arc = fd_owner_data();
    let data = data_arc.lock();
    let mut handles = HandleTable::new();
    // Carried out of the guard rather than dropped inside it. Every entry a
    // stdio pair displaces here is a duplicate this same loop just made, so the
    // decrement reaches nothing — but `install_at`'s contract is that the
    // caller decides *where* it happens, and a site that is right by accident
    // is one an added slot kind makes wrong.
    let mut displaced = Vec::new();
    // Parent console objects this call has already minted a replacement for,
    // by the parent object's address. See the console arm below.
    let mut minted: Vec<(usize, crate::object::KObjectRef)> = Vec::new();
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
        // **A console is minted for the child, never duplicated into it.** The
        // object *is* the line buffer (`object::device::ConsoleObject`), so a
        // child sharing its parent's would accumulate into one buffer with it
        // and the two half-lines would splice inside the mechanism that exists
        // to stop splicing. Authority
        // does not move: a child gets a console exactly when this pair says it
        // does, which is the rule that was already here, and the duplicate above
        // is still what refuses a handle without `DUP`. Aliasing does not move
        // either — two slots naming one parent object get one child object, so a
        // program whose stdout and stderr are the same console still writes one
        // stream.
        let entry = match entry.object() {
            crate::object::KObjectRef::Console(parent_console) => {
                let key = alloc::sync::Arc::as_ptr(parent_console) as usize;
                let object = match minted.iter().find(|(seen, _)| *seen == key) {
                    Some((_, object)) => object.clone(),
                    None => {
                        let object = crate::object::KObjectRef::Console(
                            crate::object::device::ConsoleObject::new(),
                        );
                        minted.push((key, object.clone()));
                        object
                    }
                };
                // The duplicate goes back with everything else this loop
                // displaces, outside the parent's lock.
                displaced.push(Some(entry));
                crate::object::HandleEntry::new(object, rights)
            }
            _ => entry,
        };
        let slot = u16::try_from(child_slot)
            .map_err(|_| SyscallError::ResourceExhausted)?;
        let (_, replaced) = handles
            .install_at(slot, entry)
            .map_err(|_| SyscallError::ResourceExhausted)?;
        displaced.push(replaced);
    }

    drop(data);
    drop(displaced);

    let mut raw = alloc::vec![0u8; endow.len() as usize];
    endow.read_at(0, &mut raw);
    Ok(PendingHandles::Moving { table: handles, endow: raw, labels: labels.to_vec() })
}
