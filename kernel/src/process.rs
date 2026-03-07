use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::naked_asm;
use core::ptr::NonNull;
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
// Pid — newtype for process/thread IDs (compile-time separation from raw u32)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pid(u32);

impl Pid {
    pub const MAX: Self = Pid(u32::MAX);
    pub fn raw(self) -> u32 { self.0 }
    pub fn from_raw(v: u32) -> Self { Pid(v) }
}

impl core::fmt::Display for Pid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl core::ops::Add for Pid {
    type Output = Self;
    fn add(self, rhs: Self) -> Self { Pid(self.0 + rhs.0) }
}

impl crate::id_map::IdKey for Pid {
    const ZERO: Self = Pid(0);
    const ONE: Self = Pid(1);
}

// ---------------------------------------------------------------------------
// PageTableRoot — type-safe PML4 pointer (prevents double-free via Option::take)
// ---------------------------------------------------------------------------

/// Physical address of a PML4 page table. Used as `Option<PageTableRoot>` in Process
/// so that `take()` makes double-free of page tables impossible at compile time.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PageTableRoot(NonNull<u64>);

impl PageTableRoot {
    pub fn new(ptr: *mut u64) -> Self {
        Self(NonNull::new(ptr).expect("PageTableRoot::new with null"))
    }

    pub fn as_ptr(self) -> *mut u64 { self.0.as_ptr() }
    pub fn as_u64(self) -> u64 { self.0.as_ptr() as u64 }
}

// ---------------------------------------------------------------------------
// OwnedAlloc — RAII wrapper for page-aligned allocations
// ---------------------------------------------------------------------------

/// Move-only wrapper around a (`*mut u8`, `Layout`) pair.
/// `Drop` calls `dealloc`, so forgetting to free memory is a compile-time error
/// (you'd have to actively `mem::forget` it).
pub struct OwnedAlloc {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl OwnedAlloc {
    /// Allocate zeroed memory with the given size and alignment.
    /// Returns `None` if the allocator returns null.
    pub fn new(size: usize, align: usize) -> Option<Self> {
        let layout = Layout::from_size_align(size, align).ok()?;
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })?;
        Some(Self { ptr, layout })
    }

    /// Consume `self` without running `Drop`, returning the raw parts.
    /// The caller takes ownership of the allocation.
    pub fn into_raw(self) -> (*mut u8, Layout) {
        let parts = (self.ptr.as_ptr(), self.layout);
        core::mem::forget(self);
        parts
    }

    pub fn ptr(&self) -> *mut u8 { self.ptr.as_ptr() }
    pub fn size(&self) -> usize { self.layout.size() }
}

impl Drop for OwnedAlloc {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout); }
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
    BlockedPipeRead(pipe::PipeId),
    BlockedPipeWrite(pipe::PipeId),
    BlockedWaitPid(Pid),
    BlockedThreadJoin(Pid),
    BlockedPoll { deadline: u64 },
    BlockedRecvMsg,
    BlockedNetRecv { deadline: u64 },
    BlockedSleep { deadline: u64 },
    BlockedFutex { addr: u64, deadline: u64 },
    Zombie(i32),
}

impl ProcessState {
    fn is_blocked(&self) -> bool {
        matches!(self, Self::BlockedKeyboard | Self::BlockedPipeRead(_) | Self::BlockedPipeWrite(_)
            | Self::BlockedWaitPid(_) | Self::BlockedThreadJoin(_) | Self::BlockedPoll { .. }
            | Self::BlockedRecvMsg | Self::BlockedNetRecv { .. } | Self::BlockedSleep { .. }
            | Self::BlockedFutex { .. })
    }

    fn can_transition_to(&self, new: &Self) -> bool {
        match (self, new) {
            (Self::Zombie(_), _) => false,
            (_, Self::Zombie(_)) => true,
            (Self::Running, Self::Ready) => true,
            (Self::Running, _) if new.is_blocked() => true,
            (Self::Ready, Self::Running) => true,
            (s, Self::Ready) if s.is_blocked() => true,
            _ => false,
        }
    }

