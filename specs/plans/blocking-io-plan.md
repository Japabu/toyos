# A CPU never waits for a device: what that costs in this kernel, and the wave that pays it

> **SUPERSEDED by `specs/completion-architecture-spec.md` (2026-08-09).** That
> document collapses this and `specs/iouring-blocking-spec.md` into one
> deliverable. §1's measurement, §3's `usb-slow-device` A/B and §5's boundary
> statement are carried over verbatim and are still the evidence.
>
> What it corrects: B1's `SleepLock` "spins where it cannot park" is a primitive
> whose behaviour depends on invisible context, and is replaced by a `Parkable`
> token that separates the two worlds at compile time; B3's deferred question —
> "a task killed while holding a `SleepLock` leaks it … it is the moment to decide
> whether it stays one" — is answered first rather than last, because with sleep
> locks it is no longer survivable; and B5's "the idle loop's whole remaining duty
> is to kick the log thread" becomes no duty at all.
>
> Read it for the measurement; build from the superseding spec.

The T14's audio pops are the log sink writing `/log` while an audio daemon waits
for the CPU it is on. Three fixes have chosen *which* CPU absorbs that stall and
a fourth was commissioned to remove it at its source — `xhci::wait_transfer`,
which spins where the hardware was designed to interrupt.

**The driver is not the source, and changing it alone changes nothing
measurable.** This document is the measurement that says so, the reframing it
forces, and the staged wave that does close it.

## 1. The measurement

`io-depth-probe` (kernel feature, `xhci/wait/mod.rs`) reports the preempt depth
at the deepest point of a disk transfer, with the backtrace that got there. It
is a measurement and not an actuator: nothing about the driver changes with it
on. Run 2026-08-08 on `wt/toyos-asyncusb` at `87835d1`, `cargo test --test
toyos-build -- usb_storage_gate`, an ordinary guest — the boot stick is USB in
every profile this suite has, so `/log` is a USB volume here exactly as it is on
the T14.

**The deepest wait in an ordinary boot runs at preempt depth 5.** The idle
loop's own path runs at 4. Both, verbatim from the guest's log:

```
[kernel 0.340 cpu0] io-depth: ... preempt depth 4, task None
    <XhciController>::bulk                            ← XHCI.lock()
    <XhciController>::bot
    <XhciController>::scsi
    <XhciController>::transfer_blocks
    xhci::with_disk::<…storage_write::{closure#0}>
    <UsbBlockDevice as BlockDevice>::write_blocks
    <FatVolume as BlockAccess>::write_at              ← fat32_adapter::VOLUMES[Log]
    <Fat32<FatVolume>>::set_fat_entry
    <Fat32<FatVolume>>::alloc_cluster
    <Fat32<FatVolume>>::ensure_capacity
    <Fat32<FatVolume>>::write
    <FatFs as vfs::FileSystem>::write_page
    <vfs::Vfs>::flush_file                            ← vfs::VFS
    <log_file::Sink>::flush
    log_file::poll                                    ← log_file::SINK
    sched::driver::idle_loop
```

```
[kernel 0.395 cpu1 tid=0] io-depth: ... preempt depth 5, task Some(0)
    … the same eleven frames …
    <log_file::Sink>::flush
    log_file::flush_final
    arch::syscall::syscall_handler                    ← the entry level
    arch::syscall::syscall_entry
```

Four ticket spinlocks are held while the transfer is waited for, and every one
of them disables preemption for its whole life (`sync.rs`, `Lock::lock`). The
number is not derivable from the call graph — a reader counting names in that
backtrace finds three, and `fat32_adapter::VOLUMES` is the fourth.

**So the CPU is unavailable to the scheduler for the whole device round trip no
matter what the driver does.** `scheduler::prepare_wait` asserts the preempt
depth equals the context's baseline (`BASELINE_TRAP = 1`, or 0 in the idle
context) precisely so that a park with a lock held is a named panic rather than
a machine that spins into `Lock::lock`'s 500M-spin `DEADLOCK`. A
`wait_transfer` that parked would trip that assertion on the very first flush.

