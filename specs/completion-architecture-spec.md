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

One period is 2.902 ms and the pipeline is eight of them: 8 x 128 frames at 44100 Hz = 23.219 ms. (Multiplying the rounded 2.902 gives 23.216; 23.219 is the estate's standing figure and the exact one.)

**1.3 Three generations of fix moved the stall between CPUs and never removed it.**
`specs/issues/audio/client-cpu-takes-the-log-flush.md` is the third: `owes_deadline`
steered the flush off soundd's CPU onto its client's, which costs the same audio,
because an audio client parks on a pipe owing no time at all.

**1.4 Gate A's thorough tier is red on `main` itself** — **10 dropout runs of 120
against a recorded 0 of 120, Fisher p=8.03e-4**, on `80fe031`
(`specs/issues/audio/thorough-tier-reds-on-unmodified-main.md`). The first draft
wrote "7 of 28", which is a real measurement from a *different* document and a
different session — `specs/memory-boundary-spec.md`'s M3 three-arm run. It
therefore answers an A/B and never a pass/fail, which §20 turns into a protocol.

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
of a closed set of kinds, each a distinct type, and the constructor of each
demands what justifies it.** The first draft had four; the sweep in §3.4 found two
more the kernel already needs (`Floor`, §3.1; `Budget`, §3.3). Six is the current
count and C1 owns making it total.

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

### 3.1 A fifth kind, because two things in the kernel are neither

A **`Floor`** is a duration used as a bound on *another* duration rather than as a
wait. Nothing expires; there is no caller and no register. Two instances, and the
first is why this kind has to exist:

- **`apic.rs:286`'s `MIN_ONE_SHOT_NS` (10 µs)** — the LAPIC one-shot floor.
  `OneShot::ticks` clamps every arm to it, so a count that does not outlast the
  interrupt it schedules is unrepresentable. This is #156: cpu0 gone off the T14
  on eight boots of eight, reachable from Ring 3 by a deadline already past. Its
  own doc says "Policy, not physics", bounded below by an interrupt entry plus
  `iretq` and above by `QUANTUM_NS` — so it is not `from_register`, has no spec
  section, never expires and has no caller. **The first draft named none of the
  four kinds for it and did not mention it at all**; an implementer applying RT7
  mechanically finds it unconstructible and deletes it, and #156 reopens.
- `sched/dump.rs:59`'s `ABSURD_HORIZON_NS` (1 h) — the threshold at which a parked
  task's deadline is classified `Absurd`. A predicate on a duration, not a wait.

### 3.2 What §3 deletes, and what each deletion actually costs

- `LOG_DEFERRAL_CEILING_NS` (1 s), `LOG_DEFERRED_SINCE` — **covered.** Both
  readers (`flush_log_file_if_affordable`, `log_file_flush_due`) go together, and
  a runnable `logd` under fair share cannot be starved the way "prefer a CPU that
  owes nothing" could. The promise changes shape and should be stated: today at
  most 1 s stale, after this whatever the scheduler gives `logd`, with §22's
  drops-and-counts as the backstop. **`log_file.rs:190`'s `MAX_BLOCKED_NANOS`
  (10 s) goes with them** and the first draft missed it — it exists only because
  `log_file::poll` `try_lock`s the VFS, and with a sleep-locked VFS and a parking
  `logd` there is nothing to `try_lock`, so it and its sink-disable path are dead.
- `retire_task`'s `RECHECK_NS` (50 ms) — **covered, but it is a widening and not a
  deletion, and the first draft's wording will make an implementer delete both
  halves.** The poster already exists (`payload.rs:155` `publish_released` →
  `waitqs::wake_all`), so parking is sound. But the 1 s panic at
  `scheduler.rs:378` is evaluated *only at the top of the `while` loop*, which is
  re-entered only because the 50 ms deadline returns `block_on`. Park with no
  deadline and the `Tripwire` never fires: a lost wake parks forever. **The park
  must carry `Tripwire(1 s)` itself** — one deadline instead of twenty re-polls,
  expiry a panic instead of a retry. §7.3 then re-derives that 1 s, because it now
  bounds an unwind.
- `arch/syscall.rs:783`'s 10 ms — **NOT COVERED. It is the only reason a
  serial-console read ever returns**, and the replacement named in §4.1 P5 and
  §11 is about a different device. Evidence: the 16550's IER is written to zero
  (`serial.rs:35`, "Disable all interrupts"), `virtio_console.rs` has no interrupt
  handler at all, readiness is `serial::has_data()` (`fd.rs:672`) but the park is
  on `waitqs::KEYBOARD`, whose only waker is `keyboard.rs:67` — the i8042/USB
  keyboard, a different device. **Nothing posts.** §11 gives the *i8042* a `Poll`,
  and the i8042 is the one device here whose IRQ line does work
  (`i8042/mod.rs:1554` arms it, `:1586` unmasks it, `:145` counts the edges).
  **The correct scope is a third `Poll`, on `serial::has_data`, whose `Cadence` is
  this 10 ms.** The number survives, reclassified; the deletion is withdrawn.
- `USB_TIMEOUT_NS` (2 s) **as a transfer bound.** USB publishes none for a bulk
  transfer, so §12 gives it no bound and names its cancellers instead. **As a
  register-settle bound the first draft's binary is wrong**: there are six settle
  call sites, all on the boot path, all through `settles()`, and they need at
  least three different numbers — `boot.rs:362`/`:430` (`USBSTS.HCH`, xHCI 1.2
  §5.4.1.1 gives 16 ms), `boot.rs:514`/`:526` (port reset done, USB 2.0 §7.1.7.5),
  and `boot.rs:368`/`:372` (`USBCMD.HCRST` self-clear and `USBSTS.CNR`, for which
  **xHCI 1.2 publishes no number at all** — §4.2 says only "wait until CNR is
  '0'"). C10 decides each site and says which; the two uncitable ones become
  `Tripwire`s, because a controller that never clears CNR is a broken machine.
  **Collateral the first draft missed:** `arch/tlb.rs:77` derives `ACK_TIMEOUT_NS`
  = 5 s from this constant *by name*, so splitting it orphans the one keep §3 was
  most confident about, and C10 owes that constant a new reason.
- `apic.rs`'s `LOG_FILE_DRAIN_NANOS` (500 ms) — **not a `Tripwire`.** `apic.rs:194`
  logs "the panel is the only copy" and **returns**; it deliberately does not
  panic, because the machine is already going down and a second panic loses the
  report. Under §3's own definition ("a duration whose expiry is a **panic**")
  this is a `Bound` whose expiry is a named refusal. Reclassified.

### 3.3 What §3 keeps

- `arch/tlb.rs`'s `ACK_TIMEOUT_NS` (5 s) — a `Tripwire`; it already panics
  (`tlb.rs:139`). Its *derivation* does not survive: see above.
- `sched/dump.rs`'s budgets — **not `Tripwire`s.** The first draft's own sentence
  gives it away: "their expiry degrades the report field by field, which is the
  point". None of them panics — `ANSWER` (`:213`) logs and breaks, `NMI` (`:265`)
  breaks, `TABLE` (`:530`) returns `false` and the summary says the census is
  missing. They are a **sixth shape: a `Budget`, whose expiry is a degraded
  answer.** That is exactly right for a diagnostic on a machine already known to
  be broken, and it must be constructible or the dump cannot be written. Also:
  `ACK` is declared inside `#[cfg(feature = "dump-deaf-cpu")] deaf_window()` —
  a test actuator, not one of the four, and listing it beside the three
  production ones while omitting `ABSURD_HORIZON_NS` was an error.
- `xhci/wait/boot.rs`'s `PORT_POLL_NS` (1 ms) — a `Cadence`. The value and the
  classification are right; the comment the first draft quoted is from
  `fat32_adapter.rs:871`, not from `boot.rs`, whose own text is "How often the
  settle re-reads the port registers."
- `smp.rs`'s 100 ms AP wait — behaves as a `Bound` (`:251` declares the AP absent
  by name) but is a bare inline literal with **no name and no citation**, and SDM
  §8.4.4.1's numbers are the 10 ms and 200 µs beside it, not this. C1 either finds
  a source or it is a `Tripwire`.

### 3.4 The taxonomy is not yet total, and that is C1's job

The sweep found **41 production durations in `kernel/src/`**; the first draft
named 12. The 29 it did not name include every one that fits no kind: all six
i8042 budgets (2,100 ms of boot, whose own comment says no real EC has ever been
timed), `PORT_SETTLE_CEILING_NS`, `EMPTY_BUS_NS`, `READY_BUDGET_NS`,
`HANDOFF_TIMEOUT_NS`, `PAGE_HOLD_NS`, `REPORT_HOLD_NS`, `clock.rs:47`,
`apic.rs:253`, and both `smp.rs` `delay_ms` calls.

**RT7 plus an incomplete taxonomy is a kernel that does not build**, so C1's
deliverable is not "the four kinds exist" but **"every one of the 41 has a kind
and a constructor, or a named exception"** — six kinds now: `Bound`, `Cadence`,
`Tripwire`, `Deadline`, `Floor` (§3.1), `Budget` (§3.3). A duration that still
fits none after C1 is a finding, not a licence to invent a citation.

Two further shapes C1 must not confuse with durations: `Cadence`'s definition
("how fast the bit can physically change") does not describe the cadences the
kernel has — `REPORT_CHECK_NS`, `HEALTH_PERIOD_NS`, `SNAPSHOT_INTERVAL_NS` are
cost budgets and log-rate limits, so the definition widens to "how often a thing
may be re-done, and what makes that rate affordable". And **spin *counts* are not
durations** — `serial.rs:202`'s `PANIC_LOCK_SPIN_LIMIT`, `serial.rs:422`'s
`THRE_SPIN_LIMIT`, `sync.rs`'s 50M/500M — even where a doc comment prices them in
seconds. RT7 must not reach them.

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
| P3 | `arch/syscall.rs:799` | virtio-sound period | none | park on the `DeviceClaim`'s completion |
| P4 | `arch/syscall.rs:804` | HDA period | none | park on the `DeviceClaim`'s completion |
| P5 | `arch/syscall.rs:809` | serial-console key | **10 ms re-poll** | **the 10 ms stays, as a `Cadence` in a `Poll` on `serial::has_data`** — nothing posts a serial-console key, and the park is on `waitqs::KEYBOARD`, a different device (§3.2) |
| P6 | `arch/syscall.rs:1279` | accept | none | park on the **`Acceptor`**'s `PortShared` (§15 row 5) |
| P7 | `arch/syscall.rs:1202,1213` | child exit | none | `SYS_PROCESS_WAIT(proc_h)`, parking on the `ProcessObject` |
| P8 | `arch/syscall.rs:1578,1584` | thread exit | none | park on the `ThreadObject`; `SYS_THREAD_JOIN` keeps its `Tid` (§15 row 6) |
| P9 | `arch/syscall.rs:1715` | an instant | caller's | park on a deadline completion |
| P10 | `io_uring.rs:410,419` | a CQE | caller's | the ring **is** an inbox (§5.2) |
| P11 | `scheduler.rs:325,330` | a futex word | caller's | park on the bucket's completion; `FUTEX_WAKE_GEN` deleted |
| P12 | `scheduler.rs:386,391` | a task's release | **50 ms re-poll + 1 s panic** | park on the release completion **carrying `Tripwire(1 s)` as its own deadline** — the panic is only reachable through the re-poll today, so deleting both parks forever (§3.2). §7.3 re-derives the 1 s: it now bounds an unwind |

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
`arch/smp.rs:238,281`, `sched/dump.rs:225,270,352,392,538`,
`log_ring.rs:328` (an ISR may log), `main.rs:357` (`debug-wait`),
`i8042/mod.rs:761` and `arch/tlb.rs:234` (test actuators).

**A completion cannot serve any of these**: there is no task to park and, for the
shootdown, the acknowledging CPU is inside an IPI handler. An agent who tries to
convert `arch::tlb` produces a deadlock, which is why they are listed rather than
left to be discovered.

### 4.5 Class X — a dying machine. Unchanged.

`serial.rs:234,272` (panic-path `try_lock` retry), `serial.rs:433` (THRE),
`panic_console/mod.rs:270,617`, `apic.rs:203`.

### 4.5a Class L — a hand-rolled lock the four classes missed

`serial.rs:104` is `BackendGuard::lock`'s spin: an `AtomicBool` compare-exchange
loop taken under `save_and_cli()`, so a CPU inside it is deaf to every IPI. The
root `CLAUDE.md` already names it ("unbounded too, with no contention warning and
no deadlock panic"). It is **not** a `Lock<T>`, so §9's table of statics does
not see it, and it is not a dying-machine spin, so §4.5 does not cover it — it is
the serial backend's mutual exclusion in ordinary operation, taken by every log
drain.

`logd` inherits it (§11). It stays a spin for now, because the alternative is a
sleep lock the panic path and the ISR path both need `try_lock` on and neither
has a `Parkable` — which is what the existing `try_lock` already is. **What
changes is that it is named**: it goes on the §4.6 allow-list with this
justification, and §23's rejection 8 is corrected, because "the THRE spin runs on
a preemptible thread" is false while the guard holds `cli`.

### 4.5b Class B — a boot-only spin inside a file whose other spins are deleted

`xhci/wait/mod.rs:278` is `settle_outstanding`'s spin. Its own doc comment
justifies it — "Blocking is correct here and only here: this is the boot scan, so
there is no scheduler yet". It is correct and it stays. The first draft omitted it
from all five classes, which is how §4.6's gate came to be file-granular.

### 4.6 The gate — and why it must be site-granular

The first draft said `core::hint::spin_loop()` "may appear only in the files
listed in 4.3 (boot arms only), 4.4 and 4.5". **A file-granular gate cannot see
any of the four deletions this document exists to make**, because three of the
four Class D spins share a file with a Class S spin that stays:

| deleted (§4.2) | survives in the same file (§4.3) |
|---|---|
| `xhci/wait/mod.rs:361` `wait_transfer` | `xhci/wait/mod.rs:169` `settles`, and `:278` (§4.5b) |
| `xhci/wait/mod.rs:299` `wait_command` | as above |
| `nvme.rs:118` `wait_completion` | `nvme.rs:436`, `nvme.rs:460` |
| `virtio.rs:416` `submit_and_wait` | `virtio.rs:455` |

So the gate is **an allow-list of sites, not of files**: each entry is the
enclosing function's name plus the one-line reason it may spin, and the test
matches on the function a spin is inside rather than the path it is in.
Reintroducing `wait_transfer`'s spin then reds by name. `src/docs.rs`'s family
walks `kernel/src`, resolves each spin to its enclosing `fn`, and fails on any
`fn` not on the list. **That list is the scope statement, machine-checked**, and
shrinking it is the only way a later agent can claim to have removed a spin.

**And it must not key on `spin_loop()`, because a wait need not call it.**
`grep -rnE "while .*\{\s*\}\s*$" kernel/src/` finds three bare busy-waits that are
real waits and appear in no `spin_loop` grep:

| site | what it waits for | class |
|---|---|---|
| `arch/apic.rs:253` | 10 ms of wall clock, LAPIC calibration | R — boot, no task exists |
| `arch/smp.rs:297` | `delay_ms`, the AP bring-up delays | R — boot |
| `clock.rs:53` | the HPET counter, TSC calibration | R — boot |

**Two of those are already in §4.4's own list** — it named `arch/apic.rs:253` and
`arch/smp.rs:297` as waits, correctly, and they are simply not `spin_loop()`
calls. A gate that greps for `spin_loop()` therefore licenses converting a
deleted spin into a bare `while {}` and passing. The gate matches **any** loop
whose body cannot make progress — `spin_loop()`, a bare `while` with an empty
body, and `core::hint::spin_loop` under any alias — and the allow-list carries all
of them.

**The enumeration was re-run on 2026-08-09 at `e6f7769`.** 39 `spin_loop()` calls,
plus the three bare waits above, is **42 wait sites**. Two corrections to the
first draft's classification: `serial.rs:104` and `xhci/wait/mod.rs:278` were in no
class at all (now §4.5a and §4.5b), and `arch/apic.rs:203` appears in both §4.4
and §4.5 — §4.5 is the right home, and §4.4's entry for it is removed above.
`grep -rn "spin_loop" kernel/src/` returns 41 lines, two of which are prose in doc
comments (`xhci/mod.rs:316`, `xhci/wait/mod.rs:160`); a gate matching the bare word
rather than the call reds on those.

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
fallible already for the same reason (root `CLAUDE.md`, Storage), and the wait is
the last link to join them.

**The first draft justified it wrongly** — it said the wait "answered
`Option<(u32, u32)>` with `None` for both 'the device said no' and 'nobody
answered'". It does not: `wait_transfer`/`wait_command` return
`Some((completion_code, residue))` when the device or controller said no, and
`None` only when nothing came back. `framed_phase` already splits those into
`Broke::Code` and `Broke::Silence`, and `Control`'s own doc comment records that
the tree fixed exactly this conflation once already ("Three variants and no
`Option`"). The real argument for `Outcome` is smaller and still sufficient:
**one shape for every wait**, so a caller cannot handle a disk's refusal and a
pipe's differently by accident, and `Gone(Reason)` makes "the subject went away"
a value rather than an absence.

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
  pages. `IoUringObject` owns those pages by the time this lands — the endowment
  branch's chunk 6 does that and closes `io-uring-abuses-shared-memory` with it —
  so all this adds is that the CQE *is* a `Record`.

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
"write a record, then do what a wake does today".

**One thing inside `toyos-sched` does change, and it is the retire handshake, not
a disposition.** §7.2 rewrites `handle_retire`'s two reap-in-place arms
(`cpu.rs:569`, `:575`) so that a killed task with a live kernel stack is made
runnable instead of reaped. That is the spine of this document and it carries its
own termination argument, its own host tests in `toyos-sched/` and its own loom
model (§7.3). Everything else in the crate is untouched.

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

**`completion::wait` and `SleepLock::lock` both take `&Parkable`.** The token
proves the *context* may park. It does not, and must not, encode which locks are
held.

### 6.1 The property the token actually delivers

**A function with no `Parkable` in scope cannot park, cannot take a sleep lock,
and cannot call anything that does.** That is a compile-time property, it is
transitive through the whole call graph, and it is the entire justification for
the token:

- `sched/dump.rs` and `panic_console` have no `Parkable` — they run from
  `drain_irqs` and from a halted machine — so they **cannot** call `lock()` at
  all. A diagnostic that blocks is untypeable (§17).
- Boot has no `Parkable`, because there is no current task before
  `scheduler::init`. Boot's filesystem access is `vfs::try_lock().expect("boot: the
  VFS is uncontended")` — a true invariant on one CPU with no scheduler, and a
  named kernel-bug panic if it ever stops being one.
- An ISR, `drain_irqs` and every `Drop` impl are in the same position, which is
  §13's argument stated as a type rather than as a rule.

There is no `Parkable::boot()` and no spin fallback anywhere. A primitive that
silently degrades to a spin depending on invisible context is the sentinel class
the root `CLAUDE.md` forbids, and `blocking-io-plan.md` B1 proposed exactly that
("`lock()` … spins where it cannot"). This is the correction.

### 6.2 The borrow rule this spec first proposed is wrong, and here is why

The first draft made `completion::wait` take `&mut Parkable` and `SleepLock::lock`
take `&Parkable`, so that a live `SleepGuard` borrowing the token would make
`wait(&mut p)` a compile error, and called that "a sleep lock held across a park
is a compile error … the single most important property in this document".

**It is the property that makes this refactor impossible.** Three of its own
sections require exactly the shape it forbids:

- §1.1's whole finding is the stack `log_file::SINK → vfs::VFS →
  fat32_adapter::VOLUMES → xhci::XHCI`, and §9's aim is that the CPU is given
  back *during* the transfer. `logd` parks on the disk from inside
  `Vfs::flush_file`, so `VFS` and `VOLUMES` are held across that park by
  construction (§11, §22's "the log sink parks on a dead stick").
- §8's own doc comment says it: "a lock that cannot be held across a device round
  trip is the defect".
- §2's premise — "the same kill abandons a **held VFS lock**" — is a statement
  that the VFS lock *is* held across the park. If it could not be, §7 would have
  nothing to fix.

It is also unimplementable as an API in two smaller ways, either of which is
fatal on its own: `SleepLock::lock` must park when the lock is held, and with
only `&Parkable` it cannot reach a `wait` that demands `&mut`; and two sleep
locks held at once — which `teardown_resources` does today, `ProcessData` then
`VFS` (`process.rs:989`, `:1014`) — needs the shared borrow to stack, which a
`&mut` acquire forbids.

**Recorded rather than quietly dropped**, because the claim is attractive and the
next reader will re-derive it. A reviewer who proposes it again should be shown
this paragraph.

### 6.3 What still guards a spinlock held across a park, and it is not the type system

`Lock::lock` (the ticket spinlock) takes **no** token and must not: it is called
from ISRs, from `drain_irqs`, from boot and from `Drop`, none of which has one.
So the type system cannot see a `Lock` guard, and a spinlock held across a park
stays what it is today — a **runtime** named panic:

- **RT1** `Parkable::of_current()` asserts the context's baseline preempt depth at
  token construction, so the failure names the trap entry.
- `completion::wait` re-asserts the baseline at the park, which is what
  `prepare_wait` does today and is the arm that actually catches a `Lock` taken
  half-way down the call chain.

§9.1's "weakening is forbidden" argument therefore stands unchanged and is *more*
load-bearing than the first draft implied: `assert_baseline` is not a
belt-and-braces check beside a compile-time guarantee, it is the only check there
is for that particular shape.

The one ordering rule §8 states is unaffected and still holds: a `SleepLock`
taken while a `Lock` is held must go through `try_lock`, because the baseline
assertion refuses the park.

---

## 7. Cancellation — a kill is not a jump

**The load-bearing change.** A task killed while it has a live kernel stack must
run again on that stack, because the kernel does not unwind and a discarded stack
takes every `SleepGuard` on it with it.

### 7.1 There are three reap-in-place arms, not one

The first draft named one — `Commit::Killed` in `sched/driver.rs:439` — and it is
the least important of the three. `toyos_sched::cpu::handle_retire`
(`cpu.rs:562`) reaps a task **wherever it finds it**:

| arm | `cpu.rs` | when | stack |
|---|---|---|---|
| `self.parked.remove(&key)` → `reap` | `:569` | the task has been parked for a while | **discarded, guards and all** |
| `self.rq.remove(key)` → `reap` | `:575` | woken by a release, not yet run | **discarded, guards and all** |
| `self.running` → `need_resched` | `:582` | running | fine: dies at its next safe point |

**The first arm is the one that matters and the first draft did not mention it.**
A thread parked on a disk transfer while holding the VFS sleep lock is in
`self.parked`. It is reaped in place, its stack is freed, and the VFS lock is
stranded forever — which is *precisely* the disaster §2 says the ordering exists
to prevent. `Commit::Killed` covers only the microscopic window in which the kill
lands between `prepare_wait` and `commit`, while the victim is still running.

The second arm is the same hazard one step later: a contender woken by a
`SleepLock` release, sitting in the run queue with the previous guard still on its
stack, killed before it is picked.

### 7.2 What must change

**A retire must never reap a task that has a kernel stack. It must make it
runnable with the cancel pending.** Both reaping arms become:

```rust
// parked: do not reap. Make it runnable; the kill bit is already sticky.
if let Some(entry) = self.parked.remove(&key) {
    self.rq.push(entry.task.into_ready_cancelled(self.id, now));
    return;
}
// ready: leave it alone. It already has the sticky kill bit and will
// observe it on its own stack at the next `wait`.
```

The task runs, `completion::wait` observes the kill bit and returns
`Err(Cancelled)`, every kernel caller `?`s it out, guards drop on the way, and the
thread dies at the syscall boundary. `Commit::Killed`'s arm becomes
`dispose_none` for the same reason: the task returns to its own code rather than
being switched away from forever.

**Two corrections to the first draft's code, both of which stop it compiling:**

- `Commit::Killed` is a **unit variant** (`waitq.rs:293`), so
  `Commit::Killed => (…, Some(registration))` has no `registration` to bind.
  `commit()`'s `Killed` arm already calls `self.queue.dequeue(&self.shared)`
  (`waitq.rs:384`), so that path needs no registration and `None` is correct.
- `Cancelled` is **already taken**: `toyos_sched::waitq::Cancelled` is a
  two-variant enum (`Clean`/`AlreadyWoken`, `waitq.rs:272`) and is already
  imported into the very file §7 edits (`driver.rs:38`). The new type needs a
  different name — `completion::Cancelled` in its own module, referred to
  qualified.

### 7.3 This is a change to `toyos-sched`'s retire handshake, and §5.5 said it was not

§5.5's "the one change inside `toyos-sched` is §7's, and it is a change to
`Commit::Killed`'s *disposition*, not to the handshake" is **false**, and the
correction is not cosmetic. Rewriting the two reaping arms changes the retire
protocol's own termination argument (`retire.rs`'s module note: "whichever CPU
ends up owning the task converts it to a dead task on arrival"), because a retire
no longer converts anything on arrival — it schedules the victim and waits.

Consequences the implementer owns:

- **`toyos-sched` needs its own host tests for the new arms**, alongside
  `retire.rs`'s five existing ones, and `kernel-loom/tests/cancel.rs` (§16.1)
  must cover retire-of-a-parked-task, not only kill-racing-a-commit.
- **The retirer's bound now covers an unwind, not a reap.** `retire_task`
  (`scheduler.rs:358`) blocks until the victim's word reaches `Dead`, with a
  1 s panic at `:378`. Today that is satisfied by a reap on the retirer's own
  CPU — effectively instant. After this change the victim must be scheduled,
  return up its stack, release its locks and exit, all inside 1 s. §4.1 P12 keeps
  that panic as a `Tripwire` without noticing that what it bounds has grown by
  the length of a kernel unwind on a loaded machine. **C4 must re-derive it and
  say what the new number is measured against**, or the first busy kill panics
  the machine.
- `dispose_exit` at a park is deleted; the only remaining disposition that exits
  is the one a thread chooses (`driver.rs:327`), which is the sole other caller.

### 7.4 `Cancelled` must be consumed, not sticky — or teardown panics

The first draft's hazard note and **RT4** say `ThreadData.cancelled` is set by the
cancel and `arm` asserts `!cancelled`, so "a task that re-arms after a cancel
panics at the offending call site".

**That panics on this spec's own death path.** §7 routes the dying thread through
`teardown_resources` (`process.rs:974`), which takes `ProcessData`
(`:989`) and then calls `fd::close_all` under it (`:1014`); releasing a descriptor
takes the VFS lock. After C8 both are sleep locks, so a cancelled thread whose
teardown contends on either **parks — which re-arms — and RT4 panics the kernel**.
A userland process killed while another thread is flushing a file is enough to
reach it.

The rule that works:

- **The cancel is a one-shot, consumed by the `wait` that reports it.** After
  `wait` returns `Err(Cancelled)` the flag is clear and the thread may park again,
  which is what teardown needs.
- **Termination comes from the sticky kill bit, not from the flag.** A caller that
  loops instead of propagating gets `Cancelled` again from the next `wait`,
  because `commit()` still refuses to park a killed task. The loop is a live
  spin — the same hazard, now un-diagnosed.
- So the fail-fast moves to where it can be both correct and cheap: **`wait`
  counts the cancels it has reported to one thread and panics on the second**,
  naming the call site. One cancel is the design; two is a caller that swallowed
  the first. Teardown's own parks are ordinary parks that never report a cancel,
  so they do not count.

**RT4 is restated accordingly in §22.**

### 7.5 Consequences

1. **`specs/issues/kernel/retired-thread-leaks-wait-queue-node.md` closes — but
   only because of §7.2, not because of `Commit::Killed`.** That entry is
   explicitly about `Msg::Retire` reaping a `BlockedTask`, which is the parked
   arm. The first draft cited the wrong mechanism for the right conclusion; with
   the parked arm rewritten the `Registration` is dropped by the thread itself and
   the corpse is gone.
2. **The endowment spec's §1.1 stranded-`Arc` leak class closes structurally.** An
   `Arc` a syscall cloned before blocking is dropped by the ordinary return path.
   Their §13's row "the Arc's memory leaks / structural fix = Phase 2 try-once
   syscalls" is retired by cancellable parks instead, which is a better answer:
   it keeps one-syscall blocking I/O.
3. Process teardown stops being a special path with privileges. It is a return.

**The hazard, and its gate.** A caller that loops on a cancel instead of
propagating it spins forever. `completion::Cancelled` is a zero-sized type kernel
code cannot construct, so it cannot be manufactured; and the second cancel
reported to one thread panics at the call site that asked for it (§7.4), named,
rather than hanging. That is RT4 in §22.

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

`kernel/src` declares **52** statics holding a `Lock` (§18 states the command and
why the number moves with the regex) and performs **244** `.lock()` and **11**
`.try_lock()` calls. Five change.

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

**Three is one too few, and §12.3 is why.** One `logd` owns the drain into *both*
sinks, so a `logd` parked on a hung `/log` stick stops the serial sink too — which
is rejection 9's own argument ("a stuck USB enumeration would stop the log")
reappearing one level down. Either **`logd` is two threads, one per sink**, or the
file sink's wait is bounded so it can fail and disable itself the way it does
today. C7 and C9 must agree on one of the two; §12.3 states the choice.

**Cardinality: one of each, machine-wide, and that is a decision the first draft
did not record.** At the 128-core target the root `CLAUDE.md` sets, a single
`logd` draining a 64 KiB ring fed by 128 CPUs and a single `iod` draining
write-back for 128 cores' closed files are both serialisation points nobody has
sized. §5.2's "it deletes the 128-core sharding risk" is about the *completion
core* and does not cover these. **C6 records the measurement or the reason one is
enough**; per-CPU is the obvious escape and costs nothing to leave open.

Mechanics: a task with no user address space, at baseline 0, running a Rust
function. `driver::spawn`'s `.expect("spawn: task without an address space")`
(`driver.rs:226`) and a trampoline beside `process_start`/`thread_start` are most
of the arch work — `alloc_kernel_stack` (`loader/start.rs:17`) already takes the
trampoline as a parameter and a kernel thread's is *simpler* than either existing
one (no `initial_user_state!`, no `iretq`, no `USER_CS`). Two things beside it:
`driver::spawn` derives and asserts `cr3` from the address space, and
`paging::KERNEL` is a `Lock<Option<AddressSpace>>`, so "its address space is the
kernel address space" has to be wired to that.

**A panic inside a kernel thread halts the machine, and today the same code
panicking inside a syscall does not.** `main.rs`'s recoverable-panic predicate is
`syscall_rip() != 0 && current_tid().is_some()`; a kernel thread fails the second
clause and falls through to `halt_all_cpus`. So moving the log flush off a syscall
stack converts a survivable panic into a dead machine. **C6 owns extending the
predicate** to cover a kernel thread — it has a stack to unwind to and a task to
kill, which is exactly what the predicate is testing for.

**Identity.** A kernel thread gets a `ProcessObject` in the process table whose
address space **is the kernel address space**. That is not a convenience: it is
what lets `capability-handles-spec.md` §9.4's one surviving retype —
`KernelPayload.address_space: Option<PageTables>` → non-`Option` — happen at all,
because a kernel thread naming *no* address space would have forced that field to
stay an `Option` forever. **Nobody currently owns that retype** (§15 row 12: the
endowment spec never names `KernelPayload`), so C6 does it. `sched/dump.rs` then
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

**Two more things are in the idle loop and the first draft's deletion list missed
both** (`driver.rs:667`, re-read 2026-08-09):

- `scheduler::log_health()` (`:679`) — a per-CPU ready/parked snapshot on a
  `SNAPSHOT_INTERVAL_NS` cadence. It becomes a `logd` deadline park: it is a
  periodic diagnostic and there is now a thread whose job is diagnostics. Its
  interval is a `Cadence` (§3).
- `scheduler::reap_poisoned()` (`:680`) — zombifies threads that died in panic
  recovery. **It cannot move to a kernel thread, and the type system says so.**
  `scheduler.rs:421` takes a *blocking* `PROCESS_TABLE.lock()` and calls
  `collect_orphan_zombies(table, IdleProof::new_unchecked())`; `IdleProof`
  (`process.rs:621`) is a zero-sized proof that the caller is on the per-CPU idle
  stack, and it exists because dropping the thread entry you are running on is a
  use-after-free. A §10 kernel thread has its own kernel stack and a
  `ProcessObject`, so `iod` is precisely the caller `IdleProof` forbids. Two more
  ties: its own doc names the idle loop as "the one context that provably holds
  none of the locks the panicking thread may have been holding", which an
  ordinary task taking the VFS and `ProcessData` sleep locks is not; and
  `scheduler.rs:65` names this guard's drop as the idle loop's only route to
  `BASELINE_IRQ_EXIT`.

  **So `reap_poisoned` stays in the idle loop, and §11's end state is not an empty
  one.** C9 owns re-deriving the two arguments or leaving it where it is; leaving
  it is the default and needs no justification, because it is where it already is.

The idle loop's remaining body is then `pass(Dispose::None)`, `reap_poisoned()`
and **three** `#[cfg]` probes — `deaf_window`, `metal-panic-probe` and
`heartbeat::poll` (`driver.rs:671`, `:675`, `:684`); the first draft said two.
`drain_irqs` keeps only: consume this CPU's `irq_ring` records and post the
completions they name, plus the dump's serve. `poll_if_pending` leaves it.
**That is the checkable end state of §1.1.**

**Two existing tests read `sched: cpu=` counts** (`tests/toyos.rs:8229`, `:8722`)
as upper bounds. Deleting `log_health` makes both *vacuous* rather than red, which
silently drops the "the CPU still halts" check the `i8042` one documents. C9
re-points them at whatever `logd` emits instead.

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

A bulk transfer gets no bound *from USB*, which publishes none for bulk — BOT and
SCSI publish none either, and SCSI timeouts are host policy everywhere (Linux's
`SD_TIMEOUT` is 30 s). The first draft concluded from that that the transfer needs
no bound at all, and listed three cancellers. **The list is circular and the
correct count for the case the paragraph is about is zero.**

1. **Port disconnect** — real, but **it does not fire for a hung device**. A
   device that answers nothing keeps CCS set; only a *removed* one raises CSC.
2. **Kill of the waiting thread** (§7) — real for a userland thread, and **not
   reachable for `logd`, `usbd` or `iod`**, which is who is parked on the log
   flush.
3. **Bulk-Only Reset Recovery** — **unreachable once the bound is deleted.**
   `scsi` (`msc.rs:579`) is its only caller and calls it only on `Err(broke)` from
   `bot`; `framed_phase` (`msc.rs:651`) produces `Broke::Silence` only when
   `wait_transfer` returns `None`; and `wait_transfer` (`wait/mod.rs:361`) returns
   `None` on exactly two events — **the 2 s deadline, or the port going away**.
   So canceller 3's trigger *is* the bound being removed, and canceller 1 is its
   only other trigger.

**So a present-but-hung stick parks `logd` with nothing able to end it.** What
that costs is worse than a parked thread, and §22's row had it backwards: today
the 2 s bound produces `Scsi::Broken` → `write_blocks` `Err` → `Sink::flush`
`Refusal` → `log_file.rs:317`'s `*guard = None; disable_file_sink()`, so **the file
sink turns itself off, says why, and the serial sink keeps working.** After the
change no error is ever produced, so the sink is never disabled — and §10 gives
**one** `logd` owning the drain into *both* the serial and file sinks, so the
parked thread takes the serial sink with it. On the T14, which has no serial port
and whose `/log` is the stick it booted from, that is total logging loss with no
line saying why, on the machine three freeze investigations are running blind on.
**That is §23 rejection 9's own argument — "a stuck USB enumeration would stop the
log" — reappearing inside `logd`.**

**The resolution, and it is C7's to pick before writing code.** §3 already
licenses one: *"A number nobody can cite is a `Tripwire` or it does not exist."* A
`Tripwire` on the bulk transfer costs one named panic on a machine that is
genuinely broken and preserves the whole sink-disable chain. The alternative,
which is where Linux puts it and is probably better, is a **`Budget` at the
filesystem/log layer rather than the transport** — the transfer stays unbounded,
and the *caller* that cannot wait forever says so. What is not available is "no
bound anywhere", because that is the option with zero cancellers.

Whichever is chosen, the honest version of the win stands: the machine stays
alive, the CPU is free, and Ctrl+Alt+D names the thread and its subject (§17.2).
Today the same device pins a **CPU** for 2 s — and then returns a *correct* answer,
an I/O error all the way to `SyscallError::Io`. The first draft said "a wrong
answer"; the defect is the pinned CPU and the latency, not the value.

**`wait_command` (D2) is a separate question the first draft folded into this
one.** A Command Completion is answered by the **xHC**, not by a device, so an xHC
that never answers is a broken controller — §3's `Tripwire` definition verbatim
("how long is absurd… the machine is broken; fail fast"). None of the three
cancellers reaches a command: a port disconnect does not cancel a Disable Slot,
Reset Recovery is BOT-only, and `usbd` is not killed. xHCI 1.2 also gives the
Command Ring an abort mechanism a transfer ring does not have — **CRCR.CA /
CRCR.CS (§4.6.1.2, §5.4.5)** — which is the citable canceller, and Linux bounds
the `CRCR.CRR → 0` handshake at 5 s. **D2 gets a `Tripwire` and the abort path;
D1's answer above is not its answer.**

Deliberately out of scope, stated rather than discovered: the EP0 control
transfers inside Reset Recovery keep their `Poll`. They run only after a device has
already broken, at most three times per command — **and their `Bound` is citable
after all**: USB 2.0 §9.2.6.4 bounds standard device requests (50 ms with no data
stage, 500 ms per data packet, 5 s total for data-to-device). §3's blanket "USB
publishes none" told the implementer no citation exists for the one bound this
paragraph still needs. Read §9.2.6 end to end before writing the number: the
Bulk-Only Mass Storage Reset is a class-specific request, not a standard one.

---

## 13. The write-back queue, and why `Drop` needs one

`fd::OpenFile::drop` takes the VFS lock at **`fd.rs:46`** — the first draft said
`fd.rs:644`, which is `fd::fsync`, a different function that this section *keeps*
synchronous. `Drop` cannot take a `&Parkable`, so with a sleep-locked VFS it
cannot flush. The root `CLAUDE.md` already carries the weaker form of this rule
("nothing in `fd` accepts a `&mut Vfs`").

**The flush moves out of `Drop`.** A closed file with dirty pages is pushed to a
write-back queue that `iod` drains with its own `Parkable`. That is exactly the
endowment spec's deferred zero-handle queue (their §1.1), so:

- `FileObject::on_zero_handles` → `writeback::push(file)` (§15 row 9b).
- `SYS_CLOSE` becomes asynchronous write-back. It never promised durability.
- `SYS_FSYNC` **submits a write-back and parks on its completion**, because a
  caller asked. `close-cannot-report-io-error` is where the honesty of that
  answer is currently owed.
- `SYS_SHUTDOWN`'s `sync_all` drains the queue and parks on the last completion.

### 13.1 The flush is not the only thing `Drop` does under that lock

`OpenFile::drop` is two operations, and the first draft moved one:

```rust
let mut vfs = crate::vfs::lock();
if self.modified { vfs.flush_file(...) }       // ← moved to iod
if file_cache::release(self.file_id) {          // ← unconditional; not addressed
    vfs.close_file(&self.path, self.file_id);
}
```

The second half runs whether or not the file was modified, **so moving the flush
does not get the VFS lock out of `Drop` at all** — C12 is not done until it is
handled. And the ordering it implies is a data-loss bug: `file_cache::release`
(`file_cache.rs:123`) calls `drop_file` when the refcount hits zero and the file
is `evictable`, which is every disk-backed file — bcachefs always
(`bcachefs_adapter.rs:194`) and FAT32 after its first flush (`vfs.rs:568`, "so its
pages were unevictable up to here. They are on disk now"). Today the order is
*flush, then discard the pages*. With a deferred flush the pages are discarded
**before** the write-back has run and the only copy is on a disk that does not
have it yet.

**C12 therefore owes two things the first draft does not name**: the pages stay
pinned until `iod` reports the write-back complete, and a re-open before that
completion sees the dirty pages rather than the device.

### 13.2 Two existing tests rely on close being synchronous, and one is a named gate

"`SYS_CLOSE` … never promised durability" is defensible as policy. It is not
currently safe:

- **`disk_backtrace`** — `tests/toyos-rust-tests/src/bin/disk_backtrace.rs:27`
  does `fs::write` to `/home/disk_backtrace/child` with **no `sync_all`**, then
  `Command::new(ON_DISK)`, and the loader demand-pages the binary off the device.
  `/home` is bcachefs and evictable. This is a gate the root `CLAUDE.md` names.
- **`esp_files`** — `esp_files.rs:122` writes a note to `/log` and reads it back
  immediately. FAT32, evictable after the first flush.

Checked and *not* counter-examples: `nvme_home_roundtrip`, `fs_truncate_persist`,
`home_backing_revoked` and `cache_eviction` all call `sync_all()` explicitly;
`std_fs_write` is on `/tmp`, whose files are non-evictable. **C12 carries both test
names**, and §13.1's pinning is what makes them pass rather than a change to them.

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
exists**, so there is nothing to collide with.

**This inverts the meaning of `0` inside the kernel, at nine call sites, and the
failure mode is silent.** `scheduler::block_on`'s contract today is the opposite —
`scheduler.rs:179`, "`deadline = 0` means no timeout", implemented as
`(deadline > 0).then(|| Nanos(deadline))`. Every in-kernel caller relies on it:
`arch/syscall.rs:690`, `:797`, `:799`, `:804`, `:1213`, `:1279`, `:1584`, and
`scheduler.rs:330` through `process::futex_wait`. A site left passing `0` after
the change goes from "block forever" to "return immediately" — a busy loop, not a
compile error, and no test asserts on it.

**So `Deadline` must not be a `u64` newtype with a public constructor.** Make the
absolute form unconstructible from a bare integer: `Deadline::at(Instant)`,
`Deadline::never()`, `Deadline::passed()`. Then the nine sites do not compile
until each has been read, which is the whole point of §3.

Two live sentinel collisions the removal must resolve rather than inherit:

- `io_uring.rs:365` already carries a **third** value: it maps relative `0`
  (non-blocking) to absolute `1`, and `:419` maps `1` back to `0` (block forever).
  That line is unreachable only because `:388` returns first. It is exactly the
  latent trap this section exists to remove and it should be cited as the
  motivating example.
- **soundd's hack is `.max(1)`, not "sleep a full period".** `userland/soundd/src/main.rs:1003`
  is `((target - now) as u64).max(1)` with the comment "timeout 0 is the kernel's
  non-blocking sentinel"; `:996` already picks the next future grid point, so the
  full-period half was fixed a generation ago. `soundd-past-due-wake-max-1` is the
  open entry for what is left, and it notes the `.max(1)` is survivable only
  *because* of `MIN_ONE_SHOT_NS` (§3.1). The deletion is right; the first draft's
  description of what is being deleted is stale.

### 14.2 Syscall numbers — the allocation that cannot collide

The highest number allocated on `main` today is **98**; 21 numbers in `0..=98` are
gaps and none is reusable.

**`specs/capability-endowment-spec.md` §3.1 takes 99–112** (fourteen new calls)
and retires thirteen more (26, 31, 36–39, 65, 68, 70, 76, 85, 87, 96). It lands
first. Its §9 merge rule is that a number added on `main` while it is open
**shifts its own block up** rather than being resolved by picking one, so its
top is not fixed until it merges.

All three numbers re-verified on `origin/main` at 2026-08-09: 78 constants,
highest **98**, exactly **21** gaps (2, 3, 4, 7, 11, 12, 16, 22, 23, 27, 29, 30,
32, 33, 34, 46, 47, 48, 69, 71, 84), and their thirteen retirements resolve to
thirteen distinct names in the tree. **`SYS_NANOSLEEP` is 49 and the endowment
branch does not touch it** — no double-claim.

**This spec therefore allocates nothing until C0 reads the merged tree**, and
takes **the first clean number, computed and never written down here**. The first
draft said "expected 113, and C0 asserts it rather than assuming it", and a
literal one clause away from the word "asserts" is what an implementer
hard-codes. `assert_eq!(first_clean, 113)` is wrong in two live cases: `main` has
already moved seven commits past the endowment branch's fork point and any
landing that adds a number shifts their block up (their §9); and if they take
their own §10.1 option 2 — chunks 0+1 as an earlier PR, which their spec calls
"the recommendation" — then 99–112 has not landed and the first clean number is
**99** (§15 row 19). C0 computes it from the merged `toyos-abi` and asserts only
that it is clean. It needs exactly one:

| new | replaces |
|---|---|
| `SYS_SLEEP_UNTIL` | `SYS_NANOSLEEP` (49), retired |

Everything else keeps its number with changed semantics: `SYS_IO_URING_ENTER`
(90) and `SYS_FUTEX_WAIT` (58) take absolute deadlines.

Two consequences of the one number, both already owned by the endowment branch
and named here so neither is done twice: their chunk 9 sizes the syscall profile
array from the ABI rather than at `[u32; 64]`
(`specs/issues/diagnostics/syscall-profile-is-64-bins-wide.md`), which one more
number does not change; and their `retired_syscalls!` macro takes 49's gravestone
as one more row.

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

Reconciled against `specs/capability-endowment-spec.md` as it stands on
`origin/wt/toyos-endow` at `f53a8de`, re-verified row by row 2026-08-09. That
branch lands first; C0 merges the result and re-checks every row. **A row that has
moved is a red for this spec, not a detail for the implementer to absorb.**

**`origin/wt/toyos-endow` is `origin/main` plus one commit that adds only the
spec** — `git diff --stat origin/main...origin/wt/toyos-endow -- toyos-abi/
kernel/ src/ toyos/` is empty. Zero lines of their nine chunks are written, so
every row below is a reading of their *plan*, and the C0 re-check is a re-reading
of their *code*. Rows 18–22 were added by this review; 2, 3, 4, 9, 10, 12, 16 and
17 were corrected.

| # | contact point (their §) | how this spec reconciles |
|---|---|---|
| 1 | **`HandleTable` inside `ProcessData`, "behind the existing lock"** (§1.1) | That lock becomes a `SleepLock` in C8. Their `get::<T>` is lock → clone an owned Arc → unlock, so no table borrow crosses a park; §6's borrow rule *proves* it rather than asserting it. No new lock and no new ordering edge. |
| 2 | **`Rights::WAIT` — "block on it / io_uring POLL_ADD / SYS_PROCESS_WAIT"** (§1.4) | The seam is taken exactly as offered. **But the check cannot live in `completion::arm`**, as the first draft said: §5.3 makes `arm` take a *borrowed `Subject`*, by which point there is no handle and no `Rights` left to test. **`WAIT` is checked at the syscall boundary that resolves the handle**, and `arm` is reachable only from code that has done so. |
| 3 | **`Acceptor`/`Connector` and `PortShared`** (§1.2), which carry `acceptors: Arc<KWaitQueue>` and `io_uring_watchers: Lock<Vec<RingId>>` | `kernel/src/object/port.rs` is theirs to create; **C13** replaces those two fields with one watch list, exactly as it does for pipes and devices (the first draft said C3, which is the park site and does not touch `port.rs`). P6 parks there. Their `Acceptor::on_zero_handles` — set `closed`, drop the queue — becomes the canceller that posts `Outcome::Gone` to every parked acceptor, which is what makes their "the bound on failure is a process lifetime and nothing else" true of a *blocked* server too. **"The one file both branches write" is false** and is withdrawn: their chunk 2 also rewrites `io_uring.rs` (which §19 deletes from) and deletes `FdTable` from `fd.rs` (which §9 and §18 both change). Three files, not one. |
| 4 | **No key at all, rather than a `Koid` key** (§1.1: `Koid` is "an identity for diagnostics and kernel-internal keys, never an authority"; their chunk 2 turns `io_uring::Source` keys into `Koid`s, and §4.1 turns `Source::Listener(ListenerId)` into `Source::Acceptor(Koid)`) | §5.3's `Subject` is a *borrowed reference to the object*, so after C3 there is no key to turn into anything and a destroyed subject is unnameable. Their chunk 2 does the `Koid` rename; C3 removes the residual. Order matters: **C3 must land after their chunk 2**, or it rewrites a `Source` that is about to be rewritten. **Internal contradiction to resolve before C3 is written**: §21 gives C3 the deliverable `Source::Terminated(koid)` while §19 has C3 delete `Source` outright and this row says nothing is left to key. One of the three is wrong; the intent is that nothing survives, so `Source::Terminated(koid)` should go. |
| 5 | **`SYS_LISTEN`/`SYS_CONNECT` retired, `SYS_ACCEPT`(86) takes an `Acceptor` and returns one handle** (§3.2, §3.3) | `kernel/src/listener.rs` is gone before C3 runs, so P6's "listener's completion" is the `Acceptor`'s. `listener::io_uring_watchers` and `wake_poll_waiters` are deleted by them, not by this spec — **removed from §19 so neither branch claims it twice.** |
| 6 | **`SYS_THREAD_JOIN`(41) is *kept* with its `Tid`** (§3.4, their deviation D5) — `capability-handles-spec.md`'s `SYS_THREAD_JOIN_H` does **not** happen | P8 therefore parks on the `ThreadObject` the `Tid` resolves to inside the caller's own process, not on a handle. `SYS_PROCESS_WAIT`(108) with `Rights::WAIT` is the handle-shaped one, and P7 uses it. `park_lot`, `PARK_BUCKETS` and `wake_task(TaskId)` are still deleted here. |
| 7 | **`SYS_OPEN_DEVICE`(31) retired; `SYS_DEVICE_CLAIM`(111) mints a claim, and only `/bin/init` holds `Rights::DEVICE`** (§1.2, §3.1) | P3/P4 park on the `DeviceClaim`. `DeviceClaim::on_zero_handles` releasing the class posts `Outcome::Gone(Revoked)` to anyone parked, so their §5.3 crash-release row gains liveness for a blocked reader for free. |
| 8 | **`SYS_IO_URING_SETUP`(89) returns `{ handle, vaddr }`; the ring owns its `PageAlloc` and the kernel maps it at setup** (§3.3, their chunk 6) | Exactly §5.2's second inbox. **They close `io-uring-abuses-shared-memory`, not this spec** — removed from §19. C11 adopts the ring as an `Inbox` and adds nothing to its allocation. The superseded spec's 32 KiB `RingArena` is dropped by both. |
| 9 | **`on_zero_handles` runs from a deferred per-CPU queue drained "at syscall exit, `do_schedule` entry and the idle loop"** (their §1.1; the first draft cited §5.2, which is *Backpressure*) | Three things. (a) C9 empties the idle loop, and that is safe because the idle loop `pass`es every iteration, so the `do_schedule` drain site subsumes the idle one — **delete the third site rather than keep an idle-loop body for it.** (b) C12 adds `FileObject::on_zero_handles → writeback::push`, because `Drop` cannot take a `&Parkable` (§13). Their spec has no hook *table* to add a row to — their §5.3 is a six-row teardown table with no `FileObject` row — so this is an extension, not an entry. C12 lands after their chunk 2. (c) **The general rule, which is new and binds their chunks 1 and 2**: none of the three drain sites has a `Parkable` (`do_schedule` entry provably does not, §6.1), so after C5 **no `on_zero_handles` hook may take a `SleepLock` at all** — the compiler refuses it. `FileObject → writeback::push` is the shape *every* hook needing the VFS must take, not a one-off. |
| 10 | **Their §1.1's closing rule: "The failing shape to check any new type against is `toyos-sched`'s `Registration`: a guard that lives on the victim's own stack and is therefore never dropped when another CPU kills it. No object introduced below places a release obligation on a blocked thread's stack."** | §7 **fixes `Registration` itself** — but by §7.2's rewrite of `handle_retire`'s two reap-in-place arms, not by `Commit::Killed`, which the first draft named and which is the wrong path (`commit()` already dequeues). The victim runs again on its own stack and drops the guard. So their rule stops being a constraint they must design around and becomes a property the kernel has. **They should not relax it until C4 has actually landed and its `toyos-sched` tests are green**: it costs them nothing to keep, and until then it is still true. `retired-thread-leaks-wait-queue-node` is closed by C4, and it is this spec's to close. |
| 11 | **Their §1.1: an `Arc` cloned before blocking "is stranded on a freed kernel stack … leaks memory, bounded and census-visible"** | Same mechanism as row 10 retires the leak class outright. `capability-handles-spec.md` §13 said the structural fix was "Phase 2 try-once syscalls"; it is not — it is the cancellable park, which keeps one-syscall blocking I/O (§14.3). **Their census baseline assertions should tighten once C4 lands.** |
| 12 | **`KernelPayload.address_space: Option<PageTables>` → non-`Option`** (`capability-handles-spec.md` §9.4's one surviving retype) | §10: a kernel thread's `ProcessObject` names the **kernel** address space. A kernel thread naming *no* address space would have forced that field to stay an `Option` forever, so the retype is *enabled* by this one. **The endowment spec does not claim it**: `KernelPayload` appears nowhere in it, and its §1.3 lists `AddressSpaceObject` only as a `KObjectRef` variant adopted "with no change of shape" — a different type from `payload.rs:88`'s field. So this row is a contact point with `capability-handles-spec.md`, not with the endowment branch, and nobody currently owns doing it. **C6 does it**, since C6 is what makes it possible. |
| 13 | **Bad-handle policy flips to kill-the-process** (their chunk 7) | §7 makes that kill safe from a thread parked anywhere, including inside a sleep-locked critical section. Their chunk 7 flips it before C4 lands, so between the two landings a killed handle-abuser can still be killed at a park under the *old* locks — which is today's behaviour and no worse. |
| 14 | **Their gates `kill_while_blocked` and `device_claim_crash_release`** (their chunk 6) | Both are strengthened rather than changed: after C4 the killed client's stack is unwound by returning, so the census returns to baseline for a reason stronger than the handle drain. Do not weaken either to accommodate this spec. |
| 15 | **The SDK** (§6.5) | Disjoint in *intent*: they change what an argument names, this changes what a timeout means (§14.3 keeps the blocking ABI shape). `toyos/src/services.rs` and `toyos/src/pipe.rs` are deleted by them; `Poller` is replaced by `toyos::ring::Ring` in C11 either way. **Not disjoint in *files*** — see row 18. |
| 16 | **The `Abi-Inseparable` trailer and the shared sysroot** (their §9, §10.1) | They hold the sysroot claim for their branch's whole life. This branch then claims it in turn — **at C0, not C1** (§21's C0 row is right; the first draft's "C1 onward" here was not), because C0 is where the merge and the re-check happen and the claim must precede the first build. One trailer on one commit is sufficient: `src/pr.rs:319`'s `abi_lands_alone` exempts the whole branch if **any** commit declares it, so §21's single-commit form satisfies the gate. **Their §10.1's own recommendation is to land chunks 0+1 as a separate earlier PR** — see row 19, because "post-endowment" then means two different trees. |
| 17 | **Root `CLAUDE.md` headroom** (their chunk 9 measures 2,678 bytes spare against 37,322) | Verified 2026-08-09: `src/docs.rs:213` budgets 40,000; `origin/main` is **37,322**, `origin/wt/toyos-endow` is **byte-identical to main** (they have spent nothing yet — their line is planned for chunk 9), and this branch is **37,594**, so this spec's edit spent **272**. Both land comfortably. **Two arbiters, not one**: `src/docs.rs:226`'s `TOTAL_BUDGET` of 80,000 across the five files is separately enforced and currently sits at 74,469. Whoever lands second re-measures both. |
| 18 | **`rust/`, the std fork and the fork estate** (their §6.6, §10.2) — **the row the first draft missed entirely** | Three collisions. **(a) Their chunk 0 is a hard prerequisite for C1, not merely for the merge.** Their §6.6 records that `rust/library/std/Cargo.toml` names `toyos-abi` by a relative path resolving to the **primary** checkout, so a worktree's `x build library` compiles std against *main's* ABI — "for a branch whose whole content is an ABI change it is not an inconvenience but a wall". §14.1 makes this exactly such a branch. **(b) `SYS_SLEEP_UNTIL` and the absolute futex reach the shared tree**: `rust/library/std/src/sys/thread/toyos.rs:87` calls `nanosleep`, `sys/pal/toyos/futex.rs:15` passes a *relative* timeout. Their chunks 2, 5 and 7 hold std edits in the same one shared working tree, and their §10.2 records one uncommitted std patch failing three other worktrees' landings in an hour. **C11's std edits must be sequenced against theirs, not merely merged.** **(c) The 24-byte CQE reaches mio**: `mio/src/sys/toyos/selector.rs` indexes the CQ by `size_of::<IoUringCqe>()` (a recompile absorbs it) but also calls `map_shared`, the line their chunk 6 deletes. Two mio landings and two submodule bumps, sequenced. |
| 19 | **Their §10.1 option 2 — "land chunk 0 + chunk 1 as their own earlier pull request", which their spec calls "the recommendation"** | If they take it, **"post-endowment" names two different trees** and C0's syscall assertion breaks: after that first PR the block 99–112 has not landed and the first clean number is **99**, not 113. §14.2's fix (assert the *computed* first clean number, never a literal) covers it; the ordering does not. **C0 should actively request their option 2**, because their chunk 0 is row 18(a)'s prerequisite and this branch needs it earliest. |
| 20 | **`EXPECTED_FAILURES` expires 2026-09-06** (their §8.5, §10.3) | Both entries carry `Stale::OnThisDate("2026-09-06")` (`tests/toyos.rs:730`, `:743`) and the harness **exits 1 by itself** when it arrives. This is a fifteen-chunk branch whose C0 cannot begin until a nine-chunk branch lands, and today is 2026-08-09: **this branch is near-certain to be open past that date and will red on two exemptions it did not cause.** Not this branch's to move — moving it is precisely what the rule forbids. **C0 puts it to the owner** with the two entries' merits, as a question about the entries and not a scheduling favour. |
| 21 | **They rewrite all eleven `system.toml` files** (their chunk 4, §2.3) | Six of them put `/bin/terminal` in `init` beside the compositor (their §8.5) — and **`blocked_dump` and `screen_blocked_dump` are two of the six**. §17 requires both green throughout and C6's gate is "`blocked_dump` names them". So this spec's kernel-thread naming is verified through a boot config the other branch rewrites, including its ready-marker semantics. C6 re-reads their §8.5 before writing the assertion. Their §8.1 also adds four `src/build.rs` `#[test]`s to the same `cargo test --lib` harness C13 adds grep gates to. |
| 22 | **They delete the userland retry constants** (their §6.2) | `NetdConn::BOOT_RETRIES`/`BOOT_RETRY_INTERVAL_NS` and `AudioStream`'s pair go, and with them **100 `SYS_NANOSLEEP` calls per metal-sim boot**. A favourable input this spec should not double-count: §3's "no timers" rule and §14.2's retirement of 49 both get easier, and the userland side of `SYS_NANOSLEEP` is much thinner by C11. Their §8.5 also moves soundd's RT entry from `set_rt_priority` to `SYS_RT_ENTER(112)` — inside the daemon whose wake latency is §20.2's assertion. |

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

**That mechanism is narrower than three of the four models need, and it constrains
the file layout.** `kernel-loom/src/lib.rs:63` reaches `kernel/src/sync.rs` and
`kernel/src/shootdown.rs` by `#[path]`, and it works only because those two files
name almost nothing outside themselves — four shimmed items in total
(`cell::UnsafeCell`, `preempt::{disable,enable}`, `arch::tlb::poll`, `log!`), and
`shootdown.rs` names none. A `completion/mod.rs` holding `Record`, `Outcome`,
`Inbox`, `Subject`, `arm` and `post` names `SyscallError`, `Instant`, `TaskId` and
— per §5.3 — borrowed references to pipe ends, listeners, device claims, process
objects and outstanding operations. **So `Inbox` and `SleepLock` must each live in
a file with a `sync.rs`-sized dependency surface**, separate from anything that
names a subject, or they cannot be modelled at all. That is a layout requirement
on C2 and C5, not a note.

Two of the four models need re-siting before they are written:

- **`sleep_lock.rs` risks reproducing the defect it cites.** Loom has no
  scheduler, so the *park* must be shimmed — and the park is the part under test.
  It genuinely does remove the unbounded branch `lock-spin-unreachable-by-loom`
  records, so the acquire/release ordering and the FIFO property are real
  coverage; the wake handshake is not, and the model must say which half it
  proves rather than implying both.
- **`cancel.rs` is a `toyos-sched` model, not a `kernel-loom` one.** `Commit`, the
  rendezvous CAS and — after §7.2 — `handle_retire`'s arms all live in
  `toyos-sched/`, which has its own `loom/` directory. Put it there.

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
2. **The RMW budget must be counted, not asserted.** The first draft said "one
   CAS replacing a `Lock` acquire's one `fetch_add`, one for one across all 244
   `.lock()` sites", and that is wrong twice: only **5** statics convert, so the
   other sites keep their `fetch_add` unchanged; and an uncontended `SleepLock`
   acquire is a CAS **plus** the holder store that §8's `holder()` needs and
   `Lock` does not have today (`sync.rs` records no holder). So the budget grows
   by one store on the converted paths and by however many RMWs the contended
   enqueue costs. **C5 counts it and C14 measures it**; it is plausibly free
   against a 2.902 ms period and it is not free by construction.
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

1. **The dump physically cannot block**, and that part is true and valuable:
   `sched/dump.rs` and `panic_console` run from `drain_irqs` and from
   `halt_all_cpus`, neither of which has a `Parkable`, so `SleepLock::lock` is not
   callable there (§6.1).

   **But the first draft's second half — that the dump gains information — is
   false, and it should not be sold to the owner.** `sched/dump.rs` contains zero
   textual `.lock()` or `.try_lock()`; it reaches exactly three locks indirectly,
   and **none of them is one of §9's five**:

   | reach | lock | today |
   |---|---|---|
   | `dump.rs:532` → `process.rs:669` | `process::PROCESS_TABLE` | `Lock<Option<ProcessTable>>`, already `try_lock` |
   | `log!` → `log_ring.rs:314` | `RingGuard` | hand-rolled CLI spinlock, unbounded |
   | `dump.rs:230` → `panic_console.rs:662` | `SEQ` seqlock + `PAINTING` | deliberately not a `Lock` |

   `process::ProcessData` (`process.rs:346`) is a **different** lock from
   `process::PROCESS_TABLE` (`process.rs:640`), and thread names come from the
   latter. So after all five conversions the one lock that can refuse the dump is
   still a raw ticket `Lock` with no holder, and the dump already asks it with
   `try_lock`. **`SleepLock::holder()` buys the dump nothing.**

   It is still worth having — **RT6** (self-deadlock) needs it — but it is new
   state, a new write on every acquire and release, it is not in §16.2's RMW
   budget, and it appears in no chunk. **C5 owns it**, and §20 should not claim a
   diagnostics improvement this refactor does not deliver.
2. **A parked task now says what it is parked on.** `driver::ParkedInfo` gains the
   armed `Subject`'s kind. Today it carries `WaitClass`, deadline and duration; a
   thread parked on a disk transfer and one parked on a pipe are the same row.
3. **`panic_console::hold_report` must NOT move to `usbd`, and the first draft's
   version of this item is withdrawn.** It proposed moving the 20 ms re-read of
   128 remembered pixels off `drain_irqs` onto `usbd`'s housekeeping step. But
   §12.3 says a present-but-hung device parks a thread forever and §22's own row
   is "`usbd` wedges on a broken controller | `usbd` alone parks … the dump names
   it" — and `hold_report` **is** how the dump reaches the panel. On a machine
   with a wedged USB controller, which is the machine the T14 write-ups are about
   and which has no serial port, the report would paint once, the compositor would
   overwrite it inside a frame (the documented reason the hold exists,
   `panic_console/mod.rs:658`), and nothing would put it back.

   It also converts a check every CPU makes every pass into a single point of
   failure. **It stays in `drain_irqs`**, where it is a `CAS` on `HOLD_CHECK_AT`
   and one clock read per pass. Its 20 ms gains an explicit `Cadence` type and
   nothing else. The halted-machine pager (`page_forever`, reachable only from
   `halt_all_cpus`) is untouched — it must be, because no thread runs there.

The panic path keeps every spin it has (§4.5). `apic.rs:203`'s 500 ms wait for the
log file to drain before power-off **is not a `Tripwire`** — `apic.rs:194` logs
"the panel is the only copy" and *returns*, deliberately, because a second panic
on a machine already going down loses the report (§3.2). It is a `Bound` whose
expiry is a named refusal. It cannot become a completion either way, because the
thread that would post it is not going to run again.

Gates that must stay green throughout: `blocked_dump`, `screen_blocked_dump`,
`dump_nmi_probe`, `screen_panic_muted`, `disk_backtrace`, `fault_gates`,
`fpu_isolation`.

---

## 18. Migration ledger

Counted with the commands in §4 and re-derived 2026-08-09 at `e6f7769`; two rows
were wrong and are corrected below.

**Every count here, and every line number in §4.1, is measured on pre-endowment
`main`, and C0 re-derives all of them — not only `io-depth-probe` and the
`--slow-usb` A/B.** §4.1 calls itself "the refactor's inventory" without that
hedge, and two of its twelve rows are already stale against the endowment plan:
**P6** (`arch/syscall.rs:1279`) sits inside the `1246-1344` span their §7.1
deletes, and **P7** (`:1202,1213`) is inside `sys_waitpid`, whose number 26 they
retire. **`with_fd_owner_data` is a name their chunk 2 may retire outright**, since
it deletes `FdTable` and makes `fd.rs` dispatch on `KObjectRef` — so C8's "55 call
sites" is a count of something that may not exist under that name.

| what | count | disposition |
|---|---|---|
| `core::hint::spin_loop();` in `kernel/src/` | 39 | 4 deleted (§4.2), 14 become `Poll` (§4.3), 21 stay and are gated to a **site** allow-list: 13 Class R, 6 Class X, 1 Class L, 1 Class B (§4.4–§4.6) |
| …of those, under `kernel/src/drivers/` | 23 | |
| `scheduler::wait_until` callers | 6 | all → `completion::wait` |
| `scheduler::prepare_wait` call sites | 7 | 3 internal to `scheduler.rs`; all → `completion::arm` |
| `scheduler::block_on` call sites | 7 | all → `completion::wait` |
| `io_uring::complete_pending_for_event` call sites | 10 | all → one `post` on a watch list |
| `.lock()` calls in `kernel/src/` | 244 | 5 statics convert (§9); the sites under them take `&Parkable` |
| `.try_lock()` calls | 11 | unchanged in meaning; two more appear (`poll_if_pending`, boot's VFS) |
| `static … Lock<…>` declarations | **52** | `grep -rnE "static [A-Z_0-9]+: *Lock<"` gives 49; add `VOLUMES` (an *array* of two `Lock`s, `fat32_adapter.rs:316`) and the two written `crate::sync::Lock<` (`percpu.rs:294`, `syscall.rs:25`). The first draft said 48. **The number moves with the regex** — three readings of this tree gave 48, 49 and 51 — so the ledger states the command, and only the 5 that convert matter |
| `vfs::lock()` / `vfs::try_lock()` textual sites | **30** | 29 + 1; the first draft said 33. Split boot from task; 2 doors keep the choke point |
| `with_fd_owner_data` sites | 55 | take `&Parkable` where they can reach a flush |
| kernel `.rs` files | 117 | |

**The 30 VFS sites and the 55 `ProcessData` sites are the blast radius, and it is
*not* purely mechanical.** The choke point is real and small — `vfs::lock()` and
`vfs::try_lock()` are the only two doors — but every caller becomes a caller that
may park, and threading `&Parkable` down to the device stops at a crate boundary:
**`BlockAccess` lives in `toyos-fat32/src/device.rs`**, a pure host-tested crate
with no kernel dependency, and `BlockDevice` in `kernel/src/block.rs`. Both take
`&mut self` and no context argument. Putting a `&Parkable` in `BlockAccess` makes
`toyos-fat32` depend on a kernel type, which is not acceptable. **C8 owns finding
the shape that avoids it** — the likely answer is that the token stops at the
kernel-side adapter and the sleep-lock acquire happens above the trait, never
inside it.

Userland is untouched until C11, because §14.3 preserves the blocking ABI shape.

---

## 19. Deletion ledger

**Code deleted, by name.** `sched/driver.rs`: `flush_log_file_if_affordable`,
`LOG_DEFERRAL_CEILING_NS`, `LOG_DEFERRED_SINCE`, `log_file_flush_due`, `owes_wake`,
`drain_serial` on the idle path, four pre-`hlt` conditions, `poll_if_pending` from
`drain_irqs`, and `log_health`/`reap_poisoned` from the idle loop (§11).
`scheduler.rs`: `wait_until`, `prepare_wait`, `block_on`,
`wake_task`, `wake_pipe_readers`, `wake_pipe_writers`, `park_lot`, `futex_wake`'s
generation protocol. `sched/waitqs.rs`: `PARK_BUCKETS`, `park_lot`.
`io_uring.rs`: `Source`, `Source::is_ready`, `complete_pending_for_event`,
`complete_pending_for_source`. `log_file.rs`: `SINK`. `xhci/wait/mod.rs`:
`wait_transfer`, `wait_command`. `nvme.rs`: `wait_completion`'s spin.
`virtio.rs`: `submit_and_wait`'s spin. Five per-source `IO_URING_WATCHERS`
statics (`net.rs`, `keyboard.rs`, `mouse.rs`, `hda.rs`, `virtio_sound.rs`) and
the sixth inside their `PortShared` (§15 row 3).

**Not deleted here, because the endowment branch deletes them first**:
`kernel/src/listener.rs` whole (verified: their §7.1 table and their chunk 3),
`wake_poll_waiters` (`arch/syscall.rs:1334`, inside the `1246-1344` span they
delete), and `io_uring`'s `shared_memory` dependency (`io_uring.rs:198`). **One
item on this list is not backed by their text**: `PendingPoll`'s fd keying. Their
chunk 2 says only that `io_uring::Source` keys become `Koid`s, and `PendingPoll`
(`io_uring.rs:185`) carries a **separate** `fd_num: u32` field their spec never
names. It plausibly becomes a `RawHandle` when `FdTable` dies, but nobody has
written that down. **C0 asks them, or C3 owns it.**

**`specs/issues/` files closed.** Slugs only, deliberately: `src/docs.rs`'s
`every_named_issue_file_resolves` walks every text file in the tree and reds on
any `specs/issues/<area>/<slug>.md` path that does not resolve, so a full path
here would red `cargo test --lib` the moment the file is deleted.

**C13 must therefore also de-path the citations elsewhere, and there are more of
them than this section.** Six of the fifteen slugs below are written as *full
paths* in this very document — §1 (`disk-wait-pins-a-cpu`), §1.3
(`client-cpu-takes-the-log-flush`), §4.3 (`driver-waits-without-a-deadline`), §5.6
(`io-uring-source-half-a-wake-pair`), §7.5 (`retired-thread-leaks-wait-queue-node`)
and §13 (`cache-eviction-wedges-an-idle-cpu`) — and eleven of the fifteen are cited
by full path from outside `specs/issues/`, the root `CLAUDE.md` and
`specs/introspection-plan.md` among them. **Every one is a `cargo test --lib` red
at C13 and none is in any chunk's budget.** The `specs/issues/README.md` protocol
says the durable rule moves into the spec that owns the subject; doing that is
what removes the citation, so it is the same edit.

| slug | area | closed by | note |
|---|---|---|---|
| `disk-wait-pins-a-cpu` | audio | C7+C8 | the headline |
| `client-cpu-takes-the-log-flush` | audio | C9 | there is no heuristic left to steer |
| `log-flush-is-unbounded` | boot-media | C9 | |
| `cache-eviction-wedges-an-idle-cpu` | boot-media | C13 | the idle CPU no longer reaches a block device; **verify the `rip` first** — that entry says symbolization was never done |
| `xhci-waits-are-spins` | hardware | C7 | EP0 recovery's `Poll` is the declared residual (§12.3) |
| `scheduler-pass-blocks-in-xhci` | kernel | C7 | and its second half, `sched-check` never being turned on, is C15's |
| `hotplug-blocks-a-scheduler-pass` | hardware | C7 | |
| `driver-waits-without-a-deadline` | kernel | C10 | `CAP.TO` included |
| `io-uring-source-half-a-wake-pair` | kernel | C3 | one post, no pair to halve |
| `panic-on-wedged-virtio-console-spins` | panic-path | C10 | `submit_and_wait` gets a `Bound` |
| `retired-thread-leaks-wait-queue-node` | kernel | C3+C4 | §7.5's consequence 1 — and by §7.2's retire arms, not by `Commit::Killed` |
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

**Two worktrees, and the arms alternate.** The first draft's protocol used
`git stash` to switch arms and then ran all of A before all of B. Both halves were
wrong: `stash` moves only *uncommitted* work, and by C14 this branch is fifteen
chunks of **commits**, so both arms would be the branch; and running A, A′, B, B′
in that order is exactly the uncontrolled shape
`one-rmw-per-log-line-cost-350ms`'s second lesson records, cited two lines below
it, where the host settled between the arms.

```
# arm A = a second worktree at origin/main; arm B = this branch. Interleaved.
# N=6 rounds of 5 gives the same 30 iterations per arm with the host drift shared out.
for round in 1 2 3 4 5 6; do
  (cd $MAIN_WT   && cargo test --test toyos-build -- --audio-gate 5)             # A
  (cd $BRANCH_WT && cargo test --test toyos-build -- --audio-gate 5)             # B
  (cd $MAIN_WT   && cargo test --test toyos-build -- --audio-gate 5 --slow-usb)  # A'
  (cd $BRANCH_WT && cargo test --test toyos-build -- --audio-gate 5 --slow-usb)  # B'
done
```

**Budget it before starting: `tests/audio-baseline.toml` prices one
`--audio-gate 30` at ~17 minutes and that single invocation already runs all four
gate-A configs, so four arms is ~68 minutes of guest time** plus a rebuild per
toggle. The first draft stated no number at all. Two worktrees also means two
guest-slot consumers against `buildlock::guest_slot`'s twelve, which is what makes
the interleave affordable rather than serial.

Every run records its host (`host: load <1/5/15min> qemu N toyos-build N`) and
adjudicates nothing on it — the owner's 2026-08-04 ruling stands: a load-coincident
audio failure is investigated as a real defect, never re-run away.

### 20.2 The number this refactor must produce

From §1.2, `--slow-usb`, `audio_tone` smp=1: **worst wake back inside one
pipeline.** The measured before is 165,948 µs and the ordinary-stick control is
7,117 µs. The gate is an assertion added in C14, not a number written here — it is
whatever the same-session A/B measures on the tree that lands, and the plan's job
is to say which measurement becomes the assertion. Add it to
`tests/audio-baseline.toml` with the run that produced it.

`io-depth-probe` must report **1 from a syscall and 0 from `logd`** (§9.1), against
5 and 4 today.

**And a positive assertion on the log's content, in the same run.** `/log` is a
USB volume in every profile, so the *cheapest* way to make §20.2's wake number
good is for the file sink to stop writing — which is exactly what §12.3's unbounded
park does. **The headline number and the worst failure mode produce the same
reading**, and none of §20.3's four negative controls separates them. So C14 also
asserts, host-side on the volume, that this boot's log file holds the lines it
should. `tests/common/volumes.rs` already reads guest volumes host-side. Without
it C9's headline is unfalsifiable.

### 20.3 Negative controls — each must red on a tree that has the defect

| feature | what it reintroduces | what must go red |
|---|---|---|
| `reintroduce-idle-flush` | `log_file::poll()` back on the idle loop | the `--slow-usb` A/B, by the §1.2 margin |
| `sleeplock-spins` | `SleepLock::lock` spins instead of parking | `io-depth-probe`'s depth, and the `--slow-usb` A/B |
| `park-holding-a-spinlock` | one converted path keeps its raw `Lock` | a named panic — **but say which**: `Parkable::of_current()` at the leaf trips RT1 and names the token; a token threaded from the trap entry reaches `scheduler.rs:43`'s `assert_baseline`. Pin the one the control stages |
| `drop-a-completion` | one `post` writes the record and does not claim | **not a hang** — see below |

Each carries a comment saying why nothing else can reach it, per the harness's own
rule. **A feature that replaces only a verdict makes its own gate vacuous** —
`reintroduce-idle-flush` replaces the *behaviour*, which is why it is the strongest
of the four. None can join `INERT_ACTUATORS`; each is its own kernel build and four
more images in the ledger.

**`drop-a-completion` cannot have a hang as its verdict.** A hung guest does red,
but it reds as `STALL`, and the harness prints "the guard expired, so this says
nothing about the tree" beside it and tells nobody to bisect it — root
`CLAUDE.md`'s rule is that a timeout is a liveness guard and never a verdict. A
control whose entire signal is the one class the suite disclaims is not a control.
**`blocking_read_stress` therefore asserts a *count* of completed round trips
inside an `await_guest` bound**, so a dropped completion reds as a number, and the
control reds on the number.

**`sleeplock-spins` cannot red at C5, where §21 asks it to.** C5 lands `SleepLock`
with *nothing converted*, so no `SleepLock` is on the disk path, `io-depth-probe`
reads the same 5 and 4 in both arms and the `--slow-usb` A/B does not move. Either
the control's gate moves to **C7** (its first real consumer) or C5's gate is
"the feature exists and its own unit test shows the spin", which is weaker and
should be labelled as such.

### 20.4 New named tests

- `blocking_read_stress` — cross-CPU pipe ping-pong, hard wall-clock bound. The
  lost-wake canary.
- `cancel_while_parked` — kill a thread parked on a disk transfer under
  `usb-slow-device`; the process exits, the lock is free (`SleepLock::holder()` is
  `None`), and a second process reads the same file. **It cannot run at C4**, where
  §21 lists it: `SleepLock` does not exist until C5 and there is no park on a disk
  transfer until C7, because `wait_transfer` still spins. C4's gate is the *return
  path* only — kill a thread parked on a pipe and assert it exits through its own
  stack — and this test moves to **C7**.
- `killed_holder_releases` — kill a thread holding the VFS sleep lock; the machine
  keeps mounting.
- `no_spin_outside_the_allow_list` — the §4.6 grep gate, host-side, seconds.
- `idle_loop_is_the_declared_body` — renamed, because "one statement" is not what
  §11 leaves (`pass`, `reap_poisoned` and three `#[cfg]` probes) and because the
  first draft's name says `idle_loop` while its description says "the halt check",
  which is a different function (`execute`'s `Idle` arm). It is a **host-side
  source gate** like `no_spin_outside_the_allow_list`, not a guest test: it asserts
  that `idle_loop`'s body and the pre-`hlt` condition list are exactly the declared
  sets, because a condition quietly re-added is invisible to every behavioural
  test.
- **`sched-check` is its own chunk, not a line in C14.**
  `scheduler-pass-blocks-in-xhci` records that invariant P "has never executed
  against the kernel in any image or any test run", and that the measured window
  starts after `drain_irqs` — both confirmed (`cpu.rs:1092`, `MAX_PASS_NS` 200 µs
  at `:703`; `drain_irqs()` precedes `SchedPass::begin` at `driver.rs:316` and
  `:418`). But **moving the window to start at `drain_irqs` makes invariant P cover
  `i8042::service`, the dump's `serve_if_owed`, `hold_report`'s 128 probes and every
  `irq_ring` post** — none of which this refactor touches, all of which must then
  fit 200 µs on a TCG guest, and `specs/test-cost-audit.md` already classes
  `sched-check` as perturbing and used by no test. Turning it on will red on things
  this refactor did not cause. It needs its own baseline before it can be a gate.

---

## 21. Work breakdown

Thirteen chunks on `wt/toyos-compl` — the first draft had fifteen; C3+C4 and
C7+C8 are each one chunk because neither half can be green alone (§21.1, and
C3+C4's reason in the table), and `sched-check` is split out as C15. **Every chunk
builds, boots, and passes `cargo test`** — plus `cargo test` inside `toyos-sched/`,
`toyos-xhci/` and `kernel-loom/` where it touches them. No intermediate landing;
one PR at the end, subject to §21.2's fallback.

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

Two chunks below are merged pairs and keep both names, so a reference elsewhere to
"C4" or to "C7" still resolves to the half it means.

| # | chunk | delivers | gate |
|---|---|---|---|
| C0 | merge `origin/main` (post-endowment); re-check every §15 row against the *merged* tree; assert the first clean syscall number (§14.2); claim the sysroot | baseline `io-depth-probe` + `--slow-usb` A/B recorded in this spec | suite green; §15 has no moved row |
| C1 | the six duration kinds (§3, §3.1, §3.3); `Instant`/`Duration`; `Deadline::{at,never,passed}`; `Parkable` | **every one of the 41 production durations has a kind or a named exception** (§3.4) — not "the kinds exist" | no behaviour change; `MIN_ONE_SHOT_NS` still compiles |
| C2 | `kernel/src/completion/`: `Record`, `Outcome`, `Inbox`, `Subject`, `arm`, `post`. Wired **behind** the existing waitq — every wake also posts | behaviour-preserving | `kernel-loom/tests/inbox.rs` |
| **C3+C4 — one chunk, not two** | the one park site (`wait_until`/`prepare_wait`/`block_on` → `completion::wait`, futex folded in, `park_lot`/`PARK_BUCKETS`/`wake_task` deleted) **and** the cancellable kill (§7.2's two `handle_retire` arms, `Commit::Killed` → `dispose_none`, the one-shot cancel). **They cannot be split**: C3 puts an `Armed` on a parked thread's stack while `Commit::Killed` still discards it, so RT5 turns an ordinary kill of a blocked thread — the endowment branch's own `kill_while_blocked` gate — into a kernel panic | §7, 12 park sites → 1 | `toyos-sched` host tests for the new retire arms; `toyos-sched`'s loom model for cancel; `blocking_read_stress`; grep: one `dispose_block` caller |
| C5 | `SleepLock`, `holder()` (§17.1), the `Parkable` threading. Nothing converted | §8 | `kernel-loom/tests/sleep_lock.rs`; the RMW count of §16.2 rule 2 |
| C6 | kernel threads: identity, dump naming, `logd`/`usbd`/`iod` spawned and idle | §10 | `blocked_dump` names them |
| **C7+C8 — one chunk, not two** | xHCI async (`wait_transfer`/`wait_command`/`configure`, the per-disk claim, `XHCI` → `SleepLock`, `poll_if_pending` → `usbd` + `try_lock`) **and** `VFS`/`VOLUMES`/`ProcessData` → `SleepLock` with their 30 + 55 call sites and the boot/task split. **They cannot be split — see below** | §9, §12 | `toyos-xhci` host tests; `usb_storage_gate`; `killed_holder_releases`; `cancel_while_parked`; `sleeplock-spins` and `park-holding-a-spinlock` red; `io-depth-probe` falls |
| C9 | `kernel/src/log/`: core + three sinks + `logd`. Every deletion in §11, minus `reap_poisoned`, which cannot move (§11) | §11 | `idle_loop_is_the_declared_body`; `reintroduce-idle-flush` reds; `--slow-usb` A/B moves |
| C10 | `Poll<T>`; NVMe `CAP.TO`; virtio, HDA, IOMMU, RTC settles; the three duplicate `settles` become one | §4.3 | `no_spin_outside_the_allow_list` |
| C11 | blocking syscalls on the one shape; `SYS_SLEEP_UNTIL`; absolute deadlines; 24-byte CQE; the ring becomes an `Inbox` (its pages are already its own — §15 row 8); `toyos::ring::Ring` replaces `Poller`; soundd's `delta == 0` hack deleted | §14 | full suite; gate A fast tier |
| C12 | the write-back queue; `FileObject::on_zero_handles`; `SYS_FSYNC` parks; page-cache eviction to `iod`; **§13.1's page pinning and `close_file`** | §13 | `close-cannot-report-io-error`'s reproduction; `disk_backtrace` and `esp_files` still green (§13.2) |
| C13 | the deletion commit; grep gates; `specs/issues/` closures **and the ~17 full-path citations that go stale with them** (§19); CLAUDE.md | §19 | `cargo test --lib` green — `every_named_issue_file_resolves` is the real gate, not "it compiles", which is a tautology |
| C14 | measurement: the interleaved four-arm A/B (§20.1, ~68 min of guest time, two worktrees); `io-depth-probe`; the positive log-content assertion (§20.2); assertions recorded in `tests/audio-baseline.toml` | §20 | the numbers go in this spec |
| C15 | `sched-check`: move invariant P's window to the scheduler entry, take its own baseline, then turn it on in one harness profile | §20.4 | its own baseline first — it will red on work this refactor did not do |

### 21.1 Why C7 and C8 are one chunk

Trace the disk path, whose lock order the source itself states
(`fat32_adapter.rs:314`, *"Lock order is VFS → here → `XHCI`"*):

```
vfs::lock()                    vfs.rs:29        VFS      [ticket Lock, preempt +1]
  → Vfs::flush_file
  → FatVolume::write_at        fat32_adapter.rs:353
      device(role).lock()      fat32_adapter.rs:316  VOLUMES  [ticket Lock, preempt +2]
  → UsbBlockDevice::write_blocks   usb_storage.rs:95
  → xhci::with_disk            xhci/mod.rs:1871
      XHCI.lock()              xhci/mod.rs:1777              [preempt +3]
```

At C7 alone, `XHCI` is a `SleepLock` while `VFS` and `VOLUMES` are still ticket
locks, so `with_disk` must call `XHCI.lock(&p)` — and **both ways of getting the
token fail**. `Parkable::of_current()` at the leaf runs at baseline +2 and RT1
refuses it. A token threaded from the syscall entry reaches the park with two
ticket spinlocks held, and `scheduler.rs:43`'s `assert_baseline` refuses *that* —
which is §9.1's "the conversion is half done" trip, firing exactly as designed.
The only C7-only escape is `try_lock` with a spin fallback, which is §23
rejection 4.

**So C7 cannot be green on its own, and §21's header claim was false for it.**
There is a second reason to merge them: `BlockAccess` lives in
`toyos-fat32/src/device.rs`, a pure host-tested crate, so the token cannot be
threaded through the trait and C8's real work is finding the shape that avoids
needing to (§18).

### 21.2 Dependencies

C3+C4 needs C2. C5 is independent of C2–C4 and must land **before** C7+C8.
C7+C8 needs C3+C4, C5 and C6. C9 needs C6 and C7+C8 — that is the stage whose
number moves, and it cannot move earlier. C11 is independent of C7–C9 and may
float. C12 needs C6. C15 is independent and last.

**§24's fallback split, if the owner wants one:** C0–C6 as one pull request and
C7–C15 as a second. The graph permits it at C6 and nothing before C7 changes a
lock.

Across the two branches: C3 must follow the endowment branch's chunk 2 (§15 row
4) and C12 its chunk 2 as well (row 9). Since the whole of this branch follows
their landing, both are satisfied by C0 — recorded so nobody reorders C3 ahead
of the merge on the grounds that it "only touches the scheduler".

---

## 22. Failure modes and runtime fail-fast

| failure | behaviour | recovery |
|---|---|---|
| A post races a park | Invariant W: the parker's recheck observes the record | self-wake, retry — structural |
| A kill races a park | `Cancelled`; the task returns and unwinds by returning | dies at the syscall boundary |
| A killed task was **already parked** holding a sleep lock | `handle_retire` makes it runnable instead of reaping it (§7.2); it observes the kill at its next `wait` and unwinds | the lock is released by the guard's own `Drop` |
| A killed task was **ready** with a guard on its stack | not reaped from the run queue; it is picked and observes the kill (§7.2) | as above |
| A cancelled thread's teardown must take a sleep lock | the cancel is one-shot and already consumed, so teardown parks normally (§7.4) | ordinary acquire |
| The victim does not reach `Dead` inside the retirer's 1 s | `retire_task`'s `Tripwire` panics — and it now bounds an unwind, not a reap | C4 re-derives the number (§7.3) |
| A device never answers | the thread parks forever; the CPU is free | Ctrl+Alt+D names the task and the subject; disconnect or kill cancels it |
| The log sink parks on a dead stick | **`logd` parks and takes the serial sink with it** — one thread drains both (§10) — and no error is produced, so the self-disable at `log_file.rs:317` never runs. Only the panel sink survives | **unresolved until §12.3's choice is made**: a `Tripwire`/`Budget` restores the self-disable, or `logd` splits in two. Today's behaviour is strictly better and the plan must not regress it |
| The inbox fills | oldest-dropped with a count, and a `Gone(Overflowed)` record so the waiter re-derives | a bounded loss, never a lost wake |
| `usbd` wedges on a broken controller | `usbd` alone parks; `logd` and `iod` are unaffected | the dump names it |
| A CPU takes an event for a transfer nobody is parked on | `Outstanding` matches by TRB address; an unmatched event is dispatched as today | unchanged |
| Boot's VFS is contended | `try_lock().expect(..)` panics by name | a kernel bug, fail fast |

Runtime fail-fast, numbered so a review can cite them:

- **RT1** `Parkable::of_current()` asserts the context's baseline preempt depth,
  and `completion::wait` re-asserts it at the park — the arm that catches a `Lock`
  taken half-way down a call chain, which no type can see (§6.3).
- **RT2** One `dispose_block` caller; a second is a grep-gate red.
- **RT3** `Armed` is `#[must_use]` and non-`Copy`; `Drop` disarms. Park-with-nothing-armed is untypeable.
- **RT4** A **second** cancel reported to one thread panics at the call site that
  asked for it (§7.4). Not "re-arming after a cancel": teardown must park after a
  cancel, and the first draft's form panicked on this spec's own death path.
- **RT5** A watch node found on a list whose owner is `Dead` panics — the corpse class §7 closes.
- **RT6** `SleepLock::holder()` naming the *current* task on `lock()` panics (self-deadlock), instead of hanging.
- **RT7** `Bound`/`Cadence`/`Tripwire` have no `from_nanos`; a duration with no justification does not compile.
- **RT8** `min_complete > cq_size` at `enter` → `InvalidArgument`.

---

## 23. Explicitly rejected

1. **A global completion registry with a `CORE` lock** (the superseded spec's §5,
   §13.2). It needs sharding at 128 cores, it re-keys every subject by an id in
   exactly the namespace the endowment architecture deletes, and a borrowed
   `Subject` costs nothing and cannot name a freed object. **"The objects already
   own watcher lists" is only half true and is withdrawn as a reason**: `Pipe` owns
   `readers_wq`/`writers_wq` per object (`pipe.rs:153`), but the io_uring watchers
   are five *global statics* that §19 itself deletes, plus `listener.rs`'s
   registry. True of the mechanism this spec keeps, false of the one it unifies.
   The other three reasons carry it.
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
8. **Interrupt-driven serial TX.** It buys throughput, not correctness, and fails
   the >2× rule. Revisit if `logd` is ever measured to be CPU-bound. **The first
   draft's reason was wrong and is withdrawn**: "the THRE spin runs on a
   preemptible thread" is false, because `BackendGuard::lock` takes
   `save_and_cli()` (`serial.rs:96`) and holds it for the whole drain, so `logd`
   is not preemptible there and the CPU is deaf to every IPI (§4.5a). The
   rejection stands on cost alone, and the `cli` window is a residual this
   document does not close — named here rather than left to be discovered.
9. **A single housekeeping thread instead of three.** A stuck USB enumeration
   would stop the log, which is the property this refactor exists to remove.
10. **Posting CQEs directly from ISR context.** Needs a lock-free registry to find
    inboxes, and post-time timestamps already preserve the fidelity (§14.3). The
    first draft priced it as "single-digit µs against a 2.902 ms period", which is
    **an unsourced number against the wrong denominator** — the quantity that
    matters to an audio client is wake latency, not period length. The conclusion
    survives the correction and is stronger for it: §1.2's healthy wake latencies
    are 6,108–10,632 µs, so ISR→`drain_irqs` latency is negligible against *that*
    too. **Measure it or say it is an estimate**; the spec's own prologue forbids
    the third option.
11. **A userland `logd`.** It cannot log the boot that precedes it, nor its own
    death.
12. **Multishot polls.** One-shot plus re-arm is what soundd does and what the
    kernel loop needs; multishot adds CQ-overflow back-pressure policy. Revisit
    with a measured re-arm cost.

---

## 24. Open risks

1. **§15 is reconciled against an unlanded branch.** `origin/wt/toyos-endow` at
   `f53a8de` is a plan, not a merge, and its own §9 says its syscall block shifts
   if `main` moves under it. C0 re-checks every row against the merged tree; a
   moved row is a red for this spec.
2. **C8's blast radius.** 33 VFS sites and 55 `ProcessData` sites, in the code path
   that boots the owner's machine. The choke point is two doors and the change is
   mechanical, but a missed site is a `Parkable` that will not thread and the
   compiler finds it — which is the argument for the token over a review rule.
3. **The `--slow-usb` A/B is one constant against a bimodal reality.** A real
   stick's write latency is microseconds when the erase block is open and tens of
   milliseconds when it is not, so the *rate* of harm on the T14 is not something
   this stages. The line to read on the owner's next boot is soundd's
   `max_wake_lat_us` clustered near 2,902 with `drains=0` and `max_batch=1`.
4. **A thread parked forever on a hung device is new behaviour, and as first
   drafted it costs the machine its whole log.** §12.3's three cancellers are
   circular for the case that matters — Reset Recovery's only trigger is the
   bound being deleted — so the real count is zero, and the parked thread is
   `logd`, which owns the serial sink too. **This is the plan's largest open
   decision and C7 makes it**: a `Tripwire` on the transfer, or a `Budget` at the
   log/filesystem layer, or `logd` split in two. "No bound anywhere" is not
   available.
5. **Gate A's thorough tier being red on `main`** means every verdict in C14 is a
   delta. If it goes green before C14, take the pass/fail — but do not wait for it.
6. **`/log` is a USB volume in every profile, so C9's headline number and C9's
   worst failure mode read the same on the instrument.** A log sink that stopped
   writing improves the `--slow-usb` wake number exactly as much as a log sink
   that got fast. §20.2's positive log-content assertion is the only thing that
   separates them, and it did not exist in the first draft.
7. **Thirteen chunks in one pull request, across the scheduler's kill path, five
   global locks, the USB transport and the whole log subsystem.** §5.5's "this is
   deliberate de-risking … the scheduler migration cost seventy defects; this
   refactor does not reopen it" is an argument about `toyos-sched`'s *internals*
   being untouched — and §7.3 has now made even that false, since the retire
   handshake changes. `specs/metal-track-history.md` records ~70 defects found in
   code whose own suites were green. "Every chunk passes `cargo test`" is a
   process, not a mitigation for one merge commit. **§21.2's C0–C6 / C7–C15 split
   is the fallback and the owner should be asked before C0, not after C13.**
8. **`usb-transport-break` goes vacuous.** That actuator exists to reproduce "the
   state a transfer that ran out `USB_TIMEOUT_NS` leaves behind". If the bound
   goes, production can no longer reach that state and the gate certifies a path
   the shipping kernel cannot take. Whatever §12.3 chooses, C7 re-points it.
