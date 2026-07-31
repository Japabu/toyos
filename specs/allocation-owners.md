# ToyOS Allocation Owners — Inventory

> Derived 2026-07-28 by five parallel per-subsystem passes plus a completeness
> critic, then spot-verified by hand. This records `bound` and `release` **as they
> are today**, not as they ought to be. `Unbounded` and `Manual` are the point of
> the document; they are not laundered anywhere.

## The numbers

| | count |
|---|---|
| Distinct allocation owners | **65** |
| `bound: Unbounded` | **25** |
| `release: Manual` | **11** |
| `release: Never` | 22 |
| Flagged (leak, unbounded, or both) | **38** |

65, not the ~42 I estimated. The gap is mostly transient allocations sized by an
unvalidated syscall argument — a category I did not think to look for, and the one
that turned out to hold the worst findings.

## The schema is missing a field

The critic's sharpest finding. `bound` and `release` are not enough, because
**`Unbounded` + userland-chosen is a categorically different thing from `Unbounded`
+ boot-constant**, and the schema cannot say which. Every owner needs a third:

```rust
size_source: Constant | Firmware | FileContent | Userland
```

Four of the six missed owners share exactly this blind spot: a `Vec::with_capacity`
or `collect()` sized by a count that crossed the trust boundary unvalidated. The
five passes reliably asked *who frees this?* and reliably never asked *who chose
this size?*

## Owner table

| Owner | Sub | Pool | Bound | Release | Risk |
|---|---|---|---|---|---|
| `DeviceClaim` | Drivers | Static | Static | Manual | **leak** |
| `GpuFramebuffer` | Drivers | Pmm | Unbounded | Drop | **unbounded** |
| `HidEventQueue` | Drivers | Heap | Unbounded | Manual | **unbounded** |
| `IoUringInstance` | Ipc | Heap | Unbounded | Manual | **leak+unbounded** |
| `IoUringWatcherList` | Ipc | Heap | Unbounded | Drop | **leak+unbounded** |
| `ListenerRegistration` | Ipc | Heap | Unbounded | Manual | **leak+unbounded** |
| `PendingConnection` | Ipc | Heap | Unbounded | Manual | **unbounded** |
| `PipeRing` | Ipc | Pmm | Unbounded | Drop | **leak+unbounded** |
| `AddressSpace` | Mm | Heap | PerProcess | Drop | **leak** |
| `DemandPage` | Mm | Pmm | Unbounded | Drop | **unbounded** |
| `EarlyBumpArena` | Mm | Static | Static | Never | **leak** |
| `KernelHeapSegment` | Mm | Pmm | Unbounded | Never | **leak+unbounded** |
| `SharedRegionMapping` | Mm | Heap | Unbounded | Manual | **leak+unbounded** |
| `SharedRegionPages` | Mm | Pmm | Unbounded | Manual | **leak+unbounded** |
| `UserPageTables` | Mm | Heap | PerProcess | OwnedBy | **leak** |
| `VmaRegionMap` | Mm | Heap | PerProcess | Drop | **leak** |
| `DynamicTlsBlock` | Proc | Pmm | Unbounded | Drop | **unbounded** |
| `ElfLoadScratch` | Proc | Heap | PerProcess | Drop | **unbounded** |
| `ExeSymtabStrtab` | Proc | Heap | Unbounded | Never | **leak+unbounded** |
| `FdTable` | Proc | Heap | Unbounded | Drop | **unbounded** |
| `LibModuleMetadata` | Proc | Heap | Unbounded | Drop | **unbounded** |
| `ProcessLoadedLib` | Proc | Pmm | Unbounded | Drop | **unbounded** |
| `ProcessMetaStrings` | Proc | Heap | PerProcess | Drop | **unbounded** |
| `ProcessTableEntry` | Proc | Heap | PerProcess | Drop | **unbounded** |
| `SharedObjectCache` | Proc | Pmm | Unbounded | Never | **leak+unbounded** |
| `StaticTlsBlock` | Proc | Pmm | PerObject | Drop | **unbounded** |
| `ThreadTableEntry` | Proc | Heap | PerObject | Drop | **unbounded** |
| `KernelStack` | Sched | Heap | PerObject | Drop | **unbounded** |
| `BcacheFsOpenFileMap` | Vfs | Heap | PerObject | Drop | **leak** |
| `BlockCachePage` | Vfs | Heap | Unbounded | Never | **unbounded** |
| `FileCacheIndex` | Vfs | Heap | PerObject | Drop | **leak** |
| `FileCachePage` | Vfs | Heap | Unbounded | Drop | **leak+unbounded** |
| `OpenFilePath` | Vfs | Heap | Unbounded | OwnedBy | **unbounded** |
| `TmpfsFileData` | Vfs | Heap | Unbounded | Manual | **leak+unbounded** |
| `TmpfsNamespace` | Vfs | Heap | Unbounded | Manual | **leak+unbounded** |
| `VfsCreatedDirs` | Vfs | Heap | Unbounded | Manual | **leak+unbounded** |
| `VfsDirListing` | Vfs | Heap | Unbounded | Drop | **unbounded** |
| `VfsPathScratch` | Vfs | Heap | Unbounded | Drop | **unbounded** |
| `AudioCompletionRecords` | Drivers | Static | Static | Never | ok |
| `AudioCompletionRing` | Drivers | Static | Static | Never | ok |
| `BoxedDriverInstance` | Drivers | Heap | Static | Never | ok |
| `DeviceSharedRegion` | Drivers | Heap | Bounded | Manual | ok |
| `DriverDmaPool` | Drivers | Pmm | Bounded | Never | ok |
| `GpuCursorPage` | Drivers | Pmm | Bounded | Never | ok |
| `KernelLogRing` | Drivers | Static | Static | Never | ok |
| `VirtqueueDescSlots` | Drivers | Heap | Bounded | Drop | ok |
| `XhciHidDeviceTable` | Drivers | Heap | Bounded | Never | ok |
| `KernelDirectMapTables` | Mm | EarlyBump | Bounded | Never | ok |
| `MmioTableGrowth` | Mm | Heap | Bounded | Never | ok |
| `Pcid` | Mm | Static | Static | Never | ok |
| `PmmPageBitmap` | Mm | Static | Static | Never | ok |
| `NicDriverBox` | Net | Heap | Static | Never | ok |
| `ChildStatsRing` | Proc | Heap | Bounded | Evictable | ok |
| `ExeRelocationIndex` | Proc | Heap | PerProcess | Drop | ok |
| `ExeTlsTemplate` | Proc | Heap | PerProcess | Drop | ok |
| `KernelSymbolTable` | Proc | Heap | Static | Never | ok |
| `UserStack` | Proc | Pmm | PerProcess | Drop | ok |
| `BlockedPool` | Sched | Heap | PerObject | Drop | ok |
| `CpuRunQueue` | Sched | Heap | PerObject | Drop | ok |
| `IrqTimestampSlots` | Sched | Static | Static | Never | ok |
| `SchedShareState` | Sched | Heap | PerProcess | Drop | ok |
| `TraceEventRings` | Sched | Static | Static | Never | ok |
| `BlockCacheIndex` | Vfs | Heap | Bounded | Never | ok — by the *cached set*. This row read `Bounded` while it meant O(device size), which is the schema gap above with a body count |
| `FileBackingExtents` | Vfs | Heap | PerObject | Drop | ok |
| `MountTable` | Vfs | Heap | Static | Never | ok |

