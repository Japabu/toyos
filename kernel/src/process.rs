use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::NonNull;
use crate::arch::percpu;
use crate::mm::PAGE_2M;
use crate::fd::{self, FdTable};
use crate::sync::Lock;
use crate::symbols::SymbolTable;
use crate::sched::payload::ThreadSched;
use crate::{elf, pipe, scheduler, shared_memory, vfs};
use crate::{DirectMap, UserAddr};
use crate::loader::{
    setup_tls, setup_combined_tls, alloc_kernel_stack, thread_start,
};

pub use toyos_abi::{Pid, Tid};
pub use crate::scheduler::TaskId;

// Re-export loader functions so existing callers (via `process::`) keep working.
pub use crate::loader::{spawn, spawn_kernel, build_child_fds};
pub(crate) use crate::loader::read_file_range;

/// Page tables shared between a process and all its threads.
pub type PageTables = Arc<Lock<crate::mm::paging::AddressSpace>>;

/// Allocate a virtual region and map physical memory into it.
/// Returns the allocated virtual address, or None if out of address space.
pub fn vma_map(
    pt: &Lock<crate::mm::paging::AddressSpace>,
    phys: u64,
    size: u64,
) -> Option<(UserAddr, u64)> {
    pt.lock().alloc_and_map(phys, size, true)
}

// ---------------------------------------------------------------------------
// OwnedAlloc — RAII heap allocation (for kernel-only buffers < 2MB)
// ---------------------------------------------------------------------------

/// Move-only wrapper around a heap allocation. Drop calls dealloc.
/// For kernel-only buffers (kernel stacks, TLS templates). NOT for user-mapped pages.
pub struct OwnedAlloc {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl OwnedAlloc {
    /// `None` for any size the kernel heap cannot serve, so a caller sizing a
    /// buffer from untrusted input gets a value to handle rather than a panic.
    /// The heap's page source asserts above `PAGE_2M` (`mm::alloc`) instead of
    /// returning null, so that ceiling has to be checked before the request,
    /// not after — the two real callers (128 KiB kernel stacks, ELF TLS
    /// templates) are far below it.
    pub fn new(size: usize, align: usize) -> Option<Self> {
        if size >= PAGE_2M as usize { return None; }
        let layout = Layout::from_size_align(size, align).ok()?;
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })?;
        Some(Self { ptr, layout })
    }

    pub fn ptr(&self) -> *mut u8 { self.ptr.as_ptr() }
    pub fn size(&self) -> usize { self.layout.size() }

    /// A bounds-checked view of the first `len` bytes.
    ///
    /// The only safe way to build a `KernelSlice` over an `OwnedAlloc` — the
    /// raw `KernelSlice::from_raw` was once handed an ELF's `p_filesz` over a
    /// `p_memsz`-sized buffer, which made every later `copy_from`/`read` on it
    /// out of bounds while still passing that slice's own checks.
    pub fn slice(&self, len: usize) -> crate::mm::KernelSlice {
        assert!(len <= self.size(), "OwnedAlloc::slice: {} > {}", len, self.size());
        unsafe { crate::mm::KernelSlice::from_raw(self.ptr(), len) }
    }
}

impl Drop for OwnedAlloc {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout); }
    }
}

unsafe impl Send for OwnedAlloc {}

// ---------------------------------------------------------------------------
// PageAlloc — contiguous 2MB physical pages from PMM
// ---------------------------------------------------------------------------

/// Contiguous 2MB-aligned physical pages from PMM. Provides a kernel-accessible
/// pointer via the direct map. Pages are zeroed on allocation, freed on drop.
pub struct PageAlloc(Vec<crate::mm::pmm::PhysPage>);

impl PageAlloc {
    /// Allocate `size` bytes as contiguous 2MB pages.
    pub fn new(size: usize, cat: crate::mm::pmm::Category) -> Option<Self> {
        let count = (size + PAGE_2M as usize - 1) / PAGE_2M as usize;
        Some(Self(crate::mm::pmm::alloc_contiguous(count, cat)?))
    }

    /// Kernel pointer to the start of the allocation (via direct map).
    pub fn ptr(&self) -> *mut u8 {
        self.0[0].direct_map().as_mut_ptr()
    }

    /// Total size in bytes (always a multiple of 2MB).
    pub fn size(&self) -> usize {
        self.0.len() * PAGE_2M as usize
    }

    /// Physical address of the start.
    pub fn phys(&self) -> u64 {
        self.0[0].direct_map().phys()
    }
}

pub const KERNEL_STACK_SIZE: usize = 128 * 1024;

/// Type-safe user stack. Knows its virtual address (what userland sees) and
/// physical address (for kernel direct-map writes). Impossible to confuse the two.
pub struct UserStack {
    vaddr: UserAddr,
    phys: DirectMap,
    size: u64,
}

impl UserStack {
    pub fn new(vaddr: UserAddr, phys: DirectMap, size: u64) -> Self {
        Self { vaddr, phys, size }
    }

    /// User-visible virtual address of the stack top (highest address).
    pub fn top(&self) -> u64 { self.vaddr.raw() + self.size }

    /// User-visible virtual base address.
    pub fn base(&self) -> UserAddr { self.vaddr }

    pub fn size(&self) -> u64 { self.size }

    /// Convert a user virtual address on this stack to a kernel direct-map pointer.
    /// Panics if the address is outside this stack.
    fn kern_ptr(&self, user_addr: u64) -> *mut u8 {
        let offset = user_addr.checked_sub(self.vaddr.raw())
            .expect("UserStack: address below stack base");
        assert!(offset < self.size, "UserStack: address above stack top");
        DirectMap::from_phys(self.phys.phys() + offset).as_mut_ptr::<u8>()
    }

    /// Write argc, argv pointers, and string data onto this stack.
    /// Returns the new user-visible stack pointer.
    pub fn write_argv(&self, args: &[&str]) -> u64 {
        let mut sp = self.top();
        let mut argv_ptrs: Vec<u64> = Vec::with_capacity(args.len());
        for arg in args.iter().rev() {
            sp -= (arg.len() + 1) as u64;
            let kptr = self.kern_ptr(sp);
            unsafe {
                core::ptr::copy_nonoverlapping(arg.as_ptr(), kptr, arg.len());
                *kptr.add(arg.len()) = 0;
            }
            argv_ptrs.push(sp);
        }
        argv_ptrs.reverse();
        let metadata_qwords = args.len() + 2;
        sp = (sp - metadata_qwords as u64 * 8) & !15;
        let ksp = self.kern_ptr(sp) as *mut u64;
        unsafe {
            *ksp = args.len() as u64;
            for (i, ptr) in argv_ptrs.iter().enumerate() {
                *ksp.add(1 + i) = *ptr;
            }
            *ksp.add(1 + args.len()) = 0;
        }
        sp
    }
}

/// Where a thread/process is in its lifecycle.
///
/// The process table tracks alive vs zombie. For alive threads, the scheduler
/// is authoritative about whether they're running, ready, or blocked —
/// query `scheduler::task_sched_state()` for that detail.
#[derive(Clone, Copy, PartialEq)]
pub enum ThreadLocation {
    /// Alive: running, ready, or blocked. The scheduler owns the detail.
    Scheduled,
    /// Exited with the given code. Waiting to be reaped.
    Zombie(i32),
}

pub type ProcessState = ThreadLocation;

/// Proof that a process was zombified and its orphaned children must be handled.
/// Returned by `ProcessEntry::zombify`, consumed by `ProcessTable::handle_orphans`.
#[must_use = "orphaned children must be collected after zombifying a process"]
pub struct OrphanCleanup(Pid);

impl ThreadLocation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Scheduled => "Scheduled",
            Self::Zombie(_) => "Zombie",
        }
    }
}

