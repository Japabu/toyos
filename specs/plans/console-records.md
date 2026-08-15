# Console Records — Spec

> 2026-07-28. Four investigations, three competing ring topologies, one synthesis.
> Linux's printk_ringbuffer was read from source, not recalled. Every ToyOS claim
> below was verified against the tree.

> **Status: design accepted, implementation deferred.** See §7 — the case against
> doing this *now* is strong and is about sequencing, not about the design.

## 1. Recommendation

TOPOLOGY: ONE global lockless record ring for the console, plus drain-side line assembly. Take the topology and record format from GLOBAL, the "share the type, never the instance" ruling from CONVERGENCE, and the writer-domain insight from PERSOURCE — but relocate that insight from the write path to the drain, which none of the three proposed.

WHY GLOBAL AND NOT PER-CPU. I verified the clock argument and it holds decisively. `kernel/src/clock.rs:41-42` latches a single global `TSC_BOOT`; `clock::init` has exactly one caller, `kernel/src/main.rs:257` (BSP). Grepping `kernel/` and `toyos-abi/` for `rdtsc|TSC_BOOT|invariant|0x80000007|TSC_ADJUST` returns only `arch/cpu.rs:23-27` (a bare non-serializing `rdtsc` with `options(nomem, nostack)`) and unrelated uses of the English word "invariant". There is no invariant-TSC check, no per-CPU offset, no AP calibration. A per-CPU topology would order the console by that clock. On QEMU/TCG it would look perfect and fail only on the 2020+ hardware ToyOS targets — the worst possible failure shape. PERCPU itself concedes this is "new work this topology forces" and names it the thing that would falsify its stance. One `lock xadd` gives a hardware total order for free.

WHY NOT PER-SOURCE. PERSOURCE's own cost (1) is disqualifying and it half-admits it: `kernel/src/mm/paging.rs` maps at 2 MiB granularity only, so a mapped per-process ring is 2 MiB per process — ~32 MiB for a normal boot against today's 64 KiB, on a project whose principles say "never hog resources without purpose". Its lifetime story (a slot must outlive the process, freed only when drained) adds a step to `teardown_bookkeeping` and a window where a dead process holds 2 MiB. Its central attribution claim is also weaker than advertised: making pid structural trades a tag that can be truncated for a slot that can be *confidently wrong* across a recycle, which is worse.

THE RECORD FORMAT. Fixed 32-byte slots; a record is one header slot plus ceil(len/32) payload slots, claimed with a single `HEAD.fetch_add(n)`.

  #[repr(C)] struct Slot {
      seq:    AtomicU64, // +0   commit stamp == this record's start sequence
      ts_ns:  u64,       // +8
      pid:    u32,       // +16  u32::MAX = kernel
      tid:    u32,       // +20
      src:    u8,        // +24  0=Kernel 1=User 2=Panic 3=Pad
      cpu:    u8,        // +25
      nslots: u8,        // +26
      flags:  u8,        // +27  bit0 NL_TERM, bit1 CONT
      len:    u16,       // +28  post-ANSI-strip byte count
      _rsvd:  u16,       // +30
  }
  const _: () = assert!(size_of::<Slot>() == 32);

The seq-stamp is the whole validity protocol: because `slot_index = seq & MASK`, a reader at sequence S checks `slot[S & MASK].seq == S`. That one field delivers what Linux buys with a descriptor array cross-indexed against a data ring plus a five-state `state_var` machine. Commit is one `Release` store of `seq` after the payload memcpy. There is no reserved/committed/finalized machine, no ABA analysis (64-bit only), no descriptor/data split. I explicitly reject the two-ring printk shape that PRINTK's dossier recommends: its descriptor array exists because Linux claims *byte*-granular spans, and a fixed 32-byte stride collapses that entire problem.

I reject STRUCTURED's 0x1E sentinel. It requires mutating user payload at a chokepoint that is conventional, not structural — PERCPU's own failure-mode 3 identifies this as its load-bearing weakness ("nothing in the type system prevents a second path into the ring"). The seq-stamp needs no payload constraint at all: an aligned candidate whose `seq & MASK` equals its own index AND whose `nslots`, `len`, `src`, `_rsvd` are all in range cannot be forged by ASCII except astronomically rarely.

THE FRAGMENTATION FIX — DRAIN-SIDE LINE ASSEMBLY. This is the part I am changing relative to all three designs, and it is forced by a fact I verified that overturns two of them (see designers_missed): the std pal `write_fmt` override that STRUCTURED and GLOBAL both make load-bearing does not work. So framing must solve fragmentation itself.

The drain — already single-threaded under `BackendGuard` (serial.rs:42-64), already the sole writer to the backend, already rendering records to text — keeps a small fixed static array of assembly slots keyed by (src, pid, tid). A record without NL_TERM is appended to its slot instead of emitted. A record with NL_TERM completes the line and emits it whole. Slots flush on age (the drain knows `nanos_since_boot`), on slot pressure (evict oldest, emit with CONT), and unconditionally on the panic path.

