use alloc::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::naked_asm;
use core::ptr::NonNull;
use crate::arch::{cpu, percpu};
use crate::mm::PAGE_2M;
use crate::fd::{self, Descriptor, FdTable};
use crate::id_map::IdMap;
use crate::sync::Lock;
use crate::symbols::ProcessSymbols;
use crate::{elf, pipe, scheduler, shared_memory, vfs};
use crate::{DirectMap, UserAddr};

pub use toyos_abi::{Pid, Tid};
use toyos_abi::syscall::SyscallError;

impl crate::id_map::IdKey for Tid {
    const ZERO: Self = Tid(0);
    const ONE: Self = Tid(1);
}

impl crate::id_map::IdKey for Pid {
    const ZERO: Self = Pid(0);
    const ONE: Self = Pid(1);
}

/// Page tables shared between a process and all its threads.
pub type PageTables = Arc<Lock<crate::mm::paging::AddressSpace>>;

/// Allocate a VMA region and map physical memory into it.
/// Returns the allocated virtual address, or None if out of address space.
pub fn vma_map(
    vmas: &mut crate::vma::VmaList,
    pt: &Lock<crate::mm::paging::AddressSpace>,
    phys: u64,
    size: u64,
) -> Option<(UserAddr, u64)> {
    let aligned = crate::mm::align_2m(size as usize) as u64;
    let vaddr = vmas.alloc_region(aligned).ok()?;
    pt.lock().map_range(vaddr, phys, aligned, true);
    Some((vaddr, aligned))
}

// ---------------------------------------------------------------------------
// OwnedAlloc — RAII wrapper for page-aligned allocations
// ---------------------------------------------------------------------------

/// Typestate: allocation contains uninitialized memory.
pub enum Uninit {}
/// Typestate: allocation is fully initialized (safe to expose to userspace).
pub enum Zeroed {}

/// Move-only wrapper around a (`*mut u8`, `Layout`) pair.
/// `Drop` calls `dealloc`, so forgetting to free memory is a compile-time error
/// (you'd have to actively `mem::forget` it).
///
/// The typestate parameter `S` tracks initialization:
/// - `OwnedAlloc<Zeroed>` (default): all bytes are initialized. Can be mapped to userspace.
/// - `OwnedAlloc<Uninit>`: bytes may be uninitialized. Must call `assume_zeroed()` after
///   initializing all bytes before storing in ProcessData or mapping to userspace.
pub struct OwnedAlloc<S = Zeroed> {
    ptr: NonNull<u8>,
    layout: Layout,
    _state: core::marker::PhantomData<S>,
}

impl OwnedAlloc<Zeroed> {
    /// Allocate zeroed memory with the given size and alignment.
    /// Returns `None` if the allocator returns null.
    pub fn new(size: usize, align: usize) -> Option<Self> {
        let layout = Layout::from_size_align(size, align).ok()?;
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })?;
        Some(Self { ptr, layout, _state: core::marker::PhantomData })
    }
}

impl OwnedAlloc<Uninit> {
    /// Allocate uninitialized memory with the given size and alignment.
    /// The caller must initialize all bytes, then call `assume_zeroed()`.
    pub fn new_uninit(size: usize, align: usize) -> Option<Self> {
        let layout = Layout::from_size_align(size, align).ok()?;
        let ptr = NonNull::new(unsafe { alloc(layout) })?;
        Some(Self { ptr, layout, _state: core::marker::PhantomData })
    }

    /// Convert to `OwnedAlloc<Zeroed>` after the caller has initialized all bytes.
    ///
    /// # Safety
    /// All bytes in the allocation must have been written. The resulting
    /// allocation may be mapped into userspace.
    pub unsafe fn assume_zeroed(self) -> OwnedAlloc<Zeroed> {
        let ptr = self.ptr;
        let layout = self.layout;
        core::mem::forget(self);
        OwnedAlloc { ptr, layout, _state: core::marker::PhantomData }
    }
}

impl<S> OwnedAlloc<S> {
    pub fn ptr(&self) -> *mut u8 { self.ptr.as_ptr() }
    pub fn size(&self) -> usize { self.layout.size() }
}

impl<S> Drop for OwnedAlloc<S> {
    fn drop(&mut self) {
        // Check for PML4 canary in every 4KB page being freed.
        // If a page contains the canary, this dealloc would corrupt a live PML4.
        let page_count = (self.layout.size() + 4095) / 4096;
        for i in 0..page_count {
            let page = unsafe { self.ptr.as_ptr().add(i * 4096) };
            let slot_255 = unsafe { *(page.add(255 * 8) as *const u64) };
            if slot_255 & 0xFFFF_FFFF_0000_0000 == 0xCAFE_BABE_0000_0000 {
                let phys = page as u64 - crate::PHYS_OFFSET;
                panic!(
                    "OwnedAlloc::drop: freeing page at phys {:#x} that contains PML4 canary {:#x}! \
                     alloc ptr={:p} size={:#x}",
                    phys, slot_255, self.ptr.as_ptr(), self.layout.size()
                );
            }
        }
        unsafe { dealloc(self.ptr.as_ptr(), self.layout); }
    }
}

// OwnedAlloc is Send — the underlying allocation is just raw memory.
unsafe impl<S> Send for OwnedAlloc<S> {}

const USER_STACK_SIZE: usize = 4 * PAGE_2M as usize; // 8 MB
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

/// Where a thread is in the system. Stored in the thread table (SchedEntry).
///
/// The thread table tracks alive vs zombie. For alive threads, the scheduler
/// is authoritative about whether they're running, ready, or blocked —
/// query `scheduler::thread_sched_state()` for that detail.
///
/// A thread is ready iff it is in a CpuRunQueue. This is enforced structurally
/// in the scheduler — there is no way to represent "ready but not in a queue."
#[derive(Clone, Copy, PartialEq)]
pub enum ThreadLocation {
    /// Thread is alive: running, ready, or blocked. The scheduler owns the detail.
    Scheduled,
    /// Thread has exited with the given code. Waiting for parent to reap.
    Zombie(i32),
}

pub type ProcessState = ThreadLocation;

/// Proof that a process was zombified and its orphaned children must be handled.
/// Returned by `SchedEntry::zombify_process`, consumed by `ProcessTable::handle_orphans`.
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
/// Thread table entry — identity and state, protected by PROCESS_TABLE lock.
/// Context switch data (kernel_rsp, address_space, etc.) lives in ThreadCtx
/// which is owned by the scheduler.
pub struct SchedEntry {
    tid: Tid,
    process: Pid,
    state: ProcessState,
    kind: Kind,
    name: [u8; 28],
    memory_size: u64,
    data: Arc<Lock<ProcessData>>,
}

impl SchedEntry {
    fn new(
        tid: Tid,
        process: Pid,
        state: ProcessState,
        kind: Kind,
        name: [u8; 28],
        memory_size: u64,
        data: Arc<Lock<ProcessData>>,
    ) -> Self {
        Self { tid, process, state, kind, name, memory_size, data }
    }

    pub fn tid(&self) -> Tid { self.tid }
    pub fn process(&self) -> Pid { self.process }
    pub fn state(&self) -> &ProcessState { &self.state }
    pub fn kind(&self) -> &Kind { &self.kind }
    pub fn name(&self) -> &[u8; 28] { &self.name }
    pub fn memory_size(&self) -> u64 { self.memory_size }
    pub fn data(&self) -> &Arc<Lock<ProcessData>> { &self.data }

    /// CPU time in nanoseconds, retrieved from the scheduler's ThreadCtx.
    pub fn cpu_ns(&self) -> u64 {
        scheduler::thread_cpu_ns(self.tid)
    }

    // --- State machine ---

    /// Zombify a thread. Panics if called on a process or already a zombie.
    pub fn zombify_thread(&mut self, code: i32) {
        assert!(matches!(self.kind, Kind::Thread { .. }),
            "zombify_thread: tid={} is not a thread", self.tid);
        assert!(!matches!(self.state, ProcessState::Zombie(_)),
            "double zombify tid={}", self.tid);
        self.state = ProcessState::Zombie(code);
    }

    /// Zombify a process. Returns an `OrphanCleanup` token that must be consumed
    /// by `ProcessTable::handle_orphans` to collect orphaned children.
    pub fn zombify_process(&mut self, code: i32) -> OrphanCleanup {
        assert!(matches!(self.kind, Kind::Process { .. }),
            "zombify_process: tid={} is not a process", self.tid);
        assert!(!matches!(self.state, ProcessState::Zombie(_)),
            "double zombify tid={}", self.tid);
        self.state = ProcessState::Zombie(code);
        OrphanCleanup(self.process)
    }

