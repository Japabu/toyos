---
status: open
kind: track
opened: 2026-08-20
---

# The capability end-state is twelve answers, written before APIs answer them accidentally

From the external review of 2026-08-20, adopted by the owner: the tree is
making strong capability commitments, and postponing the conceptual model
lets dozens of local API decisions harden into architecture by accident. The
end-state is written down here — as this tracked document, since the tree
keeps no spec corpus — with each answer either **COMMITTED** (the tree already
enforces it; the enforcing site is cited) or **OPEN** (an owner ruling is owed
before any interface constrains it). **Until each answer exists, an interface
change that would answer one silently is stopped and the question surfaced
instead.**

The audit pass that first work item asked for was run on **2026-08-20**, from
the code, against `syscall_dispatch`'s every arm
(`kernel/src/arch/syscall.rs:275`-`860`), every `HandleEntry` constructor, both
move paths and the boot's one capability mint. Eight answers are committed,
four are open.

## 1. What constitutes authority? — COMMITTED

One `HandleEntry` — an object reference plus a `Rights` word — in one process's
own table (`kernel/src/object/handle.rs:142`), reached only through a
`RawHandle`, which "designates nothing outside that table: a number lifted out
of another process's log, or counted up from zero, resolves to that process's
own slot or to nothing at all" (`toyos-abi/src/handle.rs:1`). Nothing else in
the ABI bears authority: a `Koid` is "**Never an authority**: no syscall turns a
koid into access to anything" (`kernel/src/object/mod.rs:74`), and an endowment
label is "a *local name* in one process's own table and buys nothing to guess"
(`toyos-abi/src/syscall.rs:315`). Every syscall states the right it needs at its
own arm — `Rights::NONE` where the handle alone is the authority
(`kernel/src/arch/syscall.rs:309`, `:442`, `:1032`) — because "a right left
unstated is a right each call site invents" (`toyos-abi/src/handle.rs:53`). The
qualification is question 5's ambient set.

## 2. Is every authority ultimately derived from a handle/capability? — OPEN

**Two sites disagree.** The root `CLAUDE.md`'s Capabilities paragraph says "a
process holds exactly what its parent moved into it, and there is nothing it can
name to get more". The dispatch does not: `SYS_OPEN` resolves any absolute path
against the machine's one VFS (`kernel/src/arch/syscall.rs:1233`), gated only by
a per-*mount* `user_may_modify` whose whole subject is protecting `/boot`
(`kernel/src/vfs.rs:288`); `SYS_SPAWN` starts any binary on that filesystem by
path (`kernel/src/arch/syscall.rs:359`); `SYS_DLOPEN` loads any `.so` by path
(`:504`); `SYS_SHUTDOWN` powers the machine off with no handle and no right
(`:328`). So the answer today is **no**, and the sentence in `CLAUDE.md` is true
of kernel objects and false of the filesystem.

**Smallest decision:** is the filesystem inside the capability model or
deliberately outside it? **Recommendation: deliberately outside, and say so.** A
directory capability would be a second namespace mechanism beside `Namespace`
for no caller that exists, and the ambient path space is what makes `/bin/init`
able to start `/bin/toybox` at all. What does not follow from that ruling is
`SYS_SHUTDOWN`: it is not path authority, and it is separable — see
`issues/isolation/shutdown-needs-no-capability.md`.

## 3. Are PIDs and TIDs identity-only, or can naming one confer authority? — COMMITTED

Identity-only, with one named exception that is itself gated. Every arm taking a
pid: `SYS_GETPID` answers the caller's own
(`kernel/src/arch/syscall.rs:490`), and `SYS_PROCESS_OPEN` turns a pid into a
`Process` handle only when the caller also presents a `SysCap` carrying
`Rights::MANAGE` (`:1602`), which the kernel mints once, for `/bin/init`
(`kernel/src/loader/mod.rs:938`). `ProcessStats.pid` says so at the field: "Not
authority — nothing takes a pid but `SYS_PROCESS_OPEN`, which takes a `SysCap`
beside it" (`toyos-abi/src/syscall.rs:1803`). Tids are process-local names:
`SYS_THREAD_JOIN` resolves through `thread_sched(caller, tid)` and
`collect_thread_zombie(table, tid, parent_pid)`, both keyed on the caller's own
pid (`kernel/src/arch/syscall.rs:2393`, `kernel/src/process.rs:1412`, `:848`).
Four pid-addressed syscalls were deleted and their numbers retired rather than
reused — 26 `SYS_WAITPID`, 33 `SYS_FIND_PID`, 37 `SYS_GRANT_SHARED`, 65
`SYS_KILL` (`kernel/src/arch/syscall.rs:63`).

