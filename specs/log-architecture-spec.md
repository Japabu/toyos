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
(`wt/toyos-logd`, at `19c761e`, which is `origin/main`) on 2026-08-09, or from an
entry in `specs/issues/` that records its own measurement. Where a figure is a
prediction it says so and names the chunk that must measure it.

**This spec implements after two other branches land** and is written against
their world: `wt/toyos-endow` (`specs/capability-endowment-spec.md`) and
`wt/toyos-compl` (`specs/completion-architecture-spec.md`). §12 is the
reconciliation, and it states exactly which parts of that second spec's C9 and
C12 this absorbs.

---

## 0. The shape, in one page

```
                     ┌──────────────────────────────────────┐
  log!/alert!/       │  kernel/src/log/                     │
  boot_phase!  ─────►│    emit()  →  per-CPU Shard          │
  (654 + 6 sites)    │      one reservation, one commit     │
                     │      no lock, no locked RMW, no cli  │
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
off for a device round trip), the idle loop's two log statements and the four
pre-`hlt` conditions that exist to serve them, the "affordable flush" heuristic
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
`fd.rs:586` → `SerialWriter::console` → `log_ring::write_chunk_blocking`, so the
unit of interleaving is a `write` syscall.
`specs/issues/diagnostics/serial-console-has-no-line-atomicity.md` has four
recorded splices, one of which reds a landing gate on a documentation-only
branch, and a measured **1 run in 10** for `desktop_audio_client` on CI.

### 1.3 Two things the code says about itself that are not true

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
`log_ring.rs:324` is `RING_LOCKED.compare_exchange_weak(false, true, Acquire,
Relaxed)` — a locked RMW per `write_chunk`, per drain chunk, per cursor move. The
design below removes it and adds no other. **Prediction, to be measured by L1's
boot A/B and not asserted here: removing the per-record CAS is worth a similar
saving to the one that issue records.** If it is not, the measurement is the
finding.

---

## 2. The record and the ring

`kernel/src/log/` — `mod.rs` (the macros and `emit`), `record.rs` (the type),
`shard.rs` (the ring), `read.rs` (the two readers), `console.rs` (the serial
sink and `klogd`).

### 2.1 The record

```rust
// toyos-abi/src/log.rs — one definition, shared by the kernel, the panel and logd
pub const MAX_RECORD_MESSAGE: usize = 224;
pub const RECORD_BYTES: usize = 256;

#[repr(C, align(64))]
pub struct Record {
    /// The sequence number this slot holds. **The only store that publishes a
    /// record, and the only word a reader has to check.**
    commit: AtomicU64,
    at_ns: u64,
    pid: u32,
    tid: u32,
    cpu: u16,
    len: u16,
    /// Bytes the message would have had past `MAX_RECORD_MESSAGE`. Never a
    /// silent truncation.
    elided: u16,
    level: Level,          // u8
    flags: u8,             // bit 0: written before percpu was ready
    msg: [u8; MAX_RECORD_MESSAGE],
}
const _: () = assert!(size_of::<Record>() == RECORD_BYTES);
```

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
makes "half a record" untypeable: see §2.4. The cost is space — the measured
average line is ~90 bytes of the 256 — and §2.3 prices it.

**`Level` has three variants and each has callers today.**

| variant | producer | who reads it |
|---|---|---|
| `Info` | `log!`, 654 sites | everyone |
| `Phase` | `boot_phase!`, 6 sites | the panel repaints on one |
| `Alert` | `alert!` — the fatal markers and `report_log_destination`'s two `!!!` arms | the panel paints the row red |

`Alert` **deletes a magic-value sentinel**: `panic_console::has_alert`
(`mod.rs:1035`) scans each row for three consecutive `!` bytes, and its own
comment enumerates the strings that happen to match. That is the comment the
root `CLAUDE.md` says is the type you should have written. It is not a severity
ordering and nothing orders it; every consumer matches exhaustively.

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
    /// Reservation counter. **Only this CPU writes it**; every other CPU reads.
    head: AtomicU64,
    slots: [Record; SHARD_RECORDS],
}

pub const SHARD_RECORDS: usize = 512;      // 128 KiB per CPU
```

One shard per CPU. cpu0's is a `static` in `.bss`, because `log!` runs before the
heap exists and before `PERCPU_READY`; every AP's is heap-allocated at
`percpu::init_ap`. There is no boot-shard-to-cpu0-shard handoff, because cpu0's
shard *is* the boot shard — the `boot` label in today's prefix becomes
`flags` bit 0 and the renderer prints the same word.

**Why 512.** cpu0 is the only shard that must survive a whole boot with no reader
(`logd` cannot exist until userland does), and the measured worst cpu0 boot in
`specs/metal-logs/` is **257 records** (`2026-08-08-153139-clean.log`; the other
two full boots are 241 and 202). 512 is that with headroom. Every other shard has
`klogd` runnable within a scheduler pass, and the measured worst AP boot is 124
records. One constant rather than two: the saving from giving APs 128 slots is
0.7 MiB and the cost is a runtime mask, and simplicity wins that trade.

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
2. **Reserve.** `let seq = shard.reserve();` — one **non-`lock`-prefixed `xadd`**
   on `head`, behind `arch::percpu_fetch_add`, documented as *interrupt-atomic,
   not SMP-atomic; sound only for a counter one CPU writes*. Not a locked RMW, so
   §1.4's cost does not apply; L1 confirms that with its boot A/B rather than
   asserting it.
