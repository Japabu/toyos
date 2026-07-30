# Scheduler migration log

Implementation record for `specs/scheduler-core-spec.md`. The spec says what the
protocol is; this file says how far the kernel has moved onto it, what broke on
the way, and which harness gates prove each fix. Split out of CLAUDE.md
2026-07-30.

**State: Stage 7b done — the kernel drives the core, with balance on.**

## Layout

`toyos-sched/` is the core crate:

- Stage 0 boundary types + `fair.rs`.
- Stage 3 primitives — `mailbox.rs` (the crate's only `unsafe`; everything else
  is `deny(unsafe_code)`), `waitq.rs`, `retire.rs`, `task.rs`'s state word.
- Stage 4 machine — the linear `Task` value with its five lifecycle types
  (`task.rs`), `queue.rs`, `timer.rs`, `msg.rs`, `invariants.rs`, per-CPU
  `cpu.rs`.
- `toyos-sched/sim/` deterministic simulator, `toyos-sched/loom/` model checker.

`kernel/src/sched/` is the driver half of spec §4: `payload.rs`
(`KernelPayload`/`KernelCtx`, the `LeafLock` impl, `TaskHandle`'s published
counters), `driver.rs` (percpu `CpuSched` slot behind `with_cpu`, the pass entry,
the asm switch, the idle loop, the trampoline), `waitqs.rs`.

## Stage 2 (IRQ ring)

Per-CPU `kernel/src/irq_ring.rs` carries `(IrqSource, ts)` IRQ-time stamps for
audio/net/xHCI. ISRs publish and set need_resched via the shared `msix_entry!`
macro; `drain_events` consumes the ring. No global pending flags, no `cpu == 0`
gating.

## Stage 7a (cutover)

`kernel/src/scheduler.rs` survives as the kernel-facing API and nothing else. The
cross-CPU run queues, the global blocked pool, `KILLED`, `CTX_TRANSITS`,
`IN_SCHEDULE`, `POISONED`'s scan, `PERCPU_EVENTS`, `EventSource`-keyed wakes and
`handle_outgoing`'s post-switch parking are gone, and so is the Stage 5 shim
`kernel/src/waitq.rs`. `Hw` is complete: `KernelHw` gained `switch` + `release`
with no change to the task-blind half.

Wait queues live on their objects (spec §8.6) — `Arc<WaitQueue>` on pipe ends,
listeners and io_uring rings; statics for keyboard/mouse/net/audio; fixed bucket
arrays for futex words and for the by-name waits (waitpid/join/sleep, which are
woken by `wake_direct` and never as a queue). `EventSource` survives only as
io_uring's poll key.

7a deleted what the cutover orphaned, because dead code is a build error here —
it could not leave the legacy body compiled.

## Stage 7b (balance on, retire by message)

`Env::steal` is `true`: an idle pass posts one `StealRequest` to the busiest CPU
and a loaded pass answers it from surplus, alongside the spawn-time (rotating
least-published-load) and wake-time placement 7a already had. The two pass
entries built identical `Env` values, so it now comes from one `driver::env()`
that takes the preempt guard by reference.

`retire_task` no longer spins: it posts `Msg::Retire` and parks on a wait queue
hung off the target's `TaskHandle`, woken by `KernelHw::release`. The wait
condition moved with it, from "the state word reads `Dead`" to "`Hw::release` has
run" — which is what the callers actually need, because `Dead` is published by
the reaping transition one pass *before* the release, so teardown could free
pages while the dying CPU still stood on that thread's kernel stack. It also
fixed a silent accounting loss: `exit()` and `kill_process()` call
`handle.merge_into()` immediately after retiring, and under 7a that read the
handle before `finalize()` had written to it.

Spec §7.6's `notify` field on `Msg::Retire` was deliberately **not** added — a
running target dies at a later safe point, so a notify riding the message would
have to be stashed for whichever site eventually kills it, and `Hw::release`
already *is* that single kernel-side site, where the wait can additionally cover
the payload drop. The core keeps one wake path.

