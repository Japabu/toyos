---
status: open
kind: defect
opened: 2026-08-06
---

# Pulling the boot stick freezes the T14, and the diagnosis that was wrong

**The report.** Pulling the USB stick while the desktop is up freezes the whole
machine unrecoverably, from a USB-A or a USB-C port alike, and **Ctrl+Alt+D does
not answer afterwards**. That last clause is the strongest signal available: the
blocked-task dump is dispatched from `drain_irqs` at the top of a scheduler
pass, so no CPU is reaching a pass — not three of eight as in the wedge, all of
them.

**A diagnosis to withdraw, recorded because it read well.** The mechanism first
proposed was: every CPU entering a pass takes `XHCI`, one holds it across a full
blocking teardown with 2 s waits against a device that cannot answer, and the
rest spin. **It does not survive its own prediction.** `sync::Lock::lock` logs
`LOCK CONTENTION: {N}M spins` at 50M and panics `DEADLOCK` at 500M
(`kernel/src/sync.rs`); a `pause` iteration is tens of cycles, so a CPU waiting
behind one 2 s hold passes the warning and one behind two approaches the panic.
The owner reports a freeze with neither a contention line nor a panic screen. So
"every CPU spins on the ticket for seconds" is not what happened.

What the code still supports, and all it supports: **one CPU holding `XHCI` for
the transfer budget per SCSI command against a device that has gone.** Whether
anything else was spinning behind it is not settled by any evidence in hand.

**The residual that makes this hard, as a category.** The evidence channel is
the thing that fails. `/log` is on the stick being pulled, so the event that
would be diagnosed destroys its own record — a contention line goes into a ring
drained to a file on a device that is no longer there, and the T14 has no serial
port. **A defect whose evidence channel is the failing component cannot be
investigated by reading the log afterwards**, and this will not be the last one:
any device carrying `/log` has the same shape. What would break it is a channel
that does not depend on the storage stack — the on-screen panic console covers a
panic, and this is not one.

**What `c4ba7d5` closes.** The amplifier every candidate path shares.
`wait_transfer` ended on the clock; it now ends on the register when the slot's
port reads disconnected, because a device that has been unplugged is not a
device that is slow. A filesystem sync, a page-cache fill, a teardown and a
scheduler pass all reach that function, and pulling the stick a machine logs to
aims all of them at a dead device on one event.

**What it does not close, stated so a green suite does not imply otherwise:**

- ~~Teardown and `recover_endpoints` still block a pass~~ — **closed by X2a**
  (`specs/xhci-port-machine-plan.md`). Both are submit-and-return against one
  outstanding operation per controller: the pass that starts one gives itself
  back, and the completion arrives through the event ring the poll already
  drains. What is left on that path is `device::configure`, which is X2b; the
  type split that would make a wait there a compile error belongs with it,
  because a view that still has to hand `poll` a route to `configure` is a
  signature promising a check it does not perform. Two costs moved rather than
  going away, and neither is a defect: `PORT_WORK_AT` carries the outstanding
  operation's deadline, so an idle CPU declines to halt across a teardown
  exactly as it already does across a debounce, and a teardown now takes one
  further scheduler pass.
- **The metal claim is still the owner's to make.** Everything above is the
  guest-side proxy — no pass blocks — and the acceptance test is a stick pulled
  out of a running T14 with Ctrl+Alt+D still answering.
- `log_file`'s flush still holds `SINK` and the VFS across device I/O. The doc's
  "unbounded and uninterruptible" is half right, and the precise reading is
  **bounded in acquisition, unbounded in work**: `poll` is `try_lock` on both and
  disables the sink after `MAX_BLOCKED_NANOS`, so it never waits for a lock — but
  `Sink::flush` then calls `vfs.flush_file` and `vfs.sync_mount`, which reach
  `msc_write`/`msc_flush`, which take `XHCI` and spend the transfer budget per
  command.
- ~~There is no gate for the dangerous window~~ — there is now a gate for the
  *pull*, `usb_boot_stick_pulled`, and what it certifies is below. It is still
  not a gate for the 100 ms debounce and still cannot be aimed inside it.

**A negative result worth keeping.** The change did not make
`desktop_window_child` green; it stayed red across two landing gates. That is
evidence *against* the desktop freeze and the unplug freeze sharing the xHCI
path, and it agrees with the scheduler track's independent exclusion of the
ticket lock — two tracks reaching the same exclusion from different directions.

#### The instrument, and what it showed

`usb_boot_stick_pulled` (`tests/common/usb.rs`) is the first reproduction
attempt anywhere but the owner's desk. The boot stick had no QEMU device id —
every earlier unplug test names a *data* disk, which carries neither `/boot` nor
`/log` — so `qemu::BOOT_STICK_ID` is new and the pull is `device_del` on it. It
boots metalcase on `Profile::Metal` at eight cores with `log-rotate-fast`, so the
sink creates, sweeps, deletes and syncs rather than only appending; it drives a
drumbeat of `run` lines through test-runner, each a userland `println!` into the
ring the sink drains to the stick and a VFS walk for a binary that is not there;
it pulls the stick mid-drumbeat; and then it plugs one back in. The verdicts are
two liveness ceilings on paths that share no mechanism — the compositor's 2 s
frame report, which is the owner's clock that stops advancing, and a console
round trip that comes back through the VFS. A red prints `freeze_report`:
`info registers -a` first, Ctrl+Alt+D second.

