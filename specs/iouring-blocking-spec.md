# ToyOS io_uring-Only Blocking — Technical Specification

This is the design for the replacement wake/blocking core, realizing the CLAUDE.md idea
"io_uring as the only blocking I/O mechanism". Priority ordering throughout: (1) make bug
classes unrepresentable at compile time, (2) runtime fail-fast, (3) tests. The concurrent
stabilization of the current scheduler (the task-lifecycle crash) is assumed to land first;
nothing here depends on its internals beyond the Stage-0 gate (§19).

## 1. Goals

- Exactly **one** notification primitive in the kernel (`completion::post`) — wait-free,
  callable from any context, under any lock, including ISRs
- Exactly **two** things a thread can park on (`Ring`, `Futex`) — fd-based parking is
  unrepresentable in the type system
- Exactly **one** park/recheck protocol with one lost-wake proof, source-agnostic
- Absolute-deadline time semantics end-to-end — no `0 = forever`, no `0 = nonblock`,
  no relative-timeout drift, no sentinel branch anywhere in the kernel
- Per-CQE IRQ-fidelity timestamps — the audio DLL and any future latency consumer read
  hardware time, not drain time
- Blocking syscalls keep their ABI shape (`read` blocks); internally they are
  try-once + park-on-kernel-ring — the entire userland (std fork, cpal, toyos-cc C
  bootstrap) is untouched until the polish stage
- Rings cost 32KB, not 2MB; ring memory is RAII; io_uring stops abusing `shared_memory`
- Structural fix for the open `waitpid`/`thread_join` forever-hang and the directed-wake
  (`wake_task`) race class
- Scales to 128+ cores: no lock in `post()`, a committed sharding plan for the registry

## 2. Bug classes this design deletes

Verified against the current tree:

