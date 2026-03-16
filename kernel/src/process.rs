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
use crate::{elf, pipe, scheduler, shared_memory, vfs};
use crate::{KernelAddr, PhysAddr, UserAddr};

pub use toyos_abi::Pid;
use toyos_abi::syscall::SyscallError;

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
pub struct PageTableRoot(PhysAddr);

// SAFETY: PageTableRoot points to a PML4 page table in physical memory.
// Page tables are not tied to any specific thread — they are hardware structures.
unsafe impl Send for PageTableRoot {}

impl PageTableRoot {
    pub fn new(addr: PhysAddr) -> Self { Self(addr) }
    pub fn phys(self) -> PhysAddr { self.0 }
    pub fn as_ptr(self) -> *mut u64 { self.0.as_mut_ptr() }
    pub fn as_u64(self) -> u64 { self.0.raw() }
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
/// Write argv onto a user stack. `stack_top` is the user-visible (physical) address.
/// Returns the new user-visible stack pointer.
pub fn write_argv_to_stack(stack_top: u64, args: &[&str]) -> u64 {
    let mut sp = stack_top;
    let mut argv_ptrs: Vec<u64> = Vec::with_capacity(args.len());
    for arg in args.iter().rev() {
        sp -= (arg.len() + 1) as u64;
        let kptr = PhysAddr::new(sp).as_mut_ptr::<u8>();
        unsafe {
            core::ptr::copy_nonoverlapping(arg.as_ptr(), kptr, arg.len());
            *kptr.add(arg.len()) = 0;
        }
        argv_ptrs.push(sp); // user-visible address
    }
    argv_ptrs.reverse();
    let metadata_qwords = args.len() + 2;
    sp = (sp - metadata_qwords as u64 * 8) & !15;
    unsafe {
        let ksp = PhysAddr::new(sp).as_mut_ptr::<u64>();
        *ksp = args.len() as u64;
        for (i, ptr) in argv_ptrs.iter().enumerate() {
            *ksp.add(1 + i) = *ptr;
        }
        *ksp.add(1 + args.len()) = 0;
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
    BlockedFutex { phys_addr: PhysAddr, deadline: u64 },
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

/// Record of a single demand-paged fault, stored in a ring buffer for crash diagnostics.
#[derive(Clone, Copy)]
pub struct PageFaultRecord {
    pub fault_addr: u64,
    pub page_elf_offset: u64,
    pub block_idx: u32,
    pub reloc_count: u16,
    pub flags: u16, // bit 0: writable, bit 1: has_relocs, bit 2: anonymous, bit 3: beyond_extent
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
                reloc_count: 0, flags: 0,
            }; 32],
            write_pos: 0,
            total: 0,
        }
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
    pub messages: MessageQueue,
    pub poll_fds: [u64; 64],
    pub poll_len: u32,
    pub elf_alloc: Option<OwnedAlloc>,
    pub stack_alloc: Option<OwnedAlloc>,
    // Thread-local storage
    pub tls_template: KernelAddr,
    pub tls_filesz: usize,
    pub tls_memsz: usize,
    pub tls_alloc: Option<OwnedAlloc>,
    /// Multi-module TLS layout per loaded library.
    pub tls_modules: Vec<crate::elf::TlsModule>,
    /// Total combined TLS size across all modules.
    pub tls_total_memsz: usize,
    /// Maximum TLS alignment across all modules.
    pub tls_max_align: usize,
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
pub fn setup_tls(tls_template: KernelAddr, tls_filesz: usize, tls_memsz: usize, tls_align: usize) -> Option<(OwnedAlloc, u64)> {
    setup_combined_tls(&[elf::TlsModule { template: tls_template, filesz: tls_filesz, memsz: tls_memsz, base_offset: 0 }], tls_memsz, tls_align)
}

/// Allocate a combined TLS area for multiple modules (exe + shared libraries).
/// Each module's template is copied at its base_offset within the block.
///
/// x86-64 TLS Variant II layout:
///   [alignment padding] [TLS data (.tdata + .tbss)] [TCB (64 bytes)]
///                                                    ^-- TP (FS base)
///
/// The linker (LLD) computes TPOFF = sym_offset - memsz (raw, NOT rounded).
/// TP must be placed at data_start + memsz to match.
/// data_start must be aligned to tls_align so variable offsets work correctly.
///
/// TCB layout:
///   TP+0x00: self-pointer
///   TP+0x08: DTV pointer (unused, zero)
///   TP+0x10..0x3F: reserved (zero)
const TCB_SIZE: usize = 64;

pub fn setup_combined_tls(
    modules: &[crate::elf::TlsModule],
    total_memsz: usize,
    tls_align: usize,
) -> Option<(OwnedAlloc, u64)> {
    let block_size = total_memsz + TCB_SIZE;
    let alloc_size = paging::align_2m(block_size + tls_align);
    let alloc = OwnedAlloc::new_uninit(alloc_size, PAGE_2M as usize)?;
    let block = alloc.ptr();

    // Place TLS data near the END of the allocation so dlopen can extend downward.
    // Align tls_start so that data_start (= block + tls_start) has tls_align alignment.
    let align = if tls_align > 1 { tls_align } else { 8 };
    let tls_start = (alloc_size - block_size) & !(align - 1);

    // Zero the TLS block area (BSS must be zero).
    unsafe { core::ptr::write_bytes(block.add(tls_start), 0, block_size); }

    for module in modules {
        if module.filesz > 0 && !module.template.is_null() {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    module.template.as_ptr::<u8>(),
                    block.add(tls_start + module.base_offset),
                    module.filesz,
                );
            }
        }
    }

    // TP must be a user-visible physical address (mapped with USER bit in user page tables).
    let block_phys = PhysAddr::from_ptr(block).raw();
    let tp_user = block_phys + (tls_start + total_memsz) as u64;
    // Write self-pointer via kernel direct map
    let tp_kernel = block as u64 + (tls_start + total_memsz) as u64;
    unsafe { *(tp_kernel as *mut u64) = tp_user; }

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
    let (tls_template, tls_filesz, tls_memsz, tls_modules, tls_total_memsz, tls_max_align, parent_cwd) = {
        let data = data_arc.lock();
        (data.tls_template, data.tls_filesz, data.tls_memsz,
         data.tls_modules.clone(), data.tls_total_memsz, data.tls_max_align, data.cwd.clone())
    };

    // Phase 2: Allocate TLS (outside any lock — map_user does TLB flush)
    let (tls_alloc, fs_base) = if !tls_modules.is_empty() {
        setup_combined_tls(&tls_modules, tls_total_memsz, tls_max_align)?
    } else {
        setup_tls(tls_template, tls_filesz, tls_memsz, tls_max_align)?
    };
    paging::map_user(PhysAddr::from_ptr(tls_alloc.ptr()), tls_alloc.size() as u64);

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
        tls_max_align,
        symbols: ProcessSymbols::empty(),
        loaded_libs: Vec::new(),
        mmap_regions: Vec::new(),
        user_stack_base: UserAddr::new(stack_base),
        user_stack_size: if stack_base > 0 { stack_ptr - stack_base } else { 0 },
        env: Vec::new(),
        syscall_counts: [0; 64],
        syscall_total: 0,
        vmas: crate::vma::VmaList::new(),
        demand_allocs: Vec::new(),
        reloc_index: None,
        elf_base: UserAddr::new(0),
        fault_trace: PageFaultTrace::new(),
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
pub(crate) fn read_file_range(block_map: &[u64], offset: u64, len: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(len);
    let mut remaining = len;
    let mut file_off = offset;

    while remaining > 0 {
        let block_idx = (file_off / 4096) as usize;
        let off_in_block = (file_off % 4096) as usize;
        let chunk = (4096 - off_in_block).min(remaining);

        if block_idx < block_map.len() {
            let mut cache_guard = crate::page_cache::lock();
            let (cache, dev) = cache_guard.cache_and_dev();
            let page = cache.read(dev, block_map[block_idx]);
            result.extend_from_slice(&page[off_in_block..off_in_block + chunk]);
        } else {
            // Beyond file: zero fill
            result.resize(result.len() + chunk, 0);
        }

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
    block_map: &[u64],
    dynstr_data: &[u8],
    tls_info: &elf::TlsModuleInfo,
) -> i64 {
    if r_sym == 0 {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    }

    let symtab_file_off = vaddr_to_file_offset(segments, symtab_vaddr);
    let sym_data = read_file_range(block_map, symtab_file_off + r_sym as u64 * elf::SYM_SIZE as u64, elf::SYM_SIZE);
    if sym_data.len() < elf::SYM_SIZE {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    }
    let sym = elf::Elf64Sym::from_slice(&sym_data, 0);

    if sym.st_shndx != 0 {
        exe_base_offset as i64 + sym.st_value as i64 + r_addend - total_memsz as i64
    } else {
        let sym_name = sym.name(dynstr_data);

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
pub fn spawn(argv: &[&str], fds: FdTable, parent: Option<Pid>, env: Vec<u8>) -> Result<Pid, SyscallError> {
    let path = argv[0];
    let t0 = crate::clock::nanos_since_boot();

    // 1. Get block map from VFS (follows symlinks, traverses inode structure, no data read)
    let block_map = match vfs::lock().file_block_map(path) {
        Some(bm) if !bm.is_empty() => bm,
        Some(_) => {
            log!("spawn: {}: empty block map", path);
            return Err(SyscallError::NotFound);
        }
        None => {
            log!("spawn: {}: not found or filesystem does not support block maps", path);
            return Err(SyscallError::NotFound);
        }
    };
    let block_map = Arc::new(block_map);

    // 3. Read first few blocks for ELF headers (typically 1-2 blocks suffice)
    let header_size = 4096.min(block_map.len() * 4096); // at least first block
    let header_data = read_file_range(&block_map, 0, header_size);

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
        let dyn_data = read_file_range(&block_map, dyn_off, dyn_size as usize);
        elf::parse_dynamic(&dyn_data)
    } else {
        elf::DynamicInfo::empty()
    };

    let t1 = crate::clock::nanos_since_boot();

    // 4. Choose base address in user virtual space
    let base = USER_VM_BASE - layout.vaddr_min;

    // 5. Create VMAs for each PT_LOAD segment
    let mut vmas = crate::vma::VmaList::new();
    for seg in &layout.segments {
        let seg_start = (base + seg.vaddr) & !0xFFF;
        let seg_end = (base + seg.vaddr + seg.memsz + 0xFFF) & !0xFFF;

        // File-backed portion: compute which file blocks back this segment
        let file_block_start = seg.file_offset / 4096;
        let file_blocks_needed = ((seg.filesz + (seg.file_offset % 4096) + 4095) / 4096) as usize;
        let file_backed_end = seg_start + file_blocks_needed as u64 * 4096;

        if file_blocks_needed > 0 && file_backed_end > seg_start {
            // File-backed region
            vmas.insert(crate::vma::Vma {
                start: UserAddr::new(seg_start),
                end: UserAddr::new(file_backed_end.min(seg_end)),
                writable: seg.writable,
                kind: crate::vma::VmaKind::FileBacked {
                    block_map: Arc::clone(&block_map),
                    file_offset: file_block_start * 4096,
                    file_size: seg.filesz + (seg.file_offset % 4096),
                },
            });
        }

        if file_backed_end < seg_end {
            // BSS / anonymous portion (memsz > filesz)
            vmas.insert(crate::vma::Vma {
                start: UserAddr::new(file_backed_end.max(seg_start)),
                end: UserAddr::new(seg_end),
                writable: seg.writable,
                kind: crate::vma::VmaKind::Anonymous,
            });
        }
    }

    // 6. Read and parse relocation tables from block map
    let rela_data = if dyn_info.rela_size > 0 {
        let rela_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.rela_vaddr);
        read_file_range(&block_map, rela_file_off, dyn_info.rela_size as usize)
    } else if layout.dynamic.is_none() {
        // No PT_DYNAMIC — fall back to finding .rela.dyn from section headers
        if let Some((shoff, shnum, shentsize)) = layout.section_headers {
            let shdr_data = read_file_range(&block_map, shoff, shnum as usize * shentsize as usize);
            let bm = &block_map;
            if let Some((rela_off, rela_size)) = elf::find_rela_dyn_from_sections(
                &shdr_data, shentsize, &|off, len| read_file_range(bm, off, len),
            ) {
                read_file_range(&block_map, rela_off, rela_size as usize)
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
        read_file_range(&block_map, jmprel_file_off, dyn_info.jmprel_size as usize)
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
    let loaded_libs = if !dyn_info.needed_strtab_offsets.is_empty() && dyn_info.strsz > 0 {
        let strtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
        let strtab_data = read_file_range(&block_map, strtab_file_off, dyn_info.strsz as usize);

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
                    log!("dynamic: loaded {} ({} syms, read={}ms load={}ms)",
                        lib_name, lib.sym_count,
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
            let dynstr_data = read_file_range(&block_map, dynstr_file_off, dyn_info.strsz as usize);

            // Determine .dynsym entry count via GNU hash table or SYMTAB/STRTAB gap
            let sym_count = if dyn_info.gnu_hash_vaddr != 0 {
                let gnu_hash_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.gnu_hash_vaddr);
                // Read enough for the hash table (header + bloom + buckets + chains)
                // Start with a generous read; typical .dynsym for executables is small
                let gnu_hash_data = read_file_range(&block_map, gnu_hash_file_off,
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
                let dynsym_data = read_file_range(&block_map, symtab_file_off, sym_count * elf::SYM_SIZE);
                elf::build_exe_sym_map(&dynsym_data, &dynstr_data, sym_count, PhysAddr::new(base))
            } else {
                hashbrown::HashMap::new()
            };

            // If .dynsym has no defined symbols, fall back to .symtab from section headers.
            // This handles PIE executables that don't export symbols via --export-dynamic.
            if exe_sym_map.is_empty() {
                if let Some((shoff, shnum, shentsize)) = layout.section_headers {
                    let shdr_data = read_file_range(&block_map, shoff, shnum as usize * shentsize as usize);
                    if let Some(m) = elf::build_symtab_map(&shdr_data, shentsize, &block_map, PhysAddr::new(base)) {
                        exe_sym_map = m;
                    }
                }
            }

            let t_syms = crate::clock::nanos_since_boot();
            log!("dynamic: {} exe syms hashed from block map in {}ms",
                exe_sym_map.len(), (t_syms - t2) / 1_000_000);

            // Resolve lib bind relocs against exe symbols
            for lib in &libs {
                elf::resolve_lib_bind_relocs_pub(lib, &exe_sym_map, &libs);
            }

            // 7c. Resolve exe GLOB_DAT entries against loaded libs → add to reloc index
            let symtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.symtab_vaddr);
            for &(r_offset, r_sym, _r_addend) in &parsed_relas.glob_dat {
                if r_sym == 0 { continue; }
                let sym_data = read_file_range(&block_map, symtab_file_off + r_sym as u64 * elf::SYM_SIZE as u64, elf::SYM_SIZE);
                if sym_data.len() < elf::SYM_SIZE { continue; }
                let sym_name = elf::Elf64Sym::from_slice(&sym_data, 0).name(&dynstr_data);
                let resolved = libs.iter().find_map(|lib| elf::gnu_dlsym_pub(lib, sym_name));
                match resolved {
                    Some(addr) => reloc_index.add_u64(r_offset, addr.raw()),
                    None => log!("dynamic: unresolved exe symbol: {}", sym_name),
                }
            }
        }

        libs
    } else {
        Vec::new()
    };

    let t_deps = crate::clock::nanos_since_boot();

    // 8. Create user PML4 — no pages mapped for ELF segments (all demand-faulted)
    let child_pml4_addr = paging::create_user_pml4();
    let child_pml4 = child_pml4_addr.as_mut_ptr();
    let child_cr3 = Some(PageTableRoot::new(child_pml4_addr));

    // Map shared libraries (physical pages mapped into user address space, eager)
    for lib in &loaded_libs {
        match &lib.memory {
            elf::LibMemory::Owned(alloc) => {
                paging::map_user_in(child_pml4, PhysAddr::from_ptr(alloc.ptr()), alloc.size() as u64);
            }
            elf::LibMemory::Shared { rw_alloc, cached_addr, cached_size, rw_offset, .. } => {
                let cached_phys = *cached_addr;
                paging::map_user_readonly_in(child_pml4, cached_phys, *cached_size as u64);
                let num_rw_pages = rw_alloc.size() / paging::PAGE_2M as usize;
                for i in 0..num_rw_pages {
                    let user_virt = cached_phys.raw() + *rw_offset as u64 + i as u64 * paging::PAGE_2M;
                    let phys = PhysAddr::from_ptr(rw_alloc.ptr()) + i as u64 * paging::PAGE_2M;
                    paging::remap_user_2m_in(child_pml4, UserAddr::new(user_virt), phys);
                }
            }
        }
    }

    // 9. Stack (eager, physically contiguous)
    let stack_alloc = match OwnedAlloc::new(USER_STACK_SIZE, PAGE_2M as usize) {
        Some(a) => a,
        None => {
            log!("spawn: {}: failed to allocate user stack ({} bytes)", path, USER_STACK_SIZE);
            paging::free_user_page_tables(child_pml4);
            return Err(SyscallError::ResourceExhausted);
        }
    };
    let stack_phys = PhysAddr::from_ptr(stack_alloc.ptr());
    let stack_base = stack_phys.raw();
    let stack_top = stack_base + USER_STACK_SIZE as u64;
    paging::map_user_in(child_pml4, stack_phys, USER_STACK_SIZE as u64);

    // 10. TLS setup
    let mut tls_modules: Vec<elf::TlsModule> = Vec::new();
    let mut tls_cursor = 0usize;
    let mut max_tls_align = 1usize;

    for lib in &loaded_libs {
        if lib.tls_memsz > 0 {
            if tls_cursor > 0 {
                tls_cursor = (tls_cursor + 15) & !15;
            }
            tls_modules.push(elf::TlsModule { template: lib.tls_template, filesz: lib.tls_filesz, memsz: lib.tls_memsz, base_offset: tls_cursor });
            tls_cursor += lib.tls_memsz;
            if lib.tls_align > max_tls_align { max_tls_align = lib.tls_align; }
        }
    }

    // For exe TLS with demand paging: read TLS template data from page cache
    let exe_tls_template = if layout.tls_memsz > 0 {
        let tls_file_off = vaddr_to_file_offset(&layout.segments, layout.tls_vaddr);
        let tls_data = read_file_range(&block_map, tls_file_off, layout.tls_filesz);
        // Allocate a persistent buffer for the TLS template
        let tls_buf = OwnedAlloc::new(layout.tls_memsz, 16).expect("TLS template alloc");
        unsafe {
            core::ptr::copy_nonoverlapping(tls_data.as_ptr(), tls_buf.ptr(), layout.tls_filesz);
            // Zero BSS portion
            if layout.tls_memsz > layout.tls_filesz {
                core::ptr::write_bytes(
                    tls_buf.ptr().add(layout.tls_filesz), 0,
                    layout.tls_memsz - layout.tls_filesz);
            }
        }
        Some(tls_buf)
    } else {
        None
    };

    if layout.tls_memsz > 0 {
        if tls_cursor > 0 {
            tls_cursor = (tls_cursor + 15) & !15;
        }
        let template_addr = KernelAddr::from_ptr(exe_tls_template.as_ref().unwrap().ptr());
        tls_modules.push(elf::TlsModule { template: template_addr, filesz: layout.tls_filesz, memsz: layout.tls_memsz, base_offset: tls_cursor });
        tls_cursor += layout.tls_memsz;
        if layout.tls_align > max_tls_align { max_tls_align = layout.tls_align; }
    }
    // x86-64 TLS Variant II: the linker (LLD) computes TPOFF = sym_offset - memsz,
    // using the RAW memsz (not rounded up to alignment). TP must be placed at
    // data_start + memsz to match. Alignment of data_start is handled in setup_combined_tls.
    let tls_total_memsz = tls_cursor;

    // Apply TPOFF relocations for shared libraries
    let tls_info = elf::TlsModuleInfo { libs: &loaded_libs, modules: &tls_modules };
    for lib in &loaded_libs {
        let lib_base_offset = tls_modules.iter()
            .find(|m| m.template == lib.tls_template)
            .map(|m| m.base_offset)
            .unwrap_or(0);
        elf::apply_tpoff_relocs(lib, lib_base_offset, tls_total_memsz, &tls_info);
    }
    // Resolve exe TPOFF relocations → add pre-computed values to reloc index
    {
        let exe_base_offset = tls_modules.iter()
            .find(|m| {
                exe_tls_template.as_ref().map_or(false, |buf| m.template == KernelAddr::from_ptr(buf.ptr()))
            })
            .map(|m| m.base_offset)
            .unwrap_or(0);

        // Read exe .dynsym/.dynstr for resolving named TPOFF symbols
        let dynstr_data = if dyn_info.strsz > 0 {
            let dynstr_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
            read_file_range(&block_map, dynstr_file_off, dyn_info.strsz as usize)
        } else {
            Vec::new()
        };

        for &(r_offset, r_sym, r_addend) in &parsed_relas.tpoff64 {
            let tpoff = resolve_exe_tpoff(
                r_sym, r_addend, exe_base_offset, tls_total_memsz,
                &layout.segments, dyn_info.symtab_vaddr, &block_map, &dynstr_data, &tls_info,
            );
            reloc_index.add_u64(r_offset, tpoff as u64);
        }
        for &(r_offset, r_sym, r_addend) in &parsed_relas.tpoff32 {
            let tpoff = resolve_exe_tpoff(
                r_sym, r_addend, exe_base_offset, tls_total_memsz,
                &layout.segments, dyn_info.symtab_vaddr, &block_map, &dynstr_data, &tls_info,
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

    let (tls_template, tls_filesz, tls_memsz) = if !tls_modules.is_empty() {
        (tls_modules[0].template, tls_modules[0].filesz, tls_modules[0].memsz)
    } else {
        (KernelAddr::null(), 0, 0)
    };

    log!("spawn: TLS {} modules, total_memsz={}", tls_modules.len(), tls_total_memsz);
    let (tls_alloc, fs_base) = if tls_total_memsz > 0 {
        match setup_combined_tls(&tls_modules, tls_total_memsz, max_tls_align) {
            Some(v) => v,
            None => {
                log!("spawn: {}: failed to allocate TLS ({} bytes)", path, tls_total_memsz);
                paging::free_user_page_tables(child_pml4);
                return Err(SyscallError::ResourceExhausted);
            }
        }
    } else {
        match setup_tls(KernelAddr::null(), 0, 0, 1) {
            Some(v) => v,
            None => {
                log!("spawn: {}: failed to allocate TLS (empty)", path);
                paging::free_user_page_tables(child_pml4);
                return Err(SyscallError::ResourceExhausted);
            }
        }
    };
    paging::map_user_in(child_pml4, PhysAddr::from_ptr(tls_alloc.ptr()), tls_alloc.size() as u64);

    let entry = base + layout.entry_vaddr;
    let sp = write_argv_to_stack(stack_top, argv);

    let t_tls = crate::clock::nanos_since_boot();

    // Store info for lazy symbol loading (deferred until a crash backtrace)
    let syms = if let Some((sh_off, sh_num, sh_entsize)) = layout.section_headers {
        ProcessSymbols::lazy(
            Arc::clone(&block_map),
            sh_off, sh_num as usize, sh_entsize as usize,
            base,
            base + layout.vaddr_min, base + layout.vaddr_max,
            stack_base, stack_top,
        )
    } else {
        ProcessSymbols::empty_with_bounds(
            base + layout.vaddr_min, base + layout.vaddr_max,
            stack_base, stack_top,
        )
    };

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(process_start, entry, sp, 0) {
        Some(ks) => ks,
        None => {
            log!("spawn: {}: failed to allocate kernel stack", path);
            paging::free_user_page_tables(child_pml4);
            return Err(SyscallError::ResourceExhausted);
        }
    };


    let cwd = match parent {
        Some(ppid) => {
            let arc = {
                let guard = PROCESS_TABLE.lock();
                let table = guard.as_ref().unwrap();
                Arc::clone(table.get(ppid).unwrap().data())
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
        messages: MessageQueue::new(),
        poll_fds: [0; 64],
        poll_len: 0,
        elf_alloc: exe_tls_template, // TLS template allocation (if any)
        stack_alloc: Some(stack_alloc),
        tls_template,
        tls_filesz,
        tls_memsz,
        tls_alloc: Some(tls_alloc),
        tls_modules,
        tls_total_memsz,
        tls_max_align: max_tls_align,
        symbols: syms,
        loaded_libs,
        mmap_regions: Vec::new(),
        user_stack_base: UserAddr::new(stack_base),
        user_stack_size: USER_STACK_SIZE as u64,
        env,
        syscall_counts: [0; 64],
        syscall_total: 0,
        vmas,
        demand_allocs: Vec::new(),
        reloc_index,
        elf_base: UserAddr::new(base),
        fault_trace: PageFaultTrace::new(),
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
    log!("spawn: {} pid={} base={:#x} entry={:#x} cr3={:#x} (layout={}ms relocs={}ms deps={}ms tls={}ms total={}ms)",
        path, pid, base, entry, child_cr3.unwrap().as_u64(),
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
        log!("syscalls: pid={pid} total={}{profile}", data.syscall_total);
    }

    fd::close_all(&mut data.fds, &mut *vfs::lock(), pid);
    data.tls_alloc.take();
    data.elf_alloc.take();
    data.stack_alloc.take();
    data.loaded_libs.clear();
    data.mmap_regions.clear();

    // Free demand-paged 2MB allocations (dropped automatically by OwnedAlloc).
    data.demand_allocs.clear();
    data.vmas.clear();
    data.reloc_index = None;
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
    crate::pipe::cleanup_pml4(pml4);
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
    let process_pml4 = with_current_sched(|s| s.cr3().map(|c| c.as_ptr()));

    unsafe { cpu::write_cr3(paging::kernel_cr3()); }

    match kind {
        Kind::Thread { parent } => {
            // Thread exit: close our FDs, free TLS, zombie ourselves
            {
                let data_arc = current_data();
                let mut data = data_arc.lock();
                fd::close_all(&mut data.fds, &mut *vfs::lock(), current_pid());
                // Clear USER bits from shared page tables before freeing TLS,
                // so the freed memory doesn't have stale USER PTEs.
                if let (Some(tls), Some(pml4)) = (data.tls_alloc.as_ref(), process_pml4) {
                    paging::unmap_user(pml4, PhysAddr::from_ptr(tls.ptr()), tls.size() as u64);
                }
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
        .unwrap_or(paging::kernel_cr3().as_ptr());
    let phys_addr = match paging::virt_to_phys(pml4, UserAddr::new(addr)) {
        Some(pa) => pa,
        None => return u64::MAX,
    };

    let current = unsafe { core::ptr::read_volatile(phys_addr.as_ptr::<u32>()) };
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
            .unwrap_or(paging::kernel_cr3().as_ptr())
    }).unwrap();
    let caller_phys = match paging::virt_to_phys(caller_cr3, UserAddr::new(addr)) {
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
// Demand paging
// ---------------------------------------------------------------------------

/// Handle a page fault at `fault_addr` by looking up the current process's VMAs.
/// Returns true if the fault was resolved (a page was mapped), false if fatal.
pub fn handle_page_fault(fault_addr: u64, _error_code: u64) -> bool {
    let pid = current_pid();
    if pid == Pid::MAX { return false; }

    let (data_arc, pml4) = {
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return false };
        let Some(entry) = table.get(pid) else { return false };
        let Some(cr3) = entry.cr3() else { return false };
        let data = match entry.kind() {
            Kind::Thread { parent } => {
                match table.get(*parent) {
                    Some(parent_entry) => Arc::clone(parent_entry.data()),
                    None => return false,
                }
            }
            _ => Arc::clone(entry.data()),
        };
        (data, cr3.as_ptr())
    };

    let mut data = data_arc.lock();

    // Verify the fault address is within a valid VMA
    if data.vmas.find(UserAddr::new(fault_addr)).is_none() {
        return false;
    }

    // Round down to 2MB boundary
    let page_2m = paging::PAGE_2M;
    let region_start = fault_addr & !(page_2m - 1);

    let reloc_index = data.reloc_index.clone();
    let elf_base = data.elf_base.raw();

    // If a 2MB page is already mapped at this region (from a previous fault
    // in a different VMA that shares the same 2MB range), just return success.
    if paging::virt_to_phys(pml4 as *const u64, UserAddr::new(region_start)).is_some() {
        return true;
    }

    // Allocate a zeroed 2MB physical page
    let alloc = match OwnedAlloc::new(page_2m as usize, page_2m as usize) {
        Some(a) => a,
        None => return false,
    };
    let phys = PhysAddr::from_ptr(alloc.ptr());
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
            crate::vma::VmaKind::FileBacked { block_map, file_offset, file_size } => {
                let mut cache_guard = crate::page_cache::lock();
                let (cache, dev) = cache_guard.cache_and_dev();

                // Walk 4KB blocks within the overlap of [region_start..region_end_full] and [vma_s..vma_e]
                let fill_start = region_start.max(vma_s);
                let fill_end = region_end_full.min(vma_e);
                let mut vaddr = fill_start & !0xFFF;

                while vaddr < fill_end {
                    let vma_offset = vaddr - vma_s;
                    let byte_offset = vma_offset + file_offset;
                    let block_idx = (byte_offset / 4096) as usize;
                    let page_offset = (vaddr - region_start) as usize;

                    if vma_offset < *file_size && block_idx < block_map.len() {
                        let cache_phys = cache.ensure_cached(dev, block_map[block_idx]);
                        let valid = if vma_offset + 4096 <= *file_size { 4096 } else { (*file_size - vma_offset) as usize };
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                cache_phys.as_ptr::<u8>(),
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
    if let Some(ref ri) = reloc_index {
        let mut offset = 0u64;
        while offset < page_2m {
            let page_elf_offset = (region_start + offset).wrapping_sub(elf_base);
            if ri.has_relocs_in_page(page_elf_offset) {
                ri.apply_to_page(page_elf_offset, unsafe { page_ptr.add(offset as usize) });
            }
            offset += 4096;
        }
    }

    // Map the 2MB page (writable if any overlapping VMA is writable)
    paging::remap_user_2m(pml4, UserAddr::new(region_start), phys, writable);
    cpu::flush_tlb();

    data.demand_allocs.push(alloc);

    true
}

/// Free a 4KB page at the given physical address.
// ---------------------------------------------------------------------------
// Crash diagnostics
// ---------------------------------------------------------------------------

/// Dump the page fault trace and memory around `fault_addr` for the current process.
/// Called from the exception handler on user-mode crashes.
pub fn dump_crash_diagnostics(fault_addr: u64, rip: u64) {
    let pid = current_pid();
    if pid == Pid::MAX { return; }

    let data_arc = {
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return };
        match table.get(pid) {
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
            log!("    fault={:#x} elf_off={:#x} blk={} relocs={} [{}]",
                rec.fault_addr, rec.page_elf_offset, rec.block_idx,
                rec.reloc_count, flags);
        }
    }

    // Dump memory around given addresses (if mapped in the process page tables)
    let pml4 = {
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return };
        let Some(entry) = table.get(pid) else { return };
        entry.cr3().map(|cr3| cr3.as_ptr())
    };
    let Some(pml4) = pml4 else { return };

    // Read a u64 from a user virtual address via page table translation.
    // Reads via the kernel direct map (no USER bit) to avoid SMAP faults.
    let read_user = |virt: u64| -> Option<u64> {
        if virt % 8 != 0 { return None; }
        let phys = paging::virt_to_phys(pml4 as *const u64, UserAddr::new(virt))?;
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
    let fs_base_saved = {
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return };
        table.get(pid).map(|e| e.fs_base()).unwrap_or(0)
    };
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
pub fn resolve_user_symbol(pid: Pid, addr: u64) -> bool {
    let data_arc = {
        let guard = PROCESS_TABLE.lock();
        let Some(table) = guard.as_ref() else { return false };
        match table.get(pid) {
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
    crate::pipe::cleanup_pml4(pml4);
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
