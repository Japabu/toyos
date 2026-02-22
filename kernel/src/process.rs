use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::{asm, naked_asm};
use core::fmt::Write;

use crate::arch::{gdt, paging, syscall};
use crate::drivers::serial;
use crate::fd::{self, Descriptor, FdTable};
use crate::sync::SyncCell;
use crate::{elf, keyboard, log, pipe, symbols, user_heap, vfs};

const MAX_PROCESSES: usize = 16;
const USER_STACK_SIZE: usize = 64 * 1024;
const KERNEL_STACK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq)]
pub enum ProcessState {
    Running,
    Ready,
    BlockedKeyboard,
    BlockedPipeRead(usize),
    BlockedPipeWrite(usize),
    BlockedWaitPid(u32),
    Zombie(i32),
}

pub struct Process {
    pub pid: u32,
    pub state: ProcessState,
    // Kernel context (saved RSP during context switch)
    kernel_stack_base: *mut u8,
    kernel_stack_layout: Layout,
    pub kernel_rsp: u64,
    // Per-process state
    pub fds: FdTable,
    pub user_heap: Vec<(u64, u64)>,
    pub cwd: String,
    // Hierarchy
    pub parent_pid: Option<u32>,
    // ELF memory tracking
    elf_base: *mut u8,
    elf_layout: Layout,
    stack_base: *mut u8,
    stack_layout: Layout,
}

struct ProcessTable {
    procs: [Option<Process>; MAX_PROCESSES],
    current: usize, // index of Running process
}

impl ProcessTable {
    const fn new() -> Self {
        Self {
            procs: [const { None }; MAX_PROCESSES],
            current: 0,
        }
    }
}

static PROCESS_TABLE: SyncCell<ProcessTable> = SyncCell::new(ProcessTable::new());

pub fn current_pid() -> u32 {
    let table = PROCESS_TABLE.get();
    table.procs[table.current].as_ref().unwrap().pid
}

pub fn current() -> &'static mut Process {
    let table = PROCESS_TABLE.get_mut();
    table.procs[table.current].as_mut().unwrap()
}

/// Initialize process 0 (init). Called from main after all kernel init.
pub fn init_process0(entry: u64, user_stack_top: u64, elf_base: *mut u8, elf_layout: Layout, stack_base: *mut u8, stack_layout: Layout) {
    let table = PROCESS_TABLE.get_mut();

    // Allocate kernel stack for process 0
    let ks_layout = Layout::from_size_align(KERNEL_STACK_SIZE, 4096).unwrap();
    let ks_base = unsafe { alloc_zeroed(ks_layout) };
    assert!(!ks_base.is_null(), "process 0: kernel stack alloc failed");
    let ks_top = ks_base as u64 + KERNEL_STACK_SIZE as u64;

    // Set up initial FDs: 0=Keyboard, 1=SerialConsole, 2=SerialConsole
    let mut fds = fd::new_fd_table();
    fds[0] = Some(Descriptor::Keyboard);
    fds[1] = Some(Descriptor::SerialConsole);
    fds[2] = Some(Descriptor::SerialConsole);

    // Set up kernel stack so context_switch's `ret` goes to the trampoline
    // that enters ring 3 via iretq.
    // context_switch pops: r15, r14, r13, r12, rbx, rbp, then ret.
    // Stack at frame_ptr (RSP points here, lowest address):
    //   [0] r15, [1] r14, [2] r13, [3] r12, [4] rbx, [5] rbp, [6] ret_addr
    let frame_ptr = (ks_top - 7 * 8) as *mut u64;
    unsafe {
        *frame_ptr.add(0) = 0; // r15
        *frame_ptr.add(1) = 0; // r14
        *frame_ptr.add(2) = user_stack_top; // r13 = user stack
        *frame_ptr.add(3) = entry; // r12 = entry point
        *frame_ptr.add(4) = 0; // rbx
        *frame_ptr.add(5) = 0; // rbp
        *frame_ptr.add(6) = process_entry_trampoline as u64;
    }

    user_heap::init();

    table.procs[0] = Some(Process {
        pid: 0,
        state: ProcessState::Running,
        kernel_stack_base: ks_base,
        kernel_stack_layout: ks_layout,
        kernel_rsp: frame_ptr as u64,
        fds,
        user_heap: Vec::new(),
        cwd: String::from("/"),
        parent_pid: None,
        elf_base,
        elf_layout,
        stack_base,
        stack_layout,
    });
    table.current = 0;

    // Set SYSCALL_KERNEL_RSP and TSS.RSP0 to the top of process 0's kernel stack
    *syscall::SYSCALL_KERNEL_RSP.get_mut() = ks_top;
    unsafe { *gdt::tss_rsp0_ptr() = ks_top; }

    // Context switch to process 0 (starts the trampoline)
    let mut dummy_rsp: u64 = 0;
    let new_rsp = frame_ptr as u64;
    unsafe {
        context_switch(&mut dummy_rsp, new_rsp);
    }
    // Never returns
}