| # | Class | Evidence | Closed by |
|---|---|---|---|
| B1 | Dual notification paths: every wake site must call both a wait-queue wake and `io_uring::complete_pending_for_event`; forgetting one half is a silent lost wake | **11 sites across 6 files — see the inventory below** | One `post()` function (§4); the dual-call idiom no longer exists to forget (CT2) |
| B2 | Per-source park/recheck windows: five closures in five styles, with PipeWritable, Listener, Keyboard, Mouse and Network having no recheck at all | **Evidence superseded.** `handle_outgoing`, `park_outgoing` and `PERCPU_EVENTS` were all deleted at scheduler migration 7a/7c — zero hits in `kernel/` — so `scheduler.rs:1704-1712` no longer points at anything. The *shape* survives as per-object `KWaitQueue` parks in `sched::waitqs`; re-derive the site list before sizing work off this row | One exhaustive 2-variant recheck match + one proof (§7); a new wait target without a recheck arm does not compile (CT3) |
| B3 | Open forever-hang: `sys_waitpid` blocks with `block(None, 0)` and is woken by directed `wake_task`; child exiting between the zombie check and pool insertion loses the wake forever. `SYS_THREAD_JOIN` shares it | `arch/syscall.rs:761,998` | `Source::ChildExit`/`ThreadExit` + Invariant W (§7, §8); `wake_task` is deleted entirely (§6.3) |
| B4 | Timeout sentinels: `io_uring_enter(timeout=0)` = nonblock forces soundd's `delta==0 → full-period oversleep` hack; kernel `deadline: u64` with `0 = forever` is the opposite sentinel in the same codebase | `io_uring.rs:301-303`, `soundd/main.rs:363-366` | Continuous absolute deadlines over the whole `u64` range — no sentinel branch exists to collide (§9, CT4) |
| B5 | ID-vs-pointer lifetime coupling: a queued `TaskCtx` referencing a freed kernel object (the motivating use-after-free) | crash dossier | Parked state names objects by Copy IDs only (`RingId`, futex `DirectMap`); destroyed objects fail lookups, never dangle (CT6) |
| B6 | Directed-wake race: `wake_task` scans a pool the target may not have entered yet (kill/retire paths share B3's shape) | `scheduler.rs` kill/retire sites | `kill_pending` flag + universal park recheck under the same POOL-lock fence (§6.3) |

### B1 inventory — the 11 dual-call sites

Stage 1 is sized off this list, so it is kept exact. Re-derive it with
`grep -rn complete_pending_for_event kernel/` (two further hits are comments in
`io_uring.rs`) rather than trusting the count below to have aged well.

| # | Site | First half — the wake | Second half |
|---|---|---|---|
| 1 | `pipe.rs:292` | `scheduler::wake_pipe_writers` | `complete_pending_for_event` |
| 2 | `pipe.rs:320` | `scheduler::wake_pipe_readers` | `complete_pending_for_event` |
| 3 | `process.rs:1176` | `scheduler::wake_pipe_readers` | `complete_pending_for_event` |
| 4 | `process.rs:1188` | `scheduler::wake_pipe_writers` | `complete_pending_for_event` |
| 5 | `drivers/xhci/hid.rs:45` | `keyboard::wake_waiters` | `complete_pending_for_event` |
| 6 | `drivers/xhci/hid.rs:59` | `mouse::wake_waiters` | `complete_pending_for_event` |
| 7 | `sched/driver.rs:524` | `net::wake_waiters` | `complete_pending_for_event` |
| 8 | `sched/driver.rs:534` | `audio::wake_waiters` | `complete_pending_for_event` |
| 9 | `drivers/i8042/mod.rs:270` | `keyboard::wake_waiters` | `complete_pending_for_event` |
| 10 | `drivers/i8042/mod.rs:281` | `mouse::wake_waiters` | `complete_pending_for_event` |
| 11 | `arch/syscall.rs:1041` | `sched::waitqs::wake_all(&queue)` | `complete_pending_for_event` |

Every first half bottoms out in `sched::waitqs::wake_all` — at 7 of the 11 sites
that is what the call plainly is (6 behind `<subsystem>::wake_waiters`, one
direct); the other 4 reach it behind `scheduler::wake_pipe_*`. There is no
`scheduler::wake_*` family doing this work, and the drains are in
`sched::driver::drain_irqs`, not `scheduler::drain_events`.

**This inventory previously read "~9 sites" and omitted the i8042 keyboard and
mouse pair entirely (rows 9 and 10).** That pair is the metal-track PS/2 path —
new machinery, added after the inventory was written, and exactly the class of
site B1 exists to say cannot be forgotten. The inventory failed at its one job,
which is the argument for B1's fix rather than against it: a list that must be
maintained by hand will be wrong the first time someone adds a wake site, and
being wrong is silent.

## 3. Architecture

There is exactly one notification primitive: `completion::post(Source)` — wait-free,
callable from any context. Posts drain via `completion::flush()` at the existing
bounded-latency scheduler entry points. Flush fans a `Source` out to the rings watching it —
**user rings** (32KB arena-slot io_uring instances) and **kernel rings** (page-less
per-thread mini-rings that implement blocking syscalls) — posts a timestamped CQE into
each, and calls the single wake primitive `wake_channel(WaitChannel::Ring(id))`. Because a
CQE is *level-readable state* that persists until consumed, the park-time recheck collapses
to one predicate ("does my ring hold an unconsumed completion?") in one place, with one
proof. Blocking syscalls keep their ABI shape but are internally
`try-once → arm → park → drain → retry`.

```
wake site (pipe write, ISR, exit path, connect, HID)
        │  completion::post(Source)          ← the single function, IRQ-safe, wait-free,
        │                                      captures (timestamp, rt) at post time
        ▼
per-CPU Post queue  ──(need_resched ⇒ bounded latency)──►  completion::flush()
        │ registry lock: Source → armed watches → timestamped CQE per ring
        ▼
scheduler::wake_channel(WaitChannel::Ring(id))             ← the single wake primitive
        │ blocked-pool take_by_channel → WokenBatch → enqueue_batch (+kick IPI, RT rules)
        ▼
parked thread resumes: enter() loop / kernel blocking loop drains CQ, retries
```

## 4. The single notification primitive

### 4.1 Sources

```rust
// kernel/src/completion.rs

/// Something that can become ready. Data, never a wait target: the scheduler
/// never sees this type — scheduler.rs has no dependency on pipe/listener/
/// audio/process; only the completion core maps sources to rings.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Source {
    PipeReadable(PipeId),
    PipeWritable(PipeId),
    Listener(ListenerId),
    Keyboard,
    Mouse,
    Network,
    Audio,
    /// Posted by every process exit, naming its parent. waitpid arms
    /// ChildExit(self). Closes B3.
    ChildExit(Pid),
    /// Posted by thread exit, naming the exiting thread. thread_join arms it.
    ThreadExit(TaskId),
}

/// One post = "source may be ready". Level-checked at arm time, so duplicate or
/// spurious posts are harmless; a missing post is the only bug, and exactly one
/// function can emit them.
#[derive(Clone, Copy)]
struct Post {
    source: Source,
    /// nanos_since_boot captured inside post() — IRQ-entry fidelity for device
    /// posts. Stamped into every CQE this post produces (§10).
    ts: u64,
    /// Poster was an RT thread. false for ISR posts: a device interrupt must
    /// not inherit the RT status of whatever thread it happened to interrupt.
    rt: bool,
}
```

### 4.2 post()

```rust
/// THE notification primitive. Wait-free: pushes into the calling CPU's SPSC
/// Post queue and sets need_resched, which guarantees a flush() before the
/// next return to user, the next hlt, or the next context switch. Callable
/// from ISRs, from syscall context, and while holding ANY lock — it takes none.
pub fn post(source: Source) {
    let ts = clock::nanos_since_boot();
    // percpu::in_irq(): interrupt-nesting depth maintained by the arch/idt
    // entry/exit glue. percpu::current_is_rt(): mirror written at context
    // switch. Both lock-free percpu reads.
    let rt = !percpu::in_irq() && percpu::current_is_rt();
    PENDING[percpu::cpu_id()].push(Post { source, ts, rt });
    crate::preempt::set_need_resched();
}
```

There is no `post_inline()`, no `post_irq()`, and no second path. RT attribution and
timestamping are derived from percpu state that is correct in every context, so there is no
wrong-context misuse to guard against — which is why no `IrqCtx`-style capability token
exists here (§20). Deferral cost: flush runs in the same syscall's exit path or the IRQ
epilogue — microseconds against a 2.9 ms audio period (§15). In exchange `post()` has zero
lock-ordering obligations, which is what makes "call it from anywhere" true and reviewable.

**Queue overflow.** The Post queue is 256 deep per CPU. Overflow sets a flag that makes the
next `flush()` run a *sweep*: for every source with at least one armed watch, evaluate
`Source::is_ready()` (§5.4) and synthesize the post (sweep-synthesized posts carry
`ts = now`, `rt = false`). Loud log, never a silent drop. Rejected alternatives: panic
(an IRQ burst can legitimately exceed 256), silent drop (lost wakes), per-source dedup bits
(needs lock-free registry access from ISRs; complexity fails the >2x rule).

### 4.3 flush()

```rust
/// Drain this CPU's Post queue and fan out. Called ONLY from: do_schedule
/// entry, cpu_idle_loop iteration, kernel_exit_to_user_check (need_resched).
/// Runtime assert: not reentrant per CPU.
pub fn flush() {
    let mut to_wake: SmallVec<[(RingId, bool /*rt*/); 8]> = SmallVec::new();
    {
        let mut core = CORE.lock_unwrap();
        if take_overflow_flag() { core.sweep_ready_sources(); }
        while let Some(post) = PENDING[cpu].pop() {
            for ring_id in core.watches_for(post.source) {     // consumes one-shot watches
                core.post_completion(ring_id, post);           // CQE (with ts) or fired flag
                to_wake.push((ring_id, post.rt));
            }
        }
    } // CORE released BEFORE waking — lock order §13
    for (ring_id, rt) in to_wake {
        scheduler::wake_channel(WaitChannel::Ring(ring_id),
                                rt.then_some(Boost::RtInherited));
    }
}
```

`post_completion` for `Sink::User` writes the CQE into the arena-slot CQ exactly as today's
`post_cqe` (atomics on the shared header, overflow assert with structural 2× sizing), now
including `post.ts`. For `Sink::Kernel` it sets the fired flag and records `post.ts`.

The old per-CPU event queue's `EventSource::IoUring` self-post round-trip is already gone —
scheduler migration 7a deleted `PERCPU_EVENTS`, and the never-constructed `IoUring` poll key
died with `EventSource` at 7c; ring wakes go straight to `wake_channel`.

## 5. Rings

### 5.1 Core types

```rust
pub struct RingId(u32);            // Copy ID; all references by ID, never pointer (B5)

struct Ring {
    owner: Pid,
    sink: Sink,
}

struct Watch { source: Source, user_data: u64, flags: PollFlags }

enum Sink {
    /// Full io_uring: SQ/CQ/SQEs in a 32KB slot of the process's RingArena
    /// (§5.2). ArenaSlot is a non-Copy RAII handle — Drop returns the slot.
    User { slot: ArenaSlot, sq_size: u32, cq_size: u32, armed: SmallVec<[Watch; 4]> },
    /// Page-less ring for in-kernel blocking (§8). At most one watch, by type:
    /// arm() REPLACES the previous watch (removing its registry fan-out entry
    /// and clearing a stale fired flag) instead of asserting a length.
    Kernel { watch: Option<Watch>, fired: bool, fired_ts: u64 },
}

struct CompletionCore {
    rings: IdMap<RingId, Ring>,
    /// Forward index: which rings watch a source. Kept consistent with the
    /// per-ring armed state under the single CORE lock — no cross-lock
    /// invariant to violate. Sharding plan: §13.2.
    watches: HashMap<Source, SmallVec<[RingId; 2]>>,
    overflow_sweep_needed: bool,
}
static CORE: Lock<Option<CompletionCore>> = Lock::new(None);
```

The five per-source watcher `Vec<RingId>` stores + add/remove/get triplets
(`audio.rs:148-159` and clones in keyboard/mouse/net/pipe/listener) are **deleted** — the
registry is the only watcher store.

The `Kernel` sink's `Option<Watch>` closes the stale-armed-watch interleaving: park on
source A → woken by deadline or kill (watch A still armed, unfired) → later park on
source B. `arm()` replaces A (deregistering its fan-out entry, clearing any stale
`fired` from a post-take_fired fire of A) as a type-level replace, not a latent
`len() <= 1` assert panic.

### 5.2 Ring arena — 32KB per ring

`io_uring` stops abusing `shared_memory` (removes the known-issues entry and the last
caller of `shared_memory::destroy()`). One `RingArena` per process, created on first
user-ring creation, stored in `ProcessData`:

```rust
// kernel/src/completion.rs
pub const RING_SLOT_SIZE: usize = 32 * 1024;

struct RingArena {
    page: PageAlloc,          // owned 2MB page, RAII — freed when ProcessData drops
    user_base: UserAddr,      // mapped once into the owner's tables
    slots: u64,               // bitmap of 64 × 32KB slots
}

/// Non-Copy RAII slot handle held inside Sink::User. Drop clears the bitmap
/// bit. Double-free / leak of a slot is a Drop, not call-site discipline.
struct ArenaSlot { index: u8 }
```

Slot layout (offsets within a 32KB slot; headers cache-line separated):

```
0x0000  IoUringParams
0x0040  SQ header
0x0080  CQ header
0x0100  SQE array   (depth ≤ 256 × 32 B = 8 KB)
0x2100  CQE array   (2 × depth × 24 B ≤ 12 KB)   ← structural 2× overflow headroom
```

64 rings per 2MB page: a per-thread SDK ring costs 32KB; a daemon's Poller ring costs 32KB.
The RingArena page is freed by `ProcessData` drop, ordered after all threads have retired,
so no parked waiter can outlive its ring memory.

**Kernel rings** cost ~100 bytes (no pages, no arena slot), are created lazily on a
thread's first blocking miss, live in per-thread `ThreadData` (never in `TaskCtx` — the
scheduler owns no completion objects, per B5), and are destroyed in thread teardown *after*
`retire_task` proves the thread is out of every scheduler container.

