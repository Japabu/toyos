//! The process table, and what a process is made of.
//!
//! ## What a crash report may take here, and what it may not
//!
//! Every reader in this file but two takes [`PROCESS_TABLE`] with `lock()`. The
//! two that may not — [`dump_crash_diagnostics`] and [`try_for_each_thread`] —
//! are the ones a machine in trouble reaches, and their rule is a rule about the
//! whole path rather than about this file: **a crash report may not wait for any
//! lock, because the faulting thread may be holding it.**
//! `try_recover_from_panic` exists for exactly that thread, so a wait there is a
//! deadlock on the one path in the kernel that must always produce output; and
//! whatever holds this lock while the machine is stuck is a candidate for what
//! is stuck, which is the census's version of the same argument.
//!
//! `try_lock` is what that leaves, and a `try_lock` is a coin toss: the answer
//! it loses is not printed, and until 2026-08-22 the thing it lost was the
//! faulting function's *name*. So the rule has a second half, and it is the one
//! worth stating:
//!
//! > **A crash report does not ask this table for a symbol.** It reads the
//! > symbol table of the task it is reporting on, off that task's own record,
//! > with no lock in the path — [`resolve_user_symbol`], over
//! > `sched::driver::current_symbols`. A name is the part of a report a reader
//! > cannot reconstruct from anything else in it, so it may not be the part that
//! > depends on what some other CPU happened to be doing.
//!
//! What a report still takes from the table is [`dump_crash_diagnostics`]'s
//! page-fault trace — and it still `try_lock`s, still loses sometimes, and says
//! so in the report when it does. That is allowed because the trace is a
//! supplement: the report above it is complete without it. Anything that stops
//! being true of that — anything a reader could not reconstruct — has to leave
//! this table the way the names did.

use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::NonNull;
use crate::arch::percpu;
use crate::mm::paging::{CachePolicy, Prot, WindowProt};
use crate::mm::PAGE_2M;
use crate::object::{ops, HandleTable};
use crate::sync::Lock;
use crate::symbols::SymbolTable;
use crate::sched::payload::ThreadSched;
use crate::time::{Deadline, Duration};
use crate::{elf, pipe, scheduler};
use crate::{DirectMap, UserAddr};
use crate::loader::{
    setup_tls, setup_combined_tls, alloc_kernel_stack, thread_start,
};

pub use toyos_abi::{Pid, Tid};
pub use crate::scheduler::TaskId;
use toyos_abi::syscall::EndowEntry;

/// One `EndowEntry` on the wire. Named once, because both the reader in
/// `loader::start` and the writer in [`Endowments::encode`] index by it.
pub const ENDOW_ENTRY_LEN: usize = core::mem::size_of::<EndowEntry>();

// Re-export loader functions so existing callers (via `process::`) keep working.
pub use crate::loader::{build_child_handles, spawn, spawn_init, INIT_PATH};
pub(crate) use crate::loader::read_file_range;

/// Page tables shared between a process and all its threads.
pub type PageTables = Arc<Lock<crate::mm::paging::AddressSpace>>;

/// Allocate a virtual region and map physical memory into it.
/// Returns the allocated virtual address, or None if out of address space.
///
/// **`prot` used to be the constant `true`** — every caller here got a writable
/// mapping and, before `EFER.NXE` existed, an executable one: a TLS block, a
/// pipe's ring and an `io_uring`'s rings were all pages a program could jump
/// into. Each caller now says which of the three it means, and a library image
/// — the one whose pages do not agree — does not come through here at all.
pub fn vma_map(
    pt: &Lock<crate::mm::paging::AddressSpace>,
    phys: u64,
    size: u64,
    prot: Prot,
) -> Option<(UserAddr, u64)> {
    pt.lock().alloc_and_map(phys, size, prot, CachePolicy::DeferToMtrr)
}

// OwnedAlloc — RAII heap allocation (for kernel-only buffers < 2MB)

/// Move-only wrapper around a heap allocation. Drop calls dealloc.
/// For kernel-only buffers (kernel stacks, TLS templates). NOT for user-mapped pages.
pub struct OwnedAlloc {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl OwnedAlloc {
    /// `None` for any size the kernel heap cannot serve, so a caller sizing a
    /// buffer from untrusted input gets a value to handle rather than a panic.
    /// The heap's page source asserts above its ceiling (`mm::alloc`) instead
    /// of returning null, so that ceiling is checked before the request, not
    /// after.
    ///
    /// The ceiling is `mm::MAX_HEAP_ALLOC`, not `PAGE_2M`. Testing against
    /// `PAGE_2M` was short by dlmalloc's own bookkeeping: a request in
    /// `[PAGE_2M - overhead, PAGE_2M)` passed here and then made dlmalloc ask
    /// the page source for a 4 MiB granule, which is the assert this guard
    /// exists to keep unreachable. Reached from a `PT_TLS` `p_memsz` of
    /// `0x1F_FFF0`.
    pub fn new(size: usize, align: usize) -> Option<Self> {
        if size > crate::mm::MAX_HEAP_ALLOC { return None; }
        let layout = Layout::from_size_align(size, align).ok()?;
        // SAFETY: `alloc_zeroed` requires a non-zero-size layout, and
        // `Layout::from_size_align` has just produced this one from a size the
        // guard above bounded — a zero size is refused by the allocator by
        // returning null, which `NonNull::new` turns into `None` rather than a
        // dangling `OwnedAlloc`.
        //
        // Irreducible: this is the raw allocator, and the safe wrapper over it
        // is `Box`/`Vec`, neither of which can express "give me `size` bytes
        // at `align`, decided at run time, or tell me you cannot" — `Box`
        // needs a type and aborts instead of answering. Answering is the whole
        // reason this type exists (`PT_TLS` sizes come out of an ELF).
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })?;
        Some(Self { ptr, layout })
    }

    pub fn ptr(&self) -> *mut u8 { self.ptr.as_ptr() }
    pub fn size(&self) -> usize { self.layout.size() }

    /// A bounds-checked view of the first `len` bytes.
    ///
    /// `len` is checked against the allocation rather than trusted: the whole
    /// window comes from [`crate::mm::Allocation`] and `subslice` bounds the
    /// prefix against it, so a view longer than the buffer is a panic here and
    /// not an out-of-bounds write somewhere later.
    pub fn slice(&self, len: usize) -> crate::mm::KernelSlice {
        crate::mm::KernelSlice::whole(self).subslice(0, len)
    }
}

// SAFETY: `ptr()` is the address `alloc_zeroed` returned for `layout` and
// `size()` is `layout.size()` — the same `Layout`, read off the allocation
// rather than supplied beside it. `OwnedAlloc` is move-only, frees in its own
// `Drop` and hands out no copy of the pointer that outlives `&self`, so the
// bytes stay valid for as long as `self` does.
unsafe impl crate::mm::Allocation for OwnedAlloc {
    fn ptr(&self) -> *mut u8 { OwnedAlloc::ptr(self) }
    fn size(&self) -> usize { OwnedAlloc::size(self) }
}

impl Drop for OwnedAlloc {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`layout` are the pair `new` got back from
        // `alloc_zeroed` and neither field is reachable from outside this
        // module, so this is the same layout at the same address, freed once —
        // `OwnedAlloc` is move-only and has no `Clone`.
        //
        // Irreducible: `Drop` is where a raw allocation is returned, and the
        // safe alternative (`Box<[u8]>`) cannot carry a run-time alignment.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout); }
    }
}

// SAFETY: `OwnedAlloc` is `NonNull<u8>` plus its `Layout`, and it is `!Send`
// only because `NonNull` is. The bytes behind it are an ordinary kernel heap
// allocation with exactly one owner — the type is move-only, hands out no
// copy of the pointer that outlives `&self`, and frees in its own `Drop` — so
// moving that owner to another CPU carries the whole allocation with it and
// leaves nothing behind to race with.
//
// Irreducible: `NonNull`'s `!Send` is deliberate and this is the only way to
// say "this particular raw pointer is owned". Replacing it with a `Box<[u8]>`
// (which is `Send`) is what would make the impl unnecessary, and that is the
// run-time-alignment problem `new` above explains.
unsafe impl Send for OwnedAlloc {}

// PageAlloc — contiguous 2MB physical pages from PMM

/// Contiguous 2MB-aligned physical pages from PMM. Provides a kernel-accessible
/// pointer via the direct map. Pages are zeroed on allocation, freed on drop.
pub struct PageAlloc(Vec<crate::mm::pmm::PhysPage>);

impl PageAlloc {
    /// Allocate `size` bytes as contiguous 2MB pages.
    pub fn new(size: usize, cat: crate::mm::pmm::Category) -> Option<Self> {
        let count = size.div_ceil(PAGE_2M as usize);
        Some(Self(crate::mm::pmm::alloc_contiguous(count, cat)?))
    }

    /// Kernel pointer to the start of the allocation (via direct map).
    pub fn ptr(&self) -> *mut u8 {
        self.0[0].direct_map().as_mut_ptr()
    }