## Missing owners the critic found

- MmapRegionPages — kernel/src/arch/syscall.rs:929 (PageAlloc::new, Category::Mmap), pushed at :948 and :963 into ProcessData.mmap_regions (struct at kernel/src/process.rs:444, field at :390). NO OWNER IN THE 65 COVERS THIS. The proc slice notes say 'NOT CLAIMED HERE' and the mm slice claims only the VMA bookkeeping (VmaRegionMap), so the actual physical pages of every mmap fell through the gap. Bound today: none — sys_mmap takes an unvalidated u64 size, align_2m's it (syscall.rs:921) and calls PageAlloc::new with no per-process quota, no region-count cap, no pressure check. Release: sys_munmap swap_remove at kernel/src/arch/syscall.rs:983, or teardown_resources at kernel/src/process.rs:823; neither runs on the panic path, so it is held to reap. This is the single largest userland-controlled PMM consumer in the kernel and it is the concrete mechanism behind CLAUDE.md's 'No physical memory fairness' entry.

- IoUringSqeBatch — kernel/src/io_uring.rs:365 `let mut sqes = Vec::with_capacity(to_process as usize)` (40 B per IoUringSqe, toyos-abi/src/io_uring.rs:16-26). No owner covers it; IoUringInstance covers the ring and pending_polls only. Bound today: NONE, and both inputs are userland-controlled. `to_process = count.min(available)` (io_uring.rs:363) where `count` is `to_submit` passed straight through from sys_io_uring_enter (kernel/src/arch/syscall.rs:1458 -> io_uring.rs:318) with zero validation, and `available = sq.tail - sq.head` (io_uring.rs:360-362) is read out of the IoUringRingHeader that lives in the 2 MiB page the process maps and writes itself — never clamped to `sq_size`. Release: dropped at end of submit_sqes. Consequence: a process writes tail=100000 into its own ring page and calls enter(to_submit=100000) -> a 4 MB single allocation -> assert at kernel/src/mm/alloc.rs:12.