    pub fn detach_from_parent(&mut self) {
        assert!(matches!(self.kind, Kind::Process { parent: Some(_) }),
            "detach_from_parent: tid={} is not a parented process", self.tid);
        self.kind = Kind::Process { parent: None };
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

/// Per-process data independently lockable via `Arc<Lock<ProcessData>>`.
/// Syscalls clone the Arc from SchedEntry, drop the table lock, then lock this.
pub struct ProcessData {
    pub pid: Pid,
    pub fds: FdTable,
    pub cwd: String,

    pub elf_alloc: Option<OwnedAlloc>,
    pub stack_alloc: Option<OwnedAlloc>,
    // Thread-local storage
    pub tls_template: Option<crate::mm::KernelSlice>,
    pub tls_memsz: usize,
    pub tls_alloc: Option<OwnedAlloc>,
    /// Multi-module TLS layout per loaded library.
    pub tls_modules: Vec<crate::elf::TlsModule>,
    /// Total combined TLS size across all modules.
    pub tls_total_memsz: usize,
    /// Maximum TLS alignment across all modules.
    pub tls_max_align: usize,
    /// Next module ID to assign on dlopen (1-based, exe=1).
    pub next_tls_module_id: u64,
    /// Dynamically allocated TLS blocks for dlopen'd modules (keyed by module_id).
    /// Dynamically allocated TLS blocks for dlopen'd modules, keyed by (thread Tid, module_id).
    /// Stored in the process-level (fd-owner) data so the VMA and backing memory have the same lifetime.
    pub dynamic_tls_blocks: alloc::collections::BTreeMap<(Tid, u64), OwnedAlloc>,
    // Crash diagnostics
    pub symbols: ProcessSymbols,
    // Dynamically loaded shared libraries (indexed by dlopen handle)
    pub loaded_libs: Vec<elf::LoadedLib>,
    // Anonymous memory mappings (mmap)
    pub mmap_regions: Vec<MmapRegion>,
    // User stack location (for SYS_STACK_INFO)
    pub user_stack_base: UserAddr,
    pub user_stack_size: u64,
    /// Inherited environment variables (KEY=VALUE\0KEY2=VALUE2\0...)
    pub env: Vec<u8>,
    /// Syscall counts per syscall number (for profiling)
    pub syscall_counts: [u32; 64],
    pub syscall_total: u64,
    /// Wall-clock nanoseconds spent in syscall dispatch (includes preemption time)
    pub syscall_total_ns: u64,
    /// Virtual memory areas for demand paging.
    pub vmas: crate::vma::VmaList,
    /// 2MB allocations for demand-paged pages. Freed on process exit.
    pub demand_allocs: Vec<OwnedAlloc>,
    /// RELATIVE relocation index for demand-paged ELF (applied per-page on fault).
    pub reloc_index: Option<Arc<elf::RelocationIndex>>,
    /// Runtime base address for the demand-paged ELF (for relocation computation).
    pub elf_base: UserAddr,
    /// Ring buffer of recent page faults for crash diagnostics.
    pub fault_trace: PageFaultTrace,
    /// Peak memory usage in bytes (high-water mark)
    pub peak_memory: u64,
    /// Total allocations (demand pages + mmap + TLS blocks)
    pub alloc_count: u64,
    /// Total frees (munmap)
    pub free_count: u64,
}

pub struct MmapRegion {
    pub addr: UserAddr,
    pub size: usize,
    pub _alloc: OwnedAlloc,
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
    procs: IdMap<Tid, SchedEntry>,
    next_pid: Pid,
}

impl ProcessTable {
    fn new() -> Self {
        Self { procs: IdMap::new(), next_pid: Pid(0) }
    }

    /// Allocate a new Pid for a process.
    pub fn alloc_pid(&mut self) -> Pid {
        let pid = self.next_pid;
        self.next_pid = Pid(pid.0 + 1);
        pid
    }

    // --- Passthrough accessors ---

    pub fn insert_with(&mut self, f: impl FnOnce(Tid) -> SchedEntry) -> Tid {
        self.procs.insert_with(f)
    }

    pub fn get(&self, tid: Tid) -> Option<&SchedEntry> {
        self.procs.get(tid)
    }

    pub fn get_mut(&mut self, tid: Tid) -> Option<&mut SchedEntry> {
        self.procs.get_mut(tid)
    }

    /// Get the main thread entry for a process Pid.
    pub fn get_process(&self, process: Pid) -> Option<&SchedEntry> {
        self.find_main_thread(process).and_then(|tid| self.procs.get(tid))
    }

    pub fn iter(&self) -> impl Iterator<Item = (Tid, &SchedEntry)> {
        self.procs.iter()
    }

    /// Find the main thread Tid for a given process Pid.
    pub fn find_main_thread(&self, process: Pid) -> Option<Tid> {
        self.procs.iter()
            .find(|(_, e)| e.process == process && matches!(e.kind, Kind::Process { .. }))
            .map(|(tid, _)| tid)
    }

    // --- Safe removal methods (each validates preconditions) ---

    /// Waitpid: collect a zombie child process by Pid. Validates parent relationship.
    pub fn collect_child_zombie(&mut self, child_pid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
        let tid = self.find_main_thread(child_pid).ok_or(())?;
        let entry = self.procs.get(tid).unwrap();
        if !matches!(entry.kind, Kind::Process { parent: Some(ppid) } if ppid == parent_pid) {
            return Err(());
        }
        if let ProcessState::Zombie(code) = entry.state {
            self.procs.remove(tid);
            Ok(Some(code))
        } else {
            Ok(None)
        }
    }

    /// Thread join: collect a zombie thread. Validates parent relationship.
    pub fn collect_thread_zombie(&mut self, tid: Tid, parent_pid: Pid) -> Result<Option<i32>, ()> {
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
    fn remove_orphan_zombie_child(&mut self, child_tid: Tid) {
        let entry = self.procs.get(child_tid).expect("remove_orphan_zombie_child: child not found");
        assert!(matches!(entry.state, ProcessState::Zombie(_)),
            "remove_orphan_zombie_child: tid={} is not a zombie (state={})", child_tid, entry.state.name());
        self.procs.remove(child_tid);
    }

    /// Handle orphaned children of a just-zombified process.
    /// Consumes the `OrphanCleanup` token, ensuring this step is never skipped.
    /// Zombie children are removed; running children are detached (become orphans).
    pub fn handle_orphans(&mut self, cleanup: OrphanCleanup) {
        let pid = cleanup.0;
        let orphan_tids: Vec<Tid> = self.procs.iter()
            .filter(|(_, e)| matches!(e.kind(), Kind::Process { parent: Some(ppid) } if *ppid == pid))
            .map(|(tid, _)| tid)
            .collect();
        for tid in orphan_tids {
            if matches!(self.get(tid).unwrap().state(), ProcessState::Zombie(_)) {
                self.remove_orphan_zombie_child(tid);
            } else {
                self.get_mut(tid).unwrap().detach_from_parent();
            }
        }
    }

    /// Sweep all reclaimable zombies. Requires `IdleProof` — only callable from
    /// the idle loop, which runs on the per-CPU idle stack (safe to drop kernel stacks).
    pub fn collect_orphan_zombies(&mut self, _proof: IdleProof) {
        // First pass: find zombie process pids (for thread collection)
        let zombie_pids: Vec<Pid> = self.procs.iter()
            .filter(|(_, e)| matches!(e.state, ProcessState::Zombie(_)) && matches!(e.kind, Kind::Process { .. }))
            .map(|(_, e)| e.process)
            .collect();

        // Second pass: collect reclaimable zombies
        let orphans: Vec<Tid> = self.procs.iter()
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
            .map(|(tid, _)| tid)
            .collect();
        for tid in orphans {
            self.procs.remove(tid);
        }
    }
}

pub static PROCESS_TABLE: Lock<Option<ProcessTable>> = Lock::new(None);

pub fn init() {
    *PROCESS_TABLE.lock() = Some(ProcessTable::new());
}

pub fn current_tid() -> Tid {
    percpu::current_tid().expect("current_tid() called during idle (no thread running)")
}

pub fn current_process() -> Pid {
    with_current_sched(|s| s.process())
}

// ---------------------------------------------------------------------------
// Access patterns — SchedEntry (brief table lock)
// ---------------------------------------------------------------------------

/// Access the current process's SchedEntry immutably (table lock held for closure).
pub fn with_current_sched<R>(f: impl FnOnce(&SchedEntry) -> R) -> R {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    f(table.get(current_tid()).unwrap())
}

pub fn current_address_space() -> PageTables {
    scheduler::current_address_space().expect("current_address_space: no address space")
}

// ---------------------------------------------------------------------------
// Access patterns — ProcessData (clone Arc, drop table lock, lock ProcessData)
// ---------------------------------------------------------------------------

/// Get the current process's ProcessData Arc (brief table lock).
/// If the entry is gone (process killed while thread was running), exits silently.
pub fn current_data() -> Arc<Lock<ProcessData>> {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    match table.get(current_tid()) {
        Some(entry) => Arc::clone(entry.data()),
        None => {
            drop(guard);
            scheduler::exit_current(-1);
        }
    }
}

/// Get the FD/heap owner's ProcessData Arc (brief table lock).
/// For processes this is self; for threads it's the parent process.
pub fn fd_owner_data() -> Arc<Lock<ProcessData>> {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    let tid = current_tid();
    let process_pid = table.get(tid).unwrap().process();
    Arc::clone(table.get_process(process_pid).unwrap().data())
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
pub fn setup_tls(tls_template: Option<crate::mm::KernelSlice>, tls_memsz: usize, tls_align: usize) -> Option<(OwnedAlloc, u64)> {
    setup_combined_tls(&[elf::TlsModule { template: tls_template, memsz: tls_memsz, base_offset: 0, module_id: 1, is_static: true }], tls_memsz, tls_align)
}

/// Allocate a combined TLS area for multiple modules (exe + shared libraries).
/// Each module's template is copied at its base_offset within the block.
///
/// x86-64 TLS Variant II layout:
///   [DTV] [alignment padding] [TLS data (.tdata + .tbss)] [TCB (64 bytes)]
///                                                          ^-- TP (FS base)
///
/// The linker (LLD) computes TPOFF = sym_offset - memsz (raw, NOT rounded).
/// TP must be placed at data_start + memsz to match.
/// data_start must be aligned to tls_align so variable offsets work correctly.
///
/// TCB layout:
///   TP+0x00: self-pointer (fs:[0] == &TCB, x86_64 ABI requirement)
///   TP+0x08: DTV pointer (user-visible physical address of DTV)
///   TP+0x10..0x3F: reserved (zero)
///
/// DTV layout (at start of allocation):
///   [0x00] generation: u64
///   [0x08] len: u64 (max module_id this DTV can hold)
///   [0x10] entries[0]: u64 (pointer for module_id=1)
///   [0x18] entries[1]: u64 (pointer for module_id=2)
///   ...
const TCB_SIZE: usize = 64;
/// Initial DTV capacity (number of module entries).
const DTV_INITIAL_CAPACITY: usize = 64;
/// Header size: generation (8) + len (8).
const DTV_HEADER_SIZE: usize = 16;
/// Sentinel value for unallocated DTV entries.
const DTV_UNALLOCATED: u64 = !0u64;

pub fn setup_combined_tls(
    modules: &[crate::elf::TlsModule],
    total_memsz: usize,
    tls_align: usize,
) -> Option<(OwnedAlloc, u64)> {
    let block_size = total_memsz + TCB_SIZE;
    let alloc_size = crate::mm::align_2m(block_size + tls_align);
    let uninit_alloc = OwnedAlloc::new_uninit(alloc_size, PAGE_2M as usize)?;
    let block = uninit_alloc.ptr();

    // Place TLS data near the end of the allocation (DTV at start, TLS after).
    // Align tls_start so that data_start (= block + tls_start) has tls_align alignment.
    let align = if tls_align > 1 { tls_align } else { 8 };
    let tls_start = (alloc_size - block_size) & !(align - 1);

    // Zero the entire allocation (DTV area, gap, TLS block, TCB).
    unsafe { core::ptr::write_bytes(block, 0, alloc_size); }

    for module in modules {
        if !module.is_static { continue; }
        if let Some(template) = &module.template {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    template.base(),
                    block.add(tls_start + module.base_offset),
                    template.size(),
                );
            }
        }
    }

    // TP must be a user-visible physical address (mapped with USER bit in user page tables).
    let block_phys = DirectMap::from_ptr(block).phys();
    let tp_user = block_phys + (tls_start + total_memsz) as u64;
    // Write self-pointer via kernel direct map
    let tp_kernel = block as u64 + (tls_start + total_memsz) as u64;
    unsafe { *(tp_kernel as *mut u64) = tp_user; }

    // Set up DTV at the start of the allocation.
    // DTV entries point to the start of each module's TLS data (user-visible addresses).
    let dtv_size = DTV_HEADER_SIZE + DTV_INITIAL_CAPACITY * 8;
    assert!(dtv_size < tls_start, "DTV overlaps TLS data");
    let dtv_kern = block as *mut u64;
    unsafe {
        // generation = 1 (initial)
        *dtv_kern = 1;
        // len = DTV_INITIAL_CAPACITY
        *dtv_kern.add(1) = DTV_INITIAL_CAPACITY as u64;
        // Initialize all entries as unallocated
        for i in 0..DTV_INITIAL_CAPACITY {
            *dtv_kern.add(2 + i) = DTV_UNALLOCATED;
        }
        // Fill entries for static modules only: dtv[module_id - 1] = user addr of module's TLS data.
        // Dynamic modules (dlopen'd) stay DTV_UNALLOCATED — allocated on first access.
        for module in modules {
            if !module.is_static { continue; }
            let idx = module.module_id as usize;
            if idx > 0 && idx <= DTV_INITIAL_CAPACITY {
                let module_tls_addr = block_phys + (tls_start + module.base_offset) as u64;
                *dtv_kern.add(2 + idx - 1) = module_tls_addr;
            }
        }
    }

    // Write DTV pointer to TCB[8] (user-visible physical address of DTV)
    let dtv_user = block_phys;
    unsafe { *((tp_kernel + 8) as *mut u64) = dtv_user; }

    // SAFETY: all bytes have been written (zeroed + DTV + TLS templates + TCB pointers).
    let alloc = unsafe { uninit_alloc.assume_zeroed() };
    Some((alloc, tp_user))
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
    scheduler::write_stack_canary(&alloc);
    let top = alloc.ptr() as u64 + KERNEL_STACK_SIZE as u64;
    // Must match context_switch layout: pushfq, push rbp..r15 (8 values) + return address
    let frame = (top - 8 * 8) as *mut u64;
    unsafe {
        *frame.add(0) = 0;                    // r15
        *frame.add(1) = arg;                  // r14
        *frame.add(2) = user_sp;              // r13
        *frame.add(3) = user_entry;           // r12
        *frame.add(4) = 0;                    // rbx
        *frame.add(5) = 0;                    // rbp
        *frame.add(6) = 0x002;                // RFLAGS (IF=0, AC=0)
        *frame.add(7) = trampoline as u64;    // return address
    }
    Some((alloc, frame as u64))
}

/// Release the CPU queue lock held across context_switch.
/// Called by process_start/thread_start before entering userspace.
fn scheduler_unlock() {
    unsafe { scheduler::force_unlock_current_cpu(); }
    scheduler::handle_outgoing_public();
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
pub fn spawn_thread(entry: u64, stack_ptr: u64, arg: u64, stack_base: u64) -> Option<Tid> {
    // Phase 1: Get parent's data + address space (never held simultaneously)
    let (parent_addr_space, parent_process, data_arc) = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();
        let parent = table.get(current_tid()).unwrap();
        let addr_space = scheduler::current_address_space();
        (addr_space, parent.process(), Arc::clone(parent.data()))
    };
    let (tls_template, tls_memsz, tls_modules, tls_total_memsz, tls_max_align, parent_cwd) = {
        let data = data_arc.lock();
        (data.tls_template, data.tls_memsz,
         data.tls_modules.clone(), data.tls_total_memsz, data.tls_max_align, data.cwd.clone())
    };

    // Phase 2: Allocate TLS (outside any lock)
    let (tls_alloc, fs_base) = if !tls_modules.is_empty() {
        setup_combined_tls(&tls_modules, tls_total_memsz, tls_max_align)?
    } else {
        setup_tls(tls_template, tls_memsz, tls_max_align)?
    };
    let fs_base = {
        let addr_space = parent_addr_space.as_ref().expect("spawn_thread: no address space");
        let mut parent_data = data_arc.lock();
        let tls_phys = DirectMap::phys_of(tls_alloc.ptr());
        let (tls_vaddr, _) = vma_map(&mut parent_data.vmas, addr_space, tls_phys, tls_alloc.size() as u64)
            .expect("spawn_thread: out of virtual address space");
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
    let thread_data = Arc::new(Lock::new(ProcessData {
        pid: parent_process,
        fds: FdTable::new(),

        cwd: parent_cwd,
        elf_alloc: None,
        stack_alloc: None,
        tls_template,
        tls_memsz,
        tls_alloc: Some(tls_alloc),
        tls_modules,
        tls_total_memsz,
        tls_max_align,
        next_tls_module_id: 2,
        dynamic_tls_blocks: alloc::collections::BTreeMap::new(),
        symbols: ProcessSymbols::empty(),
        loaded_libs: Vec::new(),
        mmap_regions: Vec::new(),
        user_stack_base: UserAddr::new(stack_base),
        user_stack_size: if stack_base > 0 { stack_ptr - stack_base } else { 0 },
        env: Vec::new(),
        syscall_counts: [0; 64],
        syscall_total: 0,
        syscall_total_ns: 0,
        vmas: crate::vma::VmaList::new(),
        demand_allocs: Vec::new(),
        reloc_index: None,
        elf_base: UserAddr::new(0),
        fault_trace: PageFaultTrace::new(),
        peak_memory: 0,
        alloc_count: 0,
        free_count: 0,
    }));

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let tid = table.insert_with(|tid| {
        SchedEntry::new(
            tid, parent_process, ThreadLocation::Scheduled, Kind::Thread { parent: parent_process },
            [0; 28], 0, thread_data,
        )
    });
    drop(guard);

    let ctx = scheduler::ThreadCtx {
        tid,
        process: parent_process,
        kernel_stack: ks_alloc,
        kernel_rsp: ks_rsp,
        address_space: parent_addr_space,
        fs_base,
        cpu_ns: 0,
        scheduled_at: 0,
        blocked_on: None,
        deadline: 0,
        blocked_since: 0,
    };
    scheduler::enqueue_new(ctx);

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
            let cloned = desc.clone();
            fd::alloc_at(&mut fds, child_fd, cloned);
        }
    }
    fds
}

/// User virtual address space starts at 1TB — well above any direct-mapped physical RAM.
const USER_VM_BASE: u64 = 0x100_0000_0000;

/// Convert an ELF virtual address to a file offset by searching PT_LOAD segments.
/// Falls back to extrapolating from the nearest segment for vaddrs outside all segments
/// (e.g. `.rela.dyn` sections the linker places outside PT_LOAD).
fn vaddr_to_file_offset(segments: &[elf::ElfSegment], vaddr: u64) -> u64 {
    for seg in segments {
        if vaddr >= seg.vaddr && vaddr < seg.vaddr + seg.filesz {
            return seg.file_offset + (vaddr - seg.vaddr);
        }
    }
    // Extrapolate from the nearest segment below this vaddr.
    // Works for PIE binaries where file_offset == vaddr (common pattern).
    let mut best: Option<&elf::ElfSegment> = None;
    for seg in segments {
        if seg.vaddr <= vaddr {
            if best.map_or(true, |b| seg.vaddr > b.vaddr) {
                best = Some(seg);
            }
        }
    }
    match best {
        Some(seg) => seg.file_offset + (vaddr - seg.vaddr),
        None => panic!("vaddr_to_file_offset: {:#x} not in or near any PT_LOAD segment", vaddr),
    }
}

/// Read a byte range from a file using its block map via the page cache.
pub(crate) fn read_file_range(backing: &dyn crate::file_backing::FileBacking, offset: u64, len: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(len);
    let mut remaining = len;
    let mut file_off = offset;
    let mut page_buf = [0u8; 4096];

    while remaining > 0 {
        let off_in_block = (file_off % 4096) as usize;
        let chunk = (4096 - off_in_block).min(remaining);

        backing.read_page(file_off - off_in_block as u64, &mut page_buf);
        result.extend_from_slice(&page_buf[off_in_block..off_in_block + chunk]);

        file_off += chunk as u64;
        remaining -= chunk;
    }

    result
}

/// Resolve a single exe TPOFF relocation entry to a pre-computed i64 value.
/// Handles both r_sym == 0 (simple offset) and r_sym != 0 (cross-library lookup).
fn resolve_exe_tpoff(
    r_sym: u32,
    r_addend: i64,
    exe_base_offset: usize,
    total_memsz: usize,
    segments: &[elf::ElfSegment],
    symtab_vaddr: u64,
    backing: &dyn crate::file_backing::FileBacking,
    dynstr_data: &[u8],
    tls_info: &elf::TlsModuleInfo,
) -> i64 {
    if r_sym == 0 {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    }

    let symtab_file_off = vaddr_to_file_offset(segments, symtab_vaddr);
    let sym_data = read_file_range(backing, symtab_file_off + r_sym as u64 * elf::SYM_SIZE as u64, elf::SYM_SIZE);
    if sym_data.len() < elf::SYM_SIZE {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    }
    let sym = elf::read_sym(&sym_data, 0);

    if sym.st_shndx != 0 {
        exe_base_offset as i64 + sym.st_value as i64 + r_addend - total_memsz as i64
    } else {
        let sym_name = elf::sym_name(&sym, dynstr_data);

        // Search loaded libraries for the defining TLS symbol
        for lib in tls_info.libs {
            if lib.tls_memsz == 0 { continue; }
            if let Some(sym_tls_offset) = elf::tls_dlsym_pub(lib, sym_name) {
                let other_base_offset = tls_info.modules.iter()
                    .find(|m| m.template == lib.tls_template)
                    .map(|m| m.base_offset)
                    .unwrap_or(0);
                return other_base_offset as i64 + sym_tls_offset as i64 - total_memsz as i64;
            }
        }
        log!("tpoff: unresolved exe TLS symbol: {}", sym_name);
        0
    }
}

/// Spawn a new process from an ELF binary using demand paging.
/// Only reads ELF headers and metadata from disk — PT_LOAD segments are faulted in on access.
/// Build VMAs for each PT_LOAD segment in the ELF layout.
fn build_vmas(layout: &elf::ElfLayout, base: u64, backing: &Arc<dyn crate::file_backing::FileBacking>) -> crate::vma::VmaList {
    let mut vmas = crate::vma::VmaList::new();
    for seg in &layout.segments {
        let seg_start = (base + seg.vaddr) & !0xFFF;
        let seg_end = (base + seg.vaddr + seg.memsz + 0xFFF) & !0xFFF;

        let file_block_start = seg.file_offset / 4096;
        let file_blocks_needed = ((seg.filesz + (seg.file_offset % 4096) + 4095) / 4096) as usize;
        let file_backed_end = seg_start + file_blocks_needed as u64 * 4096;

        if file_blocks_needed > 0 && file_backed_end > seg_start {
            vmas.insert(crate::vma::Vma {
                start: UserAddr::new(seg_start),
                end: UserAddr::new(file_backed_end.min(seg_end)),
                writable: seg.writable,
                kind: crate::vma::VmaKind::FileBacked {
                    backing: Arc::clone(backing),
                    file_offset: file_block_start * 4096,
                    file_size: seg.filesz + (seg.file_offset % 4096),
                },
            });
        }

        if file_backed_end < seg_end {
            vmas.insert(crate::vma::Vma {
                start: UserAddr::new(file_backed_end.max(seg_start)),
                end: UserAddr::new(seg_end),
                writable: seg.writable,
                kind: crate::vma::VmaKind::Anonymous,
            });
        }
    }
    vmas
}

/// Build TLS module layout from loaded shared libraries and the exe's TLS segment.
fn build_tls_layout(
    loaded_libs: &[elf::LoadedLib],
    layout: &elf::ElfLayout,
    exe_tls_template: Option<&OwnedAlloc>,
) -> (Vec<elf::TlsModule>, usize, usize, u64) {
    let mut modules = Vec::new();
    let mut cursor = 0usize;
    let mut max_align = 1usize;
    // Module ID 1 = exe, 2+ = shared libs. Libs are laid out first in the block,
    // then the exe. Module IDs are assigned in layout order (libs first).
    let mut next_module_id = 2u64; // 1 reserved for exe

    for lib in loaded_libs {
        if lib.tls_memsz > 0 {
            if cursor > 0 { cursor = (cursor + 15) & !15; }
            let mid = next_module_id;
            next_module_id += 1;
            modules.push(elf::TlsModule {
                template: lib.tls_template,
                memsz: lib.tls_memsz, base_offset: cursor, module_id: mid,
                is_static: true,
            });
            cursor += lib.tls_memsz;
            if lib.tls_align > max_align { max_align = lib.tls_align; }
        }
    }

    if layout.tls_memsz > 0 {
        if cursor > 0 { cursor = (cursor + 15) & !15; }
        let template = exe_tls_template
            .map(|buf| unsafe { crate::mm::KernelSlice::from_raw(buf.ptr(), layout.tls_filesz) });
        modules.push(elf::TlsModule {
            template,
            memsz: layout.tls_memsz, base_offset: cursor, module_id: 1,
            is_static: true,
        });
        cursor += layout.tls_memsz;
        if layout.tls_align > max_align { max_align = layout.tls_align; }
    }

    (modules, cursor, max_align, next_module_id)
}

pub fn spawn(argv: &[&str], fds: FdTable, parent: Option<Pid>, env: Vec<u8>) -> Result<Pid, SyscallError> {
    let path = argv[0];
    let t0 = crate::clock::nanos_since_boot();

    // 1. Open file backing from VFS (follows symlinks)
    let backing: Arc<dyn crate::file_backing::FileBacking> = match vfs::lock().open_backing(path) {
        Some(b) => b,
        None => {
            log!("spawn: {}: not found", path);
            return Err(SyscallError::NotFound);
        }
    };

    // 2. Read first few blocks for ELF headers
    let header_size = 4096.min(backing.file_size() as usize);
    let header_data = read_file_range(backing.as_ref(), 0, header_size);

    // 3. Parse ELF layout from headers
    let layout = match elf::parse_layout(&header_data) {
        Ok(l) => l,
        Err(msg) => {
            log!("spawn: {}: {}", path, msg);
            return Err(SyscallError::InvalidArgument);
        }
    };

    // 3b. Parse PT_DYNAMIC from block map (not available in the header buffer)
    let dyn_info = if let Some((dyn_off, dyn_size)) = layout.dynamic {
        let dyn_data = read_file_range(backing.as_ref(), dyn_off, dyn_size as usize);
        elf::parse_dynamic(&dyn_data)
    } else {
        elf::DynamicInfo::empty()
    };

    let t1 = crate::clock::nanos_since_boot();

    // 4. Choose base address in user virtual space
    let base = USER_VM_BASE - layout.vaddr_min;

    // 5. Create VMAs for each PT_LOAD segment
    let mut vmas = build_vmas(&layout, base, &backing);

    // 6. Read and parse relocation tables from block map
    let rela_data = if dyn_info.rela_size > 0 {
        let rela_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.rela_vaddr);
        read_file_range(backing.as_ref(), rela_file_off, dyn_info.rela_size as usize)
    } else if layout.dynamic.is_none() {
        // No PT_DYNAMIC — fall back to finding .rela.dyn from section headers
        if let Some((shoff, shnum, shentsize)) = layout.section_headers {
            let shdr_data = read_file_range(backing.as_ref(), shoff, shnum as usize * shentsize as usize);
            let bk = backing.as_ref();
            if let Some((rela_off, rela_size)) = elf::find_rela_dyn_from_sections(
                &shdr_data, shentsize, &|off, len| read_file_range(bk, off, len),
            ) {
                read_file_range(backing.as_ref(), rela_off, rela_size as usize)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let jmprel_data = if dyn_info.jmprel_size > 0 {
        let jmprel_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.jmprel_vaddr);
        read_file_range(backing.as_ref(), jmprel_file_off, dyn_info.jmprel_size as usize)
    } else {
        Vec::new()
    };
    let parsed_relas = elf::parse_rela_entries(&rela_data, &jmprel_data);

    // Start building the relocation index with RELATIVE entries (pre-computed: base + addend)
    let mut reloc_index = elf::RelocationIndex::new();
    for &(r_offset, r_addend) in &parsed_relas.relative {
        reloc_index.add_u64(r_offset, (base as i64 + r_addend) as u64);
    }

    let t2 = crate::clock::nanos_since_boot();

    // 7. Load shared libraries from block map (no full binary read)
    // Read DT_STRTAB from block map to get library names
    let mut loaded_libs = if !dyn_info.needed_strtab_offsets.is_empty() && dyn_info.strsz > 0 {
        let strtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
        let strtab_data = read_file_range(backing.as_ref(), strtab_file_off, dyn_info.strsz as usize);

        let exe_dir = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        let mut libs = Vec::new();

        for &name_offset in &dyn_info.needed_strtab_offsets {
            let name_off = name_offset as usize;
            if name_off >= strtab_data.len() { continue; }
            let name_end = strtab_data[name_off..].iter().position(|&b| b == 0)
                .unwrap_or(strtab_data.len() - name_off);
            let lib_name = core::str::from_utf8(&strtab_data[name_off..name_off + name_end]).unwrap_or("");
            if lib_name.is_empty() { continue; }

            let lib_path = alloc::format!("{}/{}", exe_dir, lib_name);
            let t_load0 = crate::clock::nanos_since_boot();

            // Check the shared library cache first
            if let Some(lib) = elf::try_clone_cached(&lib_path) {
                libs.push(lib);
                continue;
            }

            let so_data = {
                let result = vfs::lock().read_file(&lib_path);
                match result {
                    Ok(d) => d,
                    Err(_) => {
                        let fallback = alloc::format!("/lib/{}", lib_name);
                        match vfs::lock().read_file(&fallback) {
                            Ok(d) => d,
                            Err(e) => {
                                log!("spawn: {}: failed to load {}: {}", path, lib_name, e);
                                return Err(SyscallError::NotFound);
                            }
                        }
                    }
                }
            };
            let t_load1 = crate::clock::nanos_since_boot();

            match elf::load_shared_lib(&so_data) {
                Ok((lib, rw_vaddr, rw_end_vaddr)) => {
                    let t_load2 = crate::clock::nanos_since_boot();
                    log!("dynamic: loaded {} base={:#x} ({} syms, read={}ms load={}ms)",
                        lib_name, lib.phys_base, lib.sym_count,
                        (t_load1 - t_load0) / 1_000_000, (t_load2 - t_load1) / 1_000_000);
                    let lib = elf::cache_loaded_lib_pub(&lib_path, lib, rw_vaddr, rw_end_vaddr);
                    libs.push(lib);
                }
                Err(e) => {
                    log!("spawn: {}: failed to load {}: {}", path, lib_name, e);
                    return Err(SyscallError::NotFound);
                }
            }
        }

        // 7b. Read exe .dynsym/.dynstr from block map for exe sym map
        if !libs.is_empty() {
            let dynstr_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
            let dynstr_data = read_file_range(backing.as_ref(), dynstr_file_off, dyn_info.strsz as usize);

            // Determine .dynsym entry count via GNU hash table or SYMTAB/STRTAB gap
            let sym_count = if dyn_info.gnu_hash_vaddr != 0 {
                let gnu_hash_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.gnu_hash_vaddr);
                // Read enough for the hash table (header + bloom + buckets + chains)
                // Start with a generous read; typical .dynsym for executables is small
                let gnu_hash_data = read_file_range(backing.as_ref(), gnu_hash_file_off,
                    64 * 1024); // 64KB should cover most exe gnu_hash tables
                elf::gnu_hash_sym_count_from_data(&gnu_hash_data)
            } else if dyn_info.symtab_vaddr != 0 && dyn_info.strtab_vaddr > dyn_info.symtab_vaddr {
                // No GNU hash: infer from SYMTAB-to-STRTAB gap (24 bytes per entry)
                ((dyn_info.strtab_vaddr - dyn_info.symtab_vaddr) / 24) as usize
            } else {
                0
            };

            let mut exe_sym_map = if sym_count > 0 {
                let symtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.symtab_vaddr);
                let dynsym_data = read_file_range(backing.as_ref(), symtab_file_off, sym_count * elf::SYM_SIZE);
                elf::build_exe_sym_map(&dynsym_data, &dynstr_data, sym_count, UserAddr::new(base))
            } else {
                hashbrown::HashMap::new()
            };

            // If .dynsym has no defined symbols, fall back to .symtab from section headers.
            // This handles PIE executables that don't export symbols via --export-dynamic.
            if exe_sym_map.is_empty() {
                if let Some((shoff, shnum, shentsize)) = layout.section_headers {
                    let shdr_data = read_file_range(backing.as_ref(), shoff, shnum as usize * shentsize as usize);
                    if let Some(m) = elf::build_symtab_map(&shdr_data, shentsize, backing.as_ref(), UserAddr::new(base)) {
                        exe_sym_map = m;
                    }
                }
            }

            let t_syms = crate::clock::nanos_since_boot();
            log!("dynamic: {} exe syms hashed from block map in {}ms",
                exe_sym_map.len(), (t_syms - t2) / 1_000_000);

            // NOTE: lib bind relocs and exe GLOB_DAT are deferred until after
            // VMA addresses are assigned (user_base must be correct for GOT values).
        }

        libs
    } else {
        Vec::new()
    };

