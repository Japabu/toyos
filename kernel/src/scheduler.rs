use core::arch::{asm, naked_asm};

use crate::arch::{cpu, paging, percpu};
use crate::process::{IdleProof, Pid, ProcessState, ProcessTable, PROCESS_TABLE};
use crate::keyboard;
const IA32_FS_BASE: u32 = 0xC0000100;

/// Block the current process and switch to the next ready one.
pub fn block(reason: ProcessState) {
    schedule(reason);
}

/// Cooperative yield: mark current as Ready, switch to next.
pub fn yield_now() {
    schedule(ProcessState::Ready);
}

/// Timer preemption: called from the timer interrupt handler.
/// No-op if no process is running on this CPU (e.g. AP during early boot or idle).
pub fn preempt() {
    if percpu::current_pid().is_none() {
        return;
    }
    yield_now();
}

/// Mark current process with `cur_state`, find next ready process, context switch.
///
/// Single-pass: if no Ready process is found, saves the current process's RSP
/// and switches to the per-CPU idle stack. The process's kernel stack is then
/// free for any CPU to pick up later.
///
/// The process table lock is held through context_switch to prevent another
/// CPU from stealing a process before its RSP is saved. The resuming side
/// releases the lock via `force_unlock`.
fn schedule(cur_state: ProcessState) {
    debug_assert!(!matches!(cur_state, ProcessState::Zombie(_)),
        "schedule() called with Zombie state for pid");

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let cur_pid = percpu::current_pid().expect("schedule() called during idle");

    if let Some(entry) = table.get_mut(cur_pid) {
        entry.set_state(cur_state);
    } else {
        crate::log!("schedule: warning: cur_pid {cur_pid} not in table, going to idle");
    }
    schedule_inner(guard);
}

/// Schedule with a pre-held lock where the caller has already set the process state.
/// Used by futex_wait to make the value-check + block atomic.
pub fn schedule_already_blocked(guard: crate::sync::LockGuard<'_, Option<ProcessTable>>) {
    schedule_inner(guard);
}

fn schedule_inner(mut guard: crate::sync::LockGuard<'_, Option<ProcessTable>>) {
    let table = guard.as_mut().unwrap();
    let cur_pid = percpu::current_pid().expect("schedule_inner() called during idle");
    let cur_alive = table.get(cur_pid).is_some();

    idle_poll(table);

    // Round-robin: find smallest Ready PID > current, or wrap to smallest Ready PID
    let mut best_after: Option<Pid> = None;
    let mut best_any: Option<Pid> = None;
    for (pid, entry) in table.iter() {
        if *entry.state() == ProcessState::Ready {
            if pid > cur_pid && best_after.map_or(true, |b| pid < b) {
                best_after = Some(pid);
            }
            if best_any.map_or(true, |b| pid < b) {
                best_any = Some(pid);
            }
        }
    }

    if let Some(new_pid) = best_after.or(best_any) {
        if cur_alive && new_pid == cur_pid {
            table.get_mut(cur_pid).unwrap().set_state(ProcessState::Running);
            return;
        }

        let new_entry = table.get(new_pid).unwrap_or_else(|| {
            let keys: alloc::vec::Vec<Pid> = table.iter().map(|(pid, _)| pid).collect();
            panic!("schedule: pid {new_pid} vanished after scan (cur={cur_pid}, keys={keys:?})");
        });
        assert!(*new_entry.state() == ProcessState::Ready,
            "scheduling non-Ready pid={new_pid}: {}", new_entry.state().name());
        assert!(new_entry.cr3().is_some(),
            "scheduling pid={new_pid} with no page tables");
        let new_rsp = new_entry.kernel_rsp();
        let new_cr3 = new_entry.cr3().unwrap().phys();
        let new_fs_base = new_entry.fs_base();
        let new_ks_top = new_entry.kernel_stack_top();

        table.get_mut(new_pid).unwrap().set_state(ProcessState::Running);
        percpu::set_current_pid(Some(new_pid));

        let old_rsp_ptr = if let Some(cur_entry) = table.get_mut(cur_pid) {
            cur_entry.set_fs_base(cpu::rdmsr(IA32_FS_BASE));
            cur_entry.kernel_rsp_mut() as *mut u64
        } else {
            percpu::idle_rsp_ptr()
        };
        unsafe { percpu::set_kernel_stack(new_ks_top); }

        unsafe { cpu::write_cr3(new_cr3); }
        cpu::wrmsr(IA32_FS_BASE, new_fs_base);

        // Hold lock through context_switch — the resuming side releases it.
        core::mem::forget(guard);
        unsafe { context_switch(old_rsp_ptr, new_rsp); }
        unsafe { PROCESS_TABLE.force_unlock(); }
        unsafe { cpu::stac(); }
        return;
    }

    // No Ready process — switch to per-CPU idle stack.
    let old_rsp_ptr = if let Some(cur_entry) = table.get_mut(cur_pid) {
        cur_entry.set_fs_base(cpu::rdmsr(IA32_FS_BASE));
        cur_entry.kernel_rsp_mut() as *mut u64
    } else {
        percpu::idle_rsp_ptr()
    };
    percpu::set_current_pid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }

    unsafe { cpu::write_cr3(paging::kernel_cr3()); }

    core::mem::forget(guard);
    unsafe { context_switch(old_rsp_ptr, percpu::idle_rsp()); }
    unsafe { PROCESS_TABLE.force_unlock(); }
    unsafe { cpu::stac(); }
}