- SpawnArgvTokens — kernel/src/arch/syscall.rs:742 `let args: Vec<&str> = text.split('\0').filter(...).collect()` (16 B per token), feeding kernel/src/process.rs:134 `Vec::with_capacity(args.len())`. No owner. Bound today: none — `text` comes from ctx.user_str(args.argv_ptr, args.argv_len) at kernel/src/arch/syscall.rs:216 and argv_len is an unchecked u64 field of the user-supplied SpawnArgs; user_str applies no length cap (kernel/src/user_ptr.rs:147-150). ~131,000 tokens (262 KB of "a\0" repeated) crosses 2 MiB on the Vec's next doubling. Unlike the VFS path cases, the contiguity precondition is trivially satisfiable — the 8 MiB user stack is one contiguous PageAlloc (kernel/src/loader.rs:722). Release: dropped when sys_spawn returns.

- PhysPageRunVec — kernel/src/mm/pmm.rs:283 `alloc::vec::Vec::with_capacity(count)` inside alloc_contiguous, 16 B per 2 MiB page. This heap Vec is the backbone of EVERY page-scale owner: process::PageAlloc (kernel/src/process.rs:73,79), DmaPool._pages (kernel/src/drivers/mod.rs:22), both framebuffers (kernel/src/drivers/virtio_gpu.rs:475-476), shared regions (kernel/src/shared_memory.rs:120). The mm slice explicitly punted it ('outside this slice'), the proc slice recorded only the PMM pages, so nobody owns the heap half. Bound: one entry per 2 MiB page in the run; at the PMM's 64 GiB ceiling that is 32768 x 16 = 512 KiB for one call. Release: with the owning PageAlloc/DmaPool. Worth naming because it is memory that SYS_SYSINFO's per-process figure (kernel/src/arch/syscall.rs:1072-1075, which sums PageAlloc.size()) structurally cannot see.

- SysinfoSnapshot — kernel/src/arch/syscall.rs:1055, `Vec<(Tid, &ProcessEntry, &ThreadEntry)>` collected from the whole process table and then sorted, ~24 B per live thread. No owner. Bound: one entry per thread; thread count is uncapped (spawn_thread at kernel/src/process.rs:658-670 refuses only during teardown), so ~87,000 threads makes this a >2 MiB single allocation. Any process may call SYS_SYSINFO. Release: dropped at syscall exit. Also note kernel/src/arch/syscall.rs:1041 walks the same flat_map a second time just to count.

- BlockCacheSyncScratch — `sync`'s `pending: Vec<u32>` (one dirty slot per entry), `vec![0u8; 32*4096]` (fixed 128 KiB) and `prefetch`'s `vec![0u8; run_len*4096]` (run_len capped at 32, so 128 KiB). BlockCacheIndex and BlockCachePage cover the resident cache but not these transients. `pending` is the one that matters: it is bounded by the resident slot count, which nothing evicts — so it inherits BlockCachePage's unbounded growth and crosses the 2 MiB assert at kernel/src/mm/alloc.rs:12 at ~524,000 resident blocks. Not bounded by the device: at 4 bytes per *resident* slot it takes 2 GiB of cache to get there, where the u64-per-*device*-block version sat exactly on the assert at a 1 GiB image.

- AcpiApicIdList — kernel/src/drivers/acpi.rs:246 `let mut apic_ids = Vec::new()`, pushed at :261 and :268, returned inside MadtInfo (:8-10, :278). The drivers slice flagged this as unclaimed and asked for it to be assigned; no owner took it. Bound: number of enabled CPUs in the firmware MADT — unchecked but not attacker-reachable. Release: Drop, when kernel_main's frame ends after smp::boot_aps (kernel/src/main.rs:236, :284). Trivial, but the inventory is not closed without it.

- BootInitProgramString — kernel/src/main.rs:230 `alloc::string::String::from(init_programs)` (copied off bootloader memory before mm::init reclaims it) plus the per-entry `Vec<&str>` at kernel/src/main.rs:343. No owner. Bound: whatever the bootloader passes. Release: never in practice — the String is bound for the whole body of kernel_main and re-borrowed as &str at :231. Trivial in bytes, but it is one of exactly two fully-qualified `alloc::string::` bypasses in the kernel (the other is kernel/src/arch/syscall.rs:813), which is the precise pattern the spec's §4 layer-2 CI grep exists to catch — it needs a name for that grep to be actionable.

