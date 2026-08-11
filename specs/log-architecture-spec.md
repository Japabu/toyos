# The log architecture — the kernel stops being a file-writing logger

The owner recommended and accepted this design on 2026-08-09, under his standing
rule that effort is never an argument. It answers
`specs/issues/design-debt/redesign-the-log-subsystem.md`, which is the owner
asking rather than the owner deciding, and it is the design half of that
question — the `kernel/src` layout half is untouched here and stays open.

Four moves:

1. The kernel keeps **one** thing: a wait-free, per-CPU ring of **structured
   records**, written whole or not at all.
2. **File writing and every policy that goes with it move to a userland daemon,
   `logd`.** The kernel never writes a file again.
3. **Userland output never enters the kernel ring.** A program's stdout and
   stderr are IPC streams to `logd`.
4. **Reading is a real surface** — one cursor syscall serving `logd`, the
   console, the diagnostic tools and the test harness.

Every number below comes from a command run against this worktree
(`wt/toyos-logd`, at `19c761e`; `origin/main` has since moved to `0e48d2e`) on
2026-08-09, or from an
entry in `specs/issues/` that records its own measurement. Where a figure is a
prediction it says so and names the chunk that must measure it. Every figure was
re-derived by the adversarial review on 2026-08-09; §2.2's sizing derivation was
wrong and is replaced, and the corrections it made are marked **CONFIRMED**
where a command established them.

**This spec implements after one other branch and before the other, and the
order was ruled on 2026-08-09: `endowment → log → completions`**, where it had
been `endowment → completions → log`. So the baseline is `main` after
`wt/toyos-endow` (`specs/capability-endowment-spec.md`, read at `8231e90`), and
`wt/toyos-compl` (`specs/completion-architecture-spec.md`, read at `9890d5e`)
opens on the tree this branch leaves.

**The reasoning, recorded here so neither reader re-derives it.** Completions
cannot compile with the kernel file sink alive. Its §11.4 traces
`idle_loop → flush_log_file_if_affordable → log_file::poll → vfs → VOLUMES →
xhci::with_disk → XHCI.lock()`: after its C7+C8 `with_disk` needs a `&Parkable`
and the idle loop has no way to make one, and `SINK`'s raw guard is held across
the park. Its two available shapes each cost something real — moving the drain
to `iod` imports §13.4's regression list into a branch that has nothing to do
with logging and forces re-pointing `apic.rs:160`'s `wait_for_log_file` and its
kick loop (`apic.rs:146`); keeping the append on the idle loop needs its C12
before its C7+C8 and rests on a tail-page-resident premise nothing enforces.
What *this* branch needs from that one is **nothing**: the lock-free
single-waiter post `klogd`'s wake wants is `toyos_sched::waitq::wake_direct`,
in the tree today at `toyos-sched/src/waitq.rs:236` (§2.6a, **CONFIRMED**
2026-08-10). **So the ordering question is one-sided** — a compilation blocker
with two costly workarounds against no cost at all — and log-first *removes*
the work rather than
relocating it, because completions never touches `log_file.rs`: L6 has already
deleted it. That is completions' own §11.4 third option, taken.

§12 is the reconciliation, and the order change changes what it is. Completions
is **unlanded** when this branch opens, so its rows are no longer things L0
re-checks against a merged tree — they are **obligations this branch places on
that one**, checkable when it opens. §12.6 is the new section: every primitive
this branch does without because it does not exist yet, what it does instead,
and what completions converts afterwards.

## 0.0 L0 — this spec walked against the merged tree

**Merged `origin/main` `f1724a9` into `wt/toyos-logd` on 2026-08-11**, 118
commits. Everything below was planned against `19c761e`, and three pull requests
moved what it cites: **#22** rewrote the syscall layer (`Fd` → a typed
refcounted `Handle`, ports and namespaces, `/etc/system.manifest`), **#27+#29**
turned 45 kernel feature-set builds into 3 with actuators armed by a boot
parameter, **#28** was documentation.

**The mechanism survives whole.** Nothing in §2 (the record, the shard, `emit`,
the bracket), §2.6a (the wake), §3.1 (the two readers) or §4 (the drain modes)
depends on anything those three changed, and every symbol they rest on is still
there: `wake_direct` (`toyos-sched/src/waitq.rs:236`), `prepare_wait`/`block_on`
(`scheduler.rs:170`, `:181`), `park_lot` (`:201`), `steal: true`
(`sched/driver.rs:296`), `percpu_fetch_add`'s subject `head`. What moved is
**where things live, what a manifest key looks like, what an actuator costs, and
when the ABI may land**. Corrected in place below; this subsection is the
inventory, not a second copy.

**Made easier, and each replaces a mechanism this spec was going to build:**

| this spec planned | the merged tree already has |
|---|---|
| `logread = true`, a `#[serde(default)] bool` "shaped exactly like `realtime`" (§5.1) | `realtime` is gone. Authority is `syscap = [...]`, a list of names `toyos_manifest::syscap_rights` resolves — and it **refuses an unknown name by name** (`lib.rs:69`). `logread` is one row in `SYSCAP_RIGHTS` |
| §3.2's whole "`ProgramConfig` will not tell you if you forget the field … the gate must assert over the *parsed* struct" hazard | **void.** There is no field to forget: a name no `SYSCAP_RIGHTS` row carries is a build that stops. The silent-drop failure mode this spec spends a paragraph on cannot happen |
| six negative-control **kernel builds** and "six more images in `specs/test-cost-audit.md`'s ledger" (§9.4) | six declarations in `kernel/src/actuator.rs`'s `actuators!` macro, **zero extra kernel builds and zero extra images** — one test kernel carries all of them and `BootOptions::kernel_params` names which is armed |
| endow §1.5's "there is exactly one `ConsoleObject` for the machine", which §4.4 and §12.5 had to argue against | `ConsoleObject::new()` already mints a fresh object per call (`object/device.rs:202`); there is exactly one *call site*, `spawn_init`. §4.4's "one backend, one object per endowment" is what the code is already shaped for |
| init endowing "the stdio handles … as transferred handles" per endow §4.5 | init does not do this for `[boot] start`, and §6.1's table already said so. `Command::spawn` inherits, so every daemon holds a handle to **init's** console object today — which is exactly what §4.4's line buffer must not share |

**Made harder, or newly open:**

1. **The ABI cannot land in the middle of this branch, and §11's recommendation
   is unbuildable as written.** `pr::abi_lands_alone` offers the `git switch -c
   <branch>-abi <sha>` remedy **only when the sysroot commits are a prefix**
   (`src/pr.rs:339`); otherwise it says the split "cannot be made without
   reordering — and nothing in this workflow rebases." L1–L3 touch no sysroot
   source and L4 does, so an L4 landing mid-branch is refused with no remedy.
   **So the ABI is chunk zero, on its own branch off `main`**: `toyos-abi`'s
   `log.rs`, `SYS_LOG_READ`, `Rights::LOG`, landed and merged before L1 opens.
   §11 and §13.2 are corrected. This is a change to the *order* the owner ruled
   on in §13.2, not to the decision — he ruled for a separate ABI landing, and
   this is where the tooling puts it.
2. **How `/bin/logd` gets the second `ConsoleObject` is open.** init can only
   endow what it holds, `ConsoleObject::new()` is kernel-only, and no syscall
   mints one. The answer that needs no new syscall: **`spawn_init` mints two**
   — three slots from the first, and the second installed as a labelled
   endowment init moves into logd. "Exactly two in the machine" is then a
   property of `spawn_init`'s source, which is stronger than §4.4's manifest
   gate and keeps `Console` without `Rights::DUP`. Recorded here because §4.4
   assumed a mechanism that does not exist.
3. **Eleven manifests, not six** (§5.1a). `tests/desktopcase/`,
   `tests/desktopaudiocase/`, `tests/doomcase/` and `tests/doommusiccase/` were
   missed and `tests/testcases/` was not in the table at all. Every one declares
   `[boot] start`, so every one needs `logd`. Six carry a `test-runner` row —
   doomcase, doommusiccase, metalcase, netcase, sshdcase, testcases — and
   desktopcase and desktopaudiocase carry none, so §3.2's "the gate that reads
   the log runs inside `test-runner` itself" holds for six configs rather than
   three.
4. **`log-rotate-fast` is a boot parameter, not a cargo feature.** §5.5 and
   §8.1 say the feature "goes"; what goes is an `actuators!` row and
   `log_file::max_log_bytes`, and the fast value becomes a logd argument as
   planned.

**Citations re-derived. Every number in this spec that a command produced was
re-run on 2026-08-11; the ones that moved:**

| §  | said | is |
|---|---|---|
| 1.1 | `log_file.rs` 565 lines, `panic_console/mod.rs` 1,188 | 567 and 1,191. `log.rs` 64, `log_ring.rs` 549, `serial.rs` 467, `virtio_console.rs` 221 unchanged |
| 1.1 | 654 `log!` sites | **658**; `boot_phase!` still 6 |
| 2.1 | the seven `alert!` sites at `main.rs:307`, `:310` | `main.rs:312`, `:315`. The other five are unmoved, and the two deliberately-out-of-scope raw-UART producers are still `main.rs:195` and `exceptions.rs:348` |
| 2.2 | `alloc_percpu` at `percpu.rs:262`, `init_ap` at `:438`, `PerCpu` allocated at `:425` | `:283`, `:509`, `:477`/`:498`. `IST1_STACK_SIZE` is `:267` and still 16384 |
| 3.2 | `Source::Log` would be "a **sixth** `io_uring::Source`" | a **ninth**. `Source` gained `Port`, `PipeReadable` and `PipeWritable` (`io_uring.rs:152`). The five per-source `IO_URING_WATCHERS` statics are unmoved to the line (`keyboard.rs:17`, `mouse.rs:19`, `net.rs:41`, `hda.rs:220`, `virtio_sound.rs:270`) and the three new kinds keep their watchers on the object instead — so compl C3 folds five statics plus one, not six |
| 3.2 | `Rights::LOG` is a tenth bit in a nine-bit `Rights(u32)` | holds: `handle.rs:65-83`, nine bits, `ALL = 0x1ff`. `LOG` is `1 << 9` and `ALL` becomes `0x3ff` |
| 3.4 | 78 syscall constants, highest 98 | **80 constants, highest 112.** Endowment's 99–112 have landed; **113 is reserved for `SYS_PORT_REARM`** (`capability-endowment-spec.md:2682`) and 114–115 are free |
| 3.4 | this spec "allocates nothing" and L0 computes the first clean number | computed: **114**. Written down here because the reason not to — that `main` moves under two open branches — expired when endowment landed and completions took none. 115 is left for `SYS_SLEEP_UNTIL` |
| 5.4 | `USB_TIMEOUT_NS = 2_000_000_000` at `xhci/mod.rs:319`; `MAX_BLOCKED_NANOS` 10 s at `log_file.rs:190` | `:319` unmoved; `MAX_BLOCKED_NANOS` is `log_file.rs:192` |
| 6.4 | `LOG_FILE_DRAIN_NANOS` 500 ms at `apic.rs:117`, `wait_for_log_file` at `:160`, `halt_all_cpus` at `:220` | `:117`, `:160`, `:217` |
| 8.1 | `driver.rs`: `:523`, `:534`, `:552`, `:564`, `:681`, `:689`, `:701`, `:706`, `:722`, `:743`, `:762`, `:832` | `:528`, `:539`, `:557`, `:569`, `:693`, `:701`, `:713`, `:718`, `:734`, `:755`, `:774`, `:844`. Every name survives and the shape is unchanged |
| 8.1 | "`fd.rs:585`'s `Descriptor::SerialConsole` arm" | **`fd.rs` does not exist.** #22 replaced the whole `Descriptor` model; the arm is `object/ops.rs:469-470`, `KObjectRef::Console(_) => serial::SerialWriter::console().write_user(buf)`. Same mechanism, and L3's re-point is the same edit at the new address |
| 12.4 | `Vfs::flush_file` (`vfs.rs:538`) has exactly three callers: `fd.rs:48`, `fd.rs:644`, `log_file.rs:376` | **still exactly three**, at `object/file.rs:31`, `object/ops.rs:563` and `log_file.rs:378`. `SYS_FSYNC` dispatches to `ops::fsync` (`ops.rs:550`) and still reaches no `sync_mount` and no `dev.flush()`, so §12.4's finding and `log_is_durable_after_fsync` are unchanged |
| 12.5 | the endowment rows, checked against the plan at `8231e90` | checked against the merged **code**: `serves`, `receives`, `devices` and `[boot] start` are as described (`toyos-manifest/src/lib.rs:75-105`); `SYS_HANDLE_SEND` is 103; the two `ConsoleObject` contradictions are resolved by the code rather than needing an edit to that spec |

**Unmoved and re-confirmed**: the three `MAX_CPUS` declarations §2.2 names
(`sched/mod.rs:19`, `trace.rs:41`, `shootdown.rs:34`, all `8`);
`serial.rs:285`'s "no lock is acquired";
`log_ring.rs:323-324`'s per-record `compare_exchange_weak` and `:131-136`'s
recorded reason for avoiding the second; `serial.rs:97`'s `BackendGuard::lock`;
`preempt.rs`'s `lock add`/`lock sub` pair; `clock.rs:76`/`:85`;
`hw.rs:42`'s `IrqGuard::close`; every `specs/issues/` slug §8.2 names;
`specs/completion-architecture-spec.md` is still absent from this tree, so
§12.7 stands.

---

## 0. The shape, in one page

```
                     ┌──────────────────────────────────────┐
  log!/alert!/       │  kernel/src/log/                     │
  boot_phase!  ─────►│    emit()  →  per-CPU Shard          │
  (654 + 6 sites)    │      one reservation, one commit     │
                     │      no lock, no locked RMW          │
                     └───────┬──────────────┬───────────────┘
                             │              │
              drain_ordered  │              │  snapshot_committed
              (in-order,     │              │  (never blocks; a dying
               blocks on an  │              │   machine has no "later")
               uncommitted   │              │
               record)       │              │
              ┌──────────────┴───┐     ┌────┴──────────────────────┐
              │ klogd (kernel    │     │ panic_console, sched::dump│
              │ thread) → 16550  │     │ → the panel               │
              │ / virtio-console │     └───────────────────────────┘
              └──────────────────┘
                             │
                  SYS_LOG_READ (cursor, stateless in the kernel)
                             │
                     ┌───────┴────────────────────────────────┐
   every program's   │  /bin/logd                             │
   stdout/stderr ───►│    merges kernel records + N streams   │
   (an ordinary      │    writes /log, rotates, retains       │
    IPC stream)      │    owns the give-up policy on a stick  │
                     │    forwards to its own console handle  │
                     └────────────────────────────────────────┘
```

What that deletes: the kernel's filesystem write path from the log
(`log_file::SINK → vfs::VFS → fat32_adapter::VOLUMES → xhci::XHCI`, preemption
off for a device round trip), the idle loop's two log statements and **two of
its four** pre-`hlt` conditions — `log_ring::has_pending` (`driver.rs:523`) and
`log_file_flush_due` (`:552`), the two that exist to serve the log; the other
two are `i8042::verdict_due` (`:534`) and `xhci::port_work_pending` (`:564`) and
belong to the completion branch (§8.1, §12.2) — the "affordable flush" heuristic
and its deferral ceiling, the shared byte ring that lets a userland `println!`
land inside a kernel line, and the harness detector built to notice when it does.

---

## 1. What is there now, measured

### 1.1 The subsystem

Six files, no core (`wc -l`, this tree):

| file | lines | what it is |
|---|---|---|
| `kernel/src/log.rs` | 64 | the `log!` macro and a GS-validity flag |
| `kernel/src/drivers/log_ring.rs` | 549 | a 64 KiB **byte** ring and its drain policy |
| `kernel/src/log_file.rs` | 565 | the `/log` sink, its flush, its rotation and retention |
| `kernel/src/drivers/serial.rs` | 467 | the 16550 sink and the stack formatter |
| `kernel/src/drivers/panic_console/mod.rs` | 1,188 | the screen sink, panic-only |
| `kernel/src/drivers/virtio_console.rs` | 221 | the second serial-shaped sink |

654 `log!` sites and 6 `boot_phase!` sites in `kernel/`
(`grep -rn 'log!(' --include='*.rs' kernel/ | wc -l`, and the same for
`boot_phase!`).

### 1.2 Three defects, and every fix so far has been scaffolding around them

**(a) The kernel appends to a FAT32 volume over USB, from the idle loop.**
`log_file::poll` runs at `sched/driver.rs:689`, takes `SINK`, then the VFS lock,
then the volume, then the controller. Root `CLAUDE.md` records the depth
`io-depth-probe` measures: **4 from the idle loop, 5 from a syscall**, each level
a ticket spinlock that disables preemption for its whole life. Cost per flush,
measured in-guest and recorded in
`specs/issues/boot-media/log-flush-is-unbounded.md`: **7.2–26.0 ms** before the
resident-block change and **2.0–9.7 ms** after, against a DMA pipeline depth of
**23.219 ms**. Staged on this host with `usb-slow-device`, soundd's worst wake
goes from **7,117 µs to 165,948 µs** (root `CLAUDE.md`).

**(b) The ring's only drains are the timer tick and the idle loop.** So a boot
that wedges before `enter_idle_loop` produces **nothing at all**, including
everything it logged before it wedged
(`specs/issues/diagnostics/pre-idle-wedge-says-nothing.md`), and an idle machine
is always one line behind
(`specs/issues/kernel/log-ring-flushes-one-line-behind.md`). Both were patched by
adding conditions to the pre-`hlt` check — `driver.rs:523`, `:534`, `:552`,
`:564` — which is a CPU declining to sleep because the log is unwritten.

**(c) Userland console output shares the kernel's ring.**
`object/ops.rs:470` → `SerialWriter::console` → `log_ring::write_chunk_blocking`,
so the unit of interleaving is a `write` syscall.
`specs/issues/diagnostics/serial-console-has-no-line-atomicity.md` has four
recorded splices, one of which reds a landing gate on a documentation-only
branch, and a measured **1 run in 10** for `desktop_audio_client` on CI.

### 1.3 Two things the code says about itself that are not true

Both **CONFIRMED** by reading the two files, 2026-08-09.

- **`log!`'s doc comment claims it "acquires the serial lock once for the whole
  line so the prefix and body can't interleave with output from another CPU."**
  `SerialWriter::lock()` takes no lock — its own doc says so twenty lines further
  down (`serial.rs:285`: *"Despite the name `lock()`, no lock is acquired"*). The
  atomicity `log!` claims comes from the ring append, and it is exactly what (c)
  above shows does not hold against a userland writer.
- **`log_ring::DRAIN_CHUNK`'s comment prices itself against "IST1, a 4096-byte
  stack."** `IST1_STACK_SIZE` is **16384** (`arch/percpu.rs:246`). The number 512
  is still right; the reason recorded beside it is stale.

Both disappear with the modules that carry them.

### 1.4 The one hot-path cost that governs the ring's design

`specs/issues/hardware/one-rmw-per-log-line-cost-350ms.md`, measured interleaved
in one session: adding **one** `fetch_add` per `write_chunk` — a few hundred per
boot, uncontended — moved `Boot: complete` from 497–504 ms to 812–839 ms. Under
TCG an atomic read-modify-write is not one instruction; QEMU leaves the
translation block to run it exclusively, hundreds of microseconds each.

**`RingGuard::lock` already does one, on every record.**
`log_ring.rs:323-324` is `RING_LOCKED.compare_exchange_weak(false, true, Acquire,
Relaxed)` — **CONFIRMED**, a locked RMW per `write_chunk`, per drain chunk, per
cursor move. That the tree already knows this is recorded ten lines above it, at
`log_ring.rs:131-136`, where `WRITTEN` is a load and a store *"for the same reason
`OWED` is … the `lock xadd` cost **350 ms of boot**"* — the module avoided the
second RMW and kept the first.

The design below removes it in `Drain::Thread` and puts **no locked RMW on the
producer's path at all**: `emit`'s whole contribution to the wake is a
`fence(SeqCst)` and a relaxed load, and the five locked operations the post
costs are paid at most once per `klogd` park, by whichever producer wins the
`LOG_WAITER` swap (§2.6a counts them). `Drain::Inline` keeps one CAS, in
`BackendGuard`, for the boot's
185 records on one CPU (§2.3). **Prediction, to be measured by L1's boot A/B and
not asserted here: removing the per-record CAS is worth a similar saving to the
one that issue records.** If it is not, the measurement is the finding.

---

## 2. The record and the ring

`kernel/src/log/` — `mod.rs` (the macros and `emit`), `record.rs` (the type),
`shard.rs` (the ring), `read.rs` (the two readers), `console.rs` (the serial
sink and `klogd`).

### 2.1 The record