    /// Short name for debug messages (avoids printing large BlockedPoll data).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Ready => "Ready",
            Self::BlockedKeyboard => "BlockedKeyboard",
            Self::BlockedPipeRead(_) => "BlockedPipeRead",
            Self::BlockedPipeWrite(_) => "BlockedPipeWrite",
            Self::BlockedWaitPid(_) => "BlockedWaitPid",
            Self::BlockedThreadJoin(_) => "BlockedThreadJoin",
            Self::BlockedPoll { .. } => "BlockedPoll",
            Self::BlockedRecvMsg => "BlockedRecvMsg",
            Self::BlockedNetRecv { .. } => "BlockedNetRecv",
            Self::BlockedSleep { .. } => "BlockedSleep",
            Self::BlockedFutex { .. } => "BlockedFutex",
            Self::Zombie(_) => "Zombie",
        }
    }
}

#[derive(Clone, Copy)]
pub enum Kind {
    /// A process (may have a parent process for waitpid).
    Process { parent: Option<Pid> },
    /// A thread within a process (shares address space with parent).
    Thread { parent: Pid },
}

pub struct Process {
    pub pid: Pid,
    pub state: ProcessState,
    pub kind: Kind,
    // Per-process page table (physical address of PML4).
    // Option so teardown can take() it, making double-free impossible.
    pub cr3: Option<PageTableRoot>,
    // Kernel context (saved RSP during context switch)
    pub kernel_stack: OwnedAlloc,
    pub kernel_rsp: u64,
    // Per-process state
    pub fds: FdTable,
    pub user_heap: crate::user_heap::UserHeap,
    pub cwd: String,
    pub messages: MessageQueue,
    pub poll_fds: [u64; 64],
    pub poll_len: u32,
    /// PID whose user_heap to use for alloc/free. Self for processes, parent for threads.
    pub heap_owner: Pid,
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
    /// Multi-module TLS: (template_addr, filesz, memsz, base_offset) per module.
    pub tls_modules: Vec<(u64, usize, usize, usize)>,
    /// Total combined TLS size across all modules.
    pub tls_total_memsz: usize,
    // Crash diagnostics
    pub symbols: ProcessSymbols,
    // Process name (filename from argv[0], null-terminated)
    pub name: [u8; 28],
    // Dynamically loaded shared libraries (indexed by dlopen handle)
    pub loaded_libs: Vec<elf::LoadedLib>,
    // Anonymous memory mappings (mmap)
    pub mmap_regions: Vec<MmapRegion>,
    // User stack location (for SYS_STACK_INFO — works for both processes and threads)
    pub user_stack_base: u64,
    pub user_stack_size: u64,
}

impl Process {
    pub fn set_state(&mut self, new: ProcessState) {
        debug_assert!(self.state.can_transition_to(&new),
            "invalid state transition pid={}: {} -> {}", self.pid, self.state.name(), new.name());
        self.state = new;
    }

    fn zombify(&mut self, code: i32) {
        self.set_state(ProcessState::Zombie(code));
    }
}

pub struct MmapRegion {
    pub addr: u64,
    pub size: usize,
    pub alloc: OwnedAlloc,
}

pub struct ProcessTable {
    pub procs: IdMap<Pid, Process>,
}

impl ProcessTable {
    fn new() -> Self {
        Self { procs: IdMap::new() }
    }
}

pub static PROCESS_TABLE: Lock<Option<ProcessTable>> = Lock::new(None);
static NAME_REGISTRY: Lock<Option<HashMap<String, Pid>>> = Lock::new(None);

pub fn init() {
    *PROCESS_TABLE.lock() = Some(ProcessTable::new());
    *NAME_REGISTRY.lock() = Some(HashMap::new());
}

pub fn current_pid() -> Pid {
    percpu::current_pid().expect("current_pid() called during idle (no process running)")
}

/// Access the current process immutably. The process table lock is held for
/// the duration of the closure. The closure may acquire locks that come AFTER
/// process_table in the ordering (vfs, pipes, keyboard, device, allocator).
pub fn with_current<R>(f: impl FnOnce(&Process) -> R) -> R {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    let pid = current_pid();
    f(table.procs.get(pid).unwrap())
}

