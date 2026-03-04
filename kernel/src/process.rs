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

// ---------------------------------------------------------------------------
// OwnedAlloc — RAII wrapper for page-aligned allocations
// ---------------------------------------------------------------------------

/// Move-only wrapper around a (`*mut u8`, `Layout`) pair.
/// `Drop` calls `dealloc`, so forgetting to free memory is a compile-time error
/// (you'd have to actively `mem::forget` it).
pub struct OwnedAlloc {
    ptr: *mut u8,
    layout: Layout,
}

impl OwnedAlloc {
    /// Allocate zeroed memory with the given size and alignment.
    /// Returns `None` if the allocator returns null.
    pub fn new(size: usize, align: usize) -> Option<Self> {
        let layout = Layout::from_size_align(size, align).ok()?;
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() { None } else { Some(Self { ptr, layout }) }
    }

    /// Wrap an existing allocation. Caller must guarantee that `(ptr, layout)`
    /// was returned by `alloc_zeroed` (or equivalent) and is not aliased.
    pub unsafe fn from_raw(ptr: *mut u8, layout: Layout) -> Self {
        Self { ptr, layout }
    }

    /// Consume `self` without running `Drop`, returning the raw parts.
    /// The caller takes ownership of the allocation.
    pub fn into_raw(self) -> (*mut u8, Layout) {
        let parts = (self.ptr, self.layout);
        core::mem::forget(self);
        parts
    }

    pub fn ptr(&self) -> *mut u8 { self.ptr }
    pub fn size(&self) -> usize { self.layout.size() }
}

impl Drop for OwnedAlloc {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout); }
    }
}

// OwnedAlloc is Send — the underlying allocation is just raw memory.
unsafe impl Send for OwnedAlloc {}

const USER_STACK_SIZE: usize = 4 * PAGE_2M as usize; // 8 MB
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

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
    BlockedPoll { fds: [u64; 64], len: u32, deadline: u64 },
    BlockedRecvMsg,
    BlockedNetRecv { deadline: u64 },
    BlockedSleep { deadline: u64 },
    Zombie(i32),
}

#[derive(Clone, Copy)]
pub enum Kind {
    /// A process (may have a parent process for waitpid).
    Process { parent: Option<u32> },
    /// A thread within a process (shares address space with parent).
    Thread { parent: u32 },
}

