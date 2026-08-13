# ToyOS Scheduler Core — Technical Specification

Codename `toyos-sched`: the scheduling core the kernel and a host simulator both drive.
`kernel/src/scheduler.rs` is the kernel-facing API over it; `kernel/src/sched/` is the driver
half.

## 1. Goals and the prime directive

- **Catch bugs at compile time.** Every known bug class is triaged, in strict priority order, into:
  (1) unrepresentable in the type system, (2) runtime fail-fast assert, (3) exhaustively explored
  by a deterministic simulator / loom. A design that makes a bug class uncompilable beats a faster
  design that relies on discipline.
- **Per-CPU exclusive ownership.** A CPU's scheduler state is touched by no other CPU, ever. All
  cross-CPU effects are typed messages. Zero locks in the scheduler core.
- **One wait/wake primitive.** Every blocking site in the kernel uses the same two-phase protocol;
  per-source ad-hoc race patches cease to exist.
- **Simulatable off-target.** The complete scheduling logic runs deterministically on the host
  under a seeded/fuzzed interleaving explorer. The kernel and the simulator drive the same crate.
- **Scale to 128 cores.** No global locks or broadcast IPIs on any hot path.
- **Policy-identical cutover.** The just-debugged stored-lag fairness semantics (commits
  `138a625d`, `c9205ce2`, `b71d0d07`) are preserved bit-identically through the machinery cutover
  so regressions are attributable. Policy upgrades are separate, sim-gated stages.
- **Arch-portable.** x86-64 now, ARM64 later: all hardware access behind one trait; the core crate
  is arch-free.

## 2. Bug-class ledger (evidence → mechanism)

Every class is empirical — from the crash dossier and the wake-machinery audit. CT = compile-time
impossible, RT = runtime fail-fast, SIM/LOOM = explored.

