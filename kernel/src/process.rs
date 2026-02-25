use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::naked_asm;
use hashbrown::HashMap;

use crate::arch::{cpu, paging, percpu};
use crate::arch::paging::PAGE_2M;
use crate::fd::{self, Descriptor, FdTable};
use crate::id_map::IdMap;
use crate::message::MessageQueue;
use crate::sync::Lock;
use crate::symbols::ProcessSymbols;
use crate::{elf, log, pipe, scheduler, shared_memory, vfs};

const USER_STACK_SIZE: usize = PAGE_2M as usize;
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
    BlockedPoll { fds: [u64; 8], len: u32 },
    BlockedRecvMsg,
    Zombie(i32),
}

pub struct Process {
    pub pid: u32,
    pub state: ProcessState,
    // Per-process page table (physical address of PML4)
    pub cr3: u64,
    // Kernel context (saved RSP during context switch)
    pub kernel_stack_base: *mut u8,
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
    // Crash diagnostics
    pub symbols: ProcessSymbols,
}

pub struct ProcessTable {
    pub procs: IdMap<u32, Process>,
}

impl ProcessTable {
    fn new() -> Self {
        Self { procs: IdMap::new() }
    }
}

pub static PROCESS_TABLE: Lock<Option<ProcessTable>> = Lock::new(None);
static NAME_REGISTRY: Lock<Option<HashMap<String, u32>>> = Lock::new(None);

pub fn init() {
    *PROCESS_TABLE.lock() = Some(ProcessTable::new());
    *NAME_REGISTRY.lock() = Some(HashMap::new());
}

pub fn current_pid() -> u32 {
    percpu::current_pid()
}

/// Access the current process immutably. The process table lock is held for
/// the duration of the closure. The closure may acquire locks that come AFTER
/// process_table in the ordering (vfs, pipes, keyboard, device, allocator).
pub fn with_current<R>(f: impl FnOnce(&Process) -> R) -> R {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().expect("process table not initialized");
    let pid = percpu::current_pid();
    f(table.procs.get(pid).unwrap())
}

/// Access the current process mutably. Same lock ordering rules as with_current.
pub fn with_current_mut<R>(f: impl FnOnce(&mut Process) -> R) -> R {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let pid = percpu::current_pid();
    f(table.procs.get_mut(pid).unwrap())
}

/// Initialize process 0 (init). Called from main after all kernel init.
pub fn init_process0(
    entry: u64, user_stack_top: u64,
    elf_base: *mut u8, elf_layout: Layout,
    stack_base: *mut u8, stack_layout: Layout,
    cr3: u64,
    syms: ProcessSymbols,
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
        *frame_ptr.add(6) = process_start as u64;
    }

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let pid = table.procs.insert(Process {
        pid: 0,
        state: ProcessState::Running,
        cr3,
        kernel_stack_base: ks_base,
        kernel_stack_layout: ks_layout,
        kernel_rsp: frame_ptr as u64,
        fds,
        user_heap: crate::user_heap::new_heap(),
        messages: MessageQueue::new(),
        cwd: String::from("/"),
        parent_pid: None,
        elf_base,
        elf_layout,
        stack_base,
        stack_layout,
        symbols: syms,
    });
    percpu::set_current_pid(pid);
    unsafe { percpu::set_kernel_stack(ks_top); }
    percpu::reset_idle(scheduler::idle_unlock_and_loop as u64);

    // Switch to process 0's page tables before entering userspace
    unsafe { cpu::write_cr3(cr3); }

    // Hold lock through context_switch — process_start releases it.
    core::mem::forget(guard);

    let mut dummy_rsp: u64 = 0;
    unsafe { context_switch(&mut dummy_rsp, frame_ptr as u64); }
    // Never returns
}

/// Release the process table lock held across context_switch.
/// Called by process_start before entering userspace.
fn scheduler_unlock() {
    unsafe { PROCESS_TABLE.force_unlock(); }
}