**It is green, and the hazard was staged rather than missed.** The pull landed
with a write outstanding — `transport broke on SCSI 0x2a: no answer in the
command phase`, then `slot 1 would not take a Bulk-Only Reset`, then
`reset recovery failed; disk is offline`, all 89 ms *before* `xHCI: port 1
disconnected` — which is the device-gone-teardown-not-yet-run window the entry
above says cannot be aimed at. It was hit by accident and survived: 40/40 console
probes answered and the desktop kept drawing, and 40/40 again after the replug.

So the finding is that **QEMU does not reproduce it**, and the reason is
visible: `c4ba7d5` ends a transfer when the slot's port reads disconnected, and
`device_del` drops CCS at once, so nothing here ever spends the 2 s budget. What
follows is therefore read out of the code rather than watched.

#### One hypothesis killed by the code, on the machine that matters

**`drain_serial`'s `BackendGuard::lock` cannot be the T14's freeze.** That
machine has no 16550 and no virtio-console, so `serial::has_console()` is false
for the whole boot and `log_ring::set_serial_sink(false)` pins `LogRing::len` at
zero. `drain_chunk_to_serial` therefore returns 0 on its first call,
`drain_serial` leaves its loop, and the backend guard is held for one memcpy of
nothing. What makes that lock dangerous — unbounded work with interrupts off —
needs a backend, and that machine has none.

#### The mechanism the code does support, end to end

A chain, each link with the line that carries it. Nothing here has been watched;
every link is checkable by reading.

1. **One `XHCI` ticket lock serves every controller**
   (`drivers/xhci/mod.rs`, `static XHCI: Lock<Vec<XhciController>>`), and
   `storage_write`/`storage_flush` take it from whatever thread is writing — the
   idle loop's `log_file::poll`, a page-cache fill, a syscall.
2. **`poll_if_pending` takes the same lock at the top of every scheduler pass.**
   X2a made the *work* behind it submit-and-return; the *acquisition* is still a
   blocking spin against whoever holds it. A stick that has gone leaves port work
   pending, so every CPU entering `drain_irqs` reaches that `XHCI.lock()`.
3. **`Lock::lock` spins with interrupts enabled.** `preempt::disable()` is a
   per-CPU counter and touches nothing in `RFLAGS`. A convoy on this lock
   therefore reads `HLT=0` with `IF` **set** — neither of the two answers the
   #156 capture taught us to look for.
4. **At 500M spins it panics `DEADLOCK`.** On a CPU running a userland thread
   inside a syscall, `main.rs`'s `#[panic_handler]` takes the branch
   `percpu::syscall_rip() != 0 && percpu::current_tid().is_some()`, which calls
   `discard_capture()` and then `try_recover_from_panic()`. **A recovering panic
   never paints**, and on a machine with no console `panic_flush` has already
   returned early on `!has_console()`. The report exists in the ring and is
   discarded by the code that decided the panic was survivable.
5. **And the ticket is stranded.** `Lock::lock` does `ticket.fetch_add(1)` before
   it spins, and `now` is advanced only by `LockGuard::drop` — a thread that
   panics inside the spin never constructs a guard. Poisoning it leaves `now`
   permanently one short of every ticket behind it, so **every later acquirer of
   that lock waits forever, machine-wide, for the rest of the boot.**

The chain ends where the owner's machine does: nothing scheduled because no CPU
completes a pass, the clock stopped, the keyboard dead because a scancode is
decoded by `i8042::service` inside `drain_irqs`, Ctrl+Alt+D silent because
`keyboard::take_dump_request` is read from the same place, no panic screen, and
no log anywhere.

**This also repairs the argument that withdrew the ticket-lock diagnosis.** That
withdrawal rested on the owner seeing no `LOCK CONTENTION` line. On the T14 that
line is unobservable by construction — it goes to the log ring, whose serial sink
does not exist and whose file sink is on the stick that was just pulled — and the
absence of a line that cannot be printed is not evidence. What the withdrawal got
right is the missing *panic screen*, and step 4 accounts for that too.

**Two defects, either worth fixing on its own — and neither is this bug.** The
metal result below eliminates the chain as *the* mechanism; these stand on their
own reading and stay open.

- **A deadlock panic is classified recoverable because a syscall happens to be in
  progress.** The predicate asks whether a userland thread is current, not
  whether the kernel can continue. For `Lock::lock`'s own deadlock it
  demonstrably cannot, and the handler's response is to throw away the on-screen
  report and make the deadlock permanent.
- **A ticket lock cannot survive an abandoned waiter.** `sync.rs`'s
  `ticket.fetch_add(1)` happens before the spin and `now` advances only in
  `LockGuard::drop`, so a waiter that panics inside the spin never constructs a
  guard and its ticket is never served: the lock is unacquirable for the rest of
  the boot, machine-wide, with no diagnostic at all. The queue form of the
  "locks a dead thread can strand" class in `specs/issues/panic-path/`, and worse than the held-guard
  form — the abandoning thread never held the lock, so nothing in the code reads
  as if it owned anything to release. Whatever closes it should make the failure
  loud and terminal rather than silent and permanent, and note that a fix whose
  only signal is a log line is invisible on the machine that needs it.