    /// This allocation as a bounds-checked window, sized from the pages it
    /// owns.
    ///
    /// **A window and not a `&mut [u8]`, and these pages are why.** A frame
    /// filled through this becomes a *user* mapping a few statements later, and
    /// a `&mut [u8]` over bytes a process maps writable carries `noalias` and
    /// `dereferenceable` into LLVM — the borrow [`UserBytes`]'s header exists to
    /// refuse. A [`KernelSlice`] hands out no reference: `copy_from` and
    /// `subslice` are a bounds-checked address and a raw copy, which is the
    /// shape [`UserBytesMut::write_at`] already uses on the other side of the
    /// same boundary. There is one such window type in this kernel and this is
    /// it; a fourth private one would have been a fourth thing to get right.
    ///
    /// [`KernelSlice`]: crate::mm::KernelSlice
    /// [`UserBytes`]: crate::user_ptr::UserBytes
    /// [`UserBytesMut::write_at`]: crate::user_ptr::UserBytesMut::write_at
    pub fn window(&self) -> crate::mm::KernelSlice {
        crate::mm::KernelSlice::whole(self)
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

// SAFETY: both halves come from the same `Vec<PhysPage>` this type owns and
// frees — `ptr()` is the direct-map address of the first page and `size()` is
// `len() * PAGE_2M` over a run `alloc_contiguous` returned, so the pages really
// are adjacent and the length really is the allocation's. The pages go back to
// the PMM when the `Vec` drops each `PhysPage`, which is when `self` dies, so
// they stay valid for exactly as long as `self` does.
unsafe impl crate::mm::Allocation for PageAlloc {
    fn ptr(&self) -> *mut u8 { PageAlloc::ptr(self) }
    fn size(&self) -> usize { PageAlloc::size(self) }
}

// MappedPages — physical pages plus the user address they were mapped at

/// Pages handed to userland through [`vma_map`], carried with the virtual
/// address that maps them.
///
/// `PageAlloc`'s Drop returns the pages to the PMM and reaches no address
/// space, so pages and mapping cannot be dropped as one. Holding the two
/// together is what makes the unmap expressible at all; enforcing it is the
/// `SharedToken`/RAII item in `issues/`.
///
/// Dropping without unmapping is only sound when the address space itself is
/// being destroyed (process teardown).
pub struct MappedPages {
    vaddr: UserAddr,
    pages: PageAlloc,
}

impl MappedPages {
    pub fn new(vaddr: UserAddr, pages: PageAlloc) -> Self {
        Self { vaddr, pages }
    }

    pub fn vaddr(&self) -> UserAddr { self.vaddr }

    /// Kernel pointer to the start, via the direct map.
    pub fn ptr(&self) -> *mut u8 { self.pages.ptr() }

    pub fn size(&self) -> usize { self.pages.size() }

    /// Take the mapping out of `addr_space` and hand back the pages, still
    /// reachable from every other CPU until the wrapper is dropped.
    ///
    /// The obligation used to be a sentence in this comment and is now the
    /// return type: `unmap`'s `invlpg` reaches this CPU only, and a sibling
    /// running another thread of the same process can still hold an entry for a
    /// page the PMM is about to reissue.
    fn unmap_from(
        self,
        addr_space: &mut crate::mm::paging::AddressSpace,
    ) -> crate::mm::Unmapped<PageAlloc> {
        addr_space.free_and_unmap(self.vaddr);
        crate::mm::Unmapped::new(self.pages)
    }

    pub fn release(self, pt: &PageTables) {
        // Two statements, because the shootdown inside the drop may not run
        // while the address-space lock is held: a sibling taking a page fault
        // spins on that lock with `IF` clear and could never acknowledge.
        let pages = self.unmap_from(&mut pt.lock());
        drop(pages);
    }
}

/// Release every user mapping an exiting thread owns — its own TLS block and
/// one per dlopen'd module it touched.
///
/// Sibling threads keep the address space alive past a thread exit, so a
/// mapping left behind here is a live user-writable window onto memory the PMM
/// has already reissued. Process teardown does not call this: there is no
/// address space left to unmap from.
///
/// `tls` arrives by value because the caller holds the `ProcessData` lock and
/// the `ThreadData` lock is never held at the same time. The pages go back out
/// for the same reason: freeing one waits for every other CPU to flush, and one
/// shootdown per block is what the caller pays — a thread owns its TLS and at
/// most one block per module it touched, and thread exit is not a hot path.
#[must_use]
fn release_thread_mappings(
    data: &mut ProcessData,
    tls: Option<MappedPages>,
    pt: &PageTables,
    tid: Tid,
) -> Vec<crate::mm::Unmapped<PageAlloc>> {
    let keys: Vec<(Tid, u64)> = data.elf.dynamic_tls_blocks.keys()
        .filter(|&&(t, _)| t == tid)
        .copied()
        .collect();
    let mut pages = Vec::with_capacity(keys.len() + 1);
    {
        let mut addr_space = pt.lock();
        pages.extend(tls.map(|t| t.unmap_from(&mut addr_space)));
        for key in keys {
            let block = data.elf.dynamic_tls_blocks.remove(&key).unwrap();
            pages.push(block.unmap_from(&mut addr_space));
            data.free_count += 1;
        }
    }
    pages
}

pub const KERNEL_STACK_SIZE: usize = 128 * 1024;

/// Type-safe user stack. Knows its virtual address (what userland sees) and the
/// kernel window onto the same pages. Impossible to confuse the two.
///
/// The window is the stack's [`PageAlloc`]'s own, so the size the writes are
/// bounded against is the allocation's and not a constant repeated beside it.
pub struct UserStack {
    vaddr: UserAddr,
    window: crate::mm::KernelSlice,
}

impl UserStack {
    pub fn new(vaddr: UserAddr, window: crate::mm::KernelSlice) -> Self {
        Self { vaddr, window }
    }

    /// User-visible virtual address of the stack top (highest address).
    pub fn top(&self) -> u64 { self.vaddr.raw() + self.size() }

    /// User-visible virtual base address.
    pub fn base(&self) -> UserAddr { self.vaddr }

    pub fn size(&self) -> u64 { self.window.size() as u64 }

    /// Copy `src` onto this stack, at the *user* address `user_addr`.
    ///
    /// **The whole write is bounds-checked, where the `kern_ptr` accessor this
    /// replaced checked only the address it handed back.** Every caller then
    /// wrote `n` bytes through that pointer and nothing anywhere checked `n`:
    /// the argv strings ran one byte past the checked address, the metadata
    /// block `args.len() + 2` words past it. Both were in fact inside the
    /// stack — `write_argv` derives every offset by subtracting from `top()` —
    /// but that was an argument, not a check, and it is a check now.
    ///
    /// The check is [`crate::mm::KernelSlice`]'s rather than one of this type's
    /// own: a bounded window over kernel pages that become a user mapping is
    /// one problem, not four, and a stack is that problem exactly as a
    /// demand-paged frame is.
    ///
    /// Panics rather than refuses because `user_addr` and `src.len()` are the
    /// kernel's own arithmetic over its own freshly allocated pages, never a
    /// number that crossed the boundary.
    fn write_at(&self, user_addr: u64, src: &[u8]) {
        let offset = user_addr.checked_sub(self.vaddr.raw())
            .expect("UserStack: address below stack base");
        // SAFETY: `copy_from` asserts `offset + src.len() <= size` against the
        // stack's own allocation, so the write lands inside the pages
        // `PageAlloc::window` was built from. `src` is a slice the caller owns
        // and the destination is kernel-only until `loader::start` hands the
        // address space to Ring 3, so the ranges cannot overlap and nothing
        // else is looking.
        //
        // Irreducible: the destination becomes a user mapping, so the safe
        // spelling — a `&mut [u8]` — is the borrow `user_ptr.rs`'s header
        // refuses. One raw copy behind one bounds check is the shape
        // [`crate::user_ptr::UserBytesMut`] uses for the same problem.
        unsafe { self.window.copy_from(offset as usize, src) };
    }

    /// Write argc, argv pointers, and string data onto this stack.
    /// Returns the new user-visible stack pointer.
    pub fn write_argv(&self, args: &[&str]) -> u64 {
        let mut sp = self.top();
        let mut argv_ptrs: Vec<u64> = Vec::with_capacity(args.len());
        for arg in args.iter().rev() {
            sp -= (arg.len() + 1) as u64;
            self.write_at(sp, arg.as_bytes());
            self.write_at(sp + arg.len() as u64, &[0u8]);
            argv_ptrs.push(sp);
        }
        argv_ptrs.reverse();
        let metadata_qwords = args.len() + 2;
        sp = (sp - metadata_qwords as u64 * 8) & !15;
        self.write_at(sp, &(args.len() as u64).to_ne_bytes());
        for (i, ptr) in argv_ptrs.iter().enumerate() {
            self.write_at(sp + (1 + i as u64) * 8, &ptr.to_ne_bytes());
        }
        self.write_at(sp + (1 + args.len() as u64) * 8, &0u64.to_ne_bytes());
        sp
    }
}

/// Where a *thread* is in its lifecycle.
///
/// **A process has no such state.** Its exit code lives on its
/// [`ProcessObject`], published once and readable for ever after, so the table
/// never holds a corpse waiting for somebody entitled to claim it. A thread
/// still has one, because `SYS_THREAD_JOIN` reads it out of the table and a
/// `Tid` names nothing outside its own process.
///
/// For a live thread the scheduler is authoritative about running, ready or
/// blocked — `scheduler::task_sched_state()` has that detail.
///
/// [`ProcessObject`]: crate::object::process::ProcessObject
#[derive(Clone, Copy, PartialEq)]
pub enum ThreadLocation {
    /// Alive: running, ready, or blocked. The scheduler owns the detail.
    Scheduled,
    /// Exited with the given code, waiting for its joiner.
    Zombie(i32),
}

impl ThreadLocation {
}

// ProcessEntry + ThreadEntry — hierarchical process/thread table

/// How much of a process or thread name the table keeps.
///
/// Fixed by the `SYS_SYSINFO` record's name field, which is the syscall that
/// exists to report it: anything longer would not survive being asked for.
pub const THREAD_NAME_LEN: usize = 28;

/// Per-thread metadata. Tid is the HashMap key in ProcessEntry.threads.
pub struct ThreadEntry {
    state: ThreadLocation,
    name: [u8; THREAD_NAME_LEN],
    thread_data: Arc<Lock<ThreadData>>,
    /// The thread's scheduler faces (rendezvous word + published counters).
    /// `None` only between the table insert that allocates the tid and the
    /// `sched::spawn` that needs it — the task cannot exist before its own id.
    sched: Option<ThreadSched>,
}

impl ThreadEntry {
    pub fn new(thread_data: Arc<Lock<ThreadData>>) -> Self {
        Self { state: ThreadLocation::Scheduled, name: [0u8; THREAD_NAME_LEN], thread_data, sched: None }
    }
    pub fn set_sched(&mut self, sched: ThreadSched) {
        assert!(self.sched.is_none(), "thread already has a scheduler record");
        self.sched = Some(sched);
    }
    pub fn sched(&self) -> Option<&ThreadSched> { self.sched.as_ref() }
    pub fn state(&self) -> ThreadLocation { self.state }
    pub fn name(&self) -> &[u8; THREAD_NAME_LEN] { &self.name }
    pub fn name_str(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("?").trim_end_matches('\0')
    }
    pub fn set_name(&mut self, name: &[u8]) {
        self.name = [0u8; THREAD_NAME_LEN];
        let len = name.len().min(THREAD_NAME_LEN);
        self.name[..len].copy_from_slice(&name[..len]);
    }
    pub fn thread_data(&self) -> &Arc<Lock<ThreadData>> { &self.thread_data }
}

/// A process and all its threads. Removing a ProcessEntry removes all threads.
pub struct ProcessEntry {
    pid: Pid,
    /// The thing a handle to this process names, and where its exit code goes.
    ///
    /// The entry holds it, not the other way round: an object outlives the
    /// entry for exactly as long as somebody still holds a handle, and a
    /// process nobody kept a handle to leaves nothing behind at all.
    object: Arc<crate::object::process::ProcessObject>,
    name: [u8; THREAD_NAME_LEN],
    process_data: Arc<Lock<ProcessData>>,
    /// This process's backtrace symbols, and **no `Lock` around them**.
    ///
    /// A `SymbolTable` is written once, by the loader, and read-only for the
    /// rest of its life, so the only thing a lock ever guarded was the eager
    /// release at teardown — which `teardown_bookkeeping` now performs by
    /// dropping this reference instead. What that buys is the whole point: every
    /// thread of this process carries a clone of this `Arc` on its own task
    /// record, so a crash report reaches the names through the task it is
    /// reporting on and never through the table (see this module's header).
    symbols: Arc<SymbolTable>,
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
        name: [u8; THREAD_NAME_LEN],
        process_data: Arc<Lock<ProcessData>>,
        symbols: Arc<SymbolTable>,
        main_thread: ThreadEntry,
    ) -> Self {
        let mut threads = crate::id_map::IdMap::new();
        let main_tid = threads.insert(main_thread);
        Self {
            pid,
            object: crate::object::process::ProcessObject::new(pid),
            name,
            process_data,
            symbols,
            main_tid,
            threads,
            tearing_down: false,
        }
    }
    pub fn pid(&self) -> Pid { self.pid }
    pub fn object(&self) -> &Arc<crate::object::process::ProcessObject> { &self.object }
    pub fn name(&self) -> &[u8; THREAD_NAME_LEN] { &self.name }
    pub fn name_str(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("?").trim_end_matches('\0')
    }
    pub fn process_data(&self) -> &Arc<Lock<ProcessData>> { &self.process_data }
    pub fn main_tid(&self) -> Tid { self.main_tid }
    pub fn threads(&self) -> &crate::id_map::IdMap<Tid, ThreadEntry> { &self.threads }
    pub fn threads_mut(&mut self) -> &mut crate::id_map::IdMap<Tid, ThreadEntry> { &mut self.threads }
}

impl ProcessEntry {
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
        // The scheduler's per-process vruntime entry must live exactly as long
        // as the table entry: threads of a zombified process still yield inside
        // their own exit path, and the Yield re-insert reads the entry.
        scheduler::remove_vruntime(self.pid);
    }
}

