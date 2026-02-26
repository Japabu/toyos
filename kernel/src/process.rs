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

const USER_STACK_SIZE: usize = 4 * PAGE_2M as usize; // 8 MB
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
    BlockedThreadJoin(u32),
    BlockedPoll { fds: [u64; 8], len: u32, deadline: u64 },
    BlockedRecvMsg,
    BlockedNetRecv { deadline: u64 },
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
    /// If this is a thread, the PID of the parent process (shares address space).
    pub thread_parent: Option<u32>,
    /// PID whose user_heap to use for alloc/free. Self for processes, parent for threads.
    pub heap_owner: u32,
    // ELF memory tracking
    elf_base: *mut u8,
    pub elf_layout: Layout,
    stack_base: *mut u8,
    pub stack_layout: Layout,
    // Thread-local storage
    pub fs_base: u64,
    tls_template: u64,
    tls_filesz: usize,
    tls_memsz: usize,
    tls_block: *mut u8,
    tls_block_layout: Layout,
    // Crash diagnostics
    pub symbols: ProcessSymbols,
    // Process name (filename from argv[0], null-terminated)
    pub name: [u8; 28],
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

/// Allocate a TLS area using the x86-64 variant II layout:
/// [TLS data (.tdata + .tbss)] [TCB: self-pointer]
///                              ^-- FS base (thread pointer)
/// Returns (block_ptr, block_layout, fs_base).
pub fn setup_tls(tls_template: u64, tls_filesz: usize, tls_memsz: usize) -> (*mut u8, Layout, u64) {
    let block_size = tls_memsz + 8; // TLS data + self-pointer
    let alloc_size = (block_size + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1);
    let layout = Layout::from_size_align(alloc_size, PAGE_2M as usize).unwrap();
    let block = unsafe { alloc_zeroed(layout) };
    assert!(!block.is_null(), "TLS allocation failed");

    // Copy initialized data (.tdata)
    if tls_filesz > 0 && tls_template != 0 {
        unsafe { core::ptr::copy_nonoverlapping(tls_template as *const u8, block, tls_filesz); }
    }
    // .tbss already zeroed by alloc_zeroed

    // Thread pointer = address past TLS block
    let tp = block as u64 + tls_memsz as u64;
    // Self-pointer at %fs:0
    unsafe { *(tp as *mut u64) = tp; }

    (block, layout, tp)
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

/// Entry point for new threads. Entered via context_switch's `ret`.
/// r12 = entry point, r13 = user stack pointer, r14 = argument.
/// Releases the scheduler lock, then enters ring 3 via iretq with arg in rdi.
#[unsafe(naked)]
extern "C" fn thread_start() {
    naked_asm!(
        "push r12",
        "push r13",
        "push r14",
        "call {unlock}",
        "pop r14",
        "pop r13",
        "pop r12",
        // Enter userspace with arg in rdi
        "mov rdi, r14",
        "sub r13, 8",       // ABI: RSP must be 16n+8 at function entry
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

/// Spawn a thread within the current process. Returns the thread's PID (TID).
pub fn spawn_thread(entry: u64, stack_ptr: u64, arg: u64) -> u64 {
    // Read parent's TLS info (brief lock)
    let (tls_template, tls_filesz, tls_memsz) = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().expect("process table not initialized");
        let parent = table.procs.get(percpu::current_pid()).unwrap();
        (parent.tls_template, parent.tls_filesz, parent.tls_memsz)
    };

    // Allocate TLS for the thread (outside lock — map_user does TLB flush)
    let (tls_block, tls_block_layout, fs_base) = setup_tls(tls_template, tls_filesz, tls_memsz);
    paging::map_user(tls_block as u64, tls_block_layout.size() as u64);

    // Allocate kernel stack
    let ks_layout = Layout::from_size_align(KERNEL_STACK_SIZE, 4096).unwrap();
    let ks_base = unsafe { alloc_zeroed(ks_layout) };
    if ks_base.is_null() {
        unsafe { dealloc(tls_block, tls_block_layout); }
        return u64::MAX;
    }
    let ks_top = ks_base as u64 + KERNEL_STACK_SIZE as u64;

    // Set up kernel stack frame: r15, r14=arg, r13=stack, r12=entry, rbx, rbp, ret addr
    let frame_ptr = (ks_top - 7 * 8) as *mut u64;
    unsafe {
        *frame_ptr.add(0) = 0;         // r15
        *frame_ptr.add(1) = arg;       // r14 = argument
        *frame_ptr.add(2) = stack_ptr; // r13 = user stack
        *frame_ptr.add(3) = entry;     // r12 = entry point
        *frame_ptr.add(4) = 0;         // rbx
        *frame_ptr.add(5) = 0;         // rbp
        *frame_ptr.add(6) = thread_start as u64;
    }

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let parent_pid = percpu::current_pid();
    let parent = table.procs.get(parent_pid).unwrap();
    let parent_cr3 = parent.cr3;
    let parent_heap_owner = parent.heap_owner;
    let parent_cwd = parent.cwd.clone();
    // Inherit stderr so panics are visible
    let mut child_fds = FdTable::new();
    let stderr_desc = match parent.fds.get(2) {
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
    child_fds.insert_at(2, stderr_desc);

    let tid = table.procs.insert(Process {
        pid: 0,
        state: ProcessState::Ready,
        cr3: parent_cr3,
        kernel_stack_base: ks_base,
        kernel_stack_layout: ks_layout,
        kernel_rsp: frame_ptr as u64,
        fds: child_fds,
        user_heap: Vec::new(), // unused — routes through heap_owner
        messages: MessageQueue::new(),
        cwd: parent_cwd,
        parent_pid: None,
        thread_parent: Some(parent_pid),
        heap_owner: parent_heap_owner,
        elf_base: core::ptr::null_mut(),
        elf_layout: Layout::from_size_align(0, 1).unwrap(),
        stack_base: core::ptr::null_mut(),
        stack_layout: Layout::from_size_align(0, 1).unwrap(),
        fs_base,
        tls_template,
        tls_filesz,
        tls_memsz,
        tls_block,
        tls_block_layout,
        symbols: ProcessSymbols::empty(),
        name: [0; 28],
    });
    table.procs.get_mut(tid).unwrap().pid = tid;

    tid as u64
}

fn make_name(path: &str) -> [u8; 28] {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let mut name = [0u8; 28];
    let len = filename.len().min(27);
    name[..len].copy_from_slice(&filename.as_bytes()[..len]);
    name
}

/// Build a child's FdTable from (child_fd, parent_fd) pairs.
/// Duplicates each referenced parent descriptor into the child table.
pub fn build_child_fds(pairs: &[[u32; 2]]) -> FdTable {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().expect("process table not initialized");
    let parent = table.procs.get(percpu::current_pid()).unwrap();
    let mut fds = FdTable::new();
    for &[child_fd, parent_fd] in pairs {
        if let Some(desc) = parent.fds.get(parent_fd as u64) {
            fds.insert_at(child_fd as u64, fd::dup(desc));
        }
    }
    fds
}

/// Spawn a new process from an ELF binary. Returns child PID or u64::MAX.
pub fn spawn(argv: &[&str], fds: FdTable, parent: Option<u32>) -> u64 {
    let path = argv[0];

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

    let child_pml4 = paging::create_user_pml4();
    let child_cr3 = child_pml4 as u64;

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

    let (tls_block, tls_block_layout, fs_base) = setup_tls(loaded.tls_template, loaded.tls_filesz, loaded.tls_memsz);
    paging::map_user_in(child_pml4, tls_block as u64, tls_block_layout.size() as u64);

    let sp = write_argv_to_stack(stack_top, argv);

    let syms = ProcessSymbols::parse(
        &binary, loaded.base,
        loaded.base_ptr as u64, loaded.base_ptr as u64 + loaded.load_size as u64,
        stack_base as u64, stack_top,
    );

    let ks_layout = Layout::from_size_align(KERNEL_STACK_SIZE, 4096).unwrap();
    let ks_base = unsafe { alloc_zeroed(ks_layout) };
    if ks_base.is_null() {
        paging::free_user_page_tables(child_pml4);
        return u64::MAX;
    }
    let ks_top = ks_base as u64 + KERNEL_STACK_SIZE as u64;

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

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");

    let cwd = match parent {
        Some(ppid) => table.procs.get(ppid).unwrap().cwd.clone(),
        None => String::from("/"),
    };

    let pid = table.procs.insert(Process {
        pid: 0,
        state: ProcessState::Ready,
        cr3: child_cr3,
        kernel_stack_base: ks_base,
        kernel_stack_layout: ks_layout,
        kernel_rsp: frame_ptr as u64,
        fds,
        user_heap: crate::user_heap::new_heap(),
        messages: MessageQueue::new(),
        cwd,
        parent_pid: parent,
        thread_parent: None,
        heap_owner: 0,
        elf_base: loaded.base_ptr,
        elf_layout,
        stack_base,
        stack_layout,
        fs_base,
        tls_template: loaded.tls_template,
        tls_filesz: loaded.tls_filesz,
        tls_memsz: loaded.tls_memsz,
        tls_block,
        tls_block_layout,
        symbols: syms,
        name: make_name(path),
    });
    let p = table.procs.get_mut(pid).unwrap();
    p.pid = pid;
    p.heap_owner = pid;

    log!("spawn: {} pid={} base={:#x} entry={:#x} cr3={:#x} ks={:#x}..{:#x}",
        path, pid, loaded.base_ptr as u64, loaded.entry, child_cr3, ks_base as u64, ks_top);

    pid as u64
}

/// Spawn a process from kernel context (during boot). Resolves bare names
/// to `/initrd/<name>`. Panics on failure.
pub fn spawn_kernel(argv: &[&str]) -> u32 {
    let path = argv[0];
    let full_path = if path.starts_with('/') {
        String::from(path)
    } else {
        alloc::format!("/initrd/{}", path)
    };
    let mut full_argv: Vec<&str> = Vec::with_capacity(argv.len());
    full_argv.push(&full_path);
    full_argv.extend_from_slice(&argv[1..]);
    let mut fds = FdTable::new();
    fds.insert_at(0, Descriptor::SerialConsole);
    fds.insert_at(1, Descriptor::SerialConsole);
    fds.insert_at(2, Descriptor::SerialConsole);
    let pid = spawn(&full_argv, fds, None);
    assert!(pid != u64::MAX, "spawn_kernel: failed to spawn {}", full_path);
    pid as u32
}

/// Exit the current process.
pub fn exit(code: i32) -> ! {
    {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table not initialized");
        let pid = percpu::current_pid();
        let proc = table.procs.get_mut(pid).unwrap();
        let is_thread = proc.thread_parent.is_some();

        // Close all FDs (acquires VFS lock — correct order: process_table < vfs)
        fd::close_all(&mut proc.fds, &mut *vfs::lock(), proc.pid);

        // Free this process/thread's TLS block
        if !proc.tls_block.is_null() {
            unsafe { dealloc(proc.tls_block, proc.tls_block_layout); }
        }

        if is_thread {
            // Thread: don't free address space (shared with parent).
            // Just switch to kernel page tables.
            unsafe { cpu::write_cr3(paging::kernel_cr3()); }
        } else {
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
        }

        // Mark as zombie and extract parent PIDs before releasing the borrow
        proc.state = ProcessState::Zombie(code);
        let parent = proc.parent_pid;
        let thread_parent = proc.thread_parent;

        // Wake parent if blocked on WaitPid for us
        if let Some(ppid) = parent {
            if let Some(p) = table.procs.get_mut(ppid) {
                if p.state == ProcessState::BlockedWaitPid(pid) {
                    p.state = ProcessState::Ready;
                }
            }
        }

        // Wake thread parent if blocked on ThreadJoin for us
        if let Some(ppid) = thread_parent {
            if let Some(p) = table.procs.get_mut(ppid) {
                if p.state == ProcessState::BlockedThreadJoin(pid) {
                    p.state = ProcessState::Ready;
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