/// Entry point for new processes. Entered via context_switch's `ret`.
/// r12 = entry point, r13 = user stack pointer.
/// Releases the scheduler lock, then enters ring 3 via iretq.
#[unsafe(naked)]
extern "C" fn process_start() {
    naked_asm!(
        // Save r12/r13 across the Rust call
        "push r12",
        "push r13",
        "call {unlock}",
        "pop r13",
        "pop r12",
        // Enter userspace
        "push 0x1B",        // SS: user_data | RPL=3
        "push r13",         // RSP: user stack
        "push 0x202",       // RFLAGS: IF=1
        "push 0x23",        // CS: user_code | RPL=3
        "push r12",         // RIP: entry point
        "swapgs",
        "iretq",
        unlock = sym scheduler_unlock,
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

    // Create per-process page tables
    let child_pml4 = paging::create_user_pml4();
    let child_cr3 = child_pml4 as u64;

    // Map ELF and stack in the child's page tables (2MB aligned)
    let elf_alloc_size = ((loaded.load_size + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1)) as u64;
    let elf_layout = Layout::from_size_align(elf_alloc_size as usize, PAGE_2M as usize).unwrap();
    paging::map_user_in(child_pml4, loaded.base_ptr as u64, elf_alloc_size);

    let stack_layout = Layout::from_size_align(USER_STACK_SIZE, PAGE_2M as usize).unwrap();
    let stack_base = unsafe { alloc_zeroed(stack_layout) };
    if stack_base.is_null() {
        paging::free_user_page_tables(child_pml4);
        return u64::MAX;
    }
    let stack_top = stack_base as u64 + USER_STACK_SIZE as u64;
    paging::map_user_in(child_pml4, stack_base as u64, USER_STACK_SIZE as u64);

    let sp = write_argv_to_stack(stack_top, argv);

    // Parse symbols before we lock the process table
    let syms = ProcessSymbols::parse(
        &binary, loaded.base,
        loaded.base_ptr as u64, loaded.base_ptr as u64 + loaded.load_size as u64,
        stack_base as u64, stack_top,
    );
    // Allocate kernel stack for child
    let ks_layout = Layout::from_size_align(KERNEL_STACK_SIZE, 4096).unwrap();
    let ks_base = unsafe { alloc_zeroed(ks_layout) };
    if ks_base.is_null() {
        paging::free_user_page_tables(child_pml4);
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
        *frame_ptr.add(6) = process_start as u64;
    }

    // Lock process table to read parent and create child
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let parent_pid = percpu::current_pid();

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
        cr3: child_cr3,
        kernel_stack_base: ks_base,
        kernel_stack_layout: ks_layout,
        kernel_rsp: frame_ptr as u64,
        fds: child_fds,
        user_heap: crate::user_heap::new_heap(),
        messages: MessageQueue::new(),
        cwd: parent_cwd,
        parent_pid: Some(parent_pid),
        elf_base: loaded.base_ptr,
        elf_layout,
        stack_base,
        stack_layout,
        symbols: syms,
    });
    // Set the actual PID now that we know it
    table.procs.get_mut(pid).unwrap().pid = pid;

    log!("spawn: {} pid={} base={:#x} entry={:#x} cr3={:#x} ks={:#x}..{:#x}",
        path, pid, loaded.base_ptr as u64, loaded.entry, child_cr3, ks_base as u64, ks_top);

    pid as u64
}

/// Exit the current process.
pub fn exit(code: i32) -> ! {
    {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table not initialized");
        let pid = percpu::current_pid();
        let proc = table.procs.get_mut(pid).unwrap();

        // Close all FDs (acquires VFS lock — correct order: process_table < vfs)
        fd::close_all(&mut proc.fds, &mut *vfs::lock(), proc.pid);

        let proc_cr3 = proc.cr3;
        let pml4 = proc_cr3 as *mut u64;

        // Switch to kernel page tables before freeing process page tables
        unsafe { cpu::write_cr3(paging::kernel_cr3()); }

        // Clean up shared memory (unmap, free owned regions)
        shared_memory::cleanup_process(pid, pml4);

        // Free user memory
        unsafe {
            dealloc(proc.elf_base, proc.elf_layout);
            dealloc(proc.stack_base, proc.stack_layout);
        }

        // Free per-process page table structures
        paging::free_user_page_tables(pml4);
        proc.cr3 = 0;

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
    scheduler::schedule_no_return();
}

/// Block the current process and switch to the next ready one.
pub fn block(reason: ProcessState) {
    scheduler::block(reason);
}

/// Cooperative yield: mark current as Ready, switch to next.
pub fn yield_now() {
    scheduler::yield_now();
}

/// Send a message to a target process. Wakes the target if blocked.
pub fn send_message(target_pid: u32, msg: crate::message::Message) -> bool {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    if let Some(proc) = table.procs.get_mut(target_pid) {
        proc.messages.push(msg);
        match proc.state {
            ProcessState::BlockedRecvMsg | ProcessState::BlockedPoll { .. } => {
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
    scheduler::wake_pipe_readers(pipe_id);
}

/// Wake processes blocked on writing to a pipe that now has space.
pub fn wake_pipe_writers(pipe_id: usize) {
    scheduler::wake_pipe_writers(pipe_id);
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

/// Resolve an address using the current process's symbols. Lock-safe (brief hold).
pub fn resolve_symbol(addr: u64) -> Option<(String, u64)> {
    let pid = percpu::current_pid();
    if pid == u32::MAX { return None; }
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref()?;
    table.procs.get(pid)?.symbols.resolve(addr)
}

/// Check if an address is in the current process's valid memory ranges.
pub fn is_valid_user_addr(addr: u64) -> bool {
    let pid = percpu::current_pid();
    if pid == u32::MAX { return false; }
    let guard = PROCESS_TABLE.lock();
    let table = match guard.as_ref() { Some(t) => t, None => return false };
    match table.procs.get(pid) {
        Some(proc) => proc.symbols.is_valid_user_addr(addr),
        None => false,
    }
}

/// AP entry into the scheduler. Called from smp::ap_entry after SMP_READY.
pub fn ap_idle() -> ! {
    scheduler::schedule_no_return();
}

/// context_switch used only for init_process0's initial switch.
/// The main context_switch lives in scheduler.rs.
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