pub struct Process {
    pub pid: u32,
    pub state: ProcessState,
    pub kind: Kind,
    // Per-process page table (physical address of PML4)
    pub cr3: u64,
    // Kernel context (saved RSP during context switch)
    pub kernel_stack: OwnedAlloc,
    pub kernel_rsp: u64,
    // Per-process state
    pub fds: FdTable,
    pub user_heap: Vec<(u64, u64)>,
    pub cwd: String,
    pub messages: MessageQueue,
    /// PID whose user_heap to use for alloc/free. Self for processes, parent for threads.
    pub heap_owner: u32,
    // ELF memory (owned; .take() in exit frees immediately)
    pub elf_alloc: Option<OwnedAlloc>,
    // User stack (owned; .take() in exit frees immediately)
    pub stack_alloc: Option<OwnedAlloc>,
    // Thread-local storage
    pub fs_base: u64,
    tls_template: u64,
    tls_filesz: usize,
    tls_memsz: usize,
    pub tls_alloc: Option<OwnedAlloc>,
    // Crash diagnostics
    pub symbols: ProcessSymbols,
    // Process name (filename from argv[0], null-terminated)
    pub name: [u8; 28],
    // Dynamically loaded shared libraries (indexed by dlopen handle)
    pub loaded_libs: Vec<elf::LoadedLib>,
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
/// Returns (alloc, fs_base).
pub fn setup_tls(tls_template: u64, tls_filesz: usize, tls_memsz: usize) -> Option<(OwnedAlloc, u64)> {
    let block_size = tls_memsz + 8; // TLS data + self-pointer
    let alloc_size = paging::align_2m(block_size);
    let alloc = OwnedAlloc::new(alloc_size, PAGE_2M as usize)?;
    let block = alloc.ptr();

    // Copy initialized data (.tdata)
    if tls_filesz > 0 && tls_template != 0 {
        unsafe { core::ptr::copy_nonoverlapping(tls_template as *const u8, block, tls_filesz); }
    }
    // .tbss already zeroed by alloc_zeroed

    // Thread pointer = address past TLS block
    let tp = block as u64 + tls_memsz as u64;
    // Self-pointer at %fs:0
    unsafe { *(tp as *mut u64) = tp; }

    Some((alloc, tp))
}

/// Allocate a kernel stack and set up the initial register frame for context_switch.
/// Returns (alloc, saved_rsp).
fn alloc_kernel_stack(
    trampoline: unsafe extern "C" fn(),
    user_entry: u64,
    user_sp: u64,
    arg: u64,
) -> Option<(OwnedAlloc, u64)> {
    let alloc = OwnedAlloc::new(KERNEL_STACK_SIZE, 4096)?;
    let top = alloc.ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let frame = (top - 7 * 8) as *mut u64;
    unsafe {
        *frame.add(0) = 0;                    // r15
        *frame.add(1) = arg;                  // r14
        *frame.add(2) = user_sp;              // r13
        *frame.add(3) = user_entry;           // r12
        *frame.add(4) = 0;                    // rbx
        *frame.add(5) = 0;                    // rbp
        *frame.add(6) = trampoline as u64;    // return address
    }
    Some((alloc, frame as u64))
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

/// Spawn a thread within the current process.
pub fn spawn_thread(entry: u64, stack_ptr: u64, arg: u64) -> Option<u32> {
    // Read parent's TLS info (brief lock)
    let (tls_template, tls_filesz, tls_memsz) = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().expect("process table not initialized");
        let parent = table.procs.get(percpu::current_pid()).unwrap();
        (parent.tls_template, parent.tls_filesz, parent.tls_memsz)
    };

    // Allocate TLS for the thread (outside lock — map_user does TLB flush)
    let (tls_alloc, fs_base) = setup_tls(tls_template, tls_filesz, tls_memsz)?;
    paging::map_user(tls_alloc.ptr() as u64, tls_alloc.size() as u64);

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(thread_start, entry, stack_ptr, arg) {
        Some(ks) => ks,
        None => {
            drop(tls_alloc);
            return None;
        }
    };

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
        kind: Kind::Thread { parent: parent_pid },
        cr3: parent_cr3,
        kernel_stack: ks_alloc,
        kernel_rsp: ks_rsp,
        fds: child_fds,
        user_heap: Vec::new(), // unused — routes through heap_owner
        messages: MessageQueue::new(),
        cwd: parent_cwd,
        heap_owner: parent_heap_owner,
        elf_alloc: None,
        stack_alloc: None,
        fs_base,
        tls_template,
        tls_filesz,
        tls_memsz,
        tls_alloc: Some(tls_alloc),
        symbols: ProcessSymbols::empty(),
        name: [0; 28],
        loaded_libs: Vec::new(),
    });
    table.procs.get_mut(tid).unwrap().pid = tid;

    Some(tid)
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

