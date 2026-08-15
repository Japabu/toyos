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
Two splices were recorded against it, one of which reds a landing gate on a
documentation-only branch, and a measured **1 run in 10** for
`desktop_audio_client` on CI. §4.4 keeps both measurements; the entry that held
them is closed at L5.

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
`BackendGuard`, for the boot's 185 records on one CPU (§2.3).

**L1's half of that is measured and the ring is free.** Interleaved A/B, one
session, one host (the dev laptop, arm64 cross-arch TCG), `xhci_slow_connect`'s
`Boot: complete` as the instrument, five reps per arm alternating so a drift in
host load falls on both:

| arm | reps | mean |
|---|---|---|
| the record ring, in *addition* to the byte ring | 495, 495, 494, 494, 494 | **494.4 ms** |
| `main` | 497, 497, 498, 498, 497 | **497.4 ms** |

**Not a cost — a 3.0 ms saving, and the two arms do not overlap** (A's slowest
is 495, B's fastest 497). The reading is stronger than a wash for a second
reason: arm A does strictly *more* work than arm B. It writes the shard **and**
does everything `main` does, the per-record `compare_exchange_weak` included.
The most likely source of the difference is that 658 macro expansions became
658 calls to one out-of-line `emit`, which is less code in the instruction
cache; it is small, it is not what this chunk was for, and it is not worth
chasing.

What it establishes is the thing L1 owed: **the shard's reserve-and-commit is
free at this instrument's resolution**, so what §1.4 prices at 350 ms is still
entirely in the byte ring L3 deletes, and the new ring will not eat that saving.

An earlier run of the same protocol, before `Tee` (below) put both sinks on one
format pass, gave 499.0 against 500.4 — the same verdict with the arms
overlapping. **Both arms moved by ~4 ms between the two sessions with no code
between them on the B side**, which is the whole reason this protocol is
same-session and interleaved.

### 1.4a L3's half, measured: the ring's deletion costs 4.4 ms of boot, and the 350 ms is not on this instrument

**MEASURED 2026-08-15**, and it does not go the way this section's arithmetic
implies, so the arithmetic is corrected here rather than left to be re-derived.

Same protocol: interleaved A/B in one session, five reps per arm, alternating,
the guest's own `Boot: complete (Nms)` as the instrument — read out of
`i8042_absent`, which boots `Profile::Metal` twice per run and prints both
stamps (`cargo test --test toyos-build -- --nightly i8042_absent`). The arms are
this branch's two commits either side of the deletion, `b8457df` (byte ring
alive) and `ee8369c` (byte ring gone), checked out whole so nothing but the
kernel differs.

| arm | with an i8042 | mean | without one | mean |
|---|---|---|---|---|
| byte ring alive (`b8457df`) | 257, 260, 259, 257, 260 | **258.6 ms** | 253, 256, 256, 257, 257 | **255.8 ms** |
| records only (`ee8369c`) | 261, 262, 263, 264, 265 | **263.0 ms** | 259, 260, 260, 261, 261 | **260.2 ms** |

**+4.4 ms, on both machine shapes, with the distributions not overlapping** (the
ring's slowest boot is faster than the records' fastest, on both). It is a cost
and it is named as one.

**Why it is a cost and why that is not a contradiction.** `Boot: complete` is a
measurement of the boot, and **the whole boot is `Drain::Inline`** — the phase
in which this design explicitly *keeps* one locked RMW, `BackendGuard`'s CAS
(§2.3). So the boot swaps `RingGuard`'s `compare_exchange_weak` for
`BackendGuard`'s one for one, and adds what the ring existed to defer: a
synchronous backend write per record. On metal-sim that is ~250 lines of
per-byte `outb` to a 16550 that QEMU answers instantly, and 4.4 ms is what it
costs. **The 350 ms this section is about is a producer-path RMW in steady
state, which is `Drain::Thread`, and no boot-time instrument can see its
removal.**

**What it buys is the thing §4.1's second constraint asks for**, and that is
measured too: a machine wedged in phase 3 puts **63** kernel lines on the
console where it previously put none (§9.5's `pre_idle_wedge_speaks`,
re-measured 2026-08-15 on this branch's tip, 63 on three consecutive runs; it
read 61 when the A/B above was taken and the boot has gained two lines since).
The trade was taken with the number in hand: 4.4 ms of boot against the tree's
worst diagnostic hole. `Drain::Inline` is gated on `has_console()`, so a machine that
cannot speak — the T14 as flashed — pays none of it.

---

## 2. The record and the ring

`kernel/src/log/` — `mod.rs` (the macros and `emit`), `record.rs` (the type),
`shard.rs` (the ring), `read.rs` (the two readers), `console.rs` (the serial
sink and `klogd`).

### 2.1 The record

```rust
// toyos-abi/src/log.rs — one layout, two types over it
pub const MAX_RECORD_MESSAGE: usize = 992;
pub const RECORD_BYTES: usize = 1024;

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

**The kernel's slot is the same layout as atomic machine words**, and nothing
else differs:

```rust
// kernel/src/log/shard.rs
#[repr(C, align(64))]
pub struct Slot {
    seq: AtomicU64,                // the same eight bytes as LogRecord::seq
    body: [AtomicU64; BODY_WORDS], // 3 identity words then the message, LE
}
const _: () = assert!(size_of::<Slot>() == RECORD_BYTES);
const _: () = assert!(offset_of!(LogRecord, at_ns) == size_of::<u64>());
```

**Every word of the body is an `AtomicU64`, and that is a soundness requirement
rather than a style.** It was an `UnsafeCell<Body>` until 2026-08-14 — the
writer stored the whole struct through it while a reader took a `read_volatile`
of the same bytes, and §2.4's sequence re-check discarded the torn *result*
without ever legalising the *access*. A non-atomic write racing a read is
undefined in Rust's model whatever x86 makes of it, and `volatile` is not a
synchronisation primitive: it constrains the compiler and says nothing about
the abstract machine. Per-word `Relaxed` stores and loads inside the unchanged
sequence protocol have no race to discard, and they cost nothing — each is the
same `mov` the struct copy was made of, and the fences that order them are the
ones that were already there. It also deletes the `unsafe impl Sync for Shard`
that stood in for the cell: every word being an atomic is what makes the claim
true, so the compiler makes it.

**The packing is written out by hand and a host test round-trips it.** Three
identity words — `at_ns`; `pid | tid << 32`; `cpu | len << 16 | elided << 32 |
level << 48 | flags << 56` — and then the message, little-endian, eight bytes to
the word. A `transmute` of a `#[repr(C)]` struct would make the two sides'
agreement a property of the compiler's layout choice rather than of the file,
and a field shifted into the wrong half of a word is invisible to every model
here: loom explores orderings, and a consistently mis-packed record is perfectly
ordered. `kernel-loom/tests/log_body_words.rs` is the round trip, under the
`--no-default-features` invocation because `MSG_WORDS` is 1 under loom.

**A producer stores the words its message occupies and no more**, which is
`3 + ceil(len/8)` of the 127 — **twelve** words at the corpus's 68-byte mean
message (three header and nine of the 124 message words) rather than a full
1,016-byte copy. This said "nine words" until 2026-08-15, which is the message
half of the sum compared against the whole body's 127. The reader loads the same count, from a
`len` it clamps first because that word may be mid-recycle; the worst a garbage
value can do is make it read the whole message area, which is in bounds and is
then thrown away by the re-check.

**Two types and not one, because `AtomicU64` cannot cross the syscall boundary
honestly.** `Record` in an earlier draft of this section was a single type with
an `AtomicU64` in it, shared with userland — which makes the copy-out a
transmute of an atomic into a value nobody synchronises on, and gives `logd` a
field named `commit` that commits nothing. The layout is one thing and the two
`const` assertions above are what keep it one; the *types* say which side is
which. §10's L4 delivers both.

**Why 992 bytes of message.** Measured over every committed T14 log
(`cat specs/metal-logs/*/*.log`, message length after the `[kernel … ] ` prefix):
12,497 lines, min 14, p50 **59**, p90 **111**, p99 **154**, p999 **857**,
max **863**. Everything above 200 characters is one call site — a `{:?}` of
`KernelArgs`, 18 lines of the 12,497 (0.14%), and L2 splits it into six records
because it is a producer the kernel can split.

**224 was this document's first answer and it was wrong for the corpus it was
derived from.** That corpus is boots, and **none of them contains a panic
backtrace** — a frame carries a demangled Rust symbol whose width nothing in the
kernel bounds. 992 is the next power-of-two record above the measured maximum
(863), so every ordinary line in the corpus now fits *whole*; `RECORD_BYTES`
stays a power of two, which is the shift-indexing invariant a reader depends on.
Ruled 2026-08-14 under the owner's delegation, recorded in
`specs/issues/diagnostics/a-record-cannot-hold-a-demangled-frame.md` and landed
as #57. **It does not bound a symbol** — nothing does — so a backtrace frame
elides its symbol head-and-tail at the producer (§2.1a).

A message past the bound is truncated **and says by how much**; `elided` is not
decoration, it is the difference between a bound and a lie.

**Why fixed-size.** A variable-length record needs a descriptor ring and a data
ring and an argument about the two staying consistent. A fixed-size POD record
makes "half a record" untypeable: see §2.4. The cost is space, and the two
figures it is priced against are different quantities that both land near 90 —
**CONFIRMED**: the mean *rendered line* over the corpus is **89.4 bytes**
(prefix included, which is what a byte ring stores and what §14's 2.8× compares
against), and the mean *record payload* is 32 bytes of header plus a 68.2-byte
mean message, **100.2 bytes** of the 1024 (which is what §3.2's copy-out waste
is measured against, and the waste grew with the record: the bound is sized by
the tail of the distribution, not by its mean).

**`Level` has three variants and each has callers today.**

| variant | producer | who reads it |
|---|---|---|
| `Info` | `log!`, 654 sites | everyone |
| `Phase` | `boot_phase!`, 6 sites | the panel repaints on one |
| `Alert` | `alert!`, **6 sites** — enumerated below | the panel paints the row red |

`Alert` **deletes a magic-value sentinel**: `panic_console::has_alert`
(`mod.rs:1035`) scans each row for three consecutive `!` bytes, and its own
comment enumerates the strings that happen to match. That is the comment the
root `CLAUDE.md` says is the type you should have written. It is not a severity
ordering and nothing orders it; every consumer matches exhaustively.

**The conversion is exactly six sites and a conversion that misses one loses a
red row on the panel silently, so they are named** (`grep -rn 'alert!' --include='*.rs'
kernel/src/`, re-measured 2026-08-16; the conversion itself was **CONFIRMED**
2026-08-09 by the `!!!` grep this replaces):

| site | line |
|---|---|
| `main.rs:174` | `!!! EARLY PANIC !!!` |
| `main.rs:208` | `!!! DOUBLE PANIC !!!` |
| `main.rs:335` | `report_log_destination`, no `/log` |
| `main.rs:338` | `report_log_destination`, no console and no `/log` |
| `arch/idt/exceptions.rs:272` | `!!! PANIC !!!` |
| `arch/idt/exceptions.rs:568`, `:571` | `!!! FAULT …` (one refusal, two arms) |

**Seven until 2026-08-16, and the row that went names why.**
`arch/debug.rs:87`'s `!!! PTE CORRUPTION DETECTED !!!` belonged to a timer-tick
PTE poller with no caller, deleted with the rest of that module's arming tools
(`specs/issues/design-debt/four-deletions-still-owed.md`). The conversion it
records still happened; the row goes with the code rather than staying as a
citation of a line that is not there.

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

### 2.1a A symbol nothing bounds keeps both its ends

`kernel/src/log/elide.rs`. A backtrace frame renders
`    {addr:#x}  {symbol}+{offset:#x}`, and the symbol is the only part of it
whose width the kernel does not choose: `late_panic::Nest` is a generic nested
in itself and nothing stops it being nested again. So the *record* bound cannot
be the answer — raising it moves the cliff and never removes it — and the
question is instead which bytes of an over-wide name survive.

**Both ends.** The head is the crate and the module path; the tail is the
function, which is the half a backtrace is read for and the half plain
truncation drops. `screen_late_panic`'s `check_wrap` asserts on the tail for
that reason.

**At the producer, and that is what keeps everything else honest.** The record
then holds a whole message, `elided` still means "bytes past the bound" rather
than "bytes out of the middle", and the ABI's one formatter needs no second
convention. `...[N bytes elided]...` is ASCII because the panel's font is
codepoints 0x20..=0x7E.

The budget is `MAX_RECORD_MESSAGE` minus the frame's own text, and **the marker
comes out of it first** — a split that spent the whole budget on head and tail
and then wrote the marker between them put the line back over the bound and cost
it the tail. `const` assertions in `kernel/src/symbols.rs` hold the arithmetic.

**No guest test reaches it at the shipped bound, and that is stated rather than
implied.** The tree's own widest symbol is `late_panic::Nest` at 288 bytes
against a budget of 944, so `screen_late_panic` proves the panel keeps a
symbol's tail and proves nothing about the elision. The seams — a character
straddling the head cut, a character straddling the start of the tail, a value
arriving one character at a time — are checked on the host by `kernel-elide`,
which compiles the kernel file itself rather than a transliteration of it.

**`kernel-elide/` is a harness and not a crate of its own.** It is
`kernel-span/`'s arrangement — `#[path = "../../kernel/src/log/elide.rs"]` and
nothing else — and it sits beside `kernel/` rather than inside it because
`kernel/.cargo/config.toml` cross-compiles everything below it to
`x86_64-unknown-none` and cargo refuses to merge an inherited `build.target`
away. It runs as `cargo test --manifest-path kernel-elide/Cargo.toml`, from
`.github/workflows/host-tests.yml`'s host-crates list. **That is the property
`elide.rs` has to keep to stay testable**: it names nothing outside itself, so
the harness supplies nothing. A dependency added there is the file leaving the
host, and the nine tests go with it.

### 2.2 The shard