### 5.3 Arming — the `Armed` typestate

```rust
/// Proof that a one-shot watch is registered on `ring`. Non-Copy, #[must_use].
/// Consumed by completion::wait(); Drop without wait() disarms the watch.
/// Closes the "park with no armed watch" and "forgot take_fired ⇒ busy
/// self-wake loop" holes as types, not discipline.
#[must_use]
pub struct Armed { ring: RingId }

/// Arm a one-shot watch. Closes the arm-time TOCTOU exactly like today's
/// process_poll_add: check is_ready before inserting; insert; re-check under
/// CORE; if ready, consume the watch and fire immediately (CQE / fired flag,
/// ts = now). For Kernel sinks this REPLACES any previous watch (§5.1).
pub fn arm(ring: RingId, watch: Watch) -> Armed;

/// Park until the ring holds an unconsumed completion, the deadline passes, or
/// a spurious/kill wake occurs. Internally: scheduler::park(Ring(ring), d);
/// then take_fired(). Callers loop and re-derive — spurious returns are legal.
pub fn wait(armed: Armed, deadline: Deadline);

/// Sugar used by every blocking syscall (§8): current thread's kernel ring,
/// arm(source) with replace semantics, wait. One call — the three-step
/// protocol cannot be mis-sequenced because it is not exposed as steps.
pub fn wait_current(source: Source, deadline: Deadline);
```

### 5.4 The one readiness match

```rust
impl Source {
    /// The ONLY place readiness predicates live. Used at arm time (immediate
    /// fire if already ready) and by the overflow sweep. Absorbs today's single
    /// readiness match, io_uring's Source::is_ready — the scheduler-side
    /// event_ready() duplicate is already gone (sched migration 7a/7c).
    fn is_ready(self) -> bool {
        match self {
            Source::PipeReadable(id) => pipe::has_data(id),
            Source::PipeWritable(id) => pipe::has_space(id),
            Source::Listener(id)     => listener::has_pending_by_id(id),
            Source::Keyboard         => keyboard::has_data(),
            Source::Mouse            => mouse::has_data(),
            Source::Network          => net::has_packet(),
            Source::Audio            => audio::has_pending(),
            Source::ChildExit(pid)   => process::has_zombie_child(pid),
            Source::ThreadExit(id)   => process::thread_is_zombie(id),
        }
    }
}
```

## 6. Scheduler contract

The scheduler shrinks to two wait channels plus a pure timer. This is the boundary
contract with the ownership-typed scheduler core (Phase 1 track).

### 6.1 Surface

```rust
// kernel/src/scheduler.rs (end state)

/// The ONLY things a thread can park on. fd-based waiting is unrepresentable:
/// no variant for it, no function accepts an fd or a Source.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaitChannel {
    Ring(RingId),
    Futex(DirectMap),
}

/// Park the current thread. Spurious wakes allowed; callers loop.
/// Callers (grep-gated, §18): completion::wait, futex_wait. Nothing else.
pub(crate) fn park(chan: WaitChannel, deadline: Deadline);

/// Pure sleep. A concrete Instant is required BY TYPE — "sleep forever on
/// nothing" (B3's shape) cannot be written: Instant has no Forever value.
pub fn park_timeout(until: Instant);

/// The single wake primitive. Wakes ALL waiters of the channel into the run
/// queues (WokenBatch → enqueue_batch, kick IPIs, RT-preempt rules).
/// Guarantee G1: if a completion publish (CQE stored / fired set / futex gen
/// bumped) happens-before this call, every thread that has parked or will
/// park on `chan` either is woken by this call or observes the completion in
/// its park-time recheck (§7).
pub fn wake_channel(chan: WaitChannel, boost: Option<Boost>) -> usize;

/// Directed unpark for kill/retire. Sets the target's kill_pending flag, then
/// (under POOL) removes it from the blocked pool if present and enqueues it.
/// Race-free by the same fence as Invariant W (§7). Replaces wake_task, which
/// is DELETED.
pub fn unpark_for_kill(task: TaskId);
```

### 6.2 What dies