**And one removed here.** `poll_if_pending` took `XHCI` with `lock()` at the top
of every scheduler pass on every CPU, and while a port has work outstanding
every CPU finds it due — so the acquisition alone put as many CPUs as the
machine has on one ticket queue, each spinning with preemption disabled. It is a
`try_lock` now: the CPU holding that lock is doing precisely the work the
declining CPU came to do, so waiting for it buys a second look at a state
somebody else has already advanced. The `irq_ring` record is consumed only after
the lock is held, because `take` clears a slot an ISR coalesces into and a CPU
that took its record and then declined would have dropped a wake.

#### 2026-08-07: the metal result, and what it eliminates

The owner built at `8cfb6d8` — a clean tree carrying **X2a and X2b both** — and
pulled the stick. **Froze. Ctrl+Alt+D nothing. He then sat untouched for a full
minute and the panel did not change at all**; the desktop image stayed as it was.

**X2a and X2b are eliminated as the fix and are not eliminated as correct work.**
They removed real unbounded waits from the scheduler pass, their gates stand, and
the freeze is unchanged across them. What that buys is a clean narrowing: the
remaining xHCI candidate is the **acquisition** of the one controller lock rather
than the work done under it.

**The minute of nothing is the interesting half, and the arithmetic behind it.**
500M spins is seconds, not minutes, so a convoy would have reached
`Lock::lock`'s DEADLOCK panic dozens of times over. `main.rs`'s recovery branch
is per-CPU and conditional — `syscall_rip() != 0 && current_tid().is_some()` —
and an idle CPU satisfies neither clause, so it falls through to
`halt_all_cpus`, which paints. On eight cores running three processes, several
CPUs are idle. Something should have appeared. Two of the three explanations are
now closed:

- **"The panic console cannot take the screen from a compositor."** False, and
  now gated. `SCREEN_OWNED_BY_USERLAND` stops *boot checkpoints* and nothing
  else; GOP's `set_resolution` always returns `NotSupported`, so
  `gpu::set_resolution`'s missing `rearm()` on the success path never fires on
  this machine; and `panic_console::disable()` is virtio-gpu's alone.
  `screen_fatal_halt_composited` now boots metalcase with a compositor holding
  the panel and asserts the fatal report lands on it.
- **"A fatal panic on an idle CPU reports itself."** It did not, and that is
  fixed in this branch. `idle_loop` is entered by `jmp`, so its frame is the
  topmost on the 16 KiB idle stack and `rbp + 8` — which `kernel_backtrace`
  reads before checking it — was the unmapped page above. Every fatal panic on
  an idle CPU faulted inside `crash_report` while printing its own backtrace,
  the fault's report faulted the same way, and the chain ended in a double fault
  and `PANIC REENTRY`. Measured: **seven pages of cascade with no line saying
  what panicked, against one panic and three pages afterwards.**

So the "nothing paints" clause in the chain above survives, with a different and
checkable cause than the one it was written with — not the recovery branch, but
a report that could not be printed from the stack it was raised on. **What that
does not establish is that a deadlock panic is what happened**; it establishes
only that if one had, the owner would have seen a fault cascade or nothing, and
never the reason.

**A latent defect found on the way**, not live and worth an entry: on the
success path `gpu::set_resolution` calls `panic_console::detach()` and never
`rearm()`, so any driver whose resolution change *succeeds* blinds the panic
console for the rest of the boot. Unreachable today — GOP always refuses and
virtio-gpu is disabled outright — and one new GPU driver away from being live.

#### 2026-08-07, second metal round: the probe painted, and the convoy is retired

**The probe fired and painted on the T14**, over a compositor that was actively
drawing — the two lines above it on the panel are `compositor: frames=3` and
`frames=2`. Two backtrace frames, chain terminated, no cascade, no
`PANIC REENTRY`. The seven-page cascade is gone on the machine the fix was
written for, and the fatal path is now *proven* to reach that panel.

**That turns the minute of silence into positive evidence, and it retires the
chain above.** A fatal panic on that machine paints. The stick pull produced no
paint in sixty seconds. Therefore **no CPU panicked**, therefore none spun 500M
times on a ticket, therefore the convoy did not happen and neither did the
stranded ticket. The chain is eliminated as *this* bug; both defects it named
stay open on their own reading, because they are real and they are not this.

**What is left is a machine whose CPUs never panic, never schedule and never
answer an interrupt**, and there is exactly one state in this kernel that is all
three: halted with `IF` clear. The audit, all read from the code today:

- **`stub_halt_all` is `cli; 2: hlt; jmp 2b`** (`arch/idt/mod.rs`) — the only
  permanent interrupt-deaf halt in the tree, reached only by the `0xFD` IPI,
  which only `apic::halt_all_cpus` sends.
- **`halt_all_cpus` sends that IPI *before* it renders.** Every sibling is
  halted `IF`-clear at statement one; the paint is statement two. An initiating
  CPU that does not complete `render()` leaves seven CPUs deaf forever, nothing
  on the panel and no panic anywhere — the owner's report exactly.
