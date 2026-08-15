# The xHCI port machine: extraction, host model, and the three defects it carries

Plan of record. **X0, X1 (`6bfeed9`), X2a and X2b are built; X2c and X3 are
not** — §3 carries the per-stage state and is the authority. This line said
"nothing here is built yet" until 2026-08-08, three stages after it stopped
being true.

Governed by `specs/assessments/code-quality-review-2026-08.md` — the doctrine (§1) and the
`xhci/` verdict (§2 drivers/), which names this extraction as the vehicle for
#151, #152 and #156's prologue, with #155's F-K as a design input.

## 1. The three defects, as facts in the code

Each was diagnosed during #145 and is restated here with the line that carries
it, so the plan below can be read against something checkable.

**#151 — a SuperSpeed port gets the USB2 treatment, unconditionally.**
`service_port` (`kernel/src/drivers/xhci/mod.rs`) reaches
`device::begin_reset`, which writes `PORTSC_PR` for every port regardless of
protocol, and then waits for `PRC`. Verified by enumeration of the uses:

- `PORTSC_PED` is read at exactly one decision site,
  `device.rs`'s `configure`, *after* the reset, as a post-condition. It is
  never consulted to decide whether to reset.
- The speed field is read at exactly one site, in the same function, also
  after the reset.
- There is no `WPR` constant and no `PLS` read or write anywhere in the driver.
- `legacy.rs`'s `find` walks extended capabilities but the only id any caller
  asks for is 1 (USB Legacy Support). **The Supported Protocol capability (id
  2) is never parsed**, so the driver does not know which port registers are
  the USB3 view of the machine and which are the USB2 view.

A trained SuperSpeed link comes up Enabled with PED already set; writing PR to
it is a hot reset, and a USB3 port that fails one lands in Inactive, from which
only a Warm Reset recovers — a command this driver cannot issue. `reset_done`
then never sees PRC, the 2 s deadline expires, and `service_port` marks the
port `attached = true, work = Settled`, which is a port never looked at again
for the life of the boot.

**#152 — the enumeration runs inside the scheduler pass, under a lock that
disables preemption.** `poll_if_pending` calls exactly four things:
`next_event`/`dispatch_event` (a drain, no wait), `recover_endpoints`, and
`service_ports`. The last two reach `device::configure`, `restart_endpoint`
and `disable_slot`, and every one of those spins to `USB_TIMEOUT_NS` = 2 s per
command or transfer against a device that does not answer. `Lock::lock`
disables preemption for the guard's whole life and panics at 500M spins, so one
port that reports a connect it cannot complete costs every CPU that enters a
scheduler pass. `configure` alone holds eight such blocking points; the driver
has 25 call sites of the five blocking primitives across three files.

**#156 prologue — `drain_irqs` calls `poll_if_pending` first, before the
mailbox drain.** `sched/driver.rs`'s `drain_irqs` is the only caller, and both
`pass` and `pass_block` call it before `SchedPass::begin`. So the 2 s above is
spent while the CPU is holding every message addressed to it. Already filed:
`specs/issues/` "A scheduler pass may spend two seconds in xHCI before it drains
its mailbox", with the T14's `retire_task` 1 s guard panic as the observed
consequence.

**#155 / F-K — `with_storage`'s invariant is non-local.** It copies the `Disk`
out, runs `f`, and writes it back; a re-entrant call on the same block would
have its effect silently discarded. Safe today only because `dispatch_event`
touches nothing under `msc`. #152 makes the poll path do more, so the
invariant's safety argument gets weaker exactly as the code gets busier.

## 2. The vehicle

A new crate, `toyos-xhci/`, on the `toyos-sched` model: a `no_std` core with a
hardware trait and an invariants module, plus a host-only `sim` package that
explores, shrinks and replays. The kernel keeps effects only.

