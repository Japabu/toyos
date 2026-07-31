# ToyOS Capability Handles — Technical Specification

## 1. Goals

- **Compile-time kill of the free-while-referenced class** (the crash.md UAF: an
  `AddressSpace` freed/gutted while a queued `TaskCtx` held an `Arc` to it). Memory
  lifetime = `Arc` lifetime, `Drop` is the only destructor, destructive operations are
  unreachable through shared references.
- **Kill userland id-guessing.** No global integer grants access: `PipeId`,
  `SharedToken`, `signal_pipe_id`, pid-addressed `kill`/`waitpid` all die. Authority is
  a handle in your own table, installed only by the kernel via creation or explicit
  transfer.
- **Refs ≠ handles.** `Arc` strong count governs *memory*; a separate `handle_count`
  governs *userland visibility* (pipe EOF/BrokenPipe, device reclaim, process-dead
  observation). Peer-closed semantics must fire correctly even when a thread is killed
  while blocked and its kernel stack is discarded without running destructors.
- **Fail fast at runtime where compile time can't reach:** generation-tagged handles,
  use-after-close kills the offending process (end state), per-variant live-object
  census with test-asserted baselines.
- **One mechanism** unifies fds, pipes, shm, devices, io_uring, IPC connections,
  processes, and threads. `Fd` → `Handle` everywhere (closes the CLAUDE.md known
  issue). Zircon is the reference design, adopted selectively; seL4-style derivation
  trees are rejected.
- Scales to 128+ cores: the handle table is a per-process leaf lock; no global object
  registries on hot paths.

**The strongest argument for this spec is a live data leak, not ergonomics.** A
`FileBacking` outlives deletion of the file it reads: `NvmeBacking` holds extents captured
at open and reads them by absolute block number with no re-validation, so after an unlink
frees those blocks to bcachefs's allocator and another file takes them, a process
demand-paging the unlinked file reads **another process's file contents**. Ordinary
filesystem operations, no crafting (`specs/known-issues.md` §1).

That is this spec's refcount, missing: the backing must keep the file's blocks alive for
as long as it can read them. It is deliberately left unfixed pending this work, because a
local patch — re-validating extents per read, or invalidating backings on unlink —
reimplements refcounting badly at one call site while every other cached reference keeps
the same shape.

`known-issues.md` §1 names the pair this closes: **an id or a name treated as a
capability** (guessing a designation) and **a reference that outlives the object it
names** (outliving one). Handles make the first unrepresentable by carrying rights and the
second by carrying a refcount.

## 2. Bug classes and their disposition