This is strictly better than every write-side proposal: no per-process state, no lifetime problem, no 2 MiB, no PROCESS_TABLE dependency, no synchronization whatsoever (one thread holds the backend), and — critically — it is topology-independent, so it does not depend on winning the global-vs-per-CPU argument. soundd's 17 syscalls become 17 records and exactly one wire line, deterministically, with no change to the Rust fork and no cooperation from userspace.

THE WIRE. Text, not binary — `src/qemu.rs:106-112` leaves QEMU's stdout on the terminal with no decoder, and "development ergonomics above all" disqualifies breaking `cat`. Every record renders with a uniform prefix:
  [k 1.042 cpu0 tid=3] xhci: reset complete
  [u 1.043 cpu2 pid=7 tid=9] soundd: wakes=431 completions=430 ...
  [! 1.044 cpu0] 37 records lost (seq 1204..1241)
The load-bearing change is that userspace records get a prefix too. Today they get nothing, which is precisely why `is_kernel_line` (qemu.rs:57-61) has to guess.

THE PRECISE GUARANTEE, stated honestly: **every line on the wire contains bytes from exactly one source.** Not "every line is complete" — that is unachievable, and no designer noticed why. `userland/shell/src/main.rs:35-36` does `print!("{}> ", cwd); io::stdout().flush().ok();` — an unterminated prompt that a human must see promptly. So assembly must age out, and a line flushed before its newline carries an explicit CONT flag so the reader can tell "line ended" from "line continues". One timer tick (100 Hz, timer.rs:120) is invisible to a human and bounded.

## 2. How the hard constraints are met

(1) log! UNDER ARBITRARY KERNEL LOCKS, IRQ CONTEXT, IF=0, NO ALLOC, NO I/O — met, and strictly better than today.

Write path: `pushfq`/`cli` → one `lock xadd` → memcpy into a privately-owned span → one `Release` store → `popfq`. No lock is acquired, so it cannot invert against any kernel lock, cannot self-deadlock on same-CPU IRQ re-entry, and cannot be stranded by a CPU that dies mid-write.

Today's path is worse on exactly the constraint that shaped the module. `RingGuard::lock` (log_ring.rs:84-104) does `pushfq`/`cli` then an *unbounded* `compare_exchange_weak` spin (lines 95-102), held across `append`'s per-byte loop (42-55, ~80 iterations of two modulos and a branch for a typical line). A CPU that dies holding `RING_LOCKED` wedges `log!` on every other CPU permanently. That is CLAUDE.md's "locks a dead thread can strand" class, sitting on the panic path.

Re-entrancy safety is not new engineering — `trace.rs:117` already allocates slots with `head.fetch_add(1)` from IRQ context and `trace.rs:122-123` documents why it is sound. I generalize it from n=1 to n slots. The IRQ case is real, not hypothetical: `arch/idt/timer.rs`'s tick is an IF=0 interrupt gate that drains the log ring under `RingGuard::lock`, with 67 `log!` sites in exceptions.rs and 273 kernel-wide (both counts verified by grep).

I keep the `cli` across reserve→commit. Not for exclusion — for Linux's stated reason (printk_ringbuffer.c:1669-1675): to bound how long an uncommitted slot is a hole in the sequence. This is `pushfq`/`cli`/`popfq` with no lock and no spin, against today's `pushfq`/`cli`/contended-spin/per-byte-loop/`popfq`.

(2) HAND-DECODABLE FROM A RAW DUMP — met. This constraint is harder than the brief states and I verified both reasons.

`readelf -S kernel/target/x86_64-unknown-none/debug/kernel` returns exactly 9 sections: .text, .data, .rela.dyn, .dynamic, .eh_frame_hdr, .symtab, .strtab, .shstrtab. **There are no `.debug_*` sections at all.** LLDB has zero type information; `memory read` is the only route.

And the current ring is the harder of the two statics to find. `nm -S` gives:
  0000000000290780 00000000000c0200 D TRACE_RINGS
  000000000027f210 0000000000010018 d _RNvNtNtCsfTd9MF1GDC5_6kernel7drivers8log_ring4RING
Lowercase `d` — `RING` is a *local* symbol, and `CsfTd9MF1GDC5_` is a crate-metadata hash that changes between builds. `#[no_mangle]` is therefore a hard prerequisite, not a nicety.

THE DECODE, COMPLETE, NO LIVE CODE:

  (lldb) image lookup -s CONSOLE_RING            -> S
  (lldb) memory read --size 8 --count 1 S        -> head
  (lldb) memory read --size 8 --count 1 S+64     -> tail        (own cache line)
  (lldb) memory read --size 8 --count 2 S+128    -> mask, stride(=32)
  base = S + 192
  for seq in tail..head:
      a = base + (seq & mask) * stride
      if u64 @ a != seq:  gap or in-flight — resync; continue
      nslots = u8  @ a+26
      len    = u16 @ a+28
      print len bytes @ a+32        # contiguous, guaranteed
      seq += nslots

