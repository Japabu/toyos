use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::{asm, naked_asm};

use crate::arch::{gdt, paging, syscall};
use crate::fd::{self, Descriptor, FdTable};
use crate::id_map::IdMap;
use crate::message::MessageQueue;
use crate::sync::SyncCell;
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

static PROCESS_TABLE: SyncCell<Option<ProcessTable>> = SyncCell::new(None);

fn table() -> &'static mut ProcessTable {
    PROCESS_TABLE.get_mut().get_or_insert_with(|| ProcessTable {
        procs: IdMap::new(),
        current: 0,
    })
}

pub fn current_pid() -> u32 {
    table().current
}

pub fn current() -> &'static mut Process {
    let table = table();
    table.procs.get_mut(table.current).unwrap()
}

/// Initialize process 0 (init). Called from main after all kernel init.
pub fn init_process0(
    entry: u64, user_stack_top: u64,
    elf_base: *mut u8, elf_layout: Layout,
    stack_base: *mut u8, stack_layout: Layout,
) {
    let table = table();

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

    let table = table();
    let parent_pid = table.current;

    let mut child_fds = FdTable::new();

    // FD 0 (stdin)
    let src_fd = if stdin_fd != u64::MAX { stdin_fd } else { 0 };
    let parent = table.procs.get(table.current).unwrap();
    let stdin_desc = match parent.fds.get(src_fd) {
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
    let parent = table.procs.get(table.current).unwrap();
    let stdout_desc = match parent.fds.get(src_fd) {
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
    let parent_cwd = table.procs.get(table.current).unwrap().cwd.clone();

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
    let table = table();
    let pid = table.current;
    let proc = table.procs.get_mut(pid).unwrap();

    // Close all FDs
    fd::close_all(&mut proc.fds, vfs::global(), proc.pid);

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

    // Switch away (never save our context)
    schedule_no_return();
}

/// Block the current process and switch to the next ready one.
pub fn block(reason: ProcessState) {
    let table = table();
    table.procs.get_mut(table.current).unwrap().state = reason;
    schedule();
}

/// Cooperative yield: mark current as Ready, switch to next.
pub fn yield_now() {
    let table = table();
    table.procs.get_mut(table.current).unwrap().state = ProcessState::Ready;
    schedule();
}

/// Find next ready process (round-robin by PID order) and context switch to it.
pub fn schedule() {
    let table = table();
    let current_pid = table.current;

    loop {
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

            // Update TSS.RSP0 and SYSCALL_KERNEL_RSP
            *syscall::SYSCALL_KERNEL_RSP.get_mut() = new_ks_top;
            unsafe { *gdt::tss_rsp0_ptr() = new_ks_top; }

            // Load symbols for crash diagnostics
            symbols::clear();

            let old_rsp_ptr = &mut table.procs.get_mut(current_pid).unwrap().kernel_rsp as *mut u64;
            unsafe { context_switch(old_rsp_ptr, new_rsp); }
            return;
        }

        // No ready process — idle: poll USB, check for wakeups
        idle_poll(table);
    }
}

/// Schedule without saving current context (used by exit).
fn schedule_no_return() -> ! {
    let table = table();

    loop {
        // Find any Ready process
        for (pid, proc) in table.procs.iter_mut() {
            if proc.state == ProcessState::Ready {
                proc.state = ProcessState::Running;
                table.current = pid;

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

        idle_poll(table);
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
    let table = table();
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

/// Collect a zombie child. Returns exit code, or None if not a zombie yet.
pub fn collect_zombie(child_pid: u32) -> Option<i32> {
    let table = table();
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