    let t_deps = crate::clock::nanos_since_boot();

    // 8. Create user address space (PML4) — ELF segments are demand-faulted
    let child_pt: PageTables = Arc::new(Lock::new(crate::mm::paging::AddressSpace::new_user()));

    // 8a. Map shared libraries via VmaList and assign virtual addresses.
    // This MUST happen BEFORE relocation processing so that user_base is correct
    // when GOT entries are written (RELATIVE: user_base + addend, GLOB_DAT: user_base + sym.st_value).
    for lib in &mut loaded_libs {
        match &lib.memory {
            elf::LibMemory::Owned(alloc) => {
                let phys = DirectMap::phys_of(alloc.ptr());
                let (vaddr, _) = vma_map(&mut vmas, &child_pt, phys, alloc.size() as u64)
                    .expect("spawn: out of virtual address space for lib");
                lib.user_base = vaddr;
            }
            elf::LibMemory::Shared { rw_alloc, cached_image, rw_offset, .. } => {
                let cached_phys = cached_image.phys();
                let (lib_vaddr, _) = vma_map(&mut vmas, &child_pt, cached_phys, cached_image.size() as u64)
                    .expect("spawn: out of virtual address space for lib");
                let num_rw_pages = rw_alloc.size() / PAGE_2M as usize;
                let rw_phys = DirectMap::phys_of(rw_alloc.ptr());
                for i in 0..num_rw_pages {
                    let user_virt = lib_vaddr.raw() + *rw_offset as u64 + i as u64 * PAGE_2M;
                    let phys = rw_phys + i as u64 * PAGE_2M;
                    child_pt.lock().remap(UserAddr::new(user_virt), phys, true);
                }
                lib.user_base = lib_vaddr;
            }
        }
    }