| # | Bug (observed) | Today's mechanism | Fate in this design |
|---|---|---|---|
| B1 | Task in two places at once → AddressSpace Arc double-drop → UAF triple fault (crash.md) | bitwise-moved `TaskCtx` across 5 containers; steal holds ctx unlocked in transit | **CT**: linear `Task` value, five consuming state types (§5) |
| B2 | Lock guard leaked across `context_switch` (`into_raw`/`force_unlock`) | protocol by convention | **CT + RT**: no lock exists in the core; pass ends before switch (§6.2); preempt-count baseline assert (§6.4) |
| B3 | Five lost-wake windows (pipe, futex gen hack, audio, io_uring, listener) | post-switch parking + per-source rechecks | **CT + LOOM**: one `WaitTicket` two-phase commit for every source; no post-switch parking exists (§8) |
| B4 | Ready task stranded on a sleeping CPU forever (Run A: soundd, `ready=1 current=None`, all CPUs hlt) | steal filter + idle/hlt race | **RT + SIM + LOOM**: sleep handshake with doorbell (§7.5); invariant I2 |
| B5 | Deadline armed too late (blocker's deadline enters pool after successor armed quantum; 2.9 ms sleep honored 7+ ms late) | do_schedule ordering | **CT**: deadline insertion and timer arming in one pass on one CPU; `finish()` programs the timer last (§8.4) |
| B6 | `retire_task` global scans, `KILLED[16]`, `WAKE_TRANSITS`, 1 s timeout panic | cross-CPU mutation needs proof-of-absence | **CT + protocol**: kill bit + message chase; the home CPU is the proof (§7.6) |
| B7 | RT wake does not preempt promptly (up to 10 ms); broadcast kick IPIs | no wake→preempt path | **Protocol + SIM**: claim-then-post wake, targeted IPI, pass at IRQ exit (§7.4); invariant I4 |
| B8 | Silent wake drops on event-queue overflow (`EVENT_QUEUE_SIZE`) | fixed-size per-CPU queues | **CT**: intrusive embedded mailbox nodes — overflow has no representation (§7.2) |
| B9 | Scheduler untestable off-target; panic recursion destroyed the crash evidence | logic interleaved with hardware | **Architecture**: sans-IO core crate + deterministic sim + loom (§10); panic-reentry hardening in the driver (§9.4) |
| B10 | DLL fed drain-time, not IRQ-time timestamps; completion delivery quantized to 10 ms | ISR sets a flag; drain_events stamps later | **Front-loaded fix**: per-CPU IRQ ring with IRQ-time timestamps (`kernel/src/irq_ring.rs`, §8.6) |

## 3. Architecture overview

`toyos-sched` is a `no_std + alloc` crate that is a **state machine, not a control-flow library**.
It never touches hardware and never blocks. Each CPU owns a `CpuSched` — a `!Sync` value reachable
only through that CPU's percpu pointer; there is no global runqueue array. All cross-CPU effects
are typed messages into per-CPU intrusive MPSC mailboxes, consumed only at defined safe points.
Every scheduling entry is a type-state `SchedPass` that ends by returning an `Action`
(`Run(RunToken)` or `Idle(SleepToken)`) which the environment executes — nothing scheduler-related
runs after a context switch resumes. Tasks are linear values (`!Copy`, `!Clone`, drop-bomb) whose
five lifecycle states are five distinct types; a wrong transition has no function to call.

The kernel driver (percpu slot, asm switch, LAPIC/IPI, idle `hlt`) and the host simulator (virtual
time, N virtual CPUs, seeded/fuzzed interleaving explorer) drive the same crate through one `Hw`
trait. The rule that keeps the two worlds equivalent: **no scheduling decision, no state
transition, and no ordering-sensitive code may live outside `toyos-sched`**. The kernel glue is
reviewed against a whitelist (percpu plumbing, asm, Hw impl, WaitQueue placement — nothing else).

## 4. Crate and file layout

```
toyos-sched/                     # workspace member; no_std + alloc
  Cargo.toml                     # features: "std", "check" (deep asserts), "protocol-port", "loom"
  src/lib.rs                     # #![deny(unsafe_code)] — overridden in mailbox.rs (whole file)
                                 #   and at hw.rs:144 (one declaration; the core builds tokens safely)
  src/task.rs                    # Task linearity, five state types, AtomicTaskState
  src/queue.rs                   # RunQueue: RT FIFO band + fair BTreeMap ordering
  src/fair.rs                    # FairShare: per-process vruntime/lag/frontier math (pure)
  src/mailbox.rs                 # intrusive MPSC + doorbell   [#[allow(unsafe_code)], loom-checked]
  src/waitq.rs                   # WaitQueue + WaitTicket two-phase commit + boost window
  src/timer.rs                   # TimerPlan + TimerApplied (a deadline lives in ParkedEntry, §8.3)
  src/cpu.rs                     # CpuSched, SchedPass type-state, Action, sleep handshake
  src/retire.rs                  # kill bit + retire message chase
  src/msg.rs                     # Msg: the per-CPU mailbox message type
  src/sync.rs                    # atomics shim — resolves to loom's instrumented atomics under "loom"
  src/hw.rs                      # Hw trait, CpuId/Nanos newtypes, TraceEvent (shared format)
  src/invariants.rs              # feature="check": container/state-word cross-checks, timer invariant
toyos-sched/sim/                 # separate package: toyos-sched-sim (std, host-only)
  src/main.rs                    # CLI: run / fuzz / replay / shrink / from-qemu  [from-qemu is unimplemented!()]
  src/vm.rs                      # virtual CPUs, virtual clock, pending IPIs, IRQ-off gating
  src/hw_impl.rs                 # SimHw: Hw impl over vm.rs (mock Arc payload, ctx_saved shadow)
  src/choice.rs                  # ChoiceStream: SmallRng(seed) | raw fuzz bytes | PCT priorities
  src/explore.rs                 # step chooser over the enabled-step relation, trace recorder
  src/shrink.rs                  # delta-debugging minimizer; emits .trace corpus entries
                                 #   (a header + decisions), NOT generated #[test] fns
  src/workload.rs                # Script DSL: Run | Block | Wake | Spawn | Exit | FutexOp
                                 #             | IrqAt | KernelSection(ns)  (preempt-off budget)
  src/scenarios.rs               # crash_md_exit_race, lost_wake_{pipe,futex,iouring,audio,listener},
                                 #   idle_hlt_race, rt_wake_latency, audio_pipeline, old_steal_port
  src/invariants.rs              # sim-side invariant walk (distinct from the core's)
  src/msg.rs, src/payload.rs     # sim message + payload types
  src/sweep.rs                   # fuzz-sweep driver
  corpus/                        # checked-in minimized failing traces (permanent regressions)
toyos-sched/loom/                # SEPARATE package: toyos-sched-loom. Compiles the core's own
                                 # sources with feature "loom" so src/sync.rs resolves to loom atomics.
                                 # These four are NOT under sim/.
  tests/loom_mailbox.rs          # MPSC push/drain: same-CPU IRQ torn push; preempted-producer model
  tests/loom_ticket.rs           # prepare_wait / wake / block_on / cancel / timeout races
  tests/loom_sleep.rs            # doorbell + sleep handshake (abstract pending-IPI model)
  tests/loom_retire.rs           # kill bit vs wake CAS; Retire-node re-post chase; Adopt-under-kill
kernel/src/sched/                # driver half; kernel/src/scheduler.rs survives as the kernel-facing API
  mod.rs                         # module root over driver/payload/waitqs
  driver.rs                      # percpu CpuSched slot, idle loop, asm switch, trampoline, irq_ring
  payload.rs                     # KernelPayload/ThreadSched — the kernel's SchedPayload
  waitqs.rs                      # WaitQueue instances owned by pipe/futex/listener/audio/io_uring/hid/net
kernel/src/hw.rs                 # KernelHw: LAPIC one-shot (TSC-deadline later), targeted x2APIC.
                                 # At kernel/src/, NOT kernel/src/sched/: it is the machine surface,
                                 #   implementable before anything names a task (§10.1)
```

ARM64 portability: only `hw.rs` implementors and `driver.rs` asm are arch-specific (GIC timer,
`ICC_SGI1R` kick, `wfi`, DAIF guard); the core crate has no arch code and no `#[cfg(target_os)]`.

Unsafe policy: `#![deny(unsafe_code)]` at the crate root. `mailbox.rs` is the only module with
`#[allow(unsafe_code)]` (intrusive links). `RunToken`'s raw pointers are constructed by safe code
(`&raw` into stable Box-backed records) and consumed by the driver's `unsafe Hw::switch`.

## 5. The ownership model

### 5.1 The linear task value

```rust
// toyos-sched/src/task.rs

/// Monotonic, never reused. Stale messages keyed by TaskKey are provably about
/// a dead task and are benign no-ops.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TaskKey(pub u64);

/// The single owning value for a live thread. !Copy, !Clone. Box: the record has
/// a stable heap address for the task's lifetime, so raw pointers into it taken
/// before a container move stay valid (kills B1's enabling condition).
pub struct Task<X>(Box<TaskInner<X>>);

struct TaskInner<X: SchedPayload> {
    key: TaskKey,
    shared: Arc<TaskShared<Msg<X>>>,  // state word + sticky bits + embedded nodes; outlives death
    share: Arc<Share<X>>,      // per-process fairness pot (§9.1); Share<X> = FairShare<X::ShareLock>
    ctx: X::Ctx,               // saved callee context (named `ctx`, not `arch_ctx`)
    rt: RtState,               // { permanent, inherited, lends } — see below
    acct: TaskAccounting,      // cpu ns, blocked-by-class ns, runqueue-wait ns, timestamps
    since: Nanos,              // when the CURRENT residency began — one field, one state
    adopt_node: MailboxNode<Msg<X>>,  // this task's Adopt message rides inside its own record
    ext: Option<X>,            // Option, not X: taken by finalize; still present at drop is the
                               // drop bomb that turns an illegal death into a panic at the site
}

pub trait SchedPayload: Sized + Send + 'static {
    type Ctx: Sized + Send;
    /// The cell the per-process FairShare lives in — supplied by the environment
    /// because the core crate may not implement a lock itself.
    type ShareLock: LeafLock<ShareState> + Send;
}
```

`RtState` carries a third field the spec omitted: **`lends: u32`**, the count of lends that
actually extended the window. It lives in the core because invariant I9 — running time at the
borrowed priority, *per lend* — is not checkable without it: `RtState::arm` moves `inherited`
forward on re-arm, so an outside observer watching the deadline alone cannot tell a fresh grant
from a re-arm.

**Deliberate omission (container-resident state data):** `TaskInner` has **no `deadline` field,
no `blocked_on`, no `enqueued_at`** — but it does have `since`, one timestamp reused across
states (enqueue time while ready, dispatch time while running, park time while blocked),
precisely because a task is in exactly one state at a time. Data meaningful only in one lifecycle state lives in that
state's container (§6.1) — a task that is not parked *structurally cannot have* a deadline.

**Linearity enforcement:** `impl Drop for TaskInner` panics. The only legal death is
`DeadTask::finalize()`, which disassembles via `ManuallyDrop`. An accidentally dropped or leaked
task value — the B1 double-drop class — dies loudly instead of silently corrupting refcounts.

### 5.2 Five states, five types

Each is a `#[must_use]` newtype with a private field; no `Copy`, no `Clone`, drop-bomb inherited.

```rust
pub struct ReadyTask<X>(Task<X>);    // exists only inside RunQueue (or as insert's argument)
pub struct RunningTask<X>(Task<X>);  // exists only in CpuSched.running
pub struct BlockedTask<X>(Task<X>);  // exists only inside a ParkedEntry in CpuSched.parked
pub struct TransitTask<X>(Task<X>);  // exists only inside a mailbox Msg::Adopt payload
pub struct DeadTask<X>(Task<X>);     // exists only in CpuSched.zombie, until finalize()
```

**The complete transition table.** These are the only functions that exist; every one consumes
`self`. A transition not in this table does not compile.

| From | To | Function (crate-private, invoked only by `SchedPass`/mailbox handlers) | Trigger |
|---|---|---|---|
| (spawn) | `TransitTask` | `TaskBuilder::build(entry: X::Ctx, ext: X) -> TransitTask<X>` | spawn |
| `TransitTask` | `ReadyTask` | `adopt(self, cpu, now) -> Result<ReadyTask<X>, DeadTask<X>>` — kill bit checked | `Msg::Adopt` handled |
| `ReadyTask` | `RunningTask` | `dispatch(self, cpu, now) -> RunningTask<X>` — asserts the `Ready(cpu) → Running(cpu)` transition, **not** the kill bit (see below) | pick |
| `ReadyTask` | `TransitTask` | `migrate(self, from, dst, now) -> TransitTask<X>` | balance decision |
| `ReadyTask` | `DeadTask` | `reap(self, cpu, now) -> DeadTask<X>` | kill bit observed |
| `RunningTask` | `ReadyTask` | `preempt(self, cpu, now) -> ReadyTask<X>` — clears expired inherited RT | quantum/yield |
| `RunningTask` | `BlockedTask` | `park(self, ticket: &CommittedTicket, cpu, now) -> BlockedTask<X>` — ticket **by reference**: parking does not consume the proof | block |
| `RunningTask` | `DeadTask` | `die(self, cpu, now) -> DeadTask<X>` | exit / kill honored |
| `BlockedTask` | `ReadyTask` | `wake(self, cpu, cause: WakeCause, class: WaitClass, now) -> ReadyTask<X>` — applies boost | `Msg::Wake` / local deadline |
| `BlockedTask` | `DeadTask` | `reap(self, cpu, class: WaitClass, now) -> DeadTask<X>` | `Msg::Retire` handled |
| `DeadTask` | (gone) | `finalize(self) -> (TaskKey, X, TaskAccounting)` — exactly once | next pass, different stack |

Every transition takes `cpu` and `now` as values — `cpu` because identity is never an ambient
query (§6.1) and `now` because the driver samples the clock once per pass and threads it, so a
replay is reproducible. The spec's original signatures omitted both throughout.

**Why `dispatch` does not assert the kill bit clear.** A remote CPU sets that bit at any
instant, so the assert would be a race rather than a check — it would fire on legal
interleavings. `task.rs:757-761` says so at the call site. The guarantee is not weakened, it is
moved: the pass *reaps* a killed task instead of dispatching it, in `CpuSched::pick`
(`cpu.rs:811-823`), with `TransitTask::adopt` and `WaitTicket::commit` covering the other two
entries into `Ready`. Recorded because the shape alone invites someone to "restore" the assert.

Theorems the compiler proves: a task cannot be queued and dead simultaneously (two owners of one
linear value); a migrating task cannot be requeued by its home CPU (it exists only inside an
unconsumed message); the `address_space` Arc in `X` is dropped exactly once (returned exactly once
by `finalize`, which consumes the only owner). The crash.md UAF is not writable.

### 5.3 The rendezvous word — runtime shadow, not the truth

Ownership truth is the linear value. Remote wakers need a lock-free rendezvous. `TaskShared`:

```rust
pub struct TaskShared {
    /// Packs {discriminant, cpu, commit_gen} + sticky KILL + sticky RETIRE_QUEUED.
    /// States: Running(c) | Ready(c) | Committing(c,gen) | Blocked(c) | WakeQueued(c)
    ///       | InTransit(dst) | Dead
    state: AtomicU64,
    wake_node:   MailboxNode,   // ≤1 in flight, guaranteed by the Blocked→WakeQueued CAS
    retire_node: MailboxNode,   // ≤1 in flight, guaranteed by the RETIRE_QUEUED sticky bit
    waiting:     AtomicBool,    // membership in ≤1 WaitQueue (multi-wait is io_uring's job)
}
```

**The intrusive `wait_node` is still owed.** The field above is a plain `AtomicBool`
(`task.rs:268-281`): queues hold waiters as `Arc<TaskShared>` in a `VecDeque`
(`waitq.rs:40-42`), and `waiting` is the fail-fast check that a task never registers on two
queues — the *invariant* the node was for, without the allocation-free list. `waitq.rs:20-26`
records this as a deliberate departure taken to keep `unsafe` confined to `mailbox.rs`, and
says the intrusive list is still owed. It is the same debt as the parked-thread wait-queue node
leak in `specs/issues/`.

Legal CAS edges mirror §5.2 exactly and are written in one `match` in `task.rs`; any other
observed edge panics. `feature = "check"` verifies word-vs-container agreement at every
transition — the fail-fast layer that would have preserved crash.md's evidence instead of
recursing. **There is no `TaskRef` type.** The non-owning handle is `Arc<TaskShared>` itself, and the
waker funnel is a *free function*, `waitq::wake_direct(&Arc<TaskShared>, cause, ...)`
(`waitq.rs:236-251`) — waking a dead task is a failed claim CAS → benign no-op, as described.
The kernel's stand-in for the richer handle this spec imagined is `sched::payload::ThreadSched`.
This spec names `TaskRef` in five places; none of them resolve to anything in the tree.

## 6. The per-CPU machine

### 6.1 `CpuSched`: exclusive by construction

```rust
// toyos-sched/src/cpu.rs
pub struct CpuSched<X: SchedPayload> {
    id: CpuId,                                   // identity is a FIELD, never an ambient query
    running: Option<RunningTask<X>>,
    rq: RunQueue<X>,                             // §9.2
    parked: BTreeMap<TaskKey, ParkedEntry<X>>,   // BTreeMap, not HashMap (no_std, ordered).
                                                 //   Deadlines live in the entries and nowhere
                                                 //   else — there is no second index (§8.3)
    zombie: Option<DeadTask<X>>,                 // freed by the NEXT pass (can't free the stack we run on)
    mailbox: MailboxConsumer<Msg<X>>,
    steal_probe: MailboxNode<Msg<X>>,            // NOT a StealNode — see §7.7
    steal_requests: Vec<CpuId>,                  // thieves that asked this pass; answered in `finish`
                                                 //   from surplus, AFTER the pick, so answering can
                                                 //   never hand away the task we were about to run
    quantum_end: Nanos,
    loaded: Loaded,                              // which context is loaded — distinct from `running`,
                                                 //   which is None between a park and the switch off
                                                 //   the parked task's stack
    loaded_ctx: *mut X::Ctx,
    idle_ctx: Box<X::Ctx>,
    armed: Option<Nanos>,                        // what the one-shot timer is programmed to (invariant T)
    #[cfg(feature = "protocol-port")]
    park_keeps_lapsed_lend: bool,                // negative-gate escape hatch only
    _not_sync: PhantomData<*mut ()>,             // !Sync, !Send
}

pub struct ParkedEntry<X> {
    task: BlockedTask<X>,
    deadline: Option<Nanos>,      // THE deadline — exists only while parked (CT, §5.1)
    class: WaitClass,             // Io | Futex | Pipe | Ipc | Other — accounting
    // No `since`: the park timestamp lives in `TaskInner.since`, which is the
    // one-field-one-state rule of §5.1 applied here rather than an omission.
}
```

**There *is* a global array of `CpuSched`, and the exclusivity is enforced at runtime rather
than by the type.** `driver.rs:102-110` declares `static SCHEDS: [SchedSlot; MAX_CPUS]`, where
`SchedSlot` wraps an `UnsafeCell<Option<CpuSched<KernelPayload>>>` and carries an
`unsafe impl Sync`. Nothing is leaked into a percpu block. The guarantee is real but it comes
from `with_cpu` indexing by the *calling* CPU's own id, plus the `IN_PASS` reentry flag — the
safety comment at `:104` states exactly that, and `CpuSched` being `!Sync` is what stops
anything inside escaping by another route.

`driver::with_cpu(f)` panics on reentry (`IN_PASS` — the typed replacement for `IN_SCHEDULE`;
a nested pass is a bug, not a deferral).

The only globally shared, `Sync` objects are per-CPU **handles**:

```rust
pub struct CpuHandle<X> {         // one per CPU, in a boot-initialized slice
    post: MailboxProducer<X>,
    doorbell: AtomicU32,          // bit0 KICK_PENDING, bit1 SLEEPING
    load: AtomicU32,              // published ready-count heuristic (placement only)
}
```

Remote code can only reach the handle, and the handle can only post messages — the compile-time
form of "each CPU's runqueue is touched only by its own CPU".

### 6.2 `SchedPass` — the only way to touch `CpuSched`

```rust
pub struct SchedPass<'c, X, S: PassState> { cpu: &'c mut CpuSched<X>, now: Nanos, _s: PhantomData<S> }
pub enum Undisposed {} pub enum Disposed {}

impl<'c, X> SchedPass<'c, X, Undisposed> {
    /// `now` is sampled ONCE by the driver (Hw::now()) and threaded as a value —
    /// the current code reads the clock ~15x mid-flight, which is irreproducible
    /// in a simulator and the source of deadline-vs-arming skew. Entry drains the
    /// mailbox, fires due local deadlines, charges the running task's vruntime.
    /// Runs with preemption disabled, IRQs enabled.
    // There is no `CpuToken`. The pass borrows the CpuSched directly and carries
    // its environment: `Env { hw, cpus, frontier, preempt, steal }`.
    pub fn begin(cpu: &'c mut CpuSched<H::Payload>, env: Env<'e, H, P>, now: Nanos) -> Self;
    // `Env.steal` is a runtime FIELD, not a cfg — whether an idle pass probes and a
    // loaded pass answers (§7.7, §9.4's pull half). A field so that both settings stay
    // compiled and simulatable.

    // Exactly one disposition; each consumes the pass:
    pub fn dispose_yield(self)                     -> SchedPass<'c, X, Disposed>;
    pub fn dispose_block(self, t: CommittedTicket, deadline: Option<Nanos>)
                                                   -> SchedPass<'c, X, Disposed>; // parks BEFORE the switch
    pub fn dispose_exit(self)                      -> SchedPass<'c, X, Disposed>;
    pub fn dispose_none(self)                      -> SchedPass<'c, X, Disposed>;
}

impl<'c, X> SchedPass<'c, X, Disposed> {
    /// The ONLY exit. Picks next (RT band first), performs migrations decided this
    /// pass, finalizes last pass's zombie, and — LAST — programs the timer from
    /// min(quantum_end, deadline-heap min). Timer-after-everything is why the
    /// armed-if-needed invariant holds by construction (kills B5).
    pub fn finish(self) -> Action<X>;   // takes no `hw` — the pass already holds its Env
}

#[must_use]
pub enum Action<X> {
    Run(RunToken<X>),     // switch to this task
    Resume,               // decided not to switch; whatever was loaded stays loaded
    Idle(SleepToken),     // nothing runnable; may sleep with this token (§7.5)
}
```

**`Action` has three variants, not two** (`cpu.rs:100-110`). `Resume` exists so that "the pass
decided not to switch" is its own answer rather than a `Run` whose `restore` and `save` are the
same context — which would make a self-switch representable.

**No guard across the switch — structurally.** `finish` consumes the pass; when `Action` is
returned every borrow of `CpuSched` has ended. The entire kernel switch path:

```rust
// kernel/src/sched/driver.rs
match pass.finish(&HW) {
    Action::Run(tok) => unsafe { HW.switch(tok) },   // consumes the owned token (B2 dead)
    Action::Idle(sleep) => HW.idle_wait(sleep),
}
// Code here runs when THIS task is next resumed. Nothing to unlock, nothing to park:
// handle_outgoing, force_unlock, into_raw, finish_fresh_thread_switch cease to exist.
```

`RunToken<X>` holds `{ restore: *const X::Ctx, save: *mut X::Ctx }` into the stable Box-backed
records. Park-before-switch is sound *only because of per-CPU ownership*: a wake for the
just-parked task arrives as a message to this same CPU and cannot be processed until the next
pass, which necessarily runs after the switch completes. The stack-reuse race is sequentially
impossible, not locked away. Fresh tasks enter via a trampoline frame built by `TaskBuilder`; no
special-cased unlock tail (the `loader.rs` `scheduler_unlock` path is deleted).

### 6.3 Safe points (exhaustive)

1. **Voluntary**: `block_on` / `yield_now` / `exit` in syscall paths.
2. **IRQ-exit preempt**: kernel→user return and the `preempt::enable` slow path when
   `need_resched` is set (by timer IRQ, kick IPI handler, or a local RT wake).
3. **Idle loop**: every iteration.
4. **Kick IPI** (targeted x2APIC vector): handler sets `need_resched` only; the pass runs at IRQ
   exit. Broadcast kicks are gone.

IRQ handlers never touch `CpuSched`. They may only: push records into driver-owned rings
(`irq_ring`, §8.6), call the wake entry (§8), and set `need_resched`. This is what makes
`&mut CpuSched` sound with IRQs enabled.

### 6.4 The lock-across-switch tripwire

`kernel/src/scheduler.rs` entry points assert, before constructing any pass:

```rust
// scheduler.rs:42-49 — `assert!`, not `assert_eq!`, and `preempt::count()`,
// not `preempt::depth()`. `#[track_caller]` all the way down is what makes the
// message point at the offending caller instead of at these three lines.
#[track_caller]
fn assert_baseline(baseline: u32) {
    let depth = crate::preempt::count();
    assert!(
        depth == baseline,
        "scheduler entered while a lock is held: preempt depth {depth}, baseline {baseline}",
    );
}
```

Every kernel `Lock` guard increments the preempt count, so *calling the scheduler while holding
any spinlock panics immediately at every present and future call site*. Runtime, but exhaustive —
it uses the counter every lock already maintains. Combined with §6.2 this closes B2 twice over.

Two things the assert needs that do not follow from the rule alone:

1. The baseline is two constants, not one: `BASELINE_TRAP` (1) for a syscall or fault handler,
   which runs one level deep for its whole body, and `BASELINE_IRQ_EXIT` (0) for the
   deferred-preempt poll, which runs past that level. Both are measured, never derived.
2. The preempt count must be *conserved* across a context switch. Contexts owe different numbers
   of `enable`s, so the depth belongs to the context — `KernelCtx` carries it and `Hw::switch`
   swaps it with the per-CPU word.

Wake paths and the panic-path exit `schedule_no_return` do not assert, and
`kernel/src/scheduler.rs` says why at each of them.

## 7. Cross-CPU messages

### 7.1 Message set

```rust
// toyos-sched/src/mailbox.rs
pub enum Msg<X> {
    /// Target CPU owns the parked task; this is a request, not a transfer.
    Wake { key: TaskKey, cause: WakeCause },
    /// Ownership transfer: spawn placement, migration, wake-forwarding.
    Adopt { task: TransitTask<X> },
    /// Slow-path balance probe: "if overloaded, send me one" (§9.4).
    StealRequest { thief: CpuId },
    /// Kill protocol (§7.6).
    Retire { key: TaskKey },   // no `notify` — deliberately; see below
}
```

### 7.2 Mailbox: intrusive MPSC, overflow unrepresentable

One Vyukov intrusive MPSC per CPU. Producer = any CPU or IRQ context; consumer = owner CPU at pass
start. Push: `node.next = null; prev = XCHG(tail, node); prev.next = node`.

- **Nodes are embedded, never allocated, never counted**: `Wake` uses `TaskShared.wake_node` (at
  most one in flight, guaranteed by the `Blocked→WakeQueued` CAS — only the winner posts);
  `Retire` uses `TaskShared.retire_node` (at most one in flight, guaranteed by the
  `RETIRE_QUEUED` sticky bit, §7.6); `Adopt` travels inside `TaskInner.adopt_node` (a task is in
  at most one transit trivially); `StealRequest` uses the per-CPU `steal_probe` (§7.7).
  **Overflow has no representation.** There is no capacity to size wrong, no `EVENT_QUEUE_SIZE`,
  no silent drop, no scan-fallback second delivery path, and no spin-then-panic pressure valve.
  This is the resolution of B8 and the explicit answer to the other designs' full-ring holes:
  ownership-carrying messages cannot be dropped because the message *is* the owner.
- **Push runs under preemption-disable — mandatory.** A producer preempted between the XCHG and
  the `next` store would strand *other CPUs'* subsequently-pushed messages behind the unlinked
  suffix with no doorbell edge left to raise. All thread-context producers must push inside a
  preempt-disabled region (the wake path already is: the claim CAS and post happen under the
  waitq leaf lock's IRQ-off window, §8.1). IRQ-context producers cannot be preempted. The
  remaining torn-push case — an IRQ interrupting a *same-CPU* push — leaves a transiently
  unlinked suffix that the consumer treats as end-of-queue; safe because the doorbell guarantees
  a follow-up pass and the interrupted store completes before that context ever sleeps. Both
  interleavings (IRQ-tear and the forbidden preempted-producer strand) are dedicated
  `loom_mailbox.rs` cases; the second must FAIL when the preempt-disable is modeled away.

Rejected alternative — N×N SPSC matrix: O(N²) memory (≈4 MB at 128 CPUs), a per-ring overflow
cliff (a reintroduction of B8), no verification-burden reduction. The MPSC producer line is
contended only when multiple CPUs wake one CPU simultaneously — inherently serialized work.

### 7.3 Posting and kick policy

```rust
// every producer, no exceptions
preempt_disabled(|| target.post.push(msg));                 // release
let prev = target.doorbell.fetch_or(KICK, AcqRel);
let ipi = match urgency {
    // RT wake, boost wake, Adopt of an RT task, Retire: prompt preemption required.
    // Unconditional — a prior normal-wake KICK edge may not have IPI'd.
    Urgency::Preempt => true,
    // Normal wake to a busy CPU needs no interrupt for correctness: the target
    // drains at its next safe point, ≤ one quantum — matching today's latency.
    // SLEEPING targets always get the (edge-coalesced) IPI, preserving B4 safety.
    Urgency::Normal  => prev & SLEEPING != 0 && prev & KICK == 0,
};
if ipi { hw.kick(target_id); }
```

Consume protocol (pass start): `doorbell.fetch_and(!KICK)` *before* draining; a message posted
after the drain re-raises the edge → new IPI when needed. This is the Design-1 kick-elision graft:
at 128 cores a normal wake to a busy CPU costs zero IPIs, while every path that could strand work
on a sleeping CPU still kicks.

### 7.4 RT preemption path (kills B7)

Wake with RT cause for a task homed on CPU `c`, initiated from CPU `a` (thread or IRQ context):

1. `wake_one`: claim CAS `Blocked(c) → WakeQueued(c)`; the winner posts `Wake{key, cause}` with
   `Urgency::Preempt` → targeted IPI, always.
2. `c` in user mode: IPI → IRQ exit → pass. `c` in hlt: IPI ends hlt → idle pass. `c` in kernel:
   pass at the `preempt::enable` slow path or next boundary (bounded by the longest
   preempt-disabled section — modeled in the sim as `KernelSection` budgets, §10.1).
3. The pass drains the mailbox first, so the RT task is in the RT band *before* pick; the current
   normal task is preempted. End-to-end: IPI delivery + IRQ exit + one pass. Sim invariant I4
   asserts the bound.
4. Home CPU already running RT: the pass forwards the woken RT task via `Adopt` to an idle CPU
   found by `idle_sibling` (`cpu.rs:424-431`) — a linear scan of peer doorbells for a set
   SLEEPING bit. **There is no published sleep mask**; the per-64-CPU `AtomicU64` aggregating
   SLEEPING bits described here was never built, and the scan is O(cpus) on the RT wake path.

### 7.5 The idle/hlt race — sleep handshake (kills B4)

```rust
#[must_use] pub struct SleepToken { /* private; constructed by finish() only when:
    rq empty ∧ timer programmed for the deadline-heap min ∧ SLEEPING was set
    BEFORE the final mailbox-empty check */ }
