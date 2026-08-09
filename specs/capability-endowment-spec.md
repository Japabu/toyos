# Capability endowment — technical specification

The owner ruled on 2026-08-09 that ToyOS adopts the capability endowment
architecture, as **one deliverable**: `system.toml` becomes a real manifest,
spawn hands capabilities, channels queue, global connect-by-name is retired,
`TOYOS_SURFACE`'s whole class goes, POSIX fakes ambient authority inside the
compat layer, and least authority becomes something a process can be asked to
demonstrate. This is that design.

`specs/capability-handles-spec.md` is not a parallel track. The ruling's fifth
point is that this work *realizes* it: a namespace whose entries are names is
the defect being deleted, so its entries must be refcounted kernel objects
behind typed handles, which is that spec. Every place this deviates from it is
listed in §11 with the reason, and this file is the one to read where the two
disagree.

Numbers in this document come from commands run against this worktree
(`wt/toyos-endow`, at `19c761e`) on 2026-08-09. Where a figure is an estimate it
says so.

---

## 0. The shape, in one page

Today a process finds a service by presenting a **string** to a **flat global
registry** with **no access control**: `services::listen(name)` takes any name
first-come, `services::connect(name)` succeeds for any name anyone has taken.
Five production names exist — `compositor`, `soundd`, `netd`, `filepicker`,
`surface.<pid>` — and eight test-only names share the same namespace with them.
Every process can reach all thirteen, and a process that starts before its
server has registered is told `NotFound`, which it cannot distinguish from
"there is no desktop".

After this work:

- The kernel has **no service registry at all**. There is no name→process map,
  and no syscall accepts a service name from a process that was not given the
  namespace the name is in.
- A **port** is a pair of objects: an `Acceptor` (the server accepts from it)
  and a `Connector` (a client connects through it). Two types, so a client
  cannot accept its own service's connections — a compile-time property, not a
  check.
- A **namespace** is a kernel object holding `name → Arc<Connector>`, immutable
  once built. A process opens a connection by asking *a namespace handle it
  holds*. A name it was not given resolves to nothing, and there is no other
  place to ask.
- `/bin/init` is userland. The kernel spawns exactly one program. init reads
  `/etc/system.manifest` — generated from `system.toml` — creates one port per
  declared service **before any server runs**, builds each program's namespace
  out of connectors, and spawns it holding them.
- A connection therefore works from the client's first instruction, whether or
  not the server has reached `accept` or has even been spawned yet. **There is
  no instant at which a name is not bound yet**, so there is nothing to retry
  and no timeout anywhere.
- If a server exits without serving, its `Acceptor`'s last handle goes, the
  queued connections' pipe ends drop, and the client's next write is
  `BrokenPipe`. The bound on failure is a process lifetime and nothing else.

The two retry loops with identical magic constants
(`NetdConn::connect_blocking` and `AudioStream::connect_soundd`, both
`BOOT_RETRIES = 100`, `BOOT_RETRY_INTERVAL_NS = 10_000_000`) are deleted rather
than joined by a third.

**One design constraint governs the whole SDK surface.** The ecosystem forks
call `toyos::audio::AudioStream::open`, `toyos_window::Window::create_with_title`
and `toyos::net::tcp_*` by their current signatures, and a fork edit is a
separate repository and a separate landing. So **those signatures do not
change**: each resolves its service through the calling process's own namespace
internally. The fork estate's exposure is reduced to the two crates that name
`toyos_abi::Fd` and one that names `syscall::map_shared` (§6.6).

---

## 1. The object model

### 1.1 What is adopted from `capability-handles-spec.md`

`§3`–`§5` of that spec are adopted essentially verbatim and are not restated
here. In summary, and binding:

- Every kernel object is a plain `Arc<T>`. No `Weak` in the object graph, no
  custom refcounting, no `dyn` dispatcher hierarchy.
- `KObjectRef` is a closed enum, one variant per object type; object-layer
  dispatch is exhaustive with no `_` arms.
- Every object embeds `ObjectCore { koid, handle_count, retired }`. `Koid` is a
  `NonZeroU64` identity for diagnostics and kernel-internal keys, never an
  authority.
- Per-process `HandleTable` inside `ProcessData`, behind the existing lock.
  `RawHandle(u32)` = 12 bits slot, 20 bits generation; a slot at generation 0
  encodes as the bare index, so stdio is literally `0`, `1`, `2` and the std
  fork's stdio plumbing is untouched.
- `HandleTable::get::<T>(h, need_rights) -> Result<Arc<T>, HandleError>` returns
  an **owned** Arc; no borrow into the table escapes the guard.
- Rights only shrink. `HandleEntry` is `!Clone` and moves by value between
  containers.
- `handle_count` is **not** the Arc count. Userland-visible lifecycle events
  ride it, so a thread killed while blocked — whose kernel stack is freed
  without unwinding — cannot strand an EOF.
- `on_zero_handles` runs exactly once, from a deferred per-CPU queue drained at
  syscall exit, `do_schedule` entry and the idle loop. A hook can never run
  under any lock.

**Which paths each type binds.** The kernel does not unwind, so a `Drop` is a
guarantee only on paths where the value is actually dropped. Two facts make the
handle table's drops binding on the kill path as well as on exit, and both were
checked in this tree rather than assumed:

- `kill_process` and `exit` share `teardown_resources`, which drains the
  descriptor table on the *killer's* CPU (`kernel/src/fd.rs:367`'s `close_all`
  doc records this). The handle table replaces that table in the same position,
  so the same drain runs.
- An `Arc<T>` a syscall cloned out of the table before blocking is stranded on a
  freed kernel stack. That leaks memory, bounded and census-visible, and cannot
  delay a semantic event, because every semantic event is driven by
  `handle_count` which the drain decrements.

The failing shape to check any new type against is `toyos-sched`'s
`Registration`: a guard that lives on the victim's own stack and is therefore
never dropped when another CPU kills it. **No object introduced below places a
release obligation on a blocked thread's stack.** `Acceptor`, `Connector`,
`Namespace` and `ConnectionEnd` all release through the table drain.

### 1.2 New object types

```rust
// kernel/src/object/port.rs

/// Everything both ends of a port share. Neither end holds the other, so no
/// Arc cycle exists.
struct PortShared {
    queue: Lock<VecDeque<PendingConnection>>,
    acceptors: Arc<KWaitQueue>,          // threads blocked in accept
    io_uring_watchers: Lock<Vec<RingId>>,
    /// Set by Acceptor::on_zero_handles. A connector whose acceptor is gone
    /// refuses; it does not queue for a server that will never read.
    closed: AtomicBool,
}

pub struct Acceptor  { core: ObjectCore, shared: Arc<PortShared> }
pub struct Connector { core: ObjectCore, shared: Arc<PortShared> }
```

`Acceptor::on_zero_handles` sets `closed`, takes the queue out and drops it. Each
`PendingConnection`'s `PipeReader`/`PipeWriter` drop, which is what makes every
queued client observe `BrokenPipe`/EOF. `Connector::on_zero_handles` does
nothing: a service with no clients right now is not a service that has stopped.

Two types rather than one object with direction rights, for the same reason
`PipeReadEnd`/`PipeWriteEnd` are two types: "accept from a service you were only
given access to" moves from a runtime `PermissionDenied` to a state that cannot
be written.

```rust
// kernel/src/object/namespace.rs

pub struct Namespace {
    core: ObjectCore,
    /// Sorted by name, immutable after construction. There is no insert, no
    /// remove and no replace: a narrower namespace is a new object built from
    /// this one, so a handle to a namespace is a handle to a fixed set.
    entries: Box<[(Box<str>, Arc<Connector>)]>,
}

pub const MAX_NAMESPACE_ENTRIES: usize = 64;
pub const MAX_SERVICE_NAME: usize = 64;   // bytes
```

The namespace holds `Arc<Connector>`, not handle entries: a connector's
`on_zero_handles` is a no-op, so there is nothing for a namespace to keep alive
by counting. Both bounds are policy on the primitive, `MAX_*`-named per the rule,
and both answer `SyscallError::InvalidArgument` — a caller asking for a 65th
entry is a caller with a bug, not one to be truncated.

```rust
// kernel/src/object/syscap.rs
pub struct SysCap { core: ObjectCore }    // authority is entirely in Rights
```

Three rights ride on it and nothing else does:

| Right | Gates | Held by |
|---|---|---|
| `Rights::DEVICE` | `SYS_DEVICE_CLAIM` | `/bin/init` only |
| `Rights::RT` | `SYS_RT_ENTER` | init; an RT-only dup endowed per manifest |
| `Rights::MANAGE` | `SYS_PROCESS_OPEN` | init only |

The kernel creates exactly one full-rights `SysCap` at boot and installs it in
`/bin/init`'s initial handle table. Nothing else can construct one, so the set of
processes that can ever claim a device, enter the RT band or open a process by
pid is exactly what init endowed.

### 1.3 Objects adopted from `capability-handles-spec.md` with no change of shape

`PipeReadEnd`, `PipeWriteEnd`, `ConnectionEnd` (+ `ConnectionShared` with the
per-direction in-flight handle queues), `SharedMemObject`, `FileObject`,
`IoUringObject`, `DeviceClaim`, `ProcessObject`, `ThreadObject`,
`AddressSpaceObject`. The variant set is therefore:

```rust
pub enum KObjectRef {
    PipeRead(Arc<PipeReadEnd>),   PipeWrite(Arc<PipeWriteEnd>),
    Connection(Arc<ConnectionEnd>),
    Acceptor(Arc<Acceptor>),      Connector(Arc<Connector>),
    Namespace(Arc<Namespace>),
    SharedMem(Arc<SharedMemObject>),
    File(Arc<FileObject>),
    IoUring(Arc<IoUringObject>),
    Device(Arc<DeviceClaim>),
    Process(Arc<ProcessObject>),  Thread(Arc<ThreadObject>),
    SysCap(Arc<SysCap>),
}
```

Thirteen variants. `ListenerObject` from that spec is replaced by the
`Acceptor`/`Connector` pair, because a listener that holds a *name* is the
object this whole architecture exists to delete.

### 1.4 Rights

`capability-handles-spec.md` §4.2's eight bits, minus `DUP` — see below — plus
one. Every bit has a caller in this design:

```rust
pub struct Rights(u32);
Rights::DUP        // may be duplicated
Rights::TRANSFER   // may be endowed at spawn or sent over a connection
Rights::READ       // read a connection / pipe / file
Rights::WRITE      // write one
Rights::MAP        // SYS_SHM_MAP, SYS_PIPE_MAP, io_uring ring
Rights::WAIT       // block on it / io_uring POLL_ADD / SYS_PROCESS_WAIT
Rights::MANAGE     // SYS_PROCESS_KILL; SYS_PROCESS_OPEN on a SysCap
Rights::RT         // on a SysCap: SYS_RT_ENTER
Rights::DEVICE     // on a SysCap: SYS_DEVICE_CLAIM
```

`DUP` stays: init duplicates one `Connector` into several namespaces, and std's
`Command` duplicates a namespace handle to hand a child the same one. A
`DeviceClaim` is created **without** `DUP`, which is what keeps at most one
handle to a claim in existence and makes endowment of a claim a move.