// ---------------------------------------------------------------------------
// ProcessEntry + ThreadEntry — hierarchical process/thread table
// ---------------------------------------------------------------------------

/// Per-thread metadata. Tid is the HashMap key in ProcessEntry.threads.
pub struct ThreadEntry {
    state: ThreadLocation,
    name: [u8; 28],
    thread_data: Arc<Lock<ThreadData>>,
    /// The thread's scheduler faces (rendezvous word + published counters).
    /// `None` only between the table insert that allocates the tid and the
    /// `sched::spawn` that needs it — the task cannot exist before its own id.
    sched: Option<ThreadSched>,
}

impl ThreadEntry {
    pub fn new(thread_data: Arc<Lock<ThreadData>>) -> Self {
        Self { state: ThreadLocation::Scheduled, name: [0u8; 28], thread_data, sched: None }
    }
    pub fn set_sched(&mut self, sched: ThreadSched) {
        assert!(self.sched.is_none(), "thread already has a scheduler record");
        self.sched = Some(sched);
    }
    pub fn sched(&self) -> Option<&ThreadSched> { self.sched.as_ref() }
    pub fn state(&self) -> ThreadLocation { self.state }
    pub fn name(&self) -> &[u8; 28] { &self.name }
    pub fn set_name(&mut self, name: &[u8]) {
        self.name = [0u8; 28];
        let len = name.len().min(28);
        self.name[..len].copy_from_slice(&name[..len]);
    }
    pub fn thread_data(&self) -> &Arc<Lock<ThreadData>> { &self.thread_data }
}

/// A process and all its threads. Removing a ProcessEntry removes all threads.
pub struct ProcessEntry {
    pid: Pid,
    parent: Option<Pid>,
    state: ProcessState,
    name: [u8; 28],
    process_data: Arc<Lock<ProcessData>>,
    symbols: Arc<Lock<SymbolTable>>,
    main_tid: Tid,
    threads: crate::id_map::IdMap<Tid, ThreadEntry>,
    /// Set once by the single exit/kill path that owns this process's
    /// teardown. Guarded by the table lock. Checked by spawn_thread so no
    /// new thread can appear after the teardown's retire sweep.
    tearing_down: bool,
}

impl ProcessEntry {
    /// Create a new process with its main thread. Returns the entry and the
    /// allocated main tid (always Tid(0) for the first thread).
    pub fn new(
        pid: Pid,
        parent: Option<Pid>,
        name: [u8; 28],
        process_data: Arc<Lock<ProcessData>>,
        symbols: Arc<Lock<SymbolTable>>,
        main_thread: ThreadEntry,
    ) -> Self {
        let mut threads = crate::id_map::IdMap::new();
        let main_tid = threads.insert(main_thread);
        Self { pid, parent, state: ThreadLocation::Scheduled, name, process_data, symbols, main_tid, threads, tearing_down: false }
    }
    pub fn pid(&self) -> Pid { self.pid }
    pub fn parent(&self) -> Option<Pid> { self.parent }
    pub fn state(&self) -> ProcessState { self.state }
    pub fn name(&self) -> &[u8; 28] { &self.name }
    pub fn name_str(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("?").trim_end_matches('\0')
    }
    pub fn process_data(&self) -> &Arc<Lock<ProcessData>> { &self.process_data }
    pub fn symbols(&self) -> &Arc<Lock<SymbolTable>> { &self.symbols }
    pub fn main_tid(&self) -> Tid { self.main_tid }
    pub fn threads(&self) -> &crate::id_map::IdMap<Tid, ThreadEntry> { &self.threads }
    pub fn threads_mut(&mut self) -> &mut crate::id_map::IdMap<Tid, ThreadEntry> { &mut self.threads }
}

impl ProcessEntry {
    /// Zombify this process. Returns an `OrphanCleanup` token that must be consumed.
    pub fn zombify(&mut self, code: i32) -> OrphanCleanup {
        assert!(!matches!(self.state, ProcessState::Zombie(_)),
            "double zombify pid={}", self.pid);
        self.state = ProcessState::Zombie(code);
        OrphanCleanup(self.pid)
    }

    /// Claim exclusive teardown of this process. Exactly one exit/kill path
    /// wins; later callers must simply exit their own thread — the claimant's
    /// retire sweep handles them like any other thread.
    pub fn claim_teardown(&mut self) -> bool {
        if self.tearing_down {
            return false;
        }
        self.tearing_down = true;
        true
    }

    pub fn tearing_down(&self) -> bool { self.tearing_down }
}

impl Drop for ProcessEntry {
    fn drop(&mut self) {
        // The scheduler's per-process vruntime entry must live exactly as
        // long as the table entry: threads of a zombified process still
        // yield inside their own exit path, and the Yield re-insert reads
        // the entry. Freeing it earlier (the old teardown_resources site)
        // let a timer preempt inside the exit window panic in
        // current_runnable_vruntime.
        scheduler::remove_vruntime(self.pid);
    }
}

// ---------------------------------------------------------------------------
// ProcessData — per-process data behind Arc<Lock<ProcessData>>
// ---------------------------------------------------------------------------

/// Record of a single demand-paged fault, stored in a ring buffer for crash diagnostics.
#[derive(Clone, Copy)]
pub struct PageFaultRecord {
    pub fault_addr: u64,
    pub page_elf_offset: u64,
    pub block_idx: u32,
    pub reloc_count: u16,
    pub flags: u16, // bit 0: writable, bit 1: has_relocs, bit 2: anonymous, bit 3: beyond_extent
    pub duration_us: u16, // microseconds spent handling this fault
}

/// Fixed-size ring buffer of recent page fault events for crash diagnostics.
pub struct PageFaultTrace {
    entries: [PageFaultRecord; 32],
    write_pos: usize,
    total: u64,
}

impl PageFaultTrace {
    pub fn new() -> Self {
        Self {
            entries: [PageFaultRecord {
                fault_addr: 0, page_elf_offset: 0, block_idx: 0,
                reloc_count: 0, flags: 0, duration_us: 0,
            }; 32],
            write_pos: 0,
            total: 0,
        }
    }

    /// Record a page fault event.
    pub fn record(&mut self, rec: PageFaultRecord) {
        self.entries[self.write_pos] = rec;
        self.write_pos = (self.write_pos + 1) % 32;
        self.total += 1;
    }

    /// Iterate entries in chronological order (oldest first).
    pub fn iter_chronological(&self) -> impl Iterator<Item = &PageFaultRecord> {
        let count = self.total.min(32) as usize;
        let start = if self.total >= 32 { self.write_pos } else { 0 };
        (0..count).map(move |i| &self.entries[(start + i) % 32])
    }

    pub fn total(&self) -> u64 { self.total }
}

/// ELF loading artifacts and TLS state. Grouped separately from runtime process
/// state to keep concerns distinct — a future lock split becomes trivial.
pub struct ElfInfo {
    pub elf_alloc: Option<OwnedAlloc>,
    // Thread-local storage (process-level: template, modules, layout)
    pub tls_template: Option<crate::mm::KernelSlice>,
    pub tls_memsz: usize,
    /// Multi-module TLS layout per loaded library.
    pub tls_modules: Vec<crate::elf::TlsModule>,
    /// Total combined TLS size across all modules.
    pub tls_total_memsz: usize,
    /// Maximum TLS alignment across all modules.
    pub tls_max_align: usize,
    /// Next module ID to assign on dlopen (1-based, exe=1).
    pub next_tls_module_id: u64,
    /// Dynamically allocated TLS blocks for dlopen'd modules, keyed by (thread Tid, module_id).
    /// Stored in process-level data so the VMA and backing memory have the same lifetime.
    pub dynamic_tls_blocks: alloc::collections::BTreeMap<(Tid, u64), PageAlloc>,
    /// Dynamically loaded shared libraries (indexed by dlopen handle).
    pub loaded_libs: Vec<elf::LoadedLib>,
    /// RELATIVE relocation index for demand-paged ELF (applied per-page on fault).
    pub reloc_index: Option<Arc<elf::RelocationIndex>>,
    /// Runtime base address for the demand-paged ELF (for relocation computation).
    pub elf_base: UserAddr,
    /// Executable .eh_frame_hdr vaddr (stated ELF vaddr, before base offset).
    pub exe_eh_frame_hdr_vaddr: u64,
    /// Executable .eh_frame_hdr size.
    pub exe_eh_frame_hdr_size: u64,
    /// Executable virtual address extent (elf_base + vaddr_max - vaddr_min).
    pub exe_vaddr_max: u64,
    /// Paths of dlopen'd libraries (parallel to loaded_libs).
    pub lib_paths: Vec<String>,
}

