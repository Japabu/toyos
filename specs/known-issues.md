# Known issues

Every open defect, in full. CLAUDE.md carries a one-line summary of each of these
under "Known issues" and points here for the detail; keep the two in step. An
entry leaves this file when the code and `git log` carry the fix — resolved
narrative belongs in a dated investigation doc, not here.

Verified against `a88e4ee` (2026-07-30); §2's panic-path additions and §8's
display entry against `883a84d` (2026-07-31); §8's three metal-sim entries
against M1 (2026-07-31); §1's and §3's allocation-sizing entries against
`a6935c6` (2026-07-31), from the sweep that followed the T14's first boot.

---

## 1. Isolation and untrusted input

### THE CLASS: an id or a name treated as a capability

Three separate defects in this file are one defect. A `PipeId`, a service name
and a `SharedToken` are all *designations* — they say which object you mean. None
of them says you are allowed to have it. Where the kernel accepted a designation
as authority, guessing or outliving the designation was the entire attack:

- **`PipeId`** — dense sequential integers, so `for id in 0.. { pipe_open(id, 0) }`
  walked every live pipe. Gated at `be604ef` (below).
- **A service name** — **the instance that motivated this class.**
  `Descriptor::Listener` held the service *name*, and every operation re-resolved
  it through the global registry, so nothing tied a descriptor to the listener it
  was created for. `listen("compositor"); dup(fd); close(fd)` freed the name while
  leaving the dup live: the real compositor's `listen` then *succeeded*, its own
  "already running" check passing, and from that moment the attacker's stale fd
  took connections meant for it. Three calls, no race, no privilege. Closed at
  **`e42532f`** (2026-08-01) by storing a `ListenerId` — never reused, so a
  removed id names nothing forever — with `abuse_listener_hijack.rs` as a real
  exploit test.

  Not closed by `be604ef`, which this file briefly claimed: `Listener(String)`
  is an unchanged *context* line in that commit's own `fd.rs` hunk. See the
  postscript at the end of this section.
- **`SharedToken`** — a bare `u32` with no RAII and no ownership, still open
  (§7).

The adjacent failure, same root: **a reference that outlives the object it
names.** `FileBacking` after an unlink is the live instance (below) — the
reference stays valid-looking while the thing it designates is freed and reused
underneath it. Guessing a designation and outliving one are the two ways a name
gets you something you were never given.

`specs/capability-handles-spec.md` exists to make both unrepresentable: a handle
carries rights, so possession *is* the authority and there is no id left to
guess; and it is a refcount on a kernel object, so the object cannot be freed
while a handle can still reach it. Until then, every new syscall taking a raw id
needs the first question asked, and every cached reference to a filesystem or
device object needs the second. **This is here to predict the next instance, not
to summarise the last four.**

> **Postscript, worth more than the entry above it.** On 2026-08-01 this file
> briefly recorded the listener defect as already closed by `be604ef`, citing a
> doc comment and a type that were sitting in the working tree — the isolation
> agent's fix, written twenty minutes earlier and not yet committed. Read against
> `git show HEAD:kernel/src/fd.rs`, the descriptor still held a `String` and the
> attack still ran.
>
> **In a tree with six agents committing, the working tree is somebody's
> uncommitted opinion. `git show HEAD:<path>` is the arbiter.** A finding has a
> shelf life in *both* directions: it can go stale because the bug got fixed, and
> it can look fixed because someone's work-in-progress is on disk. Both cost a
> wrong conclusion here in one day. Method: `specs/spec-staleness-sweep.md`.

### A `FileBacking` outlives deletion of the file it reads

Unlink a file while a process is still demand-paging it and the backing keeps
serving reads.

On `/tmp` this is a correctness wart: `copy_page_out` returns zeros, so the
process faults in blank pages.

On `/home` it is an **information disclosure**. `NvmeBacking` holds
`extents: Vec<Extent>` captured at open (`file_backing.rs:28-31`) and
`read_page` turns a file offset into an absolute block and calls
`page_cache::raw_block_read` with **no re-validation that the block still belongs
to this file** (`file_backing.rs:53-68`). Unlink returns those blocks to
bcachefs's `BitmapAllocator`, another file allocates them, and the stale backing
reads whatever is there now. A process can read another process's file contents
through ordinary filesystem operations — no crafting, no crafted image, no
privilege.

Found by the filesystem owner while implementing tmpfs `open_backing`. **Not
introduced by that work** — the `/home` half predates it.

**This wants capability-handle refcounting, not a local patch.** The backing must
keep the file's blocks alive for as long as it can read them, and that is exactly
the refcounted-kernel-object property `specs/capability-handles-spec.md`
provides. A local fix — re-validating extents on every read, or invalidating
backings on unlink — reimplements refcounting badly at one call site while every
other cached reference keeps the same shape. Unassigned deliberately: it should
be done with that spec, not before it.

### Process isolation does not hold: what is still ungated

`be604ef` (2026-07-28) closed the headline. `SYS_PIPE_OPEN` now requires that the
caller created the pipe, already holds a descriptor for it, or holds a live
socket to the creator; `SYS_SOCKET_CREATE` must already hold both ends in the
right direction; `SYS_PIPE_MAP` is gated derivatively because it takes an fd
rather than an id. `tests/toyos-rust-tests/src/bin/abuse_pipe_owner.rs` is a real
exploit test — it sweeps ids 0..256 skipping its own, asserts every live foreign
pipe is refused, and asserts non-vacuity so it cannot pass by finding nothing.

**It is a relationship check, not a capability, and it has a stated residual:
a peer entitled to one of a creator's pipes is entitled to all of them.** A
compromised daemon can still walk its peer's other pipes. That is the part to
carry forward now that the alarming sentence is gone — a stopgap's known
remainder is exactly what gets forgotten once the headline is fixed.

Still ungated, in rough order of damage:

- `SYS_LISTEN` — no namespace, so the first process to claim a well-known name
  impersonates that service.
- `SYS_GRANT_SHARED` — **narrowed, not retired: an owner cannot withdraw a grant
  it has made.** Two of the three original clauses are closed by `e7d842f`:
  `grant` is owner-only (`shared_memory.rs:179-181` rejects a non-owner, so a
  grantee cannot re-grant) and the target must name a live process
  (`syscall.rs:1096-1102`). `abuse_shared_grant.rs` and `shm_release_reclaims.rs`
  cover both.

  **The "no revoke" clause survives, and this file briefly retired it by mistake
  — on `release`, which is a different operation.** `release` is a grantee
  dropping *its own* access: `sys_release_shared` passes
  `process::current_process()` (`syscall.rs:1128`) and no syscall lets a caller
  name another pid. `destroy` is owner-only but removes the region for everyone,
  which is not withdrawal of one grantee. **Nothing lets an owner revoke a
  specific grantee**, against its wishes, possibly while mapped.

  Deliberate, and currently sound: with `grant` owner-only, the set that can ever
  map is exactly the set the owner named, so revocation has no caller today.
  `specs/capability-handles-spec.md` §14.5 rejects unmap-others by name, and
  unmapping a running process's pages is a second instance of the
  `gpu::set_resolution` hazard — freeing memory while a consumer may hold
  pointers into it.

  **It stops being sound the moment the reachable set is no longer exactly what
  the owner named** — if delegation or re-grant is reintroduced, or when
  `SYS_HANDLE_SEND` makes a grant transferable. Revisit it then, not before.
- `SYS_SET_KEYBOARD_LAYOUT`.

`a88e4ee` gated the GPU present/cursor path, `SYS_AUDIO_SUBMIT`, the NIC
RX/TX path and `SYS_SET_RT_PRIORITY` on `device::is_owner`. Each of the above is
a one-line gate of the same shape, but they need a decision first: which of them
should instead fall out of capability handles
(`specs/capability-handles-spec.md`)? `device.rs` records five owner PIDs and,
until `device::is_owner` was added, nothing outside `release` ever read them —
this is a class, not an instance.

Those gates are exactly as strong as the claim and no stronger. `SYS_OPEN_DEVICE`
is itself first-come and ungated, so a process that beats the daemon to a device,
or claims it after the daemon dies, holds everything the claim unlocks — for
`DEVICE_AUDIO` that includes the RT band, which audio spec §9.4 wants to be a
privilege. "Gated" here does not mean "privileged".

### `SYS_DEBUG` is ungated, and two of its actions are a diagnostic-channel DoS

Action 3 — halt every CPU — no longer exists outside the `test-fatal-halt`
feature. The other three are still reachable by any process at any time, and
the audit that removed action 3 turned up what they cost:

- **0 and 1** (`panic!`, and a null read that faults in kernel context) each
  run a full `crash_report`: dozens of lines into the 64 KiB log ring, a
  `PROCESS_TABLE.try_lock()`, a kernel and a user backtrace with symbol
  resolution, and a `panic_flush` that drains the ring synchronously. A loop
  calling `debug(0)` therefore floods the one channel the kernel reports on and
  spends unbounded time in the panic path, and each iteration takes the
  recovery route, which is documented above as able to strand locks.
- **2** costs one lock permanently, by design, and is one-shot for that reason.

None of this is memory-unsafe and none of it kills the machine. It is a syscall
whose only purpose is to make the kernel misbehave, available to everything —
the same class as `SYS_SHUTDOWN` being ungated, and it wants the same decision:
a capability, or `#[cfg(debug_assertions)]`, or deletion.

### Untrusted-input panics that remain

`SYS_SYSINFO` collects one 24-byte entry per live thread into a `Vec`
(`syscall.rs:1273`) and thread count is uncapped, so ~87,000 threads makes it a
single >2 MiB allocation and the `mm/alloc.rs:12` assert fires. Any process may
call it.

The crafted-ELF panics are closed (`679086d`, `ad38148`, `fa1e9d4`, `b362082`):
`vaddr_to_file_offset` returns `Option` and `checked_add`s, both `align_2m`
wraps, the `syscall.rs:1435`/`:1446` `.expect`s, the bootloader's ESP-sized
allocations and its `filesz <= memsz` check, and the NVMe shift/divide. The
2026-07-28 audit before them closed `sys_mmap(0)`/`sys_alloc_shared(0)`,
`SYS_NIC_RX_DONE`, `SYS_TLS_ALLOC_BLOCK`, io_uring's CQ-overflow assert,
`shared_memory`'s three infallible failure modes and `SYS_THREAD_SPAWN`'s stack
underflow at `a88e4ee`; the ELF `with_capacity` sizing, `load_shared_lib`'s
unchecked `KernelSlice` offsets and the `PT_TLS` heap overflow at `f49c6b3`.