---

## 2. The manifest

### 2.1 What `system.toml` says now

Eleven `system.toml` files exist
(`find . -name system.toml -not -path './rust/*' -not -path './target/*' | wc -l`
→ 11), carrying 26 `init` entries between them. `SystemConfig`
(`src/build.rs:17-29`) has `init: Vec<String>` and `programs:
HashMap<String, ProgramConfig>`, and `init` is the only field without a serde
default. The list is joined with `;` (`src/build.rs:601` and `:805`), baked into
the *bootloader* binary as the `INIT_PROGRAMS` env var
(`bootloader/build.rs:2-4`, `bootloader/src/main.rs:350`), carried in
`KernelArgs::init_program_{addr,len}`, and split twice by the kernel — on `;`
for programs and on whitespace for argv (`kernel/src/main.rs:603-610`).

`init` is an ordered list that orders nothing, and the config can express a
dependency it cannot honour. That is the honest statement of the defect in
`specs/issues/kernel/terminal-races-compositor-at-boot.md`.

### 2.2 The new shape

```toml
# Every program the image carries. `path` and `no-default-features` are
# unchanged.
[programs.compositor]
serves   = ["compositor"]
devices  = ["framebuffer", "keyboard", "mouse"]
receives = ["soundd", "filepicker"]
realtime = true

[programs.soundd]
serves   = ["soundd"]
devices  = ["hda-audio", "virtio-sound"]
realtime = true

[programs.netd]
serves   = ["netd"]
devices  = ["nic"]

[programs.terminal]
receives = ["compositor"]

[programs.doom]
receives = ["compositor", "soundd"]

[programs.filepicker]
serves   = ["filepicker"]
receives = ["compositor"]

# What init starts. A list, and now it genuinely orders nothing — because
# nothing needs it to.
[boot]
start = ["compositor", "soundd", "netd"]
```

Four new per-program keys, all `#[serde(default)]`:

| key | type | meaning |
|---|---|---|
| `serves` | `Vec<String>` | init creates a port per name and endows the **acceptor** |
| `receives` | `Vec<String>` | names in this program's namespace, each a **connector** |
| `devices` | `Vec<String>` | device classes init mints a claim for and endows |
| `realtime` | `bool` | init endows an `RT`-only `SysCap` dup |

`init: Vec<String>` becomes `[boot] start: Vec<String>` of **program names**, not
paths — a path in a boot list is a second spelling of a key the same file
already has, and it is what let `diag/system.toml` smuggle an argument through
(`"/bin/toybox pwd"`). Arguments move to `args = ["pwd"]` on the program entry.

### 2.3 The three variants and the eight test configs

| config | `[boot] start` | notes |
|---|---|---|
| `system.toml` | `compositor`, `soundd`, `netd` | unchanged set |
| `diag/system.toml` | `toybox` with `args = ["pwd"]` | no `serves`, no `receives`, **no `devices`** — that is what "nothing in this image can claim the framebuffer" now means, and it is checkable |
| `console/system.toml` | `console` | `devices = ["framebuffer", "keyboard", "mouse"]`, `serves = ["surface"]` |
| `tests/desktopcase` | `compositor`, `terminal` | `terminal` gains `receives = ["compositor"]` — the race becomes unrepresentable here first |
| `tests/desktopaudiocase` | `compositor`, `soundd`, `terminal` | + `receives = ["soundd"]` on terminal's children |
| `tests/doomcase`, `tests/doommusiccase` | `soundd`, `test-runner` | `doom` gains `receives = ["soundd"]` |
| `tests/metalcase` | `compositor`, `soundd`, `netd`, `sshd`, `test-runner` | |
| `tests/netcase`, `tests/sshdcase` | `netd` (+ `sshd`) | |
| `tests/testcases` | `soundd`, `test-runner` | |

**`diag` keeps its guarantee and gains a way to state it.** The diagnostic image
today contains nothing that *can* claim the framebuffer because of what is in
its `[programs]`; after this it declares `devices = []` for every program, and
§8's `no_diag_program_claims_the_screen` gate refuses a diag config that does
otherwise. That is strictly stronger than the property it replaces.

### 2.4 How the manifest reaches init

`src/build.rs` writes `/etc/system.manifest` into the initrd: a small,
line-oriented, byte-for-byte deterministic rendering of the resolved manifest —
program name, path, argv, and the four lists. TOML is not re-parsed in the guest:
the build system already has the parsed value, and a TOML parser in `/bin/init`
is a dependency the guest does not need.

`INIT_PROGRAMS`, `KernelArgs::init_program_addr`, `KernelArgs::init_program_len`,
`bootloader/build.rs`'s env plumbing and the two `config.init.join(";")` sites
are **deleted**. The kernel spawns `/bin/init` and nothing else. Consequences,
both wanted:

- The bootloader binary stops being a function of the boot config, so
  `src/build.rs:808`'s `bl_key = key_hash(&[PROFILE, &init_programs])` loses its
  second component and the bootloader is memoized once per profile. The hazard
  the comment at `src/build.rs:253-271` records — a concurrent build overwriting
  the `.efi` so an image carried another config's 28-byte init string — stops
  being expressible.
- `KernelArgs` shrinks by 16 bytes. The `const _` layout asserts in
  `toyos-abi/src/boot.rs` must be updated (`boot_partition_start_lba` 144 → 128
  and everything after it; `size_of` 208 → 192). The three offsets the kernel's
  `_start` reads by hand (16, 32, 40) are all before the deleted fields and do
  not move.

---

## 3. The complete ABI delta

Baseline, measured: **78 syscall constants**, highest number **98**
(`grep -cE '^pub const SYS_[A-Z_0-9]+: u64 = [0-9]+;' toyos-abi/src/syscall.rs`
→ 78). 21 numbers in `0..=98` are gaps; 15 of them carry a retirement comment,
6 (11, 12, 16, 22, 27, 69) are undocumented holes, and 4 (29, 30, 32, 33) still
have live `NotSupported` gravestone arms in the dispatch. **The first clean
number is 99**, and this delta takes 99–112.

### 3.1 New — 14 numbers

```
SYS_ENDOWMENTS      = 99   (buf_ptr, buf_len)          -> bytes written, or bytes needed when buf_len == 0
SYS_PORT_CREATE     = 100  ()                          -> (acceptor << 32) | connector
SYS_NAMESPACE_BUILD = 101  (args_ptr: *NamespaceBuild) -> RawHandle
SYS_NAMESPACE_OPEN  = 102  (ns_h, name_ptr, name_len)  -> RawHandle (a ConnectionEnd)
SYS_HANDLE_SEND     = 103  (conn_h, handles_ptr, n)    -> ()
SYS_HANDLE_RECV     = 104  (conn_h, out_ptr, cap)      -> n
SYS_SHM_CREATE      = 105  (size)                      -> RawHandle
SYS_SHM_MAP         = 106  (shm_h)                     -> vaddr
SYS_SHM_UNMAP       = 107  (shm_h)                     -> ()
SYS_PROCESS_WAIT    = 108  (proc_h)                    -> exit code
SYS_PROCESS_KILL    = 109  (proc_h)                    -> ()
SYS_PROCESS_OPEN    = 110  (syscap_h, pid)             -> RawHandle
SYS_DEVICE_CLAIM    = 111  (syscap_h, class)           -> RawHandle
SYS_RT_ENTER        = 112  (syscap_h)                  -> ()
```

Rights required, and the error when absent (`PermissionDenied` throughout, which
is the one legitimately dynamic `HandleError` per
`capability-handles-spec.md` §4.5):

| call | handle | needs |
|---|---|---|
| `SYS_PORT_CREATE` | — | nothing; a port with no clients is not authority |
| `SYS_NAMESPACE_BUILD` | `base` | `Rights::READ` on the namespace |
| | each `connector` | `Rights::TRANSFER` |
| `SYS_NAMESPACE_OPEN` | `ns_h` | `Rights::READ` |
| `SYS_HANDLE_SEND` | `conn_h` | `Rights::TRANSFER` |
| | each sent handle | `Rights::TRANSFER` |
| `SYS_HANDLE_RECV` | `conn_h` | `Rights::READ` |
| `SYS_SHM_MAP`/`UNMAP` | `shm_h` | `Rights::MAP` |
| `SYS_PROCESS_WAIT` | `proc_h` | `Rights::WAIT` |
| `SYS_PROCESS_KILL` | `proc_h` | `Rights::MANAGE` |
| `SYS_PROCESS_OPEN` | `syscap_h` | `Rights::MANAGE` |
| `SYS_DEVICE_CLAIM` | `syscap_h` | `Rights::DEVICE` |
| `SYS_RT_ENTER` | `syscap_h` | `Rights::RT` |

### 3.2 Retired — 13 numbers, never reused

| # | was | why it goes |
|---|---|---|
| 26 | `SYS_WAITPID` | a pid is not authority over a process |
| 31 | `SYS_OPEN_DEVICE` | first-come claiming; arbitration is the manifest now |
| 36 | `SYS_ALLOC_SHARED` | `SharedToken` is an id treated as a capability |
| 37 | `SYS_GRANT_SHARED` | the pid-ACL goes with it |
| 38 | `SYS_MAP_SHARED` | |
| 39 | `SYS_RELEASE_SHARED` | |
| 65 | `SYS_KILL` | pid-addressed |
| 68 | `SYS_PIPE_OPEN` | the original id-guessing hole; nothing left to guess |
| 70 | `SYS_PIPE_ID` | there is no id to hand a peer |
| 76 | `SYS_SOCKET_CREATE` | built a connection out of two pipe ids |
| 85 | `SYS_LISTEN` | there is no global name registry to register in |
| 87 | `SYS_CONNECT` | there is no global name registry to look up in |
| 96 | `SYS_SET_RT_PRIORITY` | ungated privilege; `SYS_RT_ENTER` replaces it |

The dispatch grows thirteen gravestone arms of the shape already at
`kernel/src/arch/syscall.rs:305` and `:307`. That is the second such pair; a
third would be a table, so **`retired_syscalls!` is a macro taking `number =>
"formerly SYS_NAME"` rows**, and the four existing entries (29, 30, 32, 33) move
into it in the same commit.

### 3.3 Renamed in place — same number, argument is now a `RawHandle`

`SYS_WRITE`(0), `SYS_READ`(1), `SYS_CLOSE`(10), `SYS_SEEK`(13), `SYS_FSTAT`(14),
`SYS_FSYNC`(15), `SYS_MARK_TTY`(28), `SYS_FTRUNCATE`(60),
`SYS_READ_NONBLOCK`(66), `SYS_WRITE_NONBLOCK`(67), `SYS_PIPE_MAP`(77),
`SYS_IO_URING_ENTER`(90). No source change in any of them beyond the type.

Six change shape as well:

