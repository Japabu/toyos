# Mechanism consolidation audit, 2026-08-15

Dated evidence, frozen. The question asked: **where does ToyOS have N
implementations of one concept, such that one well-tested implementation could
absorb the job and the other N-1 get deleted?** The owner's example was
"io_uring for everything" — one blocking mechanism replacing every other wait
path.

## Provenance

Read-only audit by one orchestrator and eleven parallel sub-auditors. Every
number below comes from a command shown next to it; where a claim was not
re-verified by the orchestrator, it says so.

**Baseline: `e064a96`, the tip of `main` when the audit opened.** Three
sub-auditors measured against `16cb4a9`, which is eleven commits later; `main`
moved during the audit. Every file path and line number cited in the *defects*
section was re-verified against `71a0559` before this document was written, and
all of them still resolve. The consolidation measurements were not re-taken at
`71a0559` and should be treated as accurate to within that window.

Baseline sizes (`git ls-files <dir> | grep '\.rs$' | xargs wc -l`):

| tree | tracked `.rs` | LOC |
|---|---|---|
| `kernel/` | 136 | **51,144** |
| `userland/` | 84 | 30,732 |
| `toyos-cc/` | 32 | 11,243 |
| `toyos-ld/` | 10 | 7,112 |
| `toyos/` | 18 | 3,810 |
| `toyos-abi/` | 12 | 3,426 |

---

## 0. The finding that reframes everything else

**The consolidation this audit was asked to find has already been designed, in
2,394 lines, and it is invisible from `main`.**

```
$ git log --oneline main..wt/toyos-compl | wc -l   →  5
$ git show wt/toyos-compl:specs/completion-architecture-spec.md | wc -l   →  2394
$ ls specs/completion-architecture-spec.md   →  No such file or directory
```

`specs/completion-architecture-spec.md` lives only on `wt/toyos-compl` (also on
`origin`). Its opening line declares it supersedes
`specs/plans/iouring-blocking-spec.md` and `specs/plans/blocking-io-plan.md` —
**and those two are not marked superseded on `main`**, where `CLAUDE.md` still
names them as the live design. It carries a wait inventory by class, a §19
deletion ledger, §20 gates with negative controls, and a 14-chunk work
breakdown.

Its §21.2 states the ordering ruling: **endowment → log → completions**, with
the log branch a hard prerequisite. `ls kernel/src/log_file.rs` → *No such file
or directory*. **The log branch has landed. Pipeline 2 is unblocked.**

Two documents on `main` that agents are told to trust are measurably dead:
`blocking-io-plan.md` §1's entire 13-frame backtrace names `log_file::Sink::flush`,
`log_file::poll`, `log_file::SINK` — none exist; its §2/§3 cite `kernel/src/fd.rs`,
which does not exist (`ls kernel/src/fd.rs` → No such file); its B5 stage names
five symbols, `rg -F` finds **0 of 5** as live code. `iouring-blocking-spec.md`
§17's file layout names **7 of 11 files that do not exist**.

**Nothing reds when a spec cites a file that no longer exists** — and as of the
day before this audit was recorded, nothing reds when a spec cites a deleted
*issue* either. `src/docs.rs` held `every_named_issue_file_resolves` (dangling
`issues/<area>/<slug>.md` pointers anywhere in the tree),
`every_issue_is_well_formed`, and the parsers' negative gate. It was **deleted
whole on 2026-08-14 by `8d0db10`, an explicit owner ruling: "no tests over
documentation."**

```
$ git log --all --oneline -- 'src/docs.rs' | head -1
8d0db10 build: delete the doc tests — owner ruling, no tests over documentation
$ ls src/docs.rs   →  No such file or directory
```

So the finding is not an asymmetry to be closed by a new gate; it is that **the
tree has, by ruling, no mechanical defence against a document going stale in
either direction**, and two dead plans are the visible cost. The remedy that
does not need a gate is the one CLAUDE.md already states — *a plan dies on
completion* — applied to `console-records.md` and `toolchain-wave-plan.md`
(Wave A, A3).

One consequence for pipeline 2, worth knowing before it starts: the completion
spec on `wt/toyos-compl` leans on that gate explicitly. Its §19 says
`every_named_issue_file_resolves` *"walks every text file in the tree and reds
on"* a citation whose issue was closed, and its chunk table calls it *"the real
gate, not 'it compiles', which is a tautology"* for the twelve
`issues/` closures C13 performs. **That gate no longer exists.** The
twelve closures and their full-path citations now have nothing checking them.

---

## 1. Mechanism inventory by concept

### 1.1 Waiting and blocking — the flagship

**The park/wake *primitive* is already one.** `toyos-sched/src/waitq.rs:1-8`
says so, and the code agrees: `rg -n 'EventSource|wake_by_event|event_ready|scheduler::block\b' kernel/`
→ **0 hits**. That consolidation landed at the scheduler migration. Every wake
terminates in one CAS, `TaskShared::claim_wake`.

**What is N-fold is not the wake — it is the *announcement*.** Every event must
be published twice, by hand: once to a wait queue, once to io_uring's
completion queue.

