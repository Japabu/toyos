use core::arch::{asm, naked_asm};

use crate::arch::{cpu, paging, percpu};
use crate::process::{ProcessState, ProcessTable, PROCESS_TABLE, KERNEL_STACK_SIZE};
use crate::{fd, keyboard};
const IA32_FS_BASE: u32 = 0xC0000100;

/// Block the current process and switch to the next ready one.
pub fn block(reason: ProcessState) {
    schedule(reason);
}

/// Cooperative yield: mark current as Ready, switch to next.
pub fn yield_now() {
    schedule(ProcessState::Ready);
}

/// Timer preemption: called from the timer interrupt handler when a process
/// is interrupted in user mode. Same as yield — mark Ready, switch to next.
pub fn preempt() {
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
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let cur_pid = percpu::current_pid();

    table.procs.get_mut(cur_pid).unwrap().state = cur_state;
    idle_poll(table);

    // Round-robin: find smallest Ready PID > current, or wrap to smallest Ready PID
    let mut best_after: Option<u32> = None;
    let mut best_any: Option<u32> = None;
    for (pid, proc) in table.procs.iter() {
        if proc.state == ProcessState::Ready {
            if pid > cur_pid && best_after.map_or(true, |b| pid < b) {
                best_after = Some(pid);
            }
            if best_any.map_or(true, |b| pid < b) {
                best_any = Some(pid);
            }
        }
    }

    if let Some(new_pid) = best_after.or(best_any) {
        if new_pid == cur_pid {
            table.procs.get_mut(cur_pid).unwrap().state = ProcessState::Running;
            return;
        }

        // Switch to another process.
        let new_proc = table.procs.get(new_pid).unwrap();
        let new_rsp = new_proc.kernel_rsp;
        let new_cr3 = new_proc.cr3;
        let new_fs_base = new_proc.fs_base;
        let new_ks_top = new_proc.kernel_stack_base as u64 + KERNEL_STACK_SIZE as u64;

        table.procs.get_mut(new_pid).unwrap().state = ProcessState::Running;
        percpu::set_current_pid(new_pid);

        // Save current FS base (TLS)
        table.procs.get_mut(cur_pid).unwrap().fs_base = cpu::rdmsr(IA32_FS_BASE);
        let old_rsp_ptr = &mut table.procs.get_mut(cur_pid).unwrap().kernel_rsp as *mut u64;
        unsafe { percpu::set_kernel_stack(new_ks_top); }

        unsafe { cpu::write_cr3(new_cr3); }
        cpu::wrmsr(IA32_FS_BASE, new_fs_base);

        // Hold lock through context_switch — the resuming side releases it.
        core::mem::forget(guard);
        unsafe { context_switch(old_rsp_ptr, new_rsp); }
        unsafe { PROCESS_TABLE.force_unlock(); }
        return;
    }

    // No Ready process — switch to per-CPU idle stack.
    // Save current FS base (TLS)
    table.procs.get_mut(cur_pid).unwrap().fs_base = cpu::rdmsr(IA32_FS_BASE);
    let old_rsp_ptr = &mut table.procs.get_mut(cur_pid).unwrap().kernel_rsp as *mut u64;
    percpu::set_current_pid(u32::MAX);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }

    unsafe { cpu::write_cr3(paging::kernel_cr3()); }

    // Hold lock through context_switch — idle_unlock_and_loop releases it.
    core::mem::forget(guard);
    unsafe { context_switch(old_rsp_ptr, percpu::idle_rsp()); }
    // Resumed by cpu_idle_loop — it held the lock for us.
    unsafe { PROCESS_TABLE.force_unlock(); }
}

/// Schedule without saving current context (used by exit and ap_idle).
/// Switches to the per-CPU idle stack and enters the idle loop.
pub fn schedule_no_return() -> ! {
    percpu::set_current_pid(u32::MAX);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { cpu::write_cr3(paging::kernel_cr3()); }
    let sp = percpu::idle_stack_top();
    unsafe {
        asm!(
            "mov rsp, {sp}",
            "jmp {func}",
            sp = in(reg) sp,
            func = in(reg) cpu_idle_loop as usize,
            options(noreturn),
        );
    }
}