- **`SYS_PIPE`(24)** returns two handles rather than two `Fd`s, and gains a
  `Result` — `sys_pipe` already answers `ResourceExhausted` on three paths and
  the wrapper splits the error word into `Fd(-1)`/`Fd(-8)`
  (`specs/issues/isolation/abi-wrappers-return-error-as-value.md`). That issue's
  fork edit (mio's waker) is in this branch's fork budget anyway, so it is fixed
  here.
- **`SYS_SPAWN`(25)** returns a `Process` handle; `SpawnArgs` grows (§4.2).
- **`SYS_DUP`(50)** → `SYS_HANDLE_DUP(h, rights)`; rights must be a subset.
  **`SYS_DUP2`(74)** → `SYS_HANDLE_DUP_AT(h, slot, rights)`. Both keep their
  numbers: this is the same operation with the rights argument the capability
  model requires, not a different one.
- **`SYS_ACCEPT`(86)** takes an `Acceptor` handle and returns **one** handle.
  Today it packs `(client_pid << 32) | fd`. The pid goes: peer identity is not
  the kernel's to assert, no caller authorizes on it once `SYS_PIPE_OPEN` is
  retired, and a server that wants to name its client reads it out of the
  protocol's first frame, where it is already a client's own claim about itself.
  `services::AcceptResult` collapses to a `Connection`.
- **`SYS_IO_URING_SETUP`(89)** takes an out-pointer and writes
  `{ handle: RawHandle, vaddr: u64 }`. Today it returns `(Fd, shm_token)` packed
  in a u64 and the caller maps the token — which is the whole of
  `specs/issues/design-debt/io-uring-abuses-shared-memory.md`. The ring owns its
  `PageAlloc` and the kernel maps it at setup.
- **`SYS_PROCESS_STATS`(94)** takes a `Process` handle. That re-scopes
  `specs/issues/diagnostics/process-stats-exited-child-only.md`: with a handle
  the caller may sample a *live* target, so what is left of that issue is the
  accounting, not the addressing.

Nine gain a claim-handle argument and lose their pid-keyed gate — the whole of
`device::is_owner` and the six `DEVICE_*_OWNER` statics go with them:
`SYS_GPU_PRESENT`(35), `SYS_GPU_SET_CURSOR`(43), `SYS_GPU_MOVE_CURSOR`(44),
`SYS_GPU_SET_RESOLUTION`(83), `SYS_NIC_RX_POLL`(78), `SYS_NIC_RX_DONE`(79),
`SYS_NIC_TX`(80), `SYS_DEVICE_REG_READ`(97), `SYS_DEVICE_REG_WRITE`(98).

### 3.4 Kept, deliberately

- **`SYS_THREAD_JOIN`(41)** keeps its `Tid`. `capability-handles-spec.md` §11
  replaces it with a handle; here it does not, because a `Tid` names nothing
  outside its own process — there is no cross-process thread id, no syscall
  accepts another process's `Tid`, and `ThreadObject` exists only so a joiner
  has something to hold. Deviation D5.
- **`SYS_GETPID`(51)**, and pids in logs, stats and scheduler keys: pure names.
- **`SYS_SYSINFO`(45)**, **`SYS_SCHED_INFO`(93)**, **`SYS_DEBUG`(92)**,
  **`SYS_SHUTDOWN`(19)**: still ambient. §12.

### 3.5 Struct layouts

```rust
// toyos-abi/src/handle.rs — new file
#[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawHandle(pub u32);
pub const HANDLE_INVALID: RawHandle = RawHandle(u32::MAX);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rights(u32);
```

```rust
// toyos-abi/src/syscall.rs — SpawnArgs, 80 bytes (was 48)
#[repr(C)] #[derive(Clone, Copy)]
pub struct SpawnArgs {
    pub argv_ptr: u64,   pub argv_len: u64,
    pub env_ptr: u64,    pub env_len: u64,
    /// [(child_slot: u32, parent_handle: RawHandle)] — DUPLICATED into the
    /// child. Stdio and nothing else, in practice.
    pub slot_map_ptr: u64, pub slot_map_count: u64,
    /// [EndowEntry] — MOVED out of the parent's table.
    pub endow_ptr: u64,  pub endow_count: u64,
    /// The label blob both `EndowEntry.off/len` index into.
    pub labels_ptr: u64, pub labels_len: u64,
}

#[repr(C)] #[derive(Clone, Copy)]
pub struct EndowEntry {
    pub label_off: u32, pub label_len: u32,
    pub handle: RawHandle,
    pub _pad: u32,          // named, so nothing leaks kernel stack into it
}   // 16 bytes

#[repr(C)] #[derive(Clone, Copy)]
pub struct NamespaceBuild {
    pub base: RawHandle,       // HANDLE_INVALID for an empty base
    pub _pad: u32,
    pub keep_ptr: u64, pub keep_n: u64,   // [NameRef] — names to carry from base
    pub add_ptr: u64,  pub add_n: u64,    // [NamespaceEntry] — new (name, connector)
    pub names_ptr: u64, pub names_len: u64,
}   // 56 bytes

#[repr(C)] #[derive(Clone, Copy)]
pub struct NameRef { pub off: u32, pub len: u32 }               // 8 bytes

#[repr(C)] #[derive(Clone, Copy)]
pub struct NamespaceEntry {
    pub off: u32, pub len: u32, pub connector: RawHandle, pub _pad: u32,
}   // 16 bytes
```

`SYS_ENDOWMENTS` writes the same `(EndowEntry[], label blob)` pair back to the
child: entry count, then the entries, then the blob. One call, one copy, and the
SDK parses it once.

**Two different verbs, two different vectors, and the difference is the point.**
`slot_map` *duplicates* — the parent keeps its stdout. `endow` *moves* — the
parent must `SYS_HANDLE_DUP` first if it wants to keep the thing. That is what
makes endowing a `DeviceClaim` work with no special case: a claim has no `DUP`
right, so the move is the only expressible form, and the parent provably no
longer holds it. `build_child_fds`'s current `PermissionDenied` for a
non-duplicable descriptor (`kernel/src/loader/start.rs:124`) survives on the
`slot_map` path and disappears from the endowment path.

**Atomicity.** Building the child's table is all-or-nothing under the parent's
`ProcessData` lock: every entry is verified (exists, right type where the kernel
cares, has `TRANSFER`) and only then removed. A failure removes nothing. This is
`capability-handles-spec.md` §8.1's `TransferBatch` discipline applied to spawn,
and it uses the same type.

### 3.6 Bounds

Every one is a `MAX_*` on the primitive, refuses by name, and never truncates.

| constant | value | a function of |
|---|---|---|
| `MAX_ENDOWMENTS` | 32 | the widest manifest row plus stdio; the compositor's is 6 |
| `MAX_NAMESPACE_ENTRIES` | 64 | five production names today; room for a decade |
| `MAX_SERVICE_NAME` | 64 bytes | `surface` is 7; the longest test name is 28 |
| `MAX_LABELS_LEN` | 4096 bytes | `MAX_ENDOWMENTS` × `MAX_SERVICE_NAME` plus slack |
| `MAX_PENDING_CONNECTIONS` | 32 | unchanged (`kernel/src/listener.rs:170`), now per port |
| `MAX_TRANSFER_HANDLES` | 8 | per `SYS_HANDLE_SEND` batch |
| `MAX_QUEUED_BATCHES` | 16 | per connection direction |

---

## 4. Spawn and endowment, end to end

### 4.1 Boot

```
kernel                                            /bin/init
------                                            ---------
create SysCap{DEVICE|RT|MANAGE}
spawn /bin/init:
  slots 0,1,2 = SerialConsole
  endow "syscap" = SysCap                    ->   SYS_ENDOWMENTS once, in the runtime
                                                  read /etc/system.manifest
                                                  for each `serves` name, anywhere in the manifest:
                                                      SYS_PORT_CREATE -> (acceptor, connector)
                                                  for each program in [boot] start:
                                                      SYS_DEVICE_CLAIM(syscap, class)  per `devices`
                                                      SYS_HANDLE_DUP(syscap, RT)       if `realtime`
                                                      SYS_NAMESPACE_BUILD(add = its `receives`)
                                                      SYS_SPAWN with the acceptors, claims,
                                                          RT cap and namespace endowed
                                                  serve "launcher" (§4.5) forever
```

**Every port exists before any server runs.** That is the whole mechanism: there
is no window, not a short one. A `receives` connector is live from the instant
the child's first instruction executes, and it stays live until the *server
process* dies.

`spawn_kernel` (`kernel/src/loader/mod.rs:886`) keeps its shape — three
`SerialConsole` slots, no parent, empty env — and gains the one endowment. The
path is the literal `/bin/init`, in the kernel, and `src/build.rs` installs that
binary in **every** image regardless of `[programs]`, the way `test-runner` is
installed into a test image. A boot whose initrd lacks it panics, which is what a
missing init deserves.

init has no parent, so nothing waits on it. If it exits, the kernel logs
`init exited: <code>` from the ordinary exit path and every other process is
already parentless — the machine has userland but no launcher, which is exactly
the state and exactly what the line says.

**Ports for programs init does not start at boot exist anyway.** `filepicker`
serves a name and is launched by the compositor; init creates its port up front
and endows the *acceptor* through the launcher when it spawns it, so an editor
holding the `filepicker` connector can connect before the picker has run a single
instruction. That is the whole of
`specs/issues/design-debt/pick-file-cannot-say-why-it-failed.md`'s reachable
cause.

**A device class can be minted once at a time**, so a program the launcher starts
may declare `devices` only if no boot program declares the same class — which is
what `every_device_class_has_at_most_one_claimant` (§8.1) checks across the whole
config rather than across `[boot] start`.

**io_uring watches an acceptor by koid.** `Descriptor::Listener` →
`Source::Listener(ListenerId)` becomes `KObjectRef::Acceptor` →
`Source::Acceptor(Koid)`, per `capability-handles-spec.md` §6.8's rule that
anything reached across a process boundary is selected by the object and never by
a number one process chose.

**A device class with no device is not endowed.** `SYS_DEVICE_CLAIM` answers
`NotFound` for a class no driver registered, and init endows what it got and logs
what it did not. soundd's existing degradation ("did I get an HDA or a
virtio-sound?") becomes "which claims are in my endowment table?", which is the
same question with the answer already in hand — and it removes the two-syscall
probe. `specs/issues/hardware/device-claim-succeeds-with-no-device.md` closes
from the other end: `Keyboard` and `Mouse` gain the same presence gate as the
other four, because init endowing a claim for hardware that does not exist is
now visibly wrong where a daemon claiming it was merely useless.

### 4.2 A process's view of its own endowments

```rust
// toyos/src/endow.rs — new
pub struct Endowments { /* parsed once from SYS_ENDOWMENTS */ }

impl Endowments {
    /// The process's own, parsed on first use. No global connect exists, so
    /// this is the only place a name is resolved at all.
    pub fn get() -> &'static Endowments;

    pub fn take<T: FromHandle>(&self, label: &str) -> Option<T>;
    pub fn namespace(&self) -> Option<&Namespace>;
}

/// Open a connection to `name` in this process's namespace.
///
/// `Err(NotEndowed)` means the name is not in the namespace this process was
/// given — a fact about this process, not about the machine.
/// `Err(ServerGone)` means the server exited. There is no third answer, and in
/// particular there is no "not yet".
pub fn service(name: &str) -> Result<Connection, EndowError>;
```

`toyos::services` is deleted. Its three functions had 30 real call sites
(`services::connect` 18, `services::listen` 6, `services::accept` 6, counted with
`rg -n --glob '!rust/**' --glob '!target/**' --glob '!specs/**'
'(crate::)?services::(connect|listen|accept)\(' | grep -v ':[0-9]*: *//'`).

Well-known labels, and the SDK constants for them:

| label | object | who gets it |
|---|---|---|
| `svc` | `Namespace` | every program with a non-empty `receives` |
| `syscap` | `SysCap` | init (full), and `RT`-only dups per `realtime` |
| `serve:<name>` | `Acceptor` | the program whose `serves` names it |
| `dev:<class>` | `DeviceClaim` | the program whose `devices` names it |

A label is a *local* name in one process's own table. Guessing one buys nothing:
a name not in your table resolves to nothing, and the table is not enumerable
across processes.

### 4.3 A server

```rust
// userland/compositor/src/session.rs, replacing services::listen("compositor")
let acceptor: Acceptor = endow::get().take("serve:compositor")
    .expect("the manifest declares this program serves `compositor`");
```

`.expect` is right here and was not right before. Today
`services::listen("compositor").expect("compositor already running")` conflates
"another process squatted the name" with "I am a second copy" — a runtime race
against an unprivileged global registry. After this it is a statement about the
manifest the image was built from, which `src/build.rs` checked at build time
(§8.1). A failure is a build-system bug, and fail-fast is exactly right for one.

`accept` is otherwise unchanged: `Acceptor` is pollable, `SYS_ACCEPT` blocks or
`io_uring` watches it, `MAX_PENDING_CONNECTIONS` still bounds the queue and
`ResourceExhausted` is still what a client over it sees.

### 4.4 A client, and the shell/terminal chain

`Window::create_with_flags` (`userland/window/src/lib.rs:372`) replaces
`services::connect("compositor")` at `:378` with `endow::service("compositor")`.
Its signature does not change, so `winit-toyos/src/window.rs:47` and the five
in-tree `Window::create*` callers are untouched.

The **six** `CreateError::NoCompositor` sites in that one function (`:378`,
`:391`, `:396`, `:402`, `:409`, `:414` — one failed lookup, four transport
failures and a failed `SharedMemory::map`) become three variants that can be told
apart: `NotEndowed`, `CompositorGone`, `Protocol`. The one the terminal renders
as `terminal: no compositor is running` is then only the first, and it is a
manifest statement rather than a race.

**The terminal passes a namespace subset to the shell.** Today it serves
`surface.<pid>` and puts that name in `TOYOS_SURFACE`
(`userland/terminal/src/main.rs:41-48`); the shell inherits the string through
the environment and `userland/toybox/src/locale.rs:33` reads it back. After this:

```rust
let (acceptor, connector) = port::create();                  // SYS_PORT_CREATE
let child_ns = namespace::build()
    .keep(endow::namespace(), &["compositor", "soundd", "launcher"])
    .add("surface", connector)
    .finish();                                               // SYS_NAMESPACE_BUILD
Command::new("/bin/shell")
    .stdin(..).stdout(..).stderr(..)
    .endow("svc", child_ns)
    .spawn()?;
```

`TOYOS_SURFACE` is deleted, and with it the class: **the one ambient string in
the system that names a service**. A wizard three processes below a terminal
still finds its surface, because the namespace is inherited by handle down the
same chain the environment used to be — but nothing outside that chain can name
it, and nothing in the chain can name a service its parent did not pass. The
`surface.<pid>` name, `surface::service_name` and `MAX_NAME` go with it; the
service is called `surface` in every namespace, because a name in a private
namespace does not need to be unique machine-wide. That deletes the one
remaining place a pid was load-bearing in the IPC layer.

`/bin/console` does the same with itself at the root of the tree.

### 4.5 The launcher, and programs a user starts

`SYS_SPAWN` is the primitive and it is capability-correct: a parent can endow
only what it holds. That is not enough for the ruling's eighth point. doom is
started by the shell, and doom is to hold sound; the shell is not. Under
inheritance alone, doom could hold sound only if the terminal did.

So **init serves `launcher`**:

```
MSG_LAUNCH { program: [u8; 32], argv: bytes }  ->  MSG_LAUNCHED { } + one Process handle
```

init looks the program up in the manifest it already holds, builds exactly the
endowments that program declares, spawns it, and sends the caller a `Process`
handle over `SYS_HANDLE_SEND`. The asker's capability is "may ask init to start a
declared program" — one connector — and the *policy* is the manifest. This is a
component manager in the smallest form that does the job.

`toyos::process::Command` routes through the launcher when the program is in the
manifest and uses `SYS_SPAWN` otherwise. std's `Command` (`sys/process/toyos.rs`)
does the same, so `sh -c` and `Command::new("/bin/doom")` behave identically.

**What this buys, exactly:** the shell holds `compositor`, `surface` and
`launcher`; doom holds `compositor` and `soundd`. Neither can name the other's.
Without the launcher, doom would hold the shell's set and item 8's example would
be false.

**What it costs:** a spawn is an IPC round trip when it goes through the
launcher, and `/bin/init` is resident in every image including `diag`. The round
trip is not on any measured hot path — nothing in gate A or the desktop tests
spawns in a loop — and init's resident cost is one process with a namespace, a
manifest and an accept loop.

### 4.6 What each shipped program ends up holding

The ruling's eighth point, enumerated. Every row is exactly the program's
manifest entry; nothing else is reachable, because there is no other place to
ask. `stdio` is slots 0/1/2 in every row and is omitted.

| program | acceptors | connectors (its namespace) | devices | syscap | process handles |
|---|---|---|---|---|---|
| `init` | `launcher` | — | — | `DEVICE\|RT\|MANAGE` | every program it started |
| `compositor` | `compositor` | `soundd`, `filepicker`, `launcher` | framebuffer, keyboard, mouse | `RT` | the terminals and pickers it launched |
| `soundd` | `soundd` | — | hda-audio and/or virtio-sound | `RT` | — |
| `netd` | `netd` | — | nic | — | — |
| `sshd` | — | `netd`, `launcher` | — | — | its session shells |
| `filepicker` | `filepicker` | `compositor` | — | — | — |
| `terminal` | `surface` (created by itself) | `compositor`, `launcher` | — | — | its shell |
| `console` | `surface` (created by itself) | `launcher` | framebuffer, keyboard, mouse | — | its shell |
| `shell` | — | `surface`, `launcher` | — | — | its children |
| `doom` | — | `compositor`, `soundd` | — | — | — |
| `snake`, `editor`, `paint`, `files` | — | `compositor`, `filepicker` | — | — | — |
| `toybox` | — | `compositor`, `soundd`, `surface` | — | — | — |
| `toyos-ld`, `toyos-cc`, `proctest`, `input-test` | — | — | — | — | own children |

Read across: **doom cannot express reaching netd, the filepicker, a device, a
daemon or a process it was not given** — those names are not in its namespace and
its handle table has nothing else in it. sshd, the one program reachable from the
network, holds `netd` and `launcher` and cannot acquire a compositor connection at
all. `toyos-cc` and `toyos-ld` hold nothing but stdio.

Two honest limits, both visible in the table:

- **`toybox` is one binary with many applets, so its row is the union of what
  every applet needs**: `screen` wants the compositor
  (`userland/toybox/src/screen.rs:4`), `tone` wants soundd through cpal, `locale`
  wants the surface. `ls` therefore holds a compositor connector it will never
  use. Splitting applets into binaries is the fix and it is not this branch's;
  the granularity of least authority is the granularity of the binary.
- **Every row can still open any path.** The filesystem is ambient (D6, §12).

### 4.7 sshd sessions

sshd's manifest row is `receives = ["netd", "launcher"]`. Per session it creates
the stdio pipes it already creates and asks the launcher for `/bin/shell`,
handing the pipes as the child's slots via a `MSG_LAUNCH` that carries them over
`SYS_HANDLE_SEND`. The session shell therefore gets exactly what the manifest
says a shell gets, not whatever sshd happens to hold — which is the property
that matters for a program reachable from the network. sshd itself never holds a
`compositor` connector, and after this it cannot acquire one.

---

## 5. Queueing and failure semantics

### 5.1 What a client observes, at each point in the server's life

| server state | `SYS_NAMESPACE_OPEN` | first write | first read |
|---|---|---|---|
| not spawned yet | succeeds; connection queued on the port | goes into the ring | blocks |
| spawned, not yet at `accept` | succeeds | goes into the ring | blocks |
| accepted, alive | succeeds | ring | data |
| exited without accepting | `NotFound` (`closed`) | `BrokenPipe` on the handle already held | `0` (EOF) |
| exited after accepting | `NotFound` | `BrokenPipe` | `0` |
| never in this namespace | `NotFound`, and it is `NotEndowed` in the SDK | — | — |

The two `NotFound`s are distinguished in the SDK because the kernel answers them
from different objects: a missing namespace entry versus a closed port. The
kernel-side error word is the same and that is correct — `NotFound` truthfully
means "there is nothing here to connect to" in both cases.

**No row in that table is reached by waiting.** There is no timer, no deadline
and no retry constant anywhere in this design. The one thing that used to need a
bound — "has the server registered its name yet?" — is not a question that can
be asked, because the port is created by the process that spawns both sides,
before either runs.

### 5.2 Backpressure

Unchanged from today, and deliberately so:

- A pipe's 2 MiB ring page is allocated on first *use*, so a queued connection
  nobody has answered costs a `PendingConnection` and nothing else. Measured in
  `abuse_connect_flood`: 32 unaccepted connections cost 0 KiB; the first byte
  written on one costs 2048 KiB
  (`specs/issues/isolation/client-request-is-an-allocation.md`).
- Ring full: `pipe::try_write` returns `None`, a blocking write parks on the
  pipe's writer queue, a non-blocking one gets `WouldBlock`.
- **A server never blocks on a client.** Every server write is one `try_send`
  whose refusal drops the peer by name; `ipc::FrameRx` buffers a frame whole
  before anything acts on it. Nothing here changes that, and the rule now has a
  structural companion: a server holds an `Acceptor`, which has no write path at
  all.
- `MAX_PENDING_CONNECTIONS` (32) bounds one port's unaccepted queue and answers
  `ResourceExhausted` past it, exactly as `push_connection` does today.

**One new obligation on the manifest.** init opens no connections at boot — it
endows connectors, and a connector costs nothing until a client uses it — so the
declared-edge count does not consume queue depth. If that ever changes, the
build-time gate in §8.1 is where the depth check belongs.

### 5.3 Teardown

Every path below is on both exit and kill, because both drain the handle table
through `teardown_resources` on the killer's CPU.

| what goes | what fires | what the peer sees |
|---|---|---|
| last `Acceptor` handle | `closed = true`; queue dropped | queued clients: `BrokenPipe`/EOF. New opens: `NotFound` |
| last `ConnectionEnd` handle | both `PipeShared` ends' counts fall | peer: `BrokenPipe` on write, `0` on read |
| last `Connector` handle | nothing | the server keeps accepting from whoever else holds one |
| last `Namespace` handle | its `Arc<Connector>`s drop | nothing observable; a connector outlives it if a handle does |
| last `DeviceClaim` handle | the class is released for re-minting | init can mint a fresh claim for a respawned daemon |
| last `SharedMemObject` handle **and** mapping | pages freed | — |

**A daemon crash is recoverable and always was, but for a new reason.** Today
`device::Claim::drop` releases the class and a respawned daemon re-claims
first-come. After this the class is released the same way, and *only init* can
mint the replacement — so a process racing the respawn for the claim is not a
thing that can happen.

**The cross-pair cycle** `capability-handles-spec.md` §8.4 accepts is unchanged
and still accepted: A's connection queued in B's while B's is queued in A's leaks
both until reboot. No protocol here transfers a connection into a connection, the
per-variant `LIVE_*` census makes it visible, and `SYS_HANDLE_SEND` refuses a
handle naming the connection it is sent on.

---

## 6. The migration ledger

### 6.1 Servers

| program | today | after |
|---|---|---|
| **compositor** | `services::listen("compositor")` at `session.rs:130`, first statement of `Session::start` | `endow::get().take("serve:compositor")`. `Command::new("/bin/filepicker").spawn().ok()` at `:220` becomes a launcher call, and the filepicker port is created by *init*, so an editor's `pick_file` cannot precede it |
| **soundd** | `services::listen("soundd")` at `main.rs:1766` | acceptor endowment; `MAX_CONTROL_CLIENTS = 63` unchanged; the shm token and `signal_pipe_id` in `MSG_STREAM_OPENED` become two transferred handles (§6.3) |
| **netd** | `services::listen("netd")` at `main.rs:1206`, after `DmaNic::open()` | acceptor + `dev:nic` claim endowment; the whole pipe-id protocol becomes handle transfer (§6.4) |
| **filepicker** | `services::listen("filepicker")` at `main.rs:463`; `accept` at `:466` is `.expect("accept failed")` | acceptor endowment; the `.expect` stays — an acceptor that refuses is a kernel bug now, not a peer |
| **sshd** | no service name; `TcpListener::bind` via netd | unchanged shape; `receives = ["netd", "launcher"]` |
| **terminal / console** | `Host::listen("surface.<pid>")`, `TOYOS_SURFACE` in the child's env | `SYS_PORT_CREATE` + `"surface"` in the child's namespace |

### 6.2 Clients

Nine real `services::connect` sites outside the test binaries, all of which
change:

| site | name | today | after |
|---|---|---|---|
| `userland/window/src/lib.rs:328` (`clipboard_set`) | compositor | `.expect("compositor not running")` — **panics the caller** | `endow::service`, and the panic becomes a returned error |
| `userland/window/src/lib.rs:378` (`create_with_flags`) | compositor | 6 collapsed `NoCompositor` sites | 3 distinguishable variants |
| `userland/toybox/src/screen.rs:4` | compositor | `.expect("compositor not running")` — **panics** | error return |
| `userland/filepicker-api/src/lib.rs:23` | filepicker | `.ok()?` — silently "user cancelled" | `Result<Option<String>, PickError>`; the boot-race variant no longer exists |
| `toyos/src/audio.rs:267` | soundd | inside the 100×10 ms loop | one `endow::service` |
| `toyos/src/net.rs:268` (`NetdConn::connect`) | netd | one shot | one `endow::service` |
| `toyos/src/net.rs:273` (`connect_blocking`) | netd | 100×10 ms loop, 4 callers | **deleted**; the four callers use `connect` |
| `toyos/src/surface.rs:367` (`Keys::grab`) | `surface.<pid>` from env | `GrabError::HostGone` | namespace `"surface"` |
| `toyos/src/surface.rs:411` (`notify_layout_changed`) | same | best-effort | same |

Deleted with them: `NetdConn::BOOT_RETRIES`, `NetdConn::BOOT_RETRY_INTERVAL_NS`,
`AudioStream::BOOT_RETRIES`, `AudioStream::BOOT_RETRY_INTERVAL_NS` — four
constants, two identical policies, nine source lines
(`rg -n 'BOOT_RETRIES|BOOT_RETRY_INTERVAL_NS'` → 9 hits, 8 of them code). And
per boot on metal-sim, sshd's **100 `SYS_NANOSLEEP` calls and its exit at
t=1.69 s on a boot that completed at 0.38 s**
(`specs/issues/hardware/network-clients-pay-a-boot-retry.md`).

### 6.3 soundd's stream protocol

```
today:   MSG_STREAM_OPENED { period, rates, shm_token: u32, signal_pipe_id: u64 }
         client: syscall::map_shared(token); syscall::pipe_open(signal_pipe_id, 0)

after:   MSG_STREAM_OPENED { period, rates }            — plain data, no ids
         SYS_HANDLE_SEND(conn, [shm_h_with_MAP_only, pipe_read_h])
         client: SYS_HANDLE_RECV(conn) -> [shm_h, pipe_h]; SYS_SHM_MAP(shm_h)
```

`toyos::audio::AudioStream::open` keeps its signature, so
`/Users/jan/dev/jan/forks/cpal/src/host/toyos/mod.rs:171` is untouched. The three
`assert_eq!`s at `:181-186` that re-check rate, channels and period against what
soundd granted are untouched too.

This is `capability-handles-spec.md` §8.3 verbatim, and it closes the crash-
detection hole that spec names: a client killed while blocked in the signal-pipe
read drains its table, the last `PipeReadEnd` handle disappears, and soundd's
next write sees `BrokenPipe` — by construction rather than by bookkeeping.

### 6.4 netd's protocol

netd is the heaviest single migration and the one with the most to gain. Its
wire format carries **eleven** `*_pipe_id: u64` fields
(`toyos/src/net.rs:131,132,149,166,167,181,182` and netd's mirrors at
`main.rs:212,213`), and netd calls `pipe::open_by_id` at `main.rs:224` in five
places (`:229`, `:230`, `:563`, `:567`, `:866`, `:935`, `:1137`). Every one
becomes a `SYS_HANDLE_SEND` of the pipe end itself.

`SYS_SOCKET_CREATE` — netd's way of turning two pipe ids it was told about into a
connection — has no purpose left and is retired. `toyos::net`'s public functions
keep their signatures, so the mio fork's `toyos_stream.rs`/`toyos_listener.rs`
see no change beyond the `Fd` rename.

### 6.5 The SDK

| module | change |
|---|---|
| `toyos/src/services.rs` (25 lines) | **deleted** |
| `toyos/src/pipe.rs` (11 lines) | **deleted** — `open_by_id` is the id-as-capability API |
| `toyos/src/endow.rs` | new |
| `toyos/src/port.rs`, `toyos/src/namespace.rs` | new |
| `toyos/src/handle.rs` | new — `OwnedHandle`, `!Copy`, `Drop` closes |
| `toyos/src/audio.rs` | retry loop out, handle recv in; signature stable |
| `toyos/src/net.rs` | retry loop out, handle transfer in; signatures stable |
| `toyos/src/surface.rs` | `service_name`/`MAX_NAME`/`HOST_ENV` out; namespace in |
| `toyos/src/shm.rs` (69 lines) | `SharedToken` → `SharedMem` handle |
| `toyos/src/ipc.rs` (536 lines) | `Connection` wraps an `OwnedHandle`; framing unchanged |
| `toyos/src/lib.rs` | `Pipe::pipe_id` deleted, `pipe_map` keeps its handle form |

**The rename has a collision and it is resolved this way.** `toyos::Handle`
already exists (`toyos/src/lib.rs:47-61`) as the owning RAII wrapper whose `Drop`
calls `close`, and `AsHandle::as_handle() -> Fd` is implemented by `Listener`,
`Device`, `Pipe` and the rest. So:

- `toyos_abi::Fd` → **`toyos_abi::RawHandle`** — the bare `u32`, `Copy`, no
  destructor. This is the name in the ABI, in the kernel and in every fork.
- `toyos::Handle` → **`toyos::OwnedHandle`**, `!Copy`, `!Clone`, `Drop` closes,
  with `into_raw` as the single greppable escape hatch —
  `capability-handles-spec.md` §10's shape and std's `OwnedFd` idiom. `Pipe::into_fd`'s
  `core::mem::forget` becomes that `into_raw`.
- `AsHandle::as_handle() -> RawHandle` keeps its name and its meaning.

`specs/issues/design-debt/fd-is-a-unix-ism.md` asks for `Fd` → `Handle`. It gets
`Fd` → `RawHandle`, because `Handle` was taken by the *owning* type and the owning
type is the one that should hold the short name.

### 6.6 Out-of-repo: the std fork and the ecosystem forks

This is the part of the plan with the most schedule risk and it is stated first
in §10 as an owner-level item.

**`rust/` (submodule, separate repo).** 23 `toyos`-pathed files under
`rust/library`, 3136 lines (`find rust/library -path '*toyos*' -type f | xargs
wc -l`). The ones this touches:

| file | lines | why |
|---|---|---|
| `std/src/sys/process/toyos.rs` | 468 | `SpawnArgs`, spawn returns a handle, `waitpid`→`SYS_PROCESS_WAIT`, `kill`→`SYS_PROCESS_KILL`, launcher routing, namespace inheritance |
| `std/src/sys/net/connection/toyos.rs` | 707 | `toyos::net` signatures stable; only the `Fd` type name moves |
| `std/src/sys/pal/toyos/mod.rs` | 220 | `Fd`→`Handle` |
| `std/src/sys/stdio/toyos.rs` | 223 | slots 0/1/2 keep working by the gen-0 encoding; type name only |
| `std/src/sys/pipe/toyos.rs` | 93 | `SYS_PIPE` now fallible |
| `std/src/os/toyos/io.rs` | 76 | `AsRawFd`-shaped surface becomes `AsRawHandle` |
| `std/src/os/toyos/process.rs` | 31 | `CommandExt::namespace(..)` added |
| `std/src/sys/fs/toyos.rs` | 552 | type name only — the filesystem is out of scope |

`rg -o 'toyos_abi::Fd' rust/library` → **9 hits**.

**The blocker underneath it.** `rust/library/std/Cargo.toml:106-107` names
`toyos-abi` and `toyos` by the relative path `../../../toyos-abi`, and
`toolchain::rust_dir` resolves `rust/` to the **primary** checkout, so
`x build library` compiles std against `/Users/jan/Dev/jan/toyos/toyos-abi` —
main's — no matter which worktree runs it and no matter who holds the sysroot.
`--claim-sysroot` records a witness of *this* worktree's sources while building
from the primary's. That is
`specs/issues/build/std-change-needs-an-unlanded-abi-change.md`, and for a branch
whose whole content is an ABI change it is not an inconvenience but a wall: the
kernel would link against this tree's struct layouts and std against main's.

**Chunk 0 fixes it** (§9). `src/toolchain.rs` writes `rust/.cargo/config.toml`
with a `paths` directory override naming the *building* worktree's `toyos-abi`
and `toyos` before it invokes `x build library`, and deletes it after. No such
file exists today (`ls -a rust/.cargo` → no such directory), and the file is
written by the build system rather than committed to the fork, so the fork's
delta is unaffected. The chunk is not done until a test proves it: build a
sysroot from a worktree whose `toyos-abi` carries a marker constant and assert
the marker is in the built `libstd`.

**Ecosystem forks.** Four files across two forks name things this changes:

```
rg -n "toyos_abi::Fd|map_shared|syscall::pipe\b" ~/dev/jan/forks
  socket2/src/sys/toyos.rs:26        use toyos_abi::Fd;
  mio/src/sys/toyos/selector.rs:7    use toyos_abi::Fd;
  mio/src/sys/toyos/selector.rs:30   toyos_abi::syscall::map_shared(shm_token)
  mio/src/sys/toyos/waker.rs:4       use toyos_abi::Fd;
  mio/src/sys/toyos/waker.rs:13      let pipe = toyos_abi::syscall::pipe();
  mio/src/net/tcp/toyos_stream.rs:6  use toyos_abi::Fd;
  mio/src/net/tcp/toyos_listener.rs:7 use toyos_abi::Fd;
```

`mio`: five lines across four files (`Fd`→`Handle`, `io_uring_setup` returns its
own mapping so the `map_shared` line is deleted, `pipe()` is now fallible and
`Waker::new` already returns `io::Result`). `socket2`: one line.

`cpal`, `winit`, `softbuffer`, `tokio`, `russh`, `getrandom`, `libloading`,
`stacker`, `ctrlc`, `raw-window-handle`, `memmap2`, `target-lexicon`:
**untouched**, because the SDK signatures they call do not change. That is the
constraint stated in §0 doing its job, and it is why it is a constraint and not
a preference.

The fork half lands as commits on each fork's `toyos` branch, and the monorepo
PR carries the `Cargo.lock` rev bumps. `forks.toml`'s `delta` figures for `mio`
and `socket2` are updated in the same commit.

### 6.7 Tests

The suite produces **296** verdicts on a full run: 17 `SCREEN_TESTS`, 107
`MACHINE_TESTS`, 2 `AUDIO_TESTS`, 60 shared-boot Rust guest tests (90 files in
`tests/toyos-rust-tests/src/bin/` minus 30 `RUST_SKIP` entries) and 110 C tests
(121 discovered minus 11 `C_DOES_NOT_BUILD`). `tests/toyos.rs` is 11,712 lines
and has no `#[test]` in it — it is a harness binary.

**The C corpus needs no migration at all.** `grep -rlE '\b(socket|connect|listen|pipe|shm|ioctl)\s*\('
over `tests/testcases/tinycc/` and `tests/testcases/pp_tcc/` returns zero files.
That is 110 of the 296 verdicts untouched.

**Guest Rust binaries: 21 of 90 use the deleted surface directly**, plus four
that reach the compositor only through the `window` crate (`window_caps`,
`window_child`, `window_drag`, `locale_gate`) and five sound clients
(`audio_tone`, `audio_tone_load`, `audio_idle_suspend`, `hda_client_stall`,
`null_sink_client_exits`). Call it 30 files.

**Eleven of them, 1,591 lines, are *about* the model being deleted and are
rewritten rather than migrated:**

| file | today | after |
|---|---|---|
| `abuse_listener_hijack.rs` (178) | `listen; dup; close` must not leave a stale fd that steals the real service's name | there is no name and no second acceptor: "a `Connector` cannot accept" is a `WrongType` arm plus a type check |
| `abuse_pipe_owner.rs` (120) | sweeps `pipe_open(id)` over ids 0..256 | no pipe ids: a `RawHandle` from another process's table is `BadHandle`/`Stale` here |
| `pipe_peer_scope.rs` (85) | **written to pass because the hole is open** — a peer entitled to one of a creator's pipes is entitled to all of them — and its own first line says it goes red when the hole closes | the hole is gone with `SYS_PIPE_OPEN`; the file is deleted and its property is subsumed by the line above. **Deleting it is deliberate and must be said in the commit message**, not a red silently made green |
| `abuse_pipe_map.rs` (85), `abuse_pipe_ring.rs` (124) | `SYS_PIPE_MAP` by fd | same property, handle-addressed; `SYS_PIPE_MAP` survives |
| `abuse_shared_grant.rs` (149), `shm_release_reclaims.rs` (120) | the shm pid-ACL and `release` | `SharedMem` handles: "grant" has no spelling, and release is a handle drop |
| `abuse_connect_flood.rs` (107) | floods `SYS_CONNECT` against a name it registered itself | floods `SYS_NAMESPACE_OPEN` against a port it created. **The "the attacker can be its own service" clause of the attack dies** — it must now be given a connector — so the test becomes a bound check rather than a self-service DoS |
| `abuse_gpu_resolution.rs` (32) | `SYS_GPU_SET_RESOLUTION` is reachable only by the framebuffer owner | the claim handle is the gate; the negative arm becomes "without the claim handle" |
| `fd_lifetime.rs` (211) | last-descriptor release for listener and ring ids | last-*handle* release, which is `handle_count` by construction |
| `device_claim_lifetime.rs` (184) | `open_device; dup; close` | there is no `open_device`; the property becomes "an endowed claim moves and the parent no longer holds it" |
| `window_refusal.rs` (75) | **squats `services::listen("compositor")`** to make a fake compositor that refuses | squatting is unrepresentable. It becomes a real compositor refusal, or a test-only server endowed an acceptor the client's namespace points at |

`abuse_fd_table.rs` (69) keeps its subject — the table cap on all three insertion
paths — and gains the fourth: the endowment vector.

The eight test `system.toml` files gain `serves`/`receives`/`devices` rows.

**Host-side harness assertions that must be re-read, not just re-run:**

- `metal_sim_compositor` (`tests/toyos.rs:3863-3886`) asserts four exact daemon
  lines, one of which is `sshd: no network on this machine, exiting`.
  `tests/metalcase/system.toml`'s own comment prices that line at "the second
  `NetdConn::connect_blocking` spends retrying a netd that will never come".
  **After this branch that second is gone**: netd exits before or shortly after
  sshd asks, so sshd's `endow::service("netd")` is either `ServerGone` at once or
  a connection whose first write is `BrokenPipe` — learned from the guest, never
  from a clock. The comment is corrected in the same commit.
- `netd_connection_caps` parses netd's own declared cap out of
  `netd: ready, at most ` and passes it to the guest binary as argv. netd's cap
  arithmetic does not change; the line must survive.
- `desktop_audio_client` waits for `soundd to take up both connects`
  (`tests/toyos.rs:5429`) and counts `soundd: client ` lines (`:5653`).
- Five tests use `compositor: ready` as their `ready_marker` and **do not boot at
  all** if that line moves: `desktop_window_child`, `desktop_typing_damage`,
  `desktop_locale_detect`, `desktop_audio_client`, `blocked_dump`.
- `sshd_fail_closed` asserts five exact sshd lines including the negative
  `sshd: listening on port 22`.

---

## 7. The deletion ledger

### 7.1 Code

| what | where | size |
|---|---|---|
| the whole service registry | `kernel/src/listener.rs` | 232 lines |
| `sys_listen`, `sys_connect`, `wake_poll_waiters` | `kernel/src/arch/syscall.rs:1246-1344` | 99 lines |
| `sys_pipe_open`, `sys_pipe_id`, `sys_socket_create` and `may_open_pipe` | inside `kernel/src/arch/syscall.rs:1002-1155`, which also holds `sys_pipe_map` (kept) | ~120 lines, estimated from the span less `sys_pipe_map`'s 33 |
| `kernel/src/shared_memory.rs` | whole file | 353 lines |
| `DEVICE_*_OWNER` statics, `device::is_owner` | `kernel/src/device.rs:21-37` and callers | — |
| `Pipe::creator`, `open_reader`/`open_writer`, `NotOpened` | `kernel/src/pipe.rs:145,251-291` | ~45 lines |
| `zombify`, `OrphanCleanup`, `handle_orphans`, `collect_orphan_zombies`, the idle-loop reaping pass | `kernel/src/process.rs` | per `capability-handles-spec.md` §6.6 |
| `INIT_PROGRAMS` chain | `src/build.rs:601,805,633,874`, `bootloader/build.rs`, `bootloader/src/main.rs:350,505`, `toyos-abi/src/boot.rs` × 2 fields, `kernel/src/main.rs:371-391,603-610` | ~40 lines across 5 files |
| `toyos/src/services.rs`, `toyos/src/pipe.rs` | whole files | 36 lines |
| the two retry loops and four constants | `toyos/src/net.rs:264-279`, `toyos/src/audio.rs:187-189,265-273` | ~19 lines |
| `TOYOS_SURFACE`, `surface::service_name`, `MAX_NAME` | `toyos/src/surface.rs:40,79-103`; `userland/terminal/src/main.rs:41,48`; `userland/console/src/main.rs:66,336`; `userland/toybox/src/locale.rs:32-33` | ~40 lines across 5 files |
| `AcceptResult.client_pid` and its plumbing | `toyos/src/services.rs:7-10`, `toyos-abi`, `kernel` | — |

**Not** deleted, and worth saying: `ipc.rs`'s framing (536 lines) is unchanged.
`FrameRx`, `MAX_FRAME_LEN`, the no-padding `ipc_payload!` proof and the
never-block-on-a-client discipline are orthogonal to who holds what, and this
work neither helps nor harms them.

### 7.2 `specs/issues/` this closes

| file | how |
|---|---|
| `specs/issues/kernel/terminal-races-compositor-at-boot.md` | the race is unrepresentable: the port exists before either process |
| `specs/issues/design-debt/pick-file-cannot-say-why-it-failed.md` | `Result<Option<String>, PickError>`, and the reachable cause is gone |
| `specs/issues/hardware/network-clients-pay-a-boot-retry.md` | no retry loop exists |
| `specs/issues/isolation/capability-by-id-or-name.md` | all four instances: `PipeId`, the service name, `SharedToken`, the device claim |
| `specs/issues/design-debt/fd-is-a-unix-ism.md` | `Fd` → `Handle` |
| `specs/issues/design-debt/sharedtoken-has-no-raii.md` | `SharedToken` deleted; `SharedMemObject` is Arc-lifetimed |
| `specs/issues/design-debt/io-uring-abuses-shared-memory.md` | the ring owns its `PageAlloc`; `SYS_IO_URING_SETUP` returns its own mapping |
| `specs/issues/hardware/device-claim-succeeds-with-no-device.md` | keyboard and mouse gain the presence gate the other four have; init endows only what exists |
| `specs/issues/isolation/abi-wrappers-return-error-as-value.md` | `pipe()` becomes fallible; the mio edit is in this branch's fork budget |
| `specs/issues/build/std-change-needs-an-unlanded-abi-change.md` | chunk 0 |

### 7.3 `specs/issues/` this re-scopes

| file | what is left |
|---|---|
| `specs/issues/isolation/process-isolation-ungated.md` | `SYS_LISTEN`'s squat and the `SYS_GRANT_SHARED` no-revoke clause both go. What remains is the general revocation question, and the entry's own trigger fires: *"It stops being sound the moment the reachable set is no longer exactly what the owner named — … when `SYS_HANDLE_SEND` makes a grant transferable."* Rewrite it around that. |
| `specs/issues/diagnostics/process-stats-exited-child-only.md` | addressing fixed by the handle; the accounting gap is untouched |
| `specs/issues/diagnostics/syscall-profile-is-64-bins-wide.md` | worse, then fixed: the highest number goes 98 → 112, so the `[u32; 64]` must be sized from the ABI in this branch rather than after it |
| `specs/issues/isolation/client-request-is-an-allocation.md` | the third instance's "the attacker can be its own service" clause dies with `SYS_LISTEN`; the compositor's unbounded windows remain |
| `specs/issues/isolation/compositor-and-netd-unbounded-accept.md` | unchanged; the compositor's half is still owed |
| `specs/issues/build/abi-split-reads-commits-not-the-tree.md` | this branch is the case the `Abi-Inseparable` trailer exists for; the finding is unaffected |

---

## 8. Gates

Every gate below either fails on today's tree or has a negative control that
fails on a tree with the defect reintroduced. A gate with neither is not listed.

### 8.1 The race, refused at build time — `cargo test --lib`, milliseconds

`src/build.rs`, the shape of `no_shipped_boot_config_starts_sshd`
(`src/build.rs:1095`, the file's only `#[test]` today):

- `every_receives_names_a_serves` — for each of the 11 configs, every name in
  any program's `receives` must appear in some program's `serves` in the *same
  config*. **This is the gate with the sharpest teeth on the list**: it is the
  build-time form of "a client cannot name a service the system does not have",
  it needs no guest and no mutated tree, and its negative control is a bad config
  literal in the test body.
- `every_device_class_has_at_most_one_claimant` — two programs declaring
  `devices = ["framebuffer"]` is a config init cannot satisfy, and today it is a
  runtime first-come race.
- `no_diag_program_claims_the_screen` — `diag/system.toml`'s programs declare no
  `devices`. This makes the diagnostic image's whole reason for existing
  checkable for the first time.
- `every_started_program_is_declared` — `[boot] start` names program keys, so a
  typo is a build error rather than a kernel panic at `spawn_kernel`.

### 8.2 Queueing — `connect_before_serve`

A guest binary, on the shared `tests/testcases` boot (no new boot). The parent
creates a port, spawns the **client** with the connector endowed, waits for the
client's first frame to be in the ring, and only then spawns the server with the
acceptor.

Two arms that must answer differently, the `hda_client_stall` shape:

1. The client writes and reads a reply. It must succeed, and the server must find
   the client's frame **already buffered** at its first `accept`.
2. The same binary with the server spawned and immediately exited: the client's
   write must return `BrokenPipe` and its read `0`, and the test asserts the
   whole thing completed in less wall clock than any plausible timeout — because
   the point is that no timer is involved. A tree that reintroduced a retry would
   fail arm 2 on duration and arm 1 on the "already buffered" assertion.

### 8.3 Least authority — `endowment_denied`

A guest binary spawned twice from one parent, with two different namespaces:

1. Endowed `["echo"]` only: `SYS_NAMESPACE_OPEN(ns, "echo")` succeeds and
   `SYS_NAMESPACE_OPEN(ns, "privileged")` is `NotFound`.
2. Endowed `["echo", "privileged"]`: both succeed.

Arm 2 is what stops arm 1 passing because the service was never there. Plus:
`SYS_DEVICE_CLAIM` with `HANDLE_INVALID` and with a non-`SysCap` handle must be
`PermissionDenied`/`WrongType`, and `SYS_RT_ENTER` likewise — a process that was
not endowed the RT cap cannot enter the RT band, which is the privilege
`specs/issues/isolation/process-isolation-ungated.md` says a claim was never
enough to confer.

### 8.4 No global registry — `cargo test --lib` grep gate

`src/docs.rs`'s neighbours in the same harness: the identifiers `SYS_CONNECT`,
`SYS_LISTEN`, `SYS_PIPE_OPEN`, `SYS_PIPE_ID`, `SYS_SOCKET_CREATE`, `SharedToken`
and `services::connect` appear nowhere in `kernel/`, `toyos/`, `toyos-abi/`,
`userland/` or `tests/` outside the retired-number table and `specs/`. This is
`capability-handles-spec.md` §12.2's stage-F grep gate, moved earlier because the
deletion is the deliverable rather than a follow-up.

### 8.5 The existing suite

Every desktop, audio and net test migrates by config rather than by code: the
eight test `system.toml` files gain their `serves`/`receives`/`devices` rows and
the assertions are unchanged. Three consequences to plan for:

- `desktop_typing_damage`, `desktop_locale_detect`, `desktop_audio_client`,
  `blocked_dump`, `screen_blocked_dump` and `desktop_window_child` are the six
  boots that put `/bin/terminal` in `init` beside the compositor. Their
  `ready_marker` stays `compositor: ready`, but the failure it was hiding cannot
  occur; `shell_echoes`'s `exit: terminal ` diagnosis stays, because a diagnosis
  that can no longer fire costs nothing and is the thing that would catch a
  regression.
- **`EXPECTED_FAILURES` is a trap for a long branch, twice over.**
  `desktop_window_child`'s entry (task 156,
  `specs/issues/kernel/desktop-window-child-freeze.md`) quotes **six exact
  assertion strings**, and `hda_tone`'s quotes one; rewording any of them drops
  the exemption silently and reds the run. Neither entry may be touched,
  reclassified or deleted by this branch. And both carry
  `Stale::OnThisDate("2026-09-06")`, which is **28 days from the day this spec
  was written** — `Day::today()` compares against it at harness startup and the
  run exits 1 by itself once it arrives. A branch still open on that date reds
  for a reason that is not its own. That is a scheduling fact, not a licence to
  move the date; §10.3 puts it in front of the owner.