### `SYS_DLOPEN` never dedups and `SYS_DLCLOSE` is a no-op

A process can exhaust its virtual address space by repeated loads of the same
library. The *panic* is closed — `syscall.rs:1435`/`:1446` no longer `.expect` —
but the unbounded VA growth is not, and `SYS_DLCLOSE` (`syscall.rs:298`) still
frees nothing.

Deliberately left by the ELF-hardening pass rather than missed. Dedup is a
semantic change, not a bounds check: a second `dlopen` of a loaded library would
return a handle sharing the first module's id and TLS block, and
`std_tls_dlopen`'s test 10 exercises exactly that case. It needs its own change
with its own test, not a hardening drive-by.

### ASSIGNED — `RingHeader` wraps at 4 GiB and silently corrupts every pipe and `TcpStream`

The ring's byte counters are `u32`. Past 4 GiB of cumulative throughput on a single pipe or
socket they wrap, and the wrap is silent: no assert, no error, just wrong data. Every pipe and
every `TcpStream` is affected, and 4 GiB is an afternoon of file transfer, not a theoretical
bound. Assigned to the `pipe.rs` owner.

### ASSIGNED — a machine with no NVMe controller panics the boot

`kernel/src/main.rs:344` — `.expect("NVMe: no controller found")`. Same class M1 closed for
xHCI's zero-HID panic: a machine that simply lacks a device is not a kernel bug, and the metal
track exists precisely because the target machine's device set is not the one we chose.
Assigned to the `main.rs` owner.

### A 3 MiB `fs::write` to `/home` panics the kernel

`bcachefs/src/btree.rs:184` — `MAX_PAYLOAD - used` underflows, reached via
`split_node` ← `sys_close` ← `flush_file`. No crafting: an ordinary userland
write of a few megabytes. This is a "the kernel must never crash from userland"
violation of the plainest kind. Assigned to the bcachefs owner.

### `ftruncate` to a larger size does not persist on `/home`

`set_len(3 MiB)` followed by `metadata().len()` returns the old length. The same
sequence works on `/tmp`, so this is bcachefs-specific.

### tmpfs has no `open_backing`, so nothing under `/tmp` is loadable

`vfs.rs:62` returns `None`. Combined with the `/home` write panic above,
**userland currently cannot create a loadable file larger than about 2 MiB
anywhere** — which is why the two ELF allocation-ceiling tests assert on the
declared length in the header rather than by reaching the heap assert. Those
tests are honest about what they cover, but the ceiling itself is unexercised
end to end.

### Two allocation guards that do not cover what they claim

`OwnedAlloc::new`'s `size >= PAGE_2M` guard (`process.rs:54`) is short by
dlmalloc's bookkeeping overhead, so a request just under 2 MiB still trips the
`mm/alloc.rs:12` assert. Being fixed.

`mm::align_2m` has no checked form, and four callers take their size from a
device or from userland: `gop.rs`, `xhci/mod.rs`, `shared_memory.rs`,
`arch/syscall.rs`. Audit in progress.

### VA exhaustion is untestable, and the NVMe sector-size case has no test

The VA arena is ~1015 GB and every mapping costs physical memory at worst 2:1,
so the PMM refuses long before the address space runs out. Testing it needs a
test-only actuator on `vma::ALLOC_FLOOR`/`ALLOC_CEILING`.

The NVMe sector-size guard (`fa1e9d4`) reproduces with two QEMU flags but has no
in-suite test; staging it needs an `nvme_lba_size` field on `Shape`. Being built.

### CLOSED, kept for the lesson: a crafted `p_vaddr` could map into the kernel half

An exe image was rebased with a wrapping add, so a crafted `p_vaddr` could place
a demand-paged VMA in the kernel half of the address space — where the first user
touch ORs `PAGE_USER` onto the *shared kernel page tables*. That is exactly the
mapping `sys_mmap` refuses for a FIXED request; the loader reached the same
machinery with no such check. Closed by a `check_user_range` call.

The lesson generalises and the class will recur: **a policy enforced at one entry
point was simply absent at another that reaches the same machinery.** When a
check is added to a syscall, the question to ask is which *other* paths reach
what it protects — not whether that syscall is now safe.

### The bootloader sizes every allocation from a file the ESP handed it

`bootloader/src/main.rs:61-62` reads the UEFI-reported `file_size()` and
allocates that much for the kernel and the initrd, with no bound.
`:103,112` takes `max(p_vaddr + p_memsz)` over the kernel ELF's segments with no
overflow check and allocates it, then `:122` copies `p_filesz` bytes into that
`p_memsz`-sized buffer without checking `filesz <= memsz` — the kernel's own
`elf::parse_layout:419` enforces that, the bootloader does not.

Lower severity than the kernel entries above: it runs before ExitBootServices,
on files we put on the ESP ourselves. It is on the list because the metal track
makes the ESP a thing a user can write to, and because none of the kernel's
protections exist yet at that point.

### ASSIGNED — `std::env::current_dir()` silently returns a wrong path

`getcwd` in `rust/library/std/src/sys/pal/toyos/os.rs:7` passes a fixed
`[u8; 256]`, and `sys_getcwd` copies `min(cwd.len(), buf.len())` and returns that
length with **no error and no signal that it truncated**
(`kernel/src/arch/syscall.rs:736-743`). Any cwd over 256 bytes yields a
truncated path, which the program then builds every other path from.

**A correctness defect, not a path-length limitation.** A refusal would be a
limitation; a wrong answer that looks right is worse, because every consumer
inherits it silently. Found the hard way — it reported 256 bytes for a 2 KiB cwd
and made an agent's test fail against a broken instrument, which is the specific
cost of an instrument that lies rather than refuses.

Fix approved and staged as two halves, and **the kernel half must land first**:
`sys_getcwd` reports the required length instead of claiming success, then std
allocates and retries. Landing the std half alone would have nothing to retry
against.

### ASSIGNED — two syscalls discard a failure signal they already have

`sys_mkdir` calls `vfs.create_dir(&resolved)` and returns `0` unconditionally
(`syscall.rs:1424-1430`). `sys_connect` calls `listener::push_connection(..)`,
which returns `bool`, as a bare statement (`syscall.rs:1042`; `listener.rs:97`).

Filed as one entry because the pattern is the finding, not either instance:
**a bound is only as good as the caller's willingness to hear "no".** In both
cases the underlying operation can already refuse, and the syscall layer throws
the answer away — which is exactly why neither can be given a bound today
without the bound becoming a silent failure.

It is the direct counterpart of the class above. There, a client's request is an
allocation request that needs an owner who can say no. Here the owner *does* say
no and nobody is listening, so adding the cap without fixing the caller would
convert an unbounded resource into a silently dropped request — a worse failure,
because the first is at least visible.

### THE CLASS: a client's request is an allocation request

> **A client's request is an allocation request, and every one of them needs an
> owner who can say no.**

Three instances, and the statement is here because none of the three says it
alone — it is what predicts the fourth:

- the compositor's windows (below),
- netd's piped connections (below),
- `SYS_CONNECT` pinning 4 MiB into an unbounded pending queue.

**The third is worse than it looks, because the attacker does not need to find a
service to abuse — `SYS_LISTEN` is ungated, so it can be its own.** Register a
name, connect to yourself, never accept. No victim required and nothing to
guess.

Independent of the missing cap, and cheaper: **`push_connection` already returns
`bool` (`listener.rs:97`) and `sys_connect` ignores it** (`syscall.rs:1042`).
The queue can already refuse; nobody listens. An ignored failure return is a
defect on its own terms — the mechanism exists and the caller throws the answer
away.

### ASSIGNED — the compositor and netd do not bound what they accept

Neither program has a `MAX_` constant of any kind. The compositor calls
`Poller::new(256)` (`compositor/src/main.rs:747`) and registers three fixed fds
plus one per window, with `windows.push` unguarded (`:127`, `:1377`). netd calls
`Poller::new(64)` (`netd/src/main.rs:1060`) and registers two plus one per tx
pipe. **The 256 and the 64 are guesses, not caps derived from anything.**

This is the same class as the two bounds CLAUDE.md holds up as policy —
`user_ptr::MAX_USER_STR` and `fd.rs`'s `MAX_FDS`, each sized against a stated
ceiling and enforced at the one primitive that can breach it. Nobody wrote these
two. A defect on its own terms, not poller plumbing: the poller capacity is where
it happens to surface first.

**It compounds "No physical memory fairness" below, and the pair is worse than
either alone.** An unbounded window count is a memory-growth path any client can
drive, on a system with no per-process limits, no pressure signal and no OOM
killer. Neither entry is alarming by itself; together a single misbehaving client
takes the machine.

Fix in progress, with two requirements on its shape: the bound must state what it
is a function of, and refusing past it must be an error return — not a panic, and
not a silent drop.

### No physical memory fairness

Any process can allocate unbounded physical memory until the system runs out.
No per-process limits, no memory pressure signals, no OOM killer. A single
misbehaving process starves everything.

---

## 2. The panic path

### A panic holding the allocator lock wedges the recovered CPU

The *reporting* half is fixed: `e9f3356` flushes the crash report in the panic
handler itself, before the recovery branch, so the report reaches serial even if
nothing on that CPU ever runs again (measured byte-identical on the recovering
path, and the flush is idempotent against the `halt_all_cpus` one).

What remains: `mm/alloc.rs:12`'s >2 MiB assert fires inside
`KernelPageSource::alloc` while `KernelAllocator::alloc` holds
`self.dlmalloc.lock()` (`alloc.rs:78`). After `try_recover_from_panic` rejoins
the scheduler, the idle loop's first allocation or free (watcher-list `Vec`
clones in `net.rs`/`audio.rs`, mailbox `Box`es, BTreeMap frees — `dealloc` takes
the same lock) spins forever on the lock the dead thread still holds. The
reentry guard cannot help: a spin is not a panic, and `main.rs` disarms the
guard before recovery anyway. Same family as the `PROCESS_TABLE` hang below —
any fix should cover both (a per-CPU in-panic flag that poisons or force-releases
locks the dying thread holds). There is no `#[alloc_error_handler]`, so a null
return from dlmalloc wedges identically with a worse message.

**Scoping result: this does not need a test-only actuator, and it is the same
problem as the missing bound.** `KernelAllocator::alloc` takes the dlmalloc lock
(`alloc.rs:78`) and then calls `dlm.malloc`, which calls back into
`KernelPageSource::alloc`, whose `assert!(size <= PAGE_2M)` fires **inside that
lock**. So "panicked while holding the allocator lock" is reachable from any
syscall path that can be driven to request a kernel allocation over 2 MiB — an
ordinary workload route, not injection. The same unchecked size is independently a
userland-triggered kernel panic, so the two entries are one defect seen from two
ends: bound the allocation and both close.

