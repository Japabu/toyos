# Completion Architecture — kill every wait

**This supersedes `specs/plans/iouring-blocking-spec.md` and
`specs/plans/blocking-io-plan.md`, which are deleted with the commit that lands
this document** — a plan dies on completion, and two plans superseded in place
are two more files an agent has to read before learning they say nothing. It is
one deliverable, not two tracks: the first spec was written about *blocking
syscalls* and the second about *a thread that asked to write a file*, and the
reason neither could be built alone is §7 — a kill in a kernel that does not
unwind cannot be a jump, so making a lock parkable and making a wait cancellable
are the same change. Anything either plan measured that is still load-bearing is
quoted here, with its source named as the commit that deleted it.

The claim, in one sentence: **every wait in this kernel that runs on a CPU the
scheduler owns becomes a completion, and the three places a wait may still spin —
boot, an inter-CPU rendezvous, and a dying machine — are named modules a grep gate
enforces.**

Prime directive, unchanged from the rest of the estate: (1) make the bug class
unrepresentable, (2) fail fast at runtime, (3) test. Every number below came from
a command run on `wt/toyos-compl` at `19c761e` on 2026-08-09, or is cited to the
document that measured it.

**Re-verified against the merged tree on 2026-08-15, at `71a0559`.** Both other
branches have landed, so the tree this plan is written about now exists: every
count, line number and table row below was re-run, and the ones that moved are
corrected in place rather than annotated. What that sweep found is worth stating
up front, because it is an argument about the plan's method and not a list of
diffs:

- **The line numbers rotted almost completely and the symbols did not.** All
  twelve rows of §4.1 moved; every one of §4.5's six; seven of §4.4's twelve. Of
  the enclosing *function* names the §4.6 gate actually keys on, one changed
  (`hda_probe`'s is `spin_until_ns`, not `spin_ns`) and one was never right
  (`panic_console`'s is `screen_claimed_by_userland`). **So the site-granular
  allow-list was the right design and a line-granular one would have been
  worthless** — which is the sweep's own vindication of §4.6.
- **Three of C0's predictions came true and one conclusion drawn from them did
  not.** `log_ring.rs` is gone, `fd.rs` is gone, `FUTEX_WAKE_GEN` is gone. But
  the total number of spins went **up**, not down, because three actuator spins
  arrived while one left. §4.4's instruction — *re-run the enumeration rather
  than adjust it by one* — is the only method that survives, and it is now
  doubly earned.
- **Four claims were wrong about the tree rather than stale against it**, and
  each is rewritten where it occurs: §4.1 was not the inventory it called itself
  (three park sites were in no class); §5.6 asserted every subject is
  level-queryable and one structurally is not (§5.3a); §7.2's replacement code
  neither compiles nor terminates; and §20.4 named a gate that was never built.

**The log subsystem is no longer this spec's, as of 2026-08-09.**
`specs/log-architecture-spec.md` (branch `wt/toyos-logd`, `59f9bb6`) rebuilds it
as a per-CPU **record** ring, a kernel console drainer called `klogd`, and a
userland `/bin/logd` that owns `/log`; its §12 is the boundary and this document
is edited to match it. Every row that named the ring, the three sinks, the
"affordable flush" heuristic or the idle loop's two log statements now points at
the L-chunk that owns it. C9 shrinks to the idle loop's declared end state (§11).
The owner ruled the same day that this spec's rejection of a userland `logd` is
**overruled**; log §12.1a argues it on the merits and §23 rejection 11 records the
reversal.

**And on the same day the orchestrator ruled the pipeline order, which is what
discharges the one thing the strike left behind.** The order is
**`endowment → log → completions`**, where it had been
`endowment → completions → log`. §11.4 had this branch re-homing the kernel's
file sink because C7+C8 cannot leave it on the idle loop; under the ruled order
there is no file sink left to re-home, because log L6 has deleted it before C0
merges. **The obligation is discharged, both of its shapes are struck, and the
trace that justified them stays in §11.4 as evidence.**

The reasoning, so no reader re-derives it. Completions **cannot compile** with
the kernel file sink alive — §11.4's trace ends at `xhci::with_disk` needing a
`&Parkable` the idle loop has no way to make, with `SINK`'s raw guard held across
the park — and both available shapes cost real things: the `iod` shape imports
log §13.4's panic-path regression into a branch that has nothing to do with
logging and forces re-pointing `apic.rs:160`'s `wait_for_log_file` and its
`:146` kick loop; the idle-loop-append shape needs C12 before C7+C8 and rests on
a tail-page-resident premise nothing enforces. What the log branch needed from
*this* one is only its §2.6a's lock-free single-waiter post, and that section
already named its fallback (`irq_ring`'s shape: record, and let `drain_irqs`
post), costing an idle-machine latency it can carry. **A performance fallback
with an existing shape beats a compilation blocker with two costly workarounds**
— and log-first removes the work rather than relocating it. That is §11.4's own
third option, taken; §24.9 is closed.

**C0's baseline is therefore `main` after *both* other branches.** Every count,
line number and lock-table row below was measured on pre-endowment, pre-log
`main`; **C0 re-derives all of them**, and the rows the log branch has already
changed are marked where they occur.

---

## 1. The evidence

Four measurements, all pre-existing, all reproducible:

**1.1 Four spinlocks deep at the moment of a disk transfer.**
`io-depth-probe` (kernel feature, `kernel/src/drivers/xhci/wait/mod.rs`) reports
preempt depth **4 from the idle loop and 5 from a syscall**, with the backtrace:
`log_file::SINK` → `vfs::VFS` → `fat32_adapter::VOLUMES` → `xhci::XHCI`, each a
ticket spinlock disabling preemption for its whole life. Measured 2026-08-08 on
`wt/toyos-asyncusb` at `87835d1` (`specs/issues/audio/disk-wait-pins-a-cpu.md`,
and the full backtrace in `blocking-io-plan.md` §1, which this document's own
landing commit deletes — the backtrace is quoted in that entry, which is why the
entry and not the plan is the citation). The number is not derivable from the
call graph: a reader counting names finds three.

**Under the ruled order this is history by C0 and half of it is already fixed.**
The *idle-loop* arm of that reading is the log branch's: after its L6 there is no
`log_file::SINK`, no `log_file::poll`, and no path at all by which an idle CPU
reaches a block device, so C0 has no idle-loop depth to measure. **The syscall
arm is what remains and it is this branch's whole subject** — `ProcessData` →
`VFS` → `VOLUMES` → `XHCI`, unchanged by that branch. C0 re-measures and records
both, and §20.2's target is written against the re-measured number rather than
against 4 and 5.

**`fd.rs` no longer exists and every citation of it in this document was
re-pointed on 2026-08-15.** The endowment branch's chunk 2 (`6d81a73`, on `main`)
deleted it: `FdTable`/`Descriptor` became `HandleTable`/`KObjectRef` and the
dispatch moved to `kernel/src/object/ops.rs`. The two sites this document leaned
on hardest are now `object::ops::fsync` (`kernel/src/object/ops.rs:654`,
dispatched from `arch/syscall.rs:298`) and the readiness question
`object::ops::has_data` (`ops.rs:699`, whose `Console` arm is `serial::has_data()`
at `:703`).

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
different session — `specs/plans/memory-boundary-spec.md`'s M3 three-arm run. It
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
(`Commit::Killed => (pass.dispose_exit().finish(), None)`,
`kernel/src/sched/driver.rs:482`). Everything
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
more the kernel already needs (`Floor`, §3.1; `Budget`, §3.3). Six was the count
this section was written with, **and C1's own sweep made it seven — `Delay`,
§3.5.** `kernel/src/time.rs` is where all of them live, and its module header is
the taxonomy's home now that the types exist.

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

- **`apic.rs:322`'s `MIN_ONE_SHOT_NS` (10 µs)** — the LAPIC one-shot floor.
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

- `LOG_DEFERRAL_CEILING_NS` (1 s), `LOG_DEFERRED_SINCE`, and `log_file.rs:190`'s
  `MAX_BLOCKED_NANOS` (10 s) — **gone before C1 runs, and the order change is
  what settled it.** Their deletion belongs to `specs/log-architecture-spec.md`
  L6, which deletes `log_file.rs` whole and moves the file sink to userland
  (log §12.1). An earlier draft of this bullet said *"C1 runs long before that,
  and RT7 refuses a duration with no kind, so 'it is deleted later' is not an
  answer C1 can give"*, and worked out a `Budget` classification for two of them.
  **That was true under `completions → log` and is void under `log →
  completions`**: C1 opens on a tree where `log_file.rs` does not exist, so there
  is nothing to classify and the classification is deleted rather than carried.
  **C1's sweep therefore counts fewer than §3.4's 41 and must re-derive the
  number rather than subtract from it** — the two named here leave, and C0's
  re-derivation is the only figure worth writing down.

  What survived the strike as a *finding* dies with them: `MAX_BLOCKED_NANOS`
  existed only because `log_file::poll` `try_lock`s the VFS, and neither the poll
  nor the constant reaches this branch.
- `retire_task`'s `RECHECK_NS` (50 ms, `scheduler.rs:441`) — **covered, but it is
  a widening and not a deletion, and the first draft's wording will make an
  implementer delete both halves.** The poster already exists
  (`sched/payload.rs:155` `publish_released` → `waitqs::wake_all`), so parking is
  sound. But the 1 s panic at `scheduler.rs:445` is evaluated *only at the top of
  the `while` loop*, which is re-entered only because the 50 ms deadline returns
  `block_on`. Park with no deadline and the `Tripwire` never fires: a lost wake
  parks forever. **The park must carry `Tripwire(1 s)` itself** — one deadline
  instead of twenty re-polls, expiry a panic instead of a retry. §7.3 then
  re-derives that 1 s, because it now bounds an unwind — **and §7.3's correction
  is that the loop does not wait on the state word at all**, so the re-derivation
  is against `handle.released()`.
- The 10 ms behind a serial-console read — **NOT COVERED. It is the only reason a
  serial-console read ever returns**, and the replacement named in §4.1 P5 and
  §11 is about a different device. On the merged tree it is
  `ReadBlock::Keyboard(nanos_since_boot() + 10_000_000)` (`arch/syscall.rs:1042`,
  computed at `:1031`), consumed by the park at `:1100`. Evidence that nothing
  posts: the 16550's IER is written to zero (`serial.rs:40`, "Disable all
  interrupts"), `virtio_console.rs` has no interrupt handler at all, readiness is
  `serial::has_data()` (`object/ops.rs:703`, inside `ops::has_data`) but the park
  is on `waitqs::KEYBOARD` (`sched/waitqs.rs:39`), whose only waker is
  `keyboard.rs:67` — the i8042/USB keyboard, a different device. **Nothing
  posts.** §11 gives the *i8042* a `Poll`, and the i8042 is the one device here
  whose IRQ line does work (`i8042/mod.rs:1589` unmasks it, `:112`'s `LOST_EDGES`
  counts what it drops). **The correct scope is a third `Poll`, on
  `serial::has_data`, whose `Cadence` is this 10 ms.** The number survives,
  reclassified; the deletion is withdrawn.
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
  **Collateral the first draft missed:** `arch/tlb.rs:77`'s doc derives
  `ACK_TIMEOUT_NS` = 5 s (`tlb.rs:81`) from this constant *by name*, so splitting
  it orphans the one keep §3 was most confident about, and C10 owes that constant
  a new reason.
- `apic.rs`'s `LOG_FILE_DRAIN_NANOS` (500 ms, `apic.rs:144`) — **not a
  `Tripwire`.** `apic.rs:236` logs "the panel is the only copy" and **returns**;
  it deliberately does not panic, because the machine is already going down and a
  second panic loses the report. Under §3's own definition ("a duration whose
  expiry is a **panic**") this is a `Bound` whose expiry is a named refusal.
  Reclassified. **It is the one log-adjacent duration that survives the log
  branch** — its L6 re-derives the *value* against a userland writer and leaves
  the kind to C1 — so C1 classifies it and must not assume the comment beside it
  still describes the idle loop.

### 3.3 What §3 keeps

- `arch/tlb.rs`'s `ACK_TIMEOUT_NS` (5 s, `tlb.rs:81`) — a `Tripwire`; it already
  panics (`tlb.rs:140`). Its *derivation* does not survive: see above.
- `sched/dump.rs`'s budgets — **not `Tripwire`s.** The first draft's own sentence
  gives it away: "their expiry degrades the report field by field, which is the
  point". None of them panics — `ANSWER_BUDGET_NS` (`:65`, consumed at `:212`)
  logs and breaks, `NMI_BUDGET_NS` (`:86`, consumed at `:270`) breaks,
  `TABLE_BUDGET_NS` (`:80`, consumed at `:501`) returns `false` and the summary
  says the census is missing. They are a **sixth shape: a `Budget`, whose expiry
  is a degraded answer.** That is exactly right for a diagnostic on a machine
  already known to be broken, and it must be constructible or the dump cannot be
  written. Also: `ACK_BUDGET_NS` (`:329`) is declared inside
  `#[cfg(feature = "dump-deaf-cpu")] deaf_window()` (`:308`) — a test actuator,
  not one of the four, and listing it beside the three production ones while
  omitting `ABSURD_HORIZON_NS` (`:59`) was an error.
- `xhci/wait/boot.rs`'s `PORT_POLL_NS` (1 ms, `boot.rs:66`) — a `Cadence`. The
  value and the classification are right; the comment the first draft quoted is
  from `fat32_adapter.rs:874`, not from `boot.rs`, whose own text is "How often
  the settle re-reads the port registers."
- `smp.rs`'s 100 ms AP wait — behaves as a `Bound` (`:251` declares the AP absent
  by name) but is a bare inline literal with **no name and no citation**, and SDM
  §8.4.4.1's numbers are the 10 ms and 200 µs beside it, not this. C1 either finds
  a source or it is a `Tripwire`.

### 3.4 The taxonomy is not yet total, and that is C1's job

The sweep found **41 production durations in `kernel/src/`**; the first draft
named 12. The 29 it did not name include every one that fits no kind: all six
i8042 budgets (2,100 ms of boot, whose own comment says no real EC has ever been
timed), `PORT_SETTLE_CEILING_NS`, `EMPTY_BUS_NS`, `READY_BUDGET_NS`,
`HANDOFF_TIMEOUT_NS`, `PAGE_HOLD_NS`, `REPORT_HOLD_NS`, `clock.rs:47`'s 50 ms TSC
calibration, `apic.rs:289`'s 10 ms LAPIC calibration, and both `smp.rs`
`delay_ms` calls (`:225`, `:228`).

**RT7 plus an incomplete taxonomy is a kernel that does not build**, so C1's
deliverable is not "the four kinds exist" but **"every production duration in
`kernel/src/` has a kind and a constructor, or a named exception"** — six kinds
now: `Bound`, `Cadence`, `Tripwire`, `Deadline`, `Floor` (§3.1), `Budget` (§3.3).
A duration that still fits none after C1 is a finding, not a licence to invent a
citation.

**41 is the pre-log count and C1 must re-run the sweep rather than work from
it.** At least `LOG_DEFERRAL_CEILING_NS` and `MAX_BLOCKED_NANOS` leave with
`log_file.rs` (§3.2) before C0 merges, and the log branch introduces none of its
own in `kernel/src/` — `LOG_WRITE_BUDGET` is a constant in a userland program,
which RT7 does not reach. The deliverable is the sweep's *completeness*, never a
particular number.

Two further shapes C1 must not confuse with durations: `Cadence`'s definition
("how fast the bit can physically change") does not describe the cadences the
kernel has — `REPORT_CHECK_NS`, `HEALTH_PERIOD_NS`, `SNAPSHOT_INTERVAL_NS` are
cost budgets and log-rate limits, so the definition widens to "how often a thing
may be re-done, and what makes that rate affordable". And **spin *counts* are not
durations** — `serial.rs:237`'s `PANIC_LOCK_SPIN_LIMIT`, `serial.rs:534`'s
`THRE_SPIN_LIMIT`, `sync.rs`'s 50M/500M — even where a doc comment prices them in
seconds. RT7 must not reach them.

### 3.5 C1's sweep, re-run — and the seventh kind it found

Re-run on `wt/toyos-p2impl` at `c41b831`, by
`rg -no '\b[A-Z][A-Z_0-9]*_(NS|NANOS|MS|US|SECS)\b' kernel/src/` for the named
ones and `rg -n '[0-9]_000(_000)*' kernel/src/` for the inline ones, then
reading every site. **43 production durations**, not 41 and not fewer: the two
this section predicted would leave did leave with `log_file.rs`, and the sweep
found more than that in inline literals and in constants whose names carry no
unit — the four `panic_console` holds, `screen_claimed_by_userland`'s bare 2 s,
`smp.rs`'s AP-start 100 ms, both calibration windows, and
`sys_read`'s console re-poll.

**Thirty-nine of the 43 now carry their kind in the type.** The four that do not
are named exceptions with the chunk that owes each:

| exception | why it is still a `u64` |
|---|---|
| `USB_TIMEOUT_NS` (`xhci/mod.rs:319`) | §12.3's open decision is C7's: a `Tripwire` on the transfer, or a `Budget` at the filesystem layer. Typing it at C1 would be taking that decision |
| `hda.rs`'s and `hda_probe.rs`'s `SETTLE_NS` | a `Bound` whose citation does not exist yet. Their own doc says "the specification's own numbers are microseconds", and §3.2 gives C10 the job of deciding each settle site and saying which. Inventing a section number here is what §3.4 forbids |
| `PORT_DEBOUNCE_NS` | it is `toyos_xhci::portmachine::DEBOUNCE_NS`, a constant in a pure host-tested crate. RT7 reaches `kernel/src/` |

Not counted, and each is a class rather than an item: **ten actuator durations**
(`SLOW_TRANSFER_NS`, `SLOW_CONNECT_NS`, `ARM_WINDOW_NANOS`, `ARM_AT_NS`,
`DEAF_NS`, `ACK_BUDGET_NS`, `PROBE_DELAY_NS`, `heartbeat`'s `PERIOD_NS`,
`DIAG_TICK_NS`, and the i8042 fast-health arm), which §3.3 already treats as
outside the four; **seven instants** held in statics (`FIRST_IRQ_NS`,
`LAST_IRQ_NS`, `ARMED_NS`, `NEXT_REPORT_NS`, `LOG_DURABLE_NS`, `CPU_TIME_NS`,
`LAST_DUMP_NANOS`) — a kind classifies the *interval* somebody chose, not a
timestamp the machine remembers; and the spin counts §3.4 already excluded.

**The seventh kind is `Delay`: a duration the caller *spends*.** Six could not
hold `CODEC_DETECT_NS` (25 frames at 48 kHz between releasing `CRST` and
believing `STATESTS`), the two D3hot recoveries, the SDM's two INIT/SIPI delays,
or either calibration window. Nothing is waited *for* across any of them and
nothing expires: the elapsing **is** the success path, so there is no error, no
panic and no degraded answer. Forcing them into `Bound` would have made "expiry
means the device broke" false of a third of the `Bound`s in the tree. Two
constructors, `from_spec` for a settle a specification mandates and `to_measure`
for a window something is counted across. This is §3.1's and §3.3's method
applied once more, and it is the last one the sweep needed.

**Two classifications came out one square from this document, both recorded at
the site.** `apic.rs`'s `LOG_FILE_DRAIN` is a `Budget` and not §3.2's `Bound`:
that reclassification away from `Tripwire` is right, but `Bound`'s two
constructors both demand a register or a specification section and this number
is policy priced against a measured panel paint — while "the panel is the only
copy" is a degraded answer exactly. And `smp.rs`'s AP-start 100 ms is a `Budget`
and not the `Tripwire` §3.3 offered as the alternative to finding a source: its
expiry already names the AP as absent and boots one CPU short, so making it a
panic is a behaviour change, which C1's own gate forbids. The number still has
no source, which is what §3.3 was right about.

**`Bound::from_register` does not exist yet**, and that is the tree's dead-code
rule rather than an omission: `nvme.rs:429` reads `cap` and never takes `TO` out
of it, so C10 is the first chunk with a register to cite and writes the
constructor beside its first caller.

---

## 4. The wait inventory

`rg -n "core::hint::spin_loop\(\);" kernel/src/` returns **41** calls, **22** of
them under `kernel/src/drivers/`. `scheduler::wait_until` has **7** call sites,
`prepare_wait` **9** and `block_on` **9** — three `prepare_wait` and three
`block_on` inside `scheduler.rs` itself (`:251`/`:389`/`:450` and
`:256`/`:394`/`:455`), and **no** `wait_until` at all: all seven live in
`kernel/src/arch/syscall.rs`. Every one is below, with its disposition. This
table is the refactor's inventory and the migration ledger in §18 counts against
it.

**Re-run 2026-08-15 at `71a0559`, and the header moved in the direction the plan
did not predict.** 39 became 41 despite `log_ring.rs` leaving with its one spin
(§4.4): three actuator spins arrived (`log/nested.rs:104`, `arch/mod.rs:133`,
`main.rs:734`), and the drivers subcount fell from 23 to 22 because the departed
site was under `kernel/src/drivers/`.

### 4.1 Class P — a task waits. Fifteen sites collapse to one.

**The first draft called this "the refactor's inventory" and listed twelve, and
the tree has fifteen.** Three park sites appear in no §4 class at all, and all
three are in the log subsystem: `log/console.rs:461,478` is `klogd`'s park, and
`log/storm.rs:149,150` and `log/nested.rs:81,82` are the two log actuator threads
that park forever rather than exit. §21 says "13 park sites → 1", which counted
`klogd` and neither actuator; **15 is the number, and the two actuators park on
`park_lot()` exactly as the sites this branch deletes it for do.** They are
rows P13–P15 and the conversion has to carry them or `park_lot` cannot be
deleted.

| # | site | waits on | today's bound | after |
|---|---|---|---|---|
| P1 | `arch/syscall.rs:931` | pipe writable | none | park on the write end's completion |
| P2 | `arch/syscall.rs:1088` | pipe readable | none | park on the read end's completion |
| P3 | `arch/syscall.rs:1090` | virtio-sound period | none | park on the `DeviceClaim`'s completion |
| P4 | `arch/syscall.rs:1095` | HDA period | none | park on the `DeviceClaim`'s completion |
| P5 | `arch/syscall.rs:1100` | serial-console key | **10 ms re-poll** (`:1042`) | **the 10 ms stays, as a `Cadence` in a `Poll` on `serial::has_data`** — nothing posts a serial-console key, and the park is on `waitqs::KEYBOARD`, a different device (§3.2) |
| P6 | `arch/syscall.rs:1891` (`sys_accept`, `:1858`) | accept | none | park on the **`Acceptor`**'s `PortShared` (§15 row 5) |
| P7 | `arch/syscall.rs:1456` (`sys_process_wait`, `:1447`) | child exit | none | `SYS_PROCESS_WAIT(proc_h)`, parking on the `ProcessObject` |
| P8 | `arch/syscall.rs:2210,2216` (`sys_thread_join`, `:2205`) | thread exit | none | park on the `ThreadObject`; `SYS_THREAD_JOIN` keeps its `Tid` (§15 row 6) |
| P9 | `arch/syscall.rs:2361,2362` (`sys_nanosleep`, `:2357`) | an instant | caller's | park on a deadline completion |
| P10 | `io_uring.rs:449,458` | a CQE | caller's | the ring **is** an inbox (§5.2) |
| P11 | `scheduler.rs:389,394` (`futex_wait`, `:387`) | a futex word | caller's | park on the bucket's completion |
| P12 | `scheduler.rs:450,455` (`retire_task`, `:422`) | a task's release | **50 ms re-poll + 1 s panic** | park on the release completion **carrying `Tripwire(1 s)` as its own deadline** — the panic is only reachable through the re-poll today, so deleting both parks forever (§3.2). §7.3 re-derives the 1 s: it now bounds an unwind |
| P13 | `log/console.rs:461,478` | a committed log record | none | `klogd`'s park becomes an `Inbox` park (§11) — **and `discard_pending()` at `:441` stays in front of the arm**, because a console-less machine must arm too (§5.3a) |
| P14 | `log/storm.rs:149,150` | nothing, ever | none | a log actuator thread that parks forever rather than exit; it parks on `park_lot()`, so the conversion must carry it or `park_lot` survives |
| P15 | `log/nested.rs:81,82` | nothing, ever | none | as P14 |

**Two rows changed shape rather than position, and both are already half
landed.** P6 no longer parks on a listener at all: `kernel/src/listener.rs` and
`wake_poll_waiters` are gone with the endowment branch, and the site already
parks on `acceptor.waiters()` with the condition `acceptor.has_pending() ||
acceptor.closed()`. What is left for this branch is turning that queue into a
completion, not re-pointing it off a listener. And P7 is one `wait_until` call,
not the `prepare_wait`/`block_on` pair the first draft's two line numbers
described: `sys_process_wait` takes a `RawHandle` with `Rights::WAIT`, resolves a
`ProcessObject` and parks once on `object.finished()`. §18's note that P7 "is
inside `sys_waitpid`, whose number 26 they retire" is spent.

**P11 has nothing left to delete.** `FUTEX_WAKE_GEN` and the wake-generation
protocol went independently of this branch, in `ba76478` ("Convert futex to wait
tickets, deleting the wake-generation dance"), already on `main`: `futex_wait` is
plain register-then-read-then-park and `futex_wake` (`:400`) is one line
delegating to `waitqs::wake_n`. The row is now only the fold onto the bucket's
completion.

**P5's and P12's numbers are exactly what they were**, which is worth saying
because everything around them moved: the 10 ms is
`ReadBlock::Keyboard(nanos_since_boot() + 10_000_000)` (`:1042`) consumed at
`:1100` on `waitqs::KEYBOARD`, and P12's are `const RECHECK_NS: u64 =
50_000_000` (`scheduler.rs:441`) with `give_up = now + 1_000_000_000` (`:442`)
and the panic at `:445`.

### 4.2 Class D — a CPU waits for a device on a thread's behalf. Four spins deleted.

| # | site | today | after |
|---|---|---|---|
| D1 | `xhci/wait/mod.rs:363` (`wait_transfer`, `:340`) | spin, `XHCI` held, 2 s | submit, drop `XHCI`, park on the outstanding slot (§12) |
| D2 | `xhci/wait/mod.rs:299` (`wait_command`, `:292`) | spin, `XHCI` held, 2 s | same |
| D3 | `nvme.rs:118` (`wait_completion`, `:106`) | **unbounded** spin | park on the completion queue's ISR post |
| D4 | `virtio.rs:416` (`submit_and_wait`, `:403`) | **unbounded** spin | park on the used-ring ISR post |

These four are the whole finding. They are the only spins in the kernel that run
on a thread which could have given the CPU back.

**Only D1's line moved, and all four enclosing function names are unchanged** —
which is what §4.6's function-granular allow-list and §19's by-name deletions
rest on.

### 4.3 Class S — a register with no interrupt behind it. Spin becomes `Poll`.

`xhci/wait/mod.rs:169` (`settles`), `hda.rs:767`, `hda_probe.rs:985`,
`iommu/vtd/mod.rs:276`, `iommu/vtd/queue.rs:130`, `nvme.rs:436`, `nvme.rs:460`,
`virtio.rs:455`, `xhci/legacy.rs:181`, `rtc.rs:180`, `fat32_adapter.rs:879`,
`xhci/wait/boot.rs:117`, `hda.rs:775` (`spin_ns`) and `hda_probe.rs:993`
(**`spin_until_ns`**).

Three of them are written byte-for-byte three times against three different
constants — `xhci/wait/mod.rs:163`, `hda.rs:761`, `hda_probe.rs:979`, all three
named `settles` — which
`specs/issues/kernel/driver-waits-without-a-deadline.md` already records. All
become one `Poll<T> { bound: Bound, cadence: Cadence }`.

**The pair at the end of the list is not one name, and the gate is why that
matters.** `hda.rs`'s is `fn spin_ns` (`:772`) and `hda_probe.rs`'s is
`fn spin_until_ns` (`:990`). §4.6's allow-list keys on the enclosing `fn`, so an
allow-list written from the first draft's "`spin_ns`" would have missed one of
them by name and red the tree it was written for.

**Where a `Poll` runs on a thread it parks between reads; where it runs at boot it
spins.** `fat32_adapter.rs:879` is inside `pub fn probe_boot_disks()` (`:852`) —
a boot arm, which is the assumption that sentence makes. NVMe's two `CSTS.RDY`
polls take their bound from `CAP.TO`, which `nvme.rs:429` already reads into a
local: `cap` is not wholly discarded — `:430` takes the doorbell stride out of it
— but the `TO` field, bits 31:24, is never read.

### 4.4 Class R — an inter-CPU rendezvous with no task behind it. Unchanged.

**Fifteen sites, and the enclosing `fn` each is inside — which is what the §4.6
allow-list carries, the line number being the thing that rots.**

| site | enclosing `fn` | what it is |
|---|---|---|
| `sync.rs:52` | `Lock::lock` | the ticket spinlock itself |
| `arch/tlb.rs:131` | shootdown ack | the acknowledging CPU is inside an IPI handler |
| `arch/smp.rs:238` | AP bring-up handshake | boot, no task exists |
| `arch/smp.rs:281` | AP bring-up handshake | as above |
| `sched/dump.rs:230` | `request` (`:170`) | the kick's answer budget |
| `sched/dump.rs:275` | `request` | the NMI budget |
| `sched/dump.rs:361` | `probe_silent` (`:251`) | |
| `sched/dump.rs:401` | `probe_silent` | |
| `sched/dump.rs:547` | `walk_threads` (`:539`) | the census budget |
| `main.rs:399` | `kernel_main` (`:343`), `#[cfg(feature = "debug-wait")]` | the debugger wait |
| `drivers/i8042/mod.rs:771` | `widen_edge_window` | test actuator |
| `arch/tlb.rs:234` | test actuator | |
| **`log/nested.rs:104`** | **`mid_body`** (in `mod armed`, `:99`) | **new** — the log-nesting injection window |
| **`arch/mod.rs:133`** | **`percpu_fetch_add`** (`:112`) | **new** — the `log_shared_reservation` actuator's `sti`/spin/`cli` window |
| **`main.rs:734`** | **`pre_idle_wedge`** (`#[cfg(feature = "boot-actuators")]`) | **new** — a deliberate `cli` + `loop { spin_loop() }` wedge |

**The last three arrived after the 2026-08-09 enumeration and were in no class at
all**, so the site-granular allow-list as first specified would have red the tree
it is written for on day one. They belong here, with the two test actuators
already in the list; they are named because the gate matches on the enclosing
`fn`.

**`log_ring.rs:328` (an ISR may log) was in this list and is gone.**
`RingGuard::lock`'s `cli` bracket and `compare_exchange_weak` spin died with the
file at log L3, and the record ring that replaced it takes no lock on the
producer's path at all (log §2.3). **The prediction came true and the conclusion
drawn from it did not**: the first draft concluded "the allow-list is one site
shorter and §18's 39 becomes 38", and the total went *up* to 41, because the
three new actuator spins outnumber the one that left. §4.4's own instruction —
**C0 re-runs the enumeration rather than adjusting it by one** — is the only
method that survives contact with this tree, and it is now the third time in a
row that adjusting by one would have been wrong.

**A completion cannot serve any of these**: there is no task to park and, for the
shootdown, the acknowledging CPU is inside an IPI handler. An agent who tries to
convert `arch::tlb` produces a deadlock, which is why they are listed rather than
left to be discovered.

### 4.5 Class X — a dying machine. Unchanged.

`serial.rs:269` (`panic_flush`, the panic-path `try_lock` retry), `serial.rs:545`
(`uart_write_bytes`, THRE), `panic_console/mod.rs:423`
(`screen_claimed_by_userland`, the 2 s `PAINTING` checkpoint wait),
`panic_console/mod.rs:771` (`hold`, the halted-machine pager's polled
`i8042::poll_byte` key loop, reachable only from `page_forever`), `apic.rs:239`
(`wait_for_log_file`, `:190`).

**Five, not six: `serial.rs:300` was in this list and is not a dying machine.**
It is inside `pub fn flush_final()` (`:294`), the clean-shutdown drain before
`acpi::shutdown()`, and the function's own doc comment draws the line in terms —
it does *not* take `panic_flush`'s bypass, because "every CPU is still live here,
and reading the ring unsynchronized is only defensible when nothing else will
ever run." Classifying it as a dying machine is exactly what would let a later
agent carry `panic_flush`'s reasoning onto a live one. **It is a bounded
lock-acquire on a running machine — §4.5a's shape, not §4.5's — and it moves
there.**

### 4.5a Class L — a hand-rolled lock the four classes missed

**Two sites.**

`serial.rs:138` is `BackendGuard::lock`'s spin (the `impl` opens at `:130`): a
`BACKEND_LOCKED.compare_exchange_weak` loop over an `AtomicBool` static (`:120`)
taken under `save_and_cli()`, so a CPU inside it is deaf to every IPI. The
root `CLAUDE.md` already names it ("unbounded too, with no contention warning and
no deadlock panic"). It is **not** a `Lock<T>`, so §9's table of statics does
not see it, and it is not a dying-machine spin, so §4.5 does not cover it — it is
the serial backend's mutual exclusion in ordinary operation, taken by every log
drain.

`serial.rs:300` is `flush_final`'s bounded acquire, moved here from §4.5. Same
shape and the same reason: a `PANIC_LOCK_SPIN_LIMIT`-bounded `try_lock` retry on
a machine where every CPU is still live. Its own doc comment is what puts it
here rather than beside `panic_flush`, and the distinction is load-bearing —
`panic_flush` may read the ring unsynchronized because nothing else will ever
run, and `flush_final` may not.

Both stay spins for now, because the alternative is a sleep lock the panic path
and the ISR path both need `try_lock` on and neither has a `Parkable` — which is
what the existing `try_lock` already is. **What changes is that they are named**:
they go on the §4.6 allow-list with this justification, and §23's rejection 8 is
corrected, because "the THRE spin runs on a preemptible thread" is false while
the guard holds `cli`.

**The site is this spec's; its callers are not, and under the ruled order they
have already changed.** The idle loop's `drain_serial` took it on
pre-log `main`; `specs/log-architecture-spec.md` L3 deleted that caller and gave
the guard to `klogd`, which holds it for one bounded chunk at a time (log §4.3).
So C0 finds the *site* exactly as described and its callers already re-pointed —
**verified 2026-08-15: only the line number moved, `:104` → `:138`, and the whole
justification is intact.** The allow-list entry outlives both —
`BackendGuard::lock` is still a spin — so **C13 must not remove it on the ground
that the log branch took the drain away.**

### 4.5b Class B — a boot-only spin inside a file whose other spins are deleted

`xhci/wait/mod.rs:278` is `settle_outstanding`'s spin (the `fn` opens at `:270`).
Its own doc comment justifies it — "Blocking is correct here and only here: this
is the boot scan, so there is no scheduler yet". It is correct and it stays. The
first draft omitted it from all five classes, which is how §4.6's gate came to be
file-granular. **It is also the only §4 citation in the drivers tree that needed
no correction at all on 2026-08-15.**

### 4.6 The gate — and why it must be site-granular

The first draft said `core::hint::spin_loop()` "may appear only in the files
listed in 4.3 (boot arms only), 4.4 and 4.5". **A file-granular gate cannot see
any of the four deletions this document exists to make**, because three of the
four Class D spins share a file with a Class S spin that stays:

| deleted (§4.2) | survives in the same file (§4.3, §4.5b) |
|---|---|
| `xhci/wait/mod.rs:363` `wait_transfer` | `xhci/wait/mod.rs:169` `settles`, and `:278` `settle_outstanding` |
| `xhci/wait/mod.rs:299` `wait_command` | as above |
| `nvme.rs:118` `wait_completion` | `nvme.rs:436`, `nvme.rs:460` |
| `virtio.rs:416` `submit_and_wait` | `virtio.rs:455` |

So the gate is **an allow-list of sites, not of files**: each entry is the
enclosing function's name plus the one-line reason it may spin, and the test
matches on the function a spin is inside rather than the path it is in.
Reintroducing `wait_transfer`'s spin then reds by name. **That list is the scope
statement, machine-checked**, and shrinking it is the only way a later agent can
claim to have removed a spin.

**Its host is `src/sourcegate.rs`, not `src/docs.rs`.** `src/docs.rs` was deleted
by owner ruling in `8d0db10` ("build: delete the doc tests — owner ruling, no
tests over documentation"), so every citation of it in this document is void; the
surviving host-side kernel-source scanner is `src/sourcegate.rs`, which already
has exactly the machinery this gate needs — `kernel_lines()` (`:190`) walks
`kernel/src`, `occurrences()` (`:98`) counts per file, and `code_only()` (`:140`)
strips comments, which is what the two doc-comment false positives below require.
Five gates already live there in its `#[cfg(test)]` module and the new one joins
them.

**And it must not key on `spin_loop()`, because a wait need not call it.**
`rg -n 'while .*\{\s*\}\s*$' kernel/src/` finds **five** lines, of which **three**
are real waits:

| site | what it waits for | class |
|---|---|---|
| `arch/apic.rs:289` | 10 ms of wall clock, LAPIC calibration, in `init_timer` (`:280`) | R — boot, no task exists |
| `arch/smp.rs:297` | `delay_ms`, the AP bring-up delays | R — boot |
| `clock.rs:53` | the HPET counter, TSC calibration | R — boot |

**The other two are not waits and the gate must not red on them.**
`kernel/src/input_merge_test.rs:17` and `:18` are
`while keyboard::try_read_event().is_some() {}` and its mouse twin: draining
loops whose *condition* consumes an item each pass, so they make progress and
terminate. So "a loop whose body cannot make progress" has to be enforced on the
condition too, or the gate reds on two innocent drains the day it lands.

**None of the three is in §4.4's printed list, and the first draft said two
were.** §4.4 names `arch/smp.rs:238,281` — two genuine `spin_loop()` calls in the
AP bring-up handshake, in a different function from the bare `delay_ms` wait at
`:297` — and it names no `apic` line at all, having deliberately moved
`apic.rs:239` to §4.5. The *argument* is sound and unchanged: a gate that greps
only for `spin_loop()` licenses converting a deleted spin into a bare `while {}`
and passing. The supporting claim was false and would have sent whoever wrote
the allow-list looking for two rows that do not exist. The gate matches **any**
loop whose body cannot make progress — `spin_loop()`, a bare `while` with an
empty body, and `core::hint::spin_loop` under any alias — and the allow-list
carries all of them.

**The enumeration was re-run on 2026-08-15 at `71a0559`.** 41 `spin_loop()`
calls, plus the three bare waits above, is **44 wait sites**. Corrections to the
first draft's classification, all now folded into the sections above:
`serial.rs:138` and `xhci/wait/mod.rs:278` were in no class at all (now §4.5a and
§4.5b); `apic.rs:239` appeared in both §4.4 and §4.5, and §4.5 is the right home;
`serial.rs:300` was in §4.5 and belongs in §4.5a; and `log/nested.rs:104`,
`arch/mod.rs:133` and `main.rs:734` were in no class because they did not yet
exist.

`rg -n 'spin_loop' kernel/src/` returns **43** lines, two of which are prose in
doc comments (`xhci/mod.rs:316`, `xhci/wait/mod.rs:160` — both exact and unmoved);
43 − 2 = 41, which is the header's count. A gate matching the bare word rather
than the call reds on those two, which is what `code_only()` is for.

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

**Level-readable is a property of the *record in the inbox*, not of the subject
that posted it**, and the two layers must not be run together — §5.3a is the
whole of why. A subject whose readiness the kernel cannot query still works here,
because the record its post leaves in the inbox persists until taken. That is
live on the tree: `/bin/logd` arms before its first read
(`userland/logd/src/main.rs:187-188`) and re-arms after an empty read
(`:213-219`), and a `klogd` post landing between the empty read and the re-arm is
not lost, because the CQE from the still-outstanding poll sits in the CQ until
`wait` takes it.

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
/// Proof that a record will arrive on the armed inbox for `token`. `!Copy`,
/// `#[must_use]`; `Drop` disarms and drains. A park with nothing armed is
/// untypeable.
#[must_use]
pub struct Armed<'a> { .. }

/// Arm a watch for the running task.
///
/// **Edge-only, for every subject.** The record a post leaves means "state may
/// have moved", never "there is something for you"; the waiter's own predicate
/// stays authoritative and is re-derived after this returns, which is what
/// `wait_until`'s loop does at every call site. §5.3a's level form — where
/// `arm` asks the subject and fires immediately — is not implemented for any
/// subject, and the per-source arm-time recheck it was meant to replace still
/// stands where it always was (`io_uring::process_poll_add`).
///
/// `class` is what this wait's blocked time is attributed to. It belongs here
/// because it is a property of the *subject*: since §5.2 every thread parks on
/// a queue of its own, so a class read off the queue is the same word for
/// every wait in the machine.
///
/// `None` when there is no current task: boot has none, and neither has an
/// idle CPU.
pub fn arm<'a>(subject: Subject<'a>, token: Token, class: WaitClass) -> Option<Armed<'a>>;

/// Park until a record arrives, the deadline passes, or this thread is
/// cancelled (§7). Callers loop and re-derive: a spurious return is legal.
pub fn wait(p: &Parkable, armed: &Armed<'_>, deadline: Deadline)
    -> Result<Record, Cancelled>;
```

**Five divergences from the block above as C3+C4 landed it, and they are
amendments rather than drift.** The struck version named a by-value `Armed`, a
`&mut Parkable`, an infallible `arm`, no deadline and no class. Each is
answered:

- **`&Armed` and not `Armed`.** §5.3a's edge contract needs the arm to *outlive*
  the wait: a caller loops, re-deriving its own predicate between waits, and a
  post landing in that window must find the watch still armed. An arm consumed
  per wait would lose exactly the wake arm-before-check exists to catch.
- **`&Parkable` and not `&mut`.** §6.2 already settles this and gives three of
  this document's own sections as the reason; the block above simply had not
  caught up.
- **`Option<Armed>` and not `Armed`.** Boot and an idle CPU have no current
  task, so there is nothing to arm *for*. A panicking `arm` would make the one
  caller reachable from a kernel thread before the scheduler exists panic.
- **A `Deadline` on `wait`.** §14.1's absolute form; the timed callers
  (`sys_read`'s console, `nanosleep`, `io_uring::enter`) need it and a park with
  no deadline argument would have to smuggle one.
- **A `WaitClass` on `arm`.** The fifth, and the one nobody wrote down: without
  it every park in the machine is `WaitClass::Other`, because the queue a thread
  parks on is its own.

**§6 and §6.2 already said `&Parkable`** — "`completion::wait` and
`SleepLock::lock` both take `&Parkable`" — so the struck block was an internal
contradiction of this document rather than a claim about the tree.

`Subject` is a borrowed reference to the object being waited on — a pipe end, a
listener, a device claim, a process object, an outstanding driver operation, or
the CPU's deadline list. It is **a reference, never an id**, so a destroyed
subject cannot be named and §5.1's "maps no id to any object" holds structurally.

### 5.3a Edge-classed subjects — the one the kernel cannot ask

**A `Subject` is level or edge, and the class is a property of the subject.**

- **Level.** The subject can answer "is there something for this waiter" from
  state the kernel holds. `arm` asks, and fires immediately if the answer is yes.
  Eight of the nine `io_uring::Source` variants are this
  (`io_uring.rs:801-809`).
- **Edge.** The subject cannot be asked, because the readable state belongs to
  the waiter. `arm` asks nothing and never fires immediately; the posted record
  means *"state may have moved"*, never *"there is something for you"*. **The
  waiter's own predicate is authoritative and must be re-derived after `arm`
  returns and before the park** — the arm-then-rescan shape `shard::arm_waiter`
  already uses inside the kernel (`kernel/src/log/shard.rs:503`) and `/bin/logd`
  uses across the syscall boundary (`userland/logd/src/main.rs:187-188`,
  `:213-219`). It costs nothing at the park site: §5.4's one predicate is still
  `inbox.has_record()`, and §5.5's "a spurious return is legal" already licenses
  the re-derive.

**The machine's log is the only edge subject on this tree, and it is one by
necessity.** Three facts, each from code, and each independently sufficient:

1. **No reader cursor exists in the kernel.** A cursor is the caller's own eight
   sequence numbers and a loss count, in the caller's own memory
   (`kernel/src/log/user.rs:1-16`), so there is no state from which any arm-time
   question could be answered. `Source::is_ready` therefore answers `false`
   unconditionally for `Source::Log` (`io_uring.rs:815`), and the comment beside
   it is the argument: answering `true` would complete every poll immediately and
   turn a parked reader into a spinning one.
2. **The producer cannot post.** `emit` runs inside `sync.rs`, inside IRQ
   handlers, inside the scheduler and inside every syscall's locked region, so it
   may take no lock; its whole contribution is a `SeqCst` fence and a relaxed
   load, and the single locked RMW is paid at most once per park
   (`log/shard.rs:445-452`, `:480-492`). One RMW per log line measured 497–504 ms
   → 812–839 ms of boot under TCG
   (`specs/issues/hardware/one-rmw-per-log-line-cost-350ms.md`), and that is the
   number that forbids moving the post to the producer.
3. **So the poster is `klogd`, once per drain batch**, from the one context that
   has just observed committed records and may take a lock
   (`log/console.rs:452` → `log/user.rs:69-75`).

**Four constraints C3's fold must honour, each named from code:**

- **The poster stays `klogd` and stays batched.** A post per record, or a post
  from `emit`, reintroduces the measured regression. The completion `post` for
  this subject is therefore callable only from a lock-taking context, and the
  log's post is not on the producer's path.
- **`klogd` arms unconditionally, including with no console.** Until 2026-08-15
  it parked unarmed there, never woke, never posted, and the one machine shape
  the design exists for told userland nothing (`log/console.rs:190-220`,
  `:462-473`;
  `specs/issues/diagnostics/a-console-less-machine-posts-no-log-readiness.md`).
  Converting P13's park to an `Inbox` park must keep `discard_pending()`
  (`console.rs:441`) in front of the arm.
- **A handle closing is not the subject ending.** One `SysCap` object exists
  machine-wide (`kernel/src/loader/mod.rs:930`) and every reader holds a dup;
  `ops::ends_its_sources` (`object/ops.rs:248`) answers `false` for `SysCap` and
  `Console` precisely so one close does not cancel every log poll in the machine.
  §5.2's "a watch is a node the waiter lends to the object" needs the same
  question asked of the object at close. `log_poll_outlives_a_close`
  (`tests/toyos.rs:515`, `tests/common/logread.rs:186`) is the gate and
  `log-close-cancels-any-syscap` the negative control; neither may be weakened.
- **The userland half must survive C11.** `toyos::ring::Ring` replacing `Poller`
  keeps arm-before-read, and `logd`'s 100 ms `IDLE_NANOS`
  (`userland/logd/src/main.rs:119`) is the bound on a machine that has posted
  nothing, not pacing.

**Why this is a section and not a footnote.** Without it, §5.6's "readiness is
the object's own question" reads as a claim that every subject is
level-queryable, and §23's rejection 3 says the same thing in the strongest form
available — so a reader who takes either literally builds `arm` with no edge path
and the log's poll silently never fires. The contradiction is structural rather
than an oversight, and the tree already carries the answer: `Source::Log`'s own
doc comment says *"Edge-triggered, and it is the one source that has to be."*

### 5.4 The one park/recheck site, and its proof

`kernel/src/completion/mod.rs`'s `wait_inner`, in full:

```rust
// Register on this thread's own parking place, re-check, park. The
// registration precedes the re-check, which is the whole of §2's invariant 4.
let ticket = crate::scheduler::prepare_wait(task.park_queue(), cancel, armed.class);
if task.inbox().has_record() || (cancel == Cancel::Answers && armed.shared.kill_pending()) {
    ticket.cancel();
    continue;
}
crate::scheduler::block_on(ticket, deadline);
```

**It is not in `pass_block`, and the struck version of this section put it
there.** The code it quoted — a `Commit::Parked` arm branching on
`inbox.has_record()` — structurally cannot exist: `pass_block` runs inside the
scheduler driver, where no inbox is in scope and none can be, because the
driver knows tasks and tickets and nothing about what they wait for. The
recheck sits one layer up, between `prepare_wait` and `block_on`, and the
invariant below is preserved exactly: the registration still publishes
`Committing` before the predicate is read, and `commit()` inside the blocking
pass still refuses the park if a claim landed in between. The cancel arm is
what the struck `dispose_none` branch was standing in for, and it is the
stronger form — it withdraws the registration rather than leaving one behind.

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
`wait_inner`, every blocking site in the kernel goes through it, and `dispose_block`
still has exactly one caller (`sched::driver::pass_block`) below it.

**C1's lock is part of the proof and one producer cannot take it.** The log's
`emit` runs inside `sync.rs`, inside IRQ handlers, inside the scheduler and
inside every syscall's locked region, so it may take no lock at all — which is
§5.3a's first constraint. That producer uses `Inbox::signal` rather than
`Inbox::post`: one atomic store, no slot, and therefore no requirement that the
posters to that inbox be serialized. What it gives up is the record's content,
which an edge-classed subject never had to say. `Inbox::post`'s plain writes
keep their precondition, and it is the subject's leaf lock rather than anything
about the arm — the struck reasoning was "one poster *per park*", and `klogd`
starts a new park's epoch while the previous one's producer is still inside
`post`.

### 5.5 What does **not** change

`toyos-sched` is not rewritten. It keeps its tasks, tickets, causes, `WaitQueue`,
`Commit`, `Registration`, `wake_direct`, deadline-in-the-`ParkedEntry` and the
`Claim::Lost → continue` arm. The completion core sits **on top** of it: a post is
"write a record, then do what a wake does today".

**What changes inside `toyos-sched` is the crate's core state machine, and
"everything else is untouched" is withdrawn.** §7.2 makes a killed task with a
live kernel stack runnable instead of reaped, and that reaches every safe point,
not one: `handle_retire`'s parked and ready arms (`cpu.rs:569`, `:575`),
`SchedPass::pick`'s kill arm (`:926`), `preempt_if_due`'s interaction with it
(`:901-916`), `CpuSched::hand_off` (`:461`), `TransitTask::adopt`
(`task.rs:758`), and either `legal()` (`task.rs:222-248`) or a new
claim-arbitrated conversion path. On the verification side it reaches sim
invariant **I14** and the corpus trace that proves I14 has teeth (§7.2a), and
`scheduler-core-spec.md`'s invariant 7, which it contradicts and must amend. It
carries its own termination argument, its own host tests in `toyos-sched/` — where
`cpu.rs` has none today — and its own loom model (§7.3).

This is deliberate de-risking. The scheduler migration cost seventy defects
(`specs/assessments/metal-track-history.md`); this refactor does not reopen it.

### 5.6 The three things that go away

- **The dual-call idiom.** `complete_pending_for_event` has **11** call sites,
  **ten of them paired by hand with a queue wake** — the pair
  `specs/issues/kernel/io-uring-source-half-a-wake-pair.md` records losing twice
  in one cutover. The eleventh is the log's (`log/user.rs:74`, inside
  `post_readiness`): a single batched call with no paired wake, because `klogd`'s
  own park handles that side. It is both the eleventh entry and the
  counter-example to "each paired by hand". After this there is one `post`, and a
  ring and a thread are two entries on one watch list.

  The ten: `process.rs:1453` and `:1465`, `arch/syscall.rs:1801`, `pipe.rs:346`
  and `:372`, `sched/driver.rs:613` and `:632`, `drivers/xhci/hid.rs:141`,
  `drivers/i8042/mod.rs:800` and `:811`.
- **`io_uring::Source` and its `is_ready` match** (`io_uring.rs:800`, the match at
  `:801`). Readiness is the object's own question **wherever the object can
  answer it**; for an edge-classed subject (§5.3a) `arm` asks nothing and the
  waiter re-derives its own predicate. One of the nine arms — `Source::Log` at
  `:815` — is that case, and it is structural rather than an oversight.
- **`waitqs::PARK_BUCKETS` and `scheduler::park_lot`.** `waitpid`, `thread_join`
  and `nanosleep` stop hashing into a parking lot and arm on the object or the
  deadline. `scheduler::wake_task(TaskId)` — the pid/tid lookup — goes with them.
  **The two log actuator threads (P14, P15) also park on `park_lot()`**, so they
  are converted in the same chunk or the static survives the deletion.

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
  `scheduler::init`. **Boot's filesystem access must therefore become
  `vfs::try_lock()` with a named `expect`** — a true invariant on one CPU with no
  scheduler, and a named kernel-bug panic if it ever stops being one.
  **That is work, not a description of the tree**: `kernel/src/main.rs` calls the
  plain spinning `vfs::lock()` everywhere on the boot path today (init, mounts,
  `create_dir`), and the string "boot: the VFS is uncontended" is nowhere in the
  tree. `vfs::try_lock()` is not there either — it existed with no caller at all
  and was deleted on 2026-08-16 as dead code, which is four lines C8 writes back
  with its first caller rather than a mechanism C8 is missing. The first draft wrote it as an existing fact; it is C8's deliverable, and
  the boot/task split §21's C7+C8 row names is exactly this.
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
  back *during* the transfer. Whoever flushes a file parks on the disk from
  inside `Vfs::flush_file`, so `VFS` and `VOLUMES` are held across that park by
  construction. **Two callers reach it on the tree C0 opens** and neither can be
  exempted: a userland `SYS_FSYNC` (`object/ops.rs:668`) and `iod`'s write-back
  (§13). The third, the kernel's own file sink, is gone with log L6 — which
  changes the count and not the argument.
- §8's own doc comment says it: "a lock that cannot be held across a device round
  trip is the defect".
- §2's premise — "the same kill abandons a **held VFS lock**" — is a statement
  that the VFS lock *is* held across the park. If it could not be, §7 would have
  nothing to fix.

It is also unimplementable as an API in two smaller ways, either of which is
fatal on its own: `SleepLock::lock` must park when the lock is held, and with
only `&Parkable` it cannot reach a `wait` that demands `&mut`; and two sleep
locks held at once — which `teardown_resources` does today, `ProcessData`
(`process.rs:1110`) then `VFS` through `ops::close_all` (`:1135`) — needs the
shared borrow to stack, which a `&mut` acquire forbids.

**Recorded rather than quietly dropped**, because the claim is attractive and the
next reader will re-derive it. A reviewer who proposes it again should be shown
this paragraph.

### 6.3 What still guards a spinlock held across a park, and it is not the type system

`Lock::lock` (the ticket spinlock) takes **no** token and must not: it is called
from ISRs, from `drain_irqs`, from boot and from `Drop`, none of which has one.
So the type system cannot see a `Lock` guard, and a spinlock held across a park
stays what it is today — a **runtime** named panic:

- **RT1** `Parkable::of_current()` asserts the context's baseline preempt depth at
  token construction (`scheduler::assert_baseline`, `kernel/src/scheduler.rs:44`),
  so the failure names the trap entry.
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

### 7.1 There are five reap-in-place arms, not three and not one

The first draft named one — `Commit::Killed` in `sched/driver.rs:482` — and it is
the least important. The second draft named three, all inside
`toyos_sched::cpu::handle_retire` (`cpu.rs:562`), and **the three line numbers
are exact and the set is not**: `toyos-sched` reaps a killed task in **five**
places, and the two the second draft missed are the ones that defeat §7.2.

| # | arm | site | when | stack |
|---|---|---|---|---|
| 1 | `handle_retire`, `self.parked.remove(&key)` → `reap` | `cpu.rs:569-573` | the task has been parked for a while | **discarded, guards and all** |
| 2 | `handle_retire`, `self.rq.remove(key)` → `reap` | `cpu.rs:575-580` | woken by a release, not yet run | **discarded, guards and all** |
| 3 | `handle_retire`, `self.running` → `need_resched` | `cpu.rs:582` | running | dies at its next safe point — **and §7.2 changes what that costs** |
| 4 | **`SchedPass::pick`** — `if task.shared().kill_pending() { … reap … continue }` | `cpu.rs:926-936` | **every pick, of every killed ready task** | **discarded, guards and all** |
| 5 | **`CpuSched::hand_off`** — reaps rather than migrating | `cpu.rs:461-469` | a killed task chosen for migration | **discarded** |
| 6 | **`TransitTask::adopt`** → `Err(DeadTask)`, disposed at `cpu.rs:555-557` | `task.rs:755-764` | a kill that lands after the adopt was posted | **discarded** |

(Six rows, five *reaps*: row 3 does not reap, and it is listed because §7.2 makes
it load-bearing.)

**Arm 1 is the one that matters and the first draft did not mention it.**
A thread parked on a disk transfer while holding the VFS sleep lock is in
`self.parked`. It is reaped in place, its stack is freed, and the VFS lock is
stranded forever — which is *precisely* the disaster §2 says the ordering exists
to prevent. `Commit::Killed` covers only the microscopic window in which the kill
lands between `prepare_wait` and `commit`, while the victim is still running.

Arm 2 is the same hazard one step later: a contender woken by a
`SleepLock` release, sitting in the run queue with the previous guard still on its
stack, killed before it is picked.

**Arms 4–6 are why the section had to be rewritten**, and arm 4 is why the second
draft's replacement code was a no-op. `pick` is the crate's *general* answer to a
killed ready task, and `handle_retire`'s own comment (`cpu.rs:583-590`) declares
it load-bearing for termination:

> the sticky kill bit outlives it and *every* safe point honours it: the pick
> reaps a killed ready task, and `WaitTicket::commit` refuses to park a killed
> one. Without that second arm a task that parked through this window would never
> be picked again, so nothing would ever reap it.

So the run-queue arm is not a smaller change than the parked one. Touching either
without the other deletes the termination argument for row 3 as well.

**The quotation above is of the code as it was, and the tree no longer reads
that way.** All six rows landed; `pick` reaps nothing, and the comment that
declared it load-bearing has been rewritten to name what replaces it —
`scheduler::exit_if_killed` at the return to Ring 3, and the victim's own `die`
at the exit its unwind reaches. Quoted here in its old form deliberately,
because it is the thing this section exists to strike; a reader following the
line numbers will find the amended text and not this one.

### 7.2 What must change

**A killed task with a live kernel stack must be scheduled, not reaped. It must
run again with the cancel pending.** That is one sentence and it reaches every
one of §7.1's arms, which is what the earlier drafts got wrong: they changed the
retire and left the general reaper in place.

**The second draft's replacement code is struck. All three of its lines fail, and
the failures are independent** — recorded rather than quietly replaced, because
each is a trap the next reader walks into.

```rust
// STRUCK — does not compile, is not a legal transition, and is a no-op even
// if both of those were fixed:
if let Some(entry) = self.parked.remove(&key) {
    self.rq.push(entry.task.into_ready_cancelled(self.id, now));
    return;
}
// ready: leave it alone.        // ← also struck; see (d)
```

- **(a) `Queue` has no `push`.** Its enqueue API is `insert(vruntime, task)`
  (`toyos-sched/src/queue.rs:87`), and every enqueue in the crate goes through
  `CpuSched::enqueue`, which first calls
  `task.share().enter_runnable(env.frontier)` (`cpu.rs:434`). Skipping that
  desynchronises the per-share runnable refcount the sim walks in
  `check_share_refcounts`.
- **(b) `Blocked → Ready` is not a legal edge.** `legal()`
  (`toyos-sched/src/task.rs:222-248`) admits `(Blocked(a), WakeQueued(b))` and
  `(Blocked(_), Dead)` out of `Blocked` and nothing else, and an out-of-table
  `transition` panics — `an_edge_outside_the_table_panics` (`task.rs:1076`) is the
  gate that says so. The only route is `claim_wake()` (`:412`) → `finish_wake()`
  (`:434`), i.e. `BlockedTask::wake` (`:927`), which is what `handle_wake` already
  uses (`cpu.rs:535-537`).
- **(c) It must therefore *arbitrate*, exactly as `fire_deadlines` does**
  (`cpu.rs:645-660`): remove-then-convert loses the race. If a remote waker has
  already claimed the task — word `WakeQueued` — `claim_wake` returns
  `Claim::Lost`, the in-flight `Msg::Wake` lands on `handle_wake` whose
  `parked.remove` now returns `None` and no-ops (`cpu.rs:528-534`), and the task
  ends up in **no container at all**: never runnable, never reaped, and
  `retire_task` panics at 1 s. The correct shape is
  `parked.get_mut` → `claim_wake` → `Claim::Parked ⇒ wake + place`,
  `Claim::Lost ⇒ leave it, the Wake is in flight`.
- **(d) "ready: leave it alone" is a strict no-op, and it also deletes an
  argument.** `handle_retire` runs inside `cpu.drain()`, called by
  `SchedPass::begin` (`cpu.rs:753`); the same pass then runs `finish_inner`
  (`:875-897`) → `pick()` (`:918`), whose first act on every popped task is the
  kill reap at `:926-936`. A task pushed into `rq` by the retire is popped and
  reaped **in the very same pass**, stack and guards discarded — the disaster
  §7.2 exists to prevent, moved fifteen lines later. And a killed *ready* task is
  never picked at all: `ReadyTask::dispatch` (`task.rs:783-793`) is reachable only
  past that check, and its own doc says so.

**So §7 owns four changes inside `toyos-sched`, not two:**

1. `handle_retire`'s parked arm becomes a claim-arbitrated wake-and-place, per
   (c).
2. `pick`'s kill arm (`cpu.rs:926`) stops reaping a killed task that has a kernel
   stack, and §7 must then answer **what does reap a killed task that never parks
   again** — the question `handle_retire`'s own comment says the pick was there to
   answer.
3. `preempt_if_due` (`cpu.rs:901-916`) is in the same position: once
   `Commit::Killed` is `dispose_none` the killed thread keeps running and
   unwinds, its next quantum expiry puts it back in `rq`, and the immediately
   following `pick` reaps it mid-unwind with every `SleepGuard` still on the
   stack. **A killed task with a live kernel stack is schedulable at *every* safe
   point, not only at the retire**, and that is the property §7 has to state.
4. `hand_off` (`cpu.rs:461`) and `TransitTask::adopt` (`task.rs:758`) reap for
   the same reason and need the same answer.

Once those hold: the task runs, `completion::wait` observes the kill bit and
returns `Err(completion::Cancelled)`, every kernel caller `?`s it out, guards drop
on the way, and the thread dies at the syscall boundary. `Commit::Killed`'s arm
becomes `dispose_none` for the same reason: the task returns to its own code
rather than being switched away from forever.

**Two corrections to the first draft's code, both of which stop it compiling, and
both still exact:**

- `Commit::Killed` is a **unit variant** (`waitq.rs:293`), so
  `Commit::Killed => (…, Some(registration))` has no `registration` to bind.
  `commit()`'s `Killed` arm already calls `self.queue.dequeue(&self.shared)`
  (`waitq.rs:384`), so that path needs no registration and `None` is correct.
- `Cancelled` is **already taken**: `toyos_sched::waitq::Cancelled` is a
  two-variant enum (`Clean` `:274` / `AlreadyWoken` `:277`, declared at
  `waitq.rs:272`) and is already imported into the very file §7 edits
  (`driver.rs:38`). The new type needs a different name — `completion::Cancelled`
  in its own module, referred to qualified.

### 7.2a It contradicts the scheduler's own law, and that must be amended in writing

`specs/scheduler-core-spec.md:32` states as **invariant 7**:

> **A killed task is never migrated and never dispatched again.** The kill takes
> effect wherever the task is; release completes within one pass of the CPU
> holding the task, after at most one message hop per migration in flight, and
> never waits on a timer.

§7.2 requires a killed task to be **dispatched again, on its own stack**, and
requires release to wait for an unwind that may itself park on a device. The same
spec's failure table (`:102`) says "Task killed while in transit between CPUs |
The receiving CPU observes the kill and releases the task; **no dispatch**".

**Neither sentence survives this design, and the branch may not land a tree that
contradicts its own law.** C3+C4 amends `scheduler-core-spec.md` invariant 7 and
its release-promptness clause explicitly, in the same chunk, with the amended
form saying what replaces "never dispatched": a killed task is dispatched exactly
as far as its own unwind, and release completes when the unwind does. The failure
table's transit row is amended with it.

**And the sim's I14 is where that amendment gets teeth or loses them.**
`toyos-sched/sim/src/invariants.rs:202-215` hard-codes today's model in two
halves — "**A killed task is never migrated**", and "a retire completes within
`retire_latency_bound`", which is `QUANTUM_NS + IPI_LATENCY_NS +
max_kernel_section + 2 * RUN_CHUNK_NS` (`:43-45`). That bound cannot survive a
retire that waits for a full kernel unwind and a teardown. The corpus trace
`toyos-sched/sim/corpus/old_migrate_kept_the_corpse_i14.trace` is the negative
gate that proves I14 has teeth, and root `CLAUDE.md` forbids weakening a negative
gate to make a change pass. **C3+C4 owes a written re-derivation of I14's bound
and of what "never dispatched" now means, with the negative control still
red.**

### 7.3 This is a change to `toyos-sched`'s retire handshake, and §5.5 said it was not

§5.5's "the one change inside `toyos-sched` is §7's, and it is a change to
`Commit::Killed`'s *disposition*, not to the handshake" is **false**, and the
correction is not cosmetic. Rewriting the two reaping arms changes the retire
protocol's own termination argument (`retire.rs`'s module note: "whichever CPU
ends up owning the task converts it to a dead task on arrival"), because a retire
no longer converts anything on arrival — it schedules the victim and waits.

Consequences the implementer owns:

- **`toyos-sched` needs its own host tests for the new arms**, alongside
  `retire.rs`'s five existing ones (`retire.rs:150,163,176,194,204`), and the
  cancel loom model (§16.1) must cover retire-of-a-parked-task, not only
  kill-racing-a-commit. **`cpu.rs` has no `#[cfg(test)]` module at all**, so
  `handle_retire`'s and `pick`'s arms have *zero* host coverage today: this is
  not an addition to existing coverage, it is the first coverage those arms will
  ever have. `cargo test -p toyos-sched --lib` is 41 passing tests and none of
  them is in `cpu.rs`.
- **The retirer's bound now covers an unwind, not a reap — and it is not the
  state word it waits on.** `retire_task` is `kernel/src/scheduler.rs:422`, and
  it loops on `sched.handle.released()` (`:443`), not on the word reaching
  `Dead`. Its own doc (`:404-420`) rejects the word reading in terms: *"Waiting
  for the state word to read `Dead` would be too weak: `Dead` is published by the
  reaping transition, one pass before the release, while the dying CPU still
  stands on that thread's kernel stack."* The 1 s panic is at `:445`, with
  `give_up` computed at `:442` and tested at `:444`. **This makes §7.3's own
  warning stronger than it stated**: the bound covers unwind +
  `teardown_resources` + `close_all` + every sleep-lock acquire on the way + the
  release, not merely "the length of a kernel unwind". **C4 must re-derive it
  against `released()` and say what the new number is measured against**, or the
  first busy kill panics the machine.
- **The reap that satisfies today's bound is not on the retirer's CPU.**
  `retire::post` → `post_retire` targets `home_of(shared.state())`
  (`toyos-sched/src/retire.rs:87-97`) and kicks it with `Urgency::Preempt`
  (`:79`); the retirer then parks until the *victim's* CPU has run a pass and
  `Hw::release` has fired. "Effectively instant" is already an IPI plus a remote
  pass plus a release — which is exactly what `retire_latency_bound` prices. The
  argument that the new bound is a large multiple of the old still stands; the
  baseline is not zero and this document should not say it is.
- `dispose_exit` at a park is deleted; the only remaining disposition that exits
  is the one a thread chooses (`driver.rs:370`), which is the sole other caller.
  Confirmed by grep: exactly two `dispose_exit` call sites in the whole kernel
  (`driver.rs:370`, `:482`) and two `dispose_none` (`:368`, `:477`).

### 7.4 `Cancelled` must be consumed, not sticky — or teardown panics

The first draft's hazard note and **RT4** say `ThreadData.cancelled` is set by the
cancel and `arm` asserts `!cancelled`, so "a task that re-arms after a cancel
panics at the offending call site".

**That panics on this spec's own death path.** §7 routes the dying thread through
`teardown_resources` (`kernel/src/process.rs:1095`), which takes `ProcessData`
(`:1110`) and then calls `ops::close_all` under it (`:1135`); releasing a file
handle takes the VFS lock (`object/file.rs:29`, in `OpenFileState::drop`). After
C8 both are sleep locks, so a cancelled thread whose teardown contends on either
**parks — which re-arms — and RT4 panics the kernel**. A userland process killed
while another thread is flushing a file is enough to reach it.

The rule that works:

- **The cancel is a one-shot, consumed by the `wait` that reports it.** After
  `wait` returns `Err(Cancelled)` the flag is clear and the thread may park again,
  which is what teardown needs.
- **Termination comes from the sticky kill bit, not from the flag.** A caller that
  loops instead of propagating gets `Cancelled` again from the next `wait`.
- So the fail-fast moves to where it can be both correct and cheap: **`wait`
  counts the cancels it has reported to one thread and panics on the second**,
  naming the call site. One cancel is the design; two is a caller that swallowed
  the first.

**But the second bullet's mechanism and the first bullet's promise contradict
each other against the real `commit()`, and §7 must resolve it rather than assert
both.** The second draft's reason was "`commit()` still refuses to park a killed
task", and that is exactly true — which is what breaks the first bullet. The kill
bit is sticky and never cleared (`toyos-sched/src/task.rs:159-165`: *"Sticky: set
by the retirer before it posts, never cleared"*; `claim_retire`/`mark_kill` only
ever `fetch_or`), and `WaitTicket::commit` checks it **before anything else** —
`if self.shared.kill_pending() { dequeue; cancel_commit; return Commit::Killed }`
(`waitq.rs:381-387`). **So a killed thread cannot park, ever, including in
teardown.**

Trace it: under §7.2's `Commit::Killed → dispose_none`, a killed thread contending
on the VFS `SleepLock` inside `ops::close_all` gets `Commit::Killed` back from
*every* acquire attempt. It either busy-spins — which is the `sleeplock-spins`
negative gate in §20.3, staged accidentally by the production path — or it takes
a second `Err(Cancelled)` and trips RT4. **That is the exact panic §7.4 was
written to avoid, reached by a different route**, and §5.5's "`Commit` …
unchanged" forbids all three of the available fixes.

**§7 must pick one and write it down before C3+C4 is implemented:**

1. a **non-cancellable park variant** that ignores the kill bit, used only by
   teardown;
2. a **teardown-scoped clearing** of the kill bit, with the termination argument
   restated for the window it opens;
3. a **`commit()` that distinguishes cancellable from uncancellable waits**, which
   is the honest shape and the one that changes `toyos-sched`'s public surface.

**Chosen at C3+C4: the third.** `WaitQueue::prepare_wait_uncancellable` mints a
ticket carrying `Cancel::Ignores`, and `commit()` consults the ticket rather
than the task — so the same thread may hold both kinds one after the other,
which is exactly what teardown needs and what the first two shapes could not
express. The kill bit stays sticky and stays the termination argument; what
changes is that a wait can say whether the kill is *its* answer.

**One caller today, and it is the one §7.3 predicted**: `retire_task`, waiting
for its victim's release. A killed retirer cannot propagate a cancel with the
retire half done, and its bound is its own `Tripwire` rather than the kill.
C5's `SleepLock` is where teardown's acquires join it.

The existing test `waitq::tests::a_kill_that_lands_before_the_commit_refuses_the_park`
(`waitq.rs:549-563`) is green and asserts today's behaviour, so whichever is
chosen amends that test by name. **§5.5's claim that `Commit` is unchanged is
withdrawn here as well as in §7.3.**

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

`kernel/src` declares **45** statics holding a `Lock` (§18 states the command and
why the number moves with the regex) and performs **259** `.lock()` and **10**
`.try_lock()` calls, measured 2026-08-15 at `71a0559` and re-derived at C0.
**Four change, and there is no fifth.**

| lock | today | after |
|---|---|---|
| `vfs::VFS` | `Lock<Option<Vfs>>`, 29 textual call sites through 2 doors (`vfs.rs:29`, `:41`) | `SleepLock<Vfs>`; every task-side door takes `&Parkable`, boot uses `try_lock` |
| `fat32_adapter::VOLUMES` | `[Lock<Option<FatDevice>>; 2]` (`fat32_adapter.rs:316`) | `SleepLock` |
| `xhci::XHCI` | `Lock<Vec<XhciController>>` (`xhci/mod.rs:1775`) | `SleepLock`; `poll_if_pending` uses `try_lock` (§12) |
| `process::ProcessData` | `Arc<Lock<ProcessData>>`, 68 `with_fd_owner_data` sites | `Arc<SleepLock<ProcessData>>` |

`ProcessData` is on the list because §2's own example needs it: `SYS_FSYNC`
(`arch/syscall.rs:298`) and `SYS_CLOSE` (`:274`) reach `Vfs::flush_file`
(`vfs.rs:538`) through `object::ops` — `ops::fsync` at `:668` and
`OpenFileState::drop` at `object/file.rs:31` — from inside `with_fd_owner_data`,
so a userland `fsync` of a disk-backed file waits under
`{ProcessData, VFS, VOLUMES, XHCI}`.

**`log_file::SINK` was the fifth row, then a conditional one, and under the ruled
order it is no row at all.** On pre-log `main` `log_file::poll` holds
`SINK.try_lock()`'s guard across `sink.flush(&mut vfs)` (`log_file.rs:291`,
`:312`), which is the whole device round trip, so once `VFS`, `VOLUMES` and
`XHCI` are sleep locks that guard would be a raw ticket `Lock` held across a park
— §9.1's trip, by construction. **That is why the earlier draft made it C7+C8's
problem, and it is the problem `endowment → log → completions` removes rather
than schedules.** `specs/log-architecture-spec.md` L6 deletes `log_file.rs`
whole, `SINK` (`log_file.rs:208`) and its `Lock` with it (log §8.1, §12.1), and
that happens before C0 merges. So C0 finds four convertible statics, not five,
and this branch neither deletes `SINK` nor works around it. §11.4 records the
trace, struck.

### 9.1 The new baselines, and why weakening one is forbidden

`scheduler::assert_baseline` (`kernel/src/scheduler.rs:44`) stays exactly as it
is. `BASELINE_TRAP = 1` (`:64`), `BASELINE_IRQ_EXIT = 0` (`:72`). What changes is
*where* it fires and what a trip means:

- `Parkable::of_current()` asserts the baseline **at token construction**, so the
  failure names the entry rather than the park.
- A kernel thread's baseline is `0`; `Parkable::of_current` reads the context and
  picks. The two are not interchangeable and the token records which it was.
- `io-depth-probe` is re-sited to fire **at the park** rather than at the spin, and
  its target is **1 from a syscall and 0 from a kernel thread** — the trap entry's
  own level and nothing else. Not 0 and 0: that is unreachable, and a stage judged
  on an unreachable number is one that gets fudged. **The kernel thread it reads
  is `iod`'s write-back and nothing else**: the idle loop's flush at depth 4 is
  gone with `log_file.rs` before C0, so there is no intermediate reading and no
  §11.4 shape to attribute one to.

**Weakening is forbidden and here is the argument, unchanged from `scheduler.rs`'s
own comment** (`kernel/src/scheduler.rs:34-39`). A park with a spinlock held parks
that lock on a stack nothing returns to, and every other CPU that takes it spins
into `Lock::lock`'s 500M-spin `DEADLOCK` panic — which names the victim and never
the culprit. The
assertion is the only thing that names the culprit. After this refactor a trip can
mean only one thing: a raw `Lock` still held on a path that was supposed to be
converted, i.e. **the conversion is half done**, and a half-converted path must be
a named panic rather than a wedge. Raising a baseline to make a red go away
converts a compile-and-boot failure into a field investigation.

---

## 10. Kernel threads

Three, not one, because a stuck USB enumeration must not stop the log.

| thread | owns | why it is a thread | who builds it |
|---|---|---|---|
| `klogd` | the kernel's console drain — body and name are `specs/log-architecture-spec.md` §4.3's | it must run on an idle machine, and a runnable task is what stops a CPU halting | **the log branch, at its L3** |
| `usbd` | the xHCI port machine, enumeration, endpoint recovery, `Poll`ed register settles | `poll_if_pending` runs at the top of every pass on every CPU and may not wait | C6 |
| `iod` | the write-back queue: deferred `close` flushes, page-cache eviction write-back | `Drop` cannot take a `Parkable` (§13) | C6 |

**C6 shrinks, and this is the largest thing the order change hands to the other
branch.** The machinery — the trampoline beside `process_start`/`thread_start`,
the `ProcessObject` whose address space is the kernel address space,
`driver::spawn`'s `cr3` derivation against `paging::KERNEL`, `sched::dump`'s
naming, and the recoverable-panic predicate extended to cover a kernel thread —
is all built by log L3, because that branch needs `klogd` and has no C6 to
inherit from (log §4.3, log §12.1). **C6's own deliverable is therefore: spawn
`usbd` and `iod` on machinery that exists, add their two rows to the predicate as
the *recoverable* ones — `klogd`'s is deliberately not (log §4.3) — and do the
`KernelPayload.address_space: Option<PageTables>` → non-`Option` retype (§15 row
12), which stays here because a single kernel thread gave it no second caller.**

**The rename is discharged before C6 is written.** `/bin/logd` is a userland
program after log L6 and `sched::dump` names threads, so two things called `logd`
in one report is a collision a dump cannot survive (log §12.3); the thread is
`klogd` from L3, and C6 finds it named. The reason is kept because a later agent
will otherwise re-collide them, and the name still has to be right in
`blocked_dump`'s assertion, §9.1's `io-depth-probe` target, §22 and §24.

**"Three is one too few" is void, and this is the one place the strike changes an
answer rather than an owner.** That paragraph asked whether `logd` should be two
threads, because one thread draining both the serial and the file sink loses
serial when the file sink parks on a hung stick. After the strike **there is no
kernel file sink for a kernel thread to drain** — log §4.1 draws the line at "the
kernel never writes a file", so `klogd`'s drain has no disk in it and the
question cannot be posed. C6's table is `klogd`, `usbd`, `iod` with nothing open.
The failure mode itself does not vanish; on the tree C0 opens it belongs to
`/bin/logd` and to `iod`, which is §12.3's bound and no longer §11.4's shape.

**Cardinality: one of each, machine-wide, and that is a decision the first draft
did not record.** At the 128-core target the root `CLAUDE.md` sets, a single
`iod` draining write-back for 128 cores' closed files is a serialisation point
nobody has sized; log §15 leaves per-CPU `klogd` open for the same reason and
neither branch measures it. §5.2's "it deletes the 128-core sharding risk" is
about the *completion core* and does not cover these. **C6 records the
measurement or the reason one is enough**; per-CPU is the obvious escape and
costs nothing to leave open.

Mechanics: a task with no user address space, at baseline 0, running a Rust
function. **This is built, on `main`, and C6 inherits it rather than owing it.**
The first draft named `driver::spawn`'s
`.expect("spawn: task without an address space")` as the arch work still to do;
commit `c1be7c4` ("kthread: the scheduler could always host one, and a single
expect was the whole refusal", 2026-08-14, on `main`) removed it. `driver::spawn`
(`kernel/src/sched/driver.rs:257`) now derives `cr3` with
`match new.address_space.as_ref() { Some(space) => …, None =>
crate::mm::paging::kernel_cr3() }` (`:263-265`) — no panic, no assert, and
`address_space: None` documented at `:242` as *"a kernel thread and not an
error"*. The trampoline is `loader::start::kernel_start`
(`kernel/src/loader/start.rs:121`), beside `alloc_kernel_stack` (`:24`), which
already takes it as a parameter; a kernel thread's is *simpler* than either
existing one (no `initial_user_state!`, no `iretq`, no `USER_CS`).
`kernel/src/sched/kthread.rs` exists with the three-thread design and its own
module header names this chunk — *"the completion branch's C6 spawns `usbd` and
`iod` on it"* (`:5`, `:39`). **The landed code was written with C6 in mind**, so
C6's arch work is to use it, not to build it.

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

## 11. The idle loop's declared end state — and what this spec no longer owns

**This section used to rebuild the log subsystem and it does not any more.**
`kernel/src/log/`, the record ring, the console drain and `/log` are
`specs/log-architecture-spec.md`'s, whose §12 is the boundary. Struck from here,
with the chunk that now owns each — and under the ruled order **every row below
is already done when C0 merges**, so this is a table of what C0 will find rather
than of what it must wait for:

| struck from this spec | now owned by |
|---|---|
| `kernel/src/log/` and its file layout | log **L1** — and it is a **record** ring, not the 64 KiB byte ring this section assumed |
| the serial sink and its drain | log **L3** |
| **the file sink — deleted, not rebuilt**; it moves to a userland `/bin/logd` | log **L6** |
| the panel sink | log **L2** |
| `flush_log_file_if_affordable` (`driver.rs:722`), `LOG_DEFERRAL_CEILING_NS` (`:701`), `LOG_DEFERRED_SINCE` (`:706`), `log_file_flush_due` (`:743`), `owes_wake` (`:832`) | log **L6**, and **before C1 runs**, so C1 classifies none of them — §3.2 |
| `drain_serial` (`:762`) from the idle loop, and its `BackendGuard::lock` spin with interrupts disabled | log **L3** — the *site* stays on this spec's allow-list, §4.5a |
| the pre-`hlt` condition `log_file_flush_due` | log **L6** — **deleted, not re-pointed** |
| the pre-`hlt` condition `log_ring::has_pending` | log **L3** — **deleted, not re-pointed.** The first draft had L3 turning it into `log::pending()` for this branch to delete later; it did not. The kernel's own comment records both removals (`sched/driver.rs:547-557`): *"**No log condition survives L6, and its absence is the point.** Two used to be here."* |
| `log_file::SINK` from §9's lock table and §19's ledger | log **L6**, unconditionally — §9 |
| `MAX_BLOCKED_NANOS` (`log_file.rs:190`) | log **L6**, with the file it is in |
| `idle_loop_is_the_declared_body` | log **L6** (log §9.5), amended twice here — §11.3 |
| the conclusion *"A userland `logd` was considered and rejected"* | **overruled** — log §12.1a, and §23 rejection 11 |
| *"Three is one too few"* — two `logd` threads against a bounded file-sink wait | **void** — §10 |

**Three things travel the other way, and they are new with the order ruling.**
The log branch takes its §2.6a fallback because the completion core does not
exist when its L3 runs, and that leaves work here that this plan did not have:

| what the log branch leaves here | what this branch owes |
|---|---|
| `emit` signals with a relaxed `LOG_PENDING` store and `drain_irqs` posts the wake through `waitqs`; `klogd` parks with `prepare_wait`/`block_on` | convert it to the degenerate completion post log §2.6a describes — `commit_and_signal` / `arm_waiter` / a one-waiter `Inbox` — **with its loom model, log §2.5's W3, which nothing has written**; the model is the deliverable, not the conversion, because on x86 a lost wake here is invisible to every guest test |
| ~~one surviving pre-`hlt` condition, `log::pending()`~~ | **Void: the log branch left none.** It deleted both log conditions rather than re-pointing one, so C0 finds no log condition on the halt check. What it did leave is an *uncalled* predicate, `log::console::pending()` (`kernel/src/log/console.rs:251`, `serial::has_console() && DRAINED.any_pending()`), with zero callers repo-wide — dead code to delete, not a condition (§19) |
| a **ninth** `io_uring::Source` — `Source::Log` — carrying the **sixth** `IO_URING_WATCHERS` static (log §3.2) | C3 folds it into the one watch list; §19's deletion list gains it. **And the fold must carry §5.3a's edge contract**, which is the thing this row does not yet say: `Source::Log` is the one subject `arm` cannot ask |

All three belong with C3+C4, which is where the one park site lands, and **none
of them was in any chunk's stated deliverables** — recorded here and added to
C3+C4's row rather than discovered when the log condition survives to C13.

What is left is the idle loop, and it is worth stating on its own because §1.1's
checkable end state is a property of *that* loop and not of the log.

**The pre-`hlt` list C0 finds is two conditions, not three:
`i8042::verdict_due` (`sched/driver.rs:546`) and `xhci::port_work_pending`
(`:570`); the log branch left none.** Both are this spec's, and each **becomes a
runnable task or an armed deadline**, which is why the halt check can shed them
rather than merely move them: a CPU does not halt while anything is runnable, and
it does not sleep past an armed deadline. That is `toyos-sched`'s Invariant T,
already proven (`scheduler-core-spec.md` §8.4).

The full disjunction as C0 finds it, for the declared-set gate to be written
against: `doorbell().kick_pending() || preempt::need_resched() ||
irq_ring::any_pending_self() || !mailbox_is_empty() || i8042::verdict_due() ||
xhci::port_work_pending()`. **This removes the halt check's only claimed
dependency on the wake conversion**, which the struck version of this paragraph
asserted.

`i8042::verdict_due` in particular: the verdict fires at an instant, so it becomes
a `usbd` deadline park. And where the i8042's interrupt line is not trustworthy —
the T14 hands over an uninitialised 8042 — the driver declares itself a **polled
device** with a `Poll { bound, cadence }` on `usbd`, which is where a device-defect
workaround belongs. It stops being an invisible 10 ms inside `SYS_READ` (§4.1 P5).

### 11.1 `log_health` stays in the idle loop, and the argument is the code's own

An earlier draft of the log spec asked this one to put `scheduler::log_health()`
(`driver.rs:687`) on `iod` or give it its own cadence source, on the premise that
C9 was about to empty the idle loop. **Neither: it stays where it is, the premise
was only ever that the loop would be empty, and log §12.2 now adopts this answer
rather than asking the question.**
It never was — `reap_poisoned` cannot move, below — and three things make the
move actively wrong:

- **A thread cannot produce the datum.** `NEXT_HEALTH` (`scheduler.rs:601`) is
  per-CPU because *"which CPUs reach idle is most of what the line says"*
  (`:598-600`), and `ready_len`/`parked_len` are `try_with_cpu` on the caller's
  own `CpuSched` (`driver.rs:744`, `:748`), which is `!Sync`. A single kernel
  thread runs on one CPU at a time, so it can only ever report the CPU it happens
  to be on. The line moved onto a thread is not a worse version of the line; it
  is a different and smaller one.
- **The line's own doc forbids the reading a deadline park would give it.**
  *"What it must not be read as is a heartbeat: it comes from a CPU passing
  through idle, so a quiet machine prints nothing and a gap is not evidence"*
  (`scheduler.rs:622-624`). A periodic park makes it exactly a heartbeat.
- **A 10 s park is a wake that does not exist today.** `SNAPSHOT_INTERVAL_NS` is
  a *rate limit* on an opportunistic check — §3.4 already reclassifies it as one
  — and converting a rate limit into a deadline wakes a CPU that would otherwise
  halt. Root `CLAUDE.md`: anything added to the idle loop is an audio change, and
  so is anything that adds a wake to a machine with nothing to run.

So `log_health` costs one clock read and one relaxed compare per idle trip, on a
CPU already awake, and it is a `Cadence` under §3.4's widened definition. **C9
owns nothing here but saying so**, and the payoff is concrete: the two tests that
read its `sched: cpu=` counts as upper bounds (`tests/toyos.rs:8974`, `:9478`)
**stay live and unmodified**, so the "the CPU still halts" check they document
keeps its teeth over exactly the change C9 makes to the halt condition. The
struck plan would have made both vacuous.

### 11.2 `reap_poisoned` still cannot move

`scheduler::reap_poisoned()` (defined `scheduler.rs:510`, called from the idle
loop at `driver.rs:688`) zombifies threads that died in
panic recovery. **It cannot move to a kernel thread, and the type system says
so.** `scheduler.rs:520` takes a *blocking* `process::PROCESS_TABLE.lock()` and calls
`collect_orphan_zombies(table, IdleProof::new_unchecked())`; `IdleProof`
(`process.rs:713`) is a zero-sized proof that the caller is on the per-CPU idle
stack, and it exists because dropping the thread entry you are running on is a
use-after-free. A §10 kernel thread has its own kernel stack and a
`ProcessObject`, so `iod` is precisely the caller `IdleProof` forbids. Two more
ties: its own doc names the idle loop as "the one context that provably holds
none of the locks the panicking thread may have been holding", which an ordinary
task taking the VFS and `ProcessData` sleep locks is not; and `scheduler.rs:71`
names this guard's drop as the idle loop's only route to `BASELINE_IRQ_EXIT`.

**So `reap_poisoned` stays in the idle loop.** C9 owns re-deriving the two
arguments or leaving it where it is; leaving it is the default and needs no
justification, because it is where it already is.

### 11.3 The end state, and the two statements that are not this branch's

**C0 already finds the body at its end state**, and the order change is what did
that: `log_health()`, `reap_poisoned()`, `pass(Dispose::None)` and **three**
`#[cfg]` probes — `deaf_window`, `metal-panic-probe` and `heartbeat::poll`
(`driver.rs:678`, `:684`, `:699`). `drain_serial()` and
`flush_log_file_if_affordable()` are gone with log L3 and L6, and no shape of
§11.4 puts anything back. The body also calls
`crate::object::drain_zero_handles()` (`driver.rs:692`), which §15 row 9 has
something to say about. **This branch removes nothing from the body at all**;
what it changes is the pre-`hlt` list, which C0 finds at **two**
(`i8042::verdict_due`, `xhci::port_work_pending`) and C9 takes to none. The
struck version said three and had the wake conversion removing a log condition;
there is none to remove.

**`idle_loop_is_the_declared_body` was to land with the log branch and did
not.** The name occurs seven times in the repo and every one is prose: six in
`specs/log-architecture-spec.md` — including its own ledger row, *"|
`idle_loop_is_the_declared_body` | L6, and amended twice afterwards |"* — and one
comment at `kernel/src/sched/driver.rs:556` claiming it *"keeps a fourth from
being quietly re-added"*. There is no such test in `tests/`, in
`src/sourcegate.rs`, or anywhere else; contrast `every_boot_config_runs_logd`,
L6's other host gate, which *was* built (`src/build.rs:1716`).

**So this branch builds it rather than amending it** (§20.4), as a
`src/sourcegate.rs` declared-set test over the pre-`hlt` disjunction, and C9's
single amendment removes both device conditions at once. That is what a
declared-set gate is for: the amendment is a diff a reviewer reads, and a
condition quietly re-added is invisible to every behavioural test.

`drain_irqs` keeps only: consume this CPU's `irq_ring` records and post the
completions they name, plus the dump's serve. `poll_if_pending` leaves it (C7).
**§1.1's checkable end state is therefore split across the two branches**, and
this spec delivers the half that is about waits.

### 11.4 The obligation the strike created — **discharged by the order ruling**

**Ruled 2026-08-09: the pipeline is `endowment → log → completions`, so there is
no kernel file sink left for C7+C8 to re-home.** This section is kept because the
trace below is the *evidence* that produced the ruling, and because a discharged
obligation that is simply deleted is one the next reader re-opens.

**Every line number in this section is pre-log `main` and is deliberately not
re-pointed.** `log_file.rs`, `SINK`, `flush_log_file_if_affordable` and
`log_file::poll` no longer exist; the trace is a photograph of a tree that is
gone, and re-pointing it at anything would make it look live. Only the two live
citations in the paragraph after it — `kernel_log_file` and
`screen_fatal_halt_composited` — are kept current.

The obligation was: the kernel's file sink outlives this branch, and it cannot
outlive C7+C8 unchanged, because **this** branch is what makes `VFS`, `VOLUMES`
and `XHCI` sleep locks. Trace it on pre-log `main`:

```
idle_loop                     driver.rs:689
  → flush_log_file_if_affordable → log_file::poll   driver.rs:739  (its only caller)
      SINK.try_lock()         log_file.rs:291   [raw Lock, held across the flush at :312]
      vfs::try_lock()         log_file.rs:294
      → Sink::flush → vfs.flush_file → FatVolume::write_at → VOLUMES
      → UsbBlockDevice::write_blocks → xhci::with_disk → XHCI.lock()  xhci/mod.rs:1872
```

That is §1.1's stack and `io-depth-probe`'s **4 from the idle loop**. After C7+C8
`with_disk` needs a `&Parkable` and the idle loop has none (§6.1), so the path
does not compile; and if it did, `SINK`'s raw guard is held across the park
(§9). Leaving it alone was not one of the options, and neither was deleting it
from here: `kernel_log_file` (`tests/common/volumes.rs:485`), `esp_files`,
`screen_fatal_halt_composited`'s `/log` half (`tests/toyos.rs:3811`, which reads
the volume off the image and is muted *so that* half tests something) and
`/bin/console`'s scrollback all read this boot's log file, and §21's rule is that
every chunk boots and passes `cargo test`.

**Two shapes were available and both are struck.** They are recorded in one
sentence each so the ruling can be checked, not re-litigated.

- ~~**The drain moves onto `iod`.**~~ Cost: log §13.4's panic-path regression
  list arrives in a branch that has nothing to do with logging — the drainer
  stops being *"the one context that provably holds none of the locks the
  panicking thread may have been holding"* — and `apic.rs:160`'s
  `wait_for_log_file`, its `:146` comment (*"every other CPU's idle loop is still
  running `log_file::poll`"*) and its kick loop all have to be re-pointed here.
- ~~**The append stays on the idle loop and only the device work is queued.**~~
  Cost: it needs C12 before C7+C8, and `Sink::append`'s tail-page merge is only
  device-free while the tail page stays resident — the premise
  `sink-append-error-unreachable` records, which nothing enforces.

**What the ruling took instead is the third option this section named: land the
log branch first.** log L6 deletes `log_file.rs`, `SINK`,
`flush_log_file_if_affordable` and the idle loop's flush statement; it re-points
`wait_for_log_file`, `apic.rs:146`'s comment and the kick loop at
`LOG_DURABLE_NS` itself; and `kernel_log_file` is re-pointed at `/bin/logd` there
rather than kept green against a sink this branch had moved. **So C7+C8 owes none
of it**, §19's *"one name may come back"* is void, §20.3's `reintroduce-idle-flush`
has nothing to reintroduce, and §24.9 is closed. The cost of the ruling is paid
on the other branch (log §12.0, §12.6) and this section is where it was priced.

**One correction stays, because it was true independently of the ordering.** The
log spec named the test `screen_fatal_composited` in five places and no such test
exists — the two real names are `screen_fatal_halt` (`tests/toyos.rs:334`) and
`screen_fatal_halt_composited` (`:340`), and only the second has a `/log` half.
Fixed there.

---

## 12. xHCI

The machinery exists. `toyos_xhci::job::{Await, Stages, Outstanding}` matches a
Transfer or Command Completion Event to the operation that asked for it — **by its
Command TRB address, never by being first** — `dispatch_event` offers every
arriving event to it, and `advance_outstanding` runs from `poll_if_pending`.
Teardown and endpoint recovery were converted at X2a/X2b. This is the fourth
caller and `specs/plans/xhci-port-machine-plan.md` X2c scopes it.

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
   reachable for `klogd`, `usbd` or `iod`**. **Under the ruled order it *is*
   reachable for the log**, because `/bin/logd` is an ordinary userland process
   whose `Process` handle init holds (log §5.4, log §5.7). That is one canceller
   this list did not have; it does nothing for `iod`, which is who parks on
   everything else.
3. **Bulk-Only Reset Recovery** — **unreachable once the bound is deleted.**
   `scsi` (`xhci/wait/msc.rs:586`) is its only caller and calls it only on
   `Err(broke)` from `bot` (`:684`); `framed_phase` (`:658`) produces
   `Broke::Silence` only when `wait_transfer` returns `None`; and `wait_transfer`
   (`xhci/wait/mod.rs:340`, its spin at `:363`) returns
   `None` on exactly two events — **the 2 s deadline, or the port going away**.
   So canceller 3's trigger *is* the bound being removed, and canceller 1 is its
   only other trigger.

**So a present-but-hung stick parks the flushing thread with nothing able to end
it.** What that costs is worse than a parked thread, and §22's row had it
backwards: on pre-log `main` the 2 s bound produces `Scsi::Broken` →
`write_blocks` `Err` → `Sink::flush` `Refusal` → `log_file.rs`'s
`disable_file_sink()` (pre-log `main`, and the file is gone), so **the sink turns itself off, says why, and serial keeps
working.** After the change no error is ever produced, so nothing is ever
disabled. **The chain still exists on the tree C0 opens; it has moved to
userland** — `SyscallError::Io` → `/bin/logd`'s `LOG_WRITE_BUDGET` → one
`alert!`-grade console line naming the volume dead (log §5.4) — and it still
needs an error to fire.

**The log strike halves this and the order ruling does not close the other
half.** The half removed is the worst one: `klogd` drains the console and has no
disk in it (log §4.1), so a hung `/log` stick can no longer take serial with it
and the T14's "total logging loss with no line saying why" is gone by
construction — which was §23 rejection 9's argument reappearing one level down,
and is now unreachable. **The half that remains has a different victim from the
one §11.4 named**: not a thread holding the kernel's file sink, because there is
none, but

- **`iod`**, whose park stops **every** write-back in the machine — `SYS_FSYNC`,
  deferred close flushes and page-cache eviction — and which nothing can kill;
- **any userland process parked in `SYS_FSYNC`, including `/bin/logd`**, which
  *can* be killed and whose give-up policy (log §5.4) is a five-second budget
  sitting on top of an error the transport currently supplies.

**So the bound is still owed and the reason is sharper than it was.** On the
tree C7 opens, `USB_TIMEOUT_NS = 2_000_000_000` (`xhci/mod.rs:319`, **CONFIRMED**
2026-08-09) is what turns a hung stick into `Scsi::Broken` → `write_blocks`
`Err` → `SyscallError::Io`, and log L6 has shipped a daemon that reads that error
and says *"`/log` has not answered in 5s — this boot's log is on the console
only"*. Deleting the bound with nothing in its place does not merely park a
thread: **it makes a shipped daemon's declared policy unreachable, silently.**
The self-disable chain still needs an error to fire; it is now in userland, and
it is a caller this decision can be checked against rather than a hypothesis.

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

**`fd.rs` is gone and the `Drop` is not.** The endowment branch replaced the
descriptor table with the object-handle model, so the impl this section is about
is now `impl Drop for OpenFileState` (`kernel/src/object/file.rs:27`), which takes
`crate::vfs::lock()` at `:29` and does the same two operations in the same order
— flush if modified (`:31`), then unconditional `file_cache::release` /
`close_file`. **The substance of §13 is unaffected**; every `fd.rs:<line>`
citation in this section and in §18 is re-pointed. `fsync` is a different
function that this section *keeps* synchronous, and it is now `object::ops::fsync`
(`object/ops.rs:654`).

`Drop` cannot take a `&Parkable`, so with a sleep-locked VFS it cannot flush.

**The flush moves out of `Drop`.** A closed file with dirty pages is pushed to a
write-back queue that `iod` drains with its own `Parkable`. That is exactly the
endowment spec's deferred zero-handle queue (their §1.1), so:

- `FileObject::on_zero_handles` → `writeback::push(file)` (§15 row 9b).
- `SYS_CLOSE` becomes asynchronous write-back. It never promised durability.
- `SYS_FSYNC` **submits a write-back and parks on its completion**, because a
  caller asked. `close-cannot-report-io-error` is where the honesty of that
  answer is currently owed.
- `SYS_SHUTDOWN`'s `sync_all` drains the queue and parks on the last completion.

**C12 stays entirely with this spec** — log §12.4 confirms it, and the reason it
looked adjacent to the log is only that `log_file::Sink::append` was one of its
callers. Two things that section establishes belong here, and the second is an
obligation rather than a note.

**C12's surface has already shrunk by one caller, and C0 confirms it.**
`Vfs::flush_file` (`vfs.rs:538`) had exactly three callers on pre-log `main`; the
third was `log_file.rs:376`, the only one **not** reached from a syscall, and log
L6 deleted it with the file. On the merged tree it has **two**:
`OpenFileState::drop` (`kernel/src/object/file.rs:31`) and `object::ops::fsync`
(`object/ops.rs:668`) — so C12 opens on a queue with one class of producer instead
of two. §11.4 no longer owns anything here, and that is the whole of the
interaction between the two branches on this point.

**`SYS_FSYNC` now carries the device flush, and C0's question is answered: log L6
gave the mount sync to *every* caller.** The struck version of this paragraph
recorded that `fd::fsync` stopped one level short of what the kernel's own log
sink did, and asked C0 to record which of two forms L6 took. It took the first,
and said so in the code: `ops::fsync` (`object/ops.rs:654`) calls `flush_file`
(`:668`) and then `vfs.sync_for_path(&path)` (`:676`) **under the same
acquisition, deliberately** — two acquisitions would let this caller's file be
unmounted between them. `sync_for_path` (`vfs.rs:732`) resolves the mount and
calls `sync_mount` (`:701`) → `Fat32::sync`
(`toyos-fat32/src/fs.rs:901`) → **`self.dev.flush()`** (`:908`), SCSI SYNCHRONIZE
CACHE on a stick.

Its own doc states the reason and the rejected alternative
(`object/ops.rs:636-653`, `vfs.rs:715-731`): `/bin/logd` publishes
`LOG_DURABLE_NS` off this call's result and a panicking kernel stops waiting for
its own report when that word passes the report's timestamp, so an `fsync` that
stopped at the page cache would make the durability contract a claim about
nothing. **A second syscall for logd alone was considered and rejected** — it
needs a number, needs discussion, and would make every *other* `fsync` quietly
weaker than the one program that noticed. **So §14.2's arithmetic is unaffected:
no number was taken.** The granularity is the whole mount, because a cache flush
is per device; every `fsync` is slower for it and more honest.

**C12's job is therefore to not undo it.** *"`SYS_FSYNC` parks on a real
completion"* is still not the same promise as *"`SYS_FSYNC` is durable"*, and
C12 must not conflate them: parking on a write-back completion makes the answer
honest about the page cache, and the device flush is a separate step that must
survive the change. `log_is_durable_after_fsync` is the gate that catches a C12
which queues the write-back and drops the mount sync on the way.
`close-cannot-report-io-error` is about the error; this is about the guarantee.

### 13.1 The flush is not the only thing `Drop` does under that lock

`OpenFileState::drop` (`kernel/src/object/file.rs:27`) is two operations, and
the first draft moved one:

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
(`bcachefs_adapter.rs:194`) and FAT32 after its first flush (`vfs.rs:573`, "so its
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

**This inverts the meaning of `0` inside the kernel, at every blocking call site,
and the failure mode is silent.** `scheduler::block_on`'s contract today is the
opposite — `scheduler.rs:205`, "`deadline = 0` means no timeout" — and every
in-kernel caller relies on it. The sites are §4.1's park table, which is the
single enumeration both sections should read from: P1–P9 in `arch/syscall.rs`,
P10 in `io_uring.rs`, P11–P12 and the three internal ones in `scheduler.rs`, and
P13–P15 in `kernel/src/log/`. **Fifteen, not nine**, and naming them twice is how
the first draft's list went stale in two places at once. A site left passing `0`
after the change goes from "block forever" to "return immediately" — a busy loop,
not a compile error, and no test asserts on it.

**So `Deadline` must not be a `u64` newtype with a public constructor.** Make the
absolute form unconstructible from a bare integer: `Deadline::at(Instant)`,
`Deadline::never()`, `Deadline::passed()`. Then the nine sites do not compile
until each has been read, which is the whole point of §3.

Two live sentinel collisions the removal must resolve rather than inherit:

- `io_uring.rs:404-405` already carries a **third** value: it maps relative `0`
  (non-blocking) to absolute `1`, and `:458` maps `1` back to `0` (block forever).
  That line is unreachable only because `:426` returns first. It is exactly the
  latent trap this section exists to remove and it should be cited as the
  motivating example.
- **soundd's hack is `.max(1)`, not "sleep a full period".**
  `userland/soundd/src/main.rs:1203` is `((target - now) as u64).max(1)` with the
  comment "timeout 0 is the kernel's non-blocking sentinel" (`:1202`); `:1198`
  already picks the next future grid point, so the full-period half was fixed a
  generation ago. `soundd-past-due-wake-max-1` is the
  open entry for what is left, and it notes the `.max(1)` is survivable only
  *because* of `MIN_ONE_SHOT_NS` (§3.1). The deletion is right; the first draft's
  description of what is being deleted is stale.

### 14.2 Syscall numbers — the allocation that cannot collide

**Settled. The number is 115, it is already reserved in the ABI source by name,
and neither of this document's two guesses was right.**

The history, because the method is worth keeping even though the arithmetic is
spent. When this was written the highest number on `main` was 98 with 21 unusable
gaps; `specs/capability-endowment-spec.md` §3.1 was to take 99–112 and retire
thirteen more (26, 31, 36–39, 65, 68, 70, 76, 85, 87, 96), with a merge rule that
shifted its own block up if `main` moved under it; and
`specs/log-architecture-spec.md` was to take one too. Two free numbers, 114 and
115, and neither spec said which was which. The first draft wrote "expected 113,
and C0 asserts it rather than assuming it" — a literal one clause away from the
word "asserts" is what an implementer hard-codes, and 113 would have collided.
The second draft said *compute it and never write it down here*.

**Both branches have landed and the tree answers it.** The highest number is
**114** (`SYS_LOG_READ`), and `toyos-abi/src/syscall.rs:223-232` reserves two
numbers explicitly:

> Number 113 is **reserved, not free**: it is held for `SYS_PORT_REARM` … 115 is
> likewise held, for `SYS_SLEEP_UNTIL`, which replaces a retired `SYS_NANOSLEEP`.
> … **Both are recorded here rather than in those specs alone**, because this file
> is where an agent allocating a number looks and a reservation nobody reads is
> not a reservation.

**So C0 has nothing to compute; it consumes the existing reservation.** The
number is written down here *because the ABI writes it down* — a spec that hides
a number the source states is a second place to get it wrong. C0 asserts only
that 115 is still free and still carries that comment.

| new | number | replaces |
|---|---|---|
| `SYS_SLEEP_UNTIL` | **115**, reserved in `toyos-abi/src/syscall.rs` | `SYS_NANOSLEEP` (49), retired |

Everything else keeps its number with changed semantics: `SYS_IO_URING_ENTER`
(90) and `SYS_FUTEX_WAIT` (58) take absolute deadlines.

**One follow-up this branch deliberately does not do.** That reservation comment
cites `specs/plans/iouring-blocking-spec.md`, which this document's landing
deletes, so the citation goes stale the moment this lands. Re-pointing it at
`specs/completion-architecture-spec.md` is a one-line edit inside
`toyos-abi/src/`, which `src/pr.rs`'s `abi_lands_alone` treats as a sysroot
change and would refuse beside a doc-only branch. **It lands as its own tiny PR**,
the same shape the earlier fix to syscall 113's citation took, and it is C0's
first act rather than something this branch smuggles.

Two consequences of the one number, both already carried by the endowment branch:
`ProcessData`'s syscall profile array is sized from the ABI rather than at
`[u32; 64]` — its own comment records *"It was `[u32; 64]` while the ABI reached
98"* (`toyos-abi/src/syscall.rs:254`), and the issue that asked for it is closed
and its file deleted, so this document no longer cites it; and their
`retired_syscalls!` macro takes 49's gravestone as one more row.

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

**Two branches land before C0, not one.** `wt/toyos-logd` is between them under
the ruled order, so C0's merge brings in both and every row below must be
re-checked against a tree that also has the record ring, `klogd`, `/bin/logd` and
no `log_file.rs`. The rows that touch it are 12 (the `KernelPayload` retype,
still this branch's — see §10), 21 (the `system.toml` rewrites, to which log L6
adds a `logd` row in all six manifests and two new `ProgramConfig` keys) and 17
(the documentation budget, which log L8 also spends against). **Log's own §12.6
is the list of what it left undone here**, and it is the shorter document to read
first.

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
| 3 | **`Acceptor`/`Connector` and `PortShared`** (§1.2), which carry `acceptors: Arc<KWaitQueue>` and `io_uring_watchers: Lock<Vec<RingId>>` | `kernel/src/object/port.rs` is theirs to create; **C13** replaces those two fields with one watch list, exactly as it does for pipes and devices (the first draft said C3, which is the park site and does not touch `port.rs`). P6 parks there. Their `Acceptor::on_zero_handles` — set `closed`, drop the queue — becomes the canceller that posts `Outcome::Gone` to every parked acceptor, which is what makes their "the bound on failure is a process lifetime and nothing else" true of a *blocked* server too. **"The one file both branches write" is false** and is withdrawn: their chunk 2 also rewrote `io_uring.rs` (which §19 deletes from) and deleted `fd.rs` outright (which §9, §13 and §18 all cited). Three files, not one — **and all three landed**, so C13 opens on `object/port.rs`, `object/ops.rs` and `object/file.rs`. |
| 4 | ~~**No key at all, rather than a `Koid` key**~~ — **the premise is void; verified 2026-08-15** | The landed `Source` uses neither a `Koid` nor a `ListenerId` for ports: it holds the live object, `Source::Port(Arc<crate::object::port::PortShared>)` (`io_uring.rs:155`), with `PartialEq` by `Arc::ptr_eq`. `grep -n Koid kernel/src/io_uring.rs` returns nothing. So there is no `Koid` rename to follow and no residual key to remove — **C3's job for `Source::Port` is to remove an `Arc`-held reference, not a key**, which §5.3's borrowed `Subject` does by construction. The "internal contradiction to resolve" this row recorded — §21's `Source::Terminated(koid)` against §19's "delete `Source` outright" — dissolves with it: nothing survives, and `Source::Terminated(koid)` is struck. |
| 5 | **`SYS_LISTEN`/`SYS_CONNECT` retired, `SYS_ACCEPT`(86) takes an `Acceptor` and returns one handle** (§3.2, §3.3) | `kernel/src/listener.rs` is gone before C3 runs, so P6's "listener's completion" is the `Acceptor`'s. `listener::io_uring_watchers` and `wake_poll_waiters` are deleted by them, not by this spec — **removed from §19 so neither branch claims it twice.** |
| 6 | **`SYS_THREAD_JOIN`(41) is *kept* with its `Tid`** (§3.4, their deviation D5) — `capability-handles-spec.md`'s `SYS_THREAD_JOIN_H` does **not** happen | P8 therefore parks on the `ThreadObject` the `Tid` resolves to inside the caller's own process, not on a handle. `SYS_PROCESS_WAIT`(108) with `Rights::WAIT` is the handle-shaped one, and P7 uses it. `park_lot`, `PARK_BUCKETS` and `wake_task(TaskId)` are still deleted here. |
| 7 | **`SYS_OPEN_DEVICE`(31) retired; `SYS_DEVICE_CLAIM`(111) mints a claim, and only `/bin/init` holds `Rights::DEVICE`** (§1.2, §3.1) | P3/P4 park on the `DeviceClaim`. `DeviceClaim::on_zero_handles` releasing the class posts `Outcome::Gone(Revoked)` to anyone parked, so their §5.3 crash-release row gains liveness for a blocked reader for free. |
| 8 | **`SYS_IO_URING_SETUP`(89) returns `{ handle, vaddr }`; the ring owns its `PageAlloc` and the kernel maps it at setup** (§3.3, their chunk 6) | Exactly §5.2's second inbox. **They close `io-uring-abuses-shared-memory`, not this spec** — removed from §19. C11 adopts the ring as an `Inbox` and adds nothing to its allocation. The superseded spec's 32 KiB `RingArena` is dropped by both. |
| 9 | **`on_zero_handles` runs from a deferred per-CPU queue drained "at syscall exit, `do_schedule` entry and the idle loop"** (their §1.1; the first draft cited §5.2, which is *Backpressure*) | Three things. (a) ~~The third drain site goes~~ — **withdrawn: the landed code keeps all three deliberately and documents why.** `object::drain_zero_handles`'s doc reads *"Called at syscall exit, at the top of every scheduler pass and from the idle loop — the same three sites, and for the same reason, as the wake drains beside them"*, and the idle-loop call (`driver.rs:692`) carries its own rationale: *"`pass` below covers this too; it is here so a CPU that reaches the loop and then halts has run every hook first, rather than leaving one queued behind an interrupt that may be 102 s away."* That is the case this row's argument did not consider — subsumption by `pass` is about a CPU that keeps running, and the idle site is about one that stops. **This branch would have to actively remove an already-justified, already-shipped call**, so it does not: the recommendation is struck and all three sites stay. (b) C12 adds `FileObject::on_zero_handles → writeback::push`, because `Drop` cannot take a `&Parkable` (§13). Their spec has no hook *table* to add a row to — their §5.3 is a six-row teardown table with no `FileObject` row — so this is an extension, not an entry. C12 lands after their chunk 2. (c) **The general rule, which is new and binds their chunks 1 and 2**: none of the three drain sites has a `Parkable` (`do_schedule` entry provably does not, §6.1), so after C5 **no `on_zero_handles` hook may take a `SleepLock` at all** — the compiler refuses it. `FileObject → writeback::push` is the shape *every* hook needing the VFS must take, not a one-off. |
| 10 | **Their §1.1's closing rule: "The failing shape to check any new type against is `toyos-sched`'s `Registration`: a guard that lives on the victim's own stack and is therefore never dropped when another CPU kills it. No object introduced below places a release obligation on a blocked thread's stack."** | §7 **fixes `Registration` itself** — but by §7.2's rewrite of `handle_retire`'s two reap-in-place arms, not by `Commit::Killed`, which the first draft named and which is the wrong path (`commit()` already dequeues). The victim runs again on its own stack and drops the guard. So their rule stops being a constraint they must design around and becomes a property the kernel has. **They should not relax it until C4 has actually landed and its `toyos-sched` tests are green**: it costs them nothing to keep, and until then it is still true. `retired-thread-leaks-wait-queue-node` is closed by C4, and it is this spec's to close. |
| 11 | **Their §1.1: an `Arc` cloned before blocking "is stranded on a freed kernel stack … leaks memory, bounded and census-visible"** | Same mechanism as row 10 retires the leak class outright. `capability-handles-spec.md` §13 said the structural fix was "Phase 2 try-once syscalls"; it is not — it is the cancellable park, which keeps one-syscall blocking I/O (§14.3). **Their census baseline assertions should tighten once C4 lands.** |
| 12 | **`KernelPayload.address_space: Option<PageTables>` → non-`Option`** (`capability-handles-spec.md` §9.4's one surviving retype) | §10: a kernel thread's `ProcessObject` names the **kernel** address space. A kernel thread naming *no* address space would have forced that field to stay an `Option` forever, so the retype is *enabled* by this one. **The endowment spec does not claim it**: `KernelPayload` appears nowhere in it, and its §1.3 lists `AddressSpaceObject` only as a `KObjectRef` variant adopted "with no change of shape" — a different type from `payload.rs:88`'s field. So this row is a contact point with `capability-handles-spec.md`, not with the endowment branch, and nobody currently owns doing it. **C6 does it**, since C6 is what makes it possible. |
| 13 | **Bad-handle policy flips to kill-the-process** (their chunk 7) | §7 makes that kill safe from a thread parked anywhere, including inside a sleep-locked critical section. Their chunk 7 flips it before C4 lands, so between the two landings a killed handle-abuser can still be killed at a park under the *old* locks — which is today's behaviour and no worse. |
| 14 | **Their gates `kill_while_blocked` and `device_claim_crash_release`** (their chunk 6) | Both are strengthened rather than changed: after C4 the killed client's stack is unwound by returning, so the census returns to baseline for a reason stronger than the handle drain. Do not weaken either to accommodate this spec. |
| 15 | **The SDK** (§6.5) | Disjoint in *intent*: they change what an argument names, this changes what a timeout means (§14.3 keeps the blocking ABI shape). `toyos/src/services.rs` and `toyos/src/pipe.rs` are deleted by them; `Poller` is replaced by `toyos::ring::Ring` in C11 either way. **Not disjoint in *files*** — see row 18. |
| 16 | **The `Abi-Inseparable` trailer and the shared sysroot** (their §9, §10.1) | They hold the sysroot claim for their branch's whole life. This branch then claims it in turn — **at C0, not C1** (§21's C0 row is right; the first draft's "C1 onward" here was not), because C0 is where the merge and the re-check happen and the claim must precede the first build. One trailer on one commit is sufficient: `src/pr.rs:319`'s `abi_lands_alone` exempts the whole branch if **any** commit declares it, so §21's single-commit form satisfies the gate. **Their §10.1's own recommendation is to land chunks 0+1 as a separate earlier PR** — see row 19, because "post-endowment" then means two different trees. |
| 17 | **Root `CLAUDE.md` headroom** (their chunk 9 measured 2,678 bytes spare against 37,322) | **Both budgets are gone with `src/docs.rs`** (deleted by owner ruling in `8d0db10`, "no tests over documentation"), so neither the 40,000 per-file cap nor the 80,000 `TOTAL_BUDGET` is enforced by anything today. The pressure they existed for went with the compaction rather than with the test: `main`'s root `CLAUDE.md` is **13,163** bytes and the five files together are **28,831** — a third of what the struck cap allowed, and **28,824** after this branch's edit. This branch's own edit is **one line**, replacing the bullet that named the two superseded plans. **The row is kept as the record that the arbiter is gone**, so a later agent does not go looking for a budget test that does not exist and conclude the budget was raised. |
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
| the cancel model — **in `toyos-sched/loom/tests/loom_retire.rs`**, not a new file | kill racing a park racing a post: cancel before arm, between arm and commit, and after `Blocked` | the interleaving needs a remote CPU acting between two of the victim's instructions |
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
- **The cancel model is a `toyos-sched` model, not a `kernel-loom` one — and it
  extends an existing file rather than opening a new one.** `Commit`, the
  rendezvous CAS and — after §7.2 — `handle_retire`'s arms all live in
  `toyos-sched/`, whose `loom/tests/` already holds four models
  (`loom_mailbox.rs`, `loom_retire.rs`, `loom_sleep.rs`, `loom_ticket.rs`).
  **`loom_retire.rs` already covers three of the four orderings** — kill vs
  wake-claim, kill vs park-commit
  (`a_retire_racing_the_park_commit_always_leaves_someone_to_reap`, `:104`),
  chase vs migration (`:157`) and adopt-under-kill (`:199`). The genuinely
  missing case is exactly the one §7.3 names: **retire finding the task already in
  `parked`.** Extend that file; a fifth would split the estate that already holds
  the argument.

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

   | reach | lock | on pre-log `main` |
   |---|---|---|
   | `dump.rs` → `process.rs:727` | `process::PROCESS_TABLE` | `Lock<Option<ProcessTable>>`, already `try_lock` |
   | ~~`log!` → `log_ring.rs:314`~~ | ~~`RingGuard`~~ | **gone before C0** — log L3 deletes `log_ring.rs` and the record ring's producer path takes no lock at all (log §2.3, log §7) |
   | `dump.rs:235` → `panic_console::paint_report` | `SEQ` seqlock + `PAINTING` | deliberately not a `Lock` |

   `process::ProcessData` (`process.rs:538`, held as `Arc<Lock<ProcessData>>`) is a
   **different** lock from `process::PROCESS_TABLE` (`process.rs:727`), and
   thread names come from the
   latter. So **C0 finds two rows, not three**, and after all four conversions
   the one lock that can refuse the dump is still a raw ticket `Lock` with no
   holder, which the dump already asks with `try_lock`. **`SleepLock::holder()`
   buys the dump nothing** — the conclusion is unchanged and the log branch is
   what removed the row this section said could not be converted.

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

The panic path keeps every spin it has (§4.5). `apic.rs:239`'s 500 ms wait for the
log file to drain before power-off, inside `wait_for_log_file` (`:190`), **is not
a `Tripwire`** — `apic.rs:236` logs "the panel is the only copy" and *returns*,
deliberately, because a second panic on a machine already going down loses the
report (§3.2). It is a `Bound` whose
expiry is a named refusal. It cannot become a completion either way, because the
thread that would post it is not going to run again.

Gates that must stay green throughout: `blocked_dump`, `screen_blocked_dump`,
`dump_nmi_probe`, `screen_panic_muted`, `disk_backtrace`, `fault_gates`,
`fpu_isolation`.

---

## 18. Migration ledger

**Re-stamped 2026-08-15 at `71a0559`, on the merged tree.** Every row below moved
— several by a lot, and two in opposite directions — which is the ledger's own
argument for stating its command rather than its number.

**The hedge the first draft put on two rows applies to all of them, and C0 still
re-derives.** These are the numbers the plan is written against, not numbers C1 or
C5 may work from: the tree moves under this branch as it moved under the other
two. Three of the first draft's caveats are now spent rather than pending — P6's
`1246-1344` span is deleted and `listener.rs` with it, P7's `sys_waitpid` is
retired and replaced by `sys_process_wait`, and `with_fd_owner_data` **survived**
the endowment branch under its own name, in `process.rs`, `io_uring.rs`,
`object/ops.rs` and `arch/syscall.rs`.

| what | count | command | disposition |
|---|---|---|---|
| `core::hint::spin_loop();` in `kernel/src/` | **41** | `rg -n "core::hint::spin_loop\(\);" kernel/src/` | 4 deleted (§4.2), 14 become `Poll` (§4.3), 23 stay and are gated to a **site** allow-list: 15 Class R, 5 Class X, 2 Class L, 1 Class B (§4.4–§4.6) |
| …of those, under `kernel/src/drivers/` | **22** | the same, scoped to `drivers/` | |
| `spin_loop` textual lines | **43** | `rg -n 'spin_loop' kernel/src/` | 41 calls + 2 doc-comment mentions; §4.6's gate strips comments |
| bare `while … {}` loops | **5** | `rg -n 'while .*\{\s*\}\s*$' kernel/src/` | 3 real waits, 2 draining loops the gate must not red on (§4.6) |
| `scheduler::wait_until` call sites | **7** | `rg -n 'wait_until' kernel/src/` | all in `arch/syscall.rs`; all → `completion::wait` |
| `scheduler::prepare_wait` call sites | **9** | `rg -n 'prepare_wait' kernel/src/` | 3 internal to `scheduler.rs`; all → `completion::arm` |
| `scheduler::block_on` call sites | **9** | `rg -n 'block_on' kernel/src/` | 3 internal to `scheduler.rs`; all → `completion::wait` |
| `io_uring::complete_pending_for_event` call sites | **11** | `rg -n 'complete_pending_for_event' kernel/src/` | 10 hand-paired with a queue wake, 1 the log's batched post (§5.6); all → one `post` on a watch list |
| `IO_URING_WATCHERS` stores | **8** | `rg -n 'IO_URING_WATCHERS\|io_uring_watchers' kernel/src/` | 6 statics + 2 per-instance fields; §19 |
| `.lock()` calls in `kernel/src/` | **259** | `rg -n '\.lock\(\)' kernel/src/` | 4 statics convert (§9); the sites under them take `&Parkable` |
| `.try_lock()` calls | **10** | `rg -n '\.try_lock\(\)' kernel/src/` | unchanged in meaning; two more appear (`poll_if_pending`, boot's VFS) |
| statics holding a `Lock` | **45** | `rg -n 'static [A-Z_0-9]+: *Lock<' kernel/src/` gives **42** | plus `VOLUMES`, an *array* of two `Lock`s the regex cannot see (`fat32_adapter.rs:316`), plus the two written-out `crate::sync::Lock<` statics (`arch/syscall.rs:83`, `arch/percpu.rs:443`). **The number moves with the regex** — five readings of this tree have given 48, 49, 51, 52 and 45 — so the ledger states the command, and only the 4 that convert matter. Two further `crate::sync::Lock<` occurrences (`object/device.rs:102`, `:233`) are struct **fields**, not statics, and are not counted |
| `vfs::lock()` / `vfs::try_lock()` textual sites | **29** | `rg -n 'vfs::lock\(\)\|vfs::try_lock\(\)' kernel/src/` | split boot from task; 2 doors keep the choke point |
| `with_fd_owner_data` sites | **68** | `rg -n 'with_fd_owner_data' kernel/src/` | take `&Parkable` where they can reach a flush |
| kernel `.rs` files | **136** | `find kernel/src -name '*.rs'` | |

**Two rows moved in opposite directions and that is the interesting part**: 244
`.lock()` call sites became 259 while the statics under them fell from 49 to 42 by
the same regex — more call sites over fewer locks. C8's blast radius grew; §9's
conversion surface did not.

**The 29 VFS sites and the 68 `ProcessData` sites are the blast radius, and it is
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

**Code deleted, by name.** `sched/driver.rs`: **two** of the four pre-`hlt`
conditions — `i8042::verdict_due` (`:534`) and `xhci::port_work_pending` (`:564`)
— and `poll_if_pending` from `drain_irqs`.
`scheduler.rs`: `wait_until` (`:249`), `prepare_wait` (`:196`), `block_on`
(`:207`), `wake_task` (`:321`), `wake_pipe_readers` (`:343`),
`wake_pipe_writers` (`:355`), `park_lot` (`:265`). `sched/waitqs.rs`:
`PARK_BUCKETS` (`:36`), `park_lot` (`:52`). **`futex_wake`'s generation protocol
is struck from this list: it is already gone**, deleted independently of this
branch in `ba76478`, and `FUTEX_WAKE_GEN` appears nowhere in `kernel/src`.
`io_uring.rs`: `Source`, `Source::is_ready`, `complete_pending_for_event`,
`complete_pending_for_source`. `xhci/wait/mod.rs`:
`wait_transfer`, `wait_command`. `nvme.rs`: `wait_completion`'s spin.
`virtio.rs`: `submit_and_wait`'s spin.

**Eight watcher stores, not six, and the first draft both double-counted one and
missed another.** Re-enumerated 2026-08-15:

| store | where | kind |
|---|---|---|
| `IO_URING_WATCHERS` | `kernel/src/keyboard.rs:17` | static |
| `IO_URING_WATCHERS` | `kernel/src/mouse.rs:19` | static |
| `IO_URING_WATCHERS` | `kernel/src/net.rs:41` | static |
| `IO_URING_WATCHERS` | `kernel/src/drivers/hda.rs:220` | static |
| `IO_URING_WATCHERS` | `kernel/src/drivers/virtio_sound.rs:270` | static |
| `IO_URING_WATCHERS` | `kernel/src/log/user.rs:37` | static — **the sixth**, log L4's |
| `PortShared.io_uring_watchers` | `kernel/src/object/port.rs:74` | per-instance `Lock<Vec<RingId>>` |
| `Pipe.io_uring_watchers` | `kernel/src/pipe.rs:142` | per-instance `Vec<RingId>` |

`PortShared`'s is **not** a static and is not "the sixth" — `log/user.rs`'s is,
and §11 names it separately, so the two descriptions collided. And
**`pipe.rs:142` was named nowhere in this ledger**, which is the one store §5.6's
*"a ring and a thread are two entries on one watch list"* conversion most
obviously has to fold: it is the only one already living on the object, which is
the shape everything else is being moved to.

**Struck from this ledger, 2026-08-09** — `specs/log-architecture-spec.md` §12.1
takes them and its §8.1 is where they are now listed by name:
`flush_log_file_if_affordable`, `LOG_DEFERRAL_CEILING_NS`, `LOG_DEFERRED_SINCE`,
`log_file_flush_due` and `owes_wake` (log L6); `drain_serial` on the idle path
and the pre-`hlt` condition `log_file_flush_due` (`:552`) (log L3 and L6); and
`log_file.rs`'s `SINK` (log L6). **Neither `log_health` nor `reap_poisoned` is
deleted at all** — the struck version of this line said both leave the idle loop
and §11 now says neither does.

**~~One name may come back, and §11.4 is where that is decided.~~ Void.** Under
the ruled order log L6 lands first, so `flush_log_file_if_affordable`, its four
names and `log_file::SINK`'s `Lock` are all gone before C0 and none of them can
return to this ledger.

**~~One name joins it instead, and it is the log branch's fallback.~~ Also void,
and there is nothing to delete.** The struck version had log L3 re-pointing the
pre-`hlt` condition into `log::pending()` for this branch to remove. It did not:
both log conditions were deleted outright, and **no caller of any log pending
predicate exists anywhere in the tree**. What the log branch did leave is a dead
function — `log::console::pending()` (`kernel/src/log/console.rs:251`,
`serial::has_console() && DRAINED.any_pending()`), zero callers repo-wide. **The
ledger entry is therefore "delete the function, not a condition"**, and the
C3+C4 deliverable "the `log::pending()` pre-`hlt` condition is deleted with them"
is struck from §21.

`Source::Log` and its `IO_URING_WATCHERS` static (`log/user.rs:37`) **do** join
the ledger, as the sixth static above. The two genuine log fallbacks — `klogd`'s
park becoming an `Inbox` park, and `emit`'s relaxed signal becoming the degenerate
one-waiter post — are unaffected by any of this.

**Already deleted, by the endowment branch, and confirmed gone 2026-08-15**:
`kernel/src/listener.rs` whole, and `wake_poll_waiters` — `ls kernel/src/listener.rs`
is "No such file" and `rg wake_poll_waiters kernel/src/` is empty. `PendingPoll`
survives (`io_uring.rs:193`) and still carries its own `fd_num: u32` field, which
no spec ever named; **C3 owns it**, and the question "ask them" is spent because
they have landed.

**`specs/issues/` files closed.** Slugs only, deliberately — but **the gate that
made this a mechanical requirement no longer exists.** `src/docs.rs` and
`every_named_issue_file_resolves` were deleted with the doc tests (`8d0db10`,
owner ruling); the name now occurs only as prose in two spec documents, and
nothing in `src/` resolves a `specs/issues/<area>/<slug>.md` path. **So a stale
full-path citation is invisible to `cargo test` today**, which makes the
convention a discipline rather than a check. The convention stands regardless: a
slug survives its file moving, a path does not. **Whether to rebuild the check is
C13's to put to the owner**, alongside `no_spin_outside_the_allow_list`, which
lands in `src/sourcegate.rs` and is the natural host for it.

**C13 still de-paths the citations, and the reason is now hygiene rather than a
red.** Five of the twelve slugs are written as full paths in this very document —
§1 (`disk-wait-pins-a-cpu`), §4.3 (`driver-waits-without-a-deadline`), §5.6
(`io-uring-source-half-a-wake-pair`), §7.5 (`retired-thread-leaks-wait-queue-node`)
and §13 (`cache-eviction-wedges-an-idle-cpu`) — and several are cited by full path
from files that are not their own entry: the root `CLAUDE.md`,
`specs/plans/metal-boot-plan.md`, `specs/reference/metal-hardware-inventory.md`
and `specs/issues/audio/doom-audio-callback-stalled-on-the-t14.md` among them.
The `specs/issues/README.md` protocol says the durable rule moves into the spec
that owns the subject; doing that is what removes the citation, so it is the same
edit. **Nothing fails the build if it is missed**, which is why it needs to be a
chunk's stated deliverable rather than something the gate catches.

**Three slugs were said to have left this ledger with the log strike, and all
three files are still open on `main`.** `client-cpu-takes-the-log-flush` (audio),
`log-flush-is-unbounded` (boot-media) and `pre-idle-wedge-says-nothing`
(diagnostics) each still carry `status: open` frontmatter, and each still
describes the pre-rewrite subsystem — `log_file`, `log_ring`, `owes_wake`,
`LOG_DEFERRAL_CEILING_NS`, `drain_serial`, `wait_transfer` spins — none of which
exists in `kernel/src` any more. Neither the log branch nor anything since closed
or deleted them; `git log` on all three shows their last touch was the specs
taxonomy reorganisation (`4e690d0`), not a resolution.

**So the transfer was written down on one side only, and the obligation is real
in a different form than this section assigned it.** The entries are stale
descriptions of deleted code, which is worse than an open issue: an agent reading
`log-flush-is-unbounded` today goes looking for a file sink that does not exist.
**C0 puts all three to the owner** — closed by the log rewrite, or re-written
against the tree that replaced it — rather than assuming the first.

**~~And the order ruling turns their citations into a red on this branch, at
C0.~~ Void twice over**: the files were never deleted, and the gate that would
have red on them no longer exists. §1.3's citation of
`client-cpu-takes-the-log-flush` and `specs/plans/introspection-plan.md`'s two of
`log-flush-is-unbounded` (`:31`, `:56` — verified unmoved) all still resolve.
**What survives is the substance rather than the deadline**: those citations point
at entries describing code that is gone, so they mislead without failing
anything. That is C0's item above, and it is a judgement to put to the owner, not
a build error to race.

| slug | area | closed by | note |
|---|---|---|---|
| `disk-wait-pins-a-cpu` | audio | C7+C8 | the headline |
| `cache-eviction-wedges-an-idle-cpu` | boot-media | C13 | the idle CPU no longer reaches a block device; **verify the `rip` first** — that entry says symbolization was never done |
| `xhci-waits-are-spins` | hardware | C7 | EP0 recovery's `Poll` is the declared residual (§12.3) |
| `scheduler-pass-blocks-in-xhci` | kernel | C7 | **its second half is spent** — `sched-check` *is* turned on, as `sched_check_build` (§20.4); what is left for C15 is the window, not the switch |
| `hotplug-blocks-a-scheduler-pass` | hardware | C7 | |
| `driver-waits-without-a-deadline` | kernel | C10 | `CAP.TO` included |
| `io-uring-source-half-a-wake-pair` | kernel | C3 | one post, no pair to halve |
| `panic-on-wedged-virtio-console-spins` | panic-path | C10 | `submit_and_wait` gets a `Bound` |
| `retired-thread-leaks-wait-queue-node` | kernel | C3+C4 | §7.5's consequence 1 — and by §7.2's retire arms, not by `Commit::Killed` |
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
guest-slot consumers against `buildlock::HOST_GUESTS`'s twelve, which is what
makes the interleave affordable rather than serial — and "two consumers" is
literal: gate A's thorough tier takes exactly one slot for the whole tier
(`let _slot = slots.take("gate A, thorough")`, `tests/toyos.rs:12741`), so this
protocol is two arm processes against twelve slots.

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

`io-depth-probe` must report **1 from a syscall and 0 from `iod`** (§9.1),
against a C0 baseline this branch measures rather than inherits. **The 4 from the
idle loop is gone**, confirmed: `log_file.rs` no longer exists, the recorded
depth-4 trace was `log_file::poll → Sink::flush → vfs::flush_file →
write_blocks`, and no log condition survives in the pre-`hlt` set. So this
branch's target is the syscall arm and `iod`'s, and neither §11.4 nor C9 has an
intermediate reading to attribute.

**One residual keeps the C0 measurement honest.** The probe fires in
`wait_transfer` for *any* transfer, and `drain_irqs` still calls
`xhci::poll_if_pending()` (`sched/driver.rs:591`), so a hot-plug enumeration can
still print an `io-depth: … task None` line from an idle CPU with no block device
involved — the probe's text says "a disk transfer" for a control transfer too.
C0 records that arm separately or the baseline reads as a regression it is not.
**Half of the headline moved to the other branch with the work**: log L6 owns the
idle-loop half of the `--slow-usb` improvement and records it at its L9; C7+C8
owns the syscall half. C14 must say which arm it moved, or the two branches will
both claim the same microseconds.

**And a positive assertion on the log's content, in the same session.** `/log` is a
USB volume in every profile, so the *cheapest* way to make the wake number good
is for the log to stop being written — which is exactly what §12.3's unbounded
park does to `/bin/logd`. **The headline number and the worst failure mode
produce the same reading**, and none of §20.3's negative controls separates them.
So C14 also asserts, host-side on the volume, that this boot's log file holds the
lines it should. `kernel_log_file` (`tests/common/volumes.rs:485`) is that
assertion, and **it is pre-satisfied**: log L6 re-pointed it, and its own header
now proves *"a userland process holding `logread` reads a cursor, renders,
writes, `fsync`s and keeps up"*, asserting mid-run, post-shutdown and rotation.
C14 inherits it rather than building it, and keeps it green — a `/log` that stops
because logd parked forever reds it. Without it the headline is unfalsifiable.

**It cannot literally be the same run, and C14 must say so.** `--audio-gate`
dispatches `AUDIO_TESTS` only and returns (`tests/toyos.rs:12714-12752`), and
`kernel_log_file` is registered `Tier::Nightly` (`tests/toyos.rs:654`) and
`Relegated { why: Why::Cost, ci_ms: 43_056 }` (`src/tiers.rs:480`), so it is not
even in the PR gate. C14 invokes it as a separate command **in the same session,
on the same tree, recorded beside the A/B**.

### 20.3 Negative controls — each must red on a tree that has the defect

| feature | what it reintroduces | what must go red |
|---|---|---|
| ~~`reintroduce-idle-flush`~~ | ~~`log_file::poll()` back on the idle loop~~ — **retired, see below** | — |
| `sleeplock-spins` | `SleepLock::lock` spins instead of parking | `io-depth-probe`'s depth, and the `--slow-usb` A/B |
| `park-holding-a-spinlock` | one converted path keeps its raw `Lock` | a named panic — **but say which**: `Parkable::of_current()` at the leaf trips RT1 and names the token; a token threaded from the trap entry reaches `kernel/src/scheduler.rs:44`'s `assert_baseline`, which asserts `preempt::count() == baseline` and has seven call sites. Pin the one the control stages |
| `drop-a-completion` | one `post` writes the record and does not claim | **not a hang** — see below |

Each carries a comment saying why nothing else can reach it, per the harness's own
rule. **A feature that replaces only a verdict makes its own gate vacuous.**

**And none of them costs a kernel build, which is the opposite of what the first
draft said.** `INERT_ACTUATORS` exists nowhere in the tree — the name occurs once,
as prose in `specs/log-architecture-spec.md` — and the folding it referred to,
`qemu::fold_inert`, was deleted on 2026-08-10. **Actuators are runtime now**: one
test kernel (`boot-actuators,test-actuators`) carries all of them and arms
whichever `\toyos\cmdline` names, validated against `declared_actuators`
(`tests/common/qemu.rs:1518-1528`). So a behavioural control like
`sleeplock-spins` or `drop-a-completion` is **a new row in
`kernel/src/actuator.rs`'s `actuators!` macro** (`:64`) with its
`specs/device-test-strategy.md` claim comment, costs no new kernel build, and adds
no row to `TEST_SUITE_KERNEL_BUILDS` (`src/build.rs:791`, four entries). If a
control genuinely cannot be a runtime branch, it is a fifth entry there **with the
argument for why** — the bar `fpu-save-nothing` and `sched-check` meet and nothing
else does.

**`reintroduce-idle-flush` is retired by the order ruling, and the loss is
named.** The struck version of this paragraph moved it from C9 to C7+C8 and
concluded that *"whichever branch lands second inherits both; neither retires the
other"*, the other being `specs/log-architecture-spec.md` §9.4's
`log-writes-the-file`. **Under `log → completions` the second branch cannot build
it**: after log L6 there is no `log_file::poll` to put back and no kernel file
path for it to sit on, so the feature has no code to stage. `log-writes-the-file`
is not a replacement — it reds on a tree where the kernel writes a file at all,
where this one red on a tree where the flush is on the wrong *context* — so the
tree ends up with one shade of coverage instead of two. **That is a real cost of
the ordering, it is small against a compilation blocker, and it is recorded so
nobody re-derives the feature at C14 and finds nothing to attach it to.**

**`drop-a-completion` cannot have a hang as its verdict.** A hung guest does red,
and it reds as `STALL` — which *is* red — but the harness prints "the guard
expired, so this says nothing about the tree" beside it (`tests/toyos.rs:12132`,
verbatim) and tells nobody to bisect it. The rule is `tests/CLAUDE.md:8`'s, not
root `CLAUDE.md`'s: *"What a duration still decides prints `STALL` — red, and
named apart so nobody bisects it."* The objection is not that a STALL passes; it
is that nobody bisects one. A control whose entire signal is the one class the
suite names apart is not a control.
**`blocking_read_stress` therefore asserts a *count* of completed round trips
inside an `await_guest` bound**, so a dropped completion reds as a number, and the
control reds on the number.

**`sleeplock-spins` cannot red at C5, where §21 asks it to.** C5 lands `SleepLock`
with *nothing converted*, so no `SleepLock` is on the disk path, `io-depth-probe`
reads **whatever C0 measured** — unmoved in both arms — and the `--slow-usb` A/B
does not move. Either the control's gate moves to **C7** (its first real consumer)
or C5's gate is "the feature exists and its own unit test shows the spin", which
is weaker and should be labelled as such.

(The first draft said the probe "reads the same 5 and 4 in both arms". Both
numbers were produced by frames inside `log_file.rs` — 4 from `log_file::poll` on
the idle loop, 5 from `log_file::flush_final` under `syscall_handler` — and
neither reading is inherited now that the file is gone. That is precisely why
§20.2 demands a C0 baseline instead of a quoted pair.)

### 20.4 New named tests

- `blocking_read_stress` — cross-CPU pipe ping-pong, hard wall-clock bound. The
  lost-wake canary, with §20.3's counting assertion. Nothing to build against yet
  and nothing stale: the name existed only in the superseded
  `iouring-blocking-spec.md`, and `await_guest` is a real harness primitive with
  existing callers (`tests/toyos.rs:3135`, `:4373`, `tests/common/audio.rs:846`,
  `tests/common/faults.rs:232`).
- `cancel_while_parked` — kill a thread parked on a disk transfer under
  `usb-slow-device` (`kernel/src/actuator.rs:187`, which holds every mass-storage
  bulk completion back 2 ms and is exactly the machine this needs); the process exits, the lock is free (`SleepLock::holder()` is
  `None`), and a second process reads the same file. **It cannot run at C4**, where
  §21 lists it: `SleepLock` does not exist until C5 and there is no park on a disk
  transfer until C7, because `wait_transfer` still spins. C4's gate is the *return
  path* only — kill a thread parked on a pipe and assert it exits through its own
  stack — and this test moves to **C7**.
- `killed_holder_releases` — kill a thread holding the VFS sleep lock; the machine
  keeps mounting. **Pick a distinct name**: `killed_holder_releases_the_lock`
  already exists as a host-side buildlock test (`src/buildlock.rs:951`), and two
  near-identical names in one failure output is a bisect nobody needs.
- `no_spin_outside_the_allow_list` — the §4.6 gate, host-side, seconds. **Its
  host is `src/sourcegate.rs`** (§4.6): `src/docs.rs` was deleted by owner ruling
  and is not available to any gate this branch writes.
- `idle_loop_is_the_declared_body` — **un-struck: it was assigned to log L6 and
  was never built.** The name occurs seven times in the repo and every one is
  prose — six in `specs/log-architecture-spec.md`, including its own ledger row,
  and one comment at `kernel/src/sched/driver.rs:556`. There is no such test.
  **So this branch builds it**, as a `src/sourcegate.rs` declared-set test over
  the pre-`hlt` disjunction, and **amends it once**: C9 removes
  `i8042::verdict_due` and `xhci::port_work_pending`. The third amendment the
  struck version listed — the wake conversion removing `log::pending()` — is
  dropped, because the log branch left no log condition (§11). The omission is
  also worth filing against the log branch either way, since its own §12.1 lists
  the gate as delivered. C9's behavioural gate is still the two `sched: cpu=`
  tests (`tests/toyos.rs:8974`, `:9478`, counting the line emitted at
  `kernel/src/scheduler.rs:634`), which stay live precisely because `log_health`
  does not move.
- **`exit_wait_storm` — new, and it closes the one uncovered park class.** §4.1
  collapses P7 (child exit) and P8 (thread exit) onto parks on the
  `ProcessObject`/`ThreadObject`, and **no gate in §20.3 or §20.4 exercises
  either**: `blocking_read_stress` is pipes, `cancel_while_parked` and
  `killed_holder_releases` are disk and VFS. The tree's existing coverage is
  ordering, not volume — `process_lifecycle` is 295 lines with one arm on the wake
  (`an_unrelated_wake_does_not_end_the_wait`) and `std_threading` is 41. **N
  processes each spawning M children that exit while the parent is parked in
  `SYS_PROCESS_WAIT`, plus N threads joined in a `SYS_THREAD_JOIN` fan-in,
  asserting a count of collected exit codes inside an `await_guest` bound** — the
  same shape as `drop-a-completion`'s count, so a lost publish reds as a number
  and never as a `STALL`. `drop-a-completion` is its negative control, since it
  reaches the publish path too.
- **`every_park_declares_its_end` — new, and it is the only form that catches the
  §12.3 hazard.** §12.3 establishes that `iod` parked on a hung stick stops every
  write-back in the machine with *"nothing able to end it"*, and §22 marks that
  row still unresolved with **no second line at all**. Nothing in §20 reds on it:
  `kernel_log_file` observes `/bin/logd` and not `iod`, and is nightly anyway.
  **A host-side gate in `src/sourcegate.rs`, sibling to
  `no_spin_outside_the_allow_list`, over every `completion::wait`/`Parkable`
  construction site**: each must carry a `Bound`, `Cadence`, `Tripwire` or a named
  canceller, and a site with none is on an allow-list whose entry states which of
  §12.3's cancellers ends it. Cheap, no guest. **It has to be static**: a
  behavioural test cannot distinguish "parked forever" from "slow" without
  becoming the `STALL` §20.3 already disqualifies as a verdict.
- **`sched-check` is its own chunk, and the reason has changed.**
  `scheduler-pass-blocks-in-xhci` records that invariant P *"has never executed
  against the kernel in any image or any test run"*. **That sentence is stale at
  its source and so is the audit's**: invariant P has executed, has a baseline,
  and is already a gate. `sched_check_build` (`tests/toyos.rs:475`, `Tier::Fast`;
  body at `:8170`) boots `SCHED_CHECK_KERNEL` and runs `sched_stress` under it —
  green on CI's KVM shards, twelve of twelve, measured 5,879 ms (run
  31875856466) — and red on the dev host at 1,684,167 ns then 1,749,243 ns,
  adjudicated 2026-08-15 as cross-arch TCG and scoped to `Instrument::DevHostAlone`
  (`src/redlist.rs:884-903`,
  `specs/issues/kernel/invariant-p-cannot-hold-under-cross-arch-tcg.md`).
  `specs/assessments/test-cost-audit.md:1022`'s *"No test uses it"* is stale with
  it, and `sched-check` is one of `TEST_SUITE_KERNEL_BUILDS`'s four entries.

  **What survives, and it is the whole of the real point: the window still starts
  after `drain_irqs`** (`cpu.rs:1092` `check_pass_duration`, `MAX_PASS_NS` 200 µs
  at `:703`; `drain_irqs()` precedes `SchedPass::begin` at `driver.rs:354`/`:362`
  and `:461`/`:464`). **Widening it is the change**, and what it would newly cover
  is `xhci::poll_if_pending` (`driver.rs:591` — the two-second recovery path the
  companion issue is entirely about, and the largest term the widened window would
  swallow), `i8042::service` (`:594`), the dump's `serve_if_owed` (`:603`),
  `hold_report`'s 128 probes (`:606`, `PROBES` at `panic_console/mod.rs:1014`) and
  every `irq_ring` post (`:608-635`) — none of which this refactor touches, all of
  which must then fit 200 µs. **C15 takes its own baseline for the widened window
  before turning it on**, which is a narrower job than the struck version's.

---

## 21. Work breakdown

**Fourteen** chunks on `wt/toyos-compl` — the first draft had fifteen; C3+C4 and
C7+C8 are each one chunk because neither half can be green alone (§21.1, and
C3+C4's reason in the table), and `sched-check` is split back out as C15, which
the count of "thirteen" forgot to add. The table below is the arbiter and it has
fourteen rows. **Every chunk builds, boots, and passes `cargo test`** — plus
`cargo test` inside `toyos-sched/`, `toyos-xhci/` and `kernel-loom/` where it
touches them. No intermediate landing; one PR at the end, subject to §21.2's
fallback.

**Merge cadence.** `git merge --no-ff origin/main` at the start of C0 and at every
chunk boundary that follows a landing on `main`, and at minimum once a week.
**Never rebase, never amend** — a branch is merged by hash. **Two branches land
before C0** — the endowment branch and then `wt/toyos-logd` — and C0 is the merge
that brings both in.

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
| C0 | merge `origin/main` (**post-endowment and post-log**) — **done 2026-08-15 at `71a0559`, and this document is the result**: every §15 row re-checked, §18's counts and §4's enumeration re-stamped, §14.2's number found already reserved at 115, §13's `SYS_FSYNC` question answered (L6 gave the mount sync to every caller). **What C0 still owes**: the `toyos-abi` citation re-point as its own tiny PR (§14.2); putting the three still-open log issue files to the owner (§19); re-running §3.4's duration sweep; and claiming the sysroot | baseline `io-depth-probe` + `--slow-usb` A/B recorded in this spec, with the `poll_if_pending` arm recorded separately (§20.2) | suite green; §15 has no moved row |
| C1 | the six duration kinds (§3, §3.1, §3.3); `Instant`/`Duration`; `Deadline::{at,never,passed}`; `Parkable` | **every one of the 41 production durations has a kind or a named exception** (§3.4) — not "the kinds exist" | no behaviour change; `MIN_ONE_SHOT_NS` still compiles |
| C2 | `kernel/src/completion/`: `Record`, `Outcome`, `Inbox`, `Subject`, `arm`, `post`. Wired **behind** the existing waitq — every wake also posts | behaviour-preserving | `kernel-loom/tests/inbox.rs` |
| **C3+C4 — one chunk, not two** | the one park site (`wait_until`/`prepare_wait`/`block_on` → `completion::wait`, futex folded in, `park_lot`/`PARK_BUCKETS`/`wake_task` deleted) **and** the cancellable kill (§7.2's **four** changes — `handle_retire`'s parked arm claim-arbitrated, `SchedPass::pick`'s kill arm, `preempt_if_due`'s interaction with it, and `hand_off`/`adopt` — plus `Commit::Killed` → `dispose_none`, the one-shot cancel and §7.4's choice of teardown park). **They cannot be split**: C3 puts an `Armed` on a parked thread's stack while `Commit::Killed` still discards it, so RT5 turns an ordinary kill of a blocked thread — the endowment branch's own `kill_while_blocked` gate — into a kernel panic. **Plus the log branch's two surviving fallbacks (§11)**: `klogd`'s park becomes an `Inbox` park, and `emit`'s relaxed signal becomes the degenerate one-waiter post; `Source::Log` folds into the watch list carrying §5.3a's edge contract. The third — deleting a `log::pending()` pre-`hlt` condition — is struck: there is none. **Plus §7.2a's amendment to `scheduler-core-spec.md` invariant 7 and the re-derivation of sim I14's bound**, without weakening `old_migrate_kept_the_corpse_i14` | §7, **15 park sites → 1** | `toyos-sched` host tests for the new arms — `cpu.rs` has none today; `toyos-sched`'s loom model for cancel, extending `loom/tests/loom_retire.rs` rather than opening a fifth file; `blocking_read_stress`; `exit_wait_storm`; grep: one `dispose_block` caller; **`kernel-loom/tests/log_wake.rs` — already green, re-verify**, because the log branch shipped W3's model with its code rather than deferring it |
| C5 | `SleepLock`, `holder()` (§17.1), the `Parkable` threading. Nothing converted | §8 | `kernel-loom/tests/sleep_lock.rs`; the RMW count of §16.2 rule 2 |
| C6 | kernel threads, **shrunk further than the plan thought**: `klogd`, `sched/kthread.rs`, `driver::spawn`'s `None` fallback and `loader::start::kernel_start` are all on `main` and were written naming this chunk (§10), so C6 spawns `usbd` and `iod` on existing machinery, adds their two *recoverable* rows to the panic predicate — `klogd`'s is deliberately not one — and does the `KernelPayload.address_space` retype (§15 row 12). `iod`'s body is C12's | §10 | `blocked_dump` names all three |
| **C7+C8 — one chunk, not two** | xHCI async (`wait_transfer`/`wait_command`/`configure`, the per-disk claim, `XHCI` → `SleepLock`, `poll_if_pending` → `usbd` + `try_lock`) **and** `VFS`/`VOLUMES`/`ProcessData` → `SleepLock` with their 30 + 55 call sites and the boot/task split. **§11.4's obligation is discharged and is not in this chunk** — there is no kernel file sink left to re-home. **They cannot be split — see below** | §9, §12 | `toyos-xhci` host tests; `usb_storage_gate`; `killed_holder_releases`; `cancel_while_parked`; `sleeplock-spins` and `park-holding-a-spinlock` red; `io-depth-probe`'s syscall arm falls to 1; **`kernel_log_file` green against `/bin/logd`, which §12.3's choice can silently break**; the syscall half of the `--slow-usb` A/B moves here |
| C9 | the idle loop's declared end state: `i8042::verdict_due` off the halt check and the i8042 as a polled device on `usbd`; `xhci::port_work_pending` gone with C7's `poll_if_pending`; `log_health` and `reap_poisoned` stay and the chunk says why (§11.1, §11.2). **The log subsystem is not here** — log L1–L6 | §11 | the two `sched: cpu=` tests (`tests/toyos.rs:8974`, `:9478`) stay green **unmodified**, which is what makes the halt-check change checkable |
| C10 | `Poll<T>`; NVMe `CAP.TO`; virtio, HDA, IOMMU, RTC settles; the three duplicate `settles` become one | §4.3 | `no_spin_outside_the_allow_list` |
| C11 | blocking syscalls on the one shape; `SYS_SLEEP_UNTIL`; absolute deadlines; 24-byte CQE; the ring becomes an `Inbox` (its pages are already its own — §15 row 8); `toyos::ring::Ring` replaces `Poller`; soundd's `delta == 0` hack deleted | §14 | full suite; gate A fast tier |
| C12 | the write-back queue; `FileObject::on_zero_handles`; `SYS_FSYNC` parks; page-cache eviction to `iod`; **§13.1's page pinning and `close_file`** | §13 | `close-cannot-report-io-error`'s reproduction; `disk_backtrace` and `esp_files` still green (§13.2) |
| C13 | the deletion commit; the `src/sourcegate.rs` gates; **twelve** `specs/issues/` closures and the full-path citations that go stale with them (§19); CLAUDE.md | §19 | `cargo test --lib` green — **and no longer a real gate on the closures**, since `every_named_issue_file_resolves` was deleted with `src/docs.rs`. C13 puts rebuilding it to the owner (§19) and does the de-pathing as a stated deliverable rather than trusting a check |
| C14 | measurement: the interleaved four-arm A/B (§20.1, ~68 min of guest time, two worktrees); `io-depth-probe`; the positive log-content assertion (§20.2); assertions recorded in `tests/audio-baseline.toml` | §20 | the numbers go in this spec |
| C15 | `sched-check`: move invariant P's window to the scheduler entry, take its own baseline, then turn it on in one harness profile | §20.4 | its own baseline first — it will red on work this refactor did not do |

### 21.1 Why C7 and C8 are one chunk

Trace the disk path, whose lock order the source itself states
(`fat32_adapter.rs:308-315`, *"Lock order is VFS → here → `XHCI`"*):

```
vfs::lock()                    vfs.rs:29        VFS      [ticket Lock, preempt +1]
  → Vfs::flush_file            vfs.rs:538
  → FatVolume::write_at        fat32_adapter.rs:352
      device(role).lock()      fat32_adapter.rs:316  VOLUMES  [ticket Lock, preempt +2]
  → UsbBlockDevice::write_blocks   usb_storage.rs:95
  → xhci::with_disk            xhci/mod.rs:1869
      XHCI.lock()              xhci/mod.rs:1870 (static :1775)  [preempt +3]
```

At C7 alone, `XHCI` is a `SleepLock` while `VFS` and `VOLUMES` are still ticket
locks, so `with_disk` must call `XHCI.lock(&p)` — and **both ways of getting the
token fail**. `Parkable::of_current()` at the leaf runs at baseline +2 and RT1
refuses it. A token threaded from the syscall entry reaches the park with two
ticket spinlocks held, and `scheduler.rs:44`'s `assert_baseline` refuses *that* —
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
C7+C8 needs C3+C4, C5 and C6, and it is the stage whose number moves — the log
strike leaves C9 nothing on the disk path, so the syscall arm of the `--slow-usb`
A/B is C7+C8's alone. C9 needs C6 and C7+C8 and is small. C11 is independent of
C7–C9 and may float. C12 needs C6. **§11.4's second shape needed C12 before
C7+C8; that constraint is void with the shape** and the graph is one edge simpler
than it was. C15 is independent and last.

**§24's fallback split, if the owner wants one:** C0–C6 as one pull request and
C7–C15 as a second. The graph permits it at C6 and nothing before C7 changes a
lock. **Taken by owner ruling 2026-08-16**: this pipeline lands as two pull
requests, split at the C6 boundary.

Across the two branches: C3 must follow the endowment branch's chunk 2 (§15 row
4) and C12 its chunk 2 as well (row 9). Since the whole of this branch follows
their landing, both are satisfied by C0 — recorded so nobody reorders C3 ahead
of the merge on the grounds that it "only touches the scheduler".

**And against `wt/toyos-logd`: it lands first, and the dependency now runs this
way.** The ruling (§11.4) makes that branch a **hard prerequisite** for C0, not a
peer: C7+C8 does not compile with `log_file.rs` alive, and no chunk here re-homes
it. What C0 inherits is the tree log §12.6 describes — the record ring, `klogd`
and its machinery, `/bin/logd`, no `log_file.rs` — plus three fallbacks this
branch converts at C3+C4 (§11) and one `specs/issues/` de-pathing (§19). **Log
L0 no longer re-checks anything against this branch**; its §12 is a list of
obligations on this one.

---

## 22. Failure modes and runtime fail-fast

| failure | behaviour | recovery |
|---|---|---|
| A post races a park | Invariant W: the parker's recheck observes the record | self-wake, retry — structural |
| A kill races a park | `Cancelled`; the task returns and unwinds by returning | dies at the syscall boundary |
| A killed task was **already parked** holding a sleep lock | `handle_retire` makes it runnable instead of reaping it (§7.2); it observes the kill at its next `wait` and unwinds | the lock is released by the guard's own `Drop` |
| A killed task was **ready** with a guard on its stack | **not "left alone"**: `SchedPass::pick` reaps a killed ready task at the head of every pick (`cpu.rs:926`) and `ReadyTask::dispatch` is reachable only past that check, so §7.2 must rewrite that arm too | as above, once §7.2's four changes land |
| A cancelled thread's teardown must take a sleep lock | **unresolved, and §7.4 says why**: the kill bit is sticky and `WaitTicket::commit` refuses to park a killed task before anything else (`waitq.rs:383`), so a killed thread cannot park at all — including in teardown. One of §7.4's three shapes must be chosen | **owed by C3+C4**, not "ordinary acquire" |
| The victim does not reach **release** inside the retirer's 1 s | `retire_task`'s `Tripwire` panics. It waits on `handle.released()` and not on the word reaching `Dead` (`scheduler.rs:443`, and its doc says why), so what it now bounds is unwind + teardown + every sleep-lock acquire on the way + the release | C4 re-derives the number against `released()` (§7.3) |
| A device never answers | the thread parks forever; the CPU is free | Ctrl+Alt+D names the task and the subject; disconnect or kill cancels it |
| `/bin/logd` parks on a dead stick | with no bound no error is produced, so logd's own give-up policy (log §5.4) never fires and `/log` stops with no line saying why. **The console is unaffected** — `klogd` drains it and has no disk in it (log §4.1), so the T14's total-logging-loss case is gone | **still unresolved until §12.3's choice is made.** Unlike `iod`, logd is killable, so init is a second line; a `Tripwire`/`Budget` is the first. `kernel_log_file` is what reds |
| `iod` parks on a dead stick | **every** write-back in the machine stops — `SYS_FSYNC`, deferred close flushes, page-cache eviction — and nothing can kill it | **still unresolved until §12.3's choice is made**, and this is the row with no second line at all |
| The inbox fills | oldest-dropped with a count, and a `Gone(Overflowed)` record so the waiter re-derives | a bounded loss, never a lost wake |
| `usbd` wedges on a broken controller | `usbd` alone parks; `klogd` and `iod` are unaffected | the dump names it |
| A CPU takes an event for a transfer nobody is parked on | `Outstanding` matches by TRB address; an unmatched event is dispatched as today | unchanged |
| Boot's VFS is contended | `try_lock().expect(..)` panics by name — **once C8 has made boot use `try_lock`, which it does not today** (§6.1) | a kernel bug, fail fast |

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
   `readers_wq`/`writers_wq` per object, and its io_uring watchers too
   (`pipe.rs:142`) — but **six** of the eight watcher stores are global statics
   that §19 deletes. True of the mechanism this spec keeps, and true of exactly
   two of the eight stores it unifies.
   The other three reasons carry it.
2. **A `RingArena` of 32 KiB slots** (superseded §5.2). `IoUringObject` owning its
   own `PageAlloc` is the endowment spec's answer and it is simpler; two specs
   describing one object is worth more than a slot allocator.
3. **Two park channels (`Ring` + `Futex`)** (superseded §6.4). The futex's value
   check belongs *before* the arm, exactly like every other readiness check, and
   the wake-generation protocol existed only because **the futex word** has no
   level-readable state. **Scoped deliberately**: the first draft wrote "there is
   no level-readable state today", which implies every subject gains it after the
   refactor, and one does not (§5.3a). One channel, one recheck, one proof — and
   **a subject that cannot be asked is handled by the class in §5.3a, not by a
   second channel.** As written the unscoped sentence was the strongest statement
   of the premise `Source::Log` falsifies, and a reader who took it literally
   would build `arm` with no edge path. (The protocol itself is already gone —
   `ba76478`, §4.1 P11 — so what is rejected here is re-adding it.)
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
   the >2× rule. Revisit if the console drainer is ever measured to be CPU-bound.
   **The first draft's reason was wrong and is withdrawn**: "the THRE spin runs on
   a preemptible thread" is false, because `BackendGuard::lock` takes
   `save_and_cli()` (`serial.rs:96`) and holds it for the whole drain, so the
   drainer is not preemptible there and the CPU is deaf to every IPI (§4.5a). The
   rejection stands on cost alone, and the `cli` window is a residual neither this
   document nor `specs/log-architecture-spec.md` closes — named here rather than
   left to be discovered.
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
11. ~~**A userland `logd`.** It cannot log the boot that precedes it, nor its own
    death.~~ **Overruled by the owner on 2026-08-09, and the argument is
    `specs/log-architecture-spec.md` §12.1a rather than the ruling.** Both
    objections are answered by a mechanism: `Drain::Inline` puts every boot record
    on the wire as it is written and cpu0's shard retains all 185 of them for logd
    to write to `/log` when it starts — which is what `log_ring::enable_file_sink`
    already does one level down, moved up a layer; and logd's dying words reach
    the console through its own handle, which is the kernel's to serve, with init
    naming the exit. What is genuinely lost is `/log`'s copy of logd's own death,
    recorded there as a regression rather than waved away.

    **This document's own second-stage review reached the same split from the
    other side and that is the stronger half of the argument.** §12.3 established
    that a kernel `logd` parked on a hung `/log` stick costs the machine *all*
    logging, serial included, on the T14 which has no serial port — a failure a
    kernel drainer has and a userland one cannot, because the kernel's drainer has
    no disk in it. The design rejected here is the alternative to the design that
    review found broken.
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
4. **A thread parked forever on a hung device is new behaviour.** §12.3's three
   cancellers are circular for the case that matters — Reset Recovery's only
   trigger is the bound being deleted — so the real count is zero. **The log
   strike shrank this risk and the order ruling gave it a witness.** The parked
   thread is no longer one that owns the console too, so a hung stick cannot cost
   the machine all logging; the victims are `iod`, which costs the machine all
   write-back and cannot be killed, and any userland `SYS_FSYNC` caller, which
   can. **And one of those callers is now a shipped daemon with a written
   give-up policy** (log §5.4) that reads an `Io` the live 2 s `USB_TIMEOUT_NS`
   supplies today — so "no bound anywhere" does not merely park a thread, it
   makes that policy unreachable and `kernel_log_file` is what notices. **This is
   still the plan's largest open decision and C7 makes it**: a `Tripwire` on the
   transfer, or a `Budget` at the filesystem layer.
5. **Gate A's thorough tier being red on `main`** means every verdict in C14 is a
   delta. If it goes green before C14, take the pass/fail — but do not wait for it.
6. **`/log` is a USB volume in every profile, so the headline number and the worst
   failure mode read the same on the instrument.** A log sink that stopped writing
   improves the `--slow-usb` wake number exactly as much as a log sink that got
   fast. §20.2's positive log-content assertion is **the only thing left that
   separates them**, and the order ruling made that literally true:
   `reintroduce-idle-flush` is retired rather than moved, because there is no
   idle flush to reintroduce (§20.3). **`kernel_log_file` against `/bin/logd` is
   the whole of this branch's protection, and weakening it is not available.**
7. **Fourteen chunks in one pull request, across the scheduler's kill path, four
   global locks and the USB transport.** §5.5's "this is deliberate de-risking …
   the scheduler migration cost seventy defects; this refactor does not reopen it"
   is an argument about `toyos-sched`'s *internals* being untouched — and §7.3 has
   now made even that false, since the retire handshake changes.
   `specs/assessments/metal-track-history.md` records ~70 defects found in code whose own
   suites were green. "Every chunk passes `cargo test`" is a process, not a
   mitigation for one merge commit. **§21.2's C0–C6 / C7–C15 split is the fallback
   and the owner should be asked before C0, not after C13.** The log strike took
   the whole log subsystem out of this merge commit — the largest single
   reduction in this risk the plan has had — and the order ruling makes that a
   landed fact rather than an intention, since the code is on `main` before C0
   rather than being deleted from this branch's diff.
8. **`usb-transport-break` goes vacuous.** That actuator exists to reproduce "the
   state a transfer that ran out `USB_TIMEOUT_NS` leaves behind". If the bound
   goes, production can no longer reach that state and the gate certifies a path
   the shipping kernel cannot take. Whatever §12.3 chooses, C7 re-points it.
9. ~~**The strike leaves one obligation on this branch that neither spec had
   planned for.**~~ **Closed 2026-08-09 by the order ruling
   `endowment → log → completions`** (§11.4). C7+C8 re-homes nothing; log L6
   deletes the file sink before C0 merges.

   **What the ruling bought and what it cost, so the trade is legible.** Bought:
   §11.4's two shapes struck, `SINK` off §9's lock table, `reintroduce-idle-flush`
   unneeded, `apic.rs:146` and the kick loop re-pointed by their owner, C6
   shrunk, §17.1's dump row gone, and the largest chunk in this plan made
   smaller rather than larger. Cost, all of it paid on the other branch except
   the last three: log L3 builds the kernel-thread machinery; log §2.6a takes a
   `drain_irqs`-decided wake and keeps one pre-`hlt` condition, both of which
   **this** branch converts at C3+C4 along with log's W3 loom model (§11); log
   L4 adds a sixth `io_uring::Source` this branch deletes with the other five;
   and C0 must de-path the `specs/issues/` citations log L8 leaves dangling in
   this very file (§19). **None of those is a compilation blocker and §11.4's
   was.**