```

```
consumer (idle path):                      producer (any post):
  doorbell |= SLEEPING                       push(msg)                 [preempt-disabled]
  drain mailbox; rq still empty?             prev = doorbell.fetch_or(KICK)
  finish() -> Action::Idle(SleepToken)       ipi per §7.3 (SLEEPING seen ⇒ kick)
driver idle_wait(token):
  cli
  if doorbell & KICK or mailbox nonempty: sti; return   // retry loop
  sti; hlt        // STI shadow: an IPI arriving after cli is pending and terminates hlt
```

Any message not seen by the final check has a subsequent KICK 0→1 edge with SLEEPING set → an IPI
is in flight → hlt terminates. SLEEPING is cleared at the next non-idle pass begin. Verified three
ways: `loom_sleep.rs` (abstract pending-IPI flag), sim invariant I2, and the B4 regression
scenario. The old "final re-check of five ad-hoc flags under cli" collapses into one token.

### 7.6 Retire protocol (replaces `retire_task`, `KILLED`, `WAKE_TRANSITS`)

**`Msg::Retire` carries no `notify`, for two independent reasons — keep both.** First, a notify
riding the message would be a second wake path, which is exactly what §8.2 exists to prevent,
and it would have to outlive the message anyway: a *running* target consumes the retire and
dies at some later safe point (`msg.rs:49-54`). Second, and the subtle one — joiners must wait
for the payload to be dropped, not for the state word. `Dead` is published by the reaping
transition **one pass before** `Hw::release`, while the dying CPU is still standing on that
thread's kernel stack (`kernel/src/scheduler.rs:349-356`). A notify fired on `Dead` would
release a joiner that then frees memory the dying CPU is still using. Joiners wait on the
environment's finalize sink instead, which runs once per task and after the payload is gone.

1. Retirer: CAS the sticky `RETIRE_QUEUED` bit. A second concurrent retire of the same task is a
   kernel bug (exactly one retirer exists: process teardown / thread kill) → panic, fail fast.
   Then `kill_bit.fetch_or(KILL)` (sticky, first), read the location word, post
   `Retire{key, notify}` to the home CPU with `Urgency::Preempt`.
2. Home CPU pass: task in `parked`/`rq` → `reap → DeadTask` → zombie → finalize next pass →
   `notify.wake_direct()`. Task is `running` here → set `need_resched`; it dies at its next safe
   point (bounded by the quantum). Word says `InTransit(dst)` or another CPU → **chase**: re-post
   `Retire` using the *same* `retire_node` — legal because the node was just unlinked by this
   consumer. Termination: the kill bit is already set, so any CPU that adopts the task converts
   it to `DeadTask` on arrival; the chase is ≤ the number of in-flight hops (≤1 in practice).
3. Wake racing retire: both are messages to the same consumer, handled in order. A `Wake` that
   loses finds no parked task and no-ops (keys never reuse). The concurrent case where a `Wake`
   node and a `Retire` node for the same task are in flight simultaneously is well-formed by
   construction: they are two distinct embedded nodes. `loom_retire.rs` covers kill-bit-vs-wake
   ordering, the retire-node re-post chase, and adopt-under-kill; the sim scenario
   `crash_md_exit_race` replays the crash.md shape and asserts single ownership + mock-Arc
   refcount sanity every step.

4. **A killed task is never handed on.** Step 2's correctness argument is about ownership and says
   nothing about promptness, and the two diverge in exactly one state. Every other state's reap
   has an interrupt behind it: the `Retire` is `Urgency::Preempt` and always kicks, and a *running*
   target's `need_resched` is a kick. `InTransit` has none — it is reaped by the `Adopt` that
   carries it, and an `Adopt` of a non-RT task is `Urgency::Normal`, whose whole purpose is that a
   busy destination is *not* interrupted. So both callers of `hand_off` — `answer_steal_requests`,
   which exports from the *back* of the fair band, and `place`, which forwards a boosted wake to a
   sleeping sibling — read the kill bit and reap where they stand. That is strictly less work than
   migrating, and it makes the weak state unreachable by decision: no CPU puts a task it knows is
   dead into transit. What remains is a kill that lands after the `Adopt` was posted, which no
   protocol removes, and that case always has a `Retire` aimed at the same CPU. Invariant I14 is
   the check; `old_migrate_kept_the_corpse` is the negative gate. Evidence: a live panic on the
   T14, `retire_task: task not released after 1s: InTransit(CpuId(1))`.

Deleted wholesale: `KILLED[16]`, `mark_killed`'s 16-concurrent-retire panic, `WAKE_TRANSITS`,
`TransitGuard`, `scan_remove`, poison-set rescheduling filters.

Panic recovery leaves through `scheduler::schedule_no_return()` (`kernel/src/scheduler.rs:449`);
the faulted context dies through an ordinary `Running → Dead` transition on its own CPU. Neither
`mark_kill` nor an `abandon_current` mechanism is involved — `mark_kill` has zero kernel callers
(`toyos-sched` uses it internally from `waitq.rs`, and the simulator calls it), and no
`abandon_current` exists anywhere in the tree outside §12's table.

### 7.7 StealRequest node recycling

```rust
// There is no `StealNode`. `steal_probe` is a bare `MailboxNode<Msg<X>>`
// (cpu.rs:143-147): the generic in-flight claim flag every node kind already
// has *is* the "a probe is already outstanding" answer, so a dedicated type
// would have been a second mechanism for the same question. Behaviour is as
// described below. One guard is undocumented: `post_steal_probe` skips a
// victim whose published load is < 2 (cpu.rs:950) — a CPU with one task has
// nothing to give without going idle itself.
```

An idle pass posts a probe only if `!in_flight.swap(true, AcqRel)`; if a probe is still
outstanding, it simply doesn't post another (harmless — the previous probe is still pending, and
the thief is about to sleep with its doorbell armed). The victim clears `in_flight` (Release)
strictly **after** unlinking the node from its queue, and then — if it has surplus
(`rq.fair_len() > 1`) — pops one fair-band task, `migrate`s it, and posts `Adopt` to the thief.
The node's lifecycle is: free → in thief's post → linked in victim's mailbox → unlinked → free.
No allocation, no recycling race; covered in `loom_mailbox.rs`.

## 8. One wait/wake primitive

### 8.1 `WaitQueue` + the ticket handshake

Every waitable kernel object (pipe end, futex bucket, listener, io_uring CQ, driver queues) owns a
`WaitQueue`. Waiter nodes are embedded in `TaskShared.wait_node` (a task waits on at most one
queue — multi-wait is io_uring's job, aligned with the io_uring-only-blocking direction). The
queue's internal list is protected by a leaf lock the **environment supplies** as the type
parameter `L: LeafLock<WaitList<M>>` — the kernel passes its IRQ-off leaf lock, loom passes a
`loom::sync::Mutex`. There is no `IrqSafeRaw` type in the core; inverting it this way is what
keeps `unsafe` confined to `mailbox.rs`. The discipline is unchanged: a few-instruction critical
section that never acquires anything beneath it, the only lock the wake path holds, never held
across a pass or a switch.

```rust
// toyos-sched/src/waitq.rs
pub struct WaitQueue<M: SchedMsg, L: LeafLock<WaitList<M>>> { /* waiters behind L */ }

