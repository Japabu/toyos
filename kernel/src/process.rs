use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::{asm, naked_asm};
use hashbrown::HashMap;

use crate::arch::{paging, percpu};
use crate::fd::{self, Descriptor, FdTable};
use crate::id_map::IdMap;
use crate::message::MessageQueue;
use crate::sync::Lock;
use crate::{elf, keyboard, log, pipe, symbols, user_heap, vfs};

const USER_STACK_SIZE: usize = 64 * 1024;
const KERNEL_STACK_SIZE: usize = 64 * 1024;

/// Write argc, argv pointers, and string data onto a user stack. Returns new SP.
pub fn write_argv_to_stack(stack_top: u64, args: &[&str]) -> u64 {
    let mut sp = stack_top;
    let mut argv_ptrs: Vec<u64> = Vec::with_capacity(args.len());
    for arg in args.iter().rev() {
        sp -= (arg.len() + 1) as u64;
        unsafe {
            core::ptr::copy_nonoverlapping(arg.as_ptr(), sp as *mut u8, arg.len());
            *((sp + arg.len() as u64) as *mut u8) = 0;
        }
        argv_ptrs.push(sp);
    }
    argv_ptrs.reverse();
    let metadata_qwords = args.len() + 2;
    sp = (sp - metadata_qwords as u64 * 8) & !15;
    unsafe {
        *(sp as *mut u64) = args.len() as u64;
        for (i, ptr) in argv_ptrs.iter().enumerate() {
            *((sp + 8 + i as u64 * 8) as *mut u64) = *ptr;
        }
        *((sp + 8 + args.len() as u64 * 8) as *mut u64) = 0;
    }
    sp
}

#[derive(Clone, Copy, PartialEq)]
pub enum ProcessState {
    Running,
    Ready,
    BlockedKeyboard,
    BlockedPipeRead(usize),
    BlockedPipeWrite(usize),
    BlockedWaitPid(u32),
    BlockedPoll(u64, u32),
    BlockedRecvMsg,
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
    pub messages: MessageQueue,
    // Hierarchy
    pub parent_pid: Option<u32>,
    // ELF memory tracking
    elf_base: *mut u8,
    elf_layout: Layout,
    stack_base: *mut u8,
    stack_layout: Layout,
}

struct ProcessTable {
    procs: IdMap<u32, Process>,
    current: u32,
}

impl ProcessTable {
    fn new() -> Self {
        Self { procs: IdMap::new(), current: 0 }
    }
}

static PROCESS_TABLE: Lock<Option<ProcessTable>> = Lock::new(None);
static NAME_REGISTRY: Lock<Option<HashMap<String, u32>>> = Lock::new(None);

pub fn init() {
    *PROCESS_TABLE.lock() = Some(ProcessTable::new());
    *NAME_REGISTRY.lock() = Some(HashMap::new());
}

pub fn current_pid() -> u32 {
    PROCESS_TABLE.lock().as_ref().expect("process table not initialized").current
}

/// Access the current process immutably. The process table lock is held for
/// the duration of the closure. The closure may acquire locks that come AFTER
/// process_table in the ordering (vfs, pipes, keyboard, device, allocator).
pub fn with_current<R>(f: impl FnOnce(&Process) -> R) -> R {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().expect("process table not initialized");
    f(table.procs.get(table.current).unwrap())
}

/// Access the current process mutably. Same lock ordering rules as with_current.
pub fn with_current_mut<R>(f: impl FnOnce(&mut Process) -> R) -> R {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let current = table.current;
    f(table.procs.get_mut(current).unwrap())
}