7b changed **nothing measurable** in the audio counters. The recorded prediction
that balance would close the smp=8 tail is falsified — see
`specs/audio-gate-history.md`.

## Remaining for 7c

`EventSource` and `source_ready` (io_uring's poll key, not a scheduler concept
any more), and `Lock::force_unlock` (`kernel/src/sync.rs:98`), which now has no
caller at all.

## Host tests and negative gates

`cargo test` inside `toyos-sched/` runs unit + loom + sim in ~15 s. The full
Stage 4 exit criterion runs from the CLI, not from `cargo test`:

```
cargo run --release -p toyos-sched-sim -- gate 10000        # 10^4 seeds/scenario
cargo run --release -p toyos-sched-sim -- fuzz-sweep 10000000  # 10^7 fuzz steps/scenario
```

The harnesses' teeth are proven by four negative gates, every one of which is
required and every one of which is a *port of a shape the kernel actually had*:

1. `TOYOS_LOOM_RAW=1 cargo test -p toyos-sched-loom --features no-preempt-guard`
   must FAIL. It does, on invariant I2.
2. The simulator's `old_steal_port` scenario — a port of the OLD steal-and-scan
   algorithm — must fail while the same workload passes under the new protocol.
   It does, on I1/I6 and on I8 (address-space-freed-under-a-live-task).
3. `old_commit_before_pass` — the pre-`8508b37` blocking shape. Caught by I1 in
   every one of 200 seeds, median 3 steps. Its control `old_commit_fused` — the
   same shape with the halves fused, i.e. the simulator's own pre-split
   behaviour — must stay **clean**, because that is the blind spot itself.
4. `old_preemptible_window` — the only gate that fails by *aborting* rather than
   by a recorded violation, so it runs through `explore::run_catching`.
5. `scenarios::old_park_kept_the_lend` — under `ParkShape::KeepLapsedLend`
   (behind `protocol-port`, so the broken shape never compiles into the kernel)
   caught in 500 of 500 seeds, all on I9; under the shipped park all 500 are
   clean, so it gates the park and not the workload. The **old** I9 form catches
   0 of 500, measured rather than argued.

## Defects the migration found and fixed

### A wake lost between the ticket commit and the pass drain (`8508b37`)

`block_on` committed its wait ticket at the call site and then entered the pass.
A remote waker claiming the freshly-`Blocked` task posts `Msg::Wake` to the
parking CPU, whose own drain then consumed it before the task was in `parked` —
wake lost, and the park asserted on a word the waker had advanced. Reproduced on
`--smp 8` about twice in five audio suite runs. Fixed by committing inside the
pass, after the drain (spec §8.1 says phase 2 belongs to the pass, and this is
why).

The simulator could not reach that window: its `do_block` had the identical
shape and its `interfere` hook fired *before* the commit, so the interleaving was
not in the step relation at all.

### A block is two steps (2026-07-29, `77dd5d1`)

Spec §10.2 always said "ticket phases are separate steps"; the VM did not
implement it, so the simulator certified a protocol it could not execute. The
block is now `Step::Exec` (register + re-check) then `Step::BlockPass` (drain,
commit, park), with `Scenario.block: BlockShape` naming which side of the
boundary the commit falls on. That is where negative gate 3 comes from.

§8.1's residual window (a claim landing between the commit and the park) cannot
be a step boundary — a `SchedPass` borrows `CpuSched` — so it is reached by
injection and *counted* (`Outcome::pre_park_claims`). Before this it was zero in
every run ever made, which meant `RunningTask::park`'s `WakeQueued` arm was dead
code. The shrinker now also preserves a repro's violation *kind*, so minimizing
an I1 trace cannot silently hand back an I8 one.

### The registration window is preempt-off (2026-07-29, `ad86e91`)

An involuntary pass between `prepare_wait` and `block_on` aborted on `check_cpu`.
The window is now held preempt-off by `sched::driver::Ticket`, whose guard *is*
the blocking pass's bracket; `pass_block` no longer takes a bracket of its own.