```rust
#[repr(C, align(64))]
pub struct Shard {
    /// Reservation counter. **Only the owning CPU writes it**; every other CPU
    /// reads. Starts at `FIRST_SEQ` (1); `seq < head` is half of §2.4's
    /// validity test.
    head: AtomicU64,
    slots: [Slot; SHARD_RECORDS],
}

pub const SHARD_RECORDS: usize = 512;      // 512 KiB per CPU
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

**`alloc_zeroed` is not the AP shard's constructor.** L1 originally cast the
zeroed allocation straight to `Shard`, leaving `head == 0` even though
`Shard::new()` gives cpu0 `head == FIRST_SEQ`. The AP's first reservation then
issued sequence 0, collided with every slot's empty-state word, and was
deterministically unreadable. `Shard::initialize_zeroed` now writes
`AtomicU64::new(FIRST_SEQ)` into the unpublished allocation in place; the slots
stay zero. Constructing a full `Shard::new()` value and moving it is rejected
because it can materialise 512 KiB on the BSP's 16 KiB stack. The host-fast
`log_zeroed_init` test allocates the real layout with `alloc_zeroed`, runs this
constructor, and proves the first reservation is `FIRST_SEQ`.

**It runs under `cargo test --manifest-path kernel-loom/Cargo.toml
--no-default-features --test log_zeroed_init` and under nothing else.** Loom's
atomics are not byte-zeroable, so the file is gated `cfg(not(feature =
"loom"))` and the crate's default invocation compiles it to nothing — which
CI read as a pass from L1 until 2026-08-14, because `running 0 tests` and a
green run are the same line. Both commands are in
`.github/workflows/host-tests.yml` now; the second is scoped to this target
because `specs/issues/build/kernel-loom-ungated-models-red-without-loom.md`
records that the crate as a whole reds without loom.

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

**Memory.** 512 KiB per CPU; at the shipped `sched::MAX_CPUS = 8`, **4 MiB**,
against today's single 64 KiB ring. At the 128-core target root `CLAUDE.md` sets
it is **64 MiB**, which is 0.8% of an 8 GiB machine; the escape, if that is ever
judged too much, is to scale `SHARD_RECORDS` down above 16 CPUs, and it is one
line. Named so the number is a decision and not a discovery.

**These are the ruled figures and not the drafted ones.** This section was
written against a 256-byte record, and the ruling recorded in
`specs/issues/diagnostics/a-record-cannot-hold-a-demangled-frame.md` raised
`MAX_RECORD_MESSAGE` to 992 and `RECORD_BYTES` to 1024 — landed as #57, closed
by this branch's `b9b0857`. The +3 MiB at eight CPUs was bought deliberately
under that delegation; what was not done at the time was to carry the number
into every place this document states it, which is what the figures above and in
§13.3 now do.

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

1. **Format into a stack buffer.** `MessageBuf` is 992 bytes plus a counter, on
   the caller's stack, implementing `core::fmt::Write`; overflow increments
   `elided` and writes nothing. Formatting is *outside* every critical section —
   today it happens inside `SerialWriter`, which is at least a lock the code
   claims to hold.
   Stack budget: the smallest kernel stack in the machine is IST1 and the idle
   stack, both **16384** bytes (`percpu.rs:246`, `:204`), and 992 bytes is 6.1%
   of one. §2.3b is the measured budget for the whole fatal path. The 512-byte drain buffers that used to live on those stacks go away
   entirely (§8).
2. **Close one publication guard.** `LogCommitGuard::close()` saves RFLAGS and
   clears both `IF` and `TF`. Inside it, read this CPU's shard pointer and
   identity from `gs:`, then reserve with one **non-`lock`-prefixed `xadd`** on
   `shard.head`, behind `arch::percpu_fetch_add`, documented as
   *interrupt-atomic, not SMP-atomic; sound only for a counter one CPU writes*.
3. **Write and commit before reopening the guard.** Fill the record's five
   identity fields, store `WRITING`, perform the release fence, store the body
   words into `shard.slots[seq % SHARD_RECORDS]` — three identity words and the
   message's own `ceil(len/8)`, no more (§2.1) — and finally
   `slot.seq.store(seq, Release)`. Only then restore the caller's complete
   RFLAGS. Formatting remains outside; §2.3a says why reservation through final
   publication cannot. **The timestamp does not** — it is read inside the guard,
   one instruction from the `xadd`, which is what makes a shard's sequence order
   its timestamp order and `read.rs`'s descent sound.
4. **Read the waiter flag after the guarded commit**: `signal_after_commit()` —
   `fence(SeqCst)`, `LOG_WAITER.load(Relaxed)`, and on `true` the swap that
   admits one poster per park (§2.5, §2.6a).
5. **Post the wake, only in `Drain::Thread` mode and only when step 4 returned
   `true`**: `wake_direct` to `klogd`, inside a flags bracket of its own. Not
   once per record — once per park.

**No lock and no locked RMW in `Drain::Thread`**, which is every mode after
`scheduler::init`. The path loses today's `compare_exchange_weak` and keeps a
flags bracket across reservation and one bounded publication of at most 1,016
bytes and in practice the message's own length; it gains
one `SeqCst` fence and one relaxed load. The guard never polls, spins, schedules,
or waits for a prior writer. L1 measures the widened bracket and the net for the
ring; L3 measures the fence, which arrives with the wake (§9.6).

**`Drain::Inline` is not that path and must not be described as if it were.**
Inline `emit` calls `BackendGuard::lock()` (`serial.rs:97`), which is
`save_and_cli()` **plus a `compare_exchange_weak` spin** — so during boot every
record pays one locked RMW and a synchronous backend write. That is one CPU and
185 records (§2.2), so the cost is nil; the claim is what has to be accurate.
§9.6's RMW budget counts both modes separately.

**Inline and Thread have different nesting rules.** `BackendGuard` makes Inline
non-reentrant: it clears `IF` before it spins, but a Ring 0 exception can enter
while the same CPU holds it. That is today's shape exactly (`RingGuard::lock`
has the same `cli` and CAS), and the fatal path is covered by `panic_flush`,
which waits out a live holder and then bypasses a wedged one. Thread takes no
lock and never waits for an interrupted producer; instead its publication is
non-preemptible for every returning IRQ and single-step path. A fatal #DF or #MC
may nest, append its own records, and halt without returning; §3.1's snapshot
reader skips the interrupted `WRITING` slot. Stated because the shard's rule and
the inline backend's rule are deliberately different.

### 2.3a Why reservation and publication are one bracket

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
C6 adding `usbd` and `iod` on the machinery L3 builds (§4.3, §12.6). Without one
bracket across the operation there are two independent failures:

- a preemption **between the `gs:` read and the `xadd`** puts the `xadd` on a CPU
  that does not own the shard, and two CPUs performing a non-atomic
  read-modify-write on one word lose an update. Two records then hold the same
  `seq`, share a slot, and one overwrites the other. §9.1's conservation law is
  what would eventually catch it; a race that needs a preemption inside a
  two-instruction window is not something a suite catches on purpose.
- a preemption **during the body copy is not harmless.** Suppose the outer
  writer owns `s`, marks its slot `WRITING`, copies part of the body, and is
  scheduled away. The original CPU can publish a whole newer generation,
  including `s + SHARD_RECORDS`, before the outer task resumes. The outer then
  overwrites the tail of that committed record and may finally store the stale
  `s`. A head check before the copy only covers a lap that already happened;
  one immediately before the final sequence store notices too late, after the
  newer record's body is corrupt. There is no re-check position outside the
  copy that repairs bytes already written.

The guard therefore spans the `gs:` reads, the unlocked `xadd`, the identity
stores, the `WRITING` mark, the body copy and the final release store. It restores
RFLAGS only after the slot is committed. No newer committed record can move
back to `WRITING` or acquire another body's bytes **because an older writer
resumed**: a live writer cannot be scheduled away inside that interval. Normal
drop-oldest recycling still moves the oldest generation through `WRITING`, by
design. The guard is crate-private and carries `PhantomData<*mut ()>`, making it
neither `Send` nor `Sync`; safe code cannot restore one CPU's saved RFLAGS after
moving the guard to another CPU.

**Why the flags bracket and not `preempt::disable()`.** `preempt::disable`
(`preempt.rs:93`) is `lock add dword ptr gs:[240], 1` and `enable` is a `lock
sub` plus a `need_resched` poll — **two locked RMWs per record**, which is the
cost §1.4 exists to avoid. It buys scheduler migration exclusion and still does
not clear `TF`, so it does not buy the complete property. Single-step #DB is an
exception, not a maskable interrupt, and Ring 3 can set TF; restoring it before
publication would reintroduce same-CPU nesting one instruction later. The flags
guard clears `TF` as well as `IF`. Hardware-watchpoint #DB is
the one returning path flags cannot mask. Its handler now clears DR7 and DR6
*before its first `log!`*, preventing recursion when the watched address is the
shard head or body, and the handler emits at most 32 records including its
20-frame backtrace — less than one 512-record lap. NMI does not log; the shard,
record and kernel stack are resident and cannot fault; #DF and #MC do not
return. Those are the complete exceptional entries into this bounded region.

The flags bracket is also what every `log!` caller inside a syscall already has
for `IF` (it is clear for the whole syscall), while `TF` is normally clear
machine-wide. **`emit` has a second bracket of the same kind and it is not this
one**: the wake post sits after the commit store and after RFLAGS restoration,
and §2.6a is where that bracket is argued.

**W4's shim is sound for this shape and only this shape** (§2.5): loom models the
`xadd` as a real `fetch_add`, which is strictly stronger, and the bracket is what
makes "no other CPU writes `head`" true rather than hopeful. Loom has no CPU
flags or strict same-CPU preemption, so it carries a `LogCommitGuard` witness and
the negative control belongs to §9.2's guest actuator.

### 2.3b The fatal path's stack, measured

**6,688 bytes of IST1's 16,384**, `ist1_report` off a real #DF on a
`double_fault_stack` run, guard intact. It is taken after `render` and after
`panic_flush` (`apic.rs`'s `halt_all_cpus` fixes that order and says why), so it
covers the deepest the report goes — the record merge and the paint included.
`double_fault_stack`'s own margin is that the report must fit in half the stack,
and it does: 13,376 of 16,384.

It was **4,512** before the record ring, so the ring's net cost is 2,176 bytes
and 9,696 are free. What is large on that path is type sizes rather than a
decomposition of the measurement — `log::console`'s rendered line at 1,152,
`emit`'s `LogRecord` at 1,024, `snapshot_committed`'s one materialised record at
1,024 and its eight `Descent`s at 384, `paint`'s row table at 768 — and those
come to 4,352 of the 6,688. `kernel/src/arch/percpu.rs`'s `IST1_STACK_SIZE` is
where the number lives, beside the constant it is a budget against.

**It was 7,488 with the byte ring, and both halves of that difference are
deletions.** Making the body atomic words (§2.1) took `commit`'s 1,016-byte
`Body` staging off this stack and the measurement did not move — re-run
2026-08-14, still 7,488 — which says that frame was never the deepest one;
deleting `SerialWriter`'s 1,024-byte line buffer and `drain_to_serial`'s
512-byte chunk with the ring took it to 6,688, which says those were.

**This is the number a chunk that widens the fatal path re-measures**, and it is
cheap: one `cargo test -- double_fault_stack`.

### 2.4 What makes atomicity unrepresentable to violate

Four properties, each structural rather than checked:

- **There is no way to append bytes.** `emit` is the module's only public
  producer and it takes `fmt::Arguments`. `Shard` has no method that takes a
  `&[u8]`. A caller cannot write half a record because the smallest thing the
  module accepts is a whole one. This is what deletes `write_chunk`,
  `write_chunk_blocking` and `SerialWriter`'s spill-on-overflow, which is the
  mechanism every recorded splice went through.
- **The validity word and the identity word are the same word.** A slot is
  readable as record `s` exactly when **`oldest_readable <= s < head` and
  `slot.seq == s`.** There is no separate "valid" flag that could disagree with
  the sequence number.

  **`slot.seq == s` alone is not a total test and an earlier draft of this
  section said it was.** Slots are zero at boot — cpu0's shard is `.bss` and an
  AP's is `alloc_zeroed` — so slot 0 of a shard that has never been written holds
  `seq == 0`, which *equals* sequence number 0. The AP constructor changes only
  `head` to `FIRST_SEQ`; it deliberately leaves this slot state zero. A reader
  checking only equality would read an all-zero record as record 0 of every
  shard on every boot.

  **And `s < head` does not repair it, which is L1's first correction.** `head`
  counts *reservations*: it is bumped before the body exists, so on the very
  first record of a boot `0 < head` and `slot.seq == 0` are both true while the
  writer is still filling the slot — a reader accepts an uncommitted, half-built
  record 0, on every boot, on every shard. **The fix is that sequence numbers
  start at 1** (`FIRST_SEQ`), so no issued number can collide with the zeroed
  state, and the reader's lower bound is `oldest_readable` rather than nothing.
  Both ends are compared against words the reader loads anyway (§2.6).
  `kernel-loom`'s `a_shard_nothing_has_written_answers_for_nothing` fails if
  either goes back, and host-fast `log_zeroed_init` binds the AP allocation path
  to the same first sequence as cpu0.

  **A stale value can never be mistaken for a live one, and that is what kills
  ABA.** Slot `j` only ever holds sequence numbers `≡ j (mod SHARD_RECORDS)`, the
  sequence is a `u64` that never wraps in any reachable lifetime, and the test is
  *exact equality* rather than a range. So a slot carrying an older generation's
  number fails against every `s` the reader can ask for, and there is no value the
  reader accepts that was not written for that `s`.
- **Two stores publish, and this section said one until L1 built it.** The claim
  was: *"Exactly one store publishes, and it is the last one … the reader's
  re-check of the same word after copying the body is total: the only thing that
  can change a slot's body is a writer reserving `s + SHARD_RECORDS`, and that
  writer's own commit store changes `slot.seq` away from `s`."*

  **The re-check is not total, because that store comes after the body write.**
  Throughout the recycling writer's body write the word still reads the
  *previous* generation's number, so a reader that loads `s`, copies a
  half-overwritten body and re-checks reads `s` **both times** and accepts the
  tear. The window is the whole body write, which is the longest part of a
  commit.

  So the writer stores `WRITING` **before** it touches the body and the sequence
  number after it, with a release fence between the mark and the body so the
  mark is ahead of it for the reader too. The reader's re-check is then total for
  the reason the first draft gave: any writer that started during the copy moved
  the word first.

  `WRITING` is `u64::MAX` and it is the second state one atomic word has to be
  able to hold. A second word would be a second thing that can disagree with the
  first, which is what this whole scheme exists to avoid; the value is
  unreachable as a sequence number by 2^64 records, it is decoded at the
  boundary, and no `LogRecord` ever carries it. **This is the even/odd seqlock
  convention the first draft said it did not need**, with an explicit marker in
  place of parity because slot `j` already constrains the word modulo
  `SHARD_RECORDS` and parity would collide with that.

  **`kernel-loom` found both of these on its first run**, before any guest test
  existed to disagree, and neither is reachable on x86 in a way a test could
  catch. **Which model holds which half was written down wrongly here until
  2026-08-15, and the correction is the whole of L3's review finding F2.**
  `a_committed_record_is_whole_or_absent` holds the publication edge.
  `a_recycled_slot_does_not_answer_for_the_record_it_replaced` does **not** hold
  the mark: it is single-threaded, so its reader always sees the fresh `head`,
  and the lower bound `seq < oldest_readable` alone answers every one of its
  assertions — **measured**, it stays green with the `WRITING` store and its
  release fence deleted outright. What it holds is that the window moves before
  the body does. The mark's own model is
  `a_reader_racing_a_recycle_gets_nothing_rather_than_a_mixture`, which is
  `a_key_and_the_record_it_names_come_from_one_generation` with one word changed
  — it asks the racing reader for the number the slot *holds* rather than the
  one it is *about to* hold, so the reader's stale `head` admits the old
  generation and nothing but the mark and the two acquire fences stands between
  it and a mixture. `shard.rs`'s comment has cited that name since the two-store
  publish landed; until F2 it named no test.

  **The body is read word by word as atomics, and that is a requirement rather
  than a style.** Reading bytes that a writer may concurrently be storing into
  is a data race in Rust's model whatever x86 does about it; the re-check makes
  the *result* sound and does not make the *access* defined. A `read_volatile`
  through an `UnsafeCell` was the form until 2026-08-14 and it is not a fix —
  volatile constrains the compiler and says nothing about the abstract machine.
  `Relaxed` per-word loads against `Relaxed` per-word stores are what make the
  access defined; §2.1 carries the argument and the packing.
- **A live reservation cannot be abandoned or lapped.** The same IF/TF-off
  guard covers reservation through the final sequence store, and that region
  contains no lock, wait, spin, scheduler call, allocation, formatting, or
  fallible memory access. Ordinary IRQ preemption and single-step #DB are
  excluded. A hardware-watchpoint #DB clears its trigger before logging and is
  statically bounded to 32 nested records, below `SHARD_RECORDS`; NMI does not
  log. A #DF or #MC may nest and then halt, so the interrupted reservation can
  remain `WRITING`, but no outer writer resumes and a dying machine uses
  `snapshot_committed` (§3.1).

  Therefore every reservation on a live machine reaches its release store and
  a quiet shard cannot retain a permanent hole in front of `klogd`. `commit`
  has no lapping check: the former pre-body check missed preemption during the
  body stores, while a final check would merely diagnose corruption already
  performed. The non-preemptible publication is what prevents the transition;
  no producer ever waits for the interrupted one.

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
impl Shard {
    unsafe fn reserve(&self, guard: &LogCommitGuard) -> u64;
    unsafe fn commit(&self, seq: u64, record: &LogRecord,
                     guard: &LogCommitGuard);
}
/// Fence, read the flag after commit. True means "a reader is parked; post".
pub fn signal_after_commit(waiter: &AtomicBool) -> bool;
/// set the flag, fence, re-scan for a committed record. True means "do not park".
pub fn arm_waiter(waiter: &AtomicBool, committed_record_waiting: impl Fn() -> bool) -> bool;
```

**Two arguments rather than none, and each is what keeps the file shimmable.**
The flag comes in because loom's atomics have no `const` constructor, so a
`static` is unrepresentable in the modelled build — `registry.rs` already solves
that the same way, with `#[cfg]`ed `log_waiter()`/`waiter()` beside the
functions. The rescan comes in as a closure because the predicate lives in
`read.rs` and names `log/mod.rs`'s shard registry, which is exactly the
dependency this file may not grow: passing it means `arm_waiter` names no
subject at all, and the model supplies its own.

The caller does the post on a `true`, and never the ordering. The shimmed set for
`shard.rs` is then `LogCommitGuard` and `percpu_fetch_add` — the cell left it
with the `UnsafeCell` (§2.1) — which is smaller than `sync.rs`'s. **The layout requirement is stated here and honoured at L1 even
though the two functions arrive at L3** (§2.6a): if `shard.rs` were allowed to
grow a dependency on a subject, W3 could not be modelled at all, and that is
exactly the mistake the paragraph above records.

