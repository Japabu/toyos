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