/// Process-level data shared across all threads via `Arc<Lock<ProcessData>>`.
/// Contains fds, memory mappings, ELF state, accounting — everything that belongs to the process.
/// Accessed via `with_fd_owner_data`. All threads of a process share the same Arc.
pub struct ProcessData {
    pub fds: FdTable,
    pub cwd: String,
    /// Inherited environment variables (KEY=VALUE\0KEY2=VALUE2\0...)
    pub env: Vec<u8>,

    /// ELF loading artifacts and TLS state.
    pub elf: ElfInfo,

    // Anonymous memory mappings (mmap)
    pub mmap_regions: Vec<MmapRegion>,
    /// 2MB allocations for demand-paged pages. Freed on process exit.
    pub demand_pages: Vec<PageAlloc>,
    /// Ring buffer of recent page faults for crash diagnostics.
    pub fault_trace: PageFaultTrace,
    /// Peak memory usage in bytes (high-water mark)
    pub peak_memory: u64,
    /// Total allocations (demand pages + mmap + TLS blocks)
    pub alloc_count: u64,
    /// Total frees (munmap)
    pub free_count: u64,
    /// Executable path (for SYS_QUERY_MODULES).
    pub exe_path: String,

    // --- Process accounting (Layer 1 diagnostics) ---
    pub spawn_ns: u64,
    pub accounting: ProcessAccounting,
    /// Stashed stats from exited children (capped at 64).
    pub child_stats: Vec<(Pid, toyos_abi::syscall::ProcessStats)>,
}

/// Per-process accounting counters. Accumulated from all threads on exit.
#[derive(Default)]
pub struct ProcessAccounting {
    pub fault_demand_count: u32,
    pub fault_zero_count: u32,
    pub fault_ns: u64,
    pub io_read_ops: u32,
    pub io_read_bytes: u64,
    pub blocked_io_ns: u64,
    pub blocked_futex_ns: u64,
    pub blocked_pipe_ns: u64,
    pub blocked_ipc_ns: u64,
    pub blocked_other_ns: u64,
    pub child_threads_cpu_ns: u64,
    pub runqueue_wait_ns: u64,
}

/// Per-thread data, unique to each thread via `Arc<Lock<ThreadData>>`.
/// Contains thread-local storage pages, stack info, syscall profiling.
/// Accessed via `with_current_data`. Each thread has its own Arc.
pub struct ThreadData {
    pub tls_pages: Option<PageAlloc>,
    pub stack_pages: Option<PageAlloc>,
    // User stack location (for SYS_STACK_INFO)
    pub user_stack_base: UserAddr,
    pub user_stack_size: u64,
    /// Syscall counts per syscall number (for profiling)
    pub syscall_counts: [u32; 64],
    pub syscall_total: u64,
    /// Wall-clock nanoseconds spent in syscall dispatch (includes preemption time)
    pub syscall_total_ns: u64,
}

pub struct MmapRegion {
    pub addr: UserAddr,
    pub size: usize,
    pub _pages: PageAlloc,
    /// True if this is a MAP_FIXED mapping (virt addr != phys addr).
    pub fixed: bool,
}

// ---------------------------------------------------------------------------
// IdleProof — zero-cost proof that code runs on the per-CPU idle stack
// ---------------------------------------------------------------------------

/// Zero-sized proof that we are on the per-CPU idle stack.
/// Required by `ProcessTable::collect_orphan_zombies` to prevent calling it
/// from a process's kernel stack (which would be use-after-free if we drop
/// the thread entry we're running on).
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
// Process table — IdMap<Pid, ProcessEntry> with lifecycle operations
// ---------------------------------------------------------------------------

pub type ProcessTable = crate::id_map::IdMap<Pid, ProcessEntry>;

pub static PROCESS_TABLE: Lock<Option<ProcessTable>> = Lock::new(None);

pub fn init() {
    *PROCESS_TABLE.lock() = Some(ProcessTable::new());
}

/// Waitpid: collect a zombie child process. Removes process and ALL its threads.
pub fn collect_child_zombie(table: &mut ProcessTable, child_pid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let proc = table.get(child_pid).ok_or(())?;
    if proc.parent != Some(parent_pid) { return Err(()); }
    if let ProcessState::Zombie(code) = proc.state {
        table.remove(child_pid);
        Ok(Some(code))
    } else {
        Ok(None)
    }
}

/// Thread join: collect a zombie thread.
pub fn collect_thread_zombie(table: &mut ProcessTable, tid: Tid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let proc = table.get(parent_pid).ok_or(())?;
    let thread = proc.threads.get(tid).ok_or(())?;
    if let ThreadLocation::Zombie(code) = thread.state {
        table.get_mut(parent_pid).unwrap().threads.remove(tid);
        Ok(Some(code))
    } else {
        Ok(None)
    }
}

/// Handle orphaned child processes of a just-zombified process.
/// Consumes the `OrphanCleanup` token, ensuring this step is never skipped.
fn handle_orphans(table: &mut ProcessTable, cleanup: OrphanCleanup) {
    let pid = cleanup.0;
    let orphan_pids: Vec<Pid> = table.iter()
        .filter(|(_, p)| p.parent == Some(pid))
        .map(|(pid, _)| pid)
        .collect();
    for child_pid in orphan_pids {
        if table.get(child_pid).map_or(false, |p| matches!(p.state, ProcessState::Zombie(_))) {
            table.remove(child_pid);
        } else {
            table.get_mut(child_pid).unwrap().parent = None;
        }
    }
}

/// Zombify a thread or process. Handles main-thread-vs-child-thread
/// logic internally. Idempotent — does nothing if already a zombie.
pub fn zombify_tid(table: &mut ProcessTable, pid: Pid, tid: Tid, code: i32) {
    let is_main = table.get(pid).map_or(false, |p| p.main_tid == tid);
    if is_main {
        let proc = table.get_mut(pid).unwrap();
        if !matches!(proc.state, ProcessState::Zombie(_)) {
            let cleanup = proc.zombify(code);
            handle_orphans(table, cleanup);
        }
    } else {
        if let Some(proc) = table.get_mut(pid) {
            if let Some(thread) = proc.threads.get_mut(tid) {
                if !matches!(thread.state, ProcessState::Zombie(_)) {
                    thread.state = ProcessState::Zombie(code);
                }
            }
        }
    }
}

/// Zombify a thread that died in panic recovery, and name the waiter that
/// must be woken for it.
///
/// The panic path itself cannot do this — it may hold any lock the faulted
/// thread was holding — so it only records the thread in the poison set and
/// this runs later, from the idle loop. Returning the waiter rather than
/// waking it here keeps the wake outside the table lock, as every other exit
/// path does.
///
/// The waiter is the same one the corresponding clean exit would wake: a main
/// thread's death releases the parent's `waitpid` (`exit`), any other thread's
/// releases its own process's `thread_join` (`thread_exit`). `None` means
/// there is nothing to wake — the entry is already gone, or the process has
/// no parent, in which case `collect_orphan_zombies` reaps it.
#[must_use = "the waiter of a zombified thread must be woken"]
pub fn zombify_poisoned(table: &mut ProcessTable, pid: Pid, tid: Tid) -> Option<(Pid, Tid)> {
    let Some(proc) = table.get(pid) else { return None };
    let waiter = if tid == proc.main_tid {
        proc.parent.and_then(|ppid| table.get(ppid).map(|p| (ppid, p.main_tid)))
    } else {
        Some((pid, proc.main_tid))
    };
    zombify_tid(table, pid, tid, -1);
    waiter
}