Six lines of arithmetic. Shorter than Linux's 30-line raw gdb macro (Documentation/admin-guide/kdump/gdbmacros.txt:289-320) because the fixed stride replaces the whole descriptor lookup.

Four properties make it hold under real corruption:
 - GEOMETRY LIVES IN THE DUMP. `mask` and `stride` are struct fields, so a decoder never needs the build's constants or a version match. Linux needs VMCOREINFO (printk.c:984-1036) for exactly this; here it is 16 bytes of static.
 - BLIND RESYNC WITH NO STATE. Headers are 32-byte aligned. Scan aligned u64s for a value v with `(v & mask) == (offset-192)/32`, then check `nslots ∈ 1..=129`, `len ≤ (nslots-1)*32`, `src ≤ 3`, `_rsvd == 0`. The panic drain and the human debugger use the identical rule, so there is one procedure to get right and it is exercised on every hard panic.
 - PAYLOAD NEVER WRAP-SPLITS (the Pad rule), so `memory read --format s` at a fixed +32 always works.
 - NO INDIRECTION. `len` and payload are in the same record. This is why I refuse the printk descriptor/data split and journald's DataObject hash-table graph — both are decodable only by *walking a structure*, in the situation where you have the least to work with.

A 32-byte slot is exactly two lines of LLDB's 16-byte-per-line output and every header starts on an even line boundary, so the ASCII gutter prints payload text directly.

HONEST COST: today the ring is raw ASCII and `memory read` prints the log verbatim with no procedure at all. This interposes 32 binary bytes per line. That is a real, if small, regression in the pure-eyeball case, offset by every line now carrying timestamp, cpu, pid and tid — which today exist only as *text* on kernel lines and not at all on userspace lines.

(3) THE PANIC PATH — met, and materially simplified. First, a correction the architect needs: **CLAUDE.md's known-issue entry claiming the syscall panic path never drains the ring is STALE.** `kernel/src/main.rs` calls `drivers::serial::panic_flush()` immediately after `crash_report` and before the `percpu::syscall_rip() != 0` recovery branch, with a comment stating exactly that rationale. Two of three designers caught this; the entry should be deleted.

`crash_report` is 67 separate `log!` calls (verified count in exceptions.rs). Today those are 67 acquisitions of a global spinlock by a dying CPU. Under records they are 67 independently-committed, consecutively-sequenced, individually-decodable records — a reader can tell instantly whether the report is complete, and a panic that dies partway leaves a decodable prefix rather than a held lock.

Deleted outright: `drain_unlocked` and its torn-index clamps (log_ring.rs:246-264). It exists solely to bypass `RING_LOCKED`; with no ring lock there is nothing to bypass, and there are no torn indices because HEAD/TAIL are atomics and every record self-validates.

What STAYS, honestly: `BackendGuard` and its spin-then-bypass (serial.rs:176-196). That guard protects the virtio virtqueue and the UART — *device* state, not ring state — and the disable-virtio-then-UART fallback is still needed because a half-submitted `tx_slot` would panic recursively. `PANIC_LOCK_SPIN_LIMIT = 100_000_000` (~1 s) narrows to the backend only. `panic_flush` shrinks by roughly half; it does not vanish. Any design claiming otherwise is overselling.

The panic drain never waits on an in-flight record — it resyncs forward and emits `[! ...] record <seq> abandoned` — and it flushes every assembly slot immediately before returning.

(4) NO ALLOCATION IN THE WRITE PATH — met. `static CONSOLE_RING` plus the caller's existing stack buffer (`SerialWriter`'s `[u8; SW_BUF_SIZE]`, serial.rs:212). `fetch_add`, `copy_nonoverlapping`, field stores, one atomic store. The drain's assembly slots are a static array. This matters beyond hygiene: CLAUDE.md documents that a panic needing the allocator lock its own thread holds produces *no output at all*, so this path must never touch the allocator to be usable in the case it exists for.

Honest: Rust has no lint proving this. Enforcement is "the module imports nothing from `alloc`", grep-checkable in CI but review-enforced, not type-enforced — same as today. Under UNREPRESENTABLE > CHECKED > TESTED that is a demerit no topology fixes.

## 3. What it deletes — honestly

I am going to give you the honest number first, because a fabricated net-negative is worth less than nothing here: **on line count this is roughly a wash, not a win.** It is net-negative only if you also bank the 61 lines of dead trace code, which is available for free today and should not be credited to this work.

VERIFIED DELETIONS (line ranges read, not estimated):

Kernel — kernel/src/drivers/log_ring.rs (264 lines total):
 - `RingGuard` struct + impl + Drop (79-124) and `RING_LOCKED` (72)              ~47
 - `drain_unlocked` + its `# Safety` block and torn-index clamps (246-264)        19
 - `DROPPED_BYTES` (73), `report_dropped` (200-206), `DROP_MARKER_MAX` (208),
   `take_drop_marker` (210-244)                                                  ~44
 - `LogRing::append`'s byte-at-a-time eviction loop (42-55)                        14
 - `LogRing::drain_into` (57-65)                                                    9
 - `LogRing`'s untyped head/tail/len triple (30-35)                                 6