```rust
// toyos-abi/src/log.rs — one layout, two types over it
pub const MAX_RECORD_MESSAGE: usize = 224;
pub const RECORD_BYTES: usize = 256;

/// What a reader gets. Plain POD, `Copy`, no interior mutability: this is what
/// `SYS_LOG_READ` copies out and what the `Display` in §3.3 renders.
#[repr(C, align(64))]
pub struct LogRecord {
    /// The record's identity. In the kernel's slot this same word is the
    /// validity word (`Shard::Slot::seq`); by the time a reader holds a copy it
    /// is just the sequence number, and it is what `LogCursor::next` counts in.
    pub seq: u64,
    pub at_ns: u64,
    pub pid: u32,
    pub tid: u32,
    pub cpu: u16,
    pub len: u16,
    /// Bytes the message would have had past `MAX_RECORD_MESSAGE`, saturating.
    /// Never a silent truncation.
    pub elided: u16,
    pub level: Level,      // u8
    pub flags: u8,         // bit 0: written before percpu was ready
    pub msg: [u8; MAX_RECORD_MESSAGE],
}
const _: () = assert!(size_of::<LogRecord>() == RECORD_BYTES);
const _: () = assert!(align_of::<LogRecord>() == 64);
```

**The kernel's slot is the same layout with the first word made atomic**, and
nothing else differs:

```rust
// kernel/src/log/shard.rs
#[repr(C, align(64))]
pub struct Slot {
    seq: AtomicU64,        // the same eight bytes as LogRecord::seq
    body: UnsafeCell<Body> // LogRecord minus its first word, 248 bytes
}
const _: () = assert!(size_of::<Slot>() == RECORD_BYTES);
const _: () = assert!(offset_of!(LogRecord, at_ns) == size_of::<u64>());
```

**Two types and not one, because `AtomicU64` cannot cross the syscall boundary
honestly.** `Record` in an earlier draft of this section was a single type with
an `AtomicU64` in it, shared with userland — which makes the copy-out a
transmute of an atomic into a value nobody synchronises on, and gives `logd` a
field named `commit` that commits nothing. The layout is one thing and the two
`const` assertions above are what keep it one; the *types* say which side is
which. §10's L4 delivers both.

**Why 224 bytes of message.** Measured over every committed T14 log
(`cat specs/metal-logs/*/*.log`, message length after the `[kernel … ] ` prefix):
12,497 lines, min 14, p50 **59**, p90 **111**, p99 **154**, p999 **857**,
max **863**. Everything above 200 characters is one call site — a `{:?}` of
`KernelArgs`, 18 lines of the 12,497 (0.14%). So 224 covers 99.86% of the tree's
own output, and the one site that exceeds it is a struct dump that L1 splits into
several records rather than being truncated. A message past the bound is
truncated **and says by how much**; `elided` is not decoration, it is the
difference between a bound and a lie.

**Why fixed-size.** A variable-length record needs a descriptor ring and a data
ring and an argument about the two staying consistent. A fixed-size POD record
makes "half a record" untypeable: see §2.4. The cost is space, and the two
figures it is priced against are different quantities that both land near 90 —
**CONFIRMED**: the mean *rendered line* over the corpus is **89.4 bytes**
(prefix included, which is what a byte ring stores and what §14's 2.8× compares
against), and the mean *record payload* is 32 bytes of header plus a 68.2-byte
mean message, **100.2 bytes** of the 256 (which is what §3.2's copy-out waste is
measured against).

**`Level` has three variants and each has callers today.**

| variant | producer | who reads it |
|---|---|---|
| `Info` | `log!`, 654 sites | everyone |
| `Phase` | `boot_phase!`, 6 sites | the panel repaints on one |
| `Alert` | `alert!`, **7 sites** — enumerated below | the panel paints the row red |

`Alert` **deletes a magic-value sentinel**: `panic_console::has_alert`
(`mod.rs:1035`) scans each row for three consecutive `!` bytes, and its own
comment enumerates the strings that happen to match. That is the comment the
root `CLAUDE.md` says is the type you should have written. It is not a severity
ordering and nothing orders it; every consumer matches exhaustively.

**The conversion is exactly seven sites and a conversion that misses one loses a
red row on the panel silently, so they are named** (`grep -rn '!!!' --include='*.rs'
kernel/src/`, **CONFIRMED** 2026-08-09):

| site | line |
|---|---|
| `main.rs:175` | `!!! EARLY PANIC !!!` |
| `main.rs:209` | `!!! DOUBLE PANIC !!!` |
| `main.rs:307` | `report_log_destination`, no `/log` |
| `main.rs:310` | `report_log_destination`, no console and no `/log` |
| `arch/debug.rs:87` | `!!! PTE CORRUPTION DETECTED !!!` |
| `arch/idt/exceptions.rs:276` | `!!! PANIC !!!` |
| `arch/idt/exceptions.rs:572`, `:575` | `!!! FAULT …` (one `alert!`, two arms) |

**Two more `!!!` producers exist and are deliberately out of scope**:
`main.rs:195`'s panic-reentry line and `exceptions.rs:348`'s `!!! DB TRAP !!!`
both write raw bytes straight to the UART and never enter the ring, so
`has_alert` cannot see them today either and `Level` is not their business.
`nmi_does_not_log` (§9.5) gains a second clause: **no `log!` in `kernel/` has
`!!!` in its format string.** That is the gate that makes the deletion of the
sentinel checkable rather than asserted.

Converting the 654 `Info` sites to a finer level set is **not in scope** and is a
named exception: a level with no reader is a field built for a plan. The rule
going forward is that a refusal uses `alert!`.

**`at_ns`, not a raw TSC.** `clock::nanos_since_boot` is a `rdtsc` against one
global anchor and one global period (`clock.rs:76`), so it is directly comparable
across CPUs. Carrying the raw TSC instead would save the producer one
`__udivti3` — an out-of-line `compiler_builtins` call that `dump_nmi_probe` has
already caught a spin inside (`clock.rs:85`) — but it would make the record
un-renderable without the period, and the panel renders records on a machine with
no clock syscall left to ask. The producer pays what `log!` pays today. Recorded
as a rejected optimisation so it is not re-derived: §14.

**Cross-CPU ordering rests on the TSC being invariant and firmware-synchronised.**
ToyOS targets 2020+ x86-64, where it is. If it is not, two records from two CPUs
may be merged out of order by the skew; nothing breaks and the reader cannot tell.
Stated rather than assumed, and it is the same assumption `clock.rs` already makes
machine-wide.

### 2.2 The shard

```rust
#[repr(C, align(64))]
pub struct Shard {
    /// Reservation counter. **Only the owning CPU writes it**; every other CPU
    /// reads. Starts at 0, so `seq < head` is half of §2.4's validity test.
    head: AtomicU64,
    slots: [Slot; SHARD_RECORDS],
}

pub const SHARD_RECORDS: usize = 512;      // 128 KiB per CPU
```

One shard per CPU. cpu0's is a `static` in `.bss`, because `log!` runs before the
heap exists and before `PERCPU_READY`. There is no boot-shard-to-cpu0-shard
handoff, because cpu0's shard *is* the boot shard — the `boot` label in today's
prefix becomes `flags` bit 0 and the renderer prints the same word.

**Every AP's shard is allocated by the BSP in `percpu::alloc_percpu`**
(`arch/percpu.rs:262`), beside `alloc_idle_stack` and `alloc_ist1_stack`, and
reached through a pointer in `PerCpu`. **Not in `percpu::init_ap`**, which an
earlier draft of this section said: `init_ap` (`percpu.rs:438`) calls
`control_regs::init` → `self_check` → `log!` and `fpu::log_state`, so an AP
whose shard were allocated there would log into a shard that did not exist yet,
and the only candidate — cpu0's — is one another CPU is writing. The whole
`PerCpu` is already BSP-allocated before the AP executes an instruction
(`percpu.rs:425`, GS base set by the trampoline), so allocating the shard there
closes the window rather than narrowing it. `control_regs::init_cr0`, which does
run before `init_ap`, logs only under the `control-regs-bench` feature; that is
the whole of the pre-`init_ap` exposure and it goes with this placement.

**Why 512, re-derived — the first derivation measured the wrong thing and got
the wrong number.** It said "the measured worst cpu0 boot in `specs/metal-logs/`
is 257 records". **CONFIRMED false**: over the eighteen committed logs the worst
cpu0 count is **1,673** (`2026-08-08-audio-wake/2026-08-08-150958-heartbeat.log`)
and the worst AP is **1,443** (cpu3, same log) — that log is an instrumented
build whose `heartbeat:` and `i8042:` probes are 3,390 of its 4,705 lines, and
excluding it the worst cpu0 is 325 and the worst AP is 214. Neither number is
the one the shard has to be sized against.

**The quantity that sizes cpu0's shard is records emitted before a reader
exists**, and the proxy for it is records up to `Boot: complete`, since `logd`
is first in `[boot] start` and userland's first line follows it. **CONFIRMED,
all eighteen logs, cpu0 and `boot` records up to and including `Boot: complete`:
184–186.** It is 185 in fifteen of the eighteen; the spread is three records
wide and does not move with the instrumented build, because the instrumentation
starts after boot. 512 is that with 2.7× of headroom.

Every other shard has `klogd` runnable within a scheduler pass, so no AP shard
has to hold a boot. One constant rather than two: the saving from giving APs 128
slots is 0.7 MiB and the cost is a runtime mask, and simplicity wins that trade.

**What 512 is against today's ring, both ways.** The current ring is one global
64 KiB byte buffer; at the corpus's 89.4-byte mean line that is **733 records for
the whole machine**. So at the shipped eight CPUs this design holds **5.6× more**
machine-wide (4,096 records) and **0.70× as much for one flooding CPU** (512
against 733). The second number is the one that matters for
`rflags-tf-log-flood` — measured at 56–58 twenty-five-line reports in a
five-second boot, so ~1,425 lines — and it says plainly that a single-CPU flood
loses its oldest records slightly sooner than today. What it gains in exchange is
that it loses *only its own*, and that §2.6 makes the loss exact. Recorded as a
trade rather than a win.

**Memory.** 128 KiB per CPU; at the shipped `sched::MAX_CPUS = 8`, **1 MiB**,
against today's single 64 KiB ring. At the 128-core target root `CLAUDE.md` sets
it is 16 MiB, which is 0.2% of an 8 GiB machine; the escape, if that is ever
judged too much, is to scale `SHARD_RECORDS` down above 16 CPUs, and it is one
line. Named so the number is a decision and not a discovery.

**`MAX_CPUS` is declared three times in this kernel** — `sched/mod.rs:19`,
`trace.rs:41`, `shootdown.rs:34`, all `8`. The shard array uses `sched::MAX_CPUS`
and does not add a fourth. The duplication is filed as an issue by L8 and is not
fixed here.

### 2.3 Emission

```rust
/// The only producer. There is no byte-oriented entry point.
pub fn emit(level: Level, args: core::fmt::Arguments);
```

`log!`, `alert!` and `boot_phase!` expand to exactly this. Steps:

1. **Format into a stack buffer.** `MessageBuf` is 224 bytes plus a counter, on
   the caller's stack, implementing `core::fmt::Write`; overflow increments
   `elided` and writes nothing. Formatting is *outside* every critical section —
   today it happens inside `SerialWriter`, which is at least a lock the code
   claims to hold.
   Stack budget: the smallest kernel stack in the machine is IST1 and the idle
   stack, both **16384** bytes (`percpu.rs:246`, `:204`), and 224 bytes is 1.4%
   of one. The 512-byte drain buffers that used to live on those stacks go away
   entirely (§8).
2. **Reserve, and this is the one step that must not be interruptible by the
   scheduler.** `let (shard, seq) = reserve();` — read this CPU's shard pointer
   from `gs:`, then one **non-`lock`-prefixed `xadd`** on `shard.head`, behind
   `arch::percpu_fetch_add`, documented as *interrupt-atomic, not SMP-atomic;
   sound only for a counter one CPU writes*. **The two instructions are bracketed
   by `pushfq`/`cli`/`popfq` and nothing else is inside the bracket** — §2.3a
   says why the bracket is not optional and why it is only three instructions
   wide.
3. **Write the body** into `shard.slots[seq % SHARD_RECORDS]` — plain stores,
   outside the bracket, into the shard step 2 named and never into "this CPU's".
4. **Commit and read the waiter flag**: `commit_and_signal(shard, seq)` —
   `slot.seq.store(seq, Release)`, `fence(SeqCst)`, `LOG_WAITER.load(Relaxed)`,
   and on `true` the swap that admits one poster per park (§2.5, §2.6a).
5. **Post the wake, only in `Drain::Thread` mode and only when step 4 returned
   `true`**: `wake_direct` to `klogd`, inside a flags bracket of its own. Not
   once per record — once per park.

**No lock and no locked RMW in `Drain::Thread`**, which is every mode after
`scheduler::init`. The path loses today's `compare_exchange_weak` and keeps a
three-instruction flags bracket where today's covers the whole append; it gains
one `SeqCst` fence and one relaxed load. L1 measures the net for the ring and L3
for the fence, which arrives with the wake (§9.6).

**`Drain::Inline` is not that path and must not be described as if it were.**
Inline `emit` calls `BackendGuard::lock()` (`serial.rs:97`), which is
`save_and_cli()` **plus a `compare_exchange_weak` spin** — so during boot every
record pays one locked RMW and a synchronous backend write. That is one CPU and
185 records (§2.2), so the cost is nil; the claim is what has to be accurate.
§9.6's RMW budget counts both modes separately.

**And Inline `emit` is not reentrant, where Thread `emit` is.** `BackendGuard`
clears `IF` before it spins, so an IRQ cannot re-enter it, but an exception can —
and a Ring 0 fault during boot whose handler logs would spin on a lock its own
CPU holds. That is today's shape exactly (`RingGuard::lock` has the same `cli`
and the same CAS), and it is covered by the same mechanism: a Ring 0 fault is
fatal, so it reaches `panic_flush`, which waits out a live `BackendGuard` holder
and then bypasses a wedged one. Stated because §2.4's fourth property is about
the *shard* and does not extend to the inline backend.

### 2.3a Why the reservation is bracketed, and why three instructions is enough

A non-`lock`-prefixed `xadd` is atomic against an interrupt on its own CPU —
instructions retire whole — and it is **not** atomic against another CPU. The
design is sound only while the CPU executing the `xadd` is the shard's owner, and
**nothing in this kernel guarantees that without the bracket.**

**CONFIRMED**: work stealing is on (`sched/driver.rs:296`, `steal: true`) and a
task migrates between CPUs (`toyos-sched/src/cpu.rs:476`, `task.migrate(self.id,
dst, now)`); `Hw::switch` swaps the per-context preempt depth (`hw.rs:173`), so a
kernel context at depth 0 with `IF` set is preemptible and its next run may be on
another CPU. A kernel thread is exactly that context, and it logs — **and under
the ruled order L3 is what introduces the first one**, `klogd`, with completions'
C6 adding `usbd` and `iod` on the machinery L3 builds (§4.3, §12.6). So without
the bracket:

- a preemption **between the `gs:` read and the `xadd`** puts the `xadd` on a CPU
  that does not own the shard, and two CPUs performing a non-atomic
  read-modify-write on one word lose an update. Two records then hold the same
  `seq`, share a slot, and one overwrites the other. §9.1's conservation law is
  what would eventually catch it; a race that needs a preemption inside a
  two-instruction window is not something a suite catches on purpose.
- a preemption **after the `xadd`** is harmless, and that is why the bracket ends
  there: the sequence number is already exclusively owned, and step 3 writes into
  the shard step 2 named rather than into whatever `gs:` says now. Only the `cpu`
  field can then be stale, and it is stamped inside the bracket.

**Why the flags bracket and not `preempt::disable()`.** `preempt::disable`
(`preempt.rs:93`) is `lock add dword ptr gs:[240], 1` and `enable` is a `lock
sub` plus a `need_resched` poll — **two locked RMWs per record**, which is the
cost §1.4 exists to avoid, to buy a property `cli` buys with `pushfq`/`popfq`.
The flags bracket is also what every `log!` caller inside a syscall already has
(`IF` is clear for the whole of every syscall), so on the dominant path the
`popfq` restores a flag that was already clear. **`emit` has a second bracket of
the same kind and it is not this one**: the wake post sits after the commit
store, outside this bracket by construction, and §2.6a is where it is argued.

**W4's shim is sound for this shape and only this shape** (§2.5): loom models the
`xadd` as a real `fetch_add`, which is strictly stronger, and the bracket is what
makes "no other CPU writes `head`" true rather than hopeful.

### 2.4 What makes atomicity unrepresentable to violate

Four properties, each structural rather than checked:

- **There is no way to append bytes.** `emit` is the module's only public
  producer and it takes `fmt::Arguments`. `Shard` has no method that takes a
  `&[u8]`. A caller cannot write half a record because the smallest thing the
  module accepts is a whole one. This is what deletes `write_chunk`,
  `write_chunk_blocking` and `SerialWriter`'s spill-on-overflow, which is the
  mechanism every recorded splice went through.
- **The validity word and the identity word are the same word.** A slot is
  readable as record `s` exactly when **`s < head` and `slot.seq == s`.** There is
  no separate "valid" flag that could disagree, and no even/odd seqlock
  convention to get backwards.

  **`slot.seq == s` alone is not a total test and an earlier draft of this
  section said it was.** Slots are zero at boot — cpu0's shard is `.bss` and an
  AP's is `alloc_zeroed` — so slot 0 of a shard that has never been written holds
  `seq == 0`, which *equals* sequence number 0. A reader checking only equality
  would read an all-zero record as record 0 of every shard on every boot. `head`
  starts at 0 and only grows, so `s < head` says "this sequence number was
  reserved" and costs one comparison against a word the reader loads anyway
  (§2.6). It is the load-bearing half.

  **A stale value can never be mistaken for a live one, and that is what kills
  ABA.** Slot `j` only ever holds sequence numbers `≡ j (mod SHARD_RECORDS)`, the
  sequence is a `u64` that never wraps in any reachable lifetime, and the test is
  *exact equality* rather than a range. So a slot carrying an older generation's
  number fails against every `s` the reader can ask for, and there is no value the
  reader accepts that was not written for that `s`.
- **Exactly one store publishes**, and it is the last one. A record is visible
  whole or not at all, and the reader's re-check of the same word after copying
  the body is total: the only thing that can change a slot's body is a writer
  reserving `s + SHARD_RECORDS`, and that writer's own commit store changes
  `slot.seq` away from `s`.

  **The body copy is a volatile byte copy, and that is a requirement rather than
  a style.** Reading 248 bytes that a writer may concurrently be storing into is a
  data race in Rust's model whatever x86 does about it; the re-check makes the
  *result* sound and does not make the *access* defined. `read_volatile` through
  the `UnsafeCell` is the form, loom does not see this and neither does any guest
  test, and the reason goes in the doc comment on the copy.
- **Reentrancy cannot collide.** An exception or IRQ that logs while a record is
  being written on the same CPU takes its own reservation, so the two never share
  a slot. The nested record commits first and the outer commits after; §3.2 says
  what each reader does with that.

  **A reservation that is lapped is abandoned rather than committed.** If the
  nested run emits `SHARD_RECORDS` or more records before the outer resumes — a
  `#DB` storm inside one record's body write is the reachable case
  (`rflags-tf-log-flood`) — the outer's slot has been recycled, and writing its
  body would destroy a live newer record for every reader that had not yet taken
  it. So step 3 re-reads `head` first and skips the body and the commit when
  `head - seq >= SHARD_RECORDS`; one load and one comparison, no atomic. Without
  it nothing is *unsound* — the readers' exact-equality test rejects the stale
  commit — but a record that was committed becomes uncommitted, which is the one
  transition the rest of this section says cannot happen.

### 2.5 Memory-ordering obligations, and the loom models

x86 TSO gives every load acquire and every store release semantics, so a missing
edge is invisible to every guest test. `kernel-loom` compiles the real file a
second time against loom's atomics, and the models below are L1's and L3's
deliverables.

**The mechanism dictates the file split, and W3 as first drafted could not be
built.** `kernel-loom/src/lib.rs:63` reaches a kernel file by `#[path]`, and it
works only because `sync.rs` and `shootdown.rs` name almost nothing outside
themselves — compl §16.1 counts four shimmed items in total (`cell::UnsafeCell`,
`preempt::{disable,enable}`, `arch::tlb::poll`, `log!`). The first draft put the
wake edge in `emit`, which lives in `mod.rs` and names `core::fmt::Arguments`,
`clock::nanos_since_boot` and the wake post — three surfaces no shim
reaches, so `log_wake.rs` would have had nothing to compile. **Both fences and
`LOG_WAITER` therefore live in `shard.rs` with the ring**, behind two functions
that return a `bool` and touch no subject:

```rust
// kernel/src/log/shard.rs — the whole modelled surface
impl Shard { fn reserve(&self) -> u64; fn commit(&self, seq: u64); }
/// commit, fence, read the flag. True means "a reader is parked; post to it".
pub fn commit_and_signal(shard: &Shard, seq: u64) -> bool;
/// set the flag, fence, re-scan for a committed record. True means "do not park".
pub fn arm_waiter(cursor: &Cursor) -> bool;
```

The caller does the post on a `true`, and never the ordering. The shimmed set for
`shard.rs` is then `UnsafeCell` and `percpu_fetch_add`, which is smaller than
`sync.rs`'s. **The layout requirement is stated here and honoured at L1 even
though the two functions arrive at L3** (§2.6a): if `shard.rs` were allowed to
grow a dependency on a subject, W3 could not be modelled at all, and that is
exactly the mistake the paragraph above records.