/// Sweep orphan zombie processes. Single pass — threads are structurally owned.
pub fn collect_orphan_zombies(table: &mut ProcessTable, _proof: IdleProof) {
    let orphans: Vec<Pid> = table.iter()
        .filter(|(_, p)| p.parent.is_none() && matches!(p.state, ProcessState::Zombie(_)))
        .map(|(pid, _)| pid)
        .collect();
    for pid in orphans {
        table.remove(pid);
    }
}

pub fn current_tid() -> Tid {
    percpu::current_tid().expect("current_tid() called during idle (no thread running)")
}

pub fn current_process() -> Pid {
    percpu::current_pid().expect("current_process() called during idle (no thread running)")
}

pub fn current_address_space() -> PageTables {
    scheduler::current_address_space().expect("current_address_space: no address space")
}

// ---------------------------------------------------------------------------
// Access patterns — ProcessData (clone Arc, drop table lock, lock ProcessData)
// ---------------------------------------------------------------------------

/// Get the current thread's ThreadData Arc (brief table lock).
/// If the entry is gone (process killed while thread was running), exits silently.
pub fn current_data() -> Arc<Lock<ThreadData>> {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    match table.get(current_process()).and_then(|p| p.threads.get(current_tid())) {
        Some(thread) => Arc::clone(&thread.thread_data),
        None => {
            drop(guard);
            scheduler::exit_current(-1);
        }
    }
}

/// Set the name of the currently running thread.
pub fn set_current_thread_name(name: &[u8]) {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    if let Some(proc) = table.get_mut(current_process()) {
        if let Some(thread) = proc.threads.get_mut(current_tid()) {
            thread.set_name(name);
        }
    }
}

/// Get the process-level ProcessData Arc (brief table lock).
/// All threads of a process share the same Arc — no table walk needed.
pub fn fd_owner_data() -> Arc<Lock<ProcessData>> {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    match table.get(current_process()) {
        Some(proc) => Arc::clone(&proc.process_data),
        None => {
            drop(guard);
            scheduler::exit_current(-1);
        }
    }
}

/// Access the current thread's ThreadData mutably.
/// Table lock is NOT held during the closure — only the per-thread lock.
pub fn with_current_data<R>(f: impl FnOnce(&mut ThreadData) -> R) -> R {
    let arc = current_data();
    let mut guard = arc.lock();
    f(&mut guard)
}

/// Access the process-level ProcessData mutably.
/// Table lock is NOT held during the closure — only the per-process lock.
pub fn with_fd_owner_data<R>(f: impl FnOnce(&mut ProcessData) -> R) -> R {
    let arc = fd_owner_data();
    let mut guard = arc.lock();
    f(&mut guard)
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawn a thread within the current process.
pub fn spawn_thread(entry: u64, stack_ptr: u64, arg: u64, stack_base: u64) -> Option<Tid> {
    // Phase 1: Get parent's data + address space (never held simultaneously)
    let parent_process = current_process();
    let (parent_addr_space, process_data_arc) = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();
        let proc = table.get(parent_process).unwrap();
        if proc.tearing_down() {
            return None;
        }
        let addr_space = scheduler::current_address_space();
        (addr_space, Arc::clone(&proc.process_data))
    };
    let (tls_template, tls_memsz, tls_modules, tls_total_memsz, tls_max_align) = {
        let data = process_data_arc.lock();
        (data.elf.tls_template, data.elf.tls_memsz,
         data.elf.tls_modules.clone(), data.elf.tls_total_memsz, data.elf.tls_max_align)
    };

    // Phase 2: Allocate TLS (outside any lock)
    let (tls_alloc, fs_base) = if !tls_modules.is_empty() {
        setup_combined_tls(&tls_modules, tls_total_memsz, tls_max_align)?
    } else {
        setup_tls(tls_template, tls_memsz, tls_max_align)?
    };
    let fs_base = {
        let addr_space = parent_addr_space.as_ref().expect("spawn_thread: no address space");
        let parent_data = process_data_arc.lock();
        let tls_phys = tls_alloc.phys();
        // VA exhaustion is a resource failure a process can reach by spawning
        // threads until its range is gone, not a kernel bug. `tls_alloc`
        // drops on the way out, returning its pages.
        let Some((tls_vaddr, _)) = vma_map(addr_space, tls_phys, tls_alloc.size() as u64) else {
            return None;
        };
        // Rebase fs_base and internal TLS pointers from physical to virtual
        let tls_rebase = tls_vaddr.raw() as i64 - tls_phys as i64;
        let fs_base = (fs_base as i64 + tls_rebase) as u64;
        unsafe {
            let tls_base_ptr = DirectMap::from_phys(tls_phys).as_mut_ptr::<u8>();
            let tp_kern = tls_base_ptr.add((fs_base - tls_vaddr.raw()) as usize);
            let self_ptr = tp_kern as *mut u64;
            *self_ptr = fs_base;
            let dtv_phys = *self_ptr.add(1);
            *self_ptr.add(1) = (dtv_phys as i64 + tls_rebase) as u64;
            let dtv_kern = tls_base_ptr as *mut u64;
            let dtv_len = *dtv_kern.add(1) as usize;
            for i in 0..dtv_len {
                let entry = *dtv_kern.add(2 + i);
                if entry != !0u64 && entry != 0 {
                    *dtv_kern.add(2 + i) = (entry as i64 + tls_rebase) as u64;
                }
            }
        }
        drop(parent_data);
        fs_base
    };

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(thread_start, entry, stack_ptr, arg) {
        Some(ks) => ks,
        None => {
            drop(tls_alloc);
            return None;
        }
    };

    // Phase 3: Insert into table (brief table lock)
    // Threads share the parent's ProcessData Arc — no empty fds or zeroed process fields.
    let thread_data = Arc::new(Lock::new(ThreadData {
        tls_pages: Some(tls_alloc),
        stack_pages: None,
        user_stack_base: UserAddr::new(stack_base),
        user_stack_size: if stack_base > 0 { stack_ptr - stack_base } else { 0 },
        syscall_counts: [0; 64],
        syscall_total: 0,
        syscall_total_ns: 0,
    }));

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let proc = table.get_mut(parent_process)?;
    if proc.tearing_down() {
        // Teardown claimed the process between Phase 1 and here — a thread
        // enqueued now would be invisible to its retire sweep.
        return None;
    }
    let tid = proc.threads.insert(ThreadEntry::new(thread_data));

    // Placed while still holding the table lock: teardown claims the process
    // under this lock, so a thread that passed the check above is fully visible
    // to the scheduler before any retire sweep can start.
    let sched = scheduler::enqueue_new(
        TaskId(parent_process, tid),
        ks_alloc,
        ks_rsp,
        parent_addr_space,
        fs_base,
    );
    proc.threads.get_mut(tid).unwrap().set_sched(sched);
    drop(guard);
    Some(tid)
}


// ---------------------------------------------------------------------------
// Exit / teardown
// ---------------------------------------------------------------------------