Kernel — kernel/src/drivers/serial.rs:
 - `panic_flush`'s ring-bypass tail (184-195)                                      12
 - The `lossless: bool` two-policy split (214, 227-229, 236-240) collapses          ~8

Kernel — kernel/src/trace.rs (dead TODAY, independent of this work — verified:
`grep -rn "trace::dump\|TraceKind::Mark"` over kernel/, toyos-sched/, tests/, src/
returns nothing):
 - `dump` + `kind_name` (160-220)                                                  61
 - duplicate `pub const MAX_CPUS` (22), shadowing scheduler.rs                      1

Harness — all four workaround sites, which is the bar the owner set:
 - `strip_kernel_logging` + doc, tests/common/audio.rs:310-332                     ~23
 - `stats_field`'s resume-across-newline loop, audio.rs:346-362                    ~17
 - `END_MARKER` const + its 1-in-120 comment, tests/common/qemu.rs:62-68            ~7
 - mid-line marker salvage, qemu.rs:199-209                                        ~11
 - `is_kernel_line` (56-61) + the `line.find("[kernel ")` suffix split (243-249)   ~13

Kernel subtotal ~220 (of which 62 is free dead code). Harness ~71. Total ~291.

ADDED, estimated from the design rather than a written patch — label it as such:
 record ring + reserve/commit/pad/resync ~150; drain, three-case walk and text
 render ~90; drain-side assembly slots ~70. Total ~310.

NET: roughly +19 lines, or about -42 if you bank the dead trace code. Call it a wash.

WHAT IS GENUINELY NET-NEGATIVE IS CONCEPTS, AND THAT IS THE REAL ARGUMENT:
 - Lock protocols in the log path: 2 (`RingGuard` + `BackendGuard`) -> 1 (backend only).
 - Panic bypass paths: 1 -> 0. `drain_unlocked`'s unsafe contract and its torn-index
   clamps stop existing, and with them the class of failure where a dead CPU wedges
   `log!` kernel-wide via the unbounded spin at log_ring.rs:95-102.
 - Loss-accounting mechanisms: a byte counter rendered at an unrelated stream position
   -> a sequence gap discovered at the splice.
 - Reader-side repair functions: 4 -> 0.
 - LLDB decode procedures: today 2 incompatible (log_ring byte soup + trace records),
   heading for 3 once `toyos-sched/src/hw.rs:44`'s TraceEvent lands -> 1.

RING IMPLEMENTATIONS THAT CONVERGE: exactly one, and I want this on the record because
two of the four must NOT converge and anyone proposing 4->1 is proposing a regression.
 - `kernel/src/trace.rs`'s `TraceRing` converges onto the shared TYPE (not instance) —
   see tracing_convergence.
 - `kernel/src/irq_ring.rs` STAYS. Verified by reading it: it is `[[AtomicU64; 3]; MAX_CPUS]`,
   192 bytes, one timestamp slot per (cpu, source), and its module doc states "back-to-back
   IRQs coalesce into one record and overflow has no representation". It is a coalescing
   latch, not a ring, and it already emits *into* the trace ring at irq_ring.rs:81 — the
   correct relationship. Converging it reintroduces an overflow the scheduler spec removed.
 - `kernel/src/audio.rs`'s `RecordRing` STAYS. Verified: audio.rs:20-27 documents "overflow
   is a kernel bookkeeping bug, and the producer panics rather than mutating slots the
   consumer may be reading", with a `const _: () = assert!` tying capacity to
   `TX_INFLIGHT_MAX`. An observability ring must always drop; this one must never. Same
   shape, incompatible semantics.

MEMORY: 64 KiB -> 128 KiB + 192 B (4096 slots x 32 B) + ~16 KiB of assembly slots. At a
typical 80-byte line = 4 slots that is ~1024 lines of history against today's ~800. Framing
overhead is 32 bytes plus up to 31 bytes of slot rounding — ~45% on an 80-byte line. That is
the real price of framing and it is not small.

## 4. Tracing convergence

DECISION: YES — share the TYPE. NO — never share the INSTANCE. Two instances of one generic ring: `CONSOLE_RING` (one global, per the topology above) and `TRACE_RINGS` (per-CPU, as today).

WHY NOT ONE INSTANCE. CLAUDE.md records 1000+ context switches/s on single-core under the Doom demo, and I counted the trace call sites — scheduler.rs (7 sites), arch/idt/timer.rs, arch/apic.rs, arch/idt/mod.rs, plus `trace_irq_drain` from irq_ring.rs:81. At several events per switch that is 5-10k events/s. A shared instance would evict the console's entire history in well under a second, which destroys the property the console ring exists for. CONVERGENCE is right about this and I am adopting its ruling unchanged.