## 4. Can a process enumerate objects it lacks authority over? — OPEN

The object graph is clean. No syscall lists another process's handles; a
`Namespace` answers `lookup` and has no listing operation at all
(`kernel/src/object/namespace.rs:59`); `SYS_ENDOWMENTS` answers the caller's own
table (`kernel/src/arch/syscall.rs:1707`); reading the machine's log is gated on
`Rights::LOG` (`:1683`) precisely because it is "every process's business and no
process's right by default"; the per-kind object census is `SYS_DEBUG`, which a
shipping kernel does not have (`:704`, `:792`).

Against that, **`SYS_SYSINFO` is a census with names.** It walks
`PROCESS_TABLE` and writes pid, tid, state, resident memory, CPU time and the
28-byte *name* of every process and thread in the machine, taking no handle and
no right (`kernel/src/arch/syscall.rs:2472`-`2554`, name at `:2550`); `ps` is a
toybox applet any process can run (`userland/toybox/src/ps.rs:28`).
`SYS_READDIR` enumerates any directory by path, which is question 2's ambient
VFS rather than a separate hole. Filed:
`issues/isolation/sysinfo-enumerates-every-process.md`.

**Smallest decision:** does `SYS_SYSINFO` need a right? **Recommendation: yes**
— one more `SysCap` bit (bits 10..31 of `Rights` are free), endowed by
`system.toml` to whichever program is meant to carry `ps`, exactly as `logread`
is endowed to `logd` today.

## 5. What ambient authority intentionally remains, if any? — OPEN

Nothing states the set, so nothing is *intentional* yet. Classifying every arm
of `syscall_dispatch` by what it demands, a process with an **empty handle
table** can still reach:

- **its own execution** — `SYS_EXIT`, `SYS_THREAD_EXIT`, `SYS_THREAD_SPAWN`,
  `SYS_THREAD_JOIN`, `SYS_FUTEX_WAIT`/`WAKE`, `SYS_NANOSLEEP`,
  `SYS_SET_THREAD_NAME`;
- **its own address space** — `SYS_MMAP`, `SYS_MUNMAP`, `SYS_TLS_ALLOC_BLOCK`,
  `SYS_STACK_INFO`;
- **its own record** — `SYS_GETPID`, `SYS_GET_ENV`, `SYS_ENDOWMENTS`,
  `SYS_QUERY_MODULES`, `SYS_SCHED_INFO`, `SYS_GETCWD`, `SYS_CHDIR`;
- **machine facts** — `SYS_CLOCK`, `SYS_CLOCK_REALTIME`, `SYS_CLOCK_EPOCH`,
  `SYS_CPU_COUNT`, `SYS_RANDOM`;