This is also why the third row of the task's table repeated the second: every
fix so far has been aimed at the *scheduling* of a stall whose duration is set
by four locks nobody has proposed to change.

## 2. It is not the log sink either

`fd.rs:644` — every userland file write-back — is
`crate::vfs::lock().flush_file(...)`, the same `Vfs::flush_file` at the same
depth. `SYS_SHUTDOWN`'s `sync_all`, `fd::close`, and every `open` with `CREATE`
are the same shape. **Any process writing to a disk holds the VFS spinlock
across the device transfer**, so:

- moving the flush to a kernel thread does not help — the thread would hold the
  same four locks;
- moving it to a userland `logd` does not help either — its `write` and `fsync`
  syscalls hold the same four locks;
- the log sink is simply the *only* writer that runs continuously, which is why
  it is the one that shows up in gate A.

## 3. Only metal could measure this. Now the suite can

`usb-slow-device` (kernel feature, `xhci/mod.rs`, `SLOW_TRANSFER_NS`) holds
every mass-storage bulk completion back for 2 ms before the driver may see it.
The event is the controller's own and the bytes really moved; what is replaced
is when the driver is allowed to see it. A USB flash stick's 4 KiB write is tens
of milliseconds because of its erase block, and QEMU's `usb-storage` answers in
microseconds with no device, drive or machine property that delays one —
`rerror`/`werror` fail a transfer instead, and block-layer throttling never
reaches the transport's completion.

`cargo test --test toyos-build -- audio_tone --slow-usb` gives all four gate-A
configs that stick. Measured 2026-08-08, same tree, same session, host load
1.3–1.5 at the slow arm and 5.0–6.6 at the baseline arm — so the direction is
not the host's:

| config | wake latency, ordinary stick | wake latency, 2 ms stick | drains | periods submitted silent |
|---|---|---|---|---|
| `audio_tone` smp=1 | 7,117 µs | **165,948 / 165,115 µs** | 0/8 → 2/8 | 0 |
| `audio_tone` smp=8 | 10,632 µs | **259,706 / 260,579 µs** | 0/4 → 1–2/4 | **76 of 1137**, one boot of three |
| `audio_tone_load` smp=1 | 6,108 µs | 6,591 / 5,807 µs | 0/26 | 0 |
| `audio_tone_load` smp=8 | 6,174 µs | **250,912 / 247,237 µs** | 0/6 → 2/6 | 0 |

Two boots of each slow arm, taken in two separate invocations. One period is
2.902 ms and the pipeline is eight of them, 23.2 ms: **the machine is 7 to 11
whole pipelines late.** The 76 silent periods tripped gate A's own harm verdict
and its confirming boot did not reproduce them, which is the T14's report
exactly — an audible crack every few seconds, not a continuous fault.

`audio_tone_load` at smp=1 stays clean on both arms and is the control: with one
CPU and a load generator the idle loop is reached rarely, so there are few
flushes to be late for.

## 4. The wave

Each stage builds, boots and passes `cargo test`. The instrument for every one
of them is §3's command — `--slow-usb`, both arms, one session — and §1's
`io-depth-probe`, whose number must fall stage by stage. No stage may weaken
`assert_baseline` or `Lock::lock`'s deadlock panic: they are what makes a
half-converted path a named panic instead of a wedge.

### B1 — `sync::SleepLock`, and nothing uses it yet

A mutex whose contenders **park** and whose holder runs with preemption **on**.
That last half is the whole point: a `Lock` holder cannot be descheduled, and a
lock that cannot be held across a device transfer is the defect.

`lock()` parks where the calling context can park — a running task at its
baseline depth — and spins where it cannot: boot before the scheduler exists,
and the idle context. That is a complete rule and it is in the type's name, not
a fallback bolted onto a mutex. `try_lock` keeps its meaning, which is what the
panic paths and `log_file::poll` use.