// ProcessData — per-process data behind Arc<Lock<ProcessData>>

/// Record of a single demand-paged fault, stored in a ring buffer for crash diagnostics.
#[derive(Clone, Copy)]
pub struct PageFaultRecord {
    pub fault_addr: u64,
    pub page_elf_offset: u64,
    pub block_idx: u32,
    pub reloc_count: u16,
    // bit 0: writable, bit 1: has_relocs, bit 2: anonymous, bit 3: beyond_extent,
    // bit 4: executable. Bits 0 and 4 come from one `Prot` and are never both
    // set — a line carrying both is a kernel that has lost W^X.
    pub flags: u16,
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

/// ELF loading artifacts and TLS state.
pub struct ElfInfo {
    pub elf_alloc: Option<OwnedAlloc>,
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
    pub dynamic_tls_blocks: alloc::collections::BTreeMap<(Tid, u64), MappedPages>,
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

impl ElfInfo {
    /// The state of a process that has no ELF at all — a kernel thread
    /// (`sched::kthread`).
    ///
    /// Not `Default`, because the only honest default of `next_tls_module_id`
    /// is 1 rather than 0 and a derive would silently pick the other. Written
    /// here rather than at the one call site so that a field added to
    /// [`ElfInfo`] stops *this* build too.
    pub fn none() -> Self {
        Self {
            elf_alloc: None,
            tls_template: None,
            tls_memsz: 0,
            tls_modules: Vec::new(),
            tls_total_memsz: 0,
            tls_max_align: 0,
            next_tls_module_id: 1,
            dynamic_tls_blocks: alloc::collections::BTreeMap::new(),
            loaded_libs: Vec::new(),
            reloc_index: None,
            elf_base: UserAddr::new(0),
            exe_eh_frame_hdr_vaddr: 0,
            exe_eh_frame_hdr_size: 0,
            exe_vaddr_max: 0,
            lib_paths: Vec::new(),
        }
    }
}

/// Process-level data shared across all threads via `Arc<Lock<ProcessData>>`.
/// Contains handles, memory mappings, ELF state, accounting — everything that belongs to the process.
/// Accessed via `with_process_data`. All threads of a process share the same Arc.
pub struct ProcessData {
    /// Every kernel object this process can name. Stdio is slots 0, 1 and 2 at
    /// generation 0, which is what makes those handles literally `0`, `1`, `2`.
    pub handles: HandleTable,
    pub cwd: String,
    /// Inherited environment variables (KEY=VALUE\0KEY2=VALUE2\0...)
    pub env: Vec<u8>,

    /// ELF loading artifacts and TLS state.
    pub elf: ElfInfo,

    pub mmap_regions: Vec<MmapRegion>,
    /// Live `SYS_PIPE_MAP` windows. See [`PipeMap`].
    pub pipe_maps: Vec<PipeMap>,
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

    pub spawn_ns: u64,
    pub accounting: ProcessAccounting,
    /// What this process's parent called each handle it moved in at spawn.
    pub endowments: Endowments,
}

/// The labels a parent put on the handles it endowed, and nothing else.
///
/// **Names, not authority.** The handles are in the table whether or not the
/// child ever asks; this is only how it learns which of its slots its parent
/// meant by `serve:compositor`. Written once by spawn and freed with the
/// process, which is the whole of the per-process state the endowment design
/// adds.
pub struct Endowments {
    entries: alloc::boxed::Box<[EndowEntry]>,
    labels: alloc::boxed::Box<[u8]>,
}

impl Endowments {
    pub fn empty() -> Self {
        Self { entries: alloc::boxed::Box::new([]), labels: alloc::boxed::Box::new([]) }
    }

    pub fn new(entries: Vec<EndowEntry>, labels: Vec<u8>) -> Self {
        Self { entries: entries.into_boxed_slice(), labels: labels.into_boxed_slice() }
    }

    /// Bytes [`SYS_ENDOWMENTS`] answers with: a `u64` count, the entries, then
    /// the blob their offsets index.
    ///
    /// [`SYS_ENDOWMENTS`]: toyos_abi::syscall::SYS_ENDOWMENTS
    pub fn encoded_len(&self) -> usize {
        8 + self.entries.len() * ENDOW_ENTRY_LEN + self.labels.len()
    }