3. **Write the body** into `slots[seq % SHARD_RECORDS]` — plain stores.
4. **Commit**: `slot.commit.store(seq, Release)`.
5. **Wake, only in `Drain::Thread` mode**: `fence(SeqCst)` then
   `if LOG_WAITER.load(Relaxed) { completion::post(…) }` — §2.6.

**No lock. No `cli`. No locked RMW.** The path loses today's `pushfq`/`cli`/
`popfq` bracket and its `compare_exchange_weak`, and gains one `SeqCst` fence
(and only while a reader is parked). L1 measures the net.

### 2.4 What makes atomicity unrepresentable to violate

Four properties, each structural rather than checked:

- **There is no way to append bytes.** `emit` is the module's only public
  producer and it takes `fmt::Arguments`. `Shard` has no method that takes a
  `&[u8]`. A caller cannot write half a record because the smallest thing the
  module accepts is a whole one. This is what deletes `write_chunk`,
  `write_chunk_blocking` and `SerialWriter`'s spill-on-overflow, which is the
  mechanism every recorded splice went through.
- **The validity word and the identity word are the same word.** A slot is
  readable as record `s` exactly when `commit == s`. Below `s` it has not been
  written; above `s` it has been recycled. There is no separate "valid" flag that
  could disagree, and no even/odd seqlock convention to get backwards.
- **Exactly one store publishes**, and it is the last one. A record is visible
  whole or not at all, and the reader's re-check of the same word after copying
  the body is total: the only thing that can change a slot's body is a writer
  reserving `s + SHARD_RECORDS`, and that writer's own commit store changes
  `commit` away from `s`.
- **Reentrancy cannot collide.** An exception or IRQ that logs while a record is
  being written on the same CPU takes its own reservation, so the two never share
  a slot. The nested record commits first and the outer commits after; §3.2 says
  what each reader does with that.

### 2.5 Memory-ordering obligations, and the loom models

x86 TSO gives every load acquire and every store release semantics, so a missing
edge is invisible to every guest test. `kernel-loom` compiles the real file a
second time against loom's atomics, and the models below are L1's and L3's
deliverables. Per compl §16.1, `shard.rs` must keep a `sync.rs`-sized dependency
surface — it may name `AtomicU64`, `Record` and nothing that names a subject — or
it cannot be modelled at all. That is a layout requirement, not a note.

| obligation | shape | model |
|---|---|---|
| **W1** publication | body stores → `commit.store(Release)`; reader's `commit.load(Acquire) == s` implies the body is visible | `kernel-loom/tests/log_record.rs` |
| **W2** recycle detection | reader: `load(Acquire)`, copy, `fence(Acquire)`, `load(Relaxed)`, compare. The second load must not be hoisted above the copy | same |
| **W3** the wake edge | producer `commit.store(Release); fence(SeqCst); LOG_WAITER.load(Relaxed)` against reader `LOG_WAITER.store(true, Relaxed); fence(SeqCst); rescan heads; park`. **Invariant: no committed record is left with a parked reader.** Store-buffer shaped, so TSO hides the missing fence and only loom sees it | `kernel-loom/tests/log_wake.rs` |
| **W4** the reservation | `head` is written by one CPU through inline asm and read by others as an `AtomicU64` | shimmed |

**W4's shim, and why it is sound.** Loom cannot model inline asm, so
`percpu_fetch_add` is shimmed to a real `fetch_add`. That is a **strictly
stronger** model: the only behaviour the real instruction has that `fetch_add`
does not is non-atomicity against another CPU's write, and no other CPU writes
`head`. So every interleaving the real code can produce, loom explores, and the
shim cannot hide a race. The argument is stated in the model file, because a shim
whose direction is not argued is a model that proves nothing.

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

---

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
disappear: a torn slot is **detected** by `commit`, not tolerated.

On a live machine a shard can only be blocked by an abandoned reservation, and
the drop-oldest window unblocks it as soon as that shard writes
`SHARD_RECORDS` more records. On a machine quiet enough that it never does, the
records behind it are lost to the streaming reader and present to the snapshot
one. Stated rather than papered over.

### 3.2 The cursor syscall

```rust
pub const SYS_LOG_READ: u64 = 128;      // §3.4 on the number

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
/// `Record`, merged by `at_ns`.
pub fn log_read(cursor: &mut LogCursor, out: &mut [u8]) -> Result<usize, SyscallError>;
```

- **The kernel keeps no per-reader state at all.** No object, no handle
  lifecycle, no cursor to leak or go stale, and a second reader costs nothing.
  That is `specs/introspection-plan.md` §3.3's argument and it is adopted whole.
- **Fixed stride, not packed.** The kernel copies whole 256-byte records and
  userland indexes by shift. At the measured ~90 useful bytes per record the
  waste is real and irrelevant — a few hundred records per second is under
  100 KB/s — and packing would put length arithmetic in the kernel for nothing.
- **`lost` lives in the cursor**, so a reader that ignores loss has to actively
  ignore a field it is already passing.
- **The stream is not consumed.** Two readers see the same records. `logd` and a
  `log-follow` tool coexist with no coordination.
- **Non-blocking.** The wait is compl's one shape: `SYS_LOG_READ` returns 0 and
  the caller arms on the log completion source and parks. §2.5 W3 is that edge.

**`MAX_LOG_SHARDS` is `MAX_CPUS`** and the cursor is 1 KiB at the shipped 8. A
caller passing a smaller buffer than `shards` requires gets `InvalidArgument` —
untrusted input that cannot be satisfied, never a truncation.