- **Gate A must not degrade.** soundd's per-period cost is unaffected — the
  change is at stream open, not in the mix loop — and its RT entry moves from
  `set_rt_priority(true)` to `SYS_RT_ENTER(cap)`, one syscall either way, once.
  The fast tier runs on every `cargo test` and is the gate. The thorough tier is
  red on `main` itself (`specs/issues/audio/thorough-tier-reds-on-unmodified-main.md`)
  and cannot be a pass/fail gate, but a same-session A/B against `main` is owed
  before the PR, because "the change is not in the mix loop" is an argument and
  not a measurement.

### 8.6 Object-layer gates

Adopted from `capability-handles-spec.md` §12.4 and required here:
`handle_basic`, `handle_transfer`, `handle_kill_policy`, `kill_while_blocked`,
`device_claim_crash_release`, `process_lifecycle`, and the per-variant `LIVE_*`
census asserted back to baseline after every churn test.

`kill_while_blocked` is the one that matters most for this architecture: an audio
client killed while blocked in its signal-pipe read must make soundd see
`BrokenPipe`, and that is only true because `handle_count` is not the Arc count.

---

## 9. The work breakdown

One agent, this branch, no intermediate landings. Intermediate commits need not
compile. Chunk boundaries marked **green** must build and pass `cargo test`;
the others are explicitly not green and say why.

