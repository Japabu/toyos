---
status: open
kind: defect
opened: 2026-08-20
---

# A `BTreeMap` panicked inside its own `insert` in a scheduler pass, after the fix that closed the last one

One sighting in **1,716 boots** of an unmodified `main`, taken 2026-08-20 in a
twelve-wide `bootable.img` boot storm run as the negative-control arm of an
unrelated pull request (#180, xHCI only, and the arm that panicked has that diff
*reverted*). The tree is `e4c2c8ff` plus a merge that touches no scheduler file.

```
[kernel 0.562 cpu0] PANIC: panicked at library/core/src/slice/index.rs:492:32:
unsafe precondition(s) violated: slice::get_unchecked requires that the range is within the slice
  Backtrace:
    core::panicking::panic_nounwind_fmt
    <NodeRef<Mut, (u64,u64), ReadyTask<KernelPayload>, LeafOrInternal>>::search_tree::<(u64,u64)>
    <BTreeMap<(u64,u64), ReadyTask<KernelPayload>>>::insert
    <CpuSched<KernelPayload>>::place::<KernelHw, PreemptOff>
    <CpuSched<KernelPayload>>::drain::<KernelHw, PreemptOff>
    <SchedPass<KernelHw, PreemptOff, Undisposed>>::begin
    kernel::sched::driver::with_cpu::<…, pass_block::{closure#0}>
    kernel::sched::driver::pass_block
    kernel::completion::wait_inner
    kernel::completion::wait_until::<kernel::inbox::submit::{closure#0}>
    kernel::inbox::submit
    kernel::arch::syscall::sys_inbox_submit
[kernel 0.580 cpu0] schedule_no_return: panicked inside a pass, cannot rejoin
```

`/bin/init`, pid 0, in `SYS_INBOX_SUBMIT` at 562 ms, 2 ms after `filepicker`
spawned. The machine halted: the panic was taken inside a pass, so
`schedule_no_return` could not rejoin.

## Why this is not the entry that was just closed, and why it is the same class

`a-btreemap-panicked-inside-its-own-navigation-in-a-scheduler-pass.md` was
deleted on 2026-08-20 at `b803af2d` with the argument that #149's `pop_surplus`
fix removed the stack-sharing window behind it. That entry's sighting was
`Option::unwrap()` on `None` in `navigate.rs:161`, on the **immutable iterator**
of a CPU's **`parked`** map, from `SchedPass::apply_timer`.

This one is a different container, a different operation and a different frame:
the **`ready`** map, `insert`, `search_tree`, from `SchedPass::begin`'s `drain`.
What is the same is the only thing that mattered in that argument — the panic is
inside `BTreeMap`'s own code, on a precondition no sequence of inserts and
removes can violate, on a `!Sync` per-CPU structure reached only through
`with_cpu`. `search_tree` indexes a node's key slice by a length the node itself
carries, so reaching this check means the node's `len` and its storage disagree:
a record written by something that had no business writing it, exactly as the
closed entry said.

So the closing argument is **not refuted by this** — #149 may well have removed
that window — but the class it was taken to close is still live, and it is live
on a tree that has the fix. Whatever writes these words is not only
`pop_surplus`, or is not only reachable the way that entry reconstructed.

## What produced it, so it can be produced again

Twelve parallel guests, `-smp cores=2`, `-m 2G`, q35, TCG, the ordinary
`bootable.img` device shape (xHCI stick + usb-kbd + usb-tablet, NVMe,
virtio-gpu, virtio-net, virtio-sound), `snapshot=on` on both drives, each guest
killed as soon as its serial log carried `Boot: complete` or a panic marker.
1,716 boots in ten minutes on the dev host, one death.

**And that marker is wrong, which is worth more than the recipe it belongs to.**
`Boot: complete` lands at ~240 ms of guest time, *before* logd, the compositor,
soundd, netd and filepicker are spawned — and both sightings died in the spawn
burst behind it, at 562 ms and at 554 ms. A storm that kills on `Boot: complete`
therefore samples almost none of the window it exists to sample; it reaches the
first line or two past the marker only because the poll and the kill are not
instantaneous. The completion marker is **`compositor: ready`**, which is the
last line of that burst. Measured on the dev host with it, 12-wide: 450 boots in
two minutes, so the change costs nothing and every boot covers the window.

The companion arm — the same tree with #180's xHCI diff applied, same shape,
same session, immediately afterwards — was **1,680 boots, zero deaths**. One in
1,716 against zero in 1,680 separates nothing: both arms are consistent with the
same rare defect, and nothing in #180 touches the scheduler. It is recorded here
so the next reader does not read the clean arm as a fix.

The full capture is not in the tree; the lines above are the whole of what the
serial log carried past `Boot: complete`, and the boot before the panic is
ordinary — `Boot: complete (335ms)`, klogd/usbd/iod up, logd, compositor,
soundd, netd and filepicker spawned in order, no earlier warning of any kind.

---

## 2026-08-20, second session: reproduced, and it is not what the class was named