WHY SHARE THE TYPE — AND THE ARGUMENT IS NOT LINE COUNT. Sharing the type saves maybe 60 lines, which does not clear the >2x bar on its own. The argument that does clear it is decode-procedure count under constraint 2. Today a human on a dead machine must know two incompatible formats. `toyos-sched/src/hw.rs:44-65` already commits a THIRD, and I verified it is a `#[derive(Clone, Copy, PartialEq, Eq, Debug)]` Rust enum with payload variants (`Schedule { task }`, `Migrate { task, to }`) carrying **no `#[repr]` and no size assert** — its own doc says it is "the format shared by the kernel's per-CPU binary trace ring (Stage 6)". As written it has unspecified layout and **cannot be hand-decoded from a raw dump at all**, so it fails constraint 2 outright. Doing nothing means three formats, one of them undecodable. Sharing the type means one decode procedure and Stage 6 becomes a `Hw::trace` impl over an existing ring instead of a fifth ring.

MECHANICALLY. A trace event is a record with `nslots == 1` and its payload in the header's spare fields; a log line is a header plus payload slots. `trace.rs`'s existing 24-byte `TraceEvent` widens to the 32-byte slot — it already has `timestamp_ns`, `kind`, `cpu`, `pid`, `tid`, `data` and a `const _: () = assert!(size_of == 24)`, so this is a field-layout change, not a redesign, and `trace.rs:51`'s "Field order chosen so LLDB hexdump is easy to read" discipline carries over verbatim.

WHAT STAYS PER-INSTANCE: (a) loss policy — trace uses plain overwrite (`fetch_add`, clobber, loss derivable from the monotonic head), console uses drop-oldest for `log!` and a CAS-reserve-with-backpressure for userspace, preserving today's lossless-console guarantee at log_ring.rs:145-159; (b) sink — console renders to text at the drain, trace stays LLDB-only until spec §10.4 wants `trace.bin`; (c) the kind enum.

TIMING: do this LAST (my Stage 6), not first. It is the only part that touches `toyos-sched`, and the scheduler migration is mid-flight.

INDEPENDENT OF ALL OF THIS: `trace::dump` and `kind_name` (trace.rs:160-220, 61 lines) and `TraceKind::Mark` (trace.rs:37) are dead today — I verified zero callers across kernel/, toyos-sched/, tests/ and src/. Delete them now, in their own commit, regardless of what the architect decides here. And give `hw.rs::TraceEvent` a `#[repr(C)]` wire form with explicit discriminants before scheduler Stage 6 lands, whatever happens to the console.

## 5. Does userspace still need a one-write-per-line primitive?

NO. Userspace does not need a one-write-per-line primitive, and the reason is not that one would be redundant — it is that **the primitive two of the three designs proposed does not exist and cannot be built under the project's own std rules.** This is the finding that most changes the shape of the answer, so I want it stated precisely with the verification.

STRUCTURED and GLOBAL both make "override `write_fmt` on the pal `Stderr` in rust/library/std/src/sys/stdio/toyos.rs" the second half of a "ship both layers or neither" recommendation. I traced the call chain in the fork and it never reaches the pal:

  eprintln! -> _eprint(args)                              io/stdio.rs:1285
    -> print_to(args, stderr, "stderr")                   io/stdio.rs:1155-1166
      -> global_s().write_fmt(args)   i.e. Stderr::write_fmt
        -> (&*self).write_fmt(args)   impl Write for Stderr
          -> self.lock().write_fmt(args)  impl Write for &Stderr, io/stdio.rs:1064-1066
            -> StderrLock::write_fmt

`impl Write for StderrLock<'_>` (io/stdio.rs:1078-1097) defines `write`, `write_vectored`, `is_write_vectored`, `flush`, `write_all`, `write_all_vectored` — and **no `write_fmt`**. So it falls through to the trait default (io/mod.rs:1972-1978) -> `default_write_fmt(self, args)` where `self` is the `StderrLock`, and the `Adapter` calls `StderrLock::write_all` once per format piece (io/mod.rs:608-618), each reaching `StderrRaw::write_all` -> pal `Stderr::write` -> `syscall::write(STDERR, buf)`.

`StderrRaw::write_fmt` (io/stdio.rs:197-199, `handle_ebadf(self.0.write_fmt(fmt), ...)`) is dead code for `eprintln!` — the fragmentation is decided one level above it, at `StderrLock`, a cross-platform type in a cross-platform file. Fixing it there would change cross-platform semantics and API shape, which CLAUDE.md's std rules forbid outright. CONVERGENCE reached the right conclusion ("not available — it would violate the project's std rules") for a wrong reason (it argued nothing would flush a pal buffer; a `write_fmt` override needs no flush — it simply is never called).

Incidental confirmation of the mechanism: `write_fmt`'s fast path is `args.as_statically_known_str()`. `eprintln!("tone: done")` (userland/toybox/src/tone.rs) is statically known -> one `write_all` -> one syscall, which is exactly why a no-argument `eprintln!` is the thing observed landing atomically inside soundd's line. soundd's 8-argument line is not statically known -> 17 fragments.

SO WHAT MAKES IT UNNECESSARY. Drain-side line assembly, described in the recommendation. The kernel drain is the sole writer to the serial backend, holds `BackendGuard` across each emission, buffers records without NL_TERM per (src, pid, tid) in a fixed static array, and emits only whole single-source spans.