The commit trailer on every commit:

```
Abi-Inseparable: owner ruled the endowment architecture lands as one pull request (2026-08-09)
```

`--pr` and CI's `abi-split` refuse a branch mixing `toyos-abi/src`/`toyos/src`
with dependent work unless that trailer is present, and this branch necessarily
mixes them. This branch also legitimately claims the shared sysroot — its
`toyos-abi` and `toyos` genuinely differ from main's — and holds it for its whole
life. See §10.

**Merge cadence.** `git fetch && git merge --no-ff origin/main` at the end of
every chunk, and never inside one. A conflict in `kernel/src/arch/syscall.rs` or
`toyos-abi/src/syscall.rs` is expected and is resolved in favour of main's number
allocations: if main retires or adds a syscall while this branch is open, this
branch's 99–112 shift up and §3.1 is corrected rather than the collision being
resolved by picking one.

---

**Chunk 0 — the std path. Green.**
`src/toolchain.rs` writes and removes `rust/.cargo/config.toml` with a `paths`
override so `x build library` reads the building worktree's `toyos-abi`/`toyos`.
A host test proves it: a marker constant in this worktree's `toyos-abi` must
appear in the built `libstd`. Closes
`specs/issues/build/std-change-needs-an-unlanded-abi-change.md`.
*Nothing else in this plan can be verified until this works.*