Whether such a path exists is a read-only audit, in progress. **No conclusion is
recorded here** — that is what the audit is for.

### A panic while holding `PROCESS_TABLE` hangs the panicking CPU

`try_recover_from_panic` lands in `sched::driver::idle_loop`, whose
`reap_poisoned` takes that lock unconditionally every iteration, and the dead
thread never releases it. Pre-existing and unchanged by the panic-recovery fix; a
`try_lock` could not have saved it either, since a spinlock's `try_lock` fails
for its own holder too. The general shape — locks a dead thread can strand —
belongs to the capability-handles/ownership work.

### The on-screen console shows only what serial has *not* consumed

The log ring is a queue, not a history: `drain_to_serial` pops, and the idle
loop and timer tick drain continuously. `peek_tail` therefore returns only the
bytes no drain has reached yet, which on a running system is the last line or
two — `screen_fatal_halt`'s screen is exactly one line, the nonce.

The panic handler's own reports are unaffected and that is not luck:
`crash_report` writes the whole report with interrupts already off, and
`capture()` copies it before `panic_flush` drains. So a panic screen carries
the report and no context, and a fatal *exception* screen (which never
captures) carries whatever its `crash_report` just wrote, for the same reason.

It matters for the machine M0 exists for. A drain into a backend that discards
— no UART, no virtio-console — still pops, so on the T14 the ring is being
emptied into nothing all the time and there is no scrollback to fall back on.
Options, none taken: stop draining when no backend can write; keep a separate
non-consuming history for the console; or accept it and say so in the design.

**Measured under metal-sim (M1), and worse than "no scrollback".** With
`--metal-sim --mute` and no virtio-console the guest has no output channel at
all once the last boot checkpoint has painted: the failure screen ends at
`Boot: complete`, and soundd's and netd's exit lines — printed seconds later,
and read directly off the console by `metal_sim_compositor` on the same machine
shape with the 16550 on — reach no pixel and no file. A running ToyOS on the
T14 is mute between `Boot: complete` and the moment the compositor's terminal
exists. That is fine for a first boot and not fine for debugging M2 on the
machine. It is also the entire cost the mute default was buying, which is why
the metal-sim profile now keeps its 16550 by default.

### CLOSED — the double-fault path overflowed IST1, by 4x what was estimated

IST1 was 4096 bytes and the report used **9968** — an overrun of **5872**, not
the ~1.4 KiB this entry estimated for months. Closed by growing IST1 to 16384
(`arch/percpu.rs:207`) with a fill-pattern red zone that measures the high-water
mark and reports it straight to the UART, bypassing the ring the overflow may
have corrupted.

**Keep the reasoning, because it is the reusable part:** after the drain buffers
were cut, the report still needed **4512** bytes — so cutting buffers alone was
never sufficient, and the stack had to grow whatever happened to them. Only
measuring established that. A fix that trimmed the buffers, which is the obvious
one and which this entry's own last paragraph proposed, would have looked correct
and shipped broken.

Closed in the same batch: `uart_write_bytes`'s unbounded THRE spin, now bounded
by `THRE_SPIN_LIMIT` (`drivers/serial.rs:337`). It sat on `panic_flush`'s bypass —
the path that runs precisely when the backend holder is *already* wedged — so the
mechanism of last resort could hang the machine. And `main.rs`'s NVMe-absence
panic, now covered by `Profile::Diskless` (`tests/common/qemu.rs:59`), which makes
device **presence** a shape dimension alongside size and sector size.

Still open from this entry: `crash_report`'s `try_lock`, and the recovered CPU
wedging on the allocator lock.

### Nothing distinguishes `panic_console::capture` from a no-op

`capture`/`discard_capture` (`drivers/panic_console/mod.rs:362`, `:374`) have no
test that would fail if they stopped working. Measured, not assumed: with
`capture`'s body replaced by `return`, `screen_late_panic` still passes — and
`main.rs` claimed that test was "the one test that fails if the capture stops
happening". The claim was false; it has been corrected in the code.

An open **testing** gap, not a code defect. The functions were kept for a
narrower surviving reason — freezing the report at the panic instant, where
`live_tail` re-reads a ring that siblings running with IF=0 are still writing
to — and carry a comment saying explicitly not to delete them on the grounds
that the tests pass.

Another gate that cannot fail (`specs/metal-track-history.md`), and the third
found this session, after I5 fairness and the unreachable kernel `check` build.

### A panic while the virtio-console TX queue is wedged *and* unlocked spins

In `submit_and_wait`. Bounding that wait is a `virtio.rs` semantics change that
needs its own discussion.

### CLOSED — a CPU reporting a crash could re-enter the scheduler

Closed centrally at `bd12795`, and **not where this entry pointed.** The entry
proposed tightening the DESIGN RULE to ban `try_lock` and rewriting
`crash_report_panic`'s lookup. The actual property is narrower and belongs
elsewhere: `Lock::try_lock` raises the preempt count, and both its failure path
and its guard's `Drop` lower it — so on the pass that took the count to zero with
`need_resched` set, `preempt::enable` dispatched `do_preempt` **from inside the
crash report**. `preempt::enable` now declines the slow path while
`PerCpu::fault_state` is non-zero (`preempt.rs:129`, `faulting()`), placing "a CPU
inside a fault or panic report is not reschedulable" where preemption is decided
rather than chasing it across four `try_lock` call sites.

`panic_console` had already refused `try_lock` for exactly this reason and
documented it; the rest of the crash path kept using it. That asymmetry — one
module obeying a stricter rule than the file that states the rule — is the shape
worth remembering.

**Caveat: it is not free.** A `fault_state` left non-Normal now costs that CPU its
preemption for the rest of the boot. The invariant holds today (every recovery
path sets Normal, every other path halts), but a leak is now a **hang** rather
than a nuisance. Anyone changing fault handling needs to know that the failure
mode moved.

### No test distinguishes the crash-report preemption fix from a no-op

`bd12795` rests on reading the code, which is the weakest standard this project
accepts. Staging it needs a crash report whose preempt count returns to zero with
`need_resched` set — a timing coincidence the harness cannot ask for. The three
panic-path tests still passing says only that nothing regressed.

Fourth instance this session of the pattern in
`specs/spec-staleness-sweep.md` ("Break it and run it"), and the only one of the
four where the check is genuinely hard rather than merely skipped. Recorded so it
is not mistaken for the same tested standard as the fixes around it.

### `percpu.syscall_rip` is never cleared, so "in syscall context" is a guess

`syscall_entry` stores the user RIP at `gs:[216]` on every SYSCALL and nothing
ever zeroes it. The panic handler's recovery predicate is `syscall_rip() != 0
&& current_tid().is_some()` (`main.rs`), so on any CPU that has ever served a
syscall the first half is permanently true. A panic in IRQ context — a timer
tick, a scheduler assert — with any task current is therefore treated as a
syscall panic: `try_recover_from_panic` poisons that task, kills the process
and rejoins the scheduler.

The consequence is backwards from fail-fast: a kernel bug with nothing to do
with the current process kills an innocent process and lets the machine run on,
instead of halting and reporting. `crash_report_panic` prints a "Syscall:
num=... user_rip=..." block off the same stale value, so the report also names
a syscall that is not running. Clearing it on syscall return is one store; the
honest predicate is a per-CPU "in syscall" depth.

### `fatal_exception`'s `recursive` branch never fires for a nested `#PF`

`page_fault_handler` swaps the fault state to `PageFault` *before* dispatching
(`arch/idt/exceptions.rs:452`), and `fatal_exception`'s `recursive` tests only
`Fatal | Panic` (`:506`). A `#PF` nested inside a panic — the exact case the
short-circuit exists for — is therefore classified non-recursive and runs the
full `crash_report` again.

Termination still holds, through the panic console's `PAINTING` latch and the
per-CPU reentry guard, so this is not a live loop. But `a431e02`'s commit
message credits the `recursive` branch with bounding a renderer fault, and that
mechanism does not fire; the latch is doing all the work. Either widen the test
to include `PageFault`, or stop claiming the branch bounds anything.

### The panic console's memory-type gate checks only the framebuffer's first byte

`kernel/src/drivers/panic_console/mod.rs:208-211`'s
`maps.iter().find(|e| phys >= e.start && phys < e.end)` classifies the entry
holding the scanout's first byte and ignores the rest of the range. A firmware
map whose scanout starts in `MemoryMappedIO` but whose tail falls in a
`BootServicesData` entry the PMM later hands out passes the gate, and the
panic-path write lands in the heap — the one outcome the gate exists to make
impossible. Checking every entry overlapping `[phys, phys + size)` is the same
loop. Untestable in QEMU (its map is well-formed); a T14 firmware-map hazard,
so fix it before the first metal boot.

### `capture()` is unlatched, so two simultaneous panics interleave the snapshot

`kernel/src/drivers/panic_console/mod.rs:289-296`. Both panicking CPUs take
`cli` first (`main.rs:102`), so neither takes the other's halt IPI, and both
`peek_tail` into the same static. Harmless in itself — same ring, `len` read
once into a local, so indices stay in bounds — but the design's "exactly one
painter, ever" is true of `render` and not of the buffer it paints from, and
the screen can carry two interleaved reports. The `PAINTING` latch shape
extends to `capture` if this is ever seen.

### `uart_write_bytes` spins unbounded on the LSR

`kernel/src/drivers/serial.rs`, end of file, while `panic_raw_uart`
(`main.rs`) bounds the same wait at 100 000 iterations. A UART that is
*present but wedged* therefore hangs every `panic_flush` bypass — the last
resort of the panic path — where the raw reentry path would have escaped.

Absent hardware is no longer a hazard. The earlier wording here claimed
`2e52e8e` gated *every* UART access on the loopback probe, which was not true
of `panic_raw_uart` — it did raw `inb(0x3FD)`/`outb(0x3F8)` with no check.
That gap is closed now, and `serial::init` logs the probe byte itself, so
"no SuperIO" (0xFF), "chip answered wrongly" and "right chip, wrong port" are
distinguishable instead of collapsing into one silent `false`.

---

## 3. Kernel correctness and hazards

### A syscall runs with interrupts masked, and only incidentally with preemption disabled

`syscall_entry` raises the preempt count before `call {handler}` and lowers it
after, so `preempt::enable`'s `count() == 0` slow path can never fire inside a
syscall — no matter how many locks the handler takes and drops. Spec §7.4 counts
that slow path as an RT-wake safe point and bounds wake latency by "the longest
preempt-disabled section"; in syscall context that section is *the entire
syscall*, and the real bound is the next `kernel_exit_to_user_check`.