| obligation | shape | model | when |
|---|---|---|---|
| **W1** publication | body stores → `seq.store(Release)`; reader's `seq.load(Acquire) == s` implies the body is visible | `kernel-loom/tests/log_record.rs` | L1 |
| **W2** recycle detection | reader: `load(Acquire)`, copy, `fence(Acquire)`, `load(Relaxed)`, compare. The second load must not be hoisted above the copy | same | L1 |
| **W2b** the lapped writer | a writer whose reservation was lapped by `SHARD_RECORDS` re-reads `head` and abandons; a reader mid-copy of the newer record must not observe the older `seq` (§2.4) | same | L1 |
| **W3** the wake edge | `commit_and_signal`: `seq.store(Release); fence(SeqCst); LOG_WAITER.load(Relaxed)` against `arm_waiter`: `LOG_WAITER.store(true, Relaxed); fence(SeqCst); rescan for a committed record; park`. **Invariant: no committed record is left with a parked reader.** Store-buffer shaped, so TSO hides the missing fence and only loom sees it. Carries its own negative case — either fence removed behind a `cfg`, and the model must red — because a model that has never failed proves nothing | `kernel-loom/tests/log_wake.rs` | L3 |
| **W4** the reservation | `head` is written by one CPU through inline asm and read by others as an `AtomicU64` | shimmed | L1 |

**W3 is L3's, and it is the one obligation the wake's own correctness rests on.**
The producer-side edge is real in this branch — `emit` reads `LOG_WAITER` after
its commit and posts (§2.6a) — so the store-buffer pair exists in code L3 writes
and x86 cannot fail it on purpose. A tree that drops either fence loses a wake
against a parked `klogd` and goes quiet with a committed record behind it; loom
is the only instrument that sees it, which is why the model ships in the same
chunk as the code.

**W3's rescan is over committed records and never over `head`, and that is a
liveness property rather than a taste.** A reader that re-scans `head[i]` and
declines to park whenever `head[i] > next[i]` never parks at all on a shard
holding an abandoned reservation (§3.1) — `drain_ordered` returns nothing for
that shard and `head` does not move on a quiet machine, so `klogd` spins a CPU at
100% until 512 further records arrive, which on a quiet machine is never. The
predicate is the same one `drain_ordered` uses: *is there a committed record at
`next[i]`*. Eight loads either way.

**W4's shim, and why it is sound.** Loom cannot model inline asm, so
`percpu_fetch_add` is shimmed to a real `fetch_add`. That is a **strictly
stronger** model: the only behaviour the real instruction has that `fetch_add`
does not is non-atomicity against another CPU's write, and §2.3a's bracket is
what makes "no other CPU writes `head`" true. So every interleaving the real code
can produce, loom explores, and the shim cannot hide a race. The argument is
stated in the model file, because a shim whose direction is not argued is a model
that proves nothing — and it is stated *with* its precondition, because without
the bracket the shim is the thing hiding the bug.

### 2.6 Drops are a subtraction, not a counter

A reader holding `next[i]` for shard `i` computes, per call:

```
oldest_readable = head[i].saturating_sub(SHARD_RECORDS)
lost[i]        += oldest_readable.saturating_sub(next[i])
next[i]         = max(next[i], oldest_readable)
```

**No producer-side drop counter exists.** Loss is exact, per reader, derived from
the two numbers that already have to be right, and no counter can drift from the
ring. `DROPPED_BYTES`, `FILE_DROPPED`, `take_drop_marker`, `take_file_drops` and
`DROP_MARKER_MAX` all go (§8). It also removes the last `fetch_add` in the
module, which today runs on the overflow path.

### 2.6a The wake — `emit` posts it, and the primitive was never missing

**`emit` may not take a lock, and an ordinary completion post does.** compl §5.2:
*"a watch is a node the waiter lends to the object, so a post is a walk of a list
under the object's own leaf lock."* `log!` runs inside `sync.rs`, inside IRQ
handlers, inside the scheduler and inside every syscall's locked region, so an
`emit` that acquired a leaf `Lock` would (a) put `preempt::disable`'s two locked
RMWs on every record and (b) deadlock the first time anything on that lock's own
path logged. What the log needs is the *degenerate* case of that shape — one
waiter, no list, the rendezvous-word CAS and nothing else.

**That primitive is in the tree, and it is not the completion core's to supply.**
`toyos_sched::waitq::wake_direct` (`toyos-sched/src/waitq.rs:236`), whose own doc
comment is *"join / waitpid / sleep and the local deadline fire: the same claim
CAS, without a queue. `true` if this caller owns the wake."* **CONFIRMED**
2026-08-10 by reading every line its `Claim::Parked` arm reaches:

| step | what it is | takes a lock? |
|---|---|---|
| `shared.claim_wake()` | `task.rs:412` — a CAS loop on the packed state word, `Blocked → WakeQueued` | no |
| `shared.wake_node().claim()` | `mailbox.rs:154` — `in_flight.swap(true, AcqRel)` on a node embedded in the task | no |
| `CpuHandle::post` | `cpu.rs:1141` → `MailboxProducer::post` (`mailbox.rs:255`): a plain store into the node's slot, one `tail.swap`, one release store | no |
| `Doorbell::ring` | `mailbox.rs:457` — `bits.fetch_or(KICK_PENDING)`, returning whether the IPI is owed | no |
| `Kicker::kick` | `kernel/src/hw.rs:60` → `apic::kick_cpu` (`apic.rs:89`): one x2APIC ICR `wrmsr`, and only on `Kick::Send` | no |

No list, no leaf lock, no allocation. **And the kernel already wraps it**:
`scheduler::wake_sched` (`kernel/src/scheduler.rs:264`) is
`preempt_off(|p| wake_direct(…))`, serving `waitpid`, `thread_join` and the
panic-recovery notify. `sched/waitqs.rs`'s module doc states the idiom `klogd`
joins: a park bucket is *"never woken as a queue: `wake_direct` claims the task's
own rendezvous word and the queue node is cleaned up by the waiter's own
`Registration`."* `klogd` is a fourth waiter of a shape this kernel has, not a
mechanism this branch invents. **Posting from an IRQ handler, from inside a lock
or from inside a pass is the mailbox's ordinary case**, not an exemption `emit`
needs: the producer half is the `Sync` face of a CPU (`cpu.rs:1104`) and never
touches the `!Sync` `CpuSched`, and a wake that lands on the posting CPU's own
queue is what park-before-switch already relies on (`driver.rs:20-22`).

**The witness `wake_direct` asks for is free.** `PreemptGuard` is
`pub unsafe trait PreemptGuard {}` (`mailbox.rs:60`) — an empty marker trait, and
the cost read into it belongs to a witness *type*'s constructors. `PreemptOff`
(`driver.rs:52`) is one such type, and `preempt_off` raises the preempt count:
`lock add` plus `lock sub` (`preempt.rs:90`, `:112`) and a `need_resched` poll
that can reach `do_preempt`, which is a scheduling pass. `emit` may have neither.

**So the post takes a flags bracket of its own, and §2.3a's is not it.** That
bracket ends at the `xadd` on purpose (§2.3a's second bullet) and the post comes
after the commit store, so the wake is *not* inside a bracket that already
exists — it gets a second one of the same kind and by the same argument.
`IrqGuard::close()` (`kernel/src/hw.rs:42`) is `pushfq`/`pop`/`cli` with
`push`/`popfq` on drop: no locked RMW, and on the dominant path `IF` is already
clear.

The witness is a type beside `PreemptOff` in `sched/driver.rs`, constructible
only by that bracket, and its `unsafe impl PreemptGuard` carries this SAFETY
argument: **preemption in this kernel is delivered at an interrupt.**
`do_preempt` has exactly three callers — the LAPIC timer
(`arch/idt/timer.rs:101`), the exit-to-user epilogue (`arch/idt/mod.rs:370`,
which `sti`s before it calls) and `preempt::enable`'s poll (`preempt.rs:126`) —
**CONFIRMED**. With `IF` masked the first two are unreachable, and the bracketed
region calls `wake_direct` and nothing else, so it reaches neither
`preempt::enable` nor a voluntary pass. A voluntary pass is the one way to be
descheduled with `IF` clear, which is why the witness has no constructor but the
bracket. The scheduler core grants the same impl to an IRQ context in its own
loom model (`toyos-sched/loom/src/model.rs:105`) and the trait's own SAFETY
paragraph names one.

**`LOG_WAITER` is the gate, and it is what keeps the record path free of locked
RMWs.** Without it every record would pay `claim_wake`'s CAS. §2.5's two
functions, in `shard.rs` with the ring:

- `commit_and_signal`: `slot.seq.store(seq, Release)`, `fence(SeqCst)`,
  `LOG_WAITER.load(Relaxed)`, and **only** on `true` the
  `LOG_WAITER.swap(false, AcqRel)` that admits exactly one poster per park. It
  returns whether this caller owns the post; the caller does the post, and never
  the ordering.
- `arm_waiter`: `LOG_WAITER.store(true, Relaxed)`, `fence(SeqCst)`, rescan for a
  committed record. `true` means do not park.

**Both fences are load-bearing.** This is a store-buffer shape, and x86's one
permitted reordering is store-then-load — exactly the producer's commit-then-read
and the waiter's arm-then-scan — so without them both sides can miss and a
committed record is left under a parked reader. Buying that ordering with an
RMW instead, by reading `LOG_WAITER` with an unconditional `swap`, is §1.4's
price on every record. W3 is the model, and it is L3's (§2.5).

**`klogd` registers before it arms, and that ordering is the lost wake's other
half.** The body is `drain_ordered`, then `prepare_wait(park_lot())`
(`scheduler.rs:170`, `:201`), then `arm_waiter`, then `block_on` (`:181`) or a
cancel. Registration moves the word to `Committing` (`waitq.rs:128`), so a
producer that wins the swap from that point on gets `Claim::PrePark` and
`klogd`'s own commit refuses to park. Arming *before* registering leaves a window
where the producer claims a still-`Running` `klogd`, takes `Claim::Lost`, drops
the wake, and `klogd` parks on a committed record — which is the window
`prepare_wait`'s own doc says registering first exists to close, with the rescan
as the re-check.

**`emit` finds `klogd` through a static, never the process table.** `wake_task`'s
`process::thread_sched` lookup takes a lock; L3 publishes `klogd`'s
`Arc<KShared>` once at spawn, leaked and reached through an `AtomicPtr` read
`Acquire` — the shape `CPUS` already has in the same file (`driver.rs:69`, read
at `:78`, set once from a leaked `Box`). Null means "no `klogd` yet", which is
`Drain::Inline`'s state and needs no branch of its own.

**The RMW budget, counted from the code rather than measured.** Per record in
`Drain::Thread`: **no locked RMW**, one `fence(SeqCst)`, one relaxed load. Per
`klogd` park: **five** — the `LOG_WAITER` swap, `claim_wake`'s CAS, the node's
`in_flight` swap, the mailbox's `tail.swap` and the doorbell's `fetch_or` — plus
one `wrmsr` when the doorbell says `Kick::Send`. The fallback this section
carried until 2026-08-10 was cheaper by one fence per record and dearer
everywhere else: a relaxed store per record, a relaxed load per pass on **every**
CPU in `drain_irqs`, and a park wake through `waitqs::wake_one`, which is
`preempt_off`'s two locked RMWs plus the queue's leaf lock and a list pop
(`waitq.rs:154`) *before* the same four. §9.6 states what is owed: the fence
arrives at L3 and its cost is L3's to measure.

**And the pre-`hlt` condition goes with the fallback.** A commit makes `klogd`
runnable there and then, including a record committed inside the very pass that
is about to halt: `execute`'s `Idle` arm opens with `doorbell().kick_pending()` and
`!mailbox_is_empty()` (`driver.rs:474-478`, **CONFIRMED**), the doorbell
publishes `SLEEPING` before the final mailbox check so a post after it kicks
(`mailbox.rs:487`), and `toyos-sched`'s Invariant T refuses the halt while
anything is runnable. `log_ring::has_pending` (`:523`) is therefore **deleted**
at L3 as the first draft planned, and `log-ring-flushes-one-line-behind` closes
on one mechanism (§8.1, §8.2).

**Why this section carried a fallback at all, recorded because the failure mode
repeats.** The primitive was looked for in the completion spec's *description* of
a post rather than in the scheduler that already had one — reasoning about the
tree from a document instead of grepping it, which is the same mistake in the
same section as the wrong-CPU reservation §2.3a exists to fix.

## 3. The read surface

### 3.1 Two readers, because a dying machine and a live one want different rules

```rust
// kernel/src/log/read.rs
/// In sequence order per shard, merged by `at_ns`. Stops a shard at its first
/// uncommitted record. For klogd and for SYS_LOG_READ.
pub fn drain_ordered(cursor: &mut Cursor, out: &mut impl RecordSink) -> usize;

/// Every committed record in the window, merged by `at_ns`, skipping
/// uncommitted slots. Takes no lock and never blocks. For the panic console and
/// for Ctrl+Alt+D.
pub fn snapshot_committed(from: Instant, to: Instant, out: &mut impl RecordSink) -> usize;
```

**Why the second exists, and it is not a mode flag.** A nested writer — a #DF or
a #MC logging while an ordinary record was mid-write on the same CPU — commits
*before* the record it interrupted. A streaming reader must block at the outer
record or it would emit out of order; the outer commits microseconds later and
the block clears. But when the interrupting handler is the one that halts the
machine, the outer record never commits, and a blocking reader would then never
reach the abort's own line — the one line that matters. A machine that is halting
has no "later", so its reader has nothing to preserve order for. Two functions,
two names, each correct for its caller. `snapshot_committed` is also what makes
`log_ring::drain_unlocked`'s *"the indices may then be torn"* unsafe contract
disappear: a torn slot is **detected** by `slot.seq`, not tolerated.

**`drain_ordered` blocks a shard, never the stream.** A shard stopped at an
uncommitted record does not stop the merge: the other shards keep flowing and
their records are emitted. So the merge is in `at_ns` order *among what it
emits*, and a blocked shard's records arrive later than records with larger
timestamps once it unblocks. That is the only ordering this design does not
give, it is bounded by the block's duration (microseconds for a nested writer,
unbounded for an abandoned reservation), and the alternative — stalling every
shard on the slowest — is what turns one wedged CPU into a silent machine.
`logd` renders in arrival order and the timestamp is in every line, so a reader
sorts if it cares.

**Both readers are allocation-free `SHARD_RECORDS`-bounded merges**, because the
panic path calls one of them and can allocate nothing: `MAX_LOG_SHARDS` cursors
on the stack (8 × 16 bytes at the shipped count), pick the smallest `at_ns`,
advance. `snapshot_committed` walks each shard from `head - SHARD_RECORDS` to
`head`, which is at most 4,096 slot reads machine-wide.

On a live machine a shard can only be blocked by an abandoned reservation, and
the drop-oldest window unblocks it as soon as that shard writes
`SHARD_RECORDS` more records. On a machine quiet enough that it never does, the
records behind it are lost to the streaming reader and present to the snapshot
one. Stated rather than papered over — and §2.5's W3 note is what stops the
streaming reader burning a CPU while it waits.

### 3.2 The cursor syscall

```rust
/// L0 computed it against the merged tree; §3.4 says what made a literal safe
/// to write down here and what it would have been unsafe to predict.
pub const SYS_LOG_READ: u64 = 114;

/// Per-reader state. The kernel holds none.
#[repr(C)]
pub struct LogCursor {
    /// Out: how many shards the machine has. A caller passes a zeroed cursor the
    /// first time and reads it back.
    pub shards: u32,
    pub _pad: u32,
    /// In/out: cumulative records this cursor never saw because they were
    /// overwritten. The kernel adds; the caller never has to remember to ask.
    pub lost: u64,
    /// In/out: the next sequence wanted from each shard.
    pub next: [u64; MAX_LOG_SHARDS],
}

/// Returns records written into `out`, which is `n * RECORD_BYTES` bytes of
/// `LogRecord`, merged by `at_ns`.
pub fn log_read(cursor: &mut LogCursor, out: &mut [u8]) -> Result<usize, SyscallError>;
```

- **The kernel keeps no per-reader state at all.** No object, no handle
  lifecycle, no cursor to leak or go stale, and a second reader costs nothing.
  That is `specs/introspection-plan.md` §3.3's argument and it is adopted whole.
- **Fixed stride, not packed.** The kernel copies whole 256-byte records and
  userland indexes by shift. At the measured 100.2-byte mean payload (§2.1) the
  waste is real and irrelevant — a few hundred records per second is under
  100 KB/s — and packing would put length arithmetic in the kernel for nothing.
- **`lost` lives in the cursor**, so a reader that ignores loss has to actively
  ignore a field it is already passing.
- **The stream is not consumed.** Two readers see the same records. `logd` and a
  `log-follow` tool coexist with no coordination.
- **Non-blocking, and the wait beside it is today's mechanism rather than the one
  that does not exist yet.** `SYS_LOG_READ` returns 0 and the caller arms on a
  readiness source and parks. Under the ruled order that source is a **ninth
  `io_uring::Source`** — `Source::Log`, beside `Keyboard`, `Mouse`, `Network`,
  `VirtioSound`, `Hda` and endowment's three object-shaped kinds `Port`,
  `PipeReadable` and `PipeWritable`, with the same per-source
  `IO_URING_WATCHERS` static the first five already have (`keyboard.rs:17`,
  `mouse.rs:19`, `net.rs:41`, `hda.rs:220`, `virtio_sound.rs:270` —
  **CONFIRMED** 2026-08-11, all five unmoved) and posted by
  `klogd` after each drain batch. **Not by `emit`**, and the reason is §2.6a's
  own: each of those statics is a `Lock<Vec<RingId>>` and the post clones it
  under the lock (`keyboard.rs:71` — **CONFIRMED** 2026-08-11), which is the one
  thing `emit` may not do. `klogd` is the context that has just observed
  committed records and may take a lock, and posting there costs one wake per
  batch rather than one per record. L4 adds it; compl C3 folds all six statics
  into its one watch list and its §19 deletes them together. **Adding a
  sixth instance of a mechanism that is about to be unified is the honest cost of
  landing first**, and it is one static and one match arm.

**`MAX_LOG_SHARDS` is `MAX_CPUS`** and the cursor is 1 KiB at the shipped 8. A
caller passing a smaller buffer than `shards` requires gets `InvalidArgument` —
untrusted input that cannot be satisfied, never a truncation.

**Authority.** Reading the whole machine's kernel log is authority, so it rides
a right rather than being ambient: `Rights::LOG` on the `SysCap` the endowment
architecture already defines, alongside `DEVICE`, `RT` and `MANAGE`
(`handle.rs:65-83` — **CONFIRMED** 2026-08-11, nine bits in a `Rights(u32)`, so
`LOG` is `1 << 9` and `ALL` becomes `0x3ff`). **The manifest spelling is
`syscap = ["logread"]`** — one more row in `toyos_manifest`'s `SYSCAP_RIGHTS`,
beside `rt`, `device` and `dup`. It is not a `bool` key: `realtime` is gone and
the table it became refuses a name it does not carry (§0.0).

**Which manifest, and there are eleven of them.** `logread` is not one row in
one file: `system.toml` gives it to `logd`; `console/system.toml` gives it to
`console`; and the **six** test configs that carry a `test-runner` row
(`tests/{metalcase,netcase,sshdcase,testcases,doomcase,doommusiccase}/system.toml`)
give it to `test-runner`, which is the only manifest program in a test image.
**CONFIRMED** 2026-08-11 — neither `console` nor `test-runner` is a `[programs]`
key in the shipped `system.toml`, so §9.5's `one_console_holder` walks every
manifest in the tree and not just the first. §5.1a is the whole table.

**And a test *binary* does not inherit it.** endow §6.7a: `test-runner` passes
its whole **namespace** to every binary it spawns, and `logread` is not a
namespace entry — it is a `SysCap` dup, exactly like `realtime`, which the estate
does not hand down either. So either `test-runner` `Command::endow`s a
`Rights::LOG`-only dup alongside the namespace, or **the gate that reads the log
runs inside `test-runner` itself**, which already holds the right from its
manifest row. §9.5 takes the second: it is one fewer mechanism and L4's gate is
`test-runner`'s own.

**And the silent-drop hazard this paragraph used to carry is gone.**
`ProgramConfig` (`src/build.rs:44`) still has no `deny_unknown_fields`, so a
*key* it does not declare is still parsed and discarded — but `logread` is no
longer a key. It is a **name inside `syscap`**, and `syscap_rights`
(`toyos-manifest/src/lib.rs:63`) answers `` `{name}` is not a syscap right ``
for one it does not carry: `src/build.rs` stops the build and `/bin/init`
panics by name. The gate below still reads the *parsed* manifest rather than the
file's text, because that is the honest thing for it to read, but it is no
longer the only thing between `logd` and booting with no authority.

### 3.3 The one formatter

`LogRecord`'s rendering to a line lives in `toyos-abi` beside the type, so the
kernel's serial sink, the panel, `logd` and any diagnostic tool produce
byte-identical text from one implementation. Today `log!` bakes the prefix into
the ring bytes and every consumer inherits whatever it produced; after this the
prefix is synthesised, which is what lets `logd` render `[2033-03-07 09:14:26.123
cpu0 tid=3]` into `/log` while the panel renders `[0.123 cpu0 tid=3]` into 80
columns, from the same record.

### 3.4 Syscall numbering, and the three specs that collide over it