    /// Render into `out`, which the caller has sized at [`Self::encoded_len`].
    ///
    /// Field by field rather than a transmute of the slice: `EndowEntry`'s
    /// padding word is written as a zero here, so nothing of the kernel's
    /// reaches a child in it.
    pub fn encode(&self, out: &mut [u8]) {
        out[..8].copy_from_slice(&(self.entries.len() as u64).to_ne_bytes());
        for (i, entry) in self.entries.iter().enumerate() {
            let at = 8 + i * ENDOW_ENTRY_LEN;
            out[at..at + 4].copy_from_slice(&entry.label_off.to_ne_bytes());
            out[at + 4..at + 8].copy_from_slice(&entry.label_len.to_ne_bytes());
            out[at + 8..at + 12].copy_from_slice(&entry.handle.0.to_ne_bytes());
            out[at + 12..at + 16].copy_from_slice(&0u32.to_ne_bytes());
        }
        let blob = 8 + self.entries.len() * ENDOW_ENTRY_LEN;
        out[blob..blob + self.labels.len()].copy_from_slice(&self.labels);
    }
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
    pub tls_pages: Option<MappedPages>,
    /// Main thread's user stack, at the fixed `vma::STACK_BASE`. Only ever
    /// freed with the address space it lives in, so it needs no address here.
    pub stack_pages: Option<PageAlloc>,
    // User stack location (for SYS_STACK_INFO)
    pub user_stack_base: UserAddr,
    pub user_stack_size: u64,
    /// Syscall counts per syscall number (for profiling)
    pub syscall_counts: [u32; toyos_abi::syscall::SYSCALL_PROFILE_BINS],
    pub syscall_total: u64,
    /// Wall-clock nanoseconds spent in syscall dispatch (includes preemption time)
    pub syscall_total_ns: u64,
}

/// One process's window onto a pipe's ring page, from `SYS_PIPE_MAP`.
///
/// Recorded so it can be taken away. The page belongs to the pipe, and the
/// pipe is freed the moment its last reader and writer reference drop — so a
/// mapping that outlives the process's handles is a writable window onto
/// memory the PMM has already handed to something else. Nothing on the
/// close path used to touch it.
///
/// One entry per *pipe*, not per call: `sys_pipe_map` returns the window it
/// already made. That is what bounds this vector — every entry needs a live
/// handle naming its pipe, and distinct pipes need distinct handles, so it can
/// never hold more than `object::handle::MAX_HANDLES` entries.
pub struct PipeMap {
    pub pipe: pipe::PipeId,
    pub addr: UserAddr,
}

/// Take back every window onto `pipe` that `maps` holds.
///
/// Called when the process's last handle for the pipe goes: from there on
/// nothing keeps the ring page alive, so the mapping has to stop before the
/// page can be reissued.
pub fn revoke_pipe_maps(maps: &mut Vec<PipeMap>, pt: &PageTables, pipe: pipe::PipeId) {
    if !maps.iter().any(|m| m.pipe == pipe) {
        return;
    }
    {
        let mut addr_space = pt.lock();
        maps.retain(|m| {
            if m.pipe != pipe {
                return true;
            }
            addr_space.free_and_unmap(m.addr);
            false
        });
    }
    // Outside the block, because it waits: the lock this CPU would still be
    // holding is one a sibling can be spinning on with `IF` clear.
    crate::arch::tlb::shootdown();
}

/// One live `mmap`, and the physical pages behind it.
///
/// The range itself is registered in the address space's `regions`, which is
/// what the placement search reads and what `munmap` frees; this is the
/// ownership of the memory and the accounting, and every entry here has a
/// region of exactly its extent. It carried a `fixed` flag once, for a second
/// `munmap` path that unmapped a placed mapping without unregistering it —
/// there was nothing registered to unregister, which was the defect.
pub struct MmapRegion {
    pub addr: UserAddr,
    pub size: usize,
    /// `None` for a `MmapProt::NONE` mapping: the range is reserved so nothing
    /// else is placed in it, and no physical memory backs a page whose whole
    /// purpose is to fault.
    pub _pages: Option<PageAlloc>,
}

// IdleProof — zero-cost proof that code runs on the per-CPU idle stack

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

// Process table — IdMap<Pid, ProcessEntry> with lifecycle operations

pub type ProcessTable = crate::id_map::IdMap<Pid, ProcessEntry>;

pub static PROCESS_TABLE: Lock<Option<ProcessTable>> = Lock::new(None);

pub fn init() {
    *PROCESS_TABLE.lock() = Some(ProcessTable::new());
}

/// One thread as a diagnostic sees it: who it is, and what the scheduler's
/// cross-CPU-readable face says about it.
pub struct ThreadCensus<'a> {
    pub pid: Pid,
    pub tid: Tid,
    pub process: &'a str,
    pub thread: &'a str,
    pub zombie: Option<i32>,
    /// One of `sched::payload`'s `SCHED_*`. `None` is the window between the
    /// table insert that allocates the tid and the `sched::spawn` that mints
    /// the task — a thread that has no scheduler record cannot be scheduled,
    /// which is a state worth naming rather than eliding.
    pub sched: Option<u8>,
    /// Zero means the thread has never executed a user instruction.
    pub cpu_ns: u64,
}

/// Walk every thread in the table, or answer `false` because the table is
/// held.
///
/// `try_lock` and not `lock`: the one time anybody asks is when the machine is
/// stuck, and whatever holds this lock is a candidate for what is stuck.
pub fn try_for_each_thread(mut f: impl FnMut(ThreadCensus<'_>)) -> bool {
    let Some(guard) = PROCESS_TABLE.try_lock() else {
        return false;
    };
    let Some(table) = guard.as_ref() else {
        return false;
    };
    for (pid, proc) in table.iter() {
        for (tid, thread) in proc.threads().iter() {
            let sched = thread.sched();
            f(ThreadCensus {
                pid,
                tid,
                process: proc.name_str(),
                thread: thread.name_str(),
                zombie: match thread.state() {
                    ThreadLocation::Zombie(code) => Some(code),
                    ThreadLocation::Scheduled => None,
                },
                sched: sched.map(|s| s.sched_state()),
                cpu_ns: sched.map_or(0, |s| s.handle.cpu_ns()),
            });
        }
    }
    true
}

/// The object a handle to `pid` would name, for a process still in the table.
pub fn process_object(pid: Pid) -> Option<Arc<crate::object::process::ProcessObject>> {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref()?;
    table.get(pid).map(|proc| Arc::clone(proc.object()))
}

/// Accounting for a process, from wherever the numbers are.
///
/// A live one is sampled from its own `ProcessData` and its threads' published
/// counters; an exited one answers with what its teardown took. `None` is the
/// window between the two, which is the process being torn down right now.
pub fn stats_of(
    object: &crate::object::process::ProcessObject,
) -> Option<toyos_abi::syscall::ProcessStats> {
    if let Some(stats) = object.final_stats() {
        return Some(stats);
    }
    let pid = object.pid();
    // Nothing is locked under the table lock: the two other locks are taken
    // after it is given up, so this adds no ordering edge.
    let (data_arc, cpu_ns, threads) = {
        let guard = PROCESS_TABLE.lock();
        let proc = guard.as_ref()?.get(pid)?;
        let mut cpu_ns = 0;
        let mut threads = Vec::new();
        for (_, thread) in proc.threads().iter() {
            cpu_ns += thread.sched().map_or(0, scheduler::task_cpu_ns);
            threads.push(Arc::clone(thread.thread_data()));
        }
        (Arc::clone(proc.process_data()), cpu_ns, threads)
    };
    let (mut syscall_total, mut syscall_total_ns) = (0, 0);
    for tdata in threads {
        let tdata = tdata.lock();
        syscall_total += tdata.syscall_total;
        syscall_total_ns += tdata.syscall_total_ns;
    }
    let data = data_arc.lock();
    Some(stats_from(&data, pid, cpu_ns, syscall_total, syscall_total_ns))
}

/// Mark one thread dead.
///
/// Idempotent, and silent about an entry that has gone: a main thread reaches
/// this after its own process published its exit, by which point any idle pass
/// may already have reaped the entry.
pub fn mark_thread_zombie(table: &mut ProcessTable, pid: Pid, tid: Tid, code: i32) {
    let Some(proc) = table.get_mut(pid) else { return };
    let Some(thread) = proc.threads.get_mut(tid) else { return };
    if !matches!(thread.state, ThreadLocation::Zombie(_)) {
        thread.state = ThreadLocation::Zombie(code);
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

/// What a thread that died in panic recovery leaves to be cleaned up.
///
/// The panic path itself cannot do any of it — it may hold any lock the faulted
/// thread was holding — so it only records the thread in the poison set and
/// this runs later, from the idle loop. Both wakes are carried out rather than
/// performed here, because both must happen with the table lock given up.
#[must_use = "a poisoned thread's waiter must be woken"]
pub enum PoisonWake {
    /// A child thread died. **The pair names the thread that died**, which is
    /// what a `thread_join` arms on now — it used to name the process's main
    /// thread, because the wake was by name into a shared parking lot and
    /// whoever was woken re-checked.
    Joiner(Pid, Tid),
    /// The main thread died, so the process is over. The exit is published on
    /// the object — outside the table lock, like every other publish — and
    /// whoever holds a handle reads it there.
    Process(Arc<crate::object::process::ProcessObject>),
}

/// Mark a poisoned thread dead and name what must be woken for it.
///
/// `None` means there is nothing to do: the entry is gone, or another path
/// already owns this process's teardown and will publish its exit.
#[must_use = "a poisoned thread's waiter must be woken"]
pub fn zombify_poisoned(table: &mut ProcessTable, pid: Pid, tid: Tid) -> Option<PoisonWake> {
    let proc = table.get_mut(pid)?;
    if tid != proc.main_tid {
        let thread = proc.threads.get_mut(tid)?;
        if !matches!(thread.state, ThreadLocation::Zombie(_)) {
            thread.state = ThreadLocation::Zombie(-1);
        }
        return Some(PoisonWake::Joiner(pid, tid));
    }
    // The same claim every exit and kill takes, for the same reason: exactly
    // one path publishes one exit.
    if !proc.claim_teardown() {
        return None;
    }
    proc.threads.get_mut(tid)?.state = ThreadLocation::Zombie(-1);
    // No `teardown_resources` runs on this path, so the process's mappings and
    // handles go with the table entry rather than before it. That is the
    // pre-existing cost of a panic recovery: this is reached from the idle loop
    // and every release below it wants a lock the faulted thread may still be
    // recorded as holding.
    Some(PoisonWake::Process(Arc::clone(&proc.object)))
}

/// Take every entry whose process has published its exit.
///
/// **The whole of what replaced reaping.** Nobody has to be entitled to this,
/// there is no orphan to adopt and nothing is kept for anyone to read later:
/// the exit code and the final accounting are on the `ProcessObject`, which
/// outlives the entry for as long as a handle to it does. The [`IdleProof`]
/// stays, and for its original reason — an entry owns its threads, so this may
/// not run on one of them.
///
/// The entries come back rather than being dropped here, because the caller
/// holds the table lock and an entry's drop reaches `remove_vruntime` and — for
/// a process whose teardown never ran — the whole of its `ProcessData`.
#[must_use = "the reaped entries must be dropped outside the table lock"]
pub fn reap_finished(table: &mut ProcessTable, _proof: IdleProof) -> Vec<ProcessEntry> {
    let finished: Vec<Pid> = table
        .iter()
        .filter(|(_, p)| p.object.finished())
        .map(|(pid, _)| pid)
        .collect();
    finished.into_iter().filter_map(|pid| table.remove(pid)).collect()
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

// Access patterns — ProcessData (clone Arc, drop table lock, lock ProcessData)

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
pub fn process_data() -> Arc<Lock<ProcessData>> {
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
pub fn with_process_data<R>(f: impl FnOnce(&mut ProcessData) -> R) -> R {
    let arc = process_data();
    let mut guard = arc.lock();
    f(&mut guard)
}

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
        // A thread is spawned by a running thread, which by construction has an
        // address space: `None` here is "no task is running", and this is one.
        let addr_space = scheduler::current_address_space()
            .expect("spawn_thread: the spawning thread runs in an address space");
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
    let (tls_alloc, fs_base) = {
        let addr_space = &parent_addr_space;
        let parent_data = process_data_arc.lock();
        let tls_phys = tls_alloc.phys();
        // VA exhaustion is a resource failure a process can reach by spawning
        // threads until its range is gone, not a kernel bug. `tls_alloc`
        // drops on the way out, returning its pages.
        let (tls_vaddr, _) = vma_map(addr_space, tls_phys, tls_alloc.size() as u64, Prot::ReadWrite)?;
        // Rebase fs_base and internal TLS pointers from physical to virtual
        let tls_rebase = tls_vaddr.raw() as i64 - tls_phys as i64;
        let fs_base = (fs_base as i64 + tls_rebase) as u64;
        // SAFETY: `tls_alloc` is the block `setup_tls`/`setup_combined_tls`
        // just built and this scope still owns it, so every address below is
        // inside one contiguous allocation reached through the direct map, and
        // no other CPU can see it — `vma_map` has published the *virtual*
        // range into an address space whose only thread has not been created
        // yet. The offsets are the ones the same two functions wrote: `fs_base
        // - tls_vaddr` is the thread pointer they placed, and the DTV's length
        // at word 1 is a count they wrote, not one read back from userland.
        //
        // Irreducible: what this does is rewrite pointers *stored inside* a
        // TLS image from physical to virtual, so it walks a self-referential
        // structure by address. There is no Rust type for a DTV whose entries
        // point into itself, and building one would be modelling the psABI's
        // memory layout rather than writing it — the same untyped-by-nature
        // work `elf::reloc` does one level down.
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
        (MappedPages::new(tls_vaddr, tls_alloc), fs_base)
    };

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(thread_start, entry, stack_ptr, arg) {
        Some(ks) => ks,
        None => {
            tls_alloc.release(&parent_addr_space);
            return None;
        }
    };

    // Phase 3: Insert into table (brief table lock)
    // Threads share the parent's ProcessData Arc — no empty handle table or zeroed process fields.
    let thread_data = Arc::new(Lock::new(ThreadData {
        tls_pages: Some(tls_alloc),
        stack_pages: None,
        user_stack_base: UserAddr::new(stack_base),
        user_stack_size: if stack_base > 0 { stack_ptr - stack_base } else { 0 },
        syscall_counts: [0; toyos_abi::syscall::SYSCALL_PROFILE_BINS],
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
    // Every thread of a process names the same symbols, so a crash report on any
    // of them reads its own process's names without asking this table.
    let symbols = Arc::clone(&proc.symbols);
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
        symbols,
    );
    proc.threads.get_mut(tid).unwrap().set_sched(sched);
    drop(guard);
    Some(tid)
}


/// Tear down a process: zombie all its threads, free all resources, wake parent.
/// Called in two phases:
/// - Phase 1 (resource cleanup): ProcessData lock held, table lock NOT held.
/// - Phase 2 (scheduling): table lock held through context switch.
///
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

    if data.peak_memory > 0 || data.alloc_count > 0 {
        log!("memory: pid={pid} peak={}MB allocs={} frees={}",
            data.peak_memory / (1024 * 1024), data.alloc_count, data.free_count);
    }

    ops::close_all(&mut data.handles);
    data.elf.elf_alloc.take();
    data.elf.loaded_libs.clear();
    data.mmap_regions.clear();
    data.pipe_maps.clear();
    data.demand_pages.clear();
    data.elf.reloc_index = None;

    (syscall_total, syscall_total_ns)
}

/// Table-side teardown bookkeeping: mark remaining thread entries zombie and
/// drop the symbol table.
///
/// Caller must hold the PROCESS_TABLE lock, have claimed teardown, retired
/// every other thread of the process (so none can run), and freed resources.
/// Returns the process's object, whose exit the caller publishes once the table
/// lock is given up.
///
/// It used to hand back the shared regions to free outside the lock too. There
/// is no pid sweep left to do: a region is an object, its mappings go with the
/// last handle, and `close_all` in phase 3 is what releases them.
#[must_use = "the exit must be published on the object returned"]
fn teardown_bookkeeping(table: &mut ProcessTable, process_pid: Pid, code: i32,
                        main_cpu_ns: u64)
                        -> Arc<crate::object::process::ProcessObject> {
    let proc = table.get_mut(process_pid)
        .expect("teardown_bookkeeping: process not found");
    let main_tid = proc.main_tid;

    let tids: Vec<Tid> = proc.threads.iter().map(|(tid, _)| tid).collect();
    for tid in tids {
        let thread = proc.threads.get_mut(tid).unwrap();
        if !matches!(thread.state, ThreadLocation::Zombie(_)) {
            thread.state = ThreadLocation::Zombie(if tid == main_tid { code } else { -1 });
        }
    }

    // The symbol table is megabytes of the process's own pages now that it is
    // read off the binary rather than pointed at in the initrd, and a dead
    // process has no backtrace left for anyone to take — every caller of
    // `resolve_user_symbol` is a crash report, which runs on the live process
    // before this.
    //
    // Dropping this reference is the release, and the pages go with the *last*
    // one: phase 2 has already retired every other thread of this process, so
    // what is left holding a clone is the thread running this line, whose
    // payload `Hw::release` drops as it leaves the CPU. That is later than the
    // in-place empty this replaced, by one exit pass, and it is the property
    // that makes the lock-free read sound — a report cannot be reading a table
    // whose owner is off every CPU.
    proc.symbols = Arc::new(SymbolTable::empty());

    let cpu_ms = main_cpu_ns / 1_000_000;
    let name = proc.name_str();
    log!("exit: {name} pid={process_pid} code={code} cpu={cpu_ms}ms");

    Arc::clone(&proc.object)
}

/// The accounting a process leaves behind, for `SYS_PROCESS_STATS` to answer
/// after it is gone.
///
/// Must run after `teardown_scheduling`, which is what flushes the child
/// threads' counters into `ProcessData`.
fn final_stats(
    process_data_arc: &Arc<Lock<ProcessData>>,
    pid: Pid,
    syscall_total: u64,
    syscall_total_ns: u64,
    main_cpu_ns: u64,
) -> toyos_abi::syscall::ProcessStats {
    let data = process_data_arc.lock();
    stats_from(&data, pid, main_cpu_ns, syscall_total, syscall_total_ns)
}

/// One `ProcessStats`, from a process's own data.
///
/// Written once because `SYS_PROCESS_STATS` samples a live process through the
/// same fields the teardown snapshots, and two spellings of one record is two
/// records that can disagree.
pub fn stats_from(
    data: &ProcessData,
    pid: Pid,
    cpu_ns: u64,
    syscall_total: u64,
    syscall_total_ns: u64,
) -> toyos_abi::syscall::ProcessStats {
    use toyos_abi::syscall::ProcessStats;
    let acct = &data.accounting;
    ProcessStats {
        wall_ns: crate::clock::nanos_since_boot().saturating_sub(data.spawn_ns),
        cpu_ns: cpu_ns + acct.child_threads_cpu_ns,
        syscall_total,
        syscall_total_ns,
        fault_demand_count: acct.fault_demand_count,
        fault_zero_count: acct.fault_zero_count,
        fault_ns: acct.fault_ns,
        io_read_ops: acct.io_read_ops,
        pid: pid.raw(),
        io_read_bytes: acct.io_read_bytes,
        blocked_io_ns: acct.blocked_io_ns,
        blocked_futex_ns: acct.blocked_futex_ns,
        blocked_pipe_ns: acct.blocked_pipe_ns,
        blocked_ipc_ns: acct.blocked_ipc_ns,
        blocked_other_ns: acct.blocked_other_ns,
        runqueue_wait_ns: acct.runqueue_wait_ns,
        peak_memory: data.peak_memory,
        alloc_count: data.alloc_count,
    }
}

pub fn exit(code: i32) -> ! {
    release_process(code);
    scheduler::exit_current(code);
}

/// Everything a process's own teardown gives back — and it **returns**, which
/// is the whole reason it is a function.
///
/// `exit_current` never comes back and nothing here unwinds, so a value still
/// live at that call is a value nothing will ever drop: the kernel stack it
/// sits on is freed by the exit pass without running a destructor. Three `Arc`s
/// were — the [`ProcessObject`], the process's `ProcessData` and the exiting
/// thread's `ThreadData` — which leaked one live kernel object per process the
/// machine had ever run, and the per-variant census is what saw it. A scope
/// that ends is the only thing that releases them, and a caller that diverges
/// has none.
///
/// [`ProcessObject`]: crate::object::process::ProcessObject
fn release_process(code: i32) {
    let process_pid = current_process();
    let tid = current_tid();

    // Phase 1: claim teardown. Exactly one exit/kill path tears a process
    // down; later arrivals just exit their own thread — the claimant's
    // retire sweep accounts for them like any other thread.
    let (process_data_arc, thread_data_arc, main_tid, other_tids) = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let Some(proc) = table.get_mut(process_pid) else {
            drop(guard);
            crate::mm::paging::activate_kernel();
            return;
        };
        if proc.threads.get(tid).is_none() || !proc.claim_teardown() {
            drop(guard);
            crate::mm::paging::activate_kernel();
            return;
        }
        let other_tids: Vec<Tid> = proc.threads.iter()
            .map(|(t, _)| t)
            .filter(|&t| t != tid)
            .collect();
        let thread = proc.threads.get(tid).unwrap();
        (Arc::clone(&proc.process_data), Arc::clone(&thread.thread_data),
         proc.main_tid, other_tids)
    };

    crate::mm::paging::activate_kernel();

    // Phase 2: retire every other thread. Each one is provably out of the
    // scheduler when retire_task returns — not queued, not parked, not
    // mid-steal, not running. Only then may memory their page tables still map
    // be freed: a thread still running writes through those stale mappings into
    // 2MB frames the PMM has already re-issued.
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

    // Phase 4: table bookkeeping (thread zombie marks, symbols released)
    let object = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        teardown_bookkeeping(table, process_pid, code, main_cpu_ns)
    };

    // Phase 5: publish the exit. The table lock is given up, which is what the
    // wake inside it needs — and once it is published the entry is reapable,
    // so nothing may read the table for this pid after this point.
    let stats =
        final_stats(&process_data_arc, process_pid, syscall_total, syscall_total_ns, main_cpu_ns);
    object.publish_exit(crate::object::process::Exit { code, stats });
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

    let _parent_main_tid = release_thread(process_pid, tid, code);
    // Whoever joined this thread armed on it. Posted before the exit pass,
    // because after it this thread does not run again.
    if let Some(handle) = crate::sched::driver::current_handle() {
        crate::completion::post(
            crate::completion::Subject::of(handle.watch()),
            crate::completion::Outcome::Gone(crate::completion::Reason::Closed),
        );
    }
    scheduler::exit_current(code);
}

/// A child thread's own teardown, and the main thread it must wake.
///
/// Returns for [`release_process`]'s reason: the address space this clones out
/// is an `Arc`, and one left live where `exit_current` is called is one nothing
/// ever drops.
fn release_thread(process_pid: Pid, tid: Tid, code: i32) -> Tid {
    // Thread-only exit path: release this thread's mappings, zombify, wake parent.
    let addr_space = current_address_space();
    crate::mm::paging::activate_kernel();

    let tls = {
        let tdata_arc = current_data();
        let mut tdata = tdata_arc.lock();
        tdata.tls_pages.take()
    };
    let released = {
        let owner_arc = process_data();
        let mut owner_data = owner_arc.lock();
        release_thread_mappings(&mut owner_data, tls, &addr_space, tid)
    };
    // After the block: dropping these waits for every other CPU, and the
    // process-data lock they were taken under is one the page-fault handler
    // takes with `IF` clear.
    drop(released);

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let cpu_ms = table.get(process_pid).and_then(|p| p.threads.get(tid))
        .and_then(|t| t.sched())
        .map_or(0, scheduler::task_cpu_ns) / 1_000_000;
    table.get_mut(process_pid).unwrap().threads.get_mut(tid).unwrap().state = ThreadLocation::Zombie(code);
    let proc = table.get(process_pid).unwrap();
    let name = proc.name_str();
    log!("exit: {name} tid={tid} code={code} cpu={cpu_ms}ms");
    proc.main_tid
}

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

/// The physical address of a futex word, demand-paged in like any other user
/// access.
///
/// The scheduler dereferences this long after the syscall returns
/// (`scheduler::futex_wait` reads `*phys as u32` on every wake check), so the
/// alignment is not the caller's manners: a word four bytes below a 2 MiB
/// boundary would have its tail read out of the next *physical* page, which
/// belongs to somebody else.
///
/// For the same reason the answer is not a *lease*: nothing here pins the
/// frame, and a sibling's `munmap` can hand it back to the PMM while the wait
/// is still parked on it. What makes that safe is at the two ends —
/// `AddressSpace::unmap` ends the waits it orphans, and `scheduler::futex_wait`
/// re-derives this translation on every check rather than trusting it.
fn futex_word(addr: UserAddr) -> Option<crate::mm::DirectMap> {
    if !addr.raw().is_multiple_of(4) {
        return None;
    }
    crate::user_ptr::translate_user(addr)
}

/// Atomically check a user futex word and block if it matches the expected value.
/// Returns 0 if woken normally, 1 if timed out, an error if `addr` names no
/// word this process may have.
///
/// It takes a [`UserAddr`] rather than a `u64` because the bound is the whole
/// safety of the call and it used to live at the two syscall arms, spelled as
/// an expression whose value was discarded — dead-looking code protecting a
/// `pub fn` in another file, which a third caller would not have known to
/// repeat.
pub fn futex_wait(addr: UserAddr, expected: u32, timeout_ns: u64) -> u64 {
    // The ABI's relative `u64::MAX` means "no timeout" and every other value is
    // a relative span, turned absolute exactly once, here. C11 makes the ABI
    // itself absolute; until then this is where the two meanings meet, and the
    // [`Deadline`] is what stops the sentinel travelling any further in.
    let deadline = if timeout_ns != u64::MAX {
        Deadline::at(crate::clock::now() + Duration::from_nanos(timeout_ns))
    } else {
        Deadline::never()
    };

    // Physical, so a futex in shared memory works across processes.
    let Some(phys_addr) = futex_word(addr) else {
        return toyos_abi::syscall::SyscallError::BadAddress.to_u64();
    };

    // Both outcomes answer 0: a thread that blocked and was woken and one whose
    // word did not match and never blocked are the same answer to the caller,
    // which re-checks the word either way.
    scheduler::futex_wait(addr, phys_addr, expected, deadline);
    0
}

/// Wake up to `count` threads blocked on the same physical address as `addr`.
pub fn futex_wake(addr: UserAddr, count: u64) -> u64 {
    let Some(phys_addr) = futex_word(addr) else {
        return toyos_abi::syscall::SyscallError::BadAddress.to_u64();
    };
    scheduler::futex_wake(phys_addr, count as usize)
}

/// Wake processes blocked on reading from a pipe that now has data.
pub fn wake_pipe_readers(pipe_id: pipe::PipeId) {
    scheduler::wake_pipe_readers(pipe_id);
    let watchers = pipe::inbox_watchers(pipe_id);
    if !watchers.is_empty() {
        crate::inbox::complete_pending_for_event(
            &watchers,
            crate::inbox::Source::PipeReadable(pipe_id),
        );
    }
}

/// Wake processes blocked on writing to a pipe that now has space.
pub fn wake_pipe_writers(pipe_id: pipe::PipeId) {
    scheduler::wake_pipe_writers(pipe_id);
    let watchers = pipe::inbox_watchers(pipe_id);
    if !watchers.is_empty() {
        crate::inbox::complete_pending_for_event(
            &watchers,
            crate::inbox::Source::PipeWritable(pipe_id),
        );
    }
}

/// Atomically validate parent-thread relationship and collect a zombie thread.
pub fn wait_thread_zombie(tid: Tid, parent_pid: Pid) -> Result<Option<i32>, ()> {
    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    collect_thread_zombie(table, tid, parent_pid)
}

/// Handle a page fault at `fault_addr` by looking up the current process's VMAs.
/// Returns true if the fault was resolved (a page was mapped), false if fatal.
pub fn handle_page_fault(fault_addr: u64, _error_code: u64) -> bool {
    let t0 = crate::clock::nanos_since_boot();
    let tid = current_tid();
    if tid == Tid::MAX {
        return false;
    }
    // **A kernel thread's fault is fatal, said out loud rather than fallen
    // into.** It used to be answered by `current_address_space()` returning
    // `None` two lines below; since C6 a kernel thread names the kernel address
    // space, so that arm no longer catches it and the walk beneath would look
    // for a user region in a `ProcessData` that has none — reaching the same
    // `false` by accident. Nothing here can resolve a kernel fault: demand
    // paging is a user mapping's mechanism, and the kernel's direct map is
    // complete from `paging::init`.
    if crate::sched::kthread::current_is_kernel_thread() {
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

    let page_2m = PAGE_2M;
    let region_start = fault_addr & !(page_2m - 1);
    let region_end_full = region_start.saturating_add(page_2m);

    // Collect region info from the address space (lock addr_space briefly).
    // We gather everything we need so we can drop the lock before doing I/O.
    struct RegionSnap {
        start: u64,
        end: u64,
        prot: Prot,
        kind: RegionSnapKind,
    }
    enum RegionSnapKind {
        Anonymous,
        FileBacked { backing: Arc<dyn crate::file_backing::FileBacking>, file_offset: u64, file_size: u64 },
    }

    let (window_prot, regions) = {
        let as_guard = addr_space.lock();

        match as_guard.find_region(UserAddr::new(fault_addr)) {
            None => return false,
            // A `Mapped` region carries its pages from the moment it is
            // created, so a fault *inside* one is an access the mapping was
            // created to refuse — a `MmapProt::NONE` reservation, which is
            // mapped nowhere on purpose. Filling it here would hand back the
            // writable page the caller asked not to have.
            //
            // The test is on the faulting address and not on the overlap set
            // below, because a `Mapped` region that merely shares a 2 MiB
            // window with a demand-paged one still contributes zeros to it.
            Some((_, region)) if matches!(region.kind, crate::vma::RegionKind::Mapped) => {
                return false;
            }
            Some(_) => {}
        }

        // If a 2MB page is already mapped at this region (from a previous fault
        // in a different VMA that shares the same 2MB range), just return success.
        if as_guard.translate(UserAddr::new(region_start)).is_some() {
            return true;
        }

        // **The window's protection is per 4 KiB page and not one verdict for
        // all 512.** These regions came from `PT_LOAD` segments at 4 KiB
        // alignment, so the window where `.text` ends and `.data` begins holds
        // both — and OR-ing the two, which is what a single `writable` flag
        // did, is a window that is writable *and* executable. A page no region
        // covers is the padding past the image and gets the least of the
        // three: `map_window` still maps the whole frame, and nothing may
        // execute or write what nothing asked for.
        let mut window_prot = WindowProt::uniform(Prot::Read);
        let mut snaps = Vec::new();
        for (&start_addr, region) in as_guard.overlapping_regions(UserAddr::new(region_start), UserAddr::new(region_end_full)) {
            let (snap_kind, prot) = match &region.kind {
                crate::vma::RegionKind::Anonymous { prot } => (RegionSnapKind::Anonymous, *prot),
                crate::vma::RegionKind::FileBacked { backing, file_offset, file_size, prot } => (
                    RegionSnapKind::FileBacked {
                        backing: Arc::clone(backing),
                        file_offset: *file_offset,
                        file_size: *file_size,
                    },
                    *prot,
                ),
                // Already mapped eagerly, and its pages keep what was installed
                // — this fault is not about them. It contributes zeros to the
                // frame and the least protection to the pages it covers.
                crate::vma::RegionKind::Mapped => (RegionSnapKind::Anonymous, Prot::Read),
            };
            let start = start_addr.raw();
            let end = start + region.size;
            let mut page = region_start.max(start) & !0xFFF;
            while page < region_end_full.min(end) {
                window_prot.set(page - region_start, prot);
                page += 4096;
            }
            snaps.push(RegionSnap { start, end, prot, kind: snap_kind });
        }
        (window_prot, snaps)
    };

    let mut data = data_arc.lock();

    let reloc_index = data.elf.reloc_index.clone();
    let elf_base = data.elf.elf_base.raw();

    let page_alloc = match PageAlloc::new(page_2m as usize, crate::mm::pmm::Category::DemandPage) {
        Some(a) => a,
        None => return false,
    };
    // The frame as a bounds-checked window rather than a bare `*mut u8`: both
    // fills below are the kernel's own arithmetic over ELF fields, and an
    // arithmetic bound is an argument until something checks it.
    let page = page_alloc.window();

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
                        // Unhandled, not filled with zeros. The fault is on a
                        // file-backed mapping, so zeros here are instructions
                        // or constants the program never had, and the fault it
                        // takes later names an address rather than the disk.
                        // `page_alloc` is a local, so this return gives the
                        // 2 MiB page back.
                        if backing.read_page(byte_offset, &mut page_buf).is_err() {
                            log!("fault: {:#x} is backed by a file byte {byte_offset} that the \
                                 device would not read; leaving the fault unhandled",
                                fault_addr);
                            return false;
                        }
                        io_reads += 1;
                        let valid = if vma_offset + 4096 <= *file_size { 4096 } else { (*file_size - vma_offset) as usize };
                        // SAFETY: `copy_from` asserts
                        // `page_offset + valid <= page_2m` against
                        // `page_alloc`'s own size, so the write lands inside
                        // the frame — the bound that used to be derived from
                        // this loop's conditions is now checked. `page_alloc`
                        // is a local of this function that nothing else can
                        // see: the frame is not mapped into any address space
                        // until `map_window` below, so nothing is reading it
                        // and the `noalias` question the borrow would have
                        // raised does not arise. `page_buf` is a stack array
                        // this loop owns, so the ranges cannot overlap.
                        //
                        // Irreducible: the safe spelling is a `&mut [u8]`, and
                        // these pages become a user mapping four statements
                        // later — the borrow `user_ptr.rs`'s header refuses.
                        // A bounds-checked window that hands out no reference is
                        // the reduction, and `PageAlloc::window` is where it is
                        // taken.
                        unsafe { page.copy_from(page_offset, &page_buf[..valid]) };
                    }
                    vaddr += 4096;
                }
            }
        }
    }