Already dead: `EventSource`, `scheduler::block`, `wake_by_event`, `event_ready()`,
`take_by_event_*` and the blocked pool's `by_event` index all fell to scheduler migration
7a/7c — today threads park on per-object `KWaitQueue`s (`kernel/src/sched/waitqs.rs`) with
`io_uring::Source` as the poll key. Still to die here: `io_uring::Source` +
`complete_pending_for_event` (readiness collapses into `WaitChannel::Ring`),
`wake_pipe_readers/writers`, `wake_task`, and the per-source fd queues in `waitqs.rs`
(`by_channel: BTreeMap<WaitChannel, Vec<TaskId>>` replaces them). `SYS_AUDIO_POLL = 84`
(dead ABI) deleted.

Deadlines: `Deadline::FOREVER` (`u64::MAX`) inserts no entry in the deadline structure;
finite deadlines insert `(t, id)`. A parked deadline is already covered by its home CPU's
LAPIC one-shot — it lives in the `ParkedEntry` that `TimerPlan` → `Hw::set_timer` arms from,
and in nothing else (scheduler-core §8.3) — so the block-handoff timer hole stays closed.

### 6.3 Kill/retire protocol (deletes the directed-wake class)

Per-thread `kill_pending: AtomicBool` (in `ThreadData`).

- Killer: store `kill_pending = true` (Release) → take POOL → `take_by_id(target)` → if
  present, enqueue; if absent, done — the target either is running (will observe the flag
  at its next syscall boundary) or is about to park (will observe it in the recheck).
- Parker: after pool insertion, the universal recheck (§7) includes `kill_pending`.

No wake can be lost: the POOL lock is the fence, identical in structure to Invariant W.
The woken thread returns from `park` spuriously; the syscall loop's standard exit check
handles termination. `retire_task` then proves the thread is out of every scheduler
container before its kernel ring (and any user rings via process teardown) is destroyed.

### 6.4 Futex stays native

Futexes are memory-word-keyed and value-guarded, not readiness-guarded; their race closure
is compare-against-`expected` + the wake-generation protocol — there is no level-readable
completion to check. Forcing them through rings would add registry traffic to the hottest
sync path to gain nothing. Two channels, two rechecks, both structural. The `futex_wait`
timeout becomes an **absolute** deadline (§9.3) but the protocol is unchanged.

## 7. The park/recheck protocol — one proof instead of five

`pass_block`'s (`kernel/src/sched/driver.rs`) park arm becomes:

```rust
SwitchReason::Park { chan, deadline } => {
    park_outgoing(queue, old, chan, deadline);            // pool insert under POOL lock
    let raced = match chan {                              // exhaustive: 2 variants
        WaitChannel::Ring(r)  => completion::has_unconsumed(r),   // cq_count>0 | fired
        WaitChannel::Futex(_) => FUTEX_WAKE_GEN.load(Relaxed) != snapshot_gen,
    } || thread_kill_pending(old_id);                     // universal, channel-agnostic
    if raced { wake_channel(chan, None); }
}
SwitchReason::ParkTimeout { until } => {
    park_outgoing_deadline_only(queue, old, until);
    if thread_kill_pending(old_id) { /* self-wake */ }
}
```

**Invariant W (lost-wake closure, ring case).** Waker executes C1 `publish CQE` (under
CORE), then C2 `wake_channel` (pool scan under POOL). Parker executes P1 `pool.insert(self)`
under POOL, then P2 `has_unconsumed(r)`.

- If C2's scan sees the parker (P1 ≺ C2): woken directly.
- Else C2 ≺ P1. Then C1 ≺ C2-release(POOL) ≺ P1-acquire(POOL) ≺ P2, so P2 observes the
  CQE and self-wakes. The POOL lock is the fence; no bespoke atomics.

The proof is **source-agnostic**: it holds for pipes, listeners, HID, audio, child-exit —
anything — because they are all "a CQE in a ring". A new source cannot re-open the window:
a new wait target requires a new `WaitChannel` variant, and the exhaustive match forces its
recheck or the kernel does not compile. The kill recheck shares the identical fence
(§6.3). The futex arm keeps its existing wake-generation protocol unchanged.

