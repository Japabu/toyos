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

The companion arm — the same tree with #180's xHCI diff applied, same shape,
same session, immediately afterwards — was **1,680 boots, zero deaths**. One in
1,716 against zero in 1,680 separates nothing: both arms are consistent with the
same rare defect, and nothing in #180 touches the scheduler. It is recorded here
so the next reader does not read the clean arm as a fix.

The full capture is not in the tree; the lines above are the whole of what the
serial log carried past `Boot: complete`, and the boot before the panic is
ordinary — `Boot: complete (335ms)`, klogd/usbd/iod up, logd, compositor,
soundd, netd and filepicker spawned in order, no earlier warning of any kind.

## What is owed

A reader, not a fix. The two sightings agree on "something wrote a per-CPU
scheduler container from outside", disagree on every particular, and the closed
entry's reconstruction is the only candidate mechanism anyone has written down.
Either it is incomplete or there is a second one, and one death per ~1,700 boots
is a rate a boot storm can measure — so the instrument to settle it exists and
has been run once.
