# Known issues

Every open defect, in full. CLAUDE.md carries a one-line summary of each of these
under "Known issues" and points here for the detail; keep the two in step. An
entry leaves this file when the code and `git log` carry the fix — resolved
narrative belongs in a dated investigation doc, not here.

Verified against `a88e4ee` (2026-07-30).

---

## 1. Isolation and untrusted input

### Process isolation does not hold: pipes and sockets are ungated

`SYS_PIPE_OPEN`, `SYS_SOCKET_CREATE` and `SYS_PIPE_MAP` take a `PipeId`, which is
a small sequential integer, and check nothing. Any process can attach to any
other process's pipe or socket and read or inject, and `PIPE_MAP` hands it the
raw 2 MiB ring page. This is the most serious thing open in this tree.

Also ungated, in rough order of damage:

- `SYS_LISTEN` — no namespace, so the first process to claim a well-known name
  impersonates that service.
- `SYS_GRANT_SHARED` — any grantee can re-grant, the target PID is unvalidated,
  and there is no revoke (`kernel/src/shared_memory.rs`, `grant`).
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

### Untrusted-input panics that remain

A crafted ELF still panics the kernel outright:
`vaddr_to_file_offset` (`kernel/src/loader.rs:265`) has no failure path for a
vaddr that is in or near no `PT_LOAD` segment.

`SYS_DLOPEN` never dedups and `SYS_DLCLOSE` is a no-op (`syscall.rs:298`), so a
process can exhaust its virtual address space by repeated loads and reach
`.expect("dlopen: out of virtual address space")` at `syscall.rs:1435` and
`:1446`.

The rest of the 2026-07-28 audit is fixed: `sys_mmap(0)`/`sys_alloc_shared(0)`,
`SYS_NIC_RX_DONE`, `SYS_TLS_ALLOC_BLOCK`, io_uring's CQ-overflow assert,
`shared_memory`'s three infallible failure modes and `SYS_THREAD_SPAWN`'s stack
underflow at `a88e4ee`; the ELF `Vec::with_capacity`/`HashMap::with_capacity`
sizing, `load_shared_lib`'s unchecked `KernelSlice` offsets and the `PT_TLS`
`filesz > memsz` heap overflow at `f49c6b3`.

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

### A panic while holding `PROCESS_TABLE` hangs the panicking CPU

`try_recover_from_panic` lands in `sched::driver::idle_loop`, whose
`reap_poisoned` takes that lock unconditionally every iteration, and the dead
thread never releases it. Pre-existing and unchanged by the panic-recovery fix; a
`try_lock` could not have saved it either, since a spinlock's `try_lock` fails
for its own holder too. The general shape — locks a dead thread can strand —
belongs to the capability-handles/ownership work.

### A panic while the virtio-console TX queue is wedged *and* unlocked spins

In `submit_and_wait`. Bounding that wait is a `virtio.rs` semantics change that
needs its own discussion.

---

## 3. Kernel correctness and hazards

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

### Timer-vs-XHCI deadlock hazard

`timer_handler` (IRQ context, IF=0) → `xhci::poll_if_pending` → `XHCI.lock()`
(spinning ticket lock) deadlocks if the timer interrupts the same CPU's thread
while it holds the XHCI lock via the `fd.rs` keyboard/mouse read poll. A
`try_lock` in the timer path removes it.

### Keyboard poll readiness is spurious on repeated HID reports

`HidDevice::dispatch_report` wakes `io_uring::Source::Keyboard` watchers on every
report, but `keyboard::handle_report` diffs against the previous report and
queues zero events for identical ones (host key auto-repeat produces a stream of
these). A blocking `read` after such a wake parks the reader until the next real
key event — this froze the compositor, and the whole display, while a key was
held. Userland now reads the keyboard/mouse fds non-blocking, but the kernel
should only wake watchers when `handle_report` actually queued events. Also
inconsistent: `sys_read` on an empty Keyboard fd blocks, on an empty Mouse fd
returns `NotFound`.

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

### One unreproduced observation

`ps` appeared to stall for >2 s under heavy single-core load; later runs fine. If
seen again, capture with LLDB before restarting.

---

## 4. Audio and soundd

Spec: `specs/audio-subsystem-spec.md`. Numbered as in the 2026-07-28 audit;
(1) Poller SQ overrun, (2) `CommandRing::push` assert, (3) ungated
`SYS_SET_RT_PRIORITY`, (4) NaN volume, (7) crash detection and (9) the
"wait until clients have filled" condition are fixed (`97723dc`, `9ed8eda`,
`a88e4ee`, `069d158`).