IS THIS A CONVENTION OR A STRUCTURE? A structure, and here is the precise test. The rejected workaround (tests/common/audio.rs:310-362, qemu.rs:199-209) is a *host-side repair of an already-corrupted stream*: the corruption reaches the wire and the reader patches it up, imperfectly and probabilistically. Under this design the corruption cannot reach the wire, because the only code that writes to the backend emits whole records and whole assembled lines, and no other writer to that device exists. The host receives a stream in which a split line is not expressible. That is a property of the emitter, enforced by the emitter being singular — not a rule that writers are asked to follow. No userspace program can violate it, no matter how it fragments its writes.

THE ONE THING THAT REMAINS A CONVENTION, stated so nobody discovers it later: `flags & CONT` on an aged-out partial line is a *fact* the reader can act on, not a rule anyone must obey — but a reader that ignores it will concatenate two genuinely different lines. That is unavoidable given the shell's unterminated prompt (userland/shell/src/main.rs:35-36) and is the honest residue.

A userspace one-write-per-line primitive would still be worth having for EFFICIENCY — 17 syscalls per stats line is 17 syscalls, and each is a `with_fd_owner_data` process-lock acquisition (arch/syscall.rs:486 -> process.rs). But that is a performance argument, and it must not be the load-bearing mechanism. Recommend it separately, never as part of this correctness story, and never as "ship both or neither".

## 6. Staged plan

CONSTRAINT THAT SHAPES EVERYTHING: Gate A's thorough tier is the gate a scheduler-migration stage transition depends on, and its INPUT is the wire format — tests/toyos.rs:420 feeds `audio::parse_soundd_counters(&serial)` into `check_counters` against the recorded distributions in tests/audio-baseline.toml. Changing the wire invalidates the recorded 30-run sample and forces a ~17-minute re-record per side. The scheduler is at Stage 5 of 10 with Stage 6 (`Hw`) and Stage 7 (cutover) ahead. Therefore: **stages 0-4 below do not touch the wire at all and can land alongside scheduler work; only stage 5 changes it, and stage 5 must land in a window between scheduler stage transitions, with the baseline re-recorded as part of the same landing, not after.**

STAGE 0 — free deletions, no behaviour change. Delete `trace::dump` + `kind_name` (trace.rs:160-220, 61 lines) and `TraceKind::Mark` (trace.rs:37); verified zero callers. Delete `trace.rs:22`'s duplicate `MAX_CPUS` and import `scheduler::MAX_CPUS` as `irq_ring.rs:30` already does. Delete CLAUDE.md's stale panic_flush known-issue entry (main.rs already drains before the recovery branch). Separately, add `#[repr(C)]` + explicit discriminants to `toyos-sched/src/hw.rs`'s `TraceEvent` so it can satisfy constraint 2 whenever Stage 6 arrives. Green trivially. Land this even if the architect rejects everything else.

STAGE 1 — make TODAY's ring debuggable, no format change. `#[no_mangle]` + `#[repr(C)]` on the log-ring static so it stops being `d _RNv...CsfTd9MF1GDC5_...RING` with a per-build hash. Pure win, zero risk, and it is the prerequisite for constraint 2 under any design. Green.

STAGE 2 — introduce the record ring alongside, dual-write. `log!` and the console path write to BOTH the old byte ring and the new `CONSOLE_RING`; the drain still reads the OLD one. The wire is byte-identical, Gate A is untouched, and the new format gets exercised on every boot including panics. Add a debug-only consistency check that the two agree. Green, and this is where the reserve/commit/pad/resync code earns confidence before anything depends on it.

STAGE 3 — switch the drain to the record ring; render the SAME wire bytes. Kernel records render with today's `[kernel {secs}.{millis} cpu{n}]` prefix, userspace records render bare, exactly as now. Delete the old byte ring, `RingGuard`/`RING_LOCKED`, `drain_unlocked`, the drop-marker machinery, and the byte-at-a-time eviction. This is where ~140 kernel lines go and where the two confirmed defects — interleaving and head-truncation — become unrepresentable. The harness is untouched and the recorded Gate A baseline still applies, because the bytes on the wire have not changed. Green. **This is the stage that does the actual work.**

STAGE 4 — add drain-side line assembly. Still no wire-format change: assembled lines render exactly as today, just no longer split. The four harness workarounds become dead paths — instrument them to assert they are never taken, and run the audio suite to confirm across enough boots to cover the 1-in-120 case. Green, and the harness code is still present, so a regression here is loud rather than silent.

STAGE 5 — change the wire, delete the workarounds, re-record the baseline. Uniform per-record prefixes on every line including userspace. Delete `strip_kernel_logging`, `stats_field`'s resume loop, the `END_MARKER`-anywhere salvage, and `is_kernel_line` + the suffix split. `parse_soundd_counters` becomes an ordinary line parser. Re-record tests/audio-baseline.toml with `--audio-gate 30` on the new wire, in the same commit series, on a quiet host with no concurrent agent (CLAUDE.md: concurrent measurement is unreliable). Green, but this is the only stage that costs a measurement session and the only one that must be scheduled around the scheduler migration.