impl<M, L> WaitQueue<M, L> {
    /// Phase 1: register current task; CAS Running -> Committing(gen).
    #[must_use] pub fn prepare_wait<'q>(&'q self, cur: &CurrentTask<'_, M>) -> WaitTicket<'q, M, L>;
    /// The ONLY wake entries in the entire kernel:
    pub fn wake_one(&self, cause: WakeCause) -> bool;   // returns whether one was claimed
    pub fn wake_all(&self, cause: WakeCause);
}
impl<'q, M, L> WaitTicket<'q, M, L> {   // !Send, #[must_use]
    pub fn cancel(self) -> Cancelled;   // condition became true; dequeues, Committing -> Running
    pub fn commit(self) -> Commit<'q, M, L>;   // phase 2 — see below
}

/// The three answers phase 2 can give. The spec previously named none of them.
pub enum Commit<'q, M, L> {
    /// Word is `Blocked`; the pass may park. The `Registration` MUST be finished.
    Parked(CommittedTicket<M>, Registration<'q, M, L>),
    /// A wake landed between registration and commit: do not park, do not switch.
    AlreadyWoken,
    /// A retire landed while deciding to park. Registration already withdrawn,
    /// word back to `Running(cpu)`: dispose the pass by *exiting*, not parking.
    Killed,
}