**Authority.** Reading the whole machine's kernel log is authority, so it rides
a right rather than being ambient: `Rights::LOG` on the `SysCap` the endowment
architecture already defines, alongside `DEVICE`, `RT` and `MANAGE`
(`capability-endowment-spec.md` §1.2). One more bit in a `u32`; no number
collides. The manifest key is `logread = true`, shaped exactly like `realtime`,
and init endows it to `logd`, `console` and `test-runner` and to nothing else.

### 3.3 The one formatter

`Record`'s rendering to a line lives in `toyos-abi` beside the type, so the
kernel's serial sink, the panel, `logd` and any diagnostic tool produce
byte-identical text from one implementation. Today `log!` bakes the prefix into
the ring bytes and every consumer inherits whatever it produced; after this the
prefix is synthesised, which is what lets `logd` render `[2033-03-07 09:14:26.123
cpu0 tid=3]` into `/log` while the panel renders `[0.123 cpu0 tid=3]` into 80
columns, from the same record.

### 3.4 Syscall numbering, and the three specs that collide over it

Baseline on this tree: **78 syscall constants, highest 98**
(`grep -cE '^pub const SYS_[A-Z_0-9]+: u64 = [0-9]+;' toyos-abi/src/syscall.rs`).

| block | owner |
|---|---|
| 99–112 | `capability-endowment-spec.md` §3.1, fourteen calls |
| 113 | reserved by that spec for `SYS_PORT_REARM` |
| 114–115 | free |
| 116–127 | `completion-architecture-spec.md` §14.2 |
| **128** | **`SYS_LOG_READ`, this spec** |
| 129+ | what is left of `specs/introspection-plan.md` |

**`specs/introspection-plan.md` is wrong on today's tree and must be re-based.**
It allocates `SYS_QUERY = 97`, `SYS_LOG_READ = 98` and `SYS_DISK_ADOPT = 99`
(`:78`, `:414`, `:694`, restated at `:824`). 97 and 98 are
`SYS_DEVICE_REG_READ`/`SYS_DEVICE_REG_WRITE`, allocated after that plan was
written; 99 is `SYS_ENDOWMENTS` on the endowment branch. Its `SYS_LOG_READ` is
**superseded by this one** and its `LogCursor` — a byte cursor with a `Span` ring
tracking which bytes are kernel-origin, `:466-484` — is **deleted by move 3 of
this design**: userland output never enters the kernel ring, so there is no
origin to track and the loop it exists to prevent is unrepresentable for a
different and better reason. Its `SYS_QUERY` and `SYS_DISK_ADOPT` re-base to 129
and 130. L8 edits that plan to say so.

**This spec allocates nothing until L0 reads the merged tree.** 128 is what the
table above predicts; L0 computes the first clean number after both branches
have landed and asserts only that its choice is clean, because both preceding
blocks shift up if anything lands on `main` while they are open.

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

This also resolves an open question in the completion architecture. Its §10
gives `logd` (a kernel thread) the drain into *both* sinks and its §22 records
the consequence — *"the log sink parks on a dead stick … takes the serial sink
with it"* — as **unresolved**. Here it cannot arise: the kernel's console drainer
has no disk in it, so a hung `/log` stick cannot take serial with it, and the
file sink's give-up is a userland policy (§5.4).

### 4.2 Three modes, one transition each, and none of them is a fallback

```rust
enum Drain { Inline, Thread }
```

| phase | who drains to the console | why it is the only thing that can |
|---|---|---|
| boot, up to `scheduler::init` (`main.rs:501`) | **`Drain::Inline`** — `emit` writes the record to the backend synchronously, after committing it | there is no scheduler and no thread; one CPU, nothing else running |
| steady state | **`Drain::Thread`** — `klogd`, a kernel thread, runnable whenever records are pending | it must run on an idle machine, and a runnable task is what stops a CPU halting (`toyos-sched` Invariant T) |
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

Body: `loop { drain_ordered(&mut cursor, &mut backend); park_on(log_completion); }`.

- It is **compl C6's `logd` kernel thread, renamed and narrowed.** C6 owns the
  kernel-thread machinery — identity, the trampoline, the `ProcessObject`, the
  dump naming, the recoverable-panic predicate. This spec owns its body. The
  rename matters because `logd` is now a userland program and two things with one
  name in one machine is the kind of collision a dump report cannot survive; §12
  records it as the one-word change C6 owes.
- It never takes a filesystem lock, never touches a block device, and holds only
  `BackendGuard` — for one bounded chunk at a time, exactly as
  `drain_chunk_to_serial` does today, so an IRQs-off window is never longer than
  one chunk.
- **It is not `usbd` and not `iod`.** Those two stay exactly as compl §10 defines
  them.

### 4.4 The console object, and where line atomicity actually comes from

Three producers reach the backend: `klogd` (whole records), `logd` (whole lines
through its `Console` handle), and the panic path (whole reports). Each takes
`BackendGuard` once per unit. **So every unit that reaches the wire is whole**,
and the ordering between producers is lock-acquisition order, which is time order
to within a lock hold.

The remaining hole is a userland writer that hands the kernel half a line, which
is exactly what `println!` does — `LineWriter` issues `flush_buf()` and then
`inner.write(rest)`, two syscalls per line. **The fix is in the kernel and it is
per handle**: `ConsoleObject` (the endowment spec's fourteenth `KObjectRef`
variant, §1.5) gains a line buffer. A write accumulates until `\n` and emits
whole lines under the backend lock. `MAX_CONSOLE_LINE` is 1024 — today's
`SW_BUF_SIZE` — and a longer line is emitted in `MAX_CONSOLE_LINE` chunks with
the count of the split said out loud, because a bound whose overrun is silent is
the defect this replaces.