/// Idle loop running on the per-CPU idle stack. Polls for I/O and dispatches
/// Ready processes via context_switch.
fn cpu_idle_loop() -> ! {
    loop {
        {
            let mut guard = PROCESS_TABLE.lock();
            let table = guard.as_mut().expect("process table not initialized");
            idle_poll(table);

            let ready = table.procs.iter()
                .find(|(_, p)| p.state == ProcessState::Ready)
                .map(|(pid, _)| pid);

            if let Some(new_pid) = ready {
                let new_proc = table.procs.get(new_pid).unwrap();
                let new_rsp = new_proc.kernel_rsp;
                let new_cr3 = new_proc.cr3;
                let new_fs_base = new_proc.fs_base;
                let new_ks_top = new_proc.kernel_stack_base as u64 + KERNEL_STACK_SIZE as u64;

                table.procs.get_mut(new_pid).unwrap().state = ProcessState::Running;
                percpu::set_current_pid(new_pid);
                unsafe { percpu::set_kernel_stack(new_ks_top); }

                unsafe { cpu::write_cr3(new_cr3); }
                cpu::wrmsr(IA32_FS_BASE, new_fs_base);

                core::mem::forget(guard);
                unsafe { context_switch(percpu::idle_rsp_ptr(), new_rsp); }
                // Resumed — schedule() held the lock when switching back to idle.
                unsafe { PROCESS_TABLE.force_unlock(); }
                continue;
            }
        }
        // Halt until next interrupt (timer at 100Hz or xHCI MSI-X).
        // `sti; hlt` atomically enables interrupts and halts — any pending
        // interrupt will wake the CPU immediately.
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)); }
    }
}

/// Entry point for the idle stack when first reached via context_switch from schedule().
/// schedule() held the lock via mem::forget — release it, then enter the idle loop.
pub fn idle_unlock_and_loop() -> ! {
    unsafe { PROCESS_TABLE.force_unlock(); }
    cpu_idle_loop()
}

/// Check whether a BlockedPoll process has any ready FDs.
fn poll_has_ready_fd(poll_fds: &[u64; 8], len: u32, fds: &fd::FdTable) -> bool {
    poll_fds[..len as usize].iter().any(|&fd_num| fd::has_data(fds, fd_num))
}

/// Poll for I/O and wake blocked processes.
fn idle_poll(table: &mut ProcessTable) {
    // Process USB events only when MSI-X interrupt fired (BSP only)
    if percpu::cpu_id() == 0 {
        crate::drivers::xhci::poll_if_pending();
    }

    let kb_ready = keyboard::has_data();
    let net_ready = crate::net::has_packet();

    let mut zombie_pids = alloc::vec::Vec::new();
    for (_, proc) in table.procs.iter() {
        if matches!(proc.state, ProcessState::Zombie(_)) {
            zombie_pids.push(proc.pid);
        }
    }

    for (_, proc) in table.procs.iter_mut() {
        match proc.state {
            ProcessState::BlockedKeyboard if kb_ready => {
                proc.state = ProcessState::Ready;
            }
            ProcessState::BlockedPipeRead(id) if crate::pipe::has_data(id) => {
                proc.state = ProcessState::Ready;
            }
            ProcessState::BlockedPipeWrite(_) => {
                proc.state = ProcessState::Ready;
            }
            ProcessState::BlockedWaitPid(child_pid) | ProcessState::BlockedThreadJoin(child_pid) => {
                if zombie_pids.contains(&child_pid) {
                    proc.state = ProcessState::Ready;
                }
            }
            ProcessState::BlockedPoll { fds: ref poll_fds, len, deadline } => {
                if poll_has_ready_fd(poll_fds, len, &proc.fds)
                    || proc.messages.has_messages()
                    || (deadline > 0 && crate::clock::nanos_since_boot() >= deadline)
                {
                    proc.state = ProcessState::Ready;
                }
            }
            ProcessState::BlockedRecvMsg => {
                if proc.messages.has_messages() {
                    proc.state = ProcessState::Ready;
                }
            }
            ProcessState::BlockedNetRecv { deadline } if net_ready
                || (deadline > 0 && crate::clock::nanos_since_boot() >= deadline) =>
            {
                proc.state = ProcessState::Ready;
            }
            ProcessState::BlockedSleep { deadline } if crate::clock::nanos_since_boot() >= deadline => {
                proc.state = ProcessState::Ready;
            }
            _ => {}
        }
    }
}

/// Wake processes blocked on reading from a pipe that now has data.
pub fn wake_pipe_readers(pipe_id: usize) {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    for (_, proc) in table.procs.iter_mut() {
        match proc.state {
            ProcessState::BlockedPipeRead(id) if id == pipe_id => {
                proc.state = ProcessState::Ready;
            }
            ProcessState::BlockedPoll { fds: ref poll_fds, len, .. } => {
                if poll_has_ready_fd(poll_fds, len, &proc.fds) {
                    proc.state = ProcessState::Ready;
                }
            }
            _ => {}
        }
    }
}

/// Wake processes blocked on writing to a pipe that now has space.
pub fn wake_pipe_writers(pipe_id: usize) {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    for (_, proc) in table.procs.iter_mut() {
        if proc.state == ProcessState::BlockedPipeWrite(pipe_id) {
            proc.state = ProcessState::Ready;
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
