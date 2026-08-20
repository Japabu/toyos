---
status: assigned
kind: track
opened: 2026-08-12
---

# Every wait in this kernel is a spin, and a killed task dies by having its stack discarded

Four wait sites still spin — two in xHCI, one in NVMe, one in virtio — so a CPU
waits for a device. A kill is answered by throwing the task's kernel stack away
at five separate reap-in-place arms, and the parked arm is the one that strands
a held lock. Every duration in the kernel is a bare number. None of that is true
on `main`.

**The first five chunks landed 2026-08-19 as #91** (`a5ccf14`): duration kinds,
the completion core, the one park site with the cancellable kill, the sleep
lock, and `usbd`/`iod`. **Owner ruling 2026-08-16: this work lands as two pull
requests, split after the kernel-thread chunk**, so #91 was the first and
everything from xHCI async onwards is the second. The split is at the last
boundary where no lock has converted yet; taking it anywhere later means landing
a tree in which one of the four locks is a sleep lock and the other three are
not, which the baseline assertion refuses by design.

**The second pull request is #148, and it carries the owed tripwire rather than
the chunk.** `block::OPERATION` is landed — a two-second budget over one whole
block-device operation, minted by `UsbBlockDevice` and honoured between commands
in `XhciController::scsi` — which discharges the open decision recorded below
and is what has to exist before a transfer bound can go. The conversion itself
did not land; what stopped it is three walls, established rather than guessed,
recorded under "What xHCI async costs, measured against the tree" so nobody
derives them a third time.

**The third pull request is #152, and it carries the third wall's answer.**
Owner ruling 1B is landed as `scheduler::Operation`: `Parkable::of_current` is
deleted, the type has no public constructor, and the two doors that replace it
each refuse the other's context. The first two walls did **not** land, and the
reason is one sentence: **the park cannot exist before all four locks convert,
and no partial conversion is legal.** At `wait_transfer` on the filesystem path
the preempt count is `vfs::VFS` + `fat32_adapter::VOLUMES` + `xhci::XHCI` above
its baseline, so `Parkable`'s assertion fails by construction until the three go
together — which is the same argument §21.1 makes and which the borrow-chain
rewrite (wall 1) and the per-disk claim (wall 2) both exist only to *serve*.
Landing either of them alone is a rewrite with no park behind it and a claim
with nothing to exclude, which is the technical debt this tree deletes. What
#152 adds instead is the design each of them now has, below, and two findings
the previous pass did not have.

**The commitment: one completion primitive, one inbox, one park site, one
recheck predicate, and a kill answered by `Cancelled` at the park rather than by
discarding the stack.** This kernel does not unwind, so a killed task holding a
live kernel stack must be schedulable at every safe point and must die by
returning through that stack. Cancellable waits come before sleep locks, and
both before any lock conversion; the order is forced, not preferred.

## The chunks, and the invariant each must preserve

- **Duration kinds.** Every duration carries a kind whose constructor demands
  what justifies it, and a number nobody can cite is a tripwire or does not
  exist.
- **The completion core behind the existing wait queue.** A poster stores its
  record under the subject's own lock before it claims the waiter, so a parker
  that publishes its intent and then reads its inbox can never miss a post.
- **The one park site, and the cancellable kill.** Inseparable. A killed task
  holding a live kernel stack is schedulable at every safe point and dies by
  returning through that stack, so no guard is ever abandoned.
- **The sleep lock.** A sleep-lock holder stays preemptible and raises no
  preempt count, so the baseline assertion keeps meaning exactly "a spinlock is
  held".
- **`usbd` and `iod` on the existing kernel-thread machinery.** No housekeeping
  thread's wait can stop another's, and a panic inside one is recoverable rather
  than a halted machine. Three threads, not one, because a stuck USB enumeration
  must not stop the log.