    let mut total_relocs = 0u16;
    if let Some(ref ri) = reloc_index {
        let mut offset = 0u64;
        while offset < page_2m {
            let page_elf_offset = (region_start + offset).wrapping_sub(elf_base);
            if ri.has_relocs_in_page(page_elf_offset) {
                // `subslice` asserts `offset + 4096 <= page_2m` against the
                // frame's own size, and `apply_to_page` then bounds every write
                // against the window it was handed rather than against a 4096
                // of its own. The bound used to be `offset < page_2m` — this
                // loop's condition, an argument rather than a check, and one
                // that says nothing about the 4096 bytes past `offset`.
                total_relocs = total_relocs.saturating_add(
                    ri.apply_to_page(page_elf_offset, page.subslice(offset as usize, 4096)) as u16,
                );
            }
            offset += 4096;
        }
    }


    // No invalidation here, and none inside on the ordinary path: the fault got
    // here because the PDE was not present, and nothing is cached from one.
    // `map_window` derives that from the entry it replaced — this line used to
    // call `invlpg` a second time on the kernel's hottest paging path, for an
    // entry the first call had not needed to invalidate either.
    //
    // One 2 MiB frame either way: a window whose pages agree is one PDE, and
    // the one window per binary whose pages do not is a page table over the
    // same frame.
    addr_space.lock().map_window(UserAddr::new(region_start), page_alloc.phys(), &window_prot);