**Chunk 1 — object infrastructure, zero users. Green.**
`kernel/src/object/{mod,handle,rights}.rs`, `toyos-abi/src/handle.rs`,
`kernel/clippy.toml`'s `disallowed-methods` wall, the per-variant `LIVE_*`
census, the deferred zero-handle queue and its three drain sites. `HandleTable`
lives inside `ProcessData` alongside `FdTable`, empty. `retired_syscalls!` macro
with the four existing gravestones moved into it. Boots; nothing uses any of it.

**Chunk 2 — `Fd` → `Handle`. Green, and this is the big mechanical one.**
`pipe.rs` → `object/pipe.rs` with `PipeShared` and two end types; the remaining
`Descriptor` kinds become objects; `FdTable` deleted and `HandleTable` is the
only table with stdio pre-seeded at slots 0/1/2 generation 0; `fd.rs`'s dispatch
becomes exhaustive matches on `KObjectRef`; `io_uring::Source` keys become
`Koid`s. std's `Fd`→`Handle` and the mio/socket2 fork edits land here.
Bad-handle policy is log-and-error for now.
*Gate: full suite, `handle_basic`, gate A fast tier.*

**Chunk 3 — ports and namespaces. Not green: the registry is gone before
anything uses its replacement.**
`object/port.rs`, `object/namespace.rs`, `SYS_PORT_CREATE`,
`SYS_NAMESPACE_BUILD`, `SYS_NAMESPACE_OPEN`, `SYS_ACCEPT`'s new shape.
`kernel/src/listener.rs` deleted, `SYS_LISTEN`/`SYS_CONNECT` retired.
`toyos/src/{port,namespace}.rs`. Kernel compiles, userland does not.

