use alloc::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::sync::Arc;
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

pub use toyos_abi::Pid;

impl crate::id_map::IdKey for Pid {
    const ZERO: Self = Pid(0);
    const ONE: Self = Pid(1);
}

// ---------------------------------------------------------------------------
// PageTableRoot — type-safe PML4 pointer (prevents double-free via Option::take)
// ---------------------------------------------------------------------------

/// Physical address of a PML4 page table. Used as `Option<PageTableRoot>` in SchedEntry
/// so that `take()` makes double-free of page tables impossible at compile time.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PageTableRoot(NonNull<u64>);

// SAFETY: PageTableRoot points to a PML4 page table in physical memory.
// Page tables are not tied to any specific thread — they are hardware structures.
unsafe impl Send for PageTableRoot {}

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

    /// Allocate uninitialized memory with the given size and alignment.
    /// The caller must initialize all bytes before reading them.
    pub fn new_uninit(size: usize, align: usize) -> Option<Self> {
        let layout = Layout::from_size_align(size, align).ok()?;
        let ptr = NonNull::new(unsafe { alloc(layout) })?;
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
pub const KERNEL_STACK_SIZE: usize = 128 * 1024;

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
    BlockedFutex { phys_addr: u64, deadline: u64 },
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

// ---------------------------------------------------------------------------
// SchedEntry — scheduling metadata under the global PROCESS_TABLE lock
// ---------------------------------------------------------------------------

/// Scheduling metadata protected by the global PROCESS_TABLE lock.
/// All fields are private — access only through getters and state-machine methods.
/// This prevents accidental drops of `kernel_stack` (use-after-free) and
/// invalid state transitions.
pub struct SchedEntry {
    pid: Pid,
    state: ProcessState,
    kind: Kind,
    /// Per-process page table (physical address of PML4).
    /// Option so teardown can take() it, making double-free impossible.
    cr3: Option<PageTableRoot>,
    /// Kernel stack allocation. Dropping this frees the stack — only safe when
    /// the process is NOT running on it (i.e. from the idle loop).
    kernel_stack: OwnedAlloc,
    kernel_rsp: u64,
    fs_base: u64,
    /// PID whose ProcessData to use for FD/heap ops. Self for processes, parent for threads.
    heap_owner: Pid,
    name: [u8; 28],
    /// Snapshot of allocated memory size (elf + stack) for sysinfo reporting.
    memory_size: u64,
    /// Per-process data behind its own lock. Syscalls clone this Arc, drop the
    /// table lock, then lock ProcessData — so different processes never contend.
    data: Arc<Lock<ProcessData>>,
}

impl SchedEntry {
    fn new(
        pid: Pid,
        state: ProcessState,
        kind: Kind,
        cr3: Option<PageTableRoot>,
        kernel_stack: OwnedAlloc,
        kernel_rsp: u64,
        fs_base: u64,
        heap_owner: Pid,
        name: [u8; 28],
        memory_size: u64,
        data: Arc<Lock<ProcessData>>,
    ) -> Self {
        Self { pid, state, kind, cr3, kernel_stack, kernel_rsp, fs_base, heap_owner, name, memory_size, data }
    }

    // --- Read-only getters ---

    pub fn pid(&self) -> Pid { self.pid }
    pub fn state(&self) -> &ProcessState { &self.state }
    pub fn kind(&self) -> &Kind { &self.kind }
    pub fn heap_owner(&self) -> Pid { self.heap_owner }
    pub fn name(&self) -> &[u8; 28] { &self.name }
    pub fn memory_size(&self) -> u64 { self.memory_size }
    pub fn data(&self) -> &Arc<Lock<ProcessData>> { &self.data }
    pub fn cr3(&self) -> Option<PageTableRoot> { self.cr3 }
    pub fn fs_base(&self) -> u64 { self.fs_base }

    // --- Scheduler-only accessors ---

    pub fn kernel_stack_top(&self) -> u64 {
        self.kernel_stack.ptr() as u64 + KERNEL_STACK_SIZE as u64
    }
    pub fn kernel_rsp(&self) -> u64 { self.kernel_rsp }
    pub fn kernel_rsp_mut(&mut self) -> &mut u64 { &mut self.kernel_rsp }
    pub fn set_fs_base(&mut self, val: u64) { self.fs_base = val; }

    // --- State machine ---

    pub fn set_state(&mut self, new: ProcessState) {
        assert!(self.state.can_transition_to(&new),
            "invalid state transition pid={}: {} -> {}", self.pid, self.state.name(), new.name());
        self.state = new;
    }

    fn zombify(&mut self, code: i32) {
        self.set_state(ProcessState::Zombie(code));
    }

    /// Consume the page table root. Panics if already taken.
    pub fn take_cr3(&mut self) -> PageTableRoot {
        self.cr3.take().expect("take_cr3: already taken")
    }

    /// Detach this process from its parent (orphan it).
    pub fn detach_from_parent(&mut self) {
        assert!(matches!(self.kind, Kind::Process { parent: Some(_) }),
            "detach_from_parent: pid={} is not a parented process", self.pid);
        self.kind = Kind::Process { parent: None };
    }
}

// ---------------------------------------------------------------------------
// ProcessData — per-process data behind Arc<Lock<ProcessData>>
// ---------------------------------------------------------------------------

/// Per-process data independently lockable via `Arc<Lock<ProcessData>>`.
/// Syscalls clone the Arc from SchedEntry, drop the table lock, then lock this.
pub struct ProcessData {
    pub pid: Pid,
    pub fds: FdTable,
    pub user_heap: crate::user_heap::UserHeap,
    pub cwd: String,
    pub messages: MessageQueue,
    pub poll_fds: [u64; 64],
    pub poll_len: u32,
    pub elf_alloc: Option<OwnedAlloc>,
    pub stack_alloc: Option<OwnedAlloc>,
    // Thread-local storage
    pub tls_template: u64,
    pub tls_filesz: usize,
    pub tls_memsz: usize,
    pub tls_alloc: Option<OwnedAlloc>,
    /// Multi-module TLS: (template_addr, filesz, memsz, base_offset) per module.
    pub tls_modules: Vec<(u64, usize, usize, usize)>,
    /// Total combined TLS size across all modules.
    pub tls_total_memsz: usize,
    // Crash diagnostics
    pub symbols: ProcessSymbols,
    // Dynamically loaded shared libraries (indexed by dlopen handle)
    pub loaded_libs: Vec<elf::LoadedLib>,
    // Anonymous memory mappings (mmap)
    pub mmap_regions: Vec<MmapRegion>,
    // User stack location (for SYS_STACK_INFO)
    pub user_stack_base: u64,
    pub user_stack_size: u64,
    /// Inherited environment variables (KEY=VALUE\0KEY2=VALUE2\0...)
    pub env: Vec<u8>,
    /// Syscall counts per syscall number (for profiling)
    pub syscall_counts: [u32; 64],
    pub syscall_total: u64,
}

pub struct MmapRegion {
    pub addr: u64,
    pub size: usize,
    pub alloc: OwnedAlloc,
    /// True if this is a MAP_FIXED mapping (virt addr != phys addr).
    pub fixed: bool,
}

// ---------------------------------------------------------------------------
// IdleProof — zero-cost proof that code runs on the per-CPU idle stack
// ---------------------------------------------------------------------------

/// Zero-sized proof that we are on the per-CPU idle stack.
/// Required by `ProcessTable::collect_orphan_zombies` to prevent calling it
/// from a process's kernel stack (which would be use-after-free if we drop
/// the SchedEntry we're running on).
#[derive(Clone, Copy)]
pub struct IdleProof(());

impl IdleProof {
    /// Only call from `cpu_idle_loop` (which runs on the per-CPU idle stack).
    ///
    /// # Safety
    /// Caller must actually be running on the idle stack.
    pub(crate) unsafe fn new_unchecked() -> Self { Self(()) }
}

// ---------------------------------------------------------------------------
// Process table
// ---------------------------------------------------------------------------

pub struct ProcessTable {
    procs: IdMap<Pid, SchedEntry>,
}

impl ProcessTable {
    fn new() -> Self {
        Self { procs: IdMap::new() }
    }

    // --- Passthrough accessors ---

    pub fn insert_with(&mut self, f: impl FnOnce(Pid) -> SchedEntry) -> Pid {
        self.procs.insert_with(f)
    }

    pub fn get(&self, pid: Pid) -> Option<&SchedEntry> {
        self.procs.get(pid)
    }

    pub fn get_mut(&mut self, pid: Pid) -> Option<&mut SchedEntry> {
        self.procs.get_mut(pid)
    }

    pub fn iter(&self) -> impl Iterator<Item = (Pid, &SchedEntry)> {
        self.procs.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Pid, &mut SchedEntry)> {
        self.procs.iter_mut()
    }

    // --- Safe removal methods (each validates preconditions) ---

    /// Waitpid: collect a zombie child process. Validates parent relationship.
    pub fn collect_child_zombie(&mut self, child_pid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
        let entry = self.procs.get(child_pid).ok_or(())?;
        if !matches!(entry.kind, Kind::Process { parent: Some(ppid) } if ppid == parent_pid) {
            return Err(());
        }
        if let ProcessState::Zombie(code) = entry.state {
            self.procs.remove(child_pid);
            Ok(Some(code))
        } else {
            Ok(None)
        }
    }

    /// Thread join: collect a zombie thread. Validates parent relationship.
    pub fn collect_thread_zombie(&mut self, tid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
        let entry = self.procs.get(tid).ok_or(())?;
        if !matches!(entry.kind, Kind::Thread { parent } if parent == parent_pid) {
            return Err(());
        }
        if let ProcessState::Zombie(code) = entry.state {
            self.procs.remove(tid);
            Ok(Some(code))
        } else {
            Ok(None)
        }
    }

    /// Remove a zombie orphan child during parent teardown.
    /// Asserts the child is a zombie (safe to drop kernel stack since zombies aren't running).
    fn remove_orphan_zombie_child(&mut self, child_pid: Pid) {
        let entry = self.procs.get(child_pid).expect("remove_orphan_zombie_child: child not found");
        assert!(matches!(entry.state, ProcessState::Zombie(_)),
            "remove_orphan_zombie_child: pid={} is not a zombie (state={})", child_pid, entry.state.name());
        self.procs.remove(child_pid);
    }

    /// Sweep all reclaimable zombies. Requires `IdleProof` — only callable from
    /// the idle loop, which runs on the per-CPU idle stack (safe to drop kernel stacks).
    ///
    /// Collects:
    /// - Parentless zombie processes (orphans)
    /// - Zombie threads whose parent process is zombie or gone
    pub fn collect_orphan_zombies(&mut self, _proof: IdleProof) {
        // First pass: find zombie parent pids (for thread collection)
        let zombie_pids: Vec<Pid> = self.procs.iter()
            .filter(|(_, e)| matches!(e.state, ProcessState::Zombie(_)))
            .map(|(pid, _)| pid)
            .collect();

        // Second pass: collect reclaimable zombies
        let orphans: Vec<Pid> = self.procs.iter()
            .filter(|(_, e)| {
                if !matches!(e.state, ProcessState::Zombie(_)) {
                    return false;
                }
                match e.kind {
                    Kind::Process { parent: None } => true,
                    Kind::Thread { parent } => zombie_pids.contains(&parent),
                    _ => false,
                }
            })
            .map(|(pid, _)| pid)
            .collect();
        for pid in orphans {
            self.procs.remove(pid);
        }
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

// ---------------------------------------------------------------------------
// Access patterns — SchedEntry (brief table lock)
// ---------------------------------------------------------------------------

/// Access the current process's SchedEntry immutably (table lock held for closure).
pub fn with_current_sched<R>(f: impl FnOnce(&SchedEntry) -> R) -> R {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    f(table.get(current_pid()).unwrap())
}

/// Access the current process's SchedEntry mutably (table lock held for closure).
pub fn with_current_sched_mut<R>(f: impl FnOnce(&mut SchedEntry) -> R) -> R {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    f(table.get_mut(current_pid()).unwrap())
}

// ---------------------------------------------------------------------------
// Access patterns — ProcessData (clone Arc, drop table lock, lock ProcessData)
// ---------------------------------------------------------------------------

/// Get the current process's ProcessData Arc (brief table lock).
pub fn current_data() -> Arc<Lock<ProcessData>> {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    Arc::clone(table.get(current_pid()).unwrap().data())
}

/// Get the FD/heap owner's ProcessData Arc (brief table lock).
/// For processes this is self; for threads it's the parent process.
pub fn fd_owner_data() -> Arc<Lock<ProcessData>> {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    let pid = current_pid();
    let owner_pid = table.get(pid).unwrap().heap_owner();
    Arc::clone(table.get(owner_pid).unwrap().data())
}

/// Access the current process's ProcessData mutably.
/// Table lock is NOT held during the closure — only the per-process lock.
pub fn with_current_data<R>(f: impl FnOnce(&mut ProcessData) -> R) -> R {
    let arc = current_data();
    let mut guard = arc.lock();
    f(&mut guard)
}

/// Access the FD/heap owner's ProcessData mutably.
/// Table lock is NOT held during the closure — only the per-process lock.
pub fn with_fd_owner_data<R>(f: impl FnOnce(&mut ProcessData) -> R) -> R {
    let arc = fd_owner_data();
    let mut guard = arc.lock();
    f(&mut guard)
}

// ---------------------------------------------------------------------------
// TLS setup
// ---------------------------------------------------------------------------

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
    let tls_start = alloc_size - block_size;

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

    let tp = block as u64 + (tls_start + total_memsz) as u64;
    unsafe { *(tp as *mut u64) = tp; }

    Some((alloc, tp))
}

// ---------------------------------------------------------------------------
// Kernel stack allocation
// ---------------------------------------------------------------------------

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
        "push r12",
        "push r13",
        "call {unlock}",
        "pop r13",
        "pop r12",
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

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawn a thread within the current process.
pub fn spawn_thread(entry: u64, stack_ptr: u64, arg: u64, stack_base: u64) -> Option<Pid> {
    // Phase 1: Get parent's SchedEntry data + ProcessData (never held simultaneously)
    let (parent_cr3, parent_heap_owner, data_arc) = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();
        let parent = table.get(current_pid()).unwrap();
        (parent.cr3(), parent.heap_owner(), Arc::clone(parent.data()))
    };
    let (tls_template, tls_filesz, tls_memsz, tls_modules, tls_total_memsz, parent_cwd) = {
        let data = data_arc.lock();
        (data.tls_template, data.tls_filesz, data.tls_memsz,
         data.tls_modules.clone(), data.tls_total_memsz, data.cwd.clone())
    };

    // Phase 2: Allocate TLS (outside any lock — map_user does TLB flush)
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

    // Phase 3: Insert into table (brief table lock)
    let thread_data = Arc::new(Lock::new(ProcessData {
        pid: Pid::from_raw(0),
        fds: FdTable::new(),
        user_heap: crate::user_heap::UserHeap::new(), // unused — routes through heap_owner
        messages: MessageQueue::new(),
        poll_fds: [0; 64],
        poll_len: 0,
        cwd: parent_cwd,
        elf_alloc: None,
        stack_alloc: None,
        tls_template,
        tls_filesz,
        tls_memsz,
        tls_alloc: Some(tls_alloc),
        tls_modules,
        tls_total_memsz,
        symbols: ProcessSymbols::empty(),
        loaded_libs: Vec::new(),
        mmap_regions: Vec::new(),
        user_stack_base: stack_base,
        user_stack_size: if stack_base > 0 { stack_ptr - stack_base } else { 0 },
        env: Vec::new(),
        syscall_counts: [0; 64],
        syscall_total: 0,
    }));

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let parent_pid = current_pid();
    let tid = table.insert_with(|tid| {
        thread_data.lock().pid = tid;
        SchedEntry::new(
            tid, ProcessState::Ready, Kind::Thread { parent: parent_pid },
            parent_cr3, ks_alloc, ks_rsp, fs_base, parent_heap_owner,
            [0; 28], 0, thread_data,
        )
    });

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
    let data_arc = fd_owner_data();
    let data = data_arc.lock();
    let mut fds = FdTable::new();
    for &[child_fd, parent_fd] in pairs {
        if let Some(desc) = data.fds.get(parent_fd) {
            fds.insert_at(child_fd, fd::dup(desc));
        }
    }
    fds
}

/// Spawn a new process from an ELF binary.
pub fn spawn(argv: &[&str], fds: FdTable, parent: Option<Pid>, env: Vec<u8>) -> Option<Pid> {
    let path = argv[0];
    let t0 = crate::clock::nanos_since_boot();

    let binary = match vfs::lock().read_file(path) {
        Ok(data) => data,
        Err(e) => {
            log!("{}: {}", path, e);
            return None;
        }
    };
    let t1 = crate::clock::nanos_since_boot();

    let (elf_alloc, loaded) = match elf::load(&binary) {
        Ok(l) => l,
        Err(msg) => {
            log!("{}", msg);
            return None;
        }
    };
    let t2 = crate::clock::nanos_since_boot();

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

    let t_deps = crate::clock::nanos_since_boot();

    let child_pml4 = paging::create_user_pml4();
    let child_cr3 = Some(PageTableRoot::new(child_pml4));

    paging::map_user_in(child_pml4, elf_alloc.ptr() as u64, elf_alloc.size() as u64);

    for lib in &loaded_libs {
        match &lib.memory {
            elf::LibMemory::Owned(alloc) => {
                paging::map_user_in(child_pml4, alloc.ptr() as u64, alloc.size() as u64);
            }
            elf::LibMemory::Shared { rw_alloc, shared_addr, shared_size, total_cached_size, .. } => {
                // Map entire cached range as user-readable (code + rodata)
                paging::map_user_readonly_in(child_pml4, *shared_addr, *total_cached_size as u64);
                // Remap private RW pages over the shared range
                let num_rw_pages = paging::align_2m(rw_alloc.size()) / paging::PAGE_2M as usize;
                for i in 0..num_rw_pages {
                    let virt = *shared_addr + *shared_size as u64 + i as u64 * paging::PAGE_2M;
                    let phys = rw_alloc.ptr() as u64 + i as u64 * paging::PAGE_2M;
                    paging::remap_user_2m_in(child_pml4, virt, phys);
                }
            }
        }
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

    let tls_info = elf::TlsModuleInfo { libs: &loaded_libs, modules: &tls_modules };
    for lib in &loaded_libs {
        let lib_base_offset = tls_modules.iter()
            .find(|&&(template, _, _, _)| template == lib.tls_template)
            .map(|&(_, _, _, base_offset)| base_offset)
            .unwrap_or(0);
        elf::apply_tpoff_relocs(lib, lib_base_offset, tls_total_memsz, &tls_info);
    }
    {
        let exe_base_offset = tls_modules.iter()
            .find(|&&(template, _, _, _)| template == loaded.tls_template)
            .map(|&(_, _, _, base_offset)| base_offset)
            .unwrap_or(0);
        elf::apply_exe_tpoff_relocs(&binary, loaded.base, exe_base_offset, tls_total_memsz, &tls_info);
    }

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

    let t_tls = crate::clock::nanos_since_boot();
    // Skip full symbol parsing for large binaries (>5MB) — too slow for debug info
    let syms = if binary.len() > 5 * 1024 * 1024 {
        ProcessSymbols::empty_with_bounds(
            elf_alloc.ptr() as u64, elf_alloc.ptr() as u64 + elf_alloc.size() as u64,
            stack_base, stack_top,
        )
    } else {
        ProcessSymbols::parse(
            &binary, loaded.base,
            elf_alloc.ptr() as u64, elf_alloc.ptr() as u64 + elf_alloc.size() as u64,
            stack_base, stack_top,
        )
    };
    let t_syms = crate::clock::nanos_since_boot();

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(process_start, loaded.entry, sp, 0) {
        Some(ks) => ks,
        None => {
            paging::free_user_page_tables(child_pml4);
            return None;
        }
    };

    let ks_base = ks_alloc.ptr() as u64;

    // Get parent's cwd (brief table lock + ProcessData lock, before insertion)
    let cwd = match parent {
        Some(ppid) => {
            let arc = {
                let guard = PROCESS_TABLE.lock();
                let table = guard.as_ref().unwrap();
                Arc::clone(table.get(ppid).unwrap().data())
            };
            let guard = arc.lock();
            guard.cwd.clone()
        }
        None => String::from("/"),
    };

    let memory_size = (elf_alloc.size() + USER_STACK_SIZE) as u64;

    let proc_data = Arc::new(Lock::new(ProcessData {
        pid: Pid::from_raw(0),
        fds,
        user_heap: crate::user_heap::UserHeap::new(),
        cwd,
        messages: MessageQueue::new(),
        poll_fds: [0; 64],
        poll_len: 0,
        elf_alloc: Some(elf_alloc),
        stack_alloc: Some(stack_alloc),
        tls_template,
        tls_filesz,
        tls_memsz,
        tls_alloc: Some(tls_alloc),
        tls_modules,
        tls_total_memsz,
        symbols: syms,
        loaded_libs,
        mmap_regions: Vec::new(),
        user_stack_base: stack_base,
        user_stack_size: USER_STACK_SIZE as u64,
        env,
        syscall_counts: [0; 64],
        syscall_total: 0,
    }));

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();

    let pid = table.insert_with(|pid| {
        proc_data.lock().pid = pid;
        SchedEntry::new(
            pid, ProcessState::Ready, Kind::Process { parent },
            child_cr3, ks_alloc, ks_rsp, fs_base, pid,
            make_name(path), memory_size, proc_data,
        )
    });

    let t3 = crate::clock::nanos_since_boot();
    log!("spawn: {} pid={} base={:#x} entry={:#x} cr3={:#x} ks={:#x}..{:#x} (read={}ms elf={}ms deps={}ms tls={}ms syms={}ms rest={}ms)",
        path, pid, loaded.base as u64, loaded.entry, child_cr3.unwrap().as_u64(),
        ks_base, ks_base + KERNEL_STACK_SIZE as u64,
        (t1 - t0) / 1_000_000, (t2 - t1) / 1_000_000, (t_deps - t2) / 1_000_000,
        (t_tls - t_deps) / 1_000_000, (t_syms - t_tls) / 1_000_000,
        (t3 - t_syms) / 1_000_000);

    Some(pid)
}

/// Spawn a process from kernel context (during boot). Resolves bare names
/// to `/bin/<name>`. Panics on failure.
pub fn spawn_kernel(argv: &[&str]) -> Pid {
    let mut fds = FdTable::new();
    fds.insert_at(0, Descriptor::SerialConsole);
    fds.insert_at(1, Descriptor::SerialConsole);
    fds.insert_at(2, Descriptor::SerialConsole);
    spawn(argv, fds, None, Vec::new()).expect("spawn_kernel: failed to spawn")
}

// ---------------------------------------------------------------------------
// Exit / teardown
// ---------------------------------------------------------------------------

/// Tear down a process: zombie all its threads, free all resources, wake parent.
/// Called in two phases:
/// - Phase 1 (resource cleanup): ProcessData lock held, table lock NOT held.
/// - Phase 2 (scheduling): table lock held through context switch.
fn teardown_resources(data_arc: &Arc<Lock<ProcessData>>, pid: Pid) {
    let mut data = data_arc.lock();

    // Print syscall profile for processes with significant activity
    if data.syscall_total > 0 {
        use alloc::string::String;
        use core::fmt::Write;
        let mut profile = String::new();
        for (i, &count) in data.syscall_counts.iter().enumerate() {
            if count > 0 {
                let _ = write!(profile, " {}={}", i, count);
            }
        }
        log!("syscalls: pid={pid} total={}{profile}", data.syscall_total);
    }

    fd::close_all(&mut data.fds, &mut *vfs::lock(), pid);
    data.tls_alloc.take();
    data.elf_alloc.take();
    data.stack_alloc.take();
    data.loaded_libs.clear();
    data.mmap_regions.clear();
    data.user_heap = crate::user_heap::UserHeap::new();
}

/// Phase 2 of teardown: zombie threads, free page tables, set zombie state.
/// Caller must hold PROCESS_TABLE lock and have already switched to kernel CR3.
fn teardown_scheduling(table: &mut ProcessTable, pid: Pid, code: i32) {
    // Kill all child threads
    let child_tids: Vec<Pid> = table.iter()
        .filter(|(tid, e)| *tid != pid && matches!(e.kind(), Kind::Thread { parent } if *parent == pid))
        .map(|(tid, _)| tid)
        .collect();
    for tid in &child_tids {
        let child = table.get_mut(*tid).unwrap();
        if !matches!(child.state(), ProcessState::Zombie(_)) {
            child.zombify(-1);
        }
    }
    // Note: zombie threads are NOT removed here — they may still be running
    // on another CPU. They'll be collected by collect_orphan_zombies in the
    // idle loop (which runs on the per-CPU idle stack).

    let entry = table.get_mut(pid).unwrap();
    let root = entry.take_cr3();
    let pml4 = root.as_ptr();
    shared_memory::cleanup_process(pid);
    paging::free_user_page_tables(pml4);
    let has_parent = matches!(entry.kind(), Kind::Process { parent: Some(_) });
    entry.zombify(code);
    let name = core::str::from_utf8(entry.name()).unwrap_or("?").trim_end_matches('\0');
    log!("exit: {name} pid={pid} code={code}");

    if let Some(names) = NAME_REGISTRY.lock().as_mut() { names.retain(|_, &mut v| v != pid); }

    // Collect orphaned child processes: remove zombies, detach running ones.
    let orphans: Vec<Pid> = table.iter()
        .filter(|(_, e)| matches!(e.kind(), Kind::Process { parent: Some(ppid) } if *ppid == pid))
        .map(|(cpid, _)| cpid)
        .collect();
    for cpid in orphans {
        if matches!(table.get(cpid).unwrap().state(), ProcessState::Zombie(_)) {
            table.remove_orphan_zombie_child(cpid);
        } else {
            table.get_mut(cpid).unwrap().detach_from_parent();
        }
    }

    if has_parent {
        // Wake parent if blocked on waitpid
        if let Kind::Process { parent: Some(ppid) } = table.get(pid).unwrap().kind() {
            if let Some(p) = table.get_mut(*ppid) {
                if let ProcessState::BlockedWaitPid(child) = *p.state() {
                    if child == pid {
                        p.set_state(ProcessState::Ready);
                    }
                }
            }
        }
    }
    // Parentless zombies are cleaned up by collect_orphan_zombies() in the idle loop,
    // NOT here — we're still running on this process's kernel stack.
}

/// Exit the entire process (all threads). If called from a thread, kills the
/// parent process and all siblings.
pub fn exit(code: i32) -> ! {
    // Phase 1: Determine process pid and get data Arc (brief table lock)
    let (process_pid, data_arc) = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();
        let pid = current_pid();
        let entry = table.get(pid).unwrap();
        let process_pid = match entry.kind() {
            Kind::Thread { parent } => *parent,
            Kind::Process { .. } => pid,
        };
        (process_pid, Arc::clone(table.get(process_pid).unwrap().data()))
    };

    // Switch to kernel CR3 before freeing resources
    unsafe { cpu::write_cr3(paging::kernel_cr3()); }

    // Phase 2: Resource cleanup (ProcessData lock, no table lock)
    teardown_resources(&data_arc, process_pid);

    // Phase 3: Scheduling teardown (table lock held through context switch)
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = current_pid();

    // If we're a thread, zombie ourselves first
    if let Kind::Thread { .. } = table.get(pid).unwrap().kind() {
        table.get_mut(pid).unwrap().zombify(code);
    }

    teardown_scheduling(table, process_pid, code);
    scheduler::schedule_no_return_locked(guard);
}

/// Exit the current thread only. For processes without threads, tears down
/// the process. For threads, zombifies without freeing the address space.
pub fn thread_exit(code: i32) -> ! {
    let kind = with_current_sched(|s| *s.kind());

    unsafe { cpu::write_cr3(paging::kernel_cr3()); }

    match kind {
        Kind::Thread { parent } => {
            // Thread exit: close our FDs, free TLS, zombie ourselves
            {
                let data_arc = current_data();
                let mut data = data_arc.lock();
                fd::close_all(&mut data.fds, &mut *vfs::lock(), current_pid());
                data.tls_alloc.take();
            }

            let mut guard = PROCESS_TABLE.lock();
            let table = guard.as_mut().unwrap();
            let pid = current_pid();
            table.get_mut(pid).unwrap().zombify(code);
            let name = core::str::from_utf8(table.get(pid).unwrap().name()).unwrap_or("?").trim_end_matches('\0');
            log!("exit: {name} pid={pid} code={code}");

            // Wake parent if blocked on thread_join
            if let Some(p) = table.get_mut(parent) {
                if let ProcessState::BlockedThreadJoin(child) = *p.state() {
                    if child == pid {
                        p.set_state(ProcessState::Ready);
                    }
                }
            }

            scheduler::schedule_no_return_locked(guard);
        }
        Kind::Process { .. } => {
            // Process exit — full teardown
            let data_arc = current_data();
            teardown_resources(&data_arc, current_pid());

            let mut guard = PROCESS_TABLE.lock();
            let table = guard.as_mut().unwrap();
            teardown_scheduling(table, current_pid(), code);
            scheduler::schedule_no_return_locked(guard);
        }
    }
}

// ---------------------------------------------------------------------------
// Blocking / scheduling
// ---------------------------------------------------------------------------

/// Block the current process and switch to the next ready one.
pub fn block(reason: ProcessState) {
    scheduler::block(reason);
}

pub fn block_poll(fds: [u64; 64], len: u32, deadline: u64) {
    debug_assert!(len <= 64, "poll_len {} exceeds array size", len);
    {
        let data_arc = current_data();
        let mut data = data_arc.lock();
        data.poll_fds = fds;
        data.poll_len = len;
    }
    scheduler::block(ProcessState::BlockedPoll { deadline });
}

/// Block waiting for a message, with an atomic recheck to prevent TOCTOU races.
///
/// The race: `sys_recv_msg` pops from the queue (empty), then `send_message`
/// pushes a message and checks state (still Running, so no wake), then the
/// receiver blocks — message sits in the queue with nobody to wake it.
///
/// Fix: hold the table lock, briefly lock ProcessData to check the queue,
/// and only transition to BlockedRecvMsg if the queue is truly empty.
/// Lock ordering: PROCESS_TABLE > ProcessData (same direction as idle_poll).
pub fn block_recv_msg() {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = percpu::current_pid().expect("block_recv_msg() called during idle");
    let data_arc = Arc::clone(table.get(pid).unwrap().data());

    // Brief ProcessData lock under table lock to check for messages.
    if data_arc.lock().messages.has_messages() {
        return; // Message arrived between pop and block — retry the loop.
    }

    table.get_mut(pid).unwrap().set_state(ProcessState::BlockedRecvMsg);
    scheduler::schedule_already_blocked(guard);
}

/// Cooperative yield: mark current as Ready, switch to next.
pub fn yield_now() {
    scheduler::yield_now();
}

// ---------------------------------------------------------------------------
// Message passing
// ---------------------------------------------------------------------------

/// Send a message to a target process. Wakes the target if blocked.
/// Three-phase lock discipline: no lock nesting between table and ProcessData.
pub fn send_message(target_pid: Pid, msg: crate::message::Message) -> bool {
    // Phase 1: Get target's data Arc (brief table lock)
    let data_arc = {
        let guard = PROCESS_TABLE.lock();
        match guard.as_ref().unwrap().get(target_pid) {
            Some(entry) => Arc::clone(entry.data()),
            None => return false,
        }
    };
    // Phase 2: Push message (per-process lock, no table lock held)
    {
        let mut data = data_arc.lock();
        if data.messages.push(msg).is_err() {
            return false;
        }
    }
    // Phase 3: Wake target (brief table lock, no process lock held)
    let mut guard = PROCESS_TABLE.lock();
    if let Some(entry) = guard.as_mut().unwrap().get_mut(target_pid) {
        if matches!(entry.state(), ProcessState::BlockedRecvMsg | ProcessState::BlockedPoll { .. }) {
            entry.set_state(ProcessState::Ready);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Futex
// ---------------------------------------------------------------------------

/// Atomically check a user futex word and block if it matches the expected value.
/// Returns 0 if woken normally, 1 if timed out, u64::MAX on error.
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
    let entry = table.get_mut(pid).unwrap();

    // Translate virtual → physical so cross-process futex works on shared memory
    let pml4 = entry.cr3().map(|r| r.as_ptr() as *const u64)
        .unwrap_or(crate::arch::paging::kernel_cr3() as *const u64);
    let phys_addr = match crate::arch::paging::virt_to_phys(pml4, addr) {
        Some(pa) => pa,
        None => return u64::MAX,
    };

    // Atomic check: read the user value under the lock
    let current = unsafe { core::ptr::read_volatile(addr as *const u32) };
    if current != expected {
        return 0;
    }

    entry.set_state(ProcessState::BlockedFutex { phys_addr, deadline });
    scheduler::schedule_already_blocked(guard);
    0
}

/// Wake up to `count` threads blocked on the same physical address as `addr`.
pub fn futex_wake(addr: u64, count: u64) -> u64 {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = current_pid();
    let caller_cr3 = table.get(pid).map(|e| {
        e.cr3().map(|r| r.as_ptr() as *const u64)
            .unwrap_or(crate::arch::paging::kernel_cr3() as *const u64)
    }).unwrap();
    let caller_phys = match crate::arch::paging::virt_to_phys(caller_cr3, addr) {
        Some(pa) => pa,
        None => return 0,
    };
    let mut woken = 0u64;
    for (_, entry) in table.iter_mut() {
        if woken >= count { break; }
        if let ProcessState::BlockedFutex { phys_addr, .. } = *entry.state() {
            if phys_addr == caller_phys {
                entry.set_state(ProcessState::Ready);
                woken += 1;
            }
        }
    }
    woken
}

// ---------------------------------------------------------------------------
// Pipe wake helpers
// ---------------------------------------------------------------------------

/// Wake processes blocked on reading from a pipe that now has data.
pub fn wake_pipe_readers(pipe_id: pipe::PipeId) {
    scheduler::wake_pipe_readers(pipe_id);
}

/// Wake processes blocked on writing to a pipe that now has space.
pub fn wake_pipe_writers(pipe_id: pipe::PipeId) {
    scheduler::wake_pipe_writers(pipe_id);
}

// ---------------------------------------------------------------------------
// Zombie collection
// ---------------------------------------------------------------------------

/// Atomically validate parent-child relationship and collect a zombie child process.
/// Thin wrapper that locks PROCESS_TABLE and delegates to ProcessTable method.
pub fn collect_child_zombie(child_pid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    table.collect_child_zombie(child_pid, parent_pid)
}

/// Atomically validate parent-thread relationship and collect a zombie thread.
/// Thin wrapper that locks PROCESS_TABLE and delegates to ProcessTable method.
pub fn collect_thread_zombie(tid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    table.collect_thread_zombie(tid, parent_pid)
}

// ---------------------------------------------------------------------------
// Name registry
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Symbol resolution / address validation
// ---------------------------------------------------------------------------

/// Check if an address is in the current process's valid memory ranges.
pub fn is_valid_user_addr(addr: u64) -> bool {
    let pid = current_pid();
    if pid == Pid::MAX { return false; }
    let data_arc = {
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return false };
        match table.get(pid) {
            Some(entry) => Arc::clone(entry.data()),
            None => return false,
        }
    };
    let guard = data_arc.lock();
    guard.symbols.is_valid_user_addr(addr)
}

// ---------------------------------------------------------------------------
// Kill
// ---------------------------------------------------------------------------

/// Kill a child process. Only the parent can kill its children.
/// Returns 0 on success, error code on failure.
pub fn kill_process(target_pid: Pid) -> u64 {
    use toyos_abi::syscall::SyscallError;
    let caller = current_pid();

    // Phase 1: Validate and get data Arc (brief table lock)
    let data_arc = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();

        let Some(entry) = table.get(target_pid) else { return SyscallError::NotFound.to_u64() };
        if !matches!(entry.kind(), Kind::Process { parent: Some(ppid) } if *ppid == caller) {
            return SyscallError::PermissionDenied.to_u64();
        }
        if *entry.state() == ProcessState::Running {
            return SyscallError::WouldBlock.to_u64();
        }
        if matches!(entry.state(), ProcessState::Zombie(_)) {
            return 0;
        }
        Arc::clone(entry.data())
    };

    // Phase 2: Resource cleanup (ProcessData lock, no table lock)
    {
        let mut data = data_arc.lock();
        fd::close_all(&mut data.fds, &mut *vfs::lock(), target_pid);
        data.tls_alloc.take();
        data.elf_alloc.take();
        data.stack_alloc.take();
        data.loaded_libs.clear();
        data.mmap_regions.clear();
        data.user_heap = crate::user_heap::UserHeap::new();
    }

    // Phase 3: Scheduling teardown (table lock)
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();

    // Re-check the target is still there and not zombie (might have exited between phases)
    let Some(entry) = table.get(target_pid) else { return 0 };
    if matches!(entry.state(), ProcessState::Zombie(_)) { return 0; }

    // Kill child threads of the target
    let child_tids: Vec<Pid> = table.iter()
        .filter(|(tid, e)| *tid != target_pid && matches!(e.kind(), Kind::Thread { parent } if *parent == target_pid))
        .map(|(tid, _)| tid)
        .collect();
    for tid in &child_tids {
        let child = table.get_mut(*tid).unwrap();
        if !matches!(child.state(), ProcessState::Zombie(_)) {
            child.zombify(-1);
        }
    }

    let entry = table.get_mut(target_pid).unwrap();
    let root = entry.take_cr3();
    let pml4 = root.as_ptr();
    shared_memory::cleanup_process(target_pid);
    paging::free_user_page_tables(pml4);

    entry.zombify(137); // 128 + 9 (SIGKILL-like)
    let name = core::str::from_utf8(entry.name()).unwrap_or("?").trim_end_matches('\0');
    log!("kill: {name} pid={target_pid}");

    if let Some(names) = NAME_REGISTRY.lock().as_mut() { names.retain(|_, &mut v| v != target_pid); }

    // Wake parent if blocked on waitpid for this process
    if let Some(parent) = table.get_mut(caller) {
        if let ProcessState::BlockedWaitPid(child) = *parent.state() {
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