Same host, `wt/toyos-btree2`, base `e4c2c8ff`, the recipe above with
`compositor: ready` as the completion marker. **6,576 boots, one death** —
`search_tree`'s precondition again, on the same `BTreeMap<(u64,u64),
ReadyTask<KernelPayload>>`, on cpu0, in the same `pass_block` from
`SYS_INBOX_SUBMIT`, 2 ms after `filepicker` spawned. What differs is the
operation: `RunQueue::pop_next`'s `remove`, not `place`'s `insert`.

```
[kernel 0.554 cpu0 tid=1] PANIC: panicked at library/core/src/slice/index.rs:492:32:
unsafe precondition(s) violated: slice::get_unchecked requires that the range is within the slice
    <NodeRef<Mut, (u64,u64), ReadyTask<KernelPayload>, LeafOrInternal>>::search_tree::<(u64,u64)>
    <RunQueue<KernelPayload>>::pop_next
    <SchedPass<KernelHw, PreemptOff, Disposed>>::finish
    kernel::sched::driver::with_cpu::<…, pass_block::{closure#0}>
  Running: pid=6 tid=Some(Tid(1))   Process: soundd pid=6 state=Live
```

So the class is live, reproducible on demand, and the fair band's map is what it
keeps landing in.

### The instrumented arm, and what it settled

The same storm on the same base plus `--kernel-feature sched-tripwire` (below):
**7,136 boots, twelve deaths.** Twelve, on twelve boots, in these shapes:

| deaths | what died |
|---|---|
| 3 | inside `dlmalloc` itself — `malloc+0x7d` writing `0x18`, `malloc+0x312` reading `0x43800008`, `malloc+0x4e6`/`memalign+0x74`/`insert_large_chunk+0xf4` |
| 4 | a Ring 0 instruction fetch at `0x2`, and one at a `RawVec` |
| 2 | `Vec::push`'s `ptr::write requires that the pointer argument is aligned and non-null` |
| 2 | `attempt to subtract with overflow` on a field — `src/log/mod.rs:142`, `src/process.rs:1665` |
| 1 | the tripwire's own walk of the ready band, `navigate.rs:534`, at the *entry* to a pass |

**Three of the twelve died inside the allocator**, one of them allocating a
`BTreeMap<UserAddr, Region>` node from `loader::insert_elf_regions` during a
spawn. That is not a scheduler container reading as a value nothing can write.
It is dlmalloc's own free lists reading that way, and everything else in the
table is what an allocator handing out overlapping memory produces downstream.

**The two instruments both answered, and both answered no.**

- The `CpuSched` byte shadow (`sched-tripwire`) compares the whole record across
  every window in which nothing is allowed to write it. In 7,136 boots it fired
  **zero** times. `static SCHEDS` — the record itself, the `BTreeMap` headers
  inside it, the `Option` niche whose `None` arm produced two of 2026-08-19's
  deaths — was never altered.
- `hw::report_contexts` prints which CPU is standing on which `KernelCtx` and
  whether the crashing stack pointer lies inside another CPU's task stack. It
  ran on every one of the twelve. **Not once** did two CPUs name one context, and
  **not once** was the crash on a sibling's stack.

### The verdict on the window

There is no second window in the scheduler, and the class was never a window.

- `CpuSched::hand_off` is the **only** path that publishes a context to another
  CPU and kicks it; the other `Msg::Adopt` producer is `driver::spawn`, whose
  record nobody is standing on. Its two callers are `answer_steal_requests`
  (#149's `pop_surplus(loaded)` filter) and `place`'s real-time wake-forward —
  and the second is safe by an invariant this audit made exact: every pass exit
  leaves `loaded_key() == running.map(key)` (`switch_to_current`,
  `switch_to_idle`, `try_sleep`), and `place` only ever carries a task out of
  `parked` or `InTransit`, which the linear task states make disjoint from
  `running`. That was an argument; it is now an assertion at `hand_off`, so it
  covers every migration there is or will be.
- `check_switch_frame` (#149's guard) tests only that the incoming `rsp` is a
  kernel address and that `[rsp+56]` is one. **It sees the never-switched case
  and nothing else**: a task that has been switched away before carries a real
  old frame whose return slot is real kernel text, and restoring it inside the
  window would pass the guard silently. So a green guard was never evidence, and
  `report_contexts` is what replaces it — on every kernel crash, not only on a
  frame that fails the test.

### The three arms, and what the control rules out

Same host, same session, same recipe, same base, 12-wide, thirty minutes each:

| arm | kernel | boots | deaths | one per |
|---|---|---|---|---|
| A | `e4c2c8ff`, untouched | 6,576 | 1 | 6,576 |
| B | + this diff, `--kernel-feature sched-tripwire` | 7,136 | 12 | 595 |
| C | + this diff, feature **off** | 6,361 | 2 | 3,181 |

**C is the control and it clears the diff.** Everything this change compiles
into a default kernel is a reader — one comparison in `hand_off`, and a report
that runs after a crash has already happened — and arm C measures what that
argument predicts: 2 in 6,361 against 1 in 6,576 is the same rate, and 3 deaths
in the 12,937 boots of A and C together is this host's baseline. Both of C's
deaths are the class (a `BTreeMap` node's `idx <= node.len()`, and a panic whose
own message came out corrupted, followed by a Ring 0 fetch at `0x0`), and both
landed in the spawn burst.

Against that baseline the tripwire arm's 12 in 7,136 is a **7.2× amplification**
— Poisson, 15 events over both exposures, 5.33 expected in B under a single
rate, p ≈ 0.008. It is measured and **unexplained**: the feature compiles
readers and no decision either, so it is not *causing* corruption; what it does
is spend time and touch memory on a path every pass takes, and something about
that widens the window. Read it as an instrument that amplifies, not as a defect
the feature introduces, and do not read the amplification as understood.

### What is owed now

**The kernel heap, not the scheduler.** The next reader chases what corrupts
dlmalloc's structures — a heap overrun, a double free, or a `dealloc` with a
layout that is not the `alloc`'s — and the storm is the instrument: the
`sched-tripwire` build reproduces the class about once every 600 boots against
once every ~4,300 on a default one, which is four minutes a sample instead of
half an hour. Every death in that arm clusters in the spawn burst
(`insert_elf_regions`, `sys_inbox_setup`, the ELF load), which is where to look
first. Note also what a kernel stack is: 128 KiB of the *same* dlmalloc heap
(`loader::alloc_kernel_stack` → `OwnedAlloc::new`), one canary word at the
bottom and no unmapped page beneath it — so stacks and `BTreeMap` nodes are
neighbours in one arena, which is a shape worth ruling in or out early.

### What landed with this

- `kernel/src/hw.rs`'s `report_contexts`, on **every** kernel crash: which CPU
  is on which `KernelCtx`, whether two name one, whether this crash is running on
  another CPU's stack, and whether an idle context's stack top has been written.
  Ahead of both backtraces, because a storm capture of this very class ended
  `FAULT … RECURSIVE` one line into the user backtrace and took the rest of the
  report with it.
- `CpuSched::hand_off`'s assertion, above.
- `kernel/Cargo.toml`'s `sched-tripwire`: the byte shadow, plus a walk of the
  ready band, the park map and the dying list at both ends of the driver's
  exclusive region — so a red says *when*, and a red at the entry means the
  container was already broken before that pass ran a statement.

---

## 2026-08-21: the heap gets its own tripwire, three candidates close, and the class stops

`sched-tripwire` cleared its own subject and handed the class to the heap. This
session asked the heap the same question — *did anything write outside an
allocation it owns* — closed three of the four mechanisms that were open, and
found that the instrument built to catch the class makes it stop happening.

### The headline: 7,205 boots, no deaths

Same host, same recipe, same twelve-wide shape, thirty minutes, against the arms
already on record above:

| arm | kernel | boots | deaths | one per |
|---|---|---|---|---|
| A | `e4c2c8ff`, untouched | 6,576 | 1 | 6,576 |
| B | + `sched-tripwire` | 7,136 | 12 | 595 |
| C | + that diff, feature off | 6,361 | 2 | 3,181 |
| **H** | **+ `sched-tripwire` + `heap-tripwire`** | **7,205** | **0** | **—** |

Arm H differs from arm B in exactly one thing: the heap bands. Under arm B's
measured rate, 7,205 boots expect **12.1** deaths; H has none. Poisson,
p ≈ 5.5 × 10⁻⁶. The boot rate is arm B's — slot 5 was on its 425th boot 1,318 s
in, 3.1 s a boot against arm B's 3.0 — so this is not fewer boots, and the one
capture the script kept is a guest that missed its 30 s ceiling, not a panic.

**And no band fired in any of those 7,205 boots.**

### What that does and does not establish

Two readings survive it, and this session cannot separate them:

* **The bands absorb it.** The writer overruns by no more than 32 bytes past the
  end of an allocation, or by no more than the alignment before its start, and
  the band now takes the write that used to land in the neighbouring chunk. That
  no band *fired* is consistent with this and not evidence against it: a band is
  read at `dealloc` and — for the running task's kernel stack — at every pass,
  and nowhere else. An allocation that lives from early boot to `compositor:
  ready` is never freed inside a boot, so its band is never read.
* **The bands displace it.** Every allocation in the machine moves by between 32
  and 4,096 bytes, so whatever pointer arithmetic goes wrong now lands somewhere
  that does not matter. This is the ordinary way a heisenbug answers an
  instrument.

**What separates them is a sweep**, and there are two shapes for one. Widen the
head band to carry an intrusive `prev`/`next` and put every live allocation on a
list, which costs a lock on every allocation; or scan for the bands instead of
listing them — `KernelPageSource` is the only thing that hands `dlmalloc` a page,
so the kernel knows every 2 MiB page the heap owns, and a scan for the head
record's magic at eight-byte alignment finds every band in it without a registry
at all. Either one, run once per boot at the completion marker, turns "no band
fired" into either a named victim or a real negative. That is what is owed next,
and it is why `heap-tripwire` is a poor arm for *reproducing* the class even
though it is a good one for bounding it.

### What the allocator already answers, and has never once complained

`dlmalloc` is **unmodified crates.io `0.2.13`** (`kernel/Cargo.lock`), so nothing
in it is ours to have broken. More to the point, its `Dlmalloc::free` already
runs an *unconditional* check on every `dealloc` this kernel has ever done —
`validate_size` (`src/lib.rs:143`, `src/dlmalloc.rs:1196`):

```rust
let psize = Chunk::size(Chunk::from_mem(ptr));
let min_overhead = self.overhead_for(p);
assert!(psize >= size + min_overhead);
assert!(psize <= size + max_overhead);
```

That is every `dealloc`'s `(ptr, layout)` pair checked against the chunk header
the allocator itself wrote, in the **shipping** build, and it has not fired in
any capture on record. The PMM says the same thing one layer down with
`assert!(!bm.is_free(idx), "double free of physical page")`
(`kernel/src/mm/pmm.rs:308`). **So the class is neither a mismatched free nor a
double free.** What is left is a write landing outside the allocation that owns
it — which is what the bands were built to ask about.

### Task kernel stacks are not the writer, and here is the number

The most attractive unread hypothesis was this file's own closing note: a kernel
stack is 128 KiB of the same dlmalloc arena as the `BTreeMap` nodes, with one
canary word and no unmapped page beneath it. `arch::percpu` says in its own words
what that costs, because it fixed it for the *other* two stack kinds —
`IDLE_GUARD_SIZE`: *"an overflow used to land in the heap and be found later,
somewhere else, as a corrupted allocation."* Idle stacks got an unmapped guard
page, IST1 a fill-pattern guard. Task kernel stacks got neither.

It also explained the amplifier: `sched-tripwire`'s `verify` puts a `[u64; 96]` —
**768 bytes** — on the stack at every entry to `with_cpu`'s exclusive region,
which is on the deepest path in the kernel, and 7.2× is what an extra 768 bytes
per pass would look like against a stack that was nearly full.

It is wrong. The depth ladder, on a clean two-CPU boot of the shape the storm
uses:

```
kstack: a task kernel stack has been at least  4096 of 131072 bytes deep (tid=0)
kstack: a task kernel stack has been at least  8192 of 131072 bytes deep (tid=0)
kstack: a task kernel stack has been at least 16384 of 131072 bytes deep (tid=0)
```

and never the 32 KiB rung. **16 KiB of 128 KiB, with 96 KiB of headroom** — a
margin 768 bytes cannot bridge. The amplifier remains measured and unexplained,
and it is not stack depth.

### The deferred release cannot be the writer either

`issues/kernel/deferred-release-outlives-its-syscall.md` was the standing
candidate: releases that run after the kill has returned, on another CPU. It is
a real defect and it is not this one. `ZERO_QUEUE` is a `Lock<Vec<KObjectRef>>`
(`kernel/src/object/mod.rs:339`) and every `KObjectRef` variant holds an
`Arc<T>` (`:195`), so the queue **owns a strong reference** for as long as a
batch is in flight, and `run_zero_handles` runs on `&self` through it. An
object's heap node cannot be freed before, or while, a hook holds a reference
into it. The asynchrony that queue produces is *semantic* — a syscall answering
`Ok(22)` where the ABI says `NotFound` — and never a lifetime one.

### What landed: `heap-tripwire`

A band of known bytes on each side of every heap allocation
(`kernel/src/mm/alloc.rs`), read back when it is freed and, for the running
task's kernel stack, at every scheduler pass.

* The **tail** is 32 bytes — `dlmalloc`'s `min_chunk_size` on x86-64 — so a near
  miss lands there rather than in the next chunk's header, where it would be
  indistinguishable from allocator state.
* The **head** is as wide as the request's alignment. That is the design and not
  a rounding accident: `alloc_kernel_stack` asks for `(KERNEL_STACK_SIZE, 4096)`,
  so a task kernel stack gets **4,096 bytes of head band**, whose last 32 bytes —
  the record carrying the size and alignment the allocation was made with — sit
  immediately below the lowest usable stack word. A stack running off its own
  bottom writes there first. It is the guard page the idle stacks have, in the
  one form available to an allocation that comes out of the heap.
* The **ladder** is nine volatile reads per pass at fixed depths, reported by
  `hw::report_contexts` on every kernel crash.

**Not `dlmalloc/debug`, and that is measured rather than preferred.** The crate
carries Doug Lea's own checker behind that feature, and `check_malloc_state`
walks all 32 smallbins, all 32 treebins and every chunk of every segment at the
head of *every* `malloc` and `free`. It is the better oracle at O(heap) per
allocation on a heap that reaches several MiB — a price a TCG boot storm cannot
pay.

### A hazard the instrument found in itself, which is worth the arm it cost

The first draft logged the high water the moment it rose, from inside
`check_stack_canary` — which runs inside `with_cpu`'s exclusive region. **Four of
the first twelve-wide arm's guests wedged in six minutes**, every one in the
spawn burst, every one with the previously-logged rung as its last line, and
every one recorded as a hang rather than a panic. `sched-tripwire`'s three arms
had no hang between them.

`crate::log!` is not a leaf. The log's own readiness path reaches
`driver::pass` — `post_readiness+0x98` above `pass+0x94` is in one of this file's
own captures — so **a log emitted from inside a pass can re-enter the pass**.
`sched-tripwire`'s own `log!` gets away with it only because a `panic!` follows
and it never has to return. The number now lives in an atomic that nothing logs
from where it is written.

---

## 2026-08-21, the separating sweep: it is displacement, and a reader that writes nothing is worth more than the bands

Five arms, one host, one session, one recipe — twelve-wide `bootable.img`,
`-smp cores=2`, `-m 2G`, q35, TCG, `snapshot=on`, each guest killed on
`compositor: ready` or a death marker, thirty minutes each. `sched-tripwire` is
on in every arm, so every number below is against **this session's own
baseline** and not against the older one: `main` has moved a long way since
`e4c2c8ff` (`arch/syscall.rs`, `object/handle.rs`, `hw.rs`, `sched/driver.rs`,
the whole of `soundd`), and a stale baseline would have carried every
conclusion.

| arm | head band | tail band | `heap-sweep` | boots | deaths | hangs |
|---|---|---|---|---|---|---|
| **B2** | — | — | no | 7,251 | **6** | 19 |
| **H′** | `max(align,32)` | 32 | no | 7,211 | **0** | 19 |
| **W** | `max(align,32)` | 32 | **yes** | 7,040 | **9** | 30 |
| **F** | `max(align,32)` | **0** | yes | 7,011 | **0** | 28 |
| **R** | **0** | 64 | yes | 7,102 | **2** | 28 |

B2 is the positive control and it reproduces the class: 6 in 7,251, one per
1,208. Against that rate, H′ expects 5.97 and has none (p = 2.6 × 10⁻³), F
expects 5.80 and has none (p = 3.0 × 10⁻³), R expects 5.88 and has two
(p = 0.068), and W expects 5.83 and has nine (p = 0.14 the other way — the same
rate).

The hangs are not deaths and not the class: they are guests the thirty-second
wall-clock ceiling caught, and one of the captures carries
`[kernel 2.404 cpu0] spawn: /bin/netd`, a guest running eight times slow. Their
count is flat across every arm, so they confound nothing.

### The verdict: **displace**, and absorb is refuted as the explanation

The absorb reading made one falsifiable prediction, written into this file
before the sweep ran: *shrink the bands and the deaths return; widen them and
they stay gone*. Arm **F** shrank the tail band to nothing — the slack past a
payload is byte-for-byte what an unbanded build has, because `dlmalloc` rounds
to a 16-byte granule either way — and the deaths **did not return**. They went
to zero. And the **widest** configuration in the table, arm W, has the **most**
deaths of any arm.

There is no monotone relation between how much slack a band puts past an
allocation and how often the class kills. Every arm that moved every allocation
drew a different rate: +32 bytes per allocation (F) draws zero, +64 (H′) draws
zero, +64 with the payload at the other end of its chunk (R) draws two, and +64
with a reader added (W) draws nine. That is a layout lottery, which is what
displacement *is*.

### A band did fire, once, and it names a victim

One capture in 7,040 — arm W, slot 11's 154th boot, at 530 ms:

```
HEAP TRIPWIRE (dealloc): 0xffff800002cb0000 was written past its 131072-byte
allocation — tail band word +0 is 0xffff800002cd0010
  kernel::mm::alloc::tripwire::disarm
  <kernel::mm::alloc::KernelAllocator as ...GlobalAlloc>::dealloc
  core::ptr::drop_glue::<kernel::sched::payload::KernelPayload>
  <kernel::hw::KernelHw as toyos_sched::hw::Hw>::release
  <toyos_sched::cpu::SchedPass<...>>::begin
```

131,072 bytes is `KERNEL_STACK_SIZE`, so the victim is a **task kernel stack**
being freed as its task is reaped, and the dirty word sits at
`payload + 131072` — which is exactly `stack_top`, the first word *above* the
highest usable stack word. The value written there is `stack_top + 16`, which is
`Chunk::to_mem` of a chunk header at `stack_top`.

So a bounded, adjacent overrun does exist and the bands do catch one. It is not
what governs the rate — one fire against fifteen deaths across the three banded
arms — but it is the first time in this file's history that anything has named
a *victim* rather than a downstream symptom.

**The periodic sweep never fired**, in roughly 127,000 walks across arms W, F
and R (~7,000 boots each, six to seven sweeps a boot, 344 to 498 live bands per
walk). The one dirty band was found at `dealloc` and not by a walk, which bounds
the write to the last 25 ms before that free.

### The instrument is the amplifier, and that is now measured twice

**W and H′ differ in exactly one thing: `heap-sweep`.** Same bands, same
placement, same addresses, same everything else. H′ has zero deaths in 7,211 and
W has nine in 7,040. Conditional on the nine events, the chance that all of them
land in W under one common rate is (7040/14251)⁹ = **1.8 × 10⁻³**.

`heap-sweep` compiles no decision. It reads bands, it maintains a table of the
2 MiB pages `KernelPageSource` has handed out, and — this is the part that is
not free — it takes `dlmalloc`'s lock on the pass path every 25 ms of guest time
and holds it for a walk of every heap page. So a pure *reader* moved this class
from "does not happen" to "happens at the baseline rate".

That is the same shape as the 7.2× `sched-tripwire` amplification this file
already records as measured and unexplained, and it is now the second
independent instance: **instrumentation that spends time on the scheduler's pass
path multiplies this class.** The mechanism is still not known, but the shape
of it is now three facts rather than one — a byte-shadow copy on the pass path
amplifies, a heap walk under the allocator's lock on the pass path amplifies,
and neither writes anything the machine reads. Read every future arm of this
class knowing that the instrument's *cost* is a bigger lever on the rate than
the thing the instrument measures.

None of W's nine deaths is a new failure mode: they are the class's own
signatures — `dlmalloc::malloc` faulting on `0x43800008` (the same address the
2026-08-20 arm B recorded), a Ring 0 fetch at `0x18`, a GP fault inside
`sort_unstable_by_key` under `RelocationIndex::finalize`, `hashbrown` "went past
end of probe sequence". So the sweep amplifies the existing class rather than
introducing one.

### Two claims in the entry above this one are wrong, and here is the evidence

* **`dlmalloc`'s own `validate_size` does fire.** The 2026-08-21 entry argued
  "the class is neither a mismatched free nor a double free" from the fact that
  it never had. Arm B2 has it:
  `dlmalloc-0.2.13/src/dlmalloc.rs:1207: assertion failed: psize <= size + max_overhead`,
  from `__rust_dealloc` under `<kernel::vfs::Vfs>::open_backing_depth`, at
  484 ms. A second capture has `dlmalloc.rs:873` overflowing its subtraction in
  the treebin search. What `validate_size` actually proves when it fires is that
  the chunk header and the `(ptr, size)` pair disagree — which a corrupted
  header produces as readily as a mismatched free — so the conclusion may
  survive, but the evidence it rested on does not.
* **"No band ever fired" is no longer true**, per the capture above.

### A lead nobody has followed: one victim is written the same wrong value twice

Two of arm W's nine deaths are byte-identical in the one place that matters:

```
KernelSlice OOB: offset=0xffff80007cae3310 size=0xd total=0x200000
  <kernel::drivers::xhci::XhciController>::bot+0x939
  <kernel::drivers::xhci::XhciController>::scsi+0xe5
```

`size=0xd` is `CSW_LEN` and `total=0x200000` is the xHCI DMA region, so the
offset is `dev.block + MSC_CSW` and **`MscDevice::block` is holding a kernel
text address**. `MscDevice` lives inside `XhciController`'s fixed
`msc: [MscBlock; MSC_BLOCKS]`, so this is a victim and not a cause — but the
value is *the same in two independent boots*, which means the writer stores a
deterministic kernel pointer at a deterministic place. Every other corrupted
word this class has produced looked random. Whatever writes a stable
`0xffff80007cae3310` is the narrowest thread anyone has been handed.

Note also that `bot` reached line 763's `dma.subslice(dev.block + MSC_CBW, 31)`
successfully and died on line 829's `dma.subslice(dev.block + MSC_CSW, 13)`, so
`dev.block` changed **inside one `bot` call** — across the command phase, the
data phase and the status phase, which is where that call waits.

### What is owed now

* **The rate is not a measurement of the heap.** Any arm that changes the pass
  path's cost changes it more than the bands do. An arm that means to measure a
  *fix* has to hold the instrument set fixed and vary only the fix.
* **`heap-sweep` is the instrument that names victims**, and it earns a build
  because the one band that fired named one. It found nothing in 127,000 walks,
  so the write it is looking for either lands in a payload (where no band can
  see it) or lands within 25 ms of a free.
* **The `0xffff80007cae3310` written over `MscDevice::block`** is the one
  deterministic artefact on record. Resolving that address to a symbol, and
  finding what holds a pointer to it, is the next thing to do.

---

## 2026-08-21, the silent half: three quarters of this class never says a word, and every rate above is a count of the quarter that does

Four arms, one host, one session, one recipe, thirty minutes each, twelve-wide,
`compositor: ready` as the completion marker — and one change to the harness
that turns out to matter more than any arm: **a guest whose QEMU is gone is
counted.** Every earlier storm of this class counted a boot as a death when its
serial log carried a panic marker. This one also counts the boots where QEMU
simply is not there any more, and those are three quarters of the total.

### What a "silent" death actually is, read off the vCPUs

They are not host noise and they are not the harness. Re-run with
`-action reboot=shutdown -action shutdown=pause`, which parks QEMU on a guest
reset instead of letting `-no-reboot` end it, 441 boots produced one — and it is
readable:

```
{"return": {"status": "shutdown", "running": false}}
CPU#1  RIP=1b7b9f15ffd23100  RFL=00000446 [D--Z-P-]  CPL=0  HLT=0
       RSP=ffff800000c00680  SS =0000 0000000000000000 00000000 00000000
       CS =0008 ... CS64 [-R-]      CR2=ffff800000e21ee8
       GS =0000 ffff800000c00510    TR =0028 ffff800000c00530
```

`RIP` is not canonical, `SS` is the null selector, and `DF` is **set** — which
Ring 0 code never runs with. That is `context_switch`'s last two instructions
and nothing else: `popfq` took a garbage word for `RFLAGS` and `ret` took a
garbage word for the return address. Sixty-four bytes below the `rsp` that
leaves is `0xffff800000c00640`, and cpu1's `PerCpu` is at `0xffff800000c00510`
(its `GS` base, with its TSS at `+0x20` and its GDT at `+0x90`) — **so the frame
this switch restored was inside the per-CPU region, which is not a stack at
all.** The machine then triple-faulted, which is why nothing reached either
channel: the 16550 goes quiet at the virtio-console handover and the virtio
console needs a live guest to drive its queue.

The one other death in that probe was `rc=139` — QEMU itself taking a SIGSEGV.
Host crashes are real and they are rare: macOS wrote **one** `qemu-system-x86_64`
crash report between 17:39 and 19:26, across roughly 29,000 boots, and it is that
one.

### The four arms

| arm | kernel | boots | deaths | of those, spoke | hangs |
|---|---|---|---|---|---|
| **A** | `sched-tripwire` | 7,413 | **22** | 6 | 0 |
| **B** | `sched-tripwire` + `heap-lockspin` | 7,004 | **12** | 1 | 0 |
| **C** | `sched-tripwire` + `heap-sweep` + `stack-witness` | 7,044 | **21** | 0 | 17 |
| **D** | `sched-tripwire` + `stack-witness` | 7,349 | **23** | 4 | 0 |
| **E** | D + the switch test extended to the idle context (15 min) | 3,671 | **10** | 3 | 0 |

Every p below is the conditional binomial on the two counts — under one common
rate, `k1 | (k1+k2) ~ Binomial(k1+k2, n1/(n1+n2))`, two-sided.

### The band arms did not stop this class. They stopped it *speaking*.

**C against A is the same rate**: 21 in 7,044 against 22 in 7,413, ratio 1.00,
p = 0.79. And **not one of C's 21 deaths said anything**, against 6 of A's 22:
0 versus 6, p = 0.015.

That is the correction this session owes the two entries above it. The headline
of 2026-08-21 — *"7,205 boots, no deaths"* under `heap-tripwire`, p ≈ 5.5×10⁻⁶ —
and the sweep table's `H′ 0/7,211` and `F 0/7,011` are all counts of deaths that
**printed a panic marker**. A storm that kills on a marker cannot see a guest
that vanishes, and this arm says that is where the bands send them: the machine
dies exactly as often and dies without a word. The bands' displacement is real
and it is not a suppression — read "the bands took the deaths to zero" as
"the bands took the *reports* to zero", and treat every number in the two
sections above as a lower bound on its arm.

### Lead 3, the amplifier: the allocator's lock is not the window

`heap-lockspin` is `heap-sweep`'s cost without the sweep — it takes
`dlmalloc`'s lock on the pass path and holds it for `HOLD_NS` of guest time,
reading nothing and walking nothing. At 1 ms every 25 ms, a 4% duty cycle on
the one lock the whole kernel allocates through:

**12 in 7,004 against the baseline's 22 in 7,413 — ratio 0.58, p = 0.093.** It
did not amplify. If anything it went the other way.

So the hypothesis as it was posed — *the class is multiplied by time spent in
the pass with the allocator lock held, and the racer is whoever writes heap
memory without the lock* — is **not supported**. What is left of the amplifier
is what `sched-tripwire` already showed: time on the pass path, without any
lock. The next arm is `pass-spin` — the same visit, the same `HOLD_NS`, no lock —
which separates "the delay" from "the delay under this particular lock", and
after it a longer `HOLD_NS`, because 1 ms bounds the effect rather than refuting
it.

### Lead 1: the deterministic address is a return address inside `BTreeMap::insert_recursing`

`0xffff8000_7cae_3310` is `dev.block + MSC_CSW`, and `MSC_CSW` is `0x2040`, so
the word written into `MscDevice::block` was `0xffff8000_7cae_12d0`. The kernel
is loaded by UEFI at a fixed `0x7ca00000` on this shape — `Kernel memory located
at: 0x7ca00000` in every boot log of every arm — and its first segment is
`vaddr 0, memsz 1409024, flags 5`, so kernel offset **`0xe12d0` is inside the
executable segment**, with 0x77000 of text above it. It is kernel text.

Resolved against the `nm` of a kernel built from this tree with arm W's own
feature set (`sched-tripwire heap-sweep`), `0xe12d0` is:

```
<btree::node::Handle<NodeRef<Mut, (Tid, u64), kernel::process::MappedPages, Leaf>, Edge>
  >::insert_recursing::<Global, <VacantEntry<(Tid,u64), MappedPages>>::insert_entry::{closure#0}>
  + 0x930          (symbol at 0xe09a0, next symbol at 0xe14c0)
```

`+0x930` of a 0xb20-byte function is **mid-function**, which no function pointer
and no vtable slot ever is: it is a **return address**. And the identification
survives a rebuild — the contiguous run of `alloc::collections::btree`
monomorphizations around it spans `0xd7980 .. 0xf4920`, **118,688 bytes**, with
39,760 of them below the target, and arm W's tree differs from this one by a
loop rewrite in `mm/alloc.rs` alone.

So the word is the return site of a call inside an insert into
`BTreeMap<(Tid, u64), MappedPages>` — the map a spawn and a demand fault write.
`XhciController::with_storage` copies the whole `Disk` out of `self.msc[at]` onto
its own frame and writes it back after, so the `&mut MscDevice` every phase of
`bot` holds points **into the running task's kernel stack**. Something executed
`insert_recursing` with that slot as its stack pointer.

That is the same sentence the parked capture above writes in the other
direction: a CPU running with an `rsp` that is not its stack. The two
fingerprints agree, and neither is about the heap's *contents*.

### What landed, and what it is for

* **`stack-witness`** (`kernel/Cargo.toml`), three readers and a panic each:
  * `sched::driver::check_stack_ownership`, at every pass — the two words a
    Ring 3 entry takes its stack from (`percpu.kernel_rsp` and `tss.rsp0`)
    against the running task's own `kernel_stack_top`, and this CPU's `rsp`
    against that stack's bounds.
  * `hw::check_switch_frame`'s third test — the incoming context's `rsp` must be
    **inside the stack its own `kernel_stack_top` names**, not merely a kernel
    address. This is the test the parked capture asks for: `0xffff800000c00640`
    is a kernel address and the word at `+56` was one too, which is why #149's
    guard let it through and the machine triple-faulted instead of reporting.
  * `drivers::xhci::wait::msc`'s `BlockWitness` — `dev.block` snapshotted at the
    top of a `bot` round trip and compared before the status phase reads it,
    reporting the field's address, its depth below this CPU's Ring 3 entry
    stack, and both values.
* **`pass-spin` / `heap-lockspin`** — the amplifier's control, above.
* The heap tripwire's tail-band report now prints the **whole band**, four
  words, not the one that failed. The single fire this instrument has ever had
  held `stack_top + 16` at `stack_top`, which is what an interrupt frame's saved
  `RSP` looks like if the entry happened sixteen bytes above the stack — and
  from one word that reading could not be checked. Word +8 would be a stack
  segment selector if it is one.

**No witness fired in any arm.** 7,044 boots of C, 7,349 of D — `stack-witness`
on a kernel with no bands, at arm A's rate and arm A's speaking character — and
3,671 of E, which is D with the switch test extended to the idle context after
D's task-only form fired zero times against 19 silent deaths. So across 18,064
boots:

* `percpu.kernel_rsp` and `tss.rsp0` **never** disagreed with the running task's
  own stack top, and no pass ever ran on an `rsp` outside that stack. The
  "a Ring 3 entry landed on a stack another task is live on" mechanism is not
  what is happening — or not while a pass is looking.
* `MscDevice::block` never changed inside a `bot` round trip. The two identical
  `KernelSlice OOB` deaths did not recur in this session at all.
* No context was ever switched onto a frame outside its own stack — including
  the idle contexts, in E. The parked capture's wrecked frame is therefore
  either rarer than 1 in 3,671 boots, or it is not reached through
  `Hw::switch` at all, which would mean an `rsp` wrecked *after* the switch
  passed and before the `popfq`.

That last branch is the one worth taking next: `check_switch_frame` runs on
`ctx.rsp` and `context_switch` then reads the frame through `rsp` a few
instructions later, and nothing tests the seven words in between.

### A fourth deterministic fingerprint, and this one names a scheduler function

Two of arm E's ten deaths are byte-identical:

```
FAULT rip=0xffff80007cad48ae cr2=0x0000000000000078 err=0x0 cr3=0xded001
```

`0x7cad48ae - 0x7ca00000` is `0xd48ae`, and against that build's own `nm` it is
`<toyos_sched::task::TaskShared<Msg<KernelPayload>>>::transition + 78` — a read
through a **null** `self` at field offset `0x78`, at the same instruction, in
two independent boots. A third death in the same arm faulted at
`dlmalloc::malloc + 125`.

That is the same shape as the `MscDevice::block` fingerprint one level up: not a
random word, but the same code reaching the same wrong place. A `TaskShared`
pointer read as zero is a `Msg` or a `RunToken` whose target has been cleared,
and `transition` is on the pass path.

### Two cautions for the next reader

* **`report_contexts` calls a stale pointer an idle context.** Three captures in
  this session show `cpu1 is on ctx 0x… (its idle context) stack_top=0x0 …` with
  garbage in `saved_rsp` — `0x480011f801050000`, `0x4800123c41050000` — and one
  with a nonzero `stack_top` under the report's own "ZERO BY CONSTRUCTION"
  warning. They are not the idle context. Idle contexts are boxed together and
  sit within `0x90` of one another (`cpu-7-has-no-cpusched-returned-on-kvm.md`
  shows `0x…1900310` and `0x…19003a0`); these sit 45 MB away, in the region
  where *task* contexts live, and they read as idle only because `id: None` is a
  niche a released record's bytes can satisfy. `RUNNING_CTX[cpu]` is not cleared
  when a task is reaped, so what the report walked was a freed record. Nothing
  is wrong with reading it — the direct map stays mapped — but the rendering
  invites exactly the misreading the report's own comment warns about, one field
  over.
* **`0x4800123c41050000` recurs across two independent boots in two arms**, once
  in `rbx` and once at a released context's `rsp` offset. That is a recycled
  allocation holding the same bytes, not necessarily a second deterministic
  writer.

### What is owed now

* **Re-measure the two entries above this one.** Every "no deaths" in this file
  was counted with a marker-only storm. The bands' arms in particular have to be
  re-run counting silent guests before "displacement suppresses the class" can
  stand at all.
* **`pass-spin` at `HOLD_NS`, then at ten times it.** The amplifier is time on
  the pass path, and the lock arm has cleared the lock.
* **The seven words between the check and the `ret`.** `check_switch_frame`
  tests `ctx.rsp` and `context_switch` reads the frame through it a few
  instructions later; 18,064 boots say the value the check sees is always sound,
  and one parked capture says the value the `popfq` used was not. Testing the
  *frame* rather than the pointer — the seven saved registers and the return
  slot, against the stack they must lie in — is the next narrowing, and it is
  the only one this session leaves that has not been measured to zero.
* **`check_switch_frame`'s third test is worth the shipping kernel even so.** It
  is two compares on the switch path. It is behind `stack-witness` only because
  a scheduler change owes the two checks the root `CLAUDE.md` requires, and this
  session has a negative control for neither.
* **Any mechanism proposed for this class must also explain
  `cpu-7-has-no-cpusched-returned-on-kvm.md`** — the first sighting on real
  silicon, under KVM, from `idle_loop` during AP bring-up on a loaded machine.
  Nothing found here is TCG-specific: a `popfq`/`ret` off a frame that is not a
  stack, and a return address in a data field, are architecture-neutral shapes.