- ExeSymMapTable — kernel/src/elf.rs:650 and kernel/src/elf.rs:707, `hashbrown::HashMap::with_capacity(sym_count)`. elf.rs:650 appears in ElfLoadScratch's site list and elf.rs:704 (the strtab leak) in ExeSymtabStrtab's, but neither owner's *bound* text covers a hashbrown table pre-sized from an untrusted count: sym_count is `symtab_data.len()/SYM_SIZE` (elf.rs:706) straight off the ELF with no validation, so one with_capacity is a single >2 MiB allocation for a large enough .symtab. Different failure mode from 'read_file_range returns a big Vec'; either give it its own owner or fix ElfLoadScratch's bound.

- VfsReaddirDedupSet — kernel/src/vfs.rs:199 and kernel/src/vfs.rs:233, `hashbrown::HashSet::new()` (`seen_dirs`), one entry per distinct subdirectory name while walking a full mount listing. Not named by VfsDirListing, which records only the `Vec<(String,u64)>` and the btree Vec<Entry>. Low risk; flagged so the count is honest.

## Granularity problems

- DUPLICATE — AudioCompletionRing and AudioCompletionRecords are the same object recorded twice. Both describe `static RECORDS` at kernel/src/audio.rs:28-48, both cite RECORD_RING_CAP=16 (audio.rs:17), both cite the const assert at audio.rs:34-37 and the overflow panic at audio.rs:55-58, both list audio.rs:42 as a site. Two slices found it independently. It is one row, and since it allocates zero bytes (.bss) it is arguably zero rows.

- FOUR ROWS FOR ZERO ALLOCATED BYTES — KernelLogRing (kernel/src/drivers/log_ring.rs:28,71), TraceEventRings (kernel/src/trace.rs:95), IrqTimestampSlots (kernel/src/irq_ring.rs:52-53) and AudioCompletion* are all fixed .bss arrays the allocator never sees. Their bounds are compile-time constants, several const-asserted, which is the correct engineering answer — but in a 40-owner budget for an *allocator* taxonomy, 6% of the rows spent on memory that is never allocated or freed is the wrong altitude. One row ('StaticBssRings: compile-time-fixed, .bss, never allocated, bounds const-asserted') carries the same information.

- ProcessTableEntry MERGES THREE LIFETIMES — it bundles ProcessEntry, Arc<Lock<ProcessData>>, Arc<Lock<SymbolTable>> and the IdMap hash node into one owner. I verified SymbolTable is genuinely zero-allocation (kernel/src/symbols.rs:9-23 is raw pointers into ELF sections plus eight u64s), so its Arc is a ~72-byte box that can never be the thing that grows, while ProcessData is the root of a dozen page-scale owners. Merging a fixed-size box with an unbounded aggregate makes the owner's `bound` field untestable — you cannot check 'unbounded' against reality when part of the row is provably 72 bytes. Split SymbolTable out or delete the separate Arc.

- DemandPage MERGES TWO RECLAIM CLASSES — ProcessData.demand_pages is one Vec, but the fault handler fills it from two region kinds: kernel/src/process.rs:1204-1230 branches on RegionKind::Anonymous vs RegionKind::FileBacked (which clones an Arc<dyn FileBacking>). File-backed pages are re-fetchable from the VFS and are legitimate eviction targets; anonymous pages are not. That is exactly the split the fs slice made between FileCachePage and TmpfsFileData, for exactly the same reason, and it should be made here too — same release path today, necessarily different bound tomorrow.

- KernelSymbolTable IS TOO SMALL TO BE AN OWNER — one Box of ~72 bytes (kernel/src/symbols.rs:197), leaked once at boot, cannot grow, cannot fail. Its rationale is good prose, but the row buys nothing an allocator taxonomy can act on. Same objection to GpuCursorPage: one 2 MiB page for the machine's lifetime, chosen at kernel/src/main.rs:320 or :325.

- FOUR TRANSIENT OWNERS WITH ONE IDENTICAL BOUND — VfsPathScratch, VfsDirListing, ElfLoadScratch and (my new) BlockCacheSyncScratch all read: short-lived, dropped at syscall exit, sized by an unvalidated user or file input, and dangerous only by crossing kernel/src/mm/alloc.rs:12. Four owners is defensible only if their bounds differ; today they do not. Either the taxonomy grows a `Transient` disposition, or these merge and the 2 MiB assert becomes one cross-cutting invariant instead of four restatements.

- EarlyBumpArena / KernelDirectMapTables — correctly two owners, but the real invariant lives BETWEEN them and no owner field can express it. The arena is 512 KiB (kernel/src/mm/alloc.rs:94); the boot page tables consume up to ~264 KiB of it at the 64 GiB the PMM bitmap permits (kernel/src/mm/paging.rs:627, :645-666 vs kernel/src/mm/pmm.rs:148, :217), and nothing asserts the two are compatible. This is evidence the owner schema needs a cross-owner constraint slot, not that these rows are wrong.

