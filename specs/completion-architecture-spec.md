# Completion Architecture — kill every wait

**This supersedes `specs/iouring-blocking-spec.md` and `specs/blocking-io-plan.md`,
which are marked superseded in place and point here.** It is one deliverable, not
two tracks: the first spec is written about *blocking syscalls* and the second
about *a thread that asked to write a file*, and the reason neither could be built
alone is §7 — a kill in a kernel that does not unwind cannot be a jump, so making
a lock parkable and making a wait cancellable are the same change.

The claim, in one sentence: **every wait in this kernel that runs on a CPU the
scheduler owns becomes a completion, and the three places a wait may still spin —
boot, an inter-CPU rendezvous, and a dying machine — are named modules a grep gate
enforces.**

Prime directive, unchanged from the rest of the estate: (1) make the bug class
unrepresentable, (2) fail fast at runtime, (3) test. Every number below came from
a command run on `wt/toyos-compl` at `19c761e` on 2026-08-09, or is cited to the
document that measured it.

---

## 1. The evidence

Four measurements, all pre-existing, all reproducible:

**1.1 Four spinlocks deep at the moment of a disk transfer.**
`io-depth-probe` (kernel feature, `kernel/src/drivers/xhci/wait/mod.rs`) reports
preempt depth **4 from the idle loop and 5 from a syscall**, with the backtrace:
`log_file::SINK` → `vfs::VFS` → `fat32_adapter::VOLUMES` → `xhci::XHCI`, each a
ticket spinlock disabling preemption for its whole life. Measured 2026-08-08 on
`wt/toyos-asyncusb` at `87835d1` (`specs/issues/audio/disk-wait-pins-a-cpu.md`,
and the full backtrace in the superseded `blocking-io-plan.md` §1). The number is
not derivable from the call graph: a reader counting names finds three.

**1.2 A realistic stick costs the audio pipeline seven to eleven times over.**
`usb-slow-device` holds every mass-storage bulk completion back 2 ms
(`SLOW_TRANSFER_NS = 2_000_000`). `cargo test --test toyos-build -- audio_tone --slow-usb`,
two boots per arm, same session, 2026-08-08:

| config | ordinary stick | 2 ms stick |
|---|---|---|
| `audio_tone` smp=1 | 7,117 µs worst wake | **165,948 / 165,115 µs** |
| `audio_tone` smp=8 | 10,632 µs | **259,706 / 260,579 µs**, 76 silent periods of 1137 |
| `audio_tone_load` smp=1 | 6,108 µs | 6,591 / 5,807 µs (the control: one CPU, rarely idle) |
| `audio_tone_load` smp=8 | 6,174 µs | **250,912 / 247,237 µs** |

One period is 2.902 ms and the pipeline is eight of them, 23.219 ms.

**1.3 Three generations of fix moved the stall between CPUs and never removed it.**
`specs/issues/audio/client-cpu-takes-the-log-flush.md` is the third: `owes_deadline`
steered the flush off soundd's CPU onto its client's, which costs the same audio,
because an audio client parks on a pipe owing no time at all.

**1.4 Gate A's thorough tier is red on `main` itself** — 7 dropout runs of 28
against a recorded `0 of 120` (`specs/issues/audio/thorough-tier-reds-on-unmodified-main.md`).
It therefore answers an A/B and never a pass/fail, which §20 turns into a protocol.

---

## 2. Why the two superseded specs could not be built as written

`blocking-io-plan.md` B3 makes `vfs::VFS`, `log_file::SINK`,
`fat32_adapter::VOLUMES` and `process::ProcessData` sleep locks, and B4 makes
`xhci::wait_transfer` park. Both are right. Neither is safe on its own, and the
reason is stated in the root `CLAUDE.md` and nowhere in either plan:

> this kernel does not unwind, so a `Drop` guard constrains only paths where the
> value is actually dropped — and "killed by another CPU" is not one.

Today a task killed while parked is disposed as an **exit** at the park
(`Commit::Killed` → `dispose_exit`, `kernel/src/sched/driver.rs:439`). Everything
on its kernel stack is abandoned without running a destructor. That is survivable
only because the one thing on that stack is a *spinlock* guard, and a spinlock
guard cannot be held across a park at all — `scheduler::assert_baseline` refuses
it. Convert the four locks to sleep locks and the same kill abandons a **held VFS
lock**, which no other CPU can ever take again. `blocking-io-plan.md` B3 names
this ("it is the moment to decide whether it stays one") and defers it.

So the ordering is forced: **cancellable waits come before sleep locks, and both
come before any lock conversion.** §7 is that change and it is the spine of this
document.

---

## 3. The rule that makes "no timers" checkable

A bare `u64` of nanoseconds is not a thing. **Every duration in the kernel is one
of four kinds, each a distinct type, and the constructor of each demands what
justifies it.**

| kind | what it is | where the number comes from | expiry means |
|---|---|---|---|
| `Bound` | the contract that says the thing will happen | a device register (NVMe `CAP.TO`), a cited spec section, or a caller | the device broke — a named error, never a retry |
| `Cadence` | how often a register **with no interrupt behind it** may be re-read | how fast the bit can physically change | nothing; it is a rate |
| `Tripwire` | a duration whose expiry is a **panic** | how long is absurd | the machine is broken; fail fast |
| `Deadline` | an absolute `Instant` a **caller** chose | userland, or a caller's own arithmetic | the caller's business |

`Bound::from_register(v, "NVMe 1.4 §3.1.1 CAP.TO")` and
`Bound::from_spec(ns, "USB 2.0 §7.1.7.3")` are the two constructors; both take the
citation as a `&'static str` and it is printed in the refusal. There is no
`Bound::from_nanos`. **A number nobody can cite is a `Tripwire` or it does not
exist**, and a `Cadence` is legal only inside a `Poll { bound, cadence }`, which
is the one type that re-reads anything.

What that deletes, by construction — none of these is any of the four kinds:

- `LOG_DEFERRAL_CEILING_NS` (1 s), `LOG_DEFERRED_SINCE` — how long the log file
  may go unwritten because every CPU owed a wake.
- `retire_task`'s `RECHECK_NS` (50 ms) — a re-poll of a state word that a
  completion should publish.
- `arch/syscall.rs:783`'s 10 ms — a serial-console read re-polling the keyboard.
- `USB_TIMEOUT_NS` (2 s) **as a transfer bound**. USB publishes none, so §12 gives
  the transfer no bound at all and names its cancellers instead. As a *register
  settle* bound it survives only if the implementer finds the xHCI section it
  comes from; inventing one is forbidden.
- `apic.rs`'s `LOG_FILE_DRAIN_NANOS` (500 ms) — reclassified as a `Tripwire` on the
  shutdown path (§17), because its expiry already logs "the panel is the only copy".

What that keeps, and why each survives:

- `arch/tlb.rs`'s `ACK_TIMEOUT_NS` (5 s) — a `Tripwire`; it already panics.
- `sched/dump.rs`'s four budgets (`ANSWER`, `NMI`, `ACK`, `TABLE`) — `Tripwire`s on
  a machine already known to be broken; their expiry degrades the report field by
  field, which is the point.
- `xhci/wait/boot.rs`'s `PORT_POLL_NS` (1 ms) — a `Cadence`, and its own comment
  already says so: "the cadence the connect settle already reads port registers on".
- `smp.rs`'s 100 ms AP wait — a `Bound`; expiry declares the AP absent by name.

---

## 4. The wait inventory

`grep -rn "core::hint::spin_loop();" kernel/src/` returns **39** calls, 23 of them
under `kernel/src/drivers/`. `scheduler::wait_until` has **6** callers,
`prepare_wait` **7** and `block_on` **7** (three of each inside `scheduler.rs`
itself). Every one is below, with its disposition. This table is the refactor's
inventory and the migration ledger in §18 counts against it.

### 4.1 Class P — a task waits. Twelve sites collapse to one.