    // 8b. Rebase RELATIVE relocations: load_shared_lib applied them with phys_base,
    // but now user_base differs. Add delta = (user_base - phys_base) to each entry.
    for lib in &loaded_libs {
        let delta = lib.user_base.raw() as i64 - lib.phys_base as i64;
        if delta != 0 {
            elf::rebase_relative_relocs(lib, delta);
        }
    }

    // 8c. NOW process library bind relocations (user_base is correct for all libs).
    if !loaded_libs.is_empty() {
        // Rebuild exe_sym_map (we need it for bind relocs and exe GLOB_DAT)
        let dynstr_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
        let dynstr_data = if dyn_info.strsz > 0 {
            read_file_range(backing.as_ref(), dynstr_file_off, dyn_info.strsz as usize)
        } else {
            Vec::new()
        };

        let sym_count = if dyn_info.gnu_hash_vaddr != 0 {
            let gnu_hash_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.gnu_hash_vaddr);
            let gnu_hash_data = read_file_range(backing.as_ref(), gnu_hash_file_off, 64 * 1024);
            elf::gnu_hash_sym_count_from_data(&gnu_hash_data)
        } else if dyn_info.symtab_vaddr != 0 && dyn_info.strtab_vaddr > dyn_info.symtab_vaddr {
            ((dyn_info.strtab_vaddr - dyn_info.symtab_vaddr) / 24) as usize
        } else {
            0
        };

