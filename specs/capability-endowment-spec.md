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

**Reviewed 2026-08-09**, against the same tree — `origin/main`'s tree is
byte-identical to this branch's base (`git diff --name-only 19c761e origin/main`
is empty), so every figure below is a figure about `main` too. The review
corrected six counts, added one ABI row the design does not work without
(§3.5's error word), split `serves` from `provides` (§2.2) because `surface`
could not be both, gave the launcher message the four fields it was missing
(§4.5), added three migrations the ledger did not carry (`userland/libc` §6.5a,
the compositor's shared memory §6.3a, the test estate's authority §6.7a), moved
`SYS_ACCEPT`'s pid removal from chunk 3 to chunk 6 so chunk 5 can be green, and
replaced §5.3's claim that a daemon crash is recoverable with what actually
happens. §13 is new and is the coordination with the two other open plans.

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
  `serves` name **before any server runs**, builds each program's namespace out
  of connectors, and spawns it holding them. A `provides` name is the other kind
  — one port per *instance*, made by the program itself and handed down to its
  own children — and `surface` is the whole of that kind today (§2.2).
- A connection therefore works from the client's first instruction, whether or
  not the server has reached `accept` or has even been spawned yet. **There is
  no instant at which a name is not bound yet**, so there is nothing to retry
  and no timeout anywhere.
- If a server exits without serving, its `Acceptor`'s last handle goes, the
  queued connections' pipe ends drop, and the client's next write is
  `SyscallError::Gone` (§3.5). The bound on failure is a process lifetime and
  nothing else.

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
*handle* release obligation on a blocked thread's stack.** `Acceptor`,
`Connector`, `Namespace` and `ConnectionEnd` all release through the table drain.

That is the whole of the claim and it is narrower than it looks, so state the
residual. A thread blocked in `SYS_ACCEPT` is registered on `PortShared.acceptors`
and that registration *is* on its own stack — the identical shape to
`Registration`, and it leaks identically when another CPU kills the thread. It is
not new: it is the existing wait-queue leak this kernel already has on every
blocking syscall, `Acceptor` simply inherits it by being one more thing to block
on. Nothing here fixes it and nothing here should; `wt/toyos-compl`'s cancellable
park is the fix and §13 records that it lands after this branch.

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
queued client observe `Gone`/EOF. `Connector::on_zero_handles` does
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
    Console(Arc<ConsoleObject>),
}
```

Fourteen variants — the thirteenth was thirteen until §1.5 found that
`Descriptor::SerialConsole` has no home among them. `ListenerObject` from that spec is replaced by the
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

### 1.5 Every `Descriptor` kind, and the object it becomes

Written out because §1.2 and §1.3 between them do not cover the table this
replaces, and two of its fifteen kinds have no home in the variant set above.
Added 2026-08-09 by the implementing agent, against `kernel/src/fd.rs:68-103`.

| `Descriptor` kind | becomes |
|---|---|
| `File(OpenFile)` | `File` |
| `PipeRead`, `PipeWrite` | `PipeRead` / `PipeWrite` |
| `TtyRead`, `TtyWrite` | the same two, carrying a mark — below |
| `Socket { rx, tx, peer }` | `Connection` |
| `Keyboard`, `Mouse`, `Framebuffer`, `Nic`, `Hda`, `VirtioSound` | `Device`, one class each |
| `Listener(ListenerRef)` | transitional in chunk 2; `Acceptor`/`Connector` in chunk 3 |
| `IoUring(RingRef)` | `IoUring` |
| `SerialConsole` | **`Console`, a fourteenth variant** — below |

**`SerialConsole` needs its own variant and the thirteen do not contain one.**
It is what `spawn_kernel` puts in `/bin/init`'s three stdio slots, so it
survives everything this branch does. It is not a `DeviceClaim`: a claim's whole
content is exclusivity, and every kernel-spawned process holds one of these at
once. It is not a `File`: it has no path, no cursor and no backing. So
`Console(Arc<ConsoleObject>)` is the fourteenth variant, `READ|WRITE|DUP|
TRANSFER|WAIT`, and there is exactly one of them for the machine.

**A tty mark is per *end*, not per pipe.** `SYS_MARK_TTY` converts one
descriptor today (`PipeRead` → `TtyRead`), and `duplicate` carries the mark, so
an `AtomicBool` on `PipeReadEnd`/`PipeWriteEnd` is the faithful mapping and a
flag on the shared `PipeShared` is not. Its one caller marks both ends anyway
(`rust/library/std/src/sys/process/toyos.rs:212-213`), so the two designs are
indistinguishable in practice — which is the reason to pick the one that says
what it means rather than the one that happens to work. `FileType::Tty` is then
read off the end rather than off a variant, and `mark_tty`'s `table.update`
disappears with the two variants it existed to swap between.

**`Hda` and `VirtioSound` carry `info_read: bool` per descriptor**; it moves
onto the claim, which is sound only because a claim admits one handle. That is
`DeviceClaim`'s no-`DUP` rule doing a second job, and it is worth noticing
rather than relying on silently.

### 1.6 Why a packed pair of handles cannot be read as an error

`SYS_PORT_CREATE` answers `(acceptor << 32) | connector` (§3.1), and
`SyscallError` encodes as `u64::MAX - code` for `code < 256` — the top 256
values of the range. A packed pair could therefore be read as an error if both
handles could reach `0xFFFF_FFFF`.

They cannot, and the reason is [`RawHandle`]'s slot retirement: a slot at
`MAX_GENERATION` is retired rather than reissued, so the largest handle any
table hands out is `0xFFFF_EFFF` and the largest pair is
`0xFFFF_EFFF_FFFF_EFFF`, four billion below the error range. **The retirement
rule and the packing are therefore load-bearing for each other**, which neither
of them says, and a future handle encoding that wraps generations instead would
make `SYS_PORT_CREATE` occasionally return `Unknown`.

[`RawHandle`]: ../toyos-abi/src/handle.rs

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
`kernel/terminal-races-compositor-at-boot`.

### 2.2 The new shape

```toml
# Every program the image carries. `path` and `no-default-features` are
# unchanged.
[programs.compositor]
serves   = ["compositor"]
devices  = ["framebuffer", "keyboard", "mouse"]
receives = ["soundd", "filepicker"]

[programs.soundd]
serves   = ["soundd"]
devices  = ["hda-audio", "virtio-sound"]
realtime = true

[programs.netd]
serves   = ["netd"]
devices  = ["nic"]

[programs.terminal]
provides = ["surface"]        # one port per terminal, made by the terminal
receives = ["compositor", "launcher"]

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

Five new per-program keys, all `#[serde(default)]`:

| key | type | meaning |
|---|---|---|
| `serves` | `Vec<String>` | init creates **one machine-wide port** per name and endows the **acceptor** |
| `provides` | `Vec<String>` | names this program creates a port for **itself**, once per instance, and puts in the namespaces of the children it spawns. init creates nothing and holds nothing |
| `receives` | `Vec<String>` | names in this program's namespace, each a **connector** |
| `devices` | `Vec<String>` | device classes init mints a claim for and endows |
| `realtime` | `bool` | init endows an `RT`-only `SysCap` dup |

**`realtime = true` on soundd and on nothing else.** `rg -n 'set_rt_priority'`
outside `toyos-abi` is one line — `userland/soundd/src/main.rs:939` — so soundd
is the whole of the RT band today. Endowing the compositor an `RT` cap it never
calls would be authority granted for a plan rather than for a caller, which is
the defect this branch exists to delete; putting the compositor in the RT band is
a scheduling decision with its own gate A argument, and it is one manifest line
away whenever that argument is made.

**`serves` and `provides` are two different things and conflating them breaks
`surface`.** A `serves` name is one port for the whole machine: there is exactly
one soundd and every client's `soundd` connector points at the same
`PortShared`. `surface` is not that — every terminal owns one, `/bin/console`
owns one, and a shell must reach *its own host's* and no other's. init cannot
create it: init does not know how many terminals there will be, and a single
machine-wide `surface` port would let any process holding the connector talk to
whichever terminal happened to take the acceptor. So `provides` is declared, and
what it declares is that the program calls `SYS_PORT_CREATE` itself and hands the
connector down (§4.4). This is what makes `surface` nameable in a `receives` list
at all — without the distinction, a gate that knew only `serves` would
force `terminal` to `serves = ["surface"]` and §4.4's per-instance port would be
a lie the manifest tells.

`init: Vec<String>` becomes `[boot] start: Vec<String>` of **program names**, not
paths — a path in a boot list is a second spelling of a key the same file
already has, and it is what let `diag/system.toml` smuggle an argument through
(`"/bin/toybox pwd"`). Arguments move to `args = ["pwd"]` on the program entry.

### 2.3 The three variants and the eight test configs

| config | `[boot] start` | notes |
|---|---|---|
| `system.toml` | `compositor`, `soundd`, `netd` | unchanged set |
| `diag/system.toml` | `toybox` with `args = ["pwd"]` | no `serves`, no `receives`, **no `devices`** — that is what "nothing in this image can claim the framebuffer" now means, and it is checkable |
| `console/system.toml` | `console` | `devices = ["framebuffer", "keyboard", "mouse"]`, `provides = ["surface"]` |
| `tests/desktopcase` | `compositor`, `terminal` | `terminal` gains `receives = ["compositor"]` — the race becomes unrepresentable here first |
| `tests/desktopaudiocase` | `compositor`, `soundd`, `terminal` | + `receives = ["soundd"]` on terminal's children |
| `tests/doomcase`, `tests/doommusiccase` | `soundd`, `test-runner` | `doom` gains `receives = ["soundd"]` |
| `tests/metalcase` | `compositor`, `soundd`, `netd`, `sshd`, `test-runner` | |
| `tests/netcase` | `netd`, `test-runner` | |
| `tests/sshdcase` | `netd`, `sshd`, `test-runner` | |
| `tests/testcases` | `soundd`, `test-runner` | |

Every row is that config's `init` list today, unchanged in membership
(`system.toml` 3, `diag` 1, `console` 1, `desktopcase` 2, `desktopaudiocase` 3,
`doomcase` 2, `doommusiccase` 2, `metalcase` 5, `netcase` 2, `sshdcase` 3,
`testcases` 2 — 26 entries across 11 files).

**`diag` keeps its guarantee, and what it keeps is a different guarantee.** The
diagnostic image today contains nothing that *can* claim the framebuffer because
of what is in its `[programs]` — a property of the binaries in the image. After
this, `/bin/init` is in every image and holds `Rights::DEVICE`, so the property
becomes "the config this image was built from declares no `devices`", refused at
build time by §8.1's `no_diag_program_claims_the_screen`. The reachable set is
the same and it is checkable for the first time; it is not *strictly stronger*,
because a bug in `/bin/init` could reach a device where previously no code in
the image could. That is the trade and it is worth making — but "strictly
stronger" would be an overclaim, and the honest version is the one that survives
a reader checking it.

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

**And for every syscall that keeps its number** (§3.3), because a right left
unstated is a right each call site invents — which is the "policy at the call
site" defect this work exists to delete. There is no default:

| call | needs on its handle |
|---|---|
| `SYS_READ`, `SYS_READ_NONBLOCK`, `SYS_SEEK` | `Rights::READ` |
| `SYS_WRITE`, `SYS_WRITE_NONBLOCK`, `SYS_FSYNC`, `SYS_FTRUNCATE` | `Rights::WRITE` |
| `SYS_CLOSE` | nothing — dropping a handle is not an operation on the object |
| `SYS_FSTAT` | nothing — corrected below |
| `SYS_OPEN` | — (produces a handle; the mount's `UserAccess` is the gate, unchanged) |
| `SYS_MARK_TTY` | nothing — corrected below |
| `SYS_PIPE_MAP` | `Rights::MAP` |
| `SYS_ACCEPT` | `Rights::READ` on the `Acceptor` — accepting is reading the queue, and a `WAIT`-only handle is the io_uring watch |
| `SYS_IO_URING_SETUP` | — (produces a handle) |
| `SYS_IO_URING_ENTER` | `Rights::READ\|WRITE` on the ring; `Rights::WAIT` on every handle a `POLL_ADD` names |
| `SYS_HANDLE_DUP`, `SYS_HANDLE_DUP_AT` | `Rights::DUP`, and the requested set must be a subset of the source's |
| `SYS_PROCESS_STATS` | `Rights::READ` on the `Process` |
| the nine device calls (§3.3) | `Rights::WRITE` on the `DeviceClaim` for the seven that write; `Rights::READ` for `SYS_NIC_RX_POLL` and `SYS_DEVICE_REG_READ` |

**Two rows above said `READ` and `WRITE` and were wrong against the tree.**
Corrected 2026-08-09 by the implementing agent, in chunk 2, with the call sites
that disprove them:

- **`SYS_FSTAT` requires nothing.** It answers what kind of thing a handle names
  and how big it is; it moves no content. `userland/libc/src/stdio.rs:174`
  `fstat`s slots 1 and 2 to decide line buffering and `posix_io.rs:264`'s
  `isatty` does the same, and both are write ends with no `READ`. A right that
  refuses `isatty(1)` is not a right.
- **`SYS_MARK_TTY` requires nothing.** Its one caller
  (`rust/library/std/src/sys/process/toyos.rs:212-213`) marks *both* ends of a
  pair, and neither end carries the other's right, so any single right refuses
  one of the two calls. The mark is a statement about the end by whoever created
  the pipe, not an operation on what flows through it.

Both are the same shape as `SYS_CLOSE`'s row, which the table already had.

### 3.2 Retired — 12 numbers, never reused

**Twelve and not the thirteen this heading said until 2026-08-10.** The
thirteenth was `SYS_SOCKET_CREATE`(76), and chunk 6 found it was renamed rather
than retired — §3.3's list below carries it now, and the table here has twelve
rows and always did.

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
| 85 | `SYS_LISTEN` | there is no global name registry to register in |
| 87 | `SYS_CONNECT` | there is no global name registry to look up in |
| 96 | `SYS_SET_RT_PRIORITY` | gated on the audio claim, and a claim is not a privilege — the dispatch's own comment says so (`kernel/src/arch/syscall.rs:641-657`: *"`SYS_OPEN_DEVICE` is first-come and ungated, so whoever wins the race gets the RT band with it… a claim is not one"*). `SYS_RT_ENTER` is the privilege that comment asks for |

The dispatch grows thirteen gravestone arms of the shape already at
`kernel/src/arch/syscall.rs:305` and `:307`. That is the second such pair; a
third would be a table, so **`retired_syscalls!` is a macro taking `number =>
"formerly SYS_NAME"` rows**, and the four existing entries (29, 30, 32, 33) move
into it in the same commit.

### 3.3 Renamed in place — same number, argument is now a `RawHandle`

`SYS_OPEN`(9), `SYS_WRITE`(0), `SYS_READ`(1), `SYS_CLOSE`(10), `SYS_SEEK`(13),
`SYS_FSTAT`(14), `SYS_FSYNC`(15), `SYS_MARK_TTY`(28), `SYS_FTRUNCATE`(60),
`SYS_READ_NONBLOCK`(66), `SYS_WRITE_NONBLOCK`(67), `SYS_PIPE_MAP`(77),
`SYS_IO_URING_ENTER`(90). No source change in any of them beyond the type.

**And `SYS_SOCKET_CREATE`(76) → `SYS_CONNECTION_JOIN`(76)**, which §6.4 and
chunk 6 argue at length and this list omitted until 2026-08-10 — the one place a
reader looking for the ABI delta would go. Its two arguments were pipe *ids* and
are handles; the operation is unchanged and grants nothing, because everything it
reaches is already the caller's.

`SYS_OPEN` is the one that *produces* a handle rather than consuming one
(`toyos-abi/src/syscall.rs:479` returns `Fd`), and it is here because every other
row on this list is unreachable without it: leaving it out would leave `open`
handing back an `Fd` that `read` no longer accepts. Nothing else in the ABI
traffics in `Fd` — `rg -n '\bFd\b' toyos-abi/src` is 44 lines and every one of
them is on this list, on the retired list, in one of the six shapes below, or is
the type's own definition (`toyos-abi/src/lib.rs:17`) and the `use` that imports
it.
`SYS_DLOPEN`(55)/`SYS_DLSYM`(56)/`SYS_DLCLOSE`(57) carry a **module id**, not a
handle: it names nothing outside its own process, exactly as `Tid` does (D5), and
it stays.

Six change shape as well:

- **`SYS_PIPE`(24)** returns two handles rather than two `Fd`s, and gains a
  `Result` — `sys_pipe` already answers `ResourceExhausted` on three paths and
  the wrapper splits the error word into `Fd(-1)`/`Fd(-8)`
  (`isolation/abi-wrappers-return-error-as-value`). That issue's
  fork edit (mio's waker) is in this branch's fork budget anyway, so it is fixed
  here.
- **`SYS_SPAWN`(25)** returns a `Process` handle; `SpawnArgs` grows (§4.2).
- **`SYS_DUP`(50)** → `SYS_HANDLE_DUP(h, rights)`; rights must be a subset.
  **`SYS_DUP2`(74)** → `SYS_HANDLE_DUP_AT(h, slot, rights)`. Both keep their
  numbers: this is the same operation with the rights argument the capability
  model requires, not a different one.

  **`slot` is a `u16` and not a `RawHandle`, and the answer is not the number
  that went in.** A handle carries a generation the caller has no business
  choosing, and the handle this returns carries the slot's own — so
  `dup2(x, 1)` answers `1` only while slot 1 has never been closed. That breaks
  POSIX's "returns `newfd`" for a caller that closes first, which
  `userland/libc` says out loud at its `dup2` and nothing in the tree does:
  `grep -rn dup2` over `tests/testcases/` is empty and the only in-tree callers
  are libc's shim and two abuse tests.
- **`SYS_ACCEPT`(86)** takes an `Acceptor` handle and returns **one** handle.
  Today it packs `(client_pid << 32) | fd`. The pid goes: peer identity is not
  the kernel's to assert, and a server that wants to name its client reads it out
  of the protocol's first frame, where it is already a client's own claim about
  itself. `services::AcceptResult` collapses to a `Connection`.

  **Two callers authorize on that pid today and both must go first.** The
  compositor grants the client's window buffer with `shm.grant(pid)`
  (`userland/compositor/src/session.rs:859`) and soundd grants the stream ring
  the same way (`userland/soundd/src/main.rs:627`), both using exactly the pid
  `accept` returned. So **the pid survives until chunk 6**, and is deleted in the
  same commit that replaces both grants with `SYS_HANDLE_SEND` — not in chunk 3
  where the rest of `SYS_ACCEPT`'s shape changes. Chunk 5's green gate is what
  this buys: with the pid gone at chunk 3 and shm handles arriving at chunk 6,
  the compositor and soundd would have nothing to grant to for two chunks.

  **It is also load-bearing for diagnostics, and that half is a deletion.** 44
  lines of `userland/compositor/src/session.rs`, 25 of `toyos/src/surface.rs`, 11
  of netd's and 7 of soundd's name a pid (`rg -c pid`). Two consequences that are
  not free: `toyos::surface::Notice::{Grabbed,Released,Dropped}` carry a `pid`
  field in the SDK's **public** API and `locale_gate` prints all three
  (`tests/toyos-rust-tests/src/bin/locale_gate.rs:97-99`), and
  `tests/toyos.rs:9074` asserts on the literals `"netd: dropping pid"` and
  `"netd: refusing pid"`. A server can still name a peer, and **what it ended up naming one by is the
  connection's own `RawHandle` rather than a `Koid`**: none of §3.1's fourteen
  numbers answers a koid, and adding a fifteenth was not chunk 6's to decide. A
  handle carries a generation and a closed slot is reissued at the next one, so a
  handle value names one object for the life of the process holding it and
  designates nothing in any other table — the same property, with no ABI. That is
  `toyos::surface::ClientId`; the two netd literals become
  `"netd: dropping client"` / `"netd: refusing client"`, and `tests/toyos.rs:9074`
  changes with them. The koid stays a named option: one number and one arm.
- **`SYS_IO_URING_SETUP`(89)** takes an out-pointer and writes
  `{ handle: RawHandle, vaddr: u64 }`. Today it returns `(Fd, shm_token)` packed
  in a u64 and the caller maps the token — which is the whole of
  `design-debt/io-uring-abuses-shared-memory`. The ring owns its
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

### 3.5 One new error word, and why the design does not work without it

```rust
// toyos-abi/src/syscall.rs — SyscallError gains one variant
Gone = 10,   // the object was there and its other end is not
```

`SyscallError` has ten variants today and **`BrokenPipe` is not one of them**: it
is a kernel-internal `pipe::PipeWrite` variant that `kernel/src/fd.rs:571` and
`:579` answer as `SyscallError::NotFound` — the same word "there is no such
handle" uses. Two things in this design are false without a separate word:

- **§4.2's whole promise.** `Err(NotEndowed)` — "the name is not in the namespace
  this process was given, a fact about this process" — and `Err(ServerGone)` —
  "the server exited" — are two different facts, and §5.1 answers both from
  `SYS_NAMESPACE_OPEN`. The SDK sees one `u64`. "Distinguished in the SDK because
  the kernel answers them from different objects" is not a mechanism: the SDK
  cannot see which object answered. So a name absent from the namespace is
  `NotFound` and a name whose port is `closed` is `Gone`, and it is the kernel
  that tells them apart because only the kernel can.
- **Every dead-peer answer in §5.1 and §5.3.** A write to a connection whose peer has
  gone must not be indistinguishable from a write to a handle that was never
  there — that is the storage rule ("a failed read must not be indistinguishable
  from data") one layer up, and the client's answer to it is "retry the name"
  versus "you have a bug".

Callers: `kernel/src/fd.rs:571,579` change word; `SYS_NAMESPACE_OPEN` answers
`Gone` on `closed`; `SYS_HANDLE_SEND` answers `Gone` on a dead connection. The
std fork maps it to `io::ErrorKind::BrokenPipe` and `toyos::EndowError::ServerGone`
carries it. `SyscallError::from_u64`'s match gains one arm. The two `fd.rs` sites
are the only producers of the word being changed, and the five places userland
matches `SyscallError::NotFound` (`rg -n 'SyscallError::NotFound' toyos/src
userland` → `netd:1200`, `libc/posix_io.rs:41`, `soundd:797`, `soundd:1785`,
`soundd:1796`) are each about a device or a path and none is a pipe write, so
this is additive for every one of them.

### 3.6 Struct layouts

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
SDK parses it once. **The labels are kernel state and the only new per-process
allocation this design adds**: `ProcessData` carries a `Box<[u8]>` of at most
`MAX_LABELS_LEN` plus a `Box<[EndowEntry]>` of at most `MAX_ENDOWMENTS`, written
once by spawn and freed with the process. They are *names*, not authority — the
handles they label are in the table whether or not anybody ever calls
`SYS_ENDOWMENTS`.

**Rights on an endowed handle are the parent's, and narrowing is the parent's
job.** `EndowEntry` has no rights field: a move carries the source handle's set
unchanged, because a rights argument on a move would be a second place to shrink
rights and the first one already exists. A parent that wants to hand over less
calls `SYS_HANDLE_DUP(h, narrower)` and endows the dup — which is what init does
for every connector, so that a child holding a `Namespace` cannot re-transfer the
connectors inside it unless init said it could.

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

### 3.7 Bounds

Every one is a `MAX_*` on the primitive, refuses by name, and never truncates.

| constant | value | a function of |
|---|---|---|
| `MAX_ENDOWMENTS` | 32 | the widest manifest row plus stdio; the compositor's is 6 |
| `MAX_NAMESPACE_ENTRIES` | 64 | five production names today; room for a decade |
| `MAX_SERVICE_NAME` | 64 bytes | `surface` is 7; the longest test name is 28 |
| `MAX_PROGRAM_NAME` | 32 bytes | a `[programs]` key; the longest today is `test-runner` at 11 |
| `MAX_LAUNCH_EXTRAS` | 5 | connectors a caller may transfer with one `MSG_LAUNCH` (§4.5). Not 8: the batch also carries the three stdio handles and `MAX_TRANSFER_HANDLES` is 8, and splitting a launch across two batches would let a child's authority arrive in pieces |
| `MAX_LABELS_LEN` | 4096 bytes | `MAX_ENDOWMENTS` × (`MAX_SERVICE_NAME` + the longest `serve:`/`dev:` prefix) is 2,304; the rest is slack |
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
                                                  for each `serves` name, anywhere in the manifest
                                                  (never a `provides` name — §2.2):
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
`design-debt/pick-file-cannot-say-why-it-failed`'s reachable
cause.

**But an acceptor is endowed by move, so a `serves` program can be launched
exactly once per boot.** After init hands the filepicker its acceptor, init holds
nothing for that name; if the picker exits, its port is `closed` and a second
`MSG_LAUNCH` for it has no acceptor to give. The filepicker's own loop never
exits (`userland/filepicker/src/main.rs:463-490` accepts forever), so this is not
reachable today — but a *crashed* one is not restartable and neither is any other
`serves` program. This is the same fact §5.3 states from the client's side, and
the same `SYS_PORT_REARM` (§12) closes both halves. Until it exists, init's
answer to a second launch of a `serves` program whose acceptor is gone is a named
refusal, never a spawn with a missing endowment.

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
MSG_LAUNCH {
    program:   name,                  // a [programs] key, <= MAX_PROGRAM_NAME
    argv, env, cwd,                   // bytes, exactly what SYS_SPAWN takes today
    slot_count, extra_count,          // how many of the handles below are which
}
+ SYS_HANDLE_SEND [ slot handles…, extra connectors… ]
->  MSG_LAUNCHED { }  +  SYS_HANDLE_SEND [ one Process handle ]
```

init looks the program up in the manifest it already holds, builds the
endowments that program declares, spawns it, and sends the caller a `Process`
handle over `SYS_HANDLE_SEND`. The asker's capability is "may ask init to start a
declared program" — one connector — and the *policy* is the manifest.

**A launch carries four things the first draft of this message did not, and each
is load-bearing.**

- **`env` and `cwd`.** A child started by the launcher would otherwise inherit
  *init's* environment and working directory, so `sh -c 'cd /tmp && ls'` would
  list `/`. The launcher is a spawn service, not a session.
- **The stdio handles.** `Command::stdin/stdout/stderr` is how every shell child
  and every sshd session gets its pipes (§4.7), and `slot_map` is a `SYS_SPAWN`
  argument the caller no longer makes. They travel as transferred handles and
  init installs them at slots 0/1/2.
- **`extra_count` connectors the caller transfers.** This is what makes the
  terminal→shell→`locale` chain work: `surface` is a `provides` name (§2.2), init
  has no connector for one, and `toybox`'s `receives` row names it. The caller
  supplies what it holds; init supplies what the manifest declares; the child's
  namespace is the union. **This adds no authority** — a caller can only transfer
  a handle it already has, which is the same bound `SYS_SPAWN` has — and it keeps
  §4.6's rows exact, because a caller that transfers nothing gets exactly the
  manifest row.
- **A bound.** `MAX_LAUNCH_EXTRAS = 5`, refused by name, for the same reason
  every other `MAX_*` here exists — five and not eight because the same batch
  carries stdio and a launch is one crossing (§3.7).

**init keeps no `Process` handle from a launch.** The send is a *move*: init's
handle count for that object goes to zero and the caller's to one. Otherwise any
process holding the `launcher` connector exhausts init's handle table by
launching `/bin/true` in a loop, and the machine loses the ability to start
anything — the one process whose table cap is a machine-wide resource. init holds
`Process` handles only for what `[boot] start` names, which the manifest bounds.

**The routing rule, in three clauses, stated so it cannot be guessed.**
`toyos::process::Command` and std's `Command` (`sys/process/toyos.rs`):

1. **A caller that endowed anything itself uses `SYS_SPAWN`.** `Command::endow(..)`
   is a statement that this caller has decided the child's authority, and the
   launcher would overwrite that decision with a manifest row. §4.4's terminal is
   this case: it must give its shell a `surface` connector the manifest cannot
   name, so it spawns directly.
2. **Otherwise, a caller holding a `launcher` connector asks the launcher**, and
   init answers `MSG_NOT_DECLARED` for a name that is not a `[programs]` key. The
   SDK then falls back to `SYS_SPAWN`. **The SDK never parses the manifest** —
   init holds it and answers about it, so there is not a second reader of that
   file in every process that can disagree with the first. The cost is one round
   trip for a program that is not declared, which is `test-runner` spawning a
   test binary and nothing on a user's path.
3. **A caller with no `launcher` connector uses `SYS_SPAWN`** and gets exactly
   inheritance, which is what a program endowed nothing should get.

`sh -c` and `Command::new("/bin/doom")` therefore behave identically, and both
give doom the manifest's `compositor` + `soundd` where the shell has neither.

**Two things the launcher is not, and both are the price.** It is a
**serialisation point** — every `ls` from a shell is now a round trip through one
single-threaded accept loop, and `SYS_SPAWN`'s ELF load and symbol-table read
happen on init's thread rather than the caller's. And it is a **single point of
failure**: init wedged means no process can be created, where today each caller
spawns for itself. Neither is on a measured hot path — nothing in gate A or the
desktop tests spawns in a loop, and the C corpus's 110 spawns come from
`test-runner`, whose test binaries are not `[programs]` keys and take the direct
path (§6.7a) — but both are real and neither is bought back later by anything in
this design.

### 4.6 What each shipped program ends up holding

The ruling's eighth point, enumerated. Every row is exactly the program's
manifest entry; nothing else is reachable, because there is no other place to
ask. `stdio` is slots 0/1/2 in every row and is omitted.

| program | acceptors | connectors (its namespace) | devices | syscap | process handles |
|---|---|---|---|---|---|
| `init` | `launcher` | — | — | `DEVICE\|RT\|MANAGE` | the `[boot] start` programs, and nothing a launch produced |
| `compositor` | `compositor` | `soundd`, `filepicker`, `launcher` | framebuffer, keyboard, mouse | — | the terminals and pickers it launched |
| `soundd` | `soundd` | — | hda-audio and/or virtio-sound | `RT` | — |
| `netd` | `netd` | — | nic | — | — |
| `sshd` | — | `netd`, `launcher` | — | — | its session shells |
| `filepicker` | `filepicker` | `compositor` | — | — | — |
| `terminal` | `surface` (`provides`) | `compositor`, `launcher` | — | — | its shell |
| `console` | `surface` (`provides`) | `launcher` | framebuffer, keyboard, mouse | — | its shell |
| `shell` | — | `surface`, `launcher` | — | — | its children |
| `doom` | — | `compositor`, `soundd` | — | — | — |
| `snake`, `editor`, `paint`, `files` | — | `compositor`, `filepicker` | — | — | — |
| `toybox` | — | `compositor`, `soundd`, `surface` | — | — | — |
| `test-runner` | — | the union its guest binaries need (§6.7a) | — | `DEVICE\|DUP` in `tests/testcases`, none elsewhere | every test binary it spawned |
| `toyos-ld`, `toyos-cc`, `proctest`, `input-test` | — | — | — | — | own children |

`toybox`'s `surface` and `shell`'s `surface` are the `provides` case: they arrive
as a launch extra from whoever spawned them (§4.5), never from init, and a
`toybox` started by `/bin/init` at boot — which is what `diag/system.toml` does —
has no `surface` in its namespace at all. `locale` run there says so instead of
reading an environment variable that was never set, which is the same answer it
gives today and a better-typed one.

Read across: **doom cannot express reaching netd, the filepicker, a device, a
daemon or a process it was not given** — those names are not in its namespace and
its handle table has nothing else in it. sshd, the one program reachable from the
network, holds `netd` and `launcher` and cannot acquire a compositor connection at
all. `toyos-cc` and `toyos-ld` hold nothing but stdio.

Three honest limits, two of them visible in the table:

- **`toybox` is one binary with many applets, so its row is the union of what
  every applet needs**: `screen` wants the compositor
  (`userland/toybox/src/screen.rs:4`), `tone` wants soundd through cpal, `locale`
  wants the surface. `ls` therefore holds a compositor connector it will never
  use. Splitting applets into binaries is the fix and it is not this branch's;
  the granularity of least authority is the granularity of the binary.
- **Every row can still open any path.** The filesystem is ambient (D6, §12).
- **"Nothing else is reachable" is true of a program init started and not of one
  it did not.** §4.5 clause 3 is inheritance: a caller holding no `launcher`
  connector spawns directly and its child gets the *parent's* namespace, not its
  own row. `toybox` holds `compositor`, `soundd` and `surface` and no
  `launcher`, so anything toybox spawns holds all three whatever its row says.
  Nothing in the tree reaches it — toybox spawns nothing, and every program that
  does spawn holds a `launcher` — but the sentence above the table is stronger
  than the mechanism. Added 2026-08-10 by the adversarial review.

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
| exited without accepting | `Gone` (`closed`) | `Gone` on the handle already held | `0` (EOF) |
| exited after accepting | `Gone` | `Gone` | `0` |
| never in this namespace | `NotFound`, and it is `NotEndowed` in the SDK | — | — |

**Two facts, two words** — §3.5. A name the namespace does not carry is
`NotFound` and a name whose port has closed is `Gone`, and the SDK maps them to
`EndowError::NotEndowed` and `EndowError::ServerGone`. They cannot be told apart
by "the kernel answers them from different objects": the SDK sees one `u64` and
nothing else, so if the kernel returns one word the SDK has one answer.

**A client already blocked in `read` when the server dies** is not a row in that
table and is the case a reader will look for. The server's `ConnectionEnd` handle
drops in `teardown_resources`, the `PipeShared` writer count falls to zero, and
the pipe's reader wait queue is woken with `0`. That wake is posted from the
zero-handle hook, which runs off the deferred per-CPU queue drained at syscall
exit, `do_schedule` entry and the idle loop (§1.1) — so the *killer's* CPU posts
it and the blocked reader's CPU picks it up on its next pass. It is bounded by a
scheduler pass and by nothing that is a duration.

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
| last `Acceptor` handle | `closed = true`; queue dropped | queued clients: `Gone`/EOF. New opens: `Gone` |
| last `ConnectionEnd` handle | both `PipeShared` ends' counts fall | peer: `Gone` on write, `0` on read |
| last `Connector` handle | nothing | the server keeps accepting from whoever else holds one |
| last `Namespace` handle | its `Arc<Connector>`s drop | nothing observable; a connector outlives it if a handle does |
| last `DeviceClaim` handle | the class is released for re-minting | init can mint a fresh claim for a respawned daemon |
| last `SharedMemObject` handle **and** mapping | pages freed | — |

**A dead port stays dead, and this is the one place the design is worse than
what it replaces.** Say it plainly. Today a respawned daemon calls
`services::listen("soundd")` and the name is bound again, so a process that
connects *afterwards* reaches the new instance. After this, the acceptor moved
into the dead process and its `PortShared` is `closed` forever; a `Namespace` is
immutable, so every process whose namespace was built before the crash holds an
`Arc<Connector>` onto that dead `PortShared` and can never reach the replacement.
init can create a fresh port and put it in the namespaces of programs it launches
*next*, so a machine recovers forward and not backward.

Three things bound how much this costs, and none of them makes it not a
regression:

- **Nothing supervises a daemon today.** There is no respawn loop anywhere in the
  tree, so the reachable difference is between two things that do not happen.
- **No client recovers today either.** `AudioStream::open` and
  `Window::create_with_flags` connect once; a client whose daemon died holds a
  dead connection and does not re-`connect` in either world.
- **The device claim half genuinely improves**, and that is the row above: the
  class is released the same way, and *only init* can mint the replacement, so a
  process racing the respawn for the claim is not a thing that can happen.

The mechanism that would close it is small and is named in §12: a `PortOwner`
right init retains, and a `SYS_PORT_REARM` that mints a fresh `Acceptor` for an
existing `PortShared` and clears `closed`. Every namespace already points at that
`PortShared`, so a re-armed port is reachable by every client that predates the
crash. It is not built here because a re-arm with no supervisor to call it is
speculative generality, and **113 is the number it would take** — this delta
stops at 112 and leaves 113–115 free (§13).

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
| **compositor** | `services::listen("compositor")` at `session.rs:130`, first statement of `Session::start` | `endow::get().take("serve:compositor")`. `Command::new("/bin/filepicker").spawn().ok()` at `:219` becomes a launcher call, and the filepicker port is created by *init*, so an editor's `pick_file` cannot precede it |
| **soundd** | `services::listen("soundd")` at `main.rs:1766` | acceptor endowment; `MAX_CONTROL_CLIENTS = 63` unchanged; the shm token and `signal_pipe_id` in `MSG_STREAM_OPENED` become two transferred handles (§6.3) |
| **netd** | `services::listen("netd")` at `main.rs:1206`, after `DmaNic::open()` | acceptor + `dev:nic` claim endowment; the whole pipe-id protocol becomes handle transfer (§6.4) |
| **filepicker** | `services::listen("filepicker")` at `main.rs:463`; `accept` at `:466` is `.expect("accept failed")` | acceptor endowment; the `.expect` stays — an acceptor that refuses is a kernel bug now, not a peer |
| **sshd** | no service name; `TcpListener::bind` via netd | unchanged shape; `receives = ["netd", "launcher"]` |
| **terminal / console** | `Host::listen("surface.<pid>")` (`terminal/src/main.rs:40-44`, `console/src/main.rs:65-67`), `TOYOS_SURFACE` in the child's env | `provides = ["surface"]` (§2.2): its own `SYS_PORT_CREATE`, and `"surface"` in the namespace it builds for its shell. init creates nothing for it and holds nothing of it, which is what makes one terminal's surface unreachable from another's |

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
constants, two identical policies, eight source lines
(`rg -n --glob '!specs/**' 'BOOT_RETRIES|BOOT_RETRY_INTERVAL_NS'` → 8, all code:
`toyos/src/net.rs:264,265,272,276` and `toyos/src/audio.rs:188,189,266,270`). And
per boot on metal-sim, sshd's **100 `SYS_NANOSLEEP` calls and its exit at
t=1.69 s on a boot that completed at 0.38 s**
(`hardware/network-clients-pay-a-boot-retry`).

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
next write sees `Gone` — by construction rather than by bookkeeping.

`shm.grant(client_pid)` at `userland/soundd/src/main.rs:627` goes in the same
commit, and it is one of the two callers that keep `SYS_ACCEPT`'s pid alive until
this chunk (§3.3).

### 6.3a The compositor's shared memory, which is the same migration

soundd is not the only protocol carrying a `SharedToken`, and the compositor's is
larger. Four `token: u32` fields cross the window protocol
(`userland/window/src/lib.rs:140`, `:189`, `:197`, `:198` — the last pair is
`token`/`old_token` on a resize), six `SharedMemory::map` sites consume them
(`window/src/lib.rs:414`, `:512`, `:532`; `compositor/src/session.rs:140`, `:152`,
`:756`, `:909`), and the grant is `shm.grant(pid)` at `session.rs:859` —
the other caller that keeps the accept pid alive.

**And one of those tokens is the kernel's, not a client's.** The framebuffer
claim reports `FramebufferInfo { token: [u32; N], cursor_token: u32 }` and the
compositor maps them (`session.rs:140`, `:152`, and again at `:909` after a mode
set). Under `SharedMemObject` those stop being tokens: the claim answers with
`SharedMem` handles, and a mode set that reallocates the scanout sends a fresh
one rather than a fresh number. That is the last place a `SharedToken` reaches
userland from the kernel rather than from a peer, and it is why deleting
`shared_memory.rs` is not finished by §6.3 alone.

All of this is chunk 6, with soundd's, and the four window-protocol fields become
transferred handles the same way.

### 6.4 netd's protocol

netd is the heaviest single migration and the one with the most to gain. Its
wire format carries **nine** `*_pipe_id: u64` fields — seven in the shared
message structs (`toyos/src/net.rs:131,132,149,166,167,181,182`) and two in
netd's own `PipedConnection` (`main.rs:212,213`). `pipe::open_by_id` is called
once, at `main.rs:224` inside `open_pipe`, which has **five** call sites
(`:229`, `:230`, `:563`, `:567`, `:866`); `open_piped_connection` wraps two of
them and is itself called at `:935` and `:1137`. Every one becomes a
`SYS_HANDLE_SEND` of the pipe end itself.

`SYS_SOCKET_CREATE` is **renamed `SYS_CONNECTION_JOIN` and keeps number 76.**
The first draft of this section retired it, having read only half of what it
does: taking two pipe *ids* is the half that dies, and making one duplex object
out of two simplex ends is the half that has three callers outside this
repository — `rust/library/std/src/sys/net/connection/toyos.rs:55-57` and
`socket2/src/sys/toyos.rs:485-490,630-635`. std's `TcpStream` is one handle and
netd's data path is two pipes, so something has to join them. Handle-addressed
it grants nothing, because everything it reaches is already the caller's, which
is the same move §3.3 makes for `SYS_DUP` → `SYS_HANDLE_DUP`. §8.4's grep gate
is satisfied by the *name* being gone. `toyos::net`'s public functions keep their
signatures, so the mio fork's `toyos_stream.rs`/`toyos_listener.rs` see no change
beyond the `Fd` rename.

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
| `toyos/src/poller.rs` (204 lines) | `Poller::new` at `:75-78` does `io_uring_setup` → `(ring_fd, shm_token)` → `try_map_shared`. `SYS_IO_URING_SETUP`'s new shape (§3.3) returns the mapping, so both lines collapse to one and the `shm_token` is gone. **Same edit as the mio fork's `selector.rs:30`, and it is here for the same reason** |
| `toyos/src/lib.rs` | `Pipe::pipe_id` deleted, `pipe_map` keeps its handle form |

### 6.5a `userland/libc`, which is not the C corpus

The C test corpus needs no migration (§6.7). The layer it links against does.
`userland/libc` is 4,665 lines across 12 files and it names the deleted surface
in three places, none of them optional:

| what | where | after |
|---|---|---|
| `toyos_abi::Fd` | 5 sites | `RawHandle`, chunk 2 |
| `syscall::waitpid` | `misc.rs:110-115`'s `waitpid(pid, status, options)` | **`SYS_WAITPID` is retired** (§3.2). POSIX `waitpid` takes a pid and there is no pid-addressed wait left, so libc keeps a `pid → Process handle` map filled by whatever spawned the child and answers `ECHILD` for a pid not in it. This is the compat layer faking ambient authority, which is what CLAUDE.md says the layer is for and where it says the ugliness belongs. Chunk 7 |
| `toyos::poller::Poller` | `socket.rs` | follows the SDK's `Poller`, above |

`toyos::net::*` is 14 call sites in `socket.rs` (903 lines) and every signature is
stable by §0's constraint, so the whole BSD-socket surface is untouched — which
is the constraint paying for itself a second time. `fork`, `execvp` and `system`
already return `-1` (`misc.rs:98`, `:104`, `stdio.rs:601`), so libc has no spawn
path to route through the launcher.

`userland/libc` is the one crate built outside `[profile.toyos]` and says so where
it is; nothing here changes that.

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

`design-debt/fd-is-a-unix-ism` asks for `Fd` → `Handle`. It gets
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
`build/std-change-needs-an-unlanded-abi-change`, and for a branch
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

`mio`: **six** lines across four files — four `use toyos_abi::Fd`
(`selector.rs:7`, `waker.rs:4`, `toyos_stream.rs:6`, `toyos_listener.rs:7`)
become `RawHandle`, `selector.rs:30`'s `map_shared` is *deleted* because
`io_uring_setup` returns its own mapping, and `waker.rs:13`'s `pipe()` is now
fallible where `Waker::new` already returns `io::Result`. `socket2`: one line.
Seven lines is the whole fork estate's exposure.

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

**Twelve of them, 1,470 lines, are *about* the model being deleted and are
rewritten rather than migrated** (`wc -l` over the twelve names below):

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

### 6.7a How a test binary gets any authority at all

**The 90 guest binaries are not `[programs]` entries and never have been.**
`src/build.rs:980-997` reads `tests/toyos-rust-tests/src/bin/` and pushes every
built binary straight into the initrd; the `[programs]` map in a test
`system.toml` names `soundd`, `test-runner`, `toybox` and nothing else. So the
manifest cannot declare their `receives`, §8.1's `every_receives_names_a_provider`
cannot see them, and the launcher cannot start them — `Command::new` from
test-runner resolves no `[programs]` key and takes the direct `SYS_SPAWN` path
(§4.5). Left unstated, the implementing agent discovers this at chunk 8 with
`compositor_client_death`, `ipc_hostile_peer`, `compositor_stall`,
`netd_hostile_peer` and the five sound clients all unable to name anything.

**`test-runner` is a manifest program and its namespace is the test estate's
authority.** Its `receives` in each test config is the union of what that
config's binaries need — `compositor`, `soundd`, `netd`, `filepicker` — and
`test-runner` passes **its whole namespace** to every binary it spawns, by
`Command::endow("svc", ..)` with the handle it holds. Three things follow and all
three are deliberate:

- **The test estate is the one place least authority is not enforced**, and it is
  named here rather than discovered. A test binary holds what test-runner holds.
  Enforcing per-binary authority would mean a manifest row per test binary, which
  is a build-system change (`[programs]` would have to absorb
  `tests/toyos-rust-tests`) for a property no gate asserts.
- **§8.3's `endowment_denied` is where least authority *is* asserted**, and it
  works because that binary builds its own two namespaces and spawns itself —
  it does not rely on what test-runner gave it.
- **`test-runner`'s row in a config is checkable**, so `every_receives_names_a_provider`
  still has teeth over it: a config whose test-runner receives `compositor` while
  no program serves one is refused at build time, which is exactly the class of
  mistake a hand-edited test config makes.

**`window_refusal` needs one thing the estate does not have**: a *fake* server.
Today it squats `services::listen("compositor")` in a boot where no compositor
runs. After this it creates a port with `SYS_PORT_CREATE`, builds a namespace
mapping `"compositor"` to that port's connector, spawns a child with it, and
refuses from the acceptor — all inside one test binary, with no manifest row and
no name anyone else can see. That is strictly better than what it does now, and
it is the pattern every "hostile server" test uses from here.

The eight test `system.toml` files gain `serves`/`provides`/`receives`/`devices`
rows, `test-runner`'s among them.

**Host-side harness assertions that must be re-read, not just re-run:**

- `tests/toyos.rs:9074` asserts the literals `"netd: dropping pid"` and
  `"netd: refusing pid"`. The accept pid is gone (§3.3), so both lines and this
  assertion change together.
- `tests/toyos-rust-tests/src/bin/locale_gate.rs:97-99` prints all three
  `toyos::surface::Notice` variants including their `pid` field. `Notice` is
  public SDK API and the field becomes a `Koid`.

- `metal_sim_compositor` (`tests/toyos.rs:3863-3886`) asserts four exact daemon
  lines, one of which is `sshd: no network on this machine, exiting`.
  `tests/metalcase/system.toml`'s own comment prices that line at "the second
  `NetdConn::connect_blocking` spends retrying a netd that will never come".
  **After this branch that second is gone**: netd exits before or shortly after
  sshd asks, so sshd's `endow::service("netd")` is either `ServerGone` at once or
  a connection whose first write is `Gone` — learned from the guest, never
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
| the whole service registry | `kernel/src/listener.rs` | 231 lines |
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

Deleted on 2026-08-10; `git log -- specs/issues/<area>/<slug>.md` is the story.

| file | how |
|---|---|
| `kernel/terminal-races-compositor-at-boot` | the race is unrepresentable: the port exists before either process |
| `design-debt/pick-file-cannot-say-why-it-failed` | `Result<Option<String>, PickError>`, and the reachable cause is gone |
| `hardware/network-clients-pay-a-boot-retry` | no retry loop exists |
| `isolation/capability-by-id-or-name` | all four instances: `PipeId`, the service name, `SharedToken`, the device claim |
| `design-debt/fd-is-a-unix-ism` | `Fd` → `Handle` |
| `design-debt/sharedtoken-has-no-raii` | `SharedToken` deleted; `SharedMemObject` is Arc-lifetimed |
| `design-debt/io-uring-abuses-shared-memory` | the ring owns its `PageAlloc`; `SYS_IO_URING_SETUP` returns its own mapping |
| `isolation/abi-wrappers-return-error-as-value` | both halves: `pipe()` in chunk 2, `tls_alloc_block()` in chunk 9, and std's `__tls_get_addr_slow` `rtabort!`s rather than adding an offset to a refusal |
| `build/std-change-needs-an-unlanded-abi-change` | chunk 0 |
| `build/boot-config-gates-iterate-a-hand-written-list` | §8.1's `every_shipped_boot_config_is_covered`: one `ALL_CONFIGS` list, asserted against what the walk finds |
| `build/docs-total-budget-comment-is-stale` | the measurement is out of the comment and the assertion prints what is spare |
| `diagnostics/syscall-profile-is-64-bins-wide` | `SYSCALL_PROFILE_BINS` is the ABI's, and a number it does not issue lands in `SYSCALL_PROFILE_OTHER` rather than nowhere — the parts sum to the total |

**One row of the first draft of this table was wrong and the file stays open.**
`hardware/device-claim-succeeds-with-no-device` said "keyboard and mouse gain the
presence gate the other four have"; they did not. `device::try_claim` still
answers `Ok` for `Keyboard` and `Mouse` on a machine with no HID of any kind,
because nothing registers their presence the way a framebuffer, a NIC and the
two sound cards register theirs. Nothing in this branch needed it and inventing
a presence signal for two input classes is its own piece of work.

### 7.3 `specs/issues/` this re-scopes

| file | what is left |
|---|---|
| `specs/issues/isolation/process-isolation-ungated.md` | `SYS_LISTEN`'s squat and the `SYS_GRANT_SHARED` no-revoke clause both go. What remains is the general revocation question, and the entry's own trigger fires: *"It stops being sound the moment the reachable set is no longer exactly what the owner named — … when `SYS_HANDLE_SEND` makes a grant transferable."* Rewrite it around that. |
| `specs/issues/diagnostics/process-stats-exited-child-only.md` | addressing fixed by the handle; the accounting gap is untouched |
| `specs/issues/isolation/client-request-is-an-allocation.md` | the third instance's "the attacker can be its own service" clause dies with `SYS_LISTEN`; the compositor's unbounded windows remain |
| `specs/issues/isolation/compositor-and-netd-unbounded-accept.md` | unchanged; the compositor's half is still owed |
| `specs/issues/build/abi-split-reads-commits-not-the-tree.md` | this branch is the case the `Abi-Inseparable` trailer exists for; the finding is unaffected |

**And two the branch filed rather than closed**:
`specs/issues/isolation/a-moved-handle-is-always-re-movable.md` (§6.3 assumes a
`MAP`-only send the rights model cannot express) and
`specs/issues/isolation/a-provided-name-cannot-reach-an-undeclared-child.md`
(`SYS_NAMESPACE_BUILD` has no "keep everything in base", so the direct spawn
path cannot merge a transferred name into an inherited namespace).

---

## 8. Gates

Every gate below either fails on today's tree or has a negative control that
fails on a tree with the defect reintroduced. A gate with neither is not listed.

### 8.1 The race, refused at build time — `cargo test --lib`, milliseconds

`src/build.rs`, the shape of `no_shipped_boot_config_starts_sshd`
(`src/build.rs:1095`, the file's only `#[test]` today):

- `every_receives_names_a_provider` — for each of the 11 configs, every name in
  any program's `receives` must appear in some program's `serves` **or**
  `provides` in the *same config*. **This is the gate with the sharpest teeth on
  the list**: it is the build-time form of "a client cannot name a service the
  system does not have", it needs no guest and no mutated tree, and its negative
  control is a bad config literal in the test body. The `provides` arm is what
  lets `surface` be named at all (§2.2) and it is not a loophole — a `provides`
  name still has to be declared by *some* program in the same config, so a typo
  in `receives = ["sruface"]` is still a build error.
- `a_provides_name_is_never_also_a_serves_name` — the two mean different things
  (one port machine-wide, one per instance), and a name declared both ways is a
  config where init would create a port nobody accepts from while the real port
  is made somewhere else. Refuse it; the failure otherwise is a dead connector in
  somebody's namespace and no error anywhere.
- `every_device_class_has_at_most_one_claimant` — two programs declaring
  `devices = ["framebuffer"]` is a config init cannot satisfy, and today it is a
  runtime first-come race.
- `no_diag_program_claims_the_screen` — `diag/system.toml`'s programs declare no
  `devices`. This makes the diagnostic image's whole reason for existing
  checkable for the first time — see §2.3 for what it does and does not replace.
- `every_started_program_is_declared` — `[boot] start` names program keys, so a
  typo is a build error rather than a kernel panic at `spawn_kernel`.
- `every_shipped_boot_config_is_covered` — `no_shipped_boot_config_starts_sshd`
  iterates a hand-written `[Boot::Normal, Boot::Diag, Boot::Console]`, which is
  complete today and is complete by nobody's construction. Since this branch
  rewrites every config anyway, the four gates above iterate a single
  `ALL_CONFIGS` list asserted equal to `find . -name system.toml`'s answer, so a
  config added without a gate row is a red rather than a silence
  (`build/boot-config-gates-iterate-a-hand-written-list`).

### 8.2 Queueing — `connect_before_serve`

A guest binary, on the shared `tests/testcases` boot (no new boot). The parent
creates a port, spawns the **client** with the connector endowed, waits for the
client's first frame to be in the ring, and only then spawns the server with the
acceptor.

Two arms that must answer differently, the `hda_client_stall` shape:

1. The client writes and reads a reply. It must succeed, and the server must find
   the client's frame **already buffered** at its first `accept`.
2. The same binary with the server spawned and immediately exited: the client's
   write must return `Gone` and its read `0`, and the test asserts the
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
  was written**. `Tally::new` compares it against `Day::today()` at startup
  (`tests/toyos.rs:10189-10192`) and the run fails at its end whether or not
  either test ran (`:10245`, `:10346`) — so the cost is a whole suite followed by
  a red, not a fast refusal. A branch still open on that date reds for a reason
  that is not its own. That is a scheduling fact, not a licence to
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
`Gone`, and that is only true because `handle_count` is not the Arc count.

**Three of those six did not exist until 2026-08-10, this one included.** Chunk
8 landed the estate's migration and none of the object layer's own gates;
`device_claim_crash_release` is `device_claim_lifetime` under another name and
`process_lifecycle` and `handle_kill_policy` are real. The three that were
missing are now `tests/toyos-rust-tests/src/bin/{handle_basic,handle_transfer,
kill_while_blocked}.rs`, on the shared `tests/testcases` boot.

**And the census clause was two-thirds unmet, which is where three of the
review's findings lived.** The per-variant counters existed and their only
reader wrote them to the *kernel log*, so every leak assertion in the estate was
against the machine-wide total — where a leak of one kind is hidden by churn in
another, and `File`, `Device`, `Acceptor`, `Connection`, `IoUring` and `Console`
were covered by nothing at all. `toyos_abi::syscall::OBJECT_KINDS` declares the
kinds, `debug_action::CENSUS_KIND` answers one, `toyos::census::Census` is the
reader, and the kernel checks the two declarations against each other on every
call. Every churn assertion in the estate is per kind now.

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
`build/std-change-needs-an-unlanded-abi-change`.
*Nothing else in this plan can be verified until this works.*

Four things about that file, because it is written into a directory every
worktree shares and none of them is optional:

- **Write it unconditionally at the start of every std build**, never "if
  absent". A run killed between write and delete leaves a `paths` override
  naming a worktree that may since have been removed, and the next build in
  *any* checkout then fails resolving a path that is gone. Overwriting makes
  that state unreachable; a `write-if-absent` makes it permanent.
- **It must be inside `buildlock::Global`**, which already serialises the
  sysroot build, or two worktrees building std at once each point the shared
  file at the other's sources. Verify that the write and the delete are both
  inside the scope that lock covers rather than beside it.
- **`--pr` requires the submodule clean** (§10.2). An untracked
  `rust/.cargo/config.toml` left behind is a dirty submodule and refuses the
  landing of whichever worktree runs `--pr` next, not necessarily this one.
  Either the delete is unconditional on every exit path or `rust/.gitignore`
  carries the entry — and adding a line to the fork's `.gitignore` is a hunk
  `forks.toml`'s "expect empty" invariant explicitly forbids, so it is the
  delete.
- **`paths` may not do it, and the test is what decides.** Cargo's directory
  overrides are documented against registry and git sources; `toyos-abi` is a
  *path* dependency of `library/std`, and cargo also warns and ignores an
  override whose manifest alters the dependency list. If the marker test fails,
  the fallback is not another cargo mechanism but the build system editing
  `rust/library/std/Cargo.toml`'s two relative paths for the duration of the
  build, under the same lock and with the same unconditional restore. Say which
  one was used in the commit message.

**Done, 2026-08-09, and it is the fallback that shipped.** `paths` does apply to
path dependencies — measured, the marker reached the sysroot — so the question
the test was written to answer is settled and the answer is yes. It was not
taken. Overriding `toyos` alters `toyos-abi`'s resolved source, and cargo says
so in fifteen lines on every toolchain build, ending "in the future this message
will become a hard error". The manifest edit produces none of that, and the
crash residue is the same either way: a killed run leaves the submodule dirty
under both, and both self-heal because the rewrite matches whatever path is
there rather than one expected spelling.

The gate is stronger than the marker the chunk asked for. `assert_std_built_from`
reads cargo's **dep-info** after every std build and refuses a sysroot naming any
`toyos-abi/src` or `toyos/src` file outside the building worktree — the witness
records what the builder intended, this reads what the compiler was handed, and
the two disagreed for the whole life of the defect. Empty dep-info is a refusal
too, so it cannot go vacuous if cargo moves the files.

**Chunk 1 — object infrastructure, zero users. Green.**
`kernel/src/object/{mod,handle,rights}.rs`, `toyos-abi/src/handle.rs`,
`kernel/clippy.toml`'s `disallowed-methods` wall, the per-variant `LIVE_*`
census, the deferred zero-handle queue and its three drain sites. `HandleTable`
lives inside `ProcessData` alongside `FdTable`, empty. `retired_syscalls!` macro
with the four existing gravestones moved into it. Boots; nothing uses any of it.

**Done, 2026-08-09, with two deviations and one measurement the plan did not
have.**

- **There is no `kernel/clippy.toml`, because nothing in this repository runs
  clippy** — not CI, not `cargo test`, not the build. A `clippy.toml` is a wall
  with nothing behind it, which is the shape this project refuses. The wall is
  `src/sourcegate.rs`, a scan in `cargo test --lib` over `kernel/src` and
  `toyos-sched/src`, whose two `mem::forget` exceptions carry a **count** — so
  an added `forget` beside a permitted one is a red, and a permitted one that
  disappears is a red too.
- **There is no `kernel/src/object/rights.rs`.** `Rights` has to live in
  `toyos-abi` for the ABI to name it, so a kernel module for it would re-export
  and nothing else.
- **The zero-handle drain is guarded by a plain atomic load before the lock.**
  It runs at every syscall exit and `Lock::lock` is a `fetch_add` — the one
  operation TCG cannot emit inline, at a few hundred a boot of which one cost
  350 ms (known-issues §8). The flag is written under the lock at both ends, so
  it never reads empty over a queued object.

`toyos-abi` has host tests now and is in no list of host-test crates; chunk 9
adds it to the root `CLAUDE.md`. Its `ring` tests were already there and already
unrun.

**Chunk 2 — `Fd` → `Handle`. Green, and this is the big mechanical one.**
`pipe.rs` → `object/pipe.rs` with `PipeShared` and two end types; the remaining
`Descriptor` kinds become objects; `FdTable` deleted and `HandleTable` is the
only table with stdio pre-seeded at slots 0/1/2 generation 0; `fd.rs`'s dispatch
becomes exhaustive matches on `KObjectRef`; `io_uring::Source` keys become
`Koid`s. `SYS_OPEN`'s and `SYS_PIPE`'s new return types (§3.3) are here, which is
what makes the mio waker's fallible `pipe()` edit land in the same chunk. std's
`Fd`→`Handle` and the mio/socket2 fork edits land here. Bad-handle policy is
log-and-error for now.

**Split in two on 2026-08-09, and both halves are done.** The chunk has a
seam in it the plan did not name: the `Fd` -> `RawHandle` rename touches the
ABI, the SDK, userland, `userland/libc`, the test binaries and all three
repositories, and **the kernel never named `toyos_abi::Fd` at all**. So the
rename is green on its own with the kernel untouched, and it landed that way —
`rust` @ 4288885f, `mio` @ 2517dfaf, `socket2` @ 2ad5af0d, 298 of 298.

**The kernel half is done, 2026-08-09, 298 of 298 and gate A fast green on all
four configs.** `kernel/src/fd.rs` is deleted; `FdTable`/`Descriptor` are
`HandleTable`/`KObjectRef` with nine variants (six `deferred`, three
`immediate` — below); `object/ops.rs` is the dispatch, exhaustive with no `_`
arms. Five things it found that the plan did not have:

- **A row says whether its last handle is an event, and the queue is only for
  the ones where it is.** `HandleEntry::drop` enqueued *every* object, so the
  last `Arc` — and therefore the destructor — moved off the dropping thread's
  stack onto whichever CPU drained next. A killed process's dirty file was
  flushed from `drain_zero_handles` in the idle loop, and the VFS write path
  wrote through the guard page below a **16 KiB** per-CPU idle stack
  (`IDLE_STACK_SIZE`, against 128 KiB for a task): `fd_lifetime` panicked with
  `write unmapped address`, took its shared boot down and cost 147 collateral
  reds. `kobject!` rows are now `deferred` or `immediate`; an `immediate` row
  gets its empty `ZeroHandles` impl **from the macro**, so a hand-written hook
  beside one is a coherence error and "a hook that never runs" is
  unrepresentable. `ZeroHandles::on_zero_handles` lost its default body for the
  same reason from the other side.
- **`HandleTable::get_ref` is a borrowing accessor beside the owning one.**
  §1.1 says every accessor hands back an owned `Arc`; `read` and `write` run to
  completion under the guard they resolved through — exactly where the
  descriptor dispatch ran — and cloning an `Arc` there would put one atomic
  read-modify-write on the two hottest syscalls in the kernel. Nothing escapes:
  the lifetime is `&self`'s.
- **`io_uring::Source` keeps its `PipeId` and `ListenerId`.** The chunk line
  said the keys become `Koid`s; what was actually keyed by an fd number is
  `PendingPoll`, and that is a `RawHandle` now. A `PipeId` is the *kernel's*
  identity for the ring and not a number a process chose, and two
  `PipeReadEnd`s can name one pipe while a `Koid` names neither — so the move
  belongs with the objects that replace them, in chunks 3 and 6.
- **A file's `writable` field is gone; the right carries it.** `SYS_OPEN`
  installs without `Rights::WRITE` when the flags do not ask for it, and the
  refusal is the same `PermissionDenied` the field produced.
- **Two handles to one file share the cursor** — `capability-handles-spec.md`
  §6.8's declared semantic change, which is also what Unix `dup` does. The one
  in-tree caller that could have cared is `fd_lifetime`, which reads through the
  survivor after a `seek`, and it passes.

Two things the first half already knew about it:

- **§6.6's `AsRawFd` -> `AsRawHandle` row is not done and should not be.**
  `std::os::fd::RawFd` is cross-platform and is what `socket2` implements
  `AsRawFd` for; changing it widens the fork estate's exposure, which §0's
  constraint exists to narrow. The sign crossing lives at the `toyos_abi`
  boundary instead, one bit-preserving cast per call.
- **`library/std/src/os/fd/owned.rs` is a ninth file §6.6's table does not
  list** — two `toyos_abi::Fd` call sites in `cfg(target_os = "toyos")` arms of
  a cross-platform file. `rg -o 'toyos_abi::Fd' rust/library` -> 9 hits counted
  the *imports*, not the call sites.

**Two transitional objects exist only between here and their deleter, and saying
so is what keeps this chunk green.** `ListenerObject` is a `KObjectRef` variant
in chunk 2 and is deleted by chunk 3's `Acceptor`/`Connector` — §1.3 describes the
end state, not this one. `PipeShared` keeps its `PipeId` and the `IdMap` behind
it until chunk 6, because `SYS_PIPE_OPEN`/`SYS_PIPE_ID`/`SYS_SOCKET_CREATE` are
live through chunks 2–5 and netd's protocol still needs them.
*Gate: full suite, `handle_basic`, gate A fast tier.*

**Chunk 3 — ports and namespaces. Not green: the registry is gone before
anything uses its replacement.**
`object/port.rs`, `object/namespace.rs`, `SYS_PORT_CREATE`,
`SYS_NAMESPACE_BUILD`, `SYS_NAMESPACE_OPEN`, `SyscallError::Gone` (§3.5), and
`SYS_ACCEPT` taking an `Acceptor` handle. **`SYS_ACCEPT` keeps returning the
client pid until chunk 6** — §3.3 says why: the compositor and soundd both grant
shared memory to it and have nothing else to grant to until handle transfer
exists. `kernel/src/listener.rs` deleted, `SYS_LISTEN`/`SYS_CONNECT` retired.
`toyos/src/{port,namespace}.rs`. Kernel compiles, userland does not.

**Done, 2026-08-09, and the non-green stretch is 3–5 rather than 3–4.** The
ordering constraint the plan did not have: **`library/std` depends on `toyos` by
path**, so the SDK is compiled as part of the sysroot build and a `toyos` that
does not compile stops *everything* — the kernel included, since the toolchain
step runs first. Deleting `toyos/src/services.rs` therefore has to take its four
in-SDK callers with it, and those are `audio.rs:267`, `net.rs:268`, `net.rs:273`
and `surface.rs:36`, every one of which needs the process's own namespace and
therefore `toyos/src/endow.rs` — which is chunk 4's. So the three chunks are one
stretch: **chunk 3 leaves the sysroot build red on exactly those four lines**,
and nothing downstream of the toolchain step is compiled at all until chunk 4's
`SYS_ENDOWMENTS` lands and chunk 5 rewrites them. Anyone resuming here should
expect `cargo run -- --build-only` to stop in `toyos` and should not read that as
a kernel failure: the kernel half of chunk 3 compiled clean in the run before
`services.rs` was deleted.

Two other things it found:

- **The io_uring watch names the *port*, not either end.** §4.1 says
  `Source::Listener(ListenerId)` becomes `Source::Acceptor(Koid)`. A koid cannot
  be resolved back to an object without a registry — which is the thing being
  deleted — and the wake that has to complete an acceptor's poll is posted by a
  *client* going through the `Connector`, which holds no acceptor. So `Source`
  carries `Arc<PortShared>`, the one thing both ends share, and compares by
  `Arc::ptr_eq`. `Source` is `Clone` rather than `Copy` for it. The watch now
  holds what it watches, which is also what stops a poll outliving its port.
- **A `Connector` gets `Rights::TRANSFER` checked at `SYS_NAMESPACE_BUILD` and
  the base namespace `Rights::READ`**, per §3.1, and a name the base does not
  carry is *absent* from the result rather than an error: narrowing is an
  intersection, and asking for a name you do not hold grants nothing either way.

**`cargo test --lib` stays green across this chunk and chunk 4's first half**,
which matters because that is the command the push rule names. It builds the
`toyos-build` host crate — buildlock, toolchain, pr, docs, image formats — and
compiles no kernel and no guest binary. The non-green stretch is invisible to
it, so "green before push" is satisfiable on every commit of this branch even
where the guest does not build. The one thing that *can* red it here is
`src/docs.rs`'s reference scan, if a spec edit names a `specs/issues/` file that
does not exist.

**Chunk 4 — spawn endowment and `/bin/init`. Green.**
`SpawnArgs`'s two new vectors, `SYS_ENDOWMENTS`, `toyos/src/endow.rs`; the
manifest schema in `src/build.rs` and `/etc/system.manifest`; `/bin/init` with
the port/claim/namespace/spawn loop and the `launcher` service; the
`INIT_PROGRAMS` chain deleted and `KernelArgs` shrunk with its layout asserts
updated; `SysCap` and `SYS_DEVICE_CLAIM`/`SYS_RT_ENTER`; every one of the 11
`system.toml` files rewritten. The §8.1 host gates land with the schema.
*Gate: every config boots; `endowment_denied`; the six `--lib` config gates (§8.1).*

**Sub-boundary done, 2026-08-09 (`a20c24c`): the manifest schema and the six
§8.1 gates, staged early per the note above.** The five per-program keys plus
`args`, and `[boot] start`, are `#[serde(default)]` on `SystemConfig`/
`ProgramConfig` and authored in all eleven configs *beside* the still-live `init`
field — the build still spawns from `init`. `ALL_CONFIGS`, `INIT_SERVED` and the
six gates, each with a negative control in its own body, are in `src/build.rs`'s
test module; `cargo test --lib` is 74 of 74 and the tree is warning-clean.
Authoring principle: a program's `receives` in a config is its §4.6 production
authority intersected with the services present in *that* config, so no config
names a provider it lacks; each `boot.start` is its config's old `init`
membership unchanged; `launcher` is init's and available in every image.
`args`/`realtime`/`boot` carry `#[allow(dead_code)]` until they are consumed.

**Second sub-boundary done, 2026-08-09: spawn endows, and a process can read
its own table back.** `SpawnArgs` is 80 bytes with `slot_map`/`endow`/`labels`
(§3.6, layout asserted); `build_child_handles` verifies every endowed handle —
resolves, carries `TRANSFER`, label in range, and *the child's table has room*
— under one hold of the parent's lock and only then removes, so the
all-or-nothing claim is true rather than nearly true; `ProcessData` carries the
labels and `SYS_ENDOWMENTS` answers them; std's `SpawnArgs` and
`CommandExt::endow` are `rust` @ `e36a68a6`, pushed, pinned.

**And it found that chunk 3 never compiled the kernel.** The stretch note above
says the guest build stops in `toyos`, which is upstream of the kernel — so
nothing compiled `kernel/` after chunk 3 added its two `KObjectRef` variants.
`cargo build --target x86_64-unknown-none` inside `kernel/` reaches the
`toyos-ld` linker without the build system and is the loop that found it:
thirteen errors, all chunk 3's — eleven `ops.rs` matches missing their
`Connector`/`Namespace` arms, no `UserSafe` for `NamespaceBuild`, and a
`Source` that stopped being `Copy` when it started holding an `Arc<PortShared>`.
Anyone working inside the non-green stretch should use that command.

**Third sub-boundary done, 2026-08-09: the SDK, and with it the sysroot.**
`toyos/src/endow.rs` (`Endowments::get`/`take`/`service`, `EndowError`'s two
words), `toyos/src/syscap.rs`, both retry loops deleted, and `toyos::surface`
moved onto a port — `HOST_ENV`, `service_name` and `MAX_NAME` gone. That is
what reopened `cargo run -- --build-only`: it now gets through the sysroot and
std and stops in `userland/window` on `use toyos::services`, which is chunk 5.

**Fourth sub-boundary done, 2026-08-09: the kernel spawns one program.** The
boot `SysCap` and its endowment to `/bin/init`; `SYS_DEVICE_CLAIM` and
`SYS_RT_ENTER`; `SYS_DUP`/`SYS_DUP2` renamed to `SYS_HANDLE_DUP`/`_AT` with the
rights word; the whole `INIT_PROGRAMS` chain deleted and `KernelArgs` shrunk to
192 bytes; `[boot] start` authoritative and `init` deleted from all eleven
configs; `/bin/init` built into every image with the port/claim/namespace/spawn
loop.

Four things it decided that the plan did not have:

- **`SYS_HANDLE_DUP_AT` does not take a rights argument.** §3.3 gives it one; a
  narrowed handle at a slot is `dup_narrowed` then `dup2`, so the third
  argument would be a right no caller can request. `SYS_HANDLE_DUP`'s rights
  word carries `Option<Rights>` as `RIGHTS_UNCHANGED` — a wire encoding decoded
  at the boundary — so `dup` keeps its meaning and the seven `syscall::dup`
  call sites in the std fork do not move.
- **The manifest's format has one definition, `toyos-manifest/`.** The renderer
  is the build system's and the parser is init's, so a round-trip test is the
  only thing that makes them one format; it lives beside both. Its `parse` is
  `assert`-based on purpose — this is the build system's own output travelling
  beside the binary that reads it, not untrusted input. Add it to the root
  `CLAUDE.md`'s host-test list in chunk 9, with `toyos-abi`.
- **init's own served names travel in the manifest** as `init-serve` records,
  because `launcher` has no `[programs]` row to come from and a constant in
  init beside `INIT_SERVED` in the gate would be two spellings of one fact.
- **The launcher cannot be built in chunk 4, and this is a plan error.** §4.5's
  `MSG_LAUNCH` carries the child's stdio and answers with a `Process` handle,
  both over `SYS_HANDLE_SEND` — which is chunk 6's. So chunk 4 creates the
  `launcher` port (every namespace that names it resolves) and init parks in
  `accept` on it, dropping a client with a line rather than answering. **The
  protocol belongs in chunk 6** and the compositor's two launch sites keep
  `SYS_SPAWN` until then; the filepicker cannot be started between chunk 5 and
  chunk 6, because its acceptor is init's to hand over and the launcher is how.

Still owed for chunk 4: nothing. **Chunk 5 is next and is where the guest
reopens** — `userland/window` is the first line of it, and the migration list
is §6.1/§6.2 plus the eight `services::` sites in the test estate.

**Chunk 5 — every server and client. Green.**
compositor, soundd, netd, filepicker, terminal, console, toybox `screen`,
`filepicker-api`, `window`, and the SDK's `audio`/`net`/`surface`. The two retry
loops deleted. `TOYOS_SURFACE` deleted. `PickError`. The six `NoCompositor` sites
become three variants.
*Gate: full suite including all six desktop boots; gate A fast tier; a
same-session A/B of the thorough tier against `main`.*

**Done, 2026-08-09.** The nine `services::` sites, the surface chain and the
test estate's eleven, plus seven things the plan did not have — five of them
because init minting every claim changes more than the daemons' first three
lines.

- **`SYS_OPEN_DEVICE` and `SYS_SET_RT_PRIORITY` are retired here, and so are
  the seven pid-keyed device syscalls' gates** (§3.3's nine, less the two
  register calls chunk 2 already moved onto a handle). The moment init mints a
  claim and endows it, `device::is_owner(class, current_pid)` is init's pid and
  no daemon can present the display or the NIC ring. `SYS_GPU_PRESENT`'s
  rectangle no longer fits beside the handle in four argument words, so it is
  two packed pairs — a wire encoding decoded at the boundary and carried no
  further. `device::is_owner` and the six `Option<Pid>` statics are gone; what
  is left is one `bool` per class, which is all exclusivity ever needed.
- **A device's shared buffers are granted where the description is read, not
  where the claim is minted.** `try_claim` granted the scanout and DMA tokens to
  the claiming pid, which is now always init's — so every claimant read an
  address it was not allowed to map. `DeviceInfo` names its own tokens and
  `describe` grants them to the reader; a claim admits one handle, so at most
  one process can be that reader.
- **`realtime = true` became `syscap = [..]`, and the test estate is why.** A
  bool that can only ever say one thing was already the general mechanism
  wearing one name; the general form is what lets `test-runner` hold
  `["device", "dup"]`. It has to: five guest binaries claim the keyboard or the
  mouse, they are not `[programs]` keys so no manifest row can name them, and a
  claim *moves* — one boot runs `test_rs_i8042_keyboard` twice. So test-runner
  duplicates a `DEVICE|DUP` cap into every binary it spawns, which is §6.7a's
  "a test binary holds what test-runner holds" spelled with a capability
  instead of a namespace. `TRANSFER` is in every set and nameable in none:
  endowing *is* transferring, and chunk 4's `narrowed(Rights::RT)` would have
  refused soundd's spawn outright — a latent bug no boot had reached.
- **`Command` inherits the caller's namespace by default**, duplicated rather
  than moved, unless the caller endowed `svc` itself. That is §4.5's clause 3
  and §6.6's "namespace inheritance", and it is what makes §6.7a work without
  test-runner naming a handle at all.
- **soundd's signal pipe changed direction, and it is a simplification.** The
  client makes the pipe and names it in `MSG_STREAM_OPEN`; soundd opens the
  write end. It has to travel that way: a pipe id is openable by a *peer of its
  creator*, and chunk 3 stopped recording a peer on the client's end of a
  connection — so a client can no longer open soundd's pipes, while soundd can
  still open a client's. The old order needed soundd to hold a read end of its
  own until the client proved it had one, and §5.7's crash detection could not
  fire inside that window; there is no window now, and `signal_read_fd` is
  deleted.
- **`pipe_peer_scope` is deleted, and it is the ending that test was written
  for.** It asserted `be604ef`'s residual — a peer that only ever connected
  could open any pipe the creator ever made — and carried the message saying to
  delete it when the residual closed. Chunk 3 closed it: a client's connection
  records no peer. `specs/spec-staleness-sweep.md` §"The inverse" is updated to
  say so rather than to keep pointing at a file that is gone.
- **`abuse_listener_hijack`, `abuse_connect_flood`, `fd_lifetime`,
  `window_refusal`, `device_claim_lifetime`, `abuse_gpu_resolution`,
  `sched_stress` and `locale_gate` are rewritten here rather than in chunk 8**,
  because each names a deleted API and the guest does not build until they do.
  `window_refusal` is §6.7a's fake-server pattern: a port, a namespace, a child
  spawned holding it, and the refusals served from the parent.

**Three interim widenings, each declared in the config that carries it and each
the launcher's to remove.** They exist because chunk 4 found the launcher needs
handle transfer (chunk 6) *and* a `Process` handle to answer with (chunk 7), so
between here and there a parent can hand a child only what the parent holds:

1. `[programs.compositor] receives` names `compositor` in the four configs that
   have one, so a terminal or a picker the compositor starts inherits a
   compositor connector.
2. `[programs.terminal] receives` names `soundd` in `tests/desktopaudiocase`,
   so a shell-started `tone` reaches the mixer.
3. `filepicker` is in production's `[boot] start`, because its acceptor is
   init's to hand over and the compositor's `Command::new("/bin/filepicker")`
   is deleted — without it an editor's `pick_file` would queue on a port nobody
   accepts from and block forever.

The terminal's `KEEP_FOR_SHELL` is the same fact from the other side and says so
where it is.

Two gates the chunk added: `every_declared_capability_is_one_the_abi_has`
(`src/build.rs`, a `devices` class or a `syscap` right the ABI does not know is
a red in milliseconds rather than an init that panics at boot), and
`toyos-manifest`'s two round-trip cases for `syscap` and the class table.

**And one thing to carry:** `/bin/init`'s and netd's diagnostic lines are one
`write_all` each now, for soundd's reason — `netd: ready, at most ` and
`init: started test-runner` interleaved on the console and
`netd_connection_caps` parsed a cap out of the wrong number.

**Chunk 6 — shm objects and handle transfer. Green.**
`object/shm.rs`, `SYS_SHM_CREATE/MAP/UNMAP`, connection in-flight queues,
`SYS_HANDLE_SEND`/`RECV` and connection readiness. soundd's `MSG_STREAM_OPENED`
(§6.3), the compositor's four window-protocol tokens and the framebuffer claim's
own tokens (§6.3a), and netd's nine pipe-id fields (§6.4) all migrate.
`shared_memory.rs`, `SharedToken`, `SYS_PIPE_OPEN`, `SYS_PIPE_ID`,
`SYS_SOCKET_CREATE` and the shm pid-ACL deleted — and with the pid-ACL gone,
**`SYS_ACCEPT`'s client pid goes in this chunk**, together with the two
`shm.grant(pid)` calls that were its last authorizing readers and the diagnostic
lines and harness assertions that named it (§3.3, §6.7a).
`SYS_IO_URING_SETUP` returns its own mapping; the mio `map_shared` line and
`toyos/src/poller.rs:75-78` go with it.
*Gate: `handle_transfer`, `kill_while_blocked`, gate A both tiers' fast arm,
`device_claim_crash_release`.*

**Done, 2026-08-10. `cargo test` is 298/298 including gate A's fast tier, and
`cargo test --lib` 75/75.** One thing is outstanding and it is not code: the
`rust` submodule pin. See "What the pin owes" below.

What is done, and the eight things the plan did not have:

- **`SYS_SOCKET_CREATE` is not retired; it is renamed `SYS_CONNECTION_JOIN` and
  keeps number 76.** §3.2's reason for retiring it — "built a connection out of
  two pipe ids" — is only half of what it does. The other half is *making one
  duplex object out of two simplex ends*, and that has three callers outside
  this repository: `rust/library/std/src/sys/net/connection/toyos.rs:55-57` and
  `socket2/src/sys/toyos.rs:485-490,630-635`. `std`'s `TcpStream` is one handle
  and netd's data path is two pipes, so something has to join them. Handle-
  addressed it grants nothing — everything it reaches is already the caller's —
  which is the same move §3.3 makes for `SYS_DUP` → `SYS_HANDLE_DUP`. §8.4's
  grep gate is satisfied by the name being gone. **The userland map missed this
  because it looked only at `userland/`**; the fork estate is outside every
  check the tree runs on itself.
- **A device description installs handles where it is read, and the read path
  had to move.** `FramebufferInfo`, `NicInfo`, `HdaInfo` and `VirtioSoundInfo`
  keep their layout — a `RawHandle` is a `repr(transparent)` `u32` — but the
  fields are filled in per reader, so `ops::read_device` needs the table
  mutably and cannot run under the borrow `get_ref` hands out. `sys_read`
  resolves a `Device` twice: a `matches!` on the hot path, and a second slot
  lookup on a path that runs once per device per boot. Cloning the `Arc` for
  every read would have put an atomic on the hottest syscall in the kernel.
- **The image is minted once and remembered on the claim.** Re-minting on each
  read would install a fresh handle to the same buffer every time — an unbounded
  handle leak a process drives by reading in a loop. `DeviceClaim::describe`
  caches; `remint` replaces both the description and the cache, which is what
  `SYS_GPU_SET_RESOLUTION` needs.
- **A device region is minted per claim over an `Arc<Pages>`, not shared as an
  object.** An object whose handle count reaches zero is *retired* and can never
  be named again, so one `SharedMemObject` per screen would panic the second
  claimant. The pages are refcounted underneath instead, which also deletes
  `virtio_gpu::free_framebuffer`: a mode change drops the driver's reference and
  the old scanout survives until the compositor closes its handle. That replaces
  a forced unmap-everyone, which is the one thing a capability system may not
  do.
- **Handle transfer is two cross-wired queues on the connection, and the
  ordering rule is in the SDK rather than at each call site.** `HandleQueue` is
  `Lock<Option<VecDeque<Vec<HandleEntry>>>>`; a batch holds `HandleEntry`s, so
  `handle_count` stays raised for the whole crossing and a region sent to a
  client that dies before receiving is released by the queue dropping.
  `SYS_HANDLE_RECV` never blocks, so **handles are sent before the frame that
  announces them** and `Connection::send_with_handles` is that written once.
  `None` means the reading end has gone and a send answers `Gone`.

Two defects the audit of
`specs/issues/kernel/ring0-jump-to-zero-under-port-polls.md` found in chunk 3's
code, both fixed here: `WatcherGuard` unregistered a ring from a source another
poll of the same ring still named (deleted, `io_uring::take_poll` is the one
removal path), and `Acceptor::on_zero_handles` left a thread parked in
`SYS_ACCEPT` on a condition that had become permanently false (it wakes now, and
`sys_accept` answers `Gone`). Two new issues filed:
`specs/issues/kernel/poison-set-holds-one-thread-per-cpu.md` and
`specs/issues/isolation/a-moved-handle-is-always-re-movable.md` — the second is
a property §6.3 assumes and the design does not have.

- **A peer is named by the connection's own handle, because there is no syscall
  that answers a `Koid`.** §6.7a and D3 both say the diagnostic pid becomes the
  connection's koid; §3.1's fourteen numbers contain nothing that reports one,
  and adding a fifteenth is not this chunk's decision. A `RawHandle` does the
  same job with what already exists: it carries a generation, and a closed slot
  is reissued at the next one, so a handle value names one object for the life
  of the process holding it and designates nothing in any other table. That is
  `toyos::surface::ClientId`, the compositor's `dropping client N` and netd's
  `dropping client N`. If the owner wants the koid it is one number and one arm.
- **`ResizeInfo::old_token` is deleted rather than migrated.** It told a client
  which token was being replaced; a client holds its old buffer as a handle and
  needs nobody to name it. No reader ever used it.
- **Two "hold it or the peer cannot map it" statics are deleted** —
  `window::clipboard_set`'s `CLIPBOARD_SHM` and the compositor's `PASTE_SHM`.
  Each existed because a token meant nothing after its owner dropped the region.
  The receiver's own handle is what keeps the region alive now, so the sender
  drops its mapping in the statement after the send.
- **`DropReason::Vanished` is deleted.** Its only producer was
  `grant_shared` naming a pid the process table no longer had. A client that has
  gone is a refused send, which is `Gone`, which the compositor already had.

**Three defects the first green suite found, all in chunk 6's own kernel half.**
Named because none of them could be seen while the tree did not build:

- `SYS_HANDLE_RECV` installed the batch and answered **zero**, so every receiver
  read "the peer sent no handles" while holding them — and leaked one handle per
  message. Every audio client and every window failed on it.
- `SYS_READ_NONBLOCK` had no `Device` arm, so it reached `ops::try_read`'s
  `unreachable!`. That is the path the compositor polls its keyboard and mouse
  on: the desktop died on its first poll.
- `SYS_IO_URING_ENTER` answered `InvalidArgument` for every handle failure, so a
  closed ring and a nonsense argument were one word. It uses the table's own
  words now.

**`metal_sim_client_death`'s non-vacuity witness changed sides**, and the
harness moved with it. It asserted the compositor said "the process behind it
has exited" when a reaped creator's heir asked for a window — the grant that
killed the owner's desktop. There is no grant: the buffer travels over the
connection, the heir holds it, and the request is *served*. The heir's own
report of being served is the witness.

**The `rust` pin was broken from chunk 5 and is fixed here.** It read
`0e27504731a5f6a2f7c9d43e9e40e6b28b56a0e5`, and that commit exists in no
repository. The fork's head at the time was `0e27504731a51efe…` — the same
twelve hex characters, which is why nothing looked twice. It now reads
`d91d5a423708b67f67b3aca99631f0dd085c7d33`, which is pushed and carries this
chunk's `connection_join`.

**Nothing in the tree would have caught it.** A worktree leaves `rust/` an empty
stub by design, so no build here ever resolves the pin, and CI is the first
thing that would — on a branch CI has never run. Recording one from a worktree
is `git update-index --add --cacheinfo 160000,<sha>,rust`, or the same change
applied to the index as a patch; there is no `git add` for a submodule you do
not have checked out. Chunks 7 and 8 both touch the std fork and both have to
do it.

**Chunk 7 — process objects and the fail-fast flip. Green.**
`ProcessObject`/`ThreadObject`; `SYS_SPAWN` returns a handle;
`SYS_PROCESS_WAIT/KILL/OPEN`; `SYS_WAITPID`/`SYS_KILL` retired; the zombie and
orphan machinery deleted; std's `process/toyos.rs`; **`userland/libc`'s
`waitpid` and its `pid → Process handle` map** (§6.5a), which is what stops the
110 C tests going red on a retired syscall number. Bad-handle policy flips to
kill-the-process.

**The flip has an edge this branch does not close.** Killing a process for a bad
handle kills whatever threads of it are parked in a blocking syscall, and a
parked thread's wait-queue registration lives on its own stack (§1.1). The flip
is still right — a handle a process cannot name is a bug in that process and
fail-fast is for bugs — and the leak it produces is the pre-existing one, bounded
and census-visible. `wt/toyos-compl`'s §7 makes that kill clean, and §13 records
that it depends on this branch landing first rather than the other way round.
*Gate: `process_lifecycle`, `handle_kill_policy`, census baselines.*

**Chunk 8 — the test estate. Green.**
Every `tests/toyos-rust-tests/src/bin/` binary that used the deleted surface;
`abuse_listener_hijack` and the pipe-id sweeps rewritten to their general forms;
`connect_before_serve` and `endowment_denied` land here if they have not already.

**Chunk 10 — the adversarial review's merge blockers. Green.**
Not in the plan, because the plan had no place for what twenty green checks
could not see. Added 2026-08-10 after a review of the whole branch fixed six
defects and filed nine; two of the nine blocked the merge and both are the same
sentence one layer apart — *the kernel must never crash from userland, and
neither may the one process the machine cannot lose*.

- **`/bin/init` is an event loop.** `serve_launch`'s first statement was a
  blocking `recv_header` on a fresh connection, so any holder of a `launcher`
  connector — the compositor, every terminal, every shell, sshd — could connect,
  say nothing, and park the machine's only way to create a process, with init
  alive and looking healthy. init is now the shape `userland/netd` and the
  compositor already had: a poller over the acceptor and every accepted
  connection, `ipc::FrameRx` per connection, `MAX_PENDING_LAUNCHES` half-spoken
  launches, a handshake deadline, and every reply a `try_signal` — because a
  blocking *write* is the same defect from the other side.
- **The deferred drain has a stack.** `kobject!`'s `deferred`/`immediate` split
  is per object and a `deferred` container may hold an `immediate` member, so a
  `File` sent over a connection whose peer dies runs `vfs::flush_file` from
  `drain_zero_handles` — which the idle loop calls, on 16 KiB. That is the
  defect `6d81a73` measured and closed at a cost of 147 collateral reds,
  returning through a container, and nothing expressible in the object layer
  stops it: the entries are dropped wherever the drain runs. `IDLE_STACK_SIZE`
  is `KERNEL_STACK_SIZE` now — one stack size for every context Rust kernel code
  runs on — and `debug_action::IDLE_STACK_HIGH_WATER` is the instrument that
  says so from a running machine rather than from a guard page on a halted one.
- **§8.6's three missing gates and the per-kind census**, above.
- Four of the other seven filed issues, all in the same class of *a refusal that
  destroys what it refused*: `SYS_HANDLE_SEND` gives the batch back
  (`HandleTable::transfer`), `PortShared`'s `closed` moved inside the queue's
  lock, `SYS_SPAWN`'s handle refusal travels out as a value rather than ending
  the caller with 8 MB on the frame, and `dup2`'s displaced entry drops outside
  the process's own lock.

**Chunk 9 — audit, deletion and documentation. Green.**
The grep gate (§8.4); `[u32; 64]` sized from the ABI; dead constants and any
surviving `_ =>` arms in object-layer code; the twelve closed `specs/issues/`
files deleted and the six re-scoped ones rewritten; `specs/capability-handles-spec.md`
annotated with what this delivered and what it did not; `forks.toml` deltas;
**one line** in the root `CLAUDE.md` — it has 2,678 bytes of headroom against its
40,000 budget (`wc -c CLAUDE.md` → 37,322), so anything longer displaces
something else and `src/docs.rs`'s budget test will say so. Detail goes in
`userland/CLAUDE.md` and `kernel/CLAUDE.md`, which have 5,471 and 5,387 bytes
spare.

Two things about that paragraph that `src/docs.rs` will enforce and the numbers
above do not show:

- **`TOTAL_BUDGET` is the tighter constraint.** The five files together are
  80,000 bytes and weigh **74,197** today, so the whole set has **5,803** bytes
  spare — less than the 2,678 + 5,471 + 5,387 the per-file figures suggest. Three
  additions cannot each spend their own headroom.
- **Deleting an issue file is not one edit.** `every_named_issue_file_resolves`
  walks every `.md`, `.rs`, `.toml`, `.yml`, `.sh`, `.json` and `.txt` in the tree
  and reds on any `specs/issues/<area>/<slug>.md` that no longer exists. Each of
  the ten deletions has to take every reference with it — including the ones in
  `CLAUDE.md`, in other specs, and in source comments — and the sweep for those
  is part of the deletion rather than a follow-up.
*Gate: full `cargo test`, every host test suite named in the root `CLAUDE.md`,
`cargo test --lib`.*

**Ordering constraints, stated so they are not rediscovered.** Chunk 0 before
everything. Chunk 1 before 2. Chunk 2 before 3 (ports are objects). Chunks 3 and
4 are one non-green stretch and must be committed as such. Chunk 6 may precede 5
in principle but should not: chunk 5's gate is the first place the desktop boots
prove the architecture, and it is worth reaching early. Chunk 7 is independent of
5 and 6 and could move, but it changes std again and batching the two std
touches is worth more than the parallelism.

**One constraint runs the other way and is the reason chunk 5 can be green at
all: `SYS_ACCEPT`'s client pid must outlive chunk 5 and die in chunk 6.** Chunk
5 brings the compositor and soundd onto endowed acceptors while both still grant
shared memory by pid; chunk 6 is where the grant becomes a transferred handle.
Remove the pid in chunk 3 as the first draft of §3.3 said and there is no green
chunk 5 — the two daemons have a client and no way to hand it a buffer.

---

## 10. Owner-level decisions

Everything not listed here is a recommendation the implementing agent follows
without stopping.

### 10.1 The branch holds the shared sysroot for its whole life — **his call**

Its `toyos-abi` and `toyos` genuinely differ from main's from chunk 1 onward, so
it must claim, and while it holds the claim **every other worktree is refused and
none of them can fix that from its end** (`specs/worktrees.md` §3.1–§3.2; the
refusal is `Standing::MatchesMain`'s `panic!` at `src/toolchain.rs:638`). This is not a week-long claim in the
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
  (`build/std-change-needs-an-unlanded-abi-change`). Recommended:
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

28 days from the day this spec was written. `Stale::OnThisDate` is evaluated
once against `Day::today()` when the tally is built and reds the run at its end
regardless of whether either test executed, so a branch still open on that date
goes red on two exemptions it did not cause and cannot legitimately extend: the
rule is that an entry must be able to fail the build by itself, and moving its
date to make a red go away is exactly what the rule forbids.

Three answers, none of them an agent's: land before it; extend both dates on
their own merits when the day comes, which is a review of #156 and #88 rather
than of this branch; or accept a red tail and read it correctly. **Recommended:
plan chunks 0–5 to land the desktop gate well inside the window, and treat the
date as a real deadline rather than as something to renegotiate.**

### 10.4 Not his call, recorded so he can overrule

Three places where I chose against a literal reading, each argued in §11: the
child is born holding a **connector** rather than a pre-made connection (D1);
`accept` no longer reports the peer's pid (D3, which is a one-line reversal in
the ABI and two `shm.grant` call sites that keep their pid); `SYS_THREAD_JOIN`
keeps its `Tid`
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
leaves the pid in place. A peer's identity asserted by the kernel is exactly the
designation-as-capability shape this work deletes, and a server that wants to
name its client reads the protocol's first frame, which is already the client's
own claim about itself and already distrusted.

Two callers do authorize on it today and they are the ordering constraint, not a
counterargument: the compositor's `shm.grant(pid)`
(`userland/compositor/src/session.rs:859`) and soundd's
(`userland/soundd/src/main.rs:627`). Both are replaced by `SYS_HANDLE_SEND` in
chunk 6 and the pid is deleted in that same commit (§3.3). The rest of its use is
diagnostic — 87 lines across four files name a pid — and it becomes the
connection's own `RawHandle`, which is a name in one process's table and not
something anyone can present anywhere else. §6.7a says why that rather than the
`Koid` this paragraph first asked for.

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
it, which init can do because it holds the `Process` handle for what
`[boot] start` named.

**Restarting a daemon so its existing clients can reach it** — §5.3 states the
regression plainly and this is the mechanism it names. `SYS_PORT_REARM(port_h)`
mints a fresh `Acceptor` for an existing `PortShared` and clears `closed`; the
right that gates it rides on the handle init keeps when it endows the acceptor,
so only the process that created the port can re-arm it. Every namespace already
points at that `PortShared`, so a re-armed port is reachable by every client
whose namespace predates the crash — which is exactly what is lost today. **113
is its number** and this delta stops at 112 so it stays free. Not built here
because nothing in the tree supervises a daemon, and a re-arm with no caller is
a mechanism built for a plan.

**`SYS_SYSINFO`/`SYS_SCHED_INFO`** — read-only, ambient, and a read surface is
`specs/introspection-plan.md`'s subject rather than this one's.

---

## 13. Coordination with the other open plans

Three specs are in flight against the same ABI and two of them do not know the
state of this one. Every row here was checked against the branch as pushed, not
against what a spec says about it.

### 13.1 `wt/toyos-compl` — the completion architecture

`origin/wt/toyos-compl` at `57d6baf`, `specs/completion-architecture-spec.md`,
1,136 lines, spec-only. Its §15 says *"`wt/toyos-endow` was not pushed to
`origin` as of 2026-08-09 … the implementing agent must re-read
`specs/capability-endowment-spec.md` from the endowment branch before chunk C0"*.
It is pushed now, at `f53a8de` and at whatever this branch's head becomes, and
that instruction is the one to follow.

| what it says | how it stands against this spec |
|---|---|
| **§14.2: 99–115 is the endowment architecture's, 116–127 is its own, no retired number reused by either** | Holds. This delta is 99–112, fourteen numbers; **113 is reserved for `SYS_PORT_REARM` (§12) and 114–115 are free.** It allocates nothing at 116 or above, and it retires thirteen numbers none of which that spec touches |
| It expects a `SYS_THREAD_JOIN_H` in that block | **Not delivered.** D5 keeps `SYS_THREAD_JOIN`(41) and its `Tid`, so no number is spent on one. Its reservation is simply unused |
| **§15 row 1: `ProcessData` becomes a `SleepLock`** | Compatible. `HandleTable` lives inside `ProcessData` behind the existing lock and `get` is lock → clone → unlock, so nothing borrows the table across a park |
| **§15 row 2: `Rights::WAIT` is the seam `completion::arm` needs** | Delivered. §1.4 carries `WAIT` and §3.1 gives it callers |
| **§15 row 8: "their §5.1 no-Arc-across-block interim rule is retired by our cancellable park. *Tell them.*"** | Taken. §1.1 states the stranded-`Arc` leak as accepted, bounded and census-visible, and does not build a workaround for it. Its cancellable park removes the class after this lands; nothing here should anticipate it |
| **§15 row 12: the bad-handle kill-process flip needs its §7 to be safe from a parked thread** | Chunk 7 flips it before that exists. The residual is the pre-existing wait-queue leak (§1.1), not a new one, and the flip is not held for it |
| **Landing order: this branch first** | Its own §15 says so, and its C0 merges `origin/main` after |

### 13.2 `specs/introspection-plan.md` — stale, and it collides

That plan allocates `SYS_QUERY = 97`, `SYS_LOG_READ = 98` and
`SYS_DISK_ADOPT = 99` (`specs/introspection-plan.md:78`, `:414`, `:694`, restated
together at `:824`). **All three are wrong on today's tree**: 97 and 98 are
`SYS_DEVICE_REG_READ`/`SYS_DEVICE_REG_WRITE`, allocated after that plan was
written, and 99 is `SYS_ENDOWMENTS` here. Its allocation must be re-based off
**116 or above** — 99–115 is spoken for by this spec and `wt/toyos-compl`'s
§14.2, and 116–127 is that spec's. Nothing in this branch fixes that plan; this
row exists so the next agent to implement it does not build a third
`SYS_DISK_ADOPT = 99`.

### 13.3 `specs/capability-handles-spec.md`

Not a parallel track — this work realizes it, §11 lists the seven deviations, and
chunk 9 annotates it with what was delivered. The one thing to carry forward: its
§12.2 stage-F grep gate is landed early here as §8.4, so a later reader of that
spec should not expect stage F to still be owed.