/// Tear down a process: zombie all its threads, free all resources, wake parent.
/// Called in two phases:
/// - Phase 1 (resource cleanup): ProcessData lock held, table lock NOT held.
/// - Phase 2 (scheduling): table lock held through context switch.
/// Returns (syscall_total, syscall_total_ns) for the main thread, needed by the accounting snapshot.
fn teardown_resources(
    process_data_arc: &Arc<Lock<ProcessData>>,
    thread_data_arc: &Arc<Lock<ThreadData>>,
    pid: Pid,
) -> (u64, u64) {
    // Phase 1: Thread-level cleanup (never hold ThreadData + ProcessData simultaneously)
    let (syscall_total, syscall_total_ns, syscall_counts) = {
        let mut tdata = thread_data_arc.lock();
        let stats = (tdata.syscall_total, tdata.syscall_total_ns, tdata.syscall_counts);
        tdata.tls_pages.take();
        tdata.stack_pages.take();
        stats
    };

    // Phase 2: Process-level cleanup (single lock acquisition)
    let mut data = process_data_arc.lock();

    // Flush current thread's blocked/runqueue stats into process accounting
    if percpu::current_pid() == Some(pid) {
        scheduler::flush_current_stats(&mut data.accounting);
    }

    // Print syscall profile for processes with significant activity
    if syscall_total > 0 {
        use alloc::string::String;
        use core::fmt::Write;
        let mut profile = String::new();
        for (i, &count) in syscall_counts.iter().enumerate() {
            if count > 0 {
                let _ = write!(profile, " {}={}", i, count);
            }
        }
        let wall_ms = syscall_total_ns / 1_000_000;
        log!("syscalls: pid={pid} total={} syscall_wall={wall_ms}ms{profile}", syscall_total);
    }

    // Print memory stats
    if data.peak_memory > 0 || data.alloc_count > 0 {
        log!("memory: pid={pid} peak={}MB allocs={} frees={}",
            data.peak_memory / (1024 * 1024), data.alloc_count, data.free_count);
    }

    // Free resources
    fd::close_all(&mut data.fds, &mut *vfs::lock(), pid);
    data.elf.elf_alloc.take();
    data.elf.loaded_libs.clear();
    data.mmap_regions.clear();
    data.demand_pages.clear();
    data.elf.reloc_index = None;

    (syscall_total, syscall_total_ns)
}

/// Table-side teardown bookkeeping: mark remaining thread entries zombie,
/// release shared memory, zombify the process, adopt orphans.
///
/// Caller must hold the PROCESS_TABLE lock, have claimed teardown, retired
/// every other thread of the process (so none can run), and freed resources.
/// Returns (pid, tid) wakes to deliver after the table lock drops.
fn teardown_bookkeeping(table: &mut ProcessTable, process_pid: Pid, code: i32,
                        main_cpu_ns: u64) -> Vec<(Pid, Tid)> {
    let proc = table.get_mut(process_pid)
        .expect("teardown_bookkeeping: process not found");
    let main_tid = proc.main_tid;

    let tids: Vec<Tid> = proc.threads.iter().map(|(tid, _)| tid).collect();
    for tid in tids {
        let thread = proc.threads.get_mut(tid).unwrap();
        if !matches!(thread.state, ProcessState::Zombie(_)) {
            thread.state = ProcessState::Zombie(if tid == main_tid { code } else { -1 });
        }
    }

    shared_memory::cleanup_process(process_pid);

    let proc = table.get(process_pid).unwrap();
    let cpu_ms = main_cpu_ns / 1_000_000;
    let parent_pid = proc.parent;
    let name = proc.name_str();
    log!("exit: {name} pid={process_pid} code={code} cpu={cpu_ms}ms");

    // Tolerate an already-zombified process: a main thread that lost the
    // teardown claim zombifies itself through exit_current's zombify_tid
    // before the claimant reaches this point.
    let proc = table.get_mut(process_pid).unwrap();
    if !matches!(proc.state, ProcessState::Zombie(_)) {
        let orphan_cleanup = proc.zombify(code);
        handle_orphans(table, orphan_cleanup);
    }

    // Identify parent to wake for waitpid
    let mut to_wake = Vec::new();
    if let Some(ppid) = parent_pid {
        if let Some(parent_proc) = table.get(ppid) {
            to_wake.push((ppid, parent_proc.main_tid));
        }
    }

    to_wake
}

/// Exit the entire process (all threads). If called from a thread, kills the
/// parent process and all siblings.
/// Build accounting snapshot from ProcessData (after all threads flushed) and stash on parent.
/// Must be called after teardown_scheduling so child thread stats are included.
fn stash_accounting_snapshot(
    process_data_arc: &Arc<Lock<ProcessData>>,
    pid: Pid,
    parent_pid: Option<Pid>,
    syscall_total: u64,
    syscall_total_ns: u64,
    main_cpu_ns: u64,
) {
    use toyos_abi::syscall::ProcessStats;

    let ppid = match parent_pid {
        Some(ppid) => ppid,
        None => return,
    };
    let data = process_data_arc.lock();
    let acct = &data.accounting;
    let snapshot = ProcessStats {
        wall_ns: crate::clock::nanos_since_boot().saturating_sub(data.spawn_ns),
        cpu_ns: main_cpu_ns + acct.child_threads_cpu_ns,
        syscall_total,
        syscall_total_ns,
        fault_demand_count: acct.fault_demand_count,
        fault_zero_count: acct.fault_zero_count,
        fault_ns: acct.fault_ns,
        io_read_ops: acct.io_read_ops,
        _pad: 0,
        io_read_bytes: acct.io_read_bytes,
        blocked_io_ns: acct.blocked_io_ns,
        blocked_futex_ns: acct.blocked_futex_ns,
        blocked_pipe_ns: acct.blocked_pipe_ns,
        blocked_ipc_ns: acct.blocked_ipc_ns,
        blocked_other_ns: acct.blocked_other_ns,
        runqueue_wait_ns: acct.runqueue_wait_ns,
        peak_memory: data.peak_memory,
        alloc_count: data.alloc_count,
    };
    drop(data);

    let parent_arc = {
        let guard = PROCESS_TABLE.lock();
        guard.as_ref().and_then(|t| t.get(ppid))
            .map(|p| Arc::clone(&p.process_data))
    };
    if let Some(parent_arc) = parent_arc {
        let mut pdata = parent_arc.lock();
        if pdata.child_stats.len() >= 64 {
            pdata.child_stats.remove(0);
        }
        pdata.child_stats.push((pid, snapshot));
    }
}

pub fn exit(code: i32) -> ! {
    let process_pid = current_process();
    let tid = current_tid();

    // Phase 1: claim teardown. Exactly one exit/kill path tears a process
    // down; later arrivals just exit their own thread — the claimant's
    // retire sweep accounts for them like any other thread.
    let (process_data_arc, thread_data_arc, parent_pid, main_tid, other_tids) = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let Some(proc) = table.get_mut(process_pid) else {
            drop(guard);
            unsafe { crate::mm::paging::kernel_cr3().activate(); }
            scheduler::exit_current(code);
        };
        if proc.threads.get(tid).is_none() || !proc.claim_teardown() {
            drop(guard);
            unsafe { crate::mm::paging::kernel_cr3().activate(); }
            scheduler::exit_current(code);
        };
        let other_tids: Vec<Tid> = proc.threads.iter()
            .map(|(t, _)| t)
            .filter(|&t| t != tid)
            .collect();
        let thread = proc.threads.get(tid).unwrap();
        (Arc::clone(&proc.process_data), Arc::clone(&thread.thread_data),
         proc.parent, proc.main_tid, other_tids)
    };

    unsafe { crate::mm::paging::kernel_cr3().activate(); }

    // Phase 2: retire every other thread. Each one is provably out of the
    // scheduler when retire_task returns — not queued, not parked, not
    // mid-steal, not running. Only then is it safe to free memory their
    // page tables still map: freeing earlier let a still-running thread
    // write through stale mappings into 2MB frames the PMM had already
    // re-issued (kernel heap corruption).
    let mut main_cpu_ns = 0u64;
    for t in other_tids {
        let Some(sched) = thread_sched(process_pid, t) else { continue };
        scheduler::retire_task(&sched);
        let cpu_ns = scheduler::task_cpu_ns(&sched);
        let mut pdata = process_data_arc.lock();
        sched.handle.merge_into(&mut pdata.accounting);
        if t == main_tid {
            main_cpu_ns = cpu_ns;
        } else {
            pdata.accounting.child_threads_cpu_ns += cpu_ns;
        }
    }
    if tid == main_tid {
        main_cpu_ns = thread_sched(process_pid, tid)
            .map_or(0, |s| scheduler::task_cpu_ns(&s));
    }

    // Phase 3: free resources — no other thread of this process can run.
    let (syscall_total, syscall_total_ns) = teardown_resources(&process_data_arc, &thread_data_arc, process_pid);

    // Phase 4: table bookkeeping (zombie marks, orphan adoption, parent wake)
    let to_wake = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        teardown_bookkeeping(table, process_pid, code, main_cpu_ns)
    };

    // Phase 5: stash accounting on the parent, wake waiters, exit
    stash_accounting_snapshot(&process_data_arc, process_pid, parent_pid, syscall_total, syscall_total_ns, main_cpu_ns);

    // Table lock released — now safe to wake via scheduler
    for (pid, tid) in to_wake {
        scheduler::wake_task(TaskId(pid, tid));
    }

    scheduler::exit_current(code);
}