There are exactly **two** `Console` handles in the machine: `/bin/init`'s (it
must be able to speak before `logd` exists) and `/bin/logd`'s. A grep gate in
`cargo test --lib` asserts no third program's manifest row has one.

The ANSI CSI strip that `SerialWriter` does today (`serial.rs:354`) moves onto
the same path and keeps its reason: the backend must never carry bytes it would
drop.

---

## 5. `logd`

### 5.1 Its manifest row

```toml
[programs.logd]
serves   = ["log"]
logread  = true          # Rights::LOG on a SysCap dup, §3.2
console  = true          # the second and last ConsoleObject, §4.4
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
set over every registered pipe plus the log completion source, exactly as the
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

### 5.4 The give-up policy, and where the bound comes from

**The bound is logd's and it is a `Budget`.** `LOG_WRITE_BUDGET` — logd's own
constant, in logd's own source, with its reason there: the longest logd will wait
for the log volume before it declares the volume dead. Recommended value 5 s,
which is a policy number and says so; nothing about the device supplies one, and
`specs/completion-architecture-spec.md` §12.3 establishes that USB publishes no
bound for a bulk transfer and that SCSI timeouts are host policy everywhere.

What happens when it expires, in order:

1. logd stops feeding the volume.
2. It writes one `alert!`-grade line to the console:
   `logd: /log has not answered in 5s — this boot's log is on the console only`.
3. It **keeps serving every client and keeps draining to the console.** It does
   not exit, does not retry, and does not queue for a device that is not
   answering. "I stop waiting for this stick and say so" is the whole policy.

**This requires a mechanism the completion architecture leaves open, and this
spec owns it.** compl §12.3 states plainly that after its change a
present-but-hung stick parks the caller with nothing able to end it, and that
C7 must choose between a transport `Tripwire` and a caller-side `Budget`. A
parked writer holding the VFS sleep lock forever wedges every other filesystem
user, so "logd just parks" is not available. **This spec requires the caller-side
form: the log volume's write path takes an absolute `Deadline` from its caller,
and expiry both answers `SyscallError::Io` and cancels the outstanding transfer
through Bulk-Only Reset Recovery** — which is reachable exactly because the
caller's budget, rather than a 2 s transport bound, is what expired. That
preserves today's chain end to end (an `Io` from the device → the sink turns
itself off → it says why), with the sink in userland. If C7 chooses the transport
`Tripwire` instead, logd's policy becomes unexpressible and L6 must add the
deadline argument itself; either way L6 owns the outcome and §12 records it as a
dependency to re-check at L0.

### 5.5 Rotation, retention and naming

Moved from `log_file.rs` unchanged in behaviour, into userland where the policy
belongs:

| constant | today | after |
|---|---|---|
| `MAX_LOG_FILES` | 16 (`log_file.rs:129`) | 16, in logd |
| `MAX_LOG_BYTES` | 1 MiB, 256 B under `log-rotate-fast` | the same, and the fast value becomes a logd argument rather than a kernel cargo feature |
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
| `/bin/init` | the kernel's `Console` | `spawn_kernel` (`loader/mod.rs:889`), unchanged |
| `/bin/logd` | its own `Console` | init, per the manifest |
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

The bound is today's `LOG_FILE_DRAIN_NANOS`, 500 ms, **re-derived** because what
it now bounds is a userland thread's scheduling plus a USB round trip rather than
an idle loop's. L6 measures it under `usb-slow-device` and writes the number it
found. This is the mechanism that keeps `screen_fatal_composited`'s second half —
the assertion that the fatal report is in `/log` and not only in a photograph —
green, and root `CLAUDE.md`'s "three investigations argued from JPEGs" is why it
must be.

**What it cannot cover**, and this is the pstore's subject: a panic whose thread
holds the VFS lock, a panic in logd itself, a #DF, or a triple fault. In each of
those logd cannot run, the bound expires, and the panel is the only copy — which
is exactly what happens today, and the one line the current code prints for it
(`"the panel is the only copy"`) survives.

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

**What it buys, precisely.** Exactly the four cases §6.4 cannot cover: a panic
holding the VFS lock, a dead logd, a #DF, a triple fault. On a T14 with no serial
port those are the boots where the only record is a photograph of a panel — or,
for a triple fault, nothing at all.

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
format is the `Record` array, byte for byte. No second serialisation, no second
formatter, and the previous boot's records read out through the same cursor and
render through the same `Display`.

---

## 7. Diagnostics that must keep working

Every one of these runs from a context with no `Parkable` (compl §6.1), so none
of them can take a sleep lock, and none of them takes a `Lock` today either.

| facility | today | after |
|---|---|---|
| `panic_console` fatal report | `peek_tail` on a byte ring, tolerating tears | `snapshot_committed`, detecting them |
| `boot_checkpoint` (6 phase repaints) | `peek_tail` with IRQs on, "a torn line on screen, nothing more" | `snapshot_committed`; a torn record is skipped, so the panel stops showing garbled lines |
| Ctrl+Alt+D | two byte `Mark`s | two `Instant`s, §6.5 |
| `dump_nmi_probe` | unchanged | unchanged — **the NMI handler still must not log**, and the reason is the same shape: it would reenter its own CPU's shard. A grep gate asserts `arch/idt/nmi.rs` contains no `log!` |
| `/bin/console` scrollback seeding | reads the newest `/log` files | unchanged; the files are logd's now and their names and format do not change |
| the harness's console capture | `is_kernel_line`, `Serial::interleaved` | `is_kernel_line` stays (the prefix is unchanged); `interleaved` is **deleted**, §8 |