/// Initialize process 0 (init). Called from main after all kernel init.
pub fn init_process0(
    entry: u64, user_stack_top: u64,
    elf_base: *mut u8, elf_layout: Layout,
    stack_base: *mut u8, stack_layout: Layout,
) {
    let ks_layout = Layout::from_size_align(KERNEL_STACK_SIZE, 4096).unwrap();
    let ks_base = unsafe { alloc_zeroed(ks_layout) };
    assert!(!ks_base.is_null(), "process 0: kernel stack alloc failed");
    let ks_top = ks_base as u64 + KERNEL_STACK_SIZE as u64;

    let mut fds = FdTable::new();
    fds.insert_at(0, Descriptor::SerialConsole); // stdin
    fds.insert_at(1, Descriptor::SerialConsole); // stdout
    fds.insert_at(2, Descriptor::SerialConsole); // stderr

    // Set up kernel stack so context_switch's `ret` goes to the trampoline
    // that enters ring 3 via iretq.
    // context_switch pops: r15, r14, r13, r12, rbx, rbp, then ret.
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

    {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table not initialized");
        let pid = table.procs.insert(Process {
            pid: 0,
            state: ProcessState::Running,
            kernel_stack_base: ks_base,
            kernel_stack_layout: ks_layout,
            kernel_rsp: frame_ptr as u64,
            fds,
            user_heap: Vec::new(),
            messages: MessageQueue::new(),
            cwd: String::from("/"),
            parent_pid: None,
            elf_base,
            elf_layout,
            stack_base,
            stack_layout,
        });
        table.current = pid;
    }

    unsafe { percpu::set_kernel_stack(ks_top); }

    // Context switch to process 0 (starts the trampoline)
    let mut dummy_rsp: u64 = 0;
    unsafe { context_switch(&mut dummy_rsp, frame_ptr as u64); }
    // Never returns
}

/// Trampoline for new processes. Entered via context_switch's `ret`.
/// r12 = entry point, r13 = user stack pointer.
/// swapgs before iretq: kernel→user GS transition.
#[unsafe(naked)]
extern "C" fn process_entry_trampoline() {
    naked_asm!(
        "push 0x1B",        // SS: user_data | RPL=3
        "push r13",         // RSP: user stack
        "push 0x202",       // RFLAGS: IF=1
        "push 0x23",        // CS: user_code | RPL=3
        "push r12",         // RIP: entry point
        "swapgs",
        "iretq",
    );
}