Arm-time TOCTOU (readiness changes between the syscall's try and watch insertion) is closed
inside `arm()` (§5.3). Combined with Invariant W, the full blocking loop has no window.

## 8. Blocking syscalls: try-once + implicit kernel-ring wait

**Deliberate deviation from the literal CLAUDE.md text** ("blocking syscalls become
non-blocking try-once-and-return", with userspace ring wrappers). The *scheduler* keeps
exactly one fd-blocking mechanism (rings), but the syscall ABI keeps blocking
`read`/`write`/`accept`. Rationale:

1. The try/park loop's correctness lives in exactly one place — the kernel — covering the
   std PAL, cpal, the toyos-cc C bootstrap, and every future port alike. Userspace-side
   loops would re-open the loop-protocol bug class in every raw consumer.
2. 1 syscall per blocking op, not ≥2 (arm+wait, then retry).
3. A kernel ring costs ~100 bytes; no per-blocking-thread ring pages at all.
4. Migration risk collapses: `SYS_READ` semantics are preserved, so all of userland is
   untouched until the polish stage — no flag-day semantics flip under the whole ecosystem.

The uniformity CLAUDE.md wants is preserved where it matters: inside the kernel there is
one park mechanism, and the kernel ring is a real `Ring` (same registry, same watches, same
CQE protocol) minus the pages.

```rust
// kernel/src/arch/syscall.rs — the uniform shape, sys_read (pipe) shown
fn sys_read(fd_num: u32, buf: &mut [u8]) -> u64 {
    loop {
        match fd::try_read(..) {
            Some(n) => { /* post(PipeWritable) if pipe space freed */ return n; }
            None => {
                let Some(source) = descriptor.read_source() else { return NotFound };
                completion::wait_current(source, Deadline::FOREVER);   // §5.3
            }
        }
    }
}
```

| Syscall | try-once | Armed source | Deadline |
|---|---|---|---|
| `SYS_READ` (pipe) | `pipe::try_read` | `PipeReadable(id)` | FOREVER |
| `SYS_READ` (serial console) | console try | `Keyboard` | now + 10 ms (existing fallback) |
| `SYS_READ` (audio fd) | `drain_completed` | `Audio` | FOREVER |
| `SYS_READ` (keyboard/mouse) | device try | `Keyboard` / `Mouse` | FOREVER |
| `SYS_WRITE` (pipe) | `pipe::try_write` | `PipeWritable(id)` | FOREVER |
| `SYS_ACCEPT` | `listener::try_accept` | `Listener(id)` | FOREVER |
| net recv | net try | `Network` | FOREVER |
| `SYS_WAITPID` | `wait_child_zombie` | `ChildExit(self)` — **fixes B3** | FOREVER |
| `SYS_THREAD_JOIN` | try-join | `ThreadExit(tid)` | FOREVER |
| `SYS_SLEEP_UNTIL` | — | — (pure `park_timeout(until)`) | caller Instant |
| `SYS_IO_URING_ENTER` | `cq_count` check | (the ring itself) | caller deadline |
| `SYS_FUTEX_WAIT` | value check | Futex channel | caller deadline |

`sys_read_nonblock`/`sys_write_nonblock` remain (honest try-once calls; soundd uses them).

`fd.rs` grows `Descriptor::{read,write}_source() -> Option<Source>` — the single
fd→Source mapping.

Cancellation: fd close routes through `remove_fd` → `Cancelled` CQE per armed watch on
user rings; kernel-ring waiters wake, retry, and the try-op on the closed fd returns the
error. User-ring destroy while a thread is parked in `enter`: remove from registry under
CORE → `wake_channel(Ring(id))` unconditionally → free the slot; the woken `enter` loop's
registry lookup fails → returns `NotFound`. **Invariant: destroy always wakes.**

## 9. Time and deadline semantics

### 9.1 Types (toyos-abi/src/time.rs, new)

```rust
/// Monotonic kernel time, nanos_since_boot domain. Never a duration.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Instant(u64);
impl Instant {
    pub fn now() -> Instant;                 // existing clock syscall / rdtsc math
    pub const fn as_nanos(self) -> u64;
}

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Duration(u64);
impl Duration { pub const fn from_millis(ms: u64) -> Duration; /* … */ }

impl core::ops::Add<Duration> for Instant { type Output = Instant; /* saturating */ }
impl core::ops::Sub<Instant>  for Instant { type Output = Duration; /* saturating */ }
// The ONLY arithmetic that compiles. No Add<Instant> for Instant, no
// Duration→Instant coercion: relative/absolute confusion is a type error.

/// SDK-facing wait bound. Wire format: one u64 absolute deadline.
pub enum Wait { Poll, Until(Instant), Forever }
impl Wait {
    pub fn deadline_ns(self) -> u64 {
        match self { Wait::Poll => 0, Wait::Until(t) => t.as_nanos(),
                     Wait::Forever => u64::MAX }
    }
}
```

### 9.2 Continuous semantics — the sentinel branch does not exist

Kernel-side deadline is a transparent newtype with a **total order over the whole u64
range**: `0` is simply the past (evaluate once, return), `u64::MAX` is a time never
reached (forever). No sentinel branch exists in the kernel, so there is nothing to collide
with and nothing to assert:

```rust
#[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline(u64);
impl Deadline {
    pub const FOREVER: Deadline = Deadline(u64::MAX);
    pub fn at(t: Instant) -> Deadline;
}
```

`park_timeout` takes a bare `Instant` — `Instant` has no `FOREVER` value, so a pure
sleep is finite by construction (CT5), while every comparison downstream is branch-free.

This deletes the soundd bug class at its root: a DLL prediction in the past means "poll
and return now" — exactly the correct behavior for a late mixer. The
`delta == 0 → sleep a full period` hack becomes unwritable: there is no encoding for it.
Absolute deadlines are also idempotent across spurious-wake retries — every re-park uses
the same deadline, so no drift accumulates.

### 9.3 Syscall semantics

`SYS_IO_URING_ENTER(ring_fd, to_submit, min_complete, deadline_ns)` — same four
registers; `deadline_ns` is absolute. Non-blocking is `min_complete = 0` (already its
meaning; the `timeout=0` sentinel is removed). Semantics, total over u64:

| `cq_count ≥ min_complete` | `now ≥ deadline` | Behavior |
|---|---|---|
| yes | — | return count |
| no | yes | return count (0 ⇒ poll-once falls out; MAX ⇒ forever falls out) |
| no | no | park on `Ring(id)`; deadline armed via `ensure_armed_before` (**proposed primitive — does not exist yet**; §4) |

Reject at entry (fail fast): `min_complete > cq_size` → `InvalidArgument`.

`SYS_NANOSLEEP` → `SYS_SLEEP_UNTIL(deadline_abs_ns)`; the SDK and std PAL provide
`sleep(Duration)` sugar (`now() + d`). `SYS_FUTEX_WAIT(addr, expected, deadline_abs_ns)`
becomes absolute, matching everything else. Both are small, contained changes in the
toyos-specific std PAL files.

## 10. CQE format — timestamps ride in the completion

```rust
// toyos-abi/src/io_uring.rs
#[repr(C)]
pub struct IoUringCqe {
    pub user_data: u64,
    pub result: i32,       // ≥0: readiness flags / op result; <0: -SyscallError
    pub flags: u32,
    /// nanos_since_boot captured inside post() — IRQ-entry fidelity for device
    /// sources; arm-time now for immediate (already-ready) fires.
    pub timestamp: u64,
}                          // 24 bytes
```

This supersedes the earlier rejection of CQE timestamps ("one consumer"). The timestamp is
captured in `Post` regardless (correct RT/latency attribution requires post-time capture —
flush-time stamps would reintroduce exactly the drain-time-jitter deviation the audio
subsystem documents), so carrying it into the CQE is free generality: audio DLL, input
latency, and net timing all read hardware-event time with no extra syscall. The pinned
audio record-fd ABI is untouched — the CQE timestamp is additive, and soundd migrates to
it at the SDK polish stage.

## 11. Userland SDK

```rust
// toyos/src/io.rs — app code stays clean
pub fn read(h: &impl AsHandle, buf: &mut [u8]) -> Result<usize, Error>;  // 1 SYS_READ; kernel parks
pub fn read_until(h: &impl AsHandle, buf: &mut [u8], deadline: Instant)
    -> Result<usize, TimedOut>;  // thread-local lazily-created 32KB arena-slot Ring:
                                 // watch + enter(Until) + try loop

// toyos/src/ring.rs — successor of Poller (poller.rs DELETED)
pub struct Token(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Interest { Readable, Writable }   // enum, not bitflags: a watch waits for
                                           // exactly one thing; "armed for nothing"/
                                           // "both by accident" unrepresentable

pub struct Completion {
    pub token: Token,
    pub kind: CompletionKind,
    pub timestamp: Instant,                // straight from the CQE (§10)
}

/// Callers MUST match; silent drop of error/cancel completions — today's
/// Poller::wait behavior (poller.rs:101-103), which hid poll-cancelled and
/// ResourceExhausted from soundd — is unrepresentable.
#[non_exhaustive]
pub enum CompletionKind { Ready(Interest), Cancelled, Failed(SyscallError) }

pub struct Ring { .. }
impl Ring {
    pub fn new(entries: u32) -> Ring;
    pub fn watch(&mut self, h: &impl AsHandle, interest: Interest, token: Token);
    /// Submits pending watches, waits for ≥ min_complete or `wait`, yields
    /// every available completion. One syscall per cycle (arm + wait fused).
    pub fn wait_each(&mut self, min_complete: u32, wait: Wait,
                     f: impl FnMut(Completion)) -> u32;
}
```

- `AudioStream::wait_and_fill` (cpal clients): unchanged at the API — still one blocking
  `read` on the signal pipe; the kernel implementation changed underneath. PI preserved (§14).
- soundd: `Wait::Until(dll_target)` replaces the delta arithmetic; the DLL consumes
  per-CQE IRQ-time timestamps; one-shot re-arm per cycle kept.
- std fork: `SYS_READ`/`WRITE`/`ACCEPT` shapes preserved — no PAL blocking loops. Only the
  `sleep_until`/futex-deadline signature updates touch the toyos PAL files (§9.3).

## 12. Priority inheritance

`post()` captures `rt` from the percpu `current_is_rt` mirror (written at context switch,
no locks; forced `false` in IRQ context — §4.2). `flush` passes `Boost::RtInherited` to
`wake_channel`, which stamps woken ctxs exactly as `wake_pipe_readers` does today (cleared
at the next scheduling boundary, unchanged). The pipe-side `rt_boost_pending` consume-time
boost (for readers that were runnable rather than parked) is kept as-is. Net: PI semantics
preserved, minus the special-cased pipe wake function. The rt capture lands in Stage 1 so
the behavior-preserving claim includes PI timing (§19).

## 13. Locking

### 13.1 Order (total, documented in completion.rs)

```
CORE → {source locks (PIPES, LISTENERS, AUDIO, ...)}     (arm-time readiness recheck)
CORE released before wake_channel                         (flush collects, then wakes)
wake_channel: POOL → cpus[i] → sched_state                (existing order)
post(): NO locks, any context                             (the rule that makes B1 dead)
Nothing acquires CORE while holding a source lock or POOL.
```

`has_unconsumed(r)` in the park recheck takes CORE briefly (lookup + flag/count read);
POOL is not held at that point (`park_outgoing` dropped it) — no cycle.

### 13.2 Committed sharding plan (gated, not open-ended)

The single CORE lock on every arm + flush is the one credible 128-core risk. The plan is
designed now and implemented when the Stage-6 gate trips — not "if measurements demand"
without a plan:

- `CompletionCore` splits into `SHARDS: [Lock<Shard>; 16]`, sharded by
  `hash(Source discriminant, source id)`. Each shard owns the `watches` fan-out entries
  for its sources.
- Rings move to a separate `RINGS: IdMap<RingId, Lock<Ring>>` with per-ring locks. Lock
  order: shard → ring, always. Fan-out entries carry `(RingId, watch_gen)`; a per-ring
  generation counter invalidates stale entries so destroy/disarm never needs the reverse
  lock order (destroy bumps the gen under the ring lock, then lazily GCs fan-out entries
  as flush encounters them).
- Kernel-ring waits shard naturally (per-thread rings, distinct sources).

**Gate (Stage 6):** measure CORE hold time and contention under `audio_tone_load` + Doom
at `-smp 8`. Shard when p99 hold > 2 µs or contention > 5 % of flush time; mandatory
before the 128-core port regardless.

### 13.3 Wake-path cost note

`post()` sets `need_resched`, so every waking syscall funnels through a `do_schedule` pass
at exit. This is bounded and usually necessary (the woken task may deserve this CPU). If
Stage-6 measurement shows waste, the committed refinement is a percpu `need_flush` flag
distinct from `need_resched`: the exit path flushes, and enters `do_schedule` only when
flush enqueued locally something that preempts current (RT vs CFS check). Not implemented
until measured — >2x rule.

## 14. Audio latency budget

2.902 ms periods, 8×512 B pipeline = 23.2 ms cushion.

| Hop | Mechanism | Budget |
|---|---|---|
| DMA complete → ISR `post(Audio)` | wait-free post with IRQ-time `ts` + need_resched | < 5 µs |
| post → flush (CQE + wake) | IRQ epilogue / syscall exit on the same CPU; CORE lock, 1–2 watches | < 20 µs |
| wake → soundd running | RT band: enqueue + same-CPU need_resched or targeted kick IPI | < 50 µs (idle target CPU) |
| soundd drain + mix + submit | userspace, unchanged | ~100–300 µs |
| **Total wake-to-submit** | | **≪ 1 period; pipeline tolerates 10 ms TCG outliers** |

DLL timestamp error drops from drain-time ms-scale jitter (single shared slot) to per-CQE
µs-scale IRQ-time stamps. Deadline fallback: `park(Ring, at(t_est))` + `ensure_armed_before`
(proposed; nothing of that name exists in the tree today — and `apic.rs`'s `last_armed_ticks`
is unrelated LAPIC one-shot bookkeeping, not a partial implementation of it)
⇒ LAPIC one-shot precision when armed locally, bounded by the 10 ms quantum otherwise.
Gate: every migration stage keeps the audio glitch scan green (§18, §19).

## 15. Invariants

**Compile-time (bug class unrepresentable):**

- CT1 A thread cannot park on an fd/source: `WaitChannel` has no such variant; no
  scheduler API accepts a `Source`. Kills B2 as a class; closes B3 via typed
  `ChildExit`/`ThreadExit` sources.
- CT2 A wake site cannot notify half the machinery: one function, `post()`; the dual-call
  idiom no longer exists to forget. Kills B1.
- CT3 New wait channels force their recheck: exhaustive `match chan` in the single Park
  arm — a variant without a recheck arm is a compile error.
- CT4 No relative/absolute confusion, no sentinels: `Instant`/`Duration` restricted
  algebra; continuous `Deadline` over u64 — the sentinel branch does not exist. Kills B4.
- CT5 Pure sleeps are finite: `park_timeout(Instant)` — `Instant` has no Forever value;
  "block forever on nothing" cannot be typed.
- CT6 Parked state names objects by Copy IDs only (`RingId`, `DirectMap`) — destroyed
  rings fail lookups instead of dangling. Kills B5 (crash.md class).
- CT7 Ring memory is RAII (`RingArena` page, `ArenaSlot` bitmap bit): leak/double-free is
  a Drop, not call-site discipline. `WokenBatch` stays `#[must_use]`.
- CT8 SDK completions carry `#[non_exhaustive] CompletionKind` — error/cancel CQEs cannot
  be silently skipped.
- CT9 `Armed` is a `#[must_use]` non-Copy typestate consumed by `wait()`; Drop disarms.
  Park-without-armed-watch and forgotten-consume are untypeable through the blocking path
  (`wait_current` is the only shape syscalls use).
- CT10 Kernel sinks hold `Option<Watch>` — a second concurrent watch is unrepresentable;
  re-arm is replace, not an assert.
- CT11 `Interest` is an enum — a facade watch armed for nothing or for both by accident
  cannot be expressed.

**Runtime fail-fast:**

- RT1 CQ overflow assert (2× structural sizing, kept).
- RT2 Registry consistency: a fan-out entry without its armed twin (or vice versa,
  modulo §13.2 lazy-GC generations) panics.
- RT3 flush non-reentrancy assert per CPU; Post-queue overflow → loud log + sweep, never
  a silent drop.
- RT4 `destroy` on a ring with parked waiters must wake them — debug scan in destroy.
- RT5 `min_complete > cq_size` → `InvalidArgument` at `enter` entry.
- RT6 Arena bitmap: freeing a clear bit or allocating a set bit panics.

## 16. Failure modes

| Failure | Behavior | Recovery |
|---|---|---|
| Wake races park (any source) | Park-time recheck observes the CQE (Invariant W) | Self-wake, retry — structural |
| Child exits before parent parks in waitpid | `arm(ChildExit)` immediate-fires on `has_zombie_child` | No hang — B3 closed |
| Kill races park | `kill_pending` recheck under the POOL fence | Thread unparks, exits at syscall boundary |
| Post queue overflow (IRQ burst) | Sweep flag: flush evaluates `is_ready` for all armed sources; loud log | No lost wake, degraded to one full scan |
| fd closed while watched | `Cancelled` CQE per armed watch; kernel-ring waiter retries and gets the fd error | Waiter returns error, never hangs |
| User ring destroyed while sibling parked in `enter` | Registry removal → unconditional `wake_channel` → slot freed; woken lookup fails | `enter` returns `NotFound` |
| Process exits with live rings | Threads retire first; kernel rings destroyed post-retire; `RingArena` page freed by ProcessData drop | No dangling waiter, no leak |
| Re-park after deadline/kill wake left a stale armed watch | `arm()` replace semantics disarm the old watch and clear stale `fired` | At worst one spurious wake |
| Stale fire between `take_fired` and next arm | Spurious wake on next park | Loop re-derives — harmless |
| DLL prediction in the past (late mixer) | `enter` evaluates once and returns immediately (continuous deadline) | Correct "I'm late" behavior; no full-period oversleep |
| CQ full | Structurally impossible (2× depth vs one-shot watches ≤ depth); assert if violated | Kernel bug — loud panic |
| Multiple threads in `enter` on one shared ring | `wake_channel` wakes all; losers re-park | Documented thundering herd, bounded by sharing |

No failure mode produces a lost wake, a dangling reference, or a silent stall. The worst
cases are one spurious wake or one full-registry sweep, both loud in debug builds.

## 17. File / crate layout

```
kernel/src/completion.rs        NEW  — Source, Post queue, registry, Ring, Sink, RingArena,
                                post()/flush()/arm()/wait()/wait_current()/destroy()
kernel/src/io_uring.rs          THINNED — syscall surface: SQE decode, ops
                                (POLL_ADD/REMOVE/ACCEPT/CLOSE/NOP); storage in completion.rs
kernel/src/scheduler.rs         — park/park_timeout/wake_channel/unpark_for_kill; single
                                Park arm; by_channel index (EventSource already gone,
                                sched migration 7a/7c)
kernel/src/fd.rs                — Descriptor::{read,write}_source() → Source mapping
kernel/src/pipe.rs, listener.rs, audio.rs, drivers/xhci/hid.rs, net
                                — wake sites become one post(); per-source watcher Vecs DELETED
kernel/src/arch/syscall.rs      — blocking loops over wait_current; enter() absolute deadline
kernel/src/process.rs           — thread kernel-ring + RingArena lifecycle; exit paths post
                                ChildExit/ThreadExit; kill_pending
kernel/src/arch/idt/…           — ISR wake sites → post(); percpu in_irq depth
toyos-abi/src/time.rs           NEW  — Instant, Duration, Wait
toyos-abi/src/io_uring.rs       — 24-byte Cqe with timestamp; 32KB slot layout; Wait encoding
toyos-abi/src/syscall.rs        — SLEEP_UNTIL, absolute futex deadline; AUDIO_POLL deleted
toyos/src/ring.rs               NEW  — Ring, Completion, CompletionKind, Interest
toyos/src/io.rs                 NEW  — blocking + deadline I/O helpers
toyos/src/poller.rs             DELETED (Stage 5)
```

## 18. Test battery

New named tests (all run at `-smp 1` and `-smp 8`; audio tests additionally under load):

- `blocking_read_stress` — cross-CPU pipe ping-pong ×100 000 between two threads; the
  lost-wake canary for the pipe path. Must complete within a hard wall-clock bound.
- `waitpid_storm` — spawn/exit children in a tight loop racing the parent's waitpid. Same for
  `thread_join`. **It is a ticket-protocol canary, not a `wake_task` canary.** B3's hang was
  one instance of a general race — decide to park, then have the event land before the park
  completes — and after this design the protocol that must hold is `prepare_wait` →
  `commit` → park, with `Commit::AlreadyWoken` refusing the park. `wake_task` being gone does
  not retire this test; it is what exercises the replacement, and it should be read as failing
  when the ticket protocol has a hole rather than when one deleted function returns.
- `sleep_until_accuracy` — on an idle CPU, `SYS_SLEEP_UNTIL` wake error bounded (≤ 1 ms
  under TCG; assert recorded as a test bound, tightened on KVM).
- `enter_deadline_past` — `enter` with a past deadline returns immediately with whatever
  is ready (poll-once semantics); with `min_complete=0` never parks.
- `ring_lifecycle_stress` — many-thread ring create/watch/destroy churn; memory
  accounting asserts 32KB/ring and arena-page free at process exit.
- `destroy_while_parked` — thread A parked in `enter`, thread B closes the ring fd; A
  returns `NotFound` within a bound.
- Audio glitch scan (existing gate): zero n×2.902 ms gaps for `audio_tone` and
  `audio_tone_load`; plus the late-wake assertion — no full-period oversleep when the DLL
  prediction is in the past.
- Grep-gates per stage (§19): the set of `scheduler::park` callers, the absence of
  `wake_by_event`/`wake_task`/`scheduler::block` identifiers after the deletion stage,
  `post(` as the only notification call at wake sites.

## 19. Staged migration

Each stage builds, boots, and passes full `cargo test` including the audio glitch test,
at `-smp 1` and `-smp 8`.

**Stage 0 — gate (concurrent stabilization effort, verify only).** ISR-timestamped audio
record ring, need_resched-on-IRQ, harness audio glitch test (`tests/common/audio.rs`,
`audio_tone`/`audio_tone_load`) exist and pass. No new work in this track.

**Stage 1 — completion core, behavior-preserving.** Add `kernel/src/completion.rs`:
`Source`, per-CPU Post queue, `post()`/`flush()`, registry absorbing all per-source
watcher Vecs; percpu `in_irq` depth + `current_is_rt` mirror; `Post` carries `ts` and
`rt` from day one (so PI timing and timestamp fidelity are preserved, not deferred).
Rewire every wake site to a single `post()`. `flush` internally still calls today's two
paths — the `sched::waitqs` wake for direct blockers AND `complete_pending_for_event`'s
CQE fan-out — both behind one entry point. Delete the per-source watcher boilerplate.
Green: identical observable behavior; wake sites grep-provably single-call.

**Stage 2 — time & deadline ABI.** `toyos-abi/src/time.rs`; `io_uring_enter` absolute
continuous deadline + `min_complete=0` nonblock; 24-byte CQE with timestamp; kernel
`Deadline(u64)` replaces `0 = forever`; `SYS_SLEEP_UNTIL`; absolute futex deadline. Update
SDK + std PAL (toyos files only) + soundd (delete the `delta==0` hack; DLL consumes CQE
timestamps) in the same commit (monorepo, unstable ABI).
Green + `sleep_until_accuracy` + `enter_deadline_past` + late-wake audio assertion.

**Stage 3 — ring arena.** Per-process `RingArena`, 32KB `ArenaSlot`s; io_uring drops
`shared_memory` (last `shared_memory::destroy()` caller gone).
Green + `ring_lifecycle_stress` + memory-accounting gate.

**Stage 4 — one blocking mechanism.** Sub-staged; legacy and new coexist per source
until 4d.
- 4a: `Sink::Kernel` + per-thread kernel rings + `arm`/`Armed`/`wait_current`; migrate
  pipe + audio-fd blocking. Audio + Doom soak; `blocking_read_stress` lands here.
- 4b: migrate listener/accept, keyboard/mouse/serial-console, network reads.
- 4c: `ChildExit`/`ThreadExit` sources; `sys_waitpid`/`SYS_THREAD_JOIN` migrate (closes
  B3); `nanosleep` path → `park_timeout`; kill/retire → `kill_pending` +
  `unpark_for_kill`; `wake_task` deleted (it still has 5 call sites today —
  `scheduler.rs:257,435`, `process.rs:1081,1130,1615` — so this stage's deletion is
  outstanding, not done). `waitpid_storm` lands here.
- 4d: deletion commit — `io_uring::Source` + `complete_pending_for_event`,
  `wake_pipe_*`, ad-hoc recheck arms → single Park arm (§7), per-source fd queues →
  `by_channel`, `SYS_AUDIO_POLL` (`EventSource`, `scheduler::block`, `wake_by_event` and
  `event_ready` already fell to sched migration 7a/7c). The deletion commit is the
  proof: nothing else compiles against the old surface. Grep-gate: `scheduler::park`
  callers are exactly {completion::wait, futex_wait}.

**Stage 5 — SDK polish.** `toyos::ring::Ring` + `toyos::io` replace `Poller` (deleted);
`CompletionKind`/`Interest` surface; cpal/soundd/netd/compositor call sites updated;
soundd fully on CQE timestamps. `destroy_while_parked` lands here.
Green.

**Stage 6 — PI unification + scale validation.** Remove the pipe-specific wake-boost
special case (consume-side `rt_boost_pending` kept). Measure flush/arm CORE hold times and
contention under `audio_tone_load` + Doom at `-smp 8`; record budgets as test assertions;
apply the §13.2 sharding plan if the gate trips (mandatory before the 128-core port
either way).

Stage 3 is independent of Stage 4 and may land in either order relative to it; 4c
requires 4a's machinery; 5 requires 4d.

## 20. Explicitly rejected / scoped out

- **Userspace-only blocking wrappers** (literal CLAUDE.md reading): moves the try/park
  loop's correctness into every raw consumer (std PAL duplicate, C bootstrap, future
  ports) — the bug class moves rather than dies; ≥2 syscalls per blocking op; a flag-day
  semantics flip of the three hottest syscalls under the whole ecosystem. The kernel ring
  keeps ONE scheduler mechanism while preserving 1-syscall blocking I/O and a per-source
  migration.
- **ParkWord unification (fold IoUring into Futex; park on the CQ-tail word).** Elegant
  end state (one park predicate, one-load recheck, free userspace-executor ABI), but
  rejected until two preconditions have typed answers: (1) it narrows the futex wake
  contract — `futex_wake` without a word mutation between value-check and pool-insert is
  lost, which the current `wake_gen` protocol exists precisely to close; a generation
  counter or a mandated word-bump ABI rule would need to be designed as an invariant, not
  a convention; (2) it parks kernel threads on an address in user-freeable ring memory —
  the crash.md shape — guarded only by a runtime destroy-wake rule. Documented as a
  future direction; any proposal must answer both in the type system first.
- **Posting CQEs directly from ISR context**: needs a lock-free registry to find rings;
  buys single-digit µs over need_resched-deferred flush against a 2.9 ms budget; fails
  the >2x rule. Post-time timestamps already preserve timing fidelity.
- **An `IrqCtx` capability token / second post function**: `post()` is legal from every
  context and derives `ts`/`rt` from percpu state that is correct in all of them — there
  is no wrong context whose absence a token could prove, and a second function would
  reintroduce a choose-the-right-one discipline. (The token idea is the right tool where
  context-restricted entry points exist; this design deliberately has none.)
- **Multishot polls**: one-shot + re-arm is what soundd does and what the kernel loop
  needs; multishot adds CQ-overflow back-pressure policy. Revisit with a measured re-arm
  cost.
- **fn-pointer / trait-object recheck hooks** on park: hides the recheck from the
  exhaustiveness checker — the enum match IS the safety mechanism.
- **Keeping `wake_task` for waitpid/join/kill**: directed wakes into a pool the target
  may not have entered are B3/B6; sources + `kill_pending` + Invariant W close both
  structurally.
- **Relative timeouts / NONBLOCK flags**: two representations of one concept and the
  exact sentinel class being deleted; absolute continuous deadlines subsume both and match
  the DLL math natively.
- **Bitflags `Interest`**: a watch waits for exactly one readiness kind; users arm two
  watches if they truly need both.

## 21. Interop contract with the Phase-1 scheduler track

Required from Phase 1: `park(WaitChannel, Deadline)`, `park_timeout(Instant)`,
`wake_channel(WaitChannel, Option<Boost>)` with guarantee G1 (§6.1), `unpark_for_kill`
with the `kill_pending` fence (§6.3), a POOL lock (or equivalent serialization) providing
the happens-before edge in Invariant W, `ensure_armed_before` semantics for parked
deadlines, and percpu `current_is_rt` + `in_irq` maintained at context switch and
interrupt entry/exit.

Provided to Phase 1: the completion core never holds CORE across `wake_channel`, never
calls scheduler APIs from ISR context, and `post()` is legal inside any scheduler critical
section — Phase 1 may freely call `post()` from its own guarded regions (e.g. exit paths
posting `ChildExit`). The parked-state surface Phase 1 must model is exactly: two channel
kinds, one deadline map, one kill flag — small enough for the ownership-typed simulator.
