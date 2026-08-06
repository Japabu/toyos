# The xHCI port machine: extraction, host model, and the three defects it carries

Plan of record. Nothing here is built yet.

Governed by `specs/code-quality-review-2026-08.md` — the doctrine (§1) and the
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
known-issues "A scheduler pass may spend two seconds in xHCI before it drains
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

X0 and X1 are on main (`6bfeed9`); X2 is not started. Everything below is
established and should not be re-derived.

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
  Nothing to do: `cargo run -- --land` takes a run it fired in, and does not
  take one where it failed some other way.
- **There is no gate for the unplug window and it cannot be aimed.** The hazard
  is the 100 ms between the device going and the teardown running, and a QEMU
  `device_del` cannot be landed inside it. That is the answer and belongs in a
  report as the answer, not as an omission. Do not ship one that passes for the
  wrong reason.

Delta from the task's fix-shape: the task offers "bounded work per pass or a
dedicated context". **This plan takes neither literally.** Bounded work per
pass does not help while one unit of work can block for 2 s. A dedicated
context (a kernel thread) does not help either while the thing it would block
on is the `XHCI` ticket lock, which disables preemption — the thread would have
to drop the lock across every wait, and completions would have to be routed to
it, which is the io_uring blocking primitive `specs/iouring-blocking-spec.md`
has not built yet, in a subsystem another agent owns. Submit-and-return needs
none of that and is what the extraction produces anyway.

### X3 — the verdict on #156's prologue, stated as a gate

**Does X2 deliver it? Yes, and only because X2 covers recovery and teardown as
well as enumeration.** Enumeration alone would not: `recover_endpoints` runs on
the same path and blocks the same way. With the type-level split above,
"`drain_irqs` cannot spend 2 s in xHCI" stops being a claim and becomes a
compile error, which is the strongest form available and the one the doctrine
asks for.

What X2 does **not** fix, stated so the task is not closed on a half-answer:
`storage_read`/`storage_write` still run SCSI commands under the same lock from
whatever thread faulted, and known-issues already says that is not fixable by
this conversion. That path does not run inside `drain_irqs`, so #156's prologue
is closed and the lock-hold finding is not.

Price: no new code beyond X2 — it is X2's proof obligation. One guest gate that
measures the pass duration across a plug, plus the `sched-check` pass-budget
assertion known-issues records as never enabled.

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