- NAMING TEST FAILURES (rationale restates the name, so either the granularity is wrong or the owner is trivial) — NicDriverBox ('a driver singleton installed once at boot with no unregister path'), BoxedDriverInstance ('one per device class, decided at boot and never replaced'), and MountTable ('boot-time singleton set') are three rows saying the same sentence about three Box<dyn Trait> in three statics. They are one owner: 'BootSingleton: Box<dyn Trait> installed once at probe into a kernel static, never replaced, never freed — freeing it would leave live hardware with no driver.' Note gop.rs:59's Box::new(GopGpu) allocates literally nothing (GopGpu is a ZST, kernel/src/drivers/gop.rs:8), which is itself a hint the row is not about allocation.

- COUNTEREXAMPLE, KEEP AS THE MODEL — FileCacheIndex / FileCachePage / TmpfsFileData is the split done right: the same allocation type (Box<[u8;4096]>, kernel/src/file_cache.rs:122,165) divided by bound and release rather than by struct, plus a separate owner for the bookkeeping that must outlive the pages. Every merge/split question above should be settled against this row, not against the type names.

## Risk ranking

### CRITICAL — `IoUringSqeBatch (MISSING — kernel/src/io_uring.rs:365)` **userland-triggerable**

One syscall, no setup, no privilege, guaranteed kernel panic. Both inputs to Vec::with_capacity are attacker-chosen: to_submit is passed unvalidated from kernel/src/arch/syscall.rs:1458 through io_uring.rs:318, and `available` (io_uring.rs:360-362) is computed from the SQ head/tail atomics that live in the 2 MiB page the process maps and writes itself — never clamped to sq_size. Write tail=100000, call enter(to_submit=100000): 100000 x 40 B = 4 MB single allocation, assert at kernel/src/mm/alloc.rs:12. Set both to u32::MAX and it is a 171 GB layout, i.e. a capacity-overflow panic in core instead. Cheapest kernel kill in the whole inventory, and no owner even names the allocation.

### CRITICAL — `MmapRegionPages (MISSING — kernel/src/arch/syscall.rs:929/948/963)` **userland-triggerable**

The largest userland-controlled physical-memory allocation in the kernel, and the one owner nobody claimed. sys_mmap takes an unvalidated u64 size, align_2m's it, and calls PageAlloc::new with no quota, no region cap, and no pressure signal. A single process can loop until the PMM is dry; every other subsystem then fails at its own allocation site with whatever failure mode it happens to have (pipe.rs:105 expect, shared_memory.rs:120 expect, virtio_gpu.rs:475 expect — all panics). Blast radius is the whole machine, not the caller. Release is Manual (sys_munmap, syscall.rs:983) or teardown_resources (process.rs:823), and neither runs on the panic path, so the pages are held to reap. This is the concrete mechanism behind CLAUDE.md's 'No physical memory fairness' item and it had no name until now.

### CRITICAL — `IoUringInstance (kernel/src/io_uring.rs:270, leaked on the panic path)` **userland-triggerable**

The only entry in the inventory whose leak causes a LATER kernel panic attributed to the wrong process. destroy() is reachable only from fd::close/close_all (fd.rs:258, :286) and close_all's only caller is teardown_resources (process.rs:820), which try_recover_from_panic (arch/idt/exceptions.rs:308) never reaches. So a panicking process leaves the ring in IO_URINGS and, critically, still reachable from every device watcher list — complete_pending_for_event keeps posting CQEs into a ring nobody will ever read until post_cqe's assert at io_uring.rs:186-189 fires. A process crashes; some unrelated interrupt later kills the kernel. Debuggability is the damage as much as the 2 MiB.

### CRITICAL — `SharedRegionPages (kernel/src/shared_memory.rs:120)` **userland-triggerable**

sys_alloc_shared (kernel/src/arch/syscall.rs:888-892) passes an unvalidated u64 straight to shared_memory::alloc, which aligns up and calls pmm::alloc_contiguous(...).expect("shared_memory: allocation failed"). One syscall with a large or merely unlucky size panics the kernel instead of returning ResourceExhausted — the fail-fast principle applied to the wrong side of the trust boundary. Compounded by release being Manual with only two callers (io_uring.rs:659 and process.rs:850), neither on the panic path.

### CRITICAL — `SpawnArgvTokens (MISSING — kernel/src/arch/syscall.rs:742)` **userland-triggerable**

One spawn syscall with ~262 KB of "a\0" repeated crosses 2 MiB of Vec<&str> and panics the kernel at mm/alloc.rs:12. argv_len is an unchecked u64 in the user-supplied SpawnArgs (syscall.rs:216) and user_str applies no cap (user_ptr.rs:147-150). Unlike the VFS path-length cases — where the fs slice honestly could not prove a >2 MiB contiguous user string is producible — here the contiguity precondition is trivially met, because the 8 MiB user stack is a single contiguous PageAlloc (loader.rs:722). Proven-reachable, not merely plausible.