| obligation | shape | model | when |
|---|---|---|---|
| **W1** publication | body stores → `seq.store(Release)`; reader's `seq.load(Acquire) == s` implies the body is visible | `kernel-loom/tests/log_record.rs` | L1 |
| **W2** recycle detection | the readable window is exactly the last `SHARD_RECORDS`, and a slot a writer has entered answers for neither generation. **Deterministic, not raced** — a two-thread model over a whole ring is hundreds of thousands of interleavings, which did not finish in ten minutes here. (Until 2026-08-14 there was a second reason: loom's `UnsafeCell` recorded every access as a write and reported the unsynchronised *pair*, so a seqlock read looked to it like a second writer. The body is atomic words now, so loom models each access as the atomic access it is — which is what makes a *raced* recycle expressible at all, and is a statement about the instrument and not about coverage: no shipped model exercised the mark until `a_reader_racing_a_recycle_gets_nothing_rather_than_a_mixture` was added on 2026-08-15.) What loom checks concurrently is W1 and, since that model, the mark and both readers' acquire fences; what the deterministic model holds is that the window moves before the body does | same | L1; the raced half 2026-08-15 |
| **W2b** non-preemptible publication | one `LogCommitGuard` witnesses shard selection, reservation, `WRITING`, body copy and the final sequence store. A stale writer can neither migrate nor resume after a newer generation. CPU flags and strict same-CPU preemption are outside Loom; §9.2's guest actuator is the negative proof | `log_nested_emit` plus `log-unbracketed-reserve` | L4 |
| **W3** the wake edge | `signal_after_commit`: `fence(SeqCst); LOG_WAITER.load(Relaxed)` after `seq.store(Release)`, against `arm_waiter`: `LOG_WAITER.store(true, Relaxed); fence(SeqCst); rescan for a committed record; park`. **Invariant: no committed record is left with a parked reader.** Store-buffer shaped, so TSO hides the missing fence and only loom sees it. Carries its own negative case — either fence removed behind a `cfg`, and the model must red — because a model that has never failed proves nothing | `kernel-loom/tests/log_wake.rs` | L3 |
| **W4** the reservation | `head` is written by one CPU through inline asm and read by others as an `AtomicU64` | shimmed | L1 |

**These models are green and they have teeth, which is checked rather than
claimed — and the count this paragraph gave was wrong for a year of nothing:**
it said one model reds under the commit store's `Release` weakened to `Relaxed`
and "the other four stay green", against six models. **Re-run 2026-08-15 on the
dev host, seven models: two red.** `a_committed_record_is_whole_or_absent`
(*"torn: at_ns is another record's, left 0 right 1007"*) and
`a_key_and_the_record_it_names_come_from_one_generation` (*"a key from the
generation the slot used to hold, left 1007 right 5007"*); the other five stay
green, `a_reader_racing_a_recycle_gets_nothing_rather_than_a_mixture` among
them, because the edges that model is about are the mark's release fence and
the readers' acquire fences rather than the publishing store's.

**Each of the three edges has a weakening that reds exactly one model**, all
four runs measured 2026-08-15 and each restored before commit:

| weakening | red | on |
|---|---|---|
| `slot.seq.store(seq, Release)` → `Relaxed` | `a_committed_record_is_whole_or_absent`, `a_key_and_the_record_it_names_come_from_one_generation` | the publication edge |
| `slot.seq.store(WRITING)` + its `fence(Release)` deleted | `a_reader_racing_a_recycle_gets_nothing_rather_than_a_mixture` (*"a key from the generation the slot is being given, left 5007 right 1007"*) | the mark |
| `Shard::read`'s `fence(Acquire)` deleted | the same model (*"torn: the message body is another record's, left [5,0,…] right [1,0,…]"*) | the copy's re-check |
| `Shard::at_ns`'s `fence(Acquire)` deleted | the same model (*"a key from the generation the slot is being given"*) | the key's re-check |

The last three are F2's, and the two readers being separable is the point: they
are two chances to get the seqlock wrong and each is now checked on its own.

`SHARD_RECORDS` is 4 under loom, which is what makes the recycle cases
reachable at all — at 512 a model that has to lap a slot explores a branch it
never finishes. The layout `const` assertions are skipped in that build,
because loom's atomics and cells are wider than the real ones; they bind the
build whose layout matters and the model is about the ordering.

**W3 is L3's, and it is the one obligation the wake's own correctness rests on.**
The producer-side edge is real in this branch — `emit` reads `LOG_WAITER` after
its commit and posts (§2.6a) — so the store-buffer pair exists in code L3 writes
and x86 cannot fail it on purpose. A tree that drops either fence loses a wake
against a parked `klogd` and goes quiet with a committed record behind it; loom
is the only instrument that sees it, which is why the model ships in the same
chunk as the code.

**As built it is `a_commit_and_an_arm_cannot_both_miss`, and its invariant is
narrower than this section first wrote it.** The model asserts that after a
commit and an arm, *at least one side has seen the other* — either the producer
owns a post or the reader's rescan finds the record — and it asserts nothing
about the park. **The first draft of the model did model the park, with a plain
`parked` word, and it reported a lost wake that does not exist**: a producer
clearing a word the reader has not set yet is a race the *rendezvous CAS*
removes, because `klogd` registers before it arms and a producer that wins the
flag from there on claims a `Committing` task whose own commit then refuses to
park. That handshake is `toyos-sched`'s and has its own models; re-deriving it
here only re-derived it badly. What is left is exactly the pair of fences, which
nothing else in the tree models.

**The negative control runs rather than being recorded.** `wake-fence-off` is a
cargo feature that removes both fences, `kernel/Cargo.toml` declares it for
`cfg`-checking exactly as it declares `loom`, and
`.github/workflows/host-tests.yml` runs `log_wake` under it and fails the job if
it *passes*. Measured 2026-08-14 on the dev host: green both tests with the
fences, `a_commit_and_an_arm_cannot_both_miss` red without them.

**W3's rescan is over committed records and never over `head`, and that is a
liveness property rather than a taste.** `head[i] > next[i]` can mean one writer
is inside the bounded publication window; it does not mean the next slot is
committed. Busy-waiting on that inequality spends a CPU until the copy finishes.
The waiter instead asks the predicate `drain_ordered` uses — *is there a
committed record at `next[i]`* — and may park. If the writer is still active, its
post-commit signal wakes the waiter; if it already committed, W3's two fences
prevent the lost wake. Since §2.4 permits no abandoned reservation on a live
machine, a quiet shard cannot strand later records behind a permanent gap.
Eight loads either way.

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

**`next` starts at `FIRST_SEQ` and not at zero, and the clamp is applied on
every call rather than at construction.** Sequence numbers start at 1 (§2.4), so
a cursor sitting at 0 would compute one phantom loss per shard on its first
call — and a cursor that crossed the syscall boundary arrives *zeroed*, which is
the caller doing exactly that. `Cursor::new` gives the kernel's readers the
right value and `Cursor::open` takes `max(FIRST_SEQ)` anyway, because the second
of those two callers is untrusted input.

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
bracket ends after the commit store and restores the caller's RFLAGS before the
post, so the wake is *not* inside a bracket that already exists — it gets a
second one of the same kind and by the same argument.
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
paragraph names one. As built it is `driver::IrqOff` and `driver::irq_off`,
beside `PreemptOff` and `preempt_off`, and the pair reads as the two answers to
one question.

**`LOG_WAITER` is the gate, and it is what keeps the record path free of locked
RMWs.** Without it every record would pay `claim_wake`'s CAS. §2.5's two
functions, in `shard.rs` with the ring:

- `signal_after_commit`: after the guarded `slot.seq.store(seq, Release)`,
  `fence(SeqCst)`, `LOG_WAITER.load(Relaxed)`, and **only** on `true` the
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

**The leak is not tidiness either.** A producer reading this pointer is inside
whatever lock it was already holding, and an `Arc` clone there is a refcount it
could be the last owner of — so the `Arc<KShared>` is `Box::leak`ed once at
spawn and the static holds a `&'static` to it. `klogd` never exits, so there is
nothing for the leak to lose.

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

/// Every committed record in the window, **newest first**, merged by `at_ns`,
/// skipping uncommitted slots. Takes no lock and never blocks. For the panic
/// console and for Ctrl+Alt+D.
pub fn snapshot_committed(from: u64, to: u64, out: &mut impl RecordSink) -> usize;
```

**Newest first, and it is not a preference.** Both callers have a fixed buffer
and both show the *end* of what they hold, so a reader that filled from the
oldest end would spend a panel on the boot and drop the panic. It is also what
bounds the work by the buffer rather than by the ring: the sink answers "full"
and the walk stops, where an oldest-first reader has to copy every live record
out of every shard before it knows which ones it wanted. The bracket is two
`nanos_since_boot` readings rather than an `Instant` type, because the kernel
has no such type and inventing one for two call sites is a type built for a
plan.

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
give. On a live machine it is bounded by one non-preemptible body
publication; on a fatal machine the interrupted `WRITING` slot can remain
forever, but that machine uses `snapshot_committed` instead. The alternative —
stalling every shard on the slowest — is what turns one wedged CPU into a silent
machine.
`logd` renders in arrival order and the timestamp is in every line, so a reader
sorts if it cares.

**Both readers are allocation-free `SHARD_RECORDS`-bounded merges**, because the
panic path calls one of them and can allocate nothing: `MAX_LOG_SHARDS` cursors
on the stack (8 × 16 bytes at the shipped count), pick the smallest `at_ns`,
advance. `snapshot_committed` walks each shard from `head - SHARD_RECORDS` to
`head`, which is at most 4,096 slot reads machine-wide.

There is no abandoned reservation on a live machine (§2.4). If a reader reaches
an in-flight slot, it may park rather than burn a CPU; the bounded writer commits
and the W3 signal wakes it. A fatal handler may leave the interrupted slot
permanently `WRITING`, but the machine does not return to `klogd` and the panic
surface uses the skipping snapshot reader. That split is what makes both the
live liveness claim and the dying-machine diagnostic claim true.

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
    /// In: the timestamp of the newest record the caller has made durable, or
    /// zero. The kernel takes the maximum, **after clamping it to the newest
    /// record it actually holds**: this is a number that crossed the trust
    /// boundary and decides how long a dying kernel waits for its own report,
    /// and an unclamped `u64::MAX` from a buggy `logd` would lose it silently.
    /// Clamping cannot lengthen the wait, so the worst a hostile writer does is
    /// shorten one for its own output.
    pub durable: u64,
    /// In/out: the next sequence wanted from each shard.
    pub next: [u64; MAX_LOG_SHARDS],
}

/// Returns records written into `out`, merged by `at_ns`. `out` is whole
/// `LogRecord`s at a fixed stride — `n * RECORD_BYTES` bytes — so the caller
/// indexes by shift and the kernel does no length arithmetic.
///
/// The `SysCap` is the authority this call rides, below; a wrapper that took
/// only a cursor and a buffer would be an ambient read of the whole machine's
/// log.
pub fn log_read(
    syscap: RawHandle,
    cursor: &mut LogCursor,
    out: &mut [LogRecord],
) -> Result<usize, SyscallError>;
```

- **The kernel keeps no per-reader state at all.** No object, no handle
  lifecycle, no cursor to leak or go stale, and a second reader costs nothing.
  That is `specs/introspection-plan.md` §3.3's argument and it is adopted whole.
- **Fixed stride, not packed.** The kernel copies whole 1024-byte records and
  userland indexes by shift. At the measured 100.2-byte mean payload (§2.1) the
  waste is nine tenths of what moves, and it is still the right trade: a few
  hundred records per second is around 300 KB/s over a syscall that already
  copies, against putting length arithmetic in the kernel and "is this record
  whole?" back into every reader. **The figure moved with the record** — it was
  under 100 KB/s at the drafted 256 bytes, and the ruling in §13.3 is what
  changed it.
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
  batch rather than one per record. **Built at L4** (`log::user::post_readiness`,
  `kernel/src/log/console.rs`); compl C3 folds all six statics into its one watch
  list and its §19 deletes them together. **Adding a sixth instance of a
  mechanism that is about to be unified is the honest cost of landing first**,
  and it is one static and one match arm.
  - **The readiness is an edge and not a level, as built, and it had to be.**
    `Source::is_ready` answers `false` for the log and every completion comes
    from that post. A level is a question the kernel cannot answer — "is there
    anything for *you*" is a property of a cursor the kernel does not hold — and
    answering `true` would complete every poll immediately and turn a parked
    reader into a spinning one. The caller closes the window itself, in the shape
    `shard::arm_waiter` already uses on the kernel's side: submit the poll, read
    once more, and park only if that read was empty. `Rights::WAIT` on the
    `SysCap` is what makes the poll expressible and `Rights::LOG` is what makes
    the read answerable, which is why `logread` grants both.
  - **A machine with no console posted nothing at all until L6**, because
    `klogd` did not drain there and parked unarmed — so it never woke, so it
    never reached the post, so the one machine shape this whole design exists
    for told a reader nothing. Nothing reached it at L4, whose gate's profile
    has a console; it became live the moment `/bin/logd` was the program parking
    on it. **Closed by removing the premise rather than by a second post**:
    `klogd` advances the drain position over records nothing can carry
    (`log::console::discard_pending`), so `DRAINED.any_pending()` goes false as
    it does with a console, `arm_waiter` is called unconditionally, and a commit
    wakes the thread on every shape.
    `specs/issues/diagnostics/a-console-less-machine-posts-no-log-readiness.md`
    carries the argument and what it does not close.
  - **A handle closing is not the log ending, and `ops::close` said it was.**
    `read_source` maps every `SysCap` to `Source::Log`, and `close` handed that
    to `io_uring::remove_fd`, which cancels across every ring in the machine —
    right for a pipe whose other end has really gone, wrong for a stream no
    handle owns. So any process closing any capability posted `-NotFound` into
    every pending log poll there was, and `/bin/logd`'s whole loop is
    read-then-park. `ops::ends_its_sources` is the fix, and the question is
    asked of the *object*: `SysCap` and `Console` answer `false` because each is
    one of many handles on a machine-wide facility, while a pipe end, a
    connection, a port and a device claim each really do go away with their last
    handle. `log_poll_outlives_a_close` (§9.5) is the gate and
    `log-close-cancels-any-syscap` the control. **`Console` is in it for a
    defect L5 would otherwise have introduced**: `read_source` answers
    `Source::Keyboard` for a console, and after §4.4 every process has its own
    console object, so one of them closing stdin would have cancelled the
    compositor's key poll.

**`MAX_LOG_SHARDS` is `MAX_CPUS`** and the cursor is **88 bytes** at the
shipped 8 — `24 + 8 * MAX_LOG_SHARDS`, which `toyos-abi/src/log.rs` asserts. It
does not scale with `RECORD_BYTES`: a cursor carries one sequence number per
shard and no records at all. A
caller passing a smaller buffer than `shards` requires gets `InvalidArgument` —
untrusted input that cannot be satisfied, never a truncation. **Both bounds are
knowable before the first call**, which is what stops that refusal being
something a caller can only learn by tripping it: `MAX_LOG_SHARDS` is an ABI
constant and a buffer of that many records is always enough.

**Authority.** Reading the whole machine's kernel log is authority, so it rides
a right rather than being ambient: `Rights::LOG` on the `SysCap` the endowment
architecture already defines, alongside `DEVICE`, `RT` and `MANAGE`
(`handle.rs:65-83` — **CONFIRMED** 2026-08-11, nine bits in a `Rights(u32)`, so
`LOG` is `1 << 9` and `ALL` becomes `0x3ff`). **The manifest spelling is
`syscap = ["logread"]`** — one more row in `toyos_manifest`'s `SYSCAP_RIGHTS`,
beside `rt`, `device` and `dup`. **As built it is two bits and not one**:
`Rights::LOG` is what `SYS_LOG_READ` answers to and `Rights::WAIT` is what lets
the same capability be named in a `POLL_ADD` on `Source::Log`. The call never
blocks by design, so a name that granted the read and withheld the wait would
look complete and trap the one program whose whole loop is read-then-park. Both
bits are on the one full-rights `SysCap` the kernel makes for `/bin/init`
(`loader::spawn_init`) — rights only shrink, so a bit absent at the root is a bit
no manifest can name. It is not a `bool` key: `realtime` is gone and
the table it became refuses a name it does not carry (§0.0).

**Which manifest, and there are twelve of them as built.** `logread` is not one
row in one file. **As shipped at L6 it is on exactly two program names**:
`logd`, in every one of the twelve configs, and `test-runner`, in the seven that
carry one — the six that had a `test-runner` row plus `tests/logrotatecase`,
which L6 added (§5.5). `/bin/console` does **not** hold it, and the draft above
said it did: §5.1a is the argument, and it is this spec's own rule applied to
its own row — a right with no caller is a capability handed out for a plan, and
nothing in `/bin/console` reads a cursor. It seeds its scrollback from the
previous boot's files and will keep doing so until something in it does.
**CONFIRMED** 2026-08-15 — `every_boot_config_runs_logd` (§9.5) reads the parsed
`ProgramConfig` of every config in `ALL_CONFIGS` and asserts both halves: every
config with a `[boot] start` lists `logd`, and `logread` appears on `logd` and
`test-runner` and nowhere else. §5.1a is the whole table.

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
byte-identical text from one implementation. `log!` used to bake the prefix into
the ring bytes and every consumer inherited whatever it produced; the prefix is
synthesised now, which is what lets `logd` render `[2033-03-07 09:14:26.123
cpu0 tid=3]` into `/log` while the panel renders `[0.123 cpu0 tid=3]` into 80
columns, from the same record.

**As built at L3 the kernel's tag is composed by replacing the formatter's first
byte**, and that is worth a sentence because it looks like a hack and is a
consequence of a rule. `Display` opens with the bracket the `kernel` tag has to
sit *inside*, so `log/console.rs` writes `[kernel ` and strips the leading `[`
from the formatter's first fragment. The tidy form is a `tagged(&str)` wrapper
beside `Display` — one implementation either way — and `toyos-abi/src` is
sysroot source, so it belongs to the next ABI landing rather than to this
branch (§11).
`specs/issues/diagnostics/a-console-tag-is-composed-by-replacing-a-bracket.md`
carries it, and the same entry records the one byte of the console line that did
change: an early record renders `[kernel 0.001 cpu0 boot]` where the byte ring
wrote `[kernel 0.001 boot]`, because cpu0's shard *is* the boot shard and the
ABI writes the origin before the flag.

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

**Two constraints bind every chunk of this branch and not only its end state.**
They are here rather than in §10 because a stage that breaks either is wrong
*with a green suite* — neither failure is reachable from the dev host.

1. **No panic path may route through `logd`, at any stage.** Not
   `panic_console`, not `page_forever`, not `halt_all_cpus`'s final lines. The
   moment a panic depends on a userland daemon being alive, the machine that
   needed the report is the machine that cannot produce one, and no test here
   can stage that. §4.4's second `ConsoleObject` is the mechanism; this is the
   rule it serves. `wait_for_log_file` (§6.4) is the one place the dying machine
   *waits* for `logd`, and it is bounded, best-effort and already prints the
   line that says the panel is the only copy — it never routes the report
   through it.
2. **A boot that wedges before the idle loop must not get quieter.** It produces
   nothing at all today
   (`specs/issues/diagnostics/pre-idle-wedge-says-nothing.md`), which is the
   worst diagnostic hole in the tree. This branch is not obliged to fix it —
   **and it does**: `Drain::Inline` (§4.2) puts every boot record on the wire as
   it is written, for the whole boot, so the hole closes at L3 rather than
   narrowing. §8.3 is the root `CLAUDE.md` rule that has to be repealed when it
   does, and `pre_idle_wedge_speaks` (§9.5) is the gate.

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
| boot, up to `klogd`'s spawn — the last statement of `kernel_main` before `smp::set_ready()` | **`Drain::Inline`** — `emit` writes the record to the backend synchronously, after committing it | nothing else can run yet: there is no thread until this statement, and no CPU reaches a pass until the two statements after it |
| steady state | **`Drain::Thread`** — `klogd`, a kernel thread, made runnable at the commit of the record it will drain, by the producer's own `wake_direct` (§2.6a) | it must run on an idle machine, and a runnable task is what stops a CPU halting (`toyos-sched` Invariant T) — including a record committed inside the pass that was about to halt, which the doorbell's own handshake catches |
| panic and shutdown | `drain_inline()` called directly | `klogd` will never run again |

A fallback is a path taken when another fails. These are phases: exactly one is
active, the transition is a single statement — `klogd`'s spawn — and it is
logged. `drain_inline` is one function with three callers, not a degradation.

**The transition is not at `scheduler::init`, and that draft answer was wrong
against the code rather than merely imprecise.** L3 measured it: `smp::boot_aps`
leaves every AP spinning on `SMP_READY` (`arch/smp.rs:280`), which
`smp::set_ready()` publishes as the *second to last* statement of `kernel_main`,
and the BSP reaches no scheduler pass before `enter_idle_loop()` after it. So a
`klogd` spawned at `scheduler::init` sits in a run queue for the whole of phases
5, 6 and 7 — xHCI, the i8042, the GPU, `spawn_init` — while a `Drain::Thread`
`emit` believed it had a drainer. That window is the one a machine with no
console wedges in, and §4.1's second constraint says it may not get quieter. The
mode is therefore derived from the one fact that settles it: **`klogd`'s
published `Arc<KShared>` is null until the spawn**, which §2.6a already needed
for the wake, so there is no second flag to disagree with the first.

**`Drain::Inline` closes `pre-idle-wedge-says-nothing` completely** — not "says
less", the entry's own wording — because every record reaches the wire as it is
written, for the whole boot. Its cost is the boot log at backend speed during
boot: nothing on the T14 (`has_console()` is false, so the mode is a branch and
the records wait in their shards), **+4.4 ms under QEMU** (§1.4a, measured
2026-08-15 on metal-sim, distributions not overlapping on either machine shape),
and on a machine with a real 115200-baud UART the boot log's ~40 KB at ~87
µs/byte would be seconds. **`Drain::Inline` is gated on `has_console()` as
built** — not as a cost measure but because a record must not be marked spoken
when nothing could carry it.

**This paragraph said "nothing measurable under QEMU" and cited §1.4's A/B as
saying the mode is free on this instrument, and both were wrong.** §1.4's A/B is
L1's: it compares a tree with the record ring added *behind* the byte ring
against `main`, so its arms differ by the shard's reserve-and-commit and by
nothing else — there is no `Drain::Inline` in either of them. It is evidence
about the ring and can never be cited about the mode. §1.4a is the mode's own
measurement and it is a cost, for the reason §1.4a gives: the boot swaps
`RingGuard`'s CAS for `BackendGuard`'s one for one and *adds* the synchronous
backend write the ring existed to defer. The trade was taken with the number in
hand — 4.4 ms of boot against 63 kernel lines from a machine that never reaches
a scheduler pass (§9.5) — and it is a trade rather than a wash.

**Three things about the mode as built.**

1. **It is `try_lock` and not `lock`.** `BackendGuard::lock` clears IF and then
   spins, so it is not re-entrant on its own CPU, and in `Drain::Inline` the
   caller is an arbitrary producer: a Ring 0 exception taken inside the backend
   write whose handler logs would spin there forever with interrupts off, on the
   one path that exists to report it. Declining costs nothing — the record stays
   committed, the drain position is shared, and whoever holds the backend
   re-scans every shard before releasing, so the holder drains the decliner's
   record too.
2. **One drain position, not one per context.** `Drain::Inline`, `klogd` and the
   panic path all advance `read::Published`, which is why a record reaches the
   wire once however the machine happened to be running when it was committed.
3. **A backend arriving replays the boot into it.** `write_raw` writes to
   exactly one backend, so a record written while the only one was a 16550 has
   not been heard by the virtio-console that comes up in phase 6.
   `serial::console_changed` rewinds the position and drains again when the
   backend *changes*, which is what `log_ring::set_serial_sink`'s re-seed from
   `retained` was — and exact where that was a 64 KiB window. On the harness's
   own shape this is load-bearing: its UART goes to a file and its
   virtio-console is what the suite reads, so without the replay every test that
   asserts on an early boot line reds.

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

  **As built, three things about that list are sharper than it is.** The
  `.expect` was the *whole* of the refusal — `NewTask::address_space` has always
  been an `Option`, and `KernelCtx.id: Option<TaskId>` already documented `None`
  as the idle context, so the scheduler was already shaped for a context with no
  address space. The trampoline (`loader::start::kernel_start`) has one thing the
  two Ring 3 ones do not, and it is not an absence: **`sti`**.
  `alloc_kernel_stack` writes RFLAGS with `IF` clear into the frame
  `context_switch` pops and the Ring 3 trampolines set `IF` in their `iretq`
  frame instead, so a kernel thread that skipped it would run with interrupts
  masked forever — no timer, no preemption, no wake. And **a blocking site's
  preempt-count baseline is not the same in both contexts**:
  `scheduler::assert_baseline`'s `BASELINE_TRAP` is one because
  `common_entry`'s `lock add` covers the whole of every syscall, and a kernel
  thread's body is not a trap — `trampoline_entry` discharged the single level
  `spawn` gave its context, so it parks at zero. `scheduler::blocking_baseline`
  reads the entitlement from the context; §6.4's tripwire keeps its teeth in
  both, one level apart.

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
  `drain_chunk_to_serial` did, so an IRQs-off window is never longer than one
  chunk. **Eight records, and the number is an interrupt latency rather than a
  batch size.** L3 first held the guard across the whole backlog, which its own
  comment said it did not, and the difference is measurable from outside: over
  five full suites `i8042_undecoded_bytes` red twice on a controller whose byte
  arrived while a drain had interrupts masked, and `71_macro_empty_arg` red
  three times because a daemon's own `write` waited behind the same guard and
  landed after a marker it was written before. Neither reds on the byte-ring
  commit. Bounding the window took the second to zero in five and the first to
  one.

  **That reading blamed one holder and there were two, and the follow-up
  retires half of it.** The other was `write_console`, which took the same guard
  once for a userland-chosen length (§8.1, bounded 2026-08-15 as L3's review
  finding F1) — so every suite in the paragraph above ran with an unbounded
  interrupts-off window still live on the console path, and no rate measured
  then can be attributed to the drain's bound alone. With both bounded:
  **fourteen full suites in one session — five on the tip, nine with the window
  bounded — and neither rate moves. `i8042_undecoded_bytes` 2 of 9 against 1 of
  5; `71_macro_empty_arg` 3 of 9 against 1 of 5.** So the eight-record bound stands on its
  argument rather than on those rates; `71_macro_empty_arg` at "zero in five"
  was a lucky five, and **its mechanism turned out to be nothing on the console
  path at all** — fixed 2026-08-15 in `tests/common/console.rs`, where the whole
  of it is written down. Two causes, and neither is a lock: a daemon's whole
  line landing in the window, and — the one that decided the residual rate —
  this case being `printf("%d", …)` with no newline, so its `17` reaches the
  wire unterminated and the *host's* line splitter appends whoever wrote next.
  That is why bounding the drain looked like it helped: while the next writer
  was the kernel, the capture already cut at `[kernel `. No suite here could
  have told the two apart, which is the caution this paragraph is really about;
  `i8042_undecoded_bytes` is
  `specs/issues/kernel/an-i8042-interrupt-arrives-with-no-byte-during-init.md`'s,
  a driver race this branch's timing moves rather than one it made. **Both are
  rows in `src/redlist.rs` now**, FIRES 3 of 14 and 4 of 14, rather than reds a reader
  of this section has to re-derive. The measurement that settles the blame is
  the second `i8042_undecoded_bytes` row's: back-to-back at host loads 6.4–9.7
  it FIRES 6 of 10, every occurrence `ALONE: GREEN` — the rate tracks host
  load, not either bound.
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

   **And the reason the row must exist is sharper than "the predicate is wrong
   for a kernel thread": without it the outcome is NOT DETERMINED.** The
   predicate is `percpu::syscall_rip() != 0 && percpu::current_tid().is_some()`
   (`main.rs:237`), and `syscall_rip` is **never cleared** — `exceptions.rs:243`
   says so in its own comment and
   `specs/issues/panic-path/syscall-rip-never-cleared.md` is the entry. A kernel
   thread has a tid, so the second clause always holds; the first reads whatever
   *user* thread last ran on that CPU left behind. Work stealing is on, so the
   same panic on the same build takes the recoverable branch on a CPU that has
   served a syscall and the fatal branch on one that has not. A row that merely
   corrected a wrong answer would be a tidiness argument; this one replaces a
   coin toss, and `sched::kthread` is where it lives. The one thing that could
   have replaced it — asking the running task whether `spawn` gave it an address
   space — is unavailable to the two callers that need it: the panic handler may
   be inside a pass, and `blocking_baseline` runs with preemption on, where
   reading the `CpuSched` aliases the `&mut` a preempting pass takes. **Measured,
   not reasoned**: the first draft did read it there, and a full suite produced
   two CPUs in `!!! PANIC REENTRY !!!` on one boot.

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
whole lines under the backend lock. `MAX_CONSOLE_LINE` is 1024 — what
`SW_BUF_SIZE` was — and a longer line is emitted in `MAX_CONSOLE_LINE` pieces.

**As built at L5 the split is not announced, and this paragraph asked for it to
be.** It said "with the count of the split said out loud, because a bound whose
overrun is silent is the defect this replaces", and that is the *record's*
argument rather than the console's: `elided` exists because a message the
producer truncated is otherwise a lie about what was logged, while a line
emitted in two pieces has lost nothing — every byte reaches the wire, in order,
on the one backend. What the split costs is that another producer may get
between the pieces, which §8.1 states in full, and a kernel line announcing it
would itself be a line another producer could get inside. `write_console`'s
chunking has had the same property since L3 and said nothing either.

**The constant lands at L3 rather than at L5, and it lands as an interrupt
latency before it lands as a line bound.** `serial::write_console` holds
`BackendGuard` for one `MAX_CONSOLE_LINE` at a time (§8.1), because the guard
masks interrupts and a userland `write` has no length. L5's buffer then sits in
front of a path whose unit is already the same number, so the line it hands down
is one acquisition and nothing about the bound has to change hands.

**"Per holder" is load-bearing and the first draft of this paragraph said "per
handle" and then put the buffer on the shared object.** A buffer on one shared
object is one buffer that two processes accumulate into, and their two
half-lines splice inside the very mechanism that exists to stop splicing. So it
is **one console *backend*, and one `ConsoleObject` per holder**: the object is
the line buffer plus a reference to the one backend, and the backend keeps
`BackendGuard` as its only serialiser.

**As built at L5, "per holder" is literal, and the shape this section had could
not deliver its own gate.** It said two objects in the machine — init's and
`logd`'s — with every other program still holding a handle to *init's* by
inheritance until L7 replaces stdio with pipes. Between L5 and L7 that is one
buffer shared by init, the compositor, soundd, netd and `test-runner`, which is
exactly the arrangement the paragraph above rejects; `console_line_atomicity`
(§9.5) is two ordinary processes and could not have been green under it. So
`loader::start::build_child_handles` **mints a fresh `ConsoleObject` for each
inherited console slot** rather than duplicating its parent's handle:

- **Authority does not move.** A child gets a console exactly when the slot map
  says it does, which is the rule that was already there, and
  `duplicate_entry`'s rights check still refuses a handle without `DUP`.
- **Aliasing does not move.** Two slots naming one parent object get one child
  object, so a program whose stdout and stderr are the same console still writes
  one stream.
- **The last handle flushes.** `ConsoleObject::drop` emits whatever a process
  that exited mid-line had said; a buffer that dropped it would be a way to lose
  output rather than a way to keep it whole.

**Why the kernel must not depend on `logd` to speak, which survives the change
and is the reason a future agent must not undo it.** The panic path writes the
**backend**, not an object: `panic_flush` takes `BackendGuard` and drains
records itself. So a panic after `logd` has died or been killed has somewhere to
write however many `ConsoleObject`s exist, including none. What the original
"exactly two" bought — an independent channel for the dying machine — is bought
more cheaply by the panic path never going through an object at all, and on the
T14 the panel and its pager are the same story.

**`Console` keeps `Rights::DUP` at L5, and dropping it is L7's.** This section
said L5 drops it (`initial_rights` gives `BASE | READ | WRITE`, and `BASE` is
`DUP | TRANSFER | WAIT` — `object/ops.rs:35`, `:50`), on the argument that
nothing needs it after L7. That is true *after* L7 and false at L5: stdio
inheritance **is** a duplicate — `build_child_handles` calls `duplicate_entry`
on every slot-map pair — so a `Console` without `DUP` refuses every spawn in the
machine. The right dies with its last caller, which is L7's pipes, and not
before. Meanwhile `SYS_HANDLE_DUP` on a console gives one process a second
handle to its *own* object and one buffer still holds the line, which is the
property this section is about.

**A syscall that mints a `ConsoleObject` was the other candidate and is
rejected** (§14 row 12): it would let a process *name* its way to a console,
which is the one thing the endowment architecture exists to make impossible.
Minting one for a child at spawn is not that — it is the parent's slot map
deciding, which is the same capability move the pair already was.

**The second `spawn_init` console, the `console = true` manifest key and
`one_console_holder`'s console clause are therefore not built.** `/bin/logd` is
spawned by init like every other program and gets its own object from the same
mechanism every program's stdio uses; a labelled endowment, a `ProgramConfig`
field and a `cargo test --lib` clause over twelve manifests would all exist to
arrange something the spawn path already arranges. §5.1's row loses `console =
true`, and §9.5's gate keeps its other two clauses under a name that says what
it checks (`every_boot_config_runs_logd`).

**The two reds this buffer retires, kept here because the issue file that held
them closed with it.** `serial-console-has-no-line-atomicity` (diagnostics) was
the write-up and it is deleted at L5 — **named as a slug and never as a path,
here and in every other file that mentions it**, which is what §8.2's own rule
says and what five citations in `kernel/`, `userland/` and `tests/` had to be
corrected to. Its measurements move here rather than becoming pointers that
miss. Both are a *splice*, one writer's line cut open by another's:

- **`hda_tone`, dev host loaded, 1 of 3.** The needle
  `soundd: hda codec0 vendor=1af4` split between `codec` and `0` by a kernel
  record — `landing-1786130703-71774.log`, on a branch whose whole diff was one
  spec file, so the audio path was not in it. Three suites in one session on one
  tree, red on the third; the same test alone on a quiet host was 0 of 3 in that
  session, which is the pair a contention red is read from.
- **`desktop_audio_client`, CI, 1 of 10.** `soundd: client ` and `1 removed`
  came back either side of the kernel's four `exit:` accounting lines, so the
  test counted one removal of two and waited out its 300 s guard —
  systematically, because soundd prints a client's removal exactly while the
  kernel prints that client's exit. Probe-green run `31282019974` rep 10, and
  run `31271983043` on `main`.

Both are a kernel record landing inside a userland line, which is what a buffer
per holder makes unrepresentable: the line reaches the backend whole, under one
`BackendGuard`, and a record cannot be acquired in the middle of it.
`console_line_atomicity` is what holds it — 0 of 2000 on the shipping kernel,
and never 0 under `console-unbuffered`: **red 8 of 8, 2026-08-15, at counts from
1 to 570 of 2000**. The magnitude is a race and only the sign is a verdict; §9.4
says what the two clusters look like. The rows stay in the index as `Retired`,
because a measurement that was taken is still a measurement and this is what took
it off.

The ANSI CSI strip that `SerialWriter` does today (`serial.rs:354`) moves onto
the same path and keeps its reason: the backend must never carry bytes it would
drop.

---

## 5. `logd`

### 5.1 Its manifest row

```toml
[programs.logd]
syscap = ["logread"]     # Rights::LOG | Rights::WAIT on a SysCap dup, §3.2
```

**Two rows are struck and one is deferred, and as built the row is one line.**

- **`console = true` is struck**, and §4.4 is the argument: logd's console
  object is minted for it at spawn like every other program's, so a manifest key
  and a labelled endowment would arrange something the spawn path already
  arranges.
- **`serves = ["log"]` is not built at L6, and it belongs to L7.** This section
  left the question to §5.2 and §5.2 answers it: neither frame has a caller on
  this tree. `Register` carries the read ends of a child's stdout and stderr
  pipes and those pipes are L7's; `Sync` is struck outright (below). An acceptor
  with no client is a port that exists for a plan, which is the same thing §5.1a
  refuses `/bin/console` a `logread` for, and the argument does not get weaker
  because the program in question is this one. **§10's L7 row is where it
  arrives**, with the first `Register`, and `every_boot_config_runs_logd` does
  not count it until then.

It `receives` nothing. It claims no device. It cannot open a compositor
connection, reach netd, or name a process. Its whole authority is: accept
connections on `log`, read the kernel's records, write the console, and write
files — the filesystem being ambient, which is the endowment spec's declared
residual D6 and not this spec's to close.

`[boot] start` lists `logd` **first**. Not because the ports need it — the
endowment architecture's whole point is that every port exists before any server
runs, so a client can connect before `logd` has executed an instruction — but
because the sooner it runs the fewer boot records sit in a shard with no reader.

**One new name and one new key were planned; the key is struck.** `logread` is a row in `SYSCAP_RIGHTS`
(`toyos-manifest/src/lib.rs`) that a program asks for by name in `syscap`, and
init already narrows and endows a `SysCap` duplicate for exactly that
(`init/src/main.rs:527-539`) — nothing new is built. `console` is a
`#[serde(default)] bool` on `ProgramConfig` (`src/build.rs:44`) and on
`toyos_manifest::Program`, **and it is struck** — §4.4 is why.

### 5.1a Twelve manifests, and the two that exist for the machine this is all for

**"The kernel never writes a file" means every boot configuration that does not
run `logd` loses `/log` entirely.** That is not a corner: `logd` must be in the
`[boot] start` of every image whose partition table carries a `TOYOS-LOG`, and
root `CLAUDE.md` says every image does. **The first draft counted six, the tree
had eleven when this was walked on 2026-08-11, and it has twelve as built** —
L6 adds `tests/logrotatecase/` for the rotation arm (§10). Every one of them
declares a `[boot] start`:

| manifest | `[boot] start` today | after |
|---|---|---|
| `system.toml` | compositor, soundd, netd, filepicker | `logd` first |
| `diag/system.toml` | toybox | **`logd` too** — see below |
| `console/system.toml` | console | `logd` too. **`console` does not gain `logread`** — see below |
| `tests/metalcase/system.toml` | compositor, soundd, netd, sshd, test-runner | `logd` first; `test-runner` gains `logread` |
| `tests/netcase/system.toml` | netd, test-runner | the same |
| `tests/sshdcase/system.toml` | netd, sshd, test-runner | the same |
| `tests/testcases/system.toml` | soundd, test-runner | the same |
| `tests/doomcase/system.toml` | soundd, test-runner | the same |
| `tests/doommusiccase/system.toml` | soundd, test-runner | the same |
| `tests/desktopcase/system.toml` | compositor, terminal | `logd` first; **no `test-runner` row**, so no `logread` |
| `tests/desktopaudiocase/system.toml` | compositor, soundd, terminal | the same |
| `tests/logrotatecase/system.toml` | **added at L6** | `logd --rotate-fast` and `test-runner`, for `kernel_log_file`'s rotation arm |

**Two of the twelve have no `test-runner`**, which is why §3.2's "the gate that
reads the log runs inside `test-runner` itself" is a statement about the other
ten and not about every image: the three `log_conservation_smp*` names and the
rest of §9.5's cursor gates cannot run on `desktopcase` or `desktopaudiocase`,
and nothing asks them to.

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

**The console boot could gain something it does not have, and L6 does not give
it.** `/bin/console` seeds its scrollback from the newest `/log/*.log` files
(`console/src/main.rs:44`, `:266`), which are the *previous* boot's; with
`logread` it could show this boot's kernel records live, off the cursor, with no
file in the path. That is a program to write, and **a right with no caller is a
capability handed out for a plan** — so `console/system.toml` gets `logd` and
`/bin/console` gets no `syscap` row until something in it reads a cursor. §3.2's
"`console/system.toml` gives it to `console`" is corrected to say so. Its module doc's *"the kernel's log ring is
64 KiB (`kernel/src/drivers/log_ring.rs`)"* is a citation L3 deletes the subject
of, so L3 rewrites it.

**The gate.** `cargo test --lib`: every manifest that declares a `[boot] start`
lists `logd`, and `logread` appears in the `syscap` of `logd` and `test-runner`
and nowhere else. A manifest added later fails the first clause by default, which
is the direction that bound has to fail in. The console clause this paragraph
carried is gone with the key it counted (§4.4), and
`every_boot_config_runs_logd` is the name (§9.5).

### 5.2 The protocol

One port, and it had two frame kinds. **`Sync` is struck; `Register` is L7's; so
at L6 there is no port.**

```rust
enum ToLogd {
    /// Sent by a spawner, carrying the read ends of the child's stdout and
    /// stderr pipes over SYS_HANDLE_SEND. logd polls them from then on.
    /// **L7**, with the pipes it names.
    Register { label: [u8; MAX_STREAM_LABEL], pid: u32 },
}
```

**`Sync` is struck, 2026-08-15, and the reason is who was going to send it.**
§6.3 gave it one caller — the shutdown path — and that path is `SYS_SHUTDOWN`,
which runs **in the kernel**. Building it would mean the kernel holding a
connector to a userland server, in a namespace something had to grant it, so
that it could ask a question and wait for an answer. That is the inversion this
whole architecture exists to remove, and it would sit on the one path where the
machine is least able to afford a round trip.

Nothing is lost by striking it, because the answer already travels: `LogCursor`'s
`durable` field goes userland-to-kernel on a call logd makes every loop, and
`LOG_DURABLE_NS` is the kernel's running maximum of it (§6.4). So the shutdown
reads a word. **The panic path and the shutdown path become one mechanism
observed from two contexts** rather than two mechanisms that have to be kept
agreeing — which is strictly better than what this section proposed, and is the
argument for the strike rather than an excuse after it: `kernel/src/log/mod.rs`'s
`wait_for_durable` and `apic::wait_for_log_file` differ in their bound and in
whether they yield or spin, and in nothing else.

`MAX_STREAM_LABEL` is 32 bytes. `MAX_STREAMS` is 64 and a 65th `Register` is
`ResourceExhausted` with a line naming the label refused — a bound on the
primitive, answering the caller, never a truncation. All three arrive at L7.

**A server never blocks on a client**, which here means logd never blocks reading
one stream while another has data: it uses `ipc::FrameRx` and an io_uring poll
set over every registered pipe plus `Source::Log` (§3.2), exactly as the
compositor and netd do. **At L6 the poll set is `Source::Log` alone**, which is
the same loop with one member.

### 5.3 Back-pressure, per client

**All of this arrives with the streams, at L7.** A per-client backlog is a bound
on a client, and at L6 logd has no clients: it reads a cursor and writes a file,
and the only producer it can be outrun by is the kernel — which is not a client
and is handled by the shards dropping their oldest and counting them (§2.6).
Written here as designed, unchanged, because L7 builds it.

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
2. It writes one `alert!`-grade line to the console naming which call refused it
   and where the log stops:
   `logd: /log has not answered (the sync: …) — this boot's log is on the console only from /log/<file>`.
3. It **keeps running.** It does not exit, does not retry, and does not queue for
   a device that is not answering. "I stop waiting for this stick and say so" is
   the whole policy.

**What the budget bounds is slowness, and an *error* ends it at once. That split
is measured rather than chosen, and it is the one correction L6 makes to this
section.** The text above called it "a policy over repeated errors and a
slow-but-answering device", and the first half of that does not survive contact
with the tree: a failing write is itself logged by the driver
(`usb-storage: cache flush failed on disk 0`), that line is a kernel record, and
that record is something logd then tries to write — which fails, and logs. So
retrying inside a budget does not sample a device that might recover; it runs a
feedback loop whose gain is one. **Measured 2026-08-15 under `usb-flush-fails`:
1,737 failing flushes in six seconds** with a retrying logd, against **3** with
one that stops on the first error. The loop is in the coupling and not in either
half of it, which is why neither the driver's line nor logd's retry is wrong on
its own.

So `LOG_WRITE_BUDGET` keeps its value and gets the narrower job: a write that
*succeeded* and took longer than five seconds is a volume this program stops
waiting for. That is the case the transport's own 2 s per transfer cannot answer
— a device answering every transfer, slowly, forever — and it is the one the
number was always described against. An error is the other case and needs no
duration: `kernel/src/log_file.rs` disabled its sink on the first one, and the
reason turns out to be this rather than an idle loop's convenience.
`usb_flush_optional` is the gate, at `BOUND = 4` failing flushes, and its
twelve probes after the give-up are what make "and does not start again" a claim
rather than an absence.

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
| `MAX_LOG_BYTES` | 1 MiB, 256 B under the `log-rotate-fast` boot parameter | the same, and the fast value becomes a logd argument — **which makes it a manifest row**: `tests/logrotatecase/system.toml` is metalcase's machine shape with `args = ["--rotate-fast"]` on logd, and the actuator is deleted. The arming moves from a global the kernel parses to a config an image is built from, which is the capability-shaped answer; both callers of the old parameter (`kernel_log_file`'s second arm and `usb_boot_stick_pulled`) boot it |
| `MAX_LOG_PARTS` | 9999 | 9999 |
| naming | `<wallclock>.log`, `unknown-NN.log`, `_0002` continuations | identical, so a stick from before this change and one from after sort together — **and getting a local wall clock in userland is its own problem, below** |
| `classify` | strict; anything unrecognised is somebody else's file and never deleted | identical, and now it is *more* necessary: `/log` is userland-writable and logd is userland |

**The local wall clock, recovered rather than asked for, and the band it
refuses.** The kernel named files from `clock::local_secs()`. Userland has two
calls and neither is that: `SYS_CLOCK_EPOCH` is UTC seconds — a full instant in
the wrong zone — and `SYS_CLOCK_REALTIME` is local `h:m:s` with no date. The
offset is the difference of the two readings' seconds-of-day, taken inside a
bracket (epoch, local, epoch again, retried while the two epoch reads differ) so
the subtraction is exact rather than approximately right.

**That pins the offset modulo 24 hours, and the real range of zone offsets is 26
hours wide** — UTC−12:00 to UTC+14:00 — so the recovery is unique everywhere
except a two-hour band:

- `off ∈ [0h, 12h)`: the other candidate is west of UTC−12:00. **Unique.**
- `off ∈ (14h, 24h)`: `off` itself is east of UTC+14:00. **Unique.**
- `off ∈ [12h, 14h]`: **both candidates are real zones, a day apart.** +12 against
  −12, +13 (Tonga) against −11 (Samoa), +14 (Kiribati) against −10 (Hawaii).
  One pair of readings, two local dates.

**The band is refused by name and takes the `unknown-NN` name the format already
has.** A file claiming a date this program cannot establish is worse than one
saying it has none, which is the rule `UNDATED_STEM` was written under in the
first place. Guessing would be wrong for half the machines in the band and wrong
by a whole day.

**No new syscall, and that was the owner-delegated ruling.** The clean fix is one
field — `SYS_CLOCK_REALTIME` answering a full civil date, or a call handing back
the `UTC_OFFSET_SECS` the kernel already holds — and an ABI change is the
owner's. The proposal is recorded and not taken.

The arithmetic and the calendar live in **`toyos-wallclock/`**, a host-workspace
crate, and that placement is the point rather than tidiness: the argument above
is the one thing here a guest test could not check cheaply, and on the host it is
checked at **every real offset a quarter-hour apart across the whole range** —
each is either recovered exactly or named as one of two candidates, and the
ambiguous count is asserted at 18 so the band cannot silently change width. The
kernel's `Civil` moves into the same crate, so the machine has one calendar
rather than one per side of the syscall boundary.

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

It says so once, on the console, and carries on. **As built at L6 the split is
made, and the four-way table stays with the kernel because the panel is the
kernel's.** `report_log_destination` (`kernel/src/main.rs`) keeps both axes: it
no longer knows *which file*, and it still knows whether the log **volume
mounted**, which is the fact the line exists to carry — a machine with no `/log`
partition leaves no account of itself once userland owns the screen. `/bin/logd`
adds the half only it knows, on its own console handle: `logd: this boot's
kernel log is /log/<file> (<wall clock>)`, or `logd: no /log on this machine -
this boot's kernel log is on the console only`.

**The first draft of this split moved the whole table to logd and that was
wrong, caught by `screen_log_absent`.** `panic_console` paints *records*, so a
userland line reaches a console and never the screen — and this line's whole
audience is somebody looking at a T14 with no serial port. A split that leaves
the panel silent about `/log` deletes the diagnostic on the one machine the
subsystem exists for. `NO_LOG_ALERT` is what the gate reads and it is the
kernel's line.

The panel still paints it red because of `Level` rather than because of three
exclamation marks.

**What "console-only logger" does *not* mean, and the draft above invites the
wrong reading.** logd does not write kernel records to the console, with a volume
or without one. `klogd` already puts every committed record on the wire at the
commit (§4.3), so a second copy from logd would double every line on a machine
whose whole console is the diagnostic. §4.1's split is literal: the kernel keeps
the console, logd keeps the filesystem, and what logd writes to a console is what
only logd knows. The phrase belongs to L7, where logd holds userland's streams
and a machine with no `/log` really does have to put them somewhere.

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
| `/bin/logd` | its own `Console`, minted at spawn | `build_child_handles`, like every other `[boot] start` program — **the second `spawn_init` console is not built and §4.4 says why**: a console per holder makes a labelled endowment for this one program unnecessary |
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

`SYS_SHUTDOWN` (`arch/syscall.rs`) logged "Syncing filesystems…", synced, logged
"Shutting down.", and called `acpi::shutdown()`; both lines died in the ring
(`specs/issues/kernel/shutdown-path-logs-never-reach-console.md`). **As built at
L6**, in order:

1. `log::wait_for_durable()` — wait, bounded, until `LOG_DURABLE_NS` covers the
   newest committed record. **It yields rather than spins**: this is ordinary
   thread context with the VFS lock released, and at `--smp 1` the CPU it is on
   is the only one logd can run on, so a spin would guarantee the bound expired
   on every single-CPU shutdown — which is the width most of the suite boots at.
2. `log::console::drain_inline()` — the console, after logd has answered, so the
   last record including logd's own is on the wire before the power goes. Inline
   because `klogd` has no guarantee of another turn.
3. `acpi::shutdown()`.

**Step 1 is not the `Sync` frame this section asked for, and §5.2 is where that
is struck.** The asker is the kernel; `durable` already travels the other way on
a call logd makes every loop; so the shutdown reads a word instead of holding a
connector. The bound is its own — `SHUTDOWN_DURABLE_NANOS`, 2 s
(`kernel/src/log/mod.rs`) — and not `apic`'s 500 ms, because the two bound
different machines: that one is a panicking machine where the scheduler may be
unable to pick logd at all, this one is an orderly shutdown where every thread is
healthy and what is being waited for is one wake, one `SYS_LOG_READ`, a
write-back, a FAT append and a device cache flush. Two seconds is the same order
as the transport's own `USB_TIMEOUT_NS` for one transfer.

**A machine whose logd has given up on the volume pays the bound, once, and says
so** — `shutdown: /log did not answer in 2000ms, so this shutdown's last lines
are on the console only`. That is the honest outcome rather than a cost: the
lines are not going to reach the stick, and the console is told why.

That closes the entry. **Measured 2026-08-15**: `kernel_log_file` reads the
volume after `run shutdown` and finds `Shutting down.` in it, on both arms —
11,896 bytes on the shipped bound, and on the rotation arm **in part 5 of 5 in
the boot that was sampled**. Both of those numbers are a sample and neither is a
claim: which part the line lands in is how many records the machine emits while
`SYS_SHUTDOWN` is waiting, divided by a 256-byte bound. The gate searches *every*
part of the boot for the line and prints where it found it, which is why the
figure moves between runs without meaning anything moved — and it does move: the
same gate on the same tree later the same day reported **11,907 bytes, six parts,
and the line in part 5 of 6**.

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
logd.** `apic::wait_for_log_file` survives in shape and changes what it waits on.
**As built at L6:**

```
if serial::has_console() { return }             // unchanged: the report is already off the box
let want = read::newest_committed_at_ns()       // sampled once
if LOG_DURABLE_NS >= want { return }
kick every sibling                              // unchanged: a quiet CPU is in sti;hlt
wait, bounded, until LOG_DURABLE_NS >= want
```

**`want` is sampled once, and that is a property rather than an optimisation.**
`panic_console::capture` has already run, so the newest committed record *is* the
report's; taking it once means a sibling that keeps logging on its way down
cannot extend the wait by moving the target.

`LOG_DURABLE_NS` is a kernel global that logd publishes: `LogCursor` carries a
`durable: u64` field that logd sets to the timestamp of the newest record it has
`fsync`ed, and the kernel takes the maximum on the next `SYS_LOG_READ`. One
field, no extra syscall, and logd calls `SYS_LOG_READ` every loop anyway. It is
also what lets Ctrl+Alt+D say how far behind the file is, and — since §5.2's
`Sync` is struck — what the shutdown path reads too.

**`durable` is a number that crossed the trust boundary and decides how long a
kernel waits, so it is clamped. Built at L6 and read by nothing until then**,
which was F3: the clamp was documented here and in the ABI and no kernel code
implemented it, because the panic path was still waiting on the kernel's own file
sink. `log::user::publish_durable` now takes
`min(cursor.durable, read::newest_committed_at_ns())` before the maximum, because
an unclamped `u64::MAX` from a buggy logd makes `wait_for_log_file` return
immediately and the report is silently lost — which is exactly the "a device's
own numbers are untrusted" rule one layer up. Clamping cannot make the wait
*longer* than the bound, so the only thing a hostile logd can do is shorten a
wait for its own output, and that is acceptable. Stated because a field this
shape with no clamp reads as an oversight.

**The ceiling is a number the kernel knows for itself.**
`read::newest_committed_at_ns` is one `Descent` per shard on the stack, no lock
and no allocation — the same shape `snapshot_committed` and `drain_ordered`
already have — descending from `head - 1` to the shard's first committed slot,
which in practice is one iteration.

**A machine with no durable log pays the bound.** `LOG_DURABLE_NS` is zero until
the first publication, which is the honest state of a machine with no `/log`, a
logd that has not run yet, a logd that was killed, or one that has given up on
the volume. Each of those spends the 500 ms on a fatal panic, once, and halts
with the panel as the only copy — which is the correct outcome and not a
regression: the report is not on the stick, so there is nothing to return early
for. Those are §6.6's seven cases and §6.6's subject.

The bound is `LOG_FILE_DRAIN_NANOS`, 500 ms (`apic.rs`), **re-derived because
what it bounds has changed kind, not just size**. Its old derivation was its own
comment: *"the idle loop goes round in microseconds and a flush is one FAT append
plus a sync."* The same wait now covers a **userland process being scheduled**,
two syscalls, a page-cache write-back, a FAT append, a device cache flush and a
second `SYS_LOG_READ` to publish.

**It stays at 500 ms, and L6's re-derivation is about what the number is for
rather than what it now contains.** It is not a prediction of how long the write
takes — it is what a machine with nobody left to do the writing pays on its way
down, against the ~460 ms the panel paint costs on the T14 anyway. A machine
whose logd is alive and schedulable finishes far inside it, and
`screen_fatal_halt_composited`'s `/log` half is what says so on every run.
**The `usb-slow-device` measurement this row asked for is not taken at L6 and is
owed**: §9.6 is where it goes, alongside the interleaved A/B, because it is the
same instrument and the same worktree pair. This is the mechanism that
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
shards. It becomes a pair of timestamps: `dump::request` takes
`clock::nanos_since_boot()` before and after, and
`panic_console::paint_report(from, to)` calls `snapshot_committed(from, to)`. That is **better** than today's bracket, not a
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
| reserved RAM | one shard's worth, 512 KiB, is the natural unit |
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
| Ctrl+Alt+D | two byte `Mark`s | two `nanos_since_boot` readings, §6.5 |
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

**Done at L3 unless a row says otherwise**, and the two rows that moved are
named where they are.

**`kernel/src/drivers/log_ring.rs` — the whole file.** With it:
`LogRing`, `RingCell`, `RingGuard` and its `cli` bracket, `RING_LOCKED` and the
per-record `compare_exchange_weak`, `OWED`,
`has_pending`, `SERIAL_SINK`, `set_serial_sink`, `FILE_SINK`, `FILE_OWED`,
`FILE_DROPPED`, `file_has_pending`, `enable_file_sink`, `disable_file_sink`,
`drain_to_file`, `take_file_drops`, `write_chunk`, `write_chunk_blocking`,
`drain_to_serial`, `drain_chunk_to_serial`, `report_dropped`, `DROP_MARKER_MAX`,
`take_drop_marker`, `DROPPED_BYTES`, `drain_unlocked`,
`DRAIN_CHUNK`.

**L2 already took the reading half**, because the panel stopped calling it
there: `Mark`, `mark`, `peek_tail`, `peek_range` and the `WRITTEN` counter they
existed for are gone, which also took one store out of `append` — the file was
549 lines and is 465.

**`kernel/src/log_file.rs` — the whole file. Deleted at L6; it was 646 lines by
then, not the 565 this row was written against.** With it: `Sink`, `SINK` and its
`Lock`, `COLLECTING`, `POSITION`, `has_pending`, `IN_FLUSH`, `flush_in_progress`,
`install`, `destination`, `poll`, `flush_final`, `Refusal`, `Appender`,
`Sink::{flush,append,continue_in_next_part,stopped_at}`, `path`, `stamp`,
`Class`, `classify`, `ours`, `sweep`, `undated_stem`, `MAX_BLOCKED_NANOS`,
`max_log_bytes`, `UNDATED_STEM`, `DIR`, `MOUNT`. `MAX_LOG_FILES`,
`MAX_LOG_BYTES` and `MAX_LOG_PARTS` move to `userland/logd/src/store.rs` with
their values and their reasons; `classify` moves with them and its strictness
stops being belt-and-braces, because the writer is userland now too. The
`log-rotate-fast` actuator goes — its `actuators!` row with it; the fast value
becomes a logd argument and therefore a manifest row (§5.5).

**And `kernel/src/log/console.rs`'s `write_line` does not go with it**, which
this ledger implied it would: its doc said it goes "when `logd` does the
rendering (L6)". It stays, because the console still renders records and logd
renders its *own* line from `LogRecord`'s `Display` plus a wall-clock prefix
(§3.3) — two prefixes over one formatter, which is what §3.3 designed.

**`kernel/src/arch/idt/timer.rs`'s tick drain, which this ledger did not
name.** The timer ISR held a `try_lock`ed `BackendGuard` for one 512-byte chunk
before `do_preempt`, so that a machine under sustained user load — where no CPU
reaches the idle loop — still emptied the ring. It goes with the ring: `klogd`
is woken by the commit rather than by a tick, and a wake reaches a CPU whether
or not one is idle. **Two drains on the highest-rate event in the kernel become
none**, which is a reduction on the preemption path and is recorded here because
§8.1 is the ledger a reviewer counts against.

**`kernel/src/sched/driver.rs`:** `flush_log_file_if_affordable`,
`LOG_DEFERRAL_CEILING_NS`, `LOG_DEFERRED_SINCE`, `log_file_flush_due`,
`owes_wake` (whose only caller was the first), `drain_serial` and its
`BackendGuard::lock` spin with interrupts disabled, and **two** of the four
pre-`hlt` conditions in `execute`'s `Idle` arm: `log_ring::has_pending` at L3 and
`log_file_flush_due` at L6. The idle loop's `drain_serial()` and
`flush_log_file_if_affordable()` statements. **All done as written**, and the
whole of the log's presence on the idle path is now zero statements and zero
conditions — `idle_loop_is_the_declared_body` is what keeps a third from being
added quietly.

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
acquisition per `MAX_CONSOLE_LINE` of output, ANSI-stripped, no buffering — and
L5 puts the line buffer in front of it. That is not a detour: it makes
`console-unbuffered` (§9.4) literally L3's own state, so the negative control is
a real prior build rather than an invented one. **As built at L5 the arm is
`c.write(buf)` on the object and `serial::write_console` survives with one
caller, the actuator** — the unbuffered path is not a rewritten imitation of L3's
behaviour, it is L3's function.

**"Per `MAX_CONSOLE_LINE`" and not "per `write`", and the difference is a
defect this section carried until 2026-08-15.** The arm as first built took the
guard once around the whole call — and `BackendGuard` is `cli` plus a global
spinlock with the device write *inside* it, while `SYS_WRITE` puts no cap on
its buffer (`arch/syscall.rs`'s `user_bytes` is unbounded) and a 16550 pays a
100,000-iteration THRE spin per byte. So the length of an interrupts-off window
in this kernel was a userland argument, taken under `with_fd_owner_data`'s
per-process lock, on a path where the byte ring it replaced had held no backend
lock at all. That is precisely the holder `kernel/CLAUDE.md`'s `BackendGuard`
caveat refuses and this file's own module header forbids, and the drain three
paragraphs of §4.3 above bounds itself to eight records for the same reason.
`write_console` therefore stages output in a `MAX_CONSOLE_LINE` buffer and takes
the guard per flush.

**What that costs, stated rather than left to be discovered.** A write whose
*stripped* output exceeds 1024 bytes can now interleave with another producer at
that boundary, where before it could not — so the atomicity claim is "whole up
to `MAX_CONSOLE_LINE`", which is exactly the claim §4.4 already makes for the
finished design: a line longer than the bound is emitted in pieces of it there
too. Nothing that is whole after L5 is splittable now. `console_line_atomicity`
writes 200-byte lines and is unaffected, as built; the gate that checked this
before L5 is the three C tests that compare whole stdout, which are
line-oriented and far inside the bound. The re-acquisition is
`ceil(n/1024)` uncontended `cli`/`compare_exchange_weak`/`popfq` triples instead
of one, against a device write per kilobyte that is orders of magnitude dearer.
The CSI stripper's state and the staging buffer both outlive every guard, so a
sequence split by a flush is stripped exactly as one that is not — the same
property the user-side 256-byte chunking already needed.

**And it is not optional in the way "or L3 does not build" suggests — it is what
makes L3 *pass*.** Built with the arm left on the byte ring, a full suite reds
three C tests that compare whole stdout (`30_hanoi`, `90_stdio_buffering`,
`90_struct_init`, measured 2026-08-15): kernel records go to the backend at
commit and userland bytes sat in a ring drained later, so the two streams
interleave in a way neither did before. One lock over both is the fix, and it is
this arm. `serial::write_console` is where the ANSI strip lives now.

**`kernel/src/arch/apic.rs`:** `LOG_FILE_DRAIN_NANOS`'s derivation and `owed()`
are rewritten against `LOG_DURABLE_NS` (§6.4); `wait_for_log_file` survives.
**As built, `owed` went from two predicates to one**, and the pair is not
missed: it existed because the sink's own "anything pending" went false in the
middle of the flush that was writing it, and `LOG_DURABLE_NS` has no such gap by
construction — logd publishes it *after* `fsync` returns, so the word passing a
record's timestamp means that record is on the stick and not that somebody has
started putting it there.

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
`console_line_atomicity` asserts `interleaved().is_none()` on its own capture,
so a tree with the old coupling reintroduced reds on the detector as well as on
the count — and the `console-unbuffered` actuator, which produces exactly
kernel-into-userland splices, reds both. **`logd_gone` was named here as the
second such caller and it is not built** (§9.5): init does not wait on logd's
`Process` handle yet and the clause about a client that keeps printing is L7's,
so that name arrives with L7 and takes this assertion with it. It does **not** become
a suite-wide assertion, and the reason is concrete: `/bin/console` seeds its
scrollback from `/log` and prints lines that legitimately contain `[kernel `,
so `screen_console_shell` would red on correct behaviour. Suite-wide it stays a
note on `must_say`, which is what it is good at.

`is_kernel_line` stays either way — three other call sites use it to count kernel
lines.

**`kernel/src/actuator.rs`:** the `log-rotate-fast` row, and with it the last
reason `kernel/Cargo.toml` names the log at all.

### 8.2 `specs/issues/` this closes

Slugs only, deliberately — and **the reason is no longer a gate, which this
paragraph claimed and which is the L5 review's F2.** `src/docs.rs` walked every
text file in the tree and its `every_named_issue_file_resolves` red on a
`specs/issues/<area>/<slug>.md` path that did not resolve; it and every test
over `specs/` prose were **deleted by owner ruling** (`8d0db10`,
`src/CLAUDE.md`). So a full path written here is not caught by
`cargo test --lib` the moment the file is deleted — it is caught by a reader, or
not at all. Slugs stay, because a slug cannot rot; what changed is that the
discipline is prose and review rather than a test, and a spec that cites a
deleted gate as its safety net is a spec telling the next agent not to look.

**L8 must also de-path the citations elsewhere**, for the same reason and with
nothing behind it but that: several of the entries below are cited by full path
from the root `CLAUDE.md`, from `kernel/CLAUDE.md`, from
`specs/introspection-plan.md` and from `specs/completion-architecture-spec.md`.
The `specs/issues/README.md` protocol says the durable rule moves into the spec
that owns the subject; doing that is the same edit that removes the citation,
and `rg <slug>` is what finds the ones that are left.

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

**As built at L4**, `kernel/src/log/storm.rs`. `log-storm` spawns one kernel
thread per shard — at preempt depth 0, with `IF` set, through the shipped
`emit`, the shipped reservation and the shipped publication — each emitting
1,024 records whose message is a known pattern carrying its producer, its index
within that producer and a checksum over the two, and then one `done` record
naming what it emitted. `test-runner` — which holds `logread` from its own
manifest row, where a spawned test binary would not (§3.2) — reads through
`SYS_LOG_READ` until its own cursor has caught up and stayed caught up, and
asserts:

```
records_emitted  ==  records_read + cursor.lost
```

**with the sequence numbers as the ledger**, which is what makes it computable
at all: `records_emitted` is not a number the reader is told, it is what the
issued sequence numbers say. Per shard the reader tracks the first number it
saw and the gap before each next one, and

```
Σ (first − FIRST_SEQ) + Σ gaps  ==  cursor.lost
```

exactly. A number read twice, or read out of order within a shard, fails on the
spot; a lost record that is not counted fails the equality; a duplicated one
fails it the other way. **And every storm record's whole text is regenerated
from the two numbers it declares and compared byte for byte**, which is stronger
than the checksum this section first asked for: a body half-overwritten by
another generation fails on the byte that differs rather than on a check that
might not have covered it. `len` is checked against what the message decodes to.
This is exact, not statistical, and it is the gate the whole design turns on.

**The storm starts with the first `SYS_LOG_READ` of the boot and not at boot**,
because a storm nobody is reading has spent itself before the gate opens a
cursor: the overlap is then a property of the mechanism rather than of the
harness's timing, and the gate refuses a run in which no record was read while
a producer was still working.

The actuator carries the comment the harness rule requires: nothing else can
reach it, because a real workload's record rate is set by what the kernel happens
to log and cannot be made to saturate a shard.

**Measured on this host under TCG** (`cargo test --test toyos-build -- log_`),
one boot per width, **two samples and not one — every column but `emitted` is a
race and moves between runs**:

| `--smp` | emitted | read | dropped | read while storming | `cursor.lost` | run |
|---|---|---|---|---|---|---|
| 1 | 1,024 | 511 | 513 | 512 | 513 | 2026-08-15 a |
| 1 | 1,024 | 511 | 513 | 512 | 513 | 2026-08-15 b |
| 4 | 4,096 | 2,044 | 2,052 | 1,988 | 2,143 | 2026-08-15 a |
| 4 | 4,096 | 2,111 | 1,985 | 2,055 | 2,076 | 2026-08-15 b |
| 8 | 8,192 | 4,088 | 4,104 | 4,090 | 4,229 | 2026-08-15 a |
| 8 | 8,192 | 4,095 | 4,097 | 4,043 | 4,212 | 2026-08-15 b |

**`--smp 1` is the one width that repeats exactly**, because there the reader
and the single producer share a CPU and the interleaving is the scheduler's
rather than the hardware's. At 4 and 8 the split between `read` and `dropped`
moved by 67 and 7 records between two runs of the same build — which is the
quantity being a race and not the law being one. **The law itself is not a
sample**: `emitted == read + lost` holds exactly on every run, and it is the
guest that computes it.

`cursor.lost` exceeds `dropped` by the ordinary kernel records the same shards
dropped, and the ledger accounts for those too — which is the point of computing
the law over sequence numbers rather than over the storm.

**The reader waits for no record the ring may drop, and it used to.** It read
until every producer had said `done`, and that record is the last thing one
*producer* writes rather than the last thing written to its *shard*:
`sched::driver::placement` picks the least-loaded CPU from a rotating start, so
eight threads spawned back to back are spread only while every published load is
equal, and a task is stealable between its spawn and its first run either way.
Two producers on one shard lap the first one's `done`, and the reader then waited
for a record that was never coming — the L4 review's F2, and **not a prediction:
measured 2 of 7 full suites on the dev host, 2026-08-15**, each time as the
guest's whole 30 s ceiling with *"7 producer(s) done"* in the report, and each
time green alone.

**The fix is the reader and no kernel code.** The obvious kernel-side answer was
measured and rejected first: a park-based barrier that put every `done` past
every patterned record passed alone, passed a whole 259-test suite, and then
hung `log_conservation_smp4` in a 12-wide one with no ceiling report at all. So
`log_gate.rs` decides from its own cursor instead — the log is drained, nothing
new has arrived over eight reads, and nothing new has arrived for 100 ms of
guest time since the last producer record. `logstorm start`, `logstorm done` and
the nesting burst's `done` are all cross-checks where they survived and none is
waited on; the count a producer emitted comes from the highest index the ledger
saw when none of them did. `done=` in the gate's report is how many survived,
which is evidence about the ring rather than an assertion about it. That removes
the class — *a workload whose liveness depends on a record the ring is allowed
to drop* — rather than this instance of it.

**And a producer this reader never sees at all is the ring's policy too.** The
first rewrite kept "every producer must have been read at least once" as a hard
clause, and that is the same mistake one level up: two producers on one CPU
write one shard, and 1,024 records from the second lap all 1,024 of the first.
Measured on the dev host, 2026-08-15 — the first seven suites after the
termination fix were **2 of 7 red on exactly that**, with *"2,582 record(s) were
overwritten in a shard"* on the run that produced it, and no ceiling anywhere
(the reds took three seconds where the old ones took thirty-three). So an unseen
producer is counted, reported as `unseen=`, and **checked against the ledger
rather than waved through**: `unseen` producers emitted `declared` records each
and none was read, so at least that many sequence numbers must be among the ones
the kernel counted lost. That is a necessary condition and not an attribution —
`cursor.lost` is per shard and names no producer — and it is what separates "the
ring lapped its whole run", which is the behaviour under test, from "that thread
never ran", which is a kernel that did not spawn what it said it did.

**What the change costs, stated rather than left to be found.** The termination
condition can, in principle, fire mid-storm: eight empty reads and 100 ms of
guest quiet while a producer is stalled inside its publication bracket. That
ends the read early and computes the law over less of the workload; it cannot
make the law *wrong*, because the sequence numbers this reader took and the loss
its own cursor counted are a consistent snapshot at every instant. The
non-vacuity clauses are what refuse a run that proved nothing: every producer
must have been read at least once, some record must have been read while
producers were still emitting, and the readiness source must have completed a
poll.

**It does not cover §2.3a, and neither does anything else of this shape**: a
producer that moves between CPUs mid-storm is not reachable on this tree at all.
§9.5 records the three workloads that were measured for it and what each
answered; §9.2's nesting gate is what reaches the reservation race instead.

### 9.2 Nesting — the case loom cannot express

Loom models threads, not CPU flags or strict LIFO reentrancy on one CPU, so
§2.4's fourth property needs a machine.

**As built at L4**, `kernel/src/log/nested.rs`. `log-nested-emit` spawns one
kernel thread — **a kernel thread and not the syscall that arms it, and that is
the difference between a gate and a tautology**: `IF` is clear for the whole of
every syscall, so a record emitted from one is bracketed whether or not the
guard exists, and removing the guard would change nothing. That thread emits one
*outer* record; halfway through the body copy of that record, `Shard::commit`
sends **this CPU its own IPI** on a vector installed only in the actuator kernel
(`arch::idt::LOG_NEST_VECTOR`, 0x27) and stands still for a fixed 256 `pause`s.
The handler emits a patterned burst of exactly `SHARD_RECORDS` records, in
§9.1's own format and read by §9.1's own ledger.

**A self-IPI rather than a one-shot LAPIC timer**, which is what this section
asked for first: the LAPIC timer is the scheduler's, an interrupt aimed at it
would have to be taken back from the scheduler, and what the test needs is an
interrupt whose *arrival* is decided by `IF` and by nothing else. The IPI is
that. It is also why the vector exists: the timer vector's Ring 0 branch is pure
assembly that never reaches Rust, so no vector already in the table could carry
a handler that logs.

**The positive verdict.** With §2.3a's bracket the IPI is pending across the
whole reservation and body copy and is delivered the instant the guard drops.
The burst then laps the shard exactly once, so the outer record is dropped **by
the ring's declared drop-oldest policy and by nothing else** — one generation is
what makes that arithmetic exact — and every burst record read regenerates byte
for byte. Measured 2026-08-15 at `--smp 1`: `declared=512 read=511 dropped=1`.

**The two negative controls, and each was run.**

- `log-shared-reservation` turns the reservation's one unlocked `xadd` into a
  load, an open interrupt window and a store, and consumes the injection's
  one-shot *there* instead of mid-body — so the handler's first record takes the
  sequence number the interrupted writer had already read, and the writer's own
  store then puts `head` back where it was. Measured: the gate reds with **`an
  interrupt handler's burst was declared and never seen`**, twice out of two.
- `log-unbracketed-reserve` removes the bracket, so the same IPI lands inside
  the body copy: the burst commits `s + SHARD_RECORDS` into the outer slot and
  the resumed outer writer marks that committed slot `WRITING` under a stale
  sequence number. Measured: the shard drops out of the merge for good and the
  gate reds on its own ceiling with **`650 records over 19,630 reads, 0
  producer(s) done`**, twice out of two.

The second is specifically a negative proof against the tempting final-head
re-check: leaving the newer sequence word in place does not undo the stale body
bytes already copied. The shipping kernel has neither the vector, nor the
window, nor the pause — `LOG_NEST_VECTOR` is installed outside `install_gates`
under `#[cfg(feature = "boot-actuators")]`, because a shipping IDT with a gate
no interrupt can raise is a gate nothing deletes.

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
| `log-commit-early` | the commit store moves **before** the body write | **not built at L4** — see below |
| `log-shared-reservation` | the reservation becomes a load-then-store with an interrupt admitted between the two | `log_nested_emit`, deterministically — **measured 2026-08-15**, 2 of 2 |
| `log-unbracketed-reserve` | §2.3a's complete IF/TF guard is removed from shard selection through final publication, so a producer can resume its body copy after a newer generation committed | `log_nested_emit`'s patterned mid-body lap — **measured 2026-08-15**, 2 of 2 |
| `log-trusts-durable` | the §6.4 clamp on `LogCursor::durable` is removed | **not built at L6, and it is owed — see below** |
| `log-writes-the-file` | `klogd`'s drain appends records to `/log` through the VFS, from the idle loop — the coupling, rebuilt in miniature | **not built at L6, and it is owed — see below** |
| `log-close-cancels-any-syscap` | `ops::close` hands `remove_fd` the sources a `SysCap` or a `Console` names, so a handle going away cancels every poll in the machine on the log or the keyboard — **the behaviour this tree had** until L6 | `log_poll_outlives_a_close` — **measured 2026-08-15**, *"closing a second handle to the same capability completed the log poll with no record behind it"*, red 2 of 2 |
| `console-unbuffered` | `ConsoleObject`'s line buffer is bypassed; each `write` reaches the backend — **which is literally L3's own intermediate state** (§8.1), and as built it calls L3's own function | `console_line_atomicity` — **red 8 of 8, re-measured 2026-08-15**; the count is not, see below |

**`console-unbuffered`'s count is not a number this section may quote, and it
used to.** The row said `4 of 2000` and `2 of 2000`, which reads as the size of
the defect. Re-measured on the dev host on 2026-08-15, eight boots of the same
build under the same command gave **456, 412, 1, 2, 507, 538, 570 and 3** of
2000 — two clusters three orders of magnitude apart. What a boot decides is
whether the two writer processes end up on the machine's two CPUs or on one, and
a writer *preempting* another leaves a far narrower window than a writer running
beside it. So **the verdict is the sign and never the magnitude**: red 8 of 8,
never once zero, with the count printed as evidence about that boot. A control
quoted by its magnitude is one a later reader will reproduce and disbelieve.

**Neither L6 control is built, and the reason is the same one for both: each
needs a *second* half that this chunk does not have, and this section's own rule
is that a control must red something.**

- **`log-trusts-durable`** removes the clamp. Removing it changes nothing on its
  own: the clamp only bites when a reader publishes a value past the newest
  record, and the one reader in a shipped image is `/bin/logd`, which publishes
  the timestamp it actually synced. Making it red therefore needs a *publisher*
  of `u64::MAX` as well as a kernel that believes one — a second knob, in
  userland, on a config where `screen_fatal_halt_composited` runs. The clamp
  itself is built and its ceiling is `read::newest_committed_at_ns`; what is
  missing is the pair of arms that makes its absence observable, and shipping
  half of one would be an actuator that reds nothing, which is what this section
  forbids.
- **`log-writes-the-file`** rebuilds the coupling in miniature: `klogd`
  appending records to `/log` through the VFS from a kernel context. It reds
  `io-depth-probe` and §9.3's reading 1 — **and both of those are L9's
  instruments, which do not exist yet**. An actuator whose gate is a measurement
  nobody takes is a control nobody runs.

**Both move to L9**, where the instruments they red arrive, and this row is the
record that they are owed rather than dropped. What L6 does have against the same
subject is `idle_loop_is_the_declared_body` — a source gate over the exact set of
statements and pre-`hlt` conditions the idle loop may carry — which refuses the
*re-addition* of the coupling rather than measuring its cost.

**`log-commit-early` is not built at L4, and the reason is the workload rather
than the defect.** Publishing the sequence number ahead of the body is only
*observable* to a reader that is at the head of the ring: the reader has to load
a slot while its writer is inside it, and everywhere else it reads bodies that
are complete. §9.1's storm deliberately puts the reader far behind — the loss
path is most of what that gate is about, and at `--smp 8` it reads 4,088 of
8,192 records — so the reader never sits at the head and the defect passes
unseen. Built and run on 2026-08-15 against `log-storm` at all three widths: the
gate stayed **green**, which is an inert control and is what §9.4's own rule
forbids, so the actuator was deleted rather than shipped. What would earn it is a
*paced* storm whose reader keeps up, which is a second workload and not this one.

**And the sentence that followed it here was false, which is the L4 review's
F4.** It said "both controls above red through the same guest-side ledger, on
the same code path", and they do not: `log-shared-reservation` reds on *"an
interrupt handler's burst was declared and never seen"*, which is a presence
check, and `log-unbracketed-reserve` reds on the gate's own liveness ceiling
after the shard drops out of the merge. Both are real failures of the mechanism
and neither is the conservation law failing. **What did red the ledger is two
weakenings of the kernel the reviewer made by hand**, 2 of 2 each: halving the
loss the reader is told about in `read.rs`, and corrupting a storm record's
payload after it is formatted. So the ledger's teeth are demonstrated — by those
two, not by these.

**Neither weakening becomes an actuator, and the reason is what the ledger is
for.** An actuator earns its place by staging a state a *design decision* could
plausibly be wrong about, and these two stage a kernel that has been edited to
lie: `lost / 2` and a scribbled payload are not shapes any refactor of this
subsystem arrives at, they are the arithmetic of the assertion written
backwards. A control of that kind proves the assertion is connected to its
subject, which is worth doing once — it is done, and recorded here — and costs a
permanent row in `actuators!` plus a CI boot to keep proving it forever. The
three controls that do ship each stage a design the tree could genuinely have
had. Argued rather than asserted, and recorded so the next reader does not
re-derive it as an omission.

None can join `INERT_ACTUATORS`. `log-writes-the-file` is the strongest
because it replaces the behaviour rather than a verdict, which is the harness's
own rule for what makes an actuator worth having, and is the reason it is
deferred rather than dropped;
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

- **`log_conservation_smp1`, `log_conservation_smp4`, `log_conservation_smp8`**
  — §9.1, one registered name per width. **It runs inside `test-runner`**, which
  is the manifest program that holds `logread` in each test config; a spawned
  test binary does not inherit a `SysCap` dup (§3.2). **Built at L4**,
  `tests/common/logread.rs`, `Sched::Parallel`. **Three names and not one**: as
  a single name over three boots CI measured **17,112 ms**, over
  `FAST_CEILING_MS`, and the gate the whole design turns on may not sit in the
  nightly tier — while each boot alone is well under the line. The three widths
  are different subjects anyway: at `--smp 1` the reader and the one producer
  share a CPU, at `4` and `8` they do not.
- **`log_nested_emit`** — §9.2, at `--smp 1`, and nesting is a property of one
  CPU: at one width the interrupted writer and its interrupting handler are
  provably the same CPU. **Built at L4**, and it is the gate both reservation
  controls of §9.4 red through.
- **`log_migration_storm`** — **struck 2026-08-15, and not deferred: no workload
  of the shape this row describes exists on this tree.** The row asked for
  `log-storm` "from kernel threads … at `--smp 8` with stealing on, so a producer
  is preempted and re-runs on a sibling". A kernel thread in a Ring 0 loop that
  takes no lock is never preempted here — `need_resched` is consumed only by
  `kernel_exit_to_user_check` and by `preempt::enable`, and such a loop reaches
  neither — and a **running** task is never stolen, only a ready one. Three
  workloads were built and measured at `--smp 8`, counting producers whose
  records were found on more than one shard: one thread per shard in a tight
  loop, **0 of 8**; two per shard with an explicit `yield_now` every 16 records,
  **0 of 16**; two per shard parking 50 µs every 16 records, **0 of 16**. The
  third is the informative one — a park does reach the scheduler and the task
  still came back to the CPU it left. `specs/issues/kernel/a-ring-0-loop-is-never-preempted.md`
  carries the finding. §2.3a's gate is therefore §9.2's, on one CPU, where an
  interrupt is a stimulus a test can aim; `log-unbracketed-reserve` reds there
  rather than here.
- **`console_line_atomicity`** — **built at L5**,
  `tests/toyos-rust-tests/src/bin/console_line_atomicity.rs` and
  `tests/common/console.rs`. In-guest and deterministic in shape: two processes,
  each writing a distinguishable 200-byte line in two `write` calls, 1,000
  iterations on two CPUs. The assertion is a **count of mixed lines equal to
  zero**, not a probability, **and `Serial::interleaved().is_none()` on the same
  capture** (§8.1). Two more, because a count of zero is also what an empty
  capture gives: every one of the 2,000 lines is present at its declared width,
  and the guest declares the width and the count rather than the host carrying a
  second copy of them. **And a third writer that exits mid-line**: it says a
  hundred bytes in two `write`s, never ends them with a newline, and exits, so
  the only thing that can put them on the wire is `ConsoleObject::drop` flushing
  what the last handle left behind — the assertion is the length of that run,
  exact on both sides. **Measured 2026-08-15**: 0 of 2000 on the shipping kernel
  with the hundred bytes whole; under `console-unbuffered`, **red 8 of 8 at
  counts from 1 to 570 of 2000** (§9.4); and with `ConsoleObject::drop`'s flush
  removed by hand, red 2 of 2 with the run at 1 and then 0 bytes.
  The writers are two *processes* and not two threads, because the buffer is per
  console object and two threads of one process share one.
- **`log_poll_outlives_a_close`** — §3.2's second bullet, in guest and
  deterministic. `test-runner` submits a `POLL_ADD` on its own `SysCap`,
  **enters the kernel with it** — `poll_add` only queues a submission entry, and
  a round that closed first would stage nothing and pass on a tree with the
  defect — duplicates the capability, closes the duplicate, and asserts no
  completion. Then a child that runs and exits commits one kernel record
  (`process.rs`'s `exit:` line, which needs no actuator and no privilege) and
  the poll completes on it, so what the close did not take was a live arming
  rather than an absent one. A completion in the immediate window is
  distinguishable from the defect — an honest one leaves records in the cursor —
  and sends the round again, bounded at five. It needs `dup` beside `logread`,
  which `tests/testcases/system.toml` has. **Measured 2026-08-15**:
  `survived=1 records_after=6` green, red 2 of 2 under the control.
- **`pre_idle_wedge_speaks`** — an actuator (`pre-idle-wedge`, not a feature:
  §0.0) that wedges deliberately at the end of boot phase 3 with interrupts off;
  the host asserts the console carries every line up to the wedge and nothing
  after it. Before this branch it carried none, which is the entry this closes.
  Its verdict is content, not a duration, so it is not a `STALLED:` class.
  **On `Profile::Metal`, and that is the test's own claim rather than a
  convenience**: the headless profile's console is a virtio device the kernel
  does not bring up until phase 6, so on that shape a phase-3 wedge has nowhere
  to put a byte and the records wait for a backend that never arrives. metal-sim
  keeps a 16550 that is up from the second statement of `kernel_main`, and it is
  the shape that gets flashed. **Measured 2026-08-15 on the branch tip: 63
  kernel lines reached the console from a machine that never reached a scheduler
  pass**, the same on three consecutive runs. The test prints the count and does
  not assert on it, so the figure is re-read rather than re-derived; it read 61
  when §1.4a's A/B was taken and the boot has gained two lines since.
- **`logd_gone`** — kill logd; the machine survives, `init: logd exited` reaches
  the console, kernel records keep arriving, a client that keeps printing does
  not die, and `Serial::interleaved().is_none()` on the capture. **Not built at
  L6**: init does not wait on logd's `Process` handle yet, and the "a client that
  keeps printing does not die" clause is about the std PAL's `Gone` behaviour,
  which is L7's (§5.7). It goes with L7, and what L6 has against the same subject
  is that a dead logd is now *survivable by construction* — the console is
  `klogd`'s and the panic path writes the backend, so nothing on the kernel's
  side of §4.1 goes quiet when logd does.
- **`shutdown_last_line`** — the guest's last console line is the shutdown's own,
  and `/log` carries it. **Not built as a separate name at L6, and it would be a
  second copy of one**: `kernel_log_file` already reads the volume after
  `run shutdown` and asserts `Shutting down.` is in it, on both of its arms, and
  it is what caught §6.3's wait being absent from the rotation image. A name of
  its own would boot another guest to assert what an existing one already
  asserts, which the test-cost audit is exactly against. **Registered when it can
  claim something that gate cannot** — the *console* half, which is `Serial`'s
  last line rather than the volume's, and which L7's stdio move is what makes
  meaningful.
- **`log_is_durable_after_fsync`** — the gate for §12.4's coupling, host-side
  against the volume: logd writes, `fsync`s, and the harness reads the *image*
  rather than asking the guest. **As built this is `kernel_log_file`'s mid-run
  read and not a name of its own**, and the merge is the honest one rather than a
  saving: that gate reads the *image* while the guest is still running and
  requires `Boot: complete` in it, which is precisely "logd wrote, `fsync`ed, and
  the bytes are on the device" — the same claim, on the same instrument, in a
  test that has to boot anyway. What it does not separate is *which* of the two
  levels the flush reached, and the honest note is that a `SYS_FSYNC` stopping at
  the page cache would still pass it on a machine that shuts down cleanly
  afterwards. **What closes that gap is `usb_flush_optional`**, whose whole
  subject is a device that refuses SYNCHRONIZE CACHE: it reds the moment the
  syscall stops issuing one.
- **`every_boot_config_runs_logd`** and **`idle_loop_is_the_declared_body`** are
  the two `cargo test --lib` gates L6 owes; the first is built and the second is
  **not**, and the reason is worth stating rather than leaving as an absence. Its
  subject — the exact set of statements in `idle_loop`'s body and of conditions in
  `execute`'s pre-`hlt` list — is now *smaller* than when the row was written:
  L6 removed the last log statement and the last log condition, so what the gate
  would declare is a set with nothing of this spec's in it. It is still worth
  having, and it is still compl C9's and C10's to amend; **it is owed and it is
  named here rather than quietly dropped**. What holds the property meanwhile is
  the deletion ledger (§8.1) and a reviewer reading the diff.
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
- **`every_boot_config_runs_logd`** — a `cargo test --lib` gate over **all twelve
  manifests** (§5.1a — eleven plus `tests/logrotatecase/`, which L6 adds),
  reading the *parsed* `ProgramConfig` and never the file text (§3.2): each with
  a `[boot] start` lists `logd` in it, and `logread` appears in the `syscap` of
  `logd` and `test-runner` and nowhere else. A thirteenth manifest added later
  fails it by default. **It was `one_console_holder` and it
  has lost its console clause**, because §4.4 as built mints a console per holder
  at spawn and there is no `console = true` key for it to count.
- **`nmi_does_not_log`** — a grep gate, two clauses:
  `kernel/src/arch/idt/nmi.rs` contains no `log!`, `alert!` or `emit`; and **no
  `log!` in `kernel/` has `!!!` in its format string** (§2.1), which is what makes
  the deletion of `has_alert`'s sentinel checkable instead of asserted.

### 9.6 Measurements this branch owes, that are not pass/fail

- **The boot A/B on the per-record CAS** (§1.4), `xhci_slow_connect`'s
  `Boot: complete` as the instrument, interleaved, on the source and never on an
  instrumented build — that issue's own second lesson. L1.
- **`Drain::Inline`'s cost on a real UART** (§4.2). **Re-scoped 2026-08-15 and
  it is not L3's**: the instrument does not exist in this tree. The cost is a
  115200-baud port writing every boot record synchronously, and QEMU's 16550
  answers instantly — the only thing a guest can measure is §1.4a's +4.4 ms,
  which is the metal-sim UART and is already recorded there. **It moves to the
  metal session checklist**, `specs/plans/metal-boot-plan.md` item 2, which also
  records why that closes only half of it: the T14 has no SuperIO, so what a
  metal session can establish is the *other* claim in §4.2 — that a machine
  with `has_console()` false pays nothing — and the baud-rate arm waits for a
  machine with a port. §4.2's "would be seconds" is arithmetic and is labelled
  as arithmetic.
- **The RMW budget**, counted rather than asserted, and **counted per mode**
  because the two differ:
  - `Drain::Thread` (everything after `scheduler::init`): loses one
    `compare_exchange_weak` per record, keeps one `pushfq`/`cli`/`popfq`-shaped
    guard widened from shard selection through the bounded body publication
    (§2.3a), and gains **one `SeqCst` fence and one relaxed load** per record. No
    locked RMW on the producer's path at all. The five locked operations of the
    post (§2.6a) are paid at most once per `klogd` park, by one producer, inside
    a second bracket of the same kind.
  - `Drain::Inline` (boot, 185 records on one CPU): pays `BackendGuard`'s
    `compare_exchange_weak` and its `cli` per record, plus the synchronous
    backend write. Not a saving and not meant to be one; §4.2's measurement is
    what decides whether it is gated on `has_console()`.

  L1 counts both; L9 measures. **The review-time paired smoke measurement found
  no regression at its 1 ms guest-clock resolution**: immediately before the
  widened guard, `i8042_absent` reached `Boot: complete` in 254 ms without i8042
  and 257 ms with it; immediately after, both arms read 255 ms. This is evidence
  for the owner-approved bounded cost; two later clean rebuilds read 251/252 ms
  and 251/263 ms, for fixed-tree medians of 251/255 ms. The 11 ms scatter in the
  device-present arm is also why this smoke check is not a replacement for the
  interleaved `xhci_slow_connect` measurement this section requires. **The fence
  is L3's to A/B**, on the same
  instrument as §1.4's — `xhci_slow_connect`'s `Boot: complete`, interleaved —
  because it arrives with the wake and not with the ring, and it is separable
  behind a `#[cfg]` if that chunk moves the number.

  **RUN, 2026-08-15, and the fence is free.** Interleaved A/B in one session,
  five reps per arm alternating, `xhci_slow_connect`'s own `Boot: complete` as
  the instrument; the arms are the tip and the tip with `shard.rs`'s two
  `SeqCst` fences removed (the `wake-fence-off` `cfg` replaced by `any()`, so
  the harness's declared-build gate still sees the ordinary kernel), restored
  before commit.

  | arm | reps | mean |
  |---|---|---|
  | both fences (shipping) | 491, 494, 494, 494, 495 | **493.6 ms** |
  | both fences removed | 494, 495, 495, 496, 494 | **494.8 ms** |

  The distributions overlap almost entirely and the sign is the wrong way round
  for a cost — removing the fences read 1.2 ms *slower*. So the fence is free at
  this instrument's resolution and there is nothing for the `#[cfg]` this
  paragraph reserved to separate. **The instrument itself is what had to be
  built to close this**: `xhci_slow_connect` reached its boot stamp only through
  the per-run UART file, which the harness deletes with the guest, so the test
  now prints it (`tests/common/usb.rs`) — one line of output, `i8042_absent`'s
  arrangement, and nothing about the guest is instrumented.
- **What bounding `write_console`'s interrupts-off window costs** (§8.1, L3's
  review finding F1). **MEASURED 2026-08-15** on the tree's own audio/latency
  instrument, gate A's `max_wake_lat_us` — soundd waking later than a DMA
  completion it armed a timer on. Interleaved A/B in one session, arms
  alternating, the tip's single acquisition against the `MAX_CONSOLE_LINE`
  chunking, `µs`:

  | config | arm | n | median | mean | worst |
  |---|---|---|---|---|---|
  | `audio_tone` smp=1 | tip | 10 | 9,518 | 9,338 | 12,210 |
  | | bounded | 10 | **8,165** | **7,892** | **10,456** |
  | `audio_tone` smp=8 | tip | 10 | 7,150 | 7,198 | 9,643 |
  | | bounded | 10 | 7,070 | 7,735 | 11,173 |
  | `audio_tone_load` smp=1 | tip | 15 | 6,102 | 6,195 | 6,617 |
  | | bounded | 15 | 6,256 | 6,585 | 11,852 |
  | `audio_tone_load` smp=8 | tip | 15 | 5,865 | 6,757 | 20,856 |
  | | bounded | 15 | 5,707 | 8,858 | 30,560 |

  **What it says is "no regression", and it is honest about not saying more.**
  Only `audio_tone` smp=1 moves at all — 14% down on the median and 15% on the
  mean, with the distributions still overlapping — and the two `_load` configs
  put a 20–30 ms outlier in *both* arms, which is that config's own tail and not
  the change's. Nothing regresses. **And gate A cannot see the property that
  actually changed**, which is stated rather than implied: every line these
  guests write is far under `MAX_CONSOLE_LINE`, so it takes exactly one
  acquisition on both arms. What the fix removes is an interrupts-off window
  whose length was a userland argument, and no workload that writes ordinary
  lines can exhibit it — the argument in §8.1 is what carries that, and the
  measurement's job is to show the re-acquisitions cost nothing.
- **The `--slow-usb` A/B, unmoved** (§10's L3 gate column, §9.3's first
  reading). **RUN 2026-08-15**, five reps on the branch tip: `audio_tone` smp=1
  under `--slow-usb` reads 165,212 / 164,377 / 166,219 / 164,564 / 161,366 µs,
  **mean 164,348 µs**, against the 165,948 µs §9.3 records — unmoved, as that
  column requires, because nothing about the disk has changed. The ordinary
  stick on the same tree in the same session is 7,892 µs (the table above), so
  the slow-stick penalty is **20.8x** here against the 23.3x §9.3's pair of
  figures gives; the two absolute controls come from different trees and
  different sessions and the ratio is the comparable quantity.
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
| **L1** | `kernel/src/log/`: `Slot` over L-ABI's `LogRecord` and `Level`, `Shard`, `emit` with §2.3a's bracket, `log!`/`alert!`/`boot_phase!`, the seven `alert!` conversions **carrying the level with their text unchanged** (below). Wired **behind** the existing byte ring — every `emit` also does today's `write_chunk`, so nothing observable changes. The AP shard in `alloc_percpu` (§2.2) | `kernel-loom/tests/log_record.rs` (W1, W2, W4); host-fast `log_zeroed_init`; the full suite indistinguishable from `main`'s; the boot A/B of §9.6. W2b's CPU-flags negative arrives with `log_nested_emit` at L4 |
| **L2** | `snapshot_committed`; `panic_console`, `boot_checkpoint` and `sched::dump` re-pointed at records and at `Level`; **the `!!!` sentinel deleted from the seven `alert!` texts and from the ~20 assertions that read it**; the `KernelArgs` `{:?}` site split into several records; `Mark` → a pair of instants; the backtrace producer's head-and-tail elision (§2.1a), and `nmi_does_not_log`'s two clauses with it | `screen_panic_muted`, `screen_fatal_halt`, `screen_fatal_halt_composited`, `blocked_dump`, `screen_blocked_dump`, `dump_nmi_probe`, `screen_console_*` all green; `screen_late_panic`, whose stimulus is a symbol wider than any grid — **it gates the panel keeping a tail and not the elision**, which is `kernel-elide`'s nine host tests |
| **L3** | `drain_ordered`, with the first caller that streams; `Drain::{Inline,Thread}`; **the kernel-thread machinery for one thread** (§4.3) — trampoline, kernel-address-space `ProcessObject`, `driver::spawn`, dump naming, the recoverable-panic predicate with `klogd`'s non-recoverable row; `klogd`'s body and §2.6a's wake (`signal_after_commit`/`arm_waiter`, the IRQ-off `PreemptGuard` witness, `wake_direct`, the park-lot park); `panic_flush`/`flush_final` on records; `log_file::poll` re-pointed at a `drain_ordered` cursor so the file sink survives; **`object/ops.rs:469`'s console arm re-pointed straight at `BackendGuard`** (§8.1) so the chunk builds. **Delete `log_ring.rs` whole**, `SerialWriter`, `drain_serial` and the idle loop's serial statement; **delete the `:523` pre-`hlt` condition** | `kernel-loom/tests/log_wake.rs` (W3, with its negative case); `pre_idle_wedge_speaks`; the `--slow-usb` A/B unmoved (nothing about the disk has changed yet); the fence's A/B. **All three run and recorded in §9.6, 2026-08-15**; the fourth this column used to name, §9.6's `Drain::Inline` measurement, is not L3's and not this tree's — §9.6 says where it went |
| **L4** | **Done 2026-08-15.** The kernel's half of L-ABI, which touches no sysroot source: the `SYS_LOG_READ` dispatch over `drain_ordered` (`kernel/src/log/user.rs`), `Source::Log` and its watcher static (§3.2), `logread` in `toyos_manifest`'s `SYSCAP_RIGHTS` and on `test-runner`'s row in the six test configs that have one. **Two things the row did not name and L4 found it owed**: `Rights::LOG` and `Rights::WAIT` on the one full-rights `SysCap` the kernel makes for `/bin/init` — rights only shrink, so a bit absent at the root is a bit no manifest can name — and `logread` being *two* bits, because `SYS_LOG_READ` never blocks and a holder that may read but not park has to spin. §9.1's storm and §9.2's nesting injection with it | `test-runner` reads its own kernel log and §9.1's conservation law holds across the syscall — `log_conservation_smp1`, `log_conservation_smp4` and `log_conservation_smp8`, and `log_nested_emit` — the registered names, which are three and not one (§9.5). §9.5 records what `log_migration_storm` measured and why it was struck |
| **L5** | **Done 2026-08-15.** One `ConsoleObject` per *holder* over one backend and its line buffer — `build_child_handles` mints a child's rather than duplicating its parent's handle, which is what makes "per holder" literal before L7's pipes exist (§4.4); the ANSI strip and its state move onto the buffer; `MAX_CONSOLE_LINE` unchanged; `ConsoleObject::drop` flushes a process that exited mid-line. **Three things this row asked for are not built and §4.4 says why**: `Console` keeps `DUP` (stdio inheritance *is* a duplicate, so dropping it at L5 refuses every spawn — it dies with L7's pipes), and the second `spawn_init` console and the `console = true` manifest key are unnecessary once a holder's console is minted at spawn | `console_line_atomicity`, 0 of 2000 mixed with `Serial::interleaved` silent and a mid-line exit's hundred unterminated bytes on the wire whole; `console-unbuffered` reds it 8 of 8 and removing `ConsoleObject::drop`'s flush reds it 2 of 2 |
| **L6** | **Done 2026-08-15.** `/bin/logd` (`userland/logd/`, three files): the program, its row in **all twelve** manifests (§5.1a — eleven plus `tests/logrotatecase/`, which L6 adds), `diag/`'s restated comment, rotation, retention, the give-up policy; **`SYS_FSYNC`'s device flush, outright** (§12.4); the §6.4 clamp, which was F3 — documented in the ABI and in §3.2 and implemented nowhere. **`log_file.rs` deleted whole**, with `flush_log_file_if_affordable`, `log_file_flush_due`, `LOG_DEFERRAL_CEILING_NS`, `LOG_DEFERRED_SINCE`, `owes_wake` and the fourth pre-`hlt` condition; `wait_for_log_file` re-pointed at `LOG_DURABLE_NS`; §6.3's shutdown, which reads the same word. **Four things this row asked for are not built and each says why**: §5.2's protocol has no caller before L7 and `serves = ["log"]` goes with it (§5.1); `Sync` is struck outright (§5.2, §6.3); §5.3's per-client backlog is a bound on clients logd does not have yet; and the two negative controls are deferred to L9 with their instruments (§9.4). **One thing it did not ask for and L6 owed**: `toyos-wallclock/`, because naming a file for local time in userland is a recovery problem with a provably ambiguous band (§5.5) | `kernel_log_file` re-pointed and green mid-run and after shutdown — **measured 2026-08-15**, 11,442 bytes on the device 19 ms after the ready marker with `Boot: complete` in them, 11,896 after the shutdown carrying its own last line, and the rotation arm continuing into further parts with that line in one of them — four continuations into five parts with the line in part 5 of 5 in one boot and five continuations into six parts with the line in part 5 of 6 in another, **both samples**: the gate searches every part and prints which, so the figures move between runs without anything having moved; `every_boot_config_runs_logd`; `usb_flush_optional` at `BOUND = 4` failing flushes against **1,737** for a logd that retried (§5.4); `toyos-wallclock`'s nine host tests, the ninth being the whole-domain scan that says `Recovery`'s third variant was a state no input reaches |
| **L7** | userland stdio → IPC: init creates and registers the pipes; the launcher and sshd do the same for what they spawn; the std PAL's `Gone` behaviour; every console assertion in the suite re-pointed. **Three rows L6 handed forward**: `serves = ["log"]` on logd's manifest row and the `log` acceptor with it (§5.1), §5.2's `Register` frame and its `MAX_STREAM_LABEL`/`MAX_STREAMS` bounds, and §5.3's per-client backlog — each arrives with the first thing that sends or fills it, which is these pipes | full suite; the 234 `println!`/141 `eprintln!` sites unchanged and their output still on the console; `every_boot_config_runs_logd` gains its `serves` clause |
| **L8** | the deletion commit; the `specs/issues/` closures of §8.2 **and the citations that go stale with them**; `specs/introspection-plan.md` re-based (§3.4); the three `MAX_CPUS` declarations filed as an issue; all five `CLAUDE.md` files. **`specs/completion-architecture-spec.md` is not in this tree** (**CONFIRMED**: it exists only on `wt/toyos-compl`, not on `origin/main`), so L8 cannot de-path its citations — and nothing will see them, because `src/docs.rs` is deleted (§12.7): it cites three of these slugs by full path and those pointers simply go quiet at that branch's C0, which §12.7 records because no gate will | `cargo test --lib` still has to pass, and **nothing in it checks a citation**: `src/docs.rs` and every test over `specs/` prose were deleted by owner ruling (`8d0db10`), so what gates this chunk is `rg <slug>` and a reviewer reading the diff (§8.2, §12.7) |
| **L9** | measurement: the interleaved four-arm A/B (compl §20.1's protocol, ~68 min of guest time, two worktrees); `io-depth-probe`; the positive log-content assertion; assertions written into `tests/audio-baseline.toml` and the numbers into this spec. **And the two negative controls L6 deferred here**, `log-writes-the-file` and `log-trusts-durable`, because the first reds `io-depth-probe` and §9.3's reading 1 — instruments this chunk is what builds — and the second needs a publisher of a bad `durable` as well as a kernel that believes one (§9.4). **Plus §6.4's `usb-slow-device` measurement of `LOG_FILE_DRAIN_NANOS`**, which is the same instrument and the same worktree pair | §9.3 |
| **L10** | **conditional on the owner (§6.6, §13.1)** — pstore: the reserved region, the panic copy, the boot validate, the `SYS_LOG_READ` flag, logd's `prev-crash` file, `pstore_survives_reset` over QMP, and the `specs/issues/` entry recording that the metal arm is owed | its own; a red here is not a red on L1–L9 |

**Dependencies.** L-ABI → L1. L1 → L2, L3, L4. L3 and L4 → L6. L5 → L7. L6 → L7.
L8 after L7. L9 last. L10 independent of everything after L4.

**The two corrections L4 owed are made.** Both were in `toyos-abi/src/log.rs`
and in §3.2, and both landed on their own pull request before L4's kernel half,
under §11's rule: §3.2's `LogCursor` block was missing `durable` — which made
this spec's own "88 bytes, `24 + 8 * MAX_LOG_SHARDS`" arithmetic false, since
the block shown summed to 80 — and `LogRecord::EMPTY`'s doc gave a reason that
was backwards, *"sequence numbers start at zero"* where they start at one and
`FIRST_SEQ` is the whole argument for why they must. A third was found walking
§3 against the shipped file and went with them: §3.2's `log_read` signature had
neither the `SysCap` the call rides on nor its element type, while the same
section three paragraphs down is what says reading the whole machine's log is
authority. The same landing carried one SDK addition, for the same reason it
could not wait: `toyos::log` re-exports `LogRecord` as `Record`, because the one
type a caller of that module cannot avoid naming was reachable only through
`toyos-abi`, which userland does not depend on.

**One row moved at L2.** `drain_ordered` is **L3's, not L2's.** L2's callers
are the panic surface and the boot checkpoint, and both want
`snapshot_committed`; the streaming reader's first caller is `klogd`, which is
L3's. Delivering it at L2 would be a function nothing calls, which is dead code
by the tree's own rule and a build warning by its build.

**Two rows moved at L1, and each is a thing this table had in the wrong chunk.**

- **`log_conservation_smp{1,4,8}`, `log_nested_emit` and `log_migration_storm` are L4's, not
  L1's.** All three read records back through `SYS_LOG_READ`, and L4 is what
  dispatches it — at L1 there is no way for `test-runner` to see a shard at all.
  The `log-storm`, `log-nested-emit`, `log-shared-reservation` and
  `log-unbracketed-reserve` actuators go with them.
- **Deleting the `!!!` sentinel is L2's, not L1's.** §2.1 has it happening with
  the `alert!` conversion, and it cannot: the panel finds a red row by scanning
  the *text* until L2 re-points it at `Level`, and the suite asserts on
  `!!! PANIC !!!` and `!!! EARLY PANIC !!!` in about twenty places. Deleting the
  marker at L1 would red every one of them while the mechanism that replaces it
  is still two chunks away, against L1's own "nothing observable changes". So L1
  makes the seven sites carry `Level::Alert` **with their text byte-identical**,
  and L2 deletes the marker, re-points the panel and moves the assertions in one
  commit. `nmi_does_not_log`'s second clause — *no `log!` in `kernel/` has `!!!`
  in its format string* — is true from L1 and lands with the rest of that gate.

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

So `SYS_FSYNC` stopped one level short of what `log_file` did. **The earlier
draft said "L6 owes the equivalent regardless of what C12 does to parking";
under the ruled order the hedge is gone and L6 owed it, full stop** — C12 does
not exist yet, and a logd that fsyncs and calls the result durable without it is
a spec lying about its own guarantee.

**As built at L6 the first option is taken: `SYS_FSYNC` gains the mount sync,
for every caller.** `ops::fsync` runs `flush_file` and then `Vfs::sync_for_path`
under one acquisition of the VFS lock — one and not two, because two would let
the file's own mount be unmounted between them. The second option, a distinct
call for logd, needs a new syscall number, which needs discussion, and it would
be a *second* number on a branch §11 has already structured around one; it would
also leave every other `fsync` in the machine quietly weaker than the one program
that noticed. Every `fsync` is slower for this and more honest.

**The gate is `usb_flush_optional` and not a name of its own** (§9.5): its whole
subject is a device that refuses SYNCHRONIZE CACHE, so it reds the moment the
syscall stops issuing one, which is exactly the regression this row exists
against. `kernel_log_file`'s mid-run read is the positive half — the log is on
the *image*, read host-side, while the guest is still running.

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
  machine".** The code did not even then: `ConsoleObject::new()` mints a fresh
  object per call and there was one call site. **As built there are two, and
  neither is the one this bullet predicted**: `loader::mod`'s `spawn_init` for
  init's, and `loader::start::build_child_handles` for *every* inherited console
  slot — one object per **holder**, minted at spawn, rather than one per
  endowment with a second `spawn_init` call for logd. §4.4 carries the argument
  and the reason the predicted shape could not deliver its own gate.
- **endow §1.5 gives `Console` `READ|WRITE|DUP|TRANSFER|WAIT`.** As built it is
  `BASE | READ | WRITE` in `initial_rights`, and `BASE` carries `DUP`. **This
  bullet said L5 drops `DUP` and L5 keeps it**: stdio inheritance *is* a
  duplicate — `build_child_handles` calls `duplicate_entry` on every slot pair —
  so a `Console` without `DUP` refuses every spawn in the machine. The right
  dies with its last caller, which is L7's pipes. §4.4 is where that was argued
  and this row is what it corrects.

Neither touches a syscall number or a struct layout.

**And two programs this spec endows are not in the manifest it would endow them
from** — `console` is in `console/system.toml` and `test-runner` in the
`tests/*case/system.toml` configs, never in `system.toml`. That is what sent
§5.1a looking, and **the table it found is twelve manifests rather than the six
this sentence expected**; eight of the twelve carry a `test-runner` row.

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

### 12.7 One thing this branch breaks on the other branch, named so nobody has to find it

**This section was written against a gate that no longer exists, which is the L5
review's F2**, and the heading said "so CI does not find it". `src/docs.rs`'s
`every_named_issue_file_resolves` walked every text file in the tree and red on
a `specs/issues/<area>/<slug>.md` path that did not resolve; it and every test
over `specs/` prose were deleted by owner ruling (`8d0db10`, `src/CLAUDE.md`).
**CI will not find this — nobody will, until a reader follows the pointer**,
which makes writing it down here the whole of the mechanism rather than an
early warning about a red.

L8 deletes ten entries (§8.2) and de-paths every citation **in this tree**.
`specs/completion-architecture-spec.md` is not in this tree, and its §19 records
that it cites `client-cpu-takes-the-log-flush` by full path in its own §1.3, that
`specs/introspection-plan.md` cites `log-flush-is-unbounded` by full path twice
(`:31`, `:56`), and that seven of its twelve remaining slugs are cited by full
path from files that are not their own entry.

Its §19 concludes that *"§1.3's full-path citation therefore stays live through
this branch and goes stale at log L8, not at C13."* **Under the ruled order that
is backwards**: L8 lands first, so the citation is already dead when that branch
merges `origin/main` at its C0, and it will point at nothing, quietly, for as
long as nobody reads it. It is a one-line edit and it is in nobody's chunk
budget, so it is written here: **the completion branch's C0 de-paths every
citation of a slug this branch closed, in the same commit as the merge**, and
`rg <slug>` over that tree is how it finds them. The `specs/issues/README.md`
protocol — move the durable rule into the spec that owns the subject — is the
same edit.

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
The costs are 512 KiB of reserved RAM, a `KernelArgs` field, a bootloader change,
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

### 13.3 Memory — 4 MiB of per-CPU record ring at the shipped 8 CPUs

**Ruled 2026-08-09: accepted, at 1 MiB.** **Re-ruled 2026-08-14 at 4 MiB**, by
the owner-delegated call recorded in
`specs/issues/diagnostics/a-record-cannot-hold-a-demangled-frame.md`: the record
holds 992 message bytes rather than 224 so that every ordinary line measured in
§2.1 fits whole, which is a 1024-byte record and therefore 512 KiB a CPU. The
64 MiB at 128 cores is accepted with it, and the named escape stays available
rather than pre-emptively taken.

§2.2. Today's log costs 64 KiB. The increase buys fixed-size records (which is
where the atomicity property comes from) times eight CPUs, and 512 slots so
cpu0's whole boot survives until logd runs — 185 records measured, 2.7× headroom.
64 MiB at the 128-core target. The escape is one line and is named. Note that
seven eighths of it is seven 512 KiB `alloc_zeroed`s from the kernel heap at AP
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
9. **`preempt::disable()` around reservation and publication instead of the flags bracket.**
   It excludes scheduler migration and costs `lock add` + `lock sub` per record,
   which is two locked RMWs where §1.4 prices one at 350 ms of boot, while still
   leaving TF's returning #DB path enabled. A correct variant therefore needs
   the flags operation too and has no advantage. §2.3a. **The same cost reason
   applies on the wake post** (§2.6a), where `preempt_off` would additionally put
   `enable`'s `need_resched` poll — a scheduling pass — inside `emit`.
10. **No bracket at all, on the ground that `log!` mostly runs with `IF` already
    clear.** True of every syscall and every IRQ handler, and false of a kernel
    thread at preempt depth 0 — of which L3 adds the first, `klogd` itself, and
    completions' C6 two more. A correctness argument that holds for most callers
    is not one. It also leaves the stale mid-body resume of §2.3a even if the
    reservation itself happens not to migrate.
11. **Making `emit` post a completion through the completion core's ordinary
    path.** It walks a list under the subject's leaf lock, which `emit` may not
    take and which deadlocks the first time anything on that lock's path logs.
    The narrower thing §2.6a takes instead is `wake_direct`, which the scheduler
    has had all along.
12. **A syscall that mints a `ConsoleObject`.** It is the obvious way to give a
    program one of its own, and it hands every process a name it can ask the
    machine's console for — connect-by-name, rebuilt at the one object the
    panic path depends on. **What §4.4 takes instead is the spawn path**:
    `build_child_handles` mints a child's console for each inherited slot, so a
    program gets one exactly when its parent's slot map says it does. That is
    the same capability moved by endowment and not a name anything can ask for.
    (This row used to say "`spawn_init` minting both", which was the shape
    before L5 measured that one object shared by init, the compositor, soundd,
    netd and `test-runner` could not pass §9.5's own gate.)
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