/// Schedule without saving current context (used by ap_idle and BSP boot).
pub fn schedule_no_return() -> ! {
    percpu::set_current_pid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { cpu::write_cr3(paging::kernel_cr3()); }
    let sp = percpu::idle_stack_top();
    unsafe {
        asm!(
            "mov rsp, {sp}",
            "jmp {func}",
            sp = in(reg) sp,
            func = in(reg) cpu_idle_loop as *const () as usize,
            options(noreturn),
        );
    }
}

/// Like `schedule_no_return`, but consumes a held PROCESS_TABLE lock guard.
/// Used by `exit()` to keep the lock held through the stack switch, preventing
/// another CPU from collecting the zombie (and freeing the kernel stack) before
/// we've switched off it.
pub fn schedule_no_return_locked(guard: crate::sync::LockGuard<'_, Option<ProcessTable>>) -> ! {
    core::mem::forget(guard);
    percpu::set_current_pid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { cpu::write_cr3(paging::kernel_cr3()); }
    let sp = percpu::idle_stack_top();
    unsafe {
        asm!(
            "mov rsp, {sp}",
            "jmp {func}",
            sp = in(reg) sp,
            func = in(reg) idle_unlock_and_loop as *const () as usize,
            options(noreturn),
        );
    }
}

/// Idle loop running on the per-CPU idle stack. Polls for I/O and dispatches
/// Ready processes via context_switch.
fn cpu_idle_loop() -> ! {
    // SAFETY: cpu_idle_loop runs on the per-CPU idle stack (set up by schedule_no_return).
    let idle_proof = unsafe { IdleProof::new_unchecked() };
    loop {
        {
            let mut guard = PROCESS_TABLE.lock();
            let table = guard.as_mut().unwrap();
            idle_poll(table);
            table.collect_orphan_zombies(idle_proof);

            let ready = table.iter()
                .find(|(_, e)| *e.state() == ProcessState::Ready)
                .map(|(pid, _)| pid);

            if let Some(new_pid) = ready {
                let new_entry = table.get(new_pid).unwrap_or_else(|| {
                    let keys: alloc::vec::Vec<Pid> = table.iter().map(|(pid, _)| pid).collect();
                    panic!("idle: pid {new_pid} vanished after scan (keys={keys:?})");
                });
                assert!(new_entry.cr3().is_some(),
                    "idle: scheduling pid={new_pid} with no page tables");
                let new_rsp = new_entry.kernel_rsp();
                let new_cr3 = new_entry.cr3().unwrap().phys();
                let new_fs_base = new_entry.fs_base();
                let new_ks_top = new_entry.kernel_stack_top();

                table.get_mut(new_pid).unwrap().set_state(ProcessState::Running);
                percpu::set_current_pid(Some(new_pid));
                unsafe { percpu::set_kernel_stack(new_ks_top); }

                unsafe { cpu::write_cr3(new_cr3); }
                cpu::wrmsr(IA32_FS_BASE, new_fs_base);

                core::mem::forget(guard);
                unsafe { context_switch(percpu::idle_rsp_ptr(), new_rsp); }
                unsafe { PROCESS_TABLE.force_unlock(); }
                unsafe { cpu::stac(); }
                continue;
            }
        }
        // Halt until next interrupt (timer at 100Hz or xHCI MSI-X).
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)); }
    }
}

/// Entry point for the idle stack when first reached via context_switch from schedule().
pub fn idle_unlock_and_loop() -> ! {
    unsafe { PROCESS_TABLE.force_unlock(); }
    cpu_idle_loop()
}