// Phase 3, in kernel/src/scheduler.rs (NOT sched/mod.rs):
pub fn block_on(ticket: Ticket<'_>, deadline: u64);
// join/waitpid/sleep funnel: waitq::wake_direct(&shared, cause, ..) — free fn, same claim CAS.
```

**`Registration` is the piece the spec omitted entirely, and it is load-bearing.** A *timeout*
leaves the waiter on the queue: the local deadline fire wins the same claim CAS a waker would,
and the core does not know which queue the task was on — nor should it, since the scheduler
knows only tasks, tickets and causes. The leftover is not untidy, it is a lost wake: once the
task parks on a *second* queue, a `wake_one` on the first finds it `Blocked`, claims it, and
spends that queue's wake on a waiter no longer waiting for it. Holding the guard across the
block, on the task's own stack, makes the cleanup structural instead of one more per-source
recheck — and its `Drop` asserts, so forgetting to `finish()` is a panic at the site rather
than a wake that vanishes later.

Uniform usage (pipe read shown; futex inserts its value check; io_uring checks CQ depth):

```rust
loop {
    if let Some(n) = pipe.try_read(buf) { return n; }
    let t = pipe.readers.prepare_wait(&cur);
    if pipe.has_data() { t.cancel(); continue; }        // closes the check-then-block TOCTOU
    match block_on(t, deadline) { WakeReason::Woken(_) | WakeReason::Timeout => continue }
}
```

State-word protocol: `prepare_wait` CASes `Running → Committing(gen)`. `block_on`'s pass tries
`Committing(gen) → Blocked`; failure means a wake landed between registration and commit → dequeue
if needed, return `AlreadyWoken` **without switching**. This one protocol replaces the
IoUring-only `handle_outgoing` recheck, the missing pipe/audio/listener rechecks,
`FUTEX_LOCK` + `FUTEX_WAKE_GEN`, and the overflow-recovery scans. `EventSource` as a scheduler
concept is deleted; the scheduler knows only tasks, tickets, and causes.

### 8.2 `wake_one` retries until a claim succeeds

The waker performs the claim CAS synchronously, **at the waker** — and must loop:

```rust
pub fn wake_one(&self, cause: WakeCause) {
    loop {
        let Some(shared) = self.pop_front_locked() else { return };
        match claim(&shared) {
            // CAS Blocked(c) -> WakeQueued(c): we own the wake; post Msg::Wake to c.
            Claim::Parked(c)  => { post_wake(c, shared.key, cause); return }
            // CAS Committing(gen) -> WakeQueued: waiter hasn't parked yet; its own
            // block_on commit will observe this and refuse to park. No message needed.
            Claim::PrePark    => return,
            // CAS failed: a local deadline fire or retire beat us — this waiter is
            // no longer waiting. Its node was stale. Try the NEXT waiter; a wake_one
            // must never be satisfied by a corpse.
            Claim::Lost       => continue,
        }
    }
}
```

The `Claim::Lost → continue` arm is load-bearing: without it, a `wake_one` racing a waiter's
timeout would consume the wake and strand a second waiter forever (the futex-storm shape). This
resolves the wake-vs-timeout arbitration hole flagged against the original design. `wake_all`
claims every node, posting for each `Parked` win. All wakes — pipe, futex, listener, io_uring CQE,
device ISR tails, join, timer — terminate in this one `claim` routine; there is no second path.

### 8.3 Deadline timeouts (owner-local)

Blocked-with-deadline tasks park locally; only ready tasks migrate. Every deadline therefore
lives in exactly one place: the `ParkedEntry` of the task that owns it, on that task's home CPU.
**There is no separate index, and that is the design.** An ordered index holds the deadline a
second time, and every pass that ends a park has to retire both copies; the one that did not was
`fire_deadlines`' lost-claim arm, which discarded the index entry and left the entry's copy
standing — a CPU reporting a deadline nothing will fire, indistinguishable from health to
`sched::dump` and to anything else that reads it. `earliest_deadline` and `next_due` scan
`parked`, which is O(parked-on-this-CPU) and sits on the pass path; the number that would change
this decision is a machine with hundreds of threads blocked on one CPU, and it is not this one.

Fire path: pass start walks for a due entry → CAS `Blocked(c) → WakeQueued(c)` *locally*. CAS
success → the owner wakes it with `WakeReason::Timeout` (dequeue from its waitq under the leaf
lock — idempotent). CAS failure → a remote waker won; its `Wake` message is in our mailbox or in
flight (doorbell KICK set → this CPU will pass again before sleeping); the timeout is superseded,
**and the entry's deadline is cleared**, because no later claim can succeed either and a deadline
that will not be honoured must not be reported as pending. Same arbitration CAS as §8.2, no
special cases. Gated by `sim/tests/deadline_claim_race.rs`, which is the only thing that can
construct that arm at all — see §10.5's note on I3.

### 8.4 The armed-if-needed invariant (kills B5)

**Invariant T (provable, checked):** whenever CPU `c` is outside a pass:
`armed_deadline(c) ≤ min(quantum_end(c) if running, earliest deadline in parked(c))`, and hlt
with any parked deadline implies the timer is armed to the earliest. Proof: (a) `parked` is only
mutated inside a pass on `c` (the block disposition inserts, wake/retire/timeout remove); (b)
`finish()` is the only pass exit and programs the timer *after* all of them; (c) `SleepToken`
construction requires the timer plan applied; (d) the arming reads the same field the invariant
reads, so no second copy can be the one that is right. `ensure_armed_before`, the global
`LAST_ARMED_TICKS` saga, and the "deadline honored 7 ms late" audio failure disappear
structurally. Sim invariant I3 re-checks after every step; kernel `feature="check"` builds assert
it at pass end.

### 8.5 Priority inheritance for the audio path (audio spec §5.10)

```rust
pub struct WakeCause { pub reason: WakeReason, pub boost: Option<BoostWindow> }
pub struct BoostWindow { pub until: Nanos }
```

- soundd (permanent RT via the privilege-gated syscall) signals clients:
  `pipe.readers.wake_all(WakeCause { boost: Some(now + period), .. })` — blocked clients wake
  with `rt.inherited = Some(until)` and land in the RT band.
- A client not yet blocked at signal time: the pipe keeps `boost_until: AtomicU64` (today's
  `set_rt_boost_pending`, made time-bounded); its consume path calls
  `cur.boost_inherited(until)`.
- Expiry is a **time bound**, not "until next block": `preempt`/`park` clear `inherited` when
  `now >= until`. This upgrades today's unbounded "spinning boosted client keeps RT forever" hole
  into the spec's ~one-period bound. Sim invariant I9.
- **The bound is on time *held*, not on wall clock since the lend.** A task
  waiting in a run queue holds no priority, so queue time spends none of the window: `dispatch`
  **arms** it — a window already lapsed when the task is picked is re-armed to `now + QUANTUM_NS`
  — and `preempt`/`park` clear it as above. The pick does **not** check it and never demotes a
  queued task out of the RT band.

  Read literally, the original wording admits an implementation in which a boosted task that is
  slow to reach a CPU is demoted to the fair band *before it has run at all*, which inverts the
  lend: the task lands behind exactly the normal-priority work the lend existed to jump, and the
  only paths that re-grant a lend (a wake, or the consume point) are both unreachable from a
  starved-ready task — a demotion under that design starves a boosted audio client behind a
  CPU hog for the hog's whole slice.

  Correspondingly, **`park` releases the window unconditionally** — it does not ask whether the
  window has run out, because parking *is* the end of holding it. This is audio spec §9.4's "the
  promotion lasts until the promoted thread blocks again" and the pre-cutover scheduler's
  clear-at-deschedule, and it costs the audio path nothing: every wake that matters re-lends,
  through `WakeCause::boost` or at the consume point. `preempt` stays conditional, because there
  the task returns to a queue and is about to hold the priority again.

  Both halves are load-bearing, and the conditional park was shipped once (commit `9c2fc4d`) and
  is a hole: a lend blocked on *before* it ran out survives the block, `arm` re-arms it at the
  next dispatch, and a task that obtains one lend and thereafter runs less than a quantum before
  blocking holds inherited RT forever with nobody renewing anything — §8.5's hole reached by
  blocking instead of by spinning, off one pipe interaction and no syscall.

  With both halves, re-arming cannot compound, because both exits from `Running` end the lend. A
  boosted task is RT, so `preempt_if_due` only preempts it at its quantum end, and that quantum
  starts at the same dispatch the window was armed from — so `now >= until` always holds there and
  the window is cleared. A park clears it outright. A second arm therefore requires a *new* lend.
  **One lend buys at most one quantum of running time at the borrowed priority**, which is the
  pre-cutover scheduler's guarantee stated as a bound rather than left as a consequence of the
  deschedule path.

### 8.6 Event-source funnel table

| Source | Where the WaitQueue lives | Wake call site |
|---|---|---|
| Pipe readable/writable | `Pipe { readers, writers }` | `try_write`/`try_read` success paths |
| Futex | per-bucket `WaitQueue` in the futex table | `futex_wake` |
| Listener / IPC accept | `Listener { acceptors }` | connect |
| io_uring CQE | `Ring { cq }` | CQE post (incl. audio/net/hid completion delivery) |
| Device IRQ (audio, net, xHCI) | none — ISR pushes `(source, ts)` to `irq_ring`, sets `need_resched`; IRQ-exit resolves source → waitq → `wake_all` | IRQ exit |
| Timer/deadline | per-CPU heap (no queue) | pass start, local |
| waitpid / join / sleep | `waitq::wake_direct(&shared, cause, ..)` | zombify / timer |

IRQ timestamps are recorded **at IRQ time** in the ring entry and ride the wake into the CQE —
the audio DLL gets hardware-completion time, not drain time (B10).

## 9. Fairness: policy relocated, not changed

### 9.1 `FairShare` — per-process vruntime survives verbatim

The stored-lag semantics are preserved as pure math in `fair.rs`, so the simulator exercises the
exact production arithmetic:

```rust
// toyos-sched/src/fair.rs
pub struct FairShare<L> { state: L }   // L: LeafLock<ShareState>, supplied by the environment
// There is no `SpinSmall` type. The core does not implement a lock; the kernel passes its
// leaf lock and loom passes an instrumented one — same inversion as WaitQueue's `L` (§8.1).
enum ShareState {
    Runnable { vruntime: u64, runnable_threads: NonZeroU32, lag_at_wake: i64 },
    NonRunnable { lag: i64 },     // clamped ±MAX_VRUNTIME_LAG_NS, re-derived vs frontier on wake
}
pub const MAX_VRUNTIME_LAG_NS: u64 = 50_000_000;
pub const QUANTUM_NS: u64 = 10_000_000;
```

The global `sched_state: Lock<HashMap<Pid, ..>>` (one hot lock per charge) is deleted; each share
is a tiny lock reachable from the task. `enter/leave_runnable` are called from the owning CPU's
pass at the §5.2 transitions — the refcount cannot drift from the containers because both are
driven by the same linear-value moves (sim invariant I6). **Explicitly rejected:** Design 1's
per-thread EEVDF with weight division — a fairness *policy* change bundled into the machinery
cutover would confound the audio glitch A/B. Policy upgrades (true EEVDF virtual-deadline
ordering) are later, sim-gated, `queue.rs`/`fair.rs`-only changes.

`min_vruntime` frontier: global `AtomicU64` `fetch_max` at dispatch. Replacing it with a per-CPU
frontier whose epoch reconciliation rides the `Adopt` messages (§11) is gated on I5's fairness
bounds against the global-frontier reference across `fairness_storm` at 1–128 vcpus, and on
I13 — whose reach falls as the machine widens, so `SweepResult::thread_coverage_pct` is read
beside that verdict and never the verdict alone.

### 9.2 Ordering, RT band, quanta

`RunQueue` = `rt: VecDeque<ReadyTask>` (FIFO, drained first) + `fair: BTreeMap<(u64, u64),
ReadyTask>`, keyed by `(vruntime, insert_seq)` with a monotonic insertion sequence.

**The tie-break must not be `TaskKey`, and the sequence must be monotonic**: a re-inserted
thread has to land *behind* its equal-vruntime siblings, or the same thread can win every tie
and the others only run when it blocks. `queue.rs:18-24` carries this next to the field.
Related: `pop_surplus` takes the *last* fair task (`keys().next_back()`), so a steal never
hands away the next-to-run one.

**What that rule is worth today is smaller than the paragraph above reads, and it is now
measured rather than asserted.** The share's pot is charged for every nanosecond any of its
threads runs, so a thread re-inserted after a dispatch already carries a key *strictly* above
every sibling queued before it: the fair band serves a share's threads in insertion order
whatever the tie-break is. Exact ties survive only where no charge separates two inserts — a
`wake_all` of siblings, or the spawn burst — and one dispatch dissolves them. Ported literally,
`(vruntime, TaskKey)` is invisible to invariant I13 on `sibling_storm`
(`scenarios::fair_identity_tiebreak`, kept as a standing control for exactly that reason). The
rule stands because the *pot* is what is doing the work, and a policy change that stops
charging it once per dispatch — an ordered map of shares each holding a FIFO of its ready
threads, say — hands the whole job back to this field. That is what I13 exists to guard: its
gate `fair_identity_within_share` serves the leading share's lowest-keyed ready thread every
time and starves the rest, and invariant I5 reports a perfectly even split throughout.

`dispatch` sets `quantum_end = now + QUANTUM_NS`;
`finish()` arms `min(quantum_end, heap min)`. RT tasks round-robin within the band on the same
quantum. `SYS_SET_RT_PRIORITY` gains its privilege gate at the syscall layer (audio spec §9.4).

### 9.3 Accounting

`TaskAccounting` lives in `TaskInner`, updated at transitions: runqueue wait (`insert→dispatch`),
blocked-by-class ns (`WaitClass` at `park→wake`), cpu ns (`dispatch→preempt/park/die`).
`finalize()` returns it exactly once for the kernel to merge into `ProcessAccounting`. Sim
invariant I7 asserts conservation (Σ accounted == virtual elapsed per CPU).

### 9.4 Placement and balance (replaces cross-CPU stealing)

- **Wake placement**: `CpuSched::place` (`cpu.rs:437-450`) forwards in exactly one case — the
  woken task is RT **and** this CPU is already running RT — and only if `idle_sibling` finds a
  peer. Every other wake enqueues locally, unconditionally. There is no "would it preempt / is
  the CPU idle" test at placement time; that decision happens later, in `preempt_if_due`.

  > Previously written as "run locally if it would preempt or the CPU is idle; else forward
  > `Adopt` to an idle CPU from the sleep mask; else keep local". That reads as a load-balancing
  > property the scheduler does not have: a woken *normal* task never moves, whatever the peers
  > are doing.

  `idle_sibling` (`cpu.rs:424-431`) is a **linear scan of peer doorbells** — `(0..cpus.len())`
  looking for one whose `doorbell().sleeping()` is set. There is no published sleep mask;
  `grep sleep_mask` over `toyos-sched/` returns nothing. It is O(cpus) per RT wake, which is the
  form that matters for the 128-core claim: the cost is paid on the RT wake path itself.
- **Spawn**: spawner picks the least-loaded CPU from published `CpuHandle.load` (no `try_lock`
  probing of remote queues, which today misreads contention as nonexistence) and posts `Adopt`.
- **Pull**: an idle pass with an empty rq posts one `StealRequest` (§7.7) to the most-loaded CPU,
  then sleeps; the victim answers with `Adopt` at its next pass (its IRQ exit at latest — bounded
  by the quantum). Two-hop latency is the honest price of no shared queues; the sleep handshake
  wakes the thief the moment the task arrives. B4 cannot recur: a sleeping CPU with a nonempty
  rq violates I2, and every enqueue path to a sleeping CPU rings its doorbell.

## 10. The `Hw` trait and the simulator

### 10.1 The hardware surface

```rust
// toyos-sched/src/hw.rs — three traits stacked by what they must know about.
pub trait Kicker: Sync {
    fn kick(&self, target: CpuId);              // targeted x2APIC ICR; never broadcast
}

pub trait Machine: Kicker + 'static {           // the task-blind half
    type IrqGuard;
    // NO cpu_id() — CPU identity is CpuSched.id, a constructor parameter. An
    // ambient wrong-CPU query is unrepresentable in the core.
    fn now(&self) -> Nanos;                     // sampled ONCE per pass by the driver; core threads it
    fn set_timer(&self, deadline: Nanos);       // kernel: LAPIC one-shot, later TSC-deadline
    fn stop_timer(&self);
    fn irq_guard(&self) -> Self::IrqGuard;      // no user in either world — see below
    fn halt(&self);                             // kernel: sti;hlt (one atom); sim: mark sleeping
    fn need_resched(&self, cpu: CpuId);         // §7.6's retire-the-runner request
    fn trace(&self, ev: TraceEvent);            // shared vocabulary, kernel ring + sim recorder (§10.4)
    fn idle_wait(&self, token: SleepToken) {    // provided: consume the proof, perform the effect
        let _consumed = token;
        self.halt();
    }
}