STAGE 6 — converge the trace ring onto the shared type; make `Hw::trace` (scheduler Stage 6) an impl over it. Do this only after scheduler Stage 5 has fully settled, and ideally as an input to Stage 6 rather than concurrent with it.

Stages 0 and 1 are unconditionally worth landing. Stage 3 is the payoff. If the architect wants to stop early, stopping after Stage 4 leaves the kernel correct and the harness workarounds dead-but-present — a defensible resting point that costs no measurement session.

## 7. The case against doing this now

THE STRONGEST ARGUMENT AGAINST IS NOT ABOUT THE DESIGN. It is that this is a diagnostics subsystem consuming a multi-stage kernel migration while three judge-reviewed specs sit unimplemented and a fourth migration is mid-flight, and the measured damage it repairs is one test timeout in 120 runs that is already mitigated.

Stated in full:

1. IT PERTURBS THE ONE GATE THE SCHEDULER MIGRATION DEPENDS ON. Gate A's thorough tier is what a stage transition gates on, and its input is the serial wire (tests/toyos.rs:420 -> parse_soundd_counters -> tests/audio-baseline.toml). CLAUDE.md itself says "Concurrent measurement is unreliable" and that a baseline must be A/B'd against the same HEAD in the same session. Changing the wire mid-migration means re-recording a 30-run sample on a tree that is itself changing underneath. The scheduler migration is the project's largest in-flight commitment and this proposal touches its instrument.

2. THE CAPABILITY-HANDLES SPEC WILL REWRITE THE THING THIS TOUCHES. `specs/assessments/capability-handles-spec.md` converts `Fd` -> `Handle` with refcounted typed kernel objects, and CLAUDE.md separately lists "`Fd` is a Unix-ism — rename to Handle". `Descriptor::SerialConsole` (fd.rs:516) is on that path. Building console infrastructure around today's descriptor model risks rework.

3. FOUR RINGS BECOME FOUR RINGS. Everyone agreed on this and it should be weighed honestly: `irq_ring` must stay (verified: coalescing latch, "overflow has no representation"), `audio::RecordRing` must stay (verified: panics rather than overwrite, because a dropped record strands a DMA buffer). The console ring is replaced by a console ring. The convergence win is real but it is 2->1-plus-a-shared-type, not 4->1.

4. AND THE LINE COUNT IS A WASH. I gave the arithmetic in `deletes`: ~291 deleted against ~310 added, of which 62 deleted lines are dead code available for free today. The owner is worried the codebase only grows. This proposal does not shrink it. It buys fewer *concepts* — two lock protocols to zero in the log path, one panic bypass to none, four decode procedures to one — which is a real quality argument but not the arithmetic one the owner asked for.

THE CHEAP ALTERNATIVE, WHICH I THINK DESERVES SERIOUS CONSIDERATION. The two defects are separable and only one of them causes silent damage.

The byte-granular overflow (log_ring.rs:42-55) is the dangerous one: it eats the HEAD of the oldest line, which strips the `[kernel ` prefix, which makes `is_kernel_line` (qemu.rs:57-61) file kernel output as guest stdout, where tests/toyos.rs:170 compares it byte-exactly against a `.expect` file. That is silent test corruption. It is fixable in roughly 15 lines: on overflow, advance `tail` to the next `\n` boundary instead of one byte, and count records rather than bytes. No new format, no new topology, no wire change, no baseline re-record, no interaction with the scheduler migration.

The interleaving defect, by contrast, has already been mitigated reader-side, and its measured cost is 1 timeout in 120 runs plus a stats line split in 1 of 15. It is genuinely ugly — the owner is right that the reader-side repair is a workaround — but it is *loud* when it fails, not silent.

So the minimal responsible answer is: Stage 0 + Stage 1 + the 15-line newline-boundary eviction fix. That banks 62 lines of dead code, makes the ring findable in LLDB, and kills the silent-corruption defect, for maybe a day of work and zero risk to any migration. The full record substrate is then a deliberate choice to make interleaving unrepresentable, taken when the scheduler migration is banked — not now.

MY RECOMMENDATION ON THIS TENSION: land the cheap alternative immediately, and schedule stages 2-5 for the window after scheduler Stage 7 (cutover). The design above is what I would build; the timing argument against building it *this month* is stronger than the design argument against building it at all. If the architect wants it sooner, stages 2-4 are wire-neutral and can proceed safely; only stage 5 must wait.

## 8. What the designers missed