        let exe_sym_map = if sym_count > 0 {
            let symtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.symtab_vaddr);
            let dynsym_data = read_file_range(backing.as_ref(), symtab_file_off, sym_count * elf::SYM_SIZE);
            elf::build_exe_sym_map(&dynsym_data, &dynstr_data, sym_count, UserAddr::new(base))
        } else {
            hashbrown::HashMap::new()
        };

        // Resolve lib bind relocs against exe symbols (NOW user_base is correct)
        for lib in &loaded_libs {
            elf::resolve_lib_bind_relocs_pub(lib, &exe_sym_map, &loaded_libs);
        }

        // Resolve exe GLOB_DAT entries against loaded libs
        let symtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.symtab_vaddr);
        for &(r_offset, r_sym, _r_addend) in &parsed_relas.glob_dat {
            if r_sym == 0 { continue; }
            let sym_data = read_file_range(backing.as_ref(), symtab_file_off + r_sym as u64 * elf::SYM_SIZE as u64, elf::SYM_SIZE);
            if sym_data.len() < elf::SYM_SIZE { continue; }
            let sym = elf::read_sym(&sym_data, 0);
            let sym_name = elf::sym_name(&sym, &dynstr_data);
            let resolved = loaded_libs.iter().find_map(|lib| elf::gnu_dlsym_pub(lib, sym_name));
            match resolved {
                Some(addr) => reloc_index.add_u64(r_offset, addr.raw()),
                None => log!("dynamic: unresolved exe symbol: {}", sym_name),
            }
        }
    }

    // 9. Stack at fixed virtual address (STACK_BASE from vma.rs)
    let stack_alloc = match OwnedAlloc::new(USER_STACK_SIZE, PAGE_2M as usize) {
        Some(a) => a,
        None => {
            log!("spawn: {}: failed to allocate user stack ({} bytes)", path, USER_STACK_SIZE);
            return Err(SyscallError::ResourceExhausted);
        }
    };
    let stack_phys = DirectMap::from_ptr(stack_alloc.ptr());
    let stack_vaddr = UserAddr::new(crate::vma::STACK_BASE);
    let user_stack = UserStack::new(stack_vaddr, stack_phys, USER_STACK_SIZE as u64);
    child_pt.lock().map_range(stack_vaddr, stack_phys.phys(), USER_STACK_SIZE as u64, true);


    // 10. TLS setup — read exe TLS template from page cache, build multi-module layout
    let exe_tls_template = if layout.tls_memsz > 0 {
        let tls_file_off = vaddr_to_file_offset(&layout.segments, layout.tls_vaddr);
        let tls_data = read_file_range(backing.as_ref(), tls_file_off, layout.tls_filesz);
        let tls_buf = OwnedAlloc::new(layout.tls_memsz, 16).expect("TLS template alloc");
        unsafe {
            core::ptr::copy_nonoverlapping(tls_data.as_ptr(), tls_buf.ptr(), layout.tls_filesz);
            if layout.tls_memsz > layout.tls_filesz {
                core::ptr::write_bytes(tls_buf.ptr().add(layout.tls_filesz), 0, layout.tls_memsz - layout.tls_filesz);
            }
        }
        Some(tls_buf)
    } else {
        None
    };

    let (tls_modules, tls_total_memsz, max_tls_align, next_tls_module_id) =
        build_tls_layout(&loaded_libs, &layout, exe_tls_template.as_ref());

    // Apply TLS relocations for shared libraries loaded at startup.
    let tls_info = elf::TlsModuleInfo { libs: &loaded_libs, modules: &tls_modules };
    for lib in &loaded_libs {
        // Match by template pointer — unique per lib since each points into a distinct ELF mapping.
        // Libs without TLS (tls_memsz=0) have null template and won't match any module.
        let module = tls_modules.iter().find(|m| m.template == lib.tls_template);
        let lib_base_offset = module.map(|m| m.base_offset).unwrap_or(0);
        // IE model: TPOFF refs to static-block TLS (static modules and cross-module refs)
        elf::apply_tpoff_relocs(lib, lib_base_offset, tls_total_memsz, &tls_info);
        // GD model: DTPMOD64/DTPOFF64 for this lib's own TLS (DTV-based dynamic access)
        if let Some(m) = module {
            elf::apply_dtpmod_relocs(lib, m.module_id, &tls_info);
        }
    }
    // Resolve exe TPOFF relocations → add pre-computed values to reloc index
    {
        let exe_base_offset = tls_modules.iter()
            .find(|m| m.module_id == 1)
            .map(|m| m.base_offset)
            .unwrap_or(0);

        // Read exe .dynsym/.dynstr for resolving named TPOFF symbols
        let dynstr_data = if dyn_info.strsz > 0 {
            let dynstr_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
            read_file_range(backing.as_ref(), dynstr_file_off, dyn_info.strsz as usize)
        } else {
            Vec::new()
        };

        for &(r_offset, r_sym, r_addend) in &parsed_relas.tpoff64 {
            let tpoff = resolve_exe_tpoff(
                r_sym, r_addend, exe_base_offset, tls_total_memsz,
                &layout.segments, dyn_info.symtab_vaddr, backing.as_ref(), &dynstr_data, &tls_info,
            );
            reloc_index.add_u64(r_offset, tpoff as u64);
        }
        for &(r_offset, r_sym, r_addend) in &parsed_relas.tpoff32 {
            let tpoff = resolve_exe_tpoff(
                r_sym, r_addend, exe_base_offset, tls_total_memsz,
                &layout.segments, dyn_info.symtab_vaddr, backing.as_ref(), &dynstr_data, &tls_info,
            );
            reloc_index.add_i32(r_offset, tpoff as i32);
        }
    }

    // Finalize reloc index (sort all entries)
    reloc_index.finalize();
    let reloc_index = if reloc_index.len() > 0 {
        log!("ELF: {} relocations indexed (RELATIVE + GLOB_DAT + TPOFF)", reloc_index.len());
        Some(Arc::new(reloc_index))
    } else {
        None
    };

    let (tls_template, tls_memsz) = if !tls_modules.is_empty() {
        (tls_modules[0].template, tls_modules[0].memsz)
    } else {
        (None, 0)
    };

    log!("spawn: TLS {} modules, total_memsz={}", tls_modules.len(), tls_total_memsz);
    let (tls_alloc, fs_base) = if tls_total_memsz > 0 {
        match setup_combined_tls(&tls_modules, tls_total_memsz, max_tls_align) {
            Some(v) => v,
            None => {
                log!("spawn: {}: failed to allocate TLS ({} bytes)", path, tls_total_memsz);
                return Err(SyscallError::ResourceExhausted);
            }
        }
    } else {
        match setup_tls(None, 0, 1) {
            Some(v) => v,
            None => {
                log!("spawn: {}: failed to allocate TLS (empty)", path);
                return Err(SyscallError::ResourceExhausted);
            }
        }
    };
    // TLS mapped via VmaList — rebase all user-visible pointers from phys to vaddr
    let tls_phys = DirectMap::phys_of(tls_alloc.ptr());
    let (tls_vaddr, _) = vma_map(&mut vmas, &child_pt, tls_phys, tls_alloc.size() as u64)
        .expect("spawn: out of virtual address space for TLS");
    let tls_rebase = tls_vaddr.raw() as i64 - tls_phys as i64;
    let fs_base = (fs_base as i64 + tls_rebase) as u64;
    // Fix self-pointer (TCB[0]) and DTV pointer (TCB[8]) in the TLS block
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

    let entry = base + layout.entry_vaddr;
    let sp = user_stack.write_argv(argv);

    let t_tls = crate::clock::nanos_since_boot();

    // Store info for lazy symbol loading (deferred until a crash backtrace)
    let syms = if let Some((sh_off, sh_num, sh_entsize)) = layout.section_headers {
        ProcessSymbols::lazy(
            Arc::clone(&backing),
            sh_off, sh_num as usize, sh_entsize as usize,
            base,
            base + layout.vaddr_min, base + layout.vaddr_max,
            user_stack.base().raw(), user_stack.top(),
        )
    } else {
        ProcessSymbols::empty_with_bounds(
            base + layout.vaddr_min, base + layout.vaddr_max,
            user_stack.base().raw(), user_stack.top(),
        )
    };

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(process_start, entry, sp, 0) {
        Some(ks) => ks,
        None => {
            log!("spawn: {}: failed to allocate kernel stack", path);
            return Err(SyscallError::ResourceExhausted);
        }
    };


    let cwd = match parent {
        Some(ppid) => {
            let arc = {
                let guard = PROCESS_TABLE.lock();
                let table = guard.as_ref().unwrap();
                Arc::clone(table.get_process(ppid).unwrap().data())
            };
            let cwd = arc.lock().cwd.clone();
            cwd
        }
        None => String::from("/"),
    };

    let memory_size = USER_STACK_SIZE as u64;

    let proc_data = Arc::new(Lock::new(ProcessData {
        pid: Pid::from_raw(0),
        fds,
        cwd,

        elf_alloc: exe_tls_template, // TLS template allocation (if any)
        stack_alloc: Some(stack_alloc),
        tls_template,
        tls_memsz,
        tls_alloc: Some(tls_alloc),
        tls_modules,
        tls_total_memsz,
        tls_max_align: max_tls_align,
        next_tls_module_id,
        dynamic_tls_blocks: alloc::collections::BTreeMap::new(),
        symbols: syms,
        loaded_libs,
        mmap_regions: Vec::new(),
        user_stack_base: user_stack.base(),
        user_stack_size: user_stack.size(),
        env,
        syscall_counts: [0; 64],
        syscall_total: 0,
        syscall_total_ns: 0,
        vmas,
        demand_allocs: Vec::new(),
        reloc_index,
        elf_base: UserAddr::new(base),
        fault_trace: PageFaultTrace::new(),
        peak_memory: 0,
        alloc_count: 0,
        free_count: 0,
    }));

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = table.alloc_pid();

    let tid = table.insert_with(|tid| {
        proc_data.lock().pid = pid;
        SchedEntry::new(
            tid, pid, ThreadLocation::Scheduled, Kind::Process { parent },
            make_name(path), memory_size, proc_data,
        )
    });
    drop(guard);

    let ctx = scheduler::ThreadCtx {
        tid,
        process: pid,
        kernel_stack: ks_alloc,
        kernel_rsp: ks_rsp,
        address_space: Some(child_pt.clone()),
        fs_base,
        cpu_ns: 0,
        scheduled_at: 0,
        blocked_on: None,
        deadline: 0,
        blocked_since: 0,
    };
    scheduler::enqueue_new(ctx);

    let t3 = crate::clock::nanos_since_boot();
    log!("spawn: {} pid={} tid={} base={:#x} entry={:#x} cr3={:#x} (layout={}ms relocs={}ms deps={}ms tls={}ms total={}ms)",
        path, pid, tid, base, entry, child_pt.lock().cr3(),
        (t1 - t0) / 1_000_000, (t2 - t1) / 1_000_000, (t_deps - t2) / 1_000_000,
        (t_tls - t_deps) / 1_000_000, (t3 - t0) / 1_000_000);

    Ok(pid)
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
        let wall_ms = data.syscall_total_ns / 1_000_000;
        log!("syscalls: pid={pid} total={} syscall_wall={wall_ms}ms{profile}", data.syscall_total);
    }

    // Print memory stats
    if data.peak_memory > 0 || data.alloc_count > 0 {
        log!("memory: pid={pid} peak={}MB allocs={} frees={}",
            data.peak_memory / (1024 * 1024), data.alloc_count, data.free_count);
    }

    fd::close_all(&mut data.fds, &mut *vfs::lock(), pid);
    data.tls_alloc.take();
    data.elf_alloc.take();
    data.stack_alloc.take();
    data.loaded_libs.clear();
    data.mmap_regions.clear();

    data.demand_allocs.clear();
    data.vmas.clear();
    data.reloc_index = None;
}