### CRITICAL — `PendingConnection + PipeRing (kernel/src/listener.rs:87, kernel/src/pipe.rs:105)` **userland-triggerable**

The best amplification ratio in the inventory: sys_connect pushes into an uncapped VecDeque (listener.rs:82-89 always returns true) BEFORE allocating the client's own fd (syscall.rs:861 vs :865-868, with no rollback), so a client that connects and immediately closes its socket pins 4 MiB of PMM per iteration at zero fd cost to itself, against any server that is merely slow to accept. Terminates in a kernel panic at pipe.rs:105 expect("pipe: allocation failed"). Needs no privilege and no unusual API — just a loop against a normal daemon.

### CRITICAL — `GpuFramebuffer (kernel/src/drivers/virtio_gpu.rs:475-476)` **userland-triggerable**

SYS_GPU_SET_RESOLUTION (kernel/src/arch/syscall.rs:363-366) has NO device-claim check — any process, not just the compositor, passes two arbitrary u32s that become fb_size = width*height*4 (virtio_gpu.rs:470) and then alloc_contiguous(...).expect("framebuffer alloc failed"). Contiguous 2 MiB-granular PMM, requested twice, with the old pair still live until :561. Same missing-ownership-check class as the SYS_AUDIO_SUBMIT hole already in CLAUDE.md, but this one drives an unbounded allocation with a panicking expect, and on the panic-between-:551-and-:559 path the new pair is live and unreachable.

### CRITICAL — `FdTable (kernel/src/fd.rs:141/145, kernel/src/arch/syscall.rs:1163)` **userland-triggerable**

MAX_FDS=1024 is checked in exactly one of three insert paths (fd.rs:137-140); fd::alloc_at (fd.rs:144-146) and sys_dup2's direct insert_at (syscall.rs:1163) bypass it entirely, so the cap is advisory. `for n in 0.. { dup2(0, n) }` grows one process's table until hashbrown's next doubling exceeds 2 MiB (panic at mm/alloc.rs:12) or pipe.rs:240's expect("pipe reader overflow") fires first. It also multiplies every per-fd owner below it (OpenFilePath, PipeRing, IoUringInstance).

### HIGH — `DeviceClaim (kernel/src/device.rs:12-16)` **userland-triggerable**

Zero bytes allocated, total and permanent failure mode — the best argument in the inventory that this exercise is about ownership rather than bytes. release_descriptor is reachable only through fd::close/close_all (fd.rs:252, :280), so a compositor/netd/soundd that dies by panic leaves its slot holding a dead pid and try_claim refuses every future claim for the rest of the boot (device.rs:28-30). The display, network or audio device becomes permanently unusable and no daemon restart can recover it. Nothing scavenges owner slots against the live process table.

### HIGH — `ListenerRegistration (kernel/src/listener.rs:74/78)` **userland-triggerable**

Same shape as DeviceClaim and same two callers (fd.rs:255, :283): a process that panics while holding a listener fd leaves its name registered forever, and listener.rs:71's uniqueness check means no process can ever listen on that service name again. A crash in one daemon permanently removes a service from the namespace. Also anchors an uncapped PendingConnection queue, so the leak drags 2 MiB per stranded pipe with it.

### HIGH — `FileCachePage (kernel/src/file_cache.rs:122/165)` **userland-triggerable**

Not merely 'init is never called' — the fs slice proved the budget would not help if it were. evict_if_needed skips any file with ref_count>0 (file_cache.rs:303) while release deletes an evictable file outright the instant refcount hits 0 (file_cache.rs:69-73), so the set {evictable && ref_count==0} is unsatisfiable by construction and eviction can free nothing. max_pages is usize::MAX (file_cache.rs:32) and init has zero call sites. Any process can grow this by opening and reading files; a panic-exited process pins its share at refcount>=1 forever. A real fix has to evict clean pages from files that are still open, which the current code explicitly refuses to do.

### HIGH — `TmpfsFileData + TmpfsNamespace + VfsCreatedDirs (kernel/src/file_cache.rs:165, kernel/src/tmpfs.rs:54, kernel/src/vfs.rs:381)` **userland-triggerable**

Three unbounded userland-driven kernel-heap growers with no quota of any kind: unlimited writes to /tmp (pages are non-evictable by construction, file_cache.rs:303, and survive refcount 0, file_cache.rs:70-74), unlimited tmpfs names, unlimited mkdir paths never freed on process exit (teardown_resources at process.rs:777-830 does not touch the VFS). No MAX_PATH exists anywhere in kernel/ or toyos-abi/ and user_str applies no cap. Slower than the panic cases but unstoppable, and the terminal state is a heap OOM with no #[alloc_error_handler] — which routes into try_recover_from_panic, the path that frees nothing.