**(5) The cpal ToyOS backend hardcodes 44100/2ch/i16** and rejects everything
else, so soundd's resampler and channel-conversion paths (spec §6/§8) are
unreachable from any real client and effectively untested. It also `assert_eq!`s
the device rate against a compile-time constant, so changing the driver's rate
aborts every cpal app.

**(6) soundd never frees the per-client shm region.** `SharedMemory::Drop` only
unmaps and nothing calls `destroy()`, so each open/close cycle strands a 2 MiB
page for soundd's lifetime.

**(8) `virtio_sound.rs` queries PCM capabilities, logs them, then calls
`configure(44100, 2)` unconditionally** and silently remaps unsupported rates to
44100. Spec §9.1–§9.3 not implemented; silent degradation.

**(10) The two TPDF dither draws are not independent** (the second is a
deterministic function of the first) — a literal §5.4 non-conformance whose
practical effect was not measured.

Lower severity: decode divides by 32768 while quantize multiplies by 32767, so
the no-conversion passthrough path is not unity gain; `AudioInfo::as_bytes`
copies 6 bytes of uninitialised kernel stack padding to userspace; unknown audio
device command bytes report success and do nothing.

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

**soundd idles at ~7% of a core**, mixing and dithering silence at period rate
with no clients. With no clients `timeout` is `u64::MAX`, so the mix loop is
purely completion-driven and only wakes once the *whole* pipeline has emptied —
a continuous drain→refill cycle at ~43/s (one every 23.2 ms) instead of a
per-period cadence. Harmless (silence over silence), but worth revisiting with
the idle policy in spec §5.8.

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

### The build system suppresses rustc warnings on success

`src/build.rs` uses `Command::output()`, which captures stderr whether or not
quiet mode is on, so the zero-warning bar is not enforced by
`cargo run -- --build-only`. The kernel builds with `-D warnings` so its warnings
are errors anyway; everything else is unchecked. To see a crate's warnings
meanwhile:

```
cd kernel && touch src/main.rs && env -u RUSTFLAGS -u RUSTC RUSTUP_TOOLCHAIN=toyos \
  PATH=<repo>/toyos-ld/target/<host>/release:$PATH cargo build --target x86_64-unknown-none
```

### `bootstrap-cc` is alive but not wired in

Settled 2026-07-28: it is *not* dead code. It is the first link of a deliberate
bootstrap chain — `toyos-cc` compiles TinyCC, TinyCC compiles progressively more
capable C, and the chain is meant to end at a working C++ compiler hosted on
ToyOS. That is a self-hosting goal, so it does not need a caller today to justify
existing. What it does need: nothing references it (no build code, no
`system.toml` entry, no test) and it is excluded from the userland workspace, so
it silently rots. It also inherits `userland/rust-toolchain.toml`, so a bare
`cargo check` in its directory cross-compiles a host-only tool to the ToyOS
target and fails in `ring`/`getrandom`; it must be built with `--target <host>`.
Its TinyCC download is https but unverified — repo.or.cz serves an on-demand cgit
snapshot whose gzip wrapper is probably not byte-reproducible, so pinning a
checksum needs a stable release tarball first. Wire it into the build so it is at
least compiled, fix the toolchain inheritance, and pin the download.

### The `memmap2` fork is 165 lines of unreachable code

`rust/compiler/rustc_data_structures/src/memmap.rs` cfg-gates
`target_os = "toyos"` to a `Vec<u8>` implementation at all 8 sites, and userland
lists memmap2 under `[patch.unused]` — so no ToyOS code path calls any memmap2
API. `src/toyos.rs` is compiled and never called; the fork's only load-bearing
content is the `0.9.10 → 0.2.1` version relabel that satisfies rustc's pin.
Either delete `src/toyos.rs` and let `stub.rs` serve, or drop the toyos gate in
`rustc_data_structures` (the only two APIs rustc uses, `map_copy_read_only` and
`map_anon`, are correct in the fork). Exactly one of the two should exist. Three
real bugs in that module were found and fixed 2026-07-28 — see `forks.toml`.

### `build_toyos_bins` belongs in the test harness, not `src/build.rs`

It is only called from the test harness and contains test-specific logic (cdylib
subcrate discovery, `-L` rustflags for `.so` linking). Move it to `tests/`.

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

- PCID + INVPCID codepaths untested on real hardware — QEMU TCG supports
  neither. Both are CPUID-gated, so TCG falls back to a CR3 reload. Needs KVM or
  bare metal.
- TLB shootdowns still IPI all CPUs for a full flush. Per-page targeted
  shootdowns not implemented.
- The LAPIC timer uses one-shot mode; it should use TSC deadline mode
  (`IA32_TSC_DEADLINE` MSR) for precise absolute-time wakeups. The TSC is already
  calibrated for `nanos_since_boot()`.