/// Poll for I/O and wake blocked processes.
/// Only accesses SchedEntry data — never locks ProcessData.
/// BlockedPoll and BlockedRecvMsg use spurious wakeups: explicit wake calls
/// from pipe/message/keyboard/network subsystems handle the fast path,
/// and idle_poll only checks timeouts and global readiness flags.
fn idle_poll(table: &mut ProcessTable) {
    // Process USB events only when MSI-X interrupt fired (BSP only)
    if percpu::cpu_id() == 0 {
        crate::drivers::xhci::poll_if_pending();
    }

    let kb_ready = keyboard::has_data();
    let net_ready = crate::net::has_packet();

    let mut zombie_pids = alloc::vec::Vec::new();
    for (_, entry) in table.iter() {
        if matches!(entry.state(), ProcessState::Zombie(_)) {
            zombie_pids.push(entry.pid());
        }
    }

    for (_, entry) in table.iter_mut() {
        match *entry.state() {
            ProcessState::BlockedKeyboard if kb_ready => {
                entry.set_state(ProcessState::Ready);
            }
            ProcessState::BlockedPipeRead(id) if crate::pipe::has_data(id) => {
                entry.set_state(ProcessState::Ready);
            }
            ProcessState::BlockedPipeWrite(id) if crate::pipe::has_space(id) => {
                entry.set_state(ProcessState::Ready);
            }
            ProcessState::BlockedWaitPid(child_pid) | ProcessState::BlockedThreadJoin(child_pid) => {
                if zombie_pids.contains(&child_pid) {
                    entry.set_state(ProcessState::Ready);
                }
            }
            ProcessState::BlockedPoll { deadline } => {
                // Spurious wakeup: wake on any global event or timeout.
                // The poll syscall rechecks and re-blocks if nothing is ready.
                // Keyboard and network don't have explicit wake calls for
                // BlockedPoll (unlike pipes), so idle_poll handles them here.
                if kb_ready || net_ready
                    || (deadline > 0 && crate::clock::nanos_since_boot() >= deadline)
                {
                    entry.set_state(ProcessState::Ready);
                }
            }
            ProcessState::BlockedRecvMsg => {
                // No polling needed — send_message wakes directly.
            }
            ProcessState::BlockedNetRecv { deadline } if net_ready
                || (deadline > 0 && crate::clock::nanos_since_boot() >= deadline) =>
            {
                entry.set_state(ProcessState::Ready);
            }
            ProcessState::BlockedSleep { deadline } if crate::clock::nanos_since_boot() >= deadline => {
                entry.set_state(ProcessState::Ready);
            }
            ProcessState::BlockedFutex { deadline, .. } if deadline > 0 && crate::clock::nanos_since_boot() >= deadline => {
                entry.set_state(ProcessState::Ready);
            }
            _ => {}
        }
    }
}

/// Wake processes blocked on reading from a pipe that now has data.
/// Uses spurious wakeups for BlockedPoll — the poll syscall will recheck.
pub fn wake_pipe_readers(pipe_id: crate::pipe::PipeId) {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    for (_, entry) in table.iter_mut() {
        match *entry.state() {
            ProcessState::BlockedPipeRead(id) if id == pipe_id => {
                entry.set_state(ProcessState::Ready);
            }
            ProcessState::BlockedPoll { .. } => {
                // Spurious wakeup — poll syscall will recheck FD readiness.
                entry.set_state(ProcessState::Ready);
            }
            _ => {}
        }
    }
}

/// Wake processes blocked on writing to a pipe that now has space.
/// Uses spurious wakeups for BlockedPoll — the poll syscall will recheck.
pub fn wake_pipe_writers(pipe_id: crate::pipe::PipeId) {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    for (_, entry) in table.iter_mut() {
        match *entry.state() {
            ProcessState::BlockedPipeWrite(id) if id == pipe_id => {
                entry.set_state(ProcessState::Ready);
            }
            ProcessState::BlockedPoll { .. } => {
                // Spurious wakeup — poll syscall will recheck FD readiness.
                entry.set_state(ProcessState::Ready);
            }
            _ => {}
        }
    }
}

/// Naked assembly context switch.
/// Saves callee-saved regs to old stack, loads new stack, restores regs, returns.
#[unsafe(naked)]
unsafe extern "C" fn context_switch(old_rsp: *mut u64, new_rsp: u64) {
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",   // save old RSP
        "mov rsp, rsi",     // load new RSP
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    );
}