/// Exit the current thread. If this is the main thread, tears down the entire
/// process via `exit()`. For child threads, frees thread resources and zombifies.
pub fn thread_exit(code: i32) -> ! {
    let process_pid = current_process();
    let tid = current_tid();
    let is_main_thread = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();
        table.get(process_pid).unwrap().main_tid == tid
    };

    if is_main_thread {
        exit(code);
    }

    // Thread-only exit path: free TLS, zombify, wake parent
    unsafe { crate::mm::paging::kernel_cr3().activate(); }

    {
        let tdata_arc = current_data();
        let mut tdata = tdata_arc.lock();
        tdata.tls_pages.take();
    }
    {
        let owner_arc = fd_owner_data();
        let mut owner_data = owner_arc.lock();
        owner_data.elf.dynamic_tls_blocks.retain(|&(t, _), _| t != tid);
    }

    let parent_main_tid = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let cpu_ms = table.get(process_pid).and_then(|p| p.threads.get(tid))
            .and_then(|t| t.sched())
            .map_or(0, scheduler::task_cpu_ns) / 1_000_000;
        table.get_mut(process_pid).unwrap().threads.get_mut(tid).unwrap().state = ProcessState::Zombie(code);
        let proc = table.get(process_pid).unwrap();
        let name = proc.name_str();
        log!("exit: {name} tid={tid} code={code} cpu={cpu_ms}ms");
        proc.main_tid
    };

    scheduler::wake_task(TaskId(process_pid, parent_main_tid));
    scheduler::exit_current(code);
}

// ---------------------------------------------------------------------------
// Blocking / scheduling
// ---------------------------------------------------------------------------

/// A thread's scheduler record, cloned out of the table.
///
/// Cloning rather than borrowing is what keeps the wake and retire paths off
/// the table lock: both need the rendezvous word, and neither may hold a lock
/// while it posts.
pub fn thread_sched(pid: Pid, tid: Tid) -> Option<ThreadSched> {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref()?;
    table.get(pid)?.threads.get(tid)?.sched().cloned()
}

// ---------------------------------------------------------------------------
// Futex
// ---------------------------------------------------------------------------

/// Atomically check a user futex word and block if it matches the expected value.
/// Returns 0 if woken normally, 1 if timed out, u64::MAX on error.
pub fn futex_wait(addr: u64, expected: u32, timeout_ns: u64) -> u64 {
    let deadline = if timeout_ns != u64::MAX {
        crate::clock::nanos_since_boot().saturating_add(timeout_ns)
    } else {
        0
    };

    // Translate virtual → physical so cross-process futex works on shared memory
    let phys_addr = match scheduler::current_address_space()
        .and_then(|pt| pt.lock().translate(UserAddr::new(addr))) {
        Some(pa) => pa,
        None => return u64::MAX,
    };

    if scheduler::futex_wait(phys_addr, expected, deadline) {
        0 // blocked and woken
    } else {
        0 // value mismatch, returned immediately
    }
}

/// Wake up to `count` threads blocked on the same physical address as `addr`.
pub fn futex_wake(addr: u64, count: u64) -> u64 {
    let phys_addr = match scheduler::current_address_space()
        .and_then(|pt| pt.lock().translate(UserAddr::new(addr))) {
        Some(pa) => pa,
        None => return 0,
    };
    scheduler::futex_wake(phys_addr, count as usize)
}

// ---------------------------------------------------------------------------
// Pipe wake helpers
// ---------------------------------------------------------------------------

/// Wake processes blocked on reading from a pipe that now has data.
pub fn wake_pipe_readers(pipe_id: pipe::PipeId) {
    scheduler::wake_pipe_readers(pipe_id);
    // Also complete any io_uring pending polls watching this pipe
    let watchers = pipe::io_uring_watchers(pipe_id);
    if !watchers.is_empty() {
        crate::io_uring::complete_pending_for_event(
            &watchers,
            scheduler::EventSource::PipeReadable(pipe_id),
        );
    }
}

/// Wake processes blocked on writing to a pipe that now has space.
pub fn wake_pipe_writers(pipe_id: pipe::PipeId) {
    scheduler::wake_pipe_writers(pipe_id);
    // Also complete any io_uring pending polls watching this pipe
    let watchers = pipe::io_uring_watchers(pipe_id);
    if !watchers.is_empty() {
        crate::io_uring::complete_pending_for_event(
            &watchers,
            scheduler::EventSource::PipeWritable(pipe_id),
        );
    }
}

// ---------------------------------------------------------------------------
// Zombie collection
// ---------------------------------------------------------------------------

/// Atomically validate parent-child relationship and collect a zombie child process.
pub fn wait_child_zombie(child_pid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    collect_child_zombie(table, child_pid, parent_pid)
}

/// Atomically validate parent-thread relationship and collect a zombie thread.
pub fn wait_thread_zombie(tid: Tid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    collect_thread_zombie(table, tid, parent_pid)
}

// ---------------------------------------------------------------------------
// Demand paging
// ---------------------------------------------------------------------------