Price: ~150 lines, plus a `kernel-loom` model. Loom is the only thing in this
tree that checks a memory ordering, and a park/wake handshake is exactly the
shape x86's TSO hides. No behaviour change: nothing consumes it.

### B2 — a kernel thread

A task with no address space, running a Rust function, at the trap baseline so
it may use `prepare_wait`/`block_on`. `KernelPayload::address_space` is already
`Option`; `driver::spawn`'s `expect("spawn: task without an address space")` and
a trampoline beside `process_start`/`thread_start` are the whole of the arch
work. It needs an identity the blocked-task dump can name — that is the part to
design rather than bolt on, because `sched/dump.rs` reads names out of the
process table and a thread that is in no process is exactly what it cannot
report.

Price: ~250 lines. No behaviour change: nothing spawns one.

Why a thread at all, given §2 says a thread is not sufficient: because after B3
the flush *can* park, and something has to be parked. The idle context cannot
be: a CPU that suspended a half-finished flush to run tasks resumes it only when
it next goes idle, so a busy CPU would hold the VFS indefinitely. That is a
livelock, and it is the argument for B2 rather than for `pass()` in the wait.

### B3 — the VFS lock and the log sink's lock stop disabling preemption

`vfs::VFS`, `log_file::SINK`, `fat32_adapter::VOLUMES` and
`process::ProcessData` become `SleepLock`s.

**`ProcessData` is on that list because §2's own example needs it.**
`SYS_FSYNC` (`arch/syscall.rs:234`) and `SYS_CLOSE` (`:842`) reach `fd.rs:644`'s
`flush_file` from inside `process::with_fd_owner_data`, which holds an
`Arc<Lock<ProcessData>>` — so a userland `fsync` of a disk-backed file waits
under `{ProcessData, VFS, VOLUMES, XHCI}`, the same depth as the log sink's with
one lock different. Convert only the other three and the first userland `fsync`
after B4 trips `assert_baseline` at depth 2: a userland-triggered kernel panic,
and §4 forbids weakening the assertion to make it go away.

This is the stage that closes the finding and the stage that can slip: `VFS` is
the most-taken lock in the kernel and every caller becomes a caller that may
park. The choke point is real and small — `vfs::lock()`/`vfs::try_lock()` are
the only two doors, ~25 call sites — but the blast radius is the whole
filesystem.

Two hazards to answer in the design, not in review:

- **A task killed while holding a `SleepLock` leaks it.** Today a task killed
  holding the VFS `Lock` is equally fatal and known-issues records it, so this
  is not a regression — but it is the moment to decide whether it stays one.
  CLAUDE.md's warning applies directly: a `Drop` guard binds only paths where
  the value is dropped, and "killed by another CPU" is not one.
- **Lock order.** Holding a spinlock and taking a `SleepLock` must spin, which
  B1's rule already gives. Holding a `SleepLock` and taking a spinlock is fine.
  What must not exist is a `SleepLock` taken under `preempt::disable()` by a
  caller that believed it would park.

Price: ~200 lines and the review. Instrument: `io-depth-probe` must report **2
in a syscall and 1 in the idle context** — the probe fires inside `with_disk`'s
`XHCI` guard, so `XHCI` itself is still one of the counts until B4 removes it,
and the syscall keeps its `BASELINE_TRAP` of 1. Reading 1 and 0 here is
unreachable, and a stage judged on an unreachable number is one that gets
fudged.

### B4 — the transfer submits and returns

`wait_transfer` registers what it is waiting for, drops `XHCI`, parks, and is
woken by the completion the event ring already carries. **The machinery exists**
— `toyos_xhci::job::{Await::Transfer, Stages, Outstanding}` matches a Transfer
Event to the operation that asked for it, `dispatch_event` already offers every
arriving event to it, and `advance_outstanding` already runs from
`poll_if_pending` at the top of every scheduler pass on every CPU. Teardown,
recovery and enumeration were converted at X2a/X2b; this is the fourth caller
and the one `specs/plans/xhci-port-machine-plan.md` X2c scopes.