- **xHCI async, and the four lock conversions.** Inseparable. A CPU never waits
  for a device: the lock is dropped before the park, and a completion is matched
  to its asker by identity, never by arrival order.
- **The idle loop's declared end state.** A CPU halts only when nothing is
  runnable and no deadline is armed, and the halt condition is a declared set a
  test enumerates.
- **A poll kind for registers with no interrupt behind them.** Such a register
  is re-read at a declared cadence inside a declared bound, written once.
- **Blocking syscalls, absolute deadlines, the ring as an inbox.** A deadline is
  absolute and total over its whole range, so no value means "no timeout" and no
  site can silently turn block-forever into return-immediately.
- **The write-back queue.** A file's dirty pages outlive the handle that dirtied
  them until write-back reports complete, and a re-open before that sees the
  pages and not the device.
- **The deletion commit and its source gate.** A spin exists only at a named
  site with a stated reason, and shrinking that list is the only way to claim a
  spin was removed.
- **The interleaved A/B.** The wake number is believable only beside a positive
  assertion that the log still got written — a log that stopped improves it
  identically.
- **Widening the pass-cost window.** It starts where the pass starts, and it is
  turned on only against its own baseline.

## Decisions already made, so they are not re-argued

- A watch is a node the waiter lends to the object, and the subject is a
  borrowed reference, never an id. **Rejected:** a global registry, a slot
  arena, two park channels, posting from interrupt context, multishot polls,
  userspace-only blocking wrappers, a sleep lock that spins where it cannot
  park, poisoning, and shootdown-as-completion. A freed object cannot be named.
- The park token proves the *context* may park and never encodes which locks are
  held. **A `&mut` token is rejected**: it forbids the held-across-a-park shape
  the whole refactor exists to create, and two stacked sleep locks need the
  borrow to stack.
- A spinlock held across a park stays a *runtime* named panic. The type system
  cannot see it, and raising a baseline to clear a red converts a boot failure
  into a field investigation.
- Readiness is level or edge and the class belongs to the subject. **The
  machine's log is edge by necessity**: no reader cursor exists in the kernel,
  and one locked read-modify-write per log line costs **350 ms of boot under
  TCG** (497–504 ms became 812–839 ms), which forbids moving the post to the
  producer.
- The cancel is one-shot, consumed by the wait that reports it; the sticky kill
  bit is what terminates, and a second cancel to one thread panics at the call
  site. **Owed before implementation:** a killed thread cannot park at all
  today, so teardown needs a non-cancellable park, a scoped clear, or a commit
  that distinguishes the two.
- Four locks convert and there is no fifth. Blast radius: 29 filesystem sites,
  68 process-data sites, 259 lock calls over 45 statics. **The token cannot be
  threaded through the block-device trait**, which lives in a pure host-tested
  crate.

  **Six, and the count above is the xHCI chunk's rather than the machine's.**
  Checked 2026-08-20: the four are `vfs::VFS`, `fat32_adapter::VOLUMES`,
  `xhci::XHCI` and `process::ProcessData`, and that is exact for the USB path,
  because `FatDevice` owns its `Box<dyn BlockDevice>` outright and a FAT read
  never touches the page cache. The **NVMe** path is
  `page_cache::BLOCK_CACHE` → `page_cache::BLOCK_DEV` → `NvmeDisk::read_blocks`,
  and both of those are `sync::Lock`s held across the whole device round trip
  (`raw_block_read`, `raw_block_write`, `PageCacheGuard::cache_and_dev`; the
  order is stated in `page_cache.rs` and never reversed). So whoever converts
  the NVMe wait inherits two more statics, and neither is a leaf: `BLOCK_CACHE`
  guards a `HashMap` and a slot vector every btree walk touches, and `BLOCK_DEV`
  guards the `Box<dyn BlockDevice>` whose trait provably cannot carry a park
  token. Nobody may read "there is no fifth" as a property of the kernel.