/// Phase 2 of teardown: zombie threads, free page tables, set zombie state.
/// `addr_space` is the process's address space, extracted from the scheduler
/// before calling this function.
/// Caller must hold PROCESS_TABLE lock and have already switched to kernel CR3.
/// Returns Tids that need waking (e.g. parent blocked on waitpid, threads blocked
/// on thread_join). The caller must wake them AFTER releasing the table lock.
fn teardown_scheduling(table: &mut ProcessTable, process_pid: Pid, _addr_space: PageTables, code: i32) -> Vec<Tid> {
    let main_tid = table.find_main_thread(process_pid)
        .expect("teardown_scheduling: main thread not found");
    let mut to_wake = Vec::new();

    // Kill all child threads and remove their ThreadCtx from the scheduler
    let child_tids: Vec<Tid> = table.iter()
        .filter(|(tid, e)| *tid != main_tid && matches!(e.kind(), Kind::Thread { parent } if *parent == process_pid))
        .map(|(tid, _)| tid)
        .collect();
    for tid in &child_tids {
        let child = table.get_mut(*tid).unwrap();
        if !matches!(child.state(), ProcessState::Zombie(_)) {
            child.zombify_thread(-1);
        }
        // Remove from scheduler (drop ThreadCtx → frees kernel stack)
        scheduler::remove_thread(*tid);
    }

    // Remove the main thread from the scheduler if it's not the current thread.
    // (If it IS the current thread, exit_current handles it via SwitchReason::Exit.)
    let current = crate::arch::percpu::current_tid().unwrap();
    if main_tid != current {
        scheduler::remove_thread(main_tid);
    }

    shared_memory::cleanup_process(process_pid);

    let entry = table.get_mut(main_tid).unwrap();
    let cpu_ms = entry.cpu_ns() / 1_000_000;
    let has_parent = matches!(entry.kind(), Kind::Process { parent: Some(_) });
    let orphan_cleanup = entry.zombify_process(code);
    let name = core::str::from_utf8(entry.name()).unwrap_or("?").trim_end_matches('\0');
    log!("exit: {name} pid={process_pid} code={code} cpu={cpu_ms}ms");

    table.handle_orphans(orphan_cleanup);

    // Identify parent to wake for waitpid
    if has_parent {
        if let Kind::Process { parent: Some(ppid) } = table.get(main_tid).unwrap().kind() {
            let ppid = *ppid;
            if let Some(parent_entry) = table.get_process(ppid) {
                to_wake.push(parent_entry.tid());
            }
        }
    }

    to_wake
}