Gates that must be green throughout, per compl §17: `blocked_dump`,
`screen_blocked_dump`, `dump_nmi_probe`, `screen_panic_muted`,
`screen_fatal_composited`, `screen_console_shell`, `screen_console_panic`,
`disk_backtrace`, `fault_gates`, `fpu_isolation`, `kernel_log_file`.

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
values. The `log-rotate-fast` cargo feature goes; it becomes a logd argument.

**`kernel/src/sched/driver.rs`:** `flush_log_file_if_affordable` (`:722`),
`LOG_DEFERRAL_CEILING_NS` (`:701`), `LOG_DEFERRED_SINCE` (`:706`),
`log_file_flush_due` (`:743`), `owes_wake` (`:832`, whose only caller is the
above), `drain_serial` (`:762`) and its `BackendGuard::lock` spin with interrupts
disabled, and **two** of the four pre-`hlt` conditions in `execute`'s `Idle` arm:
`log_ring::has_pending` (`:523`) and `log_file_flush_due` (`:552`). The idle
loop's `drain_serial()` (`:681`) and `flush_log_file_if_affordable()` (`:689`)
statements.

The other two pre-`hlt` conditions — `i8042::verdict_due` (`:534`) and
`xhci::port_work_pending` (`:564`) — are **compl C9's, not this spec's** (§12).

**`kernel/src/drivers/serial.rs`:** `SerialWriter` whole (`:287-…`), `SW_BUF_SIZE`,
`SerialWriter::{lock,console,spill,push_byte,write_bytes,write_user}`. The
formatter it was moves into `log::emit`'s stack buffer; the ANSI strip moves onto
the console path (§4.4). `panic_flush` and `flush_final` survive with records in
place of bytes.

**`kernel/src/arch/apic.rs`:** `LOG_FILE_DRAIN_NANOS`'s derivation and `owed()`
are rewritten against `LOG_DURABLE_NS` (§6.4); `wait_for_log_file` survives.

**`kernel/src/log.rs`:** the `log!` macro's body — the `gs:` reads and the
`SerialWriter` — and its false doc comment (§1.3). `PERCPU_READY` survives and
selects the shard.

**`tests/common/serial.rs`:** `Serial::interleaved` (`:74`), `must_say`'s
interleaving note (`:99-106`), and the two `self_check` cases that exercise them
(`:184-195`). **This is the harness's line-splicing heuristic** and it is deleted
because the thing it detects is no longer expressible: every unit that reaches
the backend is whole (§4.4). The `is_kernel_line` predicate stays — three other
call sites use it to count kernel lines.

**`kernel/Cargo.toml`:** the `log-rotate-fast` feature.

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
| `client-cpu-takes-the-log-flush` | audio | L6 | there is no heuristic left to steer, and no CPU takes a flush |
| `pre-idle-wedge-says-nothing` | diagnostics | L3 | `Drain::Inline` puts every boot record on the wire as it is written |
| `log-ring-flushes-one-line-behind` | kernel | L3 | `klogd` is runnable whenever records are pending, so a CPU does not halt with a line unsent |
| `shutdown-path-logs-never-reach-console` | kernel | L6 | §6.3's ordered shutdown |
| `serial-console-has-no-line-atomicity` | diagnostics | L5 | §4.4: three producers, whole units, one lock |
| `sink-append-error-unreachable` | boot-media | L6 | moot with `log_file.rs`; its file-cache finding moves to the file cache's doc |
| `rotation-leaves-the-newest-in-the-older-name` | boot-media | L8 | already stale against #140; **verify before deleting** |
| `the-panic-path-does-not-write-the-log` | boot-media | L8 | it is `kind: rejected` and its argument is now a property of the architecture rather than a decision; the durable sentence moves into §6.4 |
| `idle-machine-looks-wedged` | kernel | L8 | it is already `Superseded by #156`; its log-shape argument is what L3 makes obsolete. **Verify the #156 half is closed first** |

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

---

## 9. Gates

### 9.1 Atomicity under concurrent multi-CPU producers — a conservation law

`log-storm`, a kernel feature actuator. Every CPU emits records in a tight loop;
each record's message is a known pattern carrying its shard, its sequence and a
checksum over the two. A guest test reads through `SYS_LOG_READ` until the storm
ends and asserts:

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
   `tests/audio-baseline.toml` with the run that produced it.
2. **`io-depth-probe`.** Today 4 from the idle loop and 5 from a syscall. The log
   contributes **zero** levels after this: the kernel's log path reaches no
   filesystem, no volume and no controller.
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

| feature | what it reintroduces | what reds |
|---|---|---|
| `log-commit-early` | the commit store moves **before** the body write | §9.1's checksum, within one storm |
| `log-shared-reservation` | the reservation becomes a load-then-store | §9.2, deterministically |
| `log-writes-the-file` | `klogd`'s drain appends records to `/log` through the VFS, from the idle loop — the coupling, rebuilt in miniature | `io-depth-probe`'s depth, and §9.3 reading 1 by the recorded margin |
| `console-unbuffered` | `ConsoleObject`'s line buffer is bypassed; each `write` reaches the backend | `console_line_atomicity`, below |