pub trait Hw: Machine {                         // the two members that name a task
    type Payload: SchedPayload;
    unsafe fn switch(&self, token: RunToken<Self::Payload>);
    fn release(&self, key: TaskKey, payload: Self::Payload, acct: TaskAccounting);
}
```

**Why three traits and not one.** `switch` and `release` are the only members that need
`Payload`, so a driver implements `Kicker` and `Machine` — and meets real interrupts — without
naming a task at all. `idle_wait` splits for the same reason: its `SleepToken` is unforgeable
outside a `SchedPass`, so the token is the proof and `halt` is the effect, and a `Machine`
implementor can perform the effect without being able to construct the proof.

**`irq_guard` has no user in either world, and its RAII shape is wrong for the site this spec
named for it.** The kernel's pre-halt recheck must *set* IF on both exits — the halt exit because
`sti;hlt` is one instruction pair (STI shadow), the stay-awake exit because panic recovery enters
the idle loop with IF already 0 and restoring the caller's flags would strand that CPU. The core
never calls it either. It stays on the trait as a declared surface; the first real caller decides
its shape.

Real vs mocked (the kernel-equivalence contract): task types, state word, transitions, RunQueue,
FairShare math, mailbox, doorbell, sleep handshake, ticket protocol, retire chase, deadlines,
pass logic, invariant checks — **real and shared** in both worlds. Time/timer/IPI/hlt/cli/switch —
LAPIC/TSC/ICR/asm in the kernel; virtual clock, delayed events, vcpu bookkeeping in the sim. The
sim payload holds a real `Arc<MockAddressSpace>` (refcount invariant I8) and a `ctx_saved` shadow
flag: filing an outgoing task clears it, `SimHw::switch` sets it, and any migrate/finalize of a
task with `ctx_saved == false` is an invariant violation (I11) — the park-before-switch rsp
ordering is *fuzzed*, not just argued.

### 10.2 Execution model (deterministic by construction)

Virtual CPUs are not host threads. The VM holds a set of *enabled steps*; the explorer picks one
per iteration: `RunSlice(cpu)` (advance the running task's workload script — `Run(ns)`,
`Block(waitq, deadline)`, `Wake`, `Spawn`, `Exit`, `FutexOp`, `KernelSection(ns)`),
`DeliverIpi(cpu)`, `FireTimer(cpu)`, `DeliverIrq(script)`, `Pass(cpu)`. IRQ-off (guard held)
disables delivery steps for that vcpu. IPI/timer delays are explorer-chosen within bounds — that
is the schedule-fuzzing surface. Ticket phases are separate steps, so wake-vs-`prepare_wait`
races are reachable. `KernelSection(ns)` models preempt-off kernel work with a per-scenario
budget, closing the "sim cannot see kernel critical sections" blind spot: invariant I4's RT
latency bound includes the configured max section, and kernel `feature="check"` builds assert a
max pass duration as the on-target counterpart.

### 10.3 Exploration, shrinking, replay

- **ChoiceStream**: three interchangeable drivers — `SmallRng(seed)` (CI seed sweeps), **raw fuzz
  bytes** (`cargo fuzz` target: the bytes *are* the decisions, so libFuzzer mutation is free
  interleaving search), and **PCT** (random vcpu priorities + d change points — better bug-finding
  bounds than uniform random). Identical bytes ⇒ identical run, always.
- **Shrinker**: on violation, delta-debug the decision list and workload (drop events, shorten
  runs, drop CPUs) — determinism makes every candidate exact. Output: a minimized timeline plus an
  auto-emitted `#[test]` committed to `toyos-sched/sim/corpus/` as a permanent regression.