- **A bulk transfer has zero real cancellers once the transfer bound is
  deleted** — the reset-recovery path's only trigger *is* that bound. So the
  largest open decision is a tripwire on the transfer against a budget at the
  filesystem layer, and deleting the bound with nothing in its place makes a
  shipped daemon's give-up policy silently unreachable. **Decided and landed in
  #148**, and the shape it took corrects the premise in one place: the budget
  belongs to the *block-device operation* and not to a filesystem operation,
  because that is the layer at which one call is one operation and the layer
  above it cannot reach the driver at all (see the third wall below).
  `block::OPERATION` bounds the composition `USB_TIMEOUT_NS` cannot see — the
  batching, the retries and the recoveries one `read_blocks` is made of — and
  the daemon it decides is `/bin/logd`, whose `LOG_WRITE_BUDGET` is measured in
  userland around a syscall and so is reachable only if the syscall returns. Its
  doc named `USB_TIMEOUT_NS` as what made that so, which was true of a dead
  device and never of a slow one; both bounds are now named there. **What is
  still owed at the conversion**: the park in `wait_transfer` takes its
  `Deadline` from `min(the transfer bound, until)`, so the operation's budget
  becomes the canceller rather than merely the refuser of the *next* command.
- Owner ruling on order: endowment, then the log, then completions.

## What xHCI async costs, measured against the tree

Read off the tree on 2026-08-20 while #148 was being written, so that the next
attempt starts from these rather than from the chunk list's one sentence. None
of the three is an argument against the chunk; each is work the chunk has to
contain.

- **The borrow chain is the refactor.** "The lock is dropped before the park"
  means the `&mut XhciController` the guard hands out may not be live across the
  park — and at the moment a transfer is waited for it is live eight frames
  below the acquire: `with_disk` → `msc_read` → `with_storage` →
  `transfer_blocks` → `scsi` → `bot` → `framed_phase` → `bulk` →
  `wait_transfer`. Every one of those takes `&mut self` (or hands one on).
  Making the park legal means they stop taking it and take a handle
  that can re-derive the controller after a re-acquire, which is a rewrite of
  `wait/msc.rs`, `wait/mod.rs` and the control-transfer half of `device.rs`
  rather than a conversion of `XHCI`'s declaration.

  **The shape it takes, read off the tree on 2026-08-20.** A session value
  naming `(controller index, pool block, disk number)`, with a `with(|ctrl, dev|
  …)` that re-acquires and re-derives for one short non-blocking critical
  section, and a `wait` that holds none of it. The `MscDevice` copy stays on the
  caller's stack for the whole operation rather than being written back per
  step, which is what wall 2's claim makes safe. Every device-touching step —
  the TRB enqueue, the doorbell, the DMA copies, the recovery commands — goes
  inside a `with`; every wait goes outside one.

  **Two things the rewrite gets for free and one it does not.** Free: the
  outstanding-operation slot already exists and is already identity-matched and
  deadline-carrying — `toyos_xhci::job::Outstanding<W>`, pure and host-tested,
  is exactly "what ends the wait, when the wait stops being worth having, and
  the answer once one arrives", and the MSC path needs one per pool block beside
  the controller-wide one the port machine owns. Also free: something already
  drains the event ring while a waiter is parked — `poll_if_pending` runs from
  `drain_irqs` at the top of every pass on every CPU, on a `try_lock`, and the
  xHCI ISR's `need_resched` is what makes a pass happen. Not free: **`Await::Transfer`
  is `{ slot, dci }` and matches by endpoint, not by TRB.** Its own sibling arm's
  doc says why that is wrong — "matching on anything coarser hands a command that
  ran out its deadline and answered afterwards to whatever asked next" — and the
  transfer arm does the coarser thing today, with `Stages::DataThenStatus`
  standing in for the ambiguity a control transfer's two completions on one
  endpoint create. Closing it means the arm carries the TRB address, which is a
  change in the pure crate, its host suite, `dispatch_event`, `device.rs`,
  `boot.rs` and both waits.