```
$ rg -n 'static IO_URING_WATCHERS|io_uring_watchers: *Lock' kernel/src
→ 6 statics: keyboard.rs:17, mouse.rs:19, net.rs:41, log/user.rs:37,
             drivers/hda.rs:220, drivers/virtio_sound.rs:270
  + 2 object-owned: object/port.rs:74, pipe.rs:142        = 8 stores

$ rg -n 'complete_pending_for_event\(' kernel/src | grep -v ':.*fn ' | wc -l
→ 11 dual-call sites
```

`kernel/src/io_uring.rs:152-172` carries a **second, parallel `Source` enum with
9 variants** and a 69-line dispatch block (`:795-863`) that exists only to route
between the two worlds. `kernel/src/log/user.rs:29-36` admits it: *"A sixth
per-source watcher list, knowingly… Adding a sixth instance of a mechanism that
is about to be unified is the honest cost of landing first."*

**Blocking sites: 15 logical, 13 production.** Each was opened and read; they map
1:1 onto the completion spec's §4.1 classes, and **0 of its 12 cited line
numbers still resolve**.

**Four device waits have no deadline at all** — `nvme.rs:106`, `nvme.rs:435`,
`nvme.rs:459`, `virtio.rs:412`, `virtio.rs:454`.
`rg -n 'deadline|timeout|nanos_since_boot' kernel/src/drivers/nvme.rs` → **0
hits**. Task #171 ("one bounded-wait primitive, then the NVMe controller
contract") is marked completed; whatever it delivered did not reach NVMe.
`issues/kernel/driver-waits-without-a-deadline.md` is the existing entry.

**What a completed pipeline 2 deletes — measured span by span:** ~**520 LOC of
pure dual-notification bookkeeping** (the 8 watcher stores, the `Source` enum
and its `is_ready`/`add`/`remove`/`watchers` dispatch,
`complete_pending_for_event`, `remove_fd`, `scheduler::wake_task`/`wake_sched`/
`wake_pipe_*`, `process::wake_pipe_*`, `waitqs::PARK`), plus ~70 LOC converted
rather than removed (four Class-D device spins become parks; three
byte-identical `settles()` become one).

**Wake call sites today: 50.**

```
rg -n --no-heading 'wake_waiters\(\)|wake_pipe_readers\(|wake_pipe_writers\(|wake_task\(|wake_sched\(|futex_wake\(|post_wake\(\)|complete_pending_for_event\(|waitqs::wake_all_boosted\(|waitqs::wake_all\(|waitqs::wake_one\(|waitqs::wake_n\(|wake_all\(&|wake_direct\(' kernel/src \
  | rg -v ':[0-9]+:(pub )?fn ' | rg -v '^kernel/src/sched/waitqs.rs:' | wc -l   → 50
```

19 leaf-primitive invocations (already one CAS underneath) + 11 dual-call second
halves + 20 forwarders. The compressible part is the 11.

**What the survivor must additionally carry — the honest cost, and it is not
small:**

1. **Cancellation, and it is the spine.** Today a task killed while parked is
   disposed as an exit *at the park*, abandoning its kernel stack. That is
   survivable only because `scheduler.rs:44 assert_baseline` refuses a park
   holding a lock. Make any lock parkable and the same kill abandons a held VFS
   lock forever. `handle_retire`'s reap-in-place arms in `toyos-sched/src/cpu.rs`
   must make a killed task *runnable* instead. **This reopens the crate whose
   migration cost 70 defects.**
2. A `Parkable` token proving the caller may give the CPU back.
3. `SleepLock` — ~150 LOC plus a loom model. `rg -n SleepLock kernel/src` → **0
   hits**. Never built.
4. A typed duration taxonomy — the completion spec needs **six** kinds over 41
   production durations.
5. Two kernel threads (`usbd`, `iod`).

**What genuinely cannot be absorbed:** `sync.rs:46`'s ticket spinlock (there is
no task yet — it *is* what makes a task); `arch/tlb.rs:127` (the acknowledger is
inside an IPI handler with IF clear); AP bring-up and TSC/LAPIC calibration (no
scheduler exists); `sched/dump.rs`'s 5 NMI/IPI spins; `panic_console` (a dying
machine); and every register poll where the hardware raises no interrupt —
those become a bounded `Poll`, never a completion.

**Gates that do not exist** (findings, not requests to weaken anything): no
cross-CPU pipe ping-pong lost-wake canary; no `waitpid`/`join` storm; no
assertion on `wait_transfer`/`wait_command` deadline behaviour; **nothing can red
on an unbounded wait**; no spin-site allow-list.

**Risk: HIGH, concentrated in the cancellation rewrite inside `toyos-sched`.**
The fan-out consolidation itself is mechanical and well-gated.

### 1.2 Rings and queues — the answer is *not* one ring type

Thirteen type-B (our own design) rings plus four hardware-dictated ones. The
hardware set (virtio virtqueue, xHCI TRB ring, NVMe SQ/CQ) is unmergeable by
definition — the device owns the layout.

The consolidation candidates and their genuine differences:

| axis | how the 13 differ |
|---|---|
| element ownership | bytes / fixed POD / **owned heap values** (`Box<MixCommand>`, `PendingConnection`, `Vec<HandleEntry>`) — owned values cannot live in a shared page. This splits the estate permanently. |
| trust boundary | pipe, io_uring, audio have a hostile peer in the memory; log shard, virtio-sound records, i8042, irq_ring are kernel-internal |
| overflow policy | **seven distinct ones**: short-write+park, lap, count-and-drop, producer-stalls, coalesce, drop-newest, hand-back, drop-oldest, refuse |
| reader model | the log shard is **non-consuming and multi-cursor** (`toyos-abi/src/log.rs:191-196`: *"Per-reader state. **The kernel holds none.**"*). Every other ring consumes. This changes what "read" means. |
| geometry | the pipe's capacity is deliberately **not** a power of two (`toyos-abi/src/ring.rs:87-101`); everything else masks. Unifying costs a divide on io_uring's hot path or a hole in every pipe. |
| loom compilability | `kernel/src/log/shard.rs:4-11` — the shard is compiled a second time by `kernel-loom`; a shared type it adopts must be loom-shimmable or **12 machine-checked ordering models stop compiling**. |

**Verdict: a universal `Ring<T, Policy, Owner, …>` would need six type parameters
and be harder to read than any of the thirteen it replaces. Do not build it.**
What actually collapses:

| merge | deleted | confidence |
|---|---|---|
| virtio-sound `RecordRing` (110) + i8042 byte ring (49) + soundd command ring (54) → one `SpscRing<T,N>` | ~130 net | PLAUSIBLE |
| keyboard queue + mouse queue → one `EventQueue<T>` (identical 512-entry drop-oldest; `mouse.rs:10-15` says so out loud) | ~30 + a watcher list, a `waitqs` static, a `Source` variant and an `ops` arm each | CONFIRMED |
| HDA's accumulator → virtio-sound's `Spill` (the same accumulator written twice) | ~35 | CONFIRMED |
| rename `irq_ring` — it is one slot per `(cpu, source)`; its own header says *"overflow has no representation"* | 0, but it stops misleading every reader who greps for rings | free |

**Three rings have no gate on their overflow path at all:**
`rg -ni 'spill' tests/ src/` → 0; `rg -n 'DROPPED' tests/` → 0 hits on the i8042
counter; `rg -n 'MAX_QUEUED_EVENTS' tests/` → 0. Each is a path that only fires
when something is already going wrong.

### 1.3 IPC primitives — the least duplicated family in the tree

**A connection already *is* two pipes.** `kernel/src/object/service.rs:95-101`:
`ConnectionEnd { rx: PipeId, tx: PipeId, inbox, outbox }`. There is no second
ring implementation anywhere — `ops::try_read` dispatches `PipeRead` and
`Connection` to the same `pipe::try_read`.

Pipes cannot collapse into connections because **independently-movable halves are
load-bearing in three production paths**, the sharpest being soundd's liveness
detector: soundd keeps the write end and sends the read end, so a client's death
closes the last handle and soundd's next signal answers `Gone` *by construction
rather than by bookkeeping* (`userland/soundd/src/main.rs:725-729`). A duplex
object cannot express "you get only the receiving half."

What is genuinely deletable:

- **soundd's second framing — 57 LOC, CONFIRMED.**
  `userland/soundd/src/main.rs:1671-1727` re-derives the SDK's 8-byte header by
  hand, using `from_le_bytes` where the SDK uses `from_ne_bytes`.
  `FrameRx<{size_of::<StreamOpenRequest>()}>` is the exact shape. **And it is the
  least-gated framing code in the tree** — `ipc_hostile_peer` targets the
  compositor, `netd_hostile_peer` targets netd; nothing tests soundd against a
  lying peer. That argues *for* the deletion.
- **`SYS_PIPE_MAP` (77) — 42 kernel LOC + one syscall retired, PLAUSIBLE, needs
  pipeline 2.** It maps a 2 MiB page so userland can read 4 bytes. Exactly one
  production caller (`userland/netd/src/main.rs:243-245`), which uses the mapping
  only for `is_reader_closed()`/`is_writer_closed()`. Since the cursors moved into
  kernel memory this is vestigial — but netd needs io_uring readiness on its data
  pipes first.
- Six caller-less `ipc` free functions in `toyos/src/ipc.rs` (48 LOC) whose
  module comment is stale: *"Will become `pub(crate)` once all callers migrate to
  `Connection` methods"* — netd migrated.

**ABI shrinkage from a maximal IPC consolidation: 2 syscalls of 80 dispatch arms
(2.5%), and 0 Rights bits.** The retired-number comments already in
`toyos-abi/src/syscall.rs` (lines 29-41, 75-81, 105-114) are the record of a
consolidation this subsystem has already been through.

### 1.4 Filesystems

**The `bcachefs/` crate does not implement bcachefs.** `bcachefs/src/lib.rs:1-5`:
*"an interim ToyOS-designed format sharing nothing with bcachefs but the
ambition."* It is a **flat, hash-keyed key-value store** — full path siphashed
into one key. **It has no directories at all.**

Measured (`git ls-files … | xargs wc -l`): bcachefs crate **4,039** (of which
1,166 is `tests/integration.rs`) + adapter **544**. FAT32 crates **7,540** +
adapter **1,003**.

**Defect history.** bcachefs's 15 exclusive commits contain 11 correctness fixes
of the worst class: a rename that reported success and lost the file; a kernel
panic from ordinary userland writes; the kernel **auto-formatting the T14's
NVMe**; cross-process information disclosure on `/home`. FAT32's are dominated by
*building* it, plus one hardening wave whose six findings were caught by the
crate's own gate **before the adapter shipped**.