### HIGH — `SharedRegionMapping — the `allowed` Vec (kernel/src/shared_memory.rs:182)` **userland-triggerable**

sys_grant_shared (kernel/src/arch/syscall.rs:894-903) builds the target with Pid::from_raw and never checks the pid exists; grant pushes any pid not already present. A loop over distinct u32s grows the Vec past 2 MiB (~524,288 entries) and trips mm/alloc.rs:12. Ranked below the others only because it needs ~500k syscalls rather than one. The companion mapped_in field is worse in kind though not in reach: each 24-byte tuple holds a STRONG Arc clone of a whole address space (shared_memory.rs:56,67), and cleanup_process cannot rescue a dead owner's region — unmap_from(other_pid) is a no-op and allowed.retain never removes the dead owner, so the mapped_in.is_empty() test at shared_memory.rs:259 is never satisfied and the dead process's entire address space survives the boot.

### HIGH — `SharedObjectCache (kernel/src/elf.rs:134/191)` **userland-triggerable**

No cap, no eviction, no refcount, and the key is a userland-chosen path — arch/syscall.rs:1251 caches whatever a process dlopens. N distinct paths pin N full library images in physical memory for the rest of the boot, including N copies of the same file under different names. Nothing anywhere in the kernel removes an entry. The cache is load-bearing for dlopen latency, so the fix is a real eviction policy rather than deletion, which makes it more work than the rows above it.

### HIGH — `ExeSymtabStrtab (kernel/src/elf.rs:704)` **userland-triggerable**

A whole .strtab leaked via Vec::leak on every spawn of an affected binary, and loader.rs:602-617 then drops the map it built and uses it for nothing but a log line — the leak buys literally nothing. Spawn is unprivileged and repeatable, so this is unbounded growth with a trivial trigger and, uniquely in this inventory, a zero-cost fix: delete the caller.

### HIGH — `DynamicTlsBlock (kernel/src/process.rs:360, kernel/src/arch/syscall.rs:1376)` **userland-triggerable**

The one page-bearing ProcessData field teardown_resources forgot (process.rs:819-826 clears demand_pages one field away but never touches dynamic_tls_blocks), so on every process exit or kill each remaining 2 MiB block survives to reap. Grows as threads x dlopen'd TLS modules with neither factor capped and no dlclose anywhere in the kernel. Allocation failure is a kernel panic, not a graceful error: arch/syscall.rs:1376-1377 unwrap_or_else(|| panic!).

### HIGH — `KernelHeapSegment (kernel/src/mm/alloc.rs:14/16)`

The amplifier that makes every heap row above permanent. The mm slice's correction to the established survey matters here: the heap does NOT shrink. KernelPageSource::free (alloc.rs:31-35) is unreachable through four independent gates — can_release_part returns false (alloc.rs:37) blocking both dlmalloc release paths, free_part returns false (alloc.rs:27), Dlmalloc::destroy is never called, and dlmalloc 0.2.13 has no mmap-chunk path — and the first failed trim pins trim_check to usize::MAX so it never retries. So heap pages taken by a transient VFS listing or a leaked tmpfs file never return to the PMM even after the bytes are freed. Not directly triggerable; it converts every other heap spike into a permanent PMM loss.

### MEDIUM — `HidEventQueue (kernel/src/keyboard.rs:8, kernel/src/mouse.rs:8)`

Producer is an interrupt with no back-pressure and no idea whether a consumer exists — HidDevice::dispatch_report (drivers/xhci/hid.rs:31,42) calls handle_report unconditionally with no check that the device was ever claimed. Uncapped VecDeque grows until doubling exceeds 2 MiB. Ranked medium because the growth rate is a human at a keyboard, not an attacker — but it becomes unbounded-and-permanent the moment a DeviceClaim slot is stranded by a panic, which is the row four places above. Two bugs that are individually survivable and jointly a kernel panic.

### MEDIUM — `SysinfoSnapshot (MISSING — kernel/src/arch/syscall.rs:1055)` **userland-triggerable**

~24 B per live thread collected and sorted on every SYS_SYSINFO call by any process; thread count is uncapped (spawn_thread at process.rs:658-670 refuses only during teardown), so ~87,000 threads makes it a >2 MiB single allocation. Requires building the thread count first, which is itself unbounded, so it is reachable but slow — a second-order version of the FdTable bug rather than a new one.

### MEDIUM — `BlockCachePage (kernel/src/page_cache.rs:95/106/124)`