- **There is no per-disk exclusion to drop the lock under.**
  `XhciController::with_storage` takes the `MscDevice` out of the pool block by
  `Copy`, works on the copy and writes it back — deliberately, so a command can
  borrow the controller and the device's rings at once. Today the `XHCI` guard
  is what makes that safe. Drop it mid-transfer and a second CPU entering
  `with_disk` for the same index reads the *stale* copy and enqueues its own TRBs
  on the same endpoint ring. A per-block claim is new design the chunk owes, and
  it is not implied by "convert the lock".

  **And the claim has a second job the first pass did not name: it has to stop
  the teardown.** Today `XHCI` held for the whole operation is also what stops
  `teardown_port` → `release_blocks` reclaiming a pool block, and
  `slot_gone`/`let_go` clearing `msc[at].disk`, underneath a transfer. Drop the
  lock and an unplug on another CPU can do both while the session's stack copy
  of the `MscDevice` still names the block's rings and its DMA window. So a
  claim that only excludes a second *reader* is not enough: the unplug path has
  to see the claim and either defer or mark, and the session has to notice on
  its next re-derivation. Pulling the boot stick mid-write is the test that
  exists for it (`usb_boot_stick_pulled`).
- **Neither the token nor the deadline can be threaded to the leaf, and the
  answer has to be one answer.** `BlockAccess::read_at` (`toyos-fat32`, a pure
  host-tested crate) is the frame that takes `VOLUMES`, and `BlockDevice` is a
  kernel trait but its implementors are reached from a `&mut self` that knows
  nothing of the caller. So a leaf acquire cannot receive a `&Parkable` by
  argument. It can still *mint* one — after the conversion a `SleepGuard` raises
  no preempt count, so `Parkable::of_current` at that depth asserts exactly the
  right thing and succeeds — but `scheduler::Parkable`'s own header claims the
  opposite ("a function with no `Parkable` in scope cannot park… none of them
  can make one"), which is a discipline rule and not a compile-time one, since
  `of_current` is `pub`. **The owner's call, and it is one decision covering
  both values**: either the leaves mint and that header is corrected to say what
  it really buys, or the leaves recover both the token and the operation's
  deadline from the running task, which needs a word on `TaskHandle` and makes
  the ambient recovery explicit. #148 sidestepped it by minting the deadline one
  frame *above* the trait, in `UsbBlockDevice`, which works for a value and
  cannot work for the park token.

  **Owner ruling 1B, 2026-08-20: the leaves receive, and a leaf never mints.**
  `Parkable::of_current` at a leaf is forbidden, and `scheduler::Parkable`'s
  header claim becomes enforced truth rather than discipline: the frame that
  owns the operation establishes parkability once, the depth reads the token and
  the deadline off the running task, and a depth that asks without an
  establishment above it is a loud refusal. **Landed in #152 as
  `scheduler::Operation`, and the wall is closed.** `of_current` is deleted and
  `Parkable` has no public constructor, so the forbidden call does not compile;
  `Parkable::at_entry` refuses inside an establishment and `Operation::parkable`
  refuses outside one. Three things the implementation settled that the ruling
  left open:

  * **The word has two homes.** A task's is on its `TaskHandle`, so an operation
    survives the migration a sleep-lock holder can take mid-transfer; a context
    with no task — boot, and an idle CPU's pass — gets one slot per CPU, because
    it has no handle and cannot be moved off its CPU. `sleeplock`'s `NOT_A_TASK`
    is the same split.
  * **Establishments nest and an inner one may only narrow**, which refusing to
    nest would have forbidden. `fat32_adapter::VOLUMES` is taken *above*
    `BlockDevice`, so the frame that must establish park permission on the
    filesystem path sits above the frame that owns the block-device deadline:
    the two are nested by construction. What nesting may not do is widen — an
    inner establishment takes the earlier of the two deadlines — so nobody buys
    device time by starting a second operation inside the first.
  * **The deadline is the live half and the token is not.** `block::OPERATION`
    is established by `UsbBlockDevice`'s three trait methods and recovered by
    `msc_read`/`msc_write`/`msc_flush`, so `storage_read`, `storage_write` and
    `storage_flush` lose an argument; `scsi` keeps its `until` parameter, which
    is what leaves it usable by `bring_up` — an enumeration is not a
    block-device operation, has no establishment, and passes `Deadline::never`
    by name. `Operation::parkable` carries a named `allow(dead_code)` until the
    conversion, in the shape `sleeplock.rs` already uses for the same reason.

## What the conversion still owes, in the order it has to be done

Read off the tree on 2026-08-20 with wall 3 landed. The whole of it is one pull
request, because the baseline assertion refuses any split.

1. **Wall 2's claim on the pool block**, including the teardown half above.
2. **Wall 1's session**, including `Await::Transfer` gaining the TRB address.
3. **The four locks together** — `vfs::VFS`, `fat32_adapter::VOLUMES`,
   `xhci::XHCI`, `process::ProcessData`. Counted 2026-08-20: 30 `vfs::lock()`
   sites (14 in `arch/syscall.rs`, 9 in `main.rs` which are boot and become
   `try_lock`, 3 in `loader`, 3 in `object/`), 14 in `fat32_adapter.rs`, 4 on
   `XHCI`, 26 on `PROCESS_TABLE`. Everything but `FatVolume::{read_at, write_at}`
   can take a `&Parkable` by argument; those two are the ambient recovery wall 3
   exists for, so the establishment for park permission goes at the syscall
   boundary and `block::OPERATION`'s narrows inside it.
4. **The park itself**, and only then the track's owed `min(the transfer bound,
   until)`. **Not before**, and the reason is in `XhciController::scsi`'s own
   doc: ending a *spin* at the caller's deadline abandons a transfer the device
   is still going to answer, and the recovery then reads the wreckage as a
   device that is not recovering — a slow disk marked permanently offline for
   having been slow. At a park, waking on the operation's deadline is not
   abandoning anything: the waiter wakes and *then* decides. So the owed item is
   a property of the park and lands with it.

Both findings from the 2026-08-20 pass are closed. Kernel clippy runs with the
actuator features on. The other — the two page-cache locks the count above did
not name — is now the paragraph under "Four locks convert and there is no
fifth", and the deadline half of it landed the same day:
`nvme::Queue::wait_completion` spun with nothing bounding it at all, and now
takes `drivers/nvme.rs`'s `COMMAND` inside a command and `block::OPERATION`
between two, exactly as the USB path does. The **conversion** of `BLOCK_CACHE`
and `BLOCK_DEV` did not land and is not in the list above; it is the NVMe
chunk's, and it is owed on top of the four.

`drain_zero_handles`'s derived constraint — none of its three drain sites can
park, so no `on_zero_handles` hook may take a sleep lock — is **untouched by
#148 and by #152**, because neither converts a lock. Its ground was checked rather than
assumed: the hook that would want the VFS is a file's, and `File` is an
`immediate` row whose hook is empty; the flush rides `OpenFileState::drop` on
the last `Arc` instead. So the first `deferred` row that wants the VFS is still
hypothetical, and the constraint still costs nothing to keep.

Measured, and worth carrying: a 2 ms-per-transfer stick takes the worst wake
from **7,117 µs to 165,948 µs** at smp=1, and 6,174 µs to 250,912 µs under load
at smp=8. The audio period is 2.902 ms against a 23.219 ms pipeline. The
scheduler migration cost about **70 defects** in code whose own suites were
green, which is the calibration for how this one is landed.

Six entries under `issues/design-debt/` recorded that the deleted document's own
citations had rotted — five against the tree, one against a log plan deleted
before it. All six closed with it.