/// Exit the entire process (all threads). If called from a thread, kills the
/// parent process and all siblings.
pub fn exit(code: i32) -> ! {
    // Phase 1: Determine process pid, data Arc, and address space
    let (process_pid, data_arc, addr_space) = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();
        let tid = current_tid();
        let Some(entry) = table.get(tid) else {
            drop(guard);
            unsafe { cpu::write_cr3(crate::mm::paging::kernel().lock().as_ref().unwrap().cr3()); }
            scheduler::exit_current(code);
        };
        let process_pid = entry.process();
        let data_arc = Arc::clone(table.get_process(process_pid).unwrap().data());
        let addr_space = scheduler::current_address_space()
            .expect("exit: no address space");
        (process_pid, data_arc, addr_space)
    };

    // Phase 2: Switch to kernel CR3 and clean up resources
    unsafe { cpu::write_cr3(crate::mm::paging::kernel().lock().as_ref().unwrap().cr3()); }
    teardown_resources(&data_arc, process_pid);

    // Phase 3: Scheduling teardown (table lock, then release before waking)
    let to_wake = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let tid = current_tid();

        // If we're a thread, zombie ourselves first
        if let Kind::Thread { .. } = table.get(tid).unwrap().kind() {
            table.get_mut(tid).unwrap().zombify_thread(code);
        }

        teardown_scheduling(table, process_pid, addr_space, code)
    };
    // Table lock released — now safe to wake via scheduler
    for tid in to_wake {
        scheduler::wake_tid(tid);
    }

    // Phase 4: Exit the current thread via scheduler (context switch away)
    scheduler::exit_current(code);
}

/// Exit the current thread only. For processes without threads, tears down
/// the process. For threads, zombifies without freeing the address space.
pub fn thread_exit(code: i32) -> ! {
    let kind = with_current_sched(|s| *s.kind());
    let addr_space = scheduler::current_address_space();

    unsafe { cpu::write_cr3(crate::mm::paging::kernel().lock().as_ref().unwrap().cr3()); }

    match kind {
        Kind::Thread { parent } => {
            // Thread exit: close our FDs, free TLS, zombie ourselves
            {
                let data_arc = current_data();
                let mut data = data_arc.lock();
                fd::close_all(&mut data.fds, &mut *vfs::lock(), current_process());
                // TLS page table entries are cleaned up when the AddressSpace is dropped.
                // The physical memory is freed when tls_alloc is dropped below.
                data.tls_alloc.take();
            }
            // Free this thread's dynamic TLS blocks from the process-level data.
            {
                let tid = current_tid();
                let owner_arc = fd_owner_data();
                let mut owner_data = owner_arc.lock();
                owner_data.dynamic_tls_blocks.retain(|&(t, _), _| t != tid);
            }

            let parent_tid = {
                let mut guard = PROCESS_TABLE.lock();
                let table = guard.as_mut().unwrap();
                let tid = current_tid();
                let cpu_ms = table.get(tid).unwrap().cpu_ns() / 1_000_000;
                table.get_mut(tid).unwrap().zombify_thread(code);
                let name = core::str::from_utf8(table.get(tid).unwrap().name()).unwrap_or("?").trim_end_matches('\0');
                log!("exit: {name} tid={tid} code={code} cpu={cpu_ms}ms");

                // Find parent's main thread Tid for deferred wake
                table.get_process(parent).map(|e| e.tid())
            };

            // Wake parent if blocked on thread_join (after releasing table lock)
            if let Some(ptid) = parent_tid {
                scheduler::wake_tid(ptid);
            }

            scheduler::exit_current(code);
        }
        Kind::Process { .. } => {
            // Process exit — full teardown
            let process_pid = current_process();
            let data_arc = current_data();
            let addr_space = addr_space.expect("thread_exit: process has no address space");
            teardown_resources(&data_arc, process_pid);

            let to_wake = {
                let mut guard = PROCESS_TABLE.lock();
                let table = guard.as_mut().unwrap();
                teardown_scheduling(table, process_pid, addr_space, code)
            };
            for tid in to_wake {
                scheduler::wake_tid(tid);
            }

            scheduler::exit_current(code);
        }
    }
}