- **Harness self-validation gate (mandatory, before the sim guards any kernel change):**
  `scenarios/old_steal_port.rs` ports the OLD scheduler's steal-in-transit algorithm into the sim
  model. The `crash_md_exit_race` workload **must fail** on it (I1/I8 violation) and pass on the
  new protocol. A fuzzer that has never seen the crash class proves nothing; a green run is
  meaningful only after this gate is red-on-old.

### 10.4 Shared trace format and QEMU replay

`TraceEvent` (schedule, wake, block, park-commit, migrate, adopt, retire, idle-enter/exit, IRQ,
timer-fire) is defined once in `hw.rs`. **One vocabulary, two representations.**
`hw::TraceEvent` is the vocabulary: a closed Rust enum the sim holds by value and asserts
on, with no layout guarantee, so it cannot be what goes on the wire. `kernel/src/trace.rs`'s
`Record` is the wire form: `repr(C)`, 24 bytes, hand-fixed discriminants, because its readers are
`memory read` in LLDB and `replay --from-qemu`, neither of which can parse a Rust enum.
`trace::record` is the total (wildcard-free) mapping from the first onto the second and is what
`Machine::trace` installs. The ring keeps kinds the core cannot produce — `IrqDrain` (the B10
instrument), `TimerArm`/`TimerStop`, `Preempt`, `TimerFireBurst` — because they are kernel
observations from *below* the boundary, and collapsing the ring into the core's vocabulary would
delete working instruments to buy a symmetry nothing needs. The kernel `Machine::trace` writes to
a per-CPU binary ring (drained like the log ring, off the timer path); the sim records the same
stream.
`toyos-sched-sim replay --from-qemu <trace.bin>` converts a captured kernel trace into a sim event
script — a real-world anomaly becomes a host-side repro. This is the pipeline crash.md's destroyed
evidence needed: post-cutover, one QEMU capture replays deterministically under the invariant
checkers.

### 10.5 Invariants (checked after every sim step; cheap subset at kernel pass ends)

| ID | Invariant | World |
|---|---|---|
| I1 | Single ownership: every live `TaskKey` in exactly one container/message system-wide; state word agrees | sim (global walk) + kernel check builds (local) |
| I2 | Sleeping CPU ⇒ empty rq ∧ empty mailbox, or an IPI pending to it | sim + loom_sleep |
| I3 | Invariant T: timer armed ≤ min(quantum, earliest parked deadline); hlt with deadlines ⇒ armed. Its teeth were never the problem — no *scenario* could construct a divergence, because `SchedPass::begin` runs the drain and the deadline fire with no step boundary between them. `sim/tests/deadline_claim_race.rs` is that window, scripted | sim + kernel check builds (+ `deadline_claim_race`) |
| I4 | RT-ready task while any CPU runs a normal task beyond IPI+pass+max-KernelSection bound ⇒ fail | sim |
| I5 | Fairness: per-share service within lag bounds; \|lag\| ≤ 50 ms at transitions | sim |
| I6 | `FairShare.runnable_threads` == actual Ready+Running count per share | sim |
| I7 | Accounting conservation: Σ per-task ns == virtual elapsed per CPU | sim |
| I8 | Mock-AddressSpace Arc strong count == live tasks referencing it (the crash.md detector) | sim |
| I9 | A lend of inherited RT is spent only by *running*: one lend buys ≤ one quantum of running time at the borrowed priority, and a queued task never loses one it has not spent. Checked as cumulative Running residency per lend — **not** by comparing a running task's `until` to the clock, which `arm` makes vacuous | sim (+ negative gate `old_park_kept_the_lend`) |
| I10 | Scenario end: all tasks finalized exactly once, mailboxes empty, CPUs idle, no leaks | sim |
| I11 | No migrate/finalize of a task whose `ctx_saved` shadow is false | sim |
| I12 | ≤1 `Wake` and ≤1 `Retire` node in flight per task; steal node free ⇔ unlinked | sim + loom |
| I13 | Fairness, inside a share: threads of one share get equal service over I5's window, narrowed to intervals where every CPU carries the same number of each member's runnable threads | sim (+ negative gate `fair_identity_within_share`, control `fair_identity_tiebreak`) |
| I14 | Retire promptness: a killed task is never migrated, and a retire reaches `Hw::release` within `QUANTUM + IPI_LATENCY + max KernelSection + 2×RUN_CHUNK`. The first half is what keeps the second reachable — §7.6 | sim (+ negative gate `old_migrate_kept_the_corpse`) |