/// Spawn a new process from an ELF binary.
pub fn spawn(argv: &[&str], fds: FdTable, parent: Option<u32>) -> Option<u32> {
    let path = argv[0];

    let binary = match vfs::lock().read_file(path) {
        Ok(data) => data,
        Err(e) => {
            log!("{}: {}", path, e);
            return None;
        }
    };

    let (elf_alloc, loaded) = match elf::load(&binary) {
        Ok(l) => l,
        Err(msg) => {
            log!("{}", msg);
            return None;
        }
    };

    // Load DT_NEEDED shared libraries and apply GLOB_DAT relocations
    let loaded_libs = match elf::resolve_dynamic_deps(&binary, loaded.base, path, |lib_path| {
        vfs::lock().read_file(lib_path)
    }) {
        Ok(libs) => libs,
        Err(msg) => {
            log!("{}: {}", path, msg);
            return None;
        }
    };

    let child_pml4 = paging::create_user_pml4();
    let child_cr3 = child_pml4 as u64;

    paging::map_user_in(child_pml4, elf_alloc.ptr() as u64, elf_alloc.size() as u64);

    // Map loaded shared libraries into the child's address space
    for lib in &loaded_libs {
        paging::map_user_in(child_pml4, lib.alloc.ptr() as u64, lib.alloc.size() as u64);
    }

    let stack_alloc = match OwnedAlloc::new(USER_STACK_SIZE, PAGE_2M as usize) {
        Some(a) => a,
        None => {
            paging::free_user_page_tables(child_pml4);
            return None;
        }
    };
    let stack_top = stack_alloc.ptr() as u64 + USER_STACK_SIZE as u64;
    paging::map_user_in(child_pml4, stack_alloc.ptr() as u64, USER_STACK_SIZE as u64);

    let (tls_alloc, fs_base) = setup_tls(loaded.tls_template, loaded.tls_filesz, loaded.tls_memsz)?;
    paging::map_user_in(child_pml4, tls_alloc.ptr() as u64, tls_alloc.size() as u64);

    let sp = write_argv_to_stack(stack_top, argv);

    let syms = ProcessSymbols::parse(
        &binary, loaded.base,
        elf_alloc.ptr() as u64, elf_alloc.ptr() as u64 + elf_alloc.size() as u64,
        stack_alloc.ptr() as u64, stack_top,
    );

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(process_start, loaded.entry, sp, 0) {
        Some(ks) => ks,
        None => {
            paging::free_user_page_tables(child_pml4);
            return None;
        }
    };

    let ks_base = ks_alloc.ptr() as u64;

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");

    let cwd = match parent {
        Some(ppid) => table.procs.get(ppid).unwrap().cwd.clone(),
        None => String::from("/"),
    };

    let pid = table.procs.insert(Process {
        pid: 0,
        state: ProcessState::Ready,
        kind: Kind::Process { parent },
        cr3: child_cr3,
        kernel_stack: ks_alloc,
        kernel_rsp: ks_rsp,
        fds,
        user_heap: crate::user_heap::new_heap(),
        messages: MessageQueue::new(),
        cwd,
        heap_owner: 0,
        elf_alloc: Some(elf_alloc),
        stack_alloc: Some(stack_alloc),
        fs_base,
        tls_template: loaded.tls_template,
        tls_filesz: loaded.tls_filesz,
        tls_memsz: loaded.tls_memsz,
        tls_alloc: Some(tls_alloc),
        symbols: syms,
        name: make_name(path),
        loaded_libs,
    });
    let p = table.procs.get_mut(pid).unwrap();
    p.pid = pid;
    p.heap_owner = pid;

    log!("spawn: {} pid={} base={:#x} entry={:#x} cr3={:#x} ks={:#x}..{:#x}",
        path, pid, loaded.base as u64, loaded.entry, child_cr3,
        ks_base, ks_base + KERNEL_STACK_SIZE as u64);

    Some(pid)
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
    spawn(&full_argv, fds, None).expect("spawn_kernel: failed to spawn")
}

/// Exit the current process.
pub fn exit(code: i32) -> ! {
    {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().expect("process table not initialized");
        let pid = percpu::current_pid();
        let proc = table.procs.get_mut(pid).unwrap();
        let kind = proc.kind;

        // Close all FDs (acquires VFS lock — correct order: process_table < vfs)
        fd::close_all(&mut proc.fds, &mut *vfs::lock(), proc.pid);

        // Free TLS (RAII: .take() drops the OwnedAlloc)
        proc.tls_alloc.take();

        match kind {
            Kind::Thread { .. } => {
                // Thread: don't free address space (shared with parent).
                unsafe { cpu::write_cr3(paging::kernel_cr3()); }
            }
            Kind::Process { .. } => {
                let pml4 = proc.cr3 as *mut u64;
                unsafe { cpu::write_cr3(paging::kernel_cr3()); }
                shared_memory::cleanup_process(pid, pml4);
                // RAII: .take() drops OwnedAlloc, freeing the memory
                proc.elf_alloc.take();
                proc.stack_alloc.take();
                // Bug 1 fix: clear loaded_libs (each LoadedLib owns an OwnedAlloc)
                proc.loaded_libs.clear();
                paging::free_user_page_tables(pml4);
                proc.cr3 = 0;
            }
        }

        proc.state = ProcessState::Zombie(code);

        // Bug 3 fix: clean up registered name
        if let Kind::Process { .. } = kind {
            if let Some(names) = NAME_REGISTRY.lock().as_mut() {
                names.retain(|_, &mut v| v != pid);
            }
        }

        // Wake parent waiting on us
        let wake_pid = match kind {
            Kind::Process { parent } => parent,
            Kind::Thread { parent } => Some(parent),
        };
        if let Some(ppid) = wake_pid {
            if let Some(p) = table.procs.get_mut(ppid) {
                match p.state {
                    ProcessState::BlockedWaitPid(child) | ProcessState::BlockedThreadJoin(child) if child == pid => {
                        p.state = ProcessState::Ready;
                    }
                    _ => {}
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
/// Removing from the table drops the Process, whose OwnedAlloc fields
/// (kernel_stack) are freed automatically by Drop.
pub fn collect_zombie(child_pid: u32) -> Option<i32> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().expect("process table not initialized");
    let proc = table.procs.get(child_pid)?;
    if let ProcessState::Zombie(code) = proc.state {
        table.procs.remove(child_pid);
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