Each is its own kernel build and four more images in `specs/test-cost-audit.md`'s
ledger; none can join `INERT_ACTUATORS`. `log-writes-the-file` is the strongest
because it replaces the behaviour rather than a verdict, which is the harness's
own rule for what makes an actuator worth having.

### 9.5 New named tests

- **`log_conservation`** — §9.1, at `--smp 1`, `4` and `8`.
- **`log_nested_emit`** — §9.2.
- **`console_line_atomicity`** — in-guest and deterministic in shape: two
  processes, each writing a distinguishable 200-byte line in two `write` calls,
  1,000 iterations on two CPUs. The assertion is a **count of mixed lines equal
  to zero**, not a probability. Under `console-unbuffered` the count is non-zero
  well inside the iteration budget.
- **`pre_idle_wedge_speaks`** — a kernel feature that wedges deliberately in boot
  phase 3; the host asserts the console carries every line up to the wedge.
  Today it carries none, which is the entry this closes. Its verdict is content,
  not a duration, so it is not a `STALLED:` class.
- **`logd_gone`** — kill logd; the machine survives, `init: logd exited` reaches
  the console, kernel records keep arriving, and a client that keeps printing
  does not die.
- **`shutdown_last_line`** — the guest's last console line is the shutdown's own,
  and `/log` carries it.
- **`idle_loop_is_the_declared_body`** — a **host-side source gate**, not a guest
  test: `idle_loop`'s body and `execute`'s pre-`hlt` condition list are exactly
  the declared sets. A condition quietly re-added is invisible to every
  behavioural test. This is compl C9's test and it lands here (§12).
- **`one_console_holder`** — a `cargo test --lib` gate over the manifest: exactly
  two programs may hold a `Console`, and `logread = true` appears on exactly the
  three named in §3.2.
- **`nmi_does_not_log`** — a grep gate: `kernel/src/arch/idt/nmi.rs` contains no
  `log!`, `alert!` or `emit`.

### 9.6 Measurements this branch owes, that are not pass/fail

- **The boot A/B on the per-record CAS** (§1.4), `xhci_slow_connect`'s
  `Boot: complete` as the instrument, interleaved, on the source and never on an
  instrumented build — that issue's own second lesson. L1.
- **`Drain::Inline`'s cost on a real UART** (§4.2). L3.
- **The RMW budget**, counted rather than asserted: the path loses one
  `compare_exchange_weak` and one `pushfq`/`popfq` pair per record and gains one
  `SeqCst` fence, and only while a reader is parked. L1 counts; L9 measures.
- **`LOG_FILE_DRAIN_NANOS` re-derived** against a userland writer under
  `usb-slow-device` (§6.4). L6.

---

## 10. Work breakdown

Eleven chunks, one branch, one pull request. **Every chunk builds, boots and
passes `cargo test`**, plus `cargo test` inside `kernel-loom/` and `toyos-abi`'s
host tests where it touches them. That constraint is what dictates the order:
`log_file.rs` survives, re-pointed, until logd replaces it, so `/log` is never
unwritten across a chunk boundary.

**Merge cadence.** `git merge --no-ff origin/main` at L0 and at every chunk
boundary that follows a landing on `main`, and at minimum once a week. Never
rebase, never amend.