/// Spawn a new process from an ELF binary. Returns child PID or u64::MAX.
/// stdin_fd/stdout_fd: FD numbers in the parent to dup into child's FD 0/1,
/// or u64::MAX to inherit parent's FD 0/1 type.
pub fn spawn(argv: &[&str], stdin_fd: u64, stdout_fd: u64) -> u64 {
    let path = argv[0];

    // Load binary from VFS (no process table lock needed)
    let binary = match vfs::lock().read_file(path) {
        Some(data) => data,
        None => return u64::MAX,
    };

    let loaded = match elf::load(&binary) {
        Ok(l) => l,
        Err(msg) => {
            log!("{}", msg);
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

    let sp = write_argv_to_stack(stack_top, argv);

    // Allocate kernel stack for child
    let ks_layout = Layout::from_size_align(KERNEL_STACK_SIZE, 4096).unwrap();
    let ks_base = unsafe { alloc_zeroed(ks_layout) };
    if ks_base.is_null() {
        return u64::MAX;
    }
    let ks_top = ks_base as u64 + KERNEL_STACK_SIZE as u64;

    // Set up kernel stack frame for context switch -> trampoline
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

    // Lock process table to read parent and create child
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let parent_pid = table.current;

    let mut child_fds = FdTable::new();

    // FD 0 (stdin)
    let src_fd = if stdin_fd != u64::MAX { stdin_fd } else { 0 };
    let stdin_desc = match table.procs.get(parent_pid).unwrap().fds.get(src_fd) {
        Some(Descriptor::PipeRead(id)) => {
            pipe::add_reader(*id);
            Descriptor::PipeRead(*id)
        }
        Some(Descriptor::TtyRead(id)) => {
            pipe::add_reader(*id);
            Descriptor::TtyRead(*id)
        }
        Some(Descriptor::File(file)) => Descriptor::File(file.clone()),
        _ => Descriptor::SerialConsole,
    };
    child_fds.insert_at(0, stdin_desc);

    // FD 1 (stdout)
    let src_fd = if stdout_fd != u64::MAX { stdout_fd } else { 1 };
    let stdout_desc = match table.procs.get(parent_pid).unwrap().fds.get(src_fd) {
        Some(Descriptor::PipeWrite(id)) => {
            pipe::add_writer(*id);
            Descriptor::PipeWrite(*id)
        }
        Some(Descriptor::TtyWrite(id)) => {
            pipe::add_writer(*id);
            Descriptor::TtyWrite(*id)
        }
        Some(Descriptor::File(file)) => Descriptor::File(file.clone()),
        _ => Descriptor::SerialConsole,
    };
    // FD 2 (stderr) — same as stdout so panics appear in terminal
    let stderr_desc = match &stdout_desc {
        Descriptor::PipeWrite(id) => {
            pipe::add_writer(*id);
            Descriptor::PipeWrite(*id)
        }
        Descriptor::TtyWrite(id) => {
            pipe::add_writer(*id);
            Descriptor::TtyWrite(*id)
        }
        Descriptor::File(file) => Descriptor::File(file.clone()),
        _ => Descriptor::SerialConsole,
    };
    child_fds.insert_at(1, stdout_desc);
    child_fds.insert_at(2, stderr_desc);

    // Inherit parent's cwd
    let parent_cwd = table.procs.get(parent_pid).unwrap().cwd.clone();

    let pid = table.procs.insert(Process {
        pid: 0, // placeholder, set below
        state: ProcessState::Ready,
        kernel_stack_base: ks_base,
        kernel_stack_layout: ks_layout,
        kernel_rsp: frame_ptr as u64,
        fds: child_fds,
        user_heap: Vec::new(),
        messages: MessageQueue::new(),
        cwd: parent_cwd,
        parent_pid: Some(parent_pid),
        elf_base: loaded.base_ptr,
        elf_layout,
        stack_base,
        stack_layout,
    });
    // Set the actual PID now that we know it
    table.procs.get_mut(pid).unwrap().pid = pid;

    log!("spawn: pid={} entry={:#x} stack={:#x}", pid, loaded.entry, sp);

    pid as u64
}

/// Exit the current process.
pub fn exit(code: i32) -> ! {
    {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table not initialized");
        let pid = table.current;
        let proc = table.procs.get_mut(pid).unwrap();

        // Close all FDs (acquires VFS lock — correct order: process_table < vfs)
        fd::close_all(&mut proc.fds, &mut *vfs::lock(), proc.pid);

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
        if let Some(ppid) = proc.parent_pid {
            if let Some(parent) = table.procs.get_mut(ppid) {
                if parent.state == ProcessState::BlockedWaitPid(pid) {
                    parent.state = ProcessState::Ready;
                }
            }
        }
    }

    // Switch away (never save our context)
    schedule_no_return();
}

/// Block the current process and switch to the next ready one.
pub fn block(reason: ProcessState) {
    {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table not initialized");
        let current = table.current;
        table.procs.get_mut(current).unwrap().state = reason;
    }
    schedule();
}

/// Cooperative yield: mark current as Ready, switch to next.
pub fn yield_now() {
    {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table not initialized");
        let current = table.current;
        table.procs.get_mut(current).unwrap().state = ProcessState::Ready;
    }
    schedule();
}

/// Find next ready process (round-robin by PID order) and context switch to it.
pub fn schedule() {
    loop {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table not initialized");
        let current_pid = table.current;

        // Round-robin: find smallest Ready PID > current, or wrap to smallest Ready PID
        let mut best_after: Option<u32> = None;
        let mut best_any: Option<u32> = None;
        for (pid, proc) in table.procs.iter() {
            if proc.state == ProcessState::Ready {
                if pid > current_pid && best_after.map_or(true, |b| pid < b) {
                    best_after = Some(pid);
                }
                if best_any.map_or(true, |b| pid < b) {
                    best_any = Some(pid);
                }
            }
        }

        if let Some(new_pid) = best_after.or(best_any) {
            if new_pid == current_pid {
                table.procs.get_mut(current_pid).unwrap().state = ProcessState::Running;
                return;
            }

            // Save current process's user heap
            table.procs.get_mut(current_pid).unwrap().user_heap = user_heap::save();

            // Switch to new process
            table.procs.get_mut(new_pid).unwrap().state = ProcessState::Running;
            table.current = new_pid;

            let new_proc = table.procs.get(new_pid).unwrap();
            let new_rsp = new_proc.kernel_rsp;
            let new_ks_top = new_proc.kernel_stack_base as u64 + KERNEL_STACK_SIZE as u64;

            // Restore new process's user heap
            user_heap::restore(new_proc.user_heap.clone());

            // SAFETY: old_rsp_ptr points into the current process's IdMap slot.
            // The process won't be removed while we exist (only zombies are collected).
            // Safe in Phase 2 (AP idle); Phase 3 will use per-CPU state instead.
            let old_rsp_ptr = &mut table.procs.get_mut(current_pid).unwrap().kernel_rsp as *mut u64;

            // Drop lock before context switch to avoid deadlock when resumed
            drop(guard);

            unsafe { percpu::set_kernel_stack(new_ks_top); }

            // Load symbols for crash diagnostics
            symbols::clear();

            unsafe { context_switch(old_rsp_ptr, new_rsp); }
            return;
        }

        // No ready process — idle: poll USB, check for wakeups
        idle_poll(table);
        drop(guard);
        core::hint::spin_loop();
    }
}

/// Schedule without saving current context (used by exit).
fn schedule_no_return() -> ! {
    loop {
        {
            let mut guard = PROCESS_TABLE.lock();
            let table = guard.as_mut().expect("process table not initialized");

            let ready = table.procs.iter()
                .find(|(_, p)| p.state == ProcessState::Ready)
                .map(|(pid, _)| pid);

            if let Some(new_pid) = ready {
                let proc = table.procs.get_mut(new_pid).unwrap();
                proc.state = ProcessState::Running;
                table.current = new_pid;

                let new_rsp = proc.kernel_rsp;
                let new_ks_top = proc.kernel_stack_base as u64 + KERNEL_STACK_SIZE as u64;

                user_heap::restore(proc.user_heap.clone());

                drop(guard);

                unsafe { percpu::set_kernel_stack(new_ks_top); }

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

            idle_poll(table);
        }
        core::hint::spin_loop();
    }
}

/// Poll for I/O and wake blocked processes.
fn idle_poll(table: &mut ProcessTable) {
    crate::drivers::xhci::poll_global();

    let kb_ready = keyboard::has_data();

    let mut zombie_pids: Vec<u32> = Vec::new();
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
            ProcessState::BlockedPipeRead(id) if pipe::has_data(id) => {
                proc.state = ProcessState::Ready;
            }
            ProcessState::BlockedPipeWrite(_) => {
                proc.state = ProcessState::Ready;
            }
            ProcessState::BlockedWaitPid(child_pid) => {
                if zombie_pids.contains(&child_pid) {
                    proc.state = ProcessState::Ready;
                }
            }
            ProcessState::BlockedPoll(fds_ptr, fds_len) => {
                let fds = unsafe {
                    core::slice::from_raw_parts(fds_ptr as *const u64, fds_len as usize)
                };
                let any_fd_ready = fds.iter().any(|&fd| fd::has_data(&proc.fds, fd));
                if any_fd_ready || proc.messages.has_messages() {
                    proc.state = ProcessState::Ready;
                }
            }
            ProcessState::BlockedRecvMsg => {
                if proc.messages.has_messages() {
                    proc.state = ProcessState::Ready;
                }
            }
            _ => {}
        }
    }

    core::hint::spin_loop();
}

/// Send a message to a target process. Wakes the target if blocked.
pub fn send_message(target_pid: u32, msg: crate::message::Message) -> bool {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    if let Some(proc) = table.procs.get_mut(target_pid) {
        proc.messages.push(msg);
        match proc.state {
            ProcessState::BlockedRecvMsg | ProcessState::BlockedPoll(..) => {
                proc.state = ProcessState::Ready;
            }
            _ => {}
        }
        true
    } else {
        false
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
            ProcessState::BlockedPoll(fds_ptr, fds_len) => {
                let fds = unsafe {
                    core::slice::from_raw_parts(fds_ptr as *const u64, fds_len as usize)
                };
                if fds.iter().any(|&fd| fd::has_data(&proc.fds, fd)) {
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

/// Collect a zombie child. Returns exit code, or None if not a zombie yet.
pub fn collect_zombie(child_pid: u32) -> Option<i32> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let proc = table.procs.get(child_pid)?;
    if let ProcessState::Zombie(code) = proc.state {
        let ks_base = proc.kernel_stack_base;
        let ks_layout = proc.kernel_stack_layout;
        table.procs.remove(child_pid);
        unsafe { dealloc(ks_base, ks_layout); }
        Some(code)
    } else {
        None
    }
}

pub fn register_name(name: &str, pid: u32) -> bool {
    let mut guard = NAME_REGISTRY.lock();
    let names = guard.as_mut().expect("name registry not initialized");
    if names.contains_key(name) {
        return false;
    }
    names.insert(String::from(name), pid);
    true
}

pub fn find_pid(name: &str) -> Option<u32> {
    NAME_REGISTRY.lock().as_ref().expect("name registry not initialized").get(name).copied()
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