    data.demand_pages.push(page_alloc);

    data.alloc_count += 1;
    let current_mem = data.demand_pages.len() as u64 * PAGE_2M;
    if current_mem > data.peak_memory {
        data.peak_memory = current_mem;
    }

    let fault_elapsed = crate::clock::nanos_since_boot() - t0;
    data.accounting.fault_ns += fault_elapsed;
    if io_reads > 0 {
        data.accounting.fault_demand_count += 1;
        data.accounting.io_read_ops += io_reads;
        data.accounting.io_read_bytes += io_reads as u64 * 4096;
    } else {
        data.accounting.fault_zero_count += 1;
    }

    // The faulting page's own protection and not the window's: a window may
    // hold a code page and a data page at once, and the trace is about the
    // access that faulted.
    let fault_prot = regions
        .iter()
        .find(|r| fault_addr >= r.start && fault_addr < r.end)
        .map_or(Prot::Read, |r| r.prot);
    let elapsed_us = (fault_elapsed / 1000).min(u16::MAX as u64) as u16;
    data.fault_trace.record(PageFaultRecord {
        fault_addr,
        page_elf_offset: region_start.wrapping_sub(elf_base),
        block_idx: (region_start / PAGE_2M) as u32,
        reloc_count: total_relocs,
        flags: match fault_prot {
            Prot::Read => 0,
            Prot::ReadWrite => 1,
            Prot::ReadExec => 16,
        },
        duration_us: elapsed_us,
    });