| # | site | waits on | today's bound | after |
|---|---|---|---|---|
| P1 | `arch/syscall.rs:690` | pipe writable | none | park on the write end's completion |
| P2 | `arch/syscall.rs:797` | pipe readable | none | park on the read end's completion |
| P3 | `arch/syscall.rs:799` | virtio-sound period | none | park on the claim's completion |
| P4 | `arch/syscall.rs:804` | HDA period | none | park on the claim's completion |
| P5 | `arch/syscall.rs:809` | serial-console key | **10 ms re-poll** | park; the 10 ms is deleted (§14.3) |
| P6 | `arch/syscall.rs:1279` | accept | none | park on the listener's completion |
| P7 | `arch/syscall.rs:1202,1213` | child exit | none | park on `Source::Terminated(koid)` |
| P8 | `arch/syscall.rs:1578,1584` | thread exit | none | park on `Source::Terminated(koid)` |
| P9 | `arch/syscall.rs:1715` | an instant | caller's | park on a deadline completion |
| P10 | `io_uring.rs:410,419` | a CQE | caller's | the ring **is** an inbox (§5.2) |
| P11 | `scheduler.rs:325,330` | a futex word | caller's | park on the bucket's completion; `FUTEX_WAKE_GEN` deleted |
| P12 | `scheduler.rs:386,391` | a task's release | **50 ms re-poll + 1 s panic** | park on the release completion; the re-poll deleted, the panic kept as a `Tripwire` |

### 4.2 Class D — a CPU waits for a device on a thread's behalf. Four spins deleted.

| # | site | today | after |
|---|---|---|---|
| D1 | `xhci/wait/mod.rs:361` `wait_transfer` | spin, `XHCI` held, 2 s | submit, drop `XHCI`, park on the outstanding slot (§12) |
| D2 | `xhci/wait/mod.rs:299` `wait_command` | spin, `XHCI` held, 2 s | same |
| D3 | `nvme.rs:118` `wait_completion` | **unbounded** spin | park on the completion queue's ISR post |
| D4 | `virtio.rs:416` `submit_and_wait` | **unbounded** spin | park on the used-ring ISR post |

These four are the whole finding. They are the only spins in the kernel that run
on a thread which could have given the CPU back.

### 4.3 Class S — a register with no interrupt behind it. Spin becomes `Poll`.

`xhci/wait/mod.rs:169` (`settles`), `hda.rs:764`, `hda_probe.rs:985`,
`iommu/vtd/mod.rs:276`, `iommu/vtd/queue.rs:130`, `nvme.rs:436`, `nvme.rs:460`,
`virtio.rs:455`, `xhci/legacy.rs:179`, `rtc.rs:180`, `fat32_adapter.rs:875`,
`xhci/wait/boot.rs:117`, `hda.rs:772` and `hda_probe.rs:993` (`spin_ns`).

Three of them are written byte-for-byte three times against three different
constants — `xhci/wait/mod.rs:163`, `hda.rs:759`, `hda_probe.rs:980` — which
`specs/issues/kernel/driver-waits-without-a-deadline.md` already records. All
become one `Poll<T> { bound: Bound, cadence: Cadence }`.

**Where a `Poll` runs on a thread it parks between reads; where it runs at boot it
spins.** NVMe's two `CSTS.RDY` polls take their bound from `CAP.TO`, which
`nvme.rs:429` already reads into a local and discards.

### 4.4 Class R — an inter-CPU rendezvous with no task behind it. Unchanged.

`sync.rs:52` (`Lock::lock`), `arch/tlb.rs:131` (shootdown ack),
`arch/smp.rs:238,281,297`, `arch/apic.rs:253`, `sched/dump.rs:225,270,352,392,538`,
`log_ring.rs:328` (an ISR may log), `main.rs:357` (`debug-wait`),
`i8042/mod.rs:761` and `arch/tlb.rs:234` (test actuators).

**A completion cannot serve any of these**: there is no task to park and, for the
shootdown, the acknowledging CPU is inside an IPI handler. An agent who tries to
convert `arch::tlb` produces a deadlock, which is why they are listed rather than
left to be discovered.

### 4.5 Class X — a dying machine. Unchanged.

`serial.rs:234,272` (panic-path `try_lock` retry), `serial.rs:433` (THRE),
`panic_console/mod.rs:270,617`, `apic.rs:203`.

### 4.6 The gate

`core::hint::spin_loop()` may appear only in the files listed in 4.3 (boot arms
only), 4.4 and 4.5. A host test in `src/docs.rs`'s family walks `kernel/src` and
names any other. **That list is the scope statement, machine-checked**, and
shrinking it is the only way a later agent can claim to have removed a spin.

---

## 5. The completion core

### 5.1 What a completion is

```rust
// kernel/src/completion/mod.rs

/// A record that something happened. The consumer must match: there is no
/// `Option`, and no value that means "nothing to say".
#[derive(Clone, Copy)]
pub struct Record {
    /// Chosen by the waiter when it armed. Opaque here — the completion core
    /// maps no id to any object, so nothing in it can name a freed one.
    pub token: Token,
    pub outcome: Outcome,
    /// When the *event* happened, not when it was drained. An ISR stamps it
    /// (`irq_ring` already carries IRQ-time timestamps); an immediate fire
    /// stamps arm time.
    pub at: Instant,
}

#[derive(Clone, Copy)]
pub enum Outcome {
    Ready,
    Moved(u32),          // bytes a transfer actually moved
    Failed(SyscallError),
    /// The subject is gone: the peer's last handle closed, the port
    /// disconnected, the deadline passed. Never a bare timeout — the reason
    /// is the value.
    Gone(Reason),
}
```

`Outcome` is what makes a fallible device honest end to end. `BlockDevice`,
`FileBacking::read_page`, `bcachefs::BlockIO` and `vfs::FileSystem` are all
fallible already for the same reason (root `CLAUDE.md`, Storage); the wait was
the one link in that chain that answered `Option<(u32, u32)>` with `None` for both
"the device said no" and "nobody answered".

### 5.2 The inbox is the only thing a task parks on

```rust
/// A bounded ring of records, owned by whoever waits. Level-readable: a record
/// stays until its owner takes it, which is what collapses the park-time
/// recheck to one predicate.
pub struct Inbox { .. }
```

Exactly two kinds exist and both are the same type:

- **A thread's inbox**, in `ThreadData`, holding `MAX_INBOX` records. It is the
  kernel side of every blocking syscall.
- **An io_uring ring's inbox**, whose backing store is the CQ in the ring's own
  pages. `IoUringObject` owns those pages (the endowment spec's Stage D), so
  `io_uring` stops abusing `shared_memory` and the CQE *is* a `Record`.

There is no third. There is no global registry, no `CORE` lock and no
`HashMap<Source, Vec<RingId>>`: **a watch is a node the waiter lends to the
object**, so a post is a walk of a list under the object's own leaf lock. That is
the shape `sched::waitqs` already has, and it deletes the 128-core sharding risk
the superseded spec's §13.2 had to plan for.

### 5.3 Arming

```rust
/// Proof that a record will arrive on `inbox` for `token`. `!Copy`,
/// `#[must_use]`; `Drop` disarms. A park with nothing armed is untypeable.
#[must_use]
pub struct Armed<'a> { .. }

/// Arm a watch. Closes the arm-time TOCTOU inside itself: check readiness,
/// insert the node under the subject's leaf lock, re-check, and fire
/// immediately if it became ready — exactly what `process_poll_add` does today
/// (`io_uring.rs:576`), now in one place instead of per source.
pub fn arm<'a>(inbox: &'a Inbox, subject: &Subject, token: Token) -> Armed<'a>;