1. THE STD FIX DOES NOT EXIST — and it is the second half of two designs' "ship both or neither". STRUCTURED and GLOBAL both recommend overriding `write_fmt` on the pal `Stderr` in rust/library/std/src/sys/stdio/toyos.rs, both calling it a permitted platform override of a default method. It is never called. `impl Write for StderrLock<'_>` (io/stdio.rs:1078-1097) does not override `write_fmt`, so `eprintln!` resolves `default_write_fmt` at the `StderrLock` level and fragments into `StderrLock::write_all` per piece; `StderrRaw::write_fmt` (stdio.rs:197) and the pal are both bypassed. Fixing it requires editing `impl Write for StderrLock`, a cross-platform type in a cross-platform file — forbidden by CLAUDE.md's std rules. Both designs conditioned their whole recommendation on a layer that cannot be built. CONVERGENCE reached the right verdict by the wrong route (it argued a pal buffer would never be flushed; a `write_fmt` override needs no flush — it is simply unreachable).

2. NOBODY CHECKED THE SHELL, AND IT CONSTRAINS EVERY COALESCING DESIGN. `userland/shell/src/main.rs:35-36` is `print!("{}> ", cwd); io::stdout().flush().ok();` — an unterminated line a human must see immediately. PERCPU's record extension and PERSOURCE's per-process rings both buffer unterminated writes with no flush policy for this case; PERCPU's failure-mode list does not include it. Any coalescing scheme needs an age-based flush and must weaken its guarantee from "every line is complete" to "every line is single-source". None of the three states the guarantee that precisely, and the imprecision matters because it is exactly the kind of gap that produces a workaround later.

3. PERSOURCE UNDERSOLD ITS OWN UNMAPPED VARIANT. It rejects kernel-side (unmapped) rings because they "cost the zero-syscall property AND the one-record-per-eprintln property, i.e. most of the win". The second half is wrong: extension/assembly is a kernel-side operation keyed on (pid, tid) and works identically whether the ring is mapped or not. Mapping buys only syscall elision — a performance property. Had it seen this, its 2 MiB cost (its own self-identified disqualifier, forced by `mm/paging.rs`'s 2 MiB-only mapping) would have evaporated and its stance would have been considerably stronger. That said, the insight that survives — the writer domain must be the unit of assembly — is the one I took, relocated to the drain where it needs no per-process state, no lifetime protocol, no PROCESS_TABLE, and no 2 MiB.

4. ALL THREE PUT ASSEMBLY ON THE WRITE PATH WHEN THE DRAIN IS FREE. The drain already holds `BackendGuard` (single-threaded), already renders text, already knows the time. Assembly there needs zero synchronization, zero per-process state, and is topology-independent — so it does not have to win the global-vs-per-CPU argument to be correct. Every designer put it on the write path, where it costs either a lockless reopen protocol (global), correctness-on-migration (per-CPU), or 2 MiB and a teardown protocol (per-source).

5. PERCPU'S EXTENSION IS PROBABILISTIC AND IT UNDER-WEIGHTS THAT. It concedes migration breaks extension and calls the result "labeled, never a corruption". But a harness that must join fragments 1 time in N is exactly the situation that produced this task — the 1-in-120 timeout was not a corruption either. Verified relevant: scheduler.rs:88-90 states work-stealing can resume a task on a different CPU than it parked on.

6. NOBODY STAGED AROUND GATE A'S BASELINE. The TOYOS dossier correctly notes gate A depends on the wire format, but no design's plan sequences around the fact that changing the wire invalidates the recorded 30-run sample in tests/audio-baseline.toml and costs a ~17-minute re-record on a quiet host — while the scheduler migration is mid-flight and CLAUDE.md warns that concurrent measurement is unreliable. My staged plan keeps stages 0-4 wire-neutral for exactly this reason.

7. UNMEASURED NUMBERS PRESENTED WITH EARNED-SOUNDING CONFIDENCE. GLOBAL's "roughly a 50-100x reduction in time spent on the shared cache line" and PERCPU's "~4 µs" merge cost at 128 cores are both estimates from operation counts by read-only agents. GLOBAL labels its estimate; PERCPU's is embedded in prose. More importantly, every design's line-count claim is from line ranges rather than a written patch, and none of them arrives at the honest conclusion that the net is a wash.

8. CREDIT WHERE DUE, since it corrects the brief: CLAUDE.md's known-issue entry claiming the syscall panic path branches to recovery without draining the ring is STALE. `kernel/src/main.rs` calls `panic_flush()` right after `crash_report` and before the `percpu::syscall_rip() != 0` branch, with a comment giving exactly that rationale. STRUCTURED, PERCPU and GLOBAL all caught this; it should be deleted from CLAUDE.md independent of this work.

9. A THIRD INSTANCE OF THE INTERLEAVING CLASS THAT THE BRIEF DOES NOT LIST, found by PRINTK and TOYOS and worth elevating: `SW_BUF_SIZE = 1024` (serial.rs:202) with `push_byte` spilling at that boundary (serial.rs:245-251) means the kernel's per-syscall atomicity guarantee has an undocumented 1 KiB ceiling — conceded in the doc comment at serial.rs:231-233 — and `fd.rs:516` routes every userspace console write through it with no length cap. A 4 KiB `sys_write` is already four independently-committed appends today. Any record format must pick and document a maximum payload rather than inheriting this accident.