    true
}

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

    let trace = &data.fault_trace;
    let count = trace.total().min(32);
    if count > 0 {
        log!("  Page fault trace ({} total, last {}):", trace.total(), count);
        for rec in trace.iter_chronological() {
            if rec.fault_addr == 0 { continue; }
            let mut flag_str = [b' '; 5];
            if rec.flags & 1 != 0 { flag_str[0] = b'W'; } // writable
            if rec.flags & 2 != 0 { flag_str[1] = b'R'; } // has_relocs
            if rec.flags & 4 != 0 { flag_str[2] = b'A'; } // anonymous
            if rec.flags & 8 != 0 { flag_str[3] = b'Z'; } // beyond extent (zero)
            // Never beside `W`: the two are the variants of one `Prot`, and a
            // trace line carrying both would be a kernel that lost W^X.
            if rec.flags & 16 != 0 { flag_str[4] = b'X'; } // executable
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
        if !virt.is_multiple_of(8) { return None; }
        let phys = addr_space.lock().translate(UserAddr::new(virt))?;
        // SAFETY: `translate` answered `Some`, so `virt` is mapped in this
        // process right now and `phys` is its direct-map address; the `virt %
        // 8` guard above makes the `u64` naturally aligned, and one aligned
        // `u64` cannot straddle the 2 MiB page the single translation answered
        // for. Reading through the direct map rather than `virt` is what keeps
        // SMAP on.
        //
        // Irreducible, and it is a *crash report* — the value is printed and
        // decides nothing, so a word another thread of the dying process
        // substitutes between two lines of the dump is the report's subject
        // rather than a hazard.
        //
        // `read_volatile` all the same, because the rule is the kernel's and
        // not this call site's: every read of user memory in this kernel is one
        // read, not one the compiler may split, fold or repeat. `dump_region`
        // is where that would show — it reads the same address twice, once to
        // decide the region is mapped and once inside the loop that prints it —
        // and a report whose two reads were folded into one would be printing a
        // value it did not fetch.
        Some(unsafe { phys.as_ptr::<u64>().read_volatile() })
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

    let fs_base_msr = crate::arch::cpu::rdfsbase();
    let fs_base = fs_base_msr;
    if fs_base != 0 {
        log!("  FS base: {:#x}", fs_base);
        if let Some(self_ptr) = read_user(fs_base) {
            log!("  fs:[0] = {:#x} (expected {:#x})", self_ptr, fs_base);
            for i in 0..8u64 {
                let addr = fs_base + i * 8;
                let Some(val) = read_user(addr) else { break };
                log!("    TP+{:#x} = {:#018x}", i * 8, val);
            }
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

/// What a crash report learned when it asked for a user address's symbol.
///
/// **Not a bool.** A bool answered "no symbol was logged" for two facts that
/// are not the same one — an address the tables genuinely do not cover, and an
/// address nothing looked up because a lock was held — and the caller printed
/// the same bare number for both. A fault report that says `0x10000004cbb`
/// where the backtrace three lines below says `fault_gate_child::main+0x136`
/// has told the reader something false about the binary
/// (`issues/panic-path/`, 2026-08-14).
#[must_use = "an address with no symbol line still has to be printed"]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolLookup {
    /// Resolved: the line naming it has already been logged.
    Named,
    /// The symbol table was read and covers no such address. A bare address is
    /// the whole truth about it.
    Unnamed,
    /// Nothing was read: a scheduler pass already held this CPU's task record
    /// when the report began, so the running task — and its symbols — could not
    /// be reached.
    ///
    /// **The one concession left, and it is not hypothetical.** A Rust `panic!`
    /// raised *inside* `driver::pass` reports from a CPU whose record is exactly
    /// that: `sched_stress`'s `invariant P` fired at
    /// `timer_handler -> driver::pass -> SchedPass::finish` on a KVM shard
    /// (`src/redlist.rs`), and a panic there with a user `syscall_rip` behind it
    /// asks this question and gets this answer.
    InPass,
    /// Nothing was read: this CPU is running no task, so there is no process
    /// whose symbols could be meant. The idle context, and boot before the first
    /// task.
    NoTask,
}

impl SymbolLookup {
    /// Log the bare address for a lookup that produced no symbol line, saying
    /// *why* there is none. [`Named`](Self::Named) logs nothing: its line is
    /// already out.
    ///
    /// Every caller goes through this rather than testing the variant, so a
    /// concession can never again be printed as a verdict.
    pub fn log_bare(self, addr: u64) {
        match self {
            Self::Named => {}
            Self::Unnamed => log!("    {:#x}", addr),
            Self::InPass => {
                log!("    {:#x}  <symbol unread: a scheduler pass held this CPU's task record>", addr)
            }
            Self::NoTask => {
                log!("    {:#x}  <symbol unread: no task is running on this CPU>", addr)
            }
        }
    }
}

/// Resolve and log a user-mode address against the running process's symbol
/// table. Safe to call from a fault or panic report; see
/// [`with_current_symbols`] for what the answer means.
pub fn resolve_user_symbol(addr: u64) -> SymbolLookup {
    with_current_symbols(|syms| crate::symbols::resolve_user(syms, addr))
}

/// [`resolve_user_symbol`] for a backtrace frame's return address — see
/// [`crate::symbols::SymbolTable::resolve_return`].
pub fn resolve_user_symbol_return(return_addr: u64) -> SymbolLookup {
    with_current_symbols(|syms| crate::symbols::resolve_user_return(syms, return_addr))
}

/// Run `f` against the symbol table of the task this CPU is running, and say
/// what happened.
///
/// **The contract, which is three-way and not two.** `f` decides between
/// [`Named`](SymbolLookup::Named) and [`Unnamed`](SymbolLookup::Unnamed); the
/// remaining answers are this function's own, and each one says that the address
/// was never looked up at all. The caller must print the address for any of them
/// — [`SymbolLookup::log_bare`] is that print — and a report that renders a
/// concession as a bare number is the defect this shape exists to make
/// unwritable.
///
/// **No lock, and that is the whole of it.** This used to take `PROCESS_TABLE`
/// with `try_lock` to find the faulting process's entry, deliberately: this runs
/// from the fault and panic reports, the faulting thread may itself hold that
/// lock — not a hypothesis, it is what `try_recover_from_panic` is written for —
/// and a wait would be a deadlock on the one path that must always produce
/// output. But a `try_lock` that may not wait is a `try_lock` that sometimes
/// loses, and what it lost was the name. Measured on the dev host under a
/// twelve-wide suite, 2026-08-22: three of twelve `fault_gates` +
/// `panic_recovery` rounds printed
/// `<symbol unread: the process table was held>` for a frame whose own backtrace
/// named it a line later. There is no lock holder to go and fix, either — the
/// takers in that window were a spawn, a demand-paged fault and an exit, which
/// is every process in the machine doing ordinary work.
///
/// So the table is not asked. A task carries its process's symbols on its own
/// record (`sched::payload::KernelPayload::symbols`), the report reads them off
/// the task it is reporting on, and the `Arc` is what keeps the bytes alive for
/// the length of the read. `sched::driver::current_symbols` is the read and
/// argues why nothing can start a pass underneath it.
///
/// **The pid is not a parameter, and that is a narrowing rather than a
/// convenience.** Every caller passed `percpu::current_pid()` — a report is
/// always about the process whose CPU is producing it — and passing it meant a
/// caller *could* ask for another process's names, which no longer resolves to
/// anything this path can reach.
fn with_current_symbols(f: impl FnOnce(&crate::symbols::SymbolTable) -> bool) -> SymbolLookup {
    let Some(syms) = crate::sched::driver::current_symbols() else {
        // Two causes, and this CPU cannot change its mind between the two reads:
        // a report is not reschedulable (`preempt::enable` declines while
        // `PerCpu::fault_state` is non-zero), so no pass can begin or end here.
        return if crate::sched::driver::in_pass() {
            SymbolLookup::InPass
        } else {
            SymbolLookup::NoTask
        };
    };
    if f(&syms) {
        SymbolLookup::Named
    } else {
        SymbolLookup::Unnamed
    }
}

/// Kill the process an object names.
///
/// **The handle is the whole authorization.** This used to check that the
/// caller was the target's parent — a relationship the kernel happened to
/// remember, which meant a parent could always kill a child and nobody else
/// ever could. A `Process` handle carrying `Rights::MANAGE` is the thing that
/// says who may, and it can be narrowed away or handed on.
///
/// Answers `Ok` for a process that is already gone: the caller asked for it to
/// be dead and it is.
pub fn kill_process(object: &crate::object::process::ProcessObject) -> u64 {
    let target_pid = object.pid();

    // Phase 1: claim teardown (brief table lock)
    let (process_data_arc, thread_data_arc, main_tid, tids) = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();

        let Some(proc) = table.get_mut(target_pid) else { return 0 };
        if !proc.claim_teardown() {
            return 0; // already dead or dying
        }
        let tids: Vec<Tid> = proc.threads.iter().map(|(t, _)| t).collect();
        let main_thread = proc.threads.get(proc.main_tid).unwrap();
        (Arc::clone(&proc.process_data), Arc::clone(&main_thread.thread_data), proc.main_tid, tids)
    };

    // Phase 2: retire every thread. A running target is forced to a scheduling
    // boundary and dropped there, so a running process is never refused.
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
    let published = {
        let mut guard = PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        teardown_bookkeeping(table, target_pid, KILLED_EXIT_CODE, main_cpu_ns)
    };

    // Phase 5: publish the exit, which is what releases every waiter.
    let stats =
        final_stats(&process_data_arc, target_pid, syscall_total, syscall_total_ns, main_cpu_ns);
    published
        .publish_exit(crate::object::process::Exit { code: KILLED_EXIT_CODE, stats });

    0
}

/// What a killed process's exit code is. The shell convention for "died on
/// SIGKILL", kept because every test that reads one already spells it.
pub const KILLED_EXIT_CODE: i32 = 137;

/// What a process that named a handle it does not hold exits with. The shell
/// convention for "died on SIGSEGV", which is the same class of mistake with a
/// pointer instead of a handle.
pub const HANDLE_FAULT_EXIT_CODE: i32 = 139;

/// End a process that named a handle it does not hold.
///
/// **The fail-fast rule, applied at the one boundary where it is userland's bug
/// rather than the kernel's.** A handle is a local name a process was given; a
/// name it was not given, one it closed, or one of the wrong type is a bug in
/// that process, and a word it can ignore lets the bug survive. It cannot be a
/// kernel panic — the rule about untrusted input holds — so the process dies
/// alone and says why.
///
/// Must be reached with nothing held: this is `exit`, and `exit` takes the
/// process's own lock, the table lock and whatever a released object reaches
/// for.
pub fn handle_fault(error: crate::object::HandleError) -> ! {
    log!(
        "handle fault: pid={} tid={} syscall={} {error}",
        current_process(),
        current_tid(),
        percpu::syscall_num(),
    );
    exit(HANDLE_FAULT_EXIT_CODE)
}

/// AP entry into the scheduler. Called from smp::ap_entry after SMP_READY.
pub fn ap_idle() -> ! {
    scheduler::enter_idle_loop();
}