/// Park until a record is readable. Returns the first record, or `Cancelled`
/// (§7). Callers loop and re-derive: a spurious return is legal.
pub fn wait(p: &mut Parkable, armed: Armed<'_>) -> Result<Record, Cancelled>;
```

`Subject` is a borrowed reference to the object being waited on — a pipe end, a
listener, a device claim, a process object, an outstanding driver operation, or
the CPU's deadline list. It is **a reference, never an id**, so a destroyed
subject cannot be named and §5.1's "maps no id to any object" holds structurally.

### 5.4 The one park/recheck site, and its proof

`kernel/src/sched/driver.rs`'s `pass_block` park arm, in full:

```rust
Commit::Parked(committed, registration) => {
    // The ONE recheck in the kernel. One predicate. No match on a channel,
    // no per-source closure, nothing named in it.
    if inbox.has_record() {
        (pass.dispose_none().finish(), Some(registration))
    } else {
        (pass.dispose_block(committed, deadline).finish(), Some(registration))
    }
}
```

**Invariant W.** A poster executes C1 `store the record` (under the subject's leaf
lock) then C2 `claim the waiter` (the rendezvous-word CAS `toyos-sched` already
performs). A parker executes P1 `prepare_wait` (publishing `Committing`) then P2
`has_record()`.

- If C2 observes `Committing`, the commit refuses to park (`Commit::AlreadyWoken`) —
  today's protocol, unchanged.
- If C2 ran before P1, then C1 ≺ C2 ≺ P1 ≺ P2, so P2 observes the record.
- If C2 observes `Blocked`, the wake message reaches the owning CPU behind the
  pass's own mailbox drain — today's protocol, unchanged.

The proof is **source-agnostic**: it names pipes, disks, child exits and deadlines
not at all, because they are all "a record in an inbox". A new wait source cannot
re-open the window, because it has no way to add a second predicate — there is one
`dispose_block` caller and a grep gate says so.

### 5.5 What does **not** change

`toyos-sched` is not rewritten. It keeps its tasks, tickets, causes, `WaitQueue`,
`Commit`, `Registration`, `wake_direct`, deadline-in-the-`ParkedEntry` and the
`Claim::Lost → continue` arm. The completion core sits **on top** of it: a post is
"write a record, then do what a wake does today". The one change inside
`toyos-sched` is §7's, and it is a change to `Commit::Killed`'s *disposition*, not
to the handshake.

This is deliberate de-risking. The scheduler migration cost seventy defects
(`specs/metal-track-history.md`); this refactor does not reopen it.

### 5.6 The three things that go away

- **The dual-call idiom.** `complete_pending_for_event` has **10** call sites, each
  paired by hand with a queue wake — the pair
  `specs/issues/kernel/io-uring-source-half-a-wake-pair.md` records losing twice
  in one cutover. After this there is one `post`, and a ring and a thread are two
  entries on one watch list.
- **`io_uring::Source` and its `is_ready` match** (`io_uring.rs:791`). Readiness is
  the object's own question, asked once inside `arm`.
- **`waitqs::PARK_BUCKETS` and `scheduler::park_lot`.** `waitpid`, `thread_join`
  and `nanosleep` stop hashing into a parking lot and arm on the object or the
  deadline. `scheduler::wake_task(TaskId)` — the pid/tid lookup — goes with them.

---

## 6. `Parkable`, and the borrow rule

```rust
/// The right to give the CPU back. Made once per trap entry and once per
/// kernel-thread body; `of_current` asserts the context's baseline preempt
/// depth, so a caller holding a spinlock cannot make one.
///
/// Not `Copy`, not `Clone`, never stored in a struct: it is threaded down the
/// call chain by reference and that is the whole mechanism.
pub struct Parkable(());

impl Parkable {
    #[track_caller]
    pub fn of_current() -> Parkable;   // asserts baseline; panics otherwise
}
```

**`completion::wait` takes `&mut Parkable`. `SleepLock::lock` takes `&Parkable`
and the guard borrows it.** Therefore:

```rust
let g = vfs::lock(&p);          // shared borrow of p, alive while g is
completion::wait(&mut p, armed) // ← does not compile: p is already borrowed
```

**A sleep lock held across a park is a compile error.** That is the single most
important property in this document and it is the one the two superseded specs
had to state as a review rule. It gives, for free:

- `xhci::XHCI` is provably never held across a park, which is what makes
  `poll_if_pending`'s `try_lock` safe (§12).
- `sched/dump.rs` and `panic_console` have no `Parkable` — they run from
  `drain_irqs` and from a halted machine — so they **cannot** call `lock()` at
  all. A diagnostic that blocks is untypeable (§17).
- Boot has no `Parkable`, because there is no current task before
  `scheduler::init`. Boot's filesystem access is `vfs::try_lock().expect("boot: the
  VFS is uncontended")` — a true invariant on one CPU with no scheduler, and a
  named kernel-bug panic if it ever stops being one.

There is no `Parkable::boot()` and no spin fallback anywhere. A primitive that
silently degrades to a spin depending on invisible context is the sentinel class
the root `CLAUDE.md` forbids, and `blocking-io-plan.md` B1 proposed exactly that
("`lock()` … spins where it cannot"). This is the correction.

---

## 7. Cancellation — a kill is not a jump

**The load-bearing change.** Today:

```rust
// sched/driver.rs:439
Commit::Killed => (pass.dispose_exit().finish(), None),
```

A task killed at a park never returns to its own code. Its stack is discarded
without destructors. After this refactor that is fatal, because the stack may hold
a `SleepGuard`.

**After: a kill at a park is answered by `Cancelled`, always.**

```rust
Commit::Killed => (pass.dispose_none().finish(), Some(registration)),
// completion::wait then returns Err(Cancelled)
```

The task runs again on its own stack, `wait` returns `Err(Cancelled)`, every
kernel caller `?`s it out, guards drop on the way, and the thread dies at the
syscall boundary where `teardown_resources` already runs on the way to
`process::exit`. `dispose_exit` at a park is deleted; the only remaining
disposition that exits is the one a thread chooses.

Three consequences, all good:

1. **`specs/issues/kernel/retired-thread-leaks-wait-queue-node.md` closes.** The
   `Registration` on the dying thread's stack is now dropped by the thread itself,
   so a retired waiter no longer leaves a corpse in a wait list.
2. **The endowment spec's §5.1 stranded-`Arc` leak class closes structurally.** An
   `Arc` a syscall cloned before blocking is dropped by the ordinary return path.
   Their §13's row "the Arc's memory leaks / structural fix = Phase 2 try-once
   syscalls" is retired by cancellable parks instead, which is a better answer:
   it keeps one-syscall blocking I/O.
3. Process teardown stops being a special path with privileges. It is a return.

**The hazard, and its gate.** A caller that loops on `Cancelled` spins forever.
`Cancelled` is a zero-sized type kernel code cannot construct, so it cannot be
manufactured; and `ThreadData.cancelled` is set by the cancel, with `arm`
asserting `!cancelled`. **A task that re-arms after a cancel panics at the
offending call site**, named, rather than hanging. That is RT4 in §22.

---

## 8. `SleepLock`

```rust
pub struct SleepLock<T> { .. }

impl<T> SleepLock<T> {
    /// Acquire, parking if held. Preemption stays ON for the holder — the
    /// whole point: a `Lock` holder cannot be descheduled, and a lock that
    /// cannot be held across a device round trip is the defect.
    pub fn lock<'p>(&'p self, p: &'p Parkable) -> SleepGuard<'p, T>;
    /// Total, callable from any context, including an ISR and the panic path.
    pub fn try_lock(&self) -> Option<SleepGuard<'_, T>>;
    /// Who holds it, for `sched::dump`. `None` is not "free" — it is "not a
    /// task", which is what boot and a kernel thread look like.
    pub fn holder(&self) -> Option<TaskId>;
}
```

- Contenders arm on the lock's own watch list and park. Release posts to one.
- **`preempt::count()` is not raised by a `SleepGuard`**, so `assert_baseline`
  keeps meaning exactly what it means today: *a spinlock is held*. §9 depends on
  that.
- **A killed holder cannot exist** (§7): the kill is answered at the park, the
  task returns, and the guard drops on the way out. There is no poisoning, no
  `Poisoned` state and no lock-recovery machinery, because the state the guard
  protects is left consistent by the caller's own error path.
- **Loom can model the contended path, and this is not a small thing.**
  `specs/issues/kernel/lock-spin-unreachable-by-loom.md` records that
  `Lock::lock`'s spin is unreachable by loom — "loom explores a spin as an
  unbounded branch and gives up". A parking acquire has no unbounded branch, so
  `kernel-loom/tests/sleep_lock.rs` covers what nothing covers today.

Ordering: a `SleepLock` may be taken while holding a `Lock` only through
`try_lock` (the `Parkable` is unavailable at that depth — `of_current` asserted it
away). A `Lock` may be taken while holding a `SleepGuard`. The one bad shape
`blocking-io-plan.md` B3 warned about — "a `SleepLock` taken under
`preempt::disable()` by a caller that believed it would park" — is unrepresentable.

---

## 9. The locks, and the baselines that stay load-bearing

`kernel/src` declares **48** `static … Lock<…>` and performs **244** `.lock()` and
**11** `.try_lock()` calls. Five change.

| lock | today | after |
|---|---|---|
| `log_file::SINK` | `Lock<Option<Sink>>` | **deleted** — the sink is owned by the log thread and needs no lock (§11) |
| `vfs::VFS` | `Lock<Option<Vfs>>`, 33 textual call sites through 2 doors | `SleepLock<Vfs>`; every task-side door takes `&Parkable`, boot uses `try_lock` |
| `fat32_adapter::VOLUMES` | `[Lock<Option<FatDevice>>; 2]` | `SleepLock` |
| `xhci::XHCI` | `Lock<Vec<XhciController>>` | `SleepLock`; `poll_if_pending` uses `try_lock` (§12) |
| `process::ProcessData` | `Arc<Lock<ProcessData>>`, 55 `with_fd_owner_data` sites | `Arc<SleepLock<ProcessData>>` |

`ProcessData` is on the list because §2's own example needs it: `SYS_FSYNC`
(`arch/syscall.rs:234`) and `SYS_CLOSE` (`:842`) reach `fd.rs:644`'s
`flush_file` from inside `with_fd_owner_data`, so a userland `fsync` of a
disk-backed file waits under `{ProcessData, VFS, VOLUMES, XHCI}`.

### 9.1 The new baselines, and why weakening one is forbidden

`scheduler::assert_baseline` stays exactly as it is. `BASELINE_TRAP = 1`,
`BASELINE_IRQ_EXIT = 0`. What changes is *where* it fires and what a trip means:

- `Parkable::of_current()` asserts the baseline **at token construction**, so the
  failure names the entry rather than the park.
- A kernel thread's baseline is `0`; `Parkable::of_current` reads the context and
  picks. The two are not interchangeable and the token records which it was.
- `io-depth-probe` is re-sited to fire **at the park** rather than at the spin, and
  its target is **1 from a syscall and 0 from the log thread** — the trap entry's
  own level and nothing else. Not 0 and 0: that is unreachable, and a stage judged
  on an unreachable number is one that gets fudged.

**Weakening is forbidden and here is the argument, unchanged from `scheduler.rs`'s
own comment.** A park with a spinlock held parks that lock on a stack nothing
returns to, and every other CPU that takes it spins into `Lock::lock`'s
500M-spin `DEADLOCK` panic — which names the victim and never the culprit. The
assertion is the only thing that names the culprit. After this refactor a trip can
mean only one thing: a raw `Lock` still held on a path that was supposed to be
converted, i.e. **the conversion is half done**, and a half-converted path must be
a named panic rather than a wedge. Raising a baseline to make a red go away
converts a compile-and-boot failure into a field investigation.

---

## 10. Kernel threads

Three, not one, because a stuck USB enumeration must not stop the log.

| thread | owns | why it is a thread |
|---|---|---|
| `logd` | the log ring's drain into the serial and file sinks | the flush parks on a disk; something has to be parked, and the idle context cannot be (a CPU that suspended a half-finished flush resumes it only when it next goes idle — a livelock, which is `blocking-io-plan.md` B2's argument and it stands) |
| `usbd` | the xHCI port machine, enumeration, endpoint recovery, `Poll`ed register settles | `poll_if_pending` runs at the top of every pass on every CPU and may not wait |
| `iod` | the write-back queue: deferred `close` flushes, page-cache eviction write-back | `Drop` cannot take a `Parkable` (§13) |

Mechanics: a task with no user address space, at baseline 0, running a Rust
function. `driver::spawn`'s `.expect("spawn: task without an address space")`
(`driver.rs:224`) and a trampoline beside `process_start`/`thread_start` are the
whole of the arch work.

**Identity.** A kernel thread gets a `ProcessObject` in the process table whose
address space **is the kernel address space**. That is not a convenience: it is
what lets the endowment spec's `KernelPayload.address_space` become non-`Option`
(their §9.4's one surviving retype). A kernel thread naming *no* address space
would have forced that field to stay an `Option` forever. `sched/dump.rs` then
names them like anything else, which is what B2 flagged as the part to design
rather than bolt on.

---

## 11. The log subsystem, rebuilt as an ordinary client

Today it is six places and no core
(`specs/issues/design-debt/redesign-the-log-subsystem.md`): `log.rs` 64 lines,
`drivers/log_ring.rs` 549, `log_file.rs` 564, `drivers/serial.rs` 467,
`drivers/panic_console/mod.rs` 1,188, `drivers/virtio_console.rs` 221.

After: `kernel/src/log/` — **a core and three sinks, each declaring its
backpressure.**

```
log/mod.rs      the log! macro, context stamping, the GS-validity flag
log/ring.rs     the 64 KiB ring. One writer discipline, drops-and-counts.
log/serial.rs   sink: 16550 + virtio-console. Drained by logd.
log/file.rs     sink: /log. Drained by logd. Parks on the disk.
log/panel.rs    sink: the panic console. Never drained by logd — it paints.
```

**A sink that cannot keep up drops and counts. It never blocks, never does
unbounded work in a scheduler-adjacent path, and fails alone** — which is the
shape that review already reached, now with a thread to hang it on.

Deleted, by name, all of it in `kernel/src/sched/driver.rs`:

- `flush_log_file_if_affordable` (`:722`)
- `LOG_DEFERRAL_CEILING_NS` (`:701`), `LOG_DEFERRED_SINCE` (`:706`)
- `log_file_flush_due` (`:743`)
- `owes_wake` (`:832`) — its only caller is the above
- `drain_serial` (`:762`) from the idle loop, and with it the
  `BackendGuard::lock` spin that runs **with interrupts disabled**
- the four extra pre-`hlt` conditions in `execute`'s `Idle` arm (`:523`, `:534`,
  `:552`, `:564`): `log_ring::has_pending`, `i8042::verdict_due`,
  `log_file_flush_due`, `xhci::port_work_pending`

**Each of those four becomes a runnable task or an armed deadline**, which is why
the halt check can shed them rather than merely move them: a CPU does not halt
while anything is runnable, and it does not sleep past an armed deadline. That is
`toyos-sched`'s Invariant T, already proven (`scheduler-core-spec.md` §8.4).

`i8042::verdict_due` in particular: the verdict fires at an instant, so it becomes
a `usbd` deadline park. And where the i8042's interrupt line is not trustworthy —
the T14 hands over an uninitialised 8042 — the driver declares itself a **polled
device** with a `Poll { bound, cadence }` on `usbd`, which is where a device-defect
workaround belongs. It stops being an invisible 10 ms inside `SYS_READ` (§4.1 P5).

The idle loop's whole remaining body is `pass(Dispose::None)`, plus the two
`#[cfg]` probes. `drain_irqs` keeps only: consume this CPU's `irq_ring` records
and post the completions they name, plus the dump's serve. `poll_if_pending`
leaves it. **That is the checkable end state of §1.1.**

A userland `logd` was considered and rejected: it cannot log the boot that
precedes it, and it cannot log its own death.

---

## 12. xHCI

The machinery exists. `toyos_xhci::job::{Await, Stages, Outstanding}` matches a
Transfer or Command Completion Event to the operation that asked for it — **by its
Command TRB address, never by being first** — `dispatch_event` offers every
arriving event to it, and `advance_outstanding` runs from `poll_if_pending`.
Teardown and endpoint recovery were converted at X2a/X2b. This is the fourth
caller and `specs/xhci-port-machine-plan.md` X2c scopes it.

The work is not the matching. It is that `msc.rs` holds `&mut XhciController`
across the whole Bulk-Only round trip, so the lock cannot be dropped in the
middle. `bot`, `framed_phase`, `transfer_blocks`, `scsi`, `bring_up` and
`request_sense` become code that takes `XHCI` **per step** against a `Copy`
`MscDevice`, plus a per-disk claim so two threads cannot interleave phases on one
device.

### 12.1 The simulator keeps its property

`toyos-xhci`'s host simulator has no way to express waiting, **by design**, and
this stage must not give it one. The split:

| goes in `toyos-xhci` (pure, host-tested) | stays in `kernel/src/drivers/xhci/` |
|---|---|
| the per-disk **claim**: `Option<Owner>` plus its transitions, so an interleave is a state the simulator can construct | `arm`, `wait`, `Parkable` |
| which step of BOT comes next, given the last completion | the `SleepLock` and when it is dropped |
| a disconnect **cancelling** an outstanding operation | the ISR, `irq_ring`, the post |

Nothing added to `toyos-xhci` names an `Instant`, a `Deadline` or a wait. A driver
that could only be written by spinning still could not be written in it.

### 12.2 `poll_if_pending` and `try_lock`

`usbd` owns the port machine, but the event ring must still be drained promptly on
whichever CPU took the interrupt, so `drain_irqs` keeps a **bounded** step:
`XHCI.try_lock()`, drain the event ring into `Outstanding`, post, return. If the
lock is held, skip — the holder is a task that will drop it within a few
instructions, because §6 makes holding it across a park a compile error. There is
no deadlock: while a transfer is outstanding, `XHCI` is **not** held.

### 12.3 Bounds and cancellers

A bulk transfer gets **no bound**, because USB publishes none and §3 forbids
inventing one. It is cancellable, by three parties and no others:

1. **Port disconnect.** The port state machine already sees CSC and already
   cancels an outstanding recovery; it now also posts `Outcome::Gone(Disconnected)`
   to whoever is parked. This is the T14's "pull the stick" case
   (`specs/issues/hardware/pulling-the-boot-stick-freezes-the-t14.md`).
2. **Kill of the waiting thread** (§7).
3. **The driver's own Bulk-Only Reset Recovery**, which already takes both
   endpoints off their transfers before it says a word to the device — `Owed`
   carries that order — and `scsi` re-issues.

A present-but-hung device therefore parks a thread forever. That is correct and it
is honest: the machine stays alive, the thread shows in Ctrl+Alt+D as parked on
`Source::Transfer` with a duration, and the kernel has not invented a bound the
device never gave it. Today the same device pins a **CPU** for 2 s and then
returns a wrong answer.

Deliberately out of scope, stated rather than discovered: the EP0 control
transfers inside Reset Recovery keep their `Poll`. They run only after a device has
already broken, at most three times per command.

---

## 13. The write-back queue, and why `Drop` needs one

`fd::OpenFile::drop` takes the VFS lock (`fd.rs:644`). `Drop` cannot take a
`&Parkable`, so with a sleep-locked VFS it cannot flush. The root `CLAUDE.md`
already carries the weaker form of this rule ("nothing in `fd` accepts a
`&mut Vfs`").

**The flush moves out of `Drop`.** A closed file with dirty pages is pushed to a
write-back queue that `iod` drains with its own `Parkable`. That is exactly the
endowment spec's deferred zero-handle queue (their §5.2), so:

- `FileObject::on_zero_handles` → `writeback::push(file)`. One more row in their
  §5.3 table.
- `SYS_CLOSE` becomes asynchronous write-back. It never promised durability.
- `SYS_FSYNC` **submits a write-back and parks on its completion**, because a
  caller asked. `close-cannot-report-io-error` is where the honesty of that
  answer is currently owed.
- `SYS_SHUTDOWN`'s `sync_all` drains the queue and parks on the last completion.

Page-cache eviction write-back moves to `iod` for the same reason, which is what
lets `specs/issues/boot-media/cache-eviction-wedges-an-idle-cpu.md` be judged: the
idle CPU stops reaching a block device through a filesystem at all.

---

## 14. Time and the ABI

### 14.1 Types — `toyos-abi/src/time.rs` (new)

`Instant` (monotonic, `nanos_since_boot` domain) and `Duration`, both
`#[repr(transparent)] u64`, with `Add<Duration> for Instant` and
`Sub<Instant> for Instant` and **nothing else**. No `Add<Instant> for Instant`, no
`Duration → Instant` coercion. Relative/absolute confusion becomes a type error.

`Deadline(u64)` is total over the whole `u64` range: `0` is simply the past
(evaluate once, return), `u64::MAX` is a time never reached. **No sentinel branch
exists**, so there is nothing to collide with — which deletes soundd's
`delta == 0 → sleep a full period` hack at its root, because there is no encoding
for it.

### 14.2 Syscall numbers — the allocation that cannot collide

The highest allocated number today is **98**. Retired and never reusable: 2–4, 7,
11, 12, 16, 22, 23, 27, 29–30, 32–34, 46–48, 69, 71, 84.

**Coordination rule with the endowment branch, and the reviewer must check it:**

| range | owner |
|---|---|
| 99–115 | the endowment architecture (`SYS_HANDLE_*`, `SYS_SHM_*`, `SYS_PROCESS_*`, `SYS_THREAD_JOIN_H`, `SYS_RT_ENTER` — twelve names, five spare) |
| 116–127 | this spec |

This spec needs **two**: `SYS_SLEEP_UNTIL` (116) replacing `SYS_NANOSLEEP` (49,
retired) and one spare left unallocated. Everything else keeps its number with
changed semantics: `SYS_IO_URING_ENTER` (90) takes an absolute deadline,
`SYS_FUTEX_WAIT` (58) takes an absolute deadline. **This spec allocates nothing in
99–115 and reuses no retired number**; if the endowment spec needs more than
seventeen, it takes them from 128 and says so.

### 14.3 Semantics

`SYS_IO_URING_ENTER(ring, to_submit, min_complete, deadline_abs_ns)` — same four
registers. Non-blocking is `min_complete = 0`; the `timeout = 0` sentinel is
removed. `min_complete > cq_size` → `InvalidArgument` at entry.

The CQE grows a `timestamp: u64` — 24 bytes — carrying `Record.at`. It is captured
regardless (correct RT attribution requires post-time capture; a drain-time stamp
reintroduces exactly the jitter the audio subsystem documents), so carrying it into
the CQE is free generality.

Blocking `read`/`write`/`accept` **keep their ABI shape**. The try/park loop's
correctness lives in one place — the kernel — covering the std PAL, cpal and the
`toyos-cc` C bootstrap alike, at one syscall per blocking op. This is the
superseded spec's §8 reasoning and it is still right.

---

## 15. Reconciliation with the endowment architecture

`wt/toyos-endow` was **not pushed to `origin` as of 2026-08-09** (checked with
`git ls-remote --heads origin`; the remote carries `main`, `ci/probe-green`,
`wt/toyos-dumpnote`, `wt/toyos-hdaprobe`, `wt/toyos-racedoc`, `wt/toyos-std` and
nothing else). This section is therefore written against
`specs/capability-handles-spec.md` at `19c761e`, which is that work's in-tree
ancestor, and **the implementing agent must re-read
`specs/capability-endowment-spec.md` from the endowment branch before chunk C0 and
correct any row below that has moved.** The plan-reviewer should treat an
unreconciled row as a red.

| # | contact point | how this spec reconciles |
|---|---|---|
| 1 | **The handle table** (`HandleTable` inside `ProcessData`, `get` returns owned Arcs) | `ProcessData` becomes a `SleepLock` (§9). Their `get` is lock → clone → unlock, so no table borrow crosses a park; §6's borrow rule *proves* it rather than asserting it. No new lock, no new ordering edge. |
| 2 | **`Rights::WAIT`**, which their §4.2 reserves as "the Phase-2 seam" | `completion::arm` requires `WAIT` on the handle naming the subject. That is the seam, taken exactly as offered. |
| 3 | **io_uring's object kind.** Their §6.8: `IoUringObject` owns its `PageAlloc` directly, closing `io-uring-abuses-shared-memory` | Adopted unchanged. A ring's `Inbox` **is** that page's CQ (§5.2), so the two specs describe one object. Neither needs a `RingArena`; the superseded spec's 32 KiB slot allocator is dropped. |
| 4 | **`Source` keyed by `Koid`, never by a global id.** Their §7 row: `io_uring::Source::{PipeReadable, PipeWritable, Listener}` keys become `Koid` | Stronger here: §5.3's `Subject` is a *borrowed reference to the object*, so there is no key at all and a destroyed subject is unnameable. Their rule is satisfied and its residual removed. |
| 5 | **`io_uring::Source::Terminated(Koid)`**, their §9.1 step 7 | Adopted by name. P7/P8 park on it; `park_lot`, `PARK_BUCKETS` and `wake_task(TaskId)` are deleted here rather than there. |
| 6 | **`on_zero_handles` and the deferred zero queue** (their §5.2) | This spec adds one row to their §5.3 table: `FileObject::on_zero_handles → writeback::push` (§13). It **depends** on their deferred queue existing, so C12 lands after their Stage B. |
| 7 | **`KernelPayload.address_space: Option<PageTables>` → non-`Option`** (their §9.4, the one surviving retype) | §10: a kernel thread's `ProcessObject` names the **kernel** address space. Without that, this refactor would have forced the field to stay an `Option` forever. Their retype is *enabled* by this one. |
| 8 | **Their §5.1 "no Arc across block" interim rule** and §13's "structural fix = Phase 2 try-once syscalls" | Retired by §7's cancellable park, not by try-once syscalls. A killed task returns and drops its own Arcs. They keep one-syscall blocking I/O and lose the leak class. **Tell them.** |
| 9 | **Syscall numbers** | §14.2: 99–115 theirs, 116–127 this spec's, no retired number reused by either. |
| 10 | **The SDK's blocking calls** (`toyos/src/io.rs`, `toyos::ipc::FrameRx`, `Poller`) | ABI shape unchanged (§14.3), so their handle rename and this spec's deadline change touch the SDK in disjoint places: they change *what* an argument names, this changes *what a timeout means*. `Poller` is replaced by `toyos::ring::Ring` in C11 either way. |
| 11 | **`DeviceClaim`** (their §6.5) | P3/P4 park on the claim's completion, and `on_zero_handles` releasing the class posts `Outcome::Gone(Revoked)` to anyone parked. Their crash-release path gains liveness for free. |
| 12 | **Bad-handle policy flip to kill-process** (their §4.5) | §7 makes that kill safe from a parked thread, which their stage E needs and does not have today. |

Landing order: their branch lands first (the brief's ruling). C0 merges
`origin/main` after that landing and re-reads their spec.

---

## 16. Memory ordering, and the TCG price

### 16.1 Loom models — `kernel-loom/tests/`

x86's TSO gives every load acquire and every store release semantics, so a missing
acquire edge is invisible on the only architecture ToyOS boots. Loom is the only
gate. Four new models beside `ticket_lock.rs` and `tlb_shootdown.rs`:

| model | what it explores | why the guest suite cannot |
|---|---|---|
| `inbox.rs` | Invariant W (§5.4): producer stores a record then claims; consumer arms, rechecks, parks. Two producers, one consumer, and a producer that runs entirely before the arm | the race window is a handful of instructions on two CPUs; TSO makes the missing edge unobservable |
| `sleep_lock.rs` | `SleepLock` acquire/release against a parking contender and a concurrent `try_lock`; FIFO among contenders | `Lock::lock`'s spin is unreachable by loom (`lock-spin-unreachable-by-loom`); a parking acquire has no unbounded branch, so this is the **first** contended-acquire model in the tree |
| `cancel.rs` | kill racing a park racing a post, three ways: cancel before arm, between arm and commit, and after `Blocked` | the interleaving needs a remote CPU acting between two of the victim's instructions |
| `outstanding.rs` | an ISR on CPU A records into `irq_ring`, a drain on CPU B posts, a waiter on CPU C observes | three CPUs, one publication chain; nothing in QEMU orders them |

`kernel-loom` compiles the real `kernel/src/` file a second time against loom's
atomics, so each model drives the primitive rather than a transliteration. Any new
primitive that does not compile that way is the wrong shape.

### 16.2 The TCG price, and the budget that does not grow

`specs/issues/hardware/one-rmw-per-log-line-cost-350ms.md` measured **one
`fetch_add` per `write_chunk` — a few hundred a boot, uncontended — costing 350 ms
of boot** under TCG, because QEMU cannot always emit an inline host atomic for a
guest RMW and leaves the translation block to run it exclusively. Three rules
follow and each is a review item:

1. **A `Record` is a plain store under the lock the poster already holds.** Never
   a `fetch_add` on a count. The inbox's occupancy is a plain `head`/`tail` pair
   under that same leaf lock, exactly as `log_ring`'s `OWED` and cursors already
   are.
2. **The RMW budget does not grow.** A `SleepLock` acquire is one CAS, replacing a
   `Lock` acquire's one `fetch_add`, one for one across all 244 `.lock()` sites.
3. **Any A/B that adds an atomic is interleaved and re-measured on the source**,
   not on an instrumented build — that issue's own second lesson, where an added
   `log!` moved the cost somewhere the instrument could not see and disproved a
   defect the uninstrumented build reproduced 5 of 5.

---

## 17. Diagnostics from a machine whose tasks are parked

**CPUs are not parked; tasks are.** A CPU with nothing runnable halts with `IF`
set — proven by #156's own capture, eight cores at `HLT=1 RFL=0x246` — and answers
the dump's kick IPI. A CPU with `IF` clear is reached by the NMI arm. Neither
changes. Three things do:

1. **The dump physically cannot block.** `sched/dump.rs` and `panic_console` run
   from `drain_irqs` and from `halt_all_cpus`, neither of which has a `Parkable`,
   so `SleepLock::lock` is not callable there (§6). A lock they cannot get is
   reported as `held by <TaskId>` via `SleepLock::holder()` — **more** information
   than today, where the dump simply does not ask.
2. **A parked task now says what it is parked on.** `driver::ParkedInfo` gains the
   armed `Subject`'s kind. Today it carries `WaitClass`, deadline and duration; a
   thread parked on a disk transfer and one parked on a pipe are the same row.
3. **`panic_console::hold_report` leaves `drain_irqs`** for `usbd`'s housekeeping
   step with an explicit `Cadence` (its 20 ms re-read of 128 remembered pixels).
   The halted-machine pager (`page_forever`, reachable only from `halt_all_cpus`)
   is untouched — it must be, because no thread runs there.

The panic path keeps every spin it has (§4.5). `apic.rs:203`'s 500 ms wait for the
log file to drain before power-off is reclassified as a `Tripwire` whose expiry
already logs "the panel is the only copy"; it cannot become a completion, because
the thread that would post it is not going to run again.

Gates that must stay green throughout: `blocked_dump`, `screen_blocked_dump`,
`dump_nmi_probe`, `screen_panic_muted`, `disk_backtrace`, `fault_gates`,
`fpu_isolation`.

---

## 18. Migration ledger

Counted with the commands in §4; re-derive rather than trusting these to have aged.

| what | count | disposition |
|---|---|---|
| `core::hint::spin_loop();` in `kernel/src/` | 39 | 4 deleted (§4.2), 14 become `Poll` (§4.3), 21 stay and are gated to an allow-list (§4.4, §4.5) |
| …of those, under `kernel/src/drivers/` | 23 | |
| `scheduler::wait_until` callers | 6 | all → `completion::wait` |
| `scheduler::prepare_wait` call sites | 7 | 3 internal to `scheduler.rs`; all → `completion::arm` |
| `scheduler::block_on` call sites | 7 | all → `completion::wait` |
| `io_uring::complete_pending_for_event` call sites | 10 | all → one `post` on a watch list |
| `.lock()` calls in `kernel/src/` | 244 | 5 statics convert (§9); the sites under them take `&Parkable` |
| `.try_lock()` calls | 11 | unchanged in meaning; two more appear (`poll_if_pending`, boot's VFS) |
| `static … Lock<…>` declarations | 48 | 1 deleted, 4 converted, 43 unchanged |
| `vfs::lock()` / `vfs::try_lock()` textual sites | 33 | split boot from task; 2 doors keep the choke point |
| `with_fd_owner_data` sites | 55 | take `&Parkable` where they can reach a flush |
| kernel `.rs` files | 117 | |

**The 33 VFS sites and the 55 `ProcessData` sites are the blast radius, and it is
mechanical.** The choke point is real and small — `vfs::lock()`/`vfs::try_lock()`
are the only two doors — but every caller becomes a caller that may park.

Userland is untouched until C11, because §14.3 preserves the blocking ABI shape.

---

## 19. Deletion ledger

**Code deleted, by name.** `sched/driver.rs`: `flush_log_file_if_affordable`,
`LOG_DEFERRAL_CEILING_NS`, `LOG_DEFERRED_SINCE`, `log_file_flush_due`, `owes_wake`,
`drain_serial` on the idle path, four pre-`hlt` conditions, `poll_if_pending` from
`drain_irqs`. `scheduler.rs`: `wait_until`, `prepare_wait`, `block_on`,
`wake_task`, `wake_pipe_readers`, `wake_pipe_writers`, `park_lot`, `futex_wake`'s
generation protocol. `sched/waitqs.rs`: `PARK_BUCKETS`, `park_lot`.
`io_uring.rs`: `Source`, `Source::is_ready`, `complete_pending_for_event`,
`complete_pending_for_source`, `PendingPoll`'s fd keying, the `shared_memory`
dependency. `log_file.rs`: `SINK`. `xhci/wait/mod.rs`: `wait_transfer`,
`wait_command`. `nvme.rs`: `wait_completion`'s spin. `virtio.rs`:
`submit_and_wait`'s spin. Five per-source `IO_URING_WATCHERS` statics
(`net.rs`, `keyboard.rs`, `mouse.rs`, `hda.rs`, `virtio_sound.rs`).

**`specs/issues/` files closed.** Slugs only, deliberately: `src/docs.rs` resolves
every `specs/issues/<area>/<slug>.md` path written anywhere in the tree, so a full
path here would red `cargo test --lib` the moment the file is deleted.

| slug | area | closed by | note |
|---|---|---|---|
| `disk-wait-pins-a-cpu` | audio | C7+C8 | the headline |
| `client-cpu-takes-the-log-flush` | audio | C9 | there is no heuristic left to steer |
| `log-flush-is-unbounded` | boot-media | C9 | |
| `cache-eviction-wedges-an-idle-cpu` | boot-media | C13 | the idle CPU no longer reaches a block device; **verify the `rip` first** — that entry says symbolization was never done |
| `xhci-waits-are-spins` | hardware | C7 | EP0 recovery's `Poll` is the declared residual (§12.3) |
| `scheduler-pass-blocks-in-xhci` | kernel | C7 | and its second half, `sched-check` never being turned on, is C14's |
| `hotplug-blocks-a-scheduler-pass` | hardware | C7 | |
| `driver-waits-without-a-deadline` | kernel | C10 | `CAP.TO` included |
| `io-uring-abuses-shared-memory` | design-debt | C11 | jointly with the endowment spec's Stage D |
| `io-uring-source-half-a-wake-pair` | kernel | C3 | one post, no pair to halve |
| `panic-on-wedged-virtio-console-spins` | panic-path | C10 | `submit_and_wait` gets a `Bound` |
| `retired-thread-leaks-wait-queue-node` | kernel | C4 | §7's consequence 1 |
| `pre-idle-wedge-says-nothing` | diagnostics | C9 | `logd` drains during the boot phases |
| `sys-read-empty-fd-inconsistent` | kernel | C11 | one shape for every blocking read |
| `soundd-past-due-wake-max-1` | kernel | C11 | the continuous deadline |
| `close-cannot-report-io-error` | filesystem | C13 | `SYS_FSYNC` parks on a real completion |

**Verify each before deleting.** A ledger row is a claim, not a receipt: the
closing chunk re-reads the entry, confirms the reproduction is gone, and puts the
one durable rule the entry carries into the spec or doc comment that owns the
subject — the `specs/issues/README.md` protocol.

**Not closed, and must not be claimed.** `thorough-tier-reds-on-unmodified-main`
(§20 depends on it staying open), `desktop-window-child-freeze` (#156),
`hda-tone-phase-check`, `wide-phase-reds-under-load` (a TLB-stall class this
refactor may improve and does not target),
`ap-control-registers-inherit-init`.

---

## 20. Gates

### 20.1 The instrument, and how the A/B is run

Gate A's thorough tier **is red on `main` itself** and therefore cannot be a
pass/fail gate. It answers an A/B, and the protocol is:

```
# both arms, same host, same session, interleaved — never all of one then all of the other
git stash                      # or a second worktree at origin/main
cargo test --test toyos-build -- --audio-gate 30            # arm A: main
cargo test --test toyos-build -- --audio-gate 30 --slow-usb # arm A': main, slow stick
git stash pop
cargo test --test toyos-build -- --audio-gate 30            # arm B: branch
cargo test --test toyos-build -- --audio-gate 30 --slow-usb # arm B': branch, slow stick
```

Every run records its host (`host: load <1/5/15min> qemu N toyos-build N`) and
adjudicates nothing on it — the owner's 2026-08-04 ruling stands: a load-coincident
audio failure is investigated as a real defect, never re-run away.
`one-rmw-per-log-line-cost-350ms`'s second lesson binds here too: interleave the
arms, because the first uncontrolled A/B there ran all of one and then all of the
other and the host settled in between.

### 20.2 The number this refactor must produce

From §1.2, `--slow-usb`, `audio_tone` smp=1: **worst wake back inside one
pipeline.** The measured before is 165,948 µs and the ordinary-stick control is
7,117 µs. The gate is an assertion added in C14, not a number written here — it is
whatever the same-session A/B measures on the tree that lands, and the plan's job
is to say which measurement becomes the assertion. Add it to
`tests/audio-baseline.toml` with the run that produced it.

`io-depth-probe` must report **1 from a syscall and 0 from `logd`** (§9.1), against
5 and 4 today.

### 20.3 Negative controls — each must red on a tree that has the defect

| feature | what it reintroduces | what must go red |
|---|---|---|
| `reintroduce-idle-flush` | `log_file::poll()` back on the idle loop | the `--slow-usb` A/B, by the §1.2 margin |
| `sleeplock-spins` | `SleepLock::lock` spins instead of parking | `io-depth-probe`'s depth, and the `--slow-usb` A/B |
| `park-holding-a-spinlock` | one converted path keeps its raw `Lock` | `assert_baseline` panics by name |
| `drop-a-completion` | one `post` writes the record and does not claim | `blocking_read_stress` hangs inside its bound and reds |

Each carries a comment saying why nothing else can reach it, per the harness's own
rule. **A feature that replaces only a verdict makes its own gate vacuous** —
`reintroduce-idle-flush` replaces the *behaviour*, which is why it is the strongest
of the four.

### 20.4 New named tests

- `blocking_read_stress` — cross-CPU pipe ping-pong, hard wall-clock bound. The
  lost-wake canary.
- `cancel_while_parked` — kill a thread parked on a disk transfer under
  `usb-slow-device`; the process exits, the lock is free (`SleepLock::holder()` is
  `None`), and a second process reads the same file.
- `killed_holder_releases` — kill a thread holding the VFS sleep lock; the machine
  keeps mounting.
- `no_spin_outside_the_allow_list` — the §4.6 grep gate, host-side, seconds.
- `idle_loop_is_one_statement` — the halt check names exactly the scheduler's own
  conditions. A structural gate on §11's deletion, because a condition quietly
  re-added is invisible to every behavioural test.
- `sched-check` turned on somewhere, at last: `scheduler-pass-blocks-in-xhci`
  records that invariant P "has never executed against the kernel in any image or
  any test run", and that the measured window starts after `drain_irqs`. C14 fixes
  both — the window starts at the scheduler entry, and one harness profile builds
  with `sched-check`.

---

## 21. Work breakdown

Fifteen chunks on `wt/toyos-compl`. **Every chunk builds, boots, and passes
`cargo test`** — plus `cargo test` inside `toyos-sched/`, `toyos-xhci/` and
`kernel-loom/` where it touches them. No intermediate landing; one PR at the end.

**Merge cadence.** `git merge --no-ff origin/main` at the start of C0 and at every
chunk boundary that follows a landing on `main`, and at minimum once a week.
**Never rebase, never amend** — a branch is merged by hash. The endowment branch
lands before C0; C0 is the merge that brings it in.

**`Abi-Inseparable`.** §14 changes `toyos-abi` (`time.rs`, the 24-byte CQE,
`SYS_SLEEP_UNTIL`, absolute futex and ring deadlines) and its callers in the same
tree. The owner's ruling for this pipeline is one PR, so the branch carries both
and the commit that changes the ABI declares
`Abi-Inseparable: the deadline types are the kernel/userland contract and the std
PAL's sleep and futex paths are their only callers; splitting them lands a kernel
that no userland can call.` Both `cargo run -- --pr` and CI's `abi-split` check
read that trailer.

| # | chunk | delivers | gate |
|---|---|---|---|
| C0 | merge `origin/main` (post-endowment); re-read `specs/capability-endowment-spec.md` and correct §15 | baseline `io-depth-probe` + `--slow-usb` A/B recorded in this spec | suite green; §15 has no unreconciled row |
| C1 | `Bound`/`Cadence`/`Tripwire`/`Deadline`; `Instant`/`Duration`; `Parkable` | §3's four kinds exist; nothing uses them | no behaviour change |
| C2 | `kernel/src/completion/`: `Record`, `Outcome`, `Inbox`, `Subject`, `arm`, `post`. Wired **behind** the existing waitq — every wake also posts | behaviour-preserving | `kernel-loom/tests/inbox.rs` |
| C3 | the one park site. `wait_until`/`prepare_wait`/`block_on` → `completion::wait`. Futex folded in, generation protocol deleted. `park_lot`, `PARK_BUCKETS`, `wake_task` deleted. `Source::Terminated(koid)` | 12 park sites → 1 | `blocking_read_stress`; grep: one `dispose_block` caller |
| C4 | cancellable kill: `Commit::Killed` → `Cancelled`; `dispose_exit` at a park deleted; the re-arm assertion | §7 | `kernel-loom/tests/cancel.rs`; `cancel_while_parked` (with the old locks, so it only proves the return path) |
| C5 | `SleepLock` + the `Parkable` borrow rule. Nothing converted | §8 | `kernel-loom/tests/sleep_lock.rs`; `sleeplock-spins` negative control exists and reds |
| C6 | kernel threads: identity, dump naming, `logd`/`usbd`/`iod` spawned and idle | §10 | `blocked_dump` names them |
| C7 | xHCI async: `wait_transfer`/`wait_command`/`configure`; `toyos-xhci` gains the per-disk claim; `XHCI` → `SleepLock`; `poll_if_pending` → `usbd` + `try_lock` | §12 | `toyos-xhci` host tests; `usb_storage_gate`; `io-depth-probe` falls |
| C8 | `VFS`, `VOLUMES`, `ProcessData` → `SleepLock`. 33 + 55 call sites. Boot/task split | §9 | `killed_holder_releases`; `park-holding-a-spinlock` reds |
| C9 | `kernel/src/log/`: core + three sinks + `logd`. Every deletion in §11 | §11 | `idle_loop_is_one_statement`; `reintroduce-idle-flush` reds; `--slow-usb` A/B moves |
| C10 | `Poll<T>`; NVMe `CAP.TO`; virtio, HDA, IOMMU, RTC settles; the three duplicate `settles` become one | §4.3 | `no_spin_outside_the_allow_list` |
| C11 | blocking syscalls on the one shape; `SYS_SLEEP_UNTIL`; absolute deadlines; 24-byte CQE; io_uring owns its pages; `toyos::ring::Ring` replaces `Poller`; soundd's `delta == 0` hack deleted | §14 | full suite; gate A fast tier |
| C12 | the write-back queue; `FileObject::on_zero_handles`; `SYS_FSYNC` parks; page-cache eviction to `iod` | §13 | `close-cannot-report-io-error`'s reproduction |
| C13 | the deletion commit; grep gates; `specs/issues/` closures; CLAUDE.md | §19 | the deletion commit is the proof — nothing else compiles against the old surface |
| C14 | measurement: gate A thorough A/B both arms both sticks; `io-depth-probe`; `sched-check` on; assertions recorded in `tests/audio-baseline.toml` | §20 | the numbers go in this spec |

Dependencies: C4 needs C3. C5 is independent of C2–C4 and must land **before** C7
and C8. C7 needs C5. C8 needs C4, C5 and C6. C9 needs C6, C7, C8 — that is the
stage whose number moves, and it cannot move earlier. C12 needs C6 and the
endowment spec's Stage B. C11 is independent of C7–C9 and may float.

---

## 22. Failure modes and runtime fail-fast

| failure | behaviour | recovery |
|---|---|---|
| A post races a park | Invariant W: the parker's recheck observes the record | self-wake, retry — structural |
| A kill races a park | `Cancelled`; the task returns and unwinds by returning | dies at the syscall boundary |
| A killed task held a sleep lock | it cannot: the kill is answered at the park, not by discarding the stack (§7) | — |
| A device never answers | the thread parks forever; the CPU is free | Ctrl+Alt+D names the task and the subject; disconnect or kill cancels it |
| The log sink parks on a dead stick | `logd` parks; the ring drops-and-counts; every other sink keeps working | the sink's own failure disables it, as it does today |
| The inbox fills | oldest-dropped with a count, and a `Gone(Overflowed)` record so the waiter re-derives | a bounded loss, never a lost wake |
| `usbd` wedges on a broken controller | `usbd` alone parks; `logd` and `iod` are unaffected | the dump names it |
| A CPU takes an event for a transfer nobody is parked on | `Outstanding` matches by TRB address; an unmatched event is dispatched as today | unchanged |
| Boot's VFS is contended | `try_lock().expect(..)` panics by name | a kernel bug, fail fast |

Runtime fail-fast, numbered so a review can cite them:

- **RT1** `Parkable::of_current()` asserts the context's baseline preempt depth.
- **RT2** One `dispose_block` caller; a second is a grep-gate red.
- **RT3** `Armed` is `#[must_use]` and non-`Copy`; `Drop` disarms. Park-with-nothing-armed is untypeable.
- **RT4** Re-arming after `Cancelled` panics at the call site (§7).
- **RT5** A watch node found on a list whose owner is `Dead` panics — the corpse class §7 closes.
- **RT6** `SleepLock::holder()` naming the *current* task on `lock()` panics (self-deadlock), instead of hanging.
- **RT7** `Bound`/`Cadence`/`Tripwire` have no `from_nanos`; a duration with no justification does not compile.
- **RT8** `min_complete > cq_size` at `enter` → `InvalidArgument`.

---

## 23. Explicitly rejected

1. **A global completion registry with a `CORE` lock** (the superseded spec's §5,
   §13.2). It needs sharding at 128 cores, it re-keys every subject by an id in
   exactly the namespace the endowment architecture deletes, and the objects
   already own watcher lists. A borrowed `Subject` costs nothing and cannot name a
   freed object.
2. **A `RingArena` of 32 KiB slots** (superseded §5.2). `IoUringObject` owning its
   own `PageAlloc` is the endowment spec's answer and it is simpler; two specs
   describing one object is worth more than a slot allocator.
3. **Two park channels (`Ring` + `Futex`)** (superseded §6.4). The futex's value
   check belongs *before* the arm, exactly like every other readiness check, and
   the wake-generation protocol exists only because there is no level-readable
   state today. One channel, one recheck, one proof.
4. **A `SleepLock` that spins where it cannot park** (`blocking-io-plan.md` B1). A
   primitive whose behaviour depends on invisible context is the sentinel class.
   `Parkable` makes the two worlds separate at compile time instead.
5. **Poisoning a sleep lock a killed holder abandoned.** §7 makes the abandonment
   impossible, which is strictly better than making it survivable.
6. **Userspace-only blocking wrappers** (the literal `CLAUDE.md` reading). Moves
   the try/park loop into every raw consumer — the std PAL, the C bootstrap, every
   future port — at ≥2 syscalls per blocking op, and flag-days the three hottest
   syscalls. Rejected for the superseded spec's reasons, which still hold.
7. **Making `arch::tlb::shootdown` a completion.** There is no task, and the
   acknowledging CPU is inside an IPI handler. §4.4 lists it so nobody tries.
8. **Interrupt-driven serial TX.** Once `logd` owns the drain, the THRE spin runs
   on a preemptible thread and is bounded by the UART's own byte time. It buys
   throughput, not correctness, and fails the >2× rule. Revisit if `logd` is ever
   measured to be CPU-bound.
9. **A single housekeeping thread instead of three.** A stuck USB enumeration
   would stop the log, which is the property this refactor exists to remove.
10. **Posting CQEs directly from ISR context.** Needs a lock-free registry to find
    inboxes; buys single-digit µs against a 2.902 ms period. Post-time timestamps
    already preserve the fidelity (§14.3).
11. **A userland `logd`.** It cannot log the boot that precedes it, nor its own
    death.
12. **Multishot polls.** One-shot plus re-arm is what soundd does and what the
    kernel loop needs; multishot adds CQ-overflow back-pressure policy. Revisit
    with a measured re-arm cost.

---

## 24. Open risks

1. **§15 is written against an ancestor.** The endowment branch was unpushed when
   this was written. C0 re-reads it; an unreconciled row is a red.
2. **C8's blast radius.** 33 VFS sites and 55 `ProcessData` sites, in the code path
   that boots the owner's machine. The choke point is two doors and the change is
   mechanical, but a missed site is a `Parkable` that will not thread and the
   compiler finds it — which is the argument for the token over a review rule.
3. **The `--slow-usb` A/B is one constant against a bimodal reality.** A real
   stick's write latency is microseconds when the erase block is open and tens of
   milliseconds when it is not, so the *rate* of harm on the T14 is not something
   this stages. The line to read on the owner's next boot is soundd's
   `max_wake_lat_us` clustered near 2,902 with `drains=0` and `max_batch=1`.
4. **A thread parked forever on a hung device is new behaviour**, and it is
   deliberate (§12.3). If the owner wants a bound there it has to come from
   somewhere citable, and USB does not offer one.
5. **Gate A's thorough tier being red on `main`** means every verdict in C14 is a
   delta. If it goes green before C14, take the pass/fail — but do not wait for it.