**Chunk 4 — spawn endowment and `/bin/init`. Green.**
`SpawnArgs`'s two new vectors, `SYS_ENDOWMENTS`, `toyos/src/endow.rs`; the
manifest schema in `src/build.rs` and `/etc/system.manifest`; `/bin/init` with
the port/claim/namespace/spawn loop and the `launcher` service; the
`INIT_PROGRAMS` chain deleted and `KernelArgs` shrunk with its layout asserts
updated; `SysCap` and `SYS_DEVICE_CLAIM`/`SYS_RT_ENTER`; every one of the 11
`system.toml` files rewritten. The §8.1 host gates land with the schema.
*Gate: every config boots; `endowment_denied`; the four `--lib` config gates.*

**Chunk 5 — every server and client. Green.**
compositor, soundd, netd, filepicker, terminal, console, toybox `screen`,
`filepicker-api`, `window`, and the SDK's `audio`/`net`/`surface`. The two retry
loops deleted. `TOYOS_SURFACE` deleted. `PickError`. The six `NoCompositor` sites
become three variants.
*Gate: full suite including all six desktop boots; gate A fast tier; a
same-session A/B of the thorough tier against `main`.*

**Chunk 6 — shm objects and handle transfer. Green.**
`object/shm.rs`, `SYS_SHM_CREATE/MAP/UNMAP`, connection in-flight queues,
`SYS_HANDLE_SEND`/`RECV` and connection readiness. soundd's `MSG_STREAM_OPENED`
and netd's eleven pipe-id fields migrate. `shared_memory.rs`, `SharedToken`,
`SYS_PIPE_OPEN`, `SYS_PIPE_ID`, `SYS_SOCKET_CREATE` and the shm pid-ACL deleted.
`SYS_IO_URING_SETUP` returns its own mapping; the mio `map_shared` line goes.
*Gate: `handle_transfer`, `kill_while_blocked`, gate A both tiers' fast arm,
`device_claim_crash_release`.*