/// Handle a page fault at `fault_addr` by looking up the current process's VMAs.
/// Returns true if the fault was resolved (a page was mapped), false if fatal.
pub fn handle_page_fault(fault_addr: u64, _error_code: u64) -> bool {
    let t0 = crate::clock::nanos_since_boot();
    let tid = current_tid();
    if tid == Tid::MAX {
        //log!("handle_page_fault: no tid, fault_addr={:#x}", fault_addr);
        return false;
    }

    let (data_arc, addr_space) = {
        let Some(addr_space) = scheduler::current_address_space() else { return false };
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return false };
        let pid = current_process();
        let Some(proc) = table.get(pid) else { return false };
        let data = Arc::clone(&proc.process_data);
        (data, addr_space)
    };

    // Round down to 2MB boundary
    let page_2m = PAGE_2M;
    let region_start = fault_addr & !(page_2m - 1);
    let region_end_full = region_start.saturating_add(page_2m);

    // Collect region info from the address space (lock addr_space briefly).
    // We gather everything we need so we can drop the lock before doing I/O.
    struct RegionSnap {
        start: u64,
        end: u64,
        writable: bool,
        kind: RegionSnapKind,
    }
    enum RegionSnapKind {
        Anonymous,
        FileBacked { backing: Arc<dyn crate::file_backing::FileBacking>, file_offset: u64, file_size: u64 },
    }

    let (writable, regions) = {
        let as_guard = addr_space.lock();

        // Verify the fault address is within a valid region
        if as_guard.find_region(UserAddr::new(fault_addr)).is_none() {
            return false;
        }

        // If a 2MB page is already mapped at this region (from a previous fault
        // in a different VMA that shares the same 2MB range), just return success.
        if as_guard.translate(UserAddr::new(region_start)).is_some() {
            return true;
        }

        // Collect overlapping regions info
        let mut writable = false;
        let mut snaps = Vec::new();
        for (&start_addr, region) in as_guard.overlapping_regions(UserAddr::new(region_start), UserAddr::new(region_end_full)) {
            if region.writable { writable = true; }
            let snap_kind = match &region.kind {
                crate::vma::RegionKind::Anonymous => RegionSnapKind::Anonymous,
                crate::vma::RegionKind::FileBacked { backing, file_offset, file_size } => {
                    RegionSnapKind::FileBacked {
                        backing: Arc::clone(backing),
                        file_offset: *file_offset,
                        file_size: *file_size,
                    }
                }
                crate::vma::RegionKind::Mapped => RegionSnapKind::Anonymous, // already mapped eagerly
            };
            snaps.push(RegionSnap {
                start: start_addr.raw(),
                end: start_addr.raw() + region.size,
                writable: region.writable,
                kind: snap_kind,
            });
        }
        (writable, snaps)
    };

    let mut data = data_arc.lock();

    let reloc_index = data.elf.reloc_index.clone();
    let elf_base = data.elf.elf_base.raw();

    // Allocate a zeroed 2MB physical page
    let page_alloc = match PageAlloc::new(page_2m as usize, crate::mm::pmm::Category::DemandPage) {
        Some(a) => a,
        None => return false,
    };
    let page_ptr = page_alloc.ptr();

    // Fill the 2MB page from ALL regions that overlap this range.
    // Multiple segments (e.g. .text and .rodata) can share a 2MB range.
    let mut io_reads: u32 = 0;
    for region in &regions {
        match &region.kind {
            RegionSnapKind::Anonymous => {
                // Already zeroed by PageAlloc::new
            }
            RegionSnapKind::FileBacked { backing, file_offset, file_size } => {
                let fill_start = region_start.max(region.start);
                let fill_end = region_end_full.min(region.end);
                let mut vaddr = fill_start & !0xFFF;

                while vaddr < fill_end {
                    let vma_offset = vaddr - region.start;
                    let page_offset = (vaddr - region_start) as usize;

                    if vma_offset < *file_size {
                        let byte_offset = vma_offset + file_offset;
                        let mut page_buf = [0u8; 4096];
                        backing.read_page(byte_offset, &mut page_buf);
                        io_reads += 1;
                        let valid = if vma_offset + 4096 <= *file_size { 4096 } else { (*file_size - vma_offset) as usize };
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                page_buf.as_ptr(),
                                page_ptr.add(page_offset),
                                valid,
                            );
                        }
                    }
                    vaddr += 4096;
                }
            }
        }
    }


    // Apply relocations across the entire 2MB region
    let mut total_relocs = 0u16;
    if let Some(ref ri) = reloc_index {
        let mut offset = 0u64;
        while offset < page_2m {
            let page_elf_offset = (region_start + offset).wrapping_sub(elf_base);
            if ri.has_relocs_in_page(page_elf_offset) {
                total_relocs = total_relocs.saturating_add(
                    ri.apply_to_page(page_elf_offset, unsafe { page_ptr.add(offset as usize) }) as u16
                );
            }
            offset += 4096;
        }
    }


    // Map the 2MB page (writable if any overlapping VMA is writable)
    addr_space.lock().remap(UserAddr::new(region_start), page_alloc.phys(), writable);
    crate::mm::paging::invlpg(region_start);

    data.demand_pages.push(page_alloc);

    // Update memory tracking
    data.alloc_count += 1;
    let current_mem = data.demand_pages.len() as u64 * PAGE_2M;
    if current_mem > data.peak_memory {
        data.peak_memory = current_mem;
    }

    // Update fault accounting
    let fault_elapsed = crate::clock::nanos_since_boot() - t0;
    data.accounting.fault_ns += fault_elapsed;
    if io_reads > 0 {
        data.accounting.fault_demand_count += 1;
        data.accounting.io_read_ops += io_reads;
        data.accounting.io_read_bytes += io_reads as u64 * 4096;
    } else {
        data.accounting.fault_zero_count += 1;
    }

    // Record fault for crash diagnostics
    let elapsed_us = (fault_elapsed / 1000).min(u16::MAX as u64) as u16;
    data.fault_trace.record(PageFaultRecord {
        fault_addr,
        page_elf_offset: region_start.wrapping_sub(elf_base),
        block_idx: (region_start / PAGE_2M) as u32,
        reloc_count: total_relocs,
        flags: if writable { 1 } else { 0 },
        duration_us: elapsed_us,
    });

    true
}

// ---------------------------------------------------------------------------
// Crash diagnostics
// ---------------------------------------------------------------------------

/// Dump the page fault trace and memory around `fault_addr` for the current process.
/// Called from the exception handler on user-mode crashes.
pub fn dump_crash_diagnostics(fault_addr: u64, rip: u64) {
    let Some(pid) = percpu::current_pid() else { return };

    let data_arc = {
        let Some(guard) = PROCESS_TABLE.try_lock() else {
            log!("  [crash diagnostics: PROCESS_TABLE locked, skipping]");
            return;
        };
        let Some(table) = guard.as_ref() else { return };
        match table.get(pid) {
            Some(proc) => Arc::clone(&proc.process_data),
            None => return,
        }
    };
    let Some(data) = data_arc.try_lock() else {
        log!("  [crash diagnostics: ProcessData locked, skipping]");
        return;
    };

    // Dump page fault trace
    let trace = &data.fault_trace;
    let count = trace.total().min(32);
    if count > 0 {
        log!("  Page fault trace ({} total, last {}):", trace.total(), count);
        for rec in trace.iter_chronological() {
            if rec.fault_addr == 0 { continue; }
            let mut flag_str = [b' '; 4];
            if rec.flags & 1 != 0 { flag_str[0] = b'W'; } // writable
            if rec.flags & 2 != 0 { flag_str[1] = b'R'; } // has_relocs
            if rec.flags & 4 != 0 { flag_str[2] = b'A'; } // anonymous
            if rec.flags & 8 != 0 { flag_str[3] = b'Z'; } // beyond extent (zero)
            let flags = core::str::from_utf8(&flag_str).unwrap_or("????");
            log!("    fault={:#x} elf_off={:#x} blk={} relocs={} {}us [{}]",
                rec.fault_addr, rec.page_elf_offset, rec.block_idx,
                rec.reloc_count, rec.duration_us, flags);
        }
    }

    // Dump memory around given addresses (if mapped in the process page tables)
    let Some(addr_space) = scheduler::current_address_space() else { return };

    // Read a u64 from a user virtual address via page table translation.
    // Reads via the kernel direct map (no USER bit) to avoid SMAP faults.
    let read_user = |virt: u64| -> Option<u64> {
        if virt % 8 != 0 { return None; }
        let phys = addr_space.lock().translate(UserAddr::new(virt))?;
        Some(unsafe { *phys.as_ptr::<u64>() })
    };

    let dump_region = |label: &str, addr: u64| {
        if read_user(addr).is_none() { return; }
        let start = (addr & !0x7).saturating_sub(32);
        log!("  Memory around {} ({:#x}):", label, addr);
        for i in 0..8u64 {
            let a = start + i * 8;
            let Some(val) = read_user(a) else { break };
            let marker = if a == (addr & !0x7) { " <--" } else { "" };
            log!("    [{:#x}] = {:#018x}{}", a, val, marker);
        }
    };

    if fault_addr != 0 {
        dump_region("fault_addr", fault_addr);
    }
    dump_region("rip", rip);

    // Dump TLS self-pointer at FS base
    let fs_base_msr = crate::arch::cpu::rdfsbase();
    let fs_base = fs_base_msr;
    if fs_base != 0 {
        log!("  FS base: {:#x}", fs_base);
        if let Some(self_ptr) = read_user(fs_base) {
            log!("  fs:[0] = {:#x} (expected {:#x})", self_ptr, fs_base);
            // Dump 8 qwords at TP
            for i in 0..8u64 {
                let addr = fs_base + i * 8;
                let Some(val) = read_user(addr) else { break };
                log!("    TP+{:#x} = {:#018x}", i * 8, val);
            }
            // Also dump 4 qwords before TP (TLS data area)
            log!("  TLS data before TP:");
            for i in 1..=4u64 {
                let addr = fs_base - i * 8;
                let Some(val) = read_user(addr) else { break };
                log!("    TP-{:#x} = {:#018x}", i * 8, val);
            }
        } else {
            log!("  FS base {:#x} NOT MAPPED!", fs_base);
        }
        // TLS alloc info is in ThreadData; FS base dump above gives the relevant info.
    }
}