- **`paint` is latched on `PAINTING`**, so a CPU that finds another mid-paint
  returns having painted nothing. `screen_claimed_by_userland` waits up to 2 s
  on that same latch.
- **Three of `halt_all_cpus`'s callers are not panics**:
  `iommu::vtd::fault::service` — *any* DMA remapping fault stops the machine —
  `scheduler::schedule_no_return`'s panicked-inside-a-pass arm, and `SYS_DEBUG`
  action 3. The first is where to look first: the T14 boots with its unit
  translating, and a device pulled mid-DMA is a way to raise a fault.
- **Checked, and not reproducible here.** `Profile::Metal` declares
  `IOMMU_DEFAULT`, so `usb_boot_stick_pulled` has been pulling the boot stick
  with the unit *translating* all along and no fault fires. That makes it a
  metal-only candidate rather than an untested one.
- **The two `IF`-clear spinlocks have no deadlock detection at all.**
  `log_ring::RingGuard::lock` and `serial::BackendGuard::lock` are `cli` plus an
  unbounded CAS spin: no bound, no contention warning, no panic. `sync::Lock`,
  which keeps interrupts *on* and is therefore survivable, has all three. **The
  machine can detect the deadlock it could live through and cannot detect either
  of the ones it cannot** — and a panic taken while holding `RING_LOCKED`
  deadlocks the panic handler's own first `log!` on that CPU, `IF` already
  clear, before it reaches `capture` or `render`.

#### Boot 1, same image, 79 seconds earlier: possibly #156 on metal

The compositor never came up. `spawned /bin/compositor pid=0`, soundd, netd,
`Boot: complete (1144ms)`, every CPU joining the scheduler — and then the boot
log, sitting there. **No probe panic fired, and by construction that means
nothing ever claimed the framebuffer**, since `CLAIMED_AT` is written only by
`screen_claimed_by_userland`.

**What the photograph cannot say**: the panel carries the boot-complete
checkpoint paint from ~1.15 s, so any userland output after that instant is
absent from it either way. It cannot separate "the compositor wedged before its
first instruction" from "the compositor exited at 1.2 s".

**What the log on the stick decides, and it is one line.** Boot 1 is
`/log/2026-08-07-114354.log`, boot 2 `/log/2026-08-07-114513.log`. Is there an
`exit: compositor` line? If yes it left, and that is an exit to explain. If the
process never appears again at all, that is **#156 on metal, in a boot rather
than after minutes of desktop use** — a process spawned and never given a first
instruction, the shape `specs/issues/kernel/` has been describing from T14 logs and that nothing has
been able to stage. Far cheaper to chase than anything else in this section.

**Two readings off the same photographs that are not defects.** `i8042: armed at
1002ms, idle at 1153ms, 0 interrupts … the pin has never asserted` appears on
both boots: 150 ms after arming with nobody typing, that is the health line
doing its job, and it becomes a finding only if a *later* line in a log still
says zero. And the disk refusal works as designed — `gpt: device 1 has 4
partitions and none of them is ours`, `this disk is not ours and nothing will be
written to it`.

#### 2026-08-07, the logs: the freeze is a *boot*, and USB is not the subject

Both logs came off the stick. **The freeze reproduces at boot, with nothing
unplugged, about one boot in two.** The two boots are 79 seconds apart on one
image, and with timestamps stripped they differ only in the RTC reading, one
millisecond in two phase timings, the SMP interleaving of the `CPU N: joining
scheduler` lines, and the whole divergence: boot 2 has two `compositor: frames=`
reports and boot 1 has none.

Line-for-line identical through the framebuffer claim and past it:

```
shm: 0x4000000000 mapped WriteCombining into pid 0      1.162 s
compositor: wallpaper 1920x1080, scaling to 1920x1080
compositor: ready
spawn: /bin/filepicker pid=3 … cr3=0x1a55000            1.348 s
compositor: at most 221 windows (8 MiB each of 15402 MiB total)
```

Boot 2 continues with `frames=3 … scanout_blits=3`. **Boot 1 emits nothing
further, ever.**

**The probe's absence is what makes this the machine and not the compositor.**
`0x4000000000` is the GOP framebuffer out of `KernelArgs`, so that `shm` line is
the claim, and the claim is the only writer of `CLAIMED_AT`. Probe due at claim
+ 5 s ≈ 6.162 s; boot 2 panicked at **6.164 s**, confirming the mechanism to the
millisecond. Boot 1 shows no panic on the panel or in the log, so **no CPU
reached `probe_due()` in the idle loop after 6.16 s**. A wedged compositor with a
live kernel would have panicked. Every CPU is spinning or halted with `IF`
clear — the audit above, now with evidence.

**Two honesty constraints on reading these logs.** A log file ends at the last
successful flush and not at the moment of death — boot 2's own panic is absent
from its log for exactly that reason — so boot 1's true last event may be later
than its last line. And a *healthy* idle machine would also stop producing lines
here, since the i8042 health line only repeats once the pin has asserted. **It is
the missing probe panic that carries the conclusion, not the missing log lines.**