| # | chunk | delivers | gate |
|---|---|---|---|
| **L0** | merge `origin/main` (post-endowment, post-completion). Re-derive every number in this spec against the merged tree; re-check §12 row by row; compute and assert the first clean syscall number; confirm compl C7's §12.3 choice and what it leaves L6 to add (§5.4); claim the sysroot | suite green; §12 has no moved row; the baselines of §9.3 recorded here |
| **L1** | `kernel/src/log/`: `Record`, `Level`, `Shard`, `emit`, `log!`/`alert!`/`boot_phase!`. Wired **behind** the existing byte ring — every `emit` also does today's `write_chunk`, so nothing observable changes. The `KernelArgs` `{:?}` site split into several records | `kernel-loom/tests/log_record.rs` (W1, W2, W4); `log_conservation`; `log_nested_emit`; the boot A/B of §9.6 |
| **L2** | the two readers; the `toyos-abi` formatter; `panic_console`, `boot_checkpoint` and `sched::dump` re-pointed at records; `Mark` → `Instant` | `screen_panic_muted`, `screen_fatal_composited`, `blocked_dump`, `screen_blocked_dump`, `dump_nmi_probe`, `screen_console_*` all green |
| **L3** | `Drain::{Inline,Thread}`; `klogd`'s body on compl C6's thread; `panic_flush`/`flush_final` on records; `log_file::poll` re-pointed at a `drain_ordered` cursor so the file sink survives. **Delete `log_ring.rs` whole**, `SerialWriter`, `drain_serial` and the idle loop's serial statement, and `log_ring::has_pending` from the pre-`hlt` check | `pre_idle_wedge_speaks`; the `--slow-usb` A/B unmoved (nothing about the disk has changed yet); §9.6's `Drain::Inline` measurement |
| **L4** | ABI: `SYS_LOG_READ`, `LogCursor`, `LogRecord` and its `Display` in `toyos-abi`; `Rights::LOG`; the `toyos` SDK wrapper. **This is the ABI chunk** — §11 | a guest test reads its own kernel log and §9.1's conservation law holds across the syscall |
| **L5** | `ConsoleObject`'s line buffer; the ANSI strip moves; `MAX_CONSOLE_LINE` | `console_line_atomicity`; `console-unbuffered` reds; `one_console_holder` |
| **L6** | `/bin/logd`: the program, its manifest row, the protocol, rotation, retention, per-client backlog, the give-up policy and the write deadline §5.4 requires. **Delete `log_file.rs` whole**, `flush_log_file_if_affordable` and everything in §8.1's `driver.rs` list; `wait_for_log_file` re-pointed at `LOG_DURABLE_NS`; §6.3's shutdown | `kernel_log_file` re-pointed and green mid-run and after shutdown; `screen_fatal_composited`'s `/log` half green; `logd_gone`; `shutdown_last_line`; `idle_loop_is_the_declared_body`; `log-writes-the-file` reds |
| **L7** | userland stdio → IPC: init creates and registers the pipes; the launcher and sshd do the same for what they spawn; the std PAL's `Gone` behaviour; every console assertion in the suite re-pointed | full suite; the 234 `println!`/141 `eprintln!` sites unchanged and their output still on the console |
| **L8** | the deletion commit; the `specs/issues/` closures of §8.2 **and the citations that go stale with them**; `specs/introspection-plan.md` re-based (§3.4); the three `MAX_CPUS` declarations filed as an issue; all five `CLAUDE.md` files | `cargo test --lib` — `every_named_issue_file_resolves` is the gate, not "it compiles" |
| **L9** | measurement: the interleaved four-arm A/B (compl §20.1's protocol, ~68 min of guest time, two worktrees); `io-depth-probe`; the positive log-content assertion; assertions written into `tests/audio-baseline.toml` and the numbers into this spec | §9.3 |
| **L10** | **conditional on the owner (§6.6, §13.1)** — pstore: the reserved region, the panic copy, the boot validate, the `SYS_LOG_READ` flag, logd's `prev-crash` file, `pstore_survives_reset` over QMP, and the `specs/issues/` entry recording that the metal arm is owed | its own; a red here is not a red on L1–L9 |

**Dependencies.** L1 → L2, L3. L4 → L6. L3 and L4 → L6. L5 → L7. L6 → L7. L8
after L7. L9 last. L10 independent of everything after L4.

---

## 11. The ABI split, and why the trailer would be a false claim

`toyos-abi` changes in L4: `log.rs` (the `Record`, `LogCursor` and the
formatter), `SYS_LOG_READ`, and one `Rights` bit. Its callers — logd, the SDK,
the console, test-runner — arrive in L6 and L7. Root `CLAUDE.md`: *"An ABI change
lands on its own pull request first … both `--pr` and CI's `abi-split` check
refuse a branch that mixes the two — an `Abi-Inseparable: <why>` commit trailer
declares a split that genuinely cannot be made, out loud."*

**Mine can be split, so the trailer would be a lie.** L4 is a syscall the kernel
implements and nobody calls, and the type it answers with. It compiles, boots and
is testable on its own — §9.1's conservation gate runs against it from a guest
binary with no logd in the picture.

So the recommendation is **an ABI-only pull request between L4 and L5**, landing
`toyos-abi`'s `log.rs`, the syscall number, the kernel's implementation of it and
the `Rights` bit; then `cargo run -- --sync`, then L5 onward on the same branch.
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
`toyos-abi`/`toyos` genuinely differ may claim it. That is true from L4 onward,
which is another reason to land L4 early and separately: with the ABI-only PR the
claim is held for one chunk instead of six.

---

## 12. Coordination — the C9/C12 absorption boundary

Checked against `origin/wt/toyos-compl`'s `specs/completion-architecture-spec.md`
and `origin/wt/toyos-endow`'s `specs/capability-endowment-spec.md` as fetched on
2026-08-09. **L0 re-checks every row against the merged tree; a row that has
moved is a red for this spec, not a detail to absorb.**

### 12.1 What this spec takes from compl's C9

C9 is *"`kernel/src/log/`: core + three sinks + `logd`. Every deletion in §11,
minus `reap_poisoned`"*. **All of the following is this spec's and must be struck
from C9:**

| C9 item | lands as |
|---|---|
| `kernel/src/log/` — the core and its file layout | L1 (and it is a **record** ring, not the 64 KiB byte ring §11 assumes) |
| the serial sink and its drain | L3 |
| the file sink | **deleted, not rebuilt** — it moves to userland, L6 |
| the panel sink | L2 |
| `flush_log_file_if_affordable`, `LOG_DEFERRAL_CEILING_NS`, `LOG_DEFERRED_SINCE`, `log_file_flush_due`, `owes_wake` | L6 |
| `drain_serial` from the idle loop, and its IRQs-off `BackendGuard::lock` spin | L3 |
| the pre-`hlt` conditions `log_ring::has_pending` (`:523`) and `log_file_flush_due` (`:552`) | L3 and L6 |
| `log_file.rs::SINK` from its §9 lock table and its §19 ledger | L6 |
| `MAX_BLOCKED_NANOS` (its §3.2 names it) | L6, with the file it is in |
| `idle_loop_is_the_declared_body` | L6 |
| its §11 conclusion *"A userland `logd` was considered and rejected"* | **overruled by the owner's 2026-08-09 ruling.** The two objections it raises are answered: it cannot log the boot that precedes it (`Drain::Inline` and a 512-slot shard put the boot on the wire and keep it for logd to read), and it cannot log its own death (init does, §5.7) |

### 12.2 What stays with C9

- **`scheduler::log_health()`** (`driver.rs:679`) — C9 moves it off the idle loop
  onto a deadline park. It is a scheduler diagnostic that happens to log; nothing
  about it is the log subsystem. C9 also owns re-pointing the two tests that read
  its `sched: cpu=` counts (`tests/toyos.rs:8229`, `:8722`), which is its §11's
  own note.
- **`scheduler::reap_poisoned()`** — C9's §11 establishes it cannot move
  (`IdleProof`), and this spec does not touch it. The idle loop's end state is
  therefore `pass`, `reap_poisoned` and three `#[cfg]` probes, exactly as C9 says.
- **`i8042::verdict_due` (`:534`) and `xhci::port_work_pending` (`:564`)** — the
  other two pre-`hlt` conditions. Neither is a log condition; both become a
  `usbd` deadline park or a `Poll`, which is C9's and C10's work.
- **`poll_if_pending` leaving `drain_irqs`** — C7's.
- The kernel-thread machinery itself — **C6's**, including the recoverable-panic
  predicate and the `KernelPayload.address_space` retype.

### 12.3 The one-word change C6 owes

C6's table spawns `logd`, `usbd` and `iod`. **Its `logd` is this spec's `klogd`**
and must be named that, because `/bin/logd` is a userland program in the same
machine and `sched::dump` names threads. That is the whole of the change: same
thread, same identity work, a different name and the body in §4.3.

### 12.4 C12 — the write-back queue

**C12 stays entirely with compl.** It is `fd::OpenFile::drop`, the `iod`
write-back queue, `FileObject::on_zero_handles`, `SYS_FSYNC` parking, page-cache
eviction, §13.1's page pinning and `close_file`, and the `disk_backtrace` /
`esp_files` obligations. None of it is log-specific, and the reason it appeared
adjacent is only that `log_file::Sink::append` was one of its callers.

**What this spec removes from C12's surface**, and it is a simplification worth
naming: after L6 the kernel has no `flush_file` caller of its own on a
diagnostic path. `log_file.rs` was the one place where a *kernel* thread reached
the file cache from something that was not a syscall, and it is gone. C12's
remaining callers are all userland-driven.

**One coupling that must be checked at L0**, and it is §5.4's: C12 makes
`SYS_CLOSE` asynchronous and `SYS_FSYNC` park on a real completion. logd's
durability claim (`LOG_DURABLE_NS`, §6.4) is only true if `SYS_FSYNC` means what
it says on the log volume, including the device-level cache flush that
`vfs.sync_mount` does today (`log_file.rs:382`). If C12's `SYS_FSYNC` does not
carry that, L6 owes the equivalent.

### 12.5 The endowment architecture

| what this spec needs | how it stands |
|---|---|
| `logd` as a manifest program with `serves = ["log"]` | its §2.2 shape, unchanged |
| a `Console` for `logd` beside init's | its §1.5's fourteenth `KObjectRef` variant, `ConsoleObject`. §4.4 gives it a line buffer, which is a change to that object's behaviour and not to its shape |
| `Rights::LOG` on `SysCap` | one more bit beside `DEVICE`/`RT`/`MANAGE` in its §1.4; no syscall number, no struct-layout change |
| two new manifest keys, `logread` and `console` | both `#[serde(default)]`, shaped exactly like its `realtime` |
| init creating and endowing every child's stdio pipes | its §4.1 and §4.5 already have init spawning with endowments and the launcher passing extras; the pipes are two more handles per spawn |
| `SYS_HANDLE_SEND` for the pipe read ends | its §3.1, number 103 |
| syscall numbers 99–113 untouched | this spec takes 128 |

Nothing here contradicts that spec and nothing in it needs to change.

---

## 13. Owner-level decisions

### 13.1 pstore — build it or not

§6.6 has both columns. **Recommended: yes, as L10, last, gated on this answer,
with the metal arm filed as owed.** The gate it can have proves the format and
the code path and says nothing about firmware, and the case it covers is the four
panics where the panel is the only copy — which on the T14 means a photograph.
The costs are 128 KiB of reserved RAM, a `KernelArgs` field, a bootloader change,
one `SYS_LOG_READ` flag, and a promise that is best-effort on real hardware.

### 13.2 One pull request, or an ABI-only one first — §11

The pipeline's instruction is one branch, one PR. Root `CLAUDE.md`'s instruction
is that an ABI change lands first, and the `Abi-Inseparable` trailer is for
splits that genuinely cannot be made. **Mine can**, so the trailer would be a
false claim. Recommended: an ABI-only pull request at the L4 boundary, then the
rest on the same branch. The alternative is to accept the trailer with a reason
that is not true, which is worse than the extra landing.

### 13.3 Memory — 1 MiB of per-CPU record ring at the shipped 8 CPUs

§2.2. Today's log costs 64 KiB. The increase buys fixed-size records (which is
where the atomicity property comes from) times eight CPUs, and 512 slots so
cpu0's whole boot survives until logd runs. 16 MiB at the 128-core target. The
escape is one line and is named. **Recorded so he can overrule, not asked.**

### 13.4 The regression this design accepts, stated plainly

If `wait_for_log_file` (§6.4) does not do its job — because the panicking thread
holds the VFS lock, or because logd is what died — then a T14 fatal panic leaves
the report on the panel and not in `/log`. Today the same four cases have the
same outcome, so this is not new; what *is* new is that the mechanism now runs
through a userland process, which has more ways to be unavailable. §13.1 is the
thing that closes it properly. **Recorded so he can overrule**, not asked.

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
   §2.5 W3 pays the fence and loom proves it is needed.
7. **A harness-side splice repair.** The issue itself shows why: the second
   recorded occurrence had a *userland* intruder that `is_kernel_line` cannot
   identify, and the capture ended inside the split line, so no reassembly on the
   captured text could recover it. The fix has to be guest-side, and §4.4 is it.
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