/// Trampoline for new processes. Entered via context_switch's `ret`.
/// r12 = entry point, r13 = user stack pointer.
#[unsafe(naked)]
extern "C" fn process_entry_trampoline() {
    naked_asm!(
        "push 0x1B",        // SS: user_data | RPL=3
        "push r13",         // RSP: user stack
        "push 0x202",       // RFLAGS: IF=1
        "push 0x23",        // CS: user_code | RPL=3
        "push r12",         // RIP: entry point
        "iretq",
    );
}

/// Spawn a new process from an ELF binary. Returns child PID or u64::MAX.
/// stdin_fd/stdout_fd: FD numbers in the parent to dup into child's FD 0/1,
/// or u64::MAX to inherit parent's FD 0/1 type.
pub fn spawn(argv: &[&str], stdin_fd: u64, stdout_fd: u64) -> u64 {
    let path = argv[0];

    // Load binary from VFS
    let binary = match vfs::global().read_file(path) {
        Some(data) => data,
        None => return u64::MAX,
    };

    let loaded = match elf::load(&binary) {
        Ok(l) => l,
        Err(msg) => {
            log::println(msg);
            return u64::MAX;
        }
    };

    paging::map_user(loaded.base_ptr as u64, loaded.load_size as u64);

    let stack_layout = Layout::from_size_align(USER_STACK_SIZE, 4096).unwrap();
    let stack_base = unsafe { alloc_zeroed(stack_layout) };
    if stack_base.is_null() {
        return u64::MAX;
    }
    let stack_top = stack_base as u64 + USER_STACK_SIZE as u64;
    let elf_layout = Layout::from_size_align(loaded.load_size, 4096).unwrap();
    paging::map_user(stack_base as u64, USER_STACK_SIZE as u64);

    // Write argc/argv onto user stack
    let mut sp = stack_top;
    let mut argv_ptrs: Vec<u64> = Vec::with_capacity(argv.len());
    for arg in argv.iter().rev() {
        sp -= (arg.len() + 1) as u64;
        unsafe {
            core::ptr::copy_nonoverlapping(arg.as_ptr(), sp as *mut u8, arg.len());
            *((sp + arg.len() as u64) as *mut u8) = 0;
        }
        argv_ptrs.push(sp);
    }
    argv_ptrs.reverse();
    let metadata_qwords = argv.len() + 2;
    sp = (sp - metadata_qwords as u64 * 8) & !15;
    unsafe {
        *(sp as *mut u64) = argv.len() as u64;
        for (i, ptr) in argv_ptrs.iter().enumerate() {
            *((sp + 8 + i as u64 * 8) as *mut u64) = *ptr;
        }
        *((sp + 8 + argv.len() as u64 * 8) as *mut u64) = 0;
    }

    // Allocate kernel stack for child
    let ks_layout = Layout::from_size_align(KERNEL_STACK_SIZE, 4096).unwrap();
    let ks_base = unsafe { alloc_zeroed(ks_layout) };
    if ks_base.is_null() {
        return u64::MAX;
    }
    let ks_top = ks_base as u64 + KERNEL_STACK_SIZE as u64;

    // Find free PID
    let table = PROCESS_TABLE.get_mut();
    let slot_idx = match table.procs.iter().position(|p| p.is_none()) {
        Some(i) => i,
        None => return u64::MAX,
    };
    let pid = slot_idx as u32;
    let parent_pid = current_pid();

    // Set up child FDs
    let mut child_fds = fd::new_fd_table();

    // FD 0 (stdin)
    if stdin_fd != u64::MAX {
        let parent = table.procs[table.current].as_ref().unwrap();
        match &parent.fds[stdin_fd as usize] {
            Some(Descriptor::PipeRead(id)) => child_fds[0] = Some(Descriptor::PipeRead(*id)),
            Some(Descriptor::Keyboard) => child_fds[0] = Some(Descriptor::Keyboard),
            _ => child_fds[0] = Some(Descriptor::Keyboard),
        }
    } else {
        // Inherit parent's stdin type
        let parent = table.procs[table.current].as_ref().unwrap();
        match &parent.fds[0] {
            Some(Descriptor::Keyboard) => child_fds[0] = Some(Descriptor::Keyboard),
            Some(Descriptor::PipeRead(id)) => child_fds[0] = Some(Descriptor::PipeRead(*id)),
            _ => child_fds[0] = Some(Descriptor::Keyboard),
        }
    }

    // FD 1 (stdout)
    if stdout_fd != u64::MAX {
        let parent = table.procs[table.current].as_ref().unwrap();
        match &parent.fds[stdout_fd as usize] {
            Some(Descriptor::PipeWrite(id)) => child_fds[1] = Some(Descriptor::PipeWrite(*id)),
            Some(Descriptor::SerialConsole) => child_fds[1] = Some(Descriptor::SerialConsole),
            _ => child_fds[1] = Some(Descriptor::SerialConsole),
        }
    } else {
        let parent = table.procs[table.current].as_ref().unwrap();
        match &parent.fds[1] {
            Some(Descriptor::SerialConsole) => child_fds[1] = Some(Descriptor::SerialConsole),
            Some(Descriptor::PipeWrite(id)) => child_fds[1] = Some(Descriptor::PipeWrite(*id)),
            _ => child_fds[1] = Some(Descriptor::SerialConsole),
        }
    }

    // FD 2 (stderr) — always serial console
    child_fds[2] = Some(Descriptor::SerialConsole);

    // Inherit parent's cwd
    let parent_cwd = table.procs[table.current].as_ref().unwrap().cwd.clone();

    // Set up kernel stack frame for context switch -> trampoline
    // Pop order: r15, r14, r13, r12, rbx, rbp, ret
    let frame_ptr = (ks_top - 7 * 8) as *mut u64;
    unsafe {
        *frame_ptr.add(0) = 0; // r15
        *frame_ptr.add(1) = 0; // r14
        *frame_ptr.add(2) = sp; // r13 = user stack
        *frame_ptr.add(3) = loaded.entry; // r12 = entry
        *frame_ptr.add(4) = 0; // rbx
        *frame_ptr.add(5) = 0; // rbp
        *frame_ptr.add(6) = process_entry_trampoline as u64;
    }

    let _ = writeln!(serial::SerialWriter, "spawn: pid={} entry={:#x} stack={:#x}", pid, loaded.entry, sp);

    table.procs[slot_idx] = Some(Process {
        pid,
        state: ProcessState::Ready,
        kernel_stack_base: ks_base,
        kernel_stack_layout: ks_layout,
        kernel_rsp: frame_ptr as u64,
        fds: child_fds,
        user_heap: Vec::new(),
        cwd: parent_cwd,
        parent_pid: Some(parent_pid),
        elf_base: loaded.base_ptr,
        elf_layout,
        stack_base,
        stack_layout,
    });

    pid as u64
}