### 10.6 Loom scope (honest division of labor)

Loom owns the primitives the sim's step granularity assumes correct: mailbox push/drain (IRQ torn
push; the forbidden preempted-producer strand), doorbell edge/IPI accounting, ticket CAS protocol
(wake/commit/cancel/timeout), kill-bit vs wake ordering, retire-node re-post, sleep handshake.
The simulator proves that the protocol above linearizable primitives keeps I1–I14 across
schedules. Neither overpromises: loom does not scale to the whole scheduler; the sim does not
model weak memory.

## 11. Migration state

The kernel runs on `toyos-sched` exclusively, through stage 7c: every piece of legacy machinery
this design replaces is deleted, `kernel/src/scheduler.rs` survives as the kernel-facing API and
`kernel/src/sched/` is the driver half. `SYS_SET_RT_PRIORITY` is retired at number 96, its
authority now `SYS_RT_ENTER` gated on `Rights::RT`. The host gate is `cargo test --manifest-path
toyos-sched/Cargo.toml` — the workspace's default members are the core, the simulator and the loom
package — run in CI by `.github/workflows/host-tests.yml`; it carries the four loom suites, the
corpus replays, budgeted seed and fuzz-byte sweeps, and nine negative gates against two controls.
Stage 4's full exit criterion, `toyos-sched-sim gate 10000` and `fuzz-sweep 10000000`, runs from
the CLI and no workflow invokes it. The scale stage (9) is open: the vruntime frontier is one
global `AtomicU64` advanced by `fetch_max` rather than per-CPU with epoch reconciliation (§9.1),
`Machine::set_timer` arms the LAPIC one-shot rather than `IA32_TSC_DEADLINE`, and
`scenarios::all()` carries `fairness_storm` only at widths 1 and 2 — wider runs are
`fairness_storm:<cpus>` from the CLI. Stage-by-stage history is `git log`.

## 12. Failure-mode table

| Failure / race | Behavior | Guarantee |
|---|---|---|
| Wake races `prepare_wait`→`block_on` commit | Waker CASes `Committing→WakeQueued`; commit refuses to park, returns `AlreadyWoken` | No lost wake; no switch performed (CT shape + loom_ticket) |
| Wake races local timeout fire | Both attempt the same claim CAS; loser no-ops; `wake_one` loser retries the **next** waiter | No lost wake, no double wake, no stranded second waiter (§8.2) |
| Wake races retire | Both are messages to one consumer, handled in order; losing `Wake` finds no parked task | Benign no-op; keys never reuse (loom_retire) |
| Producer preempted mid-mailbox-push | Cannot occur: push runs preempt-disabled (RT assert + loom model of the violation) | No stranded suffix (§7.2) |
| IRQ tears a same-CPU push | Consumer sees end-of-queue; doorbell guarantees a follow-up pass | Message delayed ≤ one pass, never lost (loom_mailbox) |
| Wake in flight while target enters hlt | SLEEPING set before final drain; producer sees SLEEPING ⇒ IPI; STI-shadow pending IPI ends hlt | No sleep-through (I2, loom_sleep) |
| Normal wake to a busy CPU | KICK set, no IPI; drained at next safe point | ≤ one quantum, matching today; zero IPI cost (§7.3) |
| RT wake to a busy CPU | Unconditional targeted IPI → pass at IRQ exit | IPI + pass latency, bounded incl. KernelSection budget (I4) |
| Task retired while in transit | Kill bit sticky; adopter converts to `DeadTask` on arrival; retire chase re-posts ≤ hops | Terminates; no scans, no timeout loops (§7.6) |
| Two concurrent retires of one task | Second `RETIRE_QUEUED` CAS fails → panic | Fail fast: single-retirer is a kernel invariant |
| Steal probe outstanding, thief idles again | `in_flight` swap fails → no second post; thief sleeps with doorbell armed | Node never double-linked (I12) |
| Ready task on a CPU that halts | `SleepToken` unconstructible with nonempty rq | Unrepresentable (§7.5) |
| Deadline exists but timer unarmed | `finish()` programs timer last; `SleepToken` requires the plan applied | Invariant T holds by construction (I3) |
| Blocked task's deadline on a migrated task | Cannot occur: only ready tasks migrate; deadline lives in `ParkedEntry` only | Unrepresentable (§6.1) |
| Boosted client spins without blocking | Window armed at dispatch, expiring at the quantum-end preempt | ≤ one quantum of running time per lend (I9) |
| Boosted client starved before it ever runs | Queue time spends no window; the pick never demotes | The lend survives to its first dispatch (I9, §8.5) |
| Boosted client blocks early, over and over, on one lend | `park` releases the window unconditionally | The lend cannot outlive one block (I9, negative gate `old_park_kept_the_lend`) |
| Panic inside a pass | Percpu busy flag observed on re-entry → CPU halts loudly; panic-reentry flag checked before any percpu/log access. (Recovery leaves via `scheduler::schedule_no_return()`; there is no `abandon_current` — see §7.6) | One clean report; evidence preserved (B9) |
| Task value dropped outside `finalize` | `TaskInner::drop` panics | Double-drop class converted to loud failure (§5.1) |
| Simulator finds a violation | Seed + full decision trace + shrunk replay auto-committed to corpus | Permanent regression; deterministic repro |

No failure mode silently drops a wake, silently degrades, or leaves a task representable in two
places. The worst accepted latency is one quantum (normal wake to a busy CPU) — identical to
today's contract.

## 13. Explicitly rejected

1. **Shared runqueues + cross-CPU locking** (status quo): direct cause of B1/B2; O(cpus) lock
   traffic per wake; cannot meet 128 cores.
2. **Lock guards leaked across switches**: correctness by narration. Pass-ends-before-switch
   leaves nothing to leak.
3. **Post-switch parking** (`outgoing` + `handle_outgoing`): the factory for B3 and the rsp-save
   hazard. Park-before-switch is safe only under per-CPU ownership — why ownership comes first.
4. **Global blocked pool keyed by `EventSource`**: one hot lock, per-source ad-hoc patches.
5. **Idle-initiated direct stealing**: mutating a sibling's queue is the crash.md transit window.
   Push placement + `StealRequest` messages; two-hop latency accepted and bounded.
6. **N×N SPSC mailbox matrix**: O(N²) memory, per-ring overflow cliffs — B8 reintroduced by
   sizing. Intrusive MPSC has no capacity to size wrong.
7. **Bounded rings with spin/panic overflow policy** (Design 2's R3): a userland-provokable wake
   storm reaching a 100 ms kernel panic violates "kernel must never crash from userland";
   embedded nodes make the whole question unaskable.
8. **Wake-message-as-optimization with scan fallback** (Design 1's scan_hint): a second delivery
   path that must stay behavior-identical to the first is a standing divergence risk, and its
   ownership-carrying `Deliver` case had no fallback at all. One path, no fallback needed.
9. **Per-thread EEVDF / weight-division fairness at cutover** (Design 1): policy change bundled
   into machinery replacement; discards the just-debugged stored-lag semantics. Deferred to a
   sim-gated policy stage.
10. **Dual-scheduler feature-flag coexistence**: two schedulers plus an `EventSource` compat
    registry buy bisectability the per-source conversions and the sub-staged cutover already
    provide with less standing surface — each conversion is individually revertable and each
    sub-stage individually bootable. Zero-legacy principle: we do not carry a parallel old world
    one stage longer than the conversion requires.
11. **Enum state field on one task struct**: every invalid transition compiles. Five types give
    each transition a signature.
12. **`Copy`/bitwise-movable task records**: address instability made raw pointers across
    container moves unsound. `Box` + linear moves fix it.
13. **Host-thread-per-vCPU simulator**: nondeterministic, unreplayable, heavy at 128 vcpus.
    Step machine or nothing.
14. **loom for the whole scheduler**: state-space explosion. Loom for primitives, sim for the
    protocol — stated honestly.
15. **async/await coroutine kernel tasks**: a whole-kernel rewrite, out of scope.
16. **Immediate wholesale rewrite without the simulator**: this subsystem's failures destroy
    their own evidence; the sim exists so the cutover lands against 10⁴ pre-failed schedules,
    not one QEMU boot.

## 14. Open risks

- **Two-hop steal latency** under sudden imbalance: mitigated by wake-time push placement;
  measured by sim latency histograms before any tuning.
- **`SpinSmall` per `FairShare`**: a process with hundreds of threads bounces one line —
  acceptable now; shardable later behind the same API.
- **Kernel preempt-off sections** bound RT wake latency exactly as today; `KernelSection` budgets
  make the bound visible in sim, and kernel check builds assert max pass duration — but the
  budget numbers themselves need empirical calibration on TCG vs KVM vs hardware.
- **Glitch-gate attribution**: gate A can charge a remaining gap to the scheduler only while the
  audio userland is healthy enough that nothing else explains it. When it is not, a scheduler
  change is judged on the sim's `audio_pipeline` scenario plus non-regression instead.
- **PCID/INVPCID and TSC-deadline untestable under TCG** (known issue): the `Hw` seams keep both
  CPUID-gated with fallbacks; needs KVM/bare-metal validation.