```
toyos-xhci/
  src/lib.rs
  src/portsc.rs     PORTSC as a decoded value and a write that cannot lose a bit
  src/protocol.rs   the Supported Protocol capability (xHCI §7.2) decoder
  src/port.rs       the per-port state machine: (belief, register, now) -> Step
  src/hw.rs         the trait the kernel implements and the sim fakes
  src/invariants.rs the checks the sim runs after every step
  sim/              host-only: scenarios, exploration, shrink, corpus
```

**What is pure.** Everything that decides. `portsc.rs` turns a `u32` into a
value with named fields and builds a write from a neutral base plus the bits a
caller means — the rule `portsc_neutral` states today, made unrepresentable
rather than restated. `protocol.rs` turns the bytes of capability id 2 into a
port→protocol map. `port.rs` is the state machine: it is handed what a port
reads, what the driver believes, the protocol that port speaks, and the clock,
and it answers with a `Step` — one of `Idle`, `WaitUntil(t)`, `Write(portsc)`,
`Enumerate`, `Teardown`, `GiveUp(reason)`.

**What is effect.** MMIO, the TRB rings, the DMA pool, the slot lifecycle, the
descriptor walk. These stay in `kernel/src/drivers/xhci/`. The kernel's job
becomes: read the registers, ask the machine, do what it says.

**F-K becomes representable, and it is one line.** `with_storage` stops copying
and starts taking: `self.msc[at].disk.take()`, run `f`, put it back. A
re-entrant call then sees `None` and cannot clobber — the invariant becomes a
fact the type enforces and re-entrancy becomes observable instead of silent.

**What is deliberately *not* in scope.** The BOT/SCSI transport is named by the
doctrine as a later host model. It is a separate machine with a separate
alphabet, and folding it in would make this plan unlandable. `usb_gate` keeps
its current subject.

## 3. Stages

Each stage lands on its own and leaves `main` green. Prices are stated as what
they are: line counts are measured, session estimates are estimates and say so.

### X0 — the extraction, behaviour unchanged

Create the crate; move the decisions; leave every outcome identical. The guest
gates that exist (`xhci_hotplug`, `xhci_flap`, `xhci_slow_connect`,
`xhci_portsc_rw1c`, `xhci_deaf_registers`, and the eight USB storage gates) are
the regression suite: **X0 is correct exactly when it changes none of them.**

Host tests arrive with the code, on the `toyos-sched` discipline:

- *Invariants*, checked after every simulated step. Drafted: a port the driver
  believes attached has a slot or a stated refusal; no port is in `Resetting`
  without `PR` having been written; a debounce is never shortened by a
  re-observation; every `Step` that writes PORTSC preserves the read-only and
  read-write-same bits (`portsc_neutral`'s rule, now checkable).
- *Staged sequences*: connect, disconnect, the replug that collapses inside one
  debounce, a port that flaps N times, a port that never finishes its reset, a
  connect during another port's enumeration.
- *Negative gates that must red.* Five, mirroring `toyos-sched`'s discipline —
  a gate that cannot fail proves nothing. Drafted: (a) compare CCS against
  belief without reading CSC → the collapsed replug is invisible; (b) clear
  change flags before deciding → same; (c) write back what was read → PED
  clears and the port disables (the `xhci-portsc-rw1c` finding, now a host
  test); (d) restart the debounce clock on every observation → a flapping port
  never settles; (e) drop the reset deadline → a dead port wedges the machine.

Price: the port machine is ~120 lines of `mod.rs` today plus `begin_reset` and
`reset_done`; the crate is new code of roughly 400–600 lines including the sim,
which is where the bulk is. No new guest gate. Estimate: one to two sessions.

Delta from the review's fix-shape: none. This is what §2 drivers/ asks for.

### X1 — #151: the driver learns what its ports are

1. `protocol.rs` decodes capability id 2 (§7.2: compatible port offset, port
   count, major/minor revision, protocol speed ids). `legacy::find` already
   takes the id as a parameter, so the walk needs no change — only a caller and
   the decoder. Host-tested against crafted capability lists, including the
   malformed ones the existing in-guest `xhci_xecp_walk` selftest covers, which
   then shrinks to what it is actually for.
2. The port machine gains the protocol as an input. A USB3 port that reads
   `CCS` and `PED` together has a trained link and is enumerated **without a
   reset**. A USB3 port that reads `CCS` without `PED` gets the reset it needs.
3. Inactive recovery: a USB3 port whose `PLS` reads Inactive, or whose hot
   reset ran out its deadline, gets a Warm Reset (`WPR`, bit 31) and waits for
   `WRC`. This is the command the driver does not have today and the one the
   T14's USB-A ports need.

Price: `protocol.rs` and its tests, three new `Step` variants, three new PORTSC
bits, and the branch that chooses. Estimate: one session on X0's foundation.

Delta from the task's fix-shape: **the task lists three fixes; this makes them
one decision.** "Which reset does this port need" is a function of protocol and
link state, so it is one branch in the pure machine rather than three patches
in the driver. The warm-reset path is *not* a fallback bolted onto the hot one:
Inactive is a state the machine names, and the recovery is what it does there.

**What this stage cannot prove.** QEMU's xHC has no link training and no
Inactive state; its SS ports read PED set the moment they are touched. So the
host model is where correctness lives here, and the guest gate can only certify
that the driver still enumerates everything it used to. Confirmation that the
T14's USB-A ports now mount is metal, and the owner's panel photo of the
failing boot would be the before-picture. **The gate must not claim more than
that**, and this is the stage where a gate most easily becomes vacuous.

### X2 — #152: enumeration leaves the scheduler pass

The port machine already says *when* to enumerate; X2 changes *how*. Every
blocking step becomes submit-and-return: the driver puts a TRB on a ring, rings
the doorbell, records what it is waiting for, and returns. The completion
arrives through the event ring the poll already drains and advances the machine
by one step.

The structural claim, and the reason to prefer this to a dedicated thread:
**`poll_if_pending` must call nothing that can wait.** That is enforceable by
types rather than by discipline — the poll path holds a view of the controller
that does not expose `wait_command`, `wait_transfer` or `settles` at all, so a
future edit that reintroduces a wait does not compile. The blocking primitives
survive for the boot path, where blocking is correct because there is no
scheduler yet.

The same treatment must cover `recover_endpoints` and `teardown_port`, or the
claim is false: HID recovery issues two commands and a control transfer, and
`disable_slot` issues one. This is why they are in this stage and not deferred.

Price: the largest stage by far. Eight blocking points in `configure`, four
more across recovery and teardown, and the descriptor walk becomes a resumable
sequence rather than straight-line code. Estimate: two to three sessions, and
the one most likely to need a second landing.

#### Working state, so this stage survives its agent

X0 and X1 are on main (`6bfeed9`); **X2a and X2b are both built** — see the
X2b section below, which this line contradicted until 2026-08-08. X2c is not
started. Everything below is established and should not be re-derived.

**The three call sites that must lose the ability to wait.** `poll_if_pending`
reaches exactly four things — `next_event`/`dispatch_event` (a drain, no wait),
`recover_endpoints` and `service_ports` — and the last two reach:

| path | what blocks | how many |
|---|---|---|
| `service_port` → `device::configure` | Enable Slot, Address Device, two `GET_DESCRIPTOR(Device)`, Evaluate Context, `GET_DESCRIPTOR(Config)`, SET_CONFIGURATION, Configure Endpoint | eight, `USB_TIMEOUT_NS` each |
| `recover_endpoints` → `recover_hid` → `restart_endpoint` | Reset/Stop Endpoint, Set TR Dequeue, CLEAR_FEATURE(HALT) | three commands and a control transfer |
| `service_port` → `teardown_port` → `disable_slot` | Disable Slot | one |

`teardown_port` is the owner's trigger, which is why recovery and teardown are
in this stage rather than deferred behind enumeration.

**The type split, concretely.** `poll` takes a view — `Stepper` — exposing
`next_event`, `dispatch_event`, the doorbell, ring enqueue and PORTSC access,
and **not** `wait_command`, `wait_transfer`, `settles`, `run_command` or
`control_transfer`. Those stay on `XhciController` for the boot path, where
blocking is correct because there is no scheduler yet. "`drain_irqs` cannot
spend the transfer budget in xHCI" then fails to compile rather than failing a
test.

**What "done" is, against the owner's acceptance test.** Pull the stick while
the desktop is up: the machine keeps running and Ctrl+Alt+D still answers. Not
"recovers eventually". The guest-side proxy is that no scheduler pass blocks;
the metal proof is the owner's and it is the one that counts.

#### Exclusions already established — do not re-litigate

- **The ticket lock is not the freeze as first diagnosed.** `sync::Lock::lock`
  warns `LOCK CONTENTION` at 50M spins and panics `DEADLOCK` at 500M; a CPU
  behind one 2 s hold passes the warning and one behind two approaches the
  panic. The owner reports a freeze with neither, and the scheduler track
  excluded the same lock independently for the wedge. What the code still
  supports is *one* CPU holding `XHCI` for the budget per command — not a
  spinning fleet.
- **The log flush is bounded in acquisition, unbounded in work.**
  `log_file::poll` is `try_lock` on `SINK` and the VFS and disables the sink
  after `MAX_BLOCKED_NANOS`; `Sink::flush` then holds both across
  `flush_file`/`sync_mount` → `msc_write`/`msc_flush` → `XHCI`. X2 does not
  close this and it is not this agent's.
- **`c4ba7d5` already removed the per-transfer amplifier**: a transfer to a port
  that reads disconnected fails at once. X2 is about the *pass*, not about the
  seconds — those are gone.

#### Two things that will mislead the next agent

- **`desktop_window_child` is intermittent, not parallel-phase-only.** It has
  failed under a one-test filter with nothing else on the host, in a different
  round each time. **If X2 makes it pass that is not evidence of anything** — as
  likely the race landing the other way, which is exactly why its
  `EXPECTED_FAILURES` entry expires on a date rather than on a green run.
  Nothing to do: the gate takes a run it fired in, and does not take one where
  it failed some other way.
- **There is no gate for the unplug window and it cannot be aimed.** The hazard
  is the 100 ms between the device going and the teardown running, and a QEMU
  `device_del` cannot be landed inside it. That is the answer and belongs in a
  report as the answer, not as an omission. Do not ship one that passes for the
  wrong reason.

#### X2a and X2b — the split, and why teardown goes first

X2 lands twice. **X2a is teardown and recovery, and is built; X2b is
enumeration.** The
owner's live defect is a machine that freezes when the boot stick is pulled and
answers no Ctrl+Alt+D afterwards, and neither of the two paths that run on an
unplug is enumeration. Breadth of what enumerates does not touch it.

**The mechanism, once, for both halves.** Every blocking step becomes
submit-and-return against one outstanding operation per controller:

- `submit_command` answers with the Command TRB's *physical address*, and
  `wait_command` matches the Command Completion Event's `param` against it
  (§6.4.2.2). It used to take the first completion of any command, so a command
  that ran out its deadline and answered afterwards handed its code to whatever
  was waiting next — latent today and unavoidable the moment two commands are in
  flight from different callers.
- `dispatch_event` offers every event to the outstanding operation before doing
  anything else with it, and **records the code rather than acting on it** —
  the same reason `broke_with` is recorded rather than recovered where it is
  read: the drain runs inside `wait_command` and `wait_transfer` on behalf of a
  caller waiting for one particular event.
- `toyos_xhci::job::Outstanding<W>` is that slot, pure: what ends the wait
  (`Await::{Command,Transfer}`), the deadline, the answer, and the cancellation.
  **One slot and not a queue** — the driver ran these strictly serially before,
  the command ring is one queue, and a second slot would buy concurrency the
  hardware does not have at the price of the cancellation rules' only simple
  form.
- `toyos_xhci::recovery` is the endpoint recovery as a sequence: `Recovery::begin`
  reads the Endpoint State out of the controller's output context and answers
  with the first command, `Recovery::completed` with the next. Two drivers of
  the one sequence — a blocking loop for `msc`'s bulk pair, which runs on a
  faulting thread and not in a pass, and a stepped one for HID.

**Two hazards that are not obvious and are handled.**

- A `Step::Enumerate` taken while a Disable Slot is outstanding can be handed
  back the same slot id: the controller processes the ring in order, so it will
  not, but the *driver* would then zero the DCBAA entry the new device's context
  now occupies. Enumeration and teardown both defer while an operation is
  outstanding; a register write, an acknowledge and a reset do not.
- A recovery outstanding for a device on a port that has just gone is cancelled
  rather than waited out, because **a transfer error on a port that has gone
  belongs to the disconnect**. Without it the teardown waits the deadline behind
  a job whose device will never answer.

**One thing the conversion needed that was not foreseen.** `SteppedWhileWorking`
went red the moment a teardown spanned two passes: the effect used to begin and
end inside one `service_port` call, so a step taken while `Work::Working` could
only be re-entrant, and now the ordinary poll finds a port mid-teardown every
pass. The fix is the loop and not the invariant — **a port inside an effect is
not looked at at all**, which also saves the register read it was taken to be
told nothing by. The re-entrancy gate is unchanged and still red on demand.

**What X2a does not deliver, stated so a green suite does not imply it.** The
type split above is X2b's. A `Stepper` that still has to hand `poll` a way to
reach `device::configure` is a signature promising a check it does not perform,
and privacy would not enforce it either: a child module of `xhci` can name its
parent's private methods, so the view has to be the *only* handle by then.

Nor does it change the *seconds*: `c4ba7d5` already ended a transfer on the
register when the port reads disconnected, so what X2a removes is the pass, not
the budget. And two costs move rather than going away — `PORT_WORK_AT` now
carries the outstanding operation's deadline, so an idle CPU declines to halt
across a teardown exactly as it already does across a debounce; and a teardown
takes one further scheduler pass, which is what "submitted and left" means.

**What gates X2a.** `toyos-xhci/sim/tests/teardown.rs`, nine tests, three of
them negative — a recovery left running against a device that has gone costs a
second whole deadline, a teardown taken while the controller still owes an
answer is caught, a pool block given back before its slot is answered for is
caught. Plus `job.rs` and `recovery.rs`'s own unit tests. In the guest: the
sixteen `xhci_*` gates and the nine `usb_*` ones are the regression suite and
change no outcome, which is the same standard X0 was held to. **No new guest
gate**, because the window is 100 ms wide and a `device_del` cannot be aimed
inside it — see the note below, which is the answer and not an omission.

#### X2b is built. What it delivers, and the one thing it does not

`configure`'s eight blocking points are `toyos_xhci::enumerate`'s acts, each
submitted with the outstanding slot recording what ends it. One implementation
and two drivers, as the recovery already had. The split is a **module** and not
a view — `xhci::wait`, with the poll outside it — and a pass that tries to wait
does not compile: checked by writing each of the six calls and reading the
error. Its three descendants are the three contexts where waiting is correct,
and the third of them is the door: `msc::bind`, one call site named in
`device::bind`, which X2c removes.

Measured on `xhci_hotplug`: a mouse plugged in after boot is connected,
addressed, configured and delivering inside **one millisecond**, across as many
scheduler passes as it has acts. The 16 `xhci_*` and 9 `usb_*` gates change no
outcome, which is the standard X0 and X2a were held to, and there is **no new
guest gate** — for the reason recorded below, which is the answer and not an
omission.

Two things the conversion made dead went with it: `poll` advanced the
outstanding operation a second time after `service_ports` and re-tested
`ports_dirty`, both because enumeration used to drain the event ring from
inside the pass.

**What gates X2b.** `toyos-xhci/sim/tests/enumerate.rs`, six tests, one of them
negative: an enumeration left running against a device that has gone makes the
unplug wait out its deadline as well as the teardown's, and the same measurement
with the cancellation on lands on the other side of one deadline. Plus
`enumerate.rs`'s own unit tests — which is where the two EP0 packet-size tables
finally got any, having had none while carrying the defect that addressed every
SuperSpeedPlus port at a 64-fold undersized control endpoint.

#### X2b's design, written before the code

**The plan under-counted `configure`'s reach, and the count is what decides
whether the split can hold.** The table above stops at `configure`; but
`configure` ends by calling `msc::bind`, which issues its own Configure Endpoint
and then `bring_up` — TEST UNIT READY on a 500 ms budget, INQUIRY, READ
CAPACITY, each a Bulk-Only round trip of three transfers with stall recovery
inside it. That is the BOT/SCSI transport §2 excludes by name, and it runs
**inside a scheduler pass** on every hot-plugged disk. `usb_refused_disk_first`
is the gate that proves it runs there: it `device_add`s a stick after boot and
requires `disk 1 ready on slot` in the log with no userland asking, so a pass
that declined to bring a disk up would go red.

So X2b converts everything up to the class bind, and the split holds for all of
it. What is left is one call, named below.

**The mechanism, reusing X2a's and inventing nothing.** `toyos_xhci::enumerate`
is the sequence — which request comes next, and the two places it branches — on
`recovery.rs`'s shape: `begin` answers with the first act, `completed` with the
next. The kernel holds the data (slot id, speed, EP0 ring, the parsed function)
in `What::Enumerating` and performs the acts. Two drivers of the one sequence:
`settle_outstanding` for the boot scan, where blocking is correct because there
is no scheduler to give a pass back to, and `advance_outstanding` for the pass.
`configure`'s straight-line body becomes those acts and nothing else moves.

**A control transfer with a data stage is two completions, and the job has to
own that.** The data stage carries ISP and IOC so the driver can learn how many
bytes actually arrived, so the status stage's event is a second one on the same
(slot, dci). A job that finished on the first would leave the second to be
matched by whatever asked next — the transfer-side form of the defect
`submit_command`'s TRB matching closed on the command ring, and reachable
because `poll` drains the whole ring before it advances anything. So
`Outstanding::submit` takes `Stages`, and `Outcome::Answered` carries the
residue as well as the code: the residue is the data stage's, the code is the
last one seen, and a code that is neither Success nor Short Packet ends the
operation because the halted endpoint will never run the status TRB.

**An enumeration outstanding for a port that has gone is cancelled**, for the
reason a recovery is: a transfer error on a port that has gone belongs to the
disconnect, and without it the teardown waits a whole deadline behind a device
that will never answer.

**The split is a module, not a view, and that is what makes it hold.** A view
handed to `poll` would have enforced nothing: Rust makes a module's private
items visible to its *descendants*, so `xhci::device` and `xhci::hid` can name
whatever `xhci` keeps private, a view's own field included. So the primitives
went **below** the poll instead of beside it. `wait_command`, `wait_transfer`,
`settles`, `run_command`, `control_transfer`, `restart_endpoint` and
`settle_outstanding` are private to `xhci::wait`, and `xhci`, `xhci::device` and
`xhci::hid` are not inside it.

`wait`'s three descendants are the three contexts where waiting is correct:
`wait::boot`, the scan, which has no pass to give back; `wait::msc`, a disk read
or written from the thread that faulted; and **the one door**, `msc::bind`,
because a disk plugged in after boot has to be brought up by somebody. **That
door is X2c's to remove**, and until it does, "`drain_irqs` cannot spend the
transfer budget in xHCI" is true of everything except a disk arriving after
boot.

### X2c — the BOT/SCSI machine, which closes the last door

The bring-up conversation expressed the way `recovery` and `enumerate` are: the
round trip (command block out, data, status in, with the one legal stall retry)
and the bring-up above it (TEST UNIT READY on a budget, sense, INQUIRY, READ
CAPACITY 10 then 16). Two drivers again — the blocking one for
`storage_read`/`storage_write`, which run on the thread that faulted and spend
their own time, and a stepped one for the bind. Not folded into X2b because it
is a second machine of its own size and would make the landing unreviewable;
§2's exclusion stands, and this is where it is paid off.

Delta from the task's fix-shape: the task offers "bounded work per pass or a
dedicated context". **This plan takes neither literally.** Bounded work per
pass does not help while one unit of work can block for 2 s. A dedicated
context (a kernel thread) does not help either while the thing it would block
on is the `XHCI` ticket lock, which disables preemption — the thread would have
to drop the lock across every wait, and completions would have to be routed to
it, which is the io_uring blocking primitive `specs/plans/iouring-blocking-spec.md`
has not built yet, in a subsystem another agent owns. Submit-and-return needs
none of that and is what the extraction produces anyway.

### X3 — the verdict on #156's prologue, stated as a gate

**Does X2 deliver it? Not yet, and the gap is one call.** X2a and X2b together
make "`drain_irqs` cannot spend 2 s in xHCI" a compile error for teardown,
recovery and enumeration — the strongest form available and the one the doctrine
asks for. What is left is `msc::bind`, reached from `device::bind` when a *disk*
is plugged in after boot, and it is the whole of X2c. Until then this stage is
open, and a report that closed it would be claiming a property one greppable
call site contradicts.

What X2 does **not** fix even then, stated so the task is not closed on a
half-answer:
`storage_read`/`storage_write` still run SCSI commands under the same lock from
whatever thread faulted, and `specs/issues/` already says that is not fixable by
this conversion. That path does not run inside `drain_irqs`, so #156's prologue
is closed and the lock-hold finding is not.

Price: no new code beyond X2 — it is X2's proof obligation. One guest gate that
measures the pass duration across a plug, plus the `sched-check` pass-budget
assertion `specs/issues/` records as never enabled.

### #100 rides X2; there is no fourth stage

#100 is the ~14 ms a hot-plug enumeration blocks a scheduler pass, which is
audio-relevant and is exactly what X2 removes. It is measured in X2's gate-A
A/B and claimed there if the pass-block is gone.

It is **not** the idle-wake condition — `PORT_WORK_AT` stopping an idle CPU
halting through a debounce or a reset deadline. That is CLAUDE.md's separate
pre-`hlt` item, it needs the deferred-callback facility, it is scheduler
territory and it is not in this wave. X1 shrinks it as a side effect, since an
SS port that needs no reset has no 2 s deadline to keep a CPU awake for; the
measured change is reported and never claimed as the fix.

## 4. Risks

- **X2 is the stage that can slip.** If it does, X0 and X1 still land and are
  worth landing alone: the model exists, and the T14's USB-A ports are the
  owner's actual complaint.
- **A host model can be green against a machine that does not exist.** The
  mitigation is the negative gates and the fact that X0 changes no guest
  outcome; the risk is highest in X1, where QEMU cannot stage the states the
  model is written for. Stated in X1 rather than hidden.
- **Shared host, shared locks.** Gate A watches hotplug timing and runs in the
  same suite; X2 changes when enumeration happens relative to a scheduler pass,
  so gate A is a real signal here and not noise. A/B in one session against one
  HEAD, per the standing rule.