// ---------------------------------------------------------------------------
// Symbol resolution / address validation
// ---------------------------------------------------------------------------

/// Resolve and log a user-mode address against the process's symbol table.
/// Returns true if the address was resolved and logged.
/// Uses try_lock so it's safe to call from panic handlers.
pub fn resolve_user_symbol(pid: Pid, addr: u64) -> bool {
    let syms_arc = {
        let Some(guard) = PROCESS_TABLE.try_lock() else { return false };
        let Some(table) = guard.as_ref() else { return false };
        match table.get(pid) {
            Some(proc) => Arc::clone(&proc.symbols),
            None => return false,
        }
    };
    let Some(syms) = syms_arc.try_lock() else { return false };
    crate::symbols::resolve_user(&syms, addr)
}

/// Find .symtab and .strtab in an ELF's section headers and return pointers
/// into the initrd memory. No allocation — the sections are read in-place.
pub(crate) fn find_symtab_in_memory(
    backing: &dyn crate::file_backing::FileBacking,
    sh_off: u64, sh_num: usize, sh_entsize: usize,
    base: u64,
    prog_base: u64, prog_end: u64,
    stack_base: u64, stack_end: u64,
) -> SymbolTable {
    const SHT_SYMTAB: u32 = 2;
    const SHT_STRTAB: u32 = 3;
    let empty = || SymbolTable::empty_with_bounds(prog_base, prog_end, stack_base, stack_end);

    // Read section headers — they're small enough to read via read_file_range.
    let shdr_size = sh_num * sh_entsize;
    let shdr_data = read_file_range(backing, sh_off, shdr_size);

    // Find SHT_SYMTAB and its linked SHT_STRTAB.
    let mut symtab_off = 0u64;
    let mut symtab_size = 0u64;
    let mut symtab_entsize = 0u64;
    let mut symtab_link = 0u32;
    for i in 0..sh_num {
        let off = i * sh_entsize;
        if off + 64 > shdr_data.len() { break; }
        let sh_type = u32::from_le_bytes(shdr_data[off + 4..off + 8].try_into().unwrap());
        if sh_type == SHT_SYMTAB {
            symtab_off = u64::from_le_bytes(shdr_data[off + 24..off + 32].try_into().unwrap());
            symtab_size = u64::from_le_bytes(shdr_data[off + 32..off + 40].try_into().unwrap());
            symtab_link = u32::from_le_bytes(shdr_data[off + 40..off + 44].try_into().unwrap());
            symtab_entsize = u64::from_le_bytes(shdr_data[off + 56..off + 64].try_into().unwrap());
            break;
        }
    }
    if symtab_size == 0 { return empty(); }

    // Find the linked strtab.
    let link_off = symtab_link as usize * sh_entsize;
    if link_off + 64 > shdr_data.len() { return empty(); }
    let strtab_type = u32::from_le_bytes(shdr_data[link_off + 4..link_off + 8].try_into().unwrap());
    if strtab_type != SHT_STRTAB { return empty(); }
    let strtab_off = u64::from_le_bytes(shdr_data[link_off + 24..link_off + 32].try_into().unwrap());
    let strtab_size = u64::from_le_bytes(shdr_data[link_off + 32..link_off + 40].try_into().unwrap());

    // Get in-memory pointers (only works for initrd-backed files).
    let Some(symtab_ptr) = backing.memory_ptr(symtab_off, symtab_size as usize) else { return empty() };
    let Some(strtab_ptr) = backing.memory_ptr(strtab_off, strtab_size as usize) else { return empty() };

    let entry_size = if symtab_entsize > 0 { symtab_entsize as usize } else { 24 };
    let entries = symtab_size as usize / entry_size;

    SymbolTable::from_raw(
        symtab_ptr, entries,
        strtab_ptr, strtab_size as usize,
        base,
        prog_base, prog_end, stack_base, stack_end,
    )
}

// ---------------------------------------------------------------------------
// Kill
// ---------------------------------------------------------------------------

/// Kill a child process. Only the parent can kill its children.
/// Returns 0 on success, error code on failure.
pub fn kill_process(target_pid: Pid) -> u64 {
    use toyos_abi::syscall::SyscallError;
    let caller = current_process();

    // Phase 1: validate parentage and claim teardown (brief table lock)
    let (process_data_arc, thread_data_arc, main_tid, tids) = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();

        let Some(proc) = table.get_mut(target_pid) else { return SyscallError::NotFound.to_u64() };
        if proc.parent != Some(caller) {
            return SyscallError::PermissionDenied.to_u64();
        }
        if matches!(proc.state, ProcessState::Zombie(_)) || !proc.claim_teardown() {
            return 0; // already dead or dying
        }
        let tids: Vec<Tid> = proc.threads.iter().map(|(t, _)| t).collect();
        let main_thread = proc.threads.get(proc.main_tid).unwrap();
        (Arc::clone(&proc.process_data), Arc::clone(&main_thread.thread_data), proc.main_tid, tids)
    };

    // Phase 2: retire every thread. A running target is forced to a
    // scheduling boundary and dropped there — kill no longer refuses
    // running processes (the old WouldBlock check was a racy try_lock
    // snapshot; the retire loop is the reliable version of the same wait).
    let mut main_cpu_ns = 0u64;
    for t in tids {
        let Some(sched) = thread_sched(target_pid, t) else { continue };
        scheduler::retire_task(&sched);
        let cpu_ns = scheduler::task_cpu_ns(&sched);
        let mut pdata = process_data_arc.lock();
        sched.handle.merge_into(&mut pdata.accounting);
        if t == main_tid {
            main_cpu_ns = cpu_ns;
        } else {
            pdata.accounting.child_threads_cpu_ns += cpu_ns;
        }
    }

    // Phase 3: Resource cleanup (same path as exit)
    let (syscall_total, syscall_total_ns) = teardown_resources(&process_data_arc, &thread_data_arc, target_pid);

    // Phase 4: Table bookkeeping (same path as exit)
    let to_wake = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        teardown_bookkeeping(table, target_pid, 137, main_cpu_ns)
    };

    // Phase 5: Stash accounting snapshot on parent
    stash_accounting_snapshot(&process_data_arc, target_pid, Some(caller), syscall_total, syscall_total_ns, main_cpu_ns);

    // Phase 6: Wake parent if blocked on waitpid
    for (pid, tid) in to_wake {
        scheduler::wake_task(TaskId(pid, tid));
    }

    0
}

/// AP entry into the scheduler. Called from smp::ap_entry after SMP_READY.
pub fn ap_idle() -> ! {
    scheduler::enter_idle_loop();
}