**Consequences for the rest of this section.** The reproduction is a boot rather
than ten minutes of desktop use, and **the unplug freeze may not be a USB defect
at all** — the same machine state, with nothing pulled out. Treat "pull the
stick" as one trigger of a general defect rather than as the subject. Everything
above about xHCI stands as correct work and none of it is the cause.

**The window is 1.35 s to 6.16 s.** Both ends are anchored on something that
survives: the last line boot 1 wrote, and the probe that was due at claim + 5 s
and did not fire. It is wider than it needs to be, and that is deliberate.

**Withdrawn, and not to be re-derived: the `compositor: frames=` cadence.** An
earlier reading of this pair narrowed the window to ~3.35 s by treating boot 2's
two-second frame-report interval as a clock boot 1 would have obeyed. That
inference is retracted. `frames=3 … scanout_wr_bytes=8370176` records *boot 2*
reaching its main loop and blitting a full screen; it says nothing about how far
boot 1 got, and nothing below may lean on it. The difference between the two
files stays an observation and stops there: boot 2 has two frame reports, boot 1
has none.

So what is in the window is stated as what the machine had just been *told to
do*, never as what it was caught doing. The scanout has been mapped
write-combining, `/bin/filepicker` was spawned one line earlier and is starting
up, and the compositor is entering a main loop whose first act is a full-screen
blit into a WC MMIO mapping. Every desktop test in the suite runs that sequence
in QEMU without freezing, so what differs is timing and the memory type, neither
of which TCG models.

#### The defect that is in that window: a TLB shootdown nobody waits for

`apic::tlb_shootdown()` writes the ICR with vector `0xFE` **and returns**. There
is no acknowledgement anywhere: `tlb::tlb_flush_entry` flushes, EOIs and
`iretq`s, and nothing counts completions. So every caller continues on the
assumption that its siblings have flushed, when a sibling may not do so for an
unbounded time — a CPU inside any `cli` region takes that IPI only when it
re-enables interrupts, and a CPU halted with `IF` clear never takes it at all.

Its eight callers are exactly the operations in the window:
`process.rs:161`, `:196` and `:588` — address-space teardown, which **frees the
physical pages** — `syscall.rs:1443` and `:1825`, and `paging.rs:256`, `:613`,
`:646`, which include the mapping whose *memory type* is being set. The log
shows all three shapes inside two seconds: `exit: netd pid=2` at 1.307 tears an
address space down, the scanout's memory type is set at 1.162, and
`spawn: /bin/filepicker` builds one at 1.348.

Two consequences, and the first is a defect on its own reading whatever the
freeze turns out to be:

- **Unmap-then-free is unsound.** A sibling holding a stale translation writes
  into a page that has already been handed to somebody else. Nothing bounds how
  long that window is.
- **A page is left mapped WC on one CPU and WB in another CPU's stale TLB.**
  That is a memory-type alias on one physical page, which Intel SDM Vol. 3A
  §11.12.4 does not permit and for which it specifies no behaviour. It is
  invisible to TCG, which models no memory types at all, and it is the one thing
  in this window that can stop a machine below the software layer — no panic, no
  schedule, no interrupt.

**Eliminated on the way**: the kernel's *two* mappings of the framebuffer are
not an alias — `gop.rs:66` and `panic_console/mod.rs:391` both map it
`CachePolicy::WriteCombining`, so the panic console and the scanout agree.

**This is a candidate and not a diagnosis.** What makes it worth ranking first
is that it is the only mechanism found so far that is present in the window, is
absent from QEMU by construction, and can produce the observed state without
reaching any software error path.

**Corroborated independently, and the other reading goes further — read it
before touching any of this.** `specs/memory-boundary-spec.md` §2.3 reached the
same conclusion from the memory-safety track on the same day, and it is the
authority for the fix: it names the same `ipi_all_excluding_self` one-ICR-write,
states that **the six existing call sites are therefore already wrong** rather
than merely incomplete — `MappedPages::release` (`process.rs:159-163`) drops the
pages after a shootdown nobody waited for — and enumerates four more sites that
free pages with no shootdown at all (`sys_munmap`, `shared_memory::{release,
destroy, unregister, cleanup_process}`, `virtio_gpu::free_framebuffer`, and
`virtio_gpu::set_resolution`). It also carries a half this entry missed
entirely: `invlpg` reads the *current* CR3's PCID (`paging.rs:196`), so
`shared_memory`'s and `virtio_gpu`'s unmap paths invalidate the wrong tag on
metal and merely the wrong CPU under QEMU.

**And it prices the fix, including a deadlock class this entry did not see.**
§3.3 is stage M3: an acknowledged shootdown with a per-CPU generation counter,
invalidation against the *target* address space's PCID, and the shootdown moved
ahead of every free. Its stated rule matters to anyone who reads the ninth-boot
experiment below as an invitation to write one: **the initiator must not wait
for acks while holding a lock a target could be spinning on with `IF` clear.**
The `IF`-clear windows are `serial.rs:98,114,163` — the serial lock under
`save_and_cli` — and IDT interrupt gates, so no `log!` may sit between issuing a
shootdown and collecting its acks. That is the same `BackendGuard` this entry's
own audit flagged as an unbounded `IF`-clear spin, arriving from the other
direction.