**FAT32 has an outside judge and the ToyOS format cannot have one.**
`toyos-fat32-check/` (66 tests, derived from Microsoft's fatgen103) is wired into
`src/image.rs:319-331`, so every image is asserted format-clean before it ships.
The `bcachefs/` crate's 71 tests are our writer read by our reader. There is no
second implementation of a format we invented — **which is precisely what the
owner's ruling below changes.**

**What `/home` actually holds** (`rg -n '/home'`, 205 hits, every shipped
writer): a keyboard-layout file, shell history, two SSH key files, and a default
cwd. Editor and paint have **no path literals at all**. Everything else is test
scratch. Feature demand: no permissions (nothing anywhere sets a mode), no
symlinks on `/home`, no files >4 GiB (largest test is 4 MiB), no hardlinks, no
sparse files. On real hardware `/home` is *already a tmpfs* — no metal boot has
ever exercised the write path.

**The kernel cannot boot without parsing the format.**

```
kernel/src/main.rs:591:  let initrd_fs = bcachefs_adapter::mount_initrd(initrd_base, initrd.len());
kernel/src/bcachefs_adapter.rs:543:  .expect("Failed to mount bcachefs initrd")
```

`system.toml` `[symlinks]` has 19 `/bin/*` → `/bin/toybox` links, load-bearing
for the **capability model**: `userland/init/src/main.rs:422-440` follows exactly
one symlink to find an applet's authority row. FAT32 has no representation and
refuses to invent one on purpose. Shipping 19 copies of `bin/toybox` costs
≈ +41 MB. `specs/plans/boot-image-split.md` stage 2 ("delete the initrd") is
**not done** — `src/image.rs` still creates only two GPT partitions.

Adjacent storage duplication, measured: `page_cache.rs`'s 320-line CLOCK sweep
has exactly one consumer (`rg -n 'page_cache::' kernel/src/` → 10 hits, 5 of them
in `bcachefs_adapter.rs`; file data bypasses it by design). `file_cache` and
`page_cache` do **not** duplicate each other — they cache disjoint content — but
they duplicate the *policy* verbatim: the comment *"One line per full turnover of
the cache"* appears at `page_cache.rs:169` and `file_cache.rs:456`
byte-identically, beside two CLOCK sweeps of 81 and 71 lines.

**Build cost is not an argument in any direction.** Cold builds: bcachefs 0.44 s,
toyos-fat32 0.29 s, toyos-fat32-check 0.36 s.

### 1.5 Rendering and text output — already consolidated

**Measurement note:** raw `wc -l` overstates this tree badly — it documents at
the site. `panic_console/mod.rs` is **1,344 total / 716 code**; 47% is prose.

- **One font, one source** (`assets/JetBrainsMono-Regular.ttf`), two
  rasterizations for a stated reason: the kernel's is 1bpp because its renderer
  must run with no allocator and no lock.
- **One live scroll implementation** (the terminal's cell-model scroll).
  `rg -n 'scroll_up|scroll_down' userland toyos tests toyos-desktop` returns
  **only the two definitions, zero call sites** — 36 dead code lines in
  `userland/window/src/framebuffer.rs:376-407`.
- **Three damage implementations at three granularities** — genuinely different
  problems (a compositor plans blits, a client names one box, a terminal picks
  cells). Not a candidate.
- **Real duplicates:** pixel-format encode/decode written 5×, alpha blend 3×.
  `paint`'s pair is a byte-for-byte reimplementation of `window`'s while `paint`
  already depends on `window`. ~50 code lines — **but no test asserts pixel-format
  correctness; the whole suite decodes a thresholded screendump that would survive
  an R/B swap.**

**The kernel panel's irreducible core is ~530 code lines** and it earns every one:
seqlock arm/validate/publish that runs before `mm::init`, records→text with no
allocator, pagination, an i8042-polled pager that works with every CPU halted.
~128 code lines are separable (the Ctrl+Alt+D report-hold cluster,
`boot_checkpoint`, the handover-race waiter) — **see the owner ruling below.**

What exists instead of a console→compositor handoff (task #89 is pending) is
**three boot images** (`bootable.img`, `bootable-diag.img`,
`bootable-console.img`) — the real N-implementations finding for this concept.
`issues/hardware/kernel-log-unreadable-once-userland-owns-the-screen.md`
states the trade.

### 1.6 Spawn and lifecycle — the scheduler half is already one

```
$ rg -n 'enqueue_new' kernel/  →  exactly 3 call sites
  loader/mod.rs:686 (process+main thread) | process.rs:1077 (user thread) | sched/kthread.rs:232 (kernel thread)
```

All three go through **one** `alloc_kernel_stack`, differing only in the
trampoline. `kthread.rs:8-13` says it: *"A kernel thread is not a special kind of
task."* What is duplicated is the *bookkeeping bracket* — the same seven
statements in the same order, three times (~60 deletable code lines), and
`kthread.rs:213-214` even says *"exactly as `loader::spawn` does it and for the
same reason."*

Other measured duplication: TLS block setup written twice (~30 lines, and the
fork has already produced two defects — `process.rs:1034` open-codes `!0u64`
where `loader/tls.rs:21` defines `DTV_UNALLOCATED`, and `process.rs:1012` holds a
`ProcessData` lock across `vma_map` for no reader); `release_process` vs
`kill_process` differ in 2 of 13 lines across a 28-line stretch.

**The main-thread-exit special case is 8 code lines and is currently
unreachable.** `kernel/src/process.rs:1334-1342`. Every `SYS_THREAD_EXIT` issuer
in the tree — `rust/library/std/src/sys/thread/toyos.rs:63` and
`userland/libc/src/pthread.rs:70` — sits inside a *spawned*-thread trampoline. A
main thread returns into `rt` and exits via `SYS_EXIT`. **Nothing in the shipped
SDK, std or libc can reach that branch**, and no gate asserts it. Removing it is
not free: the survivor must gain a last-thread-exit teardown trigger, which is
more code than the 8 lines removed.

### 1.7 Caches, allocators, copy, timers, limits, refusals, parsers

**Copy-in/copy-out is the model the rest of the tree should be measured
against.** `rg -n 'copy_from_user|copy_to_user|UserPtr' kernel/src` → **0 hits**.
All 49 user-memory sites go through `SyscallContext`, all in
`kernel/src/arch/syscall.rs`; `rg` for those methods outside that file →
**empty**. **Zero syscall arms do their own thing.** Nothing to consolidate.

**Physical page allocators: exactly one** (`mm/pmm.rs:192`, one bitmap). Three
*wrappers* of `Vec<PhysPage>` — `PageAlloc` (31), `DmaPool` (24, and it
`.expect()`s where the others return `Option`), `shm::Pages` (14) — ~38
deletable, and the survivor should be the `KernelSlice`-returning one, because
`PageAlloc::ptr()` hands an unbounded raw pointer straight to
`copy_nonoverlapping`.

**`SO_CACHE` (`elf/cache.rs:162`) is a cache with no eviction and no bound** — a
`Lock<Vec<..>>` that only grows. Every other cache got a budget in task #28; this
one did not.

**The calendar is written five times, and the kernel's copy is byte-identical to
the pure crate's:**

```
$ git show HEAD:kernel/src/clock.rs      | sed -n '245,282p' > k.txt
$ git show HEAD:toyos-wallclock/src/lib.rs | sed -n '193,230p' > w.txt
$ diff -u k.txt w.txt && echo IDENTICAL   →  IDENTICAL
$ grep -c 'toyos-wallclock' kernel/Cargo.toml   →  0
```

`toyos-wallclock` is **not a kernel dependency** — the crate whose header says
*"Before this crate there was one implementation in `kernel/src/clock.rs` that
userland could not reach"* was added **beside** the kernel's copy rather than
replacing it. Deleting the kernel's 99 lines and `toyos-fat32`'s 29 = **128
LOC**, replacing a zero-test copy with a 14-host-test one. **Highest-value
low-risk item in the audit.**

**Two ELF decoders exist beyond `toyos-elf`, both on crates.io `elf`:**
`bootloader/src/main.rs:244-355` (112 LOC on `elf` 0.7.4) and
`kernel/src/symbols.rs` (68 LOC on `elf` 0.8, used at exactly two lines of the
whole kernel). Landing both retires crates.io `elf` from the tree entirely. The
kernel's own `kernel/src/elf/` is **not** a duplicate —
`rg -n 'from_le_bytes|0x7f|ELFMAG' kernel/src/elf/*.rs` → **zero** raw-decode
hits; everything goes through `toyos_elf`.

**Not duplicates, checked and cleared:** GPT (`kernel/src/gpt.rs` contains no
byte-level parse), PCI (`toyos-pci` is pure bit math, the kernel does ECAM MMIO),
CRC32 (`toyos-gpt` is ISO-HDLC `0xEDB88320`, the ToyOS format crate is Castagnoli
`0x82F63B78` — and `toyos-gpt/src/crc32.rs:77-80 not_castagnoli` is a *negative
gate* asserting exactly that; merging would be a correctness defect).

**Refusal doctrine: one rule, one implementation, four bypasses.**
`rg -n 'handle_fault' kernel/src` returns exactly **two** lines — the definition
and its one call from `handle.rs:57-69`. There is no second way to end a process
for a handle bug. Of 32 handle-access sites, 28 reach it; four fold
`BadHandle`/`Stale`/rights-denied into one silent arm. One is a live defect
(`issues/kernel/a-poll-on-a-refused-handle-waits-forever.md`).

**Limit idioms: eleven, and the plurality is correct.** At the trust boundary
there are exactly two (`InvalidArgument`, `ResourceExhausted`), which is what
`specs/capability-endowment-spec.md` §5 codifies. The other nine live below a
trust boundary and each is argued at its site. **Nothing here should be
absorbed.**

---

## 2. Synergy

### 2.1 Convergence: does one-ring meet one-blocking?

**The hypothesis is half right, and the half that fails is documented in the
code.**

**What supports it.** Every type-B ring carries a wake, and there are **four
different wake mechanisms for thirteen rings**: `KWaitQueue` park (pipe,
io_uring, port), `AtomicBool` + `wake_direct` (log shard), **a pipe byte as a
doorbell** (audio slot ring, soundd's command ring), and drained at scheduler
entry with no wait at all (i8042, irq_ring). That is the real
N-implementations-of-one-concept.

The strongest single piece of evidence: **soundd builds "ring + a way to wait" by
hand, in userland, twice.** `userland/soundd/src/main.rs:1751-1756` pairs
`CommandRing::try_push` with a 1-byte pipe write; `:897-918` pairs the audio slot
ring with another. Two rings whose doorbell is a third ring, because the SDK
offers no primitive for it.

**What resists it, and this is decisive.** `kernel/src/io_uring.rs:163-171` and
`:816-818`: `Source::Log` is **edge-triggered by necessity**, and `is_ready()`
answers `false` unconditionally:

> *"Readiness here means 'records have moved', never 'there is something for
> you': the kernel holds no reader's cursor, so it cannot answer the second at
> all."*

The completion architecture leans on readiness being **level** — *"a CQE is
level-readable state that persists until consumed."* A unified inbox must keep a
source whose readiness is an edge. And `iouring-blocking-spec.md`'s `Source` enum
has **no `Log` variant at all** — it predates the log rewrite that just landed.
**The spec is stale on exactly the ring that most tests the claim.**

Second resistance: **the log producer cannot call `post()`.** `flush()` takes
`CORE.lock()`, and `emit` runs inside `sync.rs`, inside IRQ handlers, inside the
scheduler, and inside every syscall's locked region — which is precisely why
`post_readiness` lives in klogd's loop and not in `emit`. Third: **the 350 ms.**
`kernel/src/log/shard.rs:445-452` records that one locked RMW per log line cost
350 ms of boot under TCG; the log's wake is a bare `AtomicBool` for that reason,
and a unified `post()` must stay RMW-free on the producer path.

**Answer: they are adjacent consolidations, not one.** "One way to wait" is large
(~520 LOC, 8 stores, 11 dual-calls, 4 wake mechanisms → 1), already specified,
and worth doing. "One ring type" is three small merges (~180 net LOC) plus a
shared trait. **Forcing them together drags the log shard's 508 lines and its 12
loom models — the only machine-checked ordering proofs in the tree — into a
refactor they gain nothing from.** Do the wake consolidation; do the ring merges
separately and after; do not build a universal ring.

### 2.2 The enablement graph

- **Pipeline 2 is the hub**, and it is now unblocked (log landed). It is the sole
  prerequisite for: the `SYS_PIPE_MAP`/syscall-77 retirement (netd needs io_uring
  readiness on its data pipes first); the keyboard/mouse queue merge (each
  currently costs a watcher list, a `waitqs` static, a `Source` variant and an
  `ops` arm — after pipeline 2 those all vanish and the merge becomes ~30 lines of
  pure win); and the four Class-D device spins becoming parks.
- **Pipeline 2 makes the ring merges *cheaper*, not possible.** After it, merging
  two rings no longer means merging two readiness plumbings.
- **Move 3 (symbols → `toyos-elf`) is fully independent** and lands today. It also
  retires crates.io `elf` 0.8 from Ring 0 — a bonus the roadmap does not claim.
- **Move 1 (userland loader) does *not* need pipeline 2** —
  `rg -n 'io_uring' kernel/src/loader/ kernel/src/elf/` → **0 hits**. But it needs
  two prerequisites the roadmap does not name: a **file-backed `mmap`** (today
  `MmapFlags` is `ANONYMOUS|FIXED` only) and **NX + an execute bit** (`MmapProt`
  is `NONE/READ/WRITE`; `PageTables::remap` has no execute parameter;
  `rg -n 'NO_EXECUTE|NXE' kernel/src/mm/paging.rs` → empty). Without those, a
  Ring-3 loader cannot even *ask* for W^X.
- **Move 2 (FS daemons) is queued behind pipeline 2 for the weakest of its three
  blockers.** The plan's own words are *"needs the blocking story **to be
  efficient**"* — a performance dependency. The two real ones are unnamed: there
  is **no block device class in the ABI** (`device_classes!` is
  Keyboard/Mouse/Framebuffer/Nic), and the kernel's root filesystem is the ToyOS
  format over the initrd.
- **The rendering candidate is blocked on task #89**, not on pipeline 2.

### 2.3 Second-order effects

- **Gate concentration.** The keyboard/mouse merge would give the drop-oldest
  bound its *first* test — `rg -n 'MAX_QUEUED_EVENTS' tests/` → 0 hits today, in
  both copies. Same for the three-ring SPSC merge: `rg -ni 'spill' tests/ src/` →
  0. **These merges add gates rather than needing them.**
- **ABI shrinkage is small and should not motivate anything.** Maximal IPC
  consolidation retires **2 syscalls of 80**; the vestige sweep retires 2 more
  (`SYS_STACK_INFO`, `SYS_SHM_UNMAP`). `src/sourcegate.rs:120-128`'s
  `RETIRED_REGISTRY` mechanizes the never-reused rule and must gain each name.
- **Wake-path simplification is the biggest structural number: 50 distinct wake
  call sites → the completion spec puts the post-consolidation figure at one
  `post`.**
- **Specs that die.** `specs/plans/console-records.md` (290 lines) — its central
  recommendation lost; the shipped design is per-CPU, and `rg CONSOLE_RING kernel/src/`
  → empty. `specs/plans/toolchain-wave-plan.md` (879) — T1–T7 all verified landed.
  **1,169 lines of plan describing superseded or completed work.** Not deleted in
  this PR by instruction; it belongs to the Wave A execution.
- **ARM64 surface.** The four Class-D device spins and the three duplicate
  `settles()` are each arch-coupled timing code; collapsing them to one bounded
  `Poll` reduces what an ARM64 port must re-derive. **SPECULATIVE** — arch-coupling
  per mechanism was not measured.

---

## 3. Ranked: deleted LOC per unit risk

### Wave A — independent of pipeline 2, low risk

| # | candidate | LOC | risk | gate | conf |
|---|---|---|---|---|---|
| A1 | Kernel calendar + `toyos-fat32`'s copy → `toyos-wallclock` (byte-identical, `diff` shown) | **128** | LOW | 14 host tests survive; kernel copy has **0** | CONFIRMED |
| A2 | Move 3: `symbols.rs` + `loader/symbols.rs` → `toyos-elf`; retires crates.io `elf` 0.8 from Ring 0 | **68** (of 517 touched) | MED (panic path, unmeasured bounds-check cost) | 5 panic gates; **no host tests exist in either file** | CONFIRMED |
| A3 | Delete `specs/plans/console-records.md` + `toolchain-wave-plan.md` | **1,169** (docs) | ZERO | n/a | CONFIRMED |
| A4 | Delete `kernel/src/main.rs:3` `#![allow(dead_code)]` + 4 item allows → 94 items surface, 59 dead in *every* build | — | ZERO | the lint is the proof; `-Dwarnings` is the gate it defeats | CONFIRMED |
| A5 | `arch/debug.rs`: 78 of 105 lines dead; the 2 survivors log a constant | **78** | ZERO | none | CONFIRMED |
| A6 | Bootloader ELF loader → `toyos-elf`; retires `elf` 0.7.4 | **112** | LOW-MED | every boot | CONFIRMED |
| A7 | soundd `MsgBuf` → `ipc::FrameRx` (and it is the least-gated framing code in the tree) | **57** | LOW | liveness gates only | CONFIRMED |
| A8 | `paging.rs` 4 dead methods + `pmm::Category::PageTable` (its sole constructor) | **57** | LOW | none | CONFIRMED |
| A9 | Vestige sweep: `SYS_STACK_INFO`, `SYS_SHM_UNMAP`, `CENSUS_BREAKDOWN`, `CENSUS_TOTAL`, `IORING_OP_POLL_REMOVE`, 9 dead SDK items, `toyos/src/system.rs`, 6 caller-less `ipc` free fns, `window` scroll/clipboard/cursor | **~330** | ZERO–LOW | none for any | CONFIRMED |
| A10 | virtio-gpu display-info path never issued; 3 duplicate `settles()`; HDA accumulator → `Spill`; `IdKey` 3 unused impls; `KObjectVariant::into_ref` (9 monomorphisations, 0 calls) | **~130** | ZERO | none | CONFIRMED |
| A11 | Dead pixel-scroll path (`window::Framebuffer::scroll_up/down`, zero call sites verified) | **36** | MINIMAL | n/a | CONFIRMED |

**Wave A total: ~1,000 production LOC + 1,169 doc LOC, essentially all at
zero-to-low risk.**

### Wave B — pipeline 2, the flagship

| | |
|---|---|
| deletes | **~520 LOC** of dual-notification bookkeeping + ~70 converted; 8 watcher stores → 1; 11 dual-calls → 1 `post`; 4 wake mechanisms → 1; 4 unbounded device spins get deadlines |
| costs | cancellation rewrite inside `toyos-sched` (the crate whose migration cost 70 defects); a `Parkable` token; `SleepLock` (~150 LOC, does not exist); a six-kind duration taxonomy; two kernel threads |
| blocked on | nothing — log landed |
| risk | **HIGH**, concentrated in cancellation |
| first action | **re-derive the inventory.** The spec's own B1 table was wrong once (it omitted keyboard and mouse), and 0 of its 12 cited line numbers still resolve. |

### Wave C — open

| | question | size |
|---|---|---|
| C1 | `hda_probe.rs` — 1,003 LOC behind one diagnostic actuator, scheduled for deletion at H9; task #88 in flight | 1,003 |
| C4 | Move 2 sequencing: its stated blocker (completions) is the weakest of three | — |

*(C2 and C3 were answered by the owner on 2026-08-15; see below.)*

---

## 4. Owner rulings, 2026-08-15

Two questions this audit raised were answered by the owner while it was being
recorded. Both replace open options above; neither is to be re-proposed.

### C2 — bcachefs stays, and becomes real

> *"bcachefs is the default filesystem for toyos. the crate must be an
> implementation of the spec."*

Both options this audit weighed — delete the crate in favour of FAT32, or rename
it to stop it claiming a format it does not implement — **are overruled.** The
resolution of the misnamed crate is the opposite of a retreat: `bcachefs/`
becomes a real implementation of the bcachefs on-disk format, and that format is
ToyOS's default filesystem.

This changes the reading of §1.4 above. The audit's finding that the crate shares
nothing with upstream but the name stands as *measurement*; its framing of that
as an argument for deletion does not. In particular, the observation that the
ToyOS format "cannot have an outside judge, because there is no second
implementation of a format we invented" inverts under this ruling: a real
bcachefs implementation has upstream itself as the outside judge, which is the
strongest possible answer to the defect history recorded in §1.4.

`issues/kernel/bcachefs-crate-is-not-bcachefs.md` carried this question at
`status: owner` since 2026-08-01. This is its answer; the entry is updated in the
same commit as this document and is now work rather than a question.

The two prerequisites §1.4 measured remain facts about the tree whichever format
wins, and a real-bcachefs track inherits them: the kernel must parse the root
format to reach `/bin/init` (`kernel/src/main.rs:591`), and
`specs/plans/boot-image-split.md` stage 2 is not done.

### C3 — Ctrl+Alt+D stays in the kernel

**Settled: the frozen-machine diagnostic outweighs the 128-line deletion, and
nobody re-proposes it.**

The audit measured ~128 separable code lines in
`kernel/src/drivers/panic_console/mod.rs` (the report-hold cluster,
`boot_checkpoint`, the handover-race waiter) and noted the trade stated in the
code at `mod.rs:795-812`: the caller is *"Ctrl+Alt+D, pressed on a machine its
owner believes has stopped"*, and a userland reporter is exactly the thing that
may be wedged. The owner has ruled that trade decided. The ~530-line irreducible
core and the ~128 separable lines both stay in the kernel.

The module-header sentence recording this ruling at the site will be placed by a
later wave, not by this assessment.

---

## 5. Defects filed

Nine items were found in passing and are filed as their own issues rather than
fixed here. Each was re-verified against `71a0559` before filing.

| finding | issue |
|---|---|
| **CRITICAL** — a `MAP_FIXED` mapping is invisible to `find_gap`, so the next anonymous `mmap` can panic the kernel from userland | `issues/kernel/map-fixed-is-invisible-to-the-va-allocator.md` |
| `toyos-cc` honours designated array indices for globals and silently ignores them for locals | `issues/build/toyos-cc-drops-local-array-designators.md` |
| `process_poll_add` folds a refused handle into a silent arm and pushes a poll nothing can complete | `issues/kernel/a-poll-on-a-refused-handle-waits-forever.md` |
| `thread_exit` `.unwrap()`s a table entry its own neighbour documents as raceable | `issues/kernel/main-thread-exit-unwraps-a-reaped-entry.md` |
| std's `SystemTime::now` returns the epoch while libc reads the real clock | `issues/build/std-systemtime-now-returns-the-epoch.md` |
| `MAX_CPUS` is defined three times and nothing pins the copies together | `issues/kernel/max-cpus-is-defined-three-times.md` |
| `arch/tlb.rs`'s spin constant is defended by a false claim about the clock | `issues/design-debt/tlb-spin-comment-names-a-clock-that-is-not-read.md` |
| `kthread.rs` states a 16 KiB kernel stack; it is 128 KiB | `issues/design-debt/kthread-comment-states-the-wrong-stack-size.md` |
| `SYS_PROCESS_OPEN`'s latent panic — **not filed; the entry exists** | `issues/kernel/process-open-panics-on-a-reopened-process.md` |

On the last: that issue already records both the panic and the fact that
`SysCap::open_process` has no caller. This audit adds one disposition option it
does not carry — **deleting the syscall path closes the defect for free, but
costs `a_pid_is_not_authority` (`tests/toyos-rust-tests/src/bin/process_lifecycle.rs:250-262`),
the estate's only assertion that a pid is not authority.** If that route is taken,
the assertion moves to a syscall that still exists; it is not lost.

---

## 6. Proposed rules (placement pending)

Three rules with no obvious home, recorded here for the orchestrator to place or
decline. **None of them is placed by this document.** The first is withdrawn by
the author and is kept only so it is not re-derived.

1. ~~*A spec that cites a file path is reded when the path stops existing.*~~
   **Withdrawn by the author before placement.** It was drafted on a sub-auditor's
   claim that the mirror-image gate already existed in `src/docs.rs`; checking that
   claim while writing this document showed the opposite. `src/docs.rs` was
   deleted whole on 2026-08-14 by `8d0db10` — *"owner ruling, no tests over
   documentation"* — taking `every_named_issue_file_resolves` with it (§0).

   So this is not a tension to weigh: the rule proposes re-adding, in a wider
   form, a gate family the owner removed the day before, and it should be
   declined. The §0 problem is real and wants the answer that needs no gate —
   a superseded plan is deleted rather than left to rot, which is already
   CLAUDE.md's *"a plan dies on completion"* and is Wave A item A3.

   Recorded rather than silently dropped because the *next* audit will find the
   same asymmetry and draft the same rule; this paragraph is what stops it.

2. *The kernel and the bootloader decode ELF only through `toyos-elf`; a
   crates.io ELF crate in either tree is a second decoder and is refused.* Both
   extra decoders were added as one `use` line each and the tree compiled, so the
   violation is invisible.

3. *A new ring names its overflow policy in its type or module header, and ships
   the test that reaches it.* Three of thirteen do neither, and their full-ring
   paths have never run.

---

## 7. One constraint on a later wave

No gate was weakened or proposed for weakening anywhere in this audit. One hard
constraint runs the other way and must be honoured by whoever executes Wave A: if
the eight "spin until N ns" copies are consolidated, the survivor **must** be the
`rdtsc`/`tsc_deadline` form (`kernel/src/sched/dump.rs:356`), because
`src/redlist.rs:672-676` retired a `dump_nmi_probe` red specifically on the
strength of that change. Consolidating onto the `nanos_since_boot` form re-opens
a retired red.