Baseline on this tree: **78 syscall constants, highest 98** — **CONFIRMED**
2026-08-09 (`grep -cE '^pub const SYS_[A-Z_0-9]+: u64 = [0-9]+;'
toyos-abi/src/syscall.rs`).

| block | owner |
|---|---|
| 99–112 | `capability-endowment-spec.md` §3.1, fourteen calls |
| 113 | reserved by that spec for `SYS_PORT_REARM` |
| 114–115 | free by that spec's own §13 |
| **one number, unnumbered** | **`SYS_LOG_READ`, this spec** — first, under the ruled order |
| **one number, unnumbered** | `completion-architecture-spec.md` §14.2 — `SYS_SLEEP_UNTIL`, replacing a retired `SYS_NANOSLEEP` |
| after those | what is left of `specs/introspection-plan.md` |

**The first draft of this table gave the completion spec 116–127 and this one
128, and both are wrong.** Its §14.2 was rewritten by its own adversarial review:
it now takes **exactly one number**, refuses to write it down, and says why in
terms that apply verbatim here — *"a literal one clause away from the word
'asserts' is what an implementer hard-codes."* This spec had that literal twice,
once in §3.2's code block. Both are gone. Neither block is contiguous either: 114
and 115 are free, so the two remaining numbers are 114 and 115 in merge order.
**The order change swaps which is which** — this branch merges first, so it takes
the lower — and that is exactly why neither spec writes a literal: whichever
lands first takes what is clean, and a landing on `main` moves them both anyway.

**`specs/introspection-plan.md` is wrong on today's tree and must be re-based.**
It allocates `SYS_QUERY = 97`, `SYS_LOG_READ = 98` and `SYS_DISK_ADOPT = 99`
(`:78`, `:414`, `:694`, restated at `:824`). 97 and 98 are
`SYS_DEVICE_REG_READ`/`SYS_DEVICE_REG_WRITE`, allocated after that plan was
written; 99 is `SYS_ENDOWMENTS` on the endowment branch. Its `SYS_LOG_READ` is
**superseded by this one** and its `LogCursor` — a byte cursor with a `Span` ring
tracking which bytes are kernel-origin, `:466-484` — is **deleted by move 3 of
this design**: userland output never enters the kernel ring, so there is no
origin to track and the loop it exists to prevent is unrepresentable for a
different and better reason. Its `SYS_QUERY` and `SYS_DISK_ADOPT` re-base to
whatever is clean when that plan is next opened, which is not a number this spec
writes either. L8 edits that plan to say so.

**This spec allocates nothing and predicts nothing.** L0 computes the first clean
number from the merged `toyos-abi` and asserts only that its choice is clean —
never `assert_eq!` against a literal, which is wrong in the two live cases the
completion spec names: `main` moves while both branches are open, and endow's
§10.1 option 2 lands its chunks 0+1 as an earlier PR, which leaves 99–112
unclaimed at the moment this branch opens. **CONFIRMED** 2026-08-09 that `main`
has already moved under both, from `19c761e` to `0e48d2e`.

---

## 4. The kernel's console sink, and the three drain modes

### 4.1 The kernel keeps the console and gives up the filesystem

The line is not "the kernel does no I/O". It is **"the kernel never writes a
file"**:

- A **file** write is a filesystem, a page cache, a volume adapter and a device
  round trip — four locks, preemption off, unbounded. That is (a) in §1.2 and it
  leaves the kernel entirely.
- A **console** write is one port-I/O byte at a time to a 16550, or one
  virtio-console descriptor. It is the panic channel and the pre-scheduler
  channel, so it cannot leave the kernel: there is no userland at panic and none
  during boot. It stays, and it is drained by something that can afford it.

This also resolves **half** of an open question in the completion architecture,
and the first draft of this paragraph claimed the whole of it. Its §10 gives
`logd` (a kernel thread) the drain into *both* sinks and its §22 records the
consequence — *"the log sink parks on a dead stick … takes the serial sink with
it"* — as **unresolved**.

- **True of the console, and it is the half that matters on a T14.** The kernel's
  console drainer has no disk in it, so a hung `/log` stick cannot take serial
  with it and "total logging loss with no line saying why" is gone by
  construction.
- **False of the file, and its §12.3's bound question survives with a different
  victim.** Something still parks on a hung stick — after this branch it is
  `/bin/logd`, and after completions' C12 it is `iod`, which carries **every**
  write-back in the machine and not only the log: `SYS_FSYNC`, deferred close
  flushes and page-cache eviction. So the give-up is a policy that has to exist
  and be owned, not one the split deletes. §5.4 is this branch's answer for
  logd's own writes and it does not answer for `iod`; that is compl §12.3's, and
  it is still open.

### 4.2 Three modes, one transition each, and none of them is a fallback

```rust
enum Drain { Inline, Thread }
```

| phase | who drains to the console | why it is the only thing that can |
|---|---|---|
| boot, up to `scheduler::init` (`main.rs:501`) | **`Drain::Inline`** — `emit` writes the record to the backend synchronously, after committing it | there is no scheduler and no thread; one CPU, nothing else running |
| steady state | **`Drain::Thread`** — `klogd`, a kernel thread, made runnable at the commit of the record it will drain, by the producer's own `wake_direct` (§2.6a) | it must run on an idle machine, and a runnable task is what stops a CPU halting (`toyos-sched` Invariant T) — including a record committed inside the pass that was about to halt, which the doorbell's own handshake catches |
| panic and shutdown | `drain_inline()` called directly | `klogd` will never run again |

A fallback is a path taken when another fails. These are phases: exactly one is
active, the transition is a single statement at `scheduler::init`, and it is
logged. `drain_inline` is one function with three callers, not a degradation.

**`Drain::Inline` closes `pre-idle-wedge-says-nothing` completely** — not "says
less", the entry's own wording — because every record reaches the wire as it is
written, for the whole boot. Its cost is the boot log at backend speed during
boot: nothing on the T14 (`has_console()` is false, so the backend discards and
the mode is a branch), nothing measurable under QEMU, and on a machine with a
real 115200-baud UART the boot log's ~40 KB at ~87 µs/byte would be seconds. **L3
measures it on the `metal-sim` profile and, if it is not free, `Drain::Inline` is
gated on `has_console()` and a real-UART boot pays it only when someone is
watching.** The number decides; the design does not pre-empt it.

### 4.3 `klogd`

Body: `loop { drain_ordered(&mut cursor, &mut backend); park(); }`, where `park`
is `prepare_wait(park_lot())`, then `arm_waiter`, then `block_on` or a cancel —
in that order, which is §2.6a's. A spurious wake is legal; the body re-drains and
re-parks.

- **The kernel-thread machinery is L3's, and this is the largest thing the order
  change moved.** The first draft said *"it is compl C6's `logd` kernel thread,
  renamed and narrowed — C6 owns the machinery, this spec owns its body."* With
  completions landing second there is no C6 to inherit from: **CONFIRMED**
  2026-08-09, `kernel/src` contains no kernel-thread machinery of any kind
  (`grep -rn 'kernel thread\|kernel_thread' kernel/src/sched/driver.rs
  kernel/src/loader/start.rs` is empty). So L3 builds it, for one thread:
  - a trampoline beside `process_start`/`thread_start`, which is *simpler* than
    either — no `initial_user_state!`, no `iretq`, no `USER_CS` —
    `alloc_kernel_stack` (`loader/start.rs:17`) already takes one as a parameter;
  - a `ProcessObject` whose address space **is** the kernel address space, which
    means wiring `driver::spawn` (`sched/driver.rs:222`, whose
    `.expect("spawn: task without an address space")` is the thing in the way)
    to `paging::KERNEL`;
  - `sched::dump` naming it, and the recoverable-panic predicate
    (`syscall_rip() != 0 && current_tid().is_some()`) gaining `klogd`'s row — the
    non-recoverable one, below.

  **compl C6 then shrinks to spawning `usbd` and `iod` on machinery that already
  exists**, their two rows in the predicate, and the
  `KernelPayload.address_space: Option<PageTables>` → non-`Option` retype
  (`sched/payload.rs:88`, **CONFIRMED** still `Option` today), which compl §15
  row 12 says nobody else owns. That retype stays C6's: it is only *enabled* by a
  kernel thread naming the kernel address space, and L3 has no second caller to
  justify it.
- **The name is `klogd` from the start and no rename is owed.** compl §12.3's
  "one-word change C6 owes" is discharged before C6 is written: `/bin/logd` is a
  userland program from L6, `sched::dump` names threads, and two things with one
  name in one machine is a collision a dump report cannot survive.
- It never takes a filesystem lock, never touches a block device, and holds only
  `BackendGuard` — for one bounded chunk at a time, exactly as
  `drain_chunk_to_serial` does today, so an IRQs-off window is never longer than
  one chunk.
- **It is not `usbd` and not `iod`.** Those two stay exactly as compl §10 defines
  them, and neither exists until that branch lands.

**`klogd` is one thread where `drain_serial` was every idle CPU, and that is a
reduction this design accepts and must name.** Today any CPU going idle drains,
so the console keeps working while any CPU can reach a pass; after L3 the console
on a live machine has exactly one drainer. Three things bound it and the third is
a requirement:

1. Boot is `Drain::Inline` and needs no thread at all.
2. Panic and shutdown call `drain_inline()` directly, so the two paths that
   matter most never depend on `klogd` being schedulable.
3. **`klogd`'s own death must not be a silent one.** Extending the
   recoverable-panic predicate to cover a kernel thread turns a panic inside
   `klogd` into a killed thread — and a machine whose only console drainer has
   been killed goes quiet with nothing saying so, which is the exact failure the
   panel exists to make impossible. So `klogd` is the one kernel thread whose
   panic is **not** recoverable: it halts, by the same argument that makes a
   Ring 0 fault fatal. **L3 owns the predicate now** — it is the first caller —
   and compl C6 adds `usbd`'s and `iod`'s rows to it, which are the recoverable
   ones.

### 4.4 The console object, and where line atomicity actually comes from

Three producers reach the backend: `klogd` (whole records), `logd` (whole lines
through its `Console` handle), and the panic path (whole reports). Each takes
`BackendGuard` once per unit. **So every unit that reaches the wire is whole**,
and the ordering between producers is lock-acquisition order, which is time order
to within a lock hold.

The remaining hole is a userland writer that hands the kernel half a line, which
is exactly what `println!` does — `LineWriter` issues `flush_buf()` and then
`inner.write(rest)`, two syscalls per line. **The fix is in the kernel and it is
per holder**: `ConsoleObject` (the endowment spec's fourteenth `KObjectRef`
variant, §1.5) gains a line buffer. A write accumulates until `\n` and emits
whole lines under the backend lock. `MAX_CONSOLE_LINE` is 1024 — today's
`SW_BUF_SIZE` — and a longer line is emitted in `MAX_CONSOLE_LINE` chunks with
the count of the split said out loud, because a bound whose overrun is silent is
the defect this replaces.

**"Per holder" is load-bearing and the first draft of this paragraph said "per
handle" and then put the buffer on the shared object.** A buffer on one shared
object is one buffer that init and `logd` both accumulate into, and their two
half-lines splice inside the very mechanism that exists to stop splicing. So it
is **one console *backend*, and one `ConsoleObject` per endowment**: the object
is the line buffer plus a reference to the one backend, and the backend keeps
`BackendGuard` as its only serialiser. `ConsoleObject::new()` already mints a
fresh object per call (`object/device.rs:202`), so this is the shape the code
has; what it does not have is a second call site.

**Where the second one comes from, and it needs no new syscall.** init can only
endow what it holds, and nothing in userland can mint a `ConsoleObject` — so
`spawn_init` (`loader/mod.rs:912`) mints **two**: the first fills slots 0/1/2 as
it does today, the second is installed as a labelled endowment beside
`SYSCAP_LABEL`, and init moves it into whichever program its manifest marks
`console = true`. **"Exactly two in the machine" is then a property of
`spawn_init`'s source** — there are two `ConsoleObject::new()` calls in the
kernel and no way to reach a third — which is stronger than a manifest gate and
is why `one_console_holder` (§9.5) is a second line rather than the only one. An
image whose manifest marks nothing `console = true` leaves init holding the
spare, which it closes; an image that marks two is refused by that gate at
`cargo test --lib`, before any boot.

**`Console` loses `Rights::DUP`** (`initial_rights` gives it `BASE | READ |
WRITE`, and `BASE` is `DUP | TRANSFER | WAIT` — `object/ops.rs:35`, `:50`).
`KObjectRef::Device` is the precedent, dropping `DUP` in the same match for the
same reason. With `DUP` a third holder is a runtime
`SYS_HANDLE_DUP` away and no manifest gate can see it; without it "exactly two"
is a property of the object rather than something a test checks. Nothing needs it
after L7 — every child gets pipes to `logd`, not a console — and init's three
stdio slots are minted by `spawn_init`, which is inside the kernel and does not
go through the right. **Unrepresentable beats checked**, and `one_console_holder`
(§9.5) then guards the manifests as a second line rather than as the only one.

There are exactly **two** `Console` endowments in the machine: `/bin/init`'s (it
must be able to speak before `logd` exists) and `/bin/logd`'s.

The ANSI CSI strip that `SerialWriter` does today (`serial.rs:354`) moves onto
the same path and keeps its reason: the backend must never carry bytes it would
drop.

---

## 5. `logd`

### 5.1 Its manifest row

```toml
[programs.logd]
serves = ["log"]
syscap = ["logread"]     # Rights::LOG on a SysCap dup, §3.2
console = true           # the second and last ConsoleObject, §4.4
```

It `receives` nothing. It claims no device. It cannot open a compositor
connection, reach netd, or name a process. Its whole authority is: accept
connections on `log`, read the kernel's records, write the console, and write
files — the filesystem being ambient, which is the endowment spec's declared
residual D6 and not this spec's to close.

`[boot] start` lists `logd` **first**. Not because the ports need it — the
endowment architecture's whole point is that every port exists before any server
runs, so a client can connect before `logd` has executed an instruction — but
because the sooner it runs the fewer boot records sit in a shard with no reader.

**One new name and one new key**, and they are different mechanisms because the
merged tree made them different. `logread` is a row in `SYSCAP_RIGHTS`
(`toyos-manifest/src/lib.rs`) that a program asks for by name in `syscap`, and
init already narrows and endows a `SysCap` duplicate for exactly that
(`init/src/main.rs:527-539`) — nothing new is built. `console` is a
`#[serde(default)] bool` on `ProgramConfig` (`src/build.rs:44`) and on
`toyos_manifest::Program`, and what init does with it is §4.4's.

### 5.1a Eleven manifests, and the two that exist for the machine this is all for

**"The kernel never writes a file" means every boot configuration that does not
run `logd` loses `/log` entirely.** That is not a corner: `logd` must be in the
`[boot] start` of every image whose partition table carries a `TOYOS-LOG`, and
root `CLAUDE.md` says every image does. **The first draft counted six and the
tree has eleven** — **CONFIRMED** 2026-08-11, every one of them declaring a
`[boot] start`:

| manifest | `[boot] start` today | after |
|---|---|---|
| `system.toml` | compositor, soundd, netd, filepicker | `logd` first |
| `diag/system.toml` | toybox | **`logd` too** — see below |
| `console/system.toml` | console | `logd` too, and `console` gains `logread` |
| `tests/metalcase/system.toml` | compositor, soundd, netd, sshd, test-runner | `logd` first; `test-runner` gains `logread` |
| `tests/netcase/system.toml` | netd, test-runner | the same |
| `tests/sshdcase/system.toml` | netd, sshd, test-runner | the same |
| `tests/testcases/system.toml` | soundd, test-runner | the same |
| `tests/doomcase/system.toml` | soundd, test-runner | the same |
| `tests/doommusiccase/system.toml` | soundd, test-runner | the same |
| `tests/desktopcase/system.toml` | compositor, terminal | `logd` first; **no `test-runner` row**, so no `logread` |
| `tests/desktopaudiocase/system.toml` | compositor, soundd, terminal | the same |

**Two of the eleven have no `test-runner`**, which is why §3.2's "the gate that
reads the log runs inside `test-runner` itself" is a statement about six configs
and not about every image: `log_conservation` and the rest of §9.5's cursor
gates cannot run on `desktopcase` or `desktopaudiocase`, and nothing asks them
to.

**The diagnostic boot is the one that would have been missed and the one that
matters most.** `--diag-boot` exists for a T14 with no serial port; today the
kernel writes `/log` there like anywhere else, and after L6 an unchanged
`diag/system.toml` produces a flashed image that logs to a panel and nowhere
else. Adding `logd` does not weaken that image's guarantee, and the guarantee has
to be restated rather than assumed: it is *"the compositor is the only process
that claims the framebuffer, and it is not built into this image at all"* —
`logd` claims no device at all, which is §5.1's whole row, so the structural
argument survives verbatim. L6 edits that comment to say so, because a reader of
`diag/system.toml` must be able to see why the second program is safe.

**The console boot gains something it does not have today.** `/bin/console` seeds
its scrollback from the newest `/log/*.log` files (`console/src/main.rs:44`,
`:266`), which are the *previous* boot's; with `logread` it can show this boot's
kernel records live, off the cursor, with no file in the path. Not required by
anything here and not this spec's to build — named so L6 does not think
`logread` on `console` is decoration. Its module doc's *"the kernel's log ring is
64 KiB (`kernel/src/drivers/log_ring.rs`)"* is a citation L3 deletes the subject
of, so L3 rewrites it.

**The gate.** `cargo test --lib`: every manifest that declares a `[boot] start`
lists `logd`, and every manifest lists **at most** one program with
`console = true` besides init's kernel-minted one — at most rather than exactly,
because `spawn_init` mints the second object whether or not a manifest claims it
and an image that leaves it unclaimed is a valid image with no `/log` (§5.6). A
manifest added later fails the first clause by default, which is the direction
that bound has to fail in.

### 5.2 The protocol

One port, two frame kinds, both over the ordinary IPC framing (`toyos::ipc`):

```rust
enum ToLogd {
    /// Sent by a spawner, carrying the read ends of the child's stdout and
    /// stderr pipes over SYS_HANDLE_SEND. logd polls them from then on.
    Register { label: [u8; MAX_STREAM_LABEL], pid: u32 },
    /// A durability request. logd answers when the named records are on the
    /// device. One caller: the shutdown path (§6.3).
    Sync,
}
```

`MAX_STREAM_LABEL` is 32 bytes. `MAX_STREAMS` is 64 and a 65th `Register` is
`ResourceExhausted` with a line naming the label refused — a bound on the
primitive, answering the caller, never a truncation.

**A server never blocks on a client**, which here means logd never blocks reading
one stream while another has data: it uses `ipc::FrameRx` and an io_uring poll
set over every registered pipe plus `Source::Log` (§3.2), exactly as the
compositor and netd do.

### 5.3 Back-pressure, per client

Per stream, logd holds a bounded in-memory queue (`MAX_STREAM_BACKLOG`, 64 KiB).
Three states, and the client sees a different thing in each:

| logd's state | the client's `write` |
|---|---|
| draining normally | the pipe absorbs it |
| this stream's backlog full, logd still writing | the pipe fills; a blocking write parks, a non-blocking one is `WouldBlock` |
| `/log` given up on (§5.4) | logd keeps draining to the console only; a stream that outruns the console still fills its own pipe |

**A flooding process floods only its own stream.** That is the trust-domain
separation move 3 buys, and it is what makes
`specs/issues/isolation/rflags-tf-log-flood.md` a bounded event rather than a
machine-wide one — the #DB storm in that entry is a *kernel* producer, so it
drops-and-counts in its own shard and the count is exact, where today it fills a
shared 64 KiB ring and every other line goes with it. (The entry's real defect —
a handler that resumes a fault it cannot stop — is not this spec's and stays
open. §8 says so.)

**The honest other half, so the claim is not one-sided.** §2.2's depth comparison
says the flooding CPU's own window is 512 records against a shared 733, so it
loses its oldest slightly sooner than today; and the entry's own throughput note
— *"the rate is low only because each report is 25 lines of serial"* — cuts the
other way after L3, because in `Drain::Thread` nothing on the producer's path
touches the wire. The flood therefore gets *faster* and the machine gets *less*
damaged by it, which is the trade this design is making everywhere else too. What
is unambiguously better is that the loss is confined and counted.

### 5.4 The give-up policy, and where the bound comes from

**The bound is logd's.** `LOG_WRITE_BUDGET` — logd's own constant, in logd's own
source, with its reason there: the longest logd will wait for the log volume
before it declares the volume dead. Recommended value 5 s, which is a policy
number and says so; nothing about the device supplies one, and
`specs/completion-architecture-spec.md` §12.3 establishes that USB publishes no
bound for a bulk transfer and that SCSI timeouts are host policy everywhere. It
is compl §3.3's `Budget` **kind** and not that **type**: RT7 and the six duration
kinds reach `kernel/src/` and logd is userland, so until that vocabulary exists
this is a named `u64` in a userland program with its justification beside it,
and it stays one afterwards.

What happens when it expires, in order:

1. logd stops feeding the volume.
2. It writes one `alert!`-grade line to the console:
   `logd: /log has not answered in 5s — this boot's log is on the console only`.
3. It **keeps serving every client and keeps draining to the console.** It does
   not exit, does not retry, and does not queue for a device that is not
   answering. "I stop waiting for this stick and say so" is the whole policy.