**M3 is memsec2's, not this task's.** The experiment below is an A/B on a
throwaway build to test a hypothesis about the freeze; it is not the fix, and it
must not be confused for the start of M3.

#### What a ninth boot should carry

Not another probe — this one is a fix-shaped A/B, and it is the cheapest
decisive experiment available. **Make the shootdown synchronous**: a per-CPU
acknowledgement counter, the sender spinning until every online sibling has
flushed, bounded, with a loud named failure when the bound is hit. Then eight
boots with it against the eight without that the owner is already collecting.
A rate that goes from about a half to zero is the answer; a rate that does not
move eliminates the strongest candidate in one round and costs one flash.

The bounded failure is half the value: a sibling that does not acknowledge
inside the bound is a CPU that is already `IF`-clear and unreachable, and saying
so *by name* turns the freeze's own precondition into a printed line — on a
machine where, as of the probe, a fatal report reaches the panel.

That change is not made here, and it is **not** M3 being started early: M3 is
`specs/memory-boundary-spec.md` §3.3 and it belongs to the memory-safety track,
which has already priced it, enumerated the sites and written the deadlock rule.
Whoever builds this A/B builds a throwaway to test a hypothesis about the
freeze, obeys §3.3's rule about `log!` between issue and ack, and lands nothing.

**Cheaper still, and it should be tried first**: the heartbeat's `mask=` field
answers a question this experiment would answer expensively. A machine dying to
a stale translation loses CPUs the way a memory corruption does — one at a time,
in whatever order they touch the bad page — while a machine dying to a global
cause loses them between two lines. One flash of an instrument that is already
built beats one flash of a fix that is not. **The first eight boots of it read
`0/8` for a hundred seconds on a healthy machine** and the section below is why;
the field means what it says from `diag-tick` on.

#### What the owner should run — settled

**Already answered: the probe painted.** Kept as the record of what it settled.
No further reflash for that question.
`cargo run -- --console-boot --kernel-param metal-panic-probe --build-only`
(or `--diag-boot`, or the ordinary image — the probe is orthogonal to the boot
mode). Flash it, boot to the desktop, and wait. Five seconds after a process
claims the framebuffer the kernel raises a real fatal panic from an idle CPU —
the same path, the same context and the same stack a machine-stopped panic uses.

- **The panel fills with a panic report** naming
  `metal-panic-probe` → the fatal path works on his hardware, with his
  compositor, on his panel. Every future "nothing appeared" is then evidence
  that no panic happened, and the freeze is not reaching one.
- **The panel does not change** → the T14 has never been able to report a fatal
  panic, three investigations have run blind, and that is worth more than the
  freeze because it is the prerequisite for diagnosing anything else on that
  machine.

`screen_fatal_halt_composited` gates the same feature in QEMU, so a green suite
says the software half works and the reflash asks only about the panel. That
split is deliberate: QEMU's framebuffer is host RAM, while the T14's is a
write-combining MMIO mapping the compositor is also writing and a full paint
measures ~460 ms there.

#### CLOSED — the heartbeat's first build was blind on an idle machine, and the eight boots that proved it

Eight `--console-boot --kernel-param heartbeat --kernel-param
metal-panic-probe` boots, logs at `2026-08-07-15{3347,3603,3819,3854,4108,4203,
4259,4532}.log`. Every one has the same shape:

```
[kernel   1.150 cpu4] heartbeat: t=1.150s passes=8/8 mask=0xff
[kernel   1.782 cpu0] heartbeat: t=1.782s passes=8/8 mask=0xff
[kernel 104.512 cpu0] !!! PANIC !!! metal-panic-probe …
[kernel 104.512 cpu2] heartbeat: t=104.512s passes=1/8 mask=0x01
[kernel 104.512 cpu5] i8042: the pin asserts — 1 interrupts, 1 bytes, 1 keys …
```

**Exactly three heartbeats per boot, in all eight**: two while userland came up,
then nothing for 14 s, 16 s, 31 s, 102 s, 115 s or 121 s depending on the boot,
then one final line in the *same millisecond* as the first keypress. The owner's
account matches — "waited a long time no panic, Fn button definitely causes
panic", "same but for caps lock" — and every trigger he found is an interrupt.

**The finding: after ~1.8 s no CPU takes a scheduler pass at all.** Every CPU's
pass found no work and no deadline, so it stopped its LAPIC timer and halted
until something external arrived. That is correct behaviour and good power
management, and it made the instrument blind at the one job it was built for:
`heartbeat::poll` ran from the idle loop, a halted CPU does not run the idle
loop, and **a healthy quiescent machine and a dead one wrote byte-identical
logs** — the precise failure the heartbeat was built to eliminate. The same
defect blinded the probe, which is why it fired 99 s late and only on a keypress
rather than at its deadline.

**Fixed by `diag-tick`** (`kernel/Cargo.toml`, `KernelHw::idle_wait`,
`apic::arm_within`): a diagnostic build caps how long a CPU may sleep at 100 ms,
taking the minimum against whatever the pass just armed so no wakeup is ever
pushed out. `heartbeat` and `metal-panic-probe` both depend on it. The line now
reads `alive=N/8` with a `gap=` field, and names each missing CPU with how long
it has been silent. Gated by `kernel_heartbeat`, whose teeth are recorded in its
own comment: with the tick removed, 10 of 11 lines dropped a CPU, six of them at
`alive=2/8`, one CPU silent for 2.811 s — and the *old* gate's assertion was a
mask that **varies**, so it was satisfied by the defect and certified it.