| # | Bug class | Today | After |
|---|-----------|-------|-------|
| 1 | Object guts freed in place while shared (crash.md hypothesis 2: `AddressSpace.root`) | Possible; `teardown_scheduling` takes `_addr_space` unused, nobody can say what frees the root | **Compile-time impossible.** `AsInner.root` is non-`Option`, non-replaceable, private; freed exactly once in `Drop`. No `free()`/`destroy()`/`take()` exists on any object (§6.4). |
| 2 | Arc refcount underflow via bitwise `TaskCtx` duplication (crash.md hypothesis 1) | Unpoliced unsafe moves + lock-leak across `context_switch` | **Lint wall + Phase-1 contract.** `clippy.toml` bans `Arc::into_raw/from_raw/increment_strong_count/decrement_strong_count` and `mem::forget` kernel-wide (§12.2); the single `#[allow]` island is the scheduler switch path until the Phase-1 rewrite removes it. `TaskCtx` stays `!Clone`; the Phase-1 `Task` is move-only with proof-of-absence retirement (§9.4). |
| 3 | Userland id-guessing (`signal_pipe_id`, `SharedToken`, `SYS_PIPE_OPEN` with zero access check) | `MSG_STREAM_OPENED` ships global integers | **Unrepresentable.** The namespaces are deleted; there is nothing to guess (§8). |
| 4 | Lost peer-closed on kill-while-blocked (the cpal client's steady state IS blocked-on-signal-pipe) | n/a (no peer-closed semantics at all) | **Structural.** `handle_count` is decremented by handle-table drain at exit, independent of Arc clones stranded on raw-freed kernel stacks. EOF/BrokenPipe/device-reclaim fire from `on_zero_handles`, never from `Drop` (§5, §7). |
| 5 | Use-after-close / double-close of descriptors | `u32` fd reuse silently aliases | **Runtime fail-fast.** Generation-tagged handles; stale handle → error during migration → kill-process at end state (§4.5). |
| 6 | Kernel freeing shm/DMA while mapped elsewhere | `shared_memory::unregister/destroy` unmap behind your back | **Compile-time.** Mappings hold `Arc<SharedMemObject>`; pages die with the last Arc. No revocation of memory exists (§6.2, §14). |
| 7 | Handle leak / double-close in userland | raw `Fd(i32)` | **Compile-time.** SDK `OwnedHandle` is `!Copy` with `Drop`; `into_raw` is the single greppable escape hatch (§10). |

Criteria order honored throughout: (1) compile-time impossibility, (2) runtime
fail-fast, (3) tests.

## 3. Design overview

- Every kernel object is a plain `Arc<T>`. No custom refcounting, no `dyn` dispatcher
  hierarchy, **no `Weak` anywhere in the object graph** (see §14 for the one deleted
  candidate). `Arc` already proves drop-exactly-once-after-last-ref; custom counting
  would reintroduce the manual-decrement bug class this design exists to kill.
- `KObjectRef` is a **closed enum**, one variant per object type. Exhaustive matches
  make adding an object type a compile error at every dispatch site.
- Every object embeds an `ObjectCore { koid, handle_count }`. `Koid` is a
  `NonZeroU64` from a global monotonic counter — an *identity* for diagnostics and
  kernel-internal keys, never an *authority*; no syscall turns a koid into access.
- Per-process `HandleTable`: `RawHandle` (u32 = generation | slot) →
  `HandleEntry { object: KObjectRef, rights: Rights }`. The table is a leaf lock whose
  API only hands out **owned Arcs** — no borrow into the table ever escapes the guard.
- **Rights only shrink.** `dup`/`transfer` subset rights, never add.
- When `handle_count` reaches 0, `on_zero_handles` fires **exactly once**, via a
  deferred per-CPU queue drained outside all locks (§5.2) — running subsystem hooks
  under a lock is structurally impossible, not a discipline rule.
- Handle transfer is kernel-mediated over IPC connections, batched, all-or-nothing.
- Teardown = reference drain, never revocation. The single arbitration exception
  (device claims) is a tombstone state flip, never a free (§6.5, §9).
- Migration is rename-first (§15): the fd table becomes the handle table *before*
  transfer/shm objects land, so no dual-table window ever exists and no syscall ever
  has to guess whether an integer is an fd or a handle.

Ownership DAG (all edges `Arc`, acyclic by construction; the one accepted residual
cycle is documented in §8.4):

```
PROCESS_REGISTRY: Pid → Arc<ProcessObject>        (strong; entry removed at exit)
ProcessObject ──► Arc<Lock<ProcessData>>, Arc<AddressSpaceObject>
ProcessData.handles: HandleTable
   ├─► SharedMemObject ──► [PhysPage]             (pages die with last Arc)
   ├─► PipeReadEnd ─┐
   ├─► PipeWriteEnd ┴─► PipeShared ──► PhysPage   (ring page dies with last end)
   ├─► ConnectionEnd ──► ConnectionShared { 2×PipeShared, handle queues }
   ├─► IoUringObject ──► PageAlloc                (owns its ring pages directly)
   ├─► DeviceClaim                                (per-claim object, §6.5)
   ├─► FileObject / ListenerObject / SysCap
   ├─► ProcessObject                              (a handle to a child/peer)
   └─► ThreadObject
scheduler Task/TaskCtx ──► Arc<ProcessObject>, Arc<ThreadObject>,
                           Arc<AddressSpaceObject> (non-Option)   (§9.4)
AddressSpaceObject.shm_mappings ──► Arc<SharedMemObject>          (mapping pins pages)
```

`ProcessObject` never holds Tasks (scheduler exclusively owns them);
`ThreadObject` never holds `ProcessObject` (a `TaskId` suffices) — no proc↔thread
cycles. The `Task → Arc<ProcessObject>` edge guarantees a *running* process is never
freed by its last handle dropping (a hole in an earlier draft of this design).

## 4. Handle table

### 4.1 RawHandle encoding — `toyos-abi/src/handle.rs`

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct RawHandle(pub u32);
// bits  0..12  slot index   (4096 handles/process; replaces MAX_FDS = 1024)
// bits 12..32  generation   (20 bits; slot permanently retired on wrap —
//                            leak one slot, never alias)
pub const HANDLE_INVALID: RawHandle = RawHandle(u32::MAX);
```

**A slot at generation 0 encodes as the bare index.** Stdio handles are therefore
literally `0`, `1`, `2` and the std fork's stdio plumbing is untouched by the whole
migration. A bit-31 tag scheme was rejected: its only benefit (catching stale call
sites passing fds where handles are expected) is already delivered by the
kill-on-bad-handle policy, and it would force std pal churn for stdio. The
fd-vs-handle ambiguity it guards against never arises because migration is
rename-first (§15) — there is no window in which both tables front I/O syscalls.

### 4.2 Rights — hand-rolled, no bitflags dependency

```rust
// toyos-abi/src/handle.rs (~40 lines, const fns: union, contains, subset_of)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rights(u32);
impl Rights {
    pub const DUP: Rights      = Rights(1 << 0);
    pub const TRANSFER: Rights = Rights(1 << 1);
    pub const READ: Rights     = Rights(1 << 2);
    pub const WRITE: Rights    = Rights(1 << 3);
    pub const MAP: Rights      = Rights(1 << 4);  // shm / DMA / ring mapping
    pub const WAIT: Rights     = Rights(1 << 5);  // block on / io_uring POLL_ADD (Phase-2 seam)
    pub const MANAGE: Rights   = Rights(1 << 6);  // kill process, join thread, revoke
    pub const RT: Rights       = Rights(1 << 7);  // on SysCap: enter the RT band
    pub const fn subset_of(self, of: Rights) -> bool { self.0 & !of.0 == 0 }
}
```

Eight rights, not Zircon's ~20: every bit has a current caller. Add a right when a
caller exists, not before.

### 4.3 Table API — `kernel/src/object/handle.rs`

```rust
pub struct HandleEntry { object: KObjectRef, rights: Rights }   // moves by value only
struct Slot { gen: u32, entry: Option<HandleEntry> }
pub struct HandleTable { slots: Vec<Slot>, free: Vec<u16> }

#[derive(Clone, Copy, Debug)]
pub enum HandleError { BadHandle, Stale, WrongType, Rights, TableFull, NotTransferable }

impl HandleTable {
    pub fn install(&mut self, e: HandleEntry) -> Result<RawHandle, HandleError>;

    /// THE typed accessor. Returns an OWNED Arc — never a borrow into the table.
    /// `let pipe = table.get::<PipeWriteEnd>(h, Rights::WRITE)?;`
    pub fn get<T: KObjectVariant>(&self, h: RawHandle, need: Rights)
        -> Result<Arc<T>, HandleError>;

    /// Untyped variant for close/dup/transfer/stat. Clones the entry OUT —
    /// no borrow into the table escapes here either.
    pub fn get_any(&self, h: RawHandle, need: Rights)
        -> Result<(KObjectRef, Rights), HandleError>;

    /// Rights may only shrink; `new ⊄ old` → Rights error. Requires DUP.
    pub fn dup(&mut self, h: RawHandle, new_rights: Rights)
        -> Result<RawHandle, HandleError>;

    /// Close. Bumps the slot generation. The returned entry MUST be dropped by
    /// the caller after releasing the ProcessData lock (its drop only decrements
    /// handle_count and possibly enqueues a deferred hook — §5.2 — so even a
    /// drop-under-lock cannot run subsystem code, but returning it keeps the
    /// count decrement out of the guard's temporary-lifetime trap).
    #[must_use]
    pub fn remove(&mut self, h: RawHandle) -> Result<HandleEntry, HandleError>;

    /// Process exit: empty the table. Dropped by the caller outside all locks.
    #[must_use]
    pub fn drain(&mut self) -> Vec<HandleEntry>;
}
```

`HandleTable` lives inside `ProcessData` behind the existing lock — no new lock, no
new ordering edges. `get` is lock → index → clone Arc → unlock: the same cost shape as
today's fd path. At 128 cores the table only contends among one process's own threads;
it is leaf-shaped and shardable later if measurement demands (>2x rule — not built now).

### 4.4 KObjectRef and the variant trait — `kernel/src/object/mod.rs`

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Koid(core::num::NonZeroU64);           // monotonic, never reused

pub struct ObjectCore {
    koid: Koid,
    /// Userland-visibility count: table slots + in-flight transfers + spawn grants.
    /// NOT the Arc strong count. Reaching 0 fires on_zero_handles exactly once (§5).
    handle_count: AtomicU32,
    retired: AtomicBool,                          // set once when count first hits 0
}

#[derive(Clone)]
pub enum KObjectRef {
    SharedMem(Arc<SharedMemObject>),
    PipeRead(Arc<PipeReadEnd>),
    PipeWrite(Arc<PipeWriteEnd>),
    File(Arc<FileObject>),
    Connection(Arc<ConnectionEnd>),
    Listener(Arc<ListenerObject>),
    IoUring(Arc<IoUringObject>),
    Device(Arc<DeviceClaim>),
    Process(Arc<ProcessObject>),
    Thread(Arc<ThreadObject>),
    SysCap(Arc<SysCap>),
}

pub trait KObjectVariant: Send + Sync + Sized + 'static {
    const NAME: &'static str;
    fn from_ref(r: &KObjectRef) -> Option<&Arc<Self>>;
    fn into_ref(this: Arc<Self>) -> KObjectRef;
    fn core(&self) -> &ObjectCore;
    fn on_zero_handles(&self) {}
}
// kobject!(SharedMem, PipeRead, ...) implements KObjectVariant per variant and a
// per-variant `static LIVE_<T>: AtomicU64` census (inc in ctor, dec in Drop),
// dumped by log_health() and the panic path. The census is the leak detector for
// the one accepted leak class (§7.3) and the residual connection cycle (§8.4).
```

Object-layer dispatch code must use exhaustive matches — no `_` arms. Enforced by
review; the `kobject!` macro generates all per-variant plumbing so hand-written
dispatch sites are few.

### 4.5 Bad-handle policy — staged, fixed end state

Kernel code never holds `RawHandle`s (it holds Arcs), so these are pure
userland-bug detectors:

| Failure | Migration stages A–D | End state (stage E+) | Rationale |
|---|---|---|---|
| `BadHandle` / `Stale` (garbage, empty slot, old generation) | log + error return | **kill the calling process** (log raw value, syscall nr, pid) | Use-after-close/double-close is a userland bug; masking it violates fail-fast. Staged so the big mechanical rename can land green while call sites are audited. |
| `WrongType` | log + error return | **kill the calling process** | Static caller bug; unmaskable. |
| `Rights` | error (`PermissionDenied`) | error (`PermissionDenied`) | Legitimately dynamic: you can hold an attenuated handle. Probing rights is not a bug. |
| `TableFull` | error (`ResourceExhausted`) | error | Resource limit, not a bug. |

The kernel never panics on any of these (kernel must never crash from userland).

## 5. handle_count, on_zero_handles, and the deferred-close queue

### 5.1 Why refs ≠ handles is load-bearing

Today a thread killed while blocked has its `TaskCtx` (and kernel stack) freed
**without unwinding** — destructors in stack frames never run. Any `Arc<PipeReadEnd>`
a syscall cloned out of the table before blocking is stranded: its strong count never
drops. If EOF/BrokenPipe/device-reclaim were driven by `Arc`-count-reaching-zero
(Drop-based), killing a client blocked in its signal-pipe read — the cpal client's
*steady state* — would leak the read end and soundd would never see BrokenPipe. The
flagship crash-detection fix would fail in its flagship scenario.

Therefore: **userland-visible lifecycle events ride `handle_count`, never Arc counts.**
Process exit drains the handle table (§9.1) and drops the drained entries; each drop
decrements `handle_count` regardless of Arcs stranded on dead stacks. The stranded Arc
leaks only *memory* (bounded, census-visible, reclaimed when Phase 2's try-once
syscalls remove Arc-across-block entirely — §13); it can never delay or lose a
semantic event.

### 5.2 Exactly-once, never-under-a-lock — structurally

```rust
impl Drop for HandleEntry {
    fn drop(&mut self) {
        let core = self.object.core();
        if core.handle_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            let first = !core.retired.swap(true, Ordering::AcqRel);
            assert!(first, "handle_count resurrected after zero (koid={:?})", core.koid);
            zero_queue::push(self.object.clone());   // per-CPU queue, wait-free push
        }
    }
}
```

`on_zero_handles` **never runs inline at drop time**. The zero queue is drained by
`object::drain_zero_handles()` at syscall exit, at `do_schedule` entry, and in the
idle loop (the same shape and sites as `drain_events`). Consequences, all structural:

- A hook can never run under the handle-table lock, the ProcessData lock, or any
  subsystem lock — no matter how carelessly a call site drops an entry. The
  `drop(pd.lock().handles.remove(h)?)` temporary-lifetime trap is defused by
  construction, not by discipline.
- Cascades (dropping a connection drops queued entries drops a device claim…) become
  queue iterations, not recursion — no stack-depth concern.
- Hooks observe a stable world: wakes fired from a hook go through the normal
  scheduler entry points.
- Latency is bounded by the next syscall exit / schedule on that CPU (µs-scale; same
  promptness class as event delivery today).

Resurrection (installing a new handle to an object whose count already hit 0) is a
kernel bug: `OwnedHandle`-creation paths assert `!retired`.

### 5.3 What each hook does

| Object | `on_zero_handles` |
|---|---|
| `PipeWriteEnd` | set `RING_WRITER_CLOSED`, wake readers (EOF) |
| `PipeReadEnd` | set `RING_READER_CLOSED`, wake writers (BrokenPipe) |
| `ConnectionEnd` | close both directions; drop queued in-flight `HandleEntry`s (each may cascade); wake peer |
| `DeviceClaim` | release the device class in the registry; stop the stream (§6.5) |
| `ListenerObject` | unregister the service name; refuse queued connects |
| `IoUringObject` | cancel pending polls; ring pages die with the Arc |
| `SharedMem`, `File`, `Process`, `Thread`, `SysCap` | nothing (memory drains by refcount; process death is driven by exit, not by handle count) |

Manual pipe `readers`/`writers` counters and `close_read`/`close_write` bookkeeping
are deleted; the counts *are* the two ends' `handle_count`s, maintained by RAII —
miscounting becomes unrepresentable.

## 6. Objects

### 6.1 Pipes — `kernel/src/object/pipe.rs` (replaces `pipe.rs` internals)

```rust
pub struct PipeShared {
    page: PhysPage,                 // 2MB ring, RingHeader at offset 0 (layout unchanged)
    watchers: Lock<Vec<Koid>>,      // io_uring watcher rings (moves off the global PIPES map)
    rt_boost_pending: AtomicBool,
}
pub struct PipeReadEnd  { core: ObjectCore, shared: Arc<PipeShared> }
pub struct PipeWriteEnd { core: ObjectCore, shared: Arc<PipeShared> }
```

Two distinct end types, not one object + direction rights: "write to a read end" moves
from a runtime `PermissionDenied` to a **compile-time impossibility** (`PipeReadEnd`
has no write method). Neither end holds the peer — peer state travels through
`PipeShared`, so no Arc cycles. Dup of a read handle shares the same `Arc<PipeReadEnd>`
(one more table slot, +1 handle_count): EOF fires when the *last* read handle anywhere
disappears, exactly the right semantic with zero bookkeeping.

### 6.2 Shared memory — `kernel/src/object/shm.rs` (replaces `shared_memory.rs`)

```rust
pub struct SharedMemObject {
    core: ObjectCore,
    pages: SharedBacking,   // enum { Owned(Vec<PhysPage>), KernelDma { phys: DirectMap, len: u64 } }
    size: u64,
}
```

Mapping: the mapping process's `AddressSpaceObject.shm_mappings` stores
`ShmMapping { object: Arc<SharedMemObject>, vaddr: UserAddr }`. Unmap = drop the
mapping entry (frees the VA range in that address space only). Pages die when the last
handle *and* the last mapping Arc are gone. There is no `destroy()`, no `unregister()`,
no unmap-others, no `allowed: Vec<Pid>` ACL. The "freed while another process has it
mapped" state cannot be expressed. `KernelDma` backing is never freed by handle
machinery (device DMA pages follow today's `Ownership::Kernel` rule).

### 6.3 Connections and listeners — `kernel/src/object/service.rs`

```rust
pub struct ConnectionShared {
    pipes: [Arc<PipeShared>; 2],                       // rx/tx byte streams, as today
    inflight: [Lock<VecDeque<TransferBatch>>; 2],      // handle side-channel per direction
}
pub struct ConnectionEnd { core: ObjectCore, shared: Arc<ConnectionShared>, side: u8 }
pub struct ListenerObject { core: ObjectCore, name: String, /* pending queue */ }
```

Service *names* stay strings in `listen()`/`connect()` — names are rendezvous; the
returned handle is the authority. Zircon-style datagram channels are rejected: ToyOS
zero-copy shared-ring IPC is a feature; handles ride a per-connection kernel
side-queue (§8), ordered by the existing framed protocol.

### 6.4 Address space — `kernel/src/object/addr_space.rs` (the crash.md fix)

```rust
pub struct AddressSpaceObject { inner: Lock<AsInner> }

struct AsInner {
    /// NOT Option. NOT replaceable. No method moves, takes, zeroes, or reassigns
    /// this field — freed exactly once, in AsInner::drop. If you can call cr3(),
    /// the root page tables are alive: proven by the compiler.
    root: PageTableRoot,
    regions: BTreeMap<UserAddr, vma::Region>,
    shm_mappings: Vec<ShmMapping>,
}
pub type PageTables = Arc<AddressSpaceObject>;   // replaces Arc<Lock<AddressSpace>>
```

- Not handle-visible (no `ObjectCore`): userland has no legitimate use for another
  process's address space; exposing it would re-open the exact object the crash
  involved. Revisit only if a debugger needs it.
- `TaskCtx.address_space: Option<PageTables>` → **non-Option `Arc<AddressSpaceObject>`**.
  `cr3()`'s `unwrap()` disappears; a schedulable task without an address space is
  unrepresentable. This retype is the pinned contract with the Phase-1 scheduler (§9.4).
- Defense in depth against *unsafe-code* bugs (the bitwise-move class):
  `AsInner::drop` poisons the root's first qword with a sentinel; `cr3()` asserts
  non-poisoned — a resurrected stale ref panics with a named invariant instead of a
  wild deref inside #GP recursion (the crash.md evidence-destroyer).
- `teardown_scheduling`'s unused `_addr_space: PageTables` parameter is deleted; exit
  holds no address-space privileges at all (§9.1).
- User-page release at exit is gated by the `AllThreadsRetired` proof token (§9.2):
  `detach_user(&self, _proof: &AllThreadsRetired)` drops `regions` and `shm_mappings`
  eagerly (nothing can reference them — a compile-time property, not a scan). The root
  stays alive until the last Arc drops; a stale queued `TaskCtx` calling `cr3()` reads
  live root tables no matter how far exit has progressed. Worst case is a wasted
  context switch, never a UAF.

### 6.5 Device claims — `kernel/src/object/device.rs` (replaces owner statics)

```rust
pub struct DeviceClaim {
    core: ObjectCore,
    class: DeviceClass,        // Keyboard, Mouse, Framebuffer, Nic, Audio
    revoked: AtomicBool,       // tombstone: ops fail, memory drains by refcount
}
```

- **Per-claim objects, created at claim time** (`SYS_OPEN_DEVICE(class)` returns a
  fresh `DeviceClaim` handle), not one boot-time `DeviceObject` per device. A
  boot-time singleton whose master handle init retains would make
  `handle_count == 0` unreachable and device reclaim would never fire — the per-claim
  shape avoids that contradiction outright.
- Exclusivity is structural: the claim handle is created with
  `TRANSFER|READ|WRITE|MAP|WAIT` but **not DUP** — at most one handle to a claim can
  ever exist; TRANSFER moves it whole. The registry rejects a second claim while a
  live claim object exists (`handle_count > 0`).
- Daemon crash ⇒ exit drains its table ⇒ claim `handle_count` → 0 ⇒
  `on_zero_handles` releases the class and stops the stream ⇒ a respawned daemon
  claims cleanly. This works for kill-while-blocked by §5.1.
- Arbitration remains first-come-first-served as today; spawn-time device grants via a
  handle vector are scoped out for v1 (§14).
- Forced reclaim of a **wedged-alive** daemon = kill it (exit teardown unmaps its DMA
  window). There is no unmap-others revocation machinery (§14); the tombstone only
  makes a claim refuse service. Documented failure mode (§13).
- `SYS_AUDIO_SUBMIT` gains a claim-handle argument (WRITE right) — closes the
  "any process can submit / auto-start the stream" hole.
- `FramebufferInfo.token` / NIC DMA tokens are replaced by `SharedMem` handles
  installed during `SYS_OPEN_DEVICE`.

### 6.6 Process and Thread — `kernel/src/object/process.rs`

```rust
pub enum ProcState { Alive, Dead { code: i32 } }    // exit code latched at death

pub struct ProcessObject {
    core: ObjectCore,
    pid: Pid,                          // name for logs/stats; grants nothing
    state: Lock<ProcState>,
    data: Arc<Lock<ProcessData>>,      // handle table lives inside
    addr_space: Arc<AddressSpaceObject>,
}
pub struct ThreadObject {
    core: ObjectCore,
    id: TaskId,                        // (Pid, Tid) — scheduler-internal identity
    state: Lock<ThreadState>,          // Alive / Dead { code }
}
```

**Zombies and orphans die as concepts.** `SYS_PROCESS_WAIT(h)` blocks until the
handle's state is `Dead`, then reads the latched code — the object *is* the zombie
record, freed by refcount like everything else. `zombify`, `OrphanCleanup`,
`handle_orphans`, `collect_orphan_zombies` (`process.rs`) and the idle-loop reaping
pass are deleted. A parent that never waits leaks nothing; an orphan leaks nothing.
The `proc.parent == Some(caller)` authority check is deleted: possession of a MANAGE
process handle *is* the permission. "Dead" is a state, not a third lifetime — no
`Option::take` of process guts exists.

`ThreadObject` does **not** own the kernel stack (a joiner's handle must not pin
128 KB per dead thread); the stack stays in `TaskCtx`, freed by the Phase-1 drop
protocol.

### 6.7 SysCap — `kernel/src/object/syscap.rs`

One tiny object closes the two remaining ambient-authority holes with pure rights
attenuation, no policy framework:

```rust
pub struct SysCap { core: ObjectCore }   // authority is entirely in the handle's Rights
```

- Created once at boot; init receives the full-rights handle via its spawn vector.
- `Rights::RT` on a SysCap handle gates `SYS_RT_ENTER(h)` (replaces the unprivileged
  `SYS_SET_RT_PRIORITY`). init grants an RT-only dup to soundd and the compositor per
  `system.toml` (`caps = ["rt"]`).
- `Rights::MANAGE` on a SysCap handle gates `SYS_PROCESS_OPEN(pid) → Process handle` —
  the **single, documented name→authority bridge**, needed by the shell's `kill`/`ps`.
  The gate is a right on a handle the caller must present, not a caller-identity
  check. Flagged for removal when a supervisor process owns all process handles.

### 6.8 Files, io_uring

`FileObject { core, state: Lock<OpenFileState> }` absorbs `OpenFile`.
**Semantic change, explicit:** `SYS_HANDLE_DUP` shares the object — two handles to one
`FileObject` share the cursor (capability semantics), unlike today's `SYS_DUP`
descriptor clone. Call sites relying on independent cursors get an explicit re-open;
the migration stage includes a call-site audit. `dup2`'s slot pinning survives only as
stdio pre-seeding at spawn.

`IoUringObject` owns its ring `PageAlloc` directly — deletes the last
`shared_memory::destroy()` caller and closes the "io_uring abuses shared_memory"
known issue. `PendingPoll` keys move from fd numbers to `(Koid, RawHandle)`;
close-cancels-poll is preserved via the koid.

## 7. Kernel-internal integers: what stays, what dies

| Today | Becomes | Why safe |
|---|---|---|
| `Fd(u32)` / `FdTable` / `Descriptor` | `RawHandle` / `HandleTable` / `KObjectRef` | generation bits + kill-on-stale |
| `PipeId` + global `PIPES` map + `SYS_PIPE_OPEN/PIPE_ID` | deleted — Arc ends + transfer | no global namespace to guess |
| `SharedToken` (`Copy` u32, no RAII — known issue) | deleted — `Arc<SharedMemObject>` | RAII by construction |
| `RingId` (userland-visible) | `IoUring` handle; koid internally | ring owns its pages |
| `ListenerId` | `Listener` handle; names stay rendezvous strings | |
| `Pid`/`Tid` in `waitpid`/`kill`/`thread_join` | `Process`/`Thread` handles | authority requires capability |
| `Pid`/`Tid` in logs, stats, scheduler keys | **stay plain integers** | pure names; every authority path goes through a handle |
| `io_uring::Source::{PipeReadable, PipeWritable, Listener}` keys | keyed by `Koid` | kernel-internal wait-list keys, never accepted from userland |
| `DEVICE_*_OWNER` statics | deleted — claim-object existence is the claim | crash auto-release via §5.3 |

## 8. Handle transfer over IPC

### 8.1 Protocol

A handle entry is always owned by exactly one container:
`Installed(table)` → `InFlight(connection queue)` → `Installed(other table)` |
`Dropped(connection died)`. Enforced by Rust move semantics — `HandleEntry` is
`!Clone` and literally moves between owners; "same handle live in two places" (the
`SharedToken`/TaskCtx-duplication disease) is unrepresentable.

```rust
pub struct TransferBatch(Vec<HandleEntry>);   // #[must_use]; Drop releases entries
                                              // (correct for connection teardown)
pub const MAX_TRANSFER_HANDLES: usize = 8;    // per batch
pub const MAX_QUEUED_BATCHES: usize = 16;     // per direction
```

**SEND** — `SYS_HANDLE_SEND(conn_h, handles_ptr, n)`, all-or-nothing, atomic:

1. `conn = table.get::<ConnectionEnd>(conn_h, Rights::TRANSFER)?`.
2. Lock order: ProcessData(table) → connection inflight queue (both leaf locks, no
   blocking work inside). Under **both** locks: verify queue room; for each handle
   verify it exists, has `TRANSFER`, and is not this very connection
   (`koid == conn.koid` → error — the self-transfer cycle Zircon also forbids);
   then remove all and push one `TransferBatch`. Any failure → nothing was removed —
   atomicity by lock scope, no rollback protocol needed.
3. After unlock: raise the connection's readiness (wakes io_uring waiters).

**RECV** — `SYS_HANDLE_RECV(conn_h, out_ptr, cap) → n`:

1. Peek front batch; `cap < len` or receiver table lacks room → error, batch stays
   queued (retryable).
2. Pop, install all (rights travel unchanged), write `RawHandle`s to `out`.

### 8.2 Readiness

The connection's poll readiness (io_uring `POLL_ADD`, `WAIT` right) is
`data_available || inflight_nonempty` — kernel-level, not protocol discipline. The SDK
convention additionally frames "N handles follow" in the data stream so receivers know
how many to expect; the kernel readiness bit makes the wakeup correct even if a sender
violates the convention.

### 8.3 The audio flow, fixed end-to-end (kills reads_full §6.1/§7.1)

```
soundd open_stream:
    shm   = SYS_SHM_CREATE(size)                        // full rights
    (r,w) = SYS_PIPE                                    // two end handles
    client_shm = SYS_HANDLE_DUP(shm, MAP)               // attenuated: map-only
    reply MSG_STREAM_OPENED { period, rates, .. }       // plain data, no ids
    SYS_HANDLE_SEND(conn, [client_shm, r])              // read end MOVES out
    // soundd keeps w only. Client death — even killed while blocked in the
    // pipe read — drains its table, the last PipeReadEnd handle disappears,
    // on_zero_handles fires, soundd's next write sees BrokenPipe. Crash
    // detection (audio spec §5.7) works by construction.

client AudioStream::open:
    recv MSG_STREAM_OPENED
    [shm_h, pipe_h] = SYS_HANDLE_RECV(conn)
    vaddr = SYS_SHM_MAP(shm_h)                          // MAP right checked
```

`SYS_PIPE_OPEN`, `SYS_PIPE_ID`, `SYS_GRANT_SHARED`, and the shm pid-ACL are deleted in
the same stage. There is no id to guess because there is no id in the protocol.

### 8.4 Accepted residual: the cross-pair cycle

A's connection handle queued in B's connection while B's is queued in A's leaks both
pairs until reboot (Zircon accepts the same). Not worth a cycle collector: the
per-variant census makes it visible, and no ToyOS protocol transfers connection
handles today. Documented, census-monitored.

## 9. Teardown

### 9.1 Process exit (self or kill) — replaces `process::exit` phases

```
1. state → Dead { code }              new syscalls presenting handles to this
                                      process's objects observe Dead → BadState error
2. retire threads                     Phase-1 retire_task(id) per sibling (all threads
                                      incl. main for remote kill; killer never among
                                      them — retire_task asserts). Each recovered
                                      Task/TaskCtx is dropped → releases its
                                      Arc<ThreadObject>/Arc<AddressSpaceObject>/
                                      Arc<ProcessObject>. Yields AllThreadsRetired.
3. drained = pd.lock(){ handles.drain() + take(demand_pages, mmap, elf, tls) }
                                      lock released before step 4
4. drop(drained)                      OUTSIDE all locks. Every HandleEntry drop
                                      decrements handle_count; zero-hooks are queued
                                      (§5.2) and drained at the next schedule point:
                                      pipes EOF/BrokenPipe, device claims released,
                                      io_uring polls cancelled. Private pages freed
                                      eagerly — nothing can reference them
                                      (compile-time property via AllThreadsRetired).
5. addr_space.detach_user(&proof)     drop regions + shm mappings; root untouched
6. PROCESS_REGISTRY.remove(pid)       registry's strong Arc dropped
7. wake io_uring::Source::Terminated(koid)  parents in SYS_PROCESS_WAIT + io_uring pollers
8. scheduler exit_current             (self-exit only) last Task consumed; its Arc
                                      drops are the final scheduler-side refs
```

No step frees anything another ref might see. The `AddressSpaceObject` dies at
whichever comes last: step 5/8's drops, the last stolen/queued `TaskCtx` drop, or a
parent's handle drop. **Order cannot matter — that is the whole point.** The crash.md
scenario (idle CPU steals the exiting thread, calls `cr3()`) reads live root tables
regardless of exit progress; the stolen task dies at the next kill-mark check.

### 9.2 AllThreadsRetired proof token

```rust
/// Only constructible by the retire loop after every sibling returned
/// owned-Task-or-proof-of-absence. Extends the existing IdleProof idiom.
pub struct AllThreadsRetired { pid: Pid, _priv: () }
```

`detach_user`, demand-page release, and TLS/ELF teardown take `&AllThreadsRetired` —
"freed pages a sibling still runs on" is a **compile-time** error, not an ordering
comment.

### 9.3 Revocation vs refcount drain — the policy line

| Mechanism | Used for | Never used for |
|---|---|---|
| Refcount drain (default) | shm, pipes, files, connections, io_uring, process/thread shells | — |
| State flip observed by remaining refs | process `Dead`, thread `Dead`, `DeviceClaim.revoked` tombstone | freeing memory. Revoke never frees; freed-while-referenced stays unrepresentable even under revocation. |

### 9.4 Contract with the Phase-1 scheduler (pinned now, whichever lands second adopts)

```rust
pub struct TaskCtx {   // Phase-1 `Task`
    pub thread: Arc<ThreadObject>,
    pub process: Arc<ProcessObject>,          // strong edge: a RUNNING process can
                                              // never be freed by handle drops
    pub aspace: Arc<AddressSpaceObject>,      // non-Option
    pub kernel_stack: OwnedAlloc,             // owned here, NOT by ThreadObject
    pub kernel_rsp: u64, pub fs_base: u64,
    /* timing/sched fields unchanged */
}
impl TaskCtx { pub fn cr3(&self) -> Cr3 { self.aspace.cr3() } }   // no unwrap
```

- Phase 1 guarantees: `Task` is move-only, exclusively owned by one container;
  `retire_task(id)` returns owned-Task-or-proof-of-absence.
- This layer guarantees Phase 1: the three Arcs' `Drop`s are safe from any context a
  Task is dropped in — they take no scheduler locks and run no zero-handle hooks
  inline (Tasks hold refs, not handles; hooks only ever run from the deferred queue).
- Runtime assert inherited from Phase 1: a `TaskCtx` is never dropped on its own
  kernel stack (`handle_outgoing` protocol).
- **Interim rule until Phase 2** (stated, census-enforced): syscall code must not hold
  object Arcs across `scheduler::block` on a retirable stack — re-look-up after wake.
  Violations don't break semantics (§5.1) but leak until reboot; the census baseline
  assertions in the QEMU churn tests catch them. Phase 2's try-once syscalls make the
  rule structural and moot.

## 10. Userland SDK — `toyos/src/handle.rs`

```rust
pub struct OwnedHandle(RawHandle);          // !Copy, !Clone
impl Drop for OwnedHandle { fn drop(&mut self) { let _ = sys_handle_close(self.0); } }
impl OwnedHandle {
    pub fn raw(&self) -> RawHandle;
    pub fn into_raw(self) -> RawHandle;      // transfer consumed it — the ONLY leak door
    pub unsafe fn from_raw(h: RawHandle) -> Self;   // mirrors std OwnedFd
}
pub struct SharedMem(OwnedHandle);  pub struct PipeRx(OwnedHandle);
pub struct PipeTx(OwnedHandle);     pub struct ProcessHandle(OwnedHandle);
// ... typed wrappers mirror kernel variants
```

Double-close and handle leaks are compile-time impossible in userland. Spawn's handle
vector is parsed once by `toyos::system::init_handles()`; std's `sys/pal/toyos` keeps
reading stdio from slots 0/1/2 unchanged (gen-0 encoding, §4.1).

## 11. Syscall ABI delta

**New:**

```
SYS_HANDLE_CLOSE(h)                         → ()
SYS_HANDLE_DUP(h, rights)                   → RawHandle    // DUP; rights subset only
SYS_HANDLE_SEND(conn_h, handles_ptr, n)     → ()           // n ≤ 8, atomic batch
SYS_HANDLE_RECV(conn_h, out_ptr, cap)       → n            // FIFO batch, all-or-error
SYS_SHM_CREATE(size)                        → RawHandle    // rights: DUP|TRANSFER|MAP
SYS_SHM_MAP(h)                              → vaddr        // MAP; mapping pins Arc
SYS_SHM_UNMAP(h)                            → ()           // own mapping only
SYS_PROCESS_WAIT(h)                         → exit_code    // WAIT; replaces SYS_WAITPID
SYS_PROCESS_KILL(h)                         → ()           // MANAGE; replaces SYS_KILL
SYS_PROCESS_OPEN(pid, syscap_h)             → RawHandle    // MANAGE on SysCap (§6.7)
SYS_THREAD_JOIN_H(h)                        → exit_code    // WAIT
SYS_RT_ENTER(syscap_h)                      → ()           // RT right; replaces SYS_SET_RT_PRIORITY
```

**Renamed in place** (same numbers; the fd argument is reinterpreted as `RawHandle` —
transparent for stdio by §4.1): `SYS_READ/WRITE/CLOSE/SEEK/FSTAT/READ_NONBLOCK/`
`WRITE_NONBLOCK/IO_URING_*`, `SYS_ACCEPT/CONNECT` (return Connection handles),
`SYS_OPEN_DEVICE` (returns a `DeviceClaim` handle + shm handles for DMA windows),
`SYS_SPAWN` (returns a Process handle; gains a `(tag, RawHandle)` grant vector —
`TAG_SYSCAP`, `TAG_CUSTOM`; pid still returned inside stats for display).
`SYS_AUDIO_SUBMIT` gains the claim-handle argument.
`SYS_DUP/DUP2` are subsumed by `SYS_HANDLE_DUP` (cursor-sharing semantics, §6.8).

**Deleted:** `SYS_PIPE_OPEN`, `SYS_PIPE_ID`, `SYS_PIPE_MAP`, `SYS_ALLOC_SHARED`,
`SYS_GRANT_SHARED`, `SYS_MAP_SHARED`, `SYS_RELEASE_SHARED`, `SYS_WAITPID`, `SYS_KILL`,
`SYS_THREAD_JOIN(tid)`, `SYS_SET_RT_PRIORITY`, `SYS_AUDIO_POLL` (dead ABI).

## 12. Invariants

### 12.1 Compile-time (bug class unrepresentable)

1. Object memory freed while referenced — Arc-only lifetime; backing resources are
   private fields freed only in `Drop`; no take/gut/replace methods exist (§6.4).
2. Same handle live in two places — `HandleEntry` is `!Clone`, moves by value through
   tables and queues (§8.1).
3. Handle-count drift — mutated only in `HandleEntry` construction/`Drop` (§5.2);
   pipe end-count bookkeeping has no functions to forget or double-call.
4. Write to a read end — distinct `PipeReadEnd`/`PipeWriteEnd` types (§6.1).
5. Borrow into the handle table outliving the lock — `get`/`get_any` return owned
   values only (§4.3).
6. Type confusion — `get::<T>` returns `Arc<T>`; adding a `KObjectRef` variant breaks
   every dispatch site (closed enum, no `_` arms in object-layer code).
7. Freeing shm while mapped — mappings hold `Arc<SharedMemObject>` (§6.2).
8. Resource detach before thread retirement — `AllThreadsRetired` proof token (§9.2).
9. Rights escalation — no API constructs an entry with rights ⊄ source outside object
   constructors.
10. Zero-handle hook under a lock — hooks run only from the deferred queue (§5.2).
11. Schedulable task without an address space — non-Option `TaskCtx.aspace` (§9.4).
12. Handle leak/double-close in userland — `!Copy` `OwnedHandle` (§10).
13. Device claim duplication — claims are created without DUP (§6.5).

### 12.2 Mechanical enforcement

`kernel/clippy.toml` `disallowed-methods`: `Arc::into_raw`, `Arc::from_raw`,
`Arc::increment_strong_count`, `Arc::decrement_strong_count`, `core::mem::forget`.
Single documented `#[allow]` island: the scheduler lock-across-`context_switch` path,
removed by Phase 1. CI grep-gates (stage F): no `SharedToken`, no `pipe_open`, no
pid-authority call sites.

### 12.3 Runtime fail-fast

1. Bad/stale/wrong-type handle from userland → §4.5 policy (kill at end state).
2. `handle_count` resurrection after zero → panic (kernel bug) (§5.2).
3. Poisoned address-space root observed by `cr3()` → panic with named invariant (§6.4).
4. Generation wrap → slot permanently retired; whole-table retirement → panic.
5. Per-variant `LIVE_*` census dumped in `log_health()` and the panic path; the QEMU
   process-churn tests assert census returns to baseline (the detector for §5.1 stack
   leaks, §8.4 cycles, and §9.4 interim-rule violations).
6. Transfer of a connection into itself → error (cycle guard).
7. Double state→Dead / double retire → panic (replaces today's `zombify` assert).

### 12.4 Tests (QEMU harness; every migration stage also gates on the audio glitch test)

- `handle_basic` — create/dup/close/stale-detect/rights-attenuation.
- `handle_transfer` — shm+pipe over IPC; batch atomicity; queue-full backpressure.
- `handle_kill_policy` — use-after-close kills the offender, kernel survives.
- `kill_while_blocked` — client killed while blocked in signal-pipe read ⇒ soundd
  observes BrokenPipe ⇒ ramp-out; census returns to baseline.
- `device_claim_crash_release` — kill soundd ⇒ audio class released ⇒ respawned
  soundd claims and audio recovers.
- `process_lifecycle` — spawn/wait/kill trees; wait-after-death returns latched code;
  no zombie accumulation (census).
- Census baseline assertion after every churn test.

## 13. Failure modes

| Failure | Behavior | Recovery |
|---|---|---|
| Userland presents stale/garbage/wrong-type handle | Process killed (end state; §4.5), kernel logs and survives | Respawn by init if a daemon |
| Client killed while blocked on signal pipe | Table drain → read-end handle_count→0 → soundd sees BrokenPipe next write | Gain ramp-out; other clients unaffected |
| Daemon crash while holding a device claim | Claim released via on_zero_handles; stream stopped | init respawns; fresh claim succeeds |
| Daemon wedged alive holding a device claim | Claim not auto-released (no unmap-others revoke) | Supervisor/init kills it via MANAGE handle — the only forced-reclaim path (§6.5) |
| Receiver dies with handles in flight | Connection teardown drops queues; each entry's zero-hook fires | Nothing leaks; census flat |
| Transfer while sender table refills / queue full | Atomic under both locks: error, nothing moved | Caller retries |
| Cross-pair connection cycle (A's handle queued in B, B's in A) | Both pairs leak until reboot | Accepted (Zircon parity); census-visible (§8.4) |
| Syscall held an object Arc across block, thread killed | Semantic events still fire (§5.1); the Arc's memory leaks | Census flags it; structural fix = Phase 2 try-once syscalls |
| Exiting task stolen by an idle CPU mid-teardown (crash.md) | `cr3()` reads live root tables; task dies at next kill-mark check | One wasted context switch, never a UAF |
| Generation wraps on a hot slot | Slot permanently retired (never re-aliased) | 4095 slots remain; panic only if all retired |
| Handle table full | `ResourceExhausted` | Process closes handles or is buggy (fail fast) |
| Parent never waits on child | Dead ProcessObject shell (~100 bytes + root tables) pinned by the handle | Freed when parent's handle drops; nothing scans |

No failure mode requires a kernel-side scan, a timeout, or trusting userland. Kernel
panics happen only for kernel bugs (fail fast); userland can never induce one.

## 14. Explicitly rejected / scoped out

1. **`Arc<dyn KObject>` + downcast (Zircon dispatcher hierarchy).** Runtime dispatch,
   `_ => unreachable!()` arms, extensibility ToyOS doesn't want. The object set is
   closed and small; a closed enum makes additions compiler-guided.
2. **seL4 CNodes / derivation trees / badges / untyped retype.** Formal-revocation
   ceremony for ~11 object types and drain-not-revoke teardown. Arc + a flat table
   covers it.
3. **Custom intrusive refcounting** (`fbl::RefPtr`, adoption). Reintroduces the manual
   count discipline this design eliminates (`handle_count` is the one counted value,
   and it is mutated in exactly two places, both RAII).
4. **`Weak` anywhere.** Reintroduces "is it alive?" as a runtime question. The process
   registry holds strong Arcs removed explicitly at exit; the earlier Weak-registry
   draft had an orphan-running-process UAF — the `Task → Arc<ProcessObject>` edge
   replaces it.
5. **Shm revoke / unmap-others / TLB-shootdown machinery.** The heaviest subsystem of
   the rejected drafts, duplicating exit teardown, with Weak-upgrade races. v1 rule:
   forced reclaim = kill the holder; exit already unmaps. Revisit only if a real
   arbitration case appears that killing cannot serve.
6. **Zircon channels (kernel-copied datagrams).** ToyOS zero-copy ring IPC is a
   feature; handles ride a side-queue (§8).
7. **Jobs, job policies, configurable bad-handle policy.** One machine, one trust
   domain. The bad-handle policy is a compile-time constant (staged only during
   migration). Resource limits arrive with the separate memory-fairness work.
8. **Per-object signal/waitset machinery** (`zx_object_wait_*`). Readiness belongs to
   io_uring (Phase 2). Objects expose readiness predicates + watcher lists; one new
   event: `io_uring::Source::Terminated(Koid)`.
9. **Handle-value randomization.** Handles index the caller's own table; nothing to
   forge.
10. **Rights as const generics / typestate.** Rights are runtime data (dup subsets,
    manifests); typestate would force a dynamic fallback everywhere and double the API.
11. **Spawn-time device-grant vector (`devices = [...]`) for v1.** Per-claim
    `SYS_OPEN_DEVICE` + FCFS matches today's arbitration with strictly better crash
    semantics; the spawn vector exists (SysCap) and can carry device grants later
    without ABI change. Scoped out to keep stage E small.
12. **VMARs / user-visible address spaces; timer/profile/clock objects.** No use case;
    timers are io_uring deadlines.

## 15. Staged migration plan

Lockstep world rebuilds are the superpower: kernel, `toyos-abi`, `toyos`, std fork,
and all daemons live in one tree with an unstable ABI — each stage is one coordinated
commit series, no compat shims left behind. **Every stage: builds, boots
compositor/netd/soundd/doom, `cargo test` green, audio glitch test green.**
Ordering principle: rename first, so no dual-table window ever exists and no syscall
routes between fd-space and handle-space.

**Stage A — object infrastructure (zero users).**
`kernel/src/object/{mod,handle,rights}.rs`, `toyos-abi/src/handle.rs`,
`kernel/clippy.toml`, census counters, deferred zero-queue + drain hooks at the three
sites. `HandleTable` added inside `ProcessData` alongside `FdTable`, empty.
`SYS_HANDLE_CLOSE/DUP` wired to the new table only.
*Gate:* boots; clippy wall enforced tree-wide.

**Stage B — the Fd → Handle rename (internally staged, one green commit each).**
- B1: `pipe.rs` → `object/pipe.rs` (PipeShared + two end types with `ObjectCore`);
  `Descriptor::PipeRead/PipeWrite/Tty*/Socket` hold the new end Arcs; manual
  reader/writer counters deleted — handle_count semantics live from here, so
  kill-while-blocked EOF/BrokenPipe already works.
- B2: remaining `Descriptor` kinds become objects (`FileObject`, `ConnectionEnd`,
  `ListenerObject`, `IoUringObject`, `DeviceClaim` per class incl. Framebuffer/Nic;
  owner statics deleted; `SYS_AUDIO_SUBMIT` gains the claim handle).
- B3: table swap — `FdTable` deleted; `HandleTable` is the only table, stdio
  pre-seeded at slots 0/1/2 gen 0 (std fork untouched); rights populated
  (pre-existing descriptors get their natural rights); `fd.rs` dispatch becomes
  exhaustive matches on `KObjectRef` and shrinks to `handle_io.rs`; `io_uring::Source`
  keys become Koids; dup→`SYS_HANDLE_DUP` with the cursor-sharing audit.
  Bad handles: log + error (§4.5).
*Gate:* full boot, full suite, audio glitch test, `handle_basic`,
`kill_while_blocked` (EOF half).

**Stage C — shm objects + transfer + the audio hole fix.**
`object/shm.rs`, `SYS_SHM_CREATE/MAP/UNMAP`; connection inflight queues +
`SYS_HANDLE_SEND/RECV` + readiness; migrate soundd + `toyos/src/audio.rs` + cpal to
the §8.3 flow; soundd drops its retained read end; delete `SYS_PIPE_OPEN`,
`SYS_PIPE_ID`, `SYS_GRANT_SHARED`, shm pid-ACL. gpu/net keep legacy shm tokens until D.
*Gate:* audio glitch test (tone + doom), `handle_transfer`, `kill_while_blocked`
(BrokenPipe half), soundd-reconnect test. **The id-guessing hole is dead here.**

**Stage D — legacy shm namespace deleted.**
gpu/net DMA windows become `SharedMem` handles installed at `SYS_OPEN_DEVICE`;
`SYS_ALLOC/MAP/RELEASE_SHARED` deleted; `shared_memory.rs` + `SharedToken` deleted;
`IoUringObject` owns its `PageAlloc` (last `shared_memory::destroy()` caller gone).
*Gate:* full suite + census-baseline assertions live from here.

**Stage E — Process/Thread objects + SysCap + fail-fast flip.**
`ProcessObject`/`ThreadObject`; spawn returns a Process handle + grant vector
(`TAG_SYSCAP`); `SYS_PROCESS_WAIT/KILL/OPEN`, `SYS_THREAD_JOIN_H`,
`io_uring::Source::Terminated(Koid)`; delete `SYS_WAITPID`, `SYS_KILL`, zombie/orphan
machinery (`zombify`, `OrphanCleanup`, `handle_orphans`, `collect_orphan_zombies`);
std `process` pal updated; `SYS_RT_ENTER` gated on SysCap RT (system.toml `caps`),
`SYS_SET_RT_PRIORITY` deleted. **TaskCtx retype (§9.4) lands here** — the single
Phase-1 coordination point; against the current scheduler it is a mechanical
`Option`-removal + AsInner hardening, independently valuable. Bad-handle policy flips
to kill-process.
*Gate:* `process_lifecycle`, `handle_kill_policy`, `device_claim_crash_release`,
kill-soundd-respawn-audio-recovers.

**Stage F — audit & shrink.**
Dead constants and `_ =>` arms deleted; grep-gates added to CI (§12.2); CLAUDE.md
architecture + known-issues updated (closes: `SharedToken` RAII, io_uring shm abuse,
`Fd` rename, unprivileged RT); census check in `log_health` behind a boot flag.

Dependency notes: Stages A–D are independent of Phase 1 (scheduler) and Phase 2
(io_uring-only blocking). Stage E's TaskCtx retype is the only Phase-1 touchpoint —
the §9.4 contract is agreed now so whichever lands second adopts the other's types.
Phase 2 plugs in at exactly two seams: the `WAIT` right for `POLL_ADD`, and connection
readiness for `SYS_HANDLE_RECV`; it also retires the §9.4 interim no-Arc-across-block
rule structurally.