**Under the ruled order this needs nothing new from the transport, and the
requirement inverts into an obligation on the other branch.** The first draft
required a mechanism completions leaves open — *the log volume's write path takes
an absolute `Deadline` from its caller, expiry answers `SyscallError::Io` and
cancels the outstanding transfer through Bulk-Only Reset Recovery* — because it
assumed L6 ran on a tree where compl C7 had already deleted the transport bound.
It has not. **CONFIRMED 2026-08-09: `USB_TIMEOUT_NS = 2_000_000_000`
(`kernel/src/drivers/xhci/mod.rs:319`) is live**, and it is what produces
`Scsi::Broken` → `write_blocks` `Err` → `SyscallError::Io` on a stick that stops
answering. So at L6 logd's writes come back with an error inside 2 s per transfer
and `LOG_WRITE_BUDGET` is an ordinary policy over repeated errors and a
slow-but-answering device. Nothing in the xHCI driver changes for this branch.

**What that makes it is compl §12.3's problem, with a caller it can now see.**
That section — rewritten by its own review — states that once the transport bound
goes, the correct count of cancellers for a present-but-hung stick is **zero**,
and leaves the resolution as *"the plan's one open decision with three named
options, because it is the owner's call."* Whichever option C7 takes, **it must
leave `/bin/logd` an error rather than an unbounded park**, because the
self-disable chain this spec puts in userland (§5.4's three steps) needs a value
to fire on. compl's own text calls the caller-side `Budget` *"where Linux puts it
and is probably better"*; if it picks the transport `Tripwire` instead, the chain
survives too, because a `Tripwire` panics rather than hanging. What is not
available to C7 is "no bound anywhere", and after this branch lands that is not a
hypothetical — it is a shipped daemon whose give-up policy it would silently make
unreachable.

**And this design restores a canceller compl §12.3 says it has lost.** Its
canceller 2 is *"kill of the waiting thread — real for a userland thread, and not
reachable for `logd`, `usbd` or `iod`."* Its `logd` was a kernel thread and
therefore unkillable; **ours is a userland process, so canceller 2 is real** —
init holds its `Process` handle and can kill it. That is one more way out of a
hung stick than any arrangement in that spec has, and after this branch lands it
is true on that branch's own baseline rather than a claim about a future one.

**What `LOG_WRITE_BUDGET` is not.** It is not the move of an existing number:
today there are two bounds and neither is it. `wait_transfer`'s 2 s transport
deadline is what produces `Scsi::Broken`, and it is what logd rides at L6;
`log_file.rs`'s `MAX_BLOCKED_NANOS` is **10 s** (`:190`, **CONFIRMED**) and bounds
*VFS lock contention*, not the device — L6 deletes it with the file. 5 s is a new
number in a new place and the recommendation says so; L6 records what it measured
under `usb-slow-device` beside it.

### 5.5 Rotation, retention and naming

Moved from `log_file.rs` unchanged in behaviour, into userland where the policy
belongs:

| constant | today | after |
|---|---|---|
| `MAX_LOG_FILES` | 16 (`log_file.rs:129`) | 16, in logd |
| `MAX_LOG_BYTES` | 1 MiB, 256 B under the `log-rotate-fast` boot parameter | the same, and the fast value becomes a logd argument rather than a kernel actuator |
| `MAX_LOG_PARTS` | 9999 | 9999 |
| naming | `<wallclock>.log`, `unknown-NN.log`, `_0002` continuations | identical, so a stick from before this change and one from after sort together |
| `classify` | strict; anything unrecognised is somebody else's file and never deleted | identical, and now it is *more* necessary: `/log` is userland-writable and logd is userland |

**`specs/issues/boot-media/rotation-leaves-the-newest-in-the-older-name.md`
closes** — it describes the two-generation `kernel.log`/`kernel.log.1` scheme,
which #140 already replaced with one file per boot and `_NNNN` continuations that
sort in write order. The entry is stale rather than fixed, and L8 verifies that
before deleting it.

`specs/issues/boot-media/sink-append-error-unreachable.md` closes as **moot**:
`Sink::append` goes with `log_file.rs`. Its durable finding — that
`file_cache::write_page` merges into a failed read, and that
`log_backing_read_error` is what stages it — is about the file cache and not the
log, so L8 moves that sentence into the file cache's own doc comment and deletes
the entry.

### 5.6 What logd does when there is no `/log` at all

It says so once, on the console, and runs as a console-only logger.
`report_log_destination` (`main.rs:300`) splits: the kernel keeps the half it
knows (whether it has a console) and logd owns the half it knows (whether it has
a volume). The four-way table and its `!!!` markers survive as two `alert!`s, one
from each side, and the panel still paints them red — now because of `Level`
rather than because of three exclamation marks.

### 5.7 If logd dies

Its `Acceptor`'s last handle drops, every registered stream's pipe end drops, and
every client's next write on slot 1 or 2 is `Gone`.

- **The kernel's console output is unaffected.** `klogd` is a kernel thread and
  the panel is the kernel's. The dev host's instrument and the T14's panel both
  keep working. This is the direct payoff of §4.1's split.
- **`/log` stops.** Nothing else can write it.
- **init says so.** init holds logd's `Process` handle (it is in `[boot] start`)
  and waits on it; when it exits, init writes
  `init: logd exited (<code>) — userland output is going nowhere` to its own
  console handle.
- **No client dies of it.** `println!` panics when the underlying write returns
  an error, so a dead logd would otherwise kill every daemon on the machine. The
  ToyOS stdio PAL (`rust/library/std/src/sys/stdio/toyos.rs` — a `toyos`-named
  file, so within the std fork rules) maps `Gone` on slots 1 and 2 to a
  successful write of zero bytes. It is not silent: init has already said it on
  the console, once, which is where a machine-wide event belongs.
- **logd is not restartable**, for the same reason no `serves` program is: an
  acceptor is endowed by move and `SYS_PORT_REARM` (endowment spec §12, number
  113) does not exist yet. Recorded, not worked around. It is the same sentence
  that spec's §5.3 writes about every daemon.

---

## 6. Userland stdio, the panic path and early boot

### 6.1 Every program's path

| program | slots 0/1/2 | who made them |
|---|---|---|
| `/bin/init` | the kernel's first `Console` | `spawn_init` (`loader/mod.rs:912`), unchanged |
| `/bin/logd` | the kernel's second `Console` | `spawn_init` mints it, init moves it, per the manifest (§4.4) |
| every `[boot] start` program | a pipe pair to logd, labelled with its manifest name | init: create the pipes, `SYS_HANDLE_SEND` the read ends to logd with a `Register`, endow the write ends as the child's slots |
| a launched program | the same, from the launcher | the launcher holds a `log` connector |
| an sshd session shell | the session's own pipes, as today | sshd already creates them |
| any other child | **inherited from its parent** | nobody; `Command` inherits, exactly as today |

**Inheritance is deliberate and it is a stated limit.** A shell's children write
into the shell's stream, so their output is attributed to the shell. That is
today's behaviour, it needs no authority, and a spawner that wants separate
attribution asks logd for a new stream — which needs a `log` connector, which the
manifest grants to init, the launcher and sshd and to nobody else. The
alternative, every process holding a logd connector, hands the whole machine an
authority it does not need for the sake of a label.

### 6.2 What `println!` costs now

Today: `SYS_WRITE` on a `SerialConsole` descriptor → `SerialWriter::console` →
`write_chunk_blocking` → the shared ring → the idle loop → the backend. Measured
consequence: every `println!` any process makes is a device write from the idle
loop (`log-flush-is-unbounded`'s own second premise).

After: `SYS_WRITE` on a pipe → logd → the console and `/log`. A pipe write is a
memcpy into a shared-memory ring. **The kernel's log path and the userland
console path stop being the same path**, which is (c) in §1.2 deleted at the
root, and it is also the deletion of "a userland `println!` makes the idle loop
write a FAT volume".

Counts, for the migration's size:
`grep -rn 'println!' --include='*.rs' userland/ | wc -l` → **234**;
`eprintln!` → **141**. Not one of them changes: they are `std` macros writing to
slots 1 and 2, and only what is behind those slots changes.

### 6.3 Shutdown

`SYS_SHUTDOWN` (`arch/syscall.rs:219-251`) today logs "Syncing filesystems…",
syncs, logs "Shutting down.", and calls `acpi::shutdown()`; both lines die in the
ring (`specs/issues/kernel/shutdown-path-logs-never-reach-console.md`). After:

1. Send `Sync` to logd and wait for its answer, bounded by `LOG_WRITE_BUDGET`.
   Ordinary thread context, so it blocks properly — a shutdown that loses its own
   last lines is the one nobody can diagnose.
2. `drain_inline()` for the console, after logd has answered, so the last kernel
   record — including logd's own — is on the wire before the power goes.
3. `acpi::shutdown()`.

That closes the entry, and the gate is a named assertion that the guest's last
console line is the shutdown's own and that `/log` carries it too.

### 6.4 Panic

**The panel.** `panic_console::capture` and `live_tail` read records through
`snapshot_committed` instead of `peek_tail`, and render through §3.3's formatter.
The design rule the file states — *render and everything it calls acquires NO
synchronization primitive* — is easier to keep than it is today, because
`snapshot_committed` takes nothing at all, where `peek_tail` takes nothing and
tolerates tearing. `capture`'s stated remaining reason (freezing the report
against siblings still logging) is unchanged and it stays.

**Serial.** `panic_flush` keeps its two-stage shape — wait, bounded, for a clean
`BackendGuard` handoff, then bypass a wedged holder with virtio-console disabled
— and drains records rather than bytes. Its `# Safety` clause loses the
"unsynchronized ring read" half: there is no ring lock to bypass any more, only
the backend's.

**`/log`.** The kernel writes nothing, at panic or ever. What `/log` holds after a
panic is everything logd made durable, and the lag is one logd wakeup plus one
device round trip — the same lag `log_file::poll` has from the idle loop today.

**And the report itself still reaches `/log`, because the dying machine waits for
logd.** `apic::wait_for_log_file` survives in shape and changes what it waits on:

```
if serial::has_console() { return }             // unchanged: the report is already off the box
kick every sibling                              // unchanged: a quiet CPU is in sti;hlt
wait, bounded, until LOG_DURABLE_NS >= the panic record's at_ns
```

`LOG_DURABLE_NS` is a kernel global that logd publishes: `LogCursor` gains a
`durable: u64` field that logd sets to the timestamp of the newest record it has
`fsync`ed, and the kernel takes the maximum on the next `SYS_LOG_READ`. One
field, no extra syscall, and logd calls `SYS_LOG_READ` every loop anyway. It is
also what lets Ctrl+Alt+D say how far behind the file is.

**`durable` is a number that crossed the trust boundary and decides how long a
kernel waits, so it is clamped.** The kernel takes
`min(cursor.durable, newest_record_at_ns)` before the maximum, because an
unclamped `u64::MAX` from a buggy logd makes `wait_for_log_file` return
immediately and the report is silently lost — which is exactly the "a device's
own numbers are untrusted" rule one layer up. Clamping cannot make the wait
*longer* than the bound, so the only thing a hostile logd can do is shorten a
wait for its own output, and that is acceptable. Stated because a field this
shape with no clamp reads as an oversight.

The bound is today's `LOG_FILE_DRAIN_NANOS`, 500 ms (`apic.rs:117`), **re-derived
because what it bounds has changed kind, not just size**. Today's derivation is
its own comment: *"the idle loop goes round in microseconds and a flush is one
FAT append plus a sync."* After L6 the same wait covers a **userland process
being scheduled**, two syscalls, a page-cache write-back, a FAT append, a device
cache flush and a second `SYS_LOG_READ` to publish. L6 measures it under
`usb-slow-device` and writes the number it found. This is the mechanism that
keeps **`screen_fatal_halt_composited`**'s second half — the assertion that the
fatal report is in `/log` and not only in a photograph
(`tests/toyos.rs:3402-3427`) — green, and root `CLAUDE.md`'s "three
investigations argued from JPEGs" is why it must be.

**The test is `screen_fatal_halt_composited`, and this spec named it
`screen_fatal_composited` in five places, which is a test that does not exist.**
**CONFIRMED** 2026-08-09 (`grep -rn screen_fatal tests/`): the two real names are
`screen_fatal_halt` (`tests/toyos.rs:261`) and `screen_fatal_halt_composited`
(`:267`), and **only the second has a `/log` half** — it is `mute: true` exactly
so that `wait_for_log_file` is reachable, and after dropping the guest it reads
the newest log off the image's own partition (`:3411-3427`).
`screen_fatal_halt` asserts on the panel alone. All five sites are corrected; a
gate named in a spec and absent from the tree is a gate nobody runs.

**What it cannot cover**, and this is the pstore's subject. The first draft named
four cases and called them the same four that fail today; §13.4 now states what
is actually new. The four that are genuinely unchanged: a **#DF**, a **triple
fault**, a **panic in the log writer itself**, and a **panic whose thread holds
the VFS lock**. In each the drainer cannot run, the bound expires, and the panel
is the only copy — and the one line the current code prints for it (`"the panel
is the only copy"`) survives.

**`wait_for_log_file` runs before the halt IPI** (`apic.rs:220`, `halt_all_cpus`'s
first statement), which is what makes waiting for a *sibling* to drain coherent
at all, and it is unchanged. It is also why the wait is worthless at `--smp 1`:
the panicking CPU is the only one and it is the one spinning. That is true today
and stays true.

### 6.5 Ctrl+Alt+D

`log_ring::Mark` — a byte position in a single stream — has no meaning across
shards. It becomes an `Instant`: `dump::request` takes `clock::nanos_since_boot()`
before and after, and `panic_console::paint_report(from, to)` calls
`snapshot_committed(from, to)`. That is **better** than today's bracket, not a
concession: the dump's own records are stamped by the same clock, so the bracket
is exact rather than being a byte range that a concurrent writer can widen.

`page_forever`, the halted-machine pager, and `hold_report`'s 20 ms
evidence-driven repaint are untouched. Per compl §17.3, `hold_report` stays in
`drain_irqs` and does not move to a kernel thread.

### 6.6 The pstore question — the owner's, and here are both columns

**What it is.** A region of RAM excluded from the memory map, into which the
panic path copies the merged record tail with a header and a CRC; the next boot's
kernel validates it and exposes it through the same cursor syscall under a flag,
and logd writes it out as `prev-crash-<stamp>.log`. Every production OS has one
(Linux `pstore`/`ramoops`, Windows' `bugcheck` region, macOS' panic log).

**What it buys, precisely.** The four cases §6.4 cannot cover — a panic holding
the VFS lock, a dead logd, a #DF, a triple fault — **and the three §13.4 adds**,
which is the part that changed under review: a panic with no scheduler able to
pick logd, a panic while any CPU holds one of the four locks logd needs, and a
logd that died earlier in the boot. On a T14 with no serial port those are the
boots where the only record is a photograph of a panel — or, for a triple fault,
nothing at all. **Seven cases rather than four is why this recommendation got
stronger under review, not weaker.**

**What it costs.**

| cost | size |
|---|---|
| reserved RAM | one shard's worth, 128 KiB, is the natural unit |
| bootloader | a new `KernelArgs::pstore_{addr,len}`, and a memory-map entry the kernel must not hand to the allocator |
| kernel | a panic-path copy that takes no lock and allocates nothing (it already has `snapshot_committed`, so this is a memcpy and a CRC), plus a boot-path validate |
| ABI | one flag on `SYS_LOG_READ` selecting the previous-boot region |
| a promise it cannot keep | **it is best-effort on real hardware.** Firmware may zero or reuse the region on any reset, and a cold boot certainly retrains memory. Nothing on this host can establish what the T14's UEFI does — only the owner's stick can |
| a gate that proves less than it looks | QEMU's `system_reset` over QMP preserves guest RAM, so `pstore_survives_reset` is a real and cheap test. It certifies the format and the code path and says **nothing** about firmware |

**Recommendation: build it, as the last chunk of this branch (L10), gated on the
owner saying yes, and with its `specs/issues/` entry opened at the same time
recording that the metal arm is owed.** Effort is not the argument for keeping it
separate; *coupling* is. Its value turns on a firmware behaviour no test here can
observe, so folding it into L1–L9 would make the refactor's verdict depend on a
question the refactor cannot answer. Kept last, a red there is its own red.

**What L1–L9 must do so L10 is cheap, and it costs nothing:** the persisted
format is the `LogRecord` array, byte for byte. No second serialisation, no second
formatter, and the previous boot's records read out through the same cursor and
render through the same `Display`.

---

## 7. Diagnostics that must keep working

**None of these takes a `Lock` today, and after L3 none of them takes one at
all.** That is the property this branch is responsible for, on a tree where
`Parkable` and `SleepLock` do not exist yet. When completions lands, every one of
them still runs from a context with no `Parkable` (compl §6.1), so a sleep lock
in a diagnostic stays untypeable — this branch does not weaken that and the L3
deletion below is what makes it cheaper to keep.

| facility | today | after |
|---|---|---|
| `panic_console` fatal report | `peek_tail` on a byte ring, tolerating tears | `snapshot_committed`, detecting them |
| `boot_checkpoint` (6 phase repaints) | `peek_tail` with IRQs on, "a torn line on screen, nothing more" | `snapshot_committed`; a torn record is skipped, so the panel stops showing garbled lines |
| Ctrl+Alt+D | two byte `Mark`s | two `Instant`s, §6.5 |
| `dump_nmi_probe` | unchanged | unchanged — **the NMI handler still must not log**, and the reason is the same shape: it would reenter its own CPU's shard. A grep gate asserts `arch/idt/nmi.rs` contains no `log!` |
| `/bin/console` scrollback seeding | reads the newest `/log` files | unchanged; the files are logd's now and their names and format do not change. With `logread` it also gains this boot's records live, off the cursor (§5.1a) |
| the harness's console capture | `is_kernel_line`, `Serial::interleaved` | both stay; `interleaved` becomes an assertion where it is safe to be one, §8.1 |

**One lock the dump reaches goes with `log_ring.rs`, and landing first is what
turns that from a claim into that branch's baseline.** compl §17.1 enumerates
three locks `sched/dump.rs` reaches indirectly and concludes *"after all five
conversions the one lock that can refuse the dump is still a raw ticket `Lock` …
`SleepLock::holder()` buys the dump nothing."* Its middle row is `log!` →
`log_ring.rs:314`'s `RingGuard`, *"hand-rolled CLI spinlock, unbounded"* — a
`cli` bracket and a `compare_exchange_weak` spin (`:318`, `:323-328`,
**CONFIRMED**). **After L3 the dump's own `log!` calls take no lock at all, on
any path.** Under the ruled order that happens before compl opens, so its §17.1
should find **two** rows rather than three at its own C0, and the row is deleted
rather than converted. It is a small win and it is one compl explicitly says it
cannot make.

Gates that must be green throughout, per compl §17: `blocked_dump`,
`screen_blocked_dump`, `dump_nmi_probe`, `screen_panic_muted`,
`screen_fatal_halt`, `screen_fatal_halt_composited`, `screen_console_shell`,
`screen_console_panic`, `disk_backtrace`, `fault_gates`, `fpu_isolation`,
`kernel_log_file`.

---

## 8. Deletion ledger

### 8.1 Code, by name

**`kernel/src/drivers/log_ring.rs` — the whole file, 549 lines.** With it:
`LogRing`, `RingCell`, `RingGuard` and its `cli` bracket, `RING_LOCKED` and the
per-record `compare_exchange_weak`, `WRITTEN`, `Mark`, `mark`, `OWED`,
`has_pending`, `SERIAL_SINK`, `set_serial_sink`, `FILE_SINK`, `FILE_OWED`,
`FILE_DROPPED`, `file_has_pending`, `enable_file_sink`, `disable_file_sink`,
`drain_to_file`, `take_file_drops`, `write_chunk`, `write_chunk_blocking`,
`drain_to_serial`, `drain_chunk_to_serial`, `report_dropped`, `DROP_MARKER_MAX`,
`take_drop_marker`, `DROPPED_BYTES`, `drain_unlocked`, `peek_tail`, `peek_range`,
`DRAIN_CHUNK`.

**`kernel/src/log_file.rs` — the whole file, 565 lines.** With it: `Sink`,
`SINK` and its `Lock`, `IN_FLUSH`, `flush_in_progress`, `install`, `destination`,
`poll`, `flush_final`, `Refusal`, `Sink::{flush,append,continue_in_next_part,
stopped_at}`, `path`, `stamp`, `Class`, `classify`, `ours`, `sweep`,
`undated_stem`, `MAX_BLOCKED_NANOS`, `CHUNK`, `UNDATED_STEM`, `DIR`, `MOUNT`.
`MAX_LOG_FILES`, `MAX_LOG_BYTES` and `MAX_LOG_PARTS` move to logd with their
values. The `log-rotate-fast` actuator goes — its `actuators!` row and
`log_file::max_log_bytes` with it; the fast value becomes a logd argument.

**`kernel/src/sched/driver.rs`:** `flush_log_file_if_affordable` (`:722`),
`LOG_DEFERRAL_CEILING_NS` (`:701`), `LOG_DEFERRED_SINCE` (`:706`),
`log_file_flush_due` (`:743`), `owes_wake` (`:832`, whose only caller is the
above), `drain_serial` (`:762`) and its `BackendGuard::lock` spin with interrupts
disabled, and **two** of the four pre-`hlt` conditions in `execute`'s `Idle` arm:
`log_ring::has_pending` (`:523`) at L3 and `log_file_flush_due` (`:552`) at L6.
The idle loop's `drain_serial()` (`:681`) and `flush_log_file_if_affordable()`
(`:689`) statements.

**`log_ring::has_pending` (`:523`) is deleted outright and nothing replaces it.**
The producer posts `klogd`'s wake at the commit (§2.6a), so the halt is refused
by the mechanism that refuses it for every other runnable task: the `Idle` arm's
own `doorbell().kick_pending()` and `!mailbox_is_empty()` (`driver.rs:474-478`),
the `SLEEPING`-before-the-final-check handshake, and Invariant T. A log-specific
condition on the halt path is exactly the scaffolding §1.2 calls the patch for a
drain that could not be woken.

The other two pre-`hlt` conditions — `i8042::verdict_due` (`:534`) and
`xhci::port_work_pending` (`:564`) — are **compl C9's, not this spec's** (§12).

**`kernel/src/drivers/serial.rs`:** `SerialWriter` whole (`:287-…`), `SW_BUF_SIZE`,
`SerialWriter::{lock,console,spill,push_byte,write_bytes,write_user}`. The
formatter it was moves into `log::emit`'s stack buffer; the ANSI strip moves onto
the console path (§4.4). `panic_flush` and `flush_final` survive with records in
place of bytes.

**And `object/ops.rs:469`'s `KObjectRef::Console` write arm is re-pointed in the
same chunk, or L3 does not build.** (It was `fd.rs:585`'s
`Descriptor::SerialConsole`; #22 deleted `fd.rs` and the whole `Descriptor`
model, and the arm survived the move unchanged.) It is
`serial::SerialWriter::console().write_user(buf)`
today, and L3 deletes both `SerialWriter` and the ring underneath it while L5 is
what adds `ConsoleObject`'s line buffer. Between them userland console output has
no path at all, which breaks §10's "every chunk builds, boots and passes
`cargo test`". So **L3 re-points that arm straight at `BackendGuard`** — one
acquisition per `write`, ANSI-stripped, no buffering — and L5 puts the line buffer
in front of it. That is not a detour: it makes `console-unbuffered` (§9.4)
literally L3's own state, so the negative control is a real prior build rather
than an invented one.

**`kernel/src/arch/apic.rs`:** `LOG_FILE_DRAIN_NANOS`'s derivation and `owed()`
are rewritten against `LOG_DURABLE_NS` (§6.4); `wait_for_log_file` survives.

**`kernel/src/log.rs`:** the `log!` macro's body — the `gs:` reads and the
`SerialWriter` — and its false doc comment (§1.3). `PERCPU_READY` survives and
selects the shard.

**`tests/common/serial.rs`: nothing. `Serial::interleaved` stays and gains
teeth.** The first draft deleted it (`:74`, `must_say`'s note at `:99-106`, and
the two `self_check` cases at `:184-195`) on the ground that *"the thing it
detects is no longer expressible"*. **That is the move that hides a regression,
and it is the wrong trade here for three reasons.**

- It is a **detector**, not a repair. §14's rejection 7 is about repair and the
  first draft borrowed its argument for a different act.
- It is the only thing in the harness that observes a splice **at all**, and it
  observes every capture the suite takes for free. `console_line_atomicity`
  (§9.5) is one positive test of one mechanism on two processes;
  `interleaved` is a negative observation over ~250 boots. The replacement is
  narrower than what it replaces.
- **CONFIRMED**: it asserts nothing today — its only caller is `must_say`'s
  failure note. So "delete it, it is no longer expressible" removes coverage and
  buys fifteen lines.

**What changes is that it becomes an assertion where it safely can.**
`console_line_atomicity` and `logd_gone` assert `interleaved().is_none()` on
their own captures, so a tree with the old coupling reintroduced reds on the
detector as well as on the count — and the `console-unbuffered` actuator, which
produces exactly kernel-into-userland splices, reds both. It does **not** become
a suite-wide assertion, and the reason is concrete: `/bin/console` seeds its
scrollback from `/log` and prints lines that legitimately contain `[kernel `,
so `screen_console_shell` would red on correct behaviour. Suite-wide it stays a
note on `must_say`, which is what it is good at.

`is_kernel_line` stays either way — three other call sites use it to count kernel
lines.

**`kernel/src/actuator.rs`:** the `log-rotate-fast` row, and with it the last
reason `kernel/Cargo.toml` names the log at all.

### 8.2 `specs/issues/` this closes

Slugs only, deliberately: `src/docs.rs`'s `every_named_issue_file_resolves` walks
every text file in the tree and reds on a `specs/issues/<area>/<slug>.md` path
that does not resolve, so a full path written here would red `cargo test --lib`
the moment the file is deleted. **L8 must also de-path the citations elsewhere** —
several of the entries below are cited by full path from the root `CLAUDE.md`,
from `kernel/CLAUDE.md`, from `specs/introspection-plan.md` and from
`specs/completion-architecture-spec.md`, and every one of those is a
`cargo test --lib` red at L8. The `specs/issues/README.md` protocol says the
durable rule moves into the spec that owns the subject; doing that is the same
edit that removes the citation.

| slug | area | closed by | what makes the claim true |
|---|---|---|---|
| `redesign-the-log-subsystem` | design-debt | L8 | this spec is the answer to its design half; L8 re-files the `kernel/src` layout half as its own entry, because that question is untouched |
| `log-flush-is-unbounded` | boot-media | L6 | there is no flush on the idle path and no kernel file write |
| `client-cpu-takes-the-log-flush` | audio | L6 | there is no heuristic left to steer, and no CPU takes a flush. **Its hypothesis is closed unverified and L8 says so where it goes**: the entry's own last section is *"only the owner's next boot can verify it"*, and deleting the mechanism makes it permanently unfalsifiable. The T14 arm is owed, filed the same way §6.6 files pstore's |
| `pre-idle-wedge-says-nothing` | diagnostics | L3 | `Drain::Inline` puts every boot record on the wire as it is written |
| `log-ring-flushes-one-line-behind` | kernel | L3 | a CPU does not halt with a line unsent, **and it is one mechanism**: the commit posts `klogd`'s wake (§2.6a), so the halt is refused by the doorbell and Invariant T rather than by a log-specific pre-`hlt` condition, and the last line before a quiet period reaches the wire |
| `shutdown-path-logs-never-reach-console` | kernel | L6 | §6.3's ordered shutdown |
| `serial-console-has-no-line-atomicity` | diagnostics | L5 | §4.4: three producers, whole units, one lock |
| `sink-append-error-unreachable` | boot-media | L6 | its *subject* (`Sink::append`) is deleted, so the entry goes — but **not "moot", which the first draft said and which is backwards**. Its durable finding is *reactivated*: the entry's premise is that the kernel sink's tail page stays resident, and after L6 the appender is an ordinary userland process whose tail page is ordinary page-cache and *is* evictable. So `file_cache::write_page`'s merge-into-a-failed-read is reachable from the log path again, and the sentence moves to the file cache's own doc as a **live hazard with `log_backing_read_error` as its stager**, not as history |
| `rotation-leaves-the-newest-in-the-older-name` | boot-media | L8 | **verified stale, 2026-08-09**: the entry describes `kernel.log`/`kernel.log.1` at 4 MiB, and `sink-append-error-unreachable` records that #140 replaced that with one file per boot; `MAX_LOG_BYTES` is 1 MiB and continuations are `_NNNN`, which sort in write order. L8 re-checks that `kernel_log_file` no longer accepts "either of the two files" before deleting |
| `the-panic-path-does-not-write-the-log` | boot-media | L8 | it is `kind: rejected` and its argument is now a property of the architecture rather than a decision; the durable sentence moves into §6.4 |
| `idle-machine-looks-wedged` | kernel | L8 | **CONFIRMED** already `Superseded by #156`, and #156 is closed (`kernel/CLAUDE.md`, Scheduling). The first draft's second reason — "its log-shape argument is what L3 makes obsolete" — is right for a reason it did not give: after L3 the last record before a quiet period *does* reach the wire, so "the log stops here" becomes evidence rather than an artefact of the drain. That is what makes the entry's confusion unrepeatable |

**Re-scoped, not closed.** `log-is-userland-writable` (boot-media) keeps its
residuals 2 and 3, and residual 1 changes character: `/log` is written by a
userland daemon now, so "the kernel's own volume is not userland's to write" is
no longer half-done — it is decided the other way, on purpose, and L8 rewrites
that residual to say so.

**Named and NOT closed**, so nobody claims them: `rflags-tf-log-flood`
(isolation) — this bounds the blast radius (§5.3) and does not fix the handler
that resumes a fault it cannot stop; `one-rmw-per-log-line-cost-350ms`
(hardware) — the finding is what governs §2.3 and stays as the record;
`the-log-mount-is-not-certain` (boot-media);
`kernel-log-unreadable-once-userland-owns-the-screen` (hardware);
`disk-wait-pins-a-cpu` (audio) — that is compl C7+C8's;
`cache-eviction-wedges-an-idle-cpu` (boot-media) — compl C13's.

### 8.3 Documentation

`kernel/CLAUDE.md`'s **Boot device identity** paragraph names `log_file` as the
writer and must name logd. Root `CLAUDE.md`'s known-issues bullets on the idle
loop and on `log_file::SINK` being one of the four spinlocks lose their log half
— and the rule that must survive the edit is *"anything added to the idle loop is
an audio change"*, which is still true and now applies to a much shorter loop.
`userland/CLAUDE.md`'s "A diagnostic line is several `write`s" paragraph is
deleted outright; its subject is gone. Every removal names its replacement, per
the ratchet.

**One root `CLAUDE.md` rule is repealed outright and the first draft did not name
it**, which would have left the tree carrying a debugging instruction that is
false: *"**A boot that wedges before the idle loop produces no serial output at
all** … The corollary a freeze investigation keeps getting wrong: the end of a
metal log is where the machine went quiet, never where it stopped."* Both
sentences are `Drain::Inline`'s and `klogd`'s subject. After L3, on a machine
with a console, the end of the log **is** where it stopped. That is the single
most useful thing this branch does for a freeze investigation and it has to
replace the old rule rather than sit beside it — a reader who applies both
concludes nothing. L8 writes the replacement, and it is shorter than what it
replaces, which is what the ratchet wants.

---

## 9. Gates

### 9.1 Atomicity under concurrent multi-CPU producers — a conservation law

`log-storm`, a kernel feature actuator. Every CPU emits records in a tight loop;
each record's message is a known pattern carrying its shard, its sequence and a
checksum over the two. `test-runner` — which holds `logread` from its own
manifest row, where a spawned test binary would not (§3.2) — reads through
`SYS_LOG_READ` until the storm ends and asserts:

```
records_emitted  ==  records_read + cursor.lost
```

and that **every** record read is internally consistent — its checksum matches
its declared shard and sequence, and its `len` matches. A torn record fails the
checksum; a lost record that is not counted fails the equality; a duplicated one
fails it the other way. This is exact, not statistical, and it is the gate the
whole design turns on.

The actuator carries the comment the harness rule requires: nothing else can
reach it, because a real workload's record rate is set by what the kernel happens
to log and cannot be made to saturate a shard.

**It does not cover §2.3a**, and that is why `log_migration_storm` (§9.5) exists
beside it: a tight per-CPU loop never leaves its CPU, so the reservation race the
bracket exists to prevent cannot happen inside it however long the storm runs.
The conservation law is the right *verdict* for that defect and this storm is the
wrong *workload* for it.

### 9.2 Nesting — the case loom cannot express

Loom models threads, not strict LIFO reentrancy on one CPU, so §2.4's fourth
property needs an actuator. `log-nested-emit` arms a one-shot LAPIC timer to fire
inside a record's body-write window, from a handler that emits its own record.
The verdict: both records are read out, both pass §9.1's checksum, and their
sequences differ. On a tree where the reservation is a load-then-store rather
than an interrupt-atomic increment, they collide and the gate reds.

### 9.3 A hung device cannot stall audio

The actuators are `usb-slow-device` and `io-depth-probe`; the instrument is gate
A; the protocol is compl §20.1's interleaved four-arm A/B in two worktrees, and
**the owner's hardest bar is that audio quality improves or holds and never
degrades.**

Three readings, all from the same run:

1. **`audio_tone --slow-usb`, smp=1, soundd's worst wake.** Recorded before:
   165,948 µs against a 7,117 µs ordinary-stick control. The assertion is
   whatever the same-session A/B measures on the tree that lands, added to
   `tests/audio-baseline.toml` with the run that produced it. **Landing first
   moves this number here.** compl §20.2 puts it at its C7+C8; under the ruled
   order the *idle-loop* half of it is L6's, because L6 is what removes the only
   path by which an idle CPU reaches a block device. What remains for C7+C8 is
   the syscall half — `fd.rs:644`'s write-back and `wait_transfer`'s spin — and
   the two are separable readings of one instrument, so L9 records which it
   moved.
2. **`io-depth-probe`.** Today 4 from the idle loop and 5 from a syscall. After
   L6 **the idle-loop arm has no path to measure**: the kernel's log path reaches
   no filesystem, no volume and no controller, and nothing else on the idle loop
   does either. The syscall arm is untouched by this branch and is compl
   C7+C8's.
3. **The positive log-content assertion, in the same run.** compl §20.2 makes
   this point and it is sharper here: `/log` is a USB volume in every profile, so
   the cheapest way to make reading 1 look good is for the log to stop being
   written — and §5.4's give-up policy is a mechanism that does exactly that on
   purpose. So the run also asserts, host-side against the volume, that this
   boot's log file carries `Boot: complete`, this image's own ESP GUID nonce, and
   the tail of what the guest said. `kernel_log_file`
   (`tests/common/volumes.rs:477`) is that assertion and it already exists; L6
   re-points it at logd and it must stay green, mid-run and after shutdown.
   **Without it the headline number is unfalsifiable.**

### 9.4 Negative controls — each must red on a tree with the defect

**They are boot parameters, not kernel builds, and that is the single biggest
thing the merged tree changed about this section.** #29 turned every actuator
into a row in `kernel/src/actuator.rs`'s `actuators!` macro, armed at runtime
from `KernelArgs`' cmdline and reached through `BootOptions::kernel_params`; a
full run builds **two** kernels, the one an image ships and the one carrying all
of them. So the six below cost **no extra kernel build and no extra image**, the
first draft's "six more images in `specs/test-cost-audit.md`'s ledger" is void,
and each carries in its `actuators!` doc comment the claim that file requires —
why the host cannot stage the state. `assert_actuators_match_features` is what
holds them out of a shipping kernel.

| actuator | what it reintroduces | what reds |
|---|---|---|
| `log-commit-early` | the commit store moves **before** the body write | §9.1's checksum, within one storm |
| `log-shared-reservation` | the reservation becomes a load-then-store | §9.2, deterministically |
| `log-unbracketed-reserve` | §2.3a's `pushfq`/`cli`/`popfq` around the shard-read and the `xadd` is removed, so a preempted producer can `xadd` a shard it does not own | `log_migration_storm`'s conservation law at `--smp 8` |
| `log-trusts-durable` | the §6.4 clamp on `LogCursor::durable` is removed | a test logd publishing `u64::MAX` makes `wait_for_log_file` return with nothing written, and `screen_fatal_halt_composited`'s `/log` half reds |
| `log-writes-the-file` | `klogd`'s drain appends records to `/log` through the VFS, from the idle loop — the coupling, rebuilt in miniature | `io-depth-probe`'s depth, and §9.3 reading 1 by the recorded margin |
| `console-unbuffered` | `ConsoleObject`'s line buffer is bypassed; each `write` reaches the backend — **which is literally L3's own intermediate state** (§8.1) | `console_line_atomicity`, and `Serial::interleaved` on the same capture |

None can join `INERT_ACTUATORS`. `log-writes-the-file` is the strongest
because it replaces the behaviour rather than a verdict, which is the harness's
own rule for what makes an actuator worth having;
`log-unbracketed-reserve` is the one that has to exist, because §2.3a is the
correctness claim the whole design rests on and nothing else can make it fail on
purpose.

**There is deliberately no actuator for §2.6a's fences.** A guest build with
either one removed behaves identically on x86 — that is the whole reason W3
exists — so the negative control belongs where it can red, in `log_wake.rs`
(§2.5). That argument survived the actuator change and got weaker: a seventh
actuator now costs a declaration rather than an image, and it is still a test
that cannot fail.

**`log-writes-the-file` also inherits a control from the other branch, and the
order change is what makes it an inheritance rather than a pair.** compl §20.3's
`reintroduce-idle-flush` puts `log_file::poll()` back on the idle loop; its own
text says the two *"are not interchangeable — this one reds on a tree where the
flush is on the wrong context, that one on a tree where the kernel writes a file
at all"*, and that **"whichever branch lands second inherits both; neither
retires the other."** Under the ruled order the second branch cannot build
`reintroduce-idle-flush`: after L6 there is no `log_file::poll` to reintroduce
and no kernel file path for it to sit on. So it **is** retired, and
`log-writes-the-file` is the only control of that class the tree ends up with.
That is a real loss of one shade of coverage — a tree where the flush is on the
wrong *context* is no longer stageable — and it is the price of the work not
existing. Named rather than discovered at compl's C14.

### 9.5 New named tests

- **`log_conservation`** — §9.1, at `--smp 1`, `4` and `8`. **It runs inside
  `test-runner`**, which is the manifest program that holds `logread` in each
  test config; a spawned test binary does not inherit a `SysCap` dup (§3.2).
- **`log_nested_emit`** — §9.2.
- **`log_migration_storm`** — the gate for §2.3a, and the one §9.1 cannot supply
  on its own: `log-storm` from **kernel threads at preempt depth 0 with `IF`
  set** rather than from a tight per-CPU loop, at `--smp 8` with stealing on, so
  a producer is preempted and re-runs on a sibling. On a tree whose reservation
  is unbracketed, two CPUs read-modify-write one `head` and §9.1's conservation
  law fails. The `log-unbracketed-reserve` actuator (§9.4) is what makes it red
  on demand rather than by luck.
- **`console_line_atomicity`** — in-guest and deterministic in shape: two
  processes, each writing a distinguishable 200-byte line in two `write` calls,
  1,000 iterations on two CPUs. The assertion is a **count of mixed lines equal
  to zero**, not a probability, **and `Serial::interleaved().is_none()` on the
  same capture** (§8.1). Under `console-unbuffered` both are non-zero well inside
  the iteration budget.
- **`pre_idle_wedge_speaks`** — a kernel feature that wedges deliberately in boot
  phase 3; the host asserts the console carries every line up to the wedge.
  Today it carries none, which is the entry this closes. Its verdict is content,
  not a duration, so it is not a `STALLED:` class.
- **`logd_gone`** — kill logd; the machine survives, `init: logd exited` reaches
  the console, kernel records keep arriving, a client that keeps printing does
  not die, and `Serial::interleaved().is_none()` on the capture.
- **`shutdown_last_line`** — the guest's last console line is the shutdown's own,
  and `/log` carries it.
- **`log_is_durable_after_fsync`** — the gate for §12.4's coupling, host-side
  against the volume: logd writes, `fsync`s, and the harness reads the *image*
  rather than asking the guest. It reds on a `SYS_FSYNC` that stops at the page
  cache, which is what it does today (§12.4).
- **`idle_loop_is_the_declared_body`** — a **host-side source gate**, not a guest
  test: `idle_loop`'s body and `execute`'s pre-`hlt` condition list are exactly
  the declared sets. A condition quietly re-added is invisible to every
  behavioural test. compl §20.4 struck it from that branch and gave it here
  (§12.1). **What it declares at L6 is not the end state**: the body is
  `log_health`, `reap_poisoned`, `pass` and three `#[cfg]` probes, and the
  pre-`hlt` list is `i8042::verdict_due` and `xhci::port_work_pending` — **no log
  condition survives this branch** (§8.1), and both of those are compl's C9 and
  C10. So the gate is written to be **amended by the other branch as each of the
  two goes**, which is what a declared-set gate is for: each amendment is a diff
  a reviewer reads.
- **`one_console_holder`** — a `cargo test --lib` gate over **all eleven
  manifests** (§5.1a), reading the *parsed* `ProgramConfig` and never the file
  text (§3.2): each declares at most one `console = true` program, each with a
  `[boot] start` lists `logd` in it, and `logread` appears in the `syscap` of
  `logd`, `console` and `test-runner` and nowhere else. A twelfth manifest added
  later fails it by default.
- **`nmi_does_not_log`** — a grep gate, two clauses:
  `kernel/src/arch/idt/nmi.rs` contains no `log!`, `alert!` or `emit`; and **no
  `log!` in `kernel/` has `!!!` in its format string** (§2.1), which is what makes
  the deletion of `has_alert`'s sentinel checkable instead of asserted.

### 9.6 Measurements this branch owes, that are not pass/fail

- **The boot A/B on the per-record CAS** (§1.4), `xhci_slow_connect`'s
  `Boot: complete` as the instrument, interleaved, on the source and never on an
  instrumented build — that issue's own second lesson. L1.
- **`Drain::Inline`'s cost on a real UART** (§4.2). L3.
- **The RMW budget**, counted rather than asserted, and **counted per mode**
  because the two differ:
  - `Drain::Thread` (everything after `scheduler::init`): loses one
    `compare_exchange_weak` per record, keeps a `pushfq`/`cli`/`popfq` bracket
    narrowed from the whole append to two instructions (§2.3a), and gains **one
    `SeqCst` fence and one relaxed load** per record. No locked RMW on the
    producer's path at all. The five locked operations of the post (§2.6a) are
    paid at most once per `klogd` park, by one producer, inside a second bracket
    of the same kind.
  - `Drain::Inline` (boot, 185 records on one CPU): pays `BackendGuard`'s
    `compare_exchange_weak` and its `cli` per record, plus the synchronous
    backend write. Not a saving and not meant to be one; §4.2's measurement is
    what decides whether it is gated on `has_console()`.

  L1 counts both; L9 measures. **The fence is L3's to A/B**, on the same
  instrument as §1.4's — `xhci_slow_connect`'s `Boot: complete`, interleaved —
  because it arrives with the wake and not with the ring, and it is separable
  behind a `#[cfg]` if that chunk moves the number.
- **`LOG_FILE_DRAIN_NANOS` re-derived** against a userland writer under
  `usb-slow-device` (§6.4). L6.

---

## 10. Work breakdown

Eleven chunks, one branch, one pull request. **Every chunk builds, boots and
passes `cargo test`**, plus `cargo test` inside `kernel-loom/` and `toyos-abi`'s
host tests where it touches them. That constraint is what dictates the order:
`log_file.rs` survives, re-pointed, until logd replaces it, so `/log` is never
unwritten across a chunk boundary. **It also dictates one thing inside L3** —
deleting `SerialWriter` there while `ConsoleObject`'s buffer arrives at L5 leaves
userland console output with no path for two chunks, so L3 re-points it (§8.1).
That was the one place the constraint was stated and then broken.

**Merge cadence.** `git merge --no-ff origin/main` at L0 and at every chunk
boundary that follows a landing on `main`, and at minimum once a week. Never
rebase, never amend.

| # | chunk | delivers | gate |
|---|---|---|---|
| **L0** | merge `origin/main` (**post-endowment, and that is the whole of it** — completions lands after this branch, §12). Re-derive every number in this spec against the merged tree; re-check §12.5's endowment rows against the merged *code* and not only its plan; compute the first clean syscall number. **Nothing about the completion core is confirmable here and L0 must not wait on it** — §2.6a needs nothing from it. **Done 2026-08-11 against `f1724a9`; §0.0 is the walk** | suite green; §0.0 accounts for every moved row; the baselines of §9.3 recorded here |
| **L-ABI** | **its own branch off `main`, landed before L1** (§11). `toyos-abi/src/log.rs` — `LogRecord`, `Level`, `LogCursor` with its clamped `durable`, `MAX_LOG_SHARDS`, the two layout `const` assertions and the `Display` of §3.3; `SYS_LOG_READ = 114`; `Rights::LOG`; the `toyos` SDK wrapper. **No kernel dispatch**: there is no shard to read until L1, so the number falls to the dispatch's default and answers `InvalidArgument`, which is what an unassigned number answers | `cargo test` in `toyos-abi/`: the layout asserts, and the `Display` rendering a known record byte for byte |
| **L1** | `kernel/src/log/`: `Slot` over L-ABI's `LogRecord` and `Level`, `Shard`, `emit` with §2.3a's bracket, `log!`/`alert!`/`boot_phase!`, the seven `alert!` conversions. Wired **behind** the existing byte ring — every `emit` also does today's `write_chunk`, so nothing observable changes. The `KernelArgs` `{:?}` site split into several records. The AP shard in `alloc_percpu` (§2.2) | `kernel-loom/tests/log_record.rs` (W1, W2, W2b, W4); `log_conservation`; `log_nested_emit`; `log_migration_storm`; `nmi_does_not_log`'s second clause; the boot A/B of §9.6 |
| **L2** | the two readers; the `toyos-abi` formatter; `panic_console`, `boot_checkpoint` and `sched::dump` re-pointed at records; `Mark` → `Instant` | `screen_panic_muted`, `screen_fatal_halt`, `screen_fatal_halt_composited`, `blocked_dump`, `screen_blocked_dump`, `dump_nmi_probe`, `screen_console_*` all green |
| **L3** | `Drain::{Inline,Thread}`; **the kernel-thread machinery for one thread** (§4.3) — trampoline, kernel-address-space `ProcessObject`, `driver::spawn`, dump naming, the recoverable-panic predicate with `klogd`'s non-recoverable row; `klogd`'s body and §2.6a's wake (`commit_and_signal`/`arm_waiter`, the IRQ-off `PreemptGuard` witness, `wake_direct`, the park-lot park); `panic_flush`/`flush_final` on records; `log_file::poll` re-pointed at a `drain_ordered` cursor so the file sink survives; **`object/ops.rs:469`'s console arm re-pointed straight at `BackendGuard`** (§8.1) so the chunk builds. **Delete `log_ring.rs` whole**, `SerialWriter`, `drain_serial` and the idle loop's serial statement; **delete the `:523` pre-`hlt` condition** | `kernel-loom/tests/log_wake.rs` (W3, with its negative case); `pre_idle_wedge_speaks`; the `--slow-usb` A/B unmoved (nothing about the disk has changed yet); §9.6's `Drain::Inline` measurement and the fence's A/B |
| **L4** | the kernel's half of L-ABI, which touches no sysroot source: the `SYS_LOG_READ` dispatch over `drain_ordered`, `Source::Log` and its watcher static (§3.2), `logread` in `toyos_manifest`'s `SYSCAP_RIGHTS` and on `test-runner`'s row in the six test configs that have one | `test-runner` reads its own kernel log and §9.1's conservation law holds across the syscall |
| **L5** | one `ConsoleObject` per endowment over one backend, its line buffer, `Console` losing `DUP`; the ANSI strip moves; `MAX_CONSOLE_LINE` | `console_line_atomicity`; `console-unbuffered` reds both its clauses; `one_console_holder` |
| **L6** | `/bin/logd`: the program, **its row in all eleven manifests** (§5.1a) including `diag/`'s and its restated comment, the protocol, rotation, retention, per-client backlog, the give-up policy on today's live 2 s transport bound (§5.4); **`SYS_FSYNC`'s device flush, outright — there is no C12 to hand it to (§12.4)**. **Delete `log_file.rs` whole**, `flush_log_file_if_affordable` and everything in §8.1's `driver.rs` list; `wait_for_log_file` re-pointed at `LOG_DURABLE_NS`, **including `apic.rs:146`'s comment and its kick loop, which name the idle loop's `log_file::poll`**; §6.3's shutdown | `kernel_log_file` re-pointed and green mid-run and after shutdown; `log_is_durable_after_fsync`; `screen_fatal_halt_composited`'s `/log` half green; `logd_gone`; `shutdown_last_line`; `idle_loop_is_the_declared_body`; `log-writes-the-file` and `log-trusts-durable` red |
| **L7** | userland stdio → IPC: init creates and registers the pipes; the launcher and sshd do the same for what they spawn; the std PAL's `Gone` behaviour; every console assertion in the suite re-pointed | full suite; the 234 `println!`/141 `eprintln!` sites unchanged and their output still on the console |
| **L8** | the deletion commit; the `specs/issues/` closures of §8.2 **and the citations that go stale with them**; `specs/introspection-plan.md` re-based (§3.4); the three `MAX_CPUS` declarations filed as an issue; all five `CLAUDE.md` files. **`specs/completion-architecture-spec.md` is not in this tree** (**CONFIRMED**: it exists only on `wt/toyos-compl`, not on `origin/main`), so L8 cannot de-path its citations and `every_named_issue_file_resolves` will not see them — but it cites three of these slugs by full path and that becomes *its* C0's red, which §12.7 records so it is not discovered by CI | `cargo test --lib` — `every_named_issue_file_resolves` is the gate, not "it compiles" |
| **L9** | measurement: the interleaved four-arm A/B (compl §20.1's protocol, ~68 min of guest time, two worktrees); `io-depth-probe`; the positive log-content assertion; assertions written into `tests/audio-baseline.toml` and the numbers into this spec | §9.3 |
| **L10** | **conditional on the owner (§6.6, §13.1)** — pstore: the reserved region, the panic copy, the boot validate, the `SYS_LOG_READ` flag, logd's `prev-crash` file, `pstore_survives_reset` over QMP, and the `specs/issues/` entry recording that the metal arm is owed | its own; a red here is not a red on L1–L9 |

**Dependencies.** L-ABI → L1. L1 → L2, L3, L4. L3 and L4 → L6. L5 → L7. L6 → L7.
L8 after L7. L9 last. L10 independent of everything after L4.

---

## 11. The ABI split, and why the trailer would be a false claim

`toyos-abi` changes once: `log.rs` (the `LogRecord`, `LogCursor` and the
formatter), `SYS_LOG_READ`, and one `Rights` bit. Its callers — the kernel's
dispatch, logd, the console, test-runner — arrive in L4, L6 and L7. Root `CLAUDE.md`: *"An ABI change
lands on its own pull request first … both `--pr` and CI's `abi-split` check
refuse a branch that mixes the two — an `Abi-Inseparable: <why>` commit trailer
declares a split that genuinely cannot be made, out loud."*

**Mine can be split, so the trailer would be a lie.** L-ABI is a syscall number
nobody dispatches yet and the types it will answer with. It compiles, boots and
is testable on its own — §9.1's conservation gate runs against it from
`test-runner`, which holds `logread` from its own manifest row and needs no logd
in the picture (§3.2). **Verified** rather than assumed: that is the one thing
that could have made L4 untestable alone, because a spawned test binary does not
inherit a `SysCap` dup.

**The ABI-only pull request comes *first*, not between L4 and L5, and the merged
tree is what decides that.** `pr::abi_lands_alone` offers the
`git switch -c <branch>-abi <sha>` remedy only when the sysroot commits are a
**prefix** of the branch (`src/pr.rs:338-353`); a branch whose sysroot commits
sit after non-sysroot ones is told the split "cannot be made without
reordering — and nothing in this workflow rebases", with no remedy but the
trailer. L1, L2 and L3 touch no sysroot source, so an L4 in the middle is
precisely the shape that has no way out. **So chunk zero is the ABI**, on its own
branch off `main`: `toyos-abi/src/log.rs`, `SYS_LOG_READ = 114`, the `Rights`
bit and the kernel's implementation of the syscall; merged; then `cargo run --
--sync`; then L1 onward on `wt/toyos-logd`. §10's table is renumbered
accordingly — L4 becomes L-ABI and moves to the front, and L1's `LogRecord` and
`Level` are `toyos-abi`'s from the first line rather than being moved later.

That costs one extra landing and keeps `abi-split` honest. The alternative — one
pull request with

```
Abi-Inseparable: <a reason that would not be true>
```

— is not available, and this section exists so that the implementing agent does
not write one because the pipeline said "one PR". **The owner decides which**
(§13.2); the branch is structured so either works, because L4 is a chunk boundary
either way.

Note also: the sysroot claim. `toyos-abi/src` differing from `main`'s is what
makes `buildlock` refuse every other worktree, and only a checkout whose
`toyos-abi`/`toyos` genuinely differ may claim it. **The first draft said the
ABI-only PR holds the claim "for one chunk instead of six"; the true statement is
stronger.** The claim is held for as long as the divergence exists, so with the
ABI-only PR it is held across L4 and then *released by the merge* — L5 through L9
touch no ABI and refuse nobody. Without it, one worktree holds the claim from L4
to the single landing at the end of L9, which is the whole of the rest of the
branch and every other worktree's build with it.

---

## 12. Coordination — what this branch leaves the completion branch

**Two branches, two directions, and the ruled order decides which is which.**
Checked against `origin/wt/toyos-compl` at **`9890d5e`** and
`origin/wt/toyos-endow` at **`8231e90`**, both read 2026-08-09.

- **§12.5 is a dependency.** The endowment branch lands *first*, so its rows are
  things this branch consumes and **L0 re-checks every one against the merged
  tree; a row that has moved is a red for this spec**. That is unchanged.
- **§12.1 to §12.4 and §12.6 to §12.7 are obligations, not dependencies.** The
  completion branch lands *second*, so nothing in them is re-checkable at L0 and
  L0 must not try. They are what that branch will find, and what it owes. The
  hash is here because the obligations are quoted from a document that is still
  being edited.

**`specs/completion-architecture-spec.md` is not in this tree.** **CONFIRMED**
2026-08-09: `git ls-tree origin/main specs/` does not carry it, and it exists
only on `wt/toyos-compl`. Every citation of it below is a citation of a document
on another branch, which is why they are by section and not by line.

### 12.0 The order, and what it decided

**Ruled 2026-08-09: `endowment → log → completions`.** The prologue argues it;
this subsection records what the ruling settled and what it did not.

- **Settled: `kernel/src/log/` is built once, here.** The earlier concern — that
  compl's C9 would build a 64 KiB byte ring with three sinks and L1–L3 would
  delete it — is moot in a second, stronger way: C9 is written against a tree
  that already has the record ring. §13.5's strike stands and is now belt to the
  ordering's braces.
- **Settled: §11.4's obligation on that branch is discharged.** Its C7+C8 cannot
  leave the kernel file sink on the idle loop, and it does not have to: L6 has
  deleted it. Its two shapes — the drain onto `iod`, and the append staying on
  the idle loop behind C12 — are both struck, and the trace that justified them
  stays there as evidence.
- **Settled, and it cost nothing: §2.6a needs no completion primitive.** The
  lock-free single-waiter post is `toyos_sched::waitq::wake_direct` and has been
  in the tree all along, so `emit` posts at the commit, no pre-`hlt` condition
  survives, and this branch inherits and owes nothing on the wake. The ruling
  therefore rests on the *compilation* blocker alone, which is the stronger half
  of the argument that produced it.
- **Not settled, and it is the other cost: L3 builds the kernel-thread
  machinery** (§4.3). C6 does not exist to inherit from. That is the largest
  single thing the reorder moves and neither spec had planned for it.

### 12.1 What this branch takes off compl's C9, and what C9 finds already done

C9 was *"`kernel/src/log/`: core + three sinks + `logd`. Every deletion in §11,
minus `reap_poisoned`"*. **All of the following is this spec's; §13.5 records
that the rows were struck on that branch while it was open, and under the ruled
order C9 additionally finds each of them already built or already deleted:**

| C9 item | lands as |
|---|---|
| `kernel/src/log/` — the core and its file layout | L1 (and it is a **record** ring, not the 64 KiB byte ring its §11 assumed) |
| the serial sink and its drain | L3 |
| the file sink | **deleted, not rebuilt** — it moves to userland, L6 |
| the panel sink | L2 |
| `flush_log_file_if_affordable`, `LOG_DEFERRAL_CEILING_NS`, `LOG_DEFERRED_SINCE`, `log_file_flush_due`, `owes_wake` | L6 |
| `drain_serial` from the idle loop, and its IRQs-off `BackendGuard::lock` spin | L3 |
| the pre-`hlt` condition `log_file_flush_due` (`:552`) | L6 |
| the pre-`hlt` condition `log_ring::has_pending` (`:523`) | **L3 deletes it** (§8.1, §2.6a), with nothing in its place: the commit posts `klogd`'s wake, so the doorbell and Invariant T refuse the halt. C9 finds two conditions in that arm, both its own |
| `log_file.rs::SINK` from its §9 lock table and its §19 ledger | L6. **Not "conditionally"** — its §9 leaves the row hanging on §11.4's choice; after L6 there is no `SINK`, no `Lock` and no fifth row, and its lock table converts four statics |
| `MAX_BLOCKED_NANOS` (`log_file.rs:190`) and `LOG_DEFERRAL_CEILING_NS` from its §3.2's classification sweep | L6, with the file they are in — and **before its C1 runs**, so C1 has nothing to classify and its count of 41 production durations drops. C1 re-derives it rather than taking a number from here |
| `idle_loop_is_the_declared_body` | L6, and amended twice afterwards (§9.5) |
| `apic.rs:160`'s `wait_for_log_file`, its `:146` comment naming the idle loop's `log_file::poll`, and its kick loop | L6. Its §11.4 gave these to C7+C8; there is no idle-loop `poll` left for C7+C8 to re-point |
| its §20.3's `reintroduce-idle-flush` negative control | **retired, not inherited** (§9.4). After L6 there is no `log_file::poll` to put back |
| its §19's *"One name may come back"* — `flush_log_file_if_affordable` and `SINK`'s `Lock` dying at C7+C8 instead of at L6 | **void.** They die at L6 |
| the three `specs/issues/` slugs its §19 says left its ledger — `client-cpu-takes-the-log-flush` (audio), `log-flush-is-unbounded` (boot-media), `pre-idle-wedge-says-nothing` (diagnostics) | **L6, L6 and L3** — the same rows §8.2 carries. Its §19 is right that this table omitted them and it was the only place the transfer was written down; that is fixed here, and its §19's *"§1.3's citation goes stale at log L8, not at C13"* becomes *"goes stale before its C0"* under the ruled order (§12.7) |
| its §11 conclusion *"A userland `logd` was considered and rejected"* | **overruled**, and §12.1a argues it on the merits rather than on the ruling |
| its §10 *"Three is one too few"* — the open choice between two `logd` threads and a bounded file-sink wait | **void.** There is no kernel file sink to split off, so `klogd` drains one backend and the question it asks cannot be posed. Its C6 thread table is `klogd`, `usbd`, `iod` with nothing left open |
| the kernel-thread machinery its C6 owns — trampoline, kernel-address-space `ProcessObject`, `driver::spawn`, dump naming, the recoverable-panic predicate | **L3 builds it, for `klogd`** (§4.3). C6 spawns `usbd` and `iod` on it, adds their two predicate rows, and keeps the `KernelPayload.address_space` retype (its §15 row 12), which L3 has no second caller to justify |
| its §12.3's *"the one-word change C6 owes"* — `logd` → `klogd` | **discharged before C6 is written.** The thread is `klogd` from L3 |

### 12.1a The overruling, argued rather than asserted

The owner ruled on 2026-08-09 and that settles it, but a spec that records only
the ruling gives the next reader nothing to check. **The two objections compl §11
raises are each answered by a mechanism, and compl's own review supplies a third
argument the first draft of this section did not have.**

- *"It cannot log the boot that precedes it."* `Drain::Inline` puts every boot
  record on the wire as it is written (§4.2), and cpu0's shard retains all 185 of
  them (§2.2, **CONFIRMED**) for logd to read and write to `/log` when it starts.
  That second half is not novel: `log_ring::enable_file_sink` already does exactly
  it one level down — *"seeded from `retained` … so the file opens with this
  boot's log from its first line rather than from the moment `/boot` mounted"* —
  so the objection is answered by the mechanism the current code already relies
  on, moved up one layer.
- *"It cannot log its own death."* logd's own dying words reach the **console**
  through its own `Console` handle, which is the kernel's and not logd's to serve,
  and init then names the exit (§5.7). What is genuinely lost is `/log`'s copy of
  logd's death, which §13.4 records as one of the three new regressions rather
  than waving away.
- **And compl's own second-stage review reached the same split from the other
  side.** Its §12.3: *"the parked thread is `logd`, and `logd` owns the serial
  sink too — so a hung `/log` stick costs the machine all logging … on the T14,
  which has no serial port … that is total logging loss with no line saying
  why."* That is the failure mode a kernel `logd` has and a userland one cannot:
  the kernel's drainer has no disk in it. The design that spec's review says is
  broken is the one it rejected the alternative in favour of, and this is the
  alternative.

### 12.2 What stays with the completion branch

- **`scheduler::log_health()`** (`driver.rs:679`) — **it stays in the idle loop,
  and that is settled by compl §11.1 rather than open.** This spec's earlier
  draft asked that branch to put it on `iod` or give it a cadence source, on the
  premise that C9 was about to empty the loop. That branch answered: `NEXT_HEALTH`
  is per-CPU because *"which CPUs reach idle is most of what the line says"*, and
  `ready_len`/`parked_len` read the caller's own `!Sync` `CpuSched`, so a single
  thread can only ever report the CPU it is on; the line's own doc forbids reading
  it as a heartbeat, which a periodic park would make it; and a 10 s park is a
  wake on a machine that would otherwise halt. **The answer is right and this spec
  adopts it.** L6's `idle_loop_is_the_declared_body` therefore declares
  `log_health` as part of the body, and the two tests that read its `sched: cpu=`
  counts (`tests/toyos.rs:8229`, `:8722`) stay live and unmodified across this
  branch.
- **`scheduler::reap_poisoned()`** (`driver.rs:680`) — compl §11.2 establishes it
  cannot move (`IdleProof`), and this spec does not touch it. The idle loop's end
  state after L6 is therefore `log_health`, `reap_poisoned`, `pass` and three
  `#[cfg]` probes.
- **`i8042::verdict_due` (`:534`) and `xhci::port_work_pending` (`:564`)** — the
  other two pre-`hlt` conditions. Neither is a log condition; both become a
  `usbd` deadline park or a `Poll`, which is that branch's C9 and C10 work, and
  both are still standing when this branch ends.
- **`poll_if_pending` leaving `drain_irqs`** — its C7's.
- **The `KernelPayload.address_space` retype** — its C6's, per §12.1.

### 12.3 The one-word change is already made

That branch's C6 table spawned `logd`, `usbd` and `iod`, and owed the rename to
`klogd` because `/bin/logd` is a userland program in the same machine and
`sched::dump` names threads. **Under the ruled order the thread is `klogd` from
L3 and there is nothing to rename** — C6 finds it spawned, named and dumped, and
adds two threads beside it. The reason for the name survives because a later
agent will otherwise re-collide them: two things called `logd` in one report is a
collision a dump cannot survive.

### 12.4 C12 — the write-back queue, and the device flush L6 now owes outright

**C12 stays entirely with the completion branch.** It is `fd::OpenFile::drop`,
the `iod` write-back queue, `FileObject::on_zero_handles`, `SYS_FSYNC` parking,
page-cache eviction, its §13.1's page pinning and `close_file`, and the
`disk_backtrace` / `esp_files` obligations. None of it is log-specific, and the
reason it appeared adjacent is only that `log_file::Sink::append` was one of its
callers.

**What this branch removes from C12's surface, and landing first makes it
already-true rather than promised.** `Vfs::flush_file` (`vfs.rs:538`) has exactly
three callers today: `fd::OpenFile::drop` (`fd.rs:48`), `fd::fsync` (`fd.rs:644`)
and `log_file.rs:376` — **CONFIRMED** 2026-08-09. The third is the only one not
reached from a syscall, and L6 deletes it. So C12 opens on a tree where every
`flush_file` in the kernel is userland-driven and its queue has one class of
producer instead of two.

**The device-level flush is the coupling, and the order change makes it L6's
outright.** logd's durability claim (`LOG_DURABLE_NS`, §6.4) is only true if
`SYS_FSYNC` means what it says on the log volume, including the device's own
cache flush. **CONFIRMED against this tree, 2026-08-09:**

- `log_file.rs:376` is `vfs.flush_file(...)` **and then** `vfs.sync_mount(MOUNT)`
  at `:382`, whose own comment is the reason — *"The FAT and the directory entry
  have reached the device; the device's own write cache has not. A log that
  survives a wedge has to survive the power being cut with it."* `sync_mount`
  (`vfs.rs:698`) reaches `Fat32::sync` (`toyos-fat32/src/fs.rs:901`), which writes
  FSInfo and then calls **`self.dev.flush()`** at `:908` — the block device's
  flush, SCSI SYNCHRONIZE CACHE on a stick.
- `fd::fsync` (`fd.rs:639`), which is what `SYS_FSYNC` dispatches to, calls
  **only** `crate::vfs::lock().flush_file(...)` at `:644`. There is no
  `sync_mount` and no `dev.flush()` anywhere on that path.

So today's `SYS_FSYNC` stops one level short of what `log_file` does. **The
earlier draft said "L6 owes the equivalent regardless of what C12 does to
parking"; under the ruled order the hedge is gone and L6 owes it, full stop** —
C12 does not exist yet, and a logd that fsyncs and calls the result durable
without it is a spec lying about its own guarantee. That is a change to a shipped
syscall's semantics and therefore a decision rather than an implementation
detail: either `SYS_FSYNC` gains the mount sync for every caller — which makes
every `fsync` in the machine slower and more honest — or logd gets a distinct
call and the asymmetry is written down. **L6 raises it; the second option needs a
new syscall number, which needs discussion, and it would be a *second* number on
a branch §11 has already structured around one.** `log_is_durable_after_fsync`
(§9.5) is the gate, and it reds on today's behaviour.

**What C12 then inherits.** compl §13 says *"`SYS_FSYNC` parks on a real
completion"* and separately that parking is not durability. After L6 the two
questions are answered in the two places: L6 settles what `SYS_FSYNC`
*guarantees*, C12 settles what it *costs a CPU*. C12 must not undo the first
while changing the second, and `log_is_durable_after_fsync` is what would catch
it.

### 12.5 The endowment architecture

Every row **CONFIRMED** against `8231e90`, including the manifest parser as
built (`src/build.rs:47`, `ProgramConfig`) rather than only against the spec text.

| what this spec needs | how it stands |
|---|---|
| `logd` as a manifest program with `serves = ["log"]` | expressible as built: `serves` is a `Vec<String>` on `ProgramConfig` and `[boot] start` exists |
| `receives = []` for logd | the default; it needs no row |
| `Rights::LOG` on `SysCap` | its §1.4 is nine bits in a `Rights(u32)`; a tenth collides with nothing |
| a way to say `logread` and `console` in a manifest | **the merged code answers differently from the plan.** `realtime` is gone: authority is `syscap = [...]` over `toyos_manifest::SYSCAP_RIGHTS`, which refuses an unknown name, so `logread` is a row there and not a key. `console` is still a `#[serde(default)] bool` — §3.2, §5.1 |
| init creating and endowing every child's stdio pipes | its §4.5's `MSG_LAUNCH` already carries *"the stdio handles … they travel as transferred handles and init installs them at slots 0/1/2"*; the pipes to logd are what those handles are |
| `SYS_HANDLE_SEND` for the pipe read ends | its §3.1, number 103 |
| the syscall block | this spec takes one number after theirs, computed at L0 and written nowhere (§3.4) |

**Two rows contradicted that spec's §1.5, and the merged *code* settled both in
this spec's favour — L0 checked the code rather than the plan, which is what it
was for:**

- **endow §1.5 says of `ConsoleObject` "there is exactly one of them for the
  machine".** The code does not: `ConsoleObject::new()` mints a fresh object per
  call (`object/device.rs:202`) and there is one call site. So "one *backend*,
  one object per endowment" needs no edit to anything — only a second call
  site, which §4.4 puts in `spawn_init`.
- **endow §1.5 gives `Console` `READ|WRITE|DUP|TRANSFER|WAIT`.** As built it is
  `BASE | READ | WRITE` (`object/ops.rs:50`), and `BASE` carries `DUP`. §4.4
  drops `DUP`, so "exactly two" is structural rather than gated. L5's edit, one
  arm.

Neither touches a syscall number or a struct layout.

**And two programs this spec endows are not in the manifest it would endow them
from** — `console` and `test-runner` are in `console/system.toml` and the three
`tests/*case/system.toml` respectively, never in `system.toml`. §5.1a is the
six-manifest table that follows from it.

### 12.6 Every completion primitive this branch does without

One table, so an implementer never reaches for something that is not there and a
reviewer can check the conversions off afterwards. **None of these is optional
scope this branch declined — each is a type or a mechanism the completion branch
introduces, and the ruled order puts that branch second.** The one exception is
the struck row: it was in the tree the whole time, and it is kept visible because
a spec that quietly drops a wrong claim teaches the next reader nothing.

| primitive | what this branch does instead | what the completion branch converts |
|---|---|---|
| the completion core (`Record`, `Inbox`, `arm`, `wait`, `post`) | the wake needs none of it: `emit` posts through `toyos_sched::waitq::wake_direct` and `klogd` parks with `prepare_wait`/`block_on` (`scheduler.rs:170`, `:181`) on a park-lot bucket (§2.6a) | **nothing.** C2+C3 may re-express the park as an `Inbox` park if that unifies something on its own side; it converts no log code, inherits no obligation, and W3 (§2.5) is already paid here |
| ~~the lock-free single-waiter post~~ | **it was never missing** — `wake_direct` (`waitq.rs:236`) is exactly it, and the row is kept struck rather than deleted so the next reader does not re-derive the fallback | nothing |
| a userland readiness source for `SYS_LOG_READ` | `Source::Log`, a ninth `io_uring::Source` with the sixth per-source `IO_URING_WATCHERS` static (§3.2) | its C3 folds all six statics into one watch list and its §19 deletes them together |
| kernel threads (its C6) | **L3 builds the machinery for one** (§4.3): trampoline, kernel-address-space `ProcessObject`, `driver::spawn`, dump naming, the recoverable-panic predicate | C6 spawns `usbd` and `iod` on it, adds their predicate rows, and does the `KernelPayload.address_space` retype |
| `Parkable` (its C5) | nothing needs one: no path this branch adds takes a sleep lock, and §7's diagnostics take no `Lock` at all after L3 | the token arrives and every one of §7's contexts still lacks it, so a diagnostic that blocks stays untypeable — this branch does not weaken that |
| `SleepLock` (its C5) | nothing: `BackendGuard` stays a spin (compl §4.5a keeps it on the allow-list either way) and `klogd` holds it for one bounded chunk | its C7+C8 converts `VFS`, `VOLUMES`, `XHCI` and `ProcessData`; **none of the four is on any path this branch adds** |
| the six duration kinds (its C1) | `LOG_WRITE_BUDGET` is a named `u64` in a userland program with its reason beside it (§5.4); RT7 reaches `kernel/src/` and logd is not in it | nothing to convert. `LOG_FILE_DRAIN_NANOS` is the one kernel duration this branch touches and L6 re-derives its *value*, leaving C1 to give it a *kind* |
| a caller-side write `Deadline` (its §12.3) | the live 2 s `USB_TIMEOUT_NS` (`xhci/mod.rs:319`, **CONFIRMED**) gives logd an `Io` to act on | whatever C7 picks, **it must leave logd an error rather than an unbounded park** (§5.4) |
| the write-back queue (its C12) | `SYS_FSYNC` gains the mount sync at L6 (§12.4) | C12 changes what `SYS_FSYNC` *costs*, never what it *guarantees* |

### 12.7 One thing this branch breaks on the other branch, named so CI does not find it

`src/docs.rs`'s `every_named_issue_file_resolves` walks every text file in the
tree and reds on a `specs/issues/<area>/<slug>.md` path that does not resolve.
L8 deletes ten entries (§8.2) and de-paths every citation **in this tree**.
`specs/completion-architecture-spec.md` is not in this tree, and its §19 records
that it cites `client-cpu-takes-the-log-flush` by full path in its own §1.3, that
`specs/introspection-plan.md` cites `log-flush-is-unbounded` by full path twice
(`:31`, `:56`), and that seven of its twelve remaining slugs are cited by full
path from files that are not their own entry.

Its §19 concludes that *"§1.3's full-path citation therefore stays live through
this branch and goes stale at log L8, not at C13."* **Under the ruled order that
is backwards**: L8 lands first, so the citation is already dead when that branch
merges `origin/main` at its C0, and `cargo test --lib` reds on its own spec file
the moment it does. It is a one-line edit and it is in nobody's chunk budget, so
it is written here: **the completion branch's C0 de-paths every citation of a
slug this branch closed, in the same commit as the merge.** The
`specs/issues/README.md` protocol — move the durable rule into the spec that owns
the subject — is the same edit.

---

## 13. Owner-level decisions

**All five were put to the owner on 2026-08-09 and he agreed with every
recommendation.** Each subsection keeps its argument, because a decision whose
reasoning is not written down is one the next agent re-opens; the **Ruled** line
under each is what binds. Nothing in §13 is an open question any more.

### 13.1 pstore — build it or not

**Ruled 2026-08-09: yes, as L10, last, with the metal arm filed as owed.**

§6.6 has both columns. **Recommended: yes, as L10, last, gated on this answer,
with the metal arm filed as owed.** The gate it can have proves the format and
the code path and says nothing about firmware, and the case it covers is the four
panics where the panel is the only copy — which on the T14 means a photograph.
The costs are 128 KiB of reserved RAM, a `KernelArgs` field, a bootloader change,
one `SYS_LOG_READ` flag, and a promise that is best-effort on real hardware.

### 13.2 One pull request, or an ABI-only one first — §11

**Ruled 2026-08-09: the ABI-only pull request at L4, then the rest on this
branch.** The owner's one-branch-one-PR ruling stands wherever its premise holds;
here it does not, and he said so rather than making the trailer carry a false
reason. So L4 lands alone and the sysroot claim is released at that merge instead
of being held for five more chunks. **L0 moved it from "at L4" to "first"** — the
decision is unchanged and the position is forced by `abi_lands_alone`'s prefix
rule (§11), not chosen.

The pipeline's instruction is one branch, one PR. Root `CLAUDE.md`'s instruction
is that an ABI change lands first, and the `Abi-Inseparable` trailer is for
splits that genuinely cannot be made. **Mine can**, so the trailer would be a
false claim. Recommended: an ABI-only pull request at the L4 boundary, then the
rest on the same branch. The alternative is to accept the trailer with a reason
that is not true, which is worse than the extra landing.

### 13.3 Memory — 1 MiB of per-CPU record ring at the shipped 8 CPUs

**Ruled 2026-08-09: accepted.** The 16 MiB at 128 cores is accepted with it, and
the named escape stays available rather than pre-emptively taken.

§2.2. Today's log costs 64 KiB. The increase buys fixed-size records (which is
where the atomicity property comes from) times eight CPUs, and 512 slots so
cpu0's whole boot survives until logd runs — 185 records measured, 2.7× headroom.
16 MiB at the 128-core target. The escape is one line and is named. Note that
seven eighths of it is seven 128 KiB `alloc_zeroed`s from the kernel heap at AP
bring-up, which is where the idle and IST1 stacks already come from.
**Recorded rather than asked — and then put to him anyway; see this section's ruling above.**

### 13.4 The regression this design accepts, stated plainly

**Ruled 2026-08-09: accepted, and §13.1's pstore is what closes it.** The three
new cases are accepted as new — not as "the same as today", which is the sentence
this subsection exists to have retracted. L10 is therefore not optional polish:
it is the item this acceptance is conditioned on.

If `wait_for_log_file` (§6.4) does not do its job, a T14 fatal panic leaves the
report on the panel and not in `/log`. **The first draft said "today the same
four cases have the same outcome, so this is not new", and that sentence was
doing all the excusing and was not true.** It is true of four cases and false of
three more, and the difference has a name: today's drainer is the **idle loop**,
which `reap_poisoned`'s own doc comment (`scheduler.rs`, quoted by compl §11)
calls *"the one context that provably holds none of the locks the panicking
thread may have been holding"*. logd is not that context. It is an ordinary task,
and it needs everything an ordinary task needs.

**Unchanged, and genuinely the same as today** — a #DF, a triple fault, a panic
in the writer itself, a panic whose thread holds the VFS lock.

**New with this design, and each is a way a dying machine loses its own report
that has no analogue today**:

1. **logd has to be picked by a scheduler.** The idle loop needs a kick and one
   trip round a loop; logd needs a runnable task chosen on some sibling. A panic
   that happened inside the scheduler, or on a CPU holding a run-queue lock, can
   leave nothing able to choose it.
2. **logd needs three locks the panicking thread is not the only candidate to
   hold, and a fourth once the completion branch lands.** VFS, the volume and
   the controller at L6; the write-back queue when its C12 puts one between logd
   and the disk. Any wedged CPU holding any of them is enough, where the old
   sentence only considered the panicking thread and only the VFS. **The count
   was four because this spec assumed C12 had already landed; under the ruled
   order it is three at L6 and grows to four later**, which does not soften the
   case and is stated because a number nobody can reproduce is worse than none.
3. **logd is killable and restartable-by-nobody.** §5.7: no `SYS_PORT_REARM`, so
   a logd that died earlier in the boot for an unrelated reason means every later
   panic on that boot is panel-only, and init's one line about it is minutes
   back in the log.

Against those: the design also *removes* a failure this path has today, because
`wait_for_log_file`'s drainer no longer runs with `log_file::SINK` → `vfs::VFS` →
`VOLUMES` → `XHCI` held from an idle loop that cannot be preempted.

**So the trade is real and it is a trade, not a wash.** §13.1's pstore is what
closes it properly, and it is the reason that recommendation is a recommendation
rather than a nice-to-have. **Recorded rather than asked — and then put to him anyway; see this section's ruling above.**

### 13.5 C9's log rows — **struck, 2026-08-09; and then the order was ruled**

**Resolved twice, and the second resolution subsumes the first.** The premise —
that the completion branch implements first and this edit cannot be made from
here — was true of the branch and false of the schedule: that branch is planned
and reviewed but **parked** behind the endowment branch, no agent holds it, and
the orchestrator struck the rows on it directly. **Then, the same day, the
orchestrator ruled the pipeline order to `endowment → log → completions`**, which
makes the strike belt to the ordering's braces: C9 is now written against a tree
that already carries the record ring, so there is no version of it that builds a
64 KiB byte ring and no duplicated work to avoid.

The general lesson is worth more than the item: **a spec that says "another
branch must do X" has named an owner who does not exist.** Cross-branch
obligations belong to whoever schedules the branches. Saying so out loud got the
strike made instead of discovered at L0 — and it is also what surfaced the
sequencing question the strike could not answer, which is §12.0's ruling.

---

## 14. Explicitly rejected

Recorded because each is attractive and the next reader will re-derive it.

1. **A raw TSC in the record instead of nanoseconds.** Saves the producer one
   `__udivti3`. Costs the panel and the dump the ability to render a record
   without a period they would have to be given, on a machine where the clock
   subsystem may be exactly what died. §2.1.
2. **A variable-length record over a byte ring with a descriptor ring.** ~2.8×
   more space-efficient at the measured average line, which clears the >2×
   simplicity bar on that one axis — and loses on the axis the whole design is
   for: "written whole or not at all" becomes an argument about two structures
   staying consistent rather than a property of a type. §2.1.
3. **logd owning the 16550.** Tempting because it makes one process the only
   writer and the merged stream perfectly ordered. It puts the dev host's and
   CI's instrument behind a userland process, so a logd that is slow or dead
   takes the kernel's own output with it — and it is the panic and pre-scheduler
   channel, which userland cannot serve at all. §4.1.
4. **One kernel `logd` thread draining both sinks**, which is compl §10's shape.
   Its own §22 records the failure mode as unresolved: a park on a dead stick
   takes the serial sink with it, on the machine with no serial port. Splitting
   at "the kernel never writes a file" removes it by construction. §4.1.
5. **A global ring with a CAS reservation.** One locked RMW per record, which
   `one-rmw-per-log-line-cost-350ms` prices at hundreds of microseconds each
   under TCG. §1.4.
6. **A per-record wake with no fence, backstopped by a bounded park.** That is a
   timer hiding a lost wake, which the completion architecture forbids by name.
   §2.6a is a per-record wake **with** both fences, W3's model is what proves
   they are needed, and nothing in it is backstopped: no deadline, no pre-`hlt`
   condition, no pass that re-evaluates a predicate. §2.5.
7. **A harness-side splice *repair*.** The issue itself shows why: the second
   recorded occurrence had a *userland* intruder that `is_kernel_line` cannot
   identify, and the capture ended inside the split line, so no reassembly on the
   captured text could recover it. The fix has to be guest-side, and §4.4 is it.
   **This is not an argument for deleting the harness's splice *detector*, and
   the first draft used it as one** — §8.1 keeps `Serial::interleaved` and gives
   it teeth.
9. **`preempt::disable()` around the reservation instead of the flags bracket.**
   It buys the same property and costs `lock add` + `lock sub` per record, which
   is two locked RMWs where §1.4 prices one at 350 ms of boot. §2.3a. **The same
   choice for the same reason on the wake post** (§2.6a), where `preempt_off`
   would additionally put `enable`'s `need_resched` poll — a scheduling pass —
   inside `emit`.
10. **No bracket at all, on the ground that `log!` mostly runs with `IF` already
    clear.** True of every syscall and every IRQ handler, and false of a kernel
    thread at preempt depth 0 — of which L3 adds the first, `klogd` itself, and
    completions' C6 two more. A correctness argument that holds for most callers
    is not one. §2.3a.
11. **Making `emit` post a completion through the completion core's ordinary
    path.** It walks a list under the subject's leaf lock, which `emit` may not
    take and which deadlocks the first time anything on that lock's path logs.
    The narrower thing §2.6a takes instead is `wake_direct`, which the scheduler
    has had all along.
8. **Converting all 654 `log!` sites to a finer level set.** A level with no
   reader is a field built for a plan; §2.1's three variants each have callers
   today.

---

## 15. Out of scope, named

- **The `kernel/src` layout question** — the second half of
  `redesign-the-log-subsystem`. `kernel/src` is 39 flat `.rs` files beside seven
  directories; this spec adds `log/` and removes two of the flat files
  (`log_file.rs`, and `drivers/log_ring.rs` from `drivers/`), which is a step in
  that direction and not the decision. L8 re-files it as its own entry so it is
  not lost with the one it is currently attached to.
- **A structured `/log` format.** logd writes text, byte-compatible with what
  `/bin/console` seeds its scrollback from today. A record-per-line binary format
  is expressible now that records exist and has no caller.
- **`log-follow` and the rest of `specs/introspection-plan.md` §3.** The cursor
  syscall is what that section needed; the tool is its own work.
- **Per-CPU `klogd`.** compl §10 flags single-instance kernel threads as an
  unsized serialisation point at 128 cores. `klogd` drains a 64-record batch per
  wake and does nothing else; per-CPU is the obvious escape and costs nothing to
  leave open.
- **The `#DB` handler that resumes a fault it cannot stop**
  (`rflags-tf-log-flood`). This bounds its blast radius and does not fix it.
- **Restarting logd.** `SYS_PORT_REARM` (endowment spec §12, number 113) closes
  it for every daemon at once, and building it for one is building a mechanism
  for a plan.