**What these eight boots do not establish, and must not be read as:**

- **The desktop freeze was not tested.** The owner ran `--console-boot`. Nothing
  here is evidence about #156's own reproduction.
- **The unplug case cannot be tested with this image at all.** The probe makes
  any interaction panic the machine. Boots 4 and 5 (`153854`, `154108`) are the
  two truncated at 1.8 s with no panic recorded: he pulled the stick, the panic
  fired on the pull, and the log could not be written to a removed stick. The
  machine was then in the halted pager, so *"still responding"* is
  `page_forever` polling the i8042 by design and says nothing about whether the
  unplug would have frozen a running machine.
- **A silent log still means less than death.** A set bit says that CPU took an
  interrupt, returned from `hlt` and reached a pass; the tick that produces the
  next one is re-armed by the timer stub in assembly, so the chain breaks only
  where a CPU stops taking interrupts. If every CPU's LAPIC timer stopped at
  once — a C-state that parks it, an SMI storm — the log would go quiet and read
  as death. The kernel only ever executes `hlt` and programs no C-state MSR, but
  the T14's firmware is not ours, so the honest claim from a stopped log is *the
  machine stopped taking timer interrupts*.
- **The instrument is no longer passive.** Each heartbeat carries a
  `sync_mount` of `/log`, so a `heartbeat` build touches the boot stick four
  times a second and the boot it watches is not quite the boot without it.

#### What the owner should run next — the ninth boot

```
cargo run -- --kernel-param heartbeat --build-only
```

The **ordinary** image and not `--console-boot`: the desktop is what freezes and
the last eight boots did not run it. No `metal-panic-probe` — it makes any
keypress panic the machine, which is what stopped those boots being able to test
the unplug. Flash `target/bootable.img`, boot to the desktop, use it, and pull
the log off `/log` afterwards. **Nothing needs to be touched for the log to be
readable**, which is the whole change:

- **Heartbeats continue at ~250 ms with `alive=8/8` and `ran=` moving, to the
  end of the file** → the machine was still scheduling *and still running
  tasks* when the power went off. Previously indistinguishable from death.
- **Heartbeats continue with `alive=8/8` and `ran=0` line after line** → the
  decisive reading. The scheduler and the interrupt layer are alive and the
  failure is above them — a lost wakeup, or userland wedged — and every
  hypothesis below the software layer is out, including the shootdown. This is
  the case the `tone` boot below makes live, and it is the whole reason the next
  flash is worth making.
- **Heartbeats stop dead** → the machine stopped, at the timestamp of the last
  line ± 250 ms. That is the freeze, with a time on it for the first time.
- **`alive=` falls one CPU at a time over several lines**, each named by a
  `heartbeat: cpuN last reached one …s ago` → a local cause spreading, which is
  what a stale translation looks like: CPUs die in the order they touch the bad
  page. This is the reading §3.3's shootdown hypothesis is waiting on.
- **`alive=` goes from `8/8` to nothing between two lines** → a global cause,
  below the software layer, and the shootdown hypothesis loses its best
  evidence.
- **A `gap=` far larger than 0.250s on a line that is otherwise healthy** → the
  machine went quiet and came back rather than dying. On the eight boots above
  this was the whole file; it should now never happen.

#### The `tone` boot — 86.9 s healthy, and what the heartbeat would and would not have caught

`2026-08-07-174543.log`, 366 lines, ordinary desktop image with **no** heartbeat
feature. The owner typed `tone` into a terminal, deleted a character, let it sit,
and the machine froze; Ctrl+Alt+D did nothing afterwards. His words: *"it felt
like it died idling."* This is the first freeze with a long healthy run, a
precise last event, and a working control in the same capture.

**Observed.** Shell at 5.08 s, compositor reporting `frames=2` every ~2 s
throughout (a blinking cursor), PMM flat at 168/15402 MB across every 10 s
report, every allocator tag steady. Keys counted 4 @13.4 s, 5 @28.6 s, 12
@38.6 s (`last byte at 29115ms`), 13 @86.859 s. The log ends on three lines —
the i8042 counter from cpu0, then `sched: cpu=5` and `sched: cpu=6`, each
`ready=0 parked=1 current=None`. No dump lines at all, so Ctrl+Alt+D never
began.

**The control is real and it matters.** At 28.645 s a key produced the *identical
three lines*, and the machine carried on — the compositor's next window went
`frames=2 → 6` as the character echoed. So the last three lines of this log are
byte-shaped like a healthy keystroke, and nothing in them is a symptom.

**Correction to "died idling": the evidence says it did not.** The compositor's
2 s reports run unbroken to the end — three of them between the 81.229 s PMM
dump and the 86.859 s counter line, at ~1.9 s apart, exactly its healthy
cadence. A machine that died during the 57 s of idle would have stopped
producing those, and the log would show the gap. **It stopped at the 13th
keystroke**, within the flush window of 86.859 s. The owner's impression is
explained without contradicting him: he had stopped typing 57 s earlier, so from
his side the machine had been idle, and the key he pressed to check was the one
that coincided with the stop.