Why closed rather than tolerated, since `8508b37` tolerated its window: that one
is a *remote* CPU acting between two of our own instructions and genuinely cannot
be closed. This one's only intruder is our own `preempt::enable` slow path,
reached from the guard drop of any lock the re-check takes (and, before the fix,
from inside `WaitTicket::cancel`'s own `dequeue`). The alternative — teaching
`RunningTask::preempt` to accept `Committing` — is worse than the assert: the
`Ready` word it publishes makes every waker that pops the registration report
`Claim::Lost` and move on, turning a loud panic into a silent lost wake.

`Vm::enabled` still withholds `Step::Pass` mid-block, but now because
`Scenario.window: WindowShape` says the kernel holds preemption off there, not
because the assert would abort the run. That is negative gate 4.

### A `Retire` landing mid-registration was dropped (2026-07-29)

The thread was never reaped. `WaitTicket::commit` now checks the sticky kill bit
first: if it is set it dequeues, unwinds phase 1 back to `Running(cpu)` and
returns the new `Commit::Killed`, which both drivers dispose with `dispose_exit`.
Spec §6.3 already listed the park among the safe points and §7.6 already promised
a killed task dies at its next one — the kernel simply did not keep the promise
there, so the spec needed no change.

Fixed in the core rather than in each driver: `Vm::block_pass`'s compensating
`kill_pending` arm is deleted and the sweeps stay clean because the core covers
it. Rejected alternative: honouring the kill at *wake* instead, which does
nothing for a task nobody ever wakes. Covered by `loom_retire.rs`'s
`a_retire_racing_the_park_commit_always_leaves_someone_to_reap` and counted, not
assumed, by `Outcome::killed_at_park`.

### The RT lend was spent by waiting, not by running (`9c2fc4d`)

`CpuSched::pick` demoted a queued task whose wall-clock window had lapsed out of
the RT band, which inverts the lend — a task that waited spent none of its
window, and demoting it drops it behind the normal work the lend existed to jump,
self-reinforcingly (the only re-grant paths are a wake and the pipe consume
point, and a starved *ready* task reaches neither). It fired once in ~30
config-runs and that once starved the client's cpal thread 93.3 ms behind the hog
and produced that batch's only 24-period (70 ms) gap.

The window is now armed at dispatch, so queue time does not spend it; §8.5/I9
amended to say the bound is on time *held*. The simulator rejected the first
attempt (delete the demotion, keep clearing at dispatch) on I4.

### A park kept a lapsed lend forever (`78b7bfb`)

`9c2fc4d` opened this and I9 could not have caught it. `RtState::expire` cleared
only `if now >= until` and `park` called it, so a lend taken at T and blocked on
at T+1 ms survived the block; `arm` then re-armed it at the next dispatch. A task
that gets one lend and thereafter runs less than a quantum before blocking held
inherited RT **forever** — one pipe interaction, no syscall, and any non-lending
wake (futex, timeout, io_uring completion) keeps it alive. Strictly weaker than
both the pre-cutover scheduler and the code `9c2fc4d` replaced, so the sentence
that commit added to §8.5 ("one lend buys at most one quantum of running time")
was false as implemented.

`park` now `release()`s unconditionally — the window bounds time *held* and
parking is where holding stops, which is also audio §9.4's "the promotion lasts
until the promoted thread blocks again" and costs the audio path nothing, since
every wake that matters re-lends. `preempt` stays conditional.

**The tell was that I9 needed no change when `arm` landed:** it compared a
running task's `until` to the clock, and a re-armed `until` is by construction
fresh, so it passed for the same reason it had stopped measuring anything — the
exact shape of gate A's four instrument defects. I9 is now cumulative *Running*
residency per lend. `RtState::lends` counts grants that actually extend the
window; it must live in the core because `arm` moves `inherited` too, so an
outside observer cannot tell a fresh grant from a re-arm.

Same-session A/B of the whole boost-work delta against `1486473`, 64 first-boot
config-runs per side: 14/56 vs 18/64, Fisher p=0.84 — no rate change, as
expected, since neither commit was ever the dropout.