The preempt count is the weaker of two independent blockers, and it is not the
one that decides the bound. `MSR_FMASK = 0x40200` (`arch/syscall.rs:57`) clears
IF on every SYSCALL entry, and nothing on the straight-line syscall path sets it
again — the only `sti`s in the kernel are `cpu::enable_interrupts`
(`arch/cpu.rs:113`, reached from `trap_dispatch`'s #PF arm and from init),
`kernel_exit_to_user_check`'s own yield window (`arch/idt/mod.rs:232`), and the
idle loop (`sched/driver.rs:494`). With IF=0 the CPU cannot even be *told* to
reschedule: `KernelHw::need_resched` (`hw.rs:96-108`) documents that a remote
CPU's `need_resched` byte is unreachable from here, so a remote request is only
deliverable as a kick IPI — an interrupt the masked target will not take until
it leaves the syscall.

That makes the entry level's fix ineffective on its own: dropping it around
blocking-capable handler regions cannot move remote RT wake latency at all
while IF stays 0. A fix has to unmask interrupts over those regions — which
means auditing what each one is safe to be interrupted in — or the bound is
accepted and §7.4 corrected.

This was masked until the preempt count was made conserved across a context
switch (§6.4's baselines needed it): before that the count drifted, so a lock
drop inside a syscall reached zero at random and preempted at random. The
behaviour is now deterministic, and deterministically weaker than §7.4 assumes.
Whether that matters is measurable — gate A's wake-lateness distribution is the
instrument — and it did not move at N=8.

### `retire_task` is never reached by `cargo test`

Instrumented across all 140 tests: zero calls. Threads that `join` are removed
from the table (`collect_thread_zombie`), so `process::exit`'s phase-2 retire
sweep finds nothing, and no test kills a live process. Both callers —
multi-threaded teardown with unjoined threads, and `kill_process` — are therefore
untested, including the §7.6 message-plus-park protocol 7b rebuilt them on.

### `handle_retire`'s `need_resched` on a running target is a request the next pass may decline

`preempt_if_due` fires on quantum expiry or an RT task in the band, and on
neither for a merely-killed task — so the pass that `need_resched` asked for can
run, clear the request and resume the task, which then dies only at the real
quantum end. That is what spec §7.6 promises ("bounded by the quantum") and
`retire_task`'s spin deadline is 100 quanta, so it is conformant rather than
broken. Adding `|| current.shared().kill_pending()` to `preempt_if_due` would
make the request mean what it says, for one atomic load per pass.

### A thread retired while parked leaves its node in the wait queue forever

`Msg::Retire` reaps a `BlockedTask` on its home CPU; the `Registration` that
would have dequeued it lives on the dead thread's own stack and is never dropped
(the kernel does not unwind), so the queue keeps an `Arc<TaskShared>` for a task
that is `Dead`. A leak, not a correctness hole — `claim_wake` on a dead task
returns `Claim::Lost` and `wake_one` moves to the next waiter, which is exactly
what spec §8.2's retry arm exists for — but the list grows across process kills
and a `wake_all` walks the corpses. The fix belongs with the intrusive
`wait_node` the core still owes (`waitq.rs` holds waiters in a `VecDeque`
instead; see its module note): with an embedded node, `reap` could unlink it.

### `spawn_thread`'s late failure paths drop a mapped TLS block

The two `return None`s after `PROCESS_TABLE` is taken — the process is gone, or
`tearing_down()` claimed it between phase 1 and there — drop the `ThreadData`
holding a `MappedPages` that is already in the parent's address space. Drop frees
the pages; nothing unmaps the VA. Same shape as the `SYS_TLS_ALLOC_BLOCK`
use-after-free (`fcd481f`), which is why the kernel-stack failure path above them
now calls `MappedPages::release`.

Much narrower: reaching it means losing a race with the target process's own
exit, and the address space is destroyed moments later, so the window is a
sibling thread that has not yet been retired. Not fixed with the rest because
`tls_alloc` is already inside the `Arc<Lock<ThreadData>>` by then and the release
cannot happen under the table lock (it would put `AddressSpace` under
`PROCESS_TABLE`, a lock order this kernel does not otherwise use). Building the
`ThreadData` after the table check, inside the same lock hold, is the shape that
fixes it.

### A keyboard that resets behind our back is undetectable on the PS/2 wire

The i8042 driver runs the keyboard in set 2 with the controller translating to
set 1, which is Linux's default and the best-trodden EC path. The cost is that
`0xAA` — the keyboard's BAT-complete byte after a self-reset — is bit-identical
to left Shift's break code (`0x2A | 0x80`). `toyos-ps2` therefore does *not*
treat it as a reset; only `0x00`/`0xFF`, the overrun and detection-error codes,
report `Lost` and trigger `keyboard::release_all()`.

Consequence on the T14, where the EC does reset the keyboard after suspend or a
lid event: the reset is not noticed. It is survivable rather than silent
breakage — the keyboard comes back in set 2 with controller translation still
on, so the wire format is unchanged, and the `0xAA` it sends decodes as a Shift
*release*, which is accidentally the right direction for the one state that
could stick. Untested on real hardware. If it does bite, the fix is a
controller-side reconnect probe (`0xF2` identify on a timer), not a wire
heuristic, because no wire heuristic exists.

### `sys_read` blocks on an empty Keyboard fd and returns `NotFound` on an empty Mouse fd

Two fds of the same shape, two different answers to the same question. Pick one.

The spurious-readiness half of this entry is closed: `handle_report` returns the
number of events it queued and `dispatch_report` wakes only on a non-zero count,
so readiness and `has_data()` agree. Userland still reads both fds non-blocking,
which is now belt and braces rather than a workaround.

**The other half, not previously recorded: two of the wait queues are woken by
nobody's benefit.** `sched::waitqs::MOUSE` and `sched::waitqs::NETWORK` each
appear at exactly one site in the kernel — their own wake (`mouse.rs:104`,
`net.rs:54`). Nothing ever parks on either. The wakes are real calls on a hot
path doing nothing, and they are the direct consequence of the asymmetry above:
because `sys_read` returns `NotFound` on an empty Mouse fd rather than blocking,
there is never a parked mouse reader to wake. Fixing the asymmetry by making
Mouse block is what would give `MOUSE` a waiter; deleting the queues is what
would make the current behaviour honest. Do not do neither.

### The `bcachefs/` crate does not implement bcachefs — a question for the owner

ToyOS's `bcachefs/` crate implements a ToyOS-native on-disk format written from scratch.
It shares a name with Linux bcachefs and nothing else: ours is `MAGIC = b"BCFS"` plus
`DESIGNATION_MAGIC = b"TOYOS-FORMAT-ME\0"` (`superblock.rs:5,24`) and `NODE_MAGIC = b"BTND"`
(`btree.rs:7`), against upstream's UUID-based `BCHFS_MAGIC` / `BSET_MAGIC ^ sb.uuid` /
`JSET_MAGIC`.

`specs/bcachefs-reference.md` — real research into the *upstream* format — now carries a
warning saying so at the top, because its filename in this repo is a trap. That fixes the
document; it does not fix the collision. A crate that does not implement the format it is
named after is a hazard we keep paying for, in exactly this way. Renaming it is the owner's
call, not something to do in a docs pass.

### `#[alloc_error_handler]` does not exist anywhere in the kernel

Kernel heap exhaustion has no handler. It routes into `try_recover_from_panic`, the path that
frees nothing — so the terminal state of every unbounded-growth entry in this file is an OOM
that cannot report itself cleanly. The three unbounded userland-driven growers under §1 all end
here, which is what makes this worth its own line rather than a clause in each.

### `device-test-strategy` requires a `query-pci` verification that exists nowhere

The strategy's rule is ground truth at the hardware boundary: what QEMU was *told* to create
must be checked against what the guest actually enumerated. No such check exists — no test
queries QMP's `query-pci` and compares it against the guest's view. Every profile's device set
is therefore asserted only by the harness's own construction of the QEMU command line, which is
the same source it would be verifying.

Same class as the three scheduler instruments below: a spec requiring an instrument nobody
built. This one matters most for the metal track, where the whole point is that the machine's
device set is not what the harness chose.

### `boot-image-split.md`'s R2 refactor would fail the suite as written

R2 proposes removing the USB stick from the machine profiles and adding a virtio device. Both
halves break tests that exist today: three machine tests assert on the USB stick, and the
profile it would add a virtio device to is the one whose defining claim is that it has *no*
virtio device — that is what `--metal-sim` is for.

Not a doc bug — a plan defect. A plan that fails the suite is not ready to execute, and the
suite is right here. Whoever picks R2 up must re-scope it against those tests first.

### The scheduler's fair split degrades as the machine widens — settled: it is the policy

Worst service spread against the derived bound, in ms, from
`measure fairness_storm:<cpus> 500`:

| CPUs | 1 | 2 | 3 | 4 | 6 | 8 | 12 | 16 | 24 | 32 |
|---|---|---|---|---|---|---|---|---|---|---|
| worst | 30 | 84 | 125 | **198** | **324** | **418** | **634** | 720 | 1056 | 1386 |
| bound | 60 | 108 | 156 | 204 | 300 | 396 | 588 | 780 | 1164 | 1548 |

**Both questions the earlier filing left open are now measured, not argued.**

**Offset, not drift.** Holding the seed count and scaling the storm's per-thread
work: one CPU stays at 30 ms at every window length, while eight go 362 → 602 →
548 ms as the window doubles twice. It saturates rather than accumulating.

**Policy, not model.** Everything deciding who runs next is the shipped core —
`RunQueue`'s insertion-time keys, `FairShare`'s one vruntime pot per process,
`CpuSched::pick`, `answer_steal_requests`' surplus rule. The simulator mocks
time, timer, IPI, halt and switch: the parts that decide *when*, not *who*.

**The mechanism, which is why this is a design consequence and not an
implementation bug.** Every running thread of a process charges one pot, so the
pot advances at the process's *aggregate* rate while each queued thread's key
stays frozen at its insertion. One dispatch of staleness therefore buys more
wall-clock service the more of that process runs at once. That is why it scales
with width, and why careful coding cannot close it — the fix is a policy change.

**Caveat, and it is load-bearing.** These are worst-of-N over adversarially
chosen interleavings, seeded and PCT — not the split hardware would show on an
average schedule. **The mechanism and the scaling are the policy's; the magnitude
is a worst case.** Do not quote these numbers as expected behaviour.

**Connected to §9.2's tie-break, and that is why this is hard.** Threads of a
process sharing one vruntime is *why* an identity tie-break starves siblings, and
*why* the insertion sequence exists (`specs/scheduler-core-spec.md` §9.2,
`queue.rs:18-22`). The degradation here and that starvation are two faces of one
decision: **per-process accounting with per-thread queueing.** Anything that
fixes one has to answer for the other.

Found only because I5 measures *service* — nanoseconds actually delivered — rather
than checking vruntime bookkeeping against itself, which would have been true by
construction. The dead-gate lesson from the other side: the first question about a
gate is not whether it passes, but whether it measures the quantity you care
about (`specs/spec-staleness-sweep.md`).

### The scheduler crosses its own derived granularity bound at four of ten widths

Distinct from the entry above, and deliberately not merged with it. That one says
fairness degrades as the machine widens. **This one says the shipped scheduler
exceeds a limit its own design implies** — a different and sharper statement.

The bound is derived from granularities the policy itself picked:
`lag_spread + (ΣT_i + 1) × (QUANTUM + max KernelSection + 2 × RUN_CHUNK)`. It is
crossed at **4, 6, 8 and 12 CPUs**, by 116, 324, 418 and 634 ms (bold in the table
above).

**The gate handles this honestly rather than hiding it**, which is the part worth
preserving. It reds on `max(derived, recorded allowance)`, so a sampled scenario
is gated on not regressing — but `Outcome::fair_over_bound` records every crossing
of the *derived* bound regardless, and the sweep prints
`N ns PAST THE DERIVED BOUND on the recorded allowance`. **The allowance cannot
quietly become the standard**, which is the failure mode of every temporary
baseline and the reason most of them end up permanent.

### `src/build.rs` cannot enable `sched-check`, so no CI run exercises it

The kernel check build is reachable now — `kernel/Cargo.toml:63` forwards
`sched-check = ["toyos-sched/check"]`, and `cpu::MAX_PASS_NS` is 200 µs with
invariant P asserting against it (`cpu.rs:618`, `:1013`). But nothing in `src/`
mentions `sched-check`, so it can only be turned on by hand and the harness never
does.

A check build nobody can run from CI is halfway back to being unreachable, which
is the defect it was built to fix.

### CLOSED — the three uncertifiable scheduler instruments

All three are resolved, and the third is resolved by *subtraction*, which is worth
recording as a legitimate outcome:

- **I5 exists** and measures service against equal entitlement over a contention
  window, with `fairness_storm(cpus)` and a CLI form for Stage 9's 1–128. It
  immediately found the fairness degradation above.
- **The kernel check build is wired** (`sched-check` → `toyos-sched/check`), with
  the CI gap filed separately above.
- **`from-qemu` was deleted, not implemented** (`hw.rs:52-53`). The capability
  given up is stated precisely, and it is not the subcommand — that was an
  `unimplemented!()` — but the *promise* that a QEMU anomaly can become a
  host-side repro. Getting it back needs: a kernel drain path; emitters for
  `TraceKind::{Block, IdleExit, Irq}`, none of which exist; queue identity in the
  record; and scenario synthesis. That list is the spec for anyone who wants it.

The I5 bound is deliberately not recorded here: it is being re-derived from first
principles rather than calibrated against the shipped code's current behaviour,
with the measured behaviour kept separately as a regression sample in the style of
`tests/audio-baseline.toml`. The gap between the two becomes its own entry.

### `sys_read` blocks: two doc comments that describe code that is not there

Neither changes behaviour; both mislead a reader about an invariant.

`kernel/src/fd.rs:142` — `/// Insert at the lowest unused id.` It calls
`IdMap::insert`, which is `let id = self.next; self.next += 1` (`id_map.rs:46-51`):
a monotonic counter that never reuses a closed fd number. Lowest-unused is a
POSIX guarantee some code may assume; this is not it, and a long-lived process
leaks fd-number space rather than recycling it.

`kernel/src/process.rs:950` — `/// Must run after `teardown_scheduling`, which is
what flushes the child threads' counters into `ProcessData`.` There is no
`teardown_scheduling` anywhere in the kernel. The ordering requirement it states
may still be real; the function that was supposed to establish it is gone, so the
comment names no enforceable precondition.

### A keyboard flood into a blocked `sys_read` panics the kernel

`prepare_wait` asserts `set_waiting()`, "a task waits on at most one queue"
(`toyos-sched/src/waitq.rs:124`), and a thread blocked in `sys_read` on the
keyboard fd trips it under sustained input:

    !!! PANIC !!!: panicked at toyos-sched/src/waitq.rs:124:9:
    a task waits on at most one queue
      <WaitQueue<…>>::prepare_wait+0x1a5
      <kernel::sched::driver::Ticket>::register+0x9f
      kernel::scheduler::wait_until::<kernel::keyboard::has_data>+0x49
      kernel::arch::syscall::sys_read+0x77
    Running: pid=1 tid=Some(Tid(0))   Syscall: num=1

Seen twice while looking for something else: `Profile::MetalUsb` with QMP
key events injected across the whole boot at a few thousand a second, once
with the i8042 present and once with `q35,i8042=off`, so both the PS/2 and
the USB delivery paths reach it. The victim both times was the in-guest
test runner blocked on stdin at `===READY===`. It does not reproduce at
ordinary typing rates, and neither run was reduced further — the flag is
still set from a previous wait when `sys_read` loops round and prepares the
next one, but which of `wait_until`'s cancel/commit paths left it set was
not established. Reproducing it deliberately means a guest-side key
generator, not a host-side flood.

### An io_uring `Source` can carry one half of the wake pair

Every source needs two wakes at its event site: the direct-blocker queue, and
`complete_pending_for_event` for the ring watchers `process_poll_add`
registered. Nothing in the type system pairs them, so deleting or forgetting
one half leaves a source that looks wired and silently never completes a
`POLL_ADD` — the poller's CQE then arrives only on submit-time readiness (the
immediate post or the TOCTOU recheck) or on close, when `remove_fd` posts
`NotFound`. Otherwise it waits forever.

Both halves are present for every source today. Audio and Network were
restored at the `drain_irqs` site (`kernel/src/sched/driver.rs`); pipes wake
from `process.rs`/`pipe.rs` close paths, keyboard and mouse from
`HidDevice::dispatch_report`, listeners from `wake_poll_waiters`.

History, because it is what makes this worth a line. Stage **7a** (f4d8fa7,
not 7c as this entry and aeeaa01's commit message first said) deleted the
audio and network `complete_pending_for_event` calls out of `drain_events`
while explicitly preserving the keyboard/mouse pair in `hid.rs` — collateral
of the cutover's `EventSource` removal, not an intended deletion; see
`specs/scheduler-migration-log.md`. Neither loss was visible for two months.
soundd polls the audio fd every cycle but its streaming wakes came from its
own armed DLL timer, so only the idle path depended on the missing half —
and there was no idle path until suspend-on-idle. netd polls its NIC fd every
iteration (`userland/netd/src/main.rs`) and waits `u64::MAX` at full idle with
RX being `nic_rx_poll()` only, so a frame arriving while netd is fully idle
posted no CQE and netd slept with `net::has_packet()` true until an unrelated
wake; that never surfaced because no test drives netd, and interactive use
always has something else waking it. The 7c review compared two post-7a trees,
found them identical, and concluded there was nothing there.

The durable fix is iouring-blocking-spec's single `post()`, where a source
cannot have one half of the pair, and a fan-out cannot be deleted without
deleting the wake.

### The virtio-console has no line atomicity between writers

Kernel `log!` output and userspace `println!` interleave *mid-word* into each
other's lines: soundd's stats line was split by a kernel message in 1 of 15 runs
and by the tone client's own `println!("tone done")` in 2 of 120 config-runs,
each time pushing the line's tail onto the following line.
`tests/common/audio.rs` reassembles both cases (strip `[kernel …]` spans; resume
a field's digits after the next newline), but that is a reader-side workaround
for a writer-side defect — any tool parsing serial output has the same problem.
Serial writes of a whole line should be atomic.

Related class: a "guest hang" that only ever appears on the audio tests is more
likely to be the shared console than the scheduler. See
`specs/audio-gate-history.md`.

### On an idle machine the log ring flushes one line behind

Measured while building M2's i8042 tests. With no userland process doing
anything, a `log!` line reaches the console only when the *next* piece of work
wakes a CPU — so the most recent line is always still in the ring. Injecting
keystrokes 200 ms apart into an otherwise idle guest and watching serial:

```
0.144  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'a'
0.347  i8042: drain bytes=6 keys=0 woke_kb=0     <- Pause
0.551  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'b'
0.754  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'c'
                                                 <- key 'd' never appeared
```

`drain_chunk_to_serial` runs from the idle loop and the timer path, and an idle
CPU that has just finished its work does not come back for the line it queued
on the way out. The consequence for anyone reading serial: **the last line
before a quiet period is not evidence of anything**, and a guest that wedges
right after logging its final line looks like it never logged it. Every
existing test happens to keep the guest busy, which is why this has not bitten
before; `i8042_no_spurious_wake` drives an in-guest reader for exactly this
reason. Fix is a flush on the transition into idle, not another poll.

### Nothing the kernel logs on the shutdown path ever reaches the console

The same mechanism as the entry above, with a harder ending. `SYS_SHUTDOWN`
(`syscall.rs:219-224`) logs "Syncing filesystems...", syncs, logs
"Shutting down." and calls `acpi::shutdown()`. Both lines go into the ring and
the power goes off before anything drains it. Measured on the MetalDisk profile:
the last console line of a clean shutdown is the kernel's `spawn:` line for
`/bin/shutdown`, and QEMU exits shortly after.

So a shutdown that panics or hangs mid-sync produces no diagnostic at all —
including on the T14, where writing back is the operation with something to
lose. `nvme_large_device` had to assert on the disk image host-side instead,
which is a better assertion anyway but was not a free choice. Fix is the same
flush-before-parking as the idle case, plus an explicit drain before
`acpi::shutdown()`.

### The NVMe driver trusts the sector size the namespace reports

`drivers/nvme.rs:209-210` takes `lba_ds` out of the LBA format descriptor and
computes `1u32 << lba_ds`. The field is 8 bits, so any value ≥ 32 is a shift
overflow. `:300-301` then computes `4096 / ctrl.sector_size` and divides
`ns_size` by it, so a reported sector size above 4096 makes `sectors_per_block`
zero and the next line divides by zero. Both are firmware/device values, not
userland, but "the device said so" is not a bound — and the metal track is
exactly where a device we did not write starts answering these queries.

### Two unreproduced observations

`ps` appeared to stall for >2 s under heavy single-core load; later runs fine. If
seen again, capture with LLDB before restarting.

Doom's music was heard once at roughly half speed. It did not reproduce at HEAD,
with or without `-nodefaults`, and the wav capture measured 1.00x — so whatever
happened, the device-side path was never wrong. Leading hypothesis is host
contention: another agent was building in this tree with a second QEMU running
at the time.

The durable part is the instrument, not the sighting. **Next time, read the
numbers rather than listening**: Doom prints `[music]` synthesis
real-time-factor telemetry every ~5 s, and soundd prints wake/underrun/latency
stats every ~2 s. A starved synthesizer and a wrong playback clock sound
identical to a human, and RTF is what separates them — RTF near 1.0 with the
audio still slow means the clock, RTF well below 1.0 means synthesis is not
keeping up.

---

## 4. Audio and soundd

Spec: `specs/audio-subsystem-spec.md`. Numbered as in the 2026-07-28 audit;
(1) — see the re-filing below; it was never an SQ overrun — (2) `CommandRing::push` assert, (3) ungated
`SYS_SET_RT_PRIORITY`, (4) NaN volume, (7) crash detection and (9) the
"wait until clients have filled" condition are fixed (`97723dc`, `9ed8eda`,
`a88e4ee`, `069d158`).

**RE-FILED — audit item (1) is not an SQ overrun; it is silent completion loss on
the CQ.** The submission ring self-limits at four separate points: `poll_add_fd`
flushes at `pending() == sq_size`, `submit_sqes` refuses `count > sq_size`,
`claim_sqe` errors when `available > sq_size`, and the kernel drains `head` to
`tail`. Nothing can overrun it.

The real defect is on the completion side, and **the mid-registration flush is the
cause rather than the protection**: flushing mid-registration makes the kernel
process those registrations immediately, so fds that are already ready post CQEs
while the caller is still registering the rest. Past `cq_size` (2 × `sq_size`),
`post_cqe` increments `dropped` and returns (`kernel/src/io_uring.rs:201`) — and
**`Poller::wait` never reads `dropped`** (no occurrence anywhere in
`toyos/src/poller.rs`). The caller then blocks forever on an event that was thrown
away.

Kept rather than renamed in place, because the mislabel is the finding: an entry
filed under the wrong mechanism sends everyone to the wrong ring, and the
submission ring is exactly where you would look.

**Stale prose, same class as the rest of today's:** the `Poller`'s own doc comment
says the kernel "asserts rather than overflows" (`toyos/src/poller.rs:27`). That
stopped being true when `post_cqe` switched to incrementing `dropped` and
returning. Nobody re-checked the comment, and it is the sentence that would have
stopped someone looking for the loss.

Fix is three commits: make the loss loud, then make the drop unrepresentable via a
declared capacity, then keep the tripwire as an unreachable assert. **The second is
blocked on the compositor/netd bounds** — two callers cannot honestly declare a
capacity until they bound what they accept.

**BLOCKED ON THE CPAL FORK — one missing message, three consequences.** Killing
wedged clients, suspending on no progress, and resuming from suspend all need the
**same single client→soundd edge**: a resume notification on the control
connection. Recorded as one item deliberately — filed as three, the next planner
schedules three investigations that all reach the same wall.

Established from the code on both sides. soundd's `TOKEN_CMD` carries exactly
three commands — `MixCommand::{AddClient, RemoveClient, SetVolume}`
(`soundd/src/main.rs:151-153`) — and none is resume. The client blocks reading the
soundd→client signal pipe, while cpal's `play()` futex-wakes only its *own*
thread, so **there is no client→soundd traffic in the steady state at all.**

The wake path already exists: `TOKEN_CMD` is what wakes a fully idle mix loop when
a new client connects (`:727`, `:732`). Only the message is missing, and it has to
be sent from cpal's `play()`. **One fork change unblocks all three.**

The three consequences:

1. **Wedged clients cannot be distinguished from paused ones**, so neither can be
   killed. §6.4 *specifies* pause as "no explicit coordination required", so this
   is the spec needing to change, not the implementation.
2. **One paused client defeats idle suspend for the life of the process.**
   `is_streaming()` is `delivered && !pending_removal` (`main.rs:129-131`),
   latched by the first period a client ever supplies and never cleared, so
   `any_streaming` stays true after a pause. Audio spec §5.8's promise ("Zero
   overhead, zero wakes, device voice closed") is defeated: a wake per period
   forever, DMA engine running, codec voice open. It also pins that client's shm
   region, pipe and slot ring, compounding defect (6). **Battery-relevant on the
   T14 specifically** — the machine the metal track is building toward — because
   the wake never stops.
3. **Resume from suspend has no edge to fire on.**

**Correction, recorded because the earlier entry said the opposite.** This file
previously said the suspend half "may be fixable in soundd alone" pending an
answer. The answer is in: **it is not.** A suspended soundd would wait on device
completions that never come while the resumed client blocks forever on a signal
byte soundd is no longer writing — battery traded for permanent silence, which is
strictly worse and is exactly the trade the owner's standing quality rule forbids.

### Stopping the device voice while keeping the timer wake — soundd-only, gate-blocked

Kept out of the cluster above because **its unblock condition is different**, and
that is the useful part: it could land *first* if the quiet tree arrives before
fork access.

Stopping the device voice while keeping the periodic timer wake recovers the DMA
engine and the codec — the battery-relevant hardware — and gives up only the wake
itself. Resume still works unchanged, because soundd keeps writing signal bytes,
so it does not need the missing client→soundd message.

So it is **not blocked on the fork**; it is blocked on the **audio gate**. A
mid-session device stop/restart is an audible transient plus a DLL re-lock, which
needs the thorough tier on a quiet tree.

**A device advertising four buffers panics soundd at startup.**
`assert!(num_buffers > 5, "soundd: pipeline too shallow to defer safely")`
(`main.rs:597`) turns a device shape into a startup panic. Same class as the NVMe
and xHCI zero-device panics closed today — an unanticipated device shape killing a
process rather than being handled — and metal-relevant, since nobody knows what
the T14's codec advertises.

The fix falls out of decoupling the client slot count from the device pipeline
depth, which turns the assert into a clamp. Which is also *why* the assert exists:
`slot_count = num_buffers` (`main.rs:1290`) couples every client's ring geometry to
the kernel's `TX_INFLIGHT_MAX`. The comment's own reasoning establishes
`slot_count >= num_buffers`; **equality was assumed, not derived.** That design is
written up and deliberately not landed — it changes ring geometry and therefore
audio timing, so it needs the thorough gate tier on a quiet tree, not the fast one.

**(5) ASSIGNED — the cpal ToyOS backend hardcodes 44100/2ch/i16** and rejects
everything else, so soundd's resampler and channel-conversion paths (spec §6/§8)
are unreachable from any real client and effectively untested. It also
`assert_eq!`s the device rate against a compile-time constant, so changing the
driver's rate aborts every cpal app.

Deferred to the quiet-tree window, not neglected: editing that fork needs
`.cargo/config.toml` path overrides, which redirect cpal for **every** agent in
the tree. Same scheduling constraint as the fork lint audit
(`specs/fork-lint-audit-plan.md`).

**Client liveness is blocked on this, not on soundd.** The ambiguity between a
paused and a wedged client is *specified*: §6.4 defines pause as "no explicit
coordination required", and the cpal backend's `pause()` is a purely local futex
store soundd is never told about. No change confined to soundd can separate the
two, and landing the soundd and SDK halves alone would kill every paused cpal
client. This is a case where the **spec**, not the implementation, is what needs
to change.

**(6) soundd never frees the per-client shm region.** `SharedMemory::Drop` only
unmaps and nothing calls `destroy()`, so each open/close cycle strands a 2 MiB
page. **Bounded by the next process exit, not by soundd's lifetime** —
`cleanup_process` sweeps every region owned by an exiting process — so this is a
real leak with no release at close time, but not a permanent one. The entry
previously claimed the page was stranded for soundd's whole run, which
overstates it: a long-lived soundd accumulates only until whichever process owns
the region exits.

**ASSIGNED** to the isolation agent, merged with `SYS_GRANT_SHARED`'s missing
revocation: revoke and reclaim are one mechanism, and fixing either alone leaves
the other holding the same page.

**(8) FIXED at `4fce59c`** — `choose_params` (`virtio_sound.rs:62`) now selects a
rate and channel count the device actually advertises, and a device offering
nothing this driver implements logs *which* capability is missing and leaves the
machine to boot without audio, rather than being silently remapped to 44100/2.

> **CAVEAT — not verified by a QEMU boot.** This fix changes device negotiation,
> and seven consecutive boot attempts died in shared-toolchain contention. The
> reasoning that it still selects (44100, 2) on QEMU is *static*, read off an
> earlier boot log's advertised bitmaps. `cargo test -- audio` on a quiet tree is
> owed before this is treated as proven. Recorded as a live gap because an
> unverified change to negotiation is exactly the kind that fails on the one
> machine nobody booted.

**(10) REFUTED — the two TPDF dither draws are independent enough that nothing
can tell.** Kept rather than deleted: the measurement is the finding, and an
entry removed silently gets re-filed next year by the next person who reads
`rng.next() + rng.next()` on one `Xorshift32` state and assumes.

Measured over two million samples, one state stepped twice versus two
independent states:

| | variance (TPDF ideal 0.16667) | χ²/df vs triangular | lag-1 autocorrelation |
|---|---|---|---|
| one state, two draws | 0.16672 | 0.98 | −0.00048 |
| two independent states | 0.16652 | 0.63 | −0.00050 |

The joint distribution of the summand *pair* is where a deterministic
relationship would actually show, and it does not: χ²/df ≈ 1.00 with zero empty
cells at 32×32, 128×128 and 512×512, for both arrangements. The step function
decorrelates the two draws well enough that the pair is empirically
indistinguishable from two independent streams.

**Deliberately not "fixed anyway".** Changing the dither changes the captured wav
bit-for-bit, so it would perturb the audio gate to chase a defect nobody can
demonstrate. This project has been bitten specifically by gates that cannot fail
(`specs/metal-track-history.md`); spending the gate's sensitivity on a
non-defect is the same error wearing a tidier hat.

Two of the three lower-severity items are **FIXED at `4fce59c`**. The passthrough
gain was not a rounding nicety: decoding by 32768 and quantizing by 32767 meant
**32,703 of the 65,536 i16 values did not survive a round trip**, each off by one
LSB. Now 0, gated by an exhaustive host test over every i16
(`soundd/src/main.rs:1347`). `AudioInfo::as_bytes` no longer publishes
uninitialised kernel stack: the padding is spelled out as named fields with a
`const _` size assert, so omitting one is an E0063 compile error rather than a
convention someone can quietly break.

Still open: unknown audio device command bytes report success and do nothing.

**The kernel's byte-1 audio fd verb has no SDK caller.** `kernel/src/fd.rs`
still dispatches `1 => crate::audio::start()`, but suspend-on-idle deleted
`AudioDev::start()` from `toyos/src/device.rs`: the only PCM start left is the
implicit one inside `submit_buffer`, which is what makes resume a single
control verb inline with the first submit. Recorded rather than deleted,
deliberately — a dead-code sweep that removes the arm narrows the ABI, and
the syscall surface is a contract, not an implementation detail. Byte 0
(stop) is live; soundd calls it every suspend.

**Residual from the `069d158` fix:** the deferral predicate cannot distinguish
"mid-refill" from "stopped producing". `9ed8eda` closed most of it by releasing
soundd's read end of the client's signal pipe at the first period the client
delivers, so a dead client is now detectable — but the control thread only
notices when it next reads, and until then the stream stays `is_streaming()` and
the mix loop keeps deferring buffers for a producer that no longer exists.
Bounded harmlessly by `refill_floor_nanos`.

**`f32::round()` lowers to a `compiler_builtins` `roundf` call on the ToyOS
target, not `roundss`.** The quantizer calls it once per sample (256/period,
~344 periods/s ≈ 88k calls/s). SSE4.1 is universally present on the 2020+
hardware baseline, so enabling it in the target spec turns this into one
instruction; whether to widen the target's feature set is a separate decision.

**Gate A can still fail a run on `drains` alone** (`tests/common/audio.rs:441`),
with an empty gap histogram and zero underruns. The proportional-recovery fix
(`91a653c`) deliberately decoupled drains from harm, so a per-run failure should
require evidence of harm — gaps or underruns — with `drains` reported and not
fatal. The re-record at `dc732e5` set honest ceilings, so this is now a
robustness question rather than a live false-red.

---

## 5. Diagnostics

### `ps`, `stats` and `dump_blocked` lost their cross-CPU view at Stage 7a

A `CpuSched` is `!Sync` and reachable only from its own CPU, so walking a
sibling's queues is now unwritable rather than racy. `task_cpu_ns` and
`task_sched_state` were rebuilt on values the owning CPU *publishes* —
`TaskHandle`'s counters, republished at each end of a pass, plus the core's
rendezvous word — so they are accurate and lock-free, which also closes the old
`try_lock`-and-skip misreport. `dump_blocked` has no such substitute: it prints
only the calling CPU's parked map, by `TaskKey` and `WaitClass`, with no process
name and no per-source detail, because the pool it used to walk does not exist. A
cross-CPU view costs a message round trip; whether the diagnostic is worth
building is a diagnostics question, not a scheduler one.

### CPU attribution: the recorded "half the CPU is unattributed" claim was wrong

**Its sign was backwards.** Investigated 2026-07-29,
`specs/cpu-attribution.md`. `stop_cpu_timer` adds *one* delta to both the
per-thread `cpu_ns` that `ps` reads and the per-CPU `CPU_TIME_NS` that
`SYS_SYSINFO` reports as busy — they are one accumulator, not two measurements.
Genuinely unattributed kernel time is therefore absent from **both** numerators
and cannot open a gap between them; it pushes the 97% *down*, so true busy
**exceeds** 97%.

The 45-vs-97 gap is reader-side: `ps` divides a since-thread-creation cumulative
by since-boot uptime (`userland/toybox/src/ps.rs:54-56`) while the compositor
taskbar computes a correct one-second delta
(`userland/compositor/src/main.rs:1512-1518`) — a lifetime average against an
instantaneous sample — plus per-row flooring via `as u32` (up to a point per row,
12–20 rows) and reaped processes whose time stays in the system total forever but
vanishes from every row. The recorded prime suspect was also wrong: `mov cr3`
happens *after* `start_cpu_timer`, so the address-space switch is charged to the
incoming task, not lost.

`ps` already fetches `total_cpu_ns` at sysinfo bytes 32..40 and ignores it;
`header total_cpu_ns − Σ(printed cpu_ns)` is exactly the reaped+zombie loss,
measurable today with no kernel change. Real unattributed windows do exist — the
scheduler's pick-and-arm window (deliberate, documented) and the whole idle-loop
body, which does substantial work and is counted as idle — but they are smaller
and different from what the old entry claimed.

### `SYS_PROCESS_STATS` can only report an exited direct child, once

`sys_process_stats` (`kernel/src/arch/syscall.rs:1640`) positions in
`data.child_stats` — a per-parent list, populated only when a child exits
(`kernel/src/process.rs:998`) — and `remove`s the entry it finds. So the
syscall answers exactly one question: what did my own child, which has
already exited, cost? It cannot sample a live process, cannot be differenced
across two calls, and cannot see a daemon at all.

That is the whole of layer 1's read path, and nothing said so outside
`toyos-abi/src/syscall.rs`'s doc comment. `userland/toybox`'s `stats` is a
spawn-and-measure wrapper, which is why it works. Anyone asking "where is
soundd's / the compositor's / netd's time going?" has to reach past it —
`audio_idle_suspend` pays exactly that cost, name-matching `SYS_SYSINFO`
entries into a byte buffer to sample a running daemon twice. A per-process
query on a live target is the missing piece; it is a layer-1 gap, not a
layer-2 one.

### Profiling layers 2 and 3 are not built

Layer 1 (process accounting counters + the `stats` tool) is implemented, with
the read-path restriction above. Event tracing and RIP sampling are not. See
CLAUDE.md's diagnostics roadmap.

---

## 6. Build and toolchain

### std leaks a whole thread stack on every `thread::spawn`

`rust/library/std/src/sys/thread/toyos.rs` allocates the stack with
`alloc::alloc` (2 MiB minimum), hands its base to `SYS_THREAD_SPAWN`, and never
records the pointer. `Thread` holds only a tid and has no `Drop`, `join` does not
free it, and the trampoline cannot — it is standing on it. So every spawned
thread costs 2 MiB of heap for the life of the process, which dlmalloc serves
from a dedicated `mmap` above its 256 KiB threshold: one leaked 2 MiB kernel
region per spawn, walking the address space downwards.

Found while testing thread-exit TLS release, where the drift swamped the signal
(the test now drives `SYS_THREAD_SPAWN` directly on a reused stack). It also
makes any per-process memory measurement across a thread-spawning workload wrong.
The fix wants the stack owned by something the joiner can free — a base/layout
pair on `Thread`, freed in `join` after the tid is reaped.

### A fork depended on `toyos-abi` by **git** — the third case the rule does not cover

CLAUDE.md's rule is that forks depend on ToyOS crates *by version, never by path*,
with the reason given: a path escaping the fork's own repo cannot resolve once
cargo checks it out alone. **A git dependency is the third case, and it is worse
than the path case — because it resolves.** Silently, against a frozen snapshot,
with nothing to announce it. `toyos-abi` is the crate where a split-brain does the
most damage.

It happened here. `~/.cargo/git/checkouts/toyos-abi-9a70838a07f829d2/2fe0c57`
holds a `toyos-abi` that is not a slightly older monorepo copy but a substantially
different ABI: **seven files the monorepo does not have** (`gpu.rs`, `message.rs`,
`poll.rs`, `raw_net.rs`, `services.rs`, `shm.rs`, `system.rs`) and **missing two it
does** (`boot.rs`, `io_uring.rs`).

**Not currently live — established, not assumed.** Enumerating all fifteen
checkouts found the git form in exactly two *stale* getrandom commits
(`bb423bc`, `c473bb1`, both `toyos-abi = { git = ... }`). The three getrandom
commits actually pinned (`4659241`, `d304544`, `e05f79d`) all use
`toyos-abi = "0.1"`, and **no lockfile in the tree references
`Japabu/toyos-abi` as a git source at all.** What remains is inert cargo cache.

Filed anyway, because the near-miss is the finding: the violation occurred, ran,
and was corrected without anything in the tree ever reporting either event. The
rule should name the git case explicitly rather than leaving it to be inferred
from the path case's reasoning. Sweep the estate for the pattern when it next gets
path overrides.

### The fork estate is invisible to the zero-warning bar

Cargo passes `--cap-lints allow` to every package whose source is not a *path*
source. All 14 forks in `forks.toml` are consumed as git dependencies, so rustc
discards their warnings before anything can print them. Measured on `sshd`'s
graph: 140 of 143 units capped, the three exceptions being the local path crates
`sshd`, `toyos` and `toyos_abi`.

This is not a build-system defect and no build-system change can reach it. The
build system used to swallow cargo's diagnostics on success as well — that is
fixed — and the forks stayed invisible, because the cap is applied by cargo
upstream of anything `src/build.rs` does.

The trap to avoid is `[lints]` inside a fork: it is a manifest change, so it
lands in `git log <base>..toyos` and would put ToyOS lint policy into every
upstream PR the estate sends. Plan, procedure and the standing-mechanism
question: `specs/fork-lint-audit-plan.md`. It needs a quiet tree, because
path-overriding the forks changes what every build in the repo resolves.

### The `memmap2` fork is 165 lines of unreachable code

`rust/compiler/rustc_data_structures/src/memmap.rs` cfg-gates
`target_os = "toyos"` to a `Vec<u8>` implementation at all 8 sites, and
`rust/Cargo.toml` is the only manifest that patches memmap2 at all — userland's
duplicate entry resolved to nothing and was deleted 2026-08-01. So no ToyOS code
path calls any memmap2 API. `src/toyos.rs` is compiled and never called; the
fork's only load-bearing content is the `0.9.10 → 0.2.1` version relabel that
satisfies rustc's pin.
Either delete `src/toyos.rs` and let `stub.rs` serve, or drop the toyos gate in
`rustc_data_structures` (the only two APIs rustc uses, `map_copy_read_only` and
`map_anon`, are correct in the fork). Exactly one of the two should exist. Three
real bugs in that module were found and fixed 2026-07-28 — see `forks.toml`.

---

## 7. Design debt

### io_uring abuses shared_memory

io_uring does not share memory between processes — it shares a page between the
kernel and one userspace process. It should own its `PageAlloc` directly, map it
into the process's page tables, and store it in `IoUringInstance`; Drop frees the
pages. This also removes the only caller of `shared_memory::destroy()`.

### `SharedToken` is a bare `u32` with no RAII

Unlike `PhysPage`, which cannot leak because Drop returns it to the PMM,
`SharedToken` is `Copy` with no destructor, so the caller must remember to call
the right cleanup function. It should be a non-Copy RAII handle whose Drop
removes the region and frees the backing pages, exposing `.raw()` for the numeric
value to hand to userspace while the owning handle stays in kernel structures.

### `Fd` is a Unix-ism

ToyOS has no files-are-everything model. The integer identifies pipes, devices,
io_uring instances and IPC connections — it is a handle, not a file descriptor.
Rename `Fd` → `Handle`. Aligns with the capability-based direction.

### `gpu::set_resolution` frees the old framebuffer while consumers may hold pointers to it

`kernel/src/gpu.rs:59-76` calls into the driver, and virtio's implementation
allocates a new framebuffer and frees the old one. Today the only consumer
re-reads `GpuInfo` afterwards, so nothing breaks; the pattern is simply
unguarded for anything that caches the address. The panic console is the first
thing that would have cached one, and it handles the window explicitly —
`detach()` before the call, `rearm()` if the driver refused — which is a
per-caller workaround, not a fix. The fix is for `set_resolution` to own the
invalidation.

### `KernelSlice::from_raw` cannot check the one thing that makes the type safe

`kernel/src/mm/region.rs:16` (the live `TODO`s at `:12` and `:15`). Every bounds
check `KernelSlice` performs is against a size the caller asserted; `from_raw`
cannot validate it against the allocation, so a slice longer than its buffer
passes every check the slice makes. Three call sites, each correct only by
adjacency: `OwnedAlloc::slice` (`process.rs:70`, the one site with an assert),
the ELF loader (`elf.rs:1005`, size and allocation share `load_size` by
proximity, not by construction — and every past OOB in the loader came through
this type), and `DmaPool::alloc` (`drivers/mod.rs:34`).

Fix shape: allocators construct the slice. Give `PageAlloc` and the contiguous
PMM path a `slice()` method like `OwnedAlloc`'s, sized from the allocation they
own, then make `from_raw` private to `mm` or delete it. The loader and DmaPool
stop naming sizes at all.

---

## 8. Hardware and performance gaps

### The UEFI GOP path is off by default, and picks an absurd mode when on

Until `06ce633` no configuration in this tree produced a UEFI GOP at all:
`kernel/src/drivers/gop.rs` had never executed and `kernel_args.gop_framebuffer`
was zero everywhere. `cargo run --gop` and `BootOptions { profile: Profile::Gop }`
(`-vga std`) fixed that and the path works, but two residuals remain.

**It is not the default.** Plain `cargo run` and the default test config still
boot `-vga none` with virtio-gpu or with no display device, so `gop.rs` is
exercised only by `--gop`, by `--metal-sim` (which every machine test now
boots), and by the screen tests that boot a guest at all. Every other test in
the suite still says nothing about the display path a laptop takes.

**The mode is wrong.** `bootloader/src/main.rs:186-205` selects the mode with
the most pixels. On QEMU stdvga that is **2048x2048** — square, non-standard,
and it makes the compositor scale a 1920x1080 wallpaper to a square. It is also
16 MiB of framebuffer, which is what makes a panic-console repaint cost ~13 ms.
"Largest wins" is not a mode policy; a real one would prefer the firmware's
current mode, or the largest 16:9/16:10 mode, and only then fall back. Harmless
for M0, wrong for M1 — and M1 shipped without fixing it, so the compositor
scales a 1920x1080 wallpaper onto a 2048x2048 square on every metal-sim boot
and each panic screendump is 12 MiB. On the T14 the firmware will offer the
panel's own mode and "largest wins" may or may not pick it; that is the part
nothing here can answer.

**What the repaints actually cost.** `e5e600f`'s message gives two figures that
cannot both be true — "~13ms per repaint" and "135ms to 181ms" for six of them.
Measured A/B in one session on this host: the same tree boots to `Boot:
complete` in **118 ms** with no console armed (`-vga none`) and **188 ms** under
GOP. Five repaints happen before that line is logged and the sixth after it, so
the per-repaint figure is right (~14 ms) and the 135→181 pair is the wrong one.
Every phase boundary also carries a `wbinvd`, which QEMU ignores entirely — on
metal that is six full cache-hierarchy flushes on a machine that keeps running,
and it is not measurable here.

### A device claim succeeds on a machine that has no such device

`device::try_claim` gates `DEVICE_FRAMEBUFFER`, `DEVICE_NIC` and `DEVICE_AUDIO`
on an info struct the driver registered, so those three return
`ClaimError::Absent` when the hardware is absent — which is what makes soundd
and netd able to exit cleanly.
`DEVICE_KEYBOARD` and `DEVICE_MOUSE` are gated on nothing at all: they hand out
a `Descriptor` whether or not any driver will ever produce an event. Under
metal-sim the compositor holds both claims on a machine with no HID of any kind
and polls them forever. Harmless today, wrong in the same way the isolation
issues in §1 are wrong: a claim is supposed to be evidence.

### Every network client pays a second of boot retry on a machine with no NIC

`NetdConn::connect_blocking` (`toyos/src/net.rs:305`) retries `services::connect`
100 times at 10 ms. That is right when netd is merely slow to start and wrong
when it will never start: under metal-sim sshd sleeps 100 times, exits at
t=1.69 s on a boot that reached `Boot: complete` at 0.38 s, and its 100
`SYS_NANOSLEEP` calls are the whole of its accounting. Cheap fix: have netd
publish "no NIC" rather than not publishing at all, so the retry has something
to observe.

### The xHCI driver never gives a slot back

`init_device` enables a slot for every connected port and issues no Disable
Slot, on any path: not for the non-HID devices it walks past (the boot stick
and any hub, camera or fingerprint reader), not when Address Device fails, not
when the descriptor fetch fails, and not when the slot id comes back past the
pool's device blocks (`device.rs:194`, the `layout.device()` `None` branch).
Each of those keeps a slot for a device the driver will never talk to again.

The fourth is the one with a test behind it: `xhci_slot_exhaustion` leaves five
slots enabled with a zero DCBAA entry every run, which makes the entry's own
test the largest producer of the leak it describes.

Harmless where slots outnumber ports, which is every machine in reach: QEMU
reports 64, Intel's PCH controllers 32 or more, and no root hub has that many
ports. It stops being harmless on a controller whose slot count is below its
device count, where a HID on a later port loses its slot to a hub on an earlier
one. `xhci_slot_exhaustion` is what would catch the regression — it proves the
machine survives the shortage and that the one device which fit was enumerated
to completion, not that the right devices win it.

### The xHCI driver refuses a controller whose PAGESIZE is not 4 KiB

`init` logs OP_PAGESIZE and then asserts it is bit 0, which is the bit that
says 4 KiB; every shipping xHC sets exactly that bit. Every structure the
driver places — rings, contexts, scratchpad buffers — is sized and aligned to
a hardcoded 4 KiB, so a controller reporting 8 KiB or 64 KiB is unimplemented,
not merely unusual, and the machine says so at init instead of corrupting
memory silently.

The scratchpad is the whole exposure. Its entries are one 4 KiB page apart,
so at PAGESIZE 8 KiB with `max_scratchpad = 8` entry 7 sits at 0xF000 and the
controller writes [0xF000, 0x11000) — over entry 6 and into block 0's
interrupt ring at `dev_base`. Every other consequence runs the safe way: a
larger page size only relaxes the rule that the DCBAA and the device contexts
must not cross one.

What is still not built is honouring such a controller. If a machine ever
trips the assert, the fix is to derive `PAGE` from the register instead of
raising the bound.

### The xHCI driver resets the controller without taking ownership from firmware

`mod.rs:392` reads `hccparams1` and uses bit 2 (`csz`) and nothing else. The
xECP pointer in bits 31:16 is never followed, so there is no USBLEGSUP
BIOS-owned-semaphore handoff (xHCI §4.22.1) and no Supported Protocol parse
before the unconditional HCRST at `mod.rs:416`. Grep the whole driver for
`xecp|LEGSUP|legacy|BIOS` and only the `CAP_HCCPARAMS1` constant comes back.

QEMU cannot fail this: nothing owns the controller once OVMF's USB stack
releases it at ExitBootServices, so the path is certified exactly where it
cannot go wrong. On the T14, firmware that leaves USB legacy support armed with
SMI-on-OS-ownership gets a controller reset out from under it — the machine
fights for it, or SMIs fire, and on a serial-less laptop that presents as a
wedge after "storage ready" with the last checkpoint still on screen. It is the
one xHCI-shaped first-boot risk M1's fix does not touch.

### USB hotplug does nothing, and M1 made that reachable

`dispatch_event` handles only `EVENT_TRANSFER`, and only for a slot already in
`devices`; every other TRB type is advanced past and dropped, Port Status
Change events included. `scan_ports` has exactly one caller, inside `init`. So
the set of USB devices is whatever was connected at boot, forever.

**Read this before wiring `scan_ports` to Port Status Change events.** The
driver keeps one EP0 ring, one input context and one descriptor buffer for all
devices, and the only thing that makes that safe is that enumeration is serial:
`init_device` runs once per port, from `init`, on one CPU. The invariant is
written at `mod.rs:reset_ep0_ring` and `device.rs:scan_ports`, and hotplug is
exactly the thing that breaks it — enumeration would then run while other
devices are live, and two devices enumerating at once share an EP0 ring. What
hotplug needs is an enumeration lock, not more rings. The other half of the
problem, demuxing the event ring so a bound device's interrupt completion is
not mistaken for the enumerating device's, is already done: both waits match
the slot id and hand everything else to `dispatch_event`.

This was masked until f76ea04: a machine with no USB HID panicked the boot, so
"no keyboard, plug one in" could not happen. Now it survives, and plugging a
keyboard into a machine with no input does nothing at all — no slot enabled, no
event dispatched, and `device::try_claim(DEVICE_KEYBOARD)` already succeeded for
the compositor, so nothing reports anything and the machine is indistinguishable
from hung. That is the natural first thing to try on the T14 in the M1→M2
window. Reproducible under `--metal-sim` with QMP `device_add usb-kbd,bus=xhci.0`
after boot.

- PCID + INVPCID codepaths untested on real hardware — QEMU TCG supports
  neither. Both are CPUID-gated, so TCG falls back to a CR3 reload. Needs KVM or
  bare metal.
- TLB shootdowns still IPI all CPUs for a full flush. Per-page targeted
  shootdowns not implemented.
- The LAPIC timer uses one-shot mode; it should use TSC deadline mode
  (`IA32_TSC_DEADLINE` MSR) for precise absolute-time wakeups. The TSC is already
  calibrated for `nanos_since_boot()`.