**Second correction, smaller:** `ready_len`/`parked_len` are `try_with_cpu`, so
`parked=1` is *this CPU's* count. cpu5 and cpu6 each had one parked task, and
cpu0 reported `parked=1` at 81.229 s too — three parked tasks, not one.

**Would the heartbeat have caught it? Total stoppage yes, a lost wakeup no —
and that is the honest answer.**

- If the machine stopped scheduling or stopped taking interrupts, the heartbeat
  ends at 86.859 ± 250 ms and the mask says whether the CPUs went together or
  one at a time. Caught, with a time on it.
- If the failure is a **lost wakeup** — timers still firing, CPUs still taking
  passes, a parked task never woken — every CPU still reaches the idle loop and
  the line reads `alive=8/8` for as long as the machine sits there dead. Not
  caught. Worse than not caught: the instrument would assert health through the
  freeze.

**But this log already argues against a lost wakeup confined to the input path.**
The compositor wakes on its own timer to blink a cursor; it does not depend on
the keyboard. A lost keyboard wake leaves it blinking and leaves its 2 s report
coming. Both stopped at the same instant, so whatever failed took the compositor
with it. That does not eliminate a *global* wake failure — every wake lost while
timers still fire would look exactly like this and would still print `alive=8/8`
— but it does eliminate the narrow reading.

**What `diag-tick` buys this investigation anyway, and it is not small.** In this
log cpu5 and cpu6 published their scheduler census **twice in 87 seconds** —
at 28.645 s and 86.859 s — because `log_health` runs from the idle loop and those
CPUs reached it only when a keystroke happened to wake them. Every other CPU's
state across 87 s of a freeze investigation is simply absent. With the tick, all
eight publish `ready`/`parked`/`current` every 10 s regardless of quiescence, so
the next capture carries a whole-machine census right up to the last flush
rather than a two-CPU sample taken at the two moments a key arrived.

**The instrument's own risk, stated because it is on the suspect path.** The tick
makes all eight CPUs run the idle loop ~10×/s where they previously halted, and
every iteration takes `drain_serial`'s `BackendGuard::lock` — `save_and_cli`
then an unbounded spin with no deadline and no panic (`serial.rs:97`), the one
lock `specs/issues/boot-media/` calls out for exactly that. On the T14 there is no serial device so each
hold is an empty drain, but the *acquisition rate with `IF` clear* goes from
near-zero on an idle machine to ~80/s machine-wide. If the next boot behaves
differently from these, that is the first thing to suspect, and it is why the
build carrying this must not be confused with the shipping one.

**So `ran=` was built, and the obvious design would not have served.** That
design is a per-CPU `(ready, parked, current)` census beside the `TICKED` stamp,
and it is wrong for a reason worth keeping: a woken task is dispatched within
microseconds, so `ready=` sampled four times a second reads 0 on a healthy
machine and 0 on a dead one. **The signal is a rate, so the instrument has to be
a counter.** `heartbeat::note_dispatch` counts tasks switched onto a CPU — from
`KernelHw::switch`'s `Some(_)` arm, the one place a task rather than the idle
context becomes what a CPU is running — and the line carries the machine-wide
delta since the previous one. Two signatures that used to be one:

- **the line stops** → nothing is scheduling; the machine stopped.
- **the line continues with `ran=0`** → the machine is scheduling and running
  nothing. A lost wakeup, or a userland that has stopped asking.

`ran=0` is not self-interpreting and the module doc says so: a machine with
genuinely nothing to do also runs nothing. It is diagnostic on the T14 because
that desktop always has something — the compositor wakes about twice a second to
blink a cursor and every one of those is a dispatch — so a *run* of `ran=0`
there is a machine that has stopped doing what it was doing. Cross-check against
the i8042 counter line, which says whether input was arriving meanwhile.

#### The third freeze — the first audio period, and why one signature was not enough

`hda-metal/2026-08-07-183104.log`, 236 lines. **An older image**: flashed after
H2/H4 landed and *before* M3's shootdown, so the defect it shows may already be
closed, and it is evidence about the shape rather than about the current tree.
The HDA driver bound on the T14 — ALC257 found, both codecs walked, speaker pin
selected, path configured — then `spawn: /bin/tone pid=6` at 3.799 s, `soundd:
opening stream: 44100Hz 2ch`, `client 0 connected`, `soundd: resumed`, `tone:
440Hz for 2s`, and nothing ever again. That banner prints *before* the first
audio callback, so the machine stopped as the HDA DMA stream started.

That makes three metal freezes with three triggers: a process reaching its first
instruction (~1.36 s), a keypress after 57 s of idle (86.9 s), and a stream's
first DMA (~3.8 s). The common factor is **something being scheduled or woken**,
which is #156's own title almost verbatim. Against the instrument as it first
stood all three would have read `heartbeats stopped at T` — a time and never a
class. With `ran=` they read as a time *and* one of two classes, which is what
makes a fourth flash worth more than the third was.