- **object creation, which confers nothing over anything that exists** —
  `SYS_PIPE`, `SYS_PORT_CREATE` ("needs no right and grants none — a port with
  no clients is not authority", `kernel/src/arch/syscall.rs:1731`),
  `SYS_SHM_CREATE`, `SYS_INBOX_SETUP`;
- **and four that reach past the process** — the whole filesystem by path
  (`SYS_OPEN`, `SYS_READDIR`, `SYS_DELETE`, `SYS_MKDIR`, `SYS_RMDIR`,
  `SYS_RENAME`, `SYS_SYMLINK`, `SYS_READLINK`), `SYS_DLOPEN`/`SYS_DLSYM` over
  the same paths, `SYS_SPAWN`, and `SYS_SHUTDOWN` — plus `SYS_SYSINFO`'s census.

**Smallest decision:** the last group. **Recommendation:** keep the filesystem,
`SYS_DLOPEN` and `SYS_SPAWN` ambient under question 2's ruling; make
`SYS_SHUTDOWN` and `SYS_SYSINFO` rights-bearing, because neither is path
authority and both are machine-wide. Then this list, minus those two, is the
committed answer.

## 6. Can rights ever increase after delegation? — COMMITTED

**No, and the type has no widening primitive** (`toyos-abi/src/handle.rs:53`).
Every path that puts an entry in a table was walked. The only rights-taking
constructor a caller can reach is `HandleEntry::duplicate`, which refuses
without `Rights::DUP` and refuses a set that is not `subset_of` the source's
(`kernel/src/object/handle.rs:170`); `SYS_HANDLE_DUP`
(`kernel/src/arch/syscall.rs:2596`) and `SYS_HANDLE_DUP_AT` (`:2624`) both go
through it, so a device claim — created with no `DUP`
(`kernel/src/object/ops.rs:47`) — cannot be copied at either. The five
`HandleEntry::new` sites are all fresh objects, never a re-rating of a held one:
`ops::install`/`ops::open` take `initial_rights` (`kernel/src/object/ops.rs:79`,
`:160`), `install_buffer` a fixed `MAP|DUP|TRANSFER`
(`kernel/src/object/device.rs:54`), `spawn_init` the machine's root cap
(`kernel/src/loader/mod.rs:954`), and `build_child_handles`'s per-child console
mint carries the **parent's** rights on the handle it replaces
(`kernel/src/loader/start.rs:341`). Both move paths carry rights unchanged:
`HandleTable::transfer` (`kernel/src/object/handle.rs:446`) and
`PendingHandles::commit` (`kernel/src/loader/start.rs:230`). The one live
consequence — unchanged means `TRANSFER` survives a move, so nothing can be
delegated un-re-delegatably — is
`issues/isolation/a-moved-handle-is-always-re-movable.md`, which is a bound on
delegation and not a widening.

## 7. What authority is implicit in process creation? — COMMITTED

A child starts with an empty `HandleTable` and gets exactly two vectors:
`slot_map` *duplicates* (the parent keeps its copy) and `endow` *moves*
(`toyos-abi/src/syscall.rs:283`). Both are all-or-nothing and refuse by name — a
slot-map pair naming a handle the parent does not hold ends the parent
(`kernel/src/loader/start.rs:308`), an endowed handle without `TRANSFER` refuses
the spawn before anything leaves the table (`:206`), and the move is the last
thing a spawn does (`kernel/src/loader/mod.rs:649`). **Three things are implicit
and nothing else is:**

1. a `Console` slot is *minted fresh* rather than duplicated, at the parent's
   rights, because the object is the line buffer
   (`kernel/src/loader/start.rs:325`);
2. the child's cwd is the **caller's** cwd — `SpawnArgs` has no field for one
   (`kernel/src/arch/syscall.rs:1530`, `toyos-abi/src/syscall.rs:292`);
3. the spawner is handed a `Process` handle carrying
   `DUP|TRANSFER|WAIT|READ|MANAGE` by type (`kernel/src/object/ops.rs:74`), and
   must dup-narrow and close if it wants to hold less.

Beyond that a child holds nothing, plus question 5's ambient set, which every
process has.

## 8. Which object types are transferable? — COMMITTED

**All thirteen, by exactly two mechanisms.** `kobject!` declares thirteen kinds
(`kernel/src/object/mod.rs:278`); twelve get `Rights::TRANSFER` from
`initial_rights` (`kernel/src/object/ops.rs:34`), and the thirteenth, `SysCap`,
is created with `Rights::NONE` there because "every bit on a `SysCap` is an
authority init decides per program" — the machine's only one is minted with
`TRANSFER` at `kernel/src/loader/mod.rs:946`. The mechanisms are
`SYS_HANDLE_SEND`/`SYS_HANDLE_RECV` over a `Connection`, where the connection
and every handle must carry `TRANSFER`, no handle may be named twice, and the
connection may not be sent over itself (`kernel/src/arch/syscall.rs:2086`,
`:2090`); and `SpawnArgs::endow`. Nothing else crosses a process boundary — the
handles `install_buffer` writes are the kernel handing a claim's holder its own
buffers, not a transfer (`kernel/src/object/device.rs:49`).

## 9. Are threads intended to become independently controllable first-class kernel objects? — OPEN

**Nothing in the tree has decided it.** There is no `Thread` row in `kobject!`
(`kernel/src/object/mod.rs:278`), so a thread is not something a handle can
name. `SYS_THREAD_SPAWN` answers a bare `Tid`
(`kernel/src/arch/syscall.rs:2376`), `SYS_THREAD_JOIN` takes one and resolves it
only inside the caller's own process (`:2393`), there is no thread-kill and no
cross-process thread operation, and a `Process` handle's `MANAGE` retires every
thread at once (`kernel/src/process.rs:1960`). This is the shape that resulted
from `Process` being the only lifecycle object, not a stated position.

**Smallest decision:** does anything need to name a thread it did not create?
**Recommendation: no, and record it as declined.** The two callers that would
want one do not exist — there is no debugger, and
`issues/kernel/cpu-time-is-a-band-and-not-a-reservation.md` prices its entities
per real-time client and per CPU rather than per thread. A `Thread` object costs
a fourteenth `kobject!` row plus a rights vocabulary for it; reopen when a
caller exists.

## 10. How is device authority delegated? — COMMITTED

Four sites, and they cannot disagree. `system.toml` declares `devices` per
program; `src/build.rs:1923` refuses a config where two programs name one class
and `src/build.rs:1953` refuses a class the ABI does not have, so arbitration is
a build-time fact rather than a runtime race. At boot the kernel mints one
full-rights `SysCap` for `/bin/init` and nothing else can construct one
(`kernel/src/loader/mod.rs:938`). init reads the manifest and calls
`SYS_DEVICE_CLAIM` per declared class (`userland/init/src/main.rs:506`), which
demands `Rights::DEVICE` on a `SysCap` (`kernel/src/arch/syscall.rs:1627`) and
takes the class exclusively (`kernel/src/device.rs:56`). The claim comes back
**without `Rights::DUP`**, so endowing it is the only expressible form and init
provably no longer holds it (`kernel/src/object/ops.rs:47`). Every
device-driving syscall then presents that handle and the kernel checks the
*class*, not merely the type: "a process holding the NIC has no more business
setting the resolution than one holding nothing"
(`kernel/src/arch/syscall.rs:895`). `SYS_RT_ENTER` and `SYS_PROCESS_OPEN` are
the same shape on `Rights::RT` and `Rights::MANAGE` (`:1655`, `:1602`), narrowed
per program by `toyos_manifest::syscap_rights`
(`toyos-manifest/src/lib.rs:73`) — `system.toml` grants exactly two, `logread`
to `logd` and `rt` to `soundd`.

One inconsistency, known and unobservable: three arms demand three different
rights on the same claim handle — `Rights::WRITE`
(`kernel/src/arch/syscall.rs:902`), `Rights::READ` (`:1136`) and `Rights::NONE`
(`:1032`). No claim handle can ever carry a narrower set, because narrowing
needs `DUP` and a claim has none, so the three are the same test today.

## 11. Are namespaces capability objects or ambient process state? — COMMITTED

**Capability objects.** `Namespace` is a `kobject!` row
(`kernel/src/object/mod.rs:296`), immutable once built — "no insert, no remove,
no replace", and a narrower one is a *new* object built from an existing one
(`kernel/src/object/namespace.rs:1`). `SYS_NAMESPACE_OPEN` demands
`Rights::READ` on a namespace handle and there is no second place to ask
(`kernel/src/arch/syscall.rs:1861`); `SYS_NAMESPACE_BUILD` demands
`Rights::TRANSFER` on every added connector and resolves kept names against the
base before installing anything (`:1754`). A process with no `svc` endowment
resolves no name at all, and there is no registry to fall back to: 85
`SYS_LISTEN` and 87 `SYS_CONNECT` are retired numbers
(`kernel/src/arch/syscall.rs:77`, `:78`). The **filesystem** path space is the
other thing the word could mean, and it is ambient process state
(`kernel/src/arch/syscall.rs:1234`) — questions 2 and 5 hold that half.

## 12. Is CPU time an explicit schedulable/budget authority, or is process-level fair scheduling sufficient? — OPEN

Held by `issues/kernel/cpu-time-is-a-band-and-not-a-reservation.md`, which
already carries the commitment and the measurements; nothing is added here.
Today the only CPU-time authority is a band: `SYS_RT_ENTER` puts the calling
process in the real-time band on `Rights::RT`
(`kernel/src/arch/syscall.rs:1653`), with no budget, no period and no admission
— "no entity is promised anything a number can check", measured at 93.3 ms of
audio starvation behind a fair storm.

## The ruling set

Four decisions, in one place, for the owner:

1. **The filesystem** (question 2) — inside the capability model or deliberately
   outside it. Recommended: outside, stated.
2. **`SYS_SHUTDOWN`** (questions 2 and 5) — ambient or rights-bearing.
   Recommended: rights-bearing.
3. **`SYS_SYSINFO`** (questions 4 and 5) — ambient or rights-bearing.
   Recommended: rights-bearing, one more `SysCap` bit.
4. **Threads as objects** (question 9) — intended or declined. Recommended:
   declined until a caller exists.

Question 12 is not in this set: its track already holds it.

## Kernel-resident workers

**Kernel-resident workers are a control-flow boundary, not a memory boundary** —
each one is audited periodically, exists only where independent blocking
progress, fault containment of execution flow, or latency isolation requires it,
and a new one needs explicit architectural justification; work moves to
userspace when the IPC/wait machinery makes that an isolation gain rather than
overhead.

**Census of 2026-08-20.** `sched::kthread` caps a shipping machine at three and
dies naming a fourth (`kernel/src/sched/kthread.rs:50`, `:102`). All three
exist, and all three are started at the end of `kernel_main`
(`kernel/src/main.rs:696`-`703`):

- **`klogd`** — the machine's only console drainer, "one thread where every idle
  CPU used to drain". `OnPanic::Halt`, because "a machine whose only console
  drainer has been killed goes silent with nothing left able to say so"
  (`kernel/src/log/console.rs:1`, spawn at `:116`).
- **`usbd`** — owns the xHCI port machine so USB work runs in a context of its
  own instead of whichever thread trapped: "a stuck USB enumeration must not
  stop the log". Spawned on every machine including one with no controller, at
  one kernel stack, so the machine has one answer to how many kernel threads it
  has. `OnPanic::Recover`, because every loss it causes is visible
  (`kernel/src/drivers/xhci/usbd.rs:1`, spawn at `:47`). Its body is one park
  today — nothing posts to it yet.
- **`iod`** — owns the deferred write-back queue, because `OpenFileState::drop`
  must flush under a lock that is becoming a sleep lock and a `Drop` impl cannot
  hold a `Parkable`. `OnPanic::Recover`, because a killed `iod` costs deferred
  write-back and both `SYS_FSYNC`'s error path and `/bin/logd`'s give-up policy
  can see that (`kernel/src/iod.rs:1`, spawn at `:52`). Its body is one park
  today — nothing pushes yet. **One `iod` machine-wide is a decision with a
  measurement owed** at the 128-core target, recorded at its own site
  (`kernel/src/iod.rs:24`).

Two more exist only on a `boot-actuators` kernel and are stimulus rather than
workers: `lognest`, one thread (`kernel/src/log/nested.rs:68`), and `logstorm`,
one per log shard (`kernel/src/log/storm.rs:115`) — which is why the cap is
`3 + MAX_LOG_SHARDS` on that build and 3 on a shipping one
(`kernel/src/sched/kthread.rs:50`).

**Verdict:** all three are justified at their site, each by independent blocking
progress or fault containment of execution flow, and each states its panic
policy. None is a memory boundary. Nothing is owed to userspace yet; the sleep
locks the two idle bodies are waiting for are
`issues/kernel/every-wait-in-this-kernel-is-a-spin.md`.

**PID-backed pseudo-processes for kernel workers** are pragmatic today — a
process-table row is what makes a kernel thread nameable in `ps`, in
`sched::dump` and in a crash report, and every field of it is the empty value
rather than a plausible one (`kernel/src/sched/kthread.rs:291`). The moment that
representation leaks misleading user-process semantics into policy,
observability, lifecycle or APIs, identity/accounting separates from
user-process semantics rather than preserving the abstraction for convenience.

## The rest of the review's standing rules

- **The adversarial handle-lifecycle suite** the review lists — stale-handle
  behavior, rights reduction and duplication, cross-process transfer, table
  exhaustion, teardown with references in flight, and every path where a
  numeric identifier might become authority — is measured against what
  `handle_kill_policy`, `abuse_handle_table`, `handle_lifetime` and the
  census arm already cover, and the gaps become tests.
- **Zero-handle hooks stay mechanically constrained** — the drain sites'
  "no hook may take a sleep lock" doc constraint wants enforcement the
  compiler or an assert can see, per the review's finalization-path point.