The work is not the completion matching. It is that `msc.rs` holds
`&mut XhciController` across the whole Bulk-Only round trip, so the lock cannot
be dropped in the middle: `bot`, `framed_phase`, `transfer_blocks`, `scsi`,
`bring_up` and `request_sense` all become code that takes `XHCI` per step
against a `Copy` `MscDevice`, plus a per-disk claim so two threads cannot
interleave phases on one device.

Price: ~400 lines, the largest single stage, in the code path that boots the
owner's machine. `toyos-xhci`'s simulator cannot express waiting by design, and
that constraint is on this stage's side: everything it can hold — the claim, the
per-disk exclusion state machine — belongs there.

Deliberately out of scope, and it must be said rather than discovered: the EP0
control transfers that Reset Recovery issues keep their spin. They run only
after a device has already broken, at most three times per command, and
converting them is the stepped recovery X2c also scopes. Leaving them is a
bounded spin on an already-degraded machine; leaving them *silently* would be
the half-answer.

### B5 — the log flush moves to the kernel thread, and the heuristic is deleted

`sched::driver::flush_log_file_if_affordable`, `LOG_DEFERRAL_CEILING_NS`,
`LOG_DEFERRED_SINCE`, `owes_wake`'s use here and `log_file_flush_due`'s clause
in the pre-halt check all go. The idle loop's whole remaining duty is to *kick*
the log thread — same cadence, no period, none of the "which CPU can afford it"
question, because after B1–B4 no CPU is spending anything.

Price: ~150 lines, nearly all deletion. This is the stage that turns the
measurement in §3 into a gate: with `--slow-usb`, wake latency within one
period, drains and underruns zero.

### Ordering

B1 and B2 are independent and neither changes behaviour. B3 needs B1. B4 needs
nothing but is useless to the log path without B3. B5 needs B2, B3 and B4. The
instrument in §3 is worth re-running after every one of them, because the only
stage whose *number* should move before B5 is B3.

## 5. What this is not, and what io_uring gets

`specs/plans/iouring-blocking-spec.md` is the end state: one completion primitive, two
things a thread can park on, one park/recheck site. **Nothing here builds it**,
and nothing here should have to be torn out for it:

- B4's park is `sched::waitqs` + `scheduler::wait_until`, which is what every
  other in-kernel wait already uses. io_uring's stage 4 replaces that call, not
  the driver's shape.
- B4's completion is a `job::Outstanding` slot recorded where the event is read
  and acted on in a later pass, which is `completion::post` fanned to one
  watcher. The `Source` that names it is `Source::Disk`-shaped and additive.
- B1's `SleepLock` is not a third park target: it parks on a `KWaitQueue`, which
  is the futex channel's shape and the one thing §6.4 of that spec keeps
  native.

What that spec does *not* cover, and what this document adds to the estate: it
is written about **blocking syscalls**, and every wait in it belongs to a thread
that asked to wait. The waits here belong to a thread that asked to *write a
file* and is holding four locks while the kernel does it. That is a different
problem and it is upstream of the primitive.

## 6. What only metal can still confirm

Much less than before §3 existed, and that is the point of the actuator.

What QEMU now answers: whether the machine stays inside its audio budget while a
device is slow, on all four gate-A configs, as an A/B in one session.

What it still cannot: the T14's stick's real distribution. `SLOW_TRANSFER_NS` is
one constant and a real stick's write latency is bimodal — microseconds when the
erase block is already open, tens of milliseconds when it is not — so the *rate*
at which the harm occurs on the owner's machine is not something this stages.
The line to read on the next boot is soundd's stats:
`max_wake_lat_us` clustered near 2,902 rather than the 13,260 median and 92,608
worst of `specs/assessments/metal-logs/2026-08-08-audio-wake/`, with `drains=0` and
`max_batch=1`.