// ---------------------------------------------------------------------------
// Blocking / scheduling
// ---------------------------------------------------------------------------

/// Block the current thread on the given event sources with optional deadline.
pub fn block(events: &[scheduler::EventSource], deadline: u64) {
    scheduler::block(events, deadline);
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
/// Thin wrapper that locks PROCESS_TABLE and delegates to ProcessTable method.
pub fn collect_child_zombie(child_pid: Pid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    table.collect_child_zombie(child_pid, parent_pid)
}

/// Atomically validate parent-thread relationship and collect a zombie thread.
/// Thin wrapper that locks PROCESS_TABLE and delegates to ProcessTable method.
pub fn collect_thread_zombie(tid: Tid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    table.collect_thread_zombie(tid, parent_pid)
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
        let Some(entry) = table.get(tid) else { return false };
        let data = match entry.kind() {
            Kind::Thread { parent } => {
                match table.get_process(*parent) {
                    Some(parent_entry) => Arc::clone(parent_entry.data()),
                    None => return false,
                }
            }
            _ => Arc::clone(entry.data()),
        };
        (data, addr_space)
    };

    let mut data = data_arc.lock();

    // Verify the fault address is within a valid VMA
    if data.vmas.find(UserAddr::new(fault_addr)).is_none() {
        //log!("handle_page_fault: no VMA for {:#x} tid={}", fault_addr, tid);
        return false;
    }

    // Round down to 2MB boundary
    let page_2m = PAGE_2M;
    let region_start = fault_addr & !(page_2m - 1);

    let reloc_index = data.reloc_index.clone();
    let elf_base = data.elf_base.raw();

    // If a 2MB page is already mapped at this region (from a previous fault
    // in a different VMA that shares the same 2MB range), just return success.
    if addr_space.lock().translate(UserAddr::new(region_start)).is_some() {
        return true;
    }

    // Allocate a zeroed 2MB physical page
    let alloc = match OwnedAlloc::new(page_2m as usize, page_2m as usize) {
        Some(a) => a,
        None => return false,
    };
    let page_ptr = alloc.ptr();

    // Fill the 2MB page from ALL VMAs that overlap this region.
    // Multiple segments (e.g. .text and .rodata) can share a 2MB range.
    // If ANY overlapping VMA is writable, map the entire 2MB as writable.
    let region_end_full = region_start + page_2m;
    let writable = data.vmas.overlapping(UserAddr::new(region_start), UserAddr::new(region_end_full))
        .any(|v| v.writable);

    for vma in data.vmas.overlapping(UserAddr::new(region_start), UserAddr::new(region_end_full)) {
        let vma_s = vma.start.raw();
        let vma_e = vma.end.raw();

        match &vma.kind {
            crate::vma::VmaKind::Anonymous => {
                // Already zeroed by OwnedAlloc::new
            }
            crate::vma::VmaKind::FileBacked { backing, file_offset, file_size } => {
                let fill_start = region_start.max(vma_s);
                let fill_end = region_end_full.min(vma_e);
                let mut vaddr = fill_start & !0xFFF;

                while vaddr < fill_end {
                    let vma_offset = vaddr - vma_s;
                    let page_offset = (vaddr - region_start) as usize;

                    if vma_offset < *file_size {
                        let byte_offset = vma_offset + file_offset;
                        let mut page_buf = [0u8; 4096];
                        backing.read_page(byte_offset, &mut page_buf);
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
    addr_space.lock().remap(UserAddr::new(region_start), DirectMap::phys_of(alloc.ptr()), writable);
    cpu::flush_tlb();

    data.demand_allocs.push(alloc);

    // Update memory tracking
    data.alloc_count += 1;
    let current_mem = data.demand_allocs.len() as u64 * PAGE_2M;
    if current_mem > data.peak_memory {
        data.peak_memory = current_mem;
    }

    // Record fault for crash diagnostics
    let elapsed_us = ((crate::clock::nanos_since_boot() - t0) / 1000).min(u16::MAX as u64) as u16;
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

/// Free a 4KB page at the given physical address.
// ---------------------------------------------------------------------------
// Crash diagnostics
// ---------------------------------------------------------------------------

/// Dump the page fault trace and memory around `fault_addr` for the current process.
/// Called from the exception handler on user-mode crashes.
pub fn dump_crash_diagnostics(fault_addr: u64, rip: u64) {
    let tid = current_tid();
    if tid == Tid::MAX { return; }

    let data_arc = {
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return };
        match table.get(tid) {
            Some(entry) => Arc::clone(entry.data()),
            None => return,
        }
    };
    let data = data_arc.lock();

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
    // Read the ACTUAL FS_BASE MSR (swapgs doesn't affect FS, only GS)
    let fs_base_msr = crate::arch::read_fs_base();
    let fs_base_saved = scheduler::with_current_ctx(|ctx| ctx.fs_base).unwrap_or(0);
    let fs_base = fs_base_msr;
    if fs_base_msr != fs_base_saved {
        log!("  FS base: MSR={:#x} saved={:#x} (MISMATCH!)", fs_base_msr, fs_base_saved);
    }
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
        // Also dump TLS alloc info
        let tls_info = data.tls_alloc.as_ref().map(|a| (a.ptr() as u64, a.size()));
        log!("  TLS alloc: {:?}", tls_info);
    }
}

// ---------------------------------------------------------------------------
// Symbol resolution / address validation
// ---------------------------------------------------------------------------

/// Resolve and log a user-mode address against the process's symbol table.
/// Returns true if the address was resolved and logged.
pub fn resolve_user_symbol(tid: Tid, addr: u64) -> bool {
    let data_arc = {
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return false };
        match table.get(tid) {
            Some(entry) => Arc::clone(entry.data()),
            None => return false,
        }
    };
    let mut data = data_arc.lock();
    crate::symbols::resolve_user(&mut data.symbols, addr)
}

// ---------------------------------------------------------------------------
// Kill
// ---------------------------------------------------------------------------

/// Kill a child process. Only the parent can kill its children.
/// Returns 0 on success, error code on failure.
pub fn kill_process(target_pid: Pid) -> u64 {
    use toyos_abi::syscall::SyscallError;
    let caller = current_process();

    // Phase 1: Validate and get data Arc (brief table lock)
    let data_arc = {
        let guard = PROCESS_TABLE.lock();
        let table = guard.as_ref().unwrap();

        let Some(entry) = table.get_process(target_pid) else { return SyscallError::NotFound.to_u64() };
        if !matches!(entry.kind(), Kind::Process { parent: Some(ppid) } if *ppid == caller) {
            return SyscallError::PermissionDenied.to_u64();
        }
        if scheduler::thread_sched_state(entry.tid()) == 0 {
            return SyscallError::WouldBlock.to_u64(); // currently running on a CPU
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
        data.demand_allocs.clear();
        data.vmas.clear();
        data.reloc_index = None;
    }

    // Phase 3: Scheduling teardown (table lock)
    // Phase 3: Scheduling teardown (table lock, then release for wakes)
    let caller_tid = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();

        // Re-check the target is still there and not zombie
        let Some(main_tid) = table.find_main_thread(target_pid) else { return 0 };
        let Some(entry) = table.get(main_tid) else { return 0 };
        if matches!(entry.state(), ProcessState::Zombie(_)) { return 0; }

        // Kill child threads and remove from scheduler
        let child_tids: Vec<Tid> = table.iter()
            .filter(|(tid, e)| *tid != main_tid && matches!(e.kind(), Kind::Thread { parent } if *parent == target_pid))
            .map(|(tid, _)| tid)
            .collect();
        for tid in &child_tids {
            let child = table.get_mut(*tid).unwrap();
            if !matches!(child.state(), ProcessState::Zombie(_)) {
                child.zombify_thread(-1);
            }
            scheduler::remove_thread(*tid);
        }

        // Get address space from the main thread's ThreadCtx
        if let Some(mut ctx) = scheduler::remove_thread(main_tid) {
            if ctx.address_space.take().is_some() {
                shared_memory::cleanup_process(target_pid);
            }
        }

        let entry = table.get_mut(main_tid).unwrap();
        let orphan_cleanup = entry.zombify_process(137); // 128 + 9 (SIGKILL-like)
        let name = core::str::from_utf8(entry.name()).unwrap_or("?").trim_end_matches('\0');
        log!("kill: {name} pid={target_pid}");

        table.handle_orphans(orphan_cleanup);

        // Collect caller Tid for deferred wake
        let caller_tid = table.get_process(caller).map(|e| e.tid());
        caller_tid
    };

    // Wake caller if blocked on waitpid (after releasing table lock)
    if let Some(ctid) = caller_tid {
        scheduler::wake_tid(ctid);
    }

    0
}

/// AP entry into the scheduler. Called from smp::ap_entry after SMP_READY.
pub fn ap_idle() -> ! {
    scheduler::schedule_no_return();
}