**Chunk 7 — process objects and the fail-fast flip. Green.**
`ProcessObject`/`ThreadObject`; `SYS_SPAWN` returns a handle;
`SYS_PROCESS_WAIT/KILL/OPEN`; `SYS_WAITPID`/`SYS_KILL` retired; the zombie and
orphan machinery deleted; std's `process/toyos.rs`. Bad-handle policy flips to
kill-the-process.
*Gate: `process_lifecycle`, `handle_kill_policy`, census baselines.*

**Chunk 8 — the test estate. Green.**
Every `tests/toyos-rust-tests/src/bin/` binary that used the deleted surface;
`abuse_listener_hijack` and the pipe-id sweeps rewritten to their general forms;
`connect_before_serve` and `endowment_denied` land here if they have not already.

**Chunk 9 — audit, deletion and documentation. Green.**
The grep gate (§8.4); `[u32; 64]` sized from the ABI; dead constants and any
surviving `_ =>` arms in object-layer code; the ten closed `specs/issues/` files
deleted and the six re-scoped ones rewritten; `specs/capability-handles-spec.md`
annotated with what this delivered and what it did not; `forks.toml` deltas;
**one line** in the root `CLAUDE.md` — it has 2,678 bytes of headroom against its
40,000 budget (`wc -c CLAUDE.md` → 37,322), so anything longer displaces
something else and `src/docs.rs`'s budget test will say so. Detail goes in
`userland/CLAUDE.md` and `kernel/CLAUDE.md`, which have 5,471 and 5,387 bytes
spare.
*Gate: full `cargo test`, every host test suite named in the root `CLAUDE.md`,
`cargo test --lib`.*

**Ordering constraints, stated so they are not rediscovered.** Chunk 0 before
everything. Chunk 1 before 2. Chunk 2 before 3 (ports are objects). Chunks 3 and
4 are one non-green stretch and must be committed as such. Chunk 6 may precede 5
in principle but should not: chunk 5's gate is the first place the desktop boots
prove the architecture, and it is worth reaching early. Chunk 7 is independent of
5 and 6 and could move, but it changes std again and batching the two std
touches is worth more than the parallelism.

---

## 10. Owner-level decisions

Everything not listed here is a recommendation the implementing agent follows
without stopping.

### 10.1 The branch holds the shared sysroot for its whole life — **his call**

Its `toyos-abi` and `toyos` genuinely differ from main's from chunk 1 onward, so
it must claim, and while it holds the claim **every other worktree is refused and
none of them can fix that from its end** (`specs/worktrees.md` §3.1–§3.2; the
refusal text is at `src/toolchain.rs:635`). This is not a week-long claim in the
usual sense — it is the whole life of a nine-chunk branch.

Three ways out, and the choice is the owner's:

1. **Accept it.** Other worktrees pause, or work only on trees that do not build
   (specs, `toyos-*` host crates, `tests/common`).
2. **Land chunk 0 + chunk 1 as their own pull request first.** Chunk 1 adds
   `toyos-abi/src/handle.rs` and nothing else uses it, so it is a pure addition
   that can land on `main` in an afternoon. It does not shorten the claim — chunk
   2 still diverges — but it gets the std-path fix onto `main` where every
   worktree benefits. **This is the recommendation.**
3. **Do the work in the primary checkout.** Rejected: the primary owns `main`,
   the rustup link and 50 GiB of `rust/`, and `cargo run -- --sync` moves its
   tree. Naming it only so nobody proposes it later.

### 10.2 Three repositories land together — **his call**

The monorepo PR is not sufficient. `rust` (submodule bump), `mio` and `socket2`
must have their `toyos` branches pushed first, and the monorepo PR carries the
submodule pointer and the two `Cargo.lock` rev bumps. Two things follow that the
owner should decide rather than an agent:

- **`rust/` is one shared working tree.** An uncommitted edit there is every
  worktree's, and `--pr` requires the submodule clean. This branch will hold
  edits in `rust/library/std/src/sys/{process,pal,pipe,stdio,net,fs}/toyos*` for
  the length of chunks 2, 5 and 7. On 2026-08-05 a single uncommitted std patch
  sitting there for about an hour failed three other worktrees' landings
  (`specs/issues/build/std-change-needs-an-unlanded-abi-change.md`). Recommended:
  commit and push the std half to the fork's branch at the end of each chunk that
  touches it, and bump the submodule pointer in the monorepo at the same moment,
  rather than accumulating.
- **Between the fork pushes and the merge, `main` is momentarily inconsistent
  with the fork branch heads** — anyone running `cargo update` on `main` picks up
  a mio that needs an ABI main does not have. The window is real and short. The
  alternative is publishing `toyos-abi` to crates.io and versioning the forks
  against it, which CLAUDE.md already names as the one blocker for upstream PRs
  and is not this branch's job.

### 10.3 Both `EXPECTED_FAILURES` entries expire on 2026-09-06 — **his call**

28 days from the day this spec was written. `Stale::OnThisDate` is checked at
harness startup and exits 1 by itself, so a branch still open on that date goes
red on two exemptions it did not cause and cannot legitimately extend: the rule
is that an entry must be able to fail the build by itself, and moving its date
to make a red go away is exactly what the rule forbids.

Three answers, none of them an agent's: land before it; extend both dates on
their own merits when the day comes, which is a review of #156 and #88 rather
than of this branch; or accept a red tail and read it correctly. **Recommended:
plan chunks 0–5 to land the desktop gate well inside the window, and treat the
date as a real deadline rather than as something to renegotiate.**

### 10.4 Not his call, recorded so he can overrule

Three places where I chose against a literal reading, each argued in §11: the
child is born holding a **connector** rather than a pre-made connection (D1);
`accept` no longer reports the peer's pid (D3); `SYS_THREAD_JOIN` keeps its `Tid`
(D5). Each is a one-line reversal if he disagrees.

---

## 11. Deviations, and why

**D1 — "born holding every connection it may use" is delivered as a connector,
not a connection.** A client opens N connections over its life: the terminal
opens a fresh one per clipboard copy (`window::clipboard_set`,
`userland/window/src/lib.rs:328`), and the compositor treats any first frame that
is not `MSG_CREATE_WINDOW` as a one-shot it answers and closes
(`session.rs:655-659`). Pre-creating connection #1 would leave #2 unaccounted for
and re-open the question the ruling closes. The durable capability is therefore
the connector, and the property the ruling is buying — *there is no instant at
which a name is not bound yet* — is delivered exactly, because the port exists
before either process runs.

**D2 — the namespace is a kernel object, not a userland map.** A flat labelled
vector of connectors would deliver the same authority set with less machinery,
and I built the design that way first. Item 7's wording settles it: libc holds
*the process's namespace handle*, singular. A single handle is also what makes
"hand a child a narrowed view" one syscall instead of N, and what leaves room for
a namespace to be served by a userland directory later without an ABI change.

**D3 — `accept` returns a handle and no pid.** `capability-handles-spec.md`
leaves the pid in place. Nothing authorizes on it once `SYS_PIPE_OPEN` is retired,
and a peer's identity asserted by the kernel is exactly the designation-as-
capability shape this work deletes. A server that wants to name its client reads
the protocol's first frame, which is already the client's own claim about itself
and already distrusted.

**D4 — device claims are minted only by init, and `SYS_OPEN_DEVICE` is retired.**
`capability-handles-spec.md` §14.11 scopes spawn-time device grants out of v1 and
keeps first-come `SYS_OPEN_DEVICE`. That reservation was made to keep its stage E
small; this deliverable has no such constraint, and leaving arbitration
first-come would leave the ruling's eighth point false — a process could still
take the keyboard by starting early. The manifest is the arbitration now, and
`every_device_class_has_at_most_one_claimant` checks it before the image is
built.

**D5 — `SYS_THREAD_JOIN` keeps its `Tid`.** §3.4.

**D6 — the filesystem is untouched, so item 8's doom example is only two thirds
true.** doom ends up holding `compositor`, `soundd` and stdio, and it *cannot*
express reaching netd, the filepicker, a device or a process it was not given.
It can still `SYS_OPEN("/etc/passwd")`, because paths are ambient and per-process
directory namespaces are the follow-on stage (§12). The least-authority table in
§8.3's gate asserts what is true and does not assert what is not.

**D7 — `SYS_DEBUG` and `SYS_SHUTDOWN` stay ungated.** Both want a `SysCap` right
and this design supplies the mechanism, but gating `SYS_DEBUG` means threading a
`SysCap` through `/bin/test-runner` into seven test binaries and their children
(`test_panic_child`, `heap_ceiling`, `abuse_kernel_addr`, `tlb_shootdown_waits`,
`test_screen_graffiti` and two more). That is a change to the test estate's
authority model made in passing, inside a branch that already rewrites it, and it
would be indistinguishable in the diff from the work it rode in on.
`specs/issues/isolation/sys-debug-ungated.md` stays open and gets one sentence
saying the mechanism now exists.

---

## 12. Out of scope, named

**The filesystem stage** — per-process directory handles replacing ambient paths.
It is the natural next stage and this design deliberately does not block it: a
`Directory` object is one more `KObjectRef` variant, `openat`-shaped syscalls take
a directory handle, and the manifest grows a `directories` key beside `devices`.
Nothing here needs to change for that. It lives substantially in
`rust/library/std/src/sys/fs/toyos.rs` (552 lines) and is a separate landing.

**Namespace enumeration** (`SYS_NAMESPACE_LIST`) — wanted for `ps`-style
introspection, not needed by any gate here: §8.3 asserts a refusal, which is
stronger than a listing.

**Lazy service start** — a namespace entry that starts its server on first
connect. The launcher makes it expressible later; nothing needs it now.

**Revocation** — `capability-handles-spec.md` §14.5 rejects unmap-others by name
and this design keeps that. Forced reclaim of a wedged-alive daemon is killing
it, which init can do because it holds the `Process` handle.

**`SYS_SYSINFO`/`SYS_SCHED_INFO`** — read-only, ambient, and a read surface is
`specs/introspection-plan.md`'s subject rather than this one's.