/// Exit the current process.
pub fn exit(code: i32) -> ! {
    let table = PROCESS_TABLE.get_mut();
    let idx = table.current;
    let proc = table.procs[idx].as_mut().unwrap();

    // Close all FDs
    fd::close_all(&mut proc.fds, vfs::global());

    // Free user memory
    paging::unmap_user(proc.elf_base as u64, proc.elf_layout.size() as u64);
    paging::unmap_user(proc.stack_base as u64, proc.stack_layout.size() as u64);
    unsafe {
        dealloc(proc.elf_base, proc.elf_layout);
        dealloc(proc.stack_base, proc.stack_layout);
    }

    // Mark as zombie
    proc.state = ProcessState::Zombie(code);

    // Wake parent if blocked on WaitPid for us
    let pid = proc.pid;
    if let Some(ppid) = proc.parent_pid {
        for slot in table.procs.iter_mut() {
            if let Some(p) = slot {
                if p.pid == ppid {
                    if p.state == ProcessState::BlockedWaitPid(pid) {
                        p.state = ProcessState::Ready;
                    }
                    break;
                }
            }
        }
    }

    // Switch away (never save our context)
    schedule_no_return();
}

/// Block the current process and switch to the next ready one.
pub fn block(reason: ProcessState) {
    let table = PROCESS_TABLE.get_mut();
    let idx = table.current;
    table.procs[idx].as_mut().unwrap().state = reason;
    schedule();
}

/// Cooperative yield: mark current as Ready, switch to next.
pub fn yield_now() {
    let table = PROCESS_TABLE.get_mut();
    let idx = table.current;
    table.procs[idx].as_mut().unwrap().state = ProcessState::Ready;
    schedule();
}