No eviction of any kind: alloc_slot runs once per block ever touched (page_cache.rs:116-133) and nothing ever resets an entry to NOT_CACHED, so the worst case is the entire 1 GiB device image resident in kernel heap. next_slot only increments, chunks only pushes, sync writes back but frees nothing. Not directly attacker-driven (file data now bypasses it via raw_block_read, page_cache.rs:56-63, so it holds only bcachefs metadata), and nothing in the kernel reports its steady-state size — PageCache has no statistics and next_slot is not exposed, so the actual footprint on a long run is unknown.

### MEDIUM — `PcidCounter (kernel/src/mm/paging.rs:196)` **userland-triggerable**

Zero bytes, but a correctness bug rather than a memory one, which is why it deserves a place in a ranking that claims to measure damage. NEXT_PCID advances once per spawn and is never recycled — no free list, no Drop on AddressSpace. On the 4096th spawn alloc_pcid flushes and restarts at 1 (paging.rs:206-211); if any address space from the previous lap is still live, two address spaces share a PCID and therefore share TLB entries, which is silent memory corruption, not a leak. Inert on QEMU TCG (no PCID support), so it has never executed in CI — the most dangerous combination of 'reachable on real hardware' and 'never tested'.

## Critic's verdict

Good enough to build the taxonomy from, but not to freeze — the inventory is strong on lifetime analysis and systematically weak on unvalidated-size-at-the-syscall-boundary, which is where the worst findings turned out to be.

WHAT IT GETS RIGHT. The five slices did the hard part honestly. Unbounded and Manual are not laundered anywhere I checked; the release paths are traced to their actual single callers rather than to the function that looks like it should free (fd::close_all having exactly one caller at process.rs:820 is the load-bearing fact in half the rows, and every slice found it independently). Three of the mm slice's corrections to the established survey are real and I verified them: the heap does not shrink, AddressSpace.pages is always empty because its only insert is in dead code, and pmm::Category::PageTable is consequently never used at any site. The fs slice's proof that a file-cache budget would not help — {evictable && ref_count==0} is unsatisfiable by construction — is the single best piece of analysis in the document, because it kills a fix that would otherwise have looked obviously correct.

WHAT I WOULD FIX FIRST, IN ORDER.

1. Add the six missing owners, MmapRegionPages and IoUringSqeBatch first. MmapRegionPages is the more embarrassing gap — it is the largest userland-controlled PMM consumer in the kernel and BOTH the proc and mm slices explicitly wrote 'not claimed here' about it, each assuming the other had it. That is a process failure, not an analysis failure, and it is the thing to fix about how the next inventory is run: every 'not claimed here, belongs to X' note needs an actual handshake with X.

2. Fix the systematic blind spot the missing owners share. Four of my six gaps (IoUringSqeBatch, SpawnArgvTokens, SysinfoSnapshot, ExeSymMapTable) are Vec::with_capacity or collect() sized by an unvalidated count that crosses the trust boundary. The slices reliably asked 'who frees this?' and reliably did not ask 'who chose this size?'. The owner schema should therefore carry a third field alongside bound and release — something like `size_source: Constant | Firmware | FileContent | Userland` — because 'Unbounded' plus 'Userland' is a different and much worse thing than 'Unbounded' plus 'Constant', and today the schema cannot say so.

3. Merge the duplicates and the sub-owners before anyone builds from this. AudioCompletionRing and AudioCompletionRecords are one object recorded twice. NicDriverBox, BoxedDriverInstance and MountTable are one BootSingleton. The four .bss rings are one row. That is roughly seven rows recovered from 65, which pays for the six additions and lands near the 40 the brief asked for.

4. Split ProcessTableEntry (fixed-size SymbolTable box merged with the unbounded ProcessData root) and DemandPage (anonymous vs file-backed have the same release today and necessarily different bounds tomorrow).

ONE STRUCTURAL OBSERVATION FOR THE SPEC ITSELF. The spec's §3.3 reap-time assert — charged==0 or panic — is the right mechanism, and this inventory says it will fire immediately and constantly, which §8 stage 5 already predicts. But it cannot fire at all for the four owners whose damage is zero bytes: DeviceClaim, ListenerRegistration's name, PcidCounter, and the io_uring watcher-list entries. Those are the rows where a crash costs a permanently unusable device, a permanently burned service name, or silent TLB aliasing — and a byte-counting assert is blind to every one of them. That is the strongest argument in this inventory for the position §6 already takes: memory-ownership and capability-handles must land together, because roughly a third of the worst findings here are not about memory at all.

Finally, PipeRing deserves the last word. It is the one owner in the kernel whose release is destructor-driven end to end, and it survives the panic path for free as a result. The fix this whole exercise is pointing at is not novel — it already exists, in one file, and works. It is just unapplied everywhere else.