/// Access the current process mutably. Same lock ordering rules as with_current.
pub fn with_current_mut<R>(f: impl FnOnce(&mut Process) -> R) -> R {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = current_pid();
    f(table.procs.get_mut(pid).unwrap())
}

/// Access the fd table owner for the current thread/process.
/// Threads share their parent process's fd table.
pub fn with_fd_owner<R>(f: impl FnOnce(&Process) -> R) -> R {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    let pid = current_pid();
    let fd_pid = table.procs.get(pid).unwrap().heap_owner;
    f(table.procs.get(fd_pid).unwrap())
}

/// Access the fd table owner mutably for the current thread/process.
/// Threads share their parent process's fd table.
pub fn with_fd_owner_mut<R>(f: impl FnOnce(&mut Process) -> R) -> R {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = current_pid();
    let fd_pid = table.procs.get(pid).unwrap().heap_owner;
    f(table.procs.get_mut(fd_pid).unwrap())
}

/// Allocate a TLS area using the x86-64 variant II layout:
/// [TLS data (.tdata + .tbss)] [TCB: self-pointer]
///                              ^-- FS base (thread pointer)
/// Returns (alloc, fs_base).
pub fn setup_tls(tls_template: u64, tls_filesz: usize, tls_memsz: usize) -> Option<(OwnedAlloc, u64)> {
    setup_combined_tls(&[(tls_template, tls_filesz, tls_memsz, 0)], tls_memsz)
}