/// Find next ready process and context switch to it.
pub fn schedule() {
    let table = PROCESS_TABLE.get_mut();
    let old_idx = table.current;

    loop {
        // Try to find a Ready process (round-robin)
        let start = (old_idx + 1) % MAX_PROCESSES;
        let mut found = None;
        for i in 0..MAX_PROCESSES {
            let idx = (start + i) % MAX_PROCESSES;
            if let Some(proc) = &table.procs[idx] {
                if proc.state == ProcessState::Ready {
                    found = Some(idx);
                    break;
                }
            }
        }

        if let Some(new_idx) = found {
            if new_idx == old_idx {
                // Self-switch: just mark as Running and return
                table.procs[old_idx].as_mut().unwrap().state = ProcessState::Running;
                return;
            }

            // Save current process's user heap
            let old_proc = table.procs[old_idx].as_mut().unwrap();
            old_proc.user_heap = user_heap::save();

            // Switch to new process
            table.procs[new_idx].as_mut().unwrap().state = ProcessState::Running;
            table.current = new_idx;

            let new_proc = table.procs[new_idx].as_ref().unwrap();
            let new_rsp = new_proc.kernel_rsp;
            let new_ks_top = new_proc.kernel_stack_base as u64 + KERNEL_STACK_SIZE as u64;

            // Restore new process's user heap
            user_heap::restore(table.procs[new_idx].as_ref().unwrap().user_heap.clone());

            // Update TSS.RSP0 and SYSCALL_KERNEL_RSP
            *syscall::SYSCALL_KERNEL_RSP.get_mut() = new_ks_top;
            unsafe { *gdt::tss_rsp0_ptr() = new_ks_top; }

            // Load symbols for crash diagnostics
            symbols::clear();

            let old_rsp_ptr = &mut table.procs[old_idx].as_mut().unwrap().kernel_rsp as *mut u64;
            unsafe { context_switch(old_rsp_ptr, new_rsp); }
            return;
        }

        // No ready process — idle: poll USB, check for wakeups
        idle_poll(table);
    }
}

/// Schedule without saving current context (used by exit).
fn schedule_no_return() -> ! {
    let table = PROCESS_TABLE.get_mut();

    loop {
        // Find any Ready process
        for i in 0..MAX_PROCESSES {
            if let Some(proc) = &mut table.procs[i] {
                if proc.state == ProcessState::Ready {
                    proc.state = ProcessState::Running;
                    table.current = i;

                    let new_rsp = proc.kernel_rsp;
                    let new_ks_top = proc.kernel_stack_base as u64 + KERNEL_STACK_SIZE as u64;

                    user_heap::restore(proc.user_heap.clone());

                    *syscall::SYSCALL_KERNEL_RSP.get_mut() = new_ks_top;
                    unsafe { *gdt::tss_rsp0_ptr() = new_ks_top; }

                    symbols::clear();

                    // Jump without saving (same pop order as context_switch)
                    unsafe {
                        asm!(
                            "mov rsp, {rsp}",
                            "pop r15",
                            "pop r14",
                            "pop r13",
                            "pop r12",
                            "pop rbx",
                            "pop rbp",
                            "ret",
                            rsp = in(reg) new_rsp,
                            options(noreturn),
                        );
                    }
                }
            }
        }

        idle_poll(table);
    }
}

/// Poll for I/O and wake blocked processes.
fn idle_poll(table: &mut ProcessTable) {
    crate::drivers::xhci::poll_global();

    let kb_ready = keyboard::has_data();

    // Collect zombie PIDs first (avoids borrow conflict)
    let mut zombie_pids = [0u32; MAX_PROCESSES];
    let mut zombie_count = 0;
    for slot in table.procs.iter() {
        if let Some(p) = slot {
            if matches!(p.state, ProcessState::Zombie(_)) {
                zombie_pids[zombie_count] = p.pid;
                zombie_count += 1;
            }
        }
    }

    for slot in table.procs.iter_mut() {
        if let Some(proc) = slot {
            match proc.state {
                ProcessState::BlockedKeyboard if kb_ready => {
                    proc.state = ProcessState::Ready;
                }
                ProcessState::BlockedPipeRead(id) if pipe::has_data(id) => {
                    proc.state = ProcessState::Ready;
                }
                ProcessState::BlockedPipeWrite(_) => {
                    proc.state = ProcessState::Ready;
                }
                ProcessState::BlockedWaitPid(child_pid) => {
                    if zombie_pids[..zombie_count].contains(&child_pid) {
                        proc.state = ProcessState::Ready;
                    }
                }
                _ => {}
            }
        }
    }

    core::hint::spin_loop();
}

/// Collect a zombie child. Returns exit code, or None if not a zombie yet.
pub fn collect_zombie(child_pid: u32) -> Option<i32> {
    let table = PROCESS_TABLE.get_mut();
    for slot in table.procs.iter_mut() {
        if let Some(proc) = slot {
            if proc.pid == child_pid {
                if let ProcessState::Zombie(code) = proc.state {
                    // Free kernel stack
                    let ks_base = proc.kernel_stack_base;
                    let ks_layout = proc.kernel_stack_layout;
                    unsafe { dealloc(ks_base, ks_layout); }
                    *slot = None;
                    return Some(code);
                }
                return None;
            }
        }
    }
    None
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