/// Allocate a combined TLS area for multiple modules (exe + shared libraries).
/// Each module's template is copied at its base_offset within the block.
/// Layout: [module0 TLS][module1 TLS]...[moduleN TLS][TCB: self-pointer]
///                                                    ^-- FS base (thread pointer)
pub fn setup_combined_tls(
    modules: &[(u64, usize, usize, usize)], // (template, filesz, memsz, base_offset)
    total_memsz: usize,
) -> Option<(OwnedAlloc, u64)> {
    let block_size = total_memsz + 8; // TLS data + self-pointer
    let alloc_size = paging::align_2m(block_size);
    let alloc = OwnedAlloc::new(alloc_size, PAGE_2M as usize)?;
    let block = alloc.ptr();

    // Place TLS data at the END of the allocation so dlopen can extend downward.
    // Layout: [unused padding] [TLS data (total_memsz)] [self-pointer (8 bytes)]
    let tls_start = alloc_size - block_size;

    // Copy each module's initialized data (.tdata) at its offset within the TLS area
    for &(template, filesz, _memsz, base_offset) in modules {
        if filesz > 0 && template != 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    template as *const u8,
                    block.add(tls_start + base_offset),
                    filesz,
                );
            }
        }
    }
    // .tbss already zeroed by alloc_zeroed

    // Thread pointer = address past TLS block
    let tp = block as u64 + (tls_start + total_memsz) as u64;
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
pub fn spawn_thread(entry: u64, stack_ptr: u64, arg: u64, stack_base: u64) -> Option<Pid> {
    // Read parent's TLS info (brief lock)
    let (tls_template, tls_filesz, tls_memsz, tls_modules, tls_total_memsz) = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();
        let parent = table.procs.get(current_pid()).unwrap();
        (parent.tls_template, parent.tls_filesz, parent.tls_memsz,
         parent.tls_modules.clone(), parent.tls_total_memsz)
    };

    // Allocate TLS for the thread (outside lock — map_user does TLB flush)
    let (tls_alloc, fs_base) = if !tls_modules.is_empty() {
        setup_combined_tls(&tls_modules, tls_total_memsz)?
    } else {
        setup_tls(tls_template, tls_filesz, tls_memsz)?
    };
    paging::map_user(tls_alloc.ptr() as u64, tls_alloc.size() as u64);

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(thread_start, entry, stack_ptr, arg) {
        Some(ks) => ks,
        None => {
            drop(tls_alloc);
            return None;
        }
    };

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let parent_pid = current_pid();
    let parent = table.procs.get(parent_pid).unwrap();
    let parent_cr3 = parent.cr3;
    let parent_heap_owner = parent.heap_owner;
    let parent_cwd = parent.cwd.clone();
    // Threads share the parent's fd table via with_fd_owner routing.
    // No per-thread fd table needed — all fd ops resolve through parent.
    let tid = table.procs.insert(Process {
        pid: Pid::from_raw(0),
        state: ProcessState::Ready,
        kind: Kind::Thread { parent: parent_pid },
        cr3: parent_cr3,
        kernel_stack: ks_alloc,
        kernel_rsp: ks_rsp,
        fds: FdTable::new(),
        user_heap: crate::user_heap::UserHeap::new(), // unused — routes through heap_owner
        messages: MessageQueue::new(),
        poll_fds: [0; 64],
        poll_len: 0,
        cwd: parent_cwd,
        heap_owner: parent_heap_owner,
        elf_alloc: None,
        stack_alloc: None,
        fs_base,
        tls_template,
        tls_filesz,
        tls_memsz,
        tls_alloc: Some(tls_alloc),
        tls_modules,
        tls_total_memsz,
        symbols: ProcessSymbols::empty(),
        name: [0; 28],
        loaded_libs: Vec::new(),
        mmap_regions: Vec::new(),
        user_stack_base: stack_base,
        user_stack_size: if stack_base > 0 { stack_ptr - stack_base } else { 0 },
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
    let table = guard.as_ref().unwrap();
    let pid = current_pid();
    let proc = table.procs.get(pid).unwrap();
    // Threads share parent's fd table, so resolve through fd owner
    let fd_pid = match proc.kind {
        Kind::Thread { parent } => parent,
        Kind::Process { .. } => pid,
    };
    let fd_owner = table.procs.get(fd_pid).unwrap();
    let mut fds = FdTable::new();
    for &[child_fd, parent_fd] in pairs {
        if let Some(desc) = fd_owner.fds.get(parent_fd as u64) {
            fds.insert_at(child_fd as u64, fd::dup(desc));
        }
    }
    fds
}

/// Spawn a new process from an ELF binary.
pub fn spawn(argv: &[&str], fds: FdTable, parent: Option<Pid>) -> Option<Pid> {
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
    let child_cr3 = Some(PageTableRoot::new(child_pml4));

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
    let stack_base = stack_alloc.ptr() as u64;
    let stack_top = stack_base + USER_STACK_SIZE as u64;
    paging::map_user_in(child_pml4, stack_base, USER_STACK_SIZE as u64);

    // Build combined TLS layout: libraries first, exe last (right before TP).
    // The exe must be last because its inline tpoff values are baked in at link
    // time assuming the exe's TLS ends at the TP. Placing it last preserves those values.
    let mut tls_modules: Vec<(u64, usize, usize, usize)> = Vec::new();
    let mut tls_cursor = 0usize;

    for lib in &loaded_libs {
        if lib.tls_memsz > 0 {
            if tls_cursor > 0 {
                tls_cursor = (tls_cursor + 15) & !15;
            }
            tls_modules.push((lib.tls_template, lib.tls_filesz, lib.tls_memsz, tls_cursor));
            tls_cursor += lib.tls_memsz;
        }
    }
    if loaded.tls_memsz > 0 {
        if tls_cursor > 0 {
            tls_cursor = (tls_cursor + 15) & !15;
        }
        let exe_base_offset = tls_cursor;
        tls_modules.push((loaded.tls_template, loaded.tls_filesz, loaded.tls_memsz, exe_base_offset));
        tls_cursor += loaded.tls_memsz;
    }
    let tls_total_memsz = tls_cursor;

    // Apply R_X86_64_TPOFF64 relocations in shared libraries now that we know the layout
    let tls_info = elf::TlsModuleInfo { libs: &loaded_libs, modules: &tls_modules };
    for lib in &loaded_libs {
        // Find this library's base offset in the combined layout
        let lib_base_offset = tls_modules.iter()
            .find(|&&(template, _, _, _)| template == lib.tls_template)
            .map(|&(_, _, _, base_offset)| base_offset)
            .unwrap_or(0);
        elf::apply_tpoff_relocs(lib, lib_base_offset, tls_total_memsz, &tls_info);
    }
    // Also apply TPOFF relocs in the exe's relocations (local or cross-lib TLS)
    {
        let exe_base_offset = tls_modules.iter()
            .find(|&&(template, _, _, _)| template == loaded.tls_template)
            .map(|&(_, _, _, base_offset)| base_offset)
            .unwrap_or(0);
        elf::apply_exe_tpoff_relocs(&binary, loaded.base, exe_base_offset, tls_total_memsz, &tls_info);
    }

    // For single-module compat (used by spawn_thread)
    let (tls_template, tls_filesz, tls_memsz) = if !tls_modules.is_empty() {
        (tls_modules[0].0, tls_modules[0].1, tls_modules[0].2)
    } else {
        (0, 0, 0)
    };

    log!("spawn: TLS {} modules, total_memsz={}", tls_modules.len(), tls_total_memsz);
    let (tls_alloc, fs_base) = if tls_total_memsz > 0 {
        setup_combined_tls(&tls_modules, tls_total_memsz)?
    } else {
        setup_tls(0, 0, 0)?
    };
    paging::map_user_in(child_pml4, tls_alloc.ptr() as u64, tls_alloc.size() as u64);

    let sp = write_argv_to_stack(stack_top, argv);

    let syms = ProcessSymbols::parse(
        &binary, loaded.base,
        elf_alloc.ptr() as u64, elf_alloc.ptr() as u64 + elf_alloc.size() as u64,
        stack_base, stack_top,
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
    let table = guard.as_mut().unwrap();

    let cwd = match parent {
        Some(ppid) => table.procs.get(ppid).unwrap().cwd.clone(),
        None => String::from("/"),
    };

    let pid = table.procs.insert(Process {
        pid: Pid::from_raw(0),
        state: ProcessState::Ready,
        kind: Kind::Process { parent },
        cr3: child_cr3,
        kernel_stack: ks_alloc,
        kernel_rsp: ks_rsp,
        fds,
        user_heap: crate::user_heap::UserHeap::new(),
        messages: MessageQueue::new(),
        poll_fds: [0; 64],
        poll_len: 0,
        cwd,
        heap_owner: Pid::from_raw(0),
        elf_alloc: Some(elf_alloc),
        stack_alloc: Some(stack_alloc),
        fs_base,
        tls_template,
        tls_filesz,
        tls_memsz,
        tls_alloc: Some(tls_alloc),
        tls_modules,
        tls_total_memsz,
        symbols: syms,
        name: make_name(path),
        loaded_libs,
        mmap_regions: Vec::new(),
        user_stack_base: stack_base,
        user_stack_size: USER_STACK_SIZE as u64,
    });
    let p = table.procs.get_mut(pid).unwrap();
    p.pid = pid;
    p.heap_owner = pid;

    log!("spawn: {} pid={} base={:#x} entry={:#x} cr3={:#x} ks={:#x}..{:#x}",
        path, pid, loaded.base as u64, loaded.entry, child_cr3.unwrap().as_u64(),
        ks_base, ks_base + KERNEL_STACK_SIZE as u64);

    Some(pid)
}

/// Spawn a process from kernel context (during boot). Resolves bare names
/// to `/initrd/<name>`. Panics on failure.
pub fn spawn_kernel(argv: &[&str]) -> Pid {
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

/// Like `spawn_kernel`, but returns `None` instead of panicking if the binary
/// is missing. Used for optional services that may not be present in the initrd.
pub fn spawn_optional(argv: &[&str]) -> Option<Pid> {
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
    spawn(&full_argv, fds, None)
}

/// Tear down a process: zombie all its threads, free all resources, wake parent.
/// Caller must hold PROCESS_TABLE lock and have already switched to kernel CR3.
fn teardown_process(table: &mut ProcessTable, pid: Pid, code: i32) {
    // Kill all child threads before freeing resources they depend on
    let child_tids: Vec<Pid> = table.procs.iter()
        .filter(|(tid, p)| *tid != pid && matches!(p.kind, Kind::Thread { parent } if parent == pid))
        .map(|(tid, _)| tid)
        .collect();
    for tid in &child_tids {
        let child = table.procs.get_mut(*tid).unwrap();
        child.tls_alloc.take();
        if !matches!(child.state, ProcessState::Zombie(_)) {
            child.zombify(-1);
        }
    }

    let proc = table.procs.get_mut(pid).unwrap();
    fd::close_all(&mut proc.fds, &mut *vfs::lock(), pid);
    proc.tls_alloc.take();
    let root = proc.cr3.take().expect("teardown_process: cr3 already taken");
    let pml4 = root.as_ptr();
    shared_memory::cleanup_process(pid, pml4);
    proc.elf_alloc.take();
    proc.stack_alloc.take();
    proc.loaded_libs.clear();
    proc.mmap_regions.clear();
    paging::free_user_page_tables(pml4);
    proc.zombify(code);
    let name = core::str::from_utf8(&proc.name).unwrap_or("?").trim_end_matches('\0');
    log!("exit: {name} pid={pid} code={code}");

    if let Some(names) = NAME_REGISTRY.lock().as_mut() { names.retain(|_, &mut v| v != pid); }

    // Wake parent if blocked on waitpid
    if let Kind::Process { parent: Some(ppid) } = table.procs.get(pid).unwrap().kind {
        if let Some(p) = table.procs.get_mut(ppid) {
            if let ProcessState::BlockedWaitPid(child) = p.state {
                if child == pid {
                    p.set_state(ProcessState::Ready);
                }
            }
        }
    }
}

/// Exit the entire process (all threads). If called from a thread, kills the
/// parent process and all siblings.
pub fn exit(code: i32) -> ! {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = current_pid();
    let kind = table.procs.get(pid).unwrap().kind;

    unsafe { cpu::write_cr3(paging::kernel_cr3()); }

    let process_pid = match kind {
        Kind::Thread { parent } => {
            // Zombie ourselves, then kill the parent process (cascades to siblings)
            let proc = table.procs.get_mut(pid).unwrap();
            proc.tls_alloc.take();
            proc.zombify(code);
            parent
        }
        Kind::Process { .. } => pid,
    };

    teardown_process(table, process_pid, code);
    scheduler::schedule_no_return_locked(guard);
}

/// Exit the current thread only. For processes without threads, tears down
/// the process. For threads, zombifies without freeing the address space.
pub fn thread_exit(code: i32) -> ! {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = current_pid();
    let kind = table.procs.get(pid).unwrap().kind;

    unsafe { cpu::write_cr3(paging::kernel_cr3()); }

    match kind {
        Kind::Thread { parent } => {
            // Thread exit: zombie ourselves, wake parent for thread_join.
            // Don't free address space (shared with parent).
            let proc = table.procs.get_mut(pid).unwrap();
            fd::close_all(&mut proc.fds, &mut *vfs::lock(), pid);
            proc.tls_alloc.take();
            proc.zombify(code);
            let name = core::str::from_utf8(&proc.name).unwrap_or("?").trim_end_matches('\0');
            log!("exit: {name} pid={pid} code={code}");

            if let Some(p) = table.procs.get_mut(parent) {
                if let ProcessState::BlockedThreadJoin(child) = p.state {
                    if child == pid {
                        p.set_state(ProcessState::Ready);
                    }
                }
            }
        }
        Kind::Process { .. } => {
            teardown_process(table, pid, code);
        }
    }

    scheduler::schedule_no_return_locked(guard);
}

/// Block the current process and switch to the next ready one.
pub fn block(reason: ProcessState) {
    scheduler::block(reason);
}

pub fn block_poll(fds: [u64; 64], len: u32, deadline: u64) {
    debug_assert!(len <= 64, "poll_len {} exceeds array size", len);
    {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let proc = table.procs.get_mut(current_pid()).unwrap();
        proc.poll_fds = fds;
        proc.poll_len = len;
    }
    scheduler::block(ProcessState::BlockedPoll { deadline });
}

/// Cooperative yield: mark current as Ready, switch to next.
pub fn yield_now() {
    scheduler::yield_now();
}

/// Send a message to a target process. Wakes the target if blocked.
pub fn send_message(target_pid: Pid, msg: crate::message::Message) -> bool {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    if let Some(proc) = table.procs.get_mut(target_pid) {
        if !proc.messages.push(msg) {
            return false;
        }
        match proc.state {
            ProcessState::BlockedRecvMsg | ProcessState::BlockedPoll { .. } => {
                proc.set_state(ProcessState::Ready);
            }
            _ => {}
        }
        true
    } else {
        false
    }
}

/// Atomically check a user futex word and block if it matches the expected value.
/// Returns 0 if woken normally, 1 if timed out (handled by idle_poll), u64::MAX on error.
/// The check-and-block is atomic w.r.t. futex_wake because both hold the process table lock.
pub fn futex_wait(addr: u64, expected: u32, timeout_ns: u64) -> u64 {
    let deadline = if timeout_ns != u64::MAX {
        crate::clock::nanos_since_boot().saturating_add(timeout_ns)
    } else {
        0
    };
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = current_pid();
    let proc = table.procs.get_mut(pid).unwrap();

    // Atomic check: read the user value under the lock
    let current = unsafe { core::ptr::read_volatile(addr as *const u32) };
    if current != expected {
        return 0;
    }

    proc.set_state(ProcessState::BlockedFutex { addr, deadline });
    // Pass the held lock to the scheduler so it can context-switch atomically
    scheduler::schedule_already_blocked(guard);
    0
}

/// Wake up to `count` threads blocked on `addr` in the same address space as the caller.
pub fn futex_wake(addr: u64, count: u64) -> u64 {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = current_pid();
    let caller_cr3 = table.procs.get(pid).and_then(|p| p.cr3);
    let mut woken = 0u64;
    for (_, proc) in table.procs.iter_mut() {
        if woken >= count { break; }
        if caller_cr3.is_some() && proc.cr3 == caller_cr3 {
            if let ProcessState::BlockedFutex { addr: a, .. } = proc.state {
                if a == addr {
                    proc.set_state(ProcessState::Ready);
                    woken += 1;
                }
            }
        }
    }
    woken
}

/// Wake processes blocked on reading from a pipe that now has data.
pub fn wake_pipe_readers(pipe_id: pipe::PipeId) {
    scheduler::wake_pipe_readers(pipe_id);
}

/// Wake processes blocked on writing to a pipe that now has space.
pub fn wake_pipe_writers(pipe_id: pipe::PipeId) {
    scheduler::wake_pipe_writers(pipe_id);
}

/// Atomically validate parent-child relationship and collect a zombie child process.
/// Returns Ok(Some(code)) if collected, Ok(None) if child exists but not zombie,
/// Err(()) if child doesn't exist or isn't ours.
/// Combining validation and collection under one lock prevents TOCTOU races.
pub fn collect_child_zombie(child_pid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let proc = table.procs.get(child_pid).ok_or(())?;
    if !matches!(proc.kind, Kind::Process { parent: Some(ppid) } if ppid == parent_pid) {
        return Err(());
    }
    if let ProcessState::Zombie(code) = proc.state {
        table.procs.remove(child_pid);
        Ok(Some(code))
    } else {
        Ok(None)
    }
}

/// Atomically validate parent-thread relationship and collect a zombie thread.
/// Same atomic guarantees as `collect_child_zombie`.
pub fn collect_thread_zombie(tid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let proc = table.procs.get(tid).ok_or(())?;
    if !matches!(proc.kind, Kind::Thread { parent } if parent == parent_pid) {
        return Err(());
    }
    if let ProcessState::Zombie(code) = proc.state {
        table.procs.remove(tid);
        Ok(Some(code))
    } else {
        Ok(None)
    }
}

pub fn register_name(name: &str, pid: Pid) -> bool {
    let mut guard = NAME_REGISTRY.lock();
    let names = guard.as_mut().unwrap();
    if names.contains_key(name) {
        return false;
    }
    names.insert(String::from(name), pid);
    true
}

pub fn find_pid(name: &str) -> Option<Pid> {
    NAME_REGISTRY.lock().as_ref().unwrap().get(name).copied()
}

/// Resolve an address using the current process's symbols. Lock-safe (brief hold).
pub fn resolve_symbol(addr: u64) -> Option<(String, u64)> {
    let pid = current_pid();
    if pid == Pid::MAX { return None; }
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref()?;
    table.procs.get(pid)?.symbols.resolve(addr)
}

/// Check if an address is in the current process's valid memory ranges.
pub fn is_valid_user_addr(addr: u64) -> bool {
    let pid = current_pid();
    if pid == Pid::MAX { return false; }
    let guard = PROCESS_TABLE.lock();
    let Some(table) = guard.as_ref() else { return false };
    match table.procs.get(pid) {
        Some(proc) => proc.symbols.is_valid_user_addr(addr),
        None => false,
    }
}

/// Kill a child process. Only the parent can kill its children.
/// Returns 0 on success, error code on failure.
pub fn kill_process(target_pid: Pid) -> u64 {
    use toyos_abi::syscall::SyscallError;
    let caller = current_pid();
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();

    let Some(proc) = table.procs.get(target_pid) else { return SyscallError::NotFound.to_u64() };

    // Only allow killing our own children
    if !matches!(proc.kind, Kind::Process { parent: Some(ppid) } if ppid == caller) {
        return SyscallError::PermissionDenied.to_u64();
    }
    // Can't kill a process currently Running on another CPU
    if proc.state == ProcessState::Running {
        return SyscallError::WouldBlock.to_u64();
    }
    // Already a zombie — nothing to do
    if matches!(proc.state, ProcessState::Zombie(_)) {
        return 0;
    }

    // Clean up the target's resources
    let proc = table.procs.get_mut(target_pid).unwrap();
    fd::close_all(&mut proc.fds, &mut *vfs::lock(), target_pid);
    proc.tls_alloc.take();

    // Kill child threads of the target
    let child_tids: Vec<Pid> = table.procs.iter()
        .filter(|(tid, p)| *tid != target_pid && matches!(p.kind, Kind::Thread { parent } if parent == target_pid))
        .map(|(tid, _)| tid)
        .collect();
    for tid in &child_tids {
        let child = table.procs.get_mut(*tid).unwrap();
        child.tls_alloc.take();
        if !matches!(child.state, ProcessState::Zombie(_)) {
            child.zombify(-1);
        }
    }

    let proc = table.procs.get_mut(target_pid).unwrap();
    let root = proc.cr3.take().expect("kill_process: cr3 already taken");
    let pml4 = root.as_ptr();
    shared_memory::cleanup_process(target_pid, pml4);
    proc.elf_alloc.take();
    proc.stack_alloc.take();
    proc.loaded_libs.clear();
    proc.mmap_regions.clear();
    paging::free_user_page_tables(pml4);

    proc.zombify(137); // 128 + 9 (SIGKILL-like)
    let name = core::str::from_utf8(&proc.name).unwrap_or("?").trim_end_matches('\0');
    log!("kill: {name} pid={target_pid}");

    // Unregister name
    if let Some(names) = NAME_REGISTRY.lock().as_mut() { names.retain(|_, &mut v| v != target_pid); }

    // Wake parent if blocked on waitpid for this process
    if let Some(parent) = table.procs.get_mut(caller) {
        if let ProcessState::BlockedWaitPid(child) = parent.state {
            if child == target_pid {
                parent.set_state(ProcessState::Ready);
            }
        }
    }

    0
}

/// AP entry into the scheduler. Called from smp::ap_entry after SMP_READY.
pub fn ap_idle() -> ! {
    scheduler::schedule_no_return();
}